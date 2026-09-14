//! Geometry of a view: where a view draws each entity and annotation, and which things
//! a grouping box holds.
//!
//! The canvas and the document renderer must agree on all of this — a group's members
//! in the exported Markdown are the ones the user sees inside the box — so the rules
//! live here rather than in the app.

use crate::ir::{
    Flow, FlowEnd, Group, Id, Logical, Note, NoteAnchor, Position, Project, Size, View, NODE_SIZE,
};

/// Padding a view leaves around a container's contents when it fits the box to them.
pub const CONTAINER_PAD: f32 = 26.0;
/// Height of a container's header strip, reserved above its contents.
pub const CONTAINER_HEADER: f32 = 44.0;
/// Where an anchored note sits relative to what it explains, when it has no offset.
pub const NOTE_OFFSET: Position = Position { x: 24, y: 0 };

/// A rectangle in world units (pixels at zoom 1.0).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(pos: Position, size: Size) -> Self {
        Rect {
            x: pos.x as f32,
            y: pos.y as f32,
            w: size.w as f32,
            h: size.h as f32,
        }
    }
    pub fn center(&self) -> (f32, f32) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }
    pub fn union(self, o: Rect) -> Rect {
        let x = self.x.min(o.x);
        let y = self.y.min(o.y);
        Rect {
            x,
            y,
            w: (self.x + self.w).max(o.x + o.w) - x,
            h: (self.y + self.h).max(o.y + o.h) - y,
        }
    }
    /// Area, used to draw and hit-test the smallest (innermost) group last.
    pub fn area(&self) -> f32 {
        self.w * self.h
    }
}

/// Position of an entity as `view` sees it (a view-owned layout wins).
fn position_of(p: &Project, view: Option<&View>, id: &str) -> Option<Position> {
    let own = view
        .and_then(|v| v.layout.as_ref())
        .and_then(|l| l.positions.get(id))
        .copied();
    own.or_else(|| p.entity(id).map(|e| e.position))
}

/// Size of an entity as `view` sees it.
fn size_of(p: &Project, view: Option<&View>, id: &str) -> Option<Size> {
    if let Some(s) = view.and_then(|v| v.layout.as_ref()).and_then(|l| l.sizes.get(id)) {
        return Some(*s);
    }
    if let Some(c) = p.containers.get(id) {
        return Some(c.size);
    }
    p.nodes.get(id).map(|n| n.size.unwrap_or(NODE_SIZE))
}

/// World rect of an entity as `view` draws it. In a view with its own layout a
/// container is the padded bounding box of the members it still shows, so moving a
/// resource in the view takes its network with it; elsewhere it is the stored box.
pub fn entity_rect(
    p: &Project,
    view: Option<&View>,
    id: &str,
    visible: &dyn Fn(&str) -> bool,
) -> Option<Rect> {
    entity_rect_at(p, view, id, visible, 0)
}

fn entity_rect_at(
    p: &Project,
    view: Option<&View>,
    id: &str,
    visible: &dyn Fn(&str) -> bool,
    depth: usize,
) -> Option<Rect> {
    let stored = Rect::new(position_of(p, view, id)?, size_of(p, view, id)?);
    let fits = view.is_some_and(|v| v.layout.is_some());
    if !fits || depth > 32 || !p.containers.contains_key(id) {
        return Some(stored);
    }
    let mut content: Option<Rect> = None;
    for c in p.children_of(id) {
        if !visible(&c) {
            continue;
        }
        if let Some(r) = entity_rect_at(p, view, &c, visible, depth + 1) {
            content = Some(match content {
                Some(b) => b.union(r),
                None => r,
            });
        }
    }
    Some(match content {
        Some(b) => Rect {
            x: b.x - CONTAINER_PAD,
            y: b.y - CONTAINER_HEADER,
            w: b.w + CONTAINER_PAD * 2.0,
            h: b.h + CONTAINER_HEADER + CONTAINER_PAD,
        },
        None => stored,
    })
}

/// Does this container still hold anything the view shows? Sub-containers do not count
/// — a network holding one empty subnet is as empty as the subnet — so that a whole
/// branch of containment disappears together.
pub fn has_visible_members(p: &Project, id: &str, visible: &dyn Fn(&str) -> bool) -> bool {
    p.descendants_of(id)
        .iter()
        .any(|d| visible(d) && !p.containers.contains_key(d))
}

pub fn group_rect(g: &Group) -> Rect {
    Rect::new(g.position, g.size)
}

pub fn logical_rect(l: &Logical) -> Rect {
    Rect::new(l.position, l.size)
}

/// World rect of a flow endpoint.
pub fn end_rect(p: &Project, v: &View, end: &FlowEnd, visible: &dyn Fn(&str) -> bool) -> Option<Rect> {
    match end {
        FlowEnd::Entity { entity } => entity_rect(p, Some(v), entity, visible),
        FlowEnd::Group { group } => v.group(group).map(group_rect),
        FlowEnd::Logical { logical } => v.logical(logical).map(logical_rect),
    }
}

/// A zero-size rect at the middle of a flow, so a note can be pinned to the arrow.
fn flow_midpoint(p: &Project, v: &View, f: &Flow, visible: &dyn Fn(&str) -> bool) -> Option<Rect> {
    let a = end_rect(p, v, &f.from, visible)?.center();
    let b = end_rect(p, v, &f.to, visible)?.center();
    Some(Rect {
        x: (a.0 + b.0) / 2.0,
        y: (a.1 + b.1) / 2.0,
        w: 0.0,
        h: 0.0,
    })
}

/// What a note is anchored to, as a rect.
pub fn anchor_rect(p: &Project, v: &View, a: &NoteAnchor, visible: &dyn Fn(&str) -> bool) -> Option<Rect> {
    match a {
        NoteAnchor::End(e) => end_rect(p, v, e, visible),
        NoteAnchor::Flow { flow } => v.flow(flow).and_then(|f| flow_midpoint(p, v, f, visible)),
    }
}

/// Where a note is drawn: beside its anchor (so it travels with it) or at its own
/// position when it has none.
pub fn note_rect(p: &Project, v: &View, n: &Note, visible: &dyn Fn(&str) -> bool) -> Rect {
    let free = Rect::new(n.position, n.size);
    let Some(a) = n.anchor.as_ref().and_then(|a| anchor_rect(p, v, a, visible)) else {
        return free;
    };
    let off = if n.position == Position::default() {
        NOTE_OFFSET
    } else {
        n.position
    };
    Rect {
        x: a.x + a.w + off.x as f32,
        y: a.y + off.y as f32,
        ..free
    }
}

/// Entities whose centre lies inside the group's box (and that the view shows).
pub fn group_members(p: &Project, v: &View, gid: &str, visible: &dyn Fn(&str) -> bool) -> Vec<Id> {
    let Some(r) = v.group(gid).map(group_rect) else {
        return Vec::new();
    };
    p.entities()
        .iter()
        .filter(|e| visible(e.id))
        .filter(|e| {
            entity_rect(p, Some(v), e.id, visible).is_some_and(|er| {
                let (cx, cy) = er.center();
                r.contains(cx, cy)
            })
        })
        .map(|e| e.id.to_string())
        .collect()
}

/// Logical nodes whose centre lies inside the group's box.
pub fn group_logicals(v: &View, gid: &str) -> Vec<Id> {
    let Some(r) = v.group(gid).map(group_rect) else {
        return Vec::new();
    };
    v.logicals
        .iter()
        .filter(|l| {
            let (cx, cy) = logical_rect(l).center();
            r.contains(cx, cy)
        })
        .map(|l| l.id.clone())
        .collect()
}

/// Groups nested inside this one (their centre is in its box). Used so dragging an
/// outer box takes the inner ones along and so the document nests them.
pub fn group_children(v: &View, gid: &str) -> Vec<Id> {
    let Some(r) = v.group(gid).map(group_rect) else {
        return Vec::new();
    };
    v.groups
        .iter()
        .filter(|g| g.id != gid)
        .filter(|g| {
            let gr = group_rect(g);
            let (cx, cy) = gr.center();
            r.contains(cx, cy) && gr.area() < r.area()
        })
        .map(|g| g.id.clone())
        .collect()
}

/// The smallest group that holds this one, if any.
pub fn group_parent<'a>(v: &'a View, gid: &str) -> Option<&'a Group> {
    let me = v.group(gid).map(group_rect)?;
    let (cx, cy) = me.center();
    v.groups
        .iter()
        .filter(|g| g.id != gid)
        .filter(|g| {
            let gr = group_rect(g);
            gr.contains(cx, cy) && gr.area() > me.area()
        })
        .min_by(|a, b| {
            group_rect(a)
                .area()
                .partial_cmp(&group_rect(b).area())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// Group ids from the largest box to the smallest, the order they must be drawn in so
/// a nested box stays clickable.
pub fn groups_by_area(v: &View) -> Vec<Id> {
    let mut gs: Vec<&Group> = v.groups.iter().collect();
    gs.sort_by(|a, b| {
        group_rect(b)
            .area()
            .partial_cmp(&group_rect(a).area())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    gs.into_iter().map(|g| g.id.clone()).collect()
}

/// Display name of anything a flow or note can point at.
pub fn end_name(p: &Project, v: &View, end: &FlowEnd) -> String {
    match end {
        FlowEnd::Entity { entity } => p
            .entity(entity)
            .map(|e| e.name.to_string())
            .unwrap_or_else(|| entity.clone()),
        FlowEnd::Group { group } => v
            .group(group)
            .map(|g| format!("[{}]", g.label))
            .unwrap_or_else(|| group.clone()),
        FlowEnd::Logical { logical } => v
            .logical(logical)
            .map(|l| l.name.clone())
            .unwrap_or_else(|| logical.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Container, Node};

    fn project_with_two_nodes() -> Project {
        let mut p = Project::new("t");
        p.containers.insert(
            "net".into(),
            Container {
                id: "net".into(),
                name: "net".into(),
                container_type: "virtual_network".into(),
                config: Default::default(),
                provider_config: Default::default(),
                position: Position { x: 0, y: 0 },
                size: Size { w: 480, h: 320 },
                parent: None,
                manual: false,
                providers: Vec::new(),
                extra: Default::default(),
            },
        );
        for (id, x) in [("a", 100), ("b", 400)] {
            p.nodes.insert(
                id.into(),
                Node {
                    id: id.into(),
                    name: id.into(),
                    resource_type: "compute_instance".into(),
                    config: Default::default(),
                    provider_config: Default::default(),
                    position: Position { x, y: 100 },
                    size: None,
                    parent: Some("net".into()),
                    manual: false,
                    providers: Vec::new(),
                    extra: Default::default(),
                },
            );
        }
        p
    }

    fn all(_: &str) -> bool {
        true
    }

    #[test]
    fn a_container_follows_its_members_in_a_view() {
        let p = project_with_two_nodes();
        let mut v = View::new("map", Default::default());
        // Shared layout: the container keeps its stored box.
        let shared = entity_rect(&p, None, "net", &all).unwrap();
        assert_eq!((shared.w, shared.h), (480.0, 320.0));
        // Own layout: it is fitted around the two nodes...
        let fitted = entity_rect(&p, Some(&v), "net", &all).unwrap();
        assert!(fitted.w > 400.0 && fitted.w < 700.0, "{fitted:?}");
        assert_eq!(fitted.x, 100.0 - CONTAINER_PAD);
        // ...and follows one of them when the view moves it.
        v.layout
            .as_mut()
            .unwrap()
            .positions
            .insert("b".into(), Position { x: 900, y: 100 });
        let moved = entity_rect(&p, Some(&v), "net", &all).unwrap();
        assert!(moved.w > fitted.w, "{moved:?} vs {fitted:?}");
        // Hiding both empties it.
        assert!(!has_visible_members(&p, "net", &|_| false));
    }

    #[test]
    fn group_membership_is_recomputed_when_a_node_moves() {
        let mut p = project_with_two_nodes();
        let mut v = View::new("map", Default::default());
        v.groups.push(Group {
            id: "g".into(),
            label: "box".into(),
            position: Position { x: 60, y: 60 },
            size: Size { w: 260, h: 200 },
            color: None,
        });
        assert_eq!(group_members(&p, &v, "g", &all), vec!["a".to_string()]);
        // Move "b" into the box: it joins without anyone re-declaring membership, and
        // so does the network, which has shrunk around the two of them.
        p.nodes.get_mut("b").unwrap().position = Position { x: 120, y: 160 };
        let mut members = group_members(&p, &v, "g", &all);
        members.sort();
        assert_eq!(members, vec!["a".to_string(), "b".to_string(), "net".to_string()]);
        // Move "a" out again in the view's own layout only.
        v.layout
            .as_mut()
            .unwrap()
            .positions
            .insert("a".into(), Position { x: 900, y: 900 });
        assert_eq!(group_members(&p, &v, "g", &all), vec!["b".to_string()]);
    }

    #[test]
    fn nested_groups_report_their_parent() {
        let mut v = View::new("map", Default::default());
        v.groups.push(Group {
            id: "outer".into(),
            label: "outer".into(),
            position: Position { x: 0, y: 0 },
            size: Size { w: 800, h: 600 },
            color: None,
        });
        v.groups.push(Group {
            id: "inner".into(),
            label: "inner".into(),
            position: Position { x: 100, y: 100 },
            size: Size { w: 200, h: 200 },
            color: None,
        });
        assert_eq!(group_parent(&v, "inner").map(|g| g.id.as_str()), Some("outer"));
        assert!(group_parent(&v, "outer").is_none());
        assert_eq!(group_children(&v, "outer"), vec!["inner".to_string()]);
        // Largest first, so the inner box is drawn (and clicked) on top.
        assert_eq!(groups_by_area(&v), vec!["outer".to_string(), "inner".to_string()]);
    }

    #[test]
    fn an_anchored_note_travels_with_what_it_explains() {
        let p = project_with_two_nodes();
        let mut v = View::new("map", Default::default());
        let n = Note {
            id: "n".into(),
            title: "why".into(),
            body: "because".into(),
            position: Position::default(),
            size: Size { w: 260, h: 120 },
            anchor: Some(NoteAnchor::End(FlowEnd::Entity { entity: "a".into() })),
        };
        v.notes.push(n.clone());
        let before = note_rect(&p, &v, &n, &all);
        v.layout
            .as_mut()
            .unwrap()
            .positions
            .insert("a".into(), Position { x: 500, y: 300 });
        let after = note_rect(&p, &v, &n, &all);
        assert_eq!(after.x - before.x, 400.0);
        assert_eq!(after.y - before.y, 200.0);
    }
}
