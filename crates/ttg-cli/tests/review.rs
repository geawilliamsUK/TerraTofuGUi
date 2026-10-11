//! `ttg diff-project` and `ttg import-dir`.

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

fn example(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name)
}

#[test]
fn diff_project_prints_markdown_and_json() {
    let dir = std::env::temp_dir().join(format!("ttg-diff-project-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let old = example("minimal.ttg.json");
    let mut p = ttg_core::project::load(&old).unwrap();
    p.nodes.get_mut("worker").unwrap().name = "job worker".into();
    p.nodes.remove("worker-role");
    p.edges.clear();
    let new = dir.join("changed.ttg.json");
    ttg_core::project::save(&p, &new).unwrap();
    let (code, out) = ttg(&["diff-project", old.to_str().unwrap(), new.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("## Architecture changes: minimal.ttg.json \u{2192} changed.ttg.json"),
        "{out}"
    );
    assert!(out.contains("renamed from \"worker\""), "{out}");
    assert!(out.contains("| worker role | IAM Role |"), "{out}");
    let (code, out) = ttg(&[
        "diff-project",
        old.to_str().unwrap(),
        new.to_str().unwrap(),
        "--json",
        "--exit-code",
    ]);
    assert_eq!(code, 1, "{out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["entities"]["removed"][0]["name"], "worker role");
    let (code, _) = ttg(&[
        "diff-project",
        old.to_str().unwrap(),
        old.to_str().unwrap(),
        "--exit-code",
    ]);
    assert_eq!(code, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn import_dir_previews_then_writes() {
    let dir = std::env::temp_dir().join(format!("ttg-import-dir-{}", std::process::id()));
    let out_dir = dir.join("out");
    let project = example("minimal.ttg.json");
    let (code, out) = ttg(&[
        "export",
        project.to_str().unwrap(),
        "--provider",
        "aws",
        "--out",
        out_dir.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{out}");
    let network = out_dir.join("network.tf");
    let text = std::fs::read_to_string(&network).unwrap();
    let edited: String = text
        .lines()
        .map(|l| {
            if l.trim_start().starts_with("enable_dns_support") {
                l.replace("true", "false")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_ne!(edited, text);
    std::fs::write(&network, edited).unwrap();
    let (code, out) = ttg(&["import-dir", project.to_str().unwrap(), out_dir.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("`enable_dns_support` = `false`"), "{out}");
    assert!(out.contains("preview only"), "{out}");
    let written = dir.join("imported.ttg.json");
    let (code, out) = ttg(&[
        "import-dir",
        project.to_str().unwrap(),
        out_dir.to_str().unwrap(),
        "--out",
        written.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{out}");
    let p = ttg_core::project::load(&written).unwrap();
    assert_eq!(
        p.containers["net"].extra["aws"]["main"]["enable_dns_support"],
        false
    );
    let _ = std::fs::remove_dir_all(&dir);
}
