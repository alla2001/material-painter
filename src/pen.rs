//! Pen side buttons on Windows. winit reports a pen as touches and drops its buttons, so
//! the barrel button never reaches egui; this reads it from the pen's own messages
//! (a hook on the event loop, before winit handles them). The views treat a held barrel
//! button like the middle mouse button: dragging pans.
//!
//! `MP_PEN_LOG=1` writes what the pen and the middle mouse button send to
//! `material-painter-pen.log` in the temp folder, for tablet drivers that act up.

use std::sync::atomic::{AtomicBool, Ordering};

static BARREL: AtomicBool = AtomicBool::new(false);

/// The pen's barrel (side) button is held.
pub fn barrel_down() -> bool {
    BARREL.load(Ordering::Relaxed)
}

/// Watches the pen's messages on the app's event loop (Windows only).
pub fn install(options: &mut eframe::NativeOptions) {
    #[cfg(windows)]
    {
        options.event_loop_builder = Some(Box::new(|builder| {
            use winit::platform::windows::EventLoopBuilderExtWindows;
            let mut log = std::env::var_os("MP_PEN_LOG").is_some().then(win::Log::open).flatten();
            builder.with_msg_hook(move |msg| {
                // SAFETY: winit hands the hook the `MSG` it just took off the queue.
                let msg = unsafe { &*(msg as *const windows_sys::Win32::UI::WindowsAndMessaging::MSG) };
                win::message(msg, log.as_mut());
                false
            });
        }));
    }
    #[cfg(not(windows))]
    let _ = options;
}

#[cfg(windows)]
mod win {
    use super::BARREL;
    use std::io::Write;
    use std::sync::atomic::Ordering;
    use windows_sys::Win32::UI::Input::Pointer::{GetPointerPenInfo, GetPointerType, POINTER_PEN_INFO};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MSG, PEN_FLAG_BARREL, PT_PEN, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_POINTERDOWN, WM_POINTERUP, WM_POINTERUPDATE,
    };

    pub struct Log {
        file: std::fs::File,
        last: Option<(u32, u32)>,
    }

    impl Log {
        pub fn open() -> Option<Log> {
            let path = std::env::temp_dir().join("material-painter-pen.log");
            std::fs::File::create(path).ok().map(|file| Log { file, last: None })
        }
    }

    pub fn message(msg: &MSG, log: Option<&mut Log>) {
        match msg.message {
            WM_POINTERDOWN | WM_POINTERUPDATE | WM_POINTERUP => {
                let id = (msg.wParam & 0xffff) as u32;
                let mut kind = 0;
                // SAFETY: plain queries about the pointer this message is for.
                if unsafe { GetPointerType(id, &mut kind) } == 0 || kind != PT_PEN {
                    return;
                }
                let mut info: POINTER_PEN_INFO = unsafe { std::mem::zeroed() };
                if unsafe { GetPointerPenInfo(id, &mut info) } == 0 {
                    return;
                }
                BARREL.store(info.penFlags & PEN_FLAG_BARREL != 0, Ordering::Relaxed);
                if let Some(log) = log {
                    // Only changes: hovering sends a stream of identical updates.
                    let now = (info.penFlags, info.pointerInfo.pointerFlags);
                    if log.last != Some(now) {
                        log.last = Some(now);
                        let _ = writeln!(log.file, "pen: penFlags {:#x}, pointerFlags {:#x}", now.0, now.1);
                    }
                }
            }
            WM_MBUTTONDOWN | WM_MBUTTONUP => {
                if let Some(log) = log {
                    let what = if msg.message == WM_MBUTTONDOWN { "down" } else { "up" };
                    let _ = writeln!(log.file, "middle mouse button {what}");
                }
            }
            _ => {}
        }
    }
}
