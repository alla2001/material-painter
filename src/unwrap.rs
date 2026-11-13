//! Automatic UV unwrapping with xatlas (vendored in `vendor/xatlas`, built by `build.rs`):
//! the mesh is cut into charts, each flattened with low stretch, and the charts packed
//! into the 0..1 square without overlaps and with padding between them.

use glam::{Vec2, Vec3};
use std::ffi::{CStr, c_char, c_void};

/// Packing grid: padding is in its texels, so the gap between charts is the same share of
/// the texture at any texture size (4/1024: 8 texels at 2K, 16 at 4K).
const PACK_RESOLUTION: u32 = 1024;
const PACK_PADDING: u32 = 4;

/// A flattened mesh: new vertices (each refers to the input vertex it came from) and the
/// input's triangles, in order, over them.
pub struct Unwrapped {
    /// Input vertex of each output vertex.
    pub xref: Vec<u32>,
    pub uvs: Vec<Vec2>,
    pub indices: Vec<u32>,
}

/// A step of an unwrap and how far along it is (0..100), reported from xatlas's threads.
pub type Progress<'a> = &'a (dyn Fn(&'static str, u32) + Sync);

/// Unwraps a triangle mesh into one 0..1 layout. `normals` (per vertex) help it cut at hard
/// edges. Big smooth meshes take a while (a 180k-triangle torus: about two minutes), so
/// `progress` hears how it goes.
pub fn unwrap(positions: &[Vec3], normals: Option<&[Vec3]>, indices: &[u32], progress: Progress) -> Result<Unwrapped, String> {
    if indices.is_empty() || indices.len() % 3 != 0 {
        return Err("no triangles to unwrap".into());
    }
    let n = positions.len() as u32;
    if indices.iter().any(|&i| i >= n) {
        return Err("a triangle refers to a missing vertex".into());
    }
    // SAFETY: the declarations below match vendor/xatlas/xatlas_c.h; every pointer handed
    // to xatlas outlives the calls that read it, and the atlas is destroyed on every path.
    unsafe {
        let atlas = ffi::xatlasCreate();
        if atlas.is_null() {
            return Err("xatlas couldn't start".into());
        }
        // The callback gets a pointer to `progress`, which outlives the atlas.
        let mut report = progress;
        ffi::xatlasSetProgressCallback(atlas, Some(on_progress), (&mut report as *mut Progress).cast());
        let result = run(atlas, positions, normals.filter(|n| n.len() == positions.len()), indices);
        ffi::xatlasDestroy(atlas);
        result
    }
}

unsafe extern "C" fn on_progress(category: i32, progress: i32, user: *mut c_void) -> bool {
    let step = match category {
        0 => "reading",
        1 => "cutting charts",
        2 => "packing",
        _ => "finishing",
    };
    // SAFETY: `user` is the `Progress` set in `unwrap`, alive for the whole run.
    let report = unsafe { &*(user as *const Progress) };
    report(step, progress.clamp(0, 100) as u32);
    true
}

unsafe fn run(atlas: *mut ffi::Atlas, positions: &[Vec3], normals: Option<&[Vec3]>, indices: &[u32]) -> Result<Unwrapped, String> {
    unsafe {
        let mut decl = std::mem::zeroed::<ffi::MeshDecl>();
        ffi::xatlasMeshDeclInit(&mut decl);
        decl.vertexPositionData = positions.as_ptr().cast();
        decl.vertexPositionStride = std::mem::size_of::<Vec3>() as u32;
        decl.vertexCount = positions.len() as u32;
        if let Some(nrm) = normals {
            decl.vertexNormalData = nrm.as_ptr().cast();
            decl.vertexNormalStride = std::mem::size_of::<Vec3>() as u32;
        }
        decl.indexData = indices.as_ptr().cast();
        decl.indexCount = indices.len() as u32;
        decl.indexFormat = ffi::INDEX_FORMAT_UINT32;
        let err = ffi::xatlasAddMesh(atlas, &decl, 1);
        if err != 0 {
            let msg = CStr::from_ptr(ffi::xatlasAddMeshErrorString(err)).to_string_lossy();
            return Err(format!("xatlas rejected the mesh: {msg}"));
        }
        let mut charts = std::mem::zeroed::<ffi::ChartOptions>();
        ffi::xatlasChartOptionsInit(&mut charts);
        ffi::xatlasComputeCharts(atlas, &charts);

        let mut pack = std::mem::zeroed::<ffi::PackOptions>();
        ffi::xatlasPackOptionsInit(&mut pack);
        pack.resolution = PACK_RESOLUTION;
        pack.padding = PACK_PADDING;
        pack.bilinear = true;
        pack.blockAlign = true;
        // xatlas estimates the scale that fills one atlas of the resolution; when charts spill
        // into a second one, pack again a little smaller.
        let mut scale = 0.0f32;
        loop {
            pack.texelsPerUnit = scale;
            ffi::xatlasPackCharts(atlas, &pack);
            let a = &*atlas;
            if a.atlasCount <= 1 {
                break;
            }
            if scale != 0.0 && scale < a.texelsPerUnit * 1e-3 {
                return Err("the charts don't fit in one texture".into());
            }
            scale = a.texelsPerUnit * 0.95;
        }

        let a = &*atlas;
        if a.meshCount != 1 || a.width == 0 || a.height == 0 {
            return Err("xatlas produced no layout".into());
        }
        let m = &*a.meshes;
        let verts = std::slice::from_raw_parts(m.vertexArray, m.vertexCount as usize);
        let out_idx = std::slice::from_raw_parts(m.indexArray, m.indexCount as usize);
        if out_idx.len() != indices.len() {
            return Err("xatlas changed the triangle count".into());
        }
        let size = Vec2::new(a.width as f32, a.height as f32);
        Ok(Unwrapped {
            xref: verts.iter().map(|v| v.xref).collect(),
            uvs: verts.iter().map(|v| Vec2::from_array(v.uv) / size).collect(),
            indices: out_idx.to_vec(),
        })
    }
}

/// The parts of xatlas's C API used here (vendor/xatlas/xatlas_c.h).
#[allow(non_snake_case, dead_code)]
mod ffi {
    use super::*;

    pub const INDEX_FORMAT_UINT32: i32 = 1;

    #[repr(C)]
    pub struct Chart {
        pub faceArray: *mut u32,
        pub atlasIndex: u32,
        pub faceCount: u32,
        pub type_: i32,
        pub material: u32,
    }

    #[repr(C)]
    pub struct Vertex {
        pub atlasIndex: i32,
        pub chartIndex: i32,
        pub uv: [f32; 2],
        pub xref: u32,
    }

    #[repr(C)]
    pub struct Mesh {
        pub chartArray: *mut Chart,
        pub indexArray: *mut u32,
        pub vertexArray: *mut Vertex,
        pub chartCount: u32,
        pub indexCount: u32,
        pub vertexCount: u32,
    }

    #[repr(C)]
    pub struct Atlas {
        pub image: *mut u32,
        pub meshes: *mut Mesh,
        pub utilization: *mut f32,
        pub width: u32,
        pub height: u32,
        pub atlasCount: u32,
        pub chartCount: u32,
        pub meshCount: u32,
        pub texelsPerUnit: f32,
    }

    #[repr(C)]
    pub struct MeshDecl {
        pub vertexPositionData: *const c_void,
        pub vertexNormalData: *const c_void,
        pub vertexUvData: *const c_void,
        pub indexData: *const c_void,
        pub faceIgnoreData: *const bool,
        pub faceMaterialData: *const u32,
        pub faceVertexCount: *const u8,
        pub vertexCount: u32,
        pub vertexPositionStride: u32,
        pub vertexNormalStride: u32,
        pub vertexUvStride: u32,
        pub indexCount: u32,
        pub indexOffset: i32,
        pub faceCount: u32,
        pub indexFormat: i32,
        pub epsilon: f32,
    }

    #[repr(C)]
    pub struct ChartOptions {
        pub paramFunc: Option<unsafe extern "C" fn(*const f32, *mut f32, u32, *const u32, u32)>,
        pub maxChartArea: f32,
        pub maxBoundaryLength: f32,
        pub normalDeviationWeight: f32,
        pub roundnessWeight: f32,
        pub straightnessWeight: f32,
        pub normalSeamWeight: f32,
        pub textureSeamWeight: f32,
        pub maxCost: f32,
        pub maxIterations: u32,
        pub useInputMeshUvs: bool,
        pub fixWinding: bool,
    }

    #[repr(C)]
    pub struct PackOptions {
        pub maxChartSize: u32,
        pub padding: u32,
        pub texelsPerUnit: f32,
        pub resolution: u32,
        pub bilinear: bool,
        pub blockAlign: bool,
        pub bruteForce: bool,
        pub createImage: bool,
        pub rotateChartsToAxis: bool,
        pub rotateCharts: bool,
    }

    unsafe extern "C" {
        pub fn xatlasCreate() -> *mut Atlas;
        pub fn xatlasDestroy(atlas: *mut Atlas);
        pub fn xatlasAddMesh(atlas: *mut Atlas, decl: *const MeshDecl, mesh_count_hint: u32) -> i32;
        pub fn xatlasComputeCharts(atlas: *mut Atlas, options: *const ChartOptions);
        pub fn xatlasPackCharts(atlas: *mut Atlas, options: *const PackOptions);
        pub fn xatlasAddMeshErrorString(error: i32) -> *const c_char;
        pub fn xatlasMeshDeclInit(decl: *mut MeshDecl);
        pub fn xatlasChartOptionsInit(options: *mut ChartOptions);
        pub fn xatlasPackOptionsInit(options: *mut PackOptions);
        pub fn xatlasSetProgressCallback(
            atlas: *mut Atlas,
            func: Option<unsafe extern "C" fn(i32, i32, *mut c_void) -> bool>,
            user: *mut c_void,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cube with shared corners and no UVs.
    fn cube() -> (Vec<Vec3>, Vec<u32>) {
        let p: Vec<Vec3> = (0..8).map(|i| Vec3::new((i & 1) as f32, (i >> 1 & 1) as f32, (i >> 2 & 1) as f32)).collect();
        let quads = [[0, 2, 3, 1], [4, 5, 7, 6], [0, 1, 5, 4], [2, 6, 7, 3], [0, 4, 6, 2], [1, 3, 7, 5]];
        let idx = quads.iter().flat_map(|q| [q[0], q[1], q[2], q[0], q[2], q[3]]).collect();
        (p, idx)
    }

    #[test]
    fn unwraps_into_the_square_without_overlaps() {
        let (p, idx) = cube();
        let steps = std::sync::Mutex::new(Vec::new());
        let u = unwrap(&p, None, &idx, &|step, _| steps.lock().unwrap().push(step)).unwrap();
        assert!(steps.lock().unwrap().contains(&"packing"));
        assert_eq!(u.indices.len(), idx.len());
        assert!(u.uvs.iter().all(|uv| (0.0..=1.0).contains(&uv.x) && (0.0..=1.0).contains(&uv.y)));
        // Each output corner is the input corner it replaces.
        for (o, i) in u.indices.iter().zip(&idx) {
            assert_eq!(p[u.xref[*o as usize] as usize], p[*i as usize]);
        }
        // Rasterize: no texel center is inside two triangles.
        const R: usize = 256;
        let mut hits = vec![0u8; R * R];
        for t in u.indices.chunks_exact(3) {
            let [a, b, c] = [0, 1, 2].map(|k| u.uvs[t[k] as usize] * R as f32);
            let area = (b - a).perp_dot(c - a);
            assert!(area.abs() > 1.0, "a triangle collapsed");
            for y in 0..R {
                for x in 0..R {
                    let q = Vec2::new(x as f32 + 0.5, y as f32 + 0.5);
                    let w = [(c - b).perp_dot(q - b), (a - c).perp_dot(q - c), (b - a).perp_dot(q - a)];
                    if w.iter().all(|w| w * area.signum() > 0.0) {
                        hits[y * R + x] += 1;
                    }
                }
            }
        }
        assert!(hits.iter().all(|&h| h <= 1), "charts overlap");
        let used = hits.iter().filter(|&&h| h == 1).count() as f32 / (R * R) as f32;
        assert!(used > 0.3, "the layout uses {:.0}% of the square", used * 100.0);
    }
    /// How long a dense mesh takes: `cargo test --release unwrap_speed -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn unwrap_speed() {
        let n = 300u32;
        let p: Vec<Vec3> = (0..=n)
            .flat_map(|y| (0..=n).map(move |x| (x, y)))
            .map(|(x, y)| {
                // A bumpy torus (no poles, no collapsed triangles).
                let (u, v) = (x as f32 / n as f32 * std::f32::consts::TAU, y as f32 / n as f32 * std::f32::consts::TAU);
                let r = 0.35 * (1.0 + 0.1 * (u * 7.0).sin() * (v * 5.0).cos());
                Vec3::new((1.0 + r * v.cos()) * u.cos(), r * v.sin(), (1.0 + r * v.cos()) * u.sin())
            })
            .collect();
        let mut idx = Vec::new();
        for y in 0..n {
            for x in 0..n {
                let i = y * (n + 1) + x;
                idx.extend([i, i + n + 1, i + 1, i + 1, i + n + 1, i + n + 2]);
            }
        }
        let t = std::time::Instant::now();
        let u = unwrap(&p, None, &idx, &|_, _| {}).unwrap();
        println!("{} triangles unwrapped in {:.2}s ({} vertices)", idx.len() / 3, t.elapsed().as_secs_f32(), u.xref.len());
    }
}
