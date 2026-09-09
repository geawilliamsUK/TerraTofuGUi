//! Dependency graph over the IR using `petgraph`.
//!
//! An entity depends on (a) every target of its outgoing edges and (b) its parent
//! container. `dependency_order` returns entities so that dependencies come first, or the
//! ids that form a cycle.

use crate::ir::{Id, Project};
use petgraph::algo::{is_cyclic_directed, toposort};
use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::HashMap;

pub struct DepGraph {
    pub graph: DiGraph<Id, ()>,
    pub index: HashMap<Id, NodeIndex>,
}

/// Build the dependency graph. Edge direction in petgraph: dependency -> dependent,
/// so a topological sort yields dependencies first.
pub fn build(p: &Project) -> DepGraph {
    let mut graph = DiGraph::new();
    let mut index = HashMap::new();
    for e in p.entities() {
        let ix = graph.add_node(e.id.to_string());
        index.insert(e.id.to_string(), ix);
    }
    for e in p.entities() {
        if let Some(parent) = e.parent {
            if let (Some(&a), Some(&b)) = (index.get(parent), index.get(e.id)) {
                graph.add_edge(a, b, ());
            }
        }
    }
    for edge in &p.edges {
        if let (Some(&a), Some(&b)) = (index.get(&edge.target), index.get(&edge.source)) {
            if a != b {
                graph.add_edge(a, b, ());
            }
        }
    }
    DepGraph { graph, index }
}

/// Entities in dependency order (dependencies first). `Err` carries the ids on a cycle.
pub fn dependency_order(p: &Project) -> Result<Vec<Id>, Vec<Id>> {
    let g = build(p);
    match toposort(&g.graph, None) {
        Ok(order) => Ok(order.into_iter().map(|ix| g.graph[ix].clone()).collect()),
        Err(cycle) => {
            // Collect a readable cycle by walking from the offending node.
            let start = cycle.node_id();
            let mut path = vec![g.graph[start].clone()];
            let mut cur = start;
            let mut seen = std::collections::HashSet::new();
            seen.insert(cur);
            while let Some(next) = g
                .graph
                .neighbors_directed(cur, petgraph::Direction::Outgoing)
                .find(|n| petgraph::algo::has_path_connecting(&g.graph, *n, start, None))
            {
                path.push(g.graph[next].clone());
                if next == start || !seen.insert(next) {
                    break;
                }
                cur = next;
            }
            Err(path)
        }
    }
}

pub fn has_cycle(p: &Project) -> bool {
    is_cyclic_directed(&build(p).graph)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::*;

    fn node(id: &str) -> Node {
        Node {
            id: id.into(),
            name: id.into(),
            resource_type: "x".into(),
            config: Default::default(),
            provider_config: Default::default(),
            position: Default::default(),
            size: None,
            parent: None,
            manual: false,
            providers: Vec::new(),
            extra: Default::default(),
        }
    }

    #[test]
    fn order_and_cycle() {
        let mut p = Project::new("g");
        for id in ["a", "b", "c"] {
            p.nodes.insert(id.into(), node(id));
        }
        p.add_edge("a", "b", Relation::DependsOn);
        p.add_edge("b", "c", Relation::DependsOn);
        let order = dependency_order(&p).unwrap();
        let pos = |x: &str| order.iter().position(|i| i == x).unwrap();
        assert!(pos("c") < pos("b") && pos("b") < pos("a"));
        p.add_edge("c", "a", Relation::DependsOn);
        assert!(has_cycle(&p));
        assert!(dependency_order(&p).is_err());
    }
}
