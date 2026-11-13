//! Surface selection: grows a connected region of triangles from a seed and
//! stops at creases, so a crystal set into a socket or a blade welded to a
//! guard can be masked even when the mesh is one piece and its UVs are split.

use crate::mesh::{Mesh, weld_positions};
use glam::{Vec2, Vec3};
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StopAt {
    /// Stop at concave creases (a part set into another); cross convex edges.
    Concave,
    /// Stop at every edge sharper than the angle.
    Any,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MaskMode {
    /// The mask becomes exactly the selection.
    Replace,
    Add,
    Subtract,
}

fn position(mesh: &Mesh, v: u32) -> Vec3 {
    Vec3::from_array(mesh.vertices[v as usize].pos)
}

fn uv(mesh: &Mesh, v: u32) -> Vec2 {
    Vec2::from_array(mesh.vertices[v as usize].uv)
}

fn closest_on_triangle(p: Vec3, a: Vec3, b: Vec3, c: Vec3) -> Vec3 {
    // Ericson, Real-Time Collision Detection 5.1.5.
    let (ab, ac, ap) = (b - a, c - a, p - a);
    let (d1, d2) = (ab.dot(ap), ac.dot(ap));
    if d1 <= 0.0 && d2 <= 0.0 {
        return a;
    }
    let bp = p - b;
    let (d3, d4) = (ab.dot(bp), ac.dot(bp));
    if d3 >= 0.0 && d4 <= d3 {
        return b;
    }
    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        return a + ab * (d1 / (d1 - d3));
    }
    let cp = p - c;
    let (d5, d6) = (ab.dot(cp), ac.dot(cp));
    if d6 >= 0.0 && d5 <= d6 {
        return c;
    }
    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        return a + ac * (d2 / (d2 - d6));
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        return b + (c - b) * ((d4 - d3) / ((d4 - d3) + (d5 - d6)));
    }
    let denom = 1.0 / (va + vb + vc);
    a + ab * (vb * denom) + ac * (vc * denom)
}

/// The triangle closest to `p` (a surface point from a ray hit, or any point).
pub fn nearest_triangle(mesh: &Mesh, p: Vec3) -> Option<usize> {
    mesh.indices
        .chunks_exact(3)
        .map(|t| {
            let q = closest_on_triangle(p, position(mesh, t[0]), position(mesh, t[1]), position(mesh, t[2]));
            q.distance_squared(p)
        })
        .enumerate()
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

/// Connected parts of the paint mesh, numbered as the bake report's id_regions.
pub fn mesh_parts(mesh: &Mesh) -> (Vec<u32>, u32) {
    let positions: Vec<Vec3> = mesh.vertices.iter().map(|v| Vec3::from_array(v.pos)).collect();
    crate::bake::components(&positions, &mesh.indices)
}

/// Flood fill over welded triangle edges from `seed`, not crossing edges
/// sharper than `crease_deg` (only concave ones with [`StopAt::Concave`]).
pub fn grow(mesh: &Mesh, seed: usize, crease_deg: f32, stop: StopAt) -> Vec<bool> {
    let positions: Vec<Vec3> = mesh.vertices.iter().map(|v| Vec3::from_array(v.pos)).collect();
    let weld = weld_positions(&positions);
    let tris: Vec<[u32; 3]> = mesh.indices.chunks_exact(3).map(|t| [t[0], t[1], t[2]]).collect();
    let normal = |t: &[u32; 3]| {
        let [a, b, c] = t.map(|v| positions[v as usize]);
        (b - a).cross(c - a).normalize_or_zero()
    };
    let centroid = |t: &[u32; 3]| t.iter().map(|&v| positions[v as usize]).sum::<Vec3>() / 3.0;

    let mut edges: HashMap<(usize, usize), Vec<usize>> = HashMap::new();
    for (i, t) in tris.iter().enumerate() {
        for k in 0..3 {
            let (a, b) = (weld[t[k] as usize], weld[t[(k + 1) % 3] as usize]);
            edges.entry((a.min(b), a.max(b))).or_default().push(i);
        }
    }

    let cos_limit = crease_deg.to_radians().cos();
    let mut selected = vec![false; tris.len()];
    let mut stack = vec![seed];
    selected[seed] = true;
    while let Some(i) = stack.pop() {
        let t = &tris[i];
        let (n, c) = (normal(t), centroid(t));
        for k in 0..3 {
            let (a, b) = (weld[t[k] as usize], weld[t[(k + 1) % 3] as usize]);
            for &j in &edges[&(a.min(b), a.max(b))] {
                if selected[j] {
                    continue;
                }
                let sharp = n.dot(normal(&tris[j])) < cos_limit;
                // The neighbour rises above this face's plane: the fold is concave.
                let concave = (centroid(&tris[j]) - c).dot(n) > 0.0;
                let blocked = sharp && (stop == StopAt::Any || concave);
                if !blocked {
                    selected[j] = true;
                    stack.push(j);
                }
            }
        }
    }
    selected
}

/// Signed distances (texels) from `p` to the three edges, positive inside.
fn edge_distances(p: Vec2, t: &[Vec2; 3]) -> [f32; 3] {
    let flip = if (t[1] - t[0]).perp_dot(t[2] - t[0]) < 0.0 { -1.0 } else { 1.0 };
    std::array::from_fn(|k| {
        let (a, b) = (t[k], t[(k + 1) % 3]);
        let e = b - a;
        let len = e.length().max(1e-9);
        flip * e.perp_dot(p - a) / len
    })
}

/// Visits texel centers within `pad` texels of the UV triangle.
fn raster(t: &[Vec2; 3], res: usize, pad: f32, mut f: impl FnMut(usize, f32)) {
    let lo = t[0].min(t[1]).min(t[2]) - Vec2::splat(pad + 1.0);
    let hi = t[0].max(t[1]).max(t[2]) + Vec2::splat(pad + 1.0);
    let (x0, y0) = (lo.x.max(0.0) as usize, lo.y.max(0.0) as usize);
    let (x1, y1) = ((hi.x.max(0.0) as usize).min(res), (hi.y.max(0.0) as usize).min(res));
    for y in y0..y1 {
        for x in x0..x1 {
            let d = edge_distances(Vec2::new(x as f32 + 0.5, y as f32 + 0.5), t);
            let inside = d[0].min(d[1]).min(d[2]);
            if inside >= -pad {
                f(y * res + x, inside);
            }
        }
    }
}

/// Writes the selection into an R8 mask of `res`² texels. The selection is
/// padded by a few texels so filtering doesn't pull in the outside, but the
/// padding never covers texels of unselected triangles.
/// `mask` holds `texel` bytes per texel (the painted mask's r first; any
/// other bytes, such as the value added over the smart mask, are cleared
/// where the selection writes).
pub fn write_mask(mesh: &Mesh, selected: &[bool], res: u32, mask: &mut [u8], texel: usize, mode: MaskMode) {
    const PAD: f32 = 3.0;
    let res_us = res as usize;
    let scale = res as f32;
    let uv_tri = |t: &[u32]| [uv(mesh, t[0]) * scale, uv(mesh, t[1]) * scale, uv(mesh, t[2]) * scale];

    // 0 = empty, 1 = selected surface, 2 = other surface.
    let mut owner = vec![0u8; res_us * res_us];
    for pass in [false, true] {
        for (t, &sel) in mesh.indices.chunks_exact(3).zip(selected) {
            if sel == pass {
                raster(&uv_tri(t), res_us, 0.0, |i, _| owner[i] = if sel { 1 } else { 2 });
            }
        }
    }
    let mut cover = vec![false; res_us * res_us];
    for (t, &sel) in mesh.indices.chunks_exact(3).zip(selected) {
        if sel {
            raster(&uv_tri(t), res_us, PAD, |i, _| {
                if owner[i] != 2 {
                    cover[i] = true;
                }
            });
        }
    }
    for (m, &c) in mask.chunks_exact_mut(texel).zip(&cover) {
        let r = match (mode, c) {
            (MaskMode::Subtract, true) => 0,
            (MaskMode::Replace | MaskMode::Add, true) => 255,
            (MaskMode::Replace, false) => 0,
            (_, false) => continue,
        };
        m[0] = r;
        m[1..].fill(0);
    }
}
