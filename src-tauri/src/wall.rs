//! Wallpaper, in the bar: three of ULTRON's own, and any picture of yours dropped onto the menu.
//!
//! The picture is first put in Documents › ULTRON Files › Wallpapers (one of ULTRON's is written
//! out there, yours is copied there, so it stays your wallpaper wherever the original goes), then
//! handed to Windows, which fades to it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use base64::Engine;
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

/// ULTRON's own, as the page knows them ("our:dusk") and as they're named once written out.
const OURS: [(&str, &str); 3] = [("dusk", "Dusk"), ("tide", "Tide"), ("graphite", "Graphite")];

pub fn folder(app: &AppHandle) -> PathBuf {
    let d = app.path().document_dir().unwrap_or_else(|_| crate::skills::known_folder("documents")).join("ULTRON Files").join("Wallpapers");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn ours_file(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("ULTRON {name}.jpg"))
}

fn is_picture(p: &Path) -> bool {
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "bmp")
}

fn same(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

/// Your picture, copied into the Wallpapers folder (used where it is when it's already there, and
/// not copied twice when it has been dropped before).
fn place(dir: &Path, src: &Path) -> Result<PathBuf, String> {
    if !src.is_file() {
        return Err("That picture isn't there any more".into());
    }
    if !is_picture(src) {
        return Err("ULTRON takes JPG, PNG or BMP pictures".into());
    }
    let size = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
    if size == 0 || size > 80 * 1024 * 1024 {
        return Err("That picture is too big to use".into());
    }
    if src.parent().is_some_and(|p| same(p, dir)) {
        return Ok(src.to_path_buf());
    }
    let name = src.file_name().ok_or("That picture has no name")?.to_string_lossy().to_string();
    let twin = dir.join(&name);
    if twin.is_file() && std::fs::metadata(&twin).map(|m| m.len()).unwrap_or(0) == size {
        return Ok(twin);
    }
    let to = crate::tools::free_name(dir, &name);
    std::fs::copy(src, &to).map_err(|e| format!("Couldn't copy the picture: {e}"))?;
    // (a copy keeps the original's date; this one is new, and the newest are the ones shown)
    let _ = std::fs::File::options().write(true).open(&to).and_then(|f| f.set_modified(SystemTime::now()));
    Ok(to)
}

/// Where the picture to use ends up: one of ULTRON's own written out, or yours copied in.
fn prepare(app: &AppHandle, pick: &str) -> Result<PathBuf, String> {
    let dir = folder(app);
    let Some(id) = pick.strip_prefix("our:") else { return place(&dir, Path::new(pick)) };
    let name = OURS.iter().find(|o| o.0 == id).ok_or("That isn't one of ULTRON's wallpapers")?.1;
    let to = ours_file(&dir, name);
    let bytes = app.asset_resolver().get(format!("wallpapers/{id}.jpg")).map(|a| a.bytes).filter(|b| b.starts_with(&[0xFF, 0xD8])).ok_or("That wallpaper is missing from this copy of ULTRON")?;
    if std::fs::metadata(&to).map(|m| m.len()).unwrap_or(0) != bytes.len() as u64 {
        std::fs::write(&to, &bytes).map_err(|e| format!("Couldn't save the wallpaper: {e}"))?;
    }
    Ok(to)
}

/// Windows' own wallpaper setting, asked or told on a thread of its own (it's a COM object).
fn desktop<T: Send + 'static>(f: impl FnOnce(&windows::Win32::UI::Shell::IDesktopWallpaper) -> windows::core::Result<T> + Send + 'static) -> Option<T> {
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{DesktopWallpaper, IDesktopWallpaper};
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let r = (|| -> windows::core::Result<T> {
            let d: IDesktopWallpaper = CoCreateInstance(&DesktopWallpaper, None, CLSCTX_ALL)?;
            f(&d)
        })();
        CoUninitialize();
        r.map_err(|e| crate::log_line(&format!("wallpaper: {e}"))).ok()
    })
    .join()
    .ok()
    .flatten()
}

/// The picture Windows is showing now ("" when it's a slideshow, a colour, or differs between screens).
fn current() -> String {
    desktop(|d| unsafe {
        let p = d.GetWallpaper(windows::core::PCWSTR::null())?;
        let s = p.to_string().unwrap_or_default();
        windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as _));
        Ok(s)
    })
    .unwrap_or_default()
}

fn set(path: &Path) -> Result<(), String> {
    let p = windows::core::HSTRING::from(path);
    desktop(move |d| unsafe {
        let _ = d.SetPosition(windows::Win32::UI::Shell::DWPOS_FILL);
        d.SetWallpaper(windows::core::PCWSTR::null(), &p)
    })
    .ok_or_else(|| "Windows wouldn't change the wallpaper".to_string())
}

/// A small picture of one of yours for the menu, made once and kept while ULTRON runs.
fn thumb(p: &Path) -> Option<String> {
    static MADE: Mutex<Option<HashMap<(PathBuf, u64), String>>> = Mutex::new(None);
    let key = (p.to_path_buf(), std::fs::metadata(p).ok()?.len());
    if let Some(t) = MADE.lock().unwrap().get_or_insert_with(HashMap::new).get(&key) {
        return Some(t.clone());
    }
    let small = image::open(p).ok()?.resize_to_fill(288, 180, image::imageops::FilterType::Triangle).to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80).encode_image(&small).ok()?;
    let url = format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(out));
    MADE.lock().unwrap().get_or_insert_with(HashMap::new).insert(key, url.clone());
    Some(url)
}

/// For the menu: which wallpaper is on now ("our:dusk", the path of one of yours, or ""), and your
/// three newest pictures.
pub fn list(app: &AppHandle) -> Value {
    let dir = folder(app);
    let now = PathBuf::from(current());
    let mut mine: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(&dir)
        .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.is_file() && is_picture(p) && !OURS.iter().any(|o| same(p, &ours_file(&dir, o.1)))).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .map(|p| (std::fs::metadata(&p).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH), p))
        .collect();
    mine.sort_by(|a, b| b.0.cmp(&a.0));
    mine.truncate(3);
    let on = OURS.iter().find(|o| same(&now, &ours_file(&dir, o.1))).map(|o| format!("our:{}", o.0)).or_else(|| mine.iter().find(|m| same(&now, &m.1)).map(|m| m.1.to_string_lossy().to_string())).unwrap_or_default();
    let mine: Vec<Value> = mine
        .iter()
        .map(|(_, p)| json!({"path": p.to_string_lossy(), "name": p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(), "thumb": thumb(p).unwrap_or_default()}))
        .collect();
    json!({"current": on, "mine": mine})
}

/// Make `pick` ("our:dusk", or the path of a picture) the wallpaper. Gives back what `list` gives.
pub fn apply(app: &AppHandle, pick: &str) -> Result<Value, String> {
    let p = prepare(app, pick)?;
    set(&p)?;
    crate::backdrop::kick(); // (the glass takes on the new picture at once)
    Ok(list(app))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dropped picture is copied in once, used where it is when it's already in the folder, and
    /// anything that isn't a picture is turned away. (Nothing here touches Windows' wallpaper.)
    #[test]
    fn pictures_are_copied_in_once() {
        let base = std::env::temp_dir().join("ultron-wall-test");
        let _ = std::fs::remove_dir_all(&base);
        let (from, dir) = (base.join("from"), base.join("Wallpapers"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let src = from.join("Holiday.PNG");
        image::RgbImage::from_pixel(64, 40, image::Rgb([40, 90, 200])).save_with_format(&src, image::ImageFormat::Png).unwrap();
        let a = place(&dir, &src).unwrap();
        assert!(a.starts_with(&dir) && a.is_file());
        assert_eq!(place(&dir, &src).unwrap(), a, "dropped again: the same copy");
        assert_eq!(place(&dir, &a).unwrap(), a, "already in the folder: used where it is");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        assert!(thumb(&a).is_some_and(|t| t.starts_with("data:image/jpeg;base64,")));
        let txt = from.join("notes.txt");
        std::fs::write(&txt, "hello").unwrap();
        assert!(place(&dir, &txt).is_err());
        assert!(place(&dir, &from.join("gone.jpg")).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
