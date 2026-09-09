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
use std::sync::mpsc;
use std::time::Instant;
use ttg_core::Id;

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
    Screenshot,
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
    },
    EntityMove {
        entity: String,
        x: i32,
        y: i32,
    },
    EntityResize {
        entity: String,
        w: i32,
        h: i32,
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
    ViewSave {
        name: String,
    },
    ViewActivate {
        name: String,
    },
    GroupAdd {
        label: String,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        color: Option<String>,
    },
    FlowAdd {
        from: String,
        to: String,
        label: String,
        dashed: bool,
    },
    AnnotationRemove {
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
            AgentCommand::AnnotationRemove { key } if s.confirm_delete => {
                Some(format!("remove the annotation {key}"))
            }
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
                | AgentCommand::Screenshot
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
    pub log: Vec<LogEntry>,
    /// Entities changed by the agent, flashed on the canvas for a moment.
    pub flash: Vec<(Id, Instant)>,
    pub last_error: Option<String>,
    pub show_window: bool,
    /// A write waiting for the user's approval; nothing else is executed meanwhile.
    pub pending_confirm: Option<PendingConfirm>,
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
            log: Vec::new(),
            flash: Vec::new(),
            last_error: None,
            show_window: false,
            pending_confirm: None,
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
        let (started_tx, started_rx) = mpsc::channel::<Result<Started, String>>();
        std::thread::Builder::new()
            .name("ttg-mcp".into())
            .spawn(move || server::run(port, token, tx, ctx, started_tx))
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
    /// Apply queued agent commands on the UI thread (called once per frame).
    pub fn drain_agent_commands(&mut self, ctx: &egui::Context) {
        loop {
            if self.mcp.pending_confirm.is_some() {
                break; // the user has not answered yet; keep the queue in order
            }
            let next = self.mcp.rx.try_recv();
            let Ok((cmd, reply)) = next else { break };
            if matches!(cmd, AgentCommand::Screenshot) {
                if self.mcp.headless {
                    let _ = reply.send(Err("no display in --serve mode".into()));
                    continue;
                }
                self.mcp.pending_screenshots.push(reply);
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
                self.mcp.push_log(
                    "Screenshot".into(),
                    &Ok(serde_json::json!({"status": "requested"})),
                );
                continue;
            }
            if !self.mcp.headless {
                if let Some(reason) = cmd.confirm_reason(&self.mcp.settings) {
                    self.status = "Agent: waiting for your approval".into();
                    self.mcp.pending_confirm = Some(PendingConfirm {
                        cmd,
                        reply,
                        reason,
                        at: Instant::now(),
                    });
                    break;
                }
            }
            self.run_agent(cmd, reply);
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
                let result = encode_png(&img)
                    .map(|png| {
                        use base64::Engine;
                        serde_json::json!({
                            "mime": "image/png",
                            "width": img.size[0],
                            "height": img.size[1],
                            "data": base64::engine::general_purpose::STANDARD.encode(png),
                        })
                    })
                    .map_err(|e| e.to_string());
                for r in self.mcp.pending_screenshots.drain(..) {
                    let _ = r.send(result.clone());
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
            Some(true) => {
                let p = self.mcp.pending_confirm.take().unwrap();
                self.run_agent(p.cmd, p.reply);
            }
            Some(false) => {
                let p = self.mcp.pending_confirm.take().unwrap();
                let result = Err("denied by the user".to_string());
                self.status = format!("Agent: {} denied", p.cmd.label());
                self.mcp.push_log(p.cmd.label(), &result);
                let _ = p.reply.send(result);
            }
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
