//! The quick brain: a small model (Qwen3 1.7B, or the bundled Qwen2.5 1.5B) kept just for
//! conversation and the AI tools, so answers land in about half a second. Measured on this PC: on
//! the graphics card it reads a question with web facts at 1200-1800 tokens/s and writes at 60-70.
//! The first GPU request compiles its shaders (~40 s), so that's done here, quietly, after start-up.
//!
//! Where it runs is watched, not assumed. In Windows' "Best power efficiency" mode some laptops (this
//! ASUS among them) lock the card's memory at 810 MHz: the brain then wrote 6 tokens/s instead of 65
//! and held the card at 99%, and since that card also draws the screen, everything stuttered while it
//! answered. So when answers come out slow it moves to the processor (16 tokens/s in the same mode,
//! and the screen stays smooth), and goes back to the card once full power returns. On the card it
//! also gets a lower graphics priority than the screen, so frames always come first.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager};

const NO_WINDOW: u32 = 0x0000_0008; // DETACHED_PROCESS: no console (and no Console Window Host beside it)
const BELOW_NORMAL: u32 = 0x0000_4000;
/// Qwen3 1.7B when it's been downloaded (sharper), else the bundled Qwen2.5 1.5B
const FILES: [&str; 2] = ["Qwen3-1.7B-Q4_K_M.gguf", "qwen2.5-1.5b-instruct-q4_k_m.gguf"];
/// Below this many tokens a second on the card, the processor does better (and the screen stays smooth)
const SLOW_TPS: f64 = 20.0;

struct Server {
    child: Child,
    port: u16,
    gpu: bool,
}

static SERVER: Mutex<Option<Server>> = Mutex::new(None);
/// one answer at a time on the quick brain (its own queue: it never waits for the big brain)
pub static TALK: Mutex<()> = Mutex::new(());
static READY: AtomicBool = AtomicBool::new(false);
static WARMING: AtomicBool = AtomicBool::new(false);
static MOVING: AtomicBool = AtomicBool::new(false);
/// Qwen3 thinks out loud unless its turn starts with an empty <think> block
static QWEN3: AtomicBool = AtomicBool::new(false);
/// slow answers in a row on the graphics card
static SLOW: AtomicU32 = AtomicU32::new(0);
static APP: OnceLock<AppHandle> = OnceLock::new();
/// when it last answered, and the power state when it last gave up on the card
static LAST_USE: Mutex<Option<Instant>> = Mutex::new(None);
static GAVE_UP: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

/// Is this port the quick brain (whose own model decides the switch)?
pub fn no_think_known(port: u16) -> bool {
    SERVER.lock().map(|g| g.as_ref().map(|s| s.port == port).unwrap_or(false)).unwrap_or(false)
}

/// Does a prompt for the quick brain need the "no thinking" switch?
pub fn no_think(port: u16) -> bool {
    QWEN3.load(Ordering::SeqCst) && no_think_known(port)
}

fn find(app: &AppHandle, rel: &[&str]) -> Option<PathBuf> {
    let mut bases = Vec::new();
    if let Ok(r) = app.path().resource_dir() {
        bases.push(r.join("resources"));
        bases.push(r);
    }
    bases.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources"));
    if let Ok(d) = app.path().app_data_dir() {
        bases.push(d);
    }
    bases.into_iter().map(|b| rel.iter().fold(b, |p, s| p.join(s))).find(|p| p.exists())
}

fn healthy(port: u16) -> bool {
    ureq::get(&format!("http://127.0.0.1:{port}/health")).timeout(Duration::from_millis(800)).call().is_ok()
}

/// Is Windows saving power right now ("Best power efficiency" mode, or battery saver)?
pub fn power_saving() -> bool {
    use windows::core::GUID;
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut st = SYSTEM_POWER_STATUS::default();
    if unsafe { GetSystemPowerStatus(&mut st) }.is_ok() && st.SystemStatusFlag == 1 {
        return true; // battery saver
    }
    const EFFICIENCY: GUID = GUID::from_u128(0x961cc777_2547_4f9d_8174_7d86181b8a7a);
    let mut g = GUID::zeroed();
    let f = crate::smooth::proc_addr("powrprof.dll", "PowerGetEffectiveOverlayScheme");
    match f {
        Some(f) => {
            let f: unsafe extern "system" fn(*mut GUID) -> u32 = unsafe { std::mem::transmute(f) };
            unsafe { f(&mut g) == 0 && g == EFFICIENCY }
        }
        None => false,
    }
}

/// Generation speed of a short warm-up answer (this is also what compiles the GPU shaders).
/// The graphics card prepares its programs for each new prompt length the first time it meets one
/// (that was the odd 10-second answer). So read prompts of every length SYLVER uses now, quietly.
fn warm_up(port: u16, gpu: bool) -> f64 {
    let url = format!("http://127.0.0.1:{port}/completion");
    let say = |n: usize, out: usize| {
        let filler = "SYLVER lives on this PC and answers questions out loud. ".repeat(n);
        serde_json::json!({
            "prompt": format!("<|im_start|>system\n{filler}<|im_end|>\n<|im_start|>user\nSay hello in one short sentence.<|im_end|>\n<|im_start|>assistant\n"),
            "n_predict": out, "temperature": 0.2, "cache_prompt": false
        })
    };
    let tps = |r: ureq::Response| r.into_json::<serde_json::Value>().ok().and_then(|v| v["timings"]["predicted_per_second"].as_f64()).unwrap_or(0.0);
    if gpu {
        // a quick test first: if the card is crawling (locked in a low-power state), don't spend a minute warming it
        let _ = ureq::post(&url).timeout(Duration::from_secs(150)).send_json(say(1, 4));
        let first = ureq::post(&url).timeout(Duration::from_secs(60)).send_json(say(1, 16)).map(tps).unwrap_or(0.0);
        if first < SLOW_TPS {
            return first;
        }
        for n in [4, 12, 30, 60, 120, 200] {
            let _ = ureq::post(&url).timeout(Duration::from_secs(150)).send_json(say(n, 4));
        }
    }
    ureq::post(&url).timeout(Duration::from_secs(90)).send_json(say(24, 24)).map(tps).unwrap_or(0.0) // the real speed
}

/// Start a quick-brain server on the card or the processor and measure it.
fn launch(app: &AppHandle, gpu: bool) -> Option<(Child, u16, f64)> {
    let exe = find(app, &["llama", "llama-server.exe"])?;
    let (file, model) = FILES.iter().find_map(|f| find(app, &["models", f]).map(|p| (*f, p)))?;
    QWEN3.store(file.starts_with("Qwen3"), Ordering::SeqCst);
    let port = std::net::TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map(|a| a.port()).ok()?;
    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let threads = (n / 2).saturating_sub(1).clamp(2, 8);
    let mut cmd = Command::new(&exe);
    cmd.current_dir(exe.parent().unwrap_or(&exe))
        .arg("-m").arg(&model)
        .args(["--host", "127.0.0.1", "--port", &port.to_string(), "-c", "4096", "-np", "1", "-t", &threads.to_string(), "--log-disable", "--no-webui", "--cache-ram", "256", "-ctxcp", "4"])
        .arg("--slot-save-path").arg(crate::ai::slot_dir())
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    if gpu {
        cmd.args(["-ngl", "99"]).creation_flags(NO_WINDOW);
    } else {
        // on the processor it yields to everything that draws the screen
        cmd.args(["-dev", "none", "-tb", &(n / 2).max(2).to_string(), "--poll", "0"]).creation_flags(NO_WINDOW | BELOW_NORMAL);
    }
    let mut child = cmd.spawn().ok()?;
    if gpu {
        crate::smooth::gpu_priority(child.id(), crate::smooth::GPU_BELOW_NORMAL); // the screen's frames go first
    }
    let t0 = Instant::now();
    while !healthy(port) {
        if child.try_wait().ok().flatten().is_some() || t0.elapsed() > Duration::from_secs(60) {
            let _ = child.kill();
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let tps = warm_up(port, gpu);
    crate::log_line(&format!("quick brain on the {} ({file}): {tps:.0} tokens/s (ready in {} s)", if gpu { "GPU" } else { "CPU" }, t0.elapsed().as_secs()));
    Some((child, port, tps))
}

/// Swap the running server for a new one, between answers (never mid-sentence).
fn install(app: &AppHandle, child: Child, port: u16, gpu: bool) {
    let pid = child.id();
    let old = crate::ai::between_answers(app, || SERVER.lock().unwrap().replace(Server { child, port, gpu }));
    crate::smooth::trim(pid); // the model file it no longer reads (it's on the card, or re-packed) goes back to Windows
    if let Some(mut o) = old {
        let _ = o.child.kill();
    }
    SLOW.store(0, Ordering::SeqCst);
    READY.store(true, Ordering::SeqCst);
    let cap = if gpu { None } else { card_cap() };
    if let Some((now, usual)) = cap {
        crate::log_line(&format!("the laptop is holding the graphics card at {now:.0} of {usual:.0} watts (a power-saving mode): quick answers run on the processor meanwhile"));
    }
    let _ = app.emit("quick-device", serde_json::json!({ "gpu": gpu, "saving": power_saving(), "cap": cap.map(|(a, b)| [a, b]) }));
    // the opening of every chat goes into its memory now, so the first question doesn't wait for it
    let a = app.clone();
    std::thread::spawn(move || crate::ai::quick_ready(&a));
}

/// Is the laptop holding the graphics card back? (the watts it's allowed now, the watts it normally gets),
/// as the card's own tool reports them. On this ASUS, "Best power efficiency" allows 25 of 55: the card's
/// memory then runs at an eighth of its speed, and no program can lift that from the inside.
pub fn card_cap() -> Option<(f64, f64)> {
    let out = Command::new("nvidia-smi").args(["--query-gpu=enforced.power.limit,power.default_limit", "--format=csv,noheader,nounits"]).creation_flags(NO_WINDOW).stdin(Stdio::null()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let mut n = text.trim().split(',').filter_map(|x| x.trim().parse::<f64>().ok());
    let (now, usual) = (n.next()?, n.next()?);
    (usual > 1.0 && now < usual * 0.8).then_some((now, usual))
}

fn on_gpu() -> bool {
    SERVER.lock().map(|g| g.as_ref().map(|s| s.gpu).unwrap_or(false)).unwrap_or(false)
}

fn card_fits() -> bool {
    crate::router::specs().vram_gb >= 3.5
}

/// Start it in the background once SYLVER is up.
/// Still getting ready (the first minute or so after SYLVER starts).
pub fn warming() -> bool {
    WARMING.load(Ordering::SeqCst)
}

pub fn start(app: AppHandle) {
    let _ = APP.set(app.clone());
    if crate::router::tier() == crate::router::Tier::Light {
        return;
    }
    WARMING.store(true, Ordering::SeqCst);
    std::thread::spawn(move || {
        {
            let _done = Done;
            std::thread::sleep(Duration::from_secs(1));
            let saving = power_saving();
            let mut placed = false;
            if card_fits() {
                if let Some((mut c, p, tps)) = launch(&app, true) {
                    if tps >= SLOW_TPS {
                        install(&app, c, p, true);
                        placed = true;
                    } else {
                        let _ = c.kill();
                        *GAVE_UP.lock().unwrap() = Some((Instant::now(), saving));
                    }
                }
            }
            if !placed {
                if let Some((c, p, _)) = launch(&app, false) {
                    install(&app, c, p, false);
                }
            }
        }
        watch(app);
    });
}

/// While it's on the processor, try the card again once Windows stops saving power
/// (or every 20 minutes, in case it was slow for another reason), and only between answers.
fn watch(app: AppHandle) {
    loop {
        std::thread::sleep(Duration::from_secs(20));
        if on_gpu() || !card_fits() || MOVING.load(Ordering::SeqCst) {
            continue;
        }
        let idle = LAST_USE.lock().unwrap().map(|t| t.elapsed() > Duration::from_secs(45)).unwrap_or(true);
        let due = match *GAVE_UP.lock().unwrap() {
            // the power state changed since it was last slow, or it's been a while
            Some((at, was_saving)) => was_saving != power_saving() || at.elapsed() > Duration::from_secs(15 * 60),
            None => true,
        };
        if !idle || !due || MOVING.swap(true, Ordering::SeqCst) {
            continue;
        }
        match launch(&app, true) {
            Some((c, p, tps)) if tps >= SLOW_TPS => install(&app, c, p, true),
            Some((mut c, ..)) => {
                let _ = c.kill();
                *GAVE_UP.lock().unwrap() = Some((Instant::now(), power_saving()));
            }
            None => *GAVE_UP.lock().unwrap() = Some((Instant::now(), power_saving())),
        }
        MOVING.store(false, Ordering::SeqCst);
    }
}

/// How fast an answer came out. Slow answers on the card send the brain to the processor.
pub fn observe(port: u16, tps: f64, tokens: usize) {
    *LAST_USE.lock().unwrap() = Some(Instant::now());
    if tokens < 10 || !on_gpu() || !no_think_known(port) {
        return;
    }
    let slow = if tps < SLOW_TPS { SLOW.fetch_add(1, Ordering::SeqCst) + 1 } else { SLOW.store(0, Ordering::SeqCst); 0 };
    // very slow once, or slow twice in a row
    if !(tps < 12.0 || slow >= 2) || MOVING.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(app) = APP.get().cloned() else {
        MOVING.store(false, Ordering::SeqCst);
        return;
    };
    crate::log_line(&format!("quick brain is slow on the GPU ({tps:.0} tokens/s{}): moving it to the CPU", if power_saving() { ", Windows is saving power" } else { "" }));
    std::thread::spawn(move || {
        if let Some((c, p, _)) = launch(&app, false) {
            install(&app, c, p, false);
            *GAVE_UP.lock().unwrap() = Some((Instant::now(), power_saving()));
        }
        MOVING.store(false, Ordering::SeqCst);
    });
}

/// Clears the "warming up" flag however the start-up ends.
struct Done;
impl Drop for Done {
    fn drop(&mut self) {
        WARMING.store(false, Ordering::SeqCst);
    }
}

/// While it's still warming up (the first minute after SYLVER starts), wait for it rather than
/// sending a simple question to the slower brain.
pub fn wait(limit: Duration) -> Option<u16> {
    let t0 = Instant::now();
    while WARMING.load(Ordering::SeqCst) && t0.elapsed() < limit && !crate::ai::cancelled() {
        if let Some(p) = port() {
            return Some(p);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    port()
}

/// Where to send a chat question, when the quick brain is up.
pub fn port() -> Option<u16> {
    if !READY.load(Ordering::SeqCst) {
        return None;
    }
    let mut g = SERVER.lock().unwrap();
    let alive = g.as_mut().map(|s| s.child.try_wait().ok().flatten().is_none()).unwrap_or(false);
    if alive {
        return g.as_ref().map(|s| s.port);
    }
    READY.store(false, Ordering::SeqCst);
    g.take();
    None
}

/// Where it runs now, for Settings: Some(true) on the card, Some(false) on the processor.
pub fn device() -> Option<bool> {
    port().map(|_| on_gpu())
}

pub fn shutdown() {
    if let Ok(mut g) = SERVER.try_lock() {
        if let Some(mut s) = g.take() {
            let _ = s.child.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn power_and_gpu_priority() {
        println!("power saving now: {}", super::power_saving());
        // lowering our own GPU priority and putting it back proves the call works
        let me = std::process::id();
        let low = crate::smooth::gpu_priority(me, crate::smooth::GPU_BELOW_NORMAL);
        let back = crate::smooth::gpu_priority(me, 2);
        println!("gpu priority calls accepted: {low} {back}");
        assert!(low && back);
    }
}
