//! Configuration values stored on nodes and containers.
//!
//! The IR keeps values deliberately untyped-ish: the *type* of a field (cidr, enum, ...)
//! is declared in the resource definition, not in the project file, so the project file
//! stays a plain JSON subset that any tool can read.

use serde::{Deserialize, Serialize};
use std::fmt;

/// One row of a `struct_list` field: sub-field name -> value.
pub type Record = std::collections::BTreeMap<String, Value>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<String>),
    /// Rows of a `struct_list` field (schema_version 2 definitions).
    Records(Vec<Record>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }
    pub fn as_list(&self) -> Option<&[String]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }
    pub fn as_records(&self) -> Option<&[Record]> {
        match self {
            Value::Records(r) => Some(r),
            _ => None,
        }
    }
    /// True when the value carries no information (empty string / empty list).
    pub fn is_empty(&self) -> bool {
        match self {
            Value::Str(s) => s.trim().is_empty(),
            Value::List(l) => l.is_empty(),
            Value::Records(r) => r.is_empty(),
            _ => false,
        }
    }
    /// Human-readable rendering for templates and manual-step documents.
    pub fn display(&self) -> String {
        match self {
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Str(s) => s.clone(),
            Value::List(l) => l.join(", "),
            Value::Records(r) => format!("{} item(s)", r.len()),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(s.to_string())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s)
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}
impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}
