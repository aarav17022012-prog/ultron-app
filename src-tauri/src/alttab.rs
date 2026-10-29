//! ULTRON's own Alt+Tab (Settings > Look & feel): while Alt is held, Tab goes to ULTRON instead of Windows, and the
//! page shows the open windows as glass cards; letting go of Alt goes to the one chosen. Holding Alt and scrolling
//! (the mouse wheel, or two fingers on the touchpad) runs through them the same way, and Ctrl+Alt+Left/Right hops
//! straight to the next or last window (a four-finger swipe can be set to send those, in Windows' touchpad settings).
//! Hooks of its own, on a thread made only once this is first turned on. A game or anything else full screen keeps
//! Windows' own, as does every Alt+Tab while this is off.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering::SeqCst};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};

use tauri::{AppHandle, Emitter};
use windows::Win32::Foundation::{LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW, GetWindowLongPtrW, GetWindowRect, SetWindowsHookExW, TranslateMessage, GWL_STYLE,
    KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_MOUSEWHEEL, WM_SYSKEYDOWN,
    WM_SYSKEYUP, WS_CAPTION,
};

static ON: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
/// the switcher is up (Alt still held)
static UP: AtomicBool = AtomicBool::new(false);
static TELL: OnceLock<Mutex<Sender<&'static str>>> = OnceLock::new();
/// (developer mode: keys a test sends count as pressed; ULTRON's own never do)
static TRYING: AtomicBool = AtomicBool::new(false);
const OURS: usize = 0x0A17_7AB;
/// Alt held, as this hook has seen it (Windows' own note of it can lag a key behind)
static ALT: AtomicBool = AtomicBool::new(false);
/// a touchpad's scrolling comes in small steps: they're added up to a whole notch before the choice moves
static WHEEL: AtomicI32 = AtomicI32::new(0);

fn tell(what: &'static str) {
    if let Some(t) = TELL.get() {
        let _ = t.lock().unwrap().send(what);
    }
}

fn held(vk: i32) -> bool {
    unsafe { GetAsyncKeyState(vk) as u16 & 0x8000 != 0 }
}

/// A game or anything else is full screen (as Windows says, the same as ULTRON's bar stands aside for): Windows' own
/// switcher is left to it. (Measuring the window in front took some maximized apps with no title bar for full screen
/// when ULTRON's bar was set to hide away, and Windows' Alt+Tab came up over them.)
fn full() -> bool {
    // (and still so now: a notice that it ended can go astray, and Windows' switcher then came up from then on. And a
    //  maximized window isn't full screen, even one with no title bar filling the screen when ULTRON's bar hides away:
    //  Windows calls those full screen too, and its switcher came up over VS Code, Discord, a browser)
    crate::appbar::FULL.load(SeqCst) && crate::appbar::really_full() && !unsafe { windows::Win32::UI::WindowsAndMessaging::IsZoomed(GetForegroundWindow()) }.as_bool()
}

/// A key no program uses, pressed while Alt is held: Alt coming up after it doesn't open the app's menu bar.
fn mask() {
    let k = |up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0xE8), dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() }, dwExtraInfo: OURS, ..Default::default() } },
    };
    unsafe {
        SendInput(&[k(false), k(true)], std::mem::size_of::<INPUT>() as i32);
    }
}

unsafe extern "system" fn keyboard(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 && ON.load(SeqCst) {
            let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
            // (keys sent by a program count too: a four-finger swipe set, in Windows' touchpad settings, to a shortcut
            //  arrives as keys Windows sends; only ULTRON's own are let by)
            if k.dwExtraInfo != OURS {
                let m = wp.0 as u32;
                let down = m == WM_KEYDOWN || m == WM_SYSKEYDOWN;
                let alt_key = matches!(k.vkCode, 0x12 | 0xA4 | 0xA5);
                if alt_key {
                    ALT.store(down, SeqCst);
                }
                let alt = ALT.load(SeqCst) || held(0x12);
                // (Alt was let go and that was missed: the cards go, and nothing is held back any longer)
                if UP.load(SeqCst) && !alt && !alt_key {
                    UP.store(false, SeqCst);
                    tell("pick");
                    return CallNextHookEx(None, code, wp, lp);
                }
                if UP.load(SeqCst) {
                    match k.vkCode {
                        0x09 if down => tell(if held(0x10) { "prev" } else { "next" }),
                        0x25 | 0x26 if down => tell("prev"),
                        0x27 | 0x28 if down => tell("next"),
                        0x51 | 0x57 | 0x2E if down => tell("quit"),
                        0x1B if down => {
                            UP.store(false, SeqCst);
                            tell("cancel");
                        }
                        0x0D if down => {
                            UP.store(false, SeqCst);
                            tell("pick");
                        }
                        _ if alt_key && (m == WM_KEYUP || m == WM_SYSKEYUP) => {
                            UP.store(false, SeqCst);
                            tell("pick");
                            return CallNextHookEx(None, code, wp, lp); // (Windows hears Alt come up, as it heard it go down)
                        }
                        _ if alt_key => return CallNextHookEx(None, code, wp, lp),
                        _ => {}
                    }
                    return LRESULT(1);
                }
                // Ctrl+Alt+Left/Right: straight to the next window, or back to the last
                if (k.vkCode == 0x25 || k.vkCode == 0x27) && down && alt && held(0x11) && !held(0x5B) && !held(0x5C) && !full() {
                    mask();
                    tell(if k.vkCode == 0x27 { "hop-next" } else { "hop-prev" });
                    return LRESULT(1);
                }
                if k.vkCode == 0x09 && down && alt && !held(0x11) && !held(0x5B) && !held(0x5C) && full() {
                    tell("pass-full");
                }
                if k.vkCode == 0x09 && down && alt && !held(0x11) && !held(0x5B) && !held(0x5C) && !full() {
                    UP.store(true, SeqCst);
                    mask();
                    tell(if held(0x10) { "open-back" } else { "open" });
                    return LRESULT(1);
                }
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

unsafe extern "system" fn mouse(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 && ON.load(SeqCst) && wp.0 as u32 == WM_MOUSEWHEEL {
            let m = &*(lp.0 as *const MSLLHOOKSTRUCT);
            if (m.flags & LLMHF_INJECTED == 0 || TRYING.load(SeqCst)) && held(0x12) && !held(0x11) && (UP.load(SeqCst) || !full()) {
                if !UP.swap(true, SeqCst) {
                    WHEEL.store(0, SeqCst);
                    mask();
                    tell("open-wheel");
                }
                let d = (m.mouseData >> 16) as i16 as i32;
                let sum = WHEEL.fetch_add(d, SeqCst) + d;
                if sum.abs() >= 120 {
                    WHEEL.store(0, SeqCst);
                    tell(if sum < 0 { "next" } else { "prev" });
                }
                return LRESULT(1);
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

/// Turned on or off in Settings.
pub fn set(app: &AppHandle, on: bool) {
    ON.store(on, SeqCst);
    if !on {
        if UP.swap(false, SeqCst) {
            tell("cancel");
        }
        return;
    }
    if STARTED.swap(true, SeqCst) {
        return;
    }
    TRYING.store(std::env::var("ULTRON_DEV_INBOX").is_ok_and(|v| v == "1") || TRYING.load(SeqCst), SeqCst);
    let (tx, rx) = channel::<&'static str>();
    let _ = TELL.set(Mutex::new(tx));
    let app = app.clone();
    std::thread::spawn(move || {
        for what in rx {
            if what == "pass-full" {
                crate::log_line("alt+tab: left to Windows (something is full screen)");
                continue;
            }
            let _ = app.emit_to("main", "alttab", what);
        }
    });
    crate::hooks::keep(Some(keyboard), Some(mouse));
}
