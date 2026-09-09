//! Catalog loading: embedded built-ins and on-disk directories.

use crate::schema::*;
use crate::CatalogError;
use indexmap::IndexMap;
use std::path::Path;

/// Built-in definitions, embedded at compile time. To add a definition to the built-in
/// set, add its file to this list (the file itself is data; this list is the only Rust
/// change and it is one line).
const BUILTIN_RESOURCES: &[(&str, &str)] = &[
    (
        "resources/resource_group.toml",
        include_str!("../../../definitions/resources/resource_group.toml"),
    ),
    (
        "resources/virtual_network.toml",
        include_str!("../../../definitions/resources/virtual_network.toml"),
    ),
    (
        "resources/subnet.toml",
        include_str!("../../../definitions/resources/subnet.toml"),
    ),
    (
        "resources/compute_instance.toml",
        include_str!("../../../definitions/resources/compute_instance.toml"),
    ),
    (
        "resources/object_storage.toml",
        include_str!("../../../definitions/resources/object_storage.toml"),
    ),
    (
        "resources/iam_role.toml",
        include_str!("../../../definitions/resources/iam_role.toml"),
    ),
    (
        "resources/security_group.toml",
        include_str!("../../../definitions/resources/security_group.toml"),
    ),
    (
        "resources/internet_gateway.toml",
        include_str!("../../../definitions/resources/internet_gateway.toml"),
    ),
    (
        "resources/nat_gateway.toml",
        include_str!("../../../definitions/resources/nat_gateway.toml"),
    ),
    (
        "resources/route_table.toml",
        include_str!("../../../definitions/resources/route_table.toml"),
    ),
    (
        "resources/relational_database.toml",
        include_str!("../../../definitions/resources/relational_database.toml"),
    ),
    (
        "resources/load_balancer.toml",
        include_str!("../../../definitions/resources/load_balancer.toml"),
    ),
    (
        "resources/function.toml",
        include_str!("../../../definitions/resources/function.toml"),
    ),
    (
        "resources/event_queue.toml",
        include_str!("../../../definitions/resources/event_queue.toml"),
    ),
    (
        "resources/topic.toml",
        include_str!("../../../definitions/resources/topic.toml"),
    ),
    (
        "resources/storage_queue.toml",
        include_str!("../../../definitions/resources/storage_queue.toml"),
    ),
    (
        "resources/servicebus_namespace.toml",
        include_str!("../../../definitions/resources/servicebus_namespace.toml"),
    ),
    (
        "resources/container_registry.toml",
        include_str!("../../../definitions/resources/container_registry.toml"),
    ),
    (
        "resources/kubernetes_cluster.toml",
        include_str!("../../../definitions/resources/kubernetes_cluster.toml"),
    ),
    (
        "resources/dns_zone.toml",
        include_str!("../../../definitions/resources/dns_zone.toml"),
    ),
    (
        "resources/dns_record.toml",
        include_str!("../../../definitions/resources/dns_record.toml"),
    ),
    (
        "resources/key_vault.toml",
        include_str!("../../../definitions/resources/key_vault.toml"),
    ),
    (
        "resources/secret.toml",
        include_str!("../../../definitions/resources/secret.toml"),
    ),
    (
        "resources/log_group.toml",
        include_str!("../../../definitions/resources/log_group.toml"),
    ),
    (
        "resources/autoscaling_group.toml",
        include_str!("../../../definitions/resources/autoscaling_group.toml"),
    ),
    (
        "resources/nosql_table.toml",
        include_str!("../../../definitions/resources/nosql_table.toml"),
    ),
    (
        "resources/cache.toml",
        include_str!("../../../definitions/resources/cache.toml"),
    ),
];

const BUILTIN_PROVIDERS: &[(&str, &str)] = &[
    (
        "providers/aws.toml",
        include_str!("../../../definitions/providers/aws.toml"),
    ),
    (
        "providers/azure.toml",
        include_str!("../../../definitions/providers/azure.toml"),
    ),
];

/// The loaded, validated set of resource and provider definitions.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub resources: IndexMap<String, ResourceDef>,
    pub providers: IndexMap<String, ProviderDef>,
    /// Where each resource definition came from (for error messages).
    pub sources: IndexMap<String, String>,
}

impl Catalog {
    /// The compiled-in catalog. Panics only if the embedded files are invalid, which the
    /// test-suite guards against.
    pub fn builtin() -> Catalog {
        Self::from_sources(
            BUILTIN_RESOURCES.iter().copied(),
            BUILTIN_PROVIDERS.iter().copied(),
        )
        .expect("embedded definitions are valid")
    }

    /// Load `<dir>/resources/*.toml` and `<dir>/providers/*.toml`.
    pub fn load_dir(dir: &Path) -> Result<Catalog, CatalogError> {
        let read_all = |sub: &str| -> Result<Vec<(String, String)>, CatalogError> {
            let d = dir.join(sub);
            let mut out = Vec::new();
            let entries = std::fs::read_dir(&d).map_err(|e| CatalogError::Io {
                path: d.display().to_string(),
                source: e,
            })?;
            let mut paths: Vec<_> = entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "toml"))
                .collect();
            paths.sort();
            for p in paths {
                let text = std::fs::read_to_string(&p).map_err(|e| CatalogError::Io {
                    path: p.display().to_string(),
                    source: e,
                })?;
                out.push((p.display().to_string(), text));
            }
            Ok(out)
        };
        let res = read_all("resources")?;
        let prov = read_all("providers")?;
        Self::from_sources(
            res.iter().map(|(a, b)| (a.as_str(), b.as_str())),
            prov.iter().map(|(a, b)| (a.as_str(), b.as_str())),
        )
    }

    /// Build a catalog from (path, toml text) pairs, then validate it as a whole.
    pub fn from_sources<'a>(
        resources: impl Iterator<Item = (&'a str, &'a str)>,
        providers: impl Iterator<Item = (&'a str, &'a str)>,
    ) -> Result<Catalog, CatalogError> {
        let mut cat = Catalog::default();
        for (path, text) in providers {
            let def: ProviderDef = toml::from_str(text).map_err(|e| CatalogError::Parse {
                path: path.to_string(),
                message: e.to_string(),
            })?;
            cat.providers.insert(def.provider.id.clone(), def);
        }
        for (path, text) in resources {
            let def: ResourceDef = toml::from_str(text).map_err(|e| CatalogError::Parse {
                path: path.to_string(),
                message: e.to_string(),
            })?;
            let id = def.resource.type_id.clone();
            if cat.resources.contains_key(&id) {
                return Err(CatalogError::Parse {
                    path: path.to_string(),
                    message: format!("duplicate resource type '{id}'"),
                });
            }
            cat.sources.insert(id.clone(), path.to_string());
            cat.resources.insert(id, def);
        }
        cat.resources.sort_keys();
        let problems = crate::validate::catalog(&cat);
        if !problems.is_empty() {
            return Err(CatalogError::Invalid(problems.join("\n")));
        }
        Ok(cat)
    }

    pub fn resource(&self, type_id: &str) -> Option<&ResourceDef> {
        self.resources.get(type_id)
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderDef> {
        self.providers.get(id)
    }

    pub fn mapping(&self, type_id: &str, provider: &str) -> Option<&ProviderMapping> {
        self.resources.get(type_id)?.providers.get(provider)
    }

    pub fn is_container(&self, type_id: &str) -> bool {
        self.resources
            .get(type_id)
            .is_some_and(|r| r.resource.kind == ResourceKind::Container)
    }

    /// Distinct categories in palette order.
    pub fn categories(&self) -> Vec<String> {
        let order = [
            "compute",
            "storage",
            "network",
            "database",
            "iam",
            "serverless",
            "container",
            "dns",
            "load_balancer",
            "secrets",
            "monitoring",
        ];
        let mut cats: Vec<String> = self
            .resources
            .values()
            .map(|r| r.resource.category.clone())
            .collect();
        cats.sort();
        cats.dedup();
        cats.sort_by_key(|c| order.iter().position(|o| o == c).unwrap_or(order.len()));
        cats
    }

    pub fn provider_ids(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }

    /// Relation definition on a resource by kind key.
    pub fn relation_def<'a>(&'a self, type_id: &str, kind: &str) -> Option<&'a RelationDef> {
        self.resources
            .get(type_id)?
            .relations
            .iter()
            .find(|r| r.kind == kind)
    }
}

/// Human-readable category label.
pub fn category_label(cat: &str) -> String {
    match cat {
        "iam" => "IAM".into(),
        "dns" => "DNS".into(),
        "load_balancer" => "Load Balancer".into(),
        "container" => "Container / Kubernetes".into(),
        "secrets" => "Secrets Management".into(),
        other => {
            let mut s = other.replace('_', " ");
            if let Some(f) = s.get_mut(0..1) {
                f.make_ascii_uppercase();
            }
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_loads() {
        let c = Catalog::builtin();
        assert!(c.resources.len() >= 6);
        assert!(c.providers.contains_key("aws") && c.providers.contains_key("azure"));
        assert!(c.is_container("virtual_network"));
        assert!(!c.is_container("subnet"));
    }
}
