//! Catalog-independent structural validation of a `Project`.
//!
//! Catalog-aware checks (field types, required fields, mapping coverage) live in
//! `ttg-codegen::diagnostics`; this module only checks what can be wrong with the graph
//! itself.

use crate::{graph, ir::Project};
use std::collections::HashSet;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// Entity the problem is attached to, if any.
    pub entity: Option<String>,
    pub message: String,
}

#[derive(Debug, Default, Clone)]
pub struct Report {
    pub errors: Vec<Problem>,
}

impl Report {
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
    fn err(&mut self, entity: Option<&str>, msg: impl Into<String>) {
        self.errors.push(Problem {
            entity: entity.map(|s| s.to_string()),
            message: msg.into(),
        });
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for p in &self.errors {
            match &p.entity {
                Some(e) => writeln!(f, "- [{e}] {}", p.message)?,
                None => writeln!(f, "- {}", p.message)?,
            }
        }
        Ok(())
    }
}

pub fn structural(p: &Project) -> Report {
    let mut r = Report::default();

    // ids consistent with map keys
    for (k, n) in &p.nodes {
        if k != &n.id {
            r.err(
                Some(k),
                format!("node key '{k}' does not match its id '{}'", n.id),
            );
        }
        if p.containers.contains_key(k) {
            r.err(Some(k), "id used by both a node and a container");
        }
    }
    for (k, c) in &p.containers {
        if k != &c.id {
            r.err(
                Some(k),
                format!("container key '{k}' does not match its id '{}'", c.id),
            );
        }
    }

    // unique HCL names
    let mut seen: HashSet<String> = HashSet::new();
    for e in p.entities() {
        if e.name.trim().is_empty() {
            r.err(Some(e.id), "name is empty");
            continue;
        }
        let slug = crate::slugify(e.name);
        if !seen.insert(slug.clone()) {
            r.err(
                Some(e.id),
                format!("name '{}' collides with another resource (slug '{slug}')", e.name),
            );
        }
    }

    // parents exist and are containers; no containment cycles
    for e in p.entities() {
        if let Some(parent) = e.parent {
            if !p.containers.contains_key(parent) {
                r.err(Some(e.id), format!("parent '{parent}' is not a container"));
            } else if parent == e.id {
                r.err(Some(e.id), "entity is its own parent");
            }
        }
    }
    for c in p.containers.values() {
        // walk up; if we come back to ourselves it is a cycle
        let mut cur = c.parent.as_deref();
        let mut steps = 0;
        while let Some(pid) = cur {
            if pid == c.id {
                r.err(Some(&c.id), "containment cycle");
                break;
            }
            steps += 1;
            if steps > 64 {
                break;
            }
            cur = p.containers.get(pid).and_then(|x| x.parent.as_deref());
        }
    }

    // edges
    for (i, e) in p.edges.iter().enumerate() {
        if !p.contains(&e.source) {
            r.err(None, format!("edge #{i}: source '{}' does not exist", e.source));
        }
        if !p.contains(&e.target) {
            r.err(None, format!("edge #{i}: target '{}' does not exist", e.target));
        }
        if e.source == e.target {
            r.err(Some(&e.source), "edge from an entity to itself");
        }
    }

    // dependency cycles (edges + containment)
    if let Err(cycle) = graph::dependency_order(p) {
        r.err(
            None,
            format!("dependency cycle involving: {}", cycle.join(" -> ")),
        );
    }

    r
}
