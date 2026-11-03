//! A frozen page brought back. The page says it's alive every few seconds; if it goes quiet for 45 s while SYLVER is
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

/// SYLVER started again if it crashes (a Windows update can leave a driver, or the web engine under SYLVER, in a state
/// that brings it down). A small copy of SYLVER waits for this one to end; if it ended by crashing (not quit by you,
/// not ended from Task Manager, not Windows shutting down or restarting), it starts SYLVER again, three times in ten
/// minutes at most.
pub fn keep() {
    use std::os::windows::process::CommandExt;
    if let Ok(me) = std::env::current_exe() {
        const DETACHED: u32 = 0x0000_0008 | 0x0800_0000; // DETACHED_PROCESS, CREATE_NO_WINDOW
        let _ = std::process::Command::new(me).arg("--keeper").arg(std::process::id().to_string()).creation_flags(DETACHED).spawn();
    }
}

/// Called first thing in `main`: this copy was started as the keeper (true: nothing else to do).
pub fn keeper_if_asked() -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, WaitForSingleObject, INFINITE, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE};
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SHUTTINGDOWN};
    let args: Vec<String> = std::env::args().collect();
    let Some(i) = args.iter().position(|a| a == "--keeper") else { return false };
    let Some(pid) = args.get(i + 1).and_then(|p| p.parse::<u32>().ok()) else { return true };
    let code = unsafe {
        let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return true };
        WaitForSingleObject(h, INFINITE);
        let mut code = 0u32;
        let _ = GetExitCodeProcess(h, &mut code);
        let _ = CloseHandle(h);
        code
    };
    // (a crash: Windows' own codes for one, or a Rust panic on the main thread. Ended from Task Manager or by a script,
    //  the code is 1 or -1, and quitting is 0: those are left alone)
    let crashed = code >= 0xC000_0000 && code != 0xFFFF_FFFF || code == 101 || code == 0x8000_0003;
    if !crashed || unsafe { GetSystemMetrics(SM_SHUTTINGDOWN) } != 0 {
        return true;
    }
    let file = std::env::var("LOCALAPPDATA").map(std::path::PathBuf::from).unwrap_or_else(|_| std::env::temp_dir()).join("SYLVER").join("restarts.txt");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let recent: Vec<u64> = std::fs::read_to_string(&file).unwrap_or_default().lines().filter_map(|l| l.trim().parse().ok()).filter(|t: &u64| now.saturating_sub(*t) < 600).collect();
    if recent.len() >= 3 {
        return true;
    }
    let _ = std::fs::create_dir_all(file.parent().unwrap_or(std::path::Path::new(".")));
    let _ = std::fs::write(&file, recent.iter().chain(std::iter::once(&now)).map(|t| t.to_string()).collect::<Vec<_>>().join("\n"));
    std::thread::sleep(Duration::from_millis(1500));
    if let Ok(me) = std::env::current_exe() {
        let _ = std::process::Command::new(&me).current_dir(me.parent().unwrap_or(std::path::Path::new("."))).spawn();
    }
    true
}
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

/// SYLVER has no menu bar of Windows' own kind. An Alt press on its window (yours, while typing in SYLVER; or the one
/// SYLVER taps to hand the keyboard back to another app) put the window in Windows' keyboard-menu mode, waiting for a
/// menu key: the page stopped hearing from the app, and the app from the page, until Escape was pressed. Measured:
/// one Alt tap, SYLVER frozen. That menu mode is turned away here (installed from the window's own thread).
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
            // (not started yet; the PC was asleep; SYLVER is hidden; or the page is, and slows its timers on purpose)
            // (and while the laptop rests or has just woken: with the lid shut, Windows' modern standby kept this
            //  running while the page was frozen, and on opening the lid the whole page was reloaded as if stuck,
            //  just as everything else woke: the lag after a night closed)
            let away = gap > Duration::from_secs(15) || crate::lid::resting() || HIDDEN.load(Relaxed) || !unsafe { IsWindowVisible(HWND(hwnd as _)) }.as_bool();
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
