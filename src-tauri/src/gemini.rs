//! Answers from Gemini on the web, with no key of your own: gemini.google.com in a hidden window of ULTRON's (the same
//! engine as its other windows), the question typed into it and the answer read off the page as it's written. It
//! works signed out, as Google allows. Each new conversation in ULTRON (or a long pause) starts a new chat there. If
//! Gemini can't be reached or doesn't answer, ULTRON answers as before.
//!
//! Each PC's ULTRON has a Gemini of its own: its own hidden window, private (nothing kept between runs, nothing shared
//! with any other window or account), over that PC's own connection. Nothing goes through anyone else's server.
//!
//! (Google's terms don't provide for automating its website, so this only runs when Cloud AI is chosen in Settings › Brain.)

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
/// Until when the page stays drawn and ready (a page put to rest first had to wake before it even sent the question).
static AWAKE_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);
static SLEEPER: AtomicBool = AtomicBool::new(false);
/// Questions asked in the chat that's open there (a long chat answers more slowly: a new one every few questions).
static TURNS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Where the page stands: ready to type into, how many answers it shows, the last one's text, still writing it.
const STATE: &str = r#"(() => { const ed = document.querySelector('rich-textarea .ql-editor');
  const rs = document.querySelectorAll('model-response'), last = rs[rs.length - 1];
  const md = last && last.querySelector('.markdown'), b = last && last.querySelector('[aria-busy]');
  return {ready: !!ed, n: rs.length, text: md ? md.innerText : '', typed: !!(ed && ed.innerText.trim()),
    busy: !!document.querySelector('.send-button.stop') || !b || b.getAttribute('aria-busy') === 'true'}; })()"#;
/// The question typed into Gemini's box (as typing does, so the page sees it and its send button comes on).
const TYPE: &str = r#"(() => { const ed = document.querySelector('rich-textarea .ql-editor'); if (!ed) return 'no box';
  ed.focus(); document.execCommand('selectAll', false); document.execCommand('insertText', false, QUESTION);
  if (!ed.innerText.trim()){ ed.innerText = QUESTION; ed.dispatchEvent(new InputEvent('input', {bubbles: true})); }
  // (sent as soon as the page has seen it: its send button comes on a moment after the words go in)
  let n = 0; const go = () => { const b = document.querySelector('.send-button:not(.stop)');
    if (b && !b.disabled && b.getAttribute('aria-disabled') !== 'true'){ performance.clearResourceTimings(); window.__sentAt = performance.now(); b.click(); } else if (++n < 40) requestAnimationFrame(go);
    else ed.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter', code: 'Enter', keyCode: 13, which: 13, bubbles: true})); };
  requestAnimationFrame(go);
  return 'ok'; })()"#;
/// When the page sent the question on (its own request to Google), and when the answer began and ended, after the click.
const TIMES: &str = r#"(() => { const e = performance.getEntriesByType('resource').filter(x => x.name.includes('StreamGenerate')).pop(), s = window.__sentAt || 0;
  return e ? [Math.round(e.startTime - s), Math.round(e.responseStart - s), Math.round(e.responseEnd - s)] : null; })()"#;
/// Its chat code loaded ahead of the first question: a character typed into its box and taken out again.
const PRIME: &str = r#"(() => { const ed = document.querySelector('rich-textarea .ql-editor'); if (!ed) return 'no box';
  ed.focus(); document.execCommand('insertText', false, '.'); setTimeout(() => { document.execCommand('selectAll', false); document.execCommand('delete', false); }, 400); return 'ok'; })()"#;
/// Sending, when the question is still sitting in the box (the page was slow to see it).
const SEND: &str = r#"(() => { const b = document.querySelector('.send-button:not(.stop)');
  if (b && !b.disabled && b.getAttribute('aria-disabled') !== 'true'){ b.click(); return 'ok'; }
  const ed = document.querySelector('rich-textarea .ql-editor'); if (!ed) return 'no box';
  ed.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter', code: 'Enter', keyCode: 13, which: 13, bubbles: true})); return 'enter'; })()"#;
const STOP: &str = r#"(() => { const b = document.querySelector('.send-button.stop'); if (b) b.click(); return 'ok'; })()"#;

/// A new conversation in ULTRON: the next question starts a new chat in Gemini too.
pub fn forget() {
    FRESH.store(true, SeqCst);
}

/// Open the hidden window before the first question (when it's chosen, and a little after ULTRON starts), and once
/// its page is up, load its chat code (it's loaded on first use otherwise, which held the first question up).
pub fn warm(app: &AppHandle) {
    let fresh = app.get_webview_window(LABEL).is_none();
    match window(app) {
        Err(e) => crate::log_line(&format!("gemini: couldn't open its window: {e}")),
        Ok(w) if fresh => {
            awake(&w, true);
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(25) {
                if run(&w, STATE).is_some_and(|v| v["ready"] == true) {
                    run(&w, PRIME);
                    std::thread::sleep(Duration::from_millis(900));
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            keep_awake(app, 20);
        }
        Ok(_) => {}
    }
}

/// You've opened the notch or started typing to ULTRON: the page is woken now, so it's drawing and ready by the time
/// a question comes, and kept so a while.
pub fn wake(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(LABEL) {
        awake(&w, true);
        keep_awake(app, 120);
    }
}

/// The page stays awake for `secs` more, then is put to rest (by one small watcher).
fn keep_awake(app: &AppHandle, secs: u64) {
    {
        let mut u = AWAKE_UNTIL.lock().unwrap_or_else(|e| e.into_inner());
        let t = Instant::now() + Duration::from_secs(secs);
        if u.is_none_or(|old| old < t) {
            *u = Some(t);
        }
    }
    if SLEEPER.swap(true, SeqCst) {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(5));
        let due = AWAKE_UNTIL.lock().unwrap_or_else(|e| e.into_inner()).is_none_or(|t| Instant::now() >= t);
        if due && ONE.try_lock().is_ok() {
            if let Some(w) = app.get_webview_window(LABEL) {
                awake(&w, false);
            }
            *AWAKE_UNTIL.lock().unwrap_or_else(|e| e.into_inner()) = None;
            SLEEPER.store(false, SeqCst);
            return;
        }
    });
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
        .incognito(true)
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
    use webview2_com::Microsoft::Web::WebView2::Win32::{ICoreWebView2_19, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL};
    use wcore62::Interface;
    // (the window itself shown off the screen, where nothing of it is seen: a hidden window's page writes nothing out
    //  until the answer is done, so the first words came no sooner than the last)
    if let Some(h) = crate::hit::hwnd_of(w) {
        use windows::Win32::Foundation::COLORREF;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetWindowLongW, SetLayeredWindowAttributes, SetWindowLongW, SetWindowPos, ShowWindow, GWL_EXSTYLE, HWND_BOTTOM, LWA_ALPHA, SWP_NOACTIVATE, SWP_NOSIZE,
            SW_HIDE, SW_SHOWNOACTIVATE, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
        };
        unsafe {
            let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
            let want = ex | WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0 | WS_EX_TRANSPARENT.0 | WS_EX_LAYERED.0;
            if ex != want {
                SetWindowLongW(h, GWL_EXSTYLE, want as i32);
            }
            // (and not to be seen even if it's on the screen: Windows had moved it from where it was put, off the screen,
            //  to the middle of it, and it showed over your apps each time you asked ULTRON something. Put back each
            //  time, under every other window, and all but fully see-through)
            let _ = SetLayeredWindowAttributes(h, COLORREF(0), 1, LWA_ALPHA);
            let _ = SetWindowPos(h, Some(HWND_BOTTOM), -3000, -3000, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
            let _ = ShowWindow(h, if on { SW_SHOWNOACTIVATE } else { SW_HIDE });
        }
    }
    let _ = w.with_webview(move |pv| unsafe {
        let c = pv.controller();
        let _ = c.SetIsVisible(on);
        // (and its spare memory handed back between questions)
        if let Ok(c19) = c.CoreWebView2().and_then(|w| w.cast::<ICoreWebView2_19>()) {
            let _ = c19.SetMemoryUsageTargetLevel(if on { COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL } else { COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW });
        }
    });
}

/// Ask Gemini. `on_piece` gets the answer so far each time more of it is written.
pub fn chat(app: &AppHandle, question: &str, mut on_piece: impl FnMut(&str), stop: impl Fn() -> bool) -> Result<String, String> {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let w = window(app)?;
    let stale = LAST.lock().unwrap_or_else(|e| e.into_inner()).is_some_and(|t| t.elapsed() > Duration::from_secs(20 * 60));
    if FRESH.swap(false, SeqCst) || stale || TURNS.load(SeqCst) >= 8 {
        TURNS.store(0, SeqCst);
        let _ = w.navigate(HOME.parse().map_err(|_| "bad address")?);
        std::thread::sleep(Duration::from_millis(400));
    }
    awake(&w, true);
    TURNS.fetch_add(1, SeqCst);
    let t0 = Instant::now();
    let mut r = ask(&w, question, &mut on_piece, &stop);
    // (the question never left the box: the chat had got stuck. Once more, in a new one)
    if r.as_ref().is_err_and(|e| e == "not sent") && !stop() {
        crate::log_line("uplink: the question wasn't taken; asking again in a new chat");
        FRESH.store(false, SeqCst);
        let _ = w.navigate(HOME.parse().map_err(|_| "bad address")?);
        std::thread::sleep(Duration::from_millis(400));
        r = ask(&w, question, &mut on_piece, &stop);
    }
    if let Some(t) = run(&w, TIMES).and_then(|v| v.as_array().cloned()) {
        crate::log_line(&format!("uplink: sent on after {} ms, answer began at {} ms, ended at {} ms (all after the click); {} ms in all", t[0], t[1], t[2], t0.elapsed().as_millis()));
    }
    keep_awake(app, 120);
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
        if t0.elapsed() > Duration::from_secs(8) {
            FRESH.store(true, SeqCst);
            // (what the page shows instead, for the log: a different page, a notice, or nothing at all)
            let seen = run(w, "({url: location.href, title: document.title, text: (document.body ? document.body.innerText : '').slice(0, 240)})");
            return Err(format!("its page didn't open ({})", seen.map(|v| v.to_string()).unwrap_or_else(|| "no reply from the page".into())));
        }
        std::thread::sleep(Duration::from_millis(80));
    };
    // (an answer still being written in it (one stopped on ULTRON's side): stopped there too, so the box takes the question)
    if run(w, STATE).is_some_and(|v| v["busy"] == true && v["n"].as_u64().unwrap_or(0) > 0) {
        run(w, STOP);
        let t1 = Instant::now();
        while t1.elapsed() < Duration::from_millis(1200) && run(w, STATE).is_some_and(|v| v["busy"] == true) {
            std::thread::sleep(Duration::from_millis(60));
        }
    }
    let q = serde_json::to_string(question).unwrap_or_default();
    if run(w, &TYPE.replace("QUESTION", &q)).as_ref().and_then(Value::as_str) != Some("ok") {
        return Err("couldn't type the question".into());
    }
    let (sent, mut text, mut changed, mut nudged) = (Instant::now(), String::new(), Instant::now(), 0u32);
    loop {
        std::thread::sleep(Duration::from_millis(60));
        if stop() {
            run(w, STOP);
            return Err("stopped".into());
        }
        let Some(v) = run(w, STATE) else { continue };
        if v["n"].as_u64().unwrap_or(0) <= before {
            // (still in the box after a moment: it wasn't sent; send it again, a few times at most)
            if v["typed"] == true && sent.elapsed() > Duration::from_millis(350 + 400 * nudged as u64) && nudged < 4 {
                nudged += 1;
                run(w, SEND);
            }
            if sent.elapsed() > Duration::from_secs(6) {
                FRESH.store(true, SeqCst);
                return Err(if v["typed"] == true { "not sent".into() } else { "no answer came".into() });
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
