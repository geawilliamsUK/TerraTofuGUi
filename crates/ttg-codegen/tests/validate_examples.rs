//! Runs `tofu validate` / `terraform validate` on every example project for every
//! provider that can be exported. Skips (passes) when neither binary is installed, so the
//! ordinary test suite stays dependency-free; CI installs OpenTofu so it always runs there.
//! Set `TTG_REQUIRE_VALIDATE=1` to make a missing binary a failure.

use std::path::Path;
use ttg_catalog::Catalog;
use ttg_codegen::validate::{find_binary, run, Outcome};
use ttg_core::Tool;

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
    let cat = Catalog::builtin();
    let root = std::env::temp_dir().join(format!("ttg-validate-{}", std::process::id()));
    let mut ran = 0;
    let mut failures = Vec::new();
    for example in examples() {
        let project = ttg_core::project::load(&example).unwrap();
        let stem = example
            .file_name()
            .unwrap()
            .to_string_lossy()
            .replace(".ttg.json", "");
        for provider in cat.provider_ids() {
            for tool in Tool::ALL {
                let out = root.join(&stem).join(&provider).join(tool.binary_name());
                // Providers the example is not configured for are expected to be blocked.
                let rep = match ttg_codegen::export(&project, &cat, &provider, tool, &out) {
                    Ok(r) => r,
                    Err(ttg_codegen::GenError::Blocked(_)) => continue,
                    Err(e) => panic!("{stem}/{provider}: {e}"),
                };
                ran += 1;
                match run(&rep.out_dir, runner) {
                    Outcome::Ran { success: true, .. } => {}
                    other => failures.push(format!(
                        "{stem}/{provider}/{}: {}",
                        tool.display_name(),
                        other.summary()
                    )),
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&root);
    assert!(ran > 0, "no example produced an exportable configuration");
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
