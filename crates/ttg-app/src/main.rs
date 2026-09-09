//! TerraTofu GUI — native desktop entry point.
//!
//! `terratofu-gui [project.ttg.json] [--definitions DIR] [--screenshot out.png]`
//! `terratofu-gui --serve [--port N] [--token T] [project.ttg.json]`
//!
//! `--screenshot` renders a few frames, writes a PNG of the window and exits. Used for
//! documentation and smoke tests in CI. `--serve` runs the MCP server without a window
//! (no screenshots, no approval prompts): the same app logic, driven only by agents.
//! Used by the CI test and for scripted editing.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod annotations;
mod app;
mod camera;
mod canvas;
mod clipboard;
mod display;
mod history;
mod inspector;
#[cfg(feature = "mcp")]
mod mcp;
mod menu;
mod palette;
mod schema_editor;
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
        } else {
            open = Some(PathBuf::from(a));
        }
    }

    if serve {
        #[cfg(feature = "mcp")]
        {
            serve_headless(open, defs, port, token);
        }
        #[cfg(not(feature = "mcp"))]
        {
            let _ = (port, token);
            eprintln!("this build has no MCP support (built without the `mcp` feature)");
            std::process::exit(2);
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([960.0, 600.0])
            .with_title("TerraTofu GUI"),
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

/// Run the MCP server without a window until the process is killed.
#[cfg(feature = "mcp")]
fn serve_headless(
    open: Option<PathBuf>,
    defs: Option<PathBuf>,
    port: Option<u16>,
    token: Option<String>,
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
    if let Some(e) = app.error.take() {
        eprintln!("{e}");
        std::process::exit(1);
    }
    if let Err(e) = app.mcp.start(ctx.clone()) {
        eprintln!("[mcp] failed to start: {e}");
        std::process::exit(1);
    }
    println!("[mcp] listening on {} (headless)", app.mcp.url());
    let _ = std::io::stdout().flush();
    loop {
        app.drain_agent_commands(&ctx);
        app.refresh_diagnostics();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
