//! Web answers: facts from Brave Search for ULTRON's answers. A question about the world (people, events, prices, "what
//! is", "how many") is looked up on Brave Search from a page of ULTRON's own, hidden and kept ready in the background:
//! the results are fetched from inside it (Brave answers a real browser's page, not a bare request), read as text, and
//! only the lines that bear on the question kept, a few hundred characters, which the brain answers from in a sentence
//! or two. Kept open for five quiet minutes, so a question after another doesn't wait for it to start.
//!
//! Nothing is clicked, typed or solved there: a check asking whether this is a person stands it down for ten minutes,
//! and Wikipedia, the news and DuckDuckGo answer meanwhile. With no internet, nothing waits on it: the local brain
//! answers at once.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

const LABEL: &str = "uplink";
static ONE: Mutex<()> = Mutex::new(());
static LAST: Mutex<Option<Instant>> = Mutex::new(None);
static BLOCKED: Mutex<Option<Instant>> = Mutex::new(None);
static TIDY: AtomicBool = AtomicBool::new(false);
/// The last look found no internet (the page's fetch couldn't reach anything)
pub static OFFLINE: AtomicBool = AtomicBool::new(false);
/// What Brave said to each question in the last ten minutes
static SEEN: std::sync::LazyLock<Mutex<std::collections::HashMap<String, (Instant, String)>>> = std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// Fetches Brave's results for a question from inside its own page and reads them (titles and the words under each),
/// leaving what it found in `window.__u`; POLL reads that.
const START: &str = r#"(() => {
  window.__u = null;
  if (location.host !== 'search.brave.com'){ window.__u = {ready:true, text:'', wrong:true}; return 1; }
  const t = e => (e && e.textContent || '').replace(/\s+/g, ' ').trim();
  fetch('/search?q=' + encodeURIComponent(WANT) + '&source=web', {credentials:'include'}).then(r => {
    if (r.status === 429 || r.status === 403) return {blocked:true};
    return r.text().then(h => {
      const d = new DOMParser().parseFromString(h, 'text/html'), out = [];
      if (d.querySelector('#captcha, form[action*="captcha"], .captcha')) return {blocked:true};
      for (const c of [...d.querySelectorAll('.generic-snippet .content')].slice(0, 8)){
        let p = c; for (let i = 0; i < 6 && p && !p.querySelector('.title'); i++) p = p.parentElement;
        const h2 = p ? t(p.querySelector('.title')) : '', s = t(c).replace(/^([A-Z][a-z]+ \d{1,2}, \d{4}|\d+ (minutes?|hours?|days?|weeks?|months?|years?) ago) - /, '');
        if (s) out.push((h2 ? h2 + '\t' : '') + s);
      }
      const ib = d.querySelector('#infobox'); const ibt = ib ? t(ib).slice(0, 600) : '';
      if (ibt && out.length < 3) out.push(ibt);
      return {ready:true, text:out.join('\n')};
    });
  }).then(v => { window.__u = v; }, () => { window.__u = {ready:true, text:'', offline:true}; });
  return 1;
})()"#;
const POLL: &str = "(() => window.__u)()";

fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else if b == b' ' { "+".into() } else { format!("%{b:02X}") }).collect()
}

fn window(app: &AppHandle) -> Result<WebviewWindow, String> {
    if let Some(w) = app.get_webview_window(LABEL) {
        return Ok(w);
    }
    WebviewWindowBuilder::new(app, LABEL, WebviewUrl::External("https://search.brave.com/".parse().map_err(|_| "bad address")?))
        .title("ULTRON")
        .visible(false)
        .focused(false)
        .skip_taskbar(true)
        .decorations(false)
        .shadow(false)
        // (not private: Brave keeps its own small cookies between questions, and asks for a check far less often)
        .inner_size(900.0, 800.0)
        .position(-3000.0, -3000.0)
        .additional_browser_args(crate::BROWSER_ARGS) // (the same as every other ULTRON window, or the engine refuses it)
        .build()
        .map_err(|e| e.to_string())
}

/// Run a little script in the page and give back what it returns.
fn run(w: &WebviewWindow, js: &str) -> Option<Value> {
    let (tx, rx) = mpsc::channel::<String>();
    let js = js.to_string();
    w.with_webview(move |pv| unsafe {
        let Ok(core) = pv.controller().CoreWebView2() else { return };
        let done = webview2_com::ExecuteScriptCompletedHandler::create(Box::new(move |_, out| {
            let _ = tx.send(out);
            Ok(())
        }));
        let _ = core.ExecuteScript(&wcore62::HSTRING::from(js.as_str()), &done);
    })
    .ok()?;
    serde_json::from_str(&rx.recv_timeout(Duration::from_secs(3)).ok()?).ok()
}

/// (closed after twenty quiet minutes: its memory, a page's worth, goes back at once)
fn keep_tidy(app: &AppHandle) {
    if TIDY.swap(true, SeqCst) {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(15));
        let quiet = LAST.lock().unwrap().map_or(true, |t| t.elapsed() > Duration::from_secs(1200));
        if quiet {
            if let Some(w) = app.get_webview_window(LABEL) {
                let _ = w.destroy();
            }
            TIDY.store(false, SeqCst);
            return;
        }
    });
}

/// The lines of the results that bear on the question, in their order, about `most` characters in all (Google's own
/// answers, first on the page, count for a little more).
fn boil(q: &str, text: &str, most: usize) -> String {
    const NOISE: [&str; 9] = ["People also ask", "Videos", "Images", "Feedback", "Translate this page", "Missing:", "Show more", "Related searches", "Top stories"];
    let keys: Vec<String> = q.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 2 && !["the", "and", "what", "who", "how", "was", "are", "for", "does", "did", "with", "this", "that", "from", "when", "where", "which", "why", "much", "many"].contains(w)).map(String::from).collect();
    let mut seen = HashSet::new();
    let mut lines: Vec<(usize, f32, String)> = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let l = raw.trim();
        if l.chars().count() < 25 || NOISE.iter().any(|n| l.starts_with(n)) || l.contains(" › ") || !seen.insert(l.to_lowercase()) {
            continue;
        }
        let low = l.to_lowercase();
        let score = keys.iter().filter(|k| low.contains(k.as_str())).count() as f32 + if i < 3 { 1.5 } else { 0.0 };
        lines.push((i, score, l.chars().take(320).collect()));
    }
    let mut best = lines.clone();
    best.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut keep = HashSet::new();
    let mut size = 0;
    for (i, _, l) in &best {
        if size + l.len() > most && size > 0 {
            continue;
        }
        size += l.len();
        keep.insert(*i);
    }
    lines.into_iter().filter(|(i, _, _)| keep.contains(i)).map(|(_, _, l)| l).collect::<Vec<_>>().join("\n")
}

/// Google's own answer (its overview, or the answer box at the top), in a few sentences: moderate, not a page.
fn moderate(s: &str) -> String {
    let s = s.split('\n').map(str::trim).filter(|l| l.len() > 2 && !l.starts_with("Show more") && !l.starts_with("AI responses may")).collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for sentence in s.split_inclusive(['.', '!', '?']) {
        if out.len() > 220 && out.len() + sentence.len() > 460 {
            break;
        }
        out.push_str(sentence);
        if out.matches(['.', '!', '?']).count() >= 4 {
            break;
        }
    }
    let out = out.trim().to_string();
    if out.len() > 520 { format!("{}…", out.chars().take(500).collect::<String>().trim_end()) } else { out }
}

/// What Google says about a question: the lines that bear on it (for the brain to answer from), and Google's own
/// answer when it gives one (said as it is: no brain needed, so it's quick).
pub struct Facts {
    pub text: String,
    pub direct: Option<String>,
}

/// The window opened ahead (the notch has just opened: a question may be coming), on Google's page, ready.
pub fn warm(app: &AppHandle) {
    if BLOCKED.lock().unwrap().is_some_and(|t| t > Instant::now()) || app.get_webview_window(LABEL).is_some() {
        return;
    }
    if window(app).is_ok() {
        *LAST.lock().unwrap() = Some(Instant::now());
        keep_tidy(app);
    }
}

/// Is there internet at all (asked before saying there isn't: Brave's page failing to fetch isn't always that)
pub fn online() -> bool {
    ["1.1.1.1:443", "8.8.8.8:53"].iter().any(|a| a.parse().is_ok_and(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_millis(700)).is_ok()))
}

/// The sentences in a piece of text (not cut at "Dr." or "U.S.")
fn sentences(t: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (b, mut from) = (t.as_bytes(), 0);
    for i in 0..b.len() {
        if matches!(b[i], b'.' | b'!' | b'?') && b.get(i + 1) == Some(&b' ') && b.get(i + 2).is_some_and(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'"') {
            let word = t[from..i].rsplit(' ').next().unwrap_or("");
            if word.len() >= 3 && !word.contains('.') && !["Mr", "Mrs", "Ms", "Dr", "St", "Jr", "Sr", "vs", "No"].contains(&word) {
                out.push(t[from..=i].trim());
                from = i + 1;
            }
        }
    }
    if from < t.len() {
        out.push(t[from..].trim());
    }
    out
}

/// A sentence without what's in brackets (a pronunciation, "ⓘ", a footnote mark): said aloud, they're noise
fn unparen(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0;
    for c in s.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ").replace(" ,", ",").replace(" .", ".").replace(" ;", ";")
}

/// The sentence in the results that answers the question, said as it is when one clearly does: at once, with no wait
/// on the brain (on a laptop holding its graphics card back, the brain took ten seconds to retell it, and sometimes
/// got it wrong). None: nothing answers it clearly, and the brain reads the results instead.
pub fn answer_in(q: &str, text: &str) -> Option<String> {
    const SKIP: [&str; 53] = ["the", "and", "what", "who", "how", "was", "were", "are", "is", "for", "does", "did", "with", "this", "that", "from", "when", "where",
        "which", "why", "much", "many", "tell", "about", "me", "you", "your", "can", "could", "please", "of", "in", "on", "to", "do", "it", "its", "whats",
        "define", "meaning", "mean", "an", "a", "be", "book", "books", "novel", "novels", "movie", "film", "song", "series", "name"];
    // (one word for the forms a question and its answer put it in: "who wrote" is answered by "written by", "author")
    let stem = |w: &str| -> String {
        let w = w.trim_end_matches("'s").trim_end_matches('’');
        let w = if w.len() > 3 { w.trim_end_matches('s') } else { w };
        match w {
            "wrote" | "write" | "written" | "writer" | "author" | "authored" => "write",
            "directed" | "director" | "direct" => "direct",
            "invented" | "inventor" | "invent" | "invention" => "invent",
            "founded" | "founder" | "found" => "found",
            "discovered" | "discoverer" | "discover" | "discovery" => "discover",
            "painted" | "painter" | "paint" => "paint",
            "sang" | "sung" | "singer" | "sing" => "sing",
            "built" | "builder" | "build" => "build",
            "born" | "birth" => "born",
            "died" | "die" | "death" | "dead" => "die",
            w => w,
        }
        .to_string()
    };
    let ql = q.to_lowercase().replace('’', "'");
    let keys: Vec<String> = ql.split(|c: char| !c.is_alphanumeric() && c != '\'').filter(|w| w.len() > 1 && !SKIP.contains(w)).map(stem).collect();
    if keys.is_empty() || keys.len() > 9 {
        return None;
    }
    let num = ["how many", "how much", "how old", "how long", "how far", "how tall", "how big", "what year", "when ", "population", "price", "cost", "age of"].iter().any(|w| ql.contains(w));
    let define = ql.starts_with("what is") || ql.starts_with("what are") || ql.starts_with("what's") || ql.contains("meaning") || ql.starts_with("define") || ql.starts_with("who is") || ql.starts_with("who was");
    let mut best: Option<(f32, String)> = None;
    for (rank, line) in text.lines().enumerate() {
        let body = line.split_once('\t').map_or(line, |(_, b)| b);
        for (k, s) in sentences(body).into_iter().enumerate() {
            let s = unparen(s);
            let s = s.as_str();
            let n = s.chars().count();
            if !(20..=240).contains(&n) || s.contains("...") || s.contains('…') || s.ends_with('?') || s.contains(" | ") {
                continue;
            }
            let low = s.to_lowercase();
            let words: Vec<String> = low.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(stem).collect();
            let hit = keys.iter().filter(|k| words.contains(k)).count() as f32;
            let cover = hit / keys.len() as f32;
            if cover < 0.6 {
                continue;
            }
            let digits = s.chars().any(|c| c.is_ascii_digit());
            let mut score = cover * 3.0 + hit * 0.3 - rank as f32 * 0.35 - k as f32 * 0.25;
            if num {
                score += if digits { 0.8 } else { -1.5 };
            }
            if define && (low.contains(" is ") || low.contains(" are ") || low.contains(" was ") || low.contains(" refers to ")) {
                score += 0.5;
            }
            if !s.starts_with(|c: char| c.is_uppercase() || c.is_ascii_digit()) {
                score -= 1.0;
            }
            // ("He was the second person to walk on the Moon": true, and no answer to who)
            let first = low.split_whitespace().next().unwrap_or("");
            if ["he", "she", "it", "they", "this", "these", "those", "his", "her", "its", "their", "that", "there"].contains(&first) {
                continue;
            }
            if n > 180 {
                score -= 0.5;
            }
            if best.as_ref().map_or(true, |b| score > b.0) {
                best = Some((score, s.to_string()));
            }
        }
    }
    let (score, mut s) = best.filter(|b| b.0 >= 1.8)?;
    // (the little marks results carry: "[1]", a trailing dash or colon)
    while let Some(a) = s.rfind('[').filter(|&a| s[a..].len() <= 5 && s.ends_with(']')) {
        s.truncate(a);
    }
    let s = s.trim().trim_end_matches([',', ';', ':', '-', '—']).trim().to_string();
    let s = if s.ends_with(['.', '!']) { s } else { s + "." };
    crate::log_line(&format!("web: answered from the results themselves (sure {score:.1})"));
    Some(s)
}

fn untag(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut inside = false;
    for c in s.chars() {
        match c {
            '<' => inside = true,
            '>' => inside = false,
            c if !inside => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&").replace("&#x27;", "'").replace("&#39;", "'").replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">").replace("&nbsp;", " ")
        .split_whitespace().collect::<Vec<_>>().join(" ")
}

/// DuckDuckGo's results for a question (its plain page, which answers a plain request in about a second): asked at
/// the same time as Brave, so a check on Brave's side, or its page still getting ready, never leaves a question without
/// the web.
fn ddg(q: &str) -> Option<Facts> {
    let t0 = Instant::now();
    let url = format!("https://html.duckduckgo.com/html/?q={}", enc(q.trim()));
    let html = crate::net::agent(Duration::from_millis(2600)).get(&url)
        .set("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36")
        .call().ok()?.into_string().ok()?;
    let mut lines = Vec::new();
    for block in html.split("class=\"result results_links").skip(1) {
        if block.contains("result--ad") || lines.len() >= 8 {
            continue;
        }
        let pick = |mark: &str| block.split_once(mark).and_then(|(_, r)| r.split_once('>')).and_then(|(_, r)| r.split_once("</a>")).map(|(t, _)| untag(t)).unwrap_or_default();
        let (title, words) = (pick("class=\"result__a\""), pick("class=\"result__snippet\""));
        if !words.is_empty() {
            lines.push(if title.is_empty() { words } else { format!("{title}\t{words}") });
        }
    }
    OFFLINE.store(false, SeqCst);
    let text = boil(q, &lines.join("\n"), 700);
    crate::log_line(&format!("web: DuckDuckGo's results for that in {} ms, boiled down to {} characters", t0.elapsed().as_millis(), text.len()));
    (!text.is_empty()).then(|| Facts { direct: answer_in(q, &text), text })
}

/// The web's facts for a question: Brave and DuckDuckGo asked at once. The first with a sentence that answers it is
/// used; failing that, Brave's results if they came, else DuckDuckGo's.
pub fn facts(app: &AppHandle, q: &str) -> Option<Facts> {
    let (tx, rx) = mpsc::channel::<(bool, Option<Facts>)>();
    for brave_side in [true, false] {
        let (tx, app, q) = (tx.clone(), app.clone(), q.to_string());
        std::thread::spawn(move || {
            let _ = tx.send((brave_side, if brave_side { brave(&app, &q) } else { ddg(&q) }));
        });
    }
    drop(tx);
    let t0 = Instant::now();
    let (mut kept, mut from_brave) = (None::<Facts>, false);
    while let Some(left) = Duration::from_millis(3200).checked_sub(t0.elapsed()) {
        let Ok((is_brave, f)) = rx.recv_timeout(left) else { break };
        let Some(f) = f else { continue };
        if f.direct.is_some() {
            return Some(f);
        }
        if kept.is_none() || (is_brave && !from_brave) {
            from_brave = is_brave;
            kept = Some(f);
        }
    }
    kept
}

/// Brave's results for a question, boiled down to what bears on it (None: it's standing down, it didn't answer in time,
/// there's no internet, or there was nothing).
fn brave(app: &AppHandle, q: &str) -> Option<Facts> {
    if BLOCKED.lock().unwrap().is_some_and(|t| t > Instant::now()) {
        return None;
    }
    // (the same question within ten minutes: what Brave said then, at once, and Brave isn't asked again)
    let key = q.trim().to_lowercase();
    if let Some((at, text)) = SEEN.lock().unwrap().get(&key) {
        if at.elapsed() < Duration::from_secs(600) {
            return Some(Facts { direct: answer_in(q, text), text: text.clone() });
        }
    }
    let _one = ONE.try_lock().ok()?; // (one look at a time)
    let t0 = Instant::now();
    let fresh = app.get_webview_window(LABEL).is_none();
    let w = window(app).ok()?;
    *LAST.lock().unwrap() = Some(Instant::now());
    keep_tidy(app);
    // (its page still coming in, the first time takes ten seconds or so: this question isn't kept waiting for it; the
    //  brain answers now, and the next question has Brave)
    // (its page still coming in: waited on for up to two seconds, it's usually ready in one; past that the question isn't
    //  kept waiting, the brain answers now, and the next question has Brave)
    let ready = || run(&w, "(() => location.host === 'search.brave.com' && document.readyState !== 'loading')()") == Some(Value::Bool(true));
    let _ = fresh;
    while !ready() {
        if t0.elapsed() > Duration::from_millis(2000) {
            crate::log_line("web: Brave's page is still getting ready; answered without it this time");
            return None;
        }
        std::thread::sleep(Duration::from_millis(80));
    }
    let js = START.replace("WANT", &serde_json::to_string(q.trim()).unwrap_or_default());
    run(&w, &js)?;
    let t1 = Instant::now();
    while t1.elapsed() < Duration::from_millis(3000) {
        std::thread::sleep(Duration::from_millis(60));
        let Some(v) = run(&w, POLL) else { continue };
        if v.is_null() {
            continue;
        }
        if v["blocked"] == true {
            *BLOCKED.lock().unwrap() = Some(Instant::now() + Duration::from_secs(600));
            crate::log_line("web: Brave asked for a check; standing down for ten minutes");
            return None;
        }
        if v["wrong"] == true {
            let _ = w.navigate("https://search.brave.com/".parse().ok()?);
            return None;
        }
        OFFLINE.store(v["offline"] == true, SeqCst);
        let text = boil(q, v["text"].as_str().unwrap_or(""), 700);
        crate::log_line(&format!("web: Brave's results for that in {} ms, boiled down to {} characters{}", t0.elapsed().as_millis(), text.len(), if v["offline"] == true { " (no internet)" } else { "" }));
        if !text.is_empty() {
            let mut seen = SEEN.lock().unwrap();
            seen.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(600));
            seen.insert(key, (Instant::now(), text.clone()));
        }
        return (!text.is_empty()).then(|| Facts { direct: answer_in(q, &text), text });
    }
    crate::log_line("web: Brave didn't answer in time");
    None
}
