//! Named environments in the export: one root per provider whose differing values are
//! variables, a `.tfvars` and a backend configuration per environment, and a clear
//! refusal for what cannot be said that way.

use std::path::Path;
use ttg_catalog::Catalog;
use ttg_codegen::{generate, Code, GenError};
use ttg_core::{EnvOverride, Project, ProjectVariable, Tool, Value};

fn example(name: &str) -> Project {
    ttg_core::project::load(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../examples/{name}.ttg.json")),
    )
    .unwrap()
}

fn id_of(p: &Project, name: &str) -> String {
    p.entities()
        .iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("no entity {name}"))
        .id
        .to_string()
}

fn set(p: &mut Project, entity: &str, env: &str, field: &str, v: Value) {
    let id = id_of(p, entity);
    p.overrides_mut(&id)
        .unwrap()
        .entry(env.into())
        .or_default()
        .config
        .insert(field.into(), v);
}

/// `three-tier` with `pilot` and `prod`: the database highly available and the bucket
/// versioned only in prod.
fn three_tier_envs() -> Project {
    let mut p = example("three-tier");
    p.settings.environments = vec!["pilot".into(), "prod".into()];
    set(&mut p, "app db", "prod", "high_availability", Value::Bool(true));
    set(&mut p, "app db", "pilot", "high_availability", Value::Bool(false));
    set(&mut p, "app db", "prod", "backup_retention_days", Value::Int(30));
    p
}

#[test]
fn values_that_differ_become_variables_with_a_tfvars_per_environment() {
    let p = three_tier_envs();
    let cat = Catalog::builtin();
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap_or_else(|e| panic!("{e}"));
    let db = g
        .files
        .values()
        .find(|t| t.contains("resource \"aws_db_instance\""))
        .unwrap();
    assert!(
        db.contains("multi_az") && db.contains("var.app_db_high_availability"),
        "{db}"
    );
    assert!(db.contains("var.app_db_backup_retention_days"), "{db}");
    let vars = &g.files["variables.tf"];
    assert!(
        vars.contains("variable \"app_db_high_availability\"") && vars.contains("type        = bool"),
        "{vars}"
    );
    assert!(vars.contains("variable \"environment\""), "{vars}");
    let pilot = &g.files["environments/pilot.tfvars"];
    let prod = &g.files["environments/prod.tfvars"];
    assert!(
        pilot.contains("app_db_high_availability") && pilot.contains("= false"),
        "{pilot}"
    );
    assert!(
        prod.contains("= true") && prod.contains("= 30") && prod.contains("environment"),
        "{prod}"
    );
    // Each environment its own state; the partial backend leaves the path to it.
    assert!(g.files["environments/prod.backend.hcl"].contains("terraform.tfstate.d/prod/terraform.tfstate"));
    assert!(g.files["versions.tf"].contains("backend \"local\""));
    assert!(g.files["README.md"].contains("## Environments"));
    let names: Vec<&str> = g.lifted.iter().map(|l| l.name.as_str()).collect();
    assert!(names.contains(&"app_db_high_availability"), "{names:?}");
    // The same project without environments exports exactly as before.
    let mut plain = example("three-tier");
    plain.settings.environments.clear();
    let g0 = generate(&plain, &cat, "aws", Tool::OpenTofu).unwrap();
    assert!(!g0.files.keys().any(|k| k.starts_with("environments/")));
    assert!(g0.lifted.is_empty());
}

#[test]
fn a_nested_block_only_some_environments_have_becomes_a_dynamic_block() {
    let p = three_tier_envs();
    let cat = Catalog::builtin();
    let g = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap_or_else(|e| panic!("{e}"));
    let db = g
        .files
        .values()
        .find(|t| t.contains("azurerm_postgresql_flexible_server\" "))
        .unwrap();
    assert!(db.contains("dynamic \"high_availability\""), "{db}");
    assert!(db.contains("var.app_db_high_availability ? [1] : []"), "{db}");
}

#[test]
fn a_resource_one_environment_leaves_out_is_counted_and_read_through_one() {
    let mut p = three_tier_envs();
    let nat = id_of(&p, "nat");
    p.overrides_mut(&nat).unwrap().insert(
        "pilot".into(),
        EnvOverride {
            absent: true,
            ..Default::default()
        },
    );
    let cat = Catalog::builtin();
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap_or_else(|e| panic!("{e}"));
    let net = &g.files["network.tf"];
    assert!(
        net.contains("count") && net.contains("var.environment == \"prod\" ? 1 : 0"),
        "{net}"
    );
    assert!(net.contains("one(aws_nat_gateway.nat[*].id)"), "{net}");
}

#[test]
fn project_variables_and_the_name_prefix_are_written_as_variables() {
    let mut p = three_tier_envs();
    p.settings.name_prefix = Some("tt-${var.environment}".into());
    p.settings.variables.insert(
        "db_class".into(),
        ProjectVariable {
            value: Value::Str("db.t4g.small".into()),
            description: "RDS instance class".into(),
            environments: [("prod".to_string(), Value::Str("db.r6g.large".into()))].into(),
        },
    );
    let db = id_of(&p, "app db");
    p.nodes
        .get_mut(&db)
        .unwrap()
        .provider_config
        .entry("aws".into())
        .or_default()
        .insert("instance_class".into(), Value::Str("${var.db_class}".into()));
    let cat = Catalog::builtin();
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap_or_else(|e| panic!("{e}"));
    let all: String = g.files.values().cloned().collect();
    assert!(
        all.contains("instance_class") && all.contains("var.db_class"),
        "instance class"
    );
    assert!(all.contains("${var.name_prefix}"), "names built from the prefix");
    assert!(g.files["environments/prod.tfvars"].contains("tt-prod"));
    assert!(g.files["environments/prod.tfvars"].contains("db.r6g.large"));
}

/// An extra security-group rule in prod: on AWS each rule is a resource of its own, so
/// the extra one is counted in prod only; the rules every environment has stay as they are.
#[test]
fn an_extra_row_of_a_repeated_resource_is_counted() {
    let mut p = three_tier_envs();
    let sg = id_of(&p, "db sg");
    let Value::Records(mut rows) = p.nodes[&sg].config["rules"].clone() else {
        panic!("rules are rows")
    };
    let mut extra = rows[0].clone();
    extra.insert("name".into(), Value::Str("pgbouncer".into()));
    extra.insert("from_port".into(), Value::Int(6432));
    extra.insert("to_port".into(), Value::Int(6432));
    rows.push(extra);
    p.overrides_mut(&sg)
        .unwrap()
        .entry("prod".into())
        .or_default()
        .config
        .insert("rules".into(), Value::Records(rows));
    let cat = Catalog::builtin();
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap_or_else(|e| panic!("{e}"));
    // Repeated blocks are named by their row's key, so the extra rule has its own address.
    let addrs = &g.entity_blocks[&sg].addresses;
    let local = |key: &str| {
        let a = addrs
            .iter()
            .find(|a| a.contains(key))
            .unwrap_or_else(|| panic!("no {key} rule in {addrs:?}"));
        format!("\"{}\"", a.rsplit('.').next().unwrap())
    };
    let net = &g.files[&g.entity_blocks[&sg].file];
    let first_line_after = |label: &str| {
        net[net.find(label).unwrap()..]
            .lines()
            .nth(1)
            .unwrap()
            .to_string()
    };
    assert!(first_line_after(&local("pgbouncer")).contains("count"), "{net}");
    assert!(!first_line_after(&local("postgres")).contains("count"), "{net}");
}

/// A repeated nested block whose count differs (one more managed rule group in prod's
/// Web ACL) has no variable form: it is refused, naming the entity, the block and the
/// field behind it.
#[test]
fn what_variables_cannot_say_is_refused_by_name() {
    let mut p = example("edge");
    p.settings.environments = vec!["staging".into(), "prod".into()];
    set(
        &mut p,
        "edge waf",
        "prod",
        "managed_rules",
        Value::List(vec![
            "AWSManagedRulesCommonRuleSet".into(),
            "AWSManagedRulesKnownBadInputsRuleSet".into(),
            "AWSManagedRulesSQLiRuleSet".into(),
            "AWSManagedRulesAmazonIpReputationList".into(),
        ]),
    );
    let cat = Catalog::builtin();
    let Err(GenError::Blocked(ds)) = generate(&p, &cat, "aws", Tool::OpenTofu) else {
        panic!("a different number of rule blocks must be refused")
    };
    let env: Vec<_> = ds.iter().filter(|d| d.code == Code::Environment).collect();
    assert!(!env.is_empty(), "{ds:?}");
    let msg = &env[0].message;
    assert!(
        msg.contains("edge waf") && msg.contains("`rule` blocks") && msg.contains("managed_rules"),
        "{msg}"
    );
    // The WAF mode differing is fine: each action becomes a dynamic block.
    let mut p = example("edge");
    p.settings.environments = vec!["staging".into(), "prod".into()];
    set(&mut p, "edge waf", "staging", "mode", Value::Str("count".into()));
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap_or_else(|e| panic!("{e}"));
    let all: String = g.files.values().cloned().collect();
    assert!(
        all.contains("dynamic \"count\"") || all.contains("dynamic \"none\""),
        "mode"
    );
}

/// Diagnostics that hold in one environment only say which.
#[test]
fn a_check_that_fails_in_one_environment_names_it() {
    let mut p = three_tier_envs();
    set(
        &mut p,
        "app db",
        "prod",
        "storage",
        Value::Str("not-a-number".into()),
    );
    let cat = Catalog::builtin();
    let Err(GenError::Blocked(ds)) = generate(&p, &cat, "aws", Tool::OpenTofu) else {
        panic!("an invalid prod value blocks the export")
    };
    assert!(
        ds.iter().any(|d| d.message.starts_with("[prod] ")),
        "{:?}",
        ds.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let others =
        ttg_codegen::environments::other_environments(&p, &cat, "aws", Tool::OpenTofu, Some("pilot"));
    assert!(others.iter().any(|d| d.environment.as_deref() == Some("prod")));
}

/// A project variable in a numeric field resolves first and is brought to the field's
/// type after, so it stays a number even when a value was typed as text.
#[test]
fn a_variable_in_a_number_field_stays_a_number() {
    let mut p = three_tier_envs();
    p.settings.variables.insert(
        "db_storage".into(),
        ProjectVariable {
            value: Value::Str("50".into()),
            description: String::new(),
            environments: [("prod".to_string(), Value::Int(200))].into(),
        },
    );
    let db = id_of(&p, "app db");
    p.nodes
        .get_mut(&db)
        .unwrap()
        .config
        .insert("storage".into(), Value::Str("${var.db_storage}".into()));
    let cat = Catalog::builtin();
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap_or_else(|e| panic!("{e}"));
    let db_tf = g
        .files
        .values()
        .find(|t| t.contains("resource \"aws_db_instance\""))
        .unwrap();
    assert!(db_tf.contains("var.db_storage"), "{db_tf}");
    let pilot = &g.files["environments/pilot.tfvars"];
    assert!(
        pilot.contains("db_storage") && pilot.contains("= 50") && !pilot.contains("\"50\""),
        "{pilot}"
    );
    assert!(g.files["variables.tf"].contains("variable \"db_storage\""));
}

/// `containers` with `staging` (disposable) and `prod` sharing one account through
/// `shop-${var.environment}`: hand-set names in the registry's repositories, the
/// bucket's name and the secret's `extra` name, and the database classed by a project
/// variable with a connections alarm sized from it.
fn containers_envs() -> Project {
    let mut p = example("containers");
    p.settings.environments = vec!["staging".into(), "prod".into()];
    p.settings.disposable_environments.insert("staging".into(), true);
    p.settings.name_prefix = Some("shop-${var.environment}".into());
    p.settings.variables.insert(
        "db_class".into(),
        ProjectVariable {
            value: Value::Str("db.t4g.micro".into()),
            description: String::new(),
            environments: [("prod".to_string(), Value::Str("db.t4g.large".into()))].into(),
        },
    );
    p.nodes
        .get_mut("db-shop")
        .unwrap()
        .provider_config
        .entry("aws".into())
        .or_default()
        .insert("instance_class".into(), Value::Str("${var.db_class}".into()));
    p.nodes.get_mut("sec-app").unwrap().extra.insert(
        "aws".into(),
        serde_json::from_value(serde_json::json!({"main": {"name": "shop/app"}})).unwrap(),
    );
    let alarm: ttg_core::Node = serde_json::from_value(serde_json::json!({
        "id": "alm-conn", "name": "db connections", "resource_type": "alarm", "parent": "rg-shop",
        "config": {"metric": "db_connections_pct", "threshold": 75, "comparison": "greater_than", "period_seconds": "300", "evaluation_periods": 2}
    }))
    .unwrap();
    p.nodes.insert(alarm.id.clone(), alarm);
    p.add_edge("alm-conn", "db-shop", ttg_core::Relation::AttributeReference);
    p.add_edge("alm-conn", "topic-alerts", ttg_core::Relation::SendsTo);
    p
}

/// What the warnings say to write: every hand-set name behind the prefix.
fn follow_the_prefix(p: &mut Project) {
    let reg = p.nodes.get_mut("reg-shop").unwrap();
    let Value::List(repos) = reg.config["repositories"].clone() else {
        panic!("repositories are a list")
    };
    let repos = repos
        .iter()
        .map(|r| r.replacen("shop-", "${var.name_prefix}-", 1))
        .collect();
    reg.config.insert("repositories".into(), Value::List(repos));
    p.nodes
        .get_mut("obj-uploads")
        .unwrap()
        .provider_config
        .get_mut("aws")
        .unwrap()
        .insert(
            "bucket_name".into(),
            Value::Str("${var.name_prefix}-uploads-x7q2".into()),
        );
    p.nodes.get_mut("sec-app").unwrap().extra.insert(
        "aws".into(),
        serde_json::from_value(serde_json::json!({"main": {"name": "shop/${var.environment}/app"}})).unwrap(),
    );
}

fn shared_names(p: &Project, cat: &Catalog) -> Vec<String> {
    ttg_codegen::diagnostics::run(p, cat, "aws")
        .into_iter()
        .filter(|d| d.code == Code::Environment && d.message.contains("would collide"))
        .map(|d| d.message)
        .collect()
}

#[test]
fn hand_set_names_the_environments_would_share_are_reported() {
    let cat = Catalog::builtin();
    let mut p = containers_envs();
    let found = shared_names(&p, &cat);
    let about = |what: &str| {
        found
            .iter()
            .find(|m| m.contains(what))
            .unwrap_or_else(|| panic!("nothing about {what} in {found:?}"))
            .clone()
    };
    let repos = about("\"Repositories\" entries");
    assert!(
        repos.contains("\"shop-web\", \"shop-worker\", \"shop-migrate\"")
            && repos.contains("in staging and prod alike")
            && repos.contains("e.g. \"${var.name_prefix}-web\""),
        "{repos}"
    );
    assert!(
        about("\"Bucket name\" is \"shop-uploads-x7q2\"").contains("\"${var.name_prefix}-uploads-x7q2\""),
        "{found:?}"
    );
    assert!(about("extra argument `name` of aws_secretsmanager_secret").contains("\"shop/app\""));
    assert_eq!(found.len(), 3, "only names: {found:?}");
    // They are Warnings and do not hold up the export.
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap_or_else(|e| panic!("{e}"));
    assert!(g.diagnostics.iter().any(|d| d.message.contains("would collide")));
    // Behind the prefix, each environment has its own, and a repository keeps one
    // address in both.
    follow_the_prefix(&mut p);
    assert!(shared_names(&p, &cat).is_empty(), "{:?}", shared_names(&p, &cat));
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap_or_else(|e| panic!("{e}"));
    let all: String = g.files.values().cloned().collect();
    assert!(
        all.contains("resource \"aws_ecr_repository\" \"images_repo_web\" {\n  name"),
        "{all}"
    );
    assert!(all.contains("\"${var.name_prefix}-web\""), "{all}");
    let tf: String = g
        .files
        .iter()
        .filter(|(k, _)| k.ends_with(".tf"))
        .map(|(_, t)| t.as_str())
        .collect();
    assert!(!tf.contains("shop-prod") && !tf.contains("shop-staging"), "{tf}");
    // A prefix that is the same everywhere means one account per environment: nothing
    // collides.
    let mut p = containers_envs();
    p.settings.name_prefix = Some("shop".into());
    assert!(shared_names(&p, &cat).is_empty());
}

#[test]
fn a_name_only_one_environment_reads_follows_the_prefix() {
    let cat = Catalog::builtin();
    let mut p = containers_envs();
    follow_the_prefix(&mut p);
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap_or_else(|e| panic!("{e}"));
    let db = g
        .files
        .values()
        .find(|t| t.contains("resource \"aws_db_instance\""))
        .unwrap();
    // The snapshot suffix exists in prod only (staging is disposable), but its keeper
    // and the snapshot's name follow the environment rather than naming prod.
    assert!(db.contains("resource \"random_id\" \"shop_db_final_id\""), "{db}");
    // Counted on the environment: the class only prod's database has decides nothing.
    assert!(
        db.contains("count       = var.environment == \"prod\" ? 1 : 0"),
        "{db}"
    );
    assert!(db.contains("identifier = \"${var.name_prefix}-shop-db\""), "{db}");
    assert!(
        db.contains("format(\"%s-final-%s\", \"${var.name_prefix}-shop-db\""),
        "{db}"
    );
    assert!(!db.contains("shop-prod"), "{db}");
}

#[test]
fn a_connections_percentage_takes_each_environments_class() {
    let cat = Catalog::builtin();
    let mut p = containers_envs();
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap_or_else(|e| panic!("{e}"));
    let m = &g.files["monitoring.tf"];
    assert!(m.contains("= var.db_connections_threshold"), "{m}");
    // 75 % of 87 on a db.t4g.micro, of 856 on a db.t4g.large.
    let staging = &g.files["environments/staging.tfvars"];
    let prod = &g.files["environments/prod.tfvars"];
    assert!(
        staging.contains("db_connections_threshold") && staging.contains("= 65"),
        "{staging}"
    );
    assert!(prod.contains("= 642"), "{prod}");
    assert!(g.files["variables.tf"].contains("variable \"db_connections_threshold\""));
    // A class the table does not know is refused where it is used, and only there.
    p.settings
        .variables
        .get_mut("db_class")
        .unwrap()
        .environments
        .insert("prod".into(), Value::Str("db.x9.large".into()));
    let Err(GenError::Blocked(ds)) = generate(&p, &cat, "aws", Tool::OpenTofu) else {
        panic!("an unknown class in prod blocks the export")
    };
    let unknown: Vec<&String> = ds
        .iter()
        .map(|d| &d.message)
        .filter(|m| m.contains("not in the table of known limits"))
        .collect();
    assert!(
        unknown.len() == 1 && unknown[0].starts_with("[prod] "),
        "{unknown:?}"
    );
}
