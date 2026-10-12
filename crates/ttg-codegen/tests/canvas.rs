//! What the canvas draws must not change what is generated: a resource drawn inside a
//! subnet is the same resource as one linked to it, for the export, the diagnostics and
//! reachability, on every provider.

use std::collections::BTreeMap;
use std::path::Path;
use ttg_catalog::Catalog;
use ttg_catalog::Tier;
use ttg_codegen::canvas::SubnetRoute;
use ttg_codegen::{canvas, diagnostics, generate, reach, views};
use ttg_core::{Project, Relation, Tool};

const PROVIDERS: [&str; 3] = ["aws", "azure", "gcp"];

fn example(name: &str) -> Project {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name);
    ttg_core::project::load(&p).expect("example loads")
}

/// Every node with a membership link to a subnet it may be drawn inside, and drawn
/// beside that subnet (same parent), is moved into the first such subnet and that link
/// removed. (A node inside something else — an app in its Container Environment —
/// would leave it, which changes what it is; it keeps its links.) Returns the moved ids.
fn draw_inside_subnets(p: &mut Project, cat: &Catalog) -> Vec<String> {
    let mut moved = Vec::new();
    let ids: Vec<String> = p.nodes.keys().cloned().collect();
    for id in ids {
        let ty = p.nodes[&id].resource_type.clone();
        if !cat.allows_parent(&ty, "subnet") || p.hosts_nodes(&id) {
            continue;
        }
        let Some(i) = p.edges.iter().position(|e| {
            e.source == id
                && e.relation == Relation::NetworkMembership
                && e.providers.is_empty()
                && p.nodes.get(&e.target).is_some_and(|t| {
                    t.resource_type == "subnet"
                        && t.parent == p.nodes[&id].parent
                        && p.providers_of(&id) == p.providers_of(&t.id)
                })
        }) else {
            continue;
        };
        let subnet = p.edges[i].target.clone();
        let edge = p.edges.remove(i);
        if p.set_parent(&id, Some(&subnet)) {
            moved.push(id);
        } else {
            p.edges.insert(i, edge);
        }
    }
    moved
}

trait Providers {
    fn providers_of(&self, id: &str) -> Vec<String>;
}
impl Providers for Project {
    fn providers_of(&self, id: &str) -> Vec<String> {
        self.nodes
            .get(id)
            .map(|n| n.providers.clone())
            .unwrap_or_default()
    }
}

/// The export as what it declares: per file its lines, sorted, and per entity the blocks
/// it produced. The order of blocks in a file follows the walk of the dependency graph,
/// which a link and a containment enter at different points, so two exports declaring
/// exactly the same blocks may list them in another order; nothing else may differ.
fn generated(p: &Project, cat: &Catalog, provider: &str) -> Result<Vec<(String, String)>, String> {
    generate(p, cat, provider, Tool::OpenTofu)
        .map(|g| {
            let mut blocks: Vec<(String, String)> = g
                .entity_blocks
                .into_iter()
                .map(|(id, b)| {
                    (
                        format!("entity {id}"),
                        format!("{} {:?}\n{}", b.file, b.addresses, b.hcl()),
                    )
                })
                .collect();
            blocks.sort();
            let files = g.files.into_iter().map(|(f, c)| {
                // Manual steps are numbered in that same order.
                let mut lines: Vec<&str> = c
                    .lines()
                    .map(|l| match l.strip_prefix("## ").and_then(|r| r.split_once(". ")) {
                        Some((n, rest)) if n.chars().all(|c| c.is_ascii_digit()) => rest,
                        _ => l,
                    })
                    .collect();
                lines.sort_unstable();
                (f, lines.join("\n"))
            });
            // Entities first: a difference there reads better than sorted lines.
            blocks.extend(files);
            blocks
        })
        .map_err(|e| e.to_string())
}

/// The first difference between two exports, readably.
fn first_difference(
    a: &Result<Vec<(String, String)>, String>,
    b: &Result<Vec<(String, String)>, String>,
) -> String {
    match (a, b) {
        (Ok(a), Ok(b)) => {
            for ((fa, ca), (fb, cb)) in a.iter().zip(b.iter()) {
                if fa != fb {
                    return format!("file {fa} vs {fb}");
                }
                if ca != cb {
                    let (la, lb): (Vec<&str>, Vec<&str>) = (ca.lines().collect(), cb.lines().collect());
                    let i = la
                        .iter()
                        .zip(lb.iter())
                        .position(|(x, y)| x != y)
                        .unwrap_or(la.len().min(lb.len()));
                    let from = i.saturating_sub(3);
                    return format!(
                        "{fa} line {i}:\n--- linked\n{}\n--- inside\n{}",
                        la[from..(i + 4).min(la.len())].join("\n"),
                        lb[from..(i + 4).min(lb.len())].join("\n")
                    );
                }
            }
            format!("{} vs {} files", a.len(), b.len())
        }
        _ => format!("{a:?}\nvs\n{b:?}").chars().take(3000).collect(),
    }
}

fn diags(p: &Project, cat: &Catalog, provider: &str) -> Vec<String> {
    diagnostics::run(p, cat, provider)
        .into_iter()
        .map(|d| format!("{:?} {:?} {:?} {}", d.entity, d.severity, d.code, d.message))
        .collect()
}

fn reachability(p: &Project, cat: &Catalog, provider: &str) -> Vec<String> {
    let r = reach::analyse(p, cat, provider);
    let mut out: Vec<String> = r
        .posture
        .iter()
        .map(|(id, po)| {
            format!(
                "{id}: {:?} {:?} {:?} {:?}",
                po.subnets, po.security_groups, po.egress, po.exposed
            )
        })
        .collect();
    for id in r.posture.keys() {
        for path in reach::paths_from(p, cat, &r, id) {
            out.push(format!(
                "{id} -> {}: {:?} {:?} {}",
                path.target, path.status, path.hops, path.reason
            ));
        }
    }
    out
}

#[test]
fn a_resource_inside_a_subnet_is_the_resource_linked_to_it() {
    let cat = Catalog::builtin();
    let mut total = 0;
    for name in [
        "three-tier.ttg.json",
        "platform.ttg.json",
        "kubernetes.ttg.json",
        "managed-data.ttg.json",
        "containers.ttg.json",
        "job-pipeline.ttg.json",
        "hardened.ttg.json",
        "hub-spoke.ttg.json",
        "edge.ttg.json",
    ] {
        let linked = example(name);
        let mut inside = linked.clone();
        let moved = draw_inside_subnets(&mut inside, &cat);
        let report = ttg_core::validate::structural(&inside);
        assert!(!report.has_errors(), "{name}: {report}");
        total += moved.len();
        for provider in PROVIDERS {
            let (a, b) = (
                generated(&linked, &cat, provider),
                generated(&inside, &cat, provider),
            );
            assert!(
                a == b,
                "{name} on {provider}: the export changed when {moved:?} were drawn inside their subnets: {}",
                first_difference(&a, &b)
            );
            assert_eq!(
                diags(&linked, &cat, provider),
                diags(&inside, &cat, provider),
                "{name} on {provider}: diagnostics changed"
            );
            assert_eq!(
                reachability(&linked, &cat, provider),
                reachability(&inside, &cat, provider),
                "{name} on {provider}: reachability changed"
            );
        }
    }
    assert!(total >= 10, "only {total} resources were moved into subnets");
}

/// A resource spanning two subnets, drawn inside one and linked to the other, is in
/// both: the same `subnet_ids` / subnet group as two links.
#[test]
fn drawn_inside_one_subnet_and_linked_to_another_spans_both() {
    let cat = Catalog::builtin();
    let linked = example("three-tier.ttg.json");
    let (db, first) = linked
        .edges
        .iter()
        .find(|e| {
            e.relation == Relation::NetworkMembership
                && linked.nodes[&e.source].resource_type == "relational_database"
        })
        .map(|e| (e.source.clone(), e.target.clone()))
        .expect("the example's database is linked to subnets");
    assert!(
        linked
            .edges_from(&db)
            .filter(|e| e.relation == Relation::NetworkMembership)
            .count()
            >= 2,
        "the database spans two subnets"
    );
    let mut inside = linked.clone();
    inside
        .edges
        .retain(|e| !(e.source == db && e.target == first && e.relation == Relation::NetworkMembership));
    assert!(inside.set_parent(&db, Some(&first)));
    let e = inside.entity(&db).unwrap();
    let targets = diagnostics::relation_targets(&inside, &cat, &e, Relation::NetworkMembership);
    assert_eq!(targets.first(), Some(&first), "home subnet first: {targets:?}");
    assert!(targets.len() >= 2, "{targets:?}");
    for provider in PROVIDERS {
        let (a, b) = (
            generated(&linked, &cat, provider),
            generated(&inside, &cat, provider),
        );
        assert!(a == b, "{provider}: {}", first_difference(&a, &b));
    }
}

/// Only nodes whose type takes a membership link to a subnet go inside one; a subnet
/// never holds a subnet, a container, or anything inside a node already.
#[test]
fn what_a_subnet_may_hold() {
    let cat = Catalog::builtin();
    assert!(cat.may_contain("subnet") && !cat.is_container("subnet"));
    for t in [
        "compute_instance",
        "load_balancer",
        "relational_database",
        "container_app",
        "nat_gateway",
    ] {
        assert!(cat.allows_parent(t, "subnet"), "{t}");
    }
    for t in [
        "subnet",
        "object_storage",
        "iam_role",
        "security_group",
        "virtual_network",
    ] {
        assert!(!cat.allows_parent(t, "subnet"), "{t}");
    }
    let mut p = example("three-tier.ttg.json");
    let subnets: Vec<String> = p
        .nodes
        .values()
        .filter(|n| n.resource_type == "subnet")
        .map(|n| n.id.clone())
        .collect();
    let (a, b) = (&subnets[0], &subnets[1]);
    let vm = p
        .nodes
        .values()
        .find(|n| n.resource_type == "compute_instance")
        .unwrap()
        .id
        .clone();
    let vpc = p
        .containers
        .values()
        .find(|c| c.container_type == "virtual_network")
        .unwrap()
        .id
        .clone();
    assert!(
        !p.set_parent(&vpc, Some(a)),
        "a container never goes inside a node"
    );
    assert!(
        !p.set_parent(b, Some(a)) || !p.hosts_nodes(b),
        "structure only: the catalog refuses it"
    );
    p.set_parent(b, Some(&vpc));
    assert!(p.set_parent(&vm, Some(a)));
    assert!(
        !p.set_parent(a, Some(b)),
        "a subnet holding resources does not go inside a node"
    );
    assert!(p.is_box(a) && p.hosts_nodes(a));
    assert_eq!(
        p.ancestors(&vm).first().map(|c| c.id.as_str()),
        Some(vpc.as_str())
    );
    assert!(p.is_ancestor(a, &vm) && p.descendants_of(&vpc).contains(&vm));
    // Deleting the subnet keeps what it held, one level up.
    p.remove_entity(a);
    assert_eq!(p.nodes[&vm].parent.as_deref(), Some(vpc.as_str()));
    // And the diagnostics refuse a type that may not go inside a subnet.
    let mut q = example("three-tier.ttg.json");
    let s = q
        .nodes
        .values()
        .find(|n| n.resource_type == "subnet")
        .unwrap()
        .id
        .clone();
    let bucket = q
        .nodes
        .values()
        .find(|n| n.resource_type == "object_storage")
        .unwrap()
        .id
        .clone();
    assert!(q.set_parent(&bucket, Some(&s)));
    assert!(diagnostics::run(&q, &cat, "aws")
        .iter()
        .any(|d| d.entity.as_deref() == Some(bucket.as_str()) && d.code == ttg_codegen::Code::BadParent));
}

/// A subnet holding resources is still a node: an environment's override on it applies,
/// what it holds stays inside it there, and a subnet absent from an environment leaves
/// what it held one level up — as a link to an absent subnet goes with it.
#[test]
fn environment_overrides_reach_a_subnet_holding_resources() {
    let cat = Catalog::builtin();
    let mut p = example("containers.ttg.json");
    p.set_environments(vec!["staging".into(), "prod".into()]).unwrap();
    assert!(p.set_parent("nat-shop", Some("sub-pub-a")));
    let o = p.overrides_mut("sub-pub-a").unwrap();
    let mut az = ttg_core::environment::EnvOverride::default();
    az.provider_config.entry("aws".into()).or_default().insert(
        "availability_zone".into(),
        ttg_core::Value::Str("eu-west-2c".into()),
    );
    o.insert("prod".into(), az);
    let prod = p.for_environment("prod");
    assert_eq!(prod.nodes["nat-shop"].parent.as_deref(), Some("sub-pub-a"));
    assert_eq!(
        canvas::availability_zone(&prod, "sub-pub-a").as_deref(),
        Some("eu-west-2c")
    );
    let g = generate(&prod, &cat, "aws", Tool::OpenTofu).unwrap();
    assert!(
        g.files["network.tf"].contains("\"eu-west-2c\""),
        "the override reaches the subnet"
    );
    let staging = p.for_environment("staging");
    assert_eq!(
        canvas::availability_zone(&staging, "sub-pub-a").as_deref(),
        Some("eu-west-2a")
    );
    // Absent from staging: the gateway moves up to the network.
    let o = p.overrides_mut("sub-pub-a").unwrap();
    o.entry("staging".into()).or_default().absent = true;
    let staging = p.for_environment("staging");
    assert!(!staging.nodes.contains_key("sub-pub-a"));
    assert_eq!(staging.nodes["nat-shop"].parent.as_deref(), Some("vnet-shop"));
}

/// Each tier is derived the same way every time: from the type, a subnet's from what is
/// in it, a security group's from what carries it, a container's from what it holds.
#[test]
fn tiers_are_derived_deterministically() {
    let cat = Catalog::builtin();
    let p = example("containers.ttg.json");
    let t = canvas::tiers(&p, &cat, &Default::default());
    assert_eq!(t, canvas::tiers(&p.clone(), &cat, &Default::default()));
    let tier = |id: &str| t[id];
    assert_eq!(tier("lb-shop"), Tier::Public);
    assert_eq!(tier("app-web"), Tier::App);
    assert_eq!(tier("db-shop"), Tier::Data);
    assert_eq!(tier("q-jobs"), Tier::Data);
    assert_eq!(tier("role-web"), Tier::Regional);
    assert_eq!(tier("alm-worker"), Tier::Regional);
    assert_eq!(tier("topic-alerts"), Tier::Regional);
    assert_eq!(tier("cert-shop"), Tier::Edge);
    // From what is in them / carries them.
    assert_eq!(tier("sub-pub-a"), Tier::Public, "the load balancer is in it");
    assert_eq!(tier("sub-app-a"), Tier::App, "the apps are in it");
    assert_eq!(tier("sub-db-a"), Tier::Data, "the database is in it");
    assert_eq!(tier("sg-lb"), Tier::Public, "carried by the load balancer");
    assert_eq!(tier("sg-db"), Tier::Data, "carried by the database");
    // An override wins, and changes what follows from it.
    let o: BTreeMap<String, Tier> = [("db-shop".to_string(), Tier::App)].into();
    let t2 = canvas::tiers(&p, &cat, &o);
    assert_eq!(t2["db-shop"], Tier::App);
    assert_eq!(t2["sg-db"], Tier::App);
    // Rows: a subnet's own zone; what spans two zones is shared.
    assert_eq!(canvas::tier_row(&p, &cat, "sub-app-b"), "b");
    assert_eq!(canvas::tier_row(&p, &cat, "lb-shop"), "");
    assert_eq!(canvas::zone_label("eu-west-2a"), "a");
    assert_eq!(canvas::zone_label("europe-west2-b"), "b");
    assert_eq!(canvas::zone_label("1"), "1");
    assert_eq!(canvas::zone_badge(&p, &cat, "lb-shop").as_deref(), Some("a b"));
    // Laying out twice gives the same picture, bands left to right.
    let size = |_: &Project, _: &str| ttg_core::NODE_SIZE;
    let opts = ttg_core::layout::TidyOptions::default();
    let mut a = p.clone();
    canvas::tidy_by_tiers(&mut a, &cat, &Default::default(), &size, &opts, false);
    let mut b = p.clone();
    canvas::tidy_by_tiers(&mut b, &cat, &Default::default(), &size, &opts, false);
    assert_eq!(a, b);
    let x = |q: &Project, id: &str| q.entity(id).unwrap().position.x;
    let y = |q: &Project, id: &str| q.entity(id).unwrap().position.y;
    assert!(x(&a, "lb-shop") < x(&a, "app-web") && x(&a, "app-web") < x(&a, "db-shop"));
    // Zone rows line up across bands: subnet a of every tier above subnet b.
    assert_eq!(y(&a, "sub-pub-a"), y(&a, "sub-app-a"));
    assert!(y(&a, "sub-app-a") < y(&a, "sub-app-b"));
    // Nothing that is exported moved but positions and sizes.
    let mut a2 = a.clone();
    for n in a2.nodes.values_mut() {
        n.position = p.nodes[&n.id].position;
        n.size = p.nodes[&n.id].size;
    }
    for c in a2.containers.values_mut() {
        c.position = p.containers[&c.id].position;
        c.size = p.containers[&c.id].size;
    }
    assert_eq!(a2, p);
    // Top to bottom.
    let mut v = p.clone();
    canvas::tidy_by_tiers(&mut v, &cat, &Default::default(), &size, &opts, true);
    assert!(y(&v, "lb-shop") < y(&v, "app-web") && y(&v, "app-web") < y(&v, "db-shop"));
}

/// The containers example with the textbook three routes: the public subnets via the
/// internet gateway, the app subnets via NAT, and the data subnets in a table of their
/// own with no default route.
fn routed_three_ways() -> Project {
    let mut p = example("containers.ttg.json");
    p.edges
        .retain(|e| !(e.source == "rt-private" && e.target.starts_with("sub-db-")));
    let mut rt = p.nodes["rt-private"].clone();
    rt.id = "rt-isolated".into();
    rt.name = "isolated routes".into();
    p.nodes.insert(rt.id.clone(), rt);
    for s in ["sub-db-a", "sub-db-b"] {
        p.add_edge("rt-isolated", s, Relation::Attachment);
    }
    p
}

/// A subnet is public, private or isolated by its route table, and that decides its
/// tier: public for an internet gateway, else what is in it (apps or data), else app for
/// NAT and data for no default route; what is in a public subnet goes with it. Without a
/// route table the name and the public-address flag still say.
#[test]
fn subnets_take_their_tier_from_their_route_table() {
    let cat = Catalog::builtin();
    let p = routed_three_ways();
    let route = |q: &Project, id: &str| canvas::subnet_route(q, id);
    assert_eq!(route(&p, "sub-pub-b"), Some(SubnetRoute::Public));
    assert_eq!(route(&p, "sub-app-a"), Some(SubnetRoute::Private));
    assert_eq!(route(&p, "sub-db-b"), Some(SubnetRoute::Isolated));
    let t = canvas::tiers(&p, &cat, &Default::default());
    assert_eq!(
        (t["sub-pub-a"], t["sub-app-a"], t["sub-db-a"]),
        (Tier::Public, Tier::App, Tier::Data)
    );
    // One table shared by apps and data (as in the example) says little: what is in the
    // subnets tells them apart.
    let shared = example("containers.ttg.json");
    assert_eq!(route(&shared, "sub-db-a"), Some(SubnetRoute::Private));
    let ts = canvas::tiers(&shared, &cat, &Default::default());
    assert_eq!((ts["sub-app-a"], ts["sub-db-a"]), (Tier::App, Tier::Data));
    // Empty, they take it from the route alone.
    let mut empty = p.clone();
    empty
        .edges
        .retain(|e| !(e.relation == Relation::NetworkMembership && e.target.starts_with("sub-")));
    let te = canvas::tiers(&empty, &cat, &Default::default());
    assert_eq!(
        (te["sub-pub-a"], te["sub-app-b"], te["sub-db-b"]),
        (Tier::Public, Tier::App, Tier::Data)
    );
    // The apps in subnets routed to the internet gateway are in the public tier.
    let mut open = p.clone();
    open.edges
        .retain(|e| !(e.source == "rt-private" && e.target.starts_with("sub-app-")));
    for s in ["sub-app-a", "sub-app-b"] {
        open.add_edge("rt-public", s, Relation::Attachment);
    }
    let to = canvas::tiers(&open, &cat, &Default::default());
    assert_eq!(
        (to["sub-app-a"], to["app-web"], to["db-shop"]),
        (Tier::Public, Tier::Public, Tier::Data)
    );
    // The internet gateway is the edge.
    assert_eq!(t["igw-shop"], Tier::Edge);
    // No route tables: the name, the flag and what is in the subnet decide, as before.
    let mut bare = p.clone();
    bare.nodes.retain(|_, n| n.resource_type != "route_table");
    let nodes = bare.nodes.clone();
    bare.edges
        .retain(|e| nodes.contains_key(&e.source) && nodes.contains_key(&e.target));
    assert_eq!(route(&bare, "sub-pub-a"), None);
    let tb = canvas::tiers(&bare, &cat, &Default::default());
    assert_eq!(
        (tb["sub-pub-a"], tb["sub-app-a"], tb["sub-db-a"]),
        (Tier::Public, Tier::App, Tier::Data)
    );
}

/// The network view reads left to right — internet gateway, public, private, isolated —
/// with a row per availability zone across the tiers, the zones' subnets above what spans
/// them and the security groups last, and lays out only what the view shows.
#[test]
fn the_network_view_groups_subnets_by_tier_and_zone() {
    let cat = Catalog::builtin();
    let p = routed_three_ways();
    let visible = views::visible_set(&p, &cat, &canvas::network_view_filter());
    let shown = |id: &str| visible.as_ref().is_none_or(|v| v.contains(id));
    assert!(!shown("alm-worker") && shown("lb-shop"));
    let size = |q: &Project, id: &str| {
        q.nodes
            .get(id)
            .and_then(|n| n.size)
            .unwrap_or(ttg_core::NODE_SIZE)
    };
    let opts = ttg_core::layout::TidyOptions::default();
    let mut a = p.clone();
    canvas::tidy_shown_by_tiers(&mut a, &cat, &Default::default(), &shown, &size, &opts, false);
    let x = |id: &str| a.entity(id).unwrap().position.x;
    let y = |id: &str| a.entity(id).unwrap().position.y;
    assert!(x("igw-shop") < x("sub-pub-a"), "the gateway is the edge");
    assert!(x("sub-pub-a") < x("sub-app-a") && x("sub-app-a") < x("sub-db-a"));
    assert_eq!(x("sub-pub-a"), x("sub-pub-b"), "a tier is one column");
    // Rows: the zones line up across the tiers.
    assert_eq!(y("sub-pub-a"), y("sub-app-a"));
    assert_eq!(y("sub-app-a"), y("sub-db-a"));
    assert_eq!(y("sub-pub-b"), y("sub-db-b"));
    assert!(y("sub-pub-a") < y("sub-pub-b"));
    // What spans zones is under their subnets, the firewall under that.
    assert!(y("lb-shop") > y("sub-db-b") && y("rt-public") > y("sub-db-b"));
    assert!(y("sg-lb") > y("lb-shop"));
    // The database is with the tier of its subnets, not the gateway's.
    assert!(x("lb-shop") < x("db-shop"));
    // Nothing is drawn on top of anything else.
    let nodes: Vec<&str> = p
        .nodes
        .keys()
        .map(String::as_str)
        .filter(|id| shown(id) && !a.is_box(id))
        .collect();
    for (i, m) in nodes.iter().enumerate() {
        let r = |id: &str| ttg_core::view::Rect::new(a.entity(id).unwrap().position, size(&a, id));
        for o in &nodes[i + 1..] {
            assert!(!r(m).intersects(r(o)), "{m} overlaps {o}");
        }
    }
    // The same layout twice is the same picture.
    let mut b = p.clone();
    canvas::tidy_shown_by_tiers(&mut b, &cat, &Default::default(), &shown, &size, &opts, false);
    assert_eq!(a, b);
}

/// Security-group rows become edges between groups (or badges for addresses), in the
/// direction the traffic goes, labelled with protocol and ports.
#[test]
fn security_group_rules_become_edges() {
    let p = example("containers.ttg.json");
    let edges = canvas::rule_edges(&p);
    let find = |g: &str, name: &str| edges.iter().find(|e| e.group == g && e.name == name).unwrap();
    let from_lb = find("sg-web", "from-lb");
    assert_eq!(from_lb.peer, canvas::Peer::Entity("sg-lb".into()));
    assert_eq!((from_lb.from(), from_lb.to()), (Some("sg-lb"), Some("sg-web")));
    assert_eq!(from_lb.label, "tcp 8080");
    assert_eq!(from_lb.row, 0);
    let pg = find("sg-db", "pg-from-worker");
    assert_eq!((pg.from(), pg.to()), (Some("sg-worker"), Some("sg-db")));
    assert_eq!(pg.label, "tcp 5432");
    let https = find("sg-lb", "https-in");
    assert_eq!(https.peer, canvas::Peer::Outside("internet".into()));
    assert_eq!((https.from(), https.to()), (None, Some("sg-lb")));
    let out = find("sg-lb", "to-apps");
    assert_eq!(out.peer, canvas::Peer::Outside("10.40.0.0/16".into()));
    assert!(!out.ingress && out.to().is_none());
    let all = find("sg-web", "all-out");
    assert_eq!(all.label, "all");
    // An egress rule to a group points from this group to that one.
    let mut q = p.clone();
    if let Some(ttg_core::Value::Records(rows)) = q.nodes.get_mut("sg-web").unwrap().config.get_mut("rules") {
        rows[1].insert("source_group".into(), ttg_core::Value::Str("sg-db".into()));
        rows[1].insert("cidr".into(), ttg_core::Value::Str(String::new()));
    }
    let e = canvas::rule_edges(&q);
    let eg = e
        .iter()
        .find(|x| x.group == "sg-web" && x.name == "all-out")
        .unwrap();
    assert_eq!((eg.from(), eg.to()), (Some("sg-web"), Some("sg-db")));
    // An AWS-managed prefix list is named for what it is.
    assert_eq!(
        canvas::prefix_list_label("com.amazonaws.global.cloudfront.origin-facing"),
        "CloudFront"
    );
    // The members of a group: what links to it with "uses".
    assert_eq!(canvas::group_members(&p, "sg-db"), vec!["db-shop".to_string()]);
}

/// Monitoring links are the ones from or to an alarm, log group, audit trail or budget;
/// the region frame names the configured region and the alias region in use.
#[test]
fn monitoring_links_and_regions() {
    let cat = Catalog::builtin();
    let p = example("containers.ttg.json");
    assert!(canvas::is_monitoring_link(&p, &cat, "alm-worker", "topic-alerts"));
    assert!(canvas::is_monitoring_link(&p, &cat, "app-web", "log-apps"));
    assert!(!canvas::is_monitoring_link(&p, &cat, "app-web", "db-shop"));
    let f = canvas::network_view_filter();
    assert!(f.hide_monitoring && f.categories.contains("network"));
    let edge = example("edge.ttg.json");
    let region = canvas::project_region(&edge, &cat, "aws").unwrap();
    assert!(!region.is_empty());
    let aliased = canvas::alias_regions(&edge, &cat, "aws");
    assert_eq!(
        aliased.get("cert-site").map(String::as_str),
        Some("us-east-1"),
        "{aliased:?}"
    );
    assert_eq!(
        aliased.get("waf-edge").map(String::as_str),
        Some("us-east-1"),
        "{aliased:?}"
    );
    assert!(canvas::alias_regions(&edge, &cat, "azure").is_empty());
}

/// The loader's rules for nodes holding nodes and for tiers.
#[test]
fn may_contain_and_tier_are_checked_at_load() {
    let aws = include_str!("../../../definitions/providers/aws.toml");
    let holder = |may: &str, kind: &str| {
        format!(
            "schema_version = 2\n[resource]\ntype = \"slot\"\ncategory = \"network\"\ndisplay_name = \"Slot\"\nkind = \"{kind}\"\n{may}\n[providers.aws]\n[[providers.aws.blocks]]\nkey = \"main\"\nresource = \"aws_subnet\"\n"
        )
    };
    let thing = |relation: &str, version: u32| {
        format!(
            "schema_version = {version}\n[resource]\ntype = \"thing\"\ncategory = \"compute\"\ndisplay_name = \"Thing\"\nallowed_parents = [\"slot\"]\n{relation}\n[providers.aws]\n[[providers.aws.blocks]]\nkey = \"main\"\nresource = \"aws_instance\"\n"
        )
    };
    let via = "[[relations]]\nkind = \"network_membership\"\ntargets = [\"slot\"]\ncardinality = \"many\"\nvia_parent = true\n";
    let load = |h: &str, t: &str| {
        Catalog::from_sources(
            [("slot.toml", h), ("thing.toml", t)].into_iter(),
            [("aws.toml", aws)].into_iter(),
        )
        .map(|_| ())
    };
    load(&holder("may_contain = true", "node"), &thing(via, 2))
        .expect("a node that may contain, with a via_parent relation");
    let err = load(&holder("", "node"), &thing(via, 2)).expect_err("a node without may_contain");
    assert!(err.to_string().contains("not a container type"), "{err}");
    let err = load(&holder("may_contain = true", "node"), &thing("", 2)).expect_err("nothing implied");
    assert!(
        err.to_string().contains("no via_parent relation targets it"),
        "{err}"
    );
    let err = load(&holder("may_contain = true", "container"), &thing(via, 2)).expect_err("a container");
    assert!(err.to_string().contains("may_contain is for nodes"), "{err}");
    let err = load(&holder("may_contain = true", "node"), &thing(via, 1)).expect_err("a v1 file");
    assert!(err.to_string().contains("schema_version 2 features"), "{err}");
    let err = load(
        &holder("may_contain = true\ntier = \"cloud\"", "node"),
        &thing(via, 2),
    )
    .expect_err("unknown tier");
    assert!(err.to_string().contains("tier"), "{err}");
    load(
        &holder("may_contain = true\ntier = \"data\"", "node"),
        &thing(via, 2),
    )
    .expect("a known tier");
}
