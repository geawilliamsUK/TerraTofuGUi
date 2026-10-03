//! Environments and plans in the GUI: the environment selector in the view bar, the
//! per-environment values in the inspector (a badge on a field the current environment
//! overrides, editing that environment's value, resetting it to the base), the
//! Environments section of the settings, and the Plan button of the export window with
//! its +, ~, −, ± badges on the canvas.
//!
//! With an environment selected the inspector shows — and edits — that environment's
//! values; the diagnostics, the reachability overlay and the cost estimate answer for it
//! (`Project::for_environment`). With none ("Base") they show the values every
//! environment starts from.

use crate::app::TtgApp;
use egui::{Color32, RichText, Ui};
use std::borrow::Cow;
use std::collections::BTreeMap;
use ttg_codegen::environments::EnvDiagnostic;
use ttg_codegen::plan_run::{PlanReport, PlanStatus, ToolDiagnostic};
use ttg_core::{Config, Id, Project, Value};

/// What `validate` said about one exported provider, attributed to entities.
#[derive(Debug, Clone)]
pub struct Validated {
    pub summary: String,
    pub diagnostics: Vec<ToolDiagnostic>,
}

/// Everything the app keeps about environments and plans.
#[derive(Default)]
pub struct EnvUi {
    /// The environment the canvas, inspector and diagnostics show; `None` = the base values.
    pub current: Option<String>,
    /// The other environments' diagnostics, computed on demand like the other providers'.
    pub other_diags: Option<Vec<EnvDiagnostic>>,
    /// Text of the "add environment" box in the settings.
    pub new_env: String,
    /// `(environment being renamed, new name)`.
    pub renaming: Option<(String, String)>,
    pub new_var: String,
    /// A plan running in the background.
    pub plan_rx: Option<std::sync::mpsc::Receiver<Result<PlanReport, String>>>,
    /// The last plan, and the canvas badges it puts on entities until the next edit.
    pub plan: Option<Result<PlanReport, String>>,
    pub badges: BTreeMap<Id, &'static str>,
    pub plan_environment: Option<String>,
    pub plan_real_backend: bool,
}

impl TtgApp {
    /// The project as the canvas shows it: resolved for the selected environment, or
    /// the base values (variables filled in) when none is selected.
    pub fn shown_project(&self) -> Cow<'_, Project> {
        self.project.resolved(self.env.current.as_deref())
    }

    /// Diagnostics for the shown environment, plus the checks of the environments and
    /// overrides as written.
    pub fn diagnostics_for_shown(&self) -> Vec<ttg_codegen::Diagnostic> {
        let provider = &self.project.settings.target_provider;
        let mut out = ttg_codegen::diagnostics::run(&self.shown_project(), &self.catalog, provider);
        if self.env.current.is_some() {
            for d in ttg_codegen::environments::checks(&self.project, &self.catalog) {
                if !out.contains(&d) {
                    out.push(d);
                }
            }
        }
        out
    }

    /// The diagnostics of the environments not shown, and the differences between
    /// environments an export would refuse; computed when asked, cached until an edit.
    pub fn other_environment_diagnostics(&mut self) -> &[EnvDiagnostic] {
        self.refresh_diagnostics();
        if self.env.other_diags.is_none() {
            self.env.other_diags = Some(ttg_codegen::environments::other_environments(
                &self.project,
                &self.catalog,
                &self.project.settings.target_provider,
                self.project.settings.tool,
                self.env.current.as_deref(),
            ));
        }
        self.env.other_diags.as_deref().unwrap()
    }

    /// Show another environment (or the base with `None`).
    pub fn set_environment(&mut self, env: Option<String>) {
        if self.env.current != env {
            self.env.current = env;
            self.diag_dirty = true;
            self.env.other_diags = None;
            self.reach = None;
        }
    }

    /// Forget an environment the project no longer has.
    pub fn check_environment(&mut self) {
        if let Some(e) = &self.env.current {
            if !self.project.settings.environments.contains(e) {
                self.set_environment(None);
            }
        }
    }

    /// A field's value as shown, and whether the shown environment overrides it.
    pub fn shown_field(&self, id: &str, provider: Option<&str>, field: &str) -> (Option<Value>, bool) {
        let e = self.project.entity(id);
        let base = e.and_then(|e| match provider {
            Some(p) => e.provider_field(p, field).cloned(),
            None => e.field(field).cloned(),
        });
        let Some(env) = &self.env.current else {
            return (base, false);
        };
        let o = self
            .project
            .overrides_of(id)
            .and_then(|o| o.get(env))
            .and_then(|o| match provider {
                Some(p) => o.provider_config.get(p).and_then(|c| c.get(field)),
                None => o.config.get(field),
            })
            .cloned();
        match o {
            Some(v) => (Some(v), true),
            None => (base, false),
        }
    }

    /// The map an inspector edit writes into: the shown environment's overrides, or the
    /// entity's own values when no environment is shown.
    pub fn config_for_edit(&mut self, id: &str, provider: Option<&str>) -> &mut Config {
        let env = self.env.current.clone();
        if let Some(env) = env {
            let o = self
                .project
                .overrides_mut(id)
                .expect("entity exists")
                .entry(env)
                .or_default();
            return match provider {
                Some(p) => o.provider_config.entry(p.to_string()).or_default(),
                None => &mut o.config,
            };
        }
        if let Some(n) = self.project.nodes.get_mut(id) {
            match provider {
                Some(p) => n.provider_config.entry(p.to_string()).or_default(),
                None => &mut n.config,
            }
        } else {
            let c = self.project.containers.get_mut(id).expect("entity exists");
            match provider {
                Some(p) => c.provider_config.entry(p.to_string()).or_default(),
                None => &mut c.config,
            }
        }
    }

    /// Drop the shown environment's value for a field, back to the base.
    pub fn reset_override(&mut self, id: &str, provider: Option<&str>, field: &str) {
        let Some(env) = self.env.current.clone() else {
            return;
        };
        let before = self.snapshot();
        if let Some(o) = self.project.overrides_mut(id).and_then(|o| o.get_mut(&env)) {
            match provider {
                Some(p) => {
                    if let Some(c) = o.provider_config.get_mut(p) {
                        c.remove(field);
                    }
                }
                None => {
                    o.config.remove(field);
                }
            }
        }
        self.project.prune_overrides(id);
        self.finish(before);
    }

    /// The plan badge for an entity, if the last plan changes it.
    pub fn plan_badge(&self, id: &str) -> Option<&'static str> {
        self.env.badges.get(id).copied()
    }

    /// An edit makes the last plan stale: its badges go.
    pub fn clear_plan_badges(&mut self) {
        self.env.badges.clear();
        self.env.other_diags = None;
    }

    /// Show a plan: keep it, and badge every entity it changes.
    pub fn show_plan(&mut self, report: Result<PlanReport, String>) {
        self.env.badges = match &report {
            Ok(r) => r
                .entities
                .iter()
                .filter_map(|e| e.badge().map(|b| (e.entity.clone(), b)))
                .collect(),
            Err(_) => BTreeMap::new(),
        };
        self.status = match &report {
            Ok(r) => format!("Plan: {}", r.summary),
            Err(e) => format!("Plan failed: {e}"),
        };
        self.env.plan = Some(report);
    }

    /// The directory a plan of the target provider runs in: the last export's.
    pub fn plan_dir(&self) -> Option<std::path::PathBuf> {
        let provider = &self.project.settings.target_provider;
        let dir = self.export.dir.clone()?;
        Some(if self.export.root { dir.join(provider) } else { dir })
    }

    /// Export the target provider into the last export directory and plan it, on a
    /// background thread; [`TtgApp::poll_plan`] picks the report up.
    pub fn run_plan(&mut self) {
        if self.env.plan_rx.is_some() {
            return;
        }
        let Some(dir) = self.plan_dir() else {
            self.error = Some("Export once first: the plan runs in the export folder.".into());
            return;
        };
        let p = self.project.clone();
        let cat = self.catalog.clone();
        let provider = p.settings.target_provider.clone();
        let tool = p.settings.tool;
        let env = self
            .env
            .plan_environment
            .clone()
            .or_else(|| self.env.current.clone());
        let opts = ttg_codegen::plan_run::PlanOptions {
            real_backend: self.env.plan_real_backend,
            ..Default::default()
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let started = std::thread::Builder::new()
            .name("ttg-plan".into())
            .spawn(move || {
                let r = ttg_codegen::plan_run::export_and_plan(
                    &p,
                    &cat,
                    &provider,
                    tool,
                    &dir,
                    env.as_deref(),
                    opts,
                )
                .map_err(|e| e.to_string());
                let _ = tx.send(r);
            });
        match started {
            Ok(_) => {
                self.env.plan_rx = Some(rx);
                self.status = "Planning in the background…".into();
            }
            Err(e) => self.error = Some(format!("Could not start the plan: {e}")),
        }
    }

    /// Collect a finished plan. Called once a frame.
    pub fn poll_plan(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.env.plan_rx else { return };
        match rx.try_recv() {
            Ok(r) => {
                self.env.plan_rx = None;
                self.show_plan(r);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(250));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.env.plan_rx = None;
                self.show_plan(Err("the plan thread stopped without an answer".into()));
            }
        }
    }
}

// ------------------------------------------------------------------ widgets

const OVERRIDE: Color32 = Color32::from_rgb(150, 90, 200);

/// The environment selector in the view bar: Base, then each environment.
pub fn selector(app: &mut TtgApp, ui: &mut Ui) {
    if app.project.settings.environments.is_empty() {
        return;
    }
    app.check_environment();
    ui.label(RichText::new("Env").small().color(Color32::from_gray(110)))
        .on_hover_text("The environment the canvas, inspector, diagnostics and cost show. Edits to fields go to the shown environment; Base edits the values every environment starts from.");
    let current = app.env.current.clone();
    egui::ComboBox::from_id_salt("environment")
        .selected_text(current.clone().unwrap_or_else(|| "Base".into()))
        .width(90.0)
        .show_ui(ui, |ui| {
            if ui.selectable_label(current.is_none(), "Base").clicked() {
                app.set_environment(None);
            }
            for e in app.project.settings.environments.clone() {
                let n = app
                    .project
                    .entities()
                    .iter()
                    .filter(|x| app.project.overrides_of(x.id).is_some_and(|o| o.contains_key(&e)))
                    .count();
                let label = if n > 0 {
                    format!("{e}  ({n} overridden)")
                } else {
                    e.clone()
                };
                if ui
                    .selectable_label(current.as_deref() == Some(&e), label)
                    .clicked()
                {
                    app.set_environment(Some(e));
                }
            }
        });
    ui.separator();
}

/// A badge next to a field the shown environment overrides, with a reset button.
pub fn override_marker(
    app: &mut TtgApp,
    ui: &mut Ui,
    id: &str,
    provider: Option<&str>,
    field: &str,
    overridden: bool,
) {
    let Some(env) = app.env.current.clone() else {
        return;
    };
    if !overridden {
        return;
    }
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("● {env}")).small().color(OVERRIDE))
            .on_hover_text(format!(
                "Overridden in {env}; the other environments keep the base value."
            ));
        if ui
            .small_button("reset")
            .on_hover_text("Use the base value in this environment")
            .clicked()
        {
            app.reset_override(id, provider, field);
        }
    });
}

/// "In <env>" for an entity: unticked, the entity is absent from that environment.
pub fn presence_row(app: &mut TtgApp, ui: &mut Ui, id: &str) {
    let Some(env) = app.env.current.clone() else {
        return;
    };
    let mut present = !app.project.absent_in(id, &env);
    ui.label("In this environment");
    if ui
        .checkbox(&mut present, env.as_str())
        .on_hover_text("Untick to leave this resource, and its links, out of this environment: the export counts it (count = 0 there).")
        .changed()
    {
        let before = app.snapshot();
        if let Some(o) = app.project.overrides_mut(id) {
            o.entry(env).or_default().absent = !present;
        }
        app.project.prune_overrides(id);
        app.finish(before);
    }
    ui.end_row();
}

/// Environment tags of a link (none ticked = every environment).
pub fn edge_environments(app: &mut TtgApp, ui: &mut Ui, i: usize) {
    let envs = app.project.settings.environments.clone();
    if envs.is_empty() || i >= app.project.edges.len() {
        return;
    }
    ui.label("Environments")
        .on_hover_text("Tick the environments this link belongs to; none ticked means every environment.");
    ui.horizontal_wrapped(|ui| {
        for e in envs {
            let mut on = app.project.edges[i].environments.contains(&e);
            if ui.checkbox(&mut on, e.as_str()).changed() {
                let before = app.snapshot();
                let tags = &mut app.project.edges[i].environments;
                if on {
                    tags.push(e.clone());
                } else {
                    tags.retain(|x| x != &e);
                }
                app.finish(before);
            }
        }
    });
    ui.end_row();
}

/// The Environments section of the settings: the list, the name prefix, the variables.
pub fn settings_section(app: &mut TtgApp, ui: &mut Ui) {
    egui::CollapsingHeader::new("Environments")
        .default_open(!app.project.settings.environments.is_empty())
        .show(ui, |ui| {
            ui.label(
                RichText::new(
                    "One diagram, several environments: fields can differ per environment (pick it in the view bar, then edit), resources can be left out of one, and the export writes one configuration with a .tfvars and a state key per environment.",
                )
                .small()
                .color(Color32::from_gray(110)),
            );
            let envs = app.project.settings.environments.clone();
            let mut remove: Option<String> = None;
            let mut rename: Option<(String, String)> = None;
            for e in &envs {
                ui.horizontal(|ui| {
                    match &mut app.env.renaming {
                        Some((from, to)) if from == e => {
                            ui.add(egui::TextEdit::singleline(to).desired_width(110.0));
                            if ui.small_button("OK").clicked() {
                                rename = Some((from.clone(), to.trim().to_string()));
                            }
                        }
                        _ => {
                            ui.label(e);
                            if ui.small_button("rename").clicked() {
                                app.env.renaming = Some((e.clone(), e.clone()));
                            }
                        }
                    }
                    if ui
                        .small_button("remove")
                        .on_hover_text("Remove the environment and every override for it")
                        .clicked()
                    {
                        remove = Some(e.clone());
                    }
                });
            }
            if let Some((from, to)) = rename {
                let before = app.snapshot();
                match app.project.rename_environment(&from, &to) {
                    Ok(()) => {
                        if app.env.current.as_deref() == Some(from.as_str()) {
                            app.env.current = Some(to);
                        }
                        app.env.renaming = None;
                        app.finish(before);
                    }
                    Err(e) => app.error = Some(e),
                }
            }
            if let Some(gone) = remove {
                let before = app.snapshot();
                let keep: Vec<String> = envs.iter().filter(|e| **e != gone).cloned().collect();
                if let Err(e) = app.project.set_environments(keep) {
                    app.error = Some(e);
                }
                app.finish(before);
                app.check_environment();
            }
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut app.env.new_env)
                        .hint_text("staging")
                        .desired_width(110.0),
                );
                if ui.button("Add environment").clicked() {
                    let name = app.env.new_env.trim().to_string();
                    let mut list = envs.clone();
                    list.push(name);
                    let before = app.snapshot();
                    match app.project.set_environments(list) {
                        Ok(_) => {
                            app.env.new_env.clear();
                            app.finish(before);
                        }
                        Err(e) => app.error = Some(e),
                    }
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Name prefix").on_hover_text(
                    "Put in front of every generated resource name, so environments can share an account. May use ${var.environment} and project variables, e.g. zipos-${var.environment}.",
                );
                let mut s = app.project.settings.name_prefix.clone().unwrap_or_default();
                let r = ui.add(egui::TextEdit::singleline(&mut s).hint_text("app-${var.environment}"));
                if r.changed() {
                    let before = app.snapshot();
                    app.project.settings.name_prefix = (!s.trim().is_empty()).then(|| s.trim().to_string());
                    app.finish(before);
                }
            });
            variables_table(app, ui);
        });
}

/// Project variables: one row each, a base value and a value per environment.
fn variables_table(app: &mut TtgApp, ui: &mut Ui) {
    ui.add_space(4.0);
    ui.label(RichText::new("Variables").strong()).on_hover_text(
        "Declared once, used in any field as ${var.<name>}. Where environments give them different values the export declares a Terraform variable of the same name; ${var.environment} and ${var.name_prefix} are always there.",
    );
    let envs = app.project.settings.environments.clone();
    let names: Vec<String> = app.project.settings.variables.keys().cloned().collect();
    let mut remove: Option<String> = None;
    egui::Grid::new("project-variables").striped(true).show(ui, |ui| {
        ui.label(RichText::new("name").small());
        ui.label(RichText::new("base").small());
        for e in &envs {
            ui.label(RichText::new(e).small());
        }
        ui.label("");
        ui.end_row();
        for name in &names {
            ui.label(name);
            let base = app.project.settings.variables[name].value.display();
            if let Some(v) = text_cell(ui, ("pv", name, "base"), &base) {
                let before = app.snapshot();
                app.project.settings.variables.get_mut(name).unwrap().value = parse_value(&v);
                app.finish(before);
            }
            for e in &envs {
                let cur = app.project.settings.variables[name]
                    .environments
                    .get(e)
                    .map(|v| v.display())
                    .unwrap_or_default();
                if let Some(v) = text_cell(ui, ("pv", name, e.as_str()), &cur) {
                    let before = app.snapshot();
                    let var = app.project.settings.variables.get_mut(name).unwrap();
                    if v.trim().is_empty() {
                        var.environments.remove(e);
                    } else {
                        var.environments.insert(e.clone(), parse_value(&v));
                    }
                    app.finish(before);
                }
            }
            if ui.small_button("×").clicked() {
                remove = Some(name.clone());
            }
            ui.end_row();
        }
    });
    if let Some(n) = remove {
        let before = app.snapshot();
        app.project.settings.variables.remove(&n);
        app.finish(before);
    }
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut app.env.new_var)
                .hint_text("db_instance_class")
                .desired_width(130.0),
        );
        if ui.button("Add variable").clicked() {
            let name = app.env.new_var.trim().to_string();
            match ttg_core::environment::check_variable_name(&name) {
                Ok(()) => {
                    let before = app.snapshot();
                    app.project.settings.variables.insert(
                        name,
                        ttg_core::ProjectVariable {
                            value: Value::Str(String::new()),
                            description: String::new(),
                            environments: Default::default(),
                        },
                    );
                    app.env.new_var.clear();
                    app.finish(before);
                }
                Err(e) => app.error = Some(e),
            }
        }
    });
}

/// A text cell that reports its new text once editing ends.
fn text_cell(ui: &mut Ui, salt: impl std::hash::Hash, value: &str) -> Option<String> {
    let id = ui.make_persistent_id(salt);
    let mut s = ui
        .data_mut(|d| d.get_temp::<String>(id))
        .unwrap_or_else(|| value.to_string());
    let r = ui.add(egui::TextEdit::singleline(&mut s).desired_width(90.0));
    if r.has_focus() {
        ui.data_mut(|d| d.insert_temp(id, s.clone()));
        None
    } else {
        ui.data_mut(|d| d.remove::<String>(id));
        (r.lost_focus() && s != value).then_some(s)
    }
}

/// A variable's text as a value: a number or a boolean when it reads as one.
fn parse_value(s: &str) -> Value {
    let t = s.trim();
    if let Ok(i) = t.parse::<i64>() {
        Value::Int(i)
    } else if t == "true" || t == "false" {
        Value::Bool(t == "true")
    } else {
        Value::Str(t.to_string())
    }
}

/// Plan badges and absence on the canvas, drawn over one entity.
pub fn draw_entity_marks(app: &TtgApp, painter: &egui::Painter, sr: egui::Rect, zoom: f32, id: &str) {
    if let Some(env) = &app.env.current {
        if app.project.absent_in(id, env) {
            painter.rect_filled(sr, 6.0, Color32::from_rgba_unmultiplied(246, 247, 249, 190));
            painter.text(
                egui::Pos2::new(sr.center().x, sr.max.y - 8.0 * zoom),
                egui::Align2::CENTER_CENTER,
                format!("not in {env}"),
                egui::FontId::proportional((10.0 * zoom).max(6.0)),
                OVERRIDE,
            );
        }
    }
    if let Some(b) = app.plan_badge(id) {
        let color = match b {
            "+" => Color32::from_rgb(40, 150, 70),
            "~" => Color32::from_rgb(200, 140, 20),
            "\u{2212}" => Color32::from_rgb(200, 50, 50),
            _ => Color32::from_rgb(150, 90, 200),
        };
        let c = egui::Pos2::new(sr.min.x + 12.0 * zoom, sr.min.y + 12.0 * zoom);
        let r = 8.0 * zoom.max(0.7);
        painter.circle_filled(c, r, color);
        painter.text(
            c,
            egui::Align2::CENTER_CENTER,
            b,
            egui::FontId::proportional((12.0 * zoom).max(6.0)),
            Color32::WHITE,
        );
    }
}

/// The tool's diagnostics, attributed: entity first, then where in the files.
pub fn tool_diagnostics(ui: &mut Ui, diags: &[ToolDiagnostic]) {
    for d in diags {
        let color = if d.severity == "error" {
            Color32::from_rgb(200, 50, 50)
        } else {
            Color32::from_rgb(200, 140, 20)
        };
        let at = match (&d.entity_name, &d.file, d.line) {
            (Some(n), Some(f), Some(l)) => format!("{n} ({f}:{l})"),
            (Some(n), _, _) => n.clone(),
            (None, Some(f), Some(l)) => format!("{f}:{l}"),
            _ => String::new(),
        };
        let r = ui.label(RichText::new(format!("{} {at}: {}", d.severity, d.summary)).color(color));
        let mut tip = d.detail.clone();
        if let Some(def) = &d.definition {
            tip.push_str(&format!("\n\nMapping: {def}"));
        }
        if !tip.trim().is_empty() {
            r.on_hover_text(tip);
        }
    }
}

/// The Plan part of the export window.
pub fn plan_section(app: &mut TtgApp, ui: &mut Ui) {
    ui.separator();
    ui.horizontal(|ui| {
        let running = app.env.plan_rx.is_some();
        let available = app.tool_binary_available() && app.plan_dir().is_some();
        let bin = ttg_codegen::Profile::new(app.project.settings.tool).binary();
        if ui
            .add_enabled(available && !running, egui::Button::new(format!("Plan with `{bin}`")))
            .on_hover_text("Export the target provider again, then init and plan it in the background. Without 'real backend' the plan runs in a scratch copy with local state: what applying would create from nothing. A plan reads the cloud account, so the provider needs credentials.")
            .on_disabled_hover_text(if running {
                "A plan is already running.".to_string()
            } else {
                format!("Needs `{bin}` and a previous export of the target provider.")
            })
            .clicked()
        {
            app.run_plan();
        }
        if running {
            ui.spinner();
            ui.label("planning…");
        }
        let envs = app.project.settings.environments.clone();
        if !envs.is_empty() {
            let shown = app
                .env
                .plan_environment
                .clone()
                .or_else(|| app.env.current.clone())
                .unwrap_or_else(|| envs[0].clone());
            egui::ComboBox::from_id_salt("plan-env")
                .selected_text(shown.clone())
                .show_ui(ui, |ui| {
                    for e in envs {
                        if ui.selectable_label(shown == e, e.as_str()).clicked() {
                            app.env.plan_environment = Some(e);
                        }
                    }
                });
        }
        ui.checkbox(&mut app.env.plan_real_backend, "real backend");
    });
    let Some(plan) = app.env.plan.clone() else { return };
    match plan {
        Err(e) => {
            ui.label(RichText::new(e).color(Color32::from_rgb(200, 50, 50)));
        }
        Ok(r) => {
            let color = match r.status {
                PlanStatus::Planned => Color32::from_rgb(40, 130, 60),
                PlanStatus::NoCredentials => Color32::from_rgb(200, 140, 20),
                _ => Color32::from_rgb(200, 50, 50),
            };
            ui.label(RichText::new(&r.summary).color(color));
            egui::ScrollArea::vertical()
                .id_salt("plan-entities")
                .max_height(220.0)
                .show(ui, |ui| {
                    egui::Grid::new("plan-grid").striped(true).show(ui, |ui| {
                        for e in &r.entities {
                            ui.label(e.badge().unwrap_or(" "));
                            ui.label(&e.name);
                            let mut parts = Vec::new();
                            for (n, what) in [
                                (e.create, "create"),
                                (e.update, "update"),
                                (e.replace, "replace"),
                                (e.delete, "delete"),
                                (e.read, "read"),
                            ] {
                                if n > 0 {
                                    parts.push(format!("{n} {what}"));
                                }
                            }
                            ui.label(parts.join(", ")).on_hover_text(
                                e.addresses
                                    .iter()
                                    .map(|a| format!("{} ({})", a.address, a.change))
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            );
                            ui.end_row();
                        }
                    });
                    tool_diagnostics(ui, &r.diagnostics);
                });
        }
    }
}

/// The other environments' diagnostics in the diagnostics panel.
pub fn other_environments_section(app: &mut TtgApp, ui: &mut Ui) {
    if app.project.settings.environments.is_empty() {
        return;
    }
    let others = app.other_environment_diagnostics().to_vec();
    egui::CollapsingHeader::new(format!("Other environments ({})", others.len()))
        .id_salt("other-envs")
        .show(ui, |ui| {
            for d in others {
                let r = ui.label(
                    RichText::new(format!(
                        "{} {}",
                        crate::inspector::sev_glyph(d.diagnostic.severity),
                        d.diagnostic.message
                    ))
                    .color(crate::inspector::sev_color(d.diagnostic.severity)),
                );
                if let Some(id) = d.diagnostic.entity.clone() {
                    if r.interact(egui::Sense::click()).clicked() {
                        app.selection.clear();
                        app.selection.insert(id);
                    }
                }
            }
        });
}
