//! Answers from Gemini on the web, with no key of your own: gemini.google.com in a hidden window of ULTRON's (the same
//! engine as its other windows), the question typed into it and the answer read off the page as it's written. It
//! works signed out, as Google allows. Each new conversation in ULTRON (or a long pause) starts a new chat there. If
//! Gemini can't be reached or doesn't answer, ULTRON answers as before.
//!
//! (Google's terms don't provide for automating its website, so this only runs when you choose it in Settings › Brain.)

use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

const HOME: &str = "https://gemini.google.com/app";
const LABEL: &str = "gemini";
static FRESH: AtomicBool = AtomicBool::new(false);
static LAST: Mutex<Option<Instant>> = Mutex::new(None);
static ONE: Mutex<()> = Mutex::new(());

/// Where the page stands: ready to type into, how many answers it shows, the last one's text, still writing it.
const STATE: &str = r#"(() => { const ed = document.querySelector('rich-textarea .ql-editor');
  const rs = document.querySelectorAll('model-response'), last = rs[rs.length - 1];
  const md = last && last.querySelector('.markdown'), b = last && last.querySelector('[aria-busy]');
  return {ready: !!ed, n: rs.length, text: md ? md.innerText : '',
    busy: !!document.querySelector('.send-button.stop') || !b || b.getAttribute('aria-busy') === 'true'}; })()"#;
/// The question typed into Gemini's box (as typing does, so the page sees it and its send button comes on).
const TYPE: &str = r#"(() => { const ed = document.querySelector('rich-textarea .ql-editor'); if (!ed) return 'no box';
  ed.focus(); document.execCommand('selectAll', false); document.execCommand('insertText', false, QUESTION);
  if (!ed.innerText.trim()){ ed.innerText = QUESTION; ed.dispatchEvent(new InputEvent('input', {bubbles: true})); }
  return 'ok'; })()"#;
const SEND: &str = r#"(() => { const b = document.querySelector('.send-button:not(.stop)');
  if (b && !b.disabled && b.getAttribute('aria-disabled') !== 'true'){ b.click(); return 'ok'; }
  const ed = document.querySelector('rich-textarea .ql-editor'); if (!ed) return 'no box';
  ed.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter', code: 'Enter', keyCode: 13, which: 13, bubbles: true})); return 'enter'; })()"#;
const STOP: &str = r#"(() => { const b = document.querySelector('.send-button.stop'); if (b) b.click(); return 'ok'; })()"#;

/// A new conversation in ULTRON: the next question starts a new chat in Gemini too.
pub fn forget() {
    FRESH.store(true, SeqCst);
}

/// Open the hidden window before the first question (when it's chosen, and a little after ULTRON starts).
pub fn warm(app: &AppHandle) {
    if let Err(e) = window(app) {
        crate::log_line(&format!("gemini: couldn't open its window: {e}"));
    }
}

fn window(app: &AppHandle) -> Result<WebviewWindow, String> {
    if let Some(w) = app.get_webview_window(LABEL) {
        return Ok(w);
    }
    WebviewWindowBuilder::new(app, LABEL, WebviewUrl::External(HOME.parse().map_err(|_| "bad address")?))
        .title("Gemini")
        .visible(false)
        .focused(false)
        .skip_taskbar(true)
        .decorations(false)
        .shadow(false)
        .inner_size(820.0, 760.0)
        .position(-3000.0, -3000.0)
        .additional_browser_args(crate::BROWSER_ARGS) // (the same as every other ULTRON window, or the engine refuses it)
        .build()
        .map_err(|e| e.to_string())
}

/// Run a little script in Gemini's page and give back what it returns.
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
    serde_json::from_str(&rx.recv_timeout(Duration::from_secs(5)).ok()?).ok()
}

/// The page is drawn as if it were on screen while it answers, so the answer is written out as it comes (a hidden
/// page holds it all back to the end), and put to rest again after.
fn awake(w: &WebviewWindow, on: bool) {
    let _ = w.with_webview(move |pv| unsafe {
        let _ = pv.controller().SetIsVisible(on);
    });
}

/// Ask Gemini. `on_piece` gets the answer so far each time more of it is written.
pub fn chat(app: &AppHandle, question: &str, mut on_piece: impl FnMut(&str), stop: impl Fn() -> bool) -> Result<String, String> {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let w = window(app)?;
    let stale = LAST.lock().unwrap_or_else(|e| e.into_inner()).is_some_and(|t| t.elapsed() > Duration::from_secs(20 * 60));
    if FRESH.swap(false, SeqCst) || stale {
        let _ = w.navigate(HOME.parse().map_err(|_| "bad address")?);
        std::thread::sleep(Duration::from_millis(400));
    }
    awake(&w, true);
    let r = ask(&w, question, &mut on_piece, &stop);
    awake(&w, false);
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    r
}

fn ask(w: &WebviewWindow, question: &str, on_piece: &mut impl FnMut(&str), stop: &impl Fn() -> bool) -> Result<String, String> {
    let t0 = Instant::now();
    let before = loop {
        if stop() {
            return Err("stopped".into());
        }
        if let Some(v) = run(w, STATE).filter(|v| v["ready"] == true) {
            break v["n"].as_u64().unwrap_or(0);
        }
        if t0.elapsed() > Duration::from_secs(20) {
            // (what the page shows instead, for the log: a different page, a notice, or nothing at all)
            let seen = run(w, "({url: location.href, title: document.title, text: (document.body ? document.body.innerText : '').slice(0, 240)})");
            return Err(format!("its page didn't open ({})", seen.map(|v| v.to_string()).unwrap_or_else(|| "no reply from the page".into())));
        }
        std::thread::sleep(Duration::from_millis(150));
    };
    let q = serde_json::to_string(question).unwrap_or_default();
    if run(w, &TYPE.replace("QUESTION", &q)).as_ref().and_then(Value::as_str) != Some("ok") {
        return Err("couldn't type the question".into());
    }
    std::thread::sleep(Duration::from_millis(150));
    run(w, SEND);
    let (sent, mut text, mut changed) = (Instant::now(), String::new(), Instant::now());
    loop {
        std::thread::sleep(Duration::from_millis(90));
        if stop() {
            run(w, STOP);
            return Err("stopped".into());
        }
        let Some(v) = run(w, STATE) else { continue };
        if v["n"].as_u64().unwrap_or(0) <= before {
            if sent.elapsed() > Duration::from_secs(15) {
                return Err("no answer came".into());
            }
            continue;
        }
        let t = v["text"].as_str().unwrap_or("").trim();
        if t != text {
            text = t.to_string();
            changed = Instant::now();
            if !text.is_empty() {
                on_piece(&text);
            }
        }
        // (done: the page says so; or nothing more has come for a while)
        if !text.is_empty() && (v["busy"] == false || changed.elapsed() > Duration::from_secs(8)) {
            break;
        }
        if sent.elapsed() > Duration::from_secs(120) {
            break;
        }
    }
    if text.is_empty() {
        Err("its answer was empty".into())
    } else {
        Ok(text)
    }
}
