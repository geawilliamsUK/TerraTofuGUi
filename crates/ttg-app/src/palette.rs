//! Left panel: searchable, category-grouped resource palette.

use crate::app::{PaletteItem, TtgApp, NODE_H, NODE_W};
use egui::{Color32, RichText, Ui, Vec2};
use ttg_catalog::{load::category_label, MappingStatus};

pub fn show(app: &mut TtgApp, ui: &mut Ui) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Resources").strong());
    });
    ui.add(
        egui::TextEdit::singleline(&mut app.palette_filter)
            .hint_text("Search…")
            .desired_width(f32::INFINITY),
    );
    ui.add_space(4.0);
    let filter = app.palette_filter.to_lowercase();
    let provider = app.project.settings.target_provider.clone();
    let concrete = app.concrete_mode();

    egui::ScrollArea::vertical().show(ui, |ui| {
        for cat in app.catalog.categories() {
            let items: Vec<(String, String, String, Option<MappingStatus>)> = app
                .catalog
                .resources
                .values()
                .filter(|r| r.resource.category == cat)
                .filter(|r| {
                    filter.is_empty()
                        || r.resource.display_name.to_lowercase().contains(&filter)
                        || r.resource.type_id.contains(&filter)
                        || r.providers
                            .get(&provider)
                            .is_some_and(|m| m.blocks.iter().any(|b| b.resource.contains(&filter)))
                })
                .map(|r| {
                    (
                        r.resource.type_id.clone(),
                        r.resource.display_name.clone(),
                        r.resource.icon.clone(),
                        r.providers.get(&provider).map(|m| m.status),
                    )
                })
                .collect();
            if items.is_empty() {
                continue;
            }
            egui::CollapsingHeader::new(category_label(&cat))
                .default_open(true)
                .show(ui, |ui| {
                    for (tid, name, icon, status) in items {
                        let id = ui.id().with(("pal", &tid));
                        let resp = ui
                            .dnd_drag_source(id, PaletteItem { type_id: tid.clone() }, |ui| {
                                egui::Frame::new()
                                    .fill(Color32::from_gray(252))
                                    .stroke(egui::Stroke::new(1.0, Color32::from_gray(215)))
                                    .corner_radius(4)
                                    .inner_margin(6)
                                    .show(ui, |ui| {
                                        ui.set_min_width(ui.available_width());
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new(&icon)
                                                    .monospace()
                                                    .small()
                                                    .color(Color32::from_gray(90)),
                                            );
                                            if concrete {
                                                ui.vertical(|ui| {
                                                    ui.label(&name);
                                                    ui.label(
                                                        RichText::new(app.type_subtitle(&tid))
                                                            .small()
                                                            .monospace()
                                                            .color(Color32::from_gray(120)),
                                                    );
                                                });
                                            } else {
                                                ui.label(&name);
                                            }
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    if concrete {
                                                        return;
                                                    }
                                                    if let Some(only) = app.provider_scope(&tid) {
                                                        let ok = status.is_some();
                                                        ui.label(
                                                            RichText::new(format!("{only} only"))
                                                                .small()
                                                                .color(if ok {
                                                                    Color32::from_gray(140)
                                                                } else {
                                                                    Color32::from_rgb(200, 90, 30)
                                                                }),
                                                        );
                                                        return;
                                                    }
                                                    let (txt, col) = match status {
                                                        Some(MappingStatus::Full) => {
                                                            ("", Color32::TRANSPARENT)
                                                        }
                                                        Some(MappingStatus::Partial) => {
                                                            ("partial", Color32::from_rgb(235, 160, 30))
                                                        }
                                                        Some(MappingStatus::Logical) => {
                                                            ("logical", Color32::from_gray(150))
                                                        }
                                                        None => {
                                                            ("no mapping", Color32::from_rgb(200, 90, 30))
                                                        }
                                                    };
                                                    if !txt.is_empty() {
                                                        ui.label(RichText::new(txt).small().color(col));
                                                    }
                                                },
                                            );
                                        });
                                    });
                            })
                            .response;
                        let resp = resp.on_hover_text(format!(
                            "{}\n\nDrag onto the canvas, or click to add at the centre.",
                            app.catalog
                                .resource(&tid)
                                .map(|d| d.resource.description.clone())
                                .unwrap_or_default()
                        ));
                        if resp.clicked() {
                            let center = app.canvas_rect.center();
                            let w = app.camera.to_world(app.canvas_rect.min, center)
                                - Vec2::new(NODE_W / 2.0, NODE_H / 2.0);
                            app.add_at = Some((tid.clone(), w));
                        }
                        ui.add_space(2.0);
                    }
                });
        }
    });
}
