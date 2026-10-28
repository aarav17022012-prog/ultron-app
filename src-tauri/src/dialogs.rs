//! Glass dialogs (Settings > Look & feel): an ordinary Windows message box ("Save changes?", "Are you sure?") is shown
//! as ULTRON's own glass one instead. The real box stays where it was, and stays the one that counts: it's only made
//! see-through (and so click-through, while it keeps the keyboard: Enter and Esc still answer it), and a button pressed
//! on ULTRON's presses the same button on it. Only plain boxes are taken over, words and push buttons and nothing else;
//! one with a tick box, a link or a list in it, one of ULTRON's own, or one that ULTRON isn't allowed to change (an app
//! run as administrator) is left as Windows draws it. If the page doesn't show its own within a moment and a half, or
//! the switch is turned off, or ULTRON goes, the real box comes back.

use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    GetDlgCtrlID, GetWindowLongPtrW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsWindow, IsWindowVisible, PostMessageW, SetLayeredWindowAttributes,
    SetWindowLongPtrW, GWL_EXSTYLE, GWL_STYLE, LWA_ALPHA, WM_COMMAND, WS_CHILD, WS_EX_LAYERED,
};

static ON: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
static APP: OnceLock<AppHandle> = OnceLock::new();

/// A box made see-through: whether it was layered before (so it's left as it was), when, and whether the page has
/// shown its own yet.
struct Held {
    hwnd: isize,
    layered: bool,
    at: Instant,
    shown: bool,
}
static HELD: Mutex<Vec<Held>> = Mutex::new(Vec::new());

#[derive(serde::Serialize, Clone)]
struct Button {
    id: i32,
    label: String,
    def: bool,
}

#[derive(serde::Serialize, Clone)]
struct Dialog {
    hwnd: isize,
    title: String,
    text: String,
    exe: String,
    buttons: Vec<Button>,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

fn class(h: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = unsafe { windows::Win32::UI::WindowsAndMessaging::GetClassNameW(h, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n])
}

fn text(h: HWND) -> String {
    let mut buf = vec![0u16; 2048];
    let n = unsafe { GetWindowTextW(h, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n])
}

fn exe(pid: u32) -> String {
    use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return String::new() };
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_FORMAT(0), windows::core::PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(h);
        if ok { String::from_utf16_lossy(&buf[..len as usize]) } else { String::new() }
    }
}

/// What a plain box holds: its words and its buttons. None when there's anything else in it.
struct Kids {
    words: Vec<String>,
    buttons: Vec<Button>,
    other: bool,
}

unsafe extern "system" fn each(h: HWND, l: LPARAM) -> windows::core::BOOL {
    let k = unsafe { &mut *(l.0 as *mut Kids) };
    if !unsafe { IsWindowVisible(h) }.as_bool() {
        return true.into();
    }
    let style = unsafe { GetWindowLongPtrW(h, GWL_STYLE) } as u32;
    match class(h).as_str() {
        // (a picture: the box's warning or question sign)
        "Static" if style & 0x1f == 0x3 => {}
        "Static" => {
            let t = text(h);
            if !t.trim().is_empty() {
                k.words.push(t.trim().to_string());
            }
        }
        // (push buttons only: BS_PUSHBUTTON, BS_DEFPUSHBUTTON)
        "Button" if style & 0xf <= 1 => {
            let label: String = text(h).replace("&&", "\u{1}").replace('&', "").replace('\u{1}', "&");
            k.buttons.push(Button { id: unsafe { GetDlgCtrlID(h) }, label: label.trim().to_string(), def: style & 0xf == 1 });
        }
        _ => k.other = true,
    }
    true.into()
}

fn read(h: HWND) -> Option<Dialog> {
    unsafe {
        if class(h) != "#32770" || GetWindowLongPtrW(h, GWL_STYLE) as u32 & WS_CHILD.0 != 0 {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if pid == 0 || pid == std::process::id() {
            return None;
        }
        let mut k = Kids { words: Vec::new(), buttons: Vec::new(), other: false };
        let _ = windows::Win32::UI::WindowsAndMessaging::EnumChildWindows(Some(h), Some(each), LPARAM(&mut k as *mut _ as isize));
        if k.other || k.buttons.is_empty() || k.buttons.len() > 4 || k.words.is_empty() || k.buttons.iter().any(|b| b.label.is_empty()) {
            return None;
        }
        let mut r = RECT::default();
        let _ = GetWindowRect(h, &mut r);
        Some(Dialog {
            hwnd: h.0 as isize,
            title: text(h),
            text: k.words.join("\n\n"),
            exe: exe(pid),
            buttons: k.buttons,
            x: r.left,
            y: r.top,
            w: r.right - r.left,
            h: r.bottom - r.top,
        })
    }
}

/// See-through (or back as it was). False when Windows won't let ULTRON change it.
fn veil(h: HWND, on: bool, layered: bool) -> bool {
    unsafe {
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        if on {
            if ex & WS_EX_LAYERED.0 as isize == 0 {
                windows::Win32::Foundation::SetLastError(windows::Win32::Foundation::WIN32_ERROR(0));
                if SetWindowLongPtrW(h, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as isize) == 0 && windows::Win32::Foundation::GetLastError().is_err() {
                    return false;
                }
            }
            SetLayeredWindowAttributes(h, COLORREF(0), 0, LWA_ALPHA).is_ok()
        } else {
            let ok = SetLayeredWindowAttributes(h, COLORREF(0), 255, LWA_ALPHA).is_ok();
            if !layered {
                SetWindowLongPtrW(h, GWL_EXSTYLE, ex & !(WS_EX_LAYERED.0 as isize));
            }
            ok
        }
    }
}

fn gone(hwnd: isize) {
    if let Some(app) = APP.get() {
        let _ = app.emit_to("main", "dialog-gone", hwnd);
    }
}

/// Puts every held box back as Windows draws it.
fn release_all() {
    let all: Vec<Held> = std::mem::take(&mut *HELD.lock().unwrap());
    for d in all {
        let h = HWND(d.hwnd as _);
        if unsafe { IsWindow(Some(h)) }.as_bool() {
            veil(h, false, d.layered);
        }
        gone(d.hwnd);
    }
}

unsafe extern "system" fn started(_: windows::Win32::UI::Accessibility::HWINEVENTHOOK, ev: u32, h: HWND, object: i32, _: i32, _: u32, _: u32) {
    if object != 0 || !ON.load(SeqCst) {
        return;
    }
    let hv = h.0 as isize;
    if ev == 0x0011 {
        // EVENT_SYSTEM_DIALOGEND: answered (from either one), or closed
        let was = {
            let mut held = HELD.lock().unwrap();
            held.iter().position(|d| d.hwnd == hv).map(|i| held.remove(i))
        };
        if let Some(d) = was {
            if unsafe { IsWindow(Some(h)) }.as_bool() && unsafe { IsWindowVisible(h) }.as_bool() {
                veil(h, false, d.layered);
            }
            gone(hv);
        }
        return;
    }
    if HELD.lock().unwrap().iter().any(|d| d.hwnd == hv) {
        return;
    }
    let Some(d) = read(h) else { return };
    let layered = unsafe { GetWindowLongPtrW(h, GWL_EXSTYLE) } & WS_EX_LAYERED.0 as isize != 0;
    // (made see-through at once, so the real box doesn't flash up before the glass one)
    if !veil(h, true, layered) {
        return;
    }
    HELD.lock().unwrap().push(Held { hwnd: hv, layered, at: Instant::now(), shown: false });
    if let Some(app) = APP.get() {
        let _ = app.emit_to("main", "dialog", &d);
    }
}

fn start() {
    use windows::Win32::UI::Accessibility::SetWinEventHook;
    use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, MsgWaitForMultipleObjects, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, QS_ALLINPUT, WINEVENT_OUTOFCONTEXT};
    std::thread::spawn(|| unsafe {
        // (EVENT_SYSTEM_DIALOGSTART and DIALOGEND: rare, unlike every window shown, so this costs nothing while idle)
        let _hook = SetWinEventHook(0x0010, 0x0011, None, Some(started), 0, 0, WINEVENT_OUTOFCONTEXT);
        let mut m = MSG::default();
        loop {
            while PeekMessageW(&mut m, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&m);
                DispatchMessageW(&m);
            }
            MsgWaitForMultipleObjects(None, false, 300, QS_ALLINPUT);
            // a box that has gone without saying so, or that the page never showed: let go of it
            let mut lost = Vec::new();
            HELD.lock().unwrap().retain(|d| {
                let h = HWND(d.hwnd as _);
                let alive = IsWindow(Some(h)).as_bool();
                let late = !d.shown && d.at.elapsed() > Duration::from_millis(1500);
                if alive && late {
                    veil(h, false, d.layered);
                }
                if !alive || late {
                    lost.push(d.hwnd);
                }
                alive && !late
            });
            for h in lost {
                gone(h);
            }
        }
    });
}

/// Turned on or off in Settings.
pub fn set(app: &AppHandle, on: bool) {
    let _ = APP.set(app.clone());
    ON.store(on, SeqCst);
    if on && !STARTED.swap(true, SeqCst) {
        start();
    }
    if !on {
        release_all();
    }
}

/// The page is showing its own for this box.
pub fn shown(hwnd: isize) {
    if let Some(d) = HELD.lock().unwrap().iter_mut().find(|d| d.hwnd == hwnd) {
        d.shown = true;
    }
}

/// A button pressed on ULTRON's: the same one, pressed on the real box.
pub fn press(hwnd: isize, id: i32) {
    let h = HWND(hwnd as _);
    unsafe {
        let _ = PostMessageW(Some(h), WM_COMMAND, WPARAM(id as u16 as usize), LPARAM(0));
    }
}

/// The page can't show it now (something full screen, the lock screen): the real one comes back.
pub fn pass(hwnd: isize) {
    let d = {
        let mut held = HELD.lock().unwrap();
        held.iter().position(|d| d.hwnd == hwnd).map(|i| held.remove(i))
    };
    if let Some(d) = d {
        veil(HWND(hwnd as _), false, d.layered);
    }
}

/// ULTRON is closing: nothing is left see-through behind it.
pub fn quit() {
    release_all();
}
