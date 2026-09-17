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

Read: `project_get`, `project_summary`, `catalog_types`, `catalog_type`, `diagnostics`,
`reach_posture`, `reach_from`, `reach_to`, `export_preview`, `view_get`, `view_export`,
`view_fit`, `screenshot`.

`diagnostics` answers `{ provider, diagnostics, other_providers }`: the target provider's
list, and separately the errors the *other* providers would raise, as warnings carrying
their `provider` and a `[<Provider>] … (would block the <Provider> export)` message. The
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
headless refusal for a sized `screenshot`, and `view_delete` with its undo. The unit
tests beside `mcp/mod.rs` cover the dropped-caller rule (fresh and deferred), the
heartbeat, and the screenshot size clamp. All run in CI on the same job as the rest of
the workspace.

## 5. Open ideas

- Prompts (`prompts/list`): canned "review this diagram" / "make it Azure-ready" starters.
- Progress notifications for long `export_run --validate` calls.
- A `tasks`-style handle for validate so the tool returns immediately.
