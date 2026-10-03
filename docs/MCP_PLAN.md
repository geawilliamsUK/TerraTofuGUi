# MCP server — design and status

Status: **implemented 2026-09-08** in `crates/ttg-app/src/mcp/` behind the `mcp` cargo
feature (on by default). This page records the design decisions; the user-facing steps
are in the README ("Driving the app from an agent").

## 1. Shape

The app itself hosts the server; there is no separate process:

```
Claude Code ──Streamable HTTP (MCP), localhost, bearer token──► terratofu-gui (rmcp + axum)
claude.ai connector ──HTTPS tunnel, OAuth access token──────────┘   (optional, §5)
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

Added 2026-09-29 (round 3, views and metadata, 49 → 51 tools): `view_generate { view?,
kind, replace? }` — `data_flow` draws a view's flows from the links between the
resources it shows (`ttg_codegen::dataflow`, whose relation → direction table is in
ARCHITECTURE.md §4.1), skipping any pair that already has a flow, with `replace` first
removing the resource-to-resource flows; the reply lists what was added, and a
generation that changes nothing records no undo step. `personal_data` builds or
refreshes "Where personal data goes" and switches to it. `view_arrange_notes { view? }`
puts anchored notes back beside their anchors (`ttg_core::view::arrange_notes`, the same
placement `view_note_add` uses). `layout_tidy` takes `view` and `by: links | flows`:
`flows` is `ttg_core::flow_layout`, written into the view's own layout only, and either
way a tidied view gets its notes rearranged in the same undo step. `view_fit` reports
`legend_reserved_px`: with the legend on, the fit leaves its strip on the right free, so
a fitted screenshot never has content under it. `view_export` takes `format: sequence`
(a Mermaid `sequenceDiagram` of the numbered flows). `entity_update` takes
`classification` (`"none"` or `""` clears it), `description` and `owner`, validated
before anything is written and committed in the update's own undo step, and the bulk
form `{ select }` carries them too (`EntityChanges::meta`); `entity_json`,
`project_summary` (per entity, when set, plus a `classified` count) and `project_get`
carry them. `view_flow_add` takes `data` beside `show_hidden`, and `view_get` reports
it. New anchored notes are placed by `ttg_core::view::note_offset_beside`; `view_get`
still reports where a note is drawn and the offset it stores (`note_geometry`).

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

Added 2026-10-02 (round 4, remote and file-based access, two tools):
`approval_status { ticket? }` reports an approval ticket (§3) straight from the shared
`approvals::Approvals` table on the server thread, so polling never queues behind the UI
(without a ticket it lists every ticket still known). `project_import { json, replace? }`
(`exec/import.rs`) replaces the open project with one given as `.ttg.json` JSON, an object
or a string: `ttg_core::schema_check` validates it against the project schema first and
every problem comes back with its line, then `project::load_str` runs its structural
checks, and only then does anything change. A project with entities is not replaced
without `replace: true`; the replacement is one undo step, leaves the project dirty and
without a path (so a bare `project_save` cannot overwrite the file the old project came
from), and the reply carries the counts, the new diagnostics and any unknown fields that
were ignored. When the open project has unsaved changes the import needs approval, like
`project_open`, regardless of `confirm_disk`; an import that would not load is refused
before the prompt is shown. The format is in docs/FILE_FORMAT.md.

Defaults and replies (2026-10-03): `settings_set { name }` renames the project (it heads
the export's README and is the default state key prefix), and `project_save` to a named
file names a project still called `untitled` after the file (`TtgApp::save_to`, so the
GUI's Save As does the same). `entity_add`, `entity_update` and `link_add` take
`verbose: false` and then answer with only `{status, id, changed}` — `changed` compares
the project before and after, a link's id is `<source> -<relation>-> <target>` — instead
of the whole entity; `settings_set { verbose: false }` makes that the session default
(`McpState::terse_replies`, not saved) and a call's own `verbose` wins. The cut is made in
`exec_logged`, so it applies to direct calls; commands inside `project_apply` and the bulk
forms keep their own summaries. `entity_add` of a subnet picks the first free block of
its network (`ttg_core::cidr::next_free`), and `entity_update.extra` stores a value in
the form the schema declares (`ttg_codegen::diagnostics::canonical_extra`: `30` for a
string argument becomes `"30"`, `"30"` for a number becomes `30`); field values are
converted to their field's type the same way (`"20"` for an `int` field is stored as 20).
`catalog_type` lists a field's `units` (the unit an alarm threshold is in, per metric).

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
(default on: save / open / new / export wait for an Allow / Deny prompt),
`confirm_delete` (default off: entity and link removal; annotation removal is never
exported, so it is never confirmed), `approval_ttl_secs` (default 600), and the remote
settings of §5 (`bind`, `public_url`, `oauth`, and the OAuth clients and grants as
hashes) persist in eframe storage. `TTG_MCP=1` (+ `TTG_MCP_PORT`, `TTG_MCP_TOKEN`) force
the server on for one run without touching the stored autostart flag;
`TTG_MCP_AUTOSTART=off` clears a stored autostart. Used by the smoke test.

**Approval tickets (round 4, TF-020).** A write that needs approval used to hold its
call open for up to ten minutes while MCP clients gave up after about one, so the caller
saw "Request timed out" and could not tell whether the save or export would still
happen (one `project_save` was written after its caller's timeout). The contract now:

- `TtgApp::approval_reason` decides (the settings, plus an import over unsaved work;
  never in `--serve`). The command is claimed (see below), a ticket is opened in
  `approvals::Approvals` (an `Arc<Mutex<…>>` shared with the server thread), and the
  caller is answered *at once* with `{status: "pending_approval", ticket, tool, what,
  applied: false, prompts_ahead, waiting_s, expires_in_s, message}`. The server waits up
  to three seconds for a quick answer and then replies with whatever the ticket says, so
  a fast Allow returns the result directly, in the same ticket shape.
- The command parks as `McpState::pending_confirm` (the prompt on screen) or, when a
  prompt is already open, in `McpState::deferred` with its ticket. Prompts are shown one
  at a time in arrival order. Everything else, other writes included, keeps running:
  an approved write runs against the project as it is when the user answers.
- Allow runs it and records `applied` with the command's own reply, or `failed` with its
  error (allowed, but nothing applied); Deny records `denied`. A ticket nobody answers
  within `approval_ttl_secs` is `expired`: `drain_agent_commands` checks every frame,
  takes the prompt down, never runs the command, and moves to the next. Answered
  tickets stay readable for 30 minutes (at most 200).
- `approval_status { ticket }` returns the same object; every state carries `applied`.
  The server instructions tell agents to tell the user, poll, and never repeat the call.
- `export_run { validate: true }` behind a prompt answers with the ticket; a task on the
  server waits for it and, once the export is applied, validates the directory and adds
  `validate` to the ticket's `result` (`"running"` until it finishes).

The MCP Tasks extension (rmcp's `TaskManager`) is the protocol's own form of this, but a
client has to opt in per request and the Claude clients do not yet; a tool-level ticket
works with any client. Progress notifications (rmcp can send them for a request that
carries a progress token) were not used either: whether they extend a client's timeout
is up to the client (the TypeScript SDK only does with `resetTimeoutOnProgress`), and a
held-open call is lost when the client reconnects, where a ticket is not.

**Every call says whether it applied.** `TtgServer::exec` waits at most 45 s
(`CALL_TIMEOUT`, below the 60 s many clients allow) and its errors all say whether
anything happened. Behind that is a claim on each queued command (`AgentReply` /
`ReplyWait`, an `AtomicU8` beside the oneshot): the UI thread claims a command just
before running it (`AgentReply::claim`, in `run_agent` and when parking a ticket), and
the server withdraws it on timeout (`ReplyWait::withdraw`). Whichever comes first wins,
so a withdrawn command never runs ("…withdrawn and will not run later, so nothing was
applied"), and a command the UI had already claimed is waited for rather than reported
as not applied. This closes the window the round-2 `is_closed` check left between the
check and the run.

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
revision, every refusal listed, empty and non-matching selections), and for views and
metadata `headless_views_and_metadata`: `view_generate` (the secrets *read by* the
runner, no duplicate of a hand-drawn pair, a second run adding nothing, one undo),
`view_arrange_notes` bringing a moved note back, `layout_tidy { by: "flows" }`
refusing without a view and then ordering the pipeline left to right in the view's
layout while the shared layout stays put, the sequence export, the classification /
description / owner round trip (reply, project, summary, undo, and the bulk form),
the classification filter, and the personal-data view created and then refreshed.
Round 4 adds
`headless_oauth_sign_in_refresh_and_revoke` (the 401 and its `resource_metadata`, both
metadata documents, registration refusing a non-loopback HTTP redirect, authorization
without PKCE bounced to the client, the one-time code read from the server's stdout, a
wrong code refused, the code exchange, the code refused a second time, `tools/list` with
the access token, refresh rotation, revocation turning both access tokens into 401s, the
bearer token still working and an unknown Host refused), `headless_public_url_and_oauth_off`
(issuer and resource under the public host; with OAuth off no metadata and no
`resource_metadata`) and `headless_project_import_and_approval_status` (refused without
`replace`, schema errors with their lines and nothing changed, the import, one undo
back, unknown and listed tickets). Unit tests cover the ticket states, ordering, expiry
and the claim / withdraw race beside `mcp/mod.rs`; PKCE against RFC 7636's example,
redirect rules, codes, grant persistence and revocation, the window app's consent path
through the handlers, and the per-request base URL in `mcp/oauth.rs`; the Host rule in
`mcp/server.rs`; the validator and line index in `ttg_core::schema_check`; and
`ttg check --schema` in `crates/ttg-cli/tests/check_schema.rs`. Codegen tests cover
the other-provider info lines, `Generated::entity_preview` and `views::reveal`; the
catalog crate tests the fingerprint. The unit tests beside `mcp/mod.rs` cover the
dropped-caller rule (fresh and deferred), the heartbeat, the screenshot size clamp,
selectors and the quiet flag. `headless_cost_estimate` covers the three groupings on
job-pipeline (whose one view has four groups), a one-call assumption moving the total
and reported as `call`, the environment note and an explicit region, each refusal, and
that the revision does not move. All run in CI on the same job as the rest of
the workspace.

## 5. Remote access and sign-in (round 4, TF-001)

A cloud agent session (claude.ai, Claude Code on the web) can only use MCP servers it
reaches over the internet, added as custom connectors, and connectors authenticate with
OAuth; the connector form takes no arbitrary header. So the server can be published
through a tunnel and signs clients in itself.

**Listening and Host checks.** `McpSettings::bind` (default `127.0.0.1`; `--bind`) is
the listen address, and `public_url` (`--public-url`) the HTTPS origin a tunnel publishes
it under. rmcp's Streamable HTTP service refuses any `Host` header outside its
`allowed_hosts` to stop DNS rebinding; `ServeOptions::allowed_hosts` keeps the loopback
names and adds the public URL's host (and the bind address when it is one specific
outside address). An axum layer applies the same list to every route, the OAuth pages
included, so a page on another site cannot rebind a name to 127.0.0.1 and drive the
sign-in. cloudflared and Tailscale Funnel forward the public `Host`, which is why the
public URL has to be configured rather than inferred.

**Authorization server** (`mcp/oauth.rs`, mounted only when `oauth` is on; `--oauth`):

| Endpoint | What |
|---|---|
| `GET /.well-known/oauth-protected-resource[/mcp]` | RFC 9728: `resource` `<base>/mcp`, `authorization_servers` `[<base>]`, scope `mcp` |
| `GET /.well-known/oauth-authorization-server` (also `openid-configuration`, and `/mcp`-suffixed) | RFC 8414 metadata: the endpoints below, `code` only, PKCE `S256` only, auth methods `none` / `client_secret_post` / `client_secret_basic`, `authorization_response_iss_parameter_supported` |
| `POST /register` | RFC 7591 dynamic registration. Redirect URIs must be HTTPS, or HTTP to a loopback host (any port, RFC 8252). A client that asks for a secret gets one (its hash is kept). At most 50 clients; the oldest never authorized go first |
| `GET /authorize` | Checks client, redirect URI, `response_type=code`, an S256 `code_challenge` and, if given, `resource` (RFC 8707: must be this server's `/mcp`), then parks a sign-in request (at most five waiting, ten minutes each) and redirects the browser to `/authorize/wait` |
| `GET` / `POST /authorize/wait` | The waiting page. Window app: shows a six-character code and refreshes itself until the user answers the app's prompt. `--serve`: a form for the eight-character one-time code printed on stdout; five wrong tries end the request. Allowed: a single-use authorization code (five minutes) goes back to the client with `state` and `iss`; denied: `error=access_denied` |
| `POST /token` | `authorization_code` (PKCE verified, the code burnt on first use, right or wrong) and `refresh_token` (rotated: the token presented stops working). Access tokens last an hour, refresh tokens thirty days from their last use. `Cache-Control: no-store` |
| `POST /revoke` | RFC 7009: an access token ends itself, a refresh token ends its whole grant |

`/mcp` accepts the bearer token or a live access token. Without either it answers 401
with `WWW-Authenticate: Bearer resource_metadata="<base>/.well-known/oauth-protected-resource/mcp", scope="mcp"`
(plus `error="invalid_token"` when a token was sent), which is how a connector discovers
the sign-in. `<base>` follows the request: the public URL for requests under its host,
`http://<Host>` otherwise, so the same server signs in local and tunnelled clients with
consistent issuer and resource URLs.

**Consent.** In the window app a sign-in shows "Allow <client> to edit this project?"
(`TtgApp::oauth_consent_window`) with the client's redirect host and a code; the browser
page shows the same code, so a sign-in someone else started (anyone can reach
`/authorize` through the tunnel) is told apart from one's own. Headless, there is nobody
to click: `--serve` prints `[oauth] "<client>" asks to use this server … One-time code:
XXXX-XXXX`, and the user types it into the page. Client names are cut to 60 printable
characters and escaped in the page.

**Storage.** Only SHA-256 hashes of client secrets, codes and tokens are kept. Clients and
grants (client, created, last used, refresh-token hash and expiry) persist: with the
other MCP settings in eframe storage for the window app, in `--grants FILE` (rewritten
on every change) for `--serve`, and only in memory otherwise. Access tokens, codes and
sign-in requests are memory-only, so a restart costs a client one refresh. Agent ▸
Settings & activity ▸ Remote access lists the grants with Revoke and Revoke all;
revoking drops the grant and its access tokens at once.

**What rmcp provided.** The Streamable HTTP transport, sessions and the Host / Origin
checks. rmcp 3.2's `auth` feature is client-side only (an OAuth client, discovery and a
credential store, on reqwest), so the authorization server, the token check in front of
`/mcp` and the consent flow are this crate's, on axum, with `sha2`, `rand` (OS-seeded
CSPRNG) and `base64`.

**Security trade-offs**, spelled out to users in the README:

- A tunnel makes the server reachable by anyone who learns the URL. Nothing is served
  without the bearer token or an access token, every grant needs the user's approval,
  registration alone grants nothing, and waiting sign-ins are capped.
- A grant is as powerful as the token: read and change the diagram, and save, open and
  export by path, which writes files wherever the user can. The window app's "Ask
  before" settings still apply to OAuth clients; `--serve` never asks, so a public
  headless server should run as a restricted user or in a container.
- The bearer token is accepted through the tunnel too (it is 128 random bits); regenerate
  it if it leaks. Revoke grants that are no longer used, and stop the tunnel when done.
- An identity-aware proxy in front (Cloudflare Access) cannot cover `/mcp` or `/token`,
  which claude.ai's servers call without a browser; it can cover `/authorize*`, which
  only the user's browser loads.

## 6. Open ideas

- Prompts (`prompts/list`): canned "review this diagram" / "make it Azure-ready" starters.
- Progress notifications for long `export_run --validate` calls.
- A `tasks`-style handle for validate so the tool returns immediately.
