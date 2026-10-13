//! Turns what you said into an action on this PC. Everything runs locally:
//! no cloud, no API key. Unknown requests are reported, never guessed at.

use serde::Serialize;
use tauri::{AppHandle, Manager};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Shutdown::LockWorkStation;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_D, VK_LWIN,
};

use crate::apps::{shell_open, Apps};
use crate::media::{Cmd, Media};

#[derive(Serialize, Clone)]
pub struct Reply {
    pub reply: String,
    pub action: String, // short line for the Activity tab ("" = nothing happened)
    pub icon: &'static str,
    pub ok: bool,
}

fn done(reply: impl Into<String>, action: impl Into<String>, icon: &'static str) -> Reply {
    Reply { reply: reply.into(), action: action.into(), icon, ok: true }
}
fn say(reply: impl Into<String>) -> Reply {
    Reply { reply: reply.into(), action: String::new(), icon: "chat", ok: true }
}
/// small talk: the local AI answers these better when it is loaded
fn talk(reply: impl Into<String>) -> Reply {
    Reply { reply: reply.into(), action: String::new(), icon: "smalltalk", ok: true }
}
fn fail(reply: impl Into<String>) -> Reply {
    Reply { reply: reply.into(), action: String::new(), icon: "chat", ok: false }
}

/* ---------------- text helpers ---------------- */

pub fn normalize(t: &str) -> String {
    let t = t.to_lowercase().replace(['’', '‘'], "'");
    let t: String = t.chars().map(|c| if c.is_alphanumeric() || c == '\'' || c == '%' || c == '.' || c == ' ' { c } else { ' ' }).collect();
    let mut t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    t = t.trim_end_matches('.').to_string();
    for pre in ["hey ultron", "ok ultron", "okay ultron", "ultron", "hey there", "hey", "hi", "yo", "okay", "ok", "so", "please", "can you", "could you", "would you", "will you", "i want you to", "i'd like you to", "go ahead and", "and"] {
        if let Some(r) = t.strip_prefix(pre) {
            if r.is_empty() || r.starts_with(' ') {
                t = r.trim().to_string();
            }
        }
    }
    for suf in ["please", "for me", "now", "right now"] {
        if let Some(r) = t.strip_suffix(suf) {
            if r.ends_with(' ') {
                t = r.trim().to_string();
            }
        }
    }
    t.trim_matches('.').trim().to_string()
}

fn strip_any<'a>(t: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    prefixes.iter().find_map(|p| t.strip_prefix(p).filter(|r| r.is_empty() || r.starts_with(' ')).map(|r| r.trim()))
}

fn number_in(t: &str) -> Option<u32> {
    let words = [("zero", 0), ("ten", 10), ("twenty", 20), ("thirty", 30), ("forty", 40), ("fifty", 50), ("sixty", 60), ("seventy", 70), ("eighty", 80), ("ninety", 90), ("hundred", 100), ("half", 50), ("max", 100), ("maximum", 100), ("full", 100)];
    for w in t.split(|c: char| !c.is_alphanumeric()) {
        if let Ok(n) = w.parse::<u32>() {
            return Some(n);
        }
    }
    words.iter().find(|(w, _)| t.split(' ').any(|x| x == *w)).map(|(_, n)| *n)
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b' ' => "+".into(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn title_case(s: &str) -> String {
    s.split(' ')
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (row[j + 1] + 1).min(row[j] + 1).min(prev + if ca == *cb { 0 } else { 1 });
            prev = cur;
        }
    }
    row[b.len()]
}

/* ---------------- system actions ---------------- */

fn endpoint() -> Option<IAudioEndpointVolume> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let dev = en.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        dev.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok()
    }
}

fn volume() -> Option<f32> {
    unsafe { endpoint()?.GetMasterVolumeLevelScalar().ok() }
}

fn set_volume(v: f32) -> bool {
    unsafe {
        let Some(e) = endpoint() else { return false };
        let _ = e.SetMute(false, std::ptr::null());
        e.SetMasterVolumeLevelScalar(v.clamp(0.0, 1.0), std::ptr::null()).is_ok()
    }
}

fn set_mute(m: bool) -> bool {
    unsafe { endpoint().map(|e| e.SetMute(m, std::ptr::null()).is_ok()).unwrap_or(false) }
}

fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() }, ..Default::default() } },
    }
}

pub(crate) fn type_text(text: &str) {
    let mut inputs = Vec::new();
    for u in text.encode_utf16() {
        for up in [false, true] {
            inputs.push(INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT { wScan: u, dwFlags: if up { KEYEVENTF_UNICODE | KEYEVENTF_KEYUP } else { KEYEVENTF_UNICODE }, ..Default::default() },
                },
            });
        }
    }
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
}

fn show_desktop() {
    unsafe { SendInput(&[key(VK_LWIN, false), key(VK_D, false), key(VK_D, true), key(VK_LWIN, true)], std::mem::size_of::<INPUT>() as i32) };
}

/// The first real (not sponsored) video for a YouTube search.
pub fn youtube_first(q: &str) -> Option<String> {
    let url = format!("https://www.youtube.com/results?search_query={}", url_encode(q));
    let body = crate::net::agent(std::time::Duration::from_secs(6))
        .get(&url)
        .set("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36")
        .set("Accept-Language", "en-US,en;q=0.9")
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let key = "\"videoRenderer\":{\"videoId\":\"";
    let i = body.find(key)? + key.len();
    let id: String = body[i..].chars().take(11).collect();
    (id.len() == 11 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')).then_some(id)
}

/* ---------------- targets ---------------- */

fn website(name: &str) -> Option<(&'static str, &'static str)> {
    let sites: [(&[&str], &str, &str); 18] = [
        (&["youtube"], "YouTube", "https://www.youtube.com"),
        (&["google"], "Google", "https://www.google.com"),
        (&["gmail", "my email", "email", "mail"], "Gmail", "https://mail.google.com"),
        (&["github"], "GitHub", "https://github.com"),
        (&["chat gpt", "chatgpt"], "ChatGPT", "https://chatgpt.com"),
        (&["claude"], "Claude", "https://claude.ai"),
        (&["netflix"], "Netflix", "https://www.netflix.com"),
        (&["reddit"], "Reddit", "https://www.reddit.com"),
        (&["twitter", "x"], "X", "https://x.com"),
        (&["instagram"], "Instagram", "https://www.instagram.com"),
        (&["facebook"], "Facebook", "https://www.facebook.com"),
        (&["linkedin"], "LinkedIn", "https://www.linkedin.com"),
        (&["google maps", "maps"], "Google Maps", "https://maps.google.com"),
        (&["google drive", "drive"], "Google Drive", "https://drive.google.com"),
        (&["wikipedia"], "Wikipedia", "https://www.wikipedia.org"),
        (&["amazon"], "Amazon", "https://www.amazon.in"),
        (&["whatsapp web"], "WhatsApp Web", "https://web.whatsapp.com"),
        (&["youtube music"], "YouTube Music", "https://music.youtube.com"),
    ];
    sites.iter().find(|(keys, _, _)| keys.contains(&name)).map(|(_, n, u)| (*n, *u))
}

fn folder(name: &str) -> Option<(String, String)> {
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    let n = name.trim_start_matches("my ").trim_end_matches(" folder");
    let (label, path) = match n {
        "downloads" | "download" => ("Downloads", format!("{home}\\Downloads")),
        "documents" | "document" => ("Documents", format!("{home}\\Documents")),
        "desktop" => ("Desktop", format!("{home}\\Desktop")),
        "pictures" | "photos folder" => ("Pictures", format!("{home}\\Pictures")),
        "music" => ("Music", format!("{home}\\Music")),
        "videos" => ("Videos", format!("{home}\\Videos")),
        "this pc" | "my computer" | "computer" => ("This PC", "shell:MyComputerFolder".into()),
        "recycle bin" | "trash" | "bin" => ("Recycle Bin", "shell:RecycleBinFolder".into()),
        "file explorer" | "explorer" | "files" | "file manager" => ("File Explorer", "explorer.exe".into()),
        "settings" => ("Settings", "ms-settings:".into()),
        "task manager" => ("Task Manager", "taskmgr.exe".into()),
        "control panel" => ("Control Panel", "control.exe".into()),
        _ => return None,
    };
    Some((label.into(), path))
}

const ALIASES: [(&str, &str); 14] = [
    ("chrome", "google chrome"),
    ("edge", "microsoft edge"),
    ("vs code", "visual studio code"),
    ("vscode", "visual studio code"),
    ("code", "visual studio code"),
    ("word", "word"),
    ("excel", "excel"),
    ("powerpoint", "powerpoint"),
    ("power point", "powerpoint"),
    ("command prompt", "command prompt"),
    ("cmd", "command prompt"),
    ("terminal", "terminal"),
    ("store", "microsoft store"),
    ("calculator", "calculator"),
];

fn index_has(apps: &Apps, name: &str) -> bool {
    apps.index.read().unwrap().iter().any(|e| e.name.eq_ignore_ascii_case(name))
}

fn find_app(apps: &Apps, query: &str) -> Option<(String, String)> {
    let mut q = ALIASES.iter().find(|(a, _)| *a == query).map(|(_, b)| b.to_string()).unwrap_or_else(|| query.to_string());
    for pre in ["microsoft ", "google ", "the ", "my ", "app "] {
        if let Some(r) = q.strip_prefix(pre) {
            if index_has(apps, r) {
                q = r.to_string();
            }
        }
    }
    let index = apps.index.read().unwrap();
    let bad = ["uninstall", "readme", "help", "documentation", "website", "release notes", "license"];
    let mut best: Option<(i32, &crate::apps::AppEntry)> = None;
    for e in index.iter() {
        let n = e.name.to_lowercase();
        if bad.iter().any(|b| n.contains(b)) {
            continue;
        }
        let score = if n == q {
            100
        } else if n.starts_with(&format!("{q} ")) {
            90 - (n.len() as i32 - q.len() as i32).min(30)
        } else if n.split(' ').any(|w| w == q) {
            75 - (n.len() as i32 - q.len() as i32).min(30)
        } else if n.contains(&q) {
            60 - (n.len() as i32 - q.len() as i32).min(30)
        } else if n.len() >= 4 && q.split(' ').collect::<Vec<_>>().windows(n.split(' ').count()).any(|w| w.join(" ") == n) {
            50 // "open microsoft word" -> "Word"
        } else {
            let d = levenshtein(&n, &q) as i32;
            let tol = (q.len() as i32 / 4).max(1);
            if d <= tol { 55 - d * 5 } else { 0 }
        };
        if score > 0 && best.map(|(s, _)| score > s).unwrap_or(true) {
            best = Some((score, e));
        }
    }
    best.map(|(_, e)| (e.name.clone(), e.launch.clone()))
}

fn open_thing(app: &AppHandle, what: &str) -> Reply {
    let what = what.trim_start_matches("the ").trim_start_matches("up ").trim();
    if what.is_empty() {
        return fail("Open what?");
    }
    if let Some((n, u)) = website(what) {
        shell_open(u);
        return done(format!("Opening {n}."), format!("Opened {n}"), "globe");
    }
    if let Some((label, path)) = folder(what) {
        shell_open(&path);
        return done(format!("Opening {label}."), format!("Opened {label}"), "folder");
    }
    let compact = what.replace(" dot ", ".").replace(' ', "");
    if [".com", ".org", ".net", ".io", ".in", ".dev", ".ai", ".co"].iter().any(|t| compact.ends_with(t)) {
        shell_open(&format!("https://{compact}"));
        return done(format!("Opening {compact}."), format!("Opened {compact}"), "globe");
    }
    let apps = app.state::<Apps>();
    if let Some((name, launch)) = find_app(&apps, what) {
        if shell_open(&launch) {
            return done(format!("Opening {name}."), format!("Opened {name}"), "app");
        }
        return fail(format!("I found {name}, but Windows wouldn't open it."));
    }
    fail(format!("I couldn't find an app called “{}”.", title_case(what)))
}

/* ---------------- the interpreter ---------------- */

pub fn run(app: &AppHandle, raw: &str) -> Reply {
    let t = normalize(raw);
    if t.is_empty() {
        return say("I'm listening.");
    }
    let media = app.state::<Media>();
    let has = |w: &str| t.split(' ').any(|x| x == w);

    // time and date
    if t.contains("what time") || t == "time" || t.contains("the time") {
        let now = chrono_like_now();
        return say(format!("It's {}.", now.0));
    }
    if t.contains("what day") || t.contains("the date") || t.contains("today's date") || t.contains("what's today") {
        let now = chrono_like_now();
        return say(format!("It's {}.", now.1));
    }

    // volume
    if has("volume") || has("louder") || has("quieter") || t.starts_with("turn it") || t == "mute" || t == "unmute" || t.starts_with("mute ") || t.starts_with("unmute ") {
        if t.starts_with("unmute") {
            set_mute(false);
            return done("Sound's back on.", "Unmuted sound", "volume");
        }
        if t.starts_with("mute") || t.contains("volume off") {
            set_mute(true);
            return done("Muted.", "Muted sound", "mute");
        }
        let cur = volume().unwrap_or(0.5);
        let n = number_in(&t);
        let up = has("up") || has("louder") || has("increase") || has("raise") || has("higher");
        let down = has("down") || has("quieter") || has("decrease") || has("lower") || has("reduce");
        let target = if up {
            cur + n.map(|n| n as f32 / 100.0).unwrap_or(0.1)
        } else if down {
            cur - n.map(|n| n as f32 / 100.0).unwrap_or(0.1)
        } else if let Some(n) = n {
            n as f32 / 100.0
        } else {
            return say(format!("Volume is at {}%.", (cur * 100.0).round()));
        };
        let target = target.clamp(0.0, 1.0);
        set_volume(target);
        let pct = (target * 100.0).round();
        return done(format!("Volume {pct}%."), format!("Set volume to {pct}%"), "volume");
    }

    // media
    let music = ["", "music", "the music", "the song", "song", "it", "playback", "the video", "video", "spotify"];
    if let Some(r) = strip_any(&t, &["pause", "stop"]) {
        if music.contains(&r) {
            media.send(Cmd::Pause);
            return done("Paused.", "Paused playback", "pause");
        }
    }
    if let Some(r) = strip_any(&t, &["resume", "continue", "unpause", "play"]) {
        if music.contains(&r) {
            media.send(Cmd::Play);
            return done("Playing.", "Resumed playback", "play");
        }
    }
    if ["next", "skip", "next song", "skip song", "skip this", "skip this song", "next track", "skip track", "play the next song", "play next song"].contains(&t.as_str()) {
        media.send(Cmd::Next);
        return done("Skipping.", "Skipped to next track", "next");
    }
    if ["previous", "previous song", "previous track", "last song", "go back", "play the previous song", "play previous song", "back"].contains(&t.as_str()) {
        media.send(Cmd::Prev);
        return done("Going back.", "Went to previous track", "prev");
    }
    if let Some(q) = strip_any(&t, &["play"]) {
        // Spotify when you ask for it, or when it's the player you're using (unless you said YouTube)
        let spotify_is_player = media.state.lock().unwrap().sessions.iter().any(|s| s.app.to_lowercase().contains("spotify"));
        let spotify_q = q.strip_suffix(" on spotify").or_else(|| (spotify_is_player && !q.ends_with(" on youtube") && !q.contains("video")).then_some(q));
        if let Some(q) = spotify_q {
            return match crate::agent::spotify_play(app, q) {
                Ok(what) => done(format!("{what}."), what, "music"),
                Err(why) => {
                    shell_open(&format!("spotify:search:{}", url_encode(q).replace('+', "%20")));
                    done(format!("{why} I've searched Spotify for it instead."), format!("Searched Spotify for “{q}”"), "music")
                }
            };
        }
        let q = q.strip_suffix(" on youtube").unwrap_or(q);
        if let Some(id) = youtube_first(q) {
            shell_open(&format!("https://www.youtube.com/watch?v={id}"));
            return done(format!("Playing {q} on YouTube."), format!("Played “{q}” on YouTube"), "play");
        }
        shell_open(&format!("https://www.youtube.com/results?search_query={}", url_encode(q)));
        return done(format!("Finding {q} on YouTube."), format!("Searched YouTube for “{q}”"), "play");
    }

    // web search
    if let Some(q) = strip_any(&t, &["search youtube for", "search on youtube for", "youtube search"]).or_else(|| {
        strip_any(&t, &["search for", "search"]).and_then(|q| q.strip_suffix(" on youtube"))
    }) {
        shell_open(&format!("https://www.youtube.com/results?search_query={}", url_encode(q)));
        return done(format!("Searching YouTube for {q}."), format!("Searched YouTube for “{q}”"), "search");
    }
    if let Some(q) = strip_any(&t, &["search google for", "google search for", "search the web for", "search for", "search", "google", "look up", "find out"]) {
        let q = q.strip_suffix(" on google").unwrap_or(q);
        if !q.is_empty() {
            shell_open(&format!("https://www.google.com/search?q={}", url_encode(q)));
            return done(format!("Searching for {q}."), format!("Searched the web for “{q}”"), "search");
        }
    }

    // typing into the focused app
    if let Some(r) = strip_any(raw.trim(), &["Type", "type", "Write", "write", "Dictate", "dictate"]) {
        let text = &crate::dictate::marks(r.trim_start_matches([',', ':']).trim()); // ("comma", "question mark" said aloud)
        if !text.is_empty() {
            type_text(text);
            return done("Typed it.", format!("Typed “{text}”"), "type");
        }
    }

    // windows and system
    if ["lock", "lock my computer", "lock the computer", "lock my pc", "lock the pc", "lock screen", "lock the screen", "lock my laptop"].contains(&t.as_str()) {
        unsafe {
            let _ = LockWorkStation();
        }
        return done("Locking.", "Locked the PC", "lock");
    }
    if t.contains("screenshot") || t.contains("screen shot") {
        shell_open("ms-screenclip:");
        return done("Pick an area to capture.", "Started a screenshot", "camera");
    }
    if ["show desktop", "show the desktop", "minimize everything", "minimize all", "hide everything", "go to desktop"].contains(&t.as_str()) {
        show_desktop();
        return done("There's your desktop.", "Showed the desktop", "app");
    }
    if let Some(what) = strip_any(&t, &["close", "quit", "exit"]) {
        let what = what.trim_start_matches("the ");
        let apps = app.state::<Apps>();
        let what = ALIASES.iter().find(|(a, _)| *a == what).map(|(_, b)| *b).unwrap_or(what);
        return match apps.quit_by_name(what) {
            Some(n) => done(format!("Closing {n}."), format!("Closed {n}"), "close"),
            None => fail(format!("{} isn't open.", title_case(what))),
        };
    }

    // opening things
    if let Some(what) = strip_any(&t, &["open up", "open", "launch", "start", "run", "go to", "take me to", "show me", "bring up", "pull up"]) {
        return open_thing(app, what);
    }

    // small talk
    if ["hi", "hello", "hey", "yo", "hello there", "hi there"].contains(&t.as_str()) {
        return talk("Hey. What do you need?");
    }
    if t.contains("how are you") || t.contains("how's it going") || t.contains("how are things") || t.contains("what's up") {
        return talk("Running smoothly. What can I do for you?");
    }
    if t.contains("who are you") || t.contains("what are you") || t.contains("your name") {
        return talk("I'm ULTRON. I run right here on your PC.");
    }
    if t.contains("what can you do") || t == "help" {
        return talk("Open apps and sites, search, control music and volume, type for you, take screenshots, lock the PC.");
    }
    if t.starts_with("thank") {
        return talk("Anytime.");
    }

    fail(format!("I heard “{}”, but I don't know how to do that yet.", raw.trim()))
}

/// (time, date) in local time without pulling in a date crate
pub fn now_strings() -> (String, String) {
    chrono_like_now()
}

fn chrono_like_now() -> (String, String) {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let st = unsafe { GetLocalTime() };
    let (h, m) = (st.wHour as u32, st.wMinute as u32);
    let ampm = if h >= 12 { "PM" } else { "AM" };
    let h12 = if h % 12 == 0 { 12 } else { h % 12 };
    let days = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    let months = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
    (
        format!("{h12}:{m:02} {ampm}"),
        format!("{}, {} {}", days[st.wDayOfWeek as usize % 7], months[(st.wMonth as usize).saturating_sub(1) % 12], st.wDay),
    )
}
