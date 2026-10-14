//! The Windows key, for ULTRON's own Start (Settings, Taskbar, Start menu: ULTRON).
//!
//! While that's chosen, pressing and letting go of the Windows key on its own opens ULTRON's search
//! instead of Windows' Start menu. Every Windows-key shortcut (Win+D, Win+L, Win+Shift+S ...) is left
//! exactly as it is: the key itself is never held back. All that's done is this. When the Windows key
//! comes up without any other key having been pressed meanwhile, a key that means nothing to anything
//! (0xE8) is tapped just ahead of it; Windows then no longer counts that as "the Windows key by
//! itself", so Start stays shut, and the page is told. (The tap is also sent as the key goes down. On
//! the build this was made on, a key sent from inside the listener goes in ahead of the key being
//! looked at, so that first tap lands too early to count; it is kept for systems where it doesn't.)
//!
//! Only whether a key is the Windows key, and whether it went down or up, is ever looked at.

use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use tauri::{AppHandle, Emitter, Manager};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_LWIN, VK_RWIN};
use windows::Win32::UI::WindowsAndMessaging::{CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN};

static ON: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
static DOWN: AtomicBool = AtomicBool::new(false);
static ALONE: AtomicBool = AtomicBool::new(false);
static TRYING: AtomicBool = AtomicBool::new(false); // (developer mode: keys a test sends count as pressed)
static TELL: OnceLock<Mutex<Sender<u8>>> = OnceLock::new();
const ALONE_UP: u8 = 1; // the Windows key was pressed and let go by itself

fn tell(what: u8) {
    if let Some(t) = TELL.get() {
        let _ = t.lock().map(|t| t.send(what));
    }
}
const NOTHING: u16 = 0xE8;

/// Press and let go of the key that means nothing.
fn tap() -> bool {
    let key = |up: bool| INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(NOTHING), dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() }, ..Default::default() } } };
    unsafe { SendInput(&[key(false), key(true)], std::mem::size_of::<INPUT>() as i32) == 2 }
}

unsafe extern "system" fn hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 && ON.load(SeqCst) {
            let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
            // (keys that programs send are let by untouched: ULTRON's own tap is one of them)
            if k.flags.0 & LLKHF_INJECTED.0 == 0 || (TRYING.load(SeqCst) && k.vkCode != NOTHING as u32) {
                let win = k.vkCode == VK_LWIN.0 as u32 || k.vkCode == VK_RWIN.0 as u32;
                let down = wp.0 as u32 == WM_KEYDOWN || wp.0 as u32 == WM_SYSKEYDOWN;
                if win && down {
                    if !DOWN.swap(true, SeqCst) {
                        ALONE.store(true, SeqCst);
                        let sent = tap();
                        if TRYING.load(SeqCst) {
                            tell(if sent { 2 } else { 5 });
                        }
                    }
                } else if win {
                    DOWN.store(false, SeqCst);
                    if ALONE.swap(false, SeqCst) {
                        // (this is the tap that counts: a key sent from in here goes in ahead of the key being
                        // looked at, so it lands between the Windows key going down and coming up)
                        tap();
                        tell(ALONE_UP);
                    } else if TRYING.load(SeqCst) {
                        tell(3);
                    }
                } else if down && DOWN.load(SeqCst) && ALONE.swap(false, SeqCst) && TRYING.load(SeqCst) {
                    tell(4); // (Win with another key: a shortcut, and none of ULTRON's business)
                }
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

/// Start (or stop) answering the Windows key. The listening itself is set up once and then only switched.
pub fn set(app: &AppHandle, on: bool) {
    ON.store(on, SeqCst);
    DOWN.store(false, SeqCst);
    ALONE.store(false, SeqCst);
    if !on || STARTED.swap(true, SeqCst) {
        return;
    }
    TRYING.store(std::env::var("ULTRON_DEV_INBOX").is_ok_and(|v| v == "1"), SeqCst);
    let (tx, rx) = std::sync::mpsc::channel::<u8>();
    let _ = TELL.set(Mutex::new(tx));
    let app = app.clone();
    // (the page is told from a thread of its own: the listener has to answer Windows at once, every time)
    std::thread::spawn(move || {
        for what in rx {
            if what != ALONE_UP {
                // (developer mode only: what the listener saw, for working out a press that went astray)
                crate::log_line(match what {
                    2 => "start (trying): Windows key down, the tap was sent",
                    3 => "start (trying): Windows key up, after another key",
                    4 => "start (trying): another key while the Windows key was down",
                    _ => "start (trying): Windows key down, but the tap couldn't be sent",
                });
                continue;
            }
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.set_focus(); // (so that what's typed next goes to the search box)
            }
            let _ = app.emit_to("main", "win-key", ());
        }
    });
    std::thread::spawn(|| unsafe {
        if SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), None, 0).is_err() {
            crate::log_line("start: the Windows key couldn't be listened for");
            return;
        }
        crate::log_line("start: listening for the Windows key");
        let mut m = MSG::default();
        while GetMessageW(&mut m, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&m);
            DispatchMessageW(&m);
        }
    });
}
