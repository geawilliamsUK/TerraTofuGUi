//! Import a directory TerraTofu exported and someone has edited since: review comments
//! applied by hand, a CI fix, a resource added beside the generated ones. The edits flow
//! back into the diagram instead of being overwritten by the next export.
//!
//! How each block of the directory's top-level `.tf` files is mapped:
//!
//! - **A generated address** (the plan's, as `entity_preview` lists them) belongs to the
//!   entity that generates it. Its arguments are compared with what the project generates
//!   now; each difference becomes an `extra` override on that entity and block: a literal
//!   where the value is one, `{"$ref": …}` where it is exactly a reference to a generated
//!   (or newly imported) block, `{"$raw": …}` for any other expression, `null` for an
//!   argument or nested block that was deleted. A nested block that changed is replaced
//!   whole. A repeated block (one resource per row) takes an override only when every
//!   instance changed the same way, since `extra` applies to all of them.
//! - **An address TerraTofu does not generate** becomes a native entity
//!   (`native:<provider>:<type>`, or `native:<provider>:data.<type>` for a data source)
//!   named after the block's label, every argument in its `extra`, references converted
//!   the same way. That covers hand-written files TerraTofu does not own (a `data.tf`):
//!   once imported, the export generates those blocks, so the hand-written file should be
//!   deleted ([`DirImport::hand_files`] lists them).
//! - **A generated address missing from the directory** is reported in
//!   [`DirImport::deleted`] and left in the diagram: deleting an entity is a decision for
//!   the user, not something to infer from a missing block.
//!
//! Everything else — variables, outputs, providers, `locals`, modules, subdirectories —
//! is listed, not imported. The result is a new project; nothing is written, and the
//! caller shows [`ttg_core::arch_diff`] between the two before applying it.

use crate::plan::Plan;
use hcl::{Block, Body, Expression, ObjectKey, Structure, TraversalOperator};
use serde::Serialize;
use serde_json::{json, Map, Value as J};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use ttg_catalog::Catalog;
use ttg_core::{Id, Node, Position, Project, Tool};

/// An `extra` override the import writes on one block of an entity.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Override {
    pub entity: Id,
    pub name: String,
    pub block: String,
    pub addresses: Vec<String>,
    /// Argument -> value; `null` removes the argument.
    pub args: Map<String, J>,
}

/// A native entity the import creates for a block TerraTofu did not generate.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AddedEntity {
    pub id: Id,
    pub name: String,
    #[serde(rename = "type")]
    pub type_id: String,
    /// The block's address in the directory.
    pub address: String,
    pub file: String,
}

/// A generated block that is not in the directory.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Deleted {
    pub address: String,
    pub entity: Id,
    pub name: String,
}

/// What importing a directory would do.
#[derive(Debug, Clone)]
pub struct DirImport {
    pub provider: String,
    /// The tool the directory was exported for, from its file headers.
    pub tool: Option<Tool>,
    /// The project with every override and new entity applied.
    pub project: Project,
    pub overrides: Vec<Override>,
    pub added: Vec<AddedEntity>,
    pub deleted: Vec<Deleted>,
    /// What was not imported, and why.
    pub skipped: Vec<String>,
    /// Hand-written files whose blocks were all imported: the export now generates them,
    /// so these files should be deleted once the import is applied.
    pub hand_files: Vec<String>,
    /// Files the export owns whose non-resource content (variables, outputs, providers,
    /// versions) differs from what it generates; such edits are not imported.
    pub owned_changed: Vec<String>,
    /// Things worth knowing that did not stop anything (a label that is not a slug).
    pub notes: Vec<String>,
}

impl DirImport {
    pub fn is_empty(&self) -> bool {
        self.overrides.is_empty() && self.added.is_empty()
    }

    /// The report as JSON (without the project).
    pub fn report_json(&self) -> J {
        json!({
            "provider": self.provider,
            "tool": self.tool,
            "overrides": self.overrides,
            "added": self.added,
            "deleted": self.deleted,
            "skipped": self.skipped,
            "hand_written_files": self.hand_files,
            "owned_files_changed": self.owned_changed,
            "notes": self.notes,
        })
    }

    /// The report as Markdown, for a preview beside the architecture diff.
    pub fn report_markdown(&self) -> String {
        let mut s = String::from("## Import report\n\n");
        if self.is_empty() && self.deleted.is_empty() {
            s.push_str("The directory matches what the project generates: nothing to import.\n");
        }
        if !self.overrides.is_empty() {
            s.push_str(&format!("### Argument changes ({})\n\n", self.overrides.len()));
            for o in &self.overrides {
                s.push_str(&format!("- **{}** `{}`: ", o.name, o.addresses.join("`, `")));
                let parts: Vec<String> = o
                    .args
                    .iter()
                    .map(|(k, v)| match v {
                        J::Null => format!("`{k}` removed"),
                        v => format!("`{k}` = `{}`", short(v)),
                    })
                    .collect();
                s.push_str(&parts.join(", "));
                s.push('\n');
            }
            s.push('\n');
        }
        if !self.added.is_empty() {
            s.push_str(&format!("### New native entities ({})\n\n", self.added.len()));
            for a in &self.added {
                s.push_str(&format!(
                    "- `{}` from {} as \"{}\" (`{}`)\n",
                    a.address, a.file, a.name, a.type_id
                ));
            }
            s.push('\n');
        }
        if !self.deleted.is_empty() {
            s.push_str(&format!(
                "### Generated but missing from the directory ({}) — not deleted\n\n",
                self.deleted.len()
            ));
            for d in &self.deleted {
                s.push_str(&format!("- `{}` (\"{}\")\n", d.address, d.name));
            }
            s.push('\n');
        }
        for (title, list) in [
            ("Not imported", &self.skipped),
            ("Delete after applying (now generated)", &self.hand_files),
            ("Edits in owned files that are not imported", &self.owned_changed),
            ("Notes", &self.notes),
        ] {
            if !list.is_empty() {
                s.push_str(&format!("### {title}\n\n"));
                for x in list {
                    s.push_str(&format!("- {x}\n"));
                }
                s.push('\n');
            }
        }
        s
    }
}

fn short(v: &J) -> String {
    let t = match v {
        J::Object(o) if o.contains_key("$raw") => o["$raw"].as_str().unwrap_or("").to_string(),
        other => other.to_string(),
    };
    if t.chars().count() > 70 {
        format!("{}\u{2026}", t.chars().take(69).collect::<String>())
    } else {
        t
    }
}

// ------------------------------------------------------------------ parsing

/// A `.tf` file of the directory.
struct TfFile {
    name: String,
    owned: bool,
    body: Body,
}

/// `(tool, provider display name)` from a TerraTofu file header, if `text` starts with one.
pub fn header_of(text: &str) -> Option<(Option<Tool>, String)> {
    let first = text.lines().next()?;
    let rest = first.strip_prefix("# Generated by TerraTofu GUI for ")?;
    let (tool, provider) = rest.split_once(" \u{2014} target provider: ")?;
    let tool = Tool::ALL.into_iter().find(|t| t.display_name() == tool.trim());
    Some((tool, provider.trim().trim_end_matches('.').to_string()))
}

/// Generated files that only carry state bookkeeping (`moved` and `import` blocks); an
/// import skips them.
const GENERATED_ONLY: [&str; 2] = ["moved.tf", "imports.tf"];

/// Subdirectories an export writes and an import leaves alone without remark: the
/// per-environment `.tfvars` and backend files.
const GENERATED_DIRS: [&str; 1] = [crate::environments::DIR];

fn read_dir_tf(dir: &Path) -> Result<Vec<(String, String)>, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    let mut out = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if p.is_file() && name.ends_with(".tf") {
            let text = std::fs::read_to_string(&p).map_err(|e| format!("cannot read {name}: {e}"))?;
            out.push((name, text));
        }
    }
    out.sort();
    Ok(out)
}

fn address_of(b: &Block) -> Option<String> {
    let labels: Vec<&str> = b.labels.iter().map(|l| l.as_str()).collect();
    match (b.identifier.as_str(), labels.as_slice()) {
        ("resource", [t, l]) => Some(format!("{t}.{l}")),
        ("data", [t, l]) => Some(format!("data.{t}.{l}")),
        _ => None,
    }
}

// ------------------------------------------------------------------ canonical form

/// An expression as JSON for comparison: literals as themselves (object keys as text),
/// anything else as its formatted text.
fn canon_expr(e: &Expression) -> J {
    literal(e).unwrap_or_else(|| json!({ "$expr": e.to_string() }))
}

fn key_text(k: &ObjectKey) -> Option<String> {
    match k {
        ObjectKey::Identifier(i) => Some(i.as_str().to_string()),
        ObjectKey::Expression(Expression::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// The JSON value of an expression made only of literals.
fn literal(e: &Expression) -> Option<J> {
    Some(match e {
        Expression::Null => J::Null,
        Expression::Bool(b) => J::Bool(*b),
        Expression::Number(n) => {
            if let Some(i) = n.as_i64() {
                J::from(i)
            } else {
                J::Number(serde_json::Number::from_f64(n.as_f64()?)?)
            }
        }
        Expression::String(s) => J::String(s.clone()),
        Expression::Array(items) => J::Array(items.iter().map(literal).collect::<Option<Vec<_>>>()?),
        Expression::Object(o) => {
            let mut m = Map::new();
            for (k, v) in o {
                m.insert(key_text(k)?, literal(v)?);
            }
            J::Object(m)
        }
        _ => return None,
    })
}

fn canon_block(b: &Block) -> J {
    json!({
        "labels": b.labels.iter().map(|l| l.as_str().to_string()).collect::<Vec<_>>(),
        "body": canon_body(&b.body),
    })
}

/// A body for comparison: attributes by name, nested blocks by name in their order.
fn canon_body(b: &Body) -> J {
    let mut attrs = Map::new();
    let mut blocks: BTreeMap<String, Vec<J>> = BTreeMap::new();
    for s in b.iter() {
        match s {
            Structure::Attribute(a) => {
                attrs.insert(a.key.as_str().to_string(), canon_expr(&a.expr));
            }
            Structure::Block(nb) => blocks
                .entry(nb.identifier.as_str().to_string())
                .or_default()
                .push(canon_block(nb)),
        }
    }
    json!({ "attrs": attrs, "blocks": blocks })
}

/// The resource and data blocks of a set of files, by address, in a form that compares
/// equal whenever two directories declare the same thing (whatever the file, order or
/// formatting). Used by the round-trip tests and by callers that want to check one.
pub fn semantic_blocks(files: &[(String, String)]) -> Result<BTreeMap<String, J>, String> {
    let mut out = BTreeMap::new();
    for (name, text) in files {
        let body = hcl::parse(text).map_err(|e| format!("{name}: {e}"))?;
        for s in body.iter() {
            if let Structure::Block(b) = s {
                if let Some(a) = address_of(b) {
                    out.insert(a, canon_body(&b.body));
                }
            }
        }
    }
    Ok(out)
}

/// [`semantic_blocks`] of a directory's top-level `.tf` files.
pub fn semantic_dir(dir: &Path) -> Result<BTreeMap<String, J>, String> {
    semantic_blocks(&read_dir_tf(dir)?)
}

// ------------------------------------------------------------------ conversion

/// Where an address leads: the entity, block key and instance a `$ref` names.
#[derive(Debug, Clone)]
struct Target {
    entity: Id,
    block: String,
    primary: bool,
    /// `key` (or position) of the instance in a repeated block.
    instance: Option<J>,
    /// The address once the import is applied, when it differs from the directory's.
    renamed: Option<String>,
}

struct Converter<'a> {
    targets: &'a HashMap<String, Target>,
    /// Directory address -> the address it has once imported, where they differ.
    renames: HashMap<String, String>,
}

impl Converter<'_> {
    /// A directory value as an `extra` value.
    fn value(&self, e: &Expression) -> J {
        if let Some(l) = literal(e) {
            return l;
        }
        match e {
            Expression::Traversal(t) => {
                if let Some(r) = self.reference(t) {
                    return r;
                }
            }
            Expression::Array(items) => return J::Array(items.iter().map(|x| self.value(x)).collect()),
            Expression::Object(o) if o.keys().all(|k| key_text(k).is_some()) => {
                let mut m = Map::new();
                for (k, v) in o {
                    m.insert(key_text(k).unwrap(), self.value(v));
                }
                return J::Object(m);
            }
            _ => {}
        }
        json!({ "$raw": self.raw_text(e) })
    }

    /// The expression's text with the addresses of renamed blocks rewritten.
    fn raw_text(&self, e: &Expression) -> String {
        crate::refs::rewrite_legacy(&e.to_string(), &self.renames)
    }

    /// `type.local.attr…` / `data.type.local.attr…` to a known block as a `$ref`.
    fn reference(&self, t: &hcl::Traversal) -> Option<J> {
        let Expression::Variable(root) = &t.expr else {
            return None;
        };
        let ops = &t.operators;
        let get = |i: usize| match ops.get(i) {
            Some(TraversalOperator::GetAttr(id)) => Some(id.as_str().to_string()),
            _ => None,
        };
        let (address, rest) = if root.as_str() == "data" {
            (format!("data.{}.{}", get(0)?, get(1)?), &ops[2..])
        } else {
            (format!("{}.{}", root.as_str(), get(0)?), &ops[1..])
        };
        let target = self.targets.get(&address)?;
        if rest.is_empty() {
            return None;
        }
        let mut attr = Vec::new();
        for op in rest {
            match op {
                TraversalOperator::GetAttr(id) => attr.push(id.as_str().to_string()),
                TraversalOperator::LegacyIndex(n) => attr.push(n.to_string()),
                TraversalOperator::Index(Expression::Number(n)) => attr.push(n.as_u64()?.to_string()),
                _ => return None,
            }
        }
        let mut r = Map::new();
        r.insert("entity".into(), J::String(target.entity.clone()));
        if !target.primary {
            r.insert("block".into(), J::String(target.block.clone()));
        }
        match &target.instance {
            Some(J::Number(n)) => {
                r.insert("index".into(), J::Number(n.clone()));
            }
            Some(k) => {
                r.insert("key".into(), k.clone());
            }
            None => {}
        }
        r.insert("attr".into(), J::String(attr.join(".")));
        Some(json!({ "$ref": J::Object(r) }))
    }

    /// A block body as `extra` arguments: attributes, and nested blocks as an object (one)
    /// or a list of objects (several). `None` with the reason when it cannot be one.
    fn body(&self, b: &Body) -> Result<Map<String, J>, String> {
        let mut out = Map::new();
        let mut nested: BTreeMap<String, Vec<J>> = BTreeMap::new();
        for s in b.iter() {
            match s {
                Structure::Attribute(a) => {
                    out.insert(a.key.as_str().to_string(), self.value(&a.expr));
                }
                Structure::Block(nb) => {
                    if !nb.labels.is_empty() {
                        return Err(format!(
                            "the labelled block `{} \"{}\"` cannot be written as an extra argument",
                            nb.identifier.as_str(),
                            nb.labels[0].as_str()
                        ));
                    }
                    nested
                        .entry(nb.identifier.as_str().to_string())
                        .or_default()
                        .push(J::Object(self.body(&nb.body)?));
                }
            }
        }
        for (k, mut v) in nested {
            out.insert(k, if v.len() == 1 { v.remove(0) } else { J::Array(v) });
        }
        Ok(out)
    }

    /// The overrides that turn `gen` (what the project generates) into `dir`.
    fn diff(&self, gen: &Body, dir: &Body) -> Result<Map<String, J>, String> {
        let attrs = |b: &Body| -> BTreeMap<String, Expression> {
            b.iter()
                .filter_map(|s| match s {
                    Structure::Attribute(a) => Some((a.key.as_str().to_string(), a.expr.clone())),
                    _ => None,
                })
                .collect()
        };
        let blocks = |b: &Body| -> BTreeMap<String, Vec<Block>> {
            let mut m: BTreeMap<String, Vec<Block>> = BTreeMap::new();
            for s in b.iter() {
                if let Structure::Block(nb) = s {
                    m.entry(nb.identifier.as_str().to_string())
                        .or_default()
                        .push(nb.clone());
                }
            }
            m
        };
        let (ga, da) = (attrs(gen), attrs(dir));
        let (gb, db) = (blocks(gen), blocks(dir));
        let mut out = Map::new();
        for (k, de) in &da {
            if ga.get(k).map(canon_expr) != Some(canon_expr(de)) {
                out.insert(k.clone(), self.value(de));
            }
        }
        for k in ga.keys() {
            if !da.contains_key(k) {
                out.insert(k.clone(), J::Null);
            }
        }
        for (k, dl) in &db {
            let same = gb.get(k).is_some_and(|gl| {
                gl.len() == dl.len() && gl.iter().zip(dl).all(|(a, b)| canon_block(a) == canon_block(b))
            });
            if !same {
                let mut items = Vec::new();
                for nb in dl {
                    if !nb.labels.is_empty() {
                        return Err(format!(
                            "the labelled block `{k} \"{}\"` cannot be written as an extra argument",
                            nb.labels[0].as_str()
                        ));
                    }
                    items.push(J::Object(self.body(&nb.body)?));
                }
                out.insert(
                    k.clone(),
                    if items.len() == 1 {
                        items.remove(0)
                    } else {
                        J::Array(items)
                    },
                );
            }
        }
        for k in gb.keys() {
            if !db.contains_key(k) {
                out.insert(k.clone(), J::Null);
            }
        }
        Ok(out)
    }
}

// ------------------------------------------------------------------ the import

/// Work out what importing `dir` into `project` does. `provider` defaults to the one the
/// directory's headers name, then the project's target. Native types for new entities
/// are registered in `cat`. Nothing is written.
pub fn import_dir(
    project: &Project,
    cat: &mut Catalog,
    dir: &Path,
    provider: Option<&str>,
) -> Result<DirImport, String> {
    let texts = read_dir_tf(dir)?;
    if texts.is_empty() {
        return Err(format!("{} has no .tf files", dir.display()));
    }
    let mut files = Vec::new();
    let mut header: Option<(Option<Tool>, String)> = None;
    for (name, text) in &texts {
        let h = header_of(text);
        if header.is_none() {
            header = h.clone();
        }
        // State bookkeeping the export writes (renames, adoptions): nothing to import.
        if h.is_some() && GENERATED_ONLY.contains(&name.as_str()) {
            continue;
        }
        let body = hcl::parse(text).map_err(|e| format!("{name} does not parse as HCL: {e}"))?;
        files.push(TfFile {
            name: name.clone(),
            owned: h.is_some(),
            body,
        });
    }
    let Some((tool, display)) = header else {
        return Err(format!(
            "{} is not a TerraTofu export: none of its .tf files starts with the \"# Generated by TerraTofu GUI\" header",
            dir.display()
        ));
    };
    let from_header = cat.provider_ids().into_iter().find(|p| {
        cat.provider(p)
            .is_some_and(|d| d.provider.display_name == display)
    });
    let provider = match (provider, &from_header) {
        (Some(p), Some(h)) if p != h => {
            return Err(format!(
                "the directory was exported for {display} ({h}), not {p}; leave `provider` out or pass {h}"
            ))
        }
        (Some(p), _) => p.to_string(),
        (None, Some(h)) => h.clone(),
        (None, None) => project.settings.target_provider.clone(),
    };
    if cat.provider(&provider).is_none() {
        return Err(format!("unknown provider \"{provider}\""));
    }
    cat.ensure_native_types(project);
    let gen_tool = tool.unwrap_or(project.settings.tool);
    // Compared with what the export writes: for a project with environments that is the
    // merged root (counts, lifted variables), which only an export-clean project has.
    let base = project.resolved(None);
    let generated = if project.settings.environments.is_empty() {
        crate::emit::emit_unchecked(&base, cat, &provider, gen_tool)
    } else {
        crate::emit::generate(project, cat, &provider, gen_tool)
    }
    .map_err(|e| format!("the project does not generate for {provider}: {e}"))?;

    // What the project generates, block by block.
    let mut gen_blocks: HashMap<String, Block> = HashMap::new();
    let mut gen_files: HashMap<String, Body> = HashMap::new();
    for (name, text) in &generated.files {
        if !name.ends_with(".tf") || name.contains('/') {
            continue;
        }
        let body = hcl::parse(text).map_err(|e| format!("generated {name}: {e}"))?;
        for s in body.iter() {
            if let Structure::Block(b) = s {
                if let Some(a) = address_of(b) {
                    gen_blocks.insert(a, b.clone());
                }
            }
        }
        gen_files.insert(name.clone(), body);
    }

    // Generated addresses -> entity, block and instance.
    let layer = crate::layers::project_for(&base, cat, &provider);
    let plan = Plan::build(
        &layer,
        cat,
        &provider,
        crate::refs::bootstrap_key(&layer, cat, &provider).as_deref(),
    );
    let mut targets: HashMap<String, Target> = HashMap::new();
    let mut planned_instances: HashMap<(Id, String), Vec<String>> = HashMap::new();
    for (is_data, map) in [(false, &plan.blocks), (true, &plan.data)] {
        for ((id, key), pl) in map {
            let primary = layer
                .entity(id)
                .and_then(|e| cat.mapping(e.resource_type, &provider))
                .map(|m| crate::emit::primary_key(m) == *key)
                .unwrap_or(false);
            for (n, i) in pl.instances.iter().enumerate() {
                let address = if is_data {
                    crate::adopt::instance_address(pl, i, true)
                } else {
                    crate::adopt::instance_address(pl, i, false)
                };
                let instance = pl.repeated.then(|| match &i.key {
                    Some(k) => J::String(k.clone()),
                    None => J::from(n),
                });
                if !is_data && gen_blocks.contains_key(&address) {
                    planned_instances
                        .entry((id.clone(), key.clone()))
                        .or_default()
                        .push(address.clone());
                }
                targets.insert(
                    address,
                    Target {
                        entity: id.clone(),
                        block: key.clone(),
                        primary,
                        instance,
                        renamed: None,
                    },
                );
            }
        }
    }

    let mut out = DirImport {
        provider: provider.clone(),
        tool,
        project: project.clone(),
        overrides: Vec::new(),
        added: Vec::new(),
        deleted: Vec::new(),
        skipped: Vec::new(),
        hand_files: Vec::new(),
        owned_changed: Vec::new(),
        notes: Vec::new(),
    };
    let mut subdirs: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| !n.starts_with('.') && !GENERATED_DIRS.contains(&n.as_str()))
                .collect()
        })
        .unwrap_or_default();
    subdirs.sort();
    if !subdirs.is_empty() {
        out.notes.push(format!(
            "{} not imported; only the top-level .tf files are",
            subdirs
                .iter()
                .map(|d| format!("{d}/"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    // The directory's blocks.
    let mut dir_blocks: Vec<(String, Block, String, bool)> = Vec::new();
    let mut seen = HashSet::new();
    for f in &files {
        let mut other = Vec::new();
        for s in f.body.iter() {
            match s {
                Structure::Block(b) => match address_of(b) {
                    Some(a) => {
                        if !seen.insert(a.clone()) {
                            out.skipped.push(format!(
                                "{a} is declared twice; the copy in {} is ignored",
                                f.name
                            ));
                            continue;
                        }
                        dir_blocks.push((a, b.clone(), f.name.clone(), f.owned));
                    }
                    None => other.push(b.identifier.as_str().to_string()),
                },
                Structure::Attribute(a) => other.push(format!("attribute {}", a.key.as_str())),
            }
        }
        if f.owned {
            // Non-resource content of an owned file: compared with what is generated.
            let strip = |b: &Body| -> Vec<J> {
                b.iter()
                    .filter_map(|s| match s {
                        Structure::Block(b) if address_of(b).is_none() => {
                            Some(json!([b.identifier.as_str(), canon_block(b)]))
                        }
                        Structure::Attribute(a) => Some(json!([a.key.as_str(), canon_expr(&a.expr)])),
                        _ => None,
                    })
                    .collect()
            };
            let gen = gen_files.get(&f.name).map(strip).unwrap_or_default();
            if strip(&f.body) != gen {
                out.owned_changed.push(format!(
                    "{}: changes to its {} are not imported",
                    f.name,
                    if other.is_empty() {
                        "blocks".to_string()
                    } else {
                        other.join(", ")
                    }
                ));
            }
        } else if !other.is_empty() {
            other.dedup();
            out.skipped
                .push(format!("{}: {} not imported", f.name, other.join(", ")));
        }
    }

    // New blocks: native entities first, so references to them convert too.
    let schema = ttg_schema::index();
    let mut taken: HashSet<String> = project
        .entities()
        .iter()
        .map(|e| ttg_core::slugify(e.name))
        .collect();
    let right = project
        .entities()
        .iter()
        .map(|e| e.position.x + 240)
        .max()
        .unwrap_or(40);
    let mut row = 0;
    let mut new_blocks: Vec<(Id, Block, String)> = Vec::new();
    let mut imported_from: HashMap<String, (usize, usize)> = HashMap::new();
    for (address, b, file, owned) in &dir_blocks {
        let entry = imported_from.entry(file.clone()).or_insert((0, 0));
        if !*owned {
            entry.1 += 1;
        }
        if targets.contains_key(address) {
            if !*owned {
                entry.0 += 1;
            }
            continue;
        }
        let is_data = b.identifier.as_str() == "data";
        let tf_type = b.labels[0].as_str();
        let label = b.labels[1].as_str();
        let schema_name = if is_data {
            format!("{}{tf_type}", ttg_schema::DATA_PREFIX)
        } else {
            tf_type.to_string()
        };
        let known = schema
            .provider(&provider)
            .map(|ps| ps.block(&schema_name).is_some())
            .unwrap_or(false);
        if !known {
            out.skipped.push(format!(
                "{address} ({file}): {provider} has no {} \"{tf_type}\" in its schema",
                if is_data { "data source" } else { "resource" }
            ));
            continue;
        }
        let type_id = format!("native:{provider}:{schema_name}");
        if !cat.ensure_native(&type_id) {
            out.skipped.push(format!(
                "{address} ({file}): cannot create the native type {type_id}"
            ));
            continue;
        }
        let mut name = label.to_string();
        let mut n = 2;
        while taken.contains(&ttg_core::slugify(&name)) {
            name = format!("{label}_{n}");
            n += 1;
        }
        taken.insert(ttg_core::slugify(&name));
        let local = ttg_core::slugify(&name);
        let new_address = if is_data {
            format!("data.{tf_type}.{local}")
        } else {
            format!("{tf_type}.{local}")
        };
        if local != label {
            out.notes.push(format!(
                "{address} is imported as \"{name}\", so the export names it {new_address}"
            ));
        }
        let id = out.project.fresh_id("native");
        out.project.nodes.insert(
            id.clone(),
            Node {
                id: id.clone(),
                name: name.clone(),
                resource_type: type_id.clone(),
                config: Default::default(),
                provider_config: Default::default(),
                position: Position {
                    x: right,
                    y: 40 + row * 90,
                },
                size: None,
                parent: None,
                manual: false,
                providers: Vec::new(),
                extra: Default::default(),
                classification: None,
                // No description: it would become a tag the hand-written block lacks.
                description: String::new(),
                owner: String::new(),
                overrides: Default::default(),
                adopt: Default::default(),
            },
        );
        row += 1;
        if !*owned {
            entry.0 += 1;
        }
        targets.insert(
            address.clone(),
            Target {
                entity: id.clone(),
                block: "main".into(),
                primary: true,
                instance: None,
                renamed: (new_address != *address).then_some(new_address),
            },
        );
        out.added.push(AddedEntity {
            id: id.clone(),
            name,
            type_id,
            address: address.clone(),
            file: file.clone(),
        });
        new_blocks.push((id, b.clone(), address.clone()));
    }
    for (file, (imported, total)) in &imported_from {
        if *total > 0 && imported == total {
            out.hand_files.push(file.clone());
        }
    }
    out.hand_files.sort();

    let conv = Converter {
        targets: &targets,
        renames: targets
            .iter()
            .filter_map(|(a, t)| t.renamed.clone().map(|r| (a.clone(), r)))
            .collect(),
    };
    for (id, b, address) in &new_blocks {
        match conv.body(&b.body) {
            Ok(args) => {
                if !args.is_empty() {
                    if let Some(map) = out.project.extra_args_mut(id, &provider, "main") {
                        *map = args;
                    }
                }
            }
            Err(e) => {
                out.skipped
                    .push(format!("{address}: {e}; imported without its arguments"));
            }
        }
    }

    // Changed generated blocks.
    let mut per_block: BTreeMap<(Id, String), InstanceEdits> = BTreeMap::new();
    for (address, b, _, _) in &dir_blocks {
        let Some(t) = targets.get(address) else { continue };
        if out.added.iter().any(|a| &a.address == address) {
            continue;
        }
        let Some(g) = gen_blocks.get(address) else {
            out.skipped.push(format!("{address} is planned but not written by the export (an identical grant written once); ignored"));
            continue;
        };
        match conv.diff(&g.body, &b.body) {
            Ok(args) => per_block
                .entry((t.entity.clone(), t.block.clone()))
                .or_default()
                .push((address.clone(), args)),
            Err(e) => out.skipped.push(format!("{address}: {e}")),
        }
    }
    for ((id, key), list) in per_block {
        let instances = planned_instances
            .get(&(id.clone(), key.clone()))
            .cloned()
            .unwrap_or_default();
        let changed: Vec<&(String, Map<String, J>)> = list.iter().filter(|(_, a)| !a.is_empty()).collect();
        if changed.is_empty() {
            continue;
        }
        let name = project
            .entity(&id)
            .map(|e| e.name.to_string())
            .unwrap_or_default();
        if instances.len() > 1 {
            let first = &changed[0].1;
            let uniform = changed.len() == instances.len() && changed.iter().all(|(_, a)| a == first);
            if !uniform {
                out.skipped.push(format!(
                    "\"{name}\" {key}: {} of its {} instances were edited, or edited differently; extra arguments apply to every instance of a repeated block, so these edits are not imported ({})",
                    changed.len(),
                    instances.len(),
                    changed.iter().map(|(a, _)| a.as_str()).collect::<Vec<_>>().join(", ")
                ));
                continue;
            }
        }
        let args = changed[0].1.clone();
        merge_override(&mut out.project, cat, &provider, &id, &key, &args);
        out.overrides.push(Override {
            entity: id.clone(),
            name,
            block: key.clone(),
            addresses: changed.iter().map(|(a, _)| a.clone()).collect(),
            args,
        });
    }

    // Generated blocks the directory no longer has.
    let present: HashSet<&String> = dir_blocks.iter().map(|(a, _, _, _)| a).collect();
    let mut deleted: Vec<Deleted> = gen_blocks
        .keys()
        .filter(|a| !present.contains(a))
        .filter_map(|a| {
            let t = targets.get(a)?;
            Some(Deleted {
                address: a.clone(),
                entity: t.entity.clone(),
                name: project
                    .entity(&t.entity)
                    .map(|e| e.name.to_string())
                    .unwrap_or_default(),
            })
        })
        .collect();
    deleted.sort_by(|a, b| a.address.cmp(&b.address));
    out.deleted = deleted;
    Ok(out)
}

/// The overrides each instance of one block needs: `(address, arguments)`.
type InstanceEdits = Vec<(String, Map<String, J>)>;

/// Merge `args` into an entity's extra arguments for one block: a value replaces the
/// argument; `null` removes the argument the mapping sets (stored as `null`) or drops
/// an extra argument the mapping does not set.
fn merge_override(
    p: &mut Project,
    cat: &Catalog,
    provider: &str,
    id: &str,
    key: &str,
    args: &Map<String, J>,
) {
    let bdef = p
        .entity(id)
        .and_then(|e| cat.mapping(e.resource_type, provider))
        .and_then(|m| m.blocks.iter().chain(m.data.iter()).find(|b| b.key == key))
        .cloned();
    let mapping_sets = |k: &str| {
        bdef.as_ref()
            .is_some_and(|b| b.args.contains_key(k) || b.nested.iter().any(|n| n.block == k))
    };
    if let Some(map) = p.extra_args_mut(id, provider, key) {
        for (k, v) in args {
            if v.is_null() && !mapping_sets(k) {
                map.remove(k);
            } else {
                map.insert(k.clone(), v.clone());
            }
        }
    }
    p.prune_extras(id);
}
