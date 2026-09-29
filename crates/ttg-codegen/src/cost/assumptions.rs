//! Usage assumptions: the figures a price list cannot know (how much a bucket holds, how
//! many hours a GPU pool that scales to zero actually runs). Each has a default here; a
//! project overrides it in `settings.cost_assumptions.values`, one entity in
//! `settings.cost_assumptions.entities`, and a single `cost_estimate` call on top of both.

use serde::Serialize;
use std::collections::BTreeMap;
use ttg_core::Settings;

/// One assumption the models may read.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Assumption {
    pub key: &'static str,
    pub default: f64,
    pub unit: &'static str,
    pub label: &'static str,
    /// Abstract types whose estimate reads it (empty: many).
    pub types: &'static [&'static str],
}

const fn a(
    key: &'static str,
    default: f64,
    unit: &'static str,
    label: &'static str,
    types: &'static [&'static str],
) -> Assumption {
    Assumption {
        key,
        default,
        unit,
        label,
        types,
    }
}

/// Every assumption, in the order the Cost window lists them.
pub const ASSUMPTIONS: &[Assumption] = &[
    a("hours_per_month", 730.0, "hours", "Hours in a month", &[]),
    a(
        "pool_node_hours_per_day",
        8.0,
        "node-hours a day",
        "Node-hours a day of a pool that scales to zero (min nodes 0), such as a GPU pool that runs only while there is work",
        &["kubernetes_node_pool", "kubernetes_cluster"],
    ),
    a(
        "spot_price_fraction",
        0.35,
        "x on-demand",
        "Spot / preemptible price as a fraction of on-demand (spot prices move; 0.3-0.4 is typical)",
        &["kubernetes_node_pool"],
    ),
    a(
        "zones_per_region",
        3.0,
        "zones",
        "Zones a regional GKE cluster spreads over (its node counts are per zone)",
        &["kubernetes_cluster", "kubernetes_node_pool"],
    ),
    a("bucket_gb", 50.0, "GB", "Data stored per bucket", &["object_storage"]),
    a(
        "bucket_writes",
        100_000.0,
        "requests a month",
        "Write (PUT / Class A) requests per bucket",
        &["object_storage"],
    ),
    a(
        "bucket_reads",
        1_000_000.0,
        "requests a month",
        "Read (GET / Class B) requests per bucket",
        &["object_storage"],
    ),
    a(
        "file_system_gb",
        100.0,
        "GB",
        "Data stored on a file system billed by use (EFS); provisioned ones bill their size",
        &["file_system"],
    ),
    a("registry_gb", 10.0, "GB", "Images stored per container registry", &["container_registry"]),
    a(
        "log_ingest_gb",
        5.0,
        "GB a month",
        "Logs written to each log group / workspace",
        &["log_group"],
    ),
    a(
        "queue_messages",
        1_000_000.0,
        "messages a month",
        "Messages through each queue",
        &["event_queue", "storage_queue"],
    ),
    a("topic_messages", 100_000.0, "messages a month", "Messages published to each topic", &["topic"]),
    a(
        "function_invocations",
        1_000_000.0,
        "invocations a month",
        "Invocations of each function",
        &["function"],
    ),
    a("function_duration_ms", 200.0, "ms", "Average duration of one invocation", &["function"]),
    a(
        "container_requests",
        1_000_000.0,
        "requests a month",
        "Requests served by each container app",
        &["container_app"],
    ),
    a(
        "nat_data_gb",
        100.0,
        "GB a month",
        "Data through each NAT gateway",
        &["nat_gateway"],
    ),
    a(
        "nat_vms",
        4.0,
        "VMs",
        "VMs (nodes) sending through a Cloud NAT gateway (Google Cloud bills per VM)",
        &["nat_gateway"],
    ),
    a(
        "lb_capacity_units",
        1.0,
        "LCU",
        "Average capacity units of an AWS load balancer",
        &["load_balancer"],
    ),
    a(
        "lb_data_gb",
        100.0,
        "GB a month",
        "Data processed by each load balancer (Azure, Google Cloud)",
        &["load_balancer"],
    ),
    a("cdn_data_gb", 100.0, "GB a month", "Data served by each CDN", &["cdn"]),
    a("cdn_requests", 1_000_000.0, "requests a month", "Requests to each CDN", &["cdn"]),
    a(
        "waf_requests",
        1_000_000.0,
        "requests a month",
        "Requests inspected by each web application firewall",
        &["web_application_firewall"],
    ),
    a(
        "endpoint_data_gb",
        10.0,
        "GB a month",
        "Data through each private endpoint",
        &["private_endpoint"],
    ),
    a("dns_queries", 1_000_000.0, "queries a month", "Queries answered by each DNS zone", &["dns_zone"]),
    a(
        "key_requests",
        100_000.0,
        "requests a month",
        "Encrypt / decrypt / generate-data-key requests per key",
        &["encryption_key"],
    ),
    a("secret_requests", 10_000.0, "requests a month", "Reads of each secret", &["secret"]),
    a("table_gb", 10.0, "GB", "Data stored per NoSQL table", &["nosql_table"]),
    a(
        "table_reads",
        10_000_000.0,
        "reads a month",
        "Reads (read request units) per NoSQL table",
        &["nosql_table"],
    ),
    a(
        "table_writes",
        1_000_000.0,
        "writes a month",
        "Writes (write request units) per NoSQL table",
        &["nosql_table"],
    ),
    a(
        "monthly_active_users",
        1_000.0,
        "users",
        "Monthly active users of a user directory",
        &["user_identity"],
    ),
    a(
        "audit_data_events",
        1_000_000.0,
        "events a month",
        "Data events recorded by an audit trail that has them switched on",
        &["audit_trail"],
    ),
    a(
        "alarm_time_series",
        1.0,
        "time series",
        "Metric time series each alarm watches",
        &["alarm"],
    ),
];

/// The definition of an assumption by key.
pub fn assumption(key: &str) -> Option<&'static Assumption> {
    ASSUMPTIONS.iter().find(|a| a.key == key)
}

/// Where the value an estimate used came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Default,
    /// `settings.cost_assumptions.values`.
    Project,
    /// `settings.cost_assumptions.entities[<id>]`.
    Entity,
    /// Given for one estimate only (the `cost_estimate` tool's `assumptions`).
    Call,
}

/// An assumption as one estimate read it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Used {
    pub key: String,
    pub value: f64,
    pub unit: String,
    pub source: Source,
}

/// Assumption values layered: default < project < entity < this call.
#[derive(Debug, Clone, Default)]
pub struct Resolver {
    project: BTreeMap<String, f64>,
    entities: BTreeMap<String, BTreeMap<String, f64>>,
    call: BTreeMap<String, f64>,
}

impl Resolver {
    pub fn new(settings: &Settings, call: &BTreeMap<String, f64>) -> Self {
        Resolver {
            project: settings.cost_assumptions.values.clone(),
            entities: settings.cost_assumptions.entities.clone(),
            call: call.clone(),
        }
    }

    /// The value for one entity, and where it came from. Unknown keys are 0 (the models
    /// only ask for keys in [`ASSUMPTIONS`]; a test holds them to it).
    pub fn get(&self, key: &str, entity: &str) -> Used {
        let def = assumption(key);
        let (value, source) = if let Some(v) = self.call.get(key) {
            (*v, Source::Call)
        } else if let Some(v) = self.entities.get(entity).and_then(|m| m.get(key)) {
            (*v, Source::Entity)
        } else if let Some(v) = self.project.get(key) {
            (*v, Source::Project)
        } else {
            (def.map(|d| d.default).unwrap_or(0.0), Source::Default)
        };
        Used {
            key: key.to_string(),
            value,
            unit: def.map(|d| d.unit.to_string()).unwrap_or_default(),
            source,
        }
    }

    /// The project-wide value (no entity override), for the list of assumptions in force.
    pub fn global(&self, key: &str) -> Used {
        self.get(key, "\u{0}")
    }

    /// Keys set in the project or the call that no model reads: typos, mostly.
    pub fn unknown_keys(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .project
            .keys()
            .chain(self.call.keys())
            .chain(self.entities.values().flat_map(|m| m.keys()))
            .filter(|k| assumption(k).is_none())
            .cloned()
            .collect();
        out.sort();
        out.dedup();
        out
    }
}
