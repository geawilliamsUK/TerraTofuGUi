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
    /// Do the two rectangles share any area? Touching edges do not count, so a box
    /// placed exactly beside another is free.
    pub fn intersects(&self, o: Rect) -> bool {
        self.x < o.x + o.w && o.x < self.x + self.w && self.y < o.y + o.h && o.y < self.y + self.h
    }
}

/// Position of an entity as `view` sees it (a view-owned layout wins).
pub fn position_of(p: &Project, view: Option<&View>, id: &str) -> Option<Position> {
    let own = view
        .and_then(|v| v.layout.as_ref())
        .and_then(|l| l.positions.get(id))
        .copied();
    own.or_else(|| p.entity(id).map(|e| e.position))
}

/// Size of an entity as `view` sees it.
pub fn size_of(p: &Project, view: Option<&View>, id: &str) -> Option<Size> {
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
    if !fits || depth > 32 || !p.is_box(id) {
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

/// The leader from a note to what it explains: from the point of the note's edge nearest
/// the anchor to the point of the anchor's box nearest the note. Where the two overlap
/// along an axis the line runs straight across the middle of the overlap, so a note
/// placed below its resource gets a short vertical line that never crosses its text.
/// Returns `(from on the note, to on the anchor)`.
pub fn leader_line(note: Rect, anchor: Rect) -> ((f32, f32), (f32, f32)) {
    fn axis(a0: f32, a1: f32, b0: f32, b1: f32) -> (f32, f32) {
        if a0 > b1 {
            (a0, b1)
        } else if a1 < b0 {
            (a1, b0)
        } else {
            let mid = (a0.max(b0) + a1.min(b1)) / 2.0;
            (mid, mid)
        }
    }
    let (nx, ax) = axis(note.x, note.x + note.w, anchor.x, anchor.x + anchor.w);
    let (ny, ay) = axis(note.y, note.y + note.h, anchor.y, anchor.y + anchor.h);
    ((nx, ny), (ax, ay))
}

/// The note colours with a name: name and the accent it is drawn with (the border, and a
/// pale wash of it as the fill). Any other colour is written as `#rrggbb`.
pub const NOTE_COLORS: &[(&str, &str)] = &[
    ("neutral", "#6b7280"),
    ("info", "#2563eb"),
    ("warning", "#d97706"),
    ("danger", "#dc2626"),
    ("success", "#16a34a"),
];

/// A note colour as stored: a palette name (any case) or `#rrggbb`, normalised to lower
/// case; anything else is refused with the choices.
pub fn note_color(s: &str) -> Result<String, String> {
    let t = s.trim().to_ascii_lowercase();
    if NOTE_COLORS.iter().any(|(n, _)| *n == t) {
        return Ok(t);
    }
    let hex = t.strip_prefix('#').unwrap_or("");
    if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(t);
    }
    Err(format!(
        "note colour \"{s}\" is neither one of {} nor a #rrggbb colour",
        NOTE_COLORS.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
    ))
}

/// The accent of a stored note colour as RGB: the palette's for a name, the colour itself
/// for `#rrggbb`.
pub fn note_accent(color: &str) -> Option<(u8, u8, u8)> {
    let hex = NOTE_COLORS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(color.trim()))
        .map(|(_, h)| *h)
        .unwrap_or(color.trim());
    let hex = hex.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some(((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

/// Gap left between an anchored note and the thing it explains.
pub const NOTE_GAP: f32 = 24.0;
/// Height of a grouping box's label strip: a note may overlap a box, not its label.
pub const GROUP_LABEL_H: f32 = 26.0;

/// What a note placed beside `anchor` stores as its `position`: the offset from the
/// anchor's top-right corner (see [`note_rect`]) of the first spot that is clear of
/// `taken`. Tried in order: right, below, left, above, then the same four sides slid
/// along by one and two note-lengths, so a crowded anchor still gets a spot of its
/// own nearby rather than a long leader line. With nothing clear it goes to the right.
pub fn note_offset_beside(anchor: Rect, size: Size, taken: &[Rect]) -> Position {
    let a = anchor;
    let (w, h) = (size.w as f32, size.h as f32);
    let offset = |(x, y): (f32, f32)| Position {
        x: (x - (a.x + a.w)).round() as i32,
        y: (y - a.y).round() as i32,
    };
    let right = (a.x + a.w + NOTE_GAP, a.y);
    let mut spots = vec![
        right,
        (a.x, a.y + a.h + NOTE_GAP),
        (a.x - w - NOTE_GAP, a.y),
        (a.x, a.y - h - NOTE_GAP),
    ];
    for k in 1..=2 {
        let (dx, dy) = ((w + NOTE_GAP) * k as f32, (h + NOTE_GAP) * k as f32);
        spots.extend([
            (right.0, a.y + dy),
            (right.0, a.y - dy),
            (a.x + dx, a.y + a.h + NOTE_GAP),
            (a.x - w - NOTE_GAP, a.y + dy),
            (a.x - dx, a.y - h - NOTE_GAP),
        ]);
    }
    spots
        .into_iter()
        .find(|&(x, y)| {
            let r = Rect { x, y, w, h };
            !taken.iter().any(|o| o.intersects(r))
        })
        .map(offset)
        .unwrap_or_else(|| offset(right))
}

/// What a note must stay clear of in `v`: the resources the view shows, its logical
/// nodes, the header strips of containers and grouping boxes (a note may sit inside a
/// network or a box — it is not a member unless it is free-floating — but must not hide
/// what the box is called) and every note except `skip`.
pub fn note_obstacles(
    p: &Project,
    v: &View,
    visible: &dyn Fn(&str) -> bool,
    skip: Option<&str>,
) -> Vec<Rect> {
    let mut out: Vec<Rect> = p
        .entities()
        .iter()
        .filter(|e| visible(e.id))
        .filter_map(|e| {
            let r = entity_rect(p, Some(v), e.id, visible)?;
            Some(if p.is_box(e.id) {
                Rect {
                    h: CONTAINER_HEADER.min(r.h),
                    ..r
                }
            } else {
                r
            })
        })
        .collect();
    out.extend(v.groups.iter().map(|g| Rect {
        h: GROUP_LABEL_H,
        ..group_rect(g)
    }));
    out.extend(v.logicals.iter().map(logical_rect));
    out.extend(
        v.notes
            .iter()
            .filter(|n| Some(n.id.as_str()) != skip)
            .map(|n| note_rect(p, v, n, visible)),
    );
    out
}

/// Put every anchored note back beside what it explains, one after another so each
/// sees the ones already placed. Free notes stay where they are (see
/// [`arrange_free_notes`]). Returns how many notes moved. Used after a layout change,
/// when notes would otherwise hang at their old offsets on long leader lines.
pub fn arrange_notes(p: &Project, v: &mut View, visible: &dyn Fn(&str) -> bool) -> usize {
    let mut moved = 0;
    for i in 0..v.notes.len() {
        let n = &v.notes[i];
        let Some(a) = n.anchor.as_ref().and_then(|a| anchor_rect(p, v, a, visible)) else {
            continue;
        };
        let taken = note_obstacles(p, v, visible, Some(&n.id));
        let off = note_offset_beside(a, n.size, &taken);
        if v.notes[i].position != off {
            v.notes[i].position = off;
            moved += 1;
        }
    }
    moved
}

/// Gap between the content and the column [`arrange_free_notes`] stacks notes in: wider
/// than the region frame's margin, so a note never sits on the frame's line.
const STACK_GAP: f32 = 60.0;

/// Move the free notes (those with no anchor) that are in the way: a note that overlaps
/// a resource, the label of a container or grouping box, a logical node or another note
/// goes to the first free spot of a column to the right of what the view draws, from its
/// top down. Notes are taken in reading order — top to bottom, then left to right, as
/// they sit now — so they keep their order in the column; a note that is already clear
/// stays where it is. Returns how many notes moved.
pub fn arrange_free_notes(p: &Project, v: &mut View, visible: &dyn Fn(&str) -> bool) -> usize {
    let mut order: Vec<usize> = (0..v.notes.len())
        .filter(|&i| v.notes[i].anchor.is_none())
        .collect();
    order.sort_by_key(|&i| (v.notes[i].position.y, v.notes[i].position.x, i));
    let content = p
        .entities()
        .iter()
        .filter(|e| visible(e.id))
        .filter_map(|e| entity_rect(p, Some(v), e.id, visible))
        .chain(v.groups.iter().map(group_rect))
        .chain(v.logicals.iter().map(logical_rect))
        .chain(
            v.notes
                .iter()
                .filter(|n| n.anchor.is_some())
                .map(|n| note_rect(p, v, n, visible)),
        )
        .reduce(Rect::union)
        .unwrap_or(Rect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        });
    let mut moved = 0;
    for i in order {
        let n = &v.notes[i];
        let (w, h) = (n.size.w as f32, n.size.h as f32);
        let taken = note_obstacles(p, v, visible, Some(&n.id));
        if !taken.iter().any(|o| o.intersects(Rect::new(n.position, n.size))) {
            continue;
        }
        let x = content.x + content.w + STACK_GAP;
        let mut y = content.y;
        // Past each obstacle in turn: y only grows, so this ends.
        while let Some(o) = taken.iter().find(|o| o.intersects(Rect { x, y, w, h })) {
            y = o.y + o.h + NOTE_GAP;
        }
        v.notes[i].position = Position {
            x: x.round() as i32,
            y: y.round() as i32,
        };
        moved += 1;
    }
    moved
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
                classification: None,
                description: String::new(),
                owner: String::new(),
                overrides: Default::default(),
                adopt: Default::default(),
                outputs: None,
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
                    classification: None,
                    description: String::new(),
                    owner: String::new(),
                    overrides: Default::default(),
                    adopt: Default::default(),
                    outputs: None,
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
    fn rects_only_intersect_when_they_share_area() {
        let a = Rect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 50.0,
        };
        assert!(a.intersects(Rect {
            x: 90.0,
            y: 40.0,
            ..a
        }));
        // Touching edges are free: a box placed exactly beside another fits.
        assert!(!a.intersects(Rect {
            x: 100.0,
            y: 0.0,
            ..a
        }));
        assert!(!a.intersects(Rect { x: 0.0, y: 50.0, ..a }));
        assert!(!a.intersects(Rect {
            x: 200.0,
            y: 200.0,
            ..a
        }));
        assert!(a.intersects(a));
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
            color: None,
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

    #[test]
    fn arranging_notes_brings_them_back_beside_their_anchors() {
        let p = project_with_two_nodes();
        let mut v = View::new("map", Default::default());
        for (id, anchor) in [("n1", "a"), ("n2", "a")] {
            v.notes.push(Note {
                id: id.into(),
                title: id.into(),
                body: String::new(),
                // A long way off, as after the anchor moved and the note kept its offset.
                position: Position { x: 1500, y: 900 },
                size: Size { w: 200, h: 80 },
                anchor: Some(NoteAnchor::End(FlowEnd::Entity {
                    entity: anchor.into(),
                })),
                color: None,
            });
        }
        v.notes.push(Note {
            id: "free".into(),
            title: "free".into(),
            body: String::new(),
            position: Position { x: 3000, y: 3000 },
            size: Size { w: 200, h: 80 },
            anchor: None,
            color: None,
        });
        assert_eq!(arrange_notes(&p, &mut v, &all), 2);
        let a = entity_rect(&p, Some(&v), "a", &all).unwrap();
        let near = |r: Rect| {
            let (cx, cy) = r.center();
            let (ax, ay) = a.center();
            ((cx - ax).powi(2) + (cy - ay).powi(2)).sqrt() < 500.0
        };
        let r1 = note_rect(&p, &v, &v.notes[0], &all);
        let r2 = note_rect(&p, &v, &v.notes[1], &all);
        assert!(near(r1) && near(r2), "{r1:?} {r2:?} vs {a:?}");
        // Neither covers its anchor, the other node, or the other note.
        let b = entity_rect(&p, Some(&v), "b", &all).unwrap();
        for r in [r1, r2] {
            assert!(!r.intersects(a) && !r.intersects(b), "{r:?}");
        }
        assert!(!r1.intersects(r2), "{r1:?} {r2:?}");
        // The free note is not touched, and a second run changes nothing.
        assert_eq!(v.notes[2].position, Position { x: 3000, y: 3000 });
        assert_eq!(arrange_notes(&p, &mut v, &all), 0);
    }

    #[test]
    fn free_notes_in_the_way_stack_beside_the_content_in_order() {
        let p = project_with_two_nodes();
        let mut v = View::new("map", Default::default());
        let note = |id: &str, x: i32, y: i32| Note {
            id: id.into(),
            title: id.into(),
            body: String::new(),
            position: Position { x, y },
            size: Size { w: 200, h: 80 },
            anchor: None,
            color: None,
        };
        let a = entity_rect(&p, Some(&v), "a", &all).unwrap();
        let b = entity_rect(&p, Some(&v), "b", &all).unwrap();
        // Two on top of a resource (the lower one listed first) and one on top of the
        // second of them, one clear of everything, far off.
        v.notes.push(note("low", a.x as i32 + 10, a.y as i32 + 30));
        v.notes.push(note("high", a.x as i32 + 10, a.y as i32 + 10));
        v.notes.push(note("same", a.x as i32 + 10, a.y as i32 + 10));
        v.notes.push(note("clear", 3000, 3000));
        let clear = v.notes[3].position;
        assert_eq!(arrange_free_notes(&p, &mut v, &all), 3);
        let rects: Vec<Rect> = v.notes.iter().map(|n| note_rect(&p, &v, n, &all)).collect();
        let content = a.union(b).union(entity_rect(&p, Some(&v), "net", &all).unwrap());
        for r in &rects[..3] {
            assert!(!r.intersects(a) && !r.intersects(b), "{r:?}");
            assert!(
                r.x >= content.x + content.w,
                "right of the content: {r:?} {content:?}"
            );
        }
        for (i, r) in rects[..3].iter().enumerate() {
            for o in &rects[i + 1..3] {
                assert!(!r.intersects(*o), "{r:?} {o:?}");
            }
        }
        // Reading order is kept: "high" (listed second, higher up) first, then "same",
        // then "low", in one column from the top of the content.
        assert!(rects[1].y < rects[2].y && rects[2].y < rects[0].y);
        assert_eq!(rects[1].y, content.y);
        assert_eq!(v.notes[3].position, clear, "a clear note stays");
        // Nothing left to do the second time.
        assert_eq!(arrange_free_notes(&p, &mut v, &all), 0);
        // An anchored note is not one of them.
        v.notes[0].anchor = Some(NoteAnchor::End(FlowEnd::Entity { entity: "a".into() }));
        let at = v.notes[0].position;
        assert_eq!(arrange_free_notes(&p, &mut v, &all), 0);
        assert_eq!(v.notes[0].position, at);
    }

    #[test]
    fn a_leader_runs_between_the_nearest_edges() {
        let anchor = Rect {
            x: 100.0,
            y: 100.0,
            w: 176.0,
            h: 64.0,
        };
        // Below, overlapping horizontally: a short vertical line from the note's top
        // edge to the anchor's bottom edge.
        let below = Rect {
            x: 120.0,
            y: 200.0,
            w: 260.0,
            h: 120.0,
        };
        let (from, to) = leader_line(below, anchor);
        assert_eq!(from, (198.0, 200.0));
        assert_eq!(to, (198.0, 164.0));
        // To the right, overlapping vertically: horizontal, edge to edge.
        let right = Rect {
            x: 300.0,
            y: 80.0,
            w: 260.0,
            h: 120.0,
        };
        let (from, to) = leader_line(right, anchor);
        assert_eq!(from, (300.0, 132.0));
        assert_eq!(to, (276.0, 132.0));
        // Off a corner: corner to corner.
        let above_left = Rect {
            x: -300.0,
            y: -200.0,
            w: 260.0,
            h: 120.0,
        };
        assert_eq!(leader_line(above_left, anchor), ((-40.0, -80.0), (100.0, 100.0)));
    }

    #[test]
    fn note_colours_are_names_or_hex() {
        assert_eq!(note_color(" Warning ").unwrap(), "warning");
        assert_eq!(note_color("#AABBCC").unwrap(), "#aabbcc");
        assert!(note_color("orange").is_err());
        assert!(note_color("#abc").is_err());
        assert_eq!(note_accent("danger"), Some((0xdc, 0x26, 0x26)));
        assert_eq!(note_accent("#010203"), Some((1, 2, 3)));
    }
}
