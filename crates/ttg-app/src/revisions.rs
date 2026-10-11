//! Checkpoints, architecture diffs, patterns, adoption ids and directory imports: the
//! logic behind the windows in `revisions_ui.rs` and the matching MCP tools
//! (`mcp/exec/revisions.rs`).
//!
//! **Checkpoints** are named copies of the whole project kept in a sidecar file beside
//! it, `<name>.checkpoints.json` next to `<name>.ttg.json`, so they never reach an export,
//! the project schema or the project file's diff. A project that has never been saved
//! keeps its checkpoints in memory, and the first save writes them beside the file.
//! Restoring one is a single undo step.
//!
//! **User patterns** live in `<app data>/patterns/*.json` (`%APPDATA%\TerraTofu GUI\
//! patterns` on Windows, beside eframe's own `data` folder), or in `TTG_PATTERNS_DIR`.

use crate::app::TtgApp;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use ttg_codegen::patterns::Pattern;
use ttg_core::arch_diff::ProjectDiff;
use ttg_core::Project;

/// One named copy of the project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub name: String,
    /// Seconds since the Unix epoch.
    pub created_unix: u64,
    pub project: Project,
}

/// The sidecar file.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Sidecar {
    checkpoints_version: u32,
    #[serde(default)]
    checkpoints: Vec<Checkpoint>,
}

const SIDECAR_VERSION: u32 = 1;

/// `x.checkpoints.json` beside `x.ttg.json` (or beside any other file name).
pub fn sidecar_path(project: &Path) -> PathBuf {
    let name = project
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let stem = name
        .strip_suffix(".ttg.json")
        .or_else(|| name.strip_suffix(".json"))
        .unwrap_or(&name);
    project.with_file_name(format!("{stem}.checkpoints.json"))
}

fn load_sidecar(path: &Path) -> Result<Vec<Checkpoint>, String> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        Ok(text) => {
            let s: Sidecar = serde_json::from_str(&text)
                .map_err(|e| format!("{} is not a checkpoints file: {e}", path.display()))?;
            if s.checkpoints_version > SIDECAR_VERSION {
                return Err(format!(
                    "{} was written by a newer version (checkpoints_version {})",
                    path.display(),
                    s.checkpoints_version
                ));
            }
            Ok(s.checkpoints)
        }
    }
}

fn save_sidecar(path: &Path, list: &[Checkpoint]) -> Result<(), String> {
    if list.is_empty() {
        return match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("cannot remove {}: {e}", path.display()))
            }
            _ => Ok(()),
        };
    }
    let s = Sidecar {
        checkpoints_version: SIDECAR_VERSION,
        checkpoints: list.to_vec(),
    };
    let mut text = serde_json::to_string_pretty(&s).map_err(|e| e.to_string())?;
    text.push('\n');
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `2026-10-03 14:05 UTC`.
pub fn format_time(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        secs / 3600,
        (secs % 3600) / 60
    )
}

/// A checkpoint as listed.
#[derive(Debug, Clone, Serialize)]
pub struct CheckpointInfo {
    pub name: String,
    pub created_unix: u64,
    pub created: String,
    pub entities: usize,
    /// What restoring it would change, in one line.
    pub changes_from_current: String,
}

/// The state behind this module: the checkpoints of the open project (cached per
/// file) and the windows' own state.
#[derive(Default)]
pub struct Revisions {
    /// Which project file the cached checkpoints belong to (`None`: never saved).
    cache: Option<(Option<PathBuf>, Vec<Checkpoint>)>,
    pub ui: crate::revisions_ui::RevisionsUi,
}

impl Revisions {
    /// The open project was replaced by another one (opened, new): the next access
    /// reads that project's checkpoints.
    pub fn project_replaced(&mut self) {
        self.cache = None;
    }
}

impl TtgApp {
    /// Bring the cached checkpoints in line with the project's file: read them when
    /// the file changed, and move an unsaved project's checkpoints beside the file it
    /// was just saved as.
    fn sync_checkpoints(&mut self) -> Result<(), String> {
        let path = self.path.clone();
        let carried = match self.revisions.cache.take() {
            Some((p, list)) if p == path => {
                self.revisions.cache = Some((p, list));
                return Ok(());
            }
            Some((None, list)) if path.is_some() => list,
            _ => Vec::new(),
        };
        let mut list = match &path {
            Some(p) => load_sidecar(&sidecar_path(p))?,
            None => Vec::new(),
        };
        if !carried.is_empty() {
            for c in carried {
                list.retain(|x| !x.name.eq_ignore_ascii_case(&c.name));
                list.push(c);
            }
            if let Some(p) = &path {
                save_sidecar(&sidecar_path(p), &list)?;
            }
        }
        self.revisions.cache = Some((path, list));
        Ok(())
    }

    fn checkpoints_mut(&mut self) -> Result<&mut Vec<Checkpoint>, String> {
        self.sync_checkpoints()?;
        Ok(&mut self.revisions.cache.as_mut().unwrap().1)
    }

    fn write_checkpoints(&mut self) -> Result<(), String> {
        let Some((Some(p), list)) = &self.revisions.cache else {
            return Ok(());
        };
        save_sidecar(&sidecar_path(p), list)
    }

    /// Where checkpoints are kept, in words.
    pub fn checkpoint_storage(&self) -> String {
        match &self.path {
            Some(p) => format!("{}", sidecar_path(p).display()),
            None => "in memory until the project is saved (then beside the project file)".into(),
        }
    }

    /// The checkpoints' names, oldest first (cheap: no diff).
    pub fn checkpoint_names(&mut self) -> Vec<String> {
        self.checkpoints_mut()
            .map(|l| l.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default()
    }

    pub fn checkpoint_list(&mut self) -> Result<Vec<CheckpointInfo>, String> {
        let current = self.project.clone();
        Ok(self
            .checkpoints_mut()?
            .iter()
            .map(|c| CheckpointInfo {
                name: c.name.clone(),
                created_unix: c.created_unix,
                created: format_time(c.created_unix),
                entities: c.project.nodes.len() + c.project.containers.len(),
                changes_from_current: ttg_core::arch_diff::diff(&current, &c.project).summary(),
            })
            .collect())
    }

    /// Save the project as it is now under `name`. A name already used is refused
    /// unless `replace`.
    pub fn checkpoint_create(&mut self, name: &str, replace: bool) -> Result<CheckpointInfo, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("a checkpoint needs a name".into());
        }
        let project = self.project.clone();
        let list = self.checkpoints_mut()?;
        if let Some(i) = list.iter().position(|c| c.name.eq_ignore_ascii_case(name)) {
            if !replace {
                return Err(format!(
                    "a checkpoint named \"{}\" exists already; pass replace: true to overwrite it, or choose another name",
                    list[i].name
                ));
            }
            list.remove(i);
        }
        let c = Checkpoint {
            name: name.to_string(),
            created_unix: now_unix(),
            project,
        };
        let info = CheckpointInfo {
            name: c.name.clone(),
            created_unix: c.created_unix,
            created: format_time(c.created_unix),
            entities: c.project.nodes.len() + c.project.containers.len(),
            changes_from_current: "no architecture changes".into(),
        };
        list.push(c);
        self.write_checkpoints()?;
        self.status = format!("Checkpoint \"{name}\" saved");
        Ok(info)
    }

    pub fn checkpoint_get(&mut self, name: &str) -> Result<Checkpoint, String> {
        let list = self.checkpoints_mut()?;
        list.iter()
            .find(|c| c.name.eq_ignore_ascii_case(name.trim()))
            .cloned()
            .ok_or_else(|| {
                format!(
                    "no checkpoint named \"{name}\" ({})",
                    if list.is_empty() {
                        "there are none; checkpoint_create makes one".to_string()
                    } else {
                        format!(
                            "checkpoints: {}",
                            list.iter().map(|c| c.name.clone()).collect::<Vec<_>>().join(", ")
                        )
                    }
                )
            })
    }

    /// Replace the diagram with a checkpoint's copy, as one undo step. Returns what
    /// changed.
    pub fn checkpoint_restore(&mut self, name: &str) -> Result<ProjectDiff, String> {
        let c = self.checkpoint_get(name)?;
        let diff = ttg_core::arch_diff::diff(&self.project, &c.project);
        let before = self.snapshot();
        self.catalog.ensure_native_types(&c.project);
        self.project = c.project;
        self.ensure_provider_settings();
        self.selection.retain(|id| self.project.contains(id));
        self.selected_edge = None;
        self.activate_view(None);
        self.finish(before);
        self.refresh_diagnostics();
        self.status = format!(
            "Restored checkpoint \"{}\" (Undo brings the previous diagram back)",
            c.name
        );
        Ok(diff)
    }

    pub fn checkpoint_delete(&mut self, name: &str) -> Result<(), String> {
        let list = self.checkpoints_mut()?;
        let n = list.len();
        list.retain(|c| !c.name.eq_ignore_ascii_case(name.trim()));
        if list.len() == n {
            return Err(format!("no checkpoint named \"{name}\""));
        }
        self.write_checkpoints()
    }

    /// The version a diff compares against: `saved` (the project file on disk), a
    /// checkpoint's name, or the path of another project file. Returns a label for it.
    pub fn diff_base(&mut self, against: &str) -> Result<(String, Project), String> {
        let a = against.trim();
        if a.is_empty() {
            return Err(
                "say what to compare with: \"saved\", a checkpoint's name or a .ttg.json path".into(),
            );
        }
        if a.eq_ignore_ascii_case("saved") {
            let p = self
                .path
                .clone()
                .ok_or("the project has never been saved, so there is no saved version to compare with")?;
            let saved =
                ttg_core::project::load(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
            return Ok((
                format!("saved ({})", p.file_name().unwrap_or_default().to_string_lossy()),
                saved,
            ));
        }
        if let Ok(c) = self.checkpoint_get(a) {
            return Ok((format!("checkpoint \"{}\"", c.name), c.project));
        }
        let path = Path::new(a);
        if path.is_file() {
            let p = ttg_core::project::load(path).map_err(|e| format!("cannot read {a}: {e}"))?;
            return Ok((
                path.file_name().unwrap_or_default().to_string_lossy().to_string(),
                p,
            ));
        }
        let names: Vec<String> = self.checkpoints_mut()?.iter().map(|c| c.name.clone()).collect();
        Err(format!(
            "\"{a}\" is not \"saved\", a checkpoint ({}) or an existing file",
            if names.is_empty() {
                "there are none".to_string()
            } else {
                names.join(", ")
            }
        ))
    }

    /// The catalog's display name of an abstract type (the id for a native type).
    pub fn type_label(&self, t: &str) -> String {
        self.catalog
            .resource(t)
            .map(|d| d.resource.display_name.clone())
            .unwrap_or_else(|| t.to_string())
    }

    /// What changed from `against` to the open project: the label of the base, the diff
    /// and its Markdown.
    pub fn project_diff(&mut self, against: &str) -> Result<(String, ProjectDiff, String), String> {
        let (label, base) = self.diff_base(against)?;
        let d = ttg_core::arch_diff::diff(&base, &self.project);
        let md = d.to_markdown(&label, "working copy", &|t| self.type_label(t));
        Ok((label, d, md))
    }

    // ------------------------------------------------------------------ adoption

    /// Set (or, with `None` / an empty id, clear) the cloud id an entity adopts for one
    /// of its blocks on `provider`, as one undo step. A full address of one of its
    /// blocks is stored as that block's key, so a rename does not orphan it.
    pub fn set_adopt(
        &mut self,
        id: &str,
        provider: &str,
        key: &str,
        cloud_id: Option<&str>,
    ) -> Result<(), String> {
        if !self.project.contains(id) {
            return Err(format!("no entity \"{id}\""));
        }
        let key = self.adopt_key(id, provider, key)?;
        let before = self.snapshot();
        let a = self.project.adoption_mut(id).unwrap();
        match cloud_id.map(str::trim).filter(|s| !s.is_empty()) {
            Some(v) => {
                a.entry(provider.to_string())
                    .or_default()
                    .insert(key, v.to_string());
            }
            None => {
                if let Some(m) = a.get_mut(provider) {
                    m.remove(&key);
                }
            }
        }
        a.retain(|_, m| !m.is_empty());
        self.finish(before);
        Ok(())
    }

    /// The canonical adopt key for `key` (a block key, `<block>/<row key>` or an
    /// address) of an entity on `provider`, or why it names nothing it generates.
    pub fn adopt_key(&self, id: &str, provider: &str, key: &str) -> Result<String, String> {
        let blocks = self.adoptable(id, provider);
        let key = key.trim();
        if let Some((k, _)) = blocks.iter().find(|(k, a)| k == key || a == key) {
            return Ok(k.clone());
        }
        // Anything that resolves (a row given by position, say) is kept as written.
        let layer = ttg_codegen::layers::project_for(&self.project, &self.catalog, provider);
        let plan = ttg_codegen::plan::Plan::build(&layer, &self.catalog, provider, None);
        match ttg_codegen::adopt::resolve_key(&plan, id, key) {
            ttg_codegen::adopt::KeyTarget::Resource(..) => Ok(key.to_string()),
            ttg_codegen::adopt::KeyTarget::Data(a) => Err(format!(
                "\"{key}\" is the data source {a}: data sources are read, not imported"
            )),
            ttg_codegen::adopt::KeyTarget::Ambiguous(rows) => Err(format!(
                "\"{key}\" is repeated once per row; adopt one instance: {}",
                rows.join(", ")
            )),
            ttg_codegen::adopt::KeyTarget::Unknown => Err(format!(
                "\"{key}\" names no block this entity generates on {provider} ({})",
                if blocks.is_empty() {
                    "it generates none there".to_string()
                } else {
                    blocks
                        .iter()
                        .map(|(k, a)| format!("{k} = {a}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            )),
        }
    }

    /// `(adopt key, address)` of every resource block an entity generates on `provider`.
    pub fn adoptable(&self, id: &str, provider: &str) -> Vec<(String, String)> {
        let layer = ttg_codegen::layers::project_for(&self.project, &self.catalog, provider);
        let plan = ttg_codegen::plan::Plan::build(&layer, &self.catalog, provider, None);
        ttg_codegen::adopt::adoptable(&plan, id)
    }

    // ------------------------------------------------------------------ directory import

    /// What importing an edited export directory would do; nothing changes yet.
    pub fn import_dir_preview(
        &mut self,
        dir: &Path,
        provider: Option<&str>,
    ) -> Result<ImportPreview, String> {
        let mut cat = self.catalog.clone();
        let result = ttg_codegen::import_dir::import_dir(&self.project, &mut cat, dir, provider)?;
        let diff = ttg_core::arch_diff::diff(&self.project, &result.project);
        let label = dir.display().to_string();
        let markdown = diff.to_markdown("diagram", &format!("imported from {label}"), &|t| {
            self.type_label(t)
        });
        Ok(ImportPreview {
            dir: dir.to_path_buf(),
            base: self.project.clone(),
            result,
            diff,
            markdown,
        })
    }

    /// Apply a preview as one undo step. Refused when the diagram changed since the
    /// preview was made (preview again).
    pub fn import_dir_apply(&mut self, preview: &ImportPreview) -> Result<(), String> {
        if self.project != preview.base {
            return Err("the diagram changed since the preview; preview the import again".into());
        }
        let before = self.snapshot();
        self.catalog.ensure_native_types(&preview.result.project);
        self.project = preview.result.project.clone();
        self.selection = preview.result.added.iter().map(|a| a.id.clone()).collect();
        self.finish(before);
        self.refresh_diagnostics();
        self.status = format!(
            "Imported {}: {} override(s), {} new entit{} (Undo takes it back)",
            preview.dir.display(),
            preview.result.overrides.len(),
            preview.result.added.len(),
            if preview.result.added.len() == 1 {
                "y"
            } else {
                "ies"
            }
        );
        Ok(())
    }
}

/// An import worked out and waiting for the user (or agent) to apply it.
pub struct ImportPreview {
    pub dir: PathBuf,
    /// The diagram it was computed from.
    pub base: Project,
    pub result: ttg_codegen::import_dir::DirImport,
    pub diff: ProjectDiff,
    /// The architecture diff as Markdown.
    pub markdown: String,
}

// ------------------------------------------------------------------ patterns

/// Where saved patterns live: `TTG_PATTERNS_DIR`, else `patterns` beside eframe's data
/// folder for this app.
pub fn user_patterns_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("TTG_PATTERNS_DIR") {
        return Some(PathBuf::from(d));
    }
    let data = eframe::storage_dir("TerraTofu GUI")?;
    Some(if data.file_name().is_some_and(|n| n == "data") {
        data.parent()
            .map(|p| p.join("patterns"))
            .unwrap_or_else(|| data.join("patterns"))
    } else {
        data.join("patterns")
    })
}

/// A pattern and where it came from.
#[derive(Debug, Clone)]
pub struct PatternEntry {
    pub pattern: Pattern,
    /// `built-in`, or the file it was read from.
    pub source: String,
}

impl PatternEntry {
    pub fn builtin(&self) -> bool {
        self.source == "built-in"
    }
}

/// Built-in patterns, then the user's, plus the files that could not be read.
pub fn all_patterns() -> (Vec<PatternEntry>, Vec<String>) {
    let mut out: Vec<PatternEntry> = ttg_codegen::patterns::builtins()
        .into_iter()
        .map(|pattern| PatternEntry {
            pattern,
            source: "built-in".into(),
        })
        .collect();
    let mut problems = Vec::new();
    if let Some(dir) = user_patterns_dir() {
        let (ok, bad) = ttg_codegen::patterns::load_dir(&dir);
        for (file, pattern) in ok {
            out.push(PatternEntry {
                pattern,
                source: dir.join(file).display().to_string(),
            });
        }
        problems = bad;
    }
    (out, problems)
}

/// The pattern called `name` (case and punctuation ignored).
#[cfg_attr(not(feature = "mcp"), allow(dead_code))]
pub fn find_pattern(name: &str) -> Result<PatternEntry, String> {
    let (all, _) = all_patterns();
    all.iter()
        .find(|p| p.pattern.is_named(name))
        .cloned()
        .ok_or_else(|| {
            format!(
                "no pattern named \"{name}\" (patterns: {})",
                all.iter()
                    .map(|p| p.pattern.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// Write a pattern into the user's folder. A built-in's name is refused, and so is an
/// existing user pattern's unless `replace`.
pub fn save_pattern(pattern: &Pattern, replace: bool) -> Result<PathBuf, String> {
    if pattern.name.trim().is_empty() {
        return Err("a pattern needs a name".into());
    }
    if ttg_codegen::patterns::builtins()
        .iter()
        .any(|b| b.is_named(&pattern.name))
    {
        return Err(format!(
            "\"{}\" is a built-in pattern's name; choose another",
            pattern.name
        ));
    }
    let dir = user_patterns_dir()
        .ok_or("there is no folder for user patterns on this system (set TTG_PATTERNS_DIR)")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join(ttg_codegen::patterns::file_name(&pattern.name));
    if path.exists() && !replace {
        return Err(format!(
            "a pattern named \"{}\" exists already ({}); pass replace: true to overwrite it",
            pattern.name,
            path.display()
        ));
    }
    std::fs::write(&path, ttg_codegen::patterns::to_file_text(pattern))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_sits_beside_the_project() {
        assert_eq!(
            sidecar_path(Path::new("a/b/shop.ttg.json")),
            Path::new("a/b/shop.checkpoints.json")
        );
        assert_eq!(sidecar_path(Path::new("x.json")), Path::new("x.checkpoints.json"));
        assert_eq!(format_time(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_time(1_791_041_100), "2026-10-03 15:25 UTC");
    }
}
