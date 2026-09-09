//! Catalog-aware diagnostics. The same function drives the on-canvas badges in the GUI
//! and the export gate: `Error` blocks export, `Warning` becomes a MANUAL_STEPS entry or
//! a badge, `Info` is shown in the inspector only.

use std::collections::HashSet;
use std::fmt;
use ttg_catalog::{
    ArgSource, BlockDef, Catalog, Condition, MappingStatus, NestedBlockDef, ProviderMapping, ResourceKind,
};
use ttg_core::{EntityRef, Id, Project, Record, Relation, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    Structural,
    UnknownType,
    KindMismatch,
    BadParent,
    MissingField,
    InvalidField,
    Manual,
    Unmapped,
    Partial,
    MissingAncestor,
    MissingRelation,
    TooManyRelations,
    BadRelationTarget,
    UnknownRelation,
    UnconsumedEdge,
    /// A `unique_scope` value is used by more than one entity.
    DuplicateName,
    /// Fewer relation targets than the definition's `min_targets`.
    TooFewRelations,
    /// `expects_incoming` type that nothing links to.
    Unreferenced,
    /// A definition-level `[[providers.<id>.checks]]` fired.
    Check,
    /// Built-in cross-resource network checks (routing, CIDRs, zones, placement).
    Network,
    /// An explicit link that containment already implies.
    Redundant,
    /// Parity: an entity that is not part of this provider's layer.
    Layer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub entity: Option<Id>,
    pub severity: Severity,
    pub code: Code,
    pub message: String,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sev = match self.severity {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Error => "error",
        };
        match &self.entity {
            Some(e) => write!(f, "{sev} [{e}] {}", self.message),
            None => write!(f, "{sev} {}", self.message),
        }
    }
}

/// A relation a mapping consumes, optionally restricted to one target type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Consumed {
    pub relation: String,
    pub target_type: Option<String>,
}

/// Run every catalog-aware check for the given target provider.
pub fn run(full: &Project, cat: &Catalog, provider: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let push = |out: &mut Vec<Diagnostic>, entity: Option<&str>, sev, code, msg: String| {
        out.push(Diagnostic {
            entity: entity.map(|s| s.to_string()),
            severity: sev,
            code,
            message: msg,
        })
    };
    // Parity report: what this provider's layer leaves out.
    let provider_name = cat
        .provider(provider)
        .map(|d| d.provider.display_name.clone())
        .unwrap_or(provider.to_string());
    let names = |ids: &[String]| {
        ids.iter()
            .map(|x| {
                cat.provider(x)
                    .map(|d| d.provider.display_name.clone())
                    .unwrap_or(x.clone())
            })
            .collect::<Vec<_>>()
            .join(" / ")
    };
    for (id, why) in crate::layers::off_layer(full, cat, provider) {
        let e = full.entity(&id).unwrap();
        let display = cat
            .resource(e.resource_type)
            .map(|d| d.resource.display_name.clone())
            .unwrap_or(e.resource_type.to_string());
        match why {
            crate::layers::OffReason::Tagged(tags) => push(
                &mut out,
                Some(&id),
                Severity::Info,
                Code::Layer,
                format!("{display} is tagged {} only; the {provider_name} export leaves it out", names(&tags)),
            ),
            crate::layers::OffReason::NoCounterpart(scope) if e.is_container => push(
                &mut out,
                Some(&id),
                Severity::Info,
                Code::Layer,
                format!(
                    "{display} exists only on {}; on {provider_name} it is just grouping and its contents are exported as if they sat in the enclosing container",
                    names(&scope)
                ),
            ),
            crate::layers::OffReason::NoCounterpart(scope) => push(
                &mut out,
                Some(&id),
                Severity::Warning,
                Code::Layer,
                format!(
                    "{display} exists only on {}; there is no {provider_name} counterpart, so the {provider_name} export leaves it out (links to it are dropped)",
                    names(&scope)
                ),
            ),
            crate::layers::OffReason::InsideOffLayerContainer => {}
        }
    }
    let layer = crate::layers::project_for(full, cat, provider);
    let p = &layer;

    let structural = ttg_core::validate::structural(p);
    for e in structural.errors {
        push(
            &mut out,
            e.entity.as_deref(),
            Severity::Error,
            Code::Structural,
            e.message,
        );
    }

    // (scope, value) -> entities using it, for `unique_scope` fields.
    let mut uniques: std::collections::BTreeMap<(String, String), Vec<(Id, String)>> =
        std::collections::BTreeMap::new();

    let pdef = cat.provider(provider);
    let provider_name = pdef
        .map(|d| d.provider.display_name.clone())
        .unwrap_or(provider.to_string());

    for e in p.entities() {
        let Some(def) = cat.resource(e.resource_type) else {
            push(
                &mut out,
                Some(e.id),
                Severity::Error,
                Code::UnknownType,
                format!("unknown resource type '{}'", e.resource_type),
            );
            continue;
        };
        let is_container_def = def.resource.kind == ResourceKind::Container;
        if is_container_def != e.is_container {
            push(
                &mut out,
                Some(e.id),
                Severity::Error,
                Code::KindMismatch,
                format!(
                    "'{}' is a {} type but is stored as a {}",
                    e.resource_type,
                    if is_container_def { "container" } else { "node" },
                    if e.is_container { "container" } else { "node" }
                ),
            );
        }

        // Parent allowed?
        if let Some(parent) = e.parent {
            if let Some(pc) = p.containers.get(parent) {
                if !def
                    .resource
                    .allowed_parents
                    .iter()
                    .any(|a| a == &pc.container_type)
                {
                    push(
                        &mut out,
                        Some(e.id),
                        Severity::Error,
                        Code::BadParent,
                        format!(
                            "{} cannot be placed inside a {}",
                            def.resource.display_name,
                            cat.resource(&pc.container_type)
                                .map(|d| d.resource.display_name.clone())
                                .unwrap_or(pc.container_type.clone())
                        ),
                    );
                }
            }
        }

        // Abstract fields.
        for f in &def.fields {
            if let Err(msg) = ttg_catalog::fields::check_value(f, e.field(&f.name)) {
                let code = if msg == "required" {
                    Code::MissingField
                } else {
                    Code::InvalidField
                };
                push(
                    &mut out,
                    Some(e.id),
                    Severity::Error,
                    code,
                    format!("{}: {}", f.label(), msg),
                );
            }
        }

        // entity_ref values must point at existing entities of an allowed type.
        for f in &def.fields {
            let refs: Vec<(String, Option<String>)> = match f.field_type {
                ttg_catalog::FieldType::EntityRef => e
                    .field(&f.name)
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| vec![(s.to_string(), None)])
                    .unwrap_or_default(),
                ttg_catalog::FieldType::StructList => e
                    .field(&f.name)
                    .and_then(|v| v.as_records())
                    .map(|rows| {
                        rows.iter()
                            .enumerate()
                            .flat_map(|(n, row)| {
                                f.items
                                    .iter()
                                    .filter(|sub| sub.field_type == ttg_catalog::FieldType::EntityRef)
                                    .filter_map(move |sub| {
                                        row.get(&sub.name)
                                            .and_then(|v| v.as_str())
                                            .filter(|s| !s.is_empty())
                                            .map(|s| (s.to_string(), Some(format!("row {}", n + 1))))
                                    })
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            let allowed: Vec<&str> = if f.field_type == ttg_catalog::FieldType::EntityRef {
                f.targets.iter().map(|s| s.as_str()).collect()
            } else {
                f.items
                    .iter()
                    .flat_map(|s| s.targets.iter().map(|t| t.as_str()))
                    .collect()
            };
            for (id, at) in refs {
                let where_ = at.map(|a| format!(" ({a})")).unwrap_or_default();
                match p.entity(&id) {
                    None => push(
                        &mut out,
                        Some(e.id),
                        Severity::Error,
                        Code::InvalidField,
                        format!(
                            "{}{where_}: references a resource that no longer exists",
                            f.label()
                        ),
                    ),
                    Some(t) if !allowed.contains(&t.resource_type) => push(
                        &mut out,
                        Some(e.id),
                        Severity::Error,
                        Code::InvalidField,
                        format!(
                            "{}{where_}: '{}' is a {} which is not allowed here",
                            f.label(),
                            t.name,
                            t.resource_type
                        ),
                    ),
                    _ => {}
                }
            }
        }

        // Types that expect something to link to them.
        if def.resource.expects_incoming && p.edges_to(e.id).next().is_none() {
            push(
                &mut out,
                Some(e.id),
                Severity::Warning,
                Code::Unreferenced,
                format!(
                    "nothing links to this {}; it will be created but unused",
                    def.resource.display_name
                ),
            );
        }

        if e.manual {
            push(
                &mut out,
                Some(e.id),
                Severity::Info,
                Code::Manual,
                "flagged as external/manual: not generated; references to it become input variables".into(),
            );
            continue;
        }

        let Some(m) = def.providers.get(provider) else {
            push(
                &mut out,
                Some(e.id),
                Severity::Warning,
                Code::Unmapped,
                format!(
                    "no {provider_name} mapping for {}: it will be listed in MANUAL_STEPS.md and references to it become variables",
                    def.resource.display_name
                ),
            );
            continue;
        };

        match m.status {
            MappingStatus::Partial => {
                let steps = applicable_manual_steps(p, cat, provider, &e, m);
                if !steps.is_empty() {
                    push(
                        &mut out,
                        Some(e.id),
                        Severity::Warning,
                        Code::Partial,
                        format!(
                            "partial mapping: {} manual step(s) after apply (see MANUAL_STEPS.md)",
                            steps.len()
                        ),
                    );
                }
            }
            MappingStatus::Logical => continue,
            MappingStatus::Full => {}
        }

        // Provider-specific fields.
        for f in &m.fields {
            if let Err(msg) = ttg_catalog::fields::check_value(f, e.provider_field(provider, &f.name)) {
                let code = if msg == "required" {
                    Code::MissingField
                } else {
                    Code::InvalidField
                };
                push(
                    &mut out,
                    Some(e.id),
                    Severity::Error,
                    code,
                    format!("{} ({}): {}", f.label(), provider, msg),
                );
            }
        }

        // Fields that are only required when a relation is absent.
        for f in def.fields.iter().chain(m.fields.iter()) {
            let Some(rel) = &f.required_unless_relation else {
                continue;
            };
            let value = e.field(&f.name).or_else(|| e.provider_field(provider, &f.name));
            let unset = value.is_none_or(|v| v.is_empty());
            let linked = Relation::from_key(rel)
                .map(|k| !relation_targets(p, cat, &e, k).is_empty())
                .unwrap_or(false);
            if unset && !linked {
                let rlabel = def
                    .relations
                    .iter()
                    .find(|r| &r.kind == rel)
                    .and_then(|r| r.label.clone())
                    .unwrap_or(rel.clone());
                push(
                    &mut out,
                    Some(e.id),
                    Severity::Error,
                    Code::MissingField,
                    format!("{}: required unless a '{rlabel}' link exists", f.label()),
                );
            }
        }

        // Unique-scope values (checked across entities after the loop).
        for f in def.fields.iter().chain(m.fields.iter()) {
            if let Some(scope) = &f.unique_scope {
                let value = e
                    .field(&f.name)
                    .or_else(|| e.provider_field(provider, &f.name))
                    .filter(|v| !v.is_empty())
                    .map(|v| v.display());
                if let Some(v) = value {
                    uniques
                        .entry((scope.clone(), v))
                        .or_default()
                        .push((e.id.to_string(), e.name.to_string()));
                }
            }
        }

        // Definition-level checks.
        for chk in &m.checks {
            let sev = if chk.severity == "error" {
                Severity::Error
            } else {
                Severity::Warning
            };
            match &chk.for_each_field {
                Some(field) => {
                    for (n, row) in field_rows(&e, provider, field).iter().enumerate() {
                        if condition_holds(p, cat, provider, &e, &chk.when, Some((row, n))) {
                            push(
                                &mut out,
                                Some(e.id),
                                sev,
                                Code::Check,
                                render_message(&chk.message, &e, Some(row)),
                            );
                        }
                    }
                }
                None => {
                    if condition_holds(p, cat, provider, &e, &chk.when, None) {
                        push(
                            &mut out,
                            Some(e.id),
                            sev,
                            Code::Check,
                            render_message(&chk.message, &e, None),
                        );
                    }
                }
            }
        }

        // Ancestors the mapping needs.
        let mut needed = required_ancestors(m);
        if let Some(req) = pdef.and_then(|d| d.required_ancestor.clone()) {
            if e.resource_type != req {
                needed.insert(req);
            }
        }
        for anc in needed {
            if p.ancestor_of_type(e.id, &anc).is_none() {
                let label = cat
                    .resource(&anc)
                    .map(|d| d.resource.display_name.clone())
                    .unwrap_or(anc.clone());
                push(
                    &mut out,
                    Some(e.id),
                    Severity::Error,
                    Code::MissingAncestor,
                    format!("{provider_name} requires this resource to be inside a {label} container"),
                );
            }
        }

        // Relations: cardinality and target types.
        for r in &def.relations {
            let Some(kind) = Relation::from_key(&r.kind) else {
                continue;
            };
            let targets = relation_targets(p, cat, &e, kind);
            let label = r.label.clone().unwrap_or(r.kind.clone());
            if let Some(min) = r.min_targets {
                if !targets.is_empty() && targets.len() < min {
                    push(
                        &mut out,
                        Some(e.id),
                        Severity::Warning,
                        Code::TooFewRelations,
                        format!(
                            "'{label}' has {} link(s); at least {min} are needed",
                            targets.len()
                        ),
                    );
                }
            }
            match r.cardinality {
                ttg_catalog::Cardinality::One if targets.is_empty() => push(
                    &mut out,
                    Some(e.id),
                    Severity::Error,
                    Code::MissingRelation,
                    format!(
                        "needs a '{label}' link to a {}{}",
                        r.targets
                            .iter()
                            .map(|t| cat
                                .resource(t)
                                .map(|d| d.resource.display_name.clone())
                                .unwrap_or(t.clone()))
                            .collect::<Vec<_>>()
                            .join(" / "),
                        if r.via_parent {
                            " (draw it inside one, or connect an edge)"
                        } else {
                            " (connect an edge)"
                        }
                    ),
                ),
                ttg_catalog::Cardinality::One | ttg_catalog::Cardinality::Optional if targets.len() > 1 => {
                    push(
                        &mut out,
                        Some(e.id),
                        Severity::Warning,
                        Code::TooManyRelations,
                        format!("more than one '{label}' link; only the first is used"),
                    )
                }
                _ => {}
            }
            for edge in p.edges_from(e.id).filter(|x| x.relation == kind) {
                if let Some(t) = p.entity(&edge.target) {
                    if !r.targets.iter().any(|x| x == t.resource_type) {
                        push(
                            &mut out,
                            Some(e.id),
                            Severity::Warning,
                            Code::BadRelationTarget,
                            format!(
                                "'{}' link to '{}' ({}) is not a declared target; it will only produce depends_on",
                                r.kind, t.name, t.resource_type
                            ),
                        );
                    }
                }
            }
        }
        // Edges whose relation kind the type does not declare.
        for edge in p.edges_from(e.id) {
            if edge.relation == Relation::DependsOn {
                continue;
            }
            if !def.relations.iter().any(|r| r.kind == edge.relation.key()) {
                push(
                    &mut out,
                    Some(e.id),
                    Severity::Warning,
                    Code::UnknownRelation,
                    format!(
                        "'{}' is not a relation {} declares; the edge will only produce depends_on",
                        edge.relation.display_name(),
                        def.resource.display_name
                    ),
                );
            }
        }
        // Edges the mapping does not consume (by kind, and by target type when filtered).
        let consumed = consumed_relations(m);
        for edge in p.edges_from(e.id) {
            if edge.relation == Relation::DependsOn {
                continue;
            }
            if !def.relations.iter().any(|r| r.kind == edge.relation.key()) {
                continue; // already reported above
            }
            let Some(t) = p.entity(&edge.target) else {
                continue;
            };
            let is_consumed = consumed.iter().any(|c| {
                c.relation == edge.relation.key()
                    && c.target_type.as_deref().is_none_or(|tt| tt == t.resource_type)
            });
            if is_consumed {
                continue;
            }
            push(
                &mut out,
                Some(e.id),
                Severity::Warning,
                Code::UnconsumedEdge,
                format!(
                    "the {provider} mapping cannot express '{}' -> '{}'; it becomes depends_on plus a manual step",
                    edge.relation.display_name(),
                    t.name
                ),
            );
        }
    }
    network_checks(p, cat, provider, &mut out);

    for e in &p.edges {
        if is_redundant_edge(p, cat, e) {
            let tname = p
                .entity(&e.target)
                .map(|t| t.name.to_string())
                .unwrap_or_default();
            push(
                &mut out,
                Some(&e.source),
                Severity::Info,
                Code::Redundant,
                format!(
                    "the '{}' link to \"{tname}\" is implied by being drawn inside it; the explicit link is ignored",
                    e.relation.display_name()
                ),
            );
        }
    }

    for ((scope, value), users) in uniques {
        if users.len() > 1 {
            let names: Vec<String> = users.iter().map(|(_, n)| format!("\"{n}\"")).collect();
            for (id, _) in &users {
                push(
                    &mut out,
                    Some(id),
                    Severity::Error,
                    Code::DuplicateName,
                    format!(
                        "{} name '{value}' is also used by {}; {provider_name} names in this scope must be unique",
                        scope.replace('_', " "),
                        names.iter().filter(|n| n != &&format!("\"{}\"", users.iter().find(|(i, _)| i == id).map(|(_, n)| n.clone()).unwrap_or_default())).cloned().collect::<Vec<_>>().join(", ")
                    ),
                );
            }
        }
    }
    out.sort_by(|a, b| b.severity.cmp(&a.severity));
    out
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Int(i)) => *i != 0,
        Some(Value::Float(f)) => *f != 0.0,
        Some(Value::Str(s)) => !s.trim().is_empty() && s != "false",
        Some(Value::List(l)) => !l.is_empty(),
        Some(Value::Records(r)) => !r.is_empty(),
    }
}

fn cond_value(v: Option<&Value>, equals: &Option<String>, not_equals: &Option<String>) -> bool {
    let s = v.map(|v| v.display()).unwrap_or_default();
    match (equals, not_equals) {
        (Some(eq), _) => &s == eq,
        (None, Some(ne)) => &s != ne,
        (None, None) => truthy(v),
    }
}

/// A field's value, or its declared default when the entity has no value for it, so that
/// conditions behave the same for a freshly-added node and one whose defaults were saved.
pub fn field_or_default(
    cat: &Catalog,
    provider: &str,
    e: &EntityRef,
    name: &str,
    provider_field: bool,
) -> Option<Value> {
    let current = if provider_field {
        e.provider_field(provider, name)
    } else {
        e.field(name)
    };
    if let Some(v) = current {
        if !v.is_empty() {
            return Some(v.clone());
        }
    }
    let def = cat.resource(e.resource_type)?;
    let fields = if provider_field {
        &def.providers.get(provider)?.fields
    } else {
        &def.fields
    };
    fields.iter().find(|f| f.name == name)?.default_value()
}

/// Targets of a relation restricted to one abstract type.
pub fn relation_targets_of_type(
    p: &Project,
    cat: &Catalog,
    e: &EntityRef,
    kind: Relation,
    target_type: Option<&str>,
) -> Vec<Id> {
    relation_targets(p, cat, e, kind)
        .into_iter()
        .filter(|t| target_type.is_none_or(|tt| p.entity(t).is_some_and(|x| x.resource_type == tt)))
        .collect()
}

/// Evaluate a definition condition for an entity. `item` is the current row of a
/// repeated block (record and index), when there is one.
pub fn condition_holds(
    p: &Project,
    cat: &Catalog,
    provider: &str,
    e: &EntityRef,
    c: &Condition,
    item: Option<(&Record, usize)>,
) -> bool {
    match c {
        Condition::Relation(r) => {
            let present = Relation::from_key(&r.relation)
                .map(|k| !relation_targets_of_type(p, cat, e, k, r.target_type.as_deref()).is_empty())
                .unwrap_or(false);
            present != r.absent
        }
        Condition::Field(f) => {
            if f.absent {
                return e.field(&f.field).is_none_or(|v| v.is_empty());
            }
            let v = field_or_default(cat, provider, e, &f.field, false);
            cond_value(v.as_ref(), &f.equals, &f.not_equals)
        }
        Condition::ProviderField(f) => {
            if f.absent {
                return e
                    .provider_field(provider, &f.provider_field)
                    .is_none_or(|v| v.is_empty());
            }
            let v = field_or_default(cat, provider, e, &f.provider_field, true);
            cond_value(v.as_ref(), &f.equals, &f.not_equals)
        }
        Condition::Ancestor(a) => p.ancestor_of_type(e.id, &a.ancestor).is_some() != a.absent,
        Condition::Item(i) => {
            let Some((record, _)) = item else {
                return false;
            };
            if let Some(other) = &i.equals_item {
                let a = record.get(&i.item).map(|v| v.display());
                let b = record.get(other).map(|v| v.display());
                return a.is_some() && a == b;
            }
            cond_value(record.get(&i.item), &i.equals, &i.not_equals)
        }
        Condition::All(a) => a
            .all
            .iter()
            .all(|c| condition_holds(p, cat, provider, e, c, item)),
        Condition::Any(a) => a
            .any
            .iter()
            .any(|c| condition_holds(p, cat, provider, e, c, item)),
    }
}

/// Manual steps of a mapping whose `when` holds for this entity.
pub fn applicable_manual_steps<'m>(
    p: &Project,
    cat: &Catalog,
    provider: &str,
    e: &EntityRef,
    m: &'m ProviderMapping,
) -> Vec<&'m ttg_catalog::ManualStep> {
    m.manual_steps
        .iter()
        .filter(|s| {
            s.when
                .as_ref()
                .is_none_or(|c| condition_holds(p, cat, provider, e, c, None))
        })
        .collect()
}

/// Rows of a field for check evaluation (struct_list rows or string_list entries).
fn field_rows(e: &EntityRef, provider: &str, field: &str) -> Vec<Record> {
    match e.field(field).or_else(|| e.provider_field(provider, field)) {
        Some(Value::Records(rows)) => rows.clone(),
        Some(Value::List(items)) => items
            .iter()
            .map(|s| {
                let mut r = Record::new();
                r.insert("value".into(), Value::Str(s.clone()));
                r
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn render_message(msg: &str, e: &EntityRef, item: Option<&Record>) -> String {
    let mut out = msg.replace("{name}", e.name);
    if let Some(r) = item {
        for (k, v) in r {
            out = out.replace(&format!("{{item.{k}}}"), &v.display());
        }
    }
    out
}

/// True when an explicit edge says nothing containment does not already say: a
/// `via_parent` relation whose target is an enclosing container of the source.
pub fn is_redundant_edge(p: &Project, cat: &Catalog, e: &ttg_core::Edge) -> bool {
    let Some(src) = p.entity(&e.source) else {
        return false;
    };
    let Some(r) = cat.relation_def(src.resource_type, e.relation.key()) else {
        return false;
    };
    r.via_parent
        && p.is_ancestor(&e.target, &e.source)
        && p.entity(&e.target)
            .is_some_and(|t| r.targets.iter().any(|x| x == t.resource_type))
}

/// Targets of a relation for an entity: explicit edges first, then (if the relation is
/// `via_parent`) the nearest enclosing container of an allowed type.
pub fn relation_targets(p: &Project, cat: &Catalog, e: &EntityRef, kind: Relation) -> Vec<Id> {
    let mut out: Vec<Id> = p
        .edges_from(e.id)
        .filter(|x| x.relation == kind)
        .map(|x| x.target.clone())
        .collect();
    if out.is_empty() {
        if let Some(r) = cat.relation_def(e.resource_type, kind.key()) {
            if r.via_parent {
                if let Some(c) = p
                    .ancestors(e.id)
                    .into_iter()
                    .find(|c| r.targets.iter().any(|t| t == &c.container_type))
                {
                    out.push(c.id.clone());
                }
            }
        }
    }
    out
}

/// Relations referenced anywhere in a mapping (args, conditions, nested and data blocks).
pub fn consumed_relations(m: &ProviderMapping) -> Vec<Consumed> {
    let mut rel = HashSet::new();
    let mut anc = HashSet::new();
    for b in m.blocks.iter().chain(m.data.iter()) {
        scan_block(b, &mut rel, &mut anc);
    }
    rel.into_iter().collect()
}

/// Container types referenced by non-optional `ancestor` sources in a mapping.
pub fn required_ancestors(m: &ProviderMapping) -> HashSet<String> {
    let mut rel = HashSet::new();
    let mut anc = HashSet::new();
    for b in m.blocks.iter().chain(m.data.iter()) {
        scan_block(b, &mut rel, &mut anc);
    }
    anc
}

fn scan_block(b: &BlockDef, rel: &mut HashSet<Consumed>, anc: &mut HashSet<String>) {
    if let Some(r) = &b.for_each_relation {
        rel.insert(Consumed {
            relation: r.clone(),
            target_type: b.for_each_target_type.clone(),
        });
    }
    if let Some(c) = &b.when {
        scan_cond(c, rel);
    }
    for s in b.args.values() {
        scan_source(s, rel, anc);
    }
    for n in &b.nested {
        scan_nested(n, rel, anc);
    }
}

fn scan_nested(n: &NestedBlockDef, rel: &mut HashSet<Consumed>, anc: &mut HashSet<String>) {
    if let Some(r) = &n.for_each_relation {
        rel.insert(Consumed {
            relation: r.clone(),
            target_type: n.for_each_target_type.clone(),
        });
    }
    if let Some(c) = &n.when {
        scan_cond(c, rel);
    }
    for s in n.args.values() {
        scan_source(s, rel, anc);
    }
    for inner in &n.nested {
        scan_nested(inner, rel, anc);
    }
}

fn scan_cond(c: &Condition, rel: &mut HashSet<Consumed>) {
    match c {
        Condition::Relation(r) => {
            rel.insert(Consumed {
                relation: r.relation.clone(),
                target_type: r.target_type.clone(),
            });
        }
        Condition::All(a) => a.all.iter().for_each(|x| scan_cond(x, rel)),
        Condition::Any(a) => a.any.iter().for_each(|x| scan_cond(x, rel)),
        _ => {}
    }
}

fn scan_source(s: &ArgSource, rel: &mut HashSet<Consumed>, anc: &mut HashSet<String>) {
    match s {
        ArgSource::Relation(r) => {
            rel.insert(Consumed {
                relation: r.relation.clone(),
                target_type: r.target_type.clone(),
            });
            if let Some(fb) = &r.fallback {
                scan_source(fb, rel, anc);
            }
        }
        ArgSource::Ancestor(a) => {
            if !a.optional {
                anc.insert(a.ancestor.clone());
            }
        }
        ArgSource::Field(f) => {
            if let Some(fb) = &f.fallback {
                scan_source(fb, rel, anc);
            }
        }
        ArgSource::ProviderField(f) => {
            if let Some(fb) = &f.fallback {
                scan_source(fb, rel, anc);
            }
        }
        ArgSource::If(i) => {
            scan_cond(&i.cond, rel);
            scan_source(&i.then, rel, anc);
            if let Some(o) = &i.otherwise {
                scan_source(o, rel, anc);
            }
        }
        ArgSource::Object(o) => o.object.values().for_each(|x| scan_source(x, rel, anc)),
        ArgSource::List(l) => l.list.iter().for_each(|x| scan_source(x, rel, anc)),
        ArgSource::Func(f) => f.args.iter().for_each(|x| scan_source(x, rel, anc)),
        _ => {}
    }
}

/// Parse an IPv4 CIDR into (network, mask).
fn parse_cidr(s: &str) -> Option<(u32, u32)> {
    let (addr, len) = s.split_once('/')?;
    let len: u32 = len.parse().ok()?;
    if len > 32 {
        return None;
    }
    let ip: std::net::Ipv4Addr = addr.parse().ok()?;
    let mask = if len == 0 { 0 } else { u32::MAX << (32 - len) };
    Some((u32::from(ip) & mask, mask))
}

fn cidrs_overlap(a: &str, b: &str) -> bool {
    match (parse_cidr(a), parse_cidr(b)) {
        (Some((na, ma)), Some((nb, mb))) => {
            let m = ma & mb;
            (na & m) == (nb & m)
        }
        _ => false,
    }
}

fn cidr_within(inner: &str, outer: &str) -> bool {
    match (parse_cidr(inner), parse_cidr(outer)) {
        (Some((ni, mi)), Some((no, mo))) => mi >= mo && (ni & mo) == no,
        _ => true,
    }
}

/// Built-in checks that need to look at several resources at once. These know about
/// the network-shaped abstract types by name (subnet, route_table, gateways, function);
/// everything else about those types still comes from their definitions.
fn network_checks(p: &Project, cat: &Catalog, provider: &str, out: &mut Vec<Diagnostic>) {
    let mut push = |entity: &str, sev: Severity, msg: String| {
        out.push(Diagnostic {
            entity: Some(entity.to_string()),
            severity: sev,
            code: Code::Network,
            message: msg,
        })
    };
    let entities = p.entities();
    let name_of = |id: &str| p.entity(id).map(|e| e.name.to_string()).unwrap_or(id.to_string());

    // 1. Managed services drawn inside a network: no effect, so say so once.
    for e in &entities {
        let Some(def) = cat.resource(e.resource_type) else {
            continue;
        };
        if def.resource.network_agnostic {
            if let Some(parent) = e.parent {
                if p.containers
                    .get(parent)
                    .is_some_and(|c| c.container_type == "virtual_network")
                {
                    push(
                        e.id,
                        Severity::Info,
                        format!(
                            "{} is a managed service with no network presence; drawing it inside a Virtual Network changes nothing",
                            def.resource.display_name
                        ),
                    );
                }
            }
        }
    }

    // 2. Subnet CIDRs: inside their network, and not overlapping each other.
    let subnets: Vec<&EntityRef> = entities.iter().filter(|e| e.resource_type == "subnet").collect();
    for s in &subnets {
        let Some(cidr) = s.field("cidr_block").and_then(|v| v.as_str()) else {
            continue;
        };
        let vnet = relation_targets(p, cat, s, Relation::NetworkMembership)
            .into_iter()
            .next();
        if let Some(vid) = &vnet {
            if let Some(vcidr) = p
                .entity(vid)
                .and_then(|v| v.field("cidr_block"))
                .and_then(|v| v.as_str())
            {
                if !cidr_within(cidr, vcidr) {
                    push(
                        s.id,
                        Severity::Error,
                        format!("{cidr} is not inside the network address space {vcidr}"),
                    );
                }
            }
        }
        for o in &subnets {
            if o.id <= s.id {
                continue;
            }
            let same_net = vnet.is_some()
                && relation_targets(p, cat, o, Relation::NetworkMembership)
                    .into_iter()
                    .next()
                    == vnet;
            let Some(ocidr) = o.field("cidr_block").and_then(|v| v.as_str()) else {
                continue;
            };
            if same_net && cidrs_overlap(cidr, ocidr) {
                push(
                    s.id,
                    Severity::Error,
                    format!("{cidr} overlaps with subnet \"{}\" ({ocidr})", o.name),
                );
                push(
                    o.id,
                    Severity::Error,
                    format!("{ocidr} overlaps with subnet \"{}\" ({cidr})", s.name),
                );
            }
        }
    }

    // 3. AWS availability zones must belong to the configured region.
    if provider == "aws" {
        let region = p
            .settings
            .provider_settings
            .get("aws")
            .and_then(|m| m.get("region"))
            .cloned()
            .unwrap_or_default();
        if !region.is_empty() {
            for e in &entities {
                if let Some(az) = e
                    .provider_field("aws", "availability_zone")
                    .and_then(|v| v.as_str())
                {
                    if !az.is_empty() && !az.starts_with(&region) {
                        push(
                            e.id,
                            Severity::Error,
                            format!("availability zone {az} is not in region {region}"),
                        );
                    }
                }
            }
        }
    }

    // 4. Route tables: at most one per subnet; which subnets have an outbound route.
    let route_tables: Vec<&EntityRef> = entities
        .iter()
        .filter(|e| e.resource_type == "route_table")
        .collect();
    let mut tables_per_subnet: std::collections::BTreeMap<Id, Vec<Id>> = Default::default();
    let mut egress_subnets: HashSet<Id> = HashSet::new();
    for rt in &route_tables {
        let via_gateway = relation_targets(p, cat, rt, Relation::AttributeReference)
            .iter()
            .any(|t| {
                p.entity(t)
                    .is_some_and(|x| matches!(x.resource_type, "nat_gateway" | "internet_gateway"))
            });
        for s in relation_targets(p, cat, rt, Relation::Attachment) {
            tables_per_subnet
                .entry(s.clone())
                .or_default()
                .push(rt.id.to_string());
            if via_gateway {
                egress_subnets.insert(s);
            }
        }
    }
    for (subnet, tables) in &tables_per_subnet {
        if tables.len() > 1 {
            let names: Vec<String> = tables.iter().map(|t| format!("\"{}\"", name_of(t))).collect();
            push(
                subnet,
                Severity::Error,
                format!(
                    "attached to {} route tables ({}); a subnet can have only one",
                    tables.len(),
                    names.join(", ")
                ),
            );
        }
    }
    // A NAT gateway's own subnet must route to an internet gateway.
    for nat in entities.iter().filter(|e| e.resource_type == "nat_gateway") {
        for s in relation_targets(p, cat, nat, Relation::NetworkMembership) {
            let routed_to_igw = route_tables.iter().any(|rt| {
                relation_targets(p, cat, rt, Relation::Attachment).contains(&s)
                    && relation_targets(p, cat, rt, Relation::AttributeReference)
                        .iter()
                        .any(|t| p.entity(t).is_some_and(|x| x.resource_type == "internet_gateway"))
            });
            if !routed_to_igw {
                push(
                    nat.id,
                    Severity::Warning,
                    format!(
                        "its subnet \"{}\" has no route table with a default route via an Internet Gateway, so the NAT gateway cannot reach the internet",
                        name_of(&s)
                    ),
                );
            }
        }
    }
    // Functions in subnets need an outbound route to reach queues, secrets and storage.
    for f in entities.iter().filter(|e| e.resource_type == "function") {
        for s in relation_targets(p, cat, f, Relation::NetworkMembership) {
            if !egress_subnets.contains(&s) {
                push(
                    f.id,
                    Severity::Warning,
                    format!(
                        "runs in subnet \"{}\" which has no default route via a NAT or Internet Gateway; it will not reach queues, secrets or storage outside the network",
                        name_of(&s)
                    ),
                );
            }
        }
    }
}
