//! ULTRON's private browser: a small borderless window laid exactly over the
//! notch's content area. Private browsing (nothing saved: no history, cookies or
//! cache on disk). A separate window instead of a view inside the notch window,
//! so it always draws on top and never steals focus from the notch page.
//! Remote pages get no access to ULTRON's commands.

use tauri::webview::{NewWindowResponse, PageLoadEvent};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, Url, WebviewUrl, WebviewWindowBuilder};

const LABEL: &str = "browser";

fn parse(url: &str) -> Option<Url> {
    Url::parse(url).ok().filter(|u| matches!(u.scheme(), "http" | "https"))
}

/// Place the browser over a rectangle given in the notch page's CSS pixels.
pub fn show(app: &AppHandle, x: f64, y: f64, w: f64, h: f64, url: Option<String>) -> Result<(), String> {
    crate::log_line(&format!("browser show {x} {y} {w} {h} {url:?}"));
    let main = app.get_webview_window("main").ok_or("no main window")?;
    let origin = main.outer_position().map_err(|e| e.to_string())?;
    let s = main.scale_factor().map_err(|e| e.to_string())?;
    let pos = PhysicalPosition::new(origin.x + (x * s).round() as i32, origin.y + (y * s).round() as i32);
    let size = PhysicalSize::new((w * s).round() as u32, (h * s).round() as u32);

    if let Some(win) = app.get_webview_window(LABEL) {
        let _ = win.set_position(pos);
        let _ = win.set_size(size);
        if let Some(u) = url.as_deref().and_then(parse) {
            let _ = win.navigate(u);
        }
        win.show().map_err(|e| e.to_string())?;
        if let Some(h) = crate::hit::hwnd_of(&win) {
            crate::hit::raise(h);
        }
        return Ok(());
    }

    let start = url.as_deref().and_then(parse).unwrap_or_else(|| Url::parse("https://www.google.com").unwrap());
    let handle = app.clone();
    let win = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::External(start))
        .title("ULTRON Browser")
        .decorations(false)
        .shadow(false) // a shadow adds an invisible frame that made it overflow its box
        .resizable(false)
        .skip_taskbar(true)
        .always_on_top(true)
        .focused(false)
        .visible(false)
        .incognito(true) // private: nothing about your browsing is kept
        .additional_browser_args(crate::BROWSER_ARGS) // (the same as every other ULTRON window, or the engine refuses it)
        .background_color(tauri::window::Color(12, 12, 14, 255))
        .owner(&main)
        .map_err(|e| e.to_string())?
        .on_page_load(|w, p| {
            let ev = if p.event() == PageLoadEvent::Finished { "browser-loaded" } else { "browser-loading" };
            let _ = w.app_handle().emit_to("main", ev, p.url().to_string());
        })
        .on_document_title_changed(|w, title| {
            let _ = w.app_handle().emit_to("main", "browser-title", title);
        })
        .on_new_window(move |url, _| {
            // links that want a new window just open right here
            if let Some(w) = handle.get_webview_window(LABEL) {
                let _ = w.navigate(url);
            }
            NewWindowResponse::Deny
        })
        .build()
        .map_err(|e| e.to_string())?;
    let _ = win.set_position(pos);
    let _ = win.set_size(size);
    if let Some(h) = crate::hit::hwnd_of(&win) {
        // rounded corners on Windows 11, without the frame
        use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
        let round: u32 = 2; // DWMWCP_ROUND
        unsafe {
            let _ = DwmSetWindowAttribute(h, DWMWINDOWATTRIBUTE(33), &round as *const u32 as _, 4);
        }
    }
    win.show().map_err(|e| e.to_string())?;
    if let Some(h) = crate::hit::hwnd_of(&win) {
        crate::hit::raise(h);
    }
    Ok(())
}

pub fn hide(app: &AppHandle) {
    crate::log_line("browser hide");
    if let Some(win) = app.get_webview_window(LABEL) {
        let _ = win.hide();
    }
}

pub fn go(app: &AppHandle, action: &str, url: Option<String>) {
    let Some(win) = app.get_webview_window(LABEL) else { return };
    match action {
        "navigate" => {
            if let Some(u) = url.as_deref().and_then(parse) {
                let _ = win.navigate(u);
            }
        }
        "back" => {
            let _ = win.eval("history.back()");
        }
        "forward" => {
            let _ = win.eval("history.forward()");
        }
        "reload" => {
            let _ = win.reload();
        }
        "zoom-fit" => {
            let _ = win.set_zoom(0.8);
        }
        "zoom-full" => {
            let _ = win.set_zoom(1.0);
        }
        "external" => {
            if let Ok(u) = win.url() {
                crate::apps::shell_open(u.as_str());
            }
        }
        _ => {}
    }
}
