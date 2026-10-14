//! Clipboard history for the menu bar, saved on this PC so it survives restarts.
//! Anything a password manager marks as private is never recorded.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tauri::{AppHandle, Emitter};
use windows::core::HSTRING;
use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

const CF_UNICODETEXT: u32 = 13;
const MAX: usize = 25;
/// The Clipboard menu left the bar (13 Oct 2026), so what you copy is no longer recorded. What it had saved
/// is still read at start-up, so that Settings, Privacy, Clear can wipe it.
const KEEPING: bool = false;

static HISTORY: Mutex<Vec<String>> = Mutex::new(Vec::new());
static FILE: OnceLock<PathBuf> = OnceLock::new();

fn persist() {
    if let Some(f) = FILE.get() {
        let _ = crate::keep(f, serde_json::to_string(&*HISTORY.lock().unwrap()).unwrap_or_default());
    }
}

pub fn read_text() -> Option<String> {
    unsafe {
        let private = RegisterClipboardFormatW(&HSTRING::from("ExcludeClipboardContentFromMonitorProcessing"));
        if private != 0 && IsClipboardFormatAvailable(private).is_ok() {
            return None;
        }
        IsClipboardFormatAvailable(CF_UNICODETEXT).ok()?;
        OpenClipboard(None).ok()?;
        let text = (|| {
            let h = GetClipboardData(CF_UNICODETEXT).ok()?;
            let p = GlobalLock(HGLOBAL(h.0)) as *const u16;
            if p.is_null() {
                return None;
            }
            let mut n = 0;
            while *p.add(n) != 0 && n < 200_000 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
            let _ = GlobalUnlock(HGLOBAL(h.0));
            Some(s)
        })();
        let _ = CloseClipboard();
        text
    }
}

pub fn set(text: &str) -> bool {
    unsafe {
        if OpenClipboard(None).is_err() {
            return false;
        }
        let ok = (|| {
            EmptyClipboard().ok()?;
            let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2).ok()?;
            let p = GlobalLock(mem) as *mut u16;
            if p.is_null() {
                return None;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
            let _ = GlobalUnlock(mem);
            SetClipboardData(CF_UNICODETEXT, Some(HANDLE(mem.0))).ok()?;
            Some(())
        })()
        .is_some();
        let _ = CloseClipboard();
        ok
    }
}

/// Put a picture on the clipboard (BGRA pixels, top row first), as Windows' own kind of picture.
pub fn set_image(w: u32, h: u32, bgra: &[u8]) -> bool {
    use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
    const CF_DIB: u32 = 8;
    let head = std::mem::size_of::<BITMAPINFOHEADER>();
    let row = (w * 4) as usize;
    unsafe {
        if OpenClipboard(None).is_err() {
            return false;
        }
        let ok = (|| {
            EmptyClipboard().ok()?;
            let mem = GlobalAlloc(GMEM_MOVEABLE, head + row * h as usize).ok()?;
            let p = GlobalLock(mem) as *mut u8;
            if p.is_null() {
                return None;
            }
            let bi = BITMAPINFOHEADER { biSize: head as u32, biWidth: w as i32, biHeight: h as i32, biPlanes: 1, biBitCount: 32, ..Default::default() };
            std::ptr::copy_nonoverlapping(&bi as *const _ as *const u8, p, head);
            // (bottom row first, as such pictures are kept; and every dot solid)
            for y in 0..h as usize {
                let src = &bgra[(h as usize - 1 - y) * row..][..row];
                let dst = std::slice::from_raw_parts_mut(p.add(head + y * row), row);
                dst.copy_from_slice(src);
                dst.chunks_exact_mut(4).for_each(|d| d[3] = 255);
            }
            let _ = GlobalUnlock(mem);
            SetClipboardData(CF_DIB, Some(HANDLE(mem.0))).ok()?;
            // the same picture as a PNG, which is what browsers look for first
            let rgba: Vec<u8> = bgra.chunks_exact(4).flat_map(|d| [d[2], d[1], d[0], 255]).collect();
            if let Some(img) = image::RgbaImage::from_raw(w, h, rgba) {
                let mut png = std::io::Cursor::new(Vec::new());
                if image::DynamicImage::ImageRgba8(img).write_to(&mut png, image::ImageFormat::Png).is_ok() {
                    let png = png.into_inner();
                    let fmt = RegisterClipboardFormatW(&HSTRING::from("PNG"));
                    if fmt != 0 {
                        if let Ok(m2) = GlobalAlloc(GMEM_MOVEABLE, png.len()) {
                            let q = GlobalLock(m2) as *mut u8;
                            if !q.is_null() {
                                std::ptr::copy_nonoverlapping(png.as_ptr(), q, png.len());
                                let _ = GlobalUnlock(m2);
                                let _ = SetClipboardData(fmt, Some(HANDLE(m2.0)));
                            }
                        }
                    }
                }
            }
            Some(())
        })()
        .is_some();
        let _ = CloseClipboard();
        ok
    }
}

pub fn list() -> Vec<String> {
    HISTORY.lock().unwrap().clone()
}

pub fn clear() {
    HISTORY.lock().unwrap().clear();
    persist();
}

pub fn remove(i: usize) {
    let mut h = HISTORY.lock().unwrap();
    if i < h.len() {
        h.remove(i);
    }
    drop(h);
    persist();
}

pub fn watch(app: AppHandle) {
    use tauri::Manager;
    if let Ok(dir) = app.path().app_data_dir() {
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("clipboard.json");
        if let Some(saved) = std::fs::read_to_string(&f).ok().and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok()) {
            *HISTORY.lock().unwrap() = saved;
        }
        let _ = FILE.set(f);
    }
    if !KEEPING {
        return;
    }
    std::thread::spawn(move || {
        let mut seq = unsafe { GetClipboardSequenceNumber() };
        loop {
            let now = unsafe { GetClipboardSequenceNumber() };
            if now != seq {
                seq = now;
                // another app may still be holding the clipboard right after copying: try a few times
                let mut text = None;
                for _ in 0..6 {
                    text = read_text();
                    if text.is_some() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(60));
                }
                if let Some(t) = text.filter(|t| !t.trim().is_empty()) {
                    let t: String = t.chars().take(4000).collect();
                    let mut h = HISTORY.lock().unwrap();
                    h.retain(|x| x != &t);
                    h.insert(0, t);
                    h.truncate(MAX);
                    let list = h.clone();
                    drop(h);
                    persist();
                    let _ = app.emit_to("main", "clipboard", list);
                }
            }
            std::thread::sleep(Duration::from_millis(700));
        }
    });
}
