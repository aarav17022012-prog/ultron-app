//! The new wallpaper arriving. Windows swaps wallpapers in one cut, so ULTRON draws the arrival itself:
//! the new picture pours down from under the bar, soft-edged, over the old one, behind every window.
//! Once it covers the screen Windows is told to change the wallpaper underneath, and a moment later
//! ULTRON's copy fades away over the identical real one.
//!
//! The picture is drawn in a window of ULTRON's own that sits under every other window, just above the
//! desktop. (So the desktop's icons are covered while it plays, about a second and a half, and come
//! back as it fades. Inside the desktop, behind the icons, would be the place for it, and the code for
//! that is here, ways 1 to 3: but Windows 11 from build 26300 doesn't draw a window put there, though
//! the window reports itself visible. Measured with a magenta test picture and ULTRON's own look at
//! the screen behind its bar, 14 Oct 2026.)

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EndPaint, EnumDisplayMonitors, InvalidateRect, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER,
    BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, HMONITOR, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows, FindWindowExW, FindWindowW, GetClassNameW, GetClientRect, GetParent, GetWindow, GetWindowLongPtrW, GetWindowRect,
    IsWindowVisible, PeekMessageW, SetLayeredWindowAttributes, SetWindowLongPtrW, GWLP_USERDATA, GW_HWNDNEXT, GW_HWNDPREV, HWND_BOTTOM, LWA_ALPHA, WM_PAINT, RegisterClassW, SendMessageTimeoutW, SetParent, SetWindowPos,
    TranslateMessage, UpdateLayeredWindow, UpdateLayeredWindowIndirect, HWND_TOP, MSG, PM_REMOVE, SMTO_NORMAL, SWP_NOACTIVATE, SWP_SHOWWINDOW, ULW_ALPHA,
    UPDATELAYEREDWINDOWINFO, WNDCLASSW, WS_CHILD, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};

/// How the picture's window is made (a developer's knob, set through the rehearsal command):
/// 0 as shipped (a window of its own just above the desktop); inside the desktop, behind the icons: 1 made on its
/// own and moved in, per-pixel; 2 the same at one opacity for all of it; 3 born there, one opacity.
pub static HOW: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// One screen's worth: the window, the picture to show (BGRA, scaled to fill the screen), and the
/// surface the window is drawn from (the picture with the pour's see-through edge worked in).
struct Layer {
    hwnd: HWND,
    dc: HDC,
    bmp: HBITMAP,
    was: HGDIOBJ,
    bits: *mut u8,
    src: Vec<u8>,
    w: i32,
    h: i32,
    full: i32, // rows from the top that already show the picture whole
    rects: bool, // this Windows takes "only these rows changed" (else the whole surface is handed over each time)
    whole: bool, // shown at one opacity for the whole picture (it dissolves in) instead of row by row (it pours)
}

unsafe extern "system" fn proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        if m == WM_PAINT {
            // (only the one-opacity kind is painted this way: its picture is in the surface whose handle it carries)
            let dc = HDC(GetWindowLongPtrW(h, GWLP_USERDATA) as _);
            let mut ps = PAINTSTRUCT::default();
            let to = BeginPaint(h, &mut ps);
            if !dc.is_invalid() {
                let mut r = RECT::default();
                let _ = GetClientRect(h, &mut r);
                let _ = BitBlt(to, 0, 0, r.right, r.bottom, Some(dc), 0, 0, SRCCOPY);
            }
            let _ = EndPaint(h, &ps);
            return LRESULT(0);
        }
        DefWindowProcW(h, m, w, l)
    }
}

/// Where a window ended up, in words (for the log while this is being made to work on a new Windows).
unsafe fn whereabouts(h: HWND) -> String {
    unsafe {
        let name = |w: windows::core::Result<HWND>| -> String {
            match w {
                Ok(w) if !w.is_invalid() => {
                    let mut b = [0u16; 64];
                    let n = GetClassNameW(w, &mut b);
                    String::from_utf16_lossy(&b[..n.max(0) as usize])
                }
                _ => "nothing".into(),
            }
        };
        let mut r = RECT::default();
        let _ = GetWindowRect(h, &mut r);
        format!("visible {}, at {},{} {}x{}, inside {}, above it {}, below it {}", IsWindowVisible(h).as_bool(), r.left, r.top, r.right - r.left, r.bottom - r.top, name(GetParent(h)), name(GetWindow(h, GW_HWNDPREV)), name(GetWindow(h, GW_HWNDNEXT)))
    }
}

unsafe extern "system" fn each_window(h: HWND, l: LPARAM) -> BOOL {
    unsafe {
        if FindWindowExW(Some(h), None, w!("SHELLDLL_DefView"), PCWSTR::null()).is_ok() {
            if let Ok(under) = FindWindowExW(None, Some(h), w!("WorkerW"), PCWSTR::null()) {
                *(l.0 as *mut isize) = under.0 as isize;
            }
            return BOOL(0);
        }
        BOOL(1)
    }
}

unsafe extern "system" fn each_screen(_: HMONITOR, _: HDC, r: *mut RECT, l: LPARAM) -> BOOL {
    unsafe {
        (*(l.0 as *mut Vec<RECT>)).push(*r);
        BOOL(1)
    }
}

/// Where a window has to go to sit above the wallpaper and below the icons: (the window to live
/// in, the sibling to sit right behind, which arrangement this Windows has).
unsafe fn spot() -> Option<(HWND, Option<HWND>, &'static str)> {
    unsafe {
        let progman = FindWindowW(w!("Progman"), PCWSTR::null()).ok()?;
        // (the shell's own request to split the picture from the icons: it's what it does before fading a wallpaper)
        SendMessageTimeoutW(progman, 0x052C, WPARAM(0xD), LPARAM(1), SMTO_NORMAL, 1000, None);
        let icons = FindWindowExW(Some(progman), None, w!("SHELLDLL_DefView"), PCWSTR::null()).ok();
        let under = FindWindowExW(Some(progman), None, w!("WorkerW"), PCWSTR::null()).ok();
        if let (Some(i), Some(_)) = (icons, under) {
            return Some((progman, Some(i), "icons and picture inside Progman")); // Windows 11 24H2 and later
        }
        // before that: the icons are in a window of their own, and the picture's layer is the WorkerW after it
        let mut found = 0isize;
        let _ = EnumWindows(Some(each_window), LPARAM(&mut found as *mut isize as isize));
        (found != 0).then(|| (HWND(found as _), None, "a WorkerW behind the icons"))
    }
}

unsafe fn pump() {
    unsafe {
        let mut m = MSG::default();
        while PeekMessageW(&mut m, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&m);
            DispatchMessageW(&m);
        }
    }
}

fn blend(alpha: u8) -> BLENDFUNCTION {
    BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: alpha, AlphaFormat: AC_SRC_ALPHA as u8 }
}

unsafe fn build(picture: &Path) -> Vec<Layer> {
    unsafe {
        let way = HOW.load(std::sync::atomic::Ordering::SeqCst);
        let (home, behind, how) = if matches!(way, 1 | 2 | 3) {
            let Some(found) = spot() else {
                crate::log_line("wallpaper: the desktop's picture layer wasn't found, so the change isn't animated");
                return Vec::new();
            };
            found
        } else {
            (HWND::default(), None, "a window of its own, under every other")
        };
        let Ok(img) = image::open(picture) else { return Vec::new() };
        let mut screens: Vec<RECT> = Vec::new();
        let _ = EnumDisplayMonitors(None, None, Some(each_screen), LPARAM(&mut screens as *mut Vec<RECT> as isize));
        let mut at = RECT::default();
        if !home.is_invalid() {
            let _ = GetWindowRect(home, &mut at);
        }
        static CLASS: std::sync::Once = std::sync::Once::new();
        let inst: Option<windows::Win32::Foundation::HINSTANCE> = GetModuleHandleW(None).ok().map(|m| m.into());
        CLASS.call_once(|| {
            RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(proc), hInstance: inst.unwrap_or_default(), lpszClassName: w!("UltronVeil"), ..Default::default() });
        });
        let mut layers = Vec::new();
        for s in screens.iter().take(4) {
            let (w, h) = (s.right - s.left, s.bottom - s.top);
            if w < 64 || h < 64 {
                continue;
            }
            let mut src = img.resize_to_fill(w as u32, h as u32, image::imageops::FilterType::Triangle).to_rgba8().into_raw();
            for p in src.chunks_exact_mut(4) {
                p.swap(0, 2); // (to the blue-green-red order Windows draws from)
                p[3] = 255;
            }
            let info = BITMAPINFO { bmiHeader: BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -h, biPlanes: 1, biBitCount: 32, ..Default::default() }, ..Default::default() };
            let mut bits = std::ptr::null_mut();
            let Ok(bmp) = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) else { continue };
            if bits.is_null() {
                continue;
            }
            std::ptr::write_bytes(bits as *mut u8, 0, (w * h * 4) as usize); // (nothing of it shows yet)
            let dc = CreateCompatibleDC(None);
            let was = SelectObject(dc, bmp.into());
            let how = way;
            let make = |child: bool, whole: bool, inside: bool| -> Result<HWND, String> {
                let (style, x, y, parent) = if child { (WS_CHILD, s.left - at.left, s.top - at.top, Some(home)) } else { (WS_POPUP, s.left, s.top, None) };
                let hwnd = CreateWindowExW(WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW, w!("UltronVeil"), w!(""), style, x, y, w, h, parent, None, inst, None).map_err(|e| format!("making it: {e}"))?;
                let (size, zero) = (SIZE { cx: w, cy: h }, POINT::default());
                let placed = (|| -> Result<(), String> {
                    if whole {
                        std::ptr::copy_nonoverlapping(src.as_ptr(), bits as *mut u8, src.len());
                        SetWindowLongPtrW(hwnd, GWLP_USERDATA, dc.0 as isize);
                        SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA).map_err(|e| format!("its opacity: {e}"))?;
                    } else {
                        UpdateLayeredWindow(hwnd, None, None, Some(&size), Some(dc), Some(&zero), COLORREF(0), Some(&blend(255)), ULW_ALPHA).map_err(|e| format!("its first picture: {e}"))?;
                    }
                    if !child && inside {
                        SetParent(hwnd, Some(home)).map_err(|e| format!("moving it in: {e}"))?;
                    }
                    if inside {
                        SetWindowPos(hwnd, Some(behind.unwrap_or(HWND_TOP)), s.left - at.left, s.top - at.top, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW).map_err(|e| format!("placing it: {e}"))
                    } else {
                        // (not inside: at the very bottom of the windows, which is just above the desktop)
                        SetWindowPos(hwnd, Some(HWND_BOTTOM), s.left, s.top, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW).map_err(|e| format!("placing it: {e}"))
                    }
                })();
                if let Err(e) = placed {
                    let _ = DestroyWindow(hwnd);
                    return Err(e);
                }
                Ok(hwnd)
            };
            let made = match how {
                1 => make(false, false, true).map(|h| (h, false)),
                2 => make(false, true, true).map(|h| (h, true)),
                3 => make(true, true, true).map(|h| (h, true)),
                _ => make(false, false, false).map(|h| (h, false)),
            };
            match made {
                Ok((hwnd, whole)) => {
                    if how != 0 {
                        crate::log_line(&format!("wallpaper: way {how}: {}", whereabouts(hwnd)));
                    }
                    layers.push(Layer { hwnd, dc, bmp, was, bits: bits as *mut u8, src, w, h, full: 0, rects: true, whole })
                }
                Err(why) => {
                    crate::log_line(&format!("wallpaper: no picture could be put on the desktop's layer ({how}; {why}), so the change isn't animated"));
                    SelectObject(dc, was);
                    let _ = DeleteObject(bmp.into());
                    let _ = DeleteDC(dc);
                }
            }
        }
        if !layers.is_empty() {
            crate::log_line(&format!("wallpaper: drawing the change on {} screen(s) ({how})", layers.len()));
        }
        layers
    }
}

/// The pour: the picture comes down from the top behind a soft edge a third of the screen tall.
/// Each frame only the rows the edge has passed over are worked on and handed to Windows.
unsafe fn pour(layers: &mut [Layer], ms: u64) -> (u32, Duration) {
    unsafe {
        let (t0, mut frames) = (Instant::now(), 0u32);
        loop {
            let t = (t0.elapsed().as_secs_f32() / (ms as f32 / 1000.0)).min(1.0);
            let p = if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }; // (starts gently, lands gently)
            for l in layers.iter_mut() {
                let band = l.h as f32 * 0.36;
                let front = p * (l.h as f32 + band);
                let (from, upto, full) = (l.full, (front.ceil() as i32).clamp(0, l.h), ((front - band).floor() as i32).clamp(0, l.h));
                if upto <= from {
                    continue;
                }
                let stride = (l.w * 4) as usize;
                for y in from..upto {
                    let k = ((front - y as f32) / band).clamp(0.0, 1.0);
                    let a = (k * k * (3.0 - 2.0 * k) * 255.0).round() as u32;
                    let s = &l.src[y as usize * stride..(y as usize + 1) * stride];
                    let d = std::slice::from_raw_parts_mut(l.bits.add(y as usize * stride), stride);
                    if a >= 255 {
                        d.copy_from_slice(s);
                    } else {
                        for (o, i) in d.chunks_exact_mut(4).zip(s.chunks_exact(4)) {
                            o[0] = ((i[0] as u32 * a + 127) / 255) as u8;
                            o[1] = ((i[1] as u32 * a + 127) / 255) as u8;
                            o[2] = ((i[2] as u32 * a + 127) / 255) as u8;
                            o[3] = a as u8;
                        }
                    }
                }
                l.full = full;
                let (size, zero, b, dirty) = (SIZE { cx: l.w, cy: l.h }, POINT::default(), blend(255), RECT { left: 0, top: from, right: l.w, bottom: upto });
                let info = UPDATELAYEREDWINDOWINFO { cbSize: std::mem::size_of::<UPDATELAYEREDWINDOWINFO>() as u32, hdcDst: HDC::default(), pptDst: std::ptr::null(), psize: &size, hdcSrc: l.dc, pptSrc: &zero, crKey: COLORREF(0), pblend: &b, dwFlags: ULW_ALPHA, prcDirty: &dirty };
                if !l.rects || !UpdateLayeredWindowIndirect(l.hwnd, &info).as_bool() {
                    l.rects = false;
                    let _ = UpdateLayeredWindow(l.hwnd, None, None, Some(&size), Some(l.dc), Some(&zero), COLORREF(0), Some(&b), ULW_ALPHA);
                }
            }
            frames += 1;
            pump();
            if t >= 1.0 {
                break;
            }
            if DwmFlush().is_err() {
                std::thread::sleep(Duration::from_millis(7)); // (in step with the screen where that can be had)
            }
        }
        (frames, t0.elapsed())
    }
}

/// The plainer arrival, for where the picture can only be shown at one opacity: it dissolves in (or out).
unsafe fn dissolve(layers: &[Layer], ms: u64, up: bool) {
    unsafe {
        let t0 = Instant::now();
        loop {
            let t = (t0.elapsed().as_secs_f32() / (ms as f32 / 1000.0)).min(1.0);
            let p = if t < 0.5 { 2.0 * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(2) / 2.0 };
            for l in layers.iter().filter(|l| l.whole) {
                let _ = SetLayeredWindowAttributes(l.hwnd, COLORREF(0), ((if up { p } else { 1.0 - p }) * 255.0) as u8, LWA_ALPHA);
            }
            pump();
            if t >= 1.0 {
                break;
            }
            if DwmFlush().is_err() {
                std::thread::sleep(Duration::from_millis(7));
            }
        }
    }
}

unsafe fn wait(ms: u64) {
    unsafe {
        let until = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < until {
            pump();
            std::thread::sleep(Duration::from_millis(15));
        }
    }
}

unsafe fn leave(layers: Vec<Layer>, ms: u64) {
    unsafe {
        if layers.iter().any(|l| l.whole) {
            dissolve(&layers, ms, false);
        }
        let t0 = Instant::now();
        loop {
            let t = (t0.elapsed().as_secs_f32() / (ms as f32 / 1000.0)).min(1.0);
            for l in layers.iter().filter(|l| !l.whole) {
                let _ = UpdateLayeredWindow(l.hwnd, None, None, None, None, None, COLORREF(0), Some(&blend(((1.0 - t) * 255.0) as u8)), ULW_ALPHA);
            }
            pump();
            if t >= 1.0 {
                break;
            }
            if DwmFlush().is_err() {
                std::thread::sleep(Duration::from_millis(7));
            }
        }
        for l in layers {
            let _ = DestroyWindow(l.hwnd);
            SelectObject(l.dc, l.was);
            let _ = DeleteObject(l.bmp.into());
            let _ = DeleteDC(l.dc);
        }
    }
}

/// Pour `picture` over the desktop, then run `then` (which makes it the real wallpaper) and give back
/// what it says. ULTRON's copy stays over the real one for `hold` ms more and fades off by itself.
pub fn arrive(picture: PathBuf, hold: u64, then: Box<dyn FnOnce() -> Result<(), String> + Send>) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || unsafe {
        let mut layers = build(&picture);
        if !layers.is_empty() {
            if layers.iter().any(|l| l.whole) {
                for l in &layers {
                    let _ = InvalidateRect(Some(l.hwnd), None, false);
                }
                dissolve(&layers, 800, true);
                crate::log_line("wallpaper: dissolved in");
            } else {
                let (frames, took) = pour(&mut layers, 900);
                crate::log_line(&format!("wallpaper: poured in {} ms, {} frames{}", took.as_millis(), frames, if layers.iter().all(|l| l.rects) { "" } else { " (whole surface each frame)" }));
            }
        }
        let _ = tx.send(then());
        if !layers.is_empty() {
            wait(hold);
            leave(layers, 320);
        }
    });
    rx.recv_timeout(Duration::from_secs(12)).unwrap_or_else(|_| Err("The wallpaper took too long to change".into()))
}
