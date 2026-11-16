//! The stencil: an image fixed on the screen that the brush paints through, like a
//! physical stencil held over the model. Where the image lets paint pass is its alpha,
//! brightness or one color channel; with "Paint its colors" the brush lays the image's
//! own colors down, for decals and logos. Hold S over a view to place it: drag to move,
//! right-drag to scale (up and down) and turn (left and right).

use super::{PainterApp, ViewKind, labeled, section, slider, theme};
use crate::engine::{ImageSource, Stencil};
use crate::gpu::Tex;
use eframe::{egui, wgpu};
use egui::{Color32, Rect, Stroke, pos2, vec2};
use glam::Vec2;
use std::sync::Arc;

/// Largest stencil edge kept; bigger images are scaled down.
const MAX_EDGE: u32 = 4096;

struct StencilImage {
    name: String,
    file: image::RgbaImage,
    tex: Arc<Tex>,
    handle: egui::TextureHandle,
}

pub(super) struct StencilState {
    image: Option<StencilImage>,
    /// Paint goes through it (off: the image stays loaded, out of the way).
    pub on: bool,
    /// The brush paints the image's colors (decals).
    pub colors: bool,
    /// Which part of the image lets paint through.
    source: ImageSource,
    /// Center, as a fraction of the view's width and height.
    center: Vec2,
    /// Height, as a fraction of the view's height.
    scale: f32,
    /// Degrees, clockwise.
    angle: f32,
    /// How strongly the image shows over the view.
    overlay: f32,
}

impl Default for StencilState {
    fn default() -> Self {
        StencilState {
            image: None,
            on: true,
            colors: false,
            source: ImageSource::Auto,
            center: Vec2::splat(0.5),
            scale: 0.6,
            angle: 0.0,
            overlay: 0.35,
        }
    }
}

impl StencilState {
    fn active(&self) -> Option<&StencilImage> {
        self.image.as_ref().filter(|_| self.on)
    }

    fn aspect(&self) -> f32 {
        self.image.as_ref().map_or(1.0, |i| i.file.width() as f32 / i.file.height().max(1) as f32)
    }

    /// The image's corners and half size in a view of `size` (any unit), centered at `c`.
    fn frame(&self, size: Vec2) -> (Vec2, Vec2, f32) {
        let half_h = self.scale * size.y * 0.5;
        (self.center * size, Vec2::new(half_h * self.aspect(), half_h), self.angle.to_radians())
    }
}

/// The stencil texture: the image's colors, and in alpha what `source` reads.
fn stencil_pixels(file: &image::RgbaImage, source: ImageSource) -> image::RgbaImage {
    let mask = source.extract(image::DynamicImage::ImageRgba8(file.clone()));
    let mut out = file.clone();
    for (p, m) in out.pixels_mut().zip(mask.pixels()) {
        p[3] = m[0];
    }
    out
}

impl PainterApp {
    fn load_stencil(&mut self, path: &std::path::Path, ctx: &egui::Context) {
        let name = path.file_name().map_or("image".into(), |n| n.to_string_lossy().into_owned());
        let file = match image::open(path) {
            Ok(img) => {
                let img = if img.width().max(img.height()) > MAX_EDGE {
                    img.resize(MAX_EDGE, MAX_EDGE, image::imageops::FilterType::Triangle)
                } else {
                    img
                };
                img.into_rgba8()
            }
            Err(e) => {
                self.status = format!("Couldn't read {name}: {e}");
                return;
            }
        };
        // A transparent image cuts its own shape out; an opaque one reads as a mask
        // by brightness (Auto).
        self.stencil.source = ImageSource::Auto;
        self.stencil.image = Some(self.upload_stencil(name, file, ctx));
        self.stencil.on = true;
        self.status = "Stencil loaded. Hold S over the view: drag to move, right-drag to scale and turn.".into();
    }

    fn upload_stencil(&self, name: String, file: image::RgbaImage, ctx: &egui::Context) -> StencilImage {
        let pixels = stencil_pixels(&file, self.stencil.source);
        let size = [pixels.width() as usize, pixels.height() as usize];
        let handle = ctx.load_texture(format!("stencil {name}"), egui::ColorImage::from_rgba_unmultiplied(size, pixels.as_raw()), egui::TextureOptions::LINEAR);
        let tex = Arc::new(crate::gpu::upload_mips(&self.rs.device, &self.rs.queue, pixels, wgpu::TextureFormat::Rgba8Unorm, "stencil"));
        StencilImage { name, file, tex, handle }
    }

    /// The stencil for a stroke in view `kind` (`None`: no stencil, or it's off).
    pub(super) fn stencil_for(&self, kind: ViewKind) -> Option<Stencil> {
        let img = self.stencil.active()?;
        let [w, h] = match kind {
            ViewKind::V3d => self.views.view3d.size,
            ViewKind::V2d => self.views.view2d.size,
        };
        let (c, half, a) = self.stencil.frame(Vec2::new(w as f32, h as f32));
        Some(Stencil {
            tex: img.tex.clone(),
            uniform: [c.x, c.y, half.x, half.y, a.cos(), a.sin(), 1.0, 0.0],
            colors: self.stencil.colors,
        })
    }

    /// Placing the stencil while S is held over a view; true while it takes the input
    /// (the brush doesn't paint then).
    pub(super) fn stencil_input(&mut self, ui: &egui::Ui, rect: Rect, resp: &egui::Response) -> bool {
        if self.stencil.active().is_none() || !resp.hovered() || ui.ctx().egui_wants_keyboard_input() {
            return false;
        }
        let (held, delta) = ui.input(|i| (i.key_down(egui::Key::S), i.pointer.delta()));
        if !held {
            return false;
        }
        let size = Vec2::new(rect.width(), rect.height()).max(Vec2::ONE);
        if resp.dragged_by(egui::PointerButton::Primary) {
            self.stencil.center += Vec2::new(delta.x, delta.y) / size;
        } else if resp.dragged_by(egui::PointerButton::Secondary) {
            self.stencil.scale = (self.stencil.scale * (-delta.y * 0.01).exp()).clamp(0.02, 20.0);
            self.stencil.angle = (self.stencil.angle + delta.x * 0.4 + 180.0).rem_euclid(360.0) - 180.0;
        }
        ui.ctx().set_cursor_icon(egui::CursorIcon::Move);
        true
    }

    /// The stencil image over a view, faint, where it lets paint through.
    pub(super) fn draw_stencil(&self, painter: &egui::Painter, rect: Rect) {
        let Some(img) = self.stencil.active() else { return };
        let (c, half, a) = self.stencil.frame(Vec2::new(rect.width(), rect.height()));
        let (cos, sin) = (a.cos(), a.sin());
        let at = |x: f32, y: f32| {
            let (dx, dy) = (x * half.x, y * half.y);
            rect.min + vec2(c.x + dx * cos - dy * sin, c.y + dx * sin + dy * cos)
        };
        let tint = Color32::from_white_alpha((self.stencil.overlay * 255.0) as u8);
        let mut mesh = egui::Mesh::with_texture(img.handle.id());
        for (x, y, u, v) in [(-1.0, -1.0, 0.0, 0.0), (1.0, -1.0, 1.0, 0.0), (1.0, 1.0, 1.0, 1.0), (-1.0, 1.0, 0.0, 1.0)] {
            mesh.vertices.push(egui::epaint::Vertex { pos: at(x, y), uv: pos2(u, v), color: tint });
        }
        mesh.indices.extend([0, 1, 2, 0, 2, 3]);
        painter.add(egui::Shape::mesh(mesh));
        let outline = vec![at(-1.0, -1.0), at(1.0, -1.0), at(1.0, 1.0), at(-1.0, 1.0)];
        painter.add(egui::Shape::closed_line(outline, Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.7))));
    }

    /// The stencil's section in the brush properties.
    pub(super) fn stencil_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "Stencil");
        labeled(ui, "Image", |ui| {
            if let Some(img) = &self.stencil.image {
                let name = img.name.clone();
                ui.add(egui::Label::new(name.as_str()).truncate()).on_hover_text(name);
                if ui.small_button("✕").on_hover_text("Remove the stencil").clicked() {
                    self.stencil.image = None;
                }
            }
            let label = if self.stencil.image.is_some() { "Replace…" } else { "Load image…" };
            if ui.button(theme::with_icon(crate::icons::STICKER, label)).clicked()
                && let Some(path) = rfd::FileDialog::new().add_filter("Image", &super::IMAGE_EXT).pick_file()
            {
                self.load_stencil(&path, ui.ctx());
            }
        });
        if self.stencil.image.is_none() {
            ui.label(
                egui::RichText::new("An image held over the view that the brush paints through: logos, decals, patterns.")
                    .small()
                    .color(theme::TEXT_DIM),
            );
            return;
        }
        labeled(ui, "Use", |ui| ui.checkbox(&mut self.stencil.on, "").on_hover_text("Paint through the stencil (off: hide it)"));
        labeled(ui, "Paint its colors", |ui| {
            ui.checkbox(&mut self.stencil.colors, "")
                .on_hover_text("The brush lays down the image's colors instead of the brush color: decals. Roughness and the other channels still come from the brush.")
        });
        let before = self.stencil.source;
        labeled(ui, "Paint passes", |ui| {
            egui::ComboBox::from_id_salt("stencil_source").selected_text(before.name()).show_ui(ui, |ui| {
                for s in ImageSource::ALL {
                    ui.selectable_value(&mut self.stencil.source, s, s.name()).on_hover_text(s.tip());
                }
            })
        });
        if self.stencil.source != before
            && let Some(img) = self.stencil.image.take()
        {
            self.stencil.image = Some(self.upload_stencil(img.name, img.file, ui.ctx()));
        }
        labeled(ui, "Scale", |ui| ui.add(egui::Slider::new(&mut self.stencil.scale, 0.02..=20.0).logarithmic(true).max_decimals(2)));
        slider(ui, "Rotation", &mut self.stencil.angle, -180.0..=180.0);
        slider(ui, "Show", &mut self.stencil.overlay, 0.0..=1.0);
        ui.horizontal(|ui| {
            if ui.button("Center").on_hover_text("Back to the middle of the view, upright").clicked() {
                (self.stencil.center, self.stencil.angle) = (Vec2::splat(0.5), 0.0);
            }
            if ui.button("Fit").on_hover_text("As tall as 60% of the view").clicked() {
                self.stencil.scale = 0.6;
            }
        });
        ui.label(egui::RichText::new("Hold S over the view: drag to move, right-drag to scale and turn.").small().color(theme::TEXT_DIM));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stencil_keeps_colors_and_masks_by_the_chosen_part() {
        let mut file = image::RgbaImage::new(2, 1);
        file.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        file.put_pixel(1, 0, image::Rgba([0, 0, 255, 0]));
        let px = stencil_pixels(&file, ImageSource::Auto);
        assert_eq!(px.get_pixel(0, 0).0, [255, 0, 0, 255], "colors kept, opaque passes");
        assert_eq!(px.get_pixel(1, 0).0, [0, 0, 255, 0], "transparent blocks");
        let px = stencil_pixels(&file, ImageSource::Blue);
        assert_eq!(px.get_pixel(0, 0)[3], 0);
        assert_eq!(px.get_pixel(1, 0)[3], 255);
    }

    #[test]
    fn the_frame_follows_the_view() {
        let mut s = StencilState { scale: 0.5, ..Default::default() };
        s.center = Vec2::new(0.25, 0.5);
        let (c, half, _) = s.frame(Vec2::new(800.0, 400.0));
        assert_eq!(c, Vec2::new(200.0, 200.0));
        assert_eq!(half, Vec2::new(100.0, 100.0), "square image, half of 400 tall");
    }
}
