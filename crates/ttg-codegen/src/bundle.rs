//! Multi-provider bundling: a folder tree (`out/<provider>/…`) plus an optional zip of it.

use crate::{ExportReport, GenError};
use std::io::Write;
use std::path::Path;

/// Write a top-level README into the bundle root summarising each provider directory.
pub fn write_bundle_readme(
    out_root: &Path,
    project_name: &str,
    results: &[(String, Result<ExportReport, GenError>)],
) -> std::io::Result<()> {
    let mut s = format!(
        "# {project_name} — multi-provider export\n\n\
         Each sub-directory is a **complete, independent** project generated from the same diagram. \
         They share no files and are deployed separately.\n\n"
    );
    for (pid, r) in results {
        match r {
            Ok(rep) => {
                s.push_str(&format!(
                    "- `{pid}/` — {} files{}\n",
                    rep.files.len(),
                    if rep.manual_steps.is_empty() {
                        String::new()
                    } else {
                        format!(
                            ", **{} manual step(s)** (see `{pid}/MANUAL_STEPS.md`)",
                            rep.manual_steps.len()
                        )
                    }
                ));
            }
            Err(e) => s.push_str(&format!("- `{pid}/` — NOT generated: {e}\n")),
        }
    }
    std::fs::write(out_root.join("README.md"), s)
}

/// Zip an export directory tree into `zip_path`.
pub fn zip_dir(dir: &Path, zip_path: &Path) -> Result<(), GenError> {
    let file = std::fs::File::create(zip_path)?;
    let mut zw = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&d)?.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if p == zip_path {
                continue;
            }
            let rel = p.strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/");
            if p.is_dir() {
                zw.add_directory(format!("{rel}/"), opts)
                    .map_err(|e| GenError::Zip(e.to_string()))?;
                stack.push(p);
            } else {
                zw.start_file(rel, opts)
                    .map_err(|e| GenError::Zip(e.to_string()))?;
                zw.write_all(&std::fs::read(&p)?)?;
            }
        }
    }
    zw.finish().map_err(|e| GenError::Zip(e.to_string()))?;
    Ok(())
}
