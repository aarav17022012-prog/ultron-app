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

unsafe extern "system" fn subclass(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _ref: usize) -> LRESULT {
    match msg {
        WM_APPBAR => {
            match wp.0 as u32 {
                ABN_POSCHANGED => position(),
                ABN_FULLSCREENAPP => {
                    if let Some(app) = APP.get() {
                        let _ = app.emit_to("main", "fullscreen", lp.0 != 0);
                    }
                }
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
