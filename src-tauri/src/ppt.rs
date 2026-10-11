//! "Make me a presentation about black holes": facts are looked up, the local AI
//! writes the slides, and PowerPoint builds a designed deck live on screen
//! (Morph transitions, fading titles, points appearing one by one).

use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};

use serde::Serialize;
use tauri::{AppHandle, Manager};

const SCRIPT: &str = include_str!("ppt.ps1");
const NO_WINDOW: u32 = 0x0800_0000;

#[derive(Serialize, Default)]
struct Slide {
    title: String,
    bullets: Vec<String>,
    note: String,
}

#[derive(Serialize, Default)]
struct Deck {
    title: String,
    subtitle: String,
    theme: u32,
    slides: Vec<Slide>,
}

fn clean(s: &str) -> String {
    s.replace("**", "").replace('#', "").trim().trim_matches(|c| matches!(c, '"' | '“' | '”' | '*' | '_')).trim().to_string()
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let up = line.to_ascii_uppercase();
    if !up.starts_with(key) {
        return None;
    }
    // "SLIDE:", "SLIDE 3:", "Slide 3 -"
    let rest = &line[key.len()..];
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_digit() || c == ' ');
    rest.strip_prefix(':').or_else(|| rest.strip_prefix('-')).or_else(|| rest.strip_prefix('.')).map(str::trim)
}

fn parse(text: &str, topic: &str) -> Deck {
    let mut d = Deck::default();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        if let Some(v) = field(l, "SUBTITLE") {
            d.subtitle = clean(v);
        } else if let Some(v) = field(l, "TITLE") {
            if d.title.is_empty() {
                d.title = clean(v);
            }
        } else if let Some(v) = field(l, "SLIDE") {
            d.slides.push(Slide { title: clean(v), ..Default::default() });
        } else if let Some(v) = field(l, "NOTE") {
            if let Some(s) = d.slides.last_mut() {
                s.note = clean(v);
            }
        } else if let Some(s) = d.slides.last_mut() {
            let b = l.trim_start_matches(['-', '•', '*', '–']).trim_start_matches(|c: char| c.is_ascii_digit()).trim_start_matches(['.', ')']).trim();
            if !b.is_empty() && b.len() != l.len() && s.bullets.len() < 5 {
                s.bullets.push(clean(b));
            }
        }
    }
    d.slides.retain(|s| !s.title.is_empty() && !s.bullets.is_empty());
    if d.title.is_empty() {
        let mut c = topic.chars();
        d.title = c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default();
    }
    if d.subtitle.is_empty() {
        d.subtitle = "An overview".into();
    }
    d.theme = topic.bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32)) % 5;
    d
}

fn safe_name(s: &str) -> String {
    let n: String = s.chars().map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' { c } else { ' ' }).collect();
    let n = n.split_whitespace().collect::<Vec<_>>().join(" ");
    if n.is_empty() { "Presentation".into() } else { n.chars().take(60).collect() }
}

/// Build the deck live: PowerPoint opens while the brain is still writing, and each slide is
/// designed the moment its text is complete. `status` hears what's happening.
pub fn build(app: &AppHandle, topic: &str, n: usize, mut status: impl FnMut(&str, f64)) -> Result<String, String> {
    let tmp = std::env::temp_dir();
    let script = tmp.join("ultron-deck.ps1");
    // Windows PowerShell reads scripts without a BOM as ANSI
    let mut f = std::fs::File::create(&script).map_err(|e| e.to_string())?;
    f.write_all(b"\xEF\xBB\xBF").and_then(|_| f.write_all(SCRIPT.trim_start_matches('\u{feff}').as_bytes())).map_err(|e| e.to_string())?;
    drop(f);
    let dir = app.path().document_dir().unwrap_or_else(|_| std::env::temp_dir()).join("ULTRON Presentations");
    let _ = std::fs::create_dir_all(&dir);

    // PowerPoint starts now, while the facts are looked up and the slides are written
    status("Opening PowerPoint", 0.04);
    let mut child = Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .arg("-Out")
        .arg(dir.join("Presentation.pptx"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(NO_WINDOW)
        .spawn()
        .map_err(|_| "Couldn't start PowerPoint.".to_string())?;
    let mut stdin = child.stdin.take().ok_or("Couldn't talk to PowerPoint.")?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    if let Some(so) = child.stdout.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(so).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
    }
    let mut send = |v: serde_json::Value| writeln!(stdin, "{v}").and_then(|_| stdin.flush()).is_ok();

    status("Looking up the topic", 0.08);
    let facts = crate::web::wiki(topic);
    status("Writing the slides", 0.12);
    let (mut sent_title, mut sent) = (false, 0usize);
    let text = crate::ai::outline(app, topic, n, facts.as_deref(), |full| {
        if crate::ai::cancelled() {
            return;
        }
        // every slide before the one being written is finished: design it now
        let done_part = full.rfind("SLIDE").map(|i| &full[..i]).unwrap_or("");
        let d = parse(done_part, topic);
        if !sent_title && full.contains("SLIDE") {
            sent_title = send(serde_json::json!({ "k": "title", "title": d.title, "subtitle": d.subtitle, "theme": d.theme, "n": n }));
        }
        while sent_title && sent < d.slides.len() {
            let sl = &d.slides[sent];
            send(serde_json::json!({ "k": "slide", "title": sl.title, "bullets": sl.bullets, "note": sl.note }));
            sent += 1;
        }
        let writing = full.matches("SLIDE").count().clamp(1, n);
        status(&format!("Writing slide {writing} of {n}"), 0.12 + 0.75 * writing as f64 / n as f64);
    });
    if crate::ai::cancelled() {
        let _ = child.kill();
        return Err("Stopped.".into());
    }
    let Some(text) = text else {
        let _ = child.kill();
        return Err("My local brain isn't available to write it right now.".into());
    };
    let deck = parse(&text, topic);
    crate::log_line(&format!("deck “{}”: {} slides", deck.title, deck.slides.len()));
    if deck.slides.is_empty() {
        let _ = child.kill();
        return Err("I couldn't come up with good slides for that. Try saying the topic a little differently.".into());
    }
    if !sent_title {
        send(serde_json::json!({ "k": "title", "title": deck.title, "subtitle": deck.subtitle, "theme": deck.theme, "n": deck.slides.len() }));
    }
    for sl in deck.slides.iter().skip(sent) {
        send(serde_json::json!({ "k": "slide", "title": sl.title, "bullets": sl.bullets, "note": sl.note }));
    }
    let base = safe_name(&deck.title);
    let mut out = dir.join(format!("{base}.pptx"));
    let mut k = 2;
    while out.exists() {
        out = dir.join(format!("{base} ({k}).pptx"));
        k += 1;
    }
    send(serde_json::json!({ "k": "end", "out": out.to_string_lossy() }));
    status("Finishing the design", 0.9);
    // PowerPoint finishes the last slides and saves
    let t0 = std::time::Instant::now();
    let mut done = false;
    while t0.elapsed() < std::time::Duration::from_secs(90) {
        match rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(line) if line.starts_with("DONE") => {
                done = true;
                break;
            }
            Ok(line) => {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.first() == Some(&"SLIDE") && parts.len() == 3 {
                    let (i, t): (f64, f64) = (parts[1].parse().unwrap_or(1.0), parts[2].parse().unwrap_or(1.0));
                    status(&format!("Designing slide {} of {}", i as usize, t as usize), 0.9 + 0.09 * i / t);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(_) => {}
        }
        if crate::ai::cancelled() {
            let _ = child.kill();
            return Err("Stopped.".into());
        }
    }
    let _ = child.wait();
    if !done {
        return Err("PowerPoint didn't finish the deck.".into());
    }
    Ok(out.to_string_lossy().to_string())
}
