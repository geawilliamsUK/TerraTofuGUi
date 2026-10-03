//! Which blocks an export generates and what each one is called — decided once, before
//! anything is resolved, so that references (`relation`, `self_block`, `$ref`, `$raw`)
//! can be checked against it and the emitter and the diagnostics agree on every address.
//!
//! A plain block is one instance named `<slug>` (the `main` block) or `<slug>_<key>`. A
//! repeated block (`for_each_field` / `for_each_relation`) has one instance per row that
//! survives its `when`, named after the row's key rather than its position, so that
//! reordering a list does not replace the resources it made:
//!
//! - a relation row is keyed by the target entity's name,
//! - a `string_list` entry by its value,
//! - a `struct_list` row by its `for_each_key` item, else its `name` item.
//!
//! The key is slugified into the local name (`ecr_repo_zipos_web`). Two rows whose keys
//! slugify alike get `_2`, `_3`, … in row order; a row with no key keeps its row index.
//! Data sources collapse instead: rows sharing a key share one lookup.
//!
//! Before keys, repeated blocks were named by row index (`ecr_repo_0`). Every instance
//! whose name changed records that `legacy` name, and the export writes a `moved` block
//! from it so an existing state follows the rename instead of replacing the resource.

use crate::diagnostics;
use std::collections::{HashMap, HashSet};
use ttg_catalog::{BlockDef, Catalog, MappingStatus, ProviderMapping};
use ttg_core::{EntityRef, Id, Project, Record, Relation, Value};

/// One generated block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    /// The HCL local name: `aws_ecr_repository.<local>`.
    pub local: String,
    /// Position of the row in the full list of rows (0 for a plain block).
    pub row: usize,
    /// The row's key as written (a repository name, a rule name, the target's display
    /// name); `None` for a plain block or a row without one.
    pub key: Option<String>,
    /// The target entity of a `for_each_relation` row.
    pub target: Option<Id>,
    /// The index-based name this instance had before repeated blocks were named by
    /// key, when it differs from `local` (resources only: data sources hold no state).
    pub legacy: Option<String>,
}

/// The instances of one block of one entity.
#[derive(Debug, Clone)]
pub struct Planned {
    /// The concrete type: `aws_ecr_repository`.
    pub resource: String,
    pub repeated: bool,
    /// Position of the block in its mapping, for output in declaration order.
    pub order: usize,
    pub instances: Vec<Instance>,
}

impl Planned {
    /// The instance a `$ref` key or index picks: the key as written, its slug, or (for
    /// a relation row) the target's id; a number is the row's position among the
    /// instances.
    pub fn find(&self, sel: &KeySel) -> Option<&Instance> {
        match sel {
            KeySel::Index(n) => self.instances.get(*n),
            KeySel::Key(k) => {
                let slug = ttg_core::slugify(k);
                self.instances
                    .iter()
                    .find(|i| i.key.as_deref() == Some(k.as_str()) || i.target.as_deref() == Some(k.as_str()))
                    .or_else(|| {
                        self.instances
                            .iter()
                            .find(|i| i.key.as_deref().is_some_and(|x| ttg_core::slugify(x) == slug))
                    })
            }
        }
    }

    /// The keys a `$ref` may name, for error messages.
    pub fn keys(&self) -> Vec<String> {
        self.instances
            .iter()
            .map(|i| i.key.clone().unwrap_or_else(|| i.row.to_string()))
            .collect()
    }
}

/// How a `$ref` picks one instance of a repeated block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySel {
    Key(String),
    Index(usize),
}

impl KeySel {
    /// `"key": "zipos-web"`, or `"index": "zipos-web"` / `"index": 2` (TF-015's spelling)
    /// from a `$ref` object.
    pub fn from_ref(r: &serde_json::Value) -> Option<KeySel> {
        let v = r.get("key").or_else(|| r.get("index"))?;
        match v {
            serde_json::Value::String(s) => Some(KeySel::Key(s.clone())),
            serde_json::Value::Number(n) => n.as_u64().map(|n| KeySel::Index(n as usize)),
            _ => None,
        }
    }
}

impl std::fmt::Display for KeySel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeySel::Key(k) => write!(f, "key \"{k}\""),
            KeySel::Index(n) => write!(f, "index {n}"),
        }
    }
}

/// Every block the export generates, per `(entity, block key)`.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub blocks: HashMap<(Id, String), Planned>,
    pub data: HashMap<(Id, String), Planned>,
}

impl Plan {
    /// Plan `p`, which must be the provider's layer. `skip` is an entity treated as
    /// external although it is not flagged (the Encryption Key the bootstrap root
    /// creates).
    pub fn build(p: &Project, cat: &Catalog, provider: &str, skip: Option<&str>) -> Plan {
        let mut plan = Plan::default();
        for e in p.entities() {
            if e.manual || skip == Some(e.id) {
                continue;
            }
            let Some(m) = cat.mapping(e.resource_type, provider) else {
                continue;
            };
            if m.status == MappingStatus::Logical {
                continue;
            }
            let (blocks, data) = entity_plan(p, cat, provider, &e, m);
            for (k, v) in blocks {
                plan.blocks.insert((e.id.to_string(), k), v);
            }
            for (k, v) in data {
                plan.data.insert((e.id.to_string(), k), v);
            }
        }
        plan
    }

    /// `type.local` for every resource instance and `data.type.local` for every data
    /// source, mapped to the entity that generates it.
    pub fn addresses(&self) -> HashMap<String, Id> {
        let mut out = HashMap::new();
        for ((id, _), pl) in &self.blocks {
            for i in &pl.instances {
                out.insert(format!("{}.{}", pl.resource, i.local), id.clone());
            }
        }
        for ((id, _), pl) in &self.data {
            for i in &pl.instances {
                out.insert(format!("data.{}.{}", pl.resource, i.local), id.clone());
            }
        }
        out
    }

    /// Old index-based address -> the address the instance has now, for every renamed
    /// instance whose old address is not itself generated.
    pub fn legacy_addresses(&self) -> HashMap<String, String> {
        let current: HashSet<String> = self.addresses().into_keys().collect();
        let mut out = HashMap::new();
        for pl in self.blocks.values() {
            for i in &pl.instances {
                if let Some(old) = &i.legacy {
                    let from = format!("{}.{old}", pl.resource);
                    if !current.contains(&from) {
                        out.insert(from, format!("{}.{}", pl.resource, i.local));
                    }
                }
            }
        }
        out
    }
}

/// A block key and what it generates.
pub(crate) type KeyedPlan = (String, Planned);

/// The resource and data instances one entity generates, by block key.
pub(crate) fn entity_plan(
    p: &Project,
    cat: &Catalog,
    provider: &str,
    e: &EntityRef,
    m: &ProviderMapping,
) -> (Vec<KeyedPlan>, Vec<KeyedPlan>) {
    let mut blocks = Vec::new();
    for (order, b) in m.blocks.iter().enumerate() {
        let base = local_name(p, e.id, &b.key);
        if let Some(pl) = plan_block(p, cat, provider, e, b, &base, false, order) {
            blocks.push((b.key.clone(), pl));
        }
    }
    let mut data = Vec::new();
    for (order, d) in m.data.iter().enumerate() {
        let base = data_local_name(p, e.id, &d.key);
        if let Some(pl) = plan_block(p, cat, provider, e, d, &base, true, order) {
            data.push((d.key.clone(), pl));
        }
    }
    (blocks, data)
}

#[allow(clippy::too_many_arguments)]
fn plan_block(
    p: &Project,
    cat: &Catalog,
    provider: &str,
    e: &EntityRef,
    b: &BlockDef,
    base: &str,
    is_data: bool,
    order: usize,
) -> Option<Planned> {
    let rows = rows_for(p, cat, provider, e, b);
    let surviving: Vec<(usize, &Row)> = rows
        .iter()
        .enumerate()
        .filter(|(n, row)| {
            b.when.as_ref().is_none_or(|c| {
                diagnostics::condition_holds_for(
                    p,
                    cat,
                    provider,
                    e,
                    c,
                    row.record.as_ref().map(|r| (r, *n)),
                    row.target.as_deref(),
                )
            })
        })
        .collect();
    if surviving.is_empty() {
        return None;
    }
    let repeated = b.repeated();
    let instances = if !repeated {
        vec![Instance {
            local: base.to_string(),
            row: 0,
            key: None,
            target: None,
            legacy: None,
        }]
    } else {
        let mut used: HashSet<String> = HashSet::new();
        let mut out = Vec::new();
        for (n, row) in surviving {
            let key = row_key(p, b, row);
            let mut local = match &key {
                Some(k) => format!("{base}_{}", ttg_core::slugify(k)),
                None => format!("{base}_{n}"),
            };
            if used.contains(&local) {
                if is_data {
                    // The same lookup again: one data source serves every row.
                    continue;
                }
                let stem = local.clone();
                let mut i = 2;
                while used.contains(&local) {
                    local = format!("{stem}_{i}");
                    i += 1;
                }
            }
            used.insert(local.clone());
            let old = format!("{base}_{n}");
            out.push(Instance {
                legacy: (!is_data && old != local).then_some(old),
                local,
                row: n,
                key,
                target: row.target.clone(),
            });
        }
        out
    };
    Some(Planned {
        resource: b.resource.clone(),
        repeated,
        order,
        instances,
    })
}

/// The key a row's instance is named after (see the module documentation).
fn row_key(p: &Project, b: &BlockDef, row: &Row) -> Option<String> {
    if let Some(t) = &row.target {
        return p.entity(t).map(|x| x.name.to_string());
    }
    let r = row.record.as_ref()?;
    let item = match b.for_each_key.as_deref() {
        Some(k) => k,
        None if r.contains_key("name") => "name",
        None => "value",
    };
    r.get(item).map(|v| v.display()).filter(|s| !s.trim().is_empty())
}

/// `<slug>` for the `main` block, `<slug>_<key>` for any other.
pub(crate) fn local_name(p: &Project, id: &str, key: &str) -> String {
    let slug = p.hcl_name(id);
    if key == "main" {
        slug
    } else {
        format!("{slug}_{key}")
    }
}

/// Data sources follow the same rule: a native data source's single `main` block is
/// `data.<type>.<slug>`, a mapping's helper lookup `data.<type>.<slug>_<key>`.
pub(crate) fn data_local_name(p: &Project, id: &str, key: &str) -> String {
    local_name(p, id, key)
}

/// One iteration of a repeated block: a struct_list row, a string_list entry, or a
/// relation target. A plain block has a single row with neither.
#[derive(Debug, Clone, Default)]
pub(crate) struct Row {
    pub record: Option<Record>,
    pub target: Option<Id>,
}

pub(crate) fn rows_for(p: &Project, cat: &Catalog, provider: &str, e: &EntityRef, b: &BlockDef) -> Vec<Row> {
    rows(
        p,
        cat,
        provider,
        e,
        b.for_each_field.as_deref(),
        b.for_each_relation.as_deref(),
        b.for_each_target_type.as_deref(),
    )
}

/// The rows a block iterates: one per relation target, one per struct_list row /
/// string_list entry, or a single empty row for a plain block.
pub(crate) fn rows(
    p: &Project,
    cat: &Catalog,
    provider: &str,
    e: &EntityRef,
    field: Option<&str>,
    relation: Option<&str>,
    target_type: Option<&str>,
) -> Vec<Row> {
    if let Some(rel) = relation {
        let Some(kind) = Relation::from_key(rel) else {
            return Vec::new();
        };
        return diagnostics::relation_targets_of_type(p, cat, e, kind, target_type)
            .into_iter()
            .map(|t| Row {
                record: None,
                target: Some(t),
            })
            .collect();
    }
    let Some(field) = field else {
        return vec![Row::default()];
    };
    match e.field(field).or_else(|| e.provider_field(provider, field)) {
        Some(Value::Records(rows)) => rows
            .iter()
            .map(|r| Row {
                record: Some(r.clone()),
                target: None,
            })
            .collect(),
        Some(Value::List(items)) => items
            .iter()
            .map(|s| {
                let mut r = Record::new();
                r.insert("value".into(), Value::Str(s.clone()));
                Row {
                    record: Some(r),
                    target: None,
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}
