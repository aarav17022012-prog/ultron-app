//! Local speech recognition: microphone -> clean resampling -> neural voice
//! detection (RNNoise) -> Whisper small.en (whisper.cpp), all on this PC. (large-v3-turbo was
//! tried: ~5x slower on a laptop CPU for no real gain on short commands.)
//! No API key; nothing you say leaves the machine.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

const MODEL_FILE: &str = "ggml-small.en-q5_1.bin";
const MODEL_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.en-q5_1.bin";
/// The same size of model, for every other language (fetched the first time one is chosen, about 190 MB)
const MULTI_FILE: &str = "ggml-small-q5_1.bin";
const MULTI_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small-q5_1.bin";
/// For dictation, when "Most accurate dictation" is on: Whisper large-v3 turbo (about 550 MB, fetched once when it's
/// turned on). Held only while you dictate, and put away a minute after.
pub const BEST_FILE: &str = "ggml-large-v3-turbo-q5_0.bin";
const BEST_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin";
const RATE: usize = 16_000;
const FRAME: usize = 320; // 20 ms at 16 kHz
const NN: usize = nnnoiseless::DenoiseState::FRAME_SIZE; // 10 ms at 48 kHz

#[derive(Serialize, Clone, Default)]
pub struct VoiceStatus {
    pub state: String, // downloading | loading | listening | hearing | transcribing | paused | error
    pub detail: String,
    pub progress: f64,
}

pub struct Voice {
    paused: AtomicBool,
    ready: AtomicBool,
    /// Only speech that starts with "Sylver" would be acted on just now (the page says so).
    pub wake: AtomicBool,
    /// Dictating: a pause has to be twice as long before it ends what you're saying.
    pub patient: AtomicBool,
    /// You've tapped the notch's mic: what you say is shown as you say it, heard in one quick pass, and a shorter
    /// pause ends it.
    pub live: AtomicBool,
    sens: AtomicU32, // f32 bits
    prompt: Mutex<String>,
    /// Your names and words (learned and listed), for Windows' recognizer listening under dictation (winsr.rs)
    pub names: Mutex<Vec<String>>,
    /// The language you talk in ("en", "hi", ..., or "auto"), and that it has just changed (the model is swapped)
    pub lang: Mutex<String>,
    pub reload: AtomicBool,
    /// Dictation uses the most accurate model (BEST_FILE) when it's here
    pub best: AtomicBool,
    status: Mutex<VoiceStatus>,
    seg_tx: Mutex<Option<SyncSender<Vec<f32>>>>,
    /// (developer mode: a recording fed in as if the microphone heard it, voice::feed)
    raw_tx: Mutex<Option<SyncSender<(Vec<f32>, u32)>>>,
    /// Dictating: what's been said so far of what's still being said, to be heard quickly ("heard-partial")
    part_tx: Mutex<Option<SyncSender<Vec<f32>>>>,
}

impl Default for Voice {
    fn default() -> Self {
        Voice {
            paused: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            wake: AtomicBool::new(false),
            patient: AtomicBool::new(false),
            names: Mutex::new(Vec::new()),
            live: AtomicBool::new(false),
            sens: AtomicU32::new(1.0f32.to_bits()),
            prompt: Mutex::new(String::new()),
            lang: Mutex::new("en".into()),
            reload: AtomicBool::new(false),
            best: AtomicBool::new(false),
            status: Mutex::new(VoiceStatus { state: "loading".into(), ..Default::default() }),
            seg_tx: Mutex::new(None),
            raw_tx: Mutex::new(None),
            part_tx: Mutex::new(None),
        }
    }
}

impl Voice {
    pub fn set_paused(&self, p: bool) {
        self.paused.store(p, Ordering::SeqCst);
    }
    pub fn set_sens(&self, s: f32) {
        self.sens.store(s.clamp(0.2, 2.0).to_bits(), Ordering::SeqCst);
    }
    pub fn set_prompt(&self, p: String) {
        *self.prompt.lock().unwrap() = p;
    }
    pub fn status(&self) -> VoiceStatus {
        self.status.lock().unwrap().clone()
    }
}

fn set_status(app: &AppHandle, state: &str, detail: &str, progress: f64) {
    let st = VoiceStatus { state: state.into(), detail: detail.into(), progress };
    *app.state::<Voice>().status.lock().unwrap() = st.clone();
    let _ = app.emit_to("main", "voice", st);
}

/// mic-driven states only show once the model is ready (they'd hide download progress)
fn vad_status(app: &AppHandle, state: &str) {
    if app.state::<Voice>().ready.load(Ordering::SeqCst) {
        set_status(app, state, "", 0.0);
    }
}

/* ---------------- model ---------------- */

fn data_dir(app: &AppHandle) -> PathBuf {
    let dir = app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir()).join("models");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// The model ships inside the app; otherwise a downloaded copy.
fn bundled(app: &AppHandle, file: &str) -> Option<PathBuf> {
    let mut c = Vec::new();
    if let Ok(r) = app.path().resource_dir() {
        c.push(r.join("resources").join("models").join(file));
        c.push(r.join("models").join(file));
    }
    c.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources").join("models").join(file));
    c.push(data_dir(app).join(file));
    c.into_iter().find(|p| p.exists())
}

fn download(app: &AppHandle, path: &PathBuf, url: &str) -> Result<(), String> {
    let part = path.with_extension("part");
    let resp = ureq::get(url).timeout(Duration::from_secs(60 * 60)).call().map_err(|e| e.to_string())?;
    let total: f64 = resp.header("Content-Length").and_then(|v| v.parse().ok()).unwrap_or(190_000_000.0);
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(&part).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 1 << 16];
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
            set_status(app, "downloading", "Getting the voice model (one time)", done / total);
        }
    }
    drop(file);
    if done < 50_000_000.0 {
        let _ = std::fs::remove_file(&part);
        return Err("download incomplete".into());
    }
    std::fs::rename(&part, path).map_err(|e| e.to_string())
}

/// The most accurate model's file (None: not fetched yet)
pub fn best_path(app: &AppHandle) -> Option<PathBuf> {
    Some(data_dir(app).join(BEST_FILE)).filter(|p| p.exists())
}
/// Fetched in the background, the first time it's turned on
pub fn fetch_best(app: &AppHandle) {
    if best_path(app).is_some() {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let target = data_dir(&app).join(BEST_FILE);
        match download(&app, &target, BEST_URL) {
            Ok(()) => crate::log_line("voice: the most accurate dictation model is ready"),
            Err(e) => crate::log_line(&format!("voice: couldn't fetch the most accurate model ({e})")),
        }
    });
}

fn model_path(app: &AppHandle, lang: &str) -> Result<PathBuf, String> {
    let (file, url) = if lang == "en" { (MODEL_FILE, MODEL_URL) } else { (MULTI_FILE, MULTI_URL) };
    if let Some(p) = bundled(app, file) {
        return Ok(p);
    }
    let target = data_dir(app).join(file);
    download(app, &target, url).map(|_| target)
}

/* ---------------- audio helpers ---------------- */

/// Band-limited resampler: a windowed-sinc low-pass (no aliasing smearing the
/// consonants, which the old one-pole filter let through), then interpolation.
pub struct Resampler {
    rate: u32,
    to: u32,
    taps: Vec<f32>,
    hist: Vec<f32>,
    at: usize,
    pos: f64,
    prev: f32,
}

impl Resampler {
    pub fn new(to: u32) -> Self {
        Resampler { rate: 0, to, taps: Vec::new(), hist: Vec::new(), at: 0, pos: 0.0, prev: 0.0 }
    }

    fn build(&mut self, rate: u32) {
        const N: usize = 48;
        let fc = 0.46 * self.to.min(rate) as f64 / rate as f64; // cycles per input sample
        let mut taps: Vec<f32> = (0..N)
            .map(|i| {
                let m = i as f64 - (N - 1) as f64 / 2.0;
                let sinc = if m == 0.0 { 2.0 * fc } else { (2.0 * std::f64::consts::PI * fc * m).sin() / (std::f64::consts::PI * m) };
                let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (N - 1) as f64).cos()
                    + 0.08 * (4.0 * std::f64::consts::PI * i as f64 / (N - 1) as f64).cos(); // Blackman
                (sinc * w) as f32
            })
            .collect();
        let sum: f32 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= sum);
        self.taps = taps;
        self.hist = vec![0.0; N];
        self.at = 0;
        self.pos = 0.0;
        self.prev = 0.0;
        self.rate = rate;
    }

    pub fn push(&mut self, x: &[f32], rate: u32, out: &mut Vec<f32>) {
        if rate == self.to {
            out.extend_from_slice(x);
            return;
        }
        if rate != self.rate {
            self.build(rate);
        }
        let step = rate as f64 / self.to as f64;
        let n = self.taps.len();
        for &s in x {
            self.hist[self.at] = s;
            self.at = (self.at + 1) % n;
            let mut y = 0.0f32;
            for (k, t) in self.taps.iter().enumerate() {
                y += t * self.hist[(self.at + k) % n];
            }
            while self.pos <= 1.0 {
                out.push(self.prev + (y - self.prev) * self.pos as f32);
                self.pos += step;
            }
            self.pos -= 1.0;
            self.prev = y;
        }
    }
}

/* ---------------- whisper ---------------- */

/// The recognizer now and then writes a short phrase out twice ("How are you? How are you?"): what was said once is
/// kept once (asked twice of SYLVER, and typed twice when dictating). Two words or more repeated whole; a word said
/// over and over ("no, no, no") is left as it was.
fn unrepeat(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    let key = |w: &str| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
    let n = words.len();
    for k in 2..=n / 2 {
        if n % k == 0 && (k..n).all(|i| key(words[i]) == key(words[i % k])) {
            return words[..k].join(" ");
        }
    }
    text.to_string()
}

fn is_noise(text: &str) -> bool {
    let t = text.trim().trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
    if t.is_empty() || text.trim().starts_with('[') || text.trim().starts_with('(') || text.trim().starts_with('*') {
        return true;
    }
    const JUNK: [&str; 13] = [
        "you", "thank you", "thanks for watching", "thank you for watching", "bye", "okay", "so", "uh", "um", "hmm",
        "please subscribe", "the end", "thank you very much",
    ];
    JUNK.contains(&t.as_str())
}

/// Clean up a recorded utterance for Whisper: remove rumble, even out the level, pad with quiet.
pub fn prepare(seg: &[f32]) -> Vec<f32> {
    // 80 Hz high-pass (fans, desk knocks, mic rumble)
    let a = (-2.0 * std::f32::consts::PI * 80.0 / RATE as f32).exp();
    let (mut px, mut py) = (0f32, 0f32);
    let hp: Vec<f32> = seg
        .iter()
        .map(|&x| {
            py = a * (py + x - px);
            px = x;
            py
        })
        .collect();
    let rms = (hp.iter().map(|s| s * s).sum::<f32>() / hp.len().max(1) as f32).sqrt().max(1e-5);
    let peak = hp.iter().fold(0f32, |m, s| m.max(s.abs())).max(1e-5);
    let gain = (0.1 / rms).min(0.95 / peak).clamp(0.5, 20.0);
    let mut audio = vec![0.0f32; RATE / 5];
    audio.extend(hp.iter().map(|s| s * gain));
    audio.extend(std::iter::repeat(0.0).take(RATE / 5));
    if audio.len() < RATE + RATE / 10 {
        audio.resize(RATE + RATE / 10, 0.0);
    }
    audio
}

/// Decoding settings: a small beam search (more careful than greedy) and an
/// encoder window sized to the clip (far faster than always reading 30 s).
pub fn params<'a>(prompt: &'a str, lang: &'a str, threads: i32, samples: usize) -> FullParams<'a, 'a> {
    shaped(FullParams::new(SamplingStrategy::BeamSearch { beam_size: 4, patience: -1.0 }), prompt, lang, threads, samples)
}

/// Two guesses kept as it goes (dictation's finished pieces): a little slower than one pass, and names and odd words
/// come out right far more often.
fn steady_params<'a>(prompt: &'a str, lang: &'a str, threads: i32, samples: usize) -> FullParams<'a, 'a> {
    shaped(FullParams::new(SamplingStrategy::BeamSearch { beam_size: 2, patience: -1.0 }), prompt, lang, threads, samples)
}

/// The same, decoded in one pass (the notch's mic, and long stretches of dictation).
fn quick_params<'a>(prompt: &'a str, lang: &'a str, threads: i32, samples: usize) -> FullParams<'a, 'a> {
    shaped(FullParams::new(SamplingStrategy::Greedy { best_of: 1 }), prompt, lang, threads, samples)
}

fn shaped<'a>(mut p: FullParams<'a, 'a>, prompt: &'a str, lang: &'a str, threads: i32, samples: usize) -> FullParams<'a, 'a> {
    p.set_language(Some(lang));
    p.set_n_threads(threads);
    p.set_translate(false);
    p.set_no_context(true);
    p.set_single_segment(true);
    p.set_no_timestamps(true);
    p.set_print_special(false);
    p.set_print_progress(false);
    p.set_print_realtime(false);
    p.set_print_timestamps(false);
    p.set_suppress_blank(true);
    p.set_suppress_nst(true);
    let secs = samples as f64 / RATE as f64;
    let ctx = (((secs * 50.0 + 128.0) / 64.0).ceil() * 64.0).min(1500.0) as i32;
    p.set_audio_ctx(ctx);
    if !prompt.is_empty() {
        p.set_initial_prompt(prompt);
    }
    p
}

/// "Sylver", or one of the ways the recognizer writes it, somewhere in this.
fn has_wake(text: &str) -> bool {
    let l = text.to_lowercase();
    let squashed: String = l.chars().filter(|c| c.is_alphanumeric()).collect();
    // (and the ways it's misheard: "Aertron", "A-Chron", "Altron": a short word starting with a vowel, ending in "tron" or "chron")
    let tronish = |w: &str| w.len() <= 7 && w.starts_with(['a', 'e', 'o', 'u']) && (w.ends_with("tron") || w.ends_with("chron"));
    squashed.contains("sylver") || squashed.contains("silver") || squashed.contains("ultron")
        || l.split(|c: char| !c.is_alphanumeric()).any(|w| ["ultra", "altron", "ultran", "ultrum", "ultrom", "alltron"].contains(&w) || tronish(w))
        || l.split_whitespace().map(|w| w.chars().filter(|c| c.is_alphanumeric()).collect::<String>()).any(|w| tronish(&w))
        // (written in the language you talk in)
        || ["ultrón", "ультрон", "альтрон", "अल्ट्रॉन", "अल्ट्रोन", "अल्ट्रान", "उल्ट्रॉन", "ウルトロン", "ألترون", "الترون", "울트론", "奥创", "奧創"].iter().any(|w| l.contains(w))
}

/// What's been said so far, heard quickly (one pass, no second guesses) while you're still saying it.
fn partial(state: &mut whisper_rs::WhisperState, audio: &[f32], prompt: &str, lang: &str, threads: i32) -> String {
    let audio = prepare(audio);
    // (what's been said so far, heard as it grows: past about 7 s the one quick pass turned a whole sentence into its
    //  last word, so a longer stretch is heard the steady way)
    let mut p = if audio.len() > RATE * 7 { FullParams::new(SamplingStrategy::BeamSearch { beam_size: 2, patience: -1.0 }) } else { FullParams::new(SamplingStrategy::Greedy { best_of: 1 }) };
    p.set_language(Some(lang));
    p.set_n_threads(threads);
    p.set_no_context(true);
    p.set_single_segment(true);
    p.set_no_timestamps(true);
    p.set_print_special(false);
    p.set_print_progress(false);
    p.set_print_realtime(false);
    p.set_print_timestamps(false);
    p.set_suppress_blank(true);
    p.set_suppress_nst(true);
    let secs = audio.len() as f64 / RATE as f64;
    p.set_audio_ctx((((secs * 50.0 + 128.0) / 64.0).ceil() * 64.0).min(1500.0) as i32);
    if !prompt.is_empty() {
        p.set_initial_prompt(prompt);
    }
    let mut text = String::new();
    if state.full(p, &audio).is_ok() {
        for s in state.as_iter() {
            if let Ok(t) = s.to_str_lossy() {
                text.push_str(&t);
            }
        }
    }
    unrepeat(text.trim())
}

fn transcriber(app: AppHandle, rx: Receiver<Vec<f32>>, parts: Receiver<Vec<f32>>) {
    // (a language whose model can't be had yet: English, until it can)
    let lang0 = app.state::<Voice>().lang.lock().unwrap().clone();
    let mut path = match model_path(&app, &lang0).or_else(|_| model_path(&app, "en")) {
        Ok(p) => p,
        Err(e) => {
            set_status(&app, "error", &format!("Couldn't get the voice model: {e}"), 0.0);
            return;
        }
    };
    set_status(&app, "loading", "Warming up", 0.0);
    whisper_rs::install_logging_hooks();
    let ctx = match WhisperContext::new_with_params(path.to_string_lossy().as_ref(), WhisperContextParameters::default()) {
        Ok(c) => c,
        Err(e) => {
            if !path.starts_with(env!("CARGO_MANIFEST_DIR")) && path.starts_with(data_dir(&app)) {
                let _ = std::fs::remove_file(&path); // most likely a corrupt download; fetch again next launch
            }
            set_status(&app, "error", &format!("Voice model failed to load: {e:?}"), 0.0);
            return;
        }
    };
    let Ok(state) = ctx.create_state() else {
        set_status(&app, "error", "Voice engine failed to start", 0.0);
        return;
    };
    drop(ctx); // (the state keeps the model; dropping the state lets it go)
    let threads = (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / 2).clamp(2, 8) as i32;
    let voice = app.state::<Voice>();
    voice.ready.store(true, Ordering::SeqCst);
    set_status(&app, if voice.paused.load(Ordering::SeqCst) { "paused" } else { "listening" }, "", 0.0);

    // The speech model is about 350 MB once loaded, and in a quiet room it sits unused most of the day. After four
    // minutes with nothing heard it's put away, and the next thing heard brings it back first (a fraction of a
    // second: the file is still in Windows' cache). While something with words in it plays, it never goes quiet.
    const QUIET: std::time::Duration = std::time::Duration::from_secs(120);
    let mut held = Some(state);
    let mut last = std::time::Instant::now();
    // (the most accurate model, while dictating with it on)
    let mut best: Option<whisper_rs::WhisperState> = None;
    let mut best_at = std::time::Instant::now();
    // (loaded on a thread of its own: five seconds of loading held up the first thing dictated)
    let mut best_rx: Option<Receiver<Option<whisper_rs::WhisperState>>> = None;
    let mut wake_at = std::time::Instant::now() - std::time::Duration::from_secs(10);
    loop {
        // another language chosen: its model is fetched if need be, and used from the next thing heard
        if voice.reload.swap(false, Ordering::SeqCst) {
            let lang = voice.lang.lock().unwrap().clone();
            match model_path(&app, &lang) {
                Ok(p) => {
                    if p != path {
                        held = None;
                        path = p;
                    }
                    crate::log_line(&format!("voice: listening in {lang}"));
                }
                Err(e) => crate::log_line(&format!("voice: no model for {lang} ({e})")),
            }
            set_status(&app, if voice.paused.load(Ordering::SeqCst) { "paused" } else { "listening" }, "", 0.0);
        }
        // (the mic tapped: the speech model is got ready now, not when the first words are in)
        if held.is_none() && voice.live.load(Ordering::SeqCst) {
            held = WhisperContext::new_with_params(path.to_string_lossy().as_ref(), WhisperContextParameters::default()).ok().and_then(|c| c.create_state().ok());
            last = std::time::Instant::now();
        }
        if let Some(s) = best_rx.as_ref().and_then(|r| r.try_recv().ok()) {
            best = s;
            best_rx = None;
            best_at = std::time::Instant::now();
        }
        // (what's finished comes first; meanwhile, while dictating, what's still being said is heard as it grows)
        let got = match rx.try_recv() {
            Ok(s) => Ok(s),
            Err(_) => match parts.try_recv() {
                Ok(so_far) => {
                    if voice.patient.load(Ordering::SeqCst) || voice.live.load(Ordering::SeqCst) {
                        if held.is_none() {
                            held = WhisperContext::new_with_params(path.to_string_lossy().as_ref(), WhisperContextParameters::default()).ok().and_then(|c| c.create_state().ok());
                        }
                        if let Some(state) = held.as_mut() {
                            let prompt = voice.prompt.lock().unwrap().clone();
                            let lang = voice.lang.lock().unwrap().clone();
                            let text = partial(state, &so_far, &prompt, &lang, threads);
                            if !text.is_empty() && !is_noise(&text) {
                                let _ = app.emit_to("main", "heard-partial", &text);
                            }
                        }
                        last = std::time::Instant::now();
                    }
                    continue;
                }
                Err(_) => rx.recv_timeout(std::time::Duration::from_millis(60)),
            },
        };
        let seg = match got {
            Ok(s) => s,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let want_best = voice.patient.load(Ordering::SeqCst) && voice.best.load(Ordering::SeqCst) && *voice.lang.lock().unwrap() == "en";
                if want_best && best.is_none() && best_rx.is_none() {
                    if let Some(f) = best_path(&app) {
                        let (tx, rx) = std::sync::mpsc::channel();
                        best_rx = Some(rx);
                        std::thread::spawn(move || {
                            let t0 = std::time::Instant::now();
                            let s = WhisperContext::new_with_params(f.to_string_lossy().as_ref(), WhisperContextParameters::default()).ok().and_then(|c| c.create_state().ok());
                            crate::log_line(&format!("voice: the most accurate dictation model up in {} ms", t0.elapsed().as_millis()));
                            let _ = tx.send(s);
                        });
                    }
                }
                if best.is_some() && !want_best && best_at.elapsed() > std::time::Duration::from_secs(60) {
                    best = None;
                    // (its memory goes with it; the process itself isn't emptied: the keyboard and mouse hooks live here, and read back
                    //  from the disk they held up every key and click on the PC)
                    crate::log_line("voice: the most accurate dictation model put away");
                }
                if held.is_some() && last.elapsed() > QUIET {
                    held = None;
                    // (its memory goes with it; the process itself isn't emptied: the keyboard and mouse hooks live here, and read back
                    //  from the disk they held up every key and click on the PC)
                    crate::log_line("voice: speech model put away (nothing heard for 4 minutes)");
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        last = std::time::Instant::now();
        if held.is_none() {
            let t0 = std::time::Instant::now();
            held = WhisperContext::new_with_params(path.to_string_lossy().as_ref(), WhisperContextParameters::default()).ok().and_then(|c| c.create_state().ok());
            crate::log_line(&format!("voice: speech model back in {} ms", t0.elapsed().as_millis()));
        }
        let Some(state) = held.as_mut() else { continue };
        let music =app.state::<crate::media::Media>().state.lock().unwrap().sessions.iter().any(|s| s.playing);
        // A song or a video with words in it is "speech" for as long as it plays, and each stretch was transcribed
        // carefully (a second or two on half the processor's cores) to find that it wasn't for SYLVER: over a core,
        // all the while. Nearly all of that time goes into listening to the clip, and it grows with the clip's length.
        // So while something plays and only "Sylver ..." would be acted on, just the first second and a half (where
        // the name has to be) is heard first, the quick way; the careful pass is kept for when the name is there.
        let prompt = voice.prompt.lock().unwrap().clone();
        let lang = voice.lang.lock().unwrap().clone();
        // (only when nothing is waiting for plain speech: dictating, or after a tap on the notch's mic, everything said counts)
        let music = music && !voice.patient.load(Ordering::SeqCst) && !voice.live.load(Ordering::SeqCst);
        // (a song is "speech" all the while it plays: each look for the name took about a second of half the
        //  processor. Now at most one every four seconds, and never for a scrap shorter than a second)
        if music && voice.wake.load(Ordering::SeqCst) && (seg.len() < RATE || wake_at.elapsed() < std::time::Duration::from_secs(4)) {
            continue;
        }
        if music && voice.wake.load(Ordering::SeqCst) {
            wake_at = std::time::Instant::now();
            let t0 = std::time::Instant::now();
            let head = prepare(&seg[..seg.len().min(RATE * 8 / 5)]);
            let mut q = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            q.set_language(Some(&lang));
            q.set_n_threads(threads.min(4));
            q.set_no_context(true);
            q.set_single_segment(true);
            q.set_no_timestamps(true);
            q.set_print_special(false);
            q.set_print_progress(false);
            q.set_print_realtime(false);
            q.set_print_timestamps(false);
            q.set_audio_ctx(128); // (two and a half seconds' worth: the clip, its padding, and a little over)
            if !prompt.is_empty() {
                q.set_initial_prompt(&prompt); // (the words it's told to expect, "Sylver" first among them)
            }
            let mut start = String::new();
            if state.full(q, &head).is_ok() {
                for s in state.as_iter() {
                    if let Ok(t) = s.to_str_lossy() {
                        start.push_str(&t);
                    }
                }
            }
            if !has_wake(&start) {
                crate::log_line(&format!("heard {:.1}s while a song played: no \"Sylver\" at its start (checked in {} ms)", seg.len() as f32 / RATE as f32, t0.elapsed().as_millis()));
                set_status(&app, if voice.paused.load(Ordering::SeqCst) { "paused" } else { "listening" }, "", 0.0);
                continue;
            }
        }
        // a long unbroken stretch while music plays is the song coming through the mic, not you:
        // only its start is transcribed, quickly, instead of all of it
        let seg = if music && seg.len() > RATE * 9 { seg[..RATE * 5].to_vec() } else { seg };
        let wake_only = music && voice.wake.load(Ordering::SeqCst) && seg.len() == RATE * 5;
        set_status(&app, "transcribing", "", 0.0);
        let audio = prepare(&seg);
        let mut p = params(&prompt, &lang, threads, audio.len());
        // (dictating: one quick pass, two or three times faster; what you said already showed as you said it)
        // (the one quick pass only for a short piece from the notch's mic: on more than about 7 s it lost all but the
        //  last word. 9.8 s of dictation came out as "Good." where the steady pass heard every word, in the same 2 s)
        if voice.live.load(Ordering::SeqCst) && audio.len() <= RATE * 7 {
            p = quick_params(&prompt, &lang, threads, audio.len());
        } else if voice.patient.load(Ordering::SeqCst) || voice.live.load(Ordering::SeqCst) {
            p = steady_params(&prompt, &lang, threads, audio.len());
        }
        if wake_only {
            p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            p.set_language(Some(&lang));
            p.set_n_threads(threads);
            p.set_no_context(true);
            p.set_single_segment(true);
            p.set_no_timestamps(true);
            p.set_audio_ctx(384);
        }
        // (dictating in English: Windows' own recognizer hears it too, at the same time, winsr.rs)
        let win = (voice.patient.load(Ordering::SeqCst) && !voice.live.load(Ordering::SeqCst) && !wake_only && lang == "en" && audio.len() < RATE * 30).then(|| {
            let (a, names) = (audio.clone(), voice.names.lock().unwrap().clone());
            std::thread::spawn(move || crate::winsr::hear(&a, &names))
        });
        let mut text = String::new();
        let mut no_speech = 0f32;
        let t0 = std::time::Instant::now();
        if state.full(p, &audio).is_ok() {
            for s in state.as_iter() {
                no_speech = no_speech.max(s.no_speech_probability());
                if let Ok(t) = s.to_str_lossy() {
                    text.push_str(&t);
                }
            }
        }
        let text = unrepeat(text.trim());
        crate::log_line(&format!("heard {:.1}s in {} ms: {text:?} (no-speech {no_speech:.2})", seg.len() as f32 / RATE as f32, t0.elapsed().as_millis()));
        let back = if voice.paused.load(Ordering::SeqCst) { "paused" } else { "listening" };
        // (Windows' ears, waited on a moment at most once Whisper's done)
        let win = win.and_then(|h| {
            let t = std::time::Instant::now();
            while !h.is_finished() && t.elapsed() < std::time::Duration::from_millis(700) {
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
            if h.is_finished() { h.join().ok().flatten() } else { None }
        });
        // (Whisper and Windows' recognizer heard it differently: a hard one. With "Most accurate dictation" on, the
        //  larger model hears it again, and its words are used. It takes about as long as the speech itself on a
        //  laptop's processor, so only these few wait for it; the rest are typed at once)
        let text = match (&win, best.as_mut()) {
            (Some(h), Some(b)) if voice.patient.load(Ordering::SeqCst) && !voice.live.load(Ordering::SeqCst) && lang == "en" && !wake_only && audio.len() < RATE * 30 => {
                let key = |w: &str| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
                let ws: Vec<String> = text.split_whitespace().map(key).filter(|w| !w.is_empty()).collect();
                let agree = if ws.is_empty() { 0.0 } else { ws.iter().filter(|w| h.words.iter().any(|(x, _)| key(x) == **w)).count() as f32 / ws.len() as f32 };
                // (only when they barely agree at all: Windows' recognizer is weak, they disagreed on most clips, and
                //  each second hearing took as long as the speech itself: the lag after every sentence)
                if agree < 0.25 && ws.len() >= 3 && !h.text.is_empty() {
                    let t1 = std::time::Instant::now();
                    let mut again = String::new();
                    if b.full(steady_params(&prompt, &lang, threads, audio.len()), &audio).is_ok() {
                        for s in b.as_iter() {
                            if let Ok(t) = s.to_str_lossy() {
                                again.push_str(&t);
                            }
                        }
                    }
                    best_at = std::time::Instant::now();
                    let again = unrepeat(again.trim());
                    crate::log_line(&format!("voice: heard differently ({:.0}% alike), the most accurate model heard {again:?} in {} ms", agree * 100.0, t1.elapsed().as_millis()));
                    if again.is_empty() { text } else { again }
                } else {
                    text
                }
            }
            _ => text,
        };
        let noise = no_speech >= 0.6 || is_noise(&text);
        let text = if win.is_some() { crate::winsr::fuse(&text, noise, win.as_ref(), &voice.names.lock().unwrap()) } else { (!noise).then_some(text) };
        if let Some(text) = text.filter(|t| !wake_only || has_wake(t)) {
            let _ = app.emit_to("main", "heard", &text);
        }
        set_status(&app, back, "", 0.0);
    }
}

/* ---------------- microphone + voice activity detection ---------------- */

struct Vad {
    floor: f32,
    hot: u32,
    speaking: bool,
    voiced: u32,
    silence: u32,
    seg: Vec<f32>,
    pre: std::collections::VecDeque<Vec<f32>>,
    tick: u32,
    told: (f32, u32),
}

impl Vad {
    fn new() -> Self {
        Vad { floor: 0.004, hot: 0, speaking: false, voiced: 0, silence: 0, seg: Vec::new(), pre: Default::default(), tick: 0, told: (0.0, 0) }
    }

    fn reset(&mut self) {
        self.speaking = false;
        self.hot = 0;
        self.seg.clear();
        self.pre.clear();
    }

    /// One 20 ms frame plus RNNoise's "is this a voice?" score. Returns a finished utterance when one ends.
    fn frame(&mut self, app: &AppHandle, f: &[f32], sens: f32, voice_p: f32, quiet: u32) -> Option<Vec<f32>> {
        let rms = (f.iter().map(|s| s * s).sum::<f32>() / f.len() as f32).sqrt();
        self.tick += 1;
        if self.tick % 3 == 0 {
            let lvl = (rms / 0.09).min(1.0).sqrt();
            // (Told when it has moved, or every few seconds. In a quiet room the same small number was sent to the page
            //  17 times a second, all day, and each one woke it; then still twice a second, for nothing)
            if (lvl - self.told.0).abs() >= 0.02 || self.tick.wrapping_sub(self.told.1) >= 250 {
                self.told = (lvl, self.tick);
                let _ = app.emit_to("main", "mic-level", lvl);
            }
        }
        // (how loud a voice has to be: over the room, and never below a floor. That floor was 0.006: a laptop's own
        //  microphone set low, or quiet, gives a voice of about 0.002, and nothing it heard ever started)
        let loud = (self.floor * 2.2).max(0.0015) / sens;
        // (dictating, a softer start is caught: a quiet sentence that never looked voice-like enough was lost whole)
        let start_p = (if quiet >= 60 { 0.42 } else { 0.6 } / sens).clamp(0.3, 0.9);
        if !self.speaking {
            self.floor = if rms < self.floor { self.floor * 0.9 + rms * 0.1 } else { self.floor * 0.997 + rms * 0.003 };
            self.pre.push_back(f.to_vec());
            if self.pre.len() > 22 {
                self.pre.pop_front(); // ~440 ms kept from before you started, so first words aren't clipped
            }
            // a voice (not a fan, a click or a keyboard) that's loud enough
            self.hot = if voice_p > start_p && rms > loud { self.hot + 1 } else { 0 };
            if self.hot >= if quiet >= 60 { 2 } else { 3 } {
                self.speaking = true;
                self.voiced = self.hot;
                self.silence = 0;
                self.seg = self.pre.drain(..).flatten().collect();
                vad_status(app, "hearing");
            }
            return None;
        }
        self.seg.extend_from_slice(f);
        if voice_p > 0.3 || rms > loud * 1.4 {
            self.voiced += 1;
            self.silence = 0;
        } else {
            self.silence += 1;
        }
        let long = self.seg.len() >= RATE * 20;
        // ~0.7 s without a voice = you're done (short pauses mid-sentence don't cut you off); dictating, 1.3 s
        if self.silence >= quiet || long {
            self.speaking = false;
            self.hot = 0;
            let mut seg = std::mem::take(&mut self.seg);
            let trim = (self.silence.saturating_sub(12) as usize) * FRAME; // keep ~240 ms of the quiet after
            seg.truncate(seg.len().saturating_sub(trim));
            if self.voiced >= 12 {
                return Some(seg);
            }
            vad_status(app, "listening");
        }
        None
    }
}

/// Windows' default microphone, by its id (to notice when it's changed: headphones plugged in, another chosen)
fn default_mic() -> Option<String> {
    use windows::Win32::Media::Audio::{eCapture, eConsole, IMMDeviceEnumerator, MMDeviceEnumerator};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let id = en.GetDefaultAudioEndpoint(eCapture, eConsole).ok()?.GetId().ok()?;
        let s = id.to_string().ok();
        CoTaskMemFree(Some(id.0 as *const _));
        s
    }
}

fn microphone(app: AppHandle, raw_tx: SyncSender<(Vec<f32>, u32)>) {
    // (a microphone that only gives silence is opened again, but not more than once a minute)
    let mut reopened: Option<std::time::Instant> = None;
    loop {
        let failed = Arc::new(AtomicBool::new(false));
        // (when it last gave anything but digital silence, in ms from t0: a room is never exactly silent)
        let t0 = std::time::Instant::now();
        let alive = Arc::new(AtomicU64::new(0));
        let stream = (|| -> Result<cpal::Stream, String> {
            let host = cpal::default_host();
            let dev = host.default_input_device().ok_or("No microphone found")?;
            let cfg = dev.default_input_config().map_err(|e| e.to_string())?;
            let ch = cfg.channels() as usize;
            let rate = cfg.sample_rate();
            let fmt = cfg.sample_format();
            let config = cfg.config();
            let f1 = failed.clone();
            let err = move |_e: cpal::Error| f1.store(true, Ordering::SeqCst);
            macro_rules! build {
                ($t:ty, $conv:expr) => {{
                    let tx = raw_tx.clone();
                    let alive = alive.clone();
                    dev.build_input_stream::<$t, _, _>(
                        config,
                        move |d: &[$t], _| {
                            let mono: Vec<f32> = d.chunks(ch).map(|c| c.iter().map(|s| $conv(*s)).sum::<f32>() / ch as f32).collect();
                            if mono.iter().any(|x| x.abs() > 1e-6) {
                                alive.store(t0.elapsed().as_millis() as u64, Ordering::Relaxed);
                            }
                            let _ = tx.try_send((mono, rate));
                        },
                        err,
                        None,
                    )
                }};
            }
            let s = match fmt {
                cpal::SampleFormat::F32 => build!(f32, |s: f32| s),
                cpal::SampleFormat::I16 => build!(i16, |s: i16| s as f32 / 32768.0),
                cpal::SampleFormat::U16 => build!(u16, |s: u16| (s as f32 - 32768.0) / 32768.0),
                cpal::SampleFormat::I32 => build!(i32, |s: i32| s as f32 / 2147483648.0),
                other => return Err(format!("Unsupported microphone format {other:?}")),
            }
            .map_err(|e| e.to_string())?;
            s.play().map_err(|e| e.to_string())?;
            Ok(s)
        })();
        match stream {
            Ok(s) => {
                // Windows can switch the default microphone (headphones connected, another chosen in Settings) or
                // leave the one in use giving only silence, without any error: what you said then never reached
                // SYLVER, dictation and the notch's mic "did nothing". The default is looked at every 3 s, and a
                // microphone that gives nothing but digital silence for 6 s is opened again.
                let id = default_mic();
                let mut n = 0u32;
                while !failed.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(500));
                    n += 1;
                    if n % 6 == 0 && default_mic() != id {
                        crate::log_line("voice: the default microphone changed; listening on the new one");
                        break;
                    }
                    let quiet = (t0.elapsed().as_millis() as u64).saturating_sub(alive.load(Ordering::Relaxed));
                    // (looked at muted or not: the stream is heard from even while SYLVER ignores it, so after the laptop
                    //  wakes from sleep it's healthy again before dictation or a tap on the mic needs it)
                    if n >= 12 && quiet > 6000 && reopened.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) {
                        crate::log_line("voice: the microphone gave only silence for 6 s; opening it again");
                        reopened = Some(std::time::Instant::now());
                        break;
                    }
                }
                drop(s); // device unplugged or changed: rebuild on the new default
            }
            Err(e) => {
                set_status(&app, "error", &e, 0.0);
                std::thread::sleep(Duration::from_secs(3));
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn processor(app: AppHandle, raw_rx: Receiver<(Vec<f32>, u32)>) {
    let voice = app.state::<Voice>();
    let mut vad = Vad::new();
    let mut to16 = Resampler::new(RATE as u32);
    let mut to48 = Resampler::new(48_000);
    let mut nn = nnnoiseless::DenoiseState::new();
    let mut scratch = vec![0f32; NN];
    let (mut out16, mut buf48): (Vec<f32>, Vec<f32>) = (Vec::with_capacity(FRAME * 8), Vec::with_capacity(NN * 8));
    let mut probs: std::collections::VecDeque<f32> = Default::default();
    let mut last_p = 0f32;
    let mut was_paused = false;
    let mut sent = 0usize; // (how much of the stretch being said was last handed over to be heard so far)
    let (mut dict_loud, mut dict_voice, mut dict_at) = (0f32, 0f32, std::time::Instant::now());
    let (mut quiet_loud, mut quiet_at, mut quiet_told) = (0f32, std::time::Instant::now(), false);
    let mut env = 0.03f32; // (how loud the microphone has lately been, for the automatic gain)
    for (chunk, rate) in raw_rx {
        if voice.paused.load(Ordering::SeqCst) {
            if !was_paused {
                was_paused = true;
                set_status(&app, "paused", "", 0.0);
                vad.reset();
                out16.clear();
                buf48.clear();
                probs.clear();
                let _ = app.emit_to("main", "mic-level", 0.0f32);
            }
            continue;
        }
        // (listening again: the page is told, or it went on showing "Microphone off" while dictation was hearing you)
        if was_paused {
            set_status(&app, "listening", "", 0.0);
        }
        was_paused = false;
        // Automatic gain: a microphone that reaches SYLVER very quietly (a laptop's array set low, or turned down
        // somewhere along the way: voices of 0.002 where 0.05 is usual) is lifted to an ordinary level before anything
        // listens, up to 32 dB; an ordinary one is left as it is. Quick to come down for a loud sound, slow to rise.
        let rms = (chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len().max(1) as f32).sqrt();
        // (it rises only between sentences: rising while you spoke, it lifted the room's hum under your pause until
        //  the pause no longer counted as one, and a short phrase waited nine seconds to be typed)
        env = if rms > env { env * 0.6 + rms * 0.4 } else if vad.speaking { env } else { env * 0.9993 + rms * 0.0007 };
        let gain = (0.03 / env.max(0.00075)).clamp(1.0, 40.0);
        let chunk: Vec<f32> = if gain > 1.05 { chunk.iter().map(|s| (s * gain).clamp(-1.0, 1.0)).collect() } else { chunk };
        to16.push(&chunk, rate, &mut out16);
        to48.push(&chunk, rate, &mut buf48);
        // RNNoise: how much each 10 ms sounds like a human voice
        let mut used48 = 0;
        while buf48.len() - used48 >= NN {
            let input: Vec<f32> = buf48[used48..used48 + NN].iter().map(|s| s * 32767.0).collect();
            probs.push_back(nn.process_frame(&mut scratch, &input));
            used48 += NN;
        }
        buf48.drain(..used48);
        let sens = f32::from_bits(voice.sens.load(Ordering::SeqCst));
        let mut used = 0;
        while out16.len() - used >= FRAME {
            // two 10 ms scores per 20 ms frame
            let a = probs.pop_front().unwrap_or(last_p);
            let b = probs.pop_front().unwrap_or(a);
            last_p = b;
            // (how long a pause ends what's said: long while dictating, short when you've tapped the mic to ask)
            let quiet = if voice.patient.load(Ordering::SeqCst) { 60 } else if voice.live.load(Ordering::SeqCst) { 24 } else { 30 };
            // (dictating or asked: a microphone giving almost nothing for six seconds is said out loud, once: a mic-mute
            //  key pressed, or the input turned right down, and what you said never reached SYLVER)
            if voice.patient.load(Ordering::SeqCst) || voice.live.load(Ordering::SeqCst) {
                let rms = (out16[used..used + FRAME].iter().map(|s| s * s).sum::<f32>() / FRAME as f32).sqrt();
                quiet_loud = quiet_loud.max(rms);
                if quiet_at.elapsed() > Duration::from_secs(6) {
                    if quiet_loud < 0.0012 && !quiet_told {
                        quiet_told = true;
                        crate::log_line(&format!("voice: the microphone is almost silent (loudest {quiet_loud:.4} in 6 s)"));
                        let _ = app.emit_to("main", "mic-quiet", quiet_loud);
                    }
                    (quiet_loud, quiet_at) = (0.0, std::time::Instant::now());
                }
            } else {
                (quiet_loud, quiet_at, quiet_told) = (0.0, std::time::Instant::now(), false);
            }
            if voice.patient.load(Ordering::SeqCst) {
                // (dictating: how loud it is, and the room under it, told every ten seconds, so a dictation that
                //  hears nothing says why: too quiet for the microphone, or under music)
                dict_loud = dict_loud.max((out16[used..used + FRAME].iter().map(|s| s * s).sum::<f32>() / FRAME as f32).sqrt());
                dict_voice = dict_voice.max(a.max(b));
                if dict_at.elapsed() > Duration::from_secs(10) {
                    crate::log_line(&format!("voice: dictating: loudest {dict_loud:.3}, the room {:.3}, most voice-like {dict_voice:.2} (needs {:.2}), speaking {}", vad.floor, (0.6 / sens).clamp(0.3, 0.9), vad.speaking));
                    (dict_loud, dict_voice, dict_at) = (0.0, 0.0, std::time::Instant::now());
                }
            }
            if let Some(seg) = vad.frame(&app, &out16[used..used + FRAME], sens, a.max(b), quiet) {
                if let Some(tx) = voice.seg_tx.lock().unwrap().as_ref() {
                    // (dictating or asked: it waits its turn, a second at most; it used to be dropped when the
                    //  recognizer was still busy, and what you said vanished)
                    if tx.try_send(seg.clone()).is_err() && (voice.patient.load(Ordering::SeqCst) || voice.live.load(Ordering::SeqCst)) {
                        let tx = tx.clone();
                        std::thread::spawn(move || {
                            let _ = tx.send(seg);
                        });
                    }
                }
                sent = 0;
            } else if vad.speaking && (voice.patient.load(Ordering::SeqCst) || voice.live.load(Ordering::SeqCst)) && vad.seg.len() >= RATE / 2 && vad.seg.len() >= sent + RATE * 6 / 5 && vad.seg.len() < RATE * 9 {
                // (dictating: what's been said so far, every 0.6 s; dropped if the last one is still being heard)
                if let Some(tx) = voice.part_tx.lock().unwrap().as_ref() {
                    if tx.try_send(vad.seg.clone()).is_ok() {
                        sent = vad.seg.len();
                    }
                }
            } else if !vad.speaking {
                sent = 0;
            }
            used += FRAME;
        }
        out16.drain(..used);
        while probs.len() > 8 {
            probs.pop_front(); // never drift out of step
        }
    }
}

/// (developer mode: how long each dictation model takes on a recording, 16 kHz mono, and what it hears; nothing is
///  acted on)
pub fn feed(app: &AppHandle, path: &str) {
    let Ok(b) = std::fs::read(path) else { return crate::log_line("feed: no such recording") };
    let Some(at) = b.windows(4).position(|w| w == b"data") else { return };
    let rate = u32::from_le_bytes(b[24..28].try_into().unwrap_or([0x80, 0x3e, 0, 0]));
    let samples: Vec<f32> = b[at + 8..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect();
    let Some(tx) = app.state::<Voice>().raw_tx.lock().unwrap().clone() else { return };
    crate::log_line(&format!("feed: {:.1}s of {path} into the microphone's stream", samples.len() as f32 / rate as f32));
    let step = rate as usize / 50;
    for c in samples.chunks(step) {
        let _ = tx.send((c.to_vec(), rate));
        std::thread::sleep(Duration::from_millis(20));
    }
    // (then two seconds of quiet, for the pause that ends it)
    for _ in 0..100 {
        let _ = tx.send((vec![0.0005; step], rate));
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn try_file(app: &AppHandle, path: &str) {
    let Ok(b) = std::fs::read(path) else { return crate::log_line("try: no such recording") };
    let Some(at) = b.windows(4).position(|w| w == b"data") else { return };
    let samples: Vec<f32> = b[at + 8..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect();
    let audio = prepare(&samples);
    let threads = (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / 2).clamp(2, 8) as i32;
    for (name, f) in [("standard", model_path(app, "en").ok()), ("most accurate", best_path(app))] {
        let Some(f) = f else { continue };
        let Some(mut st) = WhisperContext::new_with_params(f.to_string_lossy().as_ref(), WhisperContextParameters::default()).ok().and_then(|c| c.create_state().ok()) else { continue };
        let t0 = std::time::Instant::now();
        let mut text = String::new();
        if st.full(steady_params("", "en", threads, audio.len()), &audio).is_ok() {
            for s in st.as_iter() {
                if let Ok(t) = s.to_str_lossy() {
                    text.push_str(&t);
                }
            }
        }
        crate::log_line(&format!("try: {name} heard {:.1}s in {} ms: {:?}", samples.len() as f32 / RATE as f32, t0.elapsed().as_millis(), text.trim()));
    }
}

pub fn start(app: AppHandle) {
    let (seg_tx, seg_rx) = sync_channel::<Vec<f32>>(2);
    *app.state::<Voice>().seg_tx.lock().unwrap() = Some(seg_tx);
    let (part_tx, part_rx) = sync_channel::<Vec<f32>>(1);
    *app.state::<Voice>().part_tx.lock().unwrap() = Some(part_tx);
    let (raw_tx, raw_rx) = sync_channel::<(Vec<f32>, u32)>(64);
    *app.state::<Voice>().raw_tx.lock().unwrap() = Some(raw_tx.clone());
    let a = app.clone();
    std::thread::spawn(move || transcriber(a, seg_rx, part_rx));
    let a = app.clone();
    std::thread::spawn(move || microphone(a, raw_tx));
    std::thread::spawn(move || processor(app, raw_rx));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(path: &str) -> (Vec<f32>, u32) {
        let b = std::fs::read(path).unwrap();
        let rate = u32::from_le_bytes(b[24..28].try_into().unwrap());
        let data = b.windows(4).position(|w| w == b"data").unwrap() + 8;
        (b[data..].chunks(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect(), rate)
    }

    /// cargo test --bin ultron dictation -- --nocapture --ignored   (needs SYLVER_WAVS=dir of .wav files)
    #[test]
    #[ignore]
    fn dictation() {
        let model = std::env::var("SYLVER_MODEL").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources").join("models").join(MODEL_FILE));
        let ctx = WhisperContext::new_with_params(model.to_string_lossy().as_ref(), WhisperContextParameters::default()).unwrap();
        let mut state = ctx.create_state().unwrap();
        println!("system: {}", whisper_rs::print_system_info());
        let limit: usize = std::env::var("SYLVER_LIMIT").ok().and_then(|v| v.parse().ok()).unwrap_or(99);
        let threads = (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / 2).clamp(2, 8) as i32;
        let dir = std::env::var("SYLVER_WAVS").unwrap();
        let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().map(|x| x == "wav").unwrap_or(false)).collect();
        files.sort();
        for f in files.into_iter().take(limit) {
            let (x, rate) = wav(f.to_str().unwrap());
            let mut r = Resampler::new(RATE as u32);
            let mut s16 = Vec::new();
            r.push(&x, rate, &mut s16);
            let audio = prepare(&s16);
            let t0 = std::time::Instant::now();
            let mut p = params("Sylver, open Spotify. Set the volume to 40 percent.", threads, audio.len());
            if std::env::var("SYLVER_GREEDY").is_ok() {
                p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
                p.set_language(Some("en"));
                p.set_n_threads(threads);
                p.set_no_context(true);
                p.set_single_segment(true);
                p.set_no_timestamps(true);
            }
            if let Some(c) = std::env::var("SYLVER_CTX").ok().and_then(|v| v.parse::<i32>().ok()) {
                p.set_audio_ctx(c);
            }
            state.full(p, &audio).unwrap();
            let text: String = state.as_iter().filter_map(|s| s.to_str_lossy().ok().map(|t| t.to_string())).collect();
            println!("{:>5} ms  {:?}  <- {}", t0.elapsed().as_millis(), text.trim(), f.file_name().unwrap().to_string_lossy());
        }
    }
}

#[cfg(test)]
mod wake_tests {
    #[test]
    fn the_name_is_found_however_it_is_written() {
        for t in ["Sylver, pause the music.", " ultron open spotify", "Hey Ul tron!", "Ultra, next song", "OK, Alltron."] {
            assert!(super::has_wake(t), "{t}");
        }
        for t in ["and I will always love you", "ultrasound of the heart", "we are the champions", ""] {
            assert!(!super::has_wake(t), "{t}");
        }
    }
}
