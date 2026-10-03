//! How entities turn into billable units on Azure. One function per priced abstract type;
//! the unit prices are rows of `definitions/prices/azure.toml`. Types that cost nothing or
//! are not estimated are listed there instead, with the reason.

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
        "storage_queue" => storage_queue,
        "servicebus_namespace" => servicebus_namespace,
        "nat_gateway" => nat_gateway,
        "load_balancer" => load_balancer,
        "private_endpoint" => private_endpoint,
        "cdn" => cdn,
        "dns_zone" => dns_zone,
        "encryption_key" => encryption_key,
        "secret" => secret,
        "log_group" => log_group,
        "alarm" => alarm,
        _ => return Model::Missing,
    })
}

/// OS disk of a VM from the mapping (Standard HDD, the image's 30 GB).
const VM_OS_DISK_GB: f64 = 30.0;
/// AKS node OS disk when the pool sets no size: a 128 GB managed Premium SSD.
const AKS_OS_DISK_GB: f64 = 128.0;

/// The managed-disk tier a disk of `gb` is billed as: the smallest that holds it.
fn disk_tier(premium: bool, gb: f64) -> &'static str {
    let tiers: &[(f64, &str)] = if premium {
        &[
            (32.0, "P4"),
            (64.0, "P6"),
            (128.0, "P10"),
            (256.0, "P15"),
            (512.0, "P20"),
        ]
    } else {
        &[(32.0, "S4"), (64.0, "S6"), (128.0, "S10"), (256.0, "S15")]
    };
    tiers
        .iter()
        .find(|(size, _)| gb <= *size)
        .map(|(_, t)| *t)
        .unwrap_or(tiers[tiers.len() - 1].1)
}

fn vms(c: &mut Ctx, size: Option<String>, count: f64) {
    let Some(size) = size else {
        c.note("no VM size resolved from the mapping");
        return;
    };
    let hours = c.hours();
    c.charge(
        format!("{} x {size}, pay as you go", fmt(count)),
        "vm",
        &size,
        count * hours,
    );
    let tier = disk_tier(false, VM_OS_DISK_GB);
    c.charge(
        format!(
            "{} x {VM_OS_DISK_GB} GB Standard HDD OS disk ({tier})",
            fmt(count)
        ),
        "managed_disk",
        tier,
        count,
    );
}

fn compute_instance(c: &mut Ctx) {
    let size = c.sku("main", "size");
    vms(c, size, 1.0);
}

fn autoscaling_group(c: &mut Ctx) {
    let size = c.sku("main", "sku");
    let n = c.num("desired_instances", 1.0).max(c.num("min_instances", 0.0));
    c.note("priced at the desired instance count around the clock");
    vms(c, size, n);
}

/// AKS nodes plus their managed OS disks, for `node_hours` in the month.
fn aks_nodes(c: &mut Ctx, size: Option<String>, node_hours: f64, disk_gb: f64, spot: bool, what: &str) {
    let Some(size) = size else {
        c.note("no VM size resolved from the mapping");
        return;
    };
    let hours = c.hours();
    let avg = node_hours / hours.max(1.0);
    let label = format!("{what}: {} node-hours of {size}", fmt(node_hours));
    if spot {
        let f = c.a("spot_price_fraction");
        c.charge(
            format!("{label} on spot (x{f} of pay as you go)"),
            "vm",
            &size,
            node_hours * f,
        );
    } else {
        c.charge(format!("{label}, pay as you go"), "vm", &size, node_hours);
    }
    let tier = disk_tier(true, disk_gb);
    c.charge(
        format!(
            "{what}: {disk_gb} GB Premium SSD OS disk per node ({tier}, average {} nodes)",
            fmt(avg)
        ),
        "managed_disk",
        tier,
        avg,
    );
}

fn kubernetes_cluster(c: &mut Ctx) {
    let hours = c.hours();
    c.charge("AKS control plane, Free tier", "aks", "free", hours);
    let size = c.sku("main", "default_node_pool.vm_size");
    let (desired, min, max) = (
        c.num("node_count", 2.0),
        c.num("min_nodes", 1.0),
        c.num("max_nodes", 3.0),
    );
    let nh = pool_node_hours(c, desired, min, max);
    let disk = c.num("disk_size_gb", AKS_OS_DISK_GB);
    aks_nodes(c, size, nh, disk, false, "default node pool");
    if c.flag("container_insights") {
        c.note("Container Insights data lands in the linked Log Analytics workspace, priced on its line");
    }
}

fn kubernetes_node_pool(c: &mut Ctx) {
    let size = c.sku("main", "vm_size");
    let (desired, min, max) = (
        c.num("desired_nodes", 0.0),
        c.num("min_nodes", 0.0),
        c.num("max_nodes", 3.0),
    );
    let hours = c.hours();
    let nh = pool_node_hours(c, desired, min, max).min(max.max(0.0) * hours);
    let disk = c.num("disk_size_gb", AKS_OS_DISK_GB);
    let spot = c.flag("spot");
    aks_nodes(c, size, nh, disk, spot, "node pool");
}

/// "1Gi" / "512Mi" / "0.5Gi" as GiB.
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
    let cpu = c
        .arg("main", "template.container.cpu")
        .as_ref()
        .and_then(num)
        .unwrap_or(0.25);
    let mem = c
        .arg("main", "template.container.memory")
        .as_ref()
        .and_then(gib)
        .unwrap_or(0.5);
    let replicas = c.num("replicas", 1.0).max(0.0);
    let seconds = c.hours() * 3600.0 * replicas;
    let req = c.a("container_requests");
    c.charge(
        format!("{} replica(s) x {cpu} vCPU, always active", fmt(replicas)),
        "container_apps",
        "vcpu",
        cpu * seconds,
    );
    c.charge(
        format!("{} replica(s) x {mem} GiB", fmt(replicas)),
        "container_apps",
        "memory",
        mem * seconds,
    );
    c.charge(
        format!("{} requests", fmt(req)),
        "container_apps",
        "requests",
        req / 1e6,
    );
    c.note("the monthly free grant (180,000 vCPU-s, 360,000 GiB-s, 2M requests) is not deducted; idle replicas bill at a lower rate than assumed here");
}

fn function(c: &mut Ctx) {
    let plan = c.sku("plan", "sku_name").unwrap_or_else(|| "Y1".into());
    if plan == "EP1" {
        let hours = c.hours();
        c.charge(
            "Elastic Premium EP1 plan, one always-ready instance",
            "functions",
            "EP1",
            hours,
        );
        c.note("VNet integration needs the Elastic Premium plan, which bills its instances whether or not they run");
        return;
    }
    let inv = c.a("function_invocations");
    let ms = c.a("function_duration_ms");
    let mem_gb = (c.num("memory_mb", 128.0) / 1024.0).max(0.125);
    c.charge(
        format!(
            "{} executions x {ms} ms x {mem_gb} GB, Consumption plan",
            fmt(inv)
        ),
        "functions",
        "duration",
        inv * ms / 1000.0 * mem_gb,
    );
    c.charge("executions", "functions", "executions", inv / 1e6);
    c.note("the Consumption free grant (1M executions, 400,000 GB-s) and the function's storage account are not included");
}

fn relational_database(c: &mut Ctx) {
    let mysql = c.text("engine").as_deref() == Some("mysql");
    let (block, table, storage_row) = if mysql {
        ("my", "mysql_flexible", "mysql")
    } else {
        ("pg", "postgres_flexible", "postgres")
    };
    let Some(sku) = c.sku(block, "sku_name") else {
        c.note("no SKU resolved from the mapping");
        return;
    };
    let ha = c.flag("high_availability");
    let copies = if ha { 2.0 } else { 1.0 };
    let hours = c.hours();
    let label = if ha {
        format!("{sku}, zone-redundant HA (primary and standby)")
    } else {
        sku.clone()
    };
    c.charge(label, table, &sku, copies * hours);
    let gb = c.field("storage").as_ref().and_then(num).unwrap_or(32.0);
    c.charge(
        format!("{gb} GB storage{}", if ha { ", HA (x2)" } else { "" }),
        "flexible_storage",
        storage_row,
        gb * copies,
    );
    c.note("backup storage up to the server's size is free");
}

fn cache(c: &mut Ctx) {
    let sku = c.sku("main", "sku_name").unwrap_or_else(|| "Basic".into());
    let family = c.sku("main", "family").unwrap_or_else(|| "C".into());
    let cap = c.arg("main", "capacity").as_ref().and_then(num).unwrap_or(0.0);
    let row = format!("{sku}_{family}{cap}");
    let hours = c.hours();
    c.charge(format!("{sku} {family}{cap} cache"), "redis", &row, hours);
}

fn nosql_table(c: &mut Ctx) {
    let hours = c.hours();
    let gb = c.a("table_gb");
    c.charge(
        "400 RU/s provisioned (the minimum)",
        "cosmos",
        "throughput",
        4.0 * hours,
    );
    c.charge(format!("{gb} GB stored"), "cosmos", "storage", gb);
    c.note("provisioned throughput bills whether used or not; reads and writes within 400 RU/s cost nothing more");
}

fn object_storage(c: &mut Ctx) {
    let gb = c.a("bucket_gb");
    let w = c.a("bucket_writes");
    let r = c.a("bucket_reads");
    let repl = c.ptext("replication").unwrap_or_else(|| "LRS".into());
    c.charge(
        format!("{gb} GB, Hot tier, {repl}"),
        "blob",
        &format!("hot_{repl}"),
        gb,
    );
    c.charge(
        format!("{} write operations", fmt(w)),
        "blob",
        "write",
        w / 10_000.0,
    );
    c.charge(
        format!("{} read operations", fmt(r)),
        "blob",
        "read",
        r / 10_000.0,
    );
}

fn file_system(c: &mut Ctx) {
    let gb = c.num("size_gb", 1024.0).max(100.0);
    c.charge(
        format!("{gb} GiB premium share, provisioned"),
        "files",
        "premium_LRS",
        gb,
    );
    c.note("a premium share bills its provisioned size, not the data stored");
}

fn container_registry(c: &mut Ctx) {
    let sku = c.sku("main", "sku").unwrap_or_else(|| "Basic".into());
    let days = c.hours() / 24.0;
    c.charge(format!("{sku} registry"), "acr", &sku, days);
    let gb = c.a("registry_gb");
    if sku == "Basic" && gb > 10.0 {
        c.note(format!(
            "{gb} GB of images is more than the 10 GB Basic includes; the extra storage is not included"
        ));
    }
}

/// The Service Bus tier a queue or topic uses: its own namespace's, or the enclosing
/// namespace container's.
fn bus_tier(c: &Ctx, own: bool, own_default: &str) -> String {
    if own {
        return c.ptext("sku").unwrap_or_else(|| own_default.into());
    }
    c.p.ancestor_of_type(c.e.id, "servicebus_namespace")
        .and_then(|ns| ns.config.get("sku").map(|v| v.display()))
        .unwrap_or_else(|| "Standard".into())
}

fn bus_operations(c: &mut Ctx, tier: &str, ops: f64, what: &str) {
    match tier {
        "Basic" => c.charge(what.to_string(), "service_bus", "basic_operations", ops / 1e6),
        "Standard" => {
            let over = (ops - 13e6).max(0.0);
            if over > 0.0 {
                c.charge(
                    format!("{what} above the 13M included"),
                    "service_bus",
                    "standard_operations",
                    over / 1e6,
                );
            } else {
                c.note(format!(
                    "{} operations fit in the 13 million a Standard namespace includes",
                    fmt(ops)
                ));
            }
        }
        _ => c.note("Premium namespaces include operations in their messaging units"),
    }
}

fn event_queue(c: &mut Ctx) {
    let own = c.emits("ns");
    let tier = bus_tier(c, own, "Basic");
    if own && tier == "Standard" {
        let hours = c.hours();
        c.charge(
            "its own Standard namespace (base charge)",
            "service_bus",
            "standard_base",
            hours,
        );
    }
    let msgs = c.a("queue_messages");
    bus_operations(
        c,
        &tier,
        msgs * 3.0,
        &format!("{} messages x 3 operations", fmt(msgs)),
    );
}

fn topic(c: &mut Ctx) {
    let own = c.emits("ns");
    let tier = bus_tier(c, own, "Standard");
    if own {
        let hours = c.hours();
        c.charge(
            "its own Standard namespace (base charge)",
            "service_bus",
            "standard_base",
            hours,
        );
    }
    let msgs = c.a("topic_messages");
    let subs = c.list_len("subscriptions").max(1) as f64;
    bus_operations(
        c,
        &tier,
        msgs * (1.0 + subs),
        &format!(
            "{} messages, sent and delivered to {subs} subscription(s)",
            fmt(msgs)
        ),
    );
}

fn storage_queue(c: &mut Ctx) {
    let msgs = c.a("queue_messages");
    c.charge(
        format!("{} messages x 3 operations", fmt(msgs)),
        "queue_storage",
        "operations",
        msgs * 3.0 / 10_000.0,
    );
    c.charge("1 GB of queue capacity", "queue_storage", "capacity", 1.0);
}

fn servicebus_namespace(c: &mut Ctx) {
    let hours = c.hours();
    match c.text("sku").as_deref().unwrap_or("Standard") {
        "Basic" => c.note("Basic has no base charge; operations are priced on its queues"),
        "Premium" => c.charge("Premium, 1 messaging unit", "service_bus", "premium_unit", hours),
        _ => c.charge(
            "Standard base charge (13M operations included)",
            "service_bus",
            "standard_base",
            hours,
        ),
    }
}

fn nat_gateway(c: &mut Ctx) {
    let hours = c.hours();
    let gb = c.a("nat_data_gb");
    c.charge("NAT gateway hours", "nat_gateway", "hour", hours);
    c.charge(format!("{gb} GB processed"), "nat_gateway", "data", gb);
    c.charge("its public IP", "public_ip", "standard_static", hours);
}

fn load_balancer(c: &mut Ctx) {
    let hours = c.hours();
    let gb = c.a("lb_data_gb");
    c.charge(
        "Standard Load Balancer (up to 5 rules)",
        "load_balancer",
        "rules",
        hours,
    );
    c.charge(format!("{gb} GB processed"), "load_balancer", "data", gb);
    if c.emits("pip") {
        c.charge("its public IP", "public_ip", "standard_static", hours);
    }
}

fn private_endpoint(c: &mut Ctx) {
    let hours = c.hours();
    let gb = c.a("endpoint_data_gb");
    c.charge("private endpoint hours", "private_endpoint", "hour", hours);
    c.charge(format!("{gb} GB processed"), "private_endpoint", "data", gb);
}

/// Front Door: a monthly base fee by tier (Premium when the linked firewall runs managed
/// rule sets — its WAF is included), data out to viewers and requests.
fn cdn(c: &mut Ctx) {
    let premium = c.sku("profile", "sku_name").as_deref() == Some("Premium_AzureFrontDoor");
    let tier = if premium { "premium" } else { "standard" };
    let gb = c.a("cdn_data_gb");
    let req = c.a("cdn_requests");
    c.charge(
        format!(
            "Front Door {} base fee",
            if premium { "Premium" } else { "Standard" }
        ),
        "front_door",
        &format!("{tier}_base"),
        1.0,
    );
    c.charge(
        format!("{gb} GB out to viewers (zone 1)"),
        "front_door",
        &format!("{tier}_data_out"),
        gb,
    );
    c.charge(
        format!("{} requests", fmt(req)),
        "front_door",
        &format!("{tier}_requests"),
        req / 10_000.0,
    );
    c.note("data from Front Door to the origin is billed per GB as well; not estimated");
}

fn dns_zone(c: &mut Ctx) {
    let q = c.a("dns_queries");
    c.charge("DNS zone", "dns", "zone", 1.0);
    c.charge(format!("{} queries", fmt(q)), "dns", "queries", q / 1e6);
}

fn encryption_key(c: &mut Ctx) {
    let r = c.a("key_requests");
    c.charge(
        format!("{} key operations", fmt(r)),
        "key_vault",
        "operations",
        r / 10_000.0,
    );
    let days = c.num("rotation_days", 90.0).max(1.0);
    let per_month = c.hours() / 24.0 / days;
    c.charge(
        format!("automated rotation every {days} days"),
        "key_vault",
        "rotation",
        per_month,
    );
}

fn secret(c: &mut Ctx) {
    let r = c.a("secret_requests");
    c.charge(
        format!("{} secret operations", fmt(r)),
        "key_vault",
        "operations",
        r / 10_000.0,
    );
}

fn log_group(c: &mut Ctx) {
    let gb = c.a("log_ingest_gb");
    let days = c.num("retention_days", 30.0);
    c.charge(
        format!("{gb} GB ingested (Analytics logs)"),
        "log_analytics",
        "ingest",
        gb,
    );
    let extra = ((days - 31.0) / 30.0).max(0.0) * gb;
    if extra > 0.0 {
        c.charge(
            format!(
                "{} GB kept past the included 31 days ({days}-day retention)",
                fmt(extra)
            ),
            "log_analytics",
            "retention",
            extra,
        );
    }
    c.note("the first 5 GB a month per billing account are free; not deducted");
}

fn alarm(c: &mut Ctx) {
    let n = c.a("alarm_time_series");
    c.charge("metric alert rule", "monitor_alerts", "metric_alert", n);
}
