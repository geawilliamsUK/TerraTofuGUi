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

## Round 2: small fixes (gap report R2.7, R2.8, R2.11, R2.15)

A follow-up pass on a design built through the MCP, fixing four rough edges the first
gap report left behind: an `entity_ref` field (a security-group rule's `source_group`)
rejected the empty string every example file uses for "no reference"; a Relational
Database's Performance Insights check only read the abstract `size`, so an
`instance_class` override neither silenced it nor caught the override itself landing on
a `.micro` class; Object Storage's CORS rule always answered `GET`/`HEAD` only, which
breaks a pre-signed `PUT` upload; and Budget could only notify an email, with GCP's
billing account id typed by hand on every node. Two small mapping-language additions
carry the lot: `ends_with` / `not_ends_with` join `starts_with` on field and (now also)
provider-field conditions, and a `string_list` field may declare `options` the same way
an `enum` does. Budget gains a `sends_to` → Topic link ("Notifies topic"), which becomes
an `aws_sns_topic_policy` statement on AWS, an explained gap on Azure (Service Bus, not
an Action Group), and a second `pubsub` notification channel on GCP; `notify_email` is
now required only when no Topic is linked. GCP's billing account moves to a project-wide
`billing_account` provider variable, with the per-budget field kept as a fallback
override for projects that already set it. `examples/operations.ttg.json` links its
Budget to its Topic to exercise the lot.

## Round 2: Kubernetes (gap report R2.12, R2.14, R2.18, R2.19)

A production EKS design rebuilt through the MCP showed what the Kubernetes types still
could not say. Four log groups were "unreferenced" because a workload could only log
through its cluster; a `/models` mount was undocumented; every bucket link granted Get,
Put, Delete and List, so the API could delete the recordings it wrote; one service calling
another was a flow annotation rather than a link; a workload could not reach a database on
AWS at all; and the Log Group drawn for the control plane sat unused next to the one EKS
named itself.

- **Workload links** — `logs_to` a Log Group (AWS: a `logs:CreateLogStream` /
  `PutLogEvents` / `DescribeLogStreams` statement on `<arn>` and `<arn>:*`, with Fluent Bit
  left as a manual step; GCP: one project-level `roles/logging.logWriter` member, because
  Cloud Logging has no per-bucket write; Azure: a manual step, since Container Insights
  writes with the *cluster's* identity), `attachment` to a File System ("Mounts": AWS
  grants `elasticfilesystem:ClientMount` / `ClientWrite` / `ClientRootAccess` and the
  manual step carries the EFS CSI PersistentVolume snippet; Azure Files and Filestore CSI
  are manual steps), and `calls` to another workload — documentation only.
- **Bucket access split by intent** — 'Uses' stays read *and* write but no longer implies
  delete: a new `delete_objects` field adds `s3:DeleteObject` / picks
  `roles/storage.objectAdmin` over `objectCreator` + `objectViewer`. A second `reads`
  declaration makes a bucket link read-only (`s3:GetObject` + `ListBucket`, *Storage Blob
  Data Reader*, `roles/storage.objectViewer`). Azure has no built-in role between reader
  and contributor, and says so.
- **Workload → database on AWS** — `iam_authentication` on Relational Database
  (`iam_database_authentication_enabled`, an `authentication` block with the Entra tenant
  on a PostgreSQL flexible server, the `cloudsql.iam_authentication` database flag). The
  workload then gets `rds-db:connect` on
  `arn:aws:rds-db:<region>:<account>:dbuser:<resource id>/<db user>`, with the account and
  region from `aws_caller_identity` / `aws_region` data sources and the user from a new
  `db_user` field. The old "database access is not an IAM grant" step now only applies
  when the switch is off; with it on, each provider explains the database role to create.
- **The EKS log group** — a Log Group a cluster logs to is named `/aws/eks/<cluster>/cluster`,
  so Terraform owns it instead of racing EKS for it. The name is built from the *cluster's
  field*, not its resource, and the cluster `depends_on` the group: a reference would be a
  cycle. Two clusters logging to one group is an error.
- **Autoscaling and insights** — `cluster_autoscaler` joins the cluster's add-ons: on AWS
  it emits the discovery tags on the cluster's node group *and* on every linked node pool,
  an IAM role trusted by pods, the autoscaler policy and a pod identity association for
  `kube-system/cluster-autoscaler`, leaving only the Helm chart; on AKS and GKE it is
  accepted and the manual step says nothing is needed. A `container_insights` bool emits
  the `amazon-cloudwatch-observability` add-on plus `CloudWatchAgentServerPolicy` on AWS,
  `oms_agent` against the linked workspace on Azure (an error without one), and
  `monitoring_config` with managed Prometheus on GCP. The `node_count` alarm warning is
  gone when the watched cluster has it on.
- **Mapping language** — `incoming = true` now works as an *argument source*, not only as
  a condition; `field = "<name>"` on a relation source reads the other entity's field as a
  literal (no reference, so no dependency); `min_count = N` counts matching targets instead
  of asking "any?"; and `equals` / `not_equals` against a `string_list` field mean
  membership, which is how a node pool reads its cluster's add-on list.
- **`calls`** — a new relation kind with no mapping at all: no `depends_on`, no manual
  step, no "cannot express" diagnostic, and out of the dependency graph, so two services
  calling each other is not a cycle.
- `examples/kubernetes.ttg.json` grows a third workload that reads a bucket read-only and
  calls the API, an EFS file system the ASR worker mounts, an application Log Group all
  three workloads write to, and an IAM-authenticated database.

## Round 3: Kubernetes manifests (gap report R3.10, and R3.31 for outputs, done 2026-09-29)

Eight of the twelve AWS manual steps on the CallScope design said "apply the Deployment and
its ServiceAccount" and one more "bind the target group to a Service", although the diagram
already knew each workload's namespace, service account, role, cluster, queues, buckets,
secrets and mounts.

- **`settings.kubernetes_manifests`** (Settings ▸ Output, `ttg export --k8s`,
  `export_run { k8s: true }`) adds a `k8s/` directory to the export: `00-namespaces.yaml`,
  one `<workload>.yaml` per Kubernetes Workload, `render.sh`, `render.ps1` and a README.
  Per workload: a ServiceAccount named as the identity binding says (no annotation on AWS,
  `azure.workload.identity/client-id` plus the pod label on Azure,
  `iam.gke.io/gcp-service-account` on GCP); a Deployment; a ClusterIP Service on the port;
  a static PersistentVolume and claim per mounted file system (EFS, Azure Files NFS and
  Filestore CSI, no StorageClass); a KEDA `ScaledObject` + `TriggerAuthentication` for a
  workload that consumes queues, or a CPU `HorizontalPodAutoscaler` otherwise, when **Max
  replicas** is set; and on AWS a `TargetGroupBinding` for a workload behind a load
  balancer that forwards to the cluster (GKE: the NEG annotation on the Service; Azure: a
  README note, because the equivalents are controller-specific).
- **Workload fields and links**, all marked `manifests = true`: `image_tag` (no default: the
  placeholder `set-image-tag` applies but cannot be pulled, and a manual step says so),
  `repository`, `image`, `port`, `replicas`, `min_replicas`, `max_replicas`,
  `messages_per_replica`, `cpu`, `memory`, `gpu`; 'Runs image from' a Container Registry,
  'Schedules on' a Node Pool (its labels become the nodeSelector — or the provider's pool
  label when it has none — its taints the tolerations, plus AKS's spot taint) and
  'Receives traffic from' a Load Balancer. Like `calls`, those links generate no
  `depends_on` and no manual step. Scaling reuses the consumed-queue link ('Uses'), one
  KEDA trigger per queue, rather than a separate "Scales on" link.
- **Connection values** — `[providers.<id>.connection]` in a resource definition names what
  a client needs (a queue's URL, a Service Bus queue's name and namespace, a Pub/Sub
  subscription, a bucket, a secret ARN / id, a database host, port and name, a cache host
  and port, a log group, a file system's volume handle, an identity's client id or email,
  a registry host, a target group ARN) as argument sources resolved on the resource itself.
  A workload gets `<PREFIX>_<NAME>_<KEY>` environment variables for everything it uses,
  sends to, reads or logs to (`QUEUE_JOBS_URL`, `BUCKET_TRANSCRIPTS_NAME`,
  `SECRET_API_KEY_ARN`; the prefix is the type's `env_prefix`).
- **Outputs and the render step** (the Kubernetes half of R3.31) — every value only
  Terraform knows becomes a `k8s_<slug>_<key>` output and a `${k8s_<slug>_<key>}` token in
  the YAML; `render.sh` (bash 3.2+, `<tool> output -raw` per token, no jq or envsubst) and
  `render.ps1` (`output -json`, parsed natively) write the finished files to `k8s/rendered/`.
  A manual or unmapped resource's value becomes an input variable, as any reference to it
  does. Generating `kubernetes_manifest` / `helm_release` resources instead is documented
  as future work: those providers need the cluster's API at plan time.
- **Mapping language** — `{ setting = "kubernetes_manifests" }` conditions (with `equals` /
  `not_equals`) let a step apply only with the manifests off: the workload's
  Deployment/ServiceAccount, annotation/label and CSI mount steps on every provider, and the
  AWS load balancer's target-group step once a workload says it is behind it. The export
  adds one step of its own, "install KEDA and/or the AWS Load Balancer Controller". Azure's
  registry now says once, not per workload, that the cluster needs AcrPull.
- `examples/kubernetes.ttg.json` is saved with the manifests on: a registry with three
  repositories, a load-balanced, CPU-autoscaled API, a GPU ASR worker on the GPU pool
  scaled to zero by KEDA on the job queue, and tags and requests on all three. Its AWS
  export goes from 11 manual steps to 7. Tests run `kubectl annotate --local -o json`
  over the rendered manifests of every provider (kubectl's own decoder, no cluster
  needed), run both render scripts against a stand-in for `tofu output`, and check that two
  exports are byte-identical.

## Round 3: state and versions (gap report R3.2, R3.4, R3.14, done 2026-09-29)

A production design exported with local state: the database password and a session key
that `secret.generate_value` creates went into a `terraform.tfstate` on the laptop, and
neither the backend nor state encryption could be set through the MCP — `settings_set`
accepted both keys and dropped them with "settings updated". The AWS provider was pinned
to 5.x a year after 6 shipped. The IR, the emitter and the settings panel already knew a
backend type and an encryption flag; this round builds on them (ARCHITECTURE.md §6.5–6.7).

- **Backends** — `s3` (with `use_lockfile`, so no DynamoDB table), `azurerm` and `gcs`, plus
  `local`. The keys each type takes are one table (`state::BACKENDS`) and one check
  (`state::check_backend`) shared by the settings panel, `settings_set` and the export
  gate. The state object is `<key_prefix>/terraform.tfstate`, `key_prefix` defaulting to the
  project name, and `state::state_key` already takes an environment that goes between the
  two, for when named environments arrive. The block moves from `backend.tf` into the one
  `terraform {}` block of `versions.tf`; `required_version` rises to OpenTofu 1.10 /
  Terraform 1.11 for S3 lock files.
- **State encryption** — the placeholder `pbkdf2` block becomes a real design: an
  `encryption {}` block with `state` and `plan` both `enforced`, keyed by `aws_kms` on AWS
  and `gcp_kms` on Google Cloud when an Encryption Key is chosen
  (`state_encryption_key`), a `state_passphrase` otherwise. Azure keeps the passphrase:
  OpenTofu 1.10 had no Azure key provider; 1.12 has `azure_vault`, but using it means moving
  a Key Vault out of the main root, which is a larger change (an info diagnostic says so).
  Terraform gets no block and a warning.
- **The bootstrap root** — the key must exist before the state it encrypts, and the bucket
  before `init` can use it, so `bootstrap/` is generated beside the main root: a small
  project of its own (the state bucket as an Object Storage node — versioning, public access
  blocked, TLS only — plus the chosen key) run through the ordinary emitter, so both get
  their curated mappings and the key keeps the policy the other resources rely on. The main
  root treats the key as external; its ARN / id arrive as variables the bootstrap root
  outputs. It validates like everything else: the validate suite exports `hardened` with
  each backend and encryption on, and runs `validate` on every root it finds. `bootstrap/`
  and the manifests' `k8s/` are one mechanism, `ttg-codegen::owned`: a table of what the
  export owns, which `export` cleans up and `diff` reports, never touching state,
  `.terraform/` or `k8s/rendered/`.
- **Secrets in state** — a new field attribute, `state_secret = true` (the Secret's
  *Generate the value*; the only field whose description says it lands in state), warns
  while such a value meets local or unencrypted state.
- **Provider versions** — `settings.provider_versions` pins a provider per project
  (Settings ▸ Provider versions, `settings_set`). The diagnostics say when a pin is outside
  the major the bundled schema describes, and otherwise run the curated-mapping schema
  check (moved from the `schema_check` test into `versions::mapping_findings`) over the
  types the project uses, naming any mapping that breaks. `ttg schema refresh
  --provider-version aws="~> 7.0"` builds an index for another major.
- **Probing the new majors** — every example exported with the new constraint and run
  through `tofu init && tofu validate`:
  - *AWS 6* (6.66.0): everything validated; the only finding was a deprecation —
    `data.aws_region.name` — in the Encryption Key's key policy and a workload's
    `rds-db:connect` ARN. Both now use the provider's `region` variable, which is valid on
    5 and 6 (6's replacement attribute, `region`, does not exist on 5). The default moves
    to `~> 6.0`; the examples also still validate on 5.100.
  - *google 7* (7.46.1): `enable_flow_logs` is gone from `google_compute_subnetwork`. The
    subnet mapping already wrote a `log_config` block, which alone turns flow logs on, so
    the argument is dropped. Every example validates on 6.50 and 7.46; the default moves to
    `~> 7.0`.
  - *azurerm 5* (5.7.0): `azurerm_storage_queue` takes `storage_account_id` instead of
    `storage_account_name` — `storage_account_id` exists on 4.81 too, so the mapping
    switched. Two breaks have no form valid on both: the private DNS zone virtual network
    link takes `private_dns_zone_id` instead of the zone name and resource group (7
    examples), and `azurerm_kubernetes_cluster` requires a `node_provisioning_profile`
    block (2 examples). The default stays `~> 4.0`.
  - The bundled schema index is rebuilt for aws 6.66.0, azurerm 4.81.0 and google 7.46.1:
    870,611 → 1,109,197 bytes (AWS 6 adds a `region` argument to every resource and 200
    resource types; google 7 adds 250).
- **`settings_set` refuses unknown keys** (R3.14), listing the valid ones, directly and
  inside `project_apply`; a refused call changes nothing.

## Round 3: cost estimate (gap report R3.12)

"Every reviewer's first question is what it costs": the CallScope author priced the
design by hand at £350-400 a month for the architecture document.

- **`ttg_codegen::cost`** estimates one provider's layer from bundled list prices
  (`definitions/prices/<provider>.toml`, USD, dated, one row per SKU, sources and
  cross-checks recorded per table; AWS us-east-1 / eu-west-1 / eu-west-2, Azure eastus /
  westeurope / uksouth, GCP us-central1 / europe-west1 / europe-west2) and editable usage
  assumptions. Models live per provider in code and resolve the concrete SKU through the
  mapping's own argument sources; free and unpriced types are listed with reasons in the
  price files, and a test refuses a mapped type that is in neither list nor priced.
  Details in ARCHITECTURE.md §6.0b and the refresh procedure in PRICES.md.
- **Assumptions** in `settings.cost_assumptions` (project values and per-entity
  overrides); a pool that scales to zero is priced at a node-hours-a-day figure (default
  8) rather than 24/7. `settings.cost_currency` shows a second currency at a fixed rate.
- **Budget diagnostic**: a warning on a `budget` whose `monthly_limit` the estimate
  exceeds, target provider only, cheap enough to run with the other diagnostics.
- **Surfaces**: the GUI's *View ▸ Cost estimate…* window, `ttg cost <project>
  [--provider] [--region] [--by entity|type] [--detail] [--json]`, and the MCP
  `cost_estimate { provider?, environment?, view?, group_by?, assumptions?, region? }`
  (named environments are accepted and, until the project has them, reported as ignored).
- **CallScope (AWS, eu-west-2)** comes to about $938 a month at the default assumptions
  (roughly £700), against the hand estimate of £350-400. The difference is in four
  places: the GPU pool at eight node-hours a day of g5.xlarge ($311; at two hours a day
  it is $78), the seven interface endpoints in two zones each ($113, easy to leave out
  by hand), Multi-AZ RDS ($122, half of that single-AZ) and the EKS control plane plus
  two t3.large nodes around the clock ($215). The budget entity's $600 limit is
  exceeded, so the warning fires; at two GPU hours a day and single-AZ the design is
  about $645.

## Round 3: views and metadata (gap report R3.22–R3.27)

The CallScope design's "Data flow" view had 31 hand-drawn flows that the workload links
already implied, labels overprinting where six or more flows met, a tidy that ranked by
dependency rather than by flow, notes left far from their anchors after a move, a legend
covering the corner of a fitted capture, and nothing in the model saying which
resources hold personal data, why a resource exists or who owns it.

- **Flows from links** (R3.22) — `ttg_codegen::dataflow`: one table decides, per
  relation kind and (where the kind is ambiguous) per source / target type, whether a
  link carries data, which way it moves and what to call it (the table is in
  ARCHITECTURE.md §4.1). `view_generate { kind: "data_flow" }` and View ▸ *Generate data
  flows from links* draw them between the view's visible resources, never duplicating a
  pair that already has a flow; one undo step. Of the Kubernetes manifest links,
  'Receives traffic from' a load balancer is a flow (the balancer forwards to the
  workload); 'Runs image from' and 'Schedules on' are not.
- **Tidy by flows** (R3.23) — `ttg_core::flow_layout`: a layered layout ranked by the
  longest path of flows in step order (cycles broken at the flow that closes them), the
  grouping boxes as non-overlapping swimlanes stacked in the order they first take part,
  four barycentre sweeps, and straight chains where there is room; always into the
  view's own layout. `layout_tidy { view, by: "flows" }` and View ▸ *Tidy by flows*.
  Flows now fan out along a shared side of a node, follow the obstacle-aware orthogonal
  routing when *Route around nodes* is on, stagger their step badges, and place labels
  clear of nodes, badges and each other: along the arrow first, then stepped off it
  perpendicular with a leader line when six or more flows meet.
- **Sequence and presentation** (R3.24) — `view_export { format: "sequence" }` / `ttg
  view export --format sequence` writes the numbered flows as a Mermaid `sequenceDiagram`
  (participants in order of appearance, logical nodes and boxes included, shared step
  numbers as `par` blocks, dashed flows as `-->>`). *Present steps* / **▶ Present** steps
  through them in the GUI with the arrow keys, highlighting the step's flows and ends,
  fading the rest and captioning the step with its labels and pinned notes; Esc leaves.
- **Notes and the legend** (R3.25) — the round-2 note placement moved to
  `ttg_core::view` (`note_offset_beside`, `arrange_notes`); `view_arrange_notes` and View ▸
  *Arrange notes* use it, and tidying a view (by links or by flows) runs it in the same
  undo step. Zoom to fit leaves the legend's strip on the right free when the legend is
  on (reserving its screen rect rather than drawing it inside the fitted bounds), so a
  fitted screenshot has nothing under it.
- **Classification** (R3.26) — an optional `classification` on every node and container
  (public, internal, confidential, personal, payment), set in the inspector or by
  `entity_update` (one entity, or every match of `select` in one undo step), drawn as a pill, filtered by `ViewFilter.classifications`, and listed in
  `project_get`, `project_summary` and the Markdown export. A flow's `data` says what it
  carries. `view_generate { kind: "personal_data" }` / View ▸ *Build "Where personal data
  goes"* builds or refreshes a view of the classified entities, what their data reaches
  one link on, and the flows between them, laid out by its flows. Posture rules keyed on
  the classification are left to a later package.
- **Description and owner** (R3.27) — on every entity too, emitted through the provider
  definition's new `default_tags.entity_arg`: `Owner` / `Description` tags on AWS and
  Azure (one line, cut to 256 characters), an `owner` label on Google Cloud, and a comment
  above the entity's first block everywhere. The mapping's tags and `extra` win over the
  entity's, which win over the project's. The Markdown export's new *Resources* table
  gains Classification / Description / Owner columns when any is set, and
  `project_summary` reports them. `examples/job-pipeline.ttg.json` gives its resource
  group and network an owner and a description, so the validate suite exercises the tags.

## Round 4: remote and file-based access (zipOS feedback TF-001, TF-019, TF-020, TF-022)

A Claude Code cloud session building zipOS could not use TerraTofu at all: the MCP
server only listened on the user's machine (TF-001). The fallback, writing a
`.ttg.json` for the user to open, was not documented as possible (TF-019). And locally,
writes behind the approval prompt timed out in the client while the server still held
them, so the caller could not tell whether a save or an export would happen (TF-020, and
the timeout bullet of TF-022).

- **Reachable from a cloud session** — the server can listen on another address
  (`bind`, default `127.0.0.1`) and accepts one public host name besides this machine's
  (`public_url`; rmcp's DNS-rebinding check stays on, now with that host allowed, and
  the OAuth pages use the same rule). Behind an HTTPS tunnel (cloudflared, Tailscale
  Funnel) it becomes a claude.ai custom connector. Connectors sign in with OAuth, so the
  app hosts a small OAuth 2.1 authorization server (`mcp/oauth.rs`): protected-resource
  and authorization-server metadata, dynamic client registration, authorization code
  with PKCE S256, rotating refresh tokens, revocation, and a 401 that points at the
  metadata. Each sign-in needs the user: a prompt in the window app ("Allow <client> to
  edit this project?", with a code the browser page also shows), or a one-time code that
  `--serve` prints and the user types into the page. Grants are stored hashed with the
  app's settings (or in `--grants FILE`), listed and revoked under Agent ▸ Settings &
  activity, and the bearer token keeps working for local clients. Design and security
  notes in MCP_PLAN.md §5; the connector steps in the README.
- **Approval-gated writes answer at once** — a write that needs Allow / Deny replies
  within seconds with `{status: "pending_approval", ticket, what, applied: false}` and
  runs only when allowed, against the project as it is then; `approval_status { ticket }`
  reports pending, applied (with the result), failed, denied or expired, and a ticket
  expires unapplied after a configurable time (default ten minutes). Other calls keep
  running while a prompt is open. Every call is answered within 45 s, and one that times
  out is withdrawn before the UI can start it (or waited for, if it already had), so the
  reply always says whether anything was applied.
- **The file as an interchange format** — docs/FILE_FORMAT.md documents the format, its
  versioning and the hand-over workflow; `examples/minimal.ttg.json` is the smallest file
  that exports cleanly; the schema carries an `$id` (the raw GitHub URL on master);
  `ttg check --schema` reports every schema problem with its line before the catalog
  diagnostics; and `project_import { json, replace }` loads a project from JSON as one
  undo step, refusing to replace a non-empty project without `replace` and asking the
  user first when it would discard unsaved changes.

## Round 4: edge (zipOS feedback TF-009, TF-010, TF-011, TF-016 item 5, the AAAA note)

The zipOS staging graph put CloudFront in front of an HTTPS ALB serving a Next.js app and
had to override the whole distribution in `extra`: POST was refused at the edge, there were
no path behaviours, timeouts or origin headers, and the origin was the ALB's own DNS name,
which the ALB's certificate cannot cover (a 502 on every request). The firewall left an
unused REGIONAL Web ACL behind and rate-limited everything with one rule, there was no
AAAA record, and Azure's `cdn` was classic Microsoft CDN, which cannot carry a WAF.

- **Mapping language** (MAPPING_FORMAT §2.6). `hop` takes a second step from the entities
  a relation reached (the CDN's origin load balancer, then the DNS Records that alias it),
  with `certificate_covers` keeping only names a certificate linked from the first hop
  covers (domain or alternative names, one-label wildcards). `for_each_item` emits a
  nested block per entry of a list item of the current row, `{ item = "…", min_count = n }`
  counts one, and `{ not = … }` negates. A link the *target's* mapping reads with
  `incoming` is expressed there: the source gets no `depends_on` (which would point the
  wrong way) and no "cannot express" warning.
- **Web ACL scopes** (TF-009). The REGIONAL `aws_wafv2_web_acl` is emitted only with a
  'Protects' link to a load balancer, the CLOUDFRONT one only when a CDN links here; a
  firewall that protects nothing is a warning on every provider and generates nothing.
  Azure likewise: an Application Gateway policy for a protected load balancer, a Front Door
  policy for a CDN. Google Cloud keeps one Cloud Armor policy, now attached by the global
  HTTPS load balancer's backend service (directly, or through the CDN in front of it).
- **Rate rules by path** (TF-011). `rate_rules` rows (name, limit per 5 minutes, path
  prefixes, method, block / count) replace `rate_limit_per_5min`, which older files load
  as one all-paths row (`ttg_core::project::load_str`). AWS: a rate-based rule per row
  whose scope-down statement is a `byte_match_statement` on `uri_path` STARTS_WITH, an
  `or_statement` of them for several prefixes, a method match, or an `and_statement` of
  both — with a `regex_match_statement` over the escaped prefixes when several prefixes
  meet a method, since a scope-down statement nests three levels at most. Cloud Armor: a
  `rate_based_ban` per row matched by `request.path.startsWith(…) || …` and
  `request.method`. Azure: `RateLimitRule` custom rules — `RequestUri` BeginsWith on an
  Application Gateway policy, a regex anchored after the host on Front Door, whose
  RequestUri is the full URL.
- **CDN for a dynamic site** (TF-010). `dynamic_site` (all seven methods, CachingDisabled),
  `allowed_methods` (read_only / all), path behaviours (pattern, managed cache policy,
  managed origin-request policy, methods, compress) as `ordered_cache_behavior`, origin read
  and keep-alive timeouts, origin headers, and a generated **secret origin header**. The
  managed policies are `data "aws_cloudfront_cache_policy"` /
  `"aws_cloudfront_origin_request_policy"` lookups by managed name, one per policy used,
  referenced as `…_cache["CachingOptimized"].id`. A load-balancer origin gets
  **AllViewerExceptHostHeader**: CloudFront then asks the origin for the origin's own name,
  which is the one item 3 makes sure the certificate covers, so TLS to the origin always
  works. AllViewer forwards the viewer's Host, which the origin's certificate must cover as
  well (and which an app that checks Host against Origin — Next.js Server Actions — wants);
  that is the `forward_host_header` option, checked against the certificate (an error when
  it does not cover the custom domain). With the secret header on, the ALB listener's
  default action becomes a 403 fixed response and an `aws_lb_listener_rule` forwards only
  requests carrying the header (a `random_password` without special characters, which a
  listener rule would read as wildcards).
- **Origin name the certificate covers** (TF-010). An https load balancer origin is reached
  by the `fqdn` of a DNS Record that aliases it and whose name the load balancer's
  certificate covers; without one it is an error that says to add `origin.<domain>` and the
  matching alternative name. An http load balancer is reached by its own name, http-only,
  with a warning. A built-in check also warns when a record points at a CDN under a name
  that is not the CDN's custom domain.
- **AAAA** (§5 table). `dns_record` takes AAAA: a Route 53 alias of a CloudFront
  distribution, an `azurerm_dns_aaaa_record` alias of the Front Door endpoint, and on Google
  Cloud an IPv6 global forwarding rule on the HTTPS load balancer's proxy (`grule6`) or an
  IPv6 address a bucket CDN reserves. An AAAA alias of an AWS or Azure load balancer is an
  error: both are IPv4-only here.
- **Azure Front Door** (TF-016 item 5). `cdn` on Azure is Front Door Standard/Premium:
  profile, endpoint, origin group, origin, a default route and one route per path behaviour
  (CloudFront's cache policies become a route `cache` block or none), a managed-certificate
  custom domain and its association, and a rule set that adds the origin headers. The tier
  follows the firewall: managed rule sets need **Premium**, so the profile and the
  `azurerm_cdn_frontdoor_firewall_policy` are Premium exactly when a linked firewall has
  managed rule groups (the type requires them), Standard without a firewall; an
  `azurerm_cdn_frontdoor_security_policy` attaches it. A DNS Record aliasing the CDN is an
  A / AAAA alias of the endpoint or a CNAME to its host name, plus the `_dnsauth` TXT record
  holding the custom domain's validation token. The cost estimate prices Front Door's base
  fee by tier, data out and requests (`front_door` rows in `definitions/prices/azure.toml`).
- **Google Cloud CDN**. In front of a load balancer, Cloud CDN is `enable_cdn` plus a
  `cdn_policy` (USE_ORIGIN_HEADERS for a dynamic site, CACHE_ALL_STATIC otherwise) on the
  global HTTPS load balancer's backend service, whose timeout follows the CDN's origin
  timeout. Path behaviours, origin headers and the secret header have no counterpart there
  and say so.

`examples/edge-dynamic.ttg.json` is the zipOS edge in miniature (dynamic site, a static
path behaviour, per-path rate rules, a secret origin header, an `origin.` record the
certificate covers, A and AAAA aliases) and validates on all three providers.

## Round 4: containers (zipOS feedback TF-005 to TF-008, TF-016, TF-021 item 5)

Building zipOS's AWS staging graph needed `$raw` container definitions, a native shared
cluster, a second role wired in through overrides, native IAM policies and a native task
definition for the migration — and the alarm on the worker watched an orphaned cluster.

- **Container Environment** (new container type): `aws_ecs_cluster` with Container
  Insights, `azurerm_container_app_environment`, logical on Google Cloud. Apps and jobs
  inside it share it; outside one they keep their own cluster / environment.
- **App configuration** on Container App: `env` and `secrets` rows (a linked Secret, a
  JSON key per variable on AWS), health check, stop timeout, CPU architecture, image from a
  linked registry with `repository` and `image_tag`, written for ECS, Container Apps and
  Cloud Run alike.
- **Roles**: 'Task role' and 'Execution role'; the execution policy follows the execution
  link (an incoming condition with `where`), the execution role may read the injected
  secrets, and 'Uses' / 'Reads' / 'Sends to' links to buckets, secrets, queues, topics and
  IAM-authenticated databases become the task role's least-privilege policy.
- **Load balancer → Container App**: `ip` target group, health-check path, deregistration
  delay, the service's `load_balancer` block; a serverless NEG on Google Cloud.
- **Background worker** flag and **Container Job** type (ECS task definition plus a run
  configuration output, Container Apps job, Cloud Run job), priced per run
  (`job_runs`, `job_run_minutes`); Fargate on arm64 priced at the Graviton rates.
- **Mapping language**: list sources from table fields (`{ for_each_field, each }`),
  `connection = "KEY"` on relation sources, `where` on relation conditions, `linked` and
  `absent` on item conditions; `depends_on` lists are flattened and `concat` drops empty
  literals. Identical grants are written once.
- **Diagnostics**: a warning when an `extra` argument orphans one of the entity's own
  blocks; the reachability note names the link the source type actually offers.
- `examples/containers.ttg.json` exercises all of it and validates on all three
  providers. The rebuilt zipOS graph has no `$raw` container definition, no native ECS
  resource and no execution-role override; its AWS export has one cluster, a task role
  without the execution policy, the load balancer forwarding to web by IP and the worker
  alarm on the shared cluster.

## Explicitly still out of scope

Running `plan`/`apply`, live-account access, multi-user collaboration, drift detection
and state visualisation remain non-goals for these phases. Cost is an *estimate* from
bundled list prices; reading real bills or live pricing APIs is out of scope.
