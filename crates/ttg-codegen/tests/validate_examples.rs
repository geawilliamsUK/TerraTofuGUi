//! Runs `tofu validate` / `terraform validate` on every example project for every
//! provider that can be exported — every root of it, the `bootstrap/` root(s) included —
//! and on `hardened` with each state backend and state encryption on, which is what
//! produces the bootstrap roots. Skips (passes) when neither binary is installed, so the
//! ordinary test suite stays dependency-free; CI installs OpenTofu so it always runs there.
//! Set `TTG_REQUIRE_VALIDATE=1` to make a missing binary a failure.

use std::path::{Path, PathBuf};
use ttg_catalog::Catalog;
use ttg_codegen::validate::{find_binary, run, Outcome};
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
