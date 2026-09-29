//! The bundled price lists (`definitions/prices/<provider>.toml`), compiled in so the
//! estimate works offline. Parsed once per process.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// The embedded files, one per provider. Adding a provider means adding a file here and a
/// model module next to this one.
const SOURCES: &[(&str, &str)] = &[
    ("aws", include_str!("../../../../definitions/prices/aws.toml")),
    ("azure", include_str!("../../../../definitions/prices/azure.toml")),
    ("gcp", include_str!("../../../../definitions/prices/gcp.toml")),
];

/// One provider's price list.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceBook {
    pub schema_version: u32,
    pub provider: String,
    /// Always `USD` today; kept in the file so a reader never has to guess.
    pub currency: String,
    /// When the figures were compiled (`YYYY-MM-DD`). Shown with every estimate.
    pub retrieved: String,
    /// Regions that have a column in the tables.
    pub regions: Vec<String>,
    /// The region whose prices stand in for one that has no column.
    pub fallback_region: String,
    /// What kind of price these are (on-demand, no discounts, …), shown with the estimate.
    pub basis: String,
    /// Abstract types that cost nothing on this provider, with the reason.
    #[serde(default)]
    pub free: BTreeMap<String, String>,
    /// Abstract types the estimate does not price on this provider, with the reason.
    #[serde(default)]
    pub not_estimated: BTreeMap<String, String>,
    #[serde(default)]
    pub tables: BTreeMap<String, Table>,
}

/// Prices for one service in one unit (a row may override the unit).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Table {
    pub unit: String,
    #[serde(default)]
    pub description: String,
    /// Where the figures come from.
    #[serde(default)]
    pub source: String,
    /// Which figures were cross-checked against the provider's own list, when and how.
    #[serde(default)]
    pub checked: String,
    #[serde(default, deserialize_with = "rows")]
    pub rows: BTreeMap<String, Row>,
}

/// One SKU: its price per region (`*` = every region) and, optionally, its own unit.
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub unit: Option<String>,
    pub prices: BTreeMap<String, f64>,
}

fn rows<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BTreeMap<String, Row>, D::Error> {
    use serde::de::Error;
    let raw: BTreeMap<String, BTreeMap<String, toml::Value>> = BTreeMap::deserialize(d)?;
    let mut out = BTreeMap::new();
    for (sku, cols) in raw {
        let mut row = Row::default();
        for (k, v) in cols {
            match (k.as_str(), v) {
                ("unit", toml::Value::String(u)) => row.unit = Some(u),
                (_, toml::Value::Float(f)) => {
                    row.prices.insert(k, f);
                }
                (_, toml::Value::Integer(i)) => {
                    row.prices.insert(k, i as f64);
                }
                (_, other) => {
                    return Err(D::Error::custom(format!(
                        "row '{sku}': '{k}' must be a price (number) or `unit` (text), not {other}"
                    )))
                }
            }
        }
        out.insert(sku, row);
    }
    Ok(out)
}

/// A price found for a row: the figure, its unit, and the region it is for.
#[derive(Debug, Clone, PartialEq)]
pub struct Price {
    pub value: f64,
    pub unit: String,
    /// The region the figure belongs to: the one asked for, `*`, or the fallback region
    /// when the row has no column for the one asked for.
    pub region: String,
}

impl PriceBook {
    /// The bundled list for a provider, or `None` for a provider without one.
    pub fn for_provider(provider: &str) -> Option<&'static PriceBook> {
        static BOOKS: OnceLock<BTreeMap<String, PriceBook>> = OnceLock::new();
        BOOKS
            .get_or_init(|| {
                SOURCES
                    .iter()
                    .map(|(id, text)| {
                        let b = PriceBook::parse(text)
                            .unwrap_or_else(|e| panic!("definitions/prices/{id}.toml is invalid: {e}"));
                        (id.to_string(), b)
                    })
                    .collect()
            })
            .get(provider)
    }

    /// Parse a price list (the embedded ones, or a candidate one in a test).
    pub fn parse(text: &str) -> Result<PriceBook, String> {
        let b: PriceBook = toml::from_str(text).map_err(|e| e.to_string())?;
        if b.currency != "USD" {
            return Err(format!("currency is {}; the estimate only knows USD", b.currency));
        }
        if !b.regions.contains(&b.fallback_region) {
            return Err(format!(
                "fallback region {} is not one of the regions",
                b.fallback_region
            ));
        }
        Ok(b)
    }

    /// Providers with a bundled list.
    pub fn providers() -> Vec<&'static str> {
        SOURCES.iter().map(|(id, _)| *id).collect()
    }

    /// The price of a row in a region: that region's column, else the row's `*`, else the
    /// fallback region's column (reported as such so the caller can say so).
    pub fn price(&self, table: &str, sku: &str, region: &str) -> Option<Price> {
        let t = self.tables.get(table)?;
        let row = t.rows.get(sku)?;
        let unit = row.unit.clone().unwrap_or_else(|| t.unit.clone());
        let pick = |r: &str| row.prices.get(r).map(|v| (*v, r.to_string()));
        let (value, region) = pick(region)
            .or_else(|| pick("*"))
            .or_else(|| pick(&self.fallback_region))?;
        Some(Price { value, unit, region })
    }

    /// Does the list have a column for this region?
    pub fn has_region(&self, region: &str) -> bool {
        self.regions.iter().any(|r| r == region)
    }
}
