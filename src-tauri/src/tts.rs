//! ULTRON's voice: Piper (a neural text-to-speech engine), running on this PC, with one of two voices:
//! "Lessac", a woman's voice (the one it starts with), or "Ryan", a man's. The page can shape either into
//! ULTRON's own sound (deeper, with a faint synthetic sheen) or leave it natural. Kept loaded between
//! sentences so a reply starts speaking quickly; unloaded after a while unused.

use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use tauri::{AppHandle, Manager};

/// The voices there are: name, file, and how it's spoken (speed, variation, rhythm, the pause after a sentence).
const VOICES: [(&str, &str, [&str; 4]); 2] = [
    ("lessac", "en_US-lessac-high.onnx", ["1.0", "0.667", "0.8", "0.22"]),
    ("ryan", "en_US-ryan-high.onnx", ["0.93", "0.62", "0.82", "0.18"]),
];
/// Piper's own voices for the other languages ULTRON can be talked to in, fetched the first time one is chosen (about
/// 60 MB each, kept with ULTRON's data). A language with none here is answered in writing only.
const TONGUES: [(&str, &str); 11] = [
    ("hi", "hi/hi_IN/pratham/medium/hi_IN-pratham-medium"),
    ("es", "es/es_ES/davefx/medium/es_ES-davefx-medium"),
    ("fr", "fr/fr_FR/siwis/medium/fr_FR-siwis-medium"),
    ("de", "de/de_DE/thorsten/medium/de_DE-thorsten-medium"),
    ("pt", "pt/pt_BR/faber/medium/pt_BR-faber-medium"),
    ("it", "it/it_IT/paola/medium/it_IT-paola-medium"),
    ("ar", "ar/ar_JO/kareem/medium/ar_JO-kareem-medium"),
    ("ru", "ru/ru_RU/irina/medium/ru_RU-irina-medium"),
    ("zh", "zh/zh_CN/huayan/medium/zh_CN-huayan-medium"),
    ("tr", "tr/tr_TR/dfki/medium/tr_TR-dfki-medium"),
    ("id", "id/id_ID/news_tts/medium/id_ID-news_tts-medium"),
];
/// The language spoken in ("" or "en": English, the voices above)
static TONGUE: Mutex<String> = Mutex::new(String::new());

fn tongue_file(app: &AppHandle, lang: &str) -> Option<PathBuf> {
    let (_, path) = TONGUES.iter().find(|t| t.0 == lang)?;
    Some(app.path().app_data_dir().ok()?.join("voices").join(format!("{}.onnx", path.rsplit('/').next()?)))
}

/// Speak in another language from now on: its voice is fetched if it isn't here yet.
pub fn set_tongue(app: &AppHandle, lang: &str) {
    let lang = if lang == "auto" { "en" } else { lang };
    {
        let mut cur = TONGUE.lock().unwrap();
        if *cur == lang {
            return;
        }
        *cur = lang.to_string();
    }
    if let Some(mut p) = app.state::<Tts>().proc.lock().unwrap().take() {
        let _ = p.child.kill();
    }
    let Some(file) = tongue_file(app, lang) else { return };
    if file.exists() {
        return;
    }
    let path = TONGUES.iter().find(|t| t.0 == lang).map(|t| t.1).unwrap_or_default();
    std::thread::spawn(move || {
        let _ = std::fs::create_dir_all(file.parent().unwrap());
        for ext in [".json", ""] {
            let url = format!("https://huggingface.co/rhasspy/piper-voices/resolve/main/{path}.onnx{ext}");
            let to = PathBuf::from(format!("{}{ext}", file.display()));
            let part = to.with_extension("part");
            let got = ureq::get(&url).timeout(Duration::from_secs(30 * 60)).call().map_err(|e| e.to_string()).and_then(|r| {
                let mut f = std::fs::File::create(&part).map_err(|e| e.to_string())?;
                std::io::copy(&mut r.into_reader(), &mut f).map_err(|e| e.to_string())
            });
            match got {
                Ok(n) if n > 100 => {
                    let _ = std::fs::rename(&part, &to);
                }
                other => {
                    let _ = std::fs::remove_file(&part);
                    crate::log_line(&format!("voice for another language: couldn't get {url} ({other:?})"));
                    return;
                }
            }
        }
        crate::log_line(&format!("voice for another language: {} is here", file.display()));
    });
}

/// Which one is speaking (Settings, Voice).
static VOICE: Mutex<&'static str> = Mutex::new("lessac");

fn chosen() -> &'static (&'static str, &'static str, [&'static str; 4]) {
    let v = *VOICE.lock().unwrap();
    VOICES.iter().find(|x| x.0 == v).unwrap_or(&VOICES[0])
}
const NO_WINDOW: u32 = 0x0000_0008; // DETACHED_PROCESS: no console (and no Console Window Host beside it)
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

/// Change the voice: the one that's loaded is let go, and the next sentence loads the new one.
pub fn set_voice(app: &AppHandle, name: &str) {
    let Some(v) = VOICES.iter().find(|x| x.0 == name) else { return };
    let mut cur = VOICE.lock().unwrap();
    if *cur == v.0 {
        return;
    }
    *cur = v.0;
    drop(cur);
    if let Some(mut p) = app.state::<Tts>().proc.lock().unwrap().take() {
        let _ = p.child.kill();
    }
}

fn start(app: &AppHandle) -> Option<Proc> {
    let exe = find(app, &["piper", "piper.exe"])?;
    let (_, file, how) = chosen();
    // (another language: its own voice, or none until it has come; English words read out in the wrong voice were worse
    //  than silence)
    let lang = TONGUE.lock().unwrap().clone();
    let (model, how) = if !lang.is_empty() && lang != "en" {
        let f = tongue_file(app, &lang).filter(|f| f.exists() && PathBuf::from(format!("{}.json", f.display())).exists())?;
        (f, &["1.0", "0.667", "0.8", "0.2"])
    } else {
        // (if the chosen voice isn't on this PC, the other one speaks)
        (find(app, &["voices", file]).or_else(|| VOICES.iter().find_map(|v| find(app, &["voices", v.1])))?, how)
    };
    let mut child = Command::new(&exe)
        .current_dir(exe.parent()?)
        .args(["--model"])
        .arg(&model)
        .args(["--json-input", "--length_scale", how[0], "--noise_scale", how[1], "--noise_w", how[2], "--sentence_silence", how[3]])
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
        // (and the marks other scripts are written with: Hindi's vowel signs, Arabic's, CJK's full stops; not emoji)
        .filter(|c| c.is_alphanumeric() || c.is_whitespace() || ".,!?;:'\"()-–—%$₹&/+".contains(*c) || ('\u{300}'..'\u{2000}').contains(c) || ('\u{3000}'..'\u{3040}').contains(c))
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
