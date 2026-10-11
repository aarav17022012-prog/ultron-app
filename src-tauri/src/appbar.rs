//! Registers the menu bar as a Windows "app bar" docked to the top edge, the same
//! mechanism the taskbar uses. The shell then shrinks the work area so maximized
//! windows start below the menu bar instead of sliding under it.

use std::sync::{Mutex, OnceLock};

use tauri::{AppHandle, Emitter};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::UI::Shell::{
    DefSubclassProc, SHAppBarMessage, SetWindowSubclass, ABE_TOP, ABM_ACTIVATE, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE,
    ABM_SETPOS, ABM_WINDOWPOSCHANGED, ABN_FULLSCREENAPP, ABN_POSCHANGED, APPBARDATA,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, WM_ACTIVATE, WM_APP, WM_DISPLAYCHANGE, WM_WINDOWPOSCHANGED,
};

use crate::hit::scale_of;

const WM_APPBAR: u32 = WM_APP + 0x51;

struct Bar {
    hwnd: isize,
    reserve: f64, // CSS px kept free for the menu bar
    height: f64,  // CSS px of the window (taller, so the notch can open downward)
}

static APP: OnceLock<AppHandle> = OnceLock::new();
static BAR: Mutex<Option<Bar>> = Mutex::new(None);

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
    *BAR.lock().unwrap() = Some(Bar { hwnd: hwnd.0 as isize, reserve, height });
    unsafe {
        let mut abd = data(hwnd);
        SHAppBarMessage(ABM_NEW, &mut abd);
        let _ = SetWindowSubclass(hwnd, Some(subclass), 0xD0C, 0);
    }
    position();
}

pub fn position() {
    let guard = BAR.lock().unwrap();
    let Some(bar) = guard.as_ref() else { return };
    let hwnd = HWND(bar.hwnd as _);
    unsafe {
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return;
        }
        let m = mi.rcMonitor;
        let s = scale_of(hwnd);
        let h = (bar.reserve * s).round() as i32;
        let mut abd = data(hwnd);
        abd.rc = RECT { left: m.left, right: m.right, top: m.top, bottom: m.top + h };
        SHAppBarMessage(ABM_QUERYPOS, &mut abd); // the shell moves us below a top taskbar if there is one
        abd.rc.bottom = abd.rc.top + h;
        SHAppBarMessage(ABM_SETPOS, &mut abd);
        let wh = (bar.height * s).round() as i32;
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), m.left, abd.rc.top, m.right - m.left, wh, SWP_NOACTIVATE);
    }
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
        WM_DISPLAYCHANGE => {
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(400));
                position();
            });
        }
        _ => {}
    }
    unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
}
