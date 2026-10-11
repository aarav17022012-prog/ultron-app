//! ULTRON's own cursor: a pointer like yours, with two eyes, that flies out of the
//! notch and shows what ULTRON is doing while it works through a task. It is a tiny
//! click-through window that never takes focus; a spring moves it so every glide
//! eases in and out.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongW, SetWindowLongW, SetWindowPos, ShowWindow, GWL_EXSTYLE, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOSIZE,
    SW_HIDE, SW_SHOWNOACTIVATE, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
};

const LABEL: &str = "ghost";
const SHOW_CURSOR: bool = false;
const W: f64 = 280.0; // CSS px: the pointer plus its little status pill
const H: f64 = 90.0;
const TIP: (f64, f64) = (8.0, 6.0); // where the pointer's tip sits inside the window

#[derive(Default)]
struct Motion {
    pos: (f64, f64),
    vel: (f64, f64),
    target: (f64, f64),
}

static MOTION: Mutex<Option<Motion>> = Mutex::new(None);
static RUNNING: AtomicBool = AtomicBool::new(false);
static VISIBLE: AtomicBool = AtomicBool::new(false);
/// a new task's cursor isn't hidden by the last one flying home
static GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn window(app: &AppHandle) -> Option<WebviewWindow> {
    if let Some(w) = app.get_webview_window(LABEL) {
        return Some(w);
    }
    let w = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("ghost.html".into()))
        .title("ULTRON cursor")
        .inner_size(W, H)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .skip_taskbar(true)
        .always_on_top(true)
        .focused(false)
        .visible(false)
        .additional_browser_args(crate::BROWSER_ARGS) // (the same as every other ULTRON window, or the engine refuses it)
        .build()
        .ok()?;
    if let Some(h) = crate::hit::hwnd_of(&w) {
        unsafe {
            let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
            let bits = WS_EX_TRANSPARENT.0 | WS_EX_LAYERED.0 | WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0;
            SetWindowLongW(h, GWL_EXSTYLE, (ex | bits) as i32);
        }
    }
    let _ = w.set_ignore_cursor_events(true);
    std::thread::sleep(Duration::from_millis(350)); // let the page load before the first animation
    Some(w)
}

fn hwnd(app: &AppHandle) -> Option<HWND> {
    app.get_webview_window(LABEL).and_then(|w| crate::hit::hwnd_of(&w))
}

fn js(app: &AppHandle, code: &str) {
    if let Some(w) = app.get_webview_window(LABEL) {
        let _ = w.eval(&format!("window.G && {code}"));
    }
}

/// Where ULTRON lives (the notch), in screen pixels.
pub fn home(app: &AppHandle) -> (f64, f64) {
    let Some(main) = app.get_webview_window("main") else { return (960.0, 30.0) };
    let (Ok(p), Ok(s), Ok(sc)) = (main.outer_position(), main.outer_size(), main.scale_factor()) else { return (960.0, 30.0) };
    (p.x as f64 + s.width as f64 / 2.0 - 90.0 * sc, p.y as f64 + 16.0 * sc)   // the little character in the notch
}

fn scale(app: &AppHandle) -> f64 {
    app.get_webview_window("main").and_then(|m| m.scale_factor().ok()).unwrap_or(1.0)
}

fn place(h: HWND, x: f64, y: f64, sc: f64) {
    unsafe {
        let _ = SetWindowPos(
            h,
            Some(HWND_TOPMOST),
            (x - TIP.0 * sc).round() as i32,
            (y - TIP.1 * sc).round() as i32,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// The spring that moves the pointer (critically damped: smooth start, smooth stop, no wobble).
fn animate(app: AppHandle) {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let sc = scale(&app);
        let mut last = Instant::now();
        let mut look_at = Instant::now();
        let mut looked = (9.0, 9.0);
        while VISIBLE.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(7));
            let dt = last.elapsed().as_secs_f64().min(0.05);
            last = Instant::now();
            let Some(h) = hwnd(&app) else { break };
            let (pos, vel) = {
                let mut g = MOTION.lock().unwrap();
                let Some(m) = g.as_mut() else { break };
                let w = 23.0; // stiffness: about 0.22 s for a long glide
                for (p, v, t) in [(&mut m.pos.0, &mut m.vel.0, m.target.0), (&mut m.pos.1, &mut m.vel.1, m.target.1)] {
                    let a = w * w * (t - *p) - 2.0 * w * *v;
                    *v += a * dt;
                    *p += *v * dt;
                }
                (m.pos, m.vel)
            };
            place(h, pos.0, pos.1, sc);
            // the eyes look where it's heading
            if look_at.elapsed() > Duration::from_millis(60) {
                look_at = Instant::now();
                let sp = (vel.0 * vel.0 + vel.1 * vel.1).sqrt();
                let dir = if sp > 60.0 { (vel.0 / sp, vel.1 / sp) } else { (0.0, 0.0) };
                if (dir.0 - looked.0).abs() + (dir.1 - looked.1).abs() > 0.25 {
                    looked = dir;
                    js(&app, &format!("G.look({:.2},{:.2})", dir.0, dir.1));
                }
            }
        }
        RUNNING.store(false, Ordering::SeqCst);
    });
}

/// Fly out of the notch.
pub fn show(app: &AppHandle) {
    // Retired: people found a second pointer on screen confusing. The notch shows each step instead,
    // and the character's face shows what ULTRON is doing. (Everything below still works if it's brought back.)
    if !SHOW_CURSOR || !crate::ai::config(app).ghost {
        return;
    }
    let Some(w) = window(app) else { return };
    let Some(h) = crate::hit::hwnd_of(&w) else { return };
    GEN.fetch_add(1, Ordering::SeqCst);
    let start = home(app);
    *MOTION.lock().unwrap() = Some(Motion { pos: start, vel: (0.0, 0.0), target: start });
    place(h, start.0, start.1, scale(app));
    unsafe {
        let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
    }
    VISIBLE.store(true, Ordering::SeqCst);
    animate(app.clone());
    js(app, "G.appear()");
}

/// Glide toward a point on screen (doesn't wait).
pub fn go(x: f64, y: f64) {
    if let Some(m) = MOTION.lock().unwrap().as_mut() {
        m.target = (x, y);
    }
}

/// Glide to a point and wait until it gets there.
pub fn go_wait(x: f64, y: f64) {
    go(x, y);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(420) {
        let near = MOTION.lock().unwrap().as_ref().map(|m| (m.pos.0 - x).abs() + (m.pos.1 - y).abs() < 3.0).unwrap_or(true);
        if near {
            break;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

pub fn say(app: &AppHandle, text: &str) {
    js(app, &format!("G.say({})", serde_json::to_string(text).unwrap_or_default()));
}

/// "think" | "type" | "idle"
pub fn mood(app: &AppHandle, m: &str) {
    js(app, &format!("G.mood('{m}')"));
    let _ = app.emit_to("main", "pet-mood", m); // the notch's character shows it too
}

pub fn progress(app: &AppHandle, f: f64) {
    js(app, &format!("G.progress({:.3})", f.clamp(0.0, 1.0)));
}

pub fn tap(app: &AppHandle) {
    js(app, "G.tap()");
}

pub fn click(app: &AppHandle) {
    js(app, "G.click()");
}

/// Stay above other always-on-top windows while it works.
pub fn raise(app: &AppHandle) {
    if let Some(h) = hwnd(app) {
        crate::hit::raise(h);
    }
}

/// Smile, fly home into the notch, and vanish.
pub fn finish(app: &AppHandle, ok: bool) {
    if !VISIBLE.load(Ordering::SeqCst) {
        return;
    }
    js(app, if ok { "G.done()" } else { "G.oops()" });
    // fly home in the background: the answer doesn't wait for the animation
    let app = app.clone();
    let gen = GEN.load(Ordering::SeqCst);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(450));
        if GEN.load(Ordering::SeqCst) != gen {
            return;
        }
        let h0 = home(&app);
        go_wait(h0.0, h0.1);
        js(&app, "G.vanish()");
        std::thread::sleep(Duration::from_millis(280));
        if GEN.load(Ordering::SeqCst) != gen {
            return;
        }
        VISIBLE.store(false, Ordering::SeqCst);
        if let Some(h) = hwnd(&app) {
            unsafe {
                let _ = ShowWindow(h, SW_HIDE);
            }
        }
    });
}
