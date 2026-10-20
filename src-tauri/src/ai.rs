//! ULTRON's brain: a local language model served by llama.cpp's `llama-server` on
//! this PC. Two brains: "Smart" (Qwen3-4B-Instruct, much better at writing, facts
//! and planning) and "Fast" (Qwen2.5-1.5B, bundled). The graphics card is used when
//! it's actually faster on this PC (measured once); otherwise the CPU. No cloud, no key:
//! unless you add a key of your own (cloud.rs), in which case answers and translations come from
//! that service, and from here again whenever it can't be reached.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::commands::{self, Reply};
use crate::media::Media;

const FAST_FILE: &str = "qwen2.5-1.5b-instruct-q4_k_m.gguf";
const FAST_URL: &str = "https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF/resolve/main/qwen2.5-1.5b-instruct-q4_k_m.gguf";
// which smart brain (and which eyes) a PC gets is decided in router.rs
const NO_WINDOW: u32 = 0x0800_0000;
/// the brain works at slightly lower priority, so the notch's animations never stutter while it thinks
const BELOW_NORMAL: u32 = 0x0000_4000;
/// the brain file that's loaded now
static LOADED: Mutex<&'static str> = Mutex::new("");
/// unload the model after this long without questions (unless "keep ready" is on and there's memory to spare)
const IDLE_UNLOAD: Duration = Duration::from_secs(8 * 60);

/// Memory that's free right now, in GB.
pub fn free_ram_gb() -> f64 {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    if unsafe { GlobalMemoryStatusEx(&mut m) }.is_ok() { m.ullAvailPhys as f64 / (1u64 << 30) as f64 } else { 8.0 }
}

/// Room left before Windows can't hand out any more memory at all (RAM and page file together), in GB.
/// Free RAM alone doesn't show this: with the page file full, a PC with 4 GB "free" still crawls.
pub fn free_commit_gb() -> f64 {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    if unsafe { GlobalMemoryStatusEx(&mut m) }.is_ok() { m.ullAvailPageFile as f64 / (1u64 << 30) as f64 } else { 8.0 }
}

/// Should the big brain sit in memory while nobody's using it? Only when "keep ready" is on AND it isn't
/// squeezing the PC: with a quick brain answering everyday questions, a 16 GB PC that's nearly full is
/// better off getting those gigabytes back (it reloads in a few seconds when a thinking question comes).
fn keep_loaded(app: &AppHandle) -> bool {
    config(app).keep && (crate::quick::port().is_none() || (free_ram_gb() > 3.0 && free_commit_gb() > 4.0))
}

/// The red stop button: stop talking, thinking and doing. Each press ends the requests that are running
/// at that moment (and only those: a question asked a second later is a new request, untouched).
///   - A request notes the count of presses when it starts (`begin_request`); `cancelled()` is true
///     for it once the count has moved on.
///   - Stop also closes that request's connection to the brain, so a brain that's still READING (which
///     on a processor can take many seconds, with nothing coming back to notice the flag by) lets go at once.
static STOPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
thread_local! { static MY_STOP: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
/// open connections to the brains: (its id, the request it belongs to or 0, the connection)
static LIVE: Mutex<Vec<(u64, u64, std::net::TcpStream)>> = Mutex::new(Vec::new());
static LIVE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn cancel() {
    let was = STOPS.fetch_add(1, Ordering::SeqCst);
    for (_, req, s) in LIVE.lock().unwrap().iter() {
        if *req != 0 && *req <= was {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// A request from the user starts on this thread; it ends when the returned value is dropped.
pub struct Request;
pub fn begin_request() -> Request {
    MY_STOP.with(|m| m.set(STOPS.load(Ordering::SeqCst)));
    Request
}
impl Drop for Request {
    fn drop(&mut self) {
        MY_STOP.with(|m| m.set(0)); // (these threads are reused)
    }
}

pub fn cancelled() -> bool {
    let mine = MY_STOP.with(|m| m.get());
    mine != 0 && STOPS.load(Ordering::SeqCst) != mine
}

/// A streaming request to one of the brains on a connection that Stop can close. `on` gets each event
/// as it arrives and says whether to keep going. Returns false if the brain couldn't be reached or refused.
fn stream_post(port: u16, path: &str, body: &serde_json::Value, patience: Duration, mut on: impl FnMut(&serde_json::Value) -> bool) -> bool {
    use std::net::{SocketAddr, TcpStream};
    let Ok(mut sock) = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(3)) else { return false };
    let _ = sock.set_nodelay(true);
    let _ = sock.set_read_timeout(Some(patience)); // nothing at all for this long: the brain has hung
    let payload = body.to_string();
    let head = format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n", payload.len());
    if sock.write_all(head.as_bytes()).is_err() || sock.write_all(payload.as_bytes()).is_err() {
        return false;
    }
    // Stop can find this connection while it's open
    struct Open(u64);
    impl Drop for Open {
        fn drop(&mut self) {
            LIVE.lock().unwrap().retain(|(id, ..)| *id != self.0);
        }
    }
    let id = LIVE_ID.fetch_add(1, Ordering::SeqCst);
    if let Ok(twin) = sock.try_clone() {
        LIVE.lock().unwrap().push((id, MY_STOP.with(|m| m.get()), twin));
    }
    let _open = Open(id);
    let mut r = BufReader::new(sock);
    let mut line = String::new();
    if r.read_line(&mut line).unwrap_or(0) == 0 || !line.contains(" 200") {
        return false;
    }
    let mut chunked = false;
    loop {
        line.clear();
        if r.read_line(&mut line).unwrap_or(0) == 0 {
            return false;
        }
        let l = line.trim().to_ascii_lowercase();
        if l.is_empty() {
            break;
        }
        if l.starts_with("transfer-encoding:") && l.contains("chunked") {
            chunked = true;
        }
    }
    // events are lines that start with "data: "; a piece of the reply can end in the middle of one
    let mut pending: Vec<u8> = Vec::new();
    let mut feed = |bytes: &[u8]| -> bool {
        pending.extend_from_slice(bytes);
        while let Some(i) = pending.iter().position(|b| *b == b'\n') {
            let l: Vec<u8> = pending.drain(..=i).collect();
            if let Some(v) = String::from_utf8_lossy(&l).trim_end().strip_prefix("data: ").and_then(|d| serde_json::from_str::<serde_json::Value>(d).ok()) {
                if !on(&v) {
                    return false;
                }
            }
        }
        true
    };
    if chunked {
        loop {
            line.clear();
            if r.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let size = line.trim().split(';').next().unwrap_or("").trim();
            if size.is_empty() {
                continue;
            }
            let n = usize::from_str_radix(size, 16).unwrap_or(0);
            if n == 0 {
                break;
            }
            let mut buf = vec![0u8; n];
            if r.read_exact(&mut buf).is_err() {
                break;
            }
            let mut crlf = [0u8; 2];
            let _ = r.read_exact(&mut crlf);
            if !feed(&buf) {
                break;
            }
        }
    } else {
        let mut buf = [0u8; 4096];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if !feed(&buf[..n]) {
                        break;
                    }
                }
            }
        }
    }
    true
}

/// Have a brain read this text into its memory (nothing is written back). Stop interrupts it.
fn read_in(port: u16, prompt: &str) -> bool {
    stream_post(port, "/completion", &serde_json::json!({ "prompt": prompt, "n_predict": 0, "cache_prompt": true, "stream": true }), Duration::from_secs(240), |v| {
        !cancelled() && !v.get("stop").and_then(|s| s.as_bool()).unwrap_or(false)
    }) && !cancelled()
}

#[derive(Serialize, Clone, Default)]
pub struct AiStatus {
    pub state: String, // downloading | ready | loading | error
    pub progress: f64,
    pub detail: String,
    pub model: String,
    pub device: String,
    pub tps: f64,
}

/// What the Settings window controls.
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    pub brain: String, // smart | fast
    pub gpu: String,   // auto | off
    pub keep: bool,    // keep the brain loaded for instant answers
    pub web: bool,     // look facts up online
    pub length: String, // short | normal | detailed
    pub ghost: bool,   // show ULTRON's cursor during tasks
    pub type_speed: String, // natural | fast | instant
}

impl Default for Config {
    fn default() -> Self {
        Config { brain: "smart".into(), gpu: "auto".into(), keep: true, web: true, length: "normal".into(), ghost: true, type_speed: "natural".into() }
    }
}

struct Server {
    child: Child,
    port: u16,
    brain: String,
}

pub struct Ai {
    server: Mutex<Option<Server>>,
    ready: AtomicBool,
    status: Mutex<AiStatus>,
    history: Mutex<Vec<(String, String)>>,
    last_used: Mutex<Instant>,
    load_lock: Mutex<()>,
    talk: Mutex<()>, // one answer at a time
    pub front_app: Mutex<String>,
    pub cfg: Mutex<Config>,
}

impl Default for Ai {
    fn default() -> Self {
        Ai {
            server: Mutex::new(None),
            ready: AtomicBool::new(false),
            status: Mutex::new(AiStatus::default()),
            history: Mutex::new(Vec::new()),
            last_used: Mutex::new(Instant::now()),
            load_lock: Mutex::new(()),
            talk: Mutex::new(()),
            front_app: Mutex::new(String::new()),
            cfg: Mutex::new(Config::default()),
        }
    }
}

impl Ai {
    pub fn status(&self) -> AiStatus {
        self.status.lock().unwrap().clone()
    }
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }
    /// The running conversation the brain sees with every question (for follow-ups like "and how tall
    /// is he?"): the last three exchanges. It no longer fades after a pause, and chats.rs keeps the
    /// whole history on disk, so it survives restarts and older talks can be recalled.
    pub fn remember(&self, user: &str, assistant: &str) {
        let mut h = self.history.lock().unwrap();
        h.push(("user".into(), user.into()));
        h.push(("assistant".into(), assistant.into()));
        let n = h.len();
        if n > 6 {
            h.drain(..n - 6);
        }
    }
    pub fn forget(&self) {
        self.history.lock().unwrap().clear();
    }
}

/// Keep an exchange: in the running conversation, and in the history on disk.
pub fn remember_chat(app: &AppHandle, user: &str, reply: &str) {
    app.state::<Ai>().remember(user, reply);
    crate::chats::add(app, user, reply);
}

/// On start-up: pick the conversation up where it was left.
pub fn seed_history(app: &AppHandle) {
    let recent = crate::chats::recent(app, 3);
    if !recent.is_empty() {
        crate::log_line(&format!("remembering {} past exchanges ({} recent ones in mind)", crate::chats::count(app), recent.len() / 2));
        *app.state::<Ai>().history.lock().unwrap() = recent;
    }
}

pub fn config(app: &AppHandle) -> Config {
    app.state::<Ai>().cfg.lock().unwrap().clone()
}

fn set_status(app: &AppHandle, state: &str, progress: f64, detail: &str) {
    let ai = app.state::<Ai>();
    let st = {
        let mut s = ai.status.lock().unwrap();
        s.state = state.into();
        s.progress = progress;
        s.detail = detail.into();
        s.clone()
    };
    let _ = app.emit_to("main", "ai-status", st);
}

fn set_info(app: &AppHandle, model: &str, device: &str, tps: Option<f64>) {
    let ai = app.state::<Ai>();
    let st = {
        let mut s = ai.status.lock().unwrap();
        s.model = model.into();
        s.device = device.into();
        if let Some(t) = tps {
            s.tps = t;
        }
        s.clone()
    };
    let _ = app.emit_to("main", "ai-status", st);
}

/* ---------------- files ---------------- */

fn data_dir(app: &AppHandle) -> PathBuf {
    let d = app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir());
    let _ = std::fs::create_dir_all(d.join("models"));
    d
}

fn find_res(app: &AppHandle, rel: &[&str]) -> Option<PathBuf> {
    let mut bases = Vec::new();
    if let Ok(r) = app.path().resource_dir() {
        bases.push(r.join("resources"));
        bases.push(r);
    }
    bases.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources"));
    bases.into_iter().map(|b| rel.iter().fold(b, |p, s| p.join(s))).find(|p| p.exists())
}

fn model_path(app: &AppHandle, file: &str) -> Option<PathBuf> {
    find_res(app, &["models", file]).or_else(|| Some(data_dir(app).join("models").join(file)).filter(|p| p.exists()))
}

fn server_exe(app: &AppHandle) -> Option<PathBuf> {
    find_res(app, &["llama", "llama-server.exe"])
}

fn download(app: &AppHandle, url: &str, path: &PathBuf, label: &str, min: f64) -> Result<(), String> {
    let part = path.with_extension("part");
    let resp = ureq::get(url).timeout(Duration::from_secs(3 * 60 * 60)).call().map_err(|e| e.to_string())?;
    let total: f64 = resp.header("Content-Length").and_then(|v| v.parse().ok()).unwrap_or(min * 1.1);
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(&part).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 1 << 17];
    let (mut done, mut last) = (0f64, -1i64);
    loop {
        let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done += n as f64;
        let pct = (done / total * 100.0).floor() as i64;
        if pct != last {
            last = pct;
            let ai = app.state::<Ai>();
            if !ai.is_ready() {
                set_status(app, "downloading", done / total, label);
            } else {
                let _ = app.emit_to("main", "ai-upgrade", done / total);
            }
        }
    }
    drop(file);
    if done < min {
        let _ = std::fs::remove_file(&part);
        return Err("download incomplete".into());
    }
    std::fs::rename(&part, path).map_err(|e| e.to_string())
}

/* ---------------- the server ---------------- */

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map(|a| a.port()).unwrap_or(18321)
}

fn threads() -> usize {
    // all but one physical core: one stays free for the screen
    (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / 2).saturating_sub(1).clamp(2, 8)
}

/// What the big brain's slot holds right now: "agent" (the task agent's instructions), "chat", or nothing.
static SLOT_HOLDS: Mutex<&'static str> = Mutex::new("");
/// which quick-brain server, and what its memory starts with ("chat": ULTRON's instructions and examples)
static QUICK_HOLDS: Mutex<(u16, &'static str)> = Mutex::new((0, ""));
/// the words of the chat opening the quick brain last had ready (their fingerprint)
static QUICK_OPENING: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// when the big brain's idle memory was last handed back to Windows
static LAST_TRIM: Mutex<Option<Instant>> = Mutex::new(None);

pub(crate) fn slot_dir() -> PathBuf {
    let d = std::env::temp_dir().join("ultron-slots");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn spawn(exe: &PathBuf, model: &PathBuf, gpu: bool, port: u16) -> Option<Child> {
    *SLOT_HOLDS.lock().unwrap() = "";
    let mut c = Command::new(exe);
    c.current_dir(exe.parent()?)
        .arg("-m")
        .arg(model)
        // one slot; the task agent's long instructions are saved to disk once and restored in a fraction
        // of a second when a task starts (agent_ready), instead of holding a second context in memory
        .args(["--host", "127.0.0.1", "--port", &port.to_string(), "-c", "6144", "-np", "1"])
        .arg("--slot-save-path").arg(slot_dir())
        // reading and writing both stay on the physical cores: using every thread made the interface hitch while it read
        .args(["-t", &threads().to_string(), "-tb", &(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / 2).max(2).to_string(), "--poll", "0"])
        .args(["--log-disable", "--no-webui", "--cache-ram", "0", "-ctxcp", "4"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(NO_WINDOW | BELOW_NORMAL);
    if !gpu {
        c.args(["-dev", "none"]);
        // (Its working memory, the context, stays at full size. Keeping it at 8 bits saved a few hundred MB
        // but made reading 2 to 3 times slower once a conversation got long: a tool's 1,100-token result
        // took 67 s instead of 30 s, measured on this PC. Writing speed was the same either way.)
        c.args(["-fa", "on"]);
        // (it reads in bites of 256 words: between bites it can drop a request that was stopped, so the next
        // question isn't kept waiting behind a long read nobody wants any more)
        c.args(["-b", "256", "-ub", "256"]);
    }
    // with a GPU, --fit (on by default) puts as much of the model on it as there's free memory for
    c.spawn().ok()
}

fn wait_healthy(port: u16, child: &mut Child) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(90) {
        if child.try_wait().ok().flatten().is_some() {
            return false;
        }
        if ureq::get(&format!("http://127.0.0.1:{port}/health")).timeout(Duration::from_millis(800)).call().is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    false
}

/// Generation speed of a running server (tokens per second) on a short prompt.
fn bench(port: u16) -> f64 {
    let body = serde_json::json!({ "prompt": render(port, &[("user".into(), "Count from one to forty in words.".into())]), "n_predict": 24, "cache_prompt": false });
    ureq::post(&format!("http://127.0.0.1:{port}/completion"))
        .timeout(Duration::from_secs(45))
        .send_json(body)
        .ok()
        .and_then(|r| r.into_json::<serde_json::Value>().ok())
        .and_then(|v| v.get("timings")?.get("predicted_per_second")?.as_f64())
        .unwrap_or(0.0)
}

fn gpu_name(exe: &PathBuf) -> Option<String> {
    let out = Command::new(exe).arg("--list-devices").creation_flags(NO_WINDOW).stdin(Stdio::null()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    text.lines().find(|l| l.trim_start().starts_with("Vulkan0:")).map(|l| {
        let n = l.split_once(':').map(|x| x.1).unwrap_or(l).trim();
        n.split(" (").next().unwrap_or(n).to_string()
    })
}

/// GPU or CPU for this brain on this PC: measured once and remembered.
fn choose_gpu(app: &AppHandle, exe: &PathBuf, model: &PathBuf, file: &str) -> (bool, String) {
    let cfg = config(app);
    if cfg.gpu == "off" {
        return (false, "CPU".into());
    }
    let Some(gpu) = gpu_name(exe) else { return (false, "CPU".into()) };
    let gb = std::fs::metadata(model).map(|m| m.len() as f64 / (1u64 << 30) as f64).unwrap_or(0.0);
    if gb > crate::router::specs().vram_gb * 0.55 {
        return (false, "CPU".into()); // the card (which also drives the screen) can't hold it: half on it is slower than none
    }
    let memo = data_dir(app).join("brain-device.json");
    let mut known: serde_json::Map<String, serde_json::Value> =
        std::fs::read(&memo).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let key = format!("{file}|{gpu}");
    if let Some(v) = known.get(&key).and_then(|v| v.as_bool()) {
        return (v, if v { gpu } else { "CPU".into() });
    }
    set_status(app, "loading", 0.0, "Checking whether your graphics card is faster");
    let speed = |use_gpu: bool| -> f64 {
        let port = free_port();
        let Some(mut c) = spawn(exe, model, use_gpu, port) else { return 0.0 };
        let s = if wait_healthy(port, &mut c) { bench(port) } else { 0.0 };
        let _ = c.kill();
        let _ = c.wait();
        s
    };
    let g = speed(true);
    let c = speed(false);
    crate::log_line(&format!("brain speed {file}: gpu {g:.1} t/s, cpu {c:.1} t/s"));
    let use_gpu = g > c * 1.15;
    known.insert(key, serde_json::Value::Bool(use_gpu));
    let _ = std::fs::write(&memo, serde_json::to_vec(&known).unwrap_or_default());
    (use_gpu, if use_gpu { gpu } else { "CPU".into() })
}

/// The brain the settings ask for, if its file is here (else the bundled fast one). The smart
/// brain ships next to the installer as ULTRON-Brain.gguf; older installs may still have Qwen3-4B.
fn pick_brain(app: &AppHandle) -> Option<(String, PathBuf, &'static str)> {
    let cfg = config(app);
    if cfg.brain == "smart" {
        for f in crate::router::text_order() {
            if let Some(p) = model_path(app, f) {
                return Some(("smart".into(), p, f));
            }
        }
    }
    model_path(app, FAST_FILE).map(|p| ("fast".into(), p, FAST_FILE))
}

/// At launch: make sure a brain is available; fetch the smart one in the background if wanted.
pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        if model_path(&app, FAST_FILE).is_none() {
            let target = data_dir(&app).join("models").join(FAST_FILE);
            set_status(&app, "downloading", 0.0, "");
            if let Err(e) = download(&app, FAST_URL, &target, "", 500_000_000.0) {
                return set_status(&app, "error", 0.0, &format!("Couldn't get the AI model: {e}"));
            }
        }
        app.state::<Ai>().ready.store(true, Ordering::SeqCst);
        set_status(&app, "ready", 1.0, "");
        // the smart brain is too big for the installer: fetch it quietly on PCs that can run it well
        let upgrade = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(20));
            let t = crate::router::text_model();
            crate::log_line(&format!("brains for this PC ({:?}): {} + {}", crate::router::tier(), t.name, crate::router::vision_model().name));
            if config(&upgrade).brain == "smart" && model_path(&upgrade, t.file).is_none() {
                let target = data_dir(&upgrade).join("models").join(t.file);
                if download(&upgrade, t.url, &target, "Upgrading the brain", t.min).is_ok() {
                    let _ = upgrade.emit_to("main", "ai-upgrade", 1.0);
                    restart(&upgrade);
                }
            }
            // and the eyes
            let v = crate::router::vision_model();
            if config(&upgrade).brain == "smart" && !crate::see::available(&upgrade) {
                let dir = data_dir(&upgrade).join("models");
                let ok = model_path(&upgrade, v.file).is_some() || download(&upgrade, v.url, &dir.join(v.file), "Getting ULTRON's eyes", v.min).is_ok();
                if let (true, Some((proj, url))) = (ok, v.mmproj) {
                    if model_path(&upgrade, proj).is_none() {
                        let _ = download(&upgrade, url, &dir.join(proj), "Getting ULTRON's eyes", 500e6);
                    }
                }
            }
        });
        // ready before you ask
        std::thread::sleep(Duration::from_secs(4));
        // (with a graphics card the quick brain covers everyday questions: the big one waits until it's needed, or there's room)
        if config(&app).keep && (crate::router::specs().vram_gb < 3.5 || (free_ram_gb() > 6.0 && free_commit_gb() > 6.0)) {
            ensure_loaded(&app);
        }
        loop {
            std::thread::sleep(Duration::from_secs(30));
            let ai = app.state::<Ai>();
            let idle = ai.last_used.lock().unwrap().elapsed() > IDLE_UNLOAD;
            // a minute after it last worked, its idle memory goes back to Windows (it stays loaded and ready)
            let used = *ai.last_used.lock().unwrap();
            // (never in the middle of an answer: a long agent step runs for over a minute after "last used",
            // and taking its memory away mid-step made it crawl)
            let free = ai.talk.try_lock();
            if free.is_ok() && used.elapsed() > Duration::from_secs(45) && LAST_TRIM.lock().unwrap().map(|t| t < used).unwrap_or(true) {
                if let Some(pid) = ai.server.try_lock().ok().and_then(|g| g.as_ref().map(|s| s.child.id())) {
                    crate::smooth::trim(pid);
                    *LAST_TRIM.lock().unwrap() = Some(Instant::now());
                }
            }
            if free.is_ok() && idle && !keep_loaded(&app) {
                if let Ok(mut g) = ai.server.try_lock() {
                    if let Some(mut s) = g.take() {
                        let _ = s.child.kill();
                    }
                }
            }
        }
    });
}

/// Stop the running brain so the next question starts the one the settings ask for.
pub fn restart(app: &AppHandle) {
    let ai = app.state::<Ai>();
    if let Some(mut s) = ai.server.lock().unwrap().take() {
        let _ = s.child.kill();
    }
    if config(app).keep {
        warm(app);
    }
}

pub fn shutdown(app: &AppHandle) {
    if let Some(mut s) = app.state::<Ai>().server.lock().unwrap().take() {
        let _ = s.child.kill();
    }
}

/// Start the brain if it isn't running (a few seconds), and read the instructions ahead of time.
fn ensure_loaded(app: &AppHandle) -> bool {
    let ai = app.state::<Ai>();
    let _one_at_a_time = ai.load_lock.lock().unwrap();
    {
        let mut g = ai.server.lock().unwrap();
        if let Some(s) = g.as_mut() {
            if s.child.try_wait().ok().flatten().is_none() {
                return true;
            }
            g.take(); // it died: start a new one
        }
    }
    let Some(exe) = server_exe(app) else {
        set_status(app, "error", 0.0, "The AI engine is missing");
        return false;
    };
    let Some((brain, model, file)) = pick_brain(app) else { return false };
    *LOADED.lock().unwrap() = file;
    set_status(app, "loading", 0.0, "");
    let (gpu, device) = choose_gpu(app, &exe, &model, file);
    let port = free_port();
    let Some(mut child) = spawn(&exe, &model, gpu, port) else {
        set_status(app, "error", 0.0, "Couldn't start the AI");
        return false;
    };
    if !wait_healthy(port, &mut child) {
        let _ = child.kill();
        if gpu {
            // the graphics card couldn't take it: use the CPU
            let port = free_port();
            if let Some(mut c) = spawn(&exe, &model, false, port) {
                if wait_healthy(port, &mut c) {
                    *ai.server.lock().unwrap() = Some(Server { child: c, port, brain: brain.clone() });
                    finish_load(app, &brain, "CPU");
                    return true;
                }
            }
        }
        set_status(app, "error", 0.0, "The AI model failed to load");
        return false;
    }
    *ai.server.lock().unwrap() = Some(Server { child, port, brain: brain.clone() });
    finish_load(app, &brain, &device);
    true
}

fn finish_load(app: &AppHandle, brain: &str, device: &str) {
    let _ = brain;
    let name = crate::router::name_of(&LOADED.lock().unwrap());
    set_info(app, name, device, None);
    set_status(app, "ready", 1.0, "");
    crate::log_line(&format!("brain ready: {name} on {device}"));
    // read the instructions and the conversation now, so a real question only adds a few words
    prefill_now(app);
    // the model file stays mapped in memory after the server has made its own working copy: give that back
    if let Some(pid) = app.state::<Ai>().server.lock().unwrap().as_ref().map(|s| s.child.id()) {
        crate::smooth::trim(pid);
        *LAST_TRIM.lock().unwrap() = Some(Instant::now());
    }
}

/// Called when you open the notch, start typing or start talking: have the brain ready by the time you ask.
pub fn warm(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let ai = app.state::<Ai>();
        if ai.is_ready() {
            // opening the notch gets the big brain ready, unless the quick one is there and memory is tight
            let loaded = ai.server.lock().unwrap().is_some();
            if loaded || crate::quick::port().is_none() || free_ram_gb() > 4.5 {
                *ai.last_used.lock().unwrap() = Instant::now();
                ensure_loaded(&app);
                crate::doer::prefill(&app); // so a task can start planning at once
            }
        }
    });
}

/// Apply new settings from the Settings window.
pub fn set_config(app: &AppHandle, c: Config) {
    let ai = app.state::<Ai>();
    let old = std::mem::replace(&mut *ai.cfg.lock().unwrap(), c.clone());
    let running = ai.server.lock().unwrap().as_ref().map(|s| s.brain.clone());
    let brain_changed = running.map(|b| b != c.brain && (c.brain == "fast" || crate::router::text_order().iter().any(|f| model_path(app, f).is_some()))).unwrap_or(false);
    if old.gpu != c.gpu || brain_changed {
        restart(app);
    } else if c.keep && !old.keep {
        warm(app);
    }
    let t = crate::router::text_model();
    if c.brain == "smart" && old.brain != "smart" && model_path(app, t.file).is_none() {
        let app = app.clone();
        std::thread::spawn(move || {
            let target = data_dir(&app).join("models").join(t.file);
            if download(&app, t.url, &target, "Upgrading the brain", t.min).is_ok() {
                let _ = app.emit_to("main", "ai-upgrade", 1.0);
                restart(&app);
            }
        });
    }
}

/* ---------------- talking to it ---------------- */

fn system_prompt(len: &str) -> String {
    let size = match len {
        "short" => "one short sentence, under 15 words",
        "detailed" => "a few sentences, up to about 6, with real detail",
        _ => "one or two short sentences, under 30 words in all (usually just one)",
    };
    format!("You are ULTRON, a sharp, warm, slightly witty assistant that lives in the menu bar of the user's Windows PC. \
You run on this computer and your replies are spoken aloud, so talk the way a relaxed, clever person talks out loud: contractions, natural rhythm, \
{size}, plain spoken English, no lists, no headings, no emojis, no symbols you wouldn't say (a key word or two may be in **bold**: it's shown, not spoken). \
Answer questions directly, correctly and helpfully. Be accurate: if you don't know or aren't sure, say so in a few words instead of guessing, \
and never invent facts, numbers, names, links or anything about the user's PC. Never encourage violence, hatred or harm toward any person or group; if asked to, decline in one short, calm sentence. The last system message gives the current time, date, front app and music; \
use it when asked about those. Sometimes a system message with fresh web results comes right before the question: they are newer than what you \
know, so answer from them (say when it happened if that matters) and don't add facts that aren't in them. \
When the user wants something done on the PC, say a few words about it and end your reply with one or more lines (in order, up to 6) that each start with \">> \" \
followed by exactly one command from this list: \
open <app, website or folder>; close <app>; switch to <app>; search for <query>; search youtube for <query>; play <song> on spotify; play <video> on youtube; \
pause; play; next song; previous song; set volume to <0-100>; volume up; volume down; mute; unmute; \
type <exact text>; type <exact text> in the <field name> box; write <what to write, e.g. 100 words about the USA>; press <keys, e.g. ctrl s>; click <button or text on screen>; save as <file name>; \
find <thing> on <amazon, flipkart, youtube, google, maps, wikipedia, github, reddit, netflix>; make a presentation about <topic>; \
message <person> on <whatsapp, telegram or discord> saying <message>; email <person or address> about <subject> saying <message>; \
rewrite the selected text to <how, e.g. sound more formal>; summarize the screen; answer about the screen: <question>; \
set a timer for <duration>; remind me in <duration> to <thing>; snap window left; snap window right; close this window; \
set brightness to <0-100>; dark mode on; dark mode off; open <wifi, bluetooth, display, sound, apps or update> settings; \
find file <name>; open latest download; new folder <name> on <desktop, documents or downloads>; \
wait <seconds>; take a screenshot; show desktop; lock the pc. Only use type or write when the user asks you to type or write something. \
Never invent other commands, and never write a \">>\" line for small talk, questions, or when nothing needs doing.")
}

fn context_line(app: &AppHandle) -> String {
    let (time, date) = commands::now_strings();
    let front = app.state::<Ai>().front_app.lock().unwrap().clone();
    let media = app.state::<Media>();
    let playing = {
        let s = media.state.lock().unwrap();
        s.sessions
            .iter()
            .find(|x| Some(&x.id) == s.current.as_ref() && x.playing)
            .map(|x| format!(" Now playing: {} by {} ({}).", x.title, x.artist, x.app))
            .unwrap_or_default()
    };
    let front = if front.is_empty() { String::new() } else { format!(" The app in front is {front}.") };
    let who = crate::intro::user_name(app).map(|n| format!(" The user's name is {n}.")).unwrap_or_default();
    let mem = crate::memory::context(app);
    format!("It is {time} on {date}.{who}{front}{playing}{mem}")
}

/// Who the user is and what ULTRON has been told to remember: the same from one question to the next, so in
/// conversation it sits with the instructions (the part a brain keeps ready) instead of being read every time.
fn about_user(app: &AppHandle) -> String {
    let who = crate::intro::user_name(app).map(|n| format!(" The user's name is {n}.")).unwrap_or_default();
    let mem = crate::memory::context(app);
    if who.is_empty() && mem.is_empty() { String::new() } else { format!("\n\nAbout the user:{who}{mem}") }
}

/// What changes by the minute: the time, the app in front, the music.
fn moment_line(app: &AppHandle) -> String {
    let (time, date) = commands::now_strings();
    let front = app.state::<Ai>().front_app.lock().unwrap().clone();
    let media = app.state::<Media>();
    let playing = {
        let s = media.state.lock().unwrap();
        s.sessions
            .iter()
            .find(|x| Some(&x.id) == s.current.as_ref() && x.playing)
            .map(|x| format!(" Now playing: {} by {} ({}).", x.title, x.artist, x.app))
            .unwrap_or_default()
    };
    let front = if front.is_empty() { String::new() } else { format!(" The app in front is {front}.") };
    format!("It is {time} on {date}.{front}{playing}")
}

/// What was looked up, cut to its first few sentences: on a processor every word the brain reads first is
/// time before the answer starts, and a two-sentence reply doesn't need two whole articles.
/// (Something of every source stays: the newest three headlines, and the opening sentences of an article.)
fn clip(w: &str, n: usize) -> String {
    w.split("\n\n")
        .map(|part| {
            if part.starts_with("Latest headlines:") {
                return part.lines().take(4).collect::<Vec<_>>().join("\n");
            }
            if part.chars().count() <= n {
                return part.to_string();
            }
            let cut: String = part.chars().take(n).collect();
            match cut.rfind(['.', '!', '?']) {
                Some(i) if i > n / 3 => cut[..=i].to_string(),
                _ => cut,
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// How many sentences an answer gets (0 = as many as it needs).
fn sentence_cap(app: &AppHandle) -> usize {
    match config(app).length.as_str() {
        "short" => 1,
        "detailed" => 0,
        _ => 2,
    }
}

/// Byte position just after the `n`th finished sentence ("." "!" "?" followed by a space), skipping
/// initials like "J. K." and decimals like "3.5".
fn nth_sentence_end(t: &str, n: usize) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let b = t.as_bytes();
    let mut count = 0;
    for i in 0..b.len().saturating_sub(1) {
        let stop = matches!(b[i], b'.' | b'!' | b'?') && b[i + 1].is_ascii_whitespace();
        // a line break ends one too (Qwen writes its working on separate lines, without full stops)
        let line = b[i] == b'\n' && i > 0 && !matches!(b[i - 1], b'.' | b'!' | b'?' | b'\n');
        if stop || line {
            let initial = stop && b[i] == b'.' && i >= 1 && b[i - 1].is_ascii_uppercase() && (i == 1 || b[i - 2] == b' ');
            if !initial {
                count += 1;
                if count == n {
                    return Some(if line { i } else { i + 1 });
                }
            }
        }
    }
    None
}

/// Stop writing once the answer has its sentences and the brain has clearly started another one
/// (a command line, ">> open spotify", is still let through).
pub(crate) fn cut_point(t: &str, n: usize) -> Option<usize> {
    let end = nth_sentence_end(t, n)?;
    let rest = t[end..].trim_start();
    (rest.len() >= 6 && !rest.starts_with('>')).then_some(end)
}

/// A lean prompt for thinking questions (maths, why/how, explanations): no command list, no examples,
/// so the smart brain on the CPU starts answering in about a second instead of reading for twenty.
fn thinking_messages(app: &AppHandle, user: &str) -> Vec<(String, String)> {
    let mut m = vec![(
        "system".to_string(),
        "You are ULTRON, a sharp, warm assistant on the user's Windows PC. Your replies are spoken aloud. Think carefully and answer correctly and clearly in plain spoken English, no markdown, no lists. Keep it very short: one or two sentences, under 30 words; for maths, just the answer with one line of working. If you're not sure, say so instead of guessing. Never encourage violence, hatred or harm toward anyone."
            .to_string(),
    )];
    let h = app.state::<Ai>().history.lock().unwrap().clone();
    m.extend(h.into_iter().rev().take(4).collect::<Vec<_>>().into_iter().rev());
    if let Some(r) = crate::chats::recall(app, user, 3) {
        m.push(("system".into(), r));
    }
    m.push(("system".into(), context_line(app)));
    m.push(("user".into(), user.to_string()));
    m
}

fn build_messages(app: &AppHandle, user: &str, web: Option<&str>) -> Vec<(String, String)> {
    let mut m = vec![
        ("system".to_string(), system_prompt(&config(app).length) + &about_user(app)),
        ("user".into(), "I'm bored, put some music on".into()),
        ("assistant".into(), "Say no more, firing up Spotify.\n>> open spotify".into()),
        ("user".into(), "how's it going?".into()),
        ("assistant".into(), "Pretty great, thanks for asking. What can I do for you?".into()),
        ("user".into(), "explain how rainbows form".into()),
        ("assistant".into(), "Sunlight bends as it enters raindrops, splits into its colors, bounces off the back of each drop and bends again on the way out. Each color leaves at a slightly different angle, so you see a curved band of colors.".into()),
        ("user".into(), "open notepad and write a haiku about tea".into()),
        ("assistant".into(), "On it.\n>> open notepad\n>> write a haiku about tea".into()),
        ("user".into(), "tell priya on whatsapp that I'll be there in 10".into()),
        ("assistant".into(), "Sure, drafting that for Priya.\n>> message priya on whatsapp saying I'll be there in 10".into()),
        ("user".into(), "can you make what I selected sound more professional".into()),
        ("assistant".into(), "Polishing it up.\n>> rewrite the selected text to sound more professional".into()),
        ("user".into(), "what's the capital of japan?".into()),
        ("assistant".into(), "Tokyo. It's also the world's biggest metro area, with about 37 million people.".into()),
    ];
    m.extend(app.state::<Ai>().history.lock().unwrap().iter().cloned());
    if let Some(r) = crate::chats::recall(app, user, 3) {
        m.push(("system".into(), r));
    }
    // the details that change every minute go last, so everything above stays cached in the model
    m.push(("system".into(), moment_line(app)));
    if let Some(w) = web {
        m.push(("system".into(), format!("Fresh web results for the next question (fetched just now). Answer from them:\n{w}")));
    }
    m.push(("user".into(), user.to_string()));
    m
}

/// Have the quick brain's memory begin with the opening of every chat (ULTRON's instructions and the
/// example exchanges: about nine hundred words that never change). On the graphics card reading them
/// is nothing; on the processor it was eight seconds before the first word of an answer, every time
/// something else (a tool, a translation) had used the brain in between. So it's read once, saved, and
/// from then on put back in a few hundredths of a second.
pub fn quick_ready(app: &AppHandle) {
    let Some(port) = crate::quick::port() else { return };
    let _turn = crate::quick::TALK.lock().unwrap_or_else(|e| e.into_inner());
    // (instructions + what's known about the user + the examples: the first fifteen messages of every chat)
    let fixed: Vec<(String, String)> = build_messages(app, "", None).into_iter().take(15).collect();
    let print = fixed.iter().flat_map(|(r, c)| r.bytes().chain(c.bytes())).fold(0xcbf29ce484222325u64, |h, c| (h ^ c as u64).wrapping_mul(0x100000001b3));
    if *QUICK_HOLDS.lock().unwrap() == (port, "chat") && QUICK_OPENING.load(Ordering::SeqCst) == print {
        return;
    }
    let with = |u: &str| {
        let mut m = fixed.clone();
        m.push(("user".to_string(), u.to_string()));
        m
    };
    let (x, y) = (render(port, &with("a")), render(port, &with("b")));
    let n = x.char_indices().zip(y.chars()).take_while(|((_, p), q)| p == q).last().map(|((i, c), _)| i + c.len_utf8()).unwrap_or(0);
    if n < 200 {
        return;
    }
    let prefix = &x[..n];
    let key = prefix.bytes().chain(*b"quick-1").fold(0xcbf29ce484222325u64, |h, c| (h ^ c as u64).wrapping_mul(0x100000001b3));
    let file = format!("chat-{key:016x}.bin");
    let t0 = Instant::now();
    let url = |act: &str| format!("http://127.0.0.1:{port}/slots/0?action={act}");
    let restored = slot_dir().join(&file).exists() && ureq::post(&url("restore")).timeout(Duration::from_secs(20)).send_json(serde_json::json!({ "filename": file })).is_ok();
    if !restored {
        if !read_in(port, prefix) {
            return;
        }
        if let Ok(rd) = std::fs::read_dir(slot_dir()) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("chat-") && name != file {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let _ = ureq::post(&url("save")).timeout(Duration::from_secs(20)).send_json(serde_json::json!({ "filename": file }));
    }
    crate::log_line(&format!("quick brain: chat opening {} in {} ms", if restored { "put back from disk" } else { "read and saved" }, t0.elapsed().as_millis()));
    QUICK_OPENING.store(print, Ordering::SeqCst);
    *QUICK_HOLDS.lock().unwrap() = (port, "chat");
}

/// Right after an answer, while nobody's waiting: have the quick brain read the conversation as it
/// now stands, so the next question only adds its own few words.
fn prefill_quick(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        quick_ready(&app); // (the opening itself, if what's known about the user has changed)
        let Some(port) = crate::quick::port() else { return };
        let Ok(_turn) = crate::quick::TALK.try_lock() else { return };
        let msgs = build_messages(&app, "", None);
        let keep = msgs.len().saturating_sub(2); // (not the minute's details, not the question)
        let (a, b) = (render(port, &msgs[..keep]), render(port, &msgs));
        let n = a.char_indices().zip(b.chars()).take_while(|((_, x), y)| x == y).last().map(|((i, c), _)| i + c.len_utf8()).unwrap_or(0);
        if n > 200 && read_in(port, &a[..n]) {
            *QUICK_HOLDS.lock().unwrap() = (port, "chat");
        }
    });
}

fn chatml(msgs: &[(String, String)]) -> String {
    let mut s = String::new();
    for (r, c) in msgs {
        s.push_str(&format!("<|im_start|>{r}\n{c}<|im_end|>\n"));
    }
    s.push_str("<|im_start|>assistant\n");
    s
}

/// The conversation in the brain's own chat format (each model family has its own), via the server.
fn render(port: u16, msgs: &[(String, String)]) -> String {
    let body = serde_json::json!({ "messages": msgs.iter().map(|(r, c)| serde_json::json!({ "role": r, "content": c })).collect::<Vec<_>>() });
    ureq::post(&format!("http://127.0.0.1:{port}/apply-template"))
        .timeout(Duration::from_secs(5))
        .send_json(body)
        .ok()
        .and_then(|r| r.into_json::<serde_json::Value>().ok())
        .and_then(|v| v.get("prompt")?.as_str().map(str::to_string))
        // Qwen3 models answer straight away, without thinking out loud first
        .map(|p| if (*LOADED.lock().unwrap() == crate::router::QWEN3_8B.file && !crate::quick::no_think_known(port)) || crate::quick::no_think(port) { p + "<think>\n\n</think>\n\n" } else { p })
        .unwrap_or_else(|| chatml(msgs))
}

/// Stream a reply from the brain. `on_piece` gets the whole text so far after each new piece.
/// Writing, summaries, slides and the writing tools: on the quick brain when it's up (seconds, not half a minute).
fn generate(app: &AppHandle, messages: &[(String, String)], max: usize, temp: f64, on_piece: impl FnMut(&str)) -> Option<String> {
    generate_in(app, messages, max, temp, None, crate::quick::port(), 0, on_piece)
}

/// Ready to write: the quick brain is up, or the smart one is loaded.
fn can_write(app: &AppHandle) -> bool {
    crate::quick::port().is_some() || ready(app)
}

/// `at`: a specific server (the quick brain); otherwise the smart one.
/// Run something while no answer is being written (used to swap the quick brain's server).
pub fn between_answers<T>(_app: &AppHandle, f: impl FnOnce() -> T) -> T {
    let _turn = crate::quick::TALK.lock().unwrap_or_else(|e| e.into_inner());
    f()
}

fn generate_in(app: &AppHandle, messages: &[(String, String)], max: usize, temp: f64, grammar: Option<&str>, at: Option<u16>, sentences: usize, mut on_piece: impl FnMut(&str)) -> Option<String> {
    let ai = app.state::<Ai>();
    // the quick brain and the big one each take one answer at a time, independently
    let on_quick = at.map(crate::quick::no_think_known).unwrap_or(false);
    let _turn = if on_quick { crate::quick::TALK.lock().unwrap_or_else(|e| e.into_inner()) } else { ai.talk.lock().unwrap_or_else(|e| e.into_inner()) };
    if stale_translation() {
        return None;
    }
    if let (true, Some(p)) = (on_quick, at) {
        *QUICK_HOLDS.lock().unwrap() = (p, "other");
    }
    let port = match at {
        Some(p) => p,
        None => {
            *SLOT_HOLDS.lock().unwrap() = "chat";
            ai.server.lock().unwrap().as_ref()?.port
        }
    };
    let mut body = serde_json::json!({
        "prompt": render(port, messages),
        "n_predict": max,
        "stream": true,
        "cache_prompt": true,
        "temperature": temp,
        "top_k": 20,
        "top_p": 0.8,
        "min_p": 0.05,
        "repeat_penalty": 1.05,
        "stop": ["<|im_end|>", "<|endoftext|>", "<|im_start|>", "<|end|>", "<|user|>"],
    });
    if let Some(g) = grammar {
        body["grammar"] = serde_json::Value::String(g.to_string());
    }
    let mut text = String::new();
    // how fast it writes (the quick brain moves to the processor when the card has slowed down)
    let (mut pieces, mut first, mut latest) = (0usize, None::<Instant>, Instant::now());
    let reached = stream_post(port, "/completion", &body, Duration::from_secs(240), |v| {
        if cancelled() || stale_translation() {
            return false; // closing the connection stops the brain mid-sentence
        }
        if let Some(t) = v.get("content").and_then(|t| t.as_str()) {
            if !t.is_empty() {
                pieces += 1;
                latest = Instant::now();
                first.get_or_insert(latest);
                text.push_str(t);
                if let Some(cut) = cut_point(&text, sentences) {
                    text.truncate(cut); // enough said: stop the brain right here
                    on_piece(&text);
                    return false;
                }
                on_piece(&text);
            }
        }
        if v.get("stop").and_then(|s| s.as_bool()).unwrap_or(false) {
            if let Some(tps) = v.get("timings").and_then(|t| t.get("predicted_per_second")).and_then(|x| x.as_f64()) {
                if max > 8 && !on_quick {
                    let st = ai.status();
                    set_info(app, &st.model, &st.device, Some(tps));
                }
            }
            if let (true, Some(t)) = (on_quick && max > 8, v.get("timings")) {
                let n = |k: &str| t.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
                crate::log_line(&format!("quick brain: read {} new tokens in {:.0} ms, wrote {} in {:.0} ms", n("prompt_n"), n("prompt_ms"), n("predicted_n"), n("predicted_ms")));
            }
            return false;
        }
        true
    });
    if !reached {
        return None;
    }
    if crate::quick::no_think_known(port) {
        let secs = first.map(|f| (latest - f).as_secs_f64()).unwrap_or(0.0);
        let tps = if secs > 0.05 { pieces.saturating_sub(1) as f64 / secs } else { 999.0 };
        crate::quick::observe(port, tps, pieces);
    }
    *ai.last_used.lock().unwrap() = Instant::now();
    Some(text)
}

fn ready(app: &AppHandle) -> bool {
    let ai = app.state::<Ai>();
    if !ai.is_ready() {
        return false;
    }
    *ai.last_used.lock().unwrap() = Instant::now();
    ensure_loaded(app)
}

/// The visible part of a reply: everything before the first ">>" command.
fn visible(text: &str) -> &str {
    let cut = text.find(">>").unwrap_or(text.len());
    // hold back a trailing ">" that might be the start of a command
    text[..cut].trim_end_matches('>')
}

/// Questions where a bigger brain gives a better answer: maths, reasons, explanations, comparisons, plans, code.
fn needs_thinking(q: &str) -> bool {
    let l = q.to_lowercase();
    let w: Vec<&str> = l.split(|c: char| !c.is_alphanumeric() && c != '\'').filter(|w| !w.is_empty()).collect();
    let has = |p: &str| l.contains(p);
    let mathy = l.chars().filter(|c| c.is_ascii_digit()).count() >= 2 && (w.iter().any(|x| ["cost", "costs", "total", "percent", "average", "sum", "times", "divided",
        "multiply", "minus", "plus", "each", "per", "how", "calculate", "solve", "km", "speed", "rupees", "dollars", "hours", "minutes"].contains(x)) || "+-*/^%=".chars().any(|c| l.contains(c)));
    let start = ["why", "explain", "compare", "solve", "prove", "plan", "analyse", "analyze", "debug", "calculate", "derive", "evaluate", "reason"];
    mathy
        || w.first().map(|f| start.contains(f)).unwrap_or(false)
        || ["how does", "how do", "how can", "how would", "how should", "how to", "what if", "difference between", "pros and cons", "should i", "which is better",
            "step by step", "write code", "write a program", "write a function", "in python", "in javascript", "in rust", "algorithm", "riddle", "puzzle", "logic"].iter().any(|p| has(p))
}

/// Requests whose answer has several parts (a recipe, how to do something, a list, ideas, a plan,
/// a story): those get the room to be complete instead of the usual one or two sentences.
/// ("Here is the recipe:" and nothing after it was the sentence limit cutting the answer off.)
pub fn wants_steps(q: &str) -> bool {
    let l = format!(" {} ", q.to_lowercase().replace(['?', '!', '.', ','], " "));
    let has = |ws: &[&str]| ws.iter().any(|w| l.contains(w));
    // things that are done to the PC or asked about the PC aren't recipes
    if has(&[" my screen", " this page", " this window", " selected text", " my pc ", " my computer ", " my laptop "]) {
        return false;
    }
    let numbered = l.split_whitespace().any(|w| w.parse::<u32>().map(|n| (2..=20).contains(&n)).unwrap_or(false))
        && has(&[" ideas", " tips", " ways", " examples", " things", " reasons", " steps", " facts", " names", " options", " suggestions", " questions", " places", " books", " movies", " songs", " games"]);
    numbered
        || has(&[" recipe", " ingredients", " how to make ", " how to cook ", " how to bake ", " how to prepare ", " how to create ", " how to build ", " how to write ", " how to start ",
            " how to learn ", " how to set up ", " how to setup ", " how to install ", " how to draw ", " how to grow ", " how to become ", " how to get ", " how to do ", " how to use ", " how to improve ",
            " how do i make ", " how do i cook ", " how do i create ", " how do i build ", " how do i start ", " how do i learn ", " how do i set up ", " how do i install ", " how do i become ",
            " how can i make ", " how can i create ", " how can i build ", " how can i start ", " how can i learn ", " how can i improve ", " how can i become ",
            " steps to ", " steps for ", " step by step", " guide to ", " guide for ", " a guide ", " instructions ", " tutorial", " walk me through", " in detail", " detailed ",
            " list of ", " list the ", " list some ", " give me a list", " give me some ", " suggest some ", " tips for ", " tips on ", " tips to ", " ideas for ", " ideas to ", " ways to ",
            " a plan ", " plan for ", " plan my ", " routine", " workout", " diet plan", " meal plan", " itinerary", " schedule for ", " examples of ", " pros and cons",
            " tell me a story", " a story ", " short story", " a poem", " poem about", " a speech", " bedtime story"])
}

/// The dish a recipe request is about ("a recipe for shawarma", "how to make butter chicken at home"), to look up what it is.
fn dish_of(q: &str) -> Option<String> {
    let l = q.to_lowercase();
    let l = l.trim().trim_end_matches(['?', '.', '!']);
    let lead = ["recipe for ", "recipe of ", "how to make ", "how to cook ", "how to bake ", "how to prepare ", "how do i make ", "how do i cook ", "how can i make ", "ingredients for ", "ingredients of ", "ingredients in "];
    let dish = lead.iter().find_map(|p| l.find(p).map(|i| l[i + p.len()..].to_string())).or_else(|| l.strip_suffix(" recipe").map(|s| s.split_whitespace().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" ")))?;
    let words: Vec<&str> = dish.split_whitespace().filter(|w| !["a", "an", "the", "some", "good", "easy", "simple", "quick", "best", "me", "my", "at", "home", "from", "scratch", "please", "tell", "give"].contains(w)).collect();
    (!words.is_empty() && words.len() <= 4).then(|| words.join(" "))
}

/// The instructions for those: no command list, no examples, just "give the whole thing, tidily".
/// (The same words every time, so the brain can keep them ready: see `steps_ready`.)
fn steps_system(app: &AppHandle) -> String {
    let who = crate::intro::user_name(app).map(|n| format!(" The user's name is {n}.")).unwrap_or_default();
    format!("You are ULTRON, a sharp, warm assistant on the user's Windows PC. The user is asking for something with several parts: a recipe, how to do something, a list, ideas or a plan. \
Give the whole answer, tight and practical. Start with one short friendly line, then the parts on separate short lines, numbered 1. 2. 3. \
For a recipe: first one line starting with 'Ingredients:' that lists them with amounts, then the numbered steps. \
At most 8 steps or items, each under 20 words. For a story or a poem, just write it, under 120 words. \
You may put the key words of a line in **bold** (it's shown, not spoken); no headings, no tables, no emojis. Be accurate, and if you aren't sure about something say so. Never encourage violence, hatred or harm toward anyone.{who}")
}

/// Only the instructions and the request (with what was looked up): on a processor every extra word
/// the brain reads first is a moment longer before the answer starts.
fn steps_messages(app: &AppHandle, user: &str, web: Option<&str>) -> Vec<(String, String)> {
    let ask = match web {
        // what the thing actually is, looked up just now: without it the brain made "shawarma" out of minced beef and rice
        Some(w) => format!("Background from the web, to get the facts right (what it is, what really goes into it):\n{}\n\nRequest: {user}", w.chars().take(380).collect::<String>()),
        None => user.to_string(),
    };
    vec![("system".to_string(), steps_system(app)), ("user".to_string(), ask)]
}

/// Have the smart brain's memory begin with those instructions: from the copy saved on disk (a fraction
/// of a second), or by reading them once (a few seconds) and saving that. Then only the request is left to read.
fn steps_ready(app: &AppHandle) {
    let ai = app.state::<Ai>();
    let _turn = ai.talk.lock().unwrap_or_else(|e| e.into_inner());
    let Some(port) = ai.server.lock().unwrap().as_ref().map(|s| s.port) else { return };
    if *SLOT_HOLDS.lock().unwrap() == "steps" {
        return;
    }
    let with = |u: &str| vec![("system".to_string(), steps_system(app)), ("user".to_string(), u.to_string())];
    let (x, y) = (render(port, &with("a")), render(port, &with("b")));
    let n = x.char_indices().zip(y.chars()).take_while(|((_, p), q)| p == q).last().map(|((i, c), _)| i + c.len_utf8()).unwrap_or(0);
    if n < 200 {
        return;
    }
    let prefix = &x[..n];
    let key = prefix.bytes().chain(LOADED.lock().unwrap().bytes()).chain(*b"f16").fold(0xcbf29ce484222325u64, |h, c| (h ^ c as u64).wrapping_mul(0x100000001b3));
    let file = format!("steps-{key:016x}.bin");
    let t0 = Instant::now();
    let url = |act: &str| format!("http://127.0.0.1:{port}/slots/0?action={act}");
    let restored = slot_dir().join(&file).exists() && ureq::post(&url("restore")).timeout(Duration::from_secs(20)).send_json(serde_json::json!({ "filename": file })).is_ok();
    if !restored {
        if !read_in(port, prefix) {
            return; // stopped half-way: nothing is saved
        }
        if let Ok(rd) = std::fs::read_dir(slot_dir()) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("steps-") && name != file {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let _ = ureq::post(&url("save")).timeout(Duration::from_secs(20)).send_json(serde_json::json!({ "filename": file }));
    }
    crate::log_line(&format!("many-part instructions {} in {} ms", if restored { "restored from disk" } else { "read and saved" }, t0.elapsed().as_millis()));
    *SLOT_HOLDS.lock().unwrap() = "steps";
}

/// "what's 23 x 47", "15% of 240", "square root of 144": worked out exactly (a small brain only guesses at sums).
fn quick_math(q: &str) -> Option<String> {
    let mut e = q.to_lowercase().trim().trim_end_matches(['?', '.', '!', '=']).trim().to_string();
    for p in ["what is ", "what's ", "whats ", "calculate ", "compute ", "how much is ", "solve ", "evaluate ", "work out "] {
        if let Some(r) = e.strip_prefix(p) {
            e = r.trim().to_string();
            break;
        }
    }
    let e = e
        .replace(" percent of ", "% of ")
        .replace("% of ", "/100*")
        .replace("square root of ", "sqrt ")
        .replace(" plus ", "+")
        .replace(" minus ", "-")
        .replace(" multiplied by ", "*")
        .replace(" times ", "*")
        .replace(" divided by ", "/")
        .replace(" x ", "*")
        .replace(" into ", "*");
    let e = match e.strip_prefix("sqrt ") {
        Some(n) => format!("sqrt({})", n.trim()),
        None => e,
    };
    // numbers and signs only (any other word means it isn't a sum)
    let bare = e.replace("sqrt", "");
    if !bare.chars().any(|c| c.is_ascii_digit()) || !bare.chars().all(|c| c.is_ascii_digit() || " .+-*/^%()".contains(c)) {
        return None;
    }
    if !(bare.chars().any(|c| "+-*/^%".contains(c)) || e.contains("sqrt")) {
        return None;
    }
    let v = crate::tools::eval(&e).ok()?;
    if !v.is_finite() {
        return None;
    }
    let shown = if (v - v.round()).abs() < 1e-9 && v.abs() < 1e15 { format!("{}", v.round() as i64) } else { format!("{:.6}", v).trim_end_matches('0').trim_end_matches('.').to_string() };
    Some(format!("That's {shown}."))
}

pub fn ask(app: &AppHandle, user: &str) -> Option<Reply> {
    // a sum is worked out exactly, at once
    if let Some(r) = quick_math(user) {
        return Some(Reply { reply: r, action: String::new(), icon: "chat", ok: true });
    }
    let ai = app.state::<Ai>();
    // everyday chat goes to the quick brain on the graphics card (about a second);
    // questions that need real thinking go to the smart brain (Qwen3 4B on most PCs)
    // (a recipe or a how-to from the smart brain is noticeably more correct, and it writes at about reading pace)
    let many = wants_steps(user);
    let thinking = (many || needs_thinking(user)) && config(app).brain == "smart";
    // with a key of your own, the service answers (and no brain here has to be up for it);
    // without one, or if it can't be reached, it's the brains on this PC as always
    let cloud = crate::cloud::active(app);
    // everything else waits a moment for the quick brain if it's still warming up (just after ULTRON starts)
    let quick = if thinking { None } else if cloud.is_some() { crate::quick::port() } else { crate::quick::port().or_else(|| crate::quick::wait(Duration::from_secs(30))) };
    if cloud.is_none() && quick.is_none() && !ready(app) {
        return None;
    }
    if quick.is_some() {
        *ai.last_used.lock().unwrap() = Instant::now();
    }
    // people, events, news and weather: look them up first, the model alone would guess
    // (thinking questions are reasoning, not facts: no lookup, so the smart brain has less to read)
    // (for a recipe it's what the dish really is, so the brain doesn't reinvent it; other many-part answers need no lookup)
    let look_up = if many { dish_of(user) } else { Some(user.to_string()) };
    let web = if let (true, Some(q)) = (config(app).web && (!thinking || many || cloud.is_some()), look_up) {
        let looking = std::thread::spawn(move || if many { crate::web::wiki(&q) } else { crate::web::lookup(&q) });
        std::thread::sleep(Duration::from_millis(200));
        if !looking.is_finished() {
            let _ = app.emit_to("main", "ai-phase", "Looking it up…");
        }
        // don't keep you waiting on a slow site: answer without it after ~1.8 s
        let t0 = Instant::now();
        while !looking.is_finished() && t0.elapsed() < Duration::from_millis(1800) && !cancelled() {
            std::thread::sleep(Duration::from_millis(40));
        }
        if looking.is_finished() { looking.join().ok().flatten() } else { None }
    } else {
        None
    };
    let cap = if many { 0 } else { sentence_cap(app) };
    let max = if many { 400 } else { match (cap, thinking) { (0, _) => 320, (1, false) => 50, (1, true) => 80, (_, false) => 90, (_, true) => 140 } };
    // (a service or the graphics card reads it all in a blink; the processor gets the first few sentences)
    let brief_web = web.as_deref().map(|w| if crate::quick::device() == Some(false) && !many { clip(w, 340) } else { w.to_string() });
    let mut shown = 0usize;
    let mut emit = |full: &str| {
        let v = visible(full);
        let v = &v[..nth_sentence_end(v, cap).unwrap_or(v.len())];
        if v.len() > shown && v.is_char_boundary(shown) {
            let _ = app.emit_to("main", "ai-token", &v[shown..]);
            shown = v.len();
        }
    };
    let t_gen = Instant::now();
    let mut via = "";
    let mut text = None;
    if let Some(c) = &cloud {
        // (a service gets the ordinary conversation prompt: it needs no lean one to be quick)
        let msgs = if many { steps_messages(app, user, web.as_deref()) } else { build_messages(app, user, web.as_deref()) };
        match crate::cloud::chat(c, &msgs, max.max(160), 0.3, cap, |f| emit(f), cancelled) {
            Ok(t) => {
                text = Some(t);
                via = "cloud";
            }
            Err(e) => {
                crate::log_line(&format!("your own key: {e} Answering on this PC instead."));
                let _ = app.emit_to("main", "ai-phase", format!("Couldn't reach {}, answering here…", crate::cloud::name_of(&c.provider)));
            }
        }
    }
    let text = match text {
        Some(t) => t,
        None => {
            if cancelled() {
                return Some(Reply { reply: "Stopped.".into(), action: String::new(), icon: "chat", ok: true });
            }
            if quick.is_none() && !ready(app) {
                return None;
            }
            if many && quick.is_none() {
                steps_ready(app);
            }
            if quick.is_some() && !many {
                quick_ready(app);
            }
            let msgs = if many { steps_messages(app, user, web.as_deref()) } else if thinking { thinking_messages(app, user) } else { build_messages(app, user, brief_web.as_deref()) };
            let t = generate_in(app, &msgs, max, if many { 0.3 } else if quick.is_some() { 0.25 } else { 0.4 }, None, quick, cap, |f| emit(f))?;
            if let (Some(p), false) = (quick, many) {
                *QUICK_HOLDS.lock().unwrap() = (p, "chat"); // (its memory still starts with the chat opening)
            }
            via = if quick.is_some() { "quick" } else { "smart" };
            t
        }
    };
    if cancelled() {
        let said = visible(&text).trim().to_string();
        ai.remember(user, &said);
        crate::chats::add(app, user, &said);
        return Some(Reply { reply: if said.is_empty() { "Stopped.".into() } else { said }, action: String::new(), icon: "chat", ok: true });
    }
    let vis = visible(&text);
    let reply_text = vis[..nth_sentence_end(vis, cap).unwrap_or(vis.len())].trim().to_string();
    crate::log_line(&format!("answered by {} in {} ms{}{}", if via == "cloud" { "your own key".to_string() } else { format!("the {via} brain") }, t_gen.elapsed().as_millis(), if web.is_some() { " (with web facts)" } else { "" }, if many { " (a many-part answer)" } else if thinking { " (a thinking question)" } else { "" }));
    let wants_typing = ["type", "write", "dictate", "compose", "draft", "note", "essay", "words"].iter().any(|w| user.to_lowercase().contains(w));
    // the quick brain is for talking: its commands only count when the action model agrees it's a task
    let trusted = !many && (via != "quick" || crate::uam::classify(user).map(|(k, c)| k != "chat" && c >= 0.4).unwrap_or(false));
    let actions: Vec<String> = text
        .find(">>")
        .filter(|_| trusted)
        .map(|i| &text[i..])
        .unwrap_or("")
        .split(">>")
        .map(|a| a.lines().next().unwrap_or("").trim().trim_end_matches('.').to_string())
        .filter(|a| !a.is_empty())
        .filter(|a| wants_typing || !["type ", "write ", "compose "].iter().any(|w| a.to_lowercase().starts_with(w)))
        .take(6)
        .collect();
    let mut logs = Vec::new();
    let mut icon = "chat";
    let mut fallback = Vec::new();
    let mut ok = true;
    let steps: Vec<crate::agent::Step> = actions.iter().map(|a| crate::agent::parse_step(a)).collect();
    if crate::agent::wants_agent(&steps) {
        // several steps, or keyboard/mouse work: ULTRON's cursor does it
        let r = crate::agent::execute(app, steps);
        if !r.action.is_empty() {
            logs.push(r.action.clone());
        }
        icon = r.icon;
        ok = r.ok;
        fallback.push(r.reply);
    } else {
        for a in &actions {
            let r = commands::run(app, a);
            if !r.action.is_empty() {
                logs.push(r.action.clone());
                icon = r.icon;
            }
            fallback.push(r.reply);
        }
    }
    let reply = if !ok || reply_text.is_empty() { fallback.join(" ") } else { reply_text };
    // a long answer is remembered by its opening (the whole thing would be re-read with every later question)
    let kept = if many && reply.chars().count() > 280 { format!("{}…", reply.chars().take(260).collect::<String>().trim_end()) } else { reply.clone() };
    let mut remembered = kept.clone();
    for a in &actions {
        remembered.push_str(&format!("\n>> {a}"));
    }
    ai.remember(user, &remembered);
    crate::chats::add(app, user, &kept);
    // (while nobody's waiting, the brain that answered reads the conversation as it now stands)
    if via == "quick" && !many {
        prefill_quick(app);
    } else if via == "smart" {
        prefill(app);
    }
    Some(Reply { reply, action: logs.join(" · "), icon, ok })
}

/// Read the conversation so far right after answering, so the next question only adds its own words.
fn prefill(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || prefill_now(&app));
}

/// (Some brains can only continue exactly where they left off, so this matters even more for them.)
fn prefill_now(app: &AppHandle) {
    {
        let app = app.clone();
        // the smart brain now answers thinking questions with the lean prompt: have that one read already
        let msgs = thinking_messages(&app, "");
        // everything up to (not including) the per-minute context and the next question
        let keep = msgs.len().saturating_sub(2);
        let ai = app.state::<Ai>();
        let Ok(_turn) = ai.talk.try_lock() else { return };
        let Some(port) = ai.server.lock().unwrap().as_ref().map(|s| s.port) else { return };
        // the part both this and the next prompt will start with
        let (a, b) = (render(port, &msgs[..keep]), render(port, &msgs));
        let n = a.char_indices().zip(b.chars()).take_while(|((_, x), y)| x == y).last().map(|((i, c), _)| i + c.len_utf8()).unwrap_or(0);
        let s = &a[..n];
        let _ = ureq::post(&format!("http://127.0.0.1:{port}/completion"))
            .timeout(Duration::from_secs(60))
            .send_json(serde_json::json!({ "prompt": s, "n_predict": 0, "cache_prompt": true }));
    }
}

/// More rules for the stronger planner (long jobs inside websites and apps), and what it must hand back to the person.
const BIG_RULES: &str = "\nBigger jobs:\n\
- You can finish long jobs inside any website or app (building something in an online tool, filling in forms, setting things up): open it, look, act, look again; when a page is still loading, wait and look again. Keep going until the job is really finished, then call done.\n\
- Websites open with open (give the full https link). To type into a page use type_in_field with the number from look.\n\
- Never type, guess or invent a password, a card or bank number, an ID number or a verification code, never solve a CAPTCHA, and never accept terms or a paid plan for the user. When a page needs one of those, call ask_user and say exactly what they need to do themselves.\n\
- Creating an account: fill in only what you know about the user from this conversation; ask_user for anything else.\n\
- Messages: only to people the user named. For \"everyone\" or \"all my contacts\", call ask_user for the list of names first; never work through a contact list on your own.\n";

/// One step of the task agent (doer.rs): the smart brain, held to a JSON schema of the tools, so
/// every reply is a well-formed call to a tool that exists.
///
/// (Tried and dropped: writing the call in two parts, tool first and then only that tool's rules.
/// It was a little faster, but the brain likes to end a call with one `"}}` and, held to the inner
/// part alone, sometimes wrote junk into the last value instead.)
pub fn agent_call(app: &AppHandle, messages: &[(String, String)], schema: &serde_json::Value, max: usize) -> Option<String> {
    // With your own key switched on, that service's model plans the task: the same tools and rules, a far
    // stronger planner, so long jobs inside websites and apps become possible. (What the tools report back,
    // the names of what's on screen included, goes to that service while the key is on.)
    if let Some(a) = crate::cloud::active(app) {
        let mut msgs = messages.to_vec();
        if let Some(first) = msgs.first_mut() {
            first.1.push_str(BIG_RULES);
            first.1.push_str(&format!("\nAnswer with exactly one JSON object and nothing else (no code fence, no words around it). It must match this schema:\n{schema}"));
        }
        let t0 = Instant::now();
        match crate::cloud::chat(&a, &msgs, max.max(2400), 0.2, 0, |_| {}, cancelled) {
            Ok(t) => {
                if let Some(call) = t.find('{').zip(t.rfind('}')).filter(|(a, b)| a < b).map(|(a, b)| t[a..=b].to_string()) {
                    crate::log_line(&format!("agent step: planned with your own key in {} ms", t0.elapsed().as_millis()));
                    return Some(call);
                }
                crate::log_line("agent step: your key's service answered without a tool call; this PC's brain takes it");
            }
            Err(e) => crate::log_line(&format!("agent step: your key's service couldn't be reached ({e}); this PC's brain takes it")),
        }
        if cancelled() {
            return None;
        }
    }
    if !ready(app) {
        return None;
    }
    let ai = app.state::<Ai>();
    let _turn = ai.talk.lock().unwrap();
    let port = ai.server.lock().unwrap().as_ref()?.port;
    *SLOT_HOLDS.lock().unwrap() = "agent";
    let body = serde_json::json!({
        "prompt": render(port, messages),
        "n_predict": max,
        "stream": true,
        "cache_prompt": true,
        "temperature": 0.2,
        "top_k": 20,
        "top_p": 0.8,
        "min_p": 0.05,
        "json_schema": schema,
        "stop": ["<|im_end|>", "<|endoftext|>", "<|im_start|>", "<|end|>", "<|user|>"],
    });
    let mut text = String::new();
    let reached = stream_post(port, "/completion", &body, Duration::from_secs(300), |v| {
        if cancelled() {
            return false;
        }
        if let Some(t) = v.get("content").and_then(|t| t.as_str()) {
            text.push_str(t);
        }
        if v.get("stop").and_then(|s| s.as_bool()).unwrap_or(false) {
            // where the time went: reading what's new in the conversation, or writing the call
            if let Some(t) = v.get("timings") {
                let n = |k: &str| t.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
                crate::log_line(&format!("agent step: read {} new tokens in {:.1} s, wrote {} in {:.1} s ({:.1} a second)",
                    n("prompt_n"), n("prompt_ms") / 1000.0, n("predicted_n"), n("predicted_ms") / 1000.0, n("predicted_per_second")));
            }
            return false;
        }
        true
    });
    if !reached {
        crate::log_line("agent call failed: the brain didn't answer");
        return None;
    }
    *ai.last_used.lock().unwrap() = Instant::now();
    Some(text)
}

/// Get the task agent's long instructions (the part every task's prompt starts with) into the big
/// brain: from the copy saved on disk when there is one (a fraction of a second), otherwise by reading
/// them once (about half a minute on a processor) and saving that for every later task and restart.
/// `wait`: a task is starting, so load the brain and wait for it; otherwise only if it's free right now.
pub fn agent_ready(app: &AppHandle, a: &[(String, String)], b: &[(String, String)], wait: bool) {
    if wait && !ready(app) {
        return;
    }
    let ai = app.state::<Ai>();
    let _turn = if wait {
        ai.talk.lock().unwrap()
    } else {
        match ai.talk.try_lock() {
            Ok(g) => g,
            Err(_) => return,
        }
    };
    let Some(port) = ai.server.lock().unwrap().as_ref().map(|s| s.port) else { return };
    if *SLOT_HOLDS.lock().unwrap() == "agent" {
        return;
    }
    let (x, y) = (render(port, a), render(port, b));
    let n = x.char_indices().zip(y.chars()).take_while(|((_, p), q)| p == q).last().map(|((i, c), _)| i + c.len_utf8()).unwrap_or(0);
    if n < 200 {
        return;
    }
    let prefix = &x[..n];
    // (the "f16" is how the server keeps its context: a copy saved under another setting can't be loaded back)
    let key = prefix.bytes().chain(LOADED.lock().unwrap().bytes()).chain(*b"f16").fold(0xcbf29ce484222325u64, |h, c| (h ^ c as u64).wrapping_mul(0x100000001b3));
    let file = format!("agent-{key:016x}.bin");
    let t0 = Instant::now();
    let url = |act: &str| format!("http://127.0.0.1:{port}/slots/0?action={act}");
    let restored = slot_dir().join(&file).exists()
        && ureq::post(&url("restore")).timeout(Duration::from_secs(40)).send_json(serde_json::json!({ "filename": file })).is_ok();
    if restored {
        crate::log_line(&format!("task agent instructions restored from disk in {} ms", t0.elapsed().as_millis()));
    } else {
        if !read_in(port, prefix) {
            return; // stopped half-way: nothing is saved
        }
        // older saved copies (another brain, or older instructions) aren't needed any more
        if let Ok(rd) = std::fs::read_dir(slot_dir()) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("agent-") && name != file {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let saved = ureq::post(&url("save")).timeout(Duration::from_secs(40)).send_json(serde_json::json!({ "filename": file })).is_ok();
        crate::log_line(&format!("task agent instructions read in {} ms{}", t0.elapsed().as_millis(), if saved { " and saved for next time" } else { "" }));
    }
    *SLOT_HOLDS.lock().unwrap() = "agent";
    *ai.last_used.lock().unwrap() = Instant::now();
}

/// ULTRON's command language. The brain can't write anything outside it.
const ACTION_GRAMMAR: &str = r#"
root ::= line ("\n" line){0,5}
line ::= ">> " cmd
cmd ::= "open " t | "close " t | "switch to " t | "search for " t | "search youtube for " t | "play " t " on spotify" | "play " t " on youtube" |
  "pause" | "resume" | "next song" | "previous song" | "set volume to " n | "volume up" | "volume down" | "mute" | "unmute" |
  "type " t " in the " t " box" | "type " t | "write " t | "press " t | "click " t | "save as " t |
  "find " t " on " site | "make a " n " slide presentation about " t |
  "message " t " on " ("whatsapp" | "telegram" | "discord") " saying " t | "email " t " about " t " saying " t |
  "rewrite the selected text to " t | "summarize the screen" | "answer about the screen: " t |
  "set a timer for " t | "remind me in " t " to " t | "snap window left" | "snap window right" | "close this window" |
  "set brightness to " n | "dark mode on" | "dark mode off" | "open " t " settings" | "find file " t | "open latest download" |
  "new folder " t " on " ("desktop" | "documents" | "downloads") | "wait " n | "take a screenshot" | "show desktop" | "lock the pc"
site ::= "amazon" | "flipkart" | "youtube" | "google" | "maps" | "wikipedia" | "github" | "reddit" | "netflix"
t ::= [^
]+
n ::= [0-9] [0-9]?
"#;

/// The action model knew you wanted something done, but ULTRON's rules couldn't read it: the brain
/// rewrites it as ULTRON commands (held to the grammar above), one per line.
pub fn plan_actions(app: &AppHandle, user: &str) -> Option<Vec<String>> {
    let quick = crate::quick::port();
    if quick.is_none() && !ready(app) {
        return None;
    }
    let msgs = vec![
        ("system".to_string(), "You turn the user's request into ULTRON commands, in order, one per line, each starting with \">> \". Keep names, numbers and quoted text exactly as the user said them. Use as few commands as needed.".to_string()),
        ("user".into(), "could you pull up spotify and put on some billie jean".into()),
        ("assistant".into(), ">> play billie jean on spotify".into()),
        ("user".into(), "jot down a haiku about rain in notepad then save it as rain".into()),
        ("assistant".into(), ">> open notepad\n>> write a haiku about rain\n>> save as rain".into()),
        ("user".into(), "shoot rahul a whatsapp that i'm on my way".into()),
        ("assistant".into(), ">> message rahul on whatsapp saying i'm on my way".into()),
        ("user".into(), "I need 3 slides on volcanoes".into()),
        ("assistant".into(), ">> make a 3 slide presentation about volcanoes".into()),
        ("user".into(), user.to_string()),
    ];
    let t0 = Instant::now();
    let out = generate_in(app, &msgs, 160, 0.1, Some(ACTION_GRAMMAR), quick, 0, |_| {})?;
    let lines: Vec<String> = out.lines().filter_map(|l| l.trim().strip_prefix(">> ")).map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
    crate::log_line(&format!("action planner ({} ms): {lines:?}", t0.elapsed().as_millis()));
    (!lines.is_empty()).then_some(lines)
}

/// A one-off answer with its own instructions (writing tools, screen questions). Streams to `on_piece`.
pub fn oneshot(app: &AppHandle, system: &str, user: &str, max: usize, on_piece: impl FnMut(&str)) -> Option<String> {
    // translating goes to the smart brain (the quick one garbles other languages); everything else is quick
    let translating = user.to_lowercase().contains("translate");
    if translating && !ready(app) || !translating && !can_write(app) {
        return None;
    }
    let msgs = vec![("system".to_string(), system.to_string()), ("user".to_string(), user.to_string())];
    let at = if translating { None } else { crate::quick::port() };
    generate_in(app, &msgs, max, 0.5, None, at, 0, on_piece).map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

/// A short piece of writing from the quick brain about text you didn't write yourself (an email, a page).
pub fn brief(app: &AppHandle, system: &str, user: &str, max: usize) -> Option<String> {
    if !can_write(app) {
        return None;
    }
    let msgs = vec![("system".to_string(), system.to_string()), ("user".to_string(), user.to_string())];
    generate_in(app, &msgs, max, 0.3, None, crate::quick::port(), 0, |_| {}).map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

/// Slides for a presentation, in a simple line format the deck builder reads. Streams
/// the number of slides written so far to `progress`.
pub fn outline(app: &AppHandle, topic: &str, n: usize, facts: Option<&str>, mut progress: impl FnMut(&str)) -> Option<String> {
    if !can_write(app) {
        return None;
    }
    let sys = "You write presentation slides. Reply ONLY in this exact format, nothing before or after it:\n\
TITLE: <short deck title>\nSUBTITLE: <one short line>\nSLIDE: <slide title>\n- <point, under 12 words>\n- <point>\n- <point>\n\
NOTE: <one sentence the presenter can say>\n(then the next SLIDE block)";
    let mut user = format!(
        "Make a {n}-slide presentation about {topic}. Give each slide 3 or 4 short, specific points (facts, numbers, examples), never vague filler. \
Don't repeat points between slides. The last slide sums it up."
    );
    if let Some(f) = facts {
        user.push_str(&format!("\n\nFacts you can use:\n{f}"));
    }
    let msgs = vec![("system".to_string(), sys.to_string()), ("user".to_string(), user)];
    generate(app, &msgs, (n * 80 + 80).min(1300), 0.5, |full| progress(full))
}

/// Write something to be typed straight into an app ("100 words on the USA"), streaming as it goes.
pub fn compose(app: &AppHandle, what: &str, mut on_piece: impl FnMut(&str)) -> bool {
    if !can_write(app) {
        return false;
    }
    // ground it in real facts when it's about a topic
    let topic = [" about ", " on ", " regarding ", " describing "].iter().find_map(|p| what.find(p).map(|i| what[i + p.len()..].trim().to_string()));
    let facts = if config(app).web { topic.as_deref().and_then(crate::web::wiki) } else { None };
    let words = what.split_whitespace().find_map(|w| w.parse::<usize>().ok()).filter(|n| (10..=1500).contains(n));
    let mut sys = String::from(
        "You are an excellent writer. You write text that is typed straight into the user's app, so write exactly what they ask for, ready to use, \
in plain text: well organised, specific and accurate, with natural flowing sentences. No intro like \"Sure\" or \"Here is\", no title unless asked, \
no markdown, no quotes around it, nothing after it.",
    );
    if let Some(n) = words {
        sys.push_str(&format!(" It must be about {n} words long."));
    }
    let mut user = format!("Write {what}");
    if let Some(f) = facts {
        user.push_str(&format!("\n\nAccurate facts you can use:\n{f}"));
    }
    let max = words.map(|n| n * 2 + 60).unwrap_or(500).min(2000);
    let msgs = vec![("system".to_string(), sys), ("user".to_string(), user)];
    let mut sent = 0usize;
    let mut began = false;
    let out = generate(app, &msgs, max, 0.6, |full| {
        if !began {
            let lead = full.len() - full.trim_start().len();
            if lead == full.len() {
                return;
            }
            began = true;
            sent = lead;
        }
        if full.len() > sent {
            on_piece(&full[sent..]);
            sent = full.len();
        }
    });
    out.is_some()
}

/// The newest translation asked for. While you type, each pause asks again: an older one that's still
/// being written is dropped the moment a newer one arrives, instead of making the new one wait its turn.
static TR_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
thread_local! { static TR_MINE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
fn stale_translation() -> bool {
    let mine = TR_MINE.with(|m| m.get());
    mine != 0 && TR_SEQ.load(std::sync::atomic::Ordering::Relaxed) != mine
}

/// Languages the quick brain reads well enough to put into English. Measured on this PC: into English it
/// is right and takes about half a second; INTO other languages, and anything involving Hindi, Marathi,
/// Urdu or Tamil, it makes real mistakes, so all of that stays with the big brain.
fn quick_reads(lang: &str) -> bool {
    matches!(lang, "Spanish" | "French" | "German" | "Italian" | "Portuguese" | "Dutch" | "Russian" | "Japanese" | "Chinese" | "Korean" | "Arabic")
}

/// Translate with the local brain (Language menu). No history, no actions. The words are sent to the
/// page as they're written ("tr-piece"), so the first ones show within a second instead of all of
/// them arriving together at the end.
pub fn translate(app: &AppHandle, text: &str, from: &str, to: &str, id: u64) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    let id = id.max(1);
    TR_SEQ.store(id, std::sync::atomic::Ordering::Relaxed);
    TR_MINE.with(|m| m.set(id));
    struct Done;
    impl Drop for Done {
        fn drop(&mut self) {
            TR_MINE.with(|m| m.set(0)); // (these threads are reused for other work)
        }
    }
    let _done = Done;
    let src = if from.is_empty() || from == "Auto" { String::new() } else { format!(" from {from}") };
    let msgs = vec![
        (
            "system".to_string(),
            format!("You are a translator. Translate the user's text{src} into {to}. Reply with only the translation in {to}, written in its own script. No quotes, no notes, no explanations."),
        ),
        ("user".to_string(), text.trim().to_string()),
    ];
    let tidy = |t: &str| t.trim().trim_matches('"').trim().to_string();
    let t0 = Instant::now();
    let mut sent = 0usize;
    // whole words only: the last one may still be growing
    let mut piece = |full: &str| {
        let upto = full.trim_end().rfind(char::is_whitespace).unwrap_or(0);
        if upto > sent {
            sent = upto;
            let _ = app.emit_to("main", "tr-piece", serde_json::json!({ "id": id, "text": tidy(&full[..upto]) }));
        }
    };
    let mut out = None;
    let mut by = "quick";
    if let Some(c) = crate::cloud::active(app) {
        match crate::cloud::chat(&c, &msgs, 500, 0.2, 0, |f| piece(f), stale_translation) {
            Ok(t) => {
                out = Some(tidy(&t)).filter(|t| !t.is_empty());
                by = "service's";
            }
            Err(e) => crate::log_line(&format!("your own key: {e} Translating on this PC instead.")),
        }
    }
    if out.is_none() && config(app).web && !stale_translation() {
        if let Some(t) = crate::web::translate(text, from, to) {
            out = Some(t);
            by = "web's";
        }
    }
    if out.is_none() && to == "English" && quick_reads(from) {
        if let Some(q) = crate::quick::port() {
            out = generate_in(app, &msgs, 220, 0.2, None, Some(q), 0, |f| piece(f)).map(|t| tidy(&t)).filter(|t| !t.is_empty());
        }
    }
    if out.is_none() && !stale_translation() {
        by = "smart";
        // right after start-up the brain may still be checking its files: wait for it rather than giving up
        while !app.state::<Ai>().is_ready() && t0.elapsed() < Duration::from_secs(25) && !stale_translation() {
            std::thread::sleep(Duration::from_millis(200));
        }
        // (if it has just been restarted, try once more)
        for _ in 0..2 {
            if out.is_none() && !stale_translation() && ready(app) {
                out = generate_in(app, &msgs, 220, 0.2, None, None, 0, |f| piece(f)).map(|t| tidy(&t)).filter(|t| !t.is_empty());
            }
        }
    }
    if out.is_none() && !stale_translation() {
        if let Some(q) = crate::quick::port() {
            by = "quick";
            crate::log_line("translate: the big brain wasn't available, used the quick one");
            out = generate_in(app, &msgs, 220, 0.2, None, Some(q), 0, |f| piece(f)).map(|t| tidy(&t)).filter(|t| !t.is_empty());
        }
    }
    if stale_translation() {
        return None;
    }
    if out.is_some() {
        crate::log_line(&format!("translated into {to} by the {by} brain in {} ms", t0.elapsed().as_millis()));
        // (the Language panel says where it came from)
        let _ = app.emit_to("main", "tr-by", serde_json::json!({ "id": id, "how": if by == "web's" { "online" } else if by == "service's" { "key" } else { "pc" } }));
    }
    out
}

/// Wake the big brain now (the Language menu was opened, a task is coming): it's loaded by the time it's needed.
pub fn wake(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        if app.state::<Ai>().is_ready() {
            *app.state::<Ai>().last_used.lock().unwrap() = Instant::now();
            ensure_loaded(&app);
        }
    });
}

#[cfg(test)]
mod route_tests {
    #[test]
    fn thinking_questions() {
        for (q, want) in [("A shop sells pens at 3 for 45 rupees. How much do 7 pens cost?", true), ("why is the sky blue", true), ("explain black holes", true),
            ("how does a car engine work", true), ("what's 15% of 240", true), ("who is Nana Patekar", false), ("tell me a joke", false), ("hello", false),
            ("what was the Junko case", false), ("should I learn rust or go", true), ("write a function to reverse a string in python", true)] {
            assert_eq!(super::needs_thinking(q), want, "{q}");
        }
    }

    #[test]
    fn many_part_answers() {
        for (q, want) in [("Can you tell me a recipe for Shawarma", true), ("How to make a Shawarma", true), ("How to create a Book", true), ("give me 5 ideas for a birthday gift", true),
            ("tell me a short story about a robot", true), ("what are the steps to get a passport?", true), ("list some good sci-fi movies", true), ("suggest a workout routine for beginners", true),
            ("who is Narendra Modi", false), ("hello", false), ("what's 15% of 240", false), ("why is the sky blue", false), ("how are you", false), ("what's the weather", false),
            ("how to fix this error on my screen", false), ("open notepad", false), ("what time is it", false)] {
            assert_eq!(super::wants_steps(q), want, "{q}");
        }
        for (q, dish) in [("Can you tell me a recipe for Shawarma", Some("shawarma")), ("how to make butter chicken at home?", Some("butter chicken")), ("give me a good pasta recipe", Some("pasta")),
            ("How to create a Book", None), ("give me 5 ideas for a birthday gift", None)] {
            assert_eq!(super::dish_of(q).as_deref(), dish, "{q}");
        }
    }
}

#[cfg(test)]
mod cap_tests {
    use super::*;
    #[test]
    fn sentences() {
        let t = "J. K. Rowling wrote it in 1997. It sold 3.5 million copies. Then more came";
        assert_eq!(&t[..nth_sentence_end(t, 1).unwrap()], "J. K. Rowling wrote it in 1997.");
        assert_eq!(&t[..nth_sentence_end(t, 2).unwrap()], "J. K. Rowling wrote it in 1997. It sold 3.5 million copies.");
        assert_eq!(cut_point("Firing up Spotify. Here", 1), None);
        assert_eq!(cut_point("Firing up Spotify. Here we go now", 1), Some(18));
        assert_eq!(cut_point("Firing up Spotify.\n>> open spotify", 1), None);
    }
}

/// A long piece of writing from the smart brain, free of any form (a whole file, say). With your own key
/// switched on, that service writes it. `on_text` is shown the text so far as it grows.
pub fn write_long(app: &AppHandle, messages: &[(String, String)], max: usize, mut on_text: impl FnMut(&str)) -> Option<String> {
    if let Some(a) = crate::cloud::active(app) {
        match crate::cloud::chat(&a, messages, max.max(8000), 0.3, 0, |t| on_text(t), cancelled) {
            Ok(t) if !t.trim().is_empty() => {
                crate::log_line("long writing: written with your own key");
                return Some(t);
            }
            Ok(_) => {}
            Err(e) => crate::log_line(&format!("long writing: your key's service couldn't be reached ({e}); this PC's brain takes it")),
        }
        if cancelled() {
            return None;
        }
    }
    if !ready(app) {
        return None;
    }
    let ai = app.state::<Ai>();
    let _turn = ai.talk.lock().unwrap();
    let port = ai.server.lock().unwrap().as_ref()?.port;
    *SLOT_HOLDS.lock().unwrap() = "other";
    let body = serde_json::json!({
        "prompt": render(port, messages),
        "n_predict": max,
        "stream": true,
        "cache_prompt": true,
        "temperature": 0.3,
        "top_k": 20,
        "top_p": 0.8,
        "min_p": 0.05,
        "stop": ["<|im_end|>", "<|endoftext|>", "<|im_start|>", "<|end|>", "<|user|>"],
    });
    let mut text = String::new();
    let reached = stream_post(port, "/completion", &body, Duration::from_secs(300), |v| {
        if cancelled() {
            return false;
        }
        if let Some(t) = v.get("content").and_then(|t| t.as_str()) {
            text.push_str(t);
            on_text(&text);
        }
        if v.get("stop").and_then(|s| s.as_bool()).unwrap_or(false) {
            if let Some(t) = v.get("timings") {
                let n = |k: &str| t.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
                crate::log_line(&format!("long writing: wrote {} tokens in {:.0} s ({:.1} a second)", n("predicted_n"), n("predicted_ms") / 1000.0, n("predicted_per_second")));
            }
            return false;
        }
        true
    });
    if !reached {
        crate::log_line("long writing failed: the brain didn't answer");
        return None;
    }
    *ai.last_used.lock().unwrap() = Instant::now();
    Some(text)
}
