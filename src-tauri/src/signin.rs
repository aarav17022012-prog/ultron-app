//! Windows asking for its sign-in when the PC wakes, on or off (Settings > Look & feel > Lock screen, when SYLVER's is
//! chosen: off, only SYLVER's lock screen comes up as the PC wakes). It's what "If you've been away, when should Windows
//! require you to sign in again?" sets: the power setting CONSOLELOCK (sleep), and DelayLockInterval (PCs that stay
//! connected while asleep). Both are the signed-in person's own; no administrator is needed. What they were before is
//! kept, and put back when it's turned on again. Restarts and Win L still go by Windows' sign-in.

use std::os::windows::process::CommandExt;
use std::process::Command;

const NO_WINDOW: u32 = 0x0800_0000;

fn kept() -> std::path::PathBuf {
    std::env::var("LOCALAPPDATA").map(std::path::PathBuf::from).unwrap_or_else(|_| std::env::temp_dir()).join("SYLVER").join("signin-on-wake-was.txt")
}

fn powercfg(args: &[&str]) -> Option<String> {
    let o = Command::new("powercfg").args(args).creation_flags(NO_WINDOW).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).to_string())
}

/// (AC, DC): 1 asks on wake, 0 doesn't
fn console_lock() -> Option<(u32, u32)> {
    let out = powercfg(&["/qh", "SCHEME_CURRENT", "SUB_NONE", "CONSOLELOCK"])?;
    let vals: Vec<u32> = out.lines().filter(|l| l.contains("Power Setting Index")).filter_map(|l| l.rsplit("0x").next().and_then(|h| u32::from_str_radix(h.trim(), 16).ok())).collect();
    Some((*vals.first()?, *vals.get(1).unwrap_or(vals.first()?)))
}

fn set_console_lock(ac: u32, dc: u32) -> bool {
    powercfg(&["/SETACVALUEINDEX", "SCHEME_CURRENT", "SUB_NONE", "CONSOLELOCK", &ac.to_string()]).is_some()
        && powercfg(&["/SETDCVALUEINDEX", "SCHEME_CURRENT", "SUB_NONE", "CONSOLELOCK", &dc.to_string()]).is_some()
        && powercfg(&["/SETACTIVE", "SCHEME_CURRENT"]).is_some()
}

fn delay_lock() -> Option<String> {
    let o = Command::new("reg").args(["query", r"HKCU\Control Panel\Desktop", "/v", "DelayLockInterval"]).creation_flags(NO_WINDOW).output().ok()?;
    let s = String::from_utf8_lossy(&o.stdout).to_string();
    s.lines().find(|l| l.contains("DelayLockInterval")).and_then(|l| l.split_whitespace().last()).map(str::to_string)
}

fn set_delay_lock(v: Option<&str>) {
    let mut c = Command::new("reg");
    match v {
        Some(v) => c.args(["add", r"HKCU\Control Panel\Desktop", "/v", "DelayLockInterval", "/t", "REG_DWORD", "/d", v, "/f"]),
        None => c.args(["delete", r"HKCU\Control Panel\Desktop", "/v", "DelayLockInterval", "/f"]),
    };
    let _ = c.creation_flags(NO_WINDOW).output();
}

/// Whether Windows asks for its sign-in as the PC wakes (None: couldn't tell).
pub fn asks() -> Option<bool> {
    console_lock().map(|(ac, dc)| ac != 0 || dc != 0)
}

/// Windows' sign-in on wake on (`true`: as it was before SYLVER turned it off) or off. Gives what it is now.
pub fn set(on: bool) -> Option<bool> {
    if on {
        let was = std::fs::read_to_string(kept()).unwrap_or_default();
        let mut p = was.split(',');
        let ac = p.next().and_then(|v| v.trim().parse().ok()).unwrap_or(1);
        let dc = p.next().and_then(|v| v.trim().parse().ok()).unwrap_or(1);
        let delay = p.next().map(str::trim).filter(|v| !v.is_empty() && *v != "-");
        set_console_lock(ac, dc);
        set_delay_lock(delay);
        let _ = std::fs::remove_file(kept());
        crate::log_line("lock screen: Windows' sign-in on wake put back");
    } else {
        if !kept().exists() {
            let (ac, dc) = console_lock().unwrap_or((1, 1));
            let delay = delay_lock().unwrap_or_else(|| "-".into());
            let _ = std::fs::create_dir_all(kept().parent().unwrap());
            let _ = std::fs::write(kept(), format!("{ac},{dc},{delay}"));
        }
        let ok = set_console_lock(0, 0);
        set_delay_lock(Some("0xffffffff"));
        crate::log_line(&format!("lock screen: Windows' sign-in on wake turned off ({})", if ok { "done" } else { "Windows refused" }));
    }
    asks()
}
