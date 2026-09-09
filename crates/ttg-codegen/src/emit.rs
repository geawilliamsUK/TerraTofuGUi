//! The emitter: resolves every mapping's declarative argument sources into `hcl`
//! expressions and assembles the per-provider file set.
//!
//! Schema_version 2 additions handled here: repeated blocks (`for_each_field`,
//! `for_each_relation`), data sources (`data` / `self_data`), row-scoped sources
//! (`item`, `item_index`, `target`), conditional sources (`if`), field fallbacks and
//! relation `target_type` filters.

use crate::diagnostics::{self, consumed_relations, relation_targets, Consumed, Diagnostic, Severity};
use crate::files;
use crate::tool::Profile;
use crate::GenError;
use hcl::{Block, Expression, FuncCall, Identifier, Number, Object, ObjectKey, Traversal, Variable};
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};
use ttg_catalog::{
    ArgSource, BlockDef, Catalog, Condition, MappingStatus, NestedBlockDef, ProviderDef, ProviderMapping,
    Wrap,
};
use ttg_core::{EntityRef, Id, Project, Record, Relation, Tool, Value};

/// A manual step for the operator, rendered into MANUAL_STEPS.md.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualEntry {
    pub entity: Option<Id>,
    pub title: String,
    pub body: String,
}

/// An input variable to emit.
#[derive(Debug, Clone)]
pub struct VarSpec {
    pub name: String,
    pub var_type: String,
    pub description: String,
    pub default: Option<Expression>,
    pub sensitive: bool,
}

#[derive(Debug, Clone)]
pub struct OutputSpec {
    pub name: String,
    pub value: Expression,
    pub description: String,
    pub sensitive: bool,
}

/// Everything produced for one provider, in memory.
#[derive(Debug, Clone)]
pub struct Generated {
    pub provider: String,
    pub provider_display: String,
    pub tool: Tool,
    /// File name -> content. Includes `.tf` files, `README.md` and (when non-empty)
    /// `MANUAL_STEPS.md`.
    pub files: IndexMap<String, String>,
    pub manual_steps: Vec<ManualEntry>,
    pub diagnostics: Vec<Diagnostic>,
}

/// One concrete block (resource or data) produced for an entity.
struct Emitted {
    entity: Id,
    /// `resource` or `data`.
    kind: &'static str,
    resource_type: String,
    local: String,
    file: String,
    block: Block,
}

enum Status<'a> {
    Emit(&'a ProviderMapping),
    Manual,
    Unmapped,
    Logical,
}

/// One iteration of a repeated block: a struct_list row, a string_list entry, or a
/// relation target. A plain block has a single row with neither.
#[derive(Clone, Default)]
struct Row {
    record: Option<Record>,
    target: Option<Id>,
}

/// The current row while emitting a repeated block.
#[derive(Clone, Copy)]
struct ItemCtx<'a> {
    record: Option<&'a Record>,
    target: Option<&'a str>,
    index: usize,
}

impl Row {
    fn ctx(&self, index: usize) -> Option<ItemCtx<'_>> {
        if self.record.is_none() && self.target.is_none() {
            None
        } else {
            Some(ItemCtx {
                record: self.record.as_ref(),
                target: self.target.as_deref(),
                index,
            })
        }
    }
}

fn repeated(b: &BlockDef) -> bool {
    b.for_each_field.is_some() || b.for_each_relation.is_some()
}

struct Emitter<'a> {
    p: &'a Project,
    cat: &'a Catalog,
    provider: &'a str,
    pdef: &'a ProviderDef,
    profile: Profile,
    /// `(entity, block key)` pairs that will produce at least one block.
    planned: HashSet<(Id, String)>,
    /// `(entity, "data:" + key)` pairs for data sources.
    planned_data: HashSet<(Id, String)>,
    /// Local names of the instances of a block, per `(entity, key)`.
    instances: HashMap<(Id, String), Vec<String>>,
    vars: IndexMap<String, VarSpec>,
    used_vars: HashSet<String>,
    manual: Vec<ManualEntry>,
    manual_refs_done: HashSet<Id>,
}

/// Generate the complete file set for one provider. Errors if diagnostics contain any
/// `Error`, or if a mapping cannot be resolved.
pub fn generate(p: &Project, cat: &Catalog, provider: &str, tool: Tool) -> Result<Generated, GenError> {
    let pdef = cat
        .provider(provider)
        .ok_or_else(|| GenError::UnknownProvider(provider.to_string()))?;
    let diags = diagnostics::run(p, cat, provider);
    if diags.iter().any(|d| d.severity == Severity::Error) {
        return Err(GenError::Blocked(diags));
    }
    // Only the provider's layer is generated.
    let layer = crate::layers::project_for(p, cat, provider);
    let p = &layer;

    let mut em = Emitter {
        p,
        cat,
        provider,
        pdef,
        profile: Profile::new(tool),
        planned: HashSet::new(),
        planned_data: HashSet::new(),
        instances: HashMap::new(),
        vars: IndexMap::new(),
        used_vars: HashSet::new(),
        manual: Vec::new(),
        manual_refs_done: HashSet::new(),
    };

    // Provider-level variables are always present.
    for v in &pdef.variables {
        let setting = p
            .settings
            .provider_settings
            .get(provider)
            .and_then(|m| m.get(&v.name))
            .filter(|s| !s.trim().is_empty());
        let default = match setting {
            Some(s) => Some(Expression::String(s.clone())),
            None => v.default.as_ref().and_then(toml_expr),
        };
        em.vars.insert(
            v.name.clone(),
            VarSpec {
                name: v.name.clone(),
                var_type: v.var_type.clone(),
                description: v.description.clone(),
                default,
                sensitive: v.sensitive,
            },
        );
        em.used_vars.insert(v.name.clone());
    }
    if tool == Tool::OpenTofu && p.settings.state_encryption {
        em.vars.insert(
            "state_passphrase".into(),
            VarSpec {
                name: "state_passphrase".into(),
                var_type: "string".into(),
                description: "Passphrase for OpenTofu state encryption (pbkdf2 key provider).".into(),
                default: None,
                sensitive: true,
            },
        );
        em.used_vars.insert("state_passphrase".into());
    }

    // Phase 1: plan which (entity, block) pairs exist, so cross references can be checked.
    let order = ttg_core::graph::dependency_order(p)
        .map_err(|c| GenError::Emit(format!("dependency cycle: {}", c.join(" -> "))))?;
    for id in &order {
        let e = p.entity(id).unwrap();
        if let Status::Emit(m) = em.status(&e) {
            for b in &m.blocks {
                let inst = em.block_instances(&e, b);
                if inst.is_empty() {
                    continue;
                }
                em.planned.insert((id.clone(), b.key.clone()));
                let locals: Vec<String> = if repeated(b) {
                    inst.into_iter()
                        .map(|i| format!("{}_{}", local_name(p, id, &b.key), i))
                        .collect()
                } else {
                    vec![local_name(p, id, &b.key)]
                };
                em.instances.insert((id.clone(), b.key.clone()), locals);
            }
            for d in &m.data {
                if !em.block_instances(&e, d).is_empty() {
                    em.planned_data.insert((id.clone(), format!("data:{}", d.key)));
                }
            }
        }
    }

    // Phase 2: resolve.
    let mut emitted: Vec<Emitted> = Vec::new();
    let mut outputs: Vec<OutputSpec> = Vec::new();
    for id in &order {
        let e = p.entity(id).unwrap();
        let def = cat.resource(e.resource_type).unwrap();
        match em.status(&e) {
            Status::Emit(m) => {
                let consumed = consumed_relations(m);
                let primary = primary_key(m);
                let file = m.file.clone().unwrap_or(def.resource.category.clone());
                for d in &m.data {
                    if !em.planned_data.contains(&(id.clone(), format!("data:{}", d.key))) {
                        continue;
                    }
                    let rows = em.rows_for(&e, d);
                    for (n, row) in rows.iter().enumerate() {
                        let item = row.ctx(n);
                        if let Some(c) = &d.when {
                            if !em.cond_holds(&e, c, item) {
                                continue;
                            }
                        }
                        let local = data_local_name(p, id, &d.key, repeated(d).then_some(n));
                        let block = em.build_block("data", &e, m, d, &local, item)?;
                        emitted.push(Emitted {
                            entity: id.clone(),
                            kind: "data",
                            resource_type: d.resource.clone(),
                            local,
                            file: file.clone(),
                            block,
                        });
                    }
                }
                for b in &m.blocks {
                    if !em.planned.contains(&(id.clone(), b.key.clone())) {
                        continue;
                    }
                    let rows = em.rows_for(&e, b);
                    for (n, row) in rows.iter().enumerate() {
                        let item = row.ctx(n);
                        if let Some(c) = &b.when {
                            if !em.cond_holds(&e, c, item) {
                                continue;
                            }
                        }
                        let local = if repeated(b) {
                            format!("{}_{}", local_name(p, id, &b.key), n)
                        } else {
                            local_name(p, id, &b.key)
                        };
                        let mut block = em.build_block("resource", &e, m, b, &local, item)?;
                        if b.key == primary && n == 0 {
                            let deps = em.explicit_depends(&e, &consumed);
                            if !deps.is_empty() {
                                block = with_depends_on(block, deps);
                            }
                        }
                        emitted.push(Emitted {
                            entity: id.clone(),
                            kind: "resource",
                            resource_type: b.resource.clone(),
                            local,
                            file: file.clone(),
                            block,
                        });
                    }
                }
                for (suffix, o) in &m.outputs {
                    let key = o.block.clone().unwrap_or(primary.clone());
                    if !em.planned.contains(&(id.clone(), key.clone())) {
                        continue;
                    }
                    let bdef = m.blocks.iter().find(|b| b.key == key).unwrap();
                    let Some(local) = em.instances[&(id.clone(), key.clone())].first().cloned() else {
                        continue;
                    };
                    outputs.push(OutputSpec {
                        name: format!("{}_{}", p.hcl_name(id), suffix),
                        value: traversal(&bdef.resource, &local, &o.attr),
                        description: if o.description.is_empty() {
                            format!("{} of {} \"{}\"", o.attr, def.resource.display_name, e.name)
                        } else {
                            format!("{} — {} \"{}\"", o.description, def.resource.display_name, e.name)
                        },
                        sensitive: o.sensitive,
                    });
                }
                for step in diagnostics::applicable_manual_steps(p, cat, provider, &e, m) {
                    em.manual.push(ManualEntry {
                        entity: Some(id.clone()),
                        title: format!("{} \"{}\": {}", def.resource.display_name, e.name, step.title),
                        body: step.body.trim().to_string(),
                    });
                }
            }
            Status::Manual => em.note_manual_entity(&e, "flagged as external / managed by hand"),
            Status::Unmapped => em.note_manual_entity(
                &e,
                &format!(
                    "no {} mapping exists for this resource type",
                    em.pdef.provider.display_name
                ),
            ),
            Status::Logical => {}
        }
    }

    // Duplicate address guard.
    let mut seen = HashSet::new();
    for b in &emitted {
        if !seen.insert((b.kind, b.resource_type.clone(), b.local.clone())) {
            return Err(GenError::Emit(format!(
                "two blocks would share the address {}.{}; rename one of them",
                b.resource_type, b.local
            )));
        }
    }

    // Phase 3: files.
    let header = em.profile.file_header(&pdef.provider.display_name);
    let mut files: IndexMap<String, String> = IndexMap::new();

    let mut by_file: IndexMap<String, Vec<(String, Vec<Block>)>> = IndexMap::new();
    for id in &order {
        let e = p.entity(id).unwrap();
        let def = cat.resource(e.resource_type).unwrap();
        let mine: Vec<&Emitted> = emitted.iter().filter(|b| &b.entity == id).collect();
        if mine.is_empty() {
            continue;
        }
        let file = mine[0].file.clone();
        let comment = format!(
            "{} \"{}\" ({})",
            def.resource.display_name, e.name, e.resource_type
        );
        let blocks: Vec<Block> = mine.iter().map(|b| b.block.clone()).collect();
        by_file.entry(file).or_default().push((comment, blocks));
    }
    let mut file_names: Vec<String> = by_file.keys().cloned().collect();
    file_names.sort();
    for name in file_names {
        let content = files::render_resource_file(&header, &by_file[&name]);
        files.insert(format!("{name}.tf"), content);
    }

    let vars: Vec<&VarSpec> = em
        .vars
        .values()
        .filter(|v| em.used_vars.contains(&v.name))
        .collect();
    files.insert("variables.tf".into(), files::render_variables(&header, &vars));
    files.insert("outputs.tf".into(), files::render_outputs(&header, &outputs));
    files.insert(
        "versions.tf".into(),
        files::render_versions(&header, &em.profile, pdef),
    );
    let provider_block = em.build_provider_block()?;
    files.insert(
        "providers.tf".into(),
        files::render_providers(&header, &provider_block),
    );
    if let Some(b) = em
        .profile
        .backend_block(p.settings.backend.as_ref(), p.settings.state_encryption)
    {
        files.insert("backend.tf".into(), files::render_backend(&header, &b));
    }
    if !em.manual.is_empty() {
        files.insert(
            "MANUAL_STEPS.md".into(),
            files::render_manual_steps(p, cat, &pdef.provider.display_name, &em.manual, &em.vars),
        );
    }
    files.insert(
        "README.md".into(),
        files::render_readme(p, &em.profile, pdef, !em.manual.is_empty()),
    );

    Ok(Generated {
        provider: provider.to_string(),
        provider_display: pdef.provider.display_name.clone(),
        tool,
        files,
        manual_steps: em.manual,
        diagnostics: diags,
    })
}

/// The block other resources reference: `main` if present, else the first.
fn primary_key(m: &ProviderMapping) -> String {
    m.blocks
        .iter()
        .find(|b| b.key == "main")
        .or(m.blocks.first())
        .map(|b| b.key.clone())
        .unwrap_or_default()
}

fn local_name(p: &Project, id: &str, key: &str) -> String {
    let slug = p.hcl_name(id);
    if key == "main" {
        slug
    } else {
        format!("{slug}_{key}")
    }
}

fn data_local_name(p: &Project, id: &str, key: &str, index: Option<usize>) -> String {
    let base = format!("{}_{}", p.hcl_name(id), key);
    match index {
        Some(i) => format!("{base}_{i}"),
        None => base,
    }
}

pub(crate) fn traversal(resource_type: &str, local: &str, attr: &str) -> Expression {
    let t = Traversal::builder(Variable::unchecked(resource_type)).attr(local);
    Expression::Traversal(Box::new(attr_path(t, attr).build()))
}

/// Append a dotted attribute path such as `kube_config.0.host` as proper traversal
/// steps: identifiers become `.attr`, numbers become `[n]`.
fn attr_path(mut t: hcl::expr::TraversalBuilder, attr: &str) -> hcl::expr::TraversalBuilder {
    for seg in attr.split('.').filter(|s| !s.is_empty()) {
        match seg.parse::<u64>() {
            Ok(n) => t = t.index(n),
            Err(_) => t = t.attr(seg),
        }
    }
    t
}

fn data_traversal(resource_type: &str, local: &str, attr: &str) -> Expression {
    let t = Traversal::builder(Variable::unchecked("data"))
        .attr(resource_type)
        .attr(local);
    Expression::Traversal(Box::new(attr_path(t, attr).build()))
}

fn var_ref(name: &str) -> Expression {
    Expression::Traversal(Box::new(
        Traversal::builder(Variable::unchecked("var")).attr(name).build(),
    ))
}

fn with_depends_on(block: Block, deps: Vec<Expression>) -> Block {
    let mut b = Block::builder(block.identifier.clone());
    for l in block.labels.iter() {
        b = b.add_label(l.clone());
    }
    for s in block.body.iter() {
        b = b.add_structure(s.clone());
    }
    b = b.add_attribute(("depends_on", Expression::Array(deps)));
    b.build()
}

pub(crate) fn toml_expr(v: &toml::Value) -> Option<Expression> {
    Some(match v {
        toml::Value::String(s) => Expression::String(s.clone()),
        toml::Value::Integer(i) => Expression::Number(Number::from(*i)),
        toml::Value::Float(f) => Expression::Number(Number::from_f64(*f)?),
        toml::Value::Boolean(b) => Expression::Bool(*b),
        toml::Value::Array(a) => Expression::Array(a.iter().filter_map(toml_expr).collect()),
        toml::Value::Table(t) => {
            let mut o = Object::new();
            for (k, v) in t {
                o.insert(object_key(k), toml_expr(v)?);
            }
            Expression::Object(o)
        }
        toml::Value::Datetime(d) => Expression::String(d.to_string()),
    })
}

fn value_expr(v: &Value) -> Expression {
    match v {
        Value::Bool(b) => Expression::Bool(*b),
        Value::Int(i) => Expression::Number(Number::from(*i)),
        Value::Float(f) => Number::from_f64(*f)
            .map(Expression::Number)
            .unwrap_or(Expression::Null),
        Value::Str(s) => Expression::String(s.clone()),
        Value::List(l) => Expression::Array(l.iter().map(|s| Expression::String(s.clone())).collect()),
        Value::Records(rows) => Expression::Array(
            rows.iter()
                .map(|r| {
                    let mut o = Object::new();
                    for (k, v) in r {
                        o.insert(object_key(k), value_expr(v));
                    }
                    Expression::Object(o)
                })
                .collect(),
        ),
    }
}

fn object_key(k: &str) -> ObjectKey {
    match Identifier::new(k) {
        Ok(id) => ObjectKey::Identifier(id),
        Err(_) => ObjectKey::Expression(Expression::String(k.to_string())),
    }
}

fn apply_wrap(e: Expression, wrap: Option<Wrap>) -> Expression {
    match (wrap, e) {
        (Some(Wrap::List), Expression::Array(a)) => Expression::Array(a),
        (Some(Wrap::List), other) => Expression::Array(vec![other]),
        (None, other) => other,
    }
}

fn transformed(v: &Value, t: Option<ttg_catalog::Transform>) -> Value {
    match (t, v) {
        (Some(t), Value::Str(s)) => Value::Str(t.apply(s)),
        (Some(t), Value::List(l)) => Value::List(l.iter().map(|s| t.apply(s)).collect()),
        _ => v.clone(),
    }
}

/// Scalar value of a field or provider field, used for lookups and rows.
fn any_field<'a>(e: &EntityRef<'a>, provider: &str, name: &str) -> Option<&'a Value> {
    e.field(name).or_else(|| e.provider_field(provider, name))
}

impl<'a> Emitter<'a> {
    fn status(&self, e: &EntityRef<'a>) -> Status<'a> {
        if e.manual {
            return Status::Manual;
        }
        match self.cat.mapping(e.resource_type, self.provider) {
            None => Status::Unmapped,
            Some(m) if m.status == MappingStatus::Logical => Status::Logical,
            Some(m) => Status::Emit(m),
        }
    }

    /// The rows a block iterates: one per relation target, one per struct_list row /
    /// string_list entry, or a single empty row for a plain block.
    fn rows_for(&self, e: &EntityRef<'a>, b: &BlockDef) -> Vec<Row> {
        self.rows(
            e,
            b.for_each_field.as_deref(),
            b.for_each_relation.as_deref(),
            b.for_each_target_type.as_deref(),
        )
    }

    fn rows(
        &self,
        e: &EntityRef<'a>,
        field: Option<&str>,
        relation: Option<&str>,
        target_type: Option<&str>,
    ) -> Vec<Row> {
        if let Some(rel) = relation {
            let Some(kind) = Relation::from_key(rel) else {
                return Vec::new();
            };
            return self
                .relation_targets_filtered(e, kind, target_type)
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
        match any_field(e, self.provider, field) {
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

    /// Indices of the rows of a block that survive its `when` condition (or `[0]` for a
    /// plain block whose condition holds). Empty means the block is not emitted.
    fn block_instances(&self, e: &EntityRef<'a>, b: &BlockDef) -> Vec<usize> {
        let rows = self.rows_for(e, b);
        rows.iter()
            .enumerate()
            .filter(|(n, row)| b.when.as_ref().is_none_or(|c| self.cond_holds(e, c, row.ctx(*n))))
            .map(|(n, _)| n)
            .collect()
    }

    fn relation_targets_filtered(
        &self,
        e: &EntityRef<'a>,
        kind: Relation,
        target_type: Option<&str>,
    ) -> Vec<Id> {
        relation_targets(self.p, self.cat, e, kind)
            .into_iter()
            .filter(|t| target_type.is_none_or(|tt| self.p.entity(t).is_some_and(|x| x.resource_type == tt)))
            .collect()
    }

    fn cond_holds(&self, e: &EntityRef<'a>, c: &Condition, item: Option<ItemCtx<'_>>) -> bool {
        diagnostics::condition_holds(
            self.p,
            self.cat,
            self.provider,
            e,
            c,
            item.and_then(|i| i.record.map(|r| (r, i.index))),
        )
    }

    fn build_block(
        &mut self,
        kind: &str,
        e: &EntityRef<'a>,
        m: &ProviderMapping,
        b: &BlockDef,
        local: &str,
        item: Option<ItemCtx<'_>>,
    ) -> Result<Block, GenError> {
        let mut builder = Block::builder(kind)
            .add_label(b.resource.as_str())
            .add_label(local);
        let extra = e.extra_args(self.provider, &b.key).cloned().unwrap_or_default();
        for (k, src) in &b.args {
            if extra.contains_key(k) {
                continue; // an extra argument overrides what the mapping sets
            }
            let at = format!("{} \"{}\" / {}.{}", e.resource_type, e.name, b.resource, k);
            if let Some(expr) = self.resolve(e, m, src, &at, item)? {
                builder = builder.add_attribute((k.as_str(), expr));
            }
        }
        for n in &b.nested {
            if extra.contains_key(&n.block) {
                continue;
            }
            for nb in self.build_nested(e, m, n, &b.resource, item)? {
                builder = builder.add_block(nb);
            }
        }
        if !extra.is_empty() {
            let schema = ttg_schema::index().resource(self.provider, &b.resource).cloned();
            let at = format!(
                "{} \"{}\" / {} extra arguments",
                e.resource_type, e.name, b.resource
            );
            builder = self.apply_extras(builder, &extra, schema.as_ref(), &at)?;
        }
        Ok(builder.build())
    }

    /// Merge extra arguments into a block: nested blocks per the schema, everything else
    /// as attributes.
    fn apply_extras(
        &mut self,
        mut builder: hcl::structure::BlockBuilder,
        extra: &ttg_core::ExtraArgs,
        schema: Option<&ttg_schema::BlockSchema>,
        at: &str,
    ) -> Result<hcl::structure::BlockBuilder, GenError> {
        for (k, v) in extra {
            let nested = schema.and_then(|s| s.blocks.get(k));
            match (nested, v) {
                (Some(ns), serde_json::Value::Object(o)) => {
                    builder = builder.add_block(self.json_block(k, o, Some(ns.block()), at)?);
                }
                (Some(ns), serde_json::Value::Array(items)) => {
                    for it in items {
                        if let serde_json::Value::Object(o) = it {
                            builder = builder.add_block(self.json_block(k, o, Some(ns.block()), at)?);
                        } else {
                            return Err(GenError::Emit(format!(
                                "{at}: nested block '{k}' items must be objects"
                            )));
                        }
                    }
                }
                (_, v) => {
                    let expr = self.json_expr(v, &format!("{at}.{k}"))?;
                    builder = builder.add_attribute((k.as_str(), expr));
                }
            }
        }
        Ok(builder)
    }

    fn json_block(
        &mut self,
        name: &str,
        o: &serde_json::Map<String, serde_json::Value>,
        schema: Option<&ttg_schema::BlockSchema>,
        at: &str,
    ) -> Result<Block, GenError> {
        let mut b = Block::builder(name);
        for (k, v) in o {
            let nested = schema.and_then(|s| s.blocks.get(k));
            match (nested, v) {
                (Some(ns), serde_json::Value::Object(inner)) => {
                    b = b.add_block(self.json_block(k, inner, Some(ns.block()), at)?);
                }
                (Some(ns), serde_json::Value::Array(items)) => {
                    for it in items {
                        if let serde_json::Value::Object(inner) = it {
                            b = b.add_block(self.json_block(k, inner, Some(ns.block()), at)?);
                        }
                    }
                }
                (_, v) => {
                    let expr = self.json_expr(v, &format!("{at}.{name}.{k}"))?;
                    b = b.add_attribute((k.as_str(), expr));
                }
            }
        }
        Ok(b.build())
    }

    /// JSON -> HCL expression, honouring `{"$ref": …}` and `{"$raw": …}`.
    fn json_expr(&mut self, v: &serde_json::Value, at: &str) -> Result<Expression, GenError> {
        Ok(match v {
            serde_json::Value::Null => Expression::Null,
            serde_json::Value::Bool(b) => Expression::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Expression::Number(Number::from(i))
                } else {
                    Number::from_f64(n.as_f64().unwrap_or(0.0))
                        .map(Expression::Number)
                        .unwrap_or(Expression::Null)
                }
            }
            serde_json::Value::String(s) => Expression::String(s.clone()),
            serde_json::Value::Array(items) => {
                let mut out = Vec::new();
                for it in items {
                    out.push(self.json_expr(it, at)?);
                }
                Expression::Array(out)
            }
            serde_json::Value::Object(o) => {
                if let Some(raw) = o.get("$raw").and_then(|r| r.as_str()) {
                    return Ok(raw_expr(raw));
                }
                if let Some(r) = o.get("$ref") {
                    let key = r
                        .get("entity")
                        .and_then(|x| x.as_str())
                        .ok_or_else(|| GenError::Emit(format!("{at}: $ref needs an \"entity\"")))?;
                    let attr = r.get("attr").and_then(|x| x.as_str()).unwrap_or("id");
                    let block = r.get("block").and_then(|x| x.as_str());
                    let id = self
                        .p
                        .entity(key)
                        .map(|e| e.id.to_string())
                        .or_else(|| {
                            self.p
                                .entities()
                                .iter()
                                .find(|e| e.name.eq_ignore_ascii_case(key))
                                .map(|e| e.id.to_string())
                        })
                        .ok_or_else(|| GenError::Emit(format!("{at}: $ref to unknown resource \"{key}\"")))?;
                    return self.reference(&id, block, attr, at)?.ok_or_else(|| {
                        GenError::Emit(format!(
                            "{at}: $ref target \"{key}\" produces nothing for this provider"
                        ))
                    });
                }
                let mut obj = Object::new();
                for (k, v) in o {
                    obj.insert(object_key(k), self.json_expr(v, &format!("{at}.{k}"))?);
                }
                Expression::Object(obj)
            }
        })
    }

    /// A nested block definition yields zero or more blocks (more than one when it has
    /// `for_each_field` / `for_each_relation`).
    fn build_nested(
        &mut self,
        e: &EntityRef<'a>,
        m: &ProviderMapping,
        n: &NestedBlockDef,
        parent: &str,
        outer_item: Option<ItemCtx<'_>>,
    ) -> Result<Vec<Block>, GenError> {
        let own_rows = n.for_each_field.is_some() || n.for_each_relation.is_some();
        let rows: Vec<Row> = if own_rows {
            self.rows(
                e,
                n.for_each_field.as_deref(),
                n.for_each_relation.as_deref(),
                n.for_each_target_type.as_deref(),
            )
        } else {
            vec![Row::default()]
        };
        let mut out = Vec::new();
        for (idx, row) in rows.iter().enumerate() {
            let item = if own_rows { row.ctx(idx) } else { outer_item };
            if let Some(c) = &n.when {
                if !self.cond_holds(e, c, item) {
                    continue;
                }
            }
            let mut builder = Block::builder(n.block.as_str());
            for l in &n.labels {
                builder = builder.add_label(l.as_str());
            }
            for (k, src) in &n.args {
                let at = format!(
                    "{} \"{}\" / {} {}.{}",
                    e.resource_type, e.name, parent, n.block, k
                );
                if let Some(expr) = self.resolve(e, m, src, &at, item)? {
                    builder = builder.add_attribute((k.as_str(), expr));
                }
            }
            for inner in &n.nested {
                for nb in self.build_nested(e, m, inner, &n.block, item)? {
                    builder = builder.add_block(nb);
                }
            }
            out.push(builder.build());
        }
        Ok(out)
    }

    fn build_provider_block(&mut self) -> Result<Block, GenError> {
        let mut builder = Block::builder("provider").add_label(self.pdef.provider.local_name());
        // Provider block sources may only use `var` / literals; resolve with a dummy entity.
        for (k, src) in &self.pdef.provider_block.args {
            let expr = match src {
                ArgSource::Var(v) => {
                    self.used_vars.insert(v.var.clone());
                    Some(var_ref(&v.var))
                }
                ArgSource::Literal(l) => toml_expr(&l.value),
                ArgSource::Raw(r) => Some(raw_expr(&r.raw)),
                _ => {
                    return Err(GenError::Emit(format!(
                        "provider block argument '{k}' may only use var, value or raw sources"
                    )))
                }
            };
            if let Some(expr) = expr {
                builder = builder.add_attribute((k.as_str(), expr));
            }
        }
        for n in &self.pdef.provider_block.nested {
            let mut nb = Block::builder(n.block.as_str());
            for (k, src) in &n.args {
                if let ArgSource::Literal(l) = src {
                    if let Some(x) = toml_expr(&l.value) {
                        nb = nb.add_attribute((k.as_str(), x));
                    }
                }
            }
            builder = builder.add_block(nb.build());
        }
        Ok(builder.build())
    }

    /// Resolve a source. `Ok(None)` means "omit this argument".
    fn resolve(
        &mut self,
        e: &EntityRef<'a>,
        m: &ProviderMapping,
        src: &ArgSource,
        at: &str,
        item: Option<ItemCtx<'_>>,
    ) -> Result<Option<Expression>, GenError> {
        let err = |msg: String| GenError::Emit(format!("{at}: {msg}"));
        Ok(match src {
            ArgSource::Literal(l) => toml_expr(&l.value),
            ArgSource::Field(f) => {
                let v = if f.field == "name" {
                    Some(Value::Str(e.name.to_string()))
                } else {
                    e.field(&f.field).cloned()
                };
                match v {
                    Some(v) if !v.is_empty() => {
                        Some(apply_wrap(value_expr(&transformed(&v, f.transform)), f.wrap))
                    }
                    _ => match &f.fallback {
                        Some(fb) => self.resolve(e, m, fb, &format!("{at}.fallback"), item)?,
                        None => None,
                    },
                }
            }
            ArgSource::ProviderField(f) => match e.provider_field(self.provider, &f.provider_field) {
                Some(v) if !v.is_empty() => {
                    Some(apply_wrap(value_expr(&transformed(v, f.transform)), f.wrap))
                }
                _ => match &f.fallback {
                    Some(fb) => self.resolve(e, m, fb, &format!("{at}.fallback"), item)?,
                    None => None,
                },
            },
            ArgSource::Item(i) => {
                let Some(record) = item.and_then(|c| c.record) else {
                    return Err(err("`item` used outside a for_each_field block".into()));
                };
                match record.get(&i.item) {
                    Some(v) if !v.is_empty() => {
                        Some(apply_wrap(value_expr(&transformed(v, i.transform)), i.wrap))
                    }
                    _ => None,
                }
            }
            ArgSource::ItemIndex(s) => {
                let Some(ctx) = item else {
                    return Err(err("`item_index` used outside a repeated block".into()));
                };
                Some(Expression::Number(Number::from(
                    s.item_index.base + s.item_index.step * ctx.index as i64,
                )))
            }
            ArgSource::EntityVar(v) => {
                let name = format!("{}_{}", self.p.hcl_name(e.id), v.entity_var);
                let description = self
                    .render_template(e, &v.description, item)
                    .unwrap_or_else(|_| v.description.clone());
                if !self.vars.contains_key(&name) {
                    self.vars.insert(
                        name.clone(),
                        VarSpec {
                            name: name.clone(),
                            var_type: v.var_type.clone(),
                            description,
                            default: v.default.as_ref().and_then(toml_expr),
                            sensitive: v.sensitive,
                        },
                    );
                }
                self.used_vars.insert(name.clone());
                Some(var_ref(&name))
            }
            ArgSource::ItemRef(r) => {
                let Some(record) = item.and_then(|c| c.record) else {
                    return Err(err("`item_ref` used outside a for_each_field block".into()));
                };
                match record.get(&r.item_ref).and_then(|v| v.as_str()) {
                    Some(id) if !id.is_empty() => {
                        let id = id.to_string();
                        self.reference(&id, r.block.as_deref(), &r.attr, at)?
                            .map(|x| apply_wrap(x, r.wrap))
                    }
                    _ => None,
                }
            }
            ArgSource::Target(t) => {
                let Some(target) = item.and_then(|c| c.target) else {
                    return Err(err("`target` used outside a for_each_relation block".into()));
                };
                let target = target.to_string();
                let subject = match &t.ancestor {
                    Some(anc) => match self.p.ancestor_of_type(&target, anc) {
                        Some(c) => c.id.clone(),
                        None => return Ok(None),
                    },
                    None => target,
                };
                self.reference(&subject, t.block.as_deref(), &t.target, at)?
                    .map(|x| apply_wrap(x, t.wrap))
            }
            ArgSource::If(i) => {
                if self.cond_holds(e, &i.cond, item) {
                    self.resolve(e, m, &i.then, &format!("{at}.then"), item)?
                } else {
                    match &i.otherwise {
                        Some(o) => self.resolve(e, m, o, &format!("{at}.else"), item)?,
                        None => None,
                    }
                }
            }
            ArgSource::Var(v) => {
                if !self.vars.contains_key(&v.var) {
                    let def = m
                        .variables
                        .iter()
                        .find(|x| x.name == v.var)
                        .ok_or_else(|| err(format!("unknown variable '{}'", v.var)))?;
                    self.vars.insert(
                        def.name.clone(),
                        VarSpec {
                            name: def.name.clone(),
                            var_type: def.var_type.clone(),
                            description: def.description.clone(),
                            default: def.default.as_ref().and_then(toml_expr),
                            sensitive: def.sensitive,
                        },
                    );
                }
                self.used_vars.insert(v.var.clone());
                Some(var_ref(&v.var))
            }
            ArgSource::Template(t) => {
                let rendered = self.render_template(e, &t.template, item).map_err(err)?;
                Some(Expression::String(match t.transform {
                    Some(tr) => tr.apply(&rendered),
                    None => rendered,
                }))
            }
            ArgSource::Map(mp) => {
                let from_item = item.and_then(|c| c.record).and_then(|r| r.get(&mp.map).cloned());
                let v = from_item.or_else(|| any_field(e, self.provider, &mp.map).cloned());
                match v {
                    Some(v) if !v.is_empty() => {
                        let key = v.display();
                        match mp.table.get(&key) {
                            Some(t) => toml_expr(t),
                            None if mp.optional => None,
                            None => {
                                return Err(err(format!(
                                    "value '{key}' of '{}' has no entry in the lookup table",
                                    mp.map
                                )))
                            }
                        }
                    }
                    _ => None,
                }
            }
            ArgSource::Relation(r) => {
                let kind = Relation::from_key(&r.relation)
                    .ok_or_else(|| err(format!("unknown relation '{}'", r.relation)))?;
                let targets = self.relation_targets_filtered(e, kind, r.target_type.as_deref());
                let mut exprs = Vec::new();
                for t in targets {
                    // `ancestor = "..."`: reference the target's enclosing container instead.
                    let subject = match &r.ancestor {
                        Some(anc) => match self.p.ancestor_of_type(&t, anc) {
                            Some(c) => c.id.clone(),
                            None => continue,
                        },
                        None => t,
                    };
                    if let Some(x) = self.reference(&subject, r.block.as_deref(), &r.attr, at)? {
                        exprs.push(x);
                    }
                }
                match (exprs.is_empty(), r.wrap) {
                    (true, _) => match &r.fallback {
                        Some(fb) => self.resolve(e, m, fb, &format!("{at}.fallback"), item)?,
                        None => None,
                    },
                    (false, Some(Wrap::List)) => Some(Expression::Array(exprs)),
                    (false, None) => Some(exprs.remove(0)),
                }
            }
            ArgSource::Ancestor(a) => match self.p.ancestor_of_type(e.id, &a.ancestor) {
                Some(c) => {
                    let id = c.id.clone();
                    self.reference(&id, a.block.as_deref(), &a.attr, at)?
                }
                None if a.optional => None,
                None => return Err(err(format!("requires an enclosing {} container", a.ancestor))),
            },
            ArgSource::SelfBlock(s) => {
                let key = (e.id.to_string(), s.self_block.clone());
                if !self.planned.contains(&key) {
                    None
                } else {
                    let bdef = m.blocks.iter().find(|b| b.key == s.self_block).unwrap();
                    let locals = self.instances.get(&key).cloned().unwrap_or_default();
                    let mut exprs: Vec<Expression> = locals
                        .iter()
                        .map(|l| traversal(&bdef.resource, l, &s.attr))
                        .collect();
                    match (exprs.is_empty(), s.wrap, repeated(bdef)) {
                        (true, _, _) => None,
                        (false, Some(Wrap::List), _) => Some(Expression::Array(exprs)),
                        (false, None, true) => Some(Expression::Array(exprs)),
                        (false, None, false) => Some(exprs.remove(0)),
                    }
                }
            }
            ArgSource::SelfData(s) => {
                if !self
                    .planned_data
                    .contains(&(e.id.to_string(), format!("data:{}", s.self_data)))
                {
                    None
                } else {
                    let ddef = m.data.iter().find(|d| d.key == s.self_data).unwrap();
                    let local = data_local_name(self.p, e.id, &s.self_data, None);
                    Some(apply_wrap(
                        data_traversal(&ddef.resource, &local, &s.attr),
                        s.wrap,
                    ))
                }
            }
            ArgSource::Object(o) => {
                let mut obj = Object::new();
                for (k, s) in &o.object {
                    if let Some(x) = self.resolve(e, m, s, &format!("{at}.{k}"), item)? {
                        obj.insert(object_key(k), x);
                    }
                }
                Some(Expression::Object(obj))
            }
            ArgSource::List(l) => {
                let mut items = Vec::new();
                for (i, s) in l.list.iter().enumerate() {
                    if let Some(x) = self.resolve(e, m, s, &format!("{at}[{i}]"), item)? {
                        items.push(x);
                    }
                }
                Some(Expression::Array(items))
            }
            ArgSource::Func(f) => {
                let mut fc = FuncCall::builder(f.func.as_str());
                for (i, s) in f.args.iter().enumerate() {
                    if let Some(x) = self.resolve(e, m, s, &format!("{at}.{}({i})", f.func), item)? {
                        fc = fc.arg(x);
                    }
                }
                Some(Expression::FuncCall(Box::new(fc.build())))
            }
            ArgSource::Raw(r) => Some(raw_expr(&r.raw)),
        })
    }

    /// A traversal to another entity's block attribute — or, when the target is manual or
    /// unmapped, an input variable standing in for it.
    fn reference(
        &mut self,
        target: &str,
        block: Option<&str>,
        attr: &str,
        at: &str,
    ) -> Result<Option<Expression>, GenError> {
        let t = self
            .p
            .entity(target)
            .ok_or_else(|| GenError::Emit(format!("{at}: unknown target '{target}'")))?;
        match self.status(&t) {
            Status::Emit(tm) => {
                let key = block.map(|s| s.to_string()).unwrap_or(primary_key(tm));
                let pk = (target.to_string(), key.clone());
                if !self.planned.contains(&pk) {
                    return Ok(None);
                }
                let bdef = tm
                    .blocks
                    .iter()
                    .find(|b| b.key == key)
                    .ok_or_else(|| GenError::Emit(format!("{at}: target has no block '{key}'")))?;
                let Some(local) = self.instances.get(&pk).and_then(|v| v.first()).cloned() else {
                    return Ok(None);
                };
                Ok(Some(traversal(&bdef.resource, &local, attr)))
            }
            Status::Logical => Ok(None),
            Status::Manual | Status::Unmapped => {
                let reason = if t.manual {
                    "flagged as external / managed by hand"
                } else {
                    "no mapping for this provider"
                };
                let tdef = self.cat.resource(t.resource_type);
                let display = tdef
                    .map(|d| d.resource.display_name.clone())
                    .unwrap_or(t.resource_type.to_string());
                let var_name = format!("{}_{}", self.p.hcl_name(target), attr.replace('.', "_"));
                self.vars.entry(var_name.clone()).or_insert(VarSpec {
                    name: var_name.clone(),
                    var_type: "string".into(),
                    description: format!(
                        "`{attr}` of the {display} \"{}\" ({reason}). Supply after creating it.",
                        t.name
                    ),
                    default: None,
                    sensitive: false,
                });
                self.used_vars.insert(var_name.clone());
                self.note_manual_entity(&t, reason);
                Ok(Some(var_ref(&var_name)))
            }
        }
    }

    fn note_manual_entity(&mut self, t: &EntityRef<'a>, reason: &str) {
        if !self.manual_refs_done.insert(t.id.to_string()) {
            return;
        }
        let display = self
            .cat
            .resource(t.resource_type)
            .map(|d| d.resource.display_name.clone())
            .unwrap_or(t.resource_type.to_string());
        self.manual.push(ManualEntry {
            entity: Some(t.id.to_string()),
            title: format!("Create {display} \"{}\" by hand ({reason})", t.name),
            body: String::new(), // details rendered from the project by files::render_manual_steps
        });
    }

    fn render_template(
        &self,
        e: &EntityRef<'a>,
        t: &str,
        item: Option<ItemCtx<'_>>,
    ) -> Result<String, String> {
        let mut out = String::new();
        let mut rest = t;
        while let Some(start) = rest.find('{') {
            out.push_str(&rest[..start]);
            let after = &rest[start + 1..];
            let Some(end) = after.find('}') else {
                out.push_str(&rest[start..]);
                return Ok(out);
            };
            let ph = &after[..end];
            let val = if ph == "name" {
                Some(e.name.to_string())
            } else if let Some(pf) = ph.strip_prefix("provider.") {
                e.provider_field(self.provider, pf).map(|v| v.display())
            } else if let Some(s) = ph.strip_prefix("settings.") {
                self.p
                    .settings
                    .provider_settings
                    .get(self.provider)
                    .and_then(|m| m.get(s))
                    .cloned()
            } else if let Some(name) = ph.strip_prefix("item.") {
                if name == "index" {
                    item.map(|c| c.index.to_string())
                } else {
                    item.and_then(|c| c.record)
                        .and_then(|r| r.get(name))
                        .map(|v| v.display())
                }
            } else if let Some(attr) = ph.strip_prefix("target.") {
                // Only `target.name` / `target.slug` make sense in a template.
                item.and_then(|c| c.target)
                    .and_then(|tid| self.p.entity(tid))
                    .map(|te| match attr {
                        "slug" => ttg_core::slugify(te.name),
                        _ => te.name.to_string(),
                    })
            } else {
                e.field(ph).map(|v| v.display())
            };
            out.push_str(&val.ok_or_else(|| format!("template placeholder '{{{ph}}}' has no value"))?);
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }

    /// `depends_on` targets for edges the mapping did not consume, plus `depends_on` edges.
    fn explicit_depends(&mut self, e: &EntityRef<'a>, consumed: &[Consumed]) -> Vec<Expression> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let edges: Vec<_> = self.p.edges_from(e.id).cloned().collect();
        for edge in edges {
            let Some(t) = self.p.entity(&edge.target) else {
                continue;
            };
            let is_consumed = consumed.iter().any(|c| {
                c.relation == edge.relation.key()
                    && c.target_type.as_deref().is_none_or(|tt| tt == t.resource_type)
            });
            if edge.relation != Relation::DependsOn && is_consumed {
                continue;
            }
            let tname = t.name.to_string();
            match self.status(&t) {
                Status::Emit(tm) => {
                    let key = primary_key(tm);
                    let pk = (edge.target.clone(), key.clone());
                    if !self.planned.contains(&pk) {
                        continue;
                    }
                    let bdef = tm.blocks.iter().find(|b| b.key == key).unwrap();
                    let Some(local) = self.instances.get(&pk).and_then(|v| v.first()).cloned() else {
                        continue;
                    };
                    if seen.insert((bdef.resource.clone(), local.clone())) {
                        out.push(traversal(&bdef.resource, &local, ""));
                    }
                    if edge.relation != Relation::DependsOn {
                        self.manual.push(ManualEntry {
                            entity: Some(e.id.to_string()),
                            title: format!(
                                "Link \"{}\" to \"{}\" by hand ({})",
                                e.name,
                                tname,
                                edge.relation.display_name()
                            ),
                            body: format!(
                                "The {} mapping for `{}` has no way to express this relationship. \
                                 A `depends_on` was emitted for ordering only; configure the actual \
                                 link (permissions, connection strings, endpoints) manually.",
                                self.pdef.provider.display_name, e.resource_type
                            ),
                        });
                    }
                }
                Status::Logical => {}
                Status::Manual | Status::Unmapped => {
                    let reason = if t.manual { "external / manual" } else { "unmapped" };
                    self.note_manual_entity(&t, reason);
                    self.manual.push(ManualEntry {
                        entity: Some(e.id.to_string()),
                        title: format!(
                            "Link \"{}\" to \"{}\" by hand ({})",
                            e.name,
                            tname,
                            edge.relation.display_name()
                        ),
                        body: format!("\"{tname}\" is {reason}, so no reference could be generated."),
                    });
                }
            }
        }
        out
    }
}

/// Parse a raw HCL expression through hcl-rs's parser so it is still a typed tree.
/// Unparseable text becomes a bare variable-like identifier rather than a panic; the
/// catalog validator warns about `raw` sources, and `validate` will catch the rest.
pub(crate) fn raw_expr(s: &str) -> Expression {
    hcl::parse(&format!("x = {s}"))
        .ok()
        .and_then(|body| body.into_attributes().next().map(|a| a.expr))
        .unwrap_or_else(|| Expression::Variable(Variable::unchecked(s.trim())))
}

/// Convenience used by the GUI: entity id -> list of HCL addresses it would produce.
pub fn addresses_for(p: &Project, cat: &Catalog, provider: &str, id: &str) -> Vec<String> {
    let Some(e) = p.entity(id) else { return vec![] };
    let Some(m) = cat.mapping(e.resource_type, provider) else {
        return vec![];
    };
    let mut out: Vec<String> = m
        .data
        .iter()
        .map(|d| format!("data.{}.{}", d.resource, data_local_name(p, id, &d.key, None)))
        .collect();
    out.extend(m.blocks.iter().map(|b| {
        let local = local_name(p, id, &b.key);
        if repeated(b) {
            format!("{}.{}_N (one per row / target)", b.resource, local)
        } else {
            format!("{}.{}", b.resource, local)
        }
    }));
    out
}
