//! The path tool: place points on the mesh (3D view) or on the UV map (2D
//! view), shape the curve by dragging them, then lay the brush along it with
//! even spacing, each stamp turned along the path. Made for stitches, seams,
//! piping and panel lines, which freehand strokes can't keep clean.
//!
//! Points live on the surface (world positions) or in UV space, not on the
//! screen, so the path stays put while the camera moves. The curve is
//! sampled in that space, then laid out on screen by arc length at the
//! brush spacing, and painted through the same stamp pipeline as strokes
//! (one undo step).
//!
//! An applied path stays live: moving, adding or removing points, or changing the brush,
//! paints it again in place of the last pass (while that pass is still the last step in
//! the history). Clicking on the curve adds a point there.

use super::{PainterApp, ViewKind, theme};
use crate::icons;
use eframe::egui;
use egui::{Color32, Rect, Stroke};
use glam::{Vec2, Vec3};

/// Grab distance for points, in screen points.
const GRAB: f32 = 9.0;
/// Curve samples per segment between two points.
const SUBDIV: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum PathPoint {
    /// On the mesh (3D view).
    Surface(Vec3),
    /// On the UV map (2D view).
    Uv(Vec2),
}

impl PathPoint {
    fn vec(self) -> Vec3 {
        match self {
            PathPoint::Surface(p) => p,
            PathPoint::Uv(p) => p.extend(0.0),
        }
    }
    fn like(self, v: Vec3) -> PathPoint {
        match self {
            PathPoint::Surface(_) => PathPoint::Surface(v),
            PathPoint::Uv(_) => PathPoint::Uv(v.truncate()),
        }
    }
}

pub(super) struct PathTool {
    /// The view the path was drawn in (paths don't move between views).
    pub view: Option<ViewKind>,
    pub points: Vec<PathPoint>,
    /// A smooth curve through the points, else straight segments.
    pub smooth: bool,
    /// Joins the last point back to the first.
    pub closed: bool,
    /// Turns each stamp along the path (stitches need it).
    pub align: bool,
    drag: Option<usize>,
    hovered: Option<usize>,
    /// Just painted: only the points are drawn, so the result shows.
    applied: bool,
    /// The painted pass that edits replace: its history serial, and what it was painted
    /// with (brush, layer, path settings), so a change paints it again.
    live: Option<(u64, String)>,
    /// The path changed since it was painted; it paints again once the pointer is up.
    changed: bool,
}

impl Default for PathTool {
    fn default() -> Self {
        PathTool {
            view: None,
            points: Vec::new(),
            smooth: true,
            closed: false,
            align: true,
            drag: None,
            hovered: None,
            applied: false,
            live: None,
            changed: false,
        }
    }
}

impl PathTool {
    pub fn clear(&mut self) {
        self.points.clear();
        self.view = None;
        self.drag = None;
        self.hovered = None;
        self.applied = false;
        self.live = None;
        self.changed = false;
    }

    /// The points moved, or one came or went.
    fn edited(&mut self) {
        self.applied = false;
        self.changed = true;
    }

    /// The curve through the points, densely sampled, in path space.
    fn curve(&self) -> Vec<PathPoint> {
        let pts = &self.points;
        let n = pts.len();
        if n < 2 {
            return pts.clone();
        }
        let seg_count = if self.closed { n } else { n - 1 };
        let at = |i: isize| -> Vec3 {
            if self.closed {
                pts[i.rem_euclid(n as isize) as usize].vec()
            } else {
                pts[i.clamp(0, n as isize - 1) as usize].vec()
            }
        };
        let mut out = Vec::with_capacity(seg_count * SUBDIV + 1);
        // Sample k lies on segment k / SUBDIV (see `insert_index`).
        for s in 0..seg_count as isize {
            let (p0, p1, p2, p3) = (at(s - 1), at(s), at(s + 1), at(s + 2));
            for k in 0..SUBDIV {
                let t = k as f32 / SUBDIV as f32;
                let v = if self.smooth { catmull_rom(p0, p1, p2, p3, t) } else { p1.lerp(p2, t) };
                out.push(pts[0].like(v));
            }
        }
        out.push(pts[0].like(if self.closed { at(0) } else { at(n as isize - 1) }));
        out
    }
}

/// Where a point picked on curve sample `sample` goes in the point list: after the
/// segment's first point.
fn insert_index(sample: usize, points: usize) -> usize {
    (sample / SUBDIV + 1).min(points)
}

/// The polyline sample nearest `at`, and how far it is.
fn nearest_sample(line: &[Vec2], at: Vec2) -> Option<(usize, f32)> {
    line.windows(2)
        .enumerate()
        .map(|(i, w)| {
            let (a, b) = (w[0], w[1]);
            let ab = b - a;
            let t = if ab.length_squared() > 1e-9 { ((at - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0) } else { 0.0 };
            (i, at.distance(a + ab * t))
        })
        .min_by(|x, y| x.1.total_cmp(&y.1))
}

/// Centripetal Catmull-Rom: no cusps or loops on uneven spacing.
fn catmull_rom(p0: Vec3, p1: Vec3, p2: Vec3, p3: Vec3, t: f32) -> Vec3 {
    let knot = |a: Vec3, b: Vec3| a.distance(b).sqrt().max(1e-5);
    let (t0, t1) = (0.0, knot(p0, p1));
    let t2 = t1 + knot(p1, p2);
    let t3 = t2 + knot(p2, p3);
    let u = t1 + (t2 - t1) * t;
    let l = |a: Vec3, b: Vec3, ta: f32, tb: f32| a * ((tb - u) / (tb - ta)) + b * ((u - ta) / (tb - ta));
    let a1 = l(p0, p1, t0, t1);
    let a2 = l(p1, p2, t1, t2);
    let a3 = l(p2, p3, t2, t3);
    let b1 = l(a1, a2, t0, t2);
    let b2 = l(a2, a3, t1, t3);
    l(b1, b2, t1, t2)
}

/// Points spaced `step` apart along a polyline, with the direction there.
pub(super) fn resample(line: &[Vec2], step: f32, closed: bool) -> Vec<(Vec2, Vec2)> {
    let mut out = Vec::new();
    if line.len() < 2 || step <= 0.0 {
        return out;
    }
    let mut carry = 0.0; // distance from the last stamp
    let mut first = true;
    for w in line.windows(2) {
        let (a, b) = (w[0], w[1]);
        let len = a.distance(b);
        if len < 1e-6 {
            continue;
        }
        let dir = (b - a) / len;
        if first {
            out.push((a, dir));
            first = false;
        }
        let mut t = step - carry;
        while t <= len {
            out.push((a + dir * t, dir));
            t += step;
        }
        carry = len - (t - step);
    }
    // A closed path's last stamp would land on its first.
    if closed && out.len() > 1 && out.last().unwrap().0.distance(out[0].0) < step * 0.5 {
        out.pop();
    }
    out
}

impl PainterApp {
    /// A path point in view pixels (the stamp coordinate space).
    pub(super) fn path_to_screen(&self, kind: ViewKind, p: PathPoint) -> Option<Vec2> {
        match (kind, p) {
            (ViewKind::V3d, PathPoint::Surface(w)) => {
                let [sw, sh] = self.views.view3d.size;
                let size = Vec2::new(sw as f32, sh as f32);
                let clip = self.cam.view_proj(size.x / size.y) * w.extend(1.0);
                if clip.w <= 1e-5 {
                    return None;
                }
                let ndc = clip.truncate() / clip.w;
                Some(Vec2::new((ndc.x + 1.0) * 0.5 * size.x, (1.0 - ndc.y) * 0.5 * size.y))
            }
            (ViewKind::V2d, PathPoint::Uv(uv)) => {
                let [sw, sh] = self.views.view2d.size;
                let center = Vec2::new(sw as f32, sh as f32) * 0.5;
                Some((uv - Vec2::splat(0.5)) * self.cam2d.zoom.max(1.0) + center + self.cam2d.pan)
            }
            _ => None,
        }
    }

    /// The path point under a view pixel (on the mesh, or on the UV map).
    pub(super) fn path_from_screen(&self, kind: ViewKind, px: Vec2) -> Option<PathPoint> {
        match kind {
            ViewKind::V3d => {
                let [w, h] = self.views.view3d.size;
                let (o, d) = self.cam.ray(px, Vec2::new(w as f32, h as f32));
                self.bvh.raycast(o, d).map(|hit| PathPoint::Surface(hit.pos))
            }
            ViewKind::V2d => {
                let [w, h] = self.views.view2d.size;
                let center = Vec2::new(w as f32, h as f32) * 0.5;
                Some(PathPoint::Uv((px - center - self.cam2d.pan) / self.cam2d.zoom.max(1.0) + Vec2::splat(0.5)))
            }
        }
    }

    /// Path tool input on one view. `local` is the cursor in view pixels.
    pub(super) fn path_input(&mut self, kind: ViewKind, cursor: Option<Vec2>, pressed: bool, down: bool, secondary: bool, hovered: bool) {
        let grab = GRAB * self.ppp;
        let near = |app: &PainterApp, at: Vec2| -> Option<usize> {
            if app.path.view != Some(kind) {
                return None;
            }
            app.path
                .points
                .iter()
                .enumerate()
                .filter_map(|(i, p)| Some((i, app.path_to_screen(kind, *p)?.distance(at))))
                .filter(|(_, d)| *d <= grab)
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| i)
        };
        if let Some(i) = self.path.drag {
            if down && let Some(at) = cursor {
                if let Some(p) = self.path_from_screen(kind, at)
                    && self.path.points[i] != p
                {
                    self.path.points[i] = p;
                    self.path.edited();
                }
                return;
            }
            self.path.drag = None;
        }
        let Some(at) = cursor else {
            self.path.hovered = None;
            return;
        };
        self.path.hovered = near(self, at);
        if !hovered {
            return;
        }
        if secondary {
            if let Some(i) = self.path.hovered {
                self.path.points.remove(i);
                self.path.edited();
                self.path.hovered = None;
            }
            return;
        }
        if !pressed {
            return;
        }
        if let Some(i) = self.path.hovered {
            self.path.drag = Some(i);
            return;
        }
        let Some(p) = self.path_from_screen(kind, at) else {
            self.status = "Place path points on the mesh.".into();
            return;
        };
        // On the curve: a new point there, between its neighbours, ready to drag.
        if self.path.view == Some(kind) && self.path.points.len() >= 2 {
            let line: Vec<Vec2> = self.path.curve().into_iter().map(|q| self.path_to_screen(kind, q).unwrap_or(Vec2::splat(f32::MAX))).collect();
            if let Some((k, d)) = nearest_sample(&line, at)
                && d <= grab
            {
                let i = insert_index(k, self.path.points.len());
                self.path.points.insert(i, p);
                self.path.drag = Some(i);
                self.path.edited();
                return;
            }
        }
        if self.path.view != Some(kind) {
            // A path belongs to one view; starting in the other begins a new one.
            self.path.clear();
            self.path.view = Some(kind);
        }
        self.path.points.push(p);
        self.path.edited();
    }

    /// The stamps the path lays down: (view pixel, direction).
    fn path_stamps(&self) -> Option<(ViewKind, Vec<(Vec2, Vec2)>)> {
        let kind = self.path.view?;
        if self.path.points.len() < 2 {
            return None;
        }
        let screen: Vec<Vec2> = self.path.curve().into_iter().filter_map(|p| self.path_to_screen(kind, p)).collect();
        let radius = self.brush.size * 0.5 * self.ppp;
        let step = (self.brush.spacing * radius * 2.0).max(1.0);
        Some((kind, resample(&screen, step, self.path.closed)))
    }

    /// What a painted path depends on besides its points: a change paints it again.
    fn path_inputs(&self) -> String {
        let b = &self.brush;
        format!(
            "{:?}",
            (
                (b.size, b.spacing, b.hardness, b.flow, b.opacity, b.angle, b.tip, b.mat, b.mask_value, b.projection, b.follow),
                (b.angle_jitter, b.size_jitter, b.scatter, b.grain, b.grain_scale, b.grain_strength, b.grain_invert),
                (self.selected, self.target, self.sets.active, self.tool == super::Tool::Eraser),
                (self.path.smooth, self.path.closed, self.path.align),
            )
        )
    }

    /// Paints a live path again after an edit, in place of its last pass, once the
    /// pointer is up (a drag repaints when it ends, not every frame). Call every frame.
    pub(super) fn update_live_path(&mut self, ctx: &egui::Context) {
        let Some((serial, inputs)) = &self.path.live else { return };
        if self.undo.state_serial() != *serial {
            // Something else was done since (or the pass was undone): nothing to replace.
            self.path.live = None;
            return;
        }
        let changed = self.path.changed || *inputs != self.path_inputs();
        if !changed || self.path.drag.is_some() || ctx.input(|i| i.pointer.any_down()) || self.stroke.is_some() {
            return;
        }
        self.path.changed = false;
        if self.path.points.len() < 2 {
            return;
        }
        self.engine.undo(&mut self.undo);
        // The replaced pass isn't something to redo.
        self.undo.redo.pop();
        self.apply_path();
    }

    /// Paints the brush along the path as one stroke (one undo step).
    pub(super) fn apply_path(&mut self) {
        let Some((kind, stamps)) = self.path_stamps() else {
            self.status = "Place at least two path points first.".into();
            return;
        };
        let Some(&(first, _)) = stamps.first() else { return };
        if !self.begin_stroke_on_target(kind, first, false) {
            return;
        }
        let follow = self.brush.follow;
        self.brush.follow = follow || self.path.align;
        let raw: Vec<([f32; 4], f32)> = stamps.iter().map(|&(p, d)| self.make_stamp(p, Some(d), 1.0)).collect();
        self.brush.follow = follow;
        for chunk in raw.chunks(crate::engine::MAX_STAMPS) {
            self.paint_stamps(kind, chunk);
        }
        self.end_stroke();
        self.path.applied = true;
        self.path.changed = false;
        self.path.live = Some((self.undo.state_serial(), self.path_inputs()));
        self.status = format!(
            "Painted {} stamps along the path. Edit the points or the brush to repaint it; Apply again adds another pass.",
            raw.len()
        );
    }

    /// Draws the path, its points and where the stamps will land.
    pub(super) fn draw_path(&self, painter: &egui::Painter, kind: ViewKind, rect: Rect) {
        if self.path.view != Some(kind) || self.tool != super::Tool::Path {
            return;
        }
        let ppp = self.ppp;
        let to_ui = |p: Vec2| rect.min + egui::vec2(p.x / ppp, p.y / ppp);
        let line: Vec<egui::Pos2> = self.path.curve().into_iter().filter_map(|p| self.path_to_screen(kind, p)).map(to_ui).collect();
        if line.len() > 1 && !self.path.applied {
            painter.add(egui::Shape::line(line.clone(), Stroke::new(3.0, Color32::from_black_alpha(140))));
            painter.add(egui::Shape::line(line, Stroke::new(1.5, theme::ACCENT)));
        }
        // Where each stamp lands, outlined at its size and turned with the path.
        if let Some((_, stamps)) = self.path_stamps().filter(|_| !self.path.applied) {
            let r = self.brush.size * 0.5;
            for (p, d) in stamps.iter().take(600) {
                let c = to_ui(*p);
                // Dark under light, so it reads on any surface.
                painter.circle_stroke(c, r, Stroke::new(2.0, Color32::from_black_alpha(90)));
                painter.circle_stroke(c, r, Stroke::new(1.0, Color32::from_white_alpha(150)));
                if self.path.align {
                    let a = d.y.atan2(d.x) + self.brush.angle.to_radians();
                    let v = egui::vec2(a.cos(), a.sin()) * r * 0.8;
                    painter.line_segment([c - v, c + v], Stroke::new(2.0, Color32::from_black_alpha(90)));
                    painter.line_segment([c - v, c + v], Stroke::new(1.0, Color32::WHITE));
                }
            }
        }
        for (i, p) in self.path.points.iter().enumerate() {
            let Some(s) = self.path_to_screen(kind, *p) else { continue };
            let c = to_ui(s);
            let hot = self.path.hovered == Some(i) || self.path.drag == Some(i);
            painter.circle_filled(c, if hot { 6.5 } else { 5.0 }, Color32::from_black_alpha(160));
            painter.circle_filled(c, if hot { 5.0 } else { 3.5 }, if i == 0 { Color32::WHITE } else { theme::ACCENT });
        }
    }

    /// The path tool's controls in the context toolbar.
    pub(super) fn path_toolbar(&mut self, ui: &mut egui::Ui) {
        let n = self.path.points.len();
        let toggle = |ui: &mut egui::Ui, on: &mut bool, text: &str, tip: &str| {
            if ui.add(theme::selectable(*on, text)).on_hover_text(tip).clicked() {
                *on = !*on;
            }
        };
        toggle(ui, &mut self.path.smooth, "Smooth", "A smooth curve through the points (off: straight segments)");
        toggle(ui, &mut self.path.closed, "Closed", "Join the last point back to the first");
        toggle(ui, &mut self.path.align, "Align to path", "Turn each stamp along the path; stitches need it");
        ui.separator();
        let apply = egui::Button::new(egui::RichText::new(theme::with_icon(icons::CHECK, "Apply")).color(Color32::WHITE)).fill(theme::ACCENT.gamma_multiply(0.6));
        if ui.add_enabled(n >= 2, apply).on_hover_text("Paint the brush along the path (Enter)").clicked() {
            self.apply_path();
        }
        if ui.add_enabled(n > 0, egui::Button::new("Clear")).on_hover_text("Start over (Esc); what's painted stays").clicked() {
            self.path.clear();
        }
        ui.add_space(6.0);
        ui.label(egui::RichText::new("Spacing").color(theme::TEXT_DIM));
        super::spacing_slider(ui, &mut self.brush.spacing);
        ui.label(egui::RichText::new("Size").color(theme::TEXT_DIM));
        ui.add(egui::Slider::new(&mut self.brush.size, 2.0..=800.0).max_decimals(0));
        ui.label(
            egui::RichText::new(if n == 0 {
                "Click to place points on the mesh or the UV map. Try a stitch brush tip."
            } else {
                "Drag points to shape · click the curve to add one · right-click removes · Backspace removes the last"
            })
            .small()
            .color(theme::TEXT_DIM),
        );
    }

    /// Path keys: Enter applies, Esc clears, Backspace removes the last point.
    pub(super) fn path_keys(&mut self, ctx: &egui::Context) {
        if self.tool != super::Tool::Path || ctx.egui_wants_keyboard_input() {
            return;
        }
        let (enter, esc, back) = ctx.input(|i| (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape), i.key_pressed(egui::Key::Backspace)));
        if enter {
            self.apply_path();
        }
        if esc {
            self.path.clear();
        }
        if back {
            self.path.points.pop();
            self.path.edited();
            if self.path.points.is_empty() {
                self.path.view = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_spaces_stamps_evenly_along_corners() {
        // An L: 10 across then 10 down, stamps every 3.
        let line = [Vec2::new(0.0, 0.0), Vec2::new(10.0, 0.0), Vec2::new(10.0, 10.0)];
        let s = resample(&line, 3.0, false);
        assert_eq!(s.len(), 7, "0, 3, 6, 9, then 12, 15, 18 along the path");
        assert_eq!(s[0].0, Vec2::ZERO);
        assert!((s[4].0 - Vec2::new(10.0, 2.0)).length() < 1e-4, "{:?}", s[4]);
        assert!((s[4].1 - Vec2::Y).length() < 1e-4, "turned down after the corner");
        for w in s.windows(2) {
            // Along the path the distance is exactly the step; across the corner it's shorter.
            assert!(w[0].0.distance(w[1].0) <= 3.0 + 1e-4);
        }
    }

    #[test]
    fn a_point_picked_on_the_curve_goes_between_its_neighbours() {
        let mut t = PathTool::default();
        t.points = vec![PathPoint::Uv(Vec2::new(0.0, 0.0)), PathPoint::Uv(Vec2::new(1.0, 0.0)), PathPoint::Uv(Vec2::new(2.0, 0.0))];
        t.smooth = false;
        let line: Vec<Vec2> = t.curve().into_iter().map(|p| p.vec().truncate() * 100.0).collect();
        let (k, d) = nearest_sample(&line, Vec2::new(150.0, 3.0)).unwrap();
        assert!(d < 3.5, "{d}");
        assert_eq!(insert_index(k, t.points.len()), 2, "between the second and third point");
        let (k, _) = nearest_sample(&line, Vec2::new(20.0, -2.0)).unwrap();
        assert_eq!(insert_index(k, t.points.len()), 1, "between the first two");
    }

    #[test]
    fn curves_pass_through_their_points() {
        let mut t = PathTool::default();
        t.points = vec![PathPoint::Uv(Vec2::new(0.1, 0.1)), PathPoint::Uv(Vec2::new(0.5, 0.3)), PathPoint::Uv(Vec2::new(0.9, 0.1))];
        for smooth in [true, false] {
            t.smooth = smooth;
            let c = t.curve();
            for p in &t.points {
                assert!(c.iter().any(|q| q.vec().distance(p.vec()) < 1e-4), "passes through {p:?} (smooth {smooth})");
            }
        }
        t.closed = true;
        let c = t.curve();
        assert!(c.last().unwrap().vec().distance(t.points[0].vec()) < 1e-4, "a closed path ends where it starts");
    }
}
