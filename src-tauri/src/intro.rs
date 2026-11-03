//! The first time someone opens SYLVER, it shows them around: no separate window. Once the bar and
//! notch have arrived, SYLVER's own cursor pops out of the notch, flies to each thing on the bar and
//! explains it in a white speech bubble, then asks what to call you (the tour itself lives in the
//! page, index.html). It plays once per PC and can be replayed from Settings › About.

use std::path::PathBuf;

use tauri::{AppHandle, Emitter, Manager};

fn seen_flag(app: &AppHandle) -> PathBuf {
    let d = app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir());
    let _ = std::fs::create_dir_all(&d);
    d.join("intro-seen")
}

/// Asked by the page once it has loaded: is this the first launch on this PC? (True only once.)
pub fn due(app: &AppHandle) -> bool {
    let flag = seen_flag(app);
    if flag.exists() {
        return false;
    }
    // marked the moment it's first asked for, even if the tour is skipped or SYLVER is closed mid-way
    let _ = std::fs::write(&flag, b"1");
    true
}

/// Play the tour again (Settings › About).
pub fn play(app: &AppHandle) {
    let _ = app.emit_to("main", "tour-start", ());
}

fn profile(app: &AppHandle) -> PathBuf {
    seen_flag(app).with_file_name("profile.json")
}

/// What you asked SYLVER to call you.
pub fn user_name(app: &AppHandle) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(profile(app)).ok()?).ok()?;
    v["name"].as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// The tour is over (or was skipped): remember it was seen, and the name if one was given.
pub fn finish(app: &AppHandle, name: Option<String>) {
    let _ = std::fs::write(seen_flag(app), b"1");
    let name = name.map(|n| n.trim().chars().take(40).collect::<String>()).filter(|n| !n.is_empty());
    if let Some(n) = &name {
        let _ = crate::keep(profile(app), serde_json::json!({ "name": n }).to_string());
    }
    let _ = app.emit_to("main", "intro-done", serde_json::json!({ "name": name }));
}
