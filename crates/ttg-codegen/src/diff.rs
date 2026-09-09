//! Line-level diff between a freshly generated project and what an earlier export left
//! on disk, so the user can see what a re-export would change before writing anything.

use crate::emit::Generated;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Removed,
    Changed,
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
    /// A run of unchanged lines folded away in a condensed view; `text` says how many.
    Skip,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: LineKind,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    pub name: String,
    pub status: FileStatus,
    pub added: usize,
    pub removed: usize,
    /// Every line of the merged view (old and new interleaved), in order.
    pub lines: Vec<DiffLine>,
}

impl FileDiff {
    pub fn changed(&self) -> bool {
        self.status != FileStatus::Unchanged
    }

    /// The lines with unchanged runs longer than `2 * context` folded into `Skip` markers.
    pub fn condensed(&self, context: usize) -> Vec<DiffLine> {
        let n = self.lines.len();
        let mut keep = vec![false; n];
        for (i, l) in self.lines.iter().enumerate() {
            if l.kind != LineKind::Context {
                let lo = i.saturating_sub(context);
                let hi = (i + context + 1).min(n);
                for k in keep.iter_mut().take(hi).skip(lo) {
                    *k = true;
                }
            }
        }
        let mut out = Vec::new();
        let mut skipped = 0usize;
        for (i, l) in self.lines.iter().enumerate() {
            if keep[i] {
                if skipped > 0 {
                    out.push(DiffLine {
                        kind: LineKind::Skip,
                        text: format!("… {skipped} unchanged line(s)"),
                    });
                    skipped = 0;
                }
                out.push(l.clone());
            } else {
                skipped += 1;
            }
        }
        if skipped > 0 {
            out.push(DiffLine {
                kind: LineKind::Skip,
                text: format!("… {skipped} unchanged line(s)"),
            });
        }
        out
    }
}

fn split(s: &str) -> Vec<&str> {
    let s = s.strip_suffix('\n').unwrap_or(s);
    if s.is_empty() {
        return Vec::new();
    }
    s.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect()
}

/// Longest-common-subsequence line diff. Common prefix and suffix are peeled off first so
/// the quadratic part only sees the region that actually differs.
pub fn diff_lines(old: &str, new: &str) -> Vec<DiffLine> {
    let a = split(old);
    let b = split(new);
    let mut pre = 0;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf] {
        suf += 1;
    }
    let ma = &a[pre..a.len() - suf];
    let mb = &b[pre..b.len() - suf];
    let mut out: Vec<DiffLine> = a[..pre]
        .iter()
        .map(|l| DiffLine {
            kind: LineKind::Context,
            text: l.to_string(),
        })
        .collect();
    // Middle: LCS table (bounded; huge inputs degrade to "all removed, all added").
    let (n, m) = (ma.len(), mb.len());
    if n * m > 6_000_000 {
        out.extend(ma.iter().map(|l| DiffLine {
            kind: LineKind::Removed,
            text: l.to_string(),
        }));
        out.extend(mb.iter().map(|l| DiffLine {
            kind: LineKind::Added,
            text: l.to_string(),
        }));
    } else {
        let mut t = vec![vec![0u32; m + 1]; n + 1];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                t[i][j] = if ma[i] == mb[j] {
                    t[i + 1][j + 1] + 1
                } else {
                    t[i + 1][j].max(t[i][j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n && j < m {
            if ma[i] == mb[j] {
                out.push(DiffLine {
                    kind: LineKind::Context,
                    text: ma[i].to_string(),
                });
                i += 1;
                j += 1;
            } else if t[i + 1][j] >= t[i][j + 1] {
                out.push(DiffLine {
                    kind: LineKind::Removed,
                    text: ma[i].to_string(),
                });
                i += 1;
            } else {
                out.push(DiffLine {
                    kind: LineKind::Added,
                    text: mb[j].to_string(),
                });
                j += 1;
            }
        }
        out.extend(ma[i..].iter().map(|l| DiffLine {
            kind: LineKind::Removed,
            text: l.to_string(),
        }));
        out.extend(mb[j..].iter().map(|l| DiffLine {
            kind: LineKind::Added,
            text: l.to_string(),
        }));
    }
    out.extend(a[a.len() - suf..].iter().map(|l| DiffLine {
        kind: LineKind::Context,
        text: l.to_string(),
    }));
    out
}

fn file_diff(name: &str, old: Option<&str>, new: Option<&str>) -> FileDiff {
    let lines = diff_lines(old.unwrap_or(""), new.unwrap_or(""));
    let added = lines.iter().filter(|l| l.kind == LineKind::Added).count();
    let removed = lines.iter().filter(|l| l.kind == LineKind::Removed).count();
    let status = match (old, new) {
        (None, Some(_)) => FileStatus::Added,
        (Some(_), None) => FileStatus::Removed,
        _ if added + removed > 0 => FileStatus::Changed,
        _ => FileStatus::Unchanged,
    };
    FileDiff {
        name: name.to_string(),
        status,
        added,
        removed,
        lines,
    }
}

/// Files an export manages inside its directory: generated `.tf` files plus the two
/// markdown companions. Anything else in the directory is the user's and never diffed.
fn managed(name: &str) -> bool {
    name.ends_with(".tf") || name == "MANUAL_STEPS.md" || name == "README.md"
}

/// Compare a generated project with the contents of `dir` (what the last export wrote,
/// possibly hand-edited since). Files are listed in generation order, then files that
/// exist on disk but would no longer be generated. A missing directory diffs as
/// "everything added".
pub fn against_dir(g: &Generated, dir: &Path) -> Vec<FileDiff> {
    let mut out = Vec::new();
    for (name, content) in &g.files {
        let old = std::fs::read_to_string(dir.join(name)).ok();
        out.push(file_diff(name, old.as_deref(), Some(content)));
    }
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut stale: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .filter(|n| managed(n) && !g.files.contains_key(n))
            .collect();
        stale.sort();
        for name in stale {
            let old = std::fs::read_to_string(dir.join(&name)).ok();
            out.push(file_diff(&name, old.as_deref(), None));
        }
    }
    out
}

/// One-line summary: "2 changed, 1 added, 1 removed (+31 −7)" or "no changes".
pub fn summary(diffs: &[FileDiff]) -> String {
    let count = |s: FileStatus| diffs.iter().filter(|d| d.status == s).count();
    let (c, a, r) = (
        count(FileStatus::Changed),
        count(FileStatus::Added),
        count(FileStatus::Removed),
    );
    if c + a + r == 0 {
        return "no changes".into();
    }
    let plus: usize = diffs.iter().map(|d| d.added).sum();
    let minus: usize = diffs.iter().map(|d| d.removed).sum();
    let mut parts = Vec::new();
    if c > 0 {
        parts.push(format!("{c} changed"));
    }
    if a > 0 {
        parts.push(format!("{a} added"));
    }
    if r > 0 {
        parts.push(format!("{r} removed"));
    }
    format!("{} (+{plus} \u{2212}{minus})", parts.join(", "))
}

/// Render one file's diff as unified-style text (`+` / `-` / ` ` prefixes).
pub fn render(d: &FileDiff, context: usize) -> String {
    let mut s = String::new();
    for l in d.condensed(context) {
        let p = match l.kind {
            LineKind::Context => ' ',
            LineKind::Added => '+',
            LineKind::Removed => '-',
            LineKind::Skip => '@',
        };
        s.push(p);
        s.push(' ');
        s.push_str(&l.text);
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lcs_diff_marks_changes() {
        let old = "a\nb\nc\nd\n";
        let new = "a\nB\nc\nd\ne\n";
        let d = diff_lines(old, new);
        let kinds: Vec<LineKind> = d.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            [
                LineKind::Context,
                LineKind::Removed,
                LineKind::Added,
                LineKind::Context,
                LineKind::Context,
                LineKind::Added
            ]
        );
    }

    #[test]
    fn condensed_folds_context() {
        let old: String = (0..40).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l20\n", "L20\n");
        let f = file_diff("x.tf", Some(&old), Some(&new));
        assert_eq!(f.status, FileStatus::Changed);
        assert_eq!((f.added, f.removed), (1, 1));
        let c = f.condensed(2);
        assert_eq!(c.first().unwrap().kind, LineKind::Skip);
        assert_eq!(c.last().unwrap().kind, LineKind::Skip);
        assert_eq!(c.iter().filter(|l| l.kind == LineKind::Context).count(), 4);
    }
}
