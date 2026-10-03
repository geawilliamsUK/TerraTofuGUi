//! Environments and plans as the agent sees them: the environment keys of
//! `settings_set`, `entity_update { environment }`, link environment tags, and the two
//! halves of `plan_run` that need the UI thread (the export and its attribution before,
//! the canvas badges after; the plan itself runs in the server, off this thread).

use super::{gen_error_text, R};
use crate::app::TtgApp;
use crate::mcp::Selector;
use serde_json::{json, Map, Value as J};
use ttg_core::{Relation, Value};

impl TtgApp {
    pub(super) fn env_settings(
        &mut self,
        environments: Option<Vec<String>>,
        rename: Option<(String, String)>,
        name_prefix: Option<J>,
        variables: Option<Map<String, J>>,
    ) -> R {
        // Everything is checked on a copy first, so a refused call changes nothing.
        let mut p = self.project.clone();
        let mut lost = Vec::new();
        if let Some((from, to)) = &rename {
            p.rename_environment(from, to)?;
        }
        if let Some(list) = environments {
            lost = p.set_environments(list)?;
        }
        match name_prefix {
            None => {}
            Some(J::Null) => p.settings.name_prefix = None,
            Some(J::String(s)) if s.trim().is_empty() => p.settings.name_prefix = None,
            Some(J::String(s)) => p.settings.name_prefix = Some(s.trim().to_string()),
            Some(other) => return Err(format!("name_prefix must be a string or null, not {other}")),
        }
        for (name, v) in variables.unwrap_or_default() {
            if v.is_null() {
                p.settings.variables.remove(&name);
                continue;
            }
            ttg_core::environment::check_variable_name(&name)?;
            let var = match v {
                // `{ "value": …, "environments": { "prod": … }, "description": "…" }`
                J::Object(o) if o.contains_key("value") || o.contains_key("environments") => {
                    let mut var =
                        p.settings
                            .variables
                            .get(&name)
                            .cloned()
                            .unwrap_or(ttg_core::ProjectVariable {
                                value: Value::Str(String::new()),
                                description: String::new(),
                                environments: Default::default(),
                            });
                    if let Some(x) = o.get("value") {
                        var.value = to_value(&name, x)?;
                    }
                    if let Some(d) = o.get("description").and_then(|d| d.as_str()) {
                        var.description = d.to_string();
                    }
                    if let Some(J::Object(per)) = o.get("environments") {
                        for (env, x) in per {
                            if !p.settings.environments.contains(env) {
                                return Err(format!(
                                    "variable \"{name}\": no environment \"{env}\" (environments: {})",
                                    p.settings.environments.join(", ")
                                ));
                            }
                            if x.is_null() {
                                var.environments.remove(env);
                            } else {
                                var.environments.insert(env.clone(), to_value(&name, x)?);
                            }
                        }
                    }
                    var
                }
                // A bare value: the base value.
                other => {
                    let mut var =
                        p.settings
                            .variables
                            .get(&name)
                            .cloned()
                            .unwrap_or(ttg_core::ProjectVariable {
                                value: Value::Str(String::new()),
                                description: String::new(),
                                environments: Default::default(),
                            });
                    var.value = to_value(&name, &other)?;
                    var
                }
            };
            p.settings.variables.insert(name, var);
        }
        let before = self.snapshot();
        self.project = p;
        self.finish(before);
        self.check_environment();
        let s = &self.project.settings;
        Ok(json!({
            "status": "settings updated",
            "environments": s.environments,
            "name_prefix": s.name_prefix,
            "variables": s.variables,
            "overrides_removed_from": lost,
        }))
    }

    /// `entity_update { environment, … }`.
    pub(super) fn env_update(
        &mut self,
        entity: Option<String>,
        select: Option<Selector>,
        environment: String,
        config: Option<Map<String, J>>,
        provider_config: Option<Map<String, J>>,
        present: Option<bool>,
    ) -> R {
        if !self.project.settings.environments.contains(&environment) {
            return Err(if self.project.settings.environments.is_empty() {
                format!(
                    "the project has no environments; add them first (settings_set {{ environments: [\"{environment}\", …] }})"
                )
            } else {
                format!(
                    "no environment \"{environment}\" (environments: {})",
                    self.project.settings.environments.join(", ")
                )
            });
        }
        let ids = match (entity, select) {
            (Some(e), None) => vec![self.resolve(&e)?],
            (None, Some(s)) => {
                if s.is_empty() {
                    return Err(
                        "an empty selection would match every entity; give types, name_glob or ids".into(),
                    );
                }
                self.select_entities(&s)?
            }
            _ => return Err("give `entity` or `select`".into()),
        };
        // Validate every value for every entity before touching the project.
        let mut writes: Vec<(String, Vec<FieldWrite>)> = Vec::new();
        let mut refused: Vec<String> = Vec::new();
        for id in &ids {
            let type_id = self.project.entity(id).unwrap().resource_type.to_string();
            let Some(def) = self.catalog.resource(&type_id).cloned() else {
                refused.push(format!("{id}: unknown type {type_id}"));
                continue;
            };
            let mut w = Vec::new();
            for (k, v) in config.iter().flatten() {
                match def.fields.iter().find(|f| &f.name == k) {
                    None => refused.push(format!("{id}: {type_id} has no field \"{k}\"")),
                    Some(f) => match self.convert_value(f, v) {
                        Ok(val) => w.push((None, k.clone(), val)),
                        Err(e) => refused.push(format!("{id}: {e}")),
                    },
                }
            }
            for (pid, vals) in provider_config.iter().flatten() {
                let Some(m) = def.providers.get(pid) else {
                    refused.push(format!("{id}: {type_id} has no {pid} mapping"));
                    continue;
                };
                let J::Object(vals) = vals else {
                    refused.push(format!("provider_config.{pid} must be an object"));
                    continue;
                };
                for (k, v) in vals {
                    match m.fields.iter().find(|f| &f.name == k) {
                        None => refused.push(format!("{id}: {type_id}/{pid} has no field \"{k}\"")),
                        Some(f) => match self.convert_value(f, v) {
                            Ok(val) => w.push((Some(pid.clone()), k.clone(), val)),
                            Err(e) => refused.push(format!("{id}: {e}")),
                        },
                    }
                }
            }
            writes.push((id.clone(), w));
        }
        if !refused.is_empty() {
            return Err(format!("nothing changed:\n- {}", refused.join("\n- ")));
        }
        let before = self.snapshot();
        for (id, w) in &writes {
            let o = self
                .project
                .overrides_mut(id)
                .unwrap()
                .entry(environment.clone())
                .or_default();
            for (pid, k, val) in w {
                let map = match pid {
                    Some(p) => o.provider_config.entry(p.clone()).or_default(),
                    None => &mut o.config,
                };
                match val {
                    Some(v) => {
                        map.insert(k.clone(), v.clone());
                    }
                    // null drops this environment's value: back to the base.
                    None => {
                        map.remove(k);
                    }
                }
            }
            if let Some(p) = present {
                o.absent = !p;
            }
            self.project.prune_overrides(id);
            self.flash(id);
        }
        self.finish(before);
        self.refresh_diagnostics();
        let shown = self.project.for_environment(&environment);
        let diags =
            ttg_codegen::diagnostics::run(&shown, &self.catalog, &self.project.settings.target_provider);
        Ok(json!({
            "status": "updated",
            "environment": environment,
            "entities": writes.iter().map(|(id, _)| json!({
                "id": id,
                "name": self.project.entity(id).map(|e| e.name.to_string()),
                "overrides": self.project.overrides_of(id).and_then(|o| o.get(&environment)),
                "diagnostics": diags.iter().filter(|d| d.entity.as_deref() == Some(id.as_str())).map(|d| format!("{:?}: {}", d.severity, d.message)).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        }))
    }

    pub(super) fn link_environments(
        &mut self,
        source: String,
        target: String,
        relation: String,
        environments: Vec<String>,
    ) -> R {
        let s = self.resolve(&source)?;
        let t = self.resolve(&target)?;
        let rel = Relation::from_key(&relation).ok_or_else(|| format!("unknown relation \"{relation}\""))?;
        for e in &environments {
            if !self.project.settings.environments.contains(e) {
                return Err(format!(
                    "no environment \"{e}\" (environments: {})",
                    self.project.settings.environments.join(", ")
                ));
            }
        }
        let Some(i) = self
            .project
            .edges
            .iter()
            .position(|x| x.source == s && x.target == t && x.relation == rel)
        else {
            return Err("no such link".into());
        };
        let before = self.snapshot();
        self.project.edges[i].environments = environments.clone();
        self.finish(before);
        Ok(json!({"status": "linked", "environments": environments}))
    }

    /// Export for a plan and say what belongs to whom; the server plans off-thread.
    pub(super) fn plan_prepare(
        &mut self,
        dir: Option<String>,
        provider: Option<String>,
        environment: Option<String>,
        tool: Option<ttg_core::Tool>,
    ) -> R {
        let provider = provider.unwrap_or(self.project.settings.target_provider.clone());
        let tool = tool.unwrap_or(self.project.settings.tool);
        let dir = dir.unwrap_or_else(|| {
            std::env::temp_dir()
                .join("terratofu-plan")
                .join(ttg_core::slugify(&self.project.name))
                .join(&provider)
                .to_string_lossy()
                .to_string()
        });
        let env = ttg_codegen::plan_run::environment_for(&self.project, environment.as_deref())?;
        let a = ttg_codegen::plan_run::attribution_for(&self.project, &self.catalog, &provider, tool)
            .map_err(|e| gen_error_text(&e))?;
        let rep = ttg_codegen::export(
            &self.project,
            &self.catalog,
            &provider,
            tool,
            std::path::Path::new(&dir),
        )
        .map_err(|e| gen_error_text(&e))?;
        Ok(json!({
            "provider": provider,
            "tool": tool,
            "dir": rep.out_dir,
            "environment": env,
            "var_file": env.as_ref().map(|e| format!("{}/{e}.tfvars", ttg_codegen::environments::DIR)),
            "backend_config": env.as_ref().map(|e| format!("{}/{e}.backend.hcl", ttg_codegen::environments::DIR)),
            "attribution": a,
        }))
    }

    pub(super) fn attribution_json(&mut self, provider: Option<String>) -> R {
        let provider = provider.unwrap_or(self.project.settings.target_provider.clone());
        let a = ttg_codegen::plan_run::attribution_for(
            &self.project,
            &self.catalog,
            &provider,
            self.project.settings.tool,
        )
        .map_err(|e| gen_error_text(&e))?;
        serde_json::to_value(a).map_err(|e| e.to_string())
    }

    pub(super) fn plan_show(&mut self, report: J) -> R {
        let r: ttg_codegen::plan_run::PlanReport =
            serde_json::from_value(report).map_err(|e| e.to_string())?;
        let n = r.entities.len();
        self.show_plan(Ok(r));
        Ok(json!({"status": "shown", "entities": n}))
    }
}

/// One value to write: provider (None = abstract), field, value (None = drop it).
type FieldWrite = (Option<String>, String, Option<Value>);

fn to_value(name: &str, v: &J) -> Result<Value, String> {
    serde_json::from_value(v.clone()).map_err(|e| format!("variable \"{name}\": {e}"))
}
