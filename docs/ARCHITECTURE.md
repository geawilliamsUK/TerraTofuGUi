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
crates/
  ttg-core                 IR types, project file (JSON), structural validation, dependency graph
  ttg-catalog              loads + validates definition files; typed schema for the mapping format
  ttg-codegen              IR + catalog -> HCL files, MANUAL_STEPS.md, Terraform/OpenTofu toggle,
                           design-time diagnostics, optional `validate` shell-out, multi-provider bundle
  ttg-cli                  headless `ttg export` / `ttg check` — used for CI and by contributors
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
    "backend": null,                    // or { "type": "s3", "args": { "bucket": "...", ... } }
    "state_encryption": false           // OpenTofu-only feature; ignored for Terraform
  },
  "containers": { "<id>": Container, ... },
  "nodes":      { "<id>": Node, ... },
  "edges":      [ Edge, ... ],
  "views":      [ { "name": "Messaging", "filter": { "categories": ["serverless"], "depth": 1 } } ]  // optional
}
```

`views` are saved canvas views: a filter (categories, `relations`, `focus` + `depth`,
`hidden`, `only`), an optional `layout` (`positions` / `sizes` keyed by entity id that
override the shared ones while the view is active), `groups` (labelled boxes with
`position`, `size`, optional `color`) and `flows` (labelled arrows between
`{ "entity": id }` / `{ "group": id }` ends, optionally `dashed`). All of it affects only
what the GUI draws, never what is generated.

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
  "extra": { "aws": { "main": { "force_destroy": true } } }   // optional: extra provider arguments per block
}
```

**Extra arguments.** `extra` is provider id → block key → argument name → JSON value,
merged into the generated block after the mapping's own arguments (yours win). Nested
blocks are objects or lists of objects, decided by the bundled provider schema
(`ttg-schema`); `{"$ref": {"entity": …, "attr": …}}` becomes a traversal to another
resource and `{"$raw": "…"}` a raw expression. Native provider resources (type id
`native:<provider>:<tf type>`) are synthetic catalog entries whose single block takes
every argument from `extra`; the catalog creates them on demand (`Catalog::ensure_native`).

**Provider layers.** One diagram serves every provider. A node, container or edge may
carry `providers`; a provider's *layer* is the project minus entities not tagged for it,
minus entities whose abstract type is provider-scoped elsewhere (auto-tagged), with the
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
| `depends_on` | pure ordering, no attribute | anything → anything |

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
  ├─ files::assemble           network.tf, compute.tf, storage.tf, iam.tf, variables.tf,
  │                            outputs.tf, versions.tf, providers.tf, [backend.tf], MANUAL_STEPS.md
  ├─ tool::Profile             the ONLY place Terraform and OpenTofu differ
  └─ write / bundle            single dir, or out/<provider>/ per provider (+ optional zip)
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
   internet gateway, functions in subnets having an outbound route, and managed
   services (`network_agnostic`) drawn inside a network being informational only.

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
(`hashicorp/aws` vs `registry.opentofu.org/hashicorp/aws`), and an optional state `encryption`
block that only OpenTofu supports. Resource, variable and output generation never branch on the
tool. Registry lookups for schema reference default to the OpenTofu registry.

### 6.3 `validate` shell-out

`ttg-codegen::validate` looks for `terraform` / `tofu` on `PATH`. If found it runs
`init -backend=false -input=false` then `validate` in the export directory and returns the output.
If not found it returns `BinaryNotFound` and the GUI shows the exact command to run. The app has
no runtime dependency on either binary.

## 7. GUI architecture (`ttg-app`)

| module | responsibility |
|---|---|
| `app.rs` | `TtgApp`: project, catalog, camera, selection, history, clipboard, dialogs, cached diagnostics |
| `canvas/` | infinite pan/zoom surface; draws containers (back to front), edges (cubic beziers), nodes; hit-testing; drag-to-move (with reparenting on drop), drag-from-port-to-connect, marquee multi-select |
| `palette.rs` | searchable, category-grouped list of catalog types; click or drag onto canvas |
| `inspector.rs` | typed property editors generated from the definition (`string`, `bool`, `int`, `cidr`, `enum`, `string_list`); provider-specific fields under a per-provider header; inline validation |
| `menu.rs` | file new/open/save/save-as, undo/redo, tool toggle, target provider, export single / export all, validate |
| `clipboard.rs` | copy/paste of a sub-diagram as JSON (fresh ids, de-duplicated names, edges between copied items kept) |
| `camera.rs` | world/screen transform, zoom-about-pointer, zoom-to-fit |
| `history.rs` | snapshot-based undo/redo (the project is small; a clone per committed action is simpler and safer than command objects) |

Diagnostics are computed by `ttg-codegen::diagnostics::run` whenever the project revision counter
changes, and the result drives the warning badges (`manual`, `unmapped`, `partial`, `invalid`).
Export is disabled while any error-level diagnostic exists; the button tooltip lists them.

## 8. Non-goals for v1

Not implemented and not designed for: running `plan`/`apply` against live accounts, real-time
multi-user collaboration, cost estimation, drift detection, state-file visualisation or
management. The pipeline ends at "validated files on disk".

## 9. Licensing

Dual-licensed MIT / Apache-2.0. Definitions are authored from publicly documented provider
schemas (OpenTofu registry as canonical); no HashiCorp source code is vendored or depended on.
