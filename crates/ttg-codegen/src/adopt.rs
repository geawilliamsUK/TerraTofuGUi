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

/// The address the plan gives one instance of a block: the one place adoption and the
/// directory import spell it, kept to what [`Plan::addresses`] says.
pub(crate) fn instance_address(pl: &crate::plan::Planned, i: &crate::plan::Instance, data: bool) -> String {
    if data {
        format!("data.{}.{}", pl.resource, i.local)
    } else {
        format!("{}.{}", pl.resource, i.local)
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
            if instance_address(pl, i, false) == key {
                return KeyTarget::Resource(pl.resource.clone(), i.local.clone());
            }
        }
    }
    for ((eid, _), pl) in &plan.data {
        if eid != id {
            continue;
        }
        for i in &pl.instances {
            let a = instance_address(pl, i, true);
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
            return KeyTarget::Data(instance_address(pl, i, true));
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
            out.push((key, instance_address(pl, i, false)));
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
    let blocks: Vec<(&ImportTarget, hcl::Expression)> = targets
        .iter()
        .map(|t| (t, hcl::Expression::String(t.id.clone())))
        .collect();
    render_blocks(header, &blocks)
}

fn render_blocks(header: &str, targets: &[(&ImportTarget, hcl::Expression)]) -> Option<String> {
    if targets.is_empty() {
        return None;
    }
    let blocks: Vec<Block> = targets
        .iter()
        .map(|(t, id)| {
            Block::builder("import")
                .add_attribute(("to", crate::emit::traversal(&t.resource_type, &t.local, "")))
                .add_attribute(("id", id.clone()))
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

// ------------------------------------------------------------------ environments

/// `imports.tf` for the merged root of a project with environments. Each environment's
/// run is planned on its own resolved project (`resolved`, in the order of `gens`); an
/// import is written once for every adopted block every environment generates, with the
/// id as given, or — where the environments' ids differ (a `${var.…}` in the id) — a
/// lookup on `var.environment`. A block some environment leaves out is counted by the
/// merge, and an import block cannot depend on the environment, so that is refused.
pub fn environment_imports(
    cat: &Catalog,
    provider: &str,
    tool: ttg_core::Tool,
    resolved: &[Project],
    gens: &[(String, crate::emit::Generated)],
) -> Result<Option<String>, Vec<Diagnostic>> {
    let mut per_env: Vec<Vec<ImportTarget>> = Vec::new();
    for (rp, (_, g)) in resolved.iter().zip(gens) {
        let layer = crate::layers::project_for(rp, cat, provider);
        let plan = Plan::build(
            &layer,
            cat,
            provider,
            crate::refs::bootstrap_key(&layer, cat, provider).as_deref(),
        );
        let written: HashSet<String> = g
            .entity_blocks
            .values()
            .flat_map(|b| b.addresses.iter().filter(|a| !a.starts_with("data.")).cloned())
            .collect();
        per_env.push(targets(&layer, &plan, provider, &written));
    }
    // Every (entity, address) some environment imports, in first-seen order.
    let mut order: Vec<(Id, String)> = Vec::new();
    for t in per_env.iter().flatten() {
        let k = (t.entity.clone(), t.address());
        if !order.contains(&k) {
            order.push(k);
        }
    }
    let pname = cat
        .provider(provider)
        .map(|d| d.provider.display_name.clone())
        .unwrap_or(provider.to_string());
    let mut problems = Vec::new();
    let mut blocks: Vec<(ImportTarget, hcl::Expression)> = Vec::new();
    for (entity, address) in order {
        let per: Vec<Option<&ImportTarget>> = per_env
            .iter()
            .map(|ts| ts.iter().find(|t| t.entity == entity && t.address() == address))
            .collect();
        let first = per.iter().flatten().next().copied().expect("seen").clone();
        if per.iter().any(Option::is_none) {
            let name = resolved
                .iter()
                .find_map(|rp| rp.entity(&entity).map(|e| e.name.to_string()))
                .unwrap_or(entity.clone());
            let present: Vec<&str> = gens
                .iter()
                .zip(&per)
                .filter(|(_, t)| t.is_some())
                .map(|(g, _)| g.0.as_str())
                .collect();
            problems.push(Diagnostic {
                entity: Some(entity.clone()),
                severity: Severity::Error,
                code: Code::Adopt,
                message: format!(
                    "\"{name}\" adopts {address} on {pname}, which only {} {} generates: the export counts it in the others, and an import block cannot depend on the environment; adopt only what every environment has",
                    if present.len() == 1 { "the environment" } else { "the environments" },
                    present.join(", ")
                ),
                provider: None,
            });
            continue;
        }
        let ids: Vec<&str> = per.iter().flatten().map(|t| t.id.as_str()).collect();
        let id = if ids.iter().all(|i| *i == ids[0]) {
            hcl::Expression::String(ids[0].to_string())
        } else {
            let entries: Vec<String> = gens
                .iter()
                .zip(&ids)
                .map(|((env, _), id)| {
                    format!(
                        "{} = {}",
                        serde_json::Value::String(env.clone()),
                        serde_json::Value::String(id.to_string())
                    )
                })
                .collect();
            crate::emit::raw_expr(&format!(
                "{{ {} }}[var.{}]",
                entries.join(", "),
                crate::environments::ENVIRONMENT_VAR
            ))
        };
        blocks.push((first, id));
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    let pdef = cat.provider(provider).expect("known provider");
    let header = crate::tool::Profile::new(tool).file_header(&pdef.provider.display_name);
    let refs: Vec<(&ImportTarget, hcl::Expression)> = blocks.iter().map(|(t, e)| (t, e.clone())).collect();
    Ok(render_blocks(&header, &refs))
}

/// An adopted entity that some environment leaves out (`absent` in its overrides): an
/// error, since its blocks are counted there and an import block cannot follow that.
/// Read from the project as written, before the overrides are folded away.
pub fn environment_checks(full: &Project, cat: &Catalog, provider: &str) -> Vec<Diagnostic> {
    if full.settings.environments.is_empty() {
        return Vec::new();
    }
    let pname = cat
        .provider(provider)
        .map(|d| d.provider.display_name.clone())
        .unwrap_or(provider.to_string());
    let mut out = Vec::new();
    for e in full.entities() {
        let adopted = full
            .adoption(e.id)
            .and_then(|a| a.get(provider))
            .is_some_and(|ids| ids.values().any(|v| !v.trim().is_empty()));
        if !adopted || e.manual {
            continue;
        }
        let absent: Vec<&str> = full
            .settings
            .environments
            .iter()
            .filter(|env| {
                full.overrides_of(e.id)
                    .and_then(|o| o.get(env.as_str()))
                    .is_some_and(|o| o.absent)
            })
            .map(String::as_str)
            .collect();
        if !absent.is_empty() {
            out.push(Diagnostic {
                entity: Some(e.id.to_string()),
                severity: Severity::Error,
                code: Code::Adopt,
                message: format!(
                    "adopts existing resources on {pname} but is absent in {}: the export counts it there, and an import block cannot depend on the environment; make it present everywhere or drop the adoption",
                    absent.join(", ")
                ),
                provider: None,
            });
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

    /// Adoption and the directory import spell addresses as the plan does.
    #[test]
    fn instance_addresses_agree_with_the_plan() {
        let cat = Catalog::builtin();
        for text in [
            include_str!("../../../examples/kubernetes.ttg.json"),
            include_str!("../../../examples/containers.ttg.json"),
        ] {
            let p = ttg_core::project::load_str(text).unwrap();
            let layer = crate::layers::project_for(&p, &cat, "aws");
            let plan = Plan::build(&layer, &cat, "aws", None);
            let mut mine: Vec<String> = Vec::new();
            for (data, map) in [(false, &plan.blocks), (true, &plan.data)] {
                for pl in map.values() {
                    mine.extend(pl.instances.iter().map(|i| instance_address(pl, i, data)));
                }
            }
            mine.sort();
            let mut theirs: Vec<String> = plan.addresses().into_keys().collect();
            theirs.sort();
            assert_eq!(mine, theirs);
        }
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
