//! Ctrl+Space opens ULTRON's Start (its search) from anywhere. It's an ordinary hot key registered with Windows, so
//! while it's on no other app gets Ctrl+Space (VS Code's suggestions, Excel's "select column"); switched off in
//! Settings, it's given back at once.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};

use tauri::{AppHandle, Emitter};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, MOD_CONTROL, MOD_NOREPEAT, VK_SPACE};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, PeekMessageW, PostThreadMessageW, MSG, PM_NOREMOVE, WM_APP, WM_HOTKEY, WM_USER};

const ID: i32 = 0x5350;
const SWITCH: u32 = WM_APP + 0x53;
static THREAD: AtomicU32 = AtomicU32::new(0);
static WANT: AtomicBool = AtomicBool::new(false);

pub fn start(app: AppHandle) {
    std::thread::spawn(move || unsafe {
        let mut m = MSG::default();
        // (the thread's message queue has to exist before the page can post to it)
        let _ = PeekMessageW(&mut m, None, WM_USER, WM_USER, PM_NOREMOVE);
        THREAD.store(GetCurrentThreadId(), SeqCst);
        let mut held = false;
        let apply = |want: bool, held: &mut bool| {
            if want == *held {
                return;
            }
            if want {
                *held = RegisterHotKey(None, ID, MOD_CONTROL | MOD_NOREPEAT, VK_SPACE.0 as u32).is_ok();
                crate::log_line(if *held { "start key: Ctrl+Space opens ULTRON's Start" } else { "start key: Ctrl+Space is taken by another app" });
                let _ = app.emit_to("main", "spot-key-state", *held);
            } else {
                let _ = UnregisterHotKey(None, ID);
                *held = false;
                crate::log_line("start key: Ctrl+Space given back");
            }
        };
        apply(WANT.load(SeqCst), &mut held);
        while GetMessageW(&mut m, None, 0, 0).as_bool() {
            if m.message == WM_HOTKEY && m.wParam.0 == ID as usize {
                let _ = app.emit_to("main", "spot-key", ());
            } else if m.message == SWITCH {
                apply(m.wParam.0 != 0, &mut held);
            }
        }
    });
}

/// Settings: Ctrl+Space on or off.
pub fn set(on: bool) {
    WANT.store(on, SeqCst);
    let t = THREAD.load(SeqCst);
    if t != 0 {
        unsafe {
            let _ = PostThreadMessageW(t, SWITCH, WPARAM(on as usize), LPARAM(0));
        }
    }
}
