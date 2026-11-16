//! Clone and smear brushes, on paint layers and masks.
//!
//! Clone: Ctrl+click where to copy from, then paint elsewhere; the source keeps its
//! offset from the brush (aligned), so later strokes carry on from where the first one
//! left off until a new source is picked. Smear: each frame mixes in what lay a step
//! back along the brush's path, so the paint travels with it.
//!
//! Both work in texture space (`apply.wgsl`): exact in the 2D view, and in the 3D view
//! wherever the UVs run on without a seam.

use super::path_tool::PathPoint;
use super::{PainterApp, ViewKind, theme};
use crate::engine::{PaintMode, Stamp};
use eframe::egui;
use egui::{Color32, Rect, Stroke, vec2};
use glam::Vec2;

#[derive(Default)]
pub(super) struct CloneState {
    /// Where cloning reads: its UV, and where to show it.
    source: Option<(Vec2, PathPoint)>,
    /// Source minus destination in UV, fixed by the first stroke after picking a source.
    offset: Option<Vec2>,
    /// The smear stroke's last UV, to move from.
    smear_last: Option<Vec2>,
}

impl CloneState {
    pub fn has_source(&self) -> bool {
        self.source.is_some()
    }

    /// A smear stroke starts from wherever it's put down.
    pub fn start_smear(&mut self) {
        self.smear_last = None;
    }
}

/// Smear steps longer than this many brush radii are jumps (across a UV seam), not drags.
const MAX_SMEAR_STEP: f32 = 3.0;

impl PainterApp {
    /// The UV under a view pixel, and the point to mark it with.
    fn uv_under(&self, kind: ViewKind, at: Vec2) -> Option<(Vec2, PathPoint)> {
        match kind {
            ViewKind::V3d => {
                let [w, h] = self.views.view3d.size;
                let (o, d) = self.cam.ray(at, Vec2::new(w as f32, h as f32));
                let hit = self.bvh.raycast(o, d)?;
                let corner = |k: usize| Vec2::from_array(self.mesh.vertices[self.mesh.indices[hit.tri as usize * 3 + k] as usize].uv);
                let uv = corner(0) * hit.bary.x + corner(1) * hit.bary.y + corner(2) * hit.bary.z;
                Some((uv, PathPoint::Surface(hit.pos)))
            }
            ViewKind::V2d => {
                let uv = self.view2d_uv(at);
                Some((uv, PathPoint::Uv(uv)))
            }
        }
    }

    fn view2d_uv(&self, at: Vec2) -> Vec2 {
        let [w, h] = self.views.view2d.size;
        let center = Vec2::new(w as f32, h as f32) * 0.5;
        (at - center - self.cam2d.pan) / self.cam2d.zoom.max(1.0) + Vec2::splat(0.5)
    }

    /// Ctrl+click with the clone brush: copy from here.
    pub(super) fn set_clone_source(&mut self, kind: ViewKind, at: Vec2) {
        match self.uv_under(kind, at) {
            Some(source) => {
                self.clone.source = Some(source);
                self.clone.offset = None;
                self.status = "Clone source set. Paint somewhere else to copy from it; Ctrl+click picks another.".into();
            }
            None => self.status = "Ctrl+click on the mesh to pick where to clone from.".into(),
        }
    }

    /// A stamp's UV and radius in UV, if it landed on the mesh.
    fn stamp_uv(&self, kind: ViewKind, s: &Stamp) -> Option<(Vec2, f32)> {
        match kind {
            ViewKind::V2d => {
                let zoom = self.cam2d.zoom.max(1.0);
                Some((self.view2d_uv(Vec2::new(s[0][0], s[0][1])), self.brush.size * 0.5 * self.ppp * s[0][2] / zoom))
            }
            ViewKind::V3d => (s[1][3] > 0.0).then(|| (Vec2::new(s[4][0], s[4][1]), s[2][3])),
        }
    }

    /// What this frame of a clone or smear stroke does; `None` skips the frame (nothing
    /// to go by yet: the brush is off the mesh).
    pub(super) fn clone_mode(&mut self, kind: ViewKind, stamps: &[Stamp], smear: bool) -> Option<PaintMode> {
        let (uv, radius) = stamps.iter().rev().find_map(|s| self.stamp_uv(kind, s))?;
        if smear {
            let step = self.clone.smear_last.map_or(Vec2::ZERO, |last| uv - last);
            self.clone.smear_last = Some(uv);
            let step = if step.length() > radius * MAX_SMEAR_STEP { Vec2::ZERO } else { step };
            return Some(PaintMode::Smear((-step).to_array()));
        }
        let (source, _) = self.clone.source?;
        // The first stamp after picking a source fixes the offset (aligned cloning).
        let offset = match self.clone.offset {
            Some(o) => o,
            None => {
                let first = stamps.iter().find_map(|s| self.stamp_uv(kind, s)).map_or(uv, |f| f.0);
                *self.clone.offset.insert(source - first)
            }
        };
        Some(PaintMode::Clone(offset.to_array()))
    }

    /// The clone source: a crosshair where it was picked, then (in the 2D view) where
    /// it follows the cursor.
    pub(super) fn draw_clone_source(&self, painter: &egui::Painter, kind: ViewKind, rect: Rect, hover: Option<egui::Pos2>) {
        if self.tool != super::Tool::Clone {
            return;
        }
        let Some((uv, point)) = self.clone.source else { return };
        let to_ui = |p: Vec2| rect.min + vec2(p.x / self.ppp, p.y / self.ppp);
        let at = match (kind, self.clone.offset, hover) {
            (ViewKind::V2d, Some(off), Some(h)) if rect.contains(h) => {
                let cursor = self.view2d_uv(Vec2::new((h.x - rect.min.x) * self.ppp, (h.y - rect.min.y) * self.ppp));
                self.path_to_screen(kind, PathPoint::Uv(cursor + off))
            }
            (ViewKind::V2d, _, _) => self.path_to_screen(kind, PathPoint::Uv(uv)),
            (ViewKind::V3d, _, _) => self.path_to_screen(kind, point),
        };
        let Some(c) = at.map(to_ui) else { return };
        for (w, color) in [(3.0, Color32::from_black_alpha(160)), (1.5, theme::ACCENT)] {
            painter.circle_stroke(c, 7.0, Stroke::new(w, color));
            painter.line_segment([c - vec2(11.0, 0.0), c + vec2(11.0, 0.0)], Stroke::new(w, color));
            painter.line_segment([c - vec2(0.0, 11.0), c + vec2(0.0, 11.0)], Stroke::new(w, color));
        }
    }
}
