//! Opening a Substance 3D Painter project (`.spp`).
//!
//! The file is read with the reader adapted from SubCraft (`crate::spp`, MIT licensed, credited
//! there). What comes across:
//! * the model, every mesh of it, with its UVs and normals;
//! * every texture set (the meshes of one material), with its size and the final look of its
//!   channels: Painter's flattened result as of its last save, as one paint layer;
//! * each set's layer stack, rebuilt as far as Material Painter has equivalents (see
//!   `app::spp_open` and `app::spp_replay`: recorded strokes and polygon fills are replayed),
//!   with the images its fills use, decoded here.
//!
//! What does not, and is listed in the import report: procedural (Substance `.sbsar`)
//! materials, filters and generators (Material Painter has no Substance engine, and never
//! reads the contents of those archives: they are Adobe's), Painter's other channels (ambient
//! occlusion...) and its baked mesh maps (Material Painter bakes its own).

use crate::mesh::Mesh;
use crate::spp::{CacheFormat, LayerInfo, SppFile, channel_bits};
use glam::Vec2;
use std::path::Path;

/// Largest texture set an import makes.
pub const MAX_RESOLUTION: u32 = 4096;

/// A Painter channel as one of Material Painter's paint-layer channels: its size and texels in
/// `engine::PAINT_FORMATS[channel]` layout.
pub type ChannelData = (u32, Vec<u8>);

/// An image a layer of the project uses (bitmap fills of content and masks), as Painter stored
/// it: 8-bit RGBA, top row first, and the same as a PNG file for fill layers.
pub struct ImportedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub png: std::sync::Arc<Vec<u8>>,
}

/// Largest edge of an imported layer image.
const MAX_IMAGE: u32 = 2048;

/// One texture set of an imported project.
pub struct ImportedSet {
    pub resolution: u32,
    /// Base color, roughness, metallic, height, opacity (Material Painter's channel order).
    pub channels: [Option<ChannelData>; crate::engine::CHANNELS],
    /// The texture set's layer stack, bottom to top, for rebuilding.
    pub layers: Vec<LayerInfo>,
}

/// What an import made of a project.
pub struct Imported {
    /// Split into texture sets, one per Painter texture set.
    pub mesh: Mesh,
    /// One per `mesh.sets`, in that order.
    pub sets: Vec<ImportedSet>,
    /// What came across, for the report.
    pub came: Vec<String>,
    /// What didn't, and why.
    pub left: Vec<String>,
    /// Painter's world (where strokes and polygon fills were recorded) to the imported mesh's
    /// normalized space, and that space's scale (mesh units per Painter unit).
    pub to_mesh: glam::Mat4,
    pub mesh_scale: f32,
    /// The images the layer stacks use, by resource URL.
    pub images: std::collections::HashMap<String, ImportedImage>,
}

/// Painter channel flag for each of Material Painter's channels.
const CHANNELS: [(u64, &str); crate::engine::CHANNELS] = [
    (channel_bits::BASE_COLOR, "Base color"),
    (channel_bits::ROUGHNESS, "Roughness"),
    (channel_bits::METALLIC, "Metallic"),
    (channel_bits::HEIGHT, "Height"),
    (channel_bits::OPACITY, "Opacity"),
    (channel_bits::EMISSIVE, "Emissive"),
    (channel_bits::NORMAL, "Normal"),
    // Painter's specular level channel: not read yet (bit 0 matches nothing).
    (0, "Specular"),
];

fn channel_name(bits: u64) -> String {
    match bits {
        channel_bits::NORMAL => "Normal (painted normal details)".into(),
        channel_bits::EMISSIVE => "Emissive".into(),
        channel_bits::OPACITY => "Opacity".into(),
        channel_bits::AMBIENT_OCCLUSION => "Ambient occlusion".into(),
        0x4 => "Specular edge color".into(),
        0x20 => "Displacement".into(),
        0x40 => "Glossiness".into(),
        b => format!("channel {b:#x}"),
    }
}

/// Reads a project. Runs on a worker thread: decoding a 4K project takes a few seconds.
pub fn import(path: &Path) -> Result<Imported, String> {
    let err = |e: crate::spp::SppError| format!("Can't read {}: {e}", path.display());
    let mut file = SppFile::open(path).map_err(err)?;
    let project = file.project().map_err(err)?;
    if project.texture_sets.is_empty() {
        return Err("The project has no texture set.".into());
    }
    let mut came = Vec::new();
    let mut left = Vec::new();

    // The model: every mesh, as one, split into texture sets by material.
    let (meshes, materials) = file.meshes_and_materials().map_err(err)?;
    if meshes.is_empty() {
        return Err("The project has no model.".into());
    }
    let (mut positions, mut normals, mut uvs, mut indices) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut objects, mut tri_object, mut tri_material) = (Vec::new(), Vec::new(), Vec::new());
    for (k, m) in meshes.iter().enumerate() {
        let base = positions.len() as u32;
        positions.extend(&m.positions);
        normals.extend((0..m.positions.len()).map(|i| m.normals.get(i).copied().unwrap_or(glam::Vec3::Y)));
        // Painter's v points up; Material Painter's texture rows start at the top (as its OBJ
        // and FBX import).
        uvs.extend((0..m.positions.len()).map(|i| m.uvs.get(i).map_or(Vec2::ZERO, |uv| Vec2::new(uv.x, 1.0 - uv.y))));
        for t in &m.triangles {
            indices.extend(t.iter().map(|i| i + base));
            tri_object.push(k as u32);
            tri_material.push(m.material);
        }
        objects.push(m.name.clone());
    }
    let tris = indices.len() / 3;
    // `Mesh::from_parts` fits the model into a [-1, 1] box; strokes and polygon fills were
    // recorded in Painter's units, so they go through the same fit.
    let (lo, hi) = positions.iter().fold((glam::Vec3::splat(f32::MAX), glam::Vec3::splat(f32::MIN)), |(lo, hi), p| (lo.min(*p), hi.max(*p)));
    let mesh_scale = 2.0 / (hi - lo).max_element().max(1e-6);
    let to_mesh = glam::Mat4::from_scale(glam::Vec3::splat(mesh_scale)) * glam::Mat4::from_translation(-(lo + hi) * 0.5);
    let name = path.file_stem().map_or("Painter project".into(), |s| s.to_string_lossy().into_owned());
    let mut mesh = Mesh::from_parts(name, positions, Some(normals), uvs, indices);
    if mesh.indices.len() / 3 == tri_object.len() {
        mesh.tri_object = tri_object;
        mesh.objects = objects;
    }
    // Without material names (an unusual dump), materials pair with texture sets in order.
    let names: Vec<String> = if materials.is_empty() { project.texture_sets.iter().map(|s| s.name.clone()).collect() } else { materials };
    mesh.split_by_material(&tri_material, &names);
    came.push(format!(
        "The model: {} mesh{} ({tris} triangles), with its UVs and normals, in {} texture set{}.",
        meshes.len(),
        if meshes.len() == 1 { "" } else { "es" },
        mesh.sets.len(),
        if mesh.sets.len() == 1 { "" } else { "s" }
    ));

    let mut sets = Vec::with_capacity(mesh.sets.len());
    let mut procedural = 0;
    let mut images = std::collections::HashMap::new();
    let mut other_channels: Vec<String> = Vec::new();
    for ms in &mesh.sets {
        let Some(set) = project.texture_sets.iter().find(|s| s.name == ms.name).or(if mesh.sets.len() == 1 { project.texture_sets.first() } else { None }) else {
            left.push(format!("Texture set \"{}\": Painter's project has no set of that name; it comes in unpainted.", ms.name));
            sets.push(ImportedSet { resolution: 2048, channels: Default::default(), layers: Vec::new() });
            continue;
        };
        let size = 1u32 << set.width_log2.clamp(7, 13);
        let resolution = size.min(MAX_RESOLUTION);

        // The flattened channels.
        let mut channels: [Option<ChannelData>; crate::engine::CHANNELS] = Default::default();
        let mut got = Vec::new();
        for (k, (bits, label)) in CHANNELS.iter().enumerate() {
            match file.channel_texture(set, *bits, MAX_RESOLUTION) {
                Ok(Some(img)) => {
                    let p = &img.plane;
                    if p.width != p.height {
                        left.push(format!("{}: {label} not square ({}x{}), skipped.", set.name, p.width, p.height));
                        continue;
                    }
                    let data = match (k, img.format) {
                        (0 | 5, CacheFormat::Srgb8Alpha8) => to_u8(&p.data),
                        (1 | 2 | 4, CacheFormat::Rg8) => to_u8(&p.data),
                        (3, CacheFormat::Rg16f) => p.data.iter().flat_map(|v| crate::gpu::f32_to_f16(*v).to_le_bytes()).collect(),
                        // Painter's normal details: tangent space, already encoded 0..1 (flat: 0.5, 0.5, 1).
                        (crate::engine::NORMAL, CacheFormat::Rgba16f) => to_u8(&p.data),
                        (_, f) => {
                            left.push(format!("{}: {label} stored as {f:?}, which this import doesn't read.", set.name));
                            continue;
                        }
                    };
                    channels[k] = Some((p.width, data));
                    got.push(*label);
                }
                Ok(None) => {
                    // Painter only stores channels it has computed.
                    if set.channels.iter().any(|c| c.bits == *bits) {
                        left.push(format!(
                            "{}: {label} wasn't stored by Painter (open and save the project in Painter to refresh it).",
                            set.name
                        ));
                    }
                }
                Err(e) => left.push(format!("{}: {label} couldn't be read ({e}).", set.name)),
            }
        }
        came.push(format!(
            "Texture set \"{}\" at {resolution}²{}{}.",
            set.name,
            if size > MAX_RESOLUTION { format!(" (Painter's {size}² is past Material Painter's largest)") } else { String::new() },
            if got.is_empty() {
                ", unpainted (no stored result)".to_string()
            } else {
                format!(", with the final look of {} as one paint layer", got.join(", ").to_lowercase())
            }
        ));
        for c in set.channels.iter().filter(|c| !CHANNELS.iter().any(|(b, _)| *b == c.bits)) {
            let n = channel_name(c.bits);
            if !other_channels.contains(&n) {
                other_channels.push(n);
            }
        }
        let mut urls = Vec::new();
        crate::spp::document::walk_layers(
            &set.layers,
            &mut |l, _| {
                procedural += l.sources.iter().filter(|s| matches!(s, crate::spp::SourceInfo::Procedural { .. })).count();
                for a in l.content.iter().chain(&l.mask) {
                    if let crate::spp::document::ActionKind::Fill { sources, .. } = &a.kind {
                        urls.extend(sources.iter().filter_map(|s| match s {
                            crate::spp::SourceInfo::Bitmap { url, .. } => Some(url.clone()),
                            _ => None,
                        }));
                    }
                }
            },
            0,
        );
        for url in urls {
            if images.contains_key(&url) {
                continue;
            }
            match read_image(&mut file, &url) {
                Ok(Some(img)) => {
                    images.insert(url, img);
                }
                Ok(None) => left.push(format!("{}: an image a layer uses isn't stored in the project.", set.name)),
                Err(e) => left.push(format!("{}: an image a layer uses couldn't be read ({e}).", set.name)),
            }
        }
        sets.push(ImportedSet { resolution, channels, layers: set.layers.clone() });
    }
    let unused: Vec<&str> =
        project.texture_sets.iter().filter(|s| !mesh.sets.iter().any(|m| m.name == s.name)).map(|s| s.name.as_str()).collect();
    if !unused.is_empty() && mesh.sets.len() > 1 {
        left.push(format!("Texture sets with no part of the model: {}.", unused.join(", ")));
    }
    if !other_channels.is_empty() {
        left.push(format!("Painter channels Material Painter doesn't have: {}.", other_channels.join(", ")));
    }
    left.push("Painter's baked mesh maps: Material Painter bakes its own from the model. If Painter baked from a high poly, load it in the bake window.".into());
    if procedural > 0 {
        left.push(format!(
            "{procedural} procedural source{} (Substance materials, generators, filters): Material Painter can't run Substance graphs. Smart masks come in as the closest Material Painter ones; materials and filters only in the flattened layer.",
            if procedural == 1 { "" } else { "s" }
        ));
    }

    Ok(Imported { mesh, sets, came, left, to_mesh, mesh_scale, images })
}

/// Decodes an image resource of the project (`None`: not stored in it).
fn read_image(file: &mut SppFile, url: &str) -> Result<Option<ImportedImage>, String> {
    let Some(blob) = file.image_resource_blob(url).map_err(|e| e.to_string())? else { return Ok(None) };
    let plane = crate::spp::images::decode_image_resource(&blob, MAX_IMAGE).map_err(|e| e.to_string())?;
    let n = plane.components.max(1);
    let mut rgba = Vec::with_capacity((plane.width * plane.height * 4) as usize);
    for px in plane.data.chunks_exact(n) {
        let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        match n {
            1 => rgba.extend([q(px[0]), q(px[0]), q(px[0]), 255]),
            2 => rgba.extend([q(px[0]), q(px[0]), q(px[0]), q(px[1])]),
            3 => rgba.extend([q(px[0]), q(px[1]), q(px[2]), 255]),
            _ => rgba.extend([q(px[0]), q(px[1]), q(px[2]), q(px[3])]),
        }
    }
    let img = image::RgbaImage::from_raw(plane.width, plane.height, rgba).ok_or("image size")?;
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).map_err(|e| e.to_string())?;
    Ok(Some(ImportedImage { width: img.width(), height: img.height(), rgba: img.into_raw(), png: std::sync::Arc::new(png) }))
}

fn to_u8(v: &[f32]) -> Vec<u8> {
    v.iter().map(|x| (x.clamp(0.0, 1.0) * 255.0).round() as u8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opt-in check of a real project: `MP_SPP_TEST=a.spp cargo test spp_from_env -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn spp_from_env() {
        let Ok(path) = std::env::var("MP_SPP_TEST") else { return };
        let imported = import(Path::new(&path)).expect("imports");
        for (set, data) in imported.mesh.sets.iter().zip(&imported.sets) {
            let got: Vec<bool> = data.channels.iter().map(Option::is_some).collect();
            let (mut lo, mut hi) = (Vec2::splat(f32::MAX), Vec2::splat(f32::MIN));
            for &i in &imported.mesh.indices[set.index_range().start as usize..set.index_range().end as usize] {
                let uv = imported.mesh.vertices[i as usize].uv;
                lo = lo.min(Vec2::from(uv));
                hi = hi.max(Vec2::from(uv));
            }
            println!("{} {:?}: {}², channels {got:?}, {} layers, uv {lo} .. {hi}", set.name, set.tris, data.resolution, data.layers.len());
        }
        assert_eq!(imported.mesh.sets.len(), imported.sets.len());
        // Each set's base color with its UV wireframe drawn on, to check they line up.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/bake_test/spp");
        std::fs::create_dir_all(&dir).unwrap();
        for (set, data) in imported.mesh.sets.iter().zip(&imported.sets) {
            let Some((w, px)) = &data.channels[0] else { continue };
            let mut img = image::RgbaImage::from_raw(*w, *w, px.clone()).unwrap();
            let r = set.index_range();
            for t in imported.mesh.indices[r.start as usize..r.end as usize].chunks_exact(3) {
                for e in 0..3 {
                    let a = Vec2::from(imported.mesh.vertices[t[e] as usize].uv) * *w as f32;
                    let b = Vec2::from(imported.mesh.vertices[t[(e + 1) % 3] as usize].uv) * *w as f32;
                    let n = (a.distance(b).ceil() as usize).max(1);
                    for s in 0..=n {
                        let p = a.lerp(b, s as f32 / n as f32);
                        if p.x >= 0.0 && p.y >= 0.0 && (p.x as u32) < *w && (p.y as u32) < *w {
                            img.put_pixel(p.x as u32, p.y as u32, image::Rgba([0, 255, 0, 255]));
                        }
                    }
                }
            }
            let small = image::imageops::resize(&img, 1024.min(*w), 1024.min(*w), image::imageops::FilterType::Triangle);
            small.save(dir.join(format!("uvcheck_{}.png", set.name))).unwrap();
        }
        for l in imported.came.iter().chain(&imported.left) {
            println!("  {l}");
        }
    }

    /// SubCraft's synthetic project (no real Painter file): the model, the stack's layers and
    /// a report that says what was left out.
    #[test]
    fn imports_synthetic_project() {
        let path = std::env::temp_dir().join(format!("mp_spp_fixture_{}.spp", std::process::id()));
        std::fs::write(&path, crate::spp::fixture::project_file()).unwrap();
        let imported = import(&path).expect("imports");
        let _ = std::fs::remove_file(&path);
        assert_eq!(imported.mesh.indices.len() / 3, 2, "the quad");
        assert_eq!(imported.sets.len(), 1);
        assert_eq!(imported.sets[0].layers.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["Rust", "Dirt"]);
        assert!(imported.came.iter().any(|l| l.starts_with("The model")));
        // Its one cached channel (roughness) is 4x2: not square, reported and skipped.
        assert!(imported.sets[0].channels.iter().all(Option::is_none));
        assert!(imported.left.iter().any(|l| l.contains("not square")), "{:?}", imported.left);
        assert!(imported.left.iter().any(|l| l.contains("baked mesh maps")));
    }

    /// Painter's version-0 `document.bin` (self-describing fields), as other Painter versions
    /// write it, reads into the same project model.
    #[test]
    fn reads_version_0_document() {
        let mut d = Vec::new();
        d.extend(0x1b7c_2fddu32.to_le_bytes());
        d.extend(0u32.to_le_bytes());
        d.extend(81u32.to_le_bytes());
        let name = |d: &mut Vec<u8>, s: &str| {
            d.extend((s.len() as u32).to_le_bytes());
            d.extend(s.as_bytes());
        };
        let object = |d: &mut Vec<u8>, class: &str, fields: u16| {
            d.extend([0u8]);
            d.extend(0u32.to_le_bytes());
            name(d, class);
            d.extend(fields.to_le_bytes());
        };
        d.push(18);
        object(&mut d, "DataDocument", 1);
        name(&mut d, "materials");
        d.push(19);
        d.extend(0u32.to_le_bytes());
        d.extend(1u32.to_le_bytes());
        d.push(18);
        object(&mut d, "DataMaterial", 4);
        name(&mut d, "resolutionLog2");
        d.push(6);
        d.extend(11i32.to_le_bytes());
        d.extend(11i32.to_le_bytes());
        name(&mut d, "sceneMaterialName");
        d.push(16);
        name(&mut d, "Body");
        name(&mut d, "colorManagementWorkflow");
        d.push(9);
        d.extend(0u64.to_le_bytes());
        // A texture set needs a stack (here an empty one).
        name(&mut d, "stacks");
        d.push(19);
        d.extend(0u32.to_le_bytes());
        d.extend(1u32.to_le_bytes());
        d.push(18);
        object(&mut d, "DataMaterialStack", 0);
        let root = crate::spp::document::parse(&d).expect("version 0 parses");
        let info = crate::spp::document::project_info(&root);
        assert_eq!(info.texture_sets.len(), 1);
        assert_eq!(info.texture_sets[0].name, "Body");
        assert_eq!(info.texture_sets[0].width_log2, 11);
    }
}
