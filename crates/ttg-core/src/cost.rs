//! Cost-estimate settings stored in the project: the usage assumptions an estimate is
//! made under, and an optional display currency. The estimate itself — prices, models,
//! totals — is `ttg_codegen::cost`; this is only what the user edits and saves.

use crate::ir::Id;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How much traffic and data the estimate assumes before any single figure is overridden.
/// `Medium` is the estimator's defaults; `Small` and `Large` scale the volume assumptions
/// (data, requests, messages, log GB) down and up by ten (`ttg_codegen::cost`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum UsagePreset {
    Small,
    #[default]
    Medium,
    Large,
}

impl UsagePreset {
    pub const ALL: [UsagePreset; 3] = [UsagePreset::Small, UsagePreset::Medium, UsagePreset::Large];

    pub fn as_str(self) -> &'static str {
        match self {
            UsagePreset::Small => "small",
            UsagePreset::Medium => "medium",
            UsagePreset::Large => "large",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s.to_ascii_lowercase())
    }
}

/// Usage assumptions behind the cost estimate, keyed by assumption name
/// (`bucket_gb`, `pool_node_hours_per_day`, …; `ttg_codegen::cost::ASSUMPTIONS` lists
/// them with their defaults). A key that is absent takes the estimator's default, so an
/// empty value is the common case and is not written to the file.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CostAssumptions {
    /// The usage level the defaults are taken at; absent means `medium`. Values below win
    /// over it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<UsagePreset>,
    /// Project-wide values, e.g. `{ "bucket_gb": 200, "log_ingest_gb": 2 }`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, f64>,
    /// Per-entity overrides: entity id → assumption → value, e.g. one bucket that holds
    /// two terabytes of recordings. They win over `values`. Entries for entities that no
    /// longer exist are ignored.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub entities: BTreeMap<Id, BTreeMap<String, f64>>,
}

impl CostAssumptions {
    pub fn is_empty(&self) -> bool {
        self.preset.is_none() && self.values.is_empty() && self.entities.values().all(|m| m.is_empty())
    }
}

/// Show estimates in another currency too. Prices are bundled in US dollars; this is a
/// fixed rate the user sets and dates, never fetched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DisplayCurrency {
    /// ISO code shown next to the converted figures, e.g. `GBP`.
    pub code: String,
    /// Units of `code` per US dollar, e.g. `0.75`.
    pub rate: f64,
    /// When the rate was taken, free text (e.g. `2026-09-29`), shown beside it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub date: String,
}
