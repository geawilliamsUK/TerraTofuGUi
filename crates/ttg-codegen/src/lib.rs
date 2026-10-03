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
pub mod conflicts;
pub mod cost;
pub mod dataflow;
pub mod diagnostics;
pub mod diff;
pub mod edge;
pub mod emit;
pub mod files;
pub mod k8s;
pub mod layers;
pub mod owned;
pub mod plan;
pub mod reach;
pub mod refs;
pub mod state;
pub mod tool;
pub mod validate;
pub mod versions;
pub mod views;

pub use diagnostics::{Code, Diagnostic, Severity};
pub use emit::{generate, EntityBlocks, EntityPreview, Generated, ManualEntry};
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
/// The files the export owns (`owned`: the `.tf` files, `MANUAL_STEPS.md` and `README.md`
/// at the top level and in the `bootstrap/` root, the files directly inside `k8s/`) are
/// replaced, or removed when no longer generated; everything else — variable values,
/// state, `.terraform/`, the render scripts' own `k8s/rendered/` — is left alone.
pub fn export(
    project: &Project,
    catalog: &Catalog,
    provider: &str,
    tool: Tool,
    out_dir: &Path,
) -> Result<ExportReport, GenError> {
    let generated = generate(project, catalog, provider, tool)?;
    std::fs::create_dir_all(out_dir)?;
    // Stale resources, manifests of workloads that are gone, a bootstrap root after the
    // backend was cleared: none of it may linger.
    owned::remove_stale(&generated, out_dir);
    let mut written = Vec::new();
    for (name, content) in &generated.files {
        let path = out_dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)?;
        #[cfg(unix)]
        if name.ends_with(".sh") {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        }
        written.push(name.clone());
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
