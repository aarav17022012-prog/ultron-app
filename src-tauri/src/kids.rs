//! ULTRON's helper processes (the language models' servers, the voice engine) must not outlive it.
//!
//! They used to: closing ULTRON, or restarting it, left its model servers running with nobody to talk
//! to, each holding a gigabyte or more of memory and, for the quick brain, of graphics memory. After a
//! day of restarts the PC was at its memory limit and everything on it lagged.
//!
//! Two things stop that. At start-up, helpers left behind by an ULTRON that is gone are stopped. And
//! every helper ULTRON starts is put in a group that Windows closes when ULTRON goes, however it goes
//! (closed, restarted, crashed, ended from Task Manager).

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
use windows::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE};
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, TerminateProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

const HELPERS: [&str; 2] = ["llama-server.exe", "piper.exe"];

/// Every process running: (its number, its parent's number, its file name in small letters).
pub(crate) fn processes() -> Vec<(u32, u32, String)> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let n = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(e.szExeFile.len());
                out.push((e.th32ProcessID, e.th32ParentProcessID, String::from_utf16_lossy(&e.szExeFile[..n]).to_lowercase()));
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

fn path_of(h: HANDLE) -> String {
    let mut buf = [0u16; 700];
    let mut n = buf.len() as u32;
    unsafe {
        match QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut n) {
            Ok(()) => String::from_utf16_lossy(&buf[..n as usize]).to_lowercase(),
            Err(_) => String::new(),
        }
    }
}

/// At start-up (only one ULTRON runs at a time, and this one has started nothing yet): any helper that
/// runs from an ULTRON folder was left behind, and is stopped. Other people's llama-server is not touched.
pub fn sweep() {
    let me = std::process::id();
    let mut stopped = 0;
    for (pid, parent, name) in processes() {
        if parent == me || !HELPERS.contains(&name.as_str()) {
            continue;
        }
        unsafe {
            let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE, false, pid) else { continue };
            if path_of(h).contains("ultron") && TerminateProcess(h, 1).is_ok() {
                stopped += 1;
            }
            let _ = CloseHandle(h);
        }
    }
    if stopped > 0 {
        crate::log_line(&format!("start-up: stopped {stopped} helper process(es) an earlier ULTRON had left running"));
    }
}

/// From now on each helper ULTRON starts is tied to it. (Looked for every few seconds rather than at each
/// place one is started: nothing can be forgotten that way. Apps ULTRON opens for you are not helpers
/// and are left alone: they stay open when ULTRON closes.)
pub fn watch() {
    std::thread::spawn(|| unsafe {
        let Ok(job) = CreateJobObjectW(None, PCWSTR::null()) else { return };
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(job, JobObjectExtendedLimitInformation, &info as *const _ as *const core::ffi::c_void, std::mem::size_of_val(&info) as u32).is_err() {
            return;
        }
        let me = std::process::id();
        let mut tied: Vec<u32> = Vec::new();
        loop {
            for (pid, parent, name) in processes() {
                if parent == me && HELPERS.contains(&name.as_str()) && !tied.contains(&pid) {
                    if let Ok(h) = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid) {
                        let _ = AssignProcessToJobObject(job, h);
                        let _ = CloseHandle(h);
                    }
                    tied.push(pid);
                }
            }
            if tied.len() > 200 {
                tied.drain(..100);
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    });
}
