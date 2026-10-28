//! Key and click sounds (Settings > Look & feel): a sound for each key you press and each click, as on a keyboard
//! with creamy switches. Two low-level hooks of their own (not keys.rs's, which holds the Windows key back) run on a
//! thread made only once the sounds are first turned on. All they pass on is what kind of press it was (a key,
//! space, enter, backspace, a click), never which key, and only presses made by hand: keys typed by programs,
//! ULTRON's own dictation among them, make no sound. The page plays the sound.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};

use tauri::{AppHandle, Emitter};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage, KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_RBUTTONDOWN, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

static KEYS: AtomicBool = AtomicBool::new(false);
static CLICKS: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
/// keys held down now (a held key repeats its "down"; only the first makes a sound)
static HELD: [AtomicU64; 4] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static TELL: OnceLock<Mutex<Sender<u8>>> = OnceLock::new();

/// (what kind of press: 0 a key, 1 space, 2 enter, 3 backspace, 4 a click, 5 a key let go, 6 a click let go)
fn tell(kind: u8) {
    if let Some(t) = TELL.get() {
        let _ = t.lock().unwrap().send(kind);
    }
}

fn held(vk: u32, on: Option<bool>) -> bool {
    let (i, bit) = ((vk as usize / 64) % 4, 1u64 << (vk % 64));
    let was = HELD[i].load(SeqCst) & bit != 0;
    match on {
        Some(true) => {
            HELD[i].fetch_or(bit, SeqCst);
        }
        Some(false) => {
            HELD[i].fetch_and(!bit, SeqCst);
        }
        None => {}
    }
    was
}

unsafe extern "system" fn keyboard(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 && KEYS.load(SeqCst) {
            let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
            if k.flags.0 & LLKHF_INJECTED.0 == 0 {
                let m = wp.0 as u32;
                if m == WM_KEYDOWN || m == WM_SYSKEYDOWN {
                    if !held(k.vkCode, Some(true)) {
                        tell(match k.vkCode {
                            0x20 => 1,
                            0x0D => 2,
                            0x08 => 3,
                            _ => 0,
                        });
                    }
                } else if m == WM_KEYUP || m == WM_SYSKEYUP {
                    held(k.vkCode, Some(false));
                    tell(5);
                }
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

unsafe extern "system" fn mouse(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 && CLICKS.load(SeqCst) {
            let m = &*(lp.0 as *const MSLLHOOKSTRUCT);
            if m.flags & LLMHF_INJECTED == 0 {
                match wp.0 as u32 {
                    WM_LBUTTONDOWN | WM_RBUTTONDOWN => tell(4),
                    WM_LBUTTONUP => tell(6),
                    _ => {}
                }
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

/// Key sounds and click sounds, on or off. The hooks are put in the first time either is turned on.
pub fn set(app: &AppHandle, keys: bool, clicks: bool) {
    KEYS.store(keys, SeqCst);
    CLICKS.store(clicks, SeqCst);
    if !(keys || clicks) || STARTED.swap(true, SeqCst) {
        return;
    }
    let (tx, rx) = channel::<u8>();
    let _ = TELL.set(Mutex::new(tx));
    let app2 = app.clone();
    // (the page told on its own thread: the hooks must answer Windows at once, or it drops them)
    std::thread::spawn(move || {
        for kind in rx {
            let _ = app2.emit_to("main", "tick", kind);
        }
    });
    std::thread::spawn(|| unsafe {
        let k = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), None, 0);
        let m = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse), None, 0);
        crate::log_line(&format!("sounds: listening for presses (keys {}, clicks {})", k.is_ok(), m.is_ok()));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
}
