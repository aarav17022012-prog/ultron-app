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
        if (s) out.push((h2 ? h2 + ': ' : '') + s);
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

/// Brave's results for a question, boiled down to what bears on it (None: it's standing down, it didn't answer in time,
/// there's no internet, or there was nothing).
pub fn facts(app: &AppHandle, q: &str) -> Option<Facts> {
    if BLOCKED.lock().unwrap().is_some_and(|t| t > Instant::now()) {
        return None;
    }
    // (the same question within ten minutes: what Brave said then, at once, and Brave isn't asked again)
    let key = q.trim().to_lowercase();
    if let Some((at, text)) = SEEN.lock().unwrap().get(&key) {
        if at.elapsed() < Duration::from_secs(600) {
            return Some(Facts { text: text.clone(), direct: None });
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
    if fresh || run(&w, "(() => location.host === 'search.brave.com' && document.readyState !== 'loading')()") != Some(Value::Bool(true)) {
        crate::log_line("web: Brave's page is still getting ready; answered without it this time");
        return None;
    }
    let js = START.replace("WANT", &serde_json::to_string(q.trim()).unwrap_or_default());
    run(&w, &js)?;
    while t0.elapsed() < Duration::from_millis(3000) {
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
        let text = boil(q, v["text"].as_str().unwrap_or(""), 700);
        crate::log_line(&format!("web: Brave's results for that in {} ms, boiled down to {} characters{}", t0.elapsed().as_millis(), text.len(), if v["offline"] == true { " (no internet)" } else { "" }));
        if !text.is_empty() {
            let mut seen = SEEN.lock().unwrap();
            seen.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(600));
            seen.insert(key, (Instant::now(), text.clone()));
        }
        return (!text.is_empty()).then_some(Facts { text, direct: None });
    }
    crate::log_line("web: Brave didn't answer in time");
    None
}
