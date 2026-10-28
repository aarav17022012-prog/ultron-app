//! App connections (Settings > Tasks): services ULTRON can read from and act in for you, each signed in with a key of
//! yours from that service (a personal access token, an integration secret, a webhook address). The keys are kept in
//! Windows Credential Manager, never in a file, and each is only ever sent to its own service: a request can name a
//! path on that service, never another address.

use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Security::Credentials::{CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC};

pub struct Service {
    pub id: &'static str,
    pub name: &'static str,
    /// where its API is ("" when you give the address: your Home Assistant, your Discord webhook)
    base: &'static str,
    /// a request that only reads, to check the key works
    test: &'static str,
    extra: &'static [(&'static str, &'static str)],
}

pub const SERVICES: [Service; 6] = [
    Service { id: "github", name: "GitHub", base: "https://api.github.com", test: "/user", extra: &[("Accept", "application/vnd.github+json"), ("X-GitHub-Api-Version", "2022-11-28")] },
    Service { id: "notion", name: "Notion", base: "https://api.notion.com/v1", test: "/users/me", extra: &[("Notion-Version", "2022-06-28")] },
    Service { id: "todoist", name: "Todoist", base: "https://api.todoist.com/rest/v2", test: "/projects", extra: &[] },
    Service { id: "slack", name: "Slack", base: "https://slack.com/api", test: "/auth.test", extra: &[] },
    Service { id: "homeassistant", name: "Home Assistant", base: "", test: "/api/", extra: &[] },
    Service { id: "discord", name: "Discord", base: "", test: "", extra: &[] },
];

#[derive(Serialize, Clone)]
pub struct Conn {
    id: &'static str,
    name: &'static str,
    connected: bool,
    /// who it's signed in as, or where
    account: String,
}

fn target(id: &str) -> Vec<u16> {
    format!("ULTRON app: {id}").encode_utf16().chain(Some(0)).collect()
}

/// (the key, and the address for a service you give one for)
fn read(id: &str) -> Option<(String, String)> {
    let t = target(id);
    let mut p: *mut CREDENTIALW = std::ptr::null_mut();
    unsafe {
        CredReadW(PCWSTR(t.as_ptr()), CRED_TYPE_GENERIC, None, &mut p).ok()?;
        let c = &*p;
        let key = String::from_utf8_lossy(std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize)).to_string();
        let addr = if c.UserName.is_null() { String::new() } else { c.UserName.to_string().unwrap_or_default() };
        CredFree(p as _);
        Some((key, addr))
    }
}

fn write(id: &str, key: &str, addr: &str) -> bool {
    let mut t = target(id);
    let mut user: Vec<u16> = addr.encode_utf16().chain(Some(0)).collect();
    let mut blob = key.as_bytes().to_vec();
    let c = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(t.as_mut_ptr()),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: PWSTR(user.as_mut_ptr()),
        ..Default::default()
    };
    unsafe { CredWriteW(&c, 0) }.is_ok()
}

fn service(id: &str) -> Result<&'static Service, String> {
    SERVICES.iter().find(|s| s.id == id).ok_or_else(|| format!("No app called {id} to connect to."))
}

/// One request to a connected service: its own address with the path given, and its key.
fn call_with(s: &Service, key: &str, addr: &str, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, String), String> {
    if path.contains("://") || path.contains('@') || (!path.is_empty() && !path.starts_with('/')) {
        return Err("A path on the service is needed (starting with /), not another address.".into());
    }
    let base = if s.base.is_empty() { addr.trim_end_matches('/') } else { s.base };
    if !base.starts_with("https://") && !(s.id == "homeassistant" && base.starts_with("http://")) {
        return Err(format!("{}'s address isn't a web address.", s.name));
    }
    let mut req = crate::net::agent(Duration::from_secs(20)).request(method, &format!("{base}{path}")).set("User-Agent", "ULTRON");
    if s.id != "discord" {
        req = req.set("Authorization", &format!("Bearer {key}"));
    }
    for (k, v) in s.extra {
        req = req.set(k, v);
    }
    let resp = match body {
        Some(b) => req.set("Content-Type", "application/json").send_string(&b.to_string()),
        None => req.call(),
    };
    match resp {
        Ok(r) => Ok((r.status(), r.into_string().unwrap_or_default())),
        Err(ureq::Error::Status(code, r)) => Ok((code, r.into_string().unwrap_or_default())),
        Err(e) => Err(format!("{} didn't answer ({e}).", s.name)),
    }
}

/// Who a working key belongs to, from the service's own answer.
fn account_of(id: &str, body: &str, addr: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let s = |p: &[&str]| p.iter().fold(Some(&v), |a, k| a.and_then(|x| x.get(*k))).and_then(|x| x.as_str()).map(String::from);
    match id {
        "github" => s(&["login"]),
        "notion" => s(&["bot", "owner", "user", "name"]).or_else(|| s(&["name"])),
        "slack" => s(&["user"]).map(|u| format!("{u} in {}", s(&["team"]).unwrap_or_default())),
        "todoist" => v.as_array().map(|a| format!("{} projects", a.len())),
        "discord" => s(&["name"]).map(|n| format!("webhook {n}")),
        _ => None,
    }
    .unwrap_or_else(|| addr.trim_start_matches("https://").trim_start_matches("http://").to_string())
}

pub fn list() -> Vec<Conn> {
    SERVICES
        .iter()
        .map(|s| {
            let got = read(s.id);
            let account = got.as_ref().map(|(_, a)| a.split('|').nth(1).unwrap_or("").to_string()).unwrap_or_default();
            Conn { id: s.id, name: s.name, connected: got.is_some(), account }
        })
        .collect()
}

/// Sign in with a key (and an address, for the services that need one): it's tried first, and kept only if it works.
pub fn set(id: &str, key: &str, addr: &str) -> Result<String, String> {
    let s = service(id)?;
    let (key, addr) = (key.trim(), addr.trim().trim_end_matches('/'));
    if s.id == "discord" {
        if !addr.starts_with("https://discord.com/api/webhooks/") && !addr.starts_with("https://discordapp.com/api/webhooks/") {
            return Err("Paste the webhook's address (it starts https://discord.com/api/webhooks/).".into());
        }
    } else if key.is_empty() {
        return Err(format!("Paste the key from {} first.", s.name));
    }
    if s.base.is_empty() && addr.is_empty() {
        return Err(format!("{}'s address is needed too.", s.name));
    }
    let (code, body) = call_with(s, key, addr, "GET", s.test, None)?;
    if code == 401 || code == 403 {
        return Err(format!("{} didn't accept that key.", s.name));
    }
    if code >= 400 || (s.id == "slack" && body.contains("\"ok\":false")) {
        return Err(format!("{} said no ({code}): {}", s.name, body.chars().take(140).collect::<String>()));
    }
    let who = account_of(s.id, &body, addr);
    // (the address and who it's signed in as ride along in the credential's user name, "address|who")
    if !write(id, key, &format!("{addr}|{who}")) {
        return Err("Windows Credential Manager wouldn't keep the key.".into());
    }
    crate::log_line(&format!("connections: {} connected ({who})", s.name));
    Ok(who)
}

pub fn forget(id: &str) {
    let t = target(id);
    let _ = unsafe { CredDeleteW(PCWSTR(t.as_ptr()), CRED_TYPE_GENERIC, None) };
}

/// The task agent's tool: a request to a connected service. What it says back is given as it came (cut short).
pub fn call(a: &Value) -> Result<String, String> {
    let st = |k: &str| a.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let s = service(&st("service"))?;
    let Some((key, addr)) = read(s.id) else {
        return Err(format!("{} isn't connected. It can be, in Settings > Tasks > App connections.", s.name));
    };
    let addr = addr.split('|').next().unwrap_or("").to_string();
    let method = match st("method").to_uppercase().as_str() {
        m @ ("GET" | "POST" | "PATCH" | "PUT" | "DELETE") => m.to_string(),
        _ => "GET".to_string(),
    };
    let body = a.get("body").filter(|b| !b.is_null());
    let (code, text) = call_with(s, &key, &addr, &method, &st("path"), body)?;
    crate::log_line(&format!("connections: {method} {} {} -> {code}", s.name, st("path")));
    let text: String = text.chars().take(6000).collect();
    if code >= 400 { Err(format!("{} answered {code}: {text}", s.name)) } else { Ok(format!("{} answered {code}: {text}", s.name)) }
}

/* ---------------- your own apps and files (the Connect tab) ---------------- */

/// Windows' own Open box, for files (several) or one folder.
pub fn pick(owner: Option<isize>, folder: bool) -> Vec<String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, FOS_ALLOWMULTISELECT, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH};
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let mut out = Vec::new();
        let _ = (|| -> windows::core::Result<()> {
            let d: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
            d.SetOptions(d.GetOptions()? | FOS_FORCEFILESYSTEM | if folder { FOS_PICKFOLDERS } else { FOS_ALLOWMULTISELECT })?;
            d.SetTitle(&windows::core::HSTRING::from(if folder { "Choose a folder for ULTRON to work with" } else { "Choose files for ULTRON to work with" }))?;
            d.SetOkButtonLabel(&windows::core::HSTRING::from("Connect"))?;
            d.Show(owner.map(|h| HWND(h as _)))?;
            let items = d.GetResults()?;
            for i in 0..items.GetCount()? {
                let p = items.GetItemAt(i)?.GetDisplayName(SIGDN_FILESYSPATH)?;
                out.push(p.to_string().unwrap_or_default());
                CoTaskMemFree(Some(p.0 as _));
            }
            Ok(())
        })();
        out
    })
    .join()
    .unwrap_or_default()
}

/// What a connected file says, for the brain to answer from: the text of a text file, code, a Word document, or the
/// names of what a folder holds. Cut to what a question can use.
pub fn file_text(path: &str) -> Result<String, String> {
    let p = std::path::Path::new(path);
    if p.is_dir() {
        let mut names: Vec<String> = std::fs::read_dir(p).map_err(|e| e.to_string())?.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        names.sort();
        let n = names.len();
        names.truncate(120);
        return Ok(format!("The folder holds {n} things: {}", names.join(", ")));
    }
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let text = if ext == "docx" {
        // (a Word document is a zip; its words are in word/document.xml)
        use std::os::windows::process::CommandExt;
        let script = "Add-Type -A System.IO.Compression.FileSystem; $z = [IO.Compression.ZipFile]::OpenRead($args[0]); $e = $z.GetEntry('word/document.xml'); $r = New-Object IO.StreamReader($e.Open()); $x = $r.ReadToEnd(); $z.Dispose(); ($x -replace '</w:p>', [char]10 -replace '<[^>]+>', '')";
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", script, path])
            .creation_flags(0x0800_0000)
            .output()
            .map_err(|e| e.to_string())?;
        String::from_utf8_lossy(&out.stdout).to_string()
    } else if ["pdf", "xlsx", "pptx", "png", "jpg", "jpeg", "mp4", "mp3", "zip", "exe"].contains(&ext.as_str()) {
        return Err(format!("I can't read the words in a .{ext} file yet."));
    } else {
        let bytes = std::fs::read(p).map_err(|e| e.to_string())?;
        String::from_utf8_lossy(&bytes[..bytes.len().min(400_000)]).to_string()
    };
    let t: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    Ok(t.chars().take(9000).collect())
}

/* ---------------- what's connected, for tasks ---------------- */

/// The apps and files connected in the Connect tab (the page says, whenever they change).
static LINKED: std::sync::Mutex<(Vec<String>, Vec<String>)> = std::sync::Mutex::new((Vec::new(), Vec::new()));

pub fn set_linked(apps: Vec<String>, files: Vec<String>) {
    *LINKED.lock().unwrap() = (apps, files);
}

/// A word to the task agent about them: which apps it can work in by name, and which files it's been given.
pub fn linked_note() -> String {
    let (apps, files) = LINKED.lock().unwrap().clone();
    let mut s = String::new();
    if !apps.is_empty() {
        s.push_str(&format!(" The user has connected these apps for tasks: {}. In them, work through ui_read (to see what's there, by name) and ui_do (to act on one control by its name): it's exact and quick; switch to or open the app first.", apps.join(", ")));
    }
    if !files.is_empty() {
        s.push_str(&format!(" Files and folders the user has connected: {}.", files.join("; ")));
    }
    s
}

/// The request names a connected app, and asks for more than one thing ("in Excel, ... and then ...").
pub fn names_linked_app(text: &str) -> bool {
    let l = format!(" {} ", text.to_lowercase());
    let apps = LINKED.lock().unwrap().0.clone();
    let named = apps.iter().any(|a| {
        let a = a.to_lowercase();
        let first = a.split_whitespace().next().unwrap_or("").to_string();
        l.contains(&format!(" {a} ")) || l.contains(&format!(" {a},")) || (first.len() >= 4 && (l.contains(&format!(" {first} ")) || l.contains(&format!(" {first},"))))
    });
    named && [" and ", " then ", ", then", "; "].iter().any(|w| l.contains(w))
}
