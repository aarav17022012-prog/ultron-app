//! ULTRON's voice: Piper (a neural text-to-speech engine) with the "Ryan" voice,
//! running on this PC. The page shapes it into ULTRON's sound (a touch deeper,
//! smoother, with a faint synthetic sheen). Kept loaded between sentences so a
//! reply starts speaking quickly; unloaded after a while unused.

use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use tauri::{AppHandle, Manager};

const VOICE: &str = "en_US-ryan-high.onnx";
const NO_WINDOW: u32 = 0x0800_0000;
const BELOW_NORMAL: u32 = 0x0000_4000;
const IDLE_UNLOAD: Duration = Duration::from_secs(15 * 60);

struct Proc {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

#[derive(Default)]
pub struct Tts {
    proc: Mutex<Option<Proc>>,
    last_used: Mutex<Option<Instant>>,
}

static SEQ: AtomicU64 = AtomicU64::new(0);

fn find(app: &AppHandle, rel: &[&str]) -> Option<PathBuf> {
    let mut bases = Vec::new();
    if let Ok(r) = app.path().resource_dir() {
        bases.push(r.join("resources"));
        bases.push(r);
    }
    bases.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources"));
    bases.into_iter().map(|b| rel.iter().fold(b, |p, s| p.join(s))).find(|p| p.exists())
}

pub fn available(app: &AppHandle) -> bool {
    find(app, &["piper", "piper.exe"]).is_some() && find(app, &["voices", VOICE]).is_some()
}

fn start(app: &AppHandle) -> Option<Proc> {
    let exe = find(app, &["piper", "piper.exe"])?;
    let model = find(app, &["voices", VOICE])?;
    let mut child = Command::new(&exe)
        .current_dir(exe.parent()?)
        .args(["--model"])
        .arg(&model)
        .args(["--json-input", "--length_scale", "0.93", "--noise_scale", "0.62", "--noise_w", "0.82", "--sentence_silence", "0.18"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(NO_WINDOW | BELOW_NORMAL)
        .spawn()
        .ok()?;
    let stdin = child.stdin.take()?;
    let stdout = BufReader::new(child.stdout.take()?);
    Some(Proc { child, stdin, stdout })
}

/// Words the voice would otherwise trip over.
fn tidy(text: &str) -> String {
    let mut t: String = text
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace() || ".,!?;:'\"()-–—%$₹&/+".contains(*c))
        .collect();
    for (a, b) in [("ULTRON", "Ultron"), ("Ultron's", "Ultron's"), ("e.g.", "for example"), ("i.e.", "that is"), (" & ", " and "), ("₹", " rupees "), ("—", ", "), ("–", ", ")] {
        t = t.replace(a, b);
    }
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Speak one sentence; returns it as a WAV file (base64) for the page to play.
pub fn speak(app: &AppHandle, text: &str) -> Option<String> {
    let text = tidy(text);
    if text.trim().is_empty() {
        return None;
    }
    let tts = app.state::<Tts>();
    let mut guard = tts.proc.lock().unwrap();
    if guard.as_mut().map(|p| p.child.try_wait().ok().flatten().is_some()).unwrap_or(true) {
        *guard = start(app);
    }
    let p = guard.as_mut()?;
    let out = std::env::temp_dir().join(format!("ultron-voice-{}-{}.wav", std::process::id(), SEQ.fetch_add(1, Ordering::SeqCst)));
    let req = serde_json::json!({ "text": text, "output_file": out.to_string_lossy() });
    if writeln!(p.stdin, "{req}").and_then(|_| p.stdin.flush()).is_err() {
        *guard = None;
        return None;
    }
    // Piper prints the file's path once it's written
    let want = out.to_string_lossy().to_string();
    loop {
        let mut line = String::new();
        match p.stdout.read_line(&mut line) {
            Ok(0) | Err(_) => {
                *guard = None;
                return None;
            }
            Ok(_) if line.trim() == want => break,
            Ok(_) => {}
        }
    }
    drop(guard);
    *tts.last_used.lock().unwrap() = Some(Instant::now());
    let bytes = std::fs::read(&out).ok()?;
    let _ = std::fs::remove_file(&out);
    Some(base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// Load the voice ahead of time (when you open ULTRON or start talking).
pub fn warm(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let tts = app.state::<Tts>();
        let mut g = tts.proc.lock().unwrap();
        if g.is_none() && available(&app) {
            *g = start(&app);
            *tts.last_used.lock().unwrap() = Some(Instant::now());
        }
    });
}

/// Free the memory when ULTRON hasn't spoken for a while.
pub fn watch(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(60));
        let tts = app.state::<Tts>();
        let idle = tts.last_used.lock().unwrap().map(|t| t.elapsed() > IDLE_UNLOAD).unwrap_or(false);
        if idle {
            if let Ok(mut g) = tts.proc.try_lock() {
                if let Some(mut p) = g.take() {
                    let _ = p.child.kill();
                }
                *tts.last_used.lock().unwrap() = None;
            }
        }
    });
}
