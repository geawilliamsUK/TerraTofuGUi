//! `ttg-codegen` — turns a `ttg_core::Project` plus a `ttg_catalog::Catalog` into a
//! complete Terraform or OpenTofu project directory for one provider.
//!
//! Pipeline (see `docs/ARCHITECTURE.md` §6):
//!
//! ```text
//! diagnostics::run  ->  emit::generate  ->  files::*  ->  write / bundle
//! ```
//!
//! "Export for all providers" is `export_all`, which runs the single-provider pipeline
//! once per provider into sibling directories. The outputs share nothing.

pub mod bundle;
pub mod diagnostics;
pub mod emit;
pub mod files;
pub mod layers;
pub mod reach;
pub mod tool;
pub mod validate;

pub use diagnostics::{Code, Diagnostic, Severity};
pub use emit::{generate, Generated, ManualEntry};
pub use tool::Profile;

use std::path::Path;
use ttg_catalog::Catalog;
use ttg_core::{Project, Tool};

#[derive(Debug, thiserror::Error)]
pub enum GenError {
    #[error("unknown provider '{0}'")]
    UnknownProvider(String),
    #[error("project has errors that block export:\n{}", format_diags(.0))]
    Blocked(Vec<Diagnostic>),
    #[error("{0}")]
    Emit(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("zip error: {0}")]
    Zip(String),
}

fn format_diags(d: &[Diagnostic]) -> String {
    d.iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| format!("- {d}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Result of writing one provider's project to disk.
#[derive(Debug, Clone)]
pub struct ExportReport {
    pub provider: String,
    pub tool: Tool,
    pub out_dir: std::path::PathBuf,
    pub files: Vec<String>,
    pub manual_steps: Vec<ManualEntry>,
    pub warnings: Vec<Diagnostic>,
}

/// Generate and write a single-provider project into `out_dir` (created if needed).
/// Existing `.tf`, `MANUAL_STEPS.md` and `README.md` files in the directory are
/// replaced; other files are left alone.
pub fn export(
    project: &Project,
    catalog: &Catalog,
    provider: &str,
    tool: Tool,
    out_dir: &Path,
) -> Result<ExportReport, GenError> {
    let generated = generate(project, catalog, provider, tool)?;
    std::fs::create_dir_all(out_dir)?;
    // Remove previously generated .tf files so stale resources do not linger.
    if let Ok(rd) = std::fs::read_dir(out_dir) {
        for e in rd.flatten() {
            let p = e.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.ends_with(".tf") && !generated.files.contains_key(name) {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    let mut written = Vec::new();
    for (name, content) in &generated.files {
        std::fs::write(out_dir.join(name), content)?;
        written.push(name.clone());
    }
    if !generated.manual_steps.is_empty() {
        // MANUAL_STEPS.md is part of `files` already when non-empty; nothing extra.
    } else {
        let _ = std::fs::remove_file(out_dir.join("MANUAL_STEPS.md"));
    }
    Ok(ExportReport {
        provider: provider.to_string(),
        tool,
        out_dir: out_dir.to_path_buf(),
        files: written,
        manual_steps: generated.manual_steps,
        warnings: generated
            .diagnostics
            .into_iter()
            .filter(|d| d.severity != Severity::Error)
            .collect(),
    })
}

/// Export every provider in the catalog into `out_root/<provider>/`. Each directory is a
/// complete, independent project. Providers whose export is blocked by errors are
/// reported in the `Err` slot for that provider; the others still succeed.
pub fn export_all(
    project: &Project,
    catalog: &Catalog,
    tool: Tool,
    out_root: &Path,
) -> Vec<(String, Result<ExportReport, GenError>)> {
    let mut out = Vec::new();
    for pid in catalog.provider_ids() {
        let dir = out_root.join(&pid);
        out.push((pid.clone(), export(project, catalog, &pid, tool, &dir)));
    }
    out
}
