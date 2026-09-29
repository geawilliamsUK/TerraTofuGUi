//! Every curated mapping must agree with the real provider schema: the resource types it
//! emits exist, and every static argument / nested block it writes is one the provider
//! accepts. Catches provider renames before a user's export does.
//!
//! The check itself is `ttg_codegen::versions::mapping_findings`, which the diagnostics
//! also run over the types a project uses, so a project pinned to the schema's major
//! version hears about a mapping that breaks on it.

use ttg_catalog::{ArgSource, Catalog};
use ttg_codegen::versions::mapping_findings;

#[test]
fn curated_mappings_match_provider_schemas() {
    let cat = Catalog::builtin();
    let mut errs = Vec::new();
    for (tid, def) in &cat.resources {
        for pid in def.providers.keys() {
            errs.extend(mapping_findings(&cat, pid, tid));
        }
    }
    assert!(
        errs.is_empty(),
        "curated definitions disagree with the provider schemas:\n{}",
        errs.join("\n")
    );
}

/// Every `self_block` a source reaches, with the attribute it reads.
fn self_block_attrs(src: &ArgSource, out: &mut Vec<(String, String)>) {
    match src {
        ArgSource::SelfBlock(s) => out.push((s.self_block.clone(), s.attr.clone())),
        ArgSource::If(i) => {
            self_block_attrs(&i.then, out);
            if let Some(o) = &i.otherwise {
                self_block_attrs(o, out);
            }
        }
        ArgSource::Func(f) => f.args.iter().for_each(|a| self_block_attrs(a, out)),
        ArgSource::Raw(r) => r.refs.values().for_each(|a| self_block_attrs(a, out)),
        ArgSource::Object(o) => o.object.values().for_each(|a| self_block_attrs(a, out)),
        ArgSource::List(l) => l.list.iter().for_each(|a| self_block_attrs(a, out)),
        _ => {}
    }
}

/// The `connection` values the Kubernetes manifests read become outputs, so an attribute
/// the resource does not have would only fail at `validate`. Check them here instead.
#[test]
fn connection_values_read_real_attributes() {
    let cat = Catalog::builtin();
    let idx = ttg_schema::index();
    let mut errs = Vec::new();
    let mut seen = 0;
    for (tid, def) in &cat.resources {
        for (pid, m) in &def.providers {
            for (key, src) in &m.connection {
                let mut refs = Vec::new();
                self_block_attrs(src, &mut refs);
                for (block, attr) in refs {
                    seen += 1;
                    let Some(b) = m.blocks.iter().find(|b| b.key == block) else {
                        errs.push(format!("{tid}/{pid} connection {key}: no block '{block}'"));
                        continue;
                    };
                    let root = attr.split('.').next().unwrap_or(&attr);
                    match idx.resource(pid, &b.resource) {
                        Some(s) if s.has(root) => {}
                        Some(_) => errs.push(format!(
                            "{tid}/{pid} connection {key}: {} has no attribute '{root}'",
                            b.resource
                        )),
                        None => errs.push(format!(
                            "{tid}/{pid} connection {key}: unknown resource {}",
                            b.resource
                        )),
                    }
                }
            }
        }
    }
    assert!(seen > 10, "the curated catalog declares connection values");
    assert!(errs.is_empty(), "{}", errs.join("\n"));
}
