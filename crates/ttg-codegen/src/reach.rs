//! Reachability analysis: where can each resource send traffic, what is exposed to the
//! internet, and can resource A actually talk to resource B?
//!
//! The analysis works on the abstract diagram plus a small per-provider policy (AWS
//! security groups deny by default in both directions; Azure NSGs allow VNet-internal
//! and outbound traffic by default). It is deliberately conservative: when a rule cannot
//! be decided from the diagram the answer is `Unknown`, never a silent "ok".

use crate::diagnostics::relation_targets;
use std::collections::BTreeMap;
use ttg_catalog::Catalog;
use ttg_core::{EntityRef, Id, Project, Relation, Value};

/// How a resource gets out of its network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Egress {
    /// A passive resource (database, cache, load balancer) that never initiates traffic.
    NotNeeded,
    /// Not inside any subnet: a managed service or a function outside the network.
    Unrestricted,
    /// Through these hops (subnet, route table, NAT / internet gateway ...).
    Via(Vec<Id>),
    /// No way out, with the reason.
    Blocked(String),
}

/// Network posture of one resource.
#[derive(Debug, Clone)]
pub struct Posture {
    pub subnets: Vec<Id>,
    pub security_group: Option<Id>,
    pub egress: Egress,
    /// How the internet can reach it, if it can.
    pub exposed: Option<String>,
    /// Does this type participate in network paths at all (as a target)?
    pub networked: bool,
    /// Managed service reached over the provider API rather than a network path.
    pub managed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Blocked,
    Unknown,
}

/// The result of asking "can `source` reach `target`?".
#[derive(Debug, Clone)]
pub struct Path {
    pub target: Id,
    pub status: Status,
    /// Entities the traffic passes through, in order (excluding source and target).
    pub hops: Vec<Id>,
    pub reason: String,
    /// Extra remarks that do not change the status (missing credentials link, etc.).
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Reach {
    pub provider: String,
    pub posture: BTreeMap<Id, Posture>,
}

const MANAGED: &[&str] = &[
    "event_queue",
    "topic",
    "storage_queue",
    "secret",
    "object_storage",
    "log_group",
    "nosql_table",
    "container_registry",
    "key_vault",
];
const NETWORKED: &[&str] = &[
    "compute_instance",
    "relational_database",
    "cache",
    "load_balancer",
    "kubernetes_cluster",
    "autoscaling_group",
    "function",
    "container_app",
];

pub(crate) fn is_managed(t: &str) -> bool {
    MANAGED.contains(&t)
}

/// Types that open connections (as opposed to passive services that only listen).
pub fn initiates(t: &str) -> bool {
    matches!(
        t,
        "function" | "compute_instance" | "autoscaling_group" | "kubernetes_cluster" | "container_app"
    )
}
fn is_networked(t: &str) -> bool {
    NETWORKED.contains(&t)
}

/// Per-provider defaults that the diagram does not spell out.
struct Policy {
    /// Traffic inside one network is allowed unless a rule denies it (Azure NSG default).
    intra_network_default_allow: bool,
    /// Outbound is allowed without an explicit egress rule (Azure NSG default).
    egress_default_allow: bool,
}

fn policy(provider: &str) -> Policy {
    match provider {
        "azure" => Policy {
            intra_network_default_allow: true,
            egress_default_allow: true,
        },
        _ => Policy {
            intra_network_default_allow: false,
            egress_default_allow: false,
        },
    }
}

fn parse_cidr(s: &str) -> Option<(u32, u32)> {
    let (addr, len) = s.split_once('/')?;
    let len: u32 = len.parse().ok()?;
    if len > 32 {
        return None;
    }
    let ip: std::net::Ipv4Addr = addr.parse().ok()?;
    let mask = if len == 0 { 0 } else { u32::MAX << (32 - len) };
    Some((u32::from(ip) & mask, mask))
}

/// Does `outer` contain every address of `inner`?
fn cidr_covers(outer: &str, inner: &str) -> bool {
    match (parse_cidr(outer), parse_cidr(inner)) {
        (Some((no, mo)), Some((ni, mi))) => mi >= mo && (ni & mo) == no,
        _ => false,
    }
}

fn subnets_of(p: &Project, cat: &Catalog, e: &EntityRef) -> Vec<Id> {
    relation_targets(p, cat, e, Relation::NetworkMembership)
        .into_iter()
        .filter(|t| p.entity(t).is_some_and(|x| x.resource_type == "subnet"))
        .collect()
}

fn sg_of(p: &Project, cat: &Catalog, e: &EntityRef) -> Option<Id> {
    relation_targets(p, cat, e, Relation::AttributeReference)
        .into_iter()
        .find(|t| p.entity(t).is_some_and(|x| x.resource_type == "security_group"))
}

fn vnet_of_subnet(p: &Project, cat: &Catalog, subnet: &str) -> Option<Id> {
    let e = p.entity(subnet)?;
    relation_targets(p, cat, &e, Relation::NetworkMembership)
        .into_iter()
        .next()
}

/// Hops from a subnet to the internet: route table, then NAT (and its own subnet's
/// route table + internet gateway) or the internet gateway directly.
fn subnet_egress(p: &Project, cat: &Catalog, subnet: &str) -> Result<Vec<Id>, String> {
    let tables: Vec<EntityRef> = p
        .entities()
        .into_iter()
        .filter(|e| e.resource_type == "route_table")
        .filter(|rt| {
            relation_targets(p, cat, rt, Relation::Attachment)
                .iter()
                .any(|s| s == subnet)
        })
        .collect();
    if tables.is_empty() {
        return Err("subnet has no route table".into());
    }
    for rt in &tables {
        for gw in relation_targets(p, cat, rt, Relation::AttributeReference) {
            let Some(g) = p.entity(&gw) else { continue };
            match g.resource_type {
                "internet_gateway" => return Ok(vec![rt.id.to_string(), gw.clone()]),
                "nat_gateway" => {
                    // The NAT itself must sit in a subnet that routes to an internet gateway.
                    let nat_subnets = subnets_of(p, cat, &g);
                    for ns in nat_subnets {
                        if let Ok(rest) = subnet_egress(p, cat, &ns) {
                            if rest
                                .iter()
                                .any(|h| p.entity(h).is_some_and(|x| x.resource_type == "internet_gateway"))
                            {
                                let mut hops = vec![rt.id.to_string(), gw.clone(), ns.clone()];
                                hops.extend(rest);
                                return Ok(hops);
                            }
                        }
                    }
                    return Err(format!(
                        "route table \"{}\" points at NAT \"{}\" whose subnet has no internet route",
                        rt.name, g.name
                    ));
                }
                _ => {}
            }
        }
    }
    Err(format!(
        "route table \"{}\" has no default route via a NAT or Internet Gateway",
        tables[0].name
    ))
}

/// Rules of a security group as (direction, protocol, from, to, cidr, source_group).
struct Rule {
    ingress: bool,
    protocol: String,
    from: i64,
    to: i64,
    cidr: String,
    source_group: String,
}

fn rules_of(p: &Project, sg: &str) -> Vec<Rule> {
    let Some(e) = p.entity(sg) else { return vec![] };
    let Some(Value::Records(rows)) = e.field("rules") else {
        return vec![];
    };
    rows.iter()
        .map(|r| Rule {
            ingress: r.get("direction").map(|v| v.display()) == Some("ingress".into()),
            protocol: r.get("protocol").map(|v| v.display()).unwrap_or("tcp".into()),
            from: r.get("from_port").and_then(|v| v.as_int()).unwrap_or(0),
            to: r.get("to_port").and_then(|v| v.as_int()).unwrap_or(0),
            cidr: r.get("cidr").map(|v| v.display()).unwrap_or_default(),
            source_group: r.get("source_group").map(|v| v.display()).unwrap_or_default(),
        })
        .collect()
}

fn rule_matches_port(r: &Rule, port: Option<i64>) -> bool {
    if r.protocol == "all" {
        return true;
    }
    match port {
        None => true,
        Some(pt) => r.from <= pt && pt <= r.to,
    }
}

/// The port a client uses to talk to this resource, when it has a well-known one.
pub fn listening_port(e: &EntityRef) -> Option<i64> {
    match e.resource_type {
        "function" if e.field("http_trigger").and_then(|v| v.as_bool()).unwrap_or(false) => Some(443),
        "relational_database" => Some(
            if e.field("engine").map(|v| v.display()) == Some("mysql".into()) {
                3306
            } else {
                5432
            },
        ),
        "cache" => Some(6379),
        "load_balancer" => e.field("listener_port").and_then(|v| v.as_int()),
        "container_app" => e.field("port").and_then(|v| v.as_int()),
        "kubernetes_cluster" => Some(443),
        _ => None,
    }
}

fn subnet_cidr(p: &Project, subnet: &str) -> Option<String> {
    p.entity(subnet)?.field("cidr_block").map(|v| v.display())
}

/// The peering that connects networks `a` and `b`, if the diagram has one. A peering
/// sits inside one network and links ("Peers with") the other.
fn peering_between(p: &Project, cat: &Catalog, a: &str, b: &str) -> Option<Id> {
    p.entities()
        .into_iter()
        .filter(|e| e.resource_type == "network_peering")
        .find(|pe| {
            let local = p
                .ancestor_of_type(pe.id, "virtual_network")
                .map(|c| c.id.as_str());
            let remote = relation_targets(p, cat, pe, Relation::AttributeReference)
                .into_iter()
                .find(|t| p.entity(t).is_some_and(|x| x.resource_type == "virtual_network"));
            match (local, remote) {
                (Some(l), Some(r)) => (l == a && r == b) || (l == b && r == a),
                _ => false,
            }
        })
        .map(|e| e.id.to_string())
}

/// AWS needs an explicit route: does one of the subnet's route tables route through
/// `peering` (route table linked from the peering's "Route tables")?
fn subnet_routes_via_peering(p: &Project, cat: &Catalog, subnet: &str, peering: &str) -> bool {
    let Some(pe) = p.entity(peering) else {
        return false;
    };
    relation_targets(p, cat, &pe, Relation::Attachment)
        .iter()
        .filter_map(|rt| p.entity(rt))
        .filter(|rt| rt.resource_type == "route_table")
        .any(|rt| {
            relation_targets(p, cat, &rt, Relation::Attachment)
                .iter()
                .any(|s| s == subnet)
        })
}

/// A private endpoint in network `vnet` that fronts `target`.
pub(crate) fn private_endpoint_in(p: &Project, cat: &Catalog, vnet: &str, target: &str) -> Option<Id> {
    p.entities()
        .into_iter()
        .filter(|e| e.resource_type == "private_endpoint")
        .find(|pe| {
            relation_targets(p, cat, pe, Relation::AttributeReference)
                .iter()
                .any(|t| t == target)
                && subnets_of(p, cat, pe)
                    .iter()
                    .any(|s| vnet_of_subnet(p, cat, s).as_deref() == Some(vnet))
        })
        .map(|e| e.id.to_string())
}

/// Load balancers that forward to `target` (LB "Forwards to" instance, or an autoscaling
/// group "Registered with" the LB).
fn load_balancers_for(p: &Project, cat: &Catalog, target: &str) -> Vec<Id> {
    let mut out: Vec<Id> = p
        .entities()
        .into_iter()
        .filter(|e| e.resource_type == "load_balancer")
        .filter(|lb| {
            relation_targets(p, cat, lb, Relation::Attachment)
                .iter()
                .any(|t| t == target)
        })
        .map(|e| e.id.to_string())
        .collect();
    if let Some(t) = p.entity(target) {
        for lb in relation_targets(p, cat, &t, Relation::Attachment) {
            if p.entity(&lb).is_some_and(|x| x.resource_type == "load_balancer") && !out.contains(&lb) {
                out.push(lb);
            }
        }
    }
    out
}

/// Compute the posture of every resource for a provider.
pub fn analyse(full: &Project, cat: &Catalog, provider: &str) -> Reach {
    let layer = crate::layers::project_for(full, cat, provider);
    let p = &layer;
    let pol = policy(provider);
    let mut posture = BTreeMap::new();
    for e in p.entities() {
        if e.is_container {
            continue;
        }
        let managed = is_managed(e.resource_type);
        let networked = is_networked(e.resource_type);
        if !managed && !networked {
            continue;
        }
        let subnets = subnets_of(p, cat, &e);
        let security_group = sg_of(p, cat, &e);
        let egress = if subnets.is_empty() {
            Egress::Unrestricted
        } else if !initiates(e.resource_type) {
            Egress::NotNeeded
        } else {
            // Any subnet with a route out is enough.
            let mut found: Option<Vec<Id>> = None;
            let mut last_err = String::new();
            for s in &subnets {
                match subnet_egress(p, cat, s) {
                    Ok(hops) => {
                        let mut h = vec![s.clone()];
                        h.extend(hops);
                        found = Some(h);
                        break;
                    }
                    Err(err) => last_err = err,
                }
            }
            match found {
                None => Egress::Blocked(last_err),
                Some(hops) => {
                    // Security group must allow outbound (AWS); Azure allows by default.
                    let allowed = pol.egress_default_allow
                        || security_group.is_none()
                        || rules_of(p, security_group.as_deref().unwrap()).iter().any(|r| {
                            !r.ingress
                                && (r.cidr == "0.0.0.0/0"
                                    || r.protocol == "all" && r.cidr.is_empty() && r.source_group.is_empty())
                        });
                    if allowed {
                        Egress::Via(hops)
                    } else {
                        Egress::Blocked("its security group has no egress rule to 0.0.0.0/0".into())
                    }
                }
            }
        };
        let exposed = exposure(p, cat, provider, &e, &subnets, security_group.as_deref());
        posture.insert(
            e.id.to_string(),
            Posture {
                subnets,
                security_group,
                egress,
                exposed,
                networked,
                managed,
            },
        );
    }
    Reach {
        provider: provider.to_string(),
        posture,
    }
}

fn exposure(
    p: &Project,
    cat: &Catalog,
    provider: &str,
    e: &EntityRef,
    subnets: &[Id],
    sg: Option<&str>,
) -> Option<String> {
    match e.resource_type {
        "function" if e.field("http_trigger").and_then(|v| v.as_bool()).unwrap_or(false) => {
            return Some("public HTTPS endpoint".into());
        }
        "load_balancer" if e.field("scheme").map(|v| v.display()) == Some("internet_facing".into()) => {
            return Some("internet-facing load balancer".into());
        }
        "container_app" if e.field("public").and_then(|v| v.as_bool()).unwrap_or(false) => {
            return Some("public ingress".into());
        }
        "relational_database" | "cache" if provider == "azure" && subnets.is_empty() => {
            return Some("public endpoint (no subnet linked)".into());
        }
        _ => {}
    }
    // Something in a public subnet with a rule open to the world.
    if subnets.is_empty() {
        return None;
    }
    let public_subnet = subnets.iter().any(|s| {
        let public_ip = p
            .entity(s)
            .and_then(|x| x.provider_field("aws", "map_public_ip"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let igw = subnet_egress(p, cat, s)
            .map(|h| h.len() == 2) // route table + internet gateway directly
            .unwrap_or(false);
        igw && (provider != "aws" || public_ip)
    });
    if !public_subnet {
        return None;
    }
    let sg = sg?;
    let open = rules_of(p, sg)
        .into_iter()
        .find(|r| r.ingress && r.cidr == "0.0.0.0/0");
    open.map(|r| {
        if r.protocol == "all" {
            "all ports open to the internet from a public subnet".into()
        } else {
            format!("port {} open to the internet from a public subnet", r.from)
        }
    })
}

/// Paths from one resource to every other managed or networked resource. A target that
/// cannot be reached directly is retried through a load balancer that forwards to it.
pub fn paths_from(full: &Project, cat: &Catalog, reach: &Reach, source: &str) -> Vec<Path> {
    let layer = crate::layers::project_for(full, cat, &reach.provider);
    let p = &layer;
    let Some(src) = p.entity(source) else {
        return vec![];
    };
    let Some(sp) = reach.posture.get(source) else {
        return vec![];
    };
    let mut out = Vec::new();
    for (tid, tp) in &reach.posture {
        if tid == source {
            continue;
        }
        let Some(tgt) = p.entity(tid) else { continue };
        let mut path = direct_path(p, cat, reach, &src, sp, &tgt, tp);
        if path.status != Status::Ok && !tp.managed && tgt.resource_type != "load_balancer" {
            for lb in load_balancers_for(p, cat, tid) {
                let (Some(lbe), Some(lp)) = (p.entity(&lb), reach.posture.get(&lb)) else {
                    continue;
                };
                let first = direct_path(p, cat, reach, &src, sp, &lbe, lp);
                if first.status != Status::Ok {
                    path.notes
                        .push(format!("via load balancer \"{}\": {}", lbe.name, first.reason));
                    continue;
                }
                let second = direct_path(p, cat, reach, &lbe, lp, &tgt, tp);
                if second.status == Status::Ok {
                    let mut hops = first.hops.clone();
                    hops.push(lb.clone());
                    hops.extend(second.hops.iter().cloned());
                    path = Path {
                        target: tid.clone(),
                        status: Status::Ok,
                        hops,
                        reason: format!("via load balancer \"{}\": {}", lbe.name, second.reason),
                        notes: second.notes.clone(),
                    };
                    break;
                }
                path.notes
                    .push(format!("via load balancer \"{}\": {}", lbe.name, second.reason));
            }
        }
        out.push(path);
    }
    out
}

/// One hop: can `src` talk to `tgt` without an intermediary (other than gateways,
/// peerings and private endpoints, which are part of the network fabric)?
fn direct_path(
    p: &Project,
    cat: &Catalog,
    reach: &Reach,
    src: &EntityRef,
    sp: &Posture,
    tgt: &EntityRef,
    tp: &Posture,
) -> Path {
    let pol = policy(&reach.provider);
    let tid = tgt.id.to_string();
    let linked = p
        .edges_from(src.id)
        .any(|e| e.target == tid && e.relation != Relation::DependsOn);
    let path = |status: Status, hops: Vec<Id>, reason: String, notes: Vec<String>| Path {
        target: tid.clone(),
        status,
        hops,
        reason,
        notes,
    };
    let sv = sp.subnets.iter().filter_map(|s| vnet_of_subnet(p, cat, s)).next();
    let pe = sv.as_deref().and_then(|v| private_endpoint_in(p, cat, v, &tid));
    let name_of = |id: &str| p.entity(id).map(|x| x.name.to_string()).unwrap_or_default();

    if tp.managed {
        // A private endpoint in the source's network bypasses egress altogether.
        if let Some(pe) = pe {
            let pe_name = name_of(&pe);
            if !linked {
                return path(
                    Status::Blocked,
                    vec![],
                    format!(
                        "private endpoint \"{pe_name}\" exists but there is no link from \"{}\" to \"{}\", so it gets no permission or address",
                        src.name, tgt.name
                    ),
                    vec![],
                );
            }
            let mut hops: Vec<Id> = sp.subnets.first().cloned().into_iter().collect();
            hops.push(pe);
            return path(
                Status::Ok,
                hops,
                format!("over private endpoint \"{pe_name}\""),
                vec![],
            );
        }
        // Reached over the provider API: needs a way out of the network + a link.
        let (net_ok, hops, reason) = match &sp.egress {
            Egress::Unrestricted | Egress::NotNeeded => (true, vec![], String::new()),
            Egress::Via(h) => (true, h.clone(), String::new()),
            Egress::Blocked(r) => (false, vec![], r.clone()),
        };
        if !net_ok {
            return path(
                Status::Blocked,
                hops,
                format!("no route out of the network: {reason}"),
                vec![],
            );
        }
        if !linked {
            return path(
                Status::Blocked,
                hops,
                format!(
                    "network path exists but there is no link from \"{}\" to \"{}\", so it gets no permission or address",
                    src.name, tgt.name
                ),
                vec![],
            );
        }
        let mut notes = vec![];
        if sp.egress == Egress::Unrestricted {
            notes.push("source is outside the network; no NAT or gateway involved".into());
        }
        return path(Status::Ok, hops, "reachable over the provider API".into(), notes);
    }

    // Networked target.
    let via_pe = |pe: Id| {
        let pe_name = name_of(&pe);
        let mut hops: Vec<Id> = sp.subnets.first().cloned().into_iter().collect();
        hops.push(pe);
        path(
            Status::Ok,
            hops,
            format!("over private endpoint \"{pe_name}\""),
            vec![],
        )
    };
    if tp.subnets.is_empty() {
        if let Some(pe) = pe {
            return via_pe(pe);
        }
        // e.g. a function outside the network, or an Azure public database.
        let status = if tp.exposed.is_some() {
            Status::Ok
        } else {
            Status::Unknown
        };
        let reason = match &tp.exposed {
            Some(how) => format!("reached through its {how}"),
            None => "target is outside the network and has no public entry point".into(),
        };
        return path(status, vec![], reason, vec![]);
    }
    if sp.subnets.is_empty() {
        // Source outside the network: only public entry points count.
        return match &tp.exposed {
            Some(how) => path(Status::Ok, vec![], format!("via its {how}"), vec![]),
            None => path(
                Status::Blocked,
                vec![],
                format!(
                    "\"{}\" is outside the network and \"{}\" is private",
                    src.name, tgt.name
                ),
                vec![],
            ),
        };
    }
    // Both inside networks: the same one, or two networks joined by a peering.
    let tv = tp.subnets.iter().filter_map(|s| vnet_of_subnet(p, cat, s)).next();
    let mut fabric: Vec<Id> = Vec::new();
    if let (Some(svn), Some(tvn)) = (sv.as_deref(), tv.as_deref()) {
        if svn != tvn {
            if let Some(pe) = pe {
                return via_pe(pe);
            }
            let Some(peer) = peering_between(p, cat, svn, tvn) else {
                return path(
                    Status::Blocked,
                    vec![],
                    "different virtual networks (no peering or private endpoint in the diagram)".into(),
                    vec![],
                );
            };
            let peer_name = name_of(&peer);
            if reach.provider == "aws" {
                let s_ok = sp
                    .subnets
                    .iter()
                    .any(|s| subnet_routes_via_peering(p, cat, s, &peer));
                let t_ok = tp
                    .subnets
                    .iter()
                    .any(|s| subnet_routes_via_peering(p, cat, s, &peer));
                if !s_ok || !t_ok {
                    let side = if !s_ok { src.name } else { tgt.name };
                    return path(
                        Status::Blocked,
                        vec![peer.clone()],
                        format!(
                            "peering \"{peer_name}\" exists but the route table of \"{side}\"'s subnet is not linked to it (no route across the peering)"
                        ),
                        vec![],
                    );
                }
            }
            fabric.push(peer);
        }
    }
    let port = listening_port(tgt);
    let mut notes = Vec::new();
    // Source egress rule (AWS only).
    let src_egress_ok = pol.egress_default_allow
        || sp.security_group.is_none()
        || rules_of(p, sp.security_group.as_deref().unwrap())
            .iter()
            .any(|r| {
                !r.ingress
                    && rule_matches_port(r, port)
                    && (r.cidr == "0.0.0.0/0"
                        || tp
                            .subnets
                            .iter()
                            .any(|s| subnet_cidr(p, s).is_some_and(|c| cidr_covers(&r.cidr, &c)))
                        || tp.security_group.as_deref() == Some(r.source_group.as_str()))
            });
    if !src_egress_ok {
        return path(
            Status::Blocked,
            vec![],
            format!(
                "\"{}\"'s security group has no egress rule allowing {}",
                src.name,
                port.map(|x| format!("port {x}")).unwrap_or("this traffic".into())
            ),
            notes,
        );
    }
    // Target ingress rule.
    let (status, reason) = match tp.security_group.as_deref() {
        None => {
            if pol.intra_network_default_allow {
                (
                    Status::Ok,
                    "allowed by the network's default rules (no security group on the target)".into(),
                )
            } else {
                (
                    Status::Unknown,
                    format!(
                        "\"{}\" has no security group; the VPC default group applies",
                        tgt.name
                    ),
                )
            }
        }
        Some(tsg) => {
            let matched = rules_of(p, tsg).into_iter().find(|r| {
                r.ingress
                    && rule_matches_port(r, port)
                    && (r.cidr == "0.0.0.0/0"
                        || (!r.source_group.is_empty()
                            && sp.security_group.as_deref() == Some(r.source_group.as_str()))
                        || (!r.cidr.is_empty()
                            && sp
                                .subnets
                                .iter()
                                .any(|s| subnet_cidr(p, s).is_some_and(|c| cidr_covers(&r.cidr, &c)))))
            });
            match matched {
                Some(r) if !r.source_group.is_empty() => (
                    Status::Ok,
                    format!("allowed by \"{}\": rule from security group", name_of(tsg)),
                ),
                Some(r) => (
                    Status::Ok,
                    format!("allowed by \"{}\": rule from {}", name_of(tsg), r.cidr),
                ),
                None => (
                    Status::Blocked,
                    format!(
                        "\"{}\" has no ingress rule allowing {} from \"{}\"",
                        name_of(tsg),
                        port.map(|x| format!("port {x}")).unwrap_or("this traffic".into()),
                        src.name
                    ),
                ),
            }
        }
    };
    if status == Status::Ok && !linked && tgt.resource_type == "relational_database" {
        notes.push(format!(
            "no 'Uses' link from \"{}\", so it receives no host name or credentials",
            src.name
        ));
    }
    let mut hops: Vec<Id> = Vec::new();
    if let Some(s) = sp.subnets.first() {
        hops.push(s.clone());
    }
    hops.extend(fabric);
    if let Some(sg) = &tp.security_group {
        hops.push(sg.clone());
    }
    path(status, hops, reason, notes)
}

/// Who can reach `target`: every initiating resource, with the path it would take.
/// The mirror image of [`paths_from`] for passive resources (databases, caches, queues).
pub fn paths_to(p: &Project, cat: &Catalog, reach: &Reach, target: &str) -> Vec<(Id, Path)> {
    let mut out = Vec::new();
    for sid in reach.posture.keys() {
        if sid == target {
            continue;
        }
        let Some(src) = p.entity(sid) else { continue };
        if !initiates(src.resource_type) {
            continue;
        }
        if let Some(path) = paths_from(p, cat, reach, sid)
            .into_iter()
            .find(|x| x.target == target)
        {
            out.push((sid.clone(), path));
        }
    }
    out
}
