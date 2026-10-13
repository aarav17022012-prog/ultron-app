//! ULTRON's own taskbar (Settings, Look & feel, Taskbar).
//!
//! While it's on, Windows' taskbar is hidden. Its strip of the screen stays reserved (nothing about
//! the taskbar's settings is changed), and ULTRON's taskbar, drawn by the page, sits in that strip.
//! This side supplies what the page can't see for itself: which windows are open and which is in
//! front, switching to them, and laying them out side by side with gaps.
//!
//! Windows' taskbar must come back whatever happens to ULTRON. It's shown again when ULTRON quits;
//! and for the times ULTRON is ended from outside, a second, tiny copy of this program does nothing
//! but wait for ULTRON to go and then show it (`--taskbar-guard`).

use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::Mutex;
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, FindWindowW, GetWindowPlacement, GetWindowRect, IsIconic, IsWindow, IsWindowVisible, IsZoomed, PostMessageW, SetWindowPlacement, SetWindowPos, ShowWindow, SystemParametersInfoW,
    SC_MINIMIZE, SPI_GETWORKAREA, SWP_NOACTIVATE, SWP_NOZORDER, SW_HIDE, SW_RESTORE, SW_SHOWNOACTIVATE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WINDOWPLACEMENT, WM_CLOSE, WM_SYSCOMMAND,
};

use crate::apps::{Apps, OpenWin};

static ON: AtomicBool = AtomicBool::new(false);
static GUARDED: AtomicBool = AtomicBool::new(false);
static WATCHING: AtomicBool = AtomicBool::new(false);
/// The windows in the order they were first seen (the taskbar's icons don't shuffle as you switch).
static ORDER: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Where the windows were before they were tiled, to put them back.
static BEFORE: Mutex<Vec<(isize, WINDOWPLACEMENT)>> = Mutex::new(Vec::new());

/// Windows' taskbar: the main one and the one on each further screen.
fn trays() -> Vec<HWND> {
    let mut v = Vec::new();
    unsafe {
        if let Ok(h) = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()) {
            v.push(h);
        }
        let mut last = None;
        while let Ok(h) = FindWindowExW(None, last, w!("Shell_SecondaryTrayWnd"), PCWSTR::null()) {
            v.push(h);
            last = Some(h);
            if v.len() > 8 {
                break;
            }
        }
    }
    v
}

pub fn taskbar(show: bool) {
    for h in trays() {
        unsafe {
            let _ = ShowWindow(h, if show { SW_SHOWNOACTIVATE } else { SW_HIDE });
        }
    }
}

/// The strip of the screen Windows' taskbar has (when it runs along the bottom, where ULTRON's goes).
fn strip() -> Option<RECT> {
    let h = *trays().first()?;
    let mut r = RECT::default();
    unsafe { GetWindowRect(h, &mut r) }.ok()?;
    let (w, t) = (r.right - r.left, r.bottom - r.top);
    (w > t * 4 && t > 16 && t < 200).then_some(r)
}

/// Windows' taskbar as it looks now, for the page to show sliding away as ULTRON's comes in.
pub fn snap() -> Option<String> {
    use base64::Engine;
    let r = strip()?;
    let px = crate::vision::grab(r)?;
    let rgb: Vec<u8> = px.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
    let img = image::RgbImage::from_raw((r.right - r.left) as u32, (r.bottom - r.top) as u32, rgb)?;
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90).encode_image(&img).ok()?;
    Some(format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(out)))
}

/// The open windows, in the order they were first seen.
pub fn list(app: &AppHandle) -> Vec<OpenWin> {
    let mut wins = app.state::<Apps>().open_windows();
    let mut order = ORDER.lock().unwrap();
    order.retain(|id| wins.iter().any(|w| &w.id == id));
    for w in &wins {
        if !order.contains(&w.id) {
            order.push(w.id.clone());
        }
    }
    wins.sort_by_key(|w| order.iter().position(|id| *id == w.id).unwrap_or(usize::MAX));
    wins
}

/// Turn ULTRON's taskbar on or off. Gives the height of the strip it sits in (screen pixels; 0 when
/// Windows' taskbar isn't along the bottom, and the page then keeps to its usual height).
pub fn set(app: &AppHandle, on: bool) -> i32 {
    let height = strip().map(|r| r.bottom - r.top).unwrap_or(0);
    ON.store(on, SeqCst);
    taskbar(!on);
    if on {
        guard();
        if !WATCHING.swap(true, SeqCst) {
            let app = app.clone();
            std::thread::spawn(move || {
                let mut last: Vec<OpenWin> = Vec::new();
                loop {
                    if !ON.load(SeqCst) {
                        last.clear();
                        std::thread::sleep(Duration::from_millis(1000));
                        continue;
                    }
                    // (Explorer shows its taskbar again now and then: at a notification, when it restarts)
                    for h in trays() {
                        if unsafe { IsWindowVisible(h) }.as_bool() {
                            unsafe {
                                let _ = ShowWindow(h, SW_HIDE);
                            }
                        }
                    }
                    let now = list(&app);
                    if now != last {
                        let _ = app.emit_to("main", "dock-wins", &now);
                        last = now;
                    }
                    std::thread::sleep(Duration::from_millis(450));
                }
            });
        }
    }
    height
}

/// Switch to a window, send it down to the taskbar, or close it.
pub fn act(id: &str, what: &str) {
    let Ok(n) = id.parse::<isize>() else { return };
    let h = HWND(n as _);
    unsafe {
        if !IsWindow(Some(h)).as_bool() {
            return;
        }
        match what {
            "focus" => crate::apps::focus_window(h),
            "min" => {
                let _ = PostMessageW(Some(h), WM_SYSCOMMAND, WPARAM(SC_MINIMIZE as usize), LPARAM(0));
            }
            "close" => {
                let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            _ => {}
        }
    }
}

/// Where `n` windows go in `area`, with `gap` between them and around them: one fills it, two share it,
/// more put the first on the left (a little over half) and stack the rest on the right; past six, a grid.
pub fn layout(n: usize, area: RECT, gap: i32) -> Vec<RECT> {
    let (x0, y0, w, h) = (area.left + gap, area.top + gap, area.right - area.left - 2 * gap, area.bottom - area.top - 2 * gap);
    let cell = |col: i32, cols: i32, row: i32, rows: i32, x: i32, wide: i32| {
        let cw = (wide - gap * (cols - 1)) / cols;
        let ch = (h - gap * (rows - 1)) / rows;
        RECT { left: x + col * (cw + gap), top: y0 + row * (ch + gap), right: x + col * (cw + gap) + cw, bottom: y0 + row * (ch + gap) + ch }
    };
    match n {
        0 => Vec::new(),
        1 => vec![cell(0, 1, 0, 1, x0, w)],
        2 => vec![cell(0, 2, 0, 1, x0, w), cell(1, 2, 0, 1, x0, w)],
        3..=6 => {
            let lead = (w - gap) * 55 / 100;
            let mut v = vec![RECT { left: x0, top: y0, right: x0 + lead, bottom: y0 + h }];
            let rest = (n - 1) as i32;
            v.extend((0..rest).map(|i| cell(0, 1, i, rest, x0 + lead + gap, w - lead - gap)));
            v
        }
        _ => {
            let cols = (n as f64).sqrt().ceil() as i32;
            let rows = (n as i32 + cols - 1) / cols;
            (0..n as i32).map(|i| cell(i % cols, cols, i / cols, rows, x0, w)).collect()
        }
    }
}

/// Lay the open windows on the main screen out side by side, the one in front first. Gives how many.
pub fn tile(app: &AppHandle, gap: i32) -> usize {
    unsafe {
        let main = MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTOPRIMARY);
        let mut wins: Vec<OpenWin> = app.state::<Apps>().open_windows().into_iter().filter(|w| !w.min).collect();
        wins.retain(|w| w.id.parse::<isize>().is_ok_and(|n| MonitorFromWindow(HWND(n as _), MONITOR_DEFAULTTONEAREST) == main));
        wins.sort_by_key(|w| !w.active); // (they come front to back; the one in use leads)
        let mut area = RECT::default();
        if wins.is_empty() || SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut area as *mut _ as _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)).is_err() {
            return 0;
        }
        let gap = (gap.clamp(0, 40) as f64 * crate::hit::scale_of(HWND::default()).max(1.0)) as i32;
        let spots = layout(wins.len(), area, gap);
        let mut before = Vec::new();
        for (w, to) in wins.iter().zip(spots) {
            let h = HWND(w.id.parse::<isize>().unwrap_or(0) as _);
            let mut was = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
            if GetWindowPlacement(h, &mut was).is_ok() {
                before.push((h.0 as isize, was));
            }
            if IsZoomed(h).as_bool() || IsIconic(h).as_bool() {
                let _ = ShowWindow(h, SW_RESTORE);
            }
            // (a window's own edges include an unseen border for resizing: the seen edges are what's lined up)
            let (mut out, mut seen) = (RECT::default(), RECT::default());
            let _ = GetWindowRect(h, &mut out);
            if DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, &mut seen as *mut _ as _, std::mem::size_of::<RECT>() as u32).is_err() {
                seen = out;
            }
            let (l, t, r, b) = (seen.left - out.left, seen.top - out.top, out.right - seen.right, out.bottom - seen.bottom);
            let _ = SetWindowPos(h, None, to.left - l, to.top - t, to.right - to.left + l + r, to.bottom - to.top + t + b, SWP_NOZORDER | SWP_NOACTIVATE);
        }
        *BEFORE.lock().unwrap() = before;
        wins.len()
    }
}

/// Put the windows back where they were before they were tiled.
pub fn untile() -> usize {
    let before = std::mem::take(&mut *BEFORE.lock().unwrap());
    for (h, was) in &before {
        unsafe {
            if IsWindow(Some(HWND(*h as _))).as_bool() {
                let _ = SetWindowPlacement(HWND(*h as _), was);
            }
        }
    }
    before.len()
}

/// The second copy that only waits: started once, the first time Windows' taskbar is hidden.
fn guard() {
    if GUARDED.swap(true, SeqCst) {
        return;
    }
    if let Ok(me) = std::env::current_exe() {
        const DETACHED: u32 = 0x0000_0008 | 0x0800_0000; // DETACHED_PROCESS, CREATE_NO_WINDOW
        let _ = std::process::Command::new(me).arg("--taskbar-guard").arg(std::process::id().to_string()).creation_flags(DETACHED).spawn();
    }
}

/// Called first thing in `main`: when this copy was started as the guard, it waits for the ULTRON
/// it was given to end, shows Windows' taskbar, and says so (true: there's nothing else to do).
pub fn guard_if_asked() -> bool {
    let args: Vec<String> = std::env::args().collect();
    let Some(i) = args.iter().position(|a| a == "--taskbar-guard") else { return false };
    if let Some(pid) = args.get(i + 1).and_then(|p| p.parse::<u32>().ok()) {
        unsafe {
            if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
                WaitForSingleObject(h, INFINITE);
            }
        }
    }
    taskbar(true);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiled windows fill the area, keep the gap everywhere, and never overlap.
    #[test]
    fn tiles_keep_their_gaps() {
        let area = RECT { left: 0, top: 40, right: 1920, bottom: 1032 };
        for n in 1..=9usize {
            let v = layout(n, area, 10);
            assert_eq!(v.len(), n);
            for (i, a) in v.iter().enumerate() {
                assert!(a.left >= area.left + 10 && a.top >= area.top + 10 && a.right <= area.right - 10 && a.bottom <= area.bottom - 10, "{n}: {a:?}");
                assert!(a.right - a.left > 150 && a.bottom - a.top > 100, "{n}: {a:?}");
                for b in &v[i + 1..] {
                    let apart = a.right + 10 <= b.left || b.right + 10 <= a.left || a.bottom + 10 <= b.top || b.bottom + 10 <= a.top;
                    assert!(apart, "{n}: {a:?} and {b:?}");
                }
            }
        }
        let three = layout(3, area, 10);
        assert!(three[0].right - three[0].left > three[1].right - three[1].left, "the first is the wider");
    }
}
