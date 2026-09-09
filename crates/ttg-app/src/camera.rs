//! World <-> screen transform for the infinite canvas.

use egui::{Pos2, Rect, Vec2};

#[derive(Debug, Clone, Copy)]
pub struct Camera {
    /// Screen-space offset of world origin relative to the canvas rect's top-left.
    pub pan: Vec2,
    pub zoom: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Camera {
            pan: Vec2::new(40.0, 40.0),
            zoom: 1.0,
        }
    }
}

impl Camera {
    pub const MIN_ZOOM: f32 = 0.2;
    pub const MAX_ZOOM: f32 = 3.0;

    pub fn to_screen(self, origin: Pos2, w: Pos2) -> Pos2 {
        origin + self.pan + w.to_vec2() * self.zoom
    }

    pub fn to_world(self, origin: Pos2, s: Pos2) -> Pos2 {
        ((s - origin - self.pan) / self.zoom).to_pos2()
    }

    pub fn rect_to_screen(&self, origin: Pos2, r: Rect) -> Rect {
        Rect::from_min_max(self.to_screen(origin, r.min), self.to_screen(origin, r.max))
    }

    /// Zoom by `factor` keeping the world point under `screen_pos` fixed.
    pub fn zoom_at(&mut self, origin: Pos2, screen_pos: Pos2, factor: f32) {
        let before = self.to_world(origin, screen_pos);
        self.zoom = (self.zoom * factor).clamp(Self::MIN_ZOOM, Self::MAX_ZOOM);
        let after = self.to_screen(origin, before);
        self.pan += screen_pos - after;
    }

    /// Fit a world rect into the canvas rect with margin.
    pub fn fit(&mut self, canvas: Rect, world: Rect) {
        if world.width() <= 0.0 || world.height() <= 0.0 {
            return;
        }
        let margin = 60.0;
        let zx = (canvas.width() - margin * 2.0) / world.width();
        let zy = (canvas.height() - margin * 2.0) / world.height();
        self.zoom = zx.min(zy).clamp(Self::MIN_ZOOM, Self::MAX_ZOOM);
        let world_center = world.center();
        let screen_center = canvas.center() - canvas.min;
        self.pan = screen_center - world_center.to_vec2() * self.zoom;
    }
}
