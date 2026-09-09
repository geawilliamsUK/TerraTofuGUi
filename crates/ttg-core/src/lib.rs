//! `ttg-core` — the provider-neutral Intermediate Representation (IR) of a cloud
//! architecture diagram, plus project-file persistence and structural validation.
//!
//! This crate deliberately knows nothing about Terraform, HCL, AWS or Azure. It models
//! *intent* ("a subnet inside this network, attached to that role") so that
//! `ttg-catalog` and `ttg-codegen` can map that intent to concrete provider resources.

pub mod graph;
pub mod ir;
pub mod layout;
pub mod project;
pub mod validate;
pub mod value;

pub use ir::*;
pub use value::{Record, Value};

/// Current on-disk schema version for `.ttg.json` project files.
pub const SCHEMA_VERSION: u32 = 1;

/// Errors that can occur while loading or saving a project file.
#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("project file is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported project schema_version {found} (this build supports {supported})")]
    UnsupportedSchema { found: u32, supported: u32 },
    #[error("project is structurally invalid:\n{0}")]
    Invalid(String),
}
