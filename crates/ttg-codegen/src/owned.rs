//! The files an export owns inside its directory, and nothing else.
//!
//! An export writes the configuration at the top level and, beside it, directories of
//! its own: `k8s/` (the Kubernetes manifests, `crate::k8s`) and `bootstrap/` (the root
//! that creates the state store and key, `crate::state`). Re-exporting must replace what
//! it wrote and remove what it would no longer write — a manifest for a workload that is
//! gone, a bootstrap root after the backend is cleared — but never touch what the user's
//! tools put there: variable values, state files, `.terraform/` and its lock file, or
//! the render scripts' `k8s/rendered/`. One table says, per directory, which files the
//! export owns and whether it looks below the top of it; `export` (which removes stale
//! files) and `diff::against_dir` (which lists them) both read it.

use crate::emit::Generated;
use std::path::Path;

/// One directory the export writes into.
struct OwnedDir {
    /// Path inside the export directory; `""` is the export directory itself.
    dir: &'static str,
    /// Look into subdirectories too (never into hidden ones such as `.terraform/`).
    deep: bool,
    /// Whether a file of this name is one the export writes.
    owns: fn(&str) -> bool,
}

/// `.tf` files and the two markdown companions: a Terraform / OpenTofu root.
fn root_file(name: &str) -> bool {
    name.ends_with(".tf") || name == "MANUAL_STEPS.md" || name == "README.md"
}

/// The manifests, their render scripts and README. `k8s/rendered/` is a subdirectory and
/// this entry is shallow, so the scripts' output is never visited.
fn manifest_file(name: &str) -> bool {
    name.ends_with(".yaml") || name.ends_with(".sh") || name.ends_with(".ps1") || name == "README.md"
}

/// Each environment's values, backend configuration and (when they differ) manual steps.
/// A `*.auto.tfvars` file is someone else's: the tools load it on their own, and the
/// export never writes one.
fn environment_file(name: &str) -> bool {
    name.ends_with(".backend.hcl")
        || name.ends_with(".MANUAL_STEPS.md")
        || (name.ends_with(".tfvars") && !name.ends_with(".auto.tfvars"))
}

const OWNED: &[OwnedDir] = &[
    OwnedDir {
        dir: crate::environments::DIR,
        deep: false,
        owns: environment_file,
    },
    OwnedDir {
        dir: "",
        deep: false,
        owns: root_file,
    },
    OwnedDir {
        dir: crate::k8s::DIR,
        deep: false,
        owns: manifest_file,
    },
    // `bootstrap/` and a second root below it (`bootstrap/<provider>/`); their state
    // files and `.terraform/` are not `.tf` files and are left where they are.
    OwnedDir {
        dir: crate::state::BOOTSTRAP_DIR,
        deep: true,
        owns: root_file,
    },
];

fn join(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_string()
    } else {
        format!("{rel}/{name}")
    }
}

fn walk(base: &Path, rel: &str, deep: bool, owns: fn(&str) -> bool, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(base.join(rel)) else {
        return;
    };
    for e in rd.flatten() {
        let Some(name) = e.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let path = e.path();
        if path.is_dir() {
            if deep && !name.starts_with('.') {
                walk(base, &join(rel, &name), deep, owns, out);
            }
        } else if owns(&name) {
            out.push(join(rel, &name));
        }
    }
}

/// The owned files already in an export directory, as `/`-separated paths relative to
/// it (the keys `Generated::files` uses), sorted.
pub fn on_disk(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for d in OWNED {
        walk(dir, d.dir, d.deep, d.owns, &mut out);
    }
    out.sort();
    out
}

/// The owned files in `dir` that `g` would no longer write.
pub fn stale(g: &Generated, dir: &Path) -> Vec<String> {
    on_disk(dir)
        .into_iter()
        .filter(|n| !g.files.contains_key(n))
        .collect()
}

/// Remove the stale files, then every owned directory (or directory below a deep one)
/// that is left empty — `k8s/` when the manifests are switched off, `bootstrap/` when
/// the backend is cleared. A directory still holding anything, state included, stays.
pub fn remove_stale(g: &Generated, dir: &Path) {
    for name in stale(g, dir) {
        let _ = std::fs::remove_file(dir.join(name));
    }
    for d in OWNED.iter().filter(|d| !d.dir.is_empty()) {
        let mut dirs = vec![d.dir.to_string()];
        if d.deep {
            let mut i = 0;
            while i < dirs.len() {
                if let Ok(rd) = std::fs::read_dir(dir.join(&dirs[i])) {
                    for e in rd.flatten() {
                        let name = e.file_name().to_string_lossy().to_string();
                        if e.path().is_dir() && !name.starts_with('.') {
                            dirs.push(join(&dirs[i], &name));
                        }
                    }
                }
                i += 1;
            }
        }
        // Deepest first; `remove_dir` only succeeds on an empty directory.
        for d in dirs.iter().rev() {
            let _ = std::fs::remove_dir(dir.join(d));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn generated(files: &[&str]) -> Generated {
        Generated {
            provider: "aws".into(),
            provider_display: "AWS".into(),
            tool: ttg_core::Tool::OpenTofu,
            files: files
                .iter()
                .map(|f| (f.to_string(), String::new()))
                .collect::<IndexMap<_, _>>(),
            manual_steps: Vec::new(),
            diagnostics: Vec::new(),
            entity_blocks: Default::default(),
            lifted: Vec::new(),
        }
    }

    #[test]
    fn only_owned_files_are_stale_and_only_empty_directories_go() {
        let dir = std::env::temp_dir().join(format!("ttg-owned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for f in [
            "main.tf",
            "old.tf",
            "terraform.tfvars",
            "MANUAL_STEPS.md",
            "k8s/api.yaml",
            "k8s/render.sh",
            "k8s/rendered/api.yaml",
            "bootstrap/storage.tf",
            "bootstrap/terraform.tfstate",
            "bootstrap/.terraform/providers.tf",
            "bootstrap/gcp/security.tf",
        ] {
            let p = dir.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        }
        let g = generated(&["main.tf", "k8s/render.sh"]);
        assert_eq!(
            stale(&g, &dir),
            vec![
                "MANUAL_STEPS.md",
                "bootstrap/gcp/security.tf",
                "bootstrap/storage.tf",
                "k8s/api.yaml",
                "old.tf"
            ]
        );
        remove_stale(&g, &dir);
        // The user's and the tools' files stay, and so do the directories holding them.
        for kept in [
            "terraform.tfvars",
            "k8s/rendered/api.yaml",
            "bootstrap/terraform.tfstate",
            "bootstrap/.terraform/providers.tf",
        ] {
            assert!(dir.join(kept).is_file(), "{kept} was removed");
        }
        // An emptied second root goes.
        assert!(!dir.join("bootstrap/gcp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
