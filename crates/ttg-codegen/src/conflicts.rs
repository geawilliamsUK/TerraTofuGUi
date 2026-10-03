//! Arguments a provider refuses together, checked against the blocks an export produces.
//!
//! `validate` cannot catch these when one side is an input variable: the variable is
//! unknown at validate time, so the provider's `ConflictsWith` / `ExactlyOneOf` rule only
//! fires at `plan` ("password conflicts with manage_master_user_password"). The provider
//! schema the app bundles has no such rules either, so they are listed here by hand, one
//! group of mutually exclusive arguments at a time, for the resource types the curated
//! mappings emit and the ones `extra` most often reaches for. Each group was confirmed by
//! planning a configuration that sets both against the provider with `tofu test` and a
//! mock provider (AWS 6.67, azurerm 4.81, google 7.46), which runs the provider's own
//! validation with known values.
//!
//! The check runs on the emitted blocks, so it sees what the mapping set, what `extra`
//! added and what `extra` overrode, exactly as `plan` would.

use crate::diagnostics::{Code, Diagnostic, Severity};
use hcl::structure::{Block, Body, Structure};
use hcl::Expression;
use ttg_catalog::Catalog;
use ttg_core::Project;

/// At most one of `members` may be set on a `resource` (inside the nested blocks of
/// `path`, when it is not empty). A member is an argument or a nested block.
pub struct Exclusive {
    pub resource: &'static str,
    pub path: &'static [&'static str],
    pub members: &'static [&'static str],
}

const fn x(resource: &'static str, members: &'static [&'static str]) -> Exclusive {
    Exclusive {
        resource,
        path: &[],
        members,
    }
}

const NAME_PREFIX: &[&str] = &["name", "name_prefix"];

/// The curated table.
pub const TABLE: &[Exclusive] = &[
    // ------------------------------------------------------------------ AWS
    x("aws_db_instance", &["password", "manage_master_user_password"]),
    x("aws_db_instance", &["identifier", "identifier_prefix"]),
    x(
        "aws_rds_cluster",
        &["master_password", "manage_master_user_password"],
    ),
    x(
        "aws_cloudwatch_metric_alarm",
        &["statistic", "extended_statistic"],
    ),
    x("aws_cloudwatch_metric_alarm", &["metric_query", "metric_name"]),
    x("aws_cloudwatch_metric_alarm", &["metric_query", "namespace"]),
    x("aws_cloudwatch_metric_alarm", &["metric_query", "period"]),
    x("aws_cloudwatch_metric_alarm", &["metric_query", "statistic"]),
    x("aws_cloudwatch_metric_alarm", &["metric_query", "dimensions"]),
    x(
        "aws_cloudwatch_metric_alarm",
        &["threshold", "threshold_metric_id"],
    ),
    x("aws_secretsmanager_secret", NAME_PREFIX),
    x(
        "aws_secretsmanager_secret_version",
        &["secret_string", "secret_binary"],
    ),
    x("aws_lb", NAME_PREFIX),
    x("aws_lb_target_group", NAME_PREFIX),
    x("aws_security_group", NAME_PREFIX),
    x("aws_iam_role", NAME_PREFIX),
    x("aws_iam_policy", NAME_PREFIX),
    x("aws_cloudwatch_log_group", NAME_PREFIX),
    x("aws_sqs_queue", NAME_PREFIX),
    x("aws_sns_topic", NAME_PREFIX),
    x("aws_db_parameter_group", NAME_PREFIX),
    x("aws_db_subnet_group", NAME_PREFIX),
    x("aws_launch_template", NAME_PREFIX),
    x("aws_s3_bucket", &["bucket", "bucket_prefix"]),
    x("aws_lambda_function", &["filename", "image_uri", "s3_bucket"]),
    x(
        "aws_autoscaling_group",
        &[
            "launch_configuration",
            "launch_template",
            "mixed_instances_policy",
        ],
    ),
    x(
        "aws_autoscaling_group",
        &["availability_zones", "vpc_zone_identifier"],
    ),
    x(
        "aws_security_group_rule",
        &["cidr_blocks", "source_security_group_id"],
    ),
    x("aws_security_group_rule", &["self", "cidr_blocks"]),
    x(
        "aws_vpc_security_group_ingress_rule",
        &[
            "cidr_ipv4",
            "cidr_ipv6",
            "prefix_list_id",
            "referenced_security_group_id",
        ],
    ),
    x(
        "aws_vpc_security_group_egress_rule",
        &[
            "cidr_ipv4",
            "cidr_ipv6",
            "prefix_list_id",
            "referenced_security_group_id",
        ],
    ),
    x(
        "aws_route",
        &[
            "gateway_id",
            "nat_gateway_id",
            "transit_gateway_id",
            "network_interface_id",
            "vpc_peering_connection_id",
            "vpc_endpoint_id",
            "egress_only_gateway_id",
            "local_gateway_id",
            "carrier_gateway_id",
            "core_network_arn",
        ],
    ),
    x(
        "aws_eks_node_group",
        &["node_group_name", "node_group_name_prefix"],
    ),
    x("aws_instance", &["ipv6_address_count", "ipv6_addresses"]),
    x(
        "aws_elasticache_replication_group",
        &["num_cache_clusters", "num_node_groups"],
    ),
    // ------------------------------------------------------------------ Azure
    x("azurerm_key_vault_secret", &["value", "value_wo"]),
    x(
        "azurerm_postgresql_flexible_server",
        &["administrator_password", "administrator_password_wo"],
    ),
    x(
        "azurerm_mysql_flexible_server",
        &["administrator_password", "administrator_password_wo"],
    ),
    x(
        "azurerm_monitor_metric_alert",
        &[
            "criteria",
            "dynamic_criteria",
            "application_insights_web_test_location_availability_criteria",
        ],
    ),
    x(
        "azurerm_network_security_rule",
        &["source_port_range", "source_port_ranges"],
    ),
    x(
        "azurerm_network_security_rule",
        &["destination_port_range", "destination_port_ranges"],
    ),
    x(
        "azurerm_network_security_rule",
        &[
            "source_address_prefix",
            "source_address_prefixes",
            "source_application_security_group_ids",
        ],
    ),
    x(
        "azurerm_network_security_rule",
        &[
            "destination_address_prefix",
            "destination_address_prefixes",
            "destination_application_security_group_ids",
        ],
    ),
    x(
        "azurerm_kubernetes_cluster",
        &["dns_prefix", "dns_prefix_private_cluster"],
    ),
    // ------------------------------------------------------------------ Google Cloud
    x("google_compute_firewall", &["allow", "deny"]),
    x(
        "google_compute_firewall",
        &["source_tags", "source_service_accounts"],
    ),
    x(
        "google_compute_firewall",
        &["target_tags", "source_service_accounts"],
    ),
    x(
        "google_compute_firewall",
        &["target_service_accounts", "source_tags"],
    ),
    Exclusive {
        resource: "google_secret_manager_secret",
        path: &["replication"],
        members: &["auto", "user_managed"],
    },
    x("google_sql_user", &["password", "password_wo"]),
];

/// Is an argument or nested block of this name set in the body? An argument set to
/// `null` is not.
fn present(body: &Body, name: &str) -> bool {
    body.iter().any(|s| match s {
        Structure::Attribute(a) => a.key.as_str() == name && !matches!(a.expr, Expression::Null),
        Structure::Block(b) => b.identifier.as_str() == name,
    })
}

/// The bodies at `path` below `body` (every nested block of each name along the way).
fn bodies_at<'b>(body: &'b Body, path: &[&str]) -> Vec<&'b Body> {
    match path.split_first() {
        None => vec![body],
        Some((first, rest)) => body
            .iter()
            .filter_map(|s| match s {
                Structure::Block(b) if b.identifier.as_str() == *first => Some(&b.body),
                _ => None,
            })
            .flat_map(|b| bodies_at(b, rest))
            .collect(),
    }
}

/// The groups of the table one block breaks, as the names it sets from each.
pub fn conflicts_in(resource_type: &str, block: &Block) -> Vec<(Vec<&'static str>, &'static [&'static str])> {
    let mut out = Vec::new();
    for rule in TABLE.iter().filter(|r| r.resource == resource_type) {
        for body in bodies_at(&block.body, rule.path) {
            let set: Vec<&'static str> = rule
                .members
                .iter()
                .copied()
                .filter(|m| present(body, m))
                .collect();
            if set.len() > 1 {
                out.push((set, rule.path));
            }
        }
    }
    out
}

/// Every conflict in what the project exports for `provider`, as an error on the entity
/// whose block it is. Nothing when the project cannot be emitted at all (the diagnostics
/// that stop it say why).
pub fn check(p: &Project, cat: &Catalog, provider: &str) -> Vec<Diagnostic> {
    let Ok(g) = crate::emit::emit_unchecked(p, cat, provider, p.settings.tool) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (id, eb) in &g.entity_blocks {
        let name = p.entity(id).map(|e| e.name.to_string()).unwrap_or_default();
        for (address, block) in eb.addresses.iter().zip(eb.blocks()) {
            if address.starts_with("data.") {
                continue;
            }
            let resource_type = address.split('.').next().unwrap_or_default();
            for (set, path) in conflicts_in(resource_type, block) {
                let at = if path.is_empty() {
                    address.clone()
                } else {
                    format!("{address}.{}", path.join("."))
                };
                let names = set
                    .iter()
                    .map(|s| format!("`{s}`"))
                    .collect::<Vec<_>>()
                    .join(" and ");
                out.push(Diagnostic {
                    entity: Some(id.clone()),
                    severity: Severity::Error,
                    code: Code::Conflict,
                    message: format!(
                        "\"{name}\": {at} sets both {names}, which the provider refuses together when it plans (validate cannot see it while one of them is a variable); remove one, from the fields or from the extra arguments"
                    ),
                    provider: None,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(src: &str) -> Block {
        let body: Body = hcl::from_str(src).unwrap();
        match body.into_iter().next().unwrap() {
            Structure::Block(b) => b,
            _ => panic!("not a block"),
        }
    }

    #[test]
    fn finds_password_with_managed_master_password() {
        let b = block(
            r#"resource "aws_db_instance" "db" {
  password                    = var.db_password
  manage_master_user_password = true
}"#,
        );
        let found = conflicts_in("aws_db_instance", &b);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, vec!["password", "manage_master_user_password"]);
    }

    #[test]
    fn null_and_absent_are_not_set() {
        let b = block(
            r#"resource "aws_db_instance" "db" {
  password                    = null
  manage_master_user_password = true
}"#,
        );
        assert!(conflicts_in("aws_db_instance", &b).is_empty());
    }

    #[test]
    fn nested_blocks_and_paths() {
        let b = block(
            r#"resource "aws_cloudwatch_metric_alarm" "a" {
  metric_name = "x"
  metric_query {
    id = "e1"
  }
}"#,
        );
        assert_eq!(conflicts_in("aws_cloudwatch_metric_alarm", &b).len(), 1);
        let s = block(
            r#"resource "google_secret_manager_secret" "s" {
  replication {
    auto {}
    user_managed {
      replicas {
        location = "europe-west2"
      }
    }
  }
}"#,
        );
        let found = conflicts_in("google_secret_manager_secret", &s);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, &["replication"]);
    }

    #[test]
    fn every_table_resource_exists_in_the_schema() {
        let idx = ttg_schema::index();
        for rule in TABLE {
            let provider = match rule.resource.split('_').next() {
                Some("aws") => "aws",
                Some("azurerm") => "azure",
                _ => "gcp",
            };
            let schema = idx
                .resource(provider, rule.resource)
                .unwrap_or_else(|| panic!("{} is not in the {provider} schema", rule.resource));
            let mut at = schema;
            for seg in rule.path {
                at = at
                    .blocks
                    .get(*seg)
                    .map(|n| n.block())
                    .unwrap_or_else(|| panic!("{}: no block {seg}", rule.resource));
            }
            for m in rule.members {
                assert!(at.has(m), "{}: {m} is not an argument or block", rule.resource);
            }
        }
    }
}
