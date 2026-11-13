//! Importing a mesh file on a worker thread (reading a big file and unwrapping it can take
//! seconds), and exporting the mesh with its UVs.

use super::PainterApp;
use crate::mesh::{AutoUnwrap, Mesh};
use eframe::egui;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};

type Loaded = Result<(Mesh, Vec<String>), String>;

/// The import running, if any.
#[derive(Default)]
pub(super) struct MeshImport {
    job: Option<(PathBuf, mpsc::Receiver<Loaded>)>,
    /// What the worker is doing (unwrap steps), for the status bar.
    progress: Arc<Mutex<Option<String>>>,
}

impl PainterApp {
    pub(super) fn start_mesh_import(&mut self, path: PathBuf, ctx: &egui::Context) {
        if self.mesh_import.job.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let (p, ctx, mode, udims) = (path.clone(), ctx.clone(), self.prefs.auto_unwrap, self.prefs.udim_tiles);
        let progress = Arc::new(Mutex::new(None));
        self.mesh_import.progress = progress.clone();
        std::thread::spawn(move || {
            let report = |text: String| {
                let mut p = progress.lock().unwrap_or_else(|e| e.into_inner());
                if p.as_deref() != Some(text.as_str()) {
                    *p = Some(text);
                    ctx.request_repaint();
                }
            };
            let _ = tx.send(Mesh::load_file_with(&p, mode, udims, &report));
            ctx.request_repaint();
        });
        self.status = match mode {
            AutoUnwrap::Off => format!("Importing {}…", path.display()),
            _ => format!("Importing {} (unwrapping where needed)…", path.display()),
        };
        self.mesh_import.job = Some((path, rx));
    }

    pub(super) fn mesh_import_busy(&self) -> bool {
        self.mesh_import.job.is_some()
    }

    /// Picks up a finished import; call every frame.
    pub(super) fn poll_mesh_import(&mut self, ctx: &egui::Context) {
        let Some((path, rx)) = &self.mesh_import.job else { return };
        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => {
                if let Some(text) = self.mesh_import.progress.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    self.status = format!("{text}…");
                }
                return;
            }
            Err(mpsc::TryRecvError::Disconnected) => Err(format!("Importing {} stopped unexpectedly.", path.display())),
        };
        self.mesh_import.job = None;
        match result {
            Ok((mesh, unwrapped)) => {
                let mut tiles: Vec<u32> = mesh.sets.iter().filter_map(|s| s.udim).collect();
                tiles.sort_unstable();
                tiles.dedup();
                self.load_mesh(mesh, ctx);
                if !unwrapped.is_empty() {
                    self.status = format!(
                        "New UVs for {}. To use the textures elsewhere, take the mesh along: File > Export mesh.",
                        unwrapped.join(", ")
                    );
                } else if let (Some(first), Some(last)) = (tiles.first(), tiles.last()) {
                    self.status = format!(
                        "UDIM tiles {first}–{last}: each tile is a texture set, exported as <name>.<tile>. (Preferences > General > UDIM tiles)"
                    );
                }
            }
            Err(e) => self.status = e,
        }
    }

    /// Saves the mesh as OBJ with its UVs (unwrapped ones included), for use with the
    /// exported textures.
    pub(super) fn export_mesh(&mut self) {
        let Some(path) = rfd::FileDialog::new().add_filter("OBJ", &["obj"]).set_file_name(format!("{}.obj", self.mesh.name)).save_file() else {
            return;
        };
        self.status = match self.mesh.write_obj(&path) {
            Ok(()) => format!("Saved {}", path.display()),
            Err(e) => format!("Couldn't save {}: {e}", path.display()),
        };
    }
}
