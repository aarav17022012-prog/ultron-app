//! Spatial wallpaper: the wallpaper as a scene with depth. Depth Anything V2 (Small, on the PC, about a second and a
//! half once per wallpaper) works out how near each part of the picture is; the page then draws the picture moving
//! as a real scene would when you look at it from a little to one side (each part shifted by its depth), following the
//! pointer and drifting slowly on its own. On the lock screen the nearest part of the picture comes in front of the
//! clock; on the desktop it's drawn by a window of ULTRON's behind your icons, where Windows draws the wallpaper.
//!
//! The depth map is kept on disk beside the models, one per wallpaper, so it's worked out only when the wallpaper
//! changes. The model is fetched once (about 100 MB) the first time Spatial is turned on.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Instant, SystemTime};

use base64::Engine;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowExW, FindWindowW, GetClassNameW, GetCursorPos, GetForegroundWindow, GetSystemMetrics, GetWindowLongPtrW, GetWindowRect, IsZoomed,
    SendMessageTimeoutW, SetParent, SetWindowLongPtrW, SetWindowPos, GWL_STYLE, SMTO_NORMAL, SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOZORDER,
    SWP_SHOWWINDOW, WS_CAPTION, WS_CHILD, WS_POPUP, WS_THICKFRAME,
};

const MODEL: &str = "depth-anything-v2-small.onnx";
const MODEL_URL: &str = "https://huggingface.co/onnx-community/depth-anything-v2-small/resolve/main/onnx/model.onnx";
const LABEL: &str = "spatialwall";
/// (the wallpaper's depth, as a picture: the wallpaper's file and when it last changed, and the map)
static LAST: Mutex<Option<(PathBuf, u64, String)>> = Mutex::new(None);

fn models(app: &AppHandle) -> PathBuf {
    let dir = app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir()).join("models");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// The model, fetched the first time it's wanted
fn model(app: &AppHandle) -> Result<PathBuf, String> {
    let p = models(app).join(MODEL);
    if p.exists() {
        return Ok(p);
    }
    crate::log_line("spatial: fetching the depth model (about 100 MB, once)");
    let part = p.with_extension("part");
    let mut body = crate::net::agent(std::time::Duration::from_secs(600)).get(MODEL_URL).call().map_err(|e| e.to_string())?.into_reader();
    let mut f = std::fs::File::create(&part).map_err(|e| e.to_string())?;
    std::io::copy(&mut body, &mut f).map_err(|e| e.to_string())?;
    drop(f);
    std::fs::rename(&part, &p).map_err(|e| e.to_string())?;
    Ok(p)
}

/// How near each part of a picture is, as a greyscale PNG (white near, black far), softened a touch so the scene
/// doesn't tear at the edges of things when it moves
fn depth_png(app: &AppHandle, picture: &PathBuf) -> Result<Vec<u8>, String> {
    let t0 = Instant::now();
    // (Windows keeps the wallpaper as a file with no extension: its kind is read from what's in it; failing that, the
    //  picture ULTRON already makes of the wallpaper for its other uses)
    let img = image::ImageReader::open(picture).ok().and_then(|r| r.with_guessed_format().ok()).and_then(|r| r.decode().ok())
        .or_else(|| {
            let url = crate::wall::picture(1920, 1080)?;
            let b64 = url.split_once(",")?.1;
            image::load_from_memory(&base64::engine::general_purpose::STANDARD.decode(b64).ok()?).ok()
        })
        .ok_or("can't read the wallpaper picture")?
        .to_rgb8();
    let (w0, h0) = img.dimensions();
    // (the model sees the picture 518 high, its width in proportion; both a multiple of 14, as it wants)
    let h = 518u32;
    let w = (((w0 as f32 / h0 as f32) * h as f32 / 14.0).round() as u32 * 14).clamp(14 * 20, 14 * 74);
    let small = image::imageops::resize(&img, w, h, image::imageops::FilterType::Triangle);
    let (mean, std) = ([0.485f32, 0.456, 0.406], [0.229f32, 0.224, 0.225]);
    let plane = (w * h) as usize;
    let mut input = vec![0f32; 3 * plane];
    for (i, p) in small.pixels().enumerate() {
        for c in 0..3 {
            input[c * plane + i] = (p[c] as f32 / 255.0 - mean[c]) / std[c];
        }
    }
    let mut session = Session::builder().map_err(|e| e.to_string())?.with_optimization_level(GraphOptimizationLevel::Level3).map_err(|e| e.to_string())?
        .with_intra_threads(4).map_err(|e| e.to_string())?.commit_from_file(model(app)?).map_err(|e| e.to_string())?;
    let tensor = Tensor::from_array(([1usize, 3, h as usize, w as usize], input)).map_err(|e| e.to_string())?;
    let outputs = session.run(ort::inputs!["pixel_values" => tensor]).map_err(|e| e.to_string())?;
    let (_, d) = outputs["predicted_depth"].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
    let (lo, hi) = d.iter().fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    let span = (hi - lo).max(1e-6);
    let grey: Vec<u8> = d.iter().map(|v| (((v - lo) / span) * 255.0).round() as u8).collect();
    let map = image::GrayImage::from_raw(w, h, grey).ok_or("the depth came back the wrong size")?;
    let map = image::imageops::blur(&map, 1.2);
    let mut out = std::io::Cursor::new(Vec::new());
    map.write_to(&mut out, image::ImageFormat::Png).map_err(|e| e.to_string())?;
    crate::log_line(&format!("spatial: the wallpaper's depth worked out in {} ms ({w}x{h})", t0.elapsed().as_millis()));
    Ok(out.into_inner())
}

/// The wallpaper's depth map as an image address for the page (worked out once per wallpaper, kept on disk)
pub fn depth(app: &AppHandle) -> Result<String, String> {
    let pic = crate::sys::wallpaper_path().ok_or("no wallpaper picture to give depth to")?;
    let stamp = std::fs::metadata(&pic).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
    if let Some((p, s, url)) = LAST.lock().unwrap().as_ref() {
        if *p == pic && *s == stamp {
            return Ok(url.clone());
        }
    }
    let name = format!("depth-{:x}.png", stamp ^ pic.to_string_lossy().bytes().fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(b as u64)));
    let cached = models(app).join("spatial").join(name);
    let png = match std::fs::read(&cached) {
        Ok(b) => b,
        Err(_) => {
            let b = depth_png(app, &pic)?;
            let _ = std::fs::create_dir_all(cached.parent().unwrap());
            let _ = std::fs::write(&cached, &b);
            b
        }
    };
    let url = format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png));
    *LAST.lock().unwrap() = Some((pic, stamp, url.clone()));
    Ok(url)
}

/* ---------------- the desktop: a window of ULTRON's where Windows draws the wallpaper ---------------- */

unsafe extern "system" fn holder(h: HWND, lp: LPARAM) -> windows::core::BOOL {
    unsafe {
        if FindWindowExW(Some(h), None, w!("SHELLDLL_DefView"), PCWSTR::null()).is_ok() {
            if let Ok(w) = FindWindowExW(None, Some(h), w!("WorkerW"), PCWSTR::null()) {
                *(lp.0 as *mut isize) = w.0 as isize;
            }
        }
        true.into()
    }
}

/// The window Windows draws the wallpaper in, behind the icons (asked to make one if it hasn't)
unsafe fn wallpaper_layer() -> Option<HWND> {
    unsafe {
        let progman = FindWindowW(w!("Progman"), PCWSTR::null()).ok()?;
        let mut r = 0usize;
        let _ = SendMessageTimeoutW(progman, 0x052C, WPARAM(0xD), LPARAM(1), SMTO_NORMAL, 1000, Some(&mut r as *mut usize));
        // (Windows 11 24H2 and later: it's inside Progman; before, the WorkerW after the one holding the icons)
        if let Ok(w) = FindWindowExW(Some(progman), None, w!("WorkerW"), PCWSTR::null()) {
            return Some(w);
        }
        let mut found = 0isize;
        let _ = EnumWindows(Some(holder), LPARAM(&mut found as *mut isize as isize));
        (found != 0).then_some(HWND(found as *mut _))
    }
}

/// The desktop's spatial wallpaper, on or off
pub fn wall(app: &AppHandle, on: bool) -> Result<(), String> {
    if !on {
        if let Some(w) = app.get_webview_window(LABEL) {
            let _ = w.destroy();
            crate::log_line("spatial: desktop wallpaper back to Windows'");
        }
        return Ok(());
    }
    if app.get_webview_window(LABEL).is_some() {
        return Ok(());
    }
    let layer = unsafe { wallpaper_layer() }.ok_or("Windows' wallpaper layer couldn't be found")?;
    let win = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("spatial.html".into()))
        .title("Sylver wallpaper")
        .visible(false)
        .focused(false)
        .skip_taskbar(true)
        .decorations(false)
        .shadow(false)
        .additional_browser_args(crate::BROWSER_ARGS)
        .build()
        .map_err(|e| e.to_string())?;
    let h = HWND(win.hwnd().map_err(|e| e.to_string())?.0 as _);
    unsafe {
        let style = GetWindowLongPtrW(h, GWL_STYLE);
        SetWindowLongPtrW(h, GWL_STYLE, (style & !((WS_POPUP.0 | WS_CAPTION.0 | WS_THICKFRAME.0) as isize)) | WS_CHILD.0 as isize);
        SetParent(h, Some(layer)).map_err(|e| e.to_string())?;
        // (placed over the main screen, in the layer's own coordinates)
        let mut lr = RECT::default();
        let _ = GetWindowRect(layer, &mut lr);
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        SetWindowPos(h, None, -lr.left, -lr.top, sw, sh, SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW).map_err(|e| e.to_string())?;
    }
    crate::log_line("spatial: desktop wallpaper drawn by ULTRON, behind the icons");
    Ok(())
}

/// Where the pointer is on the main screen (0-1 across and down), and whether the desktop can be seen at all (when
/// a window fills the screen the wallpaper page stops drawing)
pub fn cursor() -> (f64, f64, bool) {
    unsafe {
        let mut p = POINT::default();
        let _ = GetCursorPos(&mut p);
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN).max(1) as f64, GetSystemMetrics(SM_CYSCREEN).max(1) as f64);
        let fg = GetForegroundWindow();
        let mut cls = [0u16; 32];
        let n = GetClassNameW(fg, &mut cls);
        let cls = String::from_utf16_lossy(&cls[..n.max(0) as usize]);
        let desk = matches!(cls.as_str(), "Progman" | "WorkerW") || !(IsZoomed(fg).as_bool() || crate::appbar::really_full());
        (p.x as f64 / sw, p.y as f64 / sh, desk)
    }
}
