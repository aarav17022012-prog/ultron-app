//! ULTRON's own Alt+Tab (Settings > Look & feel): while Alt is held, Tab goes to ULTRON instead of Windows, and the
//! page shows the open windows as glass cards; letting go of Alt goes to the one chosen. A hook of its own, on a
//! thread made only once this is first turned on. A game or anything else full screen keeps Windows' own, as does
//! every Alt+Tab while this is off.

use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};

use tauri::{AppHandle, Emitter};
use windows::Win32::Foundation::{LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW, GetWindowRect, SetWindowsHookExW, TranslateMessage, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG,
    WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

static ON: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
/// the switcher is up (Alt still held)
static UP: AtomicBool = AtomicBool::new(false);
static TELL: OnceLock<Mutex<Sender<&'static str>>> = OnceLock::new();

fn tell(what: &'static str) {
    if let Some(t) = TELL.get() {
        let _ = t.lock().unwrap().send(what);
    }
}

fn held(vk: i32) -> bool {
    unsafe { GetAsyncKeyState(vk) as u16 & 0x8000 != 0 }
}

/// The window in front fills its whole screen (a game, a video, a slideshow).
fn full() -> bool {
    unsafe {
        let h = GetForegroundWindow();
        let mut r = RECT::default();
        if h.is_invalid() || GetWindowRect(h, &mut r).is_err() {
            return false;
        }
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST), &mut mi).as_bool() {
            return false;
        }
        let m = mi.rcMonitor;
        let class = {
            let mut buf = [0u16; 32];
            let n = windows::Win32::UI::WindowsAndMessaging::GetClassNameW(h, &mut buf).max(0) as usize;
            String::from_utf16_lossy(&buf[..n])
        };
        class != "Progman" && class != "WorkerW" && r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
    }
}

/// A key no program uses, pressed while Alt is held: Alt coming up after it doesn't open the app's menu bar.
fn mask() {
    let k = |up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0xE8), dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() }, ..Default::default() } },
    };
    unsafe {
        SendInput(&[k(false), k(true)], std::mem::size_of::<INPUT>() as i32);
    }
}

unsafe extern "system" fn keyboard(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 && ON.load(SeqCst) {
            let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
            if k.flags.0 & LLKHF_INJECTED.0 == 0 {
                let m = wp.0 as u32;
                let down = m == WM_KEYDOWN || m == WM_SYSKEYDOWN;
                let alt_key = matches!(k.vkCode, 0x12 | 0xA4 | 0xA5);
                if UP.load(SeqCst) {
                    match k.vkCode {
                        0x09 if down => tell(if held(0x10) { "prev" } else { "next" }),
                        0x25 | 0x26 if down => tell("prev"),
                        0x27 | 0x28 if down => tell("next"),
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
                if k.vkCode == 0x09 && down && held(0x12) && !held(0x11) && !held(0x5B) && !held(0x5C) && !full() {
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
    let (tx, rx) = channel::<&'static str>();
    let _ = TELL.set(Mutex::new(tx));
    let app = app.clone();
    std::thread::spawn(move || {
        for what in rx {
            let _ = app.emit_to("main", "alttab", what);
        }
    });
    std::thread::spawn(|| unsafe {
        let _hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), None, 0);
        let mut m = MSG::default();
        while GetMessageW(&mut m, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&m);
            DispatchMessageW(&m);
        }
    });
}
