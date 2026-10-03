//! Adopting existing cloud resources: an entity's `adopt` map gives, per provider, the
//! cloud id of a resource that already exists for one of the blocks it generates, and
//! the export writes an `import` block for each into `imports.tf` (Terraform 1.5+,
//! OpenTofu 1.6+), so the first `plan` takes the resource over instead of creating it.
//!
//! This is separate from the "external" flag (`manual`), which generates nothing and
//! turns references into input variables: an adopted entity is generated in full and
//! managed from then on.
//!
//! A key names a block the way the rest of the project does — `main`, a secondary block
//! key, `<block>/<row key>` for one instance of a repeated block — or is the address
//! itself (`aws_vpc.core`). Repeated blocks are written as separate resources (one per
//! row, named by key), so every instance has an address of its own and no `for_each`
//! import is ever needed.

use crate::diagnostics::{Code, Diagnostic, Severity};
use crate::plan::{KeySel, Plan};
use hcl::Block;
use std::collections::HashSet;
use ttg_catalog::Catalog;
use ttg_core::{Id, Project};

/// One `import` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportTarget {
    pub entity: Id,
    /// The adopt key as written.
    pub key: String,
    pub resource_type: String,
    pub local: String,
    /// The existing resource's id in the cloud.
    pub id: String,
}

impl ImportTarget {
    pub fn address(&self) -> String {
        format!("{}.{}", self.resource_type, self.local)
    }
}

/// What an adopt key names in the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyTarget {
    /// A resource instance: its type and local name.
    Resource(String, String),
    /// A data source: read, never imported.
    Data(String),
    /// A repeated block with several instances and no row key.
    Ambiguous(Vec<String>),
    Unknown,
}

/// Resolve one adopt key of entity `id` against `plan`.
pub fn resolve_key(plan: &Plan, id: &str, key: &str) -> KeyTarget {
    let key = key.trim();
    // A full address of one of the entity's blocks.
    for ((eid, _), pl) in &plan.blocks {
        if eid != id {
            continue;
        }
        for i in &pl.instances {
            if format!("{}.{}", pl.resource, i.local) == key {
                return KeyTarget::Resource(pl.resource.clone(), i.local.clone());
            }
        }
    }
    for ((eid, _), pl) in &plan.data {
        if eid != id {
            continue;
        }
        for i in &pl.instances {
            let a = format!("data.{}.{}", pl.resource, i.local);
            if a == key {
                return KeyTarget::Data(a);
            }
        }
    }
    let (block, row) = match key.split_once('/') {
        Some((b, r)) => (b.trim(), Some(r.trim())),
        None => (key, None),
    };
    if let Some(pl) = plan.blocks.get(&(id.to_string(), block.to_string())) {
        let inst = match row {
            Some(r) => pl
                .find(&KeySel::Key(r.to_string()))
                .or_else(|| r.parse::<usize>().ok().and_then(|n| pl.find(&KeySel::Index(n)))),
            None if pl.instances.len() == 1 => pl.instances.first(),
            None => {
                return KeyTarget::Ambiguous(
                    pl.instances
                        .iter()
                        .map(|i| format!("{block}/{}", i.key.clone().unwrap_or_else(|| i.row.to_string())))
                        .collect(),
                )
            }
        };
        return match inst {
            Some(i) => KeyTarget::Resource(pl.resource.clone(), i.local.clone()),
            None => KeyTarget::Unknown,
        };
    }
    if let Some(pl) = plan.data.get(&(id.to_string(), block.to_string())) {
        if let Some(i) = pl.instances.first() {
            return KeyTarget::Data(format!("data.{}.{}", pl.resource, i.local));
        }
    }
    KeyTarget::Unknown
}

/// Every resource address entity `id` generates in `plan`, with the adopt key that
/// names it (`main`, `<block>`, `<block>/<row key>`), in block order.
pub fn adoptable(plan: &Plan, id: &str) -> Vec<(String, String)> {
    let mut blocks: Vec<(&String, &crate::plan::Planned)> = plan
        .blocks
        .iter()
        .filter(|((eid, _), _)| eid == id)
        .map(|((_, k), pl)| (k, pl))
        .collect();
    blocks.sort_by_key(|(_, pl)| pl.order);
    let mut out = Vec::new();
    for (k, pl) in blocks {
        for i in &pl.instances {
            let key = if pl.repeated {
                format!("{k}/{}", i.key.clone().unwrap_or_else(|| i.row.to_string()))
            } else {
                k.clone()
            };
            out.push((key, format!("{}.{}", pl.resource, i.local)));
        }
    }
    out
}

/// The `import` blocks for `layer` (the provider's layer, as the emitter sees it).
/// Only keys that resolve to a resource the export writes (`written`) are included;
/// the rest are reported by [`checks`].
pub fn targets(layer: &Project, plan: &Plan, provider: &str, written: &HashSet<String>) -> Vec<ImportTarget> {
    let mut out = Vec::new();
    for e in layer.entities() {
        if e.manual {
            continue;
        }
        let Some(ids) = layer.adoption(e.id).and_then(|a| a.get(provider)) else {
            continue;
        };
        for (key, cloud_id) in ids {
            if cloud_id.trim().is_empty() {
                continue;
            }
            if let KeyTarget::Resource(t, l) = resolve_key(plan, e.id, key) {
                if written.contains(&format!("{t}.{l}")) {
                    out.push(ImportTarget {
                        entity: e.id.to_string(),
                        key: key.clone(),
                        resource_type: t,
                        local: l,
                        id: cloud_id.trim().to_string(),
                    });
                }
            }
        }
    }
    // One block per address: the same resource adopted under two keys keeps the first.
    let mut seen = HashSet::new();
    out.retain(|t| seen.insert(t.address()));
    out
}

/// `imports.tf`, or `None` when nothing is adopted.
pub fn render(header: &str, targets: &[ImportTarget]) -> Option<String> {
    if targets.is_empty() {
        return None;
    }
    let blocks: Vec<Block> = targets
        .iter()
        .map(|t| {
            Block::builder("import")
                .add_attribute(("to", crate::emit::traversal(&t.resource_type, &t.local, "")))
                .add_attribute(("id", hcl::Expression::String(t.id.clone())))
                .build()
        })
        .collect();
    let intro = format!(
        "{header}\n# Existing resources this configuration adopts (the entities' \"Adopt existing\" ids).\n\
         # The first plan imports each into the state instead of creating it; review that plan.\n\
         # Once every state using this configuration has been applied, these blocks do nothing.\n"
    );
    Some(crate::files::render_providers(&intro, &blocks))
}

/// Diagnostics for adopted entities on one provider's layer (`Code::Adopt`, warnings):
/// an adopted entity flagged external, a key that names no block it generates (or a
/// data source, or a repeated block without its row key), and an adopted entity whose
/// other blocks have no id and will therefore be created.
pub fn checks(layer: &Project, cat: &Catalog, provider: &str, plan: &Plan) -> Vec<Diagnostic> {
    let pname = cat
        .provider(provider)
        .map(|d| d.provider.display_name.clone())
        .unwrap_or(provider.to_string());
    let mut out = Vec::new();
    let mut push = |id: &str, msg: String| {
        out.push(Diagnostic {
            entity: Some(id.to_string()),
            severity: Severity::Warning,
            code: Code::Adopt,
            message: msg,
            provider: None,
        })
    };
    for e in layer.entities() {
        let Some(ids) = layer.adoption(e.id).and_then(|a| a.get(provider)) else {
            continue;
        };
        if ids.is_empty() {
            continue;
        }
        if e.manual {
            push(
                e.id,
                format!(
                    "adopts existing resources on {pname} but is flagged external, so nothing is generated or imported; clear one or the other"
                ),
            );
            continue;
        }
        let blocks = adoptable(plan, e.id);
        let keys: Vec<&str> = blocks.iter().map(|(k, _)| k.as_str()).collect();
        let mut covered = HashSet::new();
        for (key, cloud_id) in ids {
            match resolve_key(plan, e.id, key) {
                KeyTarget::Resource(t, l) => {
                    if cloud_id.trim().is_empty() {
                        continue;
                    }
                    covered.insert(format!("{t}.{l}"));
                }
                KeyTarget::Data(a) => push(
                    e.id,
                    format!(
                        "adopt key \"{key}\" is the data source {a}: data sources are read, not imported"
                    ),
                ),
                KeyTarget::Ambiguous(rows) => push(
                    e.id,
                    format!(
                        "adopt key \"{key}\" is a block repeated once per row; give one instance: {}",
                        rows.join(", ")
                    ),
                ),
                KeyTarget::Unknown => push(
                    e.id,
                    format!(
                        "adopt key \"{key}\" names no block it generates on {pname} (blocks: {})",
                        if keys.is_empty() {
                            "none".to_string()
                        } else {
                            keys.join(", ")
                        }
                    ),
                ),
            }
        }
        if covered.is_empty() {
            continue;
        }
        let missing: Vec<&str> = blocks
            .iter()
            .filter(|(_, a)| !covered.contains(a))
            .map(|(_, a)| a.as_str())
            .collect();
        if !missing.is_empty() {
            push(
                e.id,
                format!(
                    "is adopted on {pname}, but {} {} no existing id and will be created: {}",
                    missing.len(),
                    if missing.len() == 1 {
                        "block has"
                    } else {
                        "blocks have"
                    },
                    missing.join(", ")
                ),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> (Project, Catalog) {
        let text = include_str!("../../../examples/minimal.ttg.json");
        (ttg_core::project::load_str(text).unwrap(), Catalog::builtin())
    }

    #[test]
    fn keys_resolve_to_addresses() {
        let (p, cat) = project();
        let layer = crate::layers::project_for(&p, &cat, "aws");
        let plan = Plan::build(&layer, &cat, "aws", None);
        assert_eq!(
            resolve_key(&plan, "net", "main"),
            KeyTarget::Resource("aws_vpc".into(), "main".into())
        );
        assert_eq!(
            resolve_key(&plan, "net", "aws_vpc.main"),
            KeyTarget::Resource("aws_vpc".into(), "main".into())
        );
        assert_eq!(resolve_key(&plan, "net", "nope"), KeyTarget::Unknown);
        let keys: Vec<String> = adoptable(&plan, "worker-role")
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert!(keys.contains(&"main".to_string()), "{keys:?}");
    }
}
