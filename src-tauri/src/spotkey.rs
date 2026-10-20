//! ULTRON's own shortcuts, registered with Windows as ordinary hot keys: Ctrl+Space opens ULTRON's Start (its
//! search), and the wheel can have one of your choosing (Settings, Taskbar). While one is on, no other app gets those
//! keys (Ctrl+Space: VS Code's suggestions, Excel's column select); switched off, they're given back at once.

use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
use std::sync::Mutex;

use tauri::{AppHandle, Emitter};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_CONTROL, MOD_NOREPEAT, VK_SPACE};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, PeekMessageW, PostThreadMessageW, MSG, PM_NOREMOVE, WM_APP, WM_HOTKEY, WM_USER};

/// Ctrl+Space: ULTRON's Start
pub const SPOT: i32 = 0x5350;
/// the wheel's own shortcut
pub const WHEEL: i32 = 0x5351;
/// voice typing's shortcut
pub const DICTATE: i32 = 0x5352;
const SWITCH: u32 = WM_APP + 0x53;
static THREAD: AtomicU32 = AtomicU32::new(0);
/// what each shortcut should be: (id, modifiers, key); a key of 0 is "none"
static WANT: Mutex<Vec<(i32, u32, u32)>> = Mutex::new(Vec::new());

fn event(id: i32) -> &'static str {
    if id == WHEEL { "wheel-key" } else if id == DICTATE { "dictate-key" } else { "spot-key" }
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || unsafe {
        let mut m = MSG::default();
        // (the thread's message queue has to exist before the page can post to it)
        let _ = PeekMessageW(&mut m, None, WM_USER, WM_USER, PM_NOREMOVE);
        THREAD.store(GetCurrentThreadId(), SeqCst);
        let mut held: Vec<(i32, u32, u32)> = Vec::new();
        let apply = |held: &mut Vec<(i32, u32, u32)>| {
            let want = WANT.lock().unwrap().clone();
            for (id, mods, vk) in want {
                let now = held.iter().find(|h| h.0 == id).copied();
                if now.map(|h| (h.1, h.2)) == Some((mods, vk)) || (now.is_none() && vk == 0) {
                    continue;
                }
                if now.is_some() {
                    let _ = UnregisterHotKey(None, id);
                    held.retain(|h| h.0 != id);
                }
                if vk == 0 {
                    crate::log_line(&format!("shortcut {id:#x}: given back"));
                    continue;
                }
                let ok = RegisterHotKey(None, id, HOT_KEY_MODIFIERS(mods) | MOD_NOREPEAT, vk).is_ok();
                if ok {
                    held.push((id, mods, vk));
                }
                crate::log_line(&format!("shortcut {id:#x}: {}", if ok { "registered" } else { "taken by another app" }));
                let _ = app.emit_to("main", "hotkey-state", serde_json::json!({ "id": id, "ok": ok }));
                if id == SPOT {
                    let _ = app.emit_to("main", "spot-key-state", ok);
                }
            }
        };
        apply(&mut held);
        while GetMessageW(&mut m, None, 0, 0).as_bool() {
            if m.message == WM_HOTKEY {
                let _ = app.emit_to("main", event(m.wParam.0 as i32), ());
            } else if m.message == SWITCH {
                apply(&mut held);
            }
        }
    });
}

/// Set one shortcut: `vk` 0 takes it away.
pub fn set_key(id: i32, mods: u32, vk: u32) {
    {
        let mut w = WANT.lock().unwrap();
        w.retain(|x| x.0 != id);
        w.push((id, mods, vk));
    }
    let t = THREAD.load(SeqCst);
    if t != 0 {
        unsafe {
            let _ = PostThreadMessageW(t, SWITCH, WPARAM(0), LPARAM(0));
        }
    }
}

/// Settings: Ctrl+Space on or off.
pub fn set(on: bool) {
    set_key(SPOT, MOD_CONTROL.0, if on { VK_SPACE.0 as u32 } else { 0 });
}
