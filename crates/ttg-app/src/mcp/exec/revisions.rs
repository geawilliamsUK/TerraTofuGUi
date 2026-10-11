//! `project_diff`, `checkpoint_*`, `pattern_*`, `entity_update { adopt }` and
//! `project_import_dir`. The logic lives in `crate::revisions` (shared with the GUI);
//! these are the agent's replies.

use super::{AgentCommand, R};
use crate::app::TtgApp;
use serde_json::{json, Map, Value as J};
use ttg_core::Position;

/// Every pattern, built-in and saved, with its parameters.
pub(super) fn pattern_list_json() -> J {
    let (all, problems) = crate::revisions::all_patterns();
    json!({
        "patterns": all.iter().map(|e| json!({
            "name": e.pattern.name,
            "description": e.pattern.description,
            "source": e.source,
            "builtin": e.builtin(),
            "params": e.pattern.params,
            "entities": e.pattern.entities.iter().map(|x| json!({"key": x.key, "type": x.type_id, "name": x.name, "when": x.when})).collect::<Vec<_>>(),
            "links": e.pattern.links.len(),
        })).collect::<Vec<_>>(),
        "user_folder": crate::revisions::user_patterns_dir(),
        "unreadable": problems,
    })
}

impl TtgApp {
    pub(super) fn project_diff_json(&mut self, against: &str, format: Option<&str>) -> R {
        let (label, d, md) = self.project_diff(against)?;
        let mut out = json!({
            "against": label,
            "summary": d.summary(),
            "changed": !d.is_empty(),
        });
        match format.map(str::trim) {
            None | Some("") | Some("both") => {
                out["diff"] = serde_json::to_value(&d).map_err(|e| e.to_string())?;
                out["markdown"] = J::String(md);
            }
            Some("json") => out["diff"] = serde_json::to_value(&d).map_err(|e| e.to_string())?,
            Some("markdown") | Some("md") => out["markdown"] = J::String(md),
            Some(other) => return Err(format!("format \"{other}\": use json, markdown or both")),
        }
        Ok(out)
    }

    pub(super) fn checkpoint_create_json(&mut self, name: &str, replace: bool) -> R {
        let info = self.checkpoint_create(name, replace)?;
        Ok(json!({
            "status": "created",
            "checkpoint": info,
            "stored": self.checkpoint_storage(),
            "note": "The diagram itself did not change. checkpoint_restore brings this state back as one undo step; project_diff { against: name } shows what changed since.",
        }))
    }

    pub(super) fn checkpoint_list_json(&mut self) -> R {
        Ok(json!({
            "checkpoints": self.checkpoint_list()?,
            "stored": self.checkpoint_storage(),
        }))
    }

    pub(super) fn checkpoint_restore_json(&mut self, name: &str) -> R {
        let d = self.checkpoint_restore(name)?;
        Ok(json!({
            "status": "restored",
            "checkpoint": name,
            "summary": d.summary(),
            "diff": d,
            "note": "One undo brings the diagram from before the restore back.",
        }))
    }

    /// Insert a pattern: its expansion runs as one batch of ordinary write commands, so
    /// every value, parent and link is checked as `project_apply` checks it, and the
    /// whole insert is one undo step (nothing is left behind when a command fails).
    pub(crate) fn apply_pattern(
        &mut self,
        name: &str,
        params: &Map<String, J>,
        x: Option<i32>,
        y: Option<i32>,
        parent: Option<&str>,
    ) -> R {
        let entry = crate::revisions::find_pattern(name)?;
        let pid = parent.map(|p| self.resolve(p)).transpose()?;
        if let Some(p) = &pid {
            if !self.project.containers.contains_key(p) {
                return Err(format!("\"{p}\" is not a container"));
            }
        }
        let spot = self.free_spot_in(pid.as_deref());
        let origin = Position {
            x: x.unwrap_or(spot.x),
            y: y.unwrap_or(spot.y),
        };
        let expansion = ttg_codegen::patterns::expand(
            &entry.pattern,
            params,
            &self.project,
            &self.catalog,
            origin,
            pid.as_deref(),
        )
        .map_err(|e| format!("pattern \"{}\": {e}", entry.pattern.name))?;
        let mut cmds = Vec::new();
        for (tool, args) in expansion.steps {
            cmds.push(crate::mcp::server::command_from_json(&tool, args)?);
        }
        let links = cmds
            .iter()
            .filter(|c| matches!(c, AgentCommand::LinkAdd { .. }))
            .count();
        let before = self.snapshot();
        let start = self.history.len();
        let results = self
            .run_batch(cmds, &before, start)
            .map_err(|e| format!("pattern \"{}\": {e}", entry.pattern.name))?;
        let ids: Vec<String> = expansion
            .entities
            .iter()
            .filter_map(|(_, n, _)| self.resolve(n).ok())
            .collect();
        self.grow_containers_to_fit(&ids);
        self.history.truncate(start);
        self.finish(before);
        self.selection = ids.iter().cloned().collect();
        for id in &ids {
            self.flash(id);
        }
        self.refresh_diagnostics();
        let diagnostics: Vec<String> = self
            .diagnostics
            .iter()
            .filter(|d| d.entity.as_ref().is_some_and(|e| ids.contains(e)))
            .filter(|d| d.severity != ttg_codegen::Severity::Info)
            .map(|d| {
                format!(
                    "{:?} [{}]: {}",
                    d.severity,
                    d.entity.as_deref().unwrap_or(""),
                    d.message
                )
            })
            .collect();
        self.status = format!("Inserted pattern \"{}\"", entry.pattern.name);
        Ok(json!({
            "status": "applied",
            "pattern": entry.pattern.name,
            "entities": expansion.entities.iter().map(|(k, n, t)| json!({
                "key": k,
                "name": n,
                "type": t,
                "id": self.resolve(n).ok(),
            })).collect::<Vec<_>>(),
            "links": links,
            "commands": results.len(),
            "diagnostics": diagnostics,
            "note": "One undo step takes the whole pattern back.",
        }))
    }

    /// Grow the containers holding `ids` (outwards) until each holds its contents with
    /// a margin. Containers only grow.
    fn grow_containers_to_fit(&mut self, ids: &[String]) {
        const PAD: i32 = 24;
        let mut todo: Vec<String> = ids
            .iter()
            .filter_map(|id| self.project.parent_of(id).map(str::to_string))
            .collect();
        todo.sort();
        todo.dedup();
        // Inner containers first, so an outer one grows around an inner one's new size.
        todo.sort_by_key(|c| std::cmp::Reverse(self.project.depth_of(c)));
        let mut seen = std::collections::HashSet::new();
        while let Some(c) = todo.first().cloned() {
            todo.remove(0);
            if !seen.insert(c.clone()) {
                continue;
            }
            let children = self.project.children_of(&c);
            let Some(cont) = self.project.containers.get(&c).cloned() else {
                continue;
            };
            let (mut max_x, mut max_y) = (cont.position.x + cont.size.w, cont.position.y + cont.size.h);
            let (mut min_x, mut min_y) = (cont.position.x, cont.position.y);
            for ch in &children {
                let Some(e) = self.project.entity(ch) else {
                    continue;
                };
                let size = match self.project.containers.get(ch) {
                    Some(cc) => cc.size,
                    None => self.project.nodes[ch].size.unwrap_or(ttg_core::NODE_SIZE),
                };
                min_x = min_x.min(e.position.x - PAD);
                min_y = min_y.min(e.position.y - PAD * 2);
                max_x = max_x.max(e.position.x + size.w + PAD);
                max_y = max_y.max(e.position.y + size.h + PAD);
            }
            if let Some(cm) = self.project.containers.get_mut(&c) {
                cm.position = Position { x: min_x, y: min_y };
                cm.size = ttg_core::Size {
                    w: max_x - min_x,
                    h: max_y - min_y,
                };
            }
            if let Some(p) = self.project.parent_of(&c) {
                todo.push(p.to_string());
            }
        }
    }

    pub(super) fn pattern_save_json(
        &mut self,
        name: &str,
        entities: &[String],
        description: Option<&str>,
        replace: bool,
    ) -> R {
        let ids = entities
            .iter()
            .map(|e| self.resolve(e))
            .collect::<Result<Vec<_>, _>>()?;
        let pattern = ttg_codegen::patterns::infer(&self.project, &ids, name, description.unwrap_or(""))?;
        let problems = ttg_codegen::patterns::check(&pattern, &self.catalog);
        if !problems.is_empty() {
            return Err(format!(
                "the pattern would not apply cleanly: {}",
                problems.join("; ")
            ));
        }
        let path = crate::revisions::save_pattern(&pattern, replace)?;
        Ok(json!({
            "status": "saved",
            "pattern": pattern.name,
            "path": path,
            "params": pattern.params,
            "entities": pattern.entities.len(),
            "links": pattern.links.len(),
        }))
    }

    /// `entity_update { adopt }`. Values are cloud ids (`null` or `""` removes one);
    /// keys are block keys, `<block>/<row key>` or addresses, for `provider` (default:
    /// the target), or the map is keyed by provider id first.
    pub(super) fn entity_adopt_json(
        &mut self,
        entity: &str,
        adopt: Map<String, J>,
        provider: Option<String>,
        update: Option<Box<AgentCommand>>,
    ) -> R {
        let id = self.resolve(entity)?;
        let providers = self.catalog.provider_ids();
        let by_provider = !adopt.is_empty()
            && adopt
                .iter()
                .all(|(k, v)| providers.contains(k) && (v.is_object() || v.is_null()));
        let mut entries: Vec<(String, String, Option<String>)> = Vec::new();
        let target = provider.unwrap_or(self.project.settings.target_provider.clone());
        let value = |v: &J| -> Result<Option<String>, String> {
            match v {
                J::Null => Ok(None),
                J::String(s) if s.trim().is_empty() => Ok(None),
                J::String(s) => Ok(Some(s.trim().to_string())),
                other => Err(format!("an adopted resource's id is text, not {other}")),
            }
        };
        if by_provider {
            for (p, m) in &adopt {
                for (k, v) in m.as_object().cloned().unwrap_or_default() {
                    entries.push((p.clone(), k, value(&v)?));
                }
            }
        } else {
            if self.catalog.provider(&target).is_none() {
                return Err(format!("unknown provider \"{target}\""));
            }
            for (k, v) in &adopt {
                entries.push((target.clone(), k.clone(), value(v)?));
            }
        }
        // Every key checked before anything changes.
        for (p, k, _) in &entries {
            self.adopt_key(&id, p, k)?;
        }
        let before = self.snapshot();
        let start = self.history.len();
        let mut reply = match update {
            Some(cmd) => match self.agent_exec(*cmd) {
                Ok(r) => r,
                Err(e) => {
                    self.project = before;
                    self.history.truncate(start);
                    return Err(e);
                }
            },
            None => json!({}),
        };
        for (p, k, v) in &entries {
            if let Err(e) = self.set_adopt(&id, p, k, v.as_deref()) {
                self.project = before;
                self.history.truncate(start);
                self.diag_dirty = true;
                return Err(e);
            }
        }
        self.history.truncate(start);
        self.finish(before);
        self.flash(&id);
        self.refresh_diagnostics();
        let warnings: Vec<String> = self
            .diagnostics
            .iter()
            .filter(|d| d.entity.as_deref() == Some(id.as_str()) && d.code == ttg_codegen::Code::Adopt)
            .map(|d| d.message.clone())
            .collect();
        let adopted = self.project.adoption(&id).cloned().unwrap_or_default();
        let mut imports = Vec::new();
        for (p, m) in &adopted {
            let blocks = self.adoptable(&id, p);
            for (k, cloud) in m {
                let address = blocks.iter().find(|(bk, _)| bk == k).map(|(_, a)| a.clone());
                imports.push(json!({"provider": p, "key": k, "address": address, "id": cloud}));
            }
        }
        if !reply.is_object() {
            reply = json!({});
        }
        reply["status"] = J::String("updated".into());
        reply["entity"] = self.entity_json(&id);
        reply["entity"]["adopt"] = serde_json::to_value(&adopted).unwrap_or(J::Null);
        reply["imports"] = J::Array(imports);
        reply["adopt_warnings"] = json!(warnings);
        Ok(reply)
    }

    pub(super) fn import_dir_json(&mut self, dir: &str, provider: Option<&str>, apply: bool) -> R {
        let preview = self.import_dir_preview(std::path::Path::new(dir), provider)?;
        let mut out = json!({
            "dir": dir,
            "provider": preview.result.provider,
            "summary": preview.diff.summary(),
            "report": preview.result.report_json(),
            "diff": preview.diff,
            "markdown": format!("{}\n{}", preview.markdown, preview.result.report_markdown()),
        });
        if apply {
            self.import_dir_apply(&preview)?;
            out["status"] = J::String("applied".into());
            out["applied"] = J::Bool(true);
            out["note"] = J::String(
                "Applied as one undo step. Delete the hand-written files listed under hand_written_files: the export now writes their blocks.".into(),
            );
        } else {
            out["status"] = J::String("preview".into());
            out["applied"] = J::Bool(false);
            out["note"] = J::String(
                "Nothing changed. Show the user the markdown, then call project_import_dir again with apply: true.".into(),
            );
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use ttg_codegen::validate::{find_binary, run, Outcome};
    use ttg_core::Tool;

    fn app_with(example: &str) -> TtgApp {
        let ctx = egui::Context::default();
        let mut app = TtgApp::build(&ctx, None, None, None);
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples")
            .join(example);
        app.open_path(path);
        assert!(app.error.is_none(), "{:?}", app.error);
        app
    }

    fn rg(app: &TtgApp) -> String {
        app.project
            .containers
            .values()
            .find(|c| c.container_type == "resource_group")
            .map(|c| c.id.clone())
            .expect("a resource group")
    }

    fn errors(app: &TtgApp, provider: &str) -> Vec<String> {
        ttg_codegen::diagnostics::run(&app.project, &app.catalog, provider)
            .into_iter()
            .filter(|d| d.severity == ttg_codegen::Severity::Error)
            .map(|d| d.to_string())
            .collect()
    }

    /// Each built-in pattern, applied to an example that exports cleanly on every
    /// provider, leaves it clean on every provider (and, with a binary installed, every
    /// export validates); one undo takes the whole pattern back.
    #[test]
    fn builtin_patterns_apply_cleanly_on_every_provider() {
        let validator = [Tool::OpenTofu, Tool::Terraform]
            .into_iter()
            .find(|t| find_binary(*t).is_some());
        if validator.is_none() && std::env::var("TTG_REQUIRE_VALIDATE").is_ok() {
            panic!("TTG_REQUIRE_VALIDATE is set but neither tofu nor terraform is installed");
        }
        let cases: Vec<(&str, &str, J)> = vec![
            (
                "hardened.ttg.json",
                "hardened bucket",
                json!({"name": "uploads", "bucket_name": "ttg-wp-uploads-x91"}),
            ),
            (
                "hardened.ttg.json",
                "hardened bucket",
                json!({"name": "exports", "bucket_name": "ttg-wp-exports-x91", "key": "data key"}),
            ),
            (
                "operations.ttg.json",
                "queue with dead-letter queue and alarms",
                json!({"name": "orders", "topic": "ops alerts"}),
            ),
            (
                "operations.ttg.json",
                "queue with dead letter queue and alarms",
                json!({"name": "invoices"}),
            ),
            (
                "kubernetes.ttg.json",
                "queue-driven GPU worker",
                json!({"name": "ocr", "cluster": "platform"}),
            ),
            (
                "kubernetes.ttg.json",
                "queue driven gpu worker",
                json!({"name": "embedder", "cluster": "platform", "queue": "jobs"}),
            ),
            (
                "containers.ttg.json",
                "web service behind a load balancer",
                json!({"name": "admin", "subnets": ["sub-pub-a", "sub-pub-b"], "app_subnets": ["sub-app-a"], "certificate": "shop cert", "environment": "apps", "topic": "alerts"}),
            ),
            (
                "containers.ttg.json",
                "web service behind a load balancer",
                json!({"name": "portal", "subnets": ["sub-pub-a", "sub-pub-b"], "app_subnets": ["sub-app-a", "sub-app-b"], "certificate": "shop cert"}),
            ),
            // A project with environments: the new entities are the same in each.
            (
                "three-tier.ttg.json",
                "queue with dead-letter queue and alarms",
                json!({"name": "orders"}),
            ),
        ];
        let root = std::env::temp_dir().join(format!("ttg-wp-patterns-{}", std::process::id()));
        for (i, (example, pattern, params)) in cases.into_iter().enumerate() {
            let mut app = app_with(example);
            for p in ["aws", "azure", "gcp"] {
                assert!(
                    errors(&app, p).is_empty(),
                    "{example} is not clean on {p} to start with"
                );
            }
            let before = app.project.clone();
            let parent = rg(&app);
            let r = app
                .apply_pattern(pattern, params.as_object().unwrap(), None, None, Some(&parent))
                .unwrap_or_else(|e| panic!("{pattern} on {example}: {e}"));
            assert_eq!(r["status"], "applied");
            for e in r["entities"].as_array().unwrap() {
                let id = e["id"].as_str().unwrap();
                assert!(
                    app.project.overrides_of(id).is_none_or(|o| o.is_empty()),
                    "{id} got environment overrides"
                );
            }
            for p in ["aws", "azure", "gcp"] {
                let e = errors(&app, p);
                assert!(e.is_empty(), "{pattern} on {example}, {p}: {e:#?}");
            }
            if let Some(t) = validator {
                for p in ["aws", "azure", "gcp"] {
                    let dir = root.join(format!("{i}-{p}"));
                    ttg_codegen::export(&app.project, &app.catalog, p, Tool::OpenTofu, &dir)
                        .unwrap_or_else(|e| panic!("{pattern} on {example}, {p}: {e}"));
                    match run(&dir, t) {
                        Outcome::Ran { success: true, .. } => {}
                        other => panic!("{pattern} on {example}, {p}: {}", other.summary()),
                    }
                }
            }
            app.undo();
            assert_eq!(app.project, before, "one undo takes {pattern} back");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failing_pattern_leaves_nothing_behind() {
        let mut app = app_with("minimal.ttg.json");
        let before = app.project.clone();
        let steps = app.history.len();
        // The cluster parameter names something that is not a cluster.
        let e = app
            .apply_pattern(
                "queue-driven GPU worker",
                json!({"cluster": "worker"}).as_object().unwrap(),
                None,
                None,
                None,
            )
            .unwrap_err();
        assert!(e.contains("is a function, not a kubernetes_cluster"), "{e}");
        assert_eq!(app.project, before);
        assert_eq!(app.history.len(), steps);
    }
}
