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

pub mod exec;
pub mod server;

use crate::app::TtgApp;
use std::collections::VecDeque;
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
}

impl Default for McpSettings {
    fn default() -> Self {
        McpSettings {
            autostart: false,
            port: DEFAULT_PORT,
            token: new_token(),
            confirm_disk: true,
            confirm_delete: false,
        }
    }
}

pub fn new_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
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
    },
    SchemaSearch {
        provider: Option<String>,
        query: String,
    },
    SchemaShow {
        provider: Option<String>,
        resource: String,
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
    },
    /// What an export would change in an existing directory (nothing is written).
    ExportDiff {
        dir: String,
        provider: Option<String>,
    },
    /// Has the project changed since revision `since` (user or agent edits)?
    Changes {
        since: Option<u64>,
    },
    /// Several write commands as one undo step; any failure rolls all of them back.
    Batch(Vec<AgentCommand>),
    Undo,
    Redo,
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
        )
    }
}

/// Events pushed from the UI thread to the server runtime (fan-out to subscribed
/// clients as `notifications/resources/updated`).
#[derive(Debug, Clone)]
pub enum ServerEvent {
    ProjectChanged,
}

/// A write the agent asked for that waits for the user's Allow / Deny.
pub struct PendingConfirm {
    pub cmd: AgentCommand,
    pub reply: AgentReply,
    pub reason: String,
    pub at: Instant,
}

pub type AgentReply = tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>;

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
    /// A write waiting for the user's approval; reads keep running meanwhile, and any
    /// further writes queue up in `deferred` instead of jumping ahead of it.
    pub pending_confirm: Option<PendingConfirm>,
    /// Write commands that arrived while another write was already waiting for
    /// approval, in arrival order. Started one at a time as `pending_confirm` is
    /// resolved (Allow or Deny); one of these may itself need approval and become the
    /// new `pending_confirm`, still ahead of the rest of this queue.
    pub deferred: VecDeque<(AgentCommand, AgentReply)>,
    /// Bumped on every committed change (user or agent); `project_changes` reports it.
    pub revision: u64,
    pub last_change: Option<(Instant, &'static str)>,
    /// Set while an agent command runs, so `finish()` can attribute the change.
    pub in_agent: bool,
    /// `--serve` mode: no window, no screenshots, no confirmation prompts.
    pub headless: bool,
}

impl Default for McpState {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        McpState {
            settings: McpSettings::default(),
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
            revision: 0,
            last_change: None,
            in_agent: false,
            headless: false,
        }
    }
}

impl McpState {
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.settings.port)
    }

    /// The command that registers this server with Claude Code.
    pub fn claude_add_command(&self) -> String {
        format!(
            "claude mcp add --transport http terratofu {} --header \"Authorization: Bearer {}\"",
            self.url(),
            self.settings.token
        )
    }

    /// Start the server thread. Returns quickly; bind errors are reported.
    pub fn start(&mut self, ctx: egui::Context) -> Result<(), String> {
        if self.running.is_some() {
            return Ok(());
        }
        let port = self.settings.port;
        let token = self.settings.token.clone();
        let tx = self.tx.clone();
        let beat = self.heartbeat.clone();
        let (started_tx, started_rx) = mpsc::channel::<Result<Started, String>>();
        std::thread::Builder::new()
            .name("ttg-mcp".into())
            .spawn(move || server::run(port, token, tx, ctx, beat, started_tx))
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
    /// Apply queued agent commands on the UI thread (called once per frame). A pending
    /// approval prompt no longer parks the whole queue: reads keep answering while it is
    /// open, and further writes queue up behind it in `mcp.deferred` instead of jumping
    /// ahead.
    pub fn drain_agent_commands(&mut self, ctx: &egui::Context) {
        self.mcp.heartbeat.beat();
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
                // Reads must not wait behind an approval prompt.
                self.run_agent(cmd, reply);
                continue;
            }
            if self.mcp.pending_confirm.is_some() {
                // Another write is already waiting for the user; keep this one in order.
                self.mcp.deferred.push_back((cmd, reply));
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

    /// Run a write now, or park it as `pending_confirm` if the settings require
    /// approval (skipped entirely in headless `--serve` mode).
    fn offer_or_run(&mut self, cmd: AgentCommand, reply: AgentReply) {
        if !self.mcp.headless {
            if let Some(reason) = cmd.confirm_reason(&self.mcp.settings) {
                self.status = "Agent: waiting for your approval".into();
                self.mcp.pending_confirm = Some(PendingConfirm {
                    cmd,
                    reply,
                    reason,
                    at: Instant::now(),
                });
                return;
            }
        }
        self.run_agent(cmd, reply);
    }

    /// Resolve the pending approval prompt (Allow when `allow` is true, else Deny), the
    /// same decision `confirm_window` makes from the Allow/Deny buttons but callable
    /// without egui (used by the headless test). Then start the next deferred write in
    /// arrival order - it may itself need approval and become the new `pending_confirm`,
    /// still ahead of anything queued after it.
    pub fn resolve_confirm(&mut self, allow: bool) {
        let Some(p) = self.mcp.pending_confirm.take() else {
            return;
        };
        if p.reply.is_closed() {
            // The prompt outlived the call. Applying it now would land a second copy
            // beside whatever the agent retried, so it is dropped either way.
            self.status = format!("Agent: {} dropped (the caller had gone)", p.cmd.label());
            self.mcp
                .push_log(p.cmd.label(), &Err("dropped: caller gone".into()));
        } else if allow {
            self.run_agent(p.cmd, p.reply);
        } else {
            let result = Err("denied by the user".to_string());
            self.status = format!("Agent: {} denied", p.cmd.label());
            self.mcp.push_log(p.cmd.label(), &result);
            let _ = p.reply.send(result);
        }
        // Skip any deferred write whose caller has since given up: it waited behind the
        // prompt long enough to time out, and the agent will have retried it.
        while let Some((cmd, reply)) = self.mcp.deferred.pop_front() {
            if reply.is_closed() {
                self.mcp
                    .push_log(cmd.label(), &Err("dropped: caller gone".into()));
                continue;
            }
            self.offer_or_run(cmd, reply);
            break;
        }
    }

    /// Execute one agent command now, log it and answer the server.
    fn run_agent(&mut self, cmd: AgentCommand, reply: AgentReply) {
        let label = cmd.label();
        let is_write = cmd.is_write();
        self.mcp.in_agent = true;
        let result = self.agent_exec(cmd);
        self.mcp.in_agent = false;
        if is_write {
            self.status = match &result {
                Ok(_) => format!("Agent: {label}"),
                Err(e) => format!("Agent: {label} failed ({e})"),
            };
        }
        self.mcp.push_log(label, &result);
        let _ = reply.send(result);
    }

    /// The Allow / Deny prompt for a write that the settings say must be confirmed.
    pub fn confirm_window(&mut self, ctx: &egui::Context) {
        let Some(p) = self.mcp.pending_confirm.as_ref() else {
            return;
        };
        let reason = p.reason.clone();
        let label = p.cmd.label();
        let waited = p.at.elapsed().as_secs();
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
                        "Tool: {label}. Waiting {waited}s. Agent \u{25b8} settings chooses which actions ask."
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

    type Reply = tokio::sync::oneshot::Receiver<Result<serde_json::Value, String>>;

    fn push(app: &TtgApp, cmd: AgentCommand) -> Reply {
        let (tx, rx) = tokio::sync::oneshot::channel();
        app.mcp.tx.send((cmd, tx)).unwrap();
        rx
    }

    /// A pending approval prompt must not park the whole queue: a read queued behind a
    /// write that is waiting for Allow/Deny still gets answered on the same drain.
    #[test]
    fn reads_answer_while_a_write_waits_for_approval() {
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

        // The read behind the pending write was still answered immediately.
        summary_rx
            .try_recv()
            .expect("summary answered")
            .expect("summary ok");
        // The write is parked, not yet answered.
        assert!(save_rx.try_recv().is_err(), "save should still be pending");
        match app.mcp.pending_confirm.as_ref().map(|p| &p.cmd) {
            Some(AgentCommand::ProjectSave { .. }) => {}
            other => panic!("expected a pending ProjectSave, got {other:?}"),
        }

        // Resolve it exactly the way the Allow/Deny window does.
        app.resolve_confirm(false);
        let result = save_rx.try_recv().expect("save answered after being resolved");
        assert!(result.is_err(), "denied write should come back as an error");
        assert!(app.mcp.pending_confirm.is_none());
    }

    /// A second write that arrives while one is already waiting for approval queues in
    /// `deferred` instead of jumping ahead; resolving the first starts the next one,
    /// which may itself need approval and become the new `pending_confirm`.
    #[test]
    fn a_second_write_defers_behind_the_first_and_starts_when_resolved() {
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

        app.drain_agent_commands(&ctx);

        assert!(app.mcp.pending_confirm.is_some());
        assert_eq!(app.mcp.deferred.len(), 1);
        assert!(second_rx.try_recv().is_err(), "deferred write not answered yet");

        app.resolve_confirm(true); // Allow the first.
        let _ = first_rx.try_recv().expect("first answered");
        assert!(app.mcp.deferred.is_empty(), "the deferred write was started");
        match app.mcp.pending_confirm.as_ref().map(|p| &p.cmd) {
            Some(AgentCommand::EntityDelete { entities }) => {
                assert_eq!(entities, &vec!["second".to_string()])
            }
            other => panic!("expected the deferred EntityDelete to now be pending, got {other:?}"),
        }
        assert!(
            second_rx.try_recv().is_err(),
            "still waiting for its own approval"
        );

        app.resolve_confirm(false); // Deny the second.
        let _ = second_rx.try_recv().expect("second answered");
        assert!(app.mcp.pending_confirm.is_none());
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

    /// The same rule for a write that waited behind an approval prompt: by the time the
    /// user answers, the call may be long gone.
    #[test]
    fn a_deferred_write_whose_caller_has_gone_is_dropped() {
        let ctx = egui::Context::default();
        let mut app = TtgApp::build(&ctx, None, None, None);
        app.mcp.settings.confirm_delete = true;

        let mut first_rx = push(
            &app,
            AgentCommand::EntityDelete {
                entities: vec!["first".into()],
            },
        );
        let abandoned = push(
            &app,
            AgentCommand::EntityDelete {
                entities: vec!["second".into()],
            },
        );
        let mut third_rx = push(
            &app,
            AgentCommand::EntityDelete {
                entities: vec!["third".into()],
            },
        );
        app.drain_agent_commands(&ctx);
        assert_eq!(app.mcp.deferred.len(), 2);
        drop(abandoned);

        app.resolve_confirm(false); // Deny the first; the queue moves on.
        let _ = first_rx.try_recv().expect("first answered");
        assert!(app.mcp.deferred.is_empty(), "both deferred writes were taken");
        // The abandoned one was skipped, so the third is what now waits for approval.
        match app.mcp.pending_confirm.as_ref().map(|p| &p.cmd) {
            Some(AgentCommand::EntityDelete { entities }) => {
                assert_eq!(entities, &vec!["third".to_string()])
            }
            other => panic!("expected the third delete to be pending, got {other:?}"),
        }
        assert!(third_rx.try_recv().is_err(), "still waiting for its own approval");
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
