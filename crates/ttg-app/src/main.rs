//! TerraTofu GUI — native desktop entry point.
//!
//! `terratofu-gui [project.ttg.json] [--definitions DIR] [--screenshot out.png]`
//! `terratofu-gui --serve [--port N] [--token T] [--bind ADDR] [--public-url URL]
//!                [--oauth] [--grants FILE] [project.ttg.json]`
//!
//! `--screenshot` renders a few frames, writes a PNG of the window and exits. Used for
//! documentation and smoke tests in CI. `--serve` runs the MCP server without a window
//! (no screenshots, no approval prompts): the same app logic, driven only by agents.
//! Used by the CI test and for scripted editing. `--public-url` is the HTTPS address a
//! tunnel publishes it under; `--oauth` serves OAuth sign-in for clients such as
//! claude.ai custom connectors, approving each sign-in with a one-time code printed on
//! stdout, and `--grants FILE` keeps what was allowed across restarts.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod annotations;
mod app;
mod camera;
mod canvas;
mod clipboard;
mod cost_panel;
mod display;
mod history;
mod inspector;
#[cfg(feature = "mcp")]
mod mcp;
mod menu;
mod palette;
mod schema_editor;
mod view_tools;
mod views;

use std::path::PathBuf;

fn main() -> eframe::Result {
    let mut args = std::env::args().skip(1);
    let mut open: Option<PathBuf> = None;
    let mut defs: Option<PathBuf> = None;
    let mut shot: Option<PathBuf> = None;
    let mut serve = false;
    let mut port: Option<u16> = None;
    let mut token: Option<String> = None;
    let mut remote = Remote::default();
    while let Some(a) = args.next() {
        if a == "--definitions" {
            defs = args.next().map(PathBuf::from);
        } else if a == "--screenshot" {
            shot = args.next().map(PathBuf::from);
        } else if a == "--serve" {
            serve = true;
        } else if a == "--port" {
            port = args.next().and_then(|p| p.parse().ok());
        } else if a == "--token" {
            token = args.next();
        } else if a == "--bind" {
            remote.bind = args.next();
        } else if a == "--public-url" {
            remote.public_url = args.next();
        } else if a == "--oauth" {
            remote.oauth = true;
        } else if a == "--grants" {
            remote.grants = args.next().map(PathBuf::from);
        } else {
            open = Some(PathBuf::from(a));
        }
    }

    if serve {
        #[cfg(feature = "mcp")]
        {
            serve_headless(open, defs, port, token, remote);
        }
        #[cfg(not(feature = "mcp"))]
        {
            let _ = (port, token, remote);
            eprintln!("this build has no MCP support (built without the `mcp` feature)");
            std::process::exit(2);
        }
    }

    let asked = app::window_size_env();
    let size = asked.unwrap_or([1440.0, 900.0]);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(size)
            // Never larger than the size asked for, or the window could not honour it.
            .with_min_inner_size([size[0].min(960.0), size[1].min(600.0)])
            .with_title("TerraTofu GUI"),
        // eframe otherwise restores (and then re-saves) the geometry of the last run,
        // which would both ignore TTG_WINDOW_SIZE and leave the user's window at
        // whatever size a documentation screenshot wanted.
        persist_window: asked.is_none(),
        ..Default::default()
    };
    eframe::run_native(
        "TerraTofu GUI",
        options,
        Box::new(move |cc| {
            let mut app = app::TtgApp::new(cc, open, defs);
            app.screenshot = shot;
            Ok(Box::new(app))
        }),
    )
}

/// `--serve` options for reaching the server from elsewhere.
#[derive(Default)]
#[cfg_attr(not(feature = "mcp"), allow(dead_code))]
struct Remote {
    bind: Option<String>,
    public_url: Option<String>,
    oauth: bool,
    grants: Option<PathBuf>,
}

/// Run the MCP server without a window until the process is killed.
#[cfg(feature = "mcp")]
fn serve_headless(
    open: Option<PathBuf>,
    defs: Option<PathBuf>,
    port: Option<u16>,
    token: Option<String>,
    remote: Remote,
) -> ! {
    use std::io::Write;
    let ctx = egui::Context::default();
    let mut app = app::TtgApp::build(&ctx, None, open, defs);
    app.mcp.headless = true;
    if let Some(p) = port {
        app.mcp.settings.port = p;
    }
    if let Some(t) = token {
        app.mcp.settings.token = t;
    }
    if let Some(b) = remote.bind {
        app.mcp.settings.bind = b;
    }
    if let Some(u) = remote.public_url {
        app.mcp.settings.public_url = u;
    }
    app.mcp.settings.oauth = remote.oauth;
    if let Some(g) = remote.grants {
        if let Err(e) = app.mcp.oauth.use_file(g) {
            eprintln!("[mcp] cannot read the grants file: {e}");
            std::process::exit(1);
        }
    }
    if let Some(e) = app.error.take() {
        eprintln!("{e}");
        std::process::exit(1);
    }
    if let Err(e) = app.mcp.start(ctx.clone()) {
        eprintln!("[mcp] failed to start: {e}");
        std::process::exit(1);
    }
    println!("[mcp] listening on {} (headless)", app.mcp.url());
    if let Some(u) = app.mcp.connector_url() {
        println!("[mcp] public URL: {u}");
    }
    if app.mcp.settings.oauth {
        println!("[mcp] OAuth sign-in is on: each sign-in prints a one-time code here to type into the browser page");
    }
    let _ = std::io::stdout().flush();
    loop {
        app.drain_agent_commands(&ctx);
        app.refresh_diagnostics();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
