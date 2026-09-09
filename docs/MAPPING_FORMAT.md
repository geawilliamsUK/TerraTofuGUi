# Resource Mapping File Format (schema_version 1)

This is the contract for `definitions/resources/*.toml` and `definitions/providers/*.toml`.
You do not need to know Rust to write one. Every file is validated when the catalog loads;
a mistake is reported with the file, the provider section and the argument name, and the
catalog refuses to load until it is fixed. Run `ttg catalog --definitions ./definitions`
to check your work.

The typed schema these files deserialize into is `crates/ttg-catalog/src/schema.rs`.

---

## 1. Resource definition

One file per abstract type. The file name must equal `resource.type`.

```toml
schema_version = 1

[resource]
type = "subnet"                       # abstract id: lowercase snake_case
category = "network"                  # palette group + default .tf file name
display_name = "Subnet"
description = "An address range inside a virtual network."
kind = "node"                         # "node" (leaf) or "container" (can hold others)
allowed_parents = ["virtual_network"] # container types this may be drawn inside
icon = "SUB"                          # short glyph shown on the canvas
```

Categories in use: `compute`, `storage`, `network`, `database`, `iam`, `serverless`,
`container`, `dns`, `load_balancer`, `secrets`, `monitoring`, `organization`
(grouping containers such as Resource Group / Project).

### 1.1 Abstract fields

Provider-neutral configuration the user edits in the inspector. The implicit field
`name` (the display name) always exists and must not be redeclared.

```toml
[[fields]]
name = "cidr_block"
label = "CIDR block"          # optional; defaults to name
type = "cidr"                 # string | bool | int | cidr | enum | string_list
required = true
default = "10.0.1.0/24"       # optional; must itself pass validation
description = "…"             # tooltip
options = ["a", "b"]          # enum only
pattern = "^[a-z0-9-]+$"      # optional regex (string only); no look-around
pattern_hint = "lowercase…"   # shown when the pattern fails
```

### 1.2 Relations

Which edges this type may be the *source* of. Direction is always
"source references / depends on target".

```toml
[[relations]]
kind = "network_membership"   # network_membership | attribute_reference | iam_binding | depends_on
label = "Belongs to network"
targets = ["virtual_network"]
cardinality = "one"           # one (required, exactly one) | optional | many
via_parent = true             # containment in a target container satisfies it
providers = ["aws"]           # v2, optional: only this provider's mapping uses the link
```

`depends_on` edges are always allowed and never need declaring. A relation scoped with
`providers` (a load balancer's security group, a peering's route tables) is simply not
the other providers' business: their mappings neither consume it nor warn that they
cannot, and the inspector labels it accordingly.

---

## 2. Provider mapping section

One `[providers.<id>]` table per provider the type supports. A missing section means
"unmapped": the node gets a badge, is skipped on export and listed in `MANUAL_STEPS.md`.

```toml
[providers.aws]
status = "full"        # full | partial | logical
file = "network"       # optional .tf file override (default: category)
notes = "…"            # shown in the inspector
```

- `partial` — the resource deploys but `manual_steps` (required) must be done afterwards.
- `logical` — nothing is emitted, on purpose, and no warning is raised. Used when a
  container has no equivalent (Resource Group on AWS). No `blocks` allowed.

### 2.1 Provider-specific fields and variables

```toml
[[providers.aws.fields]]          # same shape as abstract fields; stored under
name = "ami"                      # node.provider_config.aws.ami
type = "string"
required = true

[[providers.azure.variables]]     # input variable emitted into variables.tf
name = "ssh_public_key"           # *only when* some emitted block uses { var = … }
type = "string"
description = "…"
default = "…"                     # optional
sensitive = false
```

Provider field names must not shadow abstract field names.

### 2.2 Blocks

One abstract type becomes one or more concrete resource blocks. Blocks are emitted in
declaration order. The block named `main` (or the first block if none is named `main`)
is the **primary** block: relation references from other resources point at it unless
they name another block.

```toml
[[providers.azure.blocks]]
key = "nic"                            # local key; referenced by self_block / block
resource = "azurerm_network_interface" # concrete resource type
when = { relation = "iam_binding" }    # optional condition (see §2.4)

[providers.azure.blocks.args]          # arguments of the *last declared* block
name                = { template = "{name}-nic", transform = "kebab" }
location            = { var = "location" }
resource_group_name = { ancestor = "resource_group", attr = "name" }

[[providers.azure.blocks.nested]]      # nested block inside the last declared block
block = "ip_configuration"
labels = []                            # optional block labels
when = { field = "versioning" }        # optional condition

[providers.azure.blocks.nested.args]
name      = { value = "internal" }
subnet_id = { relation = "network_membership", attr = "id" }
```

Nested blocks may themselves contain `[[…nested.nested]]`.

HCL local names are derived from the node's display name: `main` block →
`<slug>`, other blocks → `<slug>_<key>` (e.g. `azurerm_network_interface.app_server_nic`).

### 2.3 Argument sources

Every argument value is a small inline table whose keys identify the source. An argument
whose source resolves to nothing (unset optional field, absent optional relation) is
simply omitted from the block.

| Source | Meaning |
|---|---|
| `{ value = <literal> }` | Constant. Strings, numbers, booleans, arrays and inline tables (→ objects). |
| `{ field = "x" }` | Abstract field on this entity. `field = "name"` is the display name. |
| `{ provider_field = "x" }` | Provider-specific field. |
| `{ var = "x" }` | `var.x`. Must be a provider-level or mapping-level variable. |
| `{ template = "{name}-nic" }` | String with `{field}`, `{provider.field}` or `{settings.key}` placeholders. |
| `{ map = "size", table = { small = "t3.micro" } }` | Look the field's value up in a table. Missing key is an error unless `optional = true`. |
| `{ relation = "kind", attr = "id" }` | Traversal to the related resource's primary block, e.g. `aws_vpc.main.id`. Add `block = "profile"` to target another block. |
| `{ ancestor = "resource_group", attr = "name" }` | Traversal to the nearest enclosing container of that type. Missing ancestor is an export error unless `optional = true`. |
| `{ self_block = "nic", attr = "id" }` | Traversal to another block of the same entity. Omitted if that block was not emitted. |
| `{ object = { K = <source>, … } }` | An object whose members are sources. |
| `{ list = [ <source>, … ] }` | A list of sources. |
| `{ func = "jsonencode", args = [ <source>, … ] }` | A function call. |
| `{ raw = "…" }` | Raw HCL expression, parsed by hcl-rs. Last resort; prefer the above. |

Modifiers accepted by `field`, `provider_field`, `relation`, `self_block`:

- `wrap = "list"` — wrap a scalar in a list (`address_prefixes = ["10.0.1.0/24"]`).
- `optional = true` — never fail, just omit.
- `transform = "slug" | "kebab" | "lower" | "alnum"` (`field`, `provider_field`,
  `template`) — normalise a string, e.g. display name `App Server` → `app-server` for
  resource names that forbid spaces.

Deep structures can be written as TOML tables instead of inline (see `iam_role.toml`'s
`assume_role_policy`).

### 2.4 Conditions (`when`)

- `{ relation = "iam_binding" }` — at least one target exists (edge or `via_parent`).
- `{ field = "versioning" }` — the field is truthy (true / non-zero / non-empty).
- `{ field = "permissions", equals = "admin" }` / `not_equals = "none"`.
- `{ provider_field = "…" }` with the same `equals` / `not_equals` options.

### 2.5 Outputs and manual steps

```toml
[providers.aws.outputs]              # output "<slug>_<key>" { value = <block>.<local>.<attr> }
id  = { attr = "id", description = "Subnet id" }
arn = { attr = "arn", block = "main" }

[[providers.azure.manual_steps]]     # rendered into MANUAL_STEPS.md; required if status = "partial"
title = "Check the role assignment scope"
body  = """Markdown body…"""
```

---

## 2.6 schema_version 2 features

Declare `schema_version = 2` to use any of the following. A version 1 file that uses
them is rejected at load time, so the version number stays honest. Version 1 files remain
valid and unchanged.

**Table fields.** `type = "struct_list"` with `[[fields.items]]` sub-fields (any type
except another struct_list). The inspector shows a row table; the value is a list of rows.

```toml
[[fields]]
name = "rules"
type = "struct_list"
[[fields.items]]
name = "port"
type = "int"
```

**Repeated blocks.** `for_each_field = "rules"` on a `[[…blocks]]`, `[[…data]]` or
`[[…nested]]` entry emits one block per row (or per entry of a `string_list`, whose row has
a single item `value`). Top-level repeated resources are named `<slug>_<key>_<n>`. Inside:

| Source / condition | Meaning |
|---|---|
| `{ item = "port" }` | value of a sub-field of the current row (`wrap`, `transform`, `optional` apply) |
| `{ item_index = { base = 100, step = 10 } }` | `base + step × row index`, e.g. NSG priorities |
| `{ map = "protocol", table = … }` | `map` looks in the row first, then in the entity's fields |
| `{ template = "{item.from}-{item.to}" }` | `item.` placeholders; `{item.index}` is the row index |
| `when = { item = "direction", equals = "ingress" }` | row filter; `not_equals` and `equals_item = "other"` also work |

A `self_block` reference to a repeated block yields the list of all instances.

**One block per linked resource.** `for_each_relation = "attachment"` (optionally
narrowed with `for_each_target_type = "subnet"`) emits one block per target of that
relation. Inside, `{ target = "id" }` is a traversal to the target's primary block
(`block = "nic"` addresses a secondary block of the target), and templates may use
`{target.name}` / `{target.slug}`. Targets that are manual or unmapped become input
variables exactly like `relation` references. Used by Route Table (one association per
subnet) and Load Balancer (one attachment per instance).

**Data sources.** `[[providers.<id>.data]]` entries have the same shape as blocks (`key`,
`resource`, `when`, `for_each_field`, `args`, `nested`) and emit `data "<resource>"
"<slug>_<key>"`. Reference them with `{ self_data = "<key>", attr = "id" }`, which becomes
`data.<resource>.<slug>_<key>.id`.

**Fallbacks and conditionals.**

```toml
ami = { provider_field = "ami", fallback = { self_data = "ubuntu", attr = "id" } }

[providers.azure.blocks.args.destination_port_range]
if   = { item = "protocol", equals = "all" }
then = { value = "*" }
else = { item = "from_port" }          # `else` may itself be another if-table
```

`fallback` on `field` / `provider_field` is used when the field is unset. `if` takes any
condition (relation, field, provider_field, item).

**Relation target filters.** `{ relation = "attribute_reference", target_type = "security_group", attr = "id" }`
and `when = { relation = …, target_type = … }` only consider linked resources of that
abstract type. An edge to a type no source consumes still becomes `depends_on` plus a
manual step, so one relation kind can serve several target types with different meanings.

**Per-entity sensitive variables.** `{ entity_var = "value", sensitive = true, description = "Value of the secret \"{name}\"." }`
declares an input variable named `<slug>_<name>` for this entity and resolves to
`var.<slug>_<name>`. Use it for secret values, passwords and deployment packages so the
diagram never carries them. `type` and `default` are optional; the description may use
template placeholders.

**Sensitive outputs.** `[providers.<id>.outputs] kube_config = { attr = "kube_config_raw", sensitive = true }`
adds `sensitive = true` to the output, which Terraform requires when the attribute is sensitive.

**Conditions, extended.** `{ relation = "…", absent = true }` holds when there is *no*
such target; `{ all = [ … ] }` and `{ any = [ … ] }` combine conditions. When a field or
provider field has no value, conditions use the field's declared `default`;
`{ field = "x", absent = true }` / `{ provider_field = "x", absent = true }` test for
"no value at all" (defaults do not count). `{ ancestor = "servicebus_namespace" }` holds
when the entity is drawn inside a container of that type (`absent = true` inverts), so a
mapping can create a helper resource only when no enclosing container provides it.

**Conditions on targets.** `{ relation = "network_membership", target_provider_field =
"delegation", equals = "postgres_flexible" }` (or `target_field`) counts only targets whose
field matches, so a check can say "no linked subnet is delegated". Inside a
`for_each_relation` block, `{ target_shares_ancestor = "virtual_network" }` holds when the
current target sits in the same container of that type as the entity itself; a peering
uses it to route each linked route table towards *the other* network.

**Referencing a target's container.** `{ relation = "sends_to", target_type = "event_queue",
ancestor = "servicebus_namespace", attr = "default_primary_connection_string" }` resolves
to the *enclosing container* of each linked queue rather than the queue itself (targets
without one are skipped); `{ target = "id", ancestor = "…" }` does the same inside a
`for_each_relation` block. `fallback = { … }` on a relation source is used when the
reference resolves to nothing, e.g. `block = "ns"` when the queue created its own
namespace, falling back to the enclosing namespace container otherwise.

**Provider layers.** Every node, container and link in a project may carry
`providers = ["azure"]` (empty = all). Codegen, diagnostics and reachability run on the
provider's *layer*: the graph minus entities not tagged for it, minus entities whose type
is provider-scoped elsewhere, with the contents of a dropped container re-parented to
the nearest kept one. The difference is reported as `Layer` diagnostics (parity report).

**Entity references in rows.** An item of `type = "entity_ref"` with `targets = [...]`
holds the id of another node (the inspector shows a dropdown). Inside the row,
`{ item_ref = "source_group", attr = "id" }` is a traversal to that node's primary block
(`block = "…"` for a secondary one). Used by Security Group rules whose source is another
group.

**Conditional manual steps.** `[[providers.<id>.manual_steps]]` may carry `when = <condition>`;
a `partial` mapping only warns when at least one step applies.

**Design-time checks.** `[[providers.<id>.checks]]` with `when`, optional
`for_each_field`, `severity = "warning" | "error"` and a `message` using `{name}` and
`{item.<x>}` placeholders. Errors block export like any other diagnostic.

**Field-level guards.** `required_unless_relation = "<kind>"` makes a field required only
when the entity has no target for that relation (a function's storage account name unless
it is attached to an Object Storage node). `unique_scope = "storage_account"` reports an
error when two entities use the same value in that scope for the provider. On relations,
`min_targets = 2` warns when fewer are linked; on `[resource]`, `expects_incoming = true`
warns when nothing links to an instance (a log group nobody logs to), and
`network_agnostic = true` marks a managed service that may be drawn inside a Virtual
Network but gains nothing from it (the app says so as an informational note).

**Provider-scoped types.** `providers = ["azure"]` on `[resource]` declares a type that
exists on those providers only (Azure Storage Queue: SQS is what the portable Event Queue
maps to, so there is no AWS equivalent). The loader requires a mapping for each listed
provider; using the type with any other target provider is an *error* (export blocked)
rather than the "no mapping" warning, the palette tags it "Azure only", and the concrete
display mode labels it the same way. Prefer a portable type with provider options (Event
Queue's AWS `fifo` / Azure `sku`) when the difference is a setting; use a scoped type
when the service itself has no counterpart.

Cross-resource network rules (CIDR overlap, zone vs region, one route table per subnet,
egress routes) are built into the engine rather than declared per definition; see
ARCHITECTURE.md §6.0.

See `definitions/resources/function.toml`, `security_group.toml`, `relational_database.toml`
and `secret.toml` for worked examples of every feature.

---

## 3. What the generator does with a mapping

1. **Diagnostics** check required fields, patterns, allowed parents, relation
   cardinality, required ancestors and provider coverage. Errors block export.
2. **Plan**: for every entity and block, evaluate `when`; the surviving `(entity, block)`
   pairs are the emission set.
3. **Resolve** each argument source to an `hcl` expression. Relation and ancestor
   references to *emitted* resources become attribute traversals (implicit dependency
   ordering). References to a **manual** or **unmapped** resource become an input
   variable `var.<slug>_<attr>` plus a MANUAL_STEPS entry, so the output still validates.
4. **`depends_on`** is added to the primary block for `depends_on` edges and for edges
   whose relation kind the mapping never consumes (those also become a manual step).
5. Blocks are written to `<file>.tf` in dependency order, plus `variables.tf`,
   `outputs.tf`, `versions.tf`, `providers.tf`, optional `backend.tf`, `README.md`,
   and `MANUAL_STEPS.md` when there is anything to say.

---

## 4. Provider definition

```toml
schema_version = 1
required_ancestor = "resource_group"   # optional: every resource must be inside one

[provider]
id = "azure"
display_name = "Microsoft Azure"
source_namespace = "hashicorp"
source_name = "azurerm"
version_constraint = "~> 4.0"
local_name = "azurerm"                 # optional; defaults to source_name

[[variables]]                          # always emitted; defaults come from the project's
name = "location"                      # provider settings, then from `default` here
type = "string"
description = "Azure region"
default = "uksouth"

[provider_block.args]                  # provider "<local_name>" { … } — var / value / raw only
subscription_id = { var = "subscription_id" }

[[provider_block.nested]]              # nested blocks with literal args only
block = "features"
```

The Terraform / OpenTofu difference (registry address prefix, `required_version`) is not
part of the definition; it is applied by `ttg-codegen::tool::Profile`.

---

## 5. Checklist for a new definition

1. Copy the closest existing file in `definitions/resources/`.
2. Fill `[resource]`, abstract `[[fields]]`, `[[relations]]`.
3. For each provider: pick `status`, add provider `fields` for anything that cannot be
   expressed abstractly, write `blocks`, `outputs`, and `manual_steps` for gaps.
4. Add the file to the `BUILTIN_RESOURCES` list in `crates/ttg-catalog/src/load.rs`
   (one line) so it is embedded, or test it first with `--definitions`.
5. `cargo run -p ttg-cli -- catalog` must load cleanly.
6. Add or extend an example project under `examples/` and export it for every provider;
   run `terraform validate` / `tofu validate` on the output if you have the binary.
