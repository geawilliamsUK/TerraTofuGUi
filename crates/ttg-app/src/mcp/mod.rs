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
}

impl Default for McpSettings {
    fn default() -> Self {
        McpSettings {
            autostart: false,
            port: DEFAULT_PORT,
            token: new_token(),
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
    Undo,
    Redo,
}

impl AgentCommand {
    /// Short label for the activity log.
    pub fn label(&self) -> String {
        let s = format!("{self:?}");
        s.split([' ', '{']).next().unwrap_or("?").to_string()
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
        )
    }
}

pub type AgentReply = tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>;

/// A running server.
pub struct Running {
    cancel: tokio_util::sync::CancellationToken,
}

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
        let (started_tx, started_rx) = mpsc::channel::<Result<tokio_util::sync::CancellationToken, String>>();
        std::thread::Builder::new()
            .name("ttg-mcp".into())
            .spawn(move || server::run(port, token, tx, ctx, started_tx))
            .map_err(|e| e.to_string())?;
        match started_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(cancel)) => {
                self.running = Some(Running { cancel });
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
            let next = self.mcp.rx.try_recv();
            let Ok((cmd, reply)) = next else { break };
            if matches!(cmd, AgentCommand::Screenshot) {
                self.mcp.pending_screenshots.push(reply);
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
                self.mcp.push_log(
                    "Screenshot".into(),
                    &Ok(serde_json::json!({"status": "requested"})),
                );
                continue;
            }
            let label = cmd.label();
            let is_write = cmd.is_write();
            let result = self.agent_exec(cmd);
            if is_write {
                self.status = match &result {
                    Ok(_) => format!("Agent: {label}"),
                    Err(e) => format!("Agent: {label} failed ({e})"),
                };
            }
            self.mcp.push_log(label, &result);
            let _ = reply.send(result);
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
