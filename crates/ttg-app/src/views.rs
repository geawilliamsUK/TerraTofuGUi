//! Canvas views: a working filter (categories, link kinds, focus, hidden, "only"), the
//! tab bar of saved views above the canvas, and the visibility computation the canvas,
//! marquee and reachability overlay all consult.

use crate::app::TtgApp;
use egui::{Color32, RichText, Ui};
use std::collections::{BTreeSet, VecDeque};
use ttg_catalog::Catalog;
use ttg_core::{Id, Project, Relation, View, ViewFilter};

/// Entities shown under `filter`, or `None` when everything is visible.
pub fn visible_set(p: &Project, cat: &Catalog, filter: &ViewFilter) -> Option<BTreeSet<Id>> {
    if filter.is_empty() {
        return None;
    }
    let all: Vec<Id> = p.entities().iter().map(|e| e.id.to_string()).collect();
    let mut vis: BTreeSet<Id> = if !filter.only.is_empty() {
        all.iter()
            .filter(|id| filter.only.contains(*id))
            .cloned()
            .collect()
    } else if let Some(focus) = &filter.focus {
        // Breadth-first over links (both directions) up to `depth` hops.
        let mut seen: BTreeSet<Id> = BTreeSet::new();
        let mut q: VecDeque<(Id, u32)> = VecDeque::new();
        if p.entity(focus).is_some() {
            seen.insert(focus.clone());
            q.push_back((focus.clone(), 0));
        }
        while let Some((id, d)) = q.pop_front() {
            if d >= filter.depth {
                continue;
            }
            for e in &p.edges {
                let other = if e.source == id {
                    &e.target
                } else if e.target == id {
                    &e.source
                } else {
                    continue;
                };
                if !filter.relations.is_empty() && !filter.relations.contains(&e.relation) {
                    continue;
                }
                if seen.insert(other.clone()) {
                    q.push_back((other.clone(), d + 1));
                }
            }
        }
        seen
    } else {
        all.iter().cloned().collect()
    };
    if !filter.categories.is_empty() {
        vis.retain(|id| {
            p.entity(id)
                .and_then(|e| cat.resource(e.resource_type))
                .is_none_or(|d| filter.categories.contains(&d.resource.category))
        });
    }
    for h in &filter.hidden {
        vis.remove(h);
    }
    if !filter.containers {
        vis.retain(|id| !p.containers.contains_key(id));
        return Some(vis);
    }
    // Containers of visible entities stay visible so the picture keeps its structure.
    let mut ancestors: BTreeSet<Id> = BTreeSet::new();
    for id in &vis {
        let mut cur = p.parent_of(id).map(|s| s.to_string());
        while let Some(c) = cur {
            if !ancestors.insert(c.clone()) {
                break;
            }
            cur = p.parent_of(&c).map(|s| s.to_string());
        }
    }
    vis.extend(ancestors);
    Some(vis)
}

impl TtgApp {
    /// Recompute the visible set for this frame and drop hidden entities from the
    /// selection so keyboard actions never hit something the user cannot see.
    pub fn refresh_visibility(&mut self) {
        self.visible = visible_set(&self.project, &self.catalog, &self.filter);
        self.layer = if self.concrete_mode() {
            Some(ttg_codegen::layers::members(
                &self.project,
                &self.catalog,
                &self.project.settings.target_provider,
            ))
        } else {
            None
        };
        if let Some(v) = &self.visible {
            self.selection.retain(|id| v.contains(id));
            if let Some(i) = self.selected_edge {
                if !self.project.edges.get(i).is_some_and(|e| self.edge_visible(e)) {
                    self.selected_edge = None;
                }
            }
        }
    }

    pub fn is_visible(&self, id: &str) -> bool {
        self.visible.as_ref().is_none_or(|v| v.contains(id))
    }

    pub fn edge_visible(&self, e: &ttg_core::Edge) -> bool {
        self.is_visible(&e.source)
            && self.is_visible(&e.target)
            && (self.filter.relations.is_empty() || self.filter.relations.contains(&e.relation))
    }

    /// Number of entities the current filter hides.
    pub fn hidden_count(&self) -> usize {
        match &self.visible {
            None => 0,
            Some(v) => self.project.entities().len().saturating_sub(v.len()),
        }
    }

    /// Replace the working filter. When a saved view is active the change is written
    /// into it straight away (undoable), so a view always shows what you last set.
    pub fn set_filter(&mut self, f: ViewFilter) {
        self.filter = f;
        if let Some(i) = self.active_view {
            if self.project.views.get(i).is_some_and(|v| v.filter != self.filter) {
                let before = self.snapshot();
                self.project.views[i].filter = self.filter.clone();
                self.finish(before);
            }
        }
        self.refresh_visibility();
    }

    /// Show only these entities (and their containers).
    pub fn show_only(&mut self, ids: impl IntoIterator<Item = Id>, what: &str) {
        let only: BTreeSet<Id> = ids.into_iter().collect();
        let n = only.len();
        self.set_filter(ViewFilter {
            only,
            ..ViewFilter::default()
        });
        self.status = format!("Showing {what} ({n} resources); use the view bar to clear");
    }

    pub fn activate_view(&mut self, i: Option<usize>) {
        self.active_view = i;
        let f = i
            .and_then(|i| self.project.views.get(i))
            .map(|v| v.filter.clone())
            .unwrap_or_default();
        self.set_filter(f);
    }

    /// If a newly added entity would be hidden, drop the filter so the user sees it.
    pub fn reveal_new(&mut self, id: &str) {
        self.refresh_visibility();
        if !self.is_visible(id) {
            self.active_view = None;
            self.set_filter(ViewFilter::default());
            self.status = "View filter cleared to show the new resource".into();
        }
    }
}

/// The strip between the menu bar and the canvas: view tabs, the filter menu, and a
/// summary of what is hidden.
pub fn bar(app: &mut TtgApp, ui: &mut Ui) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Views").small().color(Color32::from_gray(110)))
            .on_hover_text("A view remembers WHICH resources and links are shown (the filter), not where they are: positions are shared by every view. The active view saves filter changes as you make them.");
        if ui
            .selectable_label(app.active_view.is_none(), "All")
            .on_hover_text("Show everything")
            .clicked()
        {
            app.activate_view(None);
        }
        let n = app.project.views.len();
        let mut delete: Option<usize> = None;
        for i in 0..n {
            let active = app.active_view == Some(i);
            let name = app.project.views[i].name.clone();
            let n_hidden = crate::views::visible_set(&app.project, &app.catalog, &app.project.views[i].filter)
                .map(|v| app.project.entities().len().saturating_sub(v.len()))
                .unwrap_or(0);
            let resp = ui
                .selectable_label(active, name.clone())
                .on_hover_text(format!(
                    "{}. Click to apply; right-click to rename or delete. Filter changes save into the active view automatically.",
                    if n_hidden == 0 { "Shows everything (no filter yet)".to_string() } else { format!("Hides {n_hidden} resources") }
                ));
            if resp.clicked() {
                app.activate_view(Some(i));
            }
            resp.context_menu(|ui| {
                if ui.button("Rename…").clicked() {
                    app.view_edit = Some((Some(i), name.clone()));
                    ui.close();
                }
                let mut own = app.project.views[i].layout.is_some();
                if ui
                    .checkbox(&mut own, "Own layout")
                    .on_hover_text("On: this view positions resources itself (moves here do not affect other views). Off: it shares the All layout.")
                    .changed()
                {
                    let before = app.snapshot();
                    app.project.views[i].layout = if own { Some(ttg_core::ViewLayout::default()) } else { None };
                    app.finish(before);
                }
                if ui.button("Delete").clicked() {
                    delete = Some(i);
                    ui.close();
                }
            });
        }
        if let Some(i) = delete {
            let before = app.snapshot();
            app.project.views.remove(i);
            app.finish(before);
            if app.active_view == Some(i) {
                app.activate_view(None);
            } else if let Some(a) = app.active_view {
                if a > i {
                    app.active_view = Some(a - 1);
                }
            }
        }
        if ui
            .small_button("+")
            .on_hover_text("Save the current filter as a named view (stored in the project file). Then use Filter to change what the view shows; it saves as you go.")
            .clicked()
        {
            app.view_edit = Some((None, format!("View {}", n + 1)));
        }

        ui.separator();
        filter_menu(app, ui);
        if app.active_view.is_some() {
            if ui
                .small_button("+ Group")
                .on_hover_text("Add a grouping box to this view (annotation only, never exported). Drag its title to move it with everything inside; right-click a resource or group for a data-flow arrow.")
                .clicked()
            {
                let c = app.camera.to_world(app.canvas_rect.min, app.canvas_rect.center());
                let n = app.active_view().map(|v| v.groups.len()).unwrap_or(0);
                app.add_group(
                    &format!("Group {}", n + 1),
                    ttg_core::Position {
                        x: c.x as i32 - 200,
                        y: c.y as i32 - 120,
                    },
                    ttg_core::Size { w: 400, h: 240 },
                    None,
                );
            }
            let own = app.active_layout().is_some();
            ui.label(
                RichText::new(if own { "own layout" } else { "shared layout" })
                    .small()
                    .color(Color32::from_gray(110)),
            )
            .on_hover_text("Right-click the view tab to switch between its own layout and the shared one.");
        }
        if !app.filter.is_empty() {
            if ui
                .small_button("Clear")
                .on_hover_text("Show everything")
                .clicked()
            {
                app.set_filter(ViewFilter::default());
            }
            let hidden = app.hidden_count();
            let shown = app.project.entities().len() - hidden;
            ui.label(
                RichText::new(format!("{shown} shown, {hidden} hidden"))
                    .small()
                    .color(Color32::from_gray(110)),
            );
            if let Some(f) = &app.filter.focus {
                let name = app
                    .project
                    .entity(f)
                    .map(|e| e.name.to_string())
                    .unwrap_or_default();
                ui.label(
                    RichText::new(format!("focus: {name} ±{}", app.filter.depth))
                        .small()
                        .color(Color32::from_gray(110)),
                );
            }
        }
    });

    // Name prompt for new / renamed views.
    if let Some((idx, mut name)) = app.view_edit.clone() {
        let mut done: Option<bool> = None;
        egui::Window::new(if idx.is_some() { "Rename view" } else { "Save view" })
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                let r = ui.text_edit_singleline(&mut name);
                r.request_focus();
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    done = Some(true);
                }
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        done = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        done = Some(false);
                    }
                });
            });
        app.view_edit = Some((idx, name.clone()));
        match done {
            Some(true) if !name.trim().is_empty() => {
                app.view_edit = None;
                let before = app.snapshot();
                match idx {
                    Some(i) => app.project.views[i].name = name.trim().to_string(),
                    None => {
                        app.project.views.push(View::new(name.trim(), app.filter.clone()));
                        app.active_view = Some(app.project.views.len() - 1);
                        if app.filter.is_empty() {
                            app.status = format!(
                                "View \"{}\" saved; it shows everything until you narrow it with Filter (changes save into the view as you go)",
                                name.trim()
                            );
                        }
                    }
                }
                app.finish(before);
            }
            Some(_) => app.view_edit = None,
            None => {}
        }
    }
}

fn filter_menu(app: &mut TtgApp, ui: &mut Ui) {
    use egui::containers::menu::{MenuButton, MenuConfig};
    let active = !app.filter.is_empty();
    let title = if active { "Filter ▾ (on)" } else { "Filter ▾" };
    // Stay open while boxes are ticked; close on a click outside (or Escape).
    let config = MenuConfig::new().close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside);
    MenuButton::new(title).config(config).ui(ui, |ui| {
        ui.set_min_width(240.0);
        let sel: Vec<Id> = app.selection.iter().cloned().collect();
        ui.label(RichText::new("Selection").small().color(Color32::from_gray(110)));
        if ui
            .add_enabled(sel.len() == 1, egui::Button::new("Focus on selection"))
            .on_hover_text("Show only what is linked to the selected resource, within N hops")
            .clicked()
        {
            let mut f = app.filter.clone();
            f.focus = sel.first().cloned();
            f.only.clear();
            app.set_filter(f);
        }
        if app.filter.focus.is_some() {
            ui.horizontal(|ui| {
                ui.label("Hops");
                let mut d = app.filter.depth;
                if ui.add(egui::Slider::new(&mut d, 1..=5)).changed() {
                    let mut f = app.filter.clone();
                    f.depth = d;
                    app.set_filter(f);
                }
                if ui.small_button("clear focus").clicked() {
                    let mut f = app.filter.clone();
                    f.focus = None;
                    app.set_filter(f);
                }
            });
        }
        if ui
            .add_enabled(!sel.is_empty(), egui::Button::new("Show only selection"))
            .clicked()
        {
            app.show_only(sel.clone(), "the selection");
        }
        if ui
            .add_enabled(!sel.is_empty(), egui::Button::new("Hide selection"))
            .clicked()
        {
            let mut f = app.filter.clone();
            f.hidden.extend(sel.iter().cloned());
            app.set_filter(f);
        }
        if !app.filter.hidden.is_empty()
            && ui
                .button(format!("Unhide all ({})", app.filter.hidden.len()))
                .clicked()
        {
            let mut f = app.filter.clone();
            f.hidden.clear();
            app.set_filter(f);
        }
        if !app.filter.only.is_empty() && ui.button("Stop showing only a path/selection").clicked() {
            let mut f = app.filter.clone();
            f.only.clear();
            app.set_filter(f);
        }

        ui.separator();
        let mut containers = app.filter.containers;
        if ui
            .checkbox(&mut containers, "Show containers")
            .on_hover_text("Off: networks, resource groups and vaults are not drawn (useful for architecture maps that use their own groups). Their contents are still shown and exported.")
            .changed()
        {
            let mut f = app.filter.clone();
            f.containers = containers;
            app.set_filter(f);
        }
        ui.separator();
        ui.label(RichText::new("Categories").small().color(Color32::from_gray(110)));
        let cats = app.catalog.categories();
        for c in &cats {
            let all = app.filter.categories.is_empty();
            let mut on = all || app.filter.categories.contains(c);
            if ui
                .checkbox(&mut on, ttg_catalog::load::category_label(c))
                .changed()
            {
                let mut f = app.filter.clone();
                if all {
                    f.categories = cats.iter().cloned().collect();
                }
                if on {
                    f.categories.insert(c.clone());
                } else {
                    f.categories.remove(c);
                }
                if f.categories.len() == cats.len() {
                    f.categories.clear();
                }
                app.set_filter(f);
            }
        }

        ui.separator();
        ui.label(RichText::new("Link kinds").small().color(Color32::from_gray(110)));
        for r in Relation::ALL {
            let all = app.filter.relations.is_empty();
            let mut on = all || app.filter.relations.contains(&r);
            if ui.checkbox(&mut on, r.display_name()).changed() {
                let mut f = app.filter.clone();
                if all {
                    f.relations = Relation::ALL.into_iter().collect();
                }
                if on {
                    f.relations.insert(r);
                } else {
                    f.relations.remove(&r);
                }
                if f.relations.len() == Relation::ALL.len() {
                    f.relations.clear();
                }
                app.set_filter(f);
            }
        }
    });
}
