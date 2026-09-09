//! Checking configured values against field definitions.

use crate::schema::{FieldDef, FieldType};
use ttg_core::Value;

/// Validate a value for a field. `Ok(())` means acceptable.
pub fn check_value(def: &FieldDef, value: Option<&Value>) -> Result<(), String> {
    let Some(v) = value else {
        return if def.required {
            Err("required".into())
        } else {
            Ok(())
        };
    };
    if v.is_empty() {
        return if def.required {
            Err("required".into())
        } else {
            Ok(())
        };
    }
    match def.field_type {
        FieldType::String => {
            let s = v.as_str().ok_or("expected a string")?;
            check_pattern(def, s)
        }
        FieldType::Bool => v
            .as_bool()
            .map(|_| ())
            .ok_or_else(|| "expected true/false".into()),
        FieldType::Int => v.as_int().map(|_| ()).ok_or_else(|| "expected an integer".into()),
        FieldType::Cidr => {
            let s = v.as_str().ok_or("expected a CIDR string")?;
            if is_cidr(s) {
                Ok(())
            } else {
                Err("not a valid CIDR block (e.g. 10.0.0.0/16)".into())
            }
        }
        FieldType::Enum => {
            let s = v.as_str().ok_or("expected one of the options")?;
            if def.options.iter().any(|o| o == s) {
                Ok(())
            } else {
                Err(format!("must be one of: {}", def.options.join(", ")))
            }
        }
        FieldType::StringList => match v {
            Value::List(_) => Ok(()),
            Value::Str(_) => Ok(()),
            _ => Err("expected a list of strings".into()),
        },
        FieldType::EntityRef => v
            .as_str()
            .map(|_| ())
            .ok_or_else(|| "expected a resource reference".into()),
        FieldType::StructList => {
            let rows = v.as_records().ok_or("expected a list of rows")?;
            for (n, row) in rows.iter().enumerate() {
                for sub in &def.items {
                    if let Err(msg) = check_value(sub, row.get(&sub.name)) {
                        return Err(format!("row {}: {}: {msg}", n + 1, sub.label()));
                    }
                }
            }
            Ok(())
        }
    }
}

/// A fresh row for a `struct_list` field with every item at its default.
pub fn default_row(def: &FieldDef) -> ttg_core::Record {
    def.items
        .iter()
        .filter_map(|sub| sub.default_value().map(|v| (sub.name.clone(), v)))
        .collect()
}

/// Same as `check_value` but for a TOML literal (definition defaults).
pub fn check_value_toml(def: &FieldDef, v: &toml::Value) -> Result<(), String> {
    let val = crate::schema::toml_to_value(v).ok_or("unsupported literal type")?;
    check_value(def, Some(&val))
}

fn check_pattern(def: &FieldDef, s: &str) -> Result<(), String> {
    if let Some(p) = &def.pattern {
        let re = regex::Regex::new(p).map_err(|e| e.to_string())?;
        if !re.is_match(s) {
            return Err(def
                .pattern_hint
                .clone()
                .unwrap_or_else(|| format!("must match {p}")));
        }
    }
    Ok(())
}

/// IPv4 CIDR strictly; IPv6 leniently.
pub fn is_cidr(s: &str) -> bool {
    let Some((addr, len)) = s.split_once('/') else {
        return false;
    };
    let Ok(len) = len.parse::<u8>() else {
        return false;
    };
    if addr.contains(':') {
        return len <= 128 && addr.parse::<std::net::Ipv6Addr>().is_ok();
    }
    len <= 32 && addr.parse::<std::net::Ipv4Addr>().is_ok()
}

/// Placeholders (`{name}`, `{provider.size}`) in a template string.
pub fn template_placeholders(t: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = t;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) => {
                out.push(after[..end].to_string());
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr() {
        assert!(is_cidr("10.0.0.0/16"));
        assert!(!is_cidr("10.0.0.0"));
        assert!(!is_cidr("10.0.0.0/33"));
        assert!(!is_cidr("300.0.0.0/8"));
        assert!(is_cidr("fd00::/8"));
    }

    #[test]
    fn placeholders() {
        assert_eq!(
            template_placeholders("{name}-{provider.size}"),
            vec!["name", "provider.size"]
        );
    }
}
