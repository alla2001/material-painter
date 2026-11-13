//! Mesh data, procedural preview meshes, OBJ/FBX import (with automatic unwrapping of
//! objects that have no UVs) and OBJ export.
//!
//! UVs are stored "texture-down": t = 0 is the top row of every texture, which
//! matches how GPU textures and image files are laid out. OBJ files (v-up) are
//! flipped on import.

use glam::{Mat3, Vec2, Vec3};
use std::collections::HashMap;
use std::f32::consts::{FRAC_PI_2, PI, TAU};
use std::ops::Range;

/// When importing a mesh makes new UVs (Substance Painter's auto-unwrap). Each texture set
/// (material) is unwrapped on its own into the whole 0..1 square.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AutoUnwrap {
    /// Meshes without UVs are refused.
    Off,
    /// Texture sets with any object lacking UVs are unwrapped; the others keep theirs.
    Missing,
    /// Every texture set is unwrapped, replacing the file's UVs.
    All,
}

impl AutoUnwrap {
    pub const ALL: [AutoUnwrap; 3] = [AutoUnwrap::Missing, AutoUnwrap::All, AutoUnwrap::Off];
    pub fn name(self) -> &'static str {
        match self {
            AutoUnwrap::Off => "Off (meshes need UVs)",
            AutoUnwrap::Missing => "Where UVs are missing",
            AutoUnwrap::All => "Always (replace the mesh's UVs)",
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            AutoUnwrap::Off => "off",
            AutoUnwrap::Missing => "missing",
            AutoUnwrap::All => "all",
        }
    }
    pub fn from_key(key: &str) -> Option<AutoUnwrap> {
        AutoUnwrap::ALL.into_iter().find(|m| m.key() == key)
    }
}

/// The texture set name of material `m` (before duplicates get numbers).
fn material_name(names: &[String], m: u32) -> String {
    names.get(m as usize).filter(|n| !n.trim().is_empty()).cloned().unwrap_or_else(|| format!("Material {}", m + 1))
}

/// A texture set: the triangles of one material, which get their own layer
/// stack and textures. A set's triangles are contiguous in `Mesh::indices`,
/// and its UV edges and seams contiguous in `uv_edges` and `seam_edges`.
#[derive(Clone, Debug, PartialEq)]
pub struct MeshSet {
    pub name: String,
    /// The UDIM tile (1001, 1002, …) the set's triangles came from, when the mesh was laid
    /// out in tiles: each tile of a material is a set of its own, its UVs moved into 0..1.
    pub udim: Option<u32>,
    /// Triangles (not indices).
    pub tris: Range<u32>,
    /// Index ranges into `uv_edges` and `seam_edges`.
    pub edges: Range<u32>,
    pub seams: Range<u32>,
}

impl MeshSet {
    /// The material's name without the tile ("Body" for "Body.1002"): what `$set` names
    /// in exported files, the tile number going after it.
    pub fn base_name(&self) -> &str {
        match self.udim {
            Some(tile) => self.name.strip_suffix(&format!(".{tile}")).unwrap_or(&self.name),
            None => &self.name,
        }
    }

    /// The set's range in `Mesh::indices`.
    pub fn index_range(&self) -> Range<u32> {
        self.tris.start * 3..self.tris.end * 3
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub pos: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
    pub tangent: [f32; 4],
}

pub struct Mesh {
    pub name: String,
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    /// Unique UV edges as a line list, used by the 2D view wireframe.
    /// Polygon edges (index pairs): every triangle edge once, without the
    /// diagonals that split quads.
    pub uv_edges: Vec<u32>,
    /// UV seams (index pairs): edges where the two sides use different UVs,
    /// and open borders; drawn on both islands.
    pub seam_edges: Vec<u32>,
    pub bounds_min: Vec3,
    pub bounds_max: Vec3,
    /// Normalization applied on import: `normalized = (original - center) * scale`.
    pub center: Vec3,
    pub scale: f32,
    /// Source object per triangle and object names (empty for primitives):
    /// pairs low- and high-poly parts by name when baking.
    pub tri_object: Vec<u32>,
    pub objects: Vec<String>,
    /// Vertex colors per triangle (sRGB), for color ID maps.
    pub tri_color: Option<Vec<[u8; 3]>>,
    /// Texture sets, one per material, in triangle order; never empty.
    pub sets: Vec<MeshSet>,
}

/// A detail mesh used only as a bake source; it needs no UVs.
pub struct HighPoly {
    pub name: String,
    pub positions: Vec<Vec3>,
    pub normals: Vec<Vec3>,
    pub indices: Vec<u32>,
    /// Material index per triangle (OBJ `usemtl` groups).
    pub tri_material: Vec<u32>,
    pub material_count: u32,
    pub tri_object: Vec<u32>,
    pub objects: Vec<String>,
    pub tri_color: Option<Vec<[u8; 3]>>,
}

impl HighPoly {
    /// Loads an OBJ or FBX in the low poly's original space and applies the
    /// same normalization, so both meshes line up.
    pub fn load(path: &std::path::Path, center: Vec3, scale: f32) -> Result<HighPoly, String> {
        let raw = crate::import::load(path)?;
        let positions: Vec<Vec3> = raw.positions.iter().map(|p| (*p - center) * scale).collect();
        let normals = raw.normals.unwrap_or_else(|| smooth_normals(&positions, &raw.indices));
        Ok(HighPoly {
            name: raw.name,
            positions,
            normals,
            indices: raw.indices,
            tri_material: raw.tri_material,
            material_count: raw.material_count,
            tri_object: raw.tri_object,
            objects: raw.objects,
            tri_color: raw.tri_color,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Primitive {
    RoundedCube,
    Sphere,
    Torus,
    Cylinder,
    Plane,
    MaterialBall,
}

impl Primitive {
    pub const ALL: [Primitive; 6] = [
        Primitive::RoundedCube,
        Primitive::Sphere,
        Primitive::Torus,
        Primitive::Cylinder,
        Primitive::Plane,
        Primitive::MaterialBall,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Primitive::RoundedCube => "Rounded Cube",
            Primitive::Sphere => "Sphere",
            Primitive::Torus => "Torus",
            Primitive::Cylinder => "Cylinder",
            Primitive::Plane => "Plane",
            Primitive::MaterialBall => "Material Ball",
        }
    }

    pub fn build(self) -> Mesh {
        let (p, n, uv, idx) = match self {
            Primitive::RoundedCube => rounded_cube(0.2),
            Primitive::Sphere => uv_sphere(96, 48),
            Primitive::Torus => torus(0.7, 0.3, 128, 64),
            Primitive::Cylinder => cylinder(96, 24),
            Primitive::Plane => plane(32),
            Primitive::MaterialBall => material_ball(),
        };
        Mesh::from_parts(self.name().to_string(), p, Some(n), uv, idx)
    }
}

impl Mesh {
    pub fn from_parts(
        name: String,
        mut positions: Vec<Vec3>,
        normals: Option<Vec<Vec3>>,
        uvs: Vec<Vec2>,
        indices: Vec<u32>,
    ) -> Mesh {
        // Normalize into a [-1, 1] box so brush, noise and bake settings behave
        // the same on every mesh.
        let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        for p in &positions {
            lo = lo.min(*p);
            hi = hi.max(*p);
        }
        let center = (lo + hi) * 0.5;
        let scale = 2.0 / (hi - lo).max_element().max(1e-6);
        for p in &mut positions {
            *p = (*p - center) * scale;
        }
        let bounds_min = (lo - center) * scale;
        let bounds_max = (hi - center) * scale;

        let normals = normals.unwrap_or_else(|| smooth_normals(&positions, &indices));
        let (indices, source, tangents) = match mikk_tangents(&positions, &normals, &uvs, &indices) {
            Some(t) => t,
            None => {
                let t = compute_tangents(&positions, &normals, &uvs, &indices);
                (indices, (0..positions.len()).collect(), t)
            }
        };
        let vertices: Vec<Vertex> = source
            .iter()
            .zip(tangents)
            .map(|(&i, tangent)| Vertex {
                pos: positions[i].to_array(),
                normal: normals[i].normalize_or_zero().to_array(),
                uv: uvs[i].to_array(),
                tangent,
            })
            .collect();
        let mut mesh = Mesh {
            name,
            vertices,
            indices,
            uv_edges: Vec::new(),
            seam_edges: Vec::new(),
            bounds_min,
            bounds_max,
            center,
            scale,
            tri_object: Vec::new(),
            objects: Vec::new(),
            tri_color: None,
            sets: Vec::new(),
        };
        mesh.set_ranges(Vec::new());
        mesh
    }

    /// Rebuilds a mesh saved in a project, already normalized.
    pub fn from_saved(name: String, vertices: Vec<Vertex>, indices: Vec<u32>, center: Vec3, scale: f32) -> Mesh {
        let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        for v in &vertices {
            lo = lo.min(Vec3::from_array(v.pos));
            hi = hi.max(Vec3::from_array(v.pos));
        }
        let mut mesh = Mesh {
            name,
            vertices,
            indices,
            uv_edges: Vec::new(),
            seam_edges: Vec::new(),
            bounds_min: lo,
            bounds_max: hi,
            center,
            scale,
            tri_object: Vec::new(),
            objects: Vec::new(),
            tri_color: None,
            sets: Vec::new(),
        };
        mesh.set_ranges(Vec::new());
        mesh
    }

    /// Splits the mesh into texture sets, one per material: triangles are
    /// reordered (stably) so each material's are contiguous. `tri_material`
    /// has one entry per triangle; `names` names the materials. Materials
    /// without triangles get no set.
    pub fn split_by_material(&mut self, tri_material: &[u32], names: &[String]) {
        self.split_by_material_and_tile(tri_material, names, None);
    }

    /// `split_by_material`, and with `tiles` (UDIM tile per triangle, from
    /// `split_into_udims`) one set per tile of each material, named "Material.1001".
    pub fn split_by_material_and_tile(&mut self, tri_material: &[u32], names: &[String], tiles: Option<&[u32]>) {
        let tris = self.triangle_count();
        let tiles = tiles.filter(|t| t.len() == tris);
        let one_material = vec![0; tris];
        let tri_material = match tri_material.len() == tris {
            true => tri_material,
            false if tiles.is_some() => &one_material,
            false => return,
        };
        let tile = |t: usize| tiles.map(|tiles| tiles[t]);
        let mut order: Vec<usize> = (0..tris).collect();
        order.sort_by_key(|&t| (tri_material[t], tile(t)));
        let mut indices = Vec::with_capacity(self.indices.len());
        for &t in &order {
            indices.extend_from_slice(&self.indices[t * 3..t * 3 + 3]);
        }
        self.indices = indices;
        if self.tri_object.len() == tris {
            self.tri_object = order.iter().map(|&t| self.tri_object[t]).collect();
        }
        if let Some(c) = &self.tri_color {
            self.tri_color = Some(order.iter().map(|&t| c[t]).collect());
        }
        let mut ranges: Vec<(String, Range<u32>)> = Vec::new();
        let mut range_tiles: Vec<Option<u32>> = Vec::new();
        for (k, &t) in order.iter().enumerate() {
            let m = tri_material[t];
            let name = match tile(t) {
                Some(tile) => format!("{}.{tile}", material_name(names, m)),
                None => material_name(names, m),
            };
            match ranges.last_mut() {
                Some((n, r)) if *n == name && r.end == k as u32 => r.end += 1,
                _ => {
                    ranges.push((name, k as u32..k as u32 + 1));
                    range_tiles.push(tile(t));
                }
            }
        }
        // Two materials with one name share a set only when adjacent; keep
        // names unique so sets can be told apart (and exported) by name.
        let mut seen: HashMap<String, usize> = HashMap::new();
        for (n, _) in &mut ranges {
            let count = seen.entry(n.clone()).or_insert(0);
            *count += 1;
            if *count > 1 {
                *n = format!("{n} {count}");
            }
        }
        self.set_ranges(ranges);
        self.set_udims(&range_tiles);
    }

    /// The UDIM tile of each set (from a saved project), when there's one per set.
    pub fn set_udims(&mut self, tiles: &[Option<u32>]) {
        if tiles.len() == self.sets.len() {
            for (set, &tile) in self.sets.iter_mut().zip(tiles) {
                set.udim = tile;
            }
        }
    }

    /// When the UVs are laid out in UDIM tiles (islands in more than the 0..1 square, each
    /// triangle inside one tile), moves every triangle's UVs into 0..1 and returns its tile
    /// (1001 + u + 10 v). Vertices shared by triangles of two tiles are split. `None` (and
    /// nothing changed) for a 0..1 layout, or UVs that repeat across tile borders.
    pub fn split_into_udims(&mut self) -> Option<Vec<u32>> {
        const EPS: f32 = 1e-3;
        let tris = self.triangle_count();
        let mut tiles: Vec<(u32, u32)> = Vec::with_capacity(tris);
        for t in 0..tris {
            // UDIM v counts up; UVs are stored texture-down.
            let uv = |c: usize| {
                let [u, v] = self.vertices[self.indices[t * 3 + c] as usize].uv;
                (u, 1.0 - v)
            };
            let corners = [uv(0), uv(1), uv(2)];
            let (cu, cv) = corners.iter().fold((0.0, 0.0), |(a, b), (u, v)| (a + u / 3.0, b + v / 3.0));
            let (tu, tv) = (cu.floor(), cv.floor());
            if !(0.0..10.0).contains(&tu) || !(0.0..100.0).contains(&tv) {
                return None;
            }
            let inside = |x: f32, lo: f32| x >= lo - EPS && x <= lo + 1.0 + EPS;
            if !corners.iter().all(|&(u, v)| inside(u, tu) && inside(v, tv)) {
                return None;
            }
            tiles.push((tu as u32, tv as u32));
        }
        if tiles.iter().all(|&t| t == (0, 0)) {
            return None;
        }
        // Each vertex belongs to one tile; a vertex on a tile border used by both gets a copy.
        let mut owner: Vec<Option<(u32, u32)>> = vec![None; self.vertices.len()];
        let mut copies: HashMap<(u32, (u32, u32)), u32> = HashMap::new();
        for (t, &tile) in tiles.iter().enumerate() {
            for c in 0..3 {
                let v = self.indices[t * 3 + c];
                match owner[v as usize] {
                    None => owner[v as usize] = Some(tile),
                    Some(o) if o == tile => {}
                    Some(_) => {
                        let copy = *copies.entry((v, tile)).or_insert_with(|| {
                            self.vertices.push(self.vertices[v as usize]);
                            owner.push(Some(tile));
                            (self.vertices.len() - 1) as u32
                        });
                        self.indices[t * 3 + c] = copy;
                    }
                }
            }
        }
        for (vertex, tile) in self.vertices.iter_mut().zip(&owner) {
            if let Some((tu, tv)) = *tile {
                vertex.uv[0] -= tu as f32;
                vertex.uv[1] += tv as f32;
            }
        }
        Some(tiles.into_iter().map(|(u, v)| 1001 + u + 10 * v).collect())
    }

    /// Sets the texture sets from `(name, triangle range)` pairs covering
    /// the triangles in order (one set named after the mesh when empty or
    /// invalid), and rebuilds the per-set UV edges and seams.
    pub fn set_ranges(&mut self, ranges: Vec<(String, Range<u32>)>) {
        let tris = self.triangle_count() as u32;
        let valid = !ranges.is_empty()
            && ranges[0].1.start == 0
            && ranges.last().is_some_and(|r| r.1.end == tris)
            && ranges.windows(2).all(|w| w[0].1.end == w[1].1.start)
            && ranges.iter().all(|r| r.1.start < r.1.end);
        let ranges = if valid { ranges } else { vec![(self.name.clone(), 0..tris)] };
        self.uv_edges.clear();
        self.seam_edges.clear();
        self.sets = ranges
            .into_iter()
            .map(|(name, tris)| {
                let idx = &self.indices[tris.start as usize * 3..tris.end as usize * 3];
                let e0 = self.uv_edges.len() as u32;
                self.uv_edges.extend(polygon_edges(&self.vertices, idx));
                let s0 = self.seam_edges.len() as u32;
                self.seam_edges.extend(seam_edges(&self.vertices, idx));
                MeshSet { name, udim: None, tris, edges: e0..self.uv_edges.len() as u32, seams: s0..self.seam_edges.len() as u32 }
            })
            .collect();
    }

    /// The texture set a triangle belongs to.
    pub fn set_of(&self, tri: u32) -> usize {
        self.sets.iter().position(|s| s.tris.contains(&tri)).unwrap_or(0)
    }

    /// Loads an OBJ or FBX as the paintable mesh, unwrapping texture sets without UVs.
    #[cfg(test)]
    pub fn load_file(path: &std::path::Path) -> Result<Mesh, String> {
        Self::load_file_with(path, AutoUnwrap::Missing, true, &|_| {}).map(|(mesh, _)| mesh)
    }

    /// Loads an OBJ or FBX as the paintable mesh; also returns the texture sets `unwrap`
    /// gave new UVs. `progress` hears how unwrapping goes ("Unwrapping Body (2/3): packing
    /// 40%"), from other threads.
    /// With `udims`, UVs laid out in UDIM tiles give one set per tile of each material.
    pub fn load_file_with(path: &std::path::Path, unwrap: AutoUnwrap, udims: bool, progress: &(dyn Fn(String) + Sync)) -> Result<(Mesh, Vec<String>), String> {
        let mut raw = crate::import::load(path)?;
        let unwrapped = match unwrap {
            AutoUnwrap::Off if !raw.missing_uvs.is_empty() => {
                let names = raw.missing_uvs.join(", ");
                return Err(format!(
                    "These objects have no UVs: {names}. Unwrap the mesh before importing it, or turn on auto-unwrap in Preferences > General."
                ));
            }
            AutoUnwrap::Off => Vec::new(),
            mode => unwrap_raw(&mut raw, mode == AutoUnwrap::All, progress)?,
        };
        let mut mesh = Mesh::from_parts(raw.name, raw.positions, raw.normals, raw.uvs, raw.indices);
        // Triangle order survives tangent generation, so per-triangle data still lines up.
        mesh.tri_object = raw.tri_object;
        mesh.objects = raw.objects;
        mesh.tri_color = raw.tri_color;
        let tiles = if udims { mesh.split_into_udims() } else { None };
        mesh.split_by_material_and_tile(&raw.tri_material, &raw.material_names, tiles.as_deref());
        Ok((mesh, unwrapped))
    }

    /// Writes the mesh as OBJ (with an MTL naming one material per texture set), in its
    /// original units and placement, so its UVs (unwrapped ones too) go with the exported
    /// textures.
    pub fn write_obj(&self, path: &std::path::Path) -> Result<(), String> {
        use std::io::Write;
        let mtl = path.with_extension("mtl");
        let mut f = std::io::BufWriter::new(std::fs::File::create(path).map_err(|e| e.to_string())?);
        let mut m = std::io::BufWriter::new(std::fs::File::create(&mtl).map_err(|e| e.to_string())?);
        let io = |e: std::io::Error| e.to_string();
        writeln!(f, "# Material Painter").map_err(io)?;
        writeln!(f, "mtllib {}", mtl.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())).map_err(io)?;
        writeln!(f, "o {}", self.name).map_err(io)?;
        for v in &self.vertices {
            let p = Vec3::from_array(v.pos) / self.scale + self.center;
            writeln!(f, "v {} {} {}", p.x, p.y, p.z).map_err(io)?;
        }
        // UDIM sets' UVs go back to their tiles.
        let mut tile = vec![(0.0f32, 0.0f32); self.vertices.len()];
        for set in &self.sets {
            if let Some(udim) = set.udim {
                let offset = (((udim - 1001) % 10) as f32, ((udim - 1001) / 10) as f32);
                for &i in &self.indices[set.index_range().start as usize..set.index_range().end as usize] {
                    tile[i as usize] = offset;
                }
            }
        }
        for (v, (tu, tv)) in self.vertices.iter().zip(&tile) {
            // Stored texture-down; OBJ is v-up.
            writeln!(f, "vt {} {}", v.uv[0] + tu, 1.0 - v.uv[1] + tv).map_err(io)?;
        }
        for v in &self.vertices {
            writeln!(f, "vn {} {} {}", v.normal[0], v.normal[1], v.normal[2]).map_err(io)?;
        }
        for set in &self.sets {
            let name = set.name.replace(char::is_whitespace, "_");
            writeln!(m, "newmtl {name}\nKd 0.8 0.8 0.8\n").map_err(io)?;
            writeln!(f, "usemtl {name}").map_err(io)?;
            for t in self.indices[set.index_range().start as usize..set.index_range().end as usize].chunks_exact(3) {
                let [a, b, c] = [t[0] + 1, t[1] + 1, t[2] + 1];
                writeln!(f, "f {a}/{a}/{a} {b}/{b}/{b} {c}/{c}/{c}").map_err(io)?;
            }
        }
        f.flush().map_err(io)?;
        m.flush().map_err(io)
    }

    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }
}

/// Gives new UVs to the texture sets (materials) that need them: all of them, or those with
/// an object that came without UVs. Triangle order is kept, so per-triangle data still lines
/// up. Returns the names of the sets unwrapped.
fn unwrap_raw(raw: &mut crate::import::RawMesh, all: bool, progress: &(dyn Fn(String) + Sync)) -> Result<Vec<String>, String> {
    let tris = raw.indices.len() / 3;
    let material = |t: usize| if raw.tri_material.len() == tris { raw.tri_material[t] } else { 0 };
    let needs: Vec<bool> = (0..tris)
        .map(|t| all || !raw.tri_object.get(t).and_then(|&o| raw.object_has_uv.get(o as usize)).copied().unwrap_or(false))
        .collect();
    let mut by_material: std::collections::BTreeMap<u32, Vec<usize>> = Default::default();
    for t in 0..tris {
        by_material.entry(material(t)).or_default().push(t);
    }
    by_material.retain(|_, set| set.iter().any(|&t| needs[t]));
    let count = by_material.len();
    let mut done = Vec::new();
    for (k, (m, set)) in by_material.into_iter().enumerate() {
        let name = material_name(&raw.material_names, m);
        // The set as a mesh of its own.
        let mut local: HashMap<u32, u32> = HashMap::new();
        let (mut global, mut positions, mut normals, mut indices) = (Vec::new(), Vec::new(), Vec::new(), Vec::with_capacity(set.len() * 3));
        for &t in &set {
            for &g in &raw.indices[t * 3..t * 3 + 3] {
                let l = *local.entry(g).or_insert_with(|| {
                    global.push(g);
                    positions.push(raw.positions[g as usize]);
                    if let Some(n) = &raw.normals {
                        normals.push(n[g as usize]);
                    }
                    (global.len() - 1) as u32
                });
                indices.push(l);
            }
        }
        let of = if count > 1 { format!(" ({}/{count})", k + 1) } else { String::new() };
        let report = |step: &str, pct: u32| progress(format!("Unwrapping {name}{of}: {step} {pct}%"));
        let flat = crate::unwrap::unwrap(&positions, raw.normals.as_ref().map(|_| normals.as_slice()), &indices, &report)
            .map_err(|e| format!("Couldn't unwrap {name}: {e}"))?;
        let base = raw.positions.len() as u32;
        for (&x, &uv) in flat.xref.iter().zip(&flat.uvs) {
            let g = global[x as usize] as usize;
            raw.positions.push(raw.positions[g]);
            if let Some(n) = &mut raw.normals {
                n.push(n[g]);
            }
            raw.uvs.push(uv);
        }
        for (k, &t) in set.iter().enumerate() {
            for c in 0..3 {
                raw.indices[t * 3 + c] = base + flat.indices[k * 3 + c];
            }
        }
        done.push(name);
    }
    // Drop the vertices only the replaced triangles used.
    let mut remap = vec![u32::MAX; raw.positions.len()];
    let mut kept = 0u32;
    for i in raw.indices.iter_mut() {
        if remap[*i as usize] == u32::MAX {
            remap[*i as usize] = kept;
            kept += 1;
        }
        *i = remap[*i as usize];
    }
    let mut order = vec![0usize; kept as usize];
    for (old, &new) in remap.iter().enumerate() {
        if new != u32::MAX {
            order[new as usize] = old;
        }
    }
    raw.positions = order.iter().map(|&o| raw.positions[o]).collect();
    raw.uvs = order.iter().map(|&o| raw.uvs[o]).collect();
    if let Some(n) = &mut raw.normals {
        *n = order.iter().map(|&o| n[o]).collect();
    }
    Ok(done)
}

pub fn smooth_normals(positions: &[Vec3], indices: &[u32]) -> Vec<Vec3> {
    // Accumulate over welded positions so UV seams don't show up as shading seams.
    let weld = weld_positions(positions);
    let mut acc = vec![Vec3::ZERO; positions.len()];
    for tri in indices.chunks_exact(3) {
        let [a, b, c] = [tri[0] as usize, tri[1] as usize, tri[2] as usize];
        let n = (positions[b] - positions[a]).cross(positions[c] - positions[a]);
        for v in [a, b, c] {
            acc[weld[v]] += n;
        }
    }
    (0..positions.len()).map(|i| acc[weld[i]].normalize_or_zero()).collect()
}

/// Every triangle edge once, as index pairs.
pub fn unique_edges(indices: &[u32]) -> Vec<u32> {
    let mut seen = std::collections::HashSet::new();
    let mut edges = Vec::new();
    for tri in indices.chunks_exact(3) {
        for (a, b) in [(tri[0], tri[1]), (tri[1], tri[2]), (tri[2], tri[0])] {
            let key = (a.min(b), a.max(b));
            if seen.insert(key) {
                edges.push(key.0);
                edges.push(key.1);
            }
        }
    }
    edges
}

/// Triangle edges once each, leaving out quad diagonals: an edge shared by
/// two nearly coplanar triangles that is the longest edge of both.
pub fn polygon_edges(vertices: &[Vertex], indices: &[u32]) -> Vec<u32> {
    let pos = |i: u32| Vec3::from_array(vertices[i as usize].pos);
    let mut users: HashMap<(u32, u32), Vec<usize>> = HashMap::with_capacity(indices.len());
    for (t, tri) in indices.chunks_exact(3).enumerate() {
        for (a, b) in [(tri[0], tri[1]), (tri[1], tri[2]), (tri[2], tri[0])] {
            users.entry((a.min(b), a.max(b))).or_default().push(t);
        }
    }
    let longest = |t: usize| {
        let tri = &indices[t * 3..t * 3 + 3];
        [(tri[0], tri[1]), (tri[1], tri[2]), (tri[2], tri[0])]
            .into_iter()
            .max_by(|x, y| pos(x.0).distance_squared(pos(x.1)).total_cmp(&pos(y.0).distance_squared(pos(y.1))))
            .map(|(a, b)| (a.min(b), a.max(b)))
            .expect("three edges")
    };
    let normal = |t: usize| {
        let tri = &indices[t * 3..t * 3 + 3];
        (pos(tri[1]) - pos(tri[0])).cross(pos(tri[2]) - pos(tri[0])).normalize_or_zero()
    };
    let mut edges = Vec::new();
    for tri in indices.chunks_exact(3) {
        for (a, b) in [(tri[0], tri[1]), (tri[1], tri[2]), (tri[2], tri[0])] {
            let key = (a.min(b), a.max(b));
            let Some(ts) = users.get_mut(&key) else { continue };
            let diagonal = ts.len() == 2 && longest(ts[0]) == key && longest(ts[1]) == key && normal(ts[0]).dot(normal(ts[1])) > 0.95;
            if !diagonal {
                edges.push(key.0);
                edges.push(key.1);
            }
            // Each edge once.
            users.remove(&key);
        }
    }
    edges
}

/// UV island borders: edges whose sides use different UVs (or that have one
/// side only), as index pairs of every triangle along them.
pub fn seam_edges(vertices: &[Vertex], indices: &[u32]) -> Vec<u32> {
    let positions: Vec<Vec3> = vertices.iter().map(|v| Vec3::from_array(v.pos)).collect();
    let weld = weld_positions(&positions);
    let uv_key = |i: u32| {
        let uv = vertices[i as usize].uv;
        [(uv[0] * 1e5).round() as i64, (uv[1] * 1e5).round() as i64]
    };
    // Geometric edge (welded ends, low first) → (its UVs at those ends, the index pair) per side.
    let mut sides: HashMap<(usize, usize), Vec<([[i64; 2]; 2], (u32, u32))>> = HashMap::with_capacity(indices.len());
    for tri in indices.chunks_exact(3) {
        for (a, b) in [(tri[0], tri[1]), (tri[1], tri[2]), (tri[2], tri[0])] {
            let (wa, wb) = (weld[a as usize], weld[b as usize]);
            if wa == wb {
                continue;
            }
            let (lo, hi) = if wa < wb { (a, b) } else { (b, a) };
            sides.entry((wa.min(wb), wa.max(wb))).or_default().push(([uv_key(lo), uv_key(hi)], (a, b)));
        }
    }
    let mut edges = Vec::new();
    for list in sides.values() {
        let seam = list.len() != 2 || list[0].0 != list[1].0;
        if seam {
            for (_, (a, b)) in list {
                edges.push(*a);
                edges.push(*b);
            }
        }
    }
    edges
}

/// Maps every vertex to a canonical vertex sharing (almost) the same position.
pub fn weld_positions(positions: &[Vec3]) -> Vec<usize> {
    let mut map: HashMap<[i32; 3], usize> = HashMap::with_capacity(positions.len());
    positions
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let key = (*p * 1.0e4).round().as_ivec3().to_array();
            *map.entry(key).or_insert(i)
        })
        .collect()
}

/// MikkTSpace tangents, the standard Blender, Unreal, Unity and Substance
/// use, so baked normal maps shade the same in every engine. MikkTSpace works
/// per face corner; vertices whose corners disagree (mirrored or split UVs)
/// are duplicated. Returns the new indices, the source vertex of each output
/// vertex, and the tangents (w: our handedness, see `compute_tangents`).
fn mikk_tangents(positions: &[Vec3], normals: &[Vec3], uvs: &[Vec2], indices: &[u32]) -> Option<(Vec<u32>, Vec<usize>, Vec<[f32; 4]>)> {
    struct Geo<'a> {
        positions: &'a [Vec3],
        normals: &'a [Vec3],
        uvs: &'a [Vec2],
        indices: &'a [u32],
        corners: Vec<[f32; 4]>,
    }
    impl mikktspace::Geometry for Geo<'_> {
        fn num_faces(&self) -> usize {
            self.indices.len() / 3
        }
        fn num_vertices_of_face(&self, _face: usize) -> usize {
            3
        }
        fn position(&self, face: usize, vert: usize) -> [f32; 3] {
            self.positions[self.indices[face * 3 + vert] as usize].to_array()
        }
        fn normal(&self, face: usize, vert: usize) -> [f32; 3] {
            self.normals[self.indices[face * 3 + vert] as usize].normalize_or_zero().to_array()
        }
        fn tex_coord(&self, face: usize, vert: usize) -> [f32; 2] {
            // Engines run MikkTSpace with V pointing up.
            let t = self.uvs[self.indices[face * 3 + vert] as usize];
            [t.x, 1.0 - t.y]
        }
        fn set_tangent_encoded(&mut self, t: [f32; 4], face: usize, vert: usize) {
            // V up flips the handedness relative to our V-down convention.
            self.corners[face * 3 + vert] = [t[0], t[1], t[2], -t[3]];
        }
    }
    let mut geo = Geo { positions, normals, uvs, indices, corners: vec![[0.0; 4]; indices.len()] };
    if indices.is_empty() || !mikktspace::generate_tangents(&mut geo) {
        return None;
    }
    // Degenerate triangles are skipped by MikkTSpace and their corners left
    // unset; they take the simple tangent of their vertex.
    let fallback = compute_tangents(positions, normals, uvs, indices);
    for (corner, t) in geo.corners.iter_mut().enumerate() {
        if t[0] * t[0] + t[1] * t[1] + t[2] * t[2] < 0.25 {
            *t = fallback[indices[corner] as usize];
        }
    }
    let same = |a: &[f32; 4], b: &[f32; 4]| a[3] == b[3] && (0..3).all(|k| (a[k] - b[k]).abs() < 1e-4);
    let mut source: Vec<usize> = Vec::with_capacity(positions.len());
    let mut tangents: Vec<[f32; 4]> = Vec::with_capacity(positions.len());
    // Output vertices made from each input vertex so far.
    let mut made: Vec<Vec<u32>> = vec![Vec::new(); positions.len()];
    let mut out = Vec::with_capacity(indices.len());
    for (corner, &v) in indices.iter().enumerate() {
        let t = geo.corners[corner];
        let v = v as usize;
        let found = made[v].iter().copied().find(|&o| same(&tangents[o as usize], &t));
        let o = found.unwrap_or_else(|| {
            let o = source.len() as u32;
            source.push(v);
            tangents.push(t);
            made[v].push(o);
            o
        });
        out.push(o);
    }
    Some((out, source, tangents))
}

fn compute_tangents(positions: &[Vec3], normals: &[Vec3], uvs: &[Vec2], indices: &[u32]) -> Vec<[f32; 4]> {
    let mut tan = vec![Vec3::ZERO; positions.len()];
    let mut bit = vec![Vec3::ZERO; positions.len()];
    for tri in indices.chunks_exact(3) {
        let [a, b, c] = [tri[0] as usize, tri[1] as usize, tri[2] as usize];
        let (e1, e2) = (positions[b] - positions[a], positions[c] - positions[a]);
        let (d1, d2) = (uvs[b] - uvs[a], uvs[c] - uvs[a]);
        let det = d1.x * d2.y - d2.x * d1.y;
        if det.abs() < 1e-12 {
            continue;
        }
        let r = 1.0 / det;
        let t = (e1 * d2.y - e2 * d1.y) * r;
        let bt = (e2 * d1.x - e1 * d2.x) * r;
        for v in [a, b, c] {
            tan[v] += t;
            bit[v] += bt;
        }
    }
    (0..positions.len())
        .map(|i| {
            let n = normals[i].normalize_or_zero();
            let mut t = tan[i] - n * n.dot(tan[i]);
            if t.length_squared() < 1e-12 {
                t = n.any_orthonormal_vector();
            }
            let t = t.normalize();
            // w encodes the handedness of the dP/dt direction (t points down the texture).
            let w = if n.cross(t).dot(bit[i]) < 0.0 { -1.0 } else { 1.0 };
            [t.x, t.y, t.z, w]
        })
        .collect()
}

type Parts = (Vec<Vec3>, Vec<Vec3>, Vec<Vec2>, Vec<u32>);

fn grid_indices(idx: &mut Vec<u32>, base: u32, cols: u32, rows: u32) {
    let stride = cols + 1;
    for y in 0..rows {
        for x in 0..cols {
            let i0 = base + y * stride + x;
            let (i1, i2, i3) = (i0 + 1, i0 + stride, i0 + stride + 1);
            idx.extend_from_slice(&[i0, i2, i1, i1, i2, i3]);
        }
    }
}

/// A cube with rounded edges; each face gets its own UV island in a 3x2 atlas.
fn rounded_cube(radius: f32) -> Parts {
    let inner = 1.0 - radius;
    // Dense sampling in the rounded bands, sparse on the flat part.
    let mut coords = Vec::new();
    let band = 8;
    let flat = 10;
    for i in 0..band {
        coords.push(-1.0 + radius * i as f32 / band as f32);
    }
    for i in 0..flat {
        coords.push(-inner + 2.0 * inner * i as f32 / flat as f32);
    }
    for i in 0..=band {
        coords.push(inner + radius * i as f32 / band as f32);
    }
    let n = coords.len() as u32 - 1;
    // UVs follow surface arc length so flat faces and bevels get even texel density.
    let half_len = inner + radius * std::f32::consts::FRAC_PI_4;
    let arc = |x: f32| {
        let s = if x.abs() <= inner { x } else { x.signum() * (inner + radius * ((x.abs() - inner) / radius).atan()) };
        (s + half_len) / (2.0 * half_len)
    };

    // (normal, u axis, v axis) with v pointing down the texture.
    let faces = [
        (Vec3::Z, Vec3::X, -Vec3::Y),
        (Vec3::X, -Vec3::Z, -Vec3::Y),
        (-Vec3::Z, -Vec3::X, -Vec3::Y),
        (-Vec3::X, Vec3::Z, -Vec3::Y),
        (Vec3::Y, Vec3::X, Vec3::Z),
        (-Vec3::Y, Vec3::X, -Vec3::Z),
    ];
    let cell = 1.0 / 3.0;
    let pad = 0.012;
    let (mut p, mut nn, mut uv, mut idx) = (vec![], vec![], vec![], vec![]);
    for (f, (fnrm, ua, va)) in faces.iter().enumerate() {
        let base = p.len() as u32;
        let (cx, cy) = ((f % 3) as f32 * cell, (f / 3) as f32 * cell);
        for &cyv in &coords {
            for &cxv in &coords {
                let q = *fnrm + *ua * cxv + *va * cyv;
                let core = q.clamp(Vec3::splat(-inner), Vec3::splat(inner));
                let dir = (q - core).normalize_or_zero();
                p.push(core + dir * radius);
                nn.push(dir);
                let (a, b) = (arc(cxv), arc(cyv));
                uv.push(Vec2::new(
                    cx + pad + a * (cell - 2.0 * pad),
                    cy + pad + b * (cell - 2.0 * pad),
                ));
            }
        }
        grid_indices(&mut idx, base, n, n);
    }
    // Windings follow the face orientation; flip faces whose basis is left-handed.
    fix_winding(&p, &nn, &mut idx);
    (p, nn, uv, idx)
}

/// Moves a part into place and its UVs into `rect` (u, v, width, height).
fn place(parts: Parts, rot: Mat3, scale: Vec3, offset: Vec3, rect: [f32; 4]) -> Parts {
    let (p, n, uv, idx) = parts;
    let p = p.into_iter().map(|p| rot * (p * scale) + offset).collect();
    let n = n.into_iter().map(|n| (rot * (n / scale)).normalize_or_zero()).collect();
    let uv = uv.into_iter().map(|t| Vec2::new(rect[0] + t.x * rect[2], rect[1] + t.y * rect[3])).collect();
    (p, n, uv, idx)
}

fn merge(parts: impl IntoIterator<Item = Parts>) -> Parts {
    let (mut p, mut nn, mut uv, mut idx) = (vec![], vec![], vec![], vec![]);
    for (pp, pn, puv, pidx) in parts {
        let base = p.len() as u32;
        idx.extend(pidx.into_iter().map(|i| i + base));
        p.extend(pp);
        nn.extend(pn);
        uv.extend(puv);
    }
    (p, nn, uv, idx)
}

/// Preview shape for the shelf: a ball sunk into a bevelled pedestal, with a
/// belt and a cross ring half buried in it and bolts on the pedestal. It has
/// sharp and soft edges, smooth curvature, contact creases and occluded
/// pockets, so every smart mask input shows up on it.
fn material_ball() -> Parts {
    let id = Mat3::IDENTITY;
    let ball_y = 0.12;
    let mut parts = vec![
        place(uv_sphere(96, 48), id, Vec3::splat(0.62), Vec3::Y * ball_y, [0.0, 0.0, 0.62, 0.62]),
        place(rounded_cube(0.25), id, Vec3::new(0.7, 0.24, 0.7), Vec3::Y * -0.68, [0.0, 0.64, 0.54, 0.36]),
        place(torus(0.62, 0.09, 128, 24), id, Vec3::ONE, Vec3::Y * ball_y, [0.56, 0.64, 0.44, 0.10]),
        place(
            torus(0.62, 0.06, 128, 16),
            Mat3::from_rotation_x(FRAC_PI_2),
            Vec3::ONE,
            Vec3::Y * ball_y,
            [0.56, 0.76, 0.44, 0.08],
        ),
    ];
    for (k, (x, z)) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)].into_iter().enumerate() {
        let rect = [0.64 + 0.09 * k as f32, 0.0, 0.08, 0.08];
        parts.push(place(uv_sphere(24, 12), id, Vec3::splat(0.06), Vec3::new(0.5 * x, -0.44, 0.5 * z), rect));
    }
    merge(parts)
}

fn uv_sphere(seg: u32, rings: u32) -> Parts {
    let (mut p, mut nn, mut uv, mut idx) = (vec![], vec![], vec![], vec![]);
    for r in 0..=rings {
        let t = r as f32 / rings as f32;
        let phi = t * PI;
        for s in 0..=seg {
            let u = s as f32 / seg as f32;
            let th = u * TAU;
            let d = Vec3::new(phi.sin() * th.sin(), phi.cos(), phi.sin() * th.cos());
            p.push(d);
            nn.push(d);
            uv.push(Vec2::new(u, t));
        }
    }
    grid_indices(&mut idx, 0, seg, rings);
    fix_winding(&p, &nn, &mut idx);
    (p, nn, uv, idx)
}

fn torus(major: f32, minor: f32, seg: u32, sides: u32) -> Parts {
    let (mut p, mut nn, mut uv, mut idx) = (vec![], vec![], vec![], vec![]);
    for j in 0..=sides {
        let v = j as f32 / sides as f32;
        let b = v * TAU;
        for i in 0..=seg {
            let u = i as f32 / seg as f32;
            let a = u * TAU;
            let ring = Vec3::new(a.sin(), 0.0, a.cos());
            let n = ring * b.cos() + Vec3::Y * b.sin();
            p.push(ring * major + n * minor);
            nn.push(n);
            uv.push(Vec2::new(u, v));
        }
    }
    grid_indices(&mut idx, 0, seg, sides);
    fix_winding(&p, &nn, &mut idx);
    (p, nn, uv, idx)
}

fn cylinder(seg: u32, rows: u32) -> Parts {
    let (mut p, mut nn, mut uv, mut idx) = (vec![], vec![], vec![], vec![]);
    let (r, h) = (0.6, 1.0);
    // Side wall takes the top half of the texture.
    for y in 0..=rows {
        let t = y as f32 / rows as f32;
        for s in 0..=seg {
            let u = s as f32 / seg as f32;
            let a = u * TAU;
            let d = Vec3::new(a.sin(), 0.0, a.cos());
            p.push(d * r + Vec3::Y * (h - 2.0 * h * t));
            nn.push(d);
            uv.push(Vec2::new(0.01 + u * 0.98, 0.01 + t * 0.48));
        }
    }
    grid_indices(&mut idx, 0, seg, rows);
    // Caps as discs in the bottom half.
    for (cap, y, ny, cx) in [(0, h, 1.0f32, 0.25f32), (1, -h, -1.0, 0.75)] {
        let _ = cap;
        let center = p.len() as u32;
        p.push(Vec3::new(0.0, y, 0.0));
        nn.push(Vec3::Y * ny);
        uv.push(Vec2::new(cx, 0.75));
        for s in 0..=seg {
            let a = s as f32 / seg as f32 * TAU;
            p.push(Vec3::new(a.sin() * r, y, a.cos() * r));
            nn.push(Vec3::Y * ny);
            uv.push(Vec2::new(cx + a.sin() * 0.23, 0.75 + a.cos() * 0.23 * ny));
        }
        for s in 0..seg {
            idx.extend_from_slice(&[center, center + 1 + s, center + 2 + s]);
        }
    }
    fix_winding(&p, &nn, &mut idx);
    (p, nn, uv, idx)
}

fn plane(n: u32) -> Parts {
    let (mut p, mut nn, mut uv, mut idx) = (vec![], vec![], vec![], vec![]);
    for y in 0..=n {
        for x in 0..=n {
            let (u, t) = (x as f32 / n as f32, y as f32 / n as f32);
            p.push(Vec3::new(u * 2.0 - 1.0, 0.0, t * 2.0 - 1.0));
            nn.push(Vec3::Y);
            uv.push(Vec2::new(u, t));
        }
    }
    grid_indices(&mut idx, 0, n, n);
    fix_winding(&p, &nn, &mut idx);
    (p, nn, uv, idx)
}

/// Makes every triangle counter-clockwise when seen from the side its normals face.
fn fix_winding(p: &[Vec3], n: &[Vec3], idx: &mut [u32]) {
    for tri in idx.chunks_exact_mut(3) {
        let [a, b, c] = [tri[0] as usize, tri[1] as usize, tri[2] as usize];
        let face = (p[b] - p[a]).cross(p[c] - p[a]);
        if face.dot(n[a] + n[b] + n[c]) < 0.0 {
            tri.swap(1, 2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MikkTSpace must keep our handedness convention (shaders read w) and
    /// agree with the simple tangents' direction where both are well defined.
    #[test]
    fn mikk_tangents_keep_the_convention() {
        for prim in [Primitive::RoundedCube, Primitive::Sphere, Primitive::Cylinder] {
            let (p, n, uv, idx) = match prim {
                Primitive::RoundedCube => rounded_cube(0.2),
                Primitive::Sphere => uv_sphere(48, 24),
                _ => cylinder(32, 8),
            };
            let simple = compute_tangents(&p, &n, &uv, &idx);
            let (new_idx, source, mikk) = mikk_tangents(&p, &n, &uv, &idx).expect("mikktspace runs");
            assert_eq!(new_idx.len(), idx.len());
            let (mut agree, mut total) = (0, 0);
            for (o, &v) in source.iter().enumerate() {
                let (a, b) = (Vec3::from_slice(&mikk[o][..3]), Vec3::from_slice(&simple[v][..3]));
                assert!((a.length() - 1.0).abs() < 1e-3, "unit tangents");
                assert!(a.dot(n[v].normalize()).abs() < 1e-2, "tangents lie in the surface");
                // Poles and seams are where the two methods may legitimately differ.
                if n[v].y.abs() < 0.9 {
                    total += 1;
                    if mikk[o][3] == simple[v][3] && a.dot(b) > 0.9 {
                        agree += 1;
                    }
                }
            }
            assert!(agree as f32 > total as f32 * 0.98, "{prim:?}: {agree}/{total} agree");
            // Corner splitting must only ever reference real vertices.
            assert!(new_idx.iter().all(|&i| (i as usize) < source.len()));
        }
    }
}

#[cfg(test)]
mod edge_tests {
    use super::*;

    #[test]
    fn quads_lose_their_diagonals_and_islands_get_seams() {
        // A plane is one quad island: 4 border edges, no diagonal, all seams (open borders).
        let plane = Primitive::Plane.build();
        let poly = polygon_edges(&plane.vertices, &plane.indices).len() / 2;
        let all = unique_edges(&plane.indices).len() / 2;
        assert!(poly < all, "diagonals removed: {poly} of {all}");
        // The rounded cube's six islands meet along seams.
        let cube = Primitive::RoundedCube.build();
        assert!(!cube.seam_edges.is_empty());
        assert!(cube.uv_edges.len() < unique_edges(&cube.indices).len());
        // Every seam edge's ends share positions with another island's edge (or a border).
        assert_eq!(cube.seam_edges.len() % 2, 0);
    }
}

#[cfg(test)]
pub(crate) mod set_tests {
    use super::*;

    /// A plane split by material: left half "Left", right half "Right".
    pub(crate) fn split_plane() -> Mesh {
        let mut m = Primitive::Plane.build();
        let side: Vec<u32> = m
            .indices
            .chunks_exact(3)
            .map(|t| {
                let u = t.iter().map(|&i| m.vertices[i as usize].uv[0]).sum::<f32>() / 3.0;
                if u < 0.5 { 0 } else { 1 }
            })
            .collect();
        m.split_by_material(&side, &["Left".into(), "Right".into()]);
        m
    }

    #[test]
    fn materials_become_contiguous_sets() {
        let plain = Primitive::Plane.build();
        let m = split_plane();
        assert_eq!(m.sets.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Left", "Right"]);
        assert_eq!(m.sets[0].tris.start, 0);
        assert_eq!(m.sets[0].tris.end, m.sets[1].tris.start);
        assert_eq!(m.sets[1].tris.end as usize, m.triangle_count());
        // The same triangles, reordered: each set's lie on its side of the UV square.
        let key = |idx: &[u32]| {
            let mut t: Vec<[u32; 3]> = idx.chunks_exact(3).map(|t| [t[0], t[1], t[2]]).collect();
            t.sort();
            t
        };
        assert_eq!(key(&plain.indices), key(&m.indices));
        for (k, s) in m.sets.iter().enumerate() {
            for t in m.indices[s.index_range().start as usize..s.index_range().end as usize].chunks_exact(3) {
                let u = t.iter().map(|&i| m.vertices[i as usize].uv[0]).sum::<f32>() / 3.0;
                assert_eq!(u >= 0.5, k == 1);
            }
            assert!(!s.edges.is_empty() && !s.seams.is_empty(), "{} has its own UV edges", s.name);
            assert_eq!(m.set_of(s.tris.start), k);
        }
        assert_eq!(m.sets[1].edges.end as usize, m.uv_edges.len());
    }

    #[test]
    fn set_names_are_unique_and_ranges_validated() {
        let mut m = Primitive::Plane.build();
        let tris = m.triangle_count();
        // Materials 0 and 2 share a name; material 1 has none.
        let mat: Vec<u32> = (0..tris).map(|t| (t % 3) as u32).collect();
        m.split_by_material(&mat, &["Body".into(), String::new(), "Body".into()]);
        let names: Vec<&str> = m.sets.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Body", "Material 2", "Body 2"]);
        // Gaps fall back to one set.
        let n = m.triangle_count() as u32;
        m.set_ranges(vec![("A".into(), 0..2), ("B".into(), 3..n)]);
        assert_eq!(m.sets.len(), 1);
        assert_eq!(m.sets[0].tris, 0..n);
    }

    /// A cube without UVs (material A) and a quad with UVs (material B), as OBJ + MTL.
    fn write_mixed_obj(dir: &std::path::Path) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("mixed.mtl"), "newmtl A
Kd 1 0 0
newmtl B
Kd 0 1 0
").unwrap();
        let mut obj = String::from("mtllib mixed.mtl
o cube
");
        for i in 0..8 {
            obj += &format!("v {} {} {}
", i & 1, i >> 1 & 1, i >> 2 & 1);
        }
        obj += "usemtl A
";
        for q in [[1, 3, 4, 2], [5, 6, 8, 7], [1, 2, 6, 5], [3, 7, 8, 4], [1, 5, 7, 3], [2, 4, 8, 6]] {
            obj += &format!("f {} {} {} {}
", q[0], q[1], q[2], q[3]);
        }
        obj += "o quad
v 2 0 0
v 3 0 0
v 3 1 0
v 2 1 0
vt 0 0
vt 0.5 0
vt 0.5 0.5
vt 0 0.5
usemtl B
f 9/1 10/2 11/3 12/4
";
        let path = dir.join("mixed.obj");
        std::fs::write(&path, obj).unwrap();
        path
    }

    fn set_uvs(m: &Mesh, set: usize) -> Vec<Vec2> {
        let r = m.sets[set].index_range();
        m.indices[r.start as usize..r.end as usize].iter().map(|&i| Vec2::from_array(m.vertices[i as usize].uv)).collect()
    }

    #[test]
    fn imports_unwrap_only_the_sets_without_uvs() {
        let dir = std::env::temp_dir().join("mp_unwrap_test");
        let path = write_mixed_obj(&dir);

        let err = Mesh::load_file_with(&path, AutoUnwrap::Off, true, &|_| {}).err().unwrap();
        assert!(err.contains("cube"), "{err}");

        let (m, unwrapped) = Mesh::load_file_with(&path, AutoUnwrap::Missing, true, &|_| {}).unwrap();
        assert_eq!(unwrapped, ["A"]);
        assert_eq!(m.sets.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["A", "B"]);
        assert_eq!(m.triangle_count(), 14);
        let a = set_uvs(&m, 0);
        assert!(a.iter().all(|uv| (0.0..=1.0).contains(&uv.x) && (0.0..=1.0).contains(&uv.y)));
        let area: f32 = a.chunks_exact(3).map(|t| (t[1] - t[0]).perp_dot(t[2] - t[0]).abs() * 0.5).sum();
        assert!(area > 0.3 && area <= 1.0, "set A covers {area}");
        // B keeps the file's UVs (texture-down).
        let b = set_uvs(&m, 1);
        assert!(b.iter().all(|uv| uv.x <= 0.5 && uv.y >= 0.5), "{b:?}");

        let (all, unwrapped) = Mesh::load_file_with(&path, AutoUnwrap::All, true, &|_| {}).unwrap();
        assert_eq!(unwrapped, ["A", "B"]);
        assert!(set_uvs(&all, 1).iter().any(|uv| uv.x > 0.5));

        // The exported OBJ carries the new UVs and the original placement.
        let out = dir.join("exported.obj");
        m.write_obj(&out).unwrap();
        let (back, unwrapped) = Mesh::load_file_with(&out, AutoUnwrap::Off, true, &|_| {}).unwrap();
        assert!(unwrapped.is_empty());
        assert_eq!(back.triangle_count(), m.triangle_count());
        assert_eq!(back.sets.len(), 2);
        assert!((back.center - m.center).length() < 1e-4 && (back.scale - m.scale).abs() < 1e-4);
        for (x, y) in set_uvs(&back, 0).iter().zip(&a) {
            assert!((*x - *y).length() < 1e-5);
        }
    }

    /// Quads at OBJ UVs (u, v-up) offsets, one triangle pair each, all material "Skin"
    /// unless `second` names the last quad's material.
    fn write_tiled_obj(path: &std::path::Path, quads: &[(f32, f32)], second: Option<&str>) {
        let mtl = path.with_extension("mtl");
        let mut s = format!("mtllib {}
o tiled
", mtl.file_name().unwrap().to_string_lossy());
        for (k, &(u, v)) in quads.iter().enumerate() {
            let x = k as f32 * 2.0;
            s += &format!("v {x} 0 0
v {} 0 0
v {} 1 0
v {x} 1 0
", x + 1.0, x + 1.0);
            s += &format!("vt {} {}
vt {} {}
vt {} {}
vt {} {}
", u + 0.1, v + 0.1, u + 0.9, v + 0.1, u + 0.9, v + 0.9, u + 0.1, v + 0.9);
        }
        for k in 0..quads.len() {
            let mat = if k + 1 == quads.len() { second.unwrap_or("Skin") } else { "Skin" };
            let i = k * 4 + 1;
            s += &format!("usemtl {mat}
f {i}/{i} {}/{} {}/{}
f {i}/{i} {}/{} {}/{}
", i + 1, i + 1, i + 2, i + 2, i + 2, i + 2, i + 3, i + 3);
        }
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, s).unwrap();
        std::fs::write(mtl, "newmtl Skin
Kd 1 1 1
newmtl Eyes
Kd 1 1 1
").unwrap();
    }

    #[test]
    fn udim_tiles_become_texture_sets() {
        let dir = std::env::temp_dir().join("mp_udim_test");
        let path = dir.join("tiled.obj");
        write_tiled_obj(&path, &[(0.0, 0.0), (1.0, 0.0), (2.0, 1.0)], None);
        let (m, _) = Mesh::load_file_with(&path, AutoUnwrap::Missing, true, &|_| {}).unwrap();
        assert_eq!(m.sets.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Skin.1001", "Skin.1002", "Skin.1013"]);
        assert_eq!(m.sets.iter().map(|s| s.udim).collect::<Vec<_>>(), [Some(1001), Some(1002), Some(1013)]);
        assert!(m.sets.iter().all(|s| s.base_name() == "Skin"));
        for k in 0..3 {
            assert!(set_uvs(&m, k).iter().all(|uv| (0.05..0.95).contains(&uv.x) && (0.05..0.95).contains(&uv.y)), "set {k}: {:?}", set_uvs(&m, k));
        }
        // Turned off, the file's UVs stay as they are, in one set.
        let (off, _) = Mesh::load_file_with(&path, AutoUnwrap::Missing, false, &|_| {}).unwrap();
        assert_eq!(off.sets.len(), 1);
        assert!(off.sets[0].udim.is_none());
        // The exported OBJ puts the tiles back.
        let out = dir.join("tiled_out.obj");
        m.write_obj(&out).unwrap();
        let (back, _) = Mesh::load_file_with(&out, AutoUnwrap::Off, false, &|_| {}).unwrap();
        let original: Vec<Vec2> = off.vertices.iter().map(|v| Vec2::from(v.uv)).collect();
        let exported: Vec<Vec2> = back.vertices.iter().map(|v| Vec2::from(v.uv)).collect();
        for uv in &exported {
            assert!(original.iter().any(|o| (*o - *uv).length() < 1e-5), "{uv} isn't in the file");
        }
    }

    #[test]
    fn udim_tiles_split_by_material_too() {
        let path = std::env::temp_dir().join("mp_udim_test").join("two_materials.obj");
        write_tiled_obj(&path, &[(0.0, 0.0), (1.0, 0.0), (0.0, 0.0)], Some("Eyes"));
        let (m, _) = Mesh::load_file_with(&path, AutoUnwrap::Missing, true, &|_| {}).unwrap();
        let mut names: Vec<&str> = m.sets.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["Eyes.1001", "Skin.1001", "Skin.1002"]);
    }

    #[test]
    fn repeating_uvs_are_not_udims() {
        // One quad tiled 3 times across: its triangles cross tile borders.
        let mut m = Mesh::from_parts(
            "floor".into(),
            vec![Vec3::ZERO, Vec3::X, Vec3::new(1.0, 1.0, 0.0)],
            None,
            vec![Vec2::new(0.0, 1.0), Vec2::new(3.0, 1.0), Vec2::new(3.0, -2.0)],
            vec![0, 1, 2],
        );
        assert!(m.split_into_udims().is_none());
        // A 0..1 layout isn't either.
        let mut m = Mesh::from_parts("a".into(), vec![Vec3::ZERO, Vec3::X, Vec3::Y], None, vec![Vec2::ZERO, Vec2::X, Vec2::Y], vec![0, 1, 2]);
        assert!(m.split_into_udims().is_none());
    }

    #[test]
    fn a_vertex_on_a_tile_border_is_split() {
        // Two triangles sharing vertices 1 and 2 at u = 1: one in tile 1001, one in 1002.
        let p = vec![Vec3::ZERO, Vec3::X, Vec3::new(1.0, 1.0, 0.0), Vec3::new(2.0, 0.0, 0.0)];
        // Texture-down v: OBJ v-up 0.1..0.9 is 0.9..0.1.
        let uv = vec![Vec2::new(0.2, 0.9), Vec2::new(1.0, 0.9), Vec2::new(1.0, 0.1), Vec2::new(1.8, 0.9)];
        let mut m = Mesh::from_parts("border".into(), p, None, uv, vec![0, 1, 2, 1, 3, 2]);
        let before = m.vertices.len();
        let tiles = m.split_into_udims().unwrap();
        assert_eq!(tiles, [1001, 1002]);
        assert!(m.vertices.len() > before, "border vertices copied");
        let uv = |t: usize, c: usize| Vec2::from(m.vertices[m.indices[t * 3 + c] as usize].uv);
        // In 1001 the border is at u = 1; in 1002 the same corner is at u = 0.
        assert!((uv(0, 1).x - 1.0).abs() < 1e-6);
        assert!(uv(1, 0).x.abs() < 1e-6 && (uv(1, 1).x - 0.8).abs() < 1e-6);
    }

    /// Times unwrapping real files: `MP_UNWRAP="a.fbx;b.obj" cargo test --release unwrap_files -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn unwrap_files() {
        for path in std::env::var("MP_UNWRAP").unwrap_or_default().split(';').filter(|p| !p.is_empty()) {
            let t = std::time::Instant::now();
            match Mesh::load_file_with(std::path::Path::new(path), AutoUnwrap::All, true, &|_| {}) {
                Ok((m, sets)) => println!("{path}: {} triangles, {} sets unwrapped in {:.2}s", m.triangle_count(), sets.len(), t.elapsed().as_secs_f32()),
                Err(e) => println!("{path}: {e}"),
            }
        }
    }
}
