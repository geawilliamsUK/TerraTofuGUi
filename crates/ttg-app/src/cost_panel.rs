//! The Cost window: the monthly estimate (`ttg_codegen::cost`) for one provider, with
//! the usage assumptions editable in place and the price date shown where it cannot be
//! missed. The estimate is recomputed only when the project or the provider changes.

use crate::app::TtgApp;
use egui::{Color32, RichText, Ui};
use std::collections::BTreeMap;
use ttg_codegen::cost::{self, Estimate, Source, Status, ASSUMPTIONS};
use ttg_core::{DisplayCurrency, Project};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Mode {
    #[default]
    Entity,
    Type,
    View,
}

/// Window state.
#[derive(Default)]
pub struct CostUi {
    pub open: bool,
    /// Provider priced; `None` = the target provider.
    provider: Option<String>,
    mode: Mode,
    /// The project and provider the cached estimate is for.
    cache: Option<(Project, String, Result<Estimate, String>)>,
}

const MUTED: Color32 = Color32::from_gray(110);
const DATE: Color32 = Color32::from_rgb(160, 90, 0);

fn money(x: f64) -> String {
    format!("${x:.2}")
}

impl TtgApp {
    /// The estimate for the window's provider, recomputed when the project changed.
    fn cost_estimate(&mut self) -> (String, Result<Estimate, String>) {
        let provider = self
            .cost
            .provider
            .clone()
            .unwrap_or_else(|| self.project.settings.target_provider.clone());
        let fresh = self
            .cost
            .cache
            .as_ref()
            .is_some_and(|(p, pr, _)| pr == &provider && p == &self.project);
        if !fresh {
            let est = cost::estimate(&self.project, &self.catalog, &provider, None);
            self.cost.cache = Some((self.project.clone(), provider.clone(), est));
        }
        let (_, _, est) = self.cost.cache.as_ref().unwrap();
        (provider, est.clone())
    }

    /// Open one undo step when a drag or typing session starts on `r`.
    fn cost_edit_begin(&mut self, r: &egui::Response) {
        if (r.drag_started() || r.gained_focus()) && self.edit_snapshot.is_none() {
            self.edit_snapshot = Some(self.snapshot());
        }
    }

    /// Close it when the session ends.
    fn cost_edit_end(&mut self, r: &egui::Response) {
        if r.drag_stopped() || r.lost_focus() {
            if let Some(before) = self.edit_snapshot.take() {
                self.finish(before);
            }
        }
    }

    /// Set one assumption (project-wide when `entity` is `None`); `None` clears the
    /// override. Inside a session it joins that session's undo step, otherwise it is one
    /// of its own.
    fn write_assumption(&mut self, entity: Option<&str>, key: &str, value: Option<f64>) {
        let before = self.edit_snapshot.is_none().then(|| self.snapshot());
        let a = &mut self.project.settings.cost_assumptions;
        let map = match entity {
            None => &mut a.values,
            Some(id) => a.entities.entry(id.to_string()).or_default(),
        };
        match value {
            Some(v) => {
                map.insert(key.to_string(), v);
            }
            None => {
                map.remove(key);
            }
        }
        if let Some(id) = entity {
            if a.entities.get(id).is_some_and(|m| m.is_empty()) {
                a.entities.remove(id);
            }
        }
        self.dirty = true;
        self.diag_dirty = true;
        if let Some(before) = before {
            self.finish(before);
        }
    }
}

/// Draw the window when it is open.
pub fn window(app: &mut TtgApp, ctx: &egui::Context) {
    if !app.cost.open {
        return;
    }
    let mut open = true;
    egui::Window::new("Cost estimate")
        .open(&mut open)
        .default_width(720.0)
        .default_height(620.0)
        .show(ctx, |ui| cost_ui(app, ui));
    app.cost.open = open;
    if !open {
        // The cached project copy is only worth keeping while the window shows it.
        app.cost.cache = None;
    }
}

fn cost_ui(app: &mut TtgApp, ui: &mut Ui) {
    let (provider, est) = app.cost_estimate();
    ui.horizontal(|ui| {
        ui.label("Provider");
        let mut p = provider.clone();
        egui::ComboBox::from_id_salt("cost_provider")
            .selected_text(p.clone())
            .show_ui(ui, |ui| {
                for id in app.catalog.provider_ids() {
                    ui.selectable_value(&mut p, id.clone(), id);
                }
            });
        if p != provider {
            app.cost.provider = Some(p);
        }
    });
    let est = match est {
        Ok(e) => e,
        Err(e) => {
            ui.colored_label(Color32::RED, e);
            return;
        }
    };
    ui.label(
        RichText::new(format!(
            "List prices retrieved {} (USD), region {}{}",
            est.prices_retrieved,
            est.region,
            if est.price_region != est.region {
                format!(" — no prices bundled for it, {} prices used", est.price_region)
            } else {
                String::new()
            }
        ))
        .color(DATE)
        .strong(),
    );
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{} a month", money(est.monthly)))
                .size(22.0)
                .strong(),
        );
        if let Some(c) = &est.converted {
            ui.label(
                RichText::new(format!(
                    "about {:.2} {} (at {} per USD{})",
                    c.monthly,
                    c.code,
                    c.rate,
                    if c.date.is_empty() {
                        String::new()
                    } else {
                        format!(", {}", c.date)
                    }
                ))
                .size(16.0),
            );
        }
    });
    let not_est = est
        .lines
        .iter()
        .filter(|l| l.status == Status::NotEstimated)
        .count();
    if not_est > 0 {
        ui.colored_label(
            Color32::from_rgb(170, 60, 0),
            format!(
                "{not_est} resource(s) not estimated; their cost is missing from the total (listed below)"
            ),
        );
    }
    for d in app
        .diagnostics
        .iter()
        .filter(|d| d.code == ttg_codegen::Code::Cost)
    {
        ui.colored_label(Color32::from_rgb(170, 60, 0), &d.message);
    }
    ui.label(RichText::new(&est.caveat).small().color(MUTED));
    for n in &est.notes {
        ui.label(RichText::new(format!("Note: {n}")).small().color(MUTED));
    }
    ui.separator();
    ui.horizontal(|ui| {
        ui.selectable_value(&mut app.cost.mode, Mode::Entity, "By resource");
        ui.selectable_value(&mut app.cost.mode, Mode::Type, "By type");
        ui.selectable_value(&mut app.cost.mode, Mode::View, "By view");
    });
    let avail = ui.available_height();
    egui::ScrollArea::vertical()
        .id_salt("cost_rows")
        .max_height((avail * 0.55).max(160.0))
        .show(ui, |ui| match app.cost.mode {
            Mode::Entity => entity_rows(app, ui, &est),
            Mode::Type => {
                egui::Grid::new("cost_types").striped(true).show(ui, |ui| {
                    for s in &est.by_type {
                        ui.label(money(s.monthly));
                        ui.label(&s.label);
                        ui.label(RichText::new(format!("{} resource(s)", s.entities.len())).color(MUTED));
                        ui.end_row();
                    }
                });
            }
            Mode::View => {
                if est.views.is_empty() {
                    ui.label("No saved views.");
                }
                egui::Grid::new("cost_views").striped(true).show(ui, |ui| {
                    for v in &est.views {
                        ui.label(RichText::new(money(v.monthly)).strong());
                        ui.label(RichText::new(&v.view).strong());
                        ui.end_row();
                        for g in &v.groups {
                            ui.label(money(g.monthly));
                            ui.label(format!("    {}", g.label));
                            ui.end_row();
                        }
                    }
                });
            }
        });
    ui.separator();
    egui::CollapsingHeader::new("Assumptions")
        .default_open(true)
        .show(ui, |ui| assumptions_ui(app, ui, &est));
    egui::CollapsingHeader::new("Display currency").show(ui, |ui| currency_ui(app, ui));
}

fn entity_rows(app: &mut TtgApp, ui: &mut Ui, est: &Estimate) {
    let mut lines: Vec<_> = est.lines.iter().filter(|l| l.status != Status::Free).collect();
    lines.sort_by(|a, b| {
        b.monthly
            .partial_cmp(&a.monthly)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for l in lines {
        let head = match l.status {
            Status::NotEstimated => format!("   not estimated   {}  ({})", l.name, l.resource_type),
            _ => format!("{:>10}   {}  ({})", money(l.monthly), l.name, l.resource_type),
        };
        egui::CollapsingHeader::new(RichText::new(head).monospace())
            .id_salt(("cost_line", &l.entity))
            .show(ui, |ui| {
                for c in &l.charges {
                    ui.label(format!(
                        "{}  {}: {} {} at ${} ",
                        money(c.monthly),
                        c.item,
                        trim(c.quantity),
                        c.unit,
                        c.unit_price
                    ));
                }
                for n in &l.notes {
                    ui.label(RichText::new(n).small().color(MUTED));
                }
                if !l.assumptions.is_empty() {
                    ui.label(RichText::new("Assumptions for this resource").small().strong());
                }
                for u in &l.assumptions {
                    ui.horizontal(|ui| {
                        let mut v = u.value;
                        let r = ui.add(egui::DragValue::new(&mut v).speed(drag_speed(u.value)));
                        app.cost_edit_begin(&r);
                        if r.changed() {
                            app.write_assumption(Some(&l.entity), &u.key, Some(v));
                        }
                        app.cost_edit_end(&r);
                        ui.label(format!("{} ({})", u.key, u.unit));
                        if u.source == Source::Entity {
                            if ui.small_button("use the project value").clicked() {
                                app.write_assumption(Some(&l.entity), &u.key, None);
                            }
                        } else {
                            ui.label(
                                RichText::new(format!("{:?}", u.source).to_lowercase())
                                    .small()
                                    .color(MUTED),
                            );
                        }
                    });
                }
            });
    }
    let free = est.lines.iter().filter(|l| l.status == Status::Free).count();
    if free > 0 {
        ui.label(
            RichText::new(format!(
                "{free} resource(s) cost nothing here (networks, subnets, security groups, roles, ...)"
            ))
            .color(MUTED),
        );
    }
}

fn trim(x: f64) -> String {
    if x.fract() == 0.0 {
        format!("{x}")
    } else {
        format!("{x:.2}")
    }
}

fn drag_speed(v: f64) -> f64 {
    (v.abs() / 100.0).max(0.01)
}

fn assumptions_ui(app: &mut TtgApp, ui: &mut Ui, est: &Estimate) {
    ui.label(
        RichText::new(
            "Project-wide values; a resource can override one in its row above. Saved with the project.",
        )
        .small()
        .color(MUTED),
    );
    let used: BTreeMap<&str, &cost::Used> = est.assumptions.iter().map(|u| (u.key.as_str(), u)).collect();
    egui::Grid::new("cost_assumptions").striped(true).show(ui, |ui| {
        for a in ASSUMPTIONS {
            let Some(u) = used.get(a.key) else { continue };
            let mut v = u.value;
            let r = ui.add(egui::DragValue::new(&mut v).speed(drag_speed(a.default)));
            app.cost_edit_begin(&r);
            if r.changed() {
                app.write_assumption(None, a.key, Some(v));
            }
            app.cost_edit_end(&r);
            ui.label(a.unit);
            ui.label(a.label).on_hover_text(a.key);
            if u.source == Source::Project {
                if ui.small_button(format!("default {}", a.default)).clicked() {
                    app.write_assumption(None, a.key, None);
                }
            } else {
                ui.label("");
            }
            ui.end_row();
        }
    });
}

fn currency_ui(app: &mut TtgApp, ui: &mut Ui) {
    ui.label(
        RichText::new(
            "Prices are US dollars. Show the totals in another currency too, at a rate you set and date.",
        )
        .small()
        .color(MUTED),
    );
    let mut on = app.project.settings.cost_currency.is_some();
    if ui.checkbox(&mut on, "Show another currency").changed() {
        let before = app.snapshot();
        app.project.settings.cost_currency = on.then(|| DisplayCurrency {
            code: "GBP".into(),
            rate: 0.75,
            date: String::new(),
        });
        app.finish(before);
    }
    let Some(mut c) = app.project.settings.cost_currency.clone() else {
        return;
    };
    let mut changed = false;
    let mut responses = Vec::new();
    ui.horizontal(|ui| {
        ui.label("Code");
        let r = ui.add(egui::TextEdit::singleline(&mut c.code).desired_width(48.0));
        changed |= r.changed();
        responses.push(r);
        ui.label("per USD");
        let r = ui.add(
            egui::DragValue::new(&mut c.rate)
                .speed(0.001)
                .range(0.0..=100_000.0),
        );
        changed |= r.changed();
        responses.push(r);
        ui.label("rate date");
        let r = ui.add(
            egui::TextEdit::singleline(&mut c.date)
                .desired_width(90.0)
                .hint_text("YYYY-MM-DD"),
        );
        changed |= r.changed();
        responses.push(r);
    });
    for r in &responses {
        app.cost_edit_begin(r);
    }
    if changed {
        let before = app.edit_snapshot.is_none().then(|| app.snapshot());
        app.project.settings.cost_currency = Some(c);
        app.dirty = true;
        if let Some(before) = before {
            app.finish(before);
        }
    }
    for r in &responses {
        app.cost_edit_end(r);
    }
}
