//! SYLVER's own volume indicator (Settings > Look & feel): the volume keys are answered here instead of by Windows,
//! so Windows' own box never comes up, and the page shows the change under the notch. A hook of its own (as sounds.rs
//! has), on a thread made only once this is first turned on; while it's off every key goes by untouched. Brightness
//! keys belong to the laptop rather than the keyboard, so those can't be taken: the brightness is shown as it changes
//! (lid.rs hears it), beside whatever the laptop shows.

use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};

use tauri::{AppHandle, Emitter};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

static ON: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
static TELL: OnceLock<Mutex<Sender<u8>>> = OnceLock::new();

/// (VK_VOLUME_MUTE, VK_VOLUME_DOWN, VK_VOLUME_UP)
const MUTE: u32 = 0xAD;
const UP: u32 = 0xAF;
const BRIGHT_DOWN: u32 = 0xD6;
const BRIGHT_UP: u32 = 0xD7;

#[derive(serde::Serialize, Clone)]
struct Level {
    kind: &'static str,
    level: f32,
    muted: bool,
}

unsafe extern "system" fn keyboard(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 && ON.load(SeqCst) {
            let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
            let volume_key = (MUTE..=UP).contains(&k.vkCode);
            let brightness_key = matches!(k.vkCode, BRIGHT_DOWN | BRIGHT_UP);
            if (volume_key || brightness_key) && k.flags.0 & LLKHF_INJECTED.0 == 0 {
                let m = wp.0 as u32;
                if m == WM_KEYDOWN || m == WM_SYSKEYDOWN {
                    if let Some(t) = TELL.get() {
                        let what = match k.vkCode {
                            MUTE => 0,
                            0xAE => 1,
                            UP => 2,
                            BRIGHT_DOWN => 3,
                            _ => 4,
                        };
                        let _ = t.lock().unwrap().send(what);
                    }
                }
                return LRESULT(1); // (Windows never hears it, so it shows nothing of its own)
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

/// Turned on or off in Settings.
pub fn set(app: &AppHandle, on: bool) {
    ON.store(on, SeqCst);
    if !on || STARTED.swap(true, SeqCst) {
        return;
    }
    let (tx, rx) = channel::<u8>();
    let _ = TELL.set(Mutex::new(tx));
    let app = app.clone();
    // (the change is made, and the page told, away from the hook: it has to answer Windows at once, every time)
    std::thread::spawn(move || {
        let mut pending = None;
        loop {
            let Some(what) = pending.take().or_else(|| rx.recv().ok()) else { break };
            if what >= 3 {
                let mut delta = if what == 3 { -5 } else { 5 };
                while let Ok(next) = rx.try_recv() {
                    if next >= 3 {
                        delta += if next == 3 { -5 } else { 5 };
                    } else {
                        pending = Some(next);
                        break;
                    }
                }
                if delta != 0 {
                    if let Some(level) = crate::skills::step_brightness(delta) {
                        let _ = app.emit_to("main", "osd", Level { kind: "bright", level: level as f32 / 100.0, muted: false });
                    }
                }
                continue;
            }
            let now = crate::commands::volume().unwrap_or(0.0);
            match what {
                0 => { crate::commands::set_mute(!crate::commands::muted().unwrap_or(false)); }
                1 => { crate::commands::set_volume(((now * 50.0).round() - 1.0).max(0.0) / 50.0); }
                _ => { crate::commands::set_volume(((now * 50.0).round() + 1.0).min(50.0) / 50.0); }
            }
            let level = crate::commands::volume().unwrap_or(now);
            let _ = app.emit_to("main", "osd", Level { kind: "vol", level, muted: crate::commands::muted().unwrap_or(false) });
        }
    });
    crate::hooks::keep(Some(keyboard), None);
}
