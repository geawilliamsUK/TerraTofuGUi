//! TerraTofu GUI — native desktop entry point.
//!
//! `terratofu-gui [project.ttg.json] [--definitions DIR] [--screenshot out.png]`
//!
//! `--screenshot` renders a few frames, writes a PNG of the window and exits. Used for
//! documentation and smoke tests in CI.

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
mod views;

use std::path::PathBuf;

fn main() -> eframe::Result {
    let mut args = std::env::args().skip(1);
    let mut open: Option<PathBuf> = None;
    let mut defs: Option<PathBuf> = None;
    let mut shot: Option<PathBuf> = None;
    while let Some(a) = args.next() {
        if a == "--definitions" {
            defs = args.next().map(PathBuf::from);
        } else if a == "--screenshot" {
            shot = args.next().map(PathBuf::from);
        } else {
            open = Some(PathBuf::from(a));
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
