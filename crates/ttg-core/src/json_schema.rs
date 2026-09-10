//! JSON Schema for the `.ttg.json` project file, generated from the IR types so external
//! tools (editors, CI checks) can validate project files. `ttg schema project` prints it;
//! the checked-in copy lives in `schemas/project.schema.json`.

use crate::Project;

/// The schema as pretty-printed JSON (draft 2020-12, as produced by `schemars`).
pub fn project_schema_json() -> String {
    let mut schema = schemars::schema_for!(Project);
    if let Some(obj) = schema.as_object_mut() {
        obj.insert("title".into(), "TerraTofu GUI project (.ttg.json)".into());
        obj.insert(
            "description".into(),
            "A cloud-architecture diagram: settings, containers, nodes, edges and views. \
             Written by TerraTofu GUI; readable by `ttg` and any MCP client."
                .into(),
        );
    }
    serde_json::to_string_pretty(&schema).expect("schema serialises")
}

#[cfg(test)]
mod tests {
    #[test]
    fn checked_in_schema_is_current() {
        let generated = super::project_schema_json();
        let checked_in = include_str!("../../../schemas/project.schema.json");
        assert_eq!(
            generated.trim(),
            checked_in.trim().replace("\r\n", "\n"),
            "schemas/project.schema.json is stale: run `cargo run -p ttg-cli -- schema project --out schemas/project.schema.json`"
        );
    }

    #[test]
    fn examples_carry_the_schema_shape() {
        // Every example round-trips through the IR (the schema mirrors the same types).
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.to_string_lossy().ends_with(".ttg.json") {
                crate::project::load(&p).unwrap_or_else(|err| panic!("{}: {err}", p.display()));
            }
        }
    }
}
