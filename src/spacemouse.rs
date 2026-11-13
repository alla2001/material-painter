//! 3D mice (3Dconnexion SpaceMouse and other six-axis controllers), read straight from the USB
//! HID device. No vendor driver or SDK is used: the device sends its six axes and buttons as
//! plain HID input reports, which a worker thread decodes as they arrive.
//!
//! Report layout (the same on every 3Dconnexion model):
//! - id 1: translation x, y, z as little-endian i16 (newer models append rotation, 13 bytes);
//! - id 2: rotation x, y, z;
//! - id 3: buttons, one bit each.
//!
//! Axes: x right, y toward the user, z down; rotations about those axes (right-handed).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Full deflection reported by the devices (some reach a little past it).
const FULL_SCALE: f32 = 350.0;

/// Usage page and usage of a multi-axis controller (HID Generic Desktop).
const USAGE_PAGE: u16 = 0x01;
const USAGE_MULTI_AXIS: u16 = 0x08;

/// 3Dconnexion's 3D mice, for systems that don't report the usage of a device.
const KNOWN: &[(u16, u16)] = &[
    (0x046d, 0xc603), // SpaceMouse Plus XT
    (0x046d, 0xc605), // CadMan
    (0x046d, 0xc606), // SpaceMouse Classic
    (0x046d, 0xc621), // SpaceBall 5000
    (0x046d, 0xc623), // SpaceTraveler
    (0x046d, 0xc625), // SpacePilot
    (0x046d, 0xc626), // SpaceNavigator
    (0x046d, 0xc627), // SpaceExplorer
    (0x046d, 0xc628), // SpaceNavigator for Notebooks
    (0x046d, 0xc629), // SpacePilot Pro
    (0x046d, 0xc62b), // SpaceMouse Pro
    (0x256f, 0xc62e), // SpaceMouse Wireless (cable)
    (0x256f, 0xc62f), // SpaceMouse Wireless (receiver)
    (0x256f, 0xc631), // SpaceMouse Pro Wireless (cable)
    (0x256f, 0xc632), // SpaceMouse Pro Wireless (receiver)
    (0x256f, 0xc633), // SpaceMouse Enterprise
    (0x256f, 0xc635), // SpaceMouse Compact
    (0x256f, 0xc636), // SpaceMouse Module
    (0x256f, 0xc652), // Universal Receiver
];

/// What the device reports, each axis in -1..1.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct Motion {
    pub translate: [f32; 3],
    pub rotate: [f32; 3],
}

impl Motion {
    pub fn is_still(&self) -> bool {
        self.translate.iter().chain(&self.rotate).all(|v| *v == 0.0)
    }
}

#[derive(Default)]
struct State {
    motion: Motion,
    buttons: u32,
    /// Buttons pressed since the app last asked.
    pressed: u32,
}

struct Shared {
    state: Mutex<State>,
    /// The reader thread is running (cleared when the device goes away).
    alive: AtomicBool,
    /// Asks the reader thread to stop after the next report.
    stop: AtomicBool,
}

/// A connected 3D mouse. Dropping it lets the reader thread end.
pub struct SpaceMouse {
    shared: Arc<Shared>,
    name: String,
}

/// Why no device could be used.
#[derive(Debug, PartialEq)]
pub enum ConnectError {
    NotFound,
    /// Found but couldn't be opened (on Linux, usually /dev/hidraw permissions).
    Open(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::NotFound => write!(f, "No 3D mouse found."),
            ConnectError::Open(name) => write!(f, "{name} was found but couldn't be opened."),
        }
    }
}

impl SpaceMouse {
    /// Opens the first 3D mouse plugged in and starts reading it. `wake` runs whenever the
    /// device reports something (or goes away), so the app can repaint.
    pub fn connect(wake: impl Fn() + Send + 'static) -> Result<Self, ConnectError> {
        let api = hidapi::HidApi::new().map_err(|_| ConnectError::NotFound)?;
        let candidates: Vec<&hidapi::DeviceInfo> = api.device_list().filter(|d| is_3d_mouse(d)).collect();
        // The multi-axis collection of a device when the system lists its collections apart.
        let info = candidates
            .iter()
            .find(|d| d.usage_page() == USAGE_PAGE && d.usage() == USAGE_MULTI_AXIS)
            .or_else(|| candidates.first())
            .ok_or(ConnectError::NotFound)?;
        let name = info.product_string().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("3D mouse").to_string();
        let device = info.open_device(&api).map_err(|_| ConnectError::Open(name.clone()))?;
        device.set_blocking_mode(true).map_err(|_| ConnectError::Open(name.clone()))?;

        let shared = Arc::new(Shared { state: Mutex::new(State::default()), alive: AtomicBool::new(true), stop: AtomicBool::new(false) });
        let reader = shared.clone();
        std::thread::Builder::new()
            .name("3d mouse".into())
            .spawn(move || {
                let mut buf = [0u8; 64];
                while !reader.stop.load(Ordering::Relaxed) {
                    let Ok(n) = device.read(&mut buf) else { break };
                    let changed = {
                        let mut s = reader.state.lock().unwrap_or_else(|e| e.into_inner());
                        let before = (s.motion, s.buttons);
                        apply_report(&mut s, &buf[..n]);
                        before != (s.motion, s.buttons)
                    };
                    if changed {
                        wake();
                    }
                }
                *reader.state.lock().unwrap_or_else(|e| e.into_inner()) = State::default();
                reader.alive.store(false, Ordering::Relaxed);
                wake();
            })
            .map_err(|_| ConnectError::Open(name.clone()))?;
        Ok(SpaceMouse { shared, name })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// False once the device was unplugged.
    pub fn connected(&self) -> bool {
        self.shared.alive.load(Ordering::Relaxed)
    }

    /// The current deflection, and the buttons pressed since the last call (one bit each).
    pub fn take(&self) -> (Motion, u32) {
        let mut s = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let pressed = std::mem::take(&mut s.pressed);
        (s.motion, pressed)
    }
}

impl Drop for SpaceMouse {
    fn drop(&mut self) {
        // The thread is blocked in a read; it ends on the device's next report.
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

fn is_3d_mouse(d: &hidapi::DeviceInfo) -> bool {
    if d.usage_page() == USAGE_PAGE && d.usage() == USAGE_MULTI_AXIS {
        return true;
    }
    // Usage unknown (0): go by the model. A known model's other collections (keyboard,
    // mouse of a receiver) report their own usage and are skipped.
    d.usage_page() == 0 && KNOWN.contains(&(d.vendor_id(), d.product_id()))
}

fn axes(bytes: &[u8]) -> [f32; 3] {
    std::array::from_fn(|i| (i16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]) as f32 / FULL_SCALE).clamp(-1.0, 1.0))
}

/// Decodes one input report into the state.
fn apply_report(s: &mut State, report: &[u8]) {
    match report {
        [1, rest @ ..] if rest.len() >= 6 => {
            s.motion.translate = axes(rest);
            if rest.len() >= 12 {
                s.motion.rotate = axes(&rest[6..]);
            }
        }
        [2, rest @ ..] if rest.len() >= 6 => s.motion.rotate = axes(rest),
        [3, rest @ ..] => {
            let buttons = rest.iter().take(4).enumerate().fold(0u32, |b, (i, byte)| b | (*byte as u32) << (8 * i));
            s.pressed |= buttons & !s.buttons;
            s.buttons = buttons;
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(id: u8, v: &[i16]) -> Vec<u8> {
        std::iter::once(id).chain(v.iter().flat_map(|x| x.to_le_bytes())).collect()
    }

    #[test]
    fn decodes_split_and_combined_reports() {
        let mut s = State::default();
        apply_report(&mut s, &report(1, &[350, -175, 0]));
        apply_report(&mut s, &report(2, &[0, 0, 700]));
        assert_eq!(s.motion, Motion { translate: [1.0, -0.5, 0.0], rotate: [0.0, 0.0, 1.0] });
        apply_report(&mut s, &report(1, &[0, 0, 35, -35, 0, 0]));
        assert_eq!(s.motion, Motion { translate: [0.0, 0.0, 0.1], rotate: [-0.1, 0.0, 0.0] });
        apply_report(&mut s, &report(1, &[0, 0, 0, 0, 0, 0]));
        assert!(s.motion.is_still());
    }

    /// Reports the 3D mouse plugged into this machine, if any.
    #[test]
    #[ignore]
    fn probe_device() {
        match SpaceMouse::connect(|| {}) {
            Ok(d) => println!("3D mouse: {}", d.name()),
            Err(e) => println!("{e}"),
        }
    }

    #[test]
    fn counts_presses_between_reads() {
        let mut s = State::default();
        apply_report(&mut s, &[3, 0b01, 0]);
        apply_report(&mut s, &[3, 0b00, 0]);
        apply_report(&mut s, &[3, 0b10, 0]);
        assert_eq!(s.pressed, 0b11);
        assert_eq!(s.buttons, 0b10);
        // Short or unknown reports change nothing.
        apply_report(&mut s, &[1, 5]);
        apply_report(&mut s, &[9, 1, 2, 3]);
        assert!(s.motion.is_still());
    }
}
