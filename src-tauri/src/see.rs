//! ULTRON's eyes: a vision-language model (Qwen2.5-VL) that looks at a picture of a window
//! and answers about it. It runs in its own llama-server, started only when needed and
//! unloaded after a few idle minutes so it never weighs on the PC.
//!
//! And its pointing finger: UI-TARS, a vision model trained on thousands of apps to do one thing
//! well, look at a screenshot and point at "the thing you mean" (the gear icon, the blue Sign in
//! button). It's the careful half of how ULTRON finds things on screen: vision.rs first tries the
//! instant way (what the app says about itself, its text, the icon detector), and asks UI-TARS only
//! when that can't tell. Where UI-TARS points is then snapped to the nearest thing the instant map
//! knows, which makes the click exact. Only one of the two models is in memory at a time.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use tauri::{AppHandle, Manager};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

const NO_WINDOW: u32 = 0x0000_0008; // DETACHED_PROCESS: no console (and no Console Window Host beside it)
const BELOW_NORMAL: u32 = 0x0000_4000;
const IDLE: Duration = Duration::from_secs(4 * 60);

/// the running server, and which model it holds: "eyes" or "pointer"
static SERVER: Mutex<Option<(Child, u16, &'static str)>> = Mutex::new(None);
static LAST: Mutex<Option<Instant>> = Mutex::new(None);

fn models_dir(app: &AppHandle) -> PathBuf {
    app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir()).join("models")
}

/// The seeing brain's two files, if they're on this PC (the one for this tier, else any).
fn files(app: &AppHandle) -> Option<(PathBuf, PathBuf)> {
    let d = models_dir(app);
    let want = crate::router::vision_model();
    [want, &crate::router::VL_3B, &crate::router::VL_7B].into_iter().find_map(|m| {
        let (proj, _) = m.mmproj?;
        let (a, b) = (d.join(m.file), d.join(proj));
        (a.exists() && b.exists()).then_some((a, b))
    })
}

pub fn available(app: &AppHandle) -> bool {
    files(app).is_some()
}

/// UI-TARS's two files (the model, and the part that turns a picture into something it can read), if they're here.
fn pointer_files(app: &AppHandle) -> Option<(PathBuf, PathBuf)> {
    let d = models_dir(app);
    let gguf = |dir: PathBuf, proj: bool| {
        std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| {
            let n = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            n.contains("ui-tars") && n.ends_with(".gguf") && n.contains("mmproj") == proj
        })
    };
    Some((gguf(d.join("ui-tars"), false).or_else(|| gguf(d.clone(), false))?, gguf(d.join("ui-tars-mm"), true).or_else(|| gguf(d.join("ui-tars"), true)).or_else(|| gguf(d, true))?))
}

pub fn pointer_available(app: &AppHandle) -> bool {
    pointer_files(app).is_some()
}

fn exe(app: &AppHandle) -> Option<PathBuf> {
    let mut bases = Vec::new();
    if let Ok(r) = app.path().resource_dir() {
        bases.push(r.join("resources"));
        bases.push(r);
    }
    bases.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources"));
    bases.into_iter().map(|b| b.join("llama").join("llama-server.exe")).find(|p| p.exists())
}

fn healthy(port: u16) -> bool {
    ureq::get(&format!("http://127.0.0.1:{port}/health")).timeout(Duration::from_millis(800)).call().is_ok()
}

fn start(app: &AppHandle, gpu: bool, kind: &'static str) -> Option<(Child, u16)> {
    let (model, proj) = if kind == "pointer" { pointer_files(app)? } else { files(app)? };
    let exe = exe(app)?;
    let port = std::net::TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map(|a| a.port()).ok()?;
    let threads = (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / 2).saturating_sub(1).clamp(2, 8);
    let mut c = Command::new(&exe);
    c.current_dir(exe.parent()?)
        .arg("-m").arg(&model)
        .arg("--mmproj").arg(&proj)
        .args(["--host", "127.0.0.1", "--port", &port.to_string(), "-c", "6144", "-np", "1", "-t", &threads.to_string()])
        .args(["-tb", &(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / 2).max(2).to_string(), "--log-disable", "--no-webui", "--cache-ram", "0", "-ctxcp", "2"])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .creation_flags(NO_WINDOW | BELOW_NORMAL);
    if !gpu {
        c.args(["-dev", "none"]);
    }
    let mut child = c.spawn().ok()?;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(90) {
        if child.try_wait().ok().flatten().is_some() {
            return None;
        }
        if healthy(port) {
            return Some((child, port));
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    let _ = child.kill();
    None
}

/// The eyes (or the pointing finger), opened if they aren't already. Whichever of the two was in memory
/// makes room for the other.
fn port(app: &AppHandle, kind: &'static str) -> Option<u16> {
    let mut g = SERVER.lock().unwrap();
    if let Some((c, p, k)) = g.as_mut() {
        if *k == kind && c.try_wait().ok().flatten().is_none() {
            return Some(*p);
        }
        let _ = c.kill();
        g.take();
    }
    let t0 = Instant::now();
    // small laptop graphics chips read pictures slower than the CPU (measured: RTX 2050 58 s vs CPU 18 s): only real GPUs
    let gpu = crate::router::specs().vram_gb >= 6.0;
    let s = start(app, gpu, kind).or_else(|| if gpu { start(app, false, kind) } else { None })?;
    crate::log_line(&format!("{} open in {} ms ({})", if kind == "pointer" { "pointer (UI-TARS)" } else { "eyes" }, t0.elapsed().as_millis(), if gpu { "GPU" } else { "CPU" }));
    let p = s.1;
    *g = Some((s.0, s.1, kind));
    Some(p)
}

/// A window's picture as a JPEG, scaled so the model reads it quickly (longest side 672 px).
pub fn snapshot(h: HWND) -> Option<Vec<u8>> {
    let mut r = RECT::default();
    unsafe { GetWindowRect(h, &mut r).ok()? };
    let (w, ht) = ((r.right - r.left) as u32, (r.bottom - r.top) as u32);
    let px = crate::vision::grab(r)?;
    let mut rgb = Vec::with_capacity((w * ht * 3) as usize);
    for p in px.chunks_exact(4) {
        rgb.extend_from_slice(&[p[2], p[1], p[0]]);
    }
    let img = image::RgbImage::from_raw(w, ht, rgb)?;
    let k = 672.0 / w.max(ht) as f64;
    let img = if k < 1.0 { image::imageops::resize(&img, (w as f64 * k) as u32, (ht as f64 * k) as u32, image::imageops::FilterType::Triangle) } else { img };
    let mut out = std::io::Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85).encode_image(&img).ok()?;
    Some(out.into_inner())
}

/// Ask the eyes about a picture.
pub fn ask(app: &AppHandle, jpeg: &[u8], question: &str) -> Option<String> {
    let port = port(app, "eyes")?;
    *LAST.lock().unwrap() = Some(Instant::now());
    let t0 = Instant::now();
    let url = format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(jpeg));
    let body = serde_json::json!({
        "messages": [
            { "role": "system", "content": "You are ULTRON, looking at a screenshot of the user's screen. Answer in plain spoken English, briefly and accurately. Only describe what is actually visible." },
            { "role": "user", "content": [ { "type": "image_url", "image_url": { "url": url } }, { "type": "text", "text": question } ] }
        ],
        "max_tokens": 280,
        "temperature": 0.2
    });
    let v: serde_json::Value = ureq::post(&format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .timeout(Duration::from_secs(120))
        .send_json(body)
        .ok()?
        .into_json()
        .ok()?;
    *LAST.lock().unwrap() = Some(Instant::now());
    let text = v["choices"][0]["message"]["content"].as_str()?.trim().to_string();
    crate::log_line(&format!("eyes answered in {} ms", t0.elapsed().as_millis()));
    (!text.is_empty()).then_some(text)
}

/// The longest side of the picture when UI-TARS is shown a whole window. Bigger is slower, and on a
/// processor much slower: measured on this PC with a full-size window, 896 px took 24 s and 672 px 14 s,
/// with as many right answers once the spot is snapped to what's there.
const POINT_SIDE: f64 = 672.0;
/// one cut-out on the sheet of icons (see `pick`)
const TILE: u32 = 72;

/// Two numbers out of UI-TARS's answer: `click(start_box='<|box_start|>(235,512)<|box_end|>')`, "(235,512)",
/// "<point>235 512</point>" or "[235, 512]" all mean the same place.
pub fn numbers(answer: &str) -> Option<(f64, f64)> {
    let at = answer.rfind("Action:").map(|i| &answer[i..]).unwrap_or(answer);
    let mut n = at.split(|c: char| !c.is_ascii_digit() && c != '.').filter(|t| !t.is_empty() && t.chars().any(|c| c.is_ascii_digit())).filter_map(|t| t.trim_matches('.').parse::<f64>().ok());
    Some((n.next()?, n.next()?))
}

/// Show UI-TARS a picture and ask it to point at something. Its answer is a spot in thousandths of the
/// picture (0 to 1000 each way), whatever the picture's size.
fn ask_point(app: &AppHandle, img: &image::RgbImage, what: &str, kind: &str) -> Option<(f64, f64)> {
    let port = port(app, "pointer")?;
    *LAST.lock().unwrap() = Some(Instant::now());
    let t0 = Instant::now();
    let mut jpeg = std::io::Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 92).encode_image(img).ok()?;
    let url = format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(jpeg.into_inner()));
    // the words UI-TARS was trained with, cut down to the one action that's wanted
    let task = format!("You are a GUI agent. You are given a task and your action history, with screenshots. You need to perform the next action to complete the task.\n\n## Output Format\n```\nAction: ...\n```\n\n## Action Space\nclick(start_box='<|box_start|>(x1,y1)<|box_end|>')\n\n## User Instruction\nClick on {what}.");
    let body = serde_json::json!({
        "messages": [{ "role": "user", "content": [ { "type": "text", "text": task }, { "type": "image_url", "image_url": { "url": url } } ] }],
        "max_tokens": 48,
        "temperature": 0.0
    });
    let v: serde_json::Value = ureq::post(&format!("http://127.0.0.1:{port}/v1/chat/completions")).timeout(Duration::from_secs(150)).send_json(body).ok()?.into_json().ok()?;
    *LAST.lock().unwrap() = Some(Instant::now());
    let said = v["choices"][0]["message"]["content"].as_str()?.trim().to_string();
    let Some((x, y)) = numbers(&said).filter(|(x, y)| (0.0..=1000.0).contains(x) && (0.0..=1000.0).contains(y)) else {
        crate::log_line(&format!("pointer: an answer I couldn't use: {}", said.chars().take(80).collect::<String>()));
        return None;
    };
    let t = |k: &str| v["timings"][k].as_f64().unwrap_or(0.0);
    crate::log_line(&format!("pointer: UI-TARS looked at {kind} ({}x{}) and pointed at ({x:.0}, {y:.0}) of 1000 in {} ms (read {:.0} tokens in {:.1} s)", img.width(), img.height(), t0.elapsed().as_millis(), t("prompt_n"), t("prompt_ms") / 1000.0));
    Some((x, y))
}

/// The quick way to find something by its looks ("the gear icon", "the red bin"), when the icon detector
/// has already boxed everything pressable: each box is cut out of the window's picture with a little of
/// its surroundings, the cut-outs are laid side by side on one small sheet, and UI-TARS points at the one
/// that's meant. A sheet of a dozen icons is a picture a tenth the size of the window, and reading time
/// follows size: measured here, about 4 s against 14 to 24 for the whole window, and more often right
/// (each icon is seen at its real size instead of shrunk). Returns which box, counted from 0.
pub fn pick(app: &AppHandle, bgra: &[u8], w: u32, h: u32, boxes: &[RECT], what: &str) -> Option<usize> {
    if boxes.len() < 2 || boxes.len() > 48 || bgra.len() < (w * h * 4) as usize {
        return None;
    }
    let rgb: Vec<u8> = bgra.chunks_exact(4).take((w * h) as usize).flat_map(|p| [p[2], p[1], p[0]]).collect();
    let img = image::RgbImage::from_raw(w, h, rgb)?;
    let cols = ((boxes.len() as f64 * 4.0 / 3.0).sqrt().ceil() as u32).max(2);
    let rows = (boxes.len() as u32).div_ceil(cols);
    let mut sheet = image::RgbImage::from_pixel(cols * TILE, rows * TILE, image::Rgb([128, 128, 128]));
    for (i, b) in boxes.iter().enumerate() {
        // a square around the thing, with a margin so it's seen in its setting
        let side = (((b.right - b.left).max(b.bottom - b.top) + 24).max(24) as u32).min(w).min(h);
        let (cx, cy) = ((b.left + b.right) / 2, (b.top + b.bottom) / 2);
        let x0 = (cx - side as i32 / 2).clamp(0, (w - side) as i32) as u32;
        let y0 = (cy - side as i32 / 2).clamp(0, (h - side) as i32) as u32;
        let cut = image::imageops::crop_imm(&img, x0, y0, side, side).to_image();
        let tile = image::imageops::resize(&cut, TILE - 4, TILE - 4, image::imageops::FilterType::CatmullRom);
        image::imageops::replace(&mut sheet, &tile, ((i as u32 % cols) * TILE + 2) as i64, ((i as u32 / cols) * TILE + 2) as i64);
    }
    let (x, y) = ask_point(app, &sheet, what, &format!("a sheet of {} cut-outs", boxes.len()))?;
    let i = ((y / 1000.0 * rows as f64) as u32).min(rows - 1) * cols + ((x / 1000.0 * cols as f64) as u32).min(cols - 1);
    ((i as usize) < boxes.len()).then_some(i as usize)
}

/// Ask UI-TARS where something is in a whole window: "the search box", "the banner at the top".
/// Returns the spot in screen pixels.
pub fn point(app: &AppHandle, h: HWND, what: &str) -> Option<(i32, i32)> {
    let mut r = RECT::default();
    unsafe { GetWindowRect(h, &mut r).ok()? };
    let (w, ht) = ((r.right - r.left) as u32, (r.bottom - r.top) as u32);
    let px = crate::vision::grab(r)?;
    let rgb: Vec<u8> = px.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
    let img = image::RgbImage::from_raw(w, ht, rgb)?;
    let k = POINT_SIDE / w.max(ht) as f64;
    let img = if k < 1.0 { image::imageops::resize(&img, (w as f64 * k) as u32, (ht as f64 * k) as u32, image::imageops::FilterType::CatmullRom) } else { img };
    let (x, y) = ask_point(app, &img, what, "the whole window")?;
    Some((r.left + (x / 1000.0 * w as f64).round() as i32, r.top + (y / 1000.0 * ht as f64).round() as i32))
}

/// Close the eyes after a few idle minutes.
pub fn watch() {
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_secs(30));
        let idle = LAST.lock().unwrap().map(|t| t.elapsed() > IDLE).unwrap_or(true);
        if idle {
            shutdown();
        }
    });
}

pub fn shutdown() {
    if let Ok(mut g) = SERVER.try_lock() {
        if let Some((mut c, ..)) = g.take() {
            let _ = c.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_a_point() {
        for (a, want) in [
            ("Action: click(start_box='<|box_start|>(235,512)<|box_end|>')", Some((235.0, 512.0))),
            ("Thought: The gear is at 3 o'clock.\nAction: click(start_box='(871,64)')", Some((871.0, 64.0))),
            ("(12, 990)", Some((12.0, 990.0))),
            ("<point>402 77</point>", Some((402.0, 77.0))),
            ("I can't see it.", None),
        ] {
            assert_eq!(super::numbers(a), want, "{a}");
        }
    }
}
