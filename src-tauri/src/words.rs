//! Dictated words dragged off their card and dropped into a typing area: a text box, a chat, a document. It's ULTRON's
//! own drag, not Windows' (that drops words anywhere, into a folder as a file too). While it goes, ULTRON lets the
//! pointer through to the app below, and what's under it is asked, through the accessibility interface screen readers
//! use, whether you can type there. One you can is outlined; let go over it, it's clicked where you let go and the words
//! are typed in. Let go anywhere else (or Esc), they go back onto the card.
//!
//! The page draws the words following the pointer from "words-drag" (where the pointer is, and the typing area under it),
//! and "words-drop" says how it ended.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};
use windows::core::Interface;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{mouse_event, GetAsyncKeyState, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, VK_ESCAPE, VK_LBUTTON};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetWindowRect};

static ON: AtomicBool = AtomicBool::new(false);

/// Whether you can type into this: a text box, or a document that isn't only to read.
unsafe fn typable(e: &IUIAutomationElement) -> bool {
    unsafe {
        let Ok(kind) = e.CurrentControlType() else { return false };
        if kind != UIA_EditControlTypeId && kind != UIA_DocumentControlTypeId {
            return false;
        }
        if !e.CurrentIsEnabled().is_ok_and(|b| b.as_bool()) {
            return false;
        }
        if let Ok(v) = e.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) {
            return v.CurrentIsReadOnly().is_ok_and(|b| !b.as_bool());
        }
        // (a document with no value to set: one only to read, a web page, says so of its text)
        if let Ok(t) = e.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) {
            return t.DocumentRange().and_then(|r| r.GetAttributeValue(UIA_IsReadOnlyAttributeId)).ok().and_then(|v| bool::try_from(&v).ok()) == Some(false);
        }
        kind == UIA_EditControlTypeId && e.CurrentIsKeyboardFocusable().is_ok_and(|b| b.as_bool())
    }
}

/// Whether the app you were in has its cursor in somewhere you can type (it's given the keyboard back first, if ULTRON
/// had it): dictation then types straight into it, rather than onto its card.
pub fn typing_ready(app: &AppHandle) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid));
        if pid == std::process::id() {
            app.state::<crate::apps::Apps>().refocus();
            std::thread::sleep(Duration::from_millis(220));
        }
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Ok(uia) = CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER) else { return false };
        if let Ok(e) = uia.GetFocusedElement() {
            if e.CurrentProcessId().unwrap_or(0) as u32 != std::process::id() && typable(&e) {
                return true;
            }
        }
        // (given the keyboard back, an app doesn't always put its cursor back in its box at once: its own note of where
        //  its text cursor was, and of the box that had the keyboard, says so too)
        use windows::Win32::UI::WindowsAndMessaging::{GetGUIThreadInfo, GUITHREADINFO};
        let Some(h) = app.state::<crate::apps::Apps>().last_window() else { return false };
        let tid = GetWindowThreadProcessId(h, None);
        let mut gi = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        if GetGUIThreadInfo(tid, &mut gi).is_err() {
            return false;
        }
        if !gi.hwndCaret.is_invalid() {
            return true;
        }
        !gi.hwndFocus.is_invalid() && uia.ElementFromHandle(gi.hwndFocus).is_ok_and(|e| typable(&e))
    }
}

/// The typing area at this point of the screen, if there's one there (where it is).
unsafe fn typing_area(uia: &IUIAutomation, walker: &IUIAutomationTreeWalker, p: POINT, own: u32) -> Option<RECT> {
    unsafe {
        let mut e = uia.ElementFromPoint(p).ok()?;
        if e.CurrentProcessId().ok()? as u32 == own {
            return None;
        }
        // (what's under the pointer is often a line or a paragraph inside the box: the box is a little way up)
        for _ in 0..5 {
            if typable(&e) {
                return e.CurrentBoundingRectangle().ok();
            }
            e = walker.GetParentElement(&e).ok()?;
        }
        None
    }
}

/// Starts dragging these words (the button is down, on the card); it ends when it's let go.
pub fn drag(app: AppHandle, text: String) {
    if ON.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(main) = app.get_webview_window("main").and_then(|w| crate::hit::hwnd_of(&w)) else {
        ON.store(false, Ordering::SeqCst);
        return;
    };
    let main = main.0 as isize;
    crate::hit::DRAGGING_OUT.store(true, Ordering::SeqCst);
    // what's under the pointer is asked on a thread of its own (an app can be slow to answer), always where it is now
    let (ask, asks) = mpsc::channel::<(POINT, bool)>();
    let (tell, told) = mpsc::channel::<Option<RECT>>();
    let aim: Arc<Mutex<(u64, Option<RECT>)>> = Arc::new(Mutex::new((0, None)));
    let aim2 = aim.clone();
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let uia: Option<IUIAutomation> = CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER).or_else(|_| CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)).ok();
        if let Some(u2) = uia.as_ref().and_then(|u| u.cast::<IUIAutomation2>().ok()) {
            let _ = u2.SetTransactionTimeout(600);
            let _ = u2.SetConnectionTimeout(500);
        }
        let walker = uia.as_ref().and_then(|u| u.ControlViewWalker().ok());
        let own = std::process::id();
        while let Ok(mut m) = asks.recv() {
            while let Ok(n) = asks.try_recv() {
                m = n;
            }
            let r = match (&uia, &walker) {
                (Some(u), Some(w)) => typing_area(u, w, m.0, own),
                _ => None,
            };
            if m.1 {
                let _ = tell.send(r);
                return;
            }
            let mut a = aim2.lock().unwrap();
            *a = (a.0 + 1, r);
        }
    });
    std::thread::spawn(move || unsafe {
        let main = HWND(main as *mut _);
        let to_page = |x: i32, y: i32| {
            let mut w = RECT::default();
            let _ = GetWindowRect(main, &mut w);
            let s = crate::hit::scale_of(main);
            ((x - w.left) as f64 / s, (y - w.top) as f64 / s)
        };
        let rect = |r: RECT| {
            let (l, t) = to_page(r.left, r.top);
            let (rr, b) = to_page(r.right, r.bottom);
            [l, t, rr - l, b - t]
        };
        let (mut last, mut seen) = ((i32::MIN, i32::MIN), u64::MAX);
        loop {
            let mut p = POINT::default();
            let _ = GetCursorPos(&mut p);
            let esc = GetAsyncKeyState(VK_ESCAPE.0 as i32) < 0;
            if esc || GetAsyncKeyState(VK_LBUTTON.0 as i32) >= 0 {
                let at = if esc {
                    None
                } else {
                    let _ = ask.send((p, true));
                    told.recv_timeout(Duration::from_millis(1500)).ok().flatten()
                };
                let (x, y) = to_page(p.x, p.y);
                let _ = app.emit_to("main", "words-drop", serde_json::json!({ "ok": at.is_some(), "x": x, "y": y, "esc": esc, "aim": at.map(rect) }));
                if at.is_some() {
                    // clicked where it was let go (the caret goes there; ULTRON lets the click through), then typed
                    mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
                    mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
                    std::thread::sleep(Duration::from_millis(140));
                    crate::dictate::type_out(&text);
                    crate::log_line(&format!("words: {} characters dropped into a typing area", text.chars().count()));
                }
                break;
            }
            let (g, r) = *aim.lock().unwrap();
            let moved = (p.x, p.y) != last;
            if moved {
                let _ = ask.send((p, false));
            }
            if moved || g != seen {
                let (x, y) = to_page(p.x, p.y);
                let _ = app.emit_to("main", "words-drag", serde_json::json!({ "x": x, "y": y, "aim": r.map(rect) }));
                last = (p.x, p.y);
                seen = g;
            }
            std::thread::sleep(Duration::from_millis(7));
        }
        crate::hit::DRAGGING_OUT.store(false, Ordering::SeqCst);
        ON.store(false, Ordering::SeqCst);
    });
}
