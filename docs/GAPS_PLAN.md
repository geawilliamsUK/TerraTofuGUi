# Closing the gaps from the CallScope design

Execution plan for the findings in the CallScope gap report (an agent built a 111-node
production design through the MCP and listed what it could not express). Acceptance is
that report's own yardstick: the AWS export of that design needs no `$raw` and a handful
of native resources instead of forty-six, and the Azure and GCP exports carry the same
security posture.

Work is split into packages that touch disjoint parts of the tree so they can be built
in parallel and merged; each package lands as one commit.

## Wave A

| Package | Scope | Report items |
|---|---|---|
| **WP1 MCP friction** | natives through `entity_add` / `project_apply`; names in `entity_ref` fields; reads no longer wait behind an approval prompt (writes queue in order); AWS `sg-` name check; `schema_show` `depth` / `required_only`; annotation removal never asks; `$ref` with `block` documented; `--serve` in the server instructions | 1.1–1.5, 6.10, §4 |
| **WP3 Security posture** | `encryption_key` type + `encrypted_with` relation; object storage hardening (public access block, TLS-only policy, lifecycle, CORS, access logging); database HA / encryption / backups / deletion protection / final snapshot / version / instance class override; queue retention, long polling, encryption; secret recovery window, generated values; log group and topic encryption with the key policy that allows the services; registry immutability, scan on push, lifecycle; project-wide default tags | 2.2, §3 |
| **WP6 Views as documents** | note annotations; logical (annotation-only) nodes flows can attach to; `view_get` and a `view` argument on the drawing tools; `view_fit` and screenshot options; containers follow their members in a view; flow step numbers, colours and label placement; view description and legend; Markdown / Mermaid export of a view; filters by provider layer, native-vs-curated, type id, name glob, hide structural links; nested groups | 6.1–6.9, 6.11–6.13 |

## Wave B

| Package | Scope | Report items |
|---|---|---|
| **WP2 Kubernetes story** | `kubernetes_node_pool` type (free-text instance type, min 0, taints, labels, GPU image, spot); `kubernetes` trusted service on roles; `kubernetes_workload` type whose links generate pod identity (AWS), workload identity (Azure, GCP) and least-privilege policy; container apps get policies from links too; load balancers forward to clusters; clusters log to a log group; cluster version, endpoint access, log types, add-ons | 2.1, 2.5, 2.6, 2.14, 2.15, 6.2 (workload half) |
| **WP4 Internet edge** | `tls_certificate` type and an HTTPS listener; `web_application_firewall` type; `cdn` type; `user_identity` (Cognito / Identity Platform); DNS alias records to a load balancer or CDN | 2.4, 2.7, 2.8, 2.18 |
| **WP5 Operational rest** | dead-letter relation with max receives; `file_system` type with mount targets; `budget` type; AWS interface endpoints without a target, per zone, gateway for S3; repositories on a registry; alarm presets and more watch targets; topic subscriptions; flow logs; `audit_trail` type | 2.3, 2.9–2.13, 2.16, 2.17 |

## Status (2026-09-14)

All six packages are merged on `master`, each verified with clippy for both feature sets,
the full suite with `tofu validate` on all three providers, and the strict catalog check.
The catalog grew from 31 to 41 curated types (`encryption_key`, `kubernetes_node_pool`,
`kubernetes_workload`, `tls_certificate`, `web_application_firewall`, `cdn`,
`user_identity`, `file_system`, `budget`, `audit_trail`), the IR gained the
`encrypted_with` and `dead_letters_to` relations, view notes / logical nodes / flow
steps / legends / exports, project-wide tags, and the mapping language gained relation
conditions on incoming edges and target fields, `target_shares_ancestor` over a relation,
`starts_with` with a transform, `wrap = "map"`, `column`, raw expressions with spliced
`refs`, several declarations of one relation kind, helper providers, default tags, and
manual steps on logical mappings. Details per package are in `docs/PHASE2_PLAN.md`.

Against the CallScope design the report was written from (111 nodes, 46 AWS-only natives,
21 `$raw` uses): the curated types now cover every one of the 46 natives — the 15 S3
sub-resources and the KMS pair (WP3), the EKS node group, the seven pod-identity
associations and the six role policies (WP2), the ACM certificate, the WAF pair and the
Cognito trio (WP4), and the EFS trio, the budget and the five VPC endpoints (WP5) — and
`$raw` is no longer needed anywhere the report listed (the roles' KMS statements are the
one remaining `extra`). The design file itself is unchanged: rebuilding it on
the curated types is the next thing to do with the MCP.

## Then

Integrate: full suite with `tofu validate` on all three providers, clippy for both
feature sets, the strict catalog check, and an export of the CallScope design as the
acceptance test. Refresh docs (README catalog paragraph, MAPPING_FORMAT, MCP_PLAN) and
the memory of what was learnt.

Deferred, with the reason: a `tier` on subnets so route tables attach themselves
(changes the modelling of routing that the reachability engine depends on; revisit with
the auto-layout work); SVG export of a view (egui has no vector back end; PNG via the
fitted screenshot and Mermaid cover the document use).

## Round 2 (report of 2026-09-17)

The design was rebuilt on the curated types (84 resources, no natives, no `extra`, no
`$raw`, six saved views). The second report lists what that rebuild ran into. Same
method: disjoint packages, one commit each, merged onto `master` after review.

| Package | Scope | Report items |
|---|---|---|
| **WP7 MCP and views** | a command whose client has gone is never run (no double apply); the GUI validate runs off the UI thread; a busy app answers "queued behind" instead of silence; `view_save` refuses a duplicate name unless asked to replace; `view_delete`; `view_update { filter }`; `entity_move` on notes, logical nodes and groups; anchored notes placed beside their anchor; nesting surfaced and tested; `screenshot { width, height }`; `hide_edges` on saved views; flow labels that do not overprint | R2.1–R2.6, R2.9, §2.3 |
| **WP8 Kubernetes** | `kubernetes_workload`: `logs_to`, `mounts` file system, `reads` / `writes` on buckets, `calls` workload; IAM database authentication on `relational_database` so a workload link grants `rds-db:connect`; the cluster's log group named `/aws/eks/<cluster>/cluster` (incoming relation sources in the mapping language); `cluster_autoscaler` add-on with its tags and IAM; `container_insights` | R2.12, R2.14, R2.18, R2.19 |
| **WP9 Diagnostics** | a per-provider check may *omit* the entity from that provider's export with a warning instead of blocking the export; cross-provider diagnostics ("would block the Azure export") while another provider is the target | R2.10, R2.17 |
| **WP10 Internet edge** | provider aliases in the mapping language (`us-east-1` for CloudFront); `scope` on `web_application_firewall` and a global ACL for the CDN; the CDN's certificate in the right region; the CDN alias honoured by `dns_record` on Azure and GCP | R2.13, R2.16 |
| **WP11 Small fixes** | `""` / `null` in an `entity_ref` means none; the Performance Insights check reads the effective instance class; `cors_methods` on object storage; `budget` notifies a topic and GCP's billing account is a provider variable | R2.7, R2.8, R2.11, R2.15 |

### Status (2026-09-17)

All five packages are merged on `master` (commits 8c21234, 410f1cf, dfe21bd, 601d29f,
3cbf77b, ae3d5df), each verified with clippy for both feature sets, the full suite with
`tofu validate` on all three providers, the strict catalog check and the project-schema
drift check. The CallScope design exports and validates on AWS (12 manual steps, down
from 14), Azure and GCP without being touched.

Mapping language: provider aliases (`[[aliases]]` / `provider_alias`), incoming relation
sources (`incoming = true` on a source, `field` + `transform` on a relation source,
`min_count` on a relation condition, list-valued `equals` as membership), `ends_with` /
`not_ends_with` on field and provider-field conditions, `options` on `string_list`
fields, and the `omit` check severity. Diagnostics carry an optional `provider` and
`diagnostics::other_providers` / `run_all` report what the other providers would refuse.
MCP: `view_delete`, `view_update { filter }`, `view_save { replace }`, annotations
movable through `entity_move` / `entity_resize`, notes placed beside their anchor,
`parent` on groups, `screenshot { width, height }`, `hide_links` alias, and no command is
ever applied for a caller that has gone (closed reply, heartbeat, busy reason).

Open, with the reason: the AWS alarm metric check stays an `error` although EKS cluster
cpu / memory only exist through Container Insights (make it `omit` too, or map those two
presets to Container Insights, once someone needs it); an omitted entity is recomputed by
each `layers` entry point rather than cached (cheap today, worth a cache if catalogs grow);
GCP's `billing_account` provider variable is emitted for every GCP export like Azure's
`subscription_id`; `screenshot` sizes are bounded by the display because egui has no
off-screen renderer.

## Round 3 (report of 2026-09-29)

The third report (`terratofu-round3.md` beside the gaps file) tested `1f333a0` against the
CallScope design and found two things that would deploy broken or unsafe (the cluster's
nodes are in no security group; destructive defaults in the HCL), plus 30 items of
reachability, Kubernetes, environment, MCP and view work. Its own order is kept: the
P0 items first, reachability once the security groups it depends on exist, the bigger
ideas last. Three waves; packages in a wave touch disjoint parts of the tree.

### Wave A

| Package | Scope | Report items |
|---|---|---|
| **WP12 Cluster networking and safe defaults** | "Uses security group" on `kubernetes_cluster` and `kubernetes_node_pool`; the cluster itself as a rule source (its own security group, node ranges / network tags on Azure and GCP); workloads inherit their cluster's and pool's groups in reachability; orphan-source warning and a private-DNS endpoint that admits nothing as an error; `force_delete` false on registries, gp3 storage on databases, a public-API warning, launch templates with IMDSv2 and a key-encrypted root volume on node groups; `options` on cluster add-ons | R3.1, R3.3, R3.18 |
| **WP13 State and provider versions** | backend and state encryption through `settings_set` (and unknown keys refused); an OpenTofu `encryption {}` block whose `aws_kms` key provider names an `encryption_key` entity; a bootstrap root for the state bucket; a diagnostic when generated secrets meet local, unencrypted state; per-project provider version constraints with a tested default and the catalog checked against the chosen major | R3.2, R3.4, R3.14 |
| **WP15 Kubernetes manifests** | an optional `k8s/` directory per export: Namespace, ServiceAccount, Deployment skeleton with env from links, EFS / Azure Files / Filestore volumes, node selection from a new "Schedules on" pool link, KEDA scaling on a queue, Service and TargetGroupBinding; the workload fields that feeds; the outputs it needs | R3.10, R3.31 (Kubernetes half) |
| **WP16 MCP friction** | `export_diff` error text; absolute `position` plus `offset` for anchored notes; flows to hidden entities; an info diagnostic per entity omitted from another provider; version and catalog hash in `serverInfo`; `entity_preview`, filtered `diagnostics`, `project_get { fields }`, `catalog_relations`, `project_apply { dry_run }`, bulk `entity_update { select }` | R3.15–R3.17, R3.19–R3.21 |
| **WP17 Views and metadata** | `view_generate { kind: "data_flow" }`; left-to-right flow layout with label collision avoidance; Mermaid `sequenceDiagram` export and a presentation mode; notes re-flowed after layout and fit around the legend; `classification` on entities and a data label on flows; `description` and `owner` on every entity, emitted as tags and comments | R3.22–R3.27 |

### Wave B

| Package | Scope | Report items |
|---|---|---|
| **WP14 Reachability complete, and an audit** | file systems (mount targets, 2049, `mounts` as intent); interface endpoints covering their whole service, NAT egress where an endpoint exists; load balancer to cluster to workload, the internet as a source; `reach_audit` and `status` / `only_intended` filters; posture rules as a diagnostics category with ids and toggles, keyed on classification | R3.5–R3.9 |
| **WP18 Environments and plan** | named environments with per-entity overrides and a `.tfvars` each; `plan_run` with creates / updates / destroys per entity and validate errors mapped to entities | R3.11, R3.13 |
| **WP19 Cost estimate** | `cost_estimate { provider, environment }` from a bundled, dated price table with editable usage assumptions; totals per group and view; a diagnostic above the budget | R3.12 |

### Wave C

| Package | Scope | Report items |
|---|---|---|
| **WP20 Diff, patterns, adoption** | `project_diff` in architecture terms and named checkpoints; `pattern_apply` and saved patterns; `import {}` blocks for adopted resources | R3.28–R3.30 |
| **WP21 Outputs and modules** | outputs only for what is marked (plus what other parts of the export need); containers or groups exported as modules behind a thin root | R3.31, R3.32 |

Acceptance is the report's own: the CallScope design, with its cluster given a security
group, audits clean, exports with no Deployment manual steps, plans by entity, estimates
inside its budget, and plans for both a pilot and a prod environment.

## Round 4 (zipOS feedback of 2026-10-02)

A second project, zipOS (an ECS on Fargate web app behind CloudFront, with a worker and a
migration task), was built through the MCP and logged 23 findings in its own
`TERRATOFU_FEEDBACK.md` (TF-001 to TF-023). It was tested against the 2026-09-17 release
binary, so part of it was already fixed on `master` by round 3 before it was written.

### Already on master

| Item | Where |
|---|---|
| TF-013 backend block, S3 `use_lockfile`, AWS provider 6, `settings_set { backend, state_encryption }` | round 3 state and versions |
| TF-018 cost hints per node and per graph | round 3 cost estimate |
| TF-019 a published schema and a CLI that exports without the GUI | `schemas/project.schema.json`, `ttg export` (documented as an interchange format in WP26) |
| TF-020 a write whose caller has gone is never applied | round 2 (WP7); the approval-prompt timeout itself is WP26 |
| TF-022 ECR `force_delete` defaults to false | round 3 cluster networking and safe defaults |
| TF-023 flow layout, hidden structural links, sized screenshots | round 3 views; the tier-aware layout is WP27 |

### Wave 1

| Package | Scope | Items |
|---|---|---|
| **WP22 Containers** | `container_app` app config (env, secrets from a Secret with a JSON key, health check, stop timeout, CPU architecture, image from a registry repository); a shared container environment (ECS cluster with Container Insights, Container Apps environment) that apps sit in; separate execution and task roles, the execution policy only on the execution role; `uses` / `reads` links with least-privilege policies; load balancer forwards to a container app; a background-worker flag; a `container_job` type (ECS task definition, Container Apps Job, Cloud Run Job); alarms follow the shared cluster | TF-005–TF-008, TF-016 (1–4), TF-021 (5) |
| **WP23 References and export hygiene** | native data sources addressable from `$ref`; a managed-prefix-list source on security group rules; ports optional for `all` / `icmp`; `$raw` addresses parsed into graph dependencies and diagnostics; `$ref` to a repeated block by key; key-addressed `for_each` with `moved` blocks; `extra: { arg: null }` removes a mapping argument; `fmt`-clean output checked in the validate suite; lock-file platforms | TF-003 (1), TF-004, TF-013, TF-015 |
| **WP24 Secrets, databases, alarms and defaults** | a secret whose value is managed outside OpenTofu; an AWS-managed RDS master password; plan-time argument conflicts caught before export; alarm dimensions, percent storage, percentiles, rates, float thresholds, units; free-form database storage with autoscaling; provider-valid log retention; subnet CIDRs that do not collide; tagged-image lifecycle; numeric `extra` coercion; project rename over MCP; a terse reply mode | TF-014, TF-021 (1–4), TF-022 |
| **WP25 Edge** | the regional Web ACL only when a load balancer is protected; CDN methods, path behaviours with managed policies, origin timeout and a secret origin header; an origin name the certificate covers; WAF rate rules scoped by path; AAAA records; Azure Front Door for `cdn` + `web_application_firewall` | TF-009–TF-011, TF-016 (5) |
| **WP26 Remote and file-based access** | the MCP server reachable from a cloud session (a public bind behind a tunnel, OAuth for claude.ai custom connectors); approval-gated calls answer at once; the `.ttg.json` file documented as an interchange format, with `project_import` and a CLI check | TF-001, TF-019, TF-020, TF-022 (timeouts) |
| **WP18 Environments and plan** (resumed from round 3) | named environments with per-entity overrides and a tfvars each; a name prefix; project variables usable in fields; availability zones from the region; `plan_run` | R3.11, R3.13, TF-012 |

### Wave 2

| Package | Scope | Items |
|---|---|---|
| **WP14 Reachability audit** (round 3) | file systems, endpoints, load balancer to cluster and to container app, the internet as a source, `reach_audit`, posture rules | R3.5–R3.9 |
| **WP27 Canvas** | subnets as optional containers, a region frame, security group rules drawn as edges, a tier-aware layout and a toggle for monitoring links | TF-002, TF-003 (2), TF-023 |
| **WP20 Diff, patterns, adoption and import** (round 3) | `project_diff`, checkpoints, patterns, `import {}` blocks, and import of a TerraTofu-generated directory back into the graph | R3.28–R3.30, TF-017 |
| **WP21 Outputs and modules** (round 3) | outputs only for what is marked; modules behind a thin root with `envs/` | R3.31, R3.32, TF-013 (modules) |
