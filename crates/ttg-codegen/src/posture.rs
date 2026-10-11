//! Posture rules: facts about a design that usually deserve a second look — an API open
//! to the world, a rule that admits everyone, data with no key of its own. Posture is
//! data, not judgement: each rule has a stable id, reports `Code::Posture` diagnostics
//! (warnings or info, never export-blocking) whose message ends with that id, and can be
//! turned off per project (`settings.posture.disabled`). The rules run on one provider's
//! layer, like the other diagnostics.

use crate::diagnostics::{field_or_default, relation_targets, Code, Diagnostic, Severity};
use ttg_catalog::Catalog;
use ttg_core::{EntityRef, Id, Project, Relation, Value};

/// One posture rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    /// Stable id: what `settings.posture.disabled` lists and every message names.
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
}

/// Every rule, in the order the settings show them.
pub const RULES: &[Rule] = &[
    Rule {
        id: "public_kubernetes_api",
        title: "Public Kubernetes API with no allow-list",
        description: "A cluster whose API endpoint is public ('Private API endpoint' off) and accepts any address (no 'Public API access from' ranges, or 0.0.0.0/0 among them).",
    },
    Rule {
        id: "open_ingress",
        title: "Ingress from anywhere on ports other than 80/443",
        description: "A security group rule that admits 0.0.0.0/0 on any port other than 80 and 443, or on all ports.",
    },
    Rule {
        id: "data_tier_open_egress",
        title: "Data-tier group lets everything out",
        description: "A security group carried by a database, cache or file system with an egress rule to 0.0.0.0/0: data stores answer connections, they do not need to open them.",
    },
    Rule {
        id: "orphan_security_group",
        title: "Security group nothing uses",
        description: "A security group that nothing carries, links to or names as a rule source.",
    },
    Rule {
        id: "queue_dead_letter_alarm",
        title: "Queue without a dead-letter alarm",
        description: "A queue whose dead letters no alarm watches (warning), or that has no dead-letter queue at all (info).",
    },
    Rule {
        id: "database_backups",
        title: "Database without backups",
        description: "A database whose automated backups are off (retention 0 days).",
    },
    Rule {
        id: "classified_without_cmk",
        title: "Personal or payment data without a customer-managed key",
        description: "A resource classified personal or payment that can be linked to an encryption key ('Encrypted with') but is not.",
    },
];

pub fn rule(id: &str) -> Option<&'static Rule> {
    RULES.iter().find(|r| r.id == id)
}

/// The rule a posture diagnostic came from: the id its message ends with.
pub fn rule_of(d: &Diagnostic) -> Option<&'static str> {
    if d.code != Code::Posture {
        return None;
    }
    let tail = d.message.strip_suffix("')")?;
    let id = &tail[tail.rfind("(posture rule '")? + "(posture rule '".len()..];
    rule(id).map(|r| r.id)
}

/// Run every rule the project has not turned off, adding its findings to `out`. `p` is
/// the provider's layer; `full` holds the settings.
pub fn check(full: &Project, p: &Project, cat: &Catalog, provider: &str, out: &mut Vec<Diagnostic>) {
    let on = |id: &str| full.settings.posture.is_enabled(id);
    let mut push = |id: &'static str, entity: &str, severity: Severity, msg: String| {
        out.push(Diagnostic {
            entity: Some(entity.to_string()),
            severity,
            code: Code::Posture,
            message: format!("{msg} (posture rule '{id}')"),
            provider: None,
        })
    };
    let ents = p.entities();
    let of_type = |t: &'static str| ents.iter().filter(move |e| e.resource_type == t);

    if on("public_kubernetes_api") {
        for c in of_type("kubernetes_cluster") {
            let private = field_or_default(cat, provider, c, "private_endpoint", false)
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let cidrs = list_of(c.field("public_access_cidrs"));
            if !private && (cidrs.is_empty() || cidrs.iter().any(|x| is_anywhere(x))) {
                push(
                    "public_kubernetes_api",
                    c.id,
                    Severity::Warning,
                    format!(
                        "\"{}\": the Kubernetes API endpoint is public and accepts connections from any address; set 'Public API access from' to the ranges that administer the cluster, or turn on 'Private API endpoint'",
                        c.name
                    ),
                );
            }
        }
    }

    let groups: Vec<&EntityRef> = of_type("security_group").collect();
    if on("open_ingress") {
        for g in &groups {
            for r in rows(g) {
                if !r.ingress || !r.admits_anywhere() {
                    continue;
                }
                let web_only =
                    r.protocol != "all" && r.from <= r.to && (r.from..=r.to).all(|x| x == 80 || x == 443);
                if web_only {
                    continue;
                }
                let ports = if r.protocol == "all" {
                    "every port".to_string()
                } else if r.from == r.to {
                    format!("port {}", r.from)
                } else {
                    format!("ports {}-{}", r.from, r.to)
                };
                push(
                    "open_ingress",
                    g.id,
                    Severity::Warning,
                    format!(
                        "\"{}\": rule \"{}\" admits the whole internet ({}) on {ports}; narrow it to the ranges or security groups that need it",
                        g.name, r.name, r.cidr
                    ),
                );
            }
        }
    }

    if on("data_tier_open_egress") {
        for g in &groups {
            let stores: Vec<String> = ents
                .iter()
                .filter(|e| matches!(e.resource_type, "relational_database" | "cache" | "file_system"))
                .filter(|e| {
                    crate::reach::security_groups_of(p, cat, e)
                        .iter()
                        .any(|x| x == g.id)
                })
                .map(|e| format!("\"{}\"", e.name))
                .collect();
            if stores.is_empty() {
                continue;
            }
            if let Some(r) = rows(g).into_iter().find(|r| !r.ingress && r.cidr_anywhere()) {
                push(
                    "data_tier_open_egress",
                    g.id,
                    Severity::Info,
                    format!(
                        "\"{}\" guards {} but rule \"{}\" lets everything out to {}; a data store answers connections and needs no egress of its own",
                        g.name,
                        stores.join(", "),
                        r.name,
                        r.cidr
                    ),
                );
            }
        }
    }

    if on("orphan_security_group") {
        for g in &groups {
            let linked = p.edges_to(g.id).any(|e| e.relation != Relation::DependsOn);
            let named = groups
                .iter()
                .any(|other| rows(other).iter().any(|r| r.source_group == g.id));
            if !linked && !named && !crate::reach::group_has_members(p, cat, g.id) {
                push(
                    "orphan_security_group",
                    g.id,
                    Severity::Info,
                    format!(
                        "\"{}\" is used by nothing: no resource carries it and no rule names it; link it with 'Uses security group' or remove it",
                        g.name
                    ),
                );
            }
        }
    }

    if on("queue_dead_letter_alarm") {
        let alarms: Vec<&EntityRef> = of_type("alarm").collect();
        let watched_with = |q: &str, metrics: &[&str]| {
            alarms.iter().any(|a| {
                relation_targets(p, cat, a, Relation::AttributeReference)
                    .iter()
                    .any(|t| t == q)
                    && field_or_default(cat, provider, a, "metric", false)
                        .map(|v| v.display())
                        .is_some_and(|m| metrics.contains(&m.as_str()))
            })
        };
        for q in of_type("event_queue") {
            let is_dead_letter_queue = p.edges_to(q.id).any(|e| e.relation == Relation::DeadLettersTo);
            if is_dead_letter_queue {
                continue;
            }
            let dlqs: Vec<Id> = relation_targets(p, cat, q, Relation::DeadLettersTo);
            let watched = watched_with(q.id, &["dlq_depth"])
                || dlqs.iter().any(|d| {
                    watched_with(
                        d,
                        &["dlq_depth", "queue_depth", "queue_oldest_message_age", "custom"],
                    )
                });
            if watched {
                continue;
            }
            let dlq_names: Vec<String> = dlqs
                .iter()
                .filter_map(|d| p.entity(d))
                .map(|d| format!("\"{}\"", d.name))
                .collect();
            if !dlqs.is_empty() || provider == "azure" {
                let where_ = if dlq_names.is_empty() {
                    "its dead-letter sub-queue".to_string()
                } else {
                    dlq_names.join(", ")
                };
                push(
                    "queue_dead_letter_alarm",
                    q.id,
                    Severity::Warning,
                    format!(
                        "\"{}\": no alarm watches {where_}, so messages that keep failing pile up unnoticed; add an Alarm on its dead letters (metric 'dlq_depth')",
                        q.name
                    ),
                );
            } else {
                push(
                    "queue_dead_letter_alarm",
                    q.id,
                    Severity::Info,
                    format!(
                        "\"{}\" has no dead-letter queue, so a message that keeps failing is retried until it expires and nothing can alarm on it; link one with 'Dead-letters to'",
                        q.name
                    ),
                );
            }
        }
    }

    if on("database_backups") {
        for d in of_type("relational_database") {
            let days =
                field_or_default(cat, provider, d, "backup_retention_days", false).and_then(|v| v.as_int());
            if days == Some(0) {
                push(
                    "database_backups",
                    d.id,
                    Severity::Warning,
                    format!(
                        "\"{}\" keeps no automated backups (retention 0 days): a mistake or a failed upgrade cannot be undone; set 'Backup retention (days)'",
                        d.name
                    ),
                );
            }
        }
    }

    if on("classified_without_cmk") {
        for e in ents
            .iter()
            .filter(|e| e.classification.is_some_and(|c| c.is_personal()))
        {
            let can_link = cat.resource(e.resource_type).is_some_and(|def| {
                def.relations.iter().any(|r| {
                    r.kind == Relation::EncryptedWith.key()
                        && (r.providers.is_empty() || r.providers.iter().any(|x| x == provider))
                })
            });
            if !can_link || !relation_targets(p, cat, e, Relation::EncryptedWith).is_empty() {
                continue;
            }
            push(
                "classified_without_cmk",
                e.id,
                Severity::Warning,
                format!(
                    "\"{}\" holds {} but is encrypted with a provider-managed key; link a customer-managed Encryption Key with 'Encrypted with' so access to the data needs access to the key",
                    e.name,
                    e.classification.map(|c| c.display_name().to_lowercase()).unwrap_or_default()
                ),
            );
        }
    }
}

fn list_of(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::List(items)) => items.iter().filter(|s| !s.trim().is_empty()).cloned().collect(),
        Some(Value::Str(s)) if !s.trim().is_empty() => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn is_anywhere(cidr: &str) -> bool {
    matches!(cidr.trim(), "0.0.0.0/0" | "::/0")
}

/// A security group rule, as far as the posture rules care.
struct Row {
    name: String,
    ingress: bool,
    protocol: String,
    from: i64,
    to: i64,
    cidr: String,
    source_group: String,
    prefix_list: bool,
}

impl Row {
    fn cidr_anywhere(&self) -> bool {
        is_anywhere(&self.cidr)
    }
    /// The rule admits every address: an open range and no other source (a source group
    /// or a prefix list replaces the range).
    fn admits_anywhere(&self) -> bool {
        self.cidr_anywhere() && self.source_group.is_empty() && !self.prefix_list
    }
}

fn rows(g: &EntityRef) -> Vec<Row> {
    let Some(Value::Records(rs)) = g.field("rules") else {
        return Vec::new();
    };
    rs.iter()
        .map(|r| {
            let s = |k: &str| r.get(k).map(|v| v.display()).unwrap_or_default();
            Row {
                name: s("name"),
                ingress: s("direction") != "egress",
                protocol: r.get("protocol").map(|v| v.display()).unwrap_or("tcp".into()),
                from: r.get("from_port").and_then(|v| v.as_int()).unwrap_or(0),
                to: r.get("to_port").and_then(|v| v.as_int()).unwrap_or(0),
                cidr: s("cidr"),
                source_group: s("source_group"),
                prefix_list: !s("prefix_list").is_empty() || !s("source_prefix_list").is_empty(),
            }
        })
        .collect()
}
