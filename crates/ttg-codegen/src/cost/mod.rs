//! Monthly cost estimate for one provider (R3.12).
//!
//! Three pieces, kept apart so each can be maintained on its own:
//!
//! * **Prices** (`definitions/prices/<provider>.toml`, [`PriceBook`]): bundled list
//!   prices in US dollars for a few regions, one row per SKU, each table with its source
//!   and a note of what was cross-checked. Compiled in, so the estimate works offline; the
//!   refresh procedure is `docs/PRICES.md`.
//! * **Models** (`aws.rs`, `azure.rs`, `gcp.rs`): how one entity of an abstract type turns
//!   into billable units on a provider. They read the same fields the mappings read and
//!   resolve the same concrete SKU the export would ask for, through the mapping itself
//!   ([`model::Ctx::arg`]). Types that cost nothing, or that the estimate does not price,
//!   are listed with a reason in the price file; a test refuses a mapped type that is in
//!   none of the three places.
//! * **Assumptions** ([`ASSUMPTIONS`]): usage the diagram does not say (GB in a bucket,
//!   node-hours a day of a GPU pool that scales to zero). Defaults here, overridden per
//!   project and per entity in `settings.cost_assumptions`.
//!
//! [`estimate`] prices the provider's layer of a project — the same entities its export
//! would create. It takes a `&Project`, so a caller that has resolved a named environment
//! into a project of its own can estimate that. Figures are list-price estimates and every
//! surface shows [`Estimate::caveat`] with them.

mod assumptions;
mod aws;
mod azure;
mod gcp;
mod model;
mod prices;
#[cfg(test)]
mod tests;

pub use assumptions::{assumption, Assumption, Resolver, Source, Used, ASSUMPTIONS};
pub use prices::{Price, PriceBook, Row, Table};

use crate::diagnostics::{Code, Diagnostic, Severity};
use model::{Ctx, Model};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use ttg_catalog::{Catalog, MappingStatus};
use ttg_core::{Id, Project};

/// Whether a line has a figure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Priced (possibly at 0 when the usage assumed is 0).
    Priced,
    /// Costs nothing on this provider (or is logical there), with the reason.
    Free,
    /// Not priced: the reason says why. Its cost is missing from the totals.
    NotEstimated,
}

/// One billable item of a line: `quantity` × `unit_price` = `monthly`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Charge {
    /// What is being paid for, in words ("db.t4g.medium, Multi-AZ (2 instances)").
    pub item: String,
    /// Price-list table and row the unit price comes from.
    pub table: String,
    pub sku: String,
    pub quantity: f64,
    pub unit: String,
    pub unit_price: f64,
    pub monthly: f64,
}

/// One entity's estimate.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Line {
    pub entity: Id,
    pub name: String,
    pub resource_type: String,
    pub status: Status,
    /// US dollars a month.
    pub monthly: f64,
    pub charges: Vec<Charge>,
    /// Assumptions the figure depends on, with where each value came from.
    pub assumptions: Vec<Used>,
    /// Why it is free / not estimated, what could not be priced, and other caveats.
    pub notes: Vec<String>,
}

/// A sum over some lines.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Subtotal {
    /// Type id, view name or group id.
    pub key: String,
    pub label: String,
    pub monthly: f64,
    /// Entities counted.
    pub entities: Vec<Id>,
    /// Of those, how many have no figure.
    pub not_estimated: usize,
}

/// A saved view's total, and its groups'.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ViewTotal {
    pub view: String,
    pub monthly: f64,
    pub entities: usize,
    pub not_estimated: usize,
    /// Per labelled box of the view (members are what the box holds and the view shows;
    /// a nested box's members count in its parent's total too).
    pub groups: Vec<Subtotal>,
}

/// The display currency, converted.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Converted {
    pub code: String,
    pub rate: f64,
    pub date: String,
    pub monthly: f64,
}

/// A whole estimate.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Estimate {
    pub provider: String,
    /// The project's region for the provider.
    pub region: String,
    /// The region whose prices were used (the list's fallback when the project's region
    /// has no column).
    pub price_region: String,
    /// Always `USD`.
    pub currency: String,
    /// Date the bundled prices were compiled.
    pub prices_retrieved: String,
    /// What kind of price the list holds.
    pub basis: String,
    /// US dollars a month, over the priced lines.
    pub monthly: f64,
    /// The same in the display currency, when one is set.
    pub converted: Option<Converted>,
    pub lines: Vec<Line>,
    /// Totals per abstract type, largest first.
    pub by_type: Vec<Subtotal>,
    /// Totals per saved view, with the view's groups.
    pub views: Vec<ViewTotal>,
    /// Every assumption at its project-wide value.
    pub assumptions: Vec<Used>,
    /// Estimate-wide notes: fallback region, entities left out of this provider's layer,
    /// unknown assumption keys.
    pub notes: Vec<String>,
    /// The honesty line every surface shows with the figures.
    pub caveat: String,
}

/// Options beyond the provider.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Price as if the project were in this region.
    pub region: Option<String>,
    /// Assumption values for this estimate only, on top of the project's.
    pub assumptions: BTreeMap<String, f64>,
}

/// Estimate one provider's monthly cost at the project's region (or `region`).
pub fn estimate(
    project: &Project,
    catalog: &Catalog,
    provider: &str,
    region: Option<&str>,
) -> Result<Estimate, String> {
    estimate_with(
        project,
        catalog,
        provider,
        &Options {
            region: region.map(str::to_string),
            ..Options::default()
        },
    )
}

/// The provider's region as the project sets it (`region` / `location` provider
/// variable), or the variable's default.
pub fn project_region(project: &Project, catalog: &Catalog, provider: &str) -> Option<String> {
    let pdef = catalog.provider(provider)?;
    let var = pdef
        .variables
        .iter()
        .find(|v| v.name == "region" || v.name == "location")?;
    project
        .settings
        .provider_settings
        .get(provider)
        .and_then(|m| m.get(&var.name))
        .filter(|s| !s.is_empty())
        .cloned()
        .or_else(|| var.default.as_ref().and_then(|d| d.as_str().map(str::to_string)))
}

/// [`estimate`] with per-call options.
pub fn estimate_with(
    project: &Project,
    catalog: &Catalog,
    provider: &str,
    opts: &Options,
) -> Result<Estimate, String> {
    let book = PriceBook::for_provider(provider).ok_or_else(|| {
        format!(
            "no bundled prices for provider \"{provider}\" (have: {})",
            PriceBook::providers().join(", ")
        )
    })?;
    // Price what the export would create: the provider's layer.
    let layer = crate::layers::project_for(project, catalog, provider);
    Ok(build(project, &layer, catalog, provider, opts, book))
}

/// The estimate of `layer` (the provider's layer of `project`); views and groups are
/// read from `project`, where their positions and filters live.
fn build(
    project: &Project,
    layer: &Project,
    catalog: &Catalog,
    provider: &str,
    opts: &Options,
    book: &'static PriceBook,
) -> Estimate {
    let mut notes = Vec::new();
    let region = opts
        .region
        .clone()
        .or_else(|| project_region(project, catalog, provider))
        .unwrap_or_else(|| book.fallback_region.clone());
    let price_region = if book.has_region(&region) {
        region.clone()
    } else {
        notes.push(format!(
            "no prices are bundled for {region}; {} prices are used instead (bundled: {})",
            book.fallback_region,
            book.regions.join(", ")
        ));
        book.fallback_region.clone()
    };
    let asm = Resolver::new(&project.settings, &opts.assumptions);
    for k in asm.unknown_keys() {
        notes.push(format!(
            "assumption \"{k}\" is not one the estimate reads; it is ignored"
        ));
    }
    let left_out = project.entities().len() - layer.entities().len();
    if left_out > 0 {
        notes.push(format!(
            "{left_out} entit{} not part of the {provider} layer (tagged for other providers, no counterpart, or left out by a check) and cost nothing here",
            if left_out == 1 { "y is" } else { "ies are" }
        ));
    }
    let lines: Vec<Line> = layer
        .entities()
        .into_iter()
        .map(|e| {
            let mut c = Ctx::new(layer, catalog, provider, &price_region, book, e, &asm);
            price_entity(&mut c)
        })
        .collect();
    let monthly = round2(lines.iter().map(|l| l.monthly).sum());

    let mut by_type: BTreeMap<String, Subtotal> = BTreeMap::new();
    for l in &lines {
        let s = by_type
            .entry(l.resource_type.clone())
            .or_insert_with(|| Subtotal {
                key: l.resource_type.clone(),
                label: catalog
                    .resource(&l.resource_type)
                    .map(|d| d.resource.display_name.clone())
                    .unwrap_or_else(|| l.resource_type.clone()),
                monthly: 0.0,
                entities: Vec::new(),
                not_estimated: 0,
            });
        add_to(s, l);
    }
    let mut by_type: Vec<Subtotal> = by_type.into_values().collect();
    sort_subtotals(&mut by_type);

    let views = project
        .views
        .iter()
        .map(|v| view_total(project, catalog, v, &lines))
        .collect();

    let converted = project.settings.cost_currency.as_ref().map(|c| Converted {
        code: c.code.clone(),
        rate: c.rate,
        date: c.date.clone(),
        monthly: round2(monthly * c.rate),
    });
    let assumptions = ASSUMPTIONS.iter().map(|a| asm.global(a.key)).collect();
    Estimate {
        provider: provider.to_string(),
        region,
        price_region,
        currency: book.currency.clone(),
        prices_retrieved: book.retrieved.clone(),
        basis: book.basis.clone(),
        monthly,
        converted,
        lines,
        by_type,
        views,
        assumptions,
        notes,
        caveat: format!(
            "Estimate from list prices retrieved {}, in US dollars: {}. Usage figures are assumptions you can edit. Not a quote: check the provider's own calculator before committing to a budget.",
            book.retrieved, book.basis
        ),
    }
}

fn add_to(s: &mut Subtotal, l: &Line) {
    s.monthly = round2(s.monthly + l.monthly);
    s.entities.push(l.entity.clone());
    if l.status == Status::NotEstimated {
        s.not_estimated += 1;
    }
}

fn sort_subtotals(v: &mut [Subtotal]) {
    v.sort_by(|a, b| {
        b.monthly
            .partial_cmp(&a.monthly)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.key.cmp(&b.key))
    });
}

/// A saved view's share: the lines of the entities it shows, and per group.
pub fn view_total(project: &Project, catalog: &Catalog, v: &ttg_core::View, lines: &[Line]) -> ViewTotal {
    let visible = crate::views::visible_set(project, catalog, &v.filter);
    let shows = |id: &str| visible.as_ref().is_none_or(|s| s.contains(id));
    let in_view: Vec<&Line> = lines.iter().filter(|l| shows(&l.entity)).collect();
    let groups = v
        .groups
        .iter()
        .map(|g| {
            let members: BTreeSet<Id> = ttg_core::view::group_members(project, v, &g.id, &shows)
                .into_iter()
                .collect();
            let mut s = Subtotal {
                key: g.id.clone(),
                label: g.label.clone(),
                monthly: 0.0,
                entities: Vec::new(),
                not_estimated: 0,
            };
            for l in in_view.iter().filter(|l| members.contains(&l.entity)) {
                add_to(&mut s, l);
            }
            s
        })
        .collect();
    ViewTotal {
        view: v.name.clone(),
        monthly: round2(in_view.iter().map(|l| l.monthly).sum()),
        entities: in_view.len(),
        not_estimated: in_view
            .iter()
            .filter(|l| l.status == Status::NotEstimated)
            .count(),
        groups,
    }
}

/// Cents, and never `-0.00` (an empty `f64` sum is negative zero).
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0 + 0.0
}

/// The model a provider has for a type.
fn model_for(provider: &str, type_id: &str) -> Model {
    match provider {
        "aws" => aws::model(type_id),
        "azure" => azure::model(type_id),
        "gcp" => gcp::model(type_id),
        _ => Model::Missing,
    }
}

/// How the estimate answers for a type on a provider, without an entity: priced, free or
/// not estimated (with the reason), or silently missing. The coverage test and the
/// catalog listing use it.
pub fn coverage(catalog: &Catalog, provider: &str, type_id: &str) -> Coverage {
    let Some(book) = PriceBook::for_provider(provider) else {
        return Coverage::NotEstimated(format!("no bundled prices for {provider}"));
    };
    if ttg_catalog::Catalog::is_native(type_id) {
        return Coverage::NotEstimated(
            "native provider resources are not priced; only curated types are".into(),
        );
    }
    match catalog.mapping(type_id, provider) {
        None => {
            return Coverage::NotEstimated(format!(
                "{type_id} has no {provider} mapping, so nothing is created"
            ))
        }
        Some(m) if m.status == MappingStatus::Logical => {
            return Coverage::Free(format!("logical on {provider}: nothing is created"))
        }
        Some(_) => {}
    }
    if let Some(r) = book.free.get(type_id) {
        return Coverage::Free(r.clone());
    }
    if let Some(r) = book.not_estimated.get(type_id) {
        return Coverage::NotEstimated(r.clone());
    }
    match model_for(provider, type_id) {
        Model::Priced(_) => Coverage::Priced,
        Model::Missing => Coverage::Missing,
    }
}

/// See [`coverage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Coverage {
    Priced,
    Free(String),
    NotEstimated(String),
    /// A mapped type nothing answers for: a bug the coverage test catches.
    Missing,
}

fn price_entity(c: &mut Ctx) -> Line {
    let e = c.e;
    let mut line = Line {
        entity: e.id.to_string(),
        name: e.name.to_string(),
        resource_type: e.resource_type.to_string(),
        status: Status::Priced,
        monthly: 0.0,
        charges: Vec::new(),
        assumptions: Vec::new(),
        notes: Vec::new(),
    };
    if e.manual {
        line.status = Status::NotEstimated;
        line.notes
            .push("external (managed by hand): not created by this project, so not priced".into());
        return line;
    }
    match coverage(c.cat, c.provider, e.resource_type) {
        Coverage::Free(r) => {
            line.status = Status::Free;
            line.notes.push(r);
            return line;
        }
        Coverage::NotEstimated(r) => {
            line.status = Status::NotEstimated;
            line.notes.push(r);
            return line;
        }
        Coverage::Missing => {
            line.status = Status::NotEstimated;
            line.notes
                .push(format!("no cost model for {} on {}", e.resource_type, c.provider));
            return line;
        }
        Coverage::Priced => {}
    }
    if let Model::Priced(f) = model_for(c.provider, e.resource_type) {
        f(c);
    }
    line.charges = std::mem::take(&mut c.charges);
    for ch in &mut line.charges {
        ch.monthly = round2(ch.monthly);
    }
    line.monthly = round2(line.charges.iter().map(|ch| ch.monthly).sum());
    line.assumptions = std::mem::take(&mut c.used);
    // A fallback-region note comes once per charge; say it once.
    let mut seen = BTreeSet::new();
    line.notes = std::mem::take(&mut c.notes)
        .into_iter()
        .filter(|n| seen.insert(n.clone()))
        .collect();
    if !c.missing.is_empty() {
        let what = c.missing.join(", ");
        if line.charges.is_empty() {
            line.status = Status::NotEstimated;
            line.notes
                .insert(0, format!("no bundled price for {what} in {}", c.region));
        } else {
            line.notes.insert(
                0,
                format!("not included (no bundled price in {}): {what}", c.region),
            );
        }
    }
    line
}

/// A warning on each `budget` entity whose monthly limit the estimate exceeds, for the
/// target provider. `layer` is the provider's layer (what [`crate::diagnostics::run`]
/// works on); the budget's limit is read in the currency the provider bills in, which for
/// the bundled lists is US dollars.
pub fn budget_diagnostics(layer: &Project, catalog: &Catalog, provider: &str) -> Vec<Diagnostic> {
    let budgets: Vec<_> = layer
        .entities()
        .into_iter()
        .filter(|e| e.resource_type == "budget" && !e.manual)
        .collect();
    if budgets.is_empty() {
        return Vec::new();
    }
    let Some(book) = PriceBook::for_provider(provider) else {
        return Vec::new();
    };
    // `layer` already is the layer, so it is priced as it stands.
    let est = build(layer, layer, catalog, provider, &Options::default(), book);
    let mut out = Vec::new();
    for b in budgets {
        let limit = field_num(catalog, provider, &b, "monthly_limit");
        let Some(limit) = limit.filter(|l| *l > 0.0) else {
            continue;
        };
        if est.monthly > limit {
            out.push(Diagnostic {
                entity: Some(b.id.to_string()),
                severity: Severity::Warning,
                code: Code::Cost,
                message: format!(
                    "estimated ${:.0}/month exceeds the budget \"{}\" of ${:.0}; see the Cost window (list prices of {}, usage assumptions editable there)",
                    est.monthly, b.name, limit, est.prices_retrieved
                ),
                provider: None,
            });
        }
    }
    out
}

fn field_num(catalog: &Catalog, provider: &str, e: &ttg_core::EntityRef, name: &str) -> Option<f64> {
    crate::diagnostics::field_or_default(catalog, provider, e, name, false)
        .as_ref()
        .and_then(model::num)
}
