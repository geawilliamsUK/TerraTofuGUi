//! The windows behind File ▸ Compare with… / Checkpoints… / Import exported directory…
//! and Edit ▸ Insert pattern… / Save selection as pattern…, plus the inspector's "Adopt
//! existing" section. The logic is in `revisions.rs`.

use crate::app::TtgApp;
use egui::{Color32, RichText, Ui};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// What the windows remember between frames.
#[derive(Default)]
pub struct RevisionsUi {
    pub compare_open: bool,
    compare_against: String,
    /// `(label, summary, markdown)` of the last comparison, or why it failed.
    compare: Option<Result<(String, String, String), String>>,
    pub checkpoints_open: bool,
    new_checkpoint: String,
    /// A restore waiting for "discard unsaved changes?".
    confirm_restore: Option<String>,
    /// The listed checkpoints and when they were read (each line diffs a whole
    /// project, so not every frame).
    checkpoint_list: Option<(
        std::time::Instant,
        Result<Vec<crate::revisions::CheckpointInfo>, String>,
    )>,
    checkpoint_message: Option<Result<String, String>>,
    pub patterns_open: bool,
    pattern_selected: usize,
    pattern_params: BTreeMap<String, String>,
    pattern_lists: BTreeMap<String, BTreeSet<String>>,
    pattern_parent: Option<String>,
    pattern_message: Option<Result<String, String>>,
    pub save_pattern_open: bool,
    save_pattern_name: String,
    save_pattern_message: Option<Result<String, String>>,
    import: Option<crate::revisions::ImportPreview>,
    import_message: Option<Result<String, String>>,
    /// Text being typed into an "Adopt existing" id field: (entity, provider, key).
    adopt_edit: HashMap<(String, String, String), String>,
}

/// Entries for the File menu.
pub fn file_menu(app: &mut TtgApp, ui: &mut Ui) {
    if ui
        .button("Compare with…")
        .on_hover_text("What changed since the saved file, a checkpoint or another project file, in architecture terms (Markdown for a pull request)")
        .clicked()
    {
        app.revisions.ui.compare_open = true;
        if app.revisions.ui.compare_against.is_empty() {
            app.revisions.ui.compare_against = "saved".into();
        }
        ui.close();
    }
    if ui
        .button("Checkpoints…")
        .on_hover_text(
            "Named copies of the diagram to come back to or compare with, kept beside the project file",
        )
        .clicked()
    {
        app.revisions.ui.checkpoints_open = true;
        ui.close();
    }
    if ui
        .button("Import exported directory…")
        .on_hover_text("Bring hand edits made to a TerraTofu export back into the diagram (preview first)")
        .clicked()
    {
        ui.close();
        let picked = app.busy_while("a folder-picker dialog is open", || {
            rfd::FileDialog::new()
                .set_title("Directory TerraTofu exported (edited since)")
                .pick_folder()
        });
        if let Some(dir) = picked {
            match app.import_dir_preview(&dir, None) {
                Ok(p) => {
                    app.revisions.ui.import = Some(p);
                    app.revisions.ui.import_message = None;
                }
                Err(e) => app.error = Some(format!("Cannot import {}:\n{e}", dir.display())),
            }
        }
    }
}

/// Entries for the Edit menu.
pub fn edit_menu(app: &mut TtgApp, ui: &mut Ui) {
    if ui
        .button("Insert pattern…")
        .on_hover_text("Add a tested group of resources (hardened bucket, queue with dead-letter queue, …) in one undo step")
        .clicked()
    {
        app.revisions.ui.patterns_open = true;
        ui.close();
    }
    if ui
        .add_enabled(
            !app.selection.is_empty(),
            egui::Button::new("Save selection as pattern…"),
        )
        .clicked()
    {
        app.revisions.ui.save_pattern_open = true;
        ui.close();
    }
}

/// Every window of this module; called once a frame.
pub fn windows(app: &mut TtgApp, ctx: &egui::Context) {
    if app.hide_panels {
        return;
    }
    let mut st = std::mem::take(&mut app.revisions.ui);
    compare_window(app, &mut st, ctx);
    checkpoints_window(app, &mut st, ctx);
    patterns_window(app, &mut st, ctx);
    save_pattern_window(app, &mut st, ctx);
    import_window(app, &mut st, ctx);
    app.revisions.ui = st;
}

fn message(ui: &mut Ui, m: &Option<Result<String, String>>) {
    match m {
        Some(Ok(t)) => {
            ui.label(RichText::new(t).color(Color32::from_rgb(52, 140, 80)));
        }
        Some(Err(t)) => {
            ui.label(RichText::new(t).color(Color32::from_rgb(200, 60, 40)));
        }
        None => {}
    }
}

/// Read-only Markdown that can be selected and copied.
fn markdown_box(ui: &mut Ui, id: &str, text: &str, height: f32) {
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(height)
        .show(ui, |ui| {
            let mut t = text;
            ui.add(
                egui::TextEdit::multiline(&mut t)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY),
            );
        });
}

fn compare_window(app: &mut TtgApp, st: &mut RevisionsUi, ctx: &egui::Context) {
    if !st.compare_open {
        return;
    }
    let mut open = true;
    let mut run = false;
    egui::Window::new("Compare with…")
        .open(&mut open)
        .default_width(640.0)
        .show(ctx, |ui| {
            let names = app.checkpoint_names();
            ui.horizontal_wrapped(|ui| {
                ui.label("Compare the diagram with:");
                ui.add_enabled_ui(app.path.is_some(), |ui| {
                    if ui
                        .selectable_label(st.compare_against == "saved", "the saved file")
                        .clicked()
                    {
                        st.compare_against = "saved".into();
                        run = true;
                    }
                });
                for n in &names {
                    if ui
                        .selectable_label(&st.compare_against == n, format!("checkpoint \"{n}\""))
                        .clicked()
                    {
                        st.compare_against = n.clone();
                        run = true;
                    }
                }
                if ui.button("Another file…").clicked() {
                    if let Some(p) = app.busy_while("a file-picker dialog is open", || {
                        rfd::FileDialog::new()
                            .add_filter("TerraTofu project", &["json"])
                            .pick_file()
                    }) {
                        st.compare_against = p.display().to_string();
                        run = true;
                    }
                }
                if ui.button("Refresh").clicked() {
                    run = true;
                }
            });
            if run || st.compare.is_none() {
                st.compare = Some(
                    app.project_diff(&st.compare_against)
                        .map(|(l, d, md)| (l, d.summary(), md)),
                );
            }
            ui.separator();
            match &st.compare {
                Some(Ok((label, summary, md))) => {
                    ui.label(RichText::new(format!("{label} \u{2192} working copy: {summary}")).strong());
                    if ui.button("Copy Markdown").clicked() {
                        ui.ctx().copy_text(md.clone());
                    }
                    markdown_box(ui, "compare-md", md, 420.0);
                }
                Some(Err(e)) => {
                    ui.label(RichText::new(e).color(Color32::from_rgb(200, 60, 40)));
                }
                None => {}
            }
        });
    if !open {
        st.compare_open = false;
        st.compare = None;
    }
}

fn checkpoints_window(app: &mut TtgApp, st: &mut RevisionsUi, ctx: &egui::Context) {
    if !st.checkpoints_open {
        return;
    }
    let mut open = true;
    let mut restore: Option<String> = None;
    let mut delete: Option<String> = None;
    egui::Window::new("Checkpoints")
        .open(&mut open)
        .default_width(560.0)
        .show(ctx, |ui| {
            ui.label(
                RichText::new(format!("Kept in {}", app.checkpoint_storage()))
                    .small()
                    .color(Color32::from_gray(110)),
            );
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut st.new_checkpoint).hint_text("name, e.g. before queues"),
                );
                if ui
                    .add_enabled(
                        !st.new_checkpoint.trim().is_empty(),
                        egui::Button::new("Create checkpoint"),
                    )
                    .clicked()
                {
                    st.checkpoint_message = Some(
                        app.checkpoint_create(&st.new_checkpoint, false)
                            .map(|c| format!("Checkpoint \"{}\" saved", c.name)),
                    );
                    st.new_checkpoint.clear();
                    st.checkpoint_list = None;
                }
            });
            message(ui, &st.checkpoint_message);
            ui.separator();
            let stale = st
                .checkpoint_list
                .as_ref()
                .is_none_or(|(at, _)| at.elapsed() > std::time::Duration::from_secs(1));
            if stale {
                st.checkpoint_list = Some((std::time::Instant::now(), app.checkpoint_list()));
            }
            match st
                .checkpoint_list
                .as_ref()
                .map(|(_, l)| l.clone())
                .unwrap_or(Ok(Vec::new()))
            {
                Err(e) => {
                    ui.label(RichText::new(e).color(Color32::from_rgb(200, 60, 40)));
                }
                Ok(list) if list.is_empty() => {
                    ui.label(RichText::new("No checkpoints yet.").color(Color32::from_gray(120)));
                }
                Ok(list) => {
                    egui::Grid::new("checkpoints")
                        .num_columns(4)
                        .striped(true)
                        .show(ui, |ui| {
                            for c in list {
                                ui.label(RichText::new(&c.name).strong());
                                ui.label(RichText::new(&c.created).small());
                                ui.label(RichText::new(&c.changes_from_current).small())
                                    .on_hover_text("What restoring it would change");
                                ui.horizontal(|ui| {
                                    if ui.small_button("Restore").clicked() {
                                        restore = Some(c.name.clone());
                                    }
                                    if ui.small_button("Compare").clicked() {
                                        st.compare_against = c.name.clone();
                                        st.compare = None;
                                        st.compare_open = true;
                                    }
                                    if ui.small_button("Delete").clicked() {
                                        delete = Some(c.name.clone());
                                    }
                                });
                                ui.end_row();
                            }
                        });
                }
            }
        });
    if restore.is_some() || delete.is_some() {
        st.checkpoint_list = None;
    }
    if let Some(name) = restore {
        if app.dirty {
            st.confirm_restore = Some(name);
        } else {
            st.checkpoint_message = Some(
                app.checkpoint_restore(&name)
                    .map(|d| format!("Restored \"{name}\": {}", d.summary())),
            );
        }
    }
    if let Some(name) = delete {
        st.checkpoint_message = Some(
            app.checkpoint_delete(&name)
                .map(|_| format!("Deleted \"{name}\"")),
        );
    }
    if let Some(name) = st.confirm_restore.clone() {
        egui::Window::new("Restore checkpoint?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!(
                    "Restoring \"{name}\" replaces the diagram, which has unsaved changes. Undo brings them back."
                ));
                ui.horizontal(|ui| {
                    if ui.button("Restore").clicked() {
                        st.checkpoint_message =
                            Some(app.checkpoint_restore(&name).map(|d| format!("Restored \"{name}\": {}", d.summary())));
                        st.confirm_restore = None;
                    }
                    if ui.button("Cancel").clicked() {
                        st.confirm_restore = None;
                    }
                });
            });
    }
    if !open {
        st.checkpoints_open = false;
        st.checkpoint_message = None;
    }
}

fn patterns_window(app: &mut TtgApp, st: &mut RevisionsUi, ctx: &egui::Context) {
    if !st.patterns_open {
        return;
    }
    let (all, problems) = crate::revisions::all_patterns();
    if all.is_empty() {
        st.patterns_open = false;
        return;
    }
    st.pattern_selected = st.pattern_selected.min(all.len() - 1);
    let mut open = true;
    let mut insert = false;
    egui::Window::new("Insert pattern")
        .open(&mut open)
        .default_width(620.0)
        .show(ctx, |ui| {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(200.0);
                    for (i, e) in all.iter().enumerate() {
                        let label = if e.builtin() {
                            e.pattern.name.clone()
                        } else {
                            format!("{} (saved)", e.pattern.name)
                        };
                        if ui.selectable_label(st.pattern_selected == i, label).clicked()
                            && st.pattern_selected != i
                        {
                            st.pattern_selected = i;
                            st.pattern_params.clear();
                            st.pattern_lists.clear();
                            st.pattern_message = None;
                        }
                    }
                    for p in &problems {
                        ui.label(RichText::new(p).small().color(Color32::from_rgb(200, 60, 40)));
                    }
                });
                ui.separator();
                ui.vertical(|ui| {
                    let e = &all[st.pattern_selected];
                    ui.label(RichText::new(&e.pattern.name).strong());
                    if !e.pattern.description.is_empty() {
                        ui.label(RichText::new(&e.pattern.description).small());
                    }
                    ui.add_space(6.0);
                    egui::Grid::new("pattern-params")
                        .num_columns(2)
                        .spacing([8.0, 6.0])
                        .show(ui, |ui| {
                            for p in &e.pattern.params {
                                ui.label(&p.name).on_hover_text(&p.description);
                                match &p.entity {
                                    Some(want) => {
                                        let choices: Vec<(String, String)> = app
                                            .project
                                            .entities()
                                            .iter()
                                            .filter(|x| {
                                                want == "*"
                                                    || want.split(',').any(|w| w.trim() == x.resource_type)
                                            })
                                            .map(|x| (x.id.to_string(), x.name.to_string()))
                                            .collect();
                                        if p.list {
                                            let set = st.pattern_lists.entry(p.name.clone()).or_default();
                                            ui.vertical(|ui| {
                                                if choices.is_empty() {
                                                    ui.label(
                                                        RichText::new(format!("no {want} in the diagram"))
                                                            .small(),
                                                    );
                                                }
                                                for (id, name) in &choices {
                                                    let mut on = set.contains(id);
                                                    if ui.checkbox(&mut on, name).changed() {
                                                        if on {
                                                            set.insert(id.clone());
                                                        } else {
                                                            set.remove(id);
                                                        }
                                                    }
                                                }
                                            });
                                        } else {
                                            let cur = st.pattern_params.entry(p.name.clone()).or_default();
                                            let shown = choices
                                                .iter()
                                                .find(|(id, _)| id == cur)
                                                .map(|(_, n)| n.clone())
                                                .unwrap_or_else(|| {
                                                    if p.optional {
                                                        "(none)".into()
                                                    } else {
                                                        "choose…".into()
                                                    }
                                                });
                                            egui::ComboBox::from_id_salt(("pattern-entity", &p.name))
                                                .selected_text(shown)
                                                .show_ui(ui, |ui| {
                                                    if p.optional {
                                                        ui.selectable_value(cur, String::new(), "(none)");
                                                    }
                                                    for (id, name) in &choices {
                                                        ui.selectable_value(cur, id.clone(), name);
                                                    }
                                                });
                                        }
                                    }
                                    None => {
                                        let cur = st.pattern_params.entry(p.name.clone()).or_default();
                                        ui.add(
                                            egui::TextEdit::singleline(cur)
                                                .hint_text(p.default.clone().unwrap_or_default())
                                                .desired_width(240.0),
                                        );
                                    }
                                }
                                ui.end_row();
                            }
                            ui.label("Insert into");
                            let containers: Vec<(String, String)> = app
                                .project
                                .containers
                                .values()
                                .map(|c| (c.id.clone(), c.name.clone()))
                                .collect();
                            if st.pattern_parent.is_none() {
                                // The selected container, or the container of the selection.
                                st.pattern_parent = app.selection.iter().next().and_then(|id| {
                                    if app.project.containers.contains_key(id) {
                                        Some(id.clone())
                                    } else {
                                        app.project.parent_of(id).map(str::to_string)
                                    }
                                });
                                if st.pattern_parent.is_none() {
                                    st.pattern_parent = Some(String::new());
                                }
                            }
                            let cur = st.pattern_parent.get_or_insert_with(String::new);
                            let shown = containers
                                .iter()
                                .find(|(id, _)| id == cur)
                                .map(|(_, n)| n.clone())
                                .unwrap_or_else(|| "(top level)".into());
                            egui::ComboBox::from_id_salt("pattern-parent")
                                .selected_text(shown)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(cur, String::new(), "(top level)");
                                    for (id, name) in &containers {
                                        ui.selectable_value(cur, id.clone(), name);
                                    }
                                });
                            ui.end_row();
                        });
                    ui.add_space(6.0);
                    if ui.button("Insert (one undo step)").clicked() {
                        insert = true;
                    }
                    message(ui, &st.pattern_message);
                });
            });
        });
    if insert {
        let e = &all[st.pattern_selected];
        let mut params = serde_json::Map::new();
        for p in &e.pattern.params {
            if p.list {
                let set = st.pattern_lists.get(&p.name).cloned().unwrap_or_default();
                if !set.is_empty() {
                    params.insert(
                        p.name.clone(),
                        serde_json::json!(set.into_iter().collect::<Vec<_>>()),
                    );
                }
            } else if let Some(v) = st.pattern_params.get(&p.name).filter(|v| !v.trim().is_empty()) {
                params.insert(p.name.clone(), serde_json::Value::String(v.trim().to_string()));
            }
        }
        let parent = st.pattern_parent.clone().filter(|p| !p.is_empty());
        // Insertion runs through the agent's batch executor, which only the `mcp`
        // feature builds.
        #[cfg(feature = "mcp")]
        {
            st.pattern_message = Some(
                app.apply_pattern(&e.pattern.name, &params, None, None, parent.as_deref())
                    .map(|r| {
                        format!(
                            "Inserted {} entities and {} links",
                            r["entities"].as_array().map(|a| a.len()).unwrap_or(0),
                            r["links"]
                        )
                    }),
            );
        }
        #[cfg(not(feature = "mcp"))]
        {
            let _ = (params, parent);
            st.pattern_message = Some(Err(
                "inserting a pattern needs a build with the `mcp` feature (the default)".into(),
            ));
        }
    }
    if !open {
        st.patterns_open = false;
        st.pattern_message = None;
        st.pattern_parent = None;
    }
}

fn save_pattern_window(app: &mut TtgApp, st: &mut RevisionsUi, ctx: &egui::Context) {
    if !st.save_pattern_open {
        return;
    }
    let mut open = true;
    egui::Window::new("Save selection as pattern")
        .open(&mut open)
        .default_width(420.0)
        .show(ctx, |ui| {
            ui.label(format!(
                "{} selected entities and the links among them; links to anything else become parameters.",
                app.selection.len()
            ));
            ui.horizontal(|ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut st.save_pattern_name);
            });
            let can = !st.save_pattern_name.trim().is_empty() && !app.selection.is_empty();
            if ui.add_enabled(can, egui::Button::new("Save")).clicked() {
                let ids: Vec<String> = app.selection.iter().cloned().collect();
                st.save_pattern_message = Some(
                    ttg_codegen::patterns::infer(&app.project, &ids, &st.save_pattern_name, "")
                        .and_then(|p| {
                            let problems = ttg_codegen::patterns::check(&p, &app.catalog);
                            if problems.is_empty() {
                                Ok(p)
                            } else {
                                Err(problems.join("; "))
                            }
                        })
                        .and_then(|p| crate::revisions::save_pattern(&p, false))
                        .map(|path| format!("Saved to {}", path.display())),
                );
            }
            message(ui, &st.save_pattern_message);
        });
    if !open {
        st.save_pattern_open = false;
        st.save_pattern_message = None;
    }
}

fn import_window(app: &mut TtgApp, st: &mut RevisionsUi, ctx: &egui::Context) {
    let Some(preview) = &st.import else { return };
    let mut open = true;
    let mut apply = false;
    let mut cancel = false;
    egui::Window::new("Import exported directory")
        .open(&mut open)
        .default_width(680.0)
        .show(ctx, |ui| {
            ui.label(RichText::new(format!("{} ({})", preview.dir.display(), preview.result.provider)).strong());
            ui.label(format!("Architecture changes: {}", preview.diff.summary()));
            ui.label(format!(
                "{} argument override(s), {} new native entit{}, {} generated block(s) missing (not deleted), {} item(s) not imported",
                preview.result.overrides.len(),
                preview.result.added.len(),
                if preview.result.added.len() == 1 { "y" } else { "ies" },
                preview.result.deleted.len(),
                preview.result.skipped.len()
            ));
            ui.add_space(4.0);
            egui::CollapsingHeader::new("Import report")
                .default_open(true)
                .show(ui, |ui| markdown_box(ui, "import-report", &preview.result.report_markdown(), 220.0));
            egui::CollapsingHeader::new("Architecture diff")
                .default_open(false)
                .show(ui, |ui| markdown_box(ui, "import-diff", &preview.markdown, 220.0));
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!preview.result.is_empty(), egui::Button::new("Apply (one undo step)"))
                    .clicked()
                {
                    apply = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
                if ui.button("Copy Markdown").clicked() {
                    ui.ctx()
                        .copy_text(format!("{}\n{}", preview.markdown, preview.result.report_markdown()));
                }
            });
            message(ui, &st.import_message);
        });
    if apply {
        let preview = st.import.take().unwrap();
        match app.import_dir_apply(&preview) {
            Ok(()) => st.import_message = None,
            Err(e) => {
                st.import_message = Some(Err(e));
                st.import = Some(preview);
            }
        }
    } else if cancel || !open {
        st.import = None;
        st.import_message = None;
    }
}

/// The inspector's "Adopt existing" section for one provider: every resource block the
/// entity generates there, with the id of an existing resource to import into it.
pub fn adopt_section(app: &mut TtgApp, ui: &mut Ui, id: &str, provider: &str) {
    let adopted = app
        .project
        .adoption(id)
        .and_then(|a| a.get(provider))
        .cloned()
        .unwrap_or_default();
    egui::CollapsingHeader::new(RichText::new(format!("Adopt existing ({})", adopted.len())).small())
        .id_salt(("adopt", id, provider))
        .default_open(!adopted.is_empty())
        .show(ui, |ui| {
            ui.label(
                RichText::new(
                    "Ids of resources that already exist: the export writes an import block for each, so the first plan takes them over instead of creating them.",
                )
                .small()
                .color(Color32::from_gray(110)),
            );
            let blocks = app.adoptable(id, provider);
            if blocks.is_empty() {
                ui.label(RichText::new("Nothing is generated for it here.").small());
                return;
            }
            let mut commit: Option<(String, String)> = None;
            let mut st = std::mem::take(&mut app.revisions.ui.adopt_edit);
            egui::Grid::new(("adopt-grid", id, provider)).num_columns(2).show(ui, |ui| {
                for (key, address) in &blocks {
                    ui.label(RichText::new(address).monospace().small()).on_hover_text(format!("adopt key: {key}"));
                    let k = (id.to_string(), provider.to_string(), key.clone());
                    let buf = st
                        .entry(k.clone())
                        .or_insert_with(|| adopted.get(key).cloned().unwrap_or_default());
                    let r = ui.add(egui::TextEdit::singleline(buf).hint_text("existing id").desired_width(220.0));
                    if r.lost_focus() && buf.trim() != adopted.get(key).map(|s| s.as_str()).unwrap_or("") {
                        commit = Some((key.clone(), buf.clone()));
                    }
                    if !r.has_focus() && !r.lost_focus() {
                        // Follow undo / agent edits while the field is not being typed in.
                        *buf = adopted.get(key).cloned().unwrap_or_default();
                    }
                    ui.end_row();
                }
            });
            app.revisions.ui.adopt_edit = st;
            if let Some((key, value)) = commit {
                if let Err(e) = app.set_adopt(id, provider, &key, Some(&value)) {
                    app.status = e;
                }
            }
            for (key, _) in adopted.iter().filter(|(k, _)| !blocks.iter().any(|(b, a)| b == *k || a == *k)) {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("\"{key}\" names no block here"))
                            .small()
                            .color(Color32::from_rgb(200, 120, 20)),
                    );
                    if ui.small_button("Remove").clicked() {
                        let before = app.snapshot();
                        if let Some(m) = app.project.adoption_mut(id).and_then(|a| a.get_mut(provider)) {
                            m.remove(key);
                        }
                        if let Some(a) = app.project.adoption_mut(id) {
                            a.retain(|_, m| !m.is_empty());
                        }
                        app.finish(before);
                    }
                });
            }
        });
}
