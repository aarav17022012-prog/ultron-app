//! Click-through for the transparent window. The page reports the rectangles it
//! actually draws in (menu bar, notch, open panels); outside them the OS sends
//! clicks to whatever is underneath. The same loop streams the cursor (in the
//! window's CSS pixels) so the page can react to it even while click-through.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager, WebviewWindow};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowLongW, GetWindowRect, IsWindowVisible, SetWindowLongW, SetWindowPos, GWL_EXSTYLE, HWND_TOPMOST,
    SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, WS_EX_LAYERED, WS_EX_TRANSPARENT,
};

#[derive(Default, Clone, Copy, serde::Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Default)]
pub struct Hits(pub Mutex<HashMap<String, Vec<Rect>>>);

pub fn hwnd_of(w: &WebviewWindow) -> Option<HWND> {
    w.hwnd().ok().map(|h| HWND(h.0 as _))
}

pub fn scale_of(hwnd: HWND) -> f64 {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 { 1.0 } else { dpi as f64 / 96.0 }
}

/// Flip click-through right here instead of queueing it for the UI thread: a
/// queued flip could land after your click, which then went to the window below.
fn set_click_through(hwnd: HWND, on: bool) {
    unsafe {
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let bits = WS_EX_TRANSPARENT.0 | WS_EX_LAYERED.0;
        let new = if on { ex | bits } else { ex & !bits };
        if new != ex {
            SetWindowLongW(hwnd, GWL_EXSTYLE, new as i32);
        }
    }
}

/// ULTRON back on top of every other always-on-top window, at once (called as soon as another app comes to the front,
/// so nothing that opens stays over the bar or the notch for long).
pub fn raise_all(app: &AppHandle) {
    if let Some(h) = app.get_webview_window("main").and_then(|w| hwnd_of(&w)) {
        raise(h);
    }
    crate::thumbs::raise();
    if let Some(h) = app.get_webview_window("browser").and_then(|b| hwnd_of(&b)) {
        raise(h);
    }
}

/// Put a window at the very top of the always-on-top band, without focusing it.
pub fn raise(hwnd: HWND) {
    unsafe {
        if IsWindowVisible(hwnd).as_bool() {
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0, 0, 0, 0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_ASYNCWINDOWPOS,
            );
        }
    }
}

struct Target {
    label: String,
    win: WebviewWindow,
    hwnd: HWND,
    ignoring: Option<bool>,
    last: (f64, f64),
    last_emit: Instant,
}

const MARGIN: f64 = 10.0; // clickable a little before the pointer arrives
const NEAR: f64 = 120.0; // poll fast when this close to anything clickable

pub fn watch(app: AppHandle, labels: &[&str]) {
    let labels: Vec<String> = labels.iter().map(|s| s.to_string()).collect();
    std::thread::spawn(move || {
        let mut targets: Vec<Target> = labels
            .iter()
            .filter_map(|l| {
                let win = app.get_webview_window(l)?;
                let hwnd = hwnd_of(&win)?;
                Some(Target { label: l.clone(), win, hwnd, ignoring: None, last: (f64::NAN, f64::NAN), last_emit: Instant::now() })
            })
            .collect();
        let mut last_topmost = Instant::now();
        let mut near = false;
        loop {
            std::thread::sleep(Duration::from_millis(if near { 10 } else { 33 }));
            let mut p = POINT::default();
            if unsafe { GetCursorPos(&mut p) }.is_err() {
                continue;
            }
            let hits = app.state::<Hits>().0.lock().unwrap().clone();
            near = false;
            for t in targets.iter_mut() {
                if !unsafe { IsWindowVisible(t.hwnd) }.as_bool() {
                    continue;
                }
                let mut r = RECT::default();
                if unsafe { GetWindowRect(t.hwnd, &mut r) }.is_err() {
                    continue;
                }
                let s = scale_of(t.hwnd);
                let (x, y) = ((p.x - r.left) as f64 / s, (p.y - r.top) as f64 / s);
                let rects = hits.get(&t.label).map(|v| v.as_slice()).unwrap_or(&[]);
                let within = |m: f64| rects.iter().any(|h| x >= h.x - m && x <= h.x + h.w + m && y >= h.y - m && y <= h.y + h.h + m);
                let inside = within(MARGIN);
                near |= within(NEAR);
                if t.ignoring != Some(!inside) {
                    set_click_through(t.hwnd, !inside);
                    let _ = t.win.set_ignore_cursor_events(!inside); // keep the window's own bookkeeping in step
                    t.ignoring = Some(!inside);
                }
                let moved = (x - t.last.0).abs() >= 1.0 || (y - t.last.1).abs() >= 1.0 || t.last.0.is_nan();
                if moved && t.last_emit.elapsed() >= Duration::from_millis(30) {
                    t.last = (x, y);
                    t.last_emit = Instant::now();
                    // (and whether a button is held: a drag that reaches the bottom edge mustn't bring the shelf up)
                    let down = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 || GetAsyncKeyState(VK_RBUTTON.0 as i32) < 0 };
                    let _ = app.emit_to(t.label.as_str(), "cursor", (x, y, down));
                }
            }
            // other always-on-top windows can climb above us; quietly take the top back
            if last_topmost.elapsed() > Duration::from_secs(1) {
                last_topmost = Instant::now();
                for t in &targets {
                    unsafe {
                        let _ = SetWindowPos(
                            t.hwnd,
                            Some(HWND_TOPMOST),
                            0, 0, 0, 0,
                            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_ASYNCWINDOWPOS,
                        );
                    }
                }
                // the shelf's live pictures go over the page (they're see-through and let clicks through)
                crate::thumbs::raise();
                // the notch browser must stay above the notch itself
                if let Some(h) = app.get_webview_window("browser").and_then(|b| hwnd_of(&b)) {
                    raise(h);
                }
                // and the introduction plays above everything
                if let Some(h) = app.get_webview_window("intro").and_then(|b| hwnd_of(&b)) {
                    raise(h);
                }
            }
        }
    });
}
