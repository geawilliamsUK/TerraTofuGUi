# Phase 2+ Plan

Phase 1 delivered a narrow, end-to-end vertical slice: five abstract types, two
providers, both tools, save/load, per-provider export, diagnostics and manual steps. This
document plans what comes next. **Nothing here is implemented yet.**

## Guiding rule

Breadth comes from `definitions/`, not from Rust. Every item below is checked against
that rule; where the mapping format cannot express something, the *format* grows (with a
schema_version bump and a migration note), not the application.

## Phase 2 — Broaden the catalog (AWS + Azure)

Target: the full palette category list from the brief, at parity on AWS and Azure.

| Category | Abstract types | Status |
|---|---|---|
| Network | `security_group`, `internet_gateway`, `nat_gateway`, `route_table`, `private_endpoint` | **Done 2026-09-08/09**. Private Endpoint: Azure full (sub-resource field, fallback chain to the linked service's block); AWS partial (Interface / Gateway VPC endpoint by service name). |
| Database | `relational_database` (PostgreSQL / MySQL) | **Done 2026-09-08**; Azure partial (network access is a manual step). `nosql_table`, `cache` open. |
| Load balancer | `load_balancer` (application / network, with per-instance attachments) | **Done 2026-09-08**. |
| Compute | `autoscaling_group` | **Done 2026-09-08**; Azure partial (scale rules are a manual step). `compute_image` deliberately not curated: use a native `aws_ami` / `azurerm_image` resource. |
| Serverless | `function`, `event_queue`, `topic`, `storage_queue` (Azure only) | **Done 2026-09-08**; Azure function partial (code deploy is a manual step). Event Queue has AWS FIFO and Azure SKU options; Topic fans out to queues and functions (Azure queue forwarding is a manual step); Storage Queue demonstrates provider-scoped types. |
| Container | `container_registry`, `kubernetes_cluster`, `container_app` | **Done 2026-09-08/09**. Container App = ECS Fargate (cluster + task definition + service) / Azure Container Apps (environment + app), both partial (LB fronting, subnet delegation, image pull are manual steps). |
| DNS | `dns_zone` (container), `dns_record` (A / CNAME / TXT) | **Done 2026-09-08**; Azure per-type record resources via conditional blocks. |
| Secrets | `key_vault` (container), `secret` | **Done 2026-09-08**; values are per-secret sensitive variables (`entity_var`). Key Vault logical on AWS, partial on Azure (RBAC grant). |
| Monitoring | `log_group`, `alarm` | **Done 2026-09-08/09**. Alarm keeps threshold / comparison / period abstract and the metric name per provider (namespace + dimension follow the watched resource type on AWS; scope fallback chain on Azure; Azure action groups are a manual step). |
| Database | `nosql_table`, `cache` | **Done 2026-09-08** (DynamoDB / Cosmos DB SQL; ElastiCache / Azure Cache for Redis). |

The catalog now has 24 abstract types, all at AWS + Azure parity where the provider can
express the concept, with declared partial / logical status elsewhere. Step 4 also added
the `attachment` relation kind, `for_each_relation` / `target` sources (one block per
linked resource), per-entity sensitive variables (`entity_var`) and `sensitive` outputs.

Curated catalog gaps are now closed for the planned set (`compute_image` is served by
native resources). Further curated types are added on demand; anything else is a native
resource (§2.8).

### 2.4 Serverless pipeline wiring (done 2026-09-08)

Driven by a real user design (HTTP gateway function → queue → worker function → private
database, with secrets in a vault). All ten items landed:

1. `http_trigger` on functions (AWS function URL + public invoke permission).
2. `sends_to` relation (publish permission + `QUEUE_URL` / Service Bus settings).
3. Functions inside subnets with a security group (AWS `vpc_config`; Azure EP1 plan +
   VNet integration + NSG on the integration subnet; subnet `delegation` field).
4. Function 'Attached to' Object Storage as its backing storage; `unique_scope`
   diagnostics catch duplicate provider names.
5. Derived least-privilege permissions: one inline policy per function on AWS built from
   its links; per-link built-in role assignments on Azure. `iam_role.permissions` now
   defaults to `none`.
6. Function 'Reads' secrets → `SECRETS` env var (JSON name→id) + read permission.
7. Database 'Password from' secret; Secret 'Value from database' composes the
   connection URI.
8. Function 'Uses' database → `DB_*` env vars; security-group rules sourced from another
   group (`entity_ref` rows, AWS group reference / Azure VirtualNetwork tag).
9. 'Logs to' log group (AWS `logging_config`; Azure diagnostic setting).
10. New diagnostics: `min_targets`, `expects_incoming`, definition `checks` (open ports,
    missing subnets/security groups), duplicate names, and a status-bar hint when the
    selected tool is not installed but the other is.

Plus Azure flexible servers are now VNet-integrated (delegated subnet + private DNS zone)
whenever a subnet is linked; the public-access manual step only applies when none is.

Follow-ups from reviewing the design against a real diagram (all done 2026-09-08):

- AWS security-group rules are standalone `aws_vpc_security_group_*_rule` resources, so
  two groups can reference each other without a dependency cycle.
- Built-in network checks (ARCHITECTURE.md §6.0): overlapping / out-of-range subnet
  CIDRs, zone vs region, one route table per subnet, NAT subnet without an internet
  route, function subnet without an egress route, managed services drawn inside a
  network (`network_agnostic`, informational).
- Conditions fall back to a field's declared default when the entity has no value.

### 2.5 Reachability overlay (done 2026-09-08)

`ttg-codegen::reach` (ARCHITECTURE.md §6.0a) plus the canvas overlay: posture view
(exposed resources, outbound paths) and per-selection paths with the egress chain drawn
once and in-network paths drawn through the admitting security group. `ttg reach`
exposes the same analysis headless. Open refinements:

- Peering / VPN between networks (currently "different networks" is always blocked).
- Load balancer forwarding as a hop (LB → instances), and Kubernetes ingress.
- Azure private endpoints once `private_endpoint` exists in the catalog.
- Filtering: hide everything not on the selected path.

### 2.6 v2: provider layers (done 2026-09-08)

Options weighed: (A) one graph plus scoped containers, (B) two independent graphs per
project, (C) one graph with provider membership tags. Chosen: **C**, because it keeps
parity by construction for everything shared while allowing provider-specific detail.

- `providers` on every node, container and edge (empty = all); `Project::layer` /
  `ttg_codegen::layers::project_for` produce a provider's layer (off-layer entities
  dropped, dropped containers flattened). Codegen, diagnostics and reachability run on
  the layer; `Code::Layer` diagnostics form the parity report (Info for tags and
  provider-only containers, Warning for a provider-only node with no counterpart).
- Provider-scoped types auto-tag: a Storage Queue is simply left out of the AWS export
  instead of blocking it.
- GUI: concrete mode is the layer switch (off-layer entities and links dimmed, tags on
  anything not on every provider), "Providers" checkboxes in the entity and link
  inspectors, a "Provider layers" summary in the project inspector. MCP tools take a
  `providers` argument and the summary reports each layer.
- Mapping language (schema 2): `{ ancestor = "…" }` conditions, `absent = true` on field
  conditions, `ancestor = "…"` on relation / target sources, `fallback` on relation
  sources. First user: the Azure-only `servicebus_namespace` container; queues and
  topics inside it share the namespace and topic subscriptions auto-forward.
- One shared layout for provider layers; views handle presentation.

### 2.8 Full provider scope: schema index, extra arguments, native resources (2026-09-09)

Hand-curating 2,600+ provider resources is neither feasible nor desirable (the curated
tier is about portability). Instead: `ttg-schema` bundles a compact gzipped index of the
provider schemas (`ttg schema refresh` regenerates it from `<tool> providers schema
-json`, per user or into the repo), and three features sit on it: Tier 2 extra
arguments on curated resources (`Node.extra`, merged by the emitter, validated by
`Code::Extra` diagnostics, schema-driven editor in the inspector), Tier 3 native
resources (`native:<provider>:<type>` synthetic definitions, palette search, full
schema inspector, auto-tagged provider layer), and a CI test that checks every curated
mapping against the schema. MCP: `schema_search`, `schema_show`, `entity_update.extra`.
Example: `native-extras.ttg.json`.

### 2.7 Views as architecture maps (2026-09-09)

User feedback: two views looked identical and the filter menu closed on every tick.
Fixed (filter changes save into the active view; menu stays open) and extended:
`View.layout` (per-view positions / sizes, on by default for new views, toggle in the
tab menu), `View.groups` (draw.io-style boxes; members = entities whose centre is inside;
dragging the title moves them) and `View.flows` (labelled, optionally dashed arrows
between entities and/or groups). Annotations never reach codegen. MCP: `view_activate`,
`view_group_add`, `view_flow_add`, `view_annotation_remove`. Example: the "Data flow"
view in job-pipeline.

### 2.1 Mapping-format additions (schema_version 2)

Done on 2026-09-08 (step 3 of the agreed order), documented in MAPPING_FORMAT.md §2.6 and
exercised by `security_group.toml` + `compute_instance.toml`:

1. ~~Repeated blocks from a list~~ — `struct_list` fields with typed `items`,
   `for_each_field` on nested, top-level and data blocks, `item` / `item_index` sources,
   item-scoped `map`, `when` and templates.
2. ~~`for_each` over a `string_list`~~ — same mechanism (row item `value`).
3. ~~Data sources~~ — `[[providers.<id>.data]]` + `self_data` references.
4. Extra, not originally planned: `if / then / else` sources, `fallback` on fields, and
   `target_type` filters on relation sources and conditions.

Still open (schema_version 3 candidates):

- **Choice of block by field value**: `resource = { map = "record_type", table = { A = "azurerm_dns_a_record", … } }`.
- **Sensitive fields**: `sensitive = true` on a field → emitted as a `sensitive` variable
  rather than inline, with the diagram value written to a `terraform.tfvars.example`.
- **Validation across relations**: `targets` per-provider overrides for cases where one
  provider accepts a relation the other cannot.

All additions are additive; the loader keeps accepting `schema_version = 1` files and
rejects v1 files that use v2 features.

### 2.2 Codegen additions

- `moved`/`import` block generation for manual → managed transitions (design only).
- Module output: optional "one module per container" layout as an export option.
- `terraform fmt`-exact formatting by shelling out when the binary exists (cosmetic).
- Multi-line rendering of `jsonencode({...})` arguments.

### 2.3 GUI additions

Done on 2026-09-07 (step 2 of the agreed order):

- ~~Recent files; unsaved-changes prompt on close.~~
- ~~Inspector: per-field "which providers use this" hint.~~
- ~~Orthogonal edge option~~, side-aware anchoring and fan-out of edges sharing a side.

Done on 2026-09-08 (canvas ergonomics, item 1 of the visual-clarity order):

- ~~Redundant containment links~~: a `via_parent` relation whose target is an
  enclosing container is neither drawn nor created; `Code::Redundant` info diagnostic.
- ~~Container terminals~~: an edge to a container ends on the wall nearest the other
  endpoint (inside or outside), drawn as a "via" glyph instead of an arrowhead.
- ~~Per-edge anchors~~: `Edge.layout.{source,target}.{side,offset}` in the project file,
  edited in the link inspector (auto/left/right/top/bottom, -100..100 along the side).
- ~~Node resizing~~: `Node.size` (optional, default 176×64) via corner drag or inspector.

Done on 2026-09-08 (items 2–5 of the visual-clarity order):

- ~~Views~~: `Project.views` (named `ViewFilter`s: categories, link kinds, focus ±N
  hops, hidden, "only"), view tabs above the canvas, filter menu, "path" buttons in the
  reachability lists that show only one path. `TTG_VIEW=<name>` for screenshots.
- ~~Provider display toggle~~: Abstract / Concrete (`P`), concrete labels with helper
  counts and address tooltips, palette concrete names, inspector restricted to the target
  provider's fields, optional icon pack under `definitions/icons/<provider>/`.
  `TTG_DISPLAY=concrete` for screenshots.
- ~~Reachability~~: arrowheads on every path, "reached by" (`reach::paths_to`) for
  passive resources, listening-port tags (`reach::listening_port`), inbound arrow on
  exposed resources in the posture view.
- ~~Queue implementation choice~~: Event Queue gained AWS `fifo` and Azure `sku`; new
  `topic` type (SNS / Service Bus topic with per-subscriber subscriptions, queue policies
  and Lambda permissions); new Azure-only `storage_queue` type; `providers = [...]`
  scoping on `[resource]` with a blocking `ProviderOnly` diagnostic. Examples:
  `fan-out.ttg.json`, `azure-storage-queue.ttg.json`.

- ~~Tidy layout + align/distribute~~ (item 6, 2026-09-08): `ttg_core::layout` (layered
  per-container layout with cycle breaking and one barycenter pass; align; distribute),
  Edit ▸ Arrange, Ctrl+L, container "Tidy contents", `ttg tidy`, `TTG_TIDY=1` for
  screenshots.

- ~~MCP server~~ (2026-09-08): built into `ttg-app` behind the `mcp` feature with the
  official `rmcp` crate (Streamable HTTP on localhost, bearer token, off until toggled;
  see MCP_PLAN.md for the design and the tool list).

Next: the v2 (one graph per provider) discussion.

Done on 2026-09-09 (polish sweep):

- ~~Reachability gaps~~: paths cross a `network_peering` (new curated type; AWS needs the
  route tables linked, Azure routes by itself), go over a `private_endpoint` in the
  source's network, and are retried through a load balancer that forwards to the
  target (`reach::direct_path` + hop composition). Example: `hub-spoke.ttg.json`.
- ~~Azure partial mappings~~: function code deploy (`zip_deploy_file` + run-from-package),
  database firewall rule from an allowed range, alarm action group with an email
  receiver, ECS task-execution role (`trusted_service = "container"`), VMSS NSG. Subnet
  delegation and Container Apps /23 sizing are design-time checks instead of manual
  steps. Relations can be scoped with `providers = [...]` so AWS-only links (LB security
  group, NAT subnet, peering route tables) stop producing Azure "cannot express" noise.

- ~~Obstacle-aware edge routing~~ (2026-09-09): orthogonal links score a set of
  candidate Z / U detours around nearby nodes (crossings, then bends, then length) and
  take the best; View ▸ *Route around nodes* toggles it.
- ~~Diff view~~ (2026-09-09): `ttg_codegen::diff` (LCS line diff of a fresh generation
  against an export directory), the "Changes vs last export" window, and `ttg diff`.

Still open:

- Auto-layout for imported/large diagrams beyond *Tidy layout* (e.g. crossing-minimising
  ordering across containers).

## Phase 3 — GCP (done 2026-09-09)

`definitions/providers/gcp.toml` (`hashicorp/google ~> 6.0`, variables `project`,
`region`, `zone`, no required ancestor: the project is a variable and `resource_group` is
logical like on AWS) plus a `[providers.gcp]` section in every curated definition. The
Rust changes were the `BUILTIN_PROVIDERS` line, a GCP reachability policy (implied
deny-ingress / allow-egress; Cloud NAT or an internet gateway anywhere in the network
gives every subnet egress, route tables being logical) and the matching network checks.
The bundled schema index now carries `hashicorp/google` 6.50.0 (1,096 resources) and the
schema check covers the GCP mappings too.

Shapes worth knowing:

- **Security groups** become one `google_compute_firewall` per rule. Membership is a
  network tag held in a `terraform_data` resource per group, so instances, templates and
  Cloud Run services reference `terraform_data.<sg>_tag.output` and group-to-group rules
  use `source_tags`. `terraform_data` is built into both tools; the schema check skips it.
- **Cloud SQL** gets private IP through private services access generated by the
  database itself (`google_compute_global_address` + `google_service_networking_connection`);
  one connection per network is a documented manual step.
- **Cloud Functions (2nd gen)**: zip uploaded to a source bucket (the linked Object
  Storage or a generated one), Pub/Sub `event_trigger` from the linked queue's topic,
  Serverless VPC Access connector for subnet placement, least-privilege IAM members per
  link, `allUsers` invoker for HTTP triggers.
- **Queues** are a Pub/Sub topic plus a pull subscription; topics fan out as one
  subscription per linked queue or function.
- **Load balancer**: regional passthrough NLB (health check, unmanaged instance group,
  backend service, forwarding rule); HTTP is served as TCP. **Autoscaling group**:
  instance template + regional MIG + CPU autoscaler; the LB link becomes a named port.
- **Logical on GCP**: `internet_gateway`, `route_table`, `private_endpoint`, `key_vault`,
  `resource_group`.
- Provider-scoped relations keep GCP quiet where a concept does not apply (functions'
  and databases' security groups, `logs_to`, the private endpoint's links).

Open on GCP: Private Service Connect for private endpoints and provider-level project
creation (`google_project`). The HTTP(S) load balancer variant landed with the edge work
below.

## Internet-facing edge (gap report 2.4, 2.7, 2.8, 2.18)

Four curated types and the load balancer / DNS record changes that use them. The example is
`examples/edge.ttg.json`; it validates on all three providers and both tools.

- **TLS Certificate** (`tls_certificate`). AWS `aws_acm_certificate` with DNS validation;
  linking a DNS Zone ("Validated in", AWS-only) also emits the `aws_route53_record`s from
  `domain_validation_options` and an `aws_acm_certificate_validation`, so listeners can wait
  for issuance. Azure only has a certificate inside a Key Vault
  (`azurerm_key_vault_certificate`, self-signed issuer policy) — outside one, nothing is
  emitted and a manual step says why. GCP `google_compute_managed_ssl_certificate`.
- **HTTPS on Load Balancer.** `protocol` gained `https`; a Certificate link (AWS + GCP) is
  required for it by a design-time check. AWS: HTTPS listener with `certificate_arn` and
  `ELBSecurityPolicy-TLS13-1-2-2021-06`, plus an optional port-80 redirect listener
  (`redirect_http`). Azure keeps `azurerm_lb` — a layer-4 load balancer cannot terminate TLS
  and an Application Gateway needs its own subnet, so that is a manual step rather than a
  silent second mapping. GCP switches to a global external Application Load Balancer
  (health check, backend service, URL map, target HTTPS proxy, global forwarding rule).
  Hardening: `drop_invalid_header_fields`, `deletion_protection`, `idle_timeout_seconds`,
  and access logs to an Object Storage node through a `logs_to` link (AWS emits the
  `access_logs` block *and* the bucket policy the ELB service needs).
- **Web Application Firewall** (`web_application_firewall`). AWS `aws_wafv2_web_acl`
  (REGIONAL) with one managed-rule statement per entry, a rate-based rule and one
  `aws_wafv2_web_acl_association` per protected load balancer. Azure
  `azurerm_web_application_firewall_policy` (OWASP 3.2 + a rate-limit custom rule); GCP
  `google_compute_security_policy` with preconfigured Cloud Armor expressions and a
  rate-based ban. Neither Azure nor GCP can attach it from this side, so both say so.
- **CDN** (`cdn`). AWS `aws_cloudfront_distribution` (origin access control and the reader
  bucket policy for an Object Storage origin, a custom origin for a Load Balancer); Azure
  classic `azurerm_cdn_profile` + `azurerm_cdn_endpoint`; GCP
  `google_compute_backend_bucket` with `enable_cdn`. A CloudFront distribution needs a
  CLOUDFRONT-scoped Web ACL, which is a different resource from the REGIONAL one — that is a
  check plus a manual step, not a pretend link.
- **User Identity** (`user_identity`). AWS Cognito pool + client + hosted domain; GCP
  `google_identity_platform_config` (partial); Azure `logical`, because Entra External ID
  needs the `azuread` provider this catalog does not carry.
- **DNS alias** (`dns_record`). An optional "Alias of" link to a Load Balancer or CDN turns
  an A record into a Route 53 `alias` block, an Azure `target_resource_id` pointing at the
  load balancer's public IP, or the Google forwarding rule's address, and suppresses
  `records` / `ttl`.

Mapping-language additions this needed: `{ raw = "…", refs = { … } }` (splice resolved
traversals into a raw expression — the ACM `for_each` comprehension), several relation
declarations sharing one kind, and `manual_steps` on a `logical` mapping.

### Round 2: internet edge (gap report 2.13, 2.16, done 2026-09-17)

A production design put a CDN in front of the load balancer and found the seams: the WAF
and the certificate each served the load balancer and left the distribution with a check
and a manual step, and a DNS record aliasing a CDN only worked on AWS. The seam was really
one missing idea — **provider aliases** (MAPPING_FORMAT §4.1). A provider definition may
declare an extra configuration of itself (`[[aliases]] name = "us_east_1"` with `args`
overriding the provider block's), a block claims it with `provider_alias`, and the emitter
writes `provider = aws.us_east_1` plus the aliased `provider` block — only when something
emitted actually uses it. With that in hand the three items become mappings rather than
prose:

- **CDN protection.** A Web ACL is scoped once and for all when it is created, so one Web
  Application Firewall entity now emits two: the REGIONAL one for its load balancers, and,
  when a `cdn` links to it, a CLOUDFRONT-scoped copy of the same rules in us-east-1 that
  the distribution's `web_acl_id` points at. The "not wired up" check and the manual step
  are gone. Azure's classic CDN endpoint cannot carry a WAF policy at all (that is Front
  Door), so "Protected by" is an AWS + Google Cloud link; on Google Cloud the same Cloud
  Armor policy is the right object but attaches from the backend service, which is a manual
  step with the reason in it.
- **Certificate region.** Likewise `tls_certificate`: a CDN link grows a second
  `aws_acm_certificate` in us-east-1 with its own validation records and
  `aws_acm_certificate_validation`, and the distribution's `viewer_certificate` uses that
  one. Both requests validate through the same zone, and `allow_overwrite` makes it work
  whether or not ACM hands out the same CNAME for the two.
- **DNS alias to a CDN everywhere.** Azure alias record sets accept a CDN endpoint for A
  *and* CNAME (an A alias is how a zone apex reaches a CDN at all), so both blocks take
  `target_resource_id` and the checks say what each record type can alias rather than
  insisting on A. Google Cloud has no alias record: a CDN with a record pointing at it
  reserves a `google_compute_global_address` and the record holds that, with a manual step
  on both sides saying which forwarding rule has to claim it. The "no values and no alias"
  check accepts a CDN alias on every provider, so an aliased record needs no placeholders.

`examples/edge.ttg.json` now has the CDN protected by the same firewall as the load
balancer, served by the same certificate, and named by a second DNS record.

## Phase 4 — Contribution guide and tooling (done 2026-09-09)

- ~~`CONTRIBUTING.md`~~ with the definition workflow, the security-group worked example,
  schema refresh, example conventions and CI expectations.
- ~~`ttg catalog --strict`~~ (dead abstract fields, outputs naming attributes the provider
  schema lacks, relations no applicable mapping consumes; exit 1) and
  ~~`ttg catalog --example <type>`~~ (starter TOML with one stub section per provider).
- ~~CI~~ (`.github/workflows/ci.yml`, `master`/`main`): fmt, clippy for both feature sets,
  the full suite with `tofu validate` on every example x provider x tool (plus the
  headless MCP test), `catalog --strict`, and an export-all artifact; provider plugins
  cached between runs.
- ~~`schemas/project.schema.json`~~ generated from the IR types (`ttg-core` `schema`
  feature, `ttg schema project`); a unit test fails when the checked-in copy is stale.
- ~~Provider schema cross-check~~ became the bundled schema index + `schema_check` test
  in Phase 2.8.

Release builds: not automated in the repository; `docs/RELEASING.md` is a step-by-step
guide with a ready-to-paste tag-triggered workflow.

## Security posture (gap report 2.2 and §3)

The CallScope gap report's finding was that a production design exported with
public-by-default buckets, an unencrypted database, unrecoverable secrets and a `destroy`
that took the data with it — everything had to be bolted on with `extra` arguments or
native resources. That vocabulary is now curated, so all three providers get the same
posture from the same diagram:

- **`encryption_key`** (`aws_kms_key` + alias with a key policy that delegates to the
  account and lets the CloudWatch Logs, SNS and SQS service principals use the key;
  `azurerm_key_vault_key` in the enclosing vault; `google_kms_key_ring` +
  `google_kms_crypto_key`), linked with the new **`encrypted_with`** relation from Object
  Storage, Relational Database, Event Queue, Secret, Log Group and Topic. Where a provider
  encrypts with platform keys and cannot take a customer-managed one from the diagram
  (Azure storage accounts and flexible servers need a user-assigned identity; Service Bus
  needs Premium; Log Analytics needs a dedicated cluster), the relation is scoped away
  from that provider and a design-time check says so on the node.
- **Object Storage**: block public access and TLS-only (both on by default), object /
  noncurrent-version / unfinished-upload expiry, CORS origins, and access logging to a
  second bucket through `logs_to`. The AWS bucket policy carries the TLS denial and, for a
  bucket others log into, the log-delivery grant — which is why a log target keeps SSE-S3
  even when a key is linked.
- **Relational Database**: high availability, backup retention, deletion protection, a
  final snapshot on destroy (on by default, named after the server), engine version and
  instance-class overrides per provider, Performance Insights on AWS, and
  `storage_encrypted` always on.
- **Event Queue**: message retention and long-poll wait. **Secret**: recovery window
  (7 days by default instead of the old unrecoverable 0) and a generated value.
  **Container Registry**: immutable tags, scan on push, keep-last-N images.
- **Project-wide default tags** on `Settings`, emitted as AWS `default_tags`, Google
  `default_labels` (sanitised to label-safe text) or, on Azure, merged into every emitted
  resource whose schema has a `tags` argument.

Three mechanisms were added to carry this: a relation condition may look at **incoming**
edges, a provider definition may declare **helper providers** (`hashicorp/random`, pulled
into `required_providers` only when a `random_*` resource is emitted) and where its
**default tags** go. `examples/hardened.ttg.json` exercises the lot on all three providers.
## Gap report WP2 — the Kubernetes story (done 2026-09-14)

Gap-report items 2.1, 2.5, 2.6, 2.14, 2.15 and the workload half of 6.2. The curated
`kubernetes_cluster` had exactly one node pool with three sizes, roles could not be
trusted by Kubernetes, and only Functions turned their links into a policy.

- **`kubernetes_node_pool`** — extra pools linked to a cluster with 'Runs on'
  (`aws_eks_node_group` on the cluster's node role, `azurerm_kubernetes_cluster_node_pool`,
  `google_container_node_pool`). Size class with a per-provider instance-type override,
  min / desired / max (0 allowed), spot, GPU, node labels and taints. A pool with no
  'Subnets' link inherits the cluster's: a relation source cannot reach a target's
  relations, so the fallback reads `vpc_config[0].subnet_ids` /
  `default_node_pool[0].vnet_subnet_id` off the emitted cluster.
- **`kubernetes_workload`** — a deployment inside a cluster, drawn for its cloud identity.
  AWS `aws_eks_pod_identity_association` plus an `aws_iam_role_policy` built from the
  links; Azure `azurerm_federated_identity_credential` on the linked identity plus the
  role assignments `function.toml` uses; GCP a `roles/iam.workloadIdentityUser` member on
  the linked service account plus IAM members per link. Every provider is `partial`: the
  Deployment, the ServiceAccount and its annotations stay in the repository.
  `container_app` now derives the same AWS policy from its queue and topic links.
- **`iam_role.trusted_service = "kubernetes"`** — the AWS trust policy becomes
  `pods.eks.amazonaws.com` with `sts:AssumeRole` + `sts:TagSession`. Azure and GCP
  identities are generic, so nothing changes there.
- **Cluster** — `logs_to` a Log Group (`enabled_cluster_log_types`, an AKS diagnostic
  setting, GKE `logging_config`), a Kubernetes version, private API endpoint and public
  access CIDRs in the block each provider already writes, and an `addons` list rendered as
  `aws_eks_addon` resources on AWS with honest notes for AKS and GKE. Workload identity
  (`oidc_issuer_enabled` / `workload_identity_enabled`, `workload_identity_config`,
  `access_config`) is switched on so the Workload type has something to bind to.
- **Load balancer → cluster** — AWS gives the target group `target_type = "ip"` and no
  attachments, with a `TargetGroupBinding` manual step; Azure and GCP say what an AGIC
  Application Gateway or a standalone NEG needs instead.
- **Mapping language** — `wrap = "map"` (a `string_list` of `key=value` as an object) and
  `column = "<item>"` (one column of a `struct_list` as a list). A `when`-guarded manual
  step that names an unexpressible relation now replaces the generic "link by hand" entry.
- **Reachability** — a workload initiates traffic and lives in its cluster's subnets.
- `examples/kubernetes.ttg.json`: a cluster with a default pool, a GPU spot pool that
  scales to zero, two workloads sharing a queue, a load balancer in front of the cluster
  and control-plane logs.

## Operational rest (gap report 2.3, 2.9–2.13, 2.16, 2.17)

The same design had a working queue and a dead-letter queue drawn as two unrelated nodes
with a `$raw` redrive policy between them, alarms whose metric name was typed by hand for
each cloud, seven registry nodes for seven repositories, and a file system, a budget and
five VPC endpoints as native resources. All of that is curated now:

- **Dead-letter queues**: the new **`dead_letters_to`** relation on Event Queue plus a
  *Deliveries before dead-lettering* field. AWS builds the `redrive_policy` with
  `jsonencode` rather than `$raw`; Azure sets `dead_lettering_on_message_expiration` /
  `max_delivery_count` and forwards the dead letters when both queues are in one Service
  Bus namespace (a check explains when they are not); Google Cloud writes a
  `dead_letter_policy` and the publisher / subscriber bindings its service agent needs.
- **`file_system`**: `aws_efs_file_system` with one mount target per linked subnet, a
  Premium FileStorage account with an NFS share on Azure (private access is a manual
  step), a `google_filestore_instance` peered into the subnets' network.
- **`budget`**: `aws_budgets_budget`, `azurerm_consumption_budget_resource_group` (the
  start month is a field, because the honest default is computed from apply time),
  `google_billing_budget` with an email notification channel.
- **Private endpoints without a target on AWS**: the *Connects to* link is optional there
  (Azure keeps it with an error check), an interface endpoint takes one subnet per zone,
  and an S3 / DynamoDB gateway endpoint takes route tables instead.
- **Repositories on a registry**: a `repositories` list becomes one `aws_ecr_repository`
  (and lifecycle policy) each, with the registry node itself as the single repository when
  the list is empty. Azure and Google Cloud create repositories on first push and say so.
- **Alarm presets**: a portable `metric` field (cpu, queue depth, dead letters, 5xx, free
  storage, bucket size, …) that fills in the metric name, namespace, dimensions,
  statistic and Cloud Monitoring filter per provider *and* per watched type, over an
  extended *Watches* list (Kubernetes Cluster, Object Storage, Topic, Cache, File System).
  A preset the watched type has no metric for is never a guess: an error on AWS, and on
  Azure and Google Cloud an `omit` that leaves that one alarm out (see "Round 2"
  below). `custom` is the default and keeps the free-text provider fields a project may
  already carry.
- **Topic subscriptions**: a table of protocol / endpoint rows becomes one
  `aws_sns_topic_subscription` each; Google Cloud pushes to `https` endpoints and says
  what it cannot do with the rest; Azure points at an Alarm's action group.
- **Flow logs**: a `flow_logs` field on Virtual Network plus a `logs_to` link to a Log
  Group. AWS generates `aws_flow_log` with the role and policy it needs; Google Cloud puts
  `log_config` on each subnet of that network; Azure explains the Network Watcher and
  storage account it would take.
- **`audit_trail`**: `aws_cloudtrail` into a linked bucket, whose own policy grows the two
  statements CloudTrail checks before it will be created; `google_project_iam_audit_config`
  for every service; logical on Azure, where the Activity Log is always on.

Two mechanisms carry this: `target_shares_ancestor` may name a **relation** instead of
reading the current `for_each_relation` row, and an output whose block is repeated is the
**list** of its instances. `examples/operations.ttg.json` exercises the lot.

## Round 2: diagnostics (gap report R2.10 and R2.17)

An 84-resource design built through the MCP hit two diagnostics that were right about the
cloud and wrong about what to do next. Three alarms watched metrics Azure Monitor and
Cloud Monitoring do not publish, and each one stopped those providers exporting at all;
and a TLS certificate drawn outside a Key Vault said nothing until someone tried the Azure
export. Both are now answered without loosening a single message.

- **`severity = "omit"`** on a definition check (MAPPING_FORMAT.md §2.6): the provider
  cannot express *this one entity*, so it leaves that provider's layer exactly as a
  `providers` tag would — dropped from the export with its links, children re-parented,
  nothing left dangling — and the check's message is reported once as a warning with
  `; left out of the <Provider> export` appended. The entity still draws on the canvas;
  a view filtered by `providers` treats it as off that layer. The Alarm's
  metric-availability checks are `omit` on Azure and Google Cloud and stay an `error` on
  AWS, which is the reference for the presets.
- **Cross-provider diagnostics**: `diagnostics::other_providers` (and `run_all`) report
  what the *other* providers would refuse, as warnings tagged with the provider and
  prefixed `[Microsoft Azure] … (would block the Microsoft Azure export)`. They show up in
  a collapsed "Other providers (N)" section under the target's list, under the agent's
  `other_providers` key, and after `ttg check`'s own list. Exports still block on the
  target provider's errors alone, and an entity off another provider's layer produces
  nothing for it. Three providers cost about three times one — roughly 17 ms for 84
  resources — so the app computes them only while the panel is open and caches until the
  project changes.

## Explicitly still out of scope

Running `plan`/`apply`, live-account access, multi-user collaboration, cost estimation,
drift detection and state visualisation remain non-goals for these phases.
