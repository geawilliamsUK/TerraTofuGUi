//! Named environments in the export: one root per provider, a `.tfvars` and a backend
//! configuration per environment.
//!
//! A project with `settings.environments` is generated once per environment, each time
//! on the project resolved for it (`Project::for_environment`), by the ordinary
//! single-environment pipeline. The results are then merged block by block into one
//! configuration that says the same thing in every environment once given that
//! environment's `.tfvars`:
//!
//! - an argument whose value is the same everywhere stays as it is;
//! - a value that is a project variable (`${var.db_class}`), or a string built from them
//!   (`/zipos/${var.environment}/web`), or a name behind the name prefix, is written as
//!   that variable or template, the variable declared once and set per environment;
//! - any other plain value (string, number, bool, or a list or map of them) that differs
//!   becomes a variable of its own, named after the field when it *is* the field's value
//!   in every environment (`db_high_availability`, shared by every argument that follows
//!   it) and after the resource and argument otherwise (`db_instance_class`); an argument
//!   one environment leaves out gets `null` there, which Terraform reads as "not set";
//! - inside a value built from references and functions (`jsonencode({...})`, a list of
//!   subnet ids) only the parts that differ are treated so;
//! - a value that differs in references (a route through prod's second NAT gateway)
//!   chooses on `var.environment`;
//! - a nested block some environments leave out becomes a `dynamic` block, and a
//!   resource some environments leave out (an entity `absent` there, a repeated block with
//!   fewer rows) gets a `count`; both are gated by the field that decides it when one
//!   does, else by `var.environment`. References to a counted resource read
//!   `one(<address>[*].<attr>)`, which is null where it does not exist;
//! - `depends_on` lists are joined.
//!
//! What cannot be said that way — a different number of a repeated nested block, a
//! labelled nested block, a reference to a counted resource inside a heredoc, Kubernetes
//! manifests or a bootstrap root that differ — is refused with an `Environment`
//! diagnostic naming the entity, the block and the fields that differ, never written out
//! wrong. Each environment also gets `environments/<env>.backend.hcl`: the backend
//! argument that differs, its state key, completing the partial backend in `versions.tf`
//! at `init`.

use crate::diagnostics::{self, Code, Diagnostic, Severity};
use crate::emit::{self, value_expr, var_ref, EntityBlocks, Generated};
use crate::files;
use crate::tool::Profile;
use crate::GenError;
use hcl::expr::{Conditional, FuncCall, Traversal, TraversalOperator};
use hcl::{Block, Expression, Object, ObjectKey, Structure};
use indexmap::IndexMap;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use ttg_catalog::{Catalog, FieldType};
use ttg_core::environment::variable_refs;
use ttg_core::{Id, Project, Tool, Value};

/// The directory of per-environment files inside an export.
pub const DIR: &str = "environments";

/// The variable holding the environment's name; every merged export declares it.
pub const ENVIRONMENT_VAR: &str = "environment";

/// One value that differs between environments, as a variable set per environment.
#[derive(Debug, Clone, PartialEq)]
pub struct Lifted {
    /// The variable: `var.<name>` in the configuration, `<name> = …` in each `.tfvars`.
    pub name: String,
    /// Its Terraform type (`bool`, `number`, `string`, `list(string)`, …).
    pub var_type: String,
    pub description: String,
    /// The entity whose block uses it first; `None` for a project variable or the
    /// environment's name.
    pub entity: Option<Id>,
    /// The block that uses it first, e.g. `aws_db_instance.db`; empty for a project variable.
    pub address: String,
    /// Where in the block: `multi_az`, `tags.Environment`, or `high_availability` for the
    /// gate of a nested block.
    pub argument: String,
    /// The field whose value it carries, when it carries one unchanged; the project
    /// variable's own name for a project variable.
    pub field: Option<String>,
    /// The value per environment, in `settings.environments` order. `None` where the
    /// value is never read (inside a block that environment leaves out); the `.tfvars`
    /// then says `null`.
    pub values: Vec<(String, Option<Expression>)>,
    /// Allowed values, checked by a `validation` block (an enum field's options, the
    /// environment names).
    options: Vec<String>,
}

impl Lifted {
    /// The value for one environment (`Expression::Null` when it is never read).
    pub fn value(&self, env: &str) -> Expression {
        self.values
            .iter()
            .find(|(e, _)| e == env)
            .and_then(|(_, v)| v.clone())
            .unwrap_or(Expression::Null)
    }
}

/// A diagnostic from one environment's run, or about environments as a whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvDiagnostic {
    /// The environment it holds in; `None` for one about how the environments differ.
    pub environment: Option<String>,
    pub diagnostic: Diagnostic,
}

// ------------------------------------------------------------------ checks

/// What can be wrong with environments and project variables as written: a name that
/// cannot name a file or a variable, a name listed twice, overrides or values for an
/// environment the project does not have, overrides of a field the type does not
/// declare, a `${var.…}` that names no variable, a name prefix that cannot be part of a
/// resource name. Part of every `diagnostics::run`.
pub fn checks(p: &Project, cat: &Catalog) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut push = |entity: Option<&str>, severity, message: String| {
        out.push(Diagnostic {
            entity: entity.map(str::to_string),
            severity,
            code: Code::Environment,
            message,
            provider: None,
        })
    };
    let envs = &p.settings.environments;
    let mut seen = HashSet::new();
    for e in envs {
        if let Err(msg) = ttg_core::environment::check_environment_name(e) {
            push(None, Severity::Error, msg);
        }
        if !seen.insert(e) {
            push(
                None,
                Severity::Error,
                format!("environment \"{e}\" is listed twice"),
            );
        }
    }
    let unknown_env = |env: &str| !envs.iter().any(|e| e == env);
    for (name, v) in &p.settings.variables {
        if let Err(msg) = ttg_core::environment::check_variable_name(name) {
            push(None, Severity::Error, msg);
        }
        for env in v.environments.keys().filter(|e| unknown_env(e)) {
            push(
                None,
                Severity::Warning,
                format!("variable \"{name}\" has a value for \"{env}\", which is not an environment of the project: ignored"),
            );
        }
    }
    let known = |n: &str| {
        p.settings.variables.contains_key(n) || ttg_core::environment::BUILTIN_VARIABLES.contains(&n)
    };
    let unknown_refs = |entity: Option<&str>,
                        what: &str,
                        text: &str,
                        out: &mut dyn FnMut(Option<&str>, Severity, String)| {
        for r in variable_refs(text) {
            if !known(r) {
                out(
                    entity,
                    Severity::Error,
                    format!(
                        "{what} uses ${{var.{r}}}, but there is no project variable \"{r}\" (Settings ▸ Environments declares them)"
                    ),
                );
            }
        }
    };
    if let Some(prefix) = &p.settings.name_prefix {
        unknown_refs(None, "the name prefix", prefix, &mut push);
        let rendered = p.variable_values(envs.first().map(String::as_str))["name_prefix"].display();
        if !rendered
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            push(
                None,
                Severity::Warning,
                format!(
                    "the name prefix renders as \"{rendered}\": most providers' resource names take only lowercase letters, digits and hyphens"
                ),
            );
        }
    }
    for t in p.settings.tags.values() {
        unknown_refs(None, "a default tag", t, &mut push);
    }
    for e in p.entities() {
        let strings = entity_strings(&e);
        for s in &strings {
            unknown_refs(Some(e.id), &format!("\"{}\"", e.name), s, &mut push);
        }
        let Some(overrides) = p.overrides_of(e.id) else {
            continue;
        };
        let def = cat.resource(e.resource_type);
        for (env, o) in overrides {
            if o.is_empty() {
                continue;
            }
            if unknown_env(env) {
                push(
                    Some(e.id),
                    Severity::Warning,
                    if envs.is_empty() {
                        format!(
                            "\"{}\" overrides values for environment \"{env}\", but the project has no environments: the overrides are ignored",
                            e.name
                        )
                    } else {
                        format!(
                            "\"{}\" overrides values for environment \"{env}\", which is not one of the project's ({}): the overrides are ignored",
                            e.name,
                            envs.join(", ")
                        )
                    },
                );
                continue;
            }
            let Some(def) = def else { continue };
            for k in o.config.keys() {
                if !def.fields.iter().any(|f| &f.name == k) {
                    push(
                        Some(e.id),
                        Severity::Warning,
                        format!(
                            "\"{}\" overrides \"{k}\" in {env}, which is not a field of {}: ignored",
                            e.name, e.resource_type
                        ),
                    );
                }
            }
            for (pid, vals) in &o.provider_config {
                let fields = def.providers.get(pid).map(|m| &m.fields);
                for k in vals.keys() {
                    if !fields.is_some_and(|fs| fs.iter().any(|f| &f.name == k)) {
                        push(
                            Some(e.id),
                            Severity::Warning,
                            format!(
                                "\"{}\" overrides the {pid} field \"{k}\" in {env}, which {} does not have: ignored",
                                e.name, e.resource_type
                            ),
                        );
                    }
                }
            }
        }
    }
    for edge in &p.edges {
        for env in edge.environments.iter().filter(|e| unknown_env(e)) {
            push(
                Some(&edge.source),
                Severity::Warning,
                format!(
                    "a {} link from \"{}\" is tagged for environment \"{env}\", which the project does not have",
                    edge.relation.key(),
                    p.entity(&edge.source).map(|e| e.name.to_string()).unwrap_or_default()
                ),
            );
        }
    }
    out
}

/// Every string an entity carries in its fields and `extra` arguments (outside `$raw`):
/// the places `${var.…}` may appear.
fn entity_strings(e: &ttg_core::EntityRef) -> Vec<String> {
    fn of_value(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Str(s) => out.push(s.clone()),
            Value::List(l) => out.extend(l.iter().cloned()),
            Value::Records(rows) => rows
                .iter()
                .flat_map(|r| r.values())
                .for_each(|x| of_value(x, out)),
            _ => {}
        }
    }
    fn of_json(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::String(s) => out.push(s.clone()),
            serde_json::Value::Array(a) => a.iter().for_each(|x| of_json(x, out)),
            serde_json::Value::Object(o) if !o.contains_key("$raw") => {
                o.values().for_each(|x| of_json(x, out))
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    e.config.values().for_each(|v| of_value(v, &mut out));
    e.provider_config
        .values()
        .flat_map(|c| c.values())
        .for_each(|v| of_value(v, &mut out));
    e.extra
        .values()
        .flat_map(|b| b.values())
        .flat_map(|a| a.values())
        .for_each(|v| of_json(v, &mut out));
    out.retain(|s| s.contains("${var."));
    out
}

/// The diagnostics of the environments other than `current` (every environment when
/// `current` is `None`) that `current`'s own run does not already report, each tagged with
/// its environment, plus the differences between environments the export would refuse.
/// Several runs' worth of work, so the app computes it lazily, like the other providers'.
pub fn other_environments(
    p: &Project,
    cat: &Catalog,
    provider: &str,
    tool: Tool,
    current: Option<&str>,
) -> Vec<EnvDiagnostic> {
    let mut out = Vec::new();
    if p.settings.environments.is_empty() {
        return out;
    }
    let mine = diagnostics::run(&p.resolved(current), cat, provider);
    for env in &p.settings.environments {
        if Some(env.as_str()) == current {
            continue;
        }
        for d in diagnostics::run(&p.for_environment(env), cat, provider) {
            if !mine.contains(&d) {
                out.push(EnvDiagnostic {
                    environment: Some(env.clone()),
                    diagnostic: Diagnostic {
                        message: format!("[{env}] {}", d.message),
                        ..d
                    },
                });
            }
        }
    }
    if let Err(GenError::Blocked(ds)) = generate(p, cat, provider, tool) {
        for d in ds
            .into_iter()
            .filter(|d| d.code == Code::Environment && d.severity == Severity::Error)
        {
            if !mine.contains(&d) && !out.iter().any(|x| x.diagnostic == d) {
                out.push(EnvDiagnostic {
                    environment: None,
                    diagnostic: d,
                });
            }
        }
    }
    out
}

// ------------------------------------------------------------------ generation

/// Generate one root for every environment of `p` (see the module docs).
pub fn generate(p: &Project, cat: &Catalog, provider: &str, tool: Tool) -> Result<Generated, GenError> {
    let envs = p.settings.environments.clone();
    let own = checks(p, cat);
    if own.iter().any(|d| d.severity == Severity::Error) {
        return Err(GenError::Blocked(own));
    }
    // Variables are substituted first, then every value brought to its field's type, so a
    // `${var.x}` in a number field is a number by the time the environments are compared.
    let resolved: Vec<Project> = envs
        .iter()
        .map(|e| {
            let mut rp = p.for_environment(e);
            cat.normalize_values(&mut rp);
            rp
        })
        .collect();
    let mut runs: Vec<(String, Result<Generated, Vec<Diagnostic>>)> = Vec::new();
    for (env, rp) in envs.iter().zip(&resolved) {
        match emit::generate_one(rp, cat, provider, tool) {
            Ok(g) => runs.push((env.clone(), Ok(g))),
            Err(GenError::Blocked(d)) => runs.push((env.clone(), Err(d))),
            Err(GenError::Emit(msg)) => return Err(GenError::Emit(format!("[{env}] {msg}"))),
            Err(e) => return Err(e),
        }
    }
    let lists: Vec<(String, Vec<Diagnostic>)> = runs
        .iter()
        .map(|(env, r)| {
            let ds = match r {
                Ok(g) => g.diagnostics.clone(),
                Err(d) => d.clone(),
            };
            (env.clone(), ds)
        })
        .collect();
    let mut diags = tag_by_environment(&envs, &lists);
    for d in own {
        if !diags.contains(&d) {
            diags.push(d);
        }
    }
    if runs.iter().any(|(_, r)| r.is_err()) {
        diags.sort_by_key(|a| std::cmp::Reverse(a.severity));
        return Err(GenError::Blocked(diags));
    }
    let gens: Vec<(String, Generated)> = runs.into_iter().map(|(e, r)| (e, r.unwrap())).collect();
    let mut merged = match merge(p, cat, provider, tool, &resolved, &gens) {
        Ok(g) => g,
        Err(mut problems) => {
            problems.extend(diags);
            return Err(GenError::Blocked(problems));
        }
    };
    diags.sort_by_key(|a| std::cmp::Reverse(a.severity));
    merged.diagnostics = diags;
    Ok(merged)
}

/// One list of diagnostics from per-environment lists: a diagnostic every environment
/// reports appears once, as it is; one only some report is prefixed with them,
/// `[prod] …`, so a check that holds in prod but not in pilot says where it fails.
fn tag_by_environment(envs: &[String], lists: &[(String, Vec<Diagnostic>)]) -> Vec<Diagnostic> {
    let mut order: Vec<Diagnostic> = Vec::new();
    let mut holds_in: Vec<Vec<String>> = Vec::new();
    for (env, ds) in lists {
        for d in ds {
            match order.iter().position(|x| x == d) {
                Some(i) => {
                    if !holds_in[i].contains(env) {
                        holds_in[i].push(env.clone())
                    }
                }
                None => {
                    order.push(d.clone());
                    holds_in.push(vec![env.clone()]);
                }
            }
        }
    }
    order
        .into_iter()
        .zip(holds_in)
        .map(|(d, envs_with)| {
            if envs_with.len() == envs.len() {
                d
            } else {
                Diagnostic {
                    message: format!("[{}] {}", envs_with.join(", "), d.message),
                    ..d
                }
            }
        })
        .collect()
}

/// Is this expression a value a `.tfvars` file can hold: no references, no functions?
fn is_literal(e: &Expression) -> bool {
    match e {
        Expression::Null | Expression::Bool(_) | Expression::Number(_) | Expression::String(_) => true,
        Expression::Array(items) => items.iter().all(is_literal),
        Expression::Object(o) => o.iter().all(|(k, v)| {
            matches!(
                k,
                ObjectKey::Identifier(_) | ObjectKey::Expression(Expression::String(_))
            ) && is_literal(v)
        }),
        _ => false,
    }
}

fn is_container(e: &Expression) -> bool {
    matches!(
        e,
        Expression::Array(_) | Expression::Object(_) | Expression::FuncCall(_) | Expression::Parenthesis(_)
    )
}

/// One environment's view of an argument (or of whether a block exists).
#[derive(Debug, Clone, Copy)]
enum Slot<'a> {
    Val(&'a Expression),
    /// The argument is not set there.
    Unset,
    /// Never read there: the enclosing block does not exist in that environment.
    Any,
}

impl Slot<'_> {
    fn same(&self, other: &Slot) -> bool {
        match (self, other) {
            (Slot::Val(a), Slot::Val(b)) => a == b,
            (Slot::Unset, Slot::Unset) => true,
            _ => false,
        }
    }
    fn owned(&self) -> Option<Expression> {
        match self {
            Slot::Val(e) => Some((*e).clone()),
            Slot::Unset => Some(Expression::Null),
            Slot::Any => None,
        }
    }
}

/// A field of the entity being merged whose value differs between environments.
struct DiffField {
    name: String,
    /// Its value per environment as an expression, the way `field = …` would emit it
    /// (`None`: not set; a bool field that is not set reads as `false`).
    values: Vec<Option<Expression>>,
    options: Vec<String>,
}

/// What every environment brings to the merge besides its export.
struct EnvFacts {
    envs: Vec<String>,
    /// Project variable values per environment, the built-in ones included.
    vars: Vec<BTreeMap<String, Value>>,
}

/// The merge state: the variables made so far and what could not be merged.
struct Lifter<'a> {
    facts: &'a EnvFacts,
    /// Variable names the configuration already declares.
    taken: HashSet<String>,
    vars: IndexMap<String, Lifted>,
    /// Project variables a template or reference uses.
    project_vars: BTreeSet<String>,
    /// A project variable's values as the field it fills typed them (`"50"` written into a
    /// number field is the number 50), per environment; they win over the raw values.
    typed_vars: BTreeMap<String, Vec<Option<Expression>>>,
    problems: Vec<(Option<Id>, String)>,
    // The block being merged.
    entity: Option<Id>,
    entity_name: String,
    slug: String,
    local: String,
    address: String,
    fields: Vec<DiffField>,
    /// The entity's own strings that use project variables: what a differing string may
    /// be an instance of.
    templates: Vec<String>,
}

impl Lifter<'_> {
    fn envs(&self) -> &[String] {
        &self.facts.envs
    }

    fn problem(&mut self, msg: String) {
        let differs = if self.fields.is_empty() {
            String::new()
        } else {
            format!(
                " (fields that differ: {})",
                self.fields
                    .iter()
                    .map(|f| f.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let who = if self.entity_name.is_empty() {
            String::new()
        } else {
            format!("\"{}\": ", self.entity_name)
        };
        self.problems
            .push((self.entity.clone(), format!("{who}{msg}{differs}")));
    }

    /// The variable standing for these per-environment values: the field's name when the
    /// values are that field's, else the resource and the argument. A name already taken
    /// by other values gets a numeric suffix.
    /// The field whose values these are in every environment. Two fields can take the
    /// same values (high availability and deletion protection both off in pilot and on
    /// in prod); then the one whose name shares the most words with the argument wins,
    /// and when none does better than another, no field is named — a variable called
    /// after the wrong field would be worse than one called after the argument.
    fn field_for(&self, hint: &str, slots: &[Slot]) -> Option<usize> {
        let fits: Vec<usize> = self
            .fields
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                slots.iter().zip(&f.values).all(|(s, fv)| match (s, fv) {
                    (Slot::Any, _) => true,
                    (Slot::Val(v), Some(x)) => *v == x,
                    (Slot::Unset, None) => true,
                    _ => false,
                })
            })
            .map(|(i, _)| i)
            .collect();
        if fits.len() <= 1 {
            return fits.first().copied();
        }
        let words = |s: &str| -> HashSet<String> {
            s.split(|c: char| !c.is_ascii_alphanumeric())
                .filter(|w| !w.is_empty())
                .map(str::to_lowercase)
                .collect()
        };
        let mine = words(hint);
        let scored: Vec<(usize, usize)> = fits
            .iter()
            .map(|i| (*i, words(&self.fields[*i].name).intersection(&mine).count()))
            .collect();
        let best = scored.iter().map(|(_, s)| *s).max().unwrap_or(0);
        let winners: Vec<usize> = scored
            .iter()
            .filter(|(_, s)| *s == best)
            .map(|(i, _)| *i)
            .collect();
        (best > 0 && winners.len() == 1).then(|| winners[0])
    }

    fn var_for(&mut self, hint: &str, slots: &[Slot]) -> String {
        let field = self.field_for(hint, slots).map(|i| &self.fields[i]);
        let (base, field_name, options) = match field {
            Some(f) => (
                format!("{}_{}", self.slug, f.name),
                Some(f.name.clone()),
                f.options.clone(),
            ),
            None => (
                format!("{}_{}", self.local, ttg_core::slugify(hint)),
                None,
                Vec::new(),
            ),
        };
        let values: Vec<Option<Expression>> = slots.iter().map(Slot::owned).collect();
        let mut name = base.clone();
        let mut n = 2;
        loop {
            if self.taken.contains(&name) || self.facts.vars[0].contains_key(&name) {
                name = format!("{base}_{n}");
                n += 1;
                continue;
            }
            if let Some(v) = self.vars.get_mut(&name) {
                // The same name for the same values (a field two arguments follow) is
                // one variable; values it did not know yet fill its gaps.
                let fits = v.values.iter().zip(&values).all(|((_, a), b)| match (a, b) {
                    (Some(a), Some(b)) => a == b,
                    _ => true,
                });
                if fits {
                    for ((_, a), b) in v.values.iter_mut().zip(&values) {
                        if a.is_none() {
                            *a = b.clone();
                        }
                    }
                    return name;
                }
                name = format!("{base}_{n}");
                n += 1;
                continue;
            }
            break;
        }
        let what = match &field_name {
            Some(f) => format!("Field `{f}` of \"{}\"", self.entity_name),
            None if self.entity_name.is_empty() => format!("`{hint}` of `{}`", self.address),
            None => format!("`{hint}` of `{}` (\"{}\")", self.address, self.entity_name),
        };
        let lifted = Lifted {
            name: name.clone(),
            var_type: String::new(),
            description: format!("{what}: differs between environments; set in {DIR}/<environment>.tfvars."),
            entity: self.entity.clone(),
            address: self.address.clone(),
            argument: hint.to_string(),
            field: field_name,
            values: self.envs().iter().cloned().zip(values).collect(),
            options,
        };
        self.vars.insert(name.clone(), lifted);
        name
    }

    /// `var.environment == "prod"`, or `contains(["staging", "prod"], var.environment)`.
    fn env_is(&mut self, envs: &[&str]) -> Expression {
        let var = var_ref(ENVIRONMENT_VAR);
        if envs.len() == 1 {
            Expression::Operation(Box::new(hcl::expr::Operation::Binary(hcl::expr::BinaryOp::new(
                var,
                hcl::expr::BinaryOperator::Eq,
                Expression::String(envs[0].to_string()),
            ))))
        } else {
            let list = Expression::Array(envs.iter().map(|e| Expression::String(e.to_string())).collect());
            Expression::FuncCall(Box::new(FuncCall::builder("contains").arg(list).arg(var).build()))
        }
    }

    /// The condition under which a block exists: the bool field that decides it when one
    /// does (`var.db_high_availability`), else the environments it exists in.
    fn gate(&mut self, present: &[Option<bool>], hint: &str) -> Expression {
        let exprs: Vec<Option<Expression>> = present.iter().map(|p| p.map(Expression::Bool)).collect();
        let slots: Vec<Slot> = exprs
            .iter()
            .map(|e| e.as_ref().map(Slot::Val).unwrap_or(Slot::Any))
            .collect();
        // A bool field that is true exactly where the block exists.
        if self.field_for(hint, &slots).is_some() {
            return var_ref(&self.var_for(hint, &slots));
        }
        // A field that has one value exactly where the block exists (the WAF's mode is
        // "count"): `var.<field> == "count"`.
        let by_value = self.fields.iter().find_map(|f| {
            let mut on: Option<&Expression> = None;
            for (p, v) in present.iter().zip(&f.values) {
                if *p == Some(true) {
                    let v = v.as_ref()?;
                    if on.is_some_and(|x| x != v) {
                        return None;
                    }
                    on = Some(v);
                }
            }
            let on = on?.clone();
            let fits = present
                .iter()
                .zip(&f.values)
                .all(|(p, v)| *p != Some(false) || v.as_ref() != Some(&on));
            fits.then(|| (f.name.clone(), f.values.clone(), on))
        });
        if let Some((field, values, on)) = by_value {
            let slots: Vec<Slot> = values
                .iter()
                .map(|v| v.as_ref().map(Slot::Val).unwrap_or(Slot::Unset))
                .collect();
            let var = self.var_for(&field, &slots);
            return Expression::Operation(Box::new(hcl::expr::Operation::Binary(hcl::expr::BinaryOp::new(
                var_ref(&var),
                hcl::expr::BinaryOperator::Eq,
                on,
            ))));
        }
        let envs: Vec<String> = self.envs().to_vec();
        let on: Vec<&str> = envs
            .iter()
            .zip(present)
            .filter(|(_, p)| **p == Some(true))
            .map(|(e, _)| e.as_str())
            .collect();
        self.env_is(&on)
    }

    /// Is the value a project variable, or a string built from them or from the name
    /// prefix, in every environment? Then that is what to write.
    fn project_var_expr(&mut self, slots: &[Slot]) -> Option<Expression> {
        let concrete: Vec<(usize, &Expression)> = slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| match s {
                Slot::Val(e) => Some((i, *e)),
                _ => None,
            })
            .collect();
        if concrete.len() != slots.iter().filter(|s| !matches!(s, Slot::Any)).count() {
            return None; // unset somewhere
        }
        let facts = self.facts;
        for t in self.templates.clone() {
            let whole = t
                .strip_prefix("${var.")
                .and_then(|r| r.strip_suffix('}'))
                .filter(|r| !r.contains('}'));
            let fits = concrete.iter().all(|(i, v)| {
                let inst = ttg_core::environment::substitute(&t, &facts.vars[*i]);
                match whole {
                    // The field may have brought the value to its type: compare the text.
                    Some(_) => {
                        value_expr(&inst) == **v || literal_text(v).is_some_and(|x| x == inst.display())
                    }
                    None => Expression::String(inst.display()) == **v,
                }
            });
            if fits {
                for r in variable_refs(&t) {
                    self.project_vars.insert(r.to_string());
                }
                if let Some(name) = whole {
                    let typed: Vec<Option<Expression>> = slots
                        .iter()
                        .map(|s| match s {
                            Slot::Val(e) => Some((*e).clone()),
                            _ => None,
                        })
                        .collect();
                    let entry = self
                        .typed_vars
                        .entry(name.to_string())
                        .or_insert(vec![None; typed.len()]);
                    for (a, b) in entry.iter_mut().zip(typed) {
                        if a.is_none() {
                            *a = b;
                        }
                    }
                }
                return Some(match whole {
                    Some(name) => var_ref(name),
                    None => template(&t),
                });
            }
        }
        // A name behind the name prefix: as it is, in snake case, or with its hyphens
        // dropped (a name that takes letters and digits only).
        let prefixes: Vec<String> = facts.vars.iter().map(|v| v["name_prefix"].display()).collect();
        if prefixes.iter().any(|p| p.is_empty()) {
            return None;
        }
        let forms: [(&str, &str); 3] = [
            ("-", "${var.name_prefix}"),
            ("_", "${replace(var.name_prefix, \"-\", \"_\")}"),
            ("", "${replace(var.name_prefix, \"-\", \"\")}"),
        ];
        for (sep, head) in forms {
            let mut rest: Option<&str> = None;
            let ok = concrete.iter().all(|(i, v)| {
                let Expression::String(s) = v else { return false };
                let pre = prefixes[*i].replace('-', sep);
                match s.strip_prefix(pre.as_str()) {
                    Some(r) if rest.is_none_or(|x| x == r) => {
                        rest = Some(r);
                        true
                    }
                    _ => false,
                }
            });
            if ok {
                self.project_vars.insert("name_prefix".into());
                return Some(emit::raw_expr(&format!(
                    "\"{head}{}\"",
                    escape_template(rest.unwrap_or_default())
                )));
            }
        }
        None
    }

    /// Merge one argument's values. `Ok(None)`: unset everywhere (leave it out).
    fn unify_expr(&mut self, slots: &[Slot], hint: &str) -> Result<Option<Expression>, ()> {
        let concrete: Vec<Slot> = slots
            .iter()
            .filter(|s| !matches!(s, Slot::Any))
            .copied()
            .collect();
        let Some(first) = concrete.first().copied() else {
            return Ok(None);
        };
        if concrete.iter().all(|s| s.same(&first)) {
            return Ok(match first {
                Slot::Val(e) => Some(e.clone()),
                _ => None,
            });
        }
        if let Some(x) = self.project_var_expr(slots) {
            return Ok(Some(x));
        }
        let vals: Vec<&Expression> = concrete
            .iter()
            .filter_map(|s| match s {
                Slot::Val(e) => Some(*e),
                _ => None,
            })
            .collect();
        let literal = vals.iter().all(|e| is_literal(e));
        let all_set = vals.len() == concrete.len();
        // Scalars are lifted whole; a list, map or call of the same shape everywhere is
        // merged part by part, so only what differs inside it becomes a variable.
        if all_set && vals.iter().all(|e| is_container(e)) {
            if let Some(x) = self.unify_shape(slots, hint)? {
                return Ok(Some(x));
            }
        }
        if literal {
            let name = self.var_for(hint, slots);
            return Ok(Some(var_ref(&name)));
        }
        // References differ: choose on the environment.
        Ok(Some(self.choose(slots)))
    }

    /// `var.environment == "prod" ? <prod's value> : <the others'>`, grouping the
    /// environments that agree.
    fn choose(&mut self, slots: &[Slot]) -> Expression {
        let mut groups: Vec<(Expression, Vec<String>)> = Vec::new();
        for (env, s) in self.envs().to_vec().iter().zip(slots) {
            let Some(v) = s.owned() else { continue };
            match groups.iter_mut().find(|(x, _)| *x == v) {
                Some((_, es)) => es.push(env.clone()),
                None => groups.push((v, vec![env.clone()])),
            }
        }
        let (mut out, _) = groups.pop().expect("at least two groups");
        while let Some((v, es)) = groups.pop() {
            let refs: Vec<&str> = es.iter().map(String::as_str).collect();
            let cond = self.env_is(&refs);
            out = Expression::Conditional(Box::new(Conditional::new(cond, v, out)));
        }
        out
    }

    /// Part-by-part merge of lists, maps and calls with the same shape everywhere.
    /// `Ok(None)` when the shapes differ.
    fn unify_shape(&mut self, slots: &[Slot], hint: &str) -> Result<Option<Expression>, ()> {
        let vals: Vec<Option<&Expression>> = slots
            .iter()
            .map(|s| match s {
                Slot::Val(e) => Some(*e),
                _ => None,
            })
            .collect();
        let shape = vals.iter().flatten().next().copied().expect("a value");
        match shape {
            Expression::Array(first) => {
                let same_len = vals
                    .iter()
                    .flatten()
                    .all(|e| matches!(e, Expression::Array(a) if a.len() == first.len()));
                if !same_len {
                    return Ok(None);
                }
                let mut items = Vec::new();
                for i in 0..first.len() {
                    let slots = child(&vals, |e| match e {
                        Expression::Array(a) => a.get(i),
                        _ => None,
                    });
                    if let Some(x) = self.unify_expr(&slots, &format!("{hint}_{i}"))? {
                        items.push(x);
                    }
                }
                Ok(Some(Expression::Array(items)))
            }
            Expression::Object(first) => {
                let keys: Vec<&ObjectKey> = first.keys().collect();
                let same_keys = vals.iter().flatten().all(|e| {
                    matches!(e, Expression::Object(o) if o.len() == keys.len() && keys.iter().all(|k| o.contains_key(*k)))
                });
                if !same_keys {
                    return Ok(None);
                }
                let mut out = Object::new();
                for k in keys {
                    let slots = child(&vals, |e| match e {
                        Expression::Object(o) => o.get(k),
                        _ => None,
                    });
                    let key_text = match k {
                        ObjectKey::Identifier(i) => i.to_string(),
                        ObjectKey::Expression(Expression::String(s)) => s.clone(),
                        ObjectKey::Expression(other) => other.to_string(),
                        _ => String::new(),
                    };
                    if let Some(x) = self.unify_expr(&slots, &format!("{hint}.{key_text}"))? {
                        out.insert(k.clone(), x);
                    }
                }
                Ok(Some(Expression::Object(out)))
            }
            Expression::FuncCall(f) => {
                let same_call = vals.iter().flatten().all(|e| {
                    matches!(e, Expression::FuncCall(g) if g.name == f.name && g.args.len() == f.args.len() && g.expand_final == f.expand_final)
                });
                if !same_call {
                    return Ok(None);
                }
                let mut call = (**f).clone();
                for i in 0..f.args.len() {
                    let slots = child(&vals, |e| match e {
                        Expression::FuncCall(g) => g.args.get(i),
                        _ => None,
                    });
                    let arg_hint = if f.args.len() == 1 {
                        hint.to_string()
                    } else {
                        format!("{hint}_{i}")
                    };
                    call.args[i] = self.unify_expr(&slots, &arg_hint)?.unwrap_or(Expression::Null);
                }
                Ok(Some(Expression::FuncCall(Box::new(call))))
            }
            Expression::Parenthesis(_) => {
                if !vals
                    .iter()
                    .flatten()
                    .all(|e| matches!(e, Expression::Parenthesis(_)))
                {
                    return Ok(None);
                }
                let slots = child(&vals, |e| match e {
                    Expression::Parenthesis(inner) => Some(inner),
                    _ => None,
                });
                Ok(self
                    .unify_expr(&slots, hint)?
                    .map(|x| Expression::Parenthesis(Box::new(x))))
            }
            _ => Ok(None),
        }
    }

    /// Merge one block as the environments have it (`None`: the environment does not
    /// have it, so whatever it holds is never read there).
    fn unify_block(&mut self, blocks: &[Option<&Block>], path: &str) -> Result<Block, ()> {
        let first = blocks
            .iter()
            .flatten()
            .next()
            .copied()
            .expect("some environment has it");
        let mut out = Block::builder(first.identifier.clone());
        for l in first.labels.iter() {
            out = out.add_label(l.clone());
        }
        // Keys and nested block names, in the order the environments first mention them.
        let mut order: Vec<(bool, String)> = Vec::new();
        for b in blocks.iter().flatten() {
            for s in b.body.iter() {
                let key = match s {
                    Structure::Attribute(a) => (true, a.key().to_string()),
                    Structure::Block(nb) => (false, nb.identifier.to_string()),
                };
                if !order.contains(&key) {
                    order.push(key);
                }
            }
        }
        let prefix = |name: &str| {
            if path.is_empty() {
                name.to_string()
            } else {
                format!("{path}.{name}")
            }
        };
        let mut failed = false;
        for (is_attr, key) in order {
            if is_attr {
                let slots: Vec<Slot> = blocks
                    .iter()
                    .map(|b| match b {
                        None => Slot::Any,
                        Some(b) => b
                            .body
                            .attributes()
                            .find(|a| a.key() == key)
                            .map(|a| Slot::Val(a.expr()))
                            .unwrap_or(Slot::Unset),
                    })
                    .collect();
                if key == "depends_on" && path.is_empty() {
                    // Ordering only: everything any environment waits for.
                    let mut all: Vec<Expression> = Vec::new();
                    for s in &slots {
                        if let Slot::Val(Expression::Array(items)) = s {
                            for x in items {
                                if !all.contains(x) {
                                    all.push(x.clone());
                                }
                            }
                        }
                    }
                    out = out.add_attribute(("depends_on", Expression::Array(all)));
                    continue;
                }
                match self.unify_expr(&slots, &prefix(&key)) {
                    Ok(Some(x)) => out = out.add_attribute((key.as_str(), x)),
                    Ok(None) => {}
                    Err(()) => failed = true,
                }
                continue;
            }
            let groups: Vec<Option<Vec<&Block>>> = blocks
                .iter()
                .map(|b| {
                    b.map(|b| {
                        b.body
                            .blocks()
                            .filter(|nb| nb.identifier.as_str() == key)
                            .collect()
                    })
                })
                .collect();
            let counts: Vec<usize> = groups.iter().flatten().map(|g| g.len()).collect();
            let n = counts[0];
            if counts.iter().all(|c| *c == n) {
                for i in 0..n {
                    let inner: Vec<Option<&Block>> =
                        groups.iter().map(|g| g.as_ref().map(|g| g[i])).collect();
                    let sub = if n == 1 {
                        prefix(&key)
                    } else {
                        format!("{}_{i}", prefix(&key))
                    };
                    match self.unify_block(&inner, &sub) {
                        Ok(b) => out = out.add_block(b),
                        Err(()) => failed = true,
                    }
                }
                continue;
            }
            let labelled = groups.iter().flatten().flatten().any(|b| !b.labels.is_empty());
            if counts.iter().all(|c| *c <= 1) && !labelled {
                // Present in some environments only: a `dynamic` block behind a gate.
                let present: Vec<Option<bool>> =
                    groups.iter().map(|g| g.as_ref().map(|g| !g.is_empty())).collect();
                let cond = self.gate(&present, &prefix(&key));
                let inner: Vec<Option<&Block>> = groups
                    .iter()
                    .map(|g| g.as_ref().and_then(|g| g.first().copied()))
                    .collect();
                match self.unify_block(&inner, &prefix(&key)) {
                    Ok(content) => {
                        let mut body = Block::builder("content");
                        for s in content.body.iter() {
                            body = body.add_structure(s.clone());
                        }
                        let for_each = Expression::Conditional(Box::new(Conditional::new(
                            cond,
                            Expression::Array(vec![Expression::Number(1.into())]),
                            Expression::Array(vec![]),
                        )));
                        out = out.add_block(
                            Block::builder("dynamic")
                                .add_label(key.as_str())
                                .add_attribute(("for_each", for_each))
                                .add_block(body.build())
                                .build(),
                        );
                    }
                    Err(()) => failed = true,
                }
                continue;
            }
            let per: Vec<String> = self
                .envs()
                .iter()
                .zip(&groups)
                .filter_map(|(e, g)| g.as_ref().map(|g| format!("{e}: {}", g.len())))
                .collect();
            self.problem(format!(
                "`{}` has a different number of `{key}` blocks per environment ({}): a repeated nested block's count cannot differ between environments",
                self.address,
                per.join(", ")
            ));
            failed = true;
        }
        if failed {
            Err(())
        } else {
            Ok(out.build())
        }
    }
}

/// One part of each environment's value (an element, a key, an argument), as slots.
fn child<'a>(
    vals: &[Option<&'a Expression>],
    pick: impl Fn(&'a Expression) -> Option<&'a Expression>,
) -> Vec<Slot<'a>> {
    vals.iter()
        .map(|v| match v {
            None => Slot::Any,
            Some(e) => pick(e).map(Slot::Val).unwrap_or(Slot::Unset),
        })
        .collect()
}

/// The text of a plain value (`50`, `"50"`, `true`), for comparing across types.
fn literal_text(e: &Expression) -> Option<String> {
    match e {
        Expression::String(s) => Some(s.clone()),
        Expression::Number(n) => Some(n.to_string()),
        Expression::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Escape text for the inside of an HCL quoted template, keeping its `${var.…}`.
fn escape_template(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while !rest.is_empty() {
        if rest.starts_with("${var.") {
            if let Some(end) = rest.find('}') {
                out.push_str(&rest[..=end]);
                rest = &rest[end + 1..];
                continue;
            }
        }
        let c = rest.chars().next().unwrap();
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '$' if rest[1..].starts_with('{') => out.push_str("$$"),
            '%' if rest[1..].starts_with('{') => out.push_str("%%"),
            c => out.push(c),
        }
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// `"/zipos/${var.environment}/web"` as an HCL template.
fn template(t: &str) -> Expression {
    emit::raw_expr(&format!("\"{}\"", escape_template(t)))
}

/// The fields of one entity whose values differ between the environments it exists in.
fn differing_fields(resolved: &[Project], cat: &Catalog, provider: &str, id: &str) -> Vec<DiffField> {
    let ents: Vec<Option<ttg_core::EntityRef>> = resolved.iter().map(|p| p.entity(id)).collect();
    let Some(any) = ents.iter().flatten().next() else {
        return Vec::new();
    };
    let Some(def) = cat.resource(any.resource_type) else {
        return Vec::new();
    };
    let abstract_fields = def.fields.iter().map(|f| (f, false));
    let provider_fields = def
        .providers
        .get(provider)
        .into_iter()
        .flat_map(|m| m.fields.iter().map(|f| (f, true)));
    let mut out = Vec::new();
    for (f, is_provider) in abstract_fields.chain(provider_fields) {
        let raw: Vec<Option<Option<Value>>> = ents
            .iter()
            .map(|e| {
                e.as_ref()
                    .map(|e| diagnostics::field_or_default(cat, provider, e, &f.name, is_provider))
            })
            .collect();
        let present: Vec<&Option<Value>> = raw.iter().flatten().collect();
        if present.iter().all(|v| *v == present[0]) {
            continue;
        }
        let values = raw
            .iter()
            .map(|v| match v {
                Some(Some(v)) => Some(value_expr(v)),
                Some(None) if f.field_type == FieldType::Bool => Some(Expression::Bool(false)),
                _ => None,
            })
            .collect();
        let options = if f.field_type == FieldType::Enum {
            f.options.clone()
        } else {
            Vec::new()
        };
        out.push(DiffField {
            name: f.name.clone(),
            values,
            options,
        });
    }
    out
}

/// The variable names a `variables.tf` declares.
fn declared_variables(text: &str) -> HashSet<String> {
    text.lines()
        .filter_map(|l| l.strip_prefix("variable \""))
        .filter_map(|l| l.split('"').next())
        .map(str::to_string)
        .collect()
}

/// The Terraform type of a variable, from the values it takes.
fn infer_type(values: &[&Expression]) -> String {
    let non_null: Vec<&&Expression> = values.iter().filter(|v| !matches!(v, Expression::Null)).collect();
    let all = |f: &dyn Fn(&Expression) -> bool| !non_null.is_empty() && non_null.iter().all(|v| f(v));
    if all(&|v| matches!(v, Expression::Bool(_))) {
        "bool".into()
    } else if all(&|v| matches!(v, Expression::Number(_))) {
        "number".into()
    } else if all(&|v| matches!(v, Expression::String(_))) {
        "string".into()
    } else if all(
        &|v| matches!(v, Expression::Array(a) if a.iter().all(|x| matches!(x, Expression::String(_)))),
    ) {
        "list(string)".into()
    } else if all(
        &|v| matches!(v, Expression::Array(a) if a.iter().all(|x| matches!(x, Expression::Number(_)))),
    ) {
        "list(number)".into()
    } else if all(
        &|v| matches!(v, Expression::Object(o) if o.values().all(|x| matches!(x, Expression::String(_)))),
    ) {
        "map(string)".into()
    } else {
        "any".into()
    }
}

fn variable_block(v: &Lifted) -> Block {
    let mut b = Block::builder("variable")
        .add_label(v.name.as_str())
        .add_attribute(("type", emit::raw_expr(&v.var_type)))
        .add_attribute(("description", v.description.as_str()));
    let all_set = v
        .values
        .iter()
        .all(|(_, x)| x.as_ref().is_some_and(|x| !matches!(x, Expression::Null)));
    if !v.options.is_empty() && v.var_type == "string" && all_set {
        let opts = Expression::Array(v.options.iter().map(|o| Expression::String(o.clone())).collect());
        let cond = FuncCall::builder("contains")
            .arg(opts)
            .arg(var_ref(&v.name))
            .build();
        b = b.add_block(
            Block::builder("validation")
                .add_attribute(("condition", Expression::FuncCall(Box::new(cond))))
                .add_attribute((
                    "error_message",
                    format!("{} must be one of: {}.", v.name, v.options.join(", ")),
                ))
                .build(),
        );
    }
    b.build()
}

/// `environments/<env>.tfvars`.
fn render_tfvars(header: &str, env: &str, vars: &[Lifted], nulls: &[String]) -> String {
    let mut out =
        format!("{header}# Values for the \"{env}\" environment: `plan -var-file={DIR}/{env}.tfvars`.\n\n");
    let mut body = hcl::Body::builder();
    for v in vars {
        body = body.add_attribute((v.name.as_str(), v.value(env)));
    }
    for n in nulls {
        body = body.add_attribute((n.as_str(), Expression::Null));
    }
    let text = hcl::format::to_string(&body.build()).expect("hcl formatting cannot fail for a built tree");
    out.push_str(&files::align_attributes(&text));
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// The README's section on environments.
fn readme_section(p: &Project, tool: Tool, vars: &[Lifted]) -> String {
    let bin = Profile::new(tool).binary();
    let envs = &p.settings.environments;
    let first = &envs[0];
    let mut s = format!(
        "\n## Environments\n\n\
         This configuration serves {} environments — {} — from one set of files. What differs \
         between them is in variables, with one value per environment in \
         `{DIR}/<environment>.tfvars`, and each environment keeps a state of its own, whose key is in \
         `{DIR}/<environment>.backend.hcl`.\n\n\
         | Environment | Values | State |\n|---|---|---|\n",
        envs.len(),
        envs.iter()
            .map(|e| format!("`{e}`"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    for e in envs {
        let (arg, value) = crate::state::environment_arg_value(p, e);
        s.push_str(&format!(
            "| {e} | `{DIR}/{e}.tfvars` | `{DIR}/{e}.backend.hcl`: `{arg} = \"{value}\"` |\n"
        ));
    }
    s.push_str(&format!(
        "\nWork on one environment at a time. `init -reconfigure` with that environment's backend file \
         is what keeps one environment's state away from another's, so run it whenever you switch:\n\n\
         ```\n{bin} init -reconfigure -backend-config={DIR}/{first}.backend.hcl\n\
         {bin} plan -var-file={DIR}/{first}.tfvars -out={first}.tfplan\n{bin} apply {first}.tfplan\n```\n\n\
         The variables have no defaults, so a plan without `-var-file` asks for them rather than \
         picking an environment for you.\n"
    ));
    s.push_str(&format!(
        "\n| Variable | Used by | {} |\n|---|---|{}\n",
        envs.join(" | "),
        "---|".repeat(envs.len())
    ));
    for v in vars {
        let cells: Vec<String> = envs.iter().map(|e| format!("`{}`", v.value(e))).collect();
        let used = if v.address.is_empty() {
            "project variable".to_string()
        } else {
            format!("`{}`", v.address)
        };
        s.push_str(&format!("| `{}` | {used} | {} |\n", v.name, cells.join(" | ")));
    }
    s.push_str(&format!(
        "\nThe files in `{DIR}/` are regenerated on every export: change a value in the diagram (the \
         environment selector, then the field), not in the file. For a value of your own on top, pass a \
         second file after it (`-var-file={DIR}/{first}.tfvars -var-file=mine.tfvars`; later files win) \
         or use a `*.auto.tfvars` file, which the export never touches.\n"
    ));
    s
}

// ------------------------------------------------------------------ references to counted resources

/// `type.local` (or `data.type.local`) of a traversal, and where its attribute path starts.
fn traversal_target(t: &Traversal) -> Option<(String, usize)> {
    let Expression::Variable(v) = &t.expr else {
        return None;
    };
    let attr = |i: usize| match t.operators.get(i) {
        Some(TraversalOperator::GetAttr(a)) => Some(a.to_string()),
        _ => None,
    };
    if v.as_str() == "data" {
        Some((format!("data.{}.{}", attr(0)?, attr(1)?), 2))
    } else {
        Some((format!("{}.{}", v.as_str(), attr(0)?), 1))
    }
}

/// Rewrite references to counted resources: `aws_nat_gateway.nat_b.id` becomes
/// `one(aws_nat_gateway.nat_b[*].id)`. A bare `aws_nat_gateway.nat_b` (a `depends_on`
/// entry) is left alone: Terraform orders on a counted resource as a whole. A template
/// string can only be indexed (`[0]`); a heredoc that names one is reported.
fn rewrite_counted(e: &mut Expression, counted: &HashSet<String>, bad: &mut Vec<String>) {
    match e {
        Expression::Traversal(t) => {
            rewrite_counted(&mut t.expr, counted, bad);
            for op in t.operators.iter_mut() {
                if let TraversalOperator::Index(x) = op {
                    rewrite_counted(x, counted, bad);
                }
            }
            if let Some((key, at)) = traversal_target(t) {
                let indexed = matches!(
                    t.operators.get(at),
                    Some(
                        TraversalOperator::Index(_)
                            | TraversalOperator::FullSplat
                            | TraversalOperator::AttrSplat
                    )
                );
                if counted.contains(&key) && t.operators.len() > at && !indexed {
                    let mut ops = t.operators.clone();
                    ops.insert(at, TraversalOperator::FullSplat);
                    let splat = Expression::Traversal(Box::new(Traversal::new(t.expr.clone(), ops)));
                    *e = Expression::FuncCall(Box::new(FuncCall::builder("one").arg(splat).build()));
                }
            }
        }
        Expression::Array(items) => items.iter_mut().for_each(|x| rewrite_counted(x, counted, bad)),
        Expression::Object(o) => o.values_mut().for_each(|x| rewrite_counted(x, counted, bad)),
        Expression::FuncCall(f) => f.args.iter_mut().for_each(|x| rewrite_counted(x, counted, bad)),
        Expression::Parenthesis(x) => rewrite_counted(x, counted, bad),
        Expression::Conditional(c) => {
            rewrite_counted(&mut c.cond_expr, counted, bad);
            rewrite_counted(&mut c.true_expr, counted, bad);
            rewrite_counted(&mut c.false_expr, counted, bad);
        }
        Expression::Operation(op) => match op.as_mut() {
            hcl::expr::Operation::Unary(u) => rewrite_counted(&mut u.expr, counted, bad),
            hcl::expr::Operation::Binary(b) => {
                rewrite_counted(&mut b.lhs_expr, counted, bad);
                rewrite_counted(&mut b.rhs_expr, counted, bad);
            }
        },
        Expression::ForExpr(f) => {
            rewrite_counted(&mut f.collection_expr, counted, bad);
            rewrite_counted(&mut f.value_expr, counted, bad);
            if let Some(k) = &mut f.key_expr {
                rewrite_counted(k, counted, bad);
            }
            if let Some(c) = &mut f.cond_expr {
                rewrite_counted(c, counted, bad);
            }
        }
        Expression::TemplateExpr(t) => {
            if let hcl::expr::TemplateExpr::QuotedString(s) = t.as_mut() {
                for key in counted {
                    let needle = format!("{key}.");
                    if s.contains(&needle) {
                        *s = s.replace(&needle, &format!("{key}[0]."));
                    }
                }
            } else {
                let text = t.to_string();
                bad.extend(
                    counted
                        .iter()
                        .filter(|k| text.contains(&format!("{k}.")))
                        .cloned(),
                );
            }
        }
        _ => {}
    }
}

fn rewrite_block(b: &mut Block, counted: &HashSet<String>, bad: &mut Vec<String>) {
    for s in b.body.iter_mut() {
        match s {
            Structure::Attribute(a) => rewrite_counted(&mut a.expr, counted, bad),
            Structure::Block(nb) => rewrite_block(nb, counted, bad),
        }
    }
}

/// Put `count = <cond> ? 1 : 0` first in a block.
fn with_count(b: Block, cond: Expression) -> Block {
    let mut out = Block::builder(b.identifier.clone());
    for l in b.labels.iter() {
        out = out.add_label(l.clone());
    }
    let count = Expression::Conditional(Box::new(Conditional::new(
        cond,
        Expression::Number(1.into()),
        Expression::Number(0.into()),
    )));
    out = out.add_attribute(("count", count));
    for s in b.body.into_iter() {
        out = out.add_structure(s);
    }
    out.build()
}

/// Insert ids in a merged order: each list's ids in its order, a new id after the last
/// one before it that is already placed.
fn merged_order<'a>(lists: impl Iterator<Item = Vec<&'a String>>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for list in lists {
        let mut at = 0usize;
        for x in list {
            match out.iter().position(|y| y == x) {
                Some(i) => at = i + 1,
                None => {
                    out.insert(at, x.clone());
                    at += 1;
                }
            }
        }
    }
    out
}

// ------------------------------------------------------------------ merge

/// Merge the per-environment exports into one; the problems that prevent it otherwise.
fn merge(
    p: &Project,
    cat: &Catalog,
    provider: &str,
    tool: Tool,
    resolved: &[Project],
    gens: &[(String, Generated)],
) -> Result<Generated, Vec<Diagnostic>> {
    let envs: Vec<String> = gens.iter().map(|(e, _)| e.clone()).collect();
    let base = &gens[0].1;
    let facts = EnvFacts {
        envs: envs.clone(),
        vars: envs.iter().map(|e| p.variable_values(Some(e))).collect(),
    };
    let pdef = cat
        .provider(provider)
        .ok_or_else(|| vec![simple_problem(None, format!("unknown provider {provider}"))])?;
    let header = Profile::new(tool).file_header(&pdef.provider.display_name);
    let name_of = |id: &str| p.entity(id).map(|e| e.name.to_string()).unwrap_or(id.to_string());

    // Every variable any environment's root declares.
    let mut taken: HashSet<String> = HashSet::new();
    for (_, g) in gens {
        taken.extend(declared_variables(
            g.files.get("variables.tf").map(String::as_str).unwrap_or(""),
        ));
    }
    let mut lifter = Lifter {
        facts: &facts,
        taken,
        vars: IndexMap::new(),
        project_vars: BTreeSet::new(),
        typed_vars: BTreeMap::new(),
        problems: Vec::new(),
        entity: None,
        entity_name: String::new(),
        slug: String::new(),
        local: String::new(),
        address: String::new(),
        fields: Vec::new(),
        templates: Vec::new(),
    };
    for n in p
        .settings
        .variables
        .keys()
        .chain([&"name_prefix".to_string(), &ENVIRONMENT_VAR.to_string()])
    {
        if lifter.taken.contains(n) {
            lifter.problems.push((
                None,
                format!(
                    "the project variable \"{n}\" has the name of a variable the export already declares; rename it"
                ),
            ));
        }
    }

    // ---- the entities' blocks, block by block
    let ids = merged_order(gens.iter().map(|(_, g)| g.entity_blocks.keys().collect()));
    let mut entity_blocks: IndexMap<Id, EntityBlocks> = IndexMap::new();
    let mut counted: HashSet<String> = HashSet::new();
    for id in &ids {
        let per: Vec<Option<&EntityBlocks>> = gens.iter().map(|(_, g)| g.entity_blocks.get(id)).collect();
        let first = per
            .iter()
            .flatten()
            .next()
            .copied()
            .expect("listed by some environment");
        let addresses = merged_order(per.iter().flatten().map(|b| b.addresses.iter().collect()));
        lifter.entity = Some(id.clone());
        lifter.entity_name = name_of(id);
        lifter.slug = p.hcl_name(id);
        lifter.fields = differing_fields(resolved, cat, provider, id);
        lifter.templates = p.entity(id).map(|e| entity_strings(&e)).unwrap_or_default();
        let mut blocks = Vec::new();
        for addr in &addresses {
            let per_env: Vec<Option<&Block>> = per
                .iter()
                .map(|b| {
                    b.and_then(|b| {
                        let i = b.addresses.iter().position(|a| a == addr)?;
                        b.blocks.get(i)
                    })
                })
                .collect();
            let template = per_env
                .iter()
                .flatten()
                .next()
                .copied()
                .expect("some environment has it");
            lifter.address = addr.clone();
            lifter.local = template
                .labels
                .get(1)
                .map(|l| l.as_str().to_string())
                .unwrap_or_default();
            let same = per_env.iter().all(|b| b.is_some_and(|b| b == template));
            let merged = if same {
                Ok(template.clone())
            } else {
                lifter.unify_block(&per_env, "")
            };
            let Ok(mut merged) = merged else { continue };
            if per_env.iter().any(Option::is_none) {
                // Some environments leave this resource out: count it.
                if merged
                    .body
                    .attributes()
                    .any(|a| a.key() == "count" || a.key() == "for_each")
                {
                    lifter.problem(format!(
                        "`{addr}` exists in some environments only and already sets count or for_each"
                    ));
                    continue;
                }
                let present: Vec<Option<bool>> = per_env.iter().map(|b| Some(b.is_some())).collect();
                let cond = lifter.gate(&present, "enabled");
                merged = with_count(merged, cond);
                counted.insert(addr.clone());
            }
            blocks.push(merged);
        }
        entity_blocks.insert(
            id.clone(),
            EntityBlocks {
                file: first.file.clone(),
                addresses,
                lines: (0, 0),
                comment: first.comment.clone(),
                blocks,
            },
        );
    }

    // ---- the files no entity owns
    lifter.entity = None;
    lifter.entity_name = String::new();
    lifter.slug = String::new();
    lifter.fields = Vec::new();
    lifter.templates = p
        .settings
        .tags
        .values()
        .filter(|t| t.contains("${var."))
        .cloned()
        .collect();
    let resource_files: HashSet<String> = entity_blocks.values().map(|b| b.file.clone()).collect();
    let names = merged_order(gens.iter().map(|(_, g)| g.files.keys().collect()));
    let mut other_files: IndexMap<String, String> = IndexMap::new();
    let mut partial_vars: Vec<(String, Vec<bool>)> = Vec::new();
    for n in &names {
        if resource_files.contains(n.as_str()) || n == "README.md" || n == "MANUAL_STEPS.md" {
            continue;
        }
        let texts: Vec<Option<&String>> = gens.iter().map(|(_, g)| g.files.get(n)).collect();
        if texts.iter().all(|t| t == &texts[0]) {
            other_files.insert(n.clone(), texts[0].cloned().unwrap_or_default());
            continue;
        }
        let top_level_tf = n.ends_with(".tf") && !n.contains('/');
        if !top_level_tf {
            let what = if n.starts_with(crate::k8s::DIR) {
                "the Kubernetes manifests cannot differ between environments yet (a workload field overridden per environment, or a workload left out of one?)"
            } else if n.starts_with(crate::state::BOOTSTRAP_DIR) {
                "the bootstrap root is shared by every environment"
            } else {
                "only the Terraform configuration can differ between environments"
            };
            lifter
                .problems
                .push((None, format!("`{n}` would differ between environments: {what}")));
            continue;
        }
        match merge_file(&mut lifter, &texts, &header) {
            Ok((text, partial)) => {
                other_files.insert(n.clone(), text);
                partial_vars.extend(partial);
            }
            Err(msg) => lifter.problems.push((None, format!("`{n}`: {msg}"))),
        }
    }

    // ---- references to the resources some environments leave out
    let mut bad = Vec::new();
    if !counted.is_empty() {
        for eb in entity_blocks.values_mut() {
            for b in &mut eb.blocks {
                rewrite_block(b, &counted, &mut bad);
            }
        }
        if let Some(text) = other_files.get("outputs.tf").cloned() {
            match hcl::parse(&text) {
                Ok(body) => {
                    let mut out = header.clone();
                    for mut b in body.into_blocks() {
                        rewrite_block(&mut b, &counted, &mut bad);
                        out.push('\n');
                        out.push_str(&files::fmt_block(&b));
                    }
                    other_files.insert("outputs.tf".into(), out);
                }
                Err(e) => lifter
                    .problems
                    .push((None, format!("outputs.tf could not be re-read: {e}"))),
            }
        }
    }
    bad.sort();
    bad.dedup();
    for key in bad {
        lifter.problems.push((
            None,
            format!(
                "`{key}` exists in some environments only, and a heredoc template refers to it: write the reference as one({key}[*].<attribute>)"
            ),
        ));
    }

    if !lifter.problems.is_empty() {
        return Err(lifter
            .problems
            .into_iter()
            .map(|(entity, message)| simple_problem(entity, message))
            .collect());
    }

    // ---- the variables: project variables used, the environment, the lifted values
    let mut lifted: Vec<Lifted> = Vec::new();
    lifted.push(Lifted {
        name: ENVIRONMENT_VAR.into(),
        var_type: "string".into(),
        description: format!(
            "The environment this configuration is applied for: one of {}.",
            envs.join(", ")
        ),
        entity: None,
        address: String::new(),
        argument: String::new(),
        field: Some(ENVIRONMENT_VAR.into()),
        values: envs
            .iter()
            .map(|e| (e.clone(), Some(Expression::String(e.clone()))))
            .collect(),
        options: envs.clone(),
    });
    for name in &lifter.project_vars {
        if name == ENVIRONMENT_VAR {
            continue;
        }
        let description = match p.settings.variables.get(name) {
            Some(v) if !v.description.is_empty() => v.description.clone(),
            Some(_) => format!("Project variable `{name}`."),
            None => {
                "The name prefix every generated resource name starts with (Settings ▸ Environments).".into()
            }
        };
        lifted.push(Lifted {
            name: name.clone(),
            var_type: String::new(),
            description,
            entity: None,
            address: String::new(),
            argument: String::new(),
            field: Some(name.clone()),
            values: envs
                .iter()
                .zip(&facts.vars)
                .enumerate()
                .map(|(i, (e, vals))| {
                    let typed = lifter.typed_vars.get(name).and_then(|t| t[i].clone());
                    (e.clone(), typed.or_else(|| vals.get(name).map(value_expr)))
                })
                .collect(),
            options: Vec::new(),
        });
    }
    lifted.extend(lifter.vars.into_values());
    for v in &mut lifted {
        if v.var_type.is_empty() {
            let vals: Vec<&Expression> = v.values.iter().filter_map(|(_, x)| x.as_ref()).collect();
            v.var_type = infer_type(&vals);
        }
    }

    // ---- assemble
    let mut files: IndexMap<String, String> = IndexMap::new();
    let mut entity_blocks = entity_blocks;
    let resource_texts = emit::render_entity_files(&header, &mut entity_blocks);
    // Keep the base's file order: resource files first, then the rest as generated.
    for (name, text) in resource_texts {
        files.insert(name, text);
    }
    for n in &names {
        if let Some(t) = other_files.get(n) {
            files.insert(n.clone(), t.clone());
        } else if n == "README.md" || n == "MANUAL_STEPS.md" {
            if let Some(t) = gens.iter().find_map(|(_, g)| g.files.get(n)) {
                files.insert(n.clone(), t.clone());
            }
        }
    }
    {
        let vars = files.get("variables.tf").cloned().unwrap_or_default();
        let mut text = if vars.contains("# No input variables are required") {
            header.clone()
        } else {
            vars
        };
        text.push_str(
            "\n# Values that differ between environments: see environments/<environment>.tfvars.\n",
        );
        for v in &lifted {
            text.push('\n');
            text.push_str(&files::fmt_block(&variable_block(v)));
        }
        files.insert("variables.tf".into(), text);
    }
    if let Some(readme) = files.get_mut("README.md") {
        readme.push_str(&readme_section(p, tool, &lifted));
    }
    // Manual steps that differ: one file per environment, and a pointer at the top.
    let steps: Vec<Option<&String>> = gens.iter().map(|(_, g)| g.files.get("MANUAL_STEPS.md")).collect();
    if steps.iter().any(|s| s != &steps[0]) {
        for ((env, _), s) in gens.iter().zip(&steps) {
            if let Some(s) = s {
                files.insert(format!("{DIR}/{env}.MANUAL_STEPS.md"), (*s).clone());
            }
        }
        if let Some(top) = files.get_mut("MANUAL_STEPS.md") {
            let note = format!(
                "\n> The manual steps differ between environments. This file is for **{}**; each \
                 environment's are in `{DIR}/<environment>.MANUAL_STEPS.md`.\n",
                envs.iter()
                    .zip(&steps)
                    .find(|(_, s)| s.is_some())
                    .map(|(e, _)| e.as_str())
                    .unwrap_or_default()
            );
            let at = top.find('\n').map(|i| i + 1).unwrap_or(top.len());
            top.insert_str(at, &note);
        }
    }
    for (i, env) in envs.iter().enumerate() {
        let nulls: Vec<String> = partial_vars
            .iter()
            .filter(|(_, present)| !present[i])
            .map(|(n, _)| n.clone())
            .collect();
        files.insert(
            format!("{DIR}/{env}.tfvars"),
            render_tfvars(&header, env, &lifted, &nulls),
        );
        let (arg, value) = crate::state::environment_arg_value(p, env);
        let body = hcl::Body::builder().add_attribute((arg.as_str(), value)).build();
        files.insert(
            format!("{DIR}/{env}.backend.hcl"),
            format!(
                "{header}# The state of the \"{env}\" environment: `init -reconfigure -backend-config={DIR}/{env}.backend.hcl`.\n\n{}",
                hcl::format::to_string(&body).expect("hcl formatting cannot fail for a built tree")
            ),
        );
    }
    Ok(Generated {
        provider: base.provider.clone(),
        provider_display: base.provider_display.clone(),
        tool,
        files,
        manual_steps: base.manual_steps.clone(),
        diagnostics: Vec::new(),
        entity_blocks,
        lifted,
    })
}

fn simple_problem(entity: Option<Id>, message: String) -> Diagnostic {
    Diagnostic {
        entity,
        severity: Severity::Error,
        code: Code::Environment,
        message,
        provider: None,
    }
}

/// Merge one top-level `.tf` file that differs between environments (`outputs.tf`,
/// `providers.tf`, `variables.tf`, `versions.tf`): block by block, keyed by type and
/// labels. The `terraform {}` block is joined (a helper provider only one environment
/// uses is still required); an output only some environments have is null in the
/// others; a variable only some declare is declared, and set to null in the others'
/// `.tfvars` (the second value returned).
#[allow(clippy::type_complexity)]
fn merge_file(
    lifter: &mut Lifter,
    texts: &[Option<&String>],
    header: &str,
) -> Result<(String, Vec<(String, Vec<bool>)>), String> {
    let mut bodies: Vec<Option<Vec<Block>>> = Vec::new();
    for t in texts {
        match t {
            Some(t) => bodies.push(Some(
                hcl::parse(t)
                    .map_err(|e| format!("could not be re-read to merge it: {e}"))?
                    .into_blocks()
                    .collect(),
            )),
            None => bodies.push(None),
        }
    }
    // A provider's aliased configurations share its label; the alias tells them apart.
    let key = |b: &Block| {
        let alias = b
            .body
            .attributes()
            .find(|a| a.key() == "alias")
            .map(|a| format!(" {}", a.expr()))
            .unwrap_or_default();
        format!(
            "{} {}{alias}",
            b.identifier,
            b.labels.iter().map(|l| l.as_str()).collect::<Vec<_>>().join(" ")
        )
    };
    let keys = merged_order(
        bodies
            .iter()
            .map(|b| {
                b.as_ref()
                    .map(|bs| bs.iter().map(&key).collect::<Vec<_>>())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .iter()
            .map(|v| v.iter().collect()),
    );
    let mut out = header.to_string();
    let mut partial = Vec::new();
    for k in keys {
        let per: Vec<Option<&Block>> = bodies
            .iter()
            .map(|bs| bs.as_ref().and_then(|bs| bs.iter().find(|b| key(b) == k)))
            .collect();
        let first = per.iter().flatten().next().copied().expect("listed");
        let merged = if first.identifier.as_str() == "terraform" {
            join_blocks(&per)?
        } else if first.identifier.as_str() == "variable" && per.iter().all(Option::is_some) {
            // A variable block cannot use variables; what may differ is text naming the
            // environment (a description built from a prefixed name).
            neutral_variable(&per, lifter.facts)?
        } else {
            lifter.address = k.clone();
            lifter.local = first
                .labels
                .first()
                .map(|l| ttg_core::slugify(l.as_str()))
                .unwrap_or_else(|| first.identifier.to_string());
            let present: Vec<bool> = per.iter().map(Option::is_some).collect();
            let mut b = lifter
                .unify_block(&per, "")
                .map_err(|_| "differs between environments in a way the export cannot merge".to_string())?;
            if present.iter().any(|p| !p) {
                match first.identifier.as_str() {
                    "output" => {
                        let envs = lifter.envs().to_vec();
                        let on: Vec<&str> = envs
                            .iter()
                            .zip(&present)
                            .filter(|(_, p)| **p)
                            .map(|(e, _)| e.as_str())
                            .collect();
                        let cond = lifter.env_is(&on);
                        b = map_attr(b, "value", |v| {
                            Expression::Conditional(Box::new(Conditional::new(
                                cond.clone(),
                                v,
                                Expression::Null,
                            )))
                        });
                    }
                    "variable" => {
                        if let Some(l) = first.labels.first() {
                            partial.push((l.as_str().to_string(), present.clone()));
                        }
                    }
                    _ => {}
                }
            }
            b
        };
        out.push('\n');
        out.push_str(&files::fmt_block(&merged));
    }
    Ok((out, partial))
}

/// One `variable` block for every environment: its strings with each environment's name
/// prefix written as `<name prefix>` and its name as `<environment>`, which must then
/// agree; anything else that differs is refused.
fn neutral_variable(per: &[Option<&Block>], facts: &EnvFacts) -> Result<Block, String> {
    fn neutral(e: &Expression, env: &str, prefix: &str) -> Expression {
        match e {
            Expression::String(s) => {
                let mut t = s.clone();
                if !prefix.is_empty() {
                    t = t.replace(prefix, "<name prefix>");
                }
                Expression::String(t.replace(env, "<environment>"))
            }
            other => other.clone(),
        }
    }
    let mut out: Option<Block> = None;
    for (i, b) in per.iter().enumerate() {
        let b = b.expect("present everywhere");
        let env = &facts.envs[i];
        let prefix = facts.vars[i]["name_prefix"].display();
        let mut nb = Block::builder(b.identifier.clone());
        for l in b.labels.iter() {
            nb = nb.add_label(l.clone());
        }
        for s in b.body.iter() {
            match s {
                Structure::Attribute(a) => nb = nb.add_attribute((a.key(), neutral(a.expr(), env, &prefix))),
                other => nb = nb.add_structure(other.clone()),
            }
        }
        let nb = nb.build();
        match &out {
            None => out = Some(nb),
            Some(o) if *o == nb => {}
            Some(_) => {
                return Err(format!(
                    "the variable `{}` would be declared differently per environment, and a variable block cannot use variables",
                    b.labels.first().map(|l| l.as_str()).unwrap_or_default()
                ))
            }
        }
    }
    Ok(out.expect("at least one environment"))
}

/// Replace one attribute of a block through `f`.
fn map_attr(b: Block, key: &str, f: impl Fn(Expression) -> Expression) -> Block {
    let mut out = Block::builder(b.identifier.clone());
    for l in b.labels.iter() {
        out = out.add_label(l.clone());
    }
    for s in b.body.into_iter() {
        match s {
            Structure::Attribute(a) if a.key() == key => {
                out = out.add_attribute((key, f(a.expr)));
            }
            other => out = out.add_structure(other),
        }
    }
    out.build()
}

/// The union of blocks that may only differ in what they have, never in a value (the
/// `terraform {}` block: required providers, backend, encryption).
fn join_blocks(per: &[Option<&Block>]) -> Result<Block, String> {
    let first = per.iter().flatten().next().copied().expect("listed");
    let mut out = Block::builder(first.identifier.clone());
    for l in first.labels.iter() {
        out = out.add_label(l.clone());
    }
    let mut attrs: IndexMap<String, Expression> = IndexMap::new();
    let mut nested: IndexMap<String, Vec<&Block>> = IndexMap::new();
    for b in per.iter().flatten() {
        for s in b.body.iter() {
            match s {
                Structure::Attribute(a) => match attrs.get(a.key()) {
                    Some(x) if x != a.expr() => {
                        return Err(format!(
                            "`{}` in the `{}` block differs between environments, and that block cannot use variables",
                            a.key(),
                            first.identifier
                        ))
                    }
                    Some(_) => {}
                    None => {
                        attrs.insert(a.key().to_string(), a.expr().clone());
                    }
                },
                Structure::Block(nb) => {
                    let k = format!(
                        "{} {}",
                        nb.identifier,
                        nb.labels.iter().map(|l| l.as_str()).collect::<Vec<_>>().join(" ")
                    );
                    nested.entry(k).or_default().push(nb);
                }
            }
        }
    }
    for (k, v) in attrs {
        out = out.add_attribute((k.as_str(), v));
    }
    for (_, bs) in nested {
        let per: Vec<Option<&Block>> = bs.into_iter().map(Some).collect();
        out = out.add_block(join_blocks(&per)?);
    }
    Ok(out.build())
}
