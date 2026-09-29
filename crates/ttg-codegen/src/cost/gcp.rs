//! How entities turn into billable units on Google Cloud. One function per priced
//! abstract type; the unit prices are rows of `definitions/prices/gcp.toml`. Types that
//! cost nothing or are not estimated are listed there instead, with the reason.

use super::model::{fmt, num, pool_node_hours, Ctx, Model};
use ttg_core::Value;

pub(crate) fn model(type_id: &str) -> Model {
    Model::Priced(match type_id {
        "compute_instance" => compute_instance,
        "autoscaling_group" => autoscaling_group,
        "kubernetes_cluster" => kubernetes_cluster,
        "kubernetes_node_pool" => kubernetes_node_pool,
        "container_app" => container_app,
        "function" => function,
        "relational_database" => relational_database,
        "cache" => cache,
        "nosql_table" => nosql_table,
        "object_storage" => object_storage,
        "file_system" => file_system,
        "container_registry" => container_registry,
        "event_queue" => event_queue,
        "topic" => topic,
        "nat_gateway" => nat_gateway,
        "load_balancer" => load_balancer,
        "cdn" => cdn,
        "web_application_firewall" => web_application_firewall,
        "dns_zone" => dns_zone,
        "encryption_key" => encryption_key,
        "secret" => secret,
        "log_group" => log_group,
        "alarm" => alarm,
        _ => return Model::Missing,
    })
}

/// Boot disk of a VM from a stock image (the image's size, standard persistent disk).
const VM_BOOT_GB: f64 = 10.0;
/// Boot disk of a GKE node when the pool sets no size (100 GB balanced).
const GKE_BOOT_GB: f64 = 100.0;
/// Bytes Pub/Sub bills per message at least.
const PUBSUB_MIN_KB: f64 = 1.0;

fn vms(c: &mut Ctx, machine: Option<String>, count: f64) {
    let Some(m) = machine else {
        c.note("no machine type resolved from the mapping");
        return;
    };
    let hours = c.hours();
    c.charge(
        format!("{} x {m}, on demand", fmt(count)),
        "gce",
        &m,
        count * hours,
    );
    c.charge(
        format!("{} x {VM_BOOT_GB} GB standard boot disk", fmt(count)),
        "persistent_disk",
        "pd-standard",
        count * VM_BOOT_GB,
    );
    c.note("sustained-use discounts (up to 30% on N1 / N2 running all month) are not applied");
}

fn compute_instance(c: &mut Ctx) {
    let m = c.sku("main", "machine_type");
    vms(c, m, 1.0);
}

fn autoscaling_group(c: &mut Ctx) {
    let m = c.sku("template", "machine_type");
    let n = c.num("desired_instances", 1.0).max(c.num("min_instances", 0.0));
    c.note("priced at the desired instance count around the clock");
    vms(c, m, n);
}

/// GKE nodes (and their GPUs and boot disks) for `node_hours` in the month.
fn gke_nodes(
    c: &mut Ctx,
    machine: Option<String>,
    node_hours: f64,
    spot: bool,
    gpus: Option<(String, f64)>,
    what: &str,
) {
    let Some(m) = machine else {
        c.note("no machine type resolved from the mapping");
        return;
    };
    let hours = c.hours();
    let avg = node_hours / hours.max(1.0);
    let f = if spot { c.a("spot_price_fraction") } else { 1.0 };
    let how = if spot {
        format!(" on Spot (x{f} of on-demand)")
    } else {
        ", on demand".to_string()
    };
    c.charge(
        format!("{what}: {} node-hours of {m}{how}", fmt(node_hours)),
        "gce",
        &m,
        node_hours * f,
    );
    if let Some((gpu, per_node)) = gpus {
        c.charge(
            format!("{what}: {per_node} x {gpu} per node{how}"),
            "gpu",
            &gpu,
            node_hours * per_node * f,
        );
    }
    let disk = c.num("disk_size_gb", GKE_BOOT_GB);
    c.charge(
        format!(
            "{what}: {disk} GB balanced boot disk per node (average {} nodes)",
            fmt(avg)
        ),
        "persistent_disk",
        "pd-balanced",
        avg * disk,
    );
}

/// Node-hours of a regional pool: counts are per zone, so a pool that does not scale
/// to zero runs that many nodes in every zone.
fn regional_node_hours(c: &mut Ctx, desired: f64, min: f64, max: f64) -> f64 {
    let nh = pool_node_hours(c, desired, min, max);
    if min <= 0.0 {
        return nh;
    }
    let zones = c.a("zones_per_region");
    c.note(format!("regional: node counts are per zone, x{zones} zones"));
    nh * zones
}

fn kubernetes_cluster(c: &mut Ctx) {
    let hours = c.hours();
    c.charge("GKE cluster management fee", "gke", "cluster", hours);
    let m = c.sku("nodes", "node_config.machine_type");
    let (desired, min, max) = (
        c.num("node_count", 2.0),
        c.num("min_nodes", 1.0),
        c.num("max_nodes", 3.0),
    );
    let nh = regional_node_hours(c, desired, min, max);
    gke_nodes(c, m, nh, false, None, "node pool");
    c.note("the free-tier credit (one zonal or Autopilot cluster per billing account) does not cover a regional Standard cluster");
}

fn kubernetes_node_pool(c: &mut Ctx) {
    let m = c.sku("main", "node_config.machine_type");
    let (desired, min, max) = (
        c.num("desired_nodes", 0.0),
        c.num("min_nodes", 0.0),
        c.num("max_nodes", 3.0),
    );
    let nh = regional_node_hours(c, desired, min, max);
    let gpus = if c.flag("gpu") {
        let t = c.sku("main", "node_config.guest_accelerator.type");
        let n = c
            .arg("main", "node_config.guest_accelerator.count")
            .as_ref()
            .and_then(num)
            .unwrap_or(1.0);
        t.map(|t| (t, n))
    } else {
        None
    };
    let spot = c.flag("spot");
    gke_nodes(c, m, nh, spot, gpus, "node pool");
}

/// "1Gi" / "512Mi" as GiB.
fn gib(v: &Value) -> Option<f64> {
    let s = v.display();
    let s = s.trim();
    if let Some(x) = s.strip_suffix("Gi") {
        x.parse().ok()
    } else if let Some(x) = s.strip_suffix("Mi") {
        x.parse::<f64>().ok().map(|m| m / 1024.0)
    } else {
        s.parse().ok()
    }
}

fn container_app(c: &mut Ctx) {
    let path = "template.containers.resources.limits";
    let cpu = c
        .arg("main", &format!("{path}.cpu"))
        .as_ref()
        .and_then(num)
        .unwrap_or(1.0);
    let mem = c
        .arg("main", &format!("{path}.memory"))
        .as_ref()
        .and_then(gib)
        .unwrap_or(0.5);
    let replicas = c.num("replicas", 1.0).max(0.0);
    let seconds = c.hours() * 3600.0 * replicas;
    c.charge(
        format!(
            "{} instance(s) x {cpu} vCPU kept running (min instances)",
            fmt(replicas)
        ),
        "cloud_run",
        "vcpu",
        cpu * seconds,
    );
    c.charge(
        format!("{} instance(s) x {mem} GiB", fmt(replicas)),
        "cloud_run",
        "memory",
        mem * seconds,
    );
    c.note("instance-based billing (the instances are kept running): requests are not charged separately");
}

fn function(c: &mut Ctx) {
    let inv = c.a("function_invocations");
    let ms = c.a("function_duration_ms");
    let mem_mb = c.num("memory_mb", 128.0);
    // Cloud Run functions give CPU in proportion to memory (1 vCPU at 2 GiB, 0.083 at
    // 128 MiB); this is the proportional rule, close to the published steps.
    let vcpu = (mem_mb / 2048.0).max(0.083);
    let secs = inv * ms / 1000.0;
    c.charge(
        format!("{} invocations x {ms} ms x {:.3} vCPU", fmt(inv), vcpu),
        "cloud_run",
        "vcpu_request",
        secs * vcpu,
    );
    c.charge(
        format!("the same x {} MiB", fmt(mem_mb)),
        "cloud_run",
        "memory_request",
        secs * mem_mb / 1024.0,
    );
    c.charge("requests", "cloud_run", "requests", inv / 1e6);
    if c.emits("connector") {
        let hours = c.hours();
        c.charge(
            "Serverless VPC Access connector, 2 x e2-micro (its minimum)",
            "gce",
            "e2-micro",
            2.0 * hours,
        );
    }
    c.note("the free tier (2M requests, 180,000 vCPU-s, 360,000 GiB-s) is not deducted");
}

fn relational_database(c: &mut Ctx) {
    let Some(tier) = c.sku("main", "settings.tier") else {
        c.note("no tier resolved from the mapping");
        return;
    };
    let ha = c.flag("high_availability");
    let copies = if ha { 2.0 } else { 1.0 };
    let hours = c.hours();
    let how = if ha { ", regional HA (x2)" } else { "" };
    if let Some(rest) = tier.strip_prefix("db-custom-") {
        let mut it = rest.split('-');
        let vcpu: f64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(1.0);
        let mem_gb: f64 = it.next().and_then(|x| x.parse::<f64>().ok()).unwrap_or(3840.0) / 1024.0;
        c.charge(
            format!("{tier}: {vcpu} vCPU{how}"),
            "cloud_sql",
            "vcpu",
            vcpu * hours * copies,
        );
        c.charge(
            format!("{tier}: {mem_gb} GB memory{how}"),
            "cloud_sql",
            "memory",
            mem_gb * hours * copies,
        );
    } else {
        c.charge(format!("{tier}{how}"), "cloud_sql", &tier, hours * copies);
    }
    let gb = c.field("storage").as_ref().and_then(num).unwrap_or(32.0);
    c.charge(
        format!("{gb} GB SSD storage{how}"),
        "cloud_sql",
        "ssd",
        gb * copies,
    );
}

fn cache(c: &mut Ctx) {
    let gb = c
        .arg("main", "memory_size_gb")
        .as_ref()
        .and_then(num)
        .unwrap_or(1.0);
    let tier = if gb <= 4.0 {
        "basic_M1"
    } else if gb <= 10.0 {
        "basic_M2"
    } else {
        "basic_M3"
    };
    let hours = c.hours();
    c.charge(format!("{gb} GB Basic instance"), "memorystore", tier, gb * hours);
}

fn nosql_table(c: &mut Ctx) {
    let r = c.a("table_reads");
    let w = c.a("table_writes");
    let gb = c.a("table_gb");
    c.charge(
        format!("{} document reads", fmt(r)),
        "firestore",
        "read",
        r / 100_000.0,
    );
    c.charge(
        format!("{} document writes", fmt(w)),
        "firestore",
        "write",
        w / 100_000.0,
    );
    c.charge(format!("{gb} GiB stored"), "firestore", "storage", gb);
    c.note("the daily free quota (50,000 reads, 20,000 writes, 1 GiB) is not deducted");
}

fn object_storage(c: &mut Ctx) {
    let gb = c.a("bucket_gb");
    let w = c.a("bucket_writes");
    let r = c.a("bucket_reads");
    c.charge(format!("{gb} GB, Standard class"), "gcs", "standard", gb);
    c.charge(
        format!("{} Class A operations", fmt(w)),
        "gcs",
        "class_a",
        w / 1000.0,
    );
    c.charge(
        format!("{} Class B operations", fmt(r)),
        "gcs",
        "class_b",
        r / 1000.0,
    );
}

fn file_system(c: &mut Ctx) {
    let tier = c.sku("main", "tier").unwrap_or_else(|| "BASIC_HDD".into());
    let min = if tier == "BASIC_SSD" { 2560.0 } else { 1024.0 };
    let gb = c.num("size_gb", 1024.0).max(min);
    c.charge(format!("{gb} GiB {tier}, provisioned"), "filestore", &tier, gb);
    if tier == "BASIC_HDD" {
        c.charge("Basic HDD instance fee", "filestore", "BASIC_HDD_instance", 1.0);
    }
    c.note("Filestore bills its provisioned capacity, not the data stored");
}

fn container_registry(c: &mut Ctx) {
    let gb = c.a("registry_gb");
    c.charge(format!("{gb} GB of images"), "artifact_registry", "storage", gb);
}

/// Pub/Sub throughput for `messages` messages, each billed at least 1 KB, in TiB.
fn pubsub_tib(messages: f64) -> f64 {
    messages * PUBSUB_MIN_KB / (1024.0 * 1024.0 * 1024.0)
}

fn event_queue(c: &mut Ctx) {
    let msgs = c.a("queue_messages");
    c.charge(
        format!(
            "{} messages published and delivered (1 KB minimum each)",
            fmt(msgs)
        ),
        "pubsub",
        "throughput",
        pubsub_tib(msgs * 2.0),
    );
}

fn topic(c: &mut Ctx) {
    let msgs = c.a("topic_messages");
    let subs = c.list_len("subscriptions").max(1) as f64;
    c.charge(
        format!(
            "{} messages published, delivered to {subs} subscription(s)",
            fmt(msgs)
        ),
        "pubsub",
        "throughput",
        pubsub_tib(msgs * (1.0 + subs)),
    );
}

fn nat_gateway(c: &mut Ctx) {
    let hours = c.hours();
    let vms = c.a("nat_vms").clamp(0.0, 32.0);
    let gb = c.a("nat_data_gb");
    c.charge(
        format!("{vms} VMs using the gateway"),
        "cloud_nat",
        "vm",
        vms * hours,
    );
    c.charge(format!("{gb} GB processed"), "cloud_nat", "data", gb);
}

fn load_balancer(c: &mut Ctx) {
    let hours = c.hours();
    let rules = ["rule", "grule"].iter().filter(|k| c.emits(k)).count().max(1) as f64;
    let gb = c.a("lb_data_gb");
    c.charge(
        format!("{rules} forwarding rule(s)"),
        "load_balancing",
        "forwarding_rule",
        rules * hours,
    );
    c.charge(format!("{gb} GB processed"), "load_balancing", "data", gb);
}

fn cdn(c: &mut Ctx) {
    let gb = c.a("cdn_data_gb");
    let req = c.a("cdn_requests");
    c.charge(format!("{gb} GB cache egress"), "cloud_cdn", "egress", gb);
    c.charge(
        format!("{} cache lookups", fmt(req)),
        "cloud_cdn",
        "lookups",
        req / 10_000.0,
    );
}

fn web_application_firewall(c: &mut Ctx) {
    let rules = c.list_len("managed_rules") as f64
        + if c.num("rate_limit_per_5min", 0.0) > 0.0 {
            1.0
        } else {
            0.0
        };
    let req = c.a("waf_requests");
    c.charge("security policy", "cloud_armor", "policy", 1.0);
    c.charge(format!("{rules} rule(s)"), "cloud_armor", "rule", rules);
    c.charge(
        format!("{} requests", fmt(req)),
        "cloud_armor",
        "requests",
        req / 1e6,
    );
}

fn dns_zone(c: &mut Ctx) {
    let q = c.a("dns_queries");
    c.charge("managed zone", "cloud_dns", "zone", 1.0);
    c.charge(format!("{} queries", fmt(q)), "cloud_dns", "queries", q / 1e6);
}

fn encryption_key(c: &mut Ctx) {
    let r = c.a("key_requests");
    c.charge("active key version", "cloud_kms", "key_version", 1.0);
    c.charge(
        format!("{} operations", fmt(r)),
        "cloud_kms",
        "operations",
        r / 10_000.0,
    );
    c.note("rotation adds a version each period and older versions stay billable while enabled; one version is priced");
}

fn secret(c: &mut Ctx) {
    let r = c.a("secret_requests");
    c.charge("active secret version", "secret_manager", "version", 1.0);
    c.charge(
        format!("{} access operations", fmt(r)),
        "secret_manager",
        "access",
        r / 10_000.0,
    );
}

fn log_group(c: &mut Ctx) {
    let gb = c.a("log_ingest_gb");
    let days = c.num("retention_days", 30.0);
    c.charge(format!("{gb} GiB ingested"), "logging", "ingest", gb);
    let extra = ((days - 30.0) / 30.0).max(0.0) * gb;
    if extra > 0.0 {
        c.charge(
            format!("{} GiB kept past 30 days ({days}-day retention)", fmt(extra)),
            "logging",
            "retention",
            extra,
        );
    }
    c.note("the first 50 GiB a month per project are free; not deducted");
}

fn alarm(c: &mut Ctx) {
    c.charge("alerting policy condition", "monitoring", "condition", 1.0);
    c.note("Cloud Monitoring does not charge for alerting yet (announced for no sooner than September 2027); the price list says when that changes");
}
