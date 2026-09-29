//! Round 3 of the MCP friction list (WP16): calls that used to be all-or-nothing.
//!
//! A child of `exec` so it can use that file's private helpers (`resolve`,
//! `entity_json`, ...). Everything here runs on the UI thread like the rest of the
//! executor; every write goes through `snapshot()` / `finish()` and is one undo step.
//!
//! * [`TtgApp::dry_run`] — a batch, played and taken back.
//! * [`TtgApp::bulk_update`] / [`TtgApp::bulk_link`] — one write, many entities.
//! * [`TtgApp::project_slice`], [`TtgApp::diagnostics_filtered`],
//!   [`TtgApp::catalog_relations_json`], [`TtgApp::entity_preview_json`] — reads that
//!   answer with what was asked for instead of everything.
//! * [`TtgApp::add_flow_checked`] — `view_flow_add` that says when an end is hidden.

use super::{AgentCommand, R};
use crate::app::TtgApp;
use serde_json::{json, Value as J};
use std::collections::BTreeSet;
use ttg_catalog::Catalog;
use ttg_codegen::views::{hidden_because, reveal, Reveal};
use ttg_codegen::{Diagnostic, Severity};
use ttg_core::{glob_match, FlowEnd, Id, Note, Position, Project, View};

use crate::mcp::{EntityChanges, Selector};

/// Entities paired with what was done to them, or why nothing could be: the changes
/// made to a view's filter, or the reasons an entity stays hidden.
type Reasons = Vec<(Id, Vec<String>)>;

/// The text of a failed generation, never empty. `export_preview`, `export_diff`,
/// `export_run` and `entity_preview` all go through here, so one blocked project reads
/// the same whichever tool met it: the list of blocking diagnostics.
pub(super) fn gen_error_text(e: &ttg_codegen::GenError) -> String {
    let text = e.to_string();
    if text.trim().is_empty() {
        format!("the export failed without saying why ({e:?})")
    } else {
        text
    }
}

/// A diagnostic as the agent sees it, with the entity's name looked up in `p`.
pub(super) fn diag_json(p: &Project, d: &Diagnostic) -> J {
    json!({
        "entity": d.entity,
        "name": d.entity.as_ref().and_then(|id| p.entity(id).map(|e| e.name.to_string())),
        "severity": format!("{:?}", d.severity).to_lowercase(),
        "code": format!("{:?}", d.code),
        "provider": d.provider,
        "message": d.message,
    })
}

/// Where a note is drawn, and — when it hangs off something that can be found — the
/// offset from that anchor it actually stores. `entity_move` takes the first; the
/// second is what the file holds, and what an agent used to be handed as `position`.
pub(super) fn note_geometry(
    p: &Project,
    v: &View,
    n: &Note,
    visible: &dyn Fn(&str) -> bool,
) -> (Position, Option<Position>) {
    let r = ttg_core::view::note_rect(p, v, n, visible);
    let drawn = Position {
        x: r.x.round() as i32,
        y: r.y.round() as i32,
    };
    let offset = n
        .anchor
        .as_ref()
        .and_then(|a| ttg_core::view::anchor_rect(p, v, a, visible))
        .map(|_| {
            if n.position == Position::default() {
                ttg_core::view::NOTE_OFFSET
            } else {
                n.position
            }
        });
    (drawn, offset)
}

/// Diagnostics in `after` that were not in `before`, and the ones that went away
/// (compared as a multiset: two identical lines are two lines).
fn diag_delta<'a>(
    before: &'a [Diagnostic],
    after: &'a [Diagnostic],
) -> (Vec<&'a Diagnostic>, Vec<&'a Diagnostic>) {
    let mut gone: Vec<&Diagnostic> = before.iter().collect();
    let mut added = Vec::new();
    for a in after {
        match gone.iter().position(|b| *b == a) {
            Some(i) => {
                gone.remove(i);
            }
            None => added.push(a),
        }
    }
    (added, gone)
}

fn count(ds: &[Diagnostic], s: Severity) -> usize {
    ds.iter().filter(|d| d.severity == s).count()
}

/// The parts of the UI a batch can disturb without changing the project: a dry run puts
/// them back so it leaves no trace.
struct Memo {
    dirty: bool,
    selection: BTreeSet<Id>,
    selected_edge: Option<usize>,
    selected_annotation: Option<crate::annotations::Annotation>,
    active_view: Option<usize>,
    filter: ttg_core::ViewFilter,
    status: String,
    flashes: usize,
}

impl Memo {
    fn take(app: &TtgApp) -> Memo {
        Memo {
            dirty: app.dirty,
            selection: app.selection.clone(),
            selected_edge: app.selected_edge,
            selected_annotation: app.selected_annotation.clone(),
            active_view: app.active_view,
            filter: app.filter.clone(),
            status: app.status.clone(),
            flashes: app.mcp.flash.len(),
        }
    }

    fn restore(self, app: &mut TtgApp) {
        app.dirty = self.dirty;
        app.selection = self.selection;
        app.selected_edge = self.selected_edge;
        app.selected_annotation = self.selected_annotation;
        app.active_view = self.active_view;
        app.filter = self.filter;
        app.status = self.status;
        app.mcp.flash.truncate(self.flashes);
    }
}

impl TtgApp {
    // ------------------------------------------------------------------ batches

    /// Run a batch's commands against the current project. On any failure — or a command
    /// a batch may not contain — the project goes back to `before`, the undo history to
    /// `start` steps, and the error says which command failed.
    pub(super) fn run_batch(
        &mut self,
        cmds: Vec<AgentCommand>,
        before: &Project,
        start: usize,
    ) -> Result<Vec<J>, String> {
        let mut results = Vec::new();
        for (i, c) in cmds.into_iter().enumerate() {
            if matches!(
                c,
                AgentCommand::Batch(_)
                    | AgentCommand::DryRun(_)
                    | AgentCommand::Undo
                    | AgentCommand::Redo
                    | AgentCommand::ProjectOpen { .. }
                    | AgentCommand::ProjectNew { .. }
                    | AgentCommand::ProjectSave { .. }
                    | AgentCommand::ExportRun { .. }
            ) {
                self.roll_back_batch(before, start);
                return Err(format!(
                    "command {i} ({}) is not allowed inside a batch; the batch was rolled back",
                    c.label()
                ));
            }
            let label = c.label();
            match self.agent_exec(c) {
                Ok(v) => results.push(json!({ "tool": label, "result": v })),
                Err(e) => {
                    self.roll_back_batch(before, start);
                    return Err(format!(
                        "command {i} ({label}) failed: {e}; the batch was rolled back"
                    ));
                }
            }
        }
        Ok(results)
    }

    fn roll_back_batch(&mut self, before: &Project, start: usize) {
        self.project = before.clone();
        self.history.truncate(start);
        self.diag_dirty = true;
    }

    /// `project_apply { dry_run: true }`: play the batch, work out how the diagnostics
    /// moved, and take everything back. Nothing lands: not in the project, not in the
    /// undo history (the redo stack survives too), not in the revision counter and not
    /// in what subscribers are told; the selection, view and status bar are put back.
    /// A command that would fail fails here the same way, with the same message.
    pub(super) fn dry_run(&mut self, cmds: Vec<AgentCommand>) -> R {
        let target = self.project.settings.target_provider.clone();
        let before = self.snapshot();
        let memo = Memo::take(self);
        // Fresh, not `self.diagnostics`: the cached list may be waiting for a refresh.
        let diags_before = ttg_codegen::diagnostics::run(&before, &self.catalog, &target);
        // The batch runs against a history of its own, so neither its steps nor the
        // limit-driven eviction of old ones can touch the user's.
        let parked = std::mem::take(&mut self.history);
        self.mcp.quiet = true;

        let ran = self.run_batch(cmds, &before, 0);
        let reply = ran.map(|results| {
            let diags_after = ttg_codegen::diagnostics::run(&self.project, &self.catalog, &target);
            let (added, removed) = diag_delta(&diags_before, &diags_after);
            json!({
                "status": "dry run: nothing was applied",
                "dry_run": true,
                "count": results.len(),
                "results": results,
                "diagnostics": {
                    "provider": target,
                    "added": added.iter().map(|d| diag_json(&self.project, d)).collect::<Vec<_>>(),
                    "removed": removed.iter().map(|d| diag_json(&before, d)).collect::<Vec<_>>(),
                    "errors": {"before": count(&diags_before, Severity::Error), "after": count(&diags_after, Severity::Error)},
                    "warnings": {"before": count(&diags_before, Severity::Warning), "after": count(&diags_after, Severity::Warning)},
                },
            })
        });

        // Whatever happened, leave nothing behind.
        self.project = before;
        self.history = parked;
        self.mcp.quiet = false;
        memo.restore(self);
        self.diag_dirty = true;
        self.reach = None;
        self.other_diags = None;
        self.refresh_visibility();
        reply
    }

    // ------------------------------------------------------------------ bulk writes

    /// The entities a selector matches, in the order the project lists them. Refuses a
    /// selector with no criteria and one that matches nothing, both with a message that
    /// says what to give instead.
    fn select_entities(&mut self, s: &Selector) -> Result<Vec<Id>, String> {
        if s.is_empty() {
            return Err(
                "the selection is empty: give `types`, `name_glob` and/or `ids` (an empty selector would match every entity, which no bulk edit means)"
                    .into(),
            );
        }
        for t in &s.types {
            if Catalog::is_native(t) {
                self.catalog.ensure_native(t);
            }
            if self.catalog.resource(t).is_none() {
                return Err(format!("select.types: unknown type \"{t}\" (see catalog_types)"));
            }
        }
        let ids: Vec<Id> = s.ids.iter().map(|x| self.resolve(x)).collect::<Result<_, _>>()?;
        let glob = s.name_glob.as_deref().filter(|g| !g.is_empty());
        let hits: Vec<Id> = self
            .project
            .entities()
            .iter()
            .filter(|e| ids.is_empty() || ids.iter().any(|i| i == e.id))
            .filter(|e| s.types.is_empty() || s.types.iter().any(|t| t == e.resource_type))
            .filter(|e| glob.is_none_or(|g| glob_match(g, e.name)))
            .map(|e| e.id.to_string())
            .collect();
        if hits.is_empty() {
            return Err(format!(
                "the selection matched no entity (types: {}, name_glob: {}, ids: {}); check catalog_types / project_summary for what exists",
                if s.types.is_empty() { "any".to_string() } else { s.types.join(", ") },
                glob.map(|g| format!("\"{g}\"")).unwrap_or_else(|| "any".into()),
                if s.ids.is_empty() { "any".to_string() } else { s.ids.join(", ") },
            ));
        }
        Ok(hits)
    }

    /// Run `each` on every id as ONE undo step. Every refusal is collected, not just the
    /// first, and if there are any nothing is kept: the reply lists them all so the
    /// selection can be narrowed in one go. The revision moves once, on success.
    fn run_bulk<T>(
        &mut self,
        ids: &[Id],
        mut each: impl FnMut(&mut Self, &Id) -> Result<T, String>,
    ) -> Result<Vec<T>, Vec<(Id, String)>> {
        let before = self.snapshot();
        // A history of its own: the sub-commands' steps are discarded, and one step for
        // the whole write is pushed at the end.
        let parked = std::mem::take(&mut self.history);
        self.mcp.quiet = true;
        let mut done = Vec::new();
        let mut refused = Vec::new();
        for id in ids {
            match each(self, id) {
                Ok(t) => done.push(t),
                Err(e) => refused.push((id.clone(), e)),
            }
        }
        self.mcp.quiet = false;
        self.history = parked;
        if refused.is_empty() {
            self.finish(before);
            Ok(done)
        } else {
            self.project = before;
            self.diag_dirty = true;
            self.reach = None;
            self.other_diags = None;
            self.refresh_visibility();
            Err(refused)
        }
    }

    fn name_and_type(&self, id: &str) -> String {
        self.project
            .entity(id)
            .map(|e| format!("\"{}\" ({})", e.name, e.resource_type))
            .unwrap_or_else(|| id.to_string())
    }

    fn refusals(&self, what: &str, total: usize, refused: &[(Id, String)]) -> String {
        format!(
            "{what} changed nothing: {} of {total} selected entities refused it:\n- {}",
            refused.len(),
            refused
                .iter()
                .map(|(id, e)| format!("{}: {e}", self.name_and_type(id)))
                .collect::<Vec<_>>()
                .join("\n- ")
        )
    }

    /// `entity_update { select, ... }`: the same update on every match, as one undo step.
    pub(super) fn bulk_update(&mut self, select: Selector, changes: EntityChanges) -> R {
        if changes.is_empty() {
            return Err(
                "nothing to update: give config, provider_config, manual, providers or extra (a bulk update cannot rename)"
                    .into(),
            );
        }
        let ids = self.select_entities(&select)?;
        let total = ids.len();
        let done = self.run_bulk(&ids, |app, id| {
            let before = app.entity_json(id);
            let r = app.agent_exec(AgentCommand::EntityUpdate {
                entity: id.clone(),
                name: None,
                config: changes.config.clone(),
                provider_config: changes.provider_config.clone(),
                manual: changes.manual,
                providers: changes.providers.clone(),
                extra: changes.extra.clone(),
                extra_provider: changes.extra_provider.clone(),
                extra_block: changes.extra_block.clone(),
            })?;
            let after = app.entity_json(id);
            Ok(json!({
                "id": id,
                "name": after["name"],
                "type": after["type"],
                "changed": before != after,
                "diagnostics": r["diagnostics"],
            }))
        });
        let entities = done.map_err(|refused| self.refusals("the bulk entity_update", total, &refused))?;
        let changed = entities.iter().filter(|e| e["changed"] == json!(true)).count();
        Ok(json!({
            "status": "updated",
            "selected": total,
            "changed": changed,
            "unchanged": total - changed,
            "entities": entities,
        }))
    }

    /// `link_add { select, relation, target }`: link every match to one target as one undo
    /// step — the case that used to take 24 calls. An entity that is the target itself,
    /// or already inside a container that implies the link, is skipped and named; one
    /// whose type may not have that relation to the target refuses the whole call.
    pub(super) fn bulk_link(
        &mut self,
        select: Selector,
        target: String,
        relation: String,
        providers: Option<Vec<String>>,
    ) -> R {
        let t = self.resolve(&target)?;
        let ids = self.select_entities(&select)?;
        let total = ids.len();
        let done = self.run_bulk(&ids, |app, id| {
            if *id == t {
                return Ok((id.clone(), "skipped", Some("it is the target itself".to_string())));
            }
            match app.agent_exec(AgentCommand::LinkAdd {
                source: id.clone(),
                target: t.clone(),
                relation: relation.clone(),
                providers: providers.clone(),
            }) {
                Ok(v) if v["status"] == json!("linked") => Ok((id.clone(), "linked", None)),
                Ok(_) => Ok((id.clone(), "already_linked", None)),
                Err(e) if e.starts_with("redundant:") => Ok((id.clone(), "skipped", Some(e))),
                Err(e) => Err(e),
            }
        });
        let rows = done.map_err(|refused| self.refusals("the bulk link_add", total, &refused))?;
        let named = |what: &str| -> Vec<String> {
            rows.iter()
                .filter(|(_, s, _)| *s == what)
                .map(|(id, _, _)| {
                    self.project
                        .entity(id)
                        .map(|e| e.name.to_string())
                        .unwrap_or_default()
                })
                .collect()
        };
        let linked = named("linked");
        Ok(json!({
            "status": if linked.is_empty() { "no new links" } else { "linked" },
            "target": t,
            "relation": relation,
            "selected": total,
            "linked": linked,
            "already_linked": named("already_linked"),
            "skipped": rows.iter().filter(|(_, s, _)| *s == "skipped").map(|(id, _, why)| json!({
                "name": self.project.entity(id).map(|e| e.name.to_string()),
                "reason": why,
            })).collect::<Vec<_>>(),
        }))
    }

    // ------------------------------------------------------------------ reads

    /// `project_get { fields, entities }`: top-level keys of the project, and/or only
    /// the entities named (with the links among them). Naming entities alone returns
    /// `containers`, `nodes` and `edges`; the default call is unchanged.
    pub(super) fn project_slice(&self, fields: Option<Vec<String>>, entities: Option<Vec<String>>) -> R {
        const KEYS: [&str; 7] = [
            "schema_version",
            "name",
            "settings",
            "containers",
            "nodes",
            "edges",
            "views",
        ];
        if let Some(f) = &fields {
            if let Some(bad) = f.iter().find(|k| !KEYS.contains(&k.as_str())) {
                return Err(format!(
                    "no project field \"{bad}\" (fields: {})",
                    KEYS.join(", ")
                ));
            }
        }
        let mut whole = serde_json::to_value(&self.project).map_err(|e| e.to_string())?;
        let obj = whole
            .as_object_mut()
            .ok_or("the project did not serialise to an object")?;
        if let Some(names) = &entities {
            if names.is_empty() {
                return Err(
                    "`entities` is empty: name the entities you want, or leave it out for everything".into(),
                );
            }
            let keep: BTreeSet<Id> = names.iter().map(|n| self.resolve(n)).collect::<Result<_, _>>()?;
            for key in ["nodes", "containers"] {
                if let Some(J::Object(m)) = obj.get_mut(key) {
                    m.retain(|id, _| keep.contains(id));
                }
            }
            if let Some(J::Array(edges)) = obj.get_mut("edges") {
                edges.retain(|e| {
                    let end = |k: &str| e[k].as_str().is_some_and(|id| keep.contains(id));
                    end("source") && end("target")
                });
            }
        }
        let wanted: Vec<String> = match (fields, &entities) {
            (Some(f), _) => f,
            (None, Some(_)) => ["containers", "nodes", "edges"].map(String::from).to_vec(),
            (None, None) => KEYS.map(String::from).to_vec(),
        };
        obj.retain(|k, _| wanted.iter().any(|w| w == k));
        Ok(whole)
    }

    /// `diagnostics { entity, severity, provider }`. `provider` picks whose run is
    /// answered (the target's, from the cache, by default); the filters apply to both
    /// the provider's own list and the list of what the other providers would say.
    pub(super) fn diagnostics_filtered(
        &mut self,
        entity: Option<String>,
        severity: Option<String>,
        provider: Option<String>,
    ) -> R {
        let want_severity = severity
            .map(|s| match s.to_lowercase().as_str() {
                "error" | "errors" => Ok(Severity::Error),
                "warning" | "warnings" | "warn" => Ok(Severity::Warning),
                "info" => Ok(Severity::Info),
                other => Err(format!("unknown severity \"{other}\" (error | warning | info)")),
            })
            .transpose()?;
        let want_entity = entity.map(|e| self.resolve(&e)).transpose()?;
        let target = self.project.settings.target_provider.clone();
        let provider = provider.unwrap_or_else(|| target.clone());
        if self.catalog.provider(&provider).is_none() {
            return Err(format!(
                "unknown provider \"{provider}\" (known: {})",
                self.catalog.provider_ids().join(", ")
            ));
        }
        let (mine, others) = if provider == target {
            self.refresh_diagnostics();
            (self.diagnostics.clone(), self.other_diagnostics().to_vec())
        } else {
            (
                ttg_codegen::diagnostics::run(&self.project, &self.catalog, &provider),
                ttg_codegen::diagnostics::other_providers(&self.project, &self.catalog, &provider),
            )
        };
        let keep = |d: &&Diagnostic| {
            want_severity.is_none_or(|s| d.severity == s)
                && want_entity.as_ref().is_none_or(|e| d.entity.as_ref() == Some(e))
        };
        let pick = |ds: &[Diagnostic]| -> Vec<J> {
            ds.iter()
                .filter(keep)
                .map(|d| diag_json(&self.project, d))
                .collect()
        };
        let (mine_out, others_out) = (pick(&mine), pick(&others));
        Ok(json!({
            "provider": provider,
            "target_provider": target,
            "matched": {"diagnostics": mine_out.len(), "other_providers": others_out.len()},
            "of": {"diagnostics": mine.len(), "other_providers": others.len()},
            "diagnostics": mine_out,
            "other_providers": others_out,
        }))
    }

    /// `catalog_relations { source_type, target_type }`: what the definitions let one
    /// type have to another, with label, cardinality and provider scope.
    pub(super) fn catalog_relations_json(
        &mut self,
        source_type: Option<String>,
        target_type: Option<String>,
    ) -> R {
        for t in [&source_type, &target_type].into_iter().flatten() {
            if Catalog::is_native(t) {
                self.catalog.ensure_native(t);
            }
            if self.catalog.resource(t).is_none() {
                return Err(format!("unknown type \"{t}\" (see catalog_types)"));
            }
        }
        let mut rows = Vec::new();
        for (id, def) in &self.catalog.resources {
            if source_type.as_ref().is_some_and(|s| s != id) {
                continue;
            }
            for r in &def.relations {
                if target_type.as_ref().is_some_and(|t| !r.targets.contains(t)) {
                    continue;
                }
                rows.push(json!({
                    "source_type": id,
                    "source_name": def.resource.display_name,
                    "relation": r.kind,
                    "label": r.label.clone().or_else(|| {
                        ttg_core::Relation::from_key(&r.kind).map(|k| k.display_name().to_string())
                    }),
                    "targets": r.targets,
                    "cardinality": format!("{:?}", r.cardinality).to_lowercase(),
                    "min_targets": r.min_targets,
                    "via_parent": r.via_parent,
                    // Empty = every provider.
                    "providers": r.providers,
                }));
            }
        }
        Ok(json!({
            "source_type": source_type,
            "target_type": target_type,
            "relations": rows,
            "always_allowed": ["depends_on"],
            "note": "`depends_on` (ordering only) is allowed between any two entities; `via_parent` means being inside a container of a target type already satisfies the relation, so no link is needed",
        }))
    }

    /// `entity_preview { entity, provider }`: just this entity's blocks.
    pub(super) fn entity_preview_json(&mut self, entity: &str, provider: Option<String>) -> R {
        let id = self.resolve(entity)?;
        let provider = provider.unwrap_or_else(|| self.project.settings.target_provider.clone());
        let g = ttg_codegen::generate(
            &self.project,
            &self.catalog,
            &provider,
            self.project.settings.tool,
        )
        .map_err(|e| gen_error_text(&e))?;
        let pv = g.entity_preview(&self.project, &self.catalog, &id);
        let e = self.project.entity(&id);
        Ok(json!({
            "entity": id,
            "name": e.map(|e| e.name.to_string()),
            "type": e.map(|e| e.resource_type.to_string()),
            "provider": provider,
            "tool": g.tool,
            "file": pv.file,
            "addresses": pv.addresses,
            "hcl": pv.hcl,
            "no_blocks": pv.no_blocks,
            "manual_steps": pv.manual_steps.iter().map(|m| json!({"title": m.title, "body": m.body})).collect::<Vec<_>>(),
            "diagnostics": pv.diagnostics.iter().map(|d| diag_json(&self.project, d)).collect::<Vec<_>>(),
        }))
    }

    // ------------------------------------------------------------------ flows

    /// Add a flow, and say so when an end is hidden in the view it lands in. The flow is
    /// accepted either way — annotations are never exported and the view may be about to
    /// show that entity — but a hidden end used to pass without a word and leave a
    /// numbered step with nothing to draw it against. With `show_hidden` the entity is
    /// added to the view's filter first (see [`ttg_codegen::views::reveal`]: out of
    /// `hidden`, into a non-empty `only`, nothing else touched) in the same undo step;
    /// a filter part that defines the view is not rewritten, and the warning says which.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn add_flow_checked(
        &mut self,
        from: FlowEnd,
        to: FlowEnd,
        label: &str,
        dashed: bool,
        step: Option<u32>,
        color: Option<String>,
        show_hidden: bool,
    ) -> R {
        if from == to {
            return Err("a flow needs two different ends".into());
        }
        let hidden: Vec<Id> = [&from, &to]
            .into_iter()
            .filter_map(|end| match end {
                FlowEnd::Entity { entity } if !self.is_visible(entity) => Some(entity.clone()),
                _ => None,
            })
            .collect();
        let view = self.view_name().unwrap_or_default();

        let (flow_id, shown, refused) = if show_hidden && !hidden.is_empty() {
            // One undo step for the filter change and the flow together.
            let before = self.snapshot();
            let parked = std::mem::take(&mut self.history);
            self.mcp.quiet = true;
            let (shown, refused) = self.reveal_in_active_view(&hidden);
            let added = self.add_flow(from, to, label, dashed, step, color);
            self.mcp.quiet = false;
            self.history = parked;
            self.finish(before);
            (added.ok_or("a flow needs two different ends")?, shown, refused)
        } else {
            let refused: Reasons = hidden
                .iter()
                .map(|id| {
                    (
                        id.clone(),
                        hidden_because(&self.project, &self.catalog, &self.filter, id),
                    )
                })
                .collect();
            let id = self
                .add_flow(from, to, label, dashed, step, color)
                .ok_or("a flow needs two different ends")?;
            (id, Vec::new(), refused)
        };

        let name = |app: &TtgApp, id: &str| {
            app.project
                .entity(id)
                .map(|e| e.name.to_string())
                .unwrap_or_else(|| id.to_string())
        };
        let mut out = json!({"status": "flow added", "id": flow_id, "in_view": self.view_name()});
        if !shown.is_empty() {
            out["shown"] = json!(shown.iter().map(|(id, _)| name(self, id)).collect::<Vec<_>>());
            out["filter_changes"] = json!(shown
                .iter()
                .map(|(id, c)| json!({"entity": name(self, id), "changes": c}))
                .collect::<Vec<_>>());
        }
        if !refused.is_empty() {
            out["hidden_ends"] = json!(refused.iter().map(|(id, _)| name(self, id)).collect::<Vec<_>>());
            out["warning"] = json!(self.hidden_flow_warning(&view, &refused, show_hidden));
        }
        Ok(out)
    }

    /// Make each of `ids` visible in the active view with the least change to its
    /// filter, writing the result back to the view. Returns what was changed per entity
    /// and what could not be, with the reasons.
    fn reveal_in_active_view(&mut self, ids: &[Id]) -> (Reasons, Reasons) {
        let mut filter = self.filter.clone();
        let (mut shown, mut refused) = (Vec::new(), Vec::new());
        for id in ids {
            match reveal(&self.project, &self.catalog, &filter, id) {
                Reveal::Visible => {}
                Reveal::Changed { filter: f, changes } => {
                    filter = f;
                    shown.push((id.clone(), changes));
                }
                Reveal::Blocked(why) => refused.push((id.clone(), why)),
            }
        }
        if !shown.is_empty() {
            self.set_filter(filter);
        }
        (shown, refused)
    }

    fn hidden_flow_warning(&self, view: &str, hidden: &[(Id, Vec<String>)], asked: bool) -> String {
        let parts: Vec<String> = hidden
            .iter()
            .map(|(id, why)| {
                let name = self.project.entity(id).map(|e| e.name).unwrap_or(id.as_str());
                let why = if why.is_empty() {
                    "it is a container with nothing shown inside it".to_string()
                } else {
                    why.join("; ")
                };
                format!("\"{name}\" ({why})")
            })
            .collect();
        let (is, it, them) = if hidden.len() == 1 {
            ("is", "it", "it")
        } else {
            ("are", "them", "them")
        };
        let fix = if asked {
            format!(
                "show_hidden only takes {it} out of `hidden` and into `only`; the reasons above come from parts of the filter that define the view, so change {them} with view_update {{ view: \"{view}\", filter }} if the view should show {it}."
            )
        } else {
            format!(
                "Show {them} with view_update {{ view: \"{view}\", filter }} (take {it} out of `hidden`, or add {it} to `only`), or repeat this call with show_hidden: true."
            )
        };
        format!(
            "{} {is} hidden in the view \"{view}\". The flow was stored and view_export lists it, but the canvas has nothing to draw it to. {fix}",
            parts.join(" and "),
        )
    }
}
