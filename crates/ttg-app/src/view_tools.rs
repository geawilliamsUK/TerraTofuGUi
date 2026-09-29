//! View tools: flows generated from the links, "tidy by flows", anchored notes
//! put back beside their anchors, the "Where personal data goes" view, and presentation
//! mode (stepping through a view's numbered flows). The geometry and the rules live in
//! `ttg_core::flow_layout`, `ttg_core::view` and `ttg_codegen::dataflow`; this is the
//! app's side of them — one undo step each — and the menu, keys and caption.

use crate::app::TtgApp;
use egui::{Color32, RichText, Ui};
use ttg_codegen::dataflow::{self, PersonalDataReport};
use ttg_core::flow_layout::{layout_view_by_flows, FlowLayoutOptions};
use ttg_core::{Flow, View};

/// Stepping through the active view's numbered flows.
#[derive(Debug, Clone)]
pub struct Presentation {
    /// The view being presented; switching away ends the presentation.
    pub view: usize,
    /// The distinct step numbers, in order.
    pub steps: Vec<u32>,
    pub index: usize,
}

impl TtgApp {
    /// This frame's visibility as an owned predicate, so a copy of the active view can
    /// be edited while it is consulted.
    fn visible_owned(&self) -> impl Fn(&str) -> bool {
        let vis = self.visible.clone();
        move |id: &str| vis.as_ref().is_none_or(|s| s.contains(id))
    }

    fn active_index(&self, what: &str) -> Result<usize, String> {
        self.active_view
            .ok_or_else(|| format!("{what} works on a view: activate one first"))
    }

    /// Draw the flows the links imply into the active view (see
    /// `ttg_codegen::dataflow::generate`). One undo step; nothing is recorded when
    /// nothing changes.
    pub fn generate_view_flows(&mut self, replace: bool) -> Result<Vec<Flow>, String> {
        let i = self.active_index("Generating flows")?;
        let mut v: View = self.project.views[i].clone();
        let added = dataflow::generate(&self.project, &self.catalog, &mut v, replace);
        if v != self.project.views[i] {
            let before = self.snapshot();
            self.project.views[i] = v;
            self.finish(before);
        }
        self.status = format!(
            "{} data flow(s) generated from the links (Ctrl+Z to undo)",
            added.len()
        );
        Ok(added)
    }

    /// Lay the active view out by its flows, in its own layout, then put its anchored
    /// notes back beside their anchors: one undo step. Returns (things moved, notes
    /// moved).
    pub fn tidy_view_by_flows(&mut self) -> Result<(usize, usize), String> {
        let i = self.active_index("Tidy by flows")?;
        self.refresh_visibility();
        let visible = self.visible_owned();
        let mut v = self.project.views[i].clone();
        let moved = layout_view_by_flows(&self.project, &mut v, &visible, &FlowLayoutOptions::default())?;
        let notes = ttg_core::view::arrange_notes(&self.project, &mut v, &visible);
        let before = self.snapshot();
        self.project.views[i] = v;
        self.finish(before);
        self.refresh_visibility();
        self.fit_requested = true;
        self.status = "Laid the view out by its flows (Ctrl+Z to undo)".into();
        Ok((moved, notes))
    }

    /// Put the active view's anchored notes back beside their anchors, without taking
    /// a snapshot: the caller commits it as part of its own step. Returns how many moved.
    pub fn arrange_view_notes_now(&mut self) -> usize {
        let Some(i) = self.active_view else { return 0 };
        let visible = self.visible_owned();
        let mut v = self.project.views[i].clone();
        let moved = ttg_core::view::arrange_notes(&self.project, &mut v, &visible);
        if moved > 0 {
            self.project.views[i] = v;
        }
        moved
    }

    /// Build (or refresh) "Where personal data goes" and switch to it. One undo step.
    pub fn build_personal_data_view(&mut self) -> Result<PersonalDataReport, String> {
        let existing = self
            .project
            .views
            .iter()
            .position(|v| v.name.eq_ignore_ascii_case(dataflow::PERSONAL_DATA_VIEW));
        let (v, rep) = dataflow::personal_data_view(
            &self.project,
            &self.catalog,
            existing.map(|i| &self.project.views[i]),
        )?;
        let before = self.snapshot();
        let i = match existing {
            Some(i) => {
                self.project.views[i] = v;
                i
            }
            None => {
                self.project.views.push(v);
                self.project.views.len() - 1
            }
        };
        self.finish(before);
        self.activate_view(Some(i));
        self.fit_requested = true;
        self.status = format!(
            "\"{}\": {} classified, {} reached, {} flow(s) added",
            dataflow::PERSONAL_DATA_VIEW,
            rep.sources.len(),
            rep.reached.len(),
            rep.added.len()
        );
        Ok(rep)
    }

    // ------------------------------------------------------------ presentation mode

    /// Step through the active view's numbered flows.
    pub fn start_presentation(&mut self) -> Result<(), String> {
        let i = self.active_index("Presenting")?;
        let mut steps: Vec<u32> = self.project.views[i]
            .flows
            .iter()
            .filter_map(|f| f.step)
            .collect();
        steps.sort();
        steps.dedup();
        if steps.is_empty() {
            return Err("this view has no numbered flows to present: give flows a step number first".into());
        }
        self.presentation = Some(Presentation {
            view: i,
            steps,
            index: 0,
        });
        self.selection.clear();
        self.selected_annotation = None;
        self.fit_requested = true;
        Ok(())
    }

    pub fn stop_presentation(&mut self) {
        if self.presentation.take().is_some() {
            self.status = "Presentation ended".into();
        }
    }

    /// The step number on show, if presenting.
    pub fn presented_step(&self) -> Option<u32> {
        let p = self.presentation.as_ref()?;
        (self.active_view == Some(p.view)).then(|| p.steps.get(p.index).copied())?
    }

    /// `None` when not presenting; otherwise whether this flow is part of the step.
    pub fn presents_flow(&self, f: &Flow) -> Option<bool> {
        self.presented_step().map(|s| f.step == Some(s))
    }

    /// `None` when not presenting; otherwise whether this resource, logical node or
    /// group is an end of one of the step's flows.
    pub fn presents_end(&self, id: &str) -> Option<bool> {
        let s = self.presented_step()?;
        let v = self.active_view()?;
        Some(
            v.flows
                .iter()
                .filter(|f| f.step == Some(s))
                .any(|f| f.from.id() == id || f.to.id() == id),
        )
    }

    /// Next / previous / leave, from the keyboard: arrows, Space, Page Up / Down, Esc.
    /// Runs before the ordinary shortcuts so Esc leaves the presentation first.
    pub fn presentation_keys(&mut self, ctx: &egui::Context) {
        let Some(p) = &self.presentation else { return };
        if self.active_view != Some(p.view) {
            self.presentation = None;
            return;
        }
        if ctx.wants_keyboard_input() {
            return;
        }
        use egui::{Key, Modifiers};
        let (next, prev, leave) = ctx.input_mut(|i| {
            // Every key in the set is consumed, not just the first one pressed.
            let mut hit = |keys: &[Key]| {
                let mut any = false;
                for k in keys {
                    any |= i.consume_key(Modifiers::NONE, *k);
                }
                any
            };
            (
                hit(&[Key::ArrowRight, Key::ArrowDown, Key::Space, Key::PageDown]),
                hit(&[Key::ArrowLeft, Key::ArrowUp, Key::PageUp]),
                hit(&[Key::Escape]),
            )
        });
        if leave {
            self.stop_presentation();
        } else if next {
            self.present_move(1);
        } else if prev {
            self.present_move(-1);
        }
    }

    fn present_move(&mut self, by: i32) {
        if let Some(p) = &mut self.presentation {
            let last = p.steps.len().saturating_sub(1) as i32;
            p.index = (p.index as i32 + by).clamp(0, last) as usize;
        }
    }
}

/// The view tools in the View menu (enabled while a saved view is active).
pub fn view_menu(app: &mut TtgApp, ui: &mut Ui) {
    ui.separator();
    ui.label(
        RichText::new("Active view")
            .small()
            .color(Color32::from_gray(110)),
    );
    let on_view = app.active_view.is_some();
    if ui
        .add_enabled(on_view, egui::Button::new("Generate data flows from links"))
        .on_hover_text("Draw a flow for every data-carrying link between the resources this view shows (sends to, reads, uses, logs, mounts…), in the direction the data moves. Pairs that already have a flow are left alone.")
        .clicked()
    {
        if let Err(e) = app.generate_view_flows(false) {
            app.status = e;
        }
        ui.close();
    }
    if ui
        .add_enabled(on_view, egui::Button::new("Tidy by flows"))
        .on_hover_text("Lay this view out left to right by its flows and step numbers, in the view's own layout (other views never move). Grouping boxes are refitted and notes put back beside what they explain.")
        .clicked()
    {
        if let Err(e) = app.tidy_view_by_flows() {
            app.status = e;
        }
        ui.close();
    }
    if ui
        .add_enabled(on_view, egui::Button::new("Arrange notes"))
        .on_hover_text("Put every pinned note back beside what it explains")
        .clicked()
    {
        let before = app.snapshot();
        let n = app.arrange_view_notes_now();
        app.finish(before);
        app.status = format!("{n} note(s) moved");
        ui.close();
    }
    let numbered = app
        .active_view()
        .is_some_and(|v| v.flows.iter().any(|f| f.step.is_some()));
    if ui
        .add_enabled(numbered, egui::Button::new("Present steps   ▶"))
        .on_hover_text("Step through the numbered flows: ← → (or Space) to move, Esc to leave")
        .clicked()
    {
        if let Err(e) = app.start_presentation() {
            app.status = e;
        }
        ui.close();
    }
    if ui
        .button(format!("Build \"{}\"", dataflow::PERSONAL_DATA_VIEW))
        .on_hover_text("A view of the resources classified personal or payment and where their data goes one step on (refreshed if it exists)")
        .clicked()
    {
        if let Err(e) = app.build_personal_data_view() {
            app.status = e;
        }
        ui.close();
    }
}

/// The caption at the bottom of the canvas while presenting: the step, its flows, and
/// the notes pinned to them, with previous / next / leave buttons.
pub fn presentation_caption(app: &mut TtgApp, ui: &mut Ui) {
    let Some(step) = app.presented_step() else { return };
    let Some(v) = app.active_view() else { return };
    let (index, total) = app
        .presentation
        .as_ref()
        .map(|p| (p.index, p.steps.len()))
        .unwrap_or((0, 0));
    let flows: Vec<&Flow> = v.flows.iter().filter(|f| f.step == Some(step)).collect();
    let lines: Vec<String> = flows
        .iter()
        .map(|f| {
            let mut s = format!(
                "{} to {}",
                ttg_core::view::end_name(&app.project, v, &f.from),
                ttg_core::view::end_name(&app.project, v, &f.to)
            );
            if !f.label.is_empty() {
                s.push_str(&format!(": {}", f.label));
            }
            if let Some(d) = &f.data {
                s.push_str(&format!(" ({d})"));
            }
            s
        })
        .collect();
    let notes: Vec<(String, String)> = v
        .notes
        .iter()
        .filter(|n| {
            n.anchor
                .as_ref()
                .is_some_and(|a| flows.iter().any(|f| f.id == a.id()))
        })
        .map(|n| (n.title.clone(), n.body.clone()))
        .collect();
    let width = (app.canvas_rect.width() - 80.0).clamp(240.0, 640.0);
    let mut go: Option<i32> = None;
    let mut leave = false;
    egui::Area::new(ui.id().with("presentation-caption"))
        .order(egui::Order::Foreground)
        .pivot(egui::Align2::CENTER_BOTTOM)
        .fade_in(false)
        .fixed_pos(app.canvas_rect.center_bottom() - egui::vec2(0.0, 36.0))
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width(width);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("Step {step}")).strong().size(16.0));
                    ui.label(
                        RichText::new(format!("{} of {total}", index + 1))
                            .small()
                            .color(Color32::from_gray(120)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button("Leave")
                            .on_hover_text("Leave the presentation (Esc)")
                            .clicked()
                        {
                            leave = true;
                        }
                        if ui
                            .add_enabled(index + 1 < total, egui::Button::new("Next ▶"))
                            .clicked()
                        {
                            go = Some(1);
                        }
                        if ui
                            .add_enabled(index > 0, egui::Button::new("◀ Previous"))
                            .clicked()
                        {
                            go = Some(-1);
                        }
                    });
                });
                for l in &lines {
                    ui.label(RichText::new(l).size(14.0));
                }
                for (title, body) in &notes {
                    ui.add_space(4.0);
                    if !title.is_empty() {
                        ui.label(
                            RichText::new(title)
                                .strong()
                                .color(Color32::from_rgb(150, 130, 60)),
                        );
                    }
                    if !body.is_empty() {
                        ui.label(RichText::new(body).small().color(Color32::from_gray(80)));
                    }
                }
            });
        });
    if leave {
        app.stop_presentation();
    } else if let Some(by) = go {
        app.present_move(by);
    }
}
