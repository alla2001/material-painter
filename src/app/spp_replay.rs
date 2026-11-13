//! Replaying what a Substance 3D Painter project recorded, for `.spp` imports: brush strokes
//! (screen positions with the camera of the moment), polygon fills (clicks and rectangles, with
//! the inverse view-projection) and bitmap or uniform mask steps, into Material Painter's
//! layers and masks.
//!
//! How Painter's records read (worked out by SubCraft, `docs/spp-research.md`, MIT; checked on
//! real projects there): screen positions are 0..1 with the origin at the bottom left, through
//! OpenGL clip space; a 3D-view stroke's brush diameter is 1.5 x `brushSize` x the height its
//! view sees; a 2D-view stroke's is 0.15 x `brushSize` of the UV square; polygon-fill
//! rectangles take the visible triangles whose centre is inside, and rectangles under ~1% of
//! the view are clicks.
//!
//! Strokes are laid down with Material Painter's own round brush (Painter's brush tips are
//! Substance graphs): each stamp is drawn over the triangles it reaches, so a replay costs what
//! it paints (`Engine::replay_splats`). Masks take the strokes over their smart mask in
//! Painter's order (`Engine::set_mask_over`).

use super::PainterApp;
use crate::engine::{BrushPaint, MatValues, SplatBatch, SplatVertex, Stamp, StampUniform, MAX_STAMPS};
use crate::polyfill::{FillMode, Topology};
use crate::spp::document::{BrushInfo, DecalHit, StrokeInfo};
use crate::undo::StrokeTarget;
use glam::{Mat4, Vec2, Vec3, Vec4};
use std::ops::Range;
use std::sync::Arc;

/// 3D-view brush diameter per unit of `brushSize` x the view height (SubCraft's calibration).
const SCREEN_BRUSH: f32 = 1.5;
/// 2D-view brush diameter per unit of `brushSize`, in UV units (SubCraft's calibration).
const UV_BRUSH: f32 = 0.15;
/// Polygon-fill rectangles narrower than this (screen units) are clicks.
const MIN_RECT: f32 = 0.01;
/// Splat vertices per GPU batch (besides the stamp count limit).
const MAX_BATCH_VERTICES: usize = 3_000_000;

/// What replays of one texture set share.
pub(super) struct Replay {
    /// Painter's world to the mesh's normalized space, and its scale.
    pub to_mesh: Mat4,
    pub scale: f32,
    /// The texture set's triangles (paint lands only on them).
    pub tris: Range<u32>,
    pub topo: Arc<Topology>,
    /// Polygon-fill padding, in UV units.
    pub pad: f32,
}

/// One stamp found on the surface.
struct Hit {
    pos: Vec3,
    normal: Vec3,
    uv: Vec2,
    uv_scale: f32,
    /// The tip's x axis on the surface.
    x: Vec3,
    radius: f32,
}

/// A ray (origin, direction) through a screen point (0..1, origin bottom left) of a camera
/// given by its inverse view-projection (OpenGL clip space), in Painter's world.
fn painter_ray(inv: &Mat4, p: [f32; 2]) -> (Vec3, Vec3) {
    let ndc = Vec2::new(2.0 * p[0] - 1.0, 2.0 * p[1] - 1.0);
    let un = |z: f32| {
        let v = *inv * Vec4::new(ndc.x, ndc.y, z, 1.0);
        v.truncate() / v.w
    };
    let (a, b) = (un(-1.0), un(1.0));
    (a, (b - a).normalize_or_zero())
}

/// A Painter-space ray in the mesh's space.
fn mesh_ray(r: &Replay, (o, d): (Vec3, Vec3)) -> (Vec3, Vec3) {
    (r.to_mesh.transform_point3(o), r.to_mesh.transform_vector3(d).normalize_or_zero())
}

/// Mirror a 2D-view point (0..1, origin bottom left) through a symmetry matrix.
fn mirror_uv(m: &Mat4, p: [f32; 2]) -> [f32; 2] {
    let q = m.transform_point3(Vec3::new(p[0] * 2.0 - 1.0, p[1] * 2.0 - 1.0, 0.0));
    [q.x * 0.5 + 0.5, q.y * 0.5 + 0.5]
}

/// A stroke and its symmetry copy (3D: the mirrored camera; 2D: the mirrored UV path).
fn with_copies(strokes: &[StrokeInfo]) -> Vec<StrokeInfo> {
    let mut all = strokes.to_vec();
    for s in strokes {
        let Some(d) = s.duplicate else { continue };
        let m = Mat4::from_cols_array(&d);
        let mut c = s.clone();
        c.duplicate = None;
        if s.uv_space {
            c.points = s.points.iter().map(|p| mirror_uv(&m, *p)).collect();
        } else {
            c.view = (Mat4::from_cols_array(&s.view) * m).to_cols_array();
        }
        all.push(c);
    }
    all
}

/// The brush's hardness here: Painter's procedural round tip keeps its own; its other tips
/// (Substance graphs) are replayed with a soft round brush.
fn hardness(brush: &BrushInfo) -> f32 {
    match brush.tip.as_str() {
        "shape" | "shape_v2" => brush.hardness.unwrap_or(0.5).clamp(0.0, 0.99),
        "RoundBrush" | "SquareBrush" => 0.8,
        _ => 0.0,
    }
}

impl PainterApp {
    /// The surface under each stamp of a 3D-view stroke, at the brush's spacing.
    fn stroke_hits(&self, r: &Replay, s: &StrokeInfo, brush: &BrushInfo) -> Vec<Hit> {
        let view = Mat4::from_cols_array(&s.view);
        let proj = Mat4::from_cols_array(&s.projection);
        let inv = (proj * view).inverse();
        if !inv.is_finite() {
            return Vec::new();
        }
        // World size of the whole view, across and up (at the mesh's centre for perspective).
        let orthographic = proj.w_axis.w.abs() > 0.5;
        let depth = if orthographic { 1.0 } else { (-view.transform_point3(r.to_mesh.inverse().transform_point3(Vec3::ZERO)).z).max(1e-6) };
        let span = Vec2::new(2.0 * depth / proj.x_axis.x.abs().max(1e-6), 2.0 * depth / proj.y_axis.y.abs().max(1e-6));
        let mut diameter = SCREEN_BRUSH * brush.size * span.y;
        // A one-point stroke with pressure-driven size is a pen tap: Painter records no pressure,
        // and a tap stamps at the smallest size.
        if brush.pressure_size && s.points.len() == 1 {
            diameter *= brush.pressure_size_min.clamp(0.0, 1.0);
        }
        let radius = 0.5 * diameter * r.scale;
        if radius <= 0.0 || s.points.is_empty() {
            return Vec::new();
        }
        let step = brush.spacing.max(0.02) * diameter;
        let right = r.to_mesh.transform_vector3(view.inverse().transform_vector3(Vec3::X)).normalize_or(Vec3::X);
        let mut samples = vec![s.points[0]];
        for w in s.points.windows(2) {
            let (a, b) = (Vec2::from(w[0]), Vec2::from(w[1]));
            let n = (((b - a) * span).length() / step).ceil().max(1.0) as usize;
            samples.extend((1..=n).map(|k| a.lerp(b, k as f32 / n as f32).to_array()));
        }
        let tangent = |n: Vec3| (right - n * right.dot(n)).try_normalize().unwrap_or_else(|| n.any_orthonormal_vector());
        samples
            .into_iter()
            .filter_map(|p| {
                let (o, d) = mesh_ray(r, painter_ray(&inv, p));
                let hit = self.bvh.raycast(o, d)?;
                let (normal, uv, uv_scale) = super::surface_at(&self.mesh, hit.tri, hit.bary);
                Some(Hit { pos: hit.pos, normal, uv, uv_scale, x: tangent(normal), radius })
            })
            .collect()
    }

    /// Replays a paint (or eraser) action's strokes into layer `idx`'s content or mask; returns
    /// how many stamps landed.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn spp_strokes(&mut self, r: &Replay, idx: usize, target: StrokeTarget, strokes: &[StrokeInfo], brush: &BrushInfo, opacity: f32, erase: bool, mat: MatValues) -> usize {
        if !self.engine.begin_replay(idx, target, mat.enabled, erase) {
            return 0;
        }
        let res = self.engine.res as f32;
        let su = |proj: f32| StampUniform {
            view_proj: Mat4::IDENTITY.to_cols_array_2d(),
            cam_pos: [0.0, 0.0, 0.0, 1.0],
            view: [res, res, 0.0, 0.0],
            view2d: [0.0; 4],
            brush: [0.0, hardness(brush), brush.flow.clamp(0.01, 1.0), 0.0],
            clip: [crate::viewport::NEAR, crate::viewport::FAR, 0.0, 0.0],
            proj: [proj, 1.0, 0.0, 0.0],
        };
        let mut landed = 0;
        let all = with_copies(strokes);
        // Strokes made in the 3D view: discs on the surface, drawn over the triangles they reach.
        let mut batch = SplatBatch::default();
        let mut near = Vec::new();
        for s in all.iter().filter(|s| !s.uv_space) {
            for h in self.stroke_hits(r, s, brush) {
                near.clear();
                let (lo, hi) = (h.pos - Vec3::splat(h.radius), h.pos + Vec3::splat(h.radius));
                self.bvh.query(|a, b| a.cmple(hi).all() && b.cmpge(lo).all(), |t| {
                    if r.tris.contains(&t) {
                        near.push(t);
                    }
                });
                if near.is_empty() {
                    continue;
                }
                let k = batch.stamps.len() as u32;
                let stamp: Stamp = [[0.0, 0.0, 1.0, 0.0], h.pos.extend(h.radius).to_array(), h.normal.extend(h.radius * h.uv_scale).to_array(), h.x.extend(0.0).to_array(), [h.uv.x, h.uv.y, 1.0, 0.0]];
                batch.stamps.push(stamp);
                let g = r.topo.uv_geometry(&near, r.pad);
                batch.faces.extend(g.faces.iter().map(|&uv| SplatVertex { uv, stamp: k }));
                batch.padding.extend(g.padding.iter().map(|&uv| SplatVertex { uv, stamp: k }));
                landed += 1;
                if batch.stamps.len() >= MAX_STAMPS || batch.faces.len() + batch.padding.len() > MAX_BATCH_VERTICES {
                    self.engine.replay_splats(&batch, su(1.0));
                    batch = SplatBatch::default();
                }
            }
        }
        self.engine.replay_splats(&batch, su(1.0));
        // Strokes made in the 2D view: discs in UV.
        let mut batch = SplatBatch::default();
        for s in all.iter().filter(|s| s.uv_space && !s.points.is_empty()) {
            let diameter = UV_BRUSH * brush.size;
            let radius = 0.5 * diameter;
            let step = brush.spacing.max(0.02) * diameter;
            let mut samples = vec![Vec2::from(s.points[0])];
            for w in s.points.windows(2) {
                let (a, b) = (Vec2::from(w[0]), Vec2::from(w[1]));
                let n = ((b - a).length() / step).ceil().max(1.0) as usize;
                samples.extend((1..=n).map(|k| a.lerp(b, k as f32 / n as f32)));
            }
            for p in samples {
                // Painter's UV has v up; Material Painter's rows start at the top.
                let c = Vec2::new(p.x, 1.0 - p.y);
                let k = batch.stamps.len() as u32;
                batch.stamps.push([[0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0], [0.0, 0.0, 1.0, radius], [1.0, 0.0, 0.0, 0.0], [c.x, c.y, 1.0, 0.0]]);
                let (a, b) = (c - Vec2::splat(radius), c + Vec2::splat(radius));
                for uv in [[a.x, a.y], [b.x, a.y], [b.x, b.y], [a.x, a.y], [b.x, b.y], [a.x, b.y]] {
                    batch.faces.push(SplatVertex { uv, stamp: k });
                }
                landed += 1;
                if batch.stamps.len() >= MAX_STAMPS {
                    self.engine.replay_splats(&batch, su(2.0));
                    batch = SplatBatch::default();
                }
            }
        }
        self.engine.replay_splats(&batch, su(2.0));
        self.engine.finish_replay(&BrushPaint { mat, mask_value: brush.value, opacity: opacity.clamp(0.0, 1.0), erase, grain: [0.0; 4] });
        landed
    }

    /// The set's triangles one polygon-fill hit selects.
    fn decal_tris(&self, r: &Replay, h: &DecalHit) -> Vec<u32> {
        let mode = match h.granularity {
            0 => FillMode::Triangle,
            1 => FillMode::Polygon,
            2 => FillMode::Object,
            _ => FillMode::UvIsland,
        };
        let points: Vec<[f32; 2]> = match h.points.as_slice() {
            [a, b] if (a[0] - b[0]).abs() < MIN_RECT || (a[1] - b[1]).abs() < MIN_RECT => vec![[(a[0] + b[0]) * 0.5, (a[1] + b[1]) * 0.5]],
            p => p.to_vec(),
        };
        let centroid = |t: u32| {
            let v = |k: usize| Vec3::from(self.mesh.vertices[self.mesh.indices[t as usize * 3 + k] as usize].pos);
            (v(0) + v(1) + v(2)) / 3.0
        };
        let uv_centroid = |t: u32| {
            let v = |k: usize| Vec2::from(self.mesh.vertices[self.mesh.indices[t as usize * 3 + k] as usize].uv);
            (v(0) + v(1) + v(2)) / 3.0
        };
        let seeds: Vec<u32> = if h.uv_space {
            // Made in the 2D view: points are UVs (v up).
            let uv = |p: [f32; 2]| Vec2::new(p[0], 1.0 - p[1]);
            match points.as_slice() {
                [p] => r.topo.pick_uv(uv(*p)).into_iter().collect(),
                [a, b] => {
                    let (lo, hi) = (uv(*a).min(uv(*b)), uv(*a).max(uv(*b)));
                    r.tris.clone().filter(|&t| {
                        let c = uv_centroid(t);
                        c.cmpge(lo).all() && c.cmple(hi).all()
                    }).collect()
                }
                _ => Vec::new(),
            }
        } else {
            let inv = Mat4::from_cols_array(&h.proj_view_inv);
            match points.as_slice() {
                [p] => {
                    let (o, d) = mesh_ray(r, painter_ray(&inv, *p));
                    if h.through {
                        let mut hits = Vec::new();
                        self.bvh.all_hits(o, d, f32::INFINITY, &mut hits);
                        hits.into_iter().map(|(_, t)| t).collect()
                    } else {
                        self.bvh.raycast(o, d).map(|hit| hit.tri).into_iter().collect()
                    }
                }
                [a, b] => {
                    // The visible triangles whose centre is inside the rectangle.
                    let view_proj = inv.inverse();
                    let from_mesh = r.to_mesh.inverse();
                    let (x0, x1) = (a[0].min(b[0]), a[0].max(b[0]));
                    let (y0, y1) = (a[1].min(b[1]), a[1].max(b[1]));
                    r.tris.clone().filter(|&t| {
                        let c = centroid(t);
                        let clip = view_proj * from_mesh.transform_point3(c).extend(1.0);
                        if clip.w.abs() < 1e-12 {
                            return false;
                        }
                        let s = [clip.x / clip.w * 0.5 + 0.5, clip.y / clip.w * 0.5 + 0.5];
                        if s[0] < x0 || s[0] > x1 || s[1] < y0 || s[1] > y1 {
                            return false;
                        }
                        let (o, d) = mesh_ray(r, painter_ray(&inv, s));
                        self.bvh.raycast(o, d).is_some_and(|hit| hit.tri == t || hit.pos.distance(c) < 0.005)
                    }).collect()
                }
                _ => Vec::new(),
            }
        };
        let mut out: Vec<u32> = seeds.into_iter().flat_map(|t| r.topo.select(mode, t)).filter(|t| r.tris.contains(t)).collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Replays a polygon-fill action into layer `idx`'s content or mask, hit by hit (a later
    /// hit overrides an earlier one); returns how many triangles it filled.
    pub(super) fn spp_decals(&mut self, r: &Replay, idx: usize, target: StrokeTarget, hits: &[DecalHit], mat: MatValues) -> usize {
        let mut all = Vec::with_capacity(hits.len());
        for h in hits {
            all.push(h.clone());
            if let Some(d) = h.duplicate {
                let m = Mat4::from_cols_array(&d);
                let mut c = h.clone();
                if h.uv_space {
                    c.points = h.points.iter().map(|p| mirror_uv(&m, *p)).collect();
                } else {
                    c.proj_view_inv = (m * Mat4::from_cols_array(&h.proj_view_inv)).to_cols_array();
                }
                all.push(c);
            }
        }
        let mut filled = 0;
        // Runs of hits writing the same value go down as one fill.
        let mut k = 0;
        while k < all.len() {
            let value = all[k].value;
            let mut tris = Vec::new();
            while k < all.len() && all[k].value == value {
                tris.extend(self.decal_tris(r, &all[k]));
                k += 1;
            }
            tris.sort_unstable();
            tris.dedup();
            if tris.is_empty() || !self.engine.begin_replay(idx, target, mat.enabled, false) {
                continue;
            }
            self.engine.replay_polys(&r.topo.uv_geometry(&tris, r.pad));
            self.engine.finish_replay(&BrushPaint { mat, mask_value: value, opacity: 1.0, erase: false, grain: [0.0; 4] });
            filled += tris.len();
        }
        filled
    }

    /// Runs `f` over every texel of layer `idx`'s painted mask: (texel uv, multiplier of the
    /// smart mask, value added on top), both 0..1.
    pub(super) fn spp_mask_texels(&mut self, idx: usize, f: impl Fn(Vec2, &mut f32, &mut f32)) {
        let Some(mut texels) = self.engine.mask_texels(idx) else { return };
        let res = self.engine.res as usize;
        for (i, px) in texels.chunks_exact_mut(2).enumerate() {
            let uv = Vec2::new(((i % res) as f32 + 0.5) / res as f32, ((i / res) as f32 + 0.5) / res as f32);
            let (mut m, mut a) = (px[0] as f32 / 255.0, px[1] as f32 / 255.0);
            f(uv, &mut m, &mut a);
            px[0] = (m.clamp(0.0, 1.0) * 255.0).round() as u8;
            px[1] = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
        self.engine.set_mask_texels(idx, &texels);
    }
}
