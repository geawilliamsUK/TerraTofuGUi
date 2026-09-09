//! `ttg-catalog` — loads and validates the data-driven resource and provider
//! definitions that map abstract IR types to concrete Terraform/OpenTofu resources.
//!
//! Definitions are TOML files (see `docs/MAPPING_FORMAT.md`). The built-in set under
//! `definitions/` is embedded at compile time so the desktop app is self-contained; a
//! directory can be loaded at runtime to test contributions without rebuilding.

pub mod fields;
pub mod load;
pub mod schema;
pub mod usage;
pub mod validate;

pub use load::Catalog;
pub use schema::*;

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("io error reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: {message}")]
    Parse { path: String, message: String },
    #[error("definition errors:\n{0}")]
    Invalid(String),
}
