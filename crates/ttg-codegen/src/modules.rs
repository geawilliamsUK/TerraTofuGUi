//! The modules layout (`settings.layout = "modules"`): the flat export split into a thin
//! root that calls one module per group of entities, each under `modules/<name>/`.
//!
//! **What becomes a module.** With `settings.module_view` naming a saved view, each of
//! its labelled groups is a module of the entities whose centre lies in it (the smallest
//! group when they nest; a container by its own box as the view places it, not by its
//! members, which may sit in other groups). An entity in no group goes with its nearest
//! enclosing container that is in one. Everything else goes by containment: an entity
//! belongs to the module of its outermost enclosing container that generates something on this provider
//! (a VPC, an Azure resource group, a container environment) — a container that is
//! logical or external here (the resource group on AWS) is looked through. Entities in
//! neither stay in the root.
//!
//! **How references cross.** The split runs on the finished flat export, environments
//! already merged, so every address is known. Inside a module, a reference to a resource
//! of another module or of the root becomes an input variable (`var.vpc_main_id`); the
//! module that owns the resource outputs it under the same name, and the root wires the
//! two (`vpc_main_id = module.network.vpc_main_id`). A module reads the root's variables
//! (provider settings, project and environment variables, values for external
//! resources) as inputs of the same name the root passes on, so the environments'
//! `.tfvars` keep working unchanged. The root's own references to module resources
//! become `module.<name>.<output>`. A `depends_on` on something in another scope becomes
//! the module block's `depends_on` (`[module.network]`, or the root resource) when that
//! cannot make the module graph cyclic, and otherwise an input carrying the target's
//! `id`, which the resource then depends on (`depends_on = [var.vpc_main_id]`): the same
//! ordering, resource by resource.
//!
//! **Providers.** A module that sends a resource to an aliased provider configuration
//! (`aws.us_east_1`) declares it in `configuration_aliases`, and the root passes the
//! configurations explicitly (`providers = { aws = aws, aws.us_east_1 = aws.us_east_1 }`).
//!
//! **State.** The root's `moved.tf` gains `moved { from = aws_vpc.main  to =
//! module.network.aws_vpc.main }` for every resource in a module, so a state applied
//! with the flat layout follows the switch instead of replacing anything.

use crate::emit::{EntityBlocks, Generated};
use crate::files;
use crate::tool::Profile;
use crate::GenError;
use hcl::template::{Directive, Element, Template};
use hcl::{
    Block, Body, Expression, Identifier, Object, ObjectKey, Structure, Traversal, TraversalOperator, Variable,
};
use indexmap::{IndexMap, IndexSet};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use ttg_catalog::{Catalog, MappingStatus};
use ttg_core::{Id, Project, Tool};

/// The directory the modules are written under.
pub const DIR: &str = "modules";

/// The module each entity belongs to, by id; an entity not listed stays in the root.
/// `p` is the project as exported (its layer for `provider` is taken here). Fails when
/// `settings.module_view` names no saved view.
pub fn assign(p: &Project, cat: &Catalog, provider: &str) -> Result<BTreeMap<Id, String>, String> {
    let layer = crate::layers::project_for(p, cat, provider);
    let mut names = Names::default();
    let mut out: BTreeMap<Id, String> = BTreeMap::new();
    if let Some(view_name) = p.settings.module_view.as_deref().filter(|v| !v.trim().is_empty()) {
        let v = p
            .views
            .iter()
            .find(|v| v.name == view_name)
            .ok_or_else(|| {
                format!("settings.module_view names the view \"{view_name}\", but there is no saved view of that name")
            })?;
        let visible = crate::views::visible_set(p, cat, &v.filter);
        let shown = |id: &str| visible.as_ref().is_none_or(|s| s.contains(id));
        // Smallest box first, so an entity in nested boxes lands in the innermost.
        let mut groups: Vec<&ttg_core::Group> = v.groups.iter().collect();
        groups.sort_by(|a, b| {
            let area = |g: &ttg_core::Group| g.size.w as i64 * g.size.h as i64;
            area(a).cmp(&area(b)).then(a.id.cmp(&b.id))
        });
        // Module names follow the view's own order of its groups, whatever their size.
        let mut by_group: HashMap<&str, String> = HashMap::new();
        for g in &v.groups {
            by_group.insert(g.id.as_str(), names.take(&g.label, "group"));
        }
        for e in layer.entities() {
            if !shown(e.id) {
                continue;
            }
            // A container counts where its own box is, not where its members are: they
            // may well sit in other groups.
            let rect = if p.containers.contains_key(e.id) {
                ttg_core::view::position_of(p, Some(v), e.id)
                    .zip(ttg_core::view::size_of(p, Some(v), e.id))
                    .map(|(at, size)| ttg_core::view::Rect::new(at, size))
            } else {
                ttg_core::view::entity_rect(p, Some(v), e.id, &shown)
            };
            let Some(rect) = rect else { continue };
            let (cx, cy) = rect.center();
            if let Some(g) = groups
                .iter()
                .find(|g| ttg_core::view::group_rect(g).contains(cx, cy))
            {
                out.insert(e.id.to_string(), by_group[g.id.as_str()].clone());
            }
        }
    }
    let grouped = out.clone();
    // Containment: the outermost enclosing container that generates something here.
    let mut by_container: BTreeMap<Id, String> = BTreeMap::new();
    for e in layer.entities() {
        if out.contains_key(e.id) {
            continue;
        }
        let mut chain: Vec<&str> = vec![e.id];
        let mut cur = e.parent;
        let mut steps = 0;
        while let Some(c) = cur {
            chain.push(c);
            cur = layer.parent_of(c);
            steps += 1;
            if steps > 64 {
                break;
            }
        }
        // In no group itself: with the nearest enclosing container that is in one.
        if let Some(m) = chain[1..].iter().find_map(|c| grouped.get(*c)) {
            out.insert(e.id.to_string(), m.clone());
            continue;
        }
        let top = chain.iter().rev().find(|id| {
            layer.entity(id).is_some_and(|x| {
                x.is_container
                    && !x.manual
                    && cat
                        .mapping(x.resource_type, provider)
                        .is_some_and(|m| m.status != MappingStatus::Logical)
            })
        });
        if let Some(top) = top {
            let name = match by_container.get(*top) {
                Some(n) => n.clone(),
                None => {
                    let c = layer.entity(top).expect("in the chain");
                    let n = names.take(c.name, "container");
                    by_container.insert(top.to_string(), n.clone());
                    n
                }
            };
            out.insert(e.id.to_string(), name);
        }
    }
    Ok(out)
}

/// `module.<name>.<address>`, or the address as it is in the root.
pub fn qualify(module: Option<&str>, address: &str) -> String {
    match module {
        Some(m) => format!("module.{m}.{address}"),
        None => address.to_string(),
    }
}

/// Unique module names: the slug of a label or container name, `_2`, `_3`, … on a clash.
#[derive(Default)]
struct Names {
    used: HashSet<String>,
}

impl Names {
    fn take(&mut self, label: &str, fallback: &str) -> String {
        let mut base = ttg_core::slugify(label);
        if base.is_empty() {
            base = fallback.to_string();
        }
        if base.starts_with(|c: char| c.is_ascii_digit()) {
            base = format!("m_{base}");
        }
        let mut name = base.clone();
        let mut i = 2;
        while self.used.contains(&name) {
            name = format!("{base}_{i}");
            i += 1;
        }
        self.used.insert(name.clone());
        name
    }
}

// ------------------------------------------------------------------ walking expressions

/// Call `f` on every traversal in `e`, templates included (parsed), innermost first.
fn each_traversal(e: &Expression, f: &mut dyn FnMut(&Traversal)) {
    match e {
        Expression::Traversal(t) => {
            each_traversal(&t.expr, f);
            for op in &t.operators {
                if let TraversalOperator::Index(i) = op {
                    each_traversal(i, f);
                }
            }
            f(t);
        }
        Expression::Array(items) => items.iter().for_each(|x| each_traversal(x, f)),
        Expression::Object(o) => {
            for (k, v) in o {
                if let ObjectKey::Expression(k) = k {
                    each_traversal(k, f);
                }
                each_traversal(v, f);
            }
        }
        Expression::TemplateExpr(t) => {
            if let Ok(t) = Template::from_expr(t) {
                template_traversals(&t, f);
            }
        }
        Expression::FuncCall(c) => c.args.iter().for_each(|x| each_traversal(x, f)),
        Expression::Parenthesis(x) => each_traversal(x, f),
        Expression::Conditional(c) => {
            each_traversal(&c.cond_expr, f);
            each_traversal(&c.true_expr, f);
            each_traversal(&c.false_expr, f);
        }
        Expression::Operation(op) => match op.as_ref() {
            hcl::expr::Operation::Unary(u) => each_traversal(&u.expr, f),
            hcl::expr::Operation::Binary(b) => {
                each_traversal(&b.lhs_expr, f);
                each_traversal(&b.rhs_expr, f);
            }
        },
        Expression::ForExpr(x) => {
            each_traversal(&x.collection_expr, f);
            if let Some(k) = &x.key_expr {
                each_traversal(k, f);
            }
            each_traversal(&x.value_expr, f);
            if let Some(c) = &x.cond_expr {
                each_traversal(c, f);
            }
        }
        _ => {}
    }
}

fn template_traversals(t: &Template, f: &mut dyn FnMut(&Traversal)) {
    for el in t.elements() {
        match el {
            Element::Literal(_) => {}
            Element::Interpolation(i) => each_traversal(&i.expr, f),
            Element::Directive(d) => match d.as_ref() {
                Directive::If(i) => {
                    each_traversal(&i.cond_expr, f);
                    template_traversals(&i.true_template, f);
                    if let Some(x) = &i.false_template {
                        template_traversals(x, f);
                    }
                }
                Directive::For(x) => {
                    each_traversal(&x.collection_expr, f);
                    template_traversals(&x.template, f);
                }
            },
        }
    }
}

/// Replace every traversal `f` returns a replacement for. Inside a template string the
/// reference is replaced in the template's text, where it is written as an address.
fn rewrite(e: &mut Expression, f: &mut dyn FnMut(&Traversal) -> Option<Expression>) -> Result<(), String> {
    match e {
        Expression::Traversal(t) => {
            rewrite(&mut t.expr, f)?;
            for op in t.operators.iter_mut() {
                if let TraversalOperator::Index(i) = op {
                    rewrite(i, f)?;
                }
            }
            if let Some(new) = f(t) {
                *e = new;
            }
        }
        Expression::Array(items) => {
            for x in items.iter_mut() {
                rewrite(x, f)?;
            }
        }
        Expression::Object(o) => {
            for (_, v) in o.iter_mut() {
                rewrite(v, f)?;
            }
        }
        Expression::TemplateExpr(t) => {
            let parsed = Template::from_expr(t).map_err(|x| format!("a template could not be read: {x}"))?;
            let mut pairs: Vec<(String, String)> = Vec::new();
            template_traversals(&parsed, &mut |tr| {
                if let Some(new) = f(tr) {
                    let old = Expression::Traversal(Box::new(tr.clone())).to_string();
                    if !pairs.iter().any(|(o, _)| *o == old) {
                        pairs.push((old, new.to_string()));
                    }
                }
            });
            if !pairs.is_empty() {
                let text = match t.as_mut() {
                    hcl::expr::TemplateExpr::QuotedString(s) => s,
                    hcl::expr::TemplateExpr::Heredoc(h) => &mut h.template,
                };
                for (old, new) in pairs {
                    let replaced = replace_address(text, &old, &new);
                    if replaced == *text {
                        return Err(format!(
                            "the reference {old} in a template could not be rewritten"
                        ));
                    }
                    *text = replaced;
                }
            }
        }
        Expression::FuncCall(c) => {
            for x in c.args.iter_mut() {
                rewrite(x, f)?;
            }
        }
        Expression::Parenthesis(x) => rewrite(x, f)?,
        Expression::Conditional(c) => {
            rewrite(&mut c.cond_expr, f)?;
            rewrite(&mut c.true_expr, f)?;
            rewrite(&mut c.false_expr, f)?;
        }
        Expression::Operation(op) => match op.as_mut() {
            hcl::expr::Operation::Unary(u) => rewrite(&mut u.expr, f)?,
            hcl::expr::Operation::Binary(b) => {
                rewrite(&mut b.lhs_expr, f)?;
                rewrite(&mut b.rhs_expr, f)?;
            }
        },
        Expression::ForExpr(x) => {
            rewrite(&mut x.collection_expr, f)?;
            if let Some(k) = &mut x.key_expr {
                rewrite(k, f)?;
            }
            rewrite(&mut x.value_expr, f)?;
            if let Some(c) = &mut x.cond_expr {
                rewrite(c, f)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn rewrite_body(body: &mut Body, f: &mut dyn FnMut(&Traversal) -> Option<Expression>) -> Result<(), String> {
    for s in body.0.iter_mut() {
        match s {
            Structure::Attribute(a) => rewrite(&mut a.expr, f)?,
            Structure::Block(b) => rewrite_body(&mut b.body, f)?,
        }
    }
    Ok(())
}

/// The reverse of the split for one module's block: every input that carries a value of
/// another scope (`var.vpc_main_id`, see [`Generated::module_inputs`]) back to the flat
/// export's reference (`aws_vpc.main.id`), so a block read from `modules/<name>/` compares
/// with the flat export's. The root's variables passed through keep their names, which
/// are the same in both layouts.
pub(crate) fn unsplit_body(body: &mut Body, inputs: &BTreeMap<String, String>) -> Result<(), String> {
    rewrite_body(body, &mut |t| {
        let Expression::Variable(v) = &t.expr else {
            return None;
        };
        if v.as_str() != "var" {
            return None;
        }
        let Some(TraversalOperator::GetAttr(name)) = t.operators.first() else {
            return None;
        };
        let flat = inputs.get(name.as_str())?;
        let rest = t.operators[1..].to_vec();
        match crate::emit::raw_expr(flat) {
            Expression::Traversal(inner) => {
                let mut ops = inner.operators.clone();
                ops.extend(rest);
                Some(Expression::Traversal(Box::new(Traversal::new(
                    inner.expr.clone(),
                    ops,
                ))))
            }
            other if rest.is_empty() => Some(other),
            _ => None,
        }
    })
}

/// Replace `old` (an address) by `new` wherever it stands as a whole in `text`: not
/// preceded by part of a name or a `.`, not followed by part of a name.
fn replace_address(text: &str, old: &str, new: &str) -> String {
    let name = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(old) {
        let before = rest[..i].chars().next_back().or(out.chars().next_back());
        let after = rest[i + old.len()..].chars().next();
        let whole = before.is_none_or(|c| !name(c) && c != '.') && after.is_none_or(|c| !name(c));
        out.push_str(&rest[..i]);
        out.push_str(if whole { new } else { old });
        rest = &rest[i + old.len()..];
    }
    out.push_str(rest);
    out
}

/// The address a traversal starts with (`aws_vpc.main`, `data.aws_iam_policy_document.x`)
/// and the index of the first operator after it.
fn address_of(t: &Traversal) -> Option<(String, usize)> {
    let Expression::Variable(v) = &t.expr else {
        return None;
    };
    let attr = |i: usize| match t.operators.get(i) {
        Some(TraversalOperator::GetAttr(a)) => Some(a.to_string()),
        _ => None,
    };
    match v.as_str() {
        "data" => Some((format!("data.{}.{}", attr(0)?, attr(1)?), 2)),
        r if r.contains('_') => Some((format!("{r}.{}", attr(0)?), 1)),
        _ => None,
    }
}

fn traversal(root: &str, attrs: &[&str]) -> Expression {
    let mut b = Traversal::builder(Variable::unchecked(root));
    for a in attrs {
        b = b.attr(*a);
    }
    Expression::Traversal(Box::new(b.build()))
}

// ------------------------------------------------------------------ the split

/// A value one scope hands another: a resource attribute (or a whole resource) the
/// owning module outputs and the using scope reads.
#[derive(Debug, Clone)]
struct Wire {
    name: String,
    /// The module whose resource it is; `None` for a resource in the root.
    owner: Option<String>,
    /// The traversal as written where the resource is.
    value: Expression,
    sensitive: bool,
}

/// What one scope (a module, or the root as `None`) uses from outside itself.
#[derive(Debug, Default)]
struct Uses {
    vars: IndexSet<String>,
    /// Wire keys read as inputs (modules) or through `module.<owner>.<name>` (root).
    wires: IndexSet<String>,
    aliases: BTreeSet<String>,
}

struct Wiring<'a> {
    provider: &'a str,
    provider_local: String,
    /// Every address the export generates -> the module it is in (`None`: the root).
    owner: HashMap<String, Option<String>>,
    /// Addresses of resources that have a `count` (an environment leaves them out).
    counted: HashSet<String>,
    taken: HashSet<String>,
    /// Wire key (the traversal's text where the resource is) -> the wire.
    wires: IndexMap<String, Wire>,
}

impl Wiring<'_> {
    /// The wire for the leading part of `t` that names a value of `addr` (up to and
    /// including its first attribute), created on first use.
    fn wire_for(&mut self, prefix: &Traversal, addr: &str, owner: &Option<String>) -> String {
        let value = Expression::Traversal(Box::new(prefix.clone()));
        let key = value.to_string();
        if self.wires.contains_key(&key) {
            return key;
        }
        let local = addr.rsplit('.').next().unwrap_or(addr).to_string();
        let attr = prefix.operators.iter().rev().find_map(|o| match o {
            TraversalOperator::GetAttr(a) => Some(a.to_string()),
            _ => None,
        });
        let at = if addr.starts_with("data.") { 2 } else { 1 };
        let has_attr = prefix.operators.len() > at
            && prefix.operators[at..]
                .iter()
                .any(|o| matches!(o, TraversalOperator::GetAttr(_)));
        let stem = match (&attr, has_attr) {
            (Some(a), true) if addr.starts_with("data.") => format!("data_{local}_{a}"),
            (Some(a), true) => format!("{local}_{a}"),
            _ => local.clone(),
        };
        let mut name = stem.clone();
        let mut i = 2;
        while self.taken.contains(&name) {
            name = format!("{stem}_{i}");
            i += 1;
        }
        self.taken.insert(name.clone());
        let sensitive = !has_attr || attr.as_deref().is_some_and(|a| self.is_sensitive(addr, a));
        self.wires.insert(
            key.clone(),
            Wire {
                name,
                owner: owner.clone(),
                value,
                sensitive,
            },
        );
        key
    }

    /// Whether the provider's schema marks this attribute of the resource sensitive.
    fn is_sensitive(&self, addr: &str, attr: &str) -> bool {
        let (is_data, ty) = match addr.strip_prefix("data.") {
            Some(rest) => (true, rest.split('.').next().unwrap_or_default()),
            None => (false, addr.split('.').next().unwrap_or_default()),
        };
        let idx = ttg_schema::index();
        let schema = if is_data {
            idx.data_source(self.provider, ty)
        } else {
            idx.resource(self.provider, ty)
        };
        match schema {
            Some(s) => s.attributes.get(attr).is_some_and(|a| a.sensitive()),
            // Side providers' resources are not in the index; a generated password is.
            None => ty == "random_password" && (attr == "result" || attr == "bcrypt_hash"),
        }
    }

    /// The replacement for `t` read in `scope`, when it reaches into another scope.
    fn replace(&mut self, t: &Traversal, scope: &Option<String>, uses: &mut Uses) -> Option<Expression> {
        if let Expression::Variable(v) = &t.expr {
            if v.as_str() == "var" {
                if let Some(TraversalOperator::GetAttr(a)) = t.operators.first() {
                    uses.vars.insert(a.to_string());
                }
                return None;
            }
            if v.as_str() == self.provider_local {
                return None;
            }
        }
        let (addr, at) = address_of(t)?;
        let owner = self.owner.get(&addr)?.clone();
        if &owner == scope {
            return None;
        }
        // Up to and including the first attribute after the address, with any index or
        // splat between (`aws_nat_gateway.nat[*].id`).
        let k = t.operators[at..]
            .iter()
            .position(|o| matches!(o, TraversalOperator::GetAttr(_)))
            .map(|i| at + i + 1)
            .unwrap_or(t.operators.len());
        let prefix = Traversal::new(t.expr.clone(), t.operators[..k].to_vec());
        let key = self.wire_for(&prefix, &addr, &owner);
        uses.wires.insert(key.clone());
        let name = self.wires[&key].name.clone();
        let mut ops: Vec<TraversalOperator> = Vec::new();
        let root = match scope {
            Some(_) => {
                ops.push(TraversalOperator::GetAttr(Identifier::unchecked(name.as_str())));
                "var".to_string()
            }
            None => {
                let m = owner.expect("a reference from the root is to a module");
                ops.push(TraversalOperator::GetAttr(Identifier::unchecked(m.as_str())));
                ops.push(TraversalOperator::GetAttr(Identifier::unchecked(name.as_str())));
                "module".to_string()
            }
        };
        ops.extend(t.operators[k..].iter().cloned());
        Some(Expression::Traversal(Box::new(Traversal::new(
            Expression::Variable(Variable::unchecked(root.as_str())),
            ops,
        ))))
    }

    /// The traversal that orders on `addr` when a `depends_on` cannot name it: its `id`
    /// (every resource and data source has one; all instances of a counted one).
    fn id_of(&self, addr: &str) -> Traversal {
        let parts: Vec<&str> = addr.split('.').collect();
        let mut b = Traversal::builder(Variable::unchecked(parts[0]));
        for p in &parts[1..] {
            b = b.attr(*p);
        }
        if self.counted.contains(addr) {
            b = b.full_splat();
        }
        b.attr("id").build()
    }
}

/// One `depends_on` entry that names something in another scope, taken out of its block.
struct Dep {
    /// The scope of the block (`None`: the root).
    scope: Option<String>,
    entity: Id,
    /// Which of the entity's blocks.
    block: usize,
    /// The address depended on, and the scope it is in.
    target: String,
    target_owner: Option<String>,
}

/// The block's `depends_on` entries that name another scope, removed from it.
fn take_foreign_depends(
    b: &mut Block,
    scope: &Option<String>,
    owner: &HashMap<String, Option<String>>,
) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    for s in b.body.0.iter_mut() {
        let Structure::Attribute(a) = s else { continue };
        if a.key.as_str() != "depends_on" {
            continue;
        }
        let Expression::Array(items) = &mut a.expr else {
            continue;
        };
        items.retain(|x| {
            let Expression::Traversal(t) = x else { return true };
            let Some((addr, _)) = address_of(t) else {
                return true;
            };
            match owner.get(&addr) {
                Some(o) if o != scope => {
                    out.push((addr, o.clone()));
                    false
                }
                _ => true,
            }
        });
    }
    b.body.0.retain(|s| match s {
        Structure::Attribute(a) if a.key.as_str() == "depends_on" => {
            !matches!(&a.expr, Expression::Array(items) if items.is_empty())
        }
        _ => true,
    });
    out
}

/// Append `entry` to the block's `depends_on`, creating it.
fn add_depends(b: &mut Block, entry: Expression) {
    for s in b.body.0.iter_mut() {
        if let Structure::Attribute(a) = s {
            if a.key.as_str() == "depends_on" {
                if let Expression::Array(items) = &mut a.expr {
                    if !items.contains(&entry) {
                        items.push(entry);
                    }
                    return;
                }
            }
        }
    }
    b.body.0.push(Structure::Attribute(hcl::Attribute::new(
        "depends_on",
        Expression::Array(vec![entry]),
    )));
}

/// Does `from` reach `to` along `edges`?
fn reaches(
    edges: &BTreeMap<Option<String>, BTreeSet<Option<String>>>,
    from: &Option<String>,
    to: &Option<String>,
) -> bool {
    let mut seen: HashSet<Option<String>> = HashSet::new();
    let mut stack = vec![from.clone()];
    while let Some(n) = stack.pop() {
        if &n == to {
            return true;
        }
        if !seen.insert(n.clone()) {
            continue;
        }
        if let Some(next) = edges.get(&n) {
            stack.extend(next.iter().cloned());
        }
    }
    false
}

/// The blocks of one `.tf` file.
fn parse_blocks(text: &str, name: &str) -> Result<Vec<Block>, GenError> {
    hcl::parse(text).map(|b| b.into_blocks().collect()).map_err(|e| {
        GenError::Emit(format!(
            "{name} could not be re-read to lay the export out as modules: {e}"
        ))
    })
}

fn render_blocks(header: &str, note: &str, blocks: &[Block], empty: &str) -> String {
    let mut out = String::from(header);
    if !note.is_empty() {
        out.push('\n');
        out.push_str(note);
    }
    if blocks.is_empty() {
        out.push_str(&format!("\n# {empty}\n"));
        return out;
    }
    for b in blocks {
        out.push('\n');
        out.push_str(&files::fmt_block(b));
    }
    out
}

/// Lay `g`, the flat export of `p`, out as modules (see the module documentation). An
/// export in which no entity falls into a module is returned as it is.
pub(crate) fn split(
    p: &Project,
    cat: &Catalog,
    provider: &str,
    tool: Tool,
    g: Generated,
) -> Result<Generated, GenError> {
    let assignment = assign(p, cat, provider).map_err(GenError::Emit)?;
    let pdef = cat
        .provider(provider)
        .ok_or_else(|| GenError::UnknownProvider(provider.to_string()))?;
    let header = Profile::new(tool).file_header(&pdef.provider.display_name);
    let mut g = g;

    // Modules in the order their first entity is generated.
    let mut modules: Vec<String> = Vec::new();
    for id in g.entity_blocks.keys() {
        if let Some(m) = assignment.get(id) {
            if !modules.contains(m) {
                modules.push(m.clone());
            }
        }
    }
    if modules.is_empty() {
        return Ok(g);
    }
    let scope_of = |id: &str| assignment.get(id).cloned();

    let mut owner: HashMap<String, Option<String>> = HashMap::new();
    let mut counted: HashSet<String> = HashSet::new();
    for (id, eb) in &g.entity_blocks {
        for (addr, b) in eb.addresses.iter().zip(eb.blocks()) {
            owner.insert(addr.clone(), scope_of(id));
            if b.body.attributes().any(|a| a.key() == "count") {
                counted.insert(addr.clone());
            }
        }
    }

    // The root's variables, which modules read as inputs of the same name.
    let root_vars: IndexMap<String, Block> = parse_blocks(
        g.files.get("variables.tf").map(String::as_str).unwrap_or(""),
        "variables.tf",
    )?
    .into_iter()
    .filter(|b| b.identifier.as_str() == "variable")
    .filter_map(|b| b.labels.first().map(|l| (l.as_str().to_string(), b.clone())))
    .collect();

    let mut w = Wiring {
        provider,
        provider_local: pdef.provider.local_name().to_string(),
        owner,
        counted,
        taken: root_vars.keys().cloned().collect(),
        wires: IndexMap::new(),
    };
    let mut uses: BTreeMap<Option<String>, Uses> = BTreeMap::new();
    uses.insert(None, Uses::default());
    for m in &modules {
        uses.insert(Some(m.clone()), Uses::default());
    }

    // ---- the entities' blocks: foreign depends_on taken out, references rewritten
    let mut deps: Vec<Dep> = Vec::new();
    let ids: Vec<Id> = g.entity_blocks.keys().cloned().collect();
    for id in &ids {
        let scope = scope_of(id);
        let eb = g.entity_blocks.get_mut(id).expect("listed");
        let u = uses.get_mut(&scope).expect("every scope");
        for (i, b) in eb.blocks.iter_mut().enumerate() {
            for (target, target_owner) in take_foreign_depends(b, &scope, &w.owner) {
                deps.push(Dep {
                    scope: scope.clone(),
                    entity: id.clone(),
                    block: i,
                    target,
                    target_owner,
                });
            }
            if let Some(Expression::Traversal(t)) = b
                .body
                .attributes()
                .find(|a| a.key() == "provider")
                .map(|a| a.expr.clone())
            {
                if let (Expression::Variable(v), Some(TraversalOperator::GetAttr(alias))) =
                    (&t.expr, t.operators.first())
                {
                    if v.as_str() == w.provider_local {
                        u.aliases.insert(alias.to_string());
                    }
                }
            }
            rewrite_body(&mut b.body, &mut |t| w.replace(t, &scope, u))
                .map_err(|e| GenError::Emit(format!("{e} (in {})", eb.addresses[i])))?;
        }
    }

    // ---- the root's outputs (and any other root file that names a resource)
    let mut root_outputs = parse_blocks(
        g.files.get("outputs.tf").map(String::as_str).unwrap_or(""),
        "outputs.tf",
    )?;
    {
        let u = uses.get_mut(&None).expect("root");
        for b in root_outputs.iter_mut() {
            let before: IndexSet<String> = u.wires.clone();
            rewrite_body(&mut b.body, &mut |t| w.replace(t, &None, u)).map_err(GenError::Emit)?;
            let sensitive = u
                .wires
                .iter()
                .filter(|k| !before.contains(*k))
                .any(|k| w.wires[k].sensitive);
            if sensitive && !b.body.attributes().any(|a| a.key() == "sensitive") {
                b.body
                    .0
                    .push(Structure::Attribute(hcl::Attribute::new("sensitive", true)));
            }
        }
    }
    let resource_files: HashSet<String> = g.entity_blocks.values().map(|b| b.file.clone()).collect();
    let fixed = [
        "variables.tf",
        "outputs.tf",
        "versions.tf",
        "providers.tf",
        "moved.tf",
    ];
    let mut other_root: IndexMap<String, String> = IndexMap::new();
    for (name, text) in &g.files {
        if !name.ends_with(".tf")
            || name.contains('/')
            || resource_files.contains(name)
            || name == "outputs.tf"
        {
            continue;
        }
        let mut blocks = parse_blocks(text, name)?;
        let mut changed = false;
        for b in blocks.iter_mut() {
            // `import { to = <address> }` follows the resource into its module.
            if b.identifier.as_str() == "import" {
                for s in b.body.0.iter_mut() {
                    if let Structure::Attribute(a) = s {
                        if a.key.as_str() == "to" {
                            if let Expression::Traversal(t) = &a.expr {
                                if let Some((addr, _)) = address_of(t) {
                                    if let Some(Some(m)) = w.owner.get(&addr) {
                                        let mut ops = vec![TraversalOperator::GetAttr(
                                            Identifier::unchecked(m.as_str()),
                                        )];
                                        let root = match &t.expr {
                                            Expression::Variable(v) => v.as_str().to_string(),
                                            _ => continue,
                                        };
                                        ops.push(TraversalOperator::GetAttr(Identifier::unchecked(
                                            root.as_str(),
                                        )));
                                        ops.extend(t.operators.iter().cloned());
                                        a.expr = Expression::Traversal(Box::new(Traversal::new(
                                            Expression::Variable(Variable::unchecked("module")),
                                            ops,
                                        )));
                                        changed = true;
                                    }
                                }
                            }
                        }
                    }
                }
                continue;
            }
            if b.identifier.as_str() == "moved" {
                continue;
            }
            let mut found: Vec<String> = Vec::new();
            for s in b.body.0.iter() {
                if let Structure::Attribute(a) = s {
                    each_traversal(&a.expr, &mut |t| {
                        if let Some((addr, _)) = address_of(t) {
                            if matches!(w.owner.get(&addr), Some(Some(_))) {
                                found.push(addr);
                            }
                        }
                    });
                }
            }
            if !fixed.contains(&name.as_str()) && !found.is_empty() {
                let u = uses.get_mut(&None).expect("root");
                rewrite_body(&mut b.body, &mut |t| w.replace(t, &None, u)).map_err(GenError::Emit)?;
                changed = true;
            } else if let Some(addr) = found.first() {
                return Err(GenError::Emit(format!(
                    "{name} refers to {addr}, which the modules layout moves into a module; use the flat layout for this project"
                )));
            }
        }
        if changed {
            other_root.insert(
                name.clone(),
                render_blocks(&header, "", &blocks, "Nothing to declare."),
            );
        }
    }

    // ---- data edges between scopes, then the depends_on that crossed
    let mut edges: BTreeMap<Option<String>, BTreeSet<Option<String>>> = BTreeMap::new();
    for (scope, u) in &uses {
        for k in &u.wires {
            edges
                .entry(scope.clone())
                .or_default()
                .insert(w.wires[k].owner.clone());
        }
    }
    // Every crossing `depends_on` orders its scope after its target whichever form it
    // takes, so all of them count before any is made module-level: a module waits for
    // another as a whole only where nothing of the other can come to wait for it.
    for d in &deps {
        edges
            .entry(d.scope.clone())
            .or_default()
            .insert(d.target_owner.clone());
    }
    let mut module_depends: BTreeMap<String, Vec<Expression>> = BTreeMap::new();
    for d in deps {
        let candidate = match (&d.scope, &d.target_owner) {
            // A module waits for a root resource, or for another module as a whole.
            (Some(m), None) => Some((m.clone(), traversal_of(&d.target))),
            (Some(m), Some(o)) => Some((m.clone(), traversal("module", &[o.as_str()]))),
            (None, _) => None,
        };
        let acyclic = !reaches(&edges, &d.target_owner, &d.scope);
        if acyclic {
            match candidate {
                Some((m, entry)) => {
                    let list = module_depends.entry(m).or_default();
                    if !list.contains(&entry) {
                        list.push(entry);
                    }
                }
                None => {
                    let o = d.target_owner.clone().expect("the root depends on a module");
                    let eb = g.entity_blocks.get_mut(&d.entity).expect("listed");
                    add_depends(&mut eb.blocks[d.block], traversal("module", &[o.as_str()]));
                }
            }
            continue;
        }
        // The module graph would close a cycle: depend on the target's id instead.
        let id = w.id_of(&d.target);
        let key = w.wire_for(&id, &d.target, &d.target_owner);
        let u = uses.get_mut(&d.scope).expect("every scope");
        u.wires.insert(key.clone());
        let wire = &w.wires[&key];
        let entry = match &d.scope {
            Some(_) => traversal("var", &[wire.name.as_str()]),
            None => traversal(
                "module",
                &[d.target_owner.as_deref().expect("a module"), wire.name.as_str()],
            ),
        };
        let eb = g.entity_blocks.get_mut(&d.entity).expect("listed");
        add_depends(&mut eb.blocks[d.block], entry);
    }

    // ---- per module: variables, outputs, versions
    let required = required_providers(g.files.get("versions.tf").map(String::as_str).unwrap_or(""))?;
    let mut module_files: IndexMap<String, String> = IndexMap::new();
    let mut calls: Vec<Block> = Vec::new();
    let mut module_inputs: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for m in &modules {
        let scope = Some(m.clone());
        let u = &uses[&scope];
        let dir = format!("{DIR}/{m}");
        // Inputs: the root's variables it reads, then the values of other scopes.
        let mut vars: Vec<Block> = Vec::new();
        let mut call = Block::builder("module")
            .add_label(m.as_str())
            .add_attribute(("source", format!("./{dir}")));
        let types: BTreeSet<String> = g
            .entity_blocks
            .iter()
            .filter(|(id, _)| assignment.get(*id) == Some(m))
            .flat_map(|(_, eb)| {
                eb.addresses.iter().map(|a| {
                    a.trim_start_matches("data.")
                        .split('.')
                        .next()
                        .unwrap_or_default()
                        .to_string()
                })
            })
            .collect();
        let needs: Vec<&(String, Expression)> = required
            .iter()
            .filter(|(name, _)| {
                types.iter().any(|t| t.starts_with(&format!("{name}_")))
                    || (name == &w.provider_local && !u.aliases.is_empty())
            })
            .collect();
        if !u.aliases.is_empty() {
            let mut o: Object<ObjectKey, Expression> = Object::new();
            for (name, _) in &needs {
                o.insert(
                    ObjectKey::Identifier(Identifier::unchecked(name.as_str())),
                    traversal(name, &[]),
                );
                if name == &w.provider_local {
                    for a in &u.aliases {
                        o.insert(
                            ObjectKey::Expression(traversal(name, &[a.as_str()])),
                            traversal(name, &[a.as_str()]),
                        );
                    }
                }
            }
            call = call.add_attribute(("providers", Expression::Object(o)));
        }
        for (name, block) in &root_vars {
            if !u.vars.contains(name) {
                continue;
            }
            let mut b = block.clone();
            b.body
                .0
                .retain(|s| !matches!(s, Structure::Attribute(a) if a.key.as_str() == "default"));
            vars.push(b);
            call = call.add_attribute((name.as_str(), traversal("var", &[name.as_str()])));
        }
        for v in u.vars.iter().filter(|v| !root_vars.contains_key(*v)) {
            // Declared nowhere in the root (cannot happen for a valid export): typed loosely.
            vars.push(
                Block::builder("variable")
                    .add_label(v.as_str())
                    .add_attribute(("type", traversal("any", &[])))
                    .build(),
            );
        }
        for k in &u.wires {
            let wire = &w.wires[k];
            module_inputs
                .entry(m.clone())
                .or_default()
                .insert(wire.name.clone(), k.clone());
            let from = match &wire.owner {
                Some(o) => format!("module \"{o}\""),
                None => "the root".to_string(),
            };
            let mut b = Block::builder("variable")
                .add_label(wire.name.as_str())
                .add_attribute(("type", traversal("any", &[])))
                .add_attribute(("description", format!("{} from {from}.", k)));
            if wire.sensitive {
                b = b.add_attribute(("sensitive", true));
            }
            vars.push(b.build());
            let value = match &wire.owner {
                Some(o) => traversal("module", &[o.as_str(), wire.name.as_str()]),
                None => wire.value.clone(),
            };
            call = call.add_attribute((wire.name.as_str(), value));
        }
        if let Some(list) = module_depends.get(m) {
            call = call.add_attribute(("depends_on", Expression::Array(list.clone())));
        }
        calls.push(call.build());
        // Outputs: what other scopes read of this module.
        let mut outs: Vec<Block> = Vec::new();
        for wire in w.wires.values().filter(|x| x.owner.as_ref() == Some(m)) {
            let used = uses
                .values()
                .any(|u| u.wires.iter().any(|k| w.wires[k].name == wire.name));
            if !used {
                continue;
            }
            let mut b = Block::builder("output")
                .add_label(wire.name.as_str())
                .add_attribute(("value", wire.value.clone()));
            if wire.sensitive {
                b = b.add_attribute(("sensitive", true));
            }
            outs.push(b.build());
        }
        let mut rp = Block::builder("required_providers");
        for (name, entry) in &needs {
            let mut entry = entry.clone();
            if name == &w.provider_local && !u.aliases.is_empty() {
                if let Expression::Object(o) = &mut entry {
                    o.insert(
                        ObjectKey::Identifier(Identifier::unchecked("configuration_aliases")),
                        Expression::Array(u.aliases.iter().map(|a| traversal(name, &[a.as_str()])).collect()),
                    );
                }
            }
            rp = rp.add_attribute((name.as_str(), entry));
        }
        let versions = Block::builder("terraform").add_block(rp.build()).build();
        module_files.insert(
            format!("{dir}/variables.tf"),
            render_blocks(&header, "", &vars, "This module takes no inputs."),
        );
        module_files.insert(
            format!("{dir}/outputs.tf"),
            render_blocks(
                &header,
                "# The values of this module that the root or other modules read.\n",
                &outs,
                "Nothing outside this module reads its resources.",
            ),
        );
        module_files.insert(
            format!("{dir}/versions.tf"),
            format!("{header}\n{}", files::fmt_block(&versions)),
        );
    }

    // ---- moved blocks: from the flat addresses into the modules
    let mut moves: Vec<Block> = Vec::new();
    for (id, eb) in &g.entity_blocks {
        let Some(m) = assignment.get(id) else { continue };
        for a in &eb.addresses {
            if a.starts_with("data.") {
                continue;
            }
            let (ty, local) = a.split_once('.').expect("type.local");
            moves.push(
                Block::builder("moved")
                    .add_attribute(("from", traversal(ty, &[local])))
                    .add_attribute(("to", traversal("module", &[m.as_str(), ty, local])))
                    .build(),
            );
        }
    }
    let moved_note =
        "\n# The modules layout (Settings > Output > Layout) puts these resources into modules. A\n\
         # state applied with the flat layout has them at the top level; these blocks move them\n\
         # to their module on the next apply instead of replacing them, and do nothing for a\n\
         # state that never had the flat addresses.\n";
    let mut moved = match g.files.get("moved.tf") {
        Some(t) => t.clone(),
        None => header.clone(),
    };
    moved.push_str(moved_note);
    for b in &moves {
        moved.push('\n');
        moved.push_str(&files::fmt_block(b));
    }

    // ---- the entities' files, now under their modules
    for (id, eb) in g.entity_blocks.iter_mut() {
        if let Some(m) = assignment.get(id) {
            eb.file = format!("{DIR}/{m}/{}", eb.file);
            eb.addresses = eb.addresses.iter().map(|a| qualify(Some(m), a)).collect();
            eb.module = Some(m.clone());
        }
    }
    let rendered = crate::emit::render_entity_files(&header, &mut g.entity_blocks);
    let calls_file = if rendered.contains_key("main.tf") {
        "modules.tf"
    } else {
        "main.tf"
    };
    let mut out: IndexMap<String, String> = IndexMap::new();
    for (name, text) in rendered.iter().filter(|(n, _)| !n.contains('/')) {
        out.insert(name.clone(), text.clone());
    }
    out.insert(
        calls_file.to_string(),
        render_blocks(
            &header,
            "# The modules this configuration is made of (modules/<name>/), and what each is given.\n",
            &calls,
            "No modules.",
        ),
    );
    let mut moved = Some(moved);
    for (name, text) in &g.files {
        if resource_files.contains(name) {
            continue;
        }
        let text = match name.as_str() {
            "outputs.tf" => render_blocks(
                &header,
                "",
                &root_outputs,
                "No outputs are declared for this configuration.",
            ),
            "README.md" => format!(
                "{text}{}",
                readme_section(&modules, &assignment, &g.entity_blocks, p)
            ),
            "moved.tf" => moved.take().unwrap_or_default(),
            _ => other_root.get(name).cloned().unwrap_or_else(|| text.clone()),
        };
        out.insert(name.clone(), text);
    }
    if let Some(moved) = moved {
        out.insert("moved.tf".into(), moved);
    }
    for (name, text) in rendered.iter().filter(|(n, _)| n.contains('/')) {
        out.insert(name.clone(), text.clone());
    }
    out.extend(module_files);
    g.files = out;
    g.module_inputs = module_inputs;
    Ok(g)
}

fn traversal_of(addr: &str) -> Expression {
    let parts: Vec<&str> = addr.split('.').collect();
    traversal(parts[0], &parts[1..])
}

/// `required_providers` of the root's `versions.tf`: local name -> its object.
fn required_providers(text: &str) -> Result<Vec<(String, Expression)>, GenError> {
    let mut out = Vec::new();
    for b in parse_blocks(text, "versions.tf")? {
        if b.identifier.as_str() != "terraform" {
            continue;
        }
        for inner in b.body.blocks() {
            if inner.identifier.as_str() == "required_providers" {
                for a in inner.body.attributes() {
                    out.push((a.key().to_string(), a.expr.clone()));
                }
            }
        }
    }
    Ok(out)
}

/// The README's account of the modules.
fn readme_section(
    modules: &[String],
    assignment: &BTreeMap<Id, String>,
    blocks: &IndexMap<Id, EntityBlocks>,
    p: &Project,
) -> String {
    let mut s = String::from(
        "\n## Modules\n\nThis configuration is laid out as modules (Settings ▸ Output ▸ Layout): the root \
         calls each of them in `main.tf` and passes on what one needs from another. A module reads \
         the root's variables under their own names, so the values (and each environment's \
         `.tfvars`) stay where they were. `moved.tf` moves a state applied with the flat layout \
         into the modules on the next apply.\n\n| module | resources |\n|---|---|\n",
    );
    for m in modules {
        let names: Vec<String> = blocks
            .keys()
            .filter(|id| assignment.get(*id) == Some(m))
            .filter_map(|id| p.entity(id).map(|e| e.name.to_string()))
            .collect();
        s.push_str(&format!("| `{DIR}/{m}` | {} |\n", names.join(", ")));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_replaced_whole() {
        assert_eq!(
            replace_address(
                "${aws_vpc.main.id}/${aws_vpc.main.idx}",
                "aws_vpc.main.id",
                "var.x"
            ),
            "${var.x}/${aws_vpc.main.idx}"
        );
        assert_eq!(
            replace_address("a.aws_vpc.main.id", "aws_vpc.main.id", "var.x"),
            "a.aws_vpc.main.id"
        );
    }

    #[test]
    fn provider_maps_render_with_traversal_keys() {
        let mut o: Object<ObjectKey, Expression> = Object::new();
        o.insert(
            ObjectKey::Identifier(Identifier::unchecked("aws")),
            traversal("aws", &[]),
        );
        o.insert(
            ObjectKey::Expression(traversal("aws", &["us_east_1"])),
            traversal("aws", &["us_east_1"]),
        );
        let b = Block::builder("module")
            .add_label("edge")
            .add_attribute(("providers", Expression::Object(o)))
            .build();
        let text = files::fmt_block(&b);
        assert!(text.contains("aws.us_east_1 = aws.us_east_1"), "{text}");
        assert!(text.contains("aws           = aws"), "{text}");
    }
}
