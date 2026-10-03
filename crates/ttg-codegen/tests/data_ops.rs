//! Secrets kept out of state, RDS-managed master passwords, free-form database storage,
//! alarms that fire (target-group dimensions, metric math, percentiles, float thresholds),
//! provider-valid log retention, tagged-image lifecycles and the plan-time argument
//! conflict check. Mostly against examples/managed-data.ttg.json.

use std::path::Path;
use ttg_catalog::Catalog;
use ttg_codegen::{generate, Code, GenError, Severity};
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

fn file(p: &Project, provider: &str, name: &str) -> String {
    let g = generate(p, &Catalog::builtin(), provider, Tool::OpenTofu)
        .unwrap_or_else(|e| panic!("{provider}: {e}"));
    squash(
        g.files
            .get(name)
            .unwrap_or_else(|| panic!("{provider}: no {name}")),
    )
}

fn set(p: &mut Project, id: &str, field: &str, v: Value) {
    p.nodes.get_mut(id).unwrap().config.insert(field.into(), v);
}

fn set_provider(p: &mut Project, id: &str, provider: &str, field: &str, v: Value) {
    p.nodes
        .get_mut(id)
        .unwrap()
        .provider_config
        .entry(provider.into())
        .or_default()
        .insert(field.into(), v);
}

fn blocked(p: &Project, provider: &str) -> String {
    match generate(p, &Catalog::builtin(), provider, Tool::OpenTofu) {
        Err(GenError::Blocked(d)) => d
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .map(|d| d.message.clone())
            .collect::<Vec<_>>()
            .join("\n"),
        Err(e) => panic!("{provider}: {e}"),
        Ok(_) => panic!("{provider}: expected the export to be blocked"),
    }
}

// ------------------------------------------------------------------ secrets

#[test]
fn a_secret_managed_outside_has_no_value_anywhere() {
    let p = example("managed-data.ttg.json");
    let cat = Catalog::builtin();

    let aws = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let s = squash(&aws.files["secrets.tf"]);
    assert!(
        s.contains("resource \"aws_secretsmanager_secret\" \"app_config\""),
        "{s}"
    );
    assert!(!s.contains("aws_secretsmanager_secret_version"), "{s}");
    assert!(!aws.files["variables.tf"].contains("app_config_value"));
    let steps = &aws.files["MANUAL_STEPS.md"];
    assert!(steps.contains("aws secretsmanager put-secret-value"), "{steps}");

    let az = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap();
    let s = squash(&az.files["secrets.tf"]);
    assert!(s.contains("value = \"set-outside-opentofu\""), "{s}");
    assert!(s.contains("lifecycle { ignore_changes = [ value ] }"), "{s}");
    assert!(az.files["MANUAL_STEPS.md"].contains("az keyvault secret set"));

    let gcp = generate(&p, &cat, "gcp", Tool::OpenTofu).unwrap();
    let s = squash(&gcp.files["secrets.tf"]);
    assert!(s.contains("google_secret_manager_secret\" \"app_config\""), "{s}");
    assert!(!s.contains("google_secret_manager_secret_version"), "{s}");
    assert!(gcp.files["MANUAL_STEPS.md"].contains("gcloud secrets versions add"));

    // No "generated value in local state" warning: there is no value.
    assert!(aws
        .diagnostics
        .iter()
        .all(|d| d.code != Code::State || d.entity.as_deref() != Some("sec-app")));
}

#[test]
fn a_secret_cannot_be_both_outside_and_generated_or_derived() {
    let mut p = example("managed-data.ttg.json");
    set(&mut p, "sec-app", "generate_value", Value::Bool(true));
    for provider in ["aws", "azure", "gcp"] {
        assert!(
            blocked(&p, provider).contains("both generated and managed outside OpenTofu"),
            "{provider}"
        );
    }
}

#[test]
fn a_database_cannot_take_its_password_from_a_secret_nobody_wrote() {
    let mut p = example("managed-data.ttg.json");
    p.add_edge("db-app", "sec-app", ttg_core::Relation::Reads);
    // AWS: only while the password comes from the secret.
    set_provider(
        &mut p,
        "db-app",
        "aws",
        "master_password",
        Value::Str("secret".into()),
    );
    assert!(blocked(&p, "aws").contains("managed outside OpenTofu"));
    for provider in ["azure", "gcp"] {
        assert!(
            blocked(&p, provider).contains("managed outside OpenTofu"),
            "{provider}"
        );
    }
    set_provider(
        &mut p,
        "db-app",
        "aws",
        "master_password",
        Value::Str("managed".into()),
    );
    generate(&p, &Catalog::builtin(), "aws", Tool::OpenTofu).unwrap();
}

// ------------------------------------------------------------------ databases

#[test]
fn rds_manages_the_master_password_when_asked() {
    let p = example("managed-data.ttg.json");
    let db = file(&p, "aws", "database.tf");
    assert!(db.contains("manage_master_user_password = true"), "{db}");
    assert!(!db.contains(" password = "), "no password argument at all: {db}");
    assert!(!db.contains("var.db_password"), "{db}");
    let outputs = file(&p, "aws", "outputs.tf");
    assert!(
        outputs.contains("aws_db_instance.app_db.master_user_secret[0].secret_arn"),
        "{outputs}"
    );

    // The default is still today's: the variable, or the linked secret.
    let mut p2 = p.clone();
    p2.nodes
        .get_mut("db-app")
        .unwrap()
        .provider_config
        .get_mut("aws")
        .unwrap()
        .remove("master_password");
    let db = file(&p2, "aws", "database.tf");
    assert!(db.contains("password = var.db_password"), "{db}");
    assert!(!db.contains("manage_master_user_password"), "{db}");
    assert!(!file(&p2, "aws", "outputs.tf").contains("master_user_secret"));

    // 'secret' with nothing linked is refused rather than left without a password.
    let mut p3 = p.clone();
    set_provider(
        &mut p3,
        "db-app",
        "aws",
        "master_password",
        Value::Str("secret".into()),
    );
    assert!(blocked(&p3, "aws").contains("no Secret is linked"));
}

#[test]
fn storage_is_free_form_with_autoscaling_and_parameters() {
    let p = example("managed-data.ttg.json");
    let aws = file(&p, "aws", "database.tf");
    assert!(aws.contains("allocated_storage = 20"), "{aws}");
    assert!(aws.contains("max_allocated_storage = 100"), "{aws}");
    assert!(
        aws.contains("resource \"aws_db_parameter_group\" \"app_db_params\""),
        "{aws}"
    );
    assert!(
        aws.contains("family = format(\"postgres%s\", split(\".\", \"16\")[0])"),
        "{aws}"
    );
    assert!(
        aws.contains("parameter { name = \"log_min_duration_statement\" value = \"1000\" apply_method = \"immediate\" }"),
        "{aws}"
    );
    assert!(
        aws.contains("parameter_group_name = aws_db_parameter_group.app_db_params.name"),
        "{aws}"
    );

    // Azure PostgreSQL rounds 20 GB up to its smallest size, with a warning.
    let az = file(&p, "azure", "database.tf");
    assert!(az.contains("if mb >= 20 * 1024][0]"), "{az}");
    assert!(az.contains("auto_grow_enabled = true"), "{az}");
    assert!(
        az.contains("azurerm_postgresql_flexible_server_configuration\" \"app_db_pgconf_0\""),
        "{az}"
    );
    let g = generate(&p, &Catalog::builtin(), "azure", Tool::OpenTofu).unwrap();
    assert!(g
        .diagnostics
        .iter()
        .any(|d| d.message.contains("rounded up to the next")));

    // A size it offers is written as it is.
    let mut p32 = p.clone();
    set(&mut p32, "db-app", "storage", Value::Int(64));
    assert!(file(&p32, "azure", "database.tf").contains("storage_mb = 65536"));

    let gcp = file(&p, "gcp", "database.tf");
    assert!(gcp.contains("disk_size = 20"), "{gcp}");
    assert!(
        gcp.contains("disk_autoresize = true disk_autoresize_limit = 100"),
        "{gcp}"
    );
    assert!(
        gcp.contains("database_flags { name = \"log_min_duration_statement\" value = \"1000\" }"),
        "{gcp}"
    );

    // 0 keeps the size fixed, where the provider would otherwise grow it.
    let mut fixed = p.clone();
    set(&mut fixed, "db-app", "max_storage_gb", Value::Int(0));
    assert!(!file(&fixed, "aws", "database.tf").contains("max_allocated_storage"));
    assert!(file(&fixed, "gcp", "database.tf").contains("disk_autoresize = false"));
}

#[test]
fn storage_minimums_are_checked_per_provider() {
    let mut p = example("managed-data.ttg.json");
    set(&mut p, "db-app", "storage", Value::Int(15));
    assert!(blocked(&p, "aws").contains("less than 20 GB"));
    generate(&p, &Catalog::builtin(), "gcp", Tool::OpenTofu).unwrap();
    set(&mut p, "db-app", "storage", Value::Int(5));
    assert!(blocked(&p, "gcp").contains("less than 10 GB"));

    let mut io = example("managed-data.ttg.json");
    set_provider(&mut io, "db-app", "aws", "storage_type", Value::Str("io1".into()));
    assert!(blocked(&io, "aws").contains("less than 100 GB of provisioned-IOPS"));

    let mut grow = example("managed-data.ttg.json");
    set(&mut grow, "db-app", "max_storage_gb", Value::Int(20));
    assert!(blocked(&grow, "aws").contains("must be larger than the storage"));
    assert!(blocked(&grow, "gcp").contains("must be larger than the storage"));
}

/// Files saved while storage and retention were enums of numbers still export the same.
#[test]
fn values_saved_under_the_old_field_types_still_load() {
    let cat = Catalog::builtin();
    let mut p = example("managed-data.ttg.json");
    set(&mut p, "db-app", "storage", Value::Str("64".into()));
    set(&mut p, "log-app", "retention_days", Value::Str("90".into()));
    set_provider(
        &mut p,
        "alm-cpu",
        "aws",
        "statistic",
        Value::Str("Maximum".into()),
    );
    set_provider(
        &mut p,
        "alm-storage",
        "aws",
        "statistic",
        Value::Str("Average".into()),
    );
    p.nodes.get_mut("alm-cpu").unwrap().config.remove("statistic");
    p.nodes.get_mut("alm-storage").unwrap().config.remove("statistic");

    assert!(file(&p, "aws", "database.tf").contains("allocated_storage = 64"));
    assert!(file(&p, "aws", "monitoring.tf").contains("retention_in_days = 90"));

    let mut loaded = p.clone();
    assert!(cat.normalize_values(&mut loaded) >= 4);
    assert_eq!(loaded.nodes["db-app"].config["storage"], Value::Int(64));
    assert_eq!(loaded.nodes["log-app"].config["retention_days"], Value::Int(90));
    // The old AWS statistic moved to the portable field, except its old default.
    assert_eq!(
        loaded.nodes["alm-cpu"].config["statistic"],
        Value::Str("Maximum".into())
    );
    assert!(!loaded.nodes["alm-cpu"].provider_config["aws"].contains_key("statistic"));
    assert!(!loaded.nodes["alm-storage"].config.contains_key("statistic"));
    let mon = file(&p, "aws", "monitoring.tf");
    assert!(mon.contains("alarm_name = \"db-cpu\""), "{mon}");
    assert!(mon.contains("statistic = \"Maximum\""), "{mon}");
}

// ------------------------------------------------------------------ alarms

#[test]
fn alarms_carry_what_cloudwatch_publishes() {
    let p = example("managed-data.ttg.json");
    let m = file(&p, "aws", "monitoring.tf");
    // Target health: the target group and the load balancer.
    assert!(
        m.contains("metric_name = \"UnHealthyHostCount\" statistic = \"Maximum\" namespace = \"AWS/ApplicationELB\" dimensions = { TargetGroup = aws_lb_target_group.web_lb_tg.arn_suffix LoadBalancer = aws_lb.web_lb.arn_suffix }"),
        "{m}"
    );
    // 5xx as a rate, with metric math and none of the single-metric arguments.
    assert!(
        m.contains("expression = \"100 * FILL(errors, 0) / requests\""),
        "{m}"
    );
    assert!(m.contains("metric_name = \"RequestCount\""), "{m}");
    // p95 latency as an extended statistic, threshold half a second.
    assert!(
        m.contains("threshold = 0.5 alarm_description = \"p95 latency\""),
        "{m}"
    );
    assert!(
        m.contains("extended_statistic = \"p95\" metric_name = \"TargetResponseTime\""),
        "{m}"
    );
    // Free storage as a percentage of what the instance has.
    assert!(
        m.contains("expression = format(\"100 * free / (%d * 1073741824)\", aws_db_instance.app_db.allocated_storage)"),
        "{m}"
    );
    // The export passes its own conflict check (statistic vs metric_query and so on).
    let g = generate(&p, &Catalog::builtin(), "aws", Tool::OpenTofu).unwrap();
    assert!(g.diagnostics.iter().all(|d| d.code != Code::Conflict));
}

#[test]
fn a_custom_target_health_metric_gets_the_target_group_too() {
    let mut p = example("managed-data.ttg.json");
    set(&mut p, "alm-unhealthy", "metric", Value::Str("custom".into()));
    set_provider(
        &mut p,
        "alm-unhealthy",
        "aws",
        "metric_name",
        Value::Str("UnHealthyHostCount".into()),
    );
    set_provider(
        &mut p,
        "alm-unhealthy",
        "aws",
        "metric_namespace",
        Value::Str("AWS/ApplicationELB".into()),
    );
    let m = file(&p, "aws", "monitoring.tf");
    assert!(
        m.contains("TargetGroup = aws_lb_target_group.web_lb_tg.arn_suffix"),
        "{m}"
    );
}

#[test]
fn a_network_load_balancer_has_no_http_metrics() {
    let mut p = example("managed-data.ttg.json");
    set(&mut p, "lb-web", "protocol", Value::Str("tcp".into()));
    assert!(blocked(&p, "aws").contains("tcp (network) load balancer"));
    // Target health works on either, in the network namespace.
    for id in ["alm-5xx", "alm-latency"] {
        p.remove_entity(id);
    }
    let m = file(&p, "aws", "monitoring.tf");
    assert!(m.contains("namespace = \"AWS/NetworkELB\""), "{m}");
}

#[test]
fn azure_and_google_convert_the_threshold_to_their_units() {
    let p = example("managed-data.ttg.json");
    let az = file(&p, "azure", "monitoring.tf");
    // Less than 10 % free is more than 90 % used.
    assert!(az.contains("metric_name = \"storage_percent\""), "{az}");
    assert!(
        az.contains("operator = \"GreaterThan\" threshold = 100 - 10"),
        "{az}"
    );
    // A percentile has no Azure metric alert, so that alarm is left out with a warning.
    let g = generate(&p, &Catalog::builtin(), "azure", Tool::OpenTofu).unwrap();
    assert!(!g.files["monitoring.tf"].contains("p95-latency"));

    let gcp = file(&p, "gcp", "monitoring.tf");
    assert!(gcp.contains("threshold_value = 0.5 * 1000"), "{gcp}");
    assert!(
        gcp.contains("per_series_aligner = \"ALIGN_PERCENTILE_95\""),
        "{gcp}"
    );
    assert!(gcp.contains("threshold_value = 80 / 100"), "{gcp}");
    assert!(
        gcp.contains("comparison = \"COMPARISON_GT\" threshold_value = (100 - 10) / 100"),
        "{gcp}"
    );
}

// ------------------------------------------------------------------ logs and images

#[test]
fn log_retention_takes_each_providers_allowed_values() {
    let mut p = example("managed-data.ttg.json");
    assert!(file(&p, "aws", "monitoring.tf").contains("retention_in_days = 60"));
    set(&mut p, "log-app", "retention_days", Value::Int(45));
    assert!(blocked(&p, "aws").contains("CloudWatch Logs keeps logs for 1, 3, 5"));
    generate(&p, &Catalog::builtin(), "azure", Tool::OpenTofu).unwrap();
    set(&mut p, "log-app", "retention_days", Value::Int(14));
    assert!(blocked(&p, "azure").contains("30 to 730 days"));
    generate(&p, &Catalog::builtin(), "aws", Tool::OpenTofu).unwrap();
    set(&mut p, "log-app", "retention_days", Value::Int(4000));
    assert!(blocked(&p, "gcp").contains("1 to 3650 days"));
}

#[test]
fn image_lifecycles_expire_what_they_are_told() {
    let mut p = example("managed-data.ttg.json");
    let c = file(&p, "aws", "container.tf");
    assert!(c.contains("tagStatus = \"tagged\""), "{c}");
    assert!(c.contains("tagPrefixList = [\"v\"]"), "{c}");
    let gcp = file(&p, "gcp", "container.tf");
    assert!(
        gcp.contains("tag_state = \"TAGGED\" tag_prefixes = [ \"v\" ]"),
        "{gcp}"
    );

    // No prefixes: every tag, by pattern (ECR needs one or the other).
    set(&mut p, "reg-app", "expire_tag_prefixes", Value::List(vec![]));
    let c = file(&p, "aws", "container.tf");
    assert!(c.contains("tagPatternList = [\"*\"]"), "{c}");

    // The default expires untagged images only.
    p.nodes.get_mut("reg-app").unwrap().config.remove("expire_images");
    let c = file(&p, "aws", "container.tf");
    assert!(c.contains("tagStatus = \"untagged\""), "{c}");
    assert!(
        !c.contains("tagPrefixList") && !c.contains("tagPatternList"),
        "{c}"
    );
}

// ------------------------------------------------------------------ conflicts

/// The zipOS workaround: the managed password through `extra` while the mapping still
/// writes `password`. `validate` passes it; `plan` would not, and neither does export.
#[test]
fn arguments_the_provider_refuses_together_block_the_export() {
    let mut p = example("managed-data.ttg.json");
    p.nodes
        .get_mut("db-app")
        .unwrap()
        .provider_config
        .get_mut("aws")
        .unwrap()
        .remove("master_password");
    p.nodes
        .get_mut("db-app")
        .unwrap()
        .extra
        .entry("aws".into())
        .or_default()
        .entry("main".into())
        .or_default()
        .insert("manage_master_user_password".into(), serde_json::json!(true));
    let d = ttg_codegen::diagnostics::run(&p, &Catalog::builtin(), "aws");
    let c: Vec<_> = d.iter().filter(|d| d.code == Code::Conflict).collect();
    assert_eq!(c.len(), 1, "{d:?}");
    assert_eq!(c[0].entity.as_deref(), Some("db-app"));
    assert!(
        c[0].message
            .contains("aws_db_instance.app_db sets both `password` and `manage_master_user_password`"),
        "{}",
        c[0].message
    );
    assert!(blocked(&p, "aws").contains("refuses together"));

    // An extended statistic added by hand next to the preset's statistic.
    let mut q = example("managed-data.ttg.json");
    q.nodes
        .get_mut("alm-cpu")
        .unwrap()
        .extra
        .entry("aws".into())
        .or_default()
        .entry("main".into())
        .or_default()
        .insert("extended_statistic".into(), serde_json::json!("p99"));
    assert!(blocked(&q, "aws").contains("`statistic` and `extended_statistic`"));
}

/// Nothing the curated mappings produce for the examples trips the check.
#[test]
fn no_example_conflicts_with_itself() {
    let cat = Catalog::builtin();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if !path.to_string_lossy().ends_with(".ttg.json") {
            continue;
        }
        let p = ttg_core::project::load(&path).unwrap();
        let mut cat = cat.clone();
        cat.ensure_native_types(&p);
        for provider in cat.provider_ids() {
            let d = ttg_codegen::conflicts::check(&p, &cat, &provider);
            assert!(d.is_empty(), "{}/{provider}: {d:?}", path.display());
        }
    }
}

// ------------------------------------------------------------------ mapping language

#[test]
fn comparisons_units_and_moved_fields_are_checked_at_load() {
    let aws = include_str!("../../../definitions/providers/aws.toml");
    let def = |extra_fields: &str, when: &str| {
        format!(
            r#"
schema_version = 2
[resource]
type = "thing"
category = "network"
display_name = "Thing"
[[fields]]
name = "size"
type = "int"
[[fields]]
name = "label"
type = "string"
[[fields]]
name = "kind"
type = "enum"
options = ["a", "b"]
{extra_fields}
[providers.aws]
[[providers.aws.checks]]
when = {when}
message = "x"
[[providers.aws.blocks]]
key = "main"
resource = "aws_s3_bucket"
"#
        )
    };
    let load = |src: String| {
        Catalog::from_sources(
            [("thing.toml", src.as_str())].into_iter(),
            [("aws.toml", aws)].into_iter(),
        )
    };
    let reject = |src: String, needle: &str| {
        let err = load(src).expect_err("must be rejected");
        assert!(err.to_string().contains(needle), "{err}");
    };
    reject(
        def("", r#"{ field = "label", less_than = 3 }"#),
        "need an int or number field",
    );
    reject(
        def("", r#"{ field = "size", at_most = { field = "label" } }"#),
        "'label' is not an int or number field",
    );
    reject(
        def("", r#"{ field = "size", at_least = { field = "nope" } }"#),
        "undeclared field 'nope'",
    );
    reject(
        def("[[fields]]\nname = \"t\"\ntype = \"number\"\nunits = { field = \"label\", values = { a = \"x\" } }", r#"{ field = "size" }"#),
        "is not an enum field",
    );
    reject(
        def("[[fields]]\nname = \"t\"\ntype = \"number\"\nunits = { field = \"kind\", values = { c = \"x\" } }", r#"{ field = "size" }"#),
        "units for 'c'",
    );
    reject(
        def(
            "[[fields]]\nname = \"t\"\ntype = \"number\"\nmoved_from = { field = \"label\" }",
            r#"{ field = "size" }"#,
        ),
        "still declared",
    );
    // The valid shapes load.
    load(def(
        "[[fields]]\nname = \"t\"\ntype = \"number\"\nunits = { field = \"kind\", values = { a = \"s\" } }\nmoved_from = { provider = \"aws\", field = \"old\" }",
        r#"{ all = [ { field = "size", at_least = 1, at_most = { field = "t" } }, { field = "kind", not_one_of = ["a"] } ] }"#,
    ))
    .unwrap_or_else(|e| panic!("should load: {e}"));
}

// ------------------------------------------------------------------ cost

/// Free-form storage is priced as entered, and as Azure PostgreSQL rounds it.
#[test]
fn the_cost_estimate_prices_free_form_storage() {
    let p = example("managed-data.ttg.json");
    let cat = Catalog::builtin();
    let storage = |provider: &str| -> String {
        let e = ttg_codegen::cost::estimate(&p, &cat, provider, None).unwrap();
        let line = e.lines.iter().find(|l| l.entity == "db-app").unwrap();
        line.charges
            .iter()
            .map(|c| c.item.clone())
            .find(|i| i.contains("GB") && i.contains("storage"))
            .unwrap_or_else(|| panic!("{provider}: no storage charge in {:?}", line.charges))
    };
    assert!(storage("aws").starts_with("20 GB gp3"), "{}", storage("aws"));
    assert!(storage("azure").starts_with("32 GB"), "{}", storage("azure"));
    assert!(storage("gcp").starts_with("20 GB"), "{}", storage("gcp"));
}

/// Bringing saved values to their field types touches nothing but the values saved under
/// an older type: every other field of every example, numbers included (health-check
/// intervals, timeouts, rate limits), keeps exactly what was stored.
#[test]
fn normalizing_the_examples_only_touches_old_values() {
    let cat = Catalog::builtin();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let allowed = [
        ("relational_database", "storage"),
        ("log_group", "retention_days"),
        ("alarm", "statistic"),
    ];
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if !path.to_string_lossy().ends_with(".ttg.json") {
            continue;
        }
        let before = ttg_core::project::load(&path).unwrap();
        let mut after = before.clone();
        cat.normalize_values(&mut after);
        for (id, n) in &before.nodes {
            let m = &after.nodes[id];
            let keys: std::collections::BTreeSet<&String> = n.config.keys().chain(m.config.keys()).collect();
            for k in keys {
                if n.config.get(k) != m.config.get(k) {
                    assert!(
                        allowed.contains(&(n.resource_type.as_str(), k.as_str())),
                        "{}: {id}.{k} changed from {:?} to {:?}",
                        path.display(),
                        n.config.get(k),
                        m.config.get(k)
                    );
                }
            }
            for (pid, cfg) in &n.provider_config {
                let other = m.provider_config.get(pid).cloned().unwrap_or_default();
                for (k, v) in cfg {
                    if other.get(k) != Some(v) {
                        assert!(
                            n.resource_type == "alarm" && pid == "aws" && k == "statistic",
                            "{}: {id}.{pid}.{k} changed",
                            path.display()
                        );
                    }
                }
            }
        }
        for (id, c) in &before.containers {
            assert_eq!(c.config, after.containers[id].config, "{}: {id}", path.display());
        }
    }
}
