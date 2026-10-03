//! Validation of the definitions themselves. A definition that references an undeclared
//! field, relation, block or variable is rejected at load time so it can never produce
//! silently wrong HCL. Features introduced in schema_version 2 are rejected in
//! schema_version 1 files so that a file's version number is honest.

use crate::schema::*;
use crate::Catalog;
use std::collections::HashSet;
use ttg_core::Relation;

/// Highest definition schema version this build understands.
pub const MAX_SCHEMA_VERSION: u32 = 2;

pub fn catalog(cat: &Catalog) -> Vec<String> {
    let mut errs = Vec::new();

    for (pid, p) in &cat.providers {
        if p.schema_version == 0 || p.schema_version > MAX_SCHEMA_VERSION {
            errs.push(format!(
                "provider '{pid}': unsupported schema_version {}",
                p.schema_version
            ));
        }
        if &p.provider.id != pid {
            errs.push(format!("provider '{pid}': id mismatch"));
        }
        if let Some(a) = &p.required_ancestor {
            if !cat.is_container(a) {
                errs.push(format!(
                    "provider '{pid}': required_ancestor '{a}' is not a container type"
                ));
            }
        }
        for h in &p.helper_providers {
            if h.prefix.trim().is_empty() {
                errs.push(format!(
                    "provider '{pid}': helper provider '{}' has an empty prefix",
                    h.source
                ));
            }
            if !h.source.contains('/') {
                errs.push(format!(
                    "provider '{pid}': helper provider source '{}' must be <namespace>/<name>",
                    h.source
                ));
            }
        }
        if let Some(t) = &p.default_tags {
            match (&t.arg, &t.resource_arg) {
                (Some(_), Some(_)) | (None, None) => errs.push(format!(
                    "provider '{pid}': default_tags needs exactly one of 'arg' and 'resource_arg'"
                )),
                _ => {}
            }
            if t.block.is_some() && t.arg.is_none() {
                errs.push(format!(
                    "provider '{pid}': default_tags 'block' only applies together with 'arg'"
                ));
            }
        }
        let vars: Vec<&str> = p.variables.iter().map(|v| v.name.as_str()).collect();
        let mut ctx = SourceCtx {
            what: format!("provider '{pid}' provider_block"),
            fields: &[],
            provider_fields: &[],
            relations: &[],
            blocks: &[],
            data: &[],
            repeated_data: &[],
            vars: &vars,
            aliases: &[],
            item_fields: None,
            in_relation: false,
            cat,
            v2: false,
        };
        for (k, src) in &p.provider_block.args {
            check_source(&mut ctx, &format!("arg '{k}'"), src, &mut errs);
        }
        for n in &p.provider_block.nested {
            check_nested(&mut ctx, n, &mut errs);
        }
        // Aliases: a second configuration of the same provider, referenced by blocks as
        // `provider_alias` and written as `<local_name>.<name>`, so the name has to be an
        // HCL identifier.
        let mut alias_names = HashSet::new();
        for a in &p.aliases {
            if !is_hcl_identifier(&a.name) {
                errs.push(format!(
                    "provider '{pid}': alias '{}' is not a valid HCL identifier",
                    a.name
                ));
            }
            if !alias_names.insert(a.name.as_str()) {
                errs.push(format!("provider '{pid}': duplicate alias '{}'", a.name));
            }
            ctx.what = format!("provider '{pid}' alias '{}'", a.name);
            for (k, src) in &a.args {
                check_source(&mut ctx, &format!("arg '{k}'"), src, &mut errs);
            }
        }
    }

    for (tid, def) in &cat.resources {
        let where_ = |s: &str| format!("resource '{tid}': {s}");
        if def.schema_version == 0 || def.schema_version > MAX_SCHEMA_VERSION {
            errs.push(where_(&format!(
                "unsupported schema_version {}",
                def.schema_version
            )));
        }
        if def.resource.type_id != *tid {
            errs.push(where_("type id mismatch"));
        }
        if def.resource.category.trim().is_empty() {
            errs.push(where_("category is empty"));
        }
        for p in &def.resource.providers {
            if !cat.providers.contains_key(p) {
                errs.push(where_(&format!("providers entry '{p}' is not a known provider")));
            } else if !def.providers.contains_key(p) {
                errs.push(where_(&format!(
                    "scoped to provider '{p}' but has no mapping for it"
                )));
            }
        }
        for p in &def.resource.allowed_parents {
            if !cat.is_container(p) {
                errs.push(where_(&format!(
                    "allowed_parents entry '{p}' is not a container type"
                )));
            }
        }
        let mut uses_v2 = false;
        // Only the types the Kubernetes export reads may hand it fields and links.
        let manifest_type = MANIFEST_TYPES.contains(&tid.as_str());
        if let Some(prefix) = &def.resource.env_prefix {
            uses_v2 = true;
            if !is_env_name(prefix) {
                errs.push(where_(&format!(
                    "env_prefix '{prefix}' must be upper case letters, digits and '_', starting with a letter"
                )));
            }
        }
        // fields
        let mut names = HashSet::new();
        for f in &def.fields {
            if f.manifests {
                uses_v2 = true;
                if !manifest_type {
                    errs.push(where_(&format!(
                        "field '{}': manifests = true is only valid on a type the Kubernetes export reads ({})",
                        f.name,
                        MANIFEST_TYPES.join(", ")
                    )));
                }
            }
            if f.name == "name" {
                errs.push(where_("field 'name' is implicit and cannot be redeclared"));
            }
            if !names.insert(&f.name) {
                errs.push(where_(&format!("duplicate field '{}'", f.name)));
            }
            uses_v2 |= check_field(cat, &where_(&format!("field '{}'", f.name)), f, &mut errs);
            if let Some(r) = &f.required_unless_relation {
                if !def.relations.iter().any(|x| &x.kind == r) {
                    errs.push(where_(&format!(
                        "field '{}': required_unless_relation '{r}' is not declared",
                        f.name
                    )));
                }
            }
            if let Some(m) = &f.moved_from {
                uses_v2 = true;
                let still_there = match &m.provider {
                    Some(pid) => def
                        .providers
                        .get(pid)
                        .is_some_and(|pm| pm.fields.iter().any(|x| x.name == m.field)),
                    None => def.fields.iter().any(|x| x.name == m.field),
                };
                if still_there {
                    errs.push(where_(&format!(
                        "field '{}': moved_from names '{}', which is still declared",
                        f.name, m.field
                    )));
                }
                if let Some(pid) = &m.provider {
                    if !cat.providers.contains_key(pid) {
                        errs.push(where_(&format!(
                            "field '{}': moved_from provider '{pid}' is not a known provider",
                            f.name
                        )));
                    }
                }
            }
        }
        // Units picked by another field: an enum of this type, with a unit per option.
        for f in def
            .fields
            .iter()
            .chain(def.providers.values().flat_map(|m| m.fields.iter()))
        {
            let Some(u) = &f.units else { continue };
            uses_v2 = true;
            match def.fields.iter().find(|x| x.name == u.field) {
                Some(sel) if sel.field_type == FieldType::Enum => {
                    for k in u.values.keys() {
                        if !sel.options.contains(k) {
                            errs.push(where_(&format!(
                                "field '{}': units for '{k}', which is not an option of '{}'",
                                f.name, u.field
                            )));
                        }
                    }
                }
                _ => errs.push(where_(&format!(
                    "field '{}': units field '{}' is not an enum field of this type",
                    f.name, u.field
                ))),
            }
        }
        // relations
        for r in &def.relations {
            for pid in &r.providers {
                if !cat.providers.contains_key(pid) {
                    errs.push(where_(&format!(
                        "relation '{}': providers entry '{pid}' is not a known provider",
                        r.kind
                    )));
                }
            }
            if r.min_targets.is_some() {
                uses_v2 = true;
            }
            if r.manifests {
                uses_v2 = true;
                if !manifest_type {
                    errs.push(where_(&format!(
                        "relation '{}': manifests = true is only valid on a type the Kubernetes export reads ({})",
                        r.kind,
                        MANIFEST_TYPES.join(", ")
                    )));
                }
            }
            if Relation::from_key(&r.kind).is_none() {
                errs.push(where_(&format!("unknown relation kind '{}'", r.kind)));
            }
            for t in &r.targets {
                if !cat.resources.contains_key(t) {
                    errs.push(where_(&format!(
                        "relation '{}' targets unknown type '{t}'",
                        r.kind
                    )));
                }
            }
            if r.via_parent && !r.targets.iter().any(|t| cat.is_container(t)) {
                errs.push(where_(&format!(
                    "relation '{}' has via_parent but no container target",
                    r.kind
                )));
            }
        }
        // A kind may be declared several times (one per group of target types). Each
        // declaration then owns its own targets; overlapping sets would make cardinality
        // and `target_type` filters ambiguous.
        for (i, r) in def.relations.iter().enumerate() {
            for other in def.relations.iter().take(i).filter(|x| x.kind == r.kind) {
                if let Some(t) = r.targets.iter().find(|t| other.targets.contains(t)) {
                    errs.push(where_(&format!(
                        "relation '{}' declares target '{t}' twice; declarations sharing a kind need disjoint targets",
                        r.kind
                    )));
                }
            }
        }
        // providers
        for (pid, m) in &def.providers {
            let pw = |s: &str| where_(&format!("provider '{pid}': {s}"));
            let Some(pdef) = cat.providers.get(pid) else {
                errs.push(pw("unknown provider"));
                continue;
            };
            let mut pnames = HashSet::new();
            for f in &m.fields {
                if f.manifests {
                    errs.push(pw(&format!(
                        "provider field '{}': manifests = true is only valid on an abstract field",
                        f.name
                    )));
                }
                if !pnames.insert(&f.name) {
                    errs.push(pw(&format!("duplicate provider field '{}'", f.name)));
                }
                if f.moved_from.is_some() {
                    errs.push(pw(&format!(
                        "provider field '{}': moved_from is only valid on an abstract field",
                        f.name
                    )));
                }
                if f.name == "name" || def.fields.iter().any(|a| a.name == f.name) {
                    errs.push(pw(&format!(
                        "provider field '{}' shadows an abstract field",
                        f.name
                    )));
                }
                uses_v2 |= check_field(cat, &pw(&format!("field '{}'", f.name)), f, &mut errs);
                if let Some(r) = &f.required_unless_relation {
                    if !def.relations.iter().any(|x| &x.kind == r) {
                        errs.push(pw(&format!(
                            "field '{}': required_unless_relation '{r}' is not declared",
                            f.name
                        )));
                    }
                }
            }
            if m.status != MappingStatus::Logical && m.blocks.is_empty() {
                errs.push(pw(
                    "no blocks declared (use status = \"logical\" for a no-op mapping)",
                ));
            }
            if m.status == MappingStatus::Partial && m.manual_steps.is_empty() {
                errs.push(pw("status is 'partial' but no manual_steps declared"));
            }
            if !m.data.is_empty() {
                uses_v2 = true;
            }
            let block_keys: Vec<&str> = m.blocks.iter().map(|b| b.key.as_str()).collect();
            let data_keys: Vec<&str> = m.data.iter().map(|b| b.key.as_str()).collect();
            let repeated_data: Vec<&str> = m
                .data
                .iter()
                .filter(|b| b.for_each_field.is_some())
                .map(|b| b.key.as_str())
                .collect();
            let mut vars: Vec<&str> = pdef.variables.iter().map(|v| v.name.as_str()).collect();
            vars.extend(m.variables.iter().map(|v| v.name.as_str()));
            let aliases: Vec<&str> = pdef.aliases.iter().map(|a| a.name.as_str()).collect();
            let mut ctx = SourceCtx {
                what: pw(""),
                fields: &def.fields,
                provider_fields: &m.fields,
                relations: &def.relations,
                blocks: &block_keys,
                data: &data_keys,
                repeated_data: &repeated_data,
                vars: &vars,
                aliases: &aliases,
                item_fields: None,
                in_relation: false,
                cat,
                v2: false,
            };
            let mut seen_keys = HashSet::new();
            for b in &m.blocks {
                if !seen_keys.insert(&b.key) {
                    errs.push(pw(&format!("duplicate block key '{}'", b.key)));
                }
                check_block(&mut ctx, b, &mut errs);
            }
            let mut seen_data = HashSet::new();
            for b in &m.data {
                if !seen_data.insert(&b.key) {
                    errs.push(pw(&format!("duplicate data key '{}'", b.key)));
                }
                check_block(&mut ctx, b, &mut errs);
            }
            for step in &m.manual_steps {
                if let Some(c) = &step.when {
                    ctx.v2 = true;
                    check_condition(&mut ctx, c, &mut errs);
                }
            }
            for (i, chk) in m.checks.iter().enumerate() {
                ctx.v2 = true;
                let outer = ctx.item_fields.clone();
                if let Some(field) = &chk.for_each_field {
                    match for_each_items(&ctx, field) {
                        Ok(items) => ctx.item_fields = Some(items),
                        Err(e) => errs.push(pw(&format!("check #{i}: for_each_field: {e}"))),
                    }
                }
                if !["warning", "error", "omit"].contains(&chk.severity.as_str()) {
                    errs.push(pw(&format!(
                        "check #{i}: severity must be warning, error or omit"
                    )));
                }
                check_condition(&mut ctx, &chk.when, &mut errs);
                ctx.item_fields = outer;
            }
            // Connection values are resolved on the entity itself, exactly like a block
            // argument, so they are checked the same way.
            for (k, src) in &m.connection {
                ctx.v2 = true;
                if !is_env_name(k) {
                    errs.push(pw(&format!(
                        "connection key '{k}' must be upper case letters, digits and '_', starting with a letter"
                    )));
                }
                check_source(&mut ctx, &format!("connection '{k}'"), src, &mut errs);
            }
            for (k, o) in &m.outputs {
                if let Some(b) = &o.block {
                    if !block_keys.contains(&b.as_str()) {
                        errs.push(pw(&format!("output '{k}' references unknown block '{b}'")));
                    }
                }
                if let Some(c) = &o.when {
                    ctx.v2 = true;
                    ctx.what = pw(&format!("output '{k}' "));
                    check_condition(&mut ctx, c, &mut errs);
                    ctx.what = pw("");
                }
            }
            uses_v2 |= ctx.v2;
        }
        if uses_v2 && def.schema_version < 2 {
            errs.push(where_(
                "uses schema_version 2 features (struct_list, for_each_field, data, item, \
                 item_index, self_data, if, fallback, target_type, for_each_relation, target, \
                 provider_alias, setting, connection, manifests, env_prefix, where, linked) \
                 but declares schema_version = 1",
            ));
        }
    }
    errs
}

/// Returns true if the field uses v2 features.
fn check_field(cat: &Catalog, where_: &str, f: &FieldDef, errs: &mut Vec<String>) -> bool {
    let mut v2 = false;
    if f.field_type == FieldType::EntityRef {
        v2 = true;
        if f.targets.is_empty() {
            errs.push(format!("{where_}: entity_ref field declares no targets"));
        }
        for t in &f.targets {
            // A native type (`native:aws:data.aws_ec2_managed_prefix_list`) is created on
            // demand, so it is checked by shape: a known provider and a type name.
            let known = match crate::load::native_parts(t) {
                Some((provider, tf)) => cat.providers.contains_key(provider) && !tf.is_empty(),
                None => cat.resources.contains_key(t),
            };
            if !known {
                errs.push(format!("{where_}: entity_ref target '{t}' is not a known type"));
            }
        }
    } else if !f.targets.is_empty() {
        errs.push(format!("{where_}: only entity_ref fields may declare targets"));
    }
    if f.unique_scope.is_some() || f.required_unless_relation.is_some() {
        v2 = true;
    }
    if f.field_type == FieldType::StringList && !f.options.is_empty() {
        v2 = true;
    }
    if f.field_type == FieldType::Number {
        v2 = true;
    }
    if f.state_secret && f.field_type != FieldType::Bool {
        errs.push(format!(
            "{where_}: state_secret is only meaningful on a bool field (true = the value lands in state)"
        ));
    }
    if f.field_type == FieldType::Enum && f.options.is_empty() {
        errs.push(format!("{where_}: enum field has no options"));
    }
    if let Some(p) = &f.pattern {
        if let Err(e) = regex::Regex::new(p) {
            errs.push(format!("{where_}: invalid pattern: {e}"));
        }
    }
    if f.field_type == FieldType::StructList {
        v2 = true;
        if f.items.is_empty() {
            errs.push(format!("{where_}: struct_list field declares no items"));
        }
        let mut seen = HashSet::new();
        for sub in &f.items {
            if !seen.insert(&sub.name) {
                errs.push(format!("{where_}: duplicate item '{}'", sub.name));
            }
            if let Some(u) = &sub.required_unless_item {
                if !sub.required {
                    errs.push(format!(
                        "{where_}: item '{}': required_unless_item only applies to a required item",
                        sub.name
                    ));
                }
                if u.item == sub.name || !f.items.iter().any(|i| i.name == u.item) {
                    errs.push(format!(
                        "{where_}: item '{}': required_unless_item names '{}', which is not another item of the row",
                        sub.name, u.item
                    ));
                }
                if u.values.is_empty() {
                    errs.push(format!(
                        "{where_}: item '{}': required_unless_item needs at least one value in `in`",
                        sub.name
                    ));
                }
            }
            if sub.field_type == FieldType::StructList {
                errs.push(format!("{where_}: item '{}' — struct_list cannot nest", sub.name));
            }
            if sub.manifests {
                errs.push(format!(
                    "{where_}: item '{}' — manifests = true is only valid on a field",
                    sub.name
                ));
            }
            check_field(cat, &format!("{where_} item '{}'", sub.name), sub, errs);
        }
    } else if !f.items.is_empty() {
        errs.push(format!("{where_}: only struct_list fields may declare items"));
    }
    if f.required_unless_item.is_some() && f.field_type == FieldType::StructList {
        errs.push(format!(
            "{where_}: required_unless_item belongs on an item of a struct_list, not on the table"
        ));
    }
    if let Some(d) = &f.default {
        if let Err(e) = crate::fields::check_value_toml(f, d) {
            errs.push(format!("{where_}: default is invalid: {e}"));
        }
    }
    v2
}

struct SourceCtx<'a> {
    what: String,
    fields: &'a [FieldDef],
    provider_fields: &'a [FieldDef],
    relations: &'a [RelationDef],
    blocks: &'a [&'a str],
    data: &'a [&'a str],
    /// Data keys with `for_each_field`, the ones a `key_item` can pick an instance of.
    repeated_data: &'a [&'a str],
    vars: &'a [&'a str],
    /// Provider aliases a block may send itself to with `provider_alias`.
    aliases: &'a [&'a str],
    /// Sub-fields available to `item` sources, when inside a `for_each_field` block.
    item_fields: Option<Vec<String>>,
    /// Inside a `for_each_relation` block: `target` sources are valid.
    in_relation: bool,
    cat: &'a Catalog,
    v2: bool,
}

impl<'a> SourceCtx<'a> {
    fn field(&self, name: &str) -> Option<&'a FieldDef> {
        self.fields
            .iter()
            .chain(self.provider_fields.iter())
            .find(|f| f.name == name)
    }
}

/// HCL identifiers start with a letter or `_` and continue with letters, digits, `-` or
/// `_`. Alias names become one (`aws.us_east_1`), so they have to qualify.
fn is_hcl_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `QUEUE`, `SUBSCRIPTION_NAME`: the shape of an environment variable name part.
fn is_env_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn check_block(ctx: &mut SourceCtx, b: &BlockDef, errs: &mut Vec<String>) {
    let outer_items = ctx.item_fields.clone();
    let outer_rel = ctx.in_relation;
    if let Some(alias) = &b.provider_alias {
        ctx.v2 = true;
        if !ctx.aliases.contains(&alias.as_str()) {
            errs.push(format!(
                "{}block '{}': provider_alias '{alias}' is not declared by this provider",
                ctx.what, b.key
            ));
        }
    }
    if let Some(rel) = &b.for_each_relation {
        ctx.v2 = true;
        if b.for_each_field.is_some() {
            errs.push(format!(
                "{}block '{}': for_each_field and for_each_relation cannot be combined",
                ctx.what, b.key
            ));
        }
        let at = format!("block '{}' for_each_relation", b.key);
        check_relation_ref(ctx, &at, rel, b.for_each_target_type.as_deref(), errs);
        ctx.in_relation = true;
    }
    if let Some(field) = &b.for_each_field {
        ctx.v2 = true;
        match for_each_items(ctx, field) {
            Ok(items) => ctx.item_fields = Some(items),
            Err(e) => errs.push(format!("{}block '{}': for_each_field: {e}", ctx.what, b.key)),
        }
    }
    if let Some(key) = &b.for_each_key {
        match (&b.for_each_field, &ctx.item_fields) {
            (None, _) => errs.push(format!(
                "{}block '{}': for_each_key only applies to a for_each_field block",
                ctx.what, b.key
            )),
            (Some(_), Some(items)) if !items.iter().any(|i| i == key) => errs.push(format!(
                "{}block '{}': for_each_key '{key}' is not an item of the row (available: {})",
                ctx.what,
                b.key,
                items.join(", ")
            )),
            _ => {}
        }
    }
    if let Some(c) = &b.when {
        check_condition(ctx, c, errs);
    }
    for (k, src) in &b.args {
        check_source(ctx, &format!("block '{}' arg '{k}'", b.key), src, errs);
    }
    for n in &b.nested {
        check_nested(ctx, n, errs);
    }
    ctx.item_fields = outer_items;
    ctx.in_relation = outer_rel;
}

fn for_each_items(ctx: &SourceCtx, field: &str) -> Result<Vec<String>, String> {
    let f = ctx
        .field(field)
        .ok_or_else(|| format!("undeclared field '{field}'"))?;
    match f.field_type {
        FieldType::StructList => Ok(f.items.iter().map(|i| i.name.clone()).collect()),
        FieldType::StringList => Ok(vec!["value".into()]),
        _ => Err(format!("field '{field}' is not a struct_list or string_list")),
    }
}

fn check_condition(ctx: &mut SourceCtx, c: &Condition, errs: &mut Vec<String>) {
    match c {
        Condition::Relation(r) => {
            if r.absent
                || r.target_field.is_some()
                || r.target_provider_field.is_some()
                || r.min_count.is_some()
            {
                ctx.v2 = true;
            }
            if let Some(h) = &r.hop {
                check_hop(ctx, "when", r.target_type.as_deref(), h, errs);
            }
            if r.min_count == Some(0) {
                errs.push(format!(
                    "{}when: min_count must be at least 1 (use absent = true for \"none\")",
                    ctx.what
                ));
            }
            if let Some(w) = &r.where_ {
                ctx.v2 = true;
                check_where(ctx, r.target_type.as_deref(), w, errs);
            }
            if r.incoming {
                // The relation belongs to whoever links *here*, so it is not one of this
                // type's own declarations: only the kind and the other end's type exist
                // to check.
                ctx.v2 = true;
                if Relation::from_key(&r.relation).is_none() {
                    errs.push(format!(
                        "{}when incoming: unknown relation kind '{}'",
                        ctx.what, r.relation
                    ));
                }
                if let Some(tt) = &r.target_type {
                    if !ctx.cat.resources.contains_key(tt) {
                        errs.push(format!(
                            "{}when incoming: target_type '{tt}' is not a known type",
                            ctx.what
                        ));
                    }
                }
                return;
            }
            if r.target_field.is_some() && r.target_provider_field.is_some() {
                errs.push(format!(
                    "{}when: target_field and target_provider_field cannot be combined",
                    ctx.what
                ));
            }
            if (r.equals.is_some() || r.not_equals.is_some())
                && r.target_field.is_none()
                && r.target_provider_field.is_none()
            {
                errs.push(format!(
                    "{}when: equals / not_equals on a relation condition need target_field or target_provider_field",
                    ctx.what
                ));
            }
            check_relation_ref(ctx, "when", &r.relation, r.target_type.as_deref(), errs);
        }
        Condition::Target(t) => {
            ctx.v2 = true;
            match &t.relation {
                // `relation` names the subjects itself, so no enclosing repeated block is needed.
                Some(rel) => check_relation_ref(ctx, "when", rel, t.target_type.as_deref(), errs),
                None if t.target_type.is_some() => errs.push(format!(
                    "{}when target_shares_ancestor: target_type only applies together with relation",
                    ctx.what
                )),
                None if !ctx.in_relation => errs.push(format!(
                    "{}when target_shares_ancestor needs a relation, or a for_each_relation block",
                    ctx.what
                )),
                None => {}
            }
            if !ctx.cat.is_container(&t.target_shares_ancestor) {
                errs.push(format!(
                    "{}when target_shares_ancestor '{}' is not a container type",
                    ctx.what, t.target_shares_ancestor
                ));
            }
        }
        Condition::Field(f) => {
            if f.absent
                || f.starts_with.is_some()
                || f.not_starts_with.is_some()
                || f.ends_with.is_some()
                || f.not_ends_with.is_some()
                || f.transform.is_some()
            {
                ctx.v2 = true;
            }
            // "name" is the implicit display-name field: never declared in `fields`,
            // but always valid to test against.
            if f.field != "name" && !ctx.fields.iter().any(|x| x.name == f.field) {
                errs.push(format!(
                    "{}when references undeclared field '{}'",
                    ctx.what, f.field
                ));
            }
            let fields = ctx.fields;
            check_compare(
                ctx,
                fields.iter().find(|x| x.name == f.field),
                &f.field,
                f.compare(),
                errs,
            );
        }
        Condition::ProviderField(f) => {
            if f.absent || f.ends_with.is_some() || f.not_ends_with.is_some() {
                ctx.v2 = true;
            }
            if !ctx.provider_fields.iter().any(|x| x.name == f.provider_field) {
                errs.push(format!(
                    "{}when references undeclared provider field '{}'",
                    ctx.what, f.provider_field
                ));
            }
            let fields = ctx.provider_fields;
            check_compare(
                ctx,
                fields.iter().find(|x| x.name == f.provider_field),
                &f.provider_field,
                f.compare(),
                errs,
            );
        }
        Condition::Ancestor(a) => {
            ctx.v2 = true;
            if !ctx.cat.is_container(&a.ancestor) {
                errs.push(format!(
                    "{}when ancestor '{}' is not a container type",
                    ctx.what, a.ancestor
                ));
            }
        }
        Condition::Item(i) => {
            ctx.v2 = true;
            check_item_ref(ctx, "when", &i.item, errs);
            if let Some(other) = &i.equals_item {
                check_item_ref(ctx, "when equals_item", other, errs);
            }
            if let Some(ty) = &i.ref_type {
                check_ref_type(ctx, &i.item, ty, errs);
            }
            if let Some(rel) = &i.linked {
                if !entity_ref_item(ctx, &i.item) {
                    errs.push(format!(
                        "{}when linked: item '{}' is not an entity_ref",
                        ctx.what, i.item
                    ));
                }
                check_relation_ref(ctx, "when linked", rel, None, errs);
            }
            if i.min_count == Some(0) {
                errs.push(format!(
                    "{}when item '{}': min_count must be at least 1",
                    ctx.what, i.item
                ));
            }
        }
        Condition::Not(n) => {
            ctx.v2 = true;
            check_condition(ctx, &n.not, errs);
        }
        Condition::Setting(s) => {
            ctx.v2 = true;
            if !CONDITION_SETTINGS.contains(&s.setting.as_str()) {
                errs.push(format!(
                    "{}when: unknown setting '{}' (known: {})",
                    ctx.what,
                    s.setting,
                    CONDITION_SETTINGS.join(", ")
                ));
            }
        }
        Condition::All(a) => {
            ctx.v2 = true;
            for c in &a.all {
                check_condition(ctx, c, errs);
            }
        }
        Condition::Any(a) => {
            ctx.v2 = true;
            for c in &a.any {
                check_condition(ctx, c, errs);
            }
        }
    }
}

/// Is `item` an `entity_ref` sub-field of some table field of this type?
fn entity_ref_item(ctx: &SourceCtx, item: &str) -> bool {
    ctx.fields
        .iter()
        .chain(ctx.provider_fields.iter())
        .flat_map(|f| f.items.iter())
        .any(|sub| sub.name == item && sub.field_type == FieldType::EntityRef)
}

/// A relation condition's `where` is evaluated on the entity at the other end, so it is
/// checked against *that* type's fields and relations, which `target_type` names.
fn check_where(ctx: &mut SourceCtx, target_type: Option<&str>, w: &Condition, errs: &mut Vec<String>) {
    let Some(tt) = target_type else {
        errs.push(format!(
            "{}when where: needs target_type, which says whose fields and links it reads",
            ctx.what
        ));
        return;
    };
    let Some(other) = ctx.cat.resources.get(tt) else {
        return; // reported as an unknown target_type
    };
    let mut sub = SourceCtx {
        what: format!("{}when where ({tt}): ", ctx.what),
        fields: &other.fields,
        provider_fields: &[],
        relations: &other.relations,
        blocks: &[],
        data: &[],
        repeated_data: &[],
        vars: &[],
        aliases: &[],
        item_fields: None,
        in_relation: false,
        cat: ctx.cat,
        v2: true,
    };
    check_condition(&mut sub, w, errs);
}

fn is_numeric(f: &FieldDef) -> bool {
    matches!(f.field_type, FieldType::Int | FieldType::Number)
}

/// `one_of` / `not_one_of` and the numeric comparisons of a field condition: a number
/// compares only against a numeric field, and a bound that names a field must name a
/// numeric one this mapping can read.
fn check_compare(
    ctx: &mut SourceCtx,
    field: Option<&FieldDef>,
    name: &str,
    cmp: Compare<'_>,
    errs: &mut Vec<String>,
) {
    if cmp.is_empty() {
        return;
    }
    ctx.v2 = true;
    let bounds = cmp.bounds();
    if bounds.is_empty() {
        return;
    }
    if field.is_some_and(|f| !is_numeric(f)) {
        errs.push(format!(
            "{}when: numeric comparisons need an int or number field, and '{name}' is neither",
            ctx.what
        ));
    }
    for (op, b) in bounds {
        let (other, provider) = match b {
            Bound::Number(_) => continue,
            Bound::Field(f) => (&f.field, false),
            Bound::ProviderField(f) => (&f.provider_field, true),
        };
        let pool = if provider { ctx.provider_fields } else { ctx.fields };
        match pool.iter().find(|x| &x.name == other) {
            None => errs.push(format!(
                "{}when {op}: undeclared {}field '{other}'",
                ctx.what,
                if provider { "provider " } else { "" }
            )),
            Some(x) if !is_numeric(x) => errs.push(format!(
                "{}when {op}: '{other}' is not an int or number field",
                ctx.what
            )),
            Some(_) => {}
        }
    }
}

/// `ref_type` asks what an `entity_ref` item points at, so the item must be one and the
/// type must be among the ones it may point at.
fn check_ref_type(ctx: &SourceCtx, item: &str, ty: &str, errs: &mut Vec<String>) {
    let refs: Vec<&FieldDef> = ctx
        .fields
        .iter()
        .chain(ctx.provider_fields.iter())
        .flat_map(|f| f.items.iter())
        .filter(|sub| sub.name == item && sub.field_type == FieldType::EntityRef)
        .collect();
    if refs.is_empty() {
        errs.push(format!(
            "{}when ref_type: item '{item}' is not an entity_ref",
            ctx.what
        ));
    } else if !refs.iter().any(|sub| sub.targets.iter().any(|t| t == ty)) {
        errs.push(format!(
            "{}when ref_type: '{ty}' is not a target of the entity_ref item '{item}'",
            ctx.what
        ));
    }
}

fn check_item_ref(ctx: &SourceCtx, at: &str, item: &str, errs: &mut Vec<String>) {
    match &ctx.item_fields {
        None => errs.push(format!(
            "{}{at}: `item` is only valid inside a for_each_field block",
            ctx.what
        )),
        Some(items) if !items.iter().any(|x| x == item) => errs.push(format!(
            "{}{at}: row has no item '{item}' (available: {})",
            ctx.what,
            items.join(", ")
        )),
        _ => {}
    }
}

fn check_relation_ref(
    ctx: &mut SourceCtx,
    at: &str,
    relation: &str,
    target_type: Option<&str>,
    errs: &mut Vec<String>,
) {
    // A type may declare the same relation kind more than once (one per group of target
    // types), so every declaration of the kind is a candidate.
    let decls: Vec<&RelationDef> = ctx.relations.iter().filter(|x| x.kind == relation).collect();
    if decls.is_empty() {
        errs.push(format!("{}{at}: undeclared relation '{relation}'", ctx.what));
        return;
    }
    if let Some(tt) = target_type {
        ctx.v2 = true;
        if !decls.iter().any(|r| r.targets.iter().any(|t| t == tt)) {
            errs.push(format!(
                "{}{at}: target_type '{tt}' is not a declared target of relation '{relation}'",
                ctx.what
            ));
        }
    }
}

/// A second step from the entities a relation reached: the kind must exist, the type must
/// be known, an outgoing hop must be declared by the first-hop type when the mapping says
/// which type that is, and `certificate_covers` needs a type that has a host name.
fn check_hop(ctx: &mut SourceCtx, at: &str, first: Option<&str>, h: &Hop, errs: &mut Vec<String>) {
    ctx.v2 = true;
    let what = ctx.what.clone();
    let e = |msg: String| format!("{what}{at} hop: {msg}");
    if Relation::from_key(&h.relation).is_none() {
        errs.push(e(format!("unknown relation kind '{}'", h.relation)));
    }
    if let Some(tt) = &h.target_type {
        if !ctx.cat.resources.contains_key(tt) {
            errs.push(e(format!("target_type '{tt}' is not a known type")));
        }
    }
    if !h.incoming {
        if let Some(def) = first.and_then(|t| ctx.cat.resources.get(t)) {
            let declared = def.relations.iter().any(|r| {
                r.kind == h.relation
                    && h.target_type
                        .as_ref()
                        .is_none_or(|tt| r.targets.iter().any(|t| t == tt))
            });
            if !declared {
                errs.push(e(format!(
                    "'{}' declares no relation '{}'{}",
                    def.resource.type_id,
                    h.relation,
                    h.target_type
                        .as_ref()
                        .map(|t| format!(" to '{t}'"))
                        .unwrap_or_default()
                )));
            }
        }
    }
    if h.certificate_covers
        && !h
            .target_type
            .as_deref()
            .is_some_and(|t| HOST_NAME_TYPES.contains(&t))
    {
        errs.push(e(format!(
            "certificate_covers needs a target_type with a host name ({})",
            HOST_NAME_TYPES.join(", ")
        )));
    }
}

fn check_nested(ctx: &mut SourceCtx, n: &NestedBlockDef, errs: &mut Vec<String>) {
    let outer_items = ctx.item_fields.clone();
    let outer_rel = ctx.in_relation;
    if let Some(item) = &n.for_each_item {
        ctx.v2 = true;
        if n.for_each_field.is_some() || n.for_each_relation.is_some() {
            errs.push(format!(
                "{}nested '{}': for_each_item cannot be combined with for_each_field or for_each_relation",
                ctx.what, n.block
            ));
        }
        check_item_ref(ctx, &format!("nested '{}' for_each_item", n.block), item, errs);
        // The entry is read as `value`, so a row that has an item of that name would lose it.
        if let Some(items) = ctx.item_fields.as_mut() {
            if items.iter().any(|x| x == "value") {
                errs.push(format!(
                    "{}nested '{}': for_each_item needs a row without an item named 'value'",
                    ctx.what, n.block
                ));
            } else {
                items.push("value".into());
            }
        }
    }
    if let Some(rel) = &n.for_each_relation {
        ctx.v2 = true;
        if n.for_each_field.is_some() {
            errs.push(format!(
                "{}nested '{}': for_each_field and for_each_relation cannot be combined",
                ctx.what, n.block
            ));
        }
        let at = format!("nested '{}' for_each_relation", n.block);
        check_relation_ref(ctx, &at, rel, n.for_each_target_type.as_deref(), errs);
        ctx.in_relation = true;
    }
    if let Some(field) = &n.for_each_field {
        ctx.v2 = true;
        match for_each_items(ctx, field) {
            Ok(items) => ctx.item_fields = Some(items),
            Err(e) => errs.push(format!("{}nested '{}': for_each_field: {e}", ctx.what, n.block)),
        }
    }
    if let Some(c) = &n.when {
        check_condition(ctx, c, errs);
    }
    for (k, src) in &n.args {
        check_source(ctx, &format!("nested '{}' arg '{k}'", n.block), src, errs);
    }
    for inner in &n.nested {
        check_nested(ctx, inner, errs);
    }
    ctx.item_fields = outer_items;
    ctx.in_relation = outer_rel;
}

/// `column` and `wrap = "map"` both read a list field in a particular shape, so the field
/// must be declared with the matching type for the shape to mean anything.
fn check_shape(
    ctx: &mut SourceCtx,
    at: &str,
    declared: Option<&FieldDef>,
    column: Option<&str>,
    wrap: Option<Wrap>,
    transform: Option<Transform>,
    errs: &mut Vec<String>,
) {
    let what = ctx.what.clone();
    let e = |msg: String| format!("{what}{at}: {msg}");
    if let Some(col) = column {
        ctx.v2 = true;
        if wrap.is_some() || transform.is_some() {
            errs.push(e("column cannot be combined with wrap or transform".into()));
        }
        match declared {
            Some(f) if f.field_type == FieldType::StructList => {
                if !f.items.iter().any(|i| i.name == col) {
                    errs.push(e(format!("column '{col}' is not an item of '{}'", f.name)));
                }
            }
            Some(f) => errs.push(e(format!(
                "column needs a struct_list field; '{}' is {:?}",
                f.name, f.field_type
            ))),
            None => {}
        }
    }
    if wrap == Some(Wrap::Map) {
        ctx.v2 = true;
        if let Some(f) = declared {
            if f.field_type != FieldType::StringList {
                errs.push(e(format!(
                    "wrap = \"map\" needs a string_list of key=value entries; '{}' is {:?}",
                    f.name, f.field_type
                )));
            }
        }
    }
}

/// `wrap = "map"` reads a `key=value` string list, which only a field can be.
fn check_no_map_wrap(ctx: &SourceCtx, at: &str, wrap: Option<Wrap>, errs: &mut Vec<String>) {
    if wrap == Some(Wrap::Map) {
        errs.push(format!(
            "{}{at}: wrap = \"map\" is only valid on a field or provider_field",
            ctx.what
        ));
    }
}

fn check_source(ctx: &mut SourceCtx, at: &str, src: &ArgSource, errs: &mut Vec<String>) {
    let what = ctx.what.clone();
    let e = |msg: String| format!("{what}{at}: {msg}");
    match src {
        ArgSource::Literal(_) => {}
        ArgSource::Raw(r) => {
            if r.refs.is_empty() {
                return;
            }
            ctx.v2 = true;
            if r.raw.matches('@').count() % 2 != 0 {
                errs.push(e("raw has an unterminated @placeholder@".into()));
            }
            let used = r.placeholders();
            for name in &used {
                if !r.refs.contains_key(*name) {
                    errs.push(e(format!("raw placeholder '@{name}@' has no entry in refs")));
                }
            }
            for (name, sub) in &r.refs {
                if !used.contains(&name.as_str()) {
                    errs.push(e(format!("ref '{name}' never appears as '@{name}@' in raw")));
                }
                check_source(ctx, &format!("{at}.refs.{name}"), sub, errs);
            }
        }
        ArgSource::Field(f) => {
            let declared = ctx.fields.iter().find(|x| x.name == f.field);
            if f.field != "name" && declared.is_none() {
                errs.push(e(format!("undeclared field '{}'", f.field)));
            }
            check_shape(ctx, at, declared, f.column.as_deref(), f.wrap, f.transform, errs);
            if let Some(fb) = &f.fallback {
                ctx.v2 = true;
                check_source(ctx, &format!("{at}.fallback"), fb, errs);
            }
        }
        ArgSource::ProviderField(f) => {
            let declared = ctx.provider_fields.iter().find(|x| x.name == f.provider_field);
            if declared.is_none() {
                errs.push(e(format!("undeclared provider field '{}'", f.provider_field)));
            }
            check_shape(ctx, at, declared, f.column.as_deref(), f.wrap, f.transform, errs);
            if let Some(fb) = &f.fallback {
                ctx.v2 = true;
                check_source(ctx, &format!("{at}.fallback"), fb, errs);
            }
            if let Some(region) = &f.zone_of {
                if !ctx.vars.contains(&region.as_str()) {
                    errs.push(e(format!(
                        "zone_of names '{region}', which is not a variable of this provider"
                    )));
                }
                if f.wrap.is_some() || f.transform.is_some() || f.column.is_some() {
                    errs.push(e(
                        "zone_of cannot be combined with wrap, transform or column".into()
                    ));
                }
            }
        }
        ArgSource::Var(v) => {
            if !ctx.vars.contains(&v.var.as_str()) {
                errs.push(e(format!("undeclared variable '{}'", v.var)));
            }
        }
        ArgSource::Template(t) => {
            for ph in crate::fields::template_placeholders(&t.template) {
                if let Some(item) = ph.strip_prefix("item.") {
                    ctx.v2 = true;
                    // `{item.index}` is the row position, not one of the row's fields.
                    if item == "index" {
                        if ctx.item_fields.is_none() {
                            errs.push(e(
                                "template placeholder '{item.index}' is only valid inside a for_each_field block"
                                    .into(),
                            ));
                        }
                    } else {
                        check_item_ref(ctx, at, item, errs);
                    }
                    continue;
                }
                if ph.starts_with("target.") {
                    ctx.v2 = true;
                    if !ctx.in_relation {
                        errs.push(e(format!(
                            "template placeholder '{{{ph}}}' is only valid inside a for_each_relation block"
                        )));
                    }
                    continue;
                }
                let ok = ph == "name"
                    || ctx.fields.iter().any(|x| x.name == ph)
                    || ph
                        .strip_prefix("provider.")
                        .is_some_and(|p| ctx.provider_fields.iter().any(|x| x.name == p))
                    || ph.starts_with("settings.");
                if !ok {
                    errs.push(e(format!(
                        "template placeholder '{{{ph}}}' is not a declared field"
                    )));
                }
            }
        }
        ArgSource::Map(m) => {
            let is_field = ctx.field(&m.map).is_some();
            let is_item = ctx
                .item_fields
                .as_ref()
                .is_some_and(|items| items.iter().any(|x| x == &m.map));
            if !is_field && !is_item {
                errs.push(e(format!("map references undeclared field or item '{}'", m.map)));
            }
            if m.table.is_empty() {
                errs.push(e("map table is empty".into()));
            }
        }
        ArgSource::Relation(r) => {
            if r.incoming {
                // The edge belongs to whoever drew it, so it is not one of this type's
                // own declarations: only the kind and the other end's type can be checked.
                ctx.v2 = true;
                if Relation::from_key(&r.relation).is_none() {
                    errs.push(e(format!("incoming: unknown relation kind '{}'", r.relation)));
                }
                if let Some(tt) = &r.target_type {
                    if !ctx.cat.resources.contains_key(tt) {
                        errs.push(e(format!("incoming: target_type '{tt}' is not a known type")));
                    }
                }
            } else {
                check_relation_ref(ctx, at, &r.relation, r.target_type.as_deref(), errs);
            }
            if let Some(h) = &r.hop {
                check_hop(ctx, at, r.target_type.as_deref(), h, errs);
            }
            if let Some(f) = &r.field {
                ctx.v2 = true;
                if !r.attr.is_empty() {
                    errs.push(e("field and attr cannot be combined on a relation source".into()));
                }
                if r.block.is_some() {
                    errs.push(e(
                        "block addresses a resource, so it cannot be combined with field".into(),
                    ));
                }
                if f.trim().is_empty() {
                    errs.push(e("field name is empty".into()));
                }
                // The field belongs to the entity at the other end (of the hop, when there
                // is one), so it can only be checked when the mapping says which type that is.
                let subject = match &r.hop {
                    Some(h) => &h.target_type,
                    None => &r.target_type,
                };
                if let (Some(tt), Some(other)) =
                    (subject, subject.as_ref().and_then(|t| ctx.cat.resources.get(t)))
                {
                    if f != "name" && !other.fields.iter().any(|x| &x.name == f) {
                        errs.push(e(format!("'{tt}' has no field '{f}'")));
                    }
                }
            } else if r.transform.is_some() {
                errs.push(e(
                    "transform on a relation source only applies together with field".into(),
                ));
            }
            if let Some(key) = &r.connection {
                ctx.v2 = true;
                if !r.attr.is_empty() || r.field.is_some() || r.block.is_some() {
                    errs.push(e(
                        "connection reads the target's own connection value, so it cannot be combined with attr, field or block"
                            .into(),
                    ));
                }
                // The value belongs to the entity at the other end: some provider mapping of
                // a type it can be must declare the key.
                let candidates: Vec<&str> = match &r.target_type {
                    Some(tt) => vec![tt.as_str()],
                    None if r.incoming => Vec::new(),
                    None => ctx
                        .relations
                        .iter()
                        .filter(|x| x.kind == r.relation)
                        .flat_map(|x| x.targets.iter().map(String::as_str))
                        .collect(),
                };
                let declared = candidates.iter().any(|t| {
                    ctx.cat
                        .resources
                        .get(*t)
                        .is_some_and(|d| d.providers.values().any(|m| m.connection.contains_key(key)))
                });
                if !candidates.is_empty() && !declared {
                    errs.push(e(format!(
                        "connection '{key}' is not declared by any mapping of {}",
                        candidates.join(" / ")
                    )));
                }
            }
            check_no_map_wrap(ctx, at, r.wrap, errs);
            if let Some(anc) = &r.ancestor {
                ctx.v2 = true;
                if !ctx.cat.is_container(anc) {
                    errs.push(e(format!("ancestor '{anc}' is not a container type")));
                }
            }
            if let Some(fb) = &r.fallback {
                ctx.v2 = true;
                check_source(ctx, &format!("{at}.fallback"), fb, errs);
            }
        }
        ArgSource::Ancestor(a) => {
            if !ctx.cat.is_container(&a.ancestor) {
                errs.push(e(format!("ancestor '{}' is not a container type", a.ancestor)));
            }
        }
        ArgSource::SelfBlock(s) => {
            check_no_map_wrap(ctx, at, s.wrap, errs);
            if !ctx.blocks.contains(&s.self_block.as_str()) {
                errs.push(e(format!("self_block '{}' does not exist", s.self_block)));
            }
        }
        ArgSource::SelfData(s) => {
            ctx.v2 = true;
            check_no_map_wrap(ctx, at, s.wrap, errs);
            if !ctx.data.contains(&s.self_data.as_str()) {
                errs.push(e(format!("self_data '{}' does not exist", s.self_data)));
            }
            if let Some(item) = &s.key_item {
                check_item_ref(ctx, at, item, errs);
                if !ctx.repeated_data.contains(&s.self_data.as_str()) {
                    errs.push(e(format!(
                        "self_data '{}': key_item needs a data block with for_each_field",
                        s.self_data
                    )));
                }
            }
        }
        ArgSource::Item(i) => {
            ctx.v2 = true;
            check_no_map_wrap(ctx, at, i.wrap, errs);
            check_item_ref(ctx, at, &i.item, errs);
        }
        ArgSource::ItemRef(i) => {
            ctx.v2 = true;
            check_no_map_wrap(ctx, at, i.wrap, errs);
            check_item_ref(ctx, at, &i.item_ref, errs);
        }
        ArgSource::EntityVar(v) => {
            ctx.v2 = true;
            if v.entity_var.trim().is_empty() {
                errs.push(e("entity_var name is empty".into()));
            }
        }
        ArgSource::Target(t) => {
            ctx.v2 = true;
            check_no_map_wrap(ctx, at, t.wrap, errs);
            if !ctx.in_relation {
                errs.push(e("`target` is only valid inside a for_each_relation block".into()));
            }
        }
        ArgSource::ItemIndex(_) => {
            ctx.v2 = true;
            if ctx.item_fields.is_none() {
                errs.push(e("item_index is only valid inside a for_each_field block".into()));
            }
        }
        ArgSource::If(i) => {
            ctx.v2 = true;
            check_condition(ctx, &i.cond, errs);
            check_source(ctx, &format!("{at}.then"), &i.then, errs);
            if let Some(o) = &i.otherwise {
                check_source(ctx, &format!("{at}.else"), o, errs);
            }
        }
        ArgSource::Rows(r) => {
            ctx.v2 = true;
            // The rows become the `item` of `each` and `when`, exactly as in a repeated
            // block; an enclosing block's row is out of reach inside.
            let outer = ctx.item_fields.clone();
            match for_each_items(ctx, &r.for_each_field) {
                Ok(items) => ctx.item_fields = Some(items),
                Err(msg) => errs.push(e(format!("for_each_field: {msg}"))),
            }
            if let Some(c) = &r.when {
                check_condition(ctx, c, errs);
            }
            check_source(ctx, &format!("{at}.each"), &r.each, errs);
            ctx.item_fields = outer;
        }
        ArgSource::Object(o) => {
            for (k, s) in &o.object {
                check_source(ctx, &format!("{at}.{k}"), s, errs);
            }
        }
        ArgSource::List(l) => {
            for (i, s) in l.list.iter().enumerate() {
                check_source(ctx, &format!("{at}[{i}]"), s, errs);
            }
        }
        ArgSource::Func(f) => {
            if f.func.trim().is_empty() {
                errs.push(e("func name is empty".into()));
            }
            for (i, s) in f.args.iter().enumerate() {
                check_source(ctx, &format!("{at}.{}({i})", f.func), s, errs);
            }
        }
    }
}
