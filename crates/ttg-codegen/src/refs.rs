//! References written by hand in extra arguments: `{"$ref": …}` values and the addresses
//! inside `{"$raw": "…"}` expressions.
//!
//! Both become part of the graph. A `$ref` names its target entity; a `$raw` expression
//! is parsed with hcl-rs and every address in it (`aws_s3_bucket.logs.arn`,
//! `data.aws_vpc.main.id`, `var.region`, `local.x`, inside template strings too) is
//! looked up in the export's [`Plan`]. An address the export generates links the
//! entity holding the `$raw` to the entity that generates it — which clears "nothing
//! links to this" and draws a dashed reference edge on the canvas — and one it does not
//! generate is an error diagnostic naming the `$raw` and the address, before `validate`
//! would find it. A `$ref` that names nothing, or a repeated block's key that does not
//! exist, is an error the same way.

use crate::diagnostics::{Code, Diagnostic, Severity};
use crate::plan::{KeySel, Plan};
use hcl::template::{Directive, Element, Template};
use hcl::{Expression, ObjectKey, TraversalOperator};
use std::collections::{HashMap, HashSet};
use ttg_catalog::{Catalog, MappingStatus};
use ttg_core::{Id, Project};

/// How a reference was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// `{"$ref": {"entity": …}}`
    Ref,
    /// An address inside a `{"$raw": "…"}` expression.
    Raw,
}

/// One entity's extra arguments referring to another entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedRef {
    pub source: Id,
    pub target: Id,
    pub via: Via,
    /// Where it is written: `task.container_definitions`.
    pub at: String,
}

/// An address in an HCL expression.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Address {
    Resource {
        ty: String,
        name: String,
    },
    Data {
        ty: String,
        name: String,
    },
    Var(String),
    Local(String),
    Module(String),
    /// A root that is none of the above and not bound by a `for` (a typo, or something
    /// only a hand-written file could declare).
    Unknown(String),
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Address::Resource { ty, name } => write!(f, "{ty}.{name}"),
            Address::Data { ty, name } => write!(f, "data.{ty}.{name}"),
            Address::Var(n) => write!(f, "var.{n}"),
            Address::Local(n) => write!(f, "local.{n}"),
            Address::Module(n) => write!(f, "module.{n}"),
            Address::Unknown(n) => write!(f, "{n}"),
        }
    }
}

/// Roots HCL defines itself.
const BUILTIN_ROOTS: &[&str] = &["each", "count", "self", "path", "terraform"];

/// Parse a `$raw` expression the way the emitter does.
pub fn parse_raw(text: &str) -> Result<Expression, String> {
    let body = hcl::parse(&format!("x = {text}")).map_err(|e| e.to_string())?;
    body.into_attributes()
        .next()
        .map(|a| a.expr)
        .ok_or_else(|| "empty expression".to_string())
}

/// Every address in an expression, in order of appearance, without repeats. Names in
/// `bound` (and those a `for` binds) are local to the expression and are not addresses.
pub fn addresses(expr: &Expression, bound: &[String]) -> Vec<Address> {
    let mut w = Walker {
        scopes: vec![bound.to_vec()],
        out: Vec::new(),
    };
    w.expr(expr);
    let mut seen = HashSet::new();
    w.out.retain(|a| seen.insert(a.clone()));
    w.out
}

struct Walker {
    scopes: Vec<Vec<String>>,
    out: Vec<Address>,
}

impl Walker {
    fn bound(&self, name: &str) -> bool {
        self.scopes.iter().any(|s| s.iter().any(|x| x == name))
    }

    fn expr(&mut self, e: &Expression) {
        match e {
            Expression::Null | Expression::Bool(_) | Expression::Number(_) | Expression::String(_) => {}
            Expression::Array(items) => items.iter().for_each(|x| self.expr(x)),
            Expression::Object(o) => {
                for (k, v) in o {
                    if let ObjectKey::Expression(k) = k {
                        self.expr(k);
                    }
                    self.expr(v);
                }
            }
            Expression::TemplateExpr(t) => {
                if let Ok(t) = Template::from_expr(t) {
                    self.template(&t);
                }
            }
            Expression::Variable(v) => self.root(v.as_str(), &[]),
            Expression::Traversal(t) => {
                match &t.expr {
                    Expression::Variable(v) => self.root(v.as_str(), &t.operators),
                    other => self.expr(other),
                }
                for op in &t.operators {
                    if let TraversalOperator::Index(i) = op {
                        self.expr(i);
                    }
                }
            }
            Expression::FuncCall(f) => f.args.iter().for_each(|x| self.expr(x)),
            Expression::Parenthesis(x) => self.expr(x),
            Expression::Conditional(c) => {
                self.expr(&c.cond_expr);
                self.expr(&c.true_expr);
                self.expr(&c.false_expr);
            }
            Expression::Operation(op) => match op.as_ref() {
                hcl::expr::Operation::Unary(u) => self.expr(&u.expr),
                hcl::expr::Operation::Binary(b) => {
                    self.expr(&b.lhs_expr);
                    self.expr(&b.rhs_expr);
                }
            },
            Expression::ForExpr(f) => {
                self.expr(&f.collection_expr);
                let mut scope = vec![f.value_var.to_string()];
                if let Some(k) = &f.key_var {
                    scope.push(k.to_string());
                }
                self.scopes.push(scope);
                if let Some(k) = &f.key_expr {
                    self.expr(k);
                }
                self.expr(&f.value_expr);
                if let Some(c) = &f.cond_expr {
                    self.expr(c);
                }
                self.scopes.pop();
            }
            // The enum is non-exhaustive; a later variant would hold nothing to find.
            _ => {}
        }
    }

    fn template(&mut self, t: &Template) {
        for el in t.elements() {
            match el {
                Element::Literal(_) => {}
                Element::Interpolation(i) => self.expr(&i.expr),
                Element::Directive(d) => match d.as_ref() {
                    Directive::If(i) => {
                        self.expr(&i.cond_expr);
                        self.template(&i.true_template);
                        if let Some(f) = &i.false_template {
                            self.template(f);
                        }
                    }
                    Directive::For(f) => {
                        self.expr(&f.collection_expr);
                        let mut scope = vec![f.value_var.to_string()];
                        if let Some(k) = &f.key_var {
                            scope.push(k.to_string());
                        }
                        self.scopes.push(scope);
                        self.template(&f.template);
                        self.scopes.pop();
                    }
                },
            }
        }
    }

    fn root(&mut self, root: &str, ops: &[TraversalOperator]) {
        if self.bound(root) || BUILTIN_ROOTS.contains(&root) {
            return;
        }
        let attr = |i: usize| match ops.get(i) {
            Some(TraversalOperator::GetAttr(a)) => Some(a.to_string()),
            _ => None,
        };
        let a = match root {
            "data" => match (attr(0), attr(1)) {
                (Some(ty), Some(name)) => Address::Data { ty, name },
                _ => Address::Unknown("data".into()),
            },
            "var" => attr(0)
                .map(Address::Var)
                .unwrap_or(Address::Unknown("var".into())),
            "local" => attr(0)
                .map(Address::Local)
                .unwrap_or(Address::Unknown("local".into())),
            "module" => attr(0)
                .map(Address::Module)
                .unwrap_or(Address::Unknown("module".into())),
            // Every resource type is `<provider>_<name>`.
            r if r.contains('_') => match attr(0) {
                Some(name) => Address::Resource {
                    ty: r.to_string(),
                    name,
                },
                None => Address::Unknown(r.to_string()),
            },
            r => Address::Unknown(r.to_string()),
        };
        self.out.push(a);
    }
}

/// An entity by id, or else by display name (case-insensitive) — how `$ref` names it.
pub fn resolve_entity(p: &Project, key: &str) -> Option<Id> {
    p.entity(key).map(|e| e.id.to_string()).or_else(|| {
        p.entities()
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case(key))
            .map(|e| e.id.to_string())
    })
}

/// Replace each old index-based address in `text` (`aws_ecr_repository.ecr_repo_0`) by
/// the address the block has now. Only whole addresses are replaced: `ecr_repo_1` does
/// not match inside `ecr_repo_10`.
pub fn rewrite_legacy(text: &str, legacy: &HashMap<String, String>) -> String {
    if legacy.is_empty() {
        return text.to_string();
    }
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let mut out = text.to_string();
    let mut olds: Vec<&String> = legacy.keys().collect();
    // Longest first, so `x_10` is handled before `x_1` could be a prefix of it.
    olds.sort_by_key(|o| std::cmp::Reverse(o.len()));
    for old in olds {
        let new = &legacy[old];
        let mut result = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(i) = rest.find(old.as_str()) {
            let before = rest[..i].chars().last().or_else(|| result.chars().last());
            let after = rest[i + old.len()..].chars().next();
            let whole = !before.is_some_and(|c| ident(c) || c == '.') && !after.is_some_and(ident);
            result.push_str(&rest[..i]);
            result.push_str(if whole { new } else { old });
            rest = &rest[i + old.len()..];
        }
        result.push_str(rest);
        out = result;
    }
    out
}

/// What [`check`] found.
#[derive(Debug, Default)]
pub struct Checked {
    pub diagnostics: Vec<Diagnostic>,
    pub refs: Vec<DerivedRef>,
}

/// Check every `$ref` and `$raw` in the extra arguments of the entities the export
/// generates. `p` is the provider's layer and `plan` its plan; `full` is the whole
/// project, to tell "off this layer" from "does not exist".
pub fn check(full: &Project, p: &Project, cat: &Catalog, provider: &str, plan: &Plan) -> Checked {
    let mut c = Checker {
        full,
        p,
        cat,
        provider,
        plan,
        provider_name: cat
            .provider(provider)
            .map(|d| d.provider.display_name.clone())
            .unwrap_or(provider.to_string()),
        addresses: plan.addresses(),
        legacy: plan.legacy_addresses(),
        vars: declared_vars(p, cat, provider, plan),
        out: Checked::default(),
        source: String::new(),
    };
    for e in p.entities() {
        let Some(extras) = e.extra.get(provider) else {
            continue;
        };
        let generated = |key: &String| {
            let k = (e.id.to_string(), key.clone());
            plan.blocks.contains_key(&k) || plan.data.contains_key(&k)
        };
        c.source = e.id.to_string();
        for (block, args) in extras {
            if !generated(block) {
                continue;
            }
            for (arg, v) in args {
                c.value(v, &format!("{block}.{arg}"));
            }
        }
    }
    c.out
}

/// Every `$ref` / `$raw` reference between entities on a provider's layer, for the
/// canvas (dashed edges) and anything else that wants the links extra arguments make.
pub fn derived_references(full: &Project, cat: &Catalog, provider: &str) -> Vec<DerivedRef> {
    let layer = crate::layers::project_for(full, cat, provider);
    let skip = bootstrap_key(&layer, cat, provider);
    let plan = Plan::build(&layer, cat, provider, skip.as_deref());
    check(full, &layer, cat, provider, &plan).refs
}

/// The Encryption Key the bootstrap root creates, which the main root treats as
/// external (see `state`).
pub(crate) fn bootstrap_key(layer: &Project, cat: &Catalog, provider: &str) -> Option<Id> {
    crate::state::encryption_plan(layer, cat, provider, layer.settings.tool)
        .filter(|e| e.key_in_bootstrap)
        .and_then(|e| e.key)
}

/// `var.<name>` values the export can declare: the provider's variables, the state
/// passphrase, the variables of the mappings it emits, and the per-entity variables
/// (`<slug>_<name>`: entity variables and the stand-ins for external entities).
fn declared_vars(p: &Project, cat: &Catalog, provider: &str, plan: &Plan) -> VarSet {
    let mut names: HashSet<String> = HashSet::new();
    if let Some(pdef) = cat.provider(provider) {
        names.extend(pdef.variables.iter().map(|v| v.name.clone()));
    }
    if p.settings.state_encryption {
        names.insert(crate::state::PASSPHRASE_VAR.to_string());
    }
    let emitted: HashSet<&Id> = plan
        .blocks
        .keys()
        .chain(plan.data.keys())
        .map(|(id, _)| id)
        .collect();
    for id in emitted {
        if let Some(m) = p.entity(id).and_then(|e| cat.mapping(e.resource_type, provider)) {
            names.extend(m.variables.iter().map(|v| v.name.clone()));
        }
    }
    let prefixes = p
        .entities()
        .iter()
        .map(|e| format!("{}_", ttg_core::slugify(e.name)))
        .collect();
    VarSet { names, prefixes }
}

struct VarSet {
    names: HashSet<String>,
    prefixes: Vec<String>,
}

impl VarSet {
    fn contains(&self, name: &str) -> bool {
        self.names.contains(name) || self.prefixes.iter().any(|p| name.starts_with(p.as_str()))
    }
}

struct Checker<'a> {
    full: &'a Project,
    p: &'a Project,
    cat: &'a Catalog,
    provider: &'a str,
    plan: &'a Plan,
    provider_name: String,
    addresses: HashMap<String, Id>,
    legacy: HashMap<String, String>,
    vars: VarSet,
    out: Checked,
    /// The entity whose extras are being read.
    source: Id,
}

impl Checker<'_> {
    fn diag(&mut self, severity: Severity, message: String) {
        self.out.diagnostics.push(Diagnostic {
            entity: Some(self.source.clone()),
            severity,
            code: Code::Reference,
            message,
            provider: None,
        });
    }

    fn link(&mut self, target: &str, via: Via, at: &str) {
        if target == self.source {
            return;
        }
        let r = DerivedRef {
            source: self.source.clone(),
            target: target.to_string(),
            via,
            at: at.to_string(),
        };
        if !self.out.refs.contains(&r) {
            self.out.refs.push(r);
        }
    }

    fn value(&mut self, v: &serde_json::Value, at: &str) {
        match v {
            serde_json::Value::Array(items) => {
                for (i, x) in items.iter().enumerate() {
                    self.value(x, &format!("{at}[{i}]"));
                }
            }
            serde_json::Value::Object(o) => {
                if let Some(raw) = o.get("$raw") {
                    match raw.as_str() {
                        Some(text) => self.raw(text, o.get("refs"), at),
                        None => self.diag(Severity::Error, format!("$raw in {at} must be a string")),
                    }
                } else if let Some(r) = o.get("$ref") {
                    self.reference(r, at);
                } else {
                    for (k, x) in o {
                        self.value(x, &format!("{at}.{k}"));
                    }
                }
            }
            _ => {}
        }
    }

    fn reference(&mut self, r: &serde_json::Value, at: &str) {
        let Some(key) = r.get("entity").and_then(|x| x.as_str()) else {
            self.diag(Severity::Error, format!("$ref in {at} needs an \"entity\""));
            return;
        };
        let Some(tid) = resolve_entity(self.p, key) else {
            let msg = if resolve_entity(self.full, key).is_some() {
                format!(
                    "$ref in {at} names \"{key}\", which is not part of the {} export",
                    self.provider_name
                )
            } else {
                format!("$ref in {at} names \"{key}\", which does not exist")
            };
            self.diag(Severity::Error, msg);
            return;
        };
        let t = self.p.entity(&tid).unwrap();
        let name = t.name.to_string();
        let block = r.get("block").and_then(|x| x.as_str());
        let sel = KeySel::from_ref(r);
        if r.get("key").or_else(|| r.get("index")).is_some() && sel.is_none() {
            self.diag(
                Severity::Error,
                format!("$ref in {at}: \"key\" must be a string (or \"index\" a number)"),
            );
        }
        let skip = bootstrap_key(self.p, self.cat, self.provider);
        let mapping = self.cat.mapping(t.resource_type, self.provider);
        let external = t.manual || skip.as_deref() == Some(t.id) || mapping.is_none();
        if external {
            // It becomes an input variable, which is still a reference.
            self.link(&tid, Via::Ref, at);
            return;
        }
        let m = mapping.unwrap();
        if m.status == MappingStatus::Logical {
            self.diag(
                Severity::Error,
                format!(
                    "$ref in {at} names \"{name}\", which generates nothing on {} (it is only grouping there)",
                    self.provider_name
                ),
            );
            return;
        }
        let bkey = block
            .map(str::to_string)
            .unwrap_or_else(|| crate::emit::primary_key(m));
        let declared = m.blocks.iter().any(|b| b.key == bkey) || m.data.iter().any(|d| d.key == bkey);
        if !declared {
            let keys: Vec<&str> = m
                .blocks
                .iter()
                .chain(m.data.iter())
                .map(|b| b.key.as_str())
                .collect();
            self.diag(
                Severity::Error,
                format!(
                    "$ref in {at}: \"{name}\" has no block \"{bkey}\" on {} (blocks: {})",
                    self.provider_name,
                    keys.join(", ")
                ),
            );
            return;
        }
        let pk = (tid.clone(), bkey.clone());
        let Some(pl) = self.plan.blocks.get(&pk).or_else(|| self.plan.data.get(&pk)) else {
            self.diag(
                Severity::Error,
                format!(
                    "$ref in {at}: block \"{bkey}\" of \"{name}\" is not generated with its current settings"
                ),
            );
            return;
        };
        match &sel {
            Some(k) => {
                if pl.find(k).is_none() {
                    let msg = if pl.repeated {
                        format!(
                            "$ref in {at}: block \"{bkey}\" of \"{name}\" has no instance with {k} (it has: {})",
                            pl.keys().join(", ")
                        )
                    } else {
                        format!("$ref in {at}: block \"{bkey}\" of \"{name}\" is not repeated, so it takes no key")
                    };
                    self.diag(Severity::Error, msg);
                    return;
                }
            }
            None if pl.repeated && pl.instances.len() > 1 => {
                let first = &pl.instances[0];
                let msg = format!(
                    "$ref in {at} names the repeated block \"{bkey}\" of \"{name}\" without a key, so it refers to the first instance ({}.{}); add \"key\" (one of: {})",
                    pl.resource,
                    first.local,
                    pl.keys().join(", ")
                );
                self.diag(Severity::Warning, msg);
            }
            None => {}
        }
        self.link(&tid, Via::Ref, at);
    }

    fn raw(&mut self, text: &str, refs: Option<&serde_json::Value>, at: &str) {
        let mut text = text.to_string();
        let mut bound = Vec::new();
        if let Some(refs) = refs {
            match refs {
                serde_json::Value::Object(o) => {
                    for (name, v) in o {
                        self.value(v, &format!("{at}.refs.{name}"));
                        let placeholder = format!("__ttg_ref_{}", ttg_core::slugify(name));
                        text = text.replace(&format!("@{name}@"), &placeholder);
                        bound.push(placeholder);
                    }
                }
                _ => {
                    self.diag(
                        Severity::Error,
                        format!("$raw in {at}: \"refs\" must be an object of name -> value"),
                    );
                }
            }
        }
        if let Some(name) = text.split('@').skip(1).step_by(2).find(|s| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        }) {
            self.diag(
                Severity::Error,
                format!("$raw in {at} uses @{name}@ but \"refs\" has no \"{name}\""),
            );
            return;
        }
        let expr = match parse_raw(&text) {
            Ok(e) => e,
            Err(err) => {
                let first = err.lines().next().unwrap_or_default().to_string();
                self.diag(
                    Severity::Error,
                    format!("$raw in {at} is not a valid HCL expression: {first}"),
                );
                return;
            }
        };
        for a in addresses(&expr, &bound) {
            self.address(&a, at);
        }
    }

    fn address(&mut self, a: &Address, at: &str) {
        let pname = self.provider_name.clone();
        match a {
            Address::Resource { .. } | Address::Data { .. } => {
                let text = a.to_string();
                if let Some(owner) = self.addresses.get(&text).cloned() {
                    self.link(&owner, Via::Raw, at);
                    return;
                }
                if let Some(new) = self.legacy.get(&text).cloned() {
                    if let Some(owner) = self.addresses.get(&new).cloned() {
                        self.link(&owner, Via::Raw, at);
                    }
                    self.diag(
                        Severity::Warning,
                        format!(
                            "$raw in {at} refers to {text}, which is now {new} (repeated blocks are named by key); the export writes {new} — update the $raw, or use {{\"$ref\": {{…, \"key\": …}}}}"
                        ),
                    );
                    return;
                }
                let hint = self.hint(a);
                self.diag(
                    Severity::Error,
                    format!(
                        "$raw in {at} refers to {text}, which the {pname} export does not generate{hint}"
                    ),
                );
            }
            Address::Var(n) => {
                if !self.vars.contains(n) {
                    self.diag(
                        Severity::Error,
                        format!("$raw in {at} refers to var.{n}, which the {pname} export does not declare"),
                    );
                }
            }
            Address::Local(n) => self.diag(
                Severity::Error,
                format!("$raw in {at} refers to local.{n}; the export declares no locals"),
            ),
            Address::Module(n) => self.diag(
                Severity::Error,
                format!("$raw in {at} refers to module.{n}; the export declares no modules"),
            ),
            Address::Unknown(r) => self.diag(
                Severity::Error,
                format!("$raw in {at} refers to `{r}`, which is not something the export declares"),
            ),
        }
    }

    /// Why an address is missing, when an entity of that name explains it: one flagged
    /// external or off the layer, or one that generates blocks of the same type under
    /// other names (a repeated block's instances).
    fn hint(&self, a: &Address) -> String {
        let (prefix, name) = match a {
            Address::Resource { ty, name } => (format!("{ty}."), name),
            Address::Data { ty, name } => (format!("data.{ty}."), name),
            _ => return String::new(),
        };
        // The entity whose slug the local name starts with, longest first.
        let mut best: Option<(usize, ttg_core::EntityRef)> = None;
        for e in self.full.entities() {
            let slug = ttg_core::slugify(e.name);
            if (name == &slug || name.starts_with(&format!("{slug}_")))
                && best.as_ref().is_none_or(|(l, _)| slug.len() > *l)
            {
                best = Some((slug.len(), e));
            }
        }
        let Some((_, e)) = best else {
            return String::new();
        };
        let mut generated: Vec<String> = self
            .addresses
            .iter()
            .filter(|(addr, owner)| owner.as_str() == e.id && addr.starts_with(&prefix))
            .map(|(addr, _)| addr.clone())
            .collect();
        if self.p.entity(e.id).is_none() {
            return format!(
                " (\"{}\" is not part of the {} export)",
                e.name, self.provider_name
            );
        }
        if e.manual {
            return format!(
                " (\"{}\" is flagged as external / managed by hand, so nothing is generated for it; a {{\"$ref\"}} to it becomes an input variable)",
                e.name
            );
        }
        if generated.is_empty() {
            return String::new();
        }
        generated.sort();
        generated.truncate(6);
        format!(" (\"{}\" generates {})", e.name, generated.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addrs(text: &str) -> Vec<String> {
        addresses(&parse_raw(text).unwrap(), &[])
            .iter()
            .map(|a| a.to_string())
            .collect()
    }

    #[test]
    fn addresses_come_from_traversals_templates_and_functions() {
        assert_eq!(
            addrs("data.aws_ec2_managed_prefix_list.cloudfront_origin.id"),
            vec!["data.aws_ec2_managed_prefix_list.cloudfront_origin"]
        );
        let a = addrs(
            r#"jsonencode([{ image = "${aws_ecr_repository.ecr_repo_0.repository_url}:release", region = var.region, secrets = [for k in ["A", "B"] : { name = k, valueFrom = "${aws_secretsmanager_secret.app_secret.arn}:${k}::" }], group = aws_cloudwatch_log_group.web_logs.name }])"#,
        );
        assert_eq!(
            a,
            vec![
                "aws_ecr_repository.ecr_repo_0",
                "var.region",
                "aws_secretsmanager_secret.app_secret",
                "aws_cloudwatch_log_group.web_logs",
            ]
        );
        assert_eq!(
            addrs("format(\"%s/*\", aws_s3_bucket.attachments.arn)"),
            vec!["aws_s3_bucket.attachments"]
        );
        assert_eq!(
            addrs("aws_instance.web[0].id == local.x ? module.m.out : each.value"),
            vec!["aws_instance.web", "local.x", "module.m"]
        );
        assert_eq!(
            addrs("{ for k, v in var.m : k => v if v != null }"),
            vec!["var.m"]
        );
        assert_eq!(addrs("typo.attr"), vec!["typo"]);
        assert_eq!(addrs("\"%{ for x in var.l }${x}%{ endfor }\""), vec!["var.l"]);
    }

    #[test]
    fn legacy_addresses_are_rewritten_whole() {
        let mut m = HashMap::new();
        m.insert(
            "aws_ecr_repository.images_repo_1".to_string(),
            "aws_ecr_repository.images_repo_api".to_string(),
        );
        assert_eq!(
            rewrite_legacy(
                "\"${aws_ecr_repository.images_repo_1.repository_url}\" aws_ecr_repository.images_repo_10.x",
                &m
            ),
            "\"${aws_ecr_repository.images_repo_api.repository_url}\" aws_ecr_repository.images_repo_10.x"
        );
        assert_eq!(
            rewrite_legacy("xaws_ecr_repository.images_repo_1", &m),
            "xaws_ecr_repository.images_repo_1"
        );
    }
}
