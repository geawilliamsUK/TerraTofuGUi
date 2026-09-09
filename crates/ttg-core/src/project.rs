//! Loading and saving `.ttg.json` project files.

use crate::{ir::Project, validate, ProjectError, SCHEMA_VERSION};
use std::path::Path;

/// File extension used for project files.
pub const EXTENSION: &str = "ttg.json";

pub fn load_str(json: &str) -> Result<Project, ProjectError> {
    // Peek at the schema version first so we can give a precise error.
    #[derive(serde::Deserialize)]
    struct Head {
        schema_version: u32,
    }
    let head: Head = serde_json::from_str(json)?;
    if head.schema_version > SCHEMA_VERSION {
        return Err(ProjectError::UnsupportedSchema {
            found: head.schema_version,
            supported: SCHEMA_VERSION,
        });
    }
    let mut project: Project = serde_json::from_str(json)?;
    // Forward migrations go here as the schema evolves (1 -> 2 -> ...).
    project.schema_version = SCHEMA_VERSION;
    let report = validate::structural(&project);
    if report.has_errors() {
        return Err(ProjectError::Invalid(report.to_string()));
    }
    Ok(project)
}

pub fn load(path: &Path) -> Result<Project, ProjectError> {
    let text = std::fs::read_to_string(path)?;
    load_str(&text)
}

/// Serialize with stable ordering and a trailing newline (git-friendly).
pub fn to_string(project: &Project) -> Result<String, ProjectError> {
    let mut s = serde_json::to_string_pretty(project)?;
    s.push('\n');
    Ok(s)
}

pub fn save(project: &Project, path: &Path) -> Result<(), ProjectError> {
    let s = to_string(project)?;
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    std::fs::write(path, s)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::*;
    use crate::Value;

    #[test]
    fn roundtrip() {
        let mut p = Project::new("rt");
        p.nodes.insert(
            "n-1".into(),
            Node {
                id: "n-1".into(),
                name: "bucket".into(),
                resource_type: "object_storage".into(),
                config: [("versioning".to_string(), Value::Bool(true))].into(),
                provider_config: Default::default(),
                position: Position { x: 1, y: 2 },
                size: None,
                parent: None,
                manual: false,
                providers: Vec::new(),
                extra: Default::default(),
            },
        );
        let s = to_string(&p).unwrap();
        let back = load_str(&s).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn future_schema_rejected() {
        let s = r#"{"schema_version": 99, "name": "x"}"#;
        assert!(matches!(
            load_str(s),
            Err(ProjectError::UnsupportedSchema { found: 99, .. })
        ));
    }
}
