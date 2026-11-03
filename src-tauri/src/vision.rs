//! SYLVER Vision: how SYLVER finds things on your screen before it clicks or types.
//! Three layers, fastest first, all on this PC:
//!   1. what the app tells Windows about its buttons and fields (UI Automation), read
//!      in a single snapshot instead of walking the tree one element at a time;
//!   2. reading the pixels with Windows' own offline text recognition, for apps that
//!      hide their buttons (Spotify, games, many web and Electron apps);
//!   3. when nothing matches the words you used, the local brain picks the closest
//!      thing it can see ("the send thing" -> "Send message").

use tauri::{AppHandle, Manager};
use windows::core::{Interface, HSTRING};
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

/// Something on screen SYLVER could act on.
pub struct Found {
    pub x: i32,
    pub y: i32,
    pub el: Option<IUIAutomationElement>,
    pub how: &'static str,
}

struct Seen {
    name: String,
    kind: i32,
    rect: RECT,
    el: IUIAutomationElement,
}

pub struct Word {
    pub text: String,
    pub rect: RECT,
}

pub struct Line {
    pub text: String,
    pub words: Vec<Word>,
}

/* ---------------- layer 1: what the app says is there ---------------- */

const BUTTON: i32 = 50000;
const CHECK: i32 = 50002;
const COMBO: i32 = 50003;
const EDIT: i32 = 50004;
const LINK: i32 = 50005;
const LIST_ITEM: i32 = 50007;
const MENU_ITEM: i32 = 50011;
const RADIO: i32 = 50013;
const TAB_ITEM: i32 = 50019;
const TREE_ITEM: i32 = 50024;
const DATA_ITEM: i32 = 50029;
const DOCUMENT: i32 = 50030;
const SPLIT: i32 = 50031;

/// Every named, visible, enabled control in a window, in one trip to the app.
fn elements(h: HWND) -> Vec<Seen> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let uia: IUIAutomation = match CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) {
            Ok(u) => u,
            Err(_) => match CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
                Ok(u) => u,
                Err(_) => return Vec::new(),
            },
        };
        // a frozen app can't hold SYLVER up for long
        if let Ok(u2) = uia.cast::<IUIAutomation2>() {
            let _ = u2.SetTransactionTimeout(2500);
            let _ = u2.SetConnectionTimeout(1500);
        }
        let Ok(cache) = uia.CreateCacheRequest() else { return Vec::new() };
        for p in [UIA_NamePropertyId, UIA_ControlTypePropertyId, UIA_BoundingRectanglePropertyId, UIA_IsOffscreenPropertyId, UIA_IsEnabledPropertyId] {
            let _ = cache.AddProperty(p);
        }
        let (Ok(root), Ok(cond)) = (uia.ElementFromHandle(h), uia.ControlViewCondition()) else { return Vec::new() };
        let Ok(all) = root.FindAllBuildCache(TreeScope_Descendants, &cond, &cache) else { return Vec::new() };
        let n = all.Length().unwrap_or(0).min(9000);
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            let Ok(e) = all.GetElement(i) else { continue };
            let name = e.CachedName().map(|b| b.to_string()).unwrap_or_default();
            let name = name.trim();
            if name.is_empty()
                || e.CachedIsOffscreen().map(|b| b.as_bool()).unwrap_or(false)
                || !e.CachedIsEnabled().map(|b| b.as_bool()).unwrap_or(true)
            {
                continue;
            }
            let rect = e.CachedBoundingRectangle().unwrap_or_default();
            if rect.right - rect.left < 3 || rect.bottom - rect.top < 3 {
                continue;
            }
            let kind = e.CachedControlType().map(|c| c.0).unwrap_or(0);
            out.push(Seen { name: name.chars().take(160).collect(), kind, rect, el: e });
        }
        out
    }
}

/// What a window says is on it, in one look: (is it a box you type in, its name, where it is).
pub fn seen(h: HWND) -> Vec<(bool, String, RECT)> {
    elements(h).into_iter().map(|e| (e.kind == EDIT, e.name, e.rect)).collect()
}

/// One thing on screen the task agent can point at by number.
pub struct Mark {
    pub kind: &'static str,
    pub name: String,
    pub x: i32,
    pub y: i32,
}

static MARKS: std::sync::Mutex<Vec<Mark>> = std::sync::Mutex::new(Vec::new());

/// The centre of mark `n` from the last look (numbers start at 1), and what it was.
pub fn mark(n: usize) -> Option<(i32, i32, String)> {
    MARKS.lock().unwrap().get(n.checked_sub(1)?).map(|m| (m.x, m.y, format!("{} “{}”", m.kind, m.name)))
}

/// Seeing a window the way a person does: everything on it, numbered. The app's own controls
/// (buttons, boxes, tabs, links: what it reports about itself) and, laid over them, every piece of
/// text that exists only as pixels (read off the screen), each with where it is. The agent then
/// clicks or types "at 12" instead of hoping a name matches, which is what used to confuse it in
/// apps that describe themselves badly.
///
/// For apps that say little or nothing about themselves, a third layer: the icons and buttons the
/// icon detector finds in the picture itself (OmniParser's way of parsing a screen), so those can be
/// pointed at too. All three together take well under a second.
pub fn map(app: &AppHandle, h: HWND) -> String {
    let Some(win) = window_rect(h) else { return String::new() };
    let (ww, wh) = ((win.right - win.left).max(1), (win.bottom - win.top).max(1));
    let kind = |k: i32| match k {
        BUTTON | SPLIT => "button",
        CHECK => "checkbox",
        COMBO => "dropdown",
        EDIT => "text box",
        LINK => "link",
        LIST_ITEM | TREE_ITEM => "item",
        MENU_ITEM => "menu",
        RADIO => "option",
        TAB_ITEM => "tab",
        DATA_ITEM => "row",
        _ => "",
    };
    let inside = |r: &RECT| r.right > win.left && r.left < win.right && r.bottom > win.top && r.top < win.bottom;
    let mut found: Vec<(&'static str, String, RECT)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for e in elements(h) {
        let k = kind(e.kind);
        if k.is_empty() || !inside(&e.rect) {
            continue;
        }
        let name: String = e.name.replace(['\n', '\r'], " ").chars().take(60).collect();
        // the same name twice in the same spot is one thing
        if seen.insert((norm(&name), e.rect.left / 24, e.rect.top / 24)) {
            found.push((k, name, e.rect));
        }
        if found.len() >= 70 {
            break;
        }
    }
    // text that's only pixels: whatever the reader finds that no control already accounts for
    let controls = found.len();
    for line in read(win, 1) {
        let text = line.text.trim();
        if text.chars().filter(|c| c.is_alphanumeric()).count() < 2 || line.words.is_empty() {
            continue;
        }
        let r = line.words.iter().fold(line.words[0].rect, |a, w| RECT { left: a.left.min(w.rect.left), top: a.top.min(w.rect.top), right: a.right.max(w.rect.right), bottom: a.bottom.max(w.rect.bottom) });
        let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
        let q = norm(text);
        let known = found[..controls].iter().any(|(_, n, c)| cx >= c.left - 4 && cx <= c.right + 4 && cy >= c.top - 4 && cy <= c.bottom + 4 && (similar(&norm(n), &q) >= 0.55 || norm(n).contains(&q) || q.contains(&norm(n))));
        if !known && found.len() < 110 {
            found.push(("text", text.chars().take(70).collect(), r));
        }
    }
    // what the app doesn't say and nobody can read: icons. Only when the app itself named few things
    // (a well-described app already lists its buttons, and its icons would only be listed twice).
    if controls < 25 && crate::icons::available(app) {
        if let Some(px) = grab(win) {
            for r in icon_boxes(app, &px, &win) {
                let (cx, cy) = centre(&r);
                let known = found.iter().any(|(_, _, c)| cx >= c.left - 3 && cx <= c.right + 3 && cy >= c.top - 3 && cy <= c.bottom + 3);
                if !known && found.len() < 150 {
                    found.push(("icon", String::new(), r));
                }
            }
        }
    }
    // reading order: top to bottom, left to right
    found.sort_by_key(|(_, _, r)| ((r.top + r.bottom) / 2 / 22, r.left));
    let place = |x: i32, y: i32| {
        let (fx, fy) = ((x - win.left) as f32 / ww as f32, (y - win.top) as f32 / wh as f32);
        let v = if fy < 0.28 { "top" } else if fy > 0.72 { "bottom" } else { "" };
        let hz = if fx < 0.3 { "left" } else if fx > 0.7 { "right" } else { "" };
        match (v, hz) {
            ("", "") => "middle".to_string(),
            ("", h) => h.to_string(),
            (v, "") => v.to_string(),
            (v, h) => format!("{v} {h}"),
        }
    };
    let mut marks = Vec::with_capacity(found.len());
    let mut out = String::new();
    for (i, (k, name, r)) in found.into_iter().enumerate() {
        let (x, y) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
        if name.is_empty() {
            out.push_str(&format!("{} {k} · {}\n", i + 1, place(x, y)));
        } else {
            out.push_str(&format!("{} {k} “{name}” · {}\n", i + 1, place(x, y)));
        }
        marks.push(Mark { kind: k, name, x, y });
    }
    *MARKS.lock().unwrap() = marks;
    out
}

/// The icon detector's boxes for a window's picture, in screen pixels.
fn icon_boxes(app: &AppHandle, bgra: &[u8], win: &RECT) -> Vec<RECT> {
    let (w, h) = ((win.right - win.left).max(1) as usize, (win.bottom - win.top).max(1) as usize);
    crate::icons::find(app, bgra, w, h).into_iter().map(|(r, _)| RECT { left: win.left + r.left, top: win.top + r.top, right: win.left + r.right, bottom: win.top + r.bottom }).collect()
}

/// A spot someone pointed at, moved onto the thing it's on: the smallest known box that holds it (or the
/// nearest one a few pixels away), unless that box is too big to be one button. This is what makes a
/// pointing model's "about here" into an exact click.
pub fn snap(x: i32, y: i32, boxes: &[RECT]) -> (i32, i32, bool) {
    let area = |r: &RECT| ((r.right - r.left) as i64 * (r.bottom - r.top) as i64).max(1);
    let one_thing = |r: &RECT| r.right - r.left <= 340 && r.bottom - r.top <= 120;
    let holds = |r: &RECT, m: i32| x >= r.left - m && x <= r.right + m && y >= r.top - m && y <= r.bottom + m;
    if let Some(r) = boxes.iter().filter(|r| one_thing(r) && holds(r, 5)).min_by_key(|r| area(r)) {
        let (cx, cy) = centre(r);
        return (cx, cy, true);
    }
    let gap = |r: &RECT| {
        let dx = (r.left - x).max(x - r.right).max(0) as f64;
        let dy = (r.top - y).max(y - r.bottom).max(0) as f64;
        (dx * dx + dy * dy).sqrt()
    };
    match boxes.iter().filter(|r| one_thing(r)).map(|r| (gap(r), r)).filter(|(d, _)| *d <= 26.0).min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal)) {
        Some((_, r)) => {
            let (cx, cy) = centre(r);
            (cx, cy, true)
        }
        None => (x, y, false),
    }
}

/// Press a button in a window by what it's called (tried in the order given). Used to answer or
/// decline a call on the calling app's own banner or call window.
pub fn press_button(h: HWND, names: &[&str]) -> bool {
    let els = elements(h);
    for want in names {
        let hit = els.iter().find(|e| {
            let n = norm(&e.name);
            matches!(e.kind, BUTTON | SPLIT | LINK | MENU_ITEM | LIST_ITEM) && (n == *want || n.starts_with(&format!("{want} ")))
        });
        if let Some(e) = hit {
            let invoked = unsafe { e.el.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId).and_then(|p| p.Invoke()).is_ok() };
            if !invoked {
                use windows::Win32::Foundation::POINT;
                use windows::Win32::UI::Input::KeyboardAndMouse::{mouse_event, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP};
                use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};
                unsafe {
                    let mut old = POINT::default();
                    let _ = GetCursorPos(&mut old);
                    let _ = SetCursorPos((e.rect.left + e.rect.right) / 2, (e.rect.top + e.rect.bottom) / 2);
                    std::thread::sleep(std::time::Duration::from_millis(40));
                    mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
                    mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
                    std::thread::sleep(std::time::Duration::from_millis(40));
                    let _ = SetCursorPos(old.x, old.y);
                }
            }
            return true;
        }
    }
    false
}

/* ---------------- layer 2: reading the pixels ---------------- */

/// What's on screen in a rectangle, as BGRA pixels (top row first).
pub fn grab(r: RECT) -> Option<Vec<u8>> {
    use windows::Win32::Graphics::Gdi::*;
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    if w <= 0 || h <= 0 {
        return None;
    }
    unsafe {
        let sdc = GetDC(None);
        let mdc = CreateCompatibleDC(Some(sdc));
        let bmp = CreateCompatibleBitmap(sdc, w, h);
        let old = SelectObject(mdc, bmp.into());
        let ok = BitBlt(mdc, 0, 0, w, h, Some(sdc), r.left, r.top, SRCCOPY | CAPTUREBLT).is_ok();
        let mut bi = BITMAPINFO::default();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w;
        bi.bmiHeader.biHeight = -h; // top-down
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let lines = GetDIBits(mdc, bmp, 0, h as u32, Some(buf.as_mut_ptr() as _), &mut bi, DIB_RGB_COLORS);
        SelectObject(mdc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mdc);
        ReleaseDC(None, sdc);
        (ok && lines > 0).then_some(buf)
    }
}

fn engine() -> Option<OcrEngine> {
    OcrEngine::TryCreateFromUserProfileLanguages().ok().or_else(|| {
        let en = windows::Globalization::Language::CreateLanguage(&HSTRING::from("en-US")).ok()?;
        OcrEngine::TryCreateFromLanguage(&en).ok()
    })
}

/// Text in a grayscale image, with where each word is (in image pixels).
pub fn recognize(gray: &[u8], w: u32, h: u32) -> Option<Vec<Line>> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let writer = DataWriter::new().ok()?;
    writer.WriteBytes(gray).ok()?;
    let buf = writer.DetachBuffer().ok()?;
    let bmp = SoftwareBitmap::CreateCopyFromBuffer(&buf, BitmapPixelFormat::Gray8, w as i32, h as i32).ok()?;
    let res = engine()?.RecognizeAsync(&bmp).ok()?.get().ok()?;
    let mut out = Vec::new();
    for line in res.Lines().ok()? {
        let mut words = Vec::new();
        for wd in line.Words().ok()? {
            let Ok(r) = wd.BoundingRect() else { continue };
            words.push(Word {
                text: wd.Text().map(|t| t.to_string()).unwrap_or_default(),
                rect: RECT { left: r.X as i32, top: r.Y as i32, right: (r.X + r.Width) as i32, bottom: (r.Y + r.Height) as i32 },
            });
        }
        out.push(Line { text: line.Text().map(|t| t.to_string()).unwrap_or_default(), words });
    }
    Some(out)
}

/// Read the text inside a rectangle of the screen (word positions in screen pixels).
/// `zoom` 2 doubles the picture first, which helps with small writing.
pub fn read(area: RECT, zoom: usize) -> Vec<Line> {
    let Some(px) = grab(area) else { return Vec::new() };
    let (w, h) = ((area.right - area.left) as usize, (area.bottom - area.top) as usize);
    let mut gray: Vec<u8> = px.chunks_exact(4).map(|p| ((p[2] as u32 * 77 + p[1] as u32 * 150 + p[0] as u32 * 29) >> 8) as u8).collect();
    // dark themes: the reader likes dark writing on a light page
    let mean = gray.iter().map(|&v| v as u64).sum::<u64>() / gray.len().max(1) as u64;
    if mean < 110 {
        gray.iter_mut().for_each(|v| *v = 255 - *v);
    }
    let max = OcrEngine::MaxImageDimension().unwrap_or(4096) as usize;
    let z = if zoom >= 2 && w * 2 <= max && h * 2 <= max { 2 } else { 1 };
    if w * z > max || h * z > max {
        return Vec::new();
    }
    let img = if z == 2 {
        let mut big = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let v = gray[y * w + x];
                let i = (y * 2) * (w * 2) + x * 2;
                big[i] = v;
                big[i + 1] = v;
                big[i + w * 2] = v;
                big[i + w * 2 + 1] = v;
            }
        }
        big
    } else {
        gray
    };
    let Some(mut lines) = recognize(&img, (w * z) as u32, (h * z) as u32) else { return Vec::new() };
    let zi = z as i32;
    for l in &mut lines {
        for wd in &mut l.words {
            wd.rect = RECT {
                left: area.left + wd.rect.left / zi,
                top: area.top + wd.rect.top / zi,
                right: area.left + wd.rect.right / zi,
                bottom: area.top + wd.rect.bottom / zi,
            };
        }
    }
    lines
}

fn window_rect(h: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    unsafe { GetWindowRect(h, &mut r).ok()? };
    (r.right > r.left && r.bottom > r.top).then_some(r)
}

/// All the text SYLVER can see in a window, by reading its pixels.
pub fn text_of(h: HWND, limit: usize) -> String {
    let Some(r) = window_rect(h) else { return String::new() };
    let mut out = String::new();
    for l in read(r, 1) {
        if out.len() > limit {
            break;
        }
        out.push_str(&l.text);
        out.push('\n');
    }
    out
}

/* ---------------- matching words to what's on screen ---------------- */

/// lowercase letters and digits, single spaces
pub fn norm(s: &str) -> String {
    let t: String = s.chars().map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { ' ' }).collect();
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn lev(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + usize::from(a[i - 1] != b[j - 1]));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// How well something on screen matches what you asked for (0 to 1). Both normalized.
pub fn similar(cand: &str, q: &str) -> f64 {
    if cand.is_empty() || q.is_empty() {
        return 0.0;
    }
    if cand == q {
        return 1.0;
    }
    if cand.trim_end_matches('s') == q.trim_end_matches('s') {
        return 0.97;
    }
    let cw: Vec<&str> = cand.split(' ').collect();
    let qw: Vec<&str> = q.split(' ').collect();
    let ratio = (q.len() as f64 / cand.len() as f64).min(1.0);
    let mut s: f64 = 0.0;
    // the words, in order, inside a longer name ("send" in "send message")
    if qw.len() <= cw.len() && cw.windows(qw.len()).any(|w| w == qw.as_slice()) {
        s = 0.68 + 0.27 * ratio + if cw.starts_with(&qw) { 0.05 } else { 0.0 };
    } else if q.len() >= 4 && cand.contains(q) {
        s = 0.5 + 0.3 * ratio;
    }
    // typos, and letters the reader got wrong ("Bi1lie Jean")
    let long = cand.chars().count().max(q.chars().count());
    if long <= 80 {
        let r = 1.0 - lev(cand, q) as f64 / long as f64;
        if r >= 0.72 {
            s = s.max(r * 0.95);
        }
    }
    // most of the words are there
    let hit = qw.iter().filter(|w| w.len() > 1 && cw.contains(w)).count();
    if hit > 0 {
        s = s.max(0.6 * hit as f64 / qw.len() as f64 + 0.25 * hit as f64 / cw.len() as f64);
    }
    s
}

/// "the play button" -> ("play", Some(button)); "search box" -> ("search", field)
fn what_kind(q: &str) -> (String, Option<&'static [i32]>) {
    let q = norm(q);
    let q = q.strip_prefix("the ").unwrap_or(&q).to_string();
    const KINDS: [(&str, &[i32]); 10] = [
        (" button", &[BUTTON, SPLIT]),
        (" icon", &[BUTTON, SPLIT, LIST_ITEM]),
        (" link", &[LINK]),
        (" tab", &[TAB_ITEM]),
        (" checkbox", &[CHECK]),
        (" check box", &[CHECK]),
        (" option", &[RADIO, LIST_ITEM, MENU_ITEM, COMBO]),
        (" menu", &[MENU_ITEM, BUTTON]),
        (" box", &[EDIT, COMBO, DOCUMENT]),
        (" field", &[EDIT, COMBO, DOCUMENT]),
    ];
    for (suffix, kinds) in KINDS {
        if let Some(core) = q.strip_suffix(suffix).filter(|c| !c.is_empty()) {
            return (core.to_string(), Some(kinds));
        }
    }
    for suffix in [" bar", " input", " textbox", " text box", " area"] {
        if let Some(core) = q.strip_suffix(suffix).filter(|c| !c.is_empty()) {
            return (core.to_string(), Some(&[EDIT, COMBO, DOCUMENT]));
        }
    }
    (q, None)
}

fn clickable(kind: i32) -> bool {
    matches!(kind, BUTTON | CHECK | COMBO | EDIT | LINK | LIST_ITEM | MENU_ITEM | RADIO | TAB_ITEM | TREE_ITEM | DATA_ITEM | SPLIT)
}

fn centre(r: &RECT) -> (i32, i32) {
    ((r.left + r.right) / 2, (r.top + r.bottom) / 2)
}

/// The best run of words in the text read off the screen.
fn best_words(lines: &[Line], q: &str) -> Option<(f64, RECT)> {
    let qn = q.split(' ').count();
    let mut best: Option<(f64, RECT)> = None;
    for l in lines {
        let words: Vec<String> = l.words.iter().map(|w| norm(&w.text)).collect();
        for k in qn.saturating_sub(1).max(1)..=qn + 1 {
            if k > words.len() {
                break;
            }
            for i in 0..=words.len() - k {
                let joined = words[i..i + k].join(" ");
                let s = similar(&joined, q) - if k != qn { 0.03 } else { 0.0 };
                if best.as_ref().map(|b| s > b.0).unwrap_or(true) {
                    let r = l.words[i..i + k].iter().fold(l.words[i].rect, |a, w| RECT {
                        left: a.left.min(w.rect.left),
                        top: a.top.min(w.rect.top),
                        right: a.right.max(w.rect.right),
                        bottom: a.bottom.max(w.rect.bottom),
                    });
                    best = Some((s, r));
                }
            }
        }
    }
    best
}

/// Find written text inside a part of the screen. Tries again with a closer look if needed.
pub fn find_text(area: RECT, q: &str, prefer: impl Fn(&RECT) -> f64) -> Option<(i32, i32)> {
    let q = norm(q);
    for zoom in [1, 2] {
        let lines = read(area, zoom);
        let mut best: Option<(f64, RECT)> = None;
        // every line, scored, so a better-placed match can win a near tie
        for l in &lines {
            if let Some((s, r)) = best_words(std::slice::from_ref(l), &q) {
                let s = s + prefer(&r);
                if best.as_ref().map(|b| s > b.0).unwrap_or(true) {
                    best = Some((s, r));
                }
            }
        }
        if let Some((s, r)) = best.filter(|b| b.0 >= 0.74) {
            crate::log_line(&format!("vision: read “{q}” on screen ({s:.2}, zoom {zoom})"));
            return Some(centre(&r));
        }
    }
    None
}

/// Find what you named in a window: a button, link, tab, field, or just words on it.
/// `field` prefers places you can type into.
pub fn locate(app: &AppHandle, h: HWND, what: &str, field: bool) -> Option<Found> {
    let t0 = std::time::Instant::now();
    let (q, kinds) = what_kind(what);
    let kinds: Option<&[i32]> = if field { Some(kinds.unwrap_or(&[EDIT, COMBO, DOCUMENT])) } else { kinds };
    let win = window_rect(h)?;
    let inside = |r: &RECT| r.left < win.right && r.right > win.left && r.top < win.bottom && r.bottom > win.top;

    // 1. the app's own list of controls
    let els = elements(h);
    let mut best: Option<(f64, usize)> = None;
    for (i, e) in els.iter().enumerate() {
        if !inside(&e.rect) {
            continue;
        }
        let mut s = similar(&norm(&e.name), &q);
        if s < 0.5 {
            continue;
        }
        if let Some(k) = kinds {
            s += if k.contains(&e.kind) { 0.1 } else { -0.08 };
        } else if clickable(e.kind) {
            s += 0.06;
        }
        if best.map(|b| s > b.0).unwrap_or(true) {
            best = Some((s, i));
        }
    }
    if let Some((s, i)) = best.filter(|b| b.0 >= 0.78) {
        let e = &els[i];
        let (x, y) = centre(&e.rect);
        crate::log_line(&format!("vision: “{q}” is a control ({s:.2}, {} elements, {} ms)", els.len(), t0.elapsed().as_millis()));
        return Some(Found { x, y, el: Some(e.el.clone()), how: "controls" });
    }

    // 2. read the window's pixels
    let mut seen_lines: Vec<Line> = Vec::new();
    for zoom in [1, 2] {
        if zoom == 2 && ((win.right - win.left) as i64 * (win.bottom - win.top) as i64) > 3_000_000 {
            break;
        }
        let lines = read(win, zoom);
        if let Some((s, r)) = best_words(&lines, &q).filter(|b| b.0 >= 0.76) {
            let (x, y) = centre(&r);
            crate::log_line(&format!("vision: read “{q}” on screen ({s:.2}, zoom {zoom}, {} ms)", t0.elapsed().as_millis()));
            return Some(Found { x, y, el: None, how: "reading" });
        }
        if zoom == 1 {
            seen_lines = lines;
        }
    }

    // Something described by how it looks ("the gear icon", "the blue button", "the three dots") has no
    // name to pick from a list: that goes straight to the model that looks at the picture.
    let wl = format!(" {} ", what.to_lowercase());
    let by_looks = crate::see::pointer_available(app)
        && [" icon", " logo", " picture", " image", " symbol", " arrow", " dots", " gear", " cog", " bell", " heart", " star", " trash", " bin ", " pencil", " plus", " minus", " cross", " tick",
            " magnif", " hamburger", " avatar", " thumbnail", " red ", " blue ", " green ", " yellow ", " orange ", " purple ", " black ", " white ", " grey ", " gray ", " round ", " big ", " small "]
            .iter()
            .any(|w| wl.contains(w));

    // 3. nothing matched your words: let the brain pick from what's there (a second or two)
    let picked = (|| -> Option<Found> {
        if by_looks || (!app.state::<crate::ai::Ai>().is_ready() && crate::quick::port().is_none()) {
            return None;
        }
        let mut names: Vec<(String, (i32, i32), Option<IUIAutomationElement>)> = Vec::new();
        for e in els.iter().filter(|e| inside(&e.rect) && (clickable(e.kind) || e.kind == 50020) && e.name.chars().count() <= 60) {
            if !names.iter().any(|n| n.0.eq_ignore_ascii_case(&e.name)) {
                names.push((e.name.clone(), centre(&e.rect), Some(e.el.clone())));
            }
        }
        for l in seen_lines.iter().filter(|l| !l.words.is_empty() && l.text.chars().count() <= 50) {
            if !names.iter().any(|n| n.0.eq_ignore_ascii_case(&l.text)) {
                let r = l.words.iter().fold(l.words[0].rect, |a, w| RECT { left: a.left.min(w.rect.left), top: a.top.min(w.rect.top), right: a.right.max(w.rect.right), bottom: a.bottom.max(w.rect.bottom) });
                names.push((l.text.clone(), centre(&r), None));
            }
        }
        names.truncate(80);
        if names.is_empty() {
            return None;
        }
        let list: String = names.iter().enumerate().map(|(i, n)| format!("{}. {}\n", i + 1, n.0)).collect();
        let ask = format!("The user wants to {} “{what}”. These are on their screen:\n{list}\nWhich number is it? Reply with only the number, or 0 if none of them fits.", if field { "type into" } else { "click" });
        let pick = crate::ai::oneshot(app, "You help SYLVER find the right thing on the user's screen. Reply with only a number.", &ask, 6, |_| {})?;
        let n: usize = pick.split(|c: char| !c.is_ascii_digit()).find(|s| !s.is_empty())?.parse().ok()?;
        let (name, (x, y), el) = names.into_iter().nth(n.checked_sub(1)?)?;
        crate::log_line(&format!("vision: the brain picked “{name}” for “{what}” ({} ms)", t0.elapsed().as_millis()));
        Some(Found { x, y, how: if el.is_some() { "controls" } else { "reading" }, el })
    })();
    if picked.is_some() {
        return picked;
    }

    // 4. it has no name anywhere (an icon, "the blue button"): UI-TARS looks and points.
    if !crate::see::pointer_available(app) || crate::ai::cancelled() {
        return None;
    }
    let pic = grab(win);
    let icons: Vec<RECT> = pic.as_ref().map(|p| icon_boxes(app, p, &win)).unwrap_or_default();
    //    The quick way: only what the icon detector boxed, cut out and laid on one small sheet (a few seconds).
    //    Not when the words say WHERE it is ("the icon at the top right", "the second one"): a sheet has no where.
    let placed = [" top", " bottom", " left", " right", " corner", " next to", " near ", " under", " above", " below", " beside", " first", " second", " third", " last", " middle"].iter().any(|w| wl.contains(w));
    //    (the sheet holds the icon-shaped ones: small and roughly square, the surest four dozen)
    let small: Vec<RECT> = icons.iter().filter(|r| {
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        w.max(h) <= 110 && w * 5 >= h * 2 && h * 5 >= w * 2
    }).take(48).cloned().collect();
    if let (false, Some(p), true) = (placed, pic.as_ref(), small.len() >= 2) {
        let local: Vec<RECT> = small.iter().map(|r| RECT { left: r.left - win.left, top: r.top - win.top, right: r.right - win.left, bottom: r.bottom - win.top }).collect();
        if let Some(i) = crate::see::pick(app, p, (win.right - win.left) as u32, (win.bottom - win.top) as u32, &local, what) {
            let (x, y) = centre(&small[i]);
            crate::log_line(&format!("vision: UI-TARS picked “{what}” out of {} things the detector marked, at ({x}, {y}) ({} ms in all)", small.len(), t0.elapsed().as_millis()));
            return Some(Found { x, y, el: None, how: "pointing" });
        }
    }
    //    The thorough way: the whole window (slower), and its spot is moved onto the nearest thing the
    //    instant map knows (a control, a word, an icon), which makes the click exact.
    let (px, py) = crate::see::point(app, h, what)?;
    let mut boxes: Vec<RECT> = els.iter().filter(|e| inside(&e.rect) && (clickable(e.kind) || e.kind == 50020)).map(|e| e.rect).collect();
    for l in seen_lines.iter().filter(|l| !l.words.is_empty()) {
        boxes.push(l.words.iter().fold(l.words[0].rect, |a, w| RECT { left: a.left.min(w.rect.left), top: a.top.min(w.rect.top), right: a.right.max(w.rect.right), bottom: a.bottom.max(w.rect.bottom) }));
    }
    boxes.extend(icons);
    let (x, y, snapped) = snap(px, py, &boxes);
    crate::log_line(&format!("vision: UI-TARS found “{what}” at ({px}, {py}){} ({} ms in all)", if snapped { format!(", moved onto the thing there ({x}, {y})") } else { String::new() }, t0.elapsed().as_millis()));
    Some(Found { x, y, el: None, how: "pointing" })
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapping() {
        let b = |l, t, r, bt| RECT { left: l, top: t, right: r, bottom: bt };
        let boxes = [b(100, 100, 132, 132), b(0, 80, 900, 150), b(300, 300, 420, 340), b(0, 0, 1900, 1000)];
        // on the icon (which sits inside a toolbar and the whole window): the icon wins, at its centre
        assert_eq!(snap(104, 128, &boxes), (116, 116, true));
        // just beside a button: moved onto it
        assert_eq!(snap(430, 322, &boxes), (360, 320, true));
        // open space (only the toolbar strip and the window hold it, and those are too big to be one thing): left where it is
        assert_eq!(snap(700, 600, &boxes), (700, 600, false));
    }

    #[test]
    fn matching() {
        assert!(similar("send", "send") == 1.0);
        assert!(similar("send message", "send") >= 0.78);
        assert!(similar("bi1lie jean", "billie jean") >= 0.76);
        assert!(similar("settings", "setting") >= 0.9);
        assert!(similar("new tab", "save") < 0.5);
        assert!(similar("terms of service and privacy policy for this website", "service") < 0.78);
        assert_eq!(what_kind("the Play button").0, "play");
        assert_eq!(what_kind("search box").0, "search");
    }

    /// Draw some words with GDI, then read them back with the offline reader.
    #[test]
    fn reads_text() {
        use windows::Win32::Graphics::Gdi::*;
        let (w, h) = (900i32, 140i32);
        let gray = unsafe {
            let sdc = GetDC(None);
            let mdc = CreateCompatibleDC(Some(sdc));
            let bmp = CreateCompatibleBitmap(sdc, w, h);
            let old = SelectObject(mdc, bmp.into());
            let white = CreateSolidBrush(windows::Win32::Foundation::COLORREF(0x00FF_FFFF));
            FillRect(mdc, &RECT { left: 0, top: 0, right: w, bottom: h }, white);
            let font = CreateFontW(34, 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, &HSTRING::from("Segoe UI"));
            SelectObject(mdc, font.into());
            let mut t: Vec<u16> = "Billie Jean   Michael Jackson   Send message".encode_utf16().collect();
            let _ = DrawTextW(mdc, &mut t, &mut RECT { left: 20, top: 40, right: w, bottom: h }, DT_LEFT | DT_SINGLELINE);
            let mut bi = BITMAPINFO::default();
            bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = w;
            bi.bmiHeader.biHeight = -h;
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            let mut buf = vec![0u8; (w * h * 4) as usize];
            GetDIBits(mdc, bmp, 0, h as u32, Some(buf.as_mut_ptr() as _), &mut bi, DIB_RGB_COLORS);
            SelectObject(mdc, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteObject(font.into());
            let _ = DeleteDC(mdc);
            ReleaseDC(None, sdc);
            buf.chunks_exact(4).map(|p| ((p[2] as u32 * 77 + p[1] as u32 * 150 + p[0] as u32 * 29) >> 8) as u8).collect::<Vec<u8>>()
        };
        let t0 = std::time::Instant::now();
        let lines = recognize(&gray, w as u32, h as u32).expect("Windows OCR is available");
        let text: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        println!("read {text:?} in {} ms", t0.elapsed().as_millis());
        let (s, r) = best_words(&lines, "billie jean").unwrap();
        println!("billie jean -> {s:.2} at {:?}", (r.left, r.top, r.right, r.bottom));
        assert!(s >= 0.9 && r.left < 60);
        assert!(best_words(&lines, "send message").unwrap().0 >= 0.9);
    }

    /// One snapshot of the taskbar's controls: how long it takes.
    #[test]
    fn snapshot_speed() {
        use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
        let h = unsafe { FindWindowW(&HSTRING::from("Shell_TrayWnd"), None) }.unwrap();
        let t0 = std::time::Instant::now();
        let els = elements(h);
        println!("{} controls in {} ms", els.len(), t0.elapsed().as_millis());
        assert!(!els.is_empty());
    }
}

/// Only the boxes you can type in (a search box, a message box), asked for by kind. A chat app lists thousands
/// of things when asked for everything on it, and each such look took seconds (finding a chat that way took
/// 24 s, measured); the boxes alone come back at once.
pub fn edits(h: HWND) -> Vec<(String, RECT)> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let uia: IUIAutomation = match CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) {
            Ok(u) => u,
            Err(_) => match CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
                Ok(u) => u,
                Err(_) => return Vec::new(),
            },
        };
        if let Ok(u2) = uia.cast::<IUIAutomation2>() {
            let _ = u2.SetTransactionTimeout(2500);
            let _ = u2.SetConnectionTimeout(1500);
        }
        let Ok(cache) = uia.CreateCacheRequest() else { return Vec::new() };
        for p in [UIA_NamePropertyId, UIA_BoundingRectanglePropertyId, UIA_IsOffscreenPropertyId, UIA_IsEnabledPropertyId] {
            let _ = cache.AddProperty(p);
        }
        let kind = windows::Win32::System::Variant::VARIANT::from(EDIT);
        let (Ok(root), Ok(cond)) = (uia.ElementFromHandle(h), uia.CreatePropertyCondition(UIA_ControlTypePropertyId, &kind)) else { return Vec::new() };
        let Ok(all) = root.FindAllBuildCache(TreeScope_Descendants, &cond, &cache) else { return Vec::new() };
        let mut out = Vec::new();
        for i in 0..all.Length().unwrap_or(0).min(200) {
            let Ok(e) = all.GetElement(i) else { continue };
            if e.CachedIsOffscreen().map(|b| b.as_bool()).unwrap_or(false) || !e.CachedIsEnabled().map(|b| b.as_bool()).unwrap_or(true) {
                continue;
            }
            let rect = e.CachedBoundingRectangle().unwrap_or_default();
            if rect.right - rect.left < 20 || rect.bottom - rect.top < 8 {
                continue;
            }
            out.push((e.CachedName().map(|b| b.to_string()).unwrap_or_default().trim().chars().take(160).collect(), rect));
        }
        out
    }
}
