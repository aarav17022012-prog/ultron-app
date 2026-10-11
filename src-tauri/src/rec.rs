//! Screen recording. The picture and sound are captured and compressed by the web engine ULTRON's
//! window already runs in (it hands the work to the graphics card's own video encoder when there is
//! one, so recording doesn't slow the PC), a second at a time; each second is written to the file here
//! as it arrives, so a long recording never sits in memory and a crash loses at most a second.
//!
//! Recordings go to Documents\ULTRON Files\Recordings, with everything else ULTRON makes.
//! Two keys work from anywhere: Ctrl+Alt+R starts, Ctrl+Alt+S stops.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use base64::Engine;
use tauri::{AppHandle, Emitter, Manager};

static FILE: Mutex<Option<(std::fs::File, PathBuf)>> = Mutex::new(None);
static KEYS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

pub fn folder(app: &AppHandle) -> PathBuf {
    let d = app.path().document_dir().unwrap_or_else(|_| crate::skills::known_folder("documents")).join("ULTRON Files").join("Recordings");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Start a new file ("Recording 2026-10-11 14.05.33.mp4"). Returns where it is.
pub fn begin(app: &AppHandle, ext: &str) -> Result<String, String> {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    let ext = if ext == "webm" { "webm" } else { "mp4" };
    let path = folder(app).join(format!("Recording {:04}-{:02}-{:02} {:02}.{:02}.{:02}.{ext}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond));
    let f = std::fs::File::create(&path).map_err(|e| format!("I couldn't create the recording file: {e}"))?;
    *FILE.lock().unwrap() = Some((f, path.clone()));
    crate::backdrop::hold(true); // (ULTRON stays in the picture, steadily, for the whole recording)
    crate::log_line("recording: started");
    Ok(path.to_string_lossy().to_string())
}

/// The next piece of the recording (base64), added to the end of the file.
pub fn data(b64: &str) -> bool {
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) else { return false };
    match FILE.lock().unwrap().as_mut() {
        Some((f, _)) => f.write_all(&bytes).is_ok(),
        None => false,
    }
}

/// Close the file (the recording ran for `secs`). Returns where it is and how big it turned out (MB).
pub fn end(secs: f64) -> Option<(String, f64)> {
    crate::backdrop::hold(false);
    let (f, path) = FILE.lock().unwrap().take()?;
    let _ = f.sync_all();
    drop(f);
    let timed = secs > 0.0 && path.extension().is_some_and(|e| e == "mp4") && lengths(&path, secs).is_some();
    let mb = std::fs::metadata(&path).map(|m| m.len() as f64 / 1e6).unwrap_or(0.0);
    if mb == 0.0 {
        let _ = std::fs::remove_file(&path); // (it never started: no empty file left behind)
        return None;
    }
    crate::log_line(&format!("recording: saved, {mb:.1} MB, {secs:.1} s{}", if timed { "" } else { " (length not written in)" }));
    Some((path.to_string_lossy().to_string(), mb))
}

/// The sound the PC is playing (music, a video, a call), for the recording. The screen capture brings no
/// sound with it, so while a recording runs ULTRON listens to what's going to the speakers and hands it to
/// the page a fiftieth of a second at a time (two channels of 32-bit numbers); the page mixes it into the
/// file. Returns how many samples a second it comes at (0 when there is nothing to listen to).
static SOUND_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn sound(app: &AppHandle, on: bool) -> u32 {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::sync::atomic::Ordering;
    if !on {
        SOUND_ON.store(false, Ordering::SeqCst);
        return 0;
    }
    let Some(rate) = cpal::default_host().default_output_device().and_then(|d| d.default_output_config().ok()).filter(|c| c.sample_format() == cpal::SampleFormat::F32).map(|c| c.sample_rate()) else {
        crate::log_line("recording: no speaker sound to listen to");
        return 0;
    };
    if SOUND_ON.swap(true, Ordering::SeqCst) {
        return rate;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let heard: std::sync::Arc<Mutex<Vec<f32>>> = Default::default();
        let stream = (|| -> Option<cpal::Stream> {
            let dev = cpal::default_host().default_output_device()?;
            let cfg = dev.default_output_config().ok()?;
            let ch = cfg.channels() as usize;
            let h = heard.clone();
            let s = dev
                .build_input_stream::<f32, _, _>(
                    cfg.config(),
                    move |d: &[f32], _| {
                        let mut q = h.lock().unwrap();
                        for c in d.chunks(ch.max(1)) {
                            q.push(c[0]);
                            q.push(*c.get(1).unwrap_or(&c[0]));
                        }
                        let n = q.len();
                        if n > 400_000 {
                            q.drain(..n - 400_000); // (never more than a couple of seconds waiting)
                        }
                    },
                    |_| {},
                    None,
                )
                .ok()?;
            s.play().ok()?;
            Some(s)
        })();
        let Some(_stream) = stream else {
            crate::log_line("recording: the speaker sound couldn't be opened");
            SOUND_ON.store(false, Ordering::SeqCst);
            return;
        };
        let mut pieces = 0u32;
        while SOUND_ON.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let chunk: Vec<f32> = std::mem::take(&mut *heard.lock().unwrap());
            if chunk.is_empty() {
                continue; // (nothing is playing)
            }
            let bytes: Vec<u8> = chunk.iter().flat_map(|v| v.to_le_bytes()).collect();
            let _ = app.emit_to("main", "rec-sound", serde_json::json!({ "rate": rate, "pcm": base64::engine::general_purpose::STANDARD.encode(bytes) }));
            pieces += 1;
        }
        crate::log_line(&format!("recording: {pieces} pieces of sound went into it"));
    });
    rate
}

/// A real click at a spot in ULTRON's own window (page pixels). The web engine only lets a page start
/// capturing the screen in answer to a click, so the "start" key presses ULTRON's own Record button.
pub fn click(app: &AppHandle, x: f64, y: f64) {
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::UI::Input::KeyboardAndMouse::{mouse_event, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP};
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetWindowRect, SetCursorPos};
    let Some(h) = app.get_webview_window("main").and_then(|w| crate::hit::hwnd_of(&w)) else { return };
    unsafe {
        let mut r = RECT::default();
        let _ = GetWindowRect(h, &mut r);
        let s = crate::hit::scale_of(h);
        let (sx, sy) = (r.left + (x * s).round() as i32, r.top + (y * s).round() as i32);
        let mut old = POINT::default();
        let _ = GetCursorPos(&mut old);
        let _ = SetCursorPos(sx, sy);
        std::thread::sleep(std::time::Duration::from_millis(60)); // (ULTRON's window notices the pointer and takes the click)
        mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
        mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
        std::thread::sleep(std::time::Duration::from_millis(40));
        let _ = SetCursorPos(old.x, old.y);
    }
}

/// The keys held with R (start) and S (stop) on this PC: ["Ctrl", "Alt"] unless another app already had those.
pub fn keys() -> Vec<&'static str> {
    KEYS.lock().unwrap().clone()
}

/// Ctrl+Alt+R and Ctrl+Alt+S, from anywhere in Windows (or the next pair that's free, if another app has one of them).
pub fn hotkeys(app: AppHandle) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT};
    use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};
    let _ = folder(&app); // (Documents\ULTRON Files, with Recordings in it, is there from the first start)
    std::thread::spawn(move || unsafe {
        let sets: [(HOT_KEY_MODIFIERS, &[&'static str]); 3] = [(MOD_CONTROL | MOD_ALT, &["Ctrl", "Alt"]), (MOD_ALT | MOD_SHIFT, &["Alt", "Shift"]), (MOD_CONTROL | MOD_ALT | MOD_SHIFT, &["Ctrl", "Alt", "Shift"])];
        for (m, names) in sets {
            let r = RegisterHotKey(None, 1, m | MOD_NOREPEAT, b'R' as u32).is_ok();
            if r && RegisterHotKey(None, 2, m | MOD_NOREPEAT, b'S' as u32).is_ok() {
                *KEYS.lock().unwrap() = names.to_vec();
                break;
            }
            if r {
                let _ = UnregisterHotKey(None, 1);
            }
        }
        let k = keys().join("+");
        crate::log_line(&if k.is_empty() { "recording keys: none free".to_string() } else { format!("recording keys: {k}+R starts, {k}+S stops") });
        let mut m = MSG::default();
        while GetMessageW(&mut m, None, 0, 0).as_bool() {
            if m.message == WM_HOTKEY {
                let _ = app.emit_to("main", "rec-key", if m.wParam.0 == 1 { "start" } else { "stop" });
            }
        }
    });
}

/// The file is written a second at a time, so its header can't say how long the recording will be. Once it's
/// over the length is written in (the movie's, and each track's), so players and Explorer show it.
fn lengths(path: &std::path::Path, secs: f64) -> Option<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(path).ok()?;
    let mut b = Vec::new();
    (&mut f).take(1 << 16).read_to_end(&mut b).ok()?;
    // a box: 4 bytes of size, 4 of name, then what's in it
    fn kids(b: &[u8], (mut at, end): (usize, usize)) -> Vec<([u8; 4], usize, usize)> {
        let mut out = Vec::new();
        while at + 8 <= end.min(b.len()) {
            let size = u32::from_be_bytes(b[at..at + 4].try_into().unwrap()) as usize;
            let name: [u8; 4] = b[at + 4..at + 8].try_into().unwrap();
            let (head, size) = if size == 1 && at + 16 <= b.len() { (16, u64::from_be_bytes(b[at + 8..at + 16].try_into().unwrap()) as usize) } else { (8, size) };
            if size < head {
                break;
            }
            out.push((name, at + head, at.saturating_add(size)));
            at = at.saturating_add(size);
        }
        out
    }
    let moov = kids(&b, (0, b.len())).into_iter().find(|k| &k.0 == b"moov").filter(|k| k.2 <= b.len())?;
    // where the ticks-per-second and the length sit in a header (movie and media: two dates, ticks, length; track: two dates, its number, a gap, length)
    let field = |p: usize, track: bool| -> (usize, usize, usize) {
        let (dates, wide) = if b[p] == 1 { (16, 8) } else { (8, 4) };
        if track { (0, p + 4 + dates + 8, wide) } else { (p + 4 + dates, p + 4 + dates + 4, wide) }
    };
    let ticks = |at: usize| u32::from_be_bytes(b[at..at + 4].try_into().unwrap()) as f64;
    let mut edits: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut put = |at: usize, wide: usize, v: f64| edits.push((at, if wide == 8 { (v as u64).to_be_bytes().to_vec() } else { (v.min(u32::MAX as f64) as u32).to_be_bytes().to_vec() }));
    let inside = kids(&b, (moov.1, moov.2));
    let mvhd = inside.iter().find(|k| &k.0 == b"mvhd")?;
    let (ts, at, wide) = field(mvhd.1, false);
    let movie = ticks(ts);
    put(at, wide, secs * movie);
    for trak in inside.iter().filter(|k| &k.0 == b"trak") {
        let t = kids(&b, (trak.1, trak.2));
        if let Some(tkhd) = t.iter().find(|k| &k.0 == b"tkhd") {
            let (_, at, wide) = field(tkhd.1, true);
            put(at, wide, secs * movie);
        }
        if let Some(mdhd) = t.iter().find(|k| &k.0 == b"mdia").and_then(|m| kids(&b, (m.1, m.2)).into_iter().find(|k| &k.0 == b"mdhd")) {
            let (ts, at, wide) = field(mdhd.1, false);
            put(at, wide, secs * ticks(ts));
        }
    }
    for (at, bytes) in edits {
        f.seek(SeekFrom::Start(at as u64)).ok()?;
        f.write_all(&bytes).ok()?;
    }
    Some(())
}
