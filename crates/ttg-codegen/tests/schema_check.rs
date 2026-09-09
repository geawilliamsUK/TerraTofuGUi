//! Every curated mapping must agree with the real provider schema: the resource types it
//! emits exist, and every static argument / nested block it writes is one the provider
//! accepts. Catches provider renames before a user's export does.

use ttg_catalog::{BlockDef, Catalog, NestedBlockDef};
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

fn check_block(provider: &str, b: &BlockDef, at: &str, errs: &mut Vec<String>) {
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
                check_block(pid, b, &format!("{tid}/{pid}/{}", b.key), &mut errs);
            }
        }
    }
    assert!(
        errs.is_empty(),
        "curated definitions disagree with the provider schemas:\n{}",
        errs.join("\n")
    );
}
