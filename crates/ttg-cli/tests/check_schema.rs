//! `ttg check --schema`: a hand-written file is checked against the JSON Schema first,
//! every problem reported with its line, then the catalog diagnostics with theirs.

use std::path::PathBuf;
use std::process::Command;

fn ttg(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_ttg"))
        .args(args)
        .output()
        .expect("run ttg");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

fn example(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name)
        .display()
        .to_string()
}

#[test]
fn the_minimal_example_is_clean() {
    let (code, out) = ttg(&["check", &example("minimal.ttg.json"), "--schema"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("schema: 0 problem(s), 0 error(s)"), "{out}");
    assert!(out.contains("0 diagnostics, 0 error(s)"), "{out}");
}

#[test]
fn schema_problems_and_diagnostics_carry_lines() {
    let dir = std::env::temp_dir().join(format!("ttg-check-schema-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // A schema error: every problem at once, with its line, and a failing exit.
    let bad = dir.join("bad.ttg.json");
    std::fs::write(
        &bad,
        "{\n  \"schema_version\": 1,\n  \"name\": \"bad\",\n  \"nodes\": {\n    \"fn\": {\n      \"id\": \"fn\",\n      \"name\": \"fn\",\n      \"resource_type\": \"function\",\n      \"position\": { \"x\": \"left\", \"y\": 0 },\n      \"colour\": \"red\"\n    }\n  },\n  \"edges\": [ { \"source\": \"fn\", \"target\": \"fn\" } ]\n}\n",
    )
    .unwrap();
    let (code, out) = ttg(&["check", bad.to_str().unwrap(), "--schema"]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("bad.ttg.json:9: error: /nodes/fn/position/x"),
        "{out}"
    );
    assert!(
        out.contains("bad.ttg.json:10: warning: /nodes/fn/colour"),
        "{out}"
    );
    assert!(
        out.contains("bad.ttg.json:13: error: /edges/0: missing required field `relation`"),
        "{out}"
    );

    // Schema-clean but incomplete: the diagnostics name the entity's line.
    let thin = dir.join("thin.ttg.json");
    std::fs::write(
        &thin,
        "{\n  \"schema_version\": 1,\n  \"name\": \"thin\",\n  \"nodes\": {\n    \"fn\": {\n      \"id\": \"fn\",\n      \"name\": \"fn\",\n      \"resource_type\": \"function\"\n    }\n  }\n}\n",
    )
    .unwrap();
    let (code, out) = ttg(&["check", thin.to_str().unwrap(), "--schema"]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("thin.ttg.json:5: error [fn] Handler: required"),
        "{out}"
    );

    // Not JSON at all: the line serde stopped on.
    let junk = dir.join("junk.ttg.json");
    std::fs::write(&junk, "{\n  \"name\": \"x\",\n  oops\n}\n").unwrap();
    let (code, out) = ttg(&["check", junk.to_str().unwrap(), "--schema"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("junk.ttg.json:3: error: not valid JSON"), "{out}");

    let _ = std::fs::remove_dir_all(&dir);
}
