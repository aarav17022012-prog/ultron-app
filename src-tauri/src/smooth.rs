//! Zero-jank: the processes that draw ULTRON (its WebView2 browser, GPU and page processes)
//! get a little more CPU priority than normal, and its heavy workers (the brain, voice, song
//! listening) a little less. So however hard ULTRON thinks, every animation frame still lands.
//! Nothing on screen changes; it's only who goes first when the CPU is busy.
//! The same goes for the graphics card: ULTRON's drawing gets a higher GPU priority than normal,
//! and the quick brain (when it runs on the card) a lower one, so frames never wait for tokens.

use std::collections::HashMap;
use std::time::Duration;

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetPriorityClass, OpenProcess, SetPriorityClass, ABOVE_NORMAL_PRIORITY_CLASS, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
};

/// GPU scheduling priority classes (D3DKMT_SCHEDULINGPRIORITYCLASS)
pub const GPU_BELOW_NORMAL: i32 = 1;
pub const GPU_ABOVE_NORMAL: i32 = 3;

/// A function from a system DLL, looked up by name.
pub fn proc_addr(dll: &str, name: &str) -> Option<unsafe extern "system" fn() -> isize> {
    use windows::core::{PCSTR, PCWSTR};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    let w: Vec<u16> = dll.encode_utf16().chain(Some(0)).collect();
    let n: Vec<u8> = name.bytes().chain(Some(0)).collect();
    unsafe {
        let h = LoadLibraryW(PCWSTR(w.as_ptr())).ok()?;
        GetProcAddress(h, PCSTR(n.as_ptr()))
    }
}

/// Set a process's priority on the graphics card. Returns true when Windows accepted it.
pub fn gpu_priority(pid: u32, class: i32) -> bool {
    let Some(f) = proc_addr("gdi32.dll", "D3DKMTSetProcessSchedulingPriorityClass") else { return false };
    let f: unsafe extern "system" fn(windows::Win32::Foundation::HANDLE, i32) -> i32 = unsafe { std::mem::transmute(f) };
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return false };
        let ok = f(h, class) >= 0;
        let _ = CloseHandle(h);
        ok
    }
}

/// Hand a process's idle memory back to Windows. A brain's server keeps the model file mapped in
/// memory even after it has made its own faster copy of it (or moved it onto the graphics card):
/// gigabytes that are never read again. Trimming costs nothing: anything it does still need comes
/// straight back, and the rest becomes free memory for everything else on the PC.
pub fn trim(pid: u32) {
    use windows::Win32::System::Threading::{SetProcessWorkingSetSize, PROCESS_SET_QUOTA};
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_SET_QUOTA | PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            let _ = SetProcessWorkingSetSize(h, usize::MAX, usize::MAX);
            let _ = CloseHandle(h);
        }
    }
}

/// (pid, parent pid, exe name) for every process.
fn processes() -> Vec<(u32, u32, String)> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let n = String::from_utf16_lossy(&e.szExeFile);
                out.push((e.th32ProcessID, e.th32ParentProcessID, n.trim_end_matches('\0').to_lowercase()));
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

fn boost_renderers() {
    let me = unsafe { GetCurrentProcessId() };
    let list = processes();
    let parent: HashMap<u32, u32> = list.iter().map(|(p, pp, _)| (*p, *pp)).collect();
    let ours = |mut p: u32| {
        for _ in 0..6 {
            match parent.get(&p) {
                Some(&pp) if pp == me => return true,
                Some(&pp) if pp != 0 && pp != p => p = pp,
                _ => return false,
            }
        }
        false
    };
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let mut gpu_ok = 0;
    for (pid, _, name) in &list {
        if name == "msedgewebview2.exe" && ours(*pid) {
            if gpu_priority(*pid, GPU_ABOVE_NORMAL) {
                gpu_ok += 1;
            }
            unsafe {
                if let Ok(h) = OpenProcess(PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION, false, *pid) {
                    if GetPriorityClass(h) != ABOVE_NORMAL_PRIORITY_CLASS.0 {
                        let _ = SetPriorityClass(h, ABOVE_NORMAL_PRIORITY_CLASS);
                    }
                    let _ = CloseHandle(h);
                }
            }
        }
    }
    if !LOGGED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        crate::log_line(&format!("smooth: {gpu_ok} drawing processes got a higher GPU priority"));
    }
}

/* ---------------- idle memory ----------------
   Measured: with nothing open, ULTRON's processes hold about 1.3 GB, but in the 15 seconds after their working sets
   are trimmed they touch only about 130 MB again. The rest (the brain's buffers, the page's caches, a model nobody
   is talking to) just sits there. So once nothing has been open for a while, the page asks WebView2 for its low
   memory level (its own caches and spare memory go back) and ULTRON's processes are trimmed, again every few
   minutes while nothing opens. Whatever is needed comes back as it's used; the moment something opens, the page
   goes back to its normal level. */

static IDLE_GEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// ULTRON itself and every process it started (the page's, the brains'), not the taskbar guard.
fn ours() -> Vec<u32> {
    let me = unsafe { GetCurrentProcessId() };
    let list = processes();
    let parent: HashMap<u32, u32> = list.iter().map(|(p, pp, _)| (*p, *pp)).collect();
    let mut out = vec![me];
    for (pid, _, _) in &list {
        let mut p = *pid;
        for _ in 0..6 {
            match parent.get(&p) {
                Some(&pp) if pp == me => {
                    out.push(*pid);
                    break;
                }
                Some(&pp) if pp != 0 && pp != p => p = pp,
                _ => break,
            }
        }
    }
    out
}

/// The page says whether anything is open (false) or it's been quiet a while (true).
pub fn idle(app: &tauri::AppHandle, on: bool) {
    let gen = IDLE_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    memory_level(app, on);
    if !on {
        return;
    }
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        let mut n = 0;
        while IDLE_GEN.load(std::sync::atomic::Ordering::SeqCst) == gen {
            for pid in ours() {
                trim(pid);
            }
            if n == 0 {
                crate::log_line("idle: memory handed back (nothing open)");
            }
            n += 1;
            // (and again now and then: what the bar's clock and the character touch creeps back in)
            for _ in 0..(4 * 60) {
                if IDLE_GEN.load(std::sync::atomic::Ordering::SeqCst) != gen {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });
}

/// WebView2's own memory level for the page: low while nothing is open, normal otherwise.
fn memory_level(app: &tauri::AppHandle, low: bool) {
    use tauri::Manager;
    use webview2_com::Microsoft::Web::WebView2::Win32::{ICoreWebView2_19, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL};
    use wcore62::Interface;
    let Some(w) = app.get_webview_window("main") else { return };
    let _ = w.with_webview(move |pv| unsafe {
        let Ok(core) = pv.controller().CoreWebView2() else { return };
        let Ok(c19) = core.cast::<ICoreWebView2_19>() else { return };
        let _ = c19.SetMemoryUsageTargetLevel(if low { COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW } else { COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL });
    });
}

/// Keep ULTRON's drawing ahead (new page processes appear when windows open, so check now and then).
pub fn start() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(2));
        loop {
            boost_renderers();
            std::thread::sleep(Duration::from_secs(15));
        }
    });
}
