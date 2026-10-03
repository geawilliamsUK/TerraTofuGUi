//! Built-in MCP server (optional `mcp` cargo feature, off at runtime until enabled).
//!
//! The app hosts a Model Context Protocol server over Streamable HTTP on localhost so an
//! agent (Claude Code or any MCP client) can read and edit the diagram *while the user
//! watches*. Nothing runs until the user switches it on (Agent ▸ MCP server): switching
//! on starts a tokio runtime on its own thread; switching off, or closing the app,
//! cancels every session and lets the runtime exit, so the feature costs nothing while
//! idle.
//!
//! Tool calls arrive on the server thread and are executed on the UI thread: each
//! [`AgentCommand`] is queued with a reply channel, the UI is woken with
//! `request_repaint`, and [`TtgApp::drain_agent_commands`] applies it on the next frame
//! through the same snapshot/finish path as a mouse edit (so it is undoable, marks the
//! project dirty, and is drawn immediately).

pub mod approvals;
pub mod exec;
pub mod oauth;
pub mod server;

use crate::app::TtgApp;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use ttg_core::Id;

/// What the server thread may know about the UI thread without touching it: when the
/// UI last drained the command queue, and the long operation it is inside, if any.
///
/// A tool call that arrives while the UI is blocked used to sit in the channel until
/// the block cleared — long after the caller had timed out and retried, which applied
/// the work twice. The server reads this instead and refuses up front, so nothing is
/// queued that nobody is waiting for.
#[derive(Debug)]
pub struct Heartbeat {
    state: Mutex<(Instant, Option<String>)>,
}

impl Default for Heartbeat {
    fn default() -> Self {
        Heartbeat {
            state: Mutex::new((Instant::now(), None)),
        }
    }
}

impl Heartbeat {
    /// The UI drained the queue: it is alive and answering. Called once per frame.
    pub fn beat(&self) {
        let mut s = self.state.lock().unwrap();
        s.0 = Instant::now();
        s.1 = None;
    }

    /// Mark (or clear) a long operation that keeps the UI thread out of `update` — a
    /// modal file dialog, an export — so the server can say what it is waiting for.
    pub fn set_busy(&self, what: Option<&str>) {
        self.state.lock().unwrap().1 = what.map(|w| w.to_string());
    }

    /// How long since the last drain, and whatever the app said it was doing.
    pub fn since_drain(&self) -> (Duration, Option<String>) {
        let s = self.state.lock().unwrap();
        (s.0.elapsed(), s.1.clone())
    }

    /// `Some((how long, what))` when the app has not drained for `limit`. An idle app
    /// is stale too — it only wakes on `request_repaint` — so the caller gives it a
    /// moment to answer before believing this.
    pub fn stalled(&self, limit: Duration) -> Option<(Duration, Option<String>)> {
        let (since, what) = self.since_drain();
        (since > limit).then_some((since, what))
    }
}

/// Default control port. Fixed so the `claude mcp add` URL stays stable across runs.
/// (Not in the 6xxx-7xxx block: Windows reserves large chunks of it for Hyper-V/WinNAT.)
pub const DEFAULT_PORT: u16 = 9337;

/// Persistent settings (eframe storage).
#[derive(Debug, Clone)]
pub struct McpSettings {
    /// Start the server when the app starts.
    pub autostart: bool,
    pub port: u16,
    /// Bearer token every request must carry. Persistent so the client config
    /// survives restarts; regenerable from the settings window.
    pub token: String,
    /// Ask before the agent saves, opens, starts a new project or writes an export.
    pub confirm_disk: bool,
    /// Ask before the agent deletes resources, links or annotations.
    pub confirm_delete: bool,
    /// Address to listen on. `127.0.0.1` (the default) keeps the server on this machine;
    /// a tunnel (cloudflared, Tailscale Funnel) connects to it there.
    pub bind: String,
    /// The HTTPS URL a tunnel publishes the server under (`https://ttg.example.com`), or
    /// empty. Its host name is added to the Host headers the server accepts, and OAuth
    /// metadata names it as the issuer for requests that arrive through it.
    pub public_url: String,
    /// Serve the OAuth endpoints, so clients that can only sign in with OAuth (claude.ai
    /// custom connectors) can connect. The bearer token keeps working either way.
    pub oauth: bool,
    /// How long an approval prompt waits before its ticket expires unapplied.
    pub approval_ttl_secs: u64,
}

impl Default for McpSettings {
    fn default() -> Self {
        McpSettings {
            autostart: false,
            port: DEFAULT_PORT,
            token: new_token(),
            confirm_disk: true,
            confirm_delete: false,
            bind: "127.0.0.1".into(),
            public_url: String::new(),
            oauth: false,
            approval_ttl_secs: approvals::DEFAULT_TTL_SECS,
        }
    }
}

impl McpSettings {
    /// The listen address, or why the setting is not one.
    pub fn bind_addr(&self) -> Result<std::net::IpAddr, String> {
        let b = self.bind.trim();
        let b = if b.is_empty() { "127.0.0.1" } else { b };
        b.parse().map_err(|_| {
            format!(
                "bind address \"{b}\" is not an IP address (use 127.0.0.1, or 0.0.0.0 for every interface)"
            )
        })
    }

    /// The public URL without a trailing slash, when one is set and well-formed.
    pub fn public_url(&self) -> Result<Option<String>, String> {
        let u = self.public_url.trim().trim_end_matches('/');
        if u.is_empty() {
            return Ok(None);
        }
        let parsed: axum::http::Uri = u
            .parse()
            .map_err(|_| format!("public URL \"{u}\" is not a URL"))?;
        if parsed.scheme_str() != Some("https") && parsed.scheme_str() != Some("http") {
            return Err(format!("public URL \"{u}\" must start with https://"));
        }
        if parsed.host().is_none() {
            return Err(format!("public URL \"{u}\" has no host name"));
        }
        if parsed.path() != "/" && !parsed.path().is_empty() {
            return Err(format!(
                "public URL \"{u}\" must be the bare origin (no path); the MCP endpoint is <url>/mcp"
            ));
        }
        Ok(Some(u.to_string()))
    }
}

/// How a queued command and its caller agree on whether it runs: the UI thread claims
/// it just before running it, the server withdraws it when the call times out, and
/// whichever happens first wins. So a call that timed out can say for certain that
/// nothing was applied, and a command that had already started is waited for.
const REPLY_OPEN: u8 = 0;
const REPLY_CLAIMED: u8 = 1;
const REPLY_WITHDRAWN: u8 = 2;

/// The UI thread's end of one tool call: where the answer goes.
#[derive(Debug)]
pub struct AgentReply {
    tx: tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>,
    claim: Arc<AtomicU8>,
}

/// The server thread's end of one tool call.
#[derive(Debug)]
pub struct ReplyWait {
    pub rx: tokio::sync::oneshot::Receiver<Result<serde_json::Value, String>>,
    claim: Arc<AtomicU8>,
}

impl AgentReply {
    pub fn channel() -> (AgentReply, ReplyWait) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let claim = Arc::new(AtomicU8::new(REPLY_OPEN));
        (
            AgentReply {
                tx,
                claim: claim.clone(),
            },
            ReplyWait { rx, claim },
        )
    }

    /// Nobody is waiting any more: the caller went away or withdrew the call.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed() || self.claim.load(Ordering::SeqCst) == REPLY_WITHDRAWN
    }

    /// Take the command for running. `false`: the caller has withdrawn it (or gone), so
    /// it must not run.
    pub fn claim(&self) -> bool {
        if self.tx.is_closed() {
            return false;
        }
        match self
            .claim
            .compare_exchange(REPLY_OPEN, REPLY_CLAIMED, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => true,
            Err(now) => now == REPLY_CLAIMED,
        }
    }

    pub fn send(self, r: Result<serde_json::Value, String>) -> Result<(), Result<serde_json::Value, String>> {
        self.tx.send(r)
    }
}

impl ReplyWait {
    /// Take the call back before the UI has started it. `false` means the UI already
    /// claimed it: it is running (or has run), and its answer is on the way.
    pub fn withdraw(&self) -> bool {
        self.claim
            .compare_exchange(REPLY_OPEN, REPLY_WITHDRAWN, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    #[cfg(test)]
    pub fn try_recv(
        &mut self,
    ) -> Result<Result<serde_json::Value, String>, tokio::sync::oneshot::error::TryRecvError> {
        self.rx.try_recv()
    }
}

pub fn new_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Which entities a bulk write applies to. Every criterion that is given must hold, so
/// `types` with `name_glob` means "of these types, whose name matches". An empty
/// selector is refused, and so is one that matches nothing.
#[derive(Debug, Clone, Default)]
pub struct Selector {
    /// Abstract type ids (`object_storage`, `native:aws:aws_vpc_endpoint`).
    pub types: Vec<String>,
    /// Case-insensitive `*` / `?` glob on the display name.
    pub name_glob: Option<String>,
    /// Entity ids or display names.
    pub ids: Vec<String>,
}

impl Selector {
    /// No criterion at all: would mean "every entity", which no bulk edit intends.
    pub fn is_empty(&self) -> bool {
        self.types.is_empty() && self.name_glob.as_deref().is_none_or(str::is_empty) && self.ids.is_empty()
    }
}

/// The part of `entity_update` a bulk update repeats on every selected entity (a bulk
/// update cannot rename: names must stay unique).
#[derive(Debug, Clone, Default)]
pub struct EntityChanges {
    pub config: Option<serde_json::Map<String, serde_json::Value>>,
    pub provider_config: Option<serde_json::Map<String, serde_json::Value>>,
    pub manual: Option<bool>,
    pub providers: Option<Vec<String>>,
    pub extra: Option<serde_json::Map<String, serde_json::Value>>,
    pub extra_provider: Option<String>,
    pub extra_block: Option<String>,
    /// Classification, description and owner.
    pub meta: EntityMeta,
}

impl EntityChanges {
    pub fn is_empty(&self) -> bool {
        self.config.is_none()
            && self.provider_config.is_none()
            && self.manual.is_none()
            && self.providers.is_none()
            && self.extra.is_none()
            && self.meta.is_empty()
    }
}

/// What an entity is for, as `entity_update` sets it: each `Some` replaces the value,
/// and an empty string (or `"none"` for the classification) clears it.
#[derive(Debug, Clone, Default)]
pub struct EntityMeta {
    pub classification: Option<String>,
    pub description: Option<String>,
    pub owner: Option<String>,
}

impl EntityMeta {
    pub fn is_empty(&self) -> bool {
        self.classification.is_none() && self.description.is_none() && self.owner.is_none()
    }
}

/// A request from the agent, executed on the UI thread. Names may be entity ids or
/// display names (case-insensitive); the executor resolves them.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum AgentCommand {
    ProjectGet,
    ProjectSummary,
    CatalogTypes,
    CatalogType {
        type_id: String,
    },
    Diagnostics,
    ReachPosture,
    ReachFrom {
        entity: String,
    },
    ReachTo {
        entity: String,
    },
    ExportPreview {
        provider: Option<String>,
        /// Overrides `settings.kubernetes_manifests` for this call only.
        k8s: Option<bool>,
    },
    /// A PNG of the window. `view` activates a view first, `fit` frames its content and
    /// `hide_panels` drops the side panels for that one frame. `width`/`height` resize
    /// the window for the capture (and put it back afterwards), because egui cannot
    /// render off-screen here and a small window makes a fitted view unreadable.
    Screenshot {
        fit: bool,
        view: Option<String>,
        hide_panels: bool,
        width: Option<u32>,
        height: Option<u32>,
    },
    EntityAdd {
        type_id: String,
        name: Option<String>,
        parent: Option<String>,
        x: Option<i32>,
        y: Option<i32>,
        providers: Option<Vec<String>>,
        /// `Some(false)`: reply with only `status`, `id` and `changed`; `None`: the
        /// session default (`settings_set { verbose }`).
        verbose: Option<bool>,
    },
    EntityUpdate {
        entity: String,
        name: Option<String>,
        config: Option<serde_json::Map<String, serde_json::Value>>,
        provider_config: Option<serde_json::Map<String, serde_json::Value>>,
        manual: Option<bool>,
        providers: Option<Vec<String>>,
        extra: Option<serde_json::Map<String, serde_json::Value>>,
        extra_provider: Option<String>,
        extra_block: Option<String>,
        /// Classification, description and owner.
        meta: EntityMeta,
        /// `Some(false)`: reply with only `status`, `id` and `changed`; `None`: the
        /// session default (`settings_set { verbose }`).
        verbose: Option<bool>,
    },
    SchemaSearch {
        provider: Option<String>,
        query: String,
        /// `resource`, `data`, or `None` for both.
        kind: Option<String>,
    },
    SchemaShow {
        provider: Option<String>,
        resource: String,
        /// `data` reads `resource` as a data source (as `data.<type>` does).
        kind: Option<String>,
        /// Levels of nested blocks to include (0 = attributes only); `None` = everything.
        depth: Option<u32>,
        /// Only required attributes and nested blocks.
        required_only: Option<bool>,
    },
    EntityMove {
        entity: String,
        x: i32,
        y: i32,
        /// Move it in this view's own layout instead of the active view's.
        view: Option<String>,
    },
    EntityResize {
        entity: String,
        w: i32,
        h: i32,
        /// Resize it in this view instead of the active one (annotations are per-view).
        view: Option<String>,
    },
    EntitySetParent {
        entity: String,
        parent: Option<String>,
    },
    EntityDelete {
        entities: Vec<String>,
    },
    LinkAdd {
        source: String,
        target: String,
        relation: String,
        providers: Option<Vec<String>>,
        /// `Some(false)`: reply with only `status`, `id` and `changed`; `None`: the
        /// session default (`settings_set { verbose }`).
        verbose: Option<bool>,
    },
    LinkRemove {
        source: String,
        target: String,
        relation: Option<String>,
    },
    SelectionSet {
        entities: Vec<String>,
    },
    ViewSet {
        filter: serde_json::Value,
    },
    /// Save the working filter as a named view. An existing name is refused unless
    /// `replace`, which rewrites that view's filter and keeps everything else it holds.
    ViewSave {
        name: String,
        replace: bool,
    },
    /// Delete a saved view; the canvas falls back to All when it was the active one.
    ViewDelete {
        name: String,
    },
    ViewActivate {
        name: String,
    },
    /// Everything one view holds: filter, description, layout and annotations.
    ViewGet {
        name: Option<String>,
    },
    /// Rename a view, describe it, turn its legend on, or replace its saved filter.
    ViewUpdate {
        view: Option<String>,
        name: Option<String>,
        description: Option<String>,
        legend: Option<bool>,
        filter: Option<serde_json::Value>,
    },
    /// Frame a view's content in the camera (and switch to it when named).
    ViewFit {
        view: Option<String>,
    },
    /// A view as a Markdown or Mermaid document.
    ViewExport {
        view: Option<String>,
        format: String,
    },
    GroupAdd {
        view: Option<String>,
        label: String,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        color: Option<String>,
    },
    FlowAdd {
        view: Option<String>,
        from: String,
        to: String,
        label: String,
        dashed: bool,
        step: Option<u32>,
        color: Option<String>,
        /// Add a hidden end to the view's filter instead of only warning about it.
        show_hidden: bool,
        /// What travels along it ("call audio").
        data: Option<String>,
    },
    NoteAdd {
        view: Option<String>,
        title: String,
        body: String,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<i32>,
        h: Option<i32>,
        /// Resource, group, logical node or flow the note explains.
        anchor: Option<String>,
    },
    LogicalAdd {
        view: Option<String>,
        name: String,
        icon: Option<String>,
        subtitle: Option<String>,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<i32>,
        h: Option<i32>,
    },
    AnnotationRemove {
        view: Option<String>,
        key: String,
    },
    LayoutTidy {
        container: Option<String>,
        /// Tidy this view (in its own layout) and put its anchored notes back beside
        /// their anchors afterwards.
        view: Option<String>,
        /// `links` (the default: columns by dependency) or `flows` (the view's data
        /// flows, left to right in step order; needs a view).
        by: Option<String>,
    },
    // View tools: flows generated from links, notes put back beside their anchors.
    /// Derive flows from the links (`data_flow`) or build "Where personal data goes"
    /// (`personal_data`).
    ViewGenerate {
        view: Option<String>,
        kind: String,
        replace: bool,
    },
    /// Put every anchored note of a view back beside what it explains.
    ViewArrangeNotes {
        view: Option<String>,
    },
    LayoutAlign {
        how: String,
    },
    LayoutDistribute {
        horizontal: bool,
    },
    SettingsSet {
        tool: Option<String>,
        provider: Option<String>,
        provider_settings: Option<serde_json::Map<String, serde_json::Value>>,
        /// Replaces the project's default tags outright; `{}` clears them.
        tags: Option<serde_json::Map<String, serde_json::Value>>,
        kubernetes_manifests: Option<bool>,
        /// `Some(null)` clears the backend; `None` leaves it alone.
        backend: Option<serde_json::Value>,
        state_encryption: Option<bool>,
        /// Id or name; `Some(null)` / `Some("")` clears it.
        state_encryption_key: Option<serde_json::Value>,
        /// Merged into the pins; a null or empty value removes one.
        provider_versions: Option<serde_json::Map<String, serde_json::Value>>,
        /// The project's name (used in the export's README and the default state key).
        name: Option<String>,
        /// Session default for entity_add / entity_update / link_add replies: `false`
        /// returns only status, id and changed. Not saved with the project.
        verbose: Option<bool>,
    },
    ProjectSave {
        path: Option<String>,
    },
    ProjectOpen {
        path: String,
    },
    ProjectNew {
        name: Option<String>,
    },
    ExportRun {
        dir: String,
        provider: Option<String>,
        /// Overrides `settings.kubernetes_manifests` for this export only.
        k8s: Option<bool>,
    },
    /// What an export would change in an existing directory (nothing is written).
    ExportDiff {
        dir: String,
        provider: Option<String>,
        k8s: Option<bool>,
    },
    /// Has the project changed since revision `since` (user or agent edits)?
    Changes {
        since: Option<u64>,
    },
    /// Several write commands as one undo step; any failure rolls all of them back.
    Batch(Vec<AgentCommand>),
    Undo,
    Redo,

    // ---- Scoped calls: slices, filters, previews, dry runs and bulk writes.
    // Their bodies are in `exec/scoped.rs`.
    /// `project_get` with `fields` and/or `entities`: a slice of the project.
    ProjectSlice {
        fields: Option<Vec<String>>,
        entities: Option<Vec<String>>,
    },
    /// `diagnostics` with a filter (and, optionally, another provider's run).
    DiagnosticsFiltered {
        entity: Option<String>,
        severity: Option<String>,
        provider: Option<String>,
    },
    /// The relations the definitions allow, optionally between two types.
    CatalogRelations {
        source_type: Option<String>,
        target_type: Option<String>,
    },
    /// One entity's share of an export.
    EntityPreview {
        entity: String,
        provider: Option<String>,
    },
    /// Run a batch, report what it did and how the diagnostics moved, then take it all
    /// back: nothing lands in the project or the undo history.
    DryRun(Vec<AgentCommand>),
    /// `entity_update` on every entity a selector matches, as one undo step.
    BulkUpdate {
        select: Selector,
        changes: EntityChanges,
    },
    /// `link_add` from every entity a selector matches to one target, as one undo step.
    BulkLink {
        select: Selector,
        target: String,
        relation: String,
        providers: Option<Vec<String>>,
    },
    /// The monthly cost estimate (`exec/cost.rs`). Read-only.
    CostEstimate {
        provider: Option<String>,
        environment: Option<String>,
        view: Option<String>,
        group_by: Option<String>,
        assumptions: Option<serde_json::Map<String, serde_json::Value>>,
        region: Option<String>,
    },

    // ---- File interchange (`exec/import.rs`).
    /// Replace the open project with one given as `.ttg.json` text, as one undo step.
    /// Without `replace` it is refused while the open project has any entities.
    ProjectImport {
        json: String,
        replace: bool,
    },

    // ---- Environments and plans. Their bodies are in `exec/environments.rs`.
    /// The `settings_set` keys about environments: the list, a rename, the name prefix
    /// (`Some(null)` clears it) and project variables (merged; `null` removes one).
    EnvSettings {
        environments: Option<Vec<String>>,
        rename: Option<(String, String)>,
        name_prefix: Option<serde_json::Value>,
        variables: Option<serde_json::Map<String, serde_json::Value>>,
    },
    /// `entity_update { environment, … }`: one environment's values, or its presence, for
    /// one entity or every entity a selector matches (one undo step, all-or-nothing).
    EnvUpdate {
        entity: Option<String>,
        select: Option<Selector>,
        environment: String,
        config: Option<serde_json::Map<String, serde_json::Value>>,
        provider_config: Option<serde_json::Map<String, serde_json::Value>>,
        present: Option<bool>,
    },
    /// The environments a link belongs to (`[]` = every environment).
    LinkEnvironments {
        source: String,
        target: String,
        relation: String,
        environments: Vec<String>,
    },
    /// `plan_run`'s first half: export into `dir` and say what its addresses and lines
    /// belong to. The plan itself runs off the UI thread, in the server.
    PlanPrepare {
        /// `None`: a directory of its own under the system's temporary one.
        dir: Option<String>,
        provider: Option<String>,
        environment: Option<String>,
        tool: Option<ttg_core::Tool>,
    },
    /// What an export of a provider attributes to which entity (for `validate`).
    Attribution {
        provider: Option<String>,
    },
    /// Put a finished plan on the canvas: +, ~, −, ± badges until the next edit.
    PlanShow {
        report: serde_json::Value,
    },
}

impl AgentCommand {
    /// Short label for the activity log.
    pub fn label(&self) -> String {
        if let AgentCommand::Batch(cmds) = self {
            return format!("Batch({})", cmds.len());
        }
        let s = format!("{self:?}");
        s.split([' ', '{']).next().unwrap_or("?").to_string()
    }

    /// Why this command needs the user's approval under the current settings, if it does.
    pub fn confirm_reason(&self, s: &McpSettings) -> Option<String> {
        match self {
            AgentCommand::ProjectSave { path } if s.confirm_disk => Some(format!(
                "save the project{}",
                path.as_ref().map(|p| format!(" to {p}")).unwrap_or_default()
            )),
            AgentCommand::ProjectOpen { path } if s.confirm_disk => {
                Some(format!("open {path}, replacing the current project"))
            }
            AgentCommand::ProjectNew { .. } if s.confirm_disk => Some("start a new project".into()),
            AgentCommand::ExportRun { dir, .. } if s.confirm_disk => {
                Some(format!("write an export into {dir}"))
            }
            AgentCommand::PlanPrepare { dir, .. } if s.confirm_disk => Some(format!(
                "write an export into {} and plan it",
                dir.as_deref().unwrap_or("a temporary folder")
            )),
            AgentCommand::EntityDelete { entities } if s.confirm_delete => {
                Some(format!("delete {}", entities.join(", ")))
            }
            AgentCommand::LinkRemove { source, target, .. } if s.confirm_delete => {
                Some(format!("remove the link {source} \u{2192} {target}"))
            }
            // Annotations (groups, flows) are architecture-map decoration only; they are
            // never exported, so removing one never needs approval.
            AgentCommand::Batch(cmds) => {
                let reasons: Vec<String> = cmds.iter().filter_map(|c| c.confirm_reason(s)).collect();
                if reasons.is_empty() {
                    None
                } else {
                    Some(format!(
                        "apply a batch of {} commands that would: {}",
                        cmds.len(),
                        reasons.join("; ")
                    ))
                }
            }
            _ => None,
        }
    }
    /// Whether this command's reply is cut down to status, id and changed: an explicit
    /// `verbose: false`, or the session default for the three commands that take it.
    pub fn terse(&self, default: bool) -> bool {
        match self {
            AgentCommand::EntityAdd { verbose, .. }
            | AgentCommand::EntityUpdate { verbose, .. }
            | AgentCommand::LinkAdd { verbose, .. } => verbose.map(|v| !v).unwrap_or(default),
            _ => false,
        }
    }

    pub fn is_write(&self) -> bool {
        !matches!(
            self,
            AgentCommand::ProjectGet
                | AgentCommand::ProjectSummary
                | AgentCommand::CatalogTypes
                | AgentCommand::CatalogType { .. }
                | AgentCommand::Diagnostics
                | AgentCommand::ReachPosture
                | AgentCommand::ReachFrom { .. }
                | AgentCommand::ReachTo { .. }
                | AgentCommand::ExportPreview { .. }
                | AgentCommand::Screenshot { .. }
                | AgentCommand::ViewGet { .. }
                | AgentCommand::ViewExport { .. }
                | AgentCommand::ViewFit { .. }
                | AgentCommand::SchemaSearch { .. }
                | AgentCommand::SchemaShow { .. }
                | AgentCommand::ExportDiff { .. }
                | AgentCommand::Changes { .. }
                | AgentCommand::ProjectSlice { .. }
                | AgentCommand::DiagnosticsFiltered { .. }
                | AgentCommand::CatalogRelations { .. }
                | AgentCommand::EntityPreview { .. }
                | AgentCommand::CostEstimate { .. }
                | AgentCommand::Attribution { .. }
                | AgentCommand::PlanShow { .. }
        )
    }
}

/// The short form of an entity_add / entity_update / link_add reply. A link's id is
/// `<source> -<relation>-> <target>`.
fn terse_reply(r: &serde_json::Value, changed: bool) -> serde_json::Value {
    let id = r
        .get("id")
        .cloned()
        .or_else(|| r.get("entity").and_then(|e| e.get("id")).cloned())
        .or_else(|| {
            let (s, t, rel) = (r.get("source")?, r.get("target")?, r.get("relation")?);
            Some(serde_json::Value::String(format!(
                "{} -{}-> {}",
                s.as_str()?,
                rel.as_str()?,
                t.as_str()?
            )))
        })
        .unwrap_or(serde_json::Value::Null);
    serde_json::json!({"status": r["status"], "id": id, "changed": changed})
}

/// Events pushed from the UI thread to the server runtime (fan-out to subscribed
/// clients as `notifications/resources/updated`).
#[derive(Debug, Clone)]
pub enum ServerEvent {
    ProjectChanged,
}

/// A write the agent asked for that waits for the user's Allow / Deny. Its caller already
/// has `ticket`; the outcome is recorded in [`approvals::Approvals`].
pub struct PendingConfirm {
    pub cmd: AgentCommand,
    pub ticket: String,
    pub reason: String,
    pub at: Instant,
}

/// A running server.
pub struct Running {
    cancel: tokio_util::sync::CancellationToken,
    events: tokio::sync::mpsc::UnboundedSender<ServerEvent>,
}

/// What the server thread hands back once it is listening.
pub type Started = (
    tokio_util::sync::CancellationToken,
    tokio::sync::mpsc::UnboundedSender<ServerEvent>,
);

/// One log line per agent command.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub at: Instant,
    pub command: String,
    pub ok: bool,
    pub summary: String,
}

/// Everything the app keeps for the MCP feature.
pub struct McpState {
    pub settings: McpSettings,
    /// entity_add / entity_update / link_add answer with only status, id and changed
    /// unless a call asks for more (`settings_set { verbose: false }`; this session only).
    pub terse_replies: bool,
    pub running: Option<Running>,
    /// Commands queued by the server thread.
    pub rx: mpsc::Receiver<(AgentCommand, AgentReply)>,
    pub tx: mpsc::Sender<(AgentCommand, AgentReply)>,
    /// Screenshot requests waiting for the next `Event::Screenshot`.
    pub pending_screenshots: Vec<AgentReply>,
    /// Frames to let the view settle (activate, fit, hidden panels) before capturing.
    pub screenshot_after: u8,
    /// Zoom to fit once the window has reached the size the screenshot asked for.
    pub screenshot_fit: bool,
    /// Inner size to put the window back to after a resized screenshot.
    pub screenshot_restore: Option<egui::Vec2>,
    /// Inner size a resized screenshot asked for, for the reply's metadata.
    pub screenshot_asked: Option<egui::Vec2>,
    /// Read by the server thread to tell a blocked UI from an idle one.
    pub heartbeat: Arc<Heartbeat>,
    pub log: Vec<LogEntry>,
    /// Entities changed by the agent, flashed on the canvas for a moment.
    pub flash: Vec<(Id, Instant)>,
    pub last_error: Option<String>,
    pub show_window: bool,
    /// The write whose Allow / Deny prompt is on screen. Its caller was answered at once
    /// with a ticket; every other call keeps running meanwhile.
    pub pending_confirm: Option<PendingConfirm>,
    /// Further writes that need approval and arrived while a prompt was already open,
    /// with their tickets, in arrival order. Prompted one at a time as `pending_confirm`
    /// is resolved.
    pub deferred: VecDeque<(AgentCommand, String)>,
    /// Approval tickets, shared with the server thread for `approval_status`.
    pub approvals: Arc<approvals::Approvals>,
    /// OAuth clients, grants and sign-in requests, shared with the server thread.
    pub oauth: Arc<oauth::OAuth>,
    /// Bumped on every committed change (user or agent); `project_changes` reports it.
    pub revision: u64,
    pub last_change: Option<(Instant, &'static str)>,
    /// Set while an agent command runs, so `finish()` can attribute the change.
    pub in_agent: bool,
    /// `--serve` mode: no window, no screenshots, no confirmation prompts.
    pub headless: bool,
    /// Fingerprint of the loaded definitions, for `serverInfo` (see
    /// `ttg_catalog::load::fingerprint_of`). Set when the app is built.
    pub catalog_hash: String,
    /// Set while a dry run (or a bulk write that may still be rolled back) executes:
    /// `note_change` then neither bumps the revision nor notifies subscribers, so a
    /// change that never lands is invisible to `project_changes`.
    pub quiet: bool,
}

impl Default for McpState {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        McpState {
            settings: McpSettings::default(),
            terse_replies: false,
            running: None,
            rx,
            tx,
            pending_screenshots: Vec::new(),
            screenshot_after: 0,
            screenshot_fit: false,
            screenshot_restore: None,
            screenshot_asked: None,
            heartbeat: Arc::new(Heartbeat::default()),
            log: Vec::new(),
            flash: Vec::new(),
            last_error: None,
            show_window: false,
            pending_confirm: None,
            deferred: VecDeque::new(),
            approvals: Arc::new(approvals::Approvals::default()),
            oauth: Arc::new(oauth::OAuth::default()),
            revision: 0,
            last_change: None,
            in_agent: false,
            headless: false,
            catalog_hash: String::new(),
            quiet: false,
        }
    }
}

impl McpState {
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    pub fn url(&self) -> String {
        // A server bound to one outside address is not on 127.0.0.1.
        let host = match self.settings.bind_addr() {
            Ok(ip) if !ip.is_loopback() && !ip.is_unspecified() => match ip {
                std::net::IpAddr::V6(v6) => format!("[{v6}]"),
                v4 => v4.to_string(),
            },
            _ => "127.0.0.1".to_string(),
        };
        format!("http://{host}:{}/mcp", self.settings.port)
    }

    /// The command that registers this server with Claude Code.
    pub fn claude_add_command(&self) -> String {
        format!(
            "claude mcp add --transport http terratofu {} --header \"Authorization: Bearer {}\"",
            self.url(),
            self.settings.token
        )
    }

    /// The URL a remote client (a claude.ai custom connector) is given: the public URL's
    /// `/mcp`, when one is configured.
    pub fn connector_url(&self) -> Option<String> {
        self.settings
            .public_url()
            .ok()
            .flatten()
            .map(|u| format!("{u}/mcp"))
    }

    /// Start the server thread. Returns quickly; bind errors are reported.
    pub fn start(&mut self, ctx: egui::Context) -> Result<(), String> {
        if self.running.is_some() {
            return Ok(());
        }
        let checked = self
            .settings
            .bind_addr()
            .and_then(|b| self.settings.public_url().map(|u| (b, u)));
        let (bind, public_url) = match checked {
            Ok(v) => v,
            Err(e) => {
                self.last_error = Some(e.clone());
                return Err(e);
            }
        };
        self.oauth.set_waker(ctx.clone());
        self.oauth.set_headless(self.headless);
        let opts = server::ServeOptions {
            port: self.settings.port,
            bind,
            token: self.settings.token.clone(),
            public_url,
            oauth: self.settings.oauth.then(|| self.oauth.clone()),
            approvals: self.approvals.clone(),
        };
        let tx = self.tx.clone();
        let beat = self.heartbeat.clone();
        let catalog_hash = self.catalog_hash.clone();
        let (started_tx, started_rx) = mpsc::channel::<Result<Started, String>>();
        std::thread::Builder::new()
            .name("ttg-mcp".into())
            .spawn(move || server::run(opts, tx, ctx, beat, catalog_hash, started_tx))
            .map_err(|e| e.to_string())?;
        match started_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok((cancel, events))) => {
                self.running = Some(Running { cancel, events });
                self.last_error = None;
                Ok(())
            }
            Ok(Err(e)) => {
                self.last_error = Some(e.clone());
                Err(e)
            }
            Err(_) => {
                let e = "server thread did not report back in time".to_string();
                self.last_error = Some(e.clone());
                Err(e)
            }
        }
    }

    /// Cancel every session and let the runtime exit.
    pub fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            r.cancel.cancel();
        }
    }

    /// Record a committed change to the project and tell subscribed clients.
    pub fn note_change(&mut self) {
        if self.quiet {
            return;
        }
        self.revision += 1;
        self.last_change = Some((Instant::now(), if self.in_agent { "agent" } else { "user" }));
        if let Some(r) = &self.running {
            let _ = r.events.send(ServerEvent::ProjectChanged);
        }
    }

    pub fn push_log(&mut self, command: String, result: &Result<serde_json::Value, String>) {
        let (ok, summary) = match result {
            Ok(v) => (true, summarize(v)),
            Err(e) => (false, e.clone()),
        };
        self.log.push(LogEntry {
            at: Instant::now(),
            command,
            ok,
            summary,
        });
        if self.log.len() > 200 {
            self.log.remove(0);
        }
    }
}

fn summarize(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(m) => {
            if let Some(s) = m.get("status").and_then(|s| s.as_str()) {
                return s.to_string();
            }
            let s = v.to_string();
            if s.len() > 90 {
                format!("{}…", &s[..90])
            } else {
                s
            }
        }
        other => {
            let s = other.to_string();
            if s.len() > 90 {
                format!("{}…", &s[..90])
            } else {
                s
            }
        }
    }
}

impl TtgApp {
    /// Apply queued agent commands on the UI thread (called once per frame). A write that
    /// needs the user's approval answers its caller at once with a ticket and waits for
    /// the prompt on its own; everything else keeps running meanwhile.
    pub fn drain_agent_commands(&mut self, ctx: &egui::Context) {
        self.mcp.heartbeat.beat();
        self.expire_approvals();
        loop {
            let next = self.mcp.rx.try_recv();
            let Ok((cmd, reply)) = next else { break };
            // Nobody is waiting for this any more: the call timed out or the client
            // went away. Running it would apply work the caller has already retried.
            if reply.is_closed() {
                self.mcp
                    .push_log(cmd.label(), &Err("dropped: caller gone".into()));
                continue;
            }
            if let AgentCommand::Screenshot {
                fit,
                view,
                hide_panels,
                width,
                height,
            } = cmd
            {
                if self.mcp.headless {
                    let _ = reply.send(Err("no display in --serve mode".into()));
                    continue;
                }
                if let Some(name) = view {
                    if let Err(e) = self.activate_view_named(&name) {
                        let _ = reply.send(Err(e));
                        continue;
                    }
                }
                self.hide_panels = hide_panels;
                self.mcp.pending_screenshots.push(reply);
                match screenshot_size(ctx.viewport_rect().size(), width, height) {
                    // Resized: the window has to reach the new size before the fit,
                    // or the fit frames the content for the old one.
                    Some(size) => {
                        self.mcp.screenshot_restore = Some(ctx.viewport_rect().size());
                        self.mcp.screenshot_asked = Some(size);
                        self.mcp.screenshot_fit = fit;
                        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
                        self.mcp.screenshot_after = 6;
                    }
                    // The view still has to be laid out (and fitted) before the pixels
                    // are worth capturing, so ask a couple of frames from now.
                    None => {
                        self.fit_requested = fit;
                        self.mcp.screenshot_fit = false;
                        self.mcp.screenshot_after = 2;
                    }
                }
                self.mcp.push_log(
                    "Screenshot".into(),
                    &Ok(serde_json::json!({"status": "requested"})),
                );
                continue;
            }
            if !cmd.is_write() {
                self.run_agent(cmd, reply);
                continue;
            }
            self.offer_or_run(cmd, reply);
        }
        // A caller that gave up while its capture was settling gets no picture.
        self.mcp.pending_screenshots.retain(|r| !r.is_closed());
        if self.mcp.pending_screenshots.is_empty() {
            self.mcp.screenshot_after = 0;
        }
        // Ask for the capture once the window has resized and the view has settled.
        if !self.mcp.pending_screenshots.is_empty() && self.mcp.screenshot_after > 0 {
            self.mcp.screenshot_after -= 1;
            if self.mcp.screenshot_after == 2 && self.mcp.screenshot_fit {
                // The window is at its new size now, so the fit uses that size.
                self.mcp.screenshot_fit = false;
                self.fit_requested = true;
            }
            if self.mcp.screenshot_after == 0 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            }
            ctx.request_repaint();
        }
        // Deliver screenshots.
        if !self.mcp.pending_screenshots.is_empty() {
            let image = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Screenshot { image, .. } => Some(image.clone()),
                    _ => None,
                })
            });
            if let Some(img) = image {
                let asked = self.mcp.screenshot_asked.take();
                let result = encode_png(&img)
                    .map(|png| {
                        use base64::Engine;
                        serde_json::json!({
                            "mime": "image/png",
                            "width": img.size[0],
                            "height": img.size[1],
                            "asked_for": asked.map(|s| serde_json::json!({"width": s.x, "height": s.y})),
                            "data": base64::engine::general_purpose::STANDARD.encode(png),
                        })
                    })
                    .map_err(|e| e.to_string());
                for r in self.mcp.pending_screenshots.drain(..) {
                    let _ = r.send(result.clone());
                }
                // Whatever the shot hid, or resized, comes back.
                self.hide_panels = false;
                if let Some(size) = self.mcp.screenshot_restore.take() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
                }
            }
        }
        // Keep repainting while something is flashing.
        let now = Instant::now();
        self.mcp
            .flash
            .retain(|(_, t)| now.duration_since(*t).as_secs_f32() < 1.5);
        if !self.mcp.flash.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(40));
        }
    }

    /// Why `cmd` needs the user's approval right now, if it does. Never in headless
    /// `--serve` mode, which has nobody to ask.
    pub fn approval_reason(&self, cmd: &AgentCommand) -> Option<String> {
        if self.mcp.headless {
            return None;
        }
        // Like opening a file: only discarding unsaved work needs a yes. Without
        // `replace` a non-empty project refuses the import anyway.
        if let AgentCommand::ProjectImport { replace, .. } = cmd {
            let empty = self.project.nodes.is_empty() && self.project.containers.is_empty();
            if self.dirty && (*replace || empty) {
                return Some(
                    "replace the open project, which has unsaved changes, with an imported one".into(),
                );
            }
        }
        cmd.confirm_reason(&self.mcp.settings)
    }

    /// Run a write now, or, when it needs approval, give its caller a ticket at once and
    /// park the command: on screen as `pending_confirm`, or behind the prompt already
    /// there in `deferred`.
    fn offer_or_run(&mut self, cmd: AgentCommand, reply: AgentReply) {
        let Some(reason) = self.approval_reason(&cmd) else {
            self.run_agent(cmd, reply);
            return;
        };
        let label = cmd.label();
        if !reply.claim() {
            self.mcp.push_log(label, &Err("dropped: caller gone".into()));
            return;
        }
        // An import that could never load is refused now, not after the user said yes.
        if let AgentCommand::ProjectImport { json, .. } = &cmd {
            if let Err(e) = exec::import_check(json) {
                self.mcp.push_log(label, &Err(e.clone()));
                let _ = reply.send(Err(e));
                return;
            }
        }
        let ttl = Duration::from_secs(self.mcp.settings.approval_ttl_secs.max(1));
        let ticket = self.mcp.approvals.open(&label, &reason, ttl);
        if self.mcp.pending_confirm.is_none() {
            self.status = "Agent: waiting for your approval".into();
            self.mcp.pending_confirm = Some(PendingConfirm {
                cmd,
                ticket: ticket.clone(),
                reason,
                at: Instant::now(),
            });
        } else {
            self.mcp.deferred.push_back((cmd, ticket.clone()));
        }
        let status = self.mcp.approvals.status_json(&ticket).unwrap_or_else(
            || serde_json::json!({"ticket": ticket, "status": "pending_approval", "applied": false}),
        );
        self.mcp.push_log(label, &Ok(status.clone()));
        let _ = reply.send(Ok(status));
    }

    /// Resolve the pending approval prompt (Allow when `allow` is true, else Deny), the
    /// same decision `confirm_window` makes from its buttons but callable without egui.
    /// The outcome goes to the command's ticket; then the next deferred write, if any,
    /// gets the prompt.
    pub fn resolve_confirm(&mut self, allow: bool) {
        let Some(p) = self.mcp.pending_confirm.take() else {
            return;
        };
        if self.mcp.approvals.is_expired(&p.ticket, Instant::now()) {
            self.expire_ticket(&p.cmd, &p.ticket);
        } else if allow {
            self.run_ticket(p.cmd, &p.ticket);
        } else {
            let result = Err("denied by the user".to_string());
            self.status = format!("Agent: {} denied", p.cmd.label());
            self.mcp.push_log(p.cmd.label(), &result);
            self.mcp.approvals.resolve(&p.ticket, approvals::Outcome::Denied);
        }
        self.next_prompt();
    }

    /// Put the next deferred write on screen, skipping any whose ticket has run out. One
    /// that no longer needs approval (the settings changed meanwhile) simply runs.
    fn next_prompt(&mut self) {
        while self.mcp.pending_confirm.is_none() {
            let Some((cmd, ticket)) = self.mcp.deferred.pop_front() else {
                return;
            };
            if self.mcp.approvals.is_expired(&ticket, Instant::now()) {
                self.expire_ticket(&cmd, &ticket);
                continue;
            }
            match self.approval_reason(&cmd) {
                Some(reason) => {
                    self.status = "Agent: waiting for your approval".into();
                    self.mcp.pending_confirm = Some(PendingConfirm {
                        cmd,
                        ticket,
                        reason,
                        at: Instant::now(),
                    });
                }
                None => self.run_ticket(cmd, &ticket),
            }
        }
    }

    /// Expire every parked write whose ticket has run out, the one on screen included.
    fn expire_approvals(&mut self) {
        let now = Instant::now();
        let on_screen_expired = self
            .mcp
            .pending_confirm
            .as_ref()
            .is_some_and(|p| self.mcp.approvals.is_expired(&p.ticket, now));
        if on_screen_expired {
            if let Some(p) = self.mcp.pending_confirm.take() {
                self.expire_ticket(&p.cmd, &p.ticket);
            }
        }
        let parked = std::mem::take(&mut self.mcp.deferred);
        for (cmd, ticket) in parked {
            if self.mcp.approvals.is_expired(&ticket, now) {
                self.expire_ticket(&cmd, &ticket);
            } else {
                self.mcp.deferred.push_back((cmd, ticket));
            }
        }
        if on_screen_expired {
            self.next_prompt();
        }
    }

    fn expire_ticket(&mut self, cmd: &AgentCommand, ticket: &str) {
        self.status = format!("Agent: {} expired unanswered", cmd.label());
        self.mcp
            .push_log(cmd.label(), &Err("expired: nobody answered the prompt".into()));
        self.mcp.approvals.resolve(ticket, approvals::Outcome::Expired);
    }

    /// Run an approved write and record its outcome on its ticket.
    fn run_ticket(&mut self, cmd: AgentCommand, ticket: &str) {
        let outcome = match self.exec_logged(cmd) {
            Ok(v) => approvals::Outcome::Applied(v),
            Err(e) => approvals::Outcome::Failed(e),
        };
        self.mcp.approvals.resolve(ticket, outcome);
    }

    /// Execute one agent command now, log it and answer the server. A command its caller
    /// has already withdrawn (the call timed out) is dropped, never run.
    fn run_agent(&mut self, cmd: AgentCommand, reply: AgentReply) {
        if !reply.claim() {
            self.mcp
                .push_log(cmd.label(), &Err("dropped: caller gone".into()));
            return;
        }
        let result = self.exec_logged(cmd);
        let _ = reply.send(result);
    }

    /// Execute one agent command, put a write's outcome in the status line, and log it.
    fn exec_logged(&mut self, cmd: AgentCommand) -> Result<serde_json::Value, String> {
        let label = cmd.label();
        let is_write = cmd.is_write();
        let terse = cmd.terse(self.mcp.terse_replies);
        let before = terse.then(|| self.project.clone());
        self.mcp.in_agent = true;
        let result = self.agent_exec(cmd);
        self.mcp.in_agent = false;
        let result = match (result, before) {
            (Ok(r), Some(before)) => Ok(terse_reply(&r, before != self.project)),
            (other, _) => other,
        };
        if is_write {
            self.status = match &result {
                Ok(_) => format!("Agent: {label}"),
                Err(e) => format!("Agent: {label} failed ({e})"),
            };
        }
        self.mcp.push_log(label, &result);
        result
    }

    /// The Allow / Deny prompt for a write that the settings say must be confirmed, and
    /// any OAuth sign-in waiting for the user.
    pub fn confirm_window(&mut self, ctx: &egui::Context) {
        self.oauth_consent_window(ctx);
        let Some(p) = self.mcp.pending_confirm.as_ref() else {
            return;
        };
        let reason = p.reason.clone();
        let label = p.cmd.label();
        let waited = p.at.elapsed().as_secs();
        let ticket = p.ticket.clone();
        let left = self
            .mcp
            .approvals
            .get(&ticket)
            .map(|t| t.expires.saturating_duration_since(Instant::now()).as_secs())
            .unwrap_or(0);
        let queued = self.mcp.deferred.len();
        let mut decision: Option<bool> = None;
        egui::Window::new("Agent request")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_min_width(360.0);
                ui.label(egui::RichText::new(format!("The agent wants to {reason}.")).strong());
                ui.label(
                    egui::RichText::new(format!(
                        "Tool: {label}, ticket {ticket}. Waiting {waited}s; expires unapplied in {left}s.{} Agent \u{25b8} settings chooses which actions ask.",
                        if queued > 0 { format!(" {queued} more waiting after this.") } else { String::new() }
                    ))
                    .small()
                    .color(egui::Color32::from_gray(110)),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Allow").clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Deny").clicked() {
                        decision = Some(false);
                    }
                });
            });
        match decision {
            Some(allow) => self.resolve_confirm(allow),
            None => ctx.request_repaint_after(std::time::Duration::from_millis(250)),
        }
    }

    /// "Allow <client> to edit this project?" for each OAuth sign-in waiting for the
    /// user. The browser page shows the same code, so the user can tell their own
    /// sign-in from one somebody else started.
    fn oauth_consent_window(&mut self, ctx: &egui::Context) {
        let waiting = self.mcp.oauth.pending_consents();
        let Some(c) = waiting.first() else {
            return;
        };
        let mut decision: Option<bool> = None;
        egui::Window::new("Sign-in request")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -60.0])
            .show(ctx, |ui| {
                ui.set_min_width(380.0);
                ui.label(
                    egui::RichText::new(format!("Allow {} to edit this project?", c.client_name)).strong(),
                );
                ui.label(format!(
                    "It signs in through {} and gets the same access as the agent token: it can read and change the diagram, save, open and export.",
                    c.redirect_host
                ));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("The browser page should show");
                    ui.label(egui::RichText::new(&c.verify).monospace().strong().size(18.0));
                });
                ui.label(
                    egui::RichText::new(format!(
                        "Deny if it does not, or if you did not just connect a client. Waiting {}s.{}",
                        c.waiting_s,
                        if waiting.len() > 1 { format!(" {} more waiting.", waiting.len() - 1) } else { String::new() }
                    ))
                    .small()
                    .color(egui::Color32::from_gray(110)),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Allow").clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Deny").clicked() {
                        decision = Some(false);
                    }
                });
            });
        match decision {
            Some(allow) => {
                self.mcp.oauth.decide(&c.id, allow);
                self.status = format!(
                    "Sign-in for {} {}",
                    c.client_name,
                    if allow { "allowed" } else { "denied" }
                );
            }
            None => ctx.request_repaint_after(std::time::Duration::from_millis(500)),
        }
    }

    /// 0..1 intensity of the agent flash on an entity, if any.
    pub fn agent_flash(&self, id: &str) -> Option<f32> {
        let now = Instant::now();
        self.mcp
            .flash
            .iter()
            .filter(|(i, _)| i == id)
            .map(|(_, t)| 1.0 - now.duration_since(*t).as_secs_f32() / 1.5)
            .find(|f| *f > 0.0)
    }

    pub fn mcp_persist(&self, storage: &mut dyn eframe::Storage) {
        let s = &self.mcp.settings;
        storage.set_string("mcp_autostart", s.autostart.to_string());
        storage.set_string("mcp_port", s.port.to_string());
        storage.set_string("mcp_token", s.token.clone());
        storage.set_string("mcp_confirm_disk", s.confirm_disk.to_string());
        storage.set_string("mcp_confirm_delete", s.confirm_delete.to_string());
        storage.set_string("mcp_bind", s.bind.clone());
        storage.set_string("mcp_public_url", s.public_url.clone());
        storage.set_string("mcp_oauth", s.oauth.to_string());
        storage.set_string("mcp_approval_ttl", s.approval_ttl_secs.to_string());
        // Hashes only: registered clients and the grants users allowed.
        storage.set_string("mcp_oauth_grants", self.mcp.oauth.to_json());
    }

    pub fn mcp_restore(&mut self, storage: &dyn eframe::Storage) {
        if let Some(v) = storage.get_string("mcp_autostart") {
            self.mcp.settings.autostart = v == "true";
        }
        if let Some(p) = storage.get_string("mcp_port").and_then(|v| v.parse().ok()) {
            self.mcp.settings.port = p;
        }
        if let Some(t) = storage.get_string("mcp_token").filter(|t| !t.is_empty()) {
            self.mcp.settings.token = t;
        }
        if let Some(v) = storage.get_string("mcp_confirm_disk") {
            self.mcp.settings.confirm_disk = v == "true";
        }
        if let Some(v) = storage.get_string("mcp_confirm_delete") {
            self.mcp.settings.confirm_delete = v == "true";
        }
        if let Some(v) = storage.get_string("mcp_bind").filter(|v| !v.is_empty()) {
            self.mcp.settings.bind = v;
        }
        if let Some(v) = storage.get_string("mcp_public_url") {
            self.mcp.settings.public_url = v;
        }
        if let Some(v) = storage.get_string("mcp_oauth") {
            self.mcp.settings.oauth = v == "true";
        }
        if let Some(v) = storage
            .get_string("mcp_approval_ttl")
            .and_then(|v| v.parse().ok())
        {
            self.mcp.settings.approval_ttl_secs = v;
        }
        if let Some(v) = storage.get_string("mcp_oauth_grants") {
            self.mcp.oauth.load_json(&v);
        }
    }
}

/// Smallest and largest window a screenshot may ask for. The window manager may clamp
/// further — a display smaller than the request — which is why the reply reports both
/// what was asked for and the size actually captured.
pub const SHOT_MIN: f32 = 320.0;
pub const SHOT_MAX: f32 = 4096.0;

/// The inner size a screenshot asked for, clamped to something a window manager will
/// accept; `None` when it gave neither dimension and the window keeps its size. A
/// missing dimension keeps the current one, so `{ width: 1920 }` only widens.
fn screenshot_size(current: egui::Vec2, width: Option<u32>, height: Option<u32>) -> Option<egui::Vec2> {
    if width.is_none() && height.is_none() {
        return None;
    }
    let pick = |v: Option<u32>, now: f32| v.map(|v| v as f32).unwrap_or(now).clamp(SHOT_MIN, SHOT_MAX);
    Some(egui::Vec2::new(pick(width, current.x), pick(height, current.y)))
}

fn encode_png(img: &egui::ColorImage) -> Result<Vec<u8>, image::ImageError> {
    use image::ImageEncoder;
    let [w, h] = img.size;
    let raw: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out).write_image(
        &raw,
        w as u32,
        h as u32,
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::TtgApp;

    type Reply = ReplyWait;

    fn push(app: &TtgApp, cmd: AgentCommand) -> Reply {
        let (tx, rx) = AgentReply::channel();
        app.mcp.tx.send((cmd, tx)).unwrap();
        rx
    }

    /// The ticket a parked write answered with.
    fn ticket_of(rx: &mut Reply) -> String {
        let v = rx.try_recv().expect("answered at once").expect("ok");
        assert_eq!(v["status"], "pending_approval", "{v}");
        assert_eq!(v["applied"], false, "{v}");
        v["ticket"].as_str().expect("a ticket").to_string()
    }

    fn add(name: &str) -> AgentCommand {
        AgentCommand::EntityAdd {
            type_id: "compute_instance".into(),
            name: Some(name.into()),
            parent: None,
            x: None,
            y: None,
            providers: None,
            verbose: None,
        }
    }

    /// A write that needs approval answers at once with a ticket, and the queue keeps
    /// moving: a read behind it is answered on the same drain.
    #[test]
    fn an_approval_gated_write_answers_at_once_with_a_ticket() {
        let ctx = egui::Context::default();
        let mut app = TtgApp::build(&ctx, None, None, None);
        app.mcp.settings.confirm_disk = true;

        let mut save_rx = push(
            &app,
            AgentCommand::ProjectSave {
                path: Some("out.ttg.json".into()),
            },
        );
        let mut summary_rx = push(&app, AgentCommand::ProjectSummary);

        app.drain_agent_commands(&ctx);

        let ticket = ticket_of(&mut save_rx);
        summary_rx
            .try_recv()
            .expect("summary answered")
            .expect("summary ok");
        match app.mcp.pending_confirm.as_ref() {
            Some(PendingConfirm {
                cmd: AgentCommand::ProjectSave { .. },
                ticket: t,
                ..
            }) => assert_eq!(t, &ticket),
            _ => panic!("expected a pending ProjectSave"),
        }

        // Resolve it exactly the way the Allow/Deny window does.
        app.resolve_confirm(false);
        let st = app.mcp.approvals.status_json(&ticket).unwrap();
        assert_eq!(st["status"], "denied", "{st}");
        assert_eq!(st["applied"], false);
        assert!(app.mcp.pending_confirm.is_none());
    }

    /// While a prompt is open, a write that needs no approval runs at once, and a second
    /// one that does gets its own ticket and the next prompt. An approved write that
    /// fails says so on its ticket.
    #[test]
    fn approval_gated_writes_are_prompted_in_order_and_others_run() {
        let ctx = egui::Context::default();
        let mut app = TtgApp::build(&ctx, None, None, None);
        app.mcp.settings.confirm_delete = true;

        let mut first_rx = push(
            &app,
            AgentCommand::EntityDelete {
                entities: vec!["first".into()],
            },
        );
        let mut second_rx = push(
            &app,
            AgentCommand::EntityDelete {
                entities: vec!["second".into()],
            },
        );
        let mut add_rx = push(&app, add("meanwhile"));

        app.drain_agent_commands(&ctx);

        let first = ticket_of(&mut first_rx);
        let second_reply = second_rx.try_recv().expect("answered").expect("ok");
        let second = second_reply["ticket"].as_str().unwrap().to_string();
        assert_eq!(second_reply["prompts_ahead"], 1, "{second_reply}");
        add_rx.try_recv().expect("the add ran").expect("ok");
        assert!(app.project.entities().iter().any(|e| e.name == "meanwhile"));
        assert_eq!(app.mcp.deferred.len(), 1);

        app.resolve_confirm(true); // Allow the first: there is no "first", so it fails.
        let st = app.mcp.approvals.status_json(&first).unwrap();
        assert_eq!(st["status"], "failed", "{st}");
        assert_eq!(st["applied"], false);
        assert!(app.mcp.deferred.is_empty(), "the deferred write was prompted");
        match app.mcp.pending_confirm.as_ref() {
            Some(p) => assert_eq!(p.ticket, second),
            None => panic!("expected the second delete to be pending"),
        }
        assert_eq!(
            app.mcp.approvals.status_json(&second).unwrap()["prompts_ahead"],
            0
        );

        // Allow the second against a real entity name now; it is applied with a result.
        if let Some(p) = app.mcp.pending_confirm.as_mut() {
            p.cmd = AgentCommand::EntityDelete {
                entities: vec!["meanwhile".into()],
            };
        }
        app.resolve_confirm(true);
        let st = app.mcp.approvals.status_json(&second).unwrap();
        assert_eq!(st["status"], "applied", "{st}");
        assert_eq!(st["applied"], true);
        assert!(st["result"].is_object(), "{st}");
        assert!(!app.project.entities().iter().any(|e| e.name == "meanwhile"));
        assert!(app.mcp.pending_confirm.is_none());
    }

    /// A ticket nobody answers expires: the command never runs, the prompt goes, and the
    /// next parked write gets its turn.
    #[test]
    fn an_unanswered_ticket_expires_unapplied() {
        let ctx = egui::Context::default();
        let mut app = TtgApp::build(&ctx, None, None, None);
        app.mcp.settings.confirm_delete = true;
        app.mcp.settings.approval_ttl_secs = 1;
        let mut rx = push(&app, add("victim"));
        app.drain_agent_commands(&ctx);
        rx.try_recv().unwrap().unwrap();
        let mut del_rx = push(
            &app,
            AgentCommand::EntityDelete {
                entities: vec!["victim".into()],
            },
        );
        app.drain_agent_commands(&ctx);
        let ticket = ticket_of(&mut del_rx);
        std::thread::sleep(Duration::from_millis(1100));
        app.drain_agent_commands(&ctx);
        let st = app.mcp.approvals.status_json(&ticket).unwrap();
        assert_eq!(st["status"], "expired", "{st}");
        assert_eq!(st["applied"], false);
        assert!(app.mcp.pending_confirm.is_none());
        assert!(app.project.entities().iter().any(|e| e.name == "victim"));
        // Allowing after the fact does nothing.
        app.resolve_confirm(true);
        assert!(app.project.entities().iter().any(|e| e.name == "victim"));
    }

    /// The server withdraws a call that timed out before the UI started it, and the UI
    /// then never runs it; once the UI has claimed a call it can no longer be withdrawn,
    /// so the server knows to wait for the answer instead of saying nothing happened.
    #[test]
    fn a_withdrawn_call_never_runs_and_a_claimed_one_cannot_be_withdrawn() {
        let ctx = egui::Context::default();
        let mut app = TtgApp::build(&ctx, None, None, None);
        let before = app.project.nodes.len();
        let withdrawn = push(&app, add("late"));
        assert!(withdrawn.withdraw());
        app.drain_agent_commands(&ctx);
        assert_eq!(app.project.nodes.len(), before);

        let (reply, wait) = AgentReply::channel();
        assert!(reply.claim());
        assert!(!wait.withdraw(), "claimed first: the call is running");
        assert!(reply.claim(), "claiming twice is fine");
    }

    /// R2.1: a command whose caller has gone — the tool call timed out, or the client
    /// disconnected — must never be applied. It used to sit in the channel and run
    /// once the UI unblocked, which is a second copy of whatever the agent retried.
    #[test]
    fn a_command_whose_caller_has_gone_is_dropped() {
        let ctx = egui::Context::default();
        let mut app = TtgApp::build(&ctx, None, None, None);
        app.mcp.settings.confirm_disk = false;
        app.mcp.settings.confirm_delete = false;
        let before = app.project.nodes.len();

        // One write nobody is waiting for any more, then one that is.
        let abandoned = push(
            &app,
            AgentCommand::EntityAdd {
                type_id: "compute_instance".into(),
                name: Some("ghost".into()),
                parent: None,
                x: None,
                y: None,
                providers: None,
                verbose: None,
            },
        );
        drop(abandoned);
        let mut live_rx = push(
            &app,
            AgentCommand::EntityAdd {
                type_id: "compute_instance".into(),
                name: Some("kept".into()),
                parent: None,
                x: None,
                y: None,
                providers: None,
                verbose: None,
            },
        );

        app.drain_agent_commands(&ctx);

        live_rx.try_recv().expect("live command answered").expect("ok");
        assert_eq!(
            app.project.nodes.len(),
            before + 1,
            "only the command with a caller was applied"
        );
        assert!(!app.project.entities().iter().any(|e| e.name == "ghost"));
        assert!(app
            .mcp
            .log
            .iter()
            .any(|l| !l.ok && l.summary == "dropped: caller gone"));
    }

    /// R2.1: the heartbeat tells a blocked UI from a merely idle one. Draining beats
    /// it; a long operation publishes what it is stuck on.
    #[test]
    fn the_heartbeat_reports_what_the_app_is_busy_with() {
        let ctx = egui::Context::default();
        let mut app = TtgApp::build(&ctx, None, None, None);
        let beat = app.mcp.heartbeat.clone();

        // Fresh out of a drain, nothing is stale.
        app.drain_agent_commands(&ctx);
        assert!(beat.stalled(Duration::from_secs(3)).is_none());
        // A long enough gap is, and it carries the reason the app published.
        assert!(beat.stalled(Duration::from_nanos(1)).is_some());
        let out = app.busy_while("a folder-picker dialog is open", || {
            let (since, what) = beat.since_drain();
            assert!(since < Duration::from_secs(3));
            assert_eq!(what.as_deref(), Some("a folder-picker dialog is open"));
            7
        });
        assert_eq!(out, 7);
        // It clears when the operation ends, and again on the next drain.
        assert_eq!(beat.since_drain().1, None);
        app.drain_agent_commands(&ctx);
        assert!(beat.stalled(Duration::from_secs(3)).is_none());
    }

    /// R3.21: a selector needs at least one real criterion (an empty glob is none), and
    /// a bulk update needs something to change.
    #[test]
    fn empty_selectors_and_changes_are_recognised() {
        assert!(Selector::default().is_empty());
        assert!(Selector {
            name_glob: Some(String::new()),
            ..Default::default()
        }
        .is_empty());
        for s in [
            Selector {
                types: vec!["subnet".into()],
                ..Default::default()
            },
            Selector {
                name_glob: Some("web*".into()),
                ..Default::default()
            },
            Selector {
                ids: vec!["a".into()],
                ..Default::default()
            },
        ] {
            assert!(!s.is_empty(), "{s:?}");
        }
        assert!(EntityChanges::default().is_empty());
        assert!(!EntityChanges {
            manual: Some(false),
            ..Default::default()
        }
        .is_empty());
    }

    /// R3.21: while `quiet` is set (a dry run, or a bulk write that may still be rolled
    /// back) a committed change neither moves the revision nor stamps the last change;
    /// once it is cleared the next one does.
    #[test]
    fn a_quiet_change_is_invisible_to_project_changes() {
        let ctx = egui::Context::default();
        let mut app = TtgApp::build(&ctx, None, None, None);
        let rev = app.mcp.revision;
        app.mcp.quiet = true;
        app.mcp.note_change();
        assert_eq!(app.mcp.revision, rev);
        assert!(app.mcp.last_change.is_none());
        app.mcp.quiet = false;
        app.mcp.note_change();
        assert_eq!(app.mcp.revision, rev + 1);
        assert!(app.mcp.last_change.is_some());
    }

    /// R3.20: the catalog fingerprint reaches the MCP state when the app is built, so
    /// `serverInfo` can quote it.
    #[test]
    fn the_app_hands_the_catalog_fingerprint_to_the_server() {
        let ctx = egui::Context::default();
        let app = TtgApp::build(&ctx, None, None, None);
        assert_eq!(app.mcp.catalog_hash.len(), 12);
        assert_eq!(app.mcp.catalog_hash, app.catalog.fingerprint);
    }

    /// A screenshot with a size asks for that window size and remembers what to put
    /// back; without one the window is left alone.
    #[test]
    fn a_screenshot_size_is_clamped_and_the_old_size_remembered() {
        let ctx = egui::Context::default();
        let now = egui::Vec2::new(970.0, 599.0);
        assert_eq!(screenshot_size(now, None, None), None);
        assert_eq!(
            screenshot_size(now, Some(1920), Some(1200)),
            Some(egui::Vec2::new(1920.0, 1200.0))
        );
        // A missing dimension keeps the current one; silly numbers are clamped.
        assert_eq!(
            screenshot_size(now, Some(99_999), None),
            Some(egui::Vec2::new(SHOT_MAX, 599.0))
        );
        assert_eq!(
            screenshot_size(now, Some(1), Some(1)),
            Some(egui::Vec2::new(SHOT_MIN, SHOT_MIN))
        );

        // Headless there is no window, and the refusal says so rather than hanging.
        let mut app = TtgApp::build(&ctx, None, None, None);
        app.mcp.headless = true;
        let mut rx = push(
            &app,
            AgentCommand::Screenshot {
                fit: true,
                view: None,
                hide_panels: true,
                width: Some(1920),
                height: Some(1200),
            },
        );
        app.drain_agent_commands(&ctx);
        let err = rx.try_recv().expect("answered").expect_err("no display");
        assert_eq!(err, "no display in --serve mode");
        assert!(app.mcp.screenshot_restore.is_none());
    }
}
