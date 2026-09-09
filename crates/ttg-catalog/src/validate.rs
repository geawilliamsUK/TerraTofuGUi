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
        let vars: Vec<&str> = p.variables.iter().map(|v| v.name.as_str()).collect();
        let mut ctx = SourceCtx {
            what: format!("provider '{pid}' provider_block"),
            fields: &[],
            provider_fields: &[],
            relations: &[],
            blocks: &[],
            data: &[],
            vars: &vars,
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
        // fields
        let mut names = HashSet::new();
        for f in &def.fields {
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
        }
        // relations
        for r in &def.relations {
            if r.min_targets.is_some() {
                uses_v2 = true;
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
        // providers
        for (pid, m) in &def.providers {
            let pw = |s: &str| where_(&format!("provider '{pid}': {s}"));
            let Some(pdef) = cat.providers.get(pid) else {
                errs.push(pw("unknown provider"));
                continue;
            };
            let mut pnames = HashSet::new();
            for f in &m.fields {
                if !pnames.insert(&f.name) {
                    errs.push(pw(&format!("duplicate provider field '{}'", f.name)));
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
            let mut vars: Vec<&str> = pdef.variables.iter().map(|v| v.name.as_str()).collect();
            vars.extend(m.variables.iter().map(|v| v.name.as_str()));
            let mut ctx = SourceCtx {
                what: pw(""),
                fields: &def.fields,
                provider_fields: &m.fields,
                relations: &def.relations,
                blocks: &block_keys,
                data: &data_keys,
                vars: &vars,
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
                if chk.severity != "warning" && chk.severity != "error" {
                    errs.push(pw(&format!("check #{i}: severity must be warning or error")));
                }
                check_condition(&mut ctx, &chk.when, &mut errs);
                ctx.item_fields = outer;
            }
            for (k, o) in &m.outputs {
                if let Some(b) = &o.block {
                    if !block_keys.contains(&b.as_str()) {
                        errs.push(pw(&format!("output '{k}' references unknown block '{b}'")));
                    }
                }
            }
            uses_v2 |= ctx.v2;
        }
        if uses_v2 && def.schema_version < 2 {
            errs.push(where_(
                "uses schema_version 2 features (struct_list, for_each_field, data, item, \
                 item_index, self_data, if, fallback, target_type, for_each_relation, target) but declares schema_version = 1",
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
            if !cat.resources.contains_key(t) {
                errs.push(format!("{where_}: entity_ref target '{t}' is not a known type"));
            }
        }
    } else if !f.targets.is_empty() {
        errs.push(format!("{where_}: only entity_ref fields may declare targets"));
    }
    if f.unique_scope.is_some() || f.required_unless_relation.is_some() {
        v2 = true;
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
            if sub.field_type == FieldType::StructList {
                errs.push(format!("{where_}: item '{}' — struct_list cannot nest", sub.name));
            }
            check_field(cat, &format!("{where_} item '{}'", sub.name), sub, errs);
        }
    } else if !f.items.is_empty() {
        errs.push(format!("{where_}: only struct_list fields may declare items"));
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
    vars: &'a [&'a str],
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

fn check_block(ctx: &mut SourceCtx, b: &BlockDef, errs: &mut Vec<String>) {
    let outer_items = ctx.item_fields.clone();
    let outer_rel = ctx.in_relation;
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
            if r.absent {
                ctx.v2 = true;
            }
            check_relation_ref(ctx, "when", &r.relation, r.target_type.as_deref(), errs);
        }
        Condition::Field(f) => {
            if f.absent {
                ctx.v2 = true;
            }
            if !ctx.fields.iter().any(|x| x.name == f.field) {
                errs.push(format!(
                    "{}when references undeclared field '{}'",
                    ctx.what, f.field
                ));
            }
        }
        Condition::ProviderField(f) => {
            if f.absent {
                ctx.v2 = true;
            }
            if !ctx.provider_fields.iter().any(|x| x.name == f.provider_field) {
                errs.push(format!(
                    "{}when references undeclared provider field '{}'",
                    ctx.what, f.provider_field
                ));
            }
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
    let Some(r) = ctx.relations.iter().find(|x| x.kind == relation) else {
        errs.push(format!("{}{at}: undeclared relation '{relation}'", ctx.what));
        return;
    };
    if let Some(tt) = target_type {
        ctx.v2 = true;
        if !r.targets.iter().any(|t| t == tt) {
            errs.push(format!(
                "{}{at}: target_type '{tt}' is not a declared target of relation '{relation}'",
                ctx.what
            ));
        }
    }
}

fn check_nested(ctx: &mut SourceCtx, n: &NestedBlockDef, errs: &mut Vec<String>) {
    let outer_items = ctx.item_fields.clone();
    let outer_rel = ctx.in_relation;
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

fn check_source(ctx: &mut SourceCtx, at: &str, src: &ArgSource, errs: &mut Vec<String>) {
    let what = ctx.what.clone();
    let e = |msg: String| format!("{what}{at}: {msg}");
    match src {
        ArgSource::Literal(_) | ArgSource::Raw(_) => {}
        ArgSource::Field(f) => {
            if f.field != "name" && !ctx.fields.iter().any(|x| x.name == f.field) {
                errs.push(e(format!("undeclared field '{}'", f.field)));
            }
            if let Some(fb) = &f.fallback {
                ctx.v2 = true;
                check_source(ctx, &format!("{at}.fallback"), fb, errs);
            }
        }
        ArgSource::ProviderField(f) => {
            if !ctx.provider_fields.iter().any(|x| x.name == f.provider_field) {
                errs.push(e(format!("undeclared provider field '{}'", f.provider_field)));
            }
            if let Some(fb) = &f.fallback {
                ctx.v2 = true;
                check_source(ctx, &format!("{at}.fallback"), fb, errs);
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
                    check_item_ref(ctx, at, item, errs);
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
            check_relation_ref(ctx, at, &r.relation, r.target_type.as_deref(), errs);
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
            if !ctx.blocks.contains(&s.self_block.as_str()) {
                errs.push(e(format!("self_block '{}' does not exist", s.self_block)));
            }
        }
        ArgSource::SelfData(s) => {
            ctx.v2 = true;
            if !ctx.data.contains(&s.self_data.as_str()) {
                errs.push(e(format!("self_data '{}' does not exist", s.self_data)));
            }
        }
        ArgSource::Item(i) => {
            ctx.v2 = true;
            check_item_ref(ctx, at, &i.item, errs);
        }
        ArgSource::ItemRef(i) => {
            ctx.v2 = true;
            check_item_ref(ctx, at, &i.item_ref, errs);
        }
        ArgSource::EntityVar(v) => {
            ctx.v2 = true;
            if v.entity_var.trim().is_empty() {
                errs.push(e("entity_var name is empty".into()));
            }
        }
        ArgSource::Target(_) => {
            ctx.v2 = true;
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
