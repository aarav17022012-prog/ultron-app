//! Desktops (Settings > Look & feel, "Desktop switch"): when you move to another of Windows' desktops, or make a new
//! one, the page shows which one you're on. Windows writes the desktop it's on, and the list of them, under one key of
//! yours in the registry; a thread waits on that key (asleep until it changes, so it costs nothing in between).

use tauri::{AppHandle, Emitter};
use windows::core::w;
use windows::Win32::System::Registry::{
    RegGetValueW, RegNotifyChangeKeyValue, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_NOTIFY, KEY_READ, REG_NOTIFY_CHANGE_LAST_SET, REG_NOTIFY_CHANGE_NAME,
    RRF_RT_REG_BINARY, RRF_RT_REG_SZ,
};

#[derive(serde::Serialize, Clone)]
struct Desk {
    /// which one (from 0), how many, its name if you gave it one, and whether there are more than before
    i: usize,
    n: usize,
    name: String,
    new: bool,
}

fn bytes(key: HKEY, sub: windows::core::PCWSTR, value: windows::core::PCWSTR) -> Vec<u8> {
    let mut len = 0u32;
    unsafe {
        if RegGetValueW(key, sub, value, RRF_RT_REG_BINARY, None, None, Some(&mut len)).is_err() || len == 0 {
            return Vec::new();
        }
        let mut buf = vec![0u8; len as usize];
        if RegGetValueW(key, sub, value, RRF_RT_REG_BINARY, None, Some(buf.as_mut_ptr() as _), Some(&mut len)).is_err() {
            return Vec::new();
        }
        buf.truncate(len as usize);
        buf
    }
}

fn name(key: HKEY, id: &[u8]) -> String {
    if id.len() != 16 {
        return String::new();
    }
    // (the desktop's GUID, written the way the registry names its key: {xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx})
    let g = windows::core::GUID::from_values(
        u32::from_le_bytes([id[0], id[1], id[2], id[3]]),
        u16::from_le_bytes([id[4], id[5]]),
        u16::from_le_bytes([id[6], id[7]]),
        [id[8], id[9], id[10], id[11], id[12], id[13], id[14], id[15]],
    );
    let sub: Vec<u16> = format!("Desktops\\{{{g:?}}}\0").encode_utf16().collect();
    let mut buf = [0u16; 128];
    let mut len = (buf.len() * 2) as u32;
    unsafe {
        if RegGetValueW(key, windows::core::PCWSTR(sub.as_ptr()), w!("Name"), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as _), Some(&mut len)).is_err() {
            return String::new();
        }
    }
    String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)])
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\VirtualDesktops"), None, KEY_READ | KEY_NOTIFY, &mut key).is_err() {
            return;
        }
        let (mut was_cur, mut was_n) = (Vec::new(), 0usize);
        loop {
            let ids = bytes(key, windows::core::PCWSTR::null(), w!("VirtualDesktopIDs"));
            let cur = bytes(key, windows::core::PCWSTR::null(), w!("CurrentVirtualDesktop"));
            let n = ids.len() / 16;
            if !cur.is_empty() && n > 0 && (cur != was_cur || n != was_n) {
                if !was_cur.is_empty() {
                    let i = ids.chunks(16).position(|c| c == cur.as_slice()).unwrap_or(0);
                    let _ = app.emit_to("main", "desk", Desk { i, n, name: name(key, &cur), new: n > was_n });
                }
                was_cur = cur;
                was_n = n;
            }
            // (asleep until Windows writes something under the key)
            if RegNotifyChangeKeyValue(key, true, REG_NOTIFY_CHANGE_LAST_SET | REG_NOTIFY_CHANGE_NAME, None, false).is_err() {
                return;
            }
        }
    });
}
