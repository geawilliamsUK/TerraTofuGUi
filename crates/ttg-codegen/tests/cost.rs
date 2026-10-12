//! The cost estimate: models against hand-worked figures, the examples, and the budget
//! diagnostic.

use serde_json::json;
use std::path::Path;
use ttg_catalog::Catalog;
use ttg_codegen::cost::{self, Estimate, Line, PriceBook, Source, Status};
use ttg_codegen::{Code, Severity};
use ttg_core::Project;

fn example(name: &str) -> Project {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name);
    ttg_core::project::load(&p).expect("example loads")
}

/// A project from nodes (id -> (type, config, aws provider config)) and edges.
fn project(nodes: serde_json::Value, edges: serde_json::Value) -> Project {
    let mut ns = serde_json::Map::new();
    for (id, n) in nodes.as_object().unwrap() {
        ns.insert(
            id.clone(),
            json!({
                "id": id,
                "name": id,
                "resource_type": n[0],
                "config": n[1],
                "provider_config": { "aws": n.get(2).cloned().unwrap_or(json!({})) },
                "position": { "x": 0, "y": 0 },
            }),
        );
    }
    serde_json::from_value(json!({
        "schema_version": 1,
        "name": "t",
        "settings": { "target_provider": "aws", "provider_settings": { "aws": { "region": "eu-west-2" } } },
        "nodes": ns,
        "edges": edges,
    }))
    .unwrap()
}

fn price(table: &str, sku: &str) -> f64 {
    PriceBook::for_provider("aws")
        .unwrap()
        .price(table, sku, "eu-west-2")
        .unwrap_or_else(|| panic!("no {table}/{sku}"))
        .value
}

fn line<'a>(e: &'a Estimate, id: &str) -> &'a Line {
    e.lines
        .iter()
        .find(|l| l.entity == id)
        .unwrap_or_else(|| panic!("no line for {id}"))
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.02
}

#[test]
fn rds_with_high_availability_pays_for_two_instances_and_twice_the_storage() {
    let cat = Catalog::builtin();
    let p = project(
        json!({
            "db": ["relational_database", { "size": "small", "storage": "64", "high_availability": true }],
            "db2": ["relational_database", { "size": "small", "storage": "64", "high_availability": true },
                    { "instance_class": "db.t4g.medium" }],
            "db3": ["relational_database", { "size": "medium", "storage": "32" }],
        }),
        json!([]),
    );
    let e = cost::estimate(&p, &cat, "aws", None).unwrap();
    assert_eq!(e.region, "eu-west-2");
    // size small -> db.t3.micro, the mapping's own table.
    let db = line(&e, "db");
    assert_eq!(db.charges[0].sku, "db.t3.micro");
    assert_eq!(db.charges[0].quantity, 2.0 * 730.0);
    assert_eq!(db.charges[1].quantity, 128.0);
    let want = 2.0 * 730.0 * price("rds_postgres", "db.t3.micro") + 128.0 * price("rds_storage", "gp3");
    assert!(close(db.monthly, want), "{} vs {want}", db.monthly);
    // The instance-class override wins over the size table, as it does in the export.
    assert_eq!(line(&e, "db2").charges[0].sku, "db.t4g.medium");
    // Single-AZ: one instance, the storage once.
    let db3 = line(&e, "db3");
    assert_eq!(db3.charges[0].sku, "db.t3.medium");
    assert_eq!(db3.charges[0].quantity, 730.0);
    assert_eq!(db3.charges[1].quantity, 32.0);
}

#[test]
fn eks_cluster_with_a_gpu_pool_that_scales_to_zero() {
    let cat = Catalog::builtin();
    let mut p = project(
        json!({
            "eks": ["kubernetes_cluster", { "node_size": "medium", "node_count": 2, "min_nodes": 2, "max_nodes": 4 }],
            "gpu": ["kubernetes_node_pool", { "size": "xlarge", "min_nodes": 0, "max_nodes": 4, "desired_nodes": 0, "gpu": true },
                    { "instance_type": "g5.xlarge" }],
            "spot": ["kubernetes_node_pool", { "size": "large", "min_nodes": 1, "max_nodes": 3, "desired_nodes": 2, "spot": true }],
        }),
        json!([
            { "source": "gpu", "target": "eks", "relation": "attachment" },
            { "source": "spot", "target": "eks", "relation": "attachment" },
        ]),
    );
    let e = cost::estimate(&p, &cat, "aws", None).unwrap();
    let eks = line(&e, "eks");
    // Control plane around the clock, two t3.large nodes (node_size medium) around the clock.
    assert_eq!(eks.charges[0].sku, "cluster");
    assert_eq!(eks.charges[0].quantity, 730.0);
    assert_eq!(eks.charges[1].sku, "t3.large");
    assert_eq!(eks.charges[1].quantity, 1460.0);

    // min 0: the pool runs the assumed 8 node-hours a day, not 24/7.
    let gpu = line(&e, "gpu");
    assert_eq!(gpu.charges[0].sku, "g5.xlarge");
    assert!(
        close(gpu.charges[0].quantity, 8.0 * 730.0 / 24.0),
        "{:?}",
        gpu.charges[0]
    );
    assert!(gpu
        .assumptions
        .iter()
        .any(|u| u.key == "pool_node_hours_per_day" && u.value == 8.0));
    assert!(gpu.notes.iter().any(|n| n.contains("scales to zero")));

    // A spot pool that does not scale to zero: desired nodes around the clock at the
    // spot fraction.
    let spot = line(&e, "spot");
    assert_eq!(spot.charges[0].sku, "m5.xlarge");
    assert!(close(spot.charges[0].quantity, 2.0 * 730.0 * 0.35));

    // Four hours a day for this pool only: half the GPU cost, nothing else moves.
    p.settings.cost_assumptions.entities.insert(
        "gpu".into(),
        [("pool_node_hours_per_day".to_string(), 4.0)].into(),
    );
    let e2 = cost::estimate(&p, &cat, "aws", None).unwrap();
    let gpu2 = line(&e2, "gpu");
    assert!(close(gpu2.charges[0].monthly * 2.0, gpu.charges[0].monthly));
    assert_eq!(
        gpu2.assumptions
            .iter()
            .find(|u| u.key == "pool_node_hours_per_day")
            .unwrap()
            .source,
        Source::Entity
    );
    assert_eq!(line(&e2, "eks").monthly, eks.monthly);
}

#[test]
fn nat_gateway_is_hours_plus_data_plus_its_address() {
    let cat = Catalog::builtin();
    let mut p = project(json!({ "nat": ["nat_gateway", {}] }), json!([]));
    let e = cost::estimate(&p, &cat, "aws", None).unwrap();
    let nat = line(&e, "nat");
    let want = 730.0 * price("nat_gateway", "hour")
        + 100.0 * price("nat_gateway", "data")
        + 730.0 * price("public_ipv4", "address");
    assert!(close(nat.monthly, want), "{} vs {want}", nat.monthly);
    assert!(close(e.monthly, want));

    // A project-wide assumption moves it; a one-call assumption wins over that.
    p.settings
        .cost_assumptions
        .values
        .insert("nat_data_gb".into(), 1000.0);
    let e = cost::estimate(&p, &cat, "aws", None).unwrap();
    assert_eq!(line(&e, "nat").charges[1].quantity, 1000.0);
    let opts = cost::Options {
        assumptions: [("nat_data_gb".to_string(), 0.0)].into(),
        ..Default::default()
    };
    let e = cost::estimate_with(&p, &cat, "aws", &opts).unwrap();
    assert_eq!(line(&e, "nat").charges[1].quantity, 0.0);
    assert_eq!(
        line(&e, "nat")
            .assumptions
            .iter()
            .find(|u| u.key == "nat_data_gb")
            .unwrap()
            .source,
        Source::Call
    );
}

#[test]
fn a_region_without_prices_falls_back_and_says_so() {
    let cat = Catalog::builtin();
    let p = project(json!({ "nat": ["nat_gateway", {}] }), json!([]));
    let e = cost::estimate(&p, &cat, "aws", Some("ap-southeast-2")).unwrap();
    assert_eq!(e.region, "ap-southeast-2");
    assert_eq!(e.price_region, "us-east-1");
    assert!(
        e.notes.iter().any(|n| n.contains("ap-southeast-2")),
        "{:?}",
        e.notes
    );
}

#[test]
fn free_and_external_resources_say_why() {
    let cat = Catalog::builtin();
    let mut p = project(
        json!({ "sg": ["security_group", {}], "q": ["event_queue", {}] }),
        json!([]),
    );
    p.nodes.get_mut("q").unwrap().manual = true;
    let e = cost::estimate(&p, &cat, "aws", None).unwrap();
    assert_eq!(line(&e, "sg").status, Status::Free);
    assert!(!line(&e, "sg").notes.is_empty());
    assert_eq!(line(&e, "q").status, Status::NotEstimated);
    assert!(line(&e, "q").notes[0].contains("external"));
    assert_eq!(e.monthly, 0.0);
}

#[test]
fn the_kubernetes_example_is_deterministic_and_in_a_sane_range() {
    let cat = Catalog::builtin();
    let p = example("kubernetes.ttg.json");
    for provider in ["aws", "azure", "gcp"] {
        let a = cost::estimate(&p, &cat, provider, None).unwrap();
        let b = cost::estimate(&p, &cat, provider, None).unwrap();
        assert_eq!(a, b, "{provider}: two estimates differ");
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
        // A small cluster, a database, storage: hundreds of dollars a month, not tens
        // and not tens of thousands.
        assert!(
            (150.0..3000.0).contains(&a.monthly),
            "{provider}: ${} a month",
            a.monthly
        );
        assert!(a.lines.iter().all(|l| l.monthly >= 0.0));
        let sum: f64 = a.lines.iter().map(|l| l.monthly).sum();
        assert!((sum - a.monthly).abs() < 0.05);
        assert!(a.caveat.contains(&a.prices_retrieved));
    }
}

#[test]
fn view_and_group_totals_add_up() {
    let cat = Catalog::builtin();
    // A view's total is the sum of the lines it shows; a group's, of the members its box
    // holds. job-pipeline has one view with four groups.
    let p = example("job-pipeline.ttg.json");
    let e = cost::estimate(&p, &cat, "aws", None).unwrap();
    assert_eq!(e.views.len(), 1);
    let v = &e.views[0];
    let visible = ttg_codegen::views::visible_set(&p, &cat, &p.views[0].filter);
    let want: f64 = e
        .lines
        .iter()
        .filter(|l| visible.as_ref().is_none_or(|s| s.contains(&l.entity)))
        .map(|l| l.monthly)
        .sum();
    assert!(close(v.monthly, want), "{} vs {want}", v.monthly);
    assert!(v.monthly > 0.0 && v.monthly <= e.monthly);
    assert_eq!(v.groups.len(), 4);
    assert!(v.groups.iter().any(|g| g.monthly > 0.0), "{:?}", v.groups);
    for g in &v.groups {
        let sum: f64 = g.entities.iter().map(|id| line(&e, id).monthly).sum();
        assert!(close(g.monthly, sum), "{}: {} vs {sum}", g.label, g.monthly);
    }
    // A view that filters nothing costs what the project does.
    let mut q = p.clone();
    q.views[0].filter = Default::default();
    let e = cost::estimate(&q, &cat, "aws", None).unwrap();
    assert!(close(e.views[0].monthly, e.monthly));
}

#[test]
fn a_budget_the_estimate_exceeds_gets_a_warning_on_the_target_provider() {
    let cat = Catalog::builtin();
    let mut p = project(
        json!({
            "nat": ["nat_gateway", {}],
            "budget": ["budget", { "monthly_limit": 10, "notify_email": "ops@example.com" }],
        }),
        json!([]),
    );
    let d = ttg_codegen::diagnostics::run(&p, &cat, "aws");
    let w: Vec<_> = d.iter().filter(|d| d.code == Code::Cost).collect();
    assert_eq!(w.len(), 1, "{d:?}");
    assert_eq!(w[0].severity, Severity::Warning);
    assert_eq!(w[0].entity.as_deref(), Some("budget"));
    assert!(
        w[0].message.contains("exceeds the budget \"budget\" of $10"),
        "{}",
        w[0].message
    );
    // Never reported as another provider's problem.
    assert!(ttg_codegen::diagnostics::other_providers(&p, &cat, "aws")
        .iter()
        .all(|d| d.code != Code::Cost));
    // Within the limit: nothing.
    p.nodes
        .get_mut("budget")
        .unwrap()
        .config
        .insert("monthly_limit".into(), ttg_core::Value::Int(100_000));
    assert!(ttg_codegen::diagnostics::run(&p, &cat, "aws")
        .iter()
        .all(|d| d.code != Code::Cost));
}

#[test]
fn the_budget_check_is_cheap_enough_to_run_with_the_diagnostics() {
    // It runs inside `diagnostics::run`, which the app calls on every change. Measure it
    // on the largest example: well under the cost of the diagnostics themselves.
    let cat = Catalog::builtin();
    let mut p = example("kubernetes.ttg.json");
    p.nodes.insert(
        "budget-x".into(),
        serde_json::from_value(json!({
            "id": "budget-x", "name": "budget-x", "resource_type": "budget",
            "config": { "monthly_limit": 1 }, "position": { "x": 0, "y": 0 }
        }))
        .unwrap(),
    );
    let layer = ttg_codegen::layers::project_for(&p, &cat, "aws");
    let t = std::time::Instant::now();
    for _ in 0..20 {
        assert_eq!(cost::budget_diagnostics(&layer, &cat, "aws").len(), 1);
    }
    let per = t.elapsed() / 20;
    assert!(
        per < std::time::Duration::from_millis(50),
        "budget check took {per:?}"
    );
}

/// The public IPv4 addresses that "Public IP for outbound" and public subnets hand out are
/// billed for every hour they are in use (TF-040).
#[test]
fn public_ipv4_addresses_are_priced_per_running_task_and_instance() {
    let cat = Catalog::builtin();
    let ip = price("public_ipv4", "address");
    let p = project(
        json!({
            "web": ["container_app", { "replicas": 2, "outbound_public_ip": true }],
            "front": ["container_app", { "replicas": 1, "public": true }],
            "quiet": ["container_app", { "replicas": 3 }],
            "job": ["container_job", { "outbound_public_ip": true }],
            "quiet-job": ["container_job", {}],
            "pub": ["subnet", {}, { "map_public_ip": true }],
            "priv": ["subnet", {}],
            "vm-pub": ["compute_instance", { "size": "small" }],
            "vm-priv": ["compute_instance", { "size": "small" }],
        }),
        json!([
            { "source": "vm-pub", "target": "pub", "relation": "network_membership" },
            { "source": "vm-priv", "target": "priv", "relation": "network_membership" },
        ]),
    );
    let e = cost::estimate(&p, &cat, "aws", None).unwrap();
    let ips = |id: &str| -> f64 {
        line(&e, id)
            .charges
            .iter()
            .filter(|c| c.table == "public_ipv4")
            .map(|c| c.quantity)
            .sum()
    };
    // One address per running task, all month.
    assert_eq!(ips("web"), 2.0 * 730.0);
    assert!(close(line(&e, "web").charges.last().unwrap().monthly, 2.0 * 730.0 * ip));
    assert_eq!(ips("front"), 730.0);
    assert_eq!(ips("quiet"), 0.0);
    // A job holds one only while a run lasts: 30 runs of 5 minutes.
    assert!(close(ips("job"), 30.0 * 5.0 / 60.0));
    assert_eq!(ips("quiet-job"), 0.0);
    // An instance gets one when its subnet auto-assigns them.
    assert_eq!(ips("vm-pub"), 730.0);
    assert_eq!(ips("vm-priv"), 0.0);
}

/// A NAT gateway's address and a load balancer's addresses (one per zone) are priced once,
/// on their own lines.
#[test]
fn nat_and_load_balancer_addresses_are_not_counted_twice() {
    let cat = Catalog::builtin();
    let p = project(
        json!({ "nat": ["nat_gateway", {}], "alb": ["load_balancer", {}] }),
        json!([]),
    );
    let e = cost::estimate(&p, &cat, "aws", None).unwrap();
    let addresses = |id: &str| -> Vec<f64> {
        line(&e, id)
            .charges
            .iter()
            .filter(|c| c.table == "public_ipv4")
            .map(|c| c.quantity)
            .collect()
    };
    assert_eq!(addresses("nat"), vec![730.0]);
    assert_eq!(addresses("alb"), vec![2.0 * 730.0], "two zones at least");
}

/// Medium is today's defaults; small and large scale the volumes by a tenth and ten and
/// leave what describes the design alone; anything set explicitly wins over the preset.
#[test]
fn usage_presets_scale_volumes_and_yield_to_explicit_values() {
    use ttg_core::UsagePreset;
    let cat = Catalog::builtin();
    let mut p = project(
        json!({ "nat": ["nat_gateway", {}], "logs": ["log_group", {}] }),
        json!([]),
    );
    let at = |p: &Project, preset: Option<UsagePreset>| {
        cost::estimate_with(p, &cat, "aws", &cost::Options { preset, ..Default::default() }).unwrap()
    };
    let nat_qty = |e: &Estimate, sku: &str| {
        line(e, "nat").charges.iter().find(|c| c.sku == sku).unwrap().quantity
    };

    let medium = at(&p, None);
    assert_eq!(medium.usage, UsagePreset::Medium);
    assert_eq!(nat_qty(&medium, "data"), 100.0);
    assert_eq!(at(&p, Some(UsagePreset::Medium)).monthly, medium.monthly);

    let small = at(&p, Some(UsagePreset::Small));
    let large = at(&p, Some(UsagePreset::Large));
    assert_eq!((nat_qty(&small, "data"), nat_qty(&large, "data")), (10.0, 1000.0));
    assert_eq!(
        (nat_qty(&small, "hour"), nat_qty(&large, "hour")),
        (730.0, 730.0),
        "the month is not a volume"
    );
    assert!(small.monthly < medium.monthly && medium.monthly < large.monthly);
    let used = &line(&small, "nat").assumptions;
    assert_eq!(used.iter().find(|u| u.key == "nat_data_gb").unwrap().source, Source::Preset);
    assert_eq!(used.iter().find(|u| u.key == "hours_per_month").unwrap().source, Source::Default);
    assert!(small.assumptions.iter().any(|u| u.key == "log_ingest_gb" && u.value == 0.5));

    // The project's preset applies without a call preset; the call's wins over it.
    p.settings.cost_assumptions.preset = Some(UsagePreset::Large);
    assert_eq!(at(&p, None).usage, UsagePreset::Large);
    assert_eq!(nat_qty(&at(&p, None), "data"), 1000.0);
    assert_eq!(nat_qty(&at(&p, Some(UsagePreset::Small)), "data"), 10.0);
    // A value of the project beats the preset; other volumes still follow it.
    p.settings.cost_assumptions.values.insert("nat_data_gb".into(), 7.0);
    let e = at(&p, None);
    assert_eq!(nat_qty(&e, "data"), 7.0);
    assert_eq!(
        line(&e, "nat").assumptions.iter().find(|u| u.key == "nat_data_gb").unwrap().source,
        Source::Project
    );
    assert_eq!(line(&e, "logs").charges[0].quantity, 50.0);
}

/// The assumptions a preset leaves alone are the ones that describe the design or the
/// month; every other one moves in both directions.
#[test]
fn usage_presets_leave_only_the_listed_assumptions_alone() {
    use ttg_core::UsagePreset;
    for a in cost::ASSUMPTIONS {
        let (small, large) = (a.at(UsagePreset::Small), a.at(UsagePreset::Large));
        if a.is_volume() {
            assert!(small < a.default && a.default < large, "{}", a.key);
        } else {
            assert_eq!((small, large), (a.default, a.default), "{}", a.key);
        }
    }
    let fixed: Vec<&str> = cost::ASSUMPTIONS.iter().filter(|a| !a.is_volume()).map(|a| a.key).collect();
    assert_eq!(fixed.len(), 8, "{fixed:?}");
}

#[test]
fn the_usage_preset_is_saved_only_when_it_is_set() {
    use ttg_core::UsagePreset;
    let mut p = project(json!({}), json!([]));
    assert!(!ttg_core::project::to_string(&p).unwrap().contains("preset"));
    p.settings.cost_assumptions.preset = Some(UsagePreset::Small);
    let text = ttg_core::project::to_string(&p).unwrap();
    assert!(text.contains("\"preset\": \"small\""), "{text}");
    let back = ttg_core::project::load_str(&text).unwrap();
    assert_eq!(back.settings.cost_assumptions.preset, Some(UsagePreset::Small));
    assert_eq!(UsagePreset::parse("Large"), Some(UsagePreset::Large));
    assert_eq!(UsagePreset::parse("huge"), None);
}

/// A Google Cloud VM with an external address pays for it (TF-040, other providers).
#[test]
fn a_gcp_instance_with_an_external_address_pays_for_it() {
    let cat = Catalog::builtin();
    let mut p = project(
        json!({
            "vm": ["compute_instance", { "size": "small" }],
            "vm-private": ["compute_instance", { "size": "small" }],
        }),
        json!([]),
    );
    p.nodes
        .get_mut("vm")
        .unwrap()
        .provider_config
        .entry("gcp".into())
        .or_default()
        .insert("public_ip".into(), ttg_core::Value::Bool(true));
    let e = cost::estimate(&p, &cat, "gcp", None).unwrap();
    let ip = |id: &str| line(&e, id).charges.iter().find(|c| c.table == "external_ip").map(|c| c.monthly);
    assert!(close(ip("vm").unwrap(), 730.0 * 0.005), "{:?}", ip("vm"));
    assert_eq!(ip("vm-private"), None);
}
