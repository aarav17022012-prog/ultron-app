//! Uplink: facts from Google for ULTRON's answers. A question about the world (people, events, prices, "what is",
//! "how many") is looked up on Google in a hidden window of ULTRON's own (private: nothing kept between runs, nothing
//! shared with any other window or account), the results page read as text, and only the lines that bear on the
//! question kept, a few hundred characters, which the brain answers from in a sentence or two.
//!
//! Nothing is clicked, typed or solved on Google's side: a page asking whether this is a person (its "unusual
//! traffic" check) or for cookie consent stands Uplink down for half an hour, and Wikipedia, the news and DuckDuckGo
//! answer meanwhile. The window closes after a few quiet minutes, and its memory goes with it.

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

/// What the results page says, as text: Google's own answers first (an overview, a quick answer, the side panel),
/// then each result's title and words. `{ready:false}` until it's the page for this question, fully loaded.
const READ: &str = r#"(() => {
  if (location.pathname.startsWith('/sorry') || location.host.startsWith('consent.') || document.querySelector('form#captcha-form, #recaptcha')) return {blocked:true};
  if (new URLSearchParams(location.search).get('q') !== WANT || document.readyState !== 'complete' || !document.querySelector('#rso, #search')) return {ready:false};
  const t = el => (el && el.innerText || '').trim(), out = [];
  const ov = [...document.querySelectorAll('h1, h2, [role="heading"]')].find(h => /^AI Overview/i.test(h.textContent.trim()));
  if (ov){ let box = ov; for (let k = 0; k < 4 && box.parentElement && t(box).length < 200; k++) box = box.parentElement; out.push(t(box).replace(/^AI Overview\s*/i, '').slice(0, 1500)); }
  for (const s of ['[data-tts="answers"]', '.Z0LcW', '.hgKElc', '.IZ6rdc', '.LGOjhe', '[data-attrid="wa:/description"]', '.kno-rdesc span', '[data-attrid*="description"]']){
    const v = t(document.querySelector(s)); if (v && !out.includes(v)) out.push(v);
  }
  for (const g of [...document.querySelectorAll('#rso .MjjYud, #rso .g')].slice(0, 9)){
    const h = t(g.querySelector('h3')), s = t(g.querySelector('.VwiC3b, [data-sncf], .ITZIwc'));
    if (h && s && !out.some(o => o.includes(s))) out.push(h + ': ' + s);
  }
  if (out.length < 2) out.push(t(document.querySelector('#rso')).slice(0, 3000));
  return {ready:true, text:out.join('\n')};
})()"#;

fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else if b == b' ' { "+".into() } else { format!("%{b:02X}") }).collect()
}

fn window(app: &AppHandle) -> Result<WebviewWindow, String> {
    if let Some(w) = app.get_webview_window(LABEL) {
        return Ok(w);
    }
    WebviewWindowBuilder::new(app, LABEL, WebviewUrl::External("https://www.google.com/".parse().map_err(|_| "bad address")?))
        .title("Uplink")
        .visible(false)
        .focused(false)
        .skip_taskbar(true)
        .decorations(false)
        .shadow(false)
        .incognito(true)
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

/// (closed after three quiet minutes)
fn keep_tidy(app: &AppHandle) {
    if TIDY.swap(true, SeqCst) {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(30));
        let quiet = LAST.lock().unwrap().map_or(true, |t| t.elapsed() > Duration::from_secs(180));
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

/// Google's results for a question, boiled down to what bears on it (None: Uplink is standing down, the page didn't
/// come in time, or there was nothing).
pub fn facts(app: &AppHandle, q: &str) -> Option<String> {
    if BLOCKED.lock().unwrap().is_some_and(|t| t > Instant::now()) {
        return None;
    }
    let _one = ONE.try_lock().ok()?; // (one look at a time)
    let t0 = Instant::now();
    let w = window(app).ok()?;
    let _ = w.navigate(format!("https://www.google.com/search?hl=en&num=8&q={}", enc(q.trim())).parse().ok()?);
    *LAST.lock().unwrap() = Some(Instant::now());
    keep_tidy(app);
    let js = READ.replace("WANT", &serde_json::to_string(q.trim()).unwrap_or_default());
    while t0.elapsed() < Duration::from_millis(4600) {
        std::thread::sleep(Duration::from_millis(120));
        let Some(v) = run(&w, &js) else { continue };
        if v["blocked"] == true {
            *BLOCKED.lock().unwrap() = Some(Instant::now() + Duration::from_secs(1800));
            crate::log_line("uplink: Google asked for a check; standing down for half an hour");
            return None;
        }
        if v["ready"] == true {
            let text = boil(q, v["text"].as_str().unwrap_or(""), 700);
            crate::log_line(&format!("uplink: Google's results for that in {} ms, boiled down to {} characters", t0.elapsed().as_millis(), text.len()));
            return (!text.is_empty()).then_some(text);
        }
    }
    crate::log_line("uplink: Google's page didn't come in time");
    None
}
