//! Reachability analysis: where can each resource send traffic, what is exposed to the
//! internet, and can resource A actually talk to resource B?
//!
//! The analysis works on the abstract diagram plus a small per-provider policy (AWS
//! security groups deny by default in both directions; Azure NSGs allow VNet-internal
//! and outbound traffic by default). It is deliberately conservative: when a rule cannot
//! be decided from the diagram the answer is `Unknown`, never a silent "ok".

use crate::diagnostics::{relation_targets, Severity};
use std::collections::BTreeMap;
use ttg_catalog::Catalog;
use ttg_core::{EntityRef, Id, Project, Relation, Value};

/// Stands for traffic from outside every network: the source [`paths_to`] and [`audit`]
/// name for a path to something internet-facing.
pub const INTERNET: &str = "internet";

/// How a source is named in a sentence: an entity by its name, [`INTERNET`] as such.
pub fn source_name(p: &Project, id: &str) -> String {
    if id == INTERNET {
        return "the internet".into();
    }
    p.entity(id)
        .map(|e| e.name.to_string())
        .unwrap_or_else(|| id.to_string())
}

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
    /// The groups whose rules apply to it (see [`security_groups_of`]): Security Group
    /// entities, and a Kubernetes Cluster's id for the security group its provider
    /// creates for the cluster.
    pub security_groups: Vec<Id>,
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

impl Status {
    pub fn key(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Blocked => "blocked",
            Status::Unknown => "unknown",
        }
    }
    pub fn from_key(s: &str) -> Option<Status> {
        [Status::Ok, Status::Blocked, Status::Unknown]
            .into_iter()
            .find(|x| x.key().eq_ignore_ascii_case(s.trim()))
    }
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
    "kubernetes_workload",
    "autoscaling_group",
    "function",
    "container_app",
    "container_job",
    "file_system",
];

pub(crate) fn is_managed(t: &str) -> bool {
    MANAGED.contains(&t)
}

/// Types that open connections (as opposed to passive services that only listen).
pub fn initiates(t: &str) -> bool {
    matches!(
        t,
        "function"
            | "compute_instance"
            | "autoscaling_group"
            | "kubernetes_cluster"
            | "kubernetes_workload"
            | "container_app"
            | "container_job"
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
        // Custom-mode VPC: implied deny-ingress / allow-egress; no internal allow.
        "gcp" => Policy {
            intra_network_default_allow: false,
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

/// Targets of a relation that are of one abstract type.
fn targets_of_type<'p>(
    p: &'p Project,
    cat: &Catalog,
    e: &EntityRef,
    kind: Relation,
    ty: &str,
) -> Vec<EntityRef<'p>> {
    relation_targets(p, cat, e, kind)
        .into_iter()
        .filter_map(|t| p.entity(&t))
        .filter(|x| x.resource_type == ty)
        .collect()
}

/// Where the nodes that run a pool or a workload come from. A node pool runs on its
/// cluster; a workload runs on the pools it schedules on (any link to a node pool), or
/// else on its cluster's nodes. Both have no network presence of their own.
enum Hosts<'p> {
    Pools(Vec<EntityRef<'p>>),
    Clusters(Vec<EntityRef<'p>>),
}

fn hosts_of<'p>(p: &'p Project, cat: &Catalog, e: &EntityRef) -> Option<Hosts<'p>> {
    match e.resource_type {
        "kubernetes_node_pool" => Some(Hosts::Clusters(targets_of_type(
            p,
            cat,
            e,
            Relation::Attachment,
            "kubernetes_cluster",
        ))),
        "kubernetes_workload" => {
            let pools: Vec<EntityRef> = p
                .edges_from(e.id)
                .filter(|x| !matches!(x.relation, Relation::DependsOn | Relation::Calls))
                .filter_map(|x| p.entity(&x.target))
                .filter(|x| x.resource_type == "kubernetes_node_pool")
                .collect();
            Some(if pools.is_empty() {
                Hosts::Clusters(targets_of_type(
                    p,
                    cat,
                    e,
                    Relation::Attachment,
                    "kubernetes_cluster",
                ))
            } else {
                Hosts::Pools(pools)
            })
        }
        _ => None,
    }
}

fn subnets_of(p: &Project, cat: &Catalog, e: &EntityRef) -> Vec<Id> {
    let own: Vec<Id> = targets_of_type(p, cat, e, Relation::NetworkMembership, "subnet")
        .into_iter()
        .map(|s| s.id.to_string())
        .collect();
    // A pool's own subnets win over its cluster's; a workload has none of its own.
    if !own.is_empty() && e.resource_type != "kubernetes_workload" {
        return own;
    }
    match hosts_of(p, cat, e) {
        Some(Hosts::Pools(v) | Hosts::Clusters(v)) => v.iter().flat_map(|h| subnets_of(p, cat, h)).collect(),
        None => own,
    }
}

/// Security Group entities linked to `e`, whichever relation the type uses for it
/// ("Uses security group" is `attribute_reference`; a private endpoint's is `attachment`).
fn linked_groups(p: &Project, cat: &Catalog, e: &EntityRef) -> Vec<Id> {
    [Relation::AttributeReference, Relation::Attachment]
        .into_iter()
        .flat_map(|k| targets_of_type(p, cat, e, k, "security_group"))
        .map(|g| g.id.to_string())
        .collect()
}

/// The groups whose rules apply to `e`. A Kubernetes Cluster carries its linked groups
/// plus its own id, which stands for the security group the provider creates for the
/// cluster (what a rule whose source is the cluster admits). A node pool carries its own
/// linked groups, or else its cluster's, and always its cluster's own group; a workload
/// carries the groups of the nodes it runs on.
pub(crate) fn security_groups_of(p: &Project, cat: &Catalog, e: &EntityRef) -> Vec<Id> {
    let mut out = linked_groups(p, cat, e);
    match e.resource_type {
        "kubernetes_cluster" => out.push(e.id.to_string()),
        "kubernetes_node_pool" => {
            for c in targets_of_type(p, cat, e, Relation::Attachment, "kubernetes_cluster") {
                if out.is_empty() {
                    out = security_groups_of(p, cat, &c);
                } else {
                    out.push(c.id.to_string());
                }
            }
        }
        "kubernetes_workload" => {
            out = match hosts_of(p, cat, e) {
                Some(Hosts::Pools(v) | Hosts::Clusters(v)) => {
                    v.iter().flat_map(|h| security_groups_of(p, cat, h)).collect()
                }
                None => Vec::new(),
            };
        }
        _ => {}
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|g| seen.insert(g.clone()));
    out
}

/// Does anything carry `group`? A cluster's own group always has its nodes.
pub(crate) fn group_has_members(p: &Project, cat: &Catalog, group: &str) -> bool {
    if p.entity(group)
        .is_some_and(|g| g.resource_type == "kubernetes_cluster")
    {
        return true;
    }
    p.entities()
        .iter()
        .filter(|e| !e.is_container && e.id != group)
        .any(|e| security_groups_of(p, cat, e).iter().any(|g| g == group))
}

/// Security Groups that some rule names as its source (`source_group`) although nothing
/// carries them, each with the groups whose rules name it. Those rules admit no one.
pub(crate) fn memberless_sources(p: &Project, cat: &Catalog) -> Vec<(Id, Vec<Id>)> {
    let mut named: BTreeMap<Id, Vec<Id>> = BTreeMap::new();
    for sg in p
        .entities()
        .iter()
        .filter(|e| e.resource_type == "security_group")
    {
        for r in rules_of(p, sg.id) {
            let is_group = p
                .entity(&r.source_group)
                .is_some_and(|s| s.resource_type == "security_group");
            if is_group {
                let users = named.entry(r.source_group.clone()).or_default();
                if !users.iter().any(|u| u == sg.id) {
                    users.push(sg.id.to_string());
                }
            }
        }
    }
    named
        .into_iter()
        .filter(|(g, _)| !group_has_members(p, cat, g))
        .collect()
}

/// The resources an interface endpoint refuses on 443 when it admits none of the ones in
/// its network: none of its groups' ingress rules covers their subnets or names a group
/// they carry. `None` when it admits one of them, when it has no group (the VPC default
/// group applies, which the diagram cannot see), or when nothing in the network opens
/// connections at all.
pub(crate) fn endpoint_refuses_all(p: &Project, cat: &Catalog, pe: &EntityRef) -> Option<Vec<Id>> {
    let groups = linked_groups(p, cat, pe);
    let vnet = subnets_of(p, cat, pe)
        .iter()
        .find_map(|s| vnet_of_subnet(p, cat, s))
        .or_else(|| p.ancestor_of_type(pe.id, "virtual_network").map(|c| c.id.clone()))?;
    if groups.is_empty() {
        return None;
    }
    let clients: Vec<(EntityRef, Vec<Id>)> = p
        .entities()
        .into_iter()
        .filter(|e| !e.is_container && initiates(e.resource_type))
        .map(|e| {
            let subnets = subnets_of(p, cat, &e);
            (e, subnets)
        })
        .filter(|(_, subnets)| {
            subnets
                .iter()
                .any(|s| vnet_of_subnet(p, cat, s).as_deref() == Some(vnet.as_str()))
        })
        .collect();
    if clients.is_empty() {
        return None;
    }
    let admits = |r: &Rule, (client, subnets): &(EntityRef, Vec<Id>)| {
        r.cidr == "0.0.0.0/0"
            || (!r.cidr.is_empty()
                && subnets
                    .iter()
                    .any(|s| subnet_cidr(p, s).is_some_and(|c| cidr_covers(&r.cidr, &c))))
            || (!r.source_group.is_empty() && security_groups_of(p, cat, client).contains(&r.source_group))
    };
    let admitted = groups
        .iter()
        .flat_map(|g| rules_of(p, g))
        .filter(|r| r.ingress && rule_matches_port(r, Some(443)))
        .any(|r| clients.iter().any(|c| admits(&r, c)));
    (!admitted).then(|| clients.iter().map(|(e, _)| e.id.to_string()).collect())
}

/// How a group is named in a reason: a Security Group by its name, a cluster's own group
/// as that.
fn group_label(p: &Project, id: &str) -> String {
    match p.entity(id) {
        Some(e) if e.resource_type == "kubernetes_cluster" => {
            format!("the cluster security group of \"{}\"", e.name)
        }
        Some(e) => format!("\"{}\"", e.name),
        None => format!("\"{id}\""),
    }
}

fn vnet_of_subnet(p: &Project, cat: &Catalog, subnet: &str) -> Option<Id> {
    let e = p.entity(subnet)?;
    relation_targets(p, cat, &e, Relation::NetworkMembership)
        .into_iter()
        .next()
}

/// Hops from a subnet to the internet: route table, then NAT (and its own subnet's
/// route table + internet gateway) or the internet gateway directly. On GCP route
/// tables are logical: Cloud NAT covers the whole network and the default internet
/// route always exists, so the answer is the network's NAT (or its internet gateway).
fn subnet_egress(p: &Project, cat: &Catalog, provider: &str, subnet: &str) -> Result<Vec<Id>, String> {
    if provider == "gcp" {
        let vnet = vnet_of_subnet(p, cat, subnet);
        let in_vnet = |e: &EntityRef| {
            p.ancestor_of_type(e.id, "virtual_network").map(|c| c.id.as_str()) == vnet.as_deref()
                || relation_targets(p, cat, e, Relation::NetworkMembership)
                    .iter()
                    .any(|t| {
                        p.entity(t).is_some_and(|x| x.resource_type == "virtual_network")
                            && Some(t.as_str()) == vnet.as_deref()
                            || vnet_of_subnet(p, cat, t) == vnet
                    })
        };
        let ents = p.entities();
        if let Some(nat) = ents
            .iter()
            .find(|e| e.resource_type == "nat_gateway" && in_vnet(e))
        {
            return Ok(vec![nat.id.to_string()]);
        }
        if let Some(igw) = ents
            .iter()
            .find(|e| e.resource_type == "internet_gateway" && in_vnet(e))
        {
            return Ok(vec![igw.id.to_string()]);
        }
        return Err("the network has no Cloud NAT (NAT Gateway) and no Internet Gateway".into());
    }
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
                        if let Ok(rest) = subnet_egress(p, cat, provider, &ns) {
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

/// Rules of a security group as (direction, protocol, from, to, cidr, source_group,
/// prefix list).
struct Rule {
    ingress: bool,
    protocol: String,
    from: i64,
    to: i64,
    cidr: String,
    source_group: String,
    /// The name of the managed prefix list the rule admits (AWS), which wins over the
    /// rest of the source: an opaque set of addresses outside the diagram.
    prefix_list: String,
}

/// The CloudFront origin-facing list: the addresses CloudFront fetches from.
const CLOUDFRONT_ORIGIN_FACING: &str = "com.amazonaws.global.cloudfront.origin-facing";

/// The prefix list a rule names: the AWS-managed list's name, or the name a prefix
/// list on the canvas looks up (its `name` argument, else its display name).
fn rule_prefix_list(p: &Project, r: &ttg_core::Record) -> String {
    if let Some(name) = r
        .get("prefix_list")
        .map(|v| v.display())
        .filter(|s| !s.is_empty())
    {
        return name;
    }
    r.get("source_prefix_list")
        .and_then(|v| v.as_str())
        .and_then(|id| p.entity(id))
        .map(|e| {
            e.extra_args("aws", "main")
                .and_then(|x| x.get("name"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or(e.name.to_string())
        })
        .unwrap_or_default()
}

/// A group's rules. A cluster's own group is the one its provider makes: all traffic
/// between its members, and all traffic out (EKS's cluster security group, GKE's
/// intra-cluster firewall rules; on Azure the node subnets are what the rules admit).
fn rules_of(p: &Project, sg: &str) -> Vec<Rule> {
    let Some(e) = p.entity(sg) else { return vec![] };
    if e.resource_type == "kubernetes_cluster" {
        let all = |ingress: bool, cidr: &str, source_group: &str| Rule {
            ingress,
            protocol: "all".into(),
            from: 0,
            to: 0,
            cidr: cidr.into(),
            source_group: source_group.into(),
            prefix_list: String::new(),
        };
        return vec![all(true, "", sg), all(false, "0.0.0.0/0", "")];
    }
    let Some(Value::Records(rows)) = e.field("rules") else {
        return vec![];
    };
    rows.iter()
        .map(|r| {
            let prefix_list = rule_prefix_list(p, r);
            // A prefix list replaces the rest of the source (the AWS mapping drops it).
            let other = |k: &str| {
                if prefix_list.is_empty() {
                    r.get(k).map(|v| v.display()).unwrap_or_default()
                } else {
                    String::new()
                }
            };
            Rule {
                ingress: r.get("direction").map(|v| v.display()) == Some("ingress".into()),
                protocol: r.get("protocol").map(|v| v.display()).unwrap_or("tcp".into()),
                from: r.get("from_port").and_then(|v| v.as_int()).unwrap_or(0),
                to: r.get("to_port").and_then(|v| v.as_int()).unwrap_or(0),
                cidr: other("cidr"),
                source_group: other("source_group"),
                prefix_list,
            }
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
        // A background worker listens for nothing.
        "container_app" if !is_worker(e) => e.field("port").and_then(|v| v.as_int()),
        "kubernetes_cluster" => Some(443),
        "file_system" => Some(2049),
        _ => None,
    }
}

/// A Container App marked as a background worker: no ingress at all.
fn is_worker(e: &EntityRef) -> bool {
    e.field("background_worker")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// A relation label as it reads in a sentence: "Uses (read / write): queue, database, …"
/// is "Uses", "Mounts (file system)" is "Mounts", "Sends to" stays.
fn short_label(label: &str) -> String {
    label.split(['(', ':']).next().unwrap_or(label).trim().to_string()
}

/// What the source type calls its link to a target type ("Uses", "Mounts"), when it has
/// one, preferring a declaration of the given kind.
fn link_label(cat: &Catalog, source_type: &str, target_type: &str, kind: Option<Relation>) -> Option<String> {
    let def = cat.resource(source_type)?;
    let fits = |r: &&ttg_catalog::RelationDef| r.targets.iter().any(|t| t == target_type);
    let r = def
        .relations
        .iter()
        .filter(fits)
        .find(|r| kind.is_some_and(|k| r.kind == k.key()))
        .or_else(|| def.relations.iter().find(fits))?;
    Some(short_label(r.label.as_deref().unwrap_or(&r.kind)))
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

/// The AWS service an endpoint serves: its `service` field; without one, the service of
/// what it "Connects to"; else the field's default, which is what the export generates.
fn endpoint_service(p: &Project, cat: &Catalog, pe: &EntityRef) -> String {
    if let Some(s) = pe
        .provider_field("aws", "service")
        .map(|v| v.display())
        .filter(|s| !s.is_empty())
    {
        return s;
    }
    let implied = relation_targets(p, cat, pe, Relation::AttributeReference)
        .iter()
        .filter_map(|t| p.entity(t))
        .find_map(|t| match t.resource_type {
            "event_queue" => Some("sqs"),
            "object_storage" => Some("s3"),
            "nosql_table" => Some("dynamodb"),
            "key_vault" | "secret" => Some("secretsmanager"),
            "relational_database" => Some("rds"),
            _ => None,
        });
    if let Some(s) = implied {
        return s.to_string();
    }
    crate::diagnostics::field_or_default(cat, "aws", pe, "service", true)
        .map(|v| v.display())
        .unwrap_or_default()
}

/// The abstract types an AWS endpoint service answers for, every resource of them in the
/// region: an SQS endpoint carries the calls to every queue. `rds` is the management API,
/// not the database connection, so it covers nothing here.
fn service_types(service: &str) -> &'static [&'static str] {
    match service {
        "sqs" => &["event_queue"],
        "sns" => &["topic"],
        "secretsmanager" => &["secret"],
        "logs" => &["log_group"],
        "ecr.api" | "ecr.dkr" => &["container_registry"],
        "s3" => &["object_storage"],
        "dynamodb" => &["nosql_table"],
        _ => &[],
    }
}

/// Gateway endpoints install routes rather than an interface, and have no security group.
fn is_gateway_service(service: &str) -> bool {
    matches!(service, "s3" | "dynamodb")
}

/// Does endpoint `pe` front `target` by its own link: "Connects to" the target, or the
/// container holding it (a key vault's secrets, a namespace's queues)?
fn endpoint_links(p: &Project, cat: &Catalog, pe: &EntityRef, target: &str) -> bool {
    relation_targets(p, cat, pe, Relation::AttributeReference)
        .iter()
        .any(|t| t == target || p.is_ancestor(t, target))
}

/// Does endpoint `pe` carry traffic to `target`? An AWS endpoint fronts a whole service,
/// so it covers every resource of that service whatever it links; only managed services
/// count, since an `rds` endpoint never carries a database connection. Elsewhere a private
/// endpoint fronts the one resource it connects to.
fn endpoint_covers(p: &Project, cat: &Catalog, provider: &str, pe: &EntityRef, target: &EntityRef) -> bool {
    if provider == "aws" {
        is_managed(target.resource_type)
            && (endpoint_links(p, cat, pe, target.id)
                || service_types(&endpoint_service(p, cat, pe)).contains(&target.resource_type))
    } else {
        endpoint_links(p, cat, pe, target.id)
    }
}

/// The network an endpoint sits in: through its subnets, the subnets of the route tables
/// it is linked to (a gateway endpoint), or the network drawn around it.
fn endpoint_network(p: &Project, cat: &Catalog, pe: &EntityRef) -> Option<Id> {
    subnets_of(p, cat, pe)
        .iter()
        .find_map(|s| vnet_of_subnet(p, cat, s))
        .or_else(|| {
            targets_of_type(p, cat, pe, Relation::Attachment, "route_table")
                .iter()
                .flat_map(|rt| relation_targets(p, cat, rt, Relation::Attachment))
                .find_map(|s| vnet_of_subnet(p, cat, &s))
        })
        .or_else(|| p.ancestor_of_type(pe.id, "virtual_network").map(|c| c.id.clone()))
}

/// Does endpoint `pe` answer for traffic from these subnets? On AWS a gateway endpoint
/// installs routes, so it serves the subnets whose route table it is linked to (with no
/// route table linked, the subnets it is linked to); an interface endpoint with private
/// DNS answers for its service everywhere in its network. Elsewhere a private endpoint
/// serves its whole network.
fn endpoint_serves(p: &Project, cat: &Catalog, provider: &str, pe: &EntityRef, src_subnets: &[Id]) -> bool {
    if provider == "aws" && is_gateway_service(&endpoint_service(p, cat, pe)) {
        let tables = targets_of_type(p, cat, pe, Relation::Attachment, "route_table");
        if tables.is_empty() {
            return subnets_of(p, cat, pe).iter().any(|s| src_subnets.contains(s));
        }
        return tables.iter().any(|rt| {
            relation_targets(p, cat, rt, Relation::Attachment)
                .iter()
                .any(|s| src_subnets.contains(s))
        });
    }
    let src_vnet = src_subnets.iter().find_map(|s| vnet_of_subnet(p, cat, s));
    src_vnet.is_some() && endpoint_network(p, cat, pe) == src_vnet
}

/// The private endpoint that carries traffic from these subnets to `target`, if one does.
/// An endpoint that links the target wins over one that covers it by service.
pub(crate) fn private_endpoint_for(
    p: &Project,
    cat: &Catalog,
    provider: &str,
    src_subnets: &[Id],
    target: &str,
) -> Option<Id> {
    let tgt = p.entity(target)?;
    if src_subnets.is_empty() {
        return None;
    }
    let serving: Vec<EntityRef> = p
        .entities()
        .into_iter()
        .filter(|e| e.resource_type == "private_endpoint")
        .filter(|pe| {
            endpoint_covers(p, cat, provider, pe, &tgt) && endpoint_serves(p, cat, provider, pe, src_subnets)
        })
        .collect();
    serving
        .iter()
        .find(|pe| endpoint_links(p, cat, pe, target))
        .or(serving.first())
        .map(|e| e.id.to_string())
}

/// An endpoint in the source's network for the target's service that does not carry this
/// source's traffic, with what would make it: traffic that leaves through a NAT for a
/// service the network has an endpoint for pays NAT processing and crosses the internet
/// edge for nothing.
fn unused_endpoint(
    p: &Project,
    cat: &Catalog,
    provider: &str,
    src_subnets: &[Id],
    target: &EntityRef,
) -> Option<(Id, String)> {
    let src_vnet = src_subnets.iter().find_map(|s| vnet_of_subnet(p, cat, s))?;
    let name_of = |id: &str| p.entity(id).map(|e| e.name.to_string()).unwrap_or_default();
    p.entities()
        .into_iter()
        .filter(|e| e.resource_type == "private_endpoint")
        .filter(|pe| endpoint_network(p, cat, pe).as_deref() == Some(src_vnet.as_str()))
        .find_map(|pe| {
            if provider == "aws" {
                let service = endpoint_service(p, cat, &pe);
                if !endpoint_covers(p, cat, provider, &pe, target) {
                    return None;
                }
                let why = if is_gateway_service(&service) {
                    format!(
                        "it is a gateway endpoint for {service} and no route table of {} is linked to it",
                        src_subnets
                            .first()
                            .map(|s| format!("\"{}\"", name_of(s)))
                            .unwrap_or_default()
                    )
                } else {
                    format!("it does not serve {service} to this subnet")
                };
                Some((pe.id.to_string(), why))
            } else {
                let fronts: Vec<Id> = relation_targets(p, cat, &pe, Relation::AttributeReference);
                let same_kind = fronts
                    .iter()
                    .filter_map(|t| p.entity(t))
                    .any(|t| t.resource_type == target.resource_type);
                same_kind.then(|| {
                    (
                        pe.id.to_string(),
                        format!(
                            "a private endpoint fronts only the resource it connects to ({}); add one for \"{}\"",
                            fronts.iter().map(|t| format!("\"{}\"", name_of(t))).collect::<Vec<_>>().join(", "),
                            target.name
                        ),
                    )
                })
            }
        })
}

/// The first ingress rule of `groups` that admits a client carrying `client_groups` from
/// `client_subnets` on `port`: an open range, a group the client carries, or a range that
/// covers one of its subnets.
fn admitting_rule(
    p: &Project,
    groups: &[Id],
    client_groups: &[Id],
    client_subnets: &[Id],
    port: Option<i64>,
) -> Option<(Id, Rule)> {
    groups.iter().find_map(|g| {
        rules_of(p, g)
            .into_iter()
            .find(|r| {
                r.ingress
                    && rule_matches_port(r, port)
                    && (r.cidr == "0.0.0.0/0"
                        || (!r.source_group.is_empty() && client_groups.contains(&r.source_group))
                        || (!r.cidr.is_empty()
                            && client_subnets
                                .iter()
                                .any(|s| subnet_cidr(p, s).is_some_and(|c| cidr_covers(&r.cidr, &c)))))
            })
            .map(|r| (g.clone(), r))
    })
}

/// Does one of the source's groups let traffic out to the destination on `port`? Azure
/// and Google Cloud allow outbound by default; so does a source with no group.
fn egress_allowed(
    p: &Project,
    pol: &Policy,
    src_groups: &[Id],
    dst_groups: &[Id],
    dst_subnets: &[Id],
    port: Option<i64>,
) -> bool {
    pol.egress_default_allow
        || src_groups.is_empty()
        || src_groups.iter().flat_map(|g| rules_of(p, g)).any(|r| {
            !r.ingress
                && rule_matches_port(&r, port)
                && (r.cidr == "0.0.0.0/0"
                    || dst_subnets
                        .iter()
                        .any(|s| subnet_cidr(p, s).is_some_and(|c| cidr_covers(&r.cidr, &c)))
                    || (!r.source_group.is_empty() && dst_groups.contains(&r.source_group)))
        })
}

/// Does workload `w` run on `cluster`, directly or on one of its pools?
fn runs_on(p: &Project, cat: &Catalog, w: &EntityRef, cluster: &str) -> bool {
    targets_of_type(p, cat, w, Relation::Attachment, "kubernetes_cluster")
        .iter()
        .any(|c| c.id == cluster)
        || matches!(hosts_of(p, cat, w), Some(Hosts::Pools(pools)) if pools.iter().any(|pool| {
            targets_of_type(p, cat, pool, Relation::Attachment, "kubernetes_cluster")
                .iter()
                .any(|c| c.id == cluster)
        }))
}

/// One thing a load balancer forwards to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forward {
    pub target: Id,
    /// The port the traffic arrives on.
    pub port: Option<i64>,
    /// A workload on a cluster whose nodes the load balancer forwards to, when no
    /// workload says it receives that traffic: the network delivers it to the nodes, but
    /// the diagram does not say which pods get it.
    pub assumed: bool,
}

/// What a load balancer forwards to. The instances and apps it links ("Forwards to")
/// get its target port, a container app its own port (the target group is of type ip, on
/// the app's port); for a cluster, the workloads that say they receive its traffic
/// ("Receives traffic from"), each on its own port or else the target port, or, when no
/// workload says so, the cluster's nodes on the target port and every workload on it as
/// an assumed target; and every autoscaling group registered with it.
pub fn forwarding_targets(p: &Project, cat: &Catalog, lb: &EntityRef) -> Vec<Forward> {
    let target_port = lb.field("target_port").and_then(|v| v.as_int());
    let port_of = |e: &EntityRef| e.field("port").and_then(|v| v.as_int()).or(target_port);
    let mut out: Vec<Forward> = Vec::new();
    let mut push = |id: &str, port: Option<i64>, assumed: bool| {
        if !out.iter().any(|f| f.target == id) {
            out.push(Forward {
                target: id.to_string(),
                port,
                assumed,
            });
        }
    };
    let incoming: Vec<EntityRef> = p
        .edges_to(lb.id)
        .filter(|x| x.relation == Relation::Attachment)
        .filter_map(|x| p.entity(&x.source))
        .collect();
    let receivers: Vec<&EntityRef> = incoming
        .iter()
        .filter(|e| e.resource_type == "kubernetes_workload")
        .collect();
    let ents = p.entities();
    for t in relation_targets(p, cat, lb, Relation::Attachment) {
        let Some(te) = p.entity(&t) else { continue };
        match te.resource_type {
            "kubernetes_cluster" => {
                let on: Vec<&&EntityRef> = receivers.iter().filter(|w| runs_on(p, cat, w, &t)).collect();
                for w in &on {
                    push(w.id, port_of(w), false);
                }
                if on.is_empty() {
                    push(&t, target_port, false);
                    for w in ents
                        .iter()
                        .filter(|w| w.resource_type == "kubernetes_workload" && runs_on(p, cat, w, &t))
                    {
                        push(w.id, target_port, true);
                    }
                }
            }
            "container_app" => push(&t, port_of(&te), false),
            "compute_instance" => push(&t, target_port, false),
            _ => {}
        }
    }
    for w in &receivers {
        push(w.id, port_of(w), false);
    }
    for asg in incoming.iter().filter(|e| e.resource_type == "autoscaling_group") {
        push(asg.id, target_port, false);
    }
    out
}

/// Load balancers that forward to `target`, each with how.
fn load_balancers_for(p: &Project, cat: &Catalog, target: &str) -> Vec<(Id, Forward)> {
    p.entities()
        .into_iter()
        .filter(|e| e.resource_type == "load_balancer")
        .filter_map(|lb| {
            forwarding_targets(p, cat, &lb)
                .into_iter()
                .find(|f| f.target == target)
                .map(|f| (lb.id.to_string(), f))
        })
        .collect()
}

/// The path a load balancer's forwarded traffic takes to `tgt`. An assumed receiver (see
/// [`Forward::assumed`]) that the network would let through is undecided: the diagram
/// does not say the pods behind the nodes are this workload's.
#[allow(clippy::too_many_arguments)]
fn forward_leg(
    p: &Project,
    cat: &Catalog,
    reach: &Reach,
    lb: &EntityRef,
    lp: &Posture,
    tgt: &EntityRef,
    tp: &Posture,
    f: &Forward,
) -> Path {
    let mut path = direct_path(p, cat, reach, lb, lp, tgt, tp, f.port);
    let on_port = f.port.map(|x| format!(" on port {x}")).unwrap_or_default();
    if !f.assumed {
        path.reason = format!("forwards to it{on_port}: {}", path.reason);
        return path;
    }
    let cluster = relation_targets(p, cat, lb, Relation::Attachment)
        .into_iter()
        .filter_map(|c| p.entity(&c))
        .find(|c| c.resource_type == "kubernetes_cluster" && runs_on(p, cat, tgt, c.id))
        .map(|c| c.name.to_string())
        .unwrap_or_default();
    if path.status == Status::Ok {
        path.notes
            .insert(0, format!("the nodes admit it: {}", path.reason));
        path.status = Status::Unknown;
        path.reason = format!(
            "\"{}\" forwards to the nodes of \"{cluster}\"{on_port}, but nothing says \"{}\" receives that traffic (link it with 'Receives traffic from')",
            lb.name, tgt.name
        );
    } else {
        path.reason = format!("forwards to the nodes of \"{cluster}\"{on_port}: {}", path.reason);
    }
    path
}
// Compute the posture of every resource for a provider.
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
        let security_groups = security_groups_of(p, cat, &e);
        let egress = if subnets.is_empty() {
            Egress::Unrestricted
        } else if !initiates(e.resource_type) {
            Egress::NotNeeded
        } else {
            // Any subnet with a route out is enough.
            let mut found: Option<Vec<Id>> = None;
            let mut last_err = String::new();
            for s in &subnets {
                match subnet_egress(p, cat, provider, s) {
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
                    // Some security group must allow outbound (AWS); Azure allows by default.
                    let allowed = pol.egress_default_allow
                        || security_groups.is_empty()
                        || security_groups.iter().flat_map(|g| rules_of(p, g)).any(|r| {
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
        let exposed = exposure(p, cat, provider, &e, &subnets, &security_groups);
        posture.insert(
            e.id.to_string(),
            Posture {
                subnets,
                security_groups,
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
    groups: &[Id],
) -> Option<String> {
    match e.resource_type {
        "function" if e.field("http_trigger").and_then(|v| v.as_bool()).unwrap_or(false) => {
            return Some("public HTTPS endpoint".into());
        }
        "load_balancer" if e.field("scheme").map(|v| v.display()) == Some("internet_facing".into()) => {
            // Locked to CloudFront: every ingress rule admits only its origin-facing list.
            let ingress: Vec<Rule> = groups
                .iter()
                .flat_map(|g| rules_of(p, g))
                .filter(|r| r.ingress)
                .collect();
            if !ingress.is_empty() && ingress.iter().all(|r| r.prefix_list == CLOUDFRONT_ORIGIN_FACING) {
                return Some(
                    "internet-facing load balancer that admits only CloudFront (origin-facing prefix list)"
                        .into(),
                );
            }
            return Some("internet-facing load balancer".into());
        }
        "container_app" if e.field("public").and_then(|v| v.as_bool()).unwrap_or(false) && !is_worker(e) => {
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
        let igw = subnet_egress(p, cat, provider, s)
            .map(|h| {
                // Directly through an internet gateway, not via a NAT.
                h.iter()
                    .any(|x| p.entity(x).is_some_and(|y| y.resource_type == "internet_gateway"))
                    && !h
                        .iter()
                        .any(|x| p.entity(x).is_some_and(|y| y.resource_type == "nat_gateway"))
            })
            .unwrap_or(false);
        let gcp_public = provider == "gcp"
            && e.provider_field("gcp", "public_ip")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
        igw && (provider == "azure" || (provider == "aws" && public_ip) || gcp_public)
    });
    if !public_subnet {
        return None;
    }
    let open = groups
        .iter()
        .flat_map(|g| rules_of(p, g))
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
/// cannot be reached directly is retried through a load balancer that forwards to it; a
/// load balancer's own paths to what it forwards to use the port it forwards on.
pub fn paths_from(full: &Project, cat: &Catalog, reach: &Reach, source: &str) -> Vec<Path> {
    let layer = crate::layers::project_for(full, cat, &reach.provider);
    paths_from_in(&layer, cat, reach, source)
}

fn paths_from_in(p: &Project, cat: &Catalog, reach: &Reach, source: &str) -> Vec<Path> {
    let Some(src) = p.entity(source) else {
        return vec![];
    };
    let Some(sp) = reach.posture.get(source) else {
        return vec![];
    };
    let forwards = forwards_of(p, cat, &src);
    reach
        .posture
        .iter()
        .filter(|(tid, _)| tid.as_str() != source)
        .filter_map(|(tid, tp)| path_in(p, cat, reach, &src, sp, &forwards, tid, tp))
        .collect()
}

fn forwards_of(p: &Project, cat: &Catalog, src: &EntityRef) -> Vec<Forward> {
    if src.resource_type == "load_balancer" {
        forwarding_targets(p, cat, src)
    } else {
        Vec::new()
    }
}

/// The path from `source` to `target` alone (see [`paths_from`]), on a layer.
fn path_between(p: &Project, cat: &Catalog, reach: &Reach, source: &str, target: &str) -> Option<Path> {
    let src = p.entity(source)?;
    let sp = reach.posture.get(source)?;
    let tp = reach.posture.get(target)?;
    path_in(p, cat, reach, &src, sp, &forwards_of(p, cat, &src), target, tp)
}

#[allow(clippy::too_many_arguments)]
fn path_in(
    p: &Project,
    cat: &Catalog,
    reach: &Reach,
    src: &EntityRef,
    sp: &Posture,
    forwards: &[Forward],
    tid: &str,
    tp: &Posture,
) -> Option<Path> {
    let tgt = p.entity(tid)?;
    if let Some(f) = forwards.iter().find(|f| f.target == tid) {
        return Some(forward_leg(p, cat, reach, src, sp, &tgt, tp, f));
    }
    let mut path = direct_path(p, cat, reach, src, sp, &tgt, tp, listening_port(&tgt));
    if path.status != Status::Ok && !tp.managed && tgt.resource_type != "load_balancer" {
        for (lb, f) in load_balancers_for(p, cat, tid) {
            if lb == src.id {
                continue;
            }
            let (Some(lbe), Some(lp)) = (p.entity(&lb), reach.posture.get(&lb)) else {
                continue;
            };
            let first = direct_path(p, cat, reach, src, sp, &lbe, lp, listening_port(&lbe));
            if first.status != Status::Ok {
                path.notes
                    .push(format!("via load balancer \"{}\": {}", lbe.name, first.reason));
                continue;
            }
            let second = forward_leg(p, cat, reach, &lbe, lp, &tgt, tp, &f);
            if second.status == Status::Ok {
                let mut hops = first.hops.clone();
                hops.push(lb.clone());
                hops.extend(second.hops.iter().cloned());
                path = Path {
                    target: tid.to_string(),
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
    Some(path)
}

/// One hop: can `src` talk to `tgt` on `port` without an intermediary (other than
/// gateways, peerings and private endpoints, which are part of the network fabric)?
#[allow(clippy::too_many_arguments)]
fn direct_path(
    p: &Project,
    cat: &Catalog,
    reach: &Reach,
    src: &EntityRef,
    sp: &Posture,
    tgt: &EntityRef,
    tp: &Posture,
    port: Option<i64>,
) -> Path {
    let pol = policy(&reach.provider);
    let provider = reach.provider.as_str();
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
    let pe = private_endpoint_for(p, cat, provider, &sp.subnets, &tid);
    let name_of = |id: &str| p.entity(id).map(|x| x.name.to_string()).unwrap_or_default();
    // Over a private endpoint the traffic never leaves the network. An AWS interface
    // endpoint has security groups of its own, which must admit the source on 443.
    let via_pe = |pe: &Id| -> Path {
        let pe_e = p.entity(pe).expect("endpoint on the layer");
        let mut hops: Vec<Id> = sp.subnets.first().cloned().into_iter().collect();
        hops.push(pe.clone());
        let service = endpoint_service(p, cat, &pe_e);
        if provider == "aws" && !is_gateway_service(&service) {
            let groups = linked_groups(p, cat, &pe_e);
            if groups.is_empty() {
                return path(
                    Status::Unknown,
                    hops,
                    format!(
                        "private endpoint \"{}\" has no security group; the VPC default group decides whether it admits \"{}\"",
                        pe_e.name, src.name
                    ),
                    vec![],
                );
            }
            let pe_subnets = subnets_of(p, cat, &pe_e);
            if !egress_allowed(p, &pol, &sp.security_groups, &groups, &pe_subnets, Some(443)) {
                return path(
                    Status::Blocked,
                    hops,
                    format!(
                        "\"{}\"'s security group has no egress rule allowing port 443 to private endpoint \"{}\"",
                        src.name, pe_e.name
                    ),
                    vec![],
                );
            }
            let Some((g, _)) = admitting_rule(p, &groups, &sp.security_groups, &sp.subnets, Some(443)) else {
                return path(
                    Status::Blocked,
                    hops,
                    format!(
                        "private endpoint \"{}\" answers for {service} in this network (private DNS), but {} {} no ingress rule allowing port 443 from \"{}\"",
                        pe_e.name,
                        groups.iter().map(|g| group_label(p, g)).collect::<Vec<_>>().join(", "),
                        if groups.len() == 1 { "has" } else { "have" },
                        src.name
                    ),
                    vec![],
                );
            };
            hops.push(g);
        }
        path(
            Status::Ok,
            hops,
            format!("over private endpoint \"{}\"", pe_e.name),
            vec![],
        )
    };

    if tp.managed {
        // A private endpoint in the source's network bypasses egress altogether.
        if let Some(pe) = pe {
            if !linked {
                return path(
                    Status::Blocked,
                    vec![],
                    format!(
                        "private endpoint \"{}\" exists but there is no link from \"{}\" to \"{}\", so it gets no permission or address",
                        name_of(&pe),
                        src.name,
                        tgt.name
                    ),
                    vec![],
                );
            }
            return via_pe(&pe);
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
        if let Some(note) = nat_note(p, cat, provider, &sp.subnets, &hops, tgt) {
            notes.push(note);
        }
        return path(Status::Ok, hops, "reachable over the provider API".into(), notes);
    }

    // Networked target.
    if tp.subnets.is_empty() {
        if let Some(pe) = pe {
            return via_pe(&pe);
        }
        if tgt.resource_type == "file_system" {
            return path(
                Status::Blocked,
                vec![],
                format!(
                    "\"{}\" has no mount target: link the subnets it should be mounted from",
                    tgt.name
                ),
                vec![],
            );
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
                return via_pe(&pe);
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
    let mut notes = Vec::new();
    // Source egress rule (AWS only): any of the source's groups may allow it.
    if !egress_allowed(
        p,
        &pol,
        &sp.security_groups,
        &tp.security_groups,
        &tp.subnets,
        port,
    ) {
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
    // Target ingress rule: any of the target's groups may admit it, by an open range, a
    // group the source carries, or a range covering the source's subnet.
    let matched = admitting_rule(p, &tp.security_groups, &sp.security_groups, &sp.subnets, port);
    // A cluster's own group can admit traffic but never decides a refusal: the provider
    // and its controllers (the AWS Load Balancer Controller, GKE) add rules to it that
    // the diagram cannot see. Without a Security Group entity the target counts as having
    // no group.
    let real_groups: Vec<&Id> = tp
        .security_groups
        .iter()
        .filter(|g| p.entity(g).is_some_and(|x| x.resource_type == "security_group"))
        .collect();
    let (status, reason) = match (&matched, real_groups.is_empty()) {
        (None, true) => {
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
        (Some((g, r)), _) if !r.source_group.is_empty() => (
            Status::Ok,
            format!(
                "allowed by {}: rule from {}",
                group_label(p, g),
                if r.source_group == *g {
                    "its own members".to_string()
                } else {
                    format!("security group {}", group_label(p, &r.source_group))
                }
            ),
        ),
        (Some((g, r)), _) => (
            Status::Ok,
            format!("allowed by {}: rule from {}", group_label(p, g), r.cidr),
        ),
        (None, false) => (
            Status::Blocked,
            format!(
                "{} {} no ingress rule allowing {} from \"{}\"",
                real_groups
                    .iter()
                    .map(|g| group_label(p, g))
                    .collect::<Vec<_>>()
                    .join(", "),
                if real_groups.len() == 1 { "has" } else { "have" },
                port.map(|x| format!("port {x}")).unwrap_or("this traffic".into()),
                src.name
            ),
        ),
    };
    if status == Status::Ok && !linked {
        if let Some(note) = unlinked_note(cat, src, tgt) {
            notes.push(note);
        }
    }
    // A file system answers through one mount target per linked subnet; a client mounts
    // the one in its own availability zone by the file system's DNS name.
    let mut mount_target: Option<Id> = None;
    if tgt.resource_type == "file_system" {
        let zone = |s: &Id| {
            p.entity(s)
                .and_then(|e| e.provider_field("aws", "availability_zone").map(|v| v.display()))
                .filter(|z| !z.is_empty())
        };
        mount_target = sp
            .subnets
            .first()
            .and_then(|own| {
                tp.subnets
                    .iter()
                    .find(|t| zone(t).is_some() && zone(t) == zone(own))
            })
            .or(tp.subnets.first())
            .cloned();
        if provider == "aws" {
            let fs_zones: Vec<String> = tp.subnets.iter().filter_map(zone).collect();
            let mut missing: Vec<String> = sp
                .subnets
                .iter()
                .filter_map(zone)
                .filter(|z| !fs_zones.contains(z))
                .collect();
            missing.dedup();
            if !fs_zones.is_empty() && !missing.is_empty() && status == Status::Ok {
                notes.push(format!(
                    "\"{}\" has no mount target in {} (it gets one per linked subnet), so clients there cannot mount it by its DNS name",
                    tgt.name,
                    missing.join(", ")
                ));
            }
        }
    }
    let mut hops: Vec<Id> = Vec::new();
    if let Some(s) = sp.subnets.first() {
        hops.push(s.clone());
    }
    hops.extend(fabric);
    if let Some(m) = mount_target {
        if !hops.contains(&m) {
            hops.push(m);
        }
    }
    // The group that let it in, or the first one that could have.
    if let Some(g) = matched
        .map(|(g, _)| g)
        .or_else(|| real_groups.first().map(|g| g.to_string()))
    {
        if g != tid {
            hops.push(g);
        }
    }
    path(status, hops, reason, notes)
}

/// What a missing link costs when the network would let the traffic through: a database
/// hands out no host name or credentials, a file system is mounted by nothing, a cache's
/// address reaches no one.
fn unlinked_note(cat: &Catalog, src: &EntityRef, tgt: &EntityRef) -> Option<String> {
    let what = match tgt.resource_type {
        "relational_database" => "it receives no host name or credentials",
        "file_system" => "nothing mounts it (no volume, no file system id)",
        "cache" => "it receives no address",
        _ => return None,
    };
    let kind = match tgt.resource_type {
        "relational_database" => "a database",
        "file_system" => "a file system",
        _ => "a cache",
    };
    Some(
        match link_label(cat, src.resource_type, tgt.resource_type, None) {
            Some(label) => format!("no '{label}' link from \"{}\", so {what}", src.name),
            None if tgt.resource_type == "relational_database" => format!(
                "\"{}\" cannot link to a database, so nothing hands it a host name or credentials",
                src.name
            ),
            None => format!("\"{}\" cannot link to {kind}, so {what}", src.name),
        },
    )
}

/// Traffic to a managed service that leaves through a NAT gateway although the network
/// has an endpoint for that service: say so, and why the endpoint is not used.
fn nat_note(
    p: &Project,
    cat: &Catalog,
    provider: &str,
    src_subnets: &[Id],
    hops: &[Id],
    tgt: &EntityRef,
) -> Option<String> {
    let nat = hops
        .iter()
        .find(|h| p.entity(h).is_some_and(|e| e.resource_type == "nat_gateway"))?;
    let (pe, why) = unused_endpoint(p, cat, provider, src_subnets, tgt)?;
    let name_of = |id: &str| p.entity(id).map(|e| e.name.to_string()).unwrap_or_default();
    Some(format!(
        "leaves through NAT \"{}\" although private endpoint \"{}\" in the same network serves that service ({why}); the traffic pays NAT processing and crosses the internet edge",
        name_of(nat),
        name_of(&pe)
    ))
}

/// Who can reach `target`: [`INTERNET`] when it is internet-facing or sits behind
/// something that is, every load balancer that forwards to it, and every initiating
/// resource, each with the path it would take. The mirror image of [`paths_from`] for
/// passive resources (databases, caches, queues) and for what load balancers front.
pub fn paths_to(full: &Project, cat: &Catalog, reach: &Reach, target: &str) -> Vec<(Id, Path)> {
    let layer = crate::layers::project_for(full, cat, &reach.provider);
    let p = &layer;
    let mut out = Vec::new();
    if let Some(path) = internet_path(p, cat, reach, target) {
        out.push((INTERNET.to_string(), path));
    }
    for sid in reach.posture.keys() {
        if sid == target {
            continue;
        }
        let Some(src) = p.entity(sid) else { continue };
        let forwards = src.resource_type == "load_balancer"
            && forwarding_targets(p, cat, &src)
                .iter()
                .any(|f| f.target == target);
        if !initiates(src.resource_type) && !forwards {
            continue;
        }
        if let Some(path) = path_between(p, cat, reach, sid, target) {
            out.push((sid.clone(), path));
        }
    }
    out
}

/// CDNs whose origin is `id`.
fn cdns_in_front_of(p: &Project, cat: &Catalog, id: &str) -> Vec<Id> {
    p.entities()
        .into_iter()
        .filter(|e| e.resource_type == "cdn")
        .filter(|c| {
            relation_targets(p, cat, c, Relation::AttributeReference)
                .iter()
                .any(|t| t == id)
        })
        .map(|c| c.id.to_string())
        .collect()
}

/// How the internet reaches `target`, if it does: directly when the target is
/// internet-facing (through a CDN when one fronts it), through a CDN in front of a
/// bucket, or through an internet-facing load balancer that forwards to it.
fn internet_path(p: &Project, cat: &Catalog, reach: &Reach, target: &str) -> Option<Path> {
    let tgt = p.entity(target)?;
    let tp = reach.posture.get(target)?;
    let name_of = |id: &str| p.entity(id).map(|e| e.name.to_string()).unwrap_or_default();
    let ok = |hops: Vec<Id>, reason: String, notes: Vec<String>| Path {
        target: target.to_string(),
        status: Status::Ok,
        hops,
        reason,
        notes,
    };
    if let Some(how) = &tp.exposed {
        let cloudfront_only = how.contains("admits only CloudFront");
        return Some(match cdns_in_front_of(p, cat, target).first() {
            Some(c) => ok(
                vec![c.clone()],
                format!("through CDN \"{}\" to its {how}", name_of(c)),
                if cloudfront_only {
                    vec![]
                } else {
                    vec!["also reachable directly, around the CDN".into()]
                },
            ),
            None if cloudfront_only => ok(
                vec![],
                format!("only through CloudFront: it is an {how}, and no CDN in the diagram fronts it"),
                vec![],
            ),
            None => ok(vec![], format!("through its {how}"), vec![]),
        });
    }
    if tgt.resource_type == "object_storage" {
        if let Some(c) = cdns_in_front_of(p, cat, target).first() {
            return Some(ok(
                vec![c.clone()],
                format!(
                    "through CDN \"{}\", which reads the bucket as its origin",
                    name_of(c)
                ),
                vec![],
            ));
        }
    }
    let mut best: Option<Path> = None;
    for (lb, f) in load_balancers_for(p, cat, target) {
        let (Some(lbe), Some(lp)) = (p.entity(&lb), reach.posture.get(&lb)) else {
            continue;
        };
        let Some(how) = &lp.exposed else { continue };
        let leg = forward_leg(p, cat, reach, &lbe, lp, &tgt, tp, &f);
        let cdn = cdns_in_front_of(p, cat, &lb).into_iter().next();
        let mut hops: Vec<Id> = cdn.iter().cloned().collect();
        hops.push(lb.clone());
        hops.extend(leg.hops.iter().cloned());
        let mut notes = leg.notes;
        if cdn.is_none() && how.contains("admits only CloudFront") {
            notes.insert(
                0,
                format!(
                    "\"{}\" admits only CloudFront, and no CDN in the diagram fronts it",
                    lbe.name
                ),
            );
        }
        let path = Path {
            target: target.to_string(),
            status: leg.status,
            hops,
            reason: format!(
                "through{} internet-facing load balancer \"{}\": {}",
                cdn.as_ref()
                    .map(|c| format!(" CDN \"{}\" and", name_of(c)))
                    .unwrap_or_default(),
                lbe.name,
                leg.reason
            ),
            notes,
        };
        if path.status == Status::Ok {
            return Some(path);
        }
        best.get_or_insert(path);
    }
    best
}

// ---------------------------------------------------------------------------------
// Intent and the audit: what the diagram says should talk, against what the network
// lets through.

/// Why the diagram says one resource talks to another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// A link from the source, by its label as it reads in a sentence ("Uses", "Mounts").
    Link(String),
    /// The source is a load balancer that forwards to the target.
    Forwards,
    /// A flow drawn between them in a view.
    Flow { view: String, label: String },
    /// The source reads the secret the target (a database or cache) takes its password
    /// from.
    Secret(Id),
}

impl Intent {
    /// A link or a forwarding rule: something the export turns into an address and a
    /// permission. Flows and shared secrets are only evidence of intent.
    pub fn is_link(&self) -> bool {
        matches!(self, Intent::Link(_) | Intent::Forwards)
    }

    pub fn describe(&self, p: &Project) -> String {
        match self {
            Intent::Link(l) => format!("link '{l}'"),
            Intent::Forwards => "forwards to it".into(),
            Intent::Flow { view, label } if label.is_empty() => format!("flow in view \"{view}\""),
            Intent::Flow { view, label } => format!("flow \"{label}\" in view \"{view}\""),
            Intent::Secret(s) => format!(
                "reads \"{}\", its password",
                p.entity(s).map(|e| e.name.to_string()).unwrap_or_default()
            ),
        }
    }
}

/// Intended pairs, `(source, target)`, each with every reason the diagram gives.
pub type Intents = BTreeMap<(Id, Id), Vec<Intent>>;

/// Does this entity take part in reachability at all?
fn in_scope(e: &EntityRef) -> bool {
    !e.is_container && (is_managed(e.resource_type) || is_networked(e.resource_type))
}

/// Every pair of resources the diagram says talk to each other, on one provider's layer.
pub fn intents(full: &Project, cat: &Catalog, provider: &str) -> Intents {
    let layer = crate::layers::project_for(full, cat, provider);
    intents_in(&layer, cat, &full.views)
}

/// The pairs, source first:
/// - a link from a resource that opens connections to one that takes part in
///   reachability, except where it runs (a workload's cluster), where it lives, and
///   logs a provider delivers itself (a cluster's control plane, a function);
/// - a load balancer to what it forwards to;
/// - a flow drawn between two such resources in a view, from the end that opens the
///   connection (a flow from a queue to its consumer is the consumer polling it);
/// - a resource that reads the secret a database or cache takes its password from, to
///   that database or cache.
fn intents_in(p: &Project, cat: &Catalog, views: &[ttg_core::View]) -> Intents {
    let mut out: Intents = BTreeMap::new();
    let mut add = |s: &str, t: &str, why: Intent| {
        if s == t {
            return;
        }
        let list = out.entry((s.to_string(), t.to_string())).or_default();
        if !list.contains(&why) {
            list.push(why);
        }
    };
    let ents = p.entities();
    let scoped = |id: &str| p.entity(id).is_some_and(|e| in_scope(&e));
    for s in ents.iter().filter(|e| in_scope(e) && initiates(e.resource_type)) {
        for edge in p.edges_from(s.id) {
            let Some(t) = p.entity(&edge.target) else { continue };
            if !in_scope(&t) || matches!(edge.relation, Relation::DependsOn | Relation::NetworkMembership) {
                continue;
            }
            let hosting = edge.relation == Relation::Attachment
                && (t.resource_type == "kubernetes_cluster" || t.resource_type == "load_balancer");
            let provider_logs = edge.relation == Relation::LogsTo
                && matches!(s.resource_type, "kubernetes_cluster" | "function");
            if hosting || provider_logs {
                continue;
            }
            let label = link_label(cat, s.resource_type, t.resource_type, Some(edge.relation))
                .unwrap_or_else(|| edge.relation.display_name().to_string());
            add(s.id, t.id, Intent::Link(label));
        }
        // A database's password secret, read by the source.
        for secret in p
            .edges_from(s.id)
            .filter(|x| x.relation == Relation::Reads)
            .filter(|x| p.entity(&x.target).is_some_and(|e| e.resource_type == "secret"))
        {
            // The store reads its password from the secret, or the secret holds the
            // store's endpoint.
            let owners: Vec<Id> = p
                .edges_to(&secret.target)
                .map(|x| x.source.clone())
                .chain(p.edges_from(&secret.target).map(|x| x.target.clone()))
                .filter(|o| {
                    p.entity(o)
                        .is_some_and(|e| matches!(e.resource_type, "relational_database" | "cache"))
                })
                .collect();
            for owner in owners {
                add(s.id, &owner, Intent::Secret(secret.target.clone()));
            }
        }
    }
    for lb in ents
        .iter()
        .filter(|e| e.resource_type == "load_balancer" && in_scope(e))
    {
        for f in forwarding_targets(p, cat, lb).into_iter().filter(|f| !f.assumed) {
            if scoped(&f.target) {
                add(lb.id, &f.target, Intent::Forwards);
            }
        }
    }
    let opens = |id: &str| {
        p.entity(id)
            .is_some_and(|e| initiates(e.resource_type) || e.resource_type == "load_balancer")
    };
    for v in views {
        for f in &v.flows {
            let (ttg_core::FlowEnd::Entity { entity: a }, ttg_core::FlowEnd::Entity { entity: b }) =
                (&f.from, &f.to)
            else {
                continue;
            };
            if !scoped(a) || !scoped(b) {
                continue;
            }
            let why = Intent::Flow {
                view: v.name.clone(),
                label: f.label.clone(),
            };
            if opens(a) {
                add(a, b, why);
            } else if opens(b) {
                add(b, a, why);
            }
        }
    }
    out
}

/// Data worth guarding: classified personal, payment or confidential, or, with no
/// classification, a database, a secret or a bucket. Public and internal opt out.
pub fn is_sensitive(e: &EntityRef) -> bool {
    use ttg_core::Classification as C;
    match e.classification {
        Some(c) => matches!(c, C::Personal | C::Payment | C::Confidential),
        None => matches!(
            e.resource_type,
            "relational_database" | "secret" | "object_storage"
        ),
    }
}

/// One finding of [`audit`].
#[derive(Debug, Clone)]
pub struct Finding {
    pub severity: Severity,
    /// What kind of finding, stable for filtering:
    /// - `blocked` (error): intended, and the network or a missing link stops it;
    /// - `unintended` (warning): sensitive data reachable from resources with no intent;
    /// - `undecided` (info): intended, and the diagram cannot decide;
    /// - `unlinked` (info): intended by a flow or a secret only, and the network allows
    ///   it, but without a link the export hands over no address or credentials;
    /// - `nat_egress` (info): intended, and leaving through NAT for a service the
    ///   network has an endpoint for.
    pub kind: &'static str,
    /// Where the traffic starts: an entity id, or [`INTERNET`]. For `unintended`, the
    /// first of `sources`.
    pub source: Id,
    pub target: Id,
    /// Every source of a finding that groups several (`unintended`); else `[source]`.
    pub sources: Vec<Id>,
    pub message: String,
    /// Why the diagram says they talk (empty for `unintended`).
    pub intent: Vec<Intent>,
    pub path: Option<Path>,
}

/// Compare intent with the network: every intended pair the network blocks (error) or
/// cannot decide (info), intended pairs that work only on the network (info), traffic
/// through NAT for a service with an endpoint (info), and sensitive data that resources
/// with no intent can reach (warning, one per target). Errors first.
pub fn audit(full: &Project, cat: &Catalog, reach: &Reach) -> Vec<Finding> {
    let layer = crate::layers::project_for(full, cat, &reach.provider);
    let p = &layer;
    let intents = intents_in(p, cat, &full.views);
    let name = |id: &str| source_name(p, id);
    let mut out: Vec<Finding> = Vec::new();
    for ((s, t), why) in &intents {
        let Some(path) = path_between(p, cat, reach, s, t) else {
            continue;
        };
        let Some(tgt) = p.entity(t) else { continue };
        let because = why.iter().map(|w| w.describe(p)).collect::<Vec<_>>().join("; ");
        let finding = |severity, kind, message: String| Finding {
            severity,
            kind,
            source: s.clone(),
            target: t.clone(),
            sources: vec![s.clone()],
            message,
            intent: why.clone(),
            path: Some(path.clone()),
        };
        match path.status {
            Status::Blocked => out.push(finding(
                Severity::Error,
                "blocked",
                format!(
                    "\"{}\" → \"{}\" is intended ({because}) but blocked: {}",
                    name(s),
                    name(t),
                    path.reason
                ),
            )),
            Status::Unknown => out.push(finding(
                Severity::Info,
                "undecided",
                format!(
                    "\"{}\" → \"{}\" is intended ({because}) but cannot be decided: {}",
                    name(s),
                    name(t),
                    path.reason
                ),
            )),
            Status::Ok => {
                if !why.iter().any(Intent::is_link) {
                    if let Some(note) = unlinked_note(cat, &p.entity(s).unwrap(), &tgt) {
                        out.push(finding(
                            Severity::Info,
                            "unlinked",
                            format!(
                                "\"{}\" → \"{}\" is intended ({because}) and the network allows it, but there is {note}",
                                name(s),
                                name(t)
                            ),
                        ));
                    }
                }
                if let Some(note) = path.notes.iter().find(|n| n.starts_with("leaves through NAT")) {
                    out.push(finding(
                        Severity::Info,
                        "nat_egress",
                        format!("\"{}\" → \"{}\" {note}", name(s), name(t)),
                    ));
                }
            }
        }
    }
    // Sensitive data that something with no intent can reach. Managed services need a
    // link to be reached at all, so only networked targets (and anything internet-facing)
    // can be open by accident.
    for (t, tp) in &reach.posture {
        let Some(tgt) = p.entity(t) else { continue };
        if !is_sensitive(&tgt) {
            continue;
        }
        let mut sources: Vec<(Id, Path)> = Vec::new();
        if let Some(path) = internet_path(p, cat, reach, t).filter(|x| x.status == Status::Ok) {
            sources.push((INTERNET.to_string(), path));
        }
        if !tp.managed {
            for s in reach.posture.keys() {
                if s == t || intents.contains_key(&(s.clone(), t.clone())) {
                    continue;
                }
                if !p.entity(s).is_some_and(|e| initiates(e.resource_type)) {
                    continue;
                }
                if let Some(path) = path_between(p, cat, reach, s, t).filter(|x| x.status == Status::Ok) {
                    sources.push((s.clone(), path));
                }
            }
        }
        if sources.is_empty() {
            continue;
        }
        sources.sort_by_key(|(s, _)| (s != INTERNET, name(s)));
        let what = match tgt.classification {
            Some(c) => format!("classified {}", c.key()),
            None => format!(
                "an unclassified {}",
                match tgt.resource_type {
                    "relational_database" => "database",
                    "secret" => "secret",
                    _ => "bucket",
                }
            ),
        };
        let names: Vec<String> = sources.iter().map(|(s, _)| name(s)).collect();
        let port = listening_port(&tgt)
            .map(|x| format!(" on port {x}"))
            .unwrap_or_default();
        let (first, first_path) = sources[0].clone();
        out.push(Finding {
            severity: Severity::Warning,
            kind: "unintended",
            source: first,
            target: t.clone(),
            sources: sources.iter().map(|(s, _)| s.clone()).collect(),
            message: format!(
                "\"{}\" ({what}) can be reached{port} by {} with no link or flow to it: {}. Narrow what admits it to the resources that use it, or link them",
                name(t),
                if names.len() == 1 {
                    "one resource".to_string()
                } else {
                    format!("{} resources", names.len())
                },
                quoted(&names, 8)
            ),
            intent: Vec::new(),
            path: Some(first_path),
        });
    }
    out.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| name(&a.source).cmp(&name(&b.source)))
            .then_with(|| name(&a.target).cmp(&name(&b.target)))
            .then_with(|| a.kind.cmp(b.kind))
    });
    out
}

/// For a project with named environments: the audit of every environment other than
/// `current` (`None`: the base values) that `current`'s own audit does not already
/// report, each with its environment and its message prefixed `[env] `, the way
/// `environments::other_environments` reports the other environments' diagnostics. An
/// override can open or close a path (a group linked in one environment only), so each
/// environment is analysed on its own resolved values.
pub fn audit_other_environments(
    p: &Project,
    cat: &Catalog,
    provider: &str,
    current: Option<&str>,
) -> Vec<(String, Finding)> {
    if p.settings.environments.is_empty() {
        return Vec::new();
    }
    let key = |f: &Finding| (f.kind, f.source.clone(), f.target.clone(), f.message.clone());
    let shown = p.resolved(current);
    let mine: Vec<_> = audit(&shown, cat, &analyse(&shown, cat, provider))
        .iter()
        .map(key)
        .collect();
    let mut out = Vec::new();
    for env in &p.settings.environments {
        if Some(env.as_str()) == current {
            continue;
        }
        let ep = p.for_environment(env);
        for f in audit(&ep, cat, &analyse(&ep, cat, provider)) {
            if !mine.contains(&key(&f)) {
                let message = format!("[{env}] {}", f.message);
                out.push((env.clone(), Finding { message, ..f }));
            }
        }
    }
    out
}

/// `"a", "b" and "c"`; past `max` names, `… and N more`.
fn quoted(names: &[String], max: usize) -> String {
    let mut shown: Vec<String> = names.iter().take(max).map(|n| format!("\"{n}\"")).collect();
    if names.len() > max {
        shown.push(format!("{} more", names.len() - max));
    }
    match shown.len() {
        0 => String::new(),
        1 => shown.remove(0),
        _ => {
            let last = shown.pop().unwrap();
            format!("{} and {last}", shown.join(", "))
        }
    }
}
