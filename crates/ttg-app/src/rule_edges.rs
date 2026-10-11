//! Security-group rules drawn on the canvas. A rule whose source (or, for an egress
//! rule, destination) is another group is a dashed, labelled arrow in the direction the
//! traffic flows — between the two groups, or between the resources carrying them — in
//! a colour of its own so it never reads as a relation link. A rule from addresses only
//! (a CIDR, the internet, an AWS-managed prefix list) is a badge on the group. Clicking
//! either selects the group and points the inspector at the rule's row.
//! `ttg_codegen::canvas::rule_edges` derives them.

use crate::app::TtgApp;
use egui::{Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};
use ttg_codegen::canvas::{group_members, rule_edges, Peer, RuleEdge};
use ttg_core::Id;

/// How the canvas draws security-group rules (View menu).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleEdges {
    /// Between the two groups.
    Groups,
    /// Between the resources carrying them (a group with none stands for itself).
    Members,
    Off,
}

impl RuleEdges {
    pub fn key(self) -> &'static str {
        match self {
            RuleEdges::Groups => "groups",
            RuleEdges::Members => "members",
            RuleEdges::Off => "off",
        }
    }
    pub fn from_key(s: &str) -> Option<RuleEdges> {
        [RuleEdges::Groups, RuleEdges::Members, RuleEdges::Off]
            .into_iter()
            .find(|m| m.key() == s)
    }
}

const INK: Color32 = Color32::from_rgb(196, 84, 38);

impl TtgApp {
    /// Select a rule: its group, and its row in the inspector.
    pub fn select_rule(&mut self, group: &str, row: usize) {
        self.selection.clear();
        self.selection.insert(group.to_string());
        self.selected_edge = None;
        self.selected_annotation = None;
        self.selected_rule = Some((group.to_string(), row));
        self.focus_rule = Some((group.to_string(), row));
    }

    /// Are rule edges drawn at all? Not while the filter hides links, nor while it shows
    /// only some relation kinds (a rule is not a relation).
    fn rule_edges_shown(&self) -> bool {
        self.rule_edges != RuleEdges::Off && !self.filter.hide_edges && self.filter.relations.is_empty()
    }
}

/// The ends one rule is drawn between, in the direction of the traffic.
fn ends(app: &TtgApp, r: &RuleEdge) -> Vec<(Id, Id)> {
    let (Some(a), Some(b)) = (r.from(), r.to()) else {
        return Vec::new();
    };
    let side = |g: &str| -> Vec<Id> {
        let m = if app.rule_edges == RuleEdges::Members {
            group_members(&app.project, g)
        } else {
            Vec::new()
        };
        if m.is_empty() {
            vec![g.to_string()]
        } else {
            m
        }
    };
    let mut out = Vec::new();
    for x in side(a) {
        for y in side(b) {
            if x != y && app.is_visible(&x) && app.is_visible(&y) {
                out.push((x.clone(), y));
            }
        }
    }
    out
}

fn describe(app: &TtgApp, r: &RuleEdge) -> String {
    let name = |id: &str| {
        app.project
            .entity(id)
            .map(|e| e.name.to_string())
            .unwrap_or(id.to_string())
    };
    let peer = match &r.peer {
        Peer::Entity(e) => format!("\"{}\"", name(e)),
        Peer::Outside(s) => s.clone(),
    };
    format!(
        "\"{}\" {} rule \"{}\": {} {} {peer} (click to open the row)",
        name(&r.group),
        if r.ingress { "ingress" } else { "egress" },
        r.name,
        r.label,
        if r.ingress { "from" } else { "to" },
    )
}

/// Where the line from `r`'s centre towards `to` leaves it.
fn exit(r: Rect, to: Pos2) -> Pos2 {
    let c = r.center();
    let d = to - c;
    if d.length_sq() < 1.0 {
        return c;
    }
    let sx = if d.x.abs() > f32::EPSILON {
        (r.width() / 2.0) / d.x.abs()
    } else {
        f32::INFINITY
    };
    let sy = if d.y.abs() > f32::EPSILON {
        (r.height() / 2.0) / d.y.abs()
    } else {
        f32::INFINITY
    };
    c + d * sx.min(sy).min(1.0)
}

/// The rect an end attaches to: a node, or a box's header strip.
fn end_rect(app: &TtgApp, origin: Pos2, id: &str) -> Option<Rect> {
    let r = app.camera.rect_to_screen(origin, app.entity_rect(id)?);
    Some(if app.project.is_box(id) {
        Rect::from_min_size(r.min, Vec2::new(r.width(), 30.0 * app.camera.zoom))
    } else {
        r
    })
}

pub fn draw(app: &mut TtgApp, ui: &mut Ui, origin: Pos2) {
    if !app.rule_edges_shown() {
        return;
    }
    let rules = rule_edges(&app.project);
    if rules.is_empty() {
        return;
    }
    let painter = ui.painter_at(app.canvas_rect);
    let zoom = app.camera.zoom;
    let font = FontId::proportional((10.0 * zoom).max(6.0));
    let mut clicked: Option<(Id, usize)> = None;
    // Parallel rules between the same two ends fan out side by side.
    let mut seen: std::collections::HashMap<(Id, Id), usize> = Default::default();
    // Badges sit just below the group, left to right, wrapping at its width.
    let mut badges: std::collections::BTreeMap<Id, Badges> = Default::default();
    for r in &rules {
        let selected = app.selected_rule.as_ref() == Some(&(r.group.clone(), r.row));
        let color = if selected {
            Color32::from_rgb(30, 100, 220)
        } else {
            INK
        };
        if let Peer::Outside(what) = &r.peer {
            if !app.is_visible(&r.group) {
                continue;
            }
            let Some(gr) = end_rect(app, origin, &r.group) else {
                continue;
            };
            // What it admits from outside stands out; where it may send to is muted (an
            // "all traffic out" row is on nearly every group).
            let (text, fill, ink) = if r.ingress {
                (format!("in: {what} {}", r.label), color, Color32::WHITE)
            } else {
                (
                    format!("out: {what} {}", r.label),
                    if selected {
                        color
                    } else {
                        Color32::from_rgb(236, 226, 220)
                    },
                    if selected {
                        Color32::WHITE
                    } else {
                        Color32::from_rgb(130, 80, 60)
                    },
                )
            };
            let galley = painter.layout_no_wrap(text, font.clone(), ink);
            let size = galley.size() + Vec2::new(8.0, 3.0);
            let at = badges.entry(r.group.clone()).or_insert(Badges {
                x: gr.left() + 6.0 * zoom,
                y: gr.bottom() + 2.0,
                line: 0,
                more: Vec::new(),
            });
            if at.x + size.x > gr.right() && at.x > gr.left() + 6.0 * zoom {
                // Two lines at most; the rest are counted on a "+N" pill.
                if at.line + 1 >= MAX_BADGE_LINES || !at.more.is_empty() {
                    at.more.push(describe(app, r));
                    continue;
                }
                at.line += 1;
                at.x = gr.left() + 6.0 * zoom;
                at.y += size.y + 2.0;
            }
            let pill = Rect::from_min_size(Pos2::new(at.x, at.y), size);
            at.x += size.x + 3.0;
            painter.rect_filled(pill, CornerRadius::same(6), fill);
            painter.galley(pill.min + Vec2::new(4.0, 1.5), galley, ink);
            let resp = ui
                .interact(
                    pill,
                    ui.id().with(("rule-badge", &r.group, r.row)),
                    Sense::click(),
                )
                .on_hover_text(describe(app, r));
            if resp.clicked() {
                clicked = Some((r.group.clone(), r.row));
            }
            continue;
        }
        for (a, b) in ends(app, r) {
            let (Some(ar), Some(br)) = (end_rect(app, origin, &a), end_rect(app, origin, &b)) else {
                continue;
            };
            let key = if a < b {
                (a.clone(), b.clone())
            } else {
                (b.clone(), a.clone())
            };
            let n = seen.entry(key).or_default();
            let along = br.center() - ar.center();
            let normal = if along.length_sq() > 1.0 {
                Vec2::new(-along.y, along.x).normalized()
            } else {
                Vec2::ZERO
            };
            let shift = normal * (*n as f32 * 14.0 * zoom.clamp(0.5, 1.5));
            *n += 1;
            let p0 = exit(ar, br.center() + shift) + shift;
            let p1 = exit(br, ar.center() + shift) + shift;
            let stroke = Stroke::new(if selected { 2.6_f32 } else { 1.6_f32 }, color);
            painter.add(egui::Shape::dashed_line(&[p0, p1], stroke, 7.0, 3.0));
            // Arrowhead where the traffic arrives.
            let d = (p1 - p0).normalized();
            let side = Vec2::new(-d.y, d.x);
            let l = 9.0 * zoom.clamp(0.6, 1.4);
            painter.add(egui::Shape::convex_polygon(
                vec![p1, p1 - d * l + side * l * 0.5, p1 - d * l - side * l * 0.5],
                color,
                Stroke::NONE,
            ));
            let mid = p0 + (p1 - p0) * 0.5;
            let galley = painter.layout_no_wrap(r.label.clone(), font.clone(), color);
            let lr = Rect::from_center_size(mid, galley.size() + Vec2::new(8.0, 4.0));
            painter.rect(
                lr,
                CornerRadius::same(4),
                if selected {
                    Color32::from_rgb(225, 235, 255)
                } else {
                    Color32::from_rgb(255, 246, 240)
                },
                Stroke::new(1.0_f32, color),
                StrokeKind::Inside,
            );
            painter.galley(lr.min + Vec2::new(4.0, 2.0), galley, color);
            let resp = ui
                .interact(
                    lr,
                    ui.id().with(("rule", &r.group, r.row, &a, &b)),
                    Sense::click(),
                )
                .on_hover_text(describe(app, r));
            if resp.clicked() {
                clicked = Some((r.group.clone(), r.row));
            }
        }
    }
    for (g, b) in &badges {
        if b.more.is_empty() {
            continue;
        }
        let galley = painter.layout_no_wrap(format!("+{}", b.more.len()), font.clone(), Color32::WHITE);
        let pill = Rect::from_min_size(Pos2::new(b.x, b.y), galley.size() + Vec2::new(8.0, 3.0));
        painter.rect_filled(pill, CornerRadius::same(6), Color32::from_gray(120));
        painter.galley(pill.min + Vec2::new(4.0, 1.5), galley, Color32::WHITE);
        ui.interact(pill, ui.id().with(("rule-more", g)), Sense::hover())
            .on_hover_text(b.more.join("\n"));
    }
    if let Some((g, row)) = clicked {
        app.select_rule(&g, row);
    }
}

/// Where the next badge below a group goes, and what did not fit.
struct Badges {
    x: f32,
    y: f32,
    line: usize,
    more: Vec<String>,
}

const MAX_BADGE_LINES: usize = 2;
