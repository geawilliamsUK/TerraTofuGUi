//! What the canvas shows that the project file does not store, derived from the IR and
//! the catalog so the GUI, the agent tools and the tests agree on it: availability-zone
//! labels, the tier layout's placement, security-group rules as edges, which links are
//! monitoring, and the region the drawing is in. Nothing here affects the export.

use crate::diagnostics::relation_targets;
use std::collections::{BTreeMap, BTreeSet};
use ttg_catalog::{Catalog, Tier};
use ttg_core::layout::{Place, TidyOptions};
use ttg_core::{EntityRef, Id, Project, Relation, Size, Value};

// ------------------------------------------------------------------ availability zones

/// The availability zone an entity is pinned to (a subnet's `availability_zone`), from
/// the target provider's fields first, then any provider's.
pub fn availability_zone(p: &Project, id: &str) -> Option<String> {
    let e = p.entity(id)?;
    let target = p.settings.target_provider.as_str();
    let pick = |provider: &str| {
        ["availability_zone", "zone"].iter().find_map(|k| {
            e.provider_field(provider, k)
                .or_else(|| e.field(k))
                .map(Value::display)
                .filter(|s| !s.trim().is_empty())
        })
    };
    pick(target).or_else(|| e.provider_config.keys().find_map(|pv| pick(pv)))
}

/// The short label of a zone: `eu-west-2a` → `a`, `europe-west2-b` → `b`, `1` → `1`.
pub fn zone_label(zone: &str) -> String {
    let z = zone.trim();
    let tail = z.rsplit('-').next().unwrap_or(z);
    let letters = tail.trim_start_matches(|c: char| c.is_ascii_digit());
    if letters.is_empty() {
        tail.to_string()
    } else {
        letters.to_string()
    }
}

/// The subnets an entity is in: its membership targets of type subnet (drawn inside one
/// counts, see `diagnostics::relation_targets`).
pub fn subnets_of(p: &Project, cat: &Catalog, e: &EntityRef) -> Vec<Id> {
    relation_targets(p, cat, e, Relation::NetworkMembership)
        .into_iter()
        .filter(|t| p.entity(t).is_some_and(|x| x.resource_type == "subnet"))
        .collect()
}

/// The zone labels of the subnets an entity is in, sorted, each once.
pub fn zones_of(p: &Project, cat: &Catalog, id: &str) -> Vec<String> {
    let Some(e) = p.entity(id) else { return Vec::new() };
    let mut out: BTreeSet<String> = BTreeSet::new();
    for s in subnets_of(p, cat, &e) {
        if let Some(z) = availability_zone(p, &s) {
            out.insert(zone_label(&z));
        }
    }
    out.into_iter().collect()
}

/// The zone badge a node carries — `"a b"` for a load balancer on two public subnets —
/// when where it is drawn does not already say it: it spans several subnets, or is in
/// one it is not drawn inside. `None` without zones.
pub fn zone_badge(p: &Project, cat: &Catalog, id: &str) -> Option<String> {
    let e = p.entity(id)?;
    if e.is_container || e.resource_type == "subnet" {
        return None;
    }
    let subnets = subnets_of(p, cat, &e);
    if subnets.is_empty() || (subnets.len() == 1 && e.parent == Some(subnets[0].as_str())) {
        return None;
    }
    let zones = zones_of(p, cat, id);
    (!zones.is_empty()).then(|| zones.join(" "))
}

// ------------------------------------------------------------------ tier layout

/// How a subnet reaches the internet, from the route tables applied to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubnetRoute {
    /// A default route via an internet gateway.
    Public,
    /// A default route via a NAT gateway: out, but not in.
    Private,
    /// A route table with neither.
    Isolated,
}

/// The route of a subnet: public when a route table applied to it has a default route
/// via an internet gateway, else private when one has it via a NAT gateway, else isolated.
/// `None` for a subnet no route table is applied to.
pub fn subnet_route(p: &Project, id: &str) -> Option<SubnetRoute> {
    let tables: Vec<&str> = p
        .edges_to(id)
        .filter(|e| e.relation == Relation::Attachment)
        .map(|e| e.source.as_str())
        .filter(|t| p.nodes.get(*t).is_some_and(|n| n.resource_type == "route_table"))
        .collect();
    let via = |kind: &str| {
        tables.iter().any(|t| {
            p.edges_from(t).any(|e| {
                e.relation == Relation::AttributeReference
                    && p.nodes.get(&e.target).is_some_and(|n| n.resource_type == kind)
            })
        })
    };
    if tables.is_empty() {
        None
    } else if via("internet_gateway") {
        Some(SubnetRoute::Public)
    } else if via("nat_gateway") {
        Some(SubnetRoute::Private)
    } else {
        Some(SubnetRoute::Isolated)
    }
}

/// The tier of every entity, deterministic:
///
/// 1. `overrides` (from the agent's `layout_tidy` call) win;
/// 2. a subnet with a route table is public when the table has a default route via an
///    internet gateway (and what is in it goes with it); otherwise it takes the app or
///    data tier most of what is in it has (drawn inside or linked), else app for a route
///    via a NAT gateway and data for none. Without a route table it takes the tier most
///    of what is in it has, else public when it gives out public addresses or is named
///    so, data when named so, else app;
/// 3. a security group takes the tier most of the resources carrying it have, and a
///    route table that of the subnets it is attached to;
/// 4. everything else its type's tier (the definition's `tier`, else its category's);
/// 5. a container (or a subnet holding resources) the tier most of what it holds has,
///    nodes counted, its type's when it holds nothing.
///
/// Ties go to the earlier band.
pub fn tiers(p: &Project, cat: &Catalog, overrides: &BTreeMap<Id, Tier>) -> BTreeMap<Id, Tier> {
    let mut out: BTreeMap<Id, Tier> = BTreeMap::new();
    let base = |id: &str| -> Tier {
        overrides
            .get(id)
            .copied()
            .unwrap_or_else(|| cat.tier(p.entity(id).map(|e| e.resource_type).unwrap_or("")))
    };
    // Plain nodes first: their type's tier.
    for n in p.nodes.values() {
        out.insert(n.id.clone(), base(&n.id));
    }
    let majority = |ts: &[Tier]| -> Option<Tier> {
        let mut count: BTreeMap<Tier, usize> = BTreeMap::new();
        for t in ts {
            *count.entry(*t).or_default() += 1;
        }
        let best = count.values().copied().max()?;
        count.into_iter().find(|(_, c)| *c == best).map(|(t, _)| t)
    };
    // What is in a public subnet (one routed to an internet gateway) goes with it.
    let is_public = |s: &Id| subnet_route(p, s) == Some(SubnetRoute::Public);
    for n in p.nodes.values() {
        if n.resource_type == "subnet" || overrides.contains_key(&n.id) {
            continue;
        }
        let subnets = subnets_of(p, cat, &p.entity(&n.id).unwrap());
        if subnets.iter().filter(|s| is_public(s)).count() * 2 > subnets.len() {
            out.insert(n.id.clone(), Tier::Public);
        }
    }
    // Subnets: from their route table, then from what is in them.
    let mut members: BTreeMap<Id, Vec<Tier>> = BTreeMap::new();
    for n in p.nodes.values() {
        if n.resource_type == "subnet" {
            continue;
        }
        let e = p.entity(&n.id).unwrap();
        for s in subnets_of(p, cat, &e) {
            members.entry(s).or_default().push(out[&n.id]);
        }
    }
    for n in p.nodes.values().filter(|n| n.resource_type == "subnet") {
        if overrides.contains_key(&n.id) {
            continue;
        }
        let held = members.get(&n.id).map(Vec::as_slice).unwrap_or(&[]);
        let t = match subnet_route(p, &n.id) {
            Some(SubnetRoute::Public) => Tier::Public,
            // A route table shared by app and data subnets says little: what is in the
            // subnet decides, and its route only when it is empty.
            Some(route) => {
                let inner: Vec<Tier> = held
                    .iter()
                    .copied()
                    .filter(|t| matches!(t, Tier::App | Tier::Data))
                    .collect();
                majority(&inner).unwrap_or(if route == SubnetRoute::Private {
                    Tier::App
                } else {
                    Tier::Data
                })
            }
            None => majority(held).unwrap_or_else(|| {
                let name = n.name.to_lowercase();
                let public = n
                    .provider_config
                    .values()
                    .any(|c| c.get("map_public_ip").and_then(Value::as_bool) == Some(true));
                if public || name.contains("public") || name.starts_with("pub") || name.contains("dmz") {
                    Tier::Public
                } else if ["data", "db", "database", "storage"]
                    .iter()
                    .any(|w| name.contains(w))
                {
                    Tier::Data
                } else {
                    Tier::App
                }
            }),
        };
        out.insert(n.id.clone(), t);
    }
    // Security groups and route tables: from what they serve.
    let snapshot = out.clone();
    for n in p.nodes.values() {
        if overrides.contains_key(&n.id) {
            continue;
        }
        let serves: Vec<Tier> = match n.resource_type.as_str() {
            "security_group" => p
                .edges_to(&n.id)
                .filter(|e| matches!(e.relation, Relation::AttributeReference | Relation::Attachment))
                .filter_map(|e| snapshot.get(&e.source).copied())
                .collect(),
            "route_table" => p
                .edges_from(&n.id)
                .filter(|e| e.relation == Relation::Attachment)
                .filter(|e| {
                    p.nodes
                        .get(&e.target)
                        .is_some_and(|t| t.resource_type == "subnet")
                })
                .filter_map(|e| snapshot.get(&e.target).copied())
                .collect(),
            _ => continue,
        };
        if let Some(t) = majority(&serves) {
            out.insert(n.id.clone(), t);
        }
    }
    // Containers: from what they hold (deepest first, so nested ones are known).
    let mut cs: Vec<&ttg_core::Container> = p.containers.values().collect();
    cs.sort_by_key(|c| (std::cmp::Reverse(p.depth_of(&c.id)), c.id.clone()));
    for c in cs {
        let held: Vec<Tier> = p
            .descendants_of(&c.id)
            .iter()
            .filter(|d| p.nodes.contains_key(*d))
            .filter_map(|d| out.get(d).copied())
            .collect();
        let t = overrides
            .get(&c.id)
            .copied()
            .or_else(|| majority(&held))
            .unwrap_or_else(|| cat.tier(&c.container_type));
        out.insert(c.id.clone(), t);
    }
    out
}

/// The row an entity takes in the tier grid: the zone it is pinned to, or `""` when it
/// is in no zone or several. A subnet's own zone; a route table's from the subnets it is
/// attached to; anything else's from the subnets it is in.
pub fn tier_row(p: &Project, cat: &Catalog, id: &str) -> String {
    let Some(e) = p.entity(id) else {
        return String::new();
    };
    if e.is_container {
        return String::new();
    }
    let zones: BTreeSet<String> = if e.resource_type == "subnet" {
        availability_zone(p, id)
            .map(|z| zone_label(&z))
            .into_iter()
            .collect()
    } else if e.resource_type == "route_table" {
        p.edges_from(id)
            .filter(|x| x.relation == Relation::Attachment)
            .filter_map(|x| availability_zone(p, &x.target).filter(|_| p.nodes.contains_key(&x.target)))
            .map(|z| zone_label(&z))
            .collect()
    } else {
        zones_of(p, cat, id).into_iter().collect()
    };
    if zones.len() == 1 {
        zones.into_iter().next().unwrap()
    } else {
        String::new()
    }
}

/// Row of what is in no zone or several, after the zones' own.
const SHARED_ROW: &str = "~";
/// Row of the security groups, last: the firewall under everything it protects.
const FIREWALL_ROW: &str = "~~";

/// Lay the project out by tiers (see [`tiers`] and `ttg_core::layout::tidy_tiers`):
/// bands left to right, or top to bottom with `vertical`, one row per availability zone
/// — the subnets on top, then what is shared by several zones, then the security groups.
pub fn tidy_by_tiers(
    p: &mut Project,
    cat: &Catalog,
    overrides: &BTreeMap<Id, Tier>,
    node_size: &dyn Fn(&Project, &str) -> Size,
    opts: &TidyOptions,
    vertical: bool,
) {
    tidy_shown_by_tiers(p, cat, overrides, &|_| true, node_size, opts, vertical);
}

/// [`tidy_by_tiers`] for what `shown` lets through: the rest stays where it is and takes
/// no room, so a view that hides most of the diagram is laid out as if it were all there
/// is.
pub fn tidy_shown_by_tiers(
    p: &mut Project,
    cat: &Catalog,
    overrides: &BTreeMap<Id, Tier>,
    shown: &dyn Fn(&str) -> bool,
    node_size: &dyn Fn(&Project, &str) -> Size,
    opts: &TidyOptions,
    vertical: bool,
) {
    let tier = tiers(p, cat, overrides);
    let rows: BTreeMap<Id, String> = p
        .entities()
        .iter()
        .map(|e| {
            let row = match tier_row(p, cat, e.id) {
                zone if !zone.is_empty() => zone,
                _ if e.resource_type == "security_group" => FIREWALL_ROW.into(),
                _ => SHARED_ROW.into(),
            };
            (e.id.to_string(), row)
        })
        .collect();
    let place = |q: &Project, id: &str| Place {
        band: tier.get(id).copied().unwrap_or(Tier::App).index(),
        row: rows.get(id).cloned().unwrap_or_default(),
        first: q.entity(id).is_some_and(|e| e.resource_type == "subnet"),
        hidden: !shown(id)
            || (q.containers.contains_key(id) && !ttg_core::view::has_visible_members(q, id, shown)),
    };
    ttg_core::layout::tidy_tiers(p, &place, node_size, opts, vertical);
}

// ------------------------------------------------------------------ security group rules

/// The far end of a security-group rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Peer {
    /// Something on the canvas: another security group (or a Kubernetes cluster, for its
    /// nodes), or a prefix list drawn there.
    Entity(Id),
    /// Addresses only: `internet` for 0.0.0.0/0 (or ::/0), the CIDR otherwise, or the
    /// short name of an AWS-managed prefix list (`CloudFront`).
    Outside(String),
}

/// One row of a security group's `rules`, as the canvas draws it: an edge between the
/// two groups for a group-to-group rule, a badge on the group otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleEdge {
    /// The security group whose row it is.
    pub group: Id,
    /// Index of the row in its `rules`.
    pub row: usize,
    pub name: String,
    pub ingress: bool,
    pub peer: Peer,
    /// `tcp 5432`, `tcp 8000-8080`, `udp 53`, `icmp`, `all`.
    pub label: String,
}

impl RuleEdge {
    /// Where traffic starts: the peer of an ingress rule, the group of an egress rule.
    pub fn from(&self) -> Option<&str> {
        match (&self.peer, self.ingress) {
            (Peer::Entity(e), true) => Some(e),
            (_, false) => Some(&self.group),
            (Peer::Outside(_), true) => None,
        }
    }
    /// Where traffic goes: the group of an ingress rule, the peer of an egress rule.
    pub fn to(&self) -> Option<&str> {
        match (&self.peer, self.ingress) {
            (_, true) => Some(&self.group),
            (Peer::Entity(e), false) => Some(e),
            (Peer::Outside(_), false) => None,
        }
    }
}

/// The short name of an AWS-managed prefix list: `CloudFront` for CloudFront's
/// origin-facing list, else its last dotted part.
pub fn prefix_list_label(name: &str) -> String {
    if name.contains("cloudfront") {
        return "CloudFront".into();
    }
    name.rsplit('.').next().unwrap_or(name).to_string()
}

/// Every rule row of every security group, in group id and row order. A row whose
/// source (or, egress, destination) is a group or a prefix list on the canvas is an
/// edge; one with only addresses or an AWS-managed prefix list is an `Outside` peer.
pub fn rule_edges(p: &Project) -> Vec<RuleEdge> {
    let mut out = Vec::new();
    for n in p.nodes.values().filter(|n| n.resource_type == "security_group") {
        let Some(Value::Records(rows)) = n.config.get("rules") else {
            continue;
        };
        for (i, r) in rows.iter().enumerate() {
            let get = |k: &str| r.get(k).map(Value::display).filter(|s| !s.trim().is_empty());
            let entity = |k: &str| get(k).filter(|id| p.contains(id));
            let peer = if let Some(g) = entity("source_group") {
                Peer::Entity(g)
            } else if let Some(pl) = entity("source_prefix_list") {
                Peer::Entity(pl)
            } else if let Some(pl) = get("prefix_list") {
                Peer::Outside(prefix_list_label(&pl))
            } else if let Some(c) = get("cidr") {
                Peer::Outside(if c == "0.0.0.0/0" || c == "::/0" {
                    "internet".into()
                } else {
                    c
                })
            } else {
                continue;
            };
            let protocol = get("protocol").unwrap_or("tcp".into());
            let (from, to) = (
                r.get("from_port").and_then(Value::as_int),
                r.get("to_port").and_then(Value::as_int),
            );
            let label = match (protocol.as_str(), from, to) {
                ("all", _, _) | ("-1", _, _) => "all".to_string(),
                ("icmp", _, _) => "icmp".to_string(),
                (pr, Some(a), Some(b)) if a == b || b == 0 => format!("{pr} {a}"),
                (pr, Some(0), Some(65535)) => format!("{pr} all ports"),
                (pr, Some(a), Some(b)) => format!("{pr} {a}-{b}"),
                (pr, Some(a), None) => format!("{pr} {a}"),
                (pr, None, _) => pr.to_string(),
            };
            out.push(RuleEdge {
                group: n.id.clone(),
                row: i,
                name: get("name").unwrap_or_default(),
                ingress: get("direction").as_deref() != Some("egress"),
                peer,
                label,
            });
        }
    }
    out
}

/// The resources carrying a security group: whatever links to it with the relation
/// its type uses for one ("Uses security group", a private endpoint's attachment).
pub fn group_members(p: &Project, group: &str) -> Vec<Id> {
    let mut out: Vec<Id> = p
        .edges_to(group)
        .filter(|e| matches!(e.relation, Relation::AttributeReference | Relation::Attachment))
        .filter(|e| {
            p.entity(&e.source)
                .is_some_and(|s| s.resource_type != "security_group")
        })
        .map(|e| e.source.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

// ------------------------------------------------------------------ monitoring links

/// Is this a monitoring resource (alarm, log group, audit trail, budget)?
pub fn is_monitoring(p: &Project, cat: &Catalog, id: &str) -> bool {
    p.entity(id)
        .and_then(|e| cat.resource(e.resource_type))
        .is_some_and(|d| d.resource.category == "monitoring")
}

/// A link from or to a monitoring resource: what an alarm watches and notifies, what
/// logs to a log group, what an audit trail writes to. The view filter's
/// `hide_monitoring` leaves these out.
pub fn is_monitoring_link(p: &Project, cat: &Catalog, source: &str, target: &str) -> bool {
    is_monitoring(p, cat, source) || is_monitoring(p, cat, target)
}

/// Name of the saved view `view_generate { kind: "network" }` builds.
pub const NETWORK_VIEW: &str = "Network";
/// Its description.
pub const NETWORK_VIEW_DESCRIPTION: &str = "The network and what runs in it — gateways, subnets, route tables, security groups, load balancers, compute, containers and databases — laid out by tiers, without monitoring links.";

/// The filter of the "Network" view: the categories that live in a network, monitoring
/// links hidden.
pub fn network_view_filter() -> ttg_core::ViewFilter {
    ttg_core::ViewFilter {
        categories: ["network", "load_balancer", "compute", "container", "database"]
            .into_iter()
            .map(String::from)
            .collect(),
        hide_monitoring: true,
        ..Default::default()
    }
}

// ------------------------------------------------------------------ region

/// The region (or location) the provider's resources are created in: the project's
/// value of its `region` / `location` variable, else the variable's default.
pub fn project_region(p: &Project, cat: &Catalog, provider: &str) -> Option<String> {
    let pdef = cat.provider(provider)?;
    let var = pdef
        .variables
        .iter()
        .find(|v| v.name == "region" || v.name == "location")?;
    p.settings
        .provider_settings
        .get(provider)
        .and_then(|m| m.get(&var.name))
        .filter(|s| !s.trim().is_empty())
        .cloned()
        .or_else(|| var.default.as_ref()?.as_str().map(str::to_string))
}

/// The region of each provider alias (`us_east_1` → `us-east-1`), from its `region`
/// argument when that is a literal.
fn alias_region_names(cat: &Catalog, provider: &str) -> BTreeMap<String, String> {
    let Some(pdef) = cat.provider(provider) else {
        return BTreeMap::new();
    };
    pdef.aliases
        .iter()
        .filter_map(
            |a| match a.args.get("region").or_else(|| a.args.get("location")) {
                Some(ttg_catalog::ArgSource::Literal(l)) => {
                    l.value.as_str().map(|r| (a.name.clone(), r.to_string()))
                }
                _ => None,
            },
        )
        .collect()
}

/// Entities the export creates (some of) through an aliased provider configuration in
/// another region — CloudFront's certificate copy and Web ACL in us-east-1 — with that
/// region. Read from the export's plan, so it is exactly what would be generated.
pub fn alias_regions(p: &Project, cat: &Catalog, provider: &str) -> BTreeMap<Id, String> {
    let names = alias_region_names(cat, provider);
    if names.is_empty() {
        return BTreeMap::new();
    }
    let home = project_region(p, cat, provider);
    let layer = crate::layers::project_for(p, cat, provider);
    let plan = crate::plan::Plan::build(&layer, cat, provider, None);
    let mut out: BTreeMap<Id, String> = BTreeMap::new();
    let mut keys: Vec<&(Id, String)> = plan.blocks.keys().collect();
    keys.sort();
    for (id, key) in keys {
        let Some(e) = layer.entity(id) else { continue };
        let Some(m) = cat.mapping(e.resource_type, provider) else {
            continue;
        };
        let Some(b) = m.blocks.iter().find(|b| &b.key == key) else {
            continue;
        };
        if let Some(region) = b.provider_alias.as_ref().and_then(|a| names.get(a)) {
            if home.as_deref() != Some(region.as_str()) {
                out.entry(id.clone()).or_insert_with(|| region.clone());
            }
        }
    }
    out
}
