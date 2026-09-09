//! Schema-driven editor for extra provider arguments (curated resources) and for every
//! argument of a native provider resource. Argument names, types, required flags and
//! descriptions come from `ttg-schema`; values are stored as JSON on the entity.

use crate::app::TtgApp;
use egui::{Color32, RichText, Ui};
use serde_json::Value as J;
use ttg_catalog::ProviderMapping;
use ttg_schema::TypeKind;

/// (entity, provider, block, argument)
type BufferKey = (String, String, String, String);

/// Per-field text buffers for JSON / string editing, keyed by (entity, provider, block, arg).
#[derive(Default)]
pub struct EditorState {
    pub buffers: std::collections::HashMap<BufferKey, (String, Option<String>)>,
    pub search: std::collections::HashMap<(String, String), String>,
    pub block_choice: std::collections::HashMap<(String, String), String>,
    pub ref_popup: Option<RefPopup>,
}

pub struct RefPopup {
    pub entity: String,
    pub provider: String,
    pub block: String,
    pub arg: String,
    pub target: String,
    pub attr: String,
}

fn default_for(kind: Option<TypeKind>, nested: Option<&ttg_schema::NestedSchema>) -> J {
    if let Some(n) = nested {
        return if n.nesting() == "single" || n.max_items() == 1 {
            J::Object(Default::default())
        } else {
            J::Array(vec![J::Object(Default::default())])
        };
    }
    match kind {
        Some(TypeKind::Number) => J::from(0),
        Some(TypeKind::Bool) => J::Bool(false),
        Some(TypeKind::List) | Some(TypeKind::Set) => J::Array(vec![]),
        Some(TypeKind::Map) | Some(TypeKind::Object) => J::Object(Default::default()),
        _ => J::String(String::new()),
    }
}

/// The editor section for one provider mapping of an entity.
pub fn extra_args_editor(
    app: &mut TtgApp,
    ui: &mut Ui,
    id: &str,
    provider: &str,
    m: &ProviderMapping,
    native: bool,
) {
    let blocks: Vec<(String, String)> = m
        .blocks
        .iter()
        .map(|b| (b.key.clone(), b.resource.clone()))
        .collect();
    if blocks.is_empty() {
        return;
    }
    let key = (id.to_string(), provider.to_string());
    let chosen = app
        .schema_editor
        .block_choice
        .get(&key)
        .cloned()
        .filter(|c| blocks.iter().any(|(k, _)| k == c))
        .unwrap_or_else(|| {
            blocks
                .iter()
                .find(|(k, _)| k == "main")
                .map(|(k, _)| k.clone())
                .unwrap_or(blocks[0].0.clone())
        });
    let resource = blocks
        .iter()
        .find(|(k, _)| *k == chosen)
        .map(|(_, r)| r.clone())
        .unwrap();

    ui.add_space(6.0);
    let title = if native {
        "Arguments".to_string()
    } else {
        format!("Advanced arguments ({resource})")
    };
    egui::CollapsingHeader::new(RichText::new(title).strong())
        .id_salt(("extra", id, provider))
        .default_open(native)
        .show(ui, |ui| {
            if blocks.len() > 1 {
                ui.horizontal(|ui| {
                    ui.label("Block");
                    egui::ComboBox::from_id_salt(("extra-block", id, provider))
                        .selected_text(format!("{chosen} ({resource})"))
                        .show_ui(ui, |ui| {
                            for (k, r) in &blocks {
                                if ui.selectable_label(*k == chosen, format!("{k} ({r})")).clicked() {
                                    app.schema_editor.block_choice.insert(key.clone(), k.clone());
                                }
                            }
                        });
                });
            }
            let schema = ttg_schema::index().resource(provider, &resource).cloned();
            let Some(schema) = schema else {
                ui.label(
                    RichText::new(format!("No schema for {resource} in the bundled index (run `ttg schema refresh`)."))
                        .small()
                        .color(Color32::from_rgb(200, 90, 30)),
                );
                return;
            };
            if !native {
                ui.label(
                    RichText::new("Any argument the provider accepts, merged into the generated block. An argument the mapping already sets is overridden by yours.")
                        .small()
                        .color(Color32::from_gray(110)),
                );
            }
            let current: ttg_core::ExtraArgs = app
                .project
                .entity(id)
                .and_then(|e| e.extra_args(provider, &chosen).cloned())
                .unwrap_or_default();
            // Required arguments missing (native resources have no mapping to fill them).
            if native {
                let missing: Vec<&str> = schema
                    .required()
                    .into_iter()
                    .filter(|r| !current.contains_key(*r))
                    .collect();
                if !missing.is_empty() {
                    ui.label(
                        RichText::new(format!("Required: {}", missing.join(", ")))
                            .small()
                            .color(Color32::from_rgb(200, 40, 40)),
                    );
                }
            }
            // Existing arguments.
            let mut remove: Option<String> = None;
            let mut set: Option<(String, J)> = None;
            egui::Grid::new(("extra-grid", id, provider, &chosen))
                .num_columns(3)
                .spacing([8.0, 6.0])
                .show(ui, |ui| {
                    for (name, value) in &current {
                        let attr = schema.attributes.get(name);
                        let nested = schema.blocks.get(name);
                        let kind = attr.map(|a| a.kind());
                        let mut label = RichText::new(name);
                        let tip = if let Some(a) = attr {
                            format!("{}{}
{}", a.kind().label(), if a.required() { ", required" } else { "" }, a.description())
                        } else if let Some(n) = nested {
                            format!("nested block ({}{})", n.nesting(), if n.required() { ", required" } else { "" })
                        } else {
                            label = label.color(Color32::from_rgb(200, 40, 40));
                            "not in the provider schema".to_string()
                        };
                        ui.label(label).on_hover_text(tip);
                        value_editor(app, ui, id, provider, &chosen, name, value, kind, nested.is_some(), &mut set);
                        if ui.small_button("×").on_hover_text("Remove").clicked() {
                            remove = Some(name.clone());
                        }
                        ui.end_row();
                    }
                });
            if let Some((name, v)) = set {
                let before = app.snapshot();
                if let Some(m) = app.project.extra_args_mut(id, provider, &chosen) {
                    m.insert(name, v);
                }
                app.finish(before);
            }
            if let Some(name) = remove {
                let before = app.snapshot();
                if let Some(m) = app.project.extra_args_mut(id, provider, &chosen) {
                    m.remove(&name);
                }
                app.project.prune_extras(id);
                app.finish(before);
            }
            // Add an argument.
            ui.add_space(4.0);
            let search = app.schema_editor.search.entry(key.clone()).or_default();
            ui.horizontal(|ui| {
                ui.label("Add");
                ui.add(
                    egui::TextEdit::singleline(search)
                        .hint_text("search arguments…")
                        .desired_width(200.0),
                );
            });
            let q = search.to_lowercase();
            let mut candidates: Vec<(String, String, bool)> = schema
                .attributes
                .iter()
                .filter(|(k, a)| !a.read_only() && !current.contains_key(*k))
                .map(|(k, a)| (k.clone(), format!("{}{} — {}", a.kind().label(), if a.required() { ", required" } else { "" }, a.description()), a.required()))
                .chain(
                    schema
                        .blocks
                        .iter()
                        .filter(|(k, _)| !current.contains_key(*k))
                        .map(|(k, n)| (k.clone(), format!("nested block ({}{})", n.nesting(), if n.required() { ", required" } else { "" }), n.required())),
                )
                .filter(|(k, _, _)| q.is_empty() || k.contains(&q))
                .collect();
            candidates.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
            let shown = if q.is_empty() { 12 } else { 30 };
            let total = candidates.len();
            let mut add: Option<String> = None;
            egui::ScrollArea::vertical()
                .id_salt(("extra-cands", id, provider))
                .max_height(160.0)
                .show(ui, |ui| {
                    for (k, tip, required) in candidates.iter().take(shown) {
                        let text = if *required { format!("{k} *") } else { k.clone() };
                        if ui.selectable_label(false, RichText::new(text).monospace().small()).on_hover_text(tip).clicked() {
                            add = Some(k.clone());
                        }
                    }
                    if total > shown {
                        ui.label(RichText::new(format!("… {} more; keep typing", total - shown)).small().color(Color32::from_gray(120)));
                    }
                });
            if let Some(k) = add {
                let v = default_for(schema.attributes.get(&k).map(|a| a.kind()), schema.blocks.get(&k));
                let before = app.snapshot();
                if let Some(m) = app.project.extra_args_mut(id, provider, &chosen) {
                    m.insert(k.clone(), v);
                }
                app.finish(before);
                app.schema_editor.search.insert(key.clone(), String::new());
            }
        });
    ref_popup(app, ui);
}

#[allow(clippy::too_many_arguments)]
fn value_editor(
    app: &mut TtgApp,
    ui: &mut Ui,
    id: &str,
    provider: &str,
    block: &str,
    name: &str,
    value: &J,
    kind: Option<TypeKind>,
    is_block: bool,
    set: &mut Option<(String, J)>,
) {
    let bkey = (
        id.to_string(),
        provider.to_string(),
        block.to_string(),
        name.to_string(),
    );
    let is_ref = value
        .as_object()
        .is_some_and(|o| o.contains_key("$ref") || o.contains_key("$raw"));
    ui.horizontal(|ui| {
        match (kind, is_block, is_ref) {
            (_, _, true) => {
                let text = describe_ref(app, value);
                ui.label(
                    RichText::new(text)
                        .monospace()
                        .small()
                        .color(Color32::from_rgb(30, 100, 220)),
                );
                if ui.small_button("edit").on_hover_text("Edit as JSON").clicked() {
                    app.schema_editor.buffers.insert(
                        bkey.clone(),
                        (serde_json::to_string(value).unwrap_or_default(), None),
                    );
                    *set = Some((name.to_string(), J::String(String::new())));
                }
            }
            (Some(TypeKind::Bool), false, _) => {
                let mut b = value.as_bool().unwrap_or(false);
                if ui.checkbox(&mut b, "").changed() {
                    *set = Some((name.to_string(), J::Bool(b)));
                }
            }
            (Some(TypeKind::Number), false, _) => {
                let mut n = value.as_f64().unwrap_or(0.0);
                if ui.add(egui::DragValue::new(&mut n).speed(1.0)).changed() {
                    let j = if n.fract() == 0.0 {
                        J::from(n as i64)
                    } else {
                        J::from(n)
                    };
                    *set = Some((name.to_string(), j));
                }
            }
            (Some(TypeKind::String), false, _) => {
                let mut text = app
                    .schema_editor
                    .buffers
                    .get(&bkey)
                    .map(|b| b.0.clone())
                    .unwrap_or_else(|| value.as_str().unwrap_or("").to_string());
                let r = ui.add(egui::TextEdit::singleline(&mut text).desired_width(180.0));
                if r.gained_focus() {
                    app.edit_snapshot = Some(app.snapshot());
                }
                if r.changed() {
                    if let Some(m) = app.project.extra_args_mut(id, provider, block) {
                        m.insert(name.to_string(), J::String(text.clone()));
                    }
                    app.dirty = true;
                    app.diag_dirty = true;
                }
                if r.lost_focus() {
                    if let Some(before) = app.edit_snapshot.take() {
                        app.finish(before);
                    }
                }
                if r.has_focus() {
                    app.schema_editor.buffers.insert(bkey.clone(), (text, None));
                } else {
                    app.schema_editor.buffers.remove(&bkey);
                }
                if ui
                    .small_button("ref")
                    .on_hover_text("Reference an attribute of another resource")
                    .clicked()
                {
                    app.schema_editor.ref_popup = Some(RefPopup {
                        entity: id.to_string(),
                        provider: provider.to_string(),
                        block: block.to_string(),
                        arg: name.to_string(),
                        target: String::new(),
                        attr: "id".into(),
                    });
                }
            }
            _ => {
                // Lists, maps, objects, nested blocks: JSON text with parse-on-blur.
                let (mut text, mut err) = app
                    .schema_editor
                    .buffers
                    .get(&bkey)
                    .cloned()
                    .unwrap_or_else(|| (serde_json::to_string_pretty(value).unwrap_or_default(), None));
                let r = ui.add(
                    egui::TextEdit::multiline(&mut text)
                        .desired_width(240.0)
                        .desired_rows(2)
                        .font(egui::TextStyle::Monospace),
                );
                if r.gained_focus() {
                    app.edit_snapshot = Some(app.snapshot());
                }
                if r.lost_focus() {
                    match serde_json::from_str::<J>(&text) {
                        Ok(v) => {
                            err = None;
                            if let Some(m) = app.project.extra_args_mut(id, provider, block) {
                                m.insert(name.to_string(), v);
                            }
                            app.diag_dirty = true;
                            if let Some(before) = app.edit_snapshot.take() {
                                app.finish(before);
                            }
                        }
                        Err(e) => {
                            err = Some(e.to_string());
                            app.edit_snapshot = None;
                        }
                    }
                }
                if let Some(e) = &err {
                    ui.label(
                        RichText::new(format!("JSON: {e}"))
                            .small()
                            .color(Color32::from_rgb(200, 40, 40)),
                    );
                }
                if r.has_focus() || err.is_some() {
                    app.schema_editor.buffers.insert(bkey.clone(), (text, err));
                } else {
                    app.schema_editor.buffers.remove(&bkey);
                }
            }
        }
    });
}

fn describe_ref(app: &TtgApp, v: &J) -> String {
    if let Some(raw) = v.get("$raw").and_then(|r| r.as_str()) {
        return format!("raw: {raw}");
    }
    if let Some(r) = v.get("$ref") {
        let key = r.get("entity").and_then(|x| x.as_str()).unwrap_or("?");
        let name = app
            .project
            .entity(key)
            .map(|e| e.name.to_string())
            .unwrap_or(key.to_string());
        let attr = r.get("attr").and_then(|x| x.as_str()).unwrap_or("id");
        return format!("→ {name}.{attr}");
    }
    v.to_string()
}

/// The "reference another resource" popup for string arguments.
fn ref_popup(app: &mut TtgApp, ui: &mut Ui) {
    let Some(mut pop) = app.schema_editor.ref_popup.take() else {
        return;
    };
    let mut done: Option<bool> = None;
    egui::Window::new("Reference a resource attribute")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ui.ctx(), |ui| {
            ui.label(
                RichText::new(format!(
                    "Argument `{}` gets a traversal like aws_vpc.main.id",
                    pop.arg
                ))
                .small(),
            );
            let entities: Vec<(String, String)> = app
                .project
                .entities()
                .iter()
                .filter(|e| e.id != pop.entity)
                .map(|e| (e.id.to_string(), e.name.to_string()))
                .collect();
            let cur_name = entities
                .iter()
                .find(|(i, _)| *i == pop.target)
                .map(|(_, n)| n.clone())
                .unwrap_or("(choose)".into());
            egui::ComboBox::from_id_salt("ref-target")
                .selected_text(cur_name)
                .width(260.0)
                .show_ui(ui, |ui| {
                    for (i, n) in &entities {
                        if ui.selectable_label(*i == pop.target, n).clicked() {
                            pop.target = i.clone();
                        }
                    }
                });
            ui.horizontal(|ui| {
                ui.label("Attribute");
                ui.text_edit_singleline(&mut pop.attr);
            });
            // Offer the attributes the target's primary block exposes, from the schema.
            if let Some(e) = app.project.entity(&pop.target) {
                if let Some(m) = app.catalog.mapping(e.resource_type, &pop.provider) {
                    if let Some(b) = m.blocks.iter().find(|b| b.key == "main").or(m.blocks.first()) {
                        if let Some(s) = ttg_schema::index().resource(&pop.provider, &b.resource) {
                            ui.label(
                                RichText::new(format!(
                                    "{}: {}",
                                    b.resource,
                                    s.attributes
                                        .keys()
                                        .take(12)
                                        .cloned()
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ))
                                .small()
                                .color(Color32::from_gray(110)),
                            );
                        }
                    }
                }
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!pop.target.is_empty(), egui::Button::new("Insert"))
                    .clicked()
                {
                    done = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    done = Some(false);
                }
            });
        });
    match done {
        Some(true) => {
            let before = app.snapshot();
            let v = serde_json::json!({"$ref": {"entity": pop.target, "attr": pop.attr}});
            if let Some(m) = app.project.extra_args_mut(&pop.entity, &pop.provider, &pop.block) {
                m.insert(pop.arg.clone(), v);
            }
            app.schema_editor.buffers.remove(&(
                pop.entity.clone(),
                pop.provider.clone(),
                pop.block.clone(),
                pop.arg.clone(),
            ));
            app.finish(before);
        }
        Some(false) => {}
        None => app.schema_editor.ref_popup = Some(pop),
    }
}
