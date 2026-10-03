//! References and export hygiene: native data sources, managed prefix lists on security
//! group rules, ports that `all` / `icmp` rules do without, `$ref` by key, `$raw`
//! addresses checked against the export and turned into graph links, key-addressed
//! repeated blocks with their `moved` blocks, and `null` removing a mapping argument.

use serde_json::json;
use std::path::Path;
use ttg_catalog::Catalog;
use ttg_codegen::refs::{derived_references, Via};
use ttg_codegen::{diagnostics, generate, Code, Severity};
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

fn messages(p: &Project, cat: &Catalog, provider: &str, sev: Severity) -> Vec<String> {
    diagnostics::run(p, cat, provider)
        .into_iter()
        .filter(|d| d.severity == sev)
        .map(|d| d.message)
        .collect()
}

const CF: &str = "com.amazonaws.global.cloudfront.origin-facing";

/// Three-tier with a native prefix-list data source and a native ingress rule using it.
fn with_native_prefix_list() -> Project {
    let mut p = example("three-tier.ttg.json");
    add(
        &mut p,
        json!({
            "id": "pl-cf", "name": "cf prefix", "parent": "vnet-5e6f7a8b",
            "resource_type": "native:aws:data.aws_ec2_managed_prefix_list",
            "extra": {"aws": {"main": {"name": CF}}}
        }),
    );
    add(
        &mut p,
        json!({
            "id": "rule-cf", "name": "https from cloudfront", "parent": "vnet-5e6f7a8b",
            "resource_type": "native:aws:aws_vpc_security_group_ingress_rule",
            "extra": {"aws": {"main": {
                "security_group_id": {"$ref": {"entity": "web sg", "attr": "id"}},
                "ip_protocol": "tcp", "from_port": 443, "to_port": 443,
                "prefix_list_id": {"$ref": {"entity": "cf prefix", "attr": "id"}}
            }}}
        }),
    );
    p
}

#[test]
fn a_native_data_source_is_a_data_block_and_a_ref_target() {
    let mut cat = Catalog::builtin();
    let p = with_native_prefix_list();
    cat.ensure_native_types(&p);
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let native = &g.files["native.tf"];
    assert!(
        native.contains("data \"aws_ec2_managed_prefix_list\" \"cf_prefix\" {"),
        "{native}"
    );
    assert!(squash(native).contains(&format!("name = \"{CF}\"")), "{native}");
    assert!(
        squash(native).contains("prefix_list_id = data.aws_ec2_managed_prefix_list.cf_prefix.id"),
        "{native}"
    );
    let pv = g.entity_preview(&p, &cat, "pl-cf");
    assert_eq!(pv.addresses, vec!["data.aws_ec2_managed_prefix_list.cf_prefix"]);

    // The rule's references are links in the graph.
    let refs = derived_references(&p, &cat, "aws");
    for target in ["pl-cf", "sg-2b3c4d5e"] {
        assert!(
            refs.iter()
                .any(|r| r.source == "rule-cf" && r.target == target && r.via == Via::Ref),
            "{refs:?}"
        );
    }

    // Its arguments are checked against the data source's schema.
    let mut bad = p.clone();
    bad.nodes
        .get_mut("pl-cf")
        .unwrap()
        .extra
        .get_mut("aws")
        .unwrap()
        .get_mut("main")
        .unwrap()
        .insert("bogus".into(), json!(1));
    let errs = messages(&bad, &cat, "aws", Severity::Error);
    assert!(
        errs.iter()
            .any(|m| m.contains("aws_ec2_managed_prefix_list has no argument 'bogus'")),
        "{errs:?}"
    );

    // Other providers leave AWS-native entities out.
    generate(&p, &cat, "azure", Tool::OpenTofu).unwrap();
}

/// Rule rows on `web sg`: replace its table.
fn set_rules(p: &mut Project, rows: serde_json::Value) {
    let v: Value = serde_json::from_value(rows).unwrap();
    p.nodes
        .get_mut("sg-2b3c4d5e")
        .unwrap()
        .config
        .insert("rules".into(), v);
}

#[test]
fn security_group_rows_admit_a_managed_prefix_list() {
    let mut cat = Catalog::builtin();
    let mut p = with_native_prefix_list();
    cat.ensure_native_types(&p);
    set_rules(
        &mut p,
        json!([
            {"name": "https-cf", "direction": "ingress", "protocol": "tcp", "from_port": 443, "to_port": 443, "prefix_list": CF},
            {"name": "http-cf", "direction": "ingress", "protocol": "tcp", "from_port": 80, "to_port": 80, "prefix_list": CF, "cidr": "0.0.0.0/0"},
            {"name": "canvas-list", "direction": "ingress", "protocol": "tcp", "from_port": 8443, "to_port": 8443, "source_prefix_list": "pl-cf"},
            {"name": "all-out", "direction": "egress", "protocol": "all", "cidr": "0.0.0.0/0"}
        ]),
    );
    let warnings = messages(&p, &cat, "aws", Severity::Warning);
    assert!(
        warnings
            .iter()
            .any(|m| m.contains("rule 'http-cf' names a prefix list and another source")),
        "{warnings:?}"
    );
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let net = squash(&g.files["network.tf"]);
    let lookup = "web_sg_prefix_list_com_amazonaws_global_cloudfront_origin_facing";
    assert_eq!(
        net.matches("data \"aws_ec2_managed_prefix_list\"").count(),
        1,
        "one lookup per distinct name: {net}"
    );
    assert!(
        net.contains(&format!(
            "data \"aws_ec2_managed_prefix_list\" \"{lookup}\" {{ name = \"{CF}\" }}"
        )),
        "{net}"
    );
    assert_eq!(
        net.matches(&format!(
            "prefix_list_id = data.aws_ec2_managed_prefix_list.{lookup}.id"
        ))
        .count(),
        2,
        "{net}"
    );
    assert!(
        net.contains("prefix_list_id = data.aws_ec2_managed_prefix_list.cf_prefix.id"),
        "{net}"
    );
    let http = net
        .split("resource \"aws_vpc_security_group_ingress_rule\" \"web_sg_ingress_http_cf\"")
        .nth(1)
        .unwrap()
        .split('}')
        .next()
        .unwrap()
        .to_string();
    assert!(!http.contains("cidr_ipv4"), "the prefix list wins: {http}");

    // Azure and Google Cloud have no prefix lists: those rows are left out, with a
    // warning, and a reference to an AWS-native entity is not "dangling" there.
    for provider in ["azure", "gcp"] {
        let errs = messages(&p, &cat, provider, Severity::Error);
        assert!(
            !errs.iter().any(|m| m.contains("no longer exists")),
            "{provider}: {errs:?}"
        );
        let warnings = messages(&p, &cat, provider, Severity::Warning);
        assert!(
            warnings.iter().any(|m| m.contains("prefix lists are AWS-only")),
            "{provider}: {warnings:?}"
        );
        let g = generate(&p, &cat, provider, Tool::OpenTofu).unwrap();
        let net = &g.files["network.tf"];
        let rule = if provider == "azure" {
            "resource \"azurerm_network_security_rule\" \"web_sg_rule_"
        } else {
            "resource \"google_compute_firewall\" \"web_sg_rule_"
        };
        assert_eq!(net.matches(rule).count(), 1, "{provider}: only all-out: {net}");
    }
}

/// A load balancer whose group admits only CloudFront says so in its exposure.
#[test]
fn a_load_balancer_locked_to_cloudfront_says_so() {
    let cat = Catalog::builtin();
    let mut p = example("three-tier.ttg.json");
    set_rules(
        &mut p,
        json!([
            {"name": "http-cf", "direction": "ingress", "protocol": "tcp", "from_port": 80, "to_port": 80, "prefix_list": CF},
            {"name": "all-out", "direction": "egress", "protocol": "all", "cidr": "0.0.0.0/0"}
        ]),
    );
    let r = ttg_codegen::reach::analyse(&p, &cat, "aws");
    let exposed = r.posture["lb-1e2f3a4b"].exposed.clone().unwrap_or_default();
    assert!(exposed.contains("admits only CloudFront"), "{exposed}");
}

#[test]
fn ports_are_optional_for_all_and_icmp() {
    let cat = Catalog::builtin();
    let mut p = example("three-tier.ttg.json");
    set_rules(
        &mut p,
        json!([
            {"name": "ping", "direction": "ingress", "protocol": "icmp", "cidr": "10.0.0.0/16"},
            {"name": "all-out", "direction": "egress", "protocol": "all", "cidr": "0.0.0.0/0"}
        ]),
    );
    let errs = messages(&p, &cat, "aws", Severity::Error);
    assert!(errs.is_empty(), "{errs:?}");
    let net = squash(&generate(&p, &cat, "aws", Tool::OpenTofu).unwrap().files["network.tf"]);
    assert!(
        net.contains("ip_protocol = \"icmp\" from_port = -1 to_port = -1"),
        "every ICMP type when the row gives none: {net}"
    );
    let az = squash(&generate(&p, &cat, "azure", Tool::OpenTofu).unwrap().files["network.tf"]);
    assert!(az.contains("destination_port_range = \"*\""), "{az}");

    // A tcp rule still needs them.
    set_rules(
        &mut p,
        json!([{"name": "web", "direction": "ingress", "protocol": "tcp", "cidr": "10.0.0.0/16"}]),
    );
    let errs = messages(&p, &cat, "aws", Severity::Error);
    assert!(errs.iter().any(|m| m.contains("From port: required")), "{errs:?}");
}

/// A container registry with three repositories and a native parameter that uses one.
fn with_registry(value: serde_json::Value) -> (Catalog, Project) {
    let mut cat = Catalog::builtin();
    let mut p = example("three-tier.ttg.json");
    add(
        &mut p,
        json!({
            "id": "reg-ecr", "name": "ecr", "parent": "rg-1a2b3c4d",
            "resource_type": "container_registry",
            "config": {"repositories": ["zipos-web", "zipos-worker", "zipos-migrate"]}
        }),
    );
    add(
        &mut p,
        json!({
            "id": "param-img", "name": "image param", "parent": "rg-1a2b3c4d",
            "resource_type": "native:aws:aws_ssm_parameter",
            "extra": {"aws": {"main": {"name": "/app/image", "type": "String", "value": value}}}
        }),
    );
    cat.ensure_native_types(&p);
    (cat, p)
}

#[test]
fn ref_by_key_picks_one_instance_of_a_repeated_block() {
    let (cat, p) = with_registry(
        json!({"$ref": {"entity": "ecr", "block": "repo", "key": "zipos-migrate", "attr": "repository_url"}}),
    );
    let native = squash(&generate(&p, &cat, "aws", Tool::OpenTofu).unwrap().files["native.tf"]);
    assert!(
        native.contains("value = aws_ecr_repository.ecr_repo_zipos_migrate.repository_url"),
        "{native}"
    );

    // Spliced into a `$raw` through `refs`.
    let (cat, p) = with_registry(json!({
        "$raw": "\"${@repo@}:release\"",
        "refs": {"repo": {"$ref": {"entity": "ecr", "block": "repo", "key": "zipos-web", "attr": "repository_url"}}}
    }));
    assert!(messages(&p, &cat, "aws", Severity::Error).is_empty());
    let native = squash(&generate(&p, &cat, "aws", Tool::OpenTofu).unwrap().files["native.tf"]);
    assert!(
        native.contains("value = \"${aws_ecr_repository.ecr_repo_zipos_web.repository_url}:release\""),
        "{native}"
    );

    // A key the block does not have, and a repeated block without one.
    let (cat, p) = with_registry(
        json!({"$ref": {"entity": "ecr", "block": "repo", "key": "nope", "attr": "repository_url"}}),
    );
    let errs = messages(&p, &cat, "aws", Severity::Error);
    assert!(
        errs.iter()
            .any(|m| m.contains("has no instance with key \"nope\"") && m.contains("zipos-web")),
        "{errs:?}"
    );
    let (cat, p) =
        with_registry(json!({"$ref": {"entity": "ecr", "block": "repo", "attr": "repository_url"}}));
    let warnings = messages(&p, &cat, "aws", Severity::Warning);
    assert!(
        warnings.iter().any(|m| m.contains("without a key")),
        "{warnings:?}"
    );
}

#[test]
fn repeated_blocks_are_named_by_key_and_moved_from_their_index() {
    let (cat, mut p) = with_registry(json!("x"));
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let c = &g.files["container.tf"];
    for name in [
        "ecr_repo_zipos_web",
        "ecr_repo_zipos_worker",
        "ecr_repo_zipos_migrate",
    ] {
        assert!(
            c.contains(&format!("resource \"aws_ecr_repository\" \"{name}\"")),
            "{c}"
        );
    }
    let moved = squash(&g.files["moved.tf"]);
    assert!(
        moved.contains(
            "moved { from = aws_ecr_repository.ecr_repo_2 to = aws_ecr_repository.ecr_repo_zipos_migrate }"
        ),
        "{moved}"
    );

    // Reordering the list changes no address.
    let addresses = |g: &ttg_codegen::Generated| {
        let mut a = g.entity_blocks["reg-ecr"].addresses.clone();
        a.sort();
        a
    };
    let before = addresses(&g);
    p.nodes.get_mut("reg-ecr").unwrap().config.insert(
        "repositories".into(),
        Value::List(vec![
            "zipos-migrate".into(),
            "zipos-web".into(),
            "zipos-worker".into(),
        ]),
    );
    let g2 = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    assert_eq!(before, addresses(&g2));

    // Rows whose keys collide are told apart in row order.
    p.nodes.get_mut("reg-ecr").unwrap().config.insert(
        "repositories".into(),
        Value::List(vec!["api".into(), "API".into()]),
    );
    let c = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap().files["container.tf"].clone();
    assert!(
        c.contains("\"ecr_repo_api\"") && c.contains("\"ecr_repo_api_2\""),
        "{c}"
    );
}

#[test]
fn a_raw_written_against_an_index_name_is_moved_and_warned_about() {
    let (cat, p) =
        with_registry(json!({"$raw": "\"${aws_ecr_repository.ecr_repo_2.repository_url}:release\""}));
    let warnings = messages(&p, &cat, "aws", Severity::Warning);
    assert!(
        warnings.iter().any(|m| m.contains(
            "aws_ecr_repository.ecr_repo_2, which is now aws_ecr_repository.ecr_repo_zipos_migrate"
        )),
        "{warnings:?}"
    );
    let native = squash(&generate(&p, &cat, "aws", Tool::OpenTofu).unwrap().files["native.tf"]);
    assert!(
        native.contains("${aws_ecr_repository.ecr_repo_zipos_migrate.repository_url}"),
        "{native}"
    );
}

#[test]
fn raw_addresses_are_checked_and_link_the_graph() {
    let mut cat = Catalog::builtin();
    let mut p = example("three-tier.ttg.json");
    add(
        &mut p,
        json!({"id": "log-orphan", "name": "orphan logs", "parent": "rg-1a2b3c4d",
               "resource_type": "log_group", "config": {"retention_days": "30"}}),
    );
    let unreferenced = |p: &Project, cat: &Catalog| {
        diagnostics::run(p, cat, "aws")
            .iter()
            .any(|d| d.code == Code::Unreferenced && d.entity.as_deref() == Some("log-orphan"))
    };
    assert!(unreferenced(&p, &cat));
    add(
        &mut p,
        json!({
            "id": "pol", "name": "bucket policy", "parent": "rg-1a2b3c4d",
            "resource_type": "native:aws:aws_iam_role_policy",
            "extra": {"aws": {"main": {
                "role": {"$ref": {"entity": "app role", "attr": "id"}},
                "policy": {"$raw": "jsonencode({ Statement = [{ Resource = [format(\"%s/*\", aws_s3_bucket.assets.arn), aws_cloudwatch_log_group.orphan_logs.arn], Region = var.region }] })"}
            }}}
        }),
    );
    cat.ensure_native_types(&p);
    let errs: Vec<String> = diagnostics::run(&p, &cat, "aws")
        .into_iter()
        .filter(|d| d.code == Code::Reference && d.severity == Severity::Error)
        .map(|d| d.message)
        .collect();
    assert!(errs.is_empty(), "{errs:?}");
    assert!(!unreferenced(&p, &cat), "a $raw address counts as a link");
    let refs = derived_references(&p, &cat, "aws");
    for target in ["obj-1c2d3e4f", "log-orphan", "role-5a6b7c8d"] {
        assert!(
            refs.iter().any(|r| r.source == "pol" && r.target == target),
            "{target}: {refs:?}"
        );
    }

    // Addresses the export does not generate are errors naming the $raw and the address.
    let set_policy = |p: &mut Project, raw: &str| {
        p.nodes
            .get_mut("pol")
            .unwrap()
            .extra
            .get_mut("aws")
            .unwrap()
            .get_mut("main")
            .unwrap()
            .insert("policy".into(), json!({"$raw": raw}));
    };
    set_policy(
        &mut p,
        "jsonencode([aws_secretsmanager_secret.nope.arn, local.x, var.nope, data.aws_vpc.gone.id, var.region])",
    );
    let errs = messages(&p, &cat, "aws", Severity::Error);
    for want in [
        "$raw in main.policy refers to aws_secretsmanager_secret.nope, which the Amazon Web Services export does not generate",
        "$raw in main.policy refers to local.x",
        "$raw in main.policy refers to var.nope",
        "$raw in main.policy refers to data.aws_vpc.gone",
    ] {
        assert!(errs.iter().any(|m| m.contains(want)), "missing {want:?} in {errs:?}");
    }
    assert!(!errs.iter().any(|m| m.contains("var.region")), "{errs:?}");
    set_policy(&mut p, "jsonencode({");
    let errs = messages(&p, &cat, "aws", Severity::Error);
    assert!(
        errs.iter().any(|m| m.contains("is not a valid HCL expression")),
        "{errs:?}"
    );

    // Flagging the bucket external leaves the address dangling, and the message says why.
    set_policy(&mut p, "aws_s3_bucket.assets.arn");
    p.nodes.get_mut("obj-1c2d3e4f").unwrap().manual = true;
    let errs = messages(&p, &cat, "aws", Severity::Error);
    assert!(
        errs.iter()
            .any(|m| m.contains("aws_s3_bucket.assets") && m.contains("flagged as external")),
        "{errs:?}"
    );
    assert!(generate(&p, &cat, "aws", Tool::OpenTofu).is_err());
}

#[test]
fn null_removes_what_the_mapping_sets() {
    let cat = Catalog::builtin();
    let mut p = example("three-tier.ttg.json");
    let set = |p: &mut Project, id: &str, block: &str, args: serde_json::Value| {
        let m = p.extra_args_mut(id, "aws", block).unwrap();
        m.clear();
        for (k, v) in args.as_object().unwrap() {
            m.insert(k.clone(), v.clone());
        }
    };
    set(
        &mut p,
        "db-3c4d5e6f",
        "main",
        json!({"manage_master_user_password": true, "password": null}),
    );
    let infos = messages(&p, &cat, "aws", Severity::Info);
    assert!(
        infos
            .iter()
            .any(|m| m.contains("password = null removes the argument")),
        "{infos:?}"
    );
    let db = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap().files["database.tf"].clone();
    assert!(
        !db.lines().any(|l| l.trim_start().starts_with("password ")),
        "{db}"
    );
    assert!(squash(&db).contains("manage_master_user_password = true"), "{db}");

    // Removing a required argument is an error.
    set(&mut p, "db-3c4d5e6f", "main", json!({"instance_class": null}));
    let errs = messages(&p, &cat, "aws", Severity::Error);
    assert!(
        errs.iter()
            .any(|m| m.contains("instance_class = null removes an argument aws_db_instance requires")),
        "{errs:?}"
    );

    // On a nested block the mapping emits, null removes the block.
    let cat = Catalog::builtin();
    let mut k = example("kubernetes.ttg.json");
    k.settings.kubernetes_manifests = false;
    let m = k.extra_args_mut("pool-gpu", "aws", "main").unwrap();
    m.insert("taint".into(), serde_json::Value::Null);
    let c = generate(&k, &cat, "aws", Tool::OpenTofu).unwrap().files["container.tf"].clone();
    let gpu = c.split("resource \"aws_eks_node_group\" \"gpu\"").nth(1).unwrap();
    let gpu = gpu.split("\nresource ").next().unwrap();
    assert!(!gpu.contains("taint {"), "{gpu}");
}

/// Across every example and provider: each `moved` block lands on an address the export
/// writes (an identical grant dropped after planning gets none), no two share a `from`
/// or a `to`, and `entity_blocks` lists exactly the resources and data sources written.
#[test]
fn moved_blocks_land_on_written_addresses_in_every_example() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let mut checked = 0;
    // Exports where an identical grant was planned but written once.
    let mut deduped = 0;
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if !path.to_string_lossy().ends_with(".ttg.json") {
            continue;
        }
        let p = ttg_core::project::load(&path).unwrap();
        let mut cat = Catalog::builtin();
        cat.ensure_native_types(&p);
        for provider in cat.provider_ids() {
            let Ok(g) = generate(&p, &cat, &provider, Tool::OpenTofu) else {
                continue;
            };
            checked += 1;
            let written: std::collections::HashSet<String> = g
                .entity_blocks
                .values()
                .flat_map(|b| b.addresses.iter().cloned())
                .collect();
            let declared = |text: &str, kind: &str| -> Vec<String> {
                text.lines()
                    .filter_map(|l| l.strip_prefix(&format!("{kind} \"")))
                    .map(|rest| {
                        let parts: Vec<&str> = rest.split('"').collect();
                        let addr = format!("{}.{}", parts[0], parts[2]);
                        if kind == "data" {
                            format!("data.{addr}")
                        } else {
                            addr
                        }
                    })
                    .collect()
            };
            let mut in_files = std::collections::HashSet::new();
            for (name, text) in &g.files {
                if name.ends_with(".tf") && !name.contains('/') {
                    in_files.extend(declared(text, "resource"));
                    in_files.extend(declared(text, "data"));
                }
            }
            let what = format!("{} / {provider}", path.display());
            assert_eq!(written, in_files, "{what}: entity_blocks and the files disagree");
            let layer = ttg_codegen::layers::project_for(&p, &cat, &provider);
            let plan = ttg_codegen::plan::Plan::build(&layer, &cat, &provider, None);
            if plan.addresses().keys().any(|a| !written.contains(a)) {
                deduped += 1;
            }
            let Some(moved) = g.files.get("moved.tf") else {
                continue;
            };
            let squashed = squash(moved);
            let mut froms = std::collections::HashSet::new();
            let mut tos = std::collections::HashSet::new();
            for block in squashed.split("moved {").skip(1) {
                let from = block
                    .split("from = ")
                    .nth(1)
                    .unwrap()
                    .split(' ')
                    .next()
                    .unwrap()
                    .to_string();
                let to = block
                    .split("to = ")
                    .nth(1)
                    .unwrap()
                    .split(' ')
                    .next()
                    .unwrap()
                    .to_string();
                assert!(
                    written.contains(&to),
                    "{what}: moved to {to}, which is not written"
                );
                assert!(
                    !written.contains(&from),
                    "{what}: moved from {from}, which is still written"
                );
                assert!(froms.insert(from.clone()), "{what}: two moves from {from}");
                assert!(tos.insert(to.clone()), "{what}: two moves to {to}");
            }
        }
    }
    assert!(checked > 20, "only {checked} exports checked");
    assert!(
        deduped > 0,
        "no example drops a duplicate grant, so that case went unchecked"
    );
}
