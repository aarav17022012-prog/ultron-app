//! Dynamic windows (Settings, Look & feel): the window you're working in gets macOS's three buttons, red, yellow and
//! green, at the left of its title bar, and Windows' own minimise / maximise / close at the right are hidden (covered
//! with the title bar's own colour). Red closes, yellow minimises, green maximises (or restores); pointed at, they show
//! their signs, as on a Mac; pressed, they darken. Windows that asked for square corners get rounded ones, and a
//! maximised window (which Windows never rounds) gets rounded corners too: its top ones curve into ULTRON's bar (the
//! page draws those, told by "dyn-max"), its bottom ones are rounded off in black, like a Mac's screen.
//!
//! It's a strip of ULTRON's own, as wide as the title bar, painted here pixel by pixel (no web page): the two patches
//! at its ends, see-through and click-through between them. It's kept on the window in front (looked at ten times a
//! second, sixty while it's being dragged; listening to every move a browser makes slowed the PC), just under ULTRON's
//! overlay so ULTRON's panels still come over it, and hidden when the window in front has no title bar of its own, is
//! full screen, or is ULTRON. It's left out of screen captures, so it can look at the title bar under itself.
//!
//! Windows that draw their own title bar (browsers, Electron apps such as Claude) don't tell Windows where their
//! buttons are (an empty area, measured 15 Oct 2026); for those the buttons are taken to be Windows' standard three,
//! 46 px each. A window that isn't made for high-DPI screens reports them in its own smaller units.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering::SeqCst};
use std::sync::OnceLock;

use tauri::{AppHandle, Emitter};
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_CAPTION_BUTTON_BOUNDS, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_DEFAULT, DWMWCP_ROUND, DWM_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, MonitorFromWindow, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER,
    BLENDFUNCTION, DIB_RGB_COLORS, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetAncestor, GetClassNameW, GetForegroundWindow, GetMessageW, GetSystemMetrics, GetWindowLongW,
    GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, IsZoomed, KillTimer, LoadCursorW, PeekMessageW, PostMessageW,
    PostThreadMessageW, RegisterClassW, SetTimer, SetWindowDisplayAffinity, SetWindowPos, ShowWindow, TranslateMessage, UpdateLayeredWindow,
    EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZESTART, GA_ROOTOWNER,
    GWL_EXSTYLE, GWL_STYLE, IDC_ARROW, MA_NOACTIVATE, MSG, PM_NOREMOVE, SC_CLOSE, SC_MAXIMIZE, SC_MINIMIZE, SC_RESTORE, SM_CXSCREEN, SM_CYSCREEN,
    SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, ULW_ALPHA, WDA_EXCLUDEFROMCAPTURE, WINEVENT_OUTOFCONTEXT, WM_APP, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_SYSCOMMAND, WM_TIMER, WM_USER, WNDCLASSW, WS_CAPTION, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU,
};

static ON: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
static THREAD: AtomicU32 = AtomicU32::new(0);
/// ULTRON's own overlay: everything here is kept just under it, so its panels still come over
static MAIN: AtomicIsize = AtomicIsize::new(0);
static APP: OnceLock<AppHandle> = OnceLock::new();
const SWITCH: u32 = WM_APP + 0x4C;
const FADE: usize = 1;
const LOOK: usize = 2;
const TRACK: usize = 3;
const WM_MOUSELEAVE: u32 = 0x02A3;

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

/// Everything known here, kept on this thread.
#[derive(Default)]
struct St {
    win: isize,              // the strip
    cbl: isize,              // the rounded-off bottom corners of a maximised window
    cbr: isize,
    target: isize,
    hooks: Vec<isize>,       // move / resize start and end, on the target's own process
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    rw: i32,                 // the right patch's width (over Windows' buttons)
    s: f32,                  // the screen's scale
    round: bool,             // top corners rounded (not maximised)
    max: bool,
    bgl: [u8; 3],            // the title bar's colour at the left and at the right
    bgr: [u8; 3],
    cap: i32,                // the title bar's height as seen (0: not yet)
    hover: bool,
    pressed: i32,
    tracking: bool,
    fade: u8,
    shown: bool,
    pending: bool,           // just come to the front: the buttons wait a moment (it may not be drawn yet)
    looks: u8,
    moving: bool,
    rounded: Vec<isize>,     // windows given round corners (put back when this is switched off)
    told_max: Option<bool>,
}

thread_local! {
    static ST: RefCell<St> = RefCell::new(St { pressed: -1, ..Default::default() });
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
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: inst.unwrap_or_default(),
            lpszClassName: w!("UltronLights"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        });
        let make = |click: bool| {
            let ex = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | if click { Default::default() } else { WS_EX_TRANSPARENT };
            CreateWindowExW(ex, w!("UltronLights"), w!(""), WS_POPUP, 0, 0, 8, 8, None, None, inst, None).ok()
        };
        let (Some(win), Some(cbl), Some(cbr)) = (make(true), make(false), make(false)) else {
            crate::log_line("dynamic windows: its windows couldn't be made");
            STARTED.store(false, SeqCst);
            return;
        };
        // (while testing, ULTRON's own pictures of the screen are to show them)
        if std::env::var("ULTRON_DEV_INBOX").is_err() {
            for h in [win, cbl, cbr] {
                let _ = SetWindowDisplayAffinity(h, WDA_EXCLUDEFROMCAPTURE);
            }
        }
        ST.with(|s| {
            let mut s = s.borrow_mut();
            s.win = win.0 as isize;
            s.cbl = cbl.0 as isize;
            s.cbr = cbr.0 as isize;
        });
        let _fg = SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None, Some(on_event), 0, 0, WINEVENT_OUTOFCONTEXT);
        let _mm = SetWinEventHook(EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND, None, Some(on_event), 0, 0, WINEVENT_OUTOFCONTEXT);
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

unsafe extern "system" fn on_event(_: HWINEVENTHOOK, ev: u32, h: HWND, _: i32, _: i32, _: u32, _: u32) {
    unsafe {
        let ours = ST.with(|s| s.borrow().target) == h.0 as isize;
        if ev == EVENT_SYSTEM_MOVESIZESTART {
            if ours {
                ST.with(|s| s.borrow_mut().moving = true);
                tick_every(16);
            }
            return;
        }
        if ev == EVENT_SYSTEM_MOVESIZEEND {
            if ours {
                ST.with(|s| s.borrow_mut().moving = false);
                tick_every(100);
                follow(true);
            }
            return;
        }
        refresh();
    }
}

unsafe fn tick_every(ms: u32) {
    unsafe {
        let win = ST.with(|s| s.borrow().win);
        SetTimer(Some(hwnd(win)), TRACK, ms, None);
    }
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

/// The window in front now: is it one for the buttons.
unsafe fn refresh() {
    unsafe {
        if !ON.load(SeqCst) {
            hide();
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
            hide();
            tell_max(false);
            return;
        }
        if ST.with(|s| s.borrow().target) == fg.0 as isize {
            if !ST.with(|s| s.borrow().pending) {
                follow(true);
            }
            return;
        }
        hide();
        retarget(fg);
        round(fg);
        // (it has only just come to the front, maybe still opening: the buttons come a moment later, faded in)
        let win = ST.with(|s| {
            let mut s = s.borrow_mut();
            s.pending = true;
            s.looks = 0;
            s.win
        });
        SetTimer(Some(hwnd(win)), LOOK, 150, None);
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
        if ex & WS_EX_TOOLWINDOW.0 != 0 || st & WS_CAPTION.0 != WS_CAPTION.0 || st & WS_SYSMENU.0 == 0 || st & (WS_MINIMIZEBOX.0 | WS_MAXIMIZEBOX.0) == 0 {
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

/// Only the window's own process's moves and resizes are listened to (just their start and end: few).
unsafe fn retarget(h: HWND) {
    unsafe {
        for o in ST.with(|s| std::mem::take(&mut s.borrow_mut().hooks)) {
            let _ = UnhookWinEvent(HWINEVENTHOOK(o as _));
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        let a = SetWinEventHook(EVENT_SYSTEM_MOVESIZESTART, EVENT_SYSTEM_MOVESIZEEND, None, Some(on_event), pid, 0, WINEVENT_OUTOFCONTEXT);
        ST.with(|s| {
            let mut s = s.borrow_mut();
            s.target = h.0 as isize;
            s.hooks = vec![a.0 as isize];
            s.cap = 0;
        });
    }
}

/// Rounded corners for a window that asked for square ones.
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

/// Where things go: the title bar's strip along the window's top (as tall as Windows' buttons, until the bar has been
/// seen), the window's visible frame, the width of Windows' buttons at the strip's right, the screen's scale, and
/// whether it's maximised. None for a minimised or full-screen window.
unsafe fn place(h: HWND) -> Option<(RECT, RECT, i32, f32, bool)> {
    unsafe {
        if !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
            return None;
        }
        let wdpi = GetDpiForWindow(h).max(96);
        let (mut mx, mut my) = (0u32, 0u32);
        let mdpi = if GetDpiForMonitor(MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST), MDT_EFFECTIVE_DPI, &mut mx, &mut my).is_ok() { mx.max(96) } else { wdpi };
        let s = mdpi as f32 / 96.0;
        let f = mdpi as f32 / wdpi as f32;
        let k = |v: i32| (v as f32 * f).round() as i32;
        let mut wr = RECT::default();
        GetWindowRect(h, &mut wr).ok()?;
        let mut fr = wr;
        let _ = DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, &mut fr as *mut RECT as _, std::mem::size_of::<RECT>() as u32);
        let mut br = RECT::default();
        let have = DwmGetWindowAttribute(h, DWMWA_CAPTION_BUTTON_BOUNDS, &mut br as *mut RECT as _, std::mem::size_of::<RECT>() as u32).is_ok();
        let max = IsZoomed(h).as_bool();
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        if !max && fr.left <= 0 && fr.top <= 0 && fr.right >= sw && fr.bottom >= sh {
            return None;
        }
        let edge = if max { 0 } else { 1 }; // (the window's own 1-px edge is left showing)
        let top = fr.top + edge;
        let bottom = if have && br.bottom > 0 { (wr.top + k(br.bottom)).max(top + (24.0 * s) as i32) } else { fr.top + (32.0 * s).round() as i32 };
        let rw = if have && k(br.right - br.left) > (40.0 * s) as i32 {
            (fr.right - edge) - (wr.left + k(br.left) - 1)
        } else {
            (46.0 * 3.0 * s).round() as i32 + 1
        };
        let strip = RECT { left: fr.left + edge, top, right: fr.right - edge, bottom };
        if strip.right - strip.left < rw + (100.0 * s) as i32 || bottom - top < 12 {
            return None;
        }
        Some((strip, fr, rw, s, max))
    }
}

/// The title bar's colour and height from a one-pixel strip down it at `x` (the buttons' windows are left out of
/// captures, so this sees through them): the colour a little below the top, and the height where it ends.
unsafe fn measure(x: i32, top: i32, n: i32, at_frac: f32) -> ([u8; 3], i32) {
    let n = n.max(4);
    let Some(px) = crate::backdrop::grab_small(RECT { left: x, top, right: x + 1, bottom: top + n }, 1, n) else {
        return ([32, 32, 36], n);
    };
    let at = |i: i32| -> [i32; 3] {
        let o = (i.clamp(0, n - 1) * 4) as usize;
        [px[o + 2] as i32, px[o + 1] as i32, px[o] as i32]
    };
    let c = at((n as f32 * at_frac) as i32);
    let mut h = n;
    for i in (n / 2)..n {
        let d = at(i);
        if (0..3).map(|k| (d[k] - c[k]).abs()).sum::<i32>() > 36 {
            h = i;
            break;
        }
    }
    ([c[0] as u8, c[1] as u8, c[2] as u8], h.max((n as f32 * 0.6) as i32))
}

/// The title bar's true height from Windows' own close button: its X sits in the middle of the bar, so the bar is twice
/// as tall as the X's middle is low. (Apps that draw their own title bar make it taller than Windows says.) Looked for
/// in the right-most 46 px of the bar, down to 72 px; None if no X is seen.
unsafe fn glyph_cap(right: i32, top: i32, s: f32) -> Option<i32> {
    let (w, h) = ((46.0 * s).round() as i32, (72.0 * s).round() as i32);
    let px = crate::backdrop::grab_small(RECT { left: right - w, top, right, bottom: top + h }, w, h)?;
    let at = |x: i32, y: i32| -> [i32; 3] {
        let o = ((y * w + x) * 4) as usize;
        [px[o + 2] as i32, px[o + 1] as i32, px[o] as i32]
    };
    let bg = at(3, (3.0 * s) as i32);
    let (mut y0, mut y1) = (-1, -1);
    for y in 0..h {
        let ink = ((w / 2 - (7.0 * s) as i32)..=(w / 2 + (7.0 * s) as i32)).any(|x| {
            let c = at(x, y);
            (0..3).map(|k| (c[k] - bg[k]).abs()).sum::<i32>() > 90
        });
        if ink {
            if y0 < 0 {
                y0 = y;
            }
            y1 = y;
        } else if y0 >= 0 && y - y1 > (4.0 * s) as i32 {
            break; // (the X has ended: what's further down is the window's own)
        }
    }
    if y0 < 0 || y1 - y0 < (5.0 * s) as i32 || y1 - y0 > (16.0 * s) as i32 {
        return None;
    }
    Some(((y0 + y1) as f32 + 2.0).round() as i32)
}

/// Keep everything on the window. `fresh`: look at its title bar again (once a move or resize has finished, or the
/// window has just come up).
unsafe fn follow(fresh: bool) {
    unsafe {
        let (t, shown) = ST.with(|s| (s.borrow().target, s.borrow().shown));
        if t == 0 {
            return;
        }
        let Some((mut r, fr, rw, sc, max)) = place(hwnd(t)) else {
            hide();
            tell_max(false);
            return;
        };
        tell_max(max);
        if fresh || !shown || ST.with(|s| s.borrow().cap) == 0 {
            let n = r.bottom - r.top;
            // (while testing they're not left out of captures, so they're put out of the way to be looked under)
            if shown && std::env::var("ULTRON_DEV_INBOX").is_ok() {
                let win = ST.with(|s| s.borrow().win);
                let _ = ShowWindow(hwnd(win), SW_HIDE);
                let _ = windows::Win32::Graphics::Dwm::DwmFlush();
            }
            let (bgr, capr) = measure(r.right - rw + (3.0 * sc).round() as i32, r.top, n, 0.3);
            let cap0 = match glyph_cap(r.right, r.top, sc) {
                Some(g) => g.clamp((22.0 * sc) as i32, (72.0 * sc) as i32),
                None => capr.max((n as f32 * 0.6) as i32),
            };
            // the left end's colour three quarters of the way down the bar (a maximised window's top corner has ULTRON's
            // curve over its top)
            let (mut bgl, _) = measure(r.left + (2.0 * sc).round() as i32, r.top, cap0, 0.72);
            let capl = cap0;
            // A title bar is one colour end to end: if its two ends differ, the window isn't drawn yet (it has only
            // just come up): the buttons wait for it (a few more looks, a sixth of a second apart).
            let off: i32 = (0..3).map(|k| (bgl[k] as i32 - bgr[k] as i32).abs()).sum();
            if !shown && off > 40 && ST.with(|s| s.borrow().looks) < 4 {
                let win = ST.with(|s| {
                    let mut s = s.borrow_mut();
                    s.pending = true;
                    s.win
                });
                SetTimer(Some(hwnd(win)), LOOK, 160, None);
                return;
            }
            // (still two colours once it's drawn: the left end has something of the window's own frame at its very edge;
            //  the right end, read off Windows' own button, is the bar's)
            if off > 40 {
                bgl = bgr;
            }
            let cap = capl;
            ST.with(|s| {
                let mut s = s.borrow_mut();
                s.bgl = bgl;
                s.bgr = bgr;
                s.cap = cap;
            });
        }
        let cap = ST.with(|s| s.borrow().cap);
        r.bottom = r.top + cap;
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let changed = ST.with(|s| {
            let s = s.borrow();
            s.w != w || s.h != h || s.rw != rw || s.max != max || (s.s - sc).abs() > 0.01
        });
        let moved = ST.with(|s| {
            let s = s.borrow();
            s.x != r.left || s.y != r.top
        });
        ST.with(|s| {
            let mut s = s.borrow_mut();
            s.x = r.left;
            s.y = r.top;
            s.w = w;
            s.h = h;
            s.rw = rw;
            s.s = sc;
            s.max = max;
            s.round = !max;
        });
        let first = !shown;
        if first {
            ST.with(|s| s.borrow_mut().fade = 0);
        }
        if first || changed || fresh {
            paint();
            corners(&fr, sc, max);
        } else if moved {
            moveto();
            corners(&fr, sc, max);
        }
        if first {
            let win = ST.with(|s| {
                let mut s = s.borrow_mut();
                s.shown = true;
                s.win
            });
            SetTimer(Some(hwnd(win)), FADE, 15, None);
            tick_every(if ST.with(|s| s.borrow().moving) { 16 } else { 100 });
        }
    }
}

unsafe fn after() -> Option<HWND> {
    let main = MAIN.load(SeqCst);
    if main != 0 { Some(hwnd(main)) } else { None }
}

unsafe fn moveto() {
    unsafe {
        let (win, x, y, w, h) = ST.with(|s| {
            let s = s.borrow();
            (s.win, s.x, s.y, s.w, s.h)
        });
        let _ = SetWindowPos(hwnd(win), after(), x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    }
}

unsafe fn hide() {
    unsafe {
        let (win, cbl, cbr) = ST.with(|s| {
            let s = s.borrow();
            (s.win, s.cbl, s.cbr)
        });
        if win != 0 {
            for t in [FADE, LOOK, TRACK] {
                let _ = KillTimer(Some(hwnd(win)), t);
            }
            for h in [win, cbl, cbr] {
                let _ = ShowWindow(hwnd(h), SW_HIDE);
            }
        }
        ST.with(|s| {
            let mut s = s.borrow_mut();
            s.shown = false;
            s.pending = false;
            s.target = 0;
            s.hover = false;
            s.pressed = -1;
        });
    }
}

/// A picture handed to a layered window: `fill(x, y) -> (r, g, b; alpha 0..1)`, premultiplied as Windows wants it.
unsafe fn show_picture(win: isize, x: i32, y: i32, w: i32, h: i32, alpha: u8, fill: impl Fn(f32, f32) -> ([f32; 3], f32)) {
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
        let _ = SetWindowPos(hwnd(win), after(), x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        let (pos, size, zero) = (POINT { x, y }, SIZE { cx: w, cy: h }, POINT::default());
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: alpha, AlphaFormat: AC_SRC_ALPHA as u8 };
        let _ = UpdateLayeredWindow(hwnd(win), None, Some(&pos), Some(&size), Some(dc), Some(&zero), COLORREF(0), Some(&blend), ULW_ALPHA);
        SelectObject(dc, was);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(dc);
    }
}

/// A maximised window's bottom corners, rounded off in black (like a Mac's screen); hidden otherwise.
unsafe fn corners(fr: &RECT, s: f32, max: bool) {
    unsafe {
        let (cbl, cbr, fade) = ST.with(|st| {
            let st = st.borrow();
            (st.cbl, st.cbr, st.fade)
        });
        if !max {
            let _ = ShowWindow(hwnd(cbl), SW_HIDE);
            let _ = ShowWindow(hwnd(cbr), SW_HIDE);
            return;
        }
        let r = (10.0 * s).round() as i32;
        let rf = r as f32;
        // (black outside a quarter circle centred on the corner's inner point)
        let cut = move |cx: f32, cy: f32| {
            move |x: f32, y: f32| {
                let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
                ([0.0, 0.0, 0.0], (d - rf + 0.5).clamp(0.0, 1.0))
            }
        };
        show_picture(cbl, fr.left, fr.bottom - r, r, r, fade, cut(rf, 0.0));
        show_picture(cbr, fr.right - r, fr.bottom - r, r, r, fade, cut(0.0, 0.0));
    }
}

/// The three buttons, in the screen's scale: 12 px across, 8 apart, the first 13 px in from the window's left edge, as
/// on a Mac; macOS's own colours (fill, rim, sign).
const FILL: [[u8; 3]; 3] = [[0xff, 0x5f, 0x57], [0xfe, 0xbc, 0x2e], [0x28, 0xc8, 0x40]];
const RIM: [[u8; 3]; 3] = [[0xe0, 0x44, 0x3e], [0xde, 0xa1, 0x23], [0x1a, 0xab, 0x29]];
const SIGN: [[u8; 3]; 3] = [[0x4d, 0x00, 0x00], [0x98, 0x57, 0x12], [0x0b, 0x65, 0x0d]];

fn layout(h: i32, s: f32) -> ([f32; 3], f32, f32, i32) {
    let d = 12.0 * s;
    let gap = 8.0 * s;
    let x0 = 13.0 * s + d / 2.0;
    let lw = (13.0 * s * 2.0 + 3.0 * d + 2.0 * gap).round() as i32;
    ([x0, x0 + d + gap, x0 + 2.0 * (d + gap)], h as f32 / 2.0, d / 2.0, lw)
}

fn seg(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let t = (((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy).max(1e-6)).clamp(0.0, 1.0);
    ((px - ax - t * dx).powi(2) + (py - ay - t * dy).powi(2)).sqrt()
}

/// The strip: the left patch with the three buttons, the right patch over Windows' own, nothing between.
unsafe fn paint() {
    unsafe {
        let (win, x, y, w, h, rw, s, round, bgl, bgr, hover, pressed, fade) = ST.with(|st| {
            let st = st.borrow();
            (st.win, st.x, st.y, st.w, st.h, st.rw, st.s, st.round, st.bgl, st.bgr, st.hover, st.pressed, st.fade)
        });
        let (cx, cy, r, lw) = layout(h, s);
        let rc = if round { 8.0 * s } else { 0.0 };
        let (sign, thick) = (3.4 * s, 1.15 * s);
        let (wf, rwf, lwf) = (w as f32, rw as f32, lw as f32);
        show_picture(win, x, y, w, h, fade, |fx, fy| {
            let left = fx < lwf;
            if !left && fx < wf - rwf {
                return ([0.0; 3], 0.0);
            }
            // the patch, its outer top corner rounded like the window's
            let mut a = 1.0f32;
            if rc > 0.0 && fy < rc && ((left && fx < rc) || (!left && fx > wf - rc)) {
                let cxr = if left { rc } else { wf - rc };
                let d = ((fx - cxr).powi(2) + (fy - rc).powi(2)).sqrt();
                a = (rc - d + 0.5).clamp(0.0, 1.0);
            }
            let bg = if left { bgl } else { bgr };
            let mut c = [bg[0] as f32, bg[1] as f32, bg[2] as f32];
            if left {
                for k in 0..3 {
                    let d = ((fx - cx[k]).powi(2) + (fy - cy).powi(2)).sqrt();
                    let cov = (r - d + 0.5).clamp(0.0, 1.0);
                    if cov <= 0.0 {
                        continue;
                    }
                    let rim = (d - (r - 1.0 * s) + 0.5).clamp(0.0, 1.0);
                    let dark = if pressed == k as i32 { 0.82 } else { 1.0 };
                    let mut dot = [0f32; 3];
                    for q in 0..3 {
                        dot[q] = (FILL[k][q] as f32 * (1.0 - rim) + RIM[k][q] as f32 * rim) * dark;
                    }
                    if hover {
                        let (ox, oy) = (fx - cx[k], fy - cy);
                        let dist = match k {
                            0 => seg(ox, oy, -sign, -sign, sign, sign).min(seg(ox, oy, -sign, sign, sign, -sign)),
                            1 => seg(ox, oy, -sign * 1.15, 0.0, sign * 1.15, 0.0),
                            _ => seg(ox, oy, -sign * 1.1, 0.0, sign * 1.1, 0.0).min(seg(ox, oy, 0.0, -sign * 1.1, 0.0, sign * 1.1)),
                        };
                        let sc = (thick / 2.0 - dist + 0.5).clamp(0.0, 1.0);
                        for q in 0..3 {
                            dot[q] = dot[q] * (1.0 - sc) + SIGN[k][q] as f32 * sc;
                        }
                    }
                    for q in 0..3 {
                        c[q] = c[q] * (1.0 - cov) + dot[q] * cov;
                    }
                }
            }
            (c, a)
        });
    }
}

fn which(px: i32, py: i32) -> i32 {
    ST.with(|s| {
        let s = s.borrow();
        let (cx, cy, r, _) = layout(s.h, s.s);
        for k in 0..3 {
            if ((px as f32 - cx[k]).powi(2) + (py as f32 - cy).powi(2)).sqrt() <= r + 2.0 * s.s {
                return k as i32;
            }
        }
        -1
    })
}

fn over_group(px: i32, py: i32) -> bool {
    ST.with(|s| {
        let s = s.borrow();
        let (cx, cy, r, _) = layout(s.h, s.s);
        px as f32 >= cx[0] - r - 6.0 * s.s && px as f32 <= cx[2] + r + 6.0 * s.s && (py as f32 - cy).abs() <= r + 6.0 * s.s
    })
}

unsafe extern "system" fn proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let (mx, my) = ((lp.0 & 0xffff) as i16 as i32, ((lp.0 >> 16) & 0xffff) as i16 as i32);
        match m {
            WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
            WM_MOUSEMOVE => {
                if !ST.with(|s| s.borrow().tracking) {
                    let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
                    let _ = TrackMouseEvent(&mut t);
                    ST.with(|s| s.borrow_mut().tracking = true);
                }
                let hov = over_group(mx, my);
                if ST.with(|s| s.borrow().hover) != hov {
                    ST.with(|s| s.borrow_mut().hover = hov);
                    paint();
                }
                return LRESULT(0);
            }
            WM_MOUSELEAVE => {
                ST.with(|s| {
                    let mut s = s.borrow_mut();
                    s.tracking = false;
                    s.hover = false;
                    s.pressed = -1;
                });
                paint();
                return LRESULT(0);
            }
            WM_LBUTTONDOWN => {
                let k = which(mx, my);
                if k >= 0 {
                    ST.with(|s| s.borrow_mut().pressed = k);
                    SetCapture(h);
                    paint();
                }
                return LRESULT(0);
            }
            WM_LBUTTONUP => {
                let p = ST.with(|s| s.borrow().pressed);
                if p >= 0 {
                    let _ = ReleaseCapture();
                    ST.with(|s| s.borrow_mut().pressed = -1);
                    if which(mx, my) == p {
                        let t = hwnd(ST.with(|s| s.borrow().target));
                        let cmd = match p {
                            0 => SC_CLOSE,
                            1 => SC_MINIMIZE,
                            _ => {
                                if IsZoomed(t).as_bool() {
                                    SC_RESTORE
                                } else {
                                    SC_MAXIMIZE
                                }
                            }
                        };
                        let _ = PostMessageW(Some(t), WM_SYSCOMMAND, WPARAM(cmd as usize), LPARAM(0));
                    }
                    paint();
                }
                return LRESULT(0);
            }
            WM_TIMER if wp.0 == TRACK => {
                follow(false);
                return LRESULT(0);
            }
            WM_TIMER if wp.0 == LOOK => {
                // the first look puts the buttons up; one more, a little later, catches a bar still being drawn then
                let n = ST.with(|s| {
                    let mut s = s.borrow_mut();
                    s.pending = false;
                    s.looks += 1;
                    s.looks
                });
                let _ = KillTimer(Some(h), LOOK);
                follow(true);
                // (up: one more look a little later, for a bar still being drawn when it came up)
                if ST.with(|s| s.borrow().shown) && !ST.with(|s| s.borrow().pending) && ST.with(|s| s.borrow().looks) < 8 {
                    ST.with(|s| s.borrow_mut().looks = 8);
                    SetTimer(Some(h), LOOK, 700, None);
                }
                let _ = n;
                return LRESULT(0);
            }
            WM_TIMER if wp.0 == FADE => {
                // (coming on: faded in over about a tenth of a second)
                let f = ST.with(|s| {
                    let mut s = s.borrow_mut();
                    s.fade = s.fade.saturating_add(40);
                    s.fade
                });
                if f == 255 {
                    let _ = KillTimer(Some(h), FADE);
                }
                paint();
                if ST.with(|s| s.borrow().max) {
                    let t = ST.with(|s| s.borrow().target);
                    let mut fr = RECT::default();
                    let _ = DwmGetWindowAttribute(hwnd(t), DWMWA_EXTENDED_FRAME_BOUNDS, &mut fr as *mut RECT as _, std::mem::size_of::<RECT>() as u32);
                    corners(&fr, ST.with(|s| s.borrow().s), true);
                }
                return LRESULT(0);
            }
            _ => {}
        }
        DefWindowProcW(h, m, wp, lp)
    }
}
