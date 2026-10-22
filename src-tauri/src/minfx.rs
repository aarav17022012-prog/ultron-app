//! Minimizing, ULTRON's way (Settings › Look & feel › Minimize animation, under Rounded windows): a window being
//! minimized squeezes and flies up under the bar into the notch (ULTRON's taskbar lives there), and one picked again on
//! ULTRON's taskbar flies back out of it. Windows' own animation shrinks it to a taskbar at the bottom that isn't there.
//!
//! It's drawn by Windows' own compositor: a live picture of the window (as the taskbar's previews are), moved and sized
//! on a see-through layer just under ULTRON's bar at every frame the screen draws, so it is as smooth as Windows' own.
//! While it's on, Windows' own minimize and maximize animations are off, for this session only (Windows' setting on
//! disk isn't touched): they're put back when it's switched off or ULTRON quits, and a restart puts them back anyway.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering::SeqCst};
use std::sync::OnceLock;
use std::time::Instant;

use tauri::{AppHandle, Emitter, Manager};
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmFlush, DwmRegisterThumbnail, DwmUnregisterThumbnail, DwmUpdateThumbnailProperties, DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION,
    DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE,
};
use windows::Win32::Graphics::Gdi::{CreateSolidBrush, GetMonitorInfoW, MonitorFromPoint, MonitorFromRect, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::*;

static ON: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
static THREAD: AtomicU32 = AtomicU32::new(0);
static LAYER: AtomicIsize = AtomicIsize::new(0);
static APP: OnceLock<AppHandle> = OnceLock::new();
const BACK_OUT: u32 = WM_APP + 7;
const SHOW_ME: u32 = WM_APP + 8;

pub fn on() -> bool {
    ON.load(SeqCst)
}

/// Switched on or off (Settings).
pub fn set(app: &AppHandle, on: bool) {
    let _ = APP.set(app.clone());
    ON.store(on, SeqCst);
    if on {
        if windows_anim() {
            let _ = std::fs::write(note(), "1");
            set_windows_anim(false);
        }
        start();
    } else {
        heal();
    }
}

/// Windows' own animations put back if ULTRON had turned them off (switched off, quitting, or left over from a crash).
pub fn heal() {
    if std::fs::remove_file(note()).is_ok() {
        set_windows_anim(true);
    }
}

fn note() -> std::path::PathBuf {
    std::env::var("LOCALAPPDATA").map(std::path::PathBuf::from).unwrap_or_else(|_| std::env::temp_dir()).join("ULTRON").join("minimize-animation-was-on")
}

fn windows_anim() -> bool {
    let mut ai = ANIMATIONINFO { cbSize: std::mem::size_of::<ANIMATIONINFO>() as u32, iMinAnimate: 0 };
    unsafe {
        let _ = SystemParametersInfoW(SPI_GETANIMATION, ai.cbSize, Some(&mut ai as *mut _ as *mut core::ffi::c_void), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
    }
    ai.iMinAnimate != 0
}

/// (for this session only: SPIF_SENDCHANGE without SPIF_UPDATEINIFILE, so nothing is written to Windows' settings)
fn set_windows_anim(on: bool) {
    let mut ai = ANIMATIONINFO { cbSize: std::mem::size_of::<ANIMATIONINFO>() as u32, iMinAnimate: on as i32 };
    unsafe {
        let _ = SystemParametersInfoW(SPI_SETANIMATION, ai.cbSize, Some(&mut ai as *mut _ as *mut core::ffi::c_void), SPIF_SENDCHANGE);
    }
}

/// The animation shown on a window without minimizing it, and without touching Windows' own (to see what it's like).
pub fn preview(app: &AppHandle, h: HWND, back: bool) {
    let _ = APP.set(app.clone());
    start();
    for _ in 0..50 {
        let t = THREAD.load(SeqCst);
        if t != 0 && LAYER.load(SeqCst) != 0 {
            let _ = unsafe { PostThreadMessageW(t, SHOW_ME, WPARAM(h.0 as usize), LPARAM(back as isize)) };
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// A window picked on ULTRON's taskbar while minimized: it flies back out of the notch, then is there.
pub fn back_out(h: HWND) -> bool {
    let t = THREAD.load(SeqCst);
    on() && t != 0 && unsafe { PostThreadMessageW(t, BACK_OUT, WPARAM(h.0 as usize), LPARAM(0)) }.is_ok()
}

unsafe extern "system" fn layer_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(h, m, w, l) }
}

unsafe extern "system" fn minimizing(_: HWINEVENTHOOK, _: u32, h: HWND, object: i32, child: i32, _: u32, _: u32) {
    if object == 0 && child == 0 && on() && crate::apps::is_app_window(h, std::process::id()) {
        fly(h, false);
    }
}

fn start() {
    if STARTED.swap(true, SeqCst) {
        return;
    }
    std::thread::spawn(|| unsafe {
        THREAD.store(GetCurrentThreadId(), SeqCst);
        // the layer the window's picture moves on: covers every screen, shows nothing of its own (black is see-through),
        // never takes a click, and is shown only while a window is on its way
        let inst = GetModuleHandleW(None).map(|m| windows::Win32::Foundation::HINSTANCE(m.0)).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(layer_proc),
            hInstance: inst,
            hbrBackground: CreateSolidBrush(COLORREF(0)),
            lpszClassName: w!("UltronMinimize"),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let ex = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        if let Ok(layer) = CreateWindowExW(ex, w!("UltronMinimize"), w!(""), WS_POPUP, 0, 0, 1, 1, None, None, Some(inst), None) {
            let _ = SetLayeredWindowAttributes(layer, COLORREF(0), 0, LWA_COLORKEY);
            LAYER.store(layer.0 as isize, SeqCst);
        }
        let _hook = SetWinEventHook(EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZESTART, None, Some(minimizing), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS);
        let mut m = MSG::default();
        while GetMessageW(&mut m, None, 0, 0).as_bool() {
            if m.message == BACK_OUT {
                fly(HWND(m.wParam.0 as _), true);
                continue;
            }
            if m.message == SHOW_ME {
                fly(HWND(m.wParam.0 as _), m.lParam.0 != 0);
                continue;
            }
            let _ = TranslateMessage(&m);
            DispatchMessageW(&m);
        }
    });
}

/// Where a window sits when it isn't minimized (in screen pixels): its own place, or its screen's whole work area.
fn rest_rect(h: HWND) -> Option<RECT> {
    let mut wp = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
    unsafe { GetWindowPlacement(h, &mut wp) }.ok()?;
    let r = wp.rcNormalPosition;
    let mon = unsafe { MonitorFromRect(&r, MONITOR_DEFAULTTONEAREST) };
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    if !unsafe { GetMonitorInfoW(mon, &mut mi) }.as_bool() {
        return None;
    }
    if wp.flags.0 & WPF_RESTORETOMAXIMIZED.0 != 0 || wp.showCmd == SW_SHOWMAXIMIZED.0 as u32 {
        return Some(mi.rcWork);
    }
    // (a window's own place is kept relative to its screen's work area: ULTRON's bar takes the top of it)
    let (dx, dy) = (mi.rcWork.left - mi.rcMonitor.left, mi.rcWork.top - mi.rcMonitor.top);
    Some(RECT { left: r.left + dx, top: r.top + dy, right: r.right + dx, bottom: r.bottom + dy })
}

/// Where it goes: under the notch, at the middle of the main screen, just below the bar.
fn notch_point() -> (f64, f64) {
    let mon = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    let _ = unsafe { GetMonitorInfoW(mon, &mut mi) };
    ((mi.rcMonitor.left + mi.rcMonitor.right) as f64 / 2.0, mi.rcWork.top.max(mi.rcMonitor.top) as f64)
}

/// One window into the notch (`back`: out of it, then shown).
fn fly(h: HWND, back: bool) {
    let layer = HWND(LAYER.load(SeqCst) as _);
    let Some(r0) = rest_rect(h) else { return };
    if layer.is_invalid() {
        if back {
            crate::apps::focus_window(h);
        }
        return;
    }
    let (cx, ty) = notch_point();
    unsafe {
        let (vx, vy) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
        let (vw, vh) = (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN));
        // (just under ULTRON's own window: it slips in under the bar's glass)
        let under = APP.get().and_then(|a| a.get_webview_window("main")).and_then(|w| crate::hit::hwnd_of(&w));
        let _ = SetWindowPos(layer, Some(under.unwrap_or(HWND_TOPMOST)), vx, vy, vw, vh, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        let Ok(thumb) = DwmRegisterThumbnail(layer, h) else {
            let _ = ShowWindow(layer, SW_HIDE);
            if back {
                crate::apps::focus_window(h);
            }
            return;
        };
        let (x0, y0) = ((r0.left + r0.right) as f64 / 2.0, r0.top as f64);
        let (w0, h0) = ((r0.right - r0.left) as f64, (r0.bottom - r0.top) as f64);
        let shape = |q: f64| {
            // q: 0 the window as it was, 1 a sliver at the notch. It squeezes flat early and narrows late, so it
            // reads as being drawn up into the notch; at the very end it fades under the bar.
            let x = x0 + (cx - x0) * q;
            let y = y0 + (ty - 6.0 - y0) * q;
            let hh = h0 + (10.0 - h0) * q.powf(0.7);
            let n = ((q - 0.12) / 0.88).clamp(0.0, 1.0);
            let ww = w0 + (70.0 - w0) * n.powf(1.6);
            let a = if q < 0.72 { 255.0 } else { 255.0 * (1.0 - (q - 0.72) / 0.28) };
            (RECT { left: (x - ww / 2.0) as i32 - vx, top: y as i32 - vy, right: (x + ww / 2.0) as i32 - vx, bottom: (y + hh) as i32 - vy }, a.clamp(0.0, 255.0) as u8)
        };
        let dur = if back { 380.0 } else { 440.0 };
        let t0 = Instant::now();
        loop {
            let t = (t0.elapsed().as_secs_f64() * 1000.0 / dur).min(1.0);
            let q = if back {
                1.0 - (1.0 - (1.0 - t).powi(3)) // (out: quick away from the notch, settling into place)
            } else if t < 0.5 {
                4.0 * t * t * t
            } else {
                1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
            };
            let (rect, a) = shape(q);
            let p = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_OPACITY | DWM_TNP_VISIBLE | DWM_TNP_SOURCECLIENTAREAONLY,
                rcDestination: rect,
                opacity: a,
                fVisible: true.into(),
                fSourceClientAreaOnly: false.into(),
                ..Default::default()
            };
            let _ = DwmUpdateThumbnailProperties(thumb, &p);
            let _ = DwmFlush();
            if t >= 1.0 {
                break;
            }
        }
        if back {
            // (shown where the picture ended, then the picture taken away a frame later: no gap, no double)
            let _ = ShowWindow(h, SW_RESTORE);
            crate::apps::force_front(h);
            let _ = DwmFlush();
            let _ = DwmFlush();
        }
        let _ = DwmUnregisterThumbnail(thumb);
        let _ = ShowWindow(layer, SW_HIDE);
    }
    if !back {
        if let Some(app) = APP.get() {
            let _ = app.emit_to("main", "min-swallow", ());
        }
    }
}
