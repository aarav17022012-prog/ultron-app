//! ULTRON's own taskbar (Settings, Look & feel, Taskbar).
//!
//! While it's on, Windows' taskbar is out of the way altogether: it's put on "hide automatically" (so
//! it gives up its strip of the screen and windows reach the bottom edge) and then hidden, so that it
//! doesn't slide back up when the pointer touches the bottom. ULTRON's taskbar is the wheel that
//! rolls down from the top, drawn by the page. How Windows' taskbar was set is written down first and
//! put back exactly when ULTRON's is switched off.
//! This side supplies what the page can't see for itself: which windows are open and which is in
//! front, switching to them, and laying them out side by side with gaps.
//!
//! Windows' taskbar must come back whatever happens to ULTRON. It's shown again when ULTRON quits;
//! and for the times ULTRON is ended from outside, a second, tiny copy of this program does nothing
//! but wait for ULTRON to go and then show it (`--taskbar-guard`).

use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering::SeqCst};
use std::sync::Mutex;
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE};
use windows::Win32::UI::Shell::{SHAppBarMessage, ABM_GETSTATE, ABM_SETSTATE, APPBARDATA};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, FindWindowW, GetWindowPlacement, GetWindowRect, IsIconic, IsWindow, IsWindowVisible, IsZoomed, PostMessageW, SetWindowPlacement, SetWindowPos, ShowWindow, SystemParametersInfoW,
    SC_MINIMIZE, SPI_GETWORKAREA, SWP_NOACTIVATE, SWP_NOZORDER, SW_HIDE, SW_RESTORE, SW_SHOWNOACTIVATE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WINDOWPLACEMENT, WM_CLOSE, WM_SYSCOMMAND,
};

use crate::apps::{Apps, OpenWin};

static ON: AtomicBool = AtomicBool::new(false);
static GUARDED: AtomicBool = AtomicBool::new(false);
static WATCHING: AtomicBool = AtomicBool::new(false);
/// How Windows' taskbar was set before ULTRON's came on (-1: not known).
static WAS: AtomicIsize = AtomicIsize::new(-1);
/// Windows' taskbar isn't hidden before this moment (it's given time to slide away by itself first).
static HIDE_FROM: AtomicU64 = AtomicU64::new(0);
const AUTOHIDE: isize = 1;

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Whether Windows' taskbar hides automatically, stays on top: its own two settings, as one number.
fn tray_state() -> Option<isize> {
    let h = *trays().first()?;
    let mut d = APPBARDATA { cbSize: std::mem::size_of::<APPBARDATA>() as u32, hWnd: h, ..Default::default() };
    Some(unsafe { SHAppBarMessage(ABM_GETSTATE, &mut d) } as isize)
}

fn set_tray_state(state: isize) {
    if let Some(h) = trays().first() {
        let mut d = APPBARDATA { cbSize: std::mem::size_of::<APPBARDATA>() as u32, hWnd: *h, lParam: LPARAM(state), ..Default::default() };
        unsafe { SHAppBarMessage(ABM_SETSTATE, &mut d) };
    }
}

fn note(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("taskbar-was.txt"))
}

/// Windows' taskbar back as it was: shown, and its "hide automatically" as it had been.
fn give_back(was: isize) {
    if was >= 0 {
        set_tray_state(was);
    }
    taskbar(true);
}

/// ULTRON is closing: Windows gets its taskbar back (ULTRON's comes on again by itself next time, if that's the setting).
pub fn leave(app: &AppHandle) {
    if ON.swap(false, SeqCst) {
        give_back(WAS.load(SeqCst));
        if let Some(p) = note(app) {
            let _ = std::fs::remove_file(p);
        }
    }
}
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

/// Windows shows its taskbar again now and then: an app opening, one asking for attention, a notification. It was put
/// away again at the next look, up to half a second later, and it flashed up across the bottom of the screen (over the
/// Drop Shelf too). Now Windows tells ULTRON the moment it shows it, and it's put away at once.
unsafe extern "system" fn tray_shown(_: windows::Win32::UI::Accessibility::HWINEVENTHOOK, _: u32, h: HWND, object: i32, _: i32, _: u32, _: u32) {
    if object != 0 || !ON.load(SeqCst) || now_ms() < HIDE_FROM.load(SeqCst) {
        return;
    }
    let mut buf = [0u16; 32];
    let n = unsafe { windows::Win32::UI::WindowsAndMessaging::GetClassNameW(h, &mut buf) }.max(0) as usize;
    let class = String::from_utf16_lossy(&buf[..n]);
    if class == "Shell_TrayWnd" || class == "Shell_SecondaryTrayWnd" {
        unsafe {
            let _ = ShowWindow(h, SW_HIDE);
        }
    }
}

fn hook_tray() {
    use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetWindowThreadProcessId, MsgWaitForMultipleObjects, PeekMessageW, TranslateMessage, EVENT_OBJECT_SHOW, MSG, PM_REMOVE, QS_ALLINPUT,
        WINEVENT_OUTOFCONTEXT,
    };
    std::thread::spawn(|| unsafe {
        let (mut hook, mut pid_was): (Option<HWINEVENTHOOK>, u32) = (None, 0);
        loop {
            // (Explorer's own: looked for again now and then, as it's a new one when Explorer restarts)
            let pid = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()).map(|h| {
                let mut p = 0u32;
                GetWindowThreadProcessId(h, Some(&mut p));
                p
            });
            let pid = pid.unwrap_or(0);
            if pid != pid_was {
                if let Some(h) = hook.take() {
                    let _ = UnhookWinEvent(h);
                }
                if pid != 0 {
                    hook = Some(SetWinEventHook(EVENT_OBJECT_SHOW, EVENT_OBJECT_SHOW, None, Some(tray_shown), pid, 0, WINEVENT_OUTOFCONTEXT));
                }
                pid_was = pid;
            }
            let t0 = std::time::Instant::now();
            let mut m = MSG::default();
            while t0.elapsed() < Duration::from_secs(3) {
                while PeekMessageW(&mut m, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&m);
                    DispatchMessageW(&m);
                }
                MsgWaitForMultipleObjects(None, false, 250, QS_ALLINPUT);
            }
        }
    });
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

/// Turn ULTRON's taskbar on or off.
pub fn set(app: &AppHandle, on: bool) -> i32 {
    let file = note(app);
    let written = file.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| s.trim().parse::<isize>().ok());
    if !on {
        // (also the way back after ULTRON was cut off with its own taskbar up: what was written down is put back)
        let was = if ON.swap(false, SeqCst) { WAS.load(SeqCst) } else { written.unwrap_or(-1) };
        if was >= 0 || written.is_some() {
            give_back(was);
        } else {
            taskbar(true);
        }
        if let Some(p) = file {
            let _ = std::fs::remove_file(p);
        }
        return 0;
    }
    let Some(now) = tray_state() else { return 0 };
    let was = written.unwrap_or(now); // (written down already: ULTRON was cut off last time, and what's there now is its own doing)
    if let Some(p) = &file {
        let _ = std::fs::write(p, was.to_string());
    }
    WAS.store(was, SeqCst);
    ON.store(true, SeqCst);
    HIDE_FROM.store(now_ms() + 700, SeqCst);
    if now & AUTOHIDE == 0 {
        set_tray_state(now | AUTOHIDE); // (it slides away, and windows are given its strip)
    }
    guard(was);
    if !WATCHING.swap(true, SeqCst) {
        hook_tray();
        let app = app.clone();
        std::thread::spawn(move || {
            let mut last: Vec<OpenWin> = Vec::new();
            let mut front = (0isize, 0u64, 0u64); // (the window in use, since when, and when its picture was last taken)
            loop {
                if !ON.load(SeqCst) {
                    last.clear();
                    std::thread::sleep(Duration::from_millis(1000));
                    continue;
                }
                // (hidden as well, so it doesn't come up when the pointer touches the bottom edge; and Explorer shows it
                //  again now and then, at a notification or when it restarts)
                if now_ms() >= HIDE_FROM.load(SeqCst) {
                    for h in trays() {
                        if unsafe { IsWindowVisible(h) }.as_bool() {
                            unsafe {
                                let _ = ShowWindow(h, SW_HIDE);
                            }
                        }
                    }
                }
                let now = list(&app);
                keep_pictures(&now, &mut front);
                if now != last {
                    let _ = app.emit_to("main", "dock-wins", &now);
                    last = now;
                }
                std::thread::sleep(Duration::from_millis(450));
            }
        });
    }
    0
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
            // (minimized, with ULTRON's minimize animation on: it flies back out of the notch first)
            "focus" => {
                if !(IsIconic(h).as_bool() && crate::minfx::back_out(h)) {
                    crate::apps::focus_window(h)
                }
            }
            "min" => {
                // (its picture as it goes down, for the wheel to show while it's away)
                if KEEP.load(SeqCst) {
                    if let Some(pic) = shoot(h) {
                        remember(n, pic);
                    }
                }
                let _ = PostMessageW(Some(h), WM_SYSCOMMAND, WPARAM(SC_MINIMIZE as usize), LPARAM(0));
            }
            "close" => {
                let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            _ => {}
        }
    }
}

/// What an app's jump list shows as recent: Windows' own list for that app (the one its taskbar button offers),
/// as (name, where it is). `aumid` is the app's id in shell:AppsFolder.
pub fn recent(aumid: &str, n: u32) -> Vec<(String, String)> {
    use windows::core::HSTRING;
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::Common::IObjectArray;
    use windows::Win32::UI::Shell::{ApplicationDocumentLists, IApplicationDocumentLists, IShellItem, ADLT_RECENT, SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_NORMALDISPLAY};
    let mut out = Vec::new();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let Ok(dl) = CoCreateInstance::<_, IApplicationDocumentLists>(&ApplicationDocumentLists, None, CLSCTX_INPROC_SERVER) else { return out };
        if dl.SetAppID(&HSTRING::from(aumid)).is_err() {
            return out;
        }
        let Ok(list) = dl.GetList::<IObjectArray>(ADLT_RECENT, n) else { return out };
        let take = |p: windows::core::PWSTR| {
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as _));
            s
        };
        for i in 0..list.GetCount().unwrap_or(0) {
            let Ok(item) = list.GetAt::<IShellItem>(i) else { continue };
            let name = item.GetDisplayName(SIGDN_NORMALDISPLAY).map(take).unwrap_or_default();
            let path = item.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING).map(take).unwrap_or_default();
            if !name.is_empty() && !path.is_empty() {
                out.push((name, path));
            }
        }
    }
    out
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

/// Put these windows side by side on the main screen, in this order (minimized or maximized ones are
/// brought back to ordinary windows first). Where they were is kept for `untile`. Gives how many.
fn arrange(wins: &[OpenWin], gap: i32) -> usize {
    unsafe {
        let mut area = RECT::default();
        if wins.is_empty() || SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut area as *mut _ as _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)).is_err() {
            return 0;
        }
        let gap = (gap.clamp(0, 40) as f64 * crate::hit::scale_of(HWND::default()).max(1.0)) as i32;
        let spots = layout(wins.len(), area, gap);
        let mut before = Vec::new();
        let mut done = 0;
        for (w, to) in wins.iter().zip(spots) {
            let h = HWND(w.id.parse::<isize>().unwrap_or(0) as _);
            if !IsWindow(Some(h)).as_bool() {
                continue;
            }
            let mut was = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
            if GetWindowPlacement(h, &mut was).is_ok() {
                before.push((h.0 as isize, was));
            }
            if IsZoomed(h).as_bool() || IsIconic(h).as_bool() {
                let _ = ShowWindow(h, SW_RESTORE);
                std::thread::sleep(Duration::from_millis(40)); // (it has to be an ordinary window before it can be sized)
            }
            // (a window's own edges include an unseen border for resizing: the seen edges are what's lined up)
            let (mut out, mut seen) = (RECT::default(), RECT::default());
            let _ = GetWindowRect(h, &mut out);
            if DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, &mut seen as *mut _ as _, std::mem::size_of::<RECT>() as u32).is_err() {
                seen = out;
            }
            let (l, tp, r, b) = ((seen.left - out.left).clamp(0, 30), (seen.top - out.top).clamp(0, 30), (out.right - seen.right).clamp(0, 30), (out.bottom - seen.bottom).clamp(0, 30));
            if SetWindowPos(h, None, to.left - l, to.top - tp, to.right - to.left + l + r, to.bottom - to.top + tp + b, SWP_NOZORDER | SWP_NOACTIVATE).is_ok() {
                done += 1;
            }
        }
        if done > 0 {
            *BEFORE.lock().unwrap() = before;
        }
        crate::log_line(&format!("tile: {} of {} windows placed", done, wins.len()));
        done
    }
}

/// Lay the windows on the main screen out side by side: the ones that are showing, the one in use
/// first (six at most). When fewer than two are showing, the ones down on the taskbar are brought up
/// too (four at most), so that asking for tiles with everything minimized still gives tiles.
pub fn tile(app: &AppHandle, gap: i32) -> usize {
    let main = unsafe { MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTOPRIMARY) };
    let mut all: Vec<OpenWin> = app.state::<Apps>().open_windows();
    all.retain(|w| w.id.parse::<isize>().is_ok_and(|n| unsafe { MonitorFromWindow(HWND(n as _), MONITOR_DEFAULTTONEAREST) } == main));
    let mut wins: Vec<OpenWin> = all.iter().filter(|w| !w.min).cloned().collect();
    wins.sort_by_key(|w| !w.active); // (they come front to back; the one in use leads)
    if wins.len() < 2 {
        wins.extend(all.iter().filter(|w| w.min).cloned());
        wins.truncate(4);
    }
    wins.truncate(6);
    arrange(&wins, gap)
}

/// A group's apps side by side: the windows of these apps (one each, in the group's order).
pub fn tile_apps(app: &AppHandle, launches: &[String], gap: i32) -> usize {
    let all = app.state::<Apps>().open_windows();
    let wins: Vec<OpenWin> = launches.iter().filter_map(|l| all.iter().find(|w| &w.launch == l).cloned()).collect();
    if wins.len() < 2 {
        return wins.len();
    }
    arrange(&wins, gap)
}

#[link(name = "user32")]
unsafe extern "system" {
    fn PrintWindow(hwnd: HWND, hdc: windows::Win32::Graphics::Gdi::HDC, flags: u32) -> windows::core::BOOL;
}

/// The newest small picture of each window that has been in use (never written anywhere: they are
/// for the wheel to show a window that has since been minimized, which can't be pictured any more).
static SHOTS: Mutex<Vec<(isize, String)>> = Mutex::new(Vec::new());
static KEEP: AtomicBool = AtomicBool::new(false);
static SHOOTING: AtomicBool = AtomicBool::new(false);

/// Whether pictures of windows are kept at all (Settings, Taskbar: show a window when the pointer rests on its app).
pub fn keep(on: bool) {
    KEEP.store(on, SeqCst);
    if !on {
        SHOTS.lock().unwrap().clear();
    }
}

fn remember(n: isize, pic: String) {
    let mut s = SHOTS.lock().unwrap();
    s.retain(|x| x.0 != n);
    s.push((n, pic));
    if s.len() > 40 {
        s.remove(0);
    }
}

/// A small picture of a window as it is now (None when it's minimized, or draws nothing). Windows
/// draws the window into a picture of its own size and shrinks that itself; only the small one is
/// brought over and packed.
fn shoot(h: HWND) -> Option<String> {
    use base64::Engine;
    use windows::Win32::Graphics::Gdi::*;
    unsafe {
        if !IsWindow(Some(h)).as_bool() || IsIconic(h).as_bool() {
            return None;
        }
        // (a window's own edges include an unseen border for resizing: the seen part is what's kept)
        let (mut out, mut seen) = (RECT::default(), RECT::default());
        GetWindowRect(h, &mut out).ok()?;
        if DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, &mut seen as *mut _ as _, std::mem::size_of::<RECT>() as u32).is_err() {
            seen = out;
        }
        let (w, hh) = (out.right - out.left, out.bottom - out.top);
        let (sx, sy, sw, sh) = (seen.left - out.left, seen.top - out.top, seen.right - seen.left, seen.bottom - seen.top);
        if w < 60 || hh < 60 || w > 8000 || hh > 5000 || sw < 40 || sh < 40 || sx < 0 || sy < 0 || sx + sw > w || sy + sh > hh {
            return None;
        }
        let k = (440.0 / sw as f64).min(270.0 / sh as f64).min(1.0);
        let (tw, th) = (((sw as f64 * k).round() as i32).max(1), ((sh as f64 * k).round() as i32).max(1));
        let sdc = GetDC(None);
        let (mdc, tdc) = (CreateCompatibleDC(Some(sdc)), CreateCompatibleDC(Some(sdc)));
        let (big, small) = (CreateCompatibleBitmap(sdc, w, hh), CreateCompatibleBitmap(sdc, tw, th));
        let (old_b, old_s) = (SelectObject(mdc, big.into()), SelectObject(tdc, small.into()));
        let ok = PrintWindow(h, mdc, 2).as_bool(); // (2: as the screen draws it, so apps that draw with the graphics card are there too)
        SetStretchBltMode(tdc, HALFTONE);
        let _ = SetBrushOrgEx(tdc, 0, 0, None);
        let _ = StretchBlt(tdc, 0, 0, tw, th, Some(mdc), sx, sy, sw, sh, SRCCOPY);
        let mut bi = BITMAPINFO::default();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = tw;
        bi.bmiHeader.biHeight = -th;
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        let mut buf = vec![0u8; (tw * th * 4) as usize];
        let lines = GetDIBits(tdc, small, 0, th as u32, Some(buf.as_mut_ptr() as _), &mut bi, DIB_RGB_COLORS);
        SelectObject(mdc, old_b);
        SelectObject(tdc, old_s);
        let _ = DeleteObject(big.into());
        let _ = DeleteObject(small.into());
        let _ = DeleteDC(mdc);
        let _ = DeleteDC(tdc);
        ReleaseDC(None, sdc);
        if !ok || lines == 0 {
            return None;
        }
        // (a window that gives nothing comes back black from edge to edge: no picture is better than that one)
        if buf.chunks_exact(4).step_by(41).all(|p| p[0] < 6 && p[1] < 6 && p[2] < 6) {
            return None;
        }
        let rgb: Vec<u8> = buf.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
        let img = image::RgbImage::from_raw(tw as u32, th as u32, rgb)?;
        let mut jpg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpg, 82).encode_image(&img).ok()?;
        Some(format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(jpg)))
    }
}

/// A window's picture for the wheel to show beside its bead: as it is now, or, minimized, as it was
/// when last in use (None when there has never been one).
pub fn peek(id: &str) -> Option<String> {
    let n = id.parse::<isize>().ok()?;
    if let Some(pic) = shoot(HWND(n as _)) {
        if KEEP.load(SeqCst) {
            remember(n, pic.clone());
        }
        return Some(pic);
    }
    SHOTS.lock().unwrap().iter().find(|s| s.0 == n).map(|s| s.1.clone())
}

/// Called by the watcher each time it looks: pictures of windows that have closed are dropped, and
/// the window in use has its picture taken (once it has been in use for a moment, then every twenty
/// seconds), on a thread of its own so that a window slow to draw never holds the watcher up.
fn keep_pictures(wins: &[OpenWin], front: &mut (isize, u64, u64)) {
    if !KEEP.load(SeqCst) {
        return;
    }
    let live: Vec<isize> = wins.iter().filter_map(|w| w.id.parse().ok()).collect();
    SHOTS.lock().unwrap().retain(|s| live.contains(&s.0));
    let Some(a) = wins.iter().find(|w| w.active && !w.min).and_then(|w| w.id.parse::<isize>().ok()) else { return };
    let t = now_ms();
    if a != front.0 {
        *front = (a, t, 0);
    }
    if t.saturating_sub(front.1) > 900 && t.saturating_sub(front.2) > 20_000 && !SHOOTING.swap(true, SeqCst) {
        front.2 = t;
        std::thread::spawn(move || {
            if let Some(pic) = shoot(HWND(a as _)) {
                remember(a, pic);
            }
            SHOOTING.store(false, SeqCst);
        });
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

/// The second copy that only waits: started once, the first time Windows' taskbar is put away.
fn guard(was: isize) {
    if GUARDED.swap(true, SeqCst) {
        return;
    }
    if let Ok(me) = std::env::current_exe() {
        const DETACHED: u32 = 0x0000_0008 | 0x0800_0000; // DETACHED_PROCESS, CREATE_NO_WINDOW
        let _ = std::process::Command::new(me).arg("--taskbar-guard").arg(std::process::id().to_string()).arg(was.to_string()).creation_flags(DETACHED).spawn();
    }
}

/// Called first thing in `main`: when this copy was started as the guard, it waits for the ULTRON
/// it was given to end, gives Windows its taskbar back as it was, and says so (true: nothing else to do).
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
    give_back(args.get(i + 2).and_then(|s| s.parse::<isize>().ok()).unwrap_or(-1));
    crate::minfx::heal(); // (and Windows' own minimize animation, if ULTRON had turned it off)
    crate::notes::restore_left_over(); // (and the banners ULTRON's notifications had turned off)
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
