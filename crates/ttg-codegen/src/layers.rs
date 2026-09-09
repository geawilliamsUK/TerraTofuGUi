//! Provider layers: the abstract graph minus what is not part of a provider's design.
//!
//! An entity is off a provider's layer when it is tagged for other providers only
//! (`providers = ["azure"]` on the node) or when its abstract type does not exist on
//! that provider (a provider-scoped definition, auto-tagged). Codegen, diagnostics and
//! reachability all run on [`project_for`]; [`off_layer`] feeds the parity report.

use ttg_catalog::Catalog;
use ttg_core::{Id, Project};

/// Does this abstract type exist on the provider (provider-scoped definitions)?
pub fn type_on(cat: &Catalog, type_id: &str, provider: &str) -> bool {
    cat.resource(type_id)
        .is_none_or(|d| d.resource.providers.is_empty() || d.resource.providers.iter().any(|p| p == provider))
}

/// The provider's layer as a stand-alone project (see `Project::layer`).
pub fn project_for(p: &Project, cat: &Catalog, provider: &str) -> Project {
    p.layer(provider, &|t| type_on(cat, t, provider))
}

/// Ids of the entities on the layer.
pub fn members(p: &Project, cat: &Catalog, provider: &str) -> std::collections::BTreeSet<Id> {
    p.layer_members(provider, &|t| type_on(cat, t, provider))
}

/// Why an entity is off a layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OffReason {
    /// The user tagged it for other providers.
    Tagged(Vec<String>),
    /// Its type is provider-scoped and has no counterpart here.
    NoCounterpart(Vec<String>),
    /// It sits inside a container that is off the layer (and is itself untagged); it
    /// is kept, re-parented, so this is informational only.
    InsideOffLayerContainer,
}

/// Every entity that is not part of the provider's layer, with the reason.
pub fn off_layer(p: &Project, cat: &Catalog, provider: &str) -> Vec<(Id, OffReason)> {
    let mut out = Vec::new();
    for e in p.entities() {
        let tags = if let Some(n) = p.nodes.get(e.id) {
            &n.providers
        } else {
            &p.containers[e.id].providers
        };
        if !tags.is_empty() && !tags.iter().any(|x| x == provider) {
            out.push((e.id.to_string(), OffReason::Tagged(tags.clone())));
        } else if !type_on(cat, e.resource_type, provider) {
            let scope = cat
                .resource(e.resource_type)
                .map(|d| d.resource.providers.clone())
                .unwrap_or_default();
            out.push((e.id.to_string(), OffReason::NoCounterpart(scope)));
        }
    }
    out
}
