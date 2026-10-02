# The `.ttg.json` project file

A TerraTofu GUI project is one JSON file, `<name>.ttg.json`. The app saves and opens it,
`ttg` checks and exports it without the app, and the MCP server reads it
(`project_get`, the `ttg://project` resource) and loads it (`project_import`). It is
therefore also an **interchange format**: anything that can write JSON (a script, a
cloud agent session with no route to your machine, a person) can produce a diagram, and
a person opens it or exports it from the command line.

- **Schema:** [`schemas/project.schema.json`](../schemas/project.schema.json), JSON
  Schema draft 2020-12, generated from the Rust types (`ttg schema project`) and
  published at its `$id`,
  `https://raw.githubusercontent.com/geawilliamsUK/TerraTofuGUi/master/schemas/project.schema.json`.
  A test fails when the checked-in copy is out of date.
- **Version:** `schema_version`, currently **1** (`ttg_core::SCHEMA_VERSION`).
- **Example:** [`examples/minimal.ttg.json`](../examples/minimal.ttg.json) is the smallest
  file that exports cleanly; every file in `examples/` is a working project.

## Versioning

`schema_version` says which shape the file has. A build reads every version up to its
own and refuses a newer one with "unsupported project schema_version N (this build
supports M)". Within a version, changes are additive: a new key is optional and has a
default, so an older file always loads. When a change cannot be additive the version goes
up and the loader migrates older files forward (`ttg_core::project::load_str`); the app
saves the current version.

## A minimal file

One network, one subnet inside it, one function, and the role the function runs as
(AWS will not run a function without one):

```json
{
  "schema_version": 1,
  "name": "minimal",
  "settings": {
    "tool": "opentofu",
    "target_provider": "aws",
    "provider_settings": { "aws": { "region": "eu-west-2" } }
  },
  "containers": {
    "net": {
      "id": "net",
      "name": "main",
      "container_type": "virtual_network",
      "config": { "cidr_block": "10.0.0.0/16" },
      "position": { "x": 40, "y": 40 },
      "size": { "w": 320, "h": 240 }
    }
  },
  "nodes": {
    "app-subnet": {
      "id": "app-subnet",
      "name": "app",
      "resource_type": "subnet",
      "parent": "net",
      "config": { "cidr_block": "10.0.1.0/24" },
      "position": { "x": 80, "y": 120 }
    },
    "worker": {
      "id": "worker",
      "name": "worker",
      "resource_type": "function",
      "config": { "runtime": "python3.12", "handler": "app.handler", "memory_mb": 128, "timeout_seconds": 30 },
      "position": { "x": 440, "y": 80 }
    },
    "worker-role": {
      "id": "worker-role",
      "name": "worker role",
      "resource_type": "iam_role",
      "config": { "trusted_service": "serverless", "permissions": "none" },
      "position": { "x": 440, "y": 240 }
    }
  },
  "edges": [
    { "source": "worker", "target": "worker-role", "relation": "iam_binding" }
  ]
}
```

`ttg check examples/minimal.ttg.json --schema` reports nothing for AWS; the lines it
prints for Azure and Google Cloud say what those providers would still need (a Resource
Group around everything on Azure, a function app name) and never block the AWS export.
Leave the function's `config` out and the same command prints, with the entity's line:

```text
minimal.ttg.json:28: error [worker] Handler: required
minimal.ttg.json:28: error [worker] Memory (MB): required
```

**Defaults are not filled in from a file.** When the app or an agent *adds* an entity,
its fields start at the definition's defaults; a file is taken as written. Spell out every
field the type marks `required` (they are listed by `catalog_type`, and in
`definitions/resources/<type>.toml`).

## Top level

| Key | Required | What |
|---|---|---|
| `schema_version` | yes | `1` |
| `name` | yes | Project name; heads the export's README |
| `settings` | no | Tool, target provider, provider variables, tags, state backend and encryption, version pins, Kubernetes manifests, cost assumptions (defaults: OpenTofu, AWS, nothing else) |
| `containers` | no | Entities that hold others (networks, resource groups, clusters), keyed by id |
| `nodes` | no | Every other entity, keyed by id |
| `edges` | no | Links between entities, a list |
| `views` | no | Saved views: filters, per-view layouts and drawn annotations (groups, flows, notes). Never exported; leave it out when writing by hand |

A file may carry `"$schema": "<the URL above>"` so an editor completes and checks it as
you type; the app ignores the key and does not write it back. Any other unknown key is
ignored too, which is why `ttg check --schema` warns about one: it is usually a
misspelling.

## Entities

Containers and nodes have the same fields, except that a container's type is
`container_type` and its `size` is required, and a node's type is `resource_type`.

- **Ids.** The key and the `id` inside must be equal. Any string will do; the app makes
  `<prefix>-<8 hex digits>` (`subnet-3fa2b9c1`). Ids are what links, `parent` and
  references use, and they never appear in the generated HCL.
- **Names.** `name` is the display name. Names must stay unique once *slugified*
  (lower-cased, anything but letters and digits becoming `_`), because the slug is the
  Terraform resource name: `worker role` becomes `aws_iam_role.worker_role`.
- **Types.** One of the catalog's abstract types (`ttg catalog`, or `catalog_types` over
  MCP), or a native provider resource, `native:<provider>:<resource>`
  (`native:aws:aws_vpc_endpoint`), whose arguments all go in `extra`.
- **Containment.** `parent` is the id of the container the entity is drawn in. Each type
  lists the containers it may sit in (`allowed_parents`). Containment means something:
  for relations marked `via_parent`, being inside a container counts as the link (a
  subnet inside a virtual network belongs to it, with no edge needed), and a function or
  instance whose subnet is linked runs inside that network.
- **Position.** `position: {x, y}` is the top-left corner on the canvas; `size: {w, h}`
  is optional for nodes. Positions only affect the drawing; overlapping boxes are fine and
  `layout_tidy` (or Edit ▸ Arrange ▸ Tidy layout in the app) sorts them out.
- **`manual: true`** marks something that exists already or is made by hand: no blocks are
  generated for it, and the export's `MANUAL_STEPS.md` says so.
- **`providers: ["aws"]`** keeps an entity (or a link) in that provider's layer only;
  empty or absent means every provider.
- **`classification`** (`public`, `internal`, `confidential`, `personal`, `payment`),
  **`description`** and **`owner`** say what the resource is for; they become tags and an
  HCL comment.

### `config` and `provider_config`

`config` holds the type's abstract fields by name: `cidr_block`, `runtime`,
`memory_mb`. Values are JSON booleans, numbers, strings, lists of strings, or, for a
`struct_list` field such as a security group's rules, a list of objects. An `enum`
field takes one of its `options`; an `entity_ref` field (inside rules) takes an entity's
id. `provider_config` holds the fields that exist on one provider only, under its id:
`{ "aws": { "enable_dns_hostnames": true } }`. Unknown fields and wrong types are
reported by the diagnostics with the field's label.

### `extra`: provider arguments the mapping does not set

```json
"extra": {
  "aws": {
    "main": {
      "force_destroy": true,
      "log_group_name": { "$ref": { "entity": "app logs", "attr": "name" } },
      "policy": { "$raw": "data.aws_iam_policy_document.extra.json" }
    }
  }
}
```

The outer key is the provider, the next one the block the arguments go into (`main` is
the type's primary block; other keys name its secondary blocks, as `catalog_type` lists
them), then argument names exactly as the provider schema spells them (`ttg schema show
aws aws_s3_bucket`). Values are JSON, written as HCL; nested blocks are objects or lists
of objects. Two special values:

- `{"$ref": {"entity": "<id or name>", "attr": "<attribute>"}}` refers to another
  entity's primary block (`aws_cloudwatch_log_group.app_logs.name`); add
  `"block": "<key>"` to refer to one of its secondary blocks instead. The reference is
  also a dependency, and an `attr` the schema does not have is an error.
- `{"$raw": "<expression>"}` is copied into the HCL as it is.

Arguments are checked against the provider schema; for a native resource, `extra` is
where every argument goes.

## Links

```json
{ "source": "worker", "target": "worker-role", "relation": "iam_binding", "providers": [] }
```

A link reads "source uses / depends on target". `relation` is one of
`network_membership`, `attribute_reference`, `iam_binding`, `attachment`, `sends_to`,
`reads`, `logs_to`, `encrypted_with`, `dead_letters_to`, `calls` or `depends_on` (the
schema's `Relation`). Which relation a type may have to which target is declared in its
definition's `[[relations]]` (`catalog_relations` over MCP lists them, with their labels
such as "Runs as role / identity"); `depends_on` is allowed between any two entities.
Leave out a link that containment already implies (a subnet's `network_membership` to the
network it is drawn in): the app refuses to draw one. `layout` (which side of each box the
line attaches to) is optional; `providers` works as on entities.

## Settings

```json
"settings": {
  "tool": "opentofu",
  "target_provider": "aws",
  "provider_settings": { "aws": { "region": "eu-west-2" }, "azure": { "location": "uksouth" } },
  "tags": { "Project": "minimal" },
  "backend": { "type": "s3", "args": { "bucket": "acme-tfstate", "region": "eu-west-2" } },
  "state_encryption": false,
  "provider_versions": { "aws": "~> 6.0" },
  "kubernetes_manifests": false
}
```

Everything is optional. `tool` is `opentofu` or `terraform`; `target_provider` is what
`ttg check` and `ttg export` use when no `--provider` is given.

## Checking and using a file

| To | Run |
|---|---|
| Check it against the schema (every problem, with its line), then the catalog | `ttg check project.ttg.json --schema` |
| Check one provider's diagnostics | `ttg check project.ttg.json --provider azure` |
| Write the Terraform / OpenTofu directory | `ttg export project.ttg.json --provider aws --out ./out/aws [--validate]` |
| Every provider, zipped | `ttg export-all project.ttg.json --out ./out --zip` |
| Open it in the app | File ▸ Open, or `terratofu-gui project.ttg.json` |
| Load it into a running app over MCP | `project_import { json, replace: true }` |

`ttg check --schema` exits 1 when the file has a schema error or a diagnostic error.
Unknown keys are warnings; the line of an entity's diagnostics is the line its id key is
on.

## Workflow: an agent without the MCP server

A cloud agent session (claude.ai, Claude Code on the web) cannot reach a server on your
machine unless you publish it through a tunnel (README, "Use TerraTofu from a cloud
session"). Without one, the file is the hand-over:

1. The agent writes `<name>.ttg.json` against the schema, starting from
   `examples/minimal.ttg.json` or an example close to the design, and filling every
   required field (`definitions/resources/<type>.toml` lists fields, defaults and
   relations).
2. If it can run commands, it runs `ttg check <file> --schema` and fixes what is
   reported, line by line, until there are no errors for the target provider; then
   `ttg export <file> --out ./out/aws --validate` proves the result.
3. It commits or hands over the file. You open it in TerraTofu GUI (positions are
   only a starting point: Edit ▸ Arrange ▸ Tidy layout), or run `ttg export` yourself.
4. With the app running and a local agent connected, `project_import` loads the same
   text straight into the open window as one undo step, asking you first when that
   would replace unsaved work.
