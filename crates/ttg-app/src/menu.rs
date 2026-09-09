//! Top bar: file / edit / view menus, tool + provider selectors, export actions.

use crate::app::{EdgeStyle, PendingAction, TtgApp};
use egui::{Color32, RichText, Ui};
use ttg_core::Tool;

pub fn show(app: &mut TtgApp, ui: &mut Ui) {
    egui::MenuBar::new().ui(ui, |ui| {
        ui.menu_button("File", |ui| {
            if ui.button("New            Ctrl+N").clicked() {
                app.request(PendingAction::New);
                ui.close();
            }
            if ui.button("Open…        Ctrl+O").clicked() {
                app.request(PendingAction::Open);
                ui.close();
            }
            ui.add_enabled_ui(!app.recent.is_empty(), |ui| {
                ui.menu_button("Open recent", |ui| {
                    let recent = app.recent.clone();
                    for p in &recent {
                        let label = p
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| p.display().to_string());
                        if ui
                            .button(label)
                            .on_hover_text(p.display().to_string())
                            .clicked()
                        {
                            app.request(PendingAction::OpenPath(p.clone()));
                            ui.close();
                        }
                    }
                    ui.separator();
                    if ui.button("Clear list").clicked() {
                        app.recent.clear();
                        ui.close();
                    }
                });
            });
            if ui.button("Save            Ctrl+S").clicked() {
                app.save();
                ui.close();
            }
            if ui.button("Save as…").clicked() {
                app.save_as();
                ui.close();
            }
            ui.separator();
            if ui.button("Project settings…").clicked() {
                app.show_settings = true;
                ui.close();
            }
            ui.separator();
            if ui.button("Quit").clicked() {
                app.request(PendingAction::Quit);
                ui.close();
            }
        });
        ui.menu_button("Edit", |ui| {
            if ui.add_enabled(app.history.can_undo(), egui::Button::new("Undo          Ctrl+Z")).clicked() {
                app.undo();
                ui.close();
            }
            if ui.add_enabled(app.history.can_redo(), egui::Button::new("Redo          Ctrl+Y")).clicked() {
                app.redo();
                ui.close();
            }
            ui.separator();
            let has_sel = !app.selection.is_empty();
            if ui.add_enabled(has_sel, egui::Button::new("Copy          Ctrl+C")).clicked() {
                app.copy_selection(ui.ctx());
                ui.close();
            }
            if ui.add_enabled(app.clip.is_some(), egui::Button::new("Paste         Ctrl+V")).clicked() {
                app.paste(None);
                ui.close();
            }
            if ui.add_enabled(has_sel || app.selected_edge.is_some(), egui::Button::new("Delete        Del")).clicked() {
                app.delete_selection();
                ui.close();
            }
            if ui.button("Select all    Ctrl+A").clicked() {
                app.select_all();
                ui.close();
            }
            ui.separator();
            ui.menu_button("Arrange", |ui| {
                use ttg_core::layout::Align;
                if ui
                    .button("Tidy layout    Ctrl+L")
                    .on_hover_text("Lay the whole diagram out in columns (sources left, what they depend on to the right) and fit containers to their contents")
                    .clicked()
                {
                    app.tidy(None);
                    ui.close();
                }
                ui.separator();
                let two = app.selection.len() >= 2;
                let three = app.selection.len() >= 3;
                for (label, how) in [
                    ("Align left", Align::Left),
                    ("Align horizontal centres", Align::HCenter),
                    ("Align right", Align::Right),
                    ("Align top", Align::Top),
                    ("Align vertical centres", Align::VCenter),
                    ("Align bottom", Align::Bottom),
                ] {
                    if ui.add_enabled(two, egui::Button::new(label)).clicked() {
                        app.align_selection(how);
                        ui.close();
                    }
                }
                ui.separator();
                if ui.add_enabled(three, egui::Button::new("Distribute horizontally")).clicked() {
                    app.distribute_selection(true);
                    ui.close();
                }
                if ui.add_enabled(three, egui::Button::new("Distribute vertically")).clicked() {
                    app.distribute_selection(false);
                    ui.close();
                }
            });
        });
        ui.menu_button("View", |ui| {
            if ui.button("Zoom to fit   Ctrl+0").clicked() {
                app.fit_requested = true;
                ui.close();
            }
            if ui.button("Reset zoom").clicked() {
                app.camera = crate::camera::Camera::default();
                ui.close();
            }
            ui.checkbox(&mut app.show_diagnostics, "Diagnostics panel");
            ui.checkbox(&mut app.reach_mode, "Reachability overlay   R");
            ui.separator();
            ui.label("Show as");
            ui.radio_value(&mut app.display, crate::display::DisplayMode::Abstract, "Abstract types");
            ui.radio_value(
                &mut app.display,
                crate::display::DisplayMode::Concrete,
                "Concrete provider resources   P",
            );
            ui.separator();
            ui.label("Edge style");
            ui.radio_value(&mut app.edge_style, EdgeStyle::Curved, "Curved");
            ui.radio_value(&mut app.edge_style, EdgeStyle::Orthogonal, "Orthogonal");
            ui.add_enabled(
                app.edge_style == EdgeStyle::Orthogonal,
                egui::Checkbox::new(&mut app.avoid_obstacles, "Route around nodes"),
            );
        });
        #[cfg(feature = "mcp")]
        ui.menu_button("Agent", |ui| {
            let running = app.mcp.is_running();
            let label = if running {
                format!("MCP server on (port {})", app.mcp.settings.port)
            } else {
                "MCP server off".to_string()
            };
            let mut on = running;
            if ui
                .checkbox(&mut on, label)
                .on_hover_text("Let an agent (Claude Code, any MCP client) read and edit this diagram live over localhost. Nothing runs while this is off.")
                .changed()
            {
                toggle_mcp(app, ui.ctx(), on);
            }
            if ui.button("Settings & activity…").clicked() {
                app.mcp.show_window = true;
                ui.close();
            }
        });
        ui.menu_button("Help", |ui| {
            if ui.button("About").clicked() {
                app.show_about = true;
                ui.close();
            }
        });

        ui.separator();

        // Tool toggle
        ui.label("Tool:");
        let mut tool = app.project.settings.tool;
        for t in Tool::ALL {
            ui.selectable_value(&mut tool, t, t.display_name());
        }
        app.set_tool(tool);

        ui.separator();

        // Target provider
        ui.label("Provider:");
        let providers: Vec<(String, String)> = app
            .catalog
            .providers
            .iter()
            .map(|(id, p)| (id.clone(), p.provider.display_name.clone()))
            .collect();
        let current = app.project.settings.target_provider.clone();
        let mut chosen = current.clone();
        for (id, name) in &providers {
            ui.selectable_value(&mut chosen, id.clone(), name);
        }
        if chosen != current {
            app.set_provider(&chosen);
        }

        ui.separator();

        ui.label("Show as:");
        let mut mode = app.display;
        ui.selectable_value(&mut mode, crate::display::DisplayMode::Abstract, "Abstract")
            .on_hover_text("Portable, provider-neutral types");
        let pname_short = app.provider_display_name();
        ui.selectable_value(&mut mode, crate::display::DisplayMode::Concrete, pname_short)
            .on_hover_text("Label every node with the resource it generates for the selected provider, and restrict the inspector to that provider (P)");
        app.display = mode;

        ui.separator();

        if ui
            .selectable_label(app.reach_mode, "Reachability")
            .on_hover_text("Select a resource to see what it can reach and how; with nothing selected, see what is exposed to the internet. (R)")
            .clicked()
        {
            app.reach_mode = !app.reach_mode;
        }

        ui.separator();

        let errors: Vec<String> = app.errors().iter().map(|d| d.message.clone()).collect();
        let blocked = !errors.is_empty();
        let tip = if blocked {
            format!(
                "Fix these first:\n{}",
                errors.iter().map(|m| format!("• {m}")).collect::<Vec<_>>().join("\n")
            )
        } else {
            String::new()
        };
        let pname = app
            .catalog
            .provider(&current)
            .map(|p| p.provider.display_name.clone())
            .unwrap_or(current.clone());
        if ui
            .add_enabled(!blocked, egui::Button::new(format!("Export {pname}…")))
            .on_disabled_hover_text(tip.clone())
            .on_hover_text(format!("Write a complete {} project for {pname} to a folder", app.project.settings.tool.display_name()))
            .clicked()
        {
            app.export_single();
        }
        if ui
            .button("Preview changes vs last export…")
            .on_hover_text("Diff a fresh generation against the folder of the last export, without writing")
            .clicked()
        {
            ui.close();
            app.preview_changes();
        }
        if ui
            .add_enabled(true, egui::Button::new("Export all providers…"))
            .on_hover_text("One complete, independent project directory per provider (providers with errors are skipped and reported)")
            .clicked()
        {
            app.export_all();
        }
        if blocked {
            ui.label(RichText::new(format!("{} error(s)", errors.len())).color(Color32::from_rgb(200, 40, 40)).small());
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if app.dirty {
                ui.label(RichText::new("unsaved").small().color(Color32::from_gray(120)));
            }
            #[cfg(feature = "mcp")]
            if app.mcp.is_running()
                && ui
                    .selectable_label(false, RichText::new(format!("● MCP :{}", app.mcp.settings.port)).small().color(Color32::from_rgb(40, 150, 80)))
                    .on_hover_text("MCP server listening; click for settings and the activity log")
                    .clicked()
            {
                app.mcp.show_window = true;
            }
            if let Some(p) = &app.path {
                ui.label(RichText::new(p.file_name().and_then(|s| s.to_str()).unwrap_or("")).small().color(Color32::from_gray(120)));
            }
        });
    });
}

#[cfg(feature = "mcp")]
fn toggle_mcp(app: &mut TtgApp, ctx: &egui::Context, on: bool) {
    if on {
        match app.mcp.start(ctx.clone()) {
            Ok(()) => app.status = format!("MCP server listening on {}", app.mcp.url()),
            Err(e) => {
                app.status = format!("MCP server failed: {e}");
                app.mcp.show_window = true;
            }
        }
    } else {
        app.mcp.stop();
        app.status = "MCP server stopped".into();
    }
}

/// Settings, connection details and the activity log for the MCP server.
#[cfg(feature = "mcp")]
pub fn mcp_window(app: &mut TtgApp, ui: &mut Ui) {
    let running = app.mcp.is_running();
    ui.horizontal(|ui| {
        let mut on = running;
        if ui.checkbox(&mut on, "Server enabled").changed() {
            toggle_mcp(app, ui.ctx(), on);
        }
        if running {
            ui.label(
                RichText::new(format!("listening on {}", app.mcp.url()))
                    .small()
                    .color(Color32::from_rgb(40, 150, 80)),
            );
        }
    });
    if let Some(e) = &app.mcp.last_error {
        ui.label(RichText::new(e).small().color(Color32::from_rgb(200, 40, 40)));
    }
    ui.checkbox(
        &mut app.mcp.settings.autostart,
        "Start the server when the app starts",
    )
    .on_hover_text("Off by default: the runtime only exists while the server is enabled.");
    ui.label(
        RichText::new("Ask before the agent…")
            .small()
            .color(Color32::from_gray(110)),
    );
    ui.checkbox(
        &mut app.mcp.settings.confirm_disk,
        "saves, opens, starts a new project or writes an export",
    );
    ui.checkbox(
        &mut app.mcp.settings.confirm_delete,
        "deletes resources, links or annotations",
    );
    ui.add_enabled_ui(!running, |ui| {
        ui.horizontal(|ui| {
            ui.label("Port");
            ui.add(egui::DragValue::new(&mut app.mcp.settings.port).range(1024..=65535));
            ui.label("Token");
            ui.label(RichText::new(&app.mcp.settings.token).monospace().small());
            if ui
                .small_button("regenerate")
                .on_hover_text("Clients must be re-added with the new token")
                .clicked()
            {
                app.mcp.settings.token = crate::mcp::new_token();
            }
        });
    });
    if running {
        ui.label(
            RichText::new("Stop the server to change the port or token.")
                .small()
                .color(Color32::from_gray(120)),
        );
    }
    ui.add_space(6.0);
    ui.label(RichText::new("Connect Claude Code").strong());
    let cmd = app.mcp.claude_add_command();
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::multiline(&mut cmd.clone())
                .desired_rows(2)
                .desired_width(f32::INFINITY)
                .font(egui::TextStyle::Monospace),
        );
    });
    if ui.button("Copy command").clicked() {
        ui.ctx().copy_text(cmd);
        app.status = "Copied the claude mcp add command".into();
    }
    ui.label(
        RichText::new("Run it once in the project you want to work from; the token is persistent. Only localhost can connect. Every change the agent makes is one undo step and flashes on the canvas; saving to disk only happens when a tool explicitly asks.")
            .small()
            .color(Color32::from_gray(110)),
    );
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Activity").strong());
        if ui.small_button("clear").clicked() {
            app.mcp.log.clear();
        }
    });
    egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
        if app.mcp.log.is_empty() {
            ui.label(
                RichText::new("No tool calls yet.")
                    .small()
                    .color(Color32::from_gray(120)),
            );
        }
        for e in app.mcp.log.iter().rev() {
            let age = e.at.elapsed().as_secs();
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("{age:>4}s ago"))
                        .small()
                        .monospace()
                        .color(Color32::from_gray(120)),
                );
                ui.label(RichText::new(&e.command).small().strong().color(if e.ok {
                    Color32::from_gray(40)
                } else {
                    Color32::from_rgb(200, 40, 40)
                }));
                ui.label(RichText::new(&e.summary).small().color(Color32::from_gray(90)));
            });
        }
    });
}
