//! View-local layout and annotations: per-view positions, grouping boxes, data-flow
//! arrows, note boxes and logical (annotation-only) nodes. None of this reaches
//! codegen; it is the "architecture map" side of a view.

use crate::app::{Drag, TtgApp};
use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};
use ttg_core::{
    view as geom, Flow, FlowEnd, Group, Id, Logical, Note, NoteAnchor, Position, Project, Size, View,
    ViewLayout,
};

/// What annotation is selected on the canvas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Annotation {
    Group(Id),
    Flow(Id),
    Note(Id),
    Logical(Id),
}

const GROUP_HEADER: f32 = 26.0;
const PALETTE: &[&str] = &[
    "#5b7fb5", "#5fa870", "#d08a3a", "#b05aa0", "#c95555", "#3aa6a0", "#8a7f5a",
];
/// Muted ink for the logical-node style, matched by the legend.
const LOGICAL_INK: Color32 = Color32::from_rgb(120, 125, 135);
const NOTE_FILL: Color32 = Color32::from_rgb(253, 248, 228);
const NOTE_INK: Color32 = Color32::from_rgb(150, 130, 60);

/// egui rect of a core view rect.
pub fn to_rect(r: geom::Rect) -> Rect {
    Rect::from_min_size(Pos2::new(r.x, r.y), Vec2::new(r.w, r.h))
}

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
        self.active_view()?
            .group(gid)
            .map(|g| to_rect(geom::group_rect(g)))
    }

    /// Where a note is drawn: beside its anchor, or at its own position.
    pub fn note_rect(&self, nid: &str) -> Option<Rect> {
        let v = self.active_view()?;
        let n = v.note(nid)?;
        Some(to_rect(geom::note_rect(&self.project, v, n, &|id| {
            self.is_visible(id)
        })))
    }

    /// Screen rect of what a note is anchored to.
    fn anchor_screen_rect(&self, a: &NoteAnchor, origin: Pos2) -> Option<Rect> {
        let v = self.active_view()?;
        let r = geom::anchor_rect(&self.project, v, a, &|id| self.is_visible(id))?;
        Some(self.camera.rect_to_screen(origin, to_rect(r)))
    }

    /// World rect of a flow endpoint.
    pub fn end_rect(&self, end: &FlowEnd) -> Option<Rect> {
        let v = self.active_view()?;
        geom::end_rect(&self.project, v, end, &|id| self.is_visible(id)).map(to_rect)
    }

    /// Visible entities whose centre lies inside the group's box.
    pub fn group_members(&self, gid: &str) -> Vec<Id> {
        let Some(v) = self.active_view() else {
            return Vec::new();
        };
        geom::group_members(&self.project, v, gid, &|id| self.is_visible(id))
    }

    /// The members plus the contents of any container among them, so dragging the box
    /// moves a network exactly as dragging the network itself would.
    fn group_movers(&self, gid: &str) -> Vec<Id> {
        let members = self.group_members(gid);
        let mut all: std::collections::BTreeSet<Id> = members.iter().cloned().collect();
        for m in members
            .iter()
            .filter(|m| self.project.containers.contains_key(*m))
        {
            all.extend(self.project.descendants_of(m));
        }
        all.into_iter().collect()
    }

    /// Everything else a group takes with it: the logical nodes and free notes inside
    /// it, and the boxes nested in it.
    fn group_passengers(&self, gid: &str) -> (Vec<Id>, Vec<Id>, Vec<Id>) {
        let Some(v) = self.active_view() else {
            return Default::default();
        };
        let logicals = geom::group_logicals(v, gid);
        let nested = geom::group_children(v, gid);
        let Some(r) = self.group_rect(gid) else {
            return (logicals, Vec::new(), nested);
        };
        let notes = v
            .notes
            .iter()
            .filter(|n| n.anchor.is_none())
            .filter(|n| self.note_rect(&n.id).is_some_and(|nr| r.contains(nr.center())))
            .map(|n| n.id.clone())
            .collect();
        (logicals, notes, nested)
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

    #[allow(clippy::too_many_arguments)]
    pub fn add_flow(
        &mut self,
        from: FlowEnd,
        to: FlowEnd,
        label: &str,
        dashed: bool,
        step: Option<u32>,
        color: Option<String>,
    ) -> Option<Id> {
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
            step,
            color,
        });
        self.finish(before);
        self.selected_annotation = Some(Annotation::Flow(id.clone()));
        self.selection.clear();
        self.selected_edge = None;
        Some(id)
    }

    pub fn add_note(
        &mut self,
        title: &str,
        body: &str,
        pos: Position,
        size: Size,
        anchor: Option<NoteAnchor>,
    ) -> Option<Id> {
        self.active_view?;
        let id = self.project.fresh_id("note");
        let before = self.snapshot();
        self.active_view_mut().unwrap().notes.push(Note {
            id: id.clone(),
            title: title.to_string(),
            body: body.to_string(),
            position: pos,
            size,
            anchor,
        });
        self.finish(before);
        self.select_annotation(Annotation::Note(id.clone()));
        Some(id)
    }

    pub fn add_logical(
        &mut self,
        name: &str,
        icon: &str,
        subtitle: &str,
        pos: Position,
        size: Size,
    ) -> Option<Id> {
        self.active_view?;
        let id = self.project.fresh_id("lg");
        let before = self.snapshot();
        self.active_view_mut().unwrap().logicals.push(Logical {
            id: id.clone(),
            name: name.to_string(),
            icon: icon.to_string(),
            subtitle: subtitle.to_string(),
            position: pos,
            size,
        });
        self.finish(before);
        self.select_annotation(Annotation::Logical(id.clone()));
        Some(id)
    }

    pub fn select_annotation(&mut self, a: Annotation) {
        self.selected_annotation = Some(a);
        self.selection.clear();
        self.selected_edge = None;
    }

    pub fn remove_annotation(&mut self, a: &Annotation) -> bool {
        let Some(v) = self.active_view_mut() else {
            return false;
        };
        let count = |v: &View| v.groups.len() + v.flows.len() + v.notes.len() + v.logicals.len();
        let before_len = count(v);
        // Removing something a flow or note points at takes the flow with it and leaves
        // the note floating rather than pointing at nothing.
        let detach = |v: &mut View, id: &str| {
            v.flows.retain(|f| f.from.id() != id && f.to.id() != id);
            for n in &mut v.notes {
                if n.anchor.as_ref().is_some_and(|x| x.id() == id) {
                    n.anchor = None;
                }
            }
        };
        match a {
            Annotation::Group(id) => {
                v.groups.retain(|g| &g.id != id);
                detach(v, id);
            }
            Annotation::Logical(id) => {
                v.logicals.retain(|l| &l.id != id);
                detach(v, id);
            }
            Annotation::Flow(id) => {
                v.flows.retain(|f| &f.id != id);
                detach(v, id);
            }
            Annotation::Note(id) => v.notes.retain(|n| &n.id != id),
        }
        let changed = count(v) != before_len;
        if self.selected_annotation.as_ref() == Some(a) {
            self.selected_annotation = None;
        }
        changed
    }

    /// Resolve a flow endpoint from an id, an entity name, a group label or the name of
    /// a logical node.
    #[cfg_attr(not(feature = "mcp"), allow(dead_code))]
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
            if let Some(l) = v
                .logicals
                .iter()
                .find(|l| l.id == key || l.name.eq_ignore_ascii_case(key))
            {
                return Some(FlowEnd::Logical {
                    logical: l.id.clone(),
                });
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
            if self.add_flow(from, to, "", false, None, None).is_some() {
                self.status = "Data flow added; give it a label in the inspector".into();
            } else {
                self.status = "A flow needs two different ends".into();
            }
        }
    }
}

/// Draw the active view's groups (behind everything) and handle their interaction.
/// Boxes are drawn largest first, so a box nested inside another stays visible and
/// clickable.
pub fn draw_groups(app: &mut TtgApp, ui: &mut Ui, origin: Pos2) {
    let Some(view) = app.active_view() else { return };
    let order = geom::groups_by_area(view);
    let palette_index: std::collections::BTreeMap<Id, usize> = view
        .groups
        .iter()
        .enumerate()
        .map(|(i, g)| (g.id.clone(), i))
        .collect();
    let groups: Vec<Group> = order.iter().filter_map(|id| view.group(id).cloned()).collect();
    let zoom = app.camera.zoom;
    let painter = ui.painter_at(app.canvas_rect);
    for g in groups.iter() {
        let i = palette_index[&g.id];
        let Some(wr) = app.group_rect(&g.id) else { continue };
        let sr = app.camera.rect_to_screen(origin, wr);
        let color = group_color(g, i);
        let selected = app.selected_annotation == Some(Annotation::Group(g.id.clone()));
        painter.rect(
            sr,
            CornerRadius::same(10),
            Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 28),
            Stroke::new(
                if selected { 2.5_f32 } else { 1.5_f32 },
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
                app.select_annotation(Annotation::Group(g.id.clone()));
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
                let members = app.group_movers(&g.id);
                let (logicals, notes, nested) = app.group_passengers(&g.id);
                if let Some(v) = app.active_view_mut() {
                    for id in std::iter::once(&g.id).chain(nested.iter()) {
                        if let Some(gm) = v.group_mut(id) {
                            gm.position.x += dx;
                            gm.position.y += dy;
                        }
                    }
                    for id in &logicals {
                        if let Some(l) = v.logical_mut(id) {
                            l.position.x += dx;
                            l.position.y += dy;
                        }
                    }
                    for id in &notes {
                        if let Some(n) = v.note_mut(id) {
                            n.position.x += dx;
                            n.position.y += dy;
                        }
                    }
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
            Stroke::new(1.5_f32, color),
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

/// Range of the 25 points along a flow's curve that a label may sit at: never so close
/// to an end that it covers the node or the arrowhead.
const LABEL_MIN: usize = 3;
const LABEL_MAX: usize = 21;

/// How far along the arrow a label starts, so flows sharing an endpoint do not print on
/// top of each other: the first keeps the middle, the next ones step either side. The
/// steps are wide enough that consecutive labels clear each other on their own; where
/// they cannot (a short arrow, a long label), [`label_slots`] slides them further.
fn label_index(crowding: usize) -> usize {
    let step = crowding.div_ceil(2) * 4;
    let mid = 12_i32;
    let pos = if crowding % 2 == 1 {
        mid - step as i32
    } else {
        mid + step as i32
    };
    pos.clamp(LABEL_MIN as i32, LABEL_MAX as i32) as usize
}

/// Points to try for a label, the chosen one first and then alternately further along
/// and further back, so a collision slides the box along its own curve rather than
/// jumping somewhere unrelated. Deterministic: the same diagram draws the same way.
fn label_slots(start: usize) -> Vec<usize> {
    let mut out = vec![start.clamp(LABEL_MIN, LABEL_MAX)];
    for d in 1..=(LABEL_MAX - LABEL_MIN) {
        if start + d <= LABEL_MAX {
            out.push(start + d);
        }
        if start >= LABEL_MIN + d {
            out.push(start - d);
        }
    }
    out
}

/// For each flow, how many earlier flows already touch one of its endpoints.
fn crowding(flows: &[Flow]) -> Vec<usize> {
    let mut seen: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    flows
        .iter()
        .map(|f| {
            let n = [f.from.id(), f.to.id()]
                .iter()
                .map(|e| *seen.get(*e).unwrap_or(&0))
                .max()
                .unwrap_or(0);
            for e in [f.from.id(), f.to.id()] {
                *seen.entry(e).or_insert(0) += 1;
            }
            n
        })
        .collect()
}

/// The box for one flow's label: the first slot along its own curve that clears every
/// label already placed this frame, or the slot it wanted when none of them does.
fn place_label(pts: &[Pos2], base: usize, size: Vec2, placed: &[Rect]) -> Rect {
    label_slots(base)
        .into_iter()
        .filter(|i| *i < pts.len())
        .map(|i| Rect::from_center_size(pts[i], size))
        .find(|r| !placed.iter().any(|p| p.intersects(*r)))
        .unwrap_or_else(|| Rect::from_center_size(pts[base.min(pts.len() - 1)], size))
}

/// Draw the active view's data-flow arrows (above nodes) and handle selection.
pub fn draw_flows(app: &mut TtgApp, ui: &mut Ui, origin: Pos2) {
    let Some(view) = app.active_view() else { return };
    let flows: Vec<Flow> = view.flows.clone();
    let crowd = crowding(&flows);
    let zoom = app.camera.zoom;
    let painter = ui.painter_at(app.canvas_rect);
    let ink = Color32::from_rgb(45, 55, 75);
    // Label boxes already drawn this frame; the next one slides along its curve until
    // it clears them, so arrows meeting at one node stay readable.
    let mut placed: Vec<Rect> = Vec::new();
    for (fi, f) in flows.iter().enumerate() {
        let (Some(ar), Some(br)) = (app.end_rect(&f.from), app.end_rect(&f.to)) else {
            continue;
        };
        let ar = app.camera.rect_to_screen(origin, ar);
        let br = app.camera.rect_to_screen(origin, br);
        // Attach on the facing sides, centred.
        let (a, b, na, nb) = facing_points(ar, br);
        let selected = app.selected_annotation == Some(Annotation::Flow(f.id.clone()));
        let own = f.color.as_deref().and_then(parse_color);
        let color = if selected {
            Color32::from_rgb(30, 100, 220)
        } else {
            own.unwrap_or(ink)
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
        // Step badge, just outside the source.
        if let Some(step) = f.step {
            let c = pts[2] + na * 4.0 * zoom;
            let r = (10.0 * zoom).clamp(7.0, 13.0);
            painter.circle_filled(c, r, color);
            painter.circle_stroke(c, r, Stroke::new(1.5_f32, Color32::WHITE));
            painter.text(
                c,
                Align2::CENTER_CENTER,
                step.to_string(),
                FontId::proportional((11.0 * zoom).max(6.0)),
                Color32::WHITE,
            );
        }
        let label = if f.label.is_empty() {
            "flow".to_string()
        } else {
            f.label.clone()
        };
        let font = FontId::proportional((12.0 * zoom).max(7.0));
        let galley = painter.layout_no_wrap(label, font, Color32::WHITE);
        let box_size = galley.size() + Vec2::new(12.0, 6.0);
        let lr = place_label(&pts, label_index(crowd[fi]), box_size, &placed);
        placed.push(lr);
        let resp = ui.interact(lr, ui.id().with(("flow", &f.id)), Sense::click());
        painter.rect_filled(lr, CornerRadius::same(6), color);
        painter.galley(lr.min + Vec2::new(6.0, 3.0), galley, Color32::WHITE);
        if resp.clicked() {
            app.select_annotation(Annotation::Flow(f.id.clone()));
        }
        resp.context_menu(|ui| {
            if ui.button("Note about this flow…").clicked() {
                let anchor = NoteAnchor::Flow { flow: f.id.clone() };
                app.add_note(
                    "Note",
                    "",
                    Position::default(),
                    Size { w: 260, h: 120 },
                    Some(anchor),
                );
                ui.close();
            }
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

/// Draw the active view's logical nodes: annotation-only boxes that export nothing but
/// that flows can start from and end at.
pub fn draw_logicals(app: &mut TtgApp, ui: &mut Ui, origin: Pos2) {
    let Some(view) = app.active_view() else { return };
    let logicals: Vec<Logical> = view.logicals.clone();
    let zoom = app.camera.zoom;
    let painter = ui.painter_at(app.canvas_rect);
    for l in &logicals {
        let sr = app.camera.rect_to_screen(origin, to_rect(geom::logical_rect(l)));
        let selected = app.selected_annotation == Some(Annotation::Logical(l.id.clone()));
        let ink = if selected {
            Color32::from_rgb(30, 100, 220)
        } else {
            LOGICAL_INK
        };
        let cr = CornerRadius::same((6.0 * zoom).clamp(2.0, 8.0) as u8);
        painter.rect(
            sr,
            cr,
            Color32::from_gray(250),
            Stroke::new(if selected { 2.5_f32 } else { 1.2_f32 }, ink),
            StrokeKind::Inside,
        );
        dashed_outline(&painter, sr, ink);
        let icon_rect = Rect::from_min_size(
            sr.min + Vec2::splat(6.0 * zoom),
            Vec2::new(44.0 * zoom, sr.height() - 12.0 * zoom),
        );
        painter.rect_filled(icon_rect, cr, Color32::from_gray(225));
        painter.text(
            icon_rect.center(),
            Align2::CENTER_CENTER,
            if l.icon.is_empty() { "◇" } else { l.icon.as_str() },
            FontId::proportional((12.0 * zoom).max(5.0)),
            LOGICAL_INK,
        );
        let tx = icon_rect.right() + 8.0 * zoom;
        painter.text(
            Pos2::new(tx, sr.min.y + 20.0 * zoom),
            Align2::LEFT_CENTER,
            &l.name,
            FontId::proportional((13.5 * zoom).max(6.0)),
            Color32::from_gray(60),
        );
        painter.text(
            Pos2::new(tx, sr.min.y + 42.0 * zoom),
            Align2::LEFT_CENTER,
            if l.subtitle.is_empty() {
                "annotation only"
            } else {
                l.subtitle.as_str()
            },
            FontId::proportional((11.0 * zoom).max(5.0)),
            Color32::from_gray(140),
        );

        let resp = ui.interact(sr, ui.id().with(("logical", &l.id)), Sense::click_and_drag());
        if resp.clicked() {
            if app.flow_from.is_some() {
                app.finish_flow_to(FlowEnd::Logical {
                    logical: l.id.clone(),
                });
            } else {
                app.select_annotation(Annotation::Logical(l.id.clone()));
            }
        }
        if resp.drag_started_by(egui::PointerButton::Primary) {
            app.select_annotation(Annotation::Logical(l.id.clone()));
            app.drag = Drag::Move {
                before: app.snapshot(),
                accum: Vec2::ZERO,
            };
        }
        if resp.dragged_by(egui::PointerButton::Primary) {
            if let Some((dx, dy)) = drag_step(&mut app.drag, resp.drag_delta(), zoom) {
                if let Some(lm) = app.active_view_mut().and_then(|v| v.logical_mut(&l.id)) {
                    lm.position.x += dx;
                    lm.position.y += dy;
                }
            }
        }
        if resp.drag_stopped_by(egui::PointerButton::Primary) {
            if let Drag::Move { before, .. } = std::mem::replace(&mut app.drag, Drag::None) {
                app.finish(before);
            }
        }
        resp.clone()
            .on_hover_text("Logical node: documentation only, never exported.");
        resp.context_menu(|ui| {
            if ui.button("Data flow from here").clicked() {
                app.flow_from = Some(FlowEnd::Logical {
                    logical: l.id.clone(),
                });
                app.status = "Click the target of the data flow (Esc to cancel)".into();
                ui.close();
            }
            if ui.button("Delete logical node").clicked() {
                let before = app.snapshot();
                app.remove_annotation(&Annotation::Logical(l.id.clone()));
                app.finish(before);
                ui.close();
            }
        });
    }
}

/// Draw the active view's notes on top of everything else.
pub fn draw_notes(app: &mut TtgApp, ui: &mut Ui, origin: Pos2) {
    let Some(view) = app.active_view() else { return };
    let notes: Vec<Note> = view.notes.clone();
    let zoom = app.camera.zoom;
    let painter = ui.painter_at(app.canvas_rect);
    for n in &notes {
        let Some(wr) = app.note_rect(&n.id) else { continue };
        let sr = app.camera.rect_to_screen(origin, wr);
        let selected = app.selected_annotation == Some(Annotation::Note(n.id.clone()));
        painter.rect(
            sr,
            CornerRadius::same(6),
            NOTE_FILL,
            Stroke::new(
                if selected { 2.5_f32 } else { 1.0_f32 },
                if selected {
                    Color32::from_rgb(30, 100, 220)
                } else {
                    NOTE_INK
                },
            ),
            StrokeKind::Inside,
        );
        // A line back to whatever the note explains.
        if let Some(a) = n.anchor.as_ref() {
            if let Some(ar) = app.anchor_screen_rect(a, origin) {
                painter.line_segment(
                    [ar.right_center(), sr.left_center()],
                    Stroke::new(1.0_f32, NOTE_INK.gamma_multiply(0.7)),
                );
            }
        }
        let pad = 8.0 * zoom;
        let mut y = sr.min.y + pad;
        if !n.title.is_empty() {
            let g = painter.layout(
                n.title.clone(),
                FontId::proportional((12.5 * zoom).max(7.0)),
                Color32::from_gray(50),
                sr.width() - pad * 2.0,
            );
            let h = g.size().y;
            painter.galley(Pos2::new(sr.min.x + pad, y), g, Color32::from_gray(50));
            y += h + 3.0 * zoom;
        }
        if !n.body.is_empty() {
            let g = painter.layout(
                n.body.clone(),
                FontId::proportional((11.0 * zoom).max(6.0)),
                Color32::from_gray(90),
                sr.width() - pad * 2.0,
            );
            painter.galley(Pos2::new(sr.min.x + pad, y), g, Color32::from_gray(90));
        }

        let resp = ui.interact(sr, ui.id().with(("note", &n.id)), Sense::click_and_drag());
        if resp.clicked() {
            app.select_annotation(Annotation::Note(n.id.clone()));
        }
        if resp.drag_started_by(egui::PointerButton::Primary) {
            app.select_annotation(Annotation::Note(n.id.clone()));
            app.drag = Drag::Move {
                before: app.snapshot(),
                accum: Vec2::ZERO,
            };
        }
        if resp.dragged_by(egui::PointerButton::Primary) {
            if let Some((dx, dy)) = drag_step(&mut app.drag, resp.drag_delta(), zoom) {
                if let Some(nm) = app.active_view_mut().and_then(|v| v.note_mut(&n.id)) {
                    nm.position.x += dx;
                    nm.position.y += dy;
                }
            }
        }
        if resp.drag_stopped_by(egui::PointerButton::Primary) {
            if let Drag::Move { before, .. } = std::mem::replace(&mut app.drag, Drag::None) {
                app.finish(before);
            }
        }
        resp.clone().on_hover_text(if n.anchor.is_some() {
            "Note (never exported); it follows what it explains. Drag to change the offset."
        } else {
            "Note (never exported). Drag to move."
        });
        resp.context_menu(|ui| {
            if ui.button("Delete note").clicked() {
                let before = app.snapshot();
                app.remove_annotation(&Annotation::Note(n.id.clone()));
                app.finish(before);
                ui.close();
            }
        });
    }
}

/// Accumulate a drag into whole world units, returning the step to apply.
fn drag_step(drag: &mut Drag, delta: Vec2, zoom: f32) -> Option<(i32, i32)> {
    let Drag::Move { accum, .. } = drag else {
        return None;
    };
    *accum += delta / zoom;
    let dx = accum.x.trunc();
    let dy = accum.y.trunc();
    accum.x -= dx;
    accum.y -= dy;
    if dx == 0.0 && dy == 0.0 {
        return None;
    }
    Some((dx as i32, dy as i32))
}

fn dashed_outline(painter: &egui::Painter, r: Rect, color: Color32) {
    let stroke = Stroke::new(1.5_f32, color);
    let c = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom()];
    for i in 0..4 {
        painter.add(egui::Shape::dashed_line(
            &[c[i], c[(i + 1) % 4]],
            stroke,
            6.0,
            4.0,
        ));
    }
}

/// The legend: what the colours and line styles on this view mean.
pub fn legend(app: &mut TtgApp, ui: &mut Ui) {
    use egui::RichText;
    let Some(v) = app.active_view() else { return };
    if !v.legend {
        return;
    }
    let groups: Vec<(String, Color32)> = v
        .groups
        .iter()
        .enumerate()
        .map(|(i, g)| (g.label.clone(), group_color(g, i)))
        .collect();
    let flow_colors: Vec<(String, Color32)> = v
        .flows
        .iter()
        .filter_map(|f| {
            f.color
                .as_deref()
                .and_then(parse_color)
                .map(|c| (f.label.clone(), c))
        })
        .collect();
    let logicals = !v.logicals.is_empty();
    let pos = app.canvas_rect.right_top() + Vec2::new(-16.0, 16.0);
    let mut close = false;
    egui::Area::new(ui.id().with("view-legend"))
        .fixed_pos(pos - Vec2::new(210.0, 0.0))
        .order(egui::Order::Foreground)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_max_width(200.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Legend").strong().size(12.0));
                    if ui.small_button("✕").on_hover_text("Hide the legend").clicked() {
                        close = true;
                    }
                });
                for (label, color) in &groups {
                    swatch(ui, *color, label, false);
                }
                let mut seen: Vec<Color32> = Vec::new();
                for (label, color) in &flow_colors {
                    if seen.contains(color) {
                        continue;
                    }
                    seen.push(*color);
                    swatch(ui, *color, label, true);
                }
                ui.separator();
                line_key(ui, false, "solid — data flow");
                line_key(ui, true, "dashed — optional / async");
                if logicals {
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(Vec2::new(18.0, 12.0), Sense::hover());
                        dashed_outline(ui.painter(), rect, LOGICAL_INK);
                        ui.label(RichText::new("logical (not exported)").small());
                    });
                }
            });
        });
    if close {
        let before = app.snapshot();
        if let Some(v) = app.active_view_mut() {
            v.legend = false;
        }
        app.finish(before);
    }
}

fn swatch(ui: &mut Ui, color: Color32, label: &str, arrow: bool) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(18.0, 12.0), Sense::hover());
        if arrow {
            ui.painter().line_segment(
                [rect.left_center(), rect.right_center()],
                Stroke::new(3.0_f32, color),
            );
        } else {
            ui.painter().rect(
                rect,
                CornerRadius::same(3),
                Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 40),
                Stroke::new(1.5_f32, color),
                StrokeKind::Inside,
            );
        }
        ui.label(
            egui::RichText::new(if label.is_empty() { "(unlabelled)" } else { label })
                .small()
                .color(Color32::from_gray(80)),
        );
    });
}

fn line_key(ui: &mut Ui, dashed: bool, label: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(18.0, 12.0), Sense::hover());
        let stroke = Stroke::new(2.5_f32, Color32::from_rgb(45, 55, 75));
        if dashed {
            ui.painter().add(egui::Shape::dashed_line(
                &[rect.left_center(), rect.right_center()],
                stroke,
                5.0,
                3.0,
            ));
        } else {
            ui.painter()
                .line_segment([rect.left_center(), rect.right_center()], stroke);
        }
        ui.label(egui::RichText::new(label).small().color(Color32::from_gray(80)));
    });
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
                track_edit(app, &r);
            });
            ui.horizontal(|ui| {
                ui.label("Colour");
                if let Some(c) = color_row(ui, g.color.as_deref(), false) {
                    let before = app.snapshot();
                    if let Some(gm) = app.active_view_mut().and_then(|v| v.group_mut(id)) {
                        gm.color = c;
                    }
                    app.finish(before);
                }
            });
            let members = app.group_members(id);
            let logicals = app
                .active_view()
                .map(|v| geom::group_logicals(v, id))
                .unwrap_or_default();
            let names: Vec<String> = members
                .iter()
                .map(|m| {
                    app.project
                        .entity(m)
                        .map(|e| e.name.to_string())
                        .unwrap_or_default()
                })
                .chain(logicals.iter().filter_map(|l| {
                    app.active_view()
                        .and_then(|v| v.logical(l))
                        .map(|l| l.name.clone())
                }))
                .collect();
            ui.label(
                RichText::new(format!("Contains {}: {}", names.len(), names.join(", ")))
                    .small()
                    .color(Color32::from_gray(110)),
            );
            if let Some(p) = app.active_view().and_then(|v| geom::group_parent(v, id)) {
                ui.label(
                    RichText::new(format!("Nested inside \"{}\"", p.label))
                        .small()
                        .color(Color32::from_gray(110)),
                );
            }
            ui.add_space(6.0);
            if ui.button("Data flow from this group…").clicked() {
                app.flow_from = Some(FlowEnd::Group { group: id.clone() });
                app.status = "Click the target of the data flow (Esc to cancel)".into();
            }
            if ui.button("Note about this group…").clicked() {
                app.add_note(
                    "Note",
                    "",
                    Position::default(),
                    Size { w: 260, h: 120 },
                    Some(NoteAnchor::End(FlowEnd::Group { group: id.clone() })),
                );
            }
            if ui.button("Delete group").clicked() {
                let before = app.snapshot();
                app.remove_annotation(a);
                app.finish(before);
            }
        }
        Annotation::Note(id) => note_inspector(app, ui, a, id),
        Annotation::Logical(id) => logical_inspector(app, ui, a, id),
        Annotation::Flow(id) => {
            let Some(f) = app.active_view().and_then(|v| v.flow(id)).cloned() else {
                app.selected_annotation = None;
                return;
            };
            ui.label(RichText::new("Data flow").strong().size(16.0));
            let name = |app: &TtgApp, e: &FlowEnd| {
                app.active_view()
                    .map(|v| geom::end_name(&app.project, v, e))
                    .unwrap_or_default()
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
                track_edit(app, &r);
            });
            let mut dashed = f.dashed;
            if ui.checkbox(&mut dashed, "Dashed (optional / async)").changed() {
                let before = app.snapshot();
                if let Some(fm) = app.active_view_mut().and_then(|v| v.flow_mut(id)) {
                    fm.dashed = dashed;
                }
                app.finish(before);
            }
            ui.horizontal(|ui| {
                ui.label("Step");
                let mut numbered = f.step.is_some();
                if ui
                    .checkbox(&mut numbered, "")
                    .on_hover_text("Number this flow; the badge is drawn where the arrow starts")
                    .changed()
                {
                    let before = app.snapshot();
                    let next = app
                        .active_view()
                        .map(|v| v.flows.iter().filter_map(|x| x.step).max().unwrap_or(0) + 1)
                        .unwrap_or(1);
                    if let Some(fm) = app.active_view_mut().and_then(|v| v.flow_mut(id)) {
                        fm.step = numbered.then_some(next);
                    }
                    app.finish(before);
                }
                if let Some(step) = f.step {
                    let mut n = step;
                    let r = ui.add(egui::DragValue::new(&mut n).range(1..=999));
                    if r.changed() {
                        if let Some(fm) = app.active_view_mut().and_then(|v| v.flow_mut(id)) {
                            fm.step = Some(n);
                        }
                        app.dirty = true;
                    }
                    if r.gained_focus() || r.drag_started() {
                        app.edit_snapshot = Some(app.snapshot());
                    }
                    if r.lost_focus() || r.drag_stopped() {
                        if let Some(before) = app.edit_snapshot.take() {
                            app.finish(before);
                        }
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("Colour");
                if let Some(c) = color_row(ui, f.color.as_deref(), true) {
                    let before = app.snapshot();
                    if let Some(fm) = app.active_view_mut().and_then(|v| v.flow_mut(id)) {
                        fm.color = c;
                    }
                    app.finish(before);
                }
            });
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

/// A row of palette swatches; returns the pick (`Some(None)` = "no colour of its own").
fn color_row(ui: &mut Ui, current: Option<&str>, allow_none: bool) -> Option<Option<String>> {
    let mut picked = None;
    for c in PALETTE {
        let col = parse_color(c).unwrap();
        let cur = current == Some(*c);
        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
        ui.painter().rect(
            rect,
            CornerRadius::same(4),
            col,
            Stroke::new(
                if cur { 2.0_f32 } else { 1.0_f32 },
                if cur {
                    Color32::BLACK
                } else {
                    Color32::from_gray(180)
                },
            ),
            StrokeKind::Inside,
        );
        if resp.clicked() {
            picked = Some(Some(c.to_string()));
        }
    }
    if allow_none && ui.small_button("default").clicked() {
        picked = Some(None);
    }
    picked
}

/// Inspector for a note: title, body and what it is pinned to.
fn note_inspector(app: &mut TtgApp, ui: &mut Ui, a: &Annotation, id: &Id) {
    use egui::RichText;
    let Some(n) = app.active_view().and_then(|v| v.note(id)).cloned() else {
        app.selected_annotation = None;
        return;
    };
    ui.label(RichText::new("Note").strong().size(16.0));
    ui.label(
        RichText::new("Prose for this view only: never exported. Pin it to something and it follows that thing around the canvas.")
            .small()
            .color(Color32::from_gray(110)),
    );
    ui.add_space(6.0);
    let mut title = n.title.clone();
    ui.horizontal(|ui| {
        ui.label("Title");
        let r = ui.add(egui::TextEdit::singleline(&mut title).desired_width(f32::INFINITY));
        if r.changed() {
            if let Some(nm) = app.active_view_mut().and_then(|v| v.note_mut(id)) {
                nm.title = title.clone();
            }
            app.dirty = true;
        }
        track_edit(app, &r);
    });
    let mut body = n.body.clone();
    let r = ui.add(
        egui::TextEdit::multiline(&mut body)
            .desired_width(f32::INFINITY)
            .desired_rows(6)
            .hint_text("What a reader needs to know about this part of the picture"),
    );
    if r.changed() {
        if let Some(nm) = app.active_view_mut().and_then(|v| v.note_mut(id)) {
            nm.body = body.clone();
        }
        app.dirty = true;
    }
    track_edit(app, &r);
    ui.add_space(6.0);
    let anchored = n
        .anchor
        .as_ref()
        .map(|x| anchor_label(app, x))
        .unwrap_or_else(|| "nothing (free-floating)".into());
    ui.label(
        RichText::new(format!("Pinned to: {anchored}"))
            .small()
            .color(Color32::from_gray(110)),
    );
    ui.horizontal(|ui| {
        let sel = app.selection.iter().next().cloned();
        if ui
            .add_enabled(sel.is_some(), egui::Button::new("Pin to selected resource"))
            .clicked()
        {
            let before = app.snapshot();
            let anchor = NoteAnchor::End(FlowEnd::Entity { entity: sel.unwrap() });
            if let Some(nm) = app.active_view_mut().and_then(|v| v.note_mut(id)) {
                nm.anchor = Some(anchor);
                nm.position = Position::default();
            }
            app.finish(before);
        }
        if ui
            .add_enabled(n.anchor.is_some(), egui::Button::new("Unpin"))
            .clicked()
        {
            // Keep it where it is on screen rather than snapping back to the origin.
            let before = app.snapshot();
            let at = app.note_rect(id).map(|r| Position {
                x: r.min.x as i32,
                y: r.min.y as i32,
            });
            if let Some(nm) = app.active_view_mut().and_then(|v| v.note_mut(id)) {
                nm.anchor = None;
                if let Some(p) = at {
                    nm.position = p;
                }
            }
            app.finish(before);
        }
    });
    if ui.button("Delete note").clicked() {
        let before = app.snapshot();
        app.remove_annotation(a);
        app.finish(before);
    }
}

/// Inspector for a logical node.
fn logical_inspector(app: &mut TtgApp, ui: &mut Ui, a: &Annotation, id: &Id) {
    use egui::RichText;
    let Some(l) = app.active_view().and_then(|v| v.logical(id)).cloned() else {
        app.selected_annotation = None;
        return;
    };
    ui.label(RichText::new("Logical node").strong().size(16.0));
    ui.label(
        RichText::new("Something the diagram does not create — a browser, a third-party service, one workload inside a cluster. Nothing is exported for it, but flows can start and end here.")
            .small()
            .color(Color32::from_gray(110)),
    );
    ui.add_space(6.0);
    let mut fields = [
        ("Name", l.name.clone()),
        ("Icon", l.icon.clone()),
        ("Subtitle", l.subtitle.clone()),
    ];
    for (label, value) in fields.iter_mut() {
        ui.horizontal(|ui| {
            ui.label(*label);
            let r = ui.add(egui::TextEdit::singleline(value).desired_width(f32::INFINITY));
            if r.changed() {
                if let Some(lm) = app.active_view_mut().and_then(|v| v.logical_mut(id)) {
                    match *label {
                        "Name" => lm.name = value.clone(),
                        "Icon" => lm.icon = value.clone(),
                        _ => lm.subtitle = value.clone(),
                    }
                }
                app.dirty = true;
            }
            track_edit(app, &r);
        });
    }
    ui.add_space(6.0);
    if ui.button("Data flow from here…").clicked() {
        app.flow_from = Some(FlowEnd::Logical { logical: id.clone() });
        app.status = "Click the target of the data flow (Esc to cancel)".into();
    }
    if ui.button("Delete logical node").clicked() {
        let before = app.snapshot();
        app.remove_annotation(a);
        app.finish(before);
    }
}

fn anchor_label(app: &TtgApp, a: &NoteAnchor) -> String {
    let Some(v) = app.active_view() else {
        return String::new();
    };
    match a {
        NoteAnchor::End(e) => geom::end_name(&app.project, v, e),
        NoteAnchor::Flow { flow } => v
            .flow(flow)
            .map(|f| {
                if f.label.is_empty() {
                    "a data flow".to_string()
                } else {
                    format!("the \"{}\" flow", f.label)
                }
            })
            .unwrap_or_default(),
    }
}

/// One undo step per burst of typing: snapshot on focus, commit on blur.
fn track_edit(app: &mut TtgApp, r: &egui::Response) {
    if r.gained_focus() {
        app.edit_snapshot = Some(app.snapshot());
    }
    if r.lost_focus() {
        if let Some(before) = app.edit_snapshot.take() {
            app.finish(before);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 25 points in a straight line: the shape `bezier_points` hands the label code.
    fn curve() -> Vec<Pos2> {
        (0..=24).map(|i| Pos2::new(i as f32 * 20.0, 0.0)).collect()
    }

    #[test]
    fn label_slots_stay_on_the_arrow_and_start_where_asked() {
        for crowd in 0..12 {
            let base = label_index(crowd);
            assert!((LABEL_MIN..=LABEL_MAX).contains(&base), "{crowd} -> {base}");
            let slots = label_slots(base);
            assert_eq!(slots[0], base, "the wanted slot is tried first");
            assert!(slots.iter().all(|i| (LABEL_MIN..=LABEL_MAX).contains(i)));
            // Deterministic: the same input gives the same order every time.
            assert_eq!(slots, label_slots(base));
        }
        // Flows meeting at one node no longer start their labels in the same place.
        assert_ne!(label_index(0), label_index(1));
        assert_ne!(label_index(1), label_index(2));
    }

    #[test]
    fn a_label_slides_along_its_curve_until_it_clears_the_others() {
        let pts = curve();
        let size = Vec2::new(70.0, 20.0);
        // A whole band of the curve is already covered by earlier labels.
        let placed: Vec<Rect> = (8..=14).map(|i| Rect::from_center_size(pts[i], size)).collect();
        let r = place_label(&pts, 12, size, &placed);
        assert!(
            placed.iter().all(|p| !p.intersects(r)),
            "the label still overlaps: {r:?}"
        );
        // It slid along this arrow rather than jumping somewhere unrelated.
        assert!(pts.iter().any(|p| (p.x - r.center().x).abs() < 0.01), "{r:?}");
        // With nothing in the way it keeps the spot it asked for.
        assert_eq!(place_label(&pts, 12, size, &[]).center(), pts[12]);
    }
}
