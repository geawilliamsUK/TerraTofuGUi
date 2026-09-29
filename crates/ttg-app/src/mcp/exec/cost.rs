//! `cost_estimate`: the monthly estimate (`ttg_codegen::cost`) as the agent sees it.
//!
//! A read: nothing about the project changes, and the `assumptions` a call passes are
//! for that call only. The reply is shaped by `group_by` so a large design does not come
//! back as one entry per security group: free entities are summarised per type.

use super::R;
use crate::app::TtgApp;
use serde_json::{json, Map, Value as J};
use std::collections::{BTreeMap, BTreeSet};
use ttg_codegen::cost::{self, Line, Status, ASSUMPTIONS};

/// The tool's arguments.
pub(super) struct CostQuery {
    pub provider: Option<String>,
    pub environment: Option<String>,
    pub view: Option<String>,
    pub group_by: Option<String>,
    pub assumptions: Option<Map<String, J>>,
    pub region: Option<String>,
}

fn line_json(l: &Line) -> J {
    json!({
        "entity": l.entity,
        "name": l.name,
        "type": l.resource_type,
        "status": l.status,
        "monthly": l.monthly,
        "charges": l.charges.iter().map(|c| json!({
            "item": c.item,
            "quantity": c.quantity,
            "unit": c.unit,
            "unit_price": c.unit_price,
            "monthly": c.monthly,
            "price": format!("{} / {}", c.table, c.sku),
        })).collect::<Vec<_>>(),
        "assumptions": l.assumptions,
        "notes": l.notes,
    })
}

impl TtgApp {
    pub(super) fn cost_estimate_json(&mut self, q: CostQuery) -> R {
        let provider = q
            .provider
            .unwrap_or_else(|| self.project.settings.target_provider.clone());
        if self.catalog.provider(&provider).is_none() {
            return Err(format!(
                "unknown provider \"{provider}\" (one of {})",
                self.catalog.provider_ids().join(", ")
            ));
        }
        let mut notes: Vec<String> = Vec::new();
        // Named environments are not part of the project yet. When they are, the project
        // resolved for the environment is what gets estimated; the estimate itself only
        // ever sees a `&Project`.
        let project = &self.project;
        if let Some(env) = &q.environment {
            notes.push(format!(
                "environment \"{env}\" ignored: this project has no named environments, so it is priced as drawn"
            ));
        }
        let mut opts = cost::Options {
            region: q.region.clone(),
            ..Default::default()
        };
        for (k, v) in q.assumptions.unwrap_or_default() {
            if cost::assumption(&k).is_none() {
                return Err(format!(
                    "unknown assumption \"{k}\"; the estimate reads: {}",
                    ASSUMPTIONS.iter().map(|a| a.key).collect::<Vec<_>>().join(", ")
                ));
            }
            let n = v
                .as_f64()
                .ok_or_else(|| format!("assumption \"{k}\" must be a number, not {v}"))?;
            opts.assumptions.insert(k, n);
        }
        let est = cost::estimate_with(project, &self.catalog, &provider, &opts)?;
        notes.extend(est.notes.iter().cloned());

        // The view the reply is about, if any.
        let view = match &q.view {
            None => None,
            Some(name) => Some(
                est.views
                    .iter()
                    .find(|v| v.view.eq_ignore_ascii_case(name))
                    .ok_or_else(|| {
                        format!(
                            "no saved view called \"{name}\" (views: {})",
                            est.views
                                .iter()
                                .map(|v| v.view.clone())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?,
            ),
        };
        let in_scope: Option<BTreeSet<&str>> = view.and_then(|vt| {
            let v = project.views.iter().find(|v| v.name == vt.view)?;
            ttg_codegen::views::visible_set(project, &self.catalog, &v.filter).map(|s| {
                est.lines
                    .iter()
                    .filter(|l| s.contains(&l.entity))
                    .map(|l| l.entity.as_str())
                    .collect()
            })
        });
        let lines: Vec<&Line> = est
            .lines
            .iter()
            .filter(|l| in_scope.as_ref().is_none_or(|s| s.contains(l.entity.as_str())))
            .collect();
        let monthly = view.map(|v| v.monthly).unwrap_or(est.monthly);

        let group_by = q.group_by.as_deref().unwrap_or("entity");
        let body = match group_by {
            "entity" => {
                let mut priced: Vec<&&Line> = lines.iter().filter(|l| l.status == Status::Priced).collect();
                priced.sort_by(|a, b| {
                    b.monthly
                        .partial_cmp(&a.monthly)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                json!({ "lines": priced.iter().map(|l| line_json(l)).collect::<Vec<_>>() })
            }
            "type" => {
                let ids: BTreeSet<&str> = lines.iter().map(|l| l.entity.as_str()).collect();
                let rows: Vec<J> = est
                    .by_type
                    .iter()
                    .filter_map(|s| {
                        let mine: Vec<&String> = s.entities.iter().filter(|e| ids.contains(e.as_str())).collect();
                        if mine.is_empty() {
                            return None;
                        }
                        let total: f64 = lines
                            .iter()
                            .filter(|l| l.resource_type == s.key)
                            .map(|l| l.monthly)
                            .sum();
                        Some(json!({ "type": s.key, "label": s.label, "count": mine.len(), "monthly": (total * 100.0).round() / 100.0 }))
                    })
                    .collect();
                json!({ "types": rows })
            }
            "group" => {
                let views: Vec<&cost::ViewTotal> = match view {
                    Some(v) => vec![v],
                    None => est.views.iter().collect(),
                };
                if views.iter().all(|v| v.groups.is_empty()) {
                    notes.push("no view here has groups; add labelled boxes to a view to total them".into());
                }
                let names = |ids: &[String]| -> Vec<String> {
                    ids.iter()
                        .filter_map(|id| project.entity(id).map(|e| e.name.to_string()))
                        .collect()
                };
                json!({ "groups": views.iter().flat_map(|v| v.groups.iter().map(move |g| (v, g))).map(|(v, g)| json!({
                    "view": v.view,
                    "group": g.label,
                    "monthly": g.monthly,
                    "entities": names(&g.entities),
                    "not_estimated": g.not_estimated,
                })).collect::<Vec<_>>() })
            }
            other => {
                return Err(format!("group_by must be entity, type or group, not \"{other}\""));
            }
        };

        // What costs nothing, per type, and what could not be priced, with why.
        let mut free: BTreeMap<&str, (String, Vec<&str>)> = BTreeMap::new();
        for l in lines.iter().filter(|l| l.status == Status::Free) {
            free.entry(l.resource_type.as_str())
                .or_insert_with(|| (l.notes.first().cloned().unwrap_or_default(), Vec::new()))
                .1
                .push(l.name.as_str());
        }
        let not_estimated: Vec<J> = lines
            .iter()
            .filter(|l| l.status == Status::NotEstimated)
            .map(|l| json!({ "entity": l.entity, "name": l.name, "type": l.resource_type, "why": l.notes.join("; ") }))
            .collect();

        let mut out = json!({
            "provider": est.provider,
            "region": est.region,
            "price_region": est.price_region,
            "currency": est.currency,
            "prices_retrieved": est.prices_retrieved,
            "view": view.map(|v| v.view.clone()),
            "monthly": monthly,
            "converted": est.converted.as_ref().map(|c| json!({
                "code": c.code,
                "rate": c.rate,
                "date": c.date,
                "monthly": (monthly * c.rate * 100.0).round() / 100.0,
            })),
            "group_by": group_by,
            "free": free.into_iter().map(|(t, (why, names))| json!({ "type": t, "why": why, "entities": names })).collect::<Vec<_>>(),
            "not_estimated": not_estimated,
            "views": est.views.iter().map(|v| json!({ "view": v.view, "monthly": v.monthly, "entities": v.entities })).collect::<Vec<_>>(),
            "assumptions": est.assumptions,
            "notes": notes,
            "caveat": est.caveat,
        });
        if let (J::Object(o), J::Object(b)) = (&mut out, body) {
            o.extend(b);
        }
        Ok(out)
    }
}
