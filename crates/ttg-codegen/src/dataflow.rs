//! Data flows derived from the links a diagram already has.
//!
//! A workload's links say most of what a data-flow map says: it sends to a queue,
//! reads a secret, uses a database, mounts a file system. [`rule`] is the one table that
//! decides, per relation kind (and, where the kind is ambiguous, per source and target
//! type), whether a link carries data, which way the data moves, and what to call it.
//! [`generate`] draws those flows into a view between the things it shows;
//! [`personal_data_view`] builds the "Where personal data goes" view from the
//! entities classified personal or payment.
//!
//! | relation | when | data moves | label |
//! |---|---|---|---|
//! | `sends_to` | always | source → target | sends to |
//! | `dead_letters_to` | always | source → target | dead letters (dashed) |
//! | `logs_to` | always | source → target | logs (dashed) |
//! | `calls` | always | source → target | calls |
//! | `reads` | source is a workload | target → source | read by |
//! | `attribute_reference` | workload → queue | target → source | consumed by |
//! | `attribute_reference` | workload → database, cache, bucket, file system, table | source → target (both ways) | uses |
//! | `attribute_reference` | CDN → bucket or load balancer | source → target | origin |
//! | `attachment` | anything → file system | target → source (both ways) | mounted by |
//! | `attachment` | load balancer → instance or cluster | source → target | forwards to |
//! | `attachment` | scaling group or Kubernetes workload → load balancer | target → source | forwards to |
//! | `attachment` | audit trail → bucket | source → target | writes to |
//!
//! Everything else is structure, not data, and produces nothing: `network_membership`,
//! `iam_binding`, `encrypted_with`, `depends_on`, containment, a workload's security
//! group or certificate, a database reading its own password from a secret (wiring, not
//! traffic), a function's code bucket, what an alarm watches, a DNS alias, a firewall
//! attached to a load balancer, route tables, and where a Kubernetes workload pulls
//! its image from or which node pool it is scheduled on. "Workload" means a function, container
//! app, Kubernetes workload, cluster or node pool, compute instance or scaling group.
//! "Both ways" marks a read/write link: the arrow is drawn in the direction of the
//! request, but data can come back along it, which is what [`personal_data_view`]
//! needs to know.

use crate::views::visible_set;
use std::collections::BTreeSet;
use ttg_catalog::Catalog;
use ttg_core::flow_layout::{layout_view_by_flows, FlowLayoutOptions};
use ttg_core::view::arrange_notes;
use ttg_core::{Flow, FlowEnd, Id, Project, Relation, View, ViewFilter};

/// Name of the view [`personal_data_view`] builds (and refreshes).
pub const PERSONAL_DATA_VIEW: &str = "Where personal data goes";

/// What one kind of link means as a data flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlowRule {
    /// Data moves from the link's target to its source.
    pub reverse: bool,
    pub label: &'static str,
    /// Secondary traffic (logs, dead letters): drawn dashed.
    pub dashed: bool,
    /// A read/write link: data can travel back along the arrow too.
    pub two_way: bool,
}

const WORKLOADS: &[&str] = &[
    "function",
    "container_app",
    "kubernetes_workload",
    "kubernetes_cluster",
    "kubernetes_node_pool",
    "compute_instance",
    "autoscaling_group",
];
const QUEUES: &[&str] = &["event_queue", "storage_queue"];
const STORES: &[&str] = &[
    "relational_database",
    "cache",
    "object_storage",
    "file_system",
    "nosql_table",
];

const fn fwd(label: &'static str) -> FlowRule {
    FlowRule {
        reverse: false,
        label,
        dashed: false,
        two_way: false,
    }
}

/// Does a link of kind `rel` from a `source` type to a `target` type carry data, and
/// which way? See the table in the module documentation.
pub fn rule(rel: Relation, source: &str, target: &str) -> Option<FlowRule> {
    let workload = WORKLOADS.contains(&source);
    match rel {
        Relation::SendsTo => Some(fwd("sends to")),
        Relation::Calls => Some(fwd("calls")),
        Relation::DeadLettersTo => Some(FlowRule {
            dashed: true,
            ..fwd("dead letters")
        }),
        Relation::LogsTo => Some(FlowRule {
            dashed: true,
            ..fwd("logs")
        }),
        Relation::Reads if workload => Some(FlowRule {
            reverse: true,
            ..fwd("read by")
        }),
        Relation::AttributeReference if workload && QUEUES.contains(&target) => Some(FlowRule {
            reverse: true,
            ..fwd("consumed by")
        }),
        Relation::AttributeReference if workload && STORES.contains(&target) => Some(FlowRule {
            two_way: true,
            ..fwd("uses")
        }),
        Relation::AttributeReference
            if source == "cdn" && matches!(target, "object_storage" | "load_balancer") =>
        {
            Some(fwd("origin"))
        }
        Relation::Attachment if target == "file_system" => Some(FlowRule {
            reverse: true,
            two_way: true,
            ..fwd("mounted by")
        }),
        Relation::Attachment
            if source == "load_balancer" && matches!(target, "compute_instance" | "kubernetes_cluster") =>
        {
            Some(fwd("forwards to"))
        }
        // "Registered with" / "Receives traffic from": the load balancer sends the
        // requests to the group or the workload behind it.
        Relation::Attachment
            if matches!(source, "autoscaling_group" | "kubernetes_workload") && target == "load_balancer" =>
        {
            Some(FlowRule {
                reverse: true,
                ..fwd("forwards to")
            })
        }
        Relation::Attachment if source == "audit_trail" && target == "object_storage" => {
            Some(fwd("writes to"))
        }
        _ => None,
    }
}

/// A flow one link implies, in the direction the data moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedFlow {
    pub from: Id,
    pub to: Id,
    pub label: String,
    pub dashed: bool,
    pub two_way: bool,
    /// The link it came from.
    pub relation: Relation,
}

/// Every data flow the project's links imply between entities `include` accepts, at
/// most one per pair of entities (whichever link comes first in the project), in the
/// order of the links.
pub fn derive(p: &Project, include: &dyn Fn(&str) -> bool) -> Vec<DerivedFlow> {
    let mut seen: BTreeSet<(Id, Id)> = BTreeSet::new();
    let mut out = Vec::new();
    for e in &p.edges {
        if e.source == e.target || !include(&e.source) || !include(&e.target) {
            continue;
        }
        let (Some(s), Some(t)) = (p.entity(&e.source), p.entity(&e.target)) else {
            continue;
        };
        let Some(r) = rule(e.relation, s.resource_type, t.resource_type) else {
            continue;
        };
        if !seen.insert(pair(&e.source, &e.target)) {
            continue;
        }
        let (from, to) = if r.reverse {
            (e.target.clone(), e.source.clone())
        } else {
            (e.source.clone(), e.target.clone())
        };
        out.push(DerivedFlow {
            from,
            to,
            label: r.label.to_string(),
            dashed: r.dashed,
            two_way: r.two_way,
            relation: e.relation,
        });
    }
    out
}

fn pair(a: &str, b: &str) -> (Id, Id) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

/// Draw the flows the links imply into `v`, between the resources it shows.
///
/// A pair of resources that already has a flow in the view (in either direction) is
/// left alone, so hand-drawn flows — and their numbering and prose — are never
/// duplicated or overwritten. With `replace`, the view's resource-to-resource flows
/// are removed first (flows that start or end at a group or a logical node stay, since
/// no link can produce those) and notes pinned to a removed flow stop pointing at it.
/// Returns the flows added.
pub fn generate(p: &Project, cat: &Catalog, v: &mut View, replace: bool) -> Vec<Flow> {
    let shown = visible_set(p, cat, &v.filter);
    let visible = |id: &str| p.nodes.contains_key(id) && shown.as_ref().is_none_or(|s| s.contains(id));
    if replace {
        let gone: Vec<Id> = v
            .flows
            .iter()
            .filter(|f| matches!(f.from, FlowEnd::Entity { .. }) && matches!(f.to, FlowEnd::Entity { .. }))
            .map(|f| f.id.clone())
            .collect();
        v.flows.retain(|f| !gone.contains(&f.id));
        for n in &mut v.notes {
            if n.anchor
                .as_ref()
                .is_some_and(|a| gone.iter().any(|g| g == a.id()))
            {
                n.anchor = None;
            }
        }
    }
    let mut have: BTreeSet<(Id, Id)> = v.flows.iter().map(|f| pair(f.from.id(), f.to.id())).collect();
    let mut added = Vec::new();
    for d in derive(p, &visible) {
        if !have.insert(pair(&d.from, &d.to)) {
            continue;
        }
        let f = Flow {
            id: fresh_flow_id(p, v, &added),
            from: FlowEnd::Entity { entity: d.from },
            to: FlowEnd::Entity { entity: d.to },
            label: d.label,
            dashed: d.dashed,
            step: None,
            color: None,
            data: None,
        };
        v.flows.push(f.clone());
        added.push(f);
    }
    added
}

/// A flow id no other flow in the view uses.
fn fresh_flow_id(p: &Project, v: &View, added: &[Flow]) -> Id {
    loop {
        let id = p.fresh_id("flow");
        if v.flow(&id).is_none() && !added.iter().any(|f| f.id == id) {
            return id;
        }
    }
}

/// What [`personal_data_view`] did, for the reply.
#[derive(Debug, Clone, Default)]
pub struct PersonalDataReport {
    /// Entities classified personal or payment.
    pub sources: Vec<Id>,
    /// Entities one data-carrying link away that the data reaches.
    pub reached: Vec<Id>,
    pub added: Vec<Flow>,
    /// Flows dropped because an end is no longer part of the view.
    pub dropped: usize,
    /// The view was already there and has been refreshed.
    pub refreshed: bool,
}

/// Build or refresh the "Where personal data goes" view: the entities classified
/// personal or payment, everything one data-carrying link away that the data reaches
/// (the far end of a flow out of them, or either end of a read/write link), and the
/// flows between them as [`generate`] derives them — the ones leaving a classified
/// entity say what they carry. A refresh keeps the view's own flows, notes and boxes,
/// drops flows whose ends are no longer in it, and adds what is missing; a new view is
/// laid out by its flows. `Err` when nothing is classified personal or payment.
pub fn personal_data_view(
    p: &Project,
    cat: &Catalog,
    existing: Option<&View>,
) -> Result<(View, PersonalDataReport), String> {
    let sources: Vec<Id> = p
        .entities()
        .iter()
        .filter(|e| e.classification.is_some_and(|c| c.is_personal()))
        .map(|e| e.id.to_string())
        .collect();
    if sources.is_empty() {
        return Err(
            "nothing is classified personal or payment yet: set `classification` on the \
             resources that hold personal data (entity_update or the inspector) first"
                .into(),
        );
    }
    let is_source = |id: &str| sources.iter().any(|s| s == id);
    let mut reached: BTreeSet<Id> = BTreeSet::new();
    for d in derive(p, &|_| true) {
        if is_source(&d.from) && !is_source(&d.to) {
            reached.insert(d.to.clone());
        }
        if d.two_way && is_source(&d.to) && !is_source(&d.from) {
            reached.insert(d.from.clone());
        }
    }
    let only: BTreeSet<Id> = sources.iter().cloned().chain(reached.iter().cloned()).collect();
    let filter = ViewFilter {
        only: only.clone(),
        containers: false,
        hide_edges: true,
        ..ViewFilter::default()
    };
    let refreshed = existing.is_some();
    let mut v = match existing {
        Some(v) => {
            let mut v = v.clone();
            v.filter = filter;
            v
        }
        None => {
            let mut v = View::new(PERSONAL_DATA_VIEW, filter);
            v.description = "Resources classified personal or payment, and where their data goes one \
                             step on. Built from the links; refresh it with view_generate kind \
                             \"personal_data\" after the design changes."
                .into();
            v.legend = true;
            v
        }
    };
    let before = v.flows.len();
    v.flows.retain(|f| {
        [&f.from, &f.to]
            .iter()
            .all(|e| !matches!(e, FlowEnd::Entity { entity } if !only.contains(entity)))
    });
    let dropped = before - v.flows.len();
    let mut added = generate(p, cat, &mut v, false);
    for f in v.flows.iter_mut().filter(|f| f.data.is_none()) {
        let from = p.entity(f.from.id()).and_then(|e| e.classification);
        if let Some(c) = from.filter(|c| c.is_personal()) {
            if added.iter().any(|a| a.id == f.id) {
                f.data = Some(c.display_name().to_lowercase());
            }
        }
    }
    for a in added.iter_mut() {
        if let Some(f) = v.flow(&a.id) {
            a.data = f.data.clone();
        }
    }
    if !refreshed {
        let visible = |id: &str| only.contains(id);
        // A view with no flows between what it shows is still a useful list.
        let _ = layout_view_by_flows(p, &mut v, &visible, &FlowLayoutOptions::default());
        arrange_notes(p, &mut v, &visible);
    }
    Ok((
        v,
        PersonalDataReport {
            sources,
            reached: reached.into_iter().collect(),
            added,
            dropped,
            refreshed,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The direction table, one row per relation kind (and per disambiguating type).
    #[test]
    fn the_table_decides_direction_and_label_per_relation() {
        // (relation, source type, target type, expected (reversed, label) or no flow)
        type Case = (Relation, &'static str, &'static str, Option<(bool, &'static str)>);
        let cases: &[Case] = &[
            (
                Relation::SendsTo,
                "function",
                "event_queue",
                Some((false, "sends to")),
            ),
            (
                Relation::DeadLettersTo,
                "event_queue",
                "event_queue",
                Some((false, "dead letters")),
            ),
            (Relation::LogsTo, "function", "log_group", Some((false, "logs"))),
            (
                Relation::Calls,
                "kubernetes_workload",
                "kubernetes_workload",
                Some((false, "calls")),
            ),
            (Relation::Reads, "function", "secret", Some((true, "read by"))),
            (
                Relation::Reads,
                "kubernetes_workload",
                "object_storage",
                Some((true, "read by")),
            ),
            // Wiring, not traffic: a database reading its own password.
            (Relation::Reads, "relational_database", "secret", None),
            (
                Relation::AttributeReference,
                "function",
                "event_queue",
                Some((true, "consumed by")),
            ),
            (
                Relation::AttributeReference,
                "kubernetes_workload",
                "relational_database",
                Some((false, "uses")),
            ),
            (
                Relation::AttributeReference,
                "cdn",
                "load_balancer",
                Some((false, "origin")),
            ),
            (Relation::AttributeReference, "function", "security_group", None),
            (Relation::AttributeReference, "alarm", "relational_database", None),
            (
                Relation::Attachment,
                "kubernetes_workload",
                "file_system",
                Some((true, "mounted by")),
            ),
            (
                Relation::Attachment,
                "load_balancer",
                "compute_instance",
                Some((false, "forwards to")),
            ),
            (
                Relation::Attachment,
                "autoscaling_group",
                "load_balancer",
                Some((true, "forwards to")),
            ),
            (
                Relation::Attachment,
                "kubernetes_workload",
                "load_balancer",
                Some((true, "forwards to")),
            ),
            // Pulling an image and being scheduled on a pool carry no application data.
            (
                Relation::Attachment,
                "kubernetes_workload",
                "container_registry",
                None,
            ),
            (
                Relation::Attachment,
                "kubernetes_workload",
                "kubernetes_node_pool",
                None,
            ),
            (
                Relation::Attachment,
                "audit_trail",
                "object_storage",
                Some((false, "writes to")),
            ),
            (Relation::Attachment, "function", "object_storage", None),
            (
                Relation::Attachment,
                "kubernetes_workload",
                "kubernetes_cluster",
                None,
            ),
            (Relation::NetworkMembership, "function", "subnet", None),
            (Relation::IamBinding, "function", "iam_role", None),
            (Relation::EncryptedWith, "object_storage", "encryption_key", None),
            (Relation::DependsOn, "function", "object_storage", None),
        ];
        for (rel, s, t, want) in cases {
            let got = rule(*rel, s, t).map(|r| (r.reverse, r.label));
            assert_eq!(got, *want, "{} {s} -> {t}", rel.key());
        }
        assert!(rule(Relation::LogsTo, "function", "log_group").unwrap().dashed);
        assert!(
            rule(Relation::AttributeReference, "function", "cache")
                .unwrap()
                .two_way
        );
        assert!(!rule(Relation::SendsTo, "function", "topic").unwrap().two_way);
    }

    fn job_pipeline() -> Project {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/job-pipeline.ttg.json");
        ttg_core::project::load(&p).expect("example loads")
    }

    #[test]
    fn generated_flows_follow_the_data_and_skip_existing_pairs() {
        let p = job_pipeline();
        let cat = Catalog::builtin();
        let mut v = p.views.iter().find(|v| v.name == "Data flow").unwrap().clone();
        let hand_drawn = v.flows.len();
        let added = generate(&p, &cat, &mut v, false);
        let has = |a: &str, b: &str, label: &str| {
            added
                .iter()
                .any(|f| f.from.id() == a && f.to.id() == b && f.label == label)
        };
        // The secrets are read by the runner: data moves secret -> runner.
        assert!(has("sec-dbuser", "fn-runner", "read by"), "{added:#?}");
        assert!(has("fn-gateway", "log-pipeline", "logs"), "{added:#?}");
        // Already drawn by hand (gateway -> queue, queue -> runner, runner -> db).
        assert!(
            !added.iter().any(|f| [f.from.id(), f.to.id()].contains(&"q-jobs")),
            "{added:#?}"
        );
        assert!(!has("fn-runner", "db-jobs", "uses"));
        // Structure produces nothing: roles, security groups, subnets, code buckets.
        for f in &added {
            for end in [f.from.id(), f.to.id()] {
                assert!(
                    !end.starts_with("role-")
                        && !end.starts_with("sg-")
                        && !end.starts_with("subnet-")
                        && !end.starts_with("obj-"),
                    "{f:?}"
                );
            }
        }
        assert_eq!(v.flows.len(), hand_drawn + added.len());
        // Running it again adds nothing; replacing redraws the resource-to-resource ones.
        assert!(generate(&p, &cat, &mut v, false).is_empty());
        let redrawn = generate(&p, &cat, &mut v, true);
        assert!(redrawn
            .iter()
            .any(|f| f.from.id() == "q-jobs" && f.to.id() == "fn-runner"));
        // Flows from a logical node or a group survive a replace.
        assert!(v.flows.iter().any(|f| matches!(f.from, FlowEnd::Logical { .. })));
    }

    #[test]
    fn the_personal_data_view_starts_from_classified_entities() {
        let mut p = job_pipeline();
        let cat = Catalog::builtin();
        assert!(personal_data_view(&p, &cat, None).is_err());
        p.nodes.get_mut("db-jobs").unwrap().classification = Some(ttg_core::Classification::Personal);
        let (v, rep) = personal_data_view(&p, &cat, None).unwrap();
        assert_eq!(v.name, PERSONAL_DATA_VIEW);
        assert_eq!(rep.sources, vec!["db-jobs".to_string()]);
        // The runner uses the database (read/write): the data reaches it. The database
        // password secret is wiring, not a destination.
        assert!(rep.reached.contains(&"fn-runner".to_string()), "{rep:?}");
        assert!(!rep.reached.contains(&"sec-dbpass".to_string()), "{rep:?}");
        assert!(v.filter.only.contains("db-jobs") && v.filter.only.contains("fn-runner"));
        assert!(!rep.added.is_empty());
        // Laid out in its own layout, never the shared one.
        assert!(v.layout.as_ref().is_some_and(|l| !l.positions.is_empty()));
        // A refresh keeps the view and adds nothing new.
        let (again, rep2) = personal_data_view(&p, &cat, Some(&v)).unwrap();
        assert!(rep2.refreshed && rep2.added.is_empty());
        assert_eq!(again.flows.len(), v.flows.len());
    }
}
