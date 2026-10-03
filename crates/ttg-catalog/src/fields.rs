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
        FieldType::Int => coerced(def, v)
            .as_ref()
            .unwrap_or(v)
            .as_int()
            .map(|_| ())
            .ok_or_else(|| "expected a whole number".into()),
        FieldType::Number => number(v).map(|_| ()).ok_or_else(|| "expected a number".into()),
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
        FieldType::StringList => {
            let items: Vec<&str> = match v {
                Value::List(items) => items.iter().map(|s| s.as_str()).collect(),
                Value::Str(s) => vec![s.as_str()],
                _ => return Err("expected a list of strings".into()),
            };
            // `options` restricts a string_list's allowed values the same way it does an
            // enum's, e.g. Object Storage's `cors_methods` (empty `options` means "any
            // string", the historical behaviour every other string_list keeps).
            if !def.options.is_empty() {
                if let Some(bad) = items.iter().find(|s| !def.options.iter().any(|o| o == *s)) {
                    return Err(format!("\"{bad}\" is not one of: {}", def.options.join(", ")));
                }
            }
            Ok(())
        }
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

/// The numeric value of a field value: a number, or text that reads as one (a value saved
/// while the field was an `enum` of numbers, such as a database's `"32"` GB).
pub fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) if f.is_finite() => Some(*f),
        Value::Str(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
        _ => None,
    }
}

/// The canonical form of a value for its field's type, when it differs from the value
/// itself: `"32"` for an `int` field becomes `32` (a field that used to be an enum of
/// numbers), `2.0` becomes `2`, `0.5` stays `0.5` on a `number` field and `"0.5"` becomes
/// it, and `30` on an `enum` field whose options are numbers becomes `"30"`. `None` when
/// the value is already canonical or cannot be converted (the check then reports it).
/// Rows of a `struct_list` are converted item by item.
pub fn coerced(def: &FieldDef, v: &Value) -> Option<Value> {
    let whole = |f: f64| (f.fract() == 0.0 && f.abs() < 9.0e15).then_some(f as i64);
    match (def.field_type, v) {
        (FieldType::Int, Value::Str(_) | Value::Float(_)) => number(v).and_then(whole).map(Value::Int),
        (FieldType::Number, Value::Float(f)) => whole(*f).map(Value::Int),
        (FieldType::Number, Value::Str(_)) => {
            number(v).map(|f| whole(f).map(Value::Int).unwrap_or(Value::Float(f)))
        }
        (FieldType::Enum, Value::Int(i)) => Some(Value::Str(i.to_string())),
        (FieldType::String, Value::Int(i)) => Some(Value::Str(i.to_string())),
        (FieldType::String, Value::Float(f)) => Some(Value::Str(f.to_string())),
        (FieldType::StructList, Value::Records(rows)) => {
            let mut changed = false;
            let rows: Vec<ttg_core::Record> = rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|(k, x)| {
                            let c = def
                                .items
                                .iter()
                                .find(|sub| &sub.name == k)
                                .and_then(|sub| coerced(sub, x));
                            changed |= c.is_some();
                            (k.clone(), c.unwrap_or_else(|| x.clone()))
                        })
                        .collect()
                })
                .collect();
            changed.then_some(Value::Records(rows))
        }
        _ => None,
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
