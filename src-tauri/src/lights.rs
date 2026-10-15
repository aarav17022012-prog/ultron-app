//! Dynamic windows (Settings, Look & feel): the window you're working in gets macOS's three buttons, red, yellow and
//! green, where Windows' own minimise / maximise / close are. They're drawn over Windows' buttons (one set of
//! buttons, not two), on a patch of the window's own title-bar colour, and they do what Windows' did: red closes,
//! yellow minimises, green maximises (or restores). Pointed at, they show their signs, as on a Mac; pressed, they
//! darken. The window also gets rounded corners where it asked for square ones.
//!
//! It's one small window of ULTRON's own (no web page: it's painted here, pixel by pixel), kept on the window in front
//! as it moves and resizes, just under ULTRON's overlay (so ULTRON's panels still come over it), and hidden when the
//! window in front has no title bar of its own, is full screen, or is ULTRON.
//!
//! Windows that draw their own title bar (browsers, Electron apps such as Claude) don't tell Windows where their
//! buttons are (it reports an empty area, measured 15 Oct 2026); for those the buttons are taken to be Windows' standard
//! three, 46 px each, at the window's top-right.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering::SeqCst};

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_CAPTION_BUTTON_BOUNDS, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DEFAULT, DWMWCP_ROUND, DWM_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO,
    BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS, MonitorFromWindow, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetAncestor, GetClassNameW, GetForegroundWindow, GetMessageW, GetWindowLongW, GetWindowRect,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, IsZoomed, KillTimer, LoadCursorW, PeekMessageW, PostMessageW, PostThreadMessageW,
    RegisterClassW, SetTimer, SetWindowDisplayAffinity, SetWindowPos, WDA_EXCLUDEFROMCAPTURE, ShowWindow, TranslateMessage, UpdateLayeredWindow, EVENT_OBJECT_LOCATIONCHANGE, EVENT_SYSTEM_FOREGROUND,
    EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MOVESIZEEND, GA_ROOTOWNER, GWL_EXSTYLE, GWL_STYLE, IDC_ARROW, MA_NOACTIVATE,
    MSG, OBJID_WINDOW, PM_NOREMOVE, SC_CLOSE, SC_MAXIMIZE, SC_MINIMIZE, SC_RESTORE, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, ULW_ALPHA, WINEVENT_OUTOFCONTEXT,
    WM_APP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_SYSCOMMAND, WM_TIMER, WM_USER, WNDCLASSW, WS_CAPTION,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU,
};

static ON: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
static THREAD: AtomicU32 = AtomicU32::new(0);
/// ULTRON's own overlay: the buttons are kept just under it, so its panels still come over them
static MAIN: AtomicIsize = AtomicIsize::new(0);
const SWITCH: u32 = WM_APP + 0x4C;
const FADE: usize = 1;
const LOOK: usize = 2;
const WM_MOUSELEAVE: u32 = 0x02A3;

pub fn set(on: bool, main: isize) {
    crate::log_line(&format!("dynamic windows: {}", if on { "on" } else { "off" }));
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

/// Everything the buttons' window knows, kept on its own thread.
#[derive(Default)]
struct St {
    win: isize,
    target: isize,
    hook: isize,             // the location hooks (on the target's own process)
    hook2: isize,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    s: f32,                  // the target's scale (its dpi / 96)
    round: bool,             // the top-right corner is rounded (not maximised)
    bg: [u8; 3],             // the title bar's colour, red-green-blue
    hover: bool,             // the pointer is over the three
    hot: i32,                // which one it's over (-1 none)
    pressed: i32,
    tracking: bool,
    fade: u8,
    shown: bool,
    rounded: Vec<isize>,     // windows given round corners (put back when this is switched off)
    cap: i32,                // the title bar's height as seen (0: not yet)
    looks: u8,               // looks taken since it came up
    pending: bool,           // a window has just come to the front: the buttons wait a moment (it may not be drawn yet)
}

thread_local! {
    static ST: RefCell<St> = RefCell::new(St { hot: -1, pressed: -1, ..Default::default() });
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
        let Ok(win) = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("UltronLights"),
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
        ) else {
            crate::log_line("dynamic windows: the buttons' window couldn't be made");
            STARTED.store(false, SeqCst);
            return;
        };
        ST.with(|s| s.borrow_mut().win = win.0 as isize);
        // (left out of screen captures: what's under it, Windows' own buttons on the title bar, can then be looked at
        //  while it's up, for the bar's colour and height)
        let _ = SetWindowDisplayAffinity(win, WDA_EXCLUDEFROMCAPTURE);
        // which window is in front (ULTRON's own included: then the buttons go), and windows being minimised
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

unsafe extern "system" fn on_event(_: HWINEVENTHOOK, ev: u32, h: HWND, obj: i32, child: i32, _: u32, _: u32) {
    unsafe {
        if ev == EVENT_OBJECT_LOCATIONCHANGE {
            // (only the window itself moving or resizing, not its caret or its pointer)
            if obj == OBJID_WINDOW.0 && child == 0 && ST.with(|s| s.borrow().target) == h.0 as isize && !ST.with(|s| s.borrow().pending) {
                follow(false);
            }
            return;
        }
        if ev == EVENT_SYSTEM_MOVESIZEEND {
            if ST.with(|s| s.borrow().target) == h.0 as isize {
                follow(true);
            }
            return;
        }
        refresh();
    }
}

/// The window in front now: is it one to give the buttons to, and where do they go.
unsafe fn refresh() {
    unsafe {
        if !ON.load(SeqCst) {
            hide();
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
            return;
        }
        let same = ST.with(|s| s.borrow().target) == fg.0 as isize;
        if !same {
            hide();
            retarget(fg);
            round(fg);
            // (it has only just come to the front, maybe still opening: the buttons come in a moment later, faded in)
            let win = ST.with(|s| {
                let mut s = s.borrow_mut();
                s.pending = true;
                s.looks = 0;
                s.win
            });
            SetTimer(Some(hwnd(win)), LOOK, 170, None);
            return;
        }
        if !ST.with(|s| s.borrow().pending) {
            follow(true);
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

/// Moves and resizes of the window in front are followed from here on (only its own process's are listened to).
unsafe fn retarget(h: HWND) {
    unsafe {
        let old = ST.with(|s| (s.borrow().hook, s.borrow().hook2));
        for o in [old.0, old.1] {
            if o != 0 {
                let _ = UnhookWinEvent(HWINEVENTHOOK(o as _));
            }
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        let a = SetWinEventHook(EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE, None, Some(on_event), pid, 0, WINEVENT_OUTOFCONTEXT);
        let b = SetWinEventHook(EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZEEND, None, Some(on_event), pid, 0, WINEVENT_OUTOFCONTEXT);
        ST.with(|s| {
            let mut s = s.borrow_mut();
            s.target = h.0 as isize;
            s.hook = a.0 as isize;
            s.hook2 = b.0 as isize;
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
        let ok = DwmSetWindowAttribute(h, DWMWA_WINDOW_CORNER_PREFERENCE, &v as *const _ as _, std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32);
        if ok.is_ok() {
            ST.with(|s| s.borrow_mut().rounded.push(h.0 as isize));
        }
    }
}

/// Where the patch goes on the screen: over the window's caption buttons (Windows' measure of them where it has one,
/// otherwise its standard three at the top-right), from the top of the visible frame.
unsafe fn place(h: HWND) -> Option<(RECT, f32, bool)> {
    unsafe {
        if !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
            return None;
        }
        // The screen's scale (the buttons are drawn at it), and how the window measures itself: one that isn't made for
        // high-DPI screens counts in smaller units that Windows stretches (its caption-button area included).
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
        let edge = if max { 0 } else { 1 }; // (the window's own 1-px edge is left showing)
        let top = fr.top + edge;
        let bottom = if have && br.bottom > 0 { (wr.top + k(br.bottom)).max(top + (24.0 * s) as i32) } else { fr.top + (32.0 * s).round() as i32 };
        let (left, right) = if have && k(br.right - br.left) > (40.0 * s) as i32 {
            (wr.left + k(br.left) - 1, (wr.left + k(br.right) + 1).min(fr.right - edge))
        } else {
            let r = fr.right - edge;
            (r - (46.0 * 3.0 * s).round() as i32 - 1, r)
        };
        // full screen (a frame the size of the screen and not maximised): nothing to put buttons on
        let (sw, sh) = (
            windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(windows::Win32::UI::WindowsAndMessaging::SM_CXSCREEN),
            windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(windows::Win32::UI::WindowsAndMessaging::SM_CYSCREEN),
        );
        if !max && fr.left <= 0 && fr.top <= 0 && fr.right >= sw && fr.bottom >= sh {
            return None;
        }
        if right - left < 40 || bottom - top < 12 {
            return None;
        }
        Some((RECT { left, top, right, bottom }, s, !max))
    }
}

/// The title bar's colour and height, from a one-pixel strip down the left edge of Windows' minimise button (clear of
/// its sign; the buttons' window is left out of captures, so this sees through it): the colour a little below the top,
/// and the height where that colour ends (the window's content starts).
unsafe fn measure(r: &RECT, s: f32) -> ([u8; 3], i32) {
    let n = (r.bottom - r.top).max(4);
    let x = r.left + (3.0 * s).round() as i32;
    let Some(px) = crate::backdrop::grab_small(RECT { left: x, top: r.top, right: x + 1, bottom: r.top + n }, 1, n) else {
        return ([32, 32, 36], n);
    };
    let at = |i: i32| -> [i32; 3] {
        let o = (i.clamp(0, n - 1) * 4) as usize;
        [px[o + 2] as i32, px[o + 1] as i32, px[o] as i32]
    };
    let c = at((n as f32 * 0.3) as i32);
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

/// Keep the patch on the window: moved (and redrawn when its size or look changed). `fresh`: look at the title bar's
/// colour again (once a move or resize has finished, or the window has just come to the front).
unsafe fn follow(fresh: bool) {
    unsafe {
        let (t, shown) = ST.with(|s| (s.borrow().target, s.borrow().shown));
        if t == 0 {
            return;
        }
        let Some((r, sc, round)) = place(hwnd(t)) else {
            hide();
            return;
        };
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let changed = ST.with(|s| {
            let s = s.borrow();
            s.w != w || s.h != h || s.round != round || (s.s - sc).abs() > 0.01
        });
        let mut r = r;
        if fresh || !shown {
            let (bg, cap) = measure(&r, sc);
            if std::env::var("ULTRON_DEV_INBOX").is_ok() {
                crate::log_line(&format!("dynamic windows: patch {},{},{},{} bar colour {:?} height {cap}", r.left, r.top, r.right, r.bottom, bg));
            }
            ST.with(|s| {
                let mut s = s.borrow_mut();
                s.bg = bg;
                s.cap = cap;
            });
        }
        let cap = ST.with(|s| s.borrow().cap);
        if cap > 0 {
            r.bottom = r.top + cap.min(r.bottom - r.top);
        }
        ST.with(|s| {
            let mut s = s.borrow_mut();
            s.x = r.left;
            s.y = r.top;
            s.w = w;
            s.h = h;
            s.s = sc;
            s.round = round;
        });
        let first = !ST.with(|s| s.borrow().shown);
        if first {
            ST.with(|s| s.borrow_mut().fade = 0);
        }
        if first || changed || fresh {
            paint();
        } else {
            moveto();
        }
        if first {
            let win = ST.with(|s| s.borrow().win);
            ST.with(|s| s.borrow_mut().shown = true);
            SetTimer(Some(hwnd(win)), FADE, 15, None);
        }
    }
}

unsafe fn moveto() {
    unsafe {
        let (win, x, y, w, h) = ST.with(|s| {
            let s = s.borrow();
            (s.win, s.x, s.y, s.w, s.h)
        });
        let main = MAIN.load(SeqCst);
        let after = if main != 0 { Some(hwnd(main)) } else { None };
        let _ = SetWindowPos(hwnd(win), after, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    }
}

unsafe fn hide() {
    unsafe {
        let win = ST.with(|s| s.borrow().win);
        if win != 0 {
            let _ = KillTimer(Some(hwnd(win)), FADE);
            let _ = KillTimer(Some(hwnd(win)), LOOK);
            let _ = ShowWindow(hwnd(win), SW_HIDE);
        }
        ST.with(|s| {
            let mut s = s.borrow_mut();
            s.shown = false;
            s.pending = false;
            s.target = 0;
            s.hover = false;
            s.hot = -1;
            s.pressed = -1;
        });
    }
}

/// The three buttons, in the window's scale: 12 px across, 8 apart, centred on the patch's height, the group 14 px from
/// its right edge; macOS's own colours (fill, rim, sign).
const FILL: [[u8; 3]; 3] = [[0xff, 0x5f, 0x57], [0xfe, 0xbc, 0x2e], [0x28, 0xc8, 0x40]];
const RIM: [[u8; 3]; 3] = [[0xe0, 0x44, 0x3e], [0xde, 0xa1, 0x23], [0x1a, 0xab, 0x29]];
const SIGN: [[u8; 3]; 3] = [[0x4d, 0x00, 0x00], [0x98, 0x57, 0x12], [0x0b, 0x65, 0x0d]];

fn layout(w: i32, h: i32, s: f32) -> ([f32; 3], f32, f32) {
    let d = 12.0 * s;
    let gap = 8.0 * s;
    let right = w as f32 - 14.0 * s;
    let x0 = right - (3.0 * d + 2.0 * gap) + d / 2.0;
    ([x0, x0 + d + gap, x0 + 2.0 * (d + gap)], h as f32 / 2.0, d / 2.0)
}

fn seg(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let t = (((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy).max(1e-6)).clamp(0.0, 1.0);
    ((px - ax - t * dx).powi(2) + (py - ay - t * dy).powi(2)).sqrt()
}

unsafe fn paint() {
    unsafe {
        let st = ST.with(|s| {
            let s = s.borrow();
            (s.win, s.x, s.y, s.w, s.h, s.s, s.round, s.bg, s.hover, s.pressed, s.fade)
        });
        let (win, x, y, w, h, s, round, bg, hover, pressed, fade) = st;
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
        let (cx, cy, r) = layout(w, h, s);
        let rc = if round { 8.0 * s } else { 0.0 };
        let sign = 3.4 * s;
        let thick = 1.15 * s;
        for j in 0..h {
            for i in 0..w {
                let (fx, fy) = (i as f32 + 0.5, j as f32 + 0.5);
                // the patch: the title bar's colour, its top-right corner rounded like the window's
                let mut a = 1.0f32;
                if rc > 0.0 && fx > w as f32 - rc && fy < rc {
                    let d = ((fx - (w as f32 - rc)).powi(2) + (fy - rc).powi(2)).sqrt();
                    a = (rc - d + 0.5).clamp(0.0, 1.0);
                }
                let mut c = [bg[0] as f32, bg[1] as f32, bg[2] as f32];
                for k in 0..3 {
                    let d = ((fx - cx[k]).powi(2) + (fy - cy).powi(2)).sqrt();
                    let cov = (r - d + 0.5).clamp(0.0, 1.0);
                    if cov <= 0.0 {
                        continue;
                    }
                    // fill, with its rim a shade darker; pressed, the whole of it darker
                    let rim = (d - (r - 1.0 * s) + 0.5).clamp(0.0, 1.0);
                    let dark = if pressed == k as i32 { 0.82 } else { 1.0 };
                    let mut dot = [0f32; 3];
                    for q in 0..3 {
                        dot[q] = (FILL[k][q] as f32 * (1.0 - rim) + RIM[k][q] as f32 * rim) * dark;
                    }
                    // its sign, while the pointer is on the three
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
                let o = ((j * w + i) * 4) as usize;
                // (premultiplied, blue-green-red-alpha)
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
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: fade, AlphaFormat: AC_SRC_ALPHA as u8 };
        if let Err(e) = UpdateLayeredWindow(hwnd(win), None, Some(&pos), Some(&size), Some(dc), Some(&zero), COLORREF(0), Some(&blend), ULW_ALPHA) {
            crate::log_line(&format!("dynamic windows: drawing the buttons failed: {e}"));
        }
        SelectObject(dc, was);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(dc);
    }
}

/// Which of the three a point (in the patch) is on, if any.
fn which(px: i32, py: i32) -> i32 {
    ST.with(|s| {
        let s = s.borrow();
        let (cx, cy, r) = layout(s.w, s.h, s.s);
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
        let (cx, cy, r) = layout(s.w, s.h, s.s);
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
                let (hov, hot) = (over_group(mx, my), which(mx, my));
                let redo = ST.with(|s| {
                    let mut s = s.borrow_mut();
                    let r = s.hover != hov || s.hot != hot;
                    s.hover = hov;
                    s.hot = hot;
                    r
                });
                if redo {
                    paint();
                }
                return LRESULT(0);
            }
            WM_MOUSELEAVE => {
                ST.with(|s| {
                    let mut s = s.borrow_mut();
                    s.tracking = false;
                    s.hover = false;
                    s.hot = -1;
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
            WM_TIMER if wp.0 == LOOK => {
                // the first look puts the buttons up; one more, a little later, catches a bar still being drawn then
                let n = ST.with(|s| {
                    let mut s = s.borrow_mut();
                    s.pending = false;
                    s.looks += 1;
                    s.looks
                });
                if n >= 2 {
                    let _ = KillTimer(Some(h), LOOK);
                } else {
                    SetTimer(Some(h), LOOK, 700, None);
                }
                follow(true);
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
                return LRESULT(0);
            }
            _ => {}
        }
        DefWindowProcW(h, m, wp, lp)
    }
}
