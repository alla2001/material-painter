//! Updates from inside the app: asks GitHub for a newer release (at start unless turned
//! off, and from Help > Check for updates), downloads the package for this install, and
//! installs it when the app closes (`crate::update`).

use super::{Action, PainterApp};
use crate::theme;
use crate::update::{self, Install, Release, Version};
use eframe::egui;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

const PROBLEM: egui::Color32 = egui::Color32::from_rgb(0xe8, 0x9a, 0x3c);

enum Msg {
    Checked(Result<Release, String>),
    Progress(u64, u64),
    Prepared(Result<update::Pending, String>),
}

#[derive(Default)]
enum State {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Failed(String),
    Available(Release),
    Downloading {
        release: Release,
        done: u64,
        total: u64,
    },
    /// Downloaded and unpacked; goes in when the app closes.
    Ready(Release),
}

#[derive(Default)]
pub(super) struct Updates {
    state: State,
    rx: Option<mpsc::Receiver<Msg>>,
    cancel: Arc<AtomicBool>,
    /// A download that failed, shown with the release.
    error: Option<String>,
    /// The window is showing.
    open: bool,
    /// Checked from the menu: the answer shows even when there's nothing new.
    manual: bool,
}

enum Choice {
    Retry,
    Download,
    Skip,
    Later,
    Cancel,
    Restart,
    Page(String),
}

impl PainterApp {
    /// Asks GitHub for the latest release on a worker thread. A `manual` check opens the
    /// window with whatever it finds; the one at start speaks up only for a new version.
    pub(super) fn check_for_updates(&mut self, ctx: &egui::Context, manual: bool) {
        let u = &mut self.updates;
        if manual {
            u.open = true;
        }
        if matches!(u.state, State::Checking | State::Downloading { .. } | State::Ready(_)) {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new().name("update check".into()).spawn(move || {
            let _ = tx.send(Msg::Checked(update::latest(&update::agent())));
            ctx.request_repaint();
        });
        if spawned.is_ok() {
            (u.rx, u.state, u.manual, u.error) = (Some(rx), State::Checking, manual, None);
        }
    }

    fn start_update_download(&mut self, release: Release, ctx: &egui::Context) {
        let install = match Install::this() {
            Ok(i) => i.clone(),
            Err(e) => {
                self.updates.state = State::Available(release);
                self.updates.error = Some(e.clone());
                return;
            }
        };
        let Some(asset) = install.asset(&release).cloned() else {
            self.updates.state = State::Available(release);
            return;
        };
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let (stop, ctx) = (cancel.clone(), ctx.clone());
        let total = asset.size;
        let spawned = std::thread::Builder::new().name("update download".into()).spawn(move || {
            let progress = |done, total| {
                let _ = tx.send(Msg::Progress(done, total));
                ctx.request_repaint();
            };
            let agent = update::agent();
            let result = update::download(&agent, &asset, &stop, &progress).and_then(|package| update::prepare(&install, &package));
            let _ = tx.send(Msg::Prepared(result));
            ctx.request_repaint();
        });
        let u = &mut self.updates;
        match spawned {
            Ok(_) => (u.rx, u.cancel, u.error, u.state) = (Some(rx), cancel, None, State::Downloading { release, done: 0, total }),
            Err(e) => (u.state, u.error) = (State::Available(release), Some(format!("Couldn't start the download: {e}"))),
        }
    }

    /// Takes in what the update threads found; call every frame.
    pub(super) fn poll_updates(&mut self) {
        let Some(rx) = &self.updates.rx else { return };
        let msgs: Vec<Msg> = rx.try_iter().collect();
        for msg in msgs {
            let u = &mut self.updates;
            match msg {
                Msg::Progress(d, t) => {
                    if let State::Downloading { done, total, .. } = &mut u.state {
                        (*done, *total) = (d, t);
                    }
                }
                Msg::Checked(result) => {
                    u.rx = None;
                    u.state = match result {
                        Ok(r) if r.version > Version::current() => {
                            if u.manual || r.version.to_string() != self.prefs.skipped_update {
                                u.open = true;
                                State::Available(r)
                            } else {
                                State::Idle
                            }
                        }
                        Ok(_) => State::UpToDate,
                        // A check at start fails quietly (offline, say).
                        Err(e) if u.manual => State::Failed(e),
                        Err(_) => State::Idle,
                    };
                }
                Msg::Prepared(result) => {
                    u.rx = None;
                    let State::Downloading { release, .. } = std::mem::take(&mut u.state) else { continue };
                    match result {
                        Ok(pending) => {
                            update::apply_on_exit(pending);
                            u.state = State::Ready(release);
                            u.open = true;
                        }
                        Err(e) => {
                            u.error = (e != update::CANCELLED).then_some(e);
                            u.state = State::Available(release);
                        }
                    }
                }
            }
        }
    }

    /// The status bar's way back to a found or downloaded update.
    pub(super) fn update_status_button(&mut self, ui: &mut egui::Ui) {
        let text = match &self.updates.state {
            State::Available(r) => format!("Update to {}", r.version),
            State::Downloading { done, total, .. } if *total > 0 => format!("Downloading update {:.0}%", *done as f64 * 100.0 / *total as f64),
            State::Ready(_) => "Restart to update".to_string(),
            _ => return,
        };
        let button = egui::Button::new(egui::RichText::new(text).color(theme::ACCENT).small()).frame(false);
        if ui.add(button).on_hover_text("A new version of Material Painter").clicked() {
            self.updates.open = true;
        }
    }

    pub(super) fn update_window(&mut self, ctx: &egui::Context) {
        if !self.updates.open {
            return;
        }
        let current = Version::current();
        let install = Install::this();
        let mut open = true;
        let mut choice = None;
        egui::Window::new("Updates").open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
            ui.set_width(440.0);
            match &self.updates.state {
                State::Idle | State::Checking => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Looking for a newer version…");
                    });
                }
                State::UpToDate => {
                    ui.label(format!("Material Painter {current} is the latest version."));
                }
                State::Failed(e) => {
                    ui.label("Couldn't check for updates.");
                    ui.label(egui::RichText::new(e).color(PROBLEM));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Try again").clicked() {
                            choice = Some(Choice::Retry);
                        }
                        if ui.button("Release page").clicked() {
                            choice = Some(Choice::Page(update::RELEASES_PAGE.into()));
                        }
                    });
                }
                State::Available(r) => {
                    ui.heading(format!("Material Painter {} is out", r.version));
                    ui.label(egui::RichText::new(format!("You have {current}.")).color(theme::TEXT_DIM));
                    release_notes(ui, &r.notes);
                    if let Some(e) = &self.updates.error {
                        ui.label(egui::RichText::new(e).color(PROBLEM));
                    }
                    ui.add_space(6.0);
                    let package = install.as_ref().ok().and_then(|i| i.asset(r).map(|a| (i, a)));
                    match (install, package) {
                        (_, Some((_, asset))) if asset.signature_url.is_none() => {
                            ui.label(egui::RichText::new(format!("{} Get it from the release page.", update::UNSIGNED)).color(theme::TEXT_DIM));
                        }
                        (_, Some((install, asset))) => {
                            let label = format!("Download and install ({:.0} MB)", asset.size as f64 / (1024.0 * 1024.0));
                            if ui.add(egui::Button::new(egui::RichText::new(label).strong())).clicked() {
                                choice = Some(Choice::Download);
                            }
                            let note = if install.uses_setup() {
                                "It installs when you close the app; the installer may ask for permission."
                            } else {
                                "It installs when you close the app."
                            };
                            ui.label(egui::RichText::new(note).color(theme::TEXT_DIM).small());
                        }
                        (Err(why), _) => {
                            ui.label(egui::RichText::new(format!("{why} Get the new version from the release page.")).color(theme::TEXT_DIM));
                        }
                        (Ok(_), None) => {
                            ui.label(egui::RichText::new("The release has no package for this system. Get it from the release page.").color(theme::TEXT_DIM));
                        }
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Release page").clicked() {
                            choice = Some(Choice::Page(r.page.clone()));
                        }
                        if ui.button("Skip this version").on_hover_text("Don't mention this version again at start").clicked() {
                            choice = Some(Choice::Skip);
                        }
                        if ui.button("Later").clicked() {
                            choice = Some(Choice::Later);
                        }
                    });
                }
                State::Downloading { release, done, total } => {
                    ui.label(format!("Downloading Material Painter {}…", release.version));
                    let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
                    let frac = if *total > 0 { *done as f32 / *total as f32 } else { 0.0 };
                    ui.add(egui::ProgressBar::new(frac).text(format!("{:.1} / {:.1} MB", mb(*done), mb(*total))));
                    if ui.button("Cancel").clicked() {
                        choice = Some(Choice::Cancel);
                    }
                }
                State::Ready(r) => {
                    ui.heading(format!("Material Painter {} is ready", r.version));
                    ui.label("It installs when you close the app.");
                    if install.as_ref().is_ok_and(Install::uses_setup) {
                        ui.label(egui::RichText::new("The installer shows its progress and may ask for permission.").color(theme::TEXT_DIM));
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.add(egui::Button::new(egui::RichText::new("Restart now").strong())).on_hover_text("Asks to save unsaved changes first").clicked() {
                            choice = Some(Choice::Restart);
                        }
                        if ui.button("When I close the app").clicked() {
                            choice = Some(Choice::Later);
                        }
                    });
                }
            }
        });
        if !open {
            self.updates.open = false;
        }
        match choice {
            None => {}
            Some(Choice::Retry) => self.check_for_updates(ctx, true),
            Some(Choice::Page(url)) => ctx.open_url(egui::OpenUrl::new_tab(url)),
            Some(Choice::Later) => self.updates.open = false,
            Some(Choice::Skip) => {
                if let State::Available(r) = &self.updates.state {
                    self.prefs.skipped_update = r.version.to_string();
                }
                (self.updates.state, self.updates.open) = (State::Idle, false);
            }
            Some(Choice::Download) => {
                if let State::Available(r) = std::mem::take(&mut self.updates.state) {
                    self.start_update_download(r, ctx);
                }
            }
            Some(Choice::Cancel) => self.updates.cancel.store(true, Ordering::Relaxed),
            Some(Choice::Restart) => {
                update::set_relaunch(true);
                self.updates.open = false;
                self.pending.push(Action::Exit);
            }
        }
    }
}

/// GitHub's release notes, lightly tidied: headings bold, list items as bullets.
fn release_notes(ui: &mut egui::Ui, notes: &str) {
    if notes.is_empty() {
        return;
    }
    ui.add_space(6.0);
    egui::Frame::new().fill(theme::PANEL_DARK).inner_margin(8.0).corner_radius(4.0).show(ui, |ui| {
        egui::ScrollArea::vertical().max_height(220.0).auto_shrink([false, true]).show(ui, |ui| {
            for line in notes.lines().map(str::trim_end) {
                if let Some(h) = line.trim_start_matches('#').strip_prefix(' ').filter(|_| line.starts_with('#')) {
                    ui.label(egui::RichText::new(h).strong());
                } else if let Some(item) = line.strip_prefix("* ").or_else(|| line.strip_prefix("- ")) {
                    ui.label(format!("•  {item}"));
                } else if !line.is_empty() {
                    ui.label(line);
                }
            }
        });
    });
}
