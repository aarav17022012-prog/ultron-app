//! ULTRON's own notifications (Settings › Notifications › ULTRON): they hang down from the menu bar in its glass
//! and gather under the bell by the clock, instead of Windows' pop-ups in the corner.
//!
//! What comes in: Windows' notification listener (the list Windows' notification centre shows; ULTRON was already
//! allowed to read it for incoming calls) gives each notification's app, words and time. What Windows' own banner
//! would have offered (its buttons, reply box, pictures, and what a click opens) is read from the notification as the
//! app sent it, in Windows' notification database: read-only, through Windows' own winsqlite3.dll.
//!
//! Pressing one does what Windows does. An app that registered a "toast activator" (Chrome, Brave, Telegram,
//! WhatsApp, Outlook ...) is handed the button's arguments, and anything typed in its reply box, through that
//! activator; a link opens; a Store app without one is opened with the arguments; anything else is just opened.
//! Buttons that nothing outside Windows could press are left off.
//!
//! While ULTRON's are on, Windows' own banners are turned off app by app (each app's "Show notification banners",
//! the ShowBanner value under Notifications\Settings) so the two don't both show. What was changed is written down
//! and put back when the setting goes back to Windows, when ULTRON closes, or by the taskbar guard if ULTRON ends
//! without closing. Clock (alarms) and Phone Link's calls keep Windows' banners: their buttons belong there.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, OnceLock};

use base64::Engine;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use windows::core::{s, Interface, GUID, HSTRING, PCWSTR, PWSTR};
use windows::Data::Xml::Dom::{XmlDocument, XmlElement};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Storage::EnhancedStorage::{PKEY_AppUserModel_ID, PKEY_AppUserModel_ToastActivatorCLSID};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_LOCAL_SERVER, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Notifications::{INotificationActivationCallback, NOTIFICATION_USER_INPUT_DATA};
use windows::Win32::UI::Shell::{ApplicationActivationManager, IApplicationActivationManager, IShellItem2, SHCreateItemFromParsingName, AO_NONE};
use windows::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};
use windows::UI::Notifications::Management::UserNotificationListener;
use windows::UI::Notifications::{KnownNotificationBindings, UserNotification};

#[derive(Clone, Serialize, Default, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Act {
    pub label: String,
    pub args: String,
    /// foreground, background, protocol or system
    pub kind: String,
    /// the reply box this button sends, if any
    pub input: String,
}

#[derive(Clone, Serialize, Default, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Field {
    pub id: String,
    /// text or selection
    pub kind: String,
    pub hint: String,
    pub value: String,
    pub title: String,
    pub choices: Vec<[String; 2]>,
}

#[derive(Clone, Serialize, Default, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Note {
    pub id: u32,
    pub aumid: String,
    pub app: String,
    pub icon: String,
    pub title: String,
    pub body: String,
    /// the app's own heading over a run of them (Claude's conversation, a chat's name)
    pub group: String,
    /// when it came, in milliseconds since 1970
    pub at: i64,
    /// alarm, reminder, incomingCall, urgent or ""
    pub scenario: String,
    pub silent: bool,
    /// an incoming call: the notch shows those itself
    pub call: bool,
    pub actions: Vec<Act>,
    pub fields: Vec<Field>,
    /// a picture standing in for the app's icon (a contact's face)
    pub face: String,
    /// a picture in the notification itself
    pub picture: String,
    #[serde(skip)]
    launch: String,
    #[serde(skip)]
    launch_kind: String,
    #[serde(skip)]
    package: String,
}

#[derive(Default)]
struct St {
    on: bool,
    primed: bool,
    notes: HashMap<u32, Note>,
    waits: HashMap<u32, u8>,
    icons: HashMap<String, String>,
    /// notifications you cleared or dismissed that Windows still lists for a moment (it clears them a little later):
    /// they aren't taken for new ones in the meantime
    cleared: HashSet<u32>,
    /// "clear all" was pressed at this moment (Windows' time): anything older that turns up is one of those
    clear_before: i64,
}
static ST: LazyLock<Mutex<St>> = LazyLock::new(Default::default);
/// app id -> its toast activator, as found (None: it has none)
static ACTIVATORS: LazyLock<Mutex<HashMap<String, Option<GUID>>>> = LazyLock::new(Default::default);
/// app id -> toast activator, from the Start menu's shortcuts (read once)
static SHORTCUTS: OnceLock<HashMap<String, GUID>> = OnceLock::new();

pub fn on() -> bool {
    ST.lock().unwrap().on
}

/* ---------------- what's in Windows' list ---------------- */

/// Called with Windows' list each time calls.rs looks at it (every 0.9 s), only while ULTRON's are on.
/// New ones go to the page ("note"), gone ones too ("note-gone"); what was there before ULTRON looked comes in quietly.
pub fn poll(app: &AppHandle, list: &[UserNotification]) {
    let (fresh, first) = {
        let mut st = ST.lock().unwrap();
        if !st.on {
            return;
        }
        let ids: HashSet<u32> = list.iter().filter_map(|n| n.Id().ok()).collect();
        st.cleared.retain(|i| ids.contains(i));
        let gone: Vec<u32> = st.notes.keys().copied().filter(|i| !ids.contains(i)).collect();
        for i in &gone {
            st.notes.remove(i);
            st.waits.remove(i);
        }
        if !gone.is_empty() {
            let _ = app.emit_to("main", "note-gone", &gone);
        }
        let before = st.clear_before;
        let fresh: Vec<u32> = ids
            .into_iter()
            .filter(|i| !st.notes.contains_key(i) && !st.cleared.contains(i))
            .filter(|i| before == 0 || list.iter().find(|n| n.Id().ok() == Some(*i)).and_then(|n| n.CreationTime().ok()).map(|t| t.UniversalTime > before).unwrap_or(true))
            .collect();
        (fresh, !st.primed)
    };
    if fresh.is_empty() {
        if first {
            ST.lock().unwrap().primed = true;
            let _ = app.emit_to("main", "notes-reset", ());
        }
        return;
    }
    let payloads = db::payloads(&fresh);
    let mut made = Vec::new();
    for n in list {
        let Ok(id) = n.Id() else { continue };
        if !fresh.contains(&id) {
            continue;
        }
        let xml = payloads.get(&id);
        // (the database is written a moment after the list now and then: wait one look for it, then go without)
        if xml.is_none() && !first {
            let mut st = ST.lock().unwrap();
            let w = st.waits.entry(id).or_insert(0);
            *w += 1;
            if *w < 2 {
                continue;
            }
        }
        made.push(build(n, xml.map(|s| s.as_str())));
    }
    let now = unix_ms();
    let mut st = ST.lock().unwrap();
    if !st.on {
        return;
    }
    let mut new_apps = Vec::new();
    for note in made {
        if !first && now - note.at < 90_000 {
            let _ = app.emit_to("main", "note", &note);
        }
        if !note.aumid.is_empty() {
            new_apps.push(note.aumid.clone());
        }
        st.waits.remove(&note.id);
        st.notes.insert(note.id, note);
    }
    if first {
        st.primed = true;
        let _ = app.emit_to("main", "notes-reset", ());
        crate::log_line(&format!("notes: {} in Windows' list ({} read from its database)", st.notes.len(), payloads.len()));
    }
    drop(st);
    if !new_apps.is_empty() {
        let app = app.clone();
        std::thread::spawn(move || banners::cover(&app, &new_apps));
    }
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn build(n: &UserNotification, xml: Option<&str>) -> Note {
    let info = n.AppInfo().ok();
    let aumid = info.as_ref().and_then(|a| a.AppUserModelId().ok()).map(|s| s.to_string()).unwrap_or_default();
    let app = info.as_ref().and_then(|a| a.DisplayInfo().ok()).and_then(|d| d.DisplayName().ok()).map(|s| s.to_string()).unwrap_or_default();
    let package = info.as_ref().and_then(|a| a.Package().ok()).and_then(|p| p.InstalledPath().ok()).map(|s| s.to_string()).unwrap_or_default();
    let family = info.as_ref().and_then(|a| a.PackageFamilyName().ok()).map(|s| s.to_string()).unwrap_or_default();
    let created = n.CreationTime().map(|t| t.UniversalTime).unwrap_or(0);
    let texts: Vec<String> = n
        .Notification()
        .and_then(|x| x.Visual())
        .and_then(|v| KnownNotificationBindings::ToastGeneric().and_then(|k| v.GetBinding(&k)))
        .and_then(|b| b.GetTextElements())
        .map(|els| els.into_iter().filter_map(|e| e.Text().ok().map(|t| t.to_string())).filter(|t| !t.trim().is_empty()).collect())
        .unwrap_or_default();
    let mut note = Note {
        id: n.Id().unwrap_or(0),
        aumid: aumid.clone(),
        app,
        at: if created > 0 { (created - 116_444_736_000_000_000) / 10_000 } else { unix_ms() },
        title: texts.first().cloned().unwrap_or_default(),
        body: texts.iter().skip(1).cloned().collect::<Vec<_>>().join("\n"),
        launch_kind: "foreground".into(),
        package: package.clone(),
        ..Default::default()
    };
    if let Some(x) = xml {
        let _ = read_xml(&mut note, x, &package, &family);
    }
    note.call = note.scenario.eq_ignore_ascii_case("incomingCall") || crate::calls::read(&texts).is_some();
    note.icon = icon(&aumid);
    if note.app.is_empty() {
        note.app = aumid.rsplit(['\\', '!', '.']).next().unwrap_or("").to_string();
    }
    // buttons that nothing but Windows could press are left off; a reply box only reaches an app through its activator
    let has = activator(&aumid, &package).is_some();
    let packaged = aumid.contains('!');
    note.actions.retain(|a| match a.kind.as_str() {
        "protocol" => true,
        "system" => a.args.eq_ignore_ascii_case("dismiss"),
        "background" => has,
        _ => has || packaged,
    });
    if !has {
        note.fields.clear();
        for a in &mut note.actions {
            a.input.clear();
        }
    }
    note
}

fn read_xml(note: &mut Note, xml: &str, package: &str, family: &str) -> windows::core::Result<()> {
    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(xml))?;
    let root = doc.DocumentElement()?;
    let attr = |e: &XmlElement, k: &str| e.GetAttribute(&HSTRING::from(k)).map(|h| h.to_string()).unwrap_or_default();
    let all = |tag: &str| -> Vec<XmlElement> {
        let Ok(list) = doc.GetElementsByTagName(&HSTRING::from(tag)) else { return Vec::new() };
        (0..list.Length().unwrap_or(0)).filter_map(|i| list.Item(i).ok()).filter_map(|n| n.cast::<XmlElement>().ok()).collect()
    };
    note.launch = attr(&root, "launch");
    let k = attr(&root, "activationType");
    if !k.is_empty() {
        note.launch_kind = k;
    }
    note.scenario = attr(&root, "scenario");
    note.silent = all("audio").iter().any(|a| attr(a, "silent").eq_ignore_ascii_case("true"));
    if let Some(h) = all("header").first() {
        note.group = attr(h, "title");
    }
    if note.title.is_empty() {
        let texts: Vec<String> = all("text").iter().filter_map(|t| t.InnerText().ok().map(|s| s.to_string())).filter(|t| !t.trim().is_empty()).collect();
        note.title = texts.first().cloned().unwrap_or_default();
        note.body = texts.iter().skip(1).cloned().collect::<Vec<_>>().join("\n");
    }
    for img in all("image") {
        let placement = attr(&img, "placement");
        let src = attr(&img, "src");
        if placement.eq_ignore_ascii_case("appLogoOverride") && note.face.is_empty() {
            note.face = picture(&src, package, family, 600_000);
        } else if (placement.is_empty() || placement.eq_ignore_ascii_case("hero") || placement.eq_ignore_ascii_case("inline")) && note.picture.is_empty() {
            note.picture = picture(&src, package, family, 1_500_000);
        }
    }
    for i in all("input") {
        let choices = {
            let Ok(kids) = i.GetElementsByTagName(&HSTRING::from("selection")) else { continue };
            (0..kids.Length().unwrap_or(0)).filter_map(|n| kids.Item(n).ok()).filter_map(|n| n.cast::<XmlElement>().ok()).map(|s| [attr(&s, "id"), attr(&s, "content")]).collect()
        };
        note.fields.push(Field { id: attr(&i, "id"), kind: attr(&i, "type"), hint: attr(&i, "placeHolderContent"), value: attr(&i, "defaultInput"), title: attr(&i, "title"), choices });
    }
    for a in all("action") {
        if attr(&a, "placement").eq_ignore_ascii_case("contextMenu") {
            continue;
        }
        let kind = attr(&a, "activationType");
        note.actions.push(Act { label: attr(&a, "content"), args: attr(&a, "arguments"), kind: if kind.is_empty() { "foreground".into() } else { kind }, input: attr(&a, "hint-inputId") });
    }
    Ok(())
}

/// A picture the notification names, as a data URL: only ones already on this PC (a file, or inside the app's package).
fn picture(src: &str, package: &str, family: &str, max: u64) -> String {
    let src = src.trim();
    let low = src.to_lowercase();
    let path: Option<PathBuf> = if let Some(p) = low.strip_prefix("file:///").map(|_| &src[8..]) {
        Some(PathBuf::from(unescape(p).replace('/', "\\")))
    } else if low.starts_with("ms-appx:///") && !package.is_empty() {
        Some(PathBuf::from(package).join(unescape(&src[11..]).replace('/', "\\")))
    } else if low.starts_with("ms-appdata:///local/") && !family.is_empty() {
        std::env::var("LOCALAPPDATA").ok().map(|l| PathBuf::from(l).join("Packages").join(family).join("LocalState").join(unescape(&src[20..]).replace('/', "\\")))
    } else if src.len() > 3 && src.as_bytes()[1] == b':' {
        Some(PathBuf::from(src))
    } else {
        None
    };
    let Some(path) = path else { return String::new() };
    let Ok(meta) = std::fs::metadata(&path) else { return String::new() };
    if meta.len() == 0 || meta.len() > max {
        return String::new();
    }
    let Ok(bytes) = std::fs::read(&path) else { return String::new() };
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        _ => "image/png",
    };
    format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn icon(aumid: &str) -> String {
    if aumid.is_empty() {
        return String::new();
    }
    if let Some(u) = ST.lock().unwrap().icons.get(aumid) {
        return u.clone();
    }
    let u = crate::media::icon(aumid).unwrap_or_default();
    ST.lock().unwrap().icons.insert(aumid.to_string(), u.clone());
    u
}

/* ---------------- pressing one ---------------- */

/// The COM class an app asked Windows to hand its notifications' clicks to, if it has one.
fn activator(aumid: &str, package: &str) -> Option<GUID> {
    if aumid.is_empty() {
        return None;
    }
    if let Some(g) = ACTIVATORS.lock().unwrap().get(aumid) {
        return *g;
    }
    let found = (|| {
        // registered for the app id (Windows App SDK and Acrobat-style apps)
        let key = format!("Software\\Classes\\AppUserModelId\\{aumid}");
        if let Some(s) = reg::string(&key, "CustomActivator") {
            if let Ok(g) = GUID::try_from(s.trim_matches(['{', '}'])) {
                return Some(g);
            }
        }
        // in the app's package (WhatsApp, Outlook)
        if !package.is_empty() {
            if let Ok(m) = std::fs::read_to_string(PathBuf::from(package).join("AppxManifest.xml")) {
                if let Some(i) = m.find("ToastActivatorCLSID=\"") {
                    let rest = &m[i + 21..];
                    if let Some(end) = rest.find('"') {
                        if let Ok(g) = GUID::try_from(rest[..end].trim_matches(['{', '}'])) {
                            return Some(g);
                        }
                    }
                }
            }
        }
        // on its Start menu shortcut (Chrome, Brave, Telegram, Electron apps)
        shortcuts().iter().find(|(id, _)| id.eq_ignore_ascii_case(aumid)).map(|(_, g)| *g)
    })();
    ACTIVATORS.lock().unwrap().insert(aumid.to_string(), found);
    found
}

fn shortcuts() -> &'static HashMap<String, GUID> {
    SHORTCUTS.get_or_init(|| {
        let mut out = HashMap::new();
        let mut dirs = Vec::new();
        if let Ok(a) = std::env::var("APPDATA") {
            dirs.push(PathBuf::from(a).join("Microsoft\\Windows\\Start Menu\\Programs"));
        }
        if let Ok(p) = std::env::var("ProgramData") {
            dirs.push(PathBuf::from(p).join("Microsoft\\Windows\\Start Menu\\Programs"));
        }
        let mut files = Vec::new();
        while let Some(d) = dirs.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    dirs.push(p);
                } else if p.extension().map(|x| x.eq_ignore_ascii_case("lnk")).unwrap_or(false) {
                    files.push(p);
                }
            }
        }
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            for f in files {
                let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem2>(&HSTRING::from(f.to_string_lossy().as_ref()), None) else { continue };
                let Ok(g) = item.GetCLSID(&PKEY_AppUserModel_ToastActivatorCLSID) else { continue };
                if g == GUID::zeroed() {
                    continue;
                }
                if let Ok(p) = item.GetString(&PKEY_AppUserModel_ID) {
                    let id = take(p);
                    if !id.is_empty() {
                        out.insert(id, g);
                    }
                }
            }
        }
        crate::log_line(&format!("notes: {} apps take their notifications' clicks themselves", out.len()));
        out
    })
}

unsafe fn take(p: PWSTR) -> String {
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.0 as _));
    s
}

/// A click on a notification (`which` None) or on one of its buttons, with what was typed or picked in its boxes.
pub fn press(app: &AppHandle, id: u32, which: Option<usize>, typed: Vec<(String, String)>) -> Result<(), String> {
    let note = ST.lock().unwrap().notes.get(&id).cloned().ok_or("That notification has gone.")?;
    let (args, kind) = match which {
        Some(i) => note.actions.get(i).map(|a| (a.args.clone(), a.kind.clone())).ok_or("No such button.")?,
        None => (note.launch.clone(), note.launch_kind.clone()),
    };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = AllowSetForegroundWindow(ASFW_ANY);
    }
    let done: Result<(), String> = if kind.eq_ignore_ascii_case("system") {
        Ok(())
    } else if kind.eq_ignore_ascii_case("protocol") && !args.is_empty() {
        if crate::apps::shell_open(&args) { Ok(()) } else { Err("That link didn't open.".into()) }
    } else if let Some(clsid) = activator(&note.aumid, &note.package) {
        // every box the notification has goes along, the way Windows sends them (typed ones as typed, the rest as they were)
        let mut pairs: Vec<(HSTRING, HSTRING)> = note.fields.iter().map(|f| {
            let v = typed.iter().find(|(k, _)| *k == f.id).map(|(_, v)| v.clone()).unwrap_or_else(|| f.value.clone());
            (HSTRING::from(&f.id), HSTRING::from(v))
        }).collect();
        for (k, v) in &typed {
            if !note.fields.iter().any(|f| &f.id == k) {
                pairs.push((HSTRING::from(k), HSTRING::from(v)));
            }
        }
        let data: Vec<NOTIFICATION_USER_INPUT_DATA> = pairs.iter().map(|(k, v)| NOTIFICATION_USER_INPUT_DATA { Key: PCWSTR(k.as_ptr()), Value: PCWSTR(v.as_ptr()) }).collect();
        unsafe {
            CoCreateInstance::<_, INotificationActivationCallback>(&clsid, None, CLSCTX_LOCAL_SERVER)
                .and_then(|cb| cb.Activate(&HSTRING::from(&note.aumid), &HSTRING::from(&args), &data))
                .map_err(|e| format!("{} didn't answer ({:#x}).", note.app, e.code().0))
        }
    } else if note.aumid.contains('!') && !kind.eq_ignore_ascii_case("background") {
        unsafe {
            CoCreateInstance::<_, IApplicationActivationManager>(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER)
                .and_then(|m| m.ActivateApplication(&HSTRING::from(&note.aumid), &HSTRING::from(&args), AO_NONE))
                .map(|_| ())
                .map_err(|e| format!("{} didn't open ({:#x}).", note.app, e.code().0))
        }
    } else if !note.aumid.is_empty() && crate::apps::shell_open(&format!("shell:AppsFolder\\{}", note.aumid)) {
        Ok(())
    } else {
        Err(format!("{} can't be opened from here.", if note.app.is_empty() { "That app" } else { &note.app }))
    };
    crate::log_line(&format!("notes: {} on {} ({kind}): {}", if which.is_some() { "button" } else { "click" }, note.aumid, if done.is_ok() { "done" } else { "failed" }));
    // (pressed, it's dealt with: Windows takes it out of its list too)
    if done.is_ok() {
        dismiss(app, id);
    }
    done
}

/// Reply to a message whose notification has no box of its own: the notification is opened (Windows opens the
/// chat it's from), and once that app is in front the reply is typed into it and sent with Enter.
pub fn reply(app: &AppHandle, id: u32, text: &str) -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    let text = text.trim();
    if text.is_empty() {
        return Ok(());
    }
    let before = unsafe { GetForegroundWindow() }.0 as isize;
    press(app, id, None, Vec::new())?;
    // (the app comes to the front: wait for that, then a moment for the chat to open and its box to take the keys)
    let mut changed = false;
    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if unsafe { GetForegroundWindow() }.0 as isize != before {
            changed = true;
            break;
        }
    }
    if !changed {
        return Err("The app didn't open, so the reply wasn't sent.".into());
    }
    std::thread::sleep(std::time::Duration::from_millis(900));
    crate::dictate::type_out(text);
    std::thread::sleep(std::time::Duration::from_millis(120));
    crate::apps::send_keys(&[windows::Win32::UI::Input::KeyboardAndMouse::VK_RETURN]);
    Ok(())
}

pub fn dismiss(app: &AppHandle, id: u32) {
    if let Ok(l) = UserNotificationListener::Current() {
        let _ = l.RemoveNotification(id);
    }
    let had = {
        let mut st = ST.lock().unwrap();
        st.cleared.insert(id);
        st.notes.remove(&id).is_some()
    };
    if had {
        let _ = app.emit_to("main", "note-gone", [id]);
    }
}

/// Clear one app's notifications, or (None) all of them, from ULTRON's list and Windows' alike.
pub fn clear(app: &AppHandle, aumid: Option<String>) {
    let ids: Vec<u32> = {
        let st = ST.lock().unwrap();
        st.notes.values().filter(|n| aumid.as_ref().map(|a| &n.aumid == a).unwrap_or(true)).map(|n| n.id).collect()
    };
    if let Ok(l) = UserNotificationListener::Current() {
        if aumid.is_none() {
            let _ = l.ClearNotifications();
        } else {
            for id in &ids {
                let _ = l.RemoveNotification(*id);
            }
        }
    }
    let mut st = ST.lock().unwrap();
    for id in &ids {
        st.notes.remove(id);
        st.cleared.insert(*id);
    }
    if aumid.is_none() {
        // (Windows' time: 100 ns steps since 1601)
        st.clear_before = unix_ms() as i64 * 10_000 + 116_444_736_000_000_000;
    }
    drop(st);
    if !ids.is_empty() {
        let _ = app.emit_to("main", "note-gone", &ids);
    }
}

/// Everything in the list now, newest first.
pub fn list() -> Vec<Note> {
    let mut v: Vec<Note> = ST.lock().unwrap().notes.values().cloned().collect();
    v.sort_by(|a, b| b.at.cmp(&a.at));
    v
}

/// The setting: ULTRON's (true) or Windows' own (false).
pub fn set(app: &AppHandle, on: bool) {
    {
        let mut st = ST.lock().unwrap();
        if st.on == on {
            return;
        }
        st.on = on;
        st.primed = false;
        if !on {
            st.notes.clear();
            st.waits.clear();
        }
    }
    let app = app.clone();
    std::thread::spawn(move || {
        if on {
            banners::all_off(&app);
            let _ = shortcuts();
        } else {
            banners::restore(&record_path(&app));
        }
    });
}

/* ---------------- per app: Windows' own switch for its notifications ---------------- */

#[derive(Serialize)]
pub struct AppRow {
    pub aumid: String,
    pub name: String,
    pub icon: String,
    pub on: bool,
}

const SETTINGS: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Notifications\\Settings";

/// Apps that have sent notifications lately (Windows keeps a key for each), with whether they're allowed to.
pub fn apps() -> Vec<AppRow> {
    let now = (unix_ms() * 10_000) + 116_444_736_000_000_000;
    let mut rows: Vec<(i64, AppRow)> = reg::subkeys(SETTINGS)
        .into_iter()
        .filter_map(|id| {
            let key = format!("{SETTINGS}\\{id}");
            let last = reg::qword(&key, "LastNotificationAddedTime")? as i64;
            // (the last 120 days: the rest are apps long gone or long quiet)
            if now - last > 120 * 24 * 3600 * 10_000_000 {
                return None;
            }
            let name = display_name(&id)?;
            Some((last, AppRow { on: reg::dword(&key, "Enabled") != Some(0), icon: icon(&id), aumid: id, name }))
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    rows.into_iter().map(|(_, r)| r).take(24).collect()
}

fn display_name(aumid: &str) -> Option<String> {
    if let Some(n) = ST.lock().unwrap().notes.values().find(|n| n.aumid == aumid).map(|n| n.app.clone()) {
        return Some(n);
    }
    if let Some(n) = reg::string(&format!("Software\\Classes\\AppUserModelId\\{aumid}"), "DisplayName").filter(|n| !n.is_empty() && !n.starts_with('@')) {
        return Some(n);
    }
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let item = SHCreateItemFromParsingName::<_, _, windows::Win32::UI::Shell::IShellItem>(&HSTRING::from(format!("shell:AppsFolder\\{aumid}")), None).ok()?;
        let n = take(item.GetDisplayName(windows::Win32::UI::Shell::SIGDN_NORMALDISPLAY).ok()?);
        (!n.is_empty() && !n.contains('\\') && !n.contains('!')).then_some(n)
    }
}

/// Windows' own "Notifications" switch for one app.
pub fn allow(aumid: &str, on: bool) {
    let key = format!("{SETTINGS}\\{aumid}");
    if on {
        reg::delete(&key, "Enabled");
    } else {
        reg::set_dword(&key, "Enabled", 0);
    }
    crate::log_line(&format!("notes: {aumid} notifications {}", if on { "on" } else { "off" }));
}

/* ---------------- Windows' own banners, while ULTRON's are on ---------------- */

fn record_path(app: &AppHandle) -> PathBuf {
    app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir()).join("notes-banners.json")
}

/// Where the record of changed banners is kept, found without a running ULTRON (for the taskbar guard).
fn record_path_bare() -> Option<PathBuf> {
    std::env::var("APPDATA").ok().map(|a| PathBuf::from(a).join("com.ultron.notch").join("notes-banners.json"))
}

/// The taskbar guard calls this once the ULTRON it watched has gone.
pub fn restore_left_over() {
    if let Some(p) = record_path_bare() {
        banners::restore(&p);
    }
}

/// ULTRON closing.
pub fn leave(app: &AppHandle) {
    if on() {
        banners::restore(&record_path(app));
    }
}

mod banners {
    use super::*;

    static LOCK: Mutex<()> = Mutex::new(());

    fn kept(aumid: &str) -> bool {
        let l = aumid.to_lowercase();
        l.starts_with("microsoft.windowsalarms") || l.contains("yourphonecalling") || l.starts_with("com.ultron")
    }

    fn load(path: &std::path::Path) -> HashMap<String, Option<u32>> {
        std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    /// Turn Windows' banner off for these apps (if it's on), noting how each was.
    pub fn cover(app: &AppHandle, ids: &[String]) {
        if !on() {
            return;
        }
        let _g = LOCK.lock().unwrap();
        let path = record_path(app);
        let mut rec = load(&path);
        let mut changed = 0;
        for id in ids {
            if kept(id) || rec.contains_key(id) || id.contains('\\') {
                continue;
            }
            let key = format!("{SETTINGS}\\{id}");
            let was = reg::dword(&key, "ShowBanner");
            if was == Some(0) {
                continue;
            }
            reg::set_dword(&key, "ShowBanner", 0);
            rec.insert(id.clone(), was);
            changed += 1;
        }
        if changed > 0 {
            crate::keep(&path, serde_json::to_vec(&rec).unwrap_or_default());
        }
    }

    pub fn all_off(app: &AppHandle) {
        let ids: Vec<String> = reg::subkeys(SETTINGS).into_iter().filter(|id| reg::qword(&format!("{SETTINGS}\\{id}"), "LastNotificationAddedTime").is_some()).collect();
        cover(app, &ids);
        crate::log_line(&format!("notes: Windows' own banners off for {} apps while ULTRON's are on", load(&record_path(app)).len()));
    }

    pub fn restore(path: &std::path::Path) {
        let _g = LOCK.lock().unwrap();
        let rec = load(path);
        for (id, was) in &rec {
            let key = format!("{SETTINGS}\\{id}");
            match was {
                Some(v) => reg::set_dword(&key, "ShowBanner", *v),
                None => reg::delete(&key, "ShowBanner"),
            }
        }
        let _ = std::fs::remove_file(path);
        if !rec.is_empty() {
            crate::log_line(&format!("notes: Windows' own banners back for {} apps", rec.len()));
        }
    }
}

/* ---------------- the registry (the current user's part) ---------------- */

mod reg {
    use super::*;

    pub fn subkeys(path: &str) -> Vec<String> {
        let mut out = Vec::new();
        unsafe {
            let mut k = HKEY::default();
            if RegOpenKeyExW(HKEY_CURRENT_USER, &HSTRING::from(path), None, KEY_READ, &mut k) != ERROR_SUCCESS {
                return out;
            }
            let mut buf = [0u16; 512];
            for i in 0.. {
                let mut n = buf.len() as u32;
                if RegEnumKeyExW(k, i, Some(PWSTR(buf.as_mut_ptr())), &mut n, None, None, None, None) != ERROR_SUCCESS {
                    break;
                }
                out.push(String::from_utf16_lossy(&buf[..n as usize]));
            }
            let _ = RegCloseKey(k);
        }
        out
    }

    fn raw(path: &str, name: &str, flags: REG_ROUTINE_FLAGS, buf: &mut [u8]) -> Option<usize> {
        let mut n = buf.len() as u32;
        let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, &HSTRING::from(path), &HSTRING::from(name), flags, None, Some(buf.as_mut_ptr() as _), Some(&mut n)) };
        (r == ERROR_SUCCESS).then_some(n as usize)
    }

    pub fn dword(path: &str, name: &str) -> Option<u32> {
        let mut b = [0u8; 4];
        raw(path, name, RRF_RT_REG_DWORD, &mut b).map(|_| u32::from_le_bytes(b))
    }

    pub fn qword(path: &str, name: &str) -> Option<u64> {
        let mut b = [0u8; 8];
        raw(path, name, RRF_RT_REG_QWORD, &mut b).map(|_| u64::from_le_bytes(b))
    }

    pub fn string(path: &str, name: &str) -> Option<String> {
        let mut b = [0u8; 1024];
        let n = raw(path, name, RRF_RT_REG_SZ, &mut b)?;
        let w: Vec<u16> = b[..n].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|c| *c != 0).collect();
        Some(String::from_utf16_lossy(&w))
    }

    pub fn set_dword(path: &str, name: &str, v: u32) {
        unsafe {
            let mut k = HKEY::default();
            if RegCreateKeyExW(HKEY_CURRENT_USER, &HSTRING::from(path), None, PCWSTR::null(), REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut k, None) == ERROR_SUCCESS {
                let b = v.to_le_bytes();
                let _ = RegSetValueExW(k, &HSTRING::from(name), None, REG_DWORD, Some(&b));
                let _ = RegCloseKey(k);
            }
        }
    }

    pub fn delete(path: &str, name: &str) {
        unsafe {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, &HSTRING::from(path), &HSTRING::from(name));
        }
    }
}

/* ---------------- Windows' notification database (read-only) ---------------- */

mod db {
    use super::*;
    use std::ffi::c_void;

    type Open = unsafe extern "system" fn(*const u8, *mut *mut c_void, i32, *const u8) -> i32;
    type Prepare = unsafe extern "system" fn(*mut c_void, *const u8, i32, *mut *mut c_void, *mut *const u8) -> i32;
    type Step = unsafe extern "system" fn(*mut c_void) -> i32;
    type ColInt = unsafe extern "system" fn(*mut c_void, i32) -> i64;
    type ColBlob = unsafe extern "system" fn(*mut c_void, i32) -> *const u8;
    type ColBytes = unsafe extern "system" fn(*mut c_void, i32) -> i32;
    type Fin = unsafe extern "system" fn(*mut c_void) -> i32;
    type Busy = unsafe extern "system" fn(*mut c_void, i32) -> i32;

    struct Lib {
        open: Open,
        prepare: Prepare,
        step: Step,
        int: ColInt,
        blob: ColBlob,
        bytes: ColBytes,
        finalize: Fin,
        close: Fin,
        busy: Busy,
    }
    static LIB: OnceLock<Option<Lib>> = OnceLock::new();

    fn lib() -> Option<&'static Lib> {
        LIB.get_or_init(|| unsafe {
            let h = LoadLibraryW(&HSTRING::from("winsqlite3.dll")).ok()?;
            macro_rules! f {
                ($n:literal) => {
                    std::mem::transmute(GetProcAddress(h, s!($n))?)
                };
            }
            Some(Lib {
                open: f!("sqlite3_open_v2"),
                prepare: f!("sqlite3_prepare_v2"),
                step: f!("sqlite3_step"),
                int: f!("sqlite3_column_int64"),
                blob: f!("sqlite3_column_blob"),
                bytes: f!("sqlite3_column_bytes"),
                finalize: f!("sqlite3_finalize"),
                close: f!("sqlite3_close"),
                busy: f!("sqlite3_busy_timeout"),
            })
        })
        .as_ref()
    }

    /// The notifications' XML as their apps sent it, by id.
    pub fn payloads(ids: &[u32]) -> HashMap<u32, String> {
        let mut out = HashMap::new();
        let (Some(l), Ok(local)) = (lib(), std::env::var("LOCALAPPDATA")) else { return out };
        if ids.is_empty() {
            return out;
        }
        let file = format!("{local}\\Microsoft\\Windows\\Notifications\\wpndatabase.db\0");
        let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        let sql = format!("select Id, Payload from Notification where Type = 'toast' and Id in ({list})\0");
        unsafe {
            let mut db: *mut c_void = std::ptr::null_mut();
            if (l.open)(file.as_ptr(), &mut db, 1 /* read only */, std::ptr::null()) != 0 {
                if !db.is_null() {
                    (l.close)(db);
                }
                return out;
            }
            (l.busy)(db, 250);
            let mut st: *mut c_void = std::ptr::null_mut();
            if (l.prepare)(db, sql.as_ptr(), -1, &mut st, std::ptr::null_mut()) == 0 {
                while (l.step)(st) == 100 {
                    let id = (l.int)(st, 0) as u32;
                    let n = (l.bytes)(st, 1).max(0) as usize;
                    let p = (l.blob)(st, 1);
                    if n > 0 && !p.is_null() {
                        let b = std::slice::from_raw_parts(p, n);
                        out.insert(id, String::from_utf8_lossy(b).into_owned());
                    }
                }
                (l.finalize)(st);
            }
            (l.close)(db);
        }
        out
    }
}
