#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent;
mod ai;
mod appbar;
mod browser;
mod clip;
mod apps;
mod commands;
mod ghost;
mod ppt;
mod skills;
mod songs;
mod hit;
mod backdrop;
mod calls;
mod keepbelow;
mod cursorfx;
mod shelf;
mod lid;
mod kids;
mod rec;
mod squeeze;
mod dictate;
mod dock;
mod keys;
mod lights;
mod cts;
mod clock;
mod notes;
mod spotkey;
mod cleaner;
mod veil;
mod wall;
mod icons;
mod cloud;
mod pick;
mod mail;
mod routines;
mod chats;
mod doer;
mod tools;
mod memory;
mod quick;
mod smooth;
mod router;
mod uam;
mod see;
mod intro;
mod lyriceng;
mod lyrics;
mod media;
mod net;
mod sys;
mod vision;
mod voice;
mod tts;
mod web;
mod procs;
mod coders;
mod minfx;
mod signin;
mod dialogs;
mod osd;
mod alttab;
mod desks;
mod connect;
mod teach;
mod uplink;
mod link;
mod sounds;
mod watchdog;
mod words;

use std::time::Duration;

use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, RunEvent, State, WebviewWindow, WindowEvent,
};
use windows::core::HSTRING;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::CreateMutexW;

use apps::Apps;
use hit::{Hits, Rect};
use media::{Cmd, Media};
use voice::Voice;

/// CSS px the menu bar keeps free at the top of the screen, and the window height
/// (taller, so the notch can open downward over other windows).
const BAR_HEIGHT: f64 = 32.0;
const WINDOW_HEIGHT: f64 = 700.0;

/* ---------------- shared ---------------- */

#[tauri::command]
fn set_hit_rects(window: WebviewWindow, hits: State<Hits>, rects: Vec<Rect>) {
    hits.0.lock().unwrap().insert(window.label().to_string(), rects);
}

/// The log (ultron.log in the temp folder) is moved to ultron.old.log at start once it has grown past 2 MB,
/// replacing the one before, so it never grows without end. A panic anywhere is written to it too.
fn start_log() {
    let log = std::env::temp_dir().join("ultron.log");
    if std::fs::metadata(&log).map(|m| m.len() > 2_000_000).unwrap_or(false) {
        let _ = std::fs::rename(&log, std::env::temp_dir().join("ultron.old.log"));
    }
    let before = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log_line(&format!("panic: {info}"));
        before(info);
    }));
}

pub fn log_line(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(std::env::temp_dir().join("ultron.log")) {
        let _ = writeln!(f, "{msg}");
    }
}

/// Is this a developer run (started with ULTRON_DEV_INBOX=1)?
#[tauri::command]
fn dev_mode() -> bool {
    std::env::var("ULTRON_DEV_INBOX").is_ok()
}

#[tauri::command]
fn page_log(msg: String) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(std::env::temp_dir().join("ultron.log")) {
        let _ = writeln!(f, "[page] {msg}");
    }
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    quit(&app);
}

/// The introduction speaks with ULTRON's real voice, which lives in the main window.
#[tauri::command]
fn intro_say(app: AppHandle, text: String) {
    let _ = app.emit_to("main", "intro-say", text);
}

/// What you said while the introduction asked your name.
/// What this PC has, and the brains it runs.
#[tauri::command]
fn ai_specs() -> serde_json::Value {
    let s = router::specs();
    serde_json::json!({ "specs": s, "tier": format!("{:?}", router::tier()), "text": router::text_model().name, "vision": router::vision_model().name,
        "quick": quick::device(), "saving": quick::power_saving(), "cap": if quick::device() == Some(false) { quick::card_cap().map(|(a, b)| [a, b]) } else { None } })
}

#[tauri::command]
fn intro_heard(app: AppHandle, text: String) {
    let _ = app.emit_to("intro", "intro-heard", text);
}

#[tauri::command]
async fn intro_finish(app: AppHandle, name: Option<String>) {
    intro::finish(&app, name);
}

#[tauri::command]
async fn intro_play(app: AppHandle) {
    intro::play(&app);
}

/// The ringing call: answer it, decline it, or decline with a message.
#[tauri::command]
async fn call_answer(app: AppHandle) -> bool {
    tauri::async_runtime::spawn_blocking(move || calls::answer(&app)).await.unwrap_or(false)
}

#[tauri::command]
async fn call_decline(app: AppHandle) -> bool {
    tauri::async_runtime::spawn_blocking(move || calls::decline(&app)).await.unwrap_or(false)
}

#[tauri::command]
async fn call_reply(app: AppHandle, text: String) -> String {
    tauri::async_runtime::spawn_blocking(move || calls::reply(&app, &text)).await.unwrap_or_default()
}

/// Your own key (cloud.rs): what's set, check and keep a key, flip the switch or change the model, throw a key away.
#[tauri::command]
fn cloud_status(app: AppHandle) -> serde_json::Value {
    cloud::status(&app)
}

#[tauri::command]
async fn cloud_connect(app: AppHandle, provider: String, key: Option<String>, base: Option<String>, model: Option<String>) -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(move || cloud::connect(&app, &provider, &key.unwrap_or_default(), &base.unwrap_or_default(), &model.unwrap_or_default()))
        .await
        .unwrap_or_else(|_| Err("Something went wrong.".into()))
}

#[tauri::command]
fn cloud_set(app: AppHandle, on: Option<bool>, model: Option<String>) {
    cloud::set(&app, on, model);
}

#[tauri::command]
fn cloud_forget(app: AppHandle, provider: String) {
    cloud::forget(&app, &provider);
}

/// Picking text for a tool: watch what's being selected (see pick.rs), or stop watching.
#[tauri::command]
fn pick_start(app: AppHandle) {
    pick::start(app);
}

#[tauri::command]
fn pick_stop() {
    pick::stop();
}

/// Email: is an account connected, and how many of the latest messages are unread.
#[tauri::command]
fn mail_status(app: AppHandle) -> serde_json::Value {
    match mail::account(&app) {
        Some(a) => serde_json::json!({ "connected": true, "email": a.email, "unread": mail::unread() }),
        None => serde_json::json!({ "connected": false }),
    }
}

#[tauri::command]
async fn mail_connect(app: AppHandle, email: String, pass: String, host: Option<String>) -> Result<usize, String> {
    tauri::async_runtime::spawn_blocking(move || mail::connect(&app, &email, &pass, host)).await.unwrap_or_else(|_| Err("Something went wrong.".into()))
}

#[tauri::command]
async fn mail_list(app: AppHandle, refresh: bool) -> Result<Vec<mail::Mail>, String> {
    tauri::async_runtime::spawn_blocking(move || mail::inbox(&app, refresh)).await.unwrap_or_else(|_| Err("Something went wrong.".into()))
}

#[tauri::command]
async fn mail_summary(app: AppHandle, uid: u32) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || mail::summary(&app, uid)).await.ok().flatten()
}

#[tauri::command]
async fn mail_digest(app: AppHandle) -> String {
    tauri::async_runtime::spawn_blocking(move || mail::digest(&app, false)).await.unwrap_or_default()
}

#[tauri::command]
fn mail_body(uid: u32) -> Option<String> {
    mail::body(uid)
}

/// Screen recording (rec.rs): start a file, add the next second to it, close it; and press ULTRON's own Record button.
#[tauri::command]
fn rec_begin(app: AppHandle, ext: String) -> Result<String, String> {
    let r = rec::begin(&app, &ext);
    if r.is_ok() {
        hide_share_notice();
    }
    r
}

/// While a recording runs, the web engine puts up its own notice at the bottom of the screen ("http://tauri.localhost
/// is sharing your screen.", with Stop and Hide): it isn't ULTRON's, it named the page's address, and it was in the
/// recording itself. It's hidden as soon as it appears, and again if it comes back, until the recording ends.
static RECORDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn hide_share_notice() {
    use std::sync::atomic::Ordering::SeqCst;
    if RECORDING.swap(true, SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        use windows::Win32::Foundation::{HWND, LPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW, IsWindowVisible, ShowWindow, SW_HIDE};
        unsafe extern "system" fn each(h: HWND, _: LPARAM) -> windows::core::BOOL {
            unsafe {
                if IsWindowVisible(h).as_bool() {
                    let mut buf = [0u16; 200];
                    let n = GetWindowTextW(h, &mut buf);
                    let t = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
                    if t.contains("tauri.localhost") && t.contains("sharing") {
                        let _ = ShowWindow(h, SW_HIDE);
                    }
                }
            }
            true.into()
        }
        let t0 = std::time::Instant::now();
        while RECORDING.load(SeqCst) && t0.elapsed() < std::time::Duration::from_secs(6 * 3600) {
            unsafe {
                let _ = EnumWindows(Some(each), LPARAM(0));
            }
            // (looked for often just as it starts, then now and then)
            std::thread::sleep(std::time::Duration::from_millis(if t0.elapsed().as_secs() < 5 { 40 } else { 1000 }));
        }
        RECORDING.store(false, SeqCst);
    });
}

#[tauri::command]
fn rec_data(b64: String) -> bool {
    rec::data(&b64)
}

#[tauri::command]
fn rec_end(secs: f64) -> Option<(String, f64)> {
    RECORDING.store(false, std::sync::atomic::Ordering::SeqCst);
    rec::end(secs)
}

#[tauri::command]
fn rec_sound(app: AppHandle, on: bool) -> u32 {
    rec::sound(&app, on)
}

#[tauri::command]
fn rec_keys() -> Vec<&'static str> {
    rec::keys()
}

#[tauri::command]
async fn rec_click(app: AppHandle, x: f64, y: f64) {
    let _ = tauri::async_runtime::spawn_blocking(move || rec::click(&app, x, y)).await;
}

#[tauri::command]
fn rec_folder(app: AppHandle) -> String {
    rec::folder(&app).to_string_lossy().to_string()
}

#[tauri::command]
fn mail_signout(app: AppHandle) {
    mail::sign_out(&app);
}

/// Routines, for Settings › Tasks.
#[tauri::command]
fn routines_list(app: AppHandle) -> Vec<serde_json::Value> {
    routines::list(&app).into_iter().map(|r| serde_json::json!({ "id": r.id.to_string(), "task": r.task, "when": routines::when(&r), "on": r.on })).collect()
}

#[tauri::command]
fn routine_set(app: AppHandle, id: String, on: Option<bool>, delete: Option<bool>, run: Option<bool>) {
    let Ok(id) = id.parse::<u64>() else { return };
    if delete == Some(true) {
        routines::delete(&app, id);
    } else if let Some(on) = on {
        routines::set_on(&app, id, on);
    } else if run == Some(true) {
        if let Some(r) = routines::list(&app).into_iter().find(|r| r.id == id) {
            routines::fire(app.clone(), r);
        }
    }
}

/// Wake the big brain ahead of need (the Language menu opened).
#[tauri::command]
fn ai_wake(app: AppHandle) {
    ai::wake(&app);
}

/// First launch on this PC? (The page asks once it has loaded; true only the first time.)
#[tauri::command]
fn intro_due(app: AppHandle) -> bool {
    intro::due(&app)
}

fn quit(app: &AppHandle) {
    let _ = app.emit("app-quit", ());
    minfx::heal();
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(560));
        appbar::remove();
        dock::leave(&app);
        app.exit(0);
    });
}

/* ---------------- voice + assistant ---------------- */

#[tauri::command]
fn voice_status(voice: State<Voice>) -> voice::VoiceStatus {
    voice.status()
}

#[tauri::command]
fn voice_config(voice: State<Voice>, paused: Option<bool>, sens: Option<f32>, prompt: Option<String>, wake: Option<bool>, patient: Option<bool>, lang: Option<String>) {
    if let Some(l) = lang {
        let mut cur = voice.lang.lock().unwrap();
        if *cur != l {
            *cur = l.clone();
            voice.reload.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        *ai::LANG.lock().unwrap() = l;
    }
    if let Some(p) = patient {
        voice.patient.store(p, std::sync::atomic::Ordering::SeqCst);
    }
    if let Some(w) = wake {
        voice.wake.store(w, std::sync::atomic::Ordering::SeqCst);
    }
    if let Some(p) = paused {
        voice.set_paused(p);
    }
    if let Some(s) = sens {
        voice.set_sens(s);
    }
    if let Some(p) = prompt {
        voice.set_prompt(p);
    }
}

#[tauri::command]
async fn run_command(app: AppHandle, text: String) -> commands::Reply {
    tauri::async_runtime::spawn_blocking(move || commands::run(&app, &text)).await.unwrap_or_else(|_| commands::Reply {
        reply: "Something went wrong.".into(),
        action: String::new(),
        icon: "chat",
        ok: false,
    })
}

/// Fixed commands first (instant); anything else, or small talk, goes to the local AI.
#[tauri::command]
async fn assist(app: AppHandle, text: String) -> commands::Reply {
    tauri::async_runtime::spawn_blocking(move || handle(app, text))
        .await
        .unwrap_or_else(|_| commands::Reply { reply: "Something went wrong.".into(), action: String::new(), icon: "chat", ok: false })
}

/// Everything ULTRON does with a request, in order.
pub(crate) fn handle(app: AppHandle, text: String) -> commands::Reply {
    {
        let _request = ai::begin_request();
        // slips of the keyboard and chat shorthand are set right before anything reads the request
        let text = skills::tidy(&text);
        // AI Tools' own requests about your selection ("rewrite the selected text to…", "translate…", "explain…",
        // "draft a reply…") go straight to the work: nothing else could be meant by them
        {
            let l = text.to_lowercase();
            if l.contains("the selected text") {
                if let Some((how, paste)) = skills::writing(&l) {
                    return agent::execute(&app, vec![agent::Step::Rewrite(how, paste)]);
                }
            }
        }
        // "Danish's chat is open. What should I say?" -> what you say now is the message
        if let Some((chat_app, to)) = skills::asked_message(&text) {
            return agent::execute(&app, vec![agent::Step::Message(chat_app, to, text.trim().to_string())]);
        }
        // "yes" / "no" to something a task asked before changing the system
        if let Some(r) = doer::answer_pending(&app, &text) {
            return r;
        }
        // "remember that…", "what do you remember", "forget…"
        if let Some(said) = memory::command(&app, &text) {
            // (what ULTRON knows about you is part of what its quick brain keeps ready: refresh that now, quietly)
            let a = app.clone();
            std::thread::spawn(move || ai::quick_ready(&a));
            return commands::Reply { reply: said, action: String::new(), icon: "chat", ok: true };
        }
        // "what did we talk about?", "what was my last question?", "forget our chats"
        if let Some(said) = chats::command(&app, &text) {
            return commands::Reply { reply: said, action: String::new(), icon: "chat", ok: true };
        }
        // "every weekday at 9, open my work apps", "list my routines", "delete the gym routine"
        if let Some(said) = routines::command(&app, &text) {
            return commands::Reply { reply: said, action: String::new(), icon: "chat", ok: true };
        }
        // "check my email", "sum up my inbox", "read my latest email"
        if let Some(said) = mail::command(&app, &text) {
            ai::remember_chat(&app, &text, &said);
            return commands::Reply { reply: said, action: String::new(), icon: "chat", ok: true };
        }
        memory::notice(&app, &text);
        // "build me ... on Lovable / Bolt / v0 / AI Studio": straight to that builder, with the request
        if let Some(r) = doer::build_online(&app, &text) {
            ai::remember_chat(&app, &text, &r.reply);
            return r;
        }
        // bigger jobs (files, Excel, Word, research): the task agent plans them step by step with tools
        if doer::wants(&text) || doer::wants_build(&text) || (doer::wants_big(&text) && cloud::active(&app).is_some()) {
            if let Some(r) = doer::run(&app, &text) {
                ai::remember_chat(&app, &text, &r.reply);
                return r;
            }
        }
        // several steps, or keyboard and mouse work: ULTRON's own cursor does the job
        if let Some(steps) = agent::plan(&text) {
            let r = agent::execute(&app, steps);
            let r = if !r.ok && r.action.is_empty() && cloud::active(&app).is_some() { doer::run(&app, &text).unwrap_or(r) } else { r };
            ai::remember_chat(&app, &text, &r.reply);
            return r;
        }
        let r = commands::run(&app, &text);
        if r.ok && r.icon != "smalltalk" {
            ai::remember_chat(&app, &text, &r.reply);
            return r;
        }
        // the action model says it's something to do, but the rules couldn't read it:
        // the brain rewrites it in ULTRON's command language, then the agent does it
        if let Some((kind, conf)) = uam::classify(&text) {
            crate::log_line(&format!("action model: {kind} ({conf:.2}) for {text:?}"));
            // "give me 5 ideas for a gift", "tell me a recipe": an answer for you, not something to type into an app
            // (it once tried to type the ideas into whatever window was in front)
            let low = format!(" {} ", text.to_lowercase());
            let answer_me = ai::wants_steps(&text) && ![" type ", " write ", " draft ", " in notepad", " in word", " into ", " open "].iter().any(|w| low.contains(w));
            // "Who invented the light bulb?": a question that starts with who/when/why/what/which/where/how and isn't
            // about this PC is a question. (The action model called that one "system" and it went to the task agent.)
            let first = low.split_whitespace().next().unwrap_or("").trim_matches(|c: char| !c.is_alphanumeric());
            let about_pc = [" my ", " this pc", " this computer", " this laptop", " the pc", " the computer", " screen", " window", " file", " folder", " app", " disk", " drive", " battery", " wifi", " wi-fi",
                " volume", " brightness", " ram ", " memory", " cpu", " gpu", " running", " installed", " download", " clipboard", " desktop", " settings"].iter().any(|w| low.contains(w));
            let question = ["who", "whom", "whose", "when", "why", "what", "whats", "what's", "which", "where", "how", "is", "are", "was", "were", "can", "could", "does", "do", "did", "will", "would", "should"].contains(&first) && !about_pc;
            if kind != "chat" && conf >= 0.55 && !answer_me && !question {
                // files and the system are jobs for the task agent; one clear thing to do (message someone,
                // open, play, type...) is turned straight into a command, which takes half a second
                // (and with your own key on, its planner takes any job: far better at them than the rules)
                let big = matches!(kind, "files" | "system") || cloud::active(&app).is_some();
                if big {
                    if let Some(r) = doer::run(&app, &text) {
                        ai::remember_chat(&app, &text, &r.reply);
                        return r;
                    }
                }
                if let Some(lines) = ai::plan_actions(&app, &text) {
                    let steps: Vec<agent::Step> = lines.iter().map(|l| agent::parse_step(l)).collect();
                    let r = agent::execute(&app, steps);
                    let r = if !r.ok && r.action.is_empty() && cloud::active(&app).is_some() { doer::run(&app, &text).unwrap_or(r) } else { r };
                    ai::remember_chat(&app, &text, &r.reply);
                    return r;
                }
            }
        }
        if let Some(a) = ai::ask(&app, &text) {
            return a;
        }
        let st = app.state::<ai::Ai>().status();
        if r.ok {
            return r;
        }
        let why = match st.state.as_str() {
            "downloading" => format!("My local brain is still downloading ({}%). Give me a few minutes.", (st.progress * 100.0).round()),
            "loading" => "My local brain is still waking up. Ask me again in a few seconds.".to_string(),
            _ => r.reply.clone(),
        };
        commands::Reply { reply: why, ..r }
    }
}

/// For testing without touching the screen: started with ULTRON_DEV_INBOX=1, ULTRON takes requests
/// from %TEMP%\ultron-inbox.txt and logs its replies (Office files are made without opening windows).
fn dev_inbox(app: AppHandle) {
    if std::env::var("ULTRON_DEV_INBOX").is_err() {
        return;
    }
    tools::QUIET.store(true, std::sync::atomic::Ordering::SeqCst);
    std::thread::spawn(move || {
        let path = std::env::temp_dir().join("ultron-inbox.txt");
        log_line("dev inbox: listening");
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let _ = std::fs::remove_file(&path);
            let text = text.trim().to_string();
            if text.is_empty() {
                continue;
            }
            // "!open", "!ask what's 2+2", "!menu clip": drive the interface itself
            if let Some(ui) = text.strip_prefix('!') {
                let _ = app.emit_to("main", "dev-ui", ui.trim());
                continue;
            }
            let t0 = std::time::Instant::now();
            log_line(&format!("dev inbox: {text:?}"));
            let r = handle(app.clone(), text);
            log_line(&format!("dev inbox reply ({} s, ok={}): {} [{}]", t0.elapsed().as_secs(), r.ok, r.reply, r.action));
        }
    });
}

#[tauri::command]
fn ai_warm(app: AppHandle) {
    ai::warm(&app);
    // (the voice engine is no longer started ahead of time: ULTRON answers in writing now, and it held memory for nothing)
}

/// Settings that change how the brain and tasks behave.
#[tauri::command]
fn ai_config(app: AppHandle, cfg: ai::Config) {
    ai::set_config(&app, cfg);
}

/// Forget the conversation so far.
#[tauri::command]
fn ai_forget(app: AppHandle, ai: State<ai::Ai>) {
    ai.forget();
    chats::clear(&app);
}

/// "Send it" / "Cancel" for a message ULTRON drafted.
#[tauri::command]
async fn agent_confirm(app: AppHandle, ok: bool) -> String {
    tauri::async_runtime::spawn_blocking(move || agent::confirm(&app, ok)).await.unwrap_or_default()
}

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

/// Start ULTRON when you sign in to Windows.
#[tauri::command]
fn set_autostart(on: bool) -> bool {
    use std::os::windows::process::CommandExt;
    let mut c = std::process::Command::new("reg");
    if on {
        let exe = std::env::current_exe().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        c.args(["add", RUN_KEY, "/v", "ULTRON", "/t", "REG_SZ", "/d", &format!("\"{exe}\" --at-login"), "/f"]);
    } else {
        c.args(["delete", RUN_KEY, "/v", "ULTRON", "/f"]);
    }
    c.creation_flags(0x0800_0000).output().map(|o| o.status.success()).unwrap_or(false)
}

/// Started by Windows as you signed in (after a restart, say), not opened by hand: the page shows the lock screen
/// then, when it's ULTRON's. An entry for starting with Windows written before this knew nothing of the mark, so
/// it's written again with it.
#[tauri::command]
fn at_login() -> bool {
    let marked = std::env::args().any(|a| a == "--at-login");
    if get_autostart() {
        use std::os::windows::process::CommandExt;
        let now = std::process::Command::new("reg").args(["query", RUN_KEY, "/v", "ULTRON"]).creation_flags(0x0800_0000).output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        if !now.contains("--at-login") {
            set_autostart(true);
        }
    }
    marked
}

#[tauri::command]
fn get_autostart() -> bool {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("reg")
        .args(["query", RUN_KEY, "/v", "ULTRON"])
        .creation_flags(0x0800_0000)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Listen to the song: line the lyrics up with the video, or write down the words when there are no lyrics.
#[tauri::command]
fn song_listen(app: AppHandle, req: songs::Request) {
    songs::request(&app, req);
}

/// The red stop button: stop speaking, thinking and any task in progress.
#[tauri::command]
fn ai_stop() {
    ai::cancel();
}

/// Which voice ULTRON speaks with ("lessac", a woman's, or "ryan", a man's).
#[tauri::command]
fn tts_voice(app: AppHandle, name: String) {
    tts::set_voice(&app, &name);
}

/// The language ULTRON speaks back in (Settings > Voice): its voice for that language, fetched the first time.
#[tauri::command]
fn tts_lang(app: AppHandle, lang: String) {
    tts::set_tongue(&app, &lang);
}

/// One sentence in ULTRON's voice, as a WAV (base64).
#[tauri::command]
async fn tts(app: AppHandle, text: String) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || tts::speak(&app, &text)).await.ok().flatten()
}

#[tauri::command]
fn ai_status(ai: State<ai::Ai>) -> ai::AiStatus {
    ai.status()
}

#[tauri::command]
fn app_names(apps: State<Apps>) -> Vec<String> {
    let wanted = ["Spotify", "Google Chrome", "Microsoft Edge", "Visual Studio Code", "WhatsApp", "File Explorer", "Terminal", "Settings", "Notepad", "Calculator"];
    let index = apps.index.read().unwrap();
    wanted.iter().filter(|w| index.iter().any(|e| e.name.eq_ignore_ascii_case(w))).map(|w| w.to_string()).collect()
}

/* ---------------- media + lyrics ---------------- */

#[tauri::command]
fn media_state(media: State<Media>) -> media::MediaState {
    media.state.lock().unwrap().clone()
}

#[tauri::command]
fn media_control(media: State<Media>, action: String, value: Option<f64>, id: Option<String>) {
    let cmd = match action.as_str() {
        "toggle" => Cmd::Toggle,
        "play" => Cmd::Play,
        "pause" => Cmd::Pause,
        "next" => Cmd::Next,
        "prev" => Cmd::Prev,
        "seek" => Cmd::Seek(value.unwrap_or(0.0)),
        "select" => Cmd::Select(id.unwrap_or_default()),
        _ => return,
    };
    media.send(cmd);
}

#[tauri::command]
async fn get_lyrics(title: String, artist: String, album: String, duration: f64) -> Option<lyrics::Lyrics> {
    tauri::async_runtime::spawn_blocking(move || lyrics::fetch(&title, &artist, &album, duration)).await.ok().flatten()
}

/// A fresh backdrop for the glass, now (backdrop.rs).
#[tauri::command]
fn backdrop_now() {
    backdrop::kick();
}

/// The menu bar keeps its strip of the screen, or gives it up (it hides away: Settings, Look & feel).
#[tauri::command]
fn bar_reserve(on: bool) {
    appbar::reserve(on);
}

/// The icon of the app a song is playing from (media.rs).
#[tauri::command]
async fn media_icon(id: String) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || media::icon(&id)).await.ok().flatten()
}

/// A YouTube video's own captions, for the lyrics area (web.rs).
#[tauri::command]
async fn yt_captions(title: String, artist: String, duration: f64) -> Option<Vec<(f64, String)>> {
    tauri::async_runtime::spawn_blocking(move || web::yt_captions(&title, &artist, duration)).await.ok().flatten()
}

/* ---------------- glass, Wi-Fi, Start menu ---------------- */

#[tauri::command]
async fn wallpaper() -> Option<String> {
    tauri::async_runtime::spawn_blocking(sys::wallpaper_now).await.ok().flatten()
}

#[tauri::command]
async fn wifi_status(list: bool) -> sys::Wifi {
    tauri::async_runtime::spawn_blocking(move || sys::wifi_status(list)).await.unwrap_or_default()
}

#[tauri::command]
async fn wifi_connect(ssid: String) -> bool {
    tauri::async_runtime::spawn_blocking(move || sys::wifi_connect(&ssid)).await.unwrap_or(false)
}

#[tauri::command]
async fn wifi_disconnect() {
    let _ = tauri::async_runtime::spawn_blocking(sys::wifi_disconnect).await;
}

#[derive(serde::Serialize)]
struct StartApp {
    name: String,
    launch: String,
}

#[tauri::command]
fn start_apps(apps: State<Apps>) -> Vec<StartApp> {
    let skip = ["uninstall", "readme", "release notes", "documentation", "help", "license", "website"];
    let mut v: Vec<StartApp> = apps
        .index
        .read()
        .unwrap()
        .iter()
        .filter(|e| !skip.iter().any(|s| e.name.to_lowercase().contains(s)))
        .map(|e| StartApp { name: e.name.clone(), launch: e.launch.clone() })
        .collect();
    v.sort_by_key(|a| a.name.to_lowercase());
    v.dedup_by(|a, b| a.name.eq_ignore_ascii_case(&b.name));
    v
}

#[tauri::command]
async fn app_icons(sources: Vec<String>) -> Vec<String> {
    tauri::async_runtime::spawn_blocking(move || sys::icons(sources)).await.unwrap_or_default()
}

/// The Drop Shelf's Add files: Windows' Open box (in front of ULTRON); what's chosen goes on the shelf. The shelf as it is now.
#[tauri::command]
async fn shelf_pick(app: AppHandle) -> Vec<shelf::Item> {
    let owner = app.get_webview_window("main").and_then(|w| hit::hwnd_of(&w)).map(|h| h.0 as isize);
    tauri::async_runtime::spawn_blocking(move || {
        let paths = squeeze::pick_as(owner, false, "Choose files to keep on the Shelf", "Add to Shelf");
        if paths.is_empty() { shelf::list(&app) } else { shelf::add(&app, &paths) }
    })
    .await
    .unwrap_or_default()
}

/// The Drop Shelf: pin an app to it.
#[tauri::command]
async fn shelf_add_app(app: AppHandle, name: String, launch: String) -> Vec<shelf::Item> {
    tauri::async_runtime::spawn_blocking(move || shelf::add_app(&app, &name, &launch)).await.unwrap_or_default()
}

/// Files, beside Record: Windows' "Open" box, for the files to make smaller.
#[tauri::command]
async fn squeeze_pick(app: AppHandle) -> Vec<String> {
    let owner = app.get_webview_window("main").and_then(|w| hit::hwnd_of(&w)).map(|h| h.0 as isize);
    tauri::async_runtime::spawn_blocking(move || squeeze::pick(owner, false)).await.unwrap_or_default()
}

/// Wallpaper, in the bar: which one is on, and your own pictures.
#[tauri::command]
async fn wall_list(app: AppHandle) -> serde_json::Value {
    tauri::async_runtime::spawn_blocking(move || wall::list(&app)).await.unwrap_or_default()
}

/// Make one of ULTRON's wallpapers ("our:dusk") or a picture of yours (its path) the wallpaper.
#[tauri::command]
async fn wall_apply(app: AppHandle, pick: String, dry: Option<bool>, how: Option<u8>) -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(move || wall::apply(&app, &pick, dry.unwrap_or(false), how.unwrap_or(0))).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn wall_pick(app: AppHandle) -> Option<String> {
    let owner = app.get_webview_window("main").and_then(|w| hit::hwnd_of(&w)).map(|h| h.0 as isize);
    tauri::async_runtime::spawn_blocking(move || squeeze::pick(owner, true)).await.ok().and_then(|v| v.into_iter().next())
}

#[tauri::command]
fn wall_folder(app: AppHandle) -> String {
    wall::folder(&app).to_string_lossy().to_string()
}

#[tauri::command]
fn squeeze_go(app: AppHandle, items: Vec<(u32, String)>, tiny: bool) {
    squeeze::go(&app, items, tiny);
}

#[tauri::command]
fn squeeze_stop(app: AppHandle) {
    squeeze::stop(&app);
}

#[tauri::command]
fn squeeze_folder(app: AppHandle) -> String {
    squeeze::folder(&app).to_string_lossy().to_string()
}

#[tauri::command]
fn squeeze_show(app: AppHandle, path: String) {
    squeeze::show(&app, &path);
}

/// The notch's Today page: the weather outside (only asked for while "Look things up online" is on).
#[tauri::command]
async fn today_weather() -> Option<(String, String, String)> {
    tauri::async_runtime::spawn_blocking(web::weather_now).await.ok().flatten()
}

/// ULTRON's own taskbar on or off (Windows' is hidden while it's on). Gives the height of its strip of the screen.
#[tauri::command]
fn dock_set(app: AppHandle, on: bool) -> i32 {
    dock::set(&app, on)
}

#[tauri::command]
async fn dock_snap() -> Option<String> {
    tauri::async_runtime::spawn_blocking(dock::snap).await.ok().flatten()
}

#[tauri::command]
async fn dock_list(app: AppHandle) -> Vec<apps::OpenWin> {
    tauri::async_runtime::spawn_blocking(move || dock::list(&app)).await.unwrap_or_default()
}

/// The recent files an app's taskbar jump list would show (the wheel's right-click menu).
#[tauri::command]
async fn dock_recent(aumid: String) -> Vec<(String, String)> {
    tauri::async_runtime::spawn_blocking(move || dock::recent(&aumid, 6)).await.unwrap_or_default()
}

#[tauri::command]
async fn dock_act(id: String, what: String) {
    let _ = tauri::async_runtime::spawn_blocking(move || dock::act(&id, &what)).await;
}

#[tauri::command]
async fn dock_tile(app: AppHandle, gap: Option<i32>) -> usize {
    tauri::async_runtime::spawn_blocking(move || dock::tile(&app, gap.unwrap_or(10))).await.unwrap_or(0)
}

#[tauri::command]
async fn dock_tile_apps(app: AppHandle, launches: Vec<String>, gap: Option<i32>) -> usize {
    tauri::async_runtime::spawn_blocking(move || dock::tile_apps(&app, &launches, gap.unwrap_or(10))).await.unwrap_or(0)
}


/// A reply typed on a messaging app's notification that can't take one itself: the chat is opened and it's typed there.
#[tauri::command]
async fn notes_reply(app: AppHandle, id: u32, text: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || notes::reply(&app, id, &text)).await.map_err(|e| e.to_string())?
}

/// Joining a network with its password (ULTRON's own box asks for it, not Windows').
#[tauri::command]
async fn wifi_join(ssid: String, key: String, auth: String) -> bool {
    tauri::async_runtime::spawn_blocking(move || sys::wifi_join(&ssid, &key, &auth)).await.unwrap_or(false)
}

/// (developer runs only) ULTRON shows in screen captures for 20 s, live glass and all.
#[tauri::command]
fn dev_show() {
    if std::env::var("ULTRON_DEV_INBOX").is_ok() {
        backdrop::dev_show();
    }
}

/// The Drop Shelf (shelf.rs).
#[tauri::command]
async fn shelf_list(app: AppHandle) -> Vec<shelf::Item> {
    tauri::async_runtime::spawn_blocking(move || shelf::list(&app)).await.unwrap_or_default()
}

#[tauri::command]
async fn shelf_add(app: AppHandle, paths: Vec<String>) -> Vec<shelf::Item> {
    tauri::async_runtime::spawn_blocking(move || shelf::add(&app, &paths)).await.unwrap_or_default()
}

#[tauri::command]
async fn shelf_remove(app: AppHandle, path: String) -> Vec<shelf::Item> {
    tauri::async_runtime::spawn_blocking(move || shelf::remove(&app, &path)).await.unwrap_or_default()
}

#[tauri::command]
async fn shelf_clear(app: AppHandle) {
    let _ = tauri::async_runtime::spawn_blocking(move || shelf::clear(&app)).await;
}

/// A shelf item's picture (its thumbnail, or its icon).
#[tauri::command]
async fn shelf_thumb(path: String) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        sys::image_data_url(&path, 128, false)
    })
    .await
    .ok()
    .flatten()
}

/// Dragging a shelf item out into another app: Windows' own drag, run on the window's thread (a plain command runs
/// there), returning once it's dropped.
#[tauri::command]
fn shelf_drag(app: AppHandle, paths: Vec<String>) -> u32 {
    let Some(h) = app.get_webview_window("main").and_then(|w| hit::hwnd_of(&w)) else { return 0 };
    shelf::drag(h, &paths)
}

/// The notch has opened: the web-answer window is made ready on Google's page (it closes again when unused).
#[tauri::command]
async fn web_warm(app: AppHandle) {
    let _ = tauri::async_runtime::spawn_blocking(move || uplink::warm(&app)).await;
}

/// Key and click sounds on or off (sounds.rs).
#[tauri::command]
fn sounds_set(app: AppHandle, keys: bool, clicks: bool) {
    sounds::set(&app, keys, clicks);
}

/// Glass dialogs (dialogs.rs): Windows' plain message boxes shown as ULTRON's own.
#[tauri::command]
fn dialogs_set(app: AppHandle, on: bool) {
    dialogs::set(&app, on);
}

/// ULTRON's own volume indicator (osd.rs): the volume keys answered by ULTRON, not Windows.
#[tauri::command]
fn osd_set(app: AppHandle, on: bool) {
    osd::set(&app, on);
}

/// ULTRON's own Alt+Tab (alttab.rs), and the windows it shows, the one in front first.
#[tauri::command]
fn alttab_set(app: AppHandle, on: bool) {
    alttab::set(&app, on);
}

#[tauri::command]
async fn alttab_list(app: AppHandle) -> Vec<apps::OpenWin> {
    tauri::async_runtime::spawn_blocking(move || app.state::<apps::Apps>().open_windows()).await.unwrap_or_default()
}

/// App connections (connect.rs): the services signed in with a key of yours.
#[tauri::command]
fn connect_list() -> Vec<connect::Conn> {
    connect::list()
}

#[tauri::command]
async fn connect_set(id: String, key: String, addr: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || connect::set(&id, &key, &addr)).await.map_err(|e| e.to_string())?
}

/// Teach a task (teach.rs): watched once, done again on request.
#[tauri::command]
fn teach_start(app: AppHandle) {
    teach::start(&app);
}

#[tauri::command]
fn teach_stop(app: AppHandle, name: String) -> usize {
    teach::stop(&app, &name)
}

#[tauri::command]
fn teach_list(app: AppHandle) -> Vec<teach::Taught> {
    teach::list(&app)
}

#[tauri::command]
async fn teach_play(app: AppHandle, name: String) -> Result<usize, String> {
    tauri::async_runtime::spawn_blocking(move || teach::play(&app, &name)).await.map_err(|e| e.to_string())?
}

#[tauri::command]
fn teach_forget(app: AppHandle, name: String) {
    teach::forget(&app, &name);
}

#[tauri::command]
fn connect_forget(id: String) {
    connect::forget(&id);
}

#[tauri::command]
fn dialog_shown(hwnd: isize) {
    dialogs::shown(hwnd);
}

#[tauri::command]
fn dialog_press(hwnd: isize, id: i32) {
    dialogs::press(hwnd, id);
}

#[tauri::command]
fn dialog_pass(hwnd: isize) {
    dialogs::pass(hwnd);
}

/// The phone link (link.rs): phones running ULTRON on this network.
#[tauri::command]
async fn link_find() -> Result<Vec<link::Phone>, String> {
    tauri::async_runtime::spawn_blocking(|| link::find(Duration::from_millis(3000))).await.map_err(|e| e.to_string())?
}

/// Connect to one (hello both ways, the version agreed).
#[tauri::command]
async fn link_connect(app: AppHandle, name: String, host: String, port: u16) -> Result<link::State, String> {
    tauri::async_runtime::spawn_blocking(move || link::connect(&app, &link::Phone { name, host, port })).await.map_err(|e| e.to_string())?
}

/// Pair with the 6-digit code the phone shows.
#[tauri::command]
async fn link_pair(code: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || link::pair(&code)).await.map_err(|e| e.to_string())?
}

/// "On my phone, open WhatsApp": sent to the phone as a command; its result (ok, text).
#[tauri::command]
async fn link_command(text: String) -> Result<(bool, String), String> {
    tauri::async_runtime::spawn_blocking(move || link::command(&text)).await.map_err(|e| e.to_string())?
}

/// The laptop's answer to a command the phone sent.
#[tauri::command]
fn link_answer(id: String, ok: bool, text: String) {
    link::answer(&id, ok, &text);
}

#[tauri::command]
fn link_state() -> link::State {
    link::state()
}

#[tauri::command]
fn link_close() {
    link::close();
}

/// A picture in an email its server refused to the page, fetched here instead (a data address).
#[tauri::command]
async fn mail_image(url: String) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || mail::image(&url)).await.ok().flatten()
}

/// Dictation: a heard piece shaped to go after what's already there (`held`: the mark held back for its last sentence).
/// Settings > Dictation says how much is tidied.
#[tauri::command]
async fn dictate_shape(text: String, before: String, held: String, opts: dictate::Opts) -> dictate::Shaped {
    tauri::async_runtime::spawn_blocking(move || dictate::shape(&text, &before, &held, &opts)).await.unwrap_or_default()
}

/// Dictation tidied by the quick brain, when it answers within a few seconds (None: it isn't up or was slow, and the
/// rules do it).
#[tauri::command]
async fn dictate_ai(app: AppHandle, text: String) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(ai::dictation_polish(&app, &text));
    });
    tauri::async_runtime::spawn_blocking(move || rx.recv_timeout(Duration::from_millis(3500)).ok().flatten()).await.ok().flatten()
}

/// Dictation: the last piece typed taken back ("scratch that"), as many characters as it was.
#[tauri::command]
async fn dictate_erase(n: usize) {
    let _ = tauri::async_runtime::spawn_blocking(move || dictate::erase(n)).await;
}

/// Dictation's card: its words dragged off it (the button is down on it), to be dropped into a typing area in any app.
/// "words-drag" says where they are as they go, "words-drop" how it ended.
#[tauri::command]
fn words_drag(app: AppHandle, text: String) {
    words::drag(app, text);
}

/// Dictation's card: its words copied.
#[tauri::command]
fn words_copy(text: String) -> bool {
    clip::set(&text)
}

/// Dictation's card: its words typed where you were writing.
#[tauri::command]
async fn words_type(text: String) {
    let _ = tauri::async_runtime::spawn_blocking(move || dictate::type_out(&text)).await;
}

/// Show a shelf item in its folder.
#[tauri::command]
fn shelf_reveal(path: String) {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("explorer.exe").raw_arg(format!("/select,\"{path}\"")).creation_flags(0x0800_0000).spawn();
}

/// Motion blur for the mouse pointer (Look & feel), on or off.
#[tauri::command]
fn cursor_blur(on: bool) {
    cursorfx::set(on);
}

/// The voice-typing shortcut (the notch's settings).
#[tauri::command]
fn dictate_key(mods: u32, vk: u32) {
    spotkey::set_key(spotkey::DICTATE, mods, vk);
}

/// A window's picture, for the wheel to show beside its bead.
#[tauri::command]
async fn dock_peek(id: String) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || dock::peek(&id)).await.ok().flatten()
}

/// Circle to Search: the screen as it is now, held still (given back as a picture).
#[tauri::command]
async fn cts_grab() -> Option<String> {
    tauri::async_runtime::spawn_blocking(cts::grab).await.ok().flatten()
}

#[tauri::command]
fn cts_forget() {
    cts::forget();
}

/// The words in the part that was circled.
#[tauri::command]
async fn cts_read(x: i32, y: i32, w: i32, h: i32) -> Vec<String> {
    tauri::async_runtime::spawn_blocking(move || cts::read(x, y, w, h)).await.unwrap_or_default()
}

/// What ULTRON's eyes make of the part that was circled.
#[tauri::command]
async fn cts_ask(app: AppHandle, x: i32, y: i32, w: i32, h: i32, question: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || cts::ask(&app, x, y, w, h, &question)).await.map_err(|e| e.to_string())?
}

/// The part that was circled, looked up in Google Lens: the address of the results.
/// Google Lens, through Google's own page in your browser (the picture is put on the clipboard first, with cts_copy).
#[tauri::command]
async fn cts_lens() -> String {
    let how = tauri::async_runtime::spawn_blocking(cts::lens_open).await.unwrap_or("failed");
    log_line(&format!("circle to search: Google Lens {how}"));
    how.to_string()
}

/// Windows' sign-in as the PC wakes: `on` given, it's turned on or off (Settings > Look & feel > Lock screen); what it is
/// now either way (None: couldn't tell).
#[tauri::command]
async fn signin_wake(on: Option<bool>) -> Option<bool> {
    tauri::async_runtime::spawn_blocking(move || match on {
        Some(v) => signin::set(v),
        None => signin::asks(),
    })
    .await
    .ok()
    .flatten()
}

/// When Windows last started ULTRON as you signed in (for Settings > System).
#[tauri::command]
fn login_last() -> Option<String> {
    std::fs::read_to_string(login_note()).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn login_note() -> std::path::PathBuf {
    std::env::var("LOCALAPPDATA").map(std::path::PathBuf::from).unwrap_or_else(|_| std::env::temp_dir()).join("ULTRON").join("last-started-with-windows.txt")
}

/// Now, as people write it: "Thu 23 Oct, 17:05".
fn now_said() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let t = unsafe { GetLocalTime() };
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("{} {} {}, {:02}:{:02}", DAYS[(t.wDayOfWeek % 7) as usize], t.wDay, MONTHS[((t.wMonth.max(1) - 1) % 12) as usize], t.wHour, t.wMinute)
}


/// Agent watch (the notch's Settings): on or off, and which notifications count.
#[tauri::command]
fn agent_watch(on: bool, notes: bool, all: bool) {
    coders::set(on, notes, all);
}

/// Claude Code's hooks are connected.
#[tauri::command]
async fn agent_hooks_state() -> bool {
    tauri::async_runtime::spawn_blocking(coders::hooks_state).await.unwrap_or(false)
}

/// Connect Claude Code (or disconnect it): only ever from the button you press in Settings.
#[tauri::command]
async fn agent_hooks(on: bool) -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(move || coders::hooks_set(on)).await.map_err(|e| e.to_string())?
}

#[derive(serde::Serialize)]
struct QuickState {
    vol: f32,
    mute: bool,
    dark: bool,
}

/// Quick settings (the sliders button in the bar): the sound and dark mode, as they are.
#[tauri::command]
async fn qs_state() -> QuickState {
    tauri::async_runtime::spawn_blocking(|| QuickState { vol: commands::volume().unwrap_or(0.5), mute: commands::muted().unwrap_or(false), dark: skills::is_dark() })
        .await
        .unwrap_or(QuickState { vol: 0.5, mute: false, dark: true })
}

/// The screen's brightness (a laptop's; a desktop's monitor has none to give), asked for apart: it takes a moment.
#[tauri::command]
async fn qs_bright() -> Option<u32> {
    tauri::async_runtime::spawn_blocking(skills::brightness).await.ok().flatten()
}

/// Quick settings: the sound's level, mute, the screen's brightness, dark mode.
#[tauri::command]
async fn qs_set(what: String, v: f64) -> bool {
    tauri::async_runtime::spawn_blocking(move || match what.as_str() {
        "vol" => commands::set_volume(v as f32),
        "mute" => commands::set_mute(v > 0.5),
        "bright" => skills::set_brightness(v.round().clamp(0.0, 100.0) as u32),
        "dark" => skills::dark_mode(Some(v > 0.5)).is_some(),
        _ => false,
    })
    .await
    .unwrap_or(false)
}

/// The page is alive (every few seconds; and whether it's hidden, when its timers slow down on purpose).
#[tauri::command]
fn page_alive(hidden: bool) {
    watchdog::alive(hidden);
}

/// The page: nothing has been open for a while (true), or something just opened (false).
#[tauri::command]
fn page_idle(app: AppHandle, idle: bool) {
    smooth::idle(&app, idle);
}

/// A country's holidays and festivals (calendar), from Google's public holiday calendar, kept on this PC a while.
#[tauri::command]
async fn holidays(app: AppHandle, country: String) -> Vec<sys::Holiday> {
    tauri::async_runtime::spawn_blocking(move || sys::holidays(&app, &country)).await.unwrap_or_default()
}

#[tauri::command]
async fn cts_copy(x: i32, y: i32, w: i32, h: i32) -> bool {
    tauri::async_runtime::spawn_blocking(move || cts::copy(x, y, w, h)).await.unwrap_or(false)
}

/// Whether pictures of the windows in use are kept, for the wheel to show (Settings, Taskbar).
#[tauri::command]
fn dock_keep(on: bool) {
    dock::keep(on);
}

/// The wallpaper that's up, as a picture of a given size (ULTRON's search and its lock screen show it).
#[tauri::command]
async fn wall_picture(w: u32, h: u32) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || wall::picture(w, h)).await.ok().flatten()
}

/// ULTRON's own Start: whether the Windows key by itself is ULTRON's to answer.
#[tauri::command]
fn start_key(app: AppHandle, on: bool) {
    keys::set(&app, on);
}

/// Dynamic windows (Settings, Look & feel): macOS's three buttons in place of Windows' own on the window in front,
/// and rounded corners. Kept just under ULTRON's own overlay.
#[tauri::command]
fn dyn_windows(app: AppHandle, on: bool) {
    let main = app.get_webview_window("main").and_then(|w| hit::hwnd_of(&w)).map(|h| h.0 as isize).unwrap_or(0);
    lights::set(&app, on, main);
}

/// Everything the page keeps (settings, pins, places, notes, the floating lyrics' spot...) is also kept in a file of
/// ULTRON's own: the web engine's storage can lose its last changes when ULTRON is ended suddenly, and then a restart
/// would bring back old settings. Written whole, then swapped in, so it's never half written.
#[tauri::command]
fn store_save(app: AppHandle, data: String) {
    let Ok(dir) = app.path().app_data_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let tmp = dir.join("page-store.json.tmp");
    if std::fs::write(&tmp, data.as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join("page-store.json"));
    }
}

#[tauri::command]
fn store_load(app: AppHandle) -> Option<String> {
    let dir = app.path().app_data_dir().ok()?;
    std::fs::read_to_string(dir.join("page-store.json")).ok()
}

/// Ctrl+Space opens ULTRON's Start from anywhere (Settings, Taskbar).
#[tauri::command]
fn spot_key(on: bool) {
    spotkey::set(on);
}

/// The wheel's own shortcut (Settings, Taskbar): Windows' modifier bits and a key code; 0 takes it away.
#[tauri::command]
fn wheel_key(mods: u32, vk: u32) {
    spotkey::set_key(spotkey::WHEEL, mods, vk);
}

/// Cleaner: how much each kind of junk comes to (nothing is touched).
#[tauri::command]
async fn cleaner_scan(app: AppHandle) -> Vec<cleaner::Group> {
    tauri::async_runtime::spawn_blocking(move || cleaner::scan(&app)).await.unwrap_or_default()
}

/// Cleaner: clear the ticked kinds; the page hears it go ("clean-file", "clean-group"). How much was freed.
#[tauri::command]
async fn cleaner_clean(app: AppHandle, ids: Vec<String>) -> u64 {
    tauri::async_runtime::spawn_blocking(move || cleaner::clean(&app, &ids)).await.unwrap_or(0)
}

/// One email as it looks: its words and its pictures.
#[tauri::command]
async fn mail_rich(app: AppHandle, uid: u32) -> Result<Vec<mail::Block>, String> {
    tauri::async_runtime::spawn_blocking(move || mail::rich(&app, uid)).await.map_err(|e| e.to_string())?
}

/* ---------------- ULTRON's own notifications (notes.rs) ---------------- */

#[tauri::command]
fn notes_mode(app: AppHandle, on: bool) {
    notes::set(&app, on);
}

#[tauri::command]
fn notes_list() -> Vec<notes::Note> {
    notes::list()
}

#[tauri::command]
async fn notes_press(app: AppHandle, id: u32, which: Option<usize>, typed: Vec<(String, String)>) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || notes::press(&app, id, which, typed)).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn notes_dismiss(app: AppHandle, id: u32) {
    let _ = tauri::async_runtime::spawn_blocking(move || notes::dismiss(&app, id)).await;
}

#[tauri::command]
async fn notes_clear(app: AppHandle, aumid: Option<String>) {
    let _ = tauri::async_runtime::spawn_blocking(move || notes::clear(&app, aumid)).await;
}

#[tauri::command]
async fn notes_apps() -> Vec<notes::AppRow> {
    tauri::async_runtime::spawn_blocking(notes::apps).await.unwrap_or_default()
}

#[tauri::command]
fn notes_allow(aumid: String, on: bool) {
    notes::allow(&aumid, on);
}

/// ULTRON's window takes the keyboard (its search and its lock screen are typed into).
#[tauri::command]
fn focus_main(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.set_focus();
    }
}

/// The keyboard goes back to the window you were in before ULTRON took it.
#[tauri::command]
async fn focus_back(app: AppHandle) {
    let _ = tauri::async_runtime::spawn_blocking(move || app.state::<Apps>().refocus()).await;
}

#[tauri::command]
async fn dock_untile() -> usize {
    tauri::async_runtime::spawn_blocking(dock::untile).await.unwrap_or(0)
}

#[tauri::command]
async fn launch(target: String) -> bool {
    tauri::async_runtime::spawn_blocking(move || apps::shell_open(&target)).await.unwrap_or(false)
}

#[tauri::command]
fn power(action: String) {
    sys::power(&action);
}

#[tauri::command]
fn user_name() -> String {
    std::env::var("USERNAME").unwrap_or_else(|_| "You".into())
}

#[tauri::command]
async fn news(country: String) -> Vec<sys::NewsItem> {
    tauri::async_runtime::spawn_blocking(move || sys::news(&country)).await.unwrap_or_default()
}

#[tauri::command]
fn media_fast(media: State<Media>, on: bool) {
    media.set_fast(on);
}

#[tauri::command]
async fn translate(app: AppHandle, text: String, from: String, to: String, id: Option<u64>) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || ai::translate(&app, &text, &from, &to, id.unwrap_or(0))).await.ok().flatten()
}

#[tauri::command]
fn sys_stats() -> sys::Stats {
    sys::stats()
}

#[tauri::command]
fn clip_list() -> Vec<String> {
    clip::list()
}

#[tauri::command]
fn clip_action(action: String, text: Option<String>, index: Option<usize>) -> bool {
    match action.as_str() {
        "copy" => clip::set(&text.unwrap_or_default()),
        "remove" => {
            clip::remove(index.unwrap_or(usize::MAX));
            true
        }
        "clear" => {
            clip::clear();
            true
        }
        _ => false,
    }
}

#[tauri::command]
fn set_window_height(app: AppHandle, h: f64) {
    // resize through Tauri so the page's input area grows too (a raw SetWindowPos
    // only stretched the picture, and clicks below the idle height went nowhere)
    appbar::remember_height(h);
    if let Some(win) = app.get_webview_window("main") {
        if let (Ok(size), Ok(s)) = (win.outer_size(), win.scale_factor()) {
            let _ = win.set_size(tauri::PhysicalSize::new(size.width, (h * s).round() as u32));
        }
    }
}

/// async on purpose: creating a window from a plain (main-thread) command on
/// Windows deadlocks, which froze all of ULTRON the first time the browser opened
#[tauri::command]
async fn browser_show(app: AppHandle, x: f64, y: f64, w: f64, h: f64, url: Option<String>) -> Result<(), String> {
    browser::show(&app, x, y, w, h, url)
}

#[tauri::command]
fn browser_hide(app: AppHandle) {
    browser::hide(&app);
}

#[tauri::command]
fn browser_go(app: AppHandle, action: String, url: Option<String>) {
    browser::go(&app, &action, url);
}

/* ---------------- menu bar ---------------- */

/// Menu bar items act on the app you were using: give it focus back, then press its shortcut.
#[tauri::command]
async fn menu_action(app: AppHandle, action: String) {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    let _ = tauri::async_runtime::spawn_blocking(move || {
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        let open = |t: &str| {
            apps::shell_open(t);
        };
        let keys: &[VIRTUAL_KEY] = match action.as_str() {
            "new" => &[VK_CONTROL, VK_N],
            "new-window" => &[VK_CONTROL, VK_SHIFT, VK_N],
            "open" => &[VK_CONTROL, VK_O],
            "save" => &[VK_CONTROL, VK_S],
            "save-as" => &[VK_CONTROL, VK_SHIFT, VK_S],
            "print" => &[VK_CONTROL, VK_P],
            "close" => &[VK_CONTROL, VK_W],
            "undo" => &[VK_CONTROL, VK_Z],
            "redo" => &[VK_CONTROL, VK_Y],
            "cut" => &[VK_CONTROL, VK_X],
            "copy" => &[VK_CONTROL, VK_C],
            "paste" => &[VK_CONTROL, VK_V],
            "select-all" => &[VK_CONTROL, VK_A],
            "find" => &[VK_CONTROL, VK_F],
            "zoom-in" => &[VK_CONTROL, VK_OEM_PLUS],
            "zoom-out" => &[VK_CONTROL, VK_OEM_MINUS],
            "zoom-reset" => &[VK_CONTROL, VK_0],
            "fullscreen" => &[VK_F11],
            "refresh" => &[VK_F5],
            "minimize" => &[VK_LWIN, VK_DOWN],
            "maximize" => &[VK_LWIN, VK_UP],
            "snap-left" => &[VK_LWIN, VK_LEFT],
            "snap-right" => &[VK_LWIN, VK_RIGHT],
            "task-view" => &[VK_LWIN, VK_TAB],
            "quit-app" => &[VK_MENU, VK_F4],
            "desktop" => &[VK_LWIN, VK_D],
            "notifications" => &[VK_LWIN, VK_N],
            "quick-settings" => &[VK_LWIN, VK_A],
            "search" => &[VK_LWIN, VK_S],
            "home" => return open(&home),
            "downloads" => return open(&format!(r"{home}\Downloads")),
            "documents" => return open(&format!(r"{home}\Documents")),
            "desktop-folder" => return open(&format!(r"{home}\Desktop")),
            "computer" => return open("shell:MyComputerFolder"),
            "apps" => return open("shell:AppsFolder"),
            "settings" => return open("ms-settings:"),
            "task-manager" => return open("taskmgr.exe"),
            "lock" => {
                unsafe { let _ = windows::Win32::System::Shutdown::LockWorkStation(); }
                return;
            }
            _ => return,
        };
        // system-wide shortcuts don't need the app focused first
        if !matches!(action.as_str(), "desktop" | "notifications" | "quick-settings" | "search" | "task-view") {
            app.state::<Apps>().refocus();
            std::thread::sleep(Duration::from_millis(70));
        }
        apps::send_keys(keys);
    })
    .await;
}

/// Save one of ULTRON's own files so that it can't be left half-written: the new contents go into a file
/// beside it, which then takes its place in one step. (Written straight in, a file is emptied first, and
/// an ULTRON closed at that moment left it empty: that is how the saved email account came to be lost.)
pub fn keep(path: impl AsRef<std::path::Path>, data: impl AsRef<[u8]>) -> bool {
    let path = path.as_ref();
    let mut beside = path.as_os_str().to_owned();
    beside.push(".new");
    let beside = std::path::PathBuf::from(beside);
    if std::fs::write(&beside, data.as_ref()).is_ok() && std::fs::rename(&beside, path).is_ok() {
        return true;
    }
    let _ = std::fs::remove_file(&beside);
    std::fs::write(path, data.as_ref()).is_ok()
}

/// Every ULTRON window shares one WebView2 browser, so they must all start it the same way: this is what
/// tauri.conf.json gives the main window, and every window made in code (browser.rs, ghost.rs) must be given
/// it too. A window started differently is refused outright ("not in the correct state").
pub const BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,CalculateNativeWinOcclusion,MediaRouter,OptimizationHints,AutofillServerCommunication,Translate --disable-background-networking --disable-component-update --disable-domain-reliability --no-pings --autoplay-policy=no-user-gesture-required --auto-select-desktop-capture-source=creen";

/// How often the live glass refreshes (0 = paused), from what's on screen.
#[tauri::command]
fn backdrop_rate(ms: u64) {
    backdrop::set_rate(ms);
}

/// Everything ULTRON remembers about you (Settings › Memory).
#[tauri::command]
fn memory_list(app: AppHandle) -> Vec<String> {
    memory::all(&app).into_iter().map(|f| f.text).collect()
}

/// Change one thing ULTRON remembers (Settings › Privacy).
#[tauri::command]
fn memory_edit(app: AppHandle, index: usize, text: String) {
    memory::edit(&app, index, &text);
}

/// Something to remember, written in Settings › Privacy.
#[tauri::command]
fn memory_add(app: AppHandle, text: String) {
    if !text.trim().is_empty() {
        memory::add(&app, text.trim());
    }
}

/// Cleaner: apps running in the background with no window open (procs.rs).
#[tauri::command]
async fn procs_list() -> Vec<procs::Group> {
    tauri::async_runtime::spawn_blocking(procs::list).await.unwrap_or_default()
}

/// Cleaner: end the background processes of the apps chosen.
#[tauri::command]
async fn procs_end(paths: Vec<String>) -> u32 {
    tauri::async_runtime::spawn_blocking(move || procs::end(&paths)).await.unwrap_or(0)
}

/// Forget one thing (by its place in the list), or everything.
#[tauri::command]
fn memory_forget(app: AppHandle, index: Option<usize>) {
    memory::remove(&app, index);
}

/// ULTRON's window is drawn by Windows' own web engine, but nothing about it should act like a browser: F5 or Ctrl+R
/// reloaded the whole of ULTRON, Ctrl+F brought up a find bar, Ctrl+P a print dialog, Ctrl and the wheel or a pinch on
/// the touchpad zoomed everything, a two-finger swipe went "back", and the engine offered to save passwords. All off.
/// (Copy, paste, undo and the like in its text boxes stay as they are.)
fn app_like(w: &tauri::WebviewWindow) {
    use wcore62::Interface;
    use webview2_com::Microsoft::Web::WebView2::Win32::{ICoreWebView2Settings3, ICoreWebView2Settings4, ICoreWebView2Settings5, ICoreWebView2Settings6};
    let _ = w.with_webview(|pv| unsafe {
        let Ok(s) = pv.controller().CoreWebView2().and_then(|c| c.Settings()) else { return };
        let _ = s.SetIsStatusBarEnabled(false);
        let _ = s.SetIsZoomControlEnabled(false);
        if let Ok(s) = s.cast::<ICoreWebView2Settings3>() {
            let _ = s.SetAreBrowserAcceleratorKeysEnabled(false);
        }
        if let Ok(s) = s.cast::<ICoreWebView2Settings4>() {
            let _ = s.SetIsGeneralAutofillEnabled(false);
            let _ = s.SetIsPasswordAutosaveEnabled(false);
        }
        if let Ok(s) = s.cast::<ICoreWebView2Settings5>() {
            let _ = s.SetIsPinchZoomEnabled(false);
        }
        if let Ok(s) = s.cast::<ICoreWebView2Settings6>() {
            let _ = s.SetIsSwipeNavigationEnabled(false);
        }
    });
}

/// What you've selected in the app you were using (for Read aloud).
#[tauri::command]
async fn read_selection(app: AppHandle) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || agent::selection(&app)).await.ok().flatten()
}

/* ---------------- setup ---------------- */

fn main() {
    // (started by Claude Code's hooks, once connected in Settings: hand what it said to the running ULTRON, and end)
    if std::env::args().any(|a| a == "--agent") {
        coders::relay();
        return;
    }
    // (started only to bring Windows' taskbar back when the ULTRON that hid it has gone: see dock.rs)
    if watchdog::keeper_if_asked() {
        return;
    }
    if dock::guard_if_asked() {
        return;
    }
    let by_windows = std::env::args().any(|a| a == "--at-login");
    // one ULTRON at a time (two menu bars would each reserve screen space)
    unsafe {
        let _ = CreateMutexW(None, false, &HSTRING::from("Local\\ULTRON-notch-single-instance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if by_windows {
                log_line(&format!("started with Windows ({}), but ULTRON was already running", now_said()));
            }
            return;
        }
    }
    // Started by Windows as you sign in: Windows' taskbar and desktop may not be up yet, and ULTRON's bar, its
    // taskbar and its windows go round them. It waits for them (half a minute at most), then a moment more.
    if by_windows {
        use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
        let t0 = std::time::Instant::now();
        while unsafe { FindWindowW(windows::core::w!("Shell_TrayWnd"), windows::core::PCWSTR::null()) }.is_err() && t0.elapsed() < Duration::from_secs(30) {
            std::thread::sleep(Duration::from_millis(250));
        }
        std::thread::sleep(Duration::from_millis(1500));
        let _ = std::fs::create_dir_all(login_note().parent().unwrap());
        let _ = std::fs::write(login_note(), now_said());
    }
    // the language models' servers must not outlive ULTRON (kids.rs): the ones an earlier ULTRON left
    // running are stopped now, and the ones this ULTRON starts go when it goes
    kids::sweep();
    kids::watch();
    start_log();
    // (ULTRON hears you on this process's threads: above other apps, so a busy PC doesn't leave a short clip waiting
    //  ten seconds to be heard. The brains run apart from it, below normal)
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentProcess, SetPriorityClass, ABOVE_NORMAL_PRIORITY_CLASS};
        let _ = SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS);
    }
    log_line(&format!("ULTRON {} started {}, {}", env!("CARGO_PKG_VERSION"), if by_windows { "with Windows" } else { "by hand" }, now_said()));

    tauri::Builder::default()
        .manage(Hits::default())
        .manage(Apps::default())
        .manage(Media::default())
        .manage(Voice::default())
        .manage(ai::Ai::default())
        .manage(tts::Tts::default())
        .manage(songs::Songs::default())
        .invoke_handler(tauri::generate_handler![
            set_hit_rects, page_log, dev_mode, quit_app,
            voice_status, voice_config, run_command, assist, ai_status, ai_warm, app_names,
            media_state, media_control, get_lyrics,
            media_fast, translate, news, ai_stop, tts, ai_config, ai_forget, agent_confirm, song_listen, set_autostart, get_autostart, sys_stats, clip_list, clip_action, browser_show, browser_hide, browser_go, set_window_height,
            menu_action, wallpaper, wifi_status, wifi_connect, wifi_disconnect, start_apps, app_icons, launch, power, user_name,
            intro_say, intro_heard, intro_finish, intro_play, intro_due, ai_wake, routines_list, routine_set, call_answer, call_decline, call_reply, pick_start, pick_stop, cloud_status, cloud_connect, cloud_set, cloud_forget, mail_status, mail_connect, mail_list, mail_summary, mail_digest, mail_signout, mail_body, yt_captions, media_icon, bar_reserve, backdrop_now, rec_begin, rec_data, rec_end, rec_keys, rec_sound, rec_click, rec_folder, squeeze_pick, squeeze_go, squeeze_stop, squeeze_folder, squeeze_show, today_weather, wall_list, wall_apply, wall_pick, wall_folder, dock_set, dock_snap, dock_list, dock_act, dock_recent, dock_tile, dock_untile, dock_tile_apps, dock_peek, dock_keep, at_login, notes_reply, wifi_join, dictate_key, dev_show, shelf_list, shelf_add, shelf_remove, shelf_clear, shelf_thumb, shelf_drag, shelf_reveal, shelf_pick, shelf_add_app, cursor_blur, tts_voice, cts_grab, cts_forget, cts_read, cts_ask, cts_lens, cts_copy, wall_picture, start_key, spot_key, dyn_windows, store_save, store_load, wheel_key, cleaner_scan, cleaner_clean, mail_rich, page_idle, page_alive, agent_watch, agent_hooks_state, agent_hooks, signin_wake, login_last, mail_image, web_warm, tts_lang, sounds_set, dialogs_set, osd_set, alttab_set, alttab_list, connect_list, connect_set, connect_forget, teach_start, teach_stop, teach_list, teach_play, teach_forget, dialog_shown, dialog_press, dialog_pass, link_find, link_connect, link_pair, link_command, link_answer, link_state, link_close, dictate_shape, dictate_ai, dictate_erase, words_drag, words_copy, words_type, qs_state, qs_bright, qs_set, memory_edit, memory_add, procs_list, procs_end, holidays, notes_mode, notes_list, notes_press, notes_dismiss, notes_clear, notes_apps, notes_allow, focus_main, focus_back, ai_specs, read_selection, memory_list, memory_forget, backdrop_rate,
        ])
        .on_window_event(|_win, e| {
            // the notch and menu bar never close on their own (Alt+F4 included); quit from the tray
            if let WindowEvent::CloseRequested { api, .. } = e {
                api.prevent_close();
            }
        })
        .setup(|app| {
            let handle = app.handle().clone();

            let win = app.get_webview_window("main").unwrap();
            let _ = win.set_ignore_cursor_events(true);
            if let Some(h) = hit::hwnd_of(&win) {
                appbar::register(&handle, h, BAR_HEIGHT, WINDOW_HEIGHT);
            }
            win.show()?;
            app_like(&win);

            hit::watch(handle.clone(), &["main"]);
            apps::start(handle.clone());
            media::start(handle.clone());
            voice::start(handle.clone());
            sys::watch_wallpaper(handle.clone());
            ai::start(handle.clone());
            ai::seed_history(&handle);
            tts::watch(handle.clone());
            see::watch();
            clip::watch(handle.clone());
            uam::start();
            quick::start(handle.clone());
            backdrop::start(handle.clone());
            smooth::start();
            dev_inbox(handle.clone());
            routines::start(handle.clone());
            calls::start(handle.clone());
            coders::start(handle.clone());
            lid::start(handle.clone());
            desks::start(handle.clone());
            keepbelow::start();
            watchdog::start(handle.clone());
            watchdog::keep();
            minfx::heal(); // (Windows' own minimize animation back, if a crash left it off: the page asks again if it's wanted)
            mail::watch(handle.clone());
            rec::hotkeys(handle.clone());
            spotkey::start(handle.clone());

            let quit_item = MenuItem::with_id(app, "quit", "Quit ULTRON", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&quit_item])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("ULTRON")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, e| {
                    if e.id.as_ref() == "quit" {
                        quit(app);
                    }
                })
                .on_tray_icon_event(|tray, e| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                        let _ = tray.app_handle().emit_to("main", "notch-open", ());
                    }
                })
                .build(app)?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building ULTRON")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                ai::shutdown(app);
                see::shutdown();
                quick::shutdown();
                appbar::remove(); // always give the screen space back
                dock::leave(app); // (and Windows its taskbar)
                notes::leave(app); // (and Windows its banners)
                dialogs::quit(); // (and any message box shown as glass, its own look)
            }
        });
}
