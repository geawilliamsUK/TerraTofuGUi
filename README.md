# TerraTofu GUI

A native desktop application, written in Rust, for drawing cloud architecture on a
canvas — resources as nodes, relationships as edges, VPCs / resource groups as
containers — and generating clean, deployable **Terraform** or **OpenTofu** projects from
the diagram.

The diagram is stored as a provider-neutral intermediate representation. Each abstract
resource type is mapped to concrete provider resources by **data files**
(`definitions/*.toml`), so adding cloud coverage means contributing a TOML file, not
writing Rust. "Export for all providers" produces one complete, independent project per
provider — never a single "portable" HCL file, because no such thing can exist.

![TerraTofu GUI with the three-tier example open](docs/screenshot.png)

Status: **Phase 2 catalog** — 31 curated abstract types mapped for AWS and Azure, plus
every native provider resource through the bundled schema index (see "Beyond the
curated catalog"). Curated: networking (Virtual Network, Subnet, Internet Gateway, NAT
Gateway, Route Table, Security Group, Private Endpoint, Network Peering), compute (Compute Instance,
Autoscaling Group, Load Balancer), data (Relational Database, NoSQL Table, Cache, Object
Storage), serverless (Function, Event Queue, Topic, the Azure-only Storage Queue and
Service Bus Namespace), containers (Container App,
Container Registry, Kubernetes Cluster), DNS (Zone, Record), secrets (Key Vault, Secret),
monitoring (Log Group, Alarm), IAM Role and the Resource Group container. Gaps a provider cannot
express (an Azure database's network access, for example) are reported as manual steps,
never papered over. Every example passes `tofu validate` for both providers and both
tools in CI.

## Build & run

Requires Rust 1.88 or newer.

```bash
cargo run -p ttg-app --release -- examples/three-tier.ttg.json
```

Headless export (also used in CI):

```bash
cargo run -p ttg-cli -- export-all examples/three-tier.ttg.json --out ./out --zip
```

`terraform` / `tofu` are optional: if one is installed, the app and `ttg export --validate`
run `init -backend=false && validate` on the output for you. The binary is looked up on
`PATH`, in `TTG_TOOL_DIR` if set, and in the usual per-user install folders
(`%LOCALAPPDATA%\Programs\OpenTofu`, `~/.local/bin`, Homebrew, Chocolatey, winget).
Provider plugins downloaded during `init` are cached once per user
(`TF_PLUGIN_CACHE_DIR`, defaulting to `%LOCALAPPDATA%\terratofu-gui\plugin-cache` or
`~/.cache/terratofu-gui/plugin-cache`), so repeated validation is fast.

Full validation of every example against every provider and both tools:

```bash
TTG_REQUIRE_VALIDATE=1 cargo test -p ttg-codegen --test validate_examples
```

The same test runs in CI ([.github/workflows/ci.yml](.github/workflows/ci.yml)) with
OpenTofu installed, so a definition change that produces invalid HCL fails the build.

## Using the app

- **Palette** (left): drag a resource onto the canvas, or click to add it at the centre.
- **Canvas**: scroll to pan, Ctrl+scroll to zoom, right-drag / middle-drag to pan, drag on
  empty space for marquee selection. Drag a node into a container to put it inside.
  Drag from the small circle on a node's right edge onto another node to connect them.
- **Inspector** (right): typed fields from the resource definition, provider-specific
  fields, links, and the diagnostics for that node.
- **Toolbar**: choose Terraform or OpenTofu, the target provider, and export.
  *Preview changes vs last export…* (also in the File menu and the Export window) runs
  the generator without writing anything and shows a per-file diff against the folder
  of the last export, changed files expanded and unchanged runs folded, so you can see
  what a re-export would touch. Headless: `ttg diff <project> --out <dir> [--full]`,
  which exits 1 when something would change.
- Badges on nodes: red `!` = error (export blocked), orange `!` = warning (manual step),
  `✕` = no mapping for this provider, blue `M` = flagged external/manual.
- Undo/redo (Ctrl+Z / Ctrl+Y), copy/paste (Ctrl+C / Ctrl+V), delete, select all, save
  (Ctrl+S), zoom to fit (Ctrl+0).
- **File ▸ Open recent** remembers the last ten projects. New, Open and Quit (and the
  window close button) ask before discarding unsaved changes.
- **View ▸ Edge style** switches between curved and orthogonal links. Links attach to
  whichever side faces the other node and fan out when several share a side. Select a
  link to pin either end to a chosen side and position (inspector ▸ Routing). With
  *Route around nodes* on (the default for orthogonal links), a link that would cut
  through another node takes a Z- or U-shaped detour just outside it instead; turn it
  off for the plain facing-sides router.
- A link to a container ends in a small terminal on the nearest wall, wherever the
  other end sits, so a node inside a subnet linking to its VNet stays a short line.
- A link that containment already implies (a subnet drawn inside its network) is not
  drawn and is reported as an info diagnostic; the canvas refuses to create new ones.
- Drag the bottom-right corner of a node or container to resize it; the inspector has
  exact width/height fields and a reset for nodes.
- **Edit ▸ Arrange**: *Tidy layout* (Ctrl+L, also on the canvas and container context
  menus) lays the diagram out in columns, sources on the left and what they depend on
  to the right, recursively inside every container, and fits containers to their
  contents. Align left/centre/right/top/middle/bottom needs two or more selected
  items; distribute horizontally/vertically needs three. All undoable. The same layout
  is available headless with `ttg tidy <project> [--out <file>]`.
- In the inspector, an abstract field that the selected provider's mapping does not use
  is greyed out; its tooltip says which providers do use it.
- **Views** (bar above the canvas) turn the wired diagram into architecture maps. The
  *All* tab is the truth that gets exported; each view is a tab stored in the project
  file with three things of its own:
  - a **filter**: which resources and link kinds are shown (category, link kind, focus
    on the selection within N hops, hide / show-only the selection, and *Show
    containers* to drop networks and resource groups from a map; the menu stays open
    until you click outside it). Containers of anything visible stay visible, and the
    filter saves into the active view as you change it;
  - its **own layout**: moving or resizing anything while the view is active changes
    only that view, so a "network" view can cluster subnets one way and a "data" view
    can line the pipeline up another. Right-click the tab and untick *Own layout* to
    share the All layout instead;
  - **annotations**: `+ Group` adds a draw.io-style grouping box (label, colour; drag
    its title and everything inside follows). Right-click a resource or group and choose
    *Data flow from here*, then click the target, for a labelled arrow. Groups and flows
    are pure documentation: they are never exported and never become dependencies.
  Hidden resources are still exported; the corner label says how many are hidden.
  The job-pipeline example ships a "Data flow" view built this way:

![The job pipeline's "Data flow" view: own layout, grouping boxes and labelled data-flow arrows, containers hidden](docs/screenshot-dataflow.png)
- **Provider layers.** One diagram serves both providers. Anything not tagged is part of
  every provider's export; tick the *Providers* checkboxes on a resource or link to make
  it AWS-only or Azure-only, and provider-only types (Storage Queue, Service Bus
  Namespace) tag themselves. In concrete mode, things that are not part of the shown
  provider's design are dimmed; every provider-specific item carries an "Azure only"
  style tag in both modes. The project inspector lists what each provider's export
  leaves out, and the diagnostics panel reports the same per item, so the two designs
  can differ only where you can see it. A provider-only *container* (a Service Bus
  Namespace on AWS) is just grouping there: its contents are exported as if they sat in
  the enclosing container.
- **Show as: Abstract / \<provider\>** (toolbar, or `P`). In concrete mode every node is
  labelled with the resource it generates for the target provider (`aws_sqs_queue +1`
  means one helper resource; hover for the full list), logical mappings are greyed, the
  palette shows concrete names, and the inspector only offers that provider's fields.
  Drop PNGs into `definitions/icons/<provider>/<type_id>.png` to replace the glyphs with
  an icon pack of your choice (none is shipped: the official icon sets carry their own
  licences).
- **Reachability overlay** (toolbar button or `R`). With a resource that initiates
  traffic selected (function, instance, cluster), the canvas shows what it can reach:
  its way out of the network drawn once through subnet, route table, NAT and internet
  gateway, in-network paths drawn through the security group that admits them, arrows in
  the direction of traffic, targets tinted green (reachable), red (blocked, hover for
  why) or amber (undecidable). With a passive resource selected (database, queue, cache)
  it shows who can reach *it*. Every networked resource carries a tag with the port it
  listens on. With nothing selected it shows the network posture: an orange ring and
  inbound arrow on anything exposed to the internet, a dot for resources with or without
  an outbound path. The inspector lists "Can reach" / "Reached by" with a *path* button
  that filters the canvas to just that path. Paths follow the network fabric: across a
  Network Peering (on AWS only once both sides' route tables are linked to it), over a
  Private Endpoint in the source's network (no NAT needed), and through a Load Balancer
  when the target only admits the balancer's group. The same analysis is available
  headless with `ttg reach <project> --from <name>`; `examples/hub-spoke.ttg.json` shows
  all three.

![Reachability overlay from the job runner](docs/screenshot-reachability.png)

![Concrete display of the fan-out example with the "Messaging" view active](docs/screenshot-concrete.png)

![The job pipeline after Edit ▸ Arrange ▸ Tidy layout](docs/screenshot-tidy.png)

![Provider layers: the fan-out example shown as AWS, with the Azure-only Service Bus Namespace dimmed and its contents exported as plain SQS/SNS resources](docs/screenshot-layers.png)

## Beyond the curated catalog: every provider resource and argument

The curated types are the *portable* part of the catalog. For full provider scope the app
bundles a compact index of the real provider schemas (every resource, argument and nested
block of the AWS and Azure providers the catalog pins, about half a megabyte compressed)
and uses it in three ways:

- **Advanced arguments on curated resources.** Every provider mapping section in the
  inspector has an *Advanced arguments* editor: search any argument the provider
  accepts, add it, and it is merged into the generated block (an argument the mapping
  already sets is overridden by yours, with a note). Typed editors for strings, numbers
  and booleans; JSON for lists, maps and nested blocks; a *ref* button to point a string
  at another resource's attribute. Unknown names, read-only attributes and type
  mismatches are errors before validate ever runs.
- **Native provider resources.** Type two or more letters in the palette search and a
  *native* section lists matching resources from the schema: all 1,500+ AWS and 1,100+
  Azure types. Drop one on the canvas and its inspector is generated from the schema,
  with required arguments flagged. Native resources are provider-only by nature, so they
  tag themselves to that provider's layer and the other provider's export leaves them
  out. Link them to other resources with *Depends on* and reference attributes with the
  ref button (`{"$ref": {"entity": "assets", "attr": "id"}}` in the file), or use
  `{"$raw": "<hcl>"}` for an expression.
- **A test that keeps curated definitions honest.** Every resource type and argument a
  curated mapping writes is checked against the schema in CI, so a provider rename fails
  a test instead of a user's export.

`ttg schema info` shows the index in use, `ttg schema search aws "sqs queue"` and
`ttg schema show azure azurerm_storage_queue` explore it, and `ttg schema refresh`
regenerates it from the installed tool into your data directory, where it takes
precedence over the bundled copy (`--out crates/ttg-schema/data/index.json.gz` refreshes
the bundled one). See `examples/native-extras.ttg.json`.

![A native resource with its schema-driven inspector](docs/screenshot-native.png)

## Driving the app from an agent (MCP)

The app can host a [Model Context Protocol](https://modelcontextprotocol.io) server so
Claude Code (or any MCP client) can read and edit the diagram you have open, and you
watch it happen. It is off until you switch it on:

1. **Agent ▸ MCP server** (or *Agent ▸ Settings & activity…*). Nothing runs while it is
   off; switching it on starts a small HTTP server on `127.0.0.1:9337` (port and token
   are in the settings window, "start with the app" is a checkbox, off by default).
2. Copy the command from the settings window and run it once:

   ```bash
   claude mcp add --transport http terratofu http://127.0.0.1:9337/mcp --header "Authorization: Bearer <token>"
   ```

3. Ask Claude Code to look at or change the diagram. Every change is one undo step,
   flashes orange on the canvas, and is logged in the settings window; the status bar
   shows what the agent did. Saving to disk only happens when a tool is explicitly asked
   to, and opening another file goes through the same unsaved-changes prompt as the menu.

The 34 tools cover reading (project, catalog, diagnostics, reachability, export preview,
export diff, screenshot) and editing (add/update/move/resize/reparent/delete entities,
links, selection, views, tidy/align/distribute, settings, save/open/new, export with
optional validate, undo/redo). `project_apply` runs a list of diagram writes as **one**
undo step and rolls all of them back if any fails; `project_changes` (or a subscription
to the `ttg://project` resource) tells the agent when *you* changed something. The
project, its summary, diagnostics, the catalog and the docs are also exposed as MCP
resources (`ttg://project`, `ttg://catalog/<type>`, `ttg://docs/mapping-format`, …).
In *Agent ▸ Settings & activity* you choose which actions must be approved first: by
default saving, opening, starting a new project and writing an export pop an
Allow / Deny prompt; deletions can be added. Only localhost can connect and every
request needs the bearer token. `terratofu-gui --serve --port N --token T [project]`
runs the same server without a window (no screenshots, no prompts) for CI and scripted
editing; the headless integration test in `crates/ttg-app/tests` drives it that way. The feature is the `mcp` cargo feature of `ttg-app` (on by default; build with
`--no-default-features` to leave it out). Windows note: ports in the 6xxx-7xxx block are
often reserved by Hyper-V, which is why the default is 9337.

![An agent session: a queue added, linked and the diagram tidied over MCP, with the orange flash on the touched entities and the activity in the status bar](docs/screenshot-mcp.png)

## Repository layout

```
definitions/          resource + provider mapping files (data, contributable)
crates/ttg-core       IR, project file, structural validation, dependency graph
crates/ttg-catalog    loads and validates definitions
crates/ttg-codegen    HCL generation, diagnostics, MANUAL_STEPS.md, tool toggle
crates/ttg-cli        headless `ttg` command
crates/ttg-app        egui/eframe desktop application
examples/             sample projects
docs/                 ARCHITECTURE.md, MAPPING_FORMAT.md, PHASE2_PLAN.md
```

Read [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and
[docs/MAPPING_FORMAT.md](docs/MAPPING_FORMAT.md) to contribute a resource definition.

## Licence

Dual-licensed under MIT or Apache-2.0, at your option. Mapping definitions are written
from publicly documented provider schemas (the OpenTofu registry is the canonical
reference); no HashiCorp source code is used.
