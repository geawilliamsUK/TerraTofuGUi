//! Check a project file's *text* against the project JSON Schema and report every
//! problem with the line it is on, so a hand-written (or agent-written) `.ttg.json` can
//! be fixed in one pass instead of one serde error at a time.
//!
//! The validator covers the part of JSON Schema 2020-12 that the generated schema uses:
//! `type`, `$ref` into `$defs`, `properties`, `required`, `additionalProperties`,
//! `items`, `enum`, `const`, `anyOf` / `oneOf` / `allOf`, `minimum`, `uniqueItems` and the
//! integer `format`s (`uint32`, `int32`, ...). Anything else in a schema is ignored.

use crate::SCHEMA_VERSION;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;

/// One thing wrong with the file.
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    /// 1-based line of the offending value (or its key), or of the nearest enclosing
    /// value when the offending one is missing.
    pub line: usize,
    /// JSON Pointer of the offending value (`/nodes/web/config/size`); empty for the root.
    pub pointer: String,
    pub message: String,
    /// A field the schema does not know. serde ignores it, so the file still loads, but
    /// it is usually a misspelling of one it does know.
    pub warning: bool,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let at = if self.pointer.is_empty() {
            "(root)"
        } else {
            self.pointer.as_str()
        };
        let sev = if self.warning { "warning" } else { "error" };
        write!(f, "line {}: {sev}: {at}: {}", self.line, self.message)
    }
}

/// Every schema violation in `text`, in document order. `Err` when the text is not JSON
/// at all (the one problem then carries serde's line).
pub fn check_project_text(text: &str) -> Result<Vec<Problem>, Problem> {
    let value: Value = serde_json::from_str(text).map_err(|e| Problem {
        line: e.line(),
        pointer: String::new(),
        message: format!("not valid JSON: {e}"),
        warning: false,
    })?;
    let schema: Value = serde_json::from_str(&crate::json_schema::project_schema_json())
        .expect("the generated schema is JSON");
    let lines = line_index(text);
    let mut found = Vec::new();
    Validator { root: &schema }.check(&schema, &value, "", &mut found, true);
    if let Some(v) = value.get("schema_version").and_then(Value::as_u64) {
        if v > u64::from(SCHEMA_VERSION) {
            found.push(Found::new(
                "/schema_version",
                format!("schema_version {v} is newer than this build reads ({SCHEMA_VERSION})"),
            ));
        }
    }
    let mut out: Vec<Problem> = found
        .into_iter()
        .map(|f| Problem {
            line: line_of(&lines, &f.pointer),
            pointer: f.pointer,
            message: f.message,
            warning: f.warning,
        })
        .collect();
    out.sort_by_key(|p| p.line);
    out.dedup();
    Ok(out)
}

/// The line of `pointer`, or of its nearest ancestor that is in the file.
pub fn line_of(lines: &HashMap<String, usize>, pointer: &str) -> usize {
    let mut p = pointer;
    loop {
        if let Some(l) = lines.get(p) {
            return *l;
        }
        match p.rfind('/') {
            Some(i) => p = &p[..i],
            None => return 1,
        }
    }
}

/// Escape one JSON Pointer reference token.
pub fn pointer_token(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// A violation before its line is known.
struct Found {
    pointer: String,
    message: String,
    /// The value had the wrong JSON type for this (sub)schema: in an `anyOf`, a branch
    /// that fails this way is a worse explanation than one that got the type right.
    type_mismatch: bool,
    warning: bool,
}

impl Found {
    fn new(pointer: &str, message: impl Into<String>) -> Self {
        Found {
            pointer: pointer.to_string(),
            message: message.into(),
            type_mismatch: false,
            warning: false,
        }
    }
}

struct Validator<'a> {
    root: &'a Value,
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn type_matches(want: &str, v: &Value) -> bool {
    match want {
        "null" => v.is_null(),
        "boolean" => v.is_boolean(),
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "number" => v.is_number(),
        "integer" => match v {
            Value::Number(n) => n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0),
            _ => false,
        },
        _ => true,
    }
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 40 {
        format!("{}…", s.chars().take(40).collect::<String>())
    } else {
        s
    }
}

impl Validator<'_> {
    fn resolve(&self, r: &str) -> Option<&Value> {
        let path = r.strip_prefix('#')?;
        self.root.pointer(path)
    }

    fn passes(&self, schema: &Value, v: &Value, ptr: &str) -> Vec<Found> {
        let mut out = Vec::new();
        self.check(schema, v, ptr, &mut out, false);
        out
    }

    /// Append what is wrong with `v` under `schema`. `warn` adds warnings for fields the
    /// schema does not list; it is off inside `anyOf` / `oneOf`, where a branch is only
    /// asked whether it matches.
    fn check(&self, schema: &Value, v: &Value, ptr: &str, out: &mut Vec<Found>, warn: bool) {
        let s = match schema {
            Value::Bool(true) => return,
            Value::Bool(false) => {
                out.push(Found::new(ptr, "not allowed here"));
                return;
            }
            Value::Object(s) => s,
            _ => return,
        };
        if let Some(r) = s.get("$ref").and_then(Value::as_str) {
            match self.resolve(r) {
                Some(target) => self.check(target, v, ptr, out, warn),
                None => out.push(Found::new(ptr, format!("schema reference {r} not found"))),
            }
        }
        if let Some(t) = s.get("type") {
            let wants: Vec<&str> = match t {
                Value::String(one) => vec![one.as_str()],
                Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
                _ => Vec::new(),
            };
            if !wants.is_empty() && !wants.iter().any(|w| type_matches(w, v)) {
                out.push(Found {
                    pointer: ptr.to_string(),
                    message: format!("expected {}, found {} {}", wants.join(" or "), kind(v), short(v)),
                    type_mismatch: true,
                    warning: false,
                });
                return;
            }
        }
        if let Some(Value::Array(options)) = s.get("enum") {
            if !options.contains(v) {
                let list: Vec<String> = options.iter().map(|o| o.to_string()).collect();
                out.push(Found::new(
                    ptr,
                    format!("{} is not one of {}", short(v), list.join(", ")),
                ));
            }
        }
        if let Some(c) = s.get("const") {
            if c != v {
                out.push(Found::new(ptr, format!("must be {c}, found {}", short(v))));
            }
        }
        if let Value::Number(n) = v {
            self.check_number(s, n, ptr, out);
        }
        match v {
            Value::Object(o) => self.check_object(s, o, ptr, out, warn),
            Value::Array(items) => self.check_array(s, items, ptr, out, warn),
            _ => {}
        }
        if let Some(Value::Array(all)) = s.get("allOf") {
            for sub in all {
                self.check(sub, v, ptr, out, warn);
            }
        }
        for (key, exactly_one) in [("anyOf", false), ("oneOf", true)] {
            let Some(Value::Array(branches)) = s.get(key) else {
                continue;
            };
            let results: Vec<Vec<Found>> = branches.iter().map(|b| self.passes(b, v, ptr)).collect();
            let passing = results.iter().filter(|r| r.is_empty()).count();
            if passing == 0 {
                // Explain with the branch that came closest: one that got the type right,
                // then the one with the fewest complaints.
                let best = results
                    .into_iter()
                    .min_by_key(|r| {
                        let mismatch = r.iter().any(|f| f.type_mismatch && f.pointer == ptr);
                        (mismatch, r.len())
                    })
                    .unwrap_or_default();
                if best.len() == 1 && best[0].type_mismatch && best[0].pointer == ptr {
                    out.push(Found::new(
                        ptr,
                        format!("{} {} matches none of the allowed shapes", kind(v), short(v)),
                    ));
                } else {
                    out.extend(best);
                }
            } else if exactly_one && passing > 1 {
                out.push(Found::new(ptr, "matches more than one of the allowed shapes"));
            }
        }
    }

    fn check_number(
        &self,
        s: &serde_json::Map<String, Value>,
        n: &serde_json::Number,
        ptr: &str,
        out: &mut Vec<Found>,
    ) {
        let f = n.as_f64().unwrap_or(0.0);
        if let Some(min) = s.get("minimum").and_then(Value::as_f64) {
            if f < min {
                out.push(Found::new(ptr, format!("{n} is below the minimum {min}")));
            }
        }
        let range: Option<(f64, f64)> = match s.get("format").and_then(Value::as_str) {
            Some("uint8") => Some((0.0, u8::MAX as f64)),
            Some("uint16") => Some((0.0, u16::MAX as f64)),
            Some("uint32") => Some((0.0, u32::MAX as f64)),
            Some("uint64") | Some("uint") => Some((0.0, u64::MAX as f64)),
            Some("int8") => Some((i8::MIN as f64, i8::MAX as f64)),
            Some("int16") => Some((i16::MIN as f64, i16::MAX as f64)),
            Some("int32") => Some((i32::MIN as f64, i32::MAX as f64)),
            _ => None,
        };
        if let Some((lo, hi)) = range {
            if f < lo || f > hi {
                out.push(Found::new(ptr, format!("{n} is out of range ({lo} to {hi})")));
            }
        }
    }

    fn check_object(
        &self,
        s: &serde_json::Map<String, Value>,
        o: &serde_json::Map<String, Value>,
        ptr: &str,
        out: &mut Vec<Found>,
        warn: bool,
    ) {
        let props = s.get("properties").and_then(Value::as_object);
        if let Some(Value::Array(req)) = s.get("required") {
            for r in req.iter().filter_map(Value::as_str) {
                if !o.contains_key(r) {
                    out.push(Found::new(ptr, format!("missing required field `{r}`")));
                }
            }
        }
        for (k, val) in o {
            let child = format!("{ptr}/{}", pointer_token(k));
            match props.and_then(|p| p.get(k)) {
                Some(sub) => self.check(sub, val, &child, out, warn),
                None => match s.get("additionalProperties") {
                    Some(Value::Bool(false)) => {
                        let known: Vec<String> = props
                            .map(|p| p.keys().map(|k| format!("`{k}`")).collect())
                            .unwrap_or_default();
                        out.push(Found::new(
                            &child,
                            if known.is_empty() {
                                format!("unknown field `{k}`")
                            } else {
                                format!("unknown field `{k}`; expected one of {}", known.join(", "))
                            },
                        ));
                    }
                    Some(sub @ Value::Object(_)) => self.check(sub, val, &child, out, warn),
                    Some(_) => {}
                    // A file may name its schema for editors; the app ignores it.
                    None if ptr.is_empty() && k == "$schema" => {}
                    None if warn && props.is_some() => {
                        let known: Vec<String> = props
                            .map(|p| p.keys().map(|k| format!("`{k}`")).collect())
                            .unwrap_or_default();
                        out.push(Found {
                            warning: true,
                            ..Found::new(
                                &child,
                                format!(
                                    "unknown field `{k}` is ignored; known fields: {}",
                                    known.join(", ")
                                ),
                            )
                        });
                    }
                    None => {}
                },
            }
        }
    }

    fn check_array(
        &self,
        s: &serde_json::Map<String, Value>,
        items: &[Value],
        ptr: &str,
        out: &mut Vec<Found>,
        warn: bool,
    ) {
        if let Some(sub) = s.get("items") {
            for (i, it) in items.iter().enumerate() {
                self.check(sub, it, &format!("{ptr}/{i}"), out, warn);
            }
        }
        if s.get("uniqueItems").and_then(Value::as_bool) == Some(true) {
            for (i, it) in items.iter().enumerate() {
                if items[..i].contains(it) {
                    out.push(Found::new(
                        &format!("{ptr}/{i}"),
                        format!("{} appears twice", short(it)),
                    ));
                }
            }
        }
    }
}

/// The 1-based line each value of a JSON document starts on, by JSON Pointer. For an
/// object member that is the line of its key. The text must already parse as JSON;
/// anything after a syntax error is simply not indexed.
pub fn line_index(text: &str) -> HashMap<String, usize> {
    let mut idx = Indexer {
        b: text.as_bytes(),
        i: 0,
        line: 1,
        out: HashMap::new(),
    };
    idx.ws();
    let line = idx.line;
    idx.value(String::new(), line);
    idx.out
}

struct Indexer<'a> {
    b: &'a [u8],
    i: usize,
    line: usize,
    out: HashMap<String, usize>,
}

impl Indexer<'_> {
    fn ws(&mut self) {
        while let Some(&c) = self.b.get(self.i) {
            match c {
                b'\n' => {
                    self.line += 1;
                    self.i += 1;
                }
                b' ' | b'\t' | b'\r' => self.i += 1,
                _ => break,
            }
        }
    }

    /// A string starting at the opening quote; returns its decoded text.
    fn string(&mut self) -> String {
        let start = self.i;
        self.i += 1;
        while let Some(&c) = self.b.get(self.i) {
            match c {
                b'\\' => self.i += 2,
                b'"' => {
                    self.i += 1;
                    break;
                }
                _ => self.i += 1,
            }
        }
        let raw = &self.b[start..self.i.min(self.b.len())];
        std::str::from_utf8(raw)
            .ok()
            .and_then(|s| serde_json::from_str::<String>(s).ok())
            .unwrap_or_default()
    }

    fn value(&mut self, ptr: String, line: usize) {
        self.out.insert(ptr.clone(), line);
        match self.b.get(self.i) {
            Some(b'{') => {
                self.i += 1;
                loop {
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b'}') => {
                            self.i += 1;
                            return;
                        }
                        Some(b',') => {
                            self.i += 1;
                            continue;
                        }
                        Some(b'"') => {
                            let key_line = self.line;
                            let key = self.string();
                            self.ws();
                            if self.b.get(self.i) == Some(&b':') {
                                self.i += 1;
                            }
                            self.ws();
                            self.value(format!("{ptr}/{}", pointer_token(&key)), key_line);
                        }
                        _ => return,
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut n = 0usize;
                loop {
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b']') => {
                            self.i += 1;
                            return;
                        }
                        Some(b',') => {
                            self.i += 1;
                            continue;
                        }
                        Some(_) => {
                            let l = self.line;
                            self.value(format!("{ptr}/{n}"), l);
                            n += 1;
                        }
                        None => return,
                    }
                }
            }
            Some(b'"') => {
                self.string();
            }
            Some(_) => {
                while let Some(&c) = self.b.get(self.i) {
                    if matches!(c, b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n') {
                        break;
                    }
                    self.i += 1;
                }
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"{
  "schema_version": 1,
  "name": "minimal",
  "containers": {
    "net": {
      "id": "net", "name": "net", "container_type": "virtual_network",
      "config": { "cidr_block": "10.0.0.0/16" },
      "position": { "x": 40, "y": 40 }, "size": { "w": 600, "h": 400 }
    }
  },
  "nodes": {
    "app": { "id": "app", "name": "app", "resource_type": "subnet", "parent": "net",
             "config": { "cidr_block": "10.0.1.0/24" }, "position": { "x": 80, "y": 100 } }
  }
}"#;

    #[test]
    fn indexes_lines_by_pointer() {
        let text = "{\n  \"a\": {\n    \"b/c\": [\n      1,\n      {\"d\": 2}\n    ]\n  }\n}";
        let idx = line_index(text);
        assert_eq!(idx[""], 1);
        assert_eq!(idx["/a"], 2);
        assert_eq!(idx["/a/b~1c"], 3);
        assert_eq!(idx["/a/b~1c/0"], 4);
        assert_eq!(idx["/a/b~1c/1/d"], 5);
        assert_eq!(line_of(&idx, "/a/b~1c/1/missing"), 5);
    }

    #[test]
    fn every_example_passes_the_schema() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.to_string_lossy().ends_with(".ttg.json") {
                let text = std::fs::read_to_string(&p).unwrap();
                let problems = check_project_text(&text).unwrap();
                assert!(problems.is_empty(), "{}: {problems:#?}", p.display());
            }
        }
    }

    #[test]
    fn reports_each_problem_with_its_line() {
        let text = r#"{
  "schema_version": 1,
  "name": 7,
  "containers": {},
  "nodes": {
    "web": {
      "id": "web",
      "name": "web",
      "resource_type": "function",
      "position": { "x": "left", "y": 0 },
      "colour": "red"
    }
  },
  "edges": [
    { "source": "web", "target": "web", "relation": "sends_to_the_moon" }
  ]
}"#;
        let problems = check_project_text(text).unwrap();
        let at = |ptr: &str| problems.iter().find(|p| p.pointer == ptr).cloned();
        assert_eq!(at("/name").map(|p| p.line), Some(3), "{problems:#?}");
        assert!(at("/name").unwrap().message.contains("expected string"));
        assert_eq!(
            at("/nodes/web/position/x").map(|p| p.line),
            Some(10),
            "{problems:#?}"
        );
        let unknown = at("/nodes/web/colour").expect("unknown field reported");
        assert_eq!(unknown.line, 11);
        assert!(unknown.warning, "{unknown}");
        assert!(!at("/name").unwrap().warning);
        assert!(unknown.message.contains("unknown field `colour`"), "{unknown}");
        let rel = problems
            .iter()
            .find(|p| p.pointer.starts_with("/edges/0"))
            .expect("relation reported");
        assert_eq!(rel.line, 15, "{rel}");
    }

    #[test]
    fn a_minimal_hand_written_file_is_clean_and_loads() {
        let problems = check_project_text(MINIMAL).unwrap();
        assert!(problems.is_empty(), "{problems:#?}");
        crate::project::load_str(MINIMAL).unwrap();
    }

    #[test]
    fn syntax_errors_and_newer_versions_are_reported() {
        let err = check_project_text("{\n  \"name\": \"x\",\n  oops\n}").unwrap_err();
        assert_eq!(err.line, 3);
        let p = check_project_text("{\"schema_version\": 99, \"name\": \"x\"}").unwrap();
        assert!(p.iter().any(|p| p.pointer == "/schema_version"), "{p:#?}");
    }
}
