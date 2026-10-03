//! Container Environments, Apps and Jobs: one shared cluster, the app's configuration in
//! every provider's shape, execution and task roles kept apart, a load balancer in front
//! of an app, background workers and one-shot jobs — and the mapping-language features
//! they are written with.

use std::path::Path;
use ttg_catalog::Catalog;
use ttg_codegen::{generate, Code, Severity};
use ttg_core::{Project, Tool};

fn example_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name)
}

fn example(name: &str) -> Project {
    ttg_core::project::load(&example_path(name)).expect("example loads")
}

/// The example as JSON, edited, and read back as a project.
fn edited(name: &str, edit: impl FnOnce(&mut serde_json::Value)) -> Project {
    let text = std::fs::read_to_string(example_path(name)).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
    edit(&mut v);
    serde_json::from_value(v).unwrap()
}

fn count(s: &str, needle: &str) -> usize {
    s.matches(needle).count()
}

#[test]
fn aws_apps_share_the_environment_cluster_and_keep_their_roles_apart() {
    let cat = Catalog::builtin();
    let p = example("containers.ttg.json");
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let c = &g.files["container.tf"];

    // One cluster, the environment's, with Container Insights; every service runs in it.
    assert_eq!(count(c, "resource \"aws_ecs_cluster\""), 1, "{c}");
    assert!(
        c.contains("name  = \"containerInsights\"\n    value = \"enabled\""),
        "{c}"
    );
    assert_eq!(
        count(
            c,
            "= aws_ecs_cluster.apps.id
"
        ),
        2,
        "{c}"
    );
    assert!(
        c.contains("cluster         = aws_ecs_cluster.apps.name"),
        "the job's run configuration: {c}"
    );

    // The execution role starts the tasks, the task role is what the code runs as.
    assert!(
        c.contains("execution_role_arn    = aws_iam_role.exec_role.arn"),
        "{c}"
    );
    assert!(
        c.contains("task_role_arn         = aws_iam_role.web_role.arn"),
        "{c}"
    );
    let iam = &g.files["iam.tf"];
    assert_eq!(
        count(iam, "AmazonECSTaskExecutionRolePolicy"),
        1,
        "only the execution role gets the execution policy: {iam}"
    );
    assert!(iam.contains("role       = aws_iam_role.exec_role.name"), "{iam}");
    // ...and it may read the secrets it injects; the task role gets nothing for them.
    assert!(
        c.contains("resource \"aws_iam_role_policy\" \"web_exec_secrets\""),
        "{c}"
    );
    assert!(c.contains("role   = aws_iam_role.exec_role.id\n  policy = jsonencode({ Version = \"2012-10-17\", Statement = [{ Sid = \"InjectSecrets\""), "{c}");
    let web_policy = c
        .split("resource \"aws_iam_role_policy\" \"web_policy\"")
        .nth(1)
        .unwrap();
    assert!(!web_policy.contains("secretsmanager"), "{web_policy}");
    assert!(
        web_policy.contains("\"s3:DeleteObject\""),
        "delete_objects is on: {web_policy}"
    );
    assert!(web_policy.contains("rds-db:connect"), "{web_policy}");

    // The app's configuration in the container definition.
    assert!(
        c.contains(
            "valueFrom = format(\"%s:%s::\", aws_secretsmanager_secret.app_secret.arn, \"DATABASE_URL\")"
        ),
        "{c}"
    );
    assert!(
        c.contains("{ name = \"WORKER_MODE\", value = \"external\" }"),
        "{c}"
    );
    assert!(
        c.contains("{ name = \"DB_HOST\", value = aws_db_instance.shop_db.address }"),
        "{c}"
    );
    assert!(c.contains("stopTimeout = 120"), "{c}");
    assert!(
        c.contains("healthCheck = { command = [\"CMD-SHELL\", \"node -e"),
        "{c}"
    );
    assert!(c.contains("cpu_architecture        = \"ARM64\""), "{c}");
    // The image comes from the linked registry; an unset tag is an input variable.
    assert!(c.contains("\"shop-web\", \"1.4.2\")"), "{c}");
    assert!(c.contains("\"shop-worker\", var.worker_image_tag)"), "{c}");
    assert!(g.files["variables.tf"].contains("variable \"worker_image_tag\""));

    // The worker has no port mapping and no load balancer; web is behind one by IP.
    let worker = c
        .split("resource \"aws_ecs_task_definition\" \"worker_task\"")
        .nth(1)
        .unwrap();
    let worker = worker.split("resource \"aws_ecs_service\"").next().unwrap();
    assert!(!worker.contains("portMappings"), "{worker}");
    assert!(c.contains("load_balancer {\n    target_group_arn = aws_lb_target_group.shop_lb_tg.arn\n    container_name   = \"web\"\n    container_port   = 8080"), "{c}");
    assert!(
        c.contains("depends_on                        = [\n    aws_lb_listener.shop_lb_listener\n  ]"),
        "{c}"
    );
    let lb = &g.files["load_balancer.tf"];
    assert!(lb.contains("target_type          = \"ip\""), "{lb}");
    assert!(lb.contains("port                 = 8080"), "{lb}");
    assert!(lb.contains("deregistration_delay = 30"), "{lb}");
    assert!(lb.contains("path     = \"/api/health\""), "{lb}");
    assert!(
        !lb.contains("aws_lb_target_group_attachment"),
        "ECS registers the tasks: {lb}"
    );

    // The job is a task definition and the run configuration, nothing that runs.
    assert!(
        c.contains("resource \"aws_ecs_task_definition\" \"migrate\""),
        "{c}"
    );
    assert!(c.contains("resource \"terraform_data\" \"migrate_run\""), "{c}");
    assert!(g.files["outputs.tf"].contains("output \"migrate_run\""));

    // The alarm on the worker watches the cluster the worker actually runs in.
    let mon = &g.files["monitoring.tf"];
    assert!(mon.contains("ClusterName = aws_ecs_cluster.apps.name"), "{mon}");
    assert!(mon.contains("ServiceName = aws_ecs_service.worker.name"), "{mon}");
    assert!(g.diagnostics.iter().all(|d| d.severity != Severity::Error));
}

#[test]
fn azure_apps_share_the_environment_and_read_key_vault_secrets() {
    let cat = Catalog::builtin();
    let p = example("containers.ttg.json");
    let g = generate(&p, &cat, "azure", Tool::Terraform).unwrap();
    let c = &g.files["container.tf"];
    assert_eq!(
        count(c, "resource \"azurerm_container_app_environment\""),
        1,
        "{c}"
    );
    assert_eq!(
        count(
            c,
            "container_app_environment_id = azurerm_container_app_environment.apps.id"
        ),
        3,
        "{c}"
    );
    assert!(
        c.contains("resource \"azurerm_container_app_job\" \"migrate\""),
        "{c}"
    );
    assert!(c.contains("manual_trigger_config {"), "{c}");
    // Secret variables are Key Vault references read with the execution identity.
    assert!(c.contains("secret_name = \"database-url\""), "{c}");
    assert!(
        c.contains("key_vault_secret_id = azurerm_key_vault_secret.app_secret.versionless_id"),
        "{c}"
    );
    assert!(
        c.contains("identity            = azurerm_user_assigned_identity.exec_role.id"),
        "{c}"
    );
    assert!(
        c.contains("server   = azurerm_container_registry.images.login_server"),
        "{c}"
    );
    // Three apps share the execution identity: one grant each, not three, and every app
    // waits for the one kept.
    assert_eq!(count(c, "role_definition_name = \"AcrPull\""), 1, "{c}");
    assert_eq!(
        count(c, "role_definition_name = \"Key Vault Secrets User\""),
        1,
        "{c}"
    );
    assert_eq!(
        count(c, "    azurerm_role_assignment.migrate_acr_pull\n"),
        3,
        "{c}"
    );
    // The worker has no ingress; web's is external because an internet-facing load
    // balancer forwards to it.
    let worker = c
        .split("resource \"azurerm_container_app\" \"worker\"")
        .nth(1)
        .unwrap();
    let worker = worker
        .split("resource \"azurerm_role_assignment\"")
        .next()
        .unwrap();
    assert!(!worker.contains("ingress {"), "{worker}");
    assert!(
        worker.contains("path                    = \"/healthz\""),
        "{worker}"
    );
    assert!(
        worker.contains("termination_grace_period_seconds = 120"),
        "{worker}"
    );
    let web = c
        .split("resource \"azurerm_container_app\" \"web\"")
        .nth(1)
        .unwrap();
    assert!(web.contains("external_enabled = true"), "{web}");
    assert!(g.manual_steps.iter().any(|m| m
        .title
        .contains("Container Apps have a load balancer of their own")));
}

#[test]
fn gcp_worker_keeps_its_cpu_and_web_is_the_load_balancers_backend() {
    let cat = Catalog::builtin();
    let p = example("containers.ttg.json");
    let g = generate(&p, &cat, "gcp", Tool::OpenTofu).unwrap();
    let c = &g.files["container.tf"];
    let worker = c
        .split("resource \"google_cloud_run_v2_service\" \"worker\"")
        .nth(1)
        .unwrap();
    assert!(worker.contains("cpu_idle = false"), "{worker}");
    assert!(
        worker.contains("ingress             = \"INGRESS_TRAFFIC_INTERNAL_ONLY\""),
        "{worker}"
    );
    let web = c
        .split("resource \"google_cloud_run_v2_service\" \"web\"")
        .nth(1)
        .unwrap();
    assert!(web.contains("INGRESS_TRAFFIC_INTERNAL_LOAD_BALANCER"), "{web}");
    assert!(
        c.contains("secret_key_ref {\n              secret  = google_secret_manager_secret.app_secret.id"),
        "{c}"
    );
    // A job with no task role runs as its execution role.
    assert!(
        c.contains("resource \"google_cloud_run_v2_job\" \"migrate\""),
        "{c}"
    );
    assert!(
        c.contains("service_account = google_service_account.exec_role.email"),
        "{c}"
    );
    let lb = &g.files["load_balancer.tf"];
    assert!(lb.contains("network_endpoint_type = \"SERVERLESS\""), "{lb}");
    assert!(
        lb.contains("service = google_cloud_run_v2_service.web.name"),
        "{lb}"
    );
    assert!(
        lb.contains("group = google_compute_region_network_endpoint_group.shop_lb_neg.id"),
        "{lb}"
    );
    let gbs = lb
        .split("resource \"google_compute_backend_service\"")
        .nth(1)
        .unwrap();
    assert!(
        !gbs.contains("health_checks") && !gbs.contains("port_name"),
        "{gbs}"
    );
    assert!(!lb.contains("google_compute_health_check"), "{lb}");
}

/// An app drawn outside any environment keeps the cluster of its own it always had, and
/// a single role still does both jobs.
#[test]
fn an_app_outside_an_environment_keeps_its_own_cluster() {
    let cat = Catalog::builtin();
    let p = edited("containers.ttg.json", |v| {
        v["nodes"]["app-worker"]["parent"] = "vnet-shop".into();
        let edges = v["edges"].as_array_mut().unwrap();
        edges.retain(|e| !(e["source"] == "app-worker" && e["target"] == "role-exec"));
        v["nodes"]["role-worker"]["config"]["trusted_service"] = "container".into();
    });
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let c = &g.files["container.tf"];
    assert_eq!(count(c, "resource \"aws_ecs_cluster\""), 2, "{c}");
    assert!(
        c.contains("cluster         = aws_ecs_cluster.worker_cluster.id"),
        "{c}"
    );
    assert!(
        c.contains("execution_role_arn    = aws_iam_role.worker_role.arn"),
        "{c}"
    );
    let iam = &g.files["iam.tf"];
    assert_eq!(count(iam, "AmazonECSTaskExecutionRolePolicy"), 2, "{iam}");
    assert!(
        iam.contains("role       = aws_iam_role.worker_role.name"),
        "{iam}"
    );
    assert!(g.files["monitoring.tf"].contains("ClusterName = aws_ecs_cluster.worker_cluster.name"));
}

/// An `extra` that points a service at another cluster leaves its own cluster created
/// and used by nothing, which the diagnostics say.
#[test]
fn an_override_that_orphans_a_block_is_reported() {
    let cat = Catalog::builtin();
    let p = edited("containers.ttg.json", |v| {
        v["nodes"]["app-worker"]["parent"] = "vnet-shop".into();
        v["nodes"]["app-worker"]["extra"] = serde_json::json!({ "aws": { "main": { "cluster": "arn:aws:ecs:eu-west-2:123456789012:cluster/shared" } } });
    });
    let diags = ttg_codegen::diagnostics::run(&p, &cat, "aws");
    let orphan: Vec<_> = diags
        .iter()
        .filter(|d| d.code == Code::Extra && d.message.contains("used by nothing"))
        .collect();
    assert_eq!(orphan.len(), 1, "{diags:#?}");
    assert_eq!(orphan[0].entity.as_deref(), Some("app-worker"));
    assert!(
        orphan[0].message.contains("cluster (aws_ecs_cluster)"),
        "{}",
        orphan[0].message
    );
    // Inside the environment there is no cluster of its own to orphan.
    let p = edited("containers.ttg.json", |v| {
        v["nodes"]["app-worker"]["extra"] = serde_json::json!({ "aws": { "main": { "cluster": "arn:aws:ecs:eu-west-2:123456789012:cluster/shared" } } });
    });
    let diags = ttg_codegen::diagnostics::run(&p, &cat, "aws");
    assert!(
        !diags.iter().any(|d| d.message.contains("used by nothing")),
        "{diags:#?}"
    );
}

#[test]
fn secret_rows_must_name_a_linked_secret() {
    let cat = Catalog::builtin();
    let p = edited("containers.ttg.json", |v| {
        let edges = v["edges"].as_array_mut().unwrap();
        edges.retain(|e| !(e["source"] == "app-web" && e["relation"] == "reads"));
    });
    for provider in ["aws", "azure", "gcp"] {
        let diags = ttg_codegen::diagnostics::run(&p, &cat, provider);
        let errs: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error && d.entity.as_deref() == Some("app-web"))
            .map(|d| d.message.as_str())
            .collect();
        assert!(
            errs.iter()
                .any(|m| m.contains("DATABASE_URL has no Secret to read")),
            "{provider}: {errs:?}"
        );
        assert!(
            errs.iter()
                .any(|m| m.contains("PAYMENTS_KEY names a Secret the app is not linked to")),
            "{provider}: {errs:?}"
        );
    }
}

#[test]
fn reachability_names_the_link_the_type_offers() {
    let cat = Catalog::builtin();
    let p = example("containers.ttg.json");
    let reach = ttg_codegen::reach::analyse(&p, &cat, "aws");
    let paths = ttg_codegen::reach::paths_from(&p, &cat, &reach, "app-worker");
    let to_db = paths.iter().find(|x| x.target == "db-shop").unwrap();
    assert_eq!(to_db.status, ttg_codegen::reach::Status::Ok, "{to_db:?}");
    assert!(
        to_db
            .notes
            .iter()
            .any(|n| n == "no 'Uses' link from \"worker\", so it receives no host name or credentials"),
        "{to_db:?}"
    );
    let web = p.entity("app-web").unwrap();
    let worker = p.entity("app-worker").unwrap();
    assert_eq!(ttg_codegen::reach::listening_port(&web), Some(8080));
    assert_eq!(
        ttg_codegen::reach::listening_port(&worker),
        None,
        "a worker listens for nothing"
    );
}

#[test]
fn graviton_tasks_and_job_runs_are_priced() {
    let cat = Catalog::builtin();
    let p = example("containers.ttg.json");
    let est = ttg_codegen::cost::estimate(&p, &cat, "aws", Some("eu-west-2")).unwrap();
    let line = |id: &str| est.lines.iter().find(|l| l.entity == id).unwrap();
    let book = ttg_codegen::cost::PriceBook::for_provider("aws").unwrap();
    let arm = book.price("fargate", "vcpu_arm", "eu-west-2").unwrap().value;
    // web: 2 tasks x 0.5 vCPU, all month, at the Graviton rate.
    let web = line("app-web");
    let vcpu = web.charges.iter().find(|c| c.item.contains("vCPU")).unwrap();
    assert!((vcpu.monthly - 2.0 * 0.5 * 730.0 * arm).abs() < 0.02, "{web:?}");
    assert!(vcpu.item.contains("Graviton"), "{vcpu:?}");
    // migrate: 30 runs x 5 minutes x 0.25 vCPU by default.
    let job = line("job-migrate");
    let vcpu = job.charges.iter().find(|c| c.item.contains("vCPU")).unwrap();
    assert_eq!(vcpu.sku, "vcpu_arm");
    assert!((vcpu.quantity - 30.0 * 5.0 / 60.0 * 0.25).abs() < 1e-9, "{job:?}");
    assert!(job.assumptions.iter().any(|u| u.key == "job_runs"));
    for provider in ["azure", "gcp"] {
        let est = ttg_codegen::cost::estimate(&p, &cat, provider, None).unwrap();
        let job = est.lines.iter().find(|l| l.entity == "job-migrate").unwrap();
        assert!(job.monthly > 0.0, "{provider}: {job:?}");
    }
}

/// The definitions' new vocabulary is checked when the catalog loads.
#[test]
fn row_lists_connections_where_and_linked_are_checked_at_load() {
    let aws = include_str!("../../../definitions/providers/aws.toml");
    let other = r#"
schema_version = 2
[resource]
type = "other"
category = "network"
display_name = "Other"
[[fields]]
name = "size"
type = "string"
[providers.aws]
[[providers.aws.blocks]]
key = "main"
resource = "aws_other"
[providers.aws.connection]
HOST = { self_block = "main", attr = "host" }
"#;
    let thing = |extra_fields: &str, when: &str, arg: &str| {
        format!(
            r#"
schema_version = 2
[resource]
type = "thing"
category = "network"
display_name = "Thing"
[[fields]]
name = "rows"
type = "struct_list"
[[fields.items]]
name = "name"
type = "string"
[[fields.items]]
name = "target"
type = "entity_ref"
targets = ["other"]
{extra_fields}
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
    let load = |def: String| {
        ttg_catalog::Catalog::from_sources(
            [("thing.toml", def.as_str()), ("other.toml", other)].into_iter(),
            [("aws.toml", aws)].into_iter(),
        )
        .map(|_| ())
    };
    let ok = |arg: &str| load(thing("", "", arg));

    ok(r#"{ for_each_field = "rows", each = { object = { n = { item = "name" } } } }"#).expect("rows");
    let err = ok(r#"{ for_each_field = "rows", each = { item = "nope" } }"#).expect_err("unknown item");
    assert!(err.to_string().contains("row has no item 'nope'"), "{err}");
    let err = ok(r#"{ for_each_field = "size", each = { value = 1 } }"#).expect_err("not a table");
    assert!(err.to_string().contains("undeclared field 'size'"), "{err}");
    let err = ok(r#"{ item = "name" }"#).expect_err("item outside the rows");
    assert!(
        err.to_string().contains("only valid inside a for_each_field"),
        "{err}"
    );

    ok(r#"{ relation = "reads", target_type = "other", connection = "HOST" }"#).expect("connection");
    let err = ok(r#"{ relation = "reads", connection = "PORT" }"#).expect_err("undeclared key");
    assert!(
        err.to_string().contains("connection 'PORT' is not declared"),
        "{err}"
    );
    let err = ok(r#"{ relation = "reads", connection = "HOST", attr = "id" }"#).expect_err("both");
    assert!(err.to_string().contains("cannot be combined with attr"), "{err}");

    let when = |w: &str| load(thing("", &format!("when = {w}"), r#"{ value = 1 }"#));
    when(r#"{ relation = "reads", target_type = "other", where = { field = "size" } }"#).expect("where");
    let err = when(r#"{ relation = "reads", where = { field = "size" } }"#).expect_err("no target_type");
    assert!(err.to_string().contains("where: needs target_type"), "{err}");
    let err = when(r#"{ relation = "reads", target_type = "other", where = { field = "colour" } }"#)
        .expect_err("the other type's fields");
    assert!(err.to_string().contains("undeclared field 'colour'"), "{err}");

    let rows_when = |w: &str| {
        ok(&format!(
            r#"{{ for_each_field = "rows", when = {w}, each = {{ item = "name" }} }}"#
        ))
    };
    rows_when(r#"{ item = "target", linked = "reads", absent = true }"#).expect("linked");
    let err = rows_when(r#"{ item = "name", linked = "reads" }"#).expect_err("not an entity_ref");
    assert!(err.to_string().contains("is not an entity_ref"), "{err}");
    let err = rows_when(r#"{ item = "target", linked = "logs_to" }"#).expect_err("undeclared");
    assert!(err.to_string().contains("undeclared relation 'logs_to'"), "{err}");
}
