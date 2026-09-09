//! View-local layout and annotations: per-view positions, grouping boxes and data-flow
//! arrows. None of this reaches codegen; it is the "architecture map" side of a view.

use crate::app::{Drag, TtgApp};
use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};
use ttg_core::{Flow, FlowEnd, Group, Id, Position, Project, Size, View, ViewLayout};

/// What annotation is selected on the canvas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Annotation {
    Group(Id),
    Flow(Id),
}

const GROUP_HEADER: f32 = 26.0;
const PALETTE: &[&str] = &[
    "#5b7fb5", "#5fa870", "#d08a3a", "#b05aa0", "#c95555", "#3aa6a0", "#8a7f5a",
];

pub fn parse_color(s: &str) -> Option<Color32> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    Some(Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

pub fn group_color(g: &Group, index: usize) -> Color32 {
    g.color
        .as_deref()
        .and_then(parse_color)
        .unwrap_or_else(|| parse_color(PALETTE[index % PALETTE.len()]).unwrap())
}

impl TtgApp {
    pub fn active_view(&self) -> Option<&View> {
        self.active_view.and_then(|i| self.project.views.get(i))
    }
    pub fn active_view_mut(&mut self) -> Option<&mut View> {
        self.active_view.and_then(|i| self.project.views.get_mut(i))
    }
    pub fn active_layout(&self) -> Option<&ViewLayout> {
        self.active_view().and_then(|v| v.layout.as_ref())
    }

    /// Run a geometry edit against the project as the active view sees it. With a
    /// view-owned layout the edit lands in the view (positions and sizes), leaving the
    /// shared layout untouched; otherwise it edits the project directly.
    pub fn with_layout(&mut self, f: impl FnOnce(&mut Project)) {
        let Some(i) = self.active_view else {
            f(&mut self.project);
            return;
        };
        if self.project.views[i].layout.is_none() {
            f(&mut self.project);
            return;
        }
        let mut tmp = self.project.clone();
        {
            let l = self.project.views[i].layout.as_ref().unwrap();
            for (id, p) in &l.positions {
                if let Some(n) = tmp.nodes.get_mut(id) {
                    n.position = *p;
                } else if let Some(c) = tmp.containers.get_mut(id) {
                    c.position = *p;
                }
            }
            for (id, s) in &l.sizes {
                if let Some(n) = tmp.nodes.get_mut(id) {
                    n.size = Some(*s);
                } else if let Some(c) = tmp.containers.get_mut(id) {
                    c.size = *s;
                }
            }
        }
        f(&mut tmp);
        let l = self.project.views[i].layout.as_mut().unwrap();
        for n in tmp.nodes.values() {
            let base = &self.project.nodes[&n.id];
            if n.position != base.position || l.positions.contains_key(&n.id) {
                l.positions.insert(n.id.clone(), n.position);
            }
            match n.size {
                Some(s) if Some(s) != base.size || l.sizes.contains_key(&n.id) => {
                    l.sizes.insert(n.id.clone(), s);
                }
                _ => {}
            }
        }
        for c in tmp.containers.values() {
            let base = &self.project.containers[&c.id];
            if c.position != base.position || l.positions.contains_key(&c.id) {
                l.positions.insert(c.id.clone(), c.position);
            }
            if c.size != base.size || l.sizes.contains_key(&c.id) {
                l.sizes.insert(c.id.clone(), c.size);
            }
        }
    }

    /// Move entities (and their descendants) by a delta in the active layout.
    pub fn shift_entities(&mut self, ids: &[Id], dx: i32, dy: i32) {
        let ids: Vec<Id> = ids.to_vec();
        self.with_layout(|p| {
            for id in &ids {
                if let Some(n) = p.nodes.get_mut(id) {
                    n.position.x += dx;
                    n.position.y += dy;
                } else if let Some(c) = p.containers.get_mut(id) {
                    c.position.x += dx;
                    c.position.y += dy;
                }
            }
        });
    }

    pub fn group_rect(&self, gid: &str) -> Option<Rect> {
        let g = self.active_view()?.group(gid)?;
        Some(Rect::from_min_size(
            Pos2::new(g.position.x as f32, g.position.y as f32),
            Vec2::new(g.size.w as f32, g.size.h as f32),
        ))
    }

    /// World rect of a flow endpoint.
    pub fn end_rect(&self, end: &FlowEnd) -> Option<Rect> {
        match end {
            FlowEnd::Entity { entity } => self.entity_rect(entity),
            FlowEnd::Group { group } => self.group_rect(group),
        }
    }

    /// Visible entities whose centre lies inside the group's box.
    pub fn group_members(&self, gid: &str) -> Vec<Id> {
        let Some(r) = self.group_rect(gid) else {
            return Vec::new();
        };
        self.project
            .entities()
            .iter()
            .filter(|e| self.is_visible(e.id))
            .filter(|e| self.entity_rect(e.id).is_some_and(|er| r.contains(er.center())))
            .map(|e| e.id.to_string())
            .collect()
    }

    pub fn add_group(&mut self, label: &str, pos: Position, size: Size, color: Option<String>) -> Option<Id> {
        self.active_view?;
        let id = self.project.fresh_id("grp");
        let before = self.snapshot();
        self.active_view_mut().unwrap().groups.push(Group {
            id: id.clone(),
            label: label.to_string(),
            position: pos,
            size,
            color,
        });
        self.finish(before);
        self.selected_annotation = Some(Annotation::Group(id.clone()));
        self.selection.clear();
        self.selected_edge = None;
        Some(id)
    }

    pub fn add_flow(&mut self, from: FlowEnd, to: FlowEnd, label: &str, dashed: bool) -> Option<Id> {
        self.active_view?;
        if from == to {
            return None;
        }
        let id = self.project.fresh_id("flow");
        let before = self.snapshot();
        self.active_view_mut().unwrap().flows.push(Flow {
            id: id.clone(),
            from,
            to,
            label: label.to_string(),
            dashed,
        });
        self.finish(before);
        self.selected_annotation = Some(Annotation::Flow(id.clone()));
        self.selection.clear();
        self.selected_edge = None;
        Some(id)
    }

    pub fn remove_annotation(&mut self, a: &Annotation) -> bool {
        let Some(v) = self.active_view_mut() else {
            return false;
        };
        let before_len = v.groups.len() + v.flows.len();
        match a {
            Annotation::Group(id) => {
                v.groups.retain(|g| &g.id != id);
                v.flows.retain(|f| f.from.id() != id && f.to.id() != id);
            }
            Annotation::Flow(id) => v.flows.retain(|f| &f.id != id),
        }
        let changed = v.groups.len() + v.flows.len() != before_len;
        if self.selected_annotation.as_ref() == Some(a) {
            self.selected_annotation = None;
        }
        changed
    }

    /// Resolve a flow endpoint from an id, an entity name or a group label.
    pub fn resolve_end(&self, key: &str) -> Option<FlowEnd> {
        if self.project.entity(key).is_some() {
            return Some(FlowEnd::Entity {
                entity: key.to_string(),
            });
        }
        if let Some(v) = self.active_view() {
            if let Some(g) = v
                .groups
                .iter()
                .find(|g| g.id == key || g.label.eq_ignore_ascii_case(key))
            {
                return Some(FlowEnd::Group { group: g.id.clone() });
            }
        }
        let mut hits = self
            .project
            .entities()
            .iter()
            .filter(|e| e.name.eq_ignore_ascii_case(key))
            .map(|e| e.id.to_string())
            .collect::<Vec<_>>();
        if hits.len() == 1 {
            return Some(FlowEnd::Entity {
                entity: hits.remove(0),
            });
        }
        None
    }

    /// Complete a pending "data flow from …" with a clicked target.
    pub fn finish_flow_to(&mut self, to: FlowEnd) {
        if let Some(from) = self.flow_from.take() {
            if self.add_flow(from, to, "", false).is_some() {
                self.status = "Data flow added; give it a label in the inspector".into();
            } else {
                self.status = "A flow needs two different ends".into();
            }
        }
    }
}

/// Draw the active view's groups (behind everything) and handle their interaction.
pub fn draw_groups(app: &mut TtgApp, ui: &mut Ui, origin: Pos2) {
    let Some(view) = app.active_view() else { return };
    let groups: Vec<Group> = view.groups.clone();
    let zoom = app.camera.zoom;
    let painter = ui.painter_at(app.canvas_rect);
    for (i, g) in groups.iter().enumerate() {
        let Some(wr) = app.group_rect(&g.id) else { continue };
        let sr = app.camera.rect_to_screen(origin, wr);
        let color = group_color(g, i);
        let selected = app.selected_annotation == Some(Annotation::Group(g.id.clone()));
        painter.rect(
            sr,
            CornerRadius::same(10),
            Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 28),
            Stroke::new(
                if selected { 2.5 } else { 1.5 },
                if selected {
                    Color32::from_rgb(30, 100, 220)
                } else {
                    color
                },
            ),
            StrokeKind::Inside,
        );
        let header = Rect::from_min_size(sr.min, Vec2::new(sr.width(), GROUP_HEADER * zoom));
        painter.text(
            header.left_center() + Vec2::new(12.0 * zoom, 0.0),
            Align2::LEFT_CENTER,
            &g.label,
            FontId::proportional((13.0 * zoom).max(7.0)),
            color,
        );
        // Header: click to select, drag to move with the entities inside.
        let resp = ui.interact(header, ui.id().with(("grp", &g.id)), Sense::click_and_drag());
        if resp.clicked() {
            if app.flow_from.is_some() {
                app.finish_flow_to(FlowEnd::Group { group: g.id.clone() });
            } else {
                app.selected_annotation = Some(Annotation::Group(g.id.clone()));
                app.selection.clear();
                app.selected_edge = None;
            }
        }
        if resp.drag_started_by(egui::PointerButton::Primary) {
            app.selected_annotation = Some(Annotation::Group(g.id.clone()));
            app.selection.clear();
            app.drag = Drag::Move {
                before: app.snapshot(),
                accum: Vec2::ZERO,
            };
        }
        if resp.dragged_by(egui::PointerButton::Primary) {
            let (dx, dy) = {
                let Drag::Move { accum, .. } = &mut app.drag else {
                    continue;
                };
                *accum += resp.drag_delta() / zoom;
                let dx = accum.x.trunc();
                let dy = accum.y.trunc();
                accum.x -= dx;
                accum.y -= dy;
                (dx as i32, dy as i32)
            };
            if dx != 0 || dy != 0 {
                let members = app.group_members(&g.id);
                if let Some(gm) = app.active_view_mut().and_then(|v| v.group_mut(&g.id)) {
                    gm.position.x += dx;
                    gm.position.y += dy;
                }
                app.shift_entities(&members, dx, dy);
            }
        }
        if resp.drag_stopped_by(egui::PointerButton::Primary) {
            if let Drag::Move { before, .. } = std::mem::replace(&mut app.drag, Drag::None) {
                app.finish(before);
            }
        }
        resp.context_menu(|ui| {
            if ui.button("Data flow from here").clicked() {
                app.flow_from = Some(FlowEnd::Group { group: g.id.clone() });
                app.status = "Click the target of the data flow (Esc to cancel)".into();
                ui.close();
            }
            if ui.button("Delete group").clicked() {
                let before = app.snapshot();
                app.remove_annotation(&Annotation::Group(g.id.clone()));
                app.finish(before);
                ui.close();
            }
        });
        // Resize handle.
        let handle = Rect::from_min_size(sr.max - Vec2::splat(14.0 * zoom), Vec2::splat(14.0 * zoom));
        let hr = ui.interact(handle, ui.id().with(("grpresize", &g.id)), Sense::drag());
        painter.line_segment(
            [
                handle.left_bottom() + Vec2::new(3.0, -3.0),
                handle.right_top() + Vec2::new(-3.0, 3.0),
            ],
            Stroke::new(1.5, color),
        );
        if hr.drag_started() {
            app.drag = Drag::Resize {
                before: app.snapshot(),
                accum: Vec2::ZERO,
            };
        }
        if hr.dragged() {
            if let Drag::Resize { accum, .. } = &mut app.drag {
                *accum += hr.drag_delta() / zoom;
                let dx = accum.x.trunc();
                let dy = accum.y.trunc();
                accum.x -= dx;
                accum.y -= dy;
                if let Some(gm) = app.active_view_mut().and_then(|v| v.group_mut(&g.id)) {
                    gm.size.w = (gm.size.w + dx as i32).max(160);
                    gm.size.h = (gm.size.h + dy as i32).max(80);
                }
            }
        }
        if hr.drag_stopped() {
            if let Drag::Resize { before, .. } = std::mem::replace(&mut app.drag, Drag::None) {
                app.finish(before);
            }
        }
        hr.on_hover_cursor(egui::CursorIcon::ResizeNwSe);
    }
}

fn bezier_points(a: Pos2, c1: Pos2, c2: Pos2, b: Pos2, n: usize) -> Vec<Pos2> {
    (0..=n)
        .map(|i| {
            let t = i as f32 / n as f32;
            let u = 1.0 - t;
            let p = a.to_vec2() * (u * u * u)
                + c1.to_vec2() * (3.0 * u * u * t)
                + c2.to_vec2() * (3.0 * u * t * t)
                + b.to_vec2() * (t * t * t);
            p.to_pos2()
        })
        .collect()
}

/// Draw the active view's data-flow arrows (above nodes) and handle selection.
pub fn draw_flows(app: &mut TtgApp, ui: &mut Ui, origin: Pos2) {
    let Some(view) = app.active_view() else { return };
    let flows: Vec<Flow> = view.flows.clone();
    let zoom = app.camera.zoom;
    let painter = ui.painter_at(app.canvas_rect);
    let ink = Color32::from_rgb(45, 55, 75);
    for f in &flows {
        let (Some(ar), Some(br)) = (app.end_rect(&f.from), app.end_rect(&f.to)) else {
            continue;
        };
        let ar = app.camera.rect_to_screen(origin, ar);
        let br = app.camera.rect_to_screen(origin, br);
        // Attach on the facing sides, centred.
        let (a, b, na, nb) = facing_points(ar, br);
        let selected = app.selected_annotation == Some(Annotation::Flow(f.id.clone()));
        let color = if selected {
            Color32::from_rgb(30, 100, 220)
        } else {
            ink
        };
        let w = (3.0 * zoom).clamp(2.0, 4.5);
        let dist = (b - a).length();
        let k = (dist * 0.4).max(50.0 * zoom.max(0.5));
        let c1 = a + na * k;
        let c2 = b + nb * k;
        let pts = bezier_points(a, c1, c2, b, 24);
        let stroke = Stroke::new(w, color);
        if f.dashed {
            for seg in pts.windows(2) {
                painter.add(egui::Shape::dashed_line(&[seg[0], seg[1]], stroke, 9.0, 6.0));
            }
        } else {
            painter.add(egui::Shape::line(pts.clone(), stroke));
        }
        let dir = (b - c2).normalized();
        let n = Vec2::new(-dir.y, dir.x);
        let l = (14.0 * zoom).clamp(9.0, 18.0);
        painter.add(egui::Shape::convex_polygon(
            vec![b, b - dir * l + n * l * 0.5, b - dir * l - n * l * 0.5],
            color,
            Stroke::NONE,
        ));
        let mid = pts[12];
        let label = if f.label.is_empty() {
            "flow".to_string()
        } else {
            f.label.clone()
        };
        let font = FontId::proportional((12.0 * zoom).max(7.0));
        let galley = painter.layout_no_wrap(label, font, Color32::WHITE);
        let lr = Rect::from_center_size(mid, galley.size() + Vec2::new(12.0, 6.0));
        let resp = ui.interact(lr, ui.id().with(("flow", &f.id)), Sense::click());
        painter.rect_filled(lr, CornerRadius::same(6), color);
        painter.galley(lr.min + Vec2::new(6.0, 3.0), galley, Color32::WHITE);
        if resp.clicked() {
            app.selected_annotation = Some(Annotation::Flow(f.id.clone()));
            app.selection.clear();
            app.selected_edge = None;
        }
        resp.context_menu(|ui| {
            if ui.button("Delete flow").clicked() {
                let before = app.snapshot();
                app.remove_annotation(&Annotation::Flow(f.id.clone()));
                app.finish(before);
                ui.close();
            }
        });
        resp.on_hover_text("Data flow (annotation, not exported). Click to select, Delete to remove.");
    }
}

/// Points on the facing sides of two rects plus their outward normals.
fn facing_points(ar: Rect, br: Rect) -> (Pos2, Pos2, Vec2, Vec2) {
    let d = br.center() - ar.center();
    if d.x.abs() >= d.y.abs() {
        if d.x >= 0.0 {
            (
                Pos2::new(ar.right(), ar.center().y),
                Pos2::new(br.left(), br.center().y),
                Vec2::X,
                -Vec2::X,
            )
        } else {
            (
                Pos2::new(ar.left(), ar.center().y),
                Pos2::new(br.right(), br.center().y),
                -Vec2::X,
                Vec2::X,
            )
        }
    } else if d.y >= 0.0 {
        (
            Pos2::new(ar.center().x, ar.bottom()),
            Pos2::new(br.center().x, br.top()),
            Vec2::Y,
            -Vec2::Y,
        )
    } else {
        (
            Pos2::new(ar.center().x, ar.top()),
            Pos2::new(br.center().x, br.bottom()),
            -Vec2::Y,
            Vec2::Y,
        )
    }
}

/// Inspector for the selected group or flow.
pub fn annotation_inspector(app: &mut TtgApp, ui: &mut Ui, a: &Annotation) {
    use egui::RichText;
    match a {
        Annotation::Group(id) => {
            let Some(g) = app.active_view().and_then(|v| v.group(id)).cloned() else {
                app.selected_annotation = None;
                return;
            };
            ui.label(RichText::new("Group").strong().size(16.0));
            ui.label(RichText::new("A grouping box for this view only: never exported. Drag its title to move it with everything inside.").small().color(Color32::from_gray(110)));
            ui.add_space(6.0);
            let mut label = g.label.clone();
            ui.horizontal(|ui| {
                ui.label("Label");
                let r = ui.add(egui::TextEdit::singleline(&mut label).desired_width(f32::INFINITY));
                if r.changed() {
                    if let Some(gm) = app.active_view_mut().and_then(|v| v.group_mut(id)) {
                        gm.label = label.clone();
                    }
                    app.dirty = true;
                }
                if r.gained_focus() {
                    app.edit_snapshot = Some(app.snapshot());
                }
                if r.lost_focus() {
                    if let Some(before) = app.edit_snapshot.take() {
                        app.finish(before);
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("Colour");
                for (i, c) in PALETTE.iter().enumerate() {
                    let col = parse_color(c).unwrap();
                    let cur = g.color.as_deref() == Some(*c);
                    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
                    ui.painter().rect(
                        rect,
                        CornerRadius::same(4),
                        col,
                        Stroke::new(
                            if cur { 2.0 } else { 1.0 },
                            if cur {
                                Color32::BLACK
                            } else {
                                Color32::from_gray(180)
                            },
                        ),
                        StrokeKind::Inside,
                    );
                    if resp.clicked() {
                        let before = app.snapshot();
                        if let Some(gm) = app.active_view_mut().and_then(|v| v.group_mut(id)) {
                            gm.color = Some(c.to_string());
                        }
                        app.finish(before);
                    }
                    let _ = i;
                }
            });
            let members = app.group_members(id);
            ui.label(
                RichText::new(format!(
                    "Contains {}: {}",
                    members.len(),
                    members
                        .iter()
                        .map(|m| app
                            .project
                            .entity(m)
                            .map(|e| e.name.to_string())
                            .unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .small()
                .color(Color32::from_gray(110)),
            );
            ui.add_space(6.0);
            if ui.button("Data flow from this group…").clicked() {
                app.flow_from = Some(FlowEnd::Group { group: id.clone() });
                app.status = "Click the target of the data flow (Esc to cancel)".into();
            }
            if ui.button("Delete group").clicked() {
                let before = app.snapshot();
                app.remove_annotation(a);
                app.finish(before);
            }
        }
        Annotation::Flow(id) => {
            let Some(f) = app.active_view().and_then(|v| v.flow(id)).cloned() else {
                app.selected_annotation = None;
                return;
            };
            ui.label(RichText::new("Data flow").strong().size(16.0));
            let name = |app: &TtgApp, e: &FlowEnd| match e {
                FlowEnd::Entity { entity } => app
                    .project
                    .entity(entity)
                    .map(|x| x.name.to_string())
                    .unwrap_or_default(),
                FlowEnd::Group { group } => app
                    .active_view()
                    .and_then(|v| v.group(group))
                    .map(|g| format!("[{}]", g.label))
                    .unwrap_or_default(),
            };
            ui.label(
                RichText::new(format!("{}  →  {}", name(app, &f.from), name(app, &f.to)))
                    .small()
                    .color(Color32::from_gray(110)),
            );
            ui.label(
                RichText::new(
                    "An architecture-map arrow for this view only: not a dependency, never exported.",
                )
                .small()
                .color(Color32::from_gray(110)),
            );
            ui.add_space(6.0);
            let mut label = f.label.clone();
            ui.horizontal(|ui| {
                ui.label("Label");
                let r = ui.add(
                    egui::TextEdit::singleline(&mut label)
                        .desired_width(f32::INFINITY)
                        .hint_text("e.g. job requests"),
                );
                if r.changed() {
                    if let Some(fm) = app.active_view_mut().and_then(|v| v.flow_mut(id)) {
                        fm.label = label.clone();
                    }
                    app.dirty = true;
                }
                if r.gained_focus() {
                    app.edit_snapshot = Some(app.snapshot());
                }
                if r.lost_focus() {
                    if let Some(before) = app.edit_snapshot.take() {
                        app.finish(before);
                    }
                }
            });
            let mut dashed = f.dashed;
            if ui.checkbox(&mut dashed, "Dashed").changed() {
                let before = app.snapshot();
                if let Some(fm) = app.active_view_mut().and_then(|v| v.flow_mut(id)) {
                    fm.dashed = dashed;
                }
                app.finish(before);
            }
            ui.horizontal(|ui| {
                if ui.button("Reverse direction").clicked() {
                    let before = app.snapshot();
                    if let Some(fm) = app.active_view_mut().and_then(|v| v.flow_mut(id)) {
                        std::mem::swap(&mut fm.from, &mut fm.to);
                    }
                    app.finish(before);
                }
                if ui.button("Delete flow").clicked() {
                    let before = app.snapshot();
                    app.remove_annotation(a);
                    app.finish(before);
                }
            });
        }
    }
}
