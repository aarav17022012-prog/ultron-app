//! Agent watch (the notch's Settings): coding agents (Claude Code, Cursor, VS Code, Windsurf, Codex…) asking for you,
//! with a question or for a permission, or done with what they were doing. ULTRON's character changes to say so.
//!
//! It hears of it two ways, each allowed in Settings first:
//! - the agent apps' own Windows notifications (read from Windows' list, which calls.rs looks at anyway);
//! - Claude Code's hooks, connected only when you press Connect: Claude Code then starts ULTRON's own program with
//!   `--agent` when it stops or asks for you. That hands what it said (and which window it's in) to the running
//!   ULTRON through a small file, and is gone again at once.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindow, GetWindowLongW, GetWindowTextLengthW, GetWindowThreadProcessId, IsWindowVisible, GWL_EXSTYLE,
    GW_OWNER, WS_EX_TOOLWINDOW,
};
use windows::UI::Notifications::{KnownNotificationBindings, UserNotification};

/// On in Settings; and whether agent apps' notifications count (`NOTES`), or any app's (`ALL`, a smaller nod).
static WATCH: AtomicBool = AtomicBool::new(false);
static NOTES: AtomicBool = AtomicBool::new(false);
static ALL: AtomicBool = AtomicBool::new(false);
/// The notifications already in Windows' list (the first look's are old news).
static SEEN: Mutex<Option<HashSet<u32>>> = Mutex::new(None);

/// What the page is told.
#[derive(Serialize, Clone, Default)]
pub struct Nudge {
    /// "hook" (Claude Code) or "note" (a notification)
    kind: String,
    /// Claude Code's own name for it: Notification or Stop
    event: String,
    /// the app it came from, or the one Claude Code runs in
    app: String,
    title: String,
    text: String,
    cwd: String,
    /// its window, to bring forward when you click
    hwnd: i64,
    /// that window is the one you're in already
    front: bool,
    /// one of the coding agents (false: some other app's notification)
    agent: bool,
}

/// Apps that are (or hold) coding agents, as Windows names them.
const AGENTS: [&str; 22] = [
    "claude", "cursor", "visual studio code", "vscode", "windsurf", "codex", "copilot", "chatgpt", "antigravity", "kiro", "zed", "trae", "warp",
    "jetbrains", "intellij", "pycharm", "webstorm", "rider", "android studio", "terminal", "devin", "cline",
];

pub fn set(on: bool, notes: bool, all: bool) {
    WATCH.store(on, SeqCst);
    NOTES.store(on && notes, SeqCst);
    ALL.store(on && notes && all, SeqCst);
}

/// Where `--agent` leaves what Claude Code said (not the temp folder: Claude Code runs its hooks from Git's shell,
/// which may hand programs a temp folder of its own).
fn inbox() -> PathBuf {
    std::env::var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir()).join("ULTRON").join("agent")
}

fn now_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/* ---------------- windows ---------------- */

unsafe extern "system" fn each(h: HWND, l: LPARAM) -> BOOL {
    unsafe {
        let v = &mut *(l.0 as *mut Vec<(isize, u32)>);
        let owned = GetWindow(h, GW_OWNER).is_ok_and(|o| !o.is_invalid());
        if IsWindowVisible(h).as_bool() && !owned && GetWindowTextLengthW(h) > 0 && GetWindowLongW(h, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 == 0 {
            let mut pid = 0u32;
            GetWindowThreadProcessId(h, Some(&mut pid));
            v.push((h.0 as isize, pid));
        }
    }
    true.into()
}

/// The app windows on screen, with their process ids (front to back).
fn top_windows() -> Vec<(isize, u32)> {
    let mut v: Vec<(isize, u32)> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut v as *mut _ as isize));
    }
    v
}

/// A process's program file name, in lower case ("code.exe").
fn exe_of(pid: u32) -> String {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return String::new() };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit('\\').next().unwrap_or("").to_lowercase()
    }
}

/// What to call the app a window belongs to, from its program's name.
fn app_name(exe: &str) -> String {
    match exe {
        "windowsterminal.exe" | "openconsole.exe" | "conhost.exe" => "Terminal".into(),
        "code.exe" => "VS Code".into(),
        "claude.exe" => "Claude".into(),
        _ => {
            let stem = exe.trim_end_matches(".exe");
            let mut c = stem.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        }
    }
}

/// The window an app's notification is about: one of that app's own (the first in front).
fn window_of(app: &str) -> isize {
    let a = app.to_lowercase();
    let want = if a.contains("visual studio code") || a.contains("vscode") {
        "code".to_string()
    } else if a.contains("terminal") {
        "windowsterminal".to_string()
    } else {
        a.split_whitespace().next().unwrap_or("").to_string()
    };
    if want.is_empty() {
        return 0;
    }
    top_windows().into_iter().find(|(_, pid)| exe_of(*pid).starts_with(&want)).map(|(h, _)| h).unwrap_or(0)
}

/* ---------------- Claude Code's hooks: the `--agent` program ---------------- */

/// Started by Claude Code (`ULTRON.exe --agent`), with what happened on its input. Finds the window Claude Code runs
/// in (the first of its parents with one: the terminal, VS Code, the Claude app), leaves a note for the running
/// ULTRON, and ends.
pub fn relay() {
    use std::io::{IsTerminal, Read};
    let mut s = String::new();
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let _ = stdin.lock().take(1 << 20).read_to_string(&mut s);
    }
    let v: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
    let pick = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let (hwnd, app) = host_window();
    let front = hwnd != 0 && unsafe { GetForegroundWindow() }.0 as isize == hwnd;
    let note = json!({"event": pick("hook_event_name"), "text": pick("message"), "title": pick("title"), "cwd": pick("cwd"), "hwnd": hwnd, "app": app, "front": front});
    let dir = inbox();
    let _ = std::fs::create_dir_all(&dir);
    let name = format!("{}-{}.json", now_ms(), std::process::id());
    let part = dir.join(format!("{name}.part"));
    // (written whole, then named: the running ULTRON never reads half of one)
    if std::fs::write(&part, note.to_string()).is_ok() {
        let _ = std::fs::rename(&part, dir.join(name));
    }
}

/// The first of this program's parents (Claude Code, its shell, …) that has a window on screen.
fn host_window() -> (isize, String) {
    use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
    let mut parent = std::collections::HashMap::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return (0, String::new()) };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let n = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(e.szExeFile.len());
                parent.insert(e.th32ProcessID, (e.th32ParentProcessID, String::from_utf16_lossy(&e.szExeFile[..n]).to_lowercase()));
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    let wins = top_windows();
    let mut pid = std::process::id();
    for _ in 0..16 {
        let Some((pp, _)) = parent.get(&pid) else { break };
        pid = *pp;
        let Some((_, exe)) = parent.get(&pid) else { break };
        // (as far as the desktop: Windows' own shell is no app's window)
        if pid == 0 || exe == "explorer.exe" {
            break;
        }
        if let Some((h, _)) = wins.iter().find(|(_, p)| *p == pid) {
            return (*h, app_name(exe));
        }
    }
    (0, String::new())
}

/// The running ULTRON's side: notes left by `--agent` are read (and removed) a few times a second while Agent watch is on.
pub fn start(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(if WATCH.load(SeqCst) { 300 } else { 2000 }));
        // (as it is now: turned on during the wait, the note that came meanwhile is news)
        let on = WATCH.load(SeqCst);
        let Ok(rd) = std::fs::read_dir(inbox()) else { continue };
        let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
        files.sort();
        for p in files {
            // (left while ULTRON wasn't running, or wasn't watching: old news, cleared away)
            let fresh = std::fs::metadata(&p).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|d| d < Duration::from_secs(30));
            let s = std::fs::read_to_string(&p).unwrap_or_default();
            let _ = std::fs::remove_file(&p);
            if !on || !fresh {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&s) else { continue };
            let g = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
            let n = Nudge {
                kind: "hook".into(),
                event: g("event"),
                app: g("app"),
                title: g("title"),
                text: g("text"),
                cwd: g("cwd"),
                hwnd: v.get("hwnd").and_then(|x| x.as_i64()).unwrap_or(0),
                front: v.get("front").and_then(|x| x.as_bool()).unwrap_or(false),
                agent: true,
            };
            let _ = app.emit_to("main", "agent", &n);
        }
    });
}

/* ---------------- the agent apps' notifications ---------------- */

/// Each look at Windows' list (calls.rs): what's new from an agent app (or, if allowed, from any app) goes to the page.
pub fn from_notes(app: &AppHandle, list: &[UserNotification]) {
    let ids: HashSet<u32> = list.iter().filter_map(|n| n.Id().ok()).collect();
    let fresh: Vec<&UserNotification> = {
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        let Some(old) = seen.as_mut() else {
            *seen = Some(ids);
            return;
        };
        let fresh = if NOTES.load(SeqCst) { list.iter().filter(|n| n.Id().is_ok_and(|i| !old.contains(&i))).collect() } else { Vec::new() };
        *old = ids;
        fresh
    };
    for n in fresh {
        let info = n.AppInfo().ok();
        let name = info.as_ref().and_then(|a| a.DisplayInfo().ok()).and_then(|d| d.DisplayName().ok()).map(|s| s.to_string()).unwrap_or_default();
        let aumid = info.as_ref().and_then(|a| a.AppUserModelId().ok()).map(|s| s.to_string()).unwrap_or_default();
        if name.eq_ignore_ascii_case("ULTRON") {
            continue;
        }
        let both = format!("{name} {aumid}").to_lowercase();
        let agent = AGENTS.iter().any(|a| both.contains(a));
        if !agent && !ALL.load(SeqCst) {
            continue;
        }
        let texts: Vec<String> = n
            .Notification()
            .and_then(|x| x.Visual())
            .and_then(|v| KnownNotificationBindings::ToastGeneric().and_then(|k| v.GetBinding(&k)))
            .and_then(|b| b.GetTextElements())
            .map(|els| els.into_iter().filter_map(|e| e.Text().ok().map(|t| t.to_string())).filter(|t| !t.trim().is_empty()).collect())
            .unwrap_or_default();
        let hwnd = if agent { window_of(&name) } else { 0 };
        let nudge = Nudge {
            kind: "note".into(),
            event: "Notification".into(),
            app: name,
            title: texts.first().cloned().unwrap_or_default(),
            text: texts.get(1..).map(|t| t.join(" ")).unwrap_or_default(),
            hwnd: hwnd as i64,
            front: hwnd != 0 && unsafe { GetForegroundWindow() }.0 as isize == hwnd,
            agent,
            ..Default::default()
        };
        let _ = app.emit_to("main", "agent", &nudge);
    }
}

/* ---------------- connecting Claude Code ---------------- */

fn settings_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var("USERPROFILE").ok()?).join(".claude").join("settings.json"))
}

/// A hook command that is ULTRON's.
fn ours(cmd: &str) -> bool {
    cmd.contains("--agent") && cmd.to_lowercase().contains("ultron")
}

/// Claude Code's settings have ULTRON's hooks in them.
pub fn hooks_state() -> bool {
    let Some(text) = settings_path().and_then(|p| std::fs::read_to_string(p).ok()) else { return false };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return false };
    v.pointer("/hooks/Stop").and_then(|a| a.as_array()).is_some_and(|a| {
        a.iter().any(|m| m.get("hooks").and_then(|h| h.as_array()).is_some_and(|hs| hs.iter().any(|h| h.get("command").and_then(|c| c.as_str()).is_some_and(ours))))
    })
}

/// Add ULTRON's two hooks to Claude Code's settings (when it stops, when it asks for you), or take them out again.
/// Only ever from the button you press. Everything else in the file is left as it was; a copy of it from before the
/// first change is kept beside it.
pub fn hooks_set(on: bool) -> Result<bool, String> {
    let path = settings_path().ok_or("Couldn't find your user folder.")?;
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut v: Value = if text.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&text).map_err(|_| "Claude Code's settings file couldn't be read, so it wasn't changed.".to_string())?
    };
    let root = v.as_object_mut().ok_or("Claude Code's settings file isn't in the usual form, so it wasn't changed.")?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // (forward slashes: Claude Code runs it from a Unix-style shell on Windows, where back slashes are special)
    let cmd = format!("\"{}\" --agent", exe.to_string_lossy().replace('\\', "/"));
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let h = hooks.as_object_mut().ok_or("Claude Code's hooks aren't in the usual form, so nothing was changed.")?;
    for ev in ["Notification", "Stop"] {
        let list = h.entry(ev).or_insert_with(|| json!([]));
        let Some(a) = list.as_array_mut() else { continue };
        // ULTRON's own out first (one left from an ULTRON elsewhere too), then put back if wanted
        let mut emptied = Vec::new();
        for (i, m) in a.iter_mut().enumerate() {
            if let Some(hs) = m.get_mut("hooks").and_then(|x| x.as_array_mut()) {
                let before = hs.len();
                hs.retain(|x| !x.get("command").and_then(|c| c.as_str()).is_some_and(ours));
                if before > 0 && hs.is_empty() {
                    emptied.push(i);
                }
            }
        }
        for i in emptied.into_iter().rev() {
            a.remove(i);
        }
        if on {
            a.push(json!({"hooks": [{"type": "command", "command": cmd, "timeout": 10}]}));
        }
        if a.is_empty() {
            h.remove(ev);
        }
    }
    if h.is_empty() {
        root.remove("hooks");
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let keep = path.with_extension("json.before-ultron");
    if !text.is_empty() && !keep.exists() {
        let _ = std::fs::write(&keep, &text);
    }
    let out = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())? + "\n";
    let part = path.with_extension("json.ultron-part");
    std::fs::write(&part, out).map_err(|e| e.to_string())?;
    std::fs::rename(&part, &path).map_err(|e| e.to_string())?;
    Ok(hooks_state())
}
