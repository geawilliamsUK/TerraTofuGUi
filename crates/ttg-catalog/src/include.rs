//! Shared fragments of resource definitions (`definitions/fragments/<file>.toml`).
//!
//! A fragment file is a table of named TOML values. A resource definition takes one
//! with a table whose `include` key names it, `"<file>.<name>"`: the table is replaced
//! by the fragment, and its other keys are laid over the fragment (tables merge, other
//! values replace). An array fragment included as an element of an array is spliced
//! in. `with = { noun = "job" }` fills the fragment's `{{noun}}` placeholders, and the
//! fragments it includes in turn see the same values. See MAPPING_FORMAT.md §1.3.

use std::collections::BTreeMap;
use toml::{Table, Value};

/// Fragment files by stem (`container` for `fragments/container.toml`).
pub type Fragments = BTreeMap<String, Table>;

/// Parse `(path, text)` fragment files into [`Fragments`].
pub fn parse<'a>(files: impl Iterator<Item = (&'a str, &'a str)>) -> Result<Fragments, (String, String)> {
    let mut out = Fragments::new();
    for (path, text) in files {
        let stem = path
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(path)
            .trim_end_matches(".toml")
            .to_string();
        let table: Table = toml::from_str(text).map_err(|e| (path.to_string(), e.to_string()))?;
        if out.insert(stem.clone(), table).is_some() {
            return Err((path.to_string(), format!("duplicate fragment file '{stem}'")));
        }
    }
    Ok(out)
}

/// Does the document take any fragment? (A file without one is read straight from its
/// text, so its errors keep their line numbers.)
pub fn uses_fragments(doc: &Table) -> bool {
    fn uses(v: &Value) -> bool {
        match v {
            Value::Table(t) => t.get("include").is_some_and(Value::is_str) || t.values().any(uses),
            Value::Array(a) => a.iter().any(uses),
            _ => false,
        }
    }
    doc.values().any(uses)
}

/// Replace every `include` in `doc` by its fragment.
pub fn expand(doc: &mut Table, frags: &Fragments) -> Result<(), String> {
    let mut stack = Vec::new();
    for (_, v) in doc.iter_mut() {
        expand_value(v, frags, &BTreeMap::new(), &mut stack)?;
    }
    let mut left = Vec::new();
    for v in doc.values() {
        placeholders(v, &mut left);
    }
    match left.first() {
        Some(p) => Err(format!(
            "{{{{{p}}}}} has no value: the include that brings it in needs `with = {{ {p} = \"…\" }}`"
        )),
        None => Ok(()),
    }
}

type Params = BTreeMap<String, String>;

fn expand_value(
    v: &mut Value,
    frags: &Fragments,
    params: &Params,
    stack: &mut Vec<String>,
) -> Result<(), String> {
    match v {
        Value::Table(t) if t.get("include").is_some_and(Value::is_str) => {
            *v = included(std::mem::take(t), frags, params, stack)?;
        }
        Value::Table(t) => {
            for (_, x) in t.iter_mut() {
                expand_value(x, frags, params, stack)?;
            }
        }
        Value::Array(a) => {
            let mut out = Vec::with_capacity(a.len());
            for mut x in std::mem::take(a) {
                let splice = matches!(&x, Value::Table(t) if t.get("include").is_some_and(Value::is_str));
                expand_value(&mut x, frags, params, stack)?;
                match x {
                    Value::Array(items) if splice => out.extend(items),
                    x => out.push(x),
                }
            }
            *a = out;
        }
        _ => {}
    }
    Ok(())
}

/// The value an `include` table stands for.
fn included(
    mut t: Table,
    frags: &Fragments,
    params: &Params,
    stack: &mut Vec<String>,
) -> Result<Value, String> {
    let name = t
        .remove("include")
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    let mut scope = params.clone();
    if let Some(with) = t.remove("with") {
        let Value::Table(with) = with else {
            return Err(format!("include '{name}': `with` must be a table of strings"));
        };
        for (k, v) in with {
            let Value::String(s) = v else {
                return Err(format!("include '{name}': `with.{k}` must be a string"));
            };
            scope.insert(k, fill(&s, params));
        }
    }
    let (file, key) = name
        .split_once('.')
        .ok_or_else(|| format!("include '{name}' must be \"<fragment file>.<name>\""))?;
    let file_frags = frags
        .get(file)
        .ok_or_else(|| format!("include '{name}': there is no fragment file '{file}.toml'"))?;
    let mut frag = file_frags
        .get(key)
        .cloned()
        .ok_or_else(|| format!("include '{name}': fragments/{file}.toml has no '{key}'"))?;
    if stack.contains(&name) {
        return Err(format!(
            "include '{name}' includes itself ({} → {name})",
            stack.join(" → ")
        ));
    }
    stack.push(name.clone());
    expand_value(&mut frag, frags, &scope, stack)?;
    stack.pop();
    substitute(&mut frag, &scope);
    if !t.is_empty() {
        let Value::Table(base) = &mut frag else {
            return Err(format!(
                "include '{name}': only a table fragment takes other keys beside `include`"
            ));
        };
        for (_, x) in t.iter_mut() {
            expand_value(x, frags, params, stack)?;
        }
        merge(base, t);
    }
    Ok(frag)
}

/// Lay `over` on `base`: a table merges into a table of the same name, anything else
/// replaces what was there (keeping its place) or is added at the end.
fn merge(base: &mut Table, over: Table) {
    for (k, v) in over {
        match (base.get_mut(&k), v) {
            (Some(Value::Table(b)), Value::Table(o)) => merge(b, o),
            (Some(slot), v) => *slot = v,
            (None, v) => {
                base.insert(k, v);
            }
        }
    }
}

fn substitute(v: &mut Value, params: &Params) {
    match v {
        Value::String(s) => *s = fill(s, params),
        Value::Table(t) => t.iter_mut().for_each(|(_, x)| substitute(x, params)),
        Value::Array(a) => a.iter_mut().for_each(|x| substitute(x, params)),
        _ => {}
    }
}

fn fill(s: &str, params: &Params) -> String {
    let mut out = s.to_string();
    for (k, v) in params {
        out = out.replace(&format!("{{{{{k}}}}}"), v);
    }
    out
}

/// `{{name}}` placeholders still in a value.
fn placeholders(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => {
            let mut rest = s.as_str();
            while let Some(i) = rest.find("{{") {
                rest = &rest[i + 2..];
                if let Some(j) = rest.find("}}") {
                    out.push(rest[..j].to_string());
                    rest = &rest[j + 2..];
                }
            }
        }
        Value::Table(t) => t.values().for_each(|x| placeholders(x, out)),
        Value::Array(a) => a.iter().for_each(|x| placeholders(x, out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frags() -> Fragments {
        parse(
            [(
                "fragments/shared.toml",
                r#"
note = { value = "the {{noun}}" }
pair = { a = 1, b = { c = 2, d = 3 } }
outer = { list = [ { include = "shared.note" } ] }
loop = { include = "shared.loop" }

[[rows]]
name = "x"
[[rows]]
name = "y"
"#,
            )]
            .into_iter(),
        )
        .unwrap()
    }

    fn expanded(doc: &str) -> Result<Table, String> {
        let mut t: Table = toml::from_str(doc).unwrap();
        expand(&mut t, &frags()).map(|()| t)
    }

    #[test]
    fn a_table_is_replaced_and_its_other_keys_laid_over() {
        let t = expanded("v = { include = \"shared.pair\", b = { d = 4, e = 5 }, f = 6 }").unwrap();
        assert_eq!(
            t["v"].to_string(),
            "{ a = 1, b = { c = 2, d = 4, e = 5 }, f = 6 }"
        );
    }

    #[test]
    fn an_array_fragment_is_spliced_into_an_array() {
        let t =
            expanded("[[rows]]\nname = \"w\"\n[[rows]]\ninclude = \"shared.rows\"\n[[rows]]\nname = \"z\"\n")
                .unwrap();
        let names: Vec<&str> = t["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["w", "x", "y", "z"]);
    }

    #[test]
    fn placeholders_take_the_includes_values_down_through_nested_includes() {
        let t = expanded("v = { include = \"shared.outer\", with = { noun = \"job\" } }").unwrap();
        assert_eq!(t["v"]["list"][0]["value"].as_str(), Some("the job"));
        let err = expanded("v = { include = \"shared.note\" }").unwrap_err();
        assert!(err.contains("{{noun}} has no value"), "{err}");
    }

    #[test]
    fn mistakes_are_named() {
        let err = |doc: &str| expanded(doc).unwrap_err();
        assert!(err("v = { include = \"shared.nope\" }").contains("has no 'nope'"));
        assert!(err("v = { include = \"other.note\" }").contains("no fragment file 'other.toml'"));
        assert!(err("v = { include = \"note\" }").contains("<fragment file>.<name>"));
        assert!(err("v = { include = \"shared.loop\" }").contains("includes itself"));
        assert!(err("v = { include = \"shared.rows\", x = 1 }").contains("only a table fragment"));
        assert!(err("v = { include = \"shared.note\", with = { noun = 1 } }").contains("must be a string"));
    }
}
