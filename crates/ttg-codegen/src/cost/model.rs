//! What the per-provider models share: the context one entity is priced in, reading the
//! same fields and resolving the same concrete SKU the mapping would, and recording each
//! charge and each assumption it leaned on.

use super::assumptions::{Resolver, Used};
use super::prices::PriceBook;
use super::Charge;
use crate::diagnostics::{condition_holds, field_or_default, relation_targets_of_type};
use ttg_catalog::{ArgSource, BlockDef, Catalog, ProviderMapping};
use ttg_core::{EntityRef, Project, Relation, Value};

/// How a provider module answers for one abstract type.
pub(crate) enum Model {
    /// Priced by this function.
    Priced(fn(&mut Ctx)),
    /// No model; the estimate would silently leave the type out. A test refuses this.
    Missing,
}

/// One entity being priced.
pub(crate) struct Ctx<'a> {
    pub p: &'a Project,
    pub cat: &'a Catalog,
    pub provider: &'a str,
    /// The region whose prices apply (the project's, or the list's fallback).
    pub region: &'a str,
    pub book: &'a PriceBook,
    pub e: EntityRef<'a>,
    asm: &'a Resolver,
    pub charges: Vec<Charge>,
    pub used: Vec<Used>,
    pub notes: Vec<String>,
    /// Prices the model asked for and the list does not have.
    pub missing: Vec<String>,
}

impl<'a> Ctx<'a> {
    pub fn new(
        p: &'a Project,
        cat: &'a Catalog,
        provider: &'a str,
        region: &'a str,
        book: &'a PriceBook,
        e: EntityRef<'a>,
        asm: &'a Resolver,
    ) -> Self {
        Ctx {
            p,
            cat,
            provider,
            region,
            book,
            e,
            asm,
            charges: Vec::new(),
            used: Vec::new(),
            notes: Vec::new(),
            missing: Vec::new(),
        }
    }

    // ------------------------------------------------------------ assumptions

    /// An assumption's value for this entity, recorded (once) as used.
    pub fn a(&mut self, key: &str) -> f64 {
        debug_assert!(
            super::assumption(key).is_some(),
            "model reads unknown assumption {key}"
        );
        let u = self.asm.get(key, self.e.id);
        let v = u.value;
        if !self.used.iter().any(|x| x.key == key) {
            self.used.push(u);
        }
        v
    }

    pub fn hours(&mut self) -> f64 {
        self.a("hours_per_month")
    }

    // ------------------------------------------------------------ fields

    /// An abstract field, or its default.
    pub fn field(&self, name: &str) -> Option<Value> {
        field_or_default(self.cat, self.provider, &self.e, name, false)
    }

    /// A provider-specific field, or its default.
    pub fn pfield(&self, name: &str) -> Option<Value> {
        field_or_default(self.cat, self.provider, &self.e, name, true)
    }

    pub fn num(&self, name: &str, default: f64) -> f64 {
        self.field(name).as_ref().and_then(num).unwrap_or(default)
    }

    pub fn flag(&self, name: &str) -> bool {
        self.field(name).and_then(|v| v.as_bool()).unwrap_or(false)
    }

    pub fn text(&self, name: &str) -> Option<String> {
        self.field(name).map(|v| v.display()).filter(|s| !s.is_empty())
    }

    pub fn ptext(&self, name: &str) -> Option<String> {
        self.pfield(name).map(|v| v.display()).filter(|s| !s.is_empty())
    }

    pub fn list_len(&self, name: &str) -> usize {
        match self.field(name) {
            Some(Value::List(l)) => l.len(),
            Some(Value::Records(r)) => r.len(),
            _ => 0,
        }
    }

    // ------------------------------------------------------------ the mapping

    fn mapping(&self) -> Option<&'a ProviderMapping> {
        self.cat.mapping(self.e.resource_type, self.provider)
    }

    fn block(&self, key: &str) -> Option<&'a BlockDef> {
        self.mapping()?.blocks.iter().find(|b| b.key == key)
    }

    /// Does the mapping emit this block for the entity (its `when` holds)?
    pub fn emits(&self, key: &str) -> bool {
        self.block(key).is_some_and(|b| {
            b.when
                .as_ref()
                .is_none_or(|c| condition_holds(self.p, self.cat, self.provider, &self.e, c, None))
        })
    }

    /// The value the mapping gives an argument, where it can be known without generating
    /// anything: literals, fields, provider fields, lookup tables and `if`s over them.
    /// `path` walks nested blocks: `node_config.machine_type`. This is how an estimate
    /// prices the SKU the export would ask for (`size = small` → `db.t3.micro`, or the
    /// instance class override).
    pub fn arg(&self, block: &str, path: &str) -> Option<Value> {
        let b = self.block(block)?;
        let parts: Vec<&str> = path.split('.').collect();
        let (mut args, mut nested) = (&b.args, &b.nested);
        let mut i = 0;
        // Nested blocks as far as they go, then arguments, then keys of object arguments
        // (`limits.cpu` in Cloud Run's `resources` block).
        while i + 1 < parts.len() {
            match nested.iter().find(|n| n.block == parts[i]) {
                Some(n) => {
                    (args, nested) = (&n.args, &n.nested);
                    i += 1;
                }
                None => break,
            }
        }
        let mut src = args.get(parts[i])?;
        for part in &parts[i + 1..] {
            match src {
                ArgSource::Object(o) => src = o.object.get(*part)?,
                _ => return None,
            }
        }
        self.resolve(src)
    }

    /// [`Ctx::arg`] as text; a list gives its first entry (EKS `instance_types`).
    pub fn sku(&self, block: &str, path: &str) -> Option<String> {
        match self.arg(block, path)? {
            Value::List(l) => l.first().cloned(),
            v => Some(v.display()).filter(|s| !s.is_empty()),
        }
    }

    fn resolve(&self, src: &ArgSource) -> Option<Value> {
        match src {
            ArgSource::Literal(l) => ttg_catalog::toml_to_value(&l.value),
            ArgSource::Field(f) => field_or_default(self.cat, self.provider, &self.e, &f.field, false)
                .filter(|v| !v.is_empty())
                .or_else(|| f.fallback.as_deref().and_then(|s| self.resolve(s))),
            ArgSource::ProviderField(f) => {
                field_or_default(self.cat, self.provider, &self.e, &f.provider_field, true)
                    .filter(|v| !v.is_empty())
                    .or_else(|| f.fallback.as_deref().and_then(|s| self.resolve(s)))
            }
            ArgSource::Map(m) => {
                let key = self
                    .field(&m.map)
                    .or_else(|| self.pfield(&m.map))
                    .filter(|v| !v.is_empty())?
                    .display();
                m.table.get(&key).and_then(ttg_catalog::toml_to_value)
            }
            ArgSource::If(i) => {
                if condition_holds(self.p, self.cat, self.provider, &self.e, &i.cond, None) {
                    self.resolve(&i.then)
                } else {
                    i.otherwise.as_deref().and_then(|s| self.resolve(s))
                }
            }
            _ => None,
        }
    }

    /// Targets this entity links to with a relation (containment counts where the
    /// definition says so), optionally of one type.
    pub fn targets(&self, rel: Relation, target_type: Option<&str>) -> Vec<String> {
        relation_targets_of_type(self.p, self.cat, &self.e, rel, target_type)
    }

    // ------------------------------------------------------------ charges

    /// Add `quantity` of a price-list row to the line. A row the list does not have is
    /// recorded as missing (the line then says what it could not price) and costs 0.
    pub fn charge(&mut self, item: impl Into<String>, table: &str, sku: &str, quantity: f64) {
        let item = item.into();
        match self.book.price(table, sku, self.region) {
            Some(price) => {
                if price.region != self.region && price.region != "*" {
                    self.notes.push(format!(
                        "{sku}: no {} price bundled, so the {} price is used",
                        self.region, price.region
                    ));
                }
                self.charges.push(Charge {
                    item,
                    table: table.to_string(),
                    sku: sku.to_string(),
                    quantity,
                    unit: price.unit,
                    unit_price: price.value,
                    monthly: quantity * price.value,
                });
            }
            None => self.missing.push(format!("{item} ({table} / {sku})")),
        }
    }

    /// A caveat about this line, shown with it.
    pub fn note(&mut self, text: impl Into<String>) {
        self.notes.push(text.into());
    }
}

/// A number from a field value: ints, floats and numeric strings (`storage = "64"`).
pub(crate) fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        Value::Str(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Nodes running on average over a month for a pool: its desired count clamped to its
/// range, or — when it may scale to zero — the node-hours a day the user assumes. Returns
/// node-hours in the month.
pub(crate) fn pool_node_hours(c: &mut Ctx, desired: f64, min: f64, max: f64) -> f64 {
    let hours = c.hours();
    if min <= 0.0 {
        let per_day = c.a("pool_node_hours_per_day");
        c.note(format!(
            "scales to zero (min 0): priced at {per_day} node-hours a day rather than around the clock"
        ));
        return per_day * hours / 24.0;
    }
    let n = desired.max(min).min(max.max(min));
    n * hours
}

/// A quantity for a label: no trailing zeros, thousands grouped.
pub(crate) fn fmt(x: f64) -> String {
    if x >= 1000.0 && x.fract() == 0.0 {
        let s = format!("{}", x as i64);
        let mut out = String::new();
        for (i, ch) in s.chars().enumerate() {
            if i > 0 && (s.len() - i) % 3 == 0 {
                out.push(',');
            }
            out.push(ch);
        }
        out
    } else if x.fract() == 0.0 {
        format!("{}", x as i64)
    } else {
        format!("{:.1}", x)
    }
}
