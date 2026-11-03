//! Picking text with SYLVER beside you. You choose a text tool (Rewrite, Translate, Explain…) and then
//! select the text in whatever app it's in. While that's on, this watches your selecting: as you drag,
//! the page puts the character next to your cursor; when you let go it gets the selected words and
//! where they are on screen, marks them, and asks "This?".
//!
//! It only looks: the selection is read through the app's own accessibility interface (the one screen
//! readers use), nothing is copied or typed until you say yes. Apps that don't offer that interface
//! still work: the drag itself is noticed, and the words are fetched (by copying) only after your yes.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager};
use windows::core::Interface;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::System::Ole::SafeArrayDestroy;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetWindowRect};

static ON: AtomicBool = AtomicBool::new(false);
static RUN: AtomicU64 = AtomicU64::new(0);

enum Sel {
    /// the keyboard is in SYLVER itself (you clicked its panel): says nothing about the other app
    Ours,
    None,
    Some(String, Vec<RECT>),
}

/// The selected text of one element, if it has any, with where each line of it is on screen.
unsafe fn selected_in(uia: &IUIAutomation, e: &IUIAutomationElement) -> Option<(String, Vec<RECT>)> {
    let tp = e.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId).ok()?;
    let ranges = tp.GetSelection().ok()?;
    let (mut text, mut rects) = (String::new(), Vec::new());
    for i in 0..ranges.Length().unwrap_or(0).min(4) {
        let Ok(r) = ranges.GetElement(i) else { continue };
        let t = r.GetText(4000).map(|b| b.to_string()).unwrap_or_default();
        if t.trim().is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&t);
        if let Ok(sa) = r.GetBoundingRectangles() {
            if !sa.is_null() {
                let mut p: *mut RECT = std::ptr::null_mut();
                if let Ok(n) = uia.SafeArrayToRectNativeArray(sa, &mut p) {
                    if !p.is_null() {
                        rects.extend(std::slice::from_raw_parts(p, n.max(0) as usize).iter().take(60).filter(|r| r.right - r.left > 1 && r.bottom - r.top > 3).cloned());
                        CoTaskMemFree(Some(p as _));
                    }
                }
                let _ = SafeArrayDestroy(sa);
            }
        }
    }
    (text.trim().chars().count() >= 2).then(|| (text.trim().to_string(), rects))
}

/// What's selected where the keyboard is. `window`: when the keyboard is in SYLVER, look in this
/// window instead (the app you were in a moment ago), for text that was already selected.
unsafe fn selection(uia: &IUIAutomation, own: u32, window: Option<HWND>) -> Sel {
    let Ok(walker) = uia.ControlViewWalker() else { return Sel::None };
    if let Ok(focus) = uia.GetFocusedElement() {
        if focus.CurrentProcessId().map(|p| p as u32 == own).unwrap_or(false) {
            let Some(h) = window else { return Sel::Ours };
            // the page or text box in that window (a few levels down)
            let Ok(root) = uia.ElementFromHandle(h) else { return Sel::Ours };
            let mut queue = std::collections::VecDeque::from([root]);
            let (t0, mut n) = (Instant::now(), 0);
            while let Some(e) = queue.pop_front() {
                n += 1;
                if n > 400 || t0.elapsed() > Duration::from_millis(500) {
                    break;
                }
                if matches!(e.CurrentControlType().map(|c| c.0).unwrap_or(0), 50030 | 50004) {
                    if let Some((t, r)) = selected_in(uia, &e) {
                        return Sel::Some(t, r);
                    }
                }
                let mut c = walker.GetFirstChildElement(&e).ok();
                while let Some(x) = c {
                    c = walker.GetNextSiblingElement(&x).ok();
                    queue.push_back(x);
                }
            }
            return Sel::Ours;
        }
        // the thing with the keyboard, or what it sits inside (a paragraph inside a page, a field inside a form)
        let mut e = Some(focus);
        for _ in 0..7 {
            let Some(x) = e else { break };
            if let Some((t, r)) = selected_in(uia, &x) {
                return Sel::Some(t, r);
            }
            e = walker.GetParentElement(&x).ok();
        }
    }
    Sel::None
}

/// Lines of a selection: boxes on the same line are joined, so each line is marked once.
fn lines(mut rects: Vec<RECT>) -> Vec<RECT> {
    rects.sort_by_key(|r| (r.top, r.left));
    let mut out: Vec<RECT> = Vec::new();
    for r in rects {
        match out.last_mut() {
            Some(l) if (r.top - l.top).abs() <= (l.bottom - l.top) / 2 && r.left <= l.right + 14 => {
                l.right = l.right.max(r.right);
                l.bottom = l.bottom.max(r.bottom);
                l.top = l.top.min(r.top);
            }
            _ => out.push(r),
        }
    }
    out.truncate(24);
    out
}

pub fn stop() {
    ON.store(false, Ordering::Relaxed);
    RUN.fetch_add(1, Ordering::Relaxed);
}

pub fn start(app: AppHandle) {
    let run = RUN.fetch_add(1, Ordering::Relaxed) + 1;
    ON.store(true, Ordering::Relaxed);
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let uia: IUIAutomation = match CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER).or_else(|_| CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)) {
            Ok(u) => u,
            Err(_) => return,
        };
        if let Ok(u2) = uia.cast::<IUIAutomation2>() {
            let _ = u2.SetTransactionTimeout(900);
            let _ = u2.SetConnectionTimeout(700);
        }
        let own = std::process::id();
        let Some(main) = app.get_webview_window("main").and_then(|w| crate::hit::hwnd_of(&w)) else { return };
        // screen pixels -> the page's own
        let to_page = |x: i32, y: i32| {
            let mut w = RECT::default();
            let _ = GetWindowRect(main, &mut w);
            let s = crate::hit::scale_of(main);
            ((x - w.left) as f64 / s, (y - w.top) as f64 / s)
        };
        let ours = |p: POINT| {
            let (x, y) = to_page(p.x, p.y);
            app.state::<crate::hit::Hits>().0.lock().unwrap().get("main").map(|v| v.iter().any(|h| x >= h.x - 4.0 && x <= h.x + h.w + 4.0 && y >= h.y - 4.0 && y <= h.y + h.h + 4.0)).unwrap_or(false)
        };
        let found = |text: &str, rects: Vec<RECT>, at: Option<POINT>| {
            let ls = lines(rects);
            // where the character goes: where you let go, or the end of the last line
            let (ax, ay) = match (at, ls.last()) {
                (Some(p), _) => to_page(p.x, p.y),
                (None, Some(l)) => to_page(l.right, l.bottom),
                _ => (-1.0, -1.0),
            };
            let boxes: Vec<[f64; 4]> = ls.iter().map(|r| { let (x, y) = to_page(r.left, r.top); let (x2, y2) = to_page(r.right, r.bottom); [x, y, x2 - x, y2 - y] }).collect();
            crate::log_line(&format!("pick: {} characters selected, {} line(s) marked", text.chars().count(), boxes.len()));
            let _ = app.emit_to("main", "pick", serde_json::json!({ "kind": "found", "text": text.chars().take(1200).collect::<String>(), "rects": boxes, "x": ax, "y": ay }));
        };
        let lost = || { let _ = app.emit_to("main", "pick", serde_json::json!({ "kind": "lost" })); };

        let mut down: Option<(POINT, bool)> = None;
        let (mut dragging, mut shown, mut blind) = (false, String::new(), false);
        let mut last_up: Option<(POINT, Instant)> = None;
        let mut polled = Instant::now() - Duration::from_secs(1);
        let mut first = true;
        let t0 = Instant::now();
        while ON.load(Ordering::Relaxed) && RUN.load(Ordering::Relaxed) == run && t0.elapsed() < Duration::from_secs(240) {
            std::thread::sleep(Duration::from_millis(16));
            let mut p = POINT::default();
            if GetCursorPos(&mut p).is_err() {
                continue;
            }
            let pressed = GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0;
            match (pressed, down) {
                (true, None) => {
                    let mine = ours(p);
                    down = Some((p, mine));
                    dragging = false;
                }
                (true, Some((p0, false))) => {
                    if !dragging && ((p.x - p0.x).abs() > 7 || (p.y - p0.y).abs() > 7) {
                        dragging = true;
                        let _ = app.emit_to("main", "pick", serde_json::json!({ "kind": "drag" }));
                    }
                }
                (false, Some((_, mine))) => {
                    down = None;
                    if mine {
                        continue; // a click on SYLVER itself (its panel, the Yes button)
                    }
                    let twice = last_up.map(|(q, t)| t.elapsed() < Duration::from_millis(550) && (q.x - p.x).abs() < 6 && (q.y - p.y).abs() < 6).unwrap_or(false);
                    last_up = Some((p, Instant::now()));
                    let dragged = std::mem::take(&mut dragging);
                    std::thread::sleep(Duration::from_millis(90)); // (apps settle their selection as the button comes up)
                    polled = Instant::now();
                    match selection(&uia, own, None) {
                        Sel::Some(t, r) => {
                            blind = false;
                            shown = t.clone();
                            found(&t, r, Some(p));
                        }
                        // an app that doesn't say what's selected: a drag or a double-click there is still a selection
                        Sel::None if dragged || twice => {
                            blind = true;
                            shown.clear();
                            found("", Vec::new(), Some(p));
                        }
                        Sel::None => {
                            if !shown.is_empty() || blind {
                                lost();
                            }
                            shown.clear();
                            blind = false;
                        }
                        Sel::Ours => {}
                    }
                }
                (false, None) => {
                    // selections made with the keyboard, and text that was already selected when this started
                    if polled.elapsed() > Duration::from_millis(if first { 60 } else { 380 }) {
                        polled = Instant::now();
                        let window = if first { app.state::<crate::apps::Apps>().last_window() } else { None };
                        first = false;
                        match selection(&uia, own, window) {
                            Sel::Some(t, r) if t != shown => {
                                blind = false;
                                shown = t.clone();
                                found(&t, r, None);
                            }
                            Sel::None if !shown.is_empty() => {
                                shown.clear();
                                lost();
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        if RUN.load(Ordering::Relaxed) == run {
            ON.store(false, Ordering::Relaxed);
            let _ = app.emit_to("main", "pick", serde_json::json!({ "kind": "timeout" }));
        }
    });
}
