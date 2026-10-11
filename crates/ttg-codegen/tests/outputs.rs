//! Which outputs an export writes: every one for a file that does not say (as before),
//! or with `settings.outputs = "marked"` the ones each entity selects, else its
//! mapping's primary ones — plus whatever other parts of the export read.

use std::path::Path;
use ttg_catalog::Catalog;
use ttg_codegen::generate;
use ttg_core::{OutputSelection, Project, Tool};

fn example(name: &str) -> Project {
    ttg_core::project::load(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../examples/{name}.ttg.json")),
    )
    .unwrap()
}

fn marked(mut p: Project) -> Project {
    p.settings.outputs = Some(OutputSelection::Marked);
    p
}

fn select(p: &mut Project, name: &str, outputs: &[&str]) {
    let list = Some(outputs.iter().map(|s| s.to_string()).collect());
    if let Some(n) = p.nodes.values_mut().find(|n| n.name == name) {
        n.outputs = list;
    } else if let Some(c) = p.containers.values_mut().find(|c| c.name == name) {
        c.outputs = list;
    } else {
        panic!("no entity {name}");
    }
}

#[test]
fn a_file_that_does_not_say_keeps_every_output() {
    let cat = Catalog::builtin();
    let p = example("three-tier");
    assert_eq!(p.settings.outputs, None);
    assert_eq!(p.settings.output_selection(), OutputSelection::All);
    let outs = &generate(&p, &cat, "aws", Tool::OpenTofu).unwrap().files["outputs.tf"];
    for o in [
        "main_id",
        "web_a_id",
        "app_db_endpoint",
        "app_db_address",
        "igw_id",
    ] {
        assert!(outs.contains(&format!("output \"{o}\"")), "{o}: {outs}");
    }
}

#[test]
fn marked_writes_the_primary_outputs_only() {
    let cat = Catalog::builtin();
    let p = marked(example("three-tier"));
    let outs = &generate(&p, &cat, "aws", Tool::OpenTofu).unwrap().files["outputs.tf"];
    for o in ["main_id", "app_db_endpoint", "web_lb_dns_name", "app_role_arn"] {
        assert!(outs.contains(&format!("output \"{o}\"")), "{o} missing: {outs}");
    }
    for o in [
        "web_a_id",
        "app_db_address",
        "igw_id",
        "public_routes_id",
        "web_lb_zone_id",
    ] {
        assert!(!outs.contains(&format!("output \"{o}\"")), "{o} written: {outs}");
    }
}

#[test]
fn an_entity_chooses_its_own_outputs() {
    let cat = Catalog::builtin();
    let mut p = marked(example("three-tier"));
    select(&mut p, "app db", &["address"]);
    select(&mut p, "main", &[]);
    select(&mut p, "web a", &["id"]);
    let outs = &generate(&p, &cat, "aws", Tool::OpenTofu).unwrap().files["outputs.tf"];
    assert!(outs.contains("output \"app_db_address\""), "{outs}");
    assert!(!outs.contains("output \"app_db_endpoint\""), "{outs}");
    assert!(!outs.contains("output \"main_id\""), "{outs}");
    assert!(outs.contains("output \"web_a_id\""), "{outs}");
    // With `all` the selection does not matter.
    p.settings.outputs = Some(OutputSelection::All);
    let outs = &generate(&p, &cat, "aws", Tool::OpenTofu).unwrap().files["outputs.tf"];
    assert!(outs.contains("output \"app_db_endpoint\"") && outs.contains("output \"main_id\""));
}

#[test]
fn a_new_project_starts_marked_and_says_so_in_its_file() {
    let p = Project::new_document("fresh");
    assert_eq!(p.settings.outputs, Some(OutputSelection::Marked));
    let text = serde_json::to_string(&p).unwrap();
    assert!(text.contains("\"outputs\":\"marked\""), "{text}");
    // An old file round-trips without the key.
    let old = serde_json::to_string(&example("minimal")).unwrap();
    assert!(!old.contains("\"outputs\""), "{old}");
}

#[test]
fn what_other_parts_of_the_export_read_stays() {
    let cat = Catalog::builtin();
    // The manifests render from the k8s_* outputs.
    let mut k = marked(example("kubernetes"));
    k.settings.kubernetes_manifests = true;
    let g = generate(&k, &cat, "aws", Tool::OpenTofu).unwrap();
    assert!(
        g.files["outputs.tf"].contains("output \"k8s_"),
        "{}",
        g.files["outputs.tf"]
    );
    // The bootstrap root's outputs are what the main root's key variables are filled from.
    let mut h = example("hardened");
    h.settings.backend = Some(ttg_core::BackendConfig {
        backend_type: "s3".into(),
        args: [("bucket", "hardened-tfstate-x7q2"), ("region", "eu-west-2")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    });
    h.settings.state_encryption = true;
    h.settings.state_encryption_key = Some("key-main".into());
    let all = generate(&h, &cat, "aws", Tool::OpenTofu).unwrap();
    let m = generate(&marked(h), &cat, "aws", Tool::OpenTofu).unwrap();
    assert_eq!(all.files["bootstrap/outputs.tf"], m.files["bootstrap/outputs.tf"]);
    assert!(m.files["bootstrap/outputs.tf"].contains("output \""));
}
