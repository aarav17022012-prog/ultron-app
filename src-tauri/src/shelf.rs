//! The Drop Shelf: drag anything (files, folders, pictures) to the bottom edge of the screen and drop it on the glass
//! tray that comes up; it stays there, at hand, until you drag it out again into any app, or put it away.
//!
//! The shelf holds where things are (as a link would), not copies, so nothing is duplicated or moved by keeping it
//! there. The list is kept in SYLVER's data folder (shelf.json) and survives restarts; things that have since been
//! deleted or moved drop off it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use windows::core::HSTRING;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoTaskMemFree, IDataObject};
use windows::Win32::System::Ole::{DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{SHCreateShellItemArrayFromIDLists, SHDoDragDrop, SHParseDisplayName, BHID_DataObject, IShellItemArray};

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Item {
    pub path: String,
    pub name: String,
    pub dir: bool,
    pub size: u64,
    /// an app pinned to the shelf: `path` is what starts it (as the Start menu has it)
    #[serde(default)]
    pub app: bool,
}

const MOST: usize = 48;

fn file(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("shelf.json"))
}

fn item(p: &Path) -> Option<Item> {
    let m = std::fs::metadata(p).ok()?;
    Some(Item {
        path: p.to_string_lossy().to_string(),
        name: p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.to_string_lossy().to_string()),
        dir: m.is_dir(),
        size: if m.is_dir() { 0 } else { m.len() },
        app: false,
    })
}

fn save(app: &AppHandle, items: &[Item]) {
    if let (Some(f), Ok(s)) = (file(app), serde_json::to_string(items)) {
        if let Some(d) = f.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(f, s);
    }
}

/// What's on the shelf now, newest first (anything deleted or moved away since has dropped off).
pub fn list(app: &AppHandle) -> Vec<Item> {
    let kept: Vec<Item> = file(app).and_then(|f| std::fs::read_to_string(f).ok()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let now: Vec<Item> = kept.iter().filter_map(|i| if i.app { Some(i.clone()) } else { item(Path::new(&i.path)) }).collect();
    if now.len() != kept.len() {
        save(app, &now);
    }
    now
}

/// Put these on the shelf (at the front; one already there moves to the front).
pub fn add(app: &AppHandle, paths: &[String]) -> Vec<Item> {
    let mut items = list(app);
    for p in paths.iter().rev() {
        let Some(it) = item(Path::new(p)) else { continue };
        items.retain(|i| !i.path.eq_ignore_ascii_case(&it.path));
        items.insert(0, it);
    }
    items.truncate(MOST);
    save(app, &items);
    items
}

/// Pin an app to the shelf (at the front).
pub fn add_app(app: &AppHandle, name: &str, launch: &str) -> Vec<Item> {
    let mut items = list(app);
    items.retain(|i| !i.path.eq_ignore_ascii_case(launch));
    items.insert(0, Item { path: launch.to_string(), name: name.to_string(), dir: false, size: 0, app: true });
    items.truncate(MOST);
    save(app, &items);
    items
}

/// Take one off the shelf (the file itself stays where it is).
pub fn remove(app: &AppHandle, path: &str) -> Vec<Item> {
    let mut items = list(app);
    items.retain(|i| !i.path.eq_ignore_ascii_case(path));
    save(app, &items);
    items
}

pub fn clear(app: &AppHandle) {
    save(app, &[]);
}

/// Drag these out of the shelf into another app, as Windows' own drag (Explorer, a chat, an upload box, a mail all
/// take it as if dragged from a folder). Runs on the window's own thread; returns once dropped (or let go):
/// 0 nothing taken, 1 copied, 2 moved, 4 linked.
pub fn drag(hwnd: HWND, paths: &[String]) -> u32 {
    unsafe {
        let mut pidls: Vec<*mut ITEMIDLIST> = Vec::new();
        for p in paths {
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            if SHParseDisplayName(&HSTRING::from(p.as_str()), None, &mut pidl, 0, None).is_ok() && !pidl.is_null() {
                pidls.push(pidl);
            }
        }
        let effect = (|| {
            if pidls.is_empty() {
                return None;
            }
            let ids: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| *p as *const ITEMIDLIST).collect();
            let arr: IShellItemArray = SHCreateShellItemArrayFromIDLists(&ids).ok()?;
            let data: IDataObject = arr.BindToHandler(None, &BHID_DataObject).ok()?;
            crate::hit::DRAGGING_OUT.store(true, std::sync::atomic::Ordering::SeqCst);
            let r = SHDoDragDrop(Some(hwnd), &data, None, DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK).ok();
            crate::hit::DRAGGING_OUT.store(false, std::sync::atomic::Ordering::SeqCst);
            r
        })();
        for p in pidls {
            CoTaskMemFree(Some(p as *const _));
        }
        effect.map(|e| e.0).unwrap_or(0)
    }
}
