//! Which abstract fields a provider mapping actually consumes. Used by the inspector to
//! tell the user when a field is irrelevant for the selected target provider.

use crate::schema::*;
use std::collections::HashSet;

/// Abstract field names referenced anywhere in a mapping: argument sources, template
/// placeholders, lookup tables, `for_each_field`, data blocks and `when` conditions.
pub fn used_fields(m: &ProviderMapping) -> HashSet<String> {
    let mut set = HashSet::new();
    for b in m.blocks.iter().chain(m.data.iter()) {
        if let Some(f) = &b.for_each_field {
            set.insert(f.clone());
        }
        if let Some(c) = &b.when {
            scan_cond(c, &mut set);
        }
        for src in b.args.values() {
            scan(src, &mut set);
        }
        for n in &b.nested {
            scan_nested(n, &mut set);
        }
    }
    set
}

/// Providers (ids) whose mapping uses the given abstract field.
pub fn providers_using(def: &ResourceDef, field: &str) -> Vec<String> {
    def.providers
        .iter()
        .filter(|(_, m)| used_fields(m).contains(field))
        .map(|(id, _)| id.clone())
        .collect()
}

fn scan_nested(n: &NestedBlockDef, set: &mut HashSet<String>) {
    if let Some(f) = &n.for_each_field {
        set.insert(f.clone());
    }
    if let Some(c) = &n.when {
        scan_cond(c, set);
    }
    for src in n.args.values() {
        scan(src, set);
    }
    for inner in &n.nested {
        scan_nested(inner, set);
    }
}

fn scan_cond(c: &Condition, set: &mut HashSet<String>) {
    match c {
        Condition::Field(f) => {
            set.insert(f.field.clone());
        }
        Condition::All(a) => a.all.iter().for_each(|x| scan_cond(x, set)),
        Condition::Any(a) => a.any.iter().for_each(|x| scan_cond(x, set)),
        _ => {}
    }
}

fn scan(src: &ArgSource, set: &mut HashSet<String>) {
    match src {
        ArgSource::Relation(r) => {
            if let Some(fb) = &r.fallback {
                scan(fb, set);
            }
        }
        ArgSource::Field(f) => {
            set.insert(f.field.clone());
            if let Some(fb) = &f.fallback {
                scan(fb, set);
            }
        }
        ArgSource::ProviderField(f) => {
            if let Some(fb) = &f.fallback {
                scan(fb, set);
            }
        }
        ArgSource::Template(t) => {
            for ph in crate::fields::template_placeholders(&t.template) {
                if !ph.starts_with("provider.") && !ph.starts_with("settings.") && !ph.starts_with("item.") {
                    set.insert(ph);
                }
            }
        }
        ArgSource::Map(m) => {
            set.insert(m.map.clone());
        }
        ArgSource::If(i) => {
            scan_cond(&i.cond, set);
            scan(&i.then, set);
            if let Some(o) = &i.otherwise {
                scan(o, set);
            }
        }
        ArgSource::Object(o) => o.object.values().for_each(|x| scan(x, set)),
        ArgSource::List(l) => l.list.iter().for_each(|x| scan(x, set)),
        ArgSource::Func(f) => f.args.iter().for_each(|x| scan(x, set)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Catalog;

    #[test]
    fn compute_size_used_by_both_providers() {
        let cat = Catalog::builtin();
        let def = cat.resource("compute_instance").unwrap();
        let mut users = providers_using(def, "size");
        users.sort();
        assert_eq!(users, vec!["aws", "azure", "gcp"]);
        // trusted_service only drives the AWS trust policy and instance profile.
        let role = cat.resource("iam_role").unwrap();
        assert_eq!(providers_using(role, "trusted_service"), vec!["aws"]);
    }
}
