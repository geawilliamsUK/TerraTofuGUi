//! `ttg cost`: the monthly cost estimate as a table (or JSON).

use anyhow::{bail, Result};
use std::path::Path;
use ttg_catalog::Catalog;
use ttg_codegen::cost::{self, Estimate, Status};

/// How the table groups the lines.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum By {
    /// One row per entity, largest first.
    Entity,
    /// One row per abstract type.
    Type,
}

pub struct Args<'a> {
    pub project: &'a Path,
    pub provider: Option<String>,
    pub region: Option<String>,
    pub json: bool,
    pub by: By,
    /// Also print every charge of every line.
    pub detail: bool,
    /// Price this named environment's values (`Project::for_environment`).
    pub environment: Option<String>,
}

pub fn run(cat: &mut Catalog, a: Args) -> Result<()> {
    let raw = ttg_core::project::load(a.project)?;
    if let Some(e) = &a.environment {
        if !raw.settings.environments.contains(e) {
            bail!(
                "no environment \"{e}\" (environments: {})",
                raw.settings.environments.join(", ")
            );
        }
    }
    let p = raw.resolved(a.environment.as_deref()).into_owned();
    cat.ensure_native_types(&p);
    let provider = a.provider.unwrap_or(p.settings.target_provider.clone());
    let est = match cost::estimate(&p, cat, &provider, a.region.as_deref()) {
        Ok(e) => e,
        Err(e) => bail!(e),
    };
    if a.json {
        println!("{}", serde_json::to_string_pretty(&est)?);
        return Ok(());
    }
    print_table(&p.name, &est, a.by, a.detail);
    Ok(())
}

fn money(x: f64) -> String {
    format!("${x:.2}")
}

fn print_table(name: &str, est: &Estimate, by: By, detail: bool) {
    println!(
        "{name}: {} {} (prices for {}, retrieved {}, {})",
        est.provider, est.region, est.price_region, est.prices_retrieved, est.currency
    );
    println!();
    match by {
        By::Entity => {
            let mut lines: Vec<_> = est.lines.iter().filter(|l| l.status == Status::Priced).collect();
            lines.sort_by(|a, b| {
                b.monthly
                    .partial_cmp(&a.monthly)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            println!("{:>11}  {:<24} {:<22} what", "per month", "entity", "type");
            for l in lines {
                let what = l
                    .charges
                    .iter()
                    .max_by(|a, b| {
                        a.monthly
                            .partial_cmp(&b.monthly)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|c| c.item.clone())
                    .unwrap_or_default();
                println!(
                    "{:>11}  {:<24} {:<22} {what}",
                    money(l.monthly),
                    l.name,
                    l.resource_type
                );
                if detail {
                    for c in &l.charges {
                        println!(
                            "{:>13}    {}: {} {} at {} ({} / {})",
                            money(c.monthly),
                            c.item,
                            fmt_qty(c.quantity),
                            c.unit,
                            money_unit(c.unit_price),
                            c.table,
                            c.sku
                        );
                    }
                    for u in &l.assumptions {
                        println!(
                            "{:>13}    assumes {} = {} {} ({:?})",
                            "", u.key, u.value, u.unit, u.source
                        );
                    }
                    for n in &l.notes {
                        println!("{:>13}    note: {n}", "");
                    }
                }
            }
        }
        By::Type => {
            println!("{:>11}  {:<28} count", "per month", "type");
            for s in &est.by_type {
                println!("{:>11}  {:<28} {}", money(s.monthly), s.label, s.entities.len());
            }
        }
    }
    println!();
    let converted = est
        .converted
        .as_ref()
        .map(|c| {
            format!(
                "  (about {:.2} {} at {} per USD{})",
                c.monthly,
                c.code,
                c.rate,
                if c.date.is_empty() {
                    String::new()
                } else {
                    format!(", {}", c.date)
                }
            )
        })
        .unwrap_or_default();
    println!("Total: {} a month{converted}", money(est.monthly));
    let not: Vec<_> = est
        .lines
        .iter()
        .filter(|l| l.status == Status::NotEstimated)
        .collect();
    if !not.is_empty() {
        println!("Not estimated ({}):", not.len());
        for l in not {
            println!("  {} ({}): {}", l.name, l.resource_type, l.notes.join("; "));
        }
    }
    let free = est.lines.iter().filter(|l| l.status == Status::Free).count();
    if free > 0 {
        println!("Free: {free} entities (networks, roles, security groups, ...)");
    }
    if !est.views.is_empty() {
        println!("Per view:");
        for v in &est.views {
            println!("  {:>11}  {}", money(v.monthly), v.view);
            for g in v.groups.iter().filter(|g| !g.entities.is_empty()) {
                println!("  {:>11}    {}", money(g.monthly), g.label);
            }
        }
    }
    for n in &est.notes {
        println!("Note: {n}");
    }
    println!();
    println!("{}", est.caveat);
}

fn fmt_qty(x: f64) -> String {
    if x.fract() == 0.0 {
        format!("{x}")
    } else {
        format!("{x:.3}")
    }
}

fn money_unit(x: f64) -> String {
    if x >= 0.01 {
        format!("${x:.4}")
    } else {
        format!("${x}")
    }
}
