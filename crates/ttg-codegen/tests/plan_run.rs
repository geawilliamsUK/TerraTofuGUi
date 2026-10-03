//! The real `init` / `plan` / `show -json` path, on a configuration that needs no cloud:
//! `terraform_data` is built into both tools, so the plan runs without credentials or a
//! provider download. Skips (passes) when neither binary is installed, like the validate
//! suite; `TTG_REQUIRE_VALIDATE=1` makes a missing binary a failure.

use std::path::{Path, PathBuf};
use ttg_codegen::plan_run::{self, Attribution, PlanOptions, PlanStatus};
use ttg_codegen::validate::find_binary;
use ttg_core::Tool;

fn runner() -> Option<Tool> {
    let t = [Tool::OpenTofu, Tool::Terraform]
        .into_iter()
        .find(|t| find_binary(*t).is_some());
    if t.is_none() && std::env::var("TTG_REQUIRE_VALIDATE").is_ok() {
        panic!("TTG_REQUIRE_VALIDATE is set but neither tofu nor terraform is installed");
    }
    t
}

const MAIN: &str = r#"# --- Logical "first" (helper)
resource "terraform_data" "first" {
  input = var.size
}

# --- Logical "second" (helper)
resource "terraform_data" "second" {
  input = terraform_data.first.output
}
"#;

fn scratch(name: &str, main: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ttg-plan-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("environments")).unwrap();
    std::fs::write(dir.join("main.tf"), main).unwrap();
    std::fs::write(
        dir.join("variables.tf"),
        "variable \"size\" {\n  type = string\n}\n",
    )
    .unwrap();
    std::fs::write(dir.join("environments/pilot.tfvars"), "size = \"small\"\n").unwrap();
    dir
}

fn attribution() -> Attribution {
    let mut a = Attribution::default();
    for (addr, id, lines) in [
        ("terraform_data.first", "e-first", (1, 4)),
        ("terraform_data.second", "e-second", (6, 9)),
    ] {
        a.addresses.insert(addr.into(), id.into());
        a.sections.push(("main.tf".into(), lines.0, lines.1, id.into()));
        a.names.insert(id.into(), id.trim_start_matches("e-").into());
    }
    a
}

#[test]
fn a_plan_comes_back_grouped_by_entity() {
    let Some(tool) = runner() else {
        eprintln!("skipping: no tofu/terraform binary found");
        return;
    };
    let dir = scratch("ok", MAIN);
    let report = plan_run::run(
        &dir,
        tool,
        &attribution(),
        &PlanOptions {
            var_file: Some("environments/pilot.tfvars".into()),
            ..Default::default()
        },
    );
    assert_eq!(report.status, PlanStatus::Planned, "{report:#?}");
    assert_eq!(report.create, 2);
    let names: Vec<(&str, usize)> = report
        .entities
        .iter()
        .map(|e| (e.name.as_str(), e.create))
        .collect();
    assert_eq!(names, vec![("first", 1), ("second", 1)]);
    assert!(report.unattributed.is_empty());
    // The plan ran in the scratch copy; the export directory itself holds no state.
    assert!(report.dir.ends_with(plan_run::SCRATCH_DIR));
    assert!(!Path::new(&dir).join("terraform.tfstate").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn plan_and_validate_errors_come_back_on_their_entity() {
    let Some(tool) = runner() else {
        eprintln!("skipping: no tofu/terraform binary found");
        return;
    };
    // `second` refers to an attribute terraform_data does not have.
    let broken = MAIN.replace(
        "terraform_data.first.output",
        "terraform_data.first.no_such_thing",
    );
    let dir = scratch("broken", &broken);
    let (valid, ds) = plan_run::validate(&dir, tool, &attribution()).unwrap();
    assert!(!valid);
    let d = ds.iter().find(|d| d.severity == "error").expect("an error");
    assert_eq!(d.entity.as_deref(), Some("e-second"), "{ds:#?}");
    let report = plan_run::run(
        &dir,
        tool,
        &attribution(),
        &PlanOptions {
            var_file: Some("environments/pilot.tfvars".into()),
            ..Default::default()
        },
    );
    assert_eq!(report.status, PlanStatus::Failed);
    assert!(report
        .diagnostics
        .iter()
        .any(|d| d.entity.as_deref() == Some("e-second")));
    let _ = std::fs::remove_dir_all(&dir);
}
