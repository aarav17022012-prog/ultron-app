//! Local speech recognition: microphone -> clean resampling -> neural voice
//! detection (RNNoise) -> Whisper small.en (whisper.cpp), all on this PC. (large-v3-turbo was
//! tried: ~5x slower on a laptop CPU for no real gain on short commands.)
//! No API key; nothing you say leaves the machine.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

const MODEL_FILE: &str = "ggml-small.en-q5_1.bin";
const MODEL_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.en-q5_1.bin";
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
    /// Only speech that starts with "Ultron" would be acted on just now (the page says so).
    pub wake: AtomicBool,
    /// Dictating: a pause has to be twice as long before it ends what you're saying.
    pub patient: AtomicBool,
    sens: AtomicU32, // f32 bits
    prompt: Mutex<String>,
    status: Mutex<VoiceStatus>,
    seg_tx: Mutex<Option<SyncSender<Vec<f32>>>>,
}

impl Default for Voice {
    fn default() -> Self {
        Voice {
            paused: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            wake: AtomicBool::new(false),
            patient: AtomicBool::new(false),
            sens: AtomicU32::new(1.0f32.to_bits()),
            prompt: Mutex::new(String::new()),
            status: Mutex::new(VoiceStatus { state: "loading".into(), ..Default::default() }),
            seg_tx: Mutex::new(None),
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
fn bundled(app: &AppHandle) -> Option<PathBuf> {
    let mut c = Vec::new();
    if let Ok(r) = app.path().resource_dir() {
        c.push(r.join("resources").join("models").join(MODEL_FILE));
        c.push(r.join("models").join(MODEL_FILE));
    }
    c.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources").join("models").join(MODEL_FILE));
    c.push(data_dir(app).join(MODEL_FILE));
    c.into_iter().find(|p| p.exists())
}

fn download(app: &AppHandle, path: &PathBuf) -> Result<(), String> {
    let part = path.with_extension("part");
    let resp = ureq::get(MODEL_URL).timeout(Duration::from_secs(60 * 60)).call().map_err(|e| e.to_string())?;
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

fn model_path(app: &AppHandle) -> Result<PathBuf, String> {
    if let Some(p) = bundled(app) {
        return Ok(p);
    }
    let target = data_dir(app).join(MODEL_FILE);
    download(app, &target).map(|_| target)
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
pub fn params<'a>(prompt: &'a str, threads: i32, samples: usize) -> FullParams<'a, 'a> {
    let mut p = FullParams::new(SamplingStrategy::BeamSearch { beam_size: 4, patience: -1.0 });
    p.set_language(Some("en"));
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

/// "Ultron", or one of the ways the recognizer writes it, somewhere in this.
fn has_wake(text: &str) -> bool {
    let l = text.to_lowercase();
    let squashed: String = l.chars().filter(|c| c.is_alphanumeric()).collect();
    squashed.contains("ultron") || l.split(|c: char| !c.is_alphanumeric()).any(|w| ["ultra", "altron", "ultran", "ultrum", "ultrom", "alltron"].contains(&w))
}

fn transcriber(app: AppHandle, rx: Receiver<Vec<f32>>) {
    let path = match model_path(&app) {
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
    let Ok(mut state) = ctx.create_state() else {
        set_status(&app, "error", "Voice engine failed to start", 0.0);
        return;
    };
    let threads = (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / 2).clamp(2, 8) as i32;
    let voice = app.state::<Voice>();
    voice.ready.store(true, Ordering::SeqCst);
    set_status(&app, if voice.paused.load(Ordering::SeqCst) { "paused" } else { "listening" }, "", 0.0);

    for seg in rx {
        let music =app.state::<crate::media::Media>().state.lock().unwrap().sessions.iter().any(|s| s.playing);
        // A song or a video with words in it is "speech" for as long as it plays, and each stretch was transcribed
        // carefully (a second or two on half the processor's cores) to find that it wasn't for ULTRON: over a core,
        // all the while. Nearly all of that time goes into listening to the clip, and it grows with the clip's length.
        // So while something plays and only "Ultron ..." would be acted on, just the first second and a half (where
        // the name has to be) is heard first, the quick way; the careful pass is kept for when the name is there.
        let prompt = voice.prompt.lock().unwrap().clone();
        if music && voice.wake.load(Ordering::SeqCst) {
            let t0 = std::time::Instant::now();
            let head = prepare(&seg[..seg.len().min(RATE * 8 / 5)]);
            let mut q = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            q.set_language(Some("en"));
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
                q.set_initial_prompt(&prompt); // (the words it's told to expect, "Ultron" first among them)
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
                crate::log_line(&format!("heard {:.1}s while a song played: no \"Ultron\" at its start (checked in {} ms)", seg.len() as f32 / RATE as f32, t0.elapsed().as_millis()));
                set_status(&app, if voice.paused.load(Ordering::SeqCst) { "paused" } else { "listening" }, "", 0.0);
                continue;
            }
        }
        // a long unbroken stretch while music plays is the song coming through the mic, not you:
        // only its start is transcribed, quickly, instead of all of it
        let seg = if music && seg.len() > RATE * 9 { seg[..RATE * 5].to_vec() } else { seg };
        let wake_only = music && seg.len() == RATE * 5;
        set_status(&app, "transcribing", "", 0.0);
        let audio = prepare(&seg);
        let mut p = params(&prompt, threads, audio.len());
        if wake_only {
            p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            p.set_language(Some("en"));
            p.set_n_threads(threads);
            p.set_no_context(true);
            p.set_single_segment(true);
            p.set_no_timestamps(true);
            p.set_audio_ctx(384);
        }
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
        let text = text.trim().to_string();
        crate::log_line(&format!("heard {:.1}s in {} ms: {text:?} (no-speech {no_speech:.2})", seg.len() as f32 / RATE as f32, t0.elapsed().as_millis()));
        let back = if voice.paused.load(Ordering::SeqCst) { "paused" } else { "listening" };
        let woke = has_wake(&text);
        if no_speech < 0.6 && !is_noise(&text) && (!wake_only || woke) {
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
    fn frame(&mut self, app: &AppHandle, f: &[f32], sens: f32, voice_p: f32, patient: bool) -> Option<Vec<f32>> {
        let rms = (f.iter().map(|s| s * s).sum::<f32>() / f.len() as f32).sqrt();
        self.tick += 1;
        if self.tick % 3 == 0 {
            let lvl = (rms / 0.09).min(1.0).sqrt();
            // (Told when it has moved, or twice a second. In a quiet room the same small number was sent to the page
            //  17 times a second, all day, and each one woke it.)
            if (lvl - self.told.0).abs() >= 0.02 || self.tick.wrapping_sub(self.told.1) >= 25 {
                self.told = (lvl, self.tick);
                let _ = app.emit_to("main", "mic-level", lvl);
            }
        }
        let loud = (self.floor * 2.2).max(0.006) / sens;
        let start_p = (0.6 / sens).clamp(0.3, 0.9);
        if !self.speaking {
            self.floor = if rms < self.floor { self.floor * 0.9 + rms * 0.1 } else { self.floor * 0.997 + rms * 0.003 };
            self.pre.push_back(f.to_vec());
            if self.pre.len() > 22 {
                self.pre.pop_front(); // ~440 ms kept from before you started, so first words aren't clipped
            }
            // a voice (not a fan, a click or a keyboard) that's loud enough
            self.hot = if voice_p > start_p && rms > loud { self.hot + 1 } else { 0 };
            if self.hot >= 3 {
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
        if self.silence >= if patient { 60 } else { 30 } || long {
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

fn microphone(app: AppHandle, raw_tx: SyncSender<(Vec<f32>, u32)>) {
    loop {
        let failed = Arc::new(AtomicBool::new(false));
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
                    dev.build_input_stream::<$t, _, _>(
                        config,
                        move |d: &[$t], _| {
                            let mono: Vec<f32> = d.chunks(ch).map(|c| c.iter().map(|s| $conv(*s)).sum::<f32>() / ch as f32).collect();
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
                while !failed.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(500));
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
    for (chunk, rate) in raw_rx {
        if voice.paused.load(Ordering::SeqCst) {
            if !was_paused {
                was_paused = true;
                vad.reset();
                out16.clear();
                buf48.clear();
                probs.clear();
                let _ = app.emit_to("main", "mic-level", 0.0f32);
            }
            continue;
        }
        was_paused = false;
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
            if let Some(seg) = vad.frame(&app, &out16[used..used + FRAME], sens, a.max(b), voice.patient.load(Ordering::SeqCst)) {
                if let Some(tx) = voice.seg_tx.lock().unwrap().as_ref() {
                    let _ = tx.try_send(seg);
                }
            }
            used += FRAME;
        }
        out16.drain(..used);
        while probs.len() > 8 {
            probs.pop_front(); // never drift out of step
        }
    }
}

pub fn start(app: AppHandle) {
    let (seg_tx, seg_rx) = sync_channel::<Vec<f32>>(2);
    *app.state::<Voice>().seg_tx.lock().unwrap() = Some(seg_tx);
    let (raw_tx, raw_rx) = sync_channel::<(Vec<f32>, u32)>(64);
    let a = app.clone();
    std::thread::spawn(move || transcriber(a, seg_rx));
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

    /// cargo test --bin ultron dictation -- --nocapture --ignored   (needs ULTRON_WAVS=dir of .wav files)
    #[test]
    #[ignore]
    fn dictation() {
        let model = std::env::var("ULTRON_MODEL").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources").join("models").join(MODEL_FILE));
        let ctx = WhisperContext::new_with_params(model.to_string_lossy().as_ref(), WhisperContextParameters::default()).unwrap();
        let mut state = ctx.create_state().unwrap();
        println!("system: {}", whisper_rs::print_system_info());
        let limit: usize = std::env::var("ULTRON_LIMIT").ok().and_then(|v| v.parse().ok()).unwrap_or(99);
        let threads = (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / 2).clamp(2, 8) as i32;
        let dir = std::env::var("ULTRON_WAVS").unwrap();
        let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().map(|x| x == "wav").unwrap_or(false)).collect();
        files.sort();
        for f in files.into_iter().take(limit) {
            let (x, rate) = wav(f.to_str().unwrap());
            let mut r = Resampler::new(RATE as u32);
            let mut s16 = Vec::new();
            r.push(&x, rate, &mut s16);
            let audio = prepare(&s16);
            let t0 = std::time::Instant::now();
            let mut p = params("Ultron, open Spotify. Set the volume to 40 percent.", threads, audio.len());
            if std::env::var("ULTRON_GREEDY").is_ok() {
                p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
                p.set_language(Some("en"));
                p.set_n_threads(threads);
                p.set_no_context(true);
                p.set_single_segment(true);
                p.set_no_timestamps(true);
            }
            if let Some(c) = std::env::var("ULTRON_CTX").ok().and_then(|v| v.parse::<i32>().ok()) {
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
        for t in ["Ultron, pause the music.", " ultron open spotify", "Hey Ul tron!", "Ultra, next song", "OK, Alltron."] {
            assert!(super::has_wake(t), "{t}");
        }
        for t in ["and I will always love you", "ultrasound of the heart", "we are the champions", ""] {
            assert!(!super::has_wake(t), "{t}");
        }
    }
}
