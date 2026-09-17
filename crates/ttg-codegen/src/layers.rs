//! Provider layers: the abstract graph minus what is not part of a provider's design.
//!
//! An entity is off a provider's layer when it is tagged for other providers only
//! (`providers = ["azure"]` on the node), when its abstract type does not exist on
//! that provider (a provider-scoped definition, auto-tagged), or when a definition check
//! with `severity = "omit"` fired on it (the provider cannot express this one entity, so
//! it is left out rather than blocking the whole export). Codegen, diagnostics and
//! reachability all run on [`project_for`]; [`off_layer`] feeds the parity report.

use std::collections::BTreeSet;
use ttg_catalog::Catalog;
use ttg_core::{Id, Project};

/// Does this abstract type exist on the provider (provider-scoped definitions)?
pub fn type_on(cat: &Catalog, type_id: &str, provider: &str) -> bool {
    cat.resource(type_id)
        .is_none_or(|d| d.resource.providers.is_empty() || d.resource.providers.iter().any(|p| p == provider))
}

/// Entities a `severity = "omit"` check takes out of this provider's export, with the
/// check's message. The checks are evaluated on the layer the other rules leave, so a
/// condition never sees an entity the provider does not have in the first place.
pub fn omitted(p: &Project, cat: &Catalog, provider: &str) -> Vec<(Id, String)> {
    let possible = p.entities().iter().any(|e| {
        cat.mapping(e.resource_type, provider)
            .is_some_and(|m| m.checks.iter().any(|c| c.severity == "omit"))
    });
    if !possible {
        return Vec::new();
    }
    let base = p.layer(provider, &|t| type_on(cat, t, provider));
    crate::diagnostics::omit_checks(&base, cat, provider)
}

fn omitted_ids(om: &[(Id, String)]) -> BTreeSet<Id> {
    om.iter().map(|(id, _)| id.clone()).collect()
}

/// The provider's layer as a stand-alone project (see `Project::layer`).
pub fn project_for(p: &Project, cat: &Catalog, provider: &str) -> Project {
    project_for_omitting(p, cat, provider, &omitted_ids(&omitted(p, cat, provider)))
}

/// [`project_for`] with the omitted set already computed (see [`omitted`]).
pub fn project_for_omitting(p: &Project, cat: &Catalog, provider: &str, om: &BTreeSet<Id>) -> Project {
    p.layer_omitting(provider, &|t| type_on(cat, t, provider), om)
}

/// Ids of the entities on the layer.
pub fn members(p: &Project, cat: &Catalog, provider: &str) -> BTreeSet<Id> {
    p.layer_members_omitting(
        provider,
        &|t| type_on(cat, t, provider),
        &omitted_ids(&omitted(p, cat, provider)),
    )
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
    /// An `omit` check fired: the provider has no way to express this entity, so it is
    /// left out of this provider's export. Carries the check's message.
    Check(String),
}

/// Every entity that is not part of the provider's layer, with the reason.
pub fn off_layer(p: &Project, cat: &Catalog, provider: &str) -> Vec<(Id, OffReason)> {
    off_layer_omitting(p, cat, provider, &omitted(p, cat, provider))
}

/// [`off_layer`] with the omitted set already computed (see [`omitted`]).
pub fn off_layer_omitting(
    p: &Project,
    cat: &Catalog,
    provider: &str,
    om: &[(Id, String)],
) -> Vec<(Id, OffReason)> {
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
        } else if let Some((_, msg)) = om.iter().find(|(id, _)| id == e.id) {
            out.push((e.id.to_string(), OffReason::Check(msg.clone())));
        }
    }
    out
}
