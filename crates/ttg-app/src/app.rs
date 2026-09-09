//! Application state and the top-level frame loop.

use crate::camera::Camera;
use crate::clipboard::{self, Clip};
use crate::history::History;
use egui::{Pos2, Rect, Vec2};
use std::collections::BTreeSet;
use std::path::PathBuf;
use ttg_catalog::{Catalog, ResourceKind};
use ttg_codegen::{Code, Diagnostic, Severity};
use ttg_core::{Container, Id, Node, Position, Project, Relation, Size, Tool};

pub const NODE_W: f32 = 176.0;
pub const NODE_H: f32 = 64.0;

/// Payload for palette -> canvas drag and drop.
#[derive(Debug, Clone)]
pub struct PaletteItem {
    pub type_id: String,
}

pub enum Drag {
    None,
    Move { before: Project, accum: Vec2 },
    Resize { before: Project, accum: Vec2 },
    Connect { from: Id },
    Marquee { start: Pos2, additive: bool },
}

/// How edges are drawn on the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeStyle {
    Curved,
    Orthogonal,
}

/// An action deferred until the user decides what to do with unsaved changes.
#[derive(Debug, Clone)]
pub enum PendingAction {
    New,
    Open,
    OpenPath(PathBuf),
    Quit,
}

const MAX_RECENT: usize = 10;

pub struct PendingEdge {
    pub source: Id,
    pub target: Id,
    pub relation: Relation,
    pub choices: Vec<Relation>,
}

#[derive(Default)]
pub struct ExportUi {
    pub open: bool,
    pub dir: Option<PathBuf>,
    pub results: Vec<(String, Result<ttg_codegen::ExportReport, String>)>,
    pub validate: Vec<(String, String)>,
}

pub struct TtgApp {
    pub catalog: Catalog,
    pub project: Project,
    pub path: Option<PathBuf>,
    pub dirty: bool,
    pub history: History,
    pub camera: Camera,
    pub selection: BTreeSet<Id>,
    pub selected_edge: Option<usize>,
    pub drag: Drag,
    pub clip: Option<Clip>,
    pub diagnostics: Vec<Diagnostic>,
    pub diag_dirty: bool,
    pub palette_filter: String,
    pub pending_edge: Option<PendingEdge>,
    pub export: ExportUi,
    pub show_settings: bool,
    pub show_diagnostics: bool,
    pub show_about: bool,
    pub status: String,
    pub error: Option<String>,
    pub edit_snapshot: Option<Project>,
    pub canvas_rect: Rect,
    pub fit_requested: bool,
    pub add_at: Option<(String, Pos2)>,
    /// `--screenshot` mode: write a PNG after a few frames and exit.
    pub screenshot: Option<PathBuf>,
    pub frame_no: u32,
    /// Most recently opened/saved project files, newest first. Persisted by eframe.
    pub recent: Vec<PathBuf>,
    pub edge_style: EdgeStyle,
    /// Action waiting on the "unsaved changes" prompt.
    pub confirm: Option<PendingAction>,
    /// Set once the user has decided to discard/save; lets the close request through.
    allow_close: bool,
    quit_requested: bool,
    /// (checked at, tofu found, terraform found)
    tool_cache: std::cell::RefCell<Option<(std::time::Instant, bool, bool)>>,
    /// Reachability overlay on the canvas.
    pub reach_mode: bool,
    /// Cached analysis for the current project revision and provider.
    pub reach: Option<ttg_codegen::reach::Reach>,
    /// Working canvas filter (what the view bar edits).
    pub filter: ttg_core::ViewFilter,
    /// Saved view the working filter was loaded from, if any.
    pub active_view: Option<usize>,
    /// Entities visible under `filter` this frame; `None` = everything.
    pub visible: Option<BTreeSet<Id>>,
    /// Entities on the target provider's layer, computed per frame in concrete display
    /// mode; `None` in abstract mode (nothing is dimmed).
    pub layer: Option<BTreeSet<Id>>,
    /// Name prompt for a new (`None`) or renamed (`Some(index)`) view.
    pub view_edit: Option<(Option<usize>, String)>,
    /// Selected group or flow annotation of the active view.
    pub selected_annotation: Option<crate::annotations::Annotation>,
    /// "Data flow from …" waiting for its target.
    pub flow_from: Option<ttg_core::FlowEnd>,
    /// Abstract or concrete (provider-specific) labelling of the canvas.
    pub display: crate::display::DisplayMode,
    pub icons: crate::display::Icons,
    /// Built-in MCP server (off until enabled).
    #[cfg(feature = "mcp")]
    pub mcp: crate::mcp::McpState,
}

impl TtgApp {
    pub fn new(cc: &eframe::CreationContext<'_>, open: Option<PathBuf>, defs: Option<PathBuf>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::light());
        let defs_dir = defs.clone();
        let (catalog, mut status) = match defs {
            Some(d) => match Catalog::load_dir(&d) {
                Ok(c) => (c, format!("Loaded definitions from {}", d.display())),
                Err(e) => (
                    Catalog::builtin(),
                    format!("Failed to load {}: {e}; using built-in catalog", d.display()),
                ),
            },
            None => (Catalog::builtin(), "Ready".to_string()),
        };
        let mut app = TtgApp {
            catalog,
            project: Project::new("untitled"),
            path: None,
            dirty: false,
            history: History::default(),
            camera: Camera::default(),
            selection: BTreeSet::new(),
            selected_edge: None,
            drag: Drag::None,
            clip: None,
            diagnostics: Vec::new(),
            diag_dirty: true,
            palette_filter: String::new(),
            pending_edge: None,
            export: ExportUi::default(),
            show_settings: false,
            show_diagnostics: true,
            show_about: false,
            status: String::new(),
            error: None,
            edit_snapshot: None,
            canvas_rect: Rect::NOTHING,
            fit_requested: false,
            add_at: None,
            screenshot: None,
            frame_no: 0,
            recent: Vec::new(),
            edge_style: EdgeStyle::Curved,
            confirm: None,
            allow_close: false,
            quit_requested: false,
            tool_cache: std::cell::RefCell::new(None),
            reach_mode: false,
            reach: None,
            filter: ttg_core::ViewFilter::default(),
            active_view: None,
            visible: None,
            layer: None,
            view_edit: None,
            selected_annotation: None,
            flow_from: None,
            display: crate::display::DisplayMode::Abstract,
            icons: crate::display::Icons::new(defs_dir.as_ref()),
            #[cfg(feature = "mcp")]
            mcp: crate::mcp::McpState::default(),
        };
        if let Some(storage) = cc.storage {
            if let Some(json) = storage.get_string("recent_files") {
                if let Ok(v) = serde_json::from_str::<Vec<PathBuf>>(&json) {
                    app.recent = v.into_iter().filter(|p| p.exists()).collect();
                }
            }
            if storage.get_string("edge_style").as_deref() == Some("orthogonal") {
                app.edge_style = EdgeStyle::Orthogonal;
            }
            if storage.get_string("display_mode").as_deref() == Some("concrete") {
                app.display = crate::display::DisplayMode::Concrete;
            }
            #[cfg(feature = "mcp")]
            app.mcp_restore(storage);
        }
        #[cfg(feature = "mcp")]
        {
            // `TTG_MCP=1` forces the server on for this run only (tests, screenshots); the
            // persisted "start with the app" setting is left alone. `TTG_MCP_AUTOSTART=off`
            // clears a stored autostart.
            let mut forced = false;
            if std::env::var("TTG_MCP").is_ok() {
                forced = true;
                if let Ok(t) = std::env::var("TTG_MCP_TOKEN") {
                    app.mcp.settings.token = t;
                }
                app.mcp.settings.port = std::env::var("TTG_MCP_PORT")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(crate::mcp::DEFAULT_PORT);
            }
            if std::env::var("TTG_MCP_AUTOSTART").as_deref() == Ok("off") {
                app.mcp.settings.autostart = false;
            }
            if app.mcp.settings.autostart || forced {
                match app.mcp.start(cc.egui_ctx.clone()) {
                    Err(e) => {
                        eprintln!("[mcp] failed to start: {e}");
                        status = format!("MCP server failed to start: {e}");
                    }
                    Ok(()) => {
                        eprintln!("[mcp] listening on {}", app.mcp.url());
                        status = format!("MCP server listening on {}", app.mcp.url());
                    }
                }
            }
        }
        match std::env::var("TTG_DISPLAY").as_deref() {
            Ok("concrete") => app.display = crate::display::DisplayMode::Concrete,
            Ok("abstract") => app.display = crate::display::DisplayMode::Abstract,
            _ => {}
        }
        // Override for documentation screenshots / testing.
        match std::env::var("TTG_EDGE_STYLE").as_deref() {
            Ok("orthogonal") => app.edge_style = EdgeStyle::Orthogonal,
            Ok("curved") => app.edge_style = EdgeStyle::Curved,
            _ => {}
        }
        app.ensure_provider_settings();
        if let Some(p) = open {
            match ttg_core::project::load(&p) {
                Ok(project) => {
                    app.project = project;
                    app.path = Some(p.clone());
                    app.fit_requested = true;
                    app.remember(p);
                    status = format!("Opened {}", app.path.as_ref().unwrap().display());
                }
                Err(e) => app.error = Some(format!("Could not open {}:\n{e}", p.display())),
            }
        }
        app.status = status;
        app
    }

    // ------------------------------------------------------------------ mutation helpers

    /// Take a snapshot before mutating; pass it to `finish` afterwards.
    pub fn snapshot(&self) -> Project {
        self.project.clone()
    }

    pub fn finish(&mut self, before: Project) {
        if before != self.project {
            self.history.push(before);
            self.dirty = true;
            self.diag_dirty = true;
        }
    }

    pub fn refresh_diagnostics(&mut self) {
        if self.diag_dirty {
            self.diagnostics = ttg_codegen::diagnostics::run(
                &self.project,
                &self.catalog,
                &self.project.settings.target_provider,
            );
            self.diag_dirty = false;
            self.reach = None;
        }
    }

    /// Reachability analysis for the current provider, computed on demand.
    pub fn reach(&mut self) -> &ttg_codegen::reach::Reach {
        if self.reach.is_none() {
            self.reach = Some(ttg_codegen::reach::analyse(
                &self.project,
                &self.catalog,
                &self.project.settings.target_provider,
            ));
        }
        self.reach.as_ref().unwrap()
    }

    /// Paths from the single selected entity, if the overlay is on and exactly one
    /// node is selected.
    pub fn reach_paths(&mut self) -> Vec<ttg_codegen::reach::Path> {
        if !self.reach_mode || self.selection.len() != 1 {
            return Vec::new();
        }
        let src = self.selection.iter().next().unwrap().clone();
        let reach = self.reach().clone();
        ttg_codegen::reach::paths_from(&self.project, &self.catalog, &reach, &src)
    }

    /// Who can reach the single selected entity (overlay on, one node selected).
    pub fn reach_incoming(&mut self) -> Vec<(Id, ttg_codegen::reach::Path)> {
        if !self.reach_mode || self.selection.len() != 1 {
            return Vec::new();
        }
        let tgt = self.selection.iter().next().unwrap().clone();
        let reach = self.reach().clone();
        ttg_codegen::reach::paths_to(&self.project, &self.catalog, &reach, &tgt)
    }

    /// Entities on a path (source, hops, target, and the source's way out of the
    /// network when the target is reached over the provider API).
    pub fn path_entities(&mut self, source: &str, path: &ttg_codegen::reach::Path) -> Vec<Id> {
        let mut ids = vec![source.to_string()];
        ids.extend(path.hops.iter().cloned());
        let reach = self.reach().clone();
        if let Some(tp) = reach.posture.get(&path.target) {
            if tp.managed {
                if let Some(ttg_codegen::reach::Egress::Via(h)) = reach.posture.get(source).map(|s| &s.egress)
                {
                    ids.extend(h.iter().cloned());
                }
            }
        }
        ids.push(path.target.clone());
        ids
    }

    pub fn errors(&self) -> Vec<&Diagnostic> {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect()
    }

    /// Worst diagnostic for an entity, for the badge.
    pub fn entity_badge(&self, id: &str) -> Option<&Diagnostic> {
        self.diagnostics
            .iter()
            .filter(|d| d.entity.as_deref() == Some(id))
            .max_by_key(|d| match (d.severity, d.code) {
                (Severity::Error, _) => 3,
                (Severity::Warning, _) => 2,
                (Severity::Info, Code::Manual) => 1,
                _ => 0,
            })
    }

    pub fn ensure_provider_settings(&mut self) {
        for (pid, pdef) in &self.catalog.providers {
            let m = self
                .project
                .settings
                .provider_settings
                .entry(pid.clone())
                .or_default();
            for v in &pdef.variables {
                m.entry(v.name.clone()).or_insert_with(|| {
                    v.default
                        .as_ref()
                        .and_then(|d| d.as_str().map(|s| s.to_string()))
                        .unwrap_or_default()
                });
            }
        }
    }

    // ------------------------------------------------------------------ geometry

    /// World rect of an entity as the active view sees it (view-owned positions and
    /// sizes override the shared ones).
    pub fn entity_rect(&self, id: &str) -> Option<Rect> {
        let layout = self.active_layout();
        if let Some(n) = self.project.nodes.get(id) {
            let pos = layout
                .and_then(|l| l.positions.get(id))
                .copied()
                .unwrap_or(n.position);
            let size = layout
                .and_then(|l| l.sizes.get(id))
                .copied()
                .or(n.size)
                .map(|s| Vec2::new(s.w as f32, s.h as f32))
                .unwrap_or(Vec2::new(NODE_W, NODE_H));
            return Some(Rect::from_min_size(Pos2::new(pos.x as f32, pos.y as f32), size));
        }
        self.project.containers.get(id).map(|c| {
            let pos = layout
                .and_then(|l| l.positions.get(id))
                .copied()
                .unwrap_or(c.position);
            let size = layout.and_then(|l| l.sizes.get(id)).copied().unwrap_or(c.size);
            Rect::from_min_size(
                Pos2::new(pos.x as f32, pos.y as f32),
                Vec2::new(size.w as f32, size.h as f32),
            )
        })
    }

    /// Deepest container whose rect contains `world`, excluding the given ids.
    pub fn container_at(&self, world: Pos2, exclude: &BTreeSet<Id>) -> Option<Id> {
        let mut best: Option<(usize, Id)> = None;
        for c in self.project.containers.values() {
            if exclude.contains(&c.id) {
                continue;
            }
            let r = self.entity_rect(&c.id).unwrap();
            if r.contains(world) {
                let depth = self.project.depth_of(&c.id);
                if best.as_ref().is_none_or(|(d, _)| depth > *d) {
                    best = Some((depth, c.id.clone()));
                }
            }
        }
        best.map(|(_, id)| id)
    }

    pub fn world_bounds(&self) -> Option<Rect> {
        let mut r: Option<Rect> = None;
        for e in self.project.entities() {
            if !self.is_visible(e.id) {
                continue;
            }
            let er = self.entity_rect(e.id).unwrap();
            r = Some(match r {
                Some(x) => x.union(er),
                None => er,
            });
        }
        r
    }

    // ------------------------------------------------------------------ entity ops

    fn unique_name(&self, base: &str) -> String {
        let taken: BTreeSet<String> = self
            .project
            .entities()
            .iter()
            .map(|e| ttg_core::slugify(e.name))
            .collect();
        if !taken.contains(&ttg_core::slugify(base)) {
            return base.to_string();
        }
        let mut n = 2;
        loop {
            let c = format!("{base} {n}");
            if !taken.contains(&ttg_core::slugify(&c)) {
                return c;
            }
            n += 1;
        }
    }

    /// Create an entity of the given abstract type at a world position, parenting it to
    /// the container under that position when the catalog allows it.
    pub fn add_entity(&mut self, type_id: &str, world: Pos2) -> Option<Id> {
        let def = self.catalog.resource(type_id)?.clone();
        let before = self.snapshot();
        let id = self.project.fresh_id(clipboard::prefix(type_id));
        let name = self.unique_name(&def.resource.display_name.to_lowercase());
        let parent = self.container_at(world, &BTreeSet::new()).filter(|p| {
            let pt = &self.project.containers[p].container_type;
            def.resource.allowed_parents.iter().any(|a| a == pt)
        });
        let mut config = ttg_core::Config::new();
        for f in &def.fields {
            if let Some(v) = f.default_value() {
                config.insert(f.name.clone(), v);
            }
        }
        let mut provider_config = std::collections::BTreeMap::new();
        for (pid, m) in &def.providers {
            let mut c = ttg_core::Config::new();
            for f in &m.fields {
                if let Some(v) = f.default_value() {
                    c.insert(f.name.clone(), v);
                }
            }
            provider_config.insert(pid.clone(), c);
        }
        let position = Position {
            x: world.x.round() as i32,
            y: world.y.round() as i32,
        };
        match def.resource.kind {
            ResourceKind::Node => {
                self.project.nodes.insert(
                    id.clone(),
                    Node {
                        id: id.clone(),
                        name,
                        resource_type: type_id.to_string(),
                        config,
                        provider_config,
                        position,
                        size: None,
                        parent,
                        manual: false,
                        providers: Vec::new(),
                    },
                );
            }
            ResourceKind::Container => {
                self.project.containers.insert(
                    id.clone(),
                    Container {
                        id: id.clone(),
                        name,
                        container_type: type_id.to_string(),
                        config,
                        provider_config,
                        position,
                        size: Size::default(),
                        parent,
                        manual: false,
                        providers: Vec::new(),
                    },
                );
            }
        }
        self.finish(before);
        self.reveal_new(&id);
        self.selection.clear();
        self.selection.insert(id.clone());
        self.selected_edge = None;
        Some(id)
    }

    pub fn delete_selection(&mut self) {
        if let Some(a) = self.selected_annotation.take() {
            let before = self.snapshot();
            if self.remove_annotation(&a) {
                self.finish(before);
            }
            return;
        }
        if let Some(i) = self.selected_edge.take() {
            if i < self.project.edges.len() {
                let before = self.snapshot();
                self.project.edges.remove(i);
                self.finish(before);
                return;
            }
        }
        if self.selection.is_empty() {
            return;
        }
        let before = self.snapshot();
        for id in self.selection.clone() {
            self.project.remove_entity(&id);
        }
        self.selection.clear();
        self.finish(before);
    }

    pub fn select_all(&mut self) {
        self.selection = self
            .project
            .entities()
            .iter()
            .map(|e| e.id.to_string())
            .filter(|id| self.is_visible(id))
            .collect();
    }

    pub fn copy_selection(&mut self, ctx: &egui::Context) {
        if let Some(clip) = clipboard::copy(&self.project, &self.selection) {
            if let Ok(json) = serde_json::to_string(&clip) {
                ctx.copy_text(json);
            }
            self.status = format!("Copied {} item(s)", clip.nodes.len() + clip.containers.len());
            self.clip = Some(clip);
        }
    }

    pub fn paste(&mut self, text: Option<String>) {
        let clip = text
            .and_then(|t| serde_json::from_str::<Clip>(&t).ok())
            .or_else(|| self.clip.clone());
        let Some(clip) = clip else { return };
        let before = self.snapshot();
        let ids = clipboard::paste(&mut self.project, &clip, (40, 40));
        self.finish(before);
        self.selection = ids.into_iter().collect();
        self.selected_edge = None;
        self.status = format!("Pasted {} item(s)", self.selection.len());
    }

    pub fn undo(&mut self) {
        if self.history.undo(&mut self.project) {
            self.after_history_jump();
        }
    }
    pub fn redo(&mut self) {
        if self.history.redo(&mut self.project) {
            self.after_history_jump();
        }
    }
    fn after_history_jump(&mut self) {
        self.dirty = true;
        self.diag_dirty = true;
        self.selection.retain(|id| self.project.contains(id));
        self.selected_edge = None;
    }

    /// Relations the catalog allows from `source` to `target`; `depends_on` is always allowed.
    pub fn relation_choices(&self, source: &str, target: &str) -> Vec<Relation> {
        let mut out = Vec::new();
        if let (Some(s), Some(t)) = (self.project.entity(source), self.project.entity(target)) {
            if let Some(def) = self.catalog.resource(s.resource_type) {
                for r in &def.relations {
                    if r.targets.iter().any(|x| x == t.resource_type) {
                        if let Some(k) = Relation::from_key(&r.kind) {
                            out.push(k);
                        }
                    }
                }
            }
        }
        out.push(Relation::DependsOn);
        out
    }

    pub fn request_edge(&mut self, source: Id, target: Id) {
        if source == target {
            return;
        }
        // Drawing a node inside a container already links them for `via_parent`
        // relations; do not add an explicit edge that would only be reported as redundant.
        if self.project.is_ancestor(&target, &source) {
            let implied = self
                .project
                .entity(&source)
                .and_then(|s| self.catalog.resource(s.resource_type))
                .is_some_and(|def| {
                    def.relations.iter().any(|r| {
                        r.via_parent
                            && self
                                .project
                                .entity(&target)
                                .is_some_and(|t| r.targets.iter().any(|x| x == t.resource_type))
                    })
                });
            if implied {
                self.status = format!(
                    "\"{}\" is already inside \"{}\"; no link needed",
                    self.project.entity(&source).map(|e| e.name).unwrap_or(""),
                    self.project.entity(&target).map(|e| e.name).unwrap_or("")
                );
                return;
            }
        }
        let choices = self.relation_choices(&source, &target);
        if choices.len() == 1 {
            let before = self.snapshot();
            self.project.add_edge(&source, &target, choices[0]);
            self.finish(before);
        } else {
            self.pending_edge = Some(PendingEdge {
                relation: choices[0],
                source,
                target,
                choices,
            });
        }
    }

    // ------------------------------------------------------------------ file ops

    fn remember(&mut self, p: PathBuf) {
        let p = std::fs::canonicalize(&p).unwrap_or(p);
        self.recent.retain(|x| x != &p);
        self.recent.insert(0, p);
        self.recent.truncate(MAX_RECENT);
    }

    /// Run an action now, or ask about unsaved changes first.
    pub fn request(&mut self, action: PendingAction) {
        if self.dirty {
            self.confirm = Some(action);
        } else {
            self.perform(action);
        }
    }

    pub fn perform(&mut self, action: PendingAction) {
        match action {
            PendingAction::New => self.new_project(),
            PendingAction::Open => self.open_dialog(),
            PendingAction::OpenPath(p) => self.open_path(p),
            PendingAction::Quit => {
                self.allow_close = true;
                self.quit_requested = true;
            }
        }
    }

    pub fn new_project(&mut self) {
        self.project = Project::new("untitled");
        self.ensure_provider_settings();
        self.path = None;
        self.dirty = false;
        self.history.clear();
        self.selection.clear();
        self.selected_edge = None;
        self.diag_dirty = true;
        self.camera = Camera::default();
        self.activate_view(None);
        self.status = "New project".into();
    }

    pub fn open_dialog(&mut self) {
        if let Some(p) = rfd::FileDialog::new()
            .add_filter("TerraTofu project", &["json"])
            .pick_file()
        {
            self.open_path(p);
        }
    }

    pub fn open_path(&mut self, p: PathBuf) {
        match ttg_core::project::load(&p) {
            Ok(project) => {
                self.project = project;
                self.ensure_provider_settings();
                self.path = Some(p.clone());
                self.dirty = false;
                self.history.clear();
                self.selection.clear();
                self.selected_edge = None;
                self.diag_dirty = true;
                self.fit_requested = true;
                self.activate_view(None);
                self.remember(p.clone());
                self.status = format!("Opened {}", p.display());
            }
            Err(e) => self.error = Some(format!("Could not open {}:\n{e}", p.display())),
        }
    }

    pub fn save(&mut self) {
        match self.path.clone() {
            Some(p) => self.save_to(p),
            None => self.save_as(),
        }
    }

    pub fn save_as(&mut self) {
        let mut d = rfd::FileDialog::new()
            .add_filter("TerraTofu project", &["json"])
            .set_file_name(format!("{}.ttg.json", ttg_core::slugify(&self.project.name)));
        if let Some(dir) = self.path.as_ref().and_then(|p| p.parent()) {
            d = d.set_directory(dir);
        }
        if let Some(p) = d.save_file() {
            self.save_to(p);
        }
    }

    pub fn save_to(&mut self, p: PathBuf) {
        match ttg_core::project::save(&self.project, &p) {
            Ok(()) => {
                self.path = Some(p.clone());
                self.dirty = false;
                self.remember(p.clone());
                self.status = format!("Saved {}", p.display());
            }
            Err(e) => self.error = Some(format!("Could not save {}:\n{e}", p.display())),
        }
    }

    // ------------------------------------------------------------------ export

    pub fn export_single(&mut self) {
        let provider = self.project.settings.target_provider.clone();
        let Some(dir) = rfd::FileDialog::new()
            .set_title(format!("Export {provider} project to folder"))
            .pick_folder()
        else {
            return;
        };
        let tool = self.project.settings.tool;
        let r = ttg_codegen::export(&self.project, &self.catalog, &provider, tool, &dir)
            .map_err(|e| e.to_string());
        self.export = ExportUi {
            open: true,
            dir: Some(dir),
            results: vec![(provider, r)],
            validate: Vec::new(),
        };
    }

    pub fn export_all(&mut self) {
        let Some(dir) = rfd::FileDialog::new()
            .set_title("Export one project per provider into folder")
            .pick_folder()
        else {
            return;
        };
        let tool = self.project.settings.tool;
        let results = ttg_codegen::export_all(&self.project, &self.catalog, tool, &dir);
        let _ = ttg_codegen::bundle::write_bundle_readme(&dir, &self.project.name, &results);
        self.export = ExportUi {
            open: true,
            dir: Some(dir),
            results: results
                .into_iter()
                .map(|(p, r)| (p, r.map_err(|e| e.to_string())))
                .collect(),
            validate: Vec::new(),
        };
    }

    pub fn run_validate(&mut self) {
        let tool = self.project.settings.tool;
        self.export.validate.clear();
        let mut outcomes = Vec::new();
        for (pid, r) in &self.export.results {
            if let Ok(rep) = r {
                let o = ttg_codegen::validate::run(&rep.out_dir, tool);
                outcomes.push((pid.clone(), o.summary()));
            }
        }
        self.export.validate = outcomes;
    }

    pub fn tool_binary_available(&self) -> bool {
        self.tool_found(self.project.settings.tool)
    }

    /// Cached PATH lookup of a tool binary (refreshed at most every few seconds).
    pub fn tool_found(&self, tool: Tool) -> bool {
        let mut cache = self.tool_cache.borrow_mut();
        let now = std::time::Instant::now();
        if cache
            .as_ref()
            .is_none_or(|(t, _, _)| now.duration_since(*t).as_secs() > 5)
        {
            *cache = Some((
                now,
                ttg_codegen::validate::find_binary(Tool::OpenTofu).is_some(),
                ttg_codegen::validate::find_binary(Tool::Terraform).is_some(),
            ));
        }
        let (_, tofu, tf) = cache.as_ref().unwrap();
        match tool {
            Tool::OpenTofu => *tofu,
            Tool::Terraform => *tf,
        }
    }

    /// Node size lookup for the layout helpers (containers report their own size).
    fn size_of(p: &Project, id: &str) -> Size {
        if let Some(c) = p.containers.get(id) {
            return c.size;
        }
        p.nodes.get(id).and_then(|n| n.size).unwrap_or(Size {
            w: NODE_W as i32,
            h: NODE_H as i32,
        })
    }

    /// Re-lay out the whole diagram (or one container's contents) and zoom to fit.
    pub fn tidy(&mut self, container: Option<&str>) {
        let before = self.snapshot();
        let opts = ttg_core::layout::TidyOptions::default();
        let container = container.map(|s| s.to_string());
        self.with_layout(|p| match container.as_deref() {
            Some(c) => ttg_core::layout::tidy_container(p, c, &Self::size_of, &opts),
            None => ttg_core::layout::tidy(p, &Self::size_of, &opts),
        });
        self.finish(before);
        self.fit_requested = true;
        self.status = "Tidied layout (Ctrl+Z to undo)".into();
    }

    pub fn align_selection(&mut self, how: ttg_core::layout::Align) {
        let ids: Vec<Id> = self.selection.iter().cloned().collect();
        let before = self.snapshot();
        let mut n = 0;
        self.with_layout(|p| n = ttg_core::layout::align(p, &ids, how, &Self::size_of));
        self.finish(before);
        self.status = format!("Aligned {n} item(s)");
    }

    pub fn distribute_selection(&mut self, horizontal: bool) {
        let ids: Vec<Id> = self.selection.iter().cloned().collect();
        let before = self.snapshot();
        let mut n = 0;
        self.with_layout(|p| n = ttg_core::layout::distribute(p, &ids, horizontal, &Self::size_of));
        self.finish(before);
        self.status = format!("Distributed {n} item(s)");
    }

    pub fn toggle_display(&mut self) {
        use crate::display::DisplayMode;
        self.display = match self.display {
            DisplayMode::Abstract => DisplayMode::Concrete,
            DisplayMode::Concrete => DisplayMode::Abstract,
        };
    }

    pub fn set_tool(&mut self, t: Tool) {
        if self.project.settings.tool != t {
            let before = self.snapshot();
            self.project.settings.tool = t;
            self.finish(before);
        }
    }

    pub fn set_provider(&mut self, p: &str) {
        if self.project.settings.target_provider != p {
            let before = self.snapshot();
            self.project.settings.target_provider = p.to_string();
            self.finish(before);
        }
    }

    // ------------------------------------------------------------------ keyboard

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        let mut paste_text: Option<String> = None;
        let mut copy = false;
        let (toggle_reach, toggle_display) = ctx.input_mut(|i| {
            (
                i.modifiers.is_none() && i.key_pressed(egui::Key::R),
                i.modifiers.is_none() && i.key_pressed(egui::Key::P),
            )
        });
        if toggle_reach {
            self.reach_mode = !self.reach_mode;
        }
        if toggle_display {
            self.toggle_display();
        }
        let tidy = ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::L));
        if tidy {
            self.tidy(None);
        }
        let (undo, redo, del, sel_all, save, open, new, escape, fit) = ctx.input_mut(|i| {
            let cmd = i.modifiers.command;
            let shift = i.modifiers.shift;
            for ev in &i.events {
                match ev {
                    egui::Event::Paste(t) => paste_text = Some(t.clone()),
                    egui::Event::Copy => copy = true,
                    _ => {}
                }
            }
            (
                cmd && !shift && i.consume_key(egui::Modifiers::COMMAND, egui::Key::Z),
                cmd && (i.consume_key(egui::Modifiers::COMMAND, egui::Key::Y)
                    || i.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::Z)),
                i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace),
                cmd && i.consume_key(egui::Modifiers::COMMAND, egui::Key::A),
                cmd && i.consume_key(egui::Modifiers::COMMAND, egui::Key::S),
                cmd && i.consume_key(egui::Modifiers::COMMAND, egui::Key::O),
                cmd && i.consume_key(egui::Modifiers::COMMAND, egui::Key::N),
                i.key_pressed(egui::Key::Escape),
                cmd && i.consume_key(egui::Modifiers::COMMAND, egui::Key::Num0),
            )
        });
        if undo {
            self.undo();
        }
        if redo {
            self.redo();
        }
        if del {
            self.delete_selection();
        }
        if sel_all {
            self.select_all();
        }
        if save {
            self.save();
        }
        if open {
            self.request(PendingAction::Open);
        }
        if new {
            self.request(PendingAction::New);
        }
        if fit {
            self.fit_requested = true;
        }
        if escape {
            self.selection.clear();
            self.selected_edge = None;
            self.selected_annotation = None;
            self.pending_edge = None;
            if self.flow_from.take().is_some() {
                self.status = "Data flow cancelled".into();
            }
            self.drag = Drag::None;
        }
        if copy {
            self.copy_selection(ctx);
        }
        if let Some(t) = paste_text {
            self.paste(Some(t));
        }
    }

    fn windows(&mut self, ctx: &egui::Context) {
        // Unsaved changes prompt
        if let Some(action) = self.confirm.clone() {
            let mut decision: Option<bool> = None; // Some(true)=save, Some(false)=discard
            let mut cancel = false;
            egui::Window::new("Unsaved changes")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!(
                        "\"{}\" has unsaved changes. Save them first?",
                        self.project.name
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() {
                            decision = Some(true);
                        }
                        if ui.button("Don't save").clicked() {
                            decision = Some(false);
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });
            if cancel {
                self.confirm = None;
            } else if let Some(save_first) = decision {
                if save_first {
                    self.save();
                }
                if !save_first || !self.dirty {
                    self.confirm = None;
                    self.dirty = false;
                    self.perform(action);
                }
            }
        }
        // Error dialog
        if let Some(msg) = self.error.clone() {
            let mut open = true;
            egui::Window::new("Error")
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label(msg);
                    if ui.button("OK").clicked() {
                        self.error = None;
                    }
                });
            if !open {
                self.error = None;
            }
        }
        // Pending edge relation picker
        if let Some(pe) = &mut self.pending_edge {
            let mut done: Option<bool> = None;
            let sname = self
                .project
                .entity(&pe.source)
                .map(|e| e.name.to_string())
                .unwrap_or_default();
            let tname = self
                .project
                .entity(&pe.target)
                .map(|e| e.name.to_string())
                .unwrap_or_default();
            egui::Window::new("Connect")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!("\"{sname}\"  →  \"{tname}\""));
                    ui.add_space(6.0);
                    for r in pe.choices.clone() {
                        ui.radio_value(&mut pe.relation, r, r.display_name());
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Connect").clicked() {
                            done = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            done = Some(false);
                        }
                    });
                });
            if let Some(ok) = done {
                let pe = self.pending_edge.take().unwrap();
                if ok {
                    let before = self.snapshot();
                    self.project.add_edge(&pe.source, &pe.target, pe.relation);
                    self.finish(before);
                }
            }
        }
        // Settings
        if self.show_settings {
            let mut open = true;
            egui::Window::new("Project settings")
                .open(&mut open)
                .default_width(420.0)
                .show(ctx, |ui| crate::inspector::settings_ui(self, ui));
            self.show_settings = open;
        }
        // Export results
        if self.export.open {
            let mut open = true;
            egui::Window::new("Export")
                .open(&mut open)
                .default_width(560.0)
                .show(ctx, |ui| crate::inspector::export_ui(self, ui));
            self.export.open = open;
        }
        #[cfg(feature = "mcp")]
        if self.mcp.show_window {
            let mut open = true;
            egui::Window::new("Agent (MCP server)")
                .open(&mut open)
                .default_width(520.0)
                .show(ctx, |ui| crate::menu::mcp_window(self, ui));
            self.mcp.show_window = open;
        }
        if self.show_about {
            let mut open = true;
            egui::Window::new("About TerraTofu GUI")
                .open(&mut open)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label("Visual cloud architecture → Terraform / OpenTofu generator.");
                    ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                    ui.label("Dual-licensed MIT / Apache-2.0.");
                    ui.add_space(4.0);
                    ui.label(format!(
                        "{} resource types, {} providers in the catalog.",
                        self.catalog.resources.len(),
                        self.catalog.providers.len()
                    ));
                });
            self.show_about = open;
        }
    }
}

impl TtgApp {
    fn screenshot_mode(&mut self, ctx: &egui::Context) {
        let Some(path) = self.screenshot.clone() else {
            return;
        };
        self.frame_no += 1;
        if self.frame_no == 3 {
            // Select something so the inspector is populated: TTG_SELECT_NAME=<name>,
            // TTG_SELECT=<resource type> (default compute_instance), or TTG_SELECT=none.
            let by_name = std::env::var("TTG_SELECT_NAME").ok();
            let by_type = std::env::var("TTG_SELECT").unwrap_or("compute_instance".into());
            let pick = self
                .project
                .entities()
                .iter()
                .find(|e| match &by_name {
                    Some(n) => e.name.eq_ignore_ascii_case(n),
                    None => by_type != "none" && e.resource_type == by_type,
                })
                .map(|e| e.id.to_string());
            if let Some(id) = pick {
                self.selection.insert(id);
            }
            if std::env::var("TTG_REACH").is_ok() {
                self.reach_mode = true;
            }
            if std::env::var("TTG_TIDY").is_ok() {
                self.tidy(None);
                // The layout change marks the project dirty; never block the exit on it.
                self.allow_close = true;
            }
            if let Ok(v) = std::env::var("TTG_VIEW") {
                if let Some(i) = self
                    .project
                    .views
                    .iter()
                    .position(|x| x.name.eq_ignore_ascii_case(&v))
                {
                    self.activate_view(Some(i));
                }
            }
            self.fit_requested = true;
        }
        if self.frame_no == 8 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        let image = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = image {
            let [w, h] = img.size;
            let raw: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
            let _ = image::save_buffer(&path, &raw, w as u32, h as u32, image::ColorType::Rgba8);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if self.frame_no > 60 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint();
    }
}

impl eframe::App for TtgApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.refresh_diagnostics();
        self.screenshot_mode(ctx);
        #[cfg(feature = "mcp")]
        self.drain_agent_commands(ctx);
        self.handle_shortcuts(ctx);

        // Window close button: intercept when there are unsaved changes.
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close {
            if self.dirty {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.confirm = Some(PendingAction::Quit);
            } else {
                self.allow_close = true;
            }
        }
        if self.quit_requested {
            self.quit_requested = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        #[cfg(feature = "mcp")]
        if self.allow_close && ctx.input(|i| i.viewport().close_requested()) {
            self.mcp.stop();
        }

        let title = format!(
            "{}{} — TerraTofu GUI",
            self.project.name,
            if self.dirty { " *" } else { "" }
        );
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));

        egui::TopBottomPanel::top("menu").show(ctx, |ui| crate::menu::show(self, ui));
        egui::TopBottomPanel::top("views").show(ctx, |ui| crate::views::bar(self, ui));
        self.refresh_visibility();

        egui::TopBottomPanel::bottom("status")
            .resizable(true)
            .default_height(if self.show_diagnostics { 140.0 } else { 24.0 })
            .show(ctx, |ui| crate::inspector::status_ui(self, ui));

        egui::SidePanel::left("palette")
            .default_width(230.0)
            .min_width(180.0)
            .show(ctx, |ui| crate::palette::show(self, ui));

        egui::SidePanel::right("inspector")
            .default_width(340.0)
            .min_width(260.0)
            .show(ctx, |ui| crate::inspector::show(self, ui));

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(egui::Color32::from_rgb(246, 247, 249)))
            .show(ctx, |ui| crate::canvas::show(self, ui));

        self.windows(ctx);
        self.refresh_diagnostics();
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if let Ok(json) = serde_json::to_string(&self.recent) {
            storage.set_string("recent_files", json);
        }
        #[cfg(feature = "mcp")]
        self.mcp_persist(storage);
        storage.set_string(
            "display_mode",
            match self.display {
                crate::display::DisplayMode::Abstract => "abstract",
                crate::display::DisplayMode::Concrete => "concrete",
            }
            .into(),
        );
        storage.set_string(
            "edge_style",
            match self.edge_style {
                EdgeStyle::Curved => "curved",
                EdgeStyle::Orthogonal => "orthogonal",
            }
            .into(),
        );
    }
}
