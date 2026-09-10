//! Automatic layout ("tidy") and alignment helpers.
//!
//! `tidy` is a small layered layout applied recursively per container: the direct
//! children of a container (nodes and sub-containers) are ranked left-to-right by the
//! links between them (source on the left, what it depends on to its right), stacked
//! within each column, and every container is resized to fit what it holds. It is
//! deterministic, has no external dependencies, and is meant to produce a readable
//! starting point rather than an optimal drawing.

use crate::ir::{Id, Position, Project, Size};
use std::collections::{BTreeMap, BTreeSet};

/// Spacing used by `tidy`.
#[derive(Debug, Clone, Copy)]
pub struct TidyOptions {
    /// Horizontal gap between columns.
    pub column_gap: i32,
    /// Vertical gap between stacked entities.
    pub row_gap: i32,
    /// Inner padding of a container (left/right/bottom).
    pub padding: i32,
    /// Height reserved for a container's header strip.
    pub header: i32,
    /// Where the top-level layout starts.
    pub origin: Position,
}

impl Default for TidyOptions {
    fn default() -> Self {
        TidyOptions {
            column_gap: 90,
            row_gap: 40,
            padding: 30,
            header: 44,
            origin: Position { x: 40, y: 40 },
        }
    }
}

/// Lay out the whole diagram. `node_size` gives the drawn size of a node (containers
/// are resized to fit their contents).
pub fn tidy(p: &mut Project, node_size: &dyn Fn(&Project, &str) -> Size, opts: &TidyOptions) {
    let size = layout_children(p, None, node_size, opts);
    let _ = size;
}

/// Lay out the direct children of one container (or the top level), then place them
/// relative to the container's own position. Returns the content size.
pub fn tidy_container(
    p: &mut Project,
    container: &str,
    node_size: &dyn Fn(&Project, &str) -> Size,
    opts: &TidyOptions,
) {
    if p.containers.contains_key(container) {
        layout_children(p, Some(container), node_size, opts);
    }
}

/// The child of `parent` (or top-level entity) that contains `id`, if any.
fn representative(p: &Project, parent: Option<&str>, id: &str) -> Option<Id> {
    let mut cur = id.to_string();
    loop {
        let par = p.parent_of(&cur);
        if par == parent {
            return Some(cur);
        }
        cur = par?.to_string();
    }
}

fn direct_children(p: &Project, parent: Option<&str>) -> Vec<Id> {
    p.entities()
        .iter()
        .filter(|e| e.parent == parent)
        .map(|e| e.id.to_string())
        .collect()
}

/// Recursively lay out `parent`'s children; returns the bounding size of the content
/// (including padding). Children end up positioned relative to the parent's position
/// (or the origin at top level).
fn layout_children(
    p: &mut Project,
    parent: Option<&str>,
    node_size: &dyn Fn(&Project, &str) -> Size,
    opts: &TidyOptions,
) -> Size {
    let children = direct_children(p, parent);
    if children.is_empty() {
        return Size { w: 0, h: 0 };
    }
    // Bottom-up: sub-containers first so their size is known.
    let mut sizes: BTreeMap<Id, Size> = BTreeMap::new();
    for c in &children {
        if p.containers.contains_key(c) {
            let inner = layout_children(p, Some(c), node_size, opts);
            let s = Size {
                w: (inner.w + 2 * opts.padding).max(220),
                h: (inner.h + opts.header + opts.padding).max(140),
            };
            if let Some(cc) = p.containers.get_mut(c) {
                cc.size = s;
            }
            sizes.insert(c.clone(), s);
        } else {
            sizes.insert(c.clone(), node_size(p, c));
        }
    }

    // Graph among children: a -> b when something in a links to something in b.
    let set: BTreeSet<Id> = children.iter().cloned().collect();
    let mut succ: BTreeMap<Id, BTreeSet<Id>> =
        children.iter().map(|c| (c.clone(), BTreeSet::new())).collect();
    for e in &p.edges {
        let (Some(a), Some(b)) = (
            representative(p, parent, &e.source),
            representative(p, parent, &e.target),
        ) else {
            continue;
        };
        if a != b && set.contains(&a) && set.contains(&b) {
            succ.get_mut(&a).unwrap().insert(b);
        }
    }
    // Break cycles with a DFS: back edges are dropped.
    let mut order: Vec<Id> = children.clone();
    order.sort();
    let mut state: BTreeMap<Id, u8> = BTreeMap::new(); // 1 = on stack, 2 = done
    let mut dag: BTreeMap<Id, BTreeSet<Id>> = children.iter().map(|c| (c.clone(), BTreeSet::new())).collect();
    fn dfs(
        n: &Id,
        succ: &BTreeMap<Id, BTreeSet<Id>>,
        state: &mut BTreeMap<Id, u8>,
        dag: &mut BTreeMap<Id, BTreeSet<Id>>,
    ) {
        state.insert(n.clone(), 1);
        for m in &succ[n] {
            match state.get(m).copied().unwrap_or(0) {
                1 => {}
                2 => {
                    dag.get_mut(n).unwrap().insert(m.clone());
                }
                _ => {
                    dag.get_mut(n).unwrap().insert(m.clone());
                    dfs(m, succ, state, dag);
                }
            }
        }
        state.insert(n.clone(), 2);
    }
    for n in &order {
        if state.get(n).copied().unwrap_or(0) == 0 {
            dfs(n, &succ, &mut state, &mut dag);
        }
    }
    // Longest-path ranking: rank(n) = 1 + max rank of successors; sinks are 0, then
    // flip so sources sit in column 0 on the left.
    let mut rank: BTreeMap<Id, i32> = BTreeMap::new();
    fn height(n: &Id, dag: &BTreeMap<Id, BTreeSet<Id>>, rank: &mut BTreeMap<Id, i32>) -> i32 {
        if let Some(r) = rank.get(n) {
            return *r;
        }
        let r = dag[n].iter().map(|m| height(m, dag, rank) + 1).max().unwrap_or(0);
        rank.insert(n.clone(), r);
        r
    }
    for n in &order {
        height(n, &dag, &mut rank);
    }
    let max_rank = rank.values().copied().max().unwrap_or(0);
    let column = |n: &Id| max_rank - rank[n];

    // Columns; order inside a column by the average row of neighbours in the previous
    // column (one barycenter pass), ties by the original vertical position.
    let mut columns: Vec<Vec<Id>> = vec![Vec::new(); (max_rank + 1) as usize];
    for n in &order {
        columns[column(n) as usize].push(n.clone());
    }
    let pos_y = |p: &Project, id: &str| p.entity(id).map(|e| e.position.y).unwrap_or(0);
    let mut row_of: BTreeMap<Id, f32> = BTreeMap::new();
    for ci in 0..columns.len() {
        let mut col = columns[ci].clone();
        col.sort_by(|a, b| {
            let key = |n: &Id| -> (f32, i32) {
                let mut nb: Vec<f32> = Vec::new();
                if ci > 0 {
                    for m in &columns[ci - 1] {
                        if dag[m].contains(n) {
                            nb.push(row_of[m]);
                        }
                    }
                }
                let bary = if nb.is_empty() {
                    f32::MAX
                } else {
                    nb.iter().sum::<f32>() / nb.len() as f32
                };
                (bary, pos_y(p, n))
            };
            key(a).partial_cmp(&key(b)).unwrap_or(std::cmp::Ordering::Equal)
        });
        for (i, n) in col.iter().enumerate() {
            row_of.insert(n.clone(), i as f32);
        }
        columns[ci] = col;
    }

    // Place: columns left to right, entities stacked top to bottom, each column
    // vertically centred against the tallest column.
    let col_w: Vec<i32> = columns
        .iter()
        .map(|c| c.iter().map(|n| sizes[n].w).max().unwrap_or(0))
        .collect();
    let col_h: Vec<i32> = columns
        .iter()
        .map(|c| c.iter().map(|n| sizes[n].h).sum::<i32>() + opts.row_gap * (c.len() as i32 - 1).max(0))
        .collect();
    let total_h = col_h.iter().copied().max().unwrap_or(0);
    let base = match parent {
        Some(c) => {
            let cp = p.containers[c].position;
            Position {
                x: cp.x + opts.padding,
                y: cp.y + opts.header,
            }
        }
        None => opts.origin,
    };
    let mut x = base.x;
    for (ci, col) in columns.iter().enumerate() {
        let mut y = base.y + (total_h - col_h[ci]) / 2;
        for n in col {
            let s = sizes[n];
            let target = Position { x, y };
            move_entity(p, n, target);
            y += s.h + opts.row_gap;
        }
        x += col_w[ci] + opts.column_gap;
    }
    let total_w = col_w.iter().sum::<i32>() + opts.column_gap * (columns.len() as i32 - 1).max(0);
    Size {
        w: total_w,
        h: total_h,
    }
}

/// Move an entity to `to`, dragging its descendants along so their relative layout is kept.
fn move_entity(p: &mut Project, id: &str, to: Position) {
    let Some(from) = p.entity(id).map(|e| e.position) else {
        return;
    };
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    if dx == 0 && dy == 0 {
        return;
    }
    let mut ids = vec![id.to_string()];
    ids.extend(p.descendants_of(id));
    for i in ids {
        if let Some(n) = p.nodes.get_mut(&i) {
            n.position.x += dx;
            n.position.y += dy;
        } else if let Some(c) = p.containers.get_mut(&i) {
            c.position.x += dx;
            c.position.y += dy;
        }
    }
}

/// Alignment edge for [`align`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    HCenter,
    Right,
    Top,
    VCenter,
    Bottom,
}

/// Align the given entities on one edge (or centre line) of their common bounds.
/// Containers move with their contents. Returns how many entities moved.
pub fn align(p: &mut Project, ids: &[Id], how: Align, size: &dyn Fn(&Project, &str) -> Size) -> usize {
    let rects: Vec<(Id, Position, Size)> = ids
        .iter()
        .filter_map(|id| p.entity(id).map(|e| (id.clone(), e.position, size(p, id))))
        .collect();
    if rects.len() < 2 {
        return 0;
    }
    let min_x = rects.iter().map(|r| r.1.x).min().unwrap();
    let max_x = rects.iter().map(|r| r.1.x + r.2.w).max().unwrap();
    let min_y = rects.iter().map(|r| r.1.y).min().unwrap();
    let max_y = rects.iter().map(|r| r.1.y + r.2.h).max().unwrap();
    let mut moved = 0;
    for (id, pos, s) in rects {
        let target = match how {
            Align::Left => Position { x: min_x, ..pos },
            Align::Right => Position {
                x: max_x - s.w,
                ..pos
            },
            Align::HCenter => Position {
                x: (min_x + max_x) / 2 - s.w / 2,
                ..pos
            },
            Align::Top => Position { y: min_y, ..pos },
            Align::Bottom => Position {
                y: max_y - s.h,
                ..pos
            },
            Align::VCenter => Position {
                y: (min_y + max_y) / 2 - s.h / 2,
                ..pos
            },
        };
        if target != pos {
            move_entity(p, &id, target);
            moved += 1;
        }
    }
    moved
}

/// Spread the given entities so the gaps between them are equal, keeping the first and
/// last in place. `horizontal` distributes along x, otherwise along y.
pub fn distribute(
    p: &mut Project,
    ids: &[Id],
    horizontal: bool,
    size: &dyn Fn(&Project, &str) -> Size,
) -> usize {
    let mut rects: Vec<(Id, Position, Size)> = ids
        .iter()
        .filter_map(|id| p.entity(id).map(|e| (id.clone(), e.position, size(p, id))))
        .collect();
    if rects.len() < 3 {
        return 0;
    }
    rects.sort_by_key(|r| if horizontal { r.1.x } else { r.1.y });
    let (first, last) = (rects.first().unwrap().clone(), rects.last().unwrap().clone());
    let span = if horizontal {
        (last.1.x + last.2.w) - first.1.x
    } else {
        (last.1.y + last.2.h) - first.1.y
    };
    let total: i32 = rects.iter().map(|r| if horizontal { r.2.w } else { r.2.h }).sum();
    let gap = (span - total) / (rects.len() as i32 - 1);
    let mut cursor = if horizontal { first.1.x } else { first.1.y };
    let mut moved = 0;
    for (id, pos, s) in rects {
        let target = if horizontal {
            Position { x: cursor, ..pos }
        } else {
            Position { y: cursor, ..pos }
        };
        if target != pos {
            move_entity(p, &id, target);
            moved += 1;
        }
        cursor += if horizontal { s.w } else { s.h } + gap;
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::*;

    fn node(id: &str, parent: Option<&str>) -> Node {
        Node {
            id: id.into(),
            name: id.into(),
            resource_type: "x".into(),
            config: Default::default(),
            provider_config: Default::default(),
            position: Default::default(),
            size: None,
            parent: parent.map(|s| s.to_string()),
            manual: false,
            providers: Vec::new(),
            extra: Default::default(),
        }
    }
    fn sz(_: &Project, _: &str) -> Size {
        Size { w: 100, h: 50 }
    }

    #[test]
    fn tidy_ranks_sources_left_and_fits_containers() {
        let mut p = Project::new("t");
        p.containers.insert(
            "c".into(),
            Container {
                id: "c".into(),
                name: "c".into(),
                container_type: "box".into(),
                config: Default::default(),
                provider_config: Default::default(),
                position: Default::default(),
                size: Size::default(),
                parent: None,
                manual: false,
                providers: Vec::new(),
                extra: Default::default(),
            },
        );
        for (id, par) in [("a", None), ("b", Some("c")), ("d", Some("c"))] {
            p.nodes.insert(id.into(), node(id, par));
        }
        p.add_edge("a", "b", Relation::DependsOn);
        p.add_edge("b", "d", Relation::DependsOn);
        p.add_edge("d", "b", Relation::DependsOn); // cycle inside the container
        tidy(&mut p, &sz, &TidyOptions::default());
        let a = p.nodes["a"].position;
        let c = p.containers["c"].position;
        assert!(a.x < c.x, "source left of what it depends on: {a:?} {c:?}");
        let b = p.nodes["b"].position;
        let d = p.nodes["d"].position;
        assert!(b.x < d.x);
        let cs = p.containers["c"].size;
        assert!(b.x >= c.x && d.x + 100 <= c.x + cs.w, "children inside container");
        assert!(b.y >= c.y + 44);
    }

    #[test]
    fn align_and_distribute() {
        let mut p = Project::new("t");
        for (id, x, y) in [("a", 0, 0), ("b", 300, 40), ("c", 500, 90)] {
            let mut n = node(id, None);
            n.position = Position { x, y };
            p.nodes.insert(id.into(), n);
        }
        let ids: Vec<Id> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(align(&mut p, &ids, Align::Top, &sz), 2);
        assert!(ids.iter().all(|i| p.nodes[i].position.y == 0));
        assert_eq!(distribute(&mut p, &ids, true, &sz), 1);
        assert_eq!(p.nodes["b"].position.x, 250);
    }
}
