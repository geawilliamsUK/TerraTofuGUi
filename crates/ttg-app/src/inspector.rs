//! Right panel: typed property editors driven by the catalog, plus the settings, export
//! and status/diagnostics views.

use crate::app::TtgApp;
use egui::{Color32, RichText, Ui};
use ttg_catalog::{FieldDef, FieldType, MappingStatus};
use ttg_codegen::Severity;
use ttg_core::{Id, Tool, Value};

pub fn show(app: &mut TtgApp, ui: &mut Ui) {
    ui.add_space(6.0);
    if let Some(a) = app.selected_annotation.clone() {
        crate::annotations::annotation_inspector(app, ui, &a);
        return;
    }
    if let Some(i) = app.selected_edge {
        edge_inspector(app, ui, i);
        return;
    }
    match app.selection.len() {
        0 => {
            ui.label(RichText::new("Inspector").strong());
            ui.add_space(8.0);
            ui.label(
                RichText::new("Select a resource to edit its configuration.").color(Color32::from_gray(120)),
            );
            ui.add_space(12.0);
            ui.label(RichText::new("Project").strong());
            ui.horizontal(|ui| {
                ui.label("Name");
                let mut name = app.project.name.clone();
                let r = ui.text_edit_singleline(&mut name);
                track_text_edit(app, &r);
                if r.changed() {
                    app.project.name = name;
                    app.dirty = true;
                }
            });
            ui.label(format!(
                "{} resources, {} containers, {} edges",
                app.project.nodes.len(),
                app.project.containers.len(),
                app.project.edges.len()
            ));
            if ui.button("Project settings…").clicked() {
                app.show_settings = true;
            }
            layer_summary(app, ui);
            if app.reach_mode {
                ui.add_space(12.0);
                ui.label(RichText::new("Network posture").strong());
                let reach = app.reach().clone();
                let mut any = false;
                for (id, po) in &reach.posture {
                    let name = app
                        .project
                        .entity(id)
                        .map(|e| e.name.to_string())
                        .unwrap_or_default();
                    if let Some(how) = &po.exposed {
                        any = true;
                        ui.label(
                            RichText::new(format!("⚠ {name}: {how}"))
                                .small()
                                .color(Color32::from_rgb(200, 100, 20)),
                        );
                    }
                    if let ttg_codegen::reach::Egress::Blocked(why) = &po.egress {
                        any = true;
                        ui.label(
                            RichText::new(format!("✕ {name}: no outbound path ({why})"))
                                .small()
                                .color(Color32::from_rgb(200, 40, 40)),
                        );
                    }
                }
                if !any {
                    ui.label(
                        RichText::new("Nothing exposed; every resource that needs an outbound path has one.")
                            .small()
                            .color(Color32::from_gray(110)),
                    );
                }
                ui.label(
                    RichText::new("Select a resource to see what it can reach.")
                        .small()
                        .color(Color32::from_gray(110)),
                );
            }
        }
        1 => {
            let id = app.selection.iter().next().unwrap().clone();
            entity_inspector(app, ui, &id);
        }
        n => {
            ui.label(RichText::new(format!("{n} items selected")).strong());
            ui.add_space(6.0);
            if ui.button("Delete selection").clicked() {
                app.delete_selection();
            }
        }
    }
}

/// Snapshot the project when a text field gains focus and commit to history when it
/// loses focus, so one undo step covers a whole typing session.
fn track_text_edit(app: &mut TtgApp, r: &egui::Response) {
    if r.gained_focus() {
        app.edit_snapshot = Some(app.snapshot());
    }
    if r.lost_focus() {
        if let Some(before) = app.edit_snapshot.take() {
            app.finish(before);
        }
    }
    if r.changed() {
        app.diag_dirty = true;
        app.dirty = true;
    }
}

fn entity_inspector(app: &mut TtgApp, ui: &mut Ui, id: &Id) {
    let Some(e) = app.project.entity(id) else { return };
    let is_container = e.is_container;
    let type_id = e.resource_type.to_string();
    let Some(def) = app.catalog.resource(&type_id).cloned() else {
        ui.label(format!("Unknown type '{type_id}'"));
        return;
    };
    let provider = app.project.settings.target_provider.clone();

    let concrete = app.concrete_mode();
    ui.horizontal(|ui| {
        if concrete {
            let c = app.concrete(&type_id);
            let title = c
                .as_ref()
                .and_then(|c| c.primary.clone())
                .unwrap_or_else(|| def.resource.display_name.clone());
            ui.label(RichText::new(title).strong().size(16.0));
            ui.label(
                RichText::new(format!("({})", def.resource.display_name))
                    .small()
                    .color(Color32::from_gray(120)),
            );
        } else {
            ui.label(RichText::new(&def.resource.display_name).strong().size(16.0));
            ui.label(
                RichText::new(format!("({type_id})"))
                    .small()
                    .color(Color32::from_gray(120)),
            );
        }
    });
    if !def.resource.description.is_empty() {
        ui.label(
            RichText::new(&def.resource.description)
                .small()
                .color(Color32::from_gray(110)),
        );
    }
    ui.add_space(6.0);

    if app.reach_mode {
        let initiates = app
            .project
            .entity(id)
            .is_some_and(|e| ttg_codegen::reach::initiates(e.resource_type));
        let paths = if initiates { app.reach_paths() } else { Vec::new() };
        let mut show: Option<(Id, ttg_codegen::reach::Path, String)> = None;
        if !paths.is_empty() {
            ui.label(RichText::new("Can reach").strong());
            for path in &paths {
                let name = app
                    .project
                    .entity(&path.target)
                    .map(|e| e.name.to_string())
                    .unwrap_or_default();
                let (glyph, color) = reach_glyph(path.status);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{glyph} {name}")).color(color))
                        .on_hover_text(reach_tip(path));
                    if ui
                        .small_button("path")
                        .on_hover_text("Show only the resources on this path")
                        .clicked()
                    {
                        show = Some((id.clone(), path.clone(), format!("path to {name}")));
                    }
                });
            }
            ui.add_space(6.0);
        }
        let incoming = app.reach_incoming();
        if !incoming.is_empty() {
            ui.label(RichText::new("Reached by").strong());
            for (src, path) in &incoming {
                let name = app
                    .project
                    .entity(src)
                    .map(|e| e.name.to_string())
                    .unwrap_or_default();
                let (glyph, color) = reach_glyph(path.status);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{glyph} {name}")).color(color))
                        .on_hover_text(reach_tip(path));
                    if ui
                        .small_button("path")
                        .on_hover_text("Show only the resources on this path")
                        .clicked()
                    {
                        show = Some((src.clone(), path.clone(), format!("path from {name}")));
                    }
                });
            }
            ui.add_space(6.0);
        }
        if let Some(port) = app
            .project
            .entity(id)
            .and_then(|e| ttg_codegen::reach::listening_port(&e))
        {
            ui.label(
                RichText::new(format!("Listens on port {port}"))
                    .small()
                    .color(Color32::from_gray(110)),
            );
            ui.add_space(6.0);
        }
        if let Some((src, path, what)) = show {
            let ids = app.path_entities(&src, &path);
            app.show_only(ids, &what);
        }
    }

    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("props").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            // name
            ui.label("Name");
            let mut name = app.project.entity(id).unwrap().name.to_string();
            let r = ui.add(egui::TextEdit::singleline(&mut name).desired_width(f32::INFINITY));
            track_text_edit(app, &r);
            if r.changed() {
                set_name(app, id, name);
            }
            ui.end_row();

            ui.label("HCL name");
            ui.label(RichText::new(app.project.hcl_name(id)).monospace().small());
            ui.end_row();

            // parent
            ui.label("Inside");
            let parent = app.project.parent_of(id).map(|p| p.to_string());
            let parent_label = parent
                .as_ref()
                .and_then(|p| app.project.entity(p).map(|e| e.name.to_string()))
                .unwrap_or("(top level)".into());
            egui::ComboBox::from_id_salt(("parent", id))
                .selected_text(parent_label)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    let mut choice: Option<Option<String>> = None;
                    if ui.selectable_label(parent.is_none(), "(top level)").clicked() {
                        choice = Some(None);
                    }
                    let containers: Vec<(String, String, String)> = app
                        .project
                        .containers
                        .values()
                        .filter(|c| &c.id != id && !app.project.is_ancestor(id, &c.id))
                        .filter(|c| def.resource.allowed_parents.iter().any(|a| a == &c.container_type))
                        .map(|c| (c.id.clone(), c.name.clone(), c.container_type.clone()))
                        .collect();
                    for (cid, cname, ctype) in containers {
                        if ui
                            .selectable_label(parent.as_deref() == Some(&cid), format!("{cname} ({ctype})"))
                            .clicked()
                        {
                            choice = Some(Some(cid));
                        }
                    }
                    if let Some(c) = choice {
                        let before = app.snapshot();
                        app.project.set_parent(id, c.as_deref());
                        app.finish(before);
                    }
                });
            ui.end_row();

            // manual flag
            ui.label("External");
            let mut manual = app.project.entity(id).unwrap().manual;
            if ui
                .checkbox(&mut manual, "managed by hand")
                .on_hover_text("Not generated. Other resources referencing it get input variables instead, and it is listed in MANUAL_STEPS.md.")
                .changed()
            {
                let before = app.snapshot();
                if let Some(n) = app.project.nodes.get_mut(id) {
                    n.manual = manual;
                } else if let Some(c) = app.project.containers.get_mut(id) {
                    c.manual = manual;
                }
                app.finish(before);
            }
            ui.end_row();

            // provider layers
            ui.label("Providers");
            provider_checkboxes(app, ui, id, None);
            ui.end_row();

            if !is_container {
                ui.label("Size");
                ui.horizontal(|ui| {
                    let r = app.entity_rect(id).unwrap();
                    let cur = app.project.nodes[id].size;
                    let mut size = ttg_core::Size {
                        w: r.width() as i32,
                        h: r.height() as i32,
                    };
                    let a = ui.add(egui::DragValue::new(&mut size.w).range(120..=1200).suffix(" w"));
                    let b = ui.add(egui::DragValue::new(&mut size.h).range(48..=800).suffix(" h"));
                    if a.changed() || b.changed() {
                        let before = app.snapshot();
                        let id = id.clone();
                        app.with_layout(|p| p.nodes.get_mut(&id).unwrap().size = Some(size));
                        app.finish(before);
                    }
                    if cur.is_some() && app.active_layout().is_none() && ui.small_button("reset").clicked() {
                        let before = app.snapshot();
                        app.project.nodes.get_mut(id).unwrap().size = None;
                        app.finish(before);
                    }
                });
                ui.end_row();
            }
            if is_container {
                ui.label("Size");
                let r = app.entity_rect(id).unwrap();
                let mut size = ttg_core::Size {
                    w: r.width() as i32,
                    h: r.height() as i32,
                };
                ui.horizontal(|ui| {
                    let a = ui.add(egui::DragValue::new(&mut size.w).range(220..=4000).suffix(" w"));
                    let b = ui.add(egui::DragValue::new(&mut size.h).range(140..=4000).suffix(" h"));
                    if a.changed() || b.changed() {
                        let before = app.snapshot();
                        let id = id.clone();
                        app.with_layout(|p| p.containers.get_mut(&id).unwrap().size = size);
                        app.finish(before);
                    }
                });
                ui.end_row();
            }
        });

        // ---- abstract fields (concrete mode: only those the target mapping consumes)
        let used: Option<std::collections::HashSet<String>> = if concrete {
            Some(
                def.providers
                    .get(&provider)
                    .map(ttg_catalog::usage::used_fields)
                    .unwrap_or_default(),
            )
        } else {
            None
        };
        let shown_fields: Vec<&FieldDef> = def
            .fields
            .iter()
            .filter(|f| used.as_ref().is_none_or(|u| u.contains(&f.name)))
            .collect();
        let skipped = def.fields.len() - shown_fields.len();
        if !shown_fields.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new("Configuration").strong());
            egui::Grid::new("fields").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                for f in &shown_fields {
                    field_editor(app, ui, id, None, f);
                }
            });
        }
        if skipped > 0 {
            ui.label(
                RichText::new(format!(
                    "{skipped} field(s) not used by the {} mapping are hidden (switch to Abstract to see them).",
                    app.provider_display_name()
                ))
                .small()
                .color(Color32::from_gray(120)),
            );
        }

        for f in shown_fields.iter().filter(|f| f.field_type == FieldType::StructList) {
            struct_list_editor(app, ui, id, None, f);
        }

        // ---- provider fields (target provider first, others collapsed; concrete mode:
        // target provider only)
        let mut provider_ids: Vec<String> = def
            .providers
            .keys()
            .filter(|p| !concrete || *p == &provider)
            .cloned()
            .collect();
        provider_ids.sort_by_key(|p| p != &provider);
        for pid in provider_ids {
            let m = &def.providers[&pid];
            let pname = app
                .catalog
                .provider(&pid)
                .map(|p| p.provider.display_name.clone())
                .unwrap_or(pid.clone());
            let is_target = pid == provider;
            ui.add_space(8.0);
            egui::CollapsingHeader::new(RichText::new(format!("{pname} mapping")).strong())
                .id_salt(("prov", &pid))
                .default_open(is_target)
                .show(ui, |ui| {
                    let (status, color) = match m.status {
                        MappingStatus::Full => ("full", Color32::from_rgb(52, 168, 83)),
                        MappingStatus::Partial => ("partial — manual steps after apply", Color32::from_rgb(235, 160, 30)),
                        MappingStatus::Logical => ("logical — nothing generated", Color32::from_gray(140)),
                    };
                    ui.label(RichText::new(status).small().color(color));
                    if !m.notes.is_empty() {
                        ui.label(RichText::new(&m.notes).small().color(Color32::from_gray(110)));
                    }
                    let addrs = ttg_codegen::emit::addresses_for(&app.project, &app.catalog, &pid, id);
                    for a in addrs {
                        ui.label(RichText::new(a).monospace().small());
                    }
                    if !m.fields.is_empty() {
                        egui::Grid::new(("pfields", &pid)).num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                            for f in &m.fields {
                                field_editor(app, ui, id, Some(&pid), f);
                            }
                        });
                    }
                    for f in m.fields.iter().filter(|f| f.field_type == FieldType::StructList) {
                        struct_list_editor(app, ui, id, Some(&pid), f);
                    }
                    crate::schema_editor::extra_args_editor(app, ui, id, &pid, m, ttg_catalog::Catalog::is_native(&type_id));
                    for s in &m.manual_steps {
                        ui.label(RichText::new(format!("• {}", s.title)).small().color(Color32::from_rgb(200, 120, 20)));
                    }
                });
        }
        if !def.providers.contains_key(&provider) {
            ui.add_space(8.0);
            let msg = match app.provider_scope(&type_id) {
                Some(only) if is_container => format!(
                    "{} exists only on {only}. On {} it is just grouping: its contents are exported as if they sat in the enclosing container.",
                    def.resource.display_name,
                    app.provider_display_name()
                ),
                Some(only) => format!(
                    "{} exists only on {only}. The {} export leaves it out, and links to it are dropped there (see the parity note in Diagnostics).",
                    def.resource.display_name,
                    app.provider_display_name()
                ),
                None => format!("No mapping for the selected provider ({provider}). Listed in MANUAL_STEPS.md on export."),
            };
            ui.label(RichText::new(msg).color(Color32::from_rgb(200, 90, 30)));
        }

        // ---- relations
        ui.add_space(8.0);
        ui.label(RichText::new("Links").strong());
        let outgoing: Vec<(usize, String, ttg_core::Relation)> = app
            .project
            .edges
            .iter()
            .enumerate()
            .filter(|(_, e)| &e.source == id)
            .map(|(i, e)| (i, e.target.clone(), e.relation))
            .collect();
        let incoming: Vec<(usize, String, ttg_core::Relation)> = app
            .project
            .edges
            .iter()
            .enumerate()
            .filter(|(_, e)| &e.target == id)
            .map(|(i, e)| (i, e.source.clone(), e.relation))
            .collect();
        if outgoing.is_empty() && incoming.is_empty() {
            ui.label(RichText::new("No explicit links. Drag from the node's port to connect.").small().color(Color32::from_gray(120)));
        }
        let mut remove: Option<usize> = None;
        for (i, t, r) in &outgoing {
            ui.horizontal(|ui| {
                let tname = app.project.entity(t).map(|e| e.name.to_string()).unwrap_or_default();
                ui.label(format!("→ {tname}"));
                ui.label(RichText::new(r.display_name()).small().color(Color32::from_gray(110)));
                if ttg_codegen::diagnostics::is_redundant_edge(&app.project, &app.catalog, &app.project.edges[*i]) {
                    ui.label(RichText::new("redundant").small().color(Color32::from_rgb(200, 120, 20)))
                        .on_hover_text("Already implied by containment; not drawn.");
                }
                if ui.small_button("×").on_hover_text("Remove link").clicked() {
                    remove = Some(*i);
                }
            });
        }
        for (i, s, r) in &incoming {
            ui.horizontal(|ui| {
                let sname = app.project.entity(s).map(|e| e.name.to_string()).unwrap_or_default();
                ui.label(format!("← {sname}"));
                ui.label(RichText::new(r.display_name()).small().color(Color32::from_gray(110)));
                if ui.small_button("×").on_hover_text("Remove link").clicked() {
                    remove = Some(*i);
                }
            });
        }
        if let Some(i) = remove {
            let before = app.snapshot();
            app.project.edges.remove(i);
            app.finish(before);
        }
        for r in &def.relations {
            let kind = ttg_core::Relation::from_key(&r.kind);
            let satisfied = kind.is_some_and(|k| {
                !ttg_codegen::diagnostics::relation_targets(&app.project, &app.catalog, &app.project.entity(id).unwrap(), k).is_empty()
            });
            let txt = format!(
                "{}: {} ({}{})",
                r.label.clone().unwrap_or(r.kind.clone()),
                r.targets.join(" / "),
                match r.cardinality {
                    ttg_catalog::Cardinality::One => "required",
                    ttg_catalog::Cardinality::Optional => "optional",
                    ttg_catalog::Cardinality::Many => "any number",
                },
                if r.via_parent { ", or by containment" } else { "" }
            );
            ui.label(RichText::new(txt).small().color(if satisfied || r.cardinality != ttg_catalog::Cardinality::One { Color32::from_gray(110) } else { Color32::from_rgb(200, 90, 30) }));
        }

        // ---- diagnostics for this entity
        let mine: Vec<ttg_codegen::Diagnostic> = app
            .diagnostics
            .iter()
            .filter(|d| d.entity.as_deref() == Some(id.as_str()))
            .cloned()
            .collect();
        if !mine.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new("Diagnostics").strong());
            for d in mine {
                ui.label(RichText::new(format!("{} {}", sev_glyph(d.severity), d.message)).small().color(sev_color(d.severity)));
            }
        }
    });
}

fn set_name(app: &mut TtgApp, id: &str, name: String) {
    if let Some(n) = app.project.nodes.get_mut(id) {
        n.name = name;
    } else if let Some(c) = app.project.containers.get_mut(id) {
        c.name = name;
    }
}

fn field_editor(app: &mut TtgApp, ui: &mut Ui, id: &str, provider: Option<&str>, f: &FieldDef) {
    let current: Option<Value> = {
        let e = app.project.entity(id).unwrap();
        match provider {
            Some(p) => e.provider_field(p, &f.name).cloned(),
            None => e.field(&f.name).cloned(),
        }
    };
    let check = ttg_catalog::fields::check_value(f, current.as_ref());
    let label = if f.required {
        format!("{} *", f.label())
    } else {
        f.label().to_string()
    };
    // Which providers consume this abstract field? Dim it when the target ignores it.
    let target = app.project.settings.target_provider.clone();
    let usage_hint: Option<String> = if provider.is_none() {
        let type_id = app.project.entity(id).unwrap().resource_type.to_string();
        app.catalog.resource(&type_id).map(|def| {
            let users = ttg_catalog::usage::providers_using(def, &f.name);
            let names = |ids: &[String]| {
                ids.iter()
                    .map(|p| {
                        app.catalog
                            .provider(p)
                            .map(|d| d.provider.display_name.clone())
                            .unwrap_or(p.clone())
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            if users.is_empty() {
                "Not used by any provider mapping yet.".to_string()
            } else if users.contains(&target) {
                format!("Used by: {}", names(&users))
            } else {
                format!(
                    "Not used by the {} mapping. Used by: {}",
                    app.catalog
                        .provider(&target)
                        .map(|d| d.provider.display_name.clone())
                        .unwrap_or(target.clone()),
                    names(&users)
                )
            }
        })
    } else {
        None
    };
    let ignored_by_target = usage_hint.as_deref().is_some_and(|h| h.starts_with("Not used"));
    let lbl = ui.label(RichText::new(label).color(if check.is_err() {
        Color32::from_rgb(220, 50, 50)
    } else if ignored_by_target {
        Color32::from_gray(150)
    } else {
        ui.visuals().text_color()
    }));
    let mut tip = f.description.clone();
    if let Some(h) = usage_hint {
        if !tip.is_empty() {
            tip.push_str("\n\n");
        }
        tip.push_str(&h);
    }
    if !tip.is_empty() {
        lbl.on_hover_text(tip);
    }
    let mut new_value: Option<Value> = None;
    ui.vertical(|ui| {
        match f.field_type {
            FieldType::Bool => {
                let mut b = current.as_ref().and_then(|v| v.as_bool()).unwrap_or(false);
                if ui.checkbox(&mut b, "").changed() {
                    new_value = Some(Value::Bool(b));
                }
            }
            FieldType::Int => {
                let mut i = current.as_ref().and_then(|v| v.as_int()).unwrap_or(0);
                if ui.add(egui::DragValue::new(&mut i)).changed() {
                    new_value = Some(Value::Int(i));
                }
            }
            FieldType::Enum => {
                let cur = current
                    .as_ref()
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                egui::ComboBox::from_id_salt(("enum", id, provider, &f.name))
                    .selected_text(if cur.is_empty() {
                        "(choose)".to_string()
                    } else {
                        cur.clone()
                    })
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        for o in &f.options {
                            if ui.selectable_label(&cur == o, o).clicked() {
                                new_value = Some(Value::Str(o.clone()));
                            }
                        }
                    });
            }
            FieldType::StringList => {
                let mut s = current.as_ref().map(|v| v.display()).unwrap_or_default();
                let r = ui.add(
                    egui::TextEdit::singleline(&mut s)
                        .hint_text("a, b, c")
                        .desired_width(f32::INFINITY),
                );
                track_text_edit(app, &r);
                if r.changed() {
                    new_value = Some(Value::List(
                        s.split(',')
                            .map(|x| x.trim().to_string())
                            .filter(|x| !x.is_empty())
                            .collect(),
                    ));
                }
            }
            FieldType::String | FieldType::Cidr => {
                let mut s = current
                    .as_ref()
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let r = ui.add(egui::TextEdit::singleline(&mut s).desired_width(f32::INFINITY));
                track_text_edit(app, &r);
                if r.changed() {
                    new_value = Some(Value::Str(s));
                }
            }
            FieldType::EntityRef => {
                let cur = current
                    .as_ref()
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let candidates: Vec<(String, String)> = app
                    .project
                    .entities()
                    .iter()
                    .filter(|x| x.id != id && f.targets.iter().any(|t| t == x.resource_type))
                    .map(|x| (x.id.to_string(), x.name.to_string()))
                    .collect();
                let shown = candidates
                    .iter()
                    .find(|(cid, _)| cid == &cur)
                    .map(|(_, n)| n.clone())
                    .unwrap_or_else(|| {
                        if cur.is_empty() {
                            "(none)".into()
                        } else {
                            "(missing)".into()
                        }
                    });
                egui::ComboBox::from_id_salt(("eref", id, provider, &f.name))
                    .selected_text(shown)
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        if ui.selectable_label(cur.is_empty(), "(none)").clicked() {
                            new_value = Some(Value::Str(String::new()));
                        }
                        for (cid, n) in &candidates {
                            if ui.selectable_label(&cur == cid, n).clicked() {
                                new_value = Some(Value::Str(cid.clone()));
                            }
                        }
                    });
            }
            FieldType::StructList => {
                let n = current
                    .as_ref()
                    .and_then(|v| v.as_records())
                    .map(|r| r.len())
                    .unwrap_or(0);
                ui.label(
                    RichText::new(format!("{n} row(s) — edited in the table below"))
                        .small()
                        .color(Color32::from_gray(120)),
                );
            }
        }
        if let Err(msg) = &check {
            ui.label(RichText::new(msg).small().color(Color32::from_rgb(220, 50, 50)));
        }
    });
    ui.end_row();
    if let Some(v) = new_value {
        let is_text = matches!(
            f.field_type,
            FieldType::String | FieldType::Cidr | FieldType::StringList
        );
        let before = if is_text { None } else { Some(app.snapshot()) };
        {
            let cfg = match provider {
                Some(p) => {
                    if let Some(n) = app.project.nodes.get_mut(id) {
                        n.provider_config.entry(p.to_string()).or_default()
                    } else {
                        app.project
                            .containers
                            .get_mut(id)
                            .unwrap()
                            .provider_config
                            .entry(p.to_string())
                            .or_default()
                    }
                }
                None => {
                    if let Some(n) = app.project.nodes.get_mut(id) {
                        &mut n.config
                    } else {
                        &mut app.project.containers.get_mut(id).unwrap().config
                    }
                }
            };
            cfg.insert(f.name.clone(), v);
        }
        match before {
            Some(b) => app.finish(b),
            None => {
                app.dirty = true;
                app.diag_dirty = true;
            }
        }
    }
}

/// Table editor for a `struct_list` field: one row per record, one column per item.
fn struct_list_editor(app: &mut TtgApp, ui: &mut Ui, id: &str, provider: Option<&str>, f: &FieldDef) {
    let current: Vec<ttg_core::Record> = {
        let e = app.project.entity(id).unwrap();
        let v = match provider {
            Some(p) => e.provider_field(p, &f.name),
            None => e.field(&f.name),
        };
        v.and_then(|v| v.as_records())
            .map(|r| r.to_vec())
            .unwrap_or_default()
    };
    let mut rows = current.clone();
    let mut structural = false; // add/remove/combo/bool/int: one undo step each
    let mut text_changed = false; // typing: undo step managed by track_text_edit

    ui.add_space(8.0);
    let title = ui.label(RichText::new(f.label()).strong());
    if !f.description.is_empty() {
        title.on_hover_text(&f.description);
    }
    egui::ScrollArea::horizontal()
        .id_salt(("rows", id, provider, &f.name))
        .show(ui, |ui| {
            egui::Grid::new(("rowgrid", id, provider, &f.name))
                .striped(true)
                .spacing([6.0, 4.0])
                .show(ui, |ui| {
                    for sub in &f.items {
                        ui.label(RichText::new(sub.label()).small().strong())
                            .on_hover_text(&sub.description);
                    }
                    ui.label("");
                    ui.end_row();
                    let mut remove: Option<usize> = None;
                    for (ri, row) in rows.iter_mut().enumerate() {
                        for sub in &f.items {
                            let cur = row.get(&sub.name).cloned();
                            let bad = ttg_catalog::fields::check_value(sub, cur.as_ref()).is_err();
                            let salt = ("cell", id, provider, &f.name, ri, &sub.name);
                            let mut set: Option<Value> = None;
                            match sub.field_type {
                                FieldType::Bool => {
                                    let mut b = cur.as_ref().and_then(|v| v.as_bool()).unwrap_or(false);
                                    if ui.checkbox(&mut b, "").changed() {
                                        set = Some(Value::Bool(b));
                                        structural = true;
                                    }
                                }
                                FieldType::Int => {
                                    let mut i = cur.as_ref().and_then(|v| v.as_int()).unwrap_or(0);
                                    if ui.add(egui::DragValue::new(&mut i)).changed() {
                                        set = Some(Value::Int(i));
                                        structural = true;
                                    }
                                }
                                FieldType::Enum => {
                                    let s = cur.as_ref().and_then(|v| v.as_str()).unwrap_or("").to_string();
                                    egui::ComboBox::from_id_salt(salt)
                                        .selected_text(if s.is_empty() {
                                            "(choose)".into()
                                        } else {
                                            s.clone()
                                        })
                                        .width(90.0)
                                        .show_ui(ui, |ui| {
                                            for o in &sub.options {
                                                if ui.selectable_label(&s == o, o).clicked() {
                                                    set = Some(Value::Str(o.clone()));
                                                    structural = true;
                                                }
                                            }
                                        });
                                }
                                FieldType::EntityRef => {
                                    let s = cur.as_ref().and_then(|v| v.as_str()).unwrap_or("").to_string();
                                    let candidates: Vec<(String, String)> = app
                                        .project
                                        .entities()
                                        .iter()
                                        .filter(|x| {
                                            x.id != id && sub.targets.iter().any(|t| t == x.resource_type)
                                        })
                                        .map(|x| (x.id.to_string(), x.name.to_string()))
                                        .collect();
                                    let shown = candidates
                                        .iter()
                                        .find(|(cid, _)| cid == &s)
                                        .map(|(_, n)| n.clone())
                                        .unwrap_or_else(|| {
                                            if s.is_empty() {
                                                "(none)".into()
                                            } else {
                                                "(missing)".into()
                                            }
                                        });
                                    egui::ComboBox::from_id_salt(salt)
                                        .selected_text(shown)
                                        .width(110.0)
                                        .show_ui(ui, |ui| {
                                            if ui.selectable_label(s.is_empty(), "(none)").clicked() {
                                                set = Some(Value::Str(String::new()));
                                                structural = true;
                                            }
                                            for (cid, n) in &candidates {
                                                if ui.selectable_label(&s == cid, n).clicked() {
                                                    set = Some(Value::Str(cid.clone()));
                                                    structural = true;
                                                }
                                            }
                                        });
                                }
                                _ => {
                                    let mut s = cur.as_ref().map(|v| v.display()).unwrap_or_default();
                                    let width = if matches!(sub.field_type, FieldType::Cidr) {
                                        110.0
                                    } else {
                                        90.0
                                    };
                                    let mut te = egui::TextEdit::singleline(&mut s).desired_width(width);
                                    if bad {
                                        te = te.text_color(Color32::from_rgb(220, 50, 50));
                                    }
                                    let r = ui.add(te);
                                    track_text_edit(app, &r);
                                    if r.changed() {
                                        set = Some(Value::Str(s));
                                        text_changed = true;
                                    }
                                }
                            }
                            if let Some(v) = set {
                                row.insert(sub.name.clone(), v);
                            }
                        }
                        if ui.small_button("×").on_hover_text("Remove row").clicked() {
                            remove = Some(ri);
                        }
                        ui.end_row();
                    }
                    if let Some(ri) = remove {
                        rows.remove(ri);
                        structural = true;
                    }
                });
        });
    if ui.small_button("+ Add row").clicked() {
        rows.push(ttg_catalog::fields::default_row(f));
        structural = true;
    }
    if let Err(msg) = ttg_catalog::fields::check_value(f, Some(&Value::Records(rows.clone()))) {
        ui.label(RichText::new(msg).small().color(Color32::from_rgb(220, 50, 50)));
    }

    if structural || text_changed {
        let before = if structural { Some(app.snapshot()) } else { None };
        let cfg = match provider {
            Some(p) => {
                if let Some(n) = app.project.nodes.get_mut(id) {
                    n.provider_config.entry(p.to_string()).or_default()
                } else {
                    app.project
                        .containers
                        .get_mut(id)
                        .unwrap()
                        .provider_config
                        .entry(p.to_string())
                        .or_default()
                }
            }
            None => {
                if let Some(n) = app.project.nodes.get_mut(id) {
                    &mut n.config
                } else {
                    &mut app.project.containers.get_mut(id).unwrap().config
                }
            }
        };
        cfg.insert(f.name.clone(), Value::Records(rows));
        match before {
            Some(b) => app.finish(b),
            None => {
                app.dirty = true;
                app.diag_dirty = true;
            }
        }
    }
}

fn edge_inspector(app: &mut TtgApp, ui: &mut Ui, i: usize) {
    let Some(e) = app.project.edges.get(i).cloned() else {
        app.selected_edge = None;
        return;
    };
    ui.label(RichText::new("Link").strong().size(16.0));
    let sname = app
        .project
        .entity(&e.source)
        .map(|x| x.name.to_string())
        .unwrap_or_default();
    let tname = app
        .project
        .entity(&e.target)
        .map(|x| x.name.to_string())
        .unwrap_or_default();
    ui.label(format!("{sname}  →  {tname}"));
    ui.add_space(6.0);
    ui.label("Relationship");
    let choices = app.relation_choices(&e.source, &e.target);
    let mut rel = e.relation;
    for c in choices {
        ui.radio_value(&mut rel, c, c.display_name());
    }
    if rel != e.relation {
        let before = app.snapshot();
        app.project.edges[i].relation = rel;
        app.finish(before);
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label("Providers");
        provider_checkboxes(app, ui, "", Some(i));
    });
    if ttg_codegen::diagnostics::is_redundant_edge(&app.project, &app.catalog, &e) {
        ui.add_space(6.0);
        ui.label(
            RichText::new("Redundant: this relationship is already implied by containment, so the link is not drawn and has no effect.")
                .small()
                .color(Color32::from_rgb(200, 120, 20)),
        );
    }
    ui.add_space(8.0);
    ui.label(RichText::new("Routing").strong());
    ui.label(
        RichText::new("Pin which side of each end the line attaches to, and where along that side.")
            .small()
            .color(Color32::from_gray(110)),
    );
    let mut layout = e.layout.clone().unwrap_or_default();
    let mut changed = false;
    for (label, anchor) in [
        ("Source end", &mut layout.source),
        ("Target end", &mut layout.target),
    ] {
        ui.horizontal(|ui| {
            ui.label(label);
            let cur = anchor.side.clone().unwrap_or("auto".into());
            egui::ComboBox::from_id_salt(("anchor", i, label))
                .selected_text(cur.clone())
                .width(90.0)
                .show_ui(ui, |ui| {
                    for opt in ["auto", "left", "right", "top", "bottom"] {
                        if ui.selectable_label(cur == opt, opt).clicked() {
                            anchor.side = if opt == "auto" {
                                None
                            } else {
                                Some(opt.to_string())
                            };
                            changed = true;
                        }
                    }
                });
            if ui
                .add(egui::Slider::new(&mut anchor.offset, -100..=100).show_value(false))
                .on_hover_text("Position along the side (-100 = start, 0 = centre, 100 = end)")
                .drag_stopped()
            {
                changed = true;
            }
        });
    }
    if changed {
        let before = app.snapshot();
        app.project.edges[i].layout = if layout == ttg_core::EdgeLayout::default() {
            None
        } else {
            Some(layout)
        };
        app.finish(before);
    }
    ui.add_space(6.0);
    if ui.button("Remove link").clicked() {
        app.delete_selection();
    }
}

// ---------------------------------------------------------------------------------------

pub fn settings_ui(app: &mut TtgApp, ui: &mut Ui) {
    ui.label(RichText::new("Output").strong());
    ui.horizontal(|ui| {
        ui.label("Tool");
        let mut t = app.project.settings.tool;
        for opt in Tool::ALL {
            ui.radio_value(&mut t, opt, opt.display_name());
        }
        app.set_tool(t);
    });
    let mut enc = app.project.settings.state_encryption;
    if ui
        .add_enabled(
            app.project.settings.tool == Tool::OpenTofu,
            egui::Checkbox::new(&mut enc, "OpenTofu state encryption (pbkdf2 passphrase)"),
        )
        .changed()
    {
        let before = app.snapshot();
        app.project.settings.state_encryption = enc;
        app.finish(before);
    }
    ui.add_space(8.0);
    ui.label(RichText::new("State backend").strong());
    let mut kind = app
        .project
        .settings
        .backend
        .as_ref()
        .map(|b| b.backend_type.clone())
        .unwrap_or("none".into());
    egui::ComboBox::from_id_salt("backend")
        .selected_text(kind.clone())
        .show_ui(ui, |ui| {
            for k in ["none", "local", "s3", "azurerm", "gcs"] {
                ui.selectable_value(&mut kind, k.to_string(), k);
            }
        });
    let current_kind = app
        .project
        .settings
        .backend
        .as_ref()
        .map(|b| b.backend_type.clone())
        .unwrap_or("none".into());
    if kind != current_kind {
        let before = app.snapshot();
        app.project.settings.backend = if kind == "none" {
            None
        } else {
            let args: Vec<&str> = match kind.as_str() {
                "s3" => vec!["bucket", "key", "region"],
                "azurerm" => vec![
                    "resource_group_name",
                    "storage_account_name",
                    "container_name",
                    "key",
                ],
                "gcs" => vec!["bucket", "prefix"],
                _ => vec!["path"],
            };
            Some(ttg_core::BackendConfig {
                backend_type: kind.clone(),
                args: args.into_iter().map(|a| (a.to_string(), String::new())).collect(),
            })
        };
        app.finish(before);
    }
    if let Some(b) = app.project.settings.backend.clone() {
        egui::Grid::new("backend_args").num_columns(2).show(ui, |ui| {
            for (k, v) in b.args {
                ui.label(&k);
                let mut val = v.clone();
                let r = ui.add(egui::TextEdit::singleline(&mut val).desired_width(f32::INFINITY));
                track_text_edit(app, &r);
                if r.changed() {
                    app.project
                        .settings
                        .backend
                        .as_mut()
                        .unwrap()
                        .args
                        .insert(k.clone(), val);
                }
                ui.end_row();
            }
        });
    }
    ui.add_space(8.0);
    ui.label(RichText::new("Provider settings").strong());
    ui.label(
        RichText::new("Become the defaults of the provider-level variables in variables.tf.")
            .small()
            .color(Color32::from_gray(110)),
    );
    type ProviderVars = (String, String, Vec<(String, String)>);
    let providers: Vec<ProviderVars> = app
        .catalog
        .providers
        .iter()
        .map(|(id, p)| {
            (
                id.clone(),
                p.provider.display_name.clone(),
                p.variables
                    .iter()
                    .map(|v| (v.name.clone(), v.description.clone()))
                    .collect(),
            )
        })
        .collect();
    for (pid, pname, vars) in providers {
        ui.add_space(4.0);
        ui.label(RichText::new(pname).strong().small());
        egui::Grid::new(("pset", &pid)).num_columns(2).show(ui, |ui| {
            for (name, desc) in vars {
                ui.label(&name).on_hover_text(desc);
                let mut val = app
                    .project
                    .settings
                    .provider_settings
                    .get(&pid)
                    .and_then(|m| m.get(&name))
                    .cloned()
                    .unwrap_or_default();
                let r = ui.add(egui::TextEdit::singleline(&mut val).desired_width(f32::INFINITY));
                track_text_edit(app, &r);
                if r.changed() {
                    app.project
                        .settings
                        .provider_settings
                        .entry(pid.clone())
                        .or_default()
                        .insert(name.clone(), val);
                }
                ui.end_row();
            }
        });
    }
}

pub fn export_ui(app: &mut TtgApp, ui: &mut Ui) {
    if let Some(d) = &app.export.dir {
        ui.label(
            RichText::new(format!("Output: {}", d.display()))
                .monospace()
                .small(),
        );
    }
    ui.add_space(4.0);
    let results = app.export.results.clone();
    for (pid, r) in &results {
        ui.separator();
        match r {
            Ok(rep) => {
                ui.label(
                    RichText::new(format!(
                        "{} — {} ({} files)",
                        pid,
                        rep.tool.display_name(),
                        rep.files.len()
                    ))
                    .strong(),
                );
                ui.label(RichText::new(rep.files.join("  ")).small().monospace());
                for w in &rep.warnings {
                    ui.label(
                        RichText::new(format!("{} {}", sev_glyph(w.severity), w.message))
                            .small()
                            .color(sev_color(w.severity)),
                    );
                }
                if !rep.manual_steps.is_empty() {
                    ui.label(
                        RichText::new(format!(
                            "{} manual step(s) — see MANUAL_STEPS.md:",
                            rep.manual_steps.len()
                        ))
                        .color(Color32::from_rgb(200, 120, 20)),
                    );
                    for s in &rep.manual_steps {
                        ui.label(RichText::new(format!("  • {}", s.title)).small());
                    }
                }
            }
            Err(e) => {
                ui.label(
                    RichText::new(format!("{pid} — failed"))
                        .strong()
                        .color(Color32::from_rgb(220, 50, 50)),
                );
                ui.label(RichText::new(e).small());
            }
        }
    }
    ui.separator();
    ui.horizontal(|ui| {
        let bin = ttg_codegen::Profile::new(app.project.settings.tool).binary();
        let available = app.tool_binary_available();
        if ui
            .add_enabled(available, egui::Button::new(format!("Run `{bin} validate`")))
            .on_disabled_hover_text(format!("`{bin}` was not found on PATH. Run `{bin} init -backend=false && {bin} validate` in the export folder."))
            .clicked()
        {
            app.run_validate();
        }
        if !available {
            ui.label(RichText::new(format!("{bin} not on PATH")).small().color(Color32::from_gray(120)));
        }
    });
    for (pid, out) in &app.export.validate {
        ui.label(RichText::new(format!("[{pid}] {out}")).monospace().small());
    }
}

pub fn status_ui(app: &mut TtgApp, ui: &mut Ui) {
    ui.horizontal(|ui| {
        let errors = app.errors().len();
        let warnings = app
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .count();
        let toggle = ui.selectable_label(
            app.show_diagnostics,
            format!("Diagnostics: {errors} error(s), {warnings} warning(s)"),
        );
        if toggle.clicked() {
            app.show_diagnostics = !app.show_diagnostics;
        }
        ui.separator();
        ui.label(RichText::new(&app.status).small().color(Color32::from_gray(110)));
        let tool = app.project.settings.tool;
        let other = match tool {
            Tool::Terraform => Tool::OpenTofu,
            Tool::OpenTofu => Tool::Terraform,
        };
        if !app.tool_found(tool) {
            ui.separator();
            let msg = if app.tool_found(other) {
                format!(
                    "{} is not installed but {} is: switch the tool, or install {}",
                    tool.display_name(),
                    other.display_name(),
                    tool.display_name()
                )
            } else {
                format!(
                    "{} is not installed; export still works, validation does not",
                    tool.display_name()
                )
            };
            ui.label(RichText::new(msg).small().color(Color32::from_rgb(190, 120, 10)));
        }
    });
    if app.show_diagnostics {
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            let diags = app.diagnostics.clone();
            if diags.is_empty() {
                ui.label(
                    RichText::new("No problems for the selected provider.")
                        .small()
                        .color(Color32::from_gray(120)),
                );
            }
            for d in diags {
                let name = d
                    .entity
                    .as_ref()
                    .and_then(|id| app.project.entity(id).map(|e| e.name.to_string()))
                    .unwrap_or_default();
                let text = if name.is_empty() {
                    format!("{} {}", sev_glyph(d.severity), d.message)
                } else {
                    format!("{} {name}: {}", sev_glyph(d.severity), d.message)
                };
                let r = ui.selectable_label(false, RichText::new(text).small().color(sev_color(d.severity)));
                if r.clicked() {
                    if let Some(id) = &d.entity {
                        app.selection.clear();
                        app.selection.insert(id.clone());
                        app.selected_edge = None;
                    }
                }
            }
        });
    }
}

fn reach_glyph(s: ttg_codegen::reach::Status) -> (&'static str, Color32) {
    match s {
        ttg_codegen::reach::Status::Ok => ("✓", Color32::from_rgb(40, 140, 70)),
        ttg_codegen::reach::Status::Blocked => ("×", Color32::from_rgb(200, 40, 40)),
        ttg_codegen::reach::Status::Unknown => ("?", Color32::from_rgb(200, 130, 20)),
    }
}

fn reach_tip(path: &ttg_codegen::reach::Path) -> String {
    let mut tip = path.reason.clone();
    for n in &path.notes {
        tip.push('\n');
        tip.push_str(n);
    }
    tip
}

pub fn sev_glyph(s: Severity) -> &'static str {
    match s {
        Severity::Error => "⛔",
        Severity::Warning => "⚠",
        Severity::Info => "ℹ",
    }
}

pub fn sev_color(s: Severity) -> Color32 {
    match s {
        Severity::Error => Color32::from_rgb(200, 40, 40),
        Severity::Warning => Color32::from_rgb(190, 120, 10),
        Severity::Info => Color32::from_rgb(70, 90, 180),
    }
}

/// One checkbox per provider: which layers an entity (or, with `edge`, a link) is part of.
/// All checked is stored as "no tag". Provider-scoped types keep their scope disabled.
fn provider_checkboxes(app: &mut TtgApp, ui: &mut Ui, id: &str, edge: Option<usize>) {
    let providers: Vec<(String, String)> = app
        .catalog
        .providers
        .iter()
        .map(|(pid, d)| (pid.clone(), d.provider.short_name()))
        .collect();
    let current: Vec<String> = match edge {
        Some(i) => app.project.edges[i].providers.clone(),
        None => app
            .project
            .nodes
            .get(id)
            .map(|n| n.providers.clone())
            .or_else(|| app.project.containers.get(id).map(|c| c.providers.clone()))
            .unwrap_or_default(),
    };
    let scope: Vec<String> = match edge {
        Some(_) => Vec::new(),
        None => app
            .project
            .entity(id)
            .and_then(|e| app.catalog.resource(e.resource_type))
            .map(|d| d.resource.providers.clone())
            .unwrap_or_default(),
    };
    let mut new: Option<Vec<String>> = None;
    ui.horizontal(|ui| {
        for (pid, short) in &providers {
            let in_scope = scope.is_empty() || scope.contains(pid);
            let mut on = in_scope && (current.is_empty() || current.contains(pid));
            let r = ui.add_enabled(in_scope, egui::Checkbox::new(&mut on, short.as_str()));
            if !in_scope {
                r.on_hover_text(format!(
                    "{short}: this type has no {short} counterpart (provider-scoped definition)"
                ));
            } else if r.changed() {
                let mut set: Vec<String> = providers
                    .iter()
                    .map(|(p, _)| p.clone())
                    .filter(|p| {
                        if p == pid {
                            on
                        } else {
                            (scope.is_empty() || scope.contains(p))
                                && (current.is_empty() || current.contains(p))
                        }
                    })
                    .collect();
                if set.len() == providers.len() {
                    set.clear();
                }
                new = Some(set);
            }
        }
    });
    if let Some(set) = new {
        let before = app.snapshot();
        match edge {
            Some(i) => app.project.edges[i].providers = set,
            None => {
                if let Some(n) = app.project.nodes.get_mut(id) {
                    n.providers = set;
                } else if let Some(c) = app.project.containers.get_mut(id) {
                    c.providers = set;
                }
            }
        }
        app.finish(before);
    }
}

/// Parity summary for the project inspector: what each provider's layer leaves out.
fn layer_summary(app: &mut TtgApp, ui: &mut Ui) {
    let ids = app.catalog.provider_ids();
    let total = app.project.entities().len();
    if total == 0 {
        return;
    }
    ui.add_space(8.0);
    ui.label(RichText::new("Provider layers").strong());
    for pid in ids {
        let short = app
            .catalog
            .provider(&pid)
            .map(|d| d.provider.short_name())
            .unwrap_or(pid.clone());
        let off = ttg_codegen::layers::off_layer(&app.project, &app.catalog, &pid);
        let names: Vec<String> = off
            .iter()
            .map(|(id, _)| {
                app.project
                    .entity(id)
                    .map(|e| e.name.to_string())
                    .unwrap_or_default()
            })
            .collect();
        let text = if off.is_empty() {
            format!("{short}: all {total} resources")
        } else {
            format!(
                "{short}: {} of {total} resources; leaves out {}",
                total - off.len(),
                names.join(", ")
            )
        };
        ui.label(RichText::new(text).small().color(Color32::from_gray(90)));
    }
    ui.label(
        RichText::new("Tag a resource or link with the Providers checkboxes in its inspector to keep it out of the other provider's export. Provider-only types are tagged automatically.")
            .small()
            .color(Color32::from_gray(120)),
    );
}
