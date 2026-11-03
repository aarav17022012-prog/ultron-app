//! Cleaner (Settings, Cleaner): the junk Windows and your apps leave behind, found and cleared.
//!
//! Only places that are made to be thrown away are touched, and only what's inside them: temporary files more than a
//! day old, browsers' and apps' caches (pictures and scripts they fetch again by themselves), crash dumps and error
//! reports, and the download caches developer tools keep (npm, pip, yarn, NuGet: fetched again when a project needs
//! them). Never documents, downloads, settings, passwords, cookies or history. A browser or app that's open is left
//! alone (its cache is in use), and anything Windows won't let go of is skipped. The Recycle Bin is only emptied when
//! you tick it (what's in there can still come back until then).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;
use tauri::{AppHandle, Emitter};

#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub id: String,
    pub name: String,
    pub what: String,
    pub bytes: u64,
    pub files: u64,
    /// ticked to start with
    pub on: bool,
    /// what was left alone, and why ("Edge is open")
    pub note: String,
}

static RUNNING: AtomicBool = AtomicBool::new(false);
const DAY: Duration = Duration::from_secs(24 * 3600);

/// One place to clear: everything under `dir` (or, with `age`, only what's older than that).
struct Spot {
    dir: PathBuf,
    age: Option<Duration>,
}

fn env(k: &str) -> PathBuf {
    PathBuf::from(std::env::var(k).unwrap_or_default())
}

fn running() -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                out.push(String::from_utf16_lossy(&e.szExeFile).trim_end_matches('\0').to_lowercase());
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// A Chromium browser's (or Electron app's) caches: per profile, and the shared shader caches.
fn chromium(user_data: &Path, out: &mut Vec<Spot>) {
    let Ok(rd) = std::fs::read_dir(user_data) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            for sub in ["Cache", "Code Cache", "GPUCache", "DawnGraphiteCache", "DawnWebGPUCache"] {
                let d = p.join(sub);
                if d.is_dir() {
                    out.push(Spot { dir: d, age: None });
                }
            }
        }
    }
    for sub in ["ShaderCache", "GrShaderCache", "GraphiteDawnCache"] {
        let d = user_data.join(sub);
        if d.is_dir() {
            out.push(Spot { dir: d, age: None });
        }
    }
}

/// Every group, with where it lives (and what was left alone).
fn plan() -> Vec<(Group, Vec<Spot>)> {
    let (l, a, temp) = (env("LOCALAPPDATA"), env("APPDATA"), std::env::temp_dir());
    let procs = running();
    let open = |exe: &str| procs.iter().any(|p| p == exe);
    let mut groups = Vec::new();

    // temporary files a day old or more (SYLVER's own, and Claude's, are left alone)
    let mut t = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&temp) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_lowercase();
            if n.starts_with("ultron") || n == "claude" {
                continue;
            }
            t.push(Spot { dir: e.path(), age: Some(DAY) });
        }
    }
    t.push(Spot { dir: PathBuf::from(std::env::var("SystemRoot").unwrap_or("C:\\Windows".into())).join("Temp"), age: Some(DAY) });
    t.push(Spot { dir: l.join("Microsoft\\Windows\\INetCache"), age: None });
    groups.push((Group { id: "temp".into(), name: "Temporary files".into(), what: "Left behind by installers and apps, a day old or more".into(), on: true, ..Default::default() }, t));

    // browsers' caches (not their history, cookies or passwords), only for browsers that are closed
    let mut b = Vec::new();
    let mut skipped = Vec::new();
    for (name, exe, dir) in [
        ("Chrome", "chrome.exe", l.join("Google\\Chrome\\User Data")),
        ("Edge", "msedge.exe", l.join("Microsoft\\Edge\\User Data")),
        ("Brave", "brave.exe", l.join("BraveSoftware\\Brave-Browser\\User Data")),
        ("Vivaldi", "vivaldi.exe", l.join("Vivaldi\\User Data")),
        ("Opera", "opera.exe", a.join("Opera Software\\Opera Stable")),
        ("Opera GX", "opera.exe", a.join("Opera Software\\Opera GX Stable")),
    ] {
        if !dir.is_dir() {
            continue;
        }
        if open(exe) {
            skipped.push(name);
            continue;
        }
        if name.starts_with("Opera") {
            for sub in ["Cache", "Code Cache", "GPUCache"] {
                if dir.join(sub).is_dir() {
                    b.push(Spot { dir: dir.join(sub), age: None });
                }
            }
        } else {
            chromium(&dir, &mut b);
        }
    }
    // Firefox keeps its cache in each profile's cache2
    let ff = l.join("Mozilla\\Firefox\\Profiles");
    if ff.is_dir() {
        if open("firefox.exe") {
            skipped.push("Firefox");
        } else if let Ok(rd) = std::fs::read_dir(&ff) {
            for e in rd.flatten() {
                let d = e.path().join("cache2");
                if d.is_dir() {
                    b.push(Spot { dir: d, age: None });
                }
            }
        }
    }
    groups.push((Group { id: "browsers".into(), name: "Browser caches".into(), what: "Pictures and scripts from websites, fetched again as you browse. Your history, sign-ins and passwords stay".into(), on: true, note: open_note(&skipped), ..Default::default() }, b));

    // apps' caches
    let mut c = Vec::new();
    let mut skipped = Vec::new();
    for (name, exe, dir) in [
        ("Discord", "discord.exe", a.join("discord")),
        ("VS Code", "code.exe", a.join("Code")),
        ("Cursor", "cursor.exe", a.join("Cursor")),
        ("Slack", "slack.exe", a.join("Slack")),
        ("Teams", "teams.exe", a.join("Microsoft\\Teams")),
        ("Steam", "steamwebhelper.exe", l.join("Steam")),
    ] {
        if !dir.is_dir() {
            continue;
        }
        if open(exe) {
            skipped.push(name);
            continue;
        }
        if name == "Steam" {
            c.push(Spot { dir: dir.join("htmlcache"), age: None });
            continue;
        }
        for sub in ["Cache", "Code Cache", "GPUCache", "CachedData", "Service Worker\\CacheStorage"] {
            let d = dir.join(sub);
            if d.is_dir() && !(sub.starts_with("Service") && name != "Discord" && name != "Slack") {
                c.push(Spot { dir: d, age: None });
            }
        }
    }
    groups.push((Group { id: "apps".into(), name: "App caches".into(), what: "Discord, VS Code, Steam and the like keep copies of what they load".into(), on: true, note: open_note(&skipped), ..Default::default() }, c));

    // crash dumps and error reports
    let pd = env("ProgramData");
    groups.push((
        Group { id: "crash".into(), name: "Crash dumps and error reports".into(), what: "Kept after apps or Windows crashed, for sending to their makers".into(), on: true, ..Default::default() },
        vec![
            Spot { dir: l.join("CrashDumps"), age: None },
            Spot { dir: l.join("Microsoft\\Windows\\WER\\ReportArchive"), age: None },
            Spot { dir: l.join("Microsoft\\Windows\\WER\\ReportQueue"), age: None },
            Spot { dir: pd.join("Microsoft\\Windows\\WER\\ReportArchive"), age: None },
            Spot { dir: pd.join("Microsoft\\Windows\\WER\\ReportQueue"), age: None },
        ],
    ));

    // developer tools' download caches
    groups.push((
        Group { id: "dev".into(), name: "Download caches".into(), what: "Packages npm, pip, yarn and NuGet keep after installing. Fetched again when a project needs them".into(), on: true, ..Default::default() },
        vec![
            Spot { dir: l.join("npm-cache"), age: None },
            Spot { dir: l.join("pip\\cache"), age: None },
            Spot { dir: l.join("Yarn\\Cache"), age: None },
            Spot { dir: l.join("NuGet\\v3-cache"), age: None },
        ],
    ));

    // the Recycle Bin: measured through the shell, emptied only when ticked
    groups.push((Group { id: "bin".into(), name: "Recycle Bin".into(), what: "Things you deleted. Off unless you tick it: until it's emptied they can still come back".into(), on: false, ..Default::default() }, Vec::new()));
    groups
}

fn open_note(skipped: &[&str]) -> String {
    match skipped.len() {
        0 => String::new(),
        1 => format!("{} is open, so its cache is left for now", skipped[0]),
        _ => format!("{} are open, so their caches are left for now", skipped.join(", ").rsplitn(2, ", ").collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" and ")),
    }
}

/// A file or folder that is really there (not a link to somewhere else).
fn plain(p: &Path) -> Option<std::fs::Metadata> {
    use std::os::windows::fs::MetadataExt;
    let m = std::fs::symlink_metadata(p).ok()?;
    // (reparse points: junctions and links are never followed or cleared)
    (m.file_attributes() & 0x400 == 0).then_some(m)
}

fn old_enough(m: &std::fs::Metadata, age: Option<Duration>) -> bool {
    match age {
        None => true,
        Some(a) => m.modified().ok().and_then(|t| SystemTime::now().duration_since(t).ok()).map(|d| d >= a).unwrap_or(false),
    }
}

/// Walk a place: every file it would clear, with its size (`act` decides what happens to each). Sizes, dates and
/// kinds come from the folder listing itself, so no file is opened to look at it (opening each one made a look
/// through 28,000 files take 27 seconds).
fn walk(p: &Path, age: Option<Duration>, depth: u32, act: &mut dyn FnMut(&Path, u64) -> bool) {
    let Some(m) = plain(p) else { return };
    if m.is_file() {
        if old_enough(&m, age) {
            act(p, m.len());
        }
        return;
    }
    if m.is_dir() {
        inside(p, age, depth, act);
    }
}

fn inside(dir: &Path, age: Option<Duration>, depth: u32, act: &mut dyn FnMut(&Path, u64) -> bool) {
    use std::os::windows::fs::MetadataExt;
    if depth > 24 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(m) = e.metadata() else { continue };
        if m.file_attributes() & 0x400 != 0 {
            continue; // (junctions and links are never followed or cleared)
        }
        if m.is_dir() {
            inside(&e.path(), age, depth + 1, act);
        } else if old_enough(&m, age) {
            act(&e.path(), m.len());
        }
    }
}

fn bin_size() -> (u64, u64) {
    use windows::Win32::UI::Shell::{SHQueryRecycleBinW, SHQUERYRBINFO};
    let mut i = SHQUERYRBINFO { cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32, ..Default::default() };
    unsafe {
        if SHQueryRecycleBinW(windows::core::PCWSTR::null(), &mut i).is_ok() {
            return (i.i64Size.max(0) as u64, i.i64NumItems.max(0) as u64);
        }
    }
    (0, 0)
}

/// How much each group would free (nothing is touched). The groups are looked through side by side, and each is
/// told to the page as soon as it's done ("clean-scan"), so its row fills in while the rest are still being looked at.
pub fn scan(app: &AppHandle) -> Vec<Group> {
    let groups = plan();
    let n = groups.len();
    let (tx, rx) = std::sync::mpsc::channel();
    for (i, (mut g, spots)) in groups.into_iter().enumerate() {
        let (tx, app) = (tx.clone(), app.clone());
        std::thread::spawn(move || {
            if g.id == "bin" {
                let (b, n) = bin_size();
                g.bytes = b;
                g.files = n;
            } else {
                for s in &spots {
                    walk(&s.dir, s.age, 0, &mut |_, n| {
                        g.bytes += n;
                        g.files += 1;
                        true
                    });
                }
            }
            let _ = app.emit_to("main", "clean-scan", &g);
            let _ = tx.send((i, g));
        });
    }
    drop(tx);
    let mut out: Vec<(usize, Group)> = rx.iter().take(n).collect();
    out.sort_by_key(|x| x.0);
    out.into_iter().map(|x| x.1).collect()
}

/// Clear the ticked groups. As it goes, the page hears what's being shredded (a file's name now and then, and
/// each group's total), for its animation; the answer is how much was freed in all.
pub fn clean(app: &AppHandle, ids: &[String]) -> u64 {
    if RUNNING.swap(true, SeqCst) {
        return 0;
    }
    let mut total = 0u64;
    let mut last = Instant::now() - Duration::from_secs(1);
    for (g, spots) in plan() {
        if !ids.contains(&g.id) {
            continue;
        }
        let mut freed = 0u64;
        if g.id == "bin" {
            use windows::Win32::UI::Shell::{SHEmptyRecycleBinW, SHERB_NOCONFIRMATION, SHERB_NOPROGRESSUI, SHERB_NOSOUND};
            let (b, _) = bin_size();
            let ok = unsafe { SHEmptyRecycleBinW(None, windows::core::PCWSTR::null(), SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND).is_ok() };
            if ok {
                freed = b;
            }
            let _ = app.emit_to("main", "clean-file", serde_json::json!({ "group": g.id, "name": "Recycle Bin", "bytes": freed }));
        } else {
            for s in &spots {
                walk(&s.dir, s.age, 0, &mut |p, n| {
                    if std::fs::remove_file(p).is_ok() {
                        freed += n;
                        // (a name now and then is enough for the page: dozens a second would only queue up)
                        if last.elapsed() > Duration::from_millis(90) {
                            last = Instant::now();
                            let name = p.file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_default();
                            let _ = app.emit_to("main", "clean-file", serde_json::json!({ "group": g.id, "name": name, "bytes": total + freed }));
                        }
                    }
                    true
                });
                prune(&s.dir, s.age, 0);
            }
        }
        total += freed;
        let _ = app.emit_to("main", "clean-group", serde_json::json!({ "id": g.id, "freed": freed, "total": total }));
        crate::log_line(&format!("cleaner: {} freed {:.1} MB", g.name, freed as f64 / 1_048_576.0));
    }
    RUNNING.store(false, SeqCst);
    total
}

/// Folders emptied by the clean go too (inside the place, never the place itself, and only old ones in temp).
fn prune(p: &Path, age: Option<Duration>, depth: u32) {
    let Some(m) = plain(p) else { return };
    if !m.is_dir() || depth > 24 {
        return;
    }
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            prune(&e.path(), age, depth + 1);
        }
    }
    if depth > 0 && old_enough(&m, age) {
        let _ = std::fs::remove_dir(p); // (only succeeds when it's empty)
    }
}
