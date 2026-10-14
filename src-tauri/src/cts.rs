//! Circle to Search (AI Tools, On screen). The screen is held still as a picture; you draw round a
//! part of it, and that part is searched: its words are read here (Windows' own offline text
//! reading), ULTRON's eyes can say what it is (the local vision model, see.rs), and it can be looked
//! up on the web or in Google Lens. Only Lens sends anything off the PC (the part you circled), and
//! only when you ask for it.
//!
//! The picture is kept here, in memory, while the search is open (`grab` .. `forget`); everything
//! afterwards works on pieces of that, not on the screen as it is by then.

use std::sync::Mutex;
use std::time::Duration;

use base64::Engine;
use tauri::AppHandle;
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTOPRIMARY};

/// The screen as it was: width, height, BGRA pixels with the top row first.
static SHOT: Mutex<Option<(u32, u32, Vec<u8>)>> = Mutex::new(None);

/// Take the main screen as it is now; given back as a picture for the page to show.
pub fn grab() -> Option<String> {
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    unsafe {
        let mon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return None;
        }
    }
    let r = mi.rcMonitor;
    let (w, h) = ((r.right - r.left) as u32, (r.bottom - r.top) as u32);
    let px = crate::vision::grab(r)?;
    let jpg = jpeg(&px, w, h, None, 90)?;
    *SHOT.lock().unwrap() = Some((w, h, px));
    Some(format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(jpg)))
}

pub fn forget() {
    *SHOT.lock().unwrap() = None;
}

/// A piece of the kept picture (in its own pixels), cut to fit inside it.
fn piece(x: i32, y: i32, w: i32, h: i32) -> Option<(u32, u32, Vec<u8>)> {
    let g = SHOT.lock().unwrap();
    let (sw, sh, px) = g.as_ref()?;
    let (sw, sh) = (*sw as i32, *sh as i32);
    let (x0, y0) = (x.clamp(0, sw - 1), y.clamp(0, sh - 1));
    let (x1, y1) = ((x + w).clamp(x0 + 1, sw), (y + h).clamp(y0 + 1, sh));
    let (pw, ph) = ((x1 - x0) as usize, (y1 - y0) as usize);
    let mut out = Vec::with_capacity(pw * ph * 4);
    for row in y0..y1 {
        let s = ((row * sw + x0) * 4) as usize;
        out.extend_from_slice(&px[s..s + pw * 4]);
    }
    Some((pw as u32, ph as u32, out))
}

/// BGRA pixels as a JPEG (made smaller first, if `longest` says how long its longest side may be).
fn jpeg(bgra: &[u8], w: u32, h: u32, longest: Option<u32>, quality: u8) -> Option<Vec<u8>> {
    let rgb: Vec<u8> = bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
    let mut img = image::RgbImage::from_raw(w, h, rgb)?;
    if let Some(m) = longest {
        if w.max(h) > m {
            let k = m as f64 / w.max(h) as f64;
            img = image::imageops::resize(&img, ((w as f64 * k) as u32).max(1), ((h as f64 * k) as u32).max(1), image::imageops::FilterType::Triangle);
        }
    }
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality).encode_image(&img).ok()?;
    Some(out)
}

/// The words in a piece, line by line.
pub fn read(x: i32, y: i32, w: i32, h: i32) -> Vec<String> {
    let Some((pw, ph, px)) = piece(x, y, w, h) else { return Vec::new() };
    let (pw, ph) = (pw as usize, ph as usize);
    let mut gray: Vec<u8> = px.chunks_exact(4).map(|p| ((p[2] as u32 * 77 + p[1] as u32 * 150 + p[0] as u32 * 29) >> 8) as u8).collect();
    // (the reader likes dark writing on a light page)
    let mean = gray.iter().map(|&v| v as u64).sum::<u64>() / gray.len().max(1) as u64;
    if mean < 110 {
        gray.iter_mut().for_each(|v| *v = 255 - *v);
    }
    let max = windows::Media::Ocr::OcrEngine::MaxImageDimension().unwrap_or(4096) as usize;
    if pw > max || ph > max {
        return Vec::new();
    }
    // (small writing is read better doubled)
    let z = if pw * 2 <= max && ph * 2 <= max && pw.max(ph) < 1400 { 2 } else { 1 };
    let img = if z == 2 {
        let mut big = vec![0u8; pw * ph * 4];
        for yy in 0..ph {
            for xx in 0..pw {
                let v = gray[yy * pw + xx];
                let i = (yy * 2) * (pw * 2) + xx * 2;
                big[i] = v;
                big[i + 1] = v;
                big[i + pw * 2] = v;
                big[i + pw * 2 + 1] = v;
            }
        }
        big
    } else {
        gray
    };
    crate::vision::recognize(&img, (pw * z) as u32, (ph * z) as u32)
        .unwrap_or_default()
        .into_iter()
        .map(|l| l.text.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// What ULTRON's eyes make of a piece.
pub fn ask(app: &AppHandle, x: i32, y: i32, w: i32, h: i32, question: &str) -> Result<String, String> {
    if !crate::see::available(app) {
        return Err("ULTRON’s eyes (the vision model) aren’t on this PC yet, so it can only read the words here.".into());
    }
    let (pw, ph, px) = piece(x, y, w, h).ok_or("The picture has gone. Draw round it again.")?;
    let jpg = jpeg(&px, pw, ph, Some(896), 88).ok_or("That part couldn’t be pictured.")?;
    crate::see::ask(app, &jpg, question).ok_or_else(|| "ULTRON couldn’t make it out this time.".into())
}

/// Look the circled part up in Google Lens. Google keeps an uploaded picture with the connection that sent it:
/// sent from here, your browser opened its results with an empty, broken picture (and Google refuses an upload
/// posted from any page but its own). So it's done the way you would: the part is already on the clipboard
/// (`copy`, before the search closes), Google's own "Search any image" box is opened in your browser, and once
/// that page is up and in front, the picture is pasted into it. Google then uploads it in your own session.
/// "pasted", or "opened" when the page didn't come up in time (then Ctrl V there does it), or "failed".
pub fn lens_open() -> &'static str {
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_CONTROL, VK_V};
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW};
    if !crate::apps::shell_open("https://www.google.com/?olud") {
        return "failed";
    }
    let title = || unsafe {
        let fg = GetForegroundWindow();
        let mut buf = [0u16; 300];
        let n = GetWindowTextW(fg, &mut buf).max(0) as usize;
        (fg, String::from_utf16_lossy(&buf[..n]))
    };
    // the page's own title is "Google"; browsers add their name after it ("Google - Google Chrome")
    let is_page = |t: &str| {
        let t = t.trim();
        t == "Google" || ["Google - ", "Google – ", "Google — ", "Google and "].iter().any(|p| t.starts_with(p))
    };
    let t0 = std::time::Instant::now();
    let mut since: Option<(windows::Win32::Foundation::HWND, std::time::Instant)> = None;
    while t0.elapsed() < Duration::from_secs(12) {
        std::thread::sleep(Duration::from_millis(120));
        let (fg, t) = title();
        if !is_page(&t) {
            since = None;
            continue;
        }
        match since {
            Some((h, at)) if h == fg => {
                // (a moment for the page to open its picture box, which takes a paste anywhere on it)
                if at.elapsed() >= Duration::from_millis(1300) {
                    crate::apps::send_keys(&[VK_CONTROL, VK_V]);
                    return "pasted";
                }
            }
            _ => since = Some((fg, std::time::Instant::now())),
        }
    }
    "opened"
}

/// Put a piece on the clipboard as a picture.
pub fn copy(x: i32, y: i32, w: i32, h: i32) -> bool {
    piece(x, y, w, h).is_some_and(|(pw, ph, px)| crate::clip::set_image(pw, ph, &px))
}
