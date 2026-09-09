//! Copy / paste of a sub-diagram as JSON.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use ttg_core::{Container, Edge, Id, Node, Project};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Clip {
    pub ttg_clipboard: u32,
    pub nodes: Vec<Node>,
    pub containers: Vec<Container>,
    pub edges: Vec<Edge>,
}

/// Collect the selected entities (plus all descendants of selected containers) and the
/// edges between them.
pub fn copy(p: &Project, selection: &BTreeSet<Id>) -> Option<Clip> {
    let mut ids: BTreeSet<Id> = selection.clone();
    for id in selection {
        if p.containers.contains_key(id) {
            ids.extend(p.descendants_of(id));
        }
    }
    if ids.is_empty() {
        return None;
    }
    Some(Clip {
        ttg_clipboard: 1,
        nodes: ids.iter().filter_map(|i| p.nodes.get(i).cloned()).collect(),
        containers: ids.iter().filter_map(|i| p.containers.get(i).cloned()).collect(),
        edges: p
            .edges
            .iter()
            .filter(|e| ids.contains(&e.source) && ids.contains(&e.target))
            .cloned()
            .collect(),
    })
}

/// Insert a clip with fresh ids, offset positions and de-duplicated names. Returns the
/// new ids so the caller can select them.
pub fn paste(p: &mut Project, clip: &Clip, offset: (i32, i32)) -> Vec<Id> {
    let mut map: HashMap<Id, Id> = HashMap::new();
    let mut new_ids = Vec::new();
    let taken: BTreeSet<String> = p.entities().iter().map(|e| ttg_core::slugify(e.name)).collect();
    let mut used = taken.clone();
    let mut unique_name = |name: &str| -> String {
        let mut candidate = name.to_string();
        let mut n = 2;
        while used.contains(&ttg_core::slugify(&candidate)) {
            candidate = format!("{name} {n}");
            n += 1;
        }
        used.insert(ttg_core::slugify(&candidate));
        candidate
    };
    for c in &clip.containers {
        let id = p.fresh_id(prefix(&c.container_type));
        map.insert(c.id.clone(), id.clone());
    }
    for n in &clip.nodes {
        let id = p.fresh_id(prefix(&n.resource_type));
        map.insert(n.id.clone(), id.clone());
    }
    let remap_parent = |parent: &Option<Id>, p: &Project| -> Option<Id> {
        match parent {
            Some(old) => match map.get(old) {
                Some(new) => Some(new.clone()),
                None if p.containers.contains_key(old) => Some(old.clone()),
                None => None,
            },
            None => None,
        }
    };
    for c in &clip.containers {
        let mut c2 = c.clone();
        c2.id = map[&c.id].clone();
        c2.name = unique_name(&c.name);
        c2.parent = remap_parent(&c.parent, p);
        c2.position.x += offset.0;
        c2.position.y += offset.1;
        new_ids.push(c2.id.clone());
        p.containers.insert(c2.id.clone(), c2);
    }
    for n in &clip.nodes {
        let mut n2 = n.clone();
        n2.id = map[&n.id].clone();
        n2.name = unique_name(&n.name);
        n2.parent = remap_parent(&n.parent, p);
        n2.position.x += offset.0;
        n2.position.y += offset.1;
        new_ids.push(n2.id.clone());
        p.nodes.insert(n2.id.clone(), n2);
    }
    for e in &clip.edges {
        if let (Some(s), Some(t)) = (map.get(&e.source), map.get(&e.target)) {
            p.add_edge(s, t, e.relation);
        }
    }
    new_ids
}

/// Short id prefix for a resource type (`virtual_network` -> `vnet`, `subnet` -> `subnet`).
pub fn prefix(resource_type: &str) -> &str {
    match resource_type {
        "virtual_network" => "vnet",
        "resource_group" => "rg",
        "compute_instance" => "vm",
        "object_storage" => "obj",
        "iam_role" => "role",
        other => other.split('_').next().unwrap_or("res"),
    }
}

#[allow(dead_code)]
pub fn _keep(_: BTreeMap<(), ()>) {}
