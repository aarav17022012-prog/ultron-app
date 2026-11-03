//! Cleaner's background apps: programs of yours running with no window open, holding memory. Listed by app (all of
//! its processes together), largest first. Windows' own parts, its services, drivers' helpers, security software and
//! SYLVER itself are never listed. Nothing is ended unless you choose it in Settings › Cleaner.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, TerminateProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, PROCESS_VM_READ};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextLengthW, GetWindowThreadProcessId, IsWindowVisible};

#[derive(Serialize, Clone)]
pub struct Group {
    pub name: String,
    pub path: String,
    pub mb: u64,
    pub count: u32,
}

/// Programs that keep the PC itself working (or are SYLVER's own): never listed, whatever folder they're in.
const KEEP: [&str; 22] = [
    "\\windows\\", "\\windowsapps\\microsoft.windows", "windows defender", "\\microsoft\\edgewebview\\", "msedgewebview2.exe", "ultron.exe",
    "llama-server.exe", "\\nvidia corporation\\", "\\nvidia\\", "\\realtek\\", "\\intel\\", "\\amd\\", "\\lenovo\\", "\\synaptics\\", "\\dolby\\",
    "\\elan\\", "\\waves\\", "\\microsoft onedrive\\", "\\common files\\", "securityhealth", "\\microsoft\\edge\\application\\msedge_proxy", "\\ultron",
];

struct Proc {
    pid: u32,
    path: String,
}

fn session_of(pid: u32) -> Option<u32> {
    let mut s = 0u32;
    unsafe { ProcessIdToSessionId(pid, &mut s) }.ok().map(|_| s)
}

fn path_of(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut n = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut n).is_ok();
        let _ = CloseHandle(h);
        ok.then(|| String::from_utf16_lossy(&buf[..n as usize]))
    }
}

/// Memory it holds of its own (what would come back if it ended), in MB.
fn mem_of(pid: u32) -> u64 {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, false, pid) else { return 0 };
        let mut c = PROCESS_MEMORY_COUNTERS_EX { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
        let ok = K32GetProcessMemoryInfo(h, &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS, c.cb).as_bool();
        let _ = CloseHandle(h);
        if ok {
            (c.PrivateUsage as u64) >> 20
        } else {
            0
        }
    }
}

/// The processes of yours (this sign-in's), each with its program's full path; Windows' and the protected ones left out.
fn mine() -> Vec<Proc> {
    let me = unsafe { GetCurrentProcessId() };
    let Some(session) = session_of(me) else { return Vec::new() };
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let pid = e.th32ProcessID;
                if pid > 4 && pid != me && e.th32ParentProcessID != me && session_of(pid) == Some(session) {
                    if let Some(path) = path_of(pid) {
                        let low = path.to_lowercase();
                        if !KEEP.iter().any(|k| low.contains(k)) {
                            out.push(Proc { pid, path });
                        }
                    }
                }
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

unsafe extern "system" fn shown(h: HWND, lp: LPARAM) -> windows::core::BOOL {
    unsafe {
        let set = &mut *(lp.0 as *mut HashSet<u32>);
        if IsWindowVisible(h).as_bool() && GetWindowTextLengthW(h) > 0 {
            let mut pid = 0u32;
            GetWindowThreadProcessId(h, Some(&mut pid));
            set.insert(pid);
        }
    }
    true.into()
}

/// The processes that have a window on screen (their app is open: it's not in the background).
fn with_windows() -> HashSet<u32> {
    let mut set: HashSet<u32> = HashSet::new();
    unsafe {
        let _ = EnumWindows(Some(shown), LPARAM(&mut set as *mut _ as isize));
    }
    set
}

/// What's running in the background now, by app, largest first.
pub fn list() -> Vec<Group> {
    let procs = mine();
    let open = with_windows();
    let mut by: HashMap<String, (Group, bool)> = HashMap::new();
    for p in &procs {
        let key = p.path.to_lowercase();
        let e = by.entry(key).or_insert_with(|| {
            let name = crate::apps::file_description(&p.path).unwrap_or_else(|| {
                std::path::Path::new(&p.path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
            });
            (Group { name, path: p.path.clone(), mb: 0, count: 0 }, false)
        });
        e.0.mb += mem_of(p.pid);
        e.0.count += 1;
        e.1 |= open.contains(&p.pid);
    }
    let mut v: Vec<Group> = by.into_values().filter(|(g, open)| !open && g.mb >= 15).map(|(g, _)| g).collect();
    v.sort_by(|a, b| b.mb.cmp(&a.mb));
    v.truncate(30);
    v
}

/// End these apps' background processes (by program path). Any that has opened a window since is left alone.
/// How many processes ended.
pub fn end(paths: &[String]) -> u32 {
    let want: HashSet<String> = paths.iter().map(|p| p.to_lowercase()).collect();
    let open = with_windows();
    let mut n = 0;
    for p in mine() {
        if !want.contains(&p.path.to_lowercase()) || open.contains(&p.pid) {
            continue;
        }
        unsafe {
            if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, p.pid) {
                if TerminateProcess(h, 1).is_ok() {
                    n += 1;
                }
                let _ = CloseHandle(h);
            }
        }
    }
    crate::log_line(&format!("cleaner: ended {n} background processes of {} apps", paths.len()));
    n
}
