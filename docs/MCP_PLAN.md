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
`reach_posture`, `reach_from`, `reach_to`, `export_preview`, `screenshot`.

Write (each undoable): `entity_add`, `entity_update` (values validated against the
definition, unknown fields rejected with the list of valid ones), `entity_move`,
`entity_resize`, `entity_set_parent` (allowed-parent check, moves the entity inside the
container visually), `entity_delete`, `link_add` (relation must be allowed by the
definition; redundant containment links refused with the same message as the UI),
`link_remove`, `selection_set`, `view_set`, `view_save`, `view_activate`,
`view_group_add`, `view_flow_add`, `view_annotation_remove` (architecture-map
annotations in the active view; `entity_move` lands in the active view's own layout
when it has one), `schema_search`, `schema_show` (the provider schema index), `entity_update.extra`
(extra / native arguments), `layout_tidy`, `layout_align`,
`layout_distribute`, `settings_set`, `project_save`, `project_open`, `project_new`,
`export_run` (validate runs off the UI thread), `undo`, `redo`.

Entities can be addressed by id or by display name (case-insensitive; ambiguous names
are rejected). Writes are refused while a modal dialog is open, naming the dialog.

## 3. Settings and persistence

`autostart`, `port` (default 9337) and `token` (uuid, regenerable) persist in eframe
storage. `TTG_MCP=1` (+ `TTG_MCP_PORT`, `TTG_MCP_TOKEN`) force the server on for one run
without touching the stored autostart flag; `TTG_MCP_AUTOSTART=off` clears a stored
autostart. Used by the smoke test.

## 4. Testing

A python Streamable-HTTP client (`initialize` → `notifications/initialized` →
`tools/list` → `tools/call`) exercised every tool group against the running app:
add/update/link/tidy/screenshot/undo, plus the negative cases (bad field, disallowed
relation, missing token → 401). Not yet in CI because it needs a display; a headless
harness (egui `--screenshot`-style frame loop with the server on) is the next step.

## 5. Open ideas

- Resources (`ttg://project`, `ttg://catalog/<type>`, docs) alongside tools.
- A batch tool (`project_apply` with a list of commands) as one undo step.
- Notifications to the client when the user edits (MCP `resources/updated`).
- Per-tool confirmation setting ("ask before the agent deletes / saves").
