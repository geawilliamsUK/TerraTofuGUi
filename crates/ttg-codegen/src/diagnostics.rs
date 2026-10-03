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
    /// Extra / native arguments checked against the provider schema.
    Extra,
    /// Where the state lives and what protects it: the backend, state encryption, the
    /// bootstrap root, secrets that land in the state (`crate::state`).
    State,
    /// Provider version constraints against the bundled schema (`crate::versions`).
    Version,
    /// The cost estimate exceeds a budget's limit (`cost::budget_diagnostics`).
    Cost,
    /// Two arguments of one generated block that the provider refuses together at plan
    /// time, though `validate` passes (`crate::conflicts`).
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub entity: Option<Id>,
    pub severity: Severity,
    pub code: Code,
    pub message: String,
    /// `None` for the target provider's own diagnostics. [`run_all`] sets it to the
    /// provider an error came from when it reports another provider's errors as
    /// warnings; the message then carries the same provider as a `[Azure] ` prefix.
    pub provider: Option<String>,
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
            provider: None,
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
    // Entities a `severity = "omit"` check leaves out of this provider's export. They
    // are off the layer below, so the check is reported once, here, and nothing else in
    // this run sees them.
    let omitted = crate::layers::omitted(full, cat, provider);
    for (id, why) in crate::layers::off_layer_omitting(full, cat, provider, &omitted) {
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
            crate::layers::OffReason::Check(msg) => push(
                &mut out,
                Some(&id),
                Severity::Warning,
                Code::Check,
                format!("{msg}; left out of the {provider_name} export"),
            ),
            crate::layers::OffReason::InsideOffLayerContainer => {}
        }
    }
    let layer = crate::layers::project_for_omitting(
        full,
        cat,
        provider,
        &omitted.iter().map(|(id, _)| id.clone()).collect(),
    );
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
            // `omit` checks are reported by the parity loop above, on the entity they
            // took out of the layer; nothing on the layer can still be failing one.
            if chk.severity == "omit" {
                continue;
            }
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
            if Relation::from_key(&r.kind).is_none() {
                continue;
            }
            let targets = declared_relation_targets(p, cat, &e, def, r);
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
        }
        // Edges whose relation kind the type does not declare, or whose target type no
        // declaration of that kind allows. Judged against every declaration of the kind at
        // once, so a type that splits one kind over several declarations stays quiet.
        for edge in p.edges_from(e.id) {
            if edge.relation == Relation::DependsOn {
                continue;
            }
            let declared: Vec<&ttg_catalog::RelationDef> = def
                .relations
                .iter()
                .filter(|r| r.kind == edge.relation.key())
                .collect();
            if declared.is_empty() {
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
                continue;
            }
            let Some(t) = p.entity(&edge.target) else {
                continue;
            };
            if !declared
                .iter()
                .any(|r| r.targets.iter().any(|x| x == t.resource_type))
            {
                push(
                    &mut out,
                    Some(e.id),
                    Severity::Warning,
                    Code::BadRelationTarget,
                    format!(
                        "'{}' link to '{}' ({}) is not a declared target; it will only produce depends_on",
                        edge.relation.key(),
                        t.name,
                        t.resource_type
                    ),
                );
            }
        }
        // Edges the mapping does not consume (by kind, and by target type when filtered).
        let consumed = consumed_relations(m);
        for edge in p.edges_from(e.id) {
            // `calls` is documentation only: no mapping is expected to express it, so
            // "cannot express" would be noise on every service-to-service edge.
            if edge.relation == Relation::DependsOn || edge.relation == Relation::Calls {
                continue;
            }
            if !def.relations.iter().any(|r| r.kind == edge.relation.key()) {
                continue; // already reported above
            }
            let Some(t) = p.entity(&edge.target) else {
                continue;
            };
            if manifests_only(cat, e.resource_type, edge.relation, t.resource_type) {
                continue;
            }
            // Relations declared for other providers only are silently ignored here.
            let scoped_elsewhere = def.relations.iter().any(|r| {
                r.kind == edge.relation.key()
                    && r.targets.iter().any(|x| x == t.resource_type)
                    && !r.providers.is_empty()
                    && !r.providers.iter().any(|x| x == provider)
            });
            if scoped_elsewhere {
                continue;
            }
            let is_consumed = consumed.iter().any(|c| {
                c.relation == edge.relation.key()
                    && c.target_type.as_deref().is_none_or(|tt| tt == t.resource_type)
            });
            if is_consumed || read_by_target(cat, provider, edge.relation, e.resource_type, t.resource_type) {
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
    crate::edge::checks(p, cat, provider, &mut out);
    extra_checks(p, cat, provider, &mut out);
    out.extend(crate::state::checks(full, p, cat, provider, full.settings.tool));
    out.extend(crate::versions::checks(p, cat, provider));
    // A budget the cost estimate exceeds. Cheap enough to run with the rest (it is
    // skipped outright without a budget entity); see `cost::budget_diagnostics`.
    out.extend(crate::cost::budget_diagnostics(p, cat, provider));

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
    // Arguments the provider refuses together at plan time. The check emits the project,
    // so it only runs once nothing else stops the export.
    if !out.iter().any(|d| d.severity == Severity::Error) {
        out.extend(crate::conflicts::check(full, cat, provider));
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.severity));
    out
}

/// The `omit` checks that fire on `p`, which must already be the provider's layer:
/// `(entity, rendered message)`. Feeds [`crate::layers::omitted`], which takes those
/// entities out of the layer; nothing else calls this.
pub fn omit_checks(p: &Project, cat: &Catalog, provider: &str) -> Vec<(Id, String)> {
    let mut out = Vec::new();
    for e in p.entities() {
        if e.manual {
            continue;
        }
        let Some(m) = cat.mapping(e.resource_type, provider) else {
            continue;
        };
        for chk in m.checks.iter().filter(|c| c.severity == "omit") {
            let fired = match &chk.for_each_field {
                Some(field) => {
                    let rows = field_rows(&e, provider, field);
                    rows.iter()
                        .enumerate()
                        .find(|(n, row)| condition_holds(p, cat, provider, &e, &chk.when, Some((row, *n))))
                        .map(|(_, row)| render_message(&chk.message, &e, Some(row)))
                }
                None => condition_holds(p, cat, provider, &e, &chk.when, None)
                    .then(|| render_message(&chk.message, &e, None)),
            };
            if let Some(msg) = fired {
                out.push((e.id.to_string(), msg));
                break;
            }
        }
    }
    out
}

/// [`run`] for the target provider, plus what the *other* providers in the catalog would
/// refuse: each of their errors, downgraded to a warning, tagged with the provider it
/// came from and prefixed `[<Provider>] `. Exports still block on the target provider's
/// own errors only — this is the "you will hit this when you switch" list.
pub fn run_all(full: &Project, cat: &Catalog, provider: &str) -> Vec<Diagnostic> {
    let mut out = run(full, cat, provider);
    out.extend(other_providers(full, cat, provider));
    out
}

/// The other-provider half of [`run_all`], on its own, for callers that keep the two
/// lists apart (the app's panel, the MCP `other_providers` key).
///
/// Two kinds of entry, both tagged with the provider they are about and prefixed
/// `[<Provider>] `: each of that provider's *errors*, as a warning ("would block the
/// export"), and, as **info**, each entity a `severity = "omit"` check leaves out of
/// that provider's export. The second kind used to be invisible while the target was
/// another provider — an alarm with no metric on Azure vanished from the Azure layer
/// and only `left_out` in the project summary said so. It is only information, since
/// nothing blocks; but a design that quietly loses a resource on the provider you
/// switch to next should say so before you switch. The target provider is skipped
/// here, so its own omit warning (from [`run`]) is never listed twice.
pub fn other_providers(full: &Project, cat: &Catalog, provider: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for pid in cat.provider_ids() {
        if pid == provider {
            continue;
        }
        let name = cat
            .provider(&pid)
            .map(|d| d.provider.display_name.clone())
            .unwrap_or_else(|| pid.clone());
        // `run` works on that provider's own layer, so an entity tagged for other
        // providers (or omitted there) never reaches this list in the first place.
        for d in run(full, cat, &pid) {
            if d.severity != Severity::Error {
                continue;
            }
            out.push(Diagnostic {
                entity: d.entity,
                severity: Severity::Warning,
                code: d.code,
                message: format!("[{name}] {} (would block the {name} export)", d.message),
                provider: Some(pid.clone()),
            });
        }
        for (id, why) in crate::layers::omitted(full, cat, &pid) {
            out.push(Diagnostic {
                entity: Some(id),
                severity: Severity::Info,
                code: Code::Check,
                message: format!("[{name}] {why}; left out of the {name} export"),
                provider: Some(pid.clone()),
            });
        }
    }
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
    // A list-valued field (a cluster's add-ons) is compared entry by entry, so `equals`
    // reads as "contains" and `not_equals` as "does not contain". A one-entry list
    // displays as its single entry, so this only widens what used to be unmatchable.
    let holds = |want: &str| match v {
        Some(Value::List(items)) => items.iter().any(|x| x == want),
        other => other.map(|v| v.display()).unwrap_or_default() == want,
    };
    match (equals, not_equals) {
        (Some(eq), _) => holds(eq),
        (None, Some(ne)) => !holds(ne),
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
    if !provider_field && name == "name" {
        // `EntityRef::field("name")` deliberately returns `None` (it is not a config
        // entry); resolve it here the same way argument sources and templates do.
        return Some(Value::Str(e.name.to_string()));
    }
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
    targets_where(p, cat, e, kind, &|ty| target_type.is_none_or(|tt| tt == ty))
}

/// Entities that link *to* `e` with this relation, optionally only those of one abstract
/// type. Containment never stands in for an incoming edge, so `via_parent` does not apply.
pub fn relation_sources_of_type(
    p: &Project,
    e: &EntityRef,
    kind: Relation,
    source_type: Option<&str>,
) -> Vec<Id> {
    p.edges_to(e.id)
        .filter(|x| x.relation == kind)
        .map(|x| x.source.clone())
        .filter(|s| source_type.is_none_or(|st| p.entity(s).is_some_and(|x| x.resource_type == st)))
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
    condition_holds_for(p, cat, provider, e, c, item, None)
}

/// [`condition_holds`] with the current relation target of a repeated block, so
/// `target_shares_ancestor` conditions can be decided.
pub fn condition_holds_for(
    p: &Project,
    cat: &Catalog,
    provider: &str,
    e: &EntityRef,
    c: &Condition,
    item: Option<(&Record, usize)>,
    target: Option<&str>,
) -> bool {
    match c {
        Condition::Relation(r) => {
            let targets = Relation::from_key(&r.relation)
                .map(|k| {
                    if r.incoming {
                        relation_sources_of_type(p, e, k, r.target_type.as_deref())
                    } else {
                        relation_targets_of_type(p, cat, e, k, r.target_type.as_deref())
                    }
                })
                .unwrap_or_default();
            let targets = match &r.hop {
                Some(h) => crate::edge::hop(p, cat, &targets, h),
                None => targets,
            };
            let matching = targets
                .iter()
                .filter(|t| {
                    let Some(te) = p.entity(t) else { return false };
                    // `where` asks the entity at the other end a question of its own.
                    if r.where_
                        .as_ref()
                        .is_some_and(|w| !condition_holds(p, cat, provider, &te, w, None))
                    {
                        return false;
                    }
                    let v = if let Some(f) = &r.target_field {
                        field_or_default(cat, provider, &te, f, false)
                    } else if let Some(f) = &r.target_provider_field {
                        field_or_default(cat, provider, &te, f, true)
                    } else {
                        return true;
                    };
                    cond_value(v.as_ref(), &r.equals, &r.not_equals)
                })
                .count();
            (matching >= r.min_count.unwrap_or(1)) != r.absent
        }
        Condition::Target(t) => {
            // Without `relation` the subject is the current for_each_relation row; with
            // it, every target of that relation ("is my dead-letter queue in my namespace?").
            let subjects: Vec<Id> = match &t.relation {
                Some(rel) => Relation::from_key(rel)
                    .map(|k| relation_targets_of_type(p, cat, e, k, t.target_type.as_deref()))
                    .unwrap_or_default(),
                None => match target {
                    Some(tid) => vec![tid.to_string()],
                    None => return false,
                },
            };
            let mine = p
                .ancestor_of_type(e.id, &t.target_shares_ancestor)
                .map(|c| c.id.as_str());
            let same = mine.is_some()
                && subjects.iter().any(|tid| {
                    p.ancestor_of_type(tid, &t.target_shares_ancestor)
                        .map(|c| c.id.as_str())
                        == mine
                });
            same != t.absent
        }
        Condition::Field(f) => {
            if f.absent {
                return e.field(&f.field).is_none_or(|v| v.is_empty());
            }
            let v = field_or_default(cat, provider, e, &f.field, false);
            let v = match f.transform {
                Some(t) => v.map(|v| Value::Str(t.apply(&v.display()))),
                None => v,
            };
            // Equality wins when given; otherwise a prefix or suffix test; otherwise
            // truthiness.
            let prefix = f.starts_with.as_deref().or(f.not_starts_with.as_deref());
            let suffix = f.ends_with.as_deref().or(f.not_ends_with.as_deref());
            if f.equals.is_some() || f.not_equals.is_some() {
                return cond_value(v.as_ref(), &f.equals, &f.not_equals);
            }
            let cmp = f.compare();
            if !cmp.is_empty() {
                return compare_holds(cat, provider, e, v.as_ref(), cmp);
            }
            if let Some(p) = prefix {
                let s = v.as_ref().map(|v| v.display()).unwrap_or_default();
                return s.starts_with(p) == f.starts_with.is_some();
            }
            if let Some(p) = suffix {
                let s = v.as_ref().map(|v| v.display()).unwrap_or_default();
                return s.ends_with(p) == f.ends_with.is_some();
            }
            cond_value(v.as_ref(), &f.equals, &f.not_equals)
        }
        Condition::ProviderField(f) => {
            if f.absent {
                return e
                    .provider_field(provider, &f.provider_field)
                    .is_none_or(|v| v.is_empty());
            }
            let v = field_or_default(cat, provider, e, &f.provider_field, true);
            let cmp = f.compare();
            if f.equals.is_none() && f.not_equals.is_none() && !cmp.is_empty() {
                return compare_holds(cat, provider, e, v.as_ref(), cmp);
            }
            if f.equals.is_none() && f.not_equals.is_none() {
                if let Some(p) = f.ends_with.as_deref().or(f.not_ends_with.as_deref()) {
                    let s = v.as_ref().map(|v| v.display()).unwrap_or_default();
                    return s.ends_with(p) == f.ends_with.is_some();
                }
            }
            cond_value(v.as_ref(), &f.equals, &f.not_equals)
        }
        Condition::Ancestor(a) => p.ancestor_of_type(e.id, &a.ancestor).is_some() != a.absent,
        Condition::Setting(s) => cond_value(setting_value(p, &s.setting).as_ref(), &s.equals, &s.not_equals),
        Condition::Item(i) => {
            let Some((record, _)) = item else {
                return false;
            };
            let holds = if let Some(ty) = &i.ref_type {
                record
                    .get(&i.item)
                    .and_then(|v| v.as_str())
                    .and_then(|id| p.entity(id))
                    .is_some_and(|t| t.resource_type == ty)
            } else if let Some(rel) = &i.linked {
                let id = record.get(&i.item).and_then(|v| v.as_str()).unwrap_or_default();
                !id.is_empty()
                    && Relation::from_key(rel)
                        .is_some_and(|k| relation_targets(p, cat, e, k).iter().any(|t| t == id))
            } else if let Some(other) = &i.equals_item {
                let a = record.get(&i.item).map(|v| v.display());
                let b = record.get(other).map(|v| v.display());
                a.is_some() && a == b
            } else if let Some(n) = i.min_count {
                let count = match record.get(&i.item) {
                    Some(Value::List(l)) => l.len(),
                    Some(Value::Records(r)) => r.len(),
                    Some(v) if !v.is_empty() => 1,
                    _ => 0,
                };
                count >= n
            } else {
                cond_value(record.get(&i.item), &i.equals, &i.not_equals)
            };
            holds != i.absent
        }
        Condition::Not(n) => !condition_holds_for(p, cat, provider, e, &n.not, item, target),
        Condition::All(a) => a
            .all
            .iter()
            .all(|c| condition_holds_for(p, cat, provider, e, c, item, target)),
        Condition::Any(a) => a
            .any
            .iter()
            .any(|c| condition_holds_for(p, cat, provider, e, c, item, target)),
    }
}

/// `one_of` / `not_one_of` and the numeric comparisons of a field condition, all of
/// which must hold. A bound naming another field reads that field (or its default); a
/// value or bound that is not a number fails every comparison.
fn compare_holds(
    cat: &Catalog,
    provider: &str,
    e: &EntityRef,
    v: Option<&Value>,
    cmp: ttg_catalog::Compare<'_>,
) -> bool {
    use ttg_catalog::fields::number;
    let text = v.map(|v| v.display());
    if !cmp.one_of.is_empty() && !text.as_ref().is_some_and(|t| cmp.one_of.contains(t)) {
        return false;
    }
    if text.as_ref().is_some_and(|t| cmp.not_one_of.contains(t)) {
        return false;
    }
    let bounds = cmp.bounds();
    if bounds.is_empty() {
        return true;
    }
    let Some(x) = v.and_then(number) else {
        return false;
    };
    bounds.iter().all(|(op, b)| {
        let y = match b {
            ttg_catalog::Bound::Number(n) => Some(*n),
            ttg_catalog::Bound::Field(f) => field_or_default(cat, provider, e, &f.field, false)
                .as_ref()
                .and_then(number),
            ttg_catalog::Bound::ProviderField(f) => {
                field_or_default(cat, provider, e, &f.provider_field, true)
                    .as_ref()
                    .and_then(number)
            }
        };
        let Some(y) = y else { return false };
        match *op {
            "less_than" => x < y,
            "at_most" => x <= y,
            "greater_than" => x > y,
            _ => x >= y,
        }
    })
}

/// The value of a project setting a `{ setting = "…" }` condition may test (the list is
/// `ttg_catalog::CONDITION_SETTINGS`; the catalog refuses any other name).
fn setting_value(p: &Project, name: &str) -> Option<Value> {
    match name {
        "kubernetes_manifests" => Some(Value::Bool(p.settings.kubernetes_manifests)),
        _ => None,
    }
}

/// Is this link one only the Kubernetes manifests export reads (`manifests = true` on the
/// source type's relation declaration for that target type)? Such a link is never a
/// Terraform mapping's business: like `calls` it earns no `depends_on`, no "cannot
/// express" warning and no "link by hand" step.
pub fn manifests_only(cat: &Catalog, source_type: &str, relation: Relation, target_type: &str) -> bool {
    cat.resource(source_type).is_some_and(|def| {
        def.relations
            .iter()
            .any(|r| r.manifests && r.kind == relation.key() && r.targets.iter().any(|t| t == target_type))
    })
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
    let (Some(src), Some(t)) = (p.entity(&e.source), p.entity(&e.target)) else {
        return false;
    };
    let Some(def) = cat.resource(src.resource_type) else {
        return false;
    };
    p.is_ancestor(&e.target, &e.source)
        && def.relations.iter().any(|r| {
            r.kind == e.relation.key() && r.via_parent && r.targets.iter().any(|x| x == t.resource_type)
        })
}

/// Targets of a relation for an entity: explicit edges first, then (if some declaration of
/// the kind is `via_parent`) the nearest enclosing container of an allowed type.
pub fn relation_targets(p: &Project, cat: &Catalog, e: &EntityRef, kind: Relation) -> Vec<Id> {
    targets_where(p, cat, e, kind, &|_| true)
}

/// Targets of one *relation declaration*. A type may declare the same relation kind
/// several times, one per group of target types (a DNS Record's zone and its alias target
/// are both `attribute_reference`); each declaration then only owns links to its own
/// targets, so cardinality and "satisfied" are judged per declaration.
pub fn declared_relation_targets(
    p: &Project,
    cat: &Catalog,
    e: &EntityRef,
    def: &ttg_catalog::ResourceDef,
    r: &ttg_catalog::RelationDef,
) -> Vec<Id> {
    let Some(kind) = Relation::from_key(&r.kind) else {
        return Vec::new();
    };
    if def.relations.iter().filter(|x| x.kind == r.kind).count() < 2 {
        return relation_targets(p, cat, e, kind);
    }
    targets_where(p, cat, e, kind, &|ty| r.targets.iter().any(|d| d == ty))
}

/// The one place edges of a relation kind are resolved. `keep` narrows the targets by
/// abstract type; the `via_parent` fallback only fires when no *accepted* edge exists, so
/// one declaration of a kind never swallows another's containment.
fn targets_where(
    p: &Project,
    cat: &Catalog,
    e: &EntityRef,
    kind: Relation,
    keep: &dyn Fn(&str) -> bool,
) -> Vec<Id> {
    let accepted = |id: &str| p.entity(id).is_some_and(|x| keep(x.resource_type));
    let mut out: Vec<Id> = p
        .edges_from(e.id)
        .filter(|x| x.relation == kind)
        .map(|x| x.target.clone())
        .filter(|t| accepted(t))
        .collect();
    if out.is_empty() {
        let Some(def) = cat.resource(e.resource_type) else {
            return out;
        };
        let containers: Vec<&str> = def
            .relations
            .iter()
            .filter(|r| r.kind == kind.key() && r.via_parent)
            .flat_map(|r| r.targets.iter().map(String::as_str))
            .filter(|t| keep(t))
            .collect();
        if let Some(c) = p
            .ancestors(e.id)
            .into_iter()
            .find(|c| containers.contains(&c.container_type.as_str()))
        {
            out.push(c.id.clone());
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

/// Does the mapping at the *target* end of an edge read it (an `incoming` source or
/// condition naming the relation and the source's type)? Then the link is expressed
/// there — Cloud CDN switched on by the load balancer a CDN points at — and the source
/// needs neither a `depends_on` (which would point the wrong way and could close a cycle)
/// nor a "cannot express" warning.
pub fn read_by_target(
    cat: &Catalog,
    provider: &str,
    relation: Relation,
    source_type: &str,
    target_type: &str,
) -> bool {
    let Some(m) = cat.mapping(target_type, provider) else {
        return false;
    };
    let mut found = HashSet::new();
    for b in m.blocks.iter().chain(m.data.iter()) {
        scan_block_incoming(b, &mut found);
    }
    found
        .iter()
        .any(|c| c.relation == relation.key() && c.target_type.as_deref().is_none_or(|t| t == source_type))
}

fn scan_block_incoming(b: &BlockDef, out: &mut HashSet<Consumed>) {
    if let Some(c) = &b.when {
        scan_cond_incoming(c, out);
    }
    b.args.values().for_each(|s| scan_source_incoming(s, out));
    b.nested.iter().for_each(|n| scan_nested_incoming(n, out));
}

fn scan_nested_incoming(n: &NestedBlockDef, out: &mut HashSet<Consumed>) {
    if let Some(c) = &n.when {
        scan_cond_incoming(c, out);
    }
    n.args.values().for_each(|s| scan_source_incoming(s, out));
    n.nested.iter().for_each(|x| scan_nested_incoming(x, out));
}

fn scan_cond_incoming(c: &Condition, out: &mut HashSet<Consumed>) {
    match c {
        Condition::Relation(r) if r.incoming => {
            out.insert(Consumed {
                relation: r.relation.clone(),
                target_type: r.target_type.clone(),
            });
        }
        Condition::All(a) => a.all.iter().for_each(|x| scan_cond_incoming(x, out)),
        Condition::Any(a) => a.any.iter().for_each(|x| scan_cond_incoming(x, out)),
        Condition::Not(n) => scan_cond_incoming(&n.not, out),
        _ => {}
    }
}

fn scan_source_incoming(s: &ArgSource, out: &mut HashSet<Consumed>) {
    match s {
        ArgSource::Relation(r) => {
            if r.incoming {
                out.insert(Consumed {
                    relation: r.relation.clone(),
                    target_type: r.target_type.clone(),
                });
            }
            if let Some(fb) = &r.fallback {
                scan_source_incoming(fb, out);
            }
        }
        ArgSource::Field(f) => {
            if let Some(fb) = &f.fallback {
                scan_source_incoming(fb, out);
            }
        }
        ArgSource::ProviderField(f) => {
            if let Some(fb) = &f.fallback {
                scan_source_incoming(fb, out);
            }
        }
        ArgSource::If(i) => {
            scan_cond_incoming(&i.cond, out);
            scan_source_incoming(&i.then, out);
            if let Some(o) = &i.otherwise {
                scan_source_incoming(o, out);
            }
        }
        ArgSource::Raw(r) => r.refs.values().for_each(|x| scan_source_incoming(x, out)),
        ArgSource::Object(o) => o.object.values().for_each(|x| scan_source_incoming(x, out)),
        ArgSource::List(l) => l.list.iter().for_each(|x| scan_source_incoming(x, out)),
        ArgSource::Func(f) => f.args.iter().for_each(|x| scan_source_incoming(x, out)),
        _ => {}
    }
}

/// Relations a mapping talks about in a `when`-guarded manual step. It cannot generate
/// the link, but it says so in its own words, so the generic "link by hand" entry would
/// only repeat that. The `depends_on` for ordering is still emitted.
pub fn relations_with_manual_step(m: &ProviderMapping) -> Vec<Consumed> {
    let mut rel = HashSet::new();
    for step in &m.manual_steps {
        if let Some(c) = &step.when {
            scan_cond(c, &mut rel);
        }
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
        // An `incoming` condition reads the *other* end's edge; it says nothing about
        // what this entity does with its own outgoing links.
        Condition::Relation(r) if !r.incoming => {
            rel.insert(Consumed {
                relation: r.relation.clone(),
                target_type: r.target_type.clone(),
            });
        }
        Condition::All(a) => a.all.iter().for_each(|x| scan_cond(x, rel)),
        Condition::Any(a) => a.any.iter().for_each(|x| scan_cond(x, rel)),
        Condition::Not(n) => scan_cond(&n.not, rel),
        _ => {}
    }
}

fn scan_source(s: &ArgSource, rel: &mut HashSet<Consumed>, anc: &mut HashSet<String>) {
    match s {
        ArgSource::Relation(r) => {
            // An `incoming` source reads the *other* end's edge; it says nothing about
            // what this entity does with its own outgoing links (same rule as `when`).
            if !r.incoming {
                rel.insert(Consumed {
                    relation: r.relation.clone(),
                    target_type: r.target_type.clone(),
                });
            }
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
        ArgSource::Rows(r) => {
            if let Some(c) = &r.when {
                scan_cond(c, rel);
            }
            scan_source(&r.each, rel, anc);
        }
        ArgSource::Raw(r) => r.refs.values().for_each(|x| scan_source(x, rel, anc)),
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
            provider: None,
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
    // GCP: route tables are logical; Cloud NAT (or an internet gateway) covers the
    // whole network.
    if provider == "gcp" {
        for s in &subnets {
            let vnet = relation_targets(p, cat, s, Relation::NetworkMembership)
                .into_iter()
                .next();
            let covered = entities.iter().any(|e| {
                matches!(e.resource_type, "nat_gateway" | "internet_gateway")
                    && p.ancestor_of_type(e.id, "virtual_network").map(|c| c.id.clone()) == vnet
            });
            if covered {
                egress_subnets.insert(s.id.to_string());
            }
        }
    }
    for (subnet, tables) in &tables_per_subnet {
        if provider == "gcp" {
            break;
        }
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
    // A NAT gateway's own subnet must route to an internet gateway (not on GCP, where
    // Cloud NAT needs no subnet).
    for nat in entities
        .iter()
        .filter(|e| e.resource_type == "nat_gateway" && provider != "gcp")
    {
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
    // Azure: a subnet delegated to Container Apps environments must be /23 or larger.
    if provider == "azure" {
        for s in &subnets {
            let delegated = s
                .provider_field("azure", "delegation")
                .map(|v| v.display())
                .as_deref()
                == Some("app_environments");
            let prefix = s
                .field("cidr_block")
                .and_then(|v| v.as_str().map(|x| x.to_string()))
                .and_then(|c| c.split_once('/').and_then(|(_, l)| l.parse::<u32>().ok()));
            if delegated && prefix.is_some_and(|l| l > 23) {
                push(
                    s.id,
                    Severity::Warning,
                    format!(
                        "is delegated to Container Apps environments but /{} is too small: Azure needs /23 or larger",
                        prefix.unwrap()
                    ),
                );
            }
        }
    }

    // Functions, container apps and jobs in subnets need an outbound route to reach
    // queues, secrets and storage, unless every such link goes over a private endpoint in
    // the same network.
    for f in entities
        .iter()
        .filter(|e| matches!(e.resource_type, "function" | "container_app" | "container_job"))
    {
        let subnets = relation_targets(p, cat, f, Relation::NetworkMembership);
        let Some(s) = subnets.iter().find(|s| !egress_subnets.contains(*s)) else {
            continue;
        };
        if subnets.iter().any(|s| egress_subnets.contains(s)) {
            continue; // another subnet has a way out
        }
        let vnet = relation_targets(p, cat, &p.entity(s).unwrap(), Relation::NetworkMembership)
            .into_iter()
            .next();
        let managed_targets: Vec<Id> = p
            .edges_from(f.id)
            .filter(|e| e.relation != Relation::DependsOn)
            .map(|e| e.target.clone())
            .filter(|t| {
                p.entity(t)
                    .is_some_and(|x| crate::reach::is_managed(x.resource_type))
            })
            .collect();
        let uncovered: Vec<String> = managed_targets
            .iter()
            .filter(|t| {
                vnet.as_deref()
                    .is_none_or(|v| crate::reach::private_endpoint_in(p, cat, v, t).is_none())
            })
            .map(|t| name_of(t))
            .collect();
        if managed_targets.is_empty() {
            push(
                f.id,
                Severity::Info,
                format!(
                    "runs in subnet \"{}\" which has no default route via a NAT or Internet Gateway; fine while it only talks to resources inside the network or over private endpoints",
                    name_of(s)
                ),
            );
        } else if uncovered.is_empty() {
            push(
                f.id,
                Severity::Info,
                format!(
                    "runs in subnet \"{}\" with no default route; its managed-service links all go over private endpoints in the network",
                    name_of(s)
                ),
            );
        } else {
            push(
                f.id,
                Severity::Warning,
                format!(
                    "runs in subnet \"{}\" which has no default route via a NAT or Internet Gateway; it will not reach {} (add a NAT route or a private endpoint in this network)",
                    name_of(s),
                    uncovered
                        .iter()
                        .map(|n| format!("\"{n}\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }
    }

    // 5. A security group that rules admit traffic from, but that nothing carries: every
    //    one of those rules admits no one.
    for (group, users) in crate::reach::memberless_sources(p, cat) {
        let users: Vec<String> = users.iter().map(|u| name_of(u)).collect();
        push(
            &group,
            Severity::Warning,
            format!(
                "is the source of rules in {} but nothing is a member of it, so those rules admit no one; link the resources that should carry it with 'Uses security group' (a Kubernetes cluster or node pool, an instance, a function), or make the rules name them another way",
                quoted_list(&users, 4)
            ),
        );
    }

    // 6. An AWS interface endpoint with private DNS answers for its service's public name
    //    across the whole network, so one whose security group admits nobody there breaks
    //    every call to that service, not just the calls meant for it.
    if provider == "aws" {
        for pe in entities.iter().filter(|e| e.resource_type == "private_endpoint") {
            let service = field_or_default(cat, provider, pe, "service", true)
                .map(|v| v.display())
                .unwrap_or_default();
            if matches!(service.as_str(), "s3" | "dynamodb") {
                continue; // gateway endpoints: routes, no interface, no group
            }
            let Some(refused) = crate::reach::endpoint_refuses_all(p, cat, pe) else {
                continue;
            };
            let refused: Vec<String> = refused.iter().map(|r| name_of(r)).collect();
            let consequence = match service.as_str() {
                "ecr.api" | "ecr.dkr" => "image pulls from ECR will fail".to_string(),
                "sts" => "pod identity and every role assumption will fail".to_string(),
                _ => format!("calls to {service} from the network will fail"),
            };
            push(
                pe.id,
                Severity::Error,
                format!(
                    "has private DNS on, so everything in its network reaches {service} through it, but its security group admits none of {} on port 443: {consequence}. Add an ingress rule for 443 from their subnets or a security group they carry",
                    quoted_list(&refused, 4)
                ),
            );
        }
    }
}

/// `"a"`, `"a" and "b"`, `"a", "b" and "c"`; past `max` names, `… and N more`.
fn quoted_list(names: &[String], max: usize) -> String {
    let mut shown: Vec<String> = names.iter().take(max).map(|n| format!("\"{n}\"")).collect();
    if names.len() > max {
        shown.push(format!("{} more", names.len() - max));
    }
    match shown.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
        Some((only, _)) => only.clone(),
        None => String::new(),
    }
}

/// Extra provider arguments (and every argument of a native resource) against the
/// provider schema: unknown names, read-only attributes, type mismatches, missing
/// required arguments on native resources, and overrides of mapping-set arguments.
fn extra_checks(p: &Project, cat: &Catalog, provider: &str, out: &mut Vec<Diagnostic>) {
    let idx = ttg_schema::index();
    for e in p.entities() {
        let native = ttg_catalog::load::Catalog::is_native(e.resource_type);
        let Some(m) = cat.mapping(e.resource_type, provider) else {
            continue;
        };
        for b in &m.blocks {
            let extra = e.extra_args(provider, &b.key);
            let Some(schema) = idx.resource(provider, &b.resource) else {
                if extra.is_some_and(|x| !x.is_empty()) {
                    out.push(Diagnostic {
                        entity: Some(e.id.to_string()),
                        severity: Severity::Info,
                        code: Code::Extra,
                        message: format!(
                            "{}: no schema for {} in the bundled index, extra arguments are not checked",
                            b.key, b.resource
                        ),
                        provider: None,
                    });
                }
                continue;
            };
            if let Some(extra) = extra {
                for (k, v) in extra {
                    if let Some(a) = schema.attributes.get(k) {
                        if a.read_only() {
                            out.push(Diagnostic {
                                entity: Some(e.id.to_string()),
                                severity: Severity::Error,
                                code: Code::Extra,
                                message: format!("{}.{k} is read-only on {}", b.resource, provider),
                                provider: None,
                            });
                        } else if !json_matches(a.kind(), v) {
                            out.push(Diagnostic {
                                entity: Some(e.id.to_string()),
                                severity: Severity::Error,
                                code: Code::Extra,
                                message: format!("{}.{k} expects a {} value", b.resource, a.kind().label()),
                                provider: None,
                            });
                        }
                    } else if let Some(n) = schema.blocks.get(k) {
                        let ok = match v {
                            serde_json::Value::Object(_) => true,
                            serde_json::Value::Array(items) => items.iter().all(|i| i.is_object()),
                            _ => false,
                        };
                        if !ok {
                            out.push(Diagnostic {
                                entity: Some(e.id.to_string()),
                                severity: Severity::Error,
                                code: Code::Extra,
                                message: format!(
                                    "{}.{k} is a nested block ({}): give an object or a list of objects",
                                    b.resource,
                                    n.nesting()
                                ),
                                provider: None,
                            });
                        }
                    } else if !["depends_on", "count", "for_each", "provider", "lifecycle"]
                        .contains(&k.as_str())
                    {
                        out.push(Diagnostic {
                            entity: Some(e.id.to_string()),
                            severity: Severity::Error,
                            code: Code::Extra,
                            message: format!("{} has no argument '{k}' on {}", b.resource, provider),
                            provider: None,
                        });
                    }
                    if b.args.contains_key(k) || b.nested.iter().any(|n| &n.block == k) {
                        out.push(Diagnostic {
                            entity: Some(e.id.to_string()),
                            severity: Severity::Info,
                            code: Code::Extra,
                            message: format!(
                                "extra argument {k} overrides the value the {} mapping sets",
                                b.resource
                            ),
                            provider: None,
                        });
                    }
                }
            }
            if let Some(extra) = extra {
                orphaned_by_extra(p, cat, provider, &e, m, b, extra, out);
            }
            if native {
                let have = extra.cloned().unwrap_or_default();
                let missing: Vec<&str> = schema
                    .required()
                    .into_iter()
                    .filter(|r| !have.contains_key(*r))
                    .collect();
                if !missing.is_empty() {
                    out.push(Diagnostic {
                        entity: Some(e.id.to_string()),
                        severity: Severity::Error,
                        code: Code::Extra,
                        message: format!("{} requires: {}", b.resource, missing.join(", ")),
                        provider: None,
                    });
                }
            }
        }
    }
}

/// An `extra` argument that replaces the mapping's reference to another of the entity's
/// own blocks can leave that block created and used by nothing: a service pointed at a
/// shared cluster by hand still gets a cluster of its own. Reported when the replaced
/// argument was the block's only use inside the mapping — whatever else is derived from
/// it (an output, another resource's reference to it) still points at the orphan.
#[allow(clippy::too_many_arguments)]
fn orphaned_by_extra(
    p: &Project,
    cat: &Catalog,
    provider: &str,
    e: &EntityRef,
    m: &ProviderMapping,
    b: &BlockDef,
    extra: &ttg_core::ExtraArgs,
    out: &mut Vec<Diagnostic>,
) {
    // (block it referred to, the argument that now replaces the reference)
    let mut replaced: Vec<(String, String)> = Vec::new();
    for k in extra.keys() {
        if let Some(src) = b.args.get(k) {
            let mut refs = Vec::new();
            self_block_refs(src, &mut refs);
            replaced.extend(refs.into_iter().map(|r| (r, k.clone())));
        }
    }
    replaced.retain(|(k, _)| k != &b.key);
    replaced.sort();
    replaced.dedup_by(|a, b| a.0 == b.0);
    for (key, arg) in replaced {
        let Some(orphan) = m.blocks.iter().find(|x| x.key == key) else {
            continue;
        };
        // Only a block that is actually emitted, once, can be orphaned.
        if orphan.for_each_field.is_some() || orphan.for_each_relation.is_some() {
            continue;
        }
        if orphan
            .when
            .as_ref()
            .is_some_and(|c| !condition_holds(p, cat, provider, e, c, None))
        {
            continue;
        }
        let still_used = m.blocks.iter().any(|other| {
            let overridden = |arg: &str| {
                e.extra_args(provider, &other.key)
                    .is_some_and(|x| x.contains_key(arg))
            };
            let mut refs = Vec::new();
            for (k, src) in &other.args {
                if !overridden(k) {
                    self_block_refs(src, &mut refs);
                }
            }
            for n in other.nested.iter().filter(|n| !overridden(&n.block)) {
                nested_self_block_refs(n, &mut refs);
            }
            refs.iter().any(|r| r == &key)
        });
        if !still_used {
            out.push(Diagnostic {
                entity: Some(e.id.to_string()),
                severity: Severity::Warning,
                code: Code::Extra,
                message: format!(
                    "extra argument {}.{arg} replaces the only reference to this resource's own {} ({}), which is still created but used by nothing; outputs and other resources' references derived from it still point at it",
                    b.key, orphan.key, orphan.resource
                ),
                provider: None,
            });
        }
    }
}

/// Every `self_block` a source reads, at any depth.
fn self_block_refs(src: &ArgSource, out: &mut Vec<String>) {
    match src {
        ArgSource::SelfBlock(s) => out.push(s.self_block.clone()),
        ArgSource::If(i) => {
            self_block_refs(&i.then, out);
            if let Some(o) = &i.otherwise {
                self_block_refs(o, out);
            }
        }
        ArgSource::Field(f) => {
            if let Some(fb) = &f.fallback {
                self_block_refs(fb, out);
            }
        }
        ArgSource::ProviderField(f) => {
            if let Some(fb) = &f.fallback {
                self_block_refs(fb, out);
            }
        }
        ArgSource::Relation(r) => {
            if let Some(fb) = &r.fallback {
                self_block_refs(fb, out);
            }
        }
        ArgSource::Rows(r) => self_block_refs(&r.each, out),
        ArgSource::Raw(r) => r.refs.values().for_each(|x| self_block_refs(x, out)),
        ArgSource::Object(o) => o.object.values().for_each(|x| self_block_refs(x, out)),
        ArgSource::List(l) => l.list.iter().for_each(|x| self_block_refs(x, out)),
        ArgSource::Func(f) => f.args.iter().for_each(|x| self_block_refs(x, out)),
        _ => {}
    }
}

fn nested_self_block_refs(n: &NestedBlockDef, out: &mut Vec<String>) {
    for src in n.args.values() {
        self_block_refs(src, out);
    }
    for inner in &n.nested {
        nested_self_block_refs(inner, out);
    }
}

/// An extra argument in the form the provider schema declares, for storing it that way:
/// `30` for a string argument becomes `"30"` (HCL would convert it anyway), `"30"` for a
/// number becomes `30` and `"true"` for a bool becomes `true`. Nested blocks (an object,
/// or a list of objects) are converted argument by argument. Anything else, references
/// and raw expressions included, is returned as it is.
pub fn canonical_extra(provider: &str, resource: &str, key: &str, v: serde_json::Value) -> serde_json::Value {
    match ttg_schema::index().resource(provider, resource) {
        Some(schema) => canonical_in(schema, key, v),
        None => v,
    }
}

fn canonical_in(schema: &ttg_schema::BlockSchema, key: &str, v: serde_json::Value) -> serde_json::Value {
    use serde_json::Value as J;
    use ttg_schema::TypeKind as K;
    if let Some(a) = schema.attributes.get(key) {
        return match (a.kind(), v) {
            (K::String, J::Number(n)) => J::String(n.to_string()),
            (K::String, J::Bool(b)) => J::String(b.to_string()),
            (K::Number, J::String(s)) => match s.trim().parse::<i64>() {
                Ok(i) => J::from(i),
                Err(_) => match s
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
                {
                    Some(n) => J::Number(n),
                    None => J::String(s),
                },
            },
            (K::Bool, J::String(s)) if s == "true" || s == "false" => J::Bool(s == "true"),
            (_, other) => other,
        };
    }
    let Some(nested) = schema.blocks.get(key) else {
        return v;
    };
    let block = nested.block();
    let fix = |o: serde_json::Map<String, J>| -> J {
        J::Object(
            o.into_iter()
                .map(|(k, x)| {
                    let x = canonical_in(block, &k, x);
                    (k, x)
                })
                .collect(),
        )
    };
    match v {
        J::Object(o) if !o.contains_key("$ref") && !o.contains_key("$raw") => fix(o),
        J::Array(items) => J::Array(
            items
                .into_iter()
                .map(|i| match i {
                    J::Object(o) => fix(o),
                    other => other,
                })
                .collect(),
        ),
        other => other,
    }
}

fn json_matches(kind: ttg_schema::TypeKind, v: &serde_json::Value) -> bool {
    use ttg_schema::TypeKind as K;
    if v.as_object()
        .is_some_and(|o| o.contains_key("$ref") || o.contains_key("$raw"))
    {
        return true;
    }
    match kind {
        // HCL turns a number or bool into the string an argument wants.
        K::String => v.is_string() || v.is_number() || v.is_boolean(),
        K::Number => v.is_number() || v.is_string(),
        K::Bool => v.is_boolean() || v.is_string(),
        K::List | K::Set => v.is_array(),
        K::Map | K::Object => v.is_object(),
        K::Dynamic => true,
    }
}
