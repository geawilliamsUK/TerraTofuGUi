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
  patterns/*.json          built-in patterns (parameterised groups of entities and links, §6.11)
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
    "kubernetes_manifests": false,      // also write k8s/ (§6.4); omitted from the file while false
    "environments": ["staging", "prod"],          // optional: named environments (§6.9)
    "name_prefix": "zipos-${var.environment}",    // optional: in front of every generated resource name
    "variables": { "db_class": { "value": "db.t4g.small", "environments": { "prod": "db.r6g.large" } } },
    "outputs": "marked",                // "marked" | "all"; absent = "all" (files from before it); new projects: "marked"
    "layout": "modules",                // "flat" (default, omitted) | "modules" (§6.12)
    "module_view": "Modules"            // optional: the saved view whose groups become modules
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
  `hide_edges` to draw no structural links at all, `hide_monitoring` to draw none from or
  to alarms, log groups, audit trails and budgets);
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
`ttg_core::layout::tidy_tiers` lays a project out in tier bands (edge, public, app, data,
regional; left to right or top to bottom) crossed by one row per availability zone —
items in no zone or several share the first row — recursively inside every container and
every subnet holding resources, each fitted to what it holds; a cell stacks at most six
items before it wraps. Where each entity goes is `ttg_codegen::canvas`'s business: the
type's tier (the definition's `tier`, else its category's), a subnet's from what is in
it or linked to it, a security group's from what carries it, a route table's from its
subnets, a container's from what it holds, and the row from the zones of the subnets
it is in. The same module derives what else the canvas shows without storing it:
security-group rule rows as edges (`rule_edges`: group to group in the direction of the
traffic, or an `Outside` peer — the internet, a CIDR, an AWS-managed prefix list),
monitoring links (`is_monitoring_link`, any link from or to the `monitoring` category,
which the filter's `hide_monitoring` drops), zone badges, the region the provider
variables name and the alias regions the export's plan sends blocks to
(`alias_regions`), and the "Network" view preset.
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
| `attachment` | load balancer → instance, cluster or container app | source → target | forwards to |
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
  "overrides": { "prod": { "config": { "high_availability": true } },   // optional: per-environment values
                 "staging": { "absent": true } },                        //   and presence (§6.9)
  "classification": "personal",         // optional: public | internal | confidential | personal | payment
  "description": "Call recordings",     // optional: why it exists
  "owner": "Platform team",             // optional: who looks after it
  "adopt": { "aws": { "main": "vpc-0abc1234" } }, // optional: existing resources to import (§6.11)
  "outputs": ["endpoint", "address"]    // optional: the mapping outputs written with settings.outputs = "marked"
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

**Outputs.** `outputs` names, by the mappings' output names, what the export writes into
`outputs.tf` for the entity when `settings.outputs` is `marked`: absent means the outputs
its mapping marks `primary` (MAPPING_FORMAT.md §2.5), `[]` none. With `all` — which a file
that does not mention the setting gets, so older exports do not change — every output is
written whatever the entity says. `Project::new_document` (the app's *New*, MCP
`project_new`) starts a project with `marked`.

**Extra arguments.** `extra` is provider id → block key → argument name → JSON value,
merged into the generated block after the mapping's own arguments (yours win). Nested
blocks are objects or lists of objects, decided by the bundled provider schema
(`ttg-schema`); `{"$ref": {"entity": …, "attr": …}}` becomes a traversal to another
resource (`"block"` and, for a repeated block, `"key"` pick which one) and `{"$raw": "…"}`
a raw expression (`"refs"` splices `$ref`s into it at `@name@`). A `null` value removes
the argument the mapping sets, or the nested block. Native provider resources (type id
`native:<provider>:<tf type>`) and data sources (`native:<provider>:data.<tf type>`, one
`data` block) are synthetic catalog entries whose single block takes every argument from
`extra`; the catalog creates them on demand (`Catalog::ensure_native`). Every `$ref` and
every address in a `$raw` is checked against what the export generates and becomes a
link in the graph (`ttg_codegen::refs`, §6.0); MAPPING_FORMAT.md §2.8 has the details.

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

**Environments.** With `settings.environments`, an entity's `overrides` give the values
that replace its own in one environment (`config`, `provider_config`) and whether it
exists there (`absent`); a link's `environments` say which environments it belongs to.
Any string in a field or an `extra` argument may use a project variable, `${var.<name>}`,
declared in `settings.variables` with a base value and a value per environment;
`${var.environment}` and `${var.name_prefix}` are built in. `Project::for_environment`
(`ttg_core::environment`) resolves all of it into an ordinary project — the one
diagnostics, reachability, the cost estimate and the GUI work on — and the code generator
turns the differences into variables (§6.9).

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
  "layout": { "source": { "side": "right", "offset": 40 }, "target": {} },   // optional
  "environments": ["prod"] }                                                 // optional: absent = every environment
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

**Subnets hold nodes.** A Subnet stays a node, but its definition says `may_contain = true`
(MAPPING_FORMAT.md §1): a node whose type lists `subnet` in `allowed_parents` and has a
`via_parent` `network_membership` relation to it may have the subnet as its `parent` —
VPC › subnet › service. Only nodes go inside a node, one level deep (`Project::set_parent`
and `validate::structural` hold the structure, the catalog the types). `Project::ancestors`
passes through the subnet to the containers above it, so `ancestor` sources and required
ancestors see the network and resource group as before; `parent_chain` lists the subnet
too, and `diagnostics::relation_targets` finds it there, so the subnet a resource is drawn
inside is its membership target exactly as a link would make it — for a `many` relation in
addition to its links, the enclosing subnet first. Being a node rather than a container is
what keeps every existing project exporting byte for byte as before: making Subnet a
container would have moved every subnet from `nodes` to `containers`, and the export's
block order follows the dependency graph, which is built containers first. A test moves
every subnet-linked resource of nine examples into its subnet and checks that the export
declares the same blocks with the same arguments (their order in a file may differ: a link
and a containment enter the dependency walk at different points), that diagnostics are
identical and reachability unchanged, on all three providers. The canvas draws a subnet
holding resources as a box (`Project::is_box`) with them inside, fits it when something
moves in or out (`layout::fit_host`) and turns it back into a plain node when it is empty;
an environment's overrides apply to it as to any node, and a subnet absent from an
environment leaves what it held one level up.

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
  ├─ environments::generate    with named environments: everything below once per
  │                            environment, then merged into one root (§6.9)
  ├─ diagnostics::run          gate: errors abort, warnings become MANUAL_STEPS entries
  ├─ emit (plan phase)         evaluate `when` -> the set of (entity, block) pairs to emit
  ├─ emit (resolve phase)      ArgSource -> hcl::Expression, collects variables + manual steps;
  │                            identical grants (role assignments, IAM members) kept once
  ├─ graph (petgraph)          topological order of emissions; cycle = error
  ├─ k8s::generate             only with settings.kubernetes_manifests: k8s/*.yaml, render
  │                            scripts, k8s/README.md, plus the k8s_* outputs they read (§6.4)
  ├─ files::assemble           network.tf, compute.tf, storage.tf, iam.tf, variables.tf,
  │                            outputs.tf, versions.tf (versions + backend + encryption),
  │                            providers.tf, MANUAL_STEPS.md, README.md
  ├─ state::bootstrap_roots    [bootstrap/] — the state store and key, generated by the same
  │                            pipeline from a small project of their own (§6.5)
  ├─ modules::split            only with settings.layout = "modules": the finished root split
  │                            into a thin root and modules/<name>/ (§6.12)
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
4. **References in extra arguments** (`ttg-codegen::refs`, `Code::Reference`). The export's
   plan (`ttg-codegen::plan`: every block it generates and its address, the same one the
   emitter uses) is built first; each `$ref` and each address hcl-rs finds in a `$raw`
   (template strings and `for` expressions included) is looked up in it. What the export
   generates becomes a *derived reference* from the entity holding the extra argument to
   the entity generating the address: `expects_incoming` counts it as a link and the
   canvas draws it as a dashed arrow (`refs::derived_references`, refreshed with the
   diagnostics). What it does not generate — an address of an entity flagged external,
   deleted or off the layer, a `local.*`, an undeclared `var.*`, a block key or row key
   that does not exist — is an error naming the `$raw` and the address, so the export
   stops before `validate` would.
5. **Posture rules** (`ttg-codegen::posture`, `Code::Posture`): warnings and info with a
   rule id each, which a project can turn off (§6.0a2).

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

**Endpoints.** An AWS endpoint fronts a whole service in the region, not the resource it
"Connects to": an SQS endpoint carries the calls to every queue, `secretsmanager` every
secret, `logs` every log group, `ecr.api` / `ecr.dkr` the registries, `s3` / `dynamodb`
every bucket / table (`rds` is the management API and carries no database connection).
The service is the endpoint's `service` field, else what its "Connects to" target
implies. An interface endpoint has private DNS on, so it answers for its service
everywhere in its network — and its own security groups must admit the source on 443,
or the call is refused rather than sent through the NAT (`Blocked`; an endpoint with no
group is `Unknown`, the VPC default group decides). A gateway endpoint serves the
subnets whose route table it is linked to. On Azure and Google Cloud a private endpoint
still fronts the one resource it connects to (or the container holding it: a key
vault's secrets). When traffic to a managed service leaves through a NAT gateway although
the network has an endpoint for that service, the path carries a note saying why the
endpoint is not used (a gateway endpoint not on the source's route table; an Azure
endpoint for another resource).

**File systems** listen on 2049 behind their own group, with one mount target per linked
subnet: the path goes through the mount target in the client's zone, a client in a
zone without one gets a note (it cannot mount by the DNS name), and a file system with no
subnet has no mount target at all (`Blocked`). A missing 'Mounts' link is a note, like a
missing 'Uses' link to a database or cache: the network allows it, the export hands over
no volume or address.

**Load balancers and the internet as sources.** `forwarding_targets` says what a load
balancer forwards to and on which port (`Forward`): instances on its target port, a
container app on the app's port (an `ip` target group), and for a cluster the workloads
that "Receive traffic from" it, each on its own port or the target port. When no workload
on the cluster says so, the cluster's nodes are the target and every workload on it an
*assumed* one, whose path is `Unknown` even where the nodes' groups would admit it — the
diagram does not say which pods get the traffic. `paths_to` lists, besides every
resource that opens connections, the load balancers forwarding to the target and
`reach::INTERNET` ("the internet") for anything internet-facing: directly (through a CDN
whose origin it is, when one fronts it), a bucket behind a CDN, or a target behind an
internet-facing load balancer (CDN → load balancer → target). A load balancer that
admits only CloudFront's origin-facing prefix list is reached through the CDN that
fronts it, and noted when none does.

**Intent and the audit.** `reach::intents` lists the pairs the diagram says talk, each
with its reasons (`Intent`): a link from a resource that opens connections (except where
it runs and the logs its provider delivers itself), a load balancer to what it forwards
to (not to assumed receivers), a flow drawn between two resources in any view (from the
end that opens the connection: a flow from a queue to its consumer is the consumer
polling it, a flow from a file system to a workload is the workload mounting it), and a
resource that reads the secret a database or cache takes its password from.
`reach::audit` compares that with the network and returns `Finding`s, errors first:
`blocked` (error: intended, but the network or a missing link stops it), `unintended`
(warning, one per target: sensitive data — classified personal, payment or
confidential, or an unclassified database, secret or bucket — that resources with no
intent can reach, the internet included), `undecided` (info: intended but `Unknown`),
`unlinked` (info: intended only by a flow or a secret, allowed by the network, but with no
link the export hands over no address or credentials) and `nat_egress` (info: the NAT
note above on an intended pair). `reach_from` / `reach_to` (MCP and `ttg reach --from /
--to`) narrow their paths by `status` and to intended pairs (`only_intended`), and every
path names its intent. The GUI shows the audit under the network posture while the
overlay is on with nothing selected (*View ▸ Reachability audit*).

With named environments the analysis reads the values of the environment shown
(`Project::resolved`), and `reach::audit_other_environments` runs the audit on every other
environment's own values (`Project::for_environment`), keeping what the shown one does
not already report, tagged `[env]` — an override can open or close a path. `reach_audit`
returns them as `other_environments`; `ttg reach audit --environment` picks the one to
audit and prints the others' after it.

### 6.0a2 Posture rules (`ttg-codegen::posture`)

Posture is data, not judgement: a few facts about a design that usually deserve a second
look, each a rule with a stable id, run on the provider's layer from
`diagnostics::run` as `Code::Posture` warnings or info (never errors, so never
export-blocking) — and so, like every rule there, per environment through
`environments::other_environments`. Every message ends with `(posture rule '<id>')` and
`posture::rule_of` reads it back (the MCP diagnostics carry it as `rule`).
`settings.posture.disabled` lists the rules a project turns off — the settings window's
*Posture rules* boxes, or `settings_set { posture: { disabled: [...] } }`. The rules:
`public_kubernetes_api` (public endpoint, no allow-list or 0.0.0.0/0 in it; this was a
catalog check on each provider), `open_ingress` (a rule admitting 0.0.0.0/0 on anything
but 80 and 443), `data_tier_open_egress` (info: a group carried by a database, cache or
file system lets everything out), `orphan_security_group` (info: nothing carries, links or
names it), `queue_dead_letter_alarm` (warning when no alarm watches a queue's dead
letters, info when it has no dead-letter queue), `database_backups` (retention 0) and
`classified_without_cmk` (personal or payment data that could be linked to an Encryption
Key and is not).

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
  tool's `assumptions`. A usage preset (`settings.cost_assumptions.preset`, or `usage` on
  one call / `ttg cost --usage`) sits between the defaults and those: *medium* is the
  defaults, *small* and *large* divide and multiply every volume by ten and leave what
  describes the design (hours in a month, a pool's node-hours a day, the spot fraction,
  zones, a function's duration, NAT VMs, alarm series) alone. A pool with `min_nodes = 0` is priced at the node-hours-a-day
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
The validate suite (`tests/validate_examples.rs`) also runs `fmt -check -recursive` with
every installed binary on every export: the renderer writes what `fmt` writes
(`files::align_attributes` follows hclwrite's alignment — consecutive single-line
attributes form a group whatever their nesting, and anything else, a multi-line value's
first line included, ends it), so an export never shows a formatting diff.

**Lock file.** An export ships no `.terraform.lock.hcl`, and re-exporting never touches
one. `init` writes a lock file with the checksums of the platform it ran on only, which a
CI runner on another platform refuses, so the export README says to lock for every
platform before committing, `<tool> providers lock -platform=linux_amd64
-platform=linux_arm64 -platform=darwin_arm64 -platform=windows_amd64`
(`validate::LOCK_PLATFORMS`), and `ttg export --lock` runs exactly that after exporting.
If not found it returns `BinaryNotFound` and the GUI shows the exact command to run. The app has
no runtime dependency on either binary. `init` can take minutes, so the GUI never runs this on the
UI thread: `TtgApp::run_validate` starts one background thread for the exported providers and
`poll_validate` collects the outcomes through an `mpsc` channel once a frame, showing "validating…"
per provider meanwhile. A frozen UI is not just an unresponsive window — it also stops the MCP
command queue draining, which is what made an agent's timed-out call get applied late.

The app's *Run validate* and the agent's `export_run { validate }` run `validate -json`
through `ttg_codegen::plan_run::validate` instead, so every error comes back attributed: the
`range` (file and line) is looked up in the export's per-entity line ranges
(`EntityBlocks::lines`, recorded as the resource files are rendered) and names the entity
and the definition file of its type; an error in `variables.tf` or `versions.tf` belongs
to no entity and says so. `validate::run` (plain text) remains for the CLI's
`export --validate` and the validate suite.

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
that goes between the two: with named environments the `backend` block is partial and
each environment's key (or `gcs` prefix, or local path) is in
`environments/<env>.backend.hcl` (§6.9). The `backend` block goes into the one `terraform {}` block in
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

### 6.7a Containers: environments, apps and jobs

Three types share one vocabulary (`container_environment.toml`, `container_app.toml`,
`container_job.toml`). An app and a job take what they have in common — the container's
fields, links, environment, image and grants — from one fragment file,
`definitions/fragments/container.toml` (MAPPING_FORMAT.md §1.3), so a change to the
container's settings is made once:

- **Container Environment** is a container: an `aws_ecs_cluster` (with Container
  Insights on or off) or an `azurerm_container_app_environment` (with its infrastructure
  subnet and Log Analytics workspace); logical on Google Cloud, which has no environment.
  Apps and jobs drawn inside it use it — `{ ancestor = "container_environment" }` decides,
  and the app's own `cluster` / `env` block is only emitted outside one, so projects drawn
  before it existed export as they did. Everything derived from an app reads the cluster
  actually used: the Alarm's `ClusterName` dimension falls back from the app's own
  cluster block to its environment through a relation source's `ancestor`.
- **Configuration that travels**: `env` and `secrets` rows, health check (command or HTTP
  path, interval, timeout, retries, start period), stop timeout, CPU architecture, and an
  image from a linked Container Registry at `image_tag` (an input variable while unset):
  on AWS the URL of the repository the app names (`repository`, else the registry's
  first) — `"${aws_ecr_repository.<registry>_repo_<repository>.repository_url}:<tag>"`, a
  relation source's `key` picking that instance of the repeated block, so a task depends
  on its own repository only; on Azure and Google Cloud the registry's `REGISTRY`
  connection value and the repository as a path. ECS gets them in its
  container definition (`secrets[].valueFrom = "<arn>:<key>::"`, `healthCheck`,
  `stopTimeout`, `runtime_platform`), Container Apps as env blocks, Key Vault secret
  references, a liveness probe and the termination grace period, Cloud Run as env blocks
  with `secret_key_ref` and a liveness probe. What a provider cannot express (a command
  probe off AWS, a JSON key in a Key Vault or Secret Manager secret, arm64, Cloud Run's
  fixed ten-second stop) is a warning or a manual step. Links add variables from the
  target's connection values: `DB_*` for a database (unless *Database variables* is
  cleared), `CACHE_*` for a cache, `BUCKET_NAME` and `BUCKET_REGION` for a 'Uses' bucket
  (`BUCKET_ACCOUNT` / `BUCKET_CONTAINER` on Azure). The "Database access is not an IAM
  grant" step drops out once the container reads a Secret built from that database
  (a relation condition's `except`).
- **Two roles**: 'Task role' (`iam_binding`) is what the code runs as and gets the
  least-privilege policy built from 'Uses' / 'Reads' / 'Sends to'; 'Execution role'
  (`attribute_reference` to an `iam_role`) starts the task — the ECS execution role,
  the Azure identity that reads Key Vault and pulls from ACR (with `AcrPull`), the job's
  service account on Google Cloud when it has no task role. With one role linked it does
  both, as before. The IAM Role attaches `AmazonECSTaskExecutionRolePolicy` only when an
  app or job starts with it, decided with an incoming relation condition whose `where`
  asks the app whether it has an execution role of its own; the app adds an inline
  `secretsmanager:GetSecretValue` policy for the secrets it injects. Identical grants
  from several apps sharing an identity are written once (`emit::dedupe_grants`).
- **Load balancer → app**: an `ip` target group with the app's port, its health check
  (path, interval, timeout, its retries as the unhealthy threshold, and the healthy
  threshold at its minimum of 2, since the container's own check counts no successes),
  stop timeout as deregistration delay, and the ECS service's `load_balancer` block plus
  `depends_on` the listener; on Google Cloud a serverless NEG as the backend of the
  global HTTPS load balancer (no health check, port name or timeout on that backend
  service) with the service's ingress narrowed to the load balancer; on Azure nothing to
  attach — the app's own ingress is made external instead, and a manual step says why.
- **Background workers** have no port mapping, ingress or invoker, and Cloud Run keeps
  their CPU allocated (`cpu_idle = false`, min = max instances). A Cloud Run worker pool
  would fit too, but alarms, IAM and the load balancer all address the app as a service.
- **Jobs**: an ECS task definition plus a `terraform_data` holding what `aws ecs run-task`
  needs (cluster, task definition, subnets, security groups) as the `<job>_run` output; a
  manually triggered `azurerm_container_app_job`; a `google_cloud_run_v2_job`.

Reachability treats jobs like apps (they initiate connections from their subnets), a
background worker listens on no port, and the "no link" note names the link the source
type offers. An `extra` argument that replaces a mapping's only reference to another of
the entity's own blocks — a service pointed at a hand-made cluster — is reported, since
that block is still created and whatever else is derived from it still points at it.

### 6.7 What an export owns (`ttg-codegen::owned`)

An export writes the configuration at the top level and two directories of its own beside
it: `k8s/` (§6.4) and `bootstrap/` (§6.5). Re-exporting replaces what it wrote and removes
what it would no longer write — a manifest for a workload that is gone, a bootstrap root
once the backend is cleared — but must not touch what other tools put there: variable
values, state files, `.terraform/` and its lock file, the render scripts' `k8s/rendered/`.
`owned` is one table of `(directory, how deep, which file names)`: the top level's `.tf`
files, `README.md` and `MANUAL_STEPS.md`; the files directly inside `k8s/` (`.yaml`,
`.sh`, `.ps1`, `README.md`; shallow, so `rendered/` is never visited); and the same
Terraform files at every level of `bootstrap/` except hidden directories; and the files
directly inside `environments/` (§6.9): each environment's `.tfvars`, `.backend.hcl` and,
when the steps differ, `.MANUAL_STEPS.md` — but never a `*.auto.tfvars`, which is the
user's; and the Terraform files at every level of `modules/` (§6.12), so switching back
to the flat layout removes the modules. Hand edits to `environments/<env>.tfvars` are overwritten (the README says so and
points at a second `-var-file` or an `*.auto.tfvars`). `export` removes the stale ones and
then any owned directory left empty (one still holding a state file stays);
`diff::against_dir` lists the same stale files as removed. A plan's scratch copy,
`.ttg-plan/` (§6.10), is hidden and never visited.

### 6.8 Addresses (`ttg-codegen::plan`)

Before anything is resolved, `Plan::build` decides which blocks the export generates
and what each is called: `<slug>` for an entity's `main` block, `<slug>_<key>` for its
others, and for a repeated block (`for_each_field` / `for_each_relation`) one instance per
surviving row named after the row's key — the target entity's name, the list entry, or
the row's `for_each_key` / `name` item — so reordering a list does not replace the
resources it made (MAPPING_FORMAT.md §2.6). The emitter, the diagnostics' reference
checks, `$ref` by key, `entity_preview`'s addresses and the inspector's address list all
read the same plan. Repeated blocks used to be named by row index; each instance whose
name changed keeps its old name as `legacy`, and the export writes `moved.tf` with one
`moved` block per rename so an existing state follows it (no-ops for a state that never
had the old names). The file can be deleted once every state has been applied with it;
`emit::LEGACY_INDEX_MOVES` turns it off one release after the rename. One step runs on
the planned names after emission: `dedupe_grants` writes an identical grant only once
(§6.7a), so `entity_blocks` and the files list the copy kept, and a dropped copy gets no
`moved` block (its address is not declared; the kept one has its own).

In the modules layout (§6.12) every address of a resource in a module is the module's —
`module.network.aws_subnet.app_a` — in `EntityBlocks::addresses`, `Generated::address_map`,
`entity_preview` and the inspector's list (`emit::addresses_for`); `$ref` and `$raw` are
still written and checked against the flat names, and `modules::assign` / `qualify` say
where an entity's address goes for anything else that needs it (an `import` block's
`to`).

### 6.9 Environments (`ttg_core::environment`, `ttg-codegen::environments`)

One diagram, several environments; one configuration per provider, with a `.tfvars` and
a state per environment — the "one module plus `envs/`" layout, with the module still
flat. `emit::generate` sends a project with `settings.environments` to
`environments::generate`, which generates it once per environment on
`Project::for_environment(env)` with the ordinary pipeline (`emit::generate_one`) and
merges the results block by block:

- a value the same everywhere stays as it is;
- a value that is a project variable, or a string built from them (`/zipos/${var.environment}/web`,
  found by instantiating the entity's own `${var.…}` strings per environment), or a name
  behind the name prefix (as it is, snake-cased or with its hyphens dropped), is written
  as that variable or template, and the variable declared once (`db_class`, `name_prefix`);
  so is such a string in a value only some environments read (a block only prod has, a
  `var.environment == …` branch), where it is the same everywhere it is read but would be
  another environment's own: the final snapshot's `"${var.name_prefix}-db"`, not
  `"zipos-prod-db"`;
- arithmetic on numbers alone (`floor(75 * 87 / 100)`, a threshold worked out from each
  environment's instance class) is folded to the number per environment and lifted like
  a plain value (`db_connections_threshold = 65` / `642`);
- any other plain value that differs becomes a variable of its own, named after the field
  when it *is* that field's value in every environment (`db_high_availability`; when two
  fields fit, the one sharing words with the argument wins, and with a tie the variable is
  named after the argument: `db_multi_az`) and after the block and argument otherwise
  (`logs_retention_in_days`); an argument one environment leaves out is `null` there;
- inside a list, map or call of the same shape, only the parts that differ;
- a value that differs in references chooses on `var.environment`
  (`var.environment == "prod" ? … : …`);
- a nested block some environments leave out becomes a `dynamic` block, and a resource
  some leave out (an entity `absent` there, a repeated block with fewer rows) a `count`,
  gated by the bool field that decides it (`var.db_high_availability ? [1] : []`), by a
  choice field's option (`var.waf_mode == "count"`; a free value such as an instance class
  that happens to line up decides nothing), or else by `var.environment`; references to a
  counted resource become `one(<address>[*].<attr>)`, outputs about it are null where it
  does not exist, and `depends_on` lists are joined;
- `outputs.tf`, `providers.tf`, `variables.tf` and `versions.tf` are merged the same way
  when they differ (a provider's aliases told apart by `alias`; the `terraform {}` block
  joined, never variable; a variable's description neutralised to `<environment>` /
  `<name prefix>`).

What cannot be said that way is refused with a `Code::Environment` error naming the entity,
the address and the fields that differ: a repeated nested block whose count differs, a
labelled nested block, a heredoc that names a counted resource, Kubernetes manifests or a
bootstrap root that differ. The merged root declares `environment` (validated against the
list), the project variables it uses and every lifted value, none with a default — a plan
without `-var-file` asks rather than guessing — and writes
`environments/<env>.tfvars` and `environments/<env>.backend.hcl`: the backend block in
`versions.tf` is partial (the `local` backend when none is configured, so environments
never share a state file), each environment's file completing it with its key
(`<key_prefix>/<env>/terraform.tfstate`), `gcs` prefix or local path
(`terraform.tfstate.d/<env>/terraform.tfstate`). The README gains an *Environments*
section (the table of variables and their values per environment, and the
`init -reconfigure -backend-config` / `plan -var-file` commands); manual steps that differ
go to `environments/<env>.MANUAL_STEPS.md`. `Generated::lifted` lists the variables.

Diagnostics: `diagnostics::run` checks environments, overrides and `${var.…}` references
as written (`environments::checks`) and everything else on the resolved values. The app
runs it for the environment the canvas shows; `environments::other_environments` — the
other environments' runs, each diagnostic prefixed `[prod]`, plus the differences the
merge would refuse — is computed only when the diagnostics panel or the agent asks, like
the other providers'. The export blocks on any environment's errors, tagged the same way.

Names: `Project::resource_name` puts the rendered prefix in front of a display name, and
the emitter uses it wherever a mapping builds a provider name from the entity's name
(`field = "name"`, `{name}`, a relation's `field = "name"`). What a project names by hand
the prefix does not reach: `ttg_catalog::names` reads from the mappings which arguments
are names (those filled from the entity's name or from a `unique_scope` field, less the
ones that only describe it) and which fields feed them (the bucket's `bucket_name`, the
rows of the registry's `repositories`), and once the rendered prefix differs between
environments `environments::checks` warns about each such field or `extra` argument that
comes out the same in two of them, with the value to write instead
(`"${var.name_prefix}-alb"`, `"/zipos/${var.environment}/web"`). A repeated block's row
key behind a prefix that names the environment is named without it
(`ecr_repo_web` for `${var.name_prefix}-web`), so the row keeps one address. Zones: a provider field
declared with `zone_of = "region"` (MAPPING_FORMAT §2.3) writes a bare zone letter as
`"${var.region}a"`.

### 6.10 Plan (`ttg-codegen::plan_run`)

`plan_run::export_and_plan` exports, then runs `<tool> init -input=false`,
`<tool> plan -input=false -json -out=tfplan [-var-file=environments/<env>.tfvars]` and
`<tool> show -json tfplan`. `Attribution::of(&Generated, …)` is what makes the answer
structured: every address the emitter wrote maps to its entity (`Generated::address_map`;
`[index]` and module parts dropped), and every file line range to the entity whose
blocks it holds. The plan's `resource_changes` become per entity `create` / `update` /
`delete` / `replace` / `read` / `no_op` counts with their addresses; the `plan -json`
message stream's diagnostics are attributed by range, or by the `address` they carry.
By default the plan runs in `.ttg-plan/`, a scratch copy of the configuration whose
backend an override file points at a local state: what applying would create from
nothing, without touching the real state or needing the state store. `real_backend`
plans in the export directory with `init -reconfigure -backend-config=…`. A plan reads the
account, so it needs the provider's credentials; errors that say they are missing make the
status `no_credentials` rather than `failed`. `PlanReport` is the shape the CLI's
`ttg plan --json`, the MCP `plan_run` and the GUI share; the GUI puts `+`, `~`, `−`, `±`
badges on the entities until the next edit.

### 6.11 Reviewing and reshaping a design: diff, checkpoints, patterns, adoption, import

**Architecture diff** (`ttg_core::arch_diff`). `diff(old, new)` compares two projects in
diagram terms. Entities are matched by id, then — among the rest — by slugified name and
type when that pairs them one to one (an entity deleted and drawn again, pasted or
imported is "changed, id old → new", not "removed and added"). Per entity it reports a
rename, a containment move (old and new container by name) and every leaf that differs
under `config`, `provider_config`, `extra`, `adopt` and `overrides` (per environment:
`overrides.prod.config.replicas`, `overrides.pilot.absent`), plus `manual`, `providers`,
classification, description and owner. Links are compared with their old ends mapped to
the new ids, by `(source, target, relation)`; a link whose `providers` or `environments`
changed is "changed". Views are matched by name: description, legend, the filter's leaves,
whether it has a layout of its own, how many entities it places differently, and its
groups, flows, notes and logical nodes by id (added / removed / changed, geometry left
out). Settings are compared leaf by leaf (`settings.tags.Project`,
`settings.environments`, `settings.variables.db_class.environments.prod`). Positions and
sizes are never compared: every drag would be a change. The result serializes as JSON,
and `to_markdown(from, to, type_label)` renders it for a pull-request description — a
summary line, tables of added and removed entities, a list of changed ones with
`old → new` per field, then links, views and settings.

**Checkpoints** (`ttg-app::revisions`). A checkpoint is a named copy of the whole project,
kept in a sidecar beside the project file — `shop.checkpoints.json` next to
`shop.ttg.json`, `{ "checkpoints_version": 1, "checkpoints": [{ name, created_unix,
project }] }` — so it never reaches an export, the project schema or the project file's
diff, and a project file copied elsewhere does not drag its history along. A project never
saved keeps its checkpoints in memory; the first save writes them beside the new file
(opening or starting another project drops them). Restoring is one undo step and asks
first, in the GUI and over MCP, when the diagram has unsaved changes. `project_diff`
compares the open project with `saved` (the file on disk), a checkpoint, or another file.

**Patterns** (`ttg_codegen::patterns`, data in `definitions/patterns/*.json`). A pattern
is parameters, entities (key, type, name, parent, offset, size, config, provider_config,
extra, `when`) and links, with `{param|filter}` templates in its strings (filters `kebab`,
`alnum`, `slug`, `lower`, `upper`, `max:N`; `{project}` built in) and entity parameters
(`"entity": "<type>"`, optionally a `list`) that reach existing entities as `$param` in
links and as `$param` / `@param` (that entity / its container) in `parent`. `expand`
checks the parameters (unknown, missing, wrong type of entity), the names (no collision
with the project, as slugs) and turns the pattern into the very commands `project_apply`
takes — `entity_add`, `entity_resize`, `entity_update`, `link_add` — so the app runs it
through `run_batch` (one undo step, all-or-nothing, every value, parent and relation
checked as an agent's would be). An entity whose container does not allow its type goes
into the nearest ancestor that does, and containers grow around what was inserted. New
entities carry no environment overrides: they are the same in every environment until
someone says otherwise. `check` validates a pattern against the catalog (types, fields,
parents, relations, templates, `when`); a test runs it on every built-in, and the app's
test inserts each built-in into an example and validates the export on all three
providers. `infer` makes a pattern from a selection: links among the entities come along,
a link to anything outside becomes an optional entity parameter, and leading words the
names share become a `name` parameter, templated in names and provider values. Saved
patterns live in the user's folder (`%APPDATA%\TerraTofu GUI\patterns` on Windows, beside
eframe's `data`; `TTG_PATTERNS_DIR` overrides it); built-in names are reserved. Inserting
a pattern in the GUI needs the `mcp` feature (default on), whose executor it uses.

**Adoption** (`ttg_codegen::adopt`). An entity's `adopt` map (provider → block key,
`<block>/<row key>` or address → cloud id) makes the export write `imports.tf`, one
`import { to = <address> id = "<id>" }` per id whose block it writes (Terraform 1.5+,
OpenTofu 1.6+, within the existing version floors). Repeated blocks are separate resources
named by key, so an instance is adopted by its key and no `for_each` import is needed.
`Code::Adopt` warnings name the blocks of an adopted entity that have no id (they will be
created), keys that name nothing, data sources and entities that are also flagged
external. With environments the merged root gets the import blocks once
(`environment_imports`, called after the merge): the same id everywhere, or — when an id
uses a project variable (`${var.vpc_id}`, substituted per environment like any field) —
`id = { "pilot" = "…", "prod" = "…" }[var.environment]`. An adopted block some environment
leaves out is counted there, and an import cannot depend on the environment, so that is an
error, from the diagnostics (an `absent` override) and from the export (a repeated row only
some environments have). The `manual` flag stays what it was: nothing generated, inputs
instead.

**Directory import** (`ttg_codegen::import_dir`). `import_dir(project, catalog, dir,
provider)` reads the top-level `.tf` files of a directory TerraTofu exported (the file
header names the provider and tool; a directory without one is refused) and compares each
resource and data block with what the project generates now — `emit_unchecked`, or for a
project with environments the merged root `generate` writes. Addresses are mapped to
`(entity, block key, instance)` through the plan, spelled by `adopt::instance_address`
(the one place that agrees with `Plan::addresses`, which a test pins). Per changed block,
each differing argument becomes an `extra` override: a literal where the value is one, a
`$ref` (with `block` and `key` / `index`) where it is exactly a reference to a known
block, a `$raw` otherwise, `null` for a deleted argument; a changed nested block is
replaced whole, and a repeated block takes overrides only when every instance changed the
same way. Blocks the plan does not know become native entities named after their label,
their arguments in `extra`, references converted the same way — hand-written files such
as `data.tf` included, which the report then lists for deletion. Generated blocks missing
from the directory are reported, never deleted. `moved.tf`, `imports.tf` and
`environments/` are the export's own bookkeeping and are skipped; the `modules/<name>/`
of the modules layout are read like the top level (§6.12); other subdirectories,
`locals`, module calls and edits to variables, outputs or providers are listed as not
imported. Comparison is semantic (`semantic_blocks`: attributes by name, nested blocks by
name in order, object keys as text, expressions as formatted text), which is also what
the round-trip test uses: export, edit by hand, import, export again, and the second
export declares exactly the edited directory's blocks.

### 6.12 Modules (`ttg-codegen::modules`)

`settings.layout = "modules"` lays an export out as a thin root calling one module per
group of entities, each under `modules/<name>/` with its resource files (split by
category as in the root), `variables.tf`, `outputs.tf` and `versions.tf`
(`required_providers` with the same sources and constraints as the root's). The flat
export is generated first — environments merged and all — and `modules::split` then moves
blocks and rewrites references, so the emitter, the reference checks and the merge are
unchanged.

**What becomes a module** (`modules::assign`). With `settings.module_view` naming a saved
view, each of its labelled groups: the entities whose centre lies in the box, the
smallest box when boxes nest; a container counts where its own box is, not by its
members (which may sit in other groups); an entity in no group goes with its nearest
enclosing container that is in one. Everything else goes by containment: the module of
its outermost enclosing container that generates something on this provider — a network,
an Azure resource group, a container environment. A container that is logical or
external there (a resource group on AWS) is looked through, so on AWS a resource group's
networks are modules of their own and what sits directly in it stays in the root. Names
are the slugs of the group labels or container names, made unique with `_2`. A project in
which nothing falls into a module exports exactly as flat.

**References crossing a boundary.** Inside a module a reference to a resource in another
module or in the root becomes an input, `var.<local>_<attr>`, of type `any`; the module
that owns the resource outputs it under the same name (`sensitive` when the provider
schema says the attribute is), and the root's `main.tf` wires the two:
`subnet_ids = module.network.app_a_id`. The root's own references — `outputs.tf`, the
`k8s_*` outputs, any root resource — read `module.<name>.<output>`. A module reads the
root's variables (provider settings, project and environment variables, the values of
external resources, the bootstrap key) as inputs of the same name and type with no
default, and the root passes them on (`region = var.region`), so `variables.tf`, the
`.tfvars` and the backend files are those of the flat export. Nothing inside a module
ever names `module.…`.

**`depends_on` across the boundary** becomes the module block's `depends_on`
(`[module.network]`, or a root resource) when that cannot close a cycle in the graph of
modules (its edges: what each scope reads from another, and every crossing `depends_on` in either form; the root
counts as one node), and otherwise an input carrying the target's `id` the resource
depends on (`depends_on = [var.app_a_id]`, which Terraform and OpenTofu both accept):
the same ordering, resource by resource, without making two modules wait for each other
as a whole. A root resource depending on a module's resource names the module the same
way, or its output.

**Provider aliases.** A module with a resource sent to an aliased configuration
(CloudFront's `aws.us_east_1` certificate and Web ACL) declares it in
`configuration_aliases`, and the root passes every configuration the module needs
explicitly (`providers = { aws = aws, aws.us_east_1 = aws.us_east_1 }`); the aliased
provider block stays in the root's `providers.tf`.

**State.** `moved.tf` gains a `moved { from = aws_vpc.main  to = module.network.aws_vpc.main }`
per resource in a module (data sources hold no state), after any index-to-key moves of
§6.8 — chained, so an old index address goes to the key address and on into the module —
so a state applied with the flat layout follows the switch; for a state that never had
the flat addresses they do nothing. An `import { to = … }` block in a root file — an
adopted entity's, in `imports.tf` — follows its resource into the module
(`to = module.network.aws_vpc.core`), and an adopt key may be written as either address.
`import_dir` reads `modules/<name>/*.tf` beside the top level: each module's blocks, in
the directory and in the project's own modules export, have the inputs that carry other
scopes' values turned back into the flat references they stand for
(`Generated::module_inputs`, `modules::unsplit_body`), so the comparison and the overrides
it produces are those of a flat export. `versions.tf` or `providers.tf` naming a resource that moves
is refused with a message rather than written wrong.

**Environments: one root, not `envs/<env>/`.** The environments design (§6.9) already
makes the per-environment roots of a `modules/` + `envs/staging`, `envs/prod` layout
unnecessary: their only difference would be the values, which are the root's variables
and `environments/<env>.tfvars`, with a state per environment through
`environments/<env>.backend.hcl`. Writing `envs/<env>/main.tf` as copies of the same
module calls with the values inlined would give two roots that must be kept identical by
hand and a second place for each value; the README's commands (`init -reconfigure
-backend-config=environments/<env>.backend.hcl`, `plan -var-file=environments/<env>.tfvars`)
do what `cd envs/<env>` does there. The root's variables are passed to the modules, so a
module never knows which environment it is in.

**Addresses and attribution.** The entity's `EntityBlocks` records its `module`, its file
under `modules/<name>/` and its line range there, and module addresses; `plan_run`'s
`Attribution::entity_of_address` finds `module.network.aws_nat_gateway.nat[0]` directly
(and a flat address through the module one, for a state from before the switch), the
plan's scratch copy takes `modules/` along, and `validate -json` diagnostics in a module's
files are attributed by file and line like the root's. Cost, reachability and the
diagnostics work on the project, not the files, and are unaffected.

## 7. GUI architecture (`ttg-app`)

| module | responsibility |
|---|---|
| `app.rs` | `TtgApp`: project, catalog, camera, selection, history, clipboard, dialogs, cached diagnostics |
| `canvas/` | infinite pan/zoom surface; draws containers (back to front), edges (cubic beziers), nodes; hit-testing; drag-to-move (with reparenting on drop), drag-from-port-to-connect, marquee multi-select |
| `palette.rs` | searchable, category-grouped list of catalog types; click or drag onto canvas |
| `inspector.rs` | typed property editors generated from the definition (`string`, `bool`, `int`, `cidr`, `enum`, `string_list`); provider-specific fields under a per-provider header; inline validation; the project settings (tool, state backend and encryption, default tags, provider versions, provider variables) |
| `menu.rs` | file new/open/save/save-as, undo/redo, tool toggle, target provider, export single / export all, validate |
| `clipboard.rs` | copy/paste of a sub-diagram as JSON (fresh ids, de-duplicated names, edges between copied items kept) |
| `environments.rs` | the environment selector, per-environment values in the inspector (badge, reset, "In this environment"), the Environments settings (list, name prefix, project variables), the Plan button and its canvas badges, attributed validate and plan results |
| `cost_panel.rs` | the Cost window: estimate for a chosen provider, per resource / type / view, assumptions and display currency editable in place |
| `revisions.rs` | checkpoints (the sidecar, cached per project file), the base a diff compares with, adoption edits, directory-import previews and their application, the user's pattern folder (§6.11) |
| `revisions_ui.rs` | File ▸ Compare with… / Checkpoints… / Import exported directory…, Edit ▸ Insert pattern… / Save selection as pattern…, and the inspector's "Adopt existing" section per provider |
| `views.rs` | the view bar (tabs, description, legend toggle, annotation buttons), the filter menu, and the per-frame visible set (`ttg_codegen::views::visible_set` plus the "a fitted container with no visible members is not drawn" rule) |
| `annotations.rs` | drawing and editing a view's groups, flows, notes and logical nodes; the legend panel (and the strip of the canvas "zoom to fit" leaves it); geometry delegated to `ttg_core::view`. Flows fan out along a shared side, follow `canvas::routed_path` when *Route around nodes* is on, and place their labels clear of nodes, badges and each other — along the arrow first, then stepped off it on a leader line |
| `view_tools.rs` | the View menu's view tools (flows from links, tidy by flows, arrange notes, "Where personal data goes") as single undo steps, and presentation mode: the step state, its keys and the caption |
| `rule_edges.rs` | security-group rules on the canvas: dashed labelled arrows between the groups or the resources carrying them, `in:` / `out:` badges for address rules, selecting one marks its row in the inspector |
| `region.rs` | the region frame around the drawing ("AWS · eu-west-2", plus the alias regions in use); never an entity, never exported |
| `camera.rs` | world/screen transform, zoom-about-pointer, zoom-to-fit |
| `history.rs` | snapshot-based undo/redo (the project is small; a clone per committed action is simpler and safer than command objects) |

Diagnostics are computed by `ttg-codegen::diagnostics::run` whenever the project revision counter
changes, and the result drives the warning badges (`manual`, `unmapped`, `partial`, `invalid`).
Export is disabled while any error-level diagnostic exists; the button tooltip lists them.
The other providers' would-be errors (`diagnostics::other_providers`) are computed on
demand and shown in a collapsed "Other providers (N)" section under the target's list, in
a muted colour; they never reach the canvas badges, which stay about the export at hand.

## 8. Non-goals for v1

Not implemented and not designed for: running `apply` against live accounts (a `plan`
can be run, §6.10, and only reads), real-time
multi-user collaboration, drift detection, state-file visualisation or management. A
list-price cost *estimate* exists (§6.0b), but it never reads a bill. The pipeline ends at
"validated files on disk" — where the state *will* live (backend, encryption, the
bootstrap root that creates the store) is generated, but nothing reads or writes a state
file.

## 9. Licensing

Dual-licensed MIT / Apache-2.0. Definitions are authored from publicly documented provider
schemas (OpenTofu registry as canonical); no HashiCorp source code is vendored or depended on.
