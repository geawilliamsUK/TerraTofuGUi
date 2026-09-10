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

Open on GCP: an HTTP(S) load balancer variant (URL map + target proxy), Private Service
Connect for private endpoints, and provider-level project creation (`google_project`).

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

## Explicitly still out of scope

Running `plan`/`apply`, live-account access, multi-user collaboration, cost estimation,
drift detection and state visualisation remain non-goals for these phases.
