//! Dynamic windows (Settings, Look & feel): rounded corners on every window. Windows that asked for square corners
//! get rounded ones (Windows' own rounding). A maximised window, which Windows never rounds, gets them too: its top
//! corners curve into SYLVER's menu bar (the page draws those, in the bar's own glass, told by "dyn-max"), and its
//! bottom corners are rounded off in black, like a Mac's screen, by two small windows of SYLVER's own drawn here.
//! Every curve comes and goes animated: it opens out from a sharp corner and closes back into one.
//!
//! The window in front is looked at when it changes (a foreground hook) and ten times a second while it stays (for a
//! maximise or restore); nothing listens to every move a window makes (browsers send thousands, and that slowed the
//! PC). The corners are kept just under SYLVER's own overlay and left out of screen captures (except while testing).

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering::SeqCst};
use std::sync::OnceLock;

use tauri::{AppHandle, Emitter};
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DEFAULT, DWMWCP_ROUND,
    DWM_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, MonitorFromWindow, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER,
    BLENDFUNCTION, DIB_RGB_COLORS, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetAncestor, GetClassNameW, GetForegroundWindow, GetMessageW, GetSystemMetrics, GetWindowLongW,
    GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, IsZoomed, KillTimer, PeekMessageW, PostThreadMessageW, RegisterClassW,
    SetTimer, SetWindowDisplayAffinity, SetWindowPos, ShowWindow, TranslateMessage, UpdateLayeredWindow, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND,
    EVENT_SYSTEM_MINIMIZESTART, GA_ROOTOWNER, GWL_EXSTYLE, GWL_STYLE, MSG, PM_NOREMOVE, SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE,
    ULW_ALPHA, WDA_EXCLUDEFROMCAPTURE, WINEVENT_OUTOFCONTEXT, WM_APP, WM_TIMER, WM_USER, WNDCLASSW, WS_CAPTION, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

static ON: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
static THREAD: AtomicU32 = AtomicU32::new(0);
/// SYLVER's own overlay: the corners are kept just under it, so its panels still come over them
static MAIN: AtomicIsize = AtomicIsize::new(0);
static APP: OnceLock<AppHandle> = OnceLock::new();
const SWITCH: u32 = WM_APP + 0x4C;
const TRACK: usize = 1;
const GROW: usize = 2;

pub fn set(app: &AppHandle, on: bool, main: isize) {
    crate::log_line(&format!("dynamic windows: {}", if on { "on" } else { "off" }));
    let _ = APP.set(app.clone());
    ON.store(on, SeqCst);
    MAIN.store(main, SeqCst);
    if STARTED.swap(true, SeqCst) {
        let t = THREAD.load(SeqCst);
        if t != 0 {
            unsafe {
                let _ = PostThreadMessageW(t, SWITCH, WPARAM(0), LPARAM(0));
            }
        }
        return;
    }
    if !on {
        STARTED.store(false, SeqCst);
        return;
    }
    std::thread::spawn(|| unsafe { run() });
}

#[derive(Default)]
struct St {
    cbl: isize,              // the bottom corners' two windows
    cbr: isize,
    frame: RECT,             // the maximised window's frame they sit on
    s: f32,                  // the screen's scale
    k: f32,                  // how far the curve is in, 0 (a sharp corner) to 1
    want: bool,              // whether it should be (a maximised window in front)
    rounded: Vec<isize>,     // windows given round corners (put back when this is switched off)
    told_max: Option<bool>,
}

thread_local! {
    static ST: RefCell<St> = RefCell::new(St::default());
}

fn hwnd(v: isize) -> HWND {
    HWND(v as _)
}

unsafe fn run() {
    unsafe {
        let mut m = MSG::default();
        let _ = PeekMessageW(&mut m, None, WM_USER, WM_USER, PM_NOREMOVE);
        THREAD.store(GetCurrentThreadId(), SeqCst);
        let inst: Option<windows::Win32::Foundation::HINSTANCE> = GetModuleHandleW(None).ok().map(|m| m.into());
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(proc), hInstance: inst.unwrap_or_default(), lpszClassName: w!("SylverCorners"), ..Default::default() });
        let make = || {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
                w!("SylverCorners"),
                w!(""),
                WS_POPUP,
                0,
                0,
                8,
                8,
                None,
                None,
                inst,
                None,
            )
            .ok()
        };
        let (Some(cbl), Some(cbr)) = (make(), make()) else {
            crate::log_line("dynamic windows: its corners couldn't be made");
            STARTED.store(false, SeqCst);
            return;
        };
        // (left out of screen captures, as the rest of SYLVER is; while testing they're to show in SYLVER's own pictures)
        if std::env::var("ULTRON_DEV_INBOX").is_err() {
            for h in [cbl, cbr] {
                let _ = SetWindowDisplayAffinity(h, WDA_EXCLUDEFROMCAPTURE);
            }
        }
        ST.with(|s| {
            let mut s = s.borrow_mut();
            s.cbl = cbl.0 as isize;
            s.cbr = cbr.0 as isize;
        });
        let _fg = SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None, Some(on_event), 0, 0, WINEVENT_OUTOFCONTEXT);
        let _mm = SetWinEventHook(EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND, None, Some(on_event), 0, 0, WINEVENT_OUTOFCONTEXT);
        SetTimer(Some(cbl), TRACK, 100, None);
        refresh();
        while GetMessageW(&mut m, None, 0, 0).as_bool() {
            if m.hwnd.is_invalid() && m.message == SWITCH {
                refresh();
                continue;
            }
            let _ = TranslateMessage(&m);
            DispatchMessageW(&m);
        }
    }
}

unsafe extern "system" fn on_event(_: HWINEVENTHOOK, _: u32, _: HWND, _: i32, _: i32, _: u32, _: u32) {
    unsafe { refresh() }
}

fn tell_max(max: bool) {
    let changed = ST.with(|s| {
        let mut s = s.borrow_mut();
        let c = s.told_max != Some(max);
        s.told_max = Some(max);
        c
    });
    if changed {
        if let Some(app) = APP.get() {
            let _ = app.emit_to("main", "dyn-max", max);
        }
    }
}

unsafe fn eligible(h: HWND) -> bool {
    unsafe {
        if h.is_invalid() || !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if pid == GetCurrentProcessId() {
            return false;
        }
        let st = GetWindowLongW(h, GWL_STYLE) as u32;
        let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 || st & WS_CAPTION.0 != WS_CAPTION.0 {
            return false;
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut _ as _, 4);
        if cloaked != 0 {
            return false;
        }
        let mut cls = [0u16; 64];
        let n = GetClassNameW(h, &mut cls);
        let cls = String::from_utf16_lossy(&cls[..n.max(0) as usize]);
        !matches!(cls.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "Windows.UI.Core.CoreWindow")
    }
}

/// Rounded corners for a window that asked for square ones (Windows' own rounding).
unsafe fn round(h: HWND) {
    unsafe {
        if ST.with(|s| s.borrow().rounded.contains(&(h.0 as isize))) {
            return;
        }
        let v = DWMWCP_ROUND;
        if DwmSetWindowAttribute(h, DWMWA_WINDOW_CORNER_PREFERENCE, &v as *const _ as _, std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32).is_ok() {
            ST.with(|s| s.borrow_mut().rounded.push(h.0 as isize));
        }
    }
}

/// The window in front: rounded; and if it's maximised, its corners. (Called when the window in front changes, and ten
/// times a second for one maximised or restored meanwhile.)
unsafe fn refresh() {
    unsafe {
        if !ON.load(SeqCst) {
            want(false, None, 1.0);
            tell_max(false);
            let old = ST.with(|s| std::mem::take(&mut s.borrow_mut().rounded));
            for h in old {
                if IsWindow(Some(hwnd(h))).as_bool() {
                    let v = DWMWCP_DEFAULT;
                    let _ = DwmSetWindowAttribute(hwnd(h), DWMWA_WINDOW_CORNER_PREFERENCE, &v as *const _ as _, std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32);
                }
            }
            return;
        }
        let fg = GetAncestor(GetForegroundWindow(), GA_ROOTOWNER);
        if !eligible(fg) {
            want(false, None, 1.0);
            tell_max(false);
            return;
        }
        round(fg);
        let max = IsZoomed(fg).as_bool();
        let mut fr = RECT::default();
        if GetWindowRect(fg, &mut fr).is_err() {
            return;
        }
        let _ = DwmGetWindowAttribute(fg, DWMWA_EXTENDED_FRAME_BOUNDS, &mut fr as *mut RECT as _, std::mem::size_of::<RECT>() as u32);
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        // (a full-screen window, a video say, has no corners to round)
        let full = !max && fr.left <= 0 && fr.top <= 0 && fr.right >= sw && fr.bottom >= sh;
        let (mut mx, mut my) = (0u32, 0u32);
        let s = if GetDpiForMonitor(MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST), MDT_EFFECTIVE_DPI, &mut mx, &mut my).is_ok() { mx.max(96) as f32 / 96.0 } else { 1.25 };
        want(max && !full, Some(fr), s);
        tell_max(max && !full);
    }
}

/// Bring the bottom corners in or out: the curve grows from a sharp corner (or shrinks back into one) over a quarter
/// of a second.
unsafe fn want(on: bool, fr: Option<RECT>, s: f32) {
    unsafe {
        let (cbl, was, moved) = ST.with(|st| {
            let mut st = st.borrow_mut();
            let mut moved = false;
            if let Some(f) = fr {
                moved = f != st.frame || (st.s - s).abs() > 0.01;
                st.frame = f;
            }
            st.s = s;
            let was = st.want;
            st.want = on;
            (st.cbl, was, moved)
        });
        if on != was {
            SetTimer(Some(hwnd(cbl)), GROW, 16, None);
        } else if on && moved {
            draw();
        }
    }
}

/// Black outside a quarter circle (whose centre is the corner's inner point), its radius by how far in the curve is.
unsafe fn draw() {
    unsafe {
        let (cbl, cbr, fr, s, k) = ST.with(|st| {
            let st = st.borrow();
            (st.cbl, st.cbr, st.frame, st.s, st.k)
        });
        if k <= 0.001 {
            let _ = ShowWindow(hwnd(cbl), SW_HIDE);
            let _ = ShowWindow(hwnd(cbr), SW_HIDE);
            return;
        }
        // each corner's box is r square; the curve, rf across, sits in its outer corner (black only beyond it)
        let r = (11.0 * s).round() as i32;
        let (rb, rf) = (r as f32, 11.0 * s * k);
        show_picture(cbl, fr.left, fr.bottom - r, r, r, move |x, y| {
            let (cx, cy) = (rf, rb - rf);
            if x > cx || y < cy {
                return ([0.0; 3], 0.0);
            }
            let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
            ([0.0; 3], (d - rf + 0.5).clamp(0.0, 1.0))
        });
        show_picture(cbr, fr.right - r, fr.bottom - r, r, r, move |x, y| {
            let (cx, cy) = (rb - rf, rb - rf);
            if x < cx || y < cy {
                return ([0.0; 3], 0.0);
            }
            let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
            ([0.0; 3], (d - rf + 0.5).clamp(0.0, 1.0))
        });
    }
}

/// A picture handed to a layered window: `fill(x, y) -> (r, g, b; alpha 0..1)`, premultiplied as Windows wants it.
unsafe fn show_picture(win: isize, x: i32, y: i32, w: i32, h: i32, fill: impl Fn(f32, f32) -> ([f32; 3], f32)) {
    unsafe {
        if w <= 0 || h <= 0 {
            return;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -h, biPlanes: 1, biBitCount: 32, ..Default::default() },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let Ok(bmp) = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) else { return };
        if bits.is_null() {
            let _ = DeleteObject(bmp.into());
            return;
        }
        let px = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
        for j in 0..h {
            for i in 0..w {
                let (c, a) = fill(i as f32 + 0.5, j as f32 + 0.5);
                let o = ((j * w + i) * 4) as usize;
                px[o] = (c[2] * a) as u8;
                px[o + 1] = (c[1] * a) as u8;
                px[o + 2] = (c[0] * a) as u8;
                px[o + 3] = (255.0 * a) as u8;
            }
        }
        let dc = CreateCompatibleDC(None);
        let was = SelectObject(dc, bmp.into());
        let main = MAIN.load(SeqCst);
        let after = if main != 0 { Some(hwnd(main)) } else { None };
        let _ = SetWindowPos(hwnd(win), after, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        let (pos, size, zero) = (POINT { x, y }, SIZE { cx: w, cy: h }, POINT::default());
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        let _ = UpdateLayeredWindow(hwnd(win), None, Some(&pos), Some(&size), Some(dc), Some(&zero), COLORREF(0), Some(&blend), ULW_ALPHA);
        SelectObject(dc, was);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(dc);
    }
}

unsafe extern "system" fn proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if m == WM_TIMER && wp.0 == TRACK {
            refresh();
            return LRESULT(0);
        }
        if m == WM_TIMER && wp.0 == GROW {
            // (in over about a quarter of a second, easing out; out a little quicker, easing in)
            let (k, on) = ST.with(|s| {
                let mut s = s.borrow_mut();
                s.k = if s.want { (s.k + 0.075).min(1.0) } else { (s.k - 0.1).max(0.0) };
                (s.k, s.want)
            });
            draw();
            if (on && k >= 1.0) || (!on && k <= 0.0) {
                let _ = KillTimer(Some(h), GROW);
            }
            return LRESULT(0);
        }
        DefWindowProcW(h, m, wp, lp)
    }
}
