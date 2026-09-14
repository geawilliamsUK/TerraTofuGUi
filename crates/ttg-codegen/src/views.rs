//! What a saved view shows, and how it reads as a document.
//!
//! [`visible_set`] is the one answer to "which entities does this filter show" — the
//! canvas, the view bar and the exported document all ask it, so a Markdown export
//! lists exactly what the user sees. [`markdown`] and [`mermaid`] turn a view's
//! annotations into a page: the description, the grouping boxes with their members, the
//! data flows in step order, and the notes.

use std::collections::{BTreeSet, VecDeque};
use ttg_catalog::Catalog;
use ttg_core::view::{self, Rect};
use ttg_core::{glob_match, FlowEnd, Id, Origin, Project, View, ViewFilter};

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
    if !filter.types.is_empty() {
        vis.retain(|id| {
            p.entity(id)
                .is_some_and(|e| filter.types.contains(e.resource_type))
        });
    }
    match filter.origin {
        Origin::All => {}
        Origin::Curated => {
            vis.retain(|id| p.entity(id).is_some_and(|e| !Catalog::is_native(e.resource_type)))
        }
        Origin::Native => vis.retain(|id| p.entity(id).is_some_and(|e| Catalog::is_native(e.resource_type))),
    }
    if !filter.providers.is_empty() {
        let on: BTreeSet<Id> = filter
            .providers
            .iter()
            .flat_map(|prov| crate::layers::members(p, cat, prov))
            .collect();
        vis.retain(|id| on.contains(id));
    }
    if !filter.name_glob.is_empty() {
        vis.retain(|id| {
            p.entity(id)
                .is_some_and(|e| glob_match(&filter.name_glob, e.name))
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

/// The view's visibility as a predicate, for the geometry helpers.
fn visible_fn(p: &Project, cat: &Catalog, v: &View) -> impl Fn(&str) -> bool {
    let set = visible_set(p, cat, &v.filter);
    move |id: &str| set.as_ref().is_none_or(|s| s.contains(id))
}

/// Everything the document needs to know about one group.
struct GroupDoc {
    id: Id,
    label: String,
    parent: Option<Id>,
    members: Vec<String>,
    member_ids: Vec<Id>,
}

fn group_docs(p: &Project, v: &View, visible: &dyn Fn(&str) -> bool) -> Vec<GroupDoc> {
    view::groups_by_area(v)
        .into_iter()
        .map(|gid| {
            let member_ids = view::group_members(p, v, &gid, visible);
            let logicals = view::group_logicals(v, &gid);
            let members = member_ids
                .iter()
                .map(|m| {
                    p.entity(m)
                        .map(|e| e.name.to_string())
                        .unwrap_or_else(|| m.clone())
                })
                .chain(
                    logicals
                        .iter()
                        .filter_map(|l| v.logical(l).map(|l| format!("{} (logical)", l.name))),
                )
                .collect();
            GroupDoc {
                label: v.group(&gid).map(|g| g.label.clone()).unwrap_or_default(),
                parent: view::group_parent(v, &gid).map(|g| g.id.clone()),
                id: gid,
                members,
                member_ids,
            }
        })
        .collect()
}

/// The view as a Markdown page: description, groups and their members, the flows in
/// step order, then the notes.
pub fn markdown(p: &Project, cat: &Catalog, v: &View) -> String {
    let visible = visible_fn(p, cat, v);
    let mut s = format!("# {} — {}\n\n", p.name, v.name);
    if !v.description.is_empty() {
        s.push_str(&v.description);
        s.push_str("\n\n");
    }
    let groups = group_docs(p, v, &visible);
    if !groups.is_empty() {
        s.push_str("## Groups\n\n| Group | Inside | Members |\n|---|---|---|\n");
        for g in &groups {
            let parent = g
                .parent
                .as_ref()
                .and_then(|pid| v.group(pid))
                .map(|g| g.label.clone())
                .unwrap_or_else(|| "—".into());
            s.push_str(&format!(
                "| {} | {} | {} |\n",
                g.label,
                parent,
                if g.members.is_empty() {
                    "—".to_string()
                } else {
                    g.members.join(", ")
                }
            ));
        }
        s.push('\n');
    }
    if !v.logicals.is_empty() {
        s.push_str("## Logical nodes\n\nAnnotation only — nothing is generated for these.\n\n");
        for l in &v.logicals {
            let sub = if l.subtitle.is_empty() {
                String::new()
            } else {
                format!(" — {}", l.subtitle)
            };
            s.push_str(&format!("- **{}**{sub}\n", l.name));
        }
        s.push('\n');
    }
    let flows = v.flows_in_step_order();
    if !flows.is_empty() {
        s.push_str("## Data flow\n\n");
        for (i, f) in flows.iter().enumerate() {
            let n = f.step.unwrap_or(i as u32 + 1);
            let label = if f.label.is_empty() {
                "flow"
            } else {
                f.label.as_str()
            };
            let kind = if f.dashed { " *(optional / async)*" } else { "" };
            s.push_str(&format!(
                "{n}. **{} → {}** — {label}{kind}\n",
                view::end_name(p, v, &f.from),
                view::end_name(p, v, &f.to)
            ));
        }
        s.push('\n');
    }
    if !v.notes.is_empty() {
        s.push_str("## Notes\n\n");
        for n in &v.notes {
            let title = if n.title.is_empty() {
                "Note"
            } else {
                n.title.as_str()
            };
            let about = n
                .anchor
                .as_ref()
                .map(|a| format!(" *(on {})*", anchor_name(p, v, a)))
                .unwrap_or_default();
            s.push_str(&format!("### {title}{about}\n\n{}\n\n", n.body));
        }
    }
    s
}

fn anchor_name(p: &Project, v: &View, a: &ttg_core::NoteAnchor) -> String {
    match a {
        ttg_core::NoteAnchor::End(e) => view::end_name(p, v, e),
        ttg_core::NoteAnchor::Flow { flow } => v
            .flow(flow)
            .map(|f| {
                if f.label.is_empty() {
                    "a data flow".to_string()
                } else {
                    format!("the \"{}\" flow", f.label)
                }
            })
            .unwrap_or_else(|| flow.clone()),
    }
}

/// A Mermaid `flowchart LR` of the same picture: groups as subgraphs, logical nodes
/// dashed, flows as labelled edges.
pub fn mermaid(p: &Project, cat: &Catalog, v: &View) -> String {
    let visible = visible_fn(p, cat, v);
    let groups = group_docs(p, v, &visible);
    let mut s = String::from("flowchart LR\n");
    // Entities and logicals that sit in a box are declared inside it; the rest follow.
    let mut placed: BTreeSet<Id> = BTreeSet::new();
    for g in &groups {
        placed.extend(g.member_ids.iter().cloned());
        placed.extend(view::group_logicals(v, &g.id));
    }
    let roots: Vec<&GroupDoc> = groups.iter().filter(|g| g.parent.is_none()).collect();
    for g in roots {
        emit_group(p, v, &groups, g, 1, &mut s);
    }
    for e in p.entities() {
        if visible(e.id) && !placed.contains(e.id) {
            s.push_str(&format!("    {}\n", node_line(e.id, e.name, false)));
        }
    }
    for l in &v.logicals {
        if !placed.contains(&l.id) {
            s.push_str(&format!("    {}\n", node_line(&l.id, &l.name, true)));
        }
    }
    for f in v.flows_in_step_order() {
        let (Some(a), Some(b)) = (mermaid_end(v, &f.from), mermaid_end(v, &f.to)) else {
            continue;
        };
        let arrow = if f.dashed { "-.->" } else { "-->" };
        let label = match (f.step, f.label.as_str()) {
            (Some(n), "") => format!("{n}"),
            (Some(n), l) => format!("{n}. {l}"),
            (None, "") => String::new(),
            (None, l) => l.to_string(),
        };
        if label.is_empty() {
            s.push_str(&format!("    {a} {arrow} {b}\n"));
        } else {
            s.push_str(&format!("    {a} {arrow}|{}| {b}\n", escape(&label)));
        }
    }
    if !v.logicals.is_empty() {
        s.push_str("    classDef logical stroke-dasharray:4 3,fill:#f2f2f2,color:#555,stroke:#999;\n");
        let ids: Vec<String> = v.logicals.iter().map(|l| mermaid_id(&l.id)).collect();
        s.push_str(&format!("    class {} logical;\n", ids.join(",")));
    }
    s
}

fn emit_group(p: &Project, v: &View, all: &[GroupDoc], g: &GroupDoc, depth: usize, s: &mut String) {
    let pad = "    ".repeat(depth);
    s.push_str(&format!(
        "{pad}subgraph {}[\"{}\"]\n",
        mermaid_id(&g.id),
        escape(&g.label)
    ));
    for m in &g.member_ids {
        let name = p
            .entity(m)
            .map(|e| e.name.to_string())
            .unwrap_or_else(|| m.clone());
        s.push_str(&format!("{pad}    {}\n", node_line(m, &name, false)));
    }
    for l in view::group_logicals(v, &g.id) {
        if let Some(l) = v.logical(&l) {
            s.push_str(&format!("{pad}    {}\n", node_line(&l.id, &l.name, true)));
        }
    }
    for child in all.iter().filter(|c| c.parent.as_deref() == Some(g.id.as_str())) {
        emit_group(p, v, all, child, depth + 1, s);
    }
    s.push_str(&format!("{pad}end\n"));
}

fn node_line(id: &str, name: &str, logical: bool) -> String {
    let n = escape(name);
    if logical {
        format!("{}([\"{n}\"])", mermaid_id(id))
    } else {
        format!("{}[\"{n}\"]", mermaid_id(id))
    }
}

fn mermaid_end(v: &View, e: &FlowEnd) -> Option<String> {
    match e {
        FlowEnd::Entity { entity } => Some(mermaid_id(entity)),
        FlowEnd::Group { group } => v.group(group).map(|g| mermaid_id(&g.id)),
        FlowEnd::Logical { logical } => v.logical(logical).map(|l| mermaid_id(&l.id)),
    }
}

/// A Mermaid-safe node id (`fn-gateway` would be read as an arrow).
fn mermaid_id(id: &str) -> String {
    format!("n_{}", ttg_core::slugify(id))
}

/// Quotes and brackets confuse Mermaid's label parser.
fn escape(s: &str) -> String {
    s.replace('"', "'").replace(['[', ']', '|'], "")
}

/// Bounding box of everything a view draws, for "fit to the view".
pub fn view_bounds(p: &Project, cat: &Catalog, v: &View) -> Option<Rect> {
    let visible = visible_fn(p, cat, v);
    let mut r: Option<Rect> = None;
    let mut add = |x: Rect| r = Some(r.map_or(x, |b: Rect| b.union(x)));
    for e in p.entities() {
        if visible(e.id) {
            if let Some(er) = view::entity_rect(p, Some(v), e.id, &visible) {
                add(er);
            }
        }
    }
    for g in &v.groups {
        add(view::group_rect(g));
    }
    for l in &v.logicals {
        add(view::logical_rect(l));
    }
    for n in &v.notes {
        add(view::note_rect(p, v, n, &visible));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job_pipeline() -> Project {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/job-pipeline.ttg.json");
        ttg_core::project::load(&p).expect("example loads")
    }

    fn data_flow_view(p: &Project) -> View {
        p.views
            .iter()
            .find(|v| v.name == "Data flow")
            .expect("the example has a Data flow view")
            .clone()
    }

    #[test]
    fn markdown_documents_the_example_view() {
        let p = job_pipeline();
        let v = data_flow_view(&p);
        let md = markdown(&p, &Catalog::builtin(), &v);
        assert!(md.starts_with("# "), "{md}");
        assert!(md.contains("## Groups"), "{md}");
        // The geometric membership is reported, not a stored list.
        assert!(
            md.contains("Ingress (public)") && md.contains("JobGateway"),
            "{md}"
        );
        assert!(md.contains("## Data flow"), "{md}");
        assert!(md.contains("job requests"), "{md}");
        // Containers are filtered out of this view, so none of them are listed.
        assert!(!md.contains("| core |"), "{md}");
    }

    #[test]
    fn mermaid_nests_groups_and_labels_flows() {
        let p = job_pipeline();
        let v = data_flow_view(&p);
        let mm = mermaid(&p, &Catalog::builtin(), &v);
        assert!(mm.starts_with("flowchart LR"), "{mm}");
        assert!(
            mm.contains("subgraph n_grp_ingress[\"Ingress (public)\"]"),
            "{mm}"
        );
        assert!(mm.contains("-->"), "{mm}");
        assert!(mm.contains("-.->"), "{mm}");
        // Ids are slugified, so no raw entity id (which may contain `-`) leaks into the
        // graph, where Mermaid would read it as an arrow.
        assert!(!mm.contains("fn-gateway") && !mm.contains("grp-ingress"), "{mm}");
        assert!(mm.contains("n_fn_gateway"), "{mm}");
    }

    #[test]
    fn the_name_glob_narrows_the_visible_set() {
        let p = job_pipeline();
        let cat = Catalog::builtin();
        let f = ViewFilter {
            name_glob: "job*".into(),
            containers: false,
            ..Default::default()
        };
        let vis = visible_set(&p, &cat, &f).expect("a filter is set");
        let names: Vec<String> = vis
            .iter()
            .filter_map(|id| p.entity(id).map(|e| e.name.to_lowercase()))
            .collect();
        assert!(!names.is_empty());
        assert!(names.iter().all(|n| n.starts_with("job")), "{names:?}");
    }
}
