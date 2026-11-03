//! SYLVER listens to the song itself (what your PC is playing, not the microphone)
//! with whisper.cpp and a multilingual model:
//!  - "sync": the lyrics were found but the video's timing differs from the album
//!    track (intros, outros), so it hears a few lines and lines the lyrics up.
//!  - "align": only unsynced lyrics exist, so it hears where the singer is and times each line.
//!  - "transcribe": no lyrics exist anywhere, so it writes down the words it hears.
//! When it knows the lyrics, Whisper is given them as a hint, so sung words are recognised far more reliably.
//! Runs only while the lyrics are on screen and the song is playing.
//!
//! The SYLVER Lyric Engine: for lining up it first listens quickly to the raw audio; when that
//! doesn't lock on, and always when writing the words down, it isolates the singer's voice
//! (lyriceng.rs), finds each sung phrase from the voice itself (exact start times), and
//! recognises every phrase on its own, hinted with the lyrics or the song so far.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Deserialize;
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

const MODEL_FILE: &str = "ggml-small-q5_1.bin";
const VOCAL_FILE: &str = "Kim_Vocal_2.onnx";
const RATE: usize = 16_000;
const SR44: usize = crate::lyriceng::SR;
use crate::lyriceng::{CHUNK, GEN, TRIM};

#[derive(Deserialize, Clone, Default)]
pub struct Request {
    pub mode: String, // off | sync | align | transcribe
    #[serde(default)]
    pub title: String, // "Song - Artist", a hint for the words
    pub key: String,
    #[serde(default)]
    pub lines: Vec<(f64, String)>, // the lyrics, for sync
    #[serde(default)]
    pub pos: f64, // where the song is now (seconds)
    #[serde(default)]
    pub playing: bool,
}

struct Anchor {
    pos: f64,
    at: Instant,
    playing: bool,
}

#[derive(Default)]
pub struct Songs {
    req: Mutex<Request>,
    anchor: Mutex<Option<Anchor>>,
    running: AtomicBool,
}

impl Songs {
    /// Song position (seconds) at a moment in time, from the page's last report.
    fn pos_at(&self, t: Instant) -> Option<f64> {
        let a = self.anchor.lock().unwrap();
        let a = a.as_ref()?;
        if !a.playing {
            return None;
        }
        Some(a.pos + (t.saturating_duration_since(a.at).as_secs_f64() - a.at.saturating_duration_since(t).as_secs_f64()))
    }
}

fn model(app: &AppHandle) -> Option<PathBuf> {
    let mut c = Vec::new();
    if let Ok(r) = app.path().resource_dir() {
        c.push(r.join("resources").join("models").join(MODEL_FILE));
    }
    c.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources").join("models").join(MODEL_FILE));
    if let Ok(d) = app.path().app_data_dir() {
        c.push(d.join("models").join(MODEL_FILE));
    }
    c.into_iter().find(|p| p.exists())
}

fn vocal_model(app: &AppHandle) -> Option<PathBuf> {
    let mut c = Vec::new();
    if let Ok(r) = app.path().resource_dir() {
        c.push(r.join("resources").join("models").join(VOCAL_FILE));
    }
    c.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources").join("models").join(VOCAL_FILE));
    c.into_iter().find(|p| p.exists())
}

/// The page tells us what to do (and where the song is) whenever that changes.
pub fn request(app: &AppHandle, r: Request) {
    let s = app.state::<Songs>();
    *s.anchor.lock().unwrap() = Some(Anchor { pos: r.pos, at: Instant::now(), playing: r.playing });
    let off = r.mode == "off";
    *s.req.lock().unwrap() = r;
    if !off && !s.running.swap(true, Ordering::SeqCst) {
        let app = app.clone();
        std::thread::spawn(move || {
            run(&app);
            app.state::<Songs>().running.store(false, Ordering::SeqCst);
        });
    }
}

fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ñ' => 'n',
            c if c.is_alphanumeric() => c,
            _ => ' ',
        })
        .collect::<String>()
        .split_whitespace()
        .filter(|w| w.len() > 1)
        .map(str::to_string)
        .collect()
}

fn junk(t: &str) -> bool {
    let l = t.trim().to_lowercase();
    l.is_empty()
        || l.starts_with('[')
        || l.starts_with('(')
        || l.chars().all(|c| !c.is_alphanumeric())
        || ["thank you", "thanks for watching", "subscribe", "music", "♪", "applause", "you"].iter().any(|j| l.trim_matches(|c: char| !c.is_alphanumeric()) == *j)
}

/// Where a heard line sits in the lyrics: (lyric time - heard time), or None if unsure.
fn offset_from(heard: &[(f64, String)], lines: &[(f64, String)]) -> Option<f64> {
    let lw: Vec<Vec<String>> = lines.iter().map(|(_, t)| words(t)).collect();
    let mut cands = Vec::new();
    for (ht, text) in heard {
        let hw = words(text);
        if hw.len() < 2 {
            continue;
        }
        let mut best = (0.0f64, 0usize);
        for (i, w) in lw.iter().enumerate() {
            if w.is_empty() {
                continue;
            }
            let shared = hw.iter().filter(|x| w.contains(x)).count() as f64;
            let score = shared / (hw.len().max(w.len()) as f64);
            if score > best.0 {
                best = (score, i);
            }
        }
        if best.0 >= 0.5 {
            cands.push((lines[best.1].0 - ht, best.0));
        }
    }
    if cands.is_empty() {
        return None;
    }
    // the offset most of the heard lines agree on
    let mut bestc = (0.0, 0.0);
    for (o, _) in &cands {
        let agree: f64 = cands.iter().filter(|(x, _)| (x - o).abs() < 1.5).map(|(_, s)| s).sum();
        if agree > bestc.1 {
            bestc = (*o, agree);
        }
    }
    let close: Vec<f64> = cands.iter().filter(|(x, _)| (x - bestc.0).abs() < 1.5).map(|(x, _)| *x).collect();
    // two different heard lines must agree (one line alone could be Whisper echoing the hint)
    let strong = close.len() >= 2;
    strong.then(|| close.iter().sum::<f64>() / close.len() as f64)
}

/// Per-song memory of what's been heard and placed.
#[derive(Default)]
struct Song {
    key: String,
    lang: Option<String>,
    heard: Vec<(f64, String)>,
    synced: bool,
    next_from: f64,      // song time we've written lines up to (transcribe)
    aligned_to: isize,   // last lyric line placed (align)
    said: String,        // recent words heard, to keep spelling consistent
    raw_tries: u32,      // quick raw-audio attempts before isolating the voice
}

/// Phrase finder on the isolated voice: a phrase starts when the voice comes in and ends
/// after ~0.3 s of quiet (or 8 s, whichever first). 20 ms frames at 16 kHz.
#[derive(Default)]
struct Phrases {
    pos: usize,
    floor: f32,
    hot: u32,
    quiet: u32,
    start: Option<usize>,
}

const FR: usize = 320;

impl Phrases {
    /// Finished phrases as (offset in the stream, samples).
    fn feed(&mut self, v: &[f32]) -> Vec<(usize, Vec<f32>)> {
        let mut out = Vec::new();
        if self.floor == 0.0 {
            self.floor = 0.004;
        }
        while self.pos + FR <= v.len() {
            let f = &v[self.pos..self.pos + FR];
            let rms = (f.iter().map(|s| s * s).sum::<f32>() / FR as f32).sqrt();
            let thr = (self.floor * 4.0).max(0.012);
            match self.start {
                None => {
                    self.floor = if rms < self.floor * 3.0 { self.floor * 0.95 + rms * 0.05 } else { self.floor * 0.999 + rms * 0.001 };
                    self.hot = if rms > thr { self.hot + 1 } else { 0 };
                    if self.hot >= 3 {
                        // include the onset and a breath before it
                        self.start = Some(self.pos.saturating_sub(FR * 7));
                        self.quiet = 0;
                    }
                }
                Some(s) => {
                    self.quiet = if rms < thr * 0.6 { self.quiet + 1 } else { 0 };
                    let len = self.pos + FR - s;
                    if self.quiet >= 16 || len >= RATE * 8 {
                        let end = (self.pos + FR).saturating_sub(self.quiet.saturating_sub(5) as usize * FR);
                        if end > s + RATE / 2 {
                            out.push((s, v[s..end].to_vec()));
                        }
                        self.start = None;
                        self.hot = 0;
                        self.quiet = 0;
                    }
                }
            }
            self.pos += FR;
        }
        out
    }

    /// How much of the stream can be forgotten (nothing before an unfinished phrase).
    fn keep_from(&self) -> usize {
        self.start.unwrap_or(self.pos)
    }

    fn shift(&mut self, n: usize) {
        self.pos -= n;
        if let Some(s) = self.start.as_mut() {
            *s -= n;
        }
    }
}

fn hint_for(req: &Request, song: &Song, near: f64) -> String {
    if req.mode == "transcribe" {
        let tail: String = song.said.chars().rev().take(160).collect::<String>().chars().rev().collect();
        return format!("{}. {}", req.title, tail);
    }
    req.lines
        .iter()
        .enumerate()
        .filter(|(i, (t, _))| {
            if req.mode == "align" {
                (*i as isize) >= song.aligned_to - 1 && (*i as isize) <= song.aligned_to + 8
            } else {
                (*t - near).abs() < 40.0 || req.lines.len() < 12
            }
        })
        .map(|(_, (_, l))| l.as_str())
        .take(10)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whisper on a piece of audio. `whole` = one phrase (one line out), otherwise a window that may hold several.
fn recognise(state: &mut whisper_rs::WhisperState, audio: &[f32], hint: &str, song: &mut Song, careful: bool, whole: bool, threads: i32) -> Vec<(f64, String)> {
    let mut p = if careful { FullParams::new(SamplingStrategy::BeamSearch { beam_size: 3, patience: -1.0 }) } else { FullParams::new(SamplingStrategy::Greedy { best_of: 1 }) };
    if !hint.trim().is_empty() {
        p.set_initial_prompt(hint.trim());
    }
    p.set_language(Some(song.lang.as_deref().unwrap_or("auto")));
    p.set_n_threads(threads);
    p.set_translate(false);
    p.set_no_context(true);
    p.set_suppress_blank(true);
    p.set_suppress_nst(true);
    p.set_print_special(false);
    p.set_print_progress(false);
    p.set_print_realtime(false);
    p.set_print_timestamps(false);
    if whole {
        p.set_single_segment(true);
        p.set_no_timestamps(true);
    }
    let secs = audio.len() as f64 / RATE as f64;
    p.set_audio_ctx((((secs * 50.0 + 96.0) / 64.0).ceil() * 64.0).min(1500.0) as i32);
    if state.full(p, audio).is_err() {
        return Vec::new();
    }
    if song.lang.is_none() {
        let id = state.full_lang_id_from_state();
        song.lang = (id >= 0).then(|| whisper_rs::get_lang_str(id).map(str::to_string)).flatten();
    }
    let mut out = Vec::new();
    for s in state.as_iter() {
        if s.no_speech_probability() > 0.6 {
            continue;
        }
        let Ok(t) = s.to_str_lossy() else { continue };
        let t = t.trim().trim_matches('♪').trim().to_string();
        if junk(&t) || looping(&t) {
            continue;
        }
        out.push((if whole { 0.0 } else { s.start_timestamp() as f64 / 100.0 }, t));
    }
    out
}

/// Whisper sometimes gets stuck repeating a phrase on music: drop those.
fn looping(t: &str) -> bool {
    let w = words(t);
    if w.len() < 6 {
        return false;
    }
    let tri: Vec<String> = w.windows(3).map(|x| x.join(" ")).collect();
    tri.iter().any(|g| tri.iter().filter(|x| *x == g).count() > 2)
}

/// Do something with lines heard at these song times, depending on the mode. Returns true if it moved things on.
fn deliver(app: &AppHandle, req: &Request, song: &mut Song, lines: Vec<(f64, String)>) -> bool {
    match req.mode.as_str() {
        "align" => {
            let mut moved = false;
            for (at, text) in &lines {
                let hw = words(text);
                if hw.len() < 2 {
                    continue;
                }
                let from = (song.aligned_to.max(0) as usize).saturating_sub(1);
                let mut best = (0.0f64, 0usize);
                for (i, (_, l)) in req.lines.iter().enumerate().skip(from).take(12) {
                    let lw = words(l);
                    if lw.is_empty() {
                        continue;
                    }
                    let shared = hw.iter().filter(|x| lw.contains(x)).count() as f64;
                    let score = shared / (hw.len().max(lw.len()) as f64);
                    if score > best.0 {
                        best = (score, i);
                    }
                }
                if best.0 >= 0.45 && best.1 as isize >= song.aligned_to {
                    song.aligned_to = best.1 as isize;
                    moved = true;
                    let _ = app.emit_to("main", "lyrics-align", json!({ "key": song.key, "index": best.1, "at": (at * 10.0).round() / 10.0 }));
                }
            }
            moved
        }
        "sync" => {
            song.heard.extend(lines);
            if let Some(off) = offset_from(&song.heard, &req.lines) {
                song.synced = true;
                let _ = app.emit_to("main", "lyrics-offset", json!({ "key": song.key, "offset": (off * 10.0).round() / 10.0 }));
                return true;
            }
            if song.heard.len() > 14 {
                song.heard.drain(..song.heard.len() - 14);
            }
            false
        }
        _ => {
            let fresh: Vec<(f64, String)> = lines.into_iter().filter(|(t, _)| *t >= song.next_from - 0.3).collect();
            if let Some(l) = fresh.last() {
                song.next_from = l.0 + 0.5;
            }
            for (_, t) in &fresh {
                song.said.push(' ');
                song.said.push_str(t);
            }
            if fresh.is_empty() {
                return false;
            }
            let _ = app.emit_to("main", "song-lines", json!({ "key": song.key, "lines": fresh, "lang": song.lang }));
            true
        }
    }
}

fn run(app: &AppHandle) {
    let songs = app.state::<Songs>();
    let Some(path) = model(app) else { return };
    let Ok(ctx) = WhisperContext::new_with_params(path.to_string_lossy().as_ref(), WhisperContextParameters::default()) else { return };
    let Ok(mut state) = ctx.create_state() else { return };

    // what the PC is playing (WASAPI loopback): mono 16 kHz for quick listening, and stereo
    // 44.1 kHz for the vocal isolator, with a running count so we know where each sample sits in time
    struct Cap {
        mono: VecDeque<f32>,
        l: VecDeque<f32>,
        r: VecDeque<f32>,
        total: u64, // stereo samples ever captured
        last: Instant,
    }
    let cap = Arc::new(Mutex::new(Cap { mono: VecDeque::new(), l: VecDeque::new(), r: VecDeque::new(), total: 0, last: Instant::now() }));
    let stream = (|| -> Option<cpal::Stream> {
        let host = cpal::default_host();
        let dev = host.default_output_device()?;
        let cfg = dev.default_output_config().ok()?;
        let ch = cfg.channels() as usize;
        let rate = cfg.sample_rate();
        let c = cap.clone();
        let mut to16 = crate::voice::Resampler::new(RATE as u32);
        let (mut tol, mut tor) = (crate::voice::Resampler::new(SR44 as u32), crate::voice::Resampler::new(SR44 as u32));
        let (mut out, mut ol, mut or) = (Vec::new(), Vec::new(), Vec::new());
        let s = dev
            .build_input_stream::<f32, _, _>(
                cfg.config(),
                move |d: &[f32], _| {
                    let mono: Vec<f32> = d.chunks(ch).map(|c| c.iter().sum::<f32>() / ch as f32).collect();
                    let left: Vec<f32> = d.chunks(ch).map(|c| c[0]).collect();
                    let right: Vec<f32> = d.chunks(ch).map(|c| *c.get(1).unwrap_or(&c[0])).collect();
                    out.clear();
                    ol.clear();
                    or.clear();
                    to16.push(&mono, rate, &mut out);
                    tol.push(&left, rate, &mut ol);
                    tor.push(&right, rate, &mut or);
                    let mut q = c.lock().unwrap();
                    q.mono.extend(out.iter());
                    let n = ol.len().min(or.len());
                    q.l.extend(ol[..n].iter());
                    q.r.extend(or[..n].iter());
                    q.total += n as u64;
                    let m = q.mono.len();
                    if m > RATE * 30 {
                        q.mono.drain(..m - RATE * 30);
                    }
                    let k = q.l.len();
                    if k > SR44 * 30 {
                        q.l.drain(..k - SR44 * 30);
                        q.r.drain(..k - SR44 * 30);
                    }
                    q.last = Instant::now();
                },
                |_| {},
                None,
            )
            .ok()?;
        s.play().ok()?;
        Some(s)
    })();
    let Some(_stream) = stream else {
        crate::log_line("songs: no loopback audio");
        return;
    };

    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let threads = (cores / 4).clamp(2, 4) as i32;
    let mut sep: Option<crate::lyriceng::Separator> = None;
    let mut sep_failed = false;
    let mut song = Song { aligned_to: -1, ..Default::default() };
    // the isolated-voice stream (16 kHz), where it starts in song time, and the phrase finder on it
    let mut iso_next: u64 = 0;
    let mut voice: Vec<f32> = Vec::new();
    let mut voice_t0 = 0.0f64;
    let mut phrases = Phrases::default();
    let mut to16v = crate::voice::Resampler::new(RATE as u32);
    let mut idle = Instant::now();
    loop {
        let req = songs.req.lock().unwrap().clone();
        if req.mode == "off" {
            if idle.elapsed() > Duration::from_secs(20) {
                break; // nothing to do for a while: free the models and the audio
            }
            std::thread::sleep(Duration::from_millis(300));
            continue;
        }
        idle = Instant::now();
        if req.key != song.key {
            song = Song { key: req.key.clone(), aligned_to: -1, ..Default::default() };
            let mut c = cap.lock().unwrap();
            c.mono.clear();
            c.l.clear();
            c.r.clear();
            drop(c);
            iso_next = 0;
            voice.clear();
            phrases = Phrases::default();
            to16v = crate::voice::Resampler::new(RATE as u32);
        }
        if req.mode == "sync" && song.synced {
            std::thread::sleep(Duration::from_millis(300));
            continue;
        }

        // quick path first for lining lyrics up (raw audio, a few seconds); the full engine when that
        // doesn't lock on, and always when there are no lyrics to go on
        let isolate = !sep_failed && (req.mode == "transcribe" || song.raw_tries >= 2);
        if !isolate {
            let win = 5.0;
            let (audio, end_at) = {
                let mut q = cap.lock().unwrap();
                let n = q.mono.len();
                let take = (win * RATE as f64) as usize;
                if n < take {
                    drop(q);
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                let a: Vec<f32> = q.mono.iter().skip(n - take).copied().collect();
                q.mono.drain(..n - RATE); // keep a second of overlap
                (a, q.last)
            };
            let Some(end_pos) = songs.pos_at(end_at) else {
                std::thread::sleep(Duration::from_millis(250));
                continue; // paused
            };
            let start_pos = end_pos - win;
            let peak = audio.iter().fold(0f32, |m, s| m.max(s.abs()));
            if peak < 0.01 {
                continue;
            }
            let audio: Vec<f32> = audio.iter().map(|s| s * (0.9 / peak).min(8.0)).collect();
            let hint = hint_for(&req, &song, start_pos);
            let t0 = Instant::now();
            let lines: Vec<(f64, String)> = recognise(&mut state, &audio, &hint, &mut song, false, false, threads).into_iter().map(|(t, s)| (start_pos + t, s)).collect();
            crate::log_line(&format!("songs {} quick: {} lines in {} ms", req.mode, lines.len(), t0.elapsed().as_millis()));
            if !deliver(app, &req, &mut song, lines) {
                song.raw_tries += 1;
            }
            continue;
        }

        // ---- the full engine: isolate the voice, find each sung phrase, recognise it ----
        if sep.is_none() {
            let Some(mp) = vocal_model(app) else {
                sep_failed = true;
                continue;
            };
            let t0 = Instant::now();
            sep = crate::lyriceng::Separator::load(&mp, (cores / 2).clamp(2, 8));
            crate::log_line(&format!("songs: vocal isolator {} in {} ms", if sep.is_some() { "ready" } else { "FAILED" }, t0.elapsed().as_millis()));
            if sep.is_none() {
                sep_failed = true;
                continue;
            }
        }
        let (win, seg_end_pos) = {
            let q = cap.lock().unwrap();
            let start_abs = q.total - q.l.len() as u64;
            // fell behind (or just started): continue from the oldest audio we still have
            if iso_next == 0 || iso_next < start_abs + TRIM as u64 {
                if iso_next != 0 {
                    voice.clear();
                    phrases = Phrases::default();
                }
                iso_next = start_abs + TRIM as u64;
            }
            if iso_next + (GEN + TRIM) as u64 > q.total {
                drop(q);
                std::thread::sleep(Duration::from_millis(150));
                continue;
            }
            let from = (iso_next - TRIM as u64 - start_abs) as usize;
            let l: Vec<f32> = q.l.iter().skip(from).take(CHUNK).copied().collect();
            let r: Vec<f32> = q.r.iter().skip(from).take(CHUNK).copied().collect();
            let Some(end_pos) = songs.pos_at(q.last) else {
                drop(q);
                std::thread::sleep(Duration::from_millis(250));
                continue;
            };
            // song time where this window's clean part begins
            ((l, r), end_pos - (q.total - iso_next) as f64 / SR44 as f64)
        };
        let t0 = Instant::now();
        let Some(vocals) = sep.as_mut().and_then(|s| s.isolate(&win.0, &win.1)) else {
            sep_failed = true;
            continue;
        };
        let iso_ms = t0.elapsed().as_millis();
        iso_next += GEN as u64;
        if voice.is_empty() {
            voice_t0 = seg_end_pos;
        }
        let mut v16 = Vec::new();
        to16v.push(&vocals, SR44 as u32, &mut v16);
        voice.extend(v16);
        let found = phrases.feed(&voice);
        let mut lines = Vec::new();
        for (at, audio) in found {
            let when = voice_t0 + at as f64 / RATE as f64;
            let clip = crate::voice::prepare(&audio);
            let hint = hint_for(&req, &song, when);
            let careful = req.mode == "transcribe";
            for (_, text) in recognise(&mut state, &clip, &hint, &mut song, careful, true, threads) {
                lines.push((when, text));
            }
        }
        crate::log_line(&format!("songs {} engine: isolate {} ms, {} lines, total {} ms", req.mode, iso_ms, lines.len(), t0.elapsed().as_millis()));
        // forget voice we're done with
        let k = phrases.keep_from();
        if k > RATE * 2 {
            let cut = k - RATE;
            voice.drain(..cut);
            voice_t0 += cut as f64 / RATE as f64;
            phrases.shift(cut);
        }
        if !lines.is_empty() {
            deliver(app, &req, &mut song, lines);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn lines_up() {
        let lyrics = vec![(10.0, "Yeah, I'm gonna take my horse to the old town road".to_string()), (14.5, "I'm gonna ride 'til I can't no more".to_string()), (19.0, "I got the horses in the back".to_string())];
        // heard 6 s earlier in the video than the album timing
        let heard = vec![(4.1, "gonna take my horse to the old town road".to_string()), (8.6, "I'm gonna ride till I can't no more".to_string())];
        let off = super::offset_from(&heard, &lyrics);
        println!("offset {off:?}");
        assert!((off.unwrap() - 5.9).abs() < 0.3);
        assert!(super::offset_from(&[(3.0, "completely different words here".to_string())], &lyrics).is_none());
    }
}
