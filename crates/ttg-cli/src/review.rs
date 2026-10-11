//! `ttg diff-project`: what changed between two project files, in architecture terms.
//! `ttg import-dir`: bring hand edits made to an exported directory back into a project.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use ttg_catalog::Catalog;

fn label(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| p.display().to_string())
}

/// Print the Markdown (or, with `json`, the structure) of the diff from `old` to `new`.
/// Exit code 1 with `exit_code` when they differ.
pub fn diff_project(cat: &Catalog, old: &Path, new: &Path, json: bool, exit_code: bool) -> Result<()> {
    let a = ttg_core::project::load(old)?;
    let b = ttg_core::project::load(new)?;
    let d = ttg_core::arch_diff::diff(&a, &b);
    if json {
        println!("{}", serde_json::to_string_pretty(&d)?);
    } else {
        let type_label = |t: &str| {
            cat.resource(t)
                .map(|r| r.resource.display_name.clone())
                .unwrap_or_else(|| t.to_string())
        };
        print!("{}", d.to_markdown(&label(old), &label(new), &type_label));
    }
    if exit_code && !d.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

pub struct ImportArgs<'a> {
    pub project: &'a Path,
    pub dir: &'a Path,
    pub provider: Option<String>,
    /// Write the updated project here.
    pub out: Option<PathBuf>,
    /// Overwrite the project file.
    pub in_place: bool,
    pub json: bool,
}

/// Work out the import, print the report and the architecture diff, and write the
/// updated project only when asked to.
pub fn import_dir(cat: &mut Catalog, a: ImportArgs) -> Result<()> {
    if a.out.is_some() && a.in_place {
        bail!("give --out or --in-place, not both");
    }
    let project = ttg_core::project::load(a.project)?;
    cat.ensure_native_types(&project);
    let imp = match ttg_codegen::import_dir::import_dir(&project, cat, a.dir, a.provider.as_deref()) {
        Ok(i) => i,
        Err(e) => bail!(e),
    };
    let diff = ttg_core::arch_diff::diff(&project, &imp.project);
    if a.json {
        let mut v = imp.report_json();
        v["diff"] = serde_json::to_value(&diff)?;
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        let type_label = |t: &str| {
            cat.resource(t)
                .map(|r| r.resource.display_name.clone())
                .unwrap_or_else(|| t.to_string())
        };
        print!("{}", imp.report_markdown());
        println!();
        print!(
            "{}",
            diff.to_markdown(
                &label(a.project),
                &format!("imported from {}", a.dir.display()),
                &type_label
            )
        );
    }
    let dest = match (a.out, a.in_place) {
        (Some(o), _) => Some(o),
        (None, true) => Some(a.project.to_path_buf()),
        (None, false) => None,
    };
    match dest {
        Some(d) => {
            ttg_core::project::save(&imp.project, &d)?;
            eprintln!("wrote {}", d.display());
            if !imp.hand_files.is_empty() {
                eprintln!(
                    "the export now writes the blocks of {}: delete those files from {}",
                    imp.hand_files.join(", "),
                    a.dir.display()
                );
            }
        }
        None => eprintln!("preview only: pass --out <file> or --in-place to write the updated project"),
    }
    Ok(())
}
