# MCP server — design and status

Status: **implemented 2026-09-08** in `crates/ttg-app/src/mcp/` behind the `mcp` cargo
feature (on by default). This page records the design decisions; the user-facing steps
are in the README ("Driving the app from an agent").

## 1. Shape

The app itself hosts the server; there is no separate process:

```
Claude Code ──Streamable HTTP (MCP), localhost, bearer token──► terratofu-gui (rmcp + axum)
```

- **Off until toggled.** Agent ▸ MCP server starts a tokio runtime on its own thread
  (`mcp::server::run`), binds `127.0.0.1:<port>`, and serves rmcp's
  `StreamableHttpService` under `/mcp` behind an axum middleware that checks
  `Authorization: Bearer <token>`. Toggling off (or closing the app) cancels the rmcp
  cancellation token, which ends every session and lets `axum::serve` and the runtime
  exit. "Start with the app" is a setting, default off.
- **Why in-process rather than a `ttg-mcp` stdio proxy.** Streamable HTTP is exactly the
  transport for "connect to something already running"; a stdio server would have needed
  a discovery file and a second binary to keep in sync. `rmcp` 3.2 has MSRV 1.88, the
  same as this workspace, so no toolchain fight.
- **UI-thread execution.** Tools never touch the project from the server thread. Each
  call becomes an [`AgentCommand`] queued on an `mpsc` channel with a `oneshot` reply;
  the server calls `egui::Context::request_repaint()`; `TtgApp::drain_agent_commands`
  runs at the top of the next frame and executes it through the same `snapshot()` /
  `finish()` path as a mouse edit. So every write is one undo step, marks the project
  dirty, refreshes diagnostics, and is drawn immediately. Screenshots go through
  `ViewportCommand::Screenshot` and are answered when the `Event::Screenshot` arrives.
- **Feedback.** Status bar "Agent: <command>", an orange fading ring on touched
  entities for 1.5 s, an activity log (last 200 calls) in Agent ▸ Settings & activity,
  and a green "● MCP :port" indicator in the menu bar while the server is up.

## 2. Tools

Read: `project_get`, `project_summary`, `catalog_types`, `catalog_type`,
`catalog_relations`, `diagnostics`, `reach_posture`, `reach_from`, `reach_to`,
`export_preview`, `entity_preview`, `view_get`, `view_export`, `view_fit`, `screenshot`,
`cost_estimate`.

`cost_estimate { provider?, environment?, view?, group_by?, assumptions?, region? }`
(round 3, R3.12; `exec/cost.rs` over `ttg_codegen::cost::estimate_with`) answers the
monthly estimate of the provider's layer: `monthly` (and `converted` when the project
sets a display currency), `prices_retrieved`, `region` / `price_region`, the `caveat`
to repeat, and by `group_by` either `lines` (priced entities, largest first, each with
its charges — quantity, unit, unit price, the price-list row — assumptions with their
source, and notes), `types` or `groups` (the labelled boxes of `view`, or of every
view). Free entities come back summarised per type with the reason, unpriced ones in
`not_estimated` with why; `views` has every saved view's total and `assumptions` every
assumption at its project value. `view` narrows the lines and the total to what that view
shows. `assumptions` are for the call only (`source: "call"`); unknown keys, a non-number,
an unknown view, `group_by` or provider are refused with what would have worked. Named
environments do not exist yet: `environment` is accepted and the reply's `notes` say it
was ignored; the estimate takes a `&Project`, so resolving an environment first is all
that will be needed. The budget warning (`code: "Cost"`) arrives through `diagnostics`.

`diagnostics` answers `{ provider, diagnostics, other_providers }`: the target provider's
list, and separately what the *other* providers would say: their errors as warnings
carrying their `provider` and a `[<Provider>] … (would block the <Provider> export)`
message, and (round 3) an info line per entity one of them leaves out of its export. The
two lists are kept apart so an agent cannot mistake "blocks the export I am making" for
"would block an export nobody asked for". `ttg://diagnostics` returns the same object.

Write (each undoable): `entity_add`, `entity_update` (values validated against the
definition, unknown fields rejected with the list of valid ones), `entity_move`,
`entity_resize`, `entity_set_parent` (allowed-parent check, moves the entity inside the
container visually), `entity_delete`, `link_add` (relation must be allowed by the
definition; redundant containment links refused with the same message as the UI),
`link_remove`, `selection_set`, `view_set`, `view_save`, `view_delete`, `view_activate`,
`view_group_add`, `view_flow_add`, `view_annotation_remove` (architecture-map
annotations in the active view; `entity_move` lands in the active view's own layout
when it has one), `schema_search`, `schema_show` (the provider schema index; `depth` and
`required_only` narrow a large resource - the full `aws_wafv2_web_acl` schema is ~900 KB -
via `BlockSchema::filtered` in `ttg-schema`, also behind `ttg schema show --depth N
--required-only`), `entity_update.extra` (extra / native arguments), `layout_tidy`, `layout_align`,
`layout_distribute`, `settings_set`, `project_save`, `project_open`, `project_new`,
`export_run` (validate runs off the UI thread), `undo`, `redo`.

Added 2026-09-29 (round 3, Kubernetes manifests): `export_run`, `export_preview` and
`export_diff` take `k8s` (bool) to include (`true`) or leave out (`false`) the `k8s/`
directory for that one call, whatever `settings.kubernetes_manifests` says — the project
is not changed, so nothing needs undoing. `settings_set { kubernetes_manifests }` changes
the setting itself (one undo step) and echoes it back. `catalog_type` marks the fields and
relations only the manifests read (`manifests: true` / `manifests_only: true`), so an agent
can tell that a workload's `image_tag` or its 'Schedules on' link change no Terraform.

Added 2026-09-14 (views as documents): `view_note_add` and `view_logical_add` (note
boxes and annotation-only nodes; both removable through `view_annotation_remove`, which
now matches a group, flow, note or logical node by id, label, title or name),
`view_update` (name, description, legend), `view_get` (one view in full: filter,
description, layout, which resources it shows, and its groups — with the members they
currently hold — flows, notes and logicals), `view_fit` (frames the view's content in the
camera and answers with the bounding box, so it works headless) and `view_export`
(`md` / `mermaid` through `ttg_codegen::views`). `view_group_add`, `view_flow_add`,
`view_note_add`, `view_logical_add`, `view_annotation_remove` and `entity_move` take an
optional `view`: `TtgApp::with_view` makes that view active for the command and puts the
previous one back, so a batch can draw a whole map without interleaving `view_activate`.
`view_flow_add` also takes `step` and `color`. `screenshot` takes `view`, `fit` and
`hide_panels`; the options are applied when the command is dequeued and the capture is
asked for two frames later, so the view has settled, with the panels restored when the
image arrives (the asynchronous contract is unchanged).

Added 2026-09-17 (round-2 gaps): `view_delete` (removes a saved view and everything it
holds as one undo step; the canvas falls back to All when it was active).
`view_save` now takes `replace`: a name already in use (compared case-insensitively,
like `resolve_view`) is refused, and with `replace: true` only that view's filter is
rewritten — its layout, groups, flows, notes and logical nodes stay. `view_update` takes
a `filter` of its own, resolved exactly as `view_set` resolves names in
`focus` / `hidden` / `only`, so a saved filter is no longer frozen; the canvas follows
when that view is active. `view_set` while a saved view is active now writes the filter
into that view — what `TtgApp::set_filter` has always done for the filter menu in the
app — instead of silently detaching the canvas; the reply names the view it changed, and
`view_activate All` first is the way to filter without touching one. `entity_move` and
`entity_resize` also take a view's annotations (note title, logical node name, grouping
box label or any of their ids) and answer with `kind: "entity" | "note" | "logical" |
"group"`; a moved box does not drag its members, because membership is geometric. An
anchored `view_note_add` with no `x`/`y` lands beside its anchor — the first of right,
below, left, above that is clear of everything the view draws — rather than off the
right-hand edge of the whole diagram. `screenshot` takes `width`/`height`: egui cannot
render off-screen here, so the window is resized with `ViewportCommand::InnerSize`, the
fit happens at the new size, the capture follows and the old size is put back; the reply
reports both the size asked for and the size captured, since the OS may clamp to the
display. `TTG_WINDOW_SIZE=WxH` does the same for the `--screenshot` CLI path.
`ViewFilter::hide_edges` accepts `hide_links` as a serde alias and is named in the
`view_set` / `view_update` filter descriptions; it round-trips through `view_save`,
`view_update { filter }` and `view_get` like every other key.

Not applying a command twice (2026-09-17): a tool call that timed out used to leave its
command in the channel, so it ran later, beside whatever the agent retried. Three things
stop that. `TtgApp::run_validate` runs `<tool> init && validate` on a background thread
and `poll_validate` collects the outcomes each frame, so the export panel no longer
freezes the UI (and the command queue) for minutes. `drain_agent_commands`, the deferred
queue and the pending approval all skip a command whose `AgentReply` sender
`is_closed()` — the caller's oneshot receiver was dropped — logging "dropped: caller
gone". And `mcp::Heartbeat` (an `Arc<Mutex<(Instant, Option<String>)>>` beaten once per
drain) lets `TtgServer::exec` refuse before queuing anything: if the UI has not drained
for three seconds it is woken and given 600 ms to prove it is merely idle, and otherwise
the call comes back "the app is busy: <what>; nothing was queued, retry in a moment".
`TtgApp::busy_while` publishes what the app is stuck on around the modal file dialogs
and the exports. rmcp's `sse_keep_alive` default (15 s) is left as it is.

Added 2026-09-29 (round 3, "MCP friction", 47 → 49 tools; the code is in
`mcp/exec/round3.rs`, a child module of `exec.rs`, so it can use that file's private
helpers and stays out of the way of the big `agent_exec` match):

- **R3.15, one blocked-project message.** `export_preview`, `export_diff`, `export_run`
  and the new `entity_preview` all render a `GenError` through `gen_error_text`, and
  `server::fail` refuses to send an empty error text (it says the app reported an error
  without a message). A blocked project gives the same "project has errors that block
  export:" list from all four; a headless test pins that. The empty `export_diff` error
  in the report could not be reproduced (the two tools shared the same path, and both
  answered with the list on this build), so the change is to make that impossible rather
  than to fix a known bug.
- **R3.16, notes report where they are drawn.** `view_get` gives a note's `position` as
  the absolute top-left it is drawn at (`ttg_core::view::note_rect`, so it is what
  `entity_move` takes) and `offset`, the offset from the anchor that the file stores
  (`null` for a free note, or an anchor that cannot be found). `entity_move` on a note
  also answers with `offset`. `project_get` is the raw file shape and still holds the
  stored offset in `position`; its description says so. `view_export` prints no
  positions.
- **R3.17, hidden flow ends.** `view_flow_add` still accepts a flow to an entity the
  view hides (annotations are never exported and the view may be about to show it), but
  answers with `hidden_ends` and a `warning` naming the entity, the part of the filter
  that hides it (`ttg_codegen::views::hidden_because`, one reason per clause of
  `visible_set`) and the fix. `show_hidden: true` applies `ttg_codegen::views::reveal`:
  the entity leaves `hidden` and joins `only` when `only` is non-empty, nothing else
  changes, the filter change and the flow are one undo step, and the reply lists
  `shown`. `categories`, `types`, `origin`, `providers`, `name_glob`, `focus` and
  `containers: false` are what a view is *for*, so they are never rewritten; the flow is
  added anyway and the warning names them.
- **R3.19, omissions elsewhere are visible.** `diagnostics::other_providers` adds, per
  other provider, an **info** line for each entity an `omit` check leaves out of that
  provider's export (`[Microsoft Azure] <check message>; left out of the Microsoft Azure
  export`, `provider` set, `code` Check). The target provider is skipped there, so its
  own warning from `run` still appears exactly once. `ttg check` counts the two kinds
  separately.
- **R3.20, identity.** `serverInfo` is `terratofu-gui`, version
  `<ttg-app version>+catalog.<hash>` (semver build metadata), title `TerraTofu GUI <v>`,
  and the instructions open with "TerraTofu GUI <v>, catalog <hash>: if your tool list
  lacks view_delete or entity_preview, refresh it". The hash is
  `ttg_catalog::Catalog::fingerprint`: FNV-1a over the definition texts (sorted, so
  the same files hash the same from the embedded list or a directory), twelve hex
  digits, computed in `Catalog::from_sources`; native types registered later do not
  move it. `McpState::catalog_hash` carries it from `TtgApp::build` to `server::run`.
- **R3.21, calls that stop being all-or-nothing.**
  `entity_preview { entity, provider? }` slices one entity's share out of a full
  generation (`Generated::entity_preview`, from the `entity_blocks` the emitter now
  records: file, resource addresses, the blocks rendered as the export renders them) and
  adds its manual steps and diagnostics; an entity with no blocks says why in
  `no_blocks`. It needs an export-clean project, like `export_preview`.
  `diagnostics { entity?, severity?, provider? }` filters both lists and reports
  `matched` / `of`; `provider` answers that provider's run (and what the others would
  say relative to it) without touching the target. `project_get { fields?, entities? }`
  returns top-level keys and/or just those entities with the links among them (with
  `entities` alone: `containers`, `nodes`, `edges`); no arguments is unchanged.
  `catalog_relations { source_type?, target_type? }` lists the definitions'
  `[[relations]]` with label, cardinality, `min_targets`, `via_parent` and provider
  scope (`catalog_type` now reports the provider scope too).
  `project_apply { dry_run: true }` (`AgentCommand::DryRun`) runs the batch against an
  empty history of its own, computes the diagnostics before and after (`added` /
  `removed` as a multiset, errors and warnings before and after), then puts everything
  back: the project, the history *and* its redo stack, the dirty flag, selection, active
  view, filter and status line. `McpState::quiet` silences `note_change` for the
  duration, so the revision counter and subscribers never see a change that did not
  land. A failing command fails with the same message as a real batch.
  `entity_update { select }` and `link_add { select }` (`BulkUpdate` / `BulkLink`;
  `select` is `{ types?, name_glob?, ids? }`, every given criterion must match, ids take
  names) run the ordinary single-entity command on each match against a private history
  and push one undo step at the end. They are all-or-nothing and report *every* refusal
  at once; an empty selector and one that matches nothing are refused with a message
  saying so; a bulk update cannot rename. A bulk link skips (and names) the target
  itself and entities whose container already implies the link, and lists `linked`,
  `already_linked` and `skipped`. Both work inside `project_apply`, including
  `dry_run`.

`settings_set` (round 3, 2026-09-29): besides `tool`, `provider`, `provider_settings`,
`tags` and `kubernetes_manifests` it now takes `backend` (`{"type": "s3", "bucket": …, "region": …}` flat, or the
`{type, args}` shape `project_get` returns, so a read can be written back; `null` clears
it), `state_encryption` (bool), `state_encryption_key` (an Encryption Key's id or name;
`null` clears it) and `provider_versions` (`{"aws": "~> 6.0"}`, merged into the pins; `null`
or `""` removes one). Everything is checked before anything changes, with the same rules as
the settings panel and the export gate (`ttg_codegen::state::check_backend`,
`ttg_codegen::versions::check_constraint`): an unknown backend type, a missing required
key, a key that is not an Encryption Key or a malformed constraint is refused with what
would be right. A key the tool does not know — `backnd`, or the `state_encrypt` of an
older client — is refused too, listing the valid keys, instead of being dropped with
"settings updated": `SettingsArgs` collects unknown keys through a flattened map that
`SettingsArgs::into_command` rejects, for direct calls and inside `project_apply` alike.
The reply echoes the state and version settings.

Added 2026-09-09: `project_apply` (a list of `{tool, args}` diagram writes executed as
one undo step; `AgentCommand::Batch` snapshots first, runs each sub-command through the
normal path, then truncates the history back and pushes one step; any failure restores
the snapshot and reports which command failed), `export_diff` (`ttg_codegen::diff`
against an export directory, nothing written) and `project_changes` (a revision counter
bumped in `finish()` for user and agent edits alike, with who changed it last).

### Resources and notifications

`resources/list` exposes `ttg://project`, `ttg://project/summary`, `ttg://diagnostics`,
`ttg://catalog` and the docs (`ttg://docs/readme`, `mapping-format`, `architecture`,
embedded at build time); templates `ttg://catalog/{type_id}` and `ttg://reach/{entity}`.
Reads go through the same command queue as tools. `resources/subscribe` is accepted for
the live resources; every committed change sends `notifications/resources/updated` to
each subscriber, coalesced over 250 ms so a drag is one notification. Subscriptions are
kept per session in a shared list; a peer that fails to receive is dropped.

Entities can be addressed by id or by display name (case-insensitive; ambiguous names
are rejected). This applies everywhere an entity is named, including `entity_ref`
struct-list items (e.g. a security-group rule's `source_group` in `entity_update.config`):
the name is resolved to the id before the value is stored, so diagnostics never see a
stray display name. Writes are refused while a modal dialog is open, naming the dialog.

`entity_add` and `catalog_type` accept native type ids (`native:<provider>:<resource>`,
as returned by `schema_search`) directly: the synthetic definition is registered on first
use (`Catalog::ensure_native`), same as dropping one from the palette.

An `entity_update.extra` value's `{"$ref": {"entity": ..., "attr": ...}}` targets the
referenced entity's primary block; add `"block": "<key>"` to address one of its secondary
blocks instead (e.g. Object Storage's `versioning` block on AWS).

## 3. Settings and persistence

`autostart`, `port` (default 9337), `token` (uuid, regenerable), `confirm_disk`
(default on: save / open / new / export wait for an Allow / Deny prompt) and
`confirm_delete` (default off: entity and link removal; annotation removal is never
exported, so it is never confirmed) persist in eframe storage. A command that needs
approval parks in `McpState::pending_confirm`; the prompt is a centred window, and the
tool call's timeout is ten minutes for writes so the user has time to answer. A pending
prompt no longer stalls the whole queue: reads (`!AgentCommand::is_write`) keep answering
while it is open, and further writes queue up in `McpState::deferred` in arrival order
rather than jumping ahead of it. Resolving the prompt (Allow or Deny, via
`TtgApp::resolve_confirm`, used by both the confirm window and the headless unit test)
starts the next deferred write, which may itself need approval and become the new
`pending_confirm`, still ahead of the rest of the queue. `TTG_MCP=1` (+ `TTG_MCP_PORT`,
`TTG_MCP_TOKEN`) force the server on for one run without touching the stored autostart
flag; `TTG_MCP_AUTOSTART=off` clears a stored autostart. Used by the smoke test.

## 4. Testing

`terratofu-gui --serve [--port N] [--token T] [project]` runs the server without a
window: `TtgApp::build` around a bare `egui::Context`, then a loop of
`drain_agent_commands` + `refresh_diagnostics`. Screenshots are refused and approval
prompts are skipped in this mode. `crates/ttg-app/tests/mcp_headless.rs` spawns it on a
free port and speaks Streamable HTTP with a ~60-line client (`ureq`, SSE `data:` lines):
initialize, tools/list, a write and the revision counter, a four-command batch undone
by one `undo`, a failing batch rolled back, `export_diff` against an empty directory,
resources list/read/templates/subscribe, a 401 and the headless screenshot refusal. A
second test drives the job-pipeline example: `view_get`, drawing notes, logical nodes
and flows into a named view while no view is active, `entity_move` landing in that
view's layout and not the shared one, `view_update`, both `view_export` formats,
`view_fit` answering without a window, removal by title, and a three-command batch undone
in one step. A third, `headless_view_editing`, covers the round-2 gaps: the duplicate and
`replace` paths of `view_save`, `hide_links` arriving as `hide_edges` and surviving a
round trip, `view_update { filter }`, `view_set` writing into the active view, two boxes
drawn nested in one `project_apply` and reported as such by `view_get` and the Markdown
"Inside" column, moving and resizing a note, a logical node and a box (and a box not
dragging its members), an anchored note landing beside its anchor, the documented
headless refusal for a sized `screenshot`, and `view_delete` with its undo. Round 3 adds
one headless test per item: `serverInfo` and the instructions, the four export tools
failing identically on a blocked project, absolute position and stored offset of an
anchored note (the report's (−1850, −1850) case), hidden flow ends with `show_hidden`
(each of `hidden`, `only` and a `categories` filter, and one undo for the flow plus the
filter), omitted entities and the `diagnostics` filters, `project_get` slices with
`catalog_relations` and `entity_preview`, `dry_run` (the delta, nothing left in the
project, history, redo stack or revision) and the bulk writes (one undo step, one
revision, every refusal listed, empty and non-matching selections). Codegen tests cover
the other-provider info lines, `Generated::entity_preview` and `views::reveal`; the
catalog crate tests the fingerprint. The unit tests beside `mcp/mod.rs` cover the
dropped-caller rule (fresh and deferred), the heartbeat, the screenshot size clamp,
selectors and the quiet flag. `headless_cost_estimate` covers the three groupings on
job-pipeline (whose one view has four groups), a one-call assumption moving the total
and reported as `call`, the environment note and an explicit region, each refusal, and
that the revision does not move. All run in CI on the same job as the rest of
the workspace.

## 5. Open ideas

- Prompts (`prompts/list`): canned "review this diagram" / "make it Azure-ready" starters.
- Progress notifications for long `export_run --validate` calls.
- A `tasks`-style handle for validate so the tool returns immediately.
