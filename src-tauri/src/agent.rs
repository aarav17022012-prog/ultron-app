//! Multi-step jobs on the PC: "open Notepad and write hello", "open chrome, search
//! for cats", "write a poem about rain and save it as rain". Each step runs through
//! Windows itself (launching, keyboard, UI Automation), and ULTRON's own cursor
//! shows what it's doing. Typing stops at once if you switch to another window.
//! It also shops and searches sites ("find headphones under 2000 on amazon") and
//! builds whole PowerPoint decks ("make a presentation about black holes").

use std::time::{Duration, Instant};

use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Threading::{GetCurrentProcessId, OpenProcess, WaitForInputIdle, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE};
use windows::Win32::UI::Accessibility::{IUIAutomationInvokePattern, UIA_InvokePatternId};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetCursorPos, GetForegroundWindow, GetGUIThreadInfo, GetSystemMetrics, GetWindowRect, GetWindowTextLengthW,
    GetWindowThreadProcessId, IsWindowVisible, SetCursorPos, ShowWindow, GA_ROOTOWNER, GUITHREADINFO, SM_CXSCREEN,
    SM_CYSCREEN, SW_MAXIMIZE, SW_MINIMIZE,
};

use crate::commands::{self, Reply};
use crate::ghost;

#[derive(Debug, Clone)]
pub enum Step {
    Open(String),
    Write(String),   // exactly these words
    Compose(String), // "a poem about rain": the local AI writes it as it types
    Keys(Vec<VIRTUAL_KEY>, String, u32),
    SaveAs(Option<String>),
    Click(String),
    TypeIn(String, String), // (field, text): "type cats in the search box"
    Wait(f32),
    Minimize,
    Maximize,
    SwitchTo(String),
    Message(String, String, String),  // (app, person, text): typed, then sent only after you confirm
    Email(String, String, String),    // (to, subject, body): opens a ready-to-send draft
    Rewrite(String, bool),            // (instruction, paste back): writing tools on the selected text
    Screen(String),                   // question about what's on screen ("" = summarize it)
    Timer(u64, String, String),       // (seconds, label, kind: timer | reminder)
    Snap(i32),                        // -1 left, 1 right, 2 next screen
    CloseWindow,
    Brightness(i32, bool),            // (value, relative)
    DarkMode(Option<bool>),
    SettingsPage(String, String),     // (name, ms-settings uri)
    FindFile(String),
    LatestDownload,
    NewFolder(String, String),        // (name, where)
    SiteSearch(String, String), // (site, what to look for, with any "under 2000" / "cheapest")
    Present(String, usize),     // (topic, number of slides)
    Command(String),            // anything the regular commands handle (volume, search, music...)
    Ui(String, String, String), // (action, control, value): a control in the window acted on by name (uictl.rs)
}

/* ---------------- understanding the request ---------------- */

const VERBS: &[&str] = &[
    "open", "launch", "start", "run", "go to", "type", "write", "compose", "draft", "dictate", "press", "hit", "save", "click", "tap",
    "select all", "copy", "paste", "undo", "redo", "wait", "search", "google", "look up", "play", "pause", "resume", "close", "quit",
    "exit", "minimize", "minimise", "maximize", "maximise", "new tab", "new window", "new file", "switch to", "go back", "refresh",
    "reload", "scroll", "mute", "unmute", "set the volume", "set volume", "turn", "volume", "take a screenshot", "lock", "show desktop",
    "skip", "next song", "previous song", "delete", "clear", "enter", "find", "show me", "get me", "buy", "shop for", "order",
    "look for", "make", "create", "build", "generate", "prepare", "message", "text", "tell", "email", "send", "rewrite", "rephrase",
    "fix", "translate", "summarize", "summarise", "set a timer", "remind", "snap", "set brightness", "brightness", "dark mode",
    "light mode", "find file", "new folder", "turn on", "turn off",
];

/* ---------------- websites ULTRON can search for you ---------------- */

const SITES: [(&str, &str); 26] = [
    ("google images", "Google Images"), ("google maps", "Google Maps"), ("google news", "Google News"), ("youtube music", "YouTube Music"),
    ("stack overflow", "Stack Overflow"), ("stackoverflow", "Stack Overflow"), ("amazon", "Amazon"), ("flipkart", "Flipkart"),
    ("youtube", "YouTube"), ("google", "Google"), ("images", "Google Images"), ("maps", "Google Maps"), ("wikipedia", "Wikipedia"),
    ("github", "GitHub"), ("reddit", "Reddit"), ("spotify", "Spotify"), ("netflix", "Netflix"), ("myntra", "Myntra"), ("ebay", "eBay"),
    ("imdb", "IMDb"), ("linkedin", "LinkedIn"), ("twitter", "X"), ("pinterest", "Pinterest"), ("quora", "Quora"), ("news", "Google News"),
    ("meesho", "Meesho"),
];

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b' ' => "+".into(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn site_name(key: &str) -> Option<&'static str> {
    SITES.iter().find(|(k, _)| *k == key).map(|(_, n)| *n)
}

/// A number written in words at the start of `s` ("two thousand five hundred", "fifteen hundred", "a lakh"):
/// the number and how many bytes of `s` it took.
fn number_words(s: &str) -> Option<(f64, usize)> {
    const ONES: [&str; 20] = ["zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve", "thirteen", "fourteen", "fifteen", "sixteen", "seventeen", "eighteen", "nineteen"];
    const TENS: [&str; 8] = ["twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety"];
    let (mut total, mut cur, mut used, mut any) = (0.0f64, 0.0f64, 0usize, false);
    let mut at = 0;
    for raw in s.split_inclusive(' ') {
        let start = at;
        at += raw.len();
        let w = raw.trim_matches(|c: char| c == ' ' || c == ',');
        if w.is_empty() {
            continue;
        }
        let mut ok = true;
        for part in w.split('-') {
            if let Some(i) = ONES.iter().position(|o| *o == part) {
                cur += i as f64;
            } else if let Some(i) = TENS.iter().position(|t| *t == part) {
                cur += (i as f64 + 2.0) * 10.0;
            } else if part == "hundred" {
                cur = cur.max(1.0) * 100.0;
            } else if part == "thousand" {
                total += cur.max(1.0) * 1000.0;
                cur = 0.0;
            } else if part == "lakh" || part == "lakhs" || part == "lac" || part == "lacs" {
                total += cur.max(1.0) * 100_000.0;
                cur = 0.0;
            } else if part == "crore" || part == "crores" {
                total += cur.max(1.0) * 10_000_000.0;
                cur = 0.0;
            } else if (part == "and" && any) || (part == "a" && !any) {
                continue; // "two thousand and five hundred", "a thousand"
            } else {
                ok = false;
                break;
            }
            any = true;
        }
        if !ok {
            break;
        }
        used = start + raw.trim_end_matches(' ').len();
    }
    let n = total + cur;
    (any && n >= 1.0).then_some((n, used))
}

/// An amount of money at the start of `s`, however it was said or written: "2000", "₹2,000", "rs. 500",
/// "30 000", "50k", "1.5 lakh", "two thousand rupees". Gives the amount and how much of `s` it took.
/// A number that counts something else ("5 gb", "10 minutes") is not an amount.
fn amount(s: &str) -> Option<(u64, usize)> {
    let t = s.trim_start();
    let mut i = s.len() - t.len();
    for cur in ["₹", "$", "rs.", "rs ", "inr ", "rupees "] {
        if s[i..].starts_with(cur) {
            i += cur.len();
            break;
        }
    }
    i += s[i..].len() - s[i..].trim_start().len();
    let b = s.as_bytes();
    let mut j = i;
    let mut digits = String::new();
    while j < b.len() && (b[j].is_ascii_digit() || ((b[j] == b',' || b[j] == b'.') && j + 1 < b.len() && b[j + 1].is_ascii_digit() && !digits.is_empty())) {
        if b[j] != b',' {
            digits.push(b[j] as char);
        }
        j += 1;
    }
    let mut n: f64 = if digits.is_empty() {
        let (v, used) = number_words(&s[i..])?;
        j = i + used;
        v
    } else {
        // "30 000", "1 50 000": the thousands set apart by spaces (the recognizer writes them so). Only noughts are
        // taken this way: "under 20000 128 gb" is twenty thousand, and then something else.
        while !digits.contains('.') && digits.len() <= 4 && j + 4 <= b.len() && b[j] == b' ' && &s[j + 1..j + 4] == "000" && !(j + 4 < b.len() && b[j + 4].is_ascii_digit()) {
            digits.push_str("000");
            j += 4;
        }
        digits.parse().ok()?
    };
    let edge = |k: usize| k >= b.len() || !b[k].is_ascii_alphanumeric();
    for (suf, mul) in [("k", 1e3), (" k", 1e3), (" thousand", 1e3), (" lakhs", 1e5), (" lakh", 1e5), (" lacs", 1e5), (" lac", 1e5), (" crores", 1e7), (" crore", 1e7)] {
        if s[j..].starts_with(suf) && edge(j + suf.len()) {
            n *= mul;
            j += suf.len();
            break;
        }
    }
    for cur in [" rupees", " rupee", " rs.", " rs", " inr", " bucks", " dollars", "/-"] {
        if s[j..].starts_with(cur) && edge(j + cur.len()) {
            j += cur.len();
            break;
        }
    }
    let next = s[j..].trim_start().split(|c: char| !c.is_alphanumeric() && c != '%').next().unwrap_or("");
    const NOT_MONEY: [&str; 40] = [
        "mb", "gb", "kb", "tb", "minutes", "minute", "mins", "min", "seconds", "second", "secs", "hours", "hour", "hrs", "days", "day", "weeks", "week", "months", "month", "years", "year", "km", "kms", "miles",
        "kg", "kgs", "grams", "words", "pages", "lines", "percent", "%", "people", "items", "inches", "inch", "cm", "calories", "stars",
    ];
    (n >= 1.0 && !NOT_MONEY.contains(&next)).then_some((n.round() as u64, j))
}

/// "wireless headphones under ₹2,000 cheapest" -> ("wireless headphones", None, Some(2000), Some("price-asc")):
/// what to look for, the least and the most to pay, and how to sort.
fn shopping_range(q: &str) -> (String, Option<u64>, Option<u64>, Option<&'static str>) {
    let mut l = format!(" {} ", q.to_lowercase());
    let (mut min, mut max) = (None, None);
    // "between 1000 and 2000", "from 20k to 30k"
    for (w, joins) in [(" between ", [" and ", " to ", "-"]), (" from ", [" to ", " till ", "-"])] {
        let Some(i) = l.find(w) else { continue };
        let Some((lo, u1)) = amount(&l[i + w.len()..]) else { continue };
        let mid = i + w.len() + u1;
        let Some(j) = joins.iter().find(|j| l[mid..].starts_with(**j)) else { continue };
        let Some((hi, u2)) = amount(&l[mid + j.len()..]) else { continue };
        (min, max) = (Some(lo.min(hi)), Some(lo.max(hi)));
        l = format!("{} {}", &l[..i], &l[mid + j.len() + u2..]);
        break;
    }
    if max.is_none() {
        for w in [" under ", " below ", " less than ", " within ", " upto ", " up to ", " cheaper than ", " not more than ", " no more than ", " at most ", " maximum ", " max ", " budget of ", " budget "] {
            let Some(i) = l.find(w) else { continue };
            let Some((n, used)) = amount(&l[i + w.len()..]) else { continue };
            max = Some(n);
            l = format!("{} {}", &l[..i], &l[i + w.len() + used..]);
            break;
        }
    }
    if min.is_none() {
        for w in [" above ", " over ", " more than ", " at least ", " minimum ", " starting from ", " starting at "] {
            let Some(i) = l.find(w) else { continue };
            let Some((n, used)) = amount(&l[i + w.len()..]) else { continue };
            min = Some(n);
            l = format!("{} {}", &l[..i], &l[i + w.len() + used..]);
            break;
        }
    }
    let mut sort = None;
    for (words, key) in [
        (&["cheapest", "lowest price", "low to high", "cheap"][..], "price-asc"),
        (&["most expensive", "high to low", "premium"][..], "price-desc"),
        (&["best rated", "top rated", "highest rated", "best reviewed", "best"][..], "rating"),
        (&["newest", "latest", "new arrivals"][..], "newest"),
    ] {
        for w in words {
            let pat = format!(" {w} ");
            if l.contains(&pat) {
                sort.get_or_insert(key);
                l = l.replace(&pat, " ");
            }
        }
    }
    let q = l.split_whitespace().map(|w| w.trim_matches(|c: char| c == ',' || c == '.' || c == '?')).filter(|w| !w.is_empty() && !matches!(*w, "a" | "an" | "the" | "some" | "me" | "for" | "priced" | "costing" | "price")).collect::<Vec<_>>().join(" ");
    (q, min, max, sort)
}

fn shopping(q: &str) -> (String, Option<u64>, Option<&'static str>) {
    let (q, _, max, sort) = shopping_range(q);
    (q, max, sort)
}

/// The search page for a site, with price limits and sorting where the site supports them.
fn site_url(site: &str, q: &str) -> Option<(String, String)> {
    let name = site_name(site)?.to_string();
    let (query, min, max, sort) = shopping_range(q);
    if query.is_empty() {
        return None;
    }
    let e = enc(&query);
    let url = match site {
        "amazon" => {
            let mut u = format!("https://www.amazon.in/s?k={e}");
            if min.is_some() || max.is_some() {
                // (amazon.in counts in paise; an open end is left empty)
                let paise = |v: Option<u64>| v.map(|m| (m * 100).to_string()).unwrap_or_default();
                u.push_str(&format!("&rh=p_36%3A{}-{}", paise(min), paise(max)));
            }
            match sort {
                Some("price-asc") => u.push_str("&s=price-asc-rank"),
                Some("price-desc") => u.push_str("&s=price-desc-rank"),
                Some("rating") => u.push_str("&s=review-rank"),
                Some("newest") => u.push_str("&s=date-desc-rank"),
                _ => {}
            }
            u
        }
        "flipkart" => {
            let mut u = format!("https://www.flipkart.com/search?q={e}");
            if min.is_some() || max.is_some() {
                let (lo, hi) = (min.map(|m| m.to_string()).unwrap_or_else(|| "Min".into()), max.map(|m| m.to_string()).unwrap_or_else(|| "Max".into()));
                u.push_str(&format!("&p%5B%5D=facets.price_range.from%3D{lo}&p%5B%5D=facets.price_range.to%3D{hi}"));
            }
            match sort {
                Some("price-asc") => u.push_str("&sort=price_asc"),
                Some("price-desc") => u.push_str("&sort=price_desc"),
                Some("rating") => u.push_str("&sort=popularity"),
                Some("newest") => u.push_str("&sort=recency_desc"),
                _ => {}
            }
            u
        }
        "youtube" => format!("https://www.youtube.com/results?search_query={e}"),
        "youtube music" => format!("https://music.youtube.com/search?q={e}"),
        "google" => format!("https://www.google.com/search?q={e}"),
        "google images" | "images" => format!("https://www.google.com/search?tbm=isch&q={e}"),
        "google maps" | "maps" => format!("https://www.google.com/maps/search/{e}"),
        "google news" | "news" => format!("https://news.google.com/search?q={e}"),
        "wikipedia" => format!("https://en.wikipedia.org/w/index.php?search={e}"),
        "github" => format!("https://github.com/search?q={e}"),
        "reddit" => format!("https://www.reddit.com/search/?q={e}"),
        "spotify" => format!("spotify:search:{}", e.replace('+', "%20")),
        "netflix" => format!("https://www.netflix.com/search?q={e}"),
        "myntra" => format!("https://www.myntra.com/{}", query.replace(' ', "-")),
        "ebay" => format!("https://www.ebay.com/sch/i.html?_nkw={e}"),
        "imdb" => format!("https://www.imdb.com/find/?q={e}"),
        "stack overflow" | "stackoverflow" => format!("https://stackoverflow.com/search?q={e}"),
        "linkedin" => format!("https://www.linkedin.com/search/results/all/?keywords={e}"),
        "twitter" => format!("https://x.com/search?q={e}"),
        "pinterest" => format!("https://www.pinterest.com/search/pins/?q={e}"),
        "quora" => format!("https://www.quora.com/search?q={e}"),
        "meesho" => format!("https://www.meesho.com/search?q={e}"),
        _ => return None,
    };
    Some((name, url))
}

const FIND_VERBS: [&str; 19] = [
    "find me", "find", "search for", "search", "look for", "look up", "show me", "get me", "buy me", "buy", "shop for", "order", "browse for",
    "browse", "i want to buy", "i want", "i need", "get", "check",
];

/// "find wireless headphones under 2000 on amazon" / "search amazon for ..." -> (site, query)
fn site_search(l: &str) -> Option<(String, String)> {
    for (site, _) in SITES.iter() {
        for form in [format!("search {site} for "), format!("search on {site} for "), format!("{site} search for "), format!("{site} search ")] {
            if let Some(q) = l.strip_prefix(form.as_str()) {
                return Some((site.to_string(), q.trim().to_string()));
            }
        }
    }
    let rest = FIND_VERBS.iter().find_map(|v| l.strip_prefix(v).filter(|r| r.starts_with(' ')).map(str::trim))?;
    for (site, _) in SITES.iter() {
        for prep in [" on ", " in ", " from ", " at "] {
            let pat = format!("{prep}{site}");
            if let Some(i) = rest.find(&pat) {
                let end = i + pat.len();
                if rest[end..].chars().next().map(|c| c.is_alphanumeric()).unwrap_or(false) {
                    continue; // "on amazonia"
                }
                let q = format!("{} {}", &rest[..i], &rest[end..]);
                let q = q.trim().trim_start_matches("some ").trim_start_matches("a ").trim().to_string();
                if !q.is_empty() {
                    return Some((site.to_string(), q));
                }
            }
        }
    }
    // no shop was named, but a price was ("find me a keyboard under 1500"): that's shopping, and Amazon is where
    let (what, min, max, _) = shopping_range(rest);
    if (min.is_some() || max.is_some()) && !what.is_empty() {
        return Some(("amazon".to_string(), rest.to_string()));
    }
    None
}

/// "make me a presentation about black holes with 8 slides" -> ("black holes", 8)
fn presentation(l: &str) -> Option<(String, usize)> {
    let words = ["presentation", "ppt", "powerpoint", "power point", "slide deck", "slideshow", "slides", "deck"];
    let starts = ["make", "create", "build", "generate", "prepare", "design", "do", "give me", "write", "i need", "i want", "put together"];
    if !starts.iter().any(|v| l.starts_with(v)) || !words.iter().any(|w| l.split(|c: char| !c.is_alphanumeric() && c != ' ').any(|p| p.contains(w))) {
        return None;
    }
    let mut topic = None;
    for p in [" about ", " on ", " for ", " regarding ", " explaining ", " of "] {
        if let Some(i) = l.find(p) {
            topic = Some(l[i + p.len()..].to_string());
            break;
        }
    }
    let mut t = topic?;
    let ws: Vec<&str> = l.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let num = |w: &str| -> Option<usize> {
        Some(match w {
            "one" | "single" => 1, "two" | "couple" => 2, "three" => 3, "four" => 4, "five" => 5, "six" => 6, "seven" => 7, "eight" => 8,
            "nine" => 9, "ten" => 10, "eleven" => 11, "twelve" => 12, "fifteen" => 15, "twenty" => 20, _ => w.parse().ok()?,
        })
    };
    let n = ws
        .iter()
        .enumerate()
        .filter_map(|(i, w)| num(w).map(|n| (n, ws.get(i + 1).copied().unwrap_or(""))))
        .find(|(n, next)| (1..=20).contains(n) && (next.starts_with("slide") || next.starts_with("page") || *n >= 3))
        .map(|(n, _)| n)
        .unwrap_or(6);
    for tail in [" with ", " in powerpoint", " in ppt", " using powerpoint", " that has ", " having "] {
        if let Some(i) = t.find(tail) {
            t.truncate(i);
        }
    }
    let t = t.trim().trim_end_matches([' ', '.', '!']).trim_start_matches("the topic ").to_string();
    (!t.is_empty()).then_some((t, n))
}

fn starts_with_verb(s: &str) -> bool {
    let s = s.trim_start_matches("then ").trim_start_matches("also ").trim_start_matches("please ");
    VERBS.iter().any(|v| s.starts_with(v) && s[v.len()..].chars().next().map(|c| !c.is_alphanumeric()).unwrap_or(true))
}

/// "open notepad and write hello, then save it" -> ["open notepad", "write hello", "save it"]
pub fn split_steps(raw: &str) -> Vec<String> {
    const SEPS: [&str; 10] = [", and then ", " and then ", ", then ", " then ", "; ", ", and ", " and ", ", ", " after that ", ". "];
    let lower = raw.to_ascii_lowercase(); // same byte offsets as raw
    let mut parts = Vec::new();
    let (mut start, mut quoted) = (0usize, false);
    let mut idx = 0usize;
    while idx < lower.len() {
        if !lower.is_char_boundary(idx) {
            idx += 1;
            continue;
        }
        let c = lower[idx..].chars().next().unwrap();
        if matches!(c, '"' | '“' | '”') {
            quoted = !quoted;
        }
        if !quoted {
            if let Some(sep) = SEPS.iter().find(|s| lower[idx..].starts_with(*s) && starts_with_verb(lower[idx + s.len()..].trim_start())) {
                parts.push(raw[start..idx].trim().to_string());
                idx += sep.len();
                start = idx;
                continue;
            }
        }
        idx += c.len_utf8();
    }
    parts.push(raw[start..].trim().to_string());
    parts.into_iter().map(|p| p.trim_matches(|c: char| c == ',' || c == '.' || c.is_whitespace()).to_string()).filter(|p| !p.is_empty()).collect()
}

/// strip a case-insensitive prefix from the original text, keeping its capitals
fn after<'a>(raw: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    let l = raw.to_ascii_lowercase();
    prefixes.iter().find_map(|p| {
        (l.starts_with(p) && l[p.len()..].chars().next().map(|c| !c.is_alphanumeric()).unwrap_or(true)).then(|| raw[p.len()..].trim())
    })
}

fn unquote(s: &str) -> String {
    let s = s.trim().trim_start_matches([':', ',']).trim();
    let t = s.trim_matches(|c| matches!(c, '"' | '“' | '”' | '\''));
    if t.len() + 2 <= s.len() { t.to_string() } else { s.to_string() }
}

fn vk(name: &str) -> Option<VIRTUAL_KEY> {
    Some(match name {
        "ctrl" | "control" | "ctl" => VK_CONTROL,
        "shift" => VK_SHIFT,
        "alt" | "option" => VK_MENU,
        "win" | "windows" | "super" | "start" => VK_LWIN,
        "enter" | "return" => VK_RETURN,
        "tab" => VK_TAB,
        "esc" | "escape" => VK_ESCAPE,
        "space" | "spacebar" => VK_SPACE,
        "backspace" => VK_BACK,
        "delete" | "del" => VK_DELETE,
        "up" => VK_UP,
        "down" => VK_DOWN,
        "left" => VK_LEFT,
        "right" => VK_RIGHT,
        "home" => VK_HOME,
        "end" => VK_END,
        "pageup" => VK_PRIOR,
        "pagedown" => VK_NEXT,
        _ => {
            if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<u16>().ok()).filter(|n| (1..=12).contains(n)) {
                return Some(VIRTUAL_KEY(VK_F1.0 + n - 1));
            }
            let mut c = name.chars();
            match (c.next(), c.next()) {
                (Some(ch), None) if ch.is_ascii_alphanumeric() => VIRTUAL_KEY(ch.to_ascii_uppercase() as u16),
                _ => return None,
            }
        }
    })
}

fn parse_keys(s: &str) -> Option<(Vec<VIRTUAL_KEY>, String, u32)> {
    let l = s.to_lowercase().replace("page up", "pageup").replace("page down", "pagedown").replace(" key", "").replace(" button", "");
    let mut times = 1;
    let mut l = l.trim().to_string();
    for (w, n) in [(" twice", 2), (" two times", 2), (" three times", 3), (" thrice", 3), (" 2 times", 2), (" 3 times", 3), (" 4 times", 4), (" 5 times", 5)] {
        if let Some(r) = l.strip_suffix(w) {
            l = r.to_string();
            times = n;
        }
    }
    let l = l.trim_start_matches("the ").replace(" plus ", " ").replace(['+', '-'], " ");
    let keys: Option<Vec<VIRTUAL_KEY>> = l.split_whitespace().map(vk).collect();
    let keys = keys.filter(|k| !k.is_empty())?;
    let label = l.split_whitespace().map(|w| {
        let mut c = w.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    }).collect::<Vec<_>>().join("+");
    Some((keys, label, times))
}

fn looks_composed(text: &str) -> bool {
    let l = text.to_lowercase();
    let w: Vec<&str> = l.split_whitespace().collect();
    if w.len() < 2 {
        return false;
    }
    // "100 words on the USA", "3 paragraphs about AI", "five lines on rain"
    let counted = w.windows(2).any(|p| {
        (p[0].parse::<u32>().is_ok() || ["two", "three", "four", "five", "six", "ten", "a", "one"].contains(&p[0]))
            && ["words", "word", "lines", "sentences", "paragraphs", "paragraph", "points", "bullet", "page", "pages"].contains(&p[1])
    });
    let kinds = ["essay", "poem", "story", "paragraph", "letter", "email", "summary", "article", "report", "speech", "haiku", "song", "joke",
        "caption", "bio", "description", "review", "message", "note", "introduction", "conclusion", "outline", "list", "blog", "post", "tweet", "script"];
    counted
        || (["a ", "an ", "me a ", "me an ", "some ", "about ", "something ", "my ", "the lyrics", "lyrics "].iter().any(|p| l.starts_with(p)))
        || (w.iter().take(4).any(|x| kinds.contains(&x.trim_end_matches(['s', ',']))) && [" on ", " about ", " for ", " to "].iter().any(|p| l.contains(p)))
}

/// "cats in the search box" -> ("cats", "search box")
fn into_field(t: &str) -> Option<(String, String)> {
    let l = t.to_lowercase();
    for pre in [" into the ", " in the ", " into ", " in "] {
        if let Some(i) = l.rfind(pre) {
            let field = l[i + pre.len()..].trim();
            if [" box", " field", " bar", " input", " textbox", " text box"].iter().any(|s| field.ends_with(s)) || ["search", "address bar", "url bar"].contains(&field) {
                let text = unquote(&t[..i]);
                if !text.is_empty() {
                    return Some((text, field.to_string()));
                }
            }
        }
    }
    None
}

pub fn parse_step(piece: &str) -> Step {
    let raw = piece.trim().trim_end_matches(['.', '!']);
    let raw = after(raw, &["then", "and", "also", "please", "ultron"]).unwrap_or(raw);
    let l = commands::normalize(raw);
    let rl = raw.to_lowercase();
    if let Some((app, to, text)) = crate::skills::message(raw) {
        return Step::Message(app, to, text);
    }
    if let Some((to, subject, body)) = crate::skills::email(raw) {
        return Step::Email(to, subject, body);
    }
    if let Some(q) = crate::skills::screen(&l) {
        return Step::Screen(q);
    }
    if let Some((topic, n)) = presentation(&l) {
        return Step::Present(topic, n);
    }
    if let Some((how, paste)) = crate::skills::writing(&l) {
        return Step::Rewrite(how, paste);
    }
    // timers and reminders
    if l.starts_with("remind me") || l.contains("set a reminder") || l.starts_with("set a timer") || l.starts_with("timer for") || l.starts_with("start a timer")
        || l.starts_with("set an alarm") || l.starts_with("wake me")
    {
        // (the time is read from what was said as it was, with its colons: the cleaned-up wording drops them)
        let src = if rl.contains("remind") || rl.contains("timer") || rl.contains("alarm") || rl.contains("wake") { rl.trim() } else { l.as_str() };
        let (when, what) = match src.find(" to ") {
            Some(i) if l.starts_with("remind") => (&src[..i], src[i + 4..].to_string()),
            _ => (src, String::new()),
        };
        let secs = if when.contains(" at ") {
            crate::skills::until_clock(when).or_else(|| crate::skills::duration(when))
        } else {
            crate::skills::duration(when).or_else(|| crate::skills::until_clock(when))
        };
        if let Some(secs) = secs {
            let kind = if l.starts_with("remind") || l.contains("reminder") { "reminder" } else if l.contains("alarm") || l.starts_with("wake") { "alarm" } else { "timer" };
            // (an alarm can repeat: it's said as "alarm:daily" and so on, for Clock)
            let kind = if kind != "alarm" { kind.to_string() } else if l.contains("every day") || l.contains("everyday") || l.contains("daily") || l.contains("each day") {
                "alarm:daily".into()
            } else if l.contains("weekday") {
                "alarm:weekdays".into()
            } else if l.contains("weekend") {
                "alarm:weekends".into()
            } else {
                "alarm".into()
            };
            return Step::Timer(secs, what, kind);
        }
    }
    match l.as_str() {
        "snap window left" | "snap left" | "snap this left" | "snap it left" | "snap this window left" | "move this window left" | "move this to the left"
        | "put this on the left" | "move window to the left" | "snap this window to the left" => return Step::Snap(-1),
        "snap window right" | "snap right" | "snap this right" | "snap it right" | "snap this window right" | "move this window right" | "move this to the right"
        | "put this on the right" | "move window to the right" | "snap this window to the right" => return Step::Snap(1),
        "move this to the other screen" | "move this window to the other screen" | "move it to my other monitor" | "move this to the other monitor" => return Step::Snap(2),
        "close this window" | "close the window" | "close this" | "close it" => return Step::CloseWindow,
        "dark mode on" | "turn on dark mode" | "enable dark mode" | "switch to dark mode" | "dark mode" | "go dark" => return Step::DarkMode(Some(true)),
        "dark mode off" | "turn off dark mode" | "disable dark mode" | "light mode" | "switch to light mode" | "light mode on" | "turn on light mode" => return Step::DarkMode(Some(false)),
        "toggle dark mode" => return Step::DarkMode(None),
        "brightness up" | "increase brightness" | "increase the brightness" | "make the screen brighter" | "brighter" | "turn up the brightness" => return Step::Brightness(15, true),
        "brightness down" | "decrease brightness" | "decrease the brightness" | "dim the screen" | "make the screen dimmer" | "dimmer" | "turn down the brightness" | "lower the brightness" => return Step::Brightness(-15, true),
        "open latest download" | "open my latest download" | "open the latest download" | "open my last download" | "open the last download" | "open the file i just downloaded"
        | "open my newest download" | "open the newest download" | "open last downloaded file" => return Step::LatestDownload,
        _ => {}
    }
    if l.contains("brightness") {
        if let Some(n) = l.split(|c: char| !c.is_ascii_digit()).find_map(|w| w.parse::<i32>().ok()) {
            return Step::Brightness(n.clamp(0, 100), false);
        }
    }
    if let Some((name, uri)) = crate::skills::settings_page(&l) {
        return Step::SettingsPage(name.into(), uri.into());
    }
    if let Some(f) = after(&rl, &["find file", "find the file", "find my file", "search my pc for", "search my computer for", "where is my file", "where's my file", "locate"]) {
        let f = f.trim_start_matches("called ").trim_start_matches("named ").trim();
        if !f.is_empty() {
            return Step::FindFile(f.to_string());
        }
    }
    if let Some(rest) = after(&rl, &["new folder", "create a folder", "create a new folder", "make a folder", "make a new folder", "create folder"]) {
        let rest = rest.trim_start_matches("called ").trim_start_matches("named ");
        let (name, place) = [" on ", " in "].iter().find_map(|p| rest.rfind(p).map(|i| (&rest[..i], rest[i + p.len()..].trim_start_matches("the ").trim_start_matches("my ").trim()))).unwrap_or((rest, "desktop"));
        let name = name.trim().trim_matches(|c| matches!(c, '"' | '\'' | '“' | '”'));
        if !name.is_empty() {
            // keep the capitals you used
            let i = raw.to_lowercase().find(name).unwrap_or(0);
            let real = raw.get(i..i + name.len()).unwrap_or(name);
            return Step::NewFolder(real.to_string(), place.to_string());
        }
    }
    if let Some((topic, n)) = presentation(&l) {
        return Step::Present(topic, n);
    }
    if let Some((site, q)) = site_search(&l) {
        return Step::SiteSearch(site, q);
    }
    // writing
    if let Some(rest) = after(raw, &["compose", "draft"]) {
        return Step::Compose(unquote(rest));
    }
    if let Some(rest) = after(raw, &["type out", "type in", "type", "write down", "write out", "write", "dictate", "enter the text", "enter text"]) {
        let mut t = unquote(rest);
        for tail in [" in it", " into it", " in there", " there", " here"] {
            if t.to_lowercase().ends_with(tail) {
                t.truncate(t.len() - tail.len());
            }
        }
        if let Some((text, field)) = into_field(&t) {
            return Step::TypeIn(field, text);
        }
        let quoted = rest.trim_start().starts_with(['"', '“', '\'']);
        if !quoted {
            t = crate::dictate::marks(&t); // ("hello comma how are you question mark")
        }
        if !quoted && looks_composed(&t) {
            let t = t.strip_prefix("me ").or_else(|| t.strip_prefix("Me ")).unwrap_or(&t).to_string();
            return Step::Compose(t);
        }
        return Step::Write(t);
    }
    // keyboard
    if let Some(rest) = after(&l, &["press", "hit", "tap the", "push"]) {
        if let Some((k, lab, n)) = parse_keys(rest) {
            return Step::Keys(k, lab, n);
        }
        return Step::Click(rest.trim_start_matches("the ").trim_end_matches(" button").to_string());
    }
    let combo = |keys: &[VIRTUAL_KEY], label: &str| Step::Keys(keys.to_vec(), label.into(), 1);
    match l.as_str() {
        "enter" => return combo(&[VK_RETURN], "Enter"),
        "select all" | "select everything" | "select all the text" => return combo(&[VK_CONTROL, VIRTUAL_KEY(b'A' as u16)], "Ctrl+A"),
        "copy" | "copy it" | "copy that" | "copy this" | "copy everything" => return combo(&[VK_CONTROL, VIRTUAL_KEY(b'C' as u16)], "Ctrl+C"),
        "paste" | "paste it" | "paste that" => return combo(&[VK_CONTROL, VIRTUAL_KEY(b'V' as u16)], "Ctrl+V"),
        "undo" | "undo that" => return combo(&[VK_CONTROL, VIRTUAL_KEY(b'Z' as u16)], "Ctrl+Z"),
        "redo" => return combo(&[VK_CONTROL, VIRTUAL_KEY(b'Y' as u16)], "Ctrl+Y"),
        "new tab" | "open a new tab" | "open new tab" => return combo(&[VK_CONTROL, VIRTUAL_KEY(b'T' as u16)], "Ctrl+T"),
        "close tab" | "close the tab" | "close this tab" => return combo(&[VK_CONTROL, VIRTUAL_KEY(b'W' as u16)], "Ctrl+W"),
        "new window" | "open a new window" | "new file" | "new document" | "new note" => return combo(&[VK_CONTROL, VIRTUAL_KEY(b'N' as u16)], "Ctrl+N"),
        "refresh" | "reload" | "refresh the page" | "reload the page" => return combo(&[VK_F5], "F5"),
        "go back" | "go back a page" => return combo(&[VK_MENU, VK_LEFT], "Alt+Left"),
        "scroll down" => return combo(&[VK_NEXT], "Page Down"),
        "scroll up" => return combo(&[VK_PRIOR], "Page Up"),
        "delete everything" | "clear it" | "clear everything" | "delete it all" | "erase everything" => {
            return Step::Keys(vec![VK_CONTROL, VIRTUAL_KEY(b'A' as u16)], "Ctrl+A, Delete".into(), 1)
        }
        "minimize" | "minimise" | "minimize it" | "minimise it" | "minimize this" | "minimize the window" => return Step::Minimize,
        "maximize" | "maximise" | "maximize it" | "maximise it" | "maximize this" | "maximize the window" | "make it full screen" => return Step::Maximize,
        _ => {}
    }
    if l == "save" || l.starts_with("save it") || l.starts_with("save the file") || l.starts_with("save this") || l.starts_with("save as") || l.starts_with("save the document") {
        let name = after(raw, &["save it as", "save the file as", "save this as", "save as", "save the document as"]).map(unquote).filter(|n| !n.is_empty());
        return Step::SaveAs(name);
    }
    {
        let clean = |s: &str| s.trim().trim_start_matches("the ").trim_end_matches(" switch").trim_end_matches(" toggle").trim_end_matches(" option").trim_end_matches(" checkbox").trim_end_matches(" box").trim().to_string();
        let skip = |s: &str| ["volume", "brightness", "the volume", "the brightness", "a timer", "an alarm", "a reminder", "dark mode", "light mode", "wifi", "wi-fi", "bluetooth", "night light"].iter().any(|x| s.starts_with(x));
        for (words, act) in [(&["turn on", "switch on", "enable", "tick"][..], "on"), (&["turn off", "switch off", "disable", "untick", "uncheck"][..], "off"), (&["expand"][..], "expand"), (&["collapse"][..], "collapse")] {
            if let Some(rest) = after(&l, words) {
                if !skip(rest) && !rest.is_empty() {
                    return Step::Ui(act.into(), clean(rest), String::new());
                }
            }
        }
        for (end, act) in [(" on", "on"), (" off", "off")] {
            if let Some(mid) = after(&l, &["turn", "switch"]).and_then(|r| r.strip_suffix(end)) {
                if !skip(mid) && !mid.is_empty() {
                    return Step::Ui(act.into(), clean(mid), String::new());
                }
            }
        }
        if let Some(rest) = after(&l, &["set"]) {
            if let Some((name, val)) = rest.split_once(" to ") {
                if !skip(name) && !name.is_empty() {
                    let raw_val = raw.rsplit_once(" to ").map(|(_, v)| v).unwrap_or(val);
                    return Step::Ui("set".into(), clean(name), unquote(raw_val.trim()));
                }
            }
        }
        if let Some(rest) = after(&l, &["select", "choose", "pick"]) {
            if let Some((val, name)) = rest.split_once(" in ").or_else(|| rest.split_once(" from ")) {
                let raw_val = after(raw, &["select", "choose", "pick", "Select", "Choose", "Pick"]).and_then(|r| r.split_once(" in ").or_else(|| r.split_once(" from "))).map(|(v, _)| v).unwrap_or(val);
                return Step::Ui("select".into(), clean(name), unquote(raw_val.trim().trim_start_matches("the ")));
            }
        }
    }
    if let Some(rest) = after(&l, &["click on", "click", "tap on", "double click"]) {
        return Step::Click(rest.trim_start_matches("the ").trim_end_matches(" button").to_string());
    }
    if let Some(rest) = after(&l, &["wait for", "wait"]) {
        let n = rest.split_whitespace().find_map(|w| w.parse::<f32>().ok()).unwrap_or(if rest.contains("minute") { 60.0 } else { 1.5 });
        return Step::Wait(if rest.contains("minute") && n < 10.0 { n * 60.0 } else { n.min(30.0) });
    }
    if let Some(rest) = after(&l, &["switch to", "go to the", "bring up", "focus"]) {
        if !rest.contains('.') {
            return Step::SwitchTo(rest.trim_start_matches("the ").trim_end_matches(" window").to_string());
        }
    }
    if let Some(rest) = after(&l, &["open up", "open", "launch", "start", "run", "go to", "take me to"]) {
        return Step::Open(rest.to_string());
    }
    Step::Command(raw.to_string())
}

fn agent_only(s: &Step) -> bool {
    !matches!(s, Step::Command(_) | Step::Open(_))
}

/// A plan, when the request needs more than one fixed command (or keyboard / mouse work).
/// "write an essay on the USA in notepad" -> open Notepad first, then write there.
fn with_app(pieces: Vec<String>) -> Vec<String> {
    const EDITORS: [&str; 9] = ["notepad", "microsoft word", "word", "wordpad", "onenote", "sticky notes", "vs code", "visual studio code", "notepad++"];
    let mut out = Vec::new();
    for p in pieces {
        let l = p.to_lowercase();
        let writing = ["write", "type", "compose", "draft"].iter().any(|v| l.starts_with(v));
        let tail = writing
            .then(|| EDITORS.iter().find_map(|e| [" in ", " into ", " on ", " using "].iter().find_map(|pre| l.strip_suffix(&format!("{pre}{e}")).map(|h| (h.len(), *e)))))
            .flatten();
        match tail {
            Some((cut, app)) if !out.last().map(|x: &String| x.to_lowercase().contains(app)).unwrap_or(false) => {
                out.push(format!("open {app}"));
                out.push(p[..cut].to_string());
            }
            Some((cut, _)) => out.push(p[..cut].to_string()),
            None => out.push(p),
        }
    }
    out
}

pub fn plan(raw: &str) -> Option<Vec<Step>> {
    // "look at this window and tell me how many buttons it has" is one question about the screen, not two steps
    let whole = raw.trim().trim_end_matches(['?', '.', '!']).to_lowercase();
    let parts = split_steps(raw);
    let telling = |p: &String| {
        let p = p.to_lowercase();
        ["tell", "say", "count", "explain", "describe", "read", "let me know", "summarize", "summarise", "list", "show me", "find"].iter().any(|v| p.starts_with(v))
    };
    if parts.len() >= 2 && parts[1..].iter().all(telling) {
        if let Some(q) = crate::skills::screen(&whole) {
            return Some(vec![Step::Screen(q)]);
        }
    }
    let pieces = with_app(parts);
    let mut steps: Vec<Step> = pieces.iter().map(|p| parse_step(p)).collect();
    if steps.iter().any(|s| matches!(s, Step::Present(..))) {
        steps.retain(|s| !matches!(s, Step::Open(a) if a.contains("powerpoint") || a.contains("power point") || a == "ppt"));
    }
    if let Some(Step::Message(a, ..)) = steps.iter().find(|s| matches!(s, Step::Message(..))).cloned() {
        steps.retain(|s| !matches!(s, Step::Open(o) if o.to_lowercase().contains(a.as_str())));
    }
    if steps.is_empty() || !(steps.len() >= 2 || steps.iter().any(agent_only)) {
        return None;
    }
    // something in there isn't an action ("tell me a joke and open spotify"): let the AI handle it
    if steps.iter().any(|s| matches!(s, Step::Command(c) if !starts_with_verb(&commands::normalize(c)))) {
        return None;
    }
    Some(steps)
}

/// The AI's ">> " lines become a plan too, when any of them needs keyboard or mouse work.
pub fn wants_agent(steps: &[Step]) -> bool {
    steps.len() >= 2 || steps.iter().any(agent_only)
}

/* ---------------- talking about it ---------------- */

fn short(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if s.chars().count() > n { format!("{t}…") } else { t }
}

fn title(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn doing(s: &Step) -> String {
    match s {
        Step::Open(x) => format!("Opening {}", title(x)),
        Step::Write(t) => format!("Typing “{}”", short(t, 26)),
        Step::Compose(t) => format!("Writing {}", short(t, 34)),
        Step::Keys(_, l, _) => format!("Pressing {l}"),
        Step::SaveAs(Some(n)) => format!("Saving as {n}"),
        Step::SaveAs(None) => "Saving".into(),
        Step::Click(x) => format!("Clicking “{}”", short(x, 24)),
        Step::TypeIn(f, t) => format!("Typing “{}” in {}", short(t, 20), short(f, 18)),
        Step::Wait(n) => format!("Waiting {n:.0}s"),
        Step::Minimize => "Minimizing the window".into(),
        Step::Ui(act, x, v) => match act.as_str() {
            "set" => format!("Setting “{}” to {}", short(x, 20), short(v, 20)),
            "select" => format!("Choosing {} in “{}”", short(v, 20), short(x, 20)),
            _ => format!("Turning “{}” {act}", short(x, 24)),
        },
        Step::Maximize => "Maximizing the window".into(),
        Step::SwitchTo(x) => format!("Switching to {}", title(x)),
        Step::Message(app, to, _) => format!("Messaging {} on {}", title(to), title(app)),
        Step::Email(to, _, _) => format!("Drafting an email to {}", short(to, 24)),
        Step::Rewrite(_, true) => "Rewriting your text".into(),
        Step::Rewrite(_, false) => "Reading your text".into(),
        Step::Screen(_) => "Reading your screen".into(),
        Step::Timer(s, _, k) => format!("Setting a {} {}", crate::skills::human(*s), k.split(':').next().unwrap_or(k)),
        Step::Snap(d) => (if *d == 2 { "Moving the window" } else { "Snapping the window" }).into(),
        Step::CloseWindow => "Closing the window".into(),
        Step::Brightness(..) => "Changing the brightness".into(),
        Step::DarkMode(_) => "Switching the theme".into(),
        Step::SettingsPage(n, _) => format!("Opening {n} settings"),
        Step::FindFile(f) => format!("Finding “{}”", short(f, 24)),
        Step::LatestDownload => "Opening your latest download".into(),
        Step::NewFolder(n, _) => format!("Creating “{}”", short(n, 24)),
        Step::SiteSearch(site, q) => format!("Searching {} for {}", site_name(site).unwrap_or(site), short(q, 26)),
        Step::Present(t, n) => format!("Making a {n}-slide deck on {}", short(t, 24)),
        Step::Command(c) => title(c),
    }
}

fn did(s: &Step, cmd_action: &str) -> String {
    match s {
        Step::Open(x) => format!("opened {}", title(x)),
        Step::Write(t) => format!("typed “{}”", short(t, 40)),
        Step::Compose(t) => format!("wrote {}", short(t, 40)),
        Step::Keys(_, l, _) => format!("pressed {l}"),
        Step::SaveAs(Some(n)) => format!("saved it as {n}"),
        Step::SaveAs(None) => "saved it".into(),
        Step::Click(x) => format!("clicked {x}"),
        Step::TypeIn(f, t) => format!("typed “{}” in the {f}", short(t, 40)),
        Step::Wait(_) => "waited".into(),
        Step::Minimize => "minimized it".into(),
        Step::Ui(act, x, _) => format!("{act} {x}"),
        Step::Maximize => "maximized it".into(),
        Step::SwitchTo(x) => format!("switched to {}", title(x)),
        Step::Message(app, to, _) => format!("wrote your message to {} in {}", title(to), title(app)),
        Step::Email(to, _, _) => format!("drafted the email to {to}"),
        Step::Rewrite(_, true) => "rewrote your text".into(),
        Step::Rewrite(_, false) => "read your text".into(),
        Step::Screen(_) => "read your screen".into(),
        Step::Timer(s, _, k) => format!("set a {} {}", crate::skills::human(*s), k.split(':').next().unwrap_or(k)),
        Step::Snap(_) => "moved the window".into(),
        Step::CloseWindow => "closed the window".into(),
        Step::Brightness(..) => if cmd_action.is_empty() { "changed the brightness".into() } else { cmd_action.to_string() },
        Step::DarkMode(_) => if cmd_action.is_empty() { "switched the theme".into() } else { cmd_action.to_string() },
        Step::SettingsPage(n, _) => format!("opened {n} settings"),
        Step::FindFile(f) => format!("searched your PC for “{f}”"),
        Step::LatestDownload => if cmd_action.is_empty() { "opened your latest download".into() } else { cmd_action.to_string() },
        Step::NewFolder(n, w) => format!("created “{n}” in {w}"),
        Step::SiteSearch(site, q) => format!("searched {} for {}", site_name(site).unwrap_or(site), short(q, 40)),
        Step::Present(t, _) => format!("made you a presentation on {t} (it's in Documents\\ULTRON Presentations)"),
        Step::Command(c) => if cmd_action.is_empty() { c.to_lowercase() } else { cmd_action.to_lowercase() },
    }
}

fn join(list: &[String]) -> String {
    match list.len() {
        0 => String::new(),
        1 => list[0].clone(),
        n => format!("{} and {}", list[..n - 1].join(", "), list[n - 1]),
    }
}

/* ---------------- Windows helpers ---------------- */

fn fg_root() -> HWND {
    unsafe { GetAncestor(GetForegroundWindow(), GA_ROOTOWNER) }
}

fn pid_of(h: HWND) -> u32 {
    let mut p = 0u32;
    unsafe { GetWindowThreadProcessId(h, Some(&mut p)) };
    p
}

fn usable(h: HWND, own: u32) -> bool {
    !h.is_invalid() && pid_of(h) != own && unsafe { IsWindowVisible(h).as_bool() && GetWindowTextLengthW(h) > 0 }
}

fn screen_center() -> (f64, f64) {
    unsafe { (GetSystemMetrics(SM_CXSCREEN) as f64 / 2.0, GetSystemMetrics(SM_CYSCREEN) as f64 / 2.0) }
}

/// Where the text cursor is in a window (or a sensible spot inside it).
fn text_spot(h: HWND) -> (f64, f64) {
    unsafe {
        let tid = GetWindowThreadProcessId(h, None);
        let mut gi = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        if GetGUIThreadInfo(tid, &mut gi).is_ok() && !gi.hwndCaret.is_invalid() && gi.rcCaret.bottom > gi.rcCaret.top {
            let mut p = POINT { x: gi.rcCaret.right, y: gi.rcCaret.bottom };
            if ClientToScreen(gi.hwndCaret, &mut p).as_bool() {
                return (p.x as f64 + 3.0, p.y as f64 + 2.0);
            }
        }
        let mut r = RECT::default();
        if GetWindowRect(h, &mut r).is_ok() && r.right > r.left {
            return (r.left as f64 + (r.right - r.left) as f64 * 0.32, r.top as f64 + (r.bottom - r.top) as f64 * 0.32);
        }
    }
    screen_center()
}

fn wait_ready(h: HWND) {
    unsafe {
        if let Ok(p) = OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, false, pid_of(h)) {
            WaitForInputIdle(p, 2500);
            let _ = windows::Win32::Foundation::CloseHandle(p);
        }
    }
    std::thread::sleep(Duration::from_millis(120));
}

fn key_input(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() }, ..Default::default() } },
    }
}

fn press(keys: &[VIRTUAL_KEY]) {
    let mut v: Vec<INPUT> = keys.iter().map(|k| key_input(*k, false)).collect();
    v.extend(keys.iter().rev().map(|k| key_input(*k, true)));
    unsafe { SendInput(&v, std::mem::size_of::<INPUT>() as i32) };
}

fn type_char(c: char) {
    match c {
        '\r' => {}
        '\n' => press(&[VK_RETURN]),
        '\t' => press(&[VK_TAB]),
        _ => {
            let mut buf = [0u16; 2];
            let mut v = Vec::new();
            for u in c.encode_utf16(&mut buf).iter() {
                for up in [false, true] {
                    v.push(INPUT {
                        r#type: INPUT_KEYBOARD,
                        Anonymous: INPUT_0 {
                            ki: KEYBDINPUT { wScan: *u, dwFlags: if up { KEYEVENTF_UNICODE | KEYEVENTF_KEYUP } else { KEYEVENTF_UNICODE }, ..Default::default() },
                        },
                    });
                }
            }
            unsafe { SendInput(&v, std::mem::size_of::<INPUT>() as i32) };
        }
    }
}

fn real_click(x: i32, y: i32) {
    unsafe {
        let mut old = POINT::default();
        let _ = GetCursorPos(&mut old);
        let _ = SetCursorPos(x, y);
        std::thread::sleep(Duration::from_millis(40));
        mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
        mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
        std::thread::sleep(Duration::from_millis(40));
        let _ = SetCursorPos(old.x, old.y);
    }
}

/// Does something on screen carry this person's name? "Danish", or a chat row like "Danish Tillu  hello  10:30",
/// for "danish" (the name leads; a title in front of it, "Dr Danish", is fine too).
fn named(label: &str, who: &str) -> bool {
    let (l, w) = (crate::vision::norm(label), crate::vision::norm(who));
    if w.is_empty() || l.is_empty() {
        return false;
    }
    let (lw, n): (Vec<&str>, usize) = (l.split(' ').collect(), w.split(' ').count());
    let head = lw.iter().take(n).cloned().collect::<Vec<_>>().join(" ");
    let after_title = lw.iter().skip(1).take(n).cloned().collect::<Vec<_>>().join(" ");
    head == w || crate::vision::similar(&head, &w) >= 0.86 || (lw.len() > n && lw[0].len() <= 4 && after_title == w)
}

/* ---------------- doing it ---------------- */

struct Ctx {
    say: Option<String>,  // an answer to show (summaries, screen questions)
    own: u32,
    target: Option<HWND>, // the window we're working in
    last_open: String,    // lowercased name of what we opened last
}

fn emit(app: &AppHandle, i: usize, n: usize, label: &str, state: &str) {
    let _ = app.emit_to("main", "agent-step", json!({ "i": i, "n": n, "label": label, "state": state }));
}

/// What Ctrl+C just put on the clipboard: taken the moment it lands (usually a few ms), not after a fixed wait.
fn copied() -> String {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(450) {
        std::thread::sleep(Duration::from_millis(12));
        if let Some(t) = crate::clip::read_text().filter(|t| !t.trim().is_empty()) {
            return t;
        }
    }
    String::new()
}

/// The text selected in the app you were last using (copied, then your clipboard is put back).
pub fn selection(app: &AppHandle) -> Option<String> {
    let mut cx = Ctx { say: None, own: unsafe { GetCurrentProcessId() }, target: None, last_open: String::new() };
    ensure_target(app, &mut cx)?;
    let before = crate::clip::read_text().unwrap_or_default();
    let _ = crate::clip::set("");
    press(&[VK_CONTROL, VIRTUAL_KEY(b'C' as u16)]);
    let text = copied();
    let _ = crate::clip::set(&before);
    let t = text.trim().to_string();
    (!t.is_empty()).then_some(t)
}

/// Make sure the window we're working in is in front (never type into ULTRON itself).
fn ensure_target(app: &AppHandle, cx: &mut Ctx) -> Option<HWND> {
    let fg = fg_root();
    if let Some(t) = cx.target.filter(|t| unsafe { IsWindowVisible(*t).as_bool() }) {
        if fg != t {
            crate::apps::focus_window(t);
            std::thread::sleep(Duration::from_millis(250));
        }
        return (fg_root() == t).then_some(t).or_else(|| usable(fg_root(), cx.own).then(fg_root));
    }
    if usable(fg, cx.own) {
        cx.target = Some(fg);
        return Some(fg);
    }
    app.state::<crate::apps::Apps>().refocus();
    // (given the keyboard back, an app puts its cursor back in its box a moment later: typed sooner, the first letter
    //  went nowhere)
    std::thread::sleep(Duration::from_millis(450));
    let fg = fg_root();
    usable(fg, cx.own).then(|| {
        cx.target = Some(fg);
        fg
    })
}

/// Type like a person would (fast), with the cursor following the text. Stops if you take over.
fn type_into(app: &AppHandle, h: HWND, text: &str) -> bool {
    let n = text.chars().count();
    let gap = match crate::ai::config(app).type_speed.as_str() {
        "instant" => 0,
        "fast" => if n > 200 { 1 } else { 3 },
        _ => if n > 200 { 2 } else { 5 },
    };
    for (i, c) in text.chars().enumerate() {
        if i % 12 == 0 {
            if fg_root() != h || crate::ai::cancelled() {
                return false; // you switched windows: stop instead of typing into the wrong place
            }
            let (x, y) = text_spot(h);
            ghost::go(x, y);
            ghost::tap(app);
        }
        type_char(c);
        if gap > 0 {
            std::thread::sleep(Duration::from_millis(gap));
        }
    }
    true
}

/// Put a piece of text in by pasting it. Stops if you switched windows or pressed Stop.
/// Type at full speed (names in a search box, a message that waits for your OK anyway).
/// A page has just been opened in the browser: find the box it wants its request typed into (the big one, or
/// the one that says what it's for; never the browser's own address bar), type the request and send it.
pub fn prompt_page(text: &str) -> Result<(), String> {
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_millis(2600)); // (the browser comes to the front and starts loading)
    let mut spot = None;
    while spot.is_none() && t0.elapsed() < Duration::from_secs(24) {
        if crate::ai::cancelled() {
            return Err("Stopped.".into());
        }
        let h = fg_root();
        let mut win = RECT::default();
        let _ = unsafe { GetWindowRect(h, &mut win) };
        let (ww, wh) = ((win.right - win.left).max(1), (win.bottom - win.top).max(1));
        let boxes: Vec<(String, RECT)> = crate::vision::edits(h).into_iter().filter(|(_, r)| r.top > win.top + wh * 14 / 100 && r.right - r.left > ww / 6).collect();
        let says = |n: &str| {
            let n = n.to_lowercase();
            ["prompt", "describe", "build", "idea", "ask", "message", "what", "create", "app", "type", "make"].iter().any(|w| n.contains(w))
        };
        let size = |r: &RECT| (r.right - r.left) * (r.bottom - r.top).min(220);
        spot = boxes.iter().filter(|(n, _)| says(n)).max_by_key(|(_, r)| size(r)).or_else(|| boxes.iter().max_by_key(|(_, r)| size(r))).map(|(_, r)| (*r, h));
        if spot.is_none() {
            std::thread::sleep(Duration::from_millis(700));
        }
    }
    let (r, h) = spot.ok_or("I couldn't find where to type the request on that page (it may be waiting for you to sign in).")?;
    crate::log_line(&format!("builder: the request box was found after {} ms", t0.elapsed().as_millis()));
    real_click((r.left + r.right) / 2, (r.top + r.bottom) / 2);
    std::thread::sleep(Duration::from_millis(260));
    if !type_fast(h, text) {
        return Err("You switched windows, so I stopped before sending it.".into());
    }
    std::thread::sleep(Duration::from_millis(380));
    press(&[VK_RETURN]);
    Ok(())
}

fn type_fast(h: HWND, text: &str) -> bool {
    for (i, c) in text.chars().enumerate() {
        if i % 16 == 0 && (fg_root() != h || crate::ai::cancelled()) {
            return false; // you switched windows: stop instead of typing into the wrong place
        }
        type_char(c);
        if i % 4 == 3 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    true
}

fn paste_into(app: &AppHandle, h: HWND, text: &str) -> bool {
    if fg_root() != h || crate::ai::cancelled() {
        return false;
    }
    if !crate::clip::set(text) {
        return type_into(app, h, text);
    }
    std::thread::sleep(Duration::from_millis(40));
    press(&[VK_CONTROL, VIRTUAL_KEY(b'V' as u16)]);
    std::thread::sleep(Duration::from_millis(90));
    let (x, y) = text_spot(h);
    ghost::go(x, y);
    ghost::tap(app);
    true
}

fn wait_window(app: &AppHandle, before: HWND, own: u32, name: &str, limit: Duration) -> Option<HWND> {
    let t0 = Instant::now();
    let mut tried = Instant::now();
    let apps = app.state::<crate::apps::Apps>();
    while t0.elapsed() < limit {
        let fg = fg_root();
        if fg != before && usable(fg, own) {
            return Some(fg);
        }
        // Windows may open it behind: go and find it
        if t0.elapsed() > Duration::from_millis(1500) && tried.elapsed() > Duration::from_millis(500) && !name.is_empty() {
            tried = Instant::now();
            if let Some(h) = apps.find_window(name) {
                crate::apps::focus_window(h);
                std::thread::sleep(Duration::from_millis(200));
                if fg_root() == h {
                    return Some(h);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

const BROWSERS: [&str; 8] = ["chrome", "edge", "firefox", "brave", "opera", "zen", "vivaldi", "browser"];

fn exec(app: &AppHandle, cx: &mut Ctx, step: &Step) -> Result<String, String> {
    match step {
        Step::Open(what) => {
            let before = fg_root();
            let r = commands::run(app, &format!("open {what}"));
            if !r.ok {
                return Err(r.reply);
            }
            ghost::mood(app, "think");
            let c = screen_center();
            ghost::go(c.0, c.1 - 80.0);
            let name = r.action.strip_prefix("Opened ").unwrap_or(what).to_string();
            cx.last_open = name.to_lowercase();
            let site = r.icon == "globe";
            let limit = Duration::from_millis(if site { 2500 } else { 9000 });
            match wait_window(app, before, cx.own, if site { "" } else { &name }, limit) {
                Some(h) => {
                    cx.target = Some(h);
                    wait_ready(h);
                    // Notepad reopens your old notes: write in a fresh tab, never into those
                    if cx.last_open.contains("notepad") && fg_root() == h {
                        press(&[VK_CONTROL, VIRTUAL_KEY(b'N' as u16)]);
                        std::thread::sleep(Duration::from_millis(320));
                    }
                    let (x, y) = text_spot(h);
                    ghost::go_wait(x, y);
                }
                None if site => {}
                None => return Err(format!("{name} didn't open a window I could find.")),
            }
            ghost::mood(app, "idle");
            Ok(r.action)
        }
        Step::Write(text) => {
            let h = ensure_target(app, cx).ok_or("There's no window to type into.")?;
            let (x, y) = text_spot(h);
            ghost::go_wait(x, y);
            ghost::mood(app, "type");
            let ok = type_into(app, h, text);
            ghost::mood(app, "idle");
            if ok { Ok(String::new()) } else if crate::ai::cancelled() { Err("Stopped.".into()) } else { Err("You switched windows, so I stopped typing.".into()) }
        }
        Step::Compose(what) => {
            let h = ensure_target(app, cx).ok_or("There's no window to write in.")?;
            let (x, y) = text_spot(h);
            ghost::go_wait(x, y);
            ghost::mood(app, "think");
            let mut ok = true;
            let mut started = false;
            // written text goes in a sentence at a time by pasting (reliable in every app, no stray keystrokes)
            let before = crate::clip::read_text();
            let mut buf = String::new();
            let mut wrote = crate::ai::compose(app, what, |piece| {
                if !ok {
                    return;
                }
                if !started {
                    started = true;
                    ghost::mood(app, "type");
                }
                buf.push_str(piece);
                if buf.chars().count() > 60 || buf.trim_end_matches(' ').ends_with(['.', '!', '?', '\n']) {
                    ok = paste_into(app, h, &std::mem::take(&mut buf));
                }
            });
            if ok && !buf.is_empty() {
                ok = paste_into(app, h, &buf);
            }
            if let Some(b) = before {
                let _ = crate::clip::set(&b); // your clipboard is left as it was
            }
            wrote = wrote && true;
            ghost::mood(app, "idle");
            if !wrote {
                return Err("My local brain isn't available to write that right now.".into());
            }
            if ok { Ok(String::new()) } else if crate::ai::cancelled() { Err("Stopped.".into()) } else { Err("You switched windows, so I stopped writing.".into()) }
        }
        Step::Keys(keys, label, times) => {
            let h = ensure_target(app, cx).ok_or("There's no window to press keys in.")?;
            let (x, y) = text_spot(h);
            ghost::go_wait(x, y);
            for _ in 0..*times {
                ghost::click(app);
                press(keys);
                std::thread::sleep(Duration::from_millis(90));
            }
            if label.ends_with("Delete") {
                press(&[VK_DELETE]);
            }
            std::thread::sleep(Duration::from_millis(120));
            Ok(String::new())
        }
        Step::SaveAs(name) => {
            let h = ensure_target(app, cx).ok_or("There's nothing to save.")?;
            ghost::click(app);
            press(&[VK_CONTROL, VIRTUAL_KEY(b'S' as u16)]);
            if let Some(n) = name {
                // wait for the Save dialog, then name the file
                let t0 = Instant::now();
                while fg_root() == h && t0.elapsed() < Duration::from_secs(3) {
                    std::thread::sleep(Duration::from_millis(80));
                }
                let dlg = fg_root();
                if dlg == h {
                    return Ok(String::new()); // saved straight away (it already had a name)
                }
                std::thread::sleep(Duration::from_millis(450));
                let (x, y) = text_spot(dlg);
                ghost::go_wait(x, y);
                press(&[VK_CONTROL, VIRTUAL_KEY(b'A' as u16)]);
                ghost::mood(app, "type");
                type_into(app, dlg, n);
                ghost::mood(app, "idle");
                std::thread::sleep(Duration::from_millis(200));
                press(&[VK_RETURN]);
                std::thread::sleep(Duration::from_millis(600));
            }
            Ok(String::new())
        }
        Step::Click(what) => {
            let h = ensure_target(app, cx).ok_or("There's no window to click in.")?;
            ghost::mood(app, "think");
            let found = crate::vision::locate(app, h, what, false);
            ghost::mood(app, "idle");
            let f = found.ok_or(format!("I looked everywhere in that window but couldn't find “{what}”."))?;
            ghost::go_wait(f.x as f64, f.y as f64);
            ghost::click(app);
            std::thread::sleep(Duration::from_millis(70));
            let invoked = f.el.as_ref().map(|el| unsafe { el.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId).and_then(|p| p.Invoke()).is_ok() }).unwrap_or(false);
            if !invoked {
                real_click(f.x, f.y);
            }
            crate::log_line(&format!("clicked “{what}” ({})", f.how));
            std::thread::sleep(Duration::from_millis(300));
            let fg = fg_root();
            if usable(fg, cx.own) {
                cx.target = Some(fg);
            }
            Ok(String::new())
        }
        Step::Ui(act, name, value) => {
            let h = ensure_target(app, cx).ok_or("There's no window to work in.")?;
            let said = crate::uictl::act(h, name, act, value)?;
            crate::log_line(&format!("ui: {act} “{name}” → {said}"));
            std::thread::sleep(Duration::from_millis(200));
            Ok(said)
        }
        Step::TypeIn(field, text) => {
            let h = ensure_target(app, cx).ok_or("There's no window to type into.")?;
            ghost::mood(app, "think");
            let found = crate::vision::locate(app, h, field, true);
            ghost::mood(app, "idle");
            let f = found.ok_or(format!("I couldn't find the {field} in that window."))?;
            ghost::go_wait(f.x as f64, f.y as f64);
            ghost::click(app);
            if let Some(el) = &f.el {
                let _ = unsafe { el.SetFocus() };
            }
            real_click(f.x, f.y);
            std::thread::sleep(Duration::from_millis(120));
            ghost::mood(app, "type");
            let ok = type_into(app, h, text);
            ghost::mood(app, "idle");
            if ok { Ok(String::new()) } else if crate::ai::cancelled() { Err("Stopped.".into()) } else { Err("You switched windows, so I stopped typing.".into()) }
        }
        Step::Message(app_name, to, text) => {
            let (uri, wname) = match app_name.as_str() {
                "telegram" => ("tg:", "Telegram"),
                "discord" => ("discord:", "Discord"),
                _ => ("whatsapp:", "WhatsApp"),
            };
            let t0 = Instant::now();
            // (a line break typed into a chat box would SEND the message: it goes in as a space)
            let text = text.replace(['\r', '\n'], " ");
            let before = fg_root();
            ghost::mood(app, "think");
            let existing = app.state::<crate::apps::Apps>().find_window(wname);
            match existing {
                Some(h) => crate::apps::focus_window(h),
                None => {
                    if !crate::apps::shell_open(uri) && commands::run(app, &format!("open {}", wname.to_lowercase())).ok == false {
                        return Err(format!("I couldn't open {wname}. Is it installed?"));
                    }
                }
            }
            // The window in front that carries the app's name is the one to work in. (A Store app has two
            // windows called "WhatsApp", its frame and what's inside it; working in the wrong one of the two
            // meant ten seconds of waiting and then "You switched windows", every time it was already open.)
            let own = cx.own;
            let is_app = |w: HWND| {
                let mut b = [0u16; 200];
                let n = unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowTextW(w, &mut b) }.max(0) as usize;
                usable(w, own) && String::from_utf16_lossy(&b[..n]).to_lowercase().contains(&wname.to_lowercase())
            };
            std::thread::sleep(Duration::from_millis(if existing.is_some() { 180 } else { 0 }));
            let h = if is_app(fg_root()) {
                fg_root()
            } else {
                match existing.filter(|e| fg_root() == *e) {
                    Some(e) => e,
                    None => wait_window(app, before, cx.own, wname, Duration::from_secs(10)).or(existing).ok_or(format!("{wname} didn't open."))?,
                }
            };
            let h = if fg_root() != h && is_app(fg_root()) { fg_root() } else { h };
            // (and if something else came to the front meanwhile, it's brought back before any typing)
            let bring = |h: HWND| {
                for _ in 0..8 {
                    if fg_root() == h {
                        return;
                    }
                    crate::apps::focus_window(h);
                    std::thread::sleep(Duration::from_millis(80));
                }
            };
            cx.target = Some(h);
            wait_ready(h);
            std::thread::sleep(Duration::from_millis(if existing.is_some() { 120 } else { 500 }));
            let (x, y) = text_spot(h);
            ghost::go_wait(x, y);
            ghost::mood(app, "type");
            let gone = |why: &str| -> String {
                if crate::ai::cancelled() {
                    return "Stopped.".into();
                }
                let mut b = [0u16; 120];
                let n = unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowTextW(fg_root(), &mut b) }.max(0) as usize;
                crate::log_line(&format!("message: the window in front changed to “{}”", String::from_utf16_lossy(&b[..n])));
                why.to_string()
            };
            // Find the chat. First by looking: the app's own search box, then the person's row in the results,
            // clicked where it is (pressing keys blind opened the wrong chat, or none, when the app was a moment slow).
            let mut opened = false;
            let mut win = RECT::default();
            let _ = unsafe { GetWindowRect(h, &mut win) };
            let (ww, wh) = ((win.right - win.left).max(1), (win.bottom - win.top).max(1));
            // (The boxes are asked for by kind, and names are read off the screen. Asking the app for everything on it,
            //  as this used to, took seconds a time in a chat app with its whole history on screen: 24 s to find a chat
            //  and 36 s to have the message written, measured. The thorough way is still there for when reading fails.)
            let search = {
                let mut found = None;
                for _ in 0..(if existing.is_some() { 3 } else { 8 }) {
                    found = crate::vision::edits(h).into_iter().filter(|(name, r)| crate::vision::norm(name).contains("search") && r.top < win.top + wh / 2)
                        .min_by_key(|(_, r)| r.top).map(|(_, r)| r);
                    if found.is_some() || crate::ai::cancelled() || fg_root() != h {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                found
            };
            if let Some(sb) = search {
                real_click((sb.left + sb.right) / 2, (sb.top + sb.bottom) / 2);
                std::thread::sleep(Duration::from_millis(140));
                bring(h);
                press(&[VK_CONTROL, VIRTUAL_KEY(b'A' as u16)]);
                if !type_fast(h, to) {
                    return Err(gone("You switched windows, so I stopped."));
                }
                // their row: the topmost line with their name under the search box, read off the screen
                let results = RECT { left: sb.left, top: sb.bottom + 4, right: sb.right, bottom: win.bottom };
                let (mut at, mut by) = (None, "reading the screen");
                for i in 0..5 {
                    std::thread::sleep(Duration::from_millis(if i == 0 { 420 } else { 260 }));
                    if crate::ai::cancelled() || fg_root() != h {
                        return Err(gone("You switched windows, so I stopped."));
                    }
                    at = crate::vision::find_text(results, to, |r| -(r.top - sb.bottom) as f64 / 4000.0);
                    if at.is_some() {
                        break;
                    }
                }
                if at.is_none() {
                    // (it couldn't be read: the thorough way, by what the app says about itself)
                    at = crate::vision::seen(h).into_iter()
                        .filter(|(edit, name, r)| !*edit && r.top >= sb.bottom - 2 && r.left < sb.right && r.right > sb.left && r.bottom - r.top < 130 && named(name, to))
                        .min_by_key(|(_, _, r)| r.top)
                        .map(|(_, _, r)| ((r.left + r.right) / 2, (r.top + r.bottom) / 2));
                    by = "its name";
                }
                match at {
                    Some((x, y)) => {
                        crate::log_line(&format!("message: found their chat by {by} after {} ms", t0.elapsed().as_millis()));
                        real_click(x, y);
                        opened = true;
                    }
                    None => crate::log_line("message: no row with that name in the search results"),
                }
                if opened {
                    // the chat is open when their name is at the top, outside the search column, and there's a box to write in
                    let two = (sb.right - sb.left) * 10 < ww * 6;
                    let top = RECT { left: if two { sb.right } else { win.left }, top: win.top, right: win.right, bottom: win.top + wh * 22 / 100 };
                    let mut ok = false;
                    let mut boxes: Vec<RECT> = Vec::new();
                    for _ in 0..7 {
                        std::thread::sleep(Duration::from_millis(220));
                        boxes = crate::vision::edits(h).into_iter().filter(|(name, r)| !crate::vision::norm(name).contains("search") && r.top > win.top + wh / 2).map(|(_, r)| r).collect();
                        if !boxes.is_empty() && crate::vision::find_text(top, to, |_| 0.0).is_some() {
                            ok = true;
                            break;
                        }
                    }
                    if !ok {
                        press(&[VK_ESCAPE]);
                        crate::log_line("message: the chat didn't open (their name isn't at the top)");
                        return Err(format!("I couldn't open a chat with {} in {wname}.", title(to)));
                    }
                    // click into the box the message goes in (the lowest one)
                    if let Some(b) = boxes.iter().max_by_key(|r| r.bottom) {
                        real_click((b.left + b.right) / 2, (b.top + b.bottom) / 2);
                        std::thread::sleep(Duration::from_millis(120));
                    }
                }
            }
            if !opened && search.is_none() {
                // an app that doesn't describe itself (Telegram, Discord): the keyboard way
                crate::log_line("message: no search box found by looking, using the keyboard");
                match app_name.as_str() {
                    "discord" => press(&[VK_CONTROL, VIRTUAL_KEY(b'K' as u16)]),
                    "telegram" => {
                        press(&[VK_ESCAPE]);
                        press(&[VK_ESCAPE]);
                        press(&[VK_CONTROL, VIRTUAL_KEY(b'F' as u16)]);
                    }
                    _ => press(&[VK_CONTROL, VIRTUAL_KEY(b'F' as u16)]),
                }
                std::thread::sleep(Duration::from_millis(260));
                press(&[VK_CONTROL, VIRTUAL_KEY(b'A' as u16)]);
                if !type_fast(h, to) {
                    return Err(gone("You switched windows, so I stopped."));
                }
                std::thread::sleep(Duration::from_millis(900)); // the search results
                if app_name == "whatsapp" {
                    press(&[VK_DOWN]);
                    std::thread::sleep(Duration::from_millis(90));
                }
                press(&[VK_RETURN]);
                std::thread::sleep(Duration::from_millis(450));
                // their name has to be somewhere other than the search box we just typed it into: the top of the window
                let top = RECT { left: win.left + ww * 30 / 100, top: win.top, right: win.right, bottom: win.top + wh * 22 / 100 };
                let mut ok = crate::vision::find_text(top, to, |_| 0.0).is_some();
                if !ok {
                    std::thread::sleep(Duration::from_millis(700));
                    ok = crate::vision::find_text(top, to, |_| 0.0).is_some();
                }
                if !ok {
                    press(&[VK_ESCAPE]);
                    return Err(format!("I couldn't find a chat with {} in {wname}.", title(to)));
                }
                opened = true;
            }
            if !opened {
                press(&[VK_ESCAPE]);
                return Err(format!("I couldn't find {} in {wname}. Check the name as it's saved there.", title(to)));
            }
            // nothing to say yet ("message Danish"): their chat is open, and what you say next is the message
            if text.trim().is_empty() {
                *crate::skills::ASKING.lock().unwrap() = Some((app_name.clone(), to.clone(), Instant::now()));
                ghost::mood(app, "idle");
                crate::log_line(&format!("message: chat open after {} ms, asking what to say", t0.elapsed().as_millis()));
                cx.say = Some(format!("{}'s chat is open. What should I say?", title(to)));
                return Ok(String::new());
            }
            bring(h);
            if !type_fast(h, &text) {
                return Err(gone("You switched windows, so I stopped."));
            }
            crate::log_line(&format!("message: written and waiting for a yes after {} ms", t0.elapsed().as_millis()));
            ghost::mood(app, "idle");
            // nothing is sent until you say so
            *crate::skills::PENDING.lock().unwrap() = Some(crate::skills::Pending { hwnd: h.0 as isize, to: title(to), app: wname.into(), at: Instant::now() });
            let _ = app.emit_to("main", "agent-confirm", json!({ "text": format!("Send “{}” to {} on {wname}?", short(&text, 60), title(to)) }));
            Ok(String::new())
        }
        Step::Email(to, subject, body) => {
            let mut body = body.clone();
            if body.is_empty() || body.split_whitespace().count() < 6 {
                // write it properly
                ghost::mood(app, "think");
                let what = if subject.is_empty() { body.clone() } else { format!("about {subject}. {body}") };
                if let Some(t) = crate::ai::oneshot(app, "You write clear, friendly, professional emails. Reply with only the email body (greeting, a few short paragraphs, sign-off without a name). No subject line, no markdown.", &format!("Write an email to {to} {what}"), 400, |_| {}) {
                    body = t;
                }
            }
            let addr = if to.contains('@') { to.replace(' ', "") } else { String::new() };
            let enc2 = |s: &str| enc(s).replace('+', "%20");
            let subj = if subject.is_empty() { String::new() } else { title(subject) };
            let url = format!("mailto:{addr}?subject={}&body={}", enc2(&subj), enc2(&body.replace("\n", "\r\n")));
            if !crate::apps::shell_open(&url) {
                return Err("I couldn't open your email app.".into());
            }
            ghost::mood(app, "idle");
            Ok(String::new())
        }
        Step::Rewrite(how, paste) => {
            let h = ensure_target(app, cx).ok_or("Select some text in an app first.")?;
            let before = crate::clip::read_text().unwrap_or_default();
            ghost::mood(app, "think");
            let _ = crate::clip::set("");
            press(&[VK_CONTROL, VIRTUAL_KEY(b'C' as u16)]);
            let text = copied();
            if text.trim().is_empty() {
                let _ = crate::clip::set(&before);
                return Err("Select the text you want me to work on first.".into());
            }
            let sys = if *paste {
                "You edit text. Apply the user's instruction to their text and reply with only the new text: same language unless asked to translate, keep the meaning, no quotes, no notes, no explanations, no markdown."
            } else {
                "You read the user's text and answer their request about it in plain spoken English, briefly."
            };
            let max = (text.len() / 3 + 80).min(1500);
            // Translating goes to the same fast translator as the Language menu (about a second, where the local
            // brain good at languages takes many); only when it can't is the local brain asked.
            let lang = {
                let h = how.to_lowercase();
                let to = h.rsplit_once(" into ").or_else(|| h.rsplit_once(" to ")).map(|(_, l)| l.trim().trim_end_matches(['.', '!', '?']).trim().to_string()).unwrap_or_default();
                let mut c = to.chars();
                c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
            };
            let fast = if *paste && how.to_lowercase().starts_with("translate") && !lang.is_empty() {
                crate::web::translate(&text.chars().take(4500).collect::<String>(), "Auto", &lang).filter(|t| !t.trim().is_empty())
            } else {
                None
            };
            if let Some(t) = &fast {
                let _ = app.emit_to("main", "rewrite-piece", t);
            }
            // (the new words go to the page as they're written: beside your selection you watch it being rewritten)
            let out = match fast {
                Some(t) => t,
                None => crate::ai::oneshot(app, sys, &format!("Instruction: {how}\n\nText:\n{}", text.chars().take(6000).collect::<String>()), max, |full| {
                    if *paste {
                        let _ = app.emit_to("main", "rewrite-piece", full);
                    }
                })
                .ok_or("My local brain isn't available right now.")?,
            };
            if crate::ai::cancelled() {
                let _ = crate::clip::set(&before);
                return Err("Stopped.".into());
            }
            if *paste {
                if fg_root() != h {
                    crate::apps::focus_window(h);
                    std::thread::sleep(Duration::from_millis(250));
                }
                let _ = crate::clip::set(&out);
                std::thread::sleep(Duration::from_millis(80));
                ghost::click(app);
                press(&[VK_CONTROL, VIRTUAL_KEY(b'V' as u16)]);
                std::thread::sleep(Duration::from_millis(400));
                let _ = crate::clip::set(&before); // your clipboard is left as it was
                Ok(String::new())
            } else {
                let _ = crate::clip::set(&before);
                cx.say = Some(out);
                Ok(String::new())
            }
        }
        Step::Screen(q) => {
            let h = cx.target.filter(|t| unsafe { IsWindowVisible(*t).as_bool() }).or_else(|| {
                let fg = fg_root();
                if usable(fg, cx.own) { Some(fg) } else { app.state::<crate::apps::Apps>().last_window() }
            }).ok_or("There's nothing on screen I can read.")?;
            ghost::mood(app, "think");
            let mut r = RECT::default();
            if unsafe { GetWindowRect(h, &mut r) }.is_ok() && r.right > r.left {
                ghost::go(r.left as f64 + (r.right - r.left) as f64 * 0.5, r.top as f64 + (r.bottom - r.top) as f64 * 0.4);
            }
            let t0 = Instant::now();
            let mut text = crate::skills::window_text(h, 7000);
            // apps that don't share their text: read it off the screen (a fraction of a second)
            if text.split_whitespace().count() < 40 {
                let seen = crate::vision::text_of(h, 7000);
                if seen.split_whitespace().count() > text.split_whitespace().count() {
                    text = seen;
                }
            }
            // questions about what can be pressed or filled in ("how many buttons", "is there a Save button",
            // "where's the search box") are answered from the numbered map of the window, counted here
            // (brains count badly) rather than guessed from its text or left to the slow eyes
            let ql = q.to_lowercase();
            let mut mapped = false;
            if ["button", "checkbox", "text box", "search box", "dropdown", "field", "menu", "link", "tabs", " tab ", "click", "press"].iter().any(|w| ql.contains(w)) {
                let map = crate::vision::map(app, h);
                let mut counts: Vec<(String, usize)> = Vec::new();
                for line in map.lines() {
                    let kind = line.split_once(' ').map(|(_, r)| r.split(" “").next().unwrap_or("")).unwrap_or("");
                    if kind.is_empty() || kind == "text" {
                        continue;
                    }
                    match counts.iter_mut().find(|(k, _)| k == kind) {
                        Some(c) => c.1 += 1,
                        None => counts.push((kind.to_string(), 1)),
                    }
                }
                if !counts.is_empty() {
                    // (an "icon" is a button with a picture and no words: found by looking, since the app doesn't name it)
                    let tally: Vec<String> = counts.iter().map(|(k, n)| {
                        let many = if *n == 1 { "" } else if k.ends_with('x') { "es" } else { "s" };
                        if k == "icon" { format!("{n} button{} that have a picture and no label (listed as “icon”)", if *n == 1 { "" } else { "s" }) } else { format!("{n} {k}{many}") }
                    }).collect();
                    text = format!("What this window has that can be pressed or filled in: {}.\nEverything on it, in reading order (number, kind, name, where):\n{}", tally.join(", "), map.chars().take(2800).collect::<String>());
                    mapped = true;
                }
            }
            crate::log_line(&format!("read the screen: {} words in {} ms{}", text.split_whitespace().count(), t0.elapsed().as_millis(), if mapped { " (map of its controls)" } else { "" }));
            // the vision model only for questions about how things look (it's much slower)
            let ask_eyes = !mapped && crate::see::available(app) && (crate::router::wants_eyes(q) || text.split_whitespace().count() < 15);
            if ask_eyes {
                ghost::say(app, "Looking at your screen…");
                let question = if q.is_empty() { "What's on this screen? Describe it in 3 or 4 sentences: what it is and the key things on it.".to_string() } else { q.clone() };
                // (and the words the window itself says it shows: the small vision model misreads small print, and with
                //  them beside the picture it names things right)
                let shown: Vec<&str> = text.split_whitespace().take(220).collect();
                let question = if shown.len() >= 5 { format!("{question}

(The words this window shows, read from it directly, so you read them right: {})", shown.join(" ")) } else { question };
                if let Some(answer) = crate::see::snapshot(h).and_then(|img| crate::see::ask(app, &img, &question)) {
                    ghost::mood(app, "idle");
                    cx.say = Some(answer);
                    return Ok(String::new());
                }
            }
            if text.split_whitespace().count() < 8 {
                return Err("I couldn't read any text in that window.".into());
            }
            let ask = if q.is_empty() { "Summarize what's on the user's screen in 3 or 4 sentences: what it is and the key points.".to_string() } else { q.clone() };
            let out = crate::ai::oneshot(
                app,
                "You are Sylver. The user is asking about what's on their screen; its text is given below. Answer in plain spoken English, briefly and accurately, using only that text.",
                &format!("Screen text:\n{text}\n\nRequest: {ask}"),
                170,
                |_| {},
            )
            .ok_or("My local brain isn't available right now.")?;
            ghost::mood(app, "idle");
            cx.say = Some(out);
            Ok(String::new())
        }
        Step::Timer(secs, what, kind) if kind.starts_with("alarm") => {
            // An alarm goes into Windows' own Clock app, where it rings like any other and stays listed. Only if Clock
            // can't be worked does ULTRON keep it and ring it itself.
            let days = kind.split_once(':').map(|x| x.1).unwrap_or("");
            let (h, m) = crate::skills::clock_after(*secs);
            let at = format!("{}:{:02} {}", if h % 12 == 0 { 12 } else { h % 12 }, m, if h < 12 { "AM" } else { "PM" });
            let label = if what.trim().is_empty() { "Sylver".to_string() } else { what.trim().chars().take(40).collect() };
            let often = match days { "daily" => " every day", "weekdays" => " on weekdays", "weekends" => " at weekends", _ => "" };
            match crate::clock::add_alarm(h, m, &label, days) {
                Ok(_) => {
                    cx.say = Some(format!("Alarm set for {at}{often}, in the Clock app."));
                    Ok(format!("Added a {at} alarm to Clock"))
                }
                Err(why) => {
                    crate::log_line(&format!("clock: couldn't add the alarm ({why}); ULTRON keeps it"));
                    let _ = app.emit_to("main", "timer", json!({ "secs": secs, "label": what, "kind": "alarm" }));
                    cx.say = Some(format!("I couldn't put it in the Clock app, so I'll ring it myself at {at}."));
                    Ok(format!("Set a {at} alarm in Sylver"))
                }
            }
        }
        Step::Timer(secs, what, kind) => {
            let _ = app.emit_to("main", "timer", json!({ "secs": secs, "label": what, "kind": kind }));
            let when = crate::skills::human(*secs);
            cx.say = Some(match kind.as_str() {
                "reminder" => format!("Okay, I'll remind you in {when}{}.", if what.is_empty() { String::new() } else { format!(" to {what}") }),
                "alarm" => format!("Alarm set. That's in {when}."),
                _ => format!("Timer's set for {when}."),
            });
            Ok(format!("Set a {when} {kind}"))
        }
        Step::Snap(d) => {
            let h = ensure_target(app, cx).ok_or("There's no window to move.")?;
            let _ = h;
            ghost::click(app);
            match d {
                2 => press(&[VK_LWIN, VK_SHIFT, VK_RIGHT]),
                -1 => press(&[VK_LWIN, VK_LEFT]),
                _ => press(&[VK_LWIN, VK_RIGHT]),
            }
            std::thread::sleep(Duration::from_millis(400));
            press(&[VK_ESCAPE]); // dismiss Snap Assist's suggestions
            Ok(String::new())
        }
        Step::CloseWindow => {
            let h = ensure_target(app, cx).ok_or("There's no window to close.")?;
            ghost::click(app);
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(Some(h), windows::Win32::UI::WindowsAndMessaging::WM_CLOSE, windows::Win32::Foundation::WPARAM(0), windows::Win32::Foundation::LPARAM(0));
            }
            cx.target = None;
            Ok(String::new())
        }
        Step::Brightness(v, rel) => {
            let target = if *rel { (crate::skills::brightness().unwrap_or(50) as i32 + v).clamp(0, 100) } else { *v };
            if !crate::skills::set_brightness(target as u32) {
                return Err("I can't change the brightness of this screen.".into());
            }
            Ok(format!("set the brightness to {target}%"))
        }
        Step::DarkMode(on) => {
            let dark = crate::skills::dark_mode(*on).ok_or("I couldn't change the theme.")?;
            Ok(if dark { "switched to dark mode".into() } else { "switched to light mode".into() })
        }
        Step::SettingsPage(_, uri) => {
            crate::apps::shell_open(uri);
            Ok(String::new())
        }
        Step::FindFile(f) => {
            let home = std::env::var("USERPROFILE").unwrap_or_default();
            let url = format!("search-ms:query={}&crumb=location:{}", enc(f).replace('+', "%20"), enc(&home).replace('+', "%20"));
            crate::apps::shell_open(&url);
            Ok(String::new())
        }
        Step::LatestDownload => {
            let f = crate::skills::latest_download().ok_or("Your Downloads folder is empty.")?;
            crate::apps::shell_open(&f.to_string_lossy());
            Ok(format!("opened {}", f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()))
        }
        Step::NewFolder(name, place) => {
            let dir = crate::skills::known_folder(place).join(name.trim());
            std::fs::create_dir_all(&dir).map_err(|_| "I couldn't create that folder.".to_string())?;
            crate::apps::shell_open(&dir.to_string_lossy());
            Ok(String::new())
        }
        Step::SiteSearch(site, q) => {
            let (name, url) = site_url(site, q).ok_or(format!("I'm not sure what to look for on {site}."))?;
            let before = fg_root();
            ghost::mood(app, "think");
            let c = screen_center();
            ghost::go(c.0, c.1 - 60.0);
            crate::apps::shell_open(&url);
            if let Some(h) = wait_window(app, before, cx.own, "", Duration::from_millis(3000)) {
                cx.target = Some(h);
            }
            let h = fg_root();
            let mut r = RECT::default();
            if unsafe { GetWindowRect(h, &mut r) }.is_ok() && r.right > r.left {
                ghost::go_wait(r.left as f64 + (r.right - r.left) as f64 * 0.45, r.top as f64 + (r.bottom - r.top) as f64 * 0.42);
            }
            ghost::mood(app, "idle");
            cx.last_open = site.clone();
            Ok(format!("Searched {name} for “{}”", shopping(q).0))
        }
        Step::Present(topic, n) => {
            ghost::mood(app, "think");
            let c = screen_center();
            ghost::go(c.0, c.1 - 80.0);
            let mut last_move = Instant::now() - Duration::from_secs(5);
            let path = crate::ppt::build(app, topic, *n, |msg, frac| {
                ghost::say(app, &format!("{msg}…"));
                ghost::progress(app, frac);
                ghost::raise(app);
                if msg.starts_with("Designing") {
                    ghost::mood(app, "idle");
                    // fly over the slide as it's being designed
                    if last_move.elapsed() > Duration::from_millis(400) {
                        last_move = Instant::now();
                        if let Some(h) = app.state::<crate::apps::Apps>().find_window("powerpoint") {
                            let mut r = RECT::default();
                            if unsafe { GetWindowRect(h, &mut r) }.is_ok() && r.right > r.left {
                                let k = (frac * 7.0).fract();
                                ghost::go(r.left as f64 + (r.right - r.left) as f64 * (0.35 + 0.3 * k), r.top as f64 + (r.bottom - r.top) as f64 * (0.35 + 0.25 * (1.0 - k)));
                                ghost::click(app);
                            }
                        }
                    }
                }
            })?;
            ghost::mood(app, "idle");
            crate::log_line(&format!("deck saved: {path}"));
            Ok(String::new())
        }
        Step::Wait(s) => {
            ghost::mood(app, "think");
            std::thread::sleep(Duration::from_secs_f32(*s));
            ghost::mood(app, "idle");
            Ok(String::new())
        }
        Step::Minimize | Step::Maximize => {
            let h = ensure_target(app, cx).ok_or("There's no window for that.")?;
            ghost::click(app);
            unsafe {
                let _ = ShowWindow(h, if matches!(step, Step::Minimize) { SW_MINIMIZE } else { SW_MAXIMIZE });
            }
            std::thread::sleep(Duration::from_millis(350));
            Ok(String::new())
        }
        Step::SwitchTo(name) => {
            let h = app.state::<crate::apps::Apps>().find_window(name).ok_or(format!("{} isn't open.", title(name)))?;
            crate::apps::focus_window(h);
            std::thread::sleep(Duration::from_millis(300));
            cx.target = Some(h);
            let (x, y) = text_spot(h);
            ghost::go_wait(x, y);
            Ok(String::new())
        }
        Step::Command(c) => {
            let l = commands::normalize(c);
            // "search for ..." right after opening a browser app: type it into that browser
            let query = after(&l, &["search google for", "search the web for", "search for", "search", "google", "look up"]).map(str::to_string);
            if let Some(q) = query.clone().filter(|_| BROWSERS.iter().any(|b| cx.last_open.contains(b))) {
                let h = ensure_target(app, cx).ok_or("The browser isn't in front.")?;
                ghost::click(app);
                press(&[VK_CONTROL, VIRTUAL_KEY(b'L' as u16)]);
                std::thread::sleep(Duration::from_millis(200));
                ghost::mood(app, "type");
                type_into(app, h, &q);
                ghost::mood(app, "idle");
                press(&[VK_RETURN]);
                return Ok(format!("Searched the web for “{q}”"));
            }
            // "... and search for X" after opening YouTube searches YouTube
            if let Some((name, url)) = query.as_deref().and_then(|q| site_url(&cx.last_open, q)) {
                crate::apps::shell_open(&url);
                return Ok(format!("Searched {name} for “{}”", query.unwrap_or_default()));
            }
            let mut c2 = c.clone();
            if cx.last_open.contains("spotify") {
                if let Some(q) = after(&l, &["play"]).filter(|q| !q.is_empty() && !q.contains("youtube")) {
                    c2 = format!("play {} on spotify", q.trim_end_matches(" on spotify"));
                }
            }
            let r = commands::run(app, &c2);
            if r.ok { Ok(r.action) } else { Err(r.reply) }
        }
    }
}

pub fn execute(app: &AppHandle, steps: Vec<Step>) -> Reply {
    let n = steps.len();
    let mut cx = Ctx { say: None, own: unsafe { GetCurrentProcessId() }, target: None, last_open: String::new() };
    ghost::show(app);
    let mut done: Vec<String> = Vec::new();
    let mut logs: Vec<String> = Vec::new();
    for (i, s) in steps.iter().enumerate() {
        if crate::ai::cancelled() {
            ghost::say(app, "Stopped");
            ghost::finish(app, false);
            return Reply { reply: "Stopped.".into(), action: logs.join(" · "), icon: "agent", ok: true };
        }
        let label = doing(s);
        ghost::progress(app, i as f64 / n as f64);
        emit(app, i, n, &label, "run");
        ghost::say(app, &format!("{label}…"));
        ghost::raise(app);
        crate::log_line(&format!("agent step {i}: {s:?}"));
        match exec(app, &mut cx, s) {
            Ok(action) => {
                emit(app, i, n, &label, "ok");
                done.push(did(s, &action));
                logs.push(if action.is_empty() { title(&did(s, "")) } else { action });
            }
            Err(why) => {
                emit(app, i, n, &label, "fail");
                ghost::say(app, if crate::ai::cancelled() { "Stopped" } else { "Couldn't finish" });
                ghost::finish(app, false);
                if why == "Stopped." {
                    let so_far = if done.is_empty() { String::new() } else { format!(" I'd already {}.", join(&done)) };
                    return Reply { reply: format!("Okay, I stopped.{so_far}"), action: logs.join(" · "), icon: "agent", ok: true };
                }
                let so_far = if done.is_empty() { String::new() } else { format!("I {}, but then got stuck: ", join(&done)) };
                crate::log_line(&format!("agent: step {i} failed: {why}"));
                return Reply { reply: format!("{so_far}{why}"), action: logs.join(" · "), icon: "agent", ok: false };
            }
        }
    }
    ghost::progress(app, 1.0);
    ghost::say(app, "Done");
    ghost::finish(app, true);
    let reply = match cx.say.take() {
        Some(said) => said,
        None if steps.iter().any(|s| matches!(s, Step::Message(_, _, t) if !t.trim().is_empty())) => format!("{}. Say “send it” or tap Send when you're happy with it.", title(&join(&done))),
        None => format!("Done. I {}.", join(&done)),
    };
    Reply { reply, action: logs.join(" · "), icon: "agent", ok: true }
}

/// ULTRON's hands, one step at a time, for the task agent (doer.rs): it decides each next step
/// after seeing how the last one went. The eyed cursor appears only once a step needs the screen.
pub struct Session {
    cx: Ctx,
    ghost: bool,
}

impl Session {
    pub fn new() -> Self {
        Session { cx: Ctx { say: None, own: unsafe { GetCurrentProcessId() }, target: None, last_open: String::new() }, ghost: false }
    }

    pub fn run(&mut self, app: &AppHandle, step: Step) -> Result<String, String> {
        if !self.ghost {
            ghost::show(app);
            self.ghost = true;
        }
        let label = doing(&step);
        ghost::say(app, &format!("{label}…"));
        ghost::raise(app);
        crate::log_line(&format!("doer hands: {step:?}"));
        if crate::ai::cancelled() {
            return Err("Stopped.".into());
        }
        let r = exec(app, &mut self.cx, &step)?;
        Ok(match self.cx.say.take() {
            Some(said) => said,
            None => format!("Done: {}.", did(&step, &r)),
        })
    }

    /// The window ULTRON is working in (or the one in front).
    fn window(&mut self, app: &AppHandle) -> Option<HWND> {
        self.cx.target.filter(|t| unsafe { IsWindowVisible(*t).as_bool() }).or_else(|| {
            let fg = fg_root();
            if usable(fg, self.cx.own) { Some(fg) } else { app.state::<crate::apps::Apps>().last_window() }
        })
    }

    /// What can be clicked or typed into in the app in front.
    pub fn look(&mut self, app: &AppHandle) -> Result<String, String> {
        let h = self.window(app).ok_or("There's no app window in front to look at.")?;
        let mut buf = [0u16; 200];
        let n = unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowTextW(h, &mut buf) }.max(0) as usize;
        let title = String::from_utf16_lossy(&buf[..n]);
        // everything on it, numbered (its own controls plus any text that's only pixels), with where each thing is
        let map = crate::vision::map(app, h);
        if map.is_empty() {
            return Err("I couldn't see anything in that window.".into());
        }
        Ok(format!("Window: {title}\nEverything on it (click or type by number):\n{map}"))
    }

    /// Click something by its number from the last look.
    pub fn click_mark(&mut self, app: &AppHandle, n: usize) -> Result<String, String> {
        let (x, y, what) = crate::vision::mark(n).ok_or(format!("There's no number {n}. Call look again to see what's there now."))?;
        if let Some(h) = self.window(app) {
            if fg_root() != h {
                crate::apps::focus_window(h);
                std::thread::sleep(Duration::from_millis(150));
            }
        }
        if crate::ai::cancelled() {
            return Err("Stopped.".into());
        }
        real_click(x, y);
        crate::log_line(&format!("clicked #{n}: {what}"));
        std::thread::sleep(Duration::from_millis(320));
        let fg = fg_root();
        if usable(fg, self.cx.own) {
            self.cx.target = Some(fg);
        }
        Ok(format!("Clicked {what}. Call look to see what changed."))
    }

    /// Click a numbered box, then type into it.
    pub fn type_at(&mut self, app: &AppHandle, n: usize, text: &str) -> Result<String, String> {
        self.click_mark(app, n)?;
        let h = self.window(app).ok_or("The window went away.")?;
        press(&[VK_CONTROL, VIRTUAL_KEY(b'A' as u16)]);
        if !type_fast(h, text) {
            return Err(if crate::ai::cancelled() { "Stopped.".into() } else { "You switched windows, so I stopped.".into() });
        }
        Ok(format!("Typed “{}” there.", short(text, 40)))
    }

    /// Turn the mouse wheel over the app in front.
    pub fn scroll(&mut self, app: &AppHandle, down: bool, notches: i32) -> Result<String, String> {
        let h = self.window(app).ok_or("There's no app window in front to scroll.")?;
        let mut r = RECT::default();
        unsafe {
            let _ = GetWindowRect(h, &mut r);
            let mut old = POINT::default();
            let _ = GetCursorPos(&mut old);
            let _ = SetCursorPos((r.left + r.right) / 2, (r.top + r.bottom) / 2);
            std::thread::sleep(Duration::from_millis(40));
            for _ in 0..notches.clamp(1, 20) {
                mouse_event(MOUSEEVENTF_WHEEL, 0, 0, if down { -120 } else { 120 }, 0);
                std::thread::sleep(Duration::from_millis(25));
            }
            let _ = SetCursorPos(old.x, old.y);
        }
        Ok(format!("Scrolled {}.", if down { "down" } else { "up" }))
    }

    pub fn finish(&mut self, app: &AppHandle, ok: bool) {
        if self.ghost {
            ghost::say(app, if ok { "Done" } else { "Couldn't finish" });
            ghost::finish(app, ok);
            self.ghost = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plans() {
        for q in [
            "Open Notepad and write Hello",
            "open notepad and write a poem about rain and save it as rain",
            "open chrome, search for cats and dogs",
            "search for salt and pepper",
            "tell me a joke and open spotify",
            "open spotify",
            "type Hello, World",
            "press ctrl s",
            "go to youtube and search for lofi",
            "open notepad then type \"buy milk and eggs\" then press enter",
            "click the save button",
            "what is the difference between open source and closed source",
            "find me wireless headphones under 2000 rupees on amazon",
            "show me the cheapest gaming mouse on flipkart",
            "search amazon for a phone case",
            "make me a presentation about black holes with 8 slides",
            "create a ppt on climate change",
            "open amazon and search for running shoes",
            "message priya on whatsapp saying I'll be there in 10",
            "text mom that dinner is ready",
            "tell rahul on telegram: gg well played",
            "tell me a joke",
            "email boss@company.com about leave saying I need Friday off",
            "write an email to my manager asking for a day off",
            "fix the grammar of this",
            "make this sound more professional",
            "translate the selected text to hindi",
            "summarize this page",
            "what's on my screen",
            "remind me in 10 minutes to call mom",
            "set a timer for 5 minutes",
            "set an alarm for 7 am",
            "snap this window left",
            "set brightness to 40",
            "turn on dark mode",
            "open bluetooth settings",
            "find file resume",
            "open my latest download",
            "create a folder called Project X on the desktop",
            "write 100 words on the USA",
            "write me a 100 Words essay on USA in notepad",
            "type hello world in notepad",
            "Open Powerpoint and make me a 2 Slide Presentation on Black Holes",
            "make a single slide presentation about mars",
            "make a slide deck about cats",
            "type cats and dogs in the search box",
            "open chrome and type hello in the address bar",
            "click the send button",
        ] {
            println!("{q:?} -> {:?}", plan(q));
        }
        for (site, q) in [("amazon", "wireless headphones under 2000 rupees"), ("flipkart", "the cheapest gaming mouse"), ("amazon", "best rated laptop below 50k"), ("youtube", "lofi beats")] {
            println!("{site} {q:?} -> {:?}", site_url(site, q));
        }
    }

    #[test]
    fn alarm_phrases_distinguish_durations_and_clock_times() {
        for (phrase, expected_secs) in [
            ("set an alarm in 10 minutes", 600),
            ("set an alarm for 10 minutes", 600),
            ("set an alarm for seven minutes", 420),
            ("wake me up in an hour", 3600),
        ] {
            match parse_step(phrase) {
                Step::Timer(secs, _, kind) => {
                    assert_eq!(secs, expected_secs, "{phrase}");
                    assert_eq!(kind, "alarm", "{phrase}");
                }
                other => panic!("{phrase} parsed as {other:?}"),
            }
        }

        for phrase in ["set an alarm at seven thirty pm", "set an alarm for 7:30 pm", "set an alarm for 7 tomorrow morning"] {
            match parse_step(phrase) {
                Step::Timer(secs, _, kind) => {
                    assert!((1..=86400).contains(&secs), "{phrase}");
                    assert_eq!(kind, "alarm", "{phrase}");
                }
                other => panic!("{phrase} parsed as {other:?}"),
            }
        }
    }
}


/// Spotify's big green Play circle inside `area` (screen coordinates): its centre, if there is one.
fn green_button(area: RECT) -> Option<(i32, i32)> {
    let px = crate::vision::grab(area)?;
    let (w, h) = ((area.right - area.left) as usize, (area.bottom - area.top) as usize);
    // a coarse grid of 4 px cells that are mostly Spotify green (#1ED760)
    const C: usize = 4;
    let (gw, gh) = (w / C, h / C);
    let mut cell = vec![false; gw * gh];
    for gy in 0..gh {
        for gx in 0..gw {
            let mut n = 0;
            for dy in 0..C {
                for dx in 0..C {
                    let i = ((gy * C + dy) * w + gx * C + dx) * 4;
                    let (b, g, r) = (px[i] as i32, px[i + 1] as i32, px[i + 2] as i32);
                    if (r - 30).abs() < 40 && (g - 215).abs() < 40 && (b - 96).abs() < 45 {
                        n += 1;
                    }
                }
            }
            cell[gy * gw + gx] = n >= 10;
        }
    }
    // the biggest round blob of green cells, button-sized
    let mut seen = vec![false; gw * gh];
    let mut best: Option<(usize, i32, i32)> = None;
    for start in 0..gw * gh {
        if !cell[start] || seen[start] {
            continue;
        }
        let mut stack = vec![start];
        seen[start] = true;
        let (mut n, mut x0, mut x1, mut y0, mut y1) = (0usize, usize::MAX, 0usize, usize::MAX, 0usize);
        while let Some(c) = stack.pop() {
            n += 1;
            let (cx, cy) = (c % gw, c / gw);
            x0 = x0.min(cx);
            x1 = x1.max(cx);
            y0 = y0.min(cy);
            y1 = y1.max(cy);
            for (nx, ny) in [(cx.wrapping_sub(1), cy), (cx + 1, cy), (cx, cy.wrapping_sub(1)), (cx, cy + 1)] {
                if nx < gw && ny < gh && cell[ny * gw + nx] && !seen[ny * gw + nx] {
                    seen[ny * gw + nx] = true;
                    stack.push(ny * gw + nx);
                }
            }
        }
        let (bw, bh) = ((x1 - x0 + 1) * C, (y1 - y0 + 1) * C);
        let round = bw as f64 / bh as f64;
        if (36..=110).contains(&bw) && (36..=110).contains(&bh) && (0.75..1.33).contains(&round) && n * C * C > bw * bh / 2 {
            if best.map(|b| n > b.0).unwrap_or(true) {
                best = Some((n, area.left + ((x0 + x1 + 1) * C / 2) as i32, area.top + ((y0 + y1 + 1) * C / 2) as i32));
            }
        }
    }
    best.map(|(_, x, y)| (x, y))
}

/// Is this song what Spotify is playing now?
fn spotify_playing(app: &AppHandle, name: &str) -> bool {
    let want: Vec<String> = name.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 1 && *w != "by").map(str::to_string).collect();
    let m = app.state::<crate::media::Media>();
    let st = m.state.lock().unwrap();
    st.sessions.iter().any(|s| {
        let have = format!("{} {}", s.title, s.artist).to_lowercase();
        s.app.to_lowercase().contains("spotify") && s.playing && want.iter().filter(|w| have.contains(w.as_str())).count() * 10 >= want.len().max(1) * 6
    })
}

/// Play a song in the Spotify app: open its track (or Spotify's search for it), press the big green
/// Play button (found on screen; Spotify hides its buttons from automation), and check it's playing.
pub fn spotify_play(app: &AppHandle, query: &str) -> Result<String, String> {
    let q = query.trim().trim_end_matches(" on spotify");
    let own = unsafe { GetCurrentProcessId() };
    let before = fg_root();
    // the exact track when we know it (remembered, or from MusicBrainz), else Spotify's own search
    let track = crate::skills::spotify_track(q);
    let uri = match &track {
        Some((id, _, _)) => format!("spotify:track:{id}"),
        None => format!("spotify:search:{}", q.replace(' ', "%20")),
    };
    if !crate::apps::shell_open(&uri) {
        return Err("Spotify doesn't seem to be installed.".into());
    }
    let label = match &track {
        Some((_, n, a)) if !a.is_empty() => format!("{n} by {a}"),
        Some((_, n, _)) => n.clone(),
        None => q.to_string(),
    };
    let check = track.as_ref().map(|(_, n, a)| format!("{n} {a}")).unwrap_or_else(|| q.to_string());
    let h = wait_window(app, before, own, "Spotify", Duration::from_secs(8)).or_else(|| app.state::<crate::apps::Apps>().find_window("spotify")).ok_or("Spotify didn't open.")?;
    crate::apps::focus_window(h);
    let mut win = RECT::default();
    unsafe { GetWindowRect(h, &mut win).ok().ok_or("Spotify's window isn't on screen.")? };
    let (w, ht) = (win.right - win.left, win.bottom - win.top);
    // the content area: right of the sidebar, below the top bar, above the player bar
    let content = RECT { left: win.left + w * 22 / 100, top: win.top + ht * 9 / 100, right: win.right - w * 2 / 100, bottom: win.bottom - 110 };
    let mut old = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut old);
    }
    // as soon as the page has drawn its Play button (up to 1.6 s)
    let mut spot = None;
    for _ in 0..9 {
        std::thread::sleep(Duration::from_millis(180));
        spot = green_button(content);
        if spot.is_some() {
            break;
        }
    }
    if spot.is_none() {
        // search results: the Top result card shows its Play button when the pointer is over it
        'hover: for fy in [30, 40, 50, 22] {
            for fx in [20, 32, 12, 44] {
                let (x, y) = (content.left + (content.right - content.left) * fx / 100, content.top + (content.bottom - content.top) * fy / 100);
                unsafe {
                    let _ = SetCursorPos(x, y);
                }
                ghost::go(x as f64, y as f64);
                std::thread::sleep(Duration::from_millis(140));
                if let Some(s) = green_button(content) {
                    spot = Some(s);
                    break 'hover;
                }
            }
        }
    }
    let result = match spot {
        Some((x, y)) => {
            ghost::go_wait(x as f64, y as f64);
            ghost::click(app);
            real_click(x, y);
            let mut ok = false;
            for _ in 0..24 {
                std::thread::sleep(Duration::from_millis(150));
                if spotify_playing(app, &check) {
                    ok = true;
                    break;
                }
            }
            if ok { Ok(format!("Playing {label} on Spotify")) } else { Ok(format!("Started {label} in Spotify")) }
        }
        None => {
            // ULTRON Vision: read the results and double-click the song's row
            let name = track.as_ref().map(|(_, n, _)| n.clone()).unwrap_or_else(|| q.to_string());
            let mid = (content.left + content.right) / 2;
            match crate::vision::find_text(content, &name, |r| if r.left > mid { 0.02 } else { 0.0 }) {
                Some((x, y)) => {
                    ghost::go_wait(x as f64, y as f64);
                    ghost::click(app);
                    real_click(x, y);
                    std::thread::sleep(Duration::from_millis(70));
                    real_click(x, y);
                    let mut ok = false;
                    for _ in 0..20 {
                        std::thread::sleep(Duration::from_millis(150));
                        if spotify_playing(app, &check) {
                            ok = true;
                            break;
                        }
                    }
                    if ok { Ok(format!("Playing {label} on Spotify")) } else { Ok(format!("Started {label} in Spotify")) }
                }
                None => Err(format!("I opened {label} in Spotify, but couldn't find its Play button. It's right there for you.")),
            }
        }
    };
    unsafe {
        let _ = SetCursorPos(old.x, old.y);
    }
    if let Some((id, n, a)) = &track {
        crate::skills::remember_track(q, id, n, a);
    }
    result
}

/// You said "send it" (or tapped Send / Cancel) for a message ULTRON drafted.
pub fn confirm(app: &AppHandle, ok: bool) -> String {
    // a system action the task agent asked about ("Run this command?")
    if let Some(r) = crate::doer::answer_pending(app, if ok { "yes" } else { "no" }) {
        return r.reply;
    }
    let Some(p) = crate::skills::PENDING.lock().unwrap().take() else { return "There's nothing waiting to be sent.".into() };
    if p.at.elapsed() > Duration::from_secs(10 * 60) {
        return "That draft is too old now, so I left it.".into();
    }
    let h = HWND(p.hwnd as _);
    if !unsafe { IsWindowVisible(h).as_bool() } {
        return format!("{} isn't open any more.", p.app);
    }
    crate::apps::focus_window(h);
    std::thread::sleep(Duration::from_millis(300));
    if fg_root() != h {
        return format!("I couldn't bring {} to the front, so I didn't send it.", p.app);
    }
    if ok {
        press(&[VK_RETURN]);
        let _ = app.emit_to("main", "agent-sent", ());
        format!("Sent to {}.", p.to)
    } else {
        press(&[VK_CONTROL, VIRTUAL_KEY(b'A' as u16)]);
        press(&[VK_BACK]);
        format!("Okay, I deleted the draft to {}.", p.to)
    }
}

#[cfg(test)]
mod price_tests {
    use super::*;

    #[test]
    fn prices_are_read_however_they_are_said() {
        let r = |q: &str| {
            let (w, lo, hi, _) = shopping_range(q);
            (w, lo, hi)
        };
        assert_eq!(r("laptops under 30 000"), ("laptops".to_string(), None, Some(30000)));
        assert_eq!(r("wireless headphones under 2000 rupees"), ("wireless headphones".to_string(), None, Some(2000)));
        assert_eq!(r("wireless headphones under ₹2,000"), ("wireless headphones".to_string(), None, Some(2000)));
        assert_eq!(r("a phone below two thousand five hundred"), ("phone".to_string(), None, Some(2500)));
        assert_eq!(r("laptop below 50k"), ("laptop".to_string(), None, Some(50000)));
        assert_eq!(r("tv under 1.5 lakh"), ("tv".to_string(), None, Some(150000)));
        assert_eq!(r("shoes between 1000 and 2,000 rupees"), ("shoes".to_string(), Some(1000), Some(2000)));
        assert_eq!(r("monitor above 10k"), ("monitor".to_string(), Some(10000), None));
        assert_eq!(r("phone under 20000 128 gb"), ("phone 128 gb".to_string(), None, Some(20000)));
        assert_eq!(r("files under 5 mb"), ("files under 5 mb".to_string(), None, None));
        assert_eq!(r("over ear headphones under 1500."), ("over ear headphones".to_string(), None, Some(1500)));
        assert_eq!(site_search("find me a keyboard under 1500").map(|s| s.0), Some("amazon".to_string()));
        assert_eq!(site_search("find files under 5 mb"), None);
        assert!(site_url("amazon", "laptops under 30 000").unwrap().1.contains("p_36%3A-3000000"));
        assert!(site_url("flipkart", "shoes between 1000 and 2000").unwrap().1.contains("from%3D1000") );
    }
}
