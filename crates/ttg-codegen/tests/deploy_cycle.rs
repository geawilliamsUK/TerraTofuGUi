//! A stack that is destroyed and deployed again, and a configuration that works the first
//! time: variables that hand-written expressions still use stay declared, the database
//! connection URI is encoded and asks for TLS, a disposable project leaves nothing behind
//! that would stop the next apply, alarms are checked against what they watch, and a
//! container's public IP for calling out is not mistaken for public ingress.

use serde_json::json;
use std::path::Path;
use ttg_catalog::Catalog;
use ttg_codegen::{diagnostics, generate, Severity};
use ttg_core::{Project, Tool, Value};

fn example(name: &str) -> Project {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name);
    ttg_core::project::load(&p).expect("example loads")
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn add(p: &mut Project, v: serde_json::Value) {
    let n: ttg_core::Node = serde_json::from_value(v).unwrap();
    p.nodes.insert(n.id.clone(), n);
}

fn link(p: &mut Project, source: &str, target: &str, relation: &str) {
    let e: ttg_core::Edge =
        serde_json::from_value(json!({"source": source, "target": target, "relation": relation})).unwrap();
    p.edges.push(e);
}

fn set(p: &mut Project, id: &str, field: &str, v: serde_json::Value) {
    let v: Value = serde_json::from_value(v).unwrap();
    p.nodes.get_mut(id).unwrap().config.insert(field.into(), v);
}

fn set_provider(p: &mut Project, id: &str, provider: &str, field: &str, v: serde_json::Value) {
    let v: Value = serde_json::from_value(v).unwrap();
    p.nodes
        .get_mut(id)
        .unwrap()
        .provider_config
        .entry(provider.into())
        .or_default()
        .insert(field.into(), v);
}

fn messages(p: &Project, cat: &Catalog, provider: &str, sev: Severity) -> Vec<String> {
    diagnostics::run(p, cat, provider)
        .into_iter()
        .filter(|d| d.severity == sev)
        .map(|d| d.message)
        .collect()
}

fn all_files(g: &ttg_codegen::Generated) -> String {
    g.files.values().cloned().collect::<Vec<_>>().join("\n")
}

// ------------------------------------------------------------------ variables

#[test]
fn an_override_that_still_reads_the_mappings_variable_keeps_it_declared() {
    let cat = Catalog::builtin();
    let mut p = example("containers.ttg.json");
    // The app secret's value, computed from its own variable by an extra argument that
    // replaces the mapping's `secret_string`.
    p.nodes.get_mut("sec-app").unwrap().extra.insert(
        "aws".into(),
        serde_json::from_value(json!({"version": {"secret_string": {
            "$raw": "jsonencode(merge(jsondecode(var.app_secret_value), { EXTRA = \"1\" }))"
        }}}))
        .unwrap(),
    );
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let vars = &g.files["variables.tf"];
    assert!(vars.contains("variable \"app_secret_value\""), "{vars}");
    assert!(
        squash(vars).contains("description = \"Value of the secret \\\"app secret\\\".\" sensitive = true"),
        "{vars}"
    );
    assert!(squash(&g.files["secrets.tf"]).contains("jsondecode(var.app_secret_value)"));

    // A mapping-level variable no emitted block uses any more, named by a native resource.
    add(
        &mut p,
        json!({
            "id": "pw-param", "name": "pw param", "parent": "rg-shop",
            "resource_type": "native:aws:aws_ssm_parameter",
            "extra": {"aws": {"main": {"name": "/shop/db", "type": "SecureString",
                "value": {"$raw": "var.db_password"}}}}
        }),
    );
    let mut cat2 = Catalog::builtin();
    cat2.ensure_native_types(&p);
    let g = generate(&p, &cat2, "aws", Tool::OpenTofu).unwrap();
    assert!(g.files["variables.tf"].contains("variable \"db_password\""));

    // A variable no mapping declares is still an error before validate would find it.
    p.nodes
        .get_mut("pw-param")
        .unwrap()
        .extra
        .get_mut("aws")
        .unwrap()
        .get_mut("main")
        .unwrap()
        .insert("value".into(), json!({"$raw": "var.app_secret_bogus"}));
    let errs = messages(&p, &cat2, "aws", Severity::Error);
    assert!(
        errs.iter().any(|m| m.contains(
            "refers to var.app_secret_bogus, which the Amazon Web Services export does not declare"
        )),
        "{errs:?}"
    );
}

#[test]
fn an_extra_argument_can_declare_a_variable_of_its_own() {
    let mut cat = Catalog::builtin();
    let mut p = example("containers.ttg.json");
    add(
        &mut p,
        json!({
            "id": "tag-param", "name": "tag param", "parent": "rg-shop",
            "resource_type": "native:aws:aws_ssm_parameter",
            "extra": {"aws": {"main": {"name": "/shop/release", "type": "String",
                "value": {"$var": {"name": "release_tag", "description": "Image tag to run", "default": "latest"}}}}}
        }),
    );
    add(
        &mut p,
        json!({
            "id": "tag-copy", "name": "tag copy", "parent": "rg-shop",
            "resource_type": "native:aws:aws_ssm_parameter",
            "extra": {"aws": {"main": {"name": "/shop/release-copy", "type": "String",
                "value": {"$raw": "\"${var.release_tag}-copy\""}}}}
        }),
    );
    cat.ensure_native_types(&p);
    assert!(messages(&p, &cat, "aws", Severity::Error).is_empty());
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let vars = squash(&g.files["variables.tf"]);
    assert!(
        vars.contains("variable \"release_tag\" { type = string description = \"Image tag to run\" default = \"latest\" }"),
        "{vars}"
    );
    assert!(squash(&g.files["native.tf"]).contains("value = var.release_tag"));

    // A name the export declares already, and two declarations that disagree, are errors.
    let main = |p: &mut Project, id: &str| {
        p.nodes
            .get_mut(id)
            .unwrap()
            .extra
            .get_mut("aws")
            .unwrap()
            .get_mut("main")
            .unwrap()
            .insert("value".into(), json!({"$var": {"name": "region"}}));
    };
    let mut clash = p.clone();
    main(&mut clash, "tag-copy");
    let errs = messages(&clash, &cat, "aws", Severity::Error);
    assert!(
        errs.iter()
            .any(|m| m.contains("declares var.region, which the export already declares")),
        "{errs:?}"
    );
    let mut twice = p.clone();
    twice
        .nodes
        .get_mut("tag-copy")
        .unwrap()
        .extra
        .get_mut("aws")
        .unwrap()
        .get_mut("main")
        .unwrap()
        .insert(
            "value".into(),
            json!({"$var": {"name": "release_tag", "type": "number"}}),
        );
    let errs = messages(&twice, &cat, "aws", Severity::Error);
    assert!(
        errs.iter()
            .any(|m| m.contains("declares var.release_tag differently")),
        "{errs:?}"
    );
}

// ------------------------------------------------------------------ connection URI

/// The containers example with a Secret whose value is the shop database's URI.
fn with_db_url() -> Project {
    let mut p = example("containers.ttg.json");
    add(
        &mut p,
        json!({
            "id": "sec-dburl", "name": "db url", "resource_type": "secret", "parent": "kv-shop",
            "config": {"generate_value": false}
        }),
    );
    link(&mut p, "sec-dburl", "db-shop", "reads");
    p
}

fn secret_value(p: &Project, provider: &str) -> String {
    let g = generate(p, &Catalog::builtin(), provider, Tool::OpenTofu).unwrap();
    let all = all_files(&g);
    all.lines()
        .find(|l| l.contains("\"postgresql://") || l.contains("\"mysql://"))
        .unwrap_or_else(|| panic!("no URI in the {provider} export"))
        .trim()
        .to_string()
}

#[test]
fn the_database_uri_is_encoded_and_asks_for_tls_when_the_server_forces_it() {
    let mut p = with_db_url();
    // AWS: no parameter forces TLS, so 'auto' adds nothing; the credentials are encoded.
    let v = secret_value(&p, "aws");
    assert_eq!(
        v,
        "secret_string = format(\"postgresql://%s:%s@%s/%s%s\", urlencode(aws_db_instance.shop_db.username), urlencode(aws_db_instance.shop_db.password), aws_db_instance.shop_db.endpoint, aws_db_instance.shop_db.db_name, \"\")"
    );
    // A parameter that forces TLS: `require`.
    set(
        &mut p,
        "db-shop",
        "parameters",
        json!([{"name": "rds.force_ssl", "value": "1"}]),
    );
    assert!(secret_value(&p, "aws").ends_with("\"?sslmode=require\")"));
    // Chosen explicitly.
    set(&mut p, "sec-dburl", "sslmode", json!("verify-full"));
    assert!(secret_value(&p, "aws").ends_with("\"?sslmode=verify-full\")"));
    set(&mut p, "sec-dburl", "sslmode", json!("disable"));
    assert!(secret_value(&p, "aws").ends_with("\"?sslmode=disable\")"));
    // MySQL has a scheme and a parameter of its own.
    set(&mut p, "sec-dburl", "sslmode", json!("auto"));
    set(&mut p, "db-shop", "engine", json!("mysql"));
    set(
        &mut p,
        "db-shop",
        "parameters",
        json!([{"name": "require_secure_transport", "value": "ON"}]),
    );
    let v = secret_value(&p, "aws");
    assert!(v.contains("\"mysql://%s:%s@%s/%s%s\""), "{v}");
    assert!(v.ends_with("\"?ssl-mode=REQUIRED\")"), "{v}");
}

#[test]
fn azure_and_google_cloud_uris_follow_their_own_tls_settings() {
    let mut p = with_db_url();
    // Azure flexible servers require TLS unless told otherwise.
    let v = secret_value(&p, "azure");
    assert!(
        v.contains("urlencode(azurerm_postgresql_flexible_server.shop_db_pg.administrator_password)"),
        "{v}"
    );
    assert!(v.ends_with("\"?sslmode=require\")"), "{v}");
    set(
        &mut p,
        "db-shop",
        "parameters",
        json!([{"name": "require_secure_transport", "value": "off"}]),
    );
    assert!(secret_value(&p, "azure").ends_with(", \"\")"));
    // MySQL reads its own blocks.
    set(&mut p, "db-shop", "engine", json!("mysql"));
    set(&mut p, "db-shop", "parameters", json!([]));
    let v = secret_value(&p, "azure");
    assert!(v.contains("azurerm_mysql_flexible_server.shop_db_my.fqdn"), "{v}");
    assert!(v.ends_with("\"?ssl-mode=REQUIRED\")"), "{v}");

    // Cloud SQL: its SSL mode decides, and the mode is written on the instance.
    let mut p = with_db_url();
    assert!(secret_value(&p, "gcp").ends_with(", \"\")"));
    set_provider(&mut p, "db-shop", "gcp", "ssl_mode", json!("ENCRYPTED_ONLY"));
    assert!(secret_value(&p, "gcp").ends_with("\"?sslmode=require\")"));
    let g = generate(&p, &Catalog::builtin(), "gcp", Tool::OpenTofu).unwrap();
    assert!(squash(&all_files(&g)).contains("ssl_mode = \"ENCRYPTED_ONLY\""));
    // A server parameter that is no Cloud SQL flag is refused.
    set(
        &mut p,
        "db-shop",
        "parameters",
        json!([{"name": "require_ssl", "value": "true"}]),
    );
    let errs = messages(&p, &Catalog::builtin(), "gcp", Severity::Error);
    assert!(
        errs.iter()
            .any(|m| m.contains("use the Google Cloud 'SSL mode' field")),
        "{errs:?}"
    );
}

#[test]
fn auto_tls_says_when_it_cannot_see_the_parameter_group() {
    let mut p = with_db_url();
    p.nodes.get_mut("db-shop").unwrap().extra.insert(
        "aws".into(),
        serde_json::from_value(json!({"main": {"parameter_group_name": "shop-pg16"}})).unwrap(),
    );
    let warns = messages(&p, &Catalog::builtin(), "aws", Severity::Warning);
    assert!(
        warns
            .iter()
            .any(|m| m.contains("which the 'auto' TLS mode cannot read")),
        "{warns:?}"
    );
    set(&mut p, "sec-dburl", "sslmode", json!("require"));
    let warns = messages(&p, &Catalog::builtin(), "aws", Severity::Warning);
    assert!(!warns.iter().any(|m| m.contains("'auto' TLS mode")), "{warns:?}");
}

// ------------------------------------------------------------------ disposable

#[test]
fn a_final_snapshot_is_named_per_creation() {
    let p = example("containers.ttg.json");
    let g = generate(&p, &Catalog::builtin(), "aws", Tool::OpenTofu).unwrap();
    let db = squash(&g.files["database.tf"]);
    assert!(
        db.contains("resource \"random_id\" \"shop_db_final_id\" { byte_length = 4 keepers = { identifier = \"shop-db\" } }"),
        "{db}"
    );
    assert!(db.contains("skip_final_snapshot = false"), "{db}");
    assert!(
        db.contains("final_snapshot_identifier = format(\"%s-final-%s\", \"shop-db\", random_id.shop_db_final_id.hex)"),
        "{db}"
    );
    assert!(g.files["versions.tf"].contains("hashicorp/random"));
}

#[test]
fn a_disposable_project_leaves_nothing_behind_that_stops_the_next_apply() {
    let mut p = example("containers.ttg.json");
    p.settings.disposable = true;
    let cat = Catalog::builtin();
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let all = squash(&all_files(&g));
    assert!(all.contains("skip_final_snapshot = true"), "{all}");
    assert!(!all.contains("final_snapshot_identifier"));
    assert!(!all.contains("random_id"));
    assert!(all.contains("recovery_window_in_days = 0"));
    assert!(
        squash(&g.files["storage.tf"]).contains("bucket = \"shop-uploads-x7q2\" force_destroy = true"),
        "{}",
        g.files["storage.tf"]
    );
    // The zero window is what was asked for, not a mistake.
    let warns = messages(&p, &cat, "aws", Severity::Warning);
    assert!(
        !warns.iter().any(|m| m.contains("recovery window of 0 days")),
        "{warns:?}"
    );

    let g = generate(&p, &cat, "gcp", Tool::OpenTofu).unwrap();
    let all = squash(&all_files(&g));
    assert!(all.contains("force_destroy = true"));
    assert!(all.contains("retain_backups_on_delete = false"));
    assert!(
        all.contains("name = format(\"%s-%s\", \"shop-db\", random_id.shop_db_name_suffix.hex)"),
        "{all}"
    );

    let g = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap();
    let all = squash(&all_files(&g));
    assert!(
        all.contains("purge_protection_enabled = false soft_delete_retention_days = 7"),
        "{all}"
    );
    assert!(!g
        .files
        .get("MANUAL_STEPS.md")
        .is_some_and(|m| m.contains("Keep a copy before destroying")));

    // A versioned bucket an extra argument keeps from emptying is reported.
    p.nodes.get_mut("obj-uploads").unwrap().extra.insert(
        "aws".into(),
        serde_json::from_value(json!({"main": {"force_destroy": false}})).unwrap(),
    );
    let warns = messages(&p, &cat, "aws", Severity::Warning);
    assert!(
        warns
            .iter()
            .any(|m| m.contains("versioned bucket in a disposable project")),
        "{warns:?}"
    );
}

#[test]
fn disposable_is_chosen_per_environment() {
    let mut p = example("containers.ttg.json");
    p.settings.environments = vec!["staging".into(), "prod".into()];
    p.settings.disposable_environments.insert("staging".into(), true);
    assert!(p.for_environment("staging").settings.disposable);
    assert!(!p.for_environment("prod").settings.disposable);
    let cat = Catalog::builtin();
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    assert!(
        g.diagnostics.iter().all(|d| d.severity != Severity::Error),
        "{:?}",
        g.diagnostics
    );
    let staging = squash(&g.files["environments/staging.tfvars"]);
    let prod = squash(&g.files["environments/prod.tfvars"]);
    assert!(staging.contains("recovery_window_in_days = 0"), "{staging}");
    assert!(prod.contains("recovery_window_in_days = 7"), "{prod}");
    // An environment the project does not have is reported.
    p.settings.disposable_environments.insert("qa".into(), true);
    let warns = messages(&p, &cat, "aws", Severity::Warning);
    assert!(
        warns.iter().any(|m| m.contains("\"qa\" is marked disposable")),
        "{warns:?}"
    );
    // Renaming or dropping an environment carries the setting with it.
    p.rename_environment("staging", "stage").unwrap();
    assert_eq!(p.settings.disposable_environments.get("stage"), Some(&true));
    p.set_environments(vec!["prod".into()]).unwrap();
    assert!(p.settings.disposable_environments.is_empty());
}

// ------------------------------------------------------------------ alarms

/// The containers example with an alarm on the shop database.
fn with_db_alarm(metric: &str, threshold: f64) -> Project {
    let mut p = example("containers.ttg.json");
    add(
        &mut p,
        json!({
            "id": "alm-conn", "name": "db connections", "resource_type": "alarm", "parent": "rg-shop",
            "config": {"metric": metric, "threshold": threshold, "comparison": "greater_than", "period_seconds": "300", "evaluation_periods": 2}
        }),
    );
    link(&mut p, "alm-conn", "db-shop", "attribute_reference");
    link(&mut p, "alm-conn", "topic-alerts", "sends_to");
    set_provider(&mut p, "db-shop", "aws", "instance_class", json!("db.t4g.micro"));
    p
}

#[test]
fn a_connections_threshold_above_what_the_class_allows_is_reported() {
    let cat = Catalog::builtin();
    let p = with_db_alarm("db_connections", 140.0);
    let warns = messages(&p, &cat, "aws", Severity::Warning);
    assert!(
        warns.iter().any(|m| m.contains(
            "fires above 140 connections, but the database it watches allows 87 at its instance class, so it can never fire"
        )),
        "{warns:?}"
    );
    let p = with_db_alarm("db_connections", 60.0);
    let warns = messages(&p, &cat, "aws", Severity::Warning);
    assert!(!warns.iter().any(|m| m.contains("can never fire")), "{warns:?}");
    // Azure and Google Cloud judge it by their own sizes: a B1ms allows 50.
    let p = with_db_alarm("db_connections", 60.0);
    let warns = messages(&p, &cat, "azure", Severity::Warning);
    assert!(
        warns.iter().any(|m| m.contains("allows 50 at its SKU")),
        "{warns:?}"
    );
}

#[test]
fn a_percentage_of_the_classes_connections_is_worked_out_at_export() {
    let cat = Catalog::builtin();
    let mut p = with_db_alarm("db_connections_pct", 80.0);
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let m = squash(&g.files["monitoring.tf"]);
    assert!(m.contains("threshold = floor(80 * 87 / 100)"), "{m}");
    assert!(m.contains("metric_name = \"DatabaseConnections\""), "{m}");
    assert!(m.contains("statistic = \"Maximum\""), "{m}");
    // The class from the size table, and on the other clouds their own figures.
    p.nodes.get_mut("db-shop").unwrap().provider_config.remove("aws");
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    assert!(squash(&g.files["monitoring.tf"]).contains("threshold = floor(80 * 87 / 100)"));
    let g = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap();
    assert!(squash(&all_files(&g)).contains("threshold = floor(80 * 50 / 100)"));
    let g = generate(&p, &cat, "gcp", Tool::OpenTofu).unwrap();
    assert!(squash(&all_files(&g)).contains("threshold_value = floor(80 * 25 / 100)"));
    // A class the table does not know cannot give a threshold.
    set_provider(&mut p, "db-shop", "aws", "instance_class", json!("db.x2g.large"));
    let errs = messages(&p, &cat, "aws", Severity::Error);
    assert!(
        errs.iter()
            .any(|m| m.contains("not in the table of known limits")),
        "{errs:?}"
    );
    // Nor does a server that sets its own limit.
    set_provider(&mut p, "db-shop", "aws", "instance_class", json!("db.t4g.micro"));
    set(
        &mut p,
        "db-shop",
        "parameters",
        json!([{"name": "max_connections", "value": "500"}]),
    );
    let errs = messages(&p, &cat, "aws", Severity::Error);
    assert!(
        errs.iter()
            .any(|m| m.contains("not in the table of known limits")),
        "{errs:?}"
    );
}

#[test]
fn thresholds_a_metric_cannot_reach_are_reported_per_environment() {
    let cat = Catalog::builtin();
    let mut p = with_db_alarm("cpu", 100.0);
    let warns = messages(&p, &cat, "aws", Severity::Warning);
    assert!(
        warns
            .iter()
            .any(|m| m.contains("fires above 100 %, which a percentage never exceeds")),
        "{warns:?}"
    );
    set(&mut p, "alm-conn", "metric", json!("db_storage"));
    set(&mut p, "alm-conn", "comparison", json!("less_than"));
    set(&mut p, "alm-conn", "threshold", json!(2147483648_i64));
    let warns = messages(&p, &cat, "aws", Severity::Warning);
    assert!(
        warns.iter().any(|m| m.contains("so it is always in alarm")),
        "{warns:?}"
    );
    set(&mut p, "alm-conn", "threshold", json!(0));
    let warns = messages(&p, &cat, "aws", Severity::Warning);
    assert!(
        warns
            .iter()
            .any(|m| m.contains("is never negative, so it can never fire")),
        "{warns:?}"
    );
    // Missing data treated as breaching does fire.
    p.nodes.get_mut("alm-conn").unwrap().extra.insert(
        "aws".into(),
        serde_json::from_value(json!({"main": {"treat_missing_data": "breaching"}})).unwrap(),
    );
    let warns = messages(&p, &cat, "aws", Severity::Warning);
    assert!(!warns.iter().any(|m| m.contains("never negative")), "{warns:?}");

    // Each environment is judged on its own class.
    let mut p = with_db_alarm("db_connections", 140.0);
    p.settings.environments = vec!["staging".into(), "prod".into()];
    let mut over = ttg_core::environment::EnvOverride::default();
    over.provider_config.insert(
        "aws".into(),
        [("instance_class".to_string(), Value::Str("db.m5.large".into()))]
            .into_iter()
            .collect(),
    );
    p.nodes
        .get_mut("db-shop")
        .unwrap()
        .overrides
        .insert("prod".into(), over);
    let never = |env: &str| {
        messages(&p.for_environment(env), &cat, "aws", Severity::Warning)
            .iter()
            .any(|m| m.contains("can never fire"))
    };
    assert!(never("staging"));
    assert!(!never("prod"));
}

#[test]
fn a_node_count_above_the_clusters_pools_is_reported() {
    let cat = Catalog::builtin();
    let p = example("kubernetes.ttg.json");
    let cluster = p
        .nodes
        .values()
        .find(|n| n.resource_type == "kubernetes_cluster")
        .expect("a cluster")
        .id
        .clone();
    let e = p.entity(&cluster).unwrap();
    let own = diagnostics::field_or_default(&cat, "aws", &e, "max_nodes", false)
        .and_then(|v| ttg_catalog::fields::number(&v))
        .unwrap();
    let pools: f64 = p
        .nodes
        .values()
        .filter(|n| n.resource_type == "kubernetes_node_pool")
        .filter(|n| p.edges.iter().any(|x| x.source == n.id && x.target == cluster))
        .map(|n| {
            let e = p.entity(&n.id).unwrap();
            diagnostics::field_or_default(&cat, "aws", &e, "max_nodes", false)
                .and_then(|v| ttg_catalog::fields::number(&v))
                .unwrap()
        })
        .sum();
    let max = diagnostics::limit_value(&p, &cat, "aws", &e, "max_nodes").unwrap();
    assert_eq!(max, own + pools);

    let parent = e.parent.map(|s| s.to_string());
    let mut p = p.clone();
    add(
        &mut p,
        json!({
            "id": "alm-nodes", "name": "nodes", "resource_type": "alarm",
            "parent": parent,
            "config": {"metric": "node_count", "threshold": max, "comparison": "greater_than", "period_seconds": "300", "evaluation_periods": 2}
        }),
    );
    link(&mut p, "alm-nodes", &cluster, "attribute_reference");
    let warns = messages(&p, &cat, "aws", Severity::Warning);
    assert!(
        warns
            .iter()
            .any(|m| m.contains(&format!("but the cluster it watches has at most {max}"))),
        "{warns:?}"
    );
}

// ------------------------------------------------------------------ public IP

fn steps(p: &Project) -> String {
    let g = generate(p, &Catalog::builtin(), "aws", Tool::OpenTofu).unwrap();
    g.files.get("MANUAL_STEPS.md").cloned().unwrap_or_default()
}

const FRONT: &str = "Front the service with a load balancer";

#[test]
fn a_public_ip_for_calling_out_is_not_public_ingress() {
    let mut p = example("containers.ttg.json");
    // The worker takes no traffic; public ingress on it never asks for a load balancer.
    set(&mut p, "app-worker", "public", json!(true));
    assert!(!steps(&p).contains(FRONT));
    // Web has a load balancer forwarding to it.
    set(&mut p, "app-web", "public", json!(true));
    assert!(!steps(&p).contains(FRONT));
    // Without the link, the step is still there, as it always was...
    p.edges
        .retain(|e| !(e.source == "lb-shop" && e.target == "app-web"));
    assert!(steps(&p).contains(FRONT));
    // ...unless a `load_balancer` block set by hand says a load balancer forwards to it.
    p.nodes.get_mut("app-web").unwrap().extra.insert(
        "aws".into(),
        serde_json::from_value(json!({"main": {"load_balancer": {
            "target_group_arn": "arn:aws:elasticloadbalancing:eu-west-2:1:targetgroup/web/1",
            "container_name": "web", "container_port": 8080
        }}}))
        .unwrap(),
    );
    assert!(!steps(&p).contains(FRONT));

    // A public IP only for calling out: on the tasks, with no step.
    let mut p = example("containers.ttg.json");
    set(&mut p, "app-worker", "outbound_public_ip", json!(true));
    set(&mut p, "job-migrate", "outbound_public_ip", json!(true));
    let g = generate(&p, &Catalog::builtin(), "aws", Tool::OpenTofu).unwrap();
    let c = squash(&g.files["container.tf"]);
    assert!(c.contains("assign_public_ip = true"), "{c}");
    assert!(c.contains("assign_public_ip = \"ENABLED\""), "{c}");
    assert!(!g.files.get("MANUAL_STEPS.md").is_some_and(|m| m.contains(FRONT)));
    // Azure and Google Cloud need none: the export does not change.
    for provider in ["azure", "gcp"] {
        let before = generate(
            &example("containers.ttg.json"),
            &Catalog::builtin(),
            provider,
            Tool::OpenTofu,
        )
        .unwrap();
        let after = generate(&p, &Catalog::builtin(), provider, Tool::OpenTofu).unwrap();
        assert_eq!(before.files, after.files, "{provider}");
    }
}

// ------------------------------------------------------------------ the definitions

/// `has_row`, `limits`, `limit` and `extra` are checked when the catalog loads.
#[test]
fn the_new_vocabulary_is_checked_at_load() {
    let aws = include_str!("../../../definitions/providers/aws.toml");
    let other = r#"
schema_version = 2
[resource]
type = "other"
category = "network"
display_name = "Other"
[[fields]]
name = "size"
type = "int"
[[fields]]
name = "rows"
type = "struct_list"
[[fields.items]]
name = "name"
type = "string"
[providers.aws]
[[providers.aws.blocks]]
key = "main"
resource = "aws_other"
[providers.aws.limits]
most = { field = "size" }
"#;
    let thing = |when: &str, arg: &str| {
        format!(
            r#"
schema_version = 2
[resource]
type = "thing"
category = "network"
display_name = "Thing"
[[fields]]
name = "level"
type = "number"
[[relations]]
kind = "reads"
targets = ["other"]
cardinality = "many"
[providers.aws]
[[providers.aws.blocks]]
key = "main"
resource = "aws_thing"
{when}
[providers.aws.blocks.args]
value = {arg}
"#
        )
    };
    let load = |def: String, other: &str| {
        ttg_catalog::Catalog::from_sources(
            [("thing.toml", def.as_str()), ("other.toml", other)].into_iter(),
            [("aws.toml", aws)].into_iter(),
        )
        .map(|_| ())
    };
    let when = |w: &str| load(thing(&format!("when = {w}"), "{ value = 1 }"), other);
    let arg = |a: &str| load(thing("", a), other);

    when(r#"{ relation = "reads", target_type = "other", where = { field = "rows", has_row = { name = ["a", "b"] } } }"#)
        .expect("has_row");
    let err = when(r#"{ relation = "reads", target_type = "other", where = { field = "size", has_row = { name = "a" } } }"#)
        .expect_err("not a table");
    assert!(
        err.to_string()
            .contains("'size' is not a struct_list or string_list"),
        "{err}"
    );
    let err = when(r#"{ relation = "reads", target_type = "other", where = { field = "rows", has_row = { value = "a" } } }"#)
        .expect_err("not an item");
    assert!(
        err.to_string().contains("'value' is not an item of 'rows'"),
        "{err}"
    );

    arg(r#"{ relation = "reads", target_type = "other", limit = "most" }"#).expect("limit");
    let err = arg(r#"{ relation = "reads", limit = "least" }"#).expect_err("undeclared limit");
    assert!(
        err.to_string()
            .contains("limit 'least' is not declared by any mapping of other"),
        "{err}"
    );
    let err = arg(r#"{ relation = "reads", limit = "most", attr = "id" }"#).expect_err("both");
    assert!(err.to_string().contains("cannot be combined with attr"), "{err}");
    when(r#"{ field = "level", at_least = { relation = "reads", target_type = "other", limit = "most" } }"#)
        .expect("limit bound");
    let err = when(r#"{ field = "level", at_least = { relation = "reads", limit = "least" } }"#)
        .expect_err("undeclared bound");
    assert!(err.to_string().contains("limit 'least' is not declared"), "{err}");

    when(r#"{ extra = "tags" }"#).expect("extra");
    let err = when(r#"{ extra = "tags", block = "nope" }"#).expect_err("unknown block");
    assert!(
        err.to_string().contains("when extra: unknown block 'nope'"),
        "{err}"
    );

    let bad_limit = other.replace(
        r#"most = { field = "size" }"#,
        r#"most = { self_block = "main", attr = "id" }"#,
    );
    let err = load(thing("", "{ value = 1 }"), &bad_limit).expect_err("a reference is no number");
    assert!(err.to_string().contains("a limit may only use"), "{err}");
}
