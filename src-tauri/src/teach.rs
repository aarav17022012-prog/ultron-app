//! Teach a task: "Sylver, learn how to file my report", you do it once, Stop, and "Sylver, do file my report" does it
//! again whenever you like. Nothing is filmed: SYLVER notes only what you did, step by step (the app, the button or box
//! you clicked, by its own name where it has one, the words you typed, the keys you pressed), so watching costs almost
//! nothing and doing it again needs no service or key, as often as you like. Your own presses only; SYLVER's never count.

use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyboardLayout, SendInput, ToUnicodeEx, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetAncestor, GetForegroundWindow, GetMessageW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, PostThreadMessageW,
    SetCursorPos, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WindowFromPoint, GA_ROOT, KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLMHF_INJECTED, MSG,
    MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_LBUTTONDOWN, WM_QUIT, WM_SYSKEYDOWN,
};

/// One thing you did.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Act {
    /// went to this app (its program's name, and the window's title then)
    App { exe: String, title: String },
    /// clicked: the thing's own name ("" when it has none), and where in the window, as a share of its width and height
    Click { name: String, x: f32, y: f32 },
    Type { text: String },
    Keys { vks: Vec<u16>, label: String },
}

static ON: AtomicBool = AtomicBool::new(false);
static TELL: OnceLock<Mutex<Sender<Raw>>> = OnceLock::new();
static HOOKS_THREAD: Mutex<u32> = Mutex::new(0);
static STEPS: Mutex<Vec<Act>> = Mutex::new(Vec::new());
static APP: OnceLock<AppHandle> = OnceLock::new();

enum Raw {
    Click(POINT),
    Key(u32, u32),
}

fn held(vk: i32) -> bool {
    unsafe { GetAsyncKeyState(vk) as u16 & 0x8000 != 0 }
}

unsafe extern "system" fn mouse(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 && ON.load(SeqCst) && wp.0 as u32 == WM_LBUTTONDOWN {
            let m = &*(lp.0 as *const MSLLHOOKSTRUCT);
            if m.flags & LLMHF_INJECTED == 0 {
                if let Some(t) = TELL.get() {
                    let _ = t.lock().unwrap().send(Raw::Click(m.pt));
                }
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

unsafe extern "system" fn keyboard(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let m = wp.0 as u32;
        if code >= 0 && ON.load(SeqCst) && (m == WM_KEYDOWN || m == WM_SYSKEYDOWN) {
            let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
            if k.flags.0 & LLKHF_INJECTED.0 == 0 {
                if let Some(t) = TELL.get() {
                    let _ = t.lock().unwrap().send(Raw::Key(k.vkCode, k.scanCode));
                }
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

fn exe_of(h: HWND) -> String {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    if pid == std::process::id() {
        return "SYLVER".into();
    }
    use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return String::new() };
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_FORMAT(0), windows::core::PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(h);
        if !ok {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        std::path::Path::new(&path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
    }
}

/// A left click where the pointer is.
fn click() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEINPUT};
    let m = |f| INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dwFlags: f, ..Default::default() } } };
    unsafe { SendInput(&[m(MOUSEEVENTF_LEFTDOWN), m(MOUSEEVENTF_LEFTUP)], std::mem::size_of::<INPUT>() as i32) };
}

fn title_of(h: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(h, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n])
}

fn key_label(vk: u32) -> String {
    match vk {
        0x0D => "Enter".into(),
        0x09 => "Tab".into(),
        0x1B => "Esc".into(),
        0x08 => "Backspace".into(),
        0x2E => "Delete".into(),
        0x25 => "Left".into(),
        0x26 => "Up".into(),
        0x27 => "Right".into(),
        0x28 => "Down".into(),
        0x24 => "Home".into(),
        0x23 => "End".into(),
        0x21 => "Page Up".into(),
        0x22 => "Page Down".into(),
        0x70..=0x7B => format!("F{}", vk - 0x6F),
        0x30..=0x39 | 0x41..=0x5A => ((vk as u8) as char).to_string(),
        _ => format!("key {vk}"),
    }
}

/// What's noted from the raw presses: on a thread of its own (an app can be slow to say what's under the pointer).
fn note(rx: std::sync::mpsc::Receiver<Raw>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let uia: Option<IUIAutomation> = unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok() };
    let mut text = String::new();
    let mut last_app = String::new();
    let flush = |text: &mut String| {
        if !text.is_empty() {
            STEPS.lock().unwrap().push(Act::Type { text: std::mem::take(text) });
        }
    };
    for raw in rx {
        if !ON.load(SeqCst) {
            continue;
        }
        match raw {
            Raw::Click(p) => {
                let root = unsafe { GetAncestor(WindowFromPoint(p), GA_ROOT) };
                let exe = exe_of(root);
                if exe == "SYLVER" || exe.is_empty() {
                    continue; // (SYLVER's own bar and its Stop button)
                }
                flush(&mut text);
                if exe != last_app {
                    STEPS.lock().unwrap().push(Act::App { exe: exe.clone(), title: title_of(root) });
                    last_app = exe;
                }
                let name = uia
                    .as_ref()
                    .and_then(|u| unsafe { u.ElementFromPoint(p) }.ok())
                    .and_then(|e| unsafe { e.CurrentName() }.ok())
                    .map(|n| n.to_string().trim().chars().take(80).collect::<String>())
                    .unwrap_or_default();
                let mut r = RECT::default();
                let _ = unsafe { GetWindowRect(root, &mut r) };
                let (w, h) = ((r.right - r.left).max(1) as f32, (r.bottom - r.top).max(1) as f32);
                STEPS.lock().unwrap().push(Act::Click { name, x: (p.x - r.left) as f32 / w, y: (p.y - r.top) as f32 / h });
            }
            Raw::Key(vk, scan) => {
                if exe_of(unsafe { GetForegroundWindow() }) == "SYLVER" {
                    continue; // (typed to SYLVER itself)
                }
                if matches!(vk, 0x10 | 0x11 | 0x12 | 0xA0..=0xA5 | 0x5B | 0x5C | 0x14) {
                    continue; // (Shift, Ctrl, Alt, Windows, Caps Lock on their own)
                }
                let (ctrl, alt, win) = (held(0x11), held(0x12), held(0x5B) || held(0x5C));
                if !(ctrl || alt || win) {
                    if vk == 0x08 && text.pop().is_some() {
                        continue; // (a mistake put right while typing: not a step)
                    }
                    // a character: as the keyboard layout of the app in front makes it
                    let mut state = [0u8; 256];
                    if held(0x10) {
                        state[0x10] = 0x80;
                    }
                    let mut buf = [0u16; 4];
                    let layout = unsafe { GetKeyboardLayout(GetWindowThreadProcessId(GetForegroundWindow(), None)) };
                    let n = unsafe { ToUnicodeEx(vk, scan, &state, &mut buf, 4, Some(layout)) };
                    if n > 0 && !(vk == 0x0D || vk == 0x09 || vk == 0x1B || vk == 0x08) {
                        let c = String::from_utf16_lossy(&buf[..n as usize]);
                        if c.chars().all(|c| !c.is_control()) {
                            text.push_str(&c);
                            continue;
                        }
                    }
                }
                flush(&mut text);
                let mut vks: Vec<u16> = Vec::new();
                let mut label = Vec::new();
                for (on, k, l) in [(ctrl, 0x11u16, "Ctrl"), (alt, 0x12, "Alt"), (win, 0x5B, "Win"), (held(0x10), 0x10, "Shift")] {
                    if on {
                        vks.push(k);
                        label.push(l.to_string());
                    }
                }
                vks.push(vk as u16);
                label.push(key_label(vk));
                STEPS.lock().unwrap().push(Act::Keys { vks, label: label.join("+") });
            }
        }
        if let Some(app) = APP.get() {
            let _ = app.emit_to("main", "teach-steps", STEPS.lock().unwrap().len() + usize::from(!text.is_empty()));
        }
    }
}

/// Starts watching.
pub fn start(app: &AppHandle) {
    let _ = APP.set(app.clone());
    STEPS.lock().unwrap().clear();
    if TELL.get().is_none() {
        let (tx, rx) = channel::<Raw>();
        let _ = TELL.set(Mutex::new(tx));
        std::thread::spawn(move || note(rx));
    }
    ON.store(true, SeqCst);
    // (the hooks are only there while watching)
    std::thread::spawn(|| unsafe {
        *HOOKS_THREAD.lock().unwrap() = windows::Win32::System::Threading::GetCurrentThreadId();
        let km = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), None, 0);
        let mm = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse), None, 0);
        let mut m = MSG::default();
        while GetMessageW(&mut m, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&m);
            DispatchMessageW(&m);
        }
        if let Ok(h) = km {
            let _ = UnhookWindowsHookEx(h);
        }
        if let Ok(h) = mm {
            let _ = UnhookWindowsHookEx(h);
        }
    });
}

fn dir(app: &AppHandle) -> std::path::PathBuf {
    let d = app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir()).join("taught");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn file_name(name: &str) -> String {
    let s: String = name.trim().to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
    s.trim_matches('-').chars().take(60).collect::<String>() + ".json"
}

/// Stops watching; what was done is kept under this name (nothing is kept if nothing was done, or no name is given).
pub fn stop(app: &AppHandle, name: &str) -> usize {
    ON.store(false, SeqCst);
    let t = std::mem::replace(&mut *HOOKS_THREAD.lock().unwrap(), 0);
    if t != 0 {
        let _ = unsafe { PostThreadMessageW(t, WM_QUIT, WPARAM(0), LPARAM(0)) };
    }
    std::thread::sleep(Duration::from_millis(150)); // (the last presses noted)
    let steps = std::mem::take(&mut *STEPS.lock().unwrap());
    if steps.is_empty() || name.trim().is_empty() {
        return 0;
    }
    let body = serde_json::json!({ "name": name.trim(), "steps": steps });
    let _ = std::fs::write(dir(app).join(file_name(name)), serde_json::to_string_pretty(&body).unwrap_or_default());
    crate::log_line(&format!("taught: \"{}\", {} steps", name.trim(), steps.len()));
    steps.len()
}

#[derive(Serialize)]
pub struct Taught {
    name: String,
    steps: usize,
}

pub fn list(app: &AppHandle) -> Vec<Taught> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir(app)).into_iter().flatten().flatten() {
        if let Some(v) = std::fs::read_to_string(e.path()).ok().and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) {
            out.push(Taught { name: v["name"].as_str().unwrap_or("").to_string(), steps: v["steps"].as_array().map_or(0, |a| a.len()) });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn forget(app: &AppHandle, name: &str) {
    let _ = std::fs::remove_file(dir(app).join(file_name(name)));
}

fn press(vks: &[u16]) {
    let k = |vk: u16, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() }, ..Default::default() } },
    };
    let mut ins: Vec<INPUT> = vks.iter().map(|&v| k(v, false)).collect();
    ins.extend(vks.iter().rev().map(|&v| k(v, true)));
    unsafe { SendInput(&ins, std::mem::size_of::<INPUT>() as i32) };
}

/// Does it again, step by step, telling the page how far it has got.
pub fn play(app: &AppHandle, name: &str) -> Result<usize, String> {
    let text = std::fs::read_to_string(dir(app).join(file_name(name))).map_err(|_| format!("I haven't been taught \"{name}\"."))?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let steps: Vec<Act> = serde_json::from_value(v["steps"].clone()).map_err(|e| e.to_string())?;
    let mut sess = crate::agent::Session::new();
    for (i, s) in steps.iter().enumerate() {
        let _ = app.emit_to("main", "teach-play", (i + 1, steps.len()));
        match s {
            Act::App { exe, .. } => {
                if sess.run(app, crate::agent::Step::SwitchTo(exe.clone())).is_err() {
                    sess.run(app, crate::agent::Step::Open(exe.clone()))?;
                }
                std::thread::sleep(Duration::from_millis(900));
            }
            Act::Click { name, x, y } => {
                // by its own name first; where it was in the window, if it has none or can't be found
                if name.is_empty() || sess.run(app, crate::agent::Step::Click(name.clone())).is_err() {
                    let mut r = RECT::default();
                    unsafe {
                        let _ = GetWindowRect(GetForegroundWindow(), &mut r);
                        let _ = SetCursorPos(r.left + ((r.right - r.left) as f32 * x) as i32, r.top + ((r.bottom - r.top) as f32 * y) as i32);
                    }
                    click();
                }
            }
            Act::Type { text } => {
                crate::commands::type_text(text);
            }
            Act::Keys { vks, .. } => press(vks),
        }
        std::thread::sleep(Duration::from_millis(380));
    }
    Ok(steps.len())
}
