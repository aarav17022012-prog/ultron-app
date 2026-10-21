//! A frozen page brought back. The page says it's alive every few seconds; if it goes quiet for 45 s while ULTRON is
//! on screen (and the page isn't hidden, when its timers are slowed on purpose), the log says which side stopped
//! answering (the page, or the app's own main thread) and the page is reloaded. At most three times in ten minutes.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tauri::Manager;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{IsWindowVisible, WM_SYSCOMMAND};

static START: OnceLock<Instant> = OnceLock::new();
static PAGE: AtomicU64 = AtomicU64::new(0);
static MAIN: AtomicU64 = AtomicU64::new(0);
static HIDDEN: AtomicBool = AtomicBool::new(false);

fn now() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// The page, every few seconds (and when it's hidden or shown).
pub fn alive(hidden: bool) {
    HIDDEN.store(hidden, Relaxed);
    PAGE.store(now(), Relaxed);
}

/// ULTRON has no menu bar of Windows' own kind. An Alt press on its window (yours, while typing in ULTRON; or the one
/// ULTRON taps to hand the keyboard back to another app) put the window in Windows' keyboard-menu mode, waiting for a
/// menu key: the page stopped hearing from the app, and the app from the page, until Escape was pressed. Measured:
/// one Alt tap, ULTRON frozen. That menu mode is turned away here (installed from the window's own thread).
unsafe extern "system" fn no_menu(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _: usize, _: usize) -> LRESULT {
    if msg == WM_SYSCOMMAND && (wp.0 & 0xFFF0) == 0xF100 {
        return LRESULT(0); // (SC_KEYMENU)
    }
    unsafe { DefSubclassProc(h, msg, wp, lp) }
}

pub fn no_alt_menu(h: HWND) {
    unsafe {
        let _ = SetWindowSubclass(h, Some(no_menu), 0x0A17, 0);
    }
}

pub fn start(app: tauri::AppHandle) {
    if let Some(h) = app.get_webview_window("main").as_ref().and_then(crate::hit::hwnd_of) {
        no_alt_menu(h);
    }
    let Some(hwnd) = app.get_webview_window("main").as_ref().and_then(crate::hit::hwnd_of) else { return };
    let hwnd = hwnd.0 as isize;
    std::thread::spawn(move || {
        let mut tick = Instant::now();
        let mut reloads: Vec<u64> = Vec::new();
        loop {
            std::thread::sleep(Duration::from_secs(5));
            let gap = tick.elapsed();
            tick = Instant::now();
            let _ = app.run_on_main_thread(|| MAIN.store(now(), Relaxed));
            let page = PAGE.load(Relaxed);
            // (not started yet; the PC was asleep; ULTRON is hidden; or the page is, and slows its timers on purpose)
            let away = gap > Duration::from_secs(15) || HIDDEN.load(Relaxed) || !unsafe { IsWindowVisible(HWND(hwnd as _)) }.as_bool();
            if page == 0 || away {
                if page != 0 {
                    PAGE.store(now(), Relaxed);
                }
                continue;
            }
            let quiet = now().saturating_sub(page);
            if quiet < 45_000 {
                continue;
            }
            let main = now().saturating_sub(MAIN.load(Relaxed)) / 1000;
            // (the page writes the calls to the app that haven't been answered into its address: see invoke in the page)
            let calls = app.get_webview_window("main").and_then(|w| w.url().ok()).and_then(|u| u.fragment().map(str::to_string)).unwrap_or_default();
            crate::log_line(&format!("watchdog: unanswered calls: {}", if calls.is_empty() { "none" } else { &calls }));
            reloads.retain(|t| now().saturating_sub(*t) < 600_000);
            if reloads.len() >= 3 {
                crate::log_line(&format!("watchdog: the page has been silent {} s (main thread answered {main} s ago); reloaded 3 times already, leaving it", quiet / 1000));
                PAGE.store(now(), Relaxed);
                continue;
            }
            crate::log_line(&format!("watchdog: the page has been silent {} s (main thread answered {main} s ago): reloading it", quiet / 1000));
            reloads.push(now());
            PAGE.store(now(), Relaxed);
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.reload();
            }
        }
    });
}
