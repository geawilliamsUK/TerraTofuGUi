//! Patterns: parameterised groups of entities and links inserted in one step ("hardened
//! bucket", "queue with dead-letter queue and alarms", …).
//!
//! A pattern is data (`definitions/patterns/*.json` for the built-ins, the user's
//! patterns folder for saved ones). [`expand`] turns one, with its parameters, into the
//! same write commands an agent sends to `project_apply` — `entity_add`,
//! `entity_resize`, `entity_update`, `link_add` — so inserting a pattern goes through
//! exactly the checks every other edit does (types, allowed parents, field values,
//! relations) and lands as one undo step. [`check`] validates a pattern against the
//! catalog without a project; [`infer`] makes a pattern from a selection.
//!
//! # Format
//!
//! ```json
//! {
//!   "name": "Hardened bucket",
//!   "description": "…",
//!   "params": [
//!     { "name": "name", "default": "records" },
//!     { "name": "bucket_name", "default": "{project|kebab}-{name|kebab}" },
//!     { "name": "key", "entity": "encryption_key", "optional": true }
//!   ],
//!   "entities": [
//!     { "key": "bucket", "type": "object_storage", "name": "{name}", "at": [0, 0],
//!       "config": { "versioning": true }, "provider_config": { "aws": { "bucket_name": "{bucket_name}" } } },
//!     { "key": "k", "type": "encryption_key", "name": "{name} key", "when": "!key" }
//!   ],
//!   "links": [
//!     { "source": "bucket", "target": "k", "relation": "encrypted_with" },
//!     { "source": "bucket", "target": "$key", "relation": "encrypted_with" }
//!   ]
//! }
//! ```
//!
//! - Strings anywhere in `name`, `config`, `provider_config` and `extra` are templates:
//!   `{param}` or `{param|filter|…}` with the filters `kebab`, `alnum`, `slug`, `lower`,
//!   `upper` and `max:N`; `{project}` is the project's name. An entity parameter reads
//!   as the entity's name. Other braces are left alone.
//! - An entity parameter (`"entity": "<type>"`, `"*"` for any type, `"list": true` for
//!   several) names entities that already exist; links reach them as `$param`.
//! - `parent`: omitted = the container the pattern is applied in; another entity's
//!   `key`; `$param` = that entity; `@param` = the container that entity is in. A
//!   container that does not allow the type is replaced by its nearest ancestor that
//!   does.
//! - `when`: `"param"` keeps the entity or link only when the parameter is given,
//!   `"!param"` only when it is not. A link to an entity left out is left out.
//! - `at` is the offset from where the pattern is placed; `size` sizes a container.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value as J};
use std::collections::{BTreeMap, HashMap, HashSet};
use ttg_catalog::{Catalog, ResourceKind};
use ttg_core::{Id, Position, Project, Relation};

/// One parameter of a pattern.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Used when the parameter is not given; a template itself (earlier parameters and
    /// `{project}` may appear in it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// An entity parameter: the abstract type the entity must have, `*` for any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity: Option<String>,
    /// Several entities (an entity parameter only).
    #[serde(default, skip_serializing_if = "is_false")]
    pub list: bool,
    /// May be left out (and has no default).
    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// One entity the pattern adds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PatternEntity {
    pub key: String,
    #[serde(rename = "type")]
    pub type_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default)]
    pub at: [i32; 2],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<[i32; 2]>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub config: Map<String, J>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub provider_config: Map<String, J>,
    /// provider -> block key -> extra arguments.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, J>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
}

/// One link the pattern adds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PatternLink {
    pub source: String,
    pub target: String,
    pub relation: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pattern {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<Param>,
    pub entities: Vec<PatternEntity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<PatternLink>,
}

impl Pattern {
    /// Does `query` name this pattern? Compared case-insensitively as slugs, so "hardened
    /// bucket", "Hardened bucket" and "hardened_bucket" are the same.
    pub fn is_named(&self, query: &str) -> bool {
        ttg_core::slugify(&self.name) == ttg_core::slugify(query)
    }

    pub fn param(&self, name: &str) -> Option<&Param> {
        self.params.iter().find(|p| p.name == name)
    }
}

const BUILTIN_SOURCES: &[(&str, &str)] = &[
    (
        "hardened_bucket.json",
        include_str!("../../../definitions/patterns/hardened_bucket.json"),
    ),
    (
        "queue_with_dlq.json",
        include_str!("../../../definitions/patterns/queue_with_dlq.json"),
    ),
    (
        "gpu_queue_worker.json",
        include_str!("../../../definitions/patterns/gpu_queue_worker.json"),
    ),
    (
        "web_service_lb.json",
        include_str!("../../../definitions/patterns/web_service_lb.json"),
    ),
];

/// The patterns that come with the app.
pub fn builtins() -> Vec<Pattern> {
    BUILTIN_SOURCES
        .iter()
        .map(|(file, text)| {
            serde_json::from_str(text).unwrap_or_else(|e| panic!("definitions/patterns/{file}: {e}"))
        })
        .collect()
}

/// Parse a pattern file.
pub fn parse(text: &str) -> Result<Pattern, String> {
    serde_json::from_str(text).map_err(|e| format!("not a pattern file: {e}"))
}

/// Pretty JSON with a trailing newline, as a pattern file.
pub fn to_file_text(p: &Pattern) -> String {
    let mut s = serde_json::to_string_pretty(p).unwrap_or_default();
    s.push('\n');
    s
}

/// The patterns in a folder (`*.json`), sorted by name, each with its file name. Files
/// that do not parse are reported beside them rather than dropped silently.
pub fn load_dir(dir: &std::path::Path) -> (Vec<(String, Pattern)>, Vec<String>) {
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return (ok, bad);
    };
    let mut files: Vec<std::path::PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    for f in files {
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        match std::fs::read_to_string(&f)
            .map_err(|e| e.to_string())
            .and_then(|t| parse(&t))
        {
            Ok(p) => ok.push((name, p)),
            Err(e) => bad.push(format!("{name}: {e}")),
        }
    }
    (ok, bad)
}

/// The file name a pattern is saved under: its name as a slug.
pub fn file_name(name: &str) -> String {
    format!("{}.json", ttg_core::slugify(name))
}

// ------------------------------------------------------------------ templates

fn apply_filter(s: String, f: &str) -> Result<String, String> {
    let f = f.trim();
    Ok(match f {
        "kebab" => {
            let mut out = String::new();
            for c in s.to_lowercase().chars() {
                if c.is_ascii_alphanumeric() {
                    out.push(c);
                } else if !out.ends_with('-') && !out.is_empty() {
                    out.push('-');
                }
            }
            out.trim_end_matches('-').to_string()
        }
        "alnum" => s
            .to_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect(),
        "slug" => ttg_core::slugify(&s),
        "lower" => s.to_lowercase(),
        "upper" => s.to_uppercase(),
        _ => match f
            .strip_prefix("max:")
            .and_then(|n| n.trim().parse::<usize>().ok())
        {
            Some(n) => {
                let cut: String = s.chars().take(n).collect();
                cut.trim_end_matches('-').to_string()
            }
            None => {
                return Err(format!(
                    "unknown template filter \"{f}\" (kebab, alnum, slug, lower, upper, max:N)"
                ))
            }
        },
    })
}

/// Fill `{param|filter}` placeholders. Placeholders whose name is not a known value are
/// left as they are, so JSON policies and `${…}` expressions pass through.
pub fn fill(template: &str, values: &HashMap<String, String>) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(j) = after.find('}') else {
            out.push_str(&rest[i..]);
            return Ok(out);
        };
        let inner = &after[..j];
        let mut parts = inner.split('|');
        let name = parts.next().unwrap_or("").trim();
        let preceded_by_dollar = out.ends_with('$');
        match values.get(name) {
            Some(v) if !preceded_by_dollar && !name.is_empty() => {
                let mut v = v.clone();
                for f in parts {
                    v = apply_filter(v, f)?;
                }
                out.push_str(&v);
            }
            _ => {
                out.push('{');
                out.push_str(inner);
                out.push('}');
            }
        }
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn fill_json(v: &J, values: &HashMap<String, String>) -> Result<J, String> {
    Ok(match v {
        J::String(s) => J::String(fill(s, values)?),
        J::Array(a) => J::Array(a.iter().map(|x| fill_json(x, values)).collect::<Result<_, _>>()?),
        J::Object(o) => {
            let mut m = Map::new();
            for (k, x) in o {
                m.insert(k.clone(), fill_json(x, values)?);
            }
            J::Object(m)
        }
        other => other.clone(),
    })
}

/// The `{name}`s a template uses.
fn placeholders(template: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        let after = &rest[i + 1..];
        let Some(j) = after.find('}') else { break };
        let name = after[..j].split('|').next().unwrap_or("").trim();
        if !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !rest[..i].ends_with('$')
        {
            out.push(name.to_string());
        }
        rest = &after[j + 1..];
    }
    out
}

fn json_strings<'a>(v: &'a J, out: &mut Vec<&'a str>) {
    match v {
        J::String(s) => out.push(s),
        J::Array(a) => a.iter().for_each(|x| json_strings(x, out)),
        J::Object(o) => o.values().for_each(|x| json_strings(x, out)),
        _ => {}
    }
}

// ------------------------------------------------------------------ expansion

/// What applying a pattern will do: the write commands, in order, as `project_apply`
/// takes them, and the entities it adds.
#[derive(Debug, Clone, Default)]
pub struct Expansion {
    /// `(tool, args)`.
    pub steps: Vec<(String, J)>,
    /// `(pattern key, display name, type)` of every entity added.
    pub entities: Vec<(String, String, String)>,
}

/// A parameter's value as given: text, or for an entity parameter the entity ids.
#[derive(Debug, Clone)]
enum Given {
    Text,
    Entities(Vec<Id>),
}

fn when_holds(when: Option<&str>, given: &HashMap<String, Given>) -> bool {
    match when.map(str::trim) {
        None | Some("") => true,
        Some(w) => match w.strip_prefix('!') {
            Some(p) => !given.contains_key(p.trim()),
            None => given.contains_key(w),
        },
    }
}

/// The nearest container, starting at `start` and walking outwards, that type
/// `type_id` may be drawn in; `None` when there is none (top level).
fn allowed_container(p: &Project, cat: &Catalog, type_id: &str, start: Option<&str>) -> Option<Id> {
    let def = cat.resource(type_id)?;
    let mut cur = start.map(str::to_string);
    while let Some(c) = cur {
        let ct = p.containers.get(&c)?;
        if def
            .resource
            .allowed_parents
            .iter()
            .any(|a| a == &ct.container_type)
        {
            return Some(c);
        }
        cur = ct.parent.clone();
    }
    None
}

/// Turn `pattern` with `params` into the commands that add it to `project`: entities
/// placed from `origin`, inside `parent` (a container id) unless the pattern says
/// otherwise. Refused with every problem at once when a parameter is unknown, missing or
/// names the wrong kind of entity, or when a name would collide with one in the project.
pub fn expand(
    pattern: &Pattern,
    params: &Map<String, J>,
    project: &Project,
    cat: &Catalog,
    origin: Position,
    parent: Option<&str>,
) -> Result<Expansion, String> {
    let mut problems = Vec::new();
    for k in params.keys() {
        if pattern.param(k).is_none() {
            problems.push(format!(
                "unknown parameter \"{k}\" (parameters: {})",
                if pattern.params.is_empty() {
                    "none".to_string()
                } else {
                    pattern
                        .params
                        .iter()
                        .map(|p| p.name.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            ));
        }
    }
    let mut values: HashMap<String, String> = HashMap::new();
    values.insert("project".into(), project.name.clone());
    let mut given: HashMap<String, Given> = HashMap::new();
    for p in &pattern.params {
        let raw = params
            .get(&p.name)
            .filter(|v| !v.is_null() && v.as_str() != Some(""));
        match &p.entity {
            Some(want) => {
                let names: Vec<String> = match raw {
                    None => Vec::new(),
                    Some(J::String(s)) => vec![s.clone()],
                    Some(J::Array(a)) if p.list => {
                        a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()
                    }
                    Some(other) => {
                        problems.push(format!(
                            "parameter \"{}\" takes {}, not {other}",
                            p.name,
                            if p.list {
                                "a list of entity names"
                            } else {
                                "an entity id or name"
                            }
                        ));
                        continue;
                    }
                };
                if names.is_empty() {
                    if !p.optional {
                        problems.push(format!(
                            "parameter \"{}\" is required: {} ({})",
                            p.name,
                            if p.description.is_empty() {
                                "an existing entity"
                            } else {
                                &p.description
                            },
                            if want == "*" {
                                "any type".to_string()
                            } else {
                                format!("a {want}")
                            }
                        ));
                    }
                    continue;
                }
                let mut ids = Vec::new();
                for n in &names {
                    match crate::refs::resolve_entity(project, n) {
                        None => problems.push(format!(
                            "parameter \"{}\": no entity with id or name \"{n}\"",
                            p.name
                        )),
                        Some(id) => {
                            let t = project.entity(&id).unwrap().resource_type.to_string();
                            if want != "*" && !want.split(',').any(|w| w.trim() == t) {
                                problems.push(format!(
                                    "parameter \"{}\": \"{n}\" is a {t}, not a {want}",
                                    p.name
                                ));
                            } else {
                                ids.push(id);
                            }
                        }
                    }
                }
                if let Some(first) = ids.first() {
                    values.insert(p.name.clone(), project.entity(first).unwrap().name.to_string());
                }
                given.insert(p.name.clone(), Given::Entities(ids));
            }
            None => {
                let text = match raw {
                    Some(J::String(s)) => Some(s.clone()),
                    Some(J::Number(n)) => Some(n.to_string()),
                    Some(J::Bool(b)) => Some(b.to_string()),
                    Some(other) => {
                        problems.push(format!("parameter \"{}\" takes text, not {other}", p.name));
                        None
                    }
                    None => match &p.default {
                        Some(d) => match fill(d, &values) {
                            Ok(v) => Some(v),
                            Err(e) => {
                                problems.push(format!("parameter \"{}\": {e}", p.name));
                                None
                            }
                        },
                        None if p.optional => None,
                        None => {
                            problems.push(format!(
                                "parameter \"{}\" is required{}",
                                p.name,
                                if p.description.is_empty() {
                                    String::new()
                                } else {
                                    format!(": {}", p.description)
                                }
                            ));
                            None
                        }
                    },
                };
                if let Some(t) = text {
                    // A default counts as given only for templates, not for `when`.
                    if raw.is_some() {
                        given.insert(p.name.clone(), Given::Text);
                    }
                    values.insert(p.name.clone(), t);
                }
            }
        }
    }
    if !problems.is_empty() {
        return Err(problems.join("; "));
    }
    if let Some(pid) = parent {
        if !project.containers.contains_key(pid) {
            return Err(format!("\"{pid}\" is not a container"));
        }
    }

    // Which entities are included, and what each is called.
    let included: Vec<&PatternEntity> = pattern
        .entities
        .iter()
        .filter(|e| when_holds(e.when.as_deref(), &given))
        .collect();
    let mut names: HashMap<&str, String> = HashMap::new();
    let mut slugs: HashSet<String> = project
        .entities()
        .iter()
        .map(|e| ttg_core::slugify(e.name))
        .collect();
    for e in &included {
        let name = fill(&e.name, &values)?;
        let slug = ttg_core::slugify(&name);
        if !slugs.insert(slug) {
            problems.push(format!(
                "\"{name}\" would collide with an existing resource of the same name; give the pattern another name parameter"
            ));
        }
        names.insert(e.key.as_str(), name);
    }
    if !problems.is_empty() {
        return Err(problems.join("; "));
    }

    let mut out = Expansion::default();
    // Containers first, so their contents can be placed inside them.
    let mut ordered = included.clone();
    ordered.sort_by_key(|e| {
        cat.resource(&e.type_id)
            .map(|d| d.resource.kind != ResourceKind::Container)
            .unwrap_or(true)
    });
    for e in &ordered {
        let name = names[e.key.as_str()].clone();
        // The first alternative that resolves wins; none = the apply container.
        let mut parent_arg: Option<Option<String>> = None;
        for alt in e.parent.as_deref().unwrap_or("").split('|').map(str::trim) {
            if let Some(pname) = alt.strip_prefix('$').or_else(|| alt.strip_prefix('@')) {
                if let Some(Given::Entities(ids)) = given.get(pname) {
                    if let Some(id) = ids.first() {
                        let start = if alt.starts_with('$') {
                            Some(id.clone())
                        } else {
                            project.parent_of(id).map(str::to_string)
                        };
                        parent_arg = Some(allowed_container(project, cat, &e.type_id, start.as_deref()));
                        break;
                    }
                }
            } else if let Some(n) = names.get(alt) {
                parent_arg = Some(Some(n.clone()));
                break;
            }
        }
        let parent_arg = parent_arg.unwrap_or_else(|| allowed_container(project, cat, &e.type_id, parent));
        let mut add = json!({
            "type_id": e.type_id,
            "name": name,
            "x": origin.x + e.at[0],
            "y": origin.y + e.at[1],
        });
        if let Some(p) = parent_arg {
            add["parent"] = J::String(p);
        }
        out.steps.push(("entity_add".into(), add));
        out.entities.push((e.key.clone(), name, e.type_id.clone()));
    }
    for e in &ordered {
        let name = names[e.key.as_str()].clone();
        if let Some([w, h]) = e.size {
            out.steps
                .push(("entity_resize".into(), json!({"entity": name, "w": w, "h": h})));
        }
        let mut upd = Map::new();
        if !e.config.is_empty() {
            upd.insert("config".into(), fill_json(&J::Object(e.config.clone()), &values)?);
        }
        if !e.provider_config.is_empty() {
            upd.insert(
                "provider_config".into(),
                fill_json(&J::Object(e.provider_config.clone()), &values)?,
            );
        }
        if !e.description.is_empty() {
            upd.insert("description".into(), J::String(fill(&e.description, &values)?));
        }
        if !upd.is_empty() {
            upd.insert("entity".into(), J::String(name.clone()));
            out.steps.push(("entity_update".into(), J::Object(upd)));
        }
        for (prov, blocks) in &e.extra {
            let Some(blocks) = blocks.as_object() else {
                continue;
            };
            for (block, args) in blocks {
                out.steps.push((
                    "entity_update".into(),
                    json!({
                        "entity": name,
                        "extra": fill_json(args, &values)?,
                        "extra_provider": prov,
                        "extra_block": block,
                    }),
                ));
            }
        }
    }
    let end = |r: &str| -> Option<Vec<String>> {
        match r.strip_prefix('$') {
            Some(pname) => match given.get(pname) {
                Some(Given::Entities(ids)) => Some(ids.clone()),
                _ => None,
            },
            None => names.get(r).map(|n| vec![n.clone()]),
        }
    };
    for l in &pattern.links {
        if !when_holds(l.when.as_deref(), &given) {
            continue;
        }
        let (Some(sources), Some(targets)) = (end(&l.source), end(&l.target)) else {
            continue;
        };
        for s in &sources {
            for t in &targets {
                let mut a = json!({"source": s, "target": t, "relation": l.relation});
                if !l.providers.is_empty() {
                    a["providers"] = json!(l.providers);
                }
                out.steps.push(("link_add".into(), a));
            }
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------ checking

/// Problems with a pattern as data: unknown types, fields, parents and relations,
/// links and templates naming nothing. Empty means it is consistent with `cat`.
pub fn check(pattern: &Pattern, cat: &Catalog) -> Vec<String> {
    let mut out = Vec::new();
    let mut keys: HashMap<&str, &PatternEntity> = HashMap::new();
    let param_names: HashSet<&str> = pattern.params.iter().map(|p| p.name.as_str()).collect();
    if pattern.entities.is_empty() {
        out.push("a pattern needs at least one entity".into());
    }
    for p in &pattern.params {
        if let Some(t) = &p.entity {
            for t in t.split(',').map(str::trim).filter(|t| *t != "*") {
                if cat.resource(t).is_none() && !Catalog::is_native(t) {
                    out.push(format!("parameter \"{}\": unknown type \"{t}\"", p.name));
                }
            }
        }
    }
    for e in &pattern.entities {
        if keys.insert(e.key.as_str(), e).is_some() {
            out.push(format!("two entities share the key \"{}\"", e.key));
        }
    }
    let known = |n: &str| n == "project" || param_names.contains(n);
    for e in &pattern.entities {
        let at = format!("entity \"{}\"", e.key);
        let Some(def) = cat.resource(&e.type_id) else {
            if !Catalog::is_native(&e.type_id) {
                out.push(format!("{at}: unknown type \"{}\"", e.type_id));
            }
            continue;
        };
        for k in e.config.keys() {
            if !def.fields.iter().any(|f| &f.name == k) {
                out.push(format!("{at}: {} has no field \"{k}\"", e.type_id));
            }
        }
        for (pid, vals) in &e.provider_config {
            let Some(m) = def.providers.get(pid) else {
                out.push(format!("{at}: {} has no {pid} mapping", e.type_id));
                continue;
            };
            for k in vals
                .as_object()
                .map(|o| o.keys().collect::<Vec<_>>())
                .unwrap_or_default()
            {
                if !m.fields.iter().any(|f| &f.name == k) {
                    out.push(format!("{at}: {}/{pid} has no field \"{k}\"", e.type_id));
                }
            }
        }
        for parent in e
            .parent
            .as_deref()
            .unwrap_or("")
            .split('|')
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            if !parent.starts_with('$') && !parent.starts_with('@') {
                match keys.get(parent) {
                    None => out.push(format!(
                        "{at}: parent \"{parent}\" is not an entity of the pattern"
                    )),
                    Some(pe) => {
                        if !def.resource.allowed_parents.iter().any(|a| a == &pe.type_id) {
                            out.push(format!(
                                "{at}: a {} cannot be inside a {} (allowed: {})",
                                e.type_id,
                                pe.type_id,
                                def.resource.allowed_parents.join(", ")
                            ));
                        }
                    }
                }
            } else if !param_names.contains(&parent[1..]) {
                out.push(format!("{at}: parent \"{parent}\" names no parameter"));
            }
        }
        let mut strings = vec![e.name.as_str(), e.description.as_str()];
        let (c, pc, x) = (
            J::Object(e.config.clone()),
            J::Object(e.provider_config.clone()),
            J::Object(e.extra.clone()),
        );
        json_strings(&c, &mut strings);
        json_strings(&pc, &mut strings);
        json_strings(&x, &mut strings);
        for s in strings {
            for n in placeholders(s) {
                if !known(&n) {
                    out.push(format!(
                        "{at}: template \"{s}\" uses \"{{{n}}}\", which is not a parameter"
                    ));
                }
            }
        }
        if let Some(w) = e.when.as_deref() {
            if !param_names.contains(w.trim_start_matches('!').trim()) {
                out.push(format!("{at}: `when` names no parameter (\"{w}\")"));
            }
        }
    }
    for p in &pattern.params {
        if let Some(d) = &p.default {
            for n in placeholders(d) {
                if !known(&n) {
                    out.push(format!(
                        "parameter \"{}\": default uses \"{{{n}}}\", which is not a parameter",
                        p.name
                    ));
                }
            }
        }
    }
    let end_type = |r: &str| -> Result<Option<Vec<String>>, String> {
        match r.strip_prefix('$') {
            Some(pname) => match pattern.param(pname) {
                Some(p) => match &p.entity {
                    Some(t) if t == "*" => Ok(None),
                    Some(t) => Ok(Some(t.split(',').map(|x| x.trim().to_string()).collect())),
                    None => Err(format!("\"${pname}\" is not an entity parameter")),
                },
                None => Err(format!("\"{r}\" names no parameter")),
            },
            None => match keys.get(r) {
                Some(e) => Ok(Some(vec![e.type_id.clone()])),
                None => Err(format!("\"{r}\" is not an entity of the pattern")),
            },
        }
    };
    for l in &pattern.links {
        let at = format!("link {} -{}-> {}", l.source, l.relation, l.target);
        let Some(rel) = Relation::from_key(&l.relation) else {
            out.push(format!("{at}: unknown relation \"{}\"", l.relation));
            continue;
        };
        if let Some(w) = l.when.as_deref() {
            if !param_names.contains(w.trim_start_matches('!').trim()) {
                out.push(format!("{at}: `when` names no parameter (\"{w}\")"));
            }
        }
        let (s, t) = match (end_type(&l.source), end_type(&l.target)) {
            (Ok(s), Ok(t)) => (s, t),
            (Err(e), _) | (_, Err(e)) => {
                out.push(format!("{at}: {e}"));
                continue;
            }
        };
        if rel == Relation::DependsOn {
            continue;
        }
        let (Some(ss), Some(ts)) = (s, t) else { continue };
        for st in &ss {
            let Some(def) = cat.resource(st) else { continue };
            for tt in &ts {
                let ok = def
                    .relations
                    .iter()
                    .any(|r| r.kind == rel.key() && r.targets.iter().any(|x| x == tt));
                if !ok {
                    out.push(format!(
                        "{at}: a {st} cannot have a '{}' link to a {tt}",
                        rel.key()
                    ));
                }
            }
        }
    }
    out
}

// ------------------------------------------------------------------ saving

/// Make a pattern from entities of `project` (ids). Links among them come along; a link
/// to an entity outside the selection becomes an entity parameter named after that
/// entity, defaulting to it, so applying the pattern in the same project wires it up
/// the same way. When the entities' names share leading words ("orders queue", "orders
/// dlq") those words become a `name` parameter and every name and provider value
/// spelling them is templated; otherwise the pattern has no parameter of its own.
pub fn infer(project: &Project, ids: &[Id], name: &str, description: &str) -> Result<Pattern, String> {
    if ids.is_empty() {
        return Err("no entities to save as a pattern".into());
    }
    let set: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let mut sorted: Vec<&str> = project
        .entities()
        .iter()
        .map(|e| e.id)
        .filter(|id| set.contains(id))
        .collect();
    if sorted.len() != set.len() {
        let missing: Vec<&&str> = set.iter().filter(|i| project.entity(i).is_none()).collect();
        return Err(format!("no such entities: {missing:?}"));
    }
    // Keep the project's order (containers outer first, then nodes) for a stable file.
    sorted.dedup();
    let entity_names: Vec<String> = sorted
        .iter()
        .map(|id| project.entity(id).unwrap().name.to_string())
        .collect();
    let prefix = common_prefix(&entity_names);
    let mut params = Vec::new();
    if let Some(p) = &prefix {
        params.push(Param {
            name: "name".into(),
            description: "The words the saved entities' names start with".into(),
            default: Some(p.clone()),
            entity: None,
            list: false,
            optional: false,
        });
    }
    let template = |s: &str| -> String {
        match &prefix {
            None => s.to_string(),
            Some(p) => {
                let forms = [
                    (p.clone(), "{name}".to_string()),
                    (
                        apply_filter(p.clone(), "kebab").unwrap_or_default(),
                        "{name|kebab}".to_string(),
                    ),
                    (
                        apply_filter(p.clone(), "alnum").unwrap_or_default(),
                        "{name|alnum}".to_string(),
                    ),
                    (ttg_core::slugify(p), "{name|slug}".to_string()),
                ];
                let mut out = s.to_string();
                for (from, to) in forms {
                    if from.len() >= 3 && out.contains(&from) {
                        out = out.replace(&from, &to);
                        break;
                    }
                }
                out
            }
        }
    };
    fn template_json(v: &J, f: &dyn Fn(&str) -> String) -> J {
        match v {
            J::String(s) => J::String(f(s)),
            J::Array(a) => J::Array(a.iter().map(|x| template_json(x, f)).collect()),
            J::Object(o) => J::Object(o.iter().map(|(k, x)| (k.clone(), template_json(x, f))).collect()),
            other => other.clone(),
        }
    }
    // Keys from names, unique.
    let mut keys: BTreeMap<&str, String> = BTreeMap::new();
    let mut used = HashSet::new();
    for id in &sorted {
        let base = ttg_core::slugify(project.entity(id).unwrap().name);
        let mut k = base.clone();
        let mut n = 2;
        while !used.insert(k.clone()) {
            k = format!("{base}_{n}");
            n += 1;
        }
        keys.insert(id, k);
    }
    // Origin: the top-left of the selection.
    let min_x = sorted
        .iter()
        .map(|id| project.entity(id).unwrap().position.x)
        .min()
        .unwrap_or(0);
    let min_y = sorted
        .iter()
        .map(|id| project.entity(id).unwrap().position.y)
        .min()
        .unwrap_or(0);
    let mut entities = Vec::new();
    for id in &sorted {
        let e = project.entity(id).unwrap();
        let parent = e.parent.filter(|p| set.contains(p)).map(|p| keys[p].clone());
        let size = project.containers.get(*id).map(|c| [c.size.w, c.size.h]);
        let to_map = |v: J| v.as_object().cloned().unwrap_or_default();
        let config = to_map(template_json(
            &serde_json::to_value(e.config).unwrap_or(J::Null),
            &template,
        ));
        let provider_config = to_map(template_json(
            &serde_json::to_value(e.provider_config).unwrap_or(J::Null),
            &template,
        ));
        let mut provider_config = provider_config;
        provider_config.retain(|_, v| v.as_object().is_some_and(|o| !o.is_empty()));
        let extra = to_map(template_json(
            &serde_json::to_value(e.extra).unwrap_or(J::Null),
            &template,
        ));
        entities.push(PatternEntity {
            key: keys[id].clone(),
            type_id: e.resource_type.to_string(),
            name: template(e.name),
            parent,
            at: [e.position.x - min_x, e.position.y - min_y],
            size,
            config,
            provider_config,
            extra,
            description: e.description.to_string(),
            when: None,
        });
    }
    let mut links = Vec::new();
    let mut outside: BTreeMap<String, (String, String)> = BTreeMap::new();
    for edge in &project.edges {
        let (s_in, t_in) = (
            set.contains(edge.source.as_str()),
            set.contains(edge.target.as_str()),
        );
        if !s_in && !t_in {
            continue;
        }
        let mut endpoint = |id: &str, inside: bool| -> String {
            if inside {
                return keys[id].clone();
            }
            let e = project.entity(id).unwrap();
            let pname = ttg_core::slugify(e.name);
            outside
                .entry(pname.clone())
                .or_insert((e.resource_type.to_string(), e.name.to_string()));
            format!("${pname}")
        };
        let source = endpoint(&edge.source, s_in);
        let target = endpoint(&edge.target, t_in);
        links.push(PatternLink {
            source,
            target,
            relation: edge.relation.key().to_string(),
            providers: edge.providers.clone(),
            when: None,
        });
    }
    for (pname, (t, ename)) in outside {
        if params.iter().any(|p: &Param| p.name == pname) {
            continue;
        }
        params.push(Param {
            name: pname,
            description: format!("An existing {t} to link to (was \"{ename}\")"),
            default: None,
            entity: Some(t),
            list: false,
            optional: false,
        });
    }
    // An outside entity parameter defaults to nothing: entity parameters are names, and
    // `expand` resolves them in the project the pattern is applied to.
    Ok(Pattern {
        name: name.trim().to_string(),
        description: description.to_string(),
        params,
        entities,
        links,
    })
}

/// Leading whole words every name starts with, when there are several names and at
/// least one word in common; a single entity's whole name.
fn common_prefix(names: &[String]) -> Option<String> {
    if names.len() == 1 {
        return Some(names[0].clone()).filter(|n| !n.trim().is_empty());
    }
    let split: Vec<Vec<&str>> = names.iter().map(|n| n.split_whitespace().collect()).collect();
    let mut common = Vec::new();
    for (i, w) in split[0].iter().enumerate() {
        if split
            .iter()
            .all(|s| s.get(i).is_some_and(|x| x.eq_ignore_ascii_case(w)))
        {
            common.push(*w);
        } else {
            break;
        }
    }
    // A name made only of the shared words leaves nothing to tell the entities apart
    // except the parameter itself, which is fine; but a prefix of one or two letters
    // templates too much.
    let p = common.join(" ");
    (p.chars().count() >= 3).then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_fill_known_names_only() {
        let v: HashMap<String, String> = [
            ("name".to_string(), "Orders Queue".to_string()),
            ("project".to_string(), "Shop".to_string()),
        ]
        .into_iter()
        .collect();
        assert_eq!(fill("{name|kebab}", &v).unwrap(), "orders-queue");
        assert_eq!(
            fill("{project|alnum}{name|alnum|max:8}", &v).unwrap(),
            "shopordersqu"
        );
        assert_eq!(fill("{name|slug} x", &v).unwrap(), "orders_queue x");
        // Unknown names, JSON and interpolations pass through.
        assert_eq!(
            fill("{\"a\": {other}} ${var.x}", &v).unwrap(),
            "{\"a\": {other}} ${var.x}"
        );
        assert!(fill("{name|bogus}", &v).is_err());
    }

    #[test]
    fn builtin_patterns_are_consistent_with_the_catalog() {
        let cat = Catalog::builtin();
        let all = builtins();
        assert!(all.len() >= 4);
        for p in &all {
            let problems = check(p, &cat);
            assert!(problems.is_empty(), "{}: {problems:#?}", p.name);
        }
        for want in [
            "hardened bucket",
            "queue with dead-letter queue and alarms",
            "queue-driven GPU worker",
            "web service behind a load balancer",
        ] {
            assert!(
                all.iter().any(|p| p.is_named(want)),
                "missing built-in pattern {want}"
            );
        }
    }

    #[test]
    fn expansion_refuses_bad_parameters_and_collisions() {
        let cat = Catalog::builtin();
        let p = ttg_core::project::load_str(include_str!("../../../examples/minimal.ttg.json")).unwrap();
        let hb = builtins()
            .into_iter()
            .find(|x| x.is_named("hardened bucket"))
            .unwrap();
        let mut params = Map::new();
        params.insert("bogus".into(), json!("x"));
        let e = expand(&hb, &params, &p, &cat, Position::default(), None).unwrap_err();
        assert!(e.contains("unknown parameter \"bogus\""), "{e}");
        let mut params = Map::new();
        params.insert("key".into(), json!("worker"));
        let e = expand(&hb, &params, &p, &cat, Position::default(), None).unwrap_err();
        assert!(e.contains("is a function, not a encryption_key"), "{e}");
        let mut params = Map::new();
        params.insert("name".into(), json!("worker"));
        let e = expand(&hb, &params, &p, &cat, Position::default(), None).unwrap_err();
        assert!(e.contains("would collide"), "{e}");
        // A good one: entity_add first, links last, names templated.
        let x = expand(&hb, &Map::new(), &p, &cat, Position { x: 100, y: 50 }, None).unwrap();
        assert_eq!(x.steps[0].0, "entity_add");
        assert_eq!(x.steps.last().unwrap().0, "link_add");
        assert!(x
            .entities
            .iter()
            .any(|(_, n, t)| n == "records" && t == "object_storage"));
    }

    #[test]
    fn infer_makes_a_name_parameter_and_entity_parameters() {
        let p = ttg_core::project::load_str(include_str!("../../../examples/minimal.ttg.json")).unwrap();
        let pat = infer(&p, &["worker".to_string()], "Worker", "").unwrap();
        assert_eq!(pat.params[0].name, "name");
        assert_eq!(pat.params[0].default.as_deref(), Some("worker"));
        assert_eq!(pat.entities[0].name, "{name}");
        // The role it runs as is outside the selection: an entity parameter.
        let role = pat.params.iter().find(|x| x.entity.is_some()).unwrap();
        assert_eq!(role.entity.as_deref(), Some("iam_role"));
        assert_eq!(pat.links[0].target, format!("${}", role.name));
        assert!(check(&pat, &Catalog::builtin()).is_empty());
        // Round trip through the file format.
        assert_eq!(parse(&to_file_text(&pat)).unwrap(), pat);
    }
}
