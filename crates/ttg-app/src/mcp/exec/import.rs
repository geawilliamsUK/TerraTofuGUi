//! `project_import`: a project handed over as `.ttg.json` text replaces the open one,
//! as one undo step. The same file format the app saves, so an agent that cannot reach
//! the server can write a file and one that can reach it can paste the same text here.

use super::R;
use crate::app::TtgApp;
use serde_json::json;
use ttg_core::Project;

/// Check the text before anything changes: every schema problem at once (with lines),
/// then the loader's own checks. Unknown fields come back as warnings (they are ignored).
pub(crate) fn import_check(text: &str) -> Result<(Project, Vec<String>), String> {
    let problems = ttg_core::schema_check::check_project_text(text).map_err(|p| {
        format!(
            "not a project file (nothing changed): line {}: {}",
            p.line, p.message
        )
    })?;
    let errors: Vec<String> = problems
        .iter()
        .filter(|p| !p.warning)
        .map(|p| p.to_string())
        .collect();
    if !errors.is_empty() {
        let shown: Vec<String> = errors.iter().take(30).cloned().collect();
        return Err(format!(
            "the JSON does not match the project schema ({} error(s); nothing changed; the schema is {}):\n{}{}",
            errors.len(),
            ttg_core::json_schema::SCHEMA_ID,
            shown.join("\n"),
            if errors.len() > shown.len() { "\n…" } else { "" }
        ));
    }
    let project =
        ttg_core::project::load_str(text).map_err(|e| format!("cannot load it (nothing changed): {e}"))?;
    let warnings = problems
        .iter()
        .filter(|p| p.warning)
        .map(|p| p.to_string())
        .collect();
    Ok((project, warnings))
}

impl TtgApp {
    pub(super) fn project_import(&mut self, text: String, replace: bool) -> R {
        let (project, warnings) = import_check(&text)?;
        let existing = self.project.nodes.len() + self.project.containers.len();
        if existing > 0 && !replace {
            return Err(format!(
                "the open project \"{}\" has {existing} entities and project_import replaces it; pass replace: true to do that (one undo step{})",
                self.project.name,
                if self.dirty {
                    "; the user is asked first, because it has unsaved changes"
                } else {
                    ""
                }
            ));
        }
        let before = self.snapshot();
        self.catalog.ensure_native_types(&project);
        self.project = project;
        self.ensure_provider_settings();
        // The imported project is not the file the old one came from: a later save
        // without a path must not overwrite that file.
        self.path = None;
        self.selection.clear();
        self.selected_edge = None;
        self.activate_view(None);
        self.fit_requested = true;
        self.finish(before);
        self.dirty = true;
        self.refresh_diagnostics();
        let errors: Vec<String> = self.errors().iter().take(20).map(|d| d.to_string()).collect();
        let warnings_n = self
            .diagnostics
            .iter()
            .filter(|d| d.severity == ttg_codegen::Severity::Warning)
            .count();
        Ok(json!({
            "status": "imported",
            "name": self.project.name,
            "counts": {
                "nodes": self.project.nodes.len(),
                "containers": self.project.containers.len(),
                "edges": self.project.edges.len(),
                "views": self.project.views.len(),
            },
            "diagnostics": {
                "errors": self.errors().len(),
                "warnings": warnings_n,
                "first_errors": errors,
            },
            "ignored_fields": warnings,
            "path": null,
            "saved": false,
            "note": "Not saved: the imported project has no file yet (project_save { path } writes one). One undo brings the previous project back.",
        }))
    }
}
