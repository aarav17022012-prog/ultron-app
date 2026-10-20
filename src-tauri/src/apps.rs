//! What the voice commands and the menu bar need to know about apps: the
//! Start-menu app list, the app in front, launching, focusing, and closing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::RwLock;
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};
use windows::core::{Interface, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Storage::EnhancedStorage::{PKEY_AppUserModel_ID, PKEY_Link_TargetParsingPath};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::Storage::Packaging::Appx::GetApplicationUserModelId;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_MENU,
};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, IEnumShellItems, IShellItem, IShellItem2, SHCreateItemFromParsingName, ShellExecuteW,
    SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GetAncestor, GetClassNameW, GetForegroundWindow, GetWindow, GetWindowLongW,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, PostMessageW, SetForegroundWindow,
    ShowWindow, GA_ROOTOWNER, GWL_EXSTYLE, GW_OWNER, SW_RESTORE, SW_SHOWNORMAL, WM_CLOSE, WS_EX_APPWINDOW,
    WS_EX_TOOLWINDOW,
};

#[derive(Clone, Debug)]
pub struct AppEntry {
    pub name: String,
    pub key: String,
    pub launch: String,
}

/// A window that's open, as ULTRON's taskbar shows it. `launch` is what starts its app (the same
/// string the Start list and its pins use, when the app is in that list; else the program's path).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct OpenWin {
    pub id: String,
    pub title: String,
    pub name: String,
    pub launch: String,
    pub active: bool,
    pub min: bool,
}

#[derive(Default)]
pub struct Apps {
    pub index: RwLock<Vec<AppEntry>>,
    last_external_fg: AtomicIsize,
}

/* ---------------- small Win32 helpers ---------------- */

fn pwstr_take(p: PWSTR) -> String {
    let s = unsafe { p.to_string().unwrap_or_default() };
    unsafe { CoTaskMemFree(Some(p.0 as _)) };
    s
}

fn window_pid(h: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    pid
}

fn class_name(h: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// (exe path, packaged app id) of a process
fn process_identity(pid: u32) -> Option<(String, Option<String>)> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let exe = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
            .ok()
            .map(|_| String::from_utf16_lossy(&buf[..len as usize]));
        let mut alen = 0u32;
        let _ = GetApplicationUserModelId(h, &mut alen, None);
        let aumid = if alen > 0 {
            let mut abuf = vec![0u16; alen as usize];
            if GetApplicationUserModelId(h, &mut alen, Some(PWSTR(abuf.as_mut_ptr()))).0 == 0 {
                Some(String::from_utf16_lossy(&abuf[..(alen as usize).saturating_sub(1)]))
            } else {
                None
            }
        } else {
            None
        };
        let _ = CloseHandle(h);
        Some((exe?, aumid))
    }
}

fn file_description(exe: &str) -> Option<String> {
    unsafe {
        let path = HSTRING::from(exe);
        let size = GetFileVersionInfoSizeW(&path, None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(&path, None, size, data.as_mut_ptr() as _).ok()?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        if !VerQueryValueW(data.as_ptr() as _, &HSTRING::from("\\VarFileInfo\\Translation"), &mut ptr, &mut len).as_bool()
            || len < 4
        {
            return None;
        }
        let tr = std::slice::from_raw_parts(ptr as *const u16, 2);
        let sub = format!("\\StringFileInfo\\{:04x}{:04x}\\FileDescription", tr[0], tr[1]);
        if !VerQueryValueW(data.as_ptr() as _, &HSTRING::from(sub), &mut ptr, &mut len).as_bool() || len == 0 {
            return None;
        }
        let s = std::slice::from_raw_parts(ptr as *const u16, len as usize);
        let s = String::from_utf16_lossy(s).trim_end_matches('\0').trim().to_string();
        (!s.is_empty()).then_some(s)
    }
}

fn exe_stem(exe: &str) -> String {
    let stem = std::path::Path::new(exe).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let mut c = stem.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or(stem)
}

pub fn key_for(exe: Option<&str>, aumid: Option<&str>) -> String {
    if let Some(a) = aumid {
        if a.eq_ignore_ascii_case("Microsoft.Windows.Explorer") {
            return "exe:c:\\windows\\explorer.exe".into();
        }
        return format!("aumid:{}", a.to_lowercase());
    }
    format!("exe:{}", exe.unwrap_or_default().to_lowercase())
}

/* ---------------- launching ---------------- */

pub fn shell_open(target: &str) -> bool {
    unsafe {
        // (the app that opens may come to the front: ULTRON was just clicked, so it may hand that on)
        let _ = windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(windows::Win32::UI::WindowsAndMessaging::ASFW_ANY);
        // An app from the Start list (shell:AppsFolder\ and its id) is opened here, in ULTRON, not by starting another
        // explorer.exe to open it: that extra Explorer coming up was what made the screen stutter as an app opened.
        // A Store app is started by Windows' app activation; any other by the shell, which knows the id.
        if let Some(id) = target.strip_prefix("shell:AppsFolder\\") {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            if id.contains('!') {
                use windows::Win32::UI::Shell::{ApplicationActivationManager, IApplicationActivationManager, AO_NONE};
                let m: windows::core::Result<IApplicationActivationManager> =
                    windows::Win32::System::Com::CoCreateInstance(&ApplicationActivationManager, None, windows::Win32::System::Com::CLSCTX_LOCAL_SERVER);
                if let Ok(m) = m {
                    if m.ActivateApplication(&HSTRING::from(id), PCWSTR::null(), AO_NONE).is_ok() {
                        return true;
                    }
                }
            }
            if ShellExecuteW(None, &HSTRING::from("open"), &HSTRING::from(target), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL).0 as isize > 32 {
                return true;
            }
        }
        let r = if target.starts_with("shell:") || target.starts_with("::") {
            ShellExecuteW(None, &HSTRING::from("open"), &HSTRING::from("explorer.exe"), &HSTRING::from(target), PCWSTR::null(), SW_SHOWNORMAL)
        } else {
            ShellExecuteW(None, &HSTRING::from("open"), &HSTRING::from(target), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL)
        };
        r.0 as isize > 32
    }
}

/// Press a key combination (modifiers first, released in reverse).
pub fn send_keys(keys: &[VIRTUAL_KEY]) {
    let k = |vk: VIRTUAL_KEY, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() }, ..Default::default() } },
    };
    let mut v: Vec<INPUT> = keys.iter().map(|vk| k(*vk, false)).collect();
    v.extend(keys.iter().rev().map(|vk| k(*vk, true)));
    unsafe { SendInput(&v, std::mem::size_of::<INPUT>() as i32) };
}

fn tap_alt() {
    let key = |up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VK_MENU, dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() }, ..Default::default() },
        },
    };
    unsafe { SendInput(&[key(false), key(true)], std::mem::size_of::<INPUT>() as i32) };
}

pub fn focus_window(h: HWND) {
    unsafe {
        if IsIconic(h).as_bool() {
            let _ = ShowWindow(h, SW_RESTORE);
        }
        if !SetForegroundWindow(h).as_bool() {
            // Windows only lets the foreground process hand focus over; an Alt tap unlocks it
            tap_alt();
            let _ = SetForegroundWindow(h);
        }
    }
}

/* ---------------- running windows ---------------- */

unsafe extern "system" fn collect(h: HWND, lp: LPARAM) -> windows::core::BOOL {
    let v = unsafe { &mut *(lp.0 as *mut Vec<HWND>) };
    v.push(h);
    true.into()
}

fn is_app_window(h: HWND, own_pid: u32) -> bool {
    unsafe {
        if !IsWindowVisible(h).as_bool() || GetWindowTextLengthW(h) == 0 {
            return false;
        }
        if GetWindow(h, GW_OWNER).map(|o| !o.is_invalid()).unwrap_or(false) {
            return false;
        }
        let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 && ex & WS_EX_APPWINDOW.0 == 0 {
            return false;
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut _ as _, 4);
        if cloaked != 0 {
            return false;
        }
        if window_pid(h) == own_pid {
            return false;
        }
        !matches!(class_name(h).as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd")
    }
}

/// Store apps live inside ApplicationFrameHost; the real process owns a child window.
fn real_pid(h: HWND, pid: u32) -> u32 {
    let mut kids: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumChildWindows(Some(h), Some(collect), LPARAM(&mut kids as *mut _ as isize));
    }
    kids.into_iter().map(window_pid).find(|p| *p != pid).unwrap_or(pid)
}

impl Apps {
    /// Every app window that's open, front to back (for ULTRON's taskbar).
    pub fn open_windows(&self) -> Vec<OpenWin> {
        let own = unsafe { GetCurrentProcessId() };
        let mut all: Vec<HWND> = Vec::new();
        unsafe {
            let _ = EnumWindows(Some(collect), LPARAM(&mut all as *mut _ as isize));
        }
        let front = unsafe { GetForegroundWindow() };
        let index = self.index.read().unwrap();
        all.into_iter()
            .filter(|h| is_app_window(*h, own))
            .filter_map(|h| {
                let mut pid = window_pid(h);
                let (mut exe, mut aumid) = process_identity(pid)?;
                if exe.to_lowercase().ends_with("applicationframehost.exe") {
                    pid = real_pid(h, pid);
                    (exe, aumid) = process_identity(pid)?;
                }
                let key = key_for(Some(&exe), aumid.as_deref());
                let known = index.iter().find(|e| e.key == key);
                let name = known.map(|e| e.name.clone()).or_else(|| file_description(&exe)).unwrap_or_else(|| exe_stem(&exe));
                let mut buf = [0u16; 300];
                let n = unsafe { GetWindowTextW(h, &mut buf) }.max(0) as usize;
                let title = String::from_utf16_lossy(&buf[..n]);
                // (a Store app whose own process couldn't be told from its host's: its window's name is its name)
                let name = if exe.to_lowercase().ends_with("applicationframehost.exe") { title.clone() } else { name };
                Some(OpenWin {
                    id: (h.0 as isize).to_string(),
                    title,
                    name,
                    launch: known.map(|e| e.launch.clone()).unwrap_or(exe),
                    active: h == front,
                    min: unsafe { IsIconic(h) }.as_bool(),
                })
            })
            .collect()
    }

    /// Close the windows of a running app whose name matches (for "close spotify").
    pub fn quit_by_name(&self, name: &str) -> Option<String> {
        let n = name.to_lowercase();
        let own = unsafe { GetCurrentProcessId() };
        let mut all: Vec<HWND> = Vec::new();
        unsafe {
            let _ = EnumWindows(Some(collect), LPARAM(&mut all as *mut _ as isize));
        }
        let mut groups: Vec<(String, Vec<HWND>)> = Vec::new();
        for h in all.into_iter().filter(|h| is_app_window(*h, own)) {
            let name = app_name_of(h, &|k| self.name_for(k)).unwrap_or_default();
            match groups.iter_mut().find(|g| g.0 == name) {
                Some(g) => g.1.push(h),
                None => groups.push((name, vec![h])),
            }
        }
        let g = groups.iter().find(|g| g.0.to_lowercase() == n).or_else(|| groups.iter().find(|g| g.0.to_lowercase().contains(&n)))?;
        for h in &g.1 {
            unsafe {
                let _ = PostMessageW(Some(*h), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
        Some(g.0.clone())
    }

    /// The main window of a running app whose name matches (for "switch to chrome").
    pub fn find_window(&self, name: &str) -> Option<HWND> {
        let n = name.to_lowercase();
        let own = unsafe { GetCurrentProcessId() };
        let mut all: Vec<HWND> = Vec::new();
        unsafe {
            let _ = EnumWindows(Some(collect), LPARAM(&mut all as *mut _ as isize));
        }
        let named: Vec<(String, HWND)> = all
            .into_iter()
            .filter(|h| is_app_window(*h, own))
            .map(|h| (app_name_of(h, &|k| self.name_for(k)).unwrap_or_default().to_lowercase(), h))
            .collect();
        named.iter().find(|(a, _)| *a == n).or_else(|| named.iter().find(|(a, _)| a.contains(&n) || (!a.is_empty() && n.contains(a.as_str())))).map(|(_, h)| *h)
    }

    /// Give focus back to the app you were using (menu bar clicks briefly take it).
    /// The app window you were last using (before clicking ULTRON).
    pub fn last_window(&self) -> Option<HWND> {
        let h = self.last_external_fg.load(Ordering::SeqCst);
        (h != 0).then(|| HWND(h as _))
    }

    pub fn refocus(&self) {
        let h = self.last_external_fg.load(Ordering::SeqCst);
        if h != 0 {
            focus_window(HWND(h as _));
        }
    }

    fn name_for(&self, key: &str) -> Option<String> {
        self.index.read().unwrap().iter().find(|e| e.key == key).map(|e| e.name.clone())
    }
}

fn app_name_of(h: HWND, lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let mut pid = window_pid(h);
    let (mut exe, mut aumid) = process_identity(pid)?;
    if exe.to_lowercase().ends_with("applicationframehost.exe") {
        pid = real_pid(h, pid);
        (exe, aumid) = process_identity(pid)?;
    }
    if exe.to_lowercase().ends_with(r"\explorer.exe") {
        return Some("Ultron".into()); // the desktop and File Explorer
    }
    let key = key_for(Some(&exe), aumid.as_deref());
    Some(lookup(&key).or_else(|| file_description(&exe)).unwrap_or_else(|| exe_stem(&exe)))
}

/* ---------------- app index (Start menu) ---------------- */

pub fn build_index() -> Vec<AppEntry> {
    let mut out = Vec::new();
    unsafe {
        let Ok(folder) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from("shell:AppsFolder"), None) else { return out };
        let Ok(en) = folder.BindToHandler::<_, IEnumShellItems>(None, &BHID_EnumItems) else { return out };
        loop {
            let mut arr: [Option<IShellItem>; 1] = [None];
            let mut got = 0u32;
            if en.Next(&mut arr, Some(&mut got)).is_err() || got == 0 {
                break;
            }
            let Some(item) = arr[0].take() else { break };
            let Ok(name) = item.GetDisplayName(SIGDN_NORMALDISPLAY).map(pwstr_take) else { continue };
            let Ok(parsing) = item.GetDisplayName(SIGDN_PARENTRELATIVEPARSING).map(pwstr_take) else { continue };
            let (mut target, mut aumid) = (None, None);
            if let Ok(i2) = item.cast::<IShellItem2>() {
                target = i2.GetString(&PKEY_Link_TargetParsingPath).ok().map(pwstr_take).filter(|s| s.to_lowercase().ends_with(".exe"));
                aumid = i2.GetString(&PKEY_AppUserModel_ID).ok().map(pwstr_take).filter(|s| !s.is_empty());
            }
            let packaged = aumid.as_deref().map(|a| a.contains('!')).unwrap_or(false);
            let key = if packaged || target.is_none() {
                key_for(None, Some(aumid.as_deref().unwrap_or(&parsing)))
            } else {
                key_for(target.as_deref(), None)
            };
            let launch = if parsing.starts_with("shell:") { parsing.clone() } else { format!("shell:AppsFolder\\{parsing}") };
            out.push(AppEntry { name, key, launch });
        }
    }
    out
}

/* ---------------- front app (for the menu bar) ---------------- */

/// The colours of a window, for the menu bar's tint: the whole window (below the bar) shrunk to 32 x 20 by Windows,
/// its average, and its main real colour (pixels weighted by how colourful they are, the very dark and very light
/// left out) with how much of the window is in colour. None when it's minimised or too small to say.
fn window_colour(h: HWND) -> Option<serde_json::Value> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Dwm::DWMWA_EXTENDED_FRAME_BOUNDS;
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, GetWindowRect, IsIconic, SM_CXSCREEN, SM_CYSCREEN};
    unsafe {
        if IsIconic(h).as_bool() {
            return None;
        }
        let mut r = RECT::default();
        if DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut RECT as *mut _, std::mem::size_of::<RECT>() as u32).is_err() {
            GetWindowRect(h, &mut r).ok()?;
        }
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        // (below the bar: 48 px covers it up to 150% scaling)
        let r = RECT { left: r.left.max(0), top: r.top.max(48), right: r.right.min(sw), bottom: r.bottom.min(sh) };
        if r.right - r.left < 60 || r.bottom - r.top < 60 {
            return None;
        }
        let px = crate::backdrop::grab_small(r, 32, 20)?;
        let (mut sum, mut acc, mut wsum) = ([0f64; 3], [0f64; 3], 0f64);
        let n = (px.len() / 4) as f64;
        for p in px.chunks_exact(4) {
            let (b, g, r) = (p[0] as f64, p[1] as f64, p[2] as f64);
            sum[0] += r;
            sum[1] += g;
            sum[2] += b;
            let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
            let v = mx / 255.0;
            let s = if mx > 0.0 { (mx - mn) / mx } else { 0.0 };
            if v > 0.16 && v < 0.97 && s > 0.3 {
                let w = s * s;
                acc[0] += r * w;
                acc[1] += g * w;
                acc[2] += b * w;
                wsum += w;
            }
        }
        let avg: Vec<u8> = sum.iter().map(|v| (v / n).round() as u8).collect();
        let main: Vec<u8> = if wsum > 0.0 { acc.iter().map(|v| (v / wsum).round() as u8).collect() } else { avg.clone() };
        Some(serde_json::json!({ "avg": avg, "acc": main, "cover": wsum / n }))
    }
}

fn window_title(h: HWND) -> String {
    use windows::Win32::UI::WindowsAndMessaging::GetWindowTextW;
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        let own = unsafe { GetCurrentProcessId() };
        let apps = app.state::<Apps>();
        *apps.index.write().unwrap() = build_index();
        let mut cache: HashMap<isize, String> = HashMap::new();
        let mut last_name = String::new();
        let mut ticks = 0u32;
        // the window in front (and its title: a browser's changes with the tab), and the looks at its colours still due
        let (mut col_win, mut col_title, mut col_due): (isize, String, Vec<std::time::Instant>) = (0, String::new(), Vec::new());
        loop {
            let fg = unsafe { GetAncestor(GetForegroundWindow(), GA_ROOTOWNER) };
            if !fg.is_invalid() && window_pid(fg) != own {
                apps.last_external_fg.store(fg.0 as isize, Ordering::SeqCst);
                // Its colours for the bar: looked at a moment after it comes to the front (once it's drawn) and again a
                // little later (once it has loaded); the desktop gives the bar its own tint back.
                let desk = matches!(class_name(fg).as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd");
                let title = window_title(fg);
                if fg.0 as isize != col_win || title != col_title {
                    let fresh = fg.0 as isize != col_win;
                    col_win = fg.0 as isize;
                    col_title = title;
                    let now = std::time::Instant::now();
                    col_due = if desk { Vec::new() } else if fresh { vec![now + Duration::from_millis(60), now + Duration::from_millis(450), now + Duration::from_millis(1500)] } else { vec![now + Duration::from_millis(400)] };
                    if desk {
                        let _ = app.emit_to("main", "front-colour", serde_json::Value::Null);
                    }
                }
                if col_due.first().is_some_and(|t| std::time::Instant::now() >= *t) {
                    col_due.remove(0);
                    if let Some(c) = window_colour(fg) {
                        let _ = app.emit_to("main", "front-colour", c);
                    }
                }
                let name = match class_name(fg).as_str() {
                    "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" => "Ultron".to_string(),
                    _ => cache
                        .entry(fg.0 as isize)
                        .or_insert_with(|| app_name_of(fg, &|k| apps.name_for(k)).unwrap_or_else(|| "Ultron".into()))
                        .clone(),
                };
                if name != last_name {
                    last_name = name.clone();
                    let _ = app.emit_to("main", "front-app", &name);
                    *app.state::<crate::ai::Ai>().front_app.lock().unwrap() = name.clone();
                }
            }
            ticks += 1;
            if ticks == 40 && col_due.is_empty() && col_win != 0 {
                col_title.clear();
            }
            if ticks % 4000 == 0 {
                *apps.index.write().unwrap() = build_index();
            }
            std::thread::sleep(Duration::from_millis(if col_due.is_empty() { 120 } else { 40 }));
        }
    });
}
