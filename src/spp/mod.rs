// Adapted from SubCraft (https://github.com/Aalzard/subcraft), `crates/mesh/src/spp/mod.rs`.
// Copyright (c) Aalzard and the SubCraft contributors. Licensed under the MIT License
// (see src/spp/LICENSE-SUBCRAFT); used here, with thanks, as part of Material Painter
// (GPL-3.0-or-later).

//! Reading Substance 3D Painter projects (`.spp`).
//!
//! An `.spp` is an HDF5 file (see [`hdf5`]). Inside it:
//! * `paint/scene.assbin`: the model ([`assbin`]).
//! * `paint/document.bin`: texture sets, channels and the layer stack ([`document`]).
//! * `resources.toc` + `resources/<sha1>`: baked mesh maps as JPEG 2000 tiles ([`jp2`], [`images`]).
//! * `texture-cache/<texture set uid>/<n>/<channel flag>`: the flattened channel textures as of the
//!   last save, LZ4-compressed ([`lz4`], [`images`]).
//!
//! Not recovered: procedural (`.sbsar`) layers, filters and generators cannot be re-evaluated (there
//! is no Substance engine); only their flattened result in the texture cache is available.
//! Material Painter never reads the contents of `.sbsar` archives (Adobe's assets): SubCraft's
//! archive and stencil-pack readers are left out. Format notes: SubCraft's `docs/spp-research.md`.

// Not every part of the reader is used (baked maps, image resources): kept whole, as SubCraft has it.
#![allow(dead_code, unused_imports)]

pub mod assbin;
pub mod document;
pub mod hdf5;
pub mod images;
pub mod jp2;
pub mod lz4;
pub mod qt;
#[cfg(test)]
pub mod fixture;

use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::Path;

pub use assbin::DumpMesh;
pub use document::{ChannelInfo, LayerInfo, LayerKind, ProjectInfo, SourceInfo, TextureSetInfo, channel_bits};
pub use images::{BakedKind, CacheFormat, CacheImage, Plane};

#[derive(Debug, thiserror::Error)]
pub enum SppError {
    #[error(transparent)]
    Hdf5(#[from] hdf5::Hdf5Error),
    #[error(transparent)]
    Mesh(#[from] assbin::AssbinError),
    #[error(transparent)]
    Document(#[from] document::DocumentError),
    #[error(transparent)]
    Image(#[from] images::ImageError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The meshes stored in a Painter project.
pub fn read_meshes(path: &Path) -> Result<Vec<DumpMesh>, SppError> {
    SppFile::open(path)?.meshes()
}

/// An open Painter project.
pub struct SppFile<R: Read + Seek = BufReader<File>> {
    h5: hdf5::Hdf5File<R>,
}

impl SppFile<BufReader<File>> {
    pub fn open(path: &Path) -> Result<Self, SppError> {
        Self::from_reader(BufReader::new(File::open(path)?))
    }
}

impl<R: Read + Seek> SppFile<R> {
    pub fn from_reader(r: R) -> Result<Self, SppError> {
        Ok(Self { h5: hdf5::Hdf5File::open(r)? })
    }

    fn bytes(&mut self, path: &str) -> Result<Vec<u8>, SppError> {
        let info = self.h5.dataset(path)?;
        Ok(self.h5.read(&info)?)
    }

    #[cfg(test)]
    pub(crate) fn bytes_for_probe(&mut self, path: &str) -> Vec<u8> {
        self.bytes(path).expect("dataset")
    }

    pub fn meshes(&mut self) -> Result<Vec<DumpMesh>, SppError> {
        Ok(assbin::parse(&self.bytes("paint/scene.assbin")?)?)
    }

    /// The meshes and the scene's material names (`DumpMesh::material` indexes them; each is
    /// the name of a texture set).
    pub fn meshes_and_materials(&mut self) -> Result<(Vec<DumpMesh>, Vec<String>), SppError> {
        let bytes = self.bytes("paint/scene.assbin")?;
        Ok((assbin::parse(&bytes)?, assbin::material_names(&bytes)))
    }

    /// Texture sets, channels and layer stack.
    pub fn project(&mut self) -> Result<ProjectInfo, SppError> {
        let root = document::parse(&self.bytes("paint/document.bin")?)?;
        Ok(document::project_info(&root))
    }

    /// Names of the baked mesh maps stored for the texture set called `set_name`.
    pub fn baked_map_kinds(&mut self, set_name: &str) -> Result<Vec<BakedKind>, SppError> {
        let toc = images::read_toc(&self.bytes("resources.toc")?);
        let mut kinds: Vec<BakedKind> = toc.keys().filter_map(|n| images::classify_baked(n)).filter(|(_, m)| *m == set_name).map(|(k, _)| k).collect();
        kinds.sort();
        Ok(kinds)
    }

    /// The stored (still encoded) blob of one baked mesh map; decode it with
    /// [`images::decode_resource_image`]. Reading is sequential, decoding is not: callers with
    /// several maps read them all first and decode them on all cores.
    pub fn baked_map_blob(&mut self, set_name: &str, kind: BakedKind) -> Result<Option<Vec<u8>>, SppError> {
        let toc = images::read_toc(&self.bytes("resources.toc")?);
        let Some(key) = toc.iter().find_map(|(n, k)| images::classify_baked(n).filter(|(bk, m)| *bk == kind && *m == set_name).map(|_| k.clone())) else {
            return Ok(None);
        };
        Ok(Some(self.bytes(&format!("resources/{key}"))?))
    }

    /// One baked mesh map of a texture set, reduced to at most `max_size`.
    pub fn baked_map(&mut self, set_name: &str, kind: BakedKind, max_size: u32) -> Result<Option<Plane>, SppError> {
        let Some(blob) = self.baked_map_blob(set_name, kind)? else { return Ok(None) };
        Ok(Some(images::decode_resource_image(&blob, max_size)?))
    }

    /// The stored blob of an imported image resource (`...?version=<sha1>.image` URL, as in bitmap
    /// sources and baked map references); decode it with [`images::decode_image_resource`].
    pub fn image_resource_blob(&mut self, url: &str) -> Result<Option<Vec<u8>>, SppError> {
        let Some((_, key)) = url.split_once("?version=") else { return Ok(None) };
        if !key.ends_with(".image") {
            return Ok(None);
        }
        let Ok(info) = self.h5.dataset(&format!("resources/{key}")) else { return Ok(None) };
        Ok(Some(self.h5.read(&info)?))
    }

    /// The stored (still encoded) flattened texture of one channel; see [`SppFile::channel_texture`].
    pub fn channel_blob(&mut self, set: &TextureSetInfo, bits: u64) -> Result<Option<Vec<u8>>, SppError> {
        let dir = format!("texture-cache/{}", set.uid);
        let Ok(subs) = self.h5.list(&dir) else { return Ok(None) };
        for sub in subs {
            let path = format!("{dir}/{sub}/{bits}");
            if let Ok(info) = self.h5.dataset(&path) {
                return Ok(Some(self.h5.read(&info)?));
            }
        }
        Ok(None)
    }

    /// The flattened texture of one channel (`bits` = [`channel_bits`] flag) of a texture set, if the
    /// project's texture cache holds it.
    pub fn channel_texture(&mut self, set: &TextureSetInfo, bits: u64, max_size: u32) -> Result<Option<CacheImage>, SppError> {
        let Some(blob) = self.channel_blob(set, bits)? else { return Ok(None) };
        Ok(Some(images::decode_cache_image(&blob, max_size)?))
    }
}

/// `<sha1>.sbsar` from a resource URL.
pub fn sbsar_key(url: &str) -> Option<&str> {
    let (_, key) = url.split_once("?version=")?;
    key.ends_with(".sbsar").then_some(key)
}

#[cfg(test)]
mod probe {
    /// Prints what a Painter project holds (`MP_SPP=path.spp`): for checking real files locally.
    #[test]
    #[ignore]
    fn spp_probe() {
        let Ok(path) = std::env::var("MP_SPP") else { return };
        for p in path.split(';') {
            let mut f = super::SppFile::open(std::path::Path::new(p)).expect("opens");
            if let Ok(dir) = std::env::var("MP_SPP_DUMP") {
                let doc = f.bytes("paint/document.bin").expect("document.bin");
                std::fs::write(std::path::Path::new(&dir).join("document.bin"), &doc).unwrap();
            }
            let meshes = f.meshes().expect("meshes");
            println!("{p}: {} meshes, {} triangles", meshes.len(), meshes.iter().map(|m| m.triangles.len()).sum::<usize>());
            let info = f.project().expect("document");
            for set in &info.texture_sets {
                println!("  set '{}' {}x{} channels {:?}", set.name, 1 << set.width_log2, 1 << set.height_log2, set.channels.iter().map(|c| &c.name).collect::<Vec<_>>());
                super::document::walk_layers(&set.layers, &mut |l, d| println!("    {}{:?} '{}' visible={} mask={} sources={}", "  ".repeat(d), l.kind, l.name, l.visible, l.has_mask, l.sources.len()), 0);
                for c in &set.channels {
                    match f.channel_texture(set, c.bits, 256) {
                        Ok(Some(img)) => println!("    cache {}: {:?} {}x{}", c.name, img.format, img.plane.width, img.plane.height),
                        Ok(None) => println!("    cache {}: none", c.name),
                        Err(e) => println!("    cache {}: {e}", c.name),
                    }
                }
            }
        }
    }
}
