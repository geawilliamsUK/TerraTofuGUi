//! Adoption (`import` blocks) and importing an edited export directory back into the
//! project. The validate parts run `tofu validate` (never plan or apply) when a binary
//! is installed; `TTG_REQUIRE_VALIDATE=1` makes a missing binary a failure.

use std::path::{Path, PathBuf};
use ttg_catalog::Catalog;
use ttg_codegen::validate::{find_binary, run, Outcome};
use ttg_core::{Project, Tool};

fn example(name: &str) -> Project {
    ttg_core::project::load(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples")
            .join(name),
    )
    .unwrap()
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ttg-wp-adopt-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn validator() -> Option<Tool> {
    let t = [Tool::OpenTofu, Tool::Terraform]
        .into_iter()
        .find(|t| find_binary(*t).is_some());
    if t.is_none() && std::env::var("TTG_REQUIRE_VALIDATE").is_ok() {
        panic!("TTG_REQUIRE_VALIDATE is set but neither tofu nor terraform is installed");
    }
    t
}

fn validate(dir: &Path, tool: Tool) {
    match run(dir, tool) {
        Outcome::Ran { success: true, .. } => {}
        other => panic!("validate failed in {}: {}", dir.display(), other.summary()),
    }
    if let Some(Err(e)) = ttg_codegen::validate::fmt_check(dir, tool) {
        panic!("fmt -check failed in {}: {e}", dir.display());
    }
}

/// An adopted entity gets an `import` block per id, the diagnostics name the blocks
/// still without an id and keys that name nothing, and the result validates on every
/// provider.
#[test]
fn adopted_entities_write_import_blocks_that_validate() {
    let cat = Catalog::builtin();
    let base = example("minimal.ttg.json");
    let ids = [
        ("aws", "vpc-0abc1234def567890"),
        (
            "azure",
            "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/rg/providers/Microsoft.Network/virtualNetworks/main",
        ),
        ("gcp", "projects/demo/global/networks/main"),
    ];
    let root = scratch("validate");
    let tool = validator();
    for (provider, id) in ids {
        let mut p = base.clone();
        let adopt = p.adoption_mut("net").unwrap();
        adopt
            .entry(provider.into())
            .or_default()
            .insert("main".into(), id.into());
        // The role adopted by address, the function's main block by key.
        let worker_role = ttg_codegen::emit::addresses_for(&p, &cat, provider, "worker-role");
        if let Some(first) = worker_role.iter().find(|a| !a.starts_with("data.")) {
            p.adoption_mut("worker-role")
                .unwrap()
                .entry(provider.into())
                .or_default()
                .insert(first.clone(), "existing-role".into());
        }
        let g = match ttg_codegen::generate(&p, &cat, provider, Tool::OpenTofu) {
            Ok(g) => g,
            // Minimal is an AWS example; the other providers need more than it has.
            Err(ttg_codegen::GenError::Blocked(_)) if provider != "aws" => {
                let layer = ttg_codegen::layers::project_for(&p, &cat, provider);
                let plan = ttg_codegen::plan::Plan::build(&layer, &cat, provider, None);
                let t = ttg_codegen::adopt::targets(
                    &layer,
                    &plan,
                    provider,
                    &plan.addresses().into_keys().collect(),
                );
                assert!(t.iter().any(|x| x.id == id), "{provider}: {t:?}");
                continue;
            }
            Err(e) => panic!("{provider}: {e}"),
        };
        let imports = g
            .files
            .get("imports.tf")
            .unwrap_or_else(|| panic!("{provider}: no imports.tf"));
        assert!(imports.contains(&format!("id = \"{id}\"")), "{imports}");
        assert!(imports.contains("import {"), "{imports}");
        if let Some(t) = tool {
            let dir = root.join(provider);
            ttg_codegen::export(&p, &cat, provider, Tool::OpenTofu, &dir).unwrap();
            validate(&dir, t);
        }
    }

    // Every example with an adopted entity validates too: three providers, full exports.
    if let Some(t) = tool {
        let mut p = example("three-tier.ttg.json");
        let vnet = p
            .containers
            .values()
            .find(|c| c.container_type == "virtual_network")
            .unwrap()
            .id
            .clone();
        for (provider, id) in ids {
            p.adoption_mut(&vnet)
                .unwrap()
                .entry(provider.into())
                .or_default()
                .insert("main".into(), id.into());
        }
        for provider in cat.provider_ids() {
            let dir = root.join(format!("three-tier-{provider}"));
            match ttg_codegen::export(&p, &cat, &provider, Tool::OpenTofu, &dir) {
                Ok(r) => {
                    assert!(
                        r.files.iter().any(|f| f == "imports.tf"),
                        "{provider}: {:?}",
                        r.files
                    );
                    validate(&dir, t);
                }
                Err(ttg_codegen::GenError::Blocked(_)) => {}
                Err(e) => panic!("{provider}: {e}"),
            }
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// With environments the merged root imports once: one id everywhere, or per environment
/// through a project variable (a lookup on `var.environment`); an adopted entity some
/// environment leaves out is refused, by the diagnostics and by the export.
#[test]
fn adoption_with_environments() {
    let cat = Catalog::builtin();
    let mut p = example("three-tier.ttg.json");
    assert!(!p.settings.environments.is_empty(), "three-tier has environments");
    let envs = p.settings.environments.clone();
    let vnet = p
        .containers
        .values()
        .find(|c| c.container_type == "virtual_network")
        .unwrap()
        .id
        .clone();
    p.settings.variables.insert(
        "vpc_id".into(),
        ttg_core::environment::ProjectVariable {
            value: ttg_core::Value::Str("vpc-0base".into()),
            description: String::new(),
            environments: envs
                .iter()
                .map(|e| (e.clone(), ttg_core::Value::Str(format!("vpc-0{e}"))))
                .collect(),
        },
    );
    p.adoption_mut(&vnet)
        .unwrap()
        .entry("aws".into())
        .or_default()
        .insert("main".into(), "${var.vpc_id}".into());
    let bucket = p
        .nodes
        .values()
        .find(|n| n.resource_type == "object_storage")
        .unwrap()
        .id
        .clone();
    p.adoption_mut(&bucket)
        .unwrap()
        .entry("aws".into())
        .or_default()
        .insert("main".into(), "three-tier-assets-x7q2".into());
    let g = ttg_codegen::generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let imports = &g.files["imports.tf"];
    for e in &envs {
        assert!(
            imports.contains(&format!("\"{e}\"")) && imports.contains(&format!("= \"vpc-0{e}\"")),
            "{imports}"
        );
    }
    assert!(imports.contains("[var.environment]"), "{imports}");
    assert!(imports.contains("id = \"three-tier-assets-x7q2\""), "{imports}");
    assert_eq!(imports.matches("import {").count(), 2, "{imports}");
    if let Some(t) = validator() {
        let dir = scratch("environments");
        ttg_codegen::export(&p, &cat, "aws", Tool::OpenTofu, &dir).unwrap();
        validate(&dir, t);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Absent from one environment: counted there, so refused.
    let last = envs.last().unwrap().clone();
    p.nodes
        .get_mut(&bucket)
        .unwrap()
        .overrides
        .entry(last.clone())
        .or_default()
        .absent = true;
    let d = ttg_codegen::diagnostics::run(&p, &cat, "aws");
    assert!(
        d.iter().any(|x| x.code == ttg_codegen::Code::Adopt
            && x.severity == ttg_codegen::Severity::Error
            && x.message.contains(&format!("absent in {last}"))),
        "{d:#?}"
    );
    match ttg_codegen::generate(&p, &cat, "aws", Tool::OpenTofu) {
        Err(ttg_codegen::GenError::Blocked(ds)) => assert!(
            ds.iter().any(|x| x.code == ttg_codegen::Code::Adopt
                && x.message.contains("cannot depend on the environment")),
            "{ds:#?}"
        ),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("an adopted entity absent from an environment must not export"),
    }
}

#[test]
fn adoption_diagnostics_name_missing_ids_and_unknown_keys() {
    let cat = Catalog::builtin();
    let mut p = example("three-tier.ttg.json");
    let bucket = p
        .nodes
        .values()
        .find(|n| n.resource_type == "object_storage")
        .unwrap()
        .id
        .clone();
    let a = p.adoption_mut(&bucket).unwrap().entry("aws".into()).or_default();
    a.insert("main".into(), "three-tier-assets-x7q2".into());
    a.insert("nope".into(), "x".into());
    let d = ttg_codegen::diagnostics::run(&p, &cat, "aws");
    let mine: Vec<&ttg_codegen::Diagnostic> = d
        .iter()
        .filter(|x| x.code == ttg_codegen::Code::Adopt && x.entity.as_deref() == Some(bucket.as_str()))
        .collect();
    assert!(
        mine.iter()
            .any(|x| x.message.contains("adopt key \"nope\" names no block")),
        "{mine:#?}"
    );
    assert!(
        mine.iter()
            .any(|x| x.message.contains("no existing id and will be created")
                && x.message.contains("aws_s3_bucket_versioning")),
        "{mine:#?}"
    );
    // The Azure layer has no ids for it: nothing to say there.
    let az = ttg_codegen::diagnostics::run(&p, &cat, "azure");
    assert!(!az.iter().any(|x| x.code == ttg_codegen::Code::Adopt));
    // Flagged external as well: one or the other.
    p.nodes.get_mut(&bucket).unwrap().manual = true;
    let d = ttg_codegen::diagnostics::run(&p, &cat, "aws");
    assert!(d
        .iter()
        .any(|x| x.code == ttg_codegen::Code::Adopt && x.message.contains("flagged external")));
}

fn edit(dir: &Path, file: &str, from: &str, to: &str) {
    let path = dir.join(file);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains(from), "{file} does not contain {from:?}");
    std::fs::write(&path, text.replacen(from, to, 1)).unwrap();
}

/// Export an example, edit it by hand (arguments changed, added and removed, a nested
/// block changed, a resource added to a generated file, a hand-written file with a data
/// source and a resource that reads it), import the directory, export again: the second
/// export declares exactly what the edited directory declares.
#[test]
fn imported_directory_round_trips() {
    let mut cat = Catalog::builtin();
    let project = example("three-tier.ttg.json");
    let root = scratch("roundtrip");
    let edited = root.join("edited");
    ttg_codegen::export(&project, &cat, "aws", Tool::OpenTofu, &edited).unwrap();
    edit(
        &edited,
        "storage.tf",
        "bucket = \"three-tier-assets-x7q2\"",
        "bucket        = \"three-tier-assets-edited\"\n  force_destroy = true",
    );
    edit(&edited, "storage.tf", "  restrict_public_buckets = true\n", "");
    edit(
        &edited,
        "storage.tf",
        "status = \"Enabled\"",
        "status = \"Suspended\"",
    );
    let storage = std::fs::read_to_string(edited.join("storage.tf")).unwrap();
    std::fs::write(
        edited.join("storage.tf"),
        format!(
            "{storage}\nresource \"aws_s3_bucket_ownership_controls\" \"assets_ownership\" {{\n  bucket = aws_s3_bucket.assets.id\n\n  rule {{\n    object_ownership = \"BucketOwnerEnforced\"\n  }}\n}}\n"
        ),
    )
    .unwrap();
    std::fs::write(
        edited.join("data.tf"),
        "data \"aws_caller_identity\" \"current\" {}\n\nresource \"aws_ssm_parameter\" \"account\" {\n  name  = \"/${aws_s3_bucket.assets.bucket}/account\"\n  type  = \"String\"\n  value = data.aws_caller_identity.current.account_id\n}\n\nlocals {\n  ignored = true\n}\n",
    )
    .unwrap();

    let imp = ttg_codegen::import_dir::import_dir(&project, &mut cat, &edited, None).unwrap();
    assert_eq!(imp.provider, "aws");
    assert_eq!(imp.tool, Some(Tool::OpenTofu));
    let bucket = imp
        .overrides
        .iter()
        .find(|o| o.addresses == ["aws_s3_bucket.assets"])
        .unwrap();
    assert_eq!(bucket.args["bucket"], "three-tier-assets-edited");
    assert_eq!(bucket.args["force_destroy"], true);
    let pab = imp
        .overrides
        .iter()
        .find(|o| o.addresses == ["aws_s3_bucket_public_access_block.assets_public_access"])
        .unwrap();
    assert!(pab.args["restrict_public_buckets"].is_null());
    let ver = imp.overrides.iter().find(|o| o.block == "versioning").unwrap();
    assert_eq!(ver.args["versioning_configuration"]["status"], "Suspended");
    let added: Vec<&str> = imp.added.iter().map(|a| a.address.as_str()).collect();
    assert_eq!(
        added,
        [
            "data.aws_caller_identity.current",
            "aws_ssm_parameter.account",
            "aws_s3_bucket_ownership_controls.assets_ownership"
        ]
    );
    // References became $ref (to a generated block and to the new data source) and a
    // template a $raw.
    let ssm = imp.added.iter().find(|a| a.name == "account").unwrap();
    let extra = &imp.project.nodes[&ssm.id].extra["aws"]["main"];
    assert_eq!(extra["value"]["$ref"]["attr"], "account_id");
    assert_eq!(
        extra["name"]["$raw"],
        "\"/${aws_s3_bucket.assets.bucket}/account\""
    );
    let own = imp.added.iter().find(|a| a.name == "assets_ownership").unwrap();
    assert_eq!(
        imp.project.nodes[&own.id].extra["aws"]["main"]["bucket"]["$ref"]["attr"],
        "id"
    );
    assert!(
        imp.skipped
            .iter()
            .any(|s| s.contains("data.tf") && s.contains("locals")),
        "{:?}",
        imp.skipped
    );
    assert!(imp.deleted.is_empty(), "{:?}", imp.deleted);
    // The example has environments: their directory and the bookkeeping files the export
    // writes are not imported and not complained about.
    assert!(edited.join("environments").is_dir());
    assert!(
        imp.notes.iter().all(|n| !n.contains("environments/")),
        "{:?}",
        imp.notes
    );
    assert!(imp.owned_changed.is_empty(), "{:?}", imp.owned_changed);
    // The preview: the architecture diff names the new entities and the changed fields.
    let diff = ttg_core::arch_diff::diff(&project, &imp.project);
    assert_eq!(diff.entities.added.len(), 3);
    assert!(diff
        .entities
        .changed
        .iter()
        .any(|c| c.fields.iter().any(|f| f.path == "extra.aws.main.force_destroy")));
    assert!(imp.report_markdown().contains("force_destroy"));

    // Re-export the imported project: the same blocks as the edited directory.
    let again = root.join("again");
    ttg_codegen::export(&imp.project, &cat, "aws", Tool::OpenTofu, &again).unwrap();
    let want = ttg_codegen::import_dir::semantic_dir(&edited).unwrap();
    let got = ttg_codegen::import_dir::semantic_dir(&again).unwrap();
    let missing: Vec<&String> = want.keys().filter(|k| !got.contains_key(*k)).collect();
    let extra: Vec<&String> = got.keys().filter(|k| !want.contains_key(*k)).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "missing {missing:?}, extra {extra:?}"
    );
    for (k, v) in &want {
        assert_eq!(&got[k], v, "{k} differs");
    }
    // A second import of the re-export finds nothing more to do.
    let mut cat2 = cat.clone();
    let second = ttg_codegen::import_dir::import_dir(&imp.project, &mut cat2, &again, None).unwrap();
    assert!(second.is_empty(), "{}", second.report_markdown());
    if let Some(t) = validator() {
        validate(&again, t);
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_directory_that_is_not_an_export_is_refused() {
    let mut cat = Catalog::builtin();
    let project = example("minimal.ttg.json");
    let root = scratch("not-export");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("main.tf"), "resource \"aws_vpc\" \"x\" {}\n").unwrap();
    let e = ttg_codegen::import_dir::import_dir(&project, &mut cat, &root, None).unwrap_err();
    assert!(e.contains("not a TerraTofu export"), "{e}");
    let _ = std::fs::remove_dir_all(&root);
}
