//! The infinite pan/zoom canvas: containers, nodes, bezier edges and all direct
//! manipulation (move, reparent, resize, connect, marquee, palette drop).

use crate::app::{Drag, PaletteItem, TtgApp, NODE_H, NODE_W};
use egui::{
    epaint::CubicBezierShape, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind,
    Ui, Vec2,
};
use std::collections::BTreeSet;
use ttg_codegen::{Code, Severity};
use ttg_core::{Id, Relation};

const HEADER_H: f32 = 30.0;
const PORT_R: f32 = 6.0;
const MIN_CONTAINER: Vec2 = Vec2::new(220.0, 140.0);

fn category_color(cat: &str) -> Color32 {
    match cat {
        "compute" => Color32::from_rgb(66, 133, 244),
        "storage" => Color32::from_rgb(52, 168, 83),
        "network" => Color32::from_rgb(142, 68, 173),
        "iam" => Color32::from_rgb(230, 126, 34),
        "database" => Color32::from_rgb(26, 188, 156),
        "organization" => Color32::from_rgb(120, 130, 140),
        _ => Color32::from_rgb(100, 100, 100),
    }
}

fn relation_color(r: Relation) -> Color32 {
    match r {
        Relation::NetworkMembership => Color32::from_rgb(142, 68, 173),
        Relation::AttributeReference => Color32::from_rgb(52, 120, 200),
        Relation::IamBinding => Color32::from_rgb(230, 126, 34),
        Relation::Attachment => Color32::from_rgb(26, 160, 140),
        Relation::SendsTo => Color32::from_rgb(200, 60, 120),
        Relation::Reads => Color32::from_rgb(120, 90, 200),
        Relation::LogsTo => Color32::from_rgb(110, 110, 60),
        Relation::DependsOn => Color32::from_rgb(130, 130, 130),
    }
}

pub fn show(app: &mut TtgApp, ui: &mut Ui) {
    let (rect, bg) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
    app.canvas_rect = rect;
    let origin = rect.min;
    let painter = ui.painter_at(rect);
    let zoom = app.camera.zoom;

    if app.fit_requested {
        app.fit_requested = false;
        if let Some(b) = app.world_bounds() {
            app.camera.fit(rect, b);
        }
    }

    // ---- camera input
    let pointer = ui.input(|i| i.pointer.hover_pos());
    if bg.hovered() || matches!(app.drag, Drag::Marquee { .. }) {
        let (zd, scroll) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta));
        if let Some(p) = pointer {
            if zd != 1.0 {
                app.camera.zoom_at(origin, p, zd);
            }
        }
        if scroll != Vec2::ZERO && zd == 1.0 {
            app.camera.pan += scroll;
        }
    }
    if bg.dragged_by(egui::PointerButton::Middle) || bg.dragged_by(egui::PointerButton::Secondary) {
        app.camera.pan += bg.drag_delta();
    }

    // ---- grid
    draw_grid(&painter, rect, &app.camera);

    // ---- palette drop / background click
    if let Some(item) = bg.dnd_release_payload::<PaletteItem>() {
        if let Some(p) = pointer {
            let w = app.camera.to_world(origin, p);
            app.add_entity(&item.type_id, w - Vec2::new(NODE_W / 2.0, NODE_H / 2.0));
        }
    }
    if let Some((type_id, w)) = app.add_at.take() {
        app.add_entity(&type_id, w);
    }
    if bg.clicked() {
        if !ui.input(|i| i.modifiers.shift) {
            app.selection.clear();
        }
        app.selected_edge = None;
        app.selected_annotation = None;
    }
    bg.context_menu(|ui| {
        let w = app.camera.to_world(origin, ui.min_rect().min);
        ui.menu_button("Add", |ui| {
            for cat in app.catalog.categories() {
                let types: Vec<(String, String)> = app
                    .catalog
                    .resources
                    .values()
                    .filter(|r| r.resource.category == cat)
                    .map(|r| (r.resource.type_id.clone(), r.resource.display_name.clone()))
                    .collect();
                ui.menu_button(ttg_catalog::load::category_label(&cat), |ui| {
                    for (tid, name) in types {
                        if ui.button(name).clicked() {
                            app.add_at = Some((tid, w));
                            ui.close();
                        }
                    }
                });
            }
        });
        if ui.button("Zoom to fit").clicked() {
            app.fit_requested = true;
            ui.close();
        }
        if ui.button("Tidy layout").clicked() {
            app.tidy(None);
            ui.close();
        }
    });

    // ---- marquee start
    if bg.drag_started_by(egui::PointerButton::Primary) {
        if let Some(p) = pointer {
            app.drag = Drag::Marquee {
                start: app.camera.to_world(origin, p),
                additive: ui.input(|i| i.modifiers.shift),
            };
        }
    }

    // ---- draw order: containers (shallow first), then edges, then nodes
    let mut containers: Vec<Id> = app
        .project
        .containers
        .keys()
        .filter(|id| app.is_visible(id))
        .cloned()
        .collect();
    containers.sort_by_key(|id| (app.project.depth_of(id), id.clone()));
    let nodes: Vec<Id> = app
        .project
        .nodes
        .keys()
        .filter(|id| app.is_visible(id))
        .cloned()
        .collect();

    crate::annotations::draw_groups(app, ui, origin);
    for id in &containers {
        entity_widget(app, ui, origin, id, true);
    }
    draw_edges(app, ui, origin);
    for id in &nodes {
        entity_widget(app, ui, origin, id, false);
    }
    crate::annotations::draw_flows(app, ui, origin);
    if app.flow_from.is_some() {
        painter.text(
            rect.center_top() + Vec2::new(0.0, 14.0),
            Align2::CENTER_TOP,
            "Data flow: click the target resource or group (Esc to cancel)",
            FontId::proportional(13.0),
            Color32::from_rgb(30, 100, 220),
        );
    }

    // ---- connection in progress
    if let Drag::Connect { from } = &app.drag {
        let from = from.clone();
        if let (Some(r), Some(p)) = (app.entity_rect(&from), pointer) {
            let sr = app.camera.rect_to_screen(origin, r);
            let a = Pos2::new(sr.right(), sr.center().y);
            draw_bezier(
                &painter,
                a,
                p,
                Stroke::new(2.0_f32, Color32::from_rgb(60, 60, 60)),
                true,
            );
        }
        if ui.input(|i| i.pointer.any_released()) {
            let target = pointer.and_then(|p| entity_under(app, origin, p, Some(&from)));
            app.drag = Drag::None;
            if let Some(t) = target {
                app.request_edge(from, t);
            }
        }
    }

    // ---- marquee
    if let Drag::Marquee { start, additive } = &app.drag {
        let (start, additive) = (*start, *additive);
        if let Some(p) = pointer {
            let cur = app.camera.to_world(origin, p);
            let wr = Rect::from_two_pos(start, cur);
            let sr = app.camera.rect_to_screen(origin, wr);
            painter.rect(
                sr,
                CornerRadius::ZERO,
                Color32::from_rgba_unmultiplied(66, 133, 244, 30),
                Stroke::new(1.0_f32, Color32::from_rgb(66, 133, 244)),
                StrokeKind::Outside,
            );
            if ui.input(|i| i.pointer.any_released()) {
                if !additive {
                    app.selection.clear();
                }
                let ids: Vec<Id> = app.project.entities().iter().map(|e| e.id.to_string()).collect();
                for id in ids {
                    if !app.is_visible(&id) {
                        continue;
                    }
                    if let Some(r) = app.entity_rect(&id) {
                        if wr.intersects(r) && (wr.contains_rect(r) || app.project.nodes.contains_key(&id)) {
                            app.selection.insert(id);
                        }
                    }
                }
                app.drag = Drag::None;
            }
        } else if ui.input(|i| i.pointer.any_released()) {
            app.drag = Drag::None;
        }
    }

    // ---- reachability overlay
    if app.reach_mode {
        draw_reach(app, ui, origin);
    }

    // ---- zoom label
    let hidden = app.hidden_count();
    painter.text(
        rect.right_bottom() - Vec2::new(8.0, 6.0),
        Align2::RIGHT_BOTTOM,
        if hidden > 0 {
            format!("{hidden} hidden by view · {:.0}%", zoom * 100.0)
        } else {
            format!("{:.0}%", zoom * 100.0)
        },
        FontId::proportional(12.0),
        Color32::from_gray(120),
    );
    if app.project.nodes.is_empty() && app.project.containers.is_empty() {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "Drag a resource from the palette, or right-click to add one.\nScroll to pan, Ctrl+scroll to zoom, drag from a node's port to connect.",
            FontId::proportional(14.0),
            Color32::from_gray(150),
        );
    }
}

fn draw_grid(painter: &egui::Painter, rect: Rect, cam: &crate::camera::Camera) {
    let step = 40.0 * cam.zoom;
    if step < 8.0 {
        return;
    }
    let color = Color32::from_gray(228);
    let ox = (rect.min.x + cam.pan.x).rem_euclid(step);
    let oy = (rect.min.y + cam.pan.y).rem_euclid(step);
    let mut x = rect.min.x + ox;
    while x < rect.max.x {
        painter.line_segment(
            [Pos2::new(x, rect.min.y), Pos2::new(x, rect.max.y)],
            Stroke::new(1.0_f32, color),
        );
        x += step;
    }
    let mut y = rect.min.y + oy;
    while y < rect.max.y {
        painter.line_segment(
            [Pos2::new(rect.min.x, y), Pos2::new(rect.max.x, y)],
            Stroke::new(1.0_f32, color),
        );
        y += step;
    }
}

/// Topmost entity whose screen rect contains `p` (nodes before containers, deeper first).
fn entity_under(app: &TtgApp, origin: Pos2, p: Pos2, exclude: Option<&str>) -> Option<Id> {
    for id in app.project.nodes.keys() {
        if Some(id.as_str()) == exclude || !app.is_visible(id) {
            continue;
        }
        if app
            .camera
            .rect_to_screen(origin, app.entity_rect(id).unwrap())
            .contains(p)
        {
            return Some(id.clone());
        }
    }
    let mut cs: Vec<&Id> = app.project.containers.keys().collect();
    cs.sort_by_key(|id| std::cmp::Reverse(app.project.depth_of(id)));
    for id in cs {
        if Some(id.as_str()) == exclude || !app.is_visible(id) {
            continue;
        }
        if app
            .camera
            .rect_to_screen(origin, app.entity_rect(id).unwrap())
            .contains(p)
        {
            return Some(id.clone());
        }
    }
    None
}

fn entity_widget(app: &mut TtgApp, ui: &mut Ui, origin: Pos2, id: &str, is_container: bool) {
    let Some(wr) = app.entity_rect(id) else { return };
    let sr = app.camera.rect_to_screen(origin, wr);
    let zoom = app.camera.zoom;
    let (name, type_id, manual) = {
        let e = app.project.entity(id).unwrap();
        (e.name.to_string(), e.resource_type.to_string(), e.manual)
    };
    let def = app.catalog.resource(&type_id);
    let (icon, cat) = def
        .map(|d| (d.resource.icon.clone(), d.resource.category.clone()))
        .unwrap_or(("?".into(), String::new()));
    let display = app.type_subtitle(&type_id);
    let concrete = app.concrete_mode();
    let logical = concrete
        && app
            .concrete(&type_id)
            .is_some_and(|c| c.status == ttg_catalog::MappingStatus::Logical);
    let icon_tex = if concrete {
        let provider = app.project.settings.target_provider.clone();
        app.icons.get(ui.ctx(), &provider, &type_id)
    } else {
        None
    };
    let color = category_color(&cat);
    let selected = app.selection.contains(id);
    let mapped = app
        .catalog
        .mapping(&type_id, &app.project.settings.target_provider)
        .is_some();

    // Interaction area: whole node, or the header strip of a container.
    let hit = if is_container {
        Rect::from_min_size(sr.min, Vec2::new(sr.width(), HEADER_H * zoom))
    } else {
        sr
    };
    let resp = ui.interact(hit, ui.id().with(("ent", id)), Sense::click_and_drag());

    // ---- selection & drag
    let shift = ui.input(|i| i.modifiers.shift);
    if resp.clicked() && app.flow_from.is_some() {
        app.finish_flow_to(ttg_core::FlowEnd::Entity {
            entity: id.to_string(),
        });
    } else if resp.clicked() {
        app.selected_annotation = None;
        if shift {
            if !app.selection.remove(id) {
                app.selection.insert(id.to_string());
            }
        } else {
            app.selection.clear();
            app.selection.insert(id.to_string());
        }
        app.selected_edge = None;
    }
    if resp.drag_started_by(egui::PointerButton::Primary) {
        if !app.selection.contains(id) {
            if !shift {
                app.selection.clear();
            }
            app.selection.insert(id.to_string());
        }
        app.selected_edge = None;
        app.selected_annotation = None;
        app.drag = Drag::Move {
            before: app.snapshot(),
            accum: Vec2::ZERO,
        };
    }
    if resp.dragged_by(egui::PointerButton::Primary) {
        let delta = if let Drag::Move { accum, .. } = &mut app.drag {
            *accum += resp.drag_delta() / zoom;
            let dx = accum.x.trunc();
            let dy = accum.y.trunc();
            accum.x -= dx;
            accum.y -= dy;
            Some((dx as i32, dy as i32))
        } else {
            None
        };
        if let Some((dx, dy)) = delta {
            if dx != 0 || dy != 0 {
                let moving: Vec<Id> = moving_set(app).into_iter().collect();
                app.shift_entities(&moving, dx, dy);
            }
        }
    }
    if resp.drag_stopped_by(egui::PointerButton::Primary) {
        if let Drag::Move { .. } = &app.drag {
            let moving = moving_set(app);
            // Re-parent the top-level moved entities to whatever container they landed in.
            let tops: Vec<Id> = moving
                .iter()
                .filter(|m| app.project.parent_of(m).is_none_or(|p| !moving.contains(p)))
                .cloned()
                .collect();
            for m in tops {
                let center = app.entity_rect(&m).unwrap().center();
                let probe = if app.project.containers.contains_key(&m) {
                    app.entity_rect(&m).unwrap().min + Vec2::new(10.0, 10.0)
                } else {
                    center
                };
                let candidate = app.container_at(probe, &moving);
                let allowed = candidate.clone().filter(|c| {
                    let ct = &app.project.containers[c].container_type;
                    app.catalog
                        .resource(app.project.entity(&m).unwrap().resource_type)
                        .is_some_and(|d| d.resource.allowed_parents.iter().any(|a| a == ct))
                });
                match (allowed, candidate) {
                    (Some(c), _) => {
                        app.project.set_parent(&m, Some(&c));
                    }
                    (None, Some(_)) => {
                        // Dropped inside a container that cannot hold it: keep old parent
                        // (diagnostics will explain if that parent is also wrong).
                    }
                    (None, None) => {
                        app.project.set_parent(&m, None);
                    }
                }
            }
            if let Drag::Move { before, .. } = std::mem::replace(&mut app.drag, Drag::None) {
                app.finish(before);
            }
        }
    }
    if concrete {
        resp.clone().on_hover_text(app.concrete_tooltip(id));
    }
    resp.context_menu(|ui| {
        if ui
            .checkbox(
                &mut app.project.entity(id).map(|e| e.manual).unwrap_or(false).clone(),
                "External / manual",
            )
            .clicked()
        {
            let before = app.snapshot();
            if let Some(n) = app.project.nodes.get_mut(id) {
                n.manual = !n.manual;
            } else if let Some(c) = app.project.containers.get_mut(id) {
                c.manual = !c.manual;
            }
            app.finish(before);
            ui.close();
        }
        if is_container && ui.button("Tidy contents").clicked() {
            app.tidy(Some(id));
            ui.close();
        }
        if app.active_view.is_some() && ui.button("Data flow from here").clicked() {
            app.flow_from = Some(ttg_core::FlowEnd::Entity {
                entity: id.to_string(),
            });
            app.status = "Click the target of the data flow (Esc to cancel)".into();
            ui.close();
        }
        if ui.button("Delete").clicked() {
            app.selection.clear();
            app.selection.insert(id.to_string());
            app.delete_selection();
            ui.close();
        }
    });

    // ---- paint
    let painter = ui.painter_at(app.canvas_rect);
    let cr = CornerRadius::same((6.0 * zoom).clamp(2.0, 8.0) as u8);
    let stroke_w = if selected { 2.5_f32 } else { 1.2_f32 };
    let stroke_color = if selected {
        Color32::from_rgb(30, 100, 220)
    } else if manual || !mapped || logical {
        Color32::from_gray(150)
    } else {
        color
    };
    let font_name = FontId::proportional((13.5 * zoom).max(6.0));
    let font_small = FontId::proportional((11.0 * zoom).max(5.0));

    if is_container {
        let fill = if manual {
            Color32::from_rgba_unmultiplied(200, 200, 200, 40)
        } else {
            Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 18)
        };
        painter.rect(
            sr,
            cr,
            fill,
            Stroke::new(stroke_w, stroke_color),
            StrokeKind::Inside,
        );
        if manual || !mapped {
            dashed_rect(&painter, sr, stroke_color);
        }
        let header = Rect::from_min_size(sr.min, Vec2::new(sr.width(), HEADER_H * zoom));
        painter.rect_filled(
            header,
            CornerRadius {
                nw: cr.nw,
                ne: cr.ne,
                sw: 0,
                se: 0,
            },
            Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 40),
        );
        painter.text(
            header.left_center() + Vec2::new(10.0 * zoom, 0.0),
            Align2::LEFT_CENTER,
            format!("{icon}  {name}"),
            font_name.clone(),
            Color32::from_gray(30),
        );
        if app.layer_tag(id).is_none() {
            painter.text(
                header.right_center() - Vec2::new(28.0 * zoom, 0.0),
                Align2::RIGHT_CENTER,
                display,
                font_small.clone(),
                Color32::from_gray(110),
            );
        }
        // resize handle
        let handle = Rect::from_min_size(sr.max - Vec2::splat(14.0 * zoom), Vec2::splat(14.0 * zoom));
        let hr = ui.interact(handle, ui.id().with(("resize", id)), Sense::drag());
        painter.line_segment(
            [
                handle.left_bottom() + Vec2::new(3.0, -3.0),
                handle.right_top() + Vec2::new(-3.0, 3.0),
            ],
            Stroke::new(1.5_f32, Color32::from_gray(150)),
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
                let id = id.to_string();
                app.with_layout(|p| {
                    if let Some(c) = p.containers.get_mut(&id) {
                        c.size.w = (c.size.w + dx as i32).max(MIN_CONTAINER.x as i32);
                        c.size.h = (c.size.h + dy as i32).max(MIN_CONTAINER.y as i32);
                    }
                });
            }
        }
        if hr.drag_stopped() {
            if let Drag::Resize { before, .. } = std::mem::replace(&mut app.drag, Drag::None) {
                app.finish(before);
            }
        }
    } else {
        let fill = if manual {
            Color32::from_gray(240)
        } else {
            Color32::WHITE
        };
        painter.rect(
            sr,
            cr,
            fill,
            Stroke::new(stroke_w, stroke_color),
            StrokeKind::Inside,
        );
        if manual || !mapped {
            dashed_rect(&painter, sr, stroke_color);
        }
        // icon block
        let icon_rect = Rect::from_min_size(
            sr.min + Vec2::splat(6.0 * zoom),
            Vec2::new(44.0 * zoom, sr.height() - 12.0 * zoom),
        );
        match &icon_tex {
            Some(tex) => {
                painter.rect_filled(icon_rect, cr, Color32::from_gray(245));
                let side = icon_rect.width().min(icon_rect.height());
                let img = Rect::from_center_size(icon_rect.center(), Vec2::splat(side));
                painter.image(
                    tex.id(),
                    img,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    if manual {
                        Color32::from_gray(160)
                    } else {
                        Color32::WHITE
                    },
                );
            }
            None => {
                painter.rect_filled(
                    icon_rect,
                    cr,
                    if manual || logical {
                        Color32::from_gray(180)
                    } else {
                        color
                    },
                );
                painter.text(
                    icon_rect.center(),
                    Align2::CENTER_CENTER,
                    icon,
                    FontId::proportional((12.0 * zoom).max(5.0)),
                    Color32::WHITE,
                );
            }
        }
        let tx = icon_rect.right() + 8.0 * zoom;
        painter.text(
            Pos2::new(tx, sr.min.y + 20.0 * zoom),
            Align2::LEFT_CENTER,
            truncate(&name, (sr.right() - tx - 14.0 * zoom) / (7.0 * zoom)),
            font_name,
            Color32::from_gray(25),
        );
        painter.text(
            Pos2::new(tx, sr.min.y + 42.0 * zoom),
            Align2::LEFT_CENTER,
            display,
            font_small,
            Color32::from_gray(120),
        );
    }

    // ---- provider layer: tag entities that are not on every provider; dim the ones
    // that are off the displayed layer (concrete mode only).
    if let Some(tag) = app.layer_tag(id) {
        let font = FontId::proportional((9.5 * zoom).max(6.0));
        let galley = painter.layout_no_wrap(tag, font, Color32::WHITE);
        let size = galley.size() + Vec2::new(8.0, 3.0);
        let pos = if is_container {
            Pos2::new(
                sr.right() - size.x - 30.0 * zoom,
                sr.top() + (HEADER_H * zoom - size.y) / 2.0,
            )
        } else {
            Pos2::new(sr.right() - size.x - 4.0, sr.top() - size.y / 2.0)
        };
        let pill = Rect::from_min_size(pos, size);
        painter.rect_filled(pill, CornerRadius::same(6), Color32::from_rgb(90, 100, 140));
        painter.galley(pill.min + Vec2::new(4.0, 1.5), galley, Color32::WHITE);
    }
    if !app.on_layer(id) {
        painter.rect_filled(
            if is_container {
                Rect::from_min_size(sr.min, Vec2::new(sr.width(), HEADER_H * zoom))
            } else {
                sr
            },
            cr,
            Color32::from_rgba_unmultiplied(246, 247, 249, 175),
        );
        let _ = resp.clone().on_hover_text(format!(
            "Not part of the {} design: left out of that export.",
            app.provider_display_name()
        ));
    }

    // ---- agent flash: a fading ring on entities the MCP agent just touched
    #[cfg(feature = "mcp")]
    if let Some(f) = app.agent_flash(id) {
        painter.rect_stroke(
            sr.expand(4.0 + 6.0 * (1.0 - f)),
            CornerRadius::same(8),
            Stroke::new(
                3.0_f32,
                Color32::from_rgba_unmultiplied(255, 140, 0, (220.0 * f) as u8),
            ),
            StrokeKind::Outside,
        );
    }

    // ---- resize handle on nodes (containers have their own above)
    if !is_container {
        let handle = Rect::from_min_size(sr.max - Vec2::splat(12.0 * zoom), Vec2::splat(12.0 * zoom));
        let hr = ui.interact(handle, ui.id().with(("nresize", id)), Sense::drag());
        if hr.hovered() || matches!(app.drag, Drag::Resize { .. }) {
            painter.line_segment(
                [
                    handle.left_bottom() + Vec2::new(3.0, -3.0),
                    handle.right_top() + Vec2::new(-3.0, 3.0),
                ],
                Stroke::new(1.5_f32, Color32::from_gray(150)),
            );
        }
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
                let id = id.to_string();
                app.with_layout(|p| {
                    if let Some(n) = p.nodes.get_mut(&id) {
                        let mut sz = n.size.unwrap_or(ttg_core::Size {
                            w: NODE_W as i32,
                            h: NODE_H as i32,
                        });
                        sz.w = (sz.w + dx as i32).max(120);
                        sz.h = (sz.h + dy as i32).max(48);
                        n.size = Some(sz);
                    }
                });
            }
        }
        if hr.drag_stopped() {
            if let Drag::Resize { before, .. } = std::mem::replace(&mut app.drag, Drag::None) {
                app.finish(before);
            }
        }
        hr.on_hover_cursor(egui::CursorIcon::ResizeNwSe);
    }

    // ---- port (connection handle) on the right edge
    let port_center = Pos2::new(
        sr.right(),
        if is_container {
            sr.min.y + HEADER_H * zoom / 2.0
        } else {
            sr.center().y
        },
    );
    let port_rect = Rect::from_center_size(port_center, Vec2::splat(PORT_R * 2.0 * zoom.max(0.6) + 6.0));
    let pr = ui.interact(port_rect, ui.id().with(("port", id)), Sense::drag());
    let port_color = if pr.hovered() || matches!(&app.drag, Drag::Connect { from } if from == id) {
        Color32::from_rgb(30, 100, 220)
    } else {
        Color32::from_gray(140)
    };
    painter.circle_filled(port_center, PORT_R * zoom.max(0.6), Color32::WHITE);
    painter.circle_stroke(
        port_center,
        PORT_R * zoom.max(0.6),
        Stroke::new(1.5_f32, port_color),
    );
    if pr.drag_started() {
        app.drag = Drag::Connect { from: id.to_string() };
    }
    pr.on_hover_text("Drag to connect to another resource");

    // ---- badge
    if let Some(d) = app.entity_badge(id).cloned() {
        let (glyph, bcolor) = match (d.severity, d.code) {
            (Severity::Error, _) => ("!", Color32::from_rgb(220, 50, 50)),
            (Severity::Warning, Code::Unmapped) => ("×", Color32::from_rgb(200, 90, 30)),
            (Severity::Warning, _) => ("!", Color32::from_rgb(235, 160, 30)),
            (Severity::Info, Code::Manual) => ("M", Color32::from_rgb(90, 110, 200)),
            _ => ("i", Color32::from_gray(150)),
        };
        let c = Pos2::new(sr.right() - 12.0 * zoom, sr.min.y + 12.0 * zoom);
        let r = 8.0 * zoom.max(0.7);
        painter.circle_filled(c, r, bcolor);
        painter.text(
            c,
            Align2::CENTER_CENTER,
            glyph,
            FontId::proportional((11.0 * zoom).max(6.0)),
            Color32::WHITE,
        );
        let badge_rect = Rect::from_center_size(c, Vec2::splat(r * 2.0));
        ui.interact(badge_rect, ui.id().with(("badge", id)), Sense::hover())
            .on_hover_text(d.message.clone());
    }
}

/// Selected entities plus every descendant of a selected container, each once.
fn moving_set(app: &TtgApp) -> BTreeSet<Id> {
    let mut set: BTreeSet<Id> = app.selection.clone();
    for id in &app.selection {
        if app.project.containers.contains_key(id) {
            set.extend(app.project.descendants_of(id));
        }
    }
    set
}

fn truncate(s: &str, max_chars: f32) -> String {
    let max = max_chars.max(3.0) as usize;
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

fn dashed_rect(painter: &egui::Painter, r: Rect, color: Color32) {
    let stroke = Stroke::new(1.0_f32, color);
    let pts = [
        r.left_top(),
        r.right_top(),
        r.right_bottom(),
        r.left_bottom(),
        r.left_top(),
    ];
    for w in pts.windows(2) {
        painter.add(egui::Shape::dashed_line(&[w[0], w[1]], stroke, 6.0, 4.0));
    }
}

fn draw_bezier(painter: &egui::Painter, a: Pos2, b: Pos2, stroke: Stroke, arrow: bool) {
    let dx = ((b.x - a.x).abs() * 0.45).max(40.0);
    let c1 = Pos2::new(a.x + dx, a.y);
    let c2 = Pos2::new(b.x - dx, b.y);
    painter.add(CubicBezierShape::from_points_stroke(
        [a, c1, c2, b],
        false,
        Color32::TRANSPARENT,
        stroke,
    ));
    if arrow {
        let dir = (b - c2).normalized();
        let n = Vec2::new(-dir.y, dir.x);
        let tip = b;
        let l = 9.0;
        painter.add(egui::Shape::convex_polygon(
            vec![tip, tip - dir * l + n * l * 0.5, tip - dir * l - n * l * 0.5],
            stroke.color,
            Stroke::NONE,
        ));
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    fn normal(self) -> Vec2 {
        match self {
            Side::Left => Vec2::new(-1.0, 0.0),
            Side::Right => Vec2::new(1.0, 0.0),
            Side::Top => Vec2::new(0.0, -1.0),
            Side::Bottom => Vec2::new(0.0, 1.0),
        }
    }
    fn horizontal(self) -> bool {
        matches!(self, Side::Left | Side::Right)
    }
}

/// The rectangle edges attach to: the whole node, or a container's header strip.
fn anchor_rect(app: &TtgApp, origin: Pos2, id: &str) -> Option<Rect> {
    let r = app.camera.rect_to_screen(origin, app.entity_rect(id)?);
    if app.project.containers.contains_key(id) {
        Some(Rect::from_min_size(
            r.min,
            Vec2::new(r.width(), HEADER_H * app.camera.zoom),
        ))
    } else {
        Some(r)
    }
}

/// Pick the pair of sides that face each other. Horizontal sides win unless the target is
/// clearly above or below (more vertical than horizontal displacement by a margin).
fn choose_sides(sr: Rect, tr: Rect) -> (Side, Side) {
    let d = tr.center() - sr.center();
    if d.y.abs() > d.x.abs() * 1.3 && (tr.min.y > sr.max.y || tr.max.y < sr.min.y) {
        if d.y >= 0.0 {
            (Side::Bottom, Side::Top)
        } else {
            (Side::Top, Side::Bottom)
        }
    } else if d.x >= 0.0 {
        (Side::Right, Side::Left)
    } else {
        (Side::Left, Side::Right)
    }
}

/// Point on `side` of `r`, shifted along the side so several edges fan out.
fn side_point(r: Rect, side: Side, index: usize, count: usize, zoom: f32) -> Pos2 {
    let along = if side.horizontal() { r.height() } else { r.width() };
    let spread = (16.0 * zoom).min(along / (count as f32 + 1.0));
    let offset = (index as f32 - (count as f32 - 1.0) / 2.0) * spread;
    match side {
        Side::Left => Pos2::new(r.left(), r.center().y + offset),
        Side::Right => Pos2::new(r.right(), r.center().y + offset),
        Side::Top => Pos2::new(r.center().x + offset, r.top()),
        Side::Bottom => Pos2::new(r.center().x + offset, r.bottom()),
    }
}

/// Polyline for an orthogonal edge between two anchors.
fn orthogonal_path(a: Pos2, sa: Side, b: Pos2, sb: Side) -> Vec<Pos2> {
    match (sa.horizontal(), sb.horizontal()) {
        (true, true) => {
            let mx = (a.x + b.x) / 2.0;
            vec![a, Pos2::new(mx, a.y), Pos2::new(mx, b.y), b]
        }
        (false, false) => {
            let my = (a.y + b.y) / 2.0;
            vec![a, Pos2::new(a.x, my), Pos2::new(b.x, my), b]
        }
        (true, false) => vec![a, Pos2::new(b.x, a.y), b],
        (false, true) => vec![a, Pos2::new(a.x, b.y), b],
    }
}

/// Does the axis-aligned segment `p`-`q` pass through the interior of `r`?
fn seg_crosses(p: Pos2, q: Pos2, r: Rect) -> bool {
    if (p.x - q.x).abs() < 0.5 {
        let (y0, y1) = (p.y.min(q.y), p.y.max(q.y));
        p.x > r.left() && p.x < r.right() && y1 > r.top() && y0 < r.bottom()
    } else {
        let (x0, x1) = (p.x.min(q.x), p.x.max(q.x));
        p.y > r.top() && p.y < r.bottom() && x1 > r.left() && x0 < r.right()
    }
}

/// Orthogonal polyline from `a` (leaving side `sa`) to `b` (arriving at side `sb`) that
/// avoids `obstacles` where a detour exists. Candidate paths are the plain route plus
/// Z- and U-shapes whose free segment runs just outside each nearby obstacle; the one
/// with the fewest crossings, then bends, then length wins.
fn routed_path(a: Pos2, sa: Side, b: Pos2, sb: Side, obstacles: &[Rect], zoom: f32) -> Vec<Pos2> {
    let plain = orthogonal_path(a, sa, b, sb);
    if obstacles
        .iter()
        .all(|r| !plain.windows(2).any(|w| seg_crosses(w[0], w[1], *r)))
    {
        return plain;
    }
    let m = 16.0 * zoom.clamp(0.5, 1.5);
    let bbox = Rect::from_two_pos(a, b).expand(4.0 * m);
    let near: Vec<Rect> = obstacles.iter().copied().filter(|r| r.intersects(bbox)).collect();
    let mut xs = vec![(a.x + b.x) / 2.0];
    let mut ys = vec![(a.y + b.y) / 2.0];
    for r in &near {
        xs.push(r.left() - m);
        xs.push(r.right() + m);
        ys.push(r.top() - m);
        ys.push(r.bottom() + m);
    }
    let ax = a + sa.normal() * m;
    let bx = b + sb.normal() * m;
    let mut cands: Vec<Vec<Pos2>> = vec![plain];
    match (sa.horizontal(), sb.horizontal()) {
        (true, true) => {
            for &x in &xs {
                cands.push(vec![a, Pos2::new(x, a.y), Pos2::new(x, b.y), b]);
            }
            for &y in &ys {
                cands.push(vec![
                    a,
                    Pos2::new(ax.x, a.y),
                    Pos2::new(ax.x, y),
                    Pos2::new(bx.x, y),
                    Pos2::new(bx.x, b.y),
                    b,
                ]);
            }
        }
        (false, false) => {
            for &y in &ys {
                cands.push(vec![a, Pos2::new(a.x, y), Pos2::new(b.x, y), b]);
            }
            for &x in &xs {
                cands.push(vec![
                    a,
                    Pos2::new(a.x, ax.y),
                    Pos2::new(x, ax.y),
                    Pos2::new(x, bx.y),
                    Pos2::new(b.x, bx.y),
                    b,
                ]);
            }
        }
        (true, false) => {
            for &x in &xs {
                cands.push(vec![
                    a,
                    Pos2::new(x, a.y),
                    Pos2::new(x, bx.y),
                    Pos2::new(b.x, bx.y),
                    b,
                ]);
            }
            for &y in &ys {
                cands.push(vec![
                    a,
                    Pos2::new(ax.x, a.y),
                    Pos2::new(ax.x, y),
                    Pos2::new(b.x, y),
                    b,
                ]);
            }
        }
        (false, true) => {
            for &y in &ys {
                cands.push(vec![
                    a,
                    Pos2::new(a.x, y),
                    Pos2::new(bx.x, y),
                    Pos2::new(bx.x, b.y),
                    b,
                ]);
            }
            for &x in &xs {
                cands.push(vec![
                    a,
                    Pos2::new(a.x, ax.y),
                    Pos2::new(x, ax.y),
                    Pos2::new(x, b.y),
                    b,
                ]);
            }
        }
    }
    let score = |pts: &[Pos2]| -> f32 {
        let crossings = pts
            .windows(2)
            .map(|w| obstacles.iter().filter(|r| seg_crosses(w[0], w[1], **r)).count())
            .sum::<usize>() as f32;
        // Leaving against the side's normal or arriving from behind runs through the
        // endpoint's own node.
        let first = pts[1] - pts[0];
        let last = pts[pts.len() - 1] - pts[pts.len() - 2];
        let backwards = (first.dot(sa.normal()) < -0.5) as u8 + (last.dot(sb.normal()) > 0.5) as u8;
        let len: f32 = pts.windows(2).map(|w| (w[1] - w[0]).length()).sum();
        crossings * 1000.0 + backwards as f32 * 500.0 + (pts.len() as f32 - 2.0) * 25.0 + len * 0.1
    };
    cands
        .into_iter()
        .min_by(|p, q| {
            score(p)
                .partial_cmp(&score(q))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap()
}

fn side_from_name(s: &str) -> Option<Side> {
    match s {
        "left" => Some(Side::Left),
        "right" => Some(Side::Right),
        "top" => Some(Side::Top),
        "bottom" => Some(Side::Bottom),
        _ => None,
    }
}

/// The side of `r` closest to `p` (for container terminals, which sit on the wall
/// nearest the other endpoint whether that endpoint is inside or outside).
fn nearest_side(r: Rect, p: Pos2) -> Side {
    let dl = (p.x - r.left()).abs();
    let dr = (p.x - r.right()).abs();
    let dt = (p.y - r.top()).abs();
    let db = (p.y - r.bottom()).abs();
    let m = dl.min(dr).min(dt).min(db);
    if m == dl {
        Side::Left
    } else if m == dr {
        Side::Right
    } else if m == dt {
        Side::Top
    } else {
        Side::Bottom
    }
}

/// Point on `side` of `r` at a manual offset (-100..100 along the side).
fn side_point_at(r: Rect, side: Side, offset_pct: i32) -> Pos2 {
    let f = (offset_pct as f32 / 100.0).clamp(-1.0, 1.0);
    let margin = 10.0;
    match side {
        Side::Left | Side::Right => {
            let half = (r.height() / 2.0 - margin).max(0.0);
            let x = if side == Side::Left { r.left() } else { r.right() };
            Pos2::new(x, r.center().y + f * half)
        }
        Side::Top | Side::Bottom => {
            let half = (r.width() / 2.0 - margin).max(0.0);
            let y = if side == Side::Top { r.top() } else { r.bottom() };
            Pos2::new(r.center().x + f * half, y)
        }
    }
}

/// Point on a container wall nearest to `other` (projection, clamped to the wall).
fn wall_point(r: Rect, side: Side, other: Pos2) -> Pos2 {
    let margin = 14.0;
    match side {
        Side::Left => Pos2::new(r.left(), other.y.clamp(r.top() + margin, r.bottom() - margin)),
        Side::Right => Pos2::new(r.right(), other.y.clamp(r.top() + margin, r.bottom() - margin)),
        Side::Top => Pos2::new(other.x.clamp(r.left() + margin, r.right() - margin), r.top()),
        Side::Bottom => Pos2::new(other.x.clamp(r.left() + margin, r.right() - margin), r.bottom()),
    }
}

/// One end of an edge after anchor resolution.
struct EdgeEnd {
    rect: Rect,
    side: Side,
    /// Manual offset along the side, if the user pinned it.
    manual: Option<i32>,
    is_container: bool,
}

fn draw_edges(app: &mut TtgApp, ui: &mut Ui, origin: Pos2) {
    let painter = ui.painter_at(app.canvas_rect);
    let zoom = app.camera.zoom;
    let edges = app.project.edges.clone();

    // Pass 1: resolve both ends of every visible edge and count automatic attachments
    // per (entity, side) so they can fan out.
    let mut ends: Vec<Option<(EdgeEnd, EdgeEnd)>> = Vec::with_capacity(edges.len());
    let mut per_side: std::collections::HashMap<(String, Side), usize> = Default::default();
    for e in &edges {
        if !app.edge_visible(e) || ttg_codegen::diagnostics::is_redundant_edge(&app.project, &app.catalog, e)
        {
            ends.push(None);
            continue;
        }
        let (Some(sr), Some(tr)) = (
            app.entity_rect(&e.source)
                .map(|r| app.camera.rect_to_screen(origin, r)),
            app.entity_rect(&e.target)
                .map(|r| app.camera.rect_to_screen(origin, r)),
        ) else {
            ends.push(None);
            continue;
        };
        let sc = app.project.containers.contains_key(&e.source);
        let tc = app.project.containers.contains_key(&e.target);
        // Automatic sides: nodes face each other; containers use the wall nearest the
        // other endpoint (which may be inside them).
        let (auto_sa, auto_sb) = choose_sides(
            if sc {
                Rect::from_center_size(sr.center(), Vec2::splat(1.0))
            } else {
                sr
            },
            if tc {
                Rect::from_center_size(tr.center(), Vec2::splat(1.0))
            } else {
                tr
            },
        );
        let auto_sa = if sc {
            nearest_side(sr, tr.center())
        } else {
            auto_sa
        };
        let auto_sb = if tc {
            nearest_side(tr, sr.center())
        } else {
            auto_sb
        };
        let (ls, lt) = match &e.layout {
            Some(l) => (Some(&l.source), Some(&l.target)),
            None => (None, None),
        };
        let sa = ls
            .and_then(|a| a.side.as_deref())
            .and_then(side_from_name)
            .unwrap_or(auto_sa);
        let sb = lt
            .and_then(|a| a.side.as_deref())
            .and_then(side_from_name)
            .unwrap_or(auto_sb);
        let ms = ls.filter(|a| a.side.is_some() || a.offset != 0).map(|a| a.offset);
        let mt = lt.filter(|a| a.side.is_some() || a.offset != 0).map(|a| a.offset);
        if ms.is_none() && !sc {
            *per_side.entry((e.source.clone(), sa)).or_default() += 1;
        }
        if mt.is_none() && !tc {
            *per_side.entry((e.target.clone(), sb)).or_default() += 1;
        }
        ends.push(Some((
            EdgeEnd {
                rect: sr,
                side: sa,
                manual: ms,
                is_container: sc,
            },
            EdgeEnd {
                rect: tr,
                side: sb,
                manual: mt,
                is_container: tc,
            },
        )));
    }
    let mut used: std::collections::HashMap<(String, Side), usize> = Default::default();
    // Obstacles for routed edges: every visible node. Containers are not obstacles (an
    // edge to something inside has to cross the wall).
    let obstacles: Vec<(String, Rect)> = if app.avoid_obstacles {
        app.project
            .nodes
            .keys()
            .filter(|id| app.is_visible(id))
            .filter_map(|id| {
                app.entity_rect(id)
                    .map(|r| (id.clone(), app.camera.rect_to_screen(origin, r)))
            })
            .collect()
    } else {
        Vec::new()
    };

    let resolve = |end: &EdgeEnd,
                   id: &str,
                   other: Pos2,
                   used: &mut std::collections::HashMap<(String, Side), usize>|
     -> Pos2 {
        if let Some(off) = end.manual {
            return side_point_at(end.rect, end.side, off);
        }
        if end.is_container {
            return wall_point(end.rect, end.side, other);
        }
        let n = per_side.get(&(id.to_string(), end.side)).copied().unwrap_or(1);
        let i = used.entry((id.to_string(), end.side)).or_default();
        let p = side_point(end.rect, end.side, *i, n, zoom);
        *i += 1;
        p
    };

    for (i, e) in edges.iter().enumerate() {
        let Some((se, te)) = &ends[i] else { continue };
        // Resolve container ends against the other end's centre; node ends fan out.
        let a = resolve(se, &e.source, te.rect.center(), &mut used);
        let b = resolve(te, &e.target, a, &mut used);
        let (sa, sb) = (se.side, te.side);

        let selected = app.selected_edge == Some(i);
        let off_layer = app.layer.as_ref().is_some_and(|l| {
            !l.contains(&e.source)
                || !l.contains(&e.target)
                || (!e.providers.is_empty()
                    && !e
                        .providers
                        .iter()
                        .any(|p| p == &app.project.settings.target_provider))
        });
        let color = {
            let c = relation_color(e.relation);
            if off_layer {
                Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 60)
            } else {
                c
            }
        };
        let stroke = Stroke::new(
            if selected { 3.0_f32 } else { 1.8_f32 },
            if selected {
                Color32::from_rgb(30, 100, 220)
            } else {
                color
            },
        );

        let (mid, dir) = match app.edge_style {
            crate::app::EdgeStyle::Curved => {
                let dist = (b - a).length();
                let k = (dist * 0.45).max(40.0 * zoom.max(0.5));
                let c1 = a + sa.normal() * k;
                let c2 = b + sb.normal() * k;
                painter.add(CubicBezierShape::from_points_stroke(
                    [a, c1, c2, b],
                    false,
                    Color32::TRANSPARENT,
                    stroke,
                ));
                (bezier_point([a, c1, c2, b], 0.5), (b - c2).normalized())
            }
            crate::app::EdgeStyle::Orthogonal => {
                let pts = if app.avoid_obstacles {
                    let obs: Vec<Rect> = obstacles
                        .iter()
                        .filter(|(id, _)| id != &e.source && id != &e.target)
                        .map(|(_, r)| *r)
                        .collect();
                    routed_path(a, sa, b, sb, &obs, zoom)
                } else {
                    orthogonal_path(a, sa, b, sb)
                };
                painter.add(egui::Shape::line(pts.clone(), stroke));
                let (mut best, mut best_len) = (pts[0], -1.0);
                for w in pts.windows(2) {
                    let l = (w[1] - w[0]).length();
                    if l > best_len {
                        best_len = l;
                        best = w[0] + (w[1] - w[0]) * 0.5;
                    }
                }
                let n = pts.len();
                (best, (pts[n - 1] - pts[n - 2]).normalized())
            }
        };
        // Container terminals: a "via" glyph on the wall instead of an arrowhead.
        let term_r = 5.0 * zoom.max(0.6);
        if te.is_container {
            painter.circle_filled(b, term_r, Color32::WHITE);
            painter.circle_stroke(b, term_r, Stroke::new(1.5_f32, stroke.color));
            painter.circle_filled(b, term_r * 0.4, stroke.color);
        } else {
            let n = Vec2::new(-dir.y, dir.x);
            let l = 9.0;
            painter.add(egui::Shape::convex_polygon(
                vec![b, b - dir * l + n * l * 0.5, b - dir * l - n * l * 0.5],
                stroke.color,
                Stroke::NONE,
            ));
        }
        if se.is_container {
            painter.circle_filled(a, term_r, Color32::WHITE);
            painter.circle_stroke(a, term_r, Stroke::new(1.5_f32, stroke.color));
            painter.circle_filled(a, term_r * 0.4, stroke.color);
        }

        let label = short_label(e.relation);
        let font = FontId::proportional((10.5 * zoom).max(6.0));
        let galley = painter.layout_no_wrap(label.to_string(), font.clone(), color);
        let lr = Rect::from_center_size(mid, galley.size() + Vec2::splat(6.0));
        let resp = ui.interact(lr, ui.id().with(("edge", i)), Sense::click());
        painter.rect(
            lr,
            CornerRadius::same(3),
            if selected {
                Color32::from_rgb(225, 235, 255)
            } else {
                Color32::from_rgba_unmultiplied(255, 255, 255, 230)
            },
            Stroke::new(
                1.0_f32,
                if selected {
                    Color32::from_rgb(30, 100, 220)
                } else {
                    color
                },
            ),
            StrokeKind::Inside,
        );
        painter.galley(lr.min + Vec2::splat(3.0), galley, color);
        if resp.clicked() {
            app.selected_edge = Some(i);
            app.selection.clear();
        }
        resp.on_hover_text(format!(
            "{} → {}: {} (click to select, Delete to remove)",
            app.project.entity(&e.source).map(|x| x.name).unwrap_or(""),
            app.project.entity(&e.target).map(|x| x.name).unwrap_or(""),
            e.relation.display_name()
        ));
    }
}

fn short_label(r: Relation) -> &'static str {
    match r {
        Relation::NetworkMembership => "network",
        Relation::AttributeReference => "uses",
        Relation::IamBinding => "assumes",
        Relation::Attachment => "attached",
        Relation::SendsTo => "sends to",
        Relation::Reads => "reads",
        Relation::LogsTo => "logs to",
        Relation::DependsOn => "depends on",
    }
}

fn bezier_point(p: [Pos2; 4], t: f32) -> Pos2 {
    let u = 1.0 - t;
    let a = p[0].to_vec2() * (u * u * u);
    let b = p[1].to_vec2() * (3.0 * u * u * t);
    let c = p[2].to_vec2() * (3.0 * u * t * t);
    let d = p[3].to_vec2() * (t * t * t);
    (a + b + c + d).to_pos2()
}

// ---------------------------------------------------------------------------------------
// Reachability overlay

fn status_color(s: ttg_codegen::reach::Status) -> Color32 {
    match s {
        ttg_codegen::reach::Status::Ok => Color32::from_rgb(40, 160, 80),
        ttg_codegen::reach::Status::Blocked => Color32::from_rgb(210, 50, 50),
        ttg_codegen::reach::Status::Unknown => Color32::from_rgb(225, 150, 30),
    }
}

/// Centre of an entity on screen (container: its header).
fn anchor_center(app: &TtgApp, origin: Pos2, id: &str) -> Option<Pos2> {
    anchor_rect(app, origin, id).map(|r| r.center())
}

/// Arrowhead at `to`, pointing along `from -> to`.
fn arrowhead(painter: &egui::Painter, from: Pos2, to: Pos2, color: Color32, size: f32) {
    let d = to - from;
    if d.length_sq() < 1.0 {
        return;
    }
    let dir = d.normalized();
    let n = Vec2::new(-dir.y, dir.x);
    painter.add(egui::Shape::convex_polygon(
        vec![
            to,
            to - dir * size + n * size * 0.55,
            to - dir * size - n * size * 0.55,
        ],
        color,
        Stroke::NONE,
    ));
}

/// A polyline with an arrowhead on its last segment; dashed when `dashed`.
fn draw_flow(painter: &egui::Painter, pts: &[Pos2], stroke: Stroke, dashed: bool, arrow: f32) {
    if pts.len() < 2 {
        return;
    }
    if dashed {
        for w in pts.windows(2) {
            painter.add(egui::Shape::dashed_line(&[w[0], w[1]], stroke, 8.0, 6.0));
        }
    } else {
        painter.add(egui::Shape::line(pts.to_vec(), stroke));
    }
    for c in pts.iter().skip(1).take(pts.len().saturating_sub(2)) {
        painter.circle_filled(*c, 4.0, stroke.color);
    }
    let n = pts.len();
    arrowhead(painter, pts[n - 2], pts[n - 1], stroke.color, arrow);
}

fn draw_reach(app: &mut TtgApp, ui: &mut Ui, origin: Pos2) {
    use ttg_codegen::reach::{Egress, Status};
    let painter = ui.painter_at(app.canvas_rect);
    let zoom = app.camera.zoom;
    let reach = app.reach().clone();
    let single = if app.selection.len() == 1 {
        app.selection.iter().next().cloned()
    } else {
        None
    };
    let line_w = (2.5 * zoom).clamp(1.5, 4.0);
    let arrow = (11.0 * zoom).clamp(7.0, 14.0);
    let green = Color32::from_rgb(40, 160, 80);

    // Dim everything so the overlay stands out.
    painter.rect_filled(
        app.canvas_rect,
        CornerRadius::ZERO,
        Color32::from_rgba_unmultiplied(246, 247, 249, 120),
    );

    // Listening-port tags on every networked / exposed resource.
    for id in reach.posture.keys() {
        if !app.is_visible(id) {
            continue;
        }
        let Some(port) = app
            .project
            .entity(id)
            .and_then(|e| ttg_codegen::reach::listening_port(&e))
        else {
            continue;
        };
        let Some(r) = anchor_rect(app, origin, id) else {
            continue;
        };
        let font = FontId::proportional((10.0 * zoom).max(6.0));
        let text = format!(":{port}");
        let galley = painter.layout_no_wrap(text, font, Color32::WHITE);
        let size = galley.size() + Vec2::new(8.0, 3.0);
        let pill = Rect::from_min_size(Pos2::new(r.right() - size.x - 4.0, r.top() - size.y / 2.0), size);
        painter.rect_filled(pill, CornerRadius::same(6), Color32::from_gray(80));
        painter.galley(pill.min + Vec2::new(4.0, 1.5), galley, Color32::WHITE);
    }

    // Tint + status glyph + tooltip on an endpoint of a path.
    let mark = |ui: &mut Ui, painter: &egui::Painter, r: Rect, status: Status, key: &str, tip: String| {
        let color = status_color(status);
        painter.rect_filled(
            r,
            CornerRadius::same(6),
            Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 55),
        );
        painter.rect_stroke(
            r,
            CornerRadius::same(6),
            Stroke::new(1.5_f32, color),
            StrokeKind::Inside,
        );
        let glyph = match status {
            Status::Ok => "✓",
            Status::Blocked => "×",
            Status::Unknown => "?",
        };
        let c = Pos2::new(r.left() + 12.0 * zoom, r.bottom() - 12.0 * zoom);
        painter.circle_filled(c, 9.0 * zoom.max(0.7), color);
        painter.text(
            c,
            Align2::CENTER_CENTER,
            glyph,
            FontId::proportional((12.0 * zoom).max(7.0)),
            Color32::WHITE,
        );
        ui.interact(r, ui.id().with(("reach", key)), Sense::hover())
            .on_hover_text(tip);
    };
    let tip_of = |path: &ttg_codegen::reach::Path| {
        let mut tip = path.reason.clone();
        for n in &path.notes {
            tip.push('\n');
            tip.push_str(n);
        }
        tip
    };

    let mode = match &single {
        Some(src) if reach.posture.contains_key(src) => {
            let sp = &reach.posture[src];
            let src_initiates = app
                .project
                .entity(src)
                .is_some_and(|e| ttg_codegen::reach::initiates(e.resource_type));
            // Source ring.
            if let Some(r) = anchor_rect(app, origin, src) {
                painter.rect_stroke(
                    r.expand(4.0),
                    CornerRadius::same(8),
                    Stroke::new(3.0_f32, Color32::from_rgb(30, 100, 220)),
                    StrokeKind::Outside,
                );
            }
            let Some(a) = anchor_center(app, origin, src) else {
                return;
            };
            if src_initiates {
                // ---- what the selection can reach, with its way out drawn once.
                let paths = app.reach_paths();
                if let Egress::Via(hops) = &sp.egress {
                    let mut pts = vec![a];
                    for h in hops {
                        if let Some(c) = anchor_center(app, origin, h) {
                            pts.push(c);
                        }
                    }
                    // Extend past the last hop to show traffic leaving the network.
                    if let (Some(last_id), Some(&last)) = (hops.last(), pts.last()) {
                        let top = anchor_rect(app, origin, last_id)
                            .map(|r| r.top())
                            .unwrap_or(last.y);
                        let exit = Pos2::new(last.x, top - 26.0 * zoom.max(0.5));
                        pts.push(exit);
                        painter.text(
                            Pos2::new(exit.x, exit.y - 4.0),
                            Align2::CENTER_BOTTOM,
                            "internet / provider APIs",
                            FontId::proportional((11.0 * zoom).max(7.0)),
                            green,
                        );
                    }
                    draw_flow(&painter, &pts, Stroke::new(line_w + 1.0, green), false, arrow);
                }
                for path in &paths {
                    if !app.is_visible(&path.target) {
                        continue;
                    }
                    let Some(tr) = anchor_rect(app, origin, &path.target) else {
                        continue;
                    };
                    let color = status_color(path.status);
                    let tp = reach.posture.get(&path.target);
                    let network_path = tp.is_some_and(|t| t.networked && !t.subnets.is_empty())
                        && !sp.subnets.is_empty()
                        && path.status != Status::Unknown;
                    if network_path {
                        let mut pts = vec![a];
                        for h in &path.hops {
                            if let Some(c) = anchor_center(app, origin, h) {
                                pts.push(c);
                            }
                        }
                        pts.push(tr.center());
                        draw_flow(
                            &painter,
                            &pts,
                            Stroke::new(line_w, color),
                            path.status != Status::Ok,
                            arrow,
                        );
                    }
                    mark(ui, &painter, tr, path.status, &path.target, tip_of(path));
                }
                "from"
            } else {
                // ---- passive resource: who can reach it.
                let incoming = app.reach_incoming();
                for (sid, path) in &incoming {
                    if !app.is_visible(sid) {
                        continue;
                    }
                    let Some(sr) = anchor_rect(app, origin, sid) else {
                        continue;
                    };
                    let color = status_color(path.status);
                    let spos = reach.posture.get(sid);
                    let network_path = spos.is_some_and(|s| !s.subnets.is_empty())
                        && !sp.subnets.is_empty()
                        && path.status != Status::Unknown;
                    let mut pts = vec![sr.center()];
                    if network_path {
                        for h in &path.hops {
                            if let Some(c) = anchor_center(app, origin, h) {
                                pts.push(c);
                            }
                        }
                    }
                    pts.push(a);
                    draw_flow(
                        &painter,
                        &pts,
                        Stroke::new(line_w, color),
                        path.status != Status::Ok,
                        arrow,
                    );
                    mark(ui, &painter, sr, path.status, sid, tip_of(path));
                }
                if incoming.is_empty() {
                    painter.text(
                        Pos2::new(
                            a.x,
                            anchor_rect(app, origin, src).map(|r| r.bottom()).unwrap_or(a.y) + 14.0,
                        ),
                        Align2::CENTER_TOP,
                        "nothing on the diagram initiates traffic to this",
                        FontId::proportional((11.0 * zoom).max(7.0)),
                        Color32::from_gray(90),
                    );
                }
                "to"
            }
        }
        _ => {
            // Posture view: exposure and egress of every resource.
            for (id, po) in &reach.posture {
                if !app.is_visible(id) {
                    continue;
                }
                let Some(r) = anchor_rect(app, origin, id) else {
                    continue;
                };
                let mut tips: Vec<String> = Vec::new();
                if let Some(how) = &po.exposed {
                    painter.rect_stroke(
                        r.expand(3.0),
                        CornerRadius::same(8),
                        Stroke::new(3.0_f32, Color32::from_rgb(230, 126, 34)),
                        StrokeKind::Outside,
                    );
                    // Inbound arrow from "the internet" above the node.
                    let top = Pos2::new(r.center().x, r.top());
                    let from = top - Vec2::new(0.0, 28.0 * zoom.max(0.5));
                    painter.line_segment([from, top], Stroke::new(line_w, Color32::from_rgb(230, 126, 34)));
                    arrowhead(&painter, from, top, Color32::from_rgb(230, 126, 34), arrow);
                    tips.push(format!("Exposed to the internet: {how}"));
                }
                let (dot, tip) = match &po.egress {
                    Egress::Via(h) => (
                        Some(green),
                        Some(format!(
                            "Outbound via {}",
                            h.iter()
                                .map(|x| app
                                    .project
                                    .entity(x)
                                    .map(|e| e.name.to_string())
                                    .unwrap_or_default())
                                .collect::<Vec<_>>()
                                .join(" > ")
                        )),
                    ),
                    Egress::Blocked(why) => (
                        Some(Color32::from_rgb(210, 50, 50)),
                        Some(format!("No outbound path: {why}")),
                    ),
                    Egress::Unrestricted => (
                        None,
                        Some("Outside the network: reaches services directly".into()),
                    ),
                    Egress::NotNeeded => (None, None),
                };
                if let Some(c) = dot {
                    let p = Pos2::new(r.left() + 12.0 * zoom, r.bottom() - 12.0 * zoom);
                    painter.circle_filled(p, 6.0 * zoom.max(0.7), c);
                }
                if let Some(t) = tip {
                    tips.push(t);
                }
                if !tips.is_empty() {
                    ui.interact(r, ui.id().with(("posture", id)), Sense::hover())
                        .on_hover_text(tips.join("\n"));
                }
            }
            "posture"
        }
    };

    // Legend.
    let (title, lines): (&str, Vec<(Color32, &str)>) = match mode {
        "from" => (
            "Reachability from selection",
            vec![
                (Color32::from_rgb(30, 100, 220), "selected source"),
                (green, "reachable (arrow = direction of traffic)"),
                (Color32::from_rgb(210, 50, 50), "blocked (hover for why)"),
                (Color32::from_rgb(225, 150, 30), "undecidable from the diagram"),
            ],
        ),
        "to" => (
            "Who reaches the selection",
            vec![
                (Color32::from_rgb(30, 100, 220), "selected target (passive)"),
                (green, "can reach it (arrow = direction of traffic)"),
                (Color32::from_rgb(210, 50, 50), "blocked (hover for why)"),
                (Color32::from_rgb(225, 150, 30), "undecidable from the diagram"),
            ],
        ),
        _ => (
            "Network posture",
            vec![
                (
                    Color32::from_rgb(230, 126, 34),
                    "ring + arrow: exposed to the internet",
                ),
                (green, "dot: has an outbound path"),
                (Color32::from_rgb(210, 50, 50), "dot: no outbound path"),
                (Color32::from_gray(80), "tag: listening port"),
            ],
        ),
    };
    let mut y = app.canvas_rect.bottom() - 10.0 - 16.0 * lines.len() as f32;
    let x = app.canvas_rect.left() + 12.0;
    let bg = Rect::from_min_size(
        Pos2::new(x - 6.0, y - 20.0),
        Vec2::new(260.0, 16.0 * lines.len() as f32 + 26.0),
    );
    painter.rect_filled(
        bg,
        CornerRadius::same(4),
        Color32::from_rgba_unmultiplied(255, 255, 255, 220),
    );
    painter.text(
        Pos2::new(x, y - 12.0),
        Align2::LEFT_CENTER,
        title,
        FontId::proportional(12.0),
        Color32::from_gray(40),
    );
    for (c, label) in lines {
        painter.circle_filled(Pos2::new(x + 5.0, y + 6.0), 5.0, c);
        painter.text(
            Pos2::new(x + 16.0, y + 6.0),
            Align2::LEFT_CENTER,
            label,
            FontId::proportional(11.0),
            Color32::from_gray(60),
        );
        y += 16.0;
    }
}
