//! Laying a view out by its data flows ("tidy by flows").
//!
//! `layout::tidy` ranks resources by their dependency links, which is the right picture
//! for the infrastructure and the wrong one for a data-flow map: a worker that reads a
//! queue depends on the queue, so tidy puts the worker to the *left* of the queue that
//! feeds it. This lays out the flows instead, as a small layered (Sugiyama-style)
//! drawing with the view's grouping boxes as swimlanes, reading left to right:
//!
//! 1. **Rank.** Flows are taken in step order (numbered steps first, then the rest in
//!    the order they were drawn). A depth-first walk from the things that take part in
//!    the earliest steps drops the flows that would close a cycle — an orchestrator's
//!    "stage done" going back to the API is drawn right to left instead of pulling the
//!    API to the far right — and every item's column is the longest path of flows that
//!    ends at it. Items no flow touches get a column of their own at the right.
//! 2. **Lanes.** Every grouping box becomes a horizontal band (a nested box a band
//!    inside its parent's), and so does "no box". Bands are stacked top to bottom in the
//!    order their members first take part, so the drawing reads diagonally from the
//!    first step to the last, and the boxes, refitted around their members afterwards,
//!    never overlap.
//! 3. **Order.** Inside a column items start in step order, then four barycentre sweeps
//!    (down and up) move each one towards the average row of its neighbours in the other
//!    columns, which removes most crossings; the lanes then keep their members together.
//! 4. **Place.** Columns are placed left to right with room for labels between them;
//!    inside its lane and column each item aims for the height of what flows into it
//!    from the same lane, so a chain of flows draws as a straight line where there is
//!    room.
//!
//! Everything is deterministic: ties are broken by step, then position, then id.

use crate::ir::{Id, Position, Size, View, ViewLayout};
use crate::view::{self, Rect};
use crate::Project;
use std::collections::{BTreeMap, BTreeSet};

/// Something the flow layout places: a resource or a logical node.
#[derive(Debug, Clone)]
pub struct FlowItem {
    pub id: Id,
    pub size: Size,
    /// Where it is now, used only to break ties so a re-run is stable.
    pub position: Position,
    /// The grouping boxes it sits in, outermost first (empty = none): its lane.
    pub groups: Vec<Id>,
}

/// A flow between two items.
#[derive(Debug, Clone)]
pub struct FlowLink {
    pub from: Id,
    pub to: Id,
    pub step: Option<u32>,
}

/// Spacing for [`flow_layout`].
#[derive(Debug, Clone, Copy)]
pub struct FlowLayoutOptions {
    /// Horizontal gap between columns: wide, because flow labels sit in it.
    pub column_gap: i32,
    /// Vertical gap between items in a column.
    pub row_gap: i32,
    /// Top-left of the drawing.
    pub origin: Position,
}

impl Default for FlowLayoutOptions {
    fn default() -> Self {
        FlowLayoutOptions {
            column_gap: 190,
            row_gap: 48,
            origin: Position { x: 40, y: 40 },
        }
    }
}

/// Padding a refitted grouping box keeps around its members (its label strip,
/// [`view::GROUP_LABEL_H`], comes on top of this).
pub const GROUP_PAD: i32 = 22;
/// Clear space between two neighbouring lanes' boxes.
const LANE_GAP: i32 = 28;

/// Column index of every item (see the module documentation, step 1).
pub fn flow_ranks(items: &[FlowItem], links: &[FlowLink]) -> BTreeMap<Id, usize> {
    let known: BTreeSet<&str> = items.iter().map(|i| i.id.as_str()).collect();
    let ordered = reading_order(links, &known);
    let first_step = first_steps(items, &ordered);
    let mut succ: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (_, l) in &ordered {
        succ.entry(l.from.as_str()).or_default().push(l.to.as_str());
    }
    // Roots in the order their first step comes up.
    let mut roots: Vec<&FlowItem> = items.iter().collect();
    roots.sort_by_key(|i| {
        (
            first_step[i.id.as_str()],
            i.position.y,
            i.position.x,
            i.id.clone(),
        )
    });
    let mut state: BTreeMap<&str, u8> = BTreeMap::new(); // 1 = on the stack, 2 = done
    let mut dag: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut topo: Vec<&str> = Vec::new();
    fn visit<'a>(
        n: &'a str,
        succ: &BTreeMap<&'a str, Vec<&'a str>>,
        state: &mut BTreeMap<&'a str, u8>,
        dag: &mut BTreeMap<&'a str, Vec<&'a str>>,
        topo: &mut Vec<&'a str>,
    ) {
        state.insert(n, 1);
        for &m in succ.get(n).map(|v| v.as_slice()).unwrap_or(&[]) {
            match state.get(m).copied().unwrap_or(0) {
                // A back edge: this flow closes a cycle, so it does not rank anything.
                1 => {}
                2 => dag.entry(n).or_default().push(m),
                _ => {
                    dag.entry(n).or_default().push(m);
                    visit(m, succ, state, dag, topo);
                }
            }
        }
        state.insert(n, 2);
        topo.push(n);
    }
    for r in &roots {
        if state.get(r.id.as_str()).copied().unwrap_or(0) == 0 {
            visit(r.id.as_str(), &succ, &mut state, &mut dag, &mut topo);
        }
    }
    // Reverse post-order is a topological order of the acyclic flows.
    topo.reverse();
    let mut rank: BTreeMap<&str, usize> = items.iter().map(|i| (i.id.as_str(), 0)).collect();
    for n in &topo {
        let r = rank[n];
        for &m in dag.get(n).map(|v| v.as_slice()).unwrap_or(&[]) {
            if rank[m] < r + 1 {
                rank.insert(m, r + 1);
            }
        }
    }
    let touched: BTreeSet<&str> = ordered
        .iter()
        .flat_map(|(_, l)| [l.from.as_str(), l.to.as_str()])
        .collect();
    let spare = rank
        .iter()
        .filter(|(id, _)| touched.contains(*id))
        .map(|(_, r)| *r + 1)
        .max()
        .unwrap_or(0);
    rank.into_iter()
        .map(|(id, r)| (id.to_string(), if touched.contains(id) { r } else { spare }))
        .collect()
}

/// The flows between known items, in reading order: numbered steps first, then the
/// order they were drawn.
fn reading_order<'a>(links: &'a [FlowLink], known: &BTreeSet<&str>) -> Vec<(usize, &'a FlowLink)> {
    let mut ordered: Vec<(usize, &FlowLink)> = links
        .iter()
        .enumerate()
        .filter(|(_, l)| l.from != l.to && known.contains(l.from.as_str()) && known.contains(l.to.as_str()))
        .collect();
    ordered.sort_by_key(|(i, l)| (l.step.unwrap_or(u32::MAX), *i));
    ordered
}

/// The earliest step each item takes part in (`u64::MAX` for none), counting unnumbered
/// flows as coming after every numbered one, in the order they were drawn.
fn first_steps<'a>(items: &'a [FlowItem], ordered: &[(usize, &FlowLink)]) -> BTreeMap<&'a str, u64> {
    let mut out: BTreeMap<&str, u64> = items.iter().map(|i| (i.id.as_str(), u64::MAX)).collect();
    for (k, (_, l)) in ordered.iter().enumerate() {
        let key = (l.step.map(|s| s as u64).unwrap_or(1 << 32) << 16) + k as u64;
        for e in [l.from.as_str(), l.to.as_str()] {
            if let Some(v) = out.get_mut(e) {
                *v = (*v).min(key);
            }
        }
    }
    out
}

/// The lanes, top to bottom (see the module documentation, step 2): each is a chain of
/// grouping boxes, outermost first. A box's own members come before, or after, its
/// nested boxes according to which takes part first, and nested lanes stay together.
fn lane_order(items: &[FlowItem], first_step: &BTreeMap<&str, u64>) -> Vec<Vec<Id>> {
    let lanes: BTreeSet<Vec<Id>> = items.iter().map(|i| i.groups.clone()).collect();
    // Earliest step of everything under a chain prefix.
    let earliest = |prefix: &[Id], exact: bool| -> u64 {
        items
            .iter()
            .filter(|i| {
                if exact {
                    i.groups.as_slice() == prefix
                } else {
                    i.groups.starts_with(prefix)
                }
            })
            .map(|i| first_step[i.id.as_str()])
            .min()
            .unwrap_or(u64::MAX)
    };
    let key = |lane: &Vec<Id>| -> Vec<(u64, Id)> {
        let mut k: Vec<(u64, Id)> = (1..=lane.len())
            .map(|n| (earliest(&lane[..n], false), lane[n - 1].clone()))
            .collect();
        // The box's own members, as a sibling of its nested boxes.
        k.push((earliest(lane, true), String::new()));
        k
    };
    let mut out: Vec<Vec<Id>> = lanes.into_iter().collect();
    out.sort_by_key(|l| key(l));
    out
}

/// Vertical space between two neighbouring lanes: room for the boxes one closes and
/// the next opens (padding, label strip), plus a gap.
fn lane_gap(a: &[Id], b: &[Id]) -> i32 {
    let common = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let closed = (a.len() - common) as i32;
    let opened = (b.len() - common) as i32;
    LANE_GAP + closed * GROUP_PAD + opened * (GROUP_PAD + view::GROUP_LABEL_H as i32)
}

/// Positions (top-left corners) for every item. See the module documentation.
pub fn flow_layout(
    items: &[FlowItem],
    links: &[FlowLink],
    opts: &FlowLayoutOptions,
) -> BTreeMap<Id, Position> {
    if items.is_empty() {
        return BTreeMap::new();
    }
    let rank = flow_ranks(items, links);
    let by_id: BTreeMap<&str, &FlowItem> = items.iter().map(|i| (i.id.as_str(), i)).collect();
    let known: BTreeSet<&str> = by_id.keys().copied().collect();
    let ordered = reading_order(links, &known);
    let first_step = first_steps(items, &ordered);
    let lanes = lane_order(items, &first_step);
    let lane_of: BTreeMap<&str, usize> = items
        .iter()
        .map(|i| {
            (
                i.id.as_str(),
                lanes.iter().position(|l| *l == i.groups).unwrap_or(0),
            )
        })
        .collect();

    // Neighbours in both directions, for the barycentre sweeps.
    let mut nbrs: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (_, l) in &ordered {
        nbrs.entry(l.from.as_str()).or_default().push(l.to.as_str());
        nbrs.entry(l.to.as_str()).or_default().push(l.from.as_str());
    }

    let n_cols = rank.values().copied().max().unwrap_or(0) + 1;
    let mut columns: Vec<Vec<&str>> = vec![Vec::new(); n_cols];
    for i in items {
        columns[rank[&i.id]].push(i.id.as_str());
    }
    for col in columns.iter_mut() {
        col.sort_by_key(|id| {
            let it = by_id[id];
            (
                lane_of[id],
                first_step[id],
                it.position.y,
                it.position.x,
                it.id.clone(),
            )
        });
    }

    // Barycentre sweeps: down (look left) then up (look right), four times. The rows
    // compared are global (lane first), so a lane never mixes with the next.
    for _ in 0..4 {
        for down in [true, false] {
            let cols: Vec<usize> = if down {
                (1..n_cols).collect()
            } else {
                (0..n_cols.saturating_sub(1)).rev().collect()
            };
            for c in cols {
                let mut at: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
                for (ci, col) in columns.iter().enumerate() {
                    for (r, id) in col.iter().enumerate() {
                        at.insert(id, (ci, r));
                    }
                }
                let key = |id: &str, here: usize| -> f32 {
                    let rows: Vec<f32> = nbrs
                        .get(id)
                        .into_iter()
                        .flatten()
                        .filter_map(|m| at.get(m))
                        .filter(|(mc, _)| if down { *mc < c } else { *mc > c })
                        .map(|(_, r)| *r as f32)
                        .collect();
                    if rows.is_empty() {
                        here as f32
                    } else {
                        rows.iter().sum::<f32>() / rows.len() as f32
                    }
                };
                let mut keyed: Vec<(usize, f32, usize, &str)> = columns[c]
                    .iter()
                    .enumerate()
                    .map(|(i, id)| (lane_of[id], key(id, i), i, *id))
                    .collect();
                keyed.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)).then(a.2.cmp(&b.2)));
                columns[c] = keyed.into_iter().map(|(_, _, _, id)| id).collect();
            }
        }
    }

    // Place: columns left to right; lanes top to bottom; inside a lane each item aims
    // for the middle of what flows into it from the same lane, further left.
    let widths: Vec<i32> = columns
        .iter()
        .map(|c| c.iter().map(|id| by_id[id].size.w).max().unwrap_or(0))
        .collect();
    let mut col_x: Vec<i32> = Vec::with_capacity(n_cols);
    let mut x = opts.origin.x;
    for w in &widths {
        col_x.push(x);
        x += w + opts.column_gap;
    }
    let mut preds: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (_, l) in &ordered {
        if rank[&l.from] < rank[&l.to] && lane_of[l.from.as_str()] == lane_of[l.to.as_str()] {
            preds.entry(l.to.as_str()).or_default().push(l.from.as_str());
        }
    }
    let mut out: BTreeMap<Id, Position> = BTreeMap::new();
    let mut centre_y: BTreeMap<&str, f32> = BTreeMap::new();
    let mut lane_top = opts.origin.y as f32;
    for (li, lane) in lanes.iter().enumerate() {
        let mut bottom = lane_top;
        for (c, col) in columns.iter().enumerate() {
            let mut next_free = lane_top;
            for &id in col.iter().filter(|id| lane_of[**id] == li) {
                let s = by_id[id].size;
                let wanted = preds.get(id).and_then(|ps| {
                    let ys: Vec<f32> = ps.iter().filter_map(|p| centre_y.get(p)).copied().collect();
                    (!ys.is_empty()).then(|| ys.iter().sum::<f32>() / ys.len() as f32 - s.h as f32 / 2.0)
                });
                let y = wanted.unwrap_or(next_free).max(next_free);
                // Centred in its column, so arrows meet the column's middle line.
                let px = col_x[c] + (widths[c] - s.w) / 2;
                out.insert(
                    id.to_string(),
                    Position {
                        x: px,
                        y: y.round() as i32,
                    },
                );
                centre_y.insert(id, y + s.h as f32 / 2.0);
                next_free = y + (s.h + opts.row_gap) as f32;
                bottom = bottom.max(y + s.h as f32);
            }
        }
        if let Some(next) = lanes.get(li + 1) {
            lane_top = bottom + lane_gap(lane, next) as f32;
        }
    }
    out
}

/// Lay a view out by its flows: resources in the view's *own* layout (one is created
/// when the view shares the All layout, so other views never move), logical nodes in
/// place, and every grouping box refitted around the members it held before. Notes are
/// left to [`view::arrange_notes`]. Returns how many things moved, or why nothing could.
pub fn layout_view_by_flows(
    p: &Project,
    v: &mut View,
    visible: &dyn Fn(&str) -> bool,
    opts: &FlowLayoutOptions,
) -> Result<usize, String> {
    // What sits in which box, before anything moves: membership is geometric, so it
    // has to be read now and restored by refitting the boxes afterwards.
    let order = view::groups_by_area(v);
    let mut innermost: BTreeMap<Id, Id> = BTreeMap::new();
    let mut members: BTreeMap<Id, Vec<Id>> = BTreeMap::new();
    for gid in &order {
        // Largest first, so a smaller box overwrites a larger one: innermost wins.
        let ms: Vec<Id> = view::group_members(p, v, gid, visible)
            .into_iter()
            .filter(|m| p.nodes.contains_key(m))
            .chain(view::group_logicals(v, gid))
            .collect();
        for m in &ms {
            innermost.insert(m.clone(), gid.clone());
        }
        members.insert(gid.clone(), ms);
    }
    let children: BTreeMap<Id, Vec<Id>> = order
        .iter()
        .map(|g| (g.clone(), view::group_children(v, g)))
        .collect();
    // The chain of boxes around an item, outermost first.
    let chain = |id: &str| -> Vec<Id> {
        let mut out = Vec::new();
        let mut cur = innermost.get(id).cloned();
        while let Some(g) = cur {
            if out.contains(&g) || out.len() > 16 {
                break;
            }
            cur = view::group_parent(v, &g).map(|x| x.id.clone());
            out.push(g);
        }
        out.reverse();
        out
    };

    let mut items: Vec<FlowItem> = Vec::new();
    for id in p.nodes.keys().filter(|id| visible(id)) {
        let Some(r) = view::entity_rect(p, Some(v), id, visible) else {
            continue;
        };
        items.push(FlowItem {
            id: id.clone(),
            size: Size {
                w: r.w as i32,
                h: r.h as i32,
            },
            position: Position {
                x: r.x as i32,
                y: r.y as i32,
            },
            groups: chain(id),
        });
    }
    for l in &v.logicals {
        items.push(FlowItem {
            id: l.id.clone(),
            size: l.size,
            position: l.position,
            groups: chain(&l.id),
        });
    }
    let placed: BTreeSet<&str> = items.iter().map(|i| i.id.as_str()).collect();
    let links: Vec<FlowLink> = v
        .flows
        .iter()
        .filter(|f| placed.contains(f.from.id()) && placed.contains(f.to.id()))
        .map(|f| FlowLink {
            from: f.from.id().to_string(),
            to: f.to.id().to_string(),
            step: f.step,
        })
        .collect();
    if links.is_empty() {
        return Err(format!(
            "the view \"{}\" has no flows between the resources and logical nodes it shows; \
             draw some (or view_generate kind \"data_flow\") first",
            v.name
        ));
    }
    // Start where the drawing already is, so the camera does not jump far.
    let origin = items
        .iter()
        .map(|i| i.position)
        .reduce(|a, b| Position {
            x: a.x.min(b.x),
            y: a.y.min(b.y),
        })
        .unwrap_or(opts.origin);
    let opts = FlowLayoutOptions { origin, ..*opts };
    let at = flow_layout(&items, &links, &opts);

    let mut moved = 0;
    let layout = v.layout.get_or_insert_with(ViewLayout::default);
    for (id, pos) in &at {
        if p.nodes.contains_key(id) {
            if layout.positions.get(id) != Some(pos) {
                layout.positions.insert(id.clone(), *pos);
                moved += 1;
            }
        } else if let Some(l) = v.logicals.iter_mut().find(|l| &l.id == id) {
            if l.position != *pos {
                l.position = *pos;
                moved += 1;
            }
        }
    }

    // Refit the boxes, smallest first so an outer box grows around the inner ones.
    let rect_of = |v: &View, id: &str| -> Option<Rect> {
        if let Some(l) = v.logical(id) {
            return Some(view::logical_rect(l));
        }
        view::entity_rect(p, Some(v), id, visible)
    };
    for gid in order.iter().rev() {
        let mut bb: Option<Rect> = None;
        for m in &members[gid] {
            if let Some(r) = rect_of(v, m) {
                bb = Some(bb.map_or(r, |b| b.union(r)));
            }
        }
        for c in &children[gid] {
            if let Some(g) = v.group(c) {
                let r = view::group_rect(g);
                bb = Some(bb.map_or(r, |b| b.union(r)));
            }
        }
        let Some(b) = bb else { continue };
        let pad = GROUP_PAD as f32;
        if let Some(g) = v.group_mut(gid) {
            g.position = Position {
                x: (b.x - pad).round() as i32,
                y: (b.y - pad - view::GROUP_LABEL_H).round() as i32,
            };
            g.size = Size {
                w: (b.w + 2.0 * pad).round() as i32,
                h: (b.h + 2.0 * pad + view::GROUP_LABEL_H).round() as i32,
            };
        }
    }
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, y: i32) -> FlowItem {
        FlowItem {
            id: id.into(),
            size: Size { w: 100, h: 50 },
            position: Position { x: 0, y },
            groups: Vec::new(),
        }
    }

    fn link(a: &str, b: &str, step: Option<u32>) -> FlowLink {
        FlowLink {
            from: a.into(),
            to: b.into(),
            step,
        }
    }

    #[test]
    fn ranks_follow_step_order_and_a_cycle_does_not_pull_the_start_right() {
        // A pipeline with an orchestrator loop: api -> queue -> worker -> results -> api.
        let items: Vec<FlowItem> = ["browser", "api", "queue", "worker", "results", "loner"]
            .iter()
            .enumerate()
            .map(|(i, id)| item(id, i as i32 * 10))
            .collect();
        let links = vec![
            link("results", "api", Some(5)),
            link("browser", "api", Some(1)),
            link("api", "queue", Some(2)),
            link("queue", "worker", Some(3)),
            link("worker", "results", Some(4)),
        ];
        let r = flow_ranks(&items, &links);
        assert_eq!(r["browser"], 0);
        assert_eq!(r["api"], 1);
        assert_eq!(r["queue"], 2);
        assert_eq!(r["worker"], 3);
        assert_eq!(r["results"], 4);
        // Nothing flows through it: a column of its own at the right.
        assert_eq!(r["loner"], 5);
        // The positions go left to right in the same order.
        let at = flow_layout(&items, &links, &FlowLayoutOptions::default());
        let xs: Vec<i32> = ["browser", "api", "queue", "worker", "results", "loner"]
            .iter()
            .map(|id| at[*id].x)
            .collect();
        assert!(xs.windows(2).all(|w| w[0] < w[1]), "{xs:?}");
    }

    #[test]
    fn the_layout_is_deterministic_and_straightens_a_chain() {
        let items: Vec<FlowItem> = ["a", "b", "c", "d"]
            .iter()
            .enumerate()
            .map(|(i, id)| item(id, 300 - i as i32 * 40))
            .collect();
        let links = vec![
            link("a", "b", Some(1)),
            link("b", "c", Some(2)),
            link("a", "d", Some(3)),
        ];
        let opts = FlowLayoutOptions::default();
        let first = flow_layout(&items, &links, &opts);
        for _ in 0..3 {
            assert_eq!(flow_layout(&items, &links, &opts), first);
        }
        // a -> b -> c is one straight line: each aims for the middle of its source.
        assert_eq!(first["a"].y, first["b"].y);
        assert_eq!(first["b"].y, first["c"].y);
        // d shares b's column and goes under it rather than on top of it.
        assert_eq!(first["d"].x, first["b"].x);
        assert!(first["d"].y >= first["b"].y + 50 + opts.row_gap);
    }

    #[test]
    fn grouping_boxes_become_lanes_that_never_overlap() {
        let mut items: Vec<FlowItem> = ["client", "api", "queue", "worker", "store"]
            .iter()
            .enumerate()
            .map(|(i, id)| item(id, i as i32 * 10))
            .collect();
        // client: no box. api + worker: "compute". queue + store: "data".
        items[1].groups = vec!["compute".into()];
        items[3].groups = vec!["compute".into()];
        items[2].groups = vec!["data".into()];
        items[4].groups = vec!["data".into()];
        let links = vec![
            link("client", "api", Some(1)),
            link("api", "queue", Some(2)),
            link("queue", "worker", Some(3)),
            link("worker", "store", Some(4)),
        ];
        let opts = FlowLayoutOptions::default();
        let at = flow_layout(&items, &links, &opts);
        // Lanes in the order they first take part: no box (step 1), compute, data.
        let band = |ids: &[&str]| {
            let top = ids.iter().map(|i| at[*i].y).min().unwrap();
            let bottom = ids.iter().map(|i| at[*i].y + 50).max().unwrap();
            (top, bottom)
        };
        let (_, client_bottom) = band(&["client"]);
        let (compute_top, compute_bottom) = band(&["api", "worker"]);
        let (data_top, _) = band(&["queue", "store"]);
        // Room between lanes for the boxes' padding and label strips.
        let label = view::GROUP_LABEL_H as i32;
        assert!(compute_top - client_bottom >= GROUP_PAD + label, "{at:?}");
        assert!(data_top - compute_bottom >= 2 * GROUP_PAD + label, "{at:?}");
        // Still left to right by step.
        assert!(at["client"].x < at["api"].x && at["api"].x < at["queue"].x);
        assert!(at["queue"].x < at["worker"].x && at["worker"].x < at["store"].x);
    }
}
