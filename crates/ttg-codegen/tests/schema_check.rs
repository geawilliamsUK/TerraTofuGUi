//! Every curated mapping must agree with the real provider schema: the resource types it
//! emits exist, and every static argument / nested block it writes is one the provider
//! accepts. Catches provider renames before a user's export does.

use ttg_catalog::{ArgSource, BlockDef, Catalog, NestedBlockDef};
use ttg_schema::BlockSchema;

/// Meta-arguments every resource accepts regardless of its schema.
const META: &[&str] = &["depends_on", "count", "for_each", "provider", "lifecycle"];

fn check_nested(schema: &BlockSchema, n: &NestedBlockDef, at: &str, errs: &mut Vec<String>) {
    let Some(ns) = schema.blocks.get(&n.block) else {
        errs.push(format!("{at}: nested block '{}' does not exist", n.block));
        return;
    };
    for k in n.args.keys() {
        let key = k.split('.').next().unwrap_or(k);
        if !ns.block().has(key) {
            errs.push(format!("{at}.{}: argument '{key}' does not exist", n.block));
        }
    }
    for inner in &n.nested {
        check_nested(ns.block(), inner, &format!("{at}.{}", n.block), errs);
    }
}

fn check_block(cat: &Catalog, provider: &str, b: &BlockDef, at: &str, errs: &mut Vec<String>) {
    if b.resource == "terraform_data" {
        return; // built into Terraform / OpenTofu, not part of any provider schema
    }
    // Helper providers (hashicorp/random) are not part of the bundled index.
    if cat.provider(provider).is_some_and(|p| {
        p.helper_providers
            .iter()
            .any(|h| b.resource.starts_with(&h.prefix))
    }) {
        return;
    }
    let idx = ttg_schema::index();
    let Some(schema) = idx.resource(provider, &b.resource) else {
        errs.push(format!(
            "{at}: resource type '{}' does not exist on {provider}",
            b.resource
        ));
        return;
    };
    for k in b.args.keys() {
        let key = k.split('.').next().unwrap_or(k);
        if !schema.has(key) && !META.contains(&key) {
            errs.push(format!("{at} ({}): argument '{key}' does not exist", b.resource));
        }
    }
    for n in &b.nested {
        check_nested(schema, n, &format!("{at} ({})", b.resource), errs);
    }
}

#[test]
fn curated_mappings_match_provider_schemas() {
    let cat = Catalog::builtin();
    let mut errs = Vec::new();
    for (tid, def) in &cat.resources {
        for (pid, m) in &def.providers {
            for b in &m.blocks {
                check_block(&cat, pid, b, &format!("{tid}/{pid}/{}", b.key), &mut errs);
            }
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
