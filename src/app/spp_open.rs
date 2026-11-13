//! Opening a Substance 3D Painter project (`.spp`) as a new project: reading it on a worker
//! thread (`crate::spp_import`), then the model, the flattened result as a paint layer, the
//! layer stack rebuilt as far as it maps onto Material Painter's layers, a bake, and a report
//! that says plainly what came across and what didn't.
//!
//! The rebuilt stack sits in a hidden folder above the flattened layer: the flattened layer is
//! the project exactly as it looked in Painter; the folder is its structure to edit. Rebuilt:
//! folders, fills of values and images, paint layers and painted masks from Painter's recorded
//! strokes and polygon fills (`spp_replay`), mask steps in Painter's order, smart masks as the
//! closest Material Painter ones. Procedural materials and filters (Substance graphs) can't
//! run here: such layers stay as named, hidden placeholders.

use super::spp_replay::Replay;
use super::{PainterApp, theme};
use crate::engine::{Blend, FillParams, FilterKind, GenKind, MaskBlend, MaskFilter, MaskLayer, MaskSource, MatValues, Projection, CHANNELS};
use crate::spp::document::{ActionInfo, ActionKind, UvTransform};
use crate::spp::{LayerInfo, LayerKind, SourceInfo, channel_bits};
use crate::spp_import::{Imported, ImportedImage};
use crate::undo::StrokeTarget;
use eframe::egui;
use egui::Color32;
use glam::Vec2;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;

/// Name of the folder holding the rebuilt Painter stack.
pub(super) const REBUILT: &str = "Painter layers (rebuilt)";
/// Name of the layer holding Painter's flattened result.
pub(super) const FLATTENED: &str = "Painter result (flattened)";

#[derive(Default)]
pub(super) struct SppOpen {
    job: Option<(PathBuf, mpsc::Receiver<Result<Imported, String>>)>,
    pub report: Option<Report>,
}

pub(super) struct Report {
    name: String,
    came: Vec<String>,
    left: Vec<String>,
    stack: Rebuilt,
}

/// What rebuilding the stack made.
#[derive(Default)]
pub(super) struct Rebuilt {
    folders: usize,
    fills: usize,
    /// Fills with images from the project.
    images: usize,
    paint_layers: usize,
    /// Painted strokes replayed (stamps laid down), and the actions they came in.
    stamps: usize,
    stroke_actions: usize,
    /// Polygon-fill actions replayed, and the triangles they filled.
    poly_fills: usize,
    /// Fills and masks made from something only close to Painter's (smart masks, partial fills).
    approximate: usize,
    masks: usize,
    /// Layers kept only as hidden, empty placeholders, by name and why.
    placeholders: Vec<(String, &'static str)>,
    /// Mask steps that couldn't be rebuilt.
    mask_steps_left: usize,
    /// Content steps of paint layers that couldn't be rebuilt.
    content_steps_left: usize,
    other_blends: usize,
}

impl Rebuilt {
    /// Adds another texture set's counts.
    fn add(&mut self, o: Rebuilt) {
        self.folders += o.folders;
        self.fills += o.fills;
        self.images += o.images;
        self.paint_layers += o.paint_layers;
        self.stamps += o.stamps;
        self.stroke_actions += o.stroke_actions;
        self.poly_fills += o.poly_fills;
        self.approximate += o.approximate;
        self.masks += o.masks;
        self.placeholders.extend(o.placeholders);
        self.mask_steps_left += o.mask_steps_left;
        self.content_steps_left += o.content_steps_left;
        self.other_blends += o.other_blends;
    }
}

fn linear_to_srgb(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

/// Painter's blending mode id as Material Painter's (ids as found by SubCraft); `None` when
/// there is no close one.
fn blend_of(mode: i32) -> Option<Blend> {
    Some(match mode {
        25 => Blend::Normal,
        4 => Blend::Multiply,
        29 => Blend::Subtract,
        2 | 11 | 15 => Blend::Overlay,
        _ => return None,
    })
}

/// Painter's channel flags and Material Painter's channel for each.
const CHANNEL_OF: [(u64, usize); 7] = [
    (channel_bits::BASE_COLOR, 0),
    (channel_bits::ROUGHNESS, 1),
    (channel_bits::METALLIC, 2),
    (channel_bits::HEIGHT, 3),
    (channel_bits::OPACITY, crate::engine::OPACITY),
    (channel_bits::EMISSIVE, crate::engine::EMISSION),
    (channel_bits::NORMAL, crate::engine::NORMAL),
];

/// Uniform sources as material values (channels the sources write enabled).
fn uniform_mat(sources: &[SourceInfo]) -> MatValues {
    let mut mat = MatValues { enabled: [false; CHANNELS], ..MatValues::default() };
    for s in sources {
        let SourceInfo::Uniform { channels, color } = s else { continue };
        for &(bit, ch) in &CHANNEL_OF {
            if channels & bit == 0 {
                continue;
            }
            mat.enabled[ch] = true;
            match ch {
                // Painter keeps uniform colors linear; Material Painter's are sRGB.
                0 => mat.color = color.map(linear_to_srgb),
                1 => mat.rough = color[0],
                2 => mat.metal = color[0],
                3 => mat.height = color[0],
                4 => mat.opacity = color[0],
                5 => mat.emission = color.map(linear_to_srgb),
                // A uniform normal is flat: nothing to add.
                _ => mat.enabled[ch] = false,
            }
        }
    }
    mat
}

/// A fill layer from a layer's fill sources: uniform values and images (`None` when it has
/// neither), the images per channel, and whether other sources (procedural materials) were
/// left out.
fn fill_of(l: &LayerInfo) -> Option<(FillParams, Vec<(usize, String)>, bool)> {
    let mut mat = uniform_mat(&l.sources);
    let mut images = Vec::new();
    let mut partial = false;
    for s in &l.sources {
        match s {
            SourceInfo::Uniform { .. } => {}
            SourceInfo::Bitmap { channels, url } => {
                for &(bit, ch) in &CHANNEL_OF {
                    if channels & bit != 0 {
                        mat.enabled[ch] = true;
                        images.push((ch, url.clone()));
                    }
                }
            }
            _ => partial = true,
        }
    }
    if !mat.enabled.iter().any(|&e| e) {
        return None;
    }
    let mut fill = FillParams { mat, ..FillParams::default() };
    // The image placement of the (first) fill action.
    if let Some(a) = l.content.iter().find(|a| matches!(a.kind, ActionKind::Fill { .. })) {
        if let ActionKind::Fill { projection, .. } = a.kind {
            fill.projection = if projection == 2 { Projection::Triplanar } else { Projection::Uv };
        }
        if let Some(t) = &a.uv {
            place_image(&mut fill, t);
        }
    }
    Some((fill, images, partial))
}

/// Painter's image placement on a fill. In Painter's UV (v up) the image's centre sits at
/// 0.5 + translation and the image is scaled and rotated (radians) around it (SubCraft's
/// measurements); in Material Painter's (v down) that is a rotation the other way and an
/// offset of -scale x the translation turned and flipped. Uneven scales take their mean.
fn place_image(fill: &mut FillParams, t: &UvTransform) {
    let s = 0.5 * (t.scale[0] + t.scale[1]);
    let (sin, cos) = t.rotation.sin_cos();
    let turned = Vec2::new(t.offset[0] * cos - t.offset[1] * sin, t.offset[0] * sin + t.offset[1] * cos);
    fill.tiling = s.max(1e-3);
    fill.rotation = -t.rotation.to_degrees();
    fill.offset = [-s * turned.x, s * turned.y];
}

/// Where Painter samples a fill's image for texel UV `uv` (v down): see [`place_image`];
/// image coordinates come back with v down too.
fn painter_image_uv(t: &UvTransform, uv: Vec2) -> Vec2 {
    let (sin, cos) = t.rotation.sin_cos();
    let p = Vec2::new(uv.x, 1.0 - uv.y);
    let d = (p - Vec2::splat(0.5) - Vec2::from(t.offset)) * Vec2::from(t.scale);
    let q = Vec2::new(d.x * cos - d.y * sin, d.x * sin + d.y * cos) + Vec2::splat(0.5);
    Vec2::new(q.x, 1.0 - q.y)
}

/// The brightness of an image at `q` (wrapping), bilinear.
fn sample_luma(img: &ImportedImage, q: Vec2) -> f32 {
    let (w, h) = (img.width as i64, img.height as i64);
    let x = q.x * w as f32 - 0.5;
    let y = q.y * h as f32 - 0.5;
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = (x - x0, y - y0);
    let at = |dx: i64, dy: i64| {
        let px = (x0 as i64 + dx).rem_euclid(w) as usize;
        let py = (y0 as i64 + dy).rem_euclid(h) as usize;
        let p = &img.rgba[(py * w as usize + px) * 4..][..4];
        (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32) / 255.0
    };
    let top = at(0, 0) * (1.0 - fx) + at(1, 0) * fx;
    let bottom = at(0, 1) * (1.0 - fx) + at(1, 1) * fx;
    top * (1.0 - fy) + bottom * fy
}

/// A Painter smart-mask graph as a generator preset (approximately), by its name.
fn generator_of(name: &str) -> Option<GenKind> {
    Some(match name {
        "mg_dirt" | "mg_ambient_occlusion" | "mg_mask_editor" | "mg_mask_builder" => GenKind::Dirt,
        "mg_curvature" => GenKind::EdgeWear,
        "mg_position" => GenKind::PositionGradient,
        // Light from above, facing up: what Dust reads.
        "light" | "mg_light" | "mg_world_space_normals" => GenKind::Dust,
        "mg_thickness" => GenKind::ThinAreas,
        n if n.contains("edge_wear") => GenKind::EdgeWear,
        n if n.contains("dust") => GenKind::Dust,
        n if n.contains("leak") || n.contains("drip") => GenKind::Drips,
        n if n.contains("noise") || n.contains("grunge") || n.contains("fractal") || n.contains("spots") => GenKind::Clouds,
        _ => return None,
    })
}

/// A mask step's own blending: Painter's mode id and opacity (Normal, opaque by default).
fn own_blend(a: &ActionInfo) -> (i32, f32) {
    a.own_blends.first().map_or((25, 1.0), |&(_, mode, opacity)| (mode, opacity.clamp(0.0, 1.0)))
}

/// A mask step that covers everything below it: an opaque Normal fill of one value, a smart
/// mask or a tiling image.
fn covers_all(a: &ActionInfo) -> bool {
    let (mode, opacity) = own_blend(a);
    let ActionKind::Fill { sources, .. } = &a.kind else { return false };
    mode == 25
        && opacity > 0.999
        && sources.iter().any(|s| match s {
            SourceInfo::Uniform { .. } | SourceInfo::Procedural { .. } => true,
            SourceInfo::Bitmap { .. } => a.uv.is_none_or(|t| t.tiling),
            SourceInfo::Reference { .. } => false,
        })
}

impl PainterApp {
    /// Asks for a `.spp` (unless given) and reads it on a worker thread.
    pub(super) fn start_spp_open(&mut self, path: Option<PathBuf>, ctx: &egui::Context) {
        let Some(path) = path.or_else(|| rfd::FileDialog::new().add_filter("Substance 3D Painter project", &["spp"]).pick_file()) else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        let (p, ctx) = (path.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(crate::spp_import::import(&p));
            ctx.request_repaint();
        });
        self.status = format!("Opening {}…", path.display());
        self.spp.job = Some((path, rx));
    }

    /// Picks up a finished read; call every frame.
    pub(super) fn poll_spp_open(&mut self, ctx: &egui::Context) {
        let Some((path, rx)) = &self.spp.job else { return };
        match rx.try_recv() {
            Ok(Ok(imported)) => {
                let path = path.clone();
                self.spp.job = None;
                self.apply_spp(&path, imported, ctx);
            }
            Ok(Err(e)) => {
                self.status = e;
                self.spp.job = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.status = "Reading the Painter project stopped unexpectedly.".into();
                self.spp.job = None;
            }
        }
    }

    pub(super) fn spp_busy(&self) -> bool {
        self.spp.job.is_some()
    }

    /// A new project from an imported Painter project: the model, then each texture set's
    /// size, flattened result and rebuilt stack.
    fn apply_spp(&mut self, path: &std::path::Path, imported: Imported, ctx: &egui::Context) {
        let Imported { mesh, sets, came, left, to_mesh, mesh_scale, images } = imported;
        self.graphs.clear(&self.rs);
        self.apply_project_defaults();
        let sizes: Vec<u32> = sets.iter().map(|s| s.resolution).collect();
        self.load_mesh_sized(mesh, ctx, &sizes);
        let mut stack = Rebuilt::default();
        // Recorded strokes go over the smart masks below them, as in Painter.
        self.engine.set_mask_over(true);
        let topo = self.poly_topology();
        for (k, set) in sets.into_iter().enumerate() {
            self.switch_set(k);
            self.engine.set_mask_over(true);
            while !self.engine.layers.is_empty() {
                self.engine.delete_layer(0, &mut self.undo);
            }
            if self.engine.res != set.resolution {
                self.engine.set_resolution(set.resolution, &self.gpu_mesh, &mut self.undo);
            }
            let flat = self.engine.add_paint_layer_from(0, None, FLATTENED.into(), set.channels, &mut self.undo);
            let replay = Replay {
                to_mesh,
                scale: mesh_scale,
                tris: self.mesh.sets.get(k).map_or(0..0, |s| s.tris.clone()),
                topo: topo.clone(),
                pad: self.prefs.poly_padding / self.engine.res as f32,
            };
            stack.add(self.rebuild_spp_stack(&set.layers, &replay, &images));
            self.engine.set_mask_over(false);
            self.selected = self.engine.layers.iter().position(|l| l.name == FLATTENED).unwrap_or(flat);
            self.target = StrokeTarget::Content;
        }
        self.switch_set(0);
        self.undo.clear();
        self.saved_serial = self.undo.state_serial();
        self.project_path = None;
        // Every set at its own size (set above), so after the sizes.
        self.start_bake_all(ctx);
        let name = path.file_name().map_or_else(|| self.mesh.name.clone(), |n| n.to_string_lossy().into_owned());
        self.status = format!("Opened {name} from Substance 3D Painter: see the report for what came across.");
        self.spp.report = Some(Report { name, came, left, stack });
    }

    /// Rebuilds Painter's layers (bottom to top) in a hidden folder on top of the stack.
    fn rebuild_spp_stack(&mut self, layers: &[LayerInfo], r: &Replay, images: &HashMap<String, ImportedImage>) -> Rebuilt {
        let mut st = Rebuilt::default();
        if layers.is_empty() {
            return st;
        }
        let at = self.engine.layers.len();
        let folder = self.engine.add_folder(at, None, REBUILT.into(), &mut self.undo);
        let folder = self.add_spp_layers(folder, layers, r, images, &mut st);
        self.engine.layers[folder].visible = false;
        self.engine.commit_props(&mut self.undo);
        st
    }

    /// Adds Painter layers into the folder at `folder`; returns the folder's index after.
    fn add_spp_layers(&mut self, mut folder: usize, layers: &[LayerInfo], r: &Replay, images: &HashMap<String, ImportedImage>, st: &mut Rebuilt) -> usize {
        let fid = self.engine.layers[folder].id;
        for l in layers {
            // Each layer goes on top of the folder's contents, right below the folder.
            let idx = match l.kind {
                LayerKind::Group => {
                    let child = self.engine.add_folder(folder, Some(fid), l.name.clone(), &mut self.undo);
                    st.folders += 1;
                    self.add_spp_layers(child, &l.children, r, images, st)
                }
                LayerKind::Fill => match fill_of(l) {
                    Some((fill, fill_images, partial)) => {
                        st.fills += 1;
                        if partial {
                            st.approximate += 1;
                        }
                        let idx = self.engine.add_fill_layer(folder, Some(fid), l.name.clone(), fill, &mut self.undo);
                        for (ch, url) in fill_images {
                            let Some(img) = images.get(&url) else { continue };
                            match self.engine.decode_channel_image(ch, format!("{}.png", l.name), img.png.clone()) {
                                Ok(im) => {
                                    self.engine.set_fill_image(idx, ch, Some(im), &mut self.undo);
                                    st.images += 1;
                                }
                                Err(_) => st.approximate += 1,
                            }
                        }
                        idx
                    }
                    None => self.spp_placeholder(folder, fid, l, "procedural material", st),
                },
                LayerKind::Paint => {
                    let idx = self.engine.add_paint_layer(folder, Some(fid), Some(l.name.clone()), &mut self.undo);
                    self.rebuild_paint(r, idx, l, st);
                    st.paint_layers += 1;
                    idx
                }
                LayerKind::Filter => self.spp_placeholder(folder, fid, l, "filter", st),
                LayerKind::Empty => self.spp_placeholder(folder, fid, l, "empty in Painter", st),
            };
            folder = idx + 1;
            let layer = &mut self.engine.layers[idx];
            if layer.visible {
                layer.visible = l.visible;
            }
            if let Some(&(_, mode, opacity)) = l.blends.first() {
                layer.opacity = opacity.clamp(0.0, 1.0);
                match blend_of(mode) {
                    Some(b) => layer.blend = b,
                    None => st.other_blends += 1,
                }
            }
            self.rebuild_mask(r, idx, l, images, st);
            self.spp_mesh_filter(r, idx, l);
        }
        folder
    }

    /// A paint layer's recorded content: strokes, erasers, uniform fills (over the whole set).
    fn rebuild_paint(&mut self, r: &Replay, idx: usize, l: &LayerInfo, st: &mut Rebuilt) {
        for a in l.content.iter().filter(|a| a.enabled) {
            match &a.kind {
                ActionKind::Paint { strokes, brush: Some(brush), opacity } => {
                    let mat = uniform_mat(&brush.sources);
                    if mat.enabled.iter().any(|&e| e) {
                        st.stamps += self.spp_strokes(r, idx, StrokeTarget::Content, strokes, brush, *opacity, false, mat);
                        st.stroke_actions += 1;
                    } else {
                        st.content_steps_left += 1;
                    }
                }
                ActionKind::Eraser { strokes, brush: Some(brush), opacity } => {
                    let mat = MatValues { enabled: [true; CHANNELS], ..MatValues::default() };
                    st.stamps += self.spp_strokes(r, idx, StrokeTarget::Content, strokes, brush, *opacity, true, mat);
                    st.stroke_actions += 1;
                }
                ActionKind::Fill { sources, .. } if sources.iter().all(|s| matches!(s, SourceInfo::Uniform { .. })) => {
                    let mat = uniform_mat(sources);
                    if self.spp_fill_set(r, idx, StrokeTarget::Content, mat, 1.0) {
                        st.poly_fills += 1;
                    }
                }
                _ => st.content_steps_left += 1,
            }
        }
    }

    /// Paints `mat` (or, into a mask, `value`) over every triangle of the set.
    fn spp_fill_set(&mut self, r: &Replay, idx: usize, target: StrokeTarget, mat: MatValues, value: f32) -> bool {
        let tris: Vec<u32> = r.tris.clone().collect();
        if tris.is_empty() || !self.engine.begin_replay(idx, target, mat.enabled, false) {
            return false;
        }
        self.engine.replay_polys(&r.topo.uv_geometry(&tris, r.pad));
        self.engine.finish_replay(&crate::engine::BrushPaint { mat, mask_value: value, opacity: 1.0, erase: false, grain: [0.0; 4] });
        true
    }

    /// A layer's mask, step by step in Painter's order: its start (a value, a smart mask or an
    /// image), then strokes, polygon fills, values and images laid over it (the painted mask
    /// keeps both what scales the smart mask and what goes over it), Levels and blurs as mask
    /// filters (after everything), further smart masks as mask layers.
    fn rebuild_mask(&mut self, r: &Replay, idx: usize, l: &LayerInfo, images: &HashMap<String, ImportedImage>, st: &mut Rebuilt) {
        if !l.has_mask || !l.mask_enabled {
            return;
        }
        let steps: Vec<&ActionInfo> = l.mask.iter().filter(|a| a.enabled).collect();
        // Steps under one that covers everything don't show.
        let first = steps.iter().rposition(|a| covers_all(a));
        let (white, generator) = match first.map(|k| &steps[k].kind) {
            Some(ActionKind::Fill { sources, .. }) => {
                let graph = sources.iter().find_map(|s| match s {
                    SourceInfo::Procedural { graph, .. } => Some(graph),
                    _ => None,
                });
                match graph {
                    Some(graph) => match generator_of(graph.name()) {
                        Some(kind) => {
                            let mut g = kind.default_generator();
                            g.invert = ["invert", "global_invert"].iter().any(|k| graph.value(k).is_some_and(|v| v > 0.5));
                            st.approximate += 1;
                            (true, Some(g))
                        }
                        // A smart mask with no counterpart: the mask starts open.
                        None => {
                            st.mask_steps_left += 1;
                            (true, None)
                        }
                    },
                    None => (l.mask_initial_white, None),
                }
            }
            _ => (l.mask_initial_white, None),
        };
        self.engine.add_mask(idx, white, generator, &mut self.undo);
        self.engine.ensure_mask_paint(idx);
        st.masks += 1;
        // The covering step itself, when it is a value or an image.
        let from = match first {
            Some(k) => {
                if let ActionKind::Fill { sources, .. } = &steps[k].kind {
                    if generator.is_none() {
                        self.spp_mask_fill(idx, steps[k], sources, images, true, st);
                    }
                }
                k + 1
            }
            None => 0,
        };
        let mut filters = Vec::new();
        for a in &steps[from..] {
            match &a.kind {
                ActionKind::Paint { strokes, brush: Some(brush), opacity } => {
                    st.stamps += self.spp_strokes(r, idx, StrokeTarget::Mask, strokes, brush, *opacity, false, MatValues::default());
                    st.stroke_actions += 1;
                }
                ActionKind::Eraser { strokes, brush: Some(brush), opacity } => {
                    st.stamps += self.spp_strokes(r, idx, StrokeTarget::Mask, strokes, brush, *opacity, true, MatValues::default());
                    st.stroke_actions += 1;
                }
                ActionKind::Decal { hits } => {
                    if self.spp_decals(r, idx, StrokeTarget::Mask, hits, MatValues::default()) > 0 {
                        st.poly_fills += 1;
                    }
                }
                ActionKind::Fill { sources, .. } => self.spp_mask_fill(idx, a, sources, images, false, st),
                ActionKind::Levels(lv) => {
                    let mut f = MaskFilter::new(FilterKind::Levels);
                    (f.a, f.b, f.c) = (lv.in_min[0], lv.in_max[0], lv.gamma[0]);
                    filters.push(f);
                }
                ActionKind::Filter { graph } if graph.name().contains("blur") => {
                    filters.push(MaskFilter::new(FilterKind::Blur));
                    st.approximate += 1;
                }
                ActionKind::Anchor { .. } => {}
                _ => st.mask_steps_left += 1,
            }
        }
        if let Some(m) = self.engine.layers[idx].mask.as_mut() {
            m.filters = filters;
        }
    }

    /// One fill step of a mask: values and images go into the painted mask (in Painter's order,
    /// through its blending), smart masks become the mask's own (when it has none yet) or a
    /// mask layer on top.
    #[allow(clippy::too_many_arguments)]
    fn spp_mask_fill(&mut self, idx: usize, a: &ActionInfo, sources: &[SourceInfo], images: &HashMap<String, ImportedImage>, covering: bool, st: &mut Rebuilt) {
        let (mode, opacity) = if covering { (25, 1.0) } else { own_blend(a) };
        if !matches!(mode, 25 | 4 | 10 | 29) {
            st.other_blends += 1;
        }
        for s in sources {
            match s {
                SourceInfo::Uniform { color, .. } => {
                    let v = color[0];
                    self.spp_mask_texels(idx, |_, m, add| match mode {
                        4 | 10 => {
                            let k = 1.0 + (v - 1.0) * opacity;
                            *m *= k;
                            *add *= k;
                        }
                        29 => *add -= v * opacity,
                        _ => {
                            *m *= 1.0 - opacity;
                            *add = *add * (1.0 - opacity) + v * opacity;
                        }
                    });
                }
                SourceInfo::Bitmap { url, .. } => {
                    let Some(img) = images.get(url) else {
                        st.mask_steps_left += 1;
                        continue;
                    };
                    if matches!(&a.kind, ActionKind::Fill { projection, .. } if *projection == 2) {
                        // Tri-planar images come in as UV images.
                        st.approximate += 1;
                    }
                    let t = a.uv.unwrap_or(UvTransform { scale: [1.0; 2], rotation: 0.0, offset: [0.0; 2], tiling: true });
                    self.spp_mask_texels(idx, |uv, m, add| {
                        let q = painter_image_uv(&t, uv);
                        if !t.tiling && !(0.0..=1.0).contains(&q.x) || !t.tiling && !(0.0..=1.0).contains(&q.y) {
                            return;
                        }
                        let v = sample_luma(img, q);
                        match mode {
                            4 | 10 => {
                                let k = 1.0 + (v - 1.0) * opacity;
                                *m *= k;
                                *add *= k;
                            }
                            29 => *add -= v * opacity,
                            _ => {
                                *m *= 1.0 - opacity;
                                *add = *add * (1.0 - opacity) + v * opacity;
                            }
                        }
                    });
                    st.images += 1;
                }
                SourceInfo::Procedural { graph, .. } => {
                    let Some(kind) = generator_of(graph.name()) else {
                        st.mask_steps_left += 1;
                        continue;
                    };
                    let mut g = kind.default_generator();
                    g.invert = ["invert", "global_invert"].iter().any(|k| graph.value(k).is_some_and(|v| v > 0.5));
                    st.approximate += 1;
                    let Some(mask) = self.engine.layers[idx].mask.as_mut() else { continue };
                    if mask.generator.is_none() && matches!(mode, 4 | 10) {
                        mask.generator = Some(g);
                    } else {
                        let blend = match mode {
                            4 | 10 => MaskBlend::Multiply,
                            29 => MaskBlend::Subtract,
                            _ => MaskBlend::Normal,
                        };
                        mask.layers.push(MaskLayer { source: MaskSource::Generator(g), blend, opacity, enabled: true });
                    }
                    self.engine.mark_dirty();
                }
                SourceInfo::Reference { .. } => st.mask_steps_left += 1,
            }
        }
    }

    /// Painter's geometry mask (a layer only on some of the model's objects): the other
    /// objects' triangles are masked out.
    fn spp_mesh_filter(&mut self, r: &Replay, idx: usize, l: &LayerInfo) {
        let Some((default_on, list)) = &l.mesh_filter else { return };
        let on = |name: &str| list.iter().any(|n| n == name) != *default_on;
        let objects = &self.mesh.objects;
        let off: Vec<u32> = r
            .tris
            .clone()
            .filter(|&t| self.mesh.tri_object.get(t as usize).and_then(|&o| objects.get(o as usize)).is_some_and(|n| !on(n)))
            .collect();
        if off.is_empty() {
            return;
        }
        if self.engine.layers[idx].mask.is_none() {
            self.engine.add_mask(idx, true, None, &mut self.undo);
        }
        if self.engine.begin_replay(idx, StrokeTarget::Mask, [false; CHANNELS], false) {
            self.engine.replay_polys(&r.topo.uv_geometry(&off, r.pad));
            self.engine.finish_replay(&crate::engine::BrushPaint { mat: MatValues::default(), mask_value: 0.0, opacity: 1.0, erase: false, grain: [0.0; 4] });
        }
    }

    /// A hidden, empty layer standing for one that can't be rebuilt, named after it.
    fn spp_placeholder(&mut self, folder: usize, fid: u64, l: &LayerInfo, why: &'static str, st: &mut Rebuilt) -> usize {
        let idx = self.engine.add_paint_layer(folder, Some(fid), Some(format!("{} (not rebuilt: {why})", l.name)), &mut self.undo);
        self.engine.layers[idx].visible = false;
        st.placeholders.push((l.name.clone(), why));
        idx
    }

    /// The report of an import: what came across, what was rebuilt, what wasn't.
    pub(super) fn spp_report_window(&mut self, ctx: &egui::Context) {
        let Some(r) = &self.spp.report else { return };
        let mut open = true;
        let mut close = false;
        egui::Window::new("Opened from Substance 3D Painter")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(520.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(egui::RichText::new(&r.name).strong().color(Color32::WHITE));
                ui.label(
                    egui::RichText::new(format!(
                        "The layer \"{FLATTENED}\" shows the project as it looked in Painter. To edit its structure instead, show the folder \"{REBUILT}\" and hide the flattened layer."
                    ))
                    .color(theme::TEXT),
                );
                ui.add_space(6.0);
                egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
                    let green = Color32::from_rgb(120, 210, 140);
                    let amber = Color32::from_rgb(240, 190, 90);
                    ui.label(egui::RichText::new("Came across").strong().color(green));
                    for line in &r.came {
                        ui.label(format!("✓ {line}"));
                    }
                    let s = &r.stack;
                    let n = |k: usize, one: &str, many: &str| format!("{k} {}", if k == 1 { one } else { many });
                    if s.folders + s.fills + s.paint_layers + s.placeholders.len() > 0 {
                        ui.label(format!(
                            "✓ The layer stack, rebuilt: {}, {}, {}, {}{}.",
                            n(s.folders, "folder", "folders"),
                            n(s.fills, "fill", "fills"),
                            n(s.paint_layers, "paint layer", "paint layers"),
                            n(s.masks, "mask", "masks"),
                            if s.approximate > 0 { format!(" ({} only approximately)", n(s.approximate, "part", "parts")) } else { String::new() },
                        ));
                    }
                    if s.stroke_actions > 0 {
                        ui.label(format!(
                            "✓ Brush strokes replayed from Painter's records: {} ({} stamps), with a round brush (Painter's brush tips are Substance graphs).",
                            n(s.stroke_actions, "paint action", "paint actions"),
                            s.stamps
                        ));
                    }
                    if s.poly_fills > 0 {
                        ui.label(format!("✓ Polygon fills replayed: {}.", n(s.poly_fills, "fill", "fills")));
                    }
                    if s.images > 0 {
                        ui.label(format!("✓ Images from the project on fills and masks: {}.", s.images));
                    }
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("Not imported").strong().color(amber));
                    for line in &r.left {
                        ui.label(format!("• {line}"));
                    }
                    if !s.placeholders.is_empty() {
                        ui.label(format!("• {} layer{} kept only as hidden, empty placeholders with their names:", s.placeholders.len(), if s.placeholders.len() == 1 { "" } else { "s" }));
                        for (name, why) in s.placeholders.iter().take(20) {
                            ui.label(egui::RichText::new(format!("    {name} ({why})")).small().color(theme::TEXT_DIM));
                        }
                        if s.placeholders.len() > 20 {
                            ui.label(egui::RichText::new(format!("    and {} more", s.placeholders.len() - 20)).small().color(theme::TEXT_DIM));
                        }
                    }
                    if s.mask_steps_left > 0 {
                        ui.label(format!("• {} (Substance filters and generators with no counterpart, anchor references) not rebuilt.", n(s.mask_steps_left, "mask step", "mask steps")));
                    }
                    if s.content_steps_left > 0 {
                        ui.label(format!("• {} of paint layers (filters, image or procedural steps) not rebuilt.", n(s.content_steps_left, "step", "steps")));
                    }
                    if s.other_blends > 0 {
                        ui.label(format!("• {} blend mode{} with no equivalent here, set to Normal.", s.other_blends, if s.other_blends == 1 { "" } else { "s" }));
                    }
                });
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(
                        "Material Painter is not affiliated with or endorsed by Adobe; Substance 3D Painter is Adobe's trademark. \
                         Only your own project's data is read: the contents of Substance (.sbsar) materials are never opened. \
                         Reading .spp files is adapted from SubCraft (github.com/Aalzard/subcraft, MIT).",
                    )
                    .small()
                    .color(theme::TEXT_DIM),
                );
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
        if !open || close {
            self.spp.report = None;
        }
    }
}

#[cfg(test)]
impl PainterApp {
    /// Opens a Painter project (offscreen tests).
    pub(crate) fn test_open_spp(&mut self, path: &std::path::Path, ctx: &egui::Context) {
        self.start_spp_open(Some(path.to_path_buf()), ctx);
    }

    /// The import finished: its report is up (offscreen tests).
    pub(crate) fn test_spp_done(&self) -> bool {
        self.spp.job.is_none() && self.spp.report.is_some()
    }

    /// The rebuilt stack alone against Painter's flattened result, per texture set: mean
    /// absolute difference (0..1) of base color, roughness and metallic over the texels the
    /// flattened result covers, and a picture of both base colors side by side in `dir`
    /// (offscreen tests).
    pub(crate) fn test_spp_compare(&mut self, dir: &std::path::Path) -> Vec<(String, [f32; 3])> {
        let mut out = Vec::new();
        let active = self.sets.active;
        for k in 0..self.mesh.sets.len() {
            self.switch_set(k);
            let (Some(flat), Some(rebuilt)) = (self.engine.layers.iter().position(|l| l.name == FLATTENED), self.engine.layers.iter().position(|l| l.name == REBUILT)) else { continue };
            let read = |app: &mut Self, show_rebuilt: bool| {
                app.engine.layers[flat].visible = !show_rebuilt;
                app.engine.layers[rebuilt].visible = show_rebuilt;
                app.engine.mark_dirty();
                app.engine.composite();
                [0, 1, 2].map(|c| app.engine.read_tex(app.engine.final_tex(c)))
            };
            let painter = read(self, false);
            let ours = read(self, true);
            self.engine.layers[flat].visible = true;
            self.engine.layers[rebuilt].visible = false;
            self.engine.mark_dirty();
            let covered = match &self.engine.layers[flat].content {
                crate::engine::Content::Paint(ch) => ch[0].as_ref().map(|t| self.engine.read_tex(t)),
                _ => None,
            };
            let Some(covered) = covered else { continue };
            let n = (self.engine.res * self.engine.res) as usize;
            let mut sum = [0.0f64; 3];
            let mut count = 0usize;
            for i in 0..n {
                if covered[i * 4 + 3] == 0 {
                    continue;
                }
                count += 1;
                let d = |a: u8, b: u8| (a as f64 - b as f64).abs() / 255.0;
                sum[0] += (0..3).map(|c| d(painter[0][i * 4 + c], ours[0][i * 4 + c])).sum::<f64>() / 3.0;
                sum[1] += d(painter[1][i], ours[1][i]);
                sum[2] += d(painter[2][i], ours[2][i]);
            }
            let mean = sum.map(|v| (v / count.max(1) as f64) as f32);
            let res = self.engine.res;
            let side = |px: &[u8]| image::imageops::resize(&image::RgbaImage::from_raw(res, res, px.to_vec()).unwrap(), 1024.min(res), 1024.min(res), image::imageops::FilterType::Triangle);
            let (a, b) = (side(&painter[0]), side(&ours[0]));
            let mut both = image::RgbaImage::new(a.width() * 2, a.height());
            image::imageops::replace(&mut both, &a, 0, 0);
            image::imageops::replace(&mut both, &b, a.width() as i64, 0);
            let _ = both.save(dir.join(format!("compare_{}.png", self.mesh.sets[k].name)));
            out.push((self.mesh.sets[k].name.clone(), mean));
        }
        self.switch_set(active);
        out
    }

    /// Names, kinds and visibility of the layers, bottom to top (offscreen tests).
    pub(crate) fn test_layers(&self) -> Vec<(String, &'static str, bool)> {
        use crate::engine::Content;
        self.engine
            .layers
            .iter()
            .map(|l| {
                let kind = match &l.content {
                    Content::Paint(ch) if ch.iter().any(Option::is_some) => "paint",
                    Content::Paint(_) => "empty paint",
                    Content::Fill(_) => "fill",
                    Content::Folder(_) => "folder",
                };
                (l.name.clone(), kind, l.visible)
            })
            .collect()
    }
}
