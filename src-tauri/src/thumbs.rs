//! Live pictures of windows, for the shelf (the pointer resting on the bottom edge, with ULTRON's taskbar).
//!
//! They're Windows' own thumbnails (what Alt Tab and the taskbar show: the window as it is this moment, a video
//! playing in it too), drawn by Windows into a window of ULTRON's that has no surface of its own, so all of it is
//! see-through except the pictures, and every click goes through it to ULTRON's page underneath.
//!
//! They move by themselves: the page says where each one goes, how long it takes, after what delay and with what
//! easing (the same as its own card's), and a thread here moves them one step per frame Windows draws (DwmFlush),
//! so they glide at the screen's own rate however busy the page is.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering::SeqCst};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::core::{w, BOOL};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmFlush, DwmGetWindowAttribute, DwmRegisterThumbnail, DwmUnregisterThumbnail, DwmUpdateThumbnailProperties, DWMWA_EXTENDED_FRAME_BOUNDS,
    DWM_THUMBNAIL_PROPERTIES,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetSystemMetrics, GetWindow, GetWindowRect, IsIconic, IsWindow, RegisterClassW,
    SetLayeredWindowAttributes, SetWindowDisplayAffinity, SetWindowPos, ShowWindow, TranslateMessage, GW_HWNDPREV, HWND_TOPMOST, LWA_ALPHA, MSG,
    SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE, WDA_EXCLUDEFROMCAPTURE, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

/// Where a window's picture goes: `to` (and, given, where it starts: `from`) as x, y, width, height in the page's
/// pixels and how solid it is (0 to 1); over `ms` after `delay` ms, eased by the cubic-bezier `ease`; its
/// solidity over `oms` (default `ms`), evenly.
#[derive(serde::Deserialize)]
pub struct Go {
    pub id: String,
    pub to: [f64; 5],
    pub from: Option<[f64; 5]>,
    pub ms: f64,
    #[serde(default)]
    pub delay: f64,
    pub oms: Option<f64>,
    pub ease: Option<[f64; 4]>,
}

struct Anim {
    from: [f64; 5],
    to: [f64; 5],
    start: Instant,
    ms: f64,
    oms: f64,
    ease: [f64; 4],
}

struct Pic {
    thumb: isize,
    part: RECT,
    /// where it is now: x, y, width, height in screen pixels, and how solid
    now: [f64; 5],
    anim: Option<Anim>,
}

static WIN: AtomicIsize = AtomicIsize::new(0);
static PICS: Mutex<Option<HashMap<isize, Pic>>> = Mutex::new(None);
static MOVING: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(h, m, wp, lp) }
}

/// The see-through window, made the first time it's needed, on a thread of its own.
fn window() -> Option<HWND> {
    let n = WIN.load(SeqCst);
    if n != 0 {
        return Some(HWND(n as _));
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || unsafe {
        let inst: Option<windows::Win32::Foundation::HINSTANCE> = GetModuleHandleW(None).ok().map(|m| m.into());
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(proc), hInstance: inst.unwrap_or_default(), lpszClassName: w!("UltronThumbs"), ..Default::default() });
        let h = CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("UltronThumbs"),
            w!(""),
            WS_POPUP,
            0,
            0,
            GetSystemMetrics(SM_CXSCREEN),
            // (2 px short of the screen: a screen-sized window on top is taken by Windows for a full-screen app)
            GetSystemMetrics(SM_CYSCREEN) - 2,
            None,
            None,
            inst,
            None,
        )
        .ok();
        if let Some(h) = h {
            let _ = SetLayeredWindowAttributes(h, COLORREF(0), 255, LWA_ALPHA);
            // (left out of screen captures, as ULTRON's glass is: its own backdrop mustn't show the pictures)
            if std::env::var("ULTRON_DEV_INBOX").is_err() {
                let _ = SetWindowDisplayAffinity(h, WDA_EXCLUDEFROMCAPTURE);
            }
            WIN.store(h.0 as isize, SeqCst);
        }
        let _ = tx.send(h.map(|h| h.0 as isize));
        let mut m = MSG::default();
        while GetMessageW(&mut m, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&m);
            DispatchMessageW(&m);
        }
    });
    rx.recv().ok().flatten().map(|n| HWND(n as _))
}

/// Keep the pictures above ULTRON's page (which takes the top of the always-on-top windows now and then).
pub fn raise() {
    let n = WIN.load(SeqCst);
    if n != 0 && PICS.lock().unwrap().as_ref().is_some_and(|m| !m.is_empty()) {
        unsafe {
            let _ = SetWindowPos(HWND(n as _), Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }
}

/// The part of a window that's the window itself, in its own pixels: Windows' thumbnail includes the invisible
/// border a window keeps round itself for resizing.
fn seen_part(src: HWND) -> RECT {
    unsafe {
        let (mut out, mut seen) = (RECT::default(), RECT::default());
        if IsIconic(src).as_bool()
            || GetWindowRect(src, &mut out).is_err()
            || DwmGetWindowAttribute(src, DWMWA_EXTENDED_FRAME_BOUNDS, &mut seen as *mut _ as _, std::mem::size_of::<RECT>() as u32).is_err()
        {
            return RECT::default();
        }
        RECT { left: seen.left - out.left, top: seen.top - out.top, right: seen.right - out.left, bottom: seen.bottom - out.top }
    }
}

fn show(p: &Pic) {
    let v = p.now;
    let r = RECT { left: v[0].round() as i32, top: v[1].round() as i32, right: (v[0] + v[2]).round() as i32, bottom: (v[1] + v[3]).round() as i32 };
    let cropped = p.part.right > p.part.left && p.part.bottom > p.part.top;
    let props = DWM_THUMBNAIL_PROPERTIES {
        dwFlags: 0x1 | 0x4 | 0x8 | 0x10 | if cropped { 0x2 } else { 0 },
        rcDestination: r,
        rcSource: p.part,
        opacity: (v[4].clamp(0.0, 1.0) * 255.0).round() as u8,
        fVisible: BOOL::from(v[4] > 0.003 && r.right > r.left && r.bottom > r.top),
        fSourceClientAreaOnly: BOOL(0),
    };
    unsafe {
        let _ = DwmUpdateThumbnailProperties(p.thumb, &props);
    }
}

/// CSS's cubic-bezier(x1, y1, x2, y2) at `t` (0 to 1): the curve's x solved for t, its y given back.
fn ease(e: [f64; 4], t: f64) -> f64 {
    let [x1, y1, x2, y2] = e;
    let bx = |s: f64| 3.0 * (1.0 - s) * (1.0 - s) * s * x1 + 3.0 * (1.0 - s) * s * s * x2 + s * s * s;
    let by = |s: f64| 3.0 * (1.0 - s) * (1.0 - s) * s * y1 + 3.0 * (1.0 - s) * s * s * y2 + s * s * s;
    let (mut lo, mut hi, mut s) = (0.0, 1.0, t);
    for _ in 0..24 {
        let x = bx(s);
        if (x - t).abs() < 1e-5 {
            break;
        }
        if x < t { lo = s } else { hi = s }
        s = (lo + hi) / 2.0;
    }
    by(s)
}

/// One step for every picture that's moving; false once none is.
fn step() -> bool {
    let mut guard = PICS.lock().unwrap();
    let Some(pics) = guard.as_mut() else { return false };
    let now = Instant::now();
    let mut moving = false;
    for p in pics.values_mut() {
        let Some(a) = &p.anim else { continue };
        if now < a.start {
            moving = true;
            continue;
        }
        let ms = now.duration_since(a.start).as_secs_f64() * 1000.0;
        let k = if a.ms > 0.0 { ease(a.ease, (ms / a.ms).min(1.0)) } else { 1.0 };
        let ko = if a.oms > 0.0 { (ms / a.oms).min(1.0) } else { 1.0 };
        for i in 0..4 {
            p.now[i] = a.from[i] + (a.to[i] - a.from[i]) * k;
        }
        p.now[4] = a.from[4] + (a.to[4] - a.from[4]) * ko;
        let done = ms >= a.ms && ms >= a.oms;
        show(p);
        if done {
            p.anim = None;
        } else {
            moving = true;
        }
    }
    moving
}

/// Send these windows' pictures where the page says. `only`: any other picture shown goes.
pub fn go(items: &[Go], scale: f64, only: bool) {
    let Some(win) = window() else { return };
    let first;
    {
        let mut guard = PICS.lock().unwrap();
        let pics = guard.get_or_insert_with(HashMap::new);
        first = pics.is_empty();
        if only {
            let want: Vec<isize> = items.iter().filter_map(|i| i.id.parse::<isize>().ok()).collect();
            pics.retain(|src, p| {
                let keep = want.contains(src);
                if !keep {
                    unsafe {
                        let _ = DwmUnregisterThumbnail(p.thumb);
                    }
                }
                keep
            });
        }
        let px = |v: [f64; 5]| [v[0] * scale, v[1] * scale, v[2] * scale, v[3] * scale, v[4]];
        let start = Instant::now();
        for it in items {
            let Ok(n) = it.id.parse::<isize>() else { continue };
            let src = HWND(n as _);
            if !pics.contains_key(&n) {
                if !unsafe { IsWindow(Some(src)) }.as_bool() {
                    continue;
                }
                let Ok(thumb) = (unsafe { DwmRegisterThumbnail(win, src) }) else { continue };
                let at = px(it.from.unwrap_or([it.to[0], it.to[1], it.to[2], it.to[3], 0.0]));
                pics.insert(n, Pic { thumb, part: seen_part(src), now: at, anim: None });
            }
            let p = pics.get_mut(&n).unwrap();
            if let Some(f) = it.from {
                p.now = px(f);
            }
            p.anim = Some(Anim {
                from: p.now,
                to: px(it.to),
                start: start + Duration::from_secs_f64(it.delay.max(0.0) / 1000.0),
                ms: it.ms.max(0.0),
                oms: it.oms.unwrap_or(it.ms).max(0.0),
                ease: it.ease.unwrap_or([0.22, 1.0, 0.36, 1.0]),
            });
            show(p);
        }
        if !pics.is_empty() {
            unsafe {
                if first {
                    let _ = ShowWindow(win, SW_SHOWNOACTIVATE);
                }
                // (on top of ULTRON's page, whatever came over it since: its cards would hide the pictures)
                let above = GetWindow(win, GW_HWNDPREV).map(|h| !h.is_invalid()).unwrap_or(false);
                if first || above {
                    let _ = SetWindowPos(win, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                }
            }
        }
    }
    // the mover: one step each time Windows draws a frame, while anything moves
    if !MOVING.swap(true, SeqCst) {
        std::thread::spawn(|| {
            loop {
                let more = step();
                if !more {
                    MOVING.store(false, SeqCst);
                    // (something may have been set moving just as this one stopped)
                    if PICS.lock().unwrap().as_ref().is_some_and(|m| m.values().any(|p| p.anim.is_some())) && !MOVING.swap(true, SeqCst) {
                        continue;
                    }
                    break;
                }
                if unsafe { DwmFlush() }.is_err() {
                    std::thread::sleep(Duration::from_millis(7));
                }
            }
        });
    }
}

/// No more pictures.
pub fn clear() {
    if let Some(m) = PICS.lock().unwrap().as_mut() {
        for (_, p) in m.drain() {
            unsafe {
                let _ = DwmUnregisterThumbnail(p.thumb);
            }
        }
    }
    let n = WIN.load(SeqCst);
    if n != 0 {
        unsafe {
            let _ = ShowWindow(HWND(n as _), SW_HIDE);
        }
    }
}
