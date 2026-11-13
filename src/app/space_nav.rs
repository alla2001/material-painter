//! 3D mouse navigation of the 3D view (the device itself: `crate::spacemouse`).
//!
//! The model follows the cap, as in 3Dconnexion's object mode: twisting turns it, tilting
//! tips it, pushing sideways or lifting moves it, and pulling brings it closer. The two side
//! buttons frame the model (F).

use super::PainterApp;
use crate::spacemouse::SpaceMouse;
use crate::viewport::OrbitCam;
use eframe::egui;

/// Deflection ignored around rest, so a resting hand doesn't drift the view.
const DEADZONE: f32 = 0.04;
/// Full twist or tilt: half a turn per second (times the orbit speed).
const ORBIT_RATE: f32 = std::f32::consts::PI;
/// Full push: the camera distance per second.
const PAN_RATE: f32 = 1.0;
/// Full pull: e^1.5 closer per second.
const ZOOM_RATE: f32 = 1.5;
/// The side buttons of every model (left/right; Menu and Fit on the Pro).
const FIT_BUTTONS: u32 = 0b11;

pub(super) struct SpaceNav {
    pub device: Option<SpaceMouse>,
    /// Why there's no device (shown in Preferences).
    pub problem: Option<String>,
    /// Look for a device on the next frame.
    pub scan: bool,
    /// The window had focus last frame.
    focused: bool,
}

impl Default for SpaceNav {
    fn default() -> Self {
        // Looks once at start.
        Self { device: None, problem: None, scan: true, focused: false }
    }
}

impl PainterApp {
    /// Connects the 3D mouse when needed and moves the 3D view with it; call every frame.
    pub(super) fn space_nav(&mut self, ctx: &egui::Context) {
        if !self.prefs.spacemouse {
            self.space.device = None;
            self.space.problem = None;
            self.space.scan = true;
            return;
        }
        let focused = ctx.input(|i| i.focused);
        let regained = focused && !self.space.focused;
        self.space.focused = focused;
        if self.space.device.as_ref().is_some_and(|d| !d.connected()) {
            self.space.device = None;
            self.space.problem = Some("The 3D mouse was disconnected.".into());
        }
        // A device plugged in later is found when the window gets focus back.
        if self.space.device.is_none() && (std::mem::take(&mut self.space.scan) || regained) {
            let wake = ctx.clone();
            match SpaceMouse::connect(move || wake.request_repaint()) {
                Ok(d) => {
                    self.space.device = Some(d);
                    self.space.problem = None;
                }
                Err(e) => self.space.problem = Some(e.to_string()),
            }
        }
        let Some(dev) = &self.space.device else { return };
        let (motion, pressed) = dev.take();
        // The device is read whatever window is in front; only move while this one is.
        if !focused {
            return;
        }
        if pressed & FIT_BUTTONS != 0 {
            self.cam = OrbitCam { yaw: self.cam.yaw, pitch: self.cam.pitch, fov_y: self.cam.fov_y, ..OrbitCam::default() };
        }
        if motion.is_still() {
            return;
        }
        let dz = |v: f32| if v.abs() < DEADZONE { 0.0 } else { (v - DEADZONE * v.signum()) / (1.0 - DEADZONE) };
        let [tx, ty, tz] = motion.translate.map(dz);
        let [rx, _, rz] = motion.rotate.map(dz);
        let p = &self.prefs;
        let dt = ctx.input(|i| i.stable_dt).min(0.1) * p.spacemouse_speed;
        let flip = |b: bool| if b { -1.0 } else { 1.0 };

        let k = ORBIT_RATE * p.orbit_speed * dt * flip(p.spacemouse_invert_rotate);
        self.cam.yaw += rz * k;
        self.cam.pitch = (self.cam.pitch + rx * k).clamp(-1.55, 1.55);
        let (right, up) = self.cam.basis();
        let k = PAN_RATE * self.cam.dist * dt * flip(p.spacemouse_invert_pan);
        self.cam.target += (-right * tx + up * tz) * k;
        let k = ZOOM_RATE * p.zoom_speed * dt * flip(p.spacemouse_invert_zoom);
        self.cam.dist = (self.cam.dist * (-ty * k).exp()).clamp(0.3, 40.0);
        // Held deflection keeps moving the view without new reports.
        ctx.request_repaint();
    }
}
