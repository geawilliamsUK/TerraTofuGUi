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
options = ["a", "b"]          # enum: the only allowed values
pattern = "^[a-z0-9-]+$"      # optional regex (string only); no look-around
pattern_hint = "lowercase…"   # shown when the pattern fails
```

### 1.2 Relations

Which edges this type may be the *source* of. Direction is always
"source references / depends on target".

```toml
[[relations]]
kind = "network_membership"   # network_membership | attribute_reference | iam_binding |
                              # attachment | sends_to | reads | logs_to | encrypted_with |
                              # dead_letters_to | calls | depends_on
label = "Belongs to network"
targets = ["virtual_network"]
cardinality = "one"           # one (required, exactly one) | optional | many
via_parent = true             # containment in a target container satisfies it
providers = ["aws"]           # v2, optional: only this provider's mapping uses the link
```

`depends_on` edges are always allowed and never need declaring. `calls` is documentation
only: it records that one service makes requests of another, and the engine generates
nothing from it — no `depends_on`, no manual step, no "cannot express" diagnostic, and it
is left out of the dependency graph, so two services calling each other is not a cycle.
Declare it (`targets`, `cardinality`) so the inspector offers it and the "not a declared
target" check still applies; no mapping is expected to consume it. A relation scoped with
`providers` (a load balancer's security group, a peering's route tables) is simply not
the other providers' business: their mappings neither consume it nor warn that they
cannot, and the inspector labels it accordingly.

There are only ten usable relation kinds, so a type may declare the **same kind more than
once**, one entry per group of target types — a DNS Record's `attribute_reference` is both
"In zone" (a `dns_zone`) and "Alias of" (a `load_balancer` or `cdn`). The declarations must
have disjoint `targets`; each then owns only links to its own targets, so cardinality,
`via_parent` and the "not a declared target" warning are judged per declaration. Mappings
tell them apart with `target_type` (§2.6).

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
  container has no equivalent (Resource Group on AWS). No `blocks` allowed. A logical
  mapping *may* still declare `manual_steps`: they land in MANUAL_STEPS.md without raising
  a diagnostic, which is how a type whose provider needs a whole other Terraform provider
  (User Identity on Azure) says what to do instead.

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
| `{ relation = "kind", attr = "id" }` | Traversal to the related resource's primary block, e.g. `aws_vpc.main.id`. Add `block = "profile"` to target another block. `attr = ""` is the block itself, which is what a `depends_on` list wants. |
| `{ ancestor = "resource_group", attr = "name" }` | Traversal to the nearest enclosing container of that type. Missing ancestor is an export error unless `optional = true`. |
| `{ self_block = "nic", attr = "id" }` | Traversal to another block of the same entity. Omitted if that block was not emitted. |
| `{ object = { K = <source>, … } }` | An object whose members are sources. |
| `{ list = [ <source>, … ] }` | A list of sources. |
| `{ func = "jsonencode", args = [ <source>, … ] }` | A function call. |
| `{ raw = "…" }` | Raw HCL expression, parsed by hcl-rs. Last resort; prefer the above. See `refs` in §2.6 for splicing traversals into one. |

Modifiers accepted by `field`, `provider_field`, `relation`, `self_block`:

- `wrap = "list"` — wrap a scalar in a list (`address_prefixes = ["10.0.1.0/24"]`).
- `wrap = "map"` (v2, `field` / `provider_field` only) — read a `string_list` of
  `key=value` entries as an object: `labels = { workload = "asr" }`. Everything after the
  first `=` is the value; an entry without one gets an empty value.
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

An output whose `block` is a repeated one (`for_each_field` / `for_each_relation`) has no
single address, so its value is the **list** of every instance's attribute — the repository
URLs of a Container Registry, the mount address of a File System in each subnet. An output
whose block is not emitted at all is simply left out.

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

**Restricted list values.** `options = [ "GET", "HEAD", "PUT" ]` on a `type = "string_list"`
field restricts every entry to that set, the same way it restricts an `enum`'s single
value; an empty `options` (the default) still means "any string". Object Storage's
`cors_methods` uses it to keep a CORS rule to the HTTP methods a bucket can actually
answer.

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

**One column of a table field.** `{ field = "taints", column = "key" }` (also on
`provider_field`) is the list of that sub-field's values across the rows of a `struct_list`,
for arguments that want parallel lists rather than repeated blocks:
`node_taints = formatlist("%s=%s:%s", ["gpu"], ["present"], ["NoSchedule"])`. It cannot be
combined with `wrap` or `transform`, and an empty table falls through to `fallback`.

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

**Incoming links.** `{ relation = "logs_to", incoming = true }` looks at the edges that
point *at* this entity rather than away from it — "somebody logs to me". `target_type`
then filters the *other* end's type, and `absent = true` still inverts. Containment never
stands in for an incoming edge, so `via_parent` plays no part. An incoming condition or
source does not count as *consuming* the relation, because the edge belongs to whoever drew
it, and neither is checked against this type's own `[[relations]]` — only the kind and the
other end's type exist to check. Object Storage uses the condition for the log-delivery
statement a bucket needs in its own policy when other buckets send their access logs to it.

`incoming` also works as an **argument source**, so an entity can reference or read the
entities that point at it. Several sources give several values, so `wrap = "list"` applies
as usual and a plain reference takes the first (write a check with `min_count` when more
than one would be wrong).

```toml
# aws_cloudwatch_log_group: the group an EKS cluster logs to must carry the name EKS
# would give it, or EKS creates a second one and owns it.
name = { if = { relation = "logs_to", incoming = true, target_type = "kubernetes_cluster" }, then = { func = "format", args = [ { value = "/aws/eks/%s/cluster" }, { relation = "logs_to", incoming = true, target_type = "kubernetes_cluster", field = "name", transform = "kebab" } ] }, else = { field = "name", transform = "kebab" } }
```

**A linked entity's field, not its attribute.** `field = "name"` on a relation source (with
or without `incoming`) reads that *abstract field* off the other entity and renders it as a
literal, where `attr` would render a traversal to its resource. `transform` applies; the two
cannot be combined, and neither can `field` and `block`. The point is that a literal creates
no dependency: the log group above is named after its cluster while the cluster
`depends_on` the log group, which an `attr` reference would turn into a cycle. When
`target_type` is given the field name is checked against that type.

**Counting targets.** `min_count = 2` on a relation condition holds when at least that many
targets (or sources, with `incoming`) match, rather than "at least one". It is how a check
says *more than one*: a Log Group with two incoming clusters cannot be named after both, so
`log_group.toml` reports it as an error. `min_count = 0` is rejected — `absent = true` is
what "none" means.

**Comparing against a list field.** `equals` / `not_equals` on a `string_list` field (or on
`target_field` when the target's field is one) test **membership**: `{ field = "addons",
equals = "cluster_autoscaler" }` holds when that entry is in the list, and a Kubernetes
Node Pool asks the same question of its cluster with `{ relation = "attachment",
target_type = "kubernetes_cluster", target_field = "addons", equals = "cluster_autoscaler" }`.
A single-entry list compares as its one entry either way, so nothing that used to match
stops matching. Bools compare as `"true"` / `"false"`, and an unset field falls back to its
declared default, so `{ target_field = "iam_authentication", not_equals = "true" }` is the
honest way to write "unless the database has it switched on".

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
uses it to route each linked route table towards *the other* network. Add
`relation = "dead_letters_to"` (optionally with `target_type`) to ask the same question of
a relation's targets instead of the current row, which works outside a repeated block:
an Event Queue only auto-forwards its dead letters when the queue it dead-letters to is in
the same Service Bus namespace.

**Referencing a target's container.** `{ relation = "sends_to", target_type = "event_queue",
ancestor = "servicebus_namespace", attr = "default_primary_connection_string" }` resolves
to the *enclosing container* of each linked queue rather than the queue itself (targets
without one are skipped); `{ target = "id", ancestor = "…" }` does the same inside a
`for_each_relation` block. `fallback = { … }` on a relation source is used when the
reference resolves to nothing, e.g. `block = "ns"` when the queue created its own
namespace, falling back to the enclosing namespace container otherwise.

**Raw expressions with references (`refs`).** `{ raw = "…", refs = { <name> = <source> } }`
resolves each source, renders it as HCL text and substitutes it for `@name@` in `raw` before
the whole string is parsed. It is the escape hatch for expressions the source language has
no form for — a `for_each` comprehension, a `for` projection — without hard-coding the local
names the emitter chooses. Every `@name@` must have a `refs` entry and every entry must be
used; a ref that resolves to nothing omits the whole argument.

```toml
# aws_route53_record, one per entry of a certificate's computed validation options
for_each = { raw = "{ for o in @options@ : o.domain_name => o }", refs = { options = { self_block = "main", attr = "domain_validation_options" } } }
name     = { raw = "each.value.resource_record_name" }
```

`for_each`, `count`, `provider` and `lifecycle` are meta-arguments: the schema check accepts
them on any block.

**Built-in resources.** A block may use `resource = "terraform_data"` (built into
Terraform and OpenTofu) to hold a value other resources reference, e.g. the network tag
a GCP security group hands to its members; the schema check skips it.

**String-prefix / suffix conditions.** `{ field = "x", starts_with = "sg-" }` /
`not_starts_with = "…"` test the resolved value's prefix (ignored when `equals` /
`not_equals` is also given). Add `transform = "slug" | "kebab" | "lower" | "alnum"` to
normalise the value first, e.g. to the kebab-cased form the mapping actually emits before
checking it against a provider naming rule. `field = "name"` is the entity's display name,
same as in an argument source or a template. `ends_with` / `not_ends_with` test the
suffix the same way, and both are also available on `provider_field` conditions (which
have no `starts_with`/`transform`) — a database's Performance Insights check uses
`{ provider_field = "instance_class", ends_with = ".micro" }` to catch an instance-class
override that still lands on a size with no Performance Insights support.

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
a `partial` mapping only warns when at least one step applies. When that condition names a
relation the mapping cannot express (a Load Balancer forwarding to a Kubernetes Cluster on
Azure), the step replaces the generic "Link X to Y by hand" entry that §3 step 4 would
otherwise add — the `depends_on` for ordering and the "cannot express" diagnostic stay,
because they are still true.

**Design-time checks.** `[[providers.<id>.checks]]` with `when`, optional
`for_each_field`, `severity = "warning" | "error" | "omit"` and a `message` using `{name}`
and `{item.<x>}` placeholders. `warning` is the default; errors block export like any
other diagnostic.

`severity = "omit"` is for the case where the provider has no way to express *this one
entity* and the rest of the design is fine: an Alarm on a metric Azure Monitor does not
publish should not stop the whole Azure export. The entity is taken off that provider's
layer exactly as if it were tagged `providers` without that provider — it is left out of
the export, links to and from it are dropped, its children are re-parented — and the
check's message is reported once, as a **warning**, with `; left out of the <Provider>
export` appended. Nothing that referenced it breaks, because the reference went with it.
The entity is still part of the project, so it still draws on the canvas; a view filtered
by `providers` treats it as off that layer, like any other off-layer entity.

Use `omit` when omission is the honest outcome and `error` when the design is wrong
everywhere. If one provider is the reference for a portable field (AWS is, for the Alarm's
metric presets), keep that provider's check an `error`: a combination the reference
refuses is a mistake, not a gap.

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

See `definitions/resources/function.toml`, `security_group.toml`, `relational_database.toml`,
`secret.toml` and `kubernetes_node_pool.toml` for worked examples of every feature;
`tls_certificate.toml` for `refs` and `dns_record.toml` for two relations sharing a kind.

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

### 4.1 Provider aliases

Some resources have to be created in a fixed place whatever the project's region:
CloudFront reads its certificate and its Web ACL from **us-east-1** only. An *alias* is a
second configuration of the same provider that a block can send itself to.

```toml
[[aliases]]
name = "us_east_1"                     # also the HCL identifier: `aws.us_east_1`
description = "CloudFront's home region: its certificates and Web ACLs live here"
args = { region = { value = "us-east-1" } }   # provider-block arguments this one replaces
```

`args` are ordinary argument sources (`var` / `value` / `raw`, as on the provider block)
and *override* the normal block's arguments of the same name; everything the alias does not
mention — the other arguments, the nested blocks, the project's default tags — is copied
from it, so an aliased configuration tags what it creates exactly like the main one.

A block claims an alias with `provider_alias` (schema_version 2):

```toml
[[providers.aws.blocks]]
key = "global"
resource = "aws_wafv2_web_acl"
provider_alias = "us_east_1"
when = { relation = "attribute_reference", incoming = true, target_type = "cdn" }
```

The emitter writes `provider = aws.us_east_1` into that resource and adds the
`provider "aws" { alias = "us_east_1" … }` block to `providers.tf` — **only when some
emitted block actually uses it**, so a configuration without a CDN never asks for a second
set of credentials. An alias a block references must be declared by that provider, and the
name must be a valid HCL identifier; both are checked at load time.

Nothing else changes: the aliased resource is an ordinary block with its own key, so
outputs, `self_block` and cross-resource references (`block = "global"`) address it as
usual. `web_application_firewall.toml` and `tls_certificate.toml` use this to grow a
second, CloudFront-scoped copy of themselves when a `cdn` links to them.

### 4.2 Helper providers

A mapping sometimes needs one resource from a small side provider — a `random_password`
for a generated secret value. Declaring it as a *helper* keeps it out of the way: there is
no provider block, no variables and no mappings of its own, and it only reaches
`required_providers` when some emitted block's resource type starts with its `prefix`, so
`init` never downloads a provider the configuration does not reference.

```toml
[[helper_providers]]
source = "hashicorp/random"
version = "~> 3.6"
prefix = "random_"
local_name = "random"                  # optional; defaults to the part after the slash
```

A block whose `resource` belongs to a helper provider is skipped by the schema check (the
bundled index only carries the target providers), so keep such blocks small and literal.

### 4.3 Project-wide default tags

`Settings::tags` (Settings ▸ Default tags in the app, `tags` on the MCP `settings_set`
tool) is one map of tags for the whole project. Each provider definition says where they
land; nothing in a *resource* definition changes.

```toml
[default_tags]                         # AWS: a nested block on the provider block
block = "default_tags"
arg = "tags"

[default_tags]                         # Google Cloud: an argument on the provider block
arg = "default_labels"
sanitize_labels = true                 # lowercase, punctuation to '-' (labels are strict)

[default_tags]                         # Azure: merged into each resource that has one
resource_arg = "tags"
```

Exactly one of `arg` and `resource_arg` must be set, and `block` only goes with `arg`.
With `resource_arg` the emitter adds the argument to every emitted resource whose provider
schema has it, after the mapping has run; a tag the mapping set itself (a resource's own
`Name`) wins over the project's.

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
