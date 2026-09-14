//! UI-thread execution of [`AgentCommand`]s. Everything here runs inside `update`, so
//! it can use the same helpers the mouse and menus use; every mutation goes through
//! `snapshot()` / `finish()` and is therefore undoable and immediately visible.

use super::AgentCommand;
use crate::app::TtgApp;
use egui::Pos2;
use serde_json::{json, Map, Value as J};
use std::time::Instant;
use ttg_catalog::{Catalog, FieldDef, FieldType, ResourceKind};
use ttg_core::{Id, Relation, Tool, Value};

type R = Result<J, String>;

impl TtgApp {
    /// Resolve an id or (case-insensitive) display name to an entity id.
    fn resolve(&self, key: &str) -> Result<Id, String> {
        if self.project.entity(key).is_some() {
            return Ok(key.to_string());
        }
        let mut hits: Vec<Id> = self
            .project
            .entities()
            .iter()
            .filter(|e| e.name.eq_ignore_ascii_case(key))
            .map(|e| e.id.to_string())
            .collect();
        match hits.len() {
            1 => Ok(hits.remove(0)),
            0 => Err(format!("no entity with id or name \"{key}\"")),
            n => Err(format!(
                "\"{key}\" is ambiguous ({n} entities share that name); use an id"
            )),
        }
    }

    fn flash(&mut self, id: &str) {
        self.mcp.flash.push((id.to_string(), Instant::now()));
    }

    fn entity_json(&self, id: &str) -> J {
        let Some(e) = self.project.entity(id) else {
            return J::Null;
        };
        json!({
            "id": e.id,
            "name": e.name,
            "type": e.resource_type,
            "container": e.is_container,
            "parent": e.parent,
            "position": { "x": e.position.x, "y": e.position.y },
            "manual": e.manual,
            "providers": if let Some(n) = self.project.nodes.get(id) { n.providers.clone() } else { self.project.containers[id].providers.clone() },
            "config": e.config,
            "provider_config": e.provider_config,
            "extra": e.extra,
        })
    }

    /// Index of a saved view by name (case-insensitive).
    fn resolve_view(&self, name: &str) -> Result<usize, String> {
        self.project
            .views
            .iter()
            .position(|v| v.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                format!(
                    "no view named \"{name}\" (views: {})",
                    self.project
                        .views
                        .iter()
                        .map(|v| v.name.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
    }

    /// Switch to a saved view, or to `All`.
    pub fn activate_view_named(&mut self, name: &str) -> Result<(), String> {
        if name.eq_ignore_ascii_case("all") {
            self.activate_view(None);
            return Ok(());
        }
        let i = self.resolve_view(name)?;
        self.activate_view(Some(i));
        Ok(())
    }

    /// Run a command against a named view without leaving the user somewhere else:
    /// the view is made active for the duration (annotations and the layout belong to
    /// whichever view is active) and the previous one comes back afterwards. With no
    /// name the active view is used, so `view` is optional everywhere.
    fn with_view<T>(
        &mut self,
        view: Option<String>,
        f: impl FnOnce(&mut Self) -> Result<T, String>,
    ) -> Result<T, String> {
        let Some(name) = view else { return f(self) };
        let i = self.resolve_view(&name)?;
        let previous = self.active_view;
        if previous == Some(i) {
            return f(self);
        }
        // Keep a working filter the user set on "All" rather than resetting it.
        let previous_filter = self.filter.clone();
        self.activate_view(Some(i));
        let r = f(self);
        self.active_view = previous;
        self.set_filter(previous_filter);
        r
    }

    /// The view an annotation command lands in, for its answer.
    fn view_name(&self) -> Option<String> {
        self.active_view().map(|v| v.name.clone())
    }

    fn require_view(&self, what: &str) -> Result<(), String> {
        if self.active_view.is_none() {
            return Err(format!(
                "{what} live in views: pass `view`, or call view_activate / view_save first"
            ));
        }
        Ok(())
    }

    /// Where a new annotation goes when the agent gives no position: clear of the
    /// diagram, so nothing lands on top of a resource.
    fn annotation_spot(&self, x: Option<i32>, y: Option<i32>) -> ttg_core::Position {
        let b = self.world_bounds();
        ttg_core::Position {
            x: x.unwrap_or_else(|| b.map(|r| r.max.x as i32 + 60).unwrap_or(60)),
            y: y.unwrap_or_else(|| b.map(|r| r.min.y as i32).unwrap_or(60)),
        }
    }

    /// One view in full: what it shows, where it puts things, and its annotations.
    fn view_json(&self, i: usize) -> J {
        let v = &self.project.views[i];
        let shown_set = ttg_codegen::views::visible_set(&self.project, &self.catalog, &v.filter);
        let visible = |id: &str| shown_set.as_ref().is_none_or(|s| s.contains(id));
        let name_of = |id: &str| {
            self.project
                .entity(id)
                .map(|e| e.name.to_string())
                .unwrap_or_else(|| id.to_string())
        };
        let shown: Vec<J> = self
            .project
            .entities()
            .iter()
            .filter(|e| visible(e.id))
            .map(|e| json!({"id": e.id, "name": e.name, "type": e.resource_type}))
            .collect();
        json!({
            "name": v.name,
            "description": v.description,
            "active": self.active_view == Some(i),
            "legend": v.legend,
            "filter": v.filter,
            "own_layout": v.layout.is_some(),
            "layout": v.layout,
            "shown": shown,
            "groups": v.groups.iter().map(|g| json!({
                "id": g.id, "label": g.label, "position": g.position, "size": g.size, "color": g.color,
                "nested_in": ttg_core::view::group_parent(v, &g.id).map(|p| p.label.clone()),
                "members": ttg_core::view::group_members(&self.project, v, &g.id, &visible)
                    .iter().map(|m| name_of(m)).collect::<Vec<_>>(),
                "logicals": ttg_core::view::group_logicals(v, &g.id).iter()
                    .filter_map(|l| v.logical(l).map(|l| l.name.clone())).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "flows": v.flows_in_step_order().iter().map(|f| json!({
                "id": f.id, "step": f.step, "label": f.label, "dashed": f.dashed, "color": f.color,
                "from": ttg_core::view::end_name(&self.project, v, &f.from),
                "to": ttg_core::view::end_name(&self.project, v, &f.to),
            })).collect::<Vec<_>>(),
            "notes": v.notes.iter().map(|n| json!({
                "id": n.id, "title": n.title, "body": n.body, "position": n.position, "size": n.size,
                "anchor": n.anchor,
            })).collect::<Vec<_>>(),
            "logicals": v.logicals.iter().map(|l| json!({
                "id": l.id, "name": l.name, "icon": l.icon, "subtitle": l.subtitle,
                "position": l.position, "size": l.size,
            })).collect::<Vec<_>>(),
        })
    }

    /// Resolve what a note points at: a resource, group or logical node, or a flow.
    fn resolve_anchor(&self, key: &str) -> Result<ttg_core::NoteAnchor, String> {
        if let Some(end) = self.resolve_end(key) {
            return Ok(ttg_core::NoteAnchor::End(end));
        }
        self.active_view()
            .and_then(|v| {
                v.flows
                    .iter()
                    .find(|f| f.id == key || (!f.label.is_empty() && f.label.eq_ignore_ascii_case(key)))
            })
            .map(|f| ttg_core::NoteAnchor::Flow { flow: f.id.clone() })
            .ok_or_else(|| format!("nothing called \"{key}\" to pin the note to in this view"))
    }

    fn modal_open(&self) -> Option<&'static str> {
        if self.confirm.is_some() {
            Some("the unsaved-changes prompt")
        } else if self.pending_edge.is_some() {
            Some("the connect dialog")
        } else if self.view_edit.is_some() {
            Some("the view-name prompt")
        } else {
            None
        }
    }

    pub fn agent_exec(&mut self, cmd: AgentCommand) -> R {
        if cmd.is_write() {
            if let Some(m) = self.modal_open() {
                return Err(format!("{m} is open; ask the user to close it first"));
            }
        }
        match cmd {
            AgentCommand::ProjectGet => serde_json::to_value(&self.project).map_err(|e| e.to_string()),
            AgentCommand::ProjectSummary => Ok(self.summary_json()),
            AgentCommand::CatalogTypes => Ok(self.catalog_types_json()),
            AgentCommand::CatalogType { type_id } => {
                if Catalog::is_native(&type_id) {
                    self.catalog.ensure_native(&type_id);
                }
                self.catalog_type_json(&type_id)
            }
            AgentCommand::Diagnostics => {
                self.refresh_diagnostics();
                Ok(self.diagnostics_json())
            }
            AgentCommand::ReachPosture => Ok(self.reach_posture_json()),
            AgentCommand::ReachFrom { entity } => {
                let id = self.resolve(&entity)?;
                Ok(self.reach_from_json(&id))
            }
            AgentCommand::ReachTo { entity } => {
                let id = self.resolve(&entity)?;
                Ok(self.reach_to_json(&id))
            }
            AgentCommand::ExportPreview { provider } => self.export_preview_json(provider),
            AgentCommand::Screenshot { .. } => Err("screenshots are handled by the frame loop".into()),
            AgentCommand::EntityAdd {
                type_id,
                name,
                parent,
                x,
                y,
                providers,
            } => {
                self.check_providers(providers.as_deref())?;
                let r = self.agent_entity_add(&type_id, name, parent, x, y)?;
                if let Some(tags) = providers {
                    let id = r["id"].as_str().unwrap_or("").to_string();
                    if let Some(n) = self.project.nodes.get_mut(&id) {
                        n.providers = tags;
                    } else if let Some(c) = self.project.containers.get_mut(&id) {
                        c.providers = tags;
                    }
                    self.diag_dirty = true;
                    return Ok(json!({"status": "added", "id": id, "entity": self.entity_json(&id)}));
                }
                Ok(r)
            }
            AgentCommand::EntityUpdate {
                entity,
                name,
                config,
                provider_config,
                manual,
                providers,
                extra,
                extra_provider,
                extra_block,
            } => {
                self.check_providers(providers.as_deref())?;
                let r = self.agent_entity_update(&entity, name, config, provider_config, manual)?;
                if let Some(extra) = extra {
                    let id = self.resolve(&entity)?;
                    let prov = extra_provider.unwrap_or(self.project.settings.target_provider.clone());
                    let type_id = self.project.entity(&id).unwrap().resource_type.to_string();
                    let m = self
                        .catalog
                        .mapping(&type_id, &prov)
                        .ok_or_else(|| format!("{type_id} has no {prov} mapping"))?;
                    let block = extra_block.unwrap_or_else(|| {
                        m.blocks
                            .iter()
                            .find(|b| b.key == "main")
                            .or(m.blocks.first())
                            .map(|b| b.key.clone())
                            .unwrap_or("main".into())
                    });
                    if !m.blocks.iter().any(|b| b.key == block) {
                        return Err(format!(
                            "{type_id}/{prov} has no block \"{block}\" (blocks: {})",
                            m.blocks
                                .iter()
                                .map(|b| b.key.clone())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    let before = self.snapshot();
                    if let Some(map) = self.project.extra_args_mut(&id, &prov, &block) {
                        for (k, v) in extra {
                            if v.is_null() {
                                map.remove(&k);
                            } else {
                                map.insert(k, v);
                            }
                        }
                    }
                    self.project.prune_extras(&id);
                    self.finish(before);
                    self.flash(&id);
                    self.refresh_diagnostics();
                    let mine: Vec<String> = self
                        .diagnostics
                        .iter()
                        .filter(|d| d.entity.as_deref() == Some(id.as_str()))
                        .map(|d| format!("{:?}: {}", d.severity, d.message))
                        .collect();
                    return Ok(
                        json!({"status": "updated", "entity": self.entity_json(&id), "diagnostics": mine}),
                    );
                }
                if let Some(tags) = providers {
                    let id = self.resolve(&entity)?;
                    let before = self.snapshot();
                    if let Some(n) = self.project.nodes.get_mut(&id) {
                        n.providers = tags;
                    } else if let Some(c) = self.project.containers.get_mut(&id) {
                        c.providers = tags;
                    }
                    self.finish(before);
                    return Ok(json!({"status": "updated", "entity": self.entity_json(&id)}));
                }
                Ok(r)
            }
            AgentCommand::EntityMove { entity, x, y, view } => {
                let id = self.resolve(&entity)?;
                self.with_view(view, |app| {
                    let before = app.snapshot();
                    let cur = app.entity_rect(&id).unwrap().min;
                    let (dx, dy) = (x - cur.x as i32, y - cur.y as i32);
                    // Moving a container takes its contents with it, in this view only
                    // when the view owns its layout.
                    let mut ids = vec![id.clone()];
                    ids.extend(app.project.descendants_of(&id));
                    app.shift_entities(&ids, dx, dy);
                    app.finish(before);
                    app.flash(&id);
                    Ok(json!({"status": "moved", "id": id, "in_view": app.view_name()}))
                })
            }
            AgentCommand::EntityResize { entity, w, h } => {
                let id = self.resolve(&entity)?;
                let before = self.snapshot();
                let id2 = id.clone();
                self.with_layout(|p| {
                    if let Some(c) = p.containers.get_mut(&id2) {
                        c.size = ttg_core::Size {
                            w: w.max(220),
                            h: h.max(140),
                        };
                    } else if let Some(n) = p.nodes.get_mut(&id2) {
                        n.size = Some(ttg_core::Size {
                            w: w.max(120),
                            h: h.max(48),
                        });
                    }
                });
                self.finish(before);
                self.flash(&id);
                Ok(json!({"status": "resized", "id": id}))
            }
            AgentCommand::EntitySetParent { entity, parent } => {
                let id = self.resolve(&entity)?;
                let pid = parent.map(|p| self.resolve(&p)).transpose()?;
                self.check_parent_allowed(&id, pid.as_deref())?;
                let before = self.snapshot();
                if !self.project.set_parent(&id, pid.as_deref()) {
                    return Err("that would create a containment cycle".into());
                }
                // Move it visually inside the new parent if it is not already there.
                if let Some(p) = &pid {
                    let pr = self.entity_rect(p).unwrap();
                    let er = self.entity_rect(&id).unwrap();
                    if !pr.contains_rect(er) {
                        let target = self.free_spot_in(Some(p));
                        let cur = self.entity_rect(&id).unwrap().min;
                        let (dx, dy) = (target.x - cur.x as i32, target.y - cur.y as i32);
                        let mut ids = vec![id.clone()];
                        ids.extend(self.project.descendants_of(&id));
                        self.shift_entities(&ids, dx, dy);
                    }
                }
                self.finish(before);
                self.flash(&id);
                Ok(json!({"status": "reparented", "id": id, "parent": pid}))
            }
            AgentCommand::EntityDelete { entities } => {
                let ids: Vec<Id> = entities
                    .iter()
                    .map(|e| self.resolve(e))
                    .collect::<Result<_, _>>()?;
                let before = self.snapshot();
                for id in &ids {
                    self.project.remove_entity(id);
                    self.selection.remove(id);
                }
                self.selected_edge = None;
                self.finish(before);
                Ok(json!({"status": "deleted", "ids": ids}))
            }
            AgentCommand::LinkAdd {
                source,
                target,
                relation,
                providers,
            } => {
                self.check_providers(providers.as_deref())?;
                let s = self.resolve(&source)?;
                let t = self.resolve(&target)?;
                let rel = Relation::from_key(&relation)
                    .ok_or_else(|| format!("unknown relation \"{relation}\""))?;
                let choices = self.relation_choices(&s, &t);
                if !choices.contains(&rel) {
                    return Err(format!(
                        "the {} definition does not allow '{}' to a {}; allowed: {}",
                        self.project.entity(&s).unwrap().resource_type,
                        rel.key(),
                        self.project.entity(&t).unwrap().resource_type,
                        choices.iter().map(|r| r.key()).collect::<Vec<_>>().join(", ")
                    ));
                }
                let probe = ttg_core::Edge {
                    source: s.clone(),
                    target: t.clone(),
                    relation: rel,
                    layout: None,
                    providers: Vec::new(),
                };
                if ttg_codegen::diagnostics::is_redundant_edge(&self.project, &self.catalog, &probe) {
                    return Err(
                        "redundant: the source is already inside that container, which implies this relation"
                            .into(),
                    );
                }
                let before = self.snapshot();
                let added = self.project.add_edge(&s, &t, rel);
                if let Some(tags) = providers {
                    if let Some(e) = self
                        .project
                        .edges
                        .iter_mut()
                        .find(|e| e.source == s && e.target == t && e.relation == rel)
                    {
                        e.providers = tags;
                    }
                }
                self.finish(before);
                self.flash(&s);
                self.flash(&t);
                Ok(
                    json!({"status": if added { "linked" } else { "already linked" }, "source": s, "target": t, "relation": rel.key()}),
                )
            }
            AgentCommand::LinkRemove {
                source,
                target,
                relation,
            } => {
                let s = self.resolve(&source)?;
                let t = self.resolve(&target)?;
                let rel = relation
                    .map(|r| Relation::from_key(&r).ok_or_else(|| format!("unknown relation \"{r}\"")))
                    .transpose()?;
                let before = self.snapshot();
                let n0 = self.project.edges.len();
                self.project
                    .edges
                    .retain(|e| !(e.source == s && e.target == t && rel.is_none_or(|r| e.relation == r)));
                let removed = n0 - self.project.edges.len();
                self.selected_edge = None;
                self.finish(before);
                Ok(
                    json!({"status": if removed > 0 { "removed" } else { "no such link" }, "removed": removed}),
                )
            }
            AgentCommand::SelectionSet { entities } => {
                let ids: Vec<Id> = entities
                    .iter()
                    .map(|e| self.resolve(e))
                    .collect::<Result<_, _>>()?;
                self.selection = ids.iter().cloned().collect();
                self.selected_edge = None;
                if ids.len() == 1 {
                    self.reach = None;
                }
                Ok(json!({"status": "selected", "ids": ids}))
            }
            AgentCommand::ViewSet { filter } => {
                let mut f: ttg_core::ViewFilter =
                    serde_json::from_value(filter).map_err(|e| format!("bad filter: {e}"))?;
                // Names are allowed anywhere an id is expected.
                if let Some(x) = f.focus.take() {
                    f.focus = Some(self.resolve(&x)?);
                }
                f.hidden = f
                    .hidden
                    .iter()
                    .map(|x| self.resolve(x))
                    .collect::<Result<_, _>>()?;
                f.only = f.only.iter().map(|x| self.resolve(x)).collect::<Result<_, _>>()?;
                self.active_view = None;
                self.set_filter(f);
                Ok(json!({"status": "view applied", "hidden": self.hidden_count()}))
            }
            AgentCommand::SchemaSearch { provider, query } => {
                let prov = provider.unwrap_or(self.project.settings.target_provider.clone());
                let ps = ttg_schema::index()
                    .provider(&prov)
                    .ok_or_else(|| format!("no schema for provider \"{prov}\""))?;
                let hits: Vec<J> = ps
                    .search(&query, 40)
                    .into_iter()
                    .map(|r| json!({"resource": r, "type_id": format!("{}{prov}:{r}", ttg_catalog::load::NATIVE_PREFIX)}))
                    .collect();
                Ok(json!({"provider": prov, "version": ps.version, "hits": hits}))
            }
            AgentCommand::SchemaShow {
                provider,
                resource,
                depth,
                required_only,
            } => {
                let prov = provider.unwrap_or(self.project.settings.target_provider.clone());
                let b = ttg_schema::index()
                    .resource(&prov, &resource)
                    .ok_or_else(|| format!("no {resource} on {prov}"))?
                    .filtered(depth, required_only.unwrap_or(false));
                let b = &b;
                fn block_json(b: &ttg_schema::BlockSchema) -> J {
                    json!({
                        "attributes": b.attributes.iter().map(|(k, a)| (k.clone(), json!({
                            "type": a.kind().label(), "required": a.required(), "read_only": a.read_only(),
                            "sensitive": a.sensitive(), "description": a.description(),
                        }))).collect::<Map<_, _>>(),
                        "blocks": b.blocks.iter().map(|(k, n)| (k.clone(), json!({
                            "nesting": n.nesting(), "required": n.required(), "block": block_json(n.block()),
                        }))).collect::<Map<_, _>>(),
                    })
                }
                Ok(
                    json!({"provider": prov, "resource": resource, "required": b.required(), "schema": block_json(b)}),
                )
            }
            AgentCommand::ViewActivate { name } => {
                self.activate_view_named(&name)?;
                match self.active_view() {
                    None => Ok(json!({"status": "showing everything (All)"})),
                    Some(v) => Ok(json!({
                        "status": "view active",
                        "name": v.name,
                        "own_layout": v.layout.is_some(),
                        "groups": v.groups.len(),
                        "flows": v.flows.len(),
                        "notes": v.notes.len(),
                        "logicals": v.logicals.len(),
                    })),
                }
            }
            AgentCommand::ViewGet { name } => {
                let i = match name {
                    Some(n) => self.resolve_view(&n)?,
                    None => self
                        .active_view
                        .ok_or("no view is active; pass a name or call view_activate")?,
                };
                Ok(self.view_json(i))
            }
            AgentCommand::ViewUpdate {
                view,
                name,
                description,
                legend,
            } => {
                let i = match view {
                    Some(n) => self.resolve_view(&n)?,
                    None => self
                        .active_view
                        .ok_or("no view is active; pass `view` or call view_activate")?,
                };
                if let Some(n) = &name {
                    if n.trim().is_empty() {
                        return Err("a view needs a name".into());
                    }
                }
                let before = self.snapshot();
                let v = &mut self.project.views[i];
                if let Some(n) = name {
                    v.name = n.trim().to_string();
                }
                if let Some(d) = description {
                    v.description = d;
                }
                if let Some(l) = legend {
                    v.legend = l;
                }
                self.finish(before);
                Ok(json!({"status": "view updated", "view": self.view_json(i)}))
            }
            AgentCommand::ViewFit { view } => {
                if let Some(n) = view {
                    self.activate_view_named(&n)?;
                }
                // The camera is fitted by the canvas on the next frame; headless there
                // is no canvas, so the bounds themselves are the answer.
                self.fit_requested = true;
                self.refresh_visibility();
                let b = self.world_bounds();
                Ok(json!({
                    "status": "fitted",
                    "view": self.view_name().unwrap_or_else(|| "All".into()),
                    "bounds": b.map(|r| json!({"x": r.min.x, "y": r.min.y, "w": r.width(), "h": r.height()})),
                    "shown": self.project.entities().len() - self.hidden_count(),
                }))
            }
            AgentCommand::ViewExport { view, format } => {
                let i = match view {
                    Some(n) => self.resolve_view(&n)?,
                    None => self
                        .active_view
                        .ok_or("no view is active; pass `view` or call view_activate")?,
                };
                let v = &self.project.views[i];
                let text = match format.to_lowercase().as_str() {
                    "md" | "markdown" => ttg_codegen::views::markdown(&self.project, &self.catalog, v),
                    "mermaid" => ttg_codegen::views::mermaid(&self.project, &self.catalog, v),
                    other => return Err(format!("unknown format \"{other}\" (md | mermaid)")),
                };
                Ok(json!({"view": v.name, "format": format.to_lowercase(), "text": text}))
            }
            AgentCommand::GroupAdd {
                view,
                label,
                x,
                y,
                w,
                h,
                color,
            } => self.with_view(view, |app| {
                app.require_view("groups")?;
                let id = app
                    .add_group(
                        &label,
                        ttg_core::Position { x, y },
                        ttg_core::Size {
                            w: w.max(160),
                            h: h.max(80),
                        },
                        color,
                    )
                    .ok_or("could not add the group")?;
                Ok(
                    json!({"status": "group added", "id": id, "in_view": app.view_name(), "members": app.group_members(&id)}),
                )
            }),
            AgentCommand::FlowAdd {
                view,
                from,
                to,
                label,
                dashed,
                step,
                color,
            } => self.with_view(view, |app| {
                app.require_view("flows")?;
                let f = app
                    .resolve_end(&from)
                    .ok_or_else(|| format!("no resource, group or logical node \"{from}\""))?;
                let t = app
                    .resolve_end(&to)
                    .ok_or_else(|| format!("no resource, group or logical node \"{to}\""))?;
                let id = app
                    .add_flow(f, t, &label, dashed, step, color)
                    .ok_or("a flow needs two different ends")?;
                Ok(json!({"status": "flow added", "id": id, "in_view": app.view_name()}))
            }),
            AgentCommand::NoteAdd {
                view,
                title,
                body,
                x,
                y,
                w,
                h,
                anchor,
            } => self.with_view(view, |app| {
                app.require_view("notes")?;
                let anchor = anchor.map(|a| app.resolve_anchor(&a)).transpose()?;
                let spot = app.annotation_spot(x, y);
                let id = app
                    .add_note(
                        &title,
                        &body,
                        spot,
                        ttg_core::Size {
                            w: w.unwrap_or(260).max(120),
                            h: h.unwrap_or(120).max(60),
                        },
                        anchor,
                    )
                    .ok_or("could not add the note")?;
                Ok(json!({"status": "note added", "id": id, "in_view": app.view_name()}))
            }),
            AgentCommand::LogicalAdd {
                view,
                name,
                icon,
                subtitle,
                x,
                y,
                w,
                h,
            } => self.with_view(view, |app| {
                app.require_view("logical nodes")?;
                let spot = app.annotation_spot(x, y);
                let id = app
                    .add_logical(
                        &name,
                        icon.as_deref().unwrap_or_default(),
                        subtitle.as_deref().unwrap_or_default(),
                        spot,
                        ttg_core::Size {
                            w: w.unwrap_or(ttg_core::NODE_SIZE.w).max(120),
                            h: h.unwrap_or(ttg_core::NODE_SIZE.h).max(48),
                        },
                    )
                    .ok_or("could not add the logical node")?;
                Ok(json!({"status": "logical node added", "id": id, "in_view": app.view_name()}))
            }),
            AgentCommand::AnnotationRemove { view, key } => self.with_view(view, |app| {
                let Some(v) = app.active_view() else {
                    return Err("no view is active".into());
                };
                use crate::annotations::Annotation;
                let named = |id: &str, label: &str| id == key || (!label.is_empty() && label.eq_ignore_ascii_case(&key));
                let target = v
                    .groups
                    .iter()
                    .find(|g| named(&g.id, &g.label))
                    .map(|g| Annotation::Group(g.id.clone()))
                    .or_else(|| {
                        v.flows
                            .iter()
                            .find(|f| named(&f.id, &f.label))
                            .map(|f| Annotation::Flow(f.id.clone()))
                    })
                    .or_else(|| {
                        v.notes
                            .iter()
                            .find(|n| named(&n.id, &n.title))
                            .map(|n| Annotation::Note(n.id.clone()))
                    })
                    .or_else(|| {
                        v.logicals
                            .iter()
                            .find(|l| named(&l.id, &l.name))
                            .map(|l| Annotation::Logical(l.id.clone()))
                    })
                    .ok_or_else(|| format!("no group, flow, note or logical node \"{key}\" in this view"))?;
                let before = app.snapshot();
                app.remove_annotation(&target);
                app.finish(before);
                Ok(json!({"status": "removed", "in_view": app.view_name()}))
            }),
            AgentCommand::ViewSave { name } => {
                let before = self.snapshot();
                self.project
                    .views
                    .push(ttg_core::View::new(&name, self.filter.clone()));
                self.active_view = Some(self.project.views.len() - 1);
                self.finish(before);
                Ok(json!({"status": "view saved", "name": name}))
            }
            AgentCommand::LayoutTidy { container } => {
                let c = container.map(|x| self.resolve(&x)).transpose()?;
                self.tidy(c.as_deref());
                Ok(json!({"status": "tidied"}))
            }
            AgentCommand::LayoutAlign { how } => {
                use ttg_core::layout::Align;
                let how = match how.as_str() {
                    "left" => Align::Left,
                    "hcenter" => Align::HCenter,
                    "right" => Align::Right,
                    "top" => Align::Top,
                    "vcenter" => Align::VCenter,
                    "bottom" => Align::Bottom,
                    other => return Err(format!("unknown alignment \"{other}\"")),
                };
                if self.selection.len() < 2 {
                    return Err("select two or more entities first (selection_set)".into());
                }
                self.align_selection(how);
                Ok(json!({"status": self.status}))
            }
            AgentCommand::LayoutDistribute { horizontal } => {
                if self.selection.len() < 3 {
                    return Err("select three or more entities first (selection_set)".into());
                }
                self.distribute_selection(horizontal);
                Ok(json!({"status": self.status}))
            }
            AgentCommand::SettingsSet {
                tool,
                provider,
                provider_settings,
            } => {
                let before = self.snapshot();
                if let Some(t) = tool {
                    let t: Tool = serde_json::from_value(J::String(t.to_lowercase()))
                        .map_err(|_| "tool must be terraform or opentofu".to_string())?;
                    self.project.settings.tool = t;
                }
                if let Some(p) = provider {
                    if self.catalog.provider(&p).is_none() {
                        return Err(format!("unknown provider \"{p}\""));
                    }
                    self.project.settings.target_provider = p;
                }
                if let Some(ps) = provider_settings {
                    for (pid, vals) in ps {
                        let J::Object(vals) = vals else {
                            return Err(format!("provider_settings.{pid} must be an object"));
                        };
                        let m = self.project.settings.provider_settings.entry(pid).or_default();
                        for (k, v) in vals {
                            m.insert(k, v.as_str().map(|s| s.to_string()).unwrap_or(v.to_string()));
                        }
                    }
                }
                self.finish(before);
                Ok(
                    json!({"status": "settings updated", "tool": self.project.settings.tool, "provider": self.project.settings.target_provider}),
                )
            }
            AgentCommand::ProjectSave { path } => {
                let p = match path {
                    Some(p) => std::path::PathBuf::from(p),
                    None => self
                        .path
                        .clone()
                        .ok_or("the project has never been saved; give a path")?,
                };
                self.save_to(p.clone());
                match &self.error {
                    Some(e) => {
                        let e = e.clone();
                        self.error = None;
                        Err(e)
                    }
                    None => Ok(json!({"status": "saved", "path": p})),
                }
            }
            AgentCommand::ProjectOpen { path } => {
                let p = std::path::PathBuf::from(&path);
                if !p.is_file() {
                    return Err(format!("{path} does not exist"));
                }
                let was_dirty = self.dirty;
                self.request(crate::app::PendingAction::OpenPath(p));
                Ok(
                    json!({"status": if was_dirty { "prompt shown: the user must choose save / discard / cancel" } else { "opened" }, "path": path}),
                )
            }
            AgentCommand::ProjectNew { name } => {
                let was_dirty = self.dirty;
                self.request(crate::app::PendingAction::New);
                if !was_dirty {
                    if let Some(n) = name {
                        self.project.name = n;
                    }
                }
                Ok(
                    json!({"status": if was_dirty { "prompt shown: the user must choose save / discard / cancel" } else { "new project" }}),
                )
            }
            AgentCommand::ExportRun { dir, provider } => {
                let provider = provider.unwrap_or(self.project.settings.target_provider.clone());
                let tool = self.project.settings.tool;
                let rep = ttg_codegen::export(
                    &self.project,
                    &self.catalog,
                    &provider,
                    tool,
                    std::path::Path::new(&dir),
                )
                .map_err(|e| e.to_string())?;
                Ok(json!({
                    "status": "exported",
                    "tool": tool,
                    "provider": provider,
                    "dir": rep.out_dir,
                    "files": rep.files,
                    "manual_steps": rep.manual_steps.iter().map(|m| m.title.clone()).collect::<Vec<_>>(),
                    "warnings": rep.warnings.iter().map(|d| d.message.clone()).collect::<Vec<_>>(),
                }))
            }
            AgentCommand::Batch(cmds) => {
                let before = self.snapshot();
                let start = self.history.len();
                let mut results = Vec::new();
                for (i, c) in cmds.into_iter().enumerate() {
                    if matches!(
                        c,
                        AgentCommand::Batch(_)
                            | AgentCommand::Undo
                            | AgentCommand::Redo
                            | AgentCommand::ProjectOpen { .. }
                            | AgentCommand::ProjectNew { .. }
                            | AgentCommand::ProjectSave { .. }
                            | AgentCommand::ExportRun { .. }
                    ) {
                        self.project = before;
                        self.history.truncate(start);
                        self.diag_dirty = true;
                        return Err(format!(
                            "command {i} ({}) is not allowed inside a batch; the batch was rolled back",
                            c.label()
                        ));
                    }
                    let label = c.label();
                    match self.agent_exec(c) {
                        Ok(v) => results.push(json!({ "tool": label, "result": v })),
                        Err(e) => {
                            self.project = before;
                            self.history.truncate(start);
                            self.diag_dirty = true;
                            return Err(format!(
                                "command {i} ({label}) failed: {e}; the batch was rolled back"
                            ));
                        }
                    }
                }
                // Collapse the steps the sub-commands pushed into one.
                self.history.truncate(start);
                self.finish(before);
                Ok(json!({ "status": "applied", "count": results.len(), "results": results }))
            }
            AgentCommand::ExportDiff { dir, provider } => {
                let provider = provider.unwrap_or(self.project.settings.target_provider.clone());
                let g = ttg_codegen::generate(
                    &self.project,
                    &self.catalog,
                    &provider,
                    self.project.settings.tool,
                )
                .map_err(|e| e.to_string())?;
                let diffs = ttg_codegen::diff::against_dir(&g, std::path::Path::new(&dir));
                Ok(json!({
                    "provider": provider,
                    "dir": dir,
                    "summary": ttg_codegen::diff::summary(&diffs),
                    "changed": diffs.iter().any(|d| d.changed()),
                    "files": diffs.iter().map(|d| json!({
                        "name": d.name,
                        "status": format!("{:?}", d.status).to_lowercase(),
                        "added": d.added,
                        "removed": d.removed,
                        "diff": if d.changed() { ttg_codegen::diff::render(d, 2) } else { String::new() },
                    })).collect::<Vec<_>>(),
                }))
            }
            AgentCommand::Changes { since } => {
                let (ago, by) = match self.mcp.last_change {
                    Some((t, by)) => (Some(t.elapsed().as_secs_f64()), Some(by)),
                    None => (None, None),
                };
                Ok(json!({
                    "revision": self.mcp.revision,
                    "changed": since.is_none_or(|s| self.mcp.revision > s),
                    "last_change_by": by,
                    "last_change_secs_ago": ago,
                    "dirty": self.dirty,
                    "path": self.path,
                    "selection": self.selection.iter().cloned().collect::<Vec<_>>(),
                }))
            }
            AgentCommand::Undo => {
                if !self.history.can_undo() {
                    return Err("nothing to undo".into());
                }
                self.undo();
                Ok(json!({"status": "undone"}))
            }
            AgentCommand::Redo => {
                if !self.history.can_redo() {
                    return Err("nothing to redo".into());
                }
                self.redo();
                Ok(json!({"status": "redone"}))
            }
        }
    }

    // ------------------------------------------------------------------ writes

    fn check_providers(&self, tags: Option<&[String]>) -> Result<(), String> {
        for t in tags.unwrap_or(&[]) {
            if self.catalog.provider(t).is_none() {
                return Err(format!(
                    "unknown provider \"{t}\" (known: {})",
                    self.catalog.provider_ids().join(", ")
                ));
            }
        }
        Ok(())
    }

    fn check_parent_allowed(&self, id: &str, parent: Option<&str>) -> Result<(), String> {
        let Some(p) = parent else { return Ok(()) };
        let Some(c) = self.project.containers.get(p) else {
            return Err(format!("\"{p}\" is not a container"));
        };
        let t = self.project.entity(id).unwrap().resource_type.to_string();
        let allowed = self
            .catalog
            .resource(&t)
            .is_some_and(|d| d.resource.allowed_parents.iter().any(|a| a == &c.container_type));
        if allowed {
            Ok(())
        } else {
            Err(format!(
                "a {t} cannot be placed inside a {} (allowed parents: {})",
                c.container_type,
                self.catalog
                    .resource(&t)
                    .map(|d| d.resource.allowed_parents.join(", "))
                    .unwrap_or_default()
            ))
        }
    }

    /// A position for a new entity: the next free slot in a grid inside the parent, or
    /// to the right of everything at top level.
    fn free_spot_in(&self, parent: Option<&str>) -> ttg_core::Position {
        match parent {
            Some(p) => {
                let pr = self.entity_rect(p).unwrap();
                let n = self.project.children_of(p).len() as i32;
                ttg_core::Position {
                    x: pr.min.x as i32 + 30 + (n % 3) * 210,
                    y: pr.min.y as i32 + 50 + (n / 3) * 100,
                }
            }
            None => match self.world_bounds() {
                Some(b) => ttg_core::Position {
                    x: b.max.x as i32 + 80,
                    y: b.min.y as i32,
                },
                None => ttg_core::Position { x: 60, y: 60 },
            },
        }
    }

    fn agent_entity_add(
        &mut self,
        type_id: &str,
        name: Option<String>,
        parent: Option<String>,
        x: Option<i32>,
        y: Option<i32>,
    ) -> R {
        if Catalog::is_native(type_id) {
            self.catalog.ensure_native(type_id);
        }
        let def = self
            .catalog
            .resource(type_id)
            .cloned()
            .ok_or_else(|| format!("unknown type \"{type_id}\" (see catalog_types)"))?;
        let pid = parent.map(|p| self.resolve(&p)).transpose()?;
        if let Some(p) = &pid {
            let ct = &self
                .project
                .containers
                .get(p)
                .ok_or("parent is not a container")?
                .container_type;
            if !def.resource.allowed_parents.iter().any(|a| a == ct) {
                return Err(format!(
                    "a {type_id} cannot be placed inside a {ct} (allowed parents: {})",
                    def.resource.allowed_parents.join(", ")
                ));
            }
        }
        let spot = self.free_spot_in(pid.as_deref());
        let pos = ttg_core::Position {
            x: x.unwrap_or(spot.x),
            y: y.unwrap_or(spot.y),
        };
        let before = self.snapshot();
        let id = self
            .add_entity(type_id, Pos2::new(pos.x as f32, pos.y as f32))
            .ok_or("could not add the entity")?;
        // add_entity already pushed a history step; collapse ours into one.
        self.history.pop_last();
        if let Some(n) = name {
            let slug = ttg_core::slugify(&n);
            if self
                .project
                .entities()
                .iter()
                .any(|e| e.id != id && ttg_core::slugify(e.name) == slug)
            {
                self.project.remove_entity(&id);
                self.project = before;
                return Err(format!(
                    "the name \"{n}\" collides with an existing resource (same slug)"
                ));
            }
            if let Some(nd) = self.project.nodes.get_mut(&id) {
                nd.name = n;
            } else if let Some(c) = self.project.containers.get_mut(&id) {
                c.name = n;
            }
        }
        if let Some(p) = &pid {
            self.project.set_parent(&id, Some(p));
        }
        self.finish(before);
        self.flash(&id);
        Ok(
            json!({"status": "added", "id": id, "entity": self.entity_json(&id), "kind": match def.resource.kind { ResourceKind::Container => "container", ResourceKind::Node => "node" }}),
        )
    }

    /// JSON -> IR value for a field, validated against the definition, with `entity_ref`
    /// values (bare, or inside a `struct_list` row) resolved from an id or display name
    /// to the entity id the same way every other tool addresses entities. `null` clears
    /// the field.
    fn convert_value(&self, f: &FieldDef, v: &J) -> Result<Option<Value>, String> {
        if v.is_null() {
            return Ok(None);
        }
        let mut val: Value =
            serde_json::from_value(v.clone()).map_err(|e| format!("field \"{}\": {e}", f.name))?;
        self.resolve_entity_refs(f, &mut val)
            .map_err(|e| format!("field \"{}\": {e}", f.name))?;
        ttg_catalog::fields::check_value(f, Some(&val)).map_err(|e| format!("field \"{}\": {e}", f.name))?;
        Ok(Some(val))
    }

    /// Resolve `entity_ref` values to entity ids, case-insensitively by display name,
    /// erroring on an ambiguous name (same rule as `resolve`). Recurses into
    /// `struct_list` rows so e.g. a security-group rule's `source_group` accepts a name.
    fn resolve_entity_refs(&self, f: &FieldDef, val: &mut Value) -> Result<(), String> {
        match f.field_type {
            FieldType::EntityRef => {
                if let Value::Str(s) = val {
                    *s = self.resolve(s)?;
                }
            }
            FieldType::StructList => {
                if let Value::Records(rows) = val {
                    for row in rows {
                        for item in &f.items {
                            if item.field_type != FieldType::EntityRef {
                                continue;
                            }
                            if let Some(v) = row.get_mut(&item.name) {
                                self.resolve_entity_refs(item, v)?;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn agent_entity_update(
        &mut self,
        entity: &str,
        name: Option<String>,
        config: Option<Map<String, J>>,
        provider_config: Option<Map<String, J>>,
        manual: Option<bool>,
    ) -> R {
        let id = self.resolve(entity)?;
        let type_id = self.project.entity(&id).unwrap().resource_type.to_string();
        let def = self.catalog.resource(&type_id).cloned().ok_or("unknown type")?;
        // Validate everything before touching the project.
        let mut abstract_vals: Vec<(String, Option<Value>)> = Vec::new();
        if let Some(cfg) = &config {
            for (k, v) in cfg {
                let f = def.fields.iter().find(|f| &f.name == k).ok_or_else(|| {
                    format!(
                        "{type_id} has no field \"{k}\" (fields: {})",
                        field_names(&def.fields)
                    )
                })?;
                abstract_vals.push((k.clone(), self.convert_value(f, v)?));
            }
        }
        let mut prov_vals: Vec<(String, String, Option<Value>)> = Vec::new();
        if let Some(pc) = &provider_config {
            for (pid, vals) in pc {
                let m = def
                    .providers
                    .get(pid)
                    .ok_or_else(|| format!("{type_id} has no {pid} mapping"))?;
                let J::Object(vals) = vals else {
                    return Err(format!("provider_config.{pid} must be an object"));
                };
                for (k, v) in vals {
                    let f = m.fields.iter().find(|f| &f.name == k).ok_or_else(|| {
                        format!(
                            "{type_id}/{pid} has no field \"{k}\" (fields: {})",
                            field_names(&m.fields)
                        )
                    })?;
                    prov_vals.push((pid.clone(), k.clone(), self.convert_value(f, v)?));
                }
            }
        }
        if let Some(n) = &name {
            let slug = ttg_core::slugify(n);
            if self
                .project
                .entities()
                .iter()
                .any(|e| e.id != id && ttg_core::slugify(e.name) == slug)
            {
                return Err(format!(
                    "the name \"{n}\" collides with an existing resource (same slug)"
                ));
            }
        }
        let before = self.snapshot();
        {
            let (cfg, pcfg, nm, mn): (&mut ttg_core::Config, &mut _, &mut String, &mut bool) =
                if let Some(n) = self.project.nodes.get_mut(&id) {
                    (&mut n.config, &mut n.provider_config, &mut n.name, &mut n.manual)
                } else {
                    let c = self.project.containers.get_mut(&id).unwrap();
                    (&mut c.config, &mut c.provider_config, &mut c.name, &mut c.manual)
                };
            for (k, v) in abstract_vals {
                match v {
                    Some(v) => {
                        cfg.insert(k, v);
                    }
                    None => {
                        cfg.remove(&k);
                    }
                }
            }
            for (pid, k, v) in prov_vals {
                let m = pcfg.entry(pid).or_default();
                match v {
                    Some(v) => {
                        m.insert(k, v);
                    }
                    None => {
                        m.remove(&k);
                    }
                }
            }
            if let Some(n) = name {
                *nm = n;
            }
            if let Some(m) = manual {
                *mn = m;
            }
        }
        self.finish(before);
        self.flash(&id);
        self.refresh_diagnostics();
        let mine: Vec<String> = self
            .diagnostics
            .iter()
            .filter(|d| d.entity.as_deref() == Some(id.as_str()))
            .map(|d| format!("{:?}: {}", d.severity, d.message))
            .collect();
        Ok(json!({"status": "updated", "entity": self.entity_json(&id), "diagnostics": mine}))
    }

    // ------------------------------------------------------------------ reads

    fn summary_json(&mut self) -> J {
        self.refresh_diagnostics();
        let errors = self.errors().len();
        let warnings = self
            .diagnostics
            .iter()
            .filter(|d| d.severity == ttg_codegen::Severity::Warning)
            .count();
        json!({
            "name": self.project.name,
            "path": self.path,
            "dirty": self.dirty,
            "revision": self.mcp.revision,
            "tool": self.project.settings.tool,
            "target_provider": self.project.settings.target_provider,
            "providers": self.catalog.provider_ids(),
            "counts": { "nodes": self.project.nodes.len(), "containers": self.project.containers.len(), "edges": self.project.edges.len() },
            "diagnostics": { "errors": errors, "warnings": warnings },
            "views": self.project.views.iter().map(|v| v.name.clone()).collect::<Vec<_>>(),
            "selection": self.selection,
            "display": if self.concrete_mode() { "concrete" } else { "abstract" },
            "reachability_overlay": self.reach_mode,
            "layers": self.catalog.provider_ids().iter().map(|pid| {
                let off = ttg_codegen::layers::off_layer(&self.project, &self.catalog, pid);
                (pid.clone(), json!({
                    "resources": self.project.entities().len() - off.len(),
                    "left_out": off.iter().map(|(id, _)| self.project.entity(id).map(|e| e.name.to_string()).unwrap_or_default()).collect::<Vec<_>>(),
                }))
            }).collect::<Map<_, _>>(),
            "entities": self.project.entities().iter().map(|e| json!({"id": e.id, "name": e.name, "type": e.resource_type, "parent": e.parent})).collect::<Vec<_>>(),
        })
    }

    fn catalog_types_json(&self) -> J {
        let provider = &self.project.settings.target_provider;
        J::Array(
            self.catalog
                .resources
                .values()
                .map(|d| {
                    json!({
                        "type_id": d.resource.type_id,
                        "display_name": d.resource.display_name,
                        "category": d.resource.category,
                        "kind": match d.resource.kind { ResourceKind::Container => "container", ResourceKind::Node => "node" },
                        "description": d.resource.description,
                        "allowed_parents": d.resource.allowed_parents,
                        "providers_only": d.resource.providers,
                        "mappings": d.providers.iter().map(|(p, m)| (p.clone(), J::String(format!("{:?}", m.status).to_lowercase()))).collect::<Map<_, _>>(),
                        "generates_on_target": d.providers.get(provider).map(|m| m.blocks.iter().map(|b| b.resource.clone()).collect::<Vec<_>>()),
                    })
                })
                .collect(),
        )
    }

    fn catalog_type_json(&self, type_id: &str) -> R {
        let d = self
            .catalog
            .resource(type_id)
            .ok_or_else(|| format!("unknown type \"{type_id}\""))?;
        let field = |f: &FieldDef| {
            json!({
                "name": f.name,
                "label": f.label(),
                "type": format!("{:?}", f.field_type).to_lowercase(),
                "required": f.required,
                "default": f.default_value(),
                "options": f.options,
                "description": f.description,
                "pattern_hint": f.pattern_hint,
                "items": f.items.iter().map(|i| json!({"name": i.name, "type": format!("{:?}", i.field_type).to_lowercase(), "options": i.options, "targets": i.targets})).collect::<Vec<_>>(),
            })
        };
        Ok(json!({
            "type_id": d.resource.type_id,
            "display_name": d.resource.display_name,
            "category": d.resource.category,
            "kind": match d.resource.kind { ResourceKind::Container => "container", ResourceKind::Node => "node" },
            "description": d.resource.description,
            "allowed_parents": d.resource.allowed_parents,
            "providers_only": d.resource.providers,
            "fields": d.fields.iter().map(field).collect::<Vec<_>>(),
            "relations": d.relations.iter().map(|r| json!({
                "kind": r.kind, "label": r.label, "targets": r.targets,
                "cardinality": format!("{:?}", r.cardinality).to_lowercase(),
                "via_parent": r.via_parent,
            })).collect::<Vec<_>>(),
            "providers": d.providers.iter().map(|(p, m)| (p.clone(), json!({
                "status": format!("{:?}", m.status).to_lowercase(),
                "notes": m.notes,
                "fields": m.fields.iter().map(field).collect::<Vec<_>>(),
                "resources": m.blocks.iter().map(|b| b.resource.clone()).collect::<Vec<_>>(),
                "manual_steps": m.manual_steps.iter().map(|s| s.title.clone()).collect::<Vec<_>>(),
            }))).collect::<Map<_, _>>(),
        }))
    }

    fn diagnostics_json(&self) -> J {
        J::Array(
            self.diagnostics
                .iter()
                .map(|d| {
                    json!({
                        "entity": d.entity,
                        "name": d.entity.as_ref().and_then(|id| self.project.entity(id).map(|e| e.name.to_string())),
                        "severity": format!("{:?}", d.severity).to_lowercase(),
                        "code": format!("{:?}", d.code),
                        "message": d.message,
                    })
                })
                .collect(),
        )
    }

    fn reach_posture_json(&mut self) -> J {
        let reach = self.reach().clone();
        let name = |p: &TtgApp, id: &str| {
            p.project
                .entity(id)
                .map(|e| e.name.to_string())
                .unwrap_or_default()
        };
        J::Array(
            reach
                .posture
                .iter()
                .map(|(id, po)| {
                    let egress = match &po.egress {
                        ttg_codegen::reach::Egress::NotNeeded => json!("not needed (passive)"),
                        ttg_codegen::reach::Egress::Unrestricted => json!("outside the network"),
                        ttg_codegen::reach::Egress::Via(h) => json!({"via": h.iter().map(|x| name(self, x)).collect::<Vec<_>>()}),
                        ttg_codegen::reach::Egress::Blocked(r) => json!({"blocked": r}),
                    };
                    json!({
                        "id": id,
                        "name": name(self, id),
                        "subnets": po.subnets.iter().map(|s| name(self, s)).collect::<Vec<_>>(),
                        "security_group": po.security_group.as_ref().map(|s| name(self, s)),
                        "egress": egress,
                        "exposed": po.exposed,
                        "listening_port": self.project.entity(id).and_then(|e| ttg_codegen::reach::listening_port(&e)),
                    })
                })
                .collect(),
        )
    }

    fn path_json(&self, other: &str, p: &ttg_codegen::reach::Path) -> J {
        let name = |id: &str| {
            self.project
                .entity(id)
                .map(|e| e.name.to_string())
                .unwrap_or_default()
        };
        json!({
            "entity": other,
            "name": name(other),
            "status": format!("{:?}", p.status).to_lowercase(),
            "hops": p.hops.iter().map(|h| name(h)).collect::<Vec<_>>(),
            "reason": p.reason,
            "notes": p.notes,
        })
    }

    fn reach_from_json(&mut self, id: &str) -> J {
        let reach = self.reach().clone();
        let paths = ttg_codegen::reach::paths_from(&self.project, &self.catalog, &reach, id);
        json!({
            "from": id,
            "initiates": self.project.entity(id).is_some_and(|e| ttg_codegen::reach::initiates(e.resource_type)),
            "paths": paths.iter().map(|p| self.path_json(&p.target, p)).collect::<Vec<_>>(),
        })
    }

    fn reach_to_json(&mut self, id: &str) -> J {
        let reach = self.reach().clone();
        let paths = ttg_codegen::reach::paths_to(&self.project, &self.catalog, &reach, id);
        json!({
            "to": id,
            "paths": paths.iter().map(|(s, p)| self.path_json(s, p)).collect::<Vec<_>>(),
        })
    }

    fn export_preview_json(&mut self, provider: Option<String>) -> R {
        let provider = provider.unwrap_or(self.project.settings.target_provider.clone());
        let g = ttg_codegen::generate(
            &self.project,
            &self.catalog,
            &provider,
            self.project.settings.tool,
        )
        .map_err(|e| e.to_string())?;
        Ok(json!({
            "provider": g.provider,
            "tool": g.tool,
            "files": g.files,
            "manual_steps": g.manual_steps.iter().map(|m| m.title.clone()).collect::<Vec<_>>(),
        }))
    }
}

fn field_names(fields: &[FieldDef]) -> String {
    fields
        .iter()
        .map(|f| f.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}
