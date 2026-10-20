//! Live pictures of windows, for the shelf (the pointer resting on the bottom edge, with ULTRON's taskbar).
//!
//! They're Windows' own thumbnails (what Alt Tab and the taskbar show: the window as it is this moment, a video
//! playing in it too), drawn by Windows into a window of ULTRON's that has no surface of its own, so all of it is
//! see-through except the pictures, and every click goes through it to ULTRON's page underneath. The page says
//! where each picture goes, frame by frame while its cards move, so a picture goes wherever its card goes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicIsize, Ordering::SeqCst};
use std::sync::Mutex;

use windows::core::{w, BOOL};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmRegisterThumbnail, DwmUnregisterThumbnail, DwmUpdateThumbnailProperties, DWMWA_EXTENDED_FRAME_BOUNDS, DWM_THUMBNAIL_PROPERTIES,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetSystemMetrics, GetWindowRect, IsIconic, IsWindow, RegisterClassW,
    SetLayeredWindowAttributes, SetWindowDisplayAffinity, SetWindowPos, ShowWindow, TranslateMessage, HWND_TOPMOST, LWA_ALPHA, MSG, SM_CXSCREEN,
    SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE, WDA_EXCLUDEFROMCAPTURE, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

/// Where a window's picture goes, in the page's pixels, and how solid it is (0 to 1).
#[derive(serde::Deserialize)]
pub struct Item {
    pub id: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub o: f64,
}

static WIN: AtomicIsize = AtomicIsize::new(0);
/// each window shown: its thumbnail, and the part of it that's the window itself (its invisible resize border left out)
static SHOWN: Mutex<Option<HashMap<isize, (isize, RECT)>>> = Mutex::new(None);

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
            GetSystemMetrics(SM_CYSCREEN),
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
    if n != 0 && SHOWN.lock().unwrap().as_ref().is_some_and(|m| !m.is_empty()) {
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

/// Show these windows' pictures where the page says (and only these).
pub fn set(items: &[Item], scale: f64) {
    let Some(win) = window() else { return };
    let mut guard = SHOWN.lock().unwrap();
    let shown = guard.get_or_insert_with(HashMap::new);
    let first = shown.is_empty();
    let want: Vec<isize> = items.iter().filter_map(|i| i.id.parse::<isize>().ok()).collect();
    shown.retain(|src, (t, _)| {
        let keep = want.contains(src);
        if !keep {
            unsafe {
                let _ = DwmUnregisterThumbnail(*t);
            }
        }
        keep
    });
    for it in items {
        let Ok(n) = it.id.parse::<isize>() else { continue };
        let src = HWND(n as _);
        if !shown.contains_key(&n) {
            if !unsafe { IsWindow(Some(src)) }.as_bool() {
                continue;
            }
            match unsafe { DwmRegisterThumbnail(win, src) } {
                Ok(t) => {
                    shown.insert(n, (t, seen_part(src)));
                }
                Err(_) => continue,
            }
        }
        let (t, part) = shown[&n];
        let r = RECT {
            left: (it.x * scale).round() as i32,
            top: (it.y * scale).round() as i32,
            right: ((it.x + it.w) * scale).round() as i32,
            bottom: ((it.y + it.h) * scale).round() as i32,
        };
        let cropped = part.right > part.left && part.bottom > part.top;
        let p = DWM_THUMBNAIL_PROPERTIES {
            dwFlags: 0x1 | 0x4 | 0x8 | 0x10 | if cropped { 0x2 } else { 0 },
            rcDestination: r,
            rcSource: part,
            opacity: (it.o.clamp(0.0, 1.0) * 255.0).round() as u8,
            fVisible: BOOL::from(it.o > 0.003 && r.right > r.left && r.bottom > r.top),
            fSourceClientAreaOnly: BOOL(0),
        };
        unsafe {
            let _ = DwmUpdateThumbnailProperties(t, &p);
        }
    }
    if first && !shown.is_empty() {
        unsafe {
            let _ = ShowWindow(win, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(win, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }
}

/// No more pictures.
pub fn clear() {
    if let Some(m) = SHOWN.lock().unwrap().as_mut() {
        for (_, (t, _)) in m.drain() {
            unsafe {
                let _ = DwmUnregisterThumbnail(t);
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
