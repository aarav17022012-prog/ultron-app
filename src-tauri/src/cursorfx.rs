//! Motion blur for the mouse pointer (Settings, Look & feel, "Pointer motion blur").
//!
//! While the pointer moves quickly, a soft streak of the pointer itself trails behind it: copies of the very cursor
//! Windows is showing (an arrow, a hand, a text beam...), fainter the further back, stamped along the way it has just
//! come, one picture per frame Windows draws. Stopping, the streak melts into the pointer. It's a see-through,
//! click-through window of ULTRON's, just under the real pointer (which Windows always draws on top).

use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::time::Duration;

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateSolidBrush, DeleteDC, DeleteObject, FillRect, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER,
    BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DrawIconEx, GetCursorInfo, GetIconInfo, GetSystemMetrics, RegisterClassW, SetWindowPos, ShowWindow, UpdateLayeredWindow,
    CURSORINFO, CURSOR_SHOWING, DI_NORMAL, HCURSOR, HICON, HWND_TOPMOST, ICONINFO, SM_CXCURSOR, SM_CYCURSOR, SWP_NOACTIVATE, SW_HIDE, SW_SHOWNOACTIVATE,
    ULW_ALPHA, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

static ON: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);

pub fn set(on: bool) {
    ON.store(on, SeqCst);
    if on && !STARTED.swap(true, SeqCst) {
        std::thread::spawn(|| unsafe { run() });
    }
}

unsafe extern "system" fn proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(h, m, wp, lp) }
}

/// The pointer's own picture, premultiplied BGRA (w x h), and its hot spot: drawn once on black and once on white,
/// the difference gives how solid each pixel is (so it works for every kind of cursor, old or new).
struct Shape {
    of: isize,
    w: i32,
    h: i32,
    hot: (i32, i32),
    px: Vec<[f32; 4]>,
}

unsafe fn dib(w: i32, h: i32) -> Option<(HDC, HBITMAP, HBITMAP, *mut u8)> {
    unsafe {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -h, biPlanes: 1, biBitCount: 32, ..Default::default() },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bmp = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        if bits.is_null() {
            let _ = DeleteObject(bmp.into());
            return None;
        }
        let dc = CreateCompatibleDC(None);
        let was = SelectObject(dc, bmp.into());
        Some((dc, bmp, HBITMAP(was.0), bits as *mut u8))
    }
}

unsafe fn shape(c: HCURSOR) -> Option<Shape> {
    unsafe {
        let (w, h) = (GetSystemMetrics(SM_CXCURSOR).max(16), GetSystemMetrics(SM_CYCURSOR).max(16));
        let mut ii = ICONINFO::default();
        GetIconInfo(HICON(c.0), &mut ii).ok()?;
        let hot = (ii.xHotspot as i32, ii.yHotspot as i32);
        if !ii.hbmMask.is_invalid() {
            let _ = DeleteObject(ii.hbmMask.into());
        }
        if !ii.hbmColor.is_invalid() {
            let _ = DeleteObject(ii.hbmColor.into());
        }
        let mut take = |bg: u32| -> Option<Vec<u8>> {
            let (dc, bmp, was, bits) = dib(w, h)?;
            let brush = CreateSolidBrush(COLORREF(bg));
            let r = windows::Win32::Foundation::RECT { left: 0, top: 0, right: w, bottom: h };
            FillRect(dc, &r, brush);
            let _ = DeleteObject(brush.into());
            let _ = DrawIconEx(dc, 0, 0, HICON(c.0), w, h, 0, None, DI_NORMAL);
            let out = std::slice::from_raw_parts(bits, (w * h * 4) as usize).to_vec();
            SelectObject(dc, was.into());
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(dc);
            Some(out)
        };
        let (black, white) = (take(0x000000)?, take(0xFFFFFF)?);
        let px = (0..(w * h) as usize)
            .map(|i| {
                let o = i * 4;
                // solidity from how much white showed through; colour (premultiplied) is what was drawn over black
                let a = (255.0 - (white[o + 1] as f32 - black[o + 1] as f32)).clamp(0.0, 255.0) / 255.0;
                [black[o] as f32 / 255.0, black[o + 1] as f32 / 255.0, black[o + 2] as f32 / 255.0, a]
            })
            .collect();
        Some(Shape { of: c.0 as isize, w, h, hot, px })
    }
}

unsafe fn run() {
    unsafe {
        let inst: Option<windows::Win32::Foundation::HINSTANCE> = GetModuleHandleW(None).ok().map(|m| m.into());
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(proc), hInstance: inst.unwrap_or_default(), lpszClassName: w!("UltronPointerBlur"), ..Default::default() });
        let Ok(win) = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("UltronPointerBlur"),
            w!(""),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            inst,
            None,
        ) else {
            STARTED.store(false, SeqCst);
            return;
        };
        let mut shp: Option<Shape> = None;
        // the last few places the pointer was, newest last, with how strong the streak still is
        let mut trail: Vec<(f32, f32)> = Vec::new();
        let mut strength = 0.0f32;
        let mut shown = false;
        loop {
            // (its window answers Windows' messages: one that never did would leave anything that sends to every
            // window, a setting or wallpaper change, waiting on it, and that froze things)
            let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();
            while windows::Win32::UI::WindowsAndMessaging::PeekMessageW(&mut msg, None, 0, 0, windows::Win32::UI::WindowsAndMessaging::PM_REMOVE).as_bool() {
                let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
                windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(&msg);
            }
            if !ON.load(SeqCst) {
                if shown {
                    let _ = ShowWindow(win, SW_HIDE);
                    shown = false;
                }
                trail.clear();
                std::thread::sleep(Duration::from_millis(60));
                continue;
            }
            if DwmFlush().is_err() {
                std::thread::sleep(Duration::from_millis(7));
            }
            let mut ci = CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
            if GetCursorInfo(&mut ci).is_err() || ci.flags.0 & CURSOR_SHOWING.0 == 0 || ci.hCursor.is_invalid() {
                continue;
            }
            if shp.as_ref().map(|s| s.of) != Some(ci.hCursor.0 as isize) {
                shp = shape(ci.hCursor);
                if shp.is_none() {
                    crate::log_line("pointer blur: couldn't read the pointer's picture");
                }
            }
            let Some(s) = shp.as_ref() else { continue };
            let p = (ci.ptScreenPos.x as f32, ci.ptScreenPos.y as f32);
            // (a jump, not a movement: the pointer put somewhere else at once draws no streak)
            if let Some(q) = trail.last() {
                if ((p.0 - q.0).powi(2) + (p.1 - q.1).powi(2)).sqrt() > 360.0 {
                    trail.clear();
                    strength = 0.0;
                }
            }
            trail.push(p);
            if trail.len() > 4 {
                trail.remove(0);
            }
            // how far it went over the last few frames: the streak grows with speed, and melts when it stops
            let span = trail.windows(2).map(|w| ((w[1].0 - w[0].0).powi(2) + (w[1].1 - w[0].1).powi(2)).sqrt()).sum::<f32>();
            strength = if span > 14.0 { (strength + 0.5).min(1.0) } else { strength * 0.62 };
            if strength < 0.04 || trail.len() < 2 {
                if shown {
                    let _ = ShowWindow(win, SW_HIDE);
                    shown = false;
                }
                continue;
            }
            // stamps from the oldest place up to (not quite) the pointer itself, fainter the further back
            let (a, b) = (trail[0], *trail.last().unwrap());
            let n = ((span / 7.0) as usize).clamp(4, 14);
            let pts: Vec<(f32, f32, f32)> = (0..n)
                .map(|i| {
                    let t = i as f32 / n as f32; // (0 = oldest, never 1: the real pointer is there)
                    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t, (0.06 + 0.3 * t * t) * strength)
                })
                .collect();
            let (minx, miny) = pts.iter().fold((f32::MAX, f32::MAX), |m, q| (m.0.min(q.0), m.1.min(q.1)));
            let (maxx, maxy) = pts.iter().fold((f32::MIN, f32::MIN), |m, q| (m.0.max(q.0), m.1.max(q.1)));
            let (ox, oy) = ((minx as i32) - s.hot.0 - 1, (miny as i32) - s.hot.1 - 1);
            let (ww, hh) = ((maxx - minx) as i32 + s.w + 3, (maxy - miny) as i32 + s.h + 3);
            if ww > 2400 || hh > 2400 {
                continue;
            }
            let Some((dc, bmp, was, bits)) = dib(ww, hh) else { continue };
            let px = std::slice::from_raw_parts_mut(bits, (ww * hh * 4) as usize);
            let mut acc = vec![[0.0f32; 4]; (ww * hh) as usize];
            for &(x, y, k) in &pts {
                let (bx, by) = ((x as i32) - s.hot.0 - ox, (y as i32) - s.hot.1 - oy);
                for j in 0..s.h {
                    for i in 0..s.w {
                        let c = s.px[(j * s.w + i) as usize];
                        if c[3] <= 0.0 {
                            continue;
                        }
                        let (tx, ty) = (bx + i, by + j);
                        if tx < 0 || ty < 0 || tx >= ww || ty >= hh {
                            continue;
                        }
                        let d = &mut acc[(ty * ww + tx) as usize];
                        // (premultiplied "over": the newer stamp over what's there)
                        let al = c[3] * k;
                        for ch in 0..3 {
                            d[ch] = c[ch] * k + d[ch] * (1.0 - al);
                        }
                        d[3] = al + d[3] * (1.0 - al);
                    }
                }
            }
            for (i, d) in acc.iter().enumerate() {
                let o = i * 4;
                px[o] = (d[0] * 255.0) as u8;
                px[o + 1] = (d[1] * 255.0) as u8;
                px[o + 2] = (d[2] * 255.0) as u8;
                px[o + 3] = (d[3] * 255.0) as u8;
            }
            let (pos, size, zero) = (POINT { x: ox, y: oy }, SIZE { cx: ww, cy: hh }, POINT::default());
            let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
            let _ = UpdateLayeredWindow(win, None, Some(&pos), Some(&size), Some(dc), Some(&zero), COLORREF(0), Some(&blend), ULW_ALPHA);
            SelectObject(dc, was.into());
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(dc);
            if !shown {
                static TOLD: std::sync::Once = std::sync::Once::new();
                TOLD.call_once(|| crate::log_line(&format!("pointer blur: drawing ({}x{} streak, {} stamps)", ww, hh, n)));
                let _ = ShowWindow(win, SW_SHOWNOACTIVATE);
                let _ = SetWindowPos(win, Some(HWND_TOPMOST), 0, 0, 0, 0, windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE | windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE | SWP_NOACTIVATE);
                shown = true;
            }
        }
    }
}
