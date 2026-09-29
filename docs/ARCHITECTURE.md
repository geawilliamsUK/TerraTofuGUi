# TerraTofu GUI — Architecture

Version 1 of this document accompanies the Phase 1 vertical slice. Every decision below is
implemented in the code in this repository unless it is explicitly marked *Phase 2+*.

## 1. Framing (restated, because it shapes everything else)

There is **no universal HCL**. An S3 bucket, an Azure Storage Account and a GCS bucket are three
different resource types with different arguments, naming rules and identity models, and no
template can make one file deploy to all three. This project therefore does not try.

What it does instead:

1. The diagram is the single source of truth. It is stored as an **abstract Intermediate
   Representation (IR)** of resource *intent*: "virtual network", "subnet", "compute instance",
   "object storage", "IAM role". Nothing in the IR names a Terraform resource type.
2. Every abstract type is mapped **per provider** to that provider's concrete resource block(s) by a
   **data-driven definition file** (TOML). Adding provider coverage means adding or editing a TOML
   file, not writing Rust.
3. "Export for all providers" produces **one complete, independent, deployable project directory
   per provider** (`out/aws/`, `out/azure/`, later `out/gcp/`) from the same diagram. The
   directories share nothing. They are not one portable file, and the code path that produces them
   is the single-provider exporter run once per provider.

Where a provider cannot express something the diagram says, the gap is surfaced twice: as a badge
on the node at design time, and as an entry in a generated `MANUAL_STEPS.md` at export time.

## 2. Repository layout and module boundaries

```
Cargo.toml                 workspace
definitions/               DATA, not code — the resource mapping catalog (TOML)
  providers/aws.toml       provider block, required_providers entry, provider-level variables
  providers/azure.toml
  resources/*.toml         one file per abstract resource type
  prices/<provider>.toml   bundled list prices for the cost estimate (see PRICES.md)
crates/
  ttg-core                 IR types, project file (JSON), structural validation, dependency graph
  ttg-catalog              loads + validates definition files; typed schema for the mapping format
  ttg-codegen              IR + catalog -> HCL files, MANUAL_STEPS.md, Terraform/OpenTofu toggle,
                           design-time diagnostics, optional `validate` shell-out, multi-provider bundle,
                           view filters and the Markdown / Mermaid renderer for a view
  ttg-cli                  headless `ttg export` / `ttg check` / `ttg view export` — used for CI and by contributors
  ttg-app                  egui/eframe desktop application
examples/                  sample .ttg.json projects
docs/                      this file, MAPPING_FORMAT.md, PHASE2_PLAN.md
```

Dependency direction is strictly one way: `ttg-app -> ttg-codegen -> ttg-catalog -> ttg-core`.
`ttg-core` knows nothing about providers or HCL. `ttg-catalog` knows nothing about the canvas.
`ttg-codegen` never touches egui. A contributor adding a resource type touches only
`definitions/`. A contributor adding a provider touches `definitions/` and, at most, the small
tool/provider profile in `ttg-codegen` if that provider needs a special `provider {}` block shape
(Phase 1 already expresses `provider "azurerm" { features {} }` in data, so even that is unlikely).

## 3. Crate choices

| Concern | Crate | Why |
|---|---|---|
| GUI | `egui` + `eframe` 0.33 (pinned: newest release whose MSRV, 1.88, matches the toolchain) | Immediate-mode fits a canvas editor: pan/zoom, drag, bezier edges and nested containment are ~200 lines of painter calls with no retained widget tree to keep in sync with the model. Prior art (`egui_node_graph`, `egui_graphs`) validated the approach; we do not depend on them because neither models *containment* and both impose their own node/port data model, which would fight the IR. **`iced` was considered and rejected**: its Elm-style retained architecture and `canvas` widget make free-form drag/drop and nested hit-testing noticeably more ceremony, and its ecosystem for this exact use case is thinner. Decision is final for v1. |
| HCL output | `hcl-rs` 0.19 | Provides `Body`/`Block`/`Attribute`/`Expression`/`Traversal` value types and a formatter. All HCL is built as a typed tree and formatted by the crate; the codebase contains **no string-templated HCL**. We format one top-level block at a time and join them, because the value-level API cannot carry comments and we want file headers and per-node comments. |
| Graph | `petgraph` 0.8 | Cycle detection (`toposort` / `is_cyclic_directed`) and dependency ordering across explicit edges *and* containment. Used in `ttg-core::graph`. |
| Serialization | `serde` + `serde_json` (project file), `toml` (definitions) | Standard. `serde_json` with `preserve_order` so the project file has a stable key order. |
| IDs | `uuid` v4 (truncated) | Short, collision-safe IDs that are still readable in diffs (`subnet-3fa2b9c1`). |
| Definition schema checks | `regex` | Provider-specific name validation rules (e.g. Azure storage account names) are regexes in the TOML. |
| Bundling | `zip` | Optional zip of the multi-provider export. Folder tree is the primary output. |
| Dialogs | `rfd` | Native open/save dialogs. |

### 3.1 Definition file format: TOML

Options were JSON, TOML and RON. **TOML** was chosen because:

- Comments. Definition files are documentation for the people who maintain them; JSON forbids
  comments and RON's are unfamiliar to non-Rust contributors.
- The audience is cloud engineers, not Rust developers. TOML is already in their toolbelt
  (Cargo, pyproject, Hugo, many CLIs); RON is Rust-only.
- Arrays of tables (`[[fields]]`, `[[providers.aws.blocks]]`) read naturally as ordered lists,
  which matters because block emission order and field order in the property panel are meaningful.
- It diffs line-per-key in git.

### 3.2 Project save format: JSON

A `.ttg.json` file. JSON rather than TOML because the file is written by the app, not humans, and
JSON tooling outside Rust (jq, JSON Schema, web viewers) is universal. Diff-friendliness is
engineered in:

- Top-level `schema_version` integer; the loader migrates forward, never silently.
- Entities are stored in maps keyed by ID (`BTreeMap`), so order is stable across saves.
- Containment is stored **once**, on the child (`parent`). Moving a node between containers changes
  one line. Container child lists are derived at load time, never persisted.
- Positions are rounded to integers so a one-pixel wobble does not produce a diff.
- Pretty-printed, two-space indent, trailing newline.

## 4. Intermediate Representation

All types live in `ttg-core::ir`. Field names below are the serialized names.

### 4.1 `Project`

```jsonc
{
  "schema_version": 1,
  "name": "demo",
  "settings": {
    "tool": "opentofu",                 // "terraform" | "opentofu"
    "target_provider": "aws",           // provider currently selected in the UI
    "provider_settings": {              // values for provider-level variables (see providers/*.toml)
      "aws":   { "region": "eu-west-2" },
      "azure": { "location": "uksouth" }
    },
    "backend": null,                    // or { "type": "s3", "args": { "bucket": "...", "region": "..." } } (§6.5)
    "state_encryption": false,          // OpenTofu-only; with Terraform it only produces a warning
    "state_encryption_key": "key-main", // optional: the Encryption Key entity that encrypts the state
    "provider_versions": { "aws": "~> 6.0" },  // optional pins; default = the definition's constraint
    "tags": { "Project": "demo" },      // put on every generated resource (see MAPPING_FORMAT §4.3)
    "kubernetes_manifests": false       // also write k8s/ (§6.4); omitted from the file while false
  },
  "containers": { "<id>": Container, ... },
  "nodes":      { "<id>": Node, ... },
  "edges":      [ Edge, ... ],
  "views":      [ { "name": "Messaging", "filter": { "categories": ["serverless"], "depth": 1 } } ]  // optional
}
```

`views` are saved canvas views, each a small document about the diagram:

- a **filter** (`categories`, `relations`, `focus` + `depth`, `hidden`, `only`,
  `containers`, plus `providers` = provider layers, `origin` = `all` | `curated` |
  `native`, `types` = abstract type ids, `name_glob` = case-insensitive `*` / `?` glob on
  the display name, `classifications` = only entities classified as one of these, and
  `hide_edges` to draw no structural links at all);
- an optional `layout` (`positions` / `sizes` keyed by entity id that override the shared
  ones while the view is active);
- a `description` and a `legend` flag;
- annotations: `groups` (labelled boxes with `position`, `size`, optional `color`;
  nesting and membership are geometric), `flows` (labelled arrows between
  `{ "entity": id }` / `{ "group": id }` / `{ "logical": id }` ends, optionally `dashed`,
  with an optional `step` number, `color` and `data` — what travels along it, in words),
  `notes` (`title`, `body`, `size` and
  either a free `position` or an `anchor` — any flow end, or `{ "flow": id }` — with
  `position` read as the offset from it) and `logicals` (annotation-only nodes: `name`,
  `icon`, `subtitle`, `position`, `size`).

All of it affects only what the GUI draws, never what is generated.
`ttg_core::view` holds the geometry the canvas and the document renderer share: where a
view puts an entity (in a view with its own layout a container is the padded bounding box
of the members it still shows), which entities and logical nodes a box holds, where an
anchored note lands (and where a note placed beside its anchor goes: `note_offset_beside`
/ `arrange_notes`, shared by `view_note_add`, `view_arrange_notes` and tidy), and how
boxes nest. `ttg_core::flow_layout` lays a view out by its flows: a layered drawing
ranked by the longest path of flows taken in step order (a depth-first walk from the
earliest steps drops the flows that close a cycle), the view's grouping boxes as
swimlanes stacked in the order their members first take part, four barycentre sweeps
against crossings, and each item aiming for the height of what flows into it; positions
land in the view's own layout and the boxes are refitted around their members.
`ttg_codegen::views` holds the filter evaluation (`visible_set`, used by the canvas, the
view bar, `view_get` and the CLI alike) and the Markdown / Mermaid flowchart / Mermaid
`sequenceDiagram` renderers behind `ttg view export` and the `view_export` tool.
`ttg_codegen::dataflow` derives flows from links — one table (`dataflow::rule`) decides
per relation kind, and per source / target type where the kind is ambiguous, whether a
link carries data, which way it moves and what to call it:

| relation | when | data moves | label |
|---|---|---|---|
| `sends_to` | always | source → target | sends to |
| `dead_letters_to` | always | source → target | dead letters (dashed) |
| `logs_to` | always | source → target | logs (dashed) |
| `calls` | always | source → target | calls |
| `reads` | source is a workload | target → source | read by |
| `attribute_reference` | workload → queue | target → source | consumed by |
| `attribute_reference` | workload → database, cache, bucket, file system, table | source → target (both ways) | uses |
| `attribute_reference` | CDN → bucket or load balancer | source → target | origin |
| `attachment` | anything → file system | target → source (both ways) | mounted by |
| `attachment` | load balancer → instance or cluster | source → target | forwards to |
| `attachment` | scaling group or Kubernetes workload → load balancer | target → source | forwards to |
| `attachment` | audit trail → bucket | source → target | writes to |

Everything else — `network_membership`, `iam_binding`, `encrypted_with`, `depends_on`,
containment, security groups, certificates, a database reading its own password, a
function's code bucket, what an alarm watches, a workload's image registry and node
pool — is structure and produces no flow.
"Both ways" marks a read/write link, which is what the "Where personal data goes" view
(`dataflow::personal_data_view`) follows from the entities classified personal or
payment.

### 4.2 `Node`

```jsonc
{
  "id": "subnet-3fa2b9c1",
  "name": "web",                        // display name; slugified into the HCL local name
  "resource_type": "subnet",            // abstract type — must exist in the catalog
  "config": { "cidr_block": "10.0.1.0/24" },          // abstract (provider-neutral) field values
  "provider_config": {                                // provider-specific extra fields
    "aws":   { "map_public_ip": true },
    "azure": { }
  },
  "position": { "x": 320, "y": 180 },
  "size": { "w": 220, "h": 80 },        // optional; omitted = default node size
  "parent": "vnet-1c9e77aa",            // container id or null
  "manual": false,                      // "external / manage by hand" flag
  "providers": ["azure"],               // optional: provider layers this entity is part of (absent = all)
  "extra": { "aws": { "main": { "force_destroy": true } } },  // optional: extra provider arguments per block
  "classification": "personal",         // optional: public | internal | confidential | personal | payment
  "description": "Call recordings",     // optional: why it exists
  "owner": "Platform team"              // optional: who looks after it
}
```

**What an entity is for.** `classification`, `description` and `owner` are on every
node and container (like `providers`, not per-type catalog fields), all optional and
absent from the file when unset. The classification drives the view filter and the
"Where personal data goes" view (posture rules on it are a later package). The owner
and description reach the HCL through the provider definition's
`default_tags.entity_arg` (MAPPING_FORMAT.md §4.3): an `Owner` and a `Description` tag on
AWS and Azure (one line, cut to 256 characters), an `owner` label on Google Cloud (label-
safe, 63 characters; no description, which a label cannot hold), and on every provider a
comment above the entity's first block with the full description, the owner and the
classification. Precedence, most specific first: the mapping's own tags and `extra` ›
the entity's Owner / Description › the project-wide `settings.tags`.

**Extra arguments.** `extra` is provider id → block key → argument name → JSON value,
merged into the generated block after the mapping's own arguments (yours win). Nested
blocks are objects or lists of objects, decided by the bundled provider schema
(`ttg-schema`); `{"$ref": {"entity": …, "attr": …}}` becomes a traversal to another
resource and `{"$raw": "…"}` a raw expression. Native provider resources (type id
`native:<provider>:<tf type>`) are synthetic catalog entries whose single block takes
every argument from `extra`; the catalog creates them on demand (`Catalog::ensure_native`).

**Provider layers.** One diagram serves every provider. A node, container or edge may
carry `providers`; a provider's *layer* is the project minus entities not tagged for it,
minus entities whose abstract type is provider-scoped elsewhere (auto-tagged), minus
entities a definition check with `severity = "omit"` fired on (the provider cannot express
that one entity, so it is left out with a warning rather than blocking the export — see
MAPPING_FORMAT.md §2.6), with the
contents of a dropped container re-parented to the nearest kept ancestor. Codegen,
diagnostics and reachability all run on the layer (`Project::layer`,
`ttg_codegen::layers`); what a layer leaves out is reported as `Layer` diagnostics so
divergence between providers is explicit. The GUI's concrete display mode dims
off-layer entities and tags anything that is not on every provider.

Values in `config` / `provider_config` are a small tagged-free JSON subset: string, bool, integer,
float, list of strings. Field *types* (including `cidr`, `enum`) live in the definition, not the
project file.

### 4.3 `Container`

```jsonc
{
  "id": "vnet-1c9e77aa",
  "name": "main",
  "container_type": "virtual_network",  // abstract type with kind = "container" in the catalog
  "config": { "cidr_block": "10.0.0.0/16" },
  "provider_config": { "aws": {}, "azure": {} },
  "position": { "x": 100, "y": 100 },
  "size": { "w": 640, "h": 400 },
  "parent": "rg-88f0e2b1",              // containers nest (VPC inside Resource Group)
  "manual": false
}
```

A container is a resource that can hold other resources. It is deliberately not a separate
"visual group" concept: a VPC container *is* the `aws_vpc` / `azurerm_virtual_network` resource, and
membership in it *is* the `network_membership` relation for the children (see 4.4). Child ids are
available through `Project::children_of(id)`.

Container types in Phase 1: `resource_group` (Azure: `azurerm_resource_group`; AWS: *logical* —
emits nothing, children are flattened, no warning because the definition says so explicitly),
`virtual_network` (AWS `aws_vpc`, Azure `azurerm_virtual_network`, GCP `google_compute_network`).
There is no project container: on GCP the project is the `project` provider variable and
`resource_group` is logical, exactly as on AWS.

### 4.4 `Edge`

```jsonc
{ "source": "vm-0d1f", "target": "role-9a2c", "relation": "iam_binding",
  "layout": { "source": { "side": "right", "offset": 40 }, "target": {} } }   // optional
```

`layout` is purely visual: each end may pin a `side` (`left|right|top|bottom`; absent =
automatic) and an `offset` along that side (-100..100, 0 = centre). An edge whose relation is
`via_parent` and whose target is an enclosing container of the source is *redundant*: the
codegen ignores it, the canvas neither draws nor creates it, and diagnostics report it as info.

Direction: **source depends on / references target**. Relation kinds:

| kind | meaning | example |
|---|---|---|
| `network_membership` | source lives inside target's network | subnet → vnet, instance → subnet |
| `attribute_reference` | source's config references an attribute of target | instance → object storage |
| `iam_binding` | source assumes / is bound to target identity | instance → IAM role |
| `attachment` | source is attached to / registered with target | route table → subnet |
| `sends_to` | source publishes messages to target | function → queue |
| `reads` | source reads target's value | function → secret |
| `logs_to` | source writes its logs to target | function → log group |
| `encrypted_with` | source is encrypted at rest with target's key | bucket → encryption key |
| `dead_letters_to` | messages the source could not deliver go to target | queue → dead-letter queue |
| `calls` | source makes requests of target; documentation only | workload → workload |
| `depends_on` | pure ordering, no attribute | anything → anything |

`calls` generates nothing: no mapping consumes it, it adds neither `depends_on` nor a manual step,
and it is left out of the dependency graph, so two services calling each other is not a cycle.

Containment creates an *implicit* `network_membership` edge from child to container when the
definition marks the relation `via_parent = true`. Explicit edges are still allowed for the cases
where drawing a node inside a container is not what the user wants.

### 4.5 Validation (`ttg-core::validate`)

Structural checks that do not need the catalog: unique ids, unique names, edge endpoints exist,
parent exists and is a container, no containment cycles, no dependency cycles (petgraph over edges
∪ containment). Catalog-aware checks (required fields, types, regex rules, allowed parents,
relation cardinality, provider coverage) are in `ttg-codegen::diagnostics` and are the same
function the GUI uses for badges and the exporter uses as its gate.

## 5. Resource mapping system

The full format is specified in [MAPPING_FORMAT.md](MAPPING_FORMAT.md). The shape, in brief:

```toml
schema_version = 1

[resource]
type = "subnet"                      # abstract id (file name matches)
category = "network"                 # palette category and default .tf file
display_name = "Subnet"
kind = "node"                        # or "container"
allowed_parents = ["virtual_network"]

[[fields]]                           # abstract, provider-neutral config
name = "cidr_block"
type = "cidr"
required = true

[[relations]]                        # which edges this node may be the source of
kind = "network_membership"
targets = ["virtual_network"]
cardinality = "one"
via_parent = true

[providers.aws]
status = "full"                      # full | partial | logical
[[providers.aws.blocks]]
key = "main"
resource = "aws_subnet"
[providers.aws.blocks.args]
vpc_id     = { relation = "network_membership", attr = "id" }
cidr_block = { field = "cidr_block" }
tags       = { object = { Name = { field = "name" } } }

[providers.azure]
status = "full"
[[providers.azure.blocks]]
key = "main"
resource = "azurerm_subnet"
[providers.azure.blocks.args]
name                 = { field = "name" }
resource_group_name  = { ancestor = "resource_group", attr = "name" }
virtual_network_name = { relation = "network_membership", attr = "name" }
address_prefixes     = { field = "cidr_block", wrap = "list" }
```

Key properties of the design:

- **One abstract type → N concrete blocks per provider.** An IAM role on AWS is
  `aws_iam_role` + `aws_iam_instance_profile`; a compute instance on Azure is
  `azurerm_network_interface` + `azurerm_linux_virtual_machine`. Blocks reference each other with
  `{ self_block = "nic", attr = "id" }`.
- **Argument values are declarative sources**, not code: literal, abstract field, provider field,
  provider variable, template, lookup table, relation reference, ancestor reference, self-block
  reference, nested object/list, function call. String sources take a `transform`
  (`kebab`, `slug`, …) so display names can become provider-legal resource names. The resolver
  in `ttg-codegen::emit` turns each into an `hcl::Expression`. There is a `raw` escape hatch
  (parsed through hcl-rs, so still a typed tree), documented as a last resort.
- **Primary block**: the block keyed `main` (or the first block) is what other resources
  reference by default; `block = "profile"` targets a secondary block explicitly.
- **Nested blocks** (`identity {}`, `os_disk {}`) are declared with the same source language and can
  be conditional on a relation or a field (`when = { relation = "iam_binding" }`).
- **Relation references resolve to attribute traversals** (`aws_vpc.main.id`), which is what gives
  Terraform its implicit dependency order. `depends_on` is emitted only for `depends_on` edges and
  for edges the mapping did not consume.
- **Coverage is declared, then checked.** `status = "partial"` plus `[[providers.X.manual_steps]]`
  is how a definition says "this deploys, but you must finish it by hand". A provider section that
  is missing altogether means "unmapped".
- **Provider-level facts live in `definitions/providers/*.toml`**: registry namespace/name, version
  constraint, `provider {}` block arguments, provider-level variables (`region`, `location`,
  `subscription_id`) and their defaults.

Validation of the *definitions themselves* happens at catalog load (`ttg-catalog::validate`):
unknown field types, sources that reference undeclared fields or relations, `self_block` keys that
do not exist, ancestors that are not container types, duplicate abstract types. A broken definition
is reported with file and path and the catalog refuses to load it, so a bad contribution cannot
produce silently wrong HCL.

## 6. Code generation pipeline (`ttg-codegen`)

```
Project + Catalog + provider + tool
  │
  ├─ diagnostics::run          gate: errors abort, warnings become MANUAL_STEPS entries
  ├─ emit (plan phase)         evaluate `when` -> the set of (entity, block) pairs to emit
  ├─ emit (resolve phase)      ArgSource -> hcl::Expression, collects variables + manual steps
  ├─ graph (petgraph)          topological order of emissions; cycle = error
  ├─ k8s::generate             only with settings.kubernetes_manifests: k8s/*.yaml, render
  │                            scripts, k8s/README.md, plus the k8s_* outputs they read (§6.4)
  ├─ files::assemble           network.tf, compute.tf, storage.tf, iam.tf, variables.tf,
  │                            outputs.tf, versions.tf (versions + backend + encryption),
  │                            providers.tf, MANUAL_STEPS.md, README.md
  ├─ state::bootstrap_roots    [bootstrap/] — the state store and key, generated by the same
  │                            pipeline from a small project of their own (§6.5)
  ├─ tool::Profile             the ONLY place Terraform and OpenTofu differ
  └─ write / bundle            owned files replaced, stale ones removed (§6.7); single dir, or
                               out/<provider>/ per provider (+ optional zip)
```

### 6.0 Diagnostics layers

Three layers, all surfaced through the same `Diagnostic` list:

1. **Structural** (`ttg-core::validate`): ids, names, containment, dependency cycles.
2. **Definition-driven** (`ttg-codegen::diagnostics`): required/typed fields, allowed
   parents, relation cardinality and `min_targets`, required ancestors, provider coverage,
   `unique_scope`, `required_unless_relation`, `expects_incoming`, and the
   `[[providers.<id>.checks]]` declared in each TOML file.
3. **Built-in network checks** (`diagnostics::network_checks`): the few rules that need
   several resources at once and therefore know the network-shaped abstract types by
   name: subnet CIDRs inside their network and non-overlapping, AWS zones inside the
   configured region, one route table per subnet, a NAT gateway's subnet routing to an
   internet gateway, functions in subnets having an outbound route, managed
   services (`network_agnostic`) drawn inside a network being informational only, a
   security group that rules name as their source although nothing carries it
   (warning), and an AWS interface endpoint with private DNS whose security group admits
   nothing in its network (error: it answers for the service's name everywhere there).
   The last two use reachability's notion of membership.

`diagnostics::run` answers for one target provider. `diagnostics::other_providers` adds
the *other* providers' errors, each downgraded to a warning, tagged with the provider it
came from (`Diagnostic::provider`) and prefixed `[<Provider>] … (would block the
<Provider> export)`; `run_all` is the two lists concatenated. They answer "this will block
the Azure export" while the target is AWS. Exports still block on the target provider's
own errors only, and an entity that is off another provider's layer produces nothing for
it. Running three providers costs roughly three times one, so the app computes the other
providers lazily — only while the diagnostics panel is open or the agent's `diagnostics`
tool asks — and caches the result until the project changes.

### 6.0a Reachability (`ttg-codegen::reach`)

A separate analysis over the same IR answers "where can this resource send traffic, what
is exposed, can A talk to B?". It reuses the routing facts from the network checks and a
small per-provider policy (AWS security groups deny by default in both directions; Azure
NSGs allow VNet-internal and outbound traffic by default). Results are `Ok`, `Blocked`
with a reason, or `Unknown` when the diagram cannot decide (for example no security group
on the target on AWS). Managed services are reached over the provider API, so the path is
"a way out of the network" plus "a link that grants permission"; networked targets need
the same network, a matching ingress rule (CIDR or source group and port) and, on AWS, an
egress rule on the source. The GUI's overlay and the `ttg reach` command both call it.

A resource may carry several groups, and any of them may allow the traffic. Kubernetes
resources have no network presence of their own: a cluster carries its linked groups
plus *its own* group (the cluster's id stands for the security group the provider makes
for it, which admits its members and lets everything out — what a rule whose source is
the cluster means, and which can admit traffic but never refuses it, since the provider's
controllers add rules to it the diagram cannot see); a node pool carries its own linked groups or else its cluster's, and
always the cluster's own; a workload carries the groups and subnets of the pools it is
linked to, or else of its cluster.

### 6.0b Cost estimate (`ttg-codegen::cost`)

`cost::estimate(project, catalog, provider, region?)` prices the provider's layer — what
its export would create — and returns an `Estimate`: one `Line` per entity (status
priced / free / not estimated, the `Charge`s that make it up as quantity × unit price,
the assumptions it used and where each value came from, notes), totals per abstract type,
per saved view and per labelled group of each view (`views::visible_set` and
`ttg_core::view::group_members`), the price date, and a caveat every surface shows. It
takes a plain `&Project`, so a caller that resolves a named environment into a project of
its own can estimate that. Three parts, maintained apart:

- **Prices** — `definitions/prices/<provider>.toml`, compiled in (`include_str!`) so the
  estimate works offline: USD list prices for a few regions (the providers' defaults and
  the UK / EU ones), one row per SKU, each table with its source and a note of what was
  cross-checked and how. A region without a column falls back to the list's reference
  region, and the estimate says so. `docs/PRICES.md` is the refresh procedure.
- **Models** — `cost/{aws,azure,gcp}.rs`, one small function per priced abstract type.
  They read the same fields the mappings read and resolve the concrete SKU through the
  mapping itself (`Ctx::arg("main", "instance_class")` evaluates the block argument's
  literal / field / provider-field / lookup-table / `if` sources, so `size = small`
  becomes `db.t3.micro` and an instance-class override wins exactly as in the export).
  Types that cost nothing or are not priced are listed in the price file's `[free]` /
  `[not_estimated]` tables with the reason shown to the user; a test fails when a mapped
  type is in none of the three places, or when a model asks for a row the list lacks.
- **Assumptions** — usage the diagram does not say (`cost::ASSUMPTIONS`: GB per bucket,
  log GB a month, node-hours a day of a pool that scales to zero, requests, …), with
  defaults in code, project values in `settings.cost_assumptions.values`, per-entity
  overrides in `settings.cost_assumptions.entities[<id>]` and, for one MCP call, the
  tool's `assumptions`. A pool with `min_nodes = 0` is priced at the node-hours-a-day
  assumption instead of around the clock. `settings.cost_currency` adds a display
  currency at a fixed, dated rate; prices stay USD.

`cost::budget_diagnostics` adds a warning (`Code::Cost`) on each `budget` entity whose
`monthly_limit` the estimate exceeds. It runs inside `diagnostics::run` for the target
provider only — the other-provider list keeps errors alone — because it is cheap: on the
84-resource CallScope design it takes about 0.5 ms against about 6 ms for the whole
diagnostics run (dev profile), and it is skipped outright without a budget. Surfaces: the
GUI's Cost window (`cost_panel.rs`: the table by resource / type / view, the assumptions
editable in place as undoable edits, the price date in a colour of its own), `ttg cost`
and the MCP `cost_estimate` tool.

### 6.1 Manual and unmapped nodes

A node flagged `manual`, or one with no mapping for the target provider, is **not emitted**.
Every reference to it from another node becomes an input variable
(`variable "role_web_arn"` with a description naming the node), so the generated project still
passes `validate` and the operator supplies the value. `MANUAL_STEPS.md` lists, per node: what to
create, which of its configured values to use, and which variables to fill in afterwards.
Partial mappings append their declared steps. Unconsumed edges are listed as "link by hand".

### 6.2 Terraform / OpenTofu toggle

`ttg-codegen::tool::Profile` is a small struct with: binary name, display name, file header,
`required_version` constraint, provider source address rendering
(`hashicorp/aws` vs `registry.opentofu.org/hashicorp/aws`), and whether the tool can encrypt
its state (only OpenTofu can; what the `encryption` block contains is `state`'s business,
§6.5). Resource, variable and output generation never branch on the tool. Registry lookups
for schema reference default to the OpenTofu registry.

### 6.3 `validate` shell-out

`ttg-codegen::validate` looks for `terraform` / `tofu` on `PATH`. If found it runs
`init -backend=false -input=false` then `validate` in the export directory and returns the output.
If not found it returns `BinaryNotFound` and the GUI shows the exact command to run. The app has
no runtime dependency on either binary. `init` can take minutes, so the GUI never runs this on the
UI thread: `TtgApp::run_validate` starts one background thread for the exported providers and
`poll_validate` collects the outcomes through an `mpsc` channel once a frame, showing "validating…"
per provider meanwhile. A frozen UI is not just an unresponsive window — it also stops the MCP
command queue draining, which is what made an agent's timed-out call get applied late.

### 6.4 Kubernetes manifests (`ttg-codegen::k8s`)

With `settings.kubernetes_manifests` on (Settings ▸ Output, `ttg export --k8s`, MCP
`export_run { k8s: true }`), one provider's export directory looks like this:

```
out/aws/
  *.tf, README.md, MANUAL_STEPS.md     as above; outputs.tf gains the k8s_* outputs
  k8s/
    00-namespaces.yaml                 the workloads' namespaces (default / kube-* left out)
    <workload>.yaml                    ServiceAccount, [PersistentVolume + claim per mount],
                                       Deployment, [Service], [TargetGroupBinding (AWS)],
                                       [ScaledObject + TriggerAuthentication | HPA]
    render.sh, render.ps1              fill the ${k8s_*} tokens from `<tool> output`
    README.md                          apply flow, prerequisites, the env-var table
    rendered/                          written by the render scripts; never touched by export
```

The module reads only the workload's own fields and links (those marked
`manifests = true`, MAPPING_FORMAT.md §1.1–1.2) and knows the per-provider identity
conventions (which annotation or label a ServiceAccount needs, which CSI driver mounts a
file system, which KEDA scaler reads a queue). Everything about the *resources* a workload
links to comes from their definitions' `connection` tables (§2.7): the emitter resolves
each one a workload uses on its own entity, exactly like a block argument, and the value
becomes a `k8s_<slug>_<key>` output plus a `${k8s_<slug>_<key>}` token in the YAML. A manual
or unmapped target resolves to an input variable, as any reference to it does, so the
manifests never carry a value that silently breaks. The render scripts are the second
stage: `tofu apply`, then `k8s/render.sh`, then `kubectl apply -f k8s/rendered/`. The
alternative — `kubernetes_manifest` resources through the Terraform `kubernetes` provider
— is left as future work, because that provider needs the cluster's API (and the KEDA /
load balancer controller CRDs) at plan time, which the same apply is only creating.

The YAML is emitted from `serde_json` values (ordered maps) by a small writer that quotes
every string that could read back as anything else, so exports are byte-identical and a
substituted value cannot change the document's structure. Manual steps that only describe
these objects carry `when = { setting = "kubernetes_manifests", equals = "false" }` and
drop out; the module adds one step of its own when a manifest needs a controller (KEDA,
the AWS Load Balancer Controller). `export` replaces the files directly in `k8s/` and
removes the ones a later export no longer writes; `diff` reports them the same way (both
through `ttg-codegen::owned`, §6.7).

### 6.5 State: backend, encryption, bootstrap root (`ttg-codegen::state`)

**Backend.** `Settings::backend` is `{ type, args }`. The backend types and the keys each
takes are a table in `state::BACKENDS` — `s3` (`bucket`, `region`; optional `key_prefix`,
`key`, `kms_key_id`), `azurerm` (`resource_group_name`, `storage_account_name`,
`container_name`; optional `key_prefix`, `key`), `gcs` (`bucket`; optional `key_prefix` or
its own name `prefix`) and `local` (optional `path`) — and `state::check_backend` is the one
check the settings panel, the MCP `settings_set` tool and the export gate all apply: an
unknown type, a key that type does not take, or a required key left blank is refused with
the list of what would be right. The state object is `<key_prefix>/terraform.tfstate`
(`key_prefix` defaulting to the project name), and `state::state_key` takes an environment
that goes between the two — `None` today; named environments slot in there without
changing a project file. The `backend` block goes into the one `terraform {}` block in
`versions.tf`, every argument a literal (backends are configured before anything is
evaluated). S3 locks with a lock file (`use_lockfile = true`), so there is no DynamoDB
table, and `required_version` rises to what the features need: OpenTofu 1.10 / Terraform
1.11 for S3 lock files, OpenTofu 1.7 for encryption.

**Encryption.** With `state_encryption` on and OpenTofu, `versions.tf` also gets an
`encryption {}` block: a key provider, an `aes_gcm` method and `state` / `plan` blocks with
`enforced = true`. The key provider follows the target provider and
`state_encryption_key`: `aws_kms` (`kms_key_id`, `region`, `key_spec = "AES_256"`) on AWS
and `gcp_kms` (`kms_encryption_key`, `key_length = 32`) on Google Cloud when a key is
chosen, otherwise `pbkdf2` with a sensitive `state_passphrase` variable. Azure always uses
the passphrase: OpenTofu 1.10 had no Azure key provider, and the `azure_vault` provider
that 1.12 has would need the key *and its Key Vault* outside the root, which the export
does not do yet (an info diagnostic says so). Terraform has no `encryption` block at all:
nothing is emitted and a warning says the state stays unencrypted.

**Bootstrap root.** The state store has to exist before `init` can use it, and the key
before it can encrypt anything — so neither can be part of the configuration whose state
it is. `state::bootstrap_roots` builds *a small project of its own*: an Object Storage node
named exactly as the backend expects (versioning, public access blocked, TLS only — inside
a Resource Group whose name is pinned with an extra argument on Azure), plus a copy of the
chosen Encryption Key entity. The ordinary `generate` turns it into `bootstrap/*.tf`, so the
bucket and the key get their curated mappings, key policy included. In the main root the
key is then treated like an external entity: every reference to it — the key provider's and
those of the resources encrypted with it — becomes a variable (`<key>_arn`) that the
bootstrap root outputs under the same name. When the state store and the key are on
different clouds (an S3 backend for the Google Cloud export), the key gets a second root,
`bootstrap/<provider>/`. The bootstrap root keeps its own state locally; it holds no
secrets. `MANUAL_STEPS.md` starts with "Apply the bootstrap root first", with the commands
and the outputs to copy, and the README's *State* section says where the state is and how
it is encrypted. `export` writes the nested files and removes stale ones in `bootstrap/`
as it does at the top level, never touching state or `.terraform/` (§6.7).

**Diagnostics** (`Code::State`, run by `diagnostics::run` for the project's tool and again
by `generate` when it exports the other flavour): the backend and the key setting are
valid; errors the bootstrap project would raise (a storage account name Azure refuses) are
reported as `State bootstrap (bootstrap/): …` so the export stops before writing half a
configuration; Terraform plus encryption; a backend on another cloud than the export; and a
field declared `state_secret` (MAPPING_FORMAT §1.1: the Secret's *Generate the value*)
that meets local or unencrypted state — "the generated value is stored in plain text in
local state; configure a remote backend and state encryption (Settings)".

### 6.6 Provider versions (`ttg-codegen::versions`)

`Settings::provider_versions` pins a provider per project; without a pin the definition's
`version_constraint` is used (the tested default: `aws ~> 6.0`, `azurerm ~> 4.0`,
`google ~> 7.0`). The argument checks run against one schema per provider — the bundled
index or a refreshed one — so `versions::checks` compares the two: a pin whose major the
schema does not describe is reported ("argument checks use aws 6.x (the bundled schema,
…); your project pins ~> 5.0"), and when it does describe it, every curated mapping the
project uses is checked against the schema (`versions::mapping_findings`, the same check
the `schema_check` test runs over the whole catalog) and a mismatch is a warning naming the
mapping file and argument. `ttg schema refresh --provider-version aws="~> 7.0"` builds an
index for another major, after which the same diagnostics name what would break there.

### 6.7 What an export owns (`ttg-codegen::owned`)

An export writes the configuration at the top level and two directories of its own beside
it: `k8s/` (§6.4) and `bootstrap/` (§6.5). Re-exporting replaces what it wrote and removes
what it would no longer write — a manifest for a workload that is gone, a bootstrap root
once the backend is cleared — but must not touch what other tools put there: variable
values, state files, `.terraform/` and its lock file, the render scripts' `k8s/rendered/`.
`owned` is one table of `(directory, how deep, which file names)`: the top level's `.tf`
files, `README.md` and `MANUAL_STEPS.md`; the files directly inside `k8s/` (`.yaml`,
`.sh`, `.ps1`, `README.md`; shallow, so `rendered/` is never visited); and the same
Terraform files at every level of `bootstrap/` except hidden directories. `export` removes
the stale ones and then any owned directory left empty (one still holding a state file
stays); `diff::against_dir` lists the same stale files as removed.

## 7. GUI architecture (`ttg-app`)

| module | responsibility |
|---|---|
| `app.rs` | `TtgApp`: project, catalog, camera, selection, history, clipboard, dialogs, cached diagnostics |
| `canvas/` | infinite pan/zoom surface; draws containers (back to front), edges (cubic beziers), nodes; hit-testing; drag-to-move (with reparenting on drop), drag-from-port-to-connect, marquee multi-select |
| `palette.rs` | searchable, category-grouped list of catalog types; click or drag onto canvas |
| `inspector.rs` | typed property editors generated from the definition (`string`, `bool`, `int`, `cidr`, `enum`, `string_list`); provider-specific fields under a per-provider header; inline validation; the project settings (tool, state backend and encryption, default tags, provider versions, provider variables) |
| `menu.rs` | file new/open/save/save-as, undo/redo, tool toggle, target provider, export single / export all, validate |
| `clipboard.rs` | copy/paste of a sub-diagram as JSON (fresh ids, de-duplicated names, edges between copied items kept) |
| `cost_panel.rs` | the Cost window: estimate for a chosen provider, per resource / type / view, assumptions and display currency editable in place |
| `views.rs` | the view bar (tabs, description, legend toggle, annotation buttons), the filter menu, and the per-frame visible set (`ttg_codegen::views::visible_set` plus the "a fitted container with no visible members is not drawn" rule) |
| `annotations.rs` | drawing and editing a view's groups, flows, notes and logical nodes; the legend panel (and the strip of the canvas "zoom to fit" leaves it); geometry delegated to `ttg_core::view`. Flows fan out along a shared side, follow `canvas::routed_path` when *Route around nodes* is on, and place their labels clear of nodes, badges and each other — along the arrow first, then stepped off it on a leader line |
| `view_tools.rs` | the View menu's view tools (flows from links, tidy by flows, arrange notes, "Where personal data goes") as single undo steps, and presentation mode: the step state, its keys and the caption |
| `camera.rs` | world/screen transform, zoom-about-pointer, zoom-to-fit |
| `history.rs` | snapshot-based undo/redo (the project is small; a clone per committed action is simpler and safer than command objects) |

Diagnostics are computed by `ttg-codegen::diagnostics::run` whenever the project revision counter
changes, and the result drives the warning badges (`manual`, `unmapped`, `partial`, `invalid`).
Export is disabled while any error-level diagnostic exists; the button tooltip lists them.
The other providers' would-be errors (`diagnostics::other_providers`) are computed on
demand and shown in a collapsed "Other providers (N)" section under the target's list, in
a muted colour; they never reach the canvas badges, which stay about the export at hand.

## 8. Non-goals for v1

Not implemented and not designed for: running `plan`/`apply` against live accounts, real-time
multi-user collaboration, drift detection, state-file visualisation or management. A
list-price cost *estimate* exists (§6.0b), but it never reads a bill. The pipeline ends at
"validated files on disk" — where the state *will* live (backend, encryption, the
bootstrap root that creates the store) is generated, but nothing reads or writes a state
file.

## 9. Licensing

Dual-licensed MIT / Apache-2.0. Definitions are authored from publicly documented provider
schemas (OpenTofu registry as canonical); no HashiCorp source code is vendored or depended on.
