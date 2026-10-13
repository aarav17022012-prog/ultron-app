//! Files, beside Record in the bar: photos, videos and anything else made much smaller.
//!
//! A photo is saved again as a JPEG (scaled down first when it's huge), a video is encoded again by
//! Windows' own transcoder (on the graphics card where it can), anything else is zipped. The file you
//! chose is never touched: the small copy goes to Documents › ULTRON Files › Compressed, and when a
//! copy wouldn't come out smaller none is kept.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};

static QUEUE: Mutex<VecDeque<(u32, PathBuf, bool)>> = Mutex::new(VecDeque::new());
static BUSY: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);

pub fn folder(app: &AppHandle) -> PathBuf {
    let d = app.path().document_dir().unwrap_or_else(|_| crate::skills::known_folder("documents")).join("ULTRON Files").join("Compressed");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Windows' own "Open" box, for several files at once. It belongs to ULTRON's window so that it
/// comes up in front. Empty when it was closed without choosing.
pub fn pick(owner: Option<isize>) -> Vec<String> {
    use windows::core::w;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, SIGDN_FILESYSPATH};
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let mut out = Vec::new();
        let _ = (|| -> windows::core::Result<()> {
            let d: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
            d.SetOptions(d.GetOptions()? | FOS_ALLOWMULTISELECT | FOS_FORCEFILESYSTEM | FOS_FILEMUSTEXIST)?;
            d.SetTitle(w!("Choose photos, videos or files to make smaller"))?;
            d.SetOkButtonLabel(w!("Shrink"))?;
            d.Show(owner.map(|h| HWND(h as _)))?; // (closing the box without choosing ends here)
            let items = d.GetResults()?;
            for i in 0..items.GetCount()? {
                let p = items.GetItemAt(i)?.GetDisplayName(SIGDN_FILESYSPATH)?;
                out.push(p.to_string().unwrap_or_default());
                CoTaskMemFree(Some(p.0 as _));
            }
            Ok(())
        })();
        CoUninitialize();
        out.retain(|p| !p.is_empty());
        out
    })
    .join()
    .unwrap_or_default()
}

/// Add files to the line. They're done one at a time, and each says how it's going with a "squeeze" event.
pub fn go(app: &AppHandle, items: Vec<(u32, String)>, tiny: bool) {
    STOP.store(false, SeqCst);
    let start = {
        let mut q = QUEUE.lock().unwrap();
        q.extend(items.into_iter().map(|(id, p)| (id, PathBuf::from(p), tiny)));
        !BUSY.swap(true, SeqCst)
    };
    if !start {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let dir = folder(&app);
        loop {
            let next = {
                let mut q = QUEUE.lock().unwrap();
                let n = q.pop_front();
                if n.is_none() {
                    BUSY.store(false, SeqCst);
                }
                n
            };
            let Some((id, path, tiny)) = next else { break };
            let from = len(&path);
            let kind = kind_of(&path);
            let _ = app.emit("squeeze", json!({"id": id, "s": "run", "kind": kind, "pct": -1}));
            let res = if from == 0 {
                Err("That file is empty or gone".to_string())
            } else {
                // (a damaged picture mustn't end the line for the files after it)
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match kind {
                    "photo" => photo(&path, &dir, tiny, from),
                    "video" => video(&app, id, &path, &dir, tiny, from),
                    _ => zip(&path, &dir, from),
                }))
                .unwrap_or_else(|_| Err("This file couldn't be read".to_string()))
            };
            let said = match res {
                Ok(Some(to)) => json!({"id": id, "s": "done", "from": from, "to": len(&to), "path": to.to_string_lossy()}),
                Ok(None) => json!({"id": id, "s": "same"}),
                Err(why) if why.is_empty() => json!({"id": id, "s": "stop"}),
                Err(why) => {
                    crate::log_line(&format!("files: a {kind} wasn't made smaller: {why}"));
                    json!({"id": id, "s": "fail", "why": why})
                }
            };
            let _ = app.emit("squeeze", said);
        }
    });
}

/// Stop: the video being encoded is given up, and nothing still waiting is started.
pub fn stop(app: &AppHandle) {
    STOP.store(true, SeqCst);
    let left: Vec<u32> = QUEUE.lock().unwrap().drain(..).map(|j| j.0).collect();
    for id in left {
        let _ = app.emit("squeeze", json!({"id": id, "s": "stop"}));
    }
}

/// Explorer, open at the Compressed folder with this copy picked out.
pub fn show(app: &AppHandle, path: &str) {
    use std::os::windows::process::CommandExt;
    let p = Path::new(path);
    if p.starts_with(folder(app)) && p.is_file() && !path.contains('"') {
        let _ = std::process::Command::new("explorer.exe").raw_arg(format!("/select,\"{path}\"")).spawn();
    }
}

fn len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

fn stem(p: &Path) -> String {
    p.file_stem().map(|s| s.to_string_lossy().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "File".into())
}

/// Worth keeping: at least a twentieth lighter than what it came from.
fn lighter(to: u64, from: u64) -> bool {
    to > 0 && to < from - from / 20
}

/// A copy that isn't wanted after all (no smaller, stopped, or failed half-way). It's ULTRON's own, just written.
fn drop_copy(p: &Path) {
    for _ in 0..20 {
        if std::fs::remove_file(p).is_ok() || !p.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100)); // (the encoder may hold it a moment longer)
    }
}

fn kind_of(p: &Path) -> &'static str {
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "jfif" | "png" | "bmp" => "photo",
        "mp4" | "mov" | "m4v" | "mkv" | "webm" | "avi" | "wmv" | "mpg" | "mpeg" | "3gp" | "mts" | "m2ts" => "video",
        _ => "file",
    }
}

/// A photo: turned the right way up, scaled down when its long side is over 2560 (Smaller) or 1600
/// (Smallest), and saved as a JPEG. One with see-through parts stays a PNG.
fn photo(src: &Path, dir: &Path, tiny: bool, from: u64) -> Result<Option<PathBuf>, String> {
    use image::codecs::jpeg::JpegEncoder;
    use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
    use image::{DynamicImage, ImageDecoder, ImageReader};

    let bad = |_| "This picture couldn't be read".to_string();
    let mut dec = ImageReader::open(src).map_err(|_| "This picture couldn't be opened".to_string())?.with_guessed_format().map_err(|_| "This picture couldn't be opened".to_string())?.into_decoder().map_err(bad)?;
    let turn = dec.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(dec).map_err(bad)?;
    img.apply_orientation(turn);
    let side = if tiny { 1600 } else { 2560 };
    if img.width().max(img.height()) > side {
        img = img.resize(side, side, image::imageops::FilterType::CatmullRom);
    }
    let clear = img.color().has_alpha() && img.to_rgba8().pixels().any(|p| p.0[3] < 250);
    let mut out = Vec::new();
    let name = if clear {
        img.write_with_encoder(PngEncoder::new_with_quality(&mut out, CompressionType::Best, PngFilter::Adaptive)).map_err(|_| "This picture couldn't be saved".to_string())?;
        format!("{}.png", stem(src))
    } else {
        JpegEncoder::new_with_quality(&mut out, if tiny { 62 } else { 80 }).encode_image(&img.to_rgb8()).map_err(|_| "This picture couldn't be saved".to_string())?;
        format!("{}.jpg", stem(src))
    };
    if !lighter(out.len() as u64, from) {
        return Ok(None);
    }
    let to = crate::tools::free_name(dir, &name);
    std::fs::write(&to, out).map_err(|e| format!("Couldn't save the copy: {e}"))?;
    Ok(Some(to))
}

/// A video, encoded again as H.264 in an MP4 by Windows: up to 1080p at a moderate bit rate
/// (Smaller), or 720p, 30 frames a second and a low one (Smallest). `Err("")` means stopped.
fn video(app: &AppHandle, id: u32, src: &Path, dir: &Path, tiny: bool, from: u64) -> Result<Option<PathBuf>, String> {
    use windows::core::{GUID, HSTRING};
    use windows::Media::MediaProperties::{AudioEncodingProperties, MediaEncodingProfile, VideoEncodingQuality};
    use windows::Media::Transcoding::MediaTranscoder;
    use windows::Storage::StorageFile;
    use windows_future::{AsyncActionProgressHandler, AsyncStatus};

    let cant = |e: windows::core::Error| {
        crate::log_line(&format!("files: video: {e}"));
        "Windows couldn't read this video".to_string()
    };
    let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(src)).and_then(|o| o.get()).map_err(&cant)?;
    let was = MediaEncodingProfile::CreateFromFileAsync(&file).and_then(|o| o.get()).map_err(&cant)?;
    let wv = was.Video().map_err(&cant)?;
    let (sw, sh) = (wv.Width().unwrap_or(0), wv.Height().unwrap_or(0));
    if sw < 16 || sh < 16 {
        return Err("Windows couldn't read this video".into());
    }
    let mut fps = wv.FrameRate().ok().and_then(|f| Some((f.Numerator().ok()?, f.Denominator().ok()?))).filter(|(n, d)| *n > 0 && *d > 0).unwrap_or((30, 1));
    let top = if tiny { 30.0 } else { 60.0 };
    if fps.0 as f64 / fps.1 as f64 > top + 1.0 {
        fps = (top as u32, 1);
    }
    let rate = fps.0 as f64 / fps.1 as f64;
    let k = ((if tiny { 720.0 } else { 1080.0 }) / sw.min(sh) as f64).min(1.0);
    let even = |v: f64| ((v / 2.0).round() as u32 * 2).max(2);
    let (w, h) = (even(sw as f64 * k), even(sh as f64 * k));
    // bits for each second: by the size of the picture, and a little more for more frames (60 a second asks 1.4 times 30's)
    let mut bps = w as f64 * h as f64 * 30.0 * (rate / 30.0).max(0.5).sqrt() * if tiny { 0.045 } else { 0.07 };
    let had = wv.Bitrate().unwrap_or(0) as f64;
    if had > 0.0 {
        bps = bps.min(had * 0.6);
    }
    let bps = bps.max(250_000.0) as u32;

    let prof = MediaEncodingProfile::CreateMp4(VideoEncodingQuality::HD1080p).map_err(&cant)?;
    (|| -> windows::core::Result<()> {
        let v = prof.Video()?;
        v.SetWidth(w)?;
        v.SetHeight(h)?;
        v.SetBitrate(bps)?;
        let f = v.FrameRate()?;
        f.SetNumerator(fps.0)?;
        f.SetDenominator(fps.1)?;
        // (a phone's upright video is stored on its side with a note to turn it: the note goes along)
        const TURN: GUID = GUID::from_u128(0xc380465d_2271_428c_9b83_ecea3b4a85c1);
        if let Ok(t) = wv.Properties().and_then(|p| p.Lookup(TURN)) {
            let _ = v.Properties().and_then(|p| p.Insert(TURN, &t));
        }
        if was.Audio().and_then(|a| a.SampleRate()).unwrap_or(0) > 0 {
            prof.Audio()?.SetBitrate(if tiny { 96_000 } else { 128_000 })?;
        } else {
            prof.SetAudio(None::<&AudioEncodingProperties>)?; // (a video with no sound)
        }
        Ok(())
    })()
    .map_err(&cant)?;

    let to = crate::tools::free_name(dir, &format!("{}.mp4", stem(src)));
    std::fs::File::create(&to).map_err(|e| format!("Couldn't save the copy: {e}"))?;
    // 0: done, 1: stopped, 2: Windows has no way to read or write this kind
    let run = || -> windows::core::Result<u8> {
        let out = StorageFile::GetFileFromPathAsync(&HSTRING::from(to.as_path()))?.get()?;
        let t = MediaTranscoder::new()?;
        t.SetHardwareAccelerationEnabled(true)?;
        t.SetAlwaysReencode(true)?;
        let prep = t.PrepareFileTranscodeAsync(&file, &out, &prof)?.get()?;
        if !prep.CanTranscode()? {
            crate::log_line(&format!("files: video: can't be encoded again (reason {})", prep.FailureReason().map(|r| r.0).unwrap_or(-1)));
            return Ok(2);
        }
        let op = prep.TranscodeAsync()?;
        let (app2, last) = (app.clone(), Mutex::new((None::<Instant>, -1i32)));
        op.SetProgress(&AsyncActionProgressHandler::<f64>::new(move |_, p: windows::core::Ref<f64>| {
            let pct: f64 = (*p).clamp(0.0, 100.0);
            let mut l = last.lock().unwrap();
            if pct as i32 != l.1 && l.0.map_or(true, |t| t.elapsed() > Duration::from_millis(300)) {
                *l = (Some(Instant::now()), pct as i32);
                let _ = app2.emit("squeeze", json!({"id": id, "s": "run", "kind": "video", "pct": pct}));
            }
            Ok(())
        }))?;
        let mut stopped = false;
        loop {
            let st = op.Status()?;
            if st == AsyncStatus::Completed {
                return Ok(if stopped { 1 } else { 0 });
            }
            if st != AsyncStatus::Started {
                if stopped || st == AsyncStatus::Canceled {
                    return Ok(1);
                }
                op.GetResults()?; // (says what went wrong)
                return Ok(0);
            }
            if !stopped && STOP.load(SeqCst) {
                stopped = true;
                let _ = op.Cancel();
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    let how = run();
    match how {
        Ok(0) if lighter(len(&to), from) => Ok(Some(to)),
        Ok(0) => {
            let empty = len(&to) == 0;
            drop_copy(&to);
            if empty { Err("Windows couldn't shrink this video".into()) } else { Ok(None) }
        }
        Ok(1) => {
            drop_copy(&to);
            Err(String::new())
        }
        Ok(_) => {
            drop_copy(&to);
            Err("Windows can't read this kind of video".into())
        }
        Err(e) => {
            drop_copy(&to);
            crate::log_line(&format!("files: video: {e}"));
            Err("Windows couldn't shrink this video".into())
        }
    }
}

/// Anything else: zipped. (What's already packed tight, like a PDF or another zip, gains nothing and no copy is kept.)
fn zip(src: &Path, dir: &Path, from: u64) -> Result<Option<PathBuf>, String> {
    if from > 1_900_000_000 {
        return Err("Too big to zip (the limit is 2 GB)".into());
    }
    let to = crate::tools::free_name(dir, &format!("{}.zip", stem(src)));
    crate::tools::zip_one(src, &to).map_err(|e| {
        crate::log_line(&format!("files: zip: {}", e.lines().next().unwrap_or("").chars().take(120).collect::<String>()));
        drop_copy(&to);
        "This file couldn't be zipped".to_string()
    })?;
    if lighter(len(&to), from) {
        Ok(Some(to))
    } else {
        drop_copy(&to);
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A big noisy-but-smooth picture comes out as a much lighter JPEG, turned into at most 2560 (or 1600) a side.
    #[test]
    fn photo_shrinks() {
        let dir = std::env::temp_dir().join("ultron-squeeze-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("in.png");
        let img = image::RgbImage::from_fn(3200, 2000, |x, y| { let n = (x.wrapping_mul(7919) ^ y.wrapping_mul(104729)) % 6; image::Rgb([(x / 13 + y / 7 + n) as u8, (x / 5 + n) as u8, (y / 9 + n) as u8]) });
        img.save(&src).unwrap();
        let from = len(&src);
        for (tiny, side) in [(false, 2560), (true, 1600)] {
            let to = photo(&src, &dir, tiny, from).unwrap().expect("a smaller copy");
            let got = image::image_dimensions(&to).unwrap();
            assert_eq!(got.0.max(got.1), side);
            assert!(len(&to) < from / 2, "{} of {}", len(&to), from);
            assert_eq!(to.extension().unwrap(), "jpg");
        }
        assert_eq!(kind_of(Path::new("a.MP4")), "video");
        assert_eq!(kind_of(Path::new("a.pdf")), "file");
        assert!(!lighter(96, 100) && lighter(90, 100));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
