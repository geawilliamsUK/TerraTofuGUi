//! Architecture diff: what changed between two versions of a project, in the terms of
//! the diagram rather than of the file — entities added, removed, renamed or moved
//! between containers, their fields and provider fields (old → new), links added and
//! removed, saved views and their annotations, and project settings.
//!
//! Entities are matched by id first; an entity whose id changed (deleted and drawn
//! again, pasted, imported) is matched by its name and type instead. Canvas geometry
//! (positions, sizes, per-view layouts) is not compared: it changes nothing that is
//! generated and every drag would show up otherwise.
//!
//! [`diff`] returns the structure (serializable as JSON); [`ProjectDiff::to_markdown`]
//! renders it for a pull-request description.

use crate::ir::{Edge, Project, View};
use serde::Serialize;
use serde_json::Value as J;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// One value that differs. `old` / `new` are `None` when the value is absent on that
/// side (added or removed).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldChange {
    /// Dotted path, e.g. `config.replicas`, `provider_config.aws.bucket_name`,
    /// `extra.aws.main.force_destroy`, `settings.tags.Project`.
    pub path: String,
    pub old: Option<J>,
    pub new: Option<J>,
}

/// An entity as listed among the added or removed ones.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EntitySummary {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub type_id: String,
    pub container: bool,
    /// Display name of the container it is drawn in.
    pub parent: Option<String>,
}

/// A containment change: display names of the old and new container (`None` = top level).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Move {
    pub from: Option<String>,
    pub to: Option<String>,
}

/// An entity present in both versions with something changed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EntityChange {
    /// Id in the new version.
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub type_id: String,
    /// Its id in the old version, when it was matched by name and type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id_was: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renamed_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moved: Option<Move>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldChange>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct EntityDiff {
    pub added: Vec<EntitySummary>,
    pub removed: Vec<EntitySummary>,
    pub changed: Vec<EntityChange>,
}

/// A link, with the display names of its ends.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LinkSummary {
    pub source: String,
    pub target: String,
    pub source_id: String,
    pub target_id: String,
    pub relation: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LinkChange {
    pub link: LinkSummary,
    pub fields: Vec<FieldChange>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LinkDiff {
    pub added: Vec<LinkSummary>,
    pub removed: Vec<LinkSummary>,
    /// Same ends and relation, different provider layers.
    pub changed: Vec<LinkChange>,
}

/// A group, flow, note or logical node of a view that was added, removed or changed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnnotationChange {
    /// `group`, `flow`, `note` or `logical`.
    pub kind: String,
    /// `added`, `removed` or `changed`.
    pub change: String,
    pub id: String,
    /// Label, title or name.
    pub label: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldChange>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ViewChange {
    pub name: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldChange>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<AnnotationChange>,
    /// Entities placed differently in the view's own layout (positions are not listed).
    #[serde(skip_serializing_if = "is_zero")]
    pub layout_moves: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ViewsDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<ViewChange>,
}

/// Everything that differs between two projects.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ProjectDiff {
    /// The project's name, when it changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<FieldChange>,
    pub entities: EntityDiff,
    pub links: LinkDiff,
    pub views: ViewsDiff,
    pub settings: Vec<FieldChange>,
}

impl ProjectDiff {
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.entities.added.is_empty()
            && self.entities.removed.is_empty()
            && self.entities.changed.is_empty()
            && self.links.added.is_empty()
            && self.links.removed.is_empty()
            && self.links.changed.is_empty()
            && self.views.added.is_empty()
            && self.views.removed.is_empty()
            && self.views.changed.is_empty()
            && self.settings.is_empty()
    }

    /// One line: "entities: 2 added, 1 changed; links: 3 added; views: 1 changed;
    /// settings changed", or "no architecture changes".
    pub fn summary(&self) -> String {
        if self.is_empty() {
            return "no architecture changes".into();
        }
        let mut groups = Vec::new();
        let mut group = |noun: &str, parts: [(usize, &str); 3]| {
            let counted: Vec<String> = parts
                .iter()
                .filter(|(n, _)| *n > 0)
                .map(|(n, w)| format!("{n} {w}"))
                .collect();
            if !counted.is_empty() {
                groups.push(format!("{noun}: {}", counted.join(", ")));
            }
        };
        let e = &self.entities;
        group(
            "entities",
            [
                (e.added.len(), "added"),
                (e.removed.len(), "removed"),
                (e.changed.len(), "changed"),
            ],
        );
        let l = &self.links;
        group(
            "links",
            [
                (l.added.len(), "added"),
                (l.removed.len(), "removed"),
                (l.changed.len(), "changed"),
            ],
        );
        let v = &self.views;
        group(
            "views",
            [
                (v.added.len(), "added"),
                (v.removed.len(), "removed"),
                (v.changed.len(), "changed"),
            ],
        );
        if !self.settings.is_empty() || self.name.is_some() {
            groups.push("settings changed".into());
        }
        groups.join("; ")
    }

    /// The diff as Markdown for a pull-request description: a summary line, then tables
    /// and lists per kind of change. `type_label` turns an abstract type id into the name
    /// to show (the catalog's display name); `from` / `to` name the two versions.
    pub fn to_markdown(&self, from: &str, to: &str, type_label: &dyn Fn(&str) -> String) -> String {
        let mut s = format!("## Architecture changes: {from} \u{2192} {to}\n\n");
        s.push_str(&format!("{}.\n", capitalise(&self.summary())));
        if self.is_empty() {
            return s;
        }
        if let Some(n) = &self.name {
            s.push_str(&format!(
                "\nProject renamed: {} \u{2192} {}.\n",
                value(&n.old),
                value(&n.new)
            ));
        }
        let e = &self.entities;
        if !e.added.is_empty() {
            s.push_str(&format!(
                "\n### Added ({})\n\n| Entity | Type | Inside |\n|---|---|---|\n",
                e.added.len()
            ));
            for a in &e.added {
                s.push_str(&format!(
                    "| {} | {} | {} |\n",
                    cell(&a.name),
                    cell(&type_label(&a.type_id)),
                    cell(a.parent.as_deref().unwrap_or("\u{2014}"))
                ));
            }
        }
        if !e.removed.is_empty() {
            s.push_str(&format!(
                "\n### Removed ({})\n\n| Entity | Type | Was inside |\n|---|---|---|\n",
                e.removed.len()
            ));
            for r in &e.removed {
                s.push_str(&format!(
                    "| {} | {} | {} |\n",
                    cell(&r.name),
                    cell(&type_label(&r.type_id)),
                    cell(r.parent.as_deref().unwrap_or("\u{2014}"))
                ));
            }
        }
        if !e.changed.is_empty() {
            s.push_str(&format!("\n### Changed ({})\n\n", e.changed.len()));
            for c in &e.changed {
                s.push_str(&format!("- **{}** ({})\n", c.name, type_label(&c.type_id)));
                if let Some(old) = &c.renamed_from {
                    s.push_str(&format!("  - renamed from \"{old}\"\n"));
                }
                if let Some(was) = &c.id_was {
                    s.push_str(&format!(
                        "  - id `{was}` \u{2192} `{}` (matched by name and type)\n",
                        c.id
                    ));
                }
                if let Some(m) = &c.moved {
                    s.push_str(&format!(
                        "  - moved from {} to {}\n",
                        place(m.from.as_deref()),
                        place(m.to.as_deref())
                    ));
                }
                for f in &c.fields {
                    s.push_str(&format!("  - {}\n", field_line(f)));
                }
            }
        }
        let l = &self.links;
        if !l.added.is_empty() || !l.removed.is_empty() || !l.changed.is_empty() {
            s.push_str("\n### Links\n\n");
            for a in &l.added {
                s.push_str(&format!("- added: {}\n", link_line(a)));
            }
            for r in &l.removed {
                s.push_str(&format!("- removed: {}\n", link_line(r)));
            }
            for c in &l.changed {
                let fields: Vec<String> = c.fields.iter().map(field_line).collect();
                s.push_str(&format!(
                    "- changed: {} ({})\n",
                    link_line(&c.link),
                    fields.join("; ")
                ));
            }
        }
        let v = &self.views;
        if !v.added.is_empty() || !v.removed.is_empty() || !v.changed.is_empty() {
            s.push_str("\n### Views\n\n");
            for a in &v.added {
                s.push_str(&format!("- added: \"{a}\"\n"));
            }
            for r in &v.removed {
                s.push_str(&format!("- removed: \"{r}\"\n"));
            }
            for c in &v.changed {
                s.push_str(&format!("- changed: \"{}\"\n", c.name));
                for f in &c.fields {
                    s.push_str(&format!("  - {}\n", field_line(f)));
                }
                for a in &c.annotations {
                    let label = if a.label.is_empty() {
                        a.id.clone()
                    } else {
                        format!("\"{}\"", a.label)
                    };
                    let detail: Vec<String> = a.fields.iter().map(field_line).collect();
                    if detail.is_empty() {
                        s.push_str(&format!("  - {} {label} {}\n", a.kind, a.change));
                    } else {
                        s.push_str(&format!(
                            "  - {} {label} {}: {}\n",
                            a.kind,
                            a.change,
                            detail.join("; ")
                        ));
                    }
                }
                if c.layout_moves > 0 {
                    s.push_str(&format!(
                        "  - {} item(s) placed differently in the view's own layout\n",
                        c.layout_moves
                    ));
                }
            }
        }
        if !self.settings.is_empty() {
            s.push_str("\n### Settings\n\n");
            for f in &self.settings {
                s.push_str(&format!("- {}\n", field_line(f)));
            }
        }
        s
    }
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn place(p: Option<&str>) -> String {
    match p {
        Some(n) => format!("\"{n}\""),
        None => "the top level".into(),
    }
}

fn link_line(l: &LinkSummary) -> String {
    format!(
        "{} \u{2192} {} ({})",
        l.source,
        l.target,
        l.relation.replace('_', " ")
    )
}

/// A value for a Markdown line: compact JSON, cut to 80 characters.
fn value(v: &Option<J>) -> String {
    match v {
        None => "(none)".into(),
        Some(v) => {
            let t = v.to_string();
            let t = if t.chars().count() > 80 {
                format!("{}\u{2026}", t.chars().take(79).collect::<String>())
            } else {
                t
            };
            format!("`{}`", t.replace('`', "'"))
        }
    }
}

fn field_line(f: &FieldChange) -> String {
    match (&f.old, &f.new) {
        (None, Some(_)) => format!("`{}`: set to {}", f.path, value(&f.new)),
        (Some(_), None) => format!("`{}`: removed (was {})", f.path, value(&f.old)),
        _ => format!("`{}`: {} \u{2192} {}", f.path, value(&f.old), value(&f.new)),
    }
}

fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

/// Flatten a JSON value into `path -> leaf` (objects are recursed, everything else —
/// lists included — is a leaf).
fn flatten(prefix: &str, v: &J, out: &mut BTreeMap<String, J>) {
    match v {
        J::Object(o) if !o.is_empty() => {
            for (k, x) in o {
                flatten(&format!("{prefix}.{k}"), x, out);
            }
        }
        J::Object(_) => {}
        other => {
            out.insert(prefix.to_string(), other.clone());
        }
    }
}

/// Every leaf that differs between two JSON values, as field changes under `prefix`.
fn json_changes(prefix: &str, old: &J, new: &J) -> Vec<FieldChange> {
    let mut a = BTreeMap::new();
    let mut b = BTreeMap::new();
    flatten(prefix, old, &mut a);
    flatten(prefix, new, &mut b);
    let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    keys.into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        .map(|k| FieldChange {
            path: k.clone(),
            old: a.get(k).cloned(),
            new: b.get(k).cloned(),
        })
        .collect()
}

fn to_json<T: Serialize>(t: &T) -> J {
    serde_json::to_value(t).unwrap_or(J::Null)
}

/// The parts of an entity that are compared, as JSON under their path names.
fn entity_fields(p: &Project, id: &str) -> J {
    let e = p.entity(id).expect("entity exists");
    let providers = match p.nodes.get(id) {
        Some(n) => &n.providers,
        None => &p.containers[id].providers,
    };
    let mut o = serde_json::Map::new();
    o.insert("type".into(), J::String(e.resource_type.to_string()));
    o.insert("config".into(), to_json(e.config));
    o.insert("provider_config".into(), to_json(e.provider_config));
    o.insert("extra".into(), to_json(e.extra));
    if let Some(a) = p.adoption(id) {
        o.insert("adopt".into(), to_json(a));
    }
    // Per-environment values and presence: `overrides.prod.config.replicas`,
    // `overrides.pilot.absent`.
    if let Some(ov) = p.overrides_of(id) {
        o.insert("overrides".into(), to_json(ov));
    }
    if e.manual {
        o.insert("manual".into(), J::Bool(true));
    }
    if !providers.is_empty() {
        o.insert("providers".into(), to_json(providers));
    }
    if let Some(c) = e.classification {
        o.insert("classification".into(), J::String(c.key().into()));
    }
    if !e.description.is_empty() {
        o.insert("description".into(), J::String(e.description.to_string()));
    }
    if !e.owner.is_empty() {
        o.insert("owner".into(), J::String(e.owner.to_string()));
    }
    J::Object(o)
}

fn summary_of(p: &Project, id: &str) -> EntitySummary {
    let e = p.entity(id).expect("entity exists");
    EntitySummary {
        id: id.to_string(),
        name: e.name.to_string(),
        type_id: e.resource_type.to_string(),
        container: e.is_container,
        parent: e.parent.and_then(|x| p.entity(x)).map(|x| x.name.to_string()),
    }
}

fn name_of(p: &Project, id: &str) -> String {
    p.entity(id)
        .map(|e| e.name.to_string())
        .unwrap_or_else(|| id.to_string())
}

/// Old id -> new id for every entity present in both versions: by id, then, among the
/// rest, by name (compared as slugs) and type when that pairs them one to one.
fn match_entities(old: &Project, new: &Project) -> (HashMap<String, String>, Vec<(String, String)>) {
    let mut map = HashMap::new();
    let mut by_name = Vec::new();
    let mut old_rest = Vec::new();
    for e in old.entities() {
        if new.contains(e.id) {
            map.insert(e.id.to_string(), e.id.to_string());
        } else {
            old_rest.push(e);
        }
    }
    let new_rest: Vec<_> = new
        .entities()
        .into_iter()
        .filter(|e| !old.contains(e.id))
        .collect();
    let key = |name: &str, t: &str| (crate::slugify(name), t.to_string());
    let mut new_by_key: HashMap<(String, String), Vec<String>> = HashMap::new();
    for e in &new_rest {
        new_by_key
            .entry(key(e.name, e.resource_type))
            .or_default()
            .push(e.id.to_string());
    }
    let mut old_by_key: HashMap<(String, String), usize> = HashMap::new();
    for e in &old_rest {
        *old_by_key.entry(key(e.name, e.resource_type)).or_default() += 1;
    }
    for e in &old_rest {
        let k = key(e.name, e.resource_type);
        if old_by_key.get(&k) == Some(&1) {
            if let Some(ids) = new_by_key.get(&k) {
                if ids.len() == 1 {
                    map.insert(e.id.to_string(), ids[0].clone());
                    by_name.push((e.id.to_string(), ids[0].clone()));
                }
            }
        }
    }
    (map, by_name)
}

/// What changed from `old` to `new`.
pub fn diff(old: &Project, new: &Project) -> ProjectDiff {
    let (map, by_name) = match_entities(old, new);
    let matched_new: BTreeSet<&String> = map.values().collect();
    let mut out = ProjectDiff::default();
    if old.name != new.name {
        out.name = Some(FieldChange {
            path: "name".into(),
            old: Some(J::String(old.name.clone())),
            new: Some(J::String(new.name.clone())),
        });
    }

    // Entities, in the order `entities()` gives them (containers outer to inner, then
    // nodes, each by id), so the output is stable.
    for e in new.entities() {
        if !matched_new.contains(&e.id.to_string()) {
            out.entities.added.push(summary_of(new, e.id));
        }
    }
    for e in old.entities() {
        let Some(nid) = map.get(e.id) else {
            out.entities.removed.push(summary_of(old, e.id));
            continue;
        };
        let n = new.entity(nid).unwrap();
        let fields = json_changes("", &entity_fields(old, e.id), &entity_fields(new, nid))
            .into_iter()
            .map(|mut f| {
                f.path = f.path.trim_start_matches('.').to_string();
                f
            })
            .collect::<Vec<_>>();
        let old_parent = e
            .parent
            .map(|p| map.get(p).cloned().unwrap_or_else(|| format!("(removed) {p}")));
        let moved = (old_parent.as_deref() != n.parent).then(|| Move {
            from: e.parent.map(|p| name_of(old, p)),
            to: n.parent.map(|p| name_of(new, p)),
        });
        let renamed_from = (e.name != n.name).then(|| e.name.to_string());
        let id_was = by_name.iter().find(|(o, _)| o == e.id).map(|(o, _)| o.clone());
        if fields.is_empty() && moved.is_none() && renamed_from.is_none() && id_was.is_none() {
            continue;
        }
        out.entities.changed.push(EntityChange {
            id: nid.clone(),
            name: n.name.to_string(),
            type_id: n.resource_type.to_string(),
            id_was,
            renamed_from,
            moved,
            fields,
        });
    }

    // Links, compared with old ends mapped to their new ids.
    let link_key =
        |e: &Edge, src: &str, tgt: &str| (src.to_string(), tgt.to_string(), e.relation.key().to_string());
    let summary = |p: &Project, s: &str, t: &str, rel: &str| LinkSummary {
        source: name_of(p, s),
        target: name_of(p, t),
        source_id: s.to_string(),
        target_id: t.to_string(),
        relation: rel.to_string(),
    };
    let mut old_links: BTreeMap<(String, String, String), &Edge> = BTreeMap::new();
    for e in &old.edges {
        let s = map
            .get(&e.source)
            .cloned()
            .unwrap_or_else(|| format!("\u{0}{}", e.source));
        let t = map
            .get(&e.target)
            .cloned()
            .unwrap_or_else(|| format!("\u{0}{}", e.target));
        old_links.insert(link_key(e, &s, &t), e);
    }
    let mut new_links: BTreeMap<(String, String, String), &Edge> = BTreeMap::new();
    for e in &new.edges {
        new_links.insert(link_key(e, &e.source, &e.target), e);
    }
    for (k, e) in &new_links {
        match old_links.get(k) {
            None => out.links.added.push(summary(new, &e.source, &e.target, &k.2)),
            Some(o) => {
                // The provider layers and environments a link belongs to.
                let mut fields = Vec::new();
                if o.providers != e.providers {
                    fields.push(FieldChange {
                        path: "providers".into(),
                        old: Some(to_json(&o.providers)),
                        new: Some(to_json(&e.providers)),
                    });
                }
                if o.environments != e.environments {
                    fields.push(FieldChange {
                        path: "environments".into(),
                        old: Some(to_json(&o.environments)),
                        new: Some(to_json(&e.environments)),
                    });
                }
                if !fields.is_empty() {
                    out.links.changed.push(LinkChange {
                        link: summary(new, &e.source, &e.target, &k.2),
                        fields,
                    });
                }
            }
        }
    }
    for (k, e) in &old_links {
        if !new_links.contains_key(k) {
            out.links.removed.push(summary(old, &e.source, &e.target, &k.2));
        }
    }

    // Views, matched by name.
    for v in &new.views {
        if !old.views.iter().any(|o| o.name == v.name) {
            out.views.added.push(v.name.clone());
        }
    }
    for o in &old.views {
        match new.views.iter().find(|v| v.name == o.name) {
            None => out.views.removed.push(o.name.clone()),
            Some(v) => {
                if let Some(c) = view_change(o, v) {
                    out.views.changed.push(c);
                }
            }
        }
    }

    out.settings = json_changes("settings", &to_json(&old.settings), &to_json(&new.settings));
    out
}

fn view_change(o: &View, v: &View) -> Option<ViewChange> {
    let mut fields = Vec::new();
    if o.description != v.description {
        fields.push(FieldChange {
            path: "description".into(),
            old: Some(J::String(o.description.clone())),
            new: Some(J::String(v.description.clone())),
        });
    }
    if o.legend != v.legend {
        fields.push(FieldChange {
            path: "legend".into(),
            old: Some(J::Bool(o.legend)),
            new: Some(J::Bool(v.legend)),
        });
    }
    fields.extend(json_changes("filter", &to_json(&o.filter), &to_json(&v.filter)));
    if o.layout.is_some() != v.layout.is_some() {
        fields.push(FieldChange {
            path: "own_layout".into(),
            old: Some(J::Bool(o.layout.is_some())),
            new: Some(J::Bool(v.layout.is_some())),
        });
    }
    let layout_moves = match (&o.layout, &v.layout) {
        (Some(a), Some(b)) => {
            let ids: BTreeSet<&String> = a.positions.keys().chain(b.positions.keys()).collect();
            ids.into_iter()
                .filter(|id| a.positions.get(*id) != b.positions.get(*id))
                .count()
        }
        _ => 0,
    };
    let mut annotations = Vec::new();
    // Geometry is left out, as for entities: what an annotation says and connects is
    // what a reviewer reads.
    let strip = |mut j: J| {
        if let J::Object(o) = &mut j {
            o.remove("position");
            o.remove("size");
            o.remove("id");
        }
        j
    };
    annotation_changes(
        "group",
        &o.groups
            .iter()
            .map(|g| (g.id.clone(), g.label.clone(), strip(to_json(g))))
            .collect::<Vec<_>>(),
        &v.groups
            .iter()
            .map(|g| (g.id.clone(), g.label.clone(), strip(to_json(g))))
            .collect::<Vec<_>>(),
        &mut annotations,
    );
    annotation_changes(
        "flow",
        &o.flows
            .iter()
            .map(|f| (f.id.clone(), f.label.clone(), strip(to_json(f))))
            .collect::<Vec<_>>(),
        &v.flows
            .iter()
            .map(|f| (f.id.clone(), f.label.clone(), strip(to_json(f))))
            .collect::<Vec<_>>(),
        &mut annotations,
    );
    annotation_changes(
        "note",
        &o.notes
            .iter()
            .map(|n| (n.id.clone(), n.title.clone(), strip(to_json(n))))
            .collect::<Vec<_>>(),
        &v.notes
            .iter()
            .map(|n| (n.id.clone(), n.title.clone(), strip(to_json(n))))
            .collect::<Vec<_>>(),
        &mut annotations,
    );
    annotation_changes(
        "logical",
        &o.logicals
            .iter()
            .map(|l| (l.id.clone(), l.name.clone(), strip(to_json(l))))
            .collect::<Vec<_>>(),
        &v.logicals
            .iter()
            .map(|l| (l.id.clone(), l.name.clone(), strip(to_json(l))))
            .collect::<Vec<_>>(),
        &mut annotations,
    );
    (!fields.is_empty() || !annotations.is_empty() || layout_moves > 0).then(|| ViewChange {
        name: v.name.clone(),
        fields,
        annotations,
        layout_moves,
    })
}

type Ann = (String, String, J);

fn annotation_changes(kind: &str, old: &[Ann], new: &[Ann], out: &mut Vec<AnnotationChange>) {
    for (id, label, j) in new {
        match old.iter().find(|(o, _, _)| o == id) {
            None => out.push(AnnotationChange {
                kind: kind.into(),
                change: "added".into(),
                id: id.clone(),
                label: label.clone(),
                fields: Vec::new(),
            }),
            Some((_, _, oj)) => {
                let fields = json_changes("", oj, j)
                    .into_iter()
                    .map(|mut f| {
                        f.path = f.path.trim_start_matches('.').to_string();
                        f
                    })
                    .collect::<Vec<_>>();
                if !fields.is_empty() {
                    out.push(AnnotationChange {
                        kind: kind.into(),
                        change: "changed".into(),
                        id: id.clone(),
                        label: label.clone(),
                        fields,
                    });
                }
            }
        }
    }
    for (id, label, _) in old {
        if !new.iter().any(|(n, _, _)| n == id) {
            out.push(AnnotationChange {
                kind: kind.into(),
                change: "removed".into(),
                id: id.clone(),
                label: label.clone(),
                fields: Vec::new(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::*;
    use crate::Value;

    fn node(id: &str, name: &str, t: &str, parent: Option<&str>) -> Node {
        Node {
            id: id.into(),
            name: name.into(),
            resource_type: t.into(),
            config: Default::default(),
            provider_config: Default::default(),
            position: Position { x: 0, y: 0 },
            size: None,
            parent: parent.map(|p| p.into()),
            manual: false,
            providers: Vec::new(),
            extra: Default::default(),
            classification: None,
            description: String::new(),
            owner: String::new(),
            overrides: Default::default(),
            adopt: Default::default(),
        }
    }

    fn container(id: &str, name: &str, t: &str) -> Container {
        Container {
            id: id.into(),
            name: name.into(),
            container_type: t.into(),
            config: Default::default(),
            provider_config: Default::default(),
            position: Position { x: 0, y: 0 },
            size: Size { w: 400, h: 300 },
            parent: None,
            manual: false,
            providers: Vec::new(),
            extra: Default::default(),
            classification: None,
            description: String::new(),
            owner: String::new(),
            overrides: Default::default(),
            adopt: Default::default(),
        }
    }

    fn base() -> Project {
        let mut p = Project::new("demo");
        p.containers
            .insert("rg".into(), container("rg", "main", "resource_group"));
        p.containers
            .insert("rg2".into(), container("rg2", "other", "resource_group"));
        let mut q = node("q", "jobs", "event_queue", Some("rg"));
        q.config
            .insert("visibility_timeout_seconds".into(), Value::Int(30));
        p.nodes.insert("q".into(), q);
        p.nodes
            .insert("fn".into(), node("fn", "worker", "function", Some("rg")));
        p.nodes
            .insert("b".into(), node("b", "uploads", "object_storage", Some("rg")));
        p.add_edge("fn", "q", Relation::SendsTo);
        p.views.push(View::new("Main", ViewFilter::default()));
        p
    }

    #[test]
    fn identical_projects_have_no_changes() {
        let p = base();
        let mut moved = p.clone();
        // Geometry alone is not an architecture change.
        moved.nodes.get_mut("q").unwrap().position = Position { x: 500, y: 500 };
        let d = diff(&p, &moved);
        assert!(d.is_empty(), "{d:?}");
        assert_eq!(d.summary(), "no architecture changes");
        assert!(d
            .to_markdown("a", "b", &|t| t.to_string())
            .contains("No architecture changes"));
    }

    #[test]
    fn entities_fields_links_and_moves() {
        let old = base();
        let mut new = old.clone();
        // Renamed, moved and reconfigured.
        {
            let q = new.nodes.get_mut("q").unwrap();
            q.name = "job queue".into();
            q.parent = Some("rg2".into());
            q.config
                .insert("visibility_timeout_seconds".into(), Value::Int(120));
            q.provider_config
                .entry("azure".into())
                .or_default()
                .insert("sku".into(), Value::Str("Standard".into()));
            q.adopt
                .entry("aws".into())
                .or_default()
                .insert("main".into(), "https://sqs/x".into());
        }
        // Removed, added, and re-created under a new id with the same name and type.
        new.nodes.remove("b");
        new.nodes.insert(
            "dlq".into(),
            node("dlq", "dead letters", "event_queue", Some("rg")),
        );
        let mut fn2 = new.nodes.remove("fn").unwrap();
        fn2.id = "fn-2".into();
        fn2.extra
            .entry("aws".into())
            .or_default()
            .entry("main".into())
            .or_default()
            .insert("timeout".into(), serde_json::json!(30));
        new.nodes.insert("fn-2".into(), fn2);
        new.edges.clear();
        new.add_edge("fn-2", "q", Relation::SendsTo);
        new.add_edge("q", "dlq", Relation::DeadLettersTo);
        new.settings.tags.insert("Project".into(), "demo".into());

        let d = diff(&old, &new);
        assert_eq!(d.entities.added.len(), 1);
        assert_eq!(d.entities.added[0].name, "dead letters");
        assert_eq!(d.entities.removed.len(), 1);
        assert_eq!(d.entities.removed[0].name, "uploads");
        let q = d.entities.changed.iter().find(|c| c.id == "q").unwrap();
        assert_eq!(q.renamed_from.as_deref(), Some("jobs"));
        assert_eq!(
            q.moved,
            Some(Move {
                from: Some("main".into()),
                to: Some("other".into())
            })
        );
        let paths: Vec<&str> = q.fields.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "adopt.aws.main",
                "config.visibility_timeout_seconds",
                "provider_config.azure.sku"
            ]
        );
        let vis = &q.fields[1];
        assert_eq!(
            (vis.old.clone(), vis.new.clone()),
            (Some(serde_json::json!(30)), Some(serde_json::json!(120)))
        );
        // The re-created function is the same entity, its link the same link.
        let f = d.entities.changed.iter().find(|c| c.id == "fn-2").unwrap();
        assert_eq!(f.id_was.as_deref(), Some("fn"));
        assert_eq!(f.fields[0].path, "extra.aws.main.timeout");
        assert_eq!(d.links.added.len(), 1);
        assert_eq!(d.links.added[0].relation, "dead_letters_to");
        assert!(d.links.removed.is_empty());
        assert_eq!(d.settings[0].path, "settings.tags.Project");
        assert_eq!(d.settings[0].old, None);

        let md = d.to_markdown("main", "working copy", &|t| t.replace('_', " "));
        assert!(md.contains("| dead letters | event queue | main |"), "{md}");
        assert!(md.contains("renamed from \"jobs\""), "{md}");
        assert!(md.contains("moved from \"main\" to \"other\""), "{md}");
        assert!(
            md.contains("`config.visibility_timeout_seconds`: `30` \u{2192} `120`"),
            "{md}"
        );
        assert!(
            md.contains("- added: job queue \u{2192} dead letters (dead letters to)"),
            "{md}"
        );
        assert!(md.contains("### Settings"), "{md}");
        assert_eq!(
            d.summary(),
            "entities: 1 added, 1 removed, 2 changed; links: 1 added; settings changed"
        );
        // Serializes for the MCP reply.
        let j = serde_json::to_value(&d).unwrap();
        assert_eq!(j["entities"]["added"][0]["type"], "event_queue");
    }

    #[test]
    fn environment_overrides_and_link_environments() {
        let mut old = base();
        old.settings.environments = vec!["pilot".into(), "prod".into()];
        let mut new = old.clone();
        let q = new.nodes.get_mut("q").unwrap();
        let prod = q.overrides.entry("prod".into()).or_default();
        prod.config
            .insert("visibility_timeout_seconds".into(), Value::Int(300));
        q.overrides.entry("pilot".into()).or_default().absent = true;
        new.edges[0].environments = vec!["prod".into()];
        new.settings.name_prefix = Some("shop-${var.environment}".into());
        let d = diff(&old, &new);
        let c = &d.entities.changed[0];
        let paths: Vec<&str> = c.fields.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "overrides.pilot.absent",
                "overrides.prod.config.visibility_timeout_seconds"
            ]
        );
        assert_eq!(d.links.changed[0].fields[0].path, "environments");
        assert_eq!(d.settings[0].path, "settings.name_prefix");
        let md = d.to_markdown("a", "b", &|t| t.to_string());
        assert!(
            md.contains("`overrides.prod.config.visibility_timeout_seconds`: set to `300`"),
            "{md}"
        );
        assert!(
            md.contains("changed: worker \u{2192} jobs (sends to) (`environments`"),
            "{md}"
        );
    }

    #[test]
    fn views_and_annotations() {
        let old = base();
        let mut new = old.clone();
        let v = &mut new.views[0];
        v.description = "what runs".into();
        v.filter.categories.insert("compute".into());
        v.flows.push(Flow {
            id: "f1".into(),
            from: FlowEnd::Entity { entity: "fn".into() },
            to: FlowEnd::Entity { entity: "q".into() },
            label: "enqueue".into(),
            dashed: false,
            step: Some(1),
            color: None,
            data: None,
        });
        new.views.push(View::new("Data", ViewFilter::default()));
        let d = diff(&old, &new);
        assert_eq!(d.views.added, ["Data"]);
        let c = &d.views.changed[0];
        assert_eq!(c.name, "Main");
        assert_eq!(c.fields[0].path, "description");
        assert_eq!(c.fields[1].path, "filter.categories");
        assert_eq!(c.annotations[0].kind, "flow");
        assert_eq!(c.annotations[0].change, "added");
        let md = d.to_markdown("a", "b", &|t| t.to_string());
        assert!(md.contains("flow \"enqueue\" added"), "{md}");

        // A flow relabelled and a group removed.
        let mut newer = new.clone();
        newer.views[0].flows[0].label = "push job".into();
        let d2 = diff(&new, &newer);
        let a = &d2.views.changed[0].annotations[0];
        assert_eq!(
            (a.change.as_str(), a.fields[0].path.as_str()),
            ("changed", "label")
        );
    }
}
