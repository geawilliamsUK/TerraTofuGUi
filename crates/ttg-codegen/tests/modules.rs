//! The modules layout: a thin root calling `modules/<name>/`, references crossing as
//! module inputs and outputs, provider aliases passed explicitly, `moved` blocks from
//! the flat addresses, environments' variables passed through, and the address map.

use serde_json::json;
use std::path::Path;
use ttg_catalog::Catalog;
use ttg_codegen::plan_run::Attribution;
use ttg_codegen::{export, generate, Generated};
use ttg_core::{Group, Layout, Position, Project, Size, Tool, View, ViewFilter};

fn example(name: &str) -> Project {
    ttg_core::project::load(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../examples/{name}.ttg.json")),
    )
    .unwrap()
}

fn modules(mut p: Project) -> Project {
    p.settings.layout = Layout::Modules;
    p
}

fn id_of(p: &Project, name: &str) -> String {
    p.entities()
        .iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("no entity {name}"))
        .id
        .to_string()
}

/// No file under `modules/` reaches into another module or the root directly.
fn assert_modules_self_contained(g: &Generated) {
    for (name, text) in g.files.iter().filter(|(n, _)| n.starts_with("modules/")) {
        assert!(!text.contains("module."), "{name} refers to a module:\n{text}");
    }
}

#[test]
fn top_level_containers_become_modules_behind_a_thin_root() {
    let cat = Catalog::builtin();
    let p = modules(example("three-tier"));
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap_or_else(|e| panic!("{e}"));
    let main = &g.files["main.tf"];
    assert!(main.contains("module \"main\" {"), "{main}");
    assert!(
        main.contains("source = \"./modules/main\"") || main.contains("\"./modules/main\""),
        "{main}"
    );
    // The VPC and everything in it moved; the IAM role and the bucket stay in the root.
    assert!(g.files["modules/main/network.tf"].contains("resource \"aws_vpc\" \"main\""));
    assert!(!g.files.contains_key("network.tf"));
    assert!(g.files["iam.tf"].contains("resource \"aws_iam_role\" \"app_role\""));
    for f in ["variables.tf", "outputs.tf", "versions.tf"] {
        assert!(
            g.files.contains_key(&format!("modules/main/{f}")),
            "modules/main/{f}"
        );
    }
    // A reference from the module to a root resource is an input the root fills in.
    let vars = &g.files["modules/main/variables.tf"];
    assert!(vars.contains("variable \"app_role_profile_name\""), "{vars}");
    assert!(
        main.contains("app_role_profile_name = aws_iam_instance_profile.app_role_profile.name")
            || main.contains("= aws_iam_instance_profile.app_role_profile.name"),
        "{main}"
    );
    assert!(g.files["modules/main/compute.tf"].contains("var.app_role_profile_name"));
    // The root's outputs read the module's.
    let outs = &g.files["outputs.tf"];
    assert!(outs.contains("value       = module.main.main_id"), "{outs}");
    assert!(g.files["modules/main/outputs.tf"].contains("output \"main_id\""));
    // The environments' variables live in the root and are passed on under their names.
    assert!(vars.contains("variable \"app_db_high_availability\""), "{vars}");
    assert!(
        !vars.contains("default"),
        "module inputs take the root's values: {vars}"
    );
    assert!(main.contains("= var.app_db_high_availability"), "{main}");
    assert!(g.files.contains_key("environments/pilot.tfvars"));
    assert!(g.files["variables.tf"].contains("variable \"app_db_high_availability\""));
    // A state applied flat follows the resources into the module.
    let moved = &g.files["moved.tf"];
    assert!(
        moved.contains("moved {\n  from = aws_vpc.main\n  to   = module.main.aws_vpc.main\n}"),
        "{moved}"
    );
    assert!(
        !moved.contains("from = data."),
        "data sources hold no state: {moved}"
    );
    assert!(g.files["README.md"].contains("## Modules"));
    assert_modules_self_contained(&g);
}

#[test]
fn the_tfvars_and_variables_are_the_same_in_both_layouts() {
    let cat = Catalog::builtin();
    let flat = generate(&example("three-tier"), &cat, "aws", Tool::OpenTofu).unwrap();
    let g = generate(&modules(example("three-tier")), &cat, "aws", Tool::OpenTofu).unwrap();
    for f in [
        "variables.tf",
        "environments/pilot.tfvars",
        "environments/prod.tfvars",
        "versions.tf",
    ] {
        assert_eq!(flat.files[f], g.files[f], "{f}");
    }
}

#[test]
fn addresses_entity_preview_and_attribution_follow_the_module() {
    let cat = Catalog::builtin();
    let p = modules(example("three-tier"));
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let vpc = id_of(&p, "main");
    let role = id_of(&p, "app role");
    let map = g.address_map();
    assert_eq!(map.get("module.main.aws_vpc.main"), Some(&vpc));
    assert_eq!(map.get("aws_iam_role.app_role"), Some(&role));
    let b = &g.entity_blocks[&vpc];
    assert_eq!(b.module.as_deref(), Some("main"));
    assert_eq!(b.file, "modules/main/network.tf");
    let text = &g.files[&b.file];
    let first = text.lines().nth(b.lines.0 - 1).unwrap();
    assert!(first.starts_with("# --- "), "{first}");
    assert_eq!(g.entity_at("modules/main/network.tf", b.lines.0 + 1), Some(&vpc));
    let preview = g.entity_preview(&p, &cat, &vpc);
    assert_eq!(preview.addresses, vec!["module.main.aws_vpc.main".to_string()]);
    assert!(preview.hcl.contains("resource \"aws_vpc\" \"main\""));
    // The inspector's list says the same before anything is generated.
    assert_eq!(
        ttg_codegen::emit::addresses_for(&p, &cat, "aws", &vpc),
        vec!["module.main.aws_vpc.main".to_string()]
    );
    // A plan's addresses, instance keys and all, find their entity.
    let a = Attribution::of(&g, &p, &cat);
    let nat = id_of(&p, "nat");
    assert_eq!(
        a.entity_of_address("module.main.aws_nat_gateway.nat[0]"),
        Some(&nat)
    );
    assert_eq!(a.entity_of_address("module.main.aws_vpc.main"), Some(&vpc));
    assert_eq!(a.entity_of_address("aws_iam_role.app_role"), Some(&role));
    // A flat address (a state from before the switch) still names its entity.
    assert_eq!(a.entity_of_address("aws_vpc.main"), Some(&vpc));
}

/// `edge` with a saved view whose groups say what the modules are: the CDN, its
/// certificate and its Web ACL in `edge` (both CloudFront-scoped, so in us-east-1).
fn edge_with_view_groups() -> Project {
    let mut p = modules(example("edge"));
    let mut v = View::new("Modules", ViewFilter::default());
    let layout = v.layout.as_mut().unwrap();
    for (i, name) in ["assets cdn", "site cert", "edge waf"].iter().enumerate() {
        layout.positions.insert(
            id_of(&p, name),
            Position {
                x: 5000 + 200 * i as i32,
                y: 5000,
            },
        );
    }
    v.groups.push(Group {
        id: "g-edge".into(),
        label: "edge".into(),
        position: Position { x: 4900, y: 4900 },
        size: Size { w: 800, h: 300 },
        color: None,
    });
    p.views.push(v);
    p.settings.module_view = Some("Modules".into());
    p
}

#[test]
fn view_groups_become_modules_and_aliases_are_passed_explicitly() {
    let cat = Catalog::builtin();
    let p = edge_with_view_groups();
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap_or_else(|e| panic!("{e}"));
    let main = &g.files["main.tf"];
    assert!(main.contains("module \"edge\" {"), "{main}");
    assert!(main.contains("aws.us_east_1 = aws.us_east_1"), "{main}");
    assert!(main.contains("providers = {"), "{main}");
    let versions = &g.files["modules/edge/versions.tf"];
    assert!(
        versions.contains("configuration_aliases = [\n        aws.us_east_1\n      ]"),
        "{versions}"
    );
    let edge_files: String = g
        .files
        .iter()
        .filter(|(n, _)| n.starts_with("modules/edge/"))
        .map(|(_, t)| t.as_str())
        .collect();
    assert!(edge_files.contains("resource \"aws_cloudfront_distribution\" \"assets_cdn\""));
    assert!(edge_files.contains("provider = aws.us_east_1") || edge_files.contains("aws.us_east_1"));
    // The containers that hold no group member still make modules of their own.
    assert!(main.contains("module \"edge_net\""), "{main}");
    // The root keeps the aliased provider configuration the module is given.
    assert!(
        g.files["providers.tf"].contains("alias  = \"us_east_1\"")
            || g.files["providers.tf"].contains("\"us_east_1\"")
    );
    assert_modules_self_contained(&g);
}

#[test]
fn a_container_counts_where_its_own_box_is_and_takes_its_ungrouped_members() {
    let cat = Catalog::builtin();
    let mut p = modules(example("three-tier"));
    let vpc = id_of(&p, "main");
    let mut v = View::new("Modules", ViewFilter::default());
    let layout = v.layout.as_mut().unwrap();
    layout
        .positions
        .insert(vpc.clone(), Position { x: 9000, y: 9000 });
    layout.sizes.insert(vpc.clone(), Size { w: 200, h: 100 });
    v.groups.push(Group {
        id: "g".into(),
        label: "network".into(),
        position: Position { x: 8900, y: 8900 },
        size: Size { w: 600, h: 400 },
        color: None,
    });
    p.views.push(v);
    p.settings.module_view = Some("Modules".into());
    let a = ttg_codegen::modules::assign(&p, &cat, "aws").unwrap();
    assert_eq!(a.get(&vpc).map(String::as_str), Some("network"));
    assert_eq!(a.get(&id_of(&p, "web a")).map(String::as_str), Some("network"));
    assert_eq!(a.get(&id_of(&p, "app role")), None);
}

#[test]
fn a_view_that_does_not_exist_is_refused() {
    let cat = Catalog::builtin();
    let mut p = modules(example("edge"));
    p.settings.module_view = Some("Nope".into());
    let e = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap_err().to_string();
    assert!(e.contains("\"Nope\""), "{e}");
}

/// Three networks with subnets: `a1` depends on `b1` and `b2` on `a2` (alpha and beta
/// wait for each other), and `c1` on `a1` (gamma waits for alpha, never the other way).
fn crossed_depends() -> Project {
    let net = |id: &str, cidr: &str, x: i32| {
        json!({"id": id, "name": id, "container_type": "virtual_network",
               "config": {"cidr_block": cidr}, "position": {"x": x, "y": 0}, "size": {"w": 400, "h": 300}})
    };
    let sub = |id: &str, parent: &str, cidr: &str| {
        json!({"id": id, "name": id, "resource_type": "subnet", "parent": parent,
               "config": {"cidr_block": cidr}, "position": {"x": 0, "y": 0}})
    };
    let v = json!({
        "schema_version": 1,
        "name": "crossed",
        "settings": {"tool": "opentofu", "target_provider": "aws", "layout": "modules",
                     "provider_settings": {"aws": {"region": "eu-west-2"}}},
        "containers": {"alpha": net("alpha", "10.1.0.0/16", 0), "beta": net("beta", "10.2.0.0/16", 600),
                       "gamma": net("gamma", "10.3.0.0/16", 1200)},
        "nodes": {
            "a1": sub("a1", "alpha", "10.1.1.0/24"), "a2": sub("a2", "alpha", "10.1.2.0/24"),
            "b1": sub("b1", "beta", "10.2.1.0/24"), "b2": sub("b2", "beta", "10.2.2.0/24"),
            "c1": sub("c1", "gamma", "10.3.1.0/24")
        },
        "edges": [
            {"source": "a1", "target": "b1", "relation": "depends_on"},
            {"source": "b2", "target": "a2", "relation": "depends_on"},
            {"source": "c1", "target": "a1", "relation": "depends_on"}
        ]
    });
    serde_json::from_value(v).unwrap()
}

#[test]
fn depends_on_across_modules_is_module_level_unless_that_would_close_a_cycle() {
    let cat = Catalog::builtin();
    let p = crossed_depends();
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap_or_else(|e| panic!("{e}"));
    let main = &g.files["main.tf"];
    // Gamma waits for alpha as a whole; nothing in alpha can come to wait for gamma.
    assert_eq!(main.matches("depends_on").count(), 1, "{main}");
    let gamma = &main[main.find("module \"gamma\"").unwrap()..];
    assert!(gamma.contains("depends_on = [\n    module.alpha\n  ]"), "{main}");
    // Alpha and beta each wait for a subnet of the other: module-level, either way round,
    // would make each wait for the other as a whole, so those resources depend on inputs
    // carrying the other subnet's id.
    let a = &g.files["modules/alpha/network.tf"];
    let b = &g.files["modules/beta/network.tf"];
    assert!(a.contains("depends_on = [\n    var.b1_id\n  ]"), "{a}");
    assert!(b.contains("depends_on = [\n    var.a2_id\n  ]"), "{b}");
    assert!(main.contains("= module.beta.b1_id"), "{main}");
    assert_modules_self_contained(&g);
}

#[test]
fn switching_layouts_replaces_the_files_the_other_layout_wrote() {
    let cat = Catalog::builtin();
    let dir = std::env::temp_dir().join(format!("ttg-modules-switch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let flat = example("three-tier");
    export(&flat, &cat, "aws", Tool::OpenTofu, &dir).unwrap();
    assert!(dir.join("network.tf").exists());
    std::fs::write(dir.join("terraform.tfstate"), "{}").unwrap();
    export(&modules(flat.clone()), &cat, "aws", Tool::OpenTofu, &dir).unwrap();
    assert!(
        !dir.join("network.tf").exists(),
        "the flat resource file is stale"
    );
    assert!(dir.join("modules/main/network.tf").exists());
    assert!(dir.join("main.tf").exists());
    let diff = ttg_codegen::diff::against_dir(&generate(&flat, &cat, "aws", Tool::OpenTofu).unwrap(), &dir);
    assert!(
        diff.iter().any(
            |d| d.name == "modules/main/network.tf" && d.status == ttg_codegen::diff::FileStatus::Removed
        ),
        "going back lists the modules as removed"
    );
    export(&flat, &cat, "aws", Tool::OpenTofu, &dir).unwrap();
    assert!(
        !dir.join("modules").exists(),
        "an empty modules/ goes with its files"
    );
    assert!(dir.join("terraform.tfstate").exists(), "state is never touched");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_provider_lays_out_its_own_containers() {
    let cat = Catalog::builtin();
    let p = modules(example("three-tier"));
    // On Azure the resource group is a resource, so it is the top-level module.
    let g = generate(&p, &cat, "azure", Tool::Terraform).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        g.files["main.tf"].contains("module \"app\""),
        "{}",
        g.files["main.tf"]
    );
    assert!(g.files["modules/app/main.tf"].contains("resource \"azurerm_resource_group\" \"app\""));
    assert!(g.files["modules/app/versions.tf"].contains("source  = \"hashicorp/azurerm\""));
    assert_modules_self_contained(&g);
    let g = generate(&p, &cat, "gcp", Tool::OpenTofu).unwrap_or_else(|e| panic!("{e}"));
    assert!(g.files["main.tf"].contains("module \"main\""));
    assert_modules_self_contained(&g);
}

#[test]
fn a_project_without_containers_stays_flat() {
    let cat = Catalog::builtin();
    let p = modules(example("fan-out"));
    let flat = generate(&example("fan-out"), &cat, "aws", Tool::OpenTofu).unwrap();
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    assert_eq!(flat.files, g.files);
}

#[test]
fn adopted_resources_are_imported_at_their_module_address() {
    let cat = Catalog::builtin();
    let mut p = modules(example("three-tier"));
    let vpc = id_of(&p, "main");
    let role = id_of(&p, "app role");
    p.adoption_mut(&vpc)
        .unwrap()
        .entry("aws".into())
        .or_default()
        .insert("main".into(), "vpc-0abc1234def567890".into());
    // A key written as the module address the inspector lists names the same block.
    let subnet = id_of(&p, "web a");
    p.adoption_mut(&subnet)
        .unwrap()
        .entry("aws".into())
        .or_default()
        .insert("module.main.aws_subnet.web_a".into(), "subnet-0abc".into());
    p.adoption_mut(&role)
        .unwrap()
        .entry("aws".into())
        .or_default()
        .insert("main".into(), "app-role".into());
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap_or_else(|e| panic!("{e}"));
    let imports = &g.files["imports.tf"];
    assert!(imports.contains("to = module.main.aws_vpc.main"), "{imports}");
    assert!(imports.contains("to = module.main.aws_subnet.web_a"), "{imports}");
    assert!(imports.contains("to = aws_iam_role.app_role"), "{imports}");
}

#[test]
fn a_modules_export_imports_back_without_changes_and_with_its_edits() {
    let mut cat = Catalog::builtin();
    let p = modules(example("three-tier"));
    let dir = std::env::temp_dir().join(format!("ttg-modules-import-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    export(&p, &cat, "aws", Tool::OpenTofu, &dir).unwrap();
    let same = ttg_codegen::import_dir::import_dir(&p, &mut cat, &dir, None).unwrap();
    assert!(same.is_empty(), "{}", same.report_markdown());
    // An edit inside a module comes back as an override in the flat form, a reference
    // through a module input as the reference it stands for.
    let path = dir.join("modules/main/network.tf");
    let text = std::fs::read_to_string(&path).unwrap();
    let edited = text.replacen("enable_dns_hostnames = true", "enable_dns_hostnames = false", 1);
    assert_ne!(text, edited, "{text}");
    std::fs::write(&path, edited).unwrap();
    let imp = ttg_codegen::import_dir::import_dir(&p, &mut cat, &dir, None).unwrap();
    let vpc = imp
        .overrides
        .iter()
        .find(|o| o.addresses == ["aws_vpc.main"])
        .unwrap_or_else(|| panic!("{}", imp.report_markdown()));
    assert_eq!(vpc.args["enable_dns_hostnames"], serde_json::json!(false));
    assert!(imp.deleted.is_empty(), "{}", imp.report_markdown());
    assert!(imp.added.is_empty(), "{}", imp.report_markdown());
    let _ = std::fs::remove_dir_all(&dir);
}
