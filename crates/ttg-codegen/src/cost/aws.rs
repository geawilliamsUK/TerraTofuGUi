//! How entities turn into billable units on AWS. One function per priced abstract type;
//! the unit prices are rows of `definitions/prices/aws.toml`. Types that cost nothing or
//! are not estimated are listed there instead, with the reason.

use super::model::{fmt, num, pool_node_hours, Ctx, Model};
use ttg_core::{Relation, Value};

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
        "private_endpoint" => private_endpoint,
        "cdn" => cdn,
        "web_application_firewall" => web_application_firewall,
        "dns_zone" => dns_zone,
        "encryption_key" => encryption_key,
        "secret" => secret,
        "user_identity" => user_identity,
        "audit_trail" => audit_trail,
        "log_group" => log_group,
        "alarm" => alarm,
        _ => return Model::Missing,
    })
}

/// Default root volume of an EC2 instance from a stock Linux image.
const INSTANCE_ROOT_GB: f64 = 8.0;
/// Root volume of an EKS node when the pool sets no disk size (the AL2023 image's).
const EKS_NODE_ROOT_GB: f64 = 20.0;

fn ec2_hours(c: &mut Ctx, sku: Option<String>, count: f64, what: &str) {
    let Some(sku) = sku else {
        c.note("no instance type resolved from the mapping");
        return;
    };
    let hours = c.hours();
    let q = count * hours;
    c.charge(format!("{what}: {count} x {sku}, on demand"), "ec2", &sku, q);
    c.charge(
        format!("{what}: {count} x {INSTANCE_ROOT_GB} GB gp3 root volume"),
        "ebs",
        "gp3",
        count * INSTANCE_ROOT_GB,
    );
}

fn compute_instance(c: &mut Ctx) {
    let sku = c.sku("main", "instance_type");
    ec2_hours(c, sku, 1.0, "instance");
}

fn autoscaling_group(c: &mut Ctx) {
    let sku = c.sku("lt", "instance_type");
    let n = c.num("desired_instances", 1.0).max(c.num("min_instances", 0.0));
    c.note("priced at the desired instance count around the clock");
    ec2_hours(c, sku, n, "instances");
}

/// Node-group instances plus their root volumes, for `node_hours` in the month.
fn eks_nodes(c: &mut Ctx, sku: Option<String>, node_hours: f64, disk_gb: f64, spot: bool, what: &str) {
    let Some(sku) = sku else {
        c.note("no instance type resolved from the mapping");
        return;
    };
    let hours = c.hours();
    let avg = node_hours / hours.max(1.0);
    let label = format!("{what}: {} node-hours of {sku}", fmt(node_hours));
    if spot {
        let f = c.a("spot_price_fraction");
        c.charge(
            format!("{label} on spot (x{f} of on-demand)"),
            "ec2",
            &sku,
            node_hours * f,
        );
    } else {
        c.charge(format!("{label}, on demand"), "ec2", &sku, node_hours);
    }
    c.charge(
        format!(
            "{what}: {disk_gb} GB gp3 root volume per node (average {} nodes)",
            fmt(avg)
        ),
        "ebs",
        "gp3",
        avg * disk_gb,
    );
}

fn kubernetes_cluster(c: &mut Ctx) {
    let hours = c.hours();
    c.charge("EKS control plane", "eks", "cluster", hours);
    let sku = c.sku("nodes", "instance_types");
    let (desired, min, max) = (
        c.num("node_count", 2.0),
        c.num("min_nodes", 1.0),
        c.num("max_nodes", 3.0),
    );
    let nh = pool_node_hours(c, desired, min, max);
    let disk = c.num("disk_size_gb", EKS_NODE_ROOT_GB);
    eks_nodes(c, sku, nh, disk, false, "default node group");
    if c.flag("container_insights") {
        c.note("Container Insights metrics and logs are billed by CloudWatch and are not included");
    }
}

fn kubernetes_node_pool(c: &mut Ctx) {
    let sku = c.sku("main", "instance_types");
    let (desired, min, max) = (
        c.num("desired_nodes", 0.0),
        c.num("min_nodes", 0.0),
        c.num("max_nodes", 3.0),
    );
    let mut nh = pool_node_hours(c, desired, min, max);
    // A pool can never run more node-hours than its maximum around the clock.
    let hours = c.hours();
    nh = nh.min(max.max(0.0) * hours);
    let disk = c.num("disk_size_gb", EKS_NODE_ROOT_GB);
    let spot = c.flag("spot");
    eks_nodes(c, sku, nh, disk, spot, "node group");
}

fn container_app(c: &mut Ctx) {
    let cpu = c.arg("task", "cpu").as_ref().and_then(num).unwrap_or(256.0) / 1024.0;
    let mem = c.arg("task", "memory").as_ref().and_then(num).unwrap_or(512.0) / 1024.0;
    let replicas = c.num("replicas", 1.0).max(0.0);
    let hours = c.hours() * replicas;
    c.charge(
        format!("Fargate: {replicas} task(s) x {cpu} vCPU"),
        "fargate",
        "vcpu",
        cpu * hours,
    );
    c.charge(
        format!("Fargate: {replicas} task(s) x {mem} GB"),
        "fargate",
        "gb",
        mem * hours,
    );
}

fn function(c: &mut Ctx) {
    let inv = c.a("function_invocations");
    let ms = c.a("function_duration_ms");
    let mem_gb = c.num("memory_mb", 128.0) / 1024.0;
    c.charge(
        format!("{} invocations x {ms} ms x {mem_gb} GB", fmt(inv)),
        "lambda",
        "duration",
        inv * ms / 1000.0 * mem_gb,
    );
    c.charge("requests", "lambda", "requests", inv / 1e6);
    c.note("the Lambda free tier (1M requests, 400,000 GB-s a month) is not deducted");
}

fn relational_database(c: &mut Ctx) {
    let engine = c.text("engine").unwrap_or_else(|| "postgres".into());
    let table = if engine == "mysql" {
        "rds_mysql"
    } else {
        "rds_postgres"
    };
    let Some(class) = c.sku("main", "instance_class") else {
        c.note("no instance class resolved from the mapping");
        return;
    };
    let ha = c.flag("high_availability");
    let copies = if ha { 2.0 } else { 1.0 };
    let hours = c.hours();
    let label = if ha {
        format!("{class}, Multi-AZ (primary and standby)")
    } else {
        format!("{class}, single-AZ")
    };
    c.charge(label, table, &class, copies * hours);
    if ha {
        c.note("a Multi-AZ instance lists at twice single-AZ or a little more; twice is used");
    }
    let gb = c.field("storage").as_ref().and_then(num).unwrap_or(32.0);
    let st = c.sku("main", "storage_type").unwrap_or_else(|| "gp3".into());
    c.charge(
        format!("{gb} GB {st} storage{}", if ha { ", Multi-AZ (x2)" } else { "" }),
        "rds_storage",
        &st,
        gb * copies,
    );
    if matches!(c.pfield("performance_insights"), Some(Value::Bool(true))) {
        c.note("Performance Insights: the 7-day retention it is created with is free");
    }
    c.note("backup storage up to the database's size is free; more retention or manual snapshots are not included");
}

fn cache(c: &mut Ctx) {
    let Some(node) = c.sku("main", "node_type") else {
        c.note("no node type resolved from the mapping");
        return;
    };
    let n = c
        .arg("main", "num_cache_nodes")
        .as_ref()
        .and_then(num)
        .unwrap_or(1.0);
    let hours = c.hours();
    c.charge(format!("{n} x {node}"), "elasticache", &node, n * hours);
}

fn nosql_table(c: &mut Ctx) {
    let r = c.a("table_reads");
    let w = c.a("table_writes");
    let gb = c.a("table_gb");
    c.charge(
        format!("{} writes, on demand", fmt(w)),
        "dynamodb",
        "write",
        w / 1e6,
    );
    c.charge(
        format!("{} reads, on demand", fmt(r)),
        "dynamodb",
        "read",
        r / 1e6,
    );
    c.charge(format!("{gb} GB stored"), "dynamodb", "storage", gb);
}

fn object_storage(c: &mut Ctx) {
    let gb = c.a("bucket_gb");
    let w = c.a("bucket_writes");
    let r = c.a("bucket_reads");
    c.charge(format!("{gb} GB in S3 Standard"), "s3", "standard", gb);
    c.charge(format!("{} PUT / LIST requests", fmt(w)), "s3", "put", w / 1000.0);
    c.charge(format!("{} GET requests", fmt(r)), "s3", "get", r / 1000.0);
    if c.flag("versioning") {
        c.note(
            "versioning keeps old versions, which are billed as storage too; bucket_gb should include them",
        );
    }
}

fn file_system(c: &mut Ctx) {
    let gb = c.a("file_system_gb");
    c.charge(format!("{gb} GB in EFS Standard"), "efs", "standard", gb);
    c.note("EFS bills the data stored, not the size field; throughput charges are not included");
}

fn container_registry(c: &mut Ctx) {
    let gb = c.a("registry_gb");
    let repos = c.list_len("repositories").max(1);
    c.charge(
        format!(
            "{gb} GB of images across {repos} repositor{}",
            if repos == 1 { "y" } else { "ies" }
        ),
        "ecr",
        "storage",
        gb,
    );
}

fn event_queue(c: &mut Ctx) {
    let msgs = c.a("queue_messages");
    let fifo = matches!(c.pfield("fifo"), Some(Value::Bool(true)));
    c.charge(
        format!("{} messages x 3 requests (send, receive, delete)", fmt(msgs)),
        "sqs",
        if fifo { "fifo" } else { "standard" },
        msgs * 3.0 / 1e6,
    );
    c.note("the first million SQS requests a month (per account) are free; not deducted");
}

fn topic(c: &mut Ctx) {
    let msgs = c.a("topic_messages");
    c.charge(format!("{} publishes", fmt(msgs)), "sns", "publish", msgs / 1e6);
    let email = c
        .field("subscriptions")
        .as_ref()
        .and_then(|v| v.as_records())
        .map(|rows| {
            rows.iter()
                .filter(|r| r.get("protocol").is_some_and(|p| p.display() == "email"))
                .count()
        })
        .unwrap_or(0);
    if email > 0 {
        c.charge(
            format!(
                "{} email deliveries ({email} subscriber(s))",
                fmt(msgs * email as f64)
            ),
            "sns",
            "email",
            msgs * email as f64 / 100_000.0,
        );
    }
}

fn nat_gateway(c: &mut Ctx) {
    let hours = c.hours();
    let gb = c.a("nat_data_gb");
    c.charge("NAT gateway hours", "nat_gateway", "hour", hours);
    c.charge(format!("{gb} GB processed"), "nat_gateway", "data", gb);
    c.charge("its Elastic IP (public IPv4)", "public_ipv4", "address", hours);
}

fn load_balancer(c: &mut Ctx) {
    let hours = c.hours();
    let lcu = c.a("lb_capacity_units");
    let network = c.sku("main", "load_balancer_type").as_deref() == Some("network");
    if network {
        c.charge("Network Load Balancer hours", "elb", "nlb", hours);
        c.charge(format!("{lcu} NLCU on average"), "elb", "nlb_nlcu", lcu * hours);
    } else {
        c.charge("Application Load Balancer hours", "elb", "alb", hours);
        c.charge(format!("{lcu} LCU on average"), "elb", "alb_lcu", lcu * hours);
    }
    if c.text("scheme").as_deref() != Some("internal") {
        let zones = c
            .targets(Relation::NetworkMembership, Some("subnet"))
            .len()
            .max(2) as f64;
        c.charge(
            format!("{zones} public IPv4 addresses (one per zone)"),
            "public_ipv4",
            "address",
            zones * hours,
        );
    }
}

fn private_endpoint(c: &mut Ctx) {
    let service = c.ptext("service").unwrap_or_default();
    if service == "s3" || service == "dynamodb" {
        c.note(format!("a Gateway endpoint ({service}): no charge"));
        return;
    }
    let hours = c.hours();
    let zones = c
        .targets(Relation::NetworkMembership, Some("subnet"))
        .len()
        .max(1) as f64;
    let gb = c.a("endpoint_data_gb");
    c.charge(
        format!("Interface endpoint ({service}) in {zones} zone(s)"),
        "vpc_endpoint",
        "interface",
        zones * hours,
    );
    c.charge(format!("{gb} GB processed"), "vpc_endpoint", "data", gb);
}

fn cdn(c: &mut Ctx) {
    let gb = c.a("cdn_data_gb");
    let req = c.a("cdn_requests");
    c.charge(format!("{gb} GB out to viewers"), "cloudfront", "data_out", gb);
    c.charge(
        format!("{} HTTPS requests", fmt(req)),
        "cloudfront",
        "https",
        req / 10_000.0,
    );
    c.note("the CloudFront free tier (1 TB and 10M requests a month) is not deducted");
}

fn web_application_firewall(c: &mut Ctx) {
    let acls = if c.emits("global") { 2.0 } else { 1.0 };
    let rules = c.list_len("managed_rules") as f64
        + if c.num("rate_limit_per_5min", 0.0) > 0.0 {
            1.0
        } else {
            0.0
        };
    let req = c.a("waf_requests");
    let what = if acls > 1.0 {
        "2 web ACLs (regional, and CloudFront-scoped for the CDN)"
    } else {
        "web ACL"
    };
    c.charge(what, "wafv2", "web_acl", acls);
    c.charge(format!("{rules} rule(s) in each"), "wafv2", "rule", rules * acls);
    c.charge(
        format!("{} requests inspected", fmt(req)),
        "wafv2",
        "requests",
        req / 1e6,
    );
}

fn dns_zone(c: &mut Ctx) {
    let q = c.a("dns_queries");
    c.charge("hosted zone", "route53", "hosted_zone", 1.0);
    c.charge(format!("{} queries", fmt(q)), "route53", "queries", q / 1e6);
}

fn encryption_key(c: &mut Ctx) {
    let r = c.a("key_requests");
    c.charge("customer managed key", "kms", "key", 1.0);
    c.charge(format!("{} requests", fmt(r)), "kms", "requests", r / 10_000.0);
    c.note("each automatic rotation adds a key version, and AWS bills the first two rotations of a key at $1/month each; not included");
}

fn secret(c: &mut Ctx) {
    let r = c.a("secret_requests");
    c.charge("secret", "secrets_manager", "secret", 1.0);
    c.charge(
        format!("{} API calls", fmt(r)),
        "secrets_manager",
        "api",
        r / 10_000.0,
    );
}

fn user_identity(c: &mut Ctx) {
    let mau = c.a("monthly_active_users");
    let billable = (mau - 10_000.0).max(0.0);
    c.charge(
        format!("{} monthly active users, the first 10,000 free", fmt(mau)),
        "cognito",
        "essentials_mau",
        billable,
    );
}

fn audit_trail(c: &mut Ctx) {
    if !c.flag("data_events") {
        c.note(
            "management events only: the first copy is free (the trail's S3 storage is priced on the bucket)",
        );
        return;
    }
    let n = c.a("audit_data_events");
    c.charge(
        format!("{} data events", fmt(n)),
        "cloudtrail",
        "data_events",
        n / 100_000.0,
    );
}

fn log_group(c: &mut Ctx) {
    let gb = c.a("log_ingest_gb");
    let days = c.num("retention_days", 30.0);
    c.charge(format!("{gb} GB ingested"), "cloudwatch_logs", "ingest", gb);
    // Stored volume settles at what the retention keeps; compressed storage is smaller
    // than ingested, which this ignores in the cautious direction.
    let stored = gb * days / 30.0;
    c.charge(
        format!("{} GB stored ({days}-day retention)", fmt(stored)),
        "cloudwatch_logs",
        "storage",
        stored,
    );
}

fn alarm(c: &mut Ctx) {
    let n = c.a("alarm_time_series");
    c.charge("standard metric alarm", "cloudwatch_alarms", "standard", n);
}
