//! The region frame: a light, read-only frame around everything the canvas shows,
//! labelled with the provider and the region its resources are created in ("AWS ·
//! eu-west-2"), plus any other region the export reaches through an aliased provider
//! (CloudFront's us-east-1). It is never an entity and never exported; the resources
//! created in the other region carry a badge of their own (`canvas.rs`).

use crate::app::TtgApp;
use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Stroke, StrokeKind, Ui, Vec2};

/// Space between the drawing and the frame, in world units.
pub const FRAME_PAD: f32 = 36.0;
/// Height of the label strip above the frame's content, in world units.
pub const FRAME_LABEL: f32 = 26.0;

/// The provider's short name for the label.
fn provider_short(id: &str, display: &str) -> String {
    match id {
        "aws" => "AWS".into(),
        "azure" => "Azure".into(),
        "gcp" => "Google Cloud".into(),
        _ => display.to_string(),
    }
}

impl TtgApp {
    /// "AWS · eu-west-2", with the other regions in use after it.
    pub fn region_label(&self) -> Option<String> {
        let provider = &self.project.settings.target_provider;
        let region = ttg_codegen::canvas::project_region(&self.project, &self.catalog, provider)?;
        let display = self
            .catalog
            .provider(provider)
            .map(|p| p.provider.display_name.clone())
            .unwrap_or_default();
        let mut label = format!("{} · {region}", provider_short(provider, &display));
        let mut others: Vec<(&String, usize)> = Vec::new();
        for r in self.alias_regions.values() {
            match others.iter_mut().find(|(x, _)| *x == r) {
                Some((_, n)) => *n += 1,
                None => others.push((r, 1)),
            }
        }
        for (r, n) in others {
            label.push_str(&format!(
                "   (+ {r} for {n} resource{})",
                if n == 1 { "" } else { "s" }
            ));
        }
        Some(label)
    }

    /// The frame's world rect: the bounds of the visible entities, padded, with room for
    /// the label on top. `None` when the frame is off or there is nothing to frame.
    pub fn region_frame_rect(&self) -> Option<Rect> {
        if !self.show_region_frame {
            return None;
        }
        self.region_label()?;
        let mut r: Option<Rect> = None;
        for e in self.project.entities() {
            if !self.is_visible(e.id) {
                continue;
            }
            if let Some(er) = self.entity_rect(e.id) {
                r = Some(r.map_or(er, |x| x.union(er)));
            }
        }
        let r = r?;
        Some(Rect::from_min_max(
            r.min - Vec2::new(FRAME_PAD, FRAME_PAD + FRAME_LABEL),
            r.max + Vec2::splat(FRAME_PAD),
        ))
    }
}

/// Draw the frame behind everything else.
pub fn draw_frame(app: &TtgApp, ui: &mut Ui, origin: Pos2) {
    let (Some(wr), Some(label)) = (app.region_frame_rect(), app.region_label()) else {
        return;
    };
    let zoom = app.camera.zoom;
    let sr = app.camera.rect_to_screen(origin, wr);
    let painter = ui.painter_at(app.canvas_rect);
    let ink = Color32::from_rgb(90, 120, 160);
    painter.rect(
        sr,
        CornerRadius::same((10.0 * zoom).clamp(2.0, 12.0) as u8),
        Color32::from_rgba_unmultiplied(90, 120, 160, 8),
        Stroke::new(1.2_f32, Color32::from_rgba_unmultiplied(90, 120, 160, 140)),
        StrokeKind::Inside,
    );
    painter.text(
        sr.min + Vec2::new(14.0 * zoom, FRAME_LABEL * zoom * 0.55),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional((13.0 * zoom).max(7.0)),
        ink,
    );
}
