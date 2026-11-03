//! Finding the buttons and icons in a picture of a window, for apps that don't describe themselves
//! (a toolbar of unlabelled icons, a game launcher, a music player that hides its controls from
//! other programs). This is OmniParser's icon detector: a YOLO model that marks everything that
//! looks pressable. It runs on the processor in about a third of a second for a whole window.
//!
//! Its file is `resources/screen/omni-icon-detect.onnx`. Without it SYLVER still works: it then
//! knows only what the app says about itself and the text it can read.
//! (The detector's licence is AGPL-3.0, from the YOLO family it's built on.)

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use tauri::{AppHandle, Manager};
use windows::Win32::Foundation::RECT;

/// the picture the detector takes: 640 × 640
const SIDE: usize = 640;
/// how sure it has to be (OmniParser itself goes as low as 0.05 and then weeds out; this is already tidy)
const SURE: f32 = 0.12;

static DETECTOR: Mutex<Option<Session>> = Mutex::new(None);
static TRIED: AtomicBool = AtomicBool::new(false);

fn file(app: &AppHandle) -> Option<PathBuf> {
    let mut bases = Vec::new();
    if let Ok(r) = app.path().resource_dir() {
        bases.push(r.join("resources"));
        bases.push(r);
    }
    bases.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources"));
    if let Ok(d) = app.path().app_data_dir() {
        bases.push(d);
    }
    bases.into_iter().map(|b| b.join("screen").join("omni-icon-detect.onnx")).find(|p| p.exists())
}

pub fn available(app: &AppHandle) -> bool {
    file(app).is_some()
}

fn load(path: &std::path::Path) -> Option<Session> {
    Session::builder().ok()?.with_optimization_level(GraphOptimizationLevel::Level3).ok()?.with_intra_threads(4).ok()?.commit_from_file(path).ok()
}

fn overlap(a: &RECT, b: &RECT) -> f32 {
    let (w, h) = ((a.right.min(b.right) - a.left.max(b.left)).max(0), (a.bottom.min(b.bottom) - a.top.max(b.top)).max(0));
    let inter = (w * h) as f32;
    let area = |r: &RECT| ((r.right - r.left) * (r.bottom - r.top)).max(1) as f32;
    inter / (area(a) + area(b) - inter)
}

/// The detector's raw answer ([4 + 1] × 8400: centre x, centre y, width, height, how sure) as boxes
/// in the picture's own pixels, surest first, with doubles of the same thing removed.
fn boxes(out: &[f32], scale: f32, pad: (f32, f32), w: usize, h: usize) -> Vec<(RECT, f32)> {
    let n = out.len() / 5;
    let mut found: Vec<(RECT, f32)> = (0..n)
        .filter(|i| out[4 * n + i] >= SURE)
        .map(|i| {
            let (cx, cy, bw, bh) = (out[i], out[n + i], out[2 * n + i], out[3 * n + i]);
            let x = |v: f32| (((v - pad.0) / scale).round() as i32).clamp(0, w as i32);
            let y = |v: f32| (((v - pad.1) / scale).round() as i32).clamp(0, h as i32);
            (RECT { left: x(cx - bw / 2.0), top: y(cy - bh / 2.0), right: x(cx + bw / 2.0), bottom: y(cy + bh / 2.0) }, out[4 * n + i])
        })
        .filter(|(r, _)| r.right - r.left >= 6 && r.bottom - r.top >= 6)
        .collect();
    found.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut kept: Vec<(RECT, f32)> = Vec::new();
    for f in found {
        if kept.len() < 160 && !kept.iter().any(|k| overlap(&k.0, &f.0) > 0.3) {
            kept.push(f);
        }
    }
    kept
}

/// Everything that looks pressable in a picture (BGRA, `w` × `h`): boxes in picture pixels with how
/// sure the detector is, surest first. Empty if the detector isn't on this PC.
pub fn find(app: &AppHandle, bgra: &[u8], w: usize, h: usize) -> Vec<(RECT, f32)> {
    if w < 8 || h < 8 || bgra.len() < w * h * 4 {
        return Vec::new();
    }
    let mut g = DETECTOR.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() && !TRIED.swap(true, Ordering::SeqCst) {
        let t0 = std::time::Instant::now();
        *g = file(app).and_then(|p| load(&p));
        crate::log_line(&match g.is_some() {
            true => format!("screen: icon detector ready in {} ms", t0.elapsed().as_millis()),
            false => "screen: no icon detector on this PC (apps are read by what they say about themselves and their text)".to_string(),
        });
    }
    let Some(session) = g.as_mut() else { return Vec::new() };
    let t0 = std::time::Instant::now();
    // the whole picture, shrunk to fit a 640 square with grey around it (how the detector was trained)
    let rgb: Vec<u8> = bgra.chunks_exact(4).take(w * h).flat_map(|p| [p[2], p[1], p[0]]).collect();
    let Some(img) = image::RgbImage::from_raw(w as u32, h as u32, rgb) else { return Vec::new() };
    let scale = (SIDE as f32 / w as f32).min(SIDE as f32 / h as f32);
    let (nw, nh) = (((w as f32 * scale).round() as usize).clamp(1, SIDE), ((h as f32 * scale).round() as usize).clamp(1, SIDE));
    let small = image::imageops::resize(&img, nw as u32, nh as u32, image::imageops::FilterType::Triangle);
    let (px, py) = ((SIDE - nw) / 2, (SIDE - nh) / 2);
    let mut input = vec![114.0f32 / 255.0; 3 * SIDE * SIDE];
    for (x, y, p) in small.enumerate_pixels() {
        let at = (y as usize + py) * SIDE + x as usize + px;
        for c in 0..3 {
            input[c * SIDE * SIDE + at] = p.0[c] as f32 / 255.0;
        }
    }
    let Ok(tensor) = Tensor::from_array(([1usize, 3, SIDE, SIDE], input)) else { return Vec::new() };
    let out: Vec<f32> = {
        let Ok(outputs) = session.run(ort::inputs!["images" => tensor]) else { return Vec::new() };
        let Ok((_, y)) = outputs["output0"].try_extract_tensor::<f32>() else { return Vec::new() };
        y.to_vec()
    };
    let found = boxes(&out, scale, (px as f32, py as f32), w, h);
    crate::log_line(&format!("screen: the icon detector marked {} things in {} ms", found.len(), t0.elapsed().as_millis()));
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_detector_answer() {
        // three candidates: a sure one, its near-double (dropped), and an unsure one (dropped)
        let n = 3;
        let mut out = vec![0f32; 5 * n];
        let set = |out: &mut Vec<f32>, i: usize, v: [f32; 5]| (0..5).for_each(|k| out[k * n + i] = v[k]);
        set(&mut out, 0, [320.0, 320.0, 40.0, 40.0, 0.9]);
        set(&mut out, 1, [322.0, 321.0, 40.0, 40.0, 0.6]);
        set(&mut out, 2, [100.0, 500.0, 30.0, 30.0, 0.05]);
        // a 1280 × 720 picture: shrunk by half, with 140 px of grey above and below
        let b = boxes(&out, 0.5, (0.0, 140.0), 1280, 720);
        assert_eq!(b.len(), 1);
        assert_eq!((b[0].0.left, b[0].0.top, b[0].0.right, b[0].0.bottom), (600, 320, 680, 400));
        assert!(overlap(&b[0].0, &b[0].0) > 0.99);
    }
}
