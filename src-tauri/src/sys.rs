//! System bits for the menu bar: the wallpaper (for the glass), Wi-Fi, the
//! Start menu's app list and icons, and power actions.

use std::collections::HashMap;
use std::io::Cursor;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use base64::Engine;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use windows::core::HSTRING;
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY};
use windows::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETDESKWALLPAPER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS};

const NO_WINDOW: u32 = 0x0800_0000;

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/* ---------------- wallpaper (pre-blurred, small) ---------------- */

pub(crate) fn wallpaper_path() -> Option<std::path::PathBuf> {
    let transcoded = std::env::var("APPDATA").ok().map(|a| std::path::PathBuf::from(a).join(r"Microsoft\Windows\Themes\TranscodedWallpaper"));
    if let Some(p) = transcoded.filter(|p| p.exists()) {
        return Some(p);
    }
    let mut buf = [0u16; 520];
    unsafe {
        SystemParametersInfoW(SPI_GETDESKWALLPAPER, buf.len() as u32, Some(buf.as_mut_ptr() as _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)).ok()?;
    }
    let s = String::from_utf16_lossy(&buf).trim_end_matches('\0').to_string();
    (!s.is_empty()).then(|| s.into()).filter(|p: &std::path::PathBuf| p.exists())
}

/// The glass's colour toning (what CSS `saturate(s) brightness(b)` does), applied to RGB bytes here,
/// once, so the page doesn't run a filter over a screen-sized layer every time the glass changes.
pub fn tone(px: &mut [u8], sat: f32, bright: f32) {
    let (r0, g0, b0) = (0.213f32, 0.715f32, 0.072f32);
    let m = [
        [r0 + (1.0 - r0) * sat, g0 - g0 * sat, b0 - b0 * sat],
        [r0 - r0 * sat, g0 + (1.0 - g0) * sat, b0 - b0 * sat],
        [r0 - r0 * sat, g0 - g0 * sat, b0 + (1.0 - b0) * sat],
    ];
    for p in px.chunks_exact_mut(3) {
        let (r, g, b) = (p[0] as f32, p[1] as f32, p[2] as f32);
        for (i, row) in m.iter().enumerate() {
            let v = (row[0] * r + row[1] * g + row[2] * b).clamp(0.0, 255.0) * bright;
            p[i] = v.round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// The wallpaper, shrunk and blurred once here so the page never blurs live (that's what keeps it smooth).
fn wallpaper_glass() -> Option<(String, String)> {
    let path = wallpaper_path()?;
    let stamp = std::fs::metadata(&path).and_then(|m| m.modified()).ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
    let key = format!("{}|{stamp}", path.display());
    let bytes = std::fs::read(&path).ok()?;
    let img = image::load_from_memory(&bytes).ok()?;
    let small = img.thumbnail(640, 640).blur(7.0);
    let mut out = Vec::new();
    let mut rgb = small.to_rgb8();
    tone(&mut rgb, 1.5, 0.92);
    rgb.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Jpeg).ok()?;
    Some((key, format!("data:image/jpeg;base64,{}", b64(&out))))
}

static WALL: Mutex<Option<(String, String)>> = Mutex::new(None);

pub fn wallpaper_now() -> Option<String> {
    if let Some((_, url)) = WALL.lock().unwrap().as_ref() {
        return Some(url.clone());
    }
    let w = wallpaper_glass()?;
    let url = w.1.clone();
    *WALL.lock().unwrap() = Some(w);
    Some(url)
}

/// Re-sends the glass whenever the wallpaper changes.
pub fn watch_wallpaper(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(15));
        let Some(path) = wallpaper_path() else { continue };
        let stamp = std::fs::metadata(&path).and_then(|m| m.modified()).ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
        let key = format!("{}|{stamp}", path.display());
        let same = WALL.lock().unwrap().as_ref().map(|w| w.0 == key).unwrap_or(false);
        if !same {
            if let Some(w) = wallpaper_glass() {
                let _ = app.emit_to("main", "wallpaper", &w.1);
                *WALL.lock().unwrap() = Some(w);
            }
        }
    });
}

/* ---------------- Wi-Fi ---------------- */

#[derive(Serialize, Clone, Default)]
pub struct Network {
    pub ssid: String,
    pub signal: u32,
    pub secure: bool,
    /// how it's secured, as Windows names it ("WPA2-Personal", "WPA3-Personal", "Open"...)
    pub auth: String,
    pub known: bool,
    pub current: bool,
}

#[derive(Serialize, Clone, Default)]
pub struct Wifi {
    pub available: bool,
    pub radio: bool,
    pub connected: bool,
    pub ssid: String,
    pub signal: u32,
    pub networks: Vec<Network>,
}

fn netsh(args: &[&str]) -> String {
    Command::new("netsh")
        .args(args)
        .creation_flags(NO_WINDOW)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

fn kv(line: &str) -> Option<(String, String)> {
    let (k, v) = line.split_once(':')?;
    Some((k.trim().to_string(), v.trim().to_string()))
}

/// What the bar shows (connected, to what, how strong, the radio on or off), asked of Windows' own Wi-Fi service. The
/// bar used to start the netsh program for this every 20 seconds: a new program each time for Windows (and its virus
/// scanner) to load, a small hitch now and then. The full list of networks, for the panel, still comes from netsh.
fn wifi_now() -> Option<Wifi> {
    use windows::core::GUID;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::NetworkManagement::WiFi::*;
    unsafe {
        let (mut ver, mut h) = (0u32, HANDLE::default());
        if WlanOpenHandle(2, None, &mut ver, &mut h) != 0 {
            return None;
        }
        let mut w = Wifi::default();
        let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
        let found = WlanEnumInterfaces(h, None, &mut list) == 0 && !list.is_null() && (*list).dwNumberOfItems > 0;
        if found {
            let info = &*(*list).InterfaceInfo.as_ptr();
            let g: GUID = info.InterfaceGuid;
            w.available = true;
            w.radio = true;
            // the radio: off when every one of its parts is switched off (by the switch, or by Windows)
            let (mut size, mut data) = (0u32, std::ptr::null_mut::<core::ffi::c_void>());
            if WlanQueryInterface(h, &g, wlan_intf_opcode_radio_state, None, &mut size, &mut data, None) == 0 && !data.is_null() {
                let rs = &*(data as *const WLAN_RADIO_STATE);
                let n = (rs.dwNumberOfPhys as usize).min(rs.PhyRadioState.len());
                if n > 0 {
                    w.radio = rs.PhyRadioState[..n].iter().any(|p| p.dot11SoftwareRadioState == dot11_radio_state_on && p.dot11HardwareRadioState == dot11_radio_state_on);
                }
                WlanFreeMemory(data);
            }
            if info.isState == wlan_interface_state_connected {
                let (mut size, mut data) = (0u32, std::ptr::null_mut::<core::ffi::c_void>());
                if WlanQueryInterface(h, &g, wlan_intf_opcode_current_connection, None, &mut size, &mut data, None) == 0 && !data.is_null() {
                    let a = &(*(data as *const WLAN_CONNECTION_ATTRIBUTES)).wlanAssociationAttributes;
                    let len = (a.dot11Ssid.uSSIDLength as usize).min(a.dot11Ssid.ucSSID.len());
                    w.connected = true;
                    w.ssid = String::from_utf8_lossy(&a.dot11Ssid.ucSSID[..len]).to_string();
                    w.signal = a.wlanSignalQuality;
                    WlanFreeMemory(data);
                }
            }
        }
        if !list.is_null() {
            WlanFreeMemory(list as *const core::ffi::c_void);
        }
        WlanCloseHandle(h, None);
        found.then_some(w)
    }
}

pub fn wifi_status(with_list: bool) -> Wifi {
    if !with_list {
        if let Some(w) = wifi_now() {
            return w;
        }
    }
    let iface = netsh(&["wlan", "show", "interfaces"]);
    let mut w = Wifi::default();
    if !iface.contains("Name") {
        return w; // no wireless adapter or the WLAN service is off
    }
    w.available = true;
    w.radio = true; // (netsh only says "Radio status" when it's off)
    for line in iface.lines() {
        let Some((k, v)) = kv(line) else {
            if line.trim().eq_ignore_ascii_case("Software Off") {
                w.radio = false;
            }
            continue;
        };
        match k.as_str() {
            "State" => w.connected = v.eq_ignore_ascii_case("connected"),
            "SSID" => w.ssid = v,
            "Signal" => w.signal = v.trim_end_matches('%').parse().unwrap_or(0),
            "Radio status" => w.radio = !v.contains("Off"),
            _ => {}
        }
    }
    if iface.contains("Software Off") {
        w.radio = false;
    }
    if !with_list {
        return w;
    }
    let known: Vec<String> = netsh(&["wlan", "show", "profiles"])
        .lines()
        .filter_map(kv)
        .filter(|(k, _)| k.contains("User Profile"))
        .map(|(_, v)| v)
        .collect();
    let mut nets: Vec<Network> = Vec::new();
    for line in netsh(&["wlan", "show", "networks", "mode=bssid"]).lines() {
        let Some((k, v)) = kv(line) else { continue };
        if k.starts_with("SSID ") {
            nets.push(Network { ssid: v, ..Default::default() });
        } else if let Some(n) = nets.last_mut() {
            match k.as_str() {
                "Authentication" => {
                    n.secure = !v.eq_ignore_ascii_case("Open");
                    n.auth = v.clone();
                }
                "Signal" => n.signal = n.signal.max(v.trim_end_matches('%').parse().unwrap_or(0)),
                _ => {}
            }
        }
    }
    nets.retain(|n| !n.ssid.is_empty());
    for n in nets.iter_mut() {
        n.known = known.iter().any(|k| k == &n.ssid);
        n.current = w.connected && n.ssid == w.ssid;
    }
    nets.sort_by(|a, b| b.current.cmp(&a.current).then(b.known.cmp(&a.known)).then(b.signal.cmp(&a.signal)));
    w.networks = nets;
    w
}

pub fn wifi_connect(ssid: &str) -> bool {
    let name = format!("name={ssid}");
    let out = netsh(&["wlan", "connect", &name]);
    out.to_lowercase().contains("successfully")
}

/// Join a network with its password: a profile for it is given to Windows (as Windows' own box would), then
/// it's joined, and SYLVER waits to see it connect. A wrong password never connects: the profile is taken away
/// again, so the network isn't left "saved" with it.
pub fn wifi_join(ssid: &str, key: &str, auth: &str) -> bool {
    let x = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;");
    let a = auth.to_lowercase();
    let open = key.is_empty() || a == "open";
    let (method, cipher) = if open { ("open", "none") } else if a.contains("wpa3") { ("WPA3SAE", "AES") } else if a.contains("wpa-personal") { ("WPAPSK", "TKIP") } else { ("WPA2PSK", "AES") };
    let shared = if open { String::new() } else { format!("<sharedKey><keyType>passPhrase</keyType><protected>false</protected><keyMaterial>{}</keyMaterial></sharedKey>", x(key)) };
    let xml = format!(
        "<?xml version=\"1.0\"?><WLANProfile xmlns=\"http://www.microsoft.com/networking/WLAN/profile/v1\"><name>{n}</name><SSIDConfig><SSID><name>{n}</name></SSID></SSIDConfig><connectionType>ESS</connectionType><connectionMode>auto</connectionMode><MSM><security><authEncryption><authentication>{method}</authentication><encryption>{cipher}</encryption><useOneX>false</useOneX></authEncryption>{shared}</security></MSM></WLANProfile>",
        n = x(ssid)
    );
    let file = std::env::temp_dir().join(format!("ultron-wifi-{}.xml", std::process::id()));
    if std::fs::write(&file, xml).is_err() {
        return false;
    }
    let added = netsh(&["wlan", "add", "profile", &format!("filename={}", file.display()), "user=current"]);
    let _ = std::fs::remove_file(&file); // (the password isn't left lying in a file)
    if !added.to_lowercase().contains("is added") {
        return false;
    }
    netsh(&["wlan", "connect", &format!("name={ssid}")]);
    for _ in 0..24 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let st = netsh(&["wlan", "show", "interfaces"]);
        let on = st.lines().filter_map(kv).any(|(k, v)| k == "SSID" && v == ssid) && st.lines().filter_map(kv).any(|(k, v)| k == "State" && v.eq_ignore_ascii_case("connected"));
        if on {
            return true;
        }
    }
    netsh(&["wlan", "delete", "profile", &format!("name={ssid}")]);
    false
}

pub fn wifi_disconnect() {
    netsh(&["wlan", "disconnect"]);
}

/* ---------------- icons for the Start menu ---------------- */

static ICONS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn icon_data_url(source: &str, size: i32) -> Option<String> {
    image_data_url(source, size, true)
}

/// A file's picture: its own thumbnail (a photo, a video's frame, a document's first page) where Windows has one,
/// otherwise its icon (`icon_only`: always the icon).
pub(crate) fn image_data_url(source: &str, size: i32, icon_only: bool) -> Option<String> {
    unsafe {
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(source), None).ok()?;
        let flags = if icon_only { SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK } else { SIIGBF_BIGGERSIZEOK };
        let hbm = factory.GetImage(SIZE { cx: size, cy: size }, flags).ok()?;
        let mut bm = BITMAP::default();
        GetObjectW(hbm.into(), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as _));
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut px = vec![0u8; (w.max(0) * h.max(0) * 4) as usize];
        let dc = GetDC(None);
        let lines = GetDIBits(dc, hbm, 0, h as u32, Some(px.as_mut_ptr() as _), &mut bmi, DIB_RGB_COLORS);
        ReleaseDC(None, dc);
        let _ = DeleteObject(hbm.into());
        if lines == 0 || w <= 0 {
            return None;
        }
        // BGRA (premultiplied) -> RGBA straight alpha
        let any_alpha = px.chunks(4).any(|p| p[3] != 0);
        for p in px.chunks_mut(4) {
            let (b, g, r, a) = (p[0], p[1], p[2], if any_alpha { p[3] } else { 255 });
            let un = |c: u8| if a == 0 { 0 } else { ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8 };
            p[0] = un(r);
            p[1] = un(g);
            p[2] = un(b);
            p[3] = a;
        }
        let img = image::RgbaImage::from_raw(w as u32, h as u32, px)?;
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).ok()?;
        Some(format!("data:image/png;base64,{}", b64(&out)))
    }
}

pub fn icons(sources: Vec<String>) -> Vec<String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    sources
        .into_iter()
        .map(|s| {
            if let Some(u) = ICONS.lock().unwrap().get_or_insert_with(HashMap::new).get(&s) {
                return u.clone();
            }
            let u = icon_data_url(&s, 96).unwrap_or_default();
            ICONS.lock().unwrap().get_or_insert_with(HashMap::new).insert(s, u.clone());
            u
        })
        .collect()
}

/* ---------------- power ---------------- */

pub fn power(action: &str) {
    let run = |args: &[&str]| {
        let _ = Command::new("shutdown").args(args).creation_flags(NO_WINDOW).spawn();
    };
    match action {
        "lock" => unsafe {
            let _ = windows::Win32::System::Shutdown::LockWorkStation();
        },
        "sleep" => unsafe {
            let _ = windows::Win32::System::Power::SetSuspendState(false, false, false);
        },
        "restart" => run(&["/r", "/t", "0"]),
        "shutdown" => run(&["/s", "/t", "0"]),
        _ => {}
    }
}

/* ---------------- live system stats ---------------- */

#[derive(Serialize, Clone, Default)]
pub struct Stats {
    pub cpu: f64,
    pub mem_used: f64,
    pub mem_total: f64,
    pub disk_free: f64,
    pub disk_total: f64,
    pub uptime: f64,
}

static CPU_LAST: Mutex<(u64, u64)> = Mutex::new((0, 0));

pub fn stats() -> Stats {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    use windows::Win32::System::SystemInformation::{GetTickCount64, GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows::Win32::System::Threading::GetSystemTimes;
    let ft = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    let mut st = Stats::default();
    unsafe {
        let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
        if GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).is_ok() {
            let (i, t) = (ft(idle), ft(kernel) + ft(user)); // kernel time includes idle
            let mut last = CPU_LAST.lock().unwrap();
            let (di, dt) = (i.saturating_sub(last.0), t.saturating_sub(last.1));
            if last.1 != 0 && dt > 0 {
                st.cpu = (1.0 - di as f64 / dt as f64).clamp(0.0, 1.0);
            }
            *last = (i, t);
        }
        let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        if GlobalMemoryStatusEx(&mut m).is_ok() {
            st.mem_total = m.ullTotalPhys as f64;
            st.mem_used = (m.ullTotalPhys - m.ullAvailPhys) as f64;
        }
        let (mut free, mut total) = (0u64, 0u64);
        if GetDiskFreeSpaceExW(&HSTRING::from("C:\\"), Some(&mut free), Some(&mut total), None).is_ok() {
            st.disk_free = free as f64;
            st.disk_total = total as f64;
        }
        st.uptime = GetTickCount64() as f64 / 1000.0;
    }
    st
}

/* ---------------- news headlines ---------------- */

#[derive(Serialize, Clone)]
pub struct NewsItem {
    pub title: String,
    pub source: String,
    pub link: String,
    pub published: String,
}

fn between<'a>(s: &'a str, a: &str, b: &str) -> Option<&'a str> {
    let i = s.find(a)? + a.len();
    let j = s[i..].find(b)? + i;
    Some(&s[i..j])
}

fn unescape(s: &str) -> String {
    s.replace("<![CDATA[", "").replace("]]>", "").replace("&amp;", "&").replace("&quot;", "\"").replace("&#39;", "'")
        .replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&nbsp;", " ").trim().to_string()
}

pub fn news(country: &str) -> Vec<NewsItem> {
    let url = if country == "GLOBAL" {
        "https://news.google.com/rss/headlines/section/topic/WORLD?hl=en-US&gl=US&ceid=US:en".to_string()
    } else {
        let (hl, lang) = match country {
            "US" => ("en-US", "en"), "GB" => ("en-GB", "en"), "CA" => ("en-CA", "en"), "AU" => ("en-AU", "en"),
            "NZ" => ("en-NZ", "en"), "IE" => ("en-IE", "en"), "SG" => ("en-SG", "en"), "ZA" => ("en-ZA", "en"),
            "NG" => ("en-NG", "en"), "PH" => ("en-PH", "en"), "PK" => ("en-PK", "en"), "AE" => ("en-AE", "en"),
            "MY" => ("en-MY", "en"), "DE" => ("de", "de"), "FR" => ("fr", "fr"), "JP" => ("ja", "ja"),
            "BR" => ("pt-BR", "pt-419"), "ES" => ("es", "es"), "IT" => ("it", "it"), "KR" => ("ko", "ko"),
            _ => ("en-IN", "en"),
        };
        let gl = if country.len() == 2 { country } else { "IN" };
        format!("https://news.google.com/rss?hl={hl}&gl={gl}&ceid={gl}:{lang}")
    };
    rss(&url)
}

/// Headlines about a topic (for questions like "what happened to ...").
pub fn news_search(q: &str) -> Vec<NewsItem> {
    let q: String = q.bytes().map(|b| match b { b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => (b as char).to_string(), b' ' => "+".into(), _ => format!("%{b:02X}") }).collect();
    rss(&format!("https://news.google.com/rss/search?q={q}&hl=en-IN&gl=IN&ceid=IN:en"))
}

fn rss(url: &str) -> Vec<NewsItem> {
    let Ok(resp) = crate::net::agent(Duration::from_secs(10)).get(url).call() else {
        return Vec::new();
    };
    let Ok(body) = resp.into_string() else { return Vec::new() };
    body.split("<item>")
        .skip(1)
        .filter_map(|it| {
            let source = between(it, ">", "</source>").and_then(|_| {
                let i = it.find("<source")?;
                between(&it[i..], ">", "</source>")
            }).map(unescape).unwrap_or_default();
            let mut title = unescape(between(it, "<title>", "</title>")?);
            if !source.is_empty() {
                if let Some(t) = title.strip_suffix(&format!(" - {source}")) {
                    title = t.to_string();
                }
            }
            Some(NewsItem {
                title,
                source,
                link: unescape(between(it, "<link>", "</link>")?),
                published: between(it, "<pubDate>", "</pubDate>").map(unescape).unwrap_or_default(),
            })
        })
        .take(12)
        .collect()
}

/* ---------------- holidays and festivals (the calendar) ---------------- */

#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct Holiday {
    /// YYYY-MM-DD
    pub date: String,
    pub name: String,
    /// a day off (true), or an observance / festival (false)
    pub off: bool,
}

/// Google's public holiday calendar for each country the calendar offers (its holidays and its festivals).
fn holiday_feed(country: &str) -> Option<&'static str> {
    Some(match country {
        "IN" => "en.indian",
        "US" => "en.usa",
        "GB" => "en.uk",
        "CA" => "en.canadian",
        "AU" => "en.australian",
        "SG" => "en.singapore",
        "AE" => "en.ae",
        "PK" => "en.pk",
        "ZA" => "en.sa",
        "NG" => "en.ng",
        "PH" => "en.philippines",
        "DE" => "en.german",
        "FR" => "en.french",
        "JP" => "en.japanese",
        "BR" => "en.brazilian",
        "ES" => "en.spain",
        "IT" => "en.italian",
        "KR" => "en.south_korea",
        _ => return None,
    })
}

/// A country's holidays (about ten years of them), fetched once and kept for three weeks in SYLVER's folder.
pub fn holidays(app: &tauri::AppHandle, country: &str) -> Vec<Holiday> {
    use tauri::Manager;
    let Some(feed) = holiday_feed(country) else { return Vec::new() };
    let file = app.path().app_data_dir().ok().map(|d| d.join(format!("holidays-{country}.json")));
    let cached: Option<Vec<Holiday>> = file.as_ref().and_then(|f| std::fs::read_to_string(f).ok()).and_then(|s| serde_json::from_str(&s).ok());
    let fresh = file.as_ref().and_then(|f| std::fs::metadata(f).ok()).and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).map(|age| age.as_secs() < 21 * 24 * 3600).unwrap_or(false);
    if let (Some(c), true) = (&cached, fresh) {
        return c.clone();
    }
    let id = format!("{feed}#holiday@group.v.calendar.google.com").replace('#', "%23").replace('@', "%40");
    let got = ureq::get(&format!("https://calendar.google.com/calendar/ical/{id}/public/basic.ics")).timeout(std::time::Duration::from_secs(15)).call().ok().and_then(|r| r.into_string().ok());
    let Some(text) = got else { return cached.unwrap_or_default() };
    let list = parse_ics(&text);
    if list.is_empty() {
        return cached.unwrap_or_default();
    }
    if let Some(f) = file {
        crate::keep(&f, serde_json::to_vec(&list).unwrap_or_default());
    }
    list
}

/// The all-day events of an iCalendar file: date, name, and whether it's a public holiday.
fn parse_ics(text: &str) -> Vec<Holiday> {
    // (long lines continue on the next line after a space)
    let unfolded = text.replace("\r\n ", "").replace("\n ", "");
    let mut out: Vec<Holiday> = Vec::new();
    let mut cur: Option<Holiday> = None;
    for line in unfolded.lines() {
        let line = line.trim_end();
        if line == "BEGIN:VEVENT" {
            cur = Some(Holiday::default());
        } else if line == "END:VEVENT" {
            if let Some(h) = cur.take() {
                if h.date.len() == 10 && !h.name.is_empty() {
                    out.push(h);
                }
            }
        } else if let Some(h) = cur.as_mut() {
            if let Some(v) = line.strip_prefix("DTSTART;VALUE=DATE:").or_else(|| line.strip_prefix("DTSTART:")) {
                let d: String = v.chars().take(8).collect();
                if d.len() == 8 && d.chars().all(|c| c.is_ascii_digit()) {
                    h.date = format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..8]);
                }
            } else if let Some(v) = line.strip_prefix("SUMMARY:") {
                h.name = v.replace("\\,", ",").replace("\\;", ";").trim().to_string();
            } else if let Some(v) = line.strip_prefix("DESCRIPTION:") {
                h.off = v.starts_with("Public holiday");
            }
        }
    }
    out.sort_by(|a, b| a.date.cmp(&b.date).then(b.off.cmp(&a.off)));
    out.dedup_by(|a, b| a.date == b.date && a.name == b.name);
    out
}

