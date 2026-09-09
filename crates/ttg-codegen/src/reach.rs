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
];

fn is_managed(t: &str) -> bool {
    MANAGED.contains(&t)
}

/// Types that open connections (as opposed to passive services that only listen).
pub fn initiates(t: &str) -> bool {
    matches!(
        t,
        "function" | "compute_instance" | "autoscaling_group" | "kubernetes_cluster"
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
        "kubernetes_cluster" => Some(443),
        _ => None,
    }
}

fn subnet_cidr(p: &Project, subnet: &str) -> Option<String> {
    p.entity(subnet)?.field("cidr_block").map(|v| v.display())
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

/// Paths from one resource to every other managed or networked resource.
pub fn paths_from(full: &Project, cat: &Catalog, reach: &Reach, source: &str) -> Vec<Path> {
    let layer = crate::layers::project_for(full, cat, &reach.provider);
    let p = &layer;
    let pol = policy(&reach.provider);
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
        let linked = p
            .edges_from(source)
            .any(|e| &e.target == tid && e.relation != Relation::DependsOn);

        if tp.managed {
            // Reached over the provider API: needs a way out of the network + a link.
            let (net_ok, hops, reason) = match &sp.egress {
                Egress::Unrestricted | Egress::NotNeeded => (true, vec![], String::new()),
                Egress::Via(h) => (true, h.clone(), String::new()),
                Egress::Blocked(r) => (false, vec![], r.clone()),
            };
            let (status, reason, mut notes) = if !net_ok {
                (
                    Status::Blocked,
                    format!("no route out of the network: {reason}"),
                    vec![],
                )
            } else if !linked {
                (
                    Status::Blocked,
                    format!(
                        "network path exists but there is no link from \"{}\" to \"{}\", so it gets no permission or address",
                        src.name, tgt.name
                    ),
                    vec![],
                )
            } else {
                (Status::Ok, "reachable over the provider API".into(), vec![])
            };
            if status == Status::Ok && sp.egress == Egress::Unrestricted {
                notes.push("source is outside the network; no NAT or gateway involved".into());
            }
            out.push(Path {
                target: tid.clone(),
                status,
                hops,
                reason,
                notes,
            });
            continue;
        }

        // Networked target.
        if tp.subnets.is_empty() {
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
            out.push(Path {
                target: tid.clone(),
                status,
                hops: vec![],
                reason,
                notes: vec![],
            });
            continue;
        }
        if sp.subnets.is_empty() {
            // Source outside the network: only public entry points count.
            let (status, reason) = match &tp.exposed {
                Some(how) => (Status::Ok, format!("via its {how}")),
                None => (
                    Status::Blocked,
                    format!(
                        "\"{}\" is outside the network and \"{}\" is private",
                        src.name, tgt.name
                    ),
                ),
            };
            out.push(Path {
                target: tid.clone(),
                status,
                hops: vec![],
                reason,
                notes: vec![],
            });
            continue;
        }
        // Both inside networks: must be the same one.
        let sv = sp.subnets.iter().filter_map(|s| vnet_of_subnet(p, cat, s)).next();
        let tv = tp.subnets.iter().filter_map(|s| vnet_of_subnet(p, cat, s)).next();
        if sv.is_some() && tv.is_some() && sv != tv {
            out.push(Path {
                target: tid.clone(),
                status: Status::Blocked,
                hops: vec![],
                reason: "different virtual networks (no peering in the diagram)".into(),
                notes: vec![],
            });
            continue;
        }
        let port = listening_port(&tgt);
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
            out.push(Path {
                target: tid.clone(),
                status: Status::Blocked,
                hops: vec![],
                reason: format!(
                    "\"{}\"'s security group has no egress rule allowing {}",
                    src.name,
                    port.map(|x| format!("port {x}")).unwrap_or("this traffic".into())
                ),
                notes,
            });
            continue;
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
                        format!(
                            "allowed by \"{}\": rule from security group",
                            p.entity(tsg).map(|x| x.name).unwrap_or("")
                        ),
                    ),
                    Some(r) => (
                        Status::Ok,
                        format!(
                            "allowed by \"{}\": rule from {}",
                            p.entity(tsg).map(|x| x.name).unwrap_or(""),
                            r.cidr
                        ),
                    ),
                    None => (
                        Status::Blocked,
                        format!(
                            "\"{}\" has no ingress rule allowing {} from \"{}\"",
                            p.entity(tsg).map(|x| x.name).unwrap_or(""),
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
        if let Some(sg) = &tp.security_group {
            hops.push(sg.clone());
        }
        out.push(Path {
            target: tid.clone(),
            status,
            hops,
            reason,
            notes,
        });
    }
    out
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
