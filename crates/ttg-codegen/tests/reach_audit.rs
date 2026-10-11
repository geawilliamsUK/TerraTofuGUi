//! Reachability completed — file systems, endpoints that cover a whole service, load
//! balancers and the internet as sources — the audit of intent against the network, and
//! the posture rules.

use std::path::Path as FsPath;
use ttg_catalog::Catalog;
use ttg_codegen::reach::{analyse, audit, paths_from, paths_to, Finding, Path, Status, INTERNET};
use ttg_codegen::{Code, Severity};
use ttg_core::{Project, Relation, Value};

fn example(name: &str) -> Project {
    let p = FsPath::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name);
    ttg_core::project::load(&p).expect("example loads")
}

fn to(p: &Project, cat: &Catalog, provider: &str, target: &str, source: &str) -> Option<Path> {
    let r = analyse(p, cat, provider);
    paths_to(p, cat, &r, target)
        .into_iter()
        .find(|(s, _)| s == source)
        .map(|(_, path)| path)
}

fn from(p: &Project, cat: &Catalog, provider: &str, source: &str, target: &str) -> Path {
    let r = analyse(p, cat, provider);
    paths_from(p, cat, &r, source)
        .into_iter()
        .find(|x| x.target == target)
        .unwrap_or_else(|| panic!("no path from {source} to {target}"))
}

fn node(json: serde_json::Value) -> ttg_core::Node {
    serde_json::from_value(json).unwrap()
}

fn findings(p: &Project, cat: &Catalog) -> Vec<Finding> {
    audit(p, cat, &analyse(p, cat, "aws"))
}

fn names(p: &Project, ids: &[String]) -> Vec<String> {
    ids.iter()
        .map(|h| ttg_codegen::reach::source_name(p, h))
        .collect()
}

/// A file system answers on 2049 through a mount target per linked subnet, behind its
/// own security group; "Mounts" is the link that hands a workload the volume.
#[test]
fn file_system_paths() {
    let cat = Catalog::builtin();
    let p = example("kubernetes.ttg.json");
    let r = analyse(&p, &cat, "aws");
    let to_fs = paths_to(&p, &cat, &r, "fs-models");
    assert!(
        ["wl-api", "wl-asr", "wl-report", "k8s-platform"]
            .iter()
            .all(|s| to_fs.iter().any(|(x, _)| x == s)),
        "{to_fs:?}"
    );
    // The asr worker mounts it: allowed by the file system's group, through the mount
    // target in its own zone, and no note about a missing link.
    let asr = to(&p, &cat, "aws", "fs-models", "wl-asr").unwrap();
    assert_eq!(asr.status, Status::Ok, "{}", asr.reason);
    assert_eq!(names(&p, &asr.hops), ["nodes a", "fs sg"]);
    assert!(asr.notes.is_empty(), "{:?}", asr.notes);
    // The api could, but nothing mounts it there.
    let api = to(&p, &cat, "aws", "fs-models", "wl-api").unwrap();
    assert_eq!(api.status, Status::Ok);
    assert!(
        api.notes
            .iter()
            .any(|n| n.contains("no 'Mounts' link from \"api\", so nothing mounts it")),
        "{:?}",
        api.notes
    );

    // One mount target only (zone a): clients in zone b cannot mount it by name.
    let mut one_zone = p.clone();
    one_zone
        .edges
        .retain(|e| !(e.source == "fs-models" && e.target == "subnet-nodes-b"));
    let asr = to(&one_zone, &cat, "aws", "fs-models", "wl-asr").unwrap();
    assert!(
        asr.notes
            .iter()
            .any(|n| n.contains("has no mount target in eu-west-2b")),
        "{:?}",
        asr.notes
    );

    // No subnet at all: no mount target, so nothing reaches it.
    let mut none = p.clone();
    none.edges
        .retain(|e| !(e.source == "fs-models" && e.relation == Relation::NetworkMembership));
    let asr = to(&none, &cat, "aws", "fs-models", "wl-asr").unwrap();
    assert_eq!(asr.status, Status::Blocked);
    assert!(asr.reason.contains("has no mount target"), "{}", asr.reason);

    // The file system's group admits the cluster's own group only; a group of its own
    // on the GPU pool does not stop that, but a rule naming nothing the nodes carry does.
    let mut closed = p.clone();
    if let Some(Value::Records(rows)) = closed.nodes.get_mut("sg-fs").unwrap().config.get_mut("rules") {
        rows[0].insert("source_group".into(), Value::Str("sg-db".into()));
    }
    let asr = to(&closed, &cat, "aws", "fs-models", "wl-asr").unwrap();
    assert_eq!(asr.status, Status::Blocked);
    assert!(
        asr.reason
            .contains("\"fs sg\" has no ingress rule allowing port 2049 from \"asr worker\""),
        "{}",
        asr.reason
    );
}

/// An SQS interface endpoint in the nodes' subnets.
fn with_sqs_endpoint(mut p: Project, admits: &str) -> Project {
    p.nodes.insert(
        "sg-endpoints".into(),
        node(serde_json::json!({
            "id": "sg-endpoints", "name": "endpoints sg", "resource_type": "security_group",
            "parent": "vnet-core",
            "config": { "rules": [ { "name": "https-from-nodes", "direction": "ingress", "protocol": "tcp",
                "from_port": 443, "to_port": 443, "cidr": "", "source_group": admits } ] }
        })),
    );
    p.nodes.insert(
        "pe-sqs".into(),
        node(serde_json::json!({
            "id": "pe-sqs", "name": "sqs endpoint", "resource_type": "private_endpoint",
            "parent": "vnet-core", "config": { "subresource": "namespace" },
            "provider_config": { "aws": { "service": "sqs" } }
        })),
    );
    p.add_edge("pe-sqs", "subnet-nodes-a", Relation::NetworkMembership);
    p.add_edge("pe-sqs", "subnet-nodes-b", Relation::NetworkMembership);
    p.add_edge("pe-sqs", "sg-endpoints", Relation::Attachment);
    p
}

/// An AWS interface endpoint answers for every resource of its service, whatever it
/// "Connects to"; its own group decides who may use it. Traffic that leaves through NAT
/// for a service the network has an endpoint for is reported.
#[test]
fn endpoints_cover_their_whole_service() {
    let cat = Catalog::builtin();
    let p = example("kubernetes.ttg.json");

    // No endpoint: the queue is reached over the API, through the NAT.
    let api = from(&p, &cat, "aws", "wl-api", "q-jobs");
    assert_eq!(api.status, Status::Ok);
    assert!(names(&p, &api.hops).contains(&"egress nat".to_string()));

    // An SQS endpoint that links nothing still carries the api's calls to the queue.
    let pe = with_sqs_endpoint(p.clone(), "sg-nodes");
    let api = from(&pe, &cat, "aws", "wl-api", "q-jobs");
    assert_eq!(api.status, Status::Ok, "{}", api.reason);
    assert_eq!(api.reason, "over private endpoint \"sqs endpoint\"");
    assert_eq!(names(&pe, &api.hops), ["nodes a", "sqs endpoint", "endpoints sg"]);

    // Its group admits nothing the workloads carry: private DNS sends them there anyway.
    let refused = with_sqs_endpoint(p.clone(), "sg-db");
    let api = from(&refused, &cat, "aws", "wl-api", "q-jobs");
    assert_eq!(api.status, Status::Blocked);
    assert!(
        api.reason
            .contains("answers for sqs in this network (private DNS)")
            && api
                .reason
                .contains("no ingress rule allowing port 443 from \"api\""),
        "{}",
        api.reason
    );
    let f = findings(&refused, &cat);
    assert!(
        f.iter().any(|x| x.kind == "blocked"
            && x.severity == Severity::Error
            && x.source == "wl-api"
            && x.target == "q-jobs"),
        "{f:#?}"
    );

    // An S3 gateway endpoint on the public route table only: the nodes' calls to the
    // bucket still leave through the NAT, which the path and the audit both point out.
    let mut gw = p.clone();
    gw.nodes.insert(
        "pe-s3".into(),
        node(serde_json::json!({
            "id": "pe-s3", "name": "s3 endpoint", "resource_type": "private_endpoint",
            "parent": "vnet-core", "config": { "subresource": "blob" },
            "provider_config": { "aws": { "service": "s3" } }
        })),
    );
    gw.add_edge("pe-s3", "rt-public", Relation::Attachment);
    let api = from(&gw, &cat, "aws", "wl-api", "obj-transcripts");
    assert_eq!(api.status, Status::Ok);
    assert!(
        api.notes
            .iter()
            .any(|n| n.starts_with("leaves through NAT \"egress nat\"")
                && n.contains("\"s3 endpoint\"")
                && n.contains("gateway endpoint for s3")),
        "{:?}",
        api.notes
    );
    let f = findings(&gw, &cat);
    let nat: Vec<&Finding> = f.iter().filter(|x| x.kind == "nat_egress").collect();
    assert!(
        nat.iter()
            .any(|x| x.severity == Severity::Info && x.source == "wl-api" && x.target == "obj-transcripts"),
        "{f:#?}"
    );
    // On the nodes' route table it is used instead.
    gw.edges
        .retain(|e| !(e.source == "pe-s3" && e.target == "rt-public"));
    gw.add_edge("pe-s3", "rt-private", Relation::Attachment);
    let api = from(&gw, &cat, "aws", "wl-api", "obj-transcripts");
    assert_eq!(api.reason, "over private endpoint \"s3 endpoint\"");
    assert!(!findings(&gw, &cat).iter().any(|x| x.kind == "nat_egress"));
}

/// A load balancer forwarding to a cluster reaches the workloads that receive its
/// traffic on their port, one forwarding to a container app reaches it on the app's
/// port, and the internet reaches whatever sits behind an internet-facing one.
#[test]
fn load_balancers_and_the_internet_are_sources() {
    let cat = Catalog::builtin();
    let p = example("kubernetes.ttg.json");
    let lb = to(&p, &cat, "aws", "wl-api", "lb-edge").expect("the load balancer reaches the api");
    assert!(
        lb.reason.starts_with("forwards to it on port 8080"),
        "{}",
        lb.reason
    );
    let internet = to(&p, &cat, "aws", "wl-api", INTERNET).expect("the internet reaches the api");
    assert!(
        internet
            .reason
            .starts_with("through internet-facing load balancer \"edge\": forwards to it on port 8080"),
        "{}",
        internet.reason
    );
    assert_eq!(internet.hops.first().map(String::as_str), Some("lb-edge"));
    assert_eq!(internet.status, lb.status);
    // The reporting workload does not receive the load balancer's traffic.
    assert!(to(&p, &cat, "aws", "wl-report", "lb-edge").is_none());
    assert!(to(&p, &cat, "aws", "wl-report", INTERNET).is_none());

    // Nothing says which workload receives it: the nodes are reached, the workloads are
    // undecided.
    let mut unmarked = p.clone();
    unmarked
        .edges
        .retain(|e| !(e.source == "wl-api" && e.target == "lb-edge"));
    let report = to(&unmarked, &cat, "aws", "wl-report", "lb-edge").expect("an assumed receiver");
    assert_ne!(report.status, Status::Ok);
    assert!(
        report.reason.contains("forwards to the nodes of \"platform\"")
            || report
                .reason
                .contains("nothing says \"reporting\" receives that traffic"),
        "{}",
        report.reason
    );
    assert!(to(&unmarked, &cat, "aws", "k8s-platform", "lb-edge").is_some());

    // A container app behind the load balancer, on the app's port (target type ip).
    let shop = example("containers.ttg.json");
    let web = to(&shop, &cat, "aws", "app-web", "lb-shop").expect("the load balancer reaches web");
    assert_eq!(web.status, Status::Ok, "{}", web.reason);
    assert!(
        web.reason.starts_with(
            "forwards to it on port 8080: allowed by \"web sg\": rule from security group \"lb sg\""
        ),
        "{}",
        web.reason
    );
    let internet = to(&shop, &cat, "aws", "app-web", INTERNET).expect("the internet reaches web");
    assert_eq!(internet.status, Status::Ok);
    assert_eq!(names(&shop, &internet.hops)[0], "shop lb");
    let lb = to(&shop, &cat, "aws", "lb-shop", INTERNET).unwrap();
    assert_eq!(lb.reason, "through its internet-facing load balancer");
    // The worker is behind nothing.
    assert!(to(&shop, &cat, "aws", "app-worker", INTERNET).is_none());

    // A CDN in front: the internet comes through it, to a bucket and to a load balancer.
    let mut edge = example("edge.ttg.json");
    let assets = to(&edge, &cat, "aws", "obj-assets", INTERNET).expect("the CDN serves the bucket");
    assert_eq!(names(&edge, &assets.hops), ["assets cdn"]);
    edge.add_edge("cdn-assets", "lb-web", Relation::AttributeReference);
    let lb = to(&edge, &cat, "aws", "lb-web", INTERNET).unwrap();
    assert!(
        lb.reason.starts_with("through CDN \"assets cdn\""),
        "{}",
        lb.reason
    );
    assert_eq!(lb.notes, ["also reachable directly, around the CDN"]);
    let vm = to(&edge, &cat, "aws", "vm-web", INTERNET).unwrap();
    assert_eq!(names(&edge, &vm.hops)[..2], ["assets cdn", "web lb"]);
}

/// The audit: an intended pair the network blocks is an error, sensitive data reachable
/// with no intent a warning, and intent drawn only as a flow a note.
#[test]
fn audit_compares_intent_with_the_network() {
    let cat = Catalog::builtin();
    let p = example("kubernetes.ttg.json");
    let f = findings(&p, &cat);
    // The load balancer forwards to the api, which says it receives that traffic, and the
    // nodes' group admits the load balancer's group on the target port: nothing the
    // design intends is blocked.
    let errors: Vec<&Finding> = f.iter().filter(|x| x.severity == Severity::Error).collect();
    assert!(errors.is_empty(), "{errors:#?}");

    // Take that rule away and the intended path is the audit's one error.
    let mut closed = p.clone();
    if let Some(Value::Records(rows)) = closed
        .nodes
        .get_mut("sg-nodes")
        .and_then(|n| n.config.get_mut("rules"))
    {
        rows.retain(|r| r.get("name").and_then(|v| v.as_str()) != Some("from-edge"));
    }
    let closed = findings(&closed, &cat);
    let errors: Vec<&Finding> = closed.iter().filter(|x| x.severity == Severity::Error).collect();
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert_eq!(
        (errors[0].source.as_str(), errors[0].target.as_str()),
        ("lb-edge", "wl-api")
    );
    assert!(
        errors[0]
            .message
            .contains("is intended (forwards to it) but blocked")
            && errors[0]
                .message
                .contains("\"nodes sg\" has no ingress rule allowing port 8080 from \"edge\""),
        "{}",
        errors[0].message
    );
    // The database admits the nodes' group, so every workload on them can reach it; only
    // the api is meant to.
    let open = f
        .iter()
        .find(|x| x.kind == "unintended" && x.target == "db-core")
        .expect("the database is open to workloads that do not use it");
    assert_eq!(open.severity, Severity::Warning);
    assert!(open.sources.contains(&"wl-report".to_string()));
    assert!(!open.sources.contains(&"wl-api".to_string()));
    assert!(
        open.message
            .contains("\"core db\" (an unclassified database) can be reached on port 5432"),
        "{}",
        open.message
    );

    // Take the nodes' group out of the database's rule: the api's link is blocked.
    let mut closed = p.clone();
    if let Some(Value::Records(rows)) = closed.nodes.get_mut("sg-db").unwrap().config.get_mut("rules") {
        rows[0].insert("source_group".into(), Value::Str("sg-fs".into()));
    }
    let f = findings(&closed, &cat);
    let blocked = f
        .iter()
        .find(|x| x.kind == "blocked" && x.source == "wl-api" && x.target == "db-core")
        .expect("api uses the database but the network blocks it");
    assert_eq!(blocked.severity, Severity::Error);
    assert!(
        blocked
            .message
            .contains("\"api\" → \"core db\" is intended (link 'Uses')")
            && blocked
                .message
                .contains("\"db sg\" has no ingress rule allowing port 5432 from \"api\""),
        "{}",
        blocked.message
    );
    assert_eq!(f[0].severity, Severity::Error, "errors come first");

    // Intent drawn only as a flow (no link): the network allows it, but the export hands
    // the reporting workload no host name.
    let mut flow = p.clone();
    let mut v = ttg_core::View::new("Data flow", Default::default());
    v.flows.push(
        serde_json::from_value(serde_json::json!({
            "id": "f1", "from": { "entity": "db-core" }, "to": { "entity": "wl-report" }, "label": "reports"
        }))
        .unwrap(),
    );
    flow.views.push(v);
    let f = findings(&flow, &cat);
    let unlinked = f
        .iter()
        .find(|x| x.kind == "unlinked" && x.source == "wl-report" && x.target == "db-core")
        .expect("a flow from the database to its reader is intent from the reader");
    assert_eq!(unlinked.severity, Severity::Info);
    assert!(
        unlinked
            .message
            .contains("flow \"reports\" in view \"Data flow\"")
            && unlinked.message.contains("receives no host name or credentials"),
        "{}",
        unlinked.message
    );
    // ... and the reporting workload is no longer an unintended source.
    let open = f
        .iter()
        .find(|x| x.kind == "unintended" && x.target == "db-core")
        .unwrap();
    assert!(!open.sources.contains(&"wl-report".to_string()));

    // A database classified public is not sensitive.
    let mut public = p.clone();
    public.nodes.get_mut("db-core").unwrap().classification = Some(ttg_core::Classification::Public);
    assert!(!findings(&public, &cat)
        .iter()
        .any(|x| x.kind == "unintended" && x.target == "db-core"));

    // Intent and the filters: only intended pairs from the api.
    let intents = ttg_codegen::reach::intents(&p, &cat, "aws");
    let api: Vec<&String> = intents
        .keys()
        .filter(|(s, _)| s == "wl-api")
        .map(|(_, t)| t)
        .collect();
    for t in [
        "db-core",
        "q-jobs",
        "obj-transcripts",
        "wl-asr",
        "reg-images",
        "sec-api-key",
    ] {
        assert!(api.contains(&&t.to_string()), "{t} not intended: {api:?}");
    }
    assert!(
        !api.contains(&&"k8s-platform".to_string()),
        "where it runs is not intent"
    );
    assert!(intents.contains_key(&("lb-edge".to_string(), "wl-api".to_string())));
}

fn posture(p: &Project, cat: &Catalog, provider: &str) -> Vec<(String, String, Severity, String)> {
    ttg_codegen::diagnostics::run(p, cat, provider)
        .into_iter()
        .filter(|d| d.code == Code::Posture)
        .map(|d| {
            (
                ttg_codegen::posture::rule_of(&d)
                    .expect("names its rule")
                    .to_string(),
                d.entity.clone().unwrap_or_default(),
                d.severity,
                d.message,
            )
        })
        .collect()
}

fn fired(p: &Project, cat: &Catalog, rule: &str, entity: &str) -> Option<Severity> {
    posture(p, cat, "aws")
        .into_iter()
        .find(|(r, e, _, _)| r == rule && e == entity)
        .map(|(_, _, s, _)| s)
}

/// Each posture rule fires on what it describes, names itself, never blocks the export,
/// and stays quiet once the project turns it off.
#[test]
fn posture_rules_and_toggles() {
    let cat = Catalog::builtin();
    let mut p = example("kubernetes.ttg.json");
    for rule in ttg_codegen::posture::RULES {
        assert!(ttg_codegen::posture::rule(rule.id).is_some());
    }

    // Public API with no allow-list, on every provider.
    p.nodes
        .get_mut("k8s-platform")
        .unwrap()
        .config
        .remove("public_access_cidrs");
    for provider in ["aws", "azure", "gcp"] {
        assert!(
            posture(&p, &cat, provider)
                .iter()
                .any(|(r, e, s, m)| r == "public_kubernetes_api"
                    && e == "k8s-platform"
                    && *s == Severity::Warning
                    && m.ends_with("(posture rule 'public_kubernetes_api')")),
            "{provider}"
        );
    }

    // SSH open to the world; 443 from the world is fine.
    let rule = |name: &str, dir: &str, proto: &str, from: i64, to: i64, cidr: &str| {
        let mut r = ttg_core::Record::new();
        for (k, v) in [
            ("name", name),
            ("direction", dir),
            ("protocol", proto),
            ("cidr", cidr),
        ] {
            r.insert(k.into(), Value::Str(v.into()));
        }
        r.insert("from_port".into(), Value::Int(from));
        r.insert("to_port".into(), Value::Int(to));
        r
    };
    if let Some(Value::Records(rows)) = p.nodes.get_mut("sg-nodes").unwrap().config.get_mut("rules") {
        rows.push(rule("ssh", "ingress", "tcp", 22, 22, "0.0.0.0/0"));
        rows.push(rule("https", "ingress", "tcp", 443, 443, "0.0.0.0/0"));
    }
    let open: Vec<String> = posture(&p, &cat, "aws")
        .into_iter()
        .filter(|(r, _, _, _)| r == "open_ingress")
        .map(|(_, _, _, m)| m)
        .collect();
    assert_eq!(open.len(), 1, "{open:?}");
    assert!(
        open[0].contains("rule \"ssh\" admits the whole internet (0.0.0.0/0) on port 22"),
        "{}",
        open[0]
    );

    // A data-tier group that lets everything out.
    if let Some(Value::Records(rows)) = p.nodes.get_mut("sg-db").unwrap().config.get_mut("rules") {
        rows.push(rule("all-out", "egress", "all", 0, 0, "0.0.0.0/0"));
    }
    assert_eq!(
        fired(&p, &cat, "data_tier_open_egress", "sg-db"),
        Some(Severity::Info)
    );
    assert_eq!(fired(&p, &cat, "data_tier_open_egress", "sg-nodes"), None);

    // A group nothing uses.
    p.nodes.insert(
        "sg-spare".into(),
        node(serde_json::json!({
            "id": "sg-spare", "name": "spare sg", "resource_type": "security_group", "parent": "vnet-core",
            "config": { "rules": [] }
        })),
    );
    assert_eq!(
        fired(&p, &cat, "orphan_security_group", "sg-spare"),
        Some(Severity::Info)
    );
    assert_eq!(fired(&p, &cat, "orphan_security_group", "sg-fs"), None);

    // A queue with no dead-letter queue (info); with one nobody watches (warning); with
    // an alarm on it (quiet).
    assert_eq!(
        fired(&p, &cat, "queue_dead_letter_alarm", "q-jobs"),
        Some(Severity::Info)
    );
    let mut dlq = p.nodes["q-jobs"].clone();
    dlq.id = "q-dead".into();
    dlq.name = "dead jobs".into();
    p.nodes.insert(dlq.id.clone(), dlq);
    p.add_edge("q-jobs", "q-dead", Relation::DeadLettersTo);
    assert_eq!(
        fired(&p, &cat, "queue_dead_letter_alarm", "q-jobs"),
        Some(Severity::Warning)
    );
    assert_eq!(fired(&p, &cat, "queue_dead_letter_alarm", "q-dead"), None);
    p.nodes.insert(
        "alm-dead".into(),
        node(serde_json::json!({
            "id": "alm-dead", "name": "dead letters", "resource_type": "alarm", "parent": "rg-k8s",
            "config": { "metric": "queue_depth", "threshold": 1 }
        })),
    );
    p.add_edge("alm-dead", "q-dead", Relation::AttributeReference);
    assert_eq!(fired(&p, &cat, "queue_dead_letter_alarm", "q-jobs"), None);

    // A database without backups.
    assert_eq!(fired(&p, &cat, "database_backups", "db-core"), None);
    p.nodes
        .get_mut("db-core")
        .unwrap()
        .config
        .insert("backup_retention_days".into(), Value::Int(0));
    assert_eq!(
        fired(&p, &cat, "database_backups", "db-core"),
        Some(Severity::Warning)
    );

    // Personal data with no customer-managed key; linking one quiets it.
    p.nodes.get_mut("obj-transcripts").unwrap().classification = Some(ttg_core::Classification::Personal);
    assert_eq!(
        fired(&p, &cat, "classified_without_cmk", "obj-transcripts"),
        Some(Severity::Warning)
    );
    let mut keyed = p.clone();
    keyed.add_edge("obj-transcripts", "key-nodes", Relation::EncryptedWith);
    assert_eq!(
        fired(&keyed, &cat, "classified_without_cmk", "obj-transcripts"),
        None
    );

    // None of it blocks the export.
    assert!(ttg_codegen::diagnostics::run(&p, &cat, "aws")
        .iter()
        .all(|d| d.code != Code::Posture || d.severity != Severity::Error));

    // Turned off per project, by id.
    p.settings.posture.disabled = vec!["open_ingress".into(), "database_backups".into()];
    let left: Vec<String> = posture(&p, &cat, "aws")
        .into_iter()
        .map(|(r, _, _, _)| r)
        .collect();
    assert!(
        !left
            .iter()
            .any(|r| r == "open_ingress" || r == "database_backups"),
        "{left:?}"
    );
    assert!(left.iter().any(|r| r == "public_kubernetes_api"));
    // The setting survives a save.
    let json = serde_json::to_string(&p).unwrap();
    assert!(json.contains("\"posture\":{\"disabled\":[\"open_ingress\",\"database_backups\"]}"));
    let back: Project = serde_json::from_str(&json).unwrap();
    assert_eq!(back.settings.posture, p.settings.posture);
}

/// With named environments, the audit and the posture rules run on each environment's
/// own values: what only one environment does is reported for it, tagged.
#[test]
fn audit_and_posture_run_per_environment() {
    let cat = Catalog::builtin();
    let mut p = example("kubernetes.ttg.json");
    p.settings.environments = vec!["pilot".into(), "prod".into()];
    // In prod the database group admits the file system's group instead of the nodes',
    // and the database keeps no backups.
    let mut rules = p.nodes["sg-db"].config["rules"].clone();
    if let Value::Records(rows) = &mut rules {
        rows[0].insert("source_group".into(), Value::Str("sg-fs".into()));
    }
    let prod = p
        .nodes
        .get_mut("sg-db")
        .unwrap()
        .overrides
        .entry("prod".into())
        .or_default();
    prod.config.insert("rules".into(), rules);
    p.nodes
        .get_mut("db-core")
        .unwrap()
        .overrides
        .entry("prod".into())
        .or_default()
        .config
        .insert("backup_retention_days".into(), Value::Int(0));

    // The base values: the api reaches the database.
    let base = findings(&p.resolved(None), &cat);
    assert!(!base.iter().any(|x| x.kind == "blocked" && x.target == "db-core"));
    // Prod alone blocks it, tagged with its name; pilot adds nothing.
    let others = ttg_codegen::reach::audit_other_environments(&p, &cat, "aws", None);
    let blocked: Vec<&(String, Finding)> = others
        .iter()
        .filter(|(_, f)| f.kind == "blocked" && f.source == "wl-api" && f.target == "db-core")
        .collect();
    assert_eq!(blocked.len(), 1, "{others:#?}");
    assert_eq!(blocked[0].0, "prod");
    assert!(
        blocked[0].1.message.starts_with("[prod] \"api\" → \"core db\""),
        "{}",
        blocked[0].1.message
    );
    assert!(others.iter().all(|(env, _)| env == "prod"), "{others:#?}");
    // Audited as prod, it is a finding of the audit itself.
    let shown = findings(&p.for_environment("prod"), &cat);
    assert!(shown.iter().any(|x| x.kind == "blocked" && x.target == "db-core"));

    // Posture follows the same environments, through the diagnostics.
    let envs = ttg_codegen::environments::other_environments(&p, &cat, "aws", ttg_core::Tool::OpenTofu, None);
    assert!(
        envs.iter().any(|d| d.environment.as_deref() == Some("prod")
            && d.diagnostic.code == Code::Posture
            && d.diagnostic.message.starts_with("[prod] ")
            && ttg_codegen::posture::rule_of(&d.diagnostic) == Some("database_backups")),
        "{:?}",
        envs.iter().map(|d| &d.diagnostic.message).collect::<Vec<_>>()
    );
    assert!(!posture(&p, &cat, "aws")
        .iter()
        .any(|(r, _, _, _)| r == "database_backups"));
}
