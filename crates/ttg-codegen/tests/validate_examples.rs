//! Runs `tofu validate` / `terraform validate` on every example project for every
//! provider that can be exported — every root of it, the `bootstrap/` root(s) included —
//! and on `hardened` with each state backend and state encryption on, which is what
//! produces the bootstrap roots, and on `three-tier` with environments that leave a
//! resource out, a name prefix and a project variable (counts, `one()`, templates and
//! `.tfvars`-driven variables on every provider), and on `containers` with two
//! environments in one account (`shared_account`), and on five projects in the modules
//! layout (`module_variants`). Every export must also already be
//! formatted: `fmt -check -recursive` passes with each installed binary. Skips (passes)
//! when neither binary is installed, so the ordinary test suite stays dependency-free; CI
//! installs OpenTofu so it always runs there. Set `TTG_REQUIRE_VALIDATE=1` to make a
//! missing binary a failure.

use std::path::{Path, PathBuf};
use ttg_catalog::Catalog;
use ttg_codegen::validate::{find_binary, fmt_check, run, Outcome};
use ttg_core::{BackendConfig, Project, Tool};

fn examples() -> Vec<std::path::PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".ttg.json"))
        .collect();
    v.sort();
    v
}

/// The export directory and every bootstrap root under it.
fn roots(out: &Path) -> Vec<PathBuf> {
    let mut v = vec![out.to_path_buf()];
    let mut stack = vec![out.join("bootstrap")];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        let mut has_tf = false;
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if p.is_dir() && !name.starts_with('.') {
                stack.push(p);
            } else if name.ends_with(".tf") {
                has_tf = true;
            }
        }
        if has_tf {
            v.push(d);
        }
    }
    v
}

/// `hardened` with each backend, encrypted with its Encryption Key: S3 for AWS, Azure
/// Blob Storage for Azure, GCS for Google Cloud, and an S3 backend for the Google Cloud
/// export (store and key on different clouds: two bootstrap roots).
fn state_variants() -> Vec<(String, Project)> {
    let base = ttg_core::project::load(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/hardened.ttg.json"),
    )
    .unwrap();
    let backend = |kind: &str, args: &[(&str, &str)]| BackendConfig {
        backend_type: kind.into(),
        args: args.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
    };
    let s3 = backend(
        "s3",
        &[("bucket", "hardened-tfstate-x7q2"), ("region", "eu-west-2")],
    );
    let cases = [
        ("state-s3", s3.clone()),
        (
            "state-azurerm",
            backend(
                "azurerm",
                &[
                    ("resource_group_name", "tfstate-rg"),
                    ("storage_account_name", "hardenedtfstatex7q2"),
                    ("container_name", "tfstate"),
                ],
            ),
        ),
        (
            "state-gcs",
            backend("gcs", &[("bucket", "hardened-tfstate-x7q2")]),
        ),
    ];
    cases
        .into_iter()
        .map(|(name, b)| {
            let mut p = base.clone();
            p.settings.backend = Some(b);
            p.settings.state_encryption = true;
            p.settings.state_encryption_key = Some("key-main".into());
            (name.to_string(), p)
        })
        .collect()
}

/// `three-tier` (which already has `pilot` and `prod`) with the rest of what environments
/// can say: the NAT gateway absent from pilot (counted, read through `one()`), a name
/// prefix built from the environment, a project variable for the instance class, and a
/// subnet whose zone is a letter that follows the region.
fn environment_variants() -> Vec<(String, Project)> {
    let mut p = ttg_core::project::load(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/three-tier.ttg.json"),
    )
    .unwrap();
    p.settings.name_prefix = Some("tt-${var.environment}".into());
    p.settings.variables.insert(
        "db_class".into(),
        ttg_core::ProjectVariable {
            value: ttg_core::Value::Str("db.t4g.small".into()),
            description: "RDS instance class".into(),
            environments: [("prod".to_string(), ttg_core::Value::Str("db.r6g.large".into()))].into(),
        },
    );
    for n in p.nodes.values_mut() {
        match n.name.as_str() {
            "nat" => {
                n.overrides.entry("pilot".into()).or_default().absent = true;
            }
            "app db" => {
                n.provider_config.entry("aws".into()).or_default().insert(
                    "instance_class".into(),
                    ttg_core::Value::Str("${var.db_class}".into()),
                );
            }
            "web a" => {
                n.provider_config
                    .entry("aws".into())
                    .or_default()
                    .insert("availability_zone".into(), ttg_core::Value::Str("a".into()));
            }
            _ => {}
        }
    }
    vec![("environments".to_string(), p), shared_account()]
}

/// `containers` with `staging` (disposable) and `prod` in one account: names set by hand
/// behind `${var.name_prefix}` (repositories that keep one address in both), the final
/// snapshot's suffix and name in prod only, and a connections alarm whose threshold
/// follows a project variable's instance class through the `.tfvars`.
fn shared_account() -> (String, Project) {
    use ttg_core::Value;
    let mut p = ttg_core::project::load(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/containers.ttg.json"),
    )
    .unwrap();
    p.settings.environments = vec!["staging".into(), "prod".into()];
    p.settings.disposable_environments.insert("staging".into(), true);
    p.settings.name_prefix = Some("shop-${var.environment}".into());
    p.settings.variables.insert(
        "db_class".into(),
        ttg_core::ProjectVariable {
            value: Value::Str("db.t4g.micro".into()),
            description: String::new(),
            environments: [("prod".to_string(), Value::Str("db.t4g.large".into()))].into(),
        },
    );
    fn aws<'a>(p: &'a mut Project, id: &str) -> &'a mut ttg_core::Config {
        p.nodes
            .get_mut(id)
            .unwrap()
            .provider_config
            .entry("aws".into())
            .or_default()
    }
    aws(&mut p, "db-shop").insert("instance_class".into(), Value::Str("${var.db_class}".into()));
    aws(&mut p, "obj-uploads").insert(
        "bucket_name".into(),
        Value::Str("${var.name_prefix}-uploads-x7q2".into()),
    );
    p.nodes.get_mut("reg-shop").unwrap().config.insert(
        "repositories".into(),
        Value::List(vec![
            "${var.name_prefix}-web".into(),
            "${var.name_prefix}-worker".into(),
        ]),
    );
    p.nodes.get_mut("sec-app").unwrap().extra.insert(
        "aws".into(),
        serde_json::from_value(serde_json::json!({"main": {"name": "shop/${var.environment}/app"}})).unwrap(),
    );
    let alarm: ttg_core::Node = serde_json::from_value(serde_json::json!({
        "id": "alm-conn", "name": "db connections", "resource_type": "alarm", "parent": "rg-shop",
        "config": {"metric": "db_connections_pct", "threshold": 75, "comparison": "greater_than", "period_seconds": "300", "evaluation_periods": 2}
    }))
    .unwrap();
    p.nodes.insert(alarm.id.clone(), alarm);
    p.add_edge("alm-conn", "db-shop", ttg_core::Relation::AttributeReference);
    p.add_edge("alm-conn", "topic-alerts", ttg_core::Relation::SendsTo);
    ("environments-shared-account".to_string(), p)
}

/// The modules layout: `three-tier` (environments' variables passed to the modules),
/// `kubernetes` with its manifests (the `k8s_*` outputs read through the modules),
/// `hardened` with an S3 backend and its key in the bootstrap root, `edge` with a view
/// whose group puts the CDN, its certificate and its Web ACL in one module (the
/// `us_east_1` alias passed in), and networks whose subnets depend on each other (through
/// inputs) and on one another one way (a module-level `depends_on`).
fn module_variants() -> Vec<(String, Project)> {
    let load = |name: &str| {
        ttg_core::project::load(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../examples/{name}.ttg.json")),
        )
        .unwrap()
    };
    let modules = |mut p: Project| {
        p.settings.layout = ttg_core::Layout::Modules;
        p
    };
    let mut out = vec![
        ("modules-three-tier".to_string(), modules(load("three-tier"))),
        ("modules-kubernetes".to_string(), {
            let mut p = modules(load("kubernetes"));
            p.settings.kubernetes_manifests = true;
            p
        }),
    ];
    let (_, hardened) = state_variants().remove(0);
    out.push(("modules-state-s3".into(), modules(hardened)));
    let mut edge = modules(load("edge"));
    let id = |p: &Project, name: &str| {
        p.entities()
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.id.to_string())
            .unwrap()
    };
    let mut v = ttg_core::View::new("Modules", Default::default());
    for (i, name) in ["assets cdn", "site cert", "edge waf"].iter().enumerate() {
        let at = ttg_core::Position {
            x: 5000 + 200 * i as i32,
            y: 5000,
        };
        v.layout.as_mut().unwrap().positions.insert(id(&edge, name), at);
    }
    v.groups.push(ttg_core::Group {
        id: "g-edge".into(),
        label: "edge".into(),
        position: ttg_core::Position { x: 4900, y: 4900 },
        size: ttg_core::Size { w: 800, h: 300 },
        color: None,
    });
    edge.views.push(v);
    edge.settings.module_view = Some("Modules".into());
    out.push(("modules-edge-groups".into(), edge));
    let crossed: Project = serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "name": "crossed",
        "settings": {"tool": "opentofu", "target_provider": "aws", "layout": "modules",
                     "provider_settings": {"aws": {"region": "eu-west-2"}}},
        "containers": {
            "alpha": {"id": "alpha", "name": "alpha", "container_type": "virtual_network", "config": {"cidr_block": "10.1.0.0/16"}},
            "beta": {"id": "beta", "name": "beta", "container_type": "virtual_network", "config": {"cidr_block": "10.2.0.0/16"}},
            "gamma": {"id": "gamma", "name": "gamma", "container_type": "virtual_network", "config": {"cidr_block": "10.3.0.0/16"}}
        },
        "nodes": {
            "a1": {"id": "a1", "name": "a1", "resource_type": "subnet", "parent": "alpha", "config": {"cidr_block": "10.1.1.0/24"}},
            "a2": {"id": "a2", "name": "a2", "resource_type": "subnet", "parent": "alpha", "config": {"cidr_block": "10.1.2.0/24"}},
            "b1": {"id": "b1", "name": "b1", "resource_type": "subnet", "parent": "beta", "config": {"cidr_block": "10.2.1.0/24"}},
            "b2": {"id": "b2", "name": "b2", "resource_type": "subnet", "parent": "beta", "config": {"cidr_block": "10.2.2.0/24"}},
            "c1": {"id": "c1", "name": "c1", "resource_type": "subnet", "parent": "gamma", "config": {"cidr_block": "10.3.1.0/24"}}
        },
        "edges": [
            {"source": "a1", "target": "b1", "relation": "depends_on"},
            {"source": "b2", "target": "a2", "relation": "depends_on"},
            {"source": "c1", "target": "a1", "relation": "depends_on"}
        ]
    }))
    .unwrap();
    out.push(("modules-crossed".into(), crossed));
    out
}

#[test]
fn every_example_validates() {
    // Prefer OpenTofu; fall back to Terraform. Either validates both flavours of output
    // because the tool profile only changes registry addresses and version floors.
    let runner = [Tool::OpenTofu, Tool::Terraform]
        .into_iter()
        .find(|t| find_binary(*t).is_some());
    let Some(runner) = runner else {
        if std::env::var("TTG_REQUIRE_VALIDATE").is_ok() {
            panic!("TTG_REQUIRE_VALIDATE is set but neither tofu nor terraform is installed");
        }
        eprintln!("skipping: no tofu/terraform binary found");
        return;
    };
    // `fmt` is quick, so every installed binary checks the formatting.
    let formatters: Vec<Tool> = Tool::ALL
        .into_iter()
        .filter(|t| find_binary(*t).is_some())
        .collect();
    let mut formatted = 0;
    let mut cat = Catalog::builtin();
    let root = std::env::temp_dir().join(format!("ttg-validate-{}", std::process::id()));
    let mut ran = 0;
    let mut failures = Vec::new();
    let mut projects: Vec<(String, Project)> = examples()
        .into_iter()
        .map(|example| {
            let stem = example
                .file_name()
                .unwrap()
                .to_string_lossy()
                .replace(".ttg.json", "");
            (stem, ttg_core::project::load(&example).unwrap())
        })
        .collect();
    projects.extend(state_variants());
    projects.extend(environment_variants());
    projects.extend(module_variants());
    let mut modules_validated = 0;
    let mut bootstraps = 0;
    for (stem, project) in &projects {
        cat.ensure_native_types(project);
        for provider in cat.provider_ids() {
            for tool in Tool::ALL {
                let out = root.join(stem).join(&provider).join(tool.binary_name());
                // Providers the example is not configured for are expected to be blocked.
                let rep = match ttg_codegen::export(project, &cat, &provider, tool, &out) {
                    Ok(r) => r,
                    Err(ttg_codegen::GenError::Blocked(_)) => continue,
                    Err(e) => panic!("{stem}/{provider}: {e}"),
                };
                for fmt_tool in &formatters {
                    formatted += 1;
                    if let Some(Err(diff)) = fmt_check(&rep.out_dir, *fmt_tool) {
                        failures.push(format!(
                            "{stem}/{provider}/{}: `{} fmt -check -recursive` wants changes:
{diff}",
                            tool.display_name(),
                            fmt_tool.binary_name()
                        ));
                    }
                }
                if rep.out_dir.join("modules").is_dir() {
                    modules_validated += 1;
                }
                for dir in roots(&rep.out_dir) {
                    ran += 1;
                    if dir != rep.out_dir {
                        bootstraps += 1;
                    }
                    match run(&dir, runner) {
                        Outcome::Ran { success: true, .. } => {}
                        other => failures.push(format!(
                            "{stem}/{provider}/{} ({}): {}",
                            tool.display_name(),
                            dir.strip_prefix(&rep.out_dir).unwrap().display(),
                            other.summary()
                        )),
                    }
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&root);
    assert!(ran > 0, "no example produced an exportable configuration");
    assert!(formatted > 0, "no export was checked with fmt");
    // Five projects in the modules layout, both flavours, on every provider that takes them.
    assert!(
        modules_validated >= 20,
        "only {modules_validated} exports in the modules layout were validated"
    );
    // Three backends x both tools, plus the Google Cloud export of the S3 variant (store
    // and key roots) and the Terraform flavours (store only): well over a dozen roots.
    assert!(
        bootstraps >= 12,
        "only {bootstraps} bootstrap roots were validated"
    );
    assert!(
        failures.is_empty(),
        "validate failures:\n{}",
        failures.join("\n\n")
    );
}

trait BinaryName {
    fn binary_name(&self) -> &'static str;
}
impl BinaryName for Tool {
    fn binary_name(&self) -> &'static str {
        match self {
            Tool::Terraform => "terraform",
            Tool::OpenTofu => "opentofu",
        }
    }
}
