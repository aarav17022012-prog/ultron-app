//! The Windows key, for ULTRON's own Start (Settings, Taskbar, Start menu: ULTRON).
//!
//! While that's chosen, pressing and letting go of the Windows key on its own opens ULTRON's search
//! instead of Windows' Start menu, and every Windows-key shortcut (Win+D, Win+E, Win+Shift+S ...)
//! still does what it always did. This is how:
//!
//! The Windows key going down is held back: Windows isn't told. If another key is pressed while it
//! is held, that's a shortcut, so Windows is told at that moment that the Windows key is down (just
//! ahead of the other key) and everything from there on is Windows' own business. If instead the
//! Windows key comes up with nothing else pressed, that is held back too, Windows never knew the key
//! was touched, and the page is told to open ULTRON's search.
//!
//! (What was tried first: letting the key through and tapping an unassigned key beside it, which is
//! how the Start menu is usually kept shut. On the build this was made on that worked for a key sent
//! by a program and not for one pressed on the keyboard: Start came up anyway.)
//!
//! Only whether a key is the Windows key, and whether it went down or up, is ever looked at.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use tauri::{AppHandle, Emitter, Manager};
use windows::Win32::Foundation::{CloseHandle, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_ESCAPE, VK_LWIN, VK_RWIN};
use windows::Win32::UI::WindowsAndMessaging::{CallNextHookEx, DispatchMessageW, GetClassNameW, GetForegroundWindow, GetMessageW, GetWindowThreadProcessId, SetWindowsHookExW, TranslateMessage, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN};

static ON: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
static DOWN: AtomicBool = AtomicBool::new(false); // the Windows key is being held (as far as the keyboard goes)
static SENT: AtomicBool = AtomicBool::new(false); // ...and Windows has been told so, because another key joined it
static WHICH: AtomicU32 = AtomicU32::new(0); // which Windows key (left or right)
static LAST: AtomicU32 = AtomicU32::new(0); // when it last went down (or repeated), on the keyboard's own clock
static TRYING: AtomicBool = AtomicBool::new(false); // (developer mode: keys a test sends count as pressed)
static TELL: OnceLock<Mutex<Sender<u8>>> = OnceLock::new();
const ALONE_UP: u8 = 1; // the Windows key was pressed and let go by itself
const LOCK: u8 = 6; // Win+L
const OURS: usize = 0x554C_5452; // marks the keys ULTRON sends itself, so that it knows them again

fn tell(what: u8) {
    if let Some(t) = TELL.get() {
        let _ = t.lock().map(|t| t.send(what));
    }
}

/// Send a key going down, or coming up, as ULTRON's own.
fn send(vk: u16, up: bool) -> bool {
    let mut flags = if up { KEYEVENTF_KEYUP } else { Default::default() };
    if vk == VK_LWIN.0 || vk == VK_RWIN.0 {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    let key = INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), dwFlags: flags, dwExtraInfo: OURS, ..Default::default() } } };
    unsafe { SendInput(&[key], std::mem::size_of::<INPUT>() as i32) == 1 }
}

/// Is Windows' own Start (or its search, which Start is part of on newer builds) the window in front?
fn start_is_up() -> bool {
    unsafe {
        let h = GetForegroundWindow();
        if h.is_invalid() {
            return false;
        }
        let mut class = [0u16; 64];
        let n = GetClassNameW(h, &mut class) as usize;
        if String::from_utf16_lossy(&class[..n.min(64)]) != "Windows.UI.Core.CoreWindow" {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        let Ok(p) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return false };
        let mut path = [0u16; 520];
        let mut len = path.len() as u32;
        let got = QueryFullProcessImageNameW(p, PROCESS_NAME_FORMAT(0), windows::core::PWSTR(path.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(p);
        if !got {
            return false;
        }
        let exe = String::from_utf16_lossy(&path[..len as usize]).to_lowercase();
        ["\\searchhost.exe", "\\startmenuexperiencehost.exe", "\\searchapp.exe"].iter().any(|e| exe.ends_with(e))
    }
}

unsafe extern "system" fn hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 && ON.load(SeqCst) {
            let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
            // (keys that programs send are let by untouched, ULTRON's own among them)
            if k.dwExtraInfo != OURS && (k.flags.0 & LLKHF_INJECTED.0 == 0 || TRYING.load(SeqCst)) {
                let win = k.vkCode == VK_LWIN.0 as u32 || k.vkCode == VK_RWIN.0 as u32;
                let down = wp.0 as u32 == WM_KEYDOWN || wp.0 as u32 == WM_SYSKEYDOWN;
                if win && down {
                    // (a press is new if the key wasn't held, or if "held" is stale: the key coming up was missed, as it
                    // is when the PC locks with it down. A held key repeats itself within a second.)
                    let stale = k.time.wrapping_sub(LAST.swap(k.time, SeqCst)) > 1500;
                    if !DOWN.swap(true, SeqCst) || stale {
                        if SENT.swap(false, SeqCst) {
                            send(WHICH.load(SeqCst) as u16, true);
                        }
                        WHICH.store(k.vkCode, SeqCst);
                    }
                    if !SENT.load(SeqCst) {
                        return LRESULT(1); // (held back, and so is its repeating while it's held)
                    }
                } else if win {
                    if DOWN.swap(false, SeqCst) && !SENT.swap(false, SeqCst) {
                        tell(ALONE_UP);
                        return LRESULT(1); // (Windows never heard it go down; it doesn't hear it come up)
                    }
                    // (otherwise it was part of a shortcut: Windows was told it went down, and now hears it come up)
                } else if down && DOWN.load(SeqCst) && !SENT.swap(true, SeqCst) {
                    // Another key, with the Windows key held: a shortcut. Windows is told now that the Windows key
                    // is down (a key sent from in here goes in ahead of the one being looked at), and the rest is its own.
                    send(WHICH.load(SeqCst) as u16, false);
                    if k.vkCode == b'L' as u32 {
                        tell(LOCK);
                    }
                }
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

/// Start (or stop) answering the Windows key. The listening itself is set up once and then only switched.
pub fn set(app: &AppHandle, on: bool) {
    ON.store(on, SeqCst);
    // (switched with the key held and Windows told of it: it is let go, so that it can't be left "down")
    if DOWN.swap(false, SeqCst) && SENT.swap(false, SeqCst) {
        send(WHICH.load(SeqCst) as u16, true);
    }
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
            if what == LOCK {
                // (Win+L. Windows locks on that itself when the Windows key is one it saw pressed; this makes sure
                // of it, and lets the key go on Windows' side, since its coming up won't be seen from the lock screen)
                std::thread::sleep(std::time::Duration::from_millis(90));
                if DOWN.swap(false, SeqCst) && SENT.swap(false, SeqCst) {
                    send(WHICH.load(SeqCst) as u16, true);
                }
                unsafe {
                    let _ = windows::Win32::System::Shutdown::LockWorkStation();
                }
                continue;
            }
            if what != ALONE_UP {
                continue;
            }
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.set_focus(); // (so that what's typed next goes to the search box)
            }
            let _ = app.emit_to("main", "win-key", ());
            // Windows' own Start can't have heard the key. All the same, if it is up in the next half second it
            // is sent away again (Escape, to it) and the keyboard is taken back, and a line is written, since that
            // would mean something here has stopped working.
            for _ in 0..4 {
                std::thread::sleep(std::time::Duration::from_millis(120));
                if start_is_up() {
                    send(VK_ESCAPE.0, false);
                    send(VK_ESCAPE.0, true);
                    crate::log_line("start: Windows' own Start came up as well; it was closed");
                    std::thread::sleep(std::time::Duration::from_millis(160));
                    if let Some(w) = app.get_webview_window("main") {
                        let _ = w.set_focus();
                    }
                    break;
                }
            }
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
