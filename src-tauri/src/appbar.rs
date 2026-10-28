//! Registers the menu bar as a Windows "app bar" docked to the top edge, the same
//! mechanism the taskbar uses. The shell then shrinks the work area so maximized
//! windows start below the menu bar instead of sliding under it.

use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Mutex, OnceLock};

use tauri::{AppHandle, Emitter};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::UI::Shell::{
    DefSubclassProc, SHAppBarMessage, SetWindowSubclass, ABE_TOP, ABM_ACTIVATE, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE,
    ABM_SETPOS, ABM_WINDOWPOSCHANGED, ABN_FULLSCREENAPP, ABN_POSCHANGED, APPBARDATA,
};
use windows::Win32::UI::WindowsAndMessaging::{
    PostMessageW, SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, WM_ACTIVATE, WM_APP, WM_DISPLAYCHANGE, WM_WINDOWPOSCHANGED,
};

use crate::hit::scale_of;

const WM_APPBAR: u32 = WM_APP + 0x51;
const WM_PLACE: u32 = WM_APP + 0x52; // "put the bar where it belongs", asked of the window's own thread

struct Bar {
    hwnd: isize,
    reserve: f64, // CSS px kept free for the menu bar
    full: f64,    // what that is when the bar is always there (it's 0 while the bar hides away)
    height: f64,  // CSS px of the window (taller, so the notch can open downward)
}

static APP: OnceLock<AppHandle> = OnceLock::new();
static BAR: Mutex<Option<Bar>> = Mutex::new(None);
static PLACING: AtomicBool = AtomicBool::new(false);
static AGAIN: AtomicBool = AtomicBool::new(false);

fn data(hwnd: HWND) -> APPBARDATA {
    APPBARDATA {
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        hWnd: hwnd,
        uCallbackMessage: WM_APPBAR,
        uEdge: ABE_TOP,
        ..Default::default()
    }
}

pub fn register(app: &AppHandle, hwnd: HWND, reserve: f64, height: f64) {
    let _ = APP.set(app.clone());
    *BAR.lock().unwrap() = Some(Bar { hwnd: hwnd.0 as isize, reserve, full: reserve, height });
    unsafe {
        let mut abd = data(hwnd);
        SHAppBarMessage(ABM_NEW, &mut abd);
        let _ = SetWindowSubclass(hwnd, Some(subclass), 0xD0C, 0);
    }
    position();
}

/// Put the bar where it belongs. Asking the shell for the strip and moving the window both send
/// messages back to the window (and the shell answers with "positions changed", which asks for this
/// again), so nothing is held while they run: this used to keep the lock on `BAR` throughout, and a
/// change of display arriving together with the shell's notice left the window's thread and a helper
/// thread waiting on each other for good (ULTRON froze until it was ended). Now only one caller at a
/// time does the placing, and anyone who asks meanwhile just has it done once more afterwards.
pub fn position() {
    AGAIN.store(true, SeqCst);
    while AGAIN.load(SeqCst) && !PLACING.swap(true, SeqCst) {
        let mut turns = 0;
        while AGAIN.swap(false, SeqCst) && turns < 4 {
            place();
            turns += 1;
        }
        PLACING.store(false, SeqCst);
    }
}

fn place() {
    let Some((hwnd, reserve, height)) = BAR.lock().unwrap().as_ref().map(|b| (HWND(b.hwnd as _), b.reserve, b.height)) else { return };
    unsafe {
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return;
        }
        let m = mi.rcMonitor;
        let s = scale_of(hwnd);
        let h = (reserve * s).round() as i32;
        let mut abd = data(hwnd);
        abd.rc = RECT { left: m.left, right: m.right, top: m.top, bottom: m.top + h };
        SHAppBarMessage(ABM_QUERYPOS, &mut abd); // the shell moves us below a top taskbar if there is one
        abd.rc.bottom = abd.rc.top + h;
        SHAppBarMessage(ABM_SETPOS, &mut abd);
        let wh = (height * s).round() as i32;
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), m.left, abd.rc.top, m.right - m.left, wh, SWP_NOACTIVATE);
    }
}

/// Keep room for the bar at the top of the screen (windows stop below it), or none: the bar then hides away and
/// comes down over the windows when it's wanted, the way Windows' own taskbar does when it hides.
pub fn reserve(on: bool) {
    if let Some(bar) = BAR.lock().unwrap().as_mut() {
        bar.reserve = if on { bar.full } else { 0.0 };
    }
    position();
}

/// Keep the remembered window height (CSS px) in step with the page's own resizes.
pub fn remember_height(height: f64) {
    if let Some(bar) = BAR.lock().unwrap().as_mut() {
        bar.height = height;
    }
}

pub fn remove() {
    if let Some(bar) = BAR.lock().unwrap().take() {
        unsafe {
            let mut abd = data(HWND(bar.hwnd as _));
            SHAppBarMessage(ABM_REMOVE, &mut abd);
        }
    }
}

/// Windows says an app has gone full screen, and ULTRON's bar, notch and the wheel's arrow stand aside. It says so
/// for more than a full-screen app, though: for a moment as you switch apps (Alt+Tab's switcher, Task View) and as some
/// apps open (a splash screen across the screen), and then that it's over. Each time the bar went and came back: the
/// arrow under it flickered. Now going is put off a moment and done only if what's in front still fills its screen and
/// is an app's window; coming back is at once.
static FULL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// An app is full screen now (a game, a video), as Windows has said: ULTRON's Alt+Tab steps aside for it.
pub static FULL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn full_screen(on: bool) {
    let n = FULL_SEQ.fetch_add(1, SeqCst) + 1;
    let Some(app) = APP.get().cloned() else { return };
    if !on {
        FULL.store(false, SeqCst);
        let _ = app.emit_to("main", "fullscreen", false);
        return;
    }
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(260));
        if FULL_SEQ.load(SeqCst) == n && really_full() {
            FULL.store(true, SeqCst);
            let _ = app.emit_to("main", "fullscreen", true);
        }
    });
}

/// What's in front fills its whole screen, and isn't one of Windows' own passing screens.
pub(crate) fn really_full() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetForegroundWindow, GetWindowRect};
    unsafe {
        let h = GetForegroundWindow();
        if h.is_invalid() {
            return false;
        }
        let mut buf = [0u16; 64];
        let n = GetClassNameW(h, &mut buf).max(0) as usize;
        let class = String::from_utf16_lossy(&buf[..n]);
        const PASSING: [&str; 9] = [
            "MultitaskingViewFrame", "XamlExplorerHostIslandWindow", "ForegroundStaging", "TaskSwitcherWnd", "Windows.UI.Core.CoreWindow",
            "Shell_TrayWnd", "Shell_SecondaryTrayWnd", "Progman", "WorkerW",
        ];
        if PASSING.contains(&class.as_str()) {
            return false;
        }
        let mut r = RECT::default();
        if GetWindowRect(h, &mut r).is_err() {
            return false;
        }
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(MonitorFromWindow(h, MONITOR_DEFAULTTOPRIMARY), &mut mi).as_bool() {
            return true;
        }
        let m = mi.rcMonitor;
        r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
    }
}

/// Windows' shell, started again (after an update, very often, or when Explorer is restarted): everything it knew of
/// ULTRON is gone. ULTRON's strip of the screen was given back to the windows, and Windows' own taskbar came back. Both
/// are put right: the strip asked for again, and the page told, so ULTRON's own taskbar puts Windows' away again.
static SHELL_BACK: OnceLock<u32> = OnceLock::new();

unsafe extern "system" fn subclass(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _ref: usize) -> LRESULT {
    let back = *SHELL_BACK.get_or_init(|| unsafe { windows::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW(windows::core::w!("TaskbarCreated")) });
    if back != 0 && msg == back {
        let mut abd = data(hwnd);
        unsafe { SHAppBarMessage(ABM_NEW, &mut abd) };
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(400));
            position();
            if let Some(app) = APP.get() {
                let _ = app.emit_to("main", "shell-restarted", ());
            }
        });
        return unsafe { DefSubclassProc(hwnd, msg, wp, lp) };
    }
    match msg {
        WM_APPBAR => {
            match wp.0 as u32 {
                ABN_POSCHANGED => position(),
                ABN_FULLSCREENAPP => full_screen(lp.0 != 0),
                _ => {}
            }
            return LRESULT(0);
        }
        WM_ACTIVATE => {
            let mut abd = data(hwnd);
            unsafe { SHAppBarMessage(ABM_ACTIVATE, &mut abd) };
        }
        WM_WINDOWPOSCHANGED => {
            let mut abd = data(hwnd);
            unsafe { SHAppBarMessage(ABM_WINDOWPOSCHANGED, &mut abd) };
        }
        WM_PLACE => {
            position();
            return LRESULT(0);
        }
        WM_DISPLAYCHANGE => {
            // (once the screens have settled; and on this thread, by a message, not from the one that waits)
            let h = hwnd.0 as isize;
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(400));
                let _ = unsafe { PostMessageW(Some(HWND(h as _)), WM_PLACE, WPARAM(0), LPARAM(0)) };
            });
        }
        _ => {}
    }
    unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
}
