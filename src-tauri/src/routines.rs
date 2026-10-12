//! Routines: things ULTRON does by itself at set times. "Every weekday at 9, open my work apps and
//! read me the news." "Every night at 10 remind me to charge my phone." "When you start, open Spotify."
//! A routine is a schedule plus an ordinary request: when its time comes, the request goes through
//! ULTRON exactly as if you'd just said it, and the result is shown and spoken.
//! If the PC was off or asleep at the time, it runs when you're back (up to two hours late), once.
//! They live in routines.json on this PC; say "list my routines" or open Settings › Tasks to manage them.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Routine {
    pub id: u64,
    pub name: String,
    /// what to do, as you'd say it
    pub task: String,
    pub hour: u32,
    pub minute: u32,
    /// 0 = Sunday ... 6 = Saturday; empty = every day
    pub days: Vec<u32>,
    /// runs when ULTRON starts instead of at a time
    #[serde(default)]
    pub at_start: bool,
    pub on: bool,
    /// the day it last ran ("2026-10-10"), so it runs once a day
    #[serde(default)]
    pub last: String,
}

static CACHE: Mutex<Option<Vec<Routine>>> = Mutex::new(None);

fn path(app: &AppHandle) -> PathBuf {
    let d = app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir());
    let _ = std::fs::create_dir_all(&d);
    d.join("routines.json")
}

fn with<T>(app: &AppHandle, change: bool, f: impl FnOnce(&mut Vec<Routine>) -> T) -> T {
    let mut g = CACHE.lock().unwrap();
    let list = g.get_or_insert_with(|| std::fs::read_to_string(path(app)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default());
    let out = f(list);
    if change {
        let _ = crate::keep(path(app), serde_json::to_string_pretty(list).unwrap_or_default());
    }
    out
}

pub fn list(app: &AppHandle) -> Vec<Routine> {
    with(app, false, |l| l.clone())
}

pub fn set_on(app: &AppHandle, id: u64, on: bool) {
    with(app, true, |l| l.iter_mut().filter(|r| r.id == id).for_each(|r| r.on = on));
}

pub fn delete(app: &AppHandle, id: u64) {
    with(app, true, |l| l.retain(|r| r.id != id));
}

const DAY_NAMES: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

/// "every weekday at 9:00 AM", "Mondays and Fridays at 6:30 PM", "when ULTRON starts"
pub fn when(r: &Routine) -> String {
    if r.at_start {
        return "when ULTRON starts".into();
    }
    let (h12, ap) = (if r.hour % 12 == 0 { 12 } else { r.hour % 12 }, if r.hour >= 12 { "PM" } else { "AM" });
    let time = format!("{h12}:{:02} {ap}", r.minute);
    let mut d = r.days.clone();
    d.sort();
    d.dedup();
    let days = match d.as_slice() {
        [] | [0, 1, 2, 3, 4, 5, 6] => "every day".to_string(),
        [1, 2, 3, 4, 5] => "every weekday".to_string(),
        [0, 6] => "every weekend".to_string(),
        some => {
            let names: Vec<String> = some.iter().map(|i| format!("{}s", DAY_NAMES[*i as usize % 7])).collect();
            match names.len() {
                1 => names[0].clone(),
                n => format!("{} and {}", names[..n - 1].join(", "), names[n - 1]),
            }
        }
    };
    format!("{days} at {time}")
}

/// A clock time in words: "9", "9am", "9:30 pm", "21:00", "noon", "half past 8" isn't handled.
fn clock(s: &str) -> Option<(u32, u32, usize)> {
    let t = s.trim_start();
    let skipped = s.len() - t.len();
    for (w, h) in [("noon", 12), ("midday", 12), ("midnight", 0)] {
        if t.starts_with(w) {
            return Some((h, 0, skipped + w.len()));
        }
    }
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    let mut used = digits.len();
    let mut hour: u32 = digits.parse().ok()?;
    let mut minute = 0;
    let rest = &t[used..];
    if let Some(m) = rest.strip_prefix(':').or_else(|| rest.strip_prefix('.')) {
        let md: String = m.chars().take_while(|c| c.is_ascii_digit()).collect();
        if md.len() == 2 {
            minute = md.parse().ok()?;
            used += 3;
        }
    }
    let rest = t[used..].trim_start();
    let gap = t[used..].len() - rest.len();
    let ap = ["a.m.", "p.m.", "am", "pm", "a m", "p m"].iter().find(|x| rest.starts_with(**x) && !rest[x.len()..].starts_with(|c: char| c.is_alphabetic()));
    if let Some(ap) = ap {
        used += gap + ap.len();
        if ap.starts_with('p') && hour < 12 {
            hour += 12;
        }
        if ap.starts_with('a') && hour == 12 {
            hour = 0;
        }
    }
    (hour < 24 && minute < 60).then_some((hour, minute, skipped + used))
}

/// Split "every weekday at 9 am, open my work apps" into its schedule and its task.
pub fn parse(text: &str) -> Option<Routine> {
    let raw = text.trim().trim_end_matches(['.', '!']).trim();
    let l = raw.to_lowercase();
    let l = l.strip_prefix("ultron").map(|r| r.trim_start_matches([',', ' '])).unwrap_or(&l).to_string();
    let raw = if raw.len() >= l.len() && raw.is_char_boundary(raw.len() - l.len()) { &raw[raw.len() - l.len()..] } else { raw };
    // when ULTRON (or the PC) starts
    for p in ["when you start", "whenever you start", "when ultron starts", "every time you start", "every time ultron starts", "when i start my pc", "when my pc starts", "when i turn on my pc",
        "when the pc starts", "at startup", "on startup", "when windows starts", "every time i start my pc", "when i log in", "when i sign in"] {
        if let Some(i) = l.find(p) {
            let task = format!("{} {}", &raw[..i], &raw[i + p.len()..]);
            let task = tidy(&task);
            return (task.split_whitespace().count() >= 2).then(|| Routine { id: 0, name: name_for(&task), task, hour: 0, minute: 0, days: vec![], at_start: true, on: true, last: String::new() });
        }
    }
    // "every ... " / "each ..." / "daily" / "on weekdays"
    let starts = ["every ", "each ", "daily ", "on weekdays", "on weekends", "weekdays ", "weekends "];
    let (at, lead) = starts.iter().filter_map(|p| l.find(p).filter(|i| *i == 0 || l[..*i].ends_with(' ')).map(|i| (i, *p))).min_by_key(|(i, _)| *i)?;
    let before = &raw[..at];
    let sched = &l[at..];
    let mut days: Vec<u32> = Vec::new();
    let mut default_time = None;
    let mut cut = if lead.starts_with("every") || lead.starts_with("each") { lead.len() } else { 0 };
    // the day words that follow, in any combination: "monday and friday", "weekday", "day", "morning"
    loop {
        let rest = sched[cut..].trim_start_matches([' ', ',']);
        let skipped = sched[cut..].len() - rest.len();
        let rest2 = rest.strip_prefix("and ").or_else(|| rest.strip_prefix("on ")).unwrap_or(rest);
        let skipped = skipped + (rest.len() - rest2.len());
        let word: String = rest2.chars().take_while(|c| c.is_alphabetic()).collect();
        let mut hit = true;
        match word.trim_end_matches('s') {
            "day" | "daily" => {}
            "weekday" => days.extend([1, 2, 3, 4, 5]),
            "weekend" => days.extend([0, 6]),
            "morning" => default_time = Some((8, 0)),
            "afternoon" => default_time = Some((15, 0)),
            "evening" => default_time = Some((18, 0)),
            "night" => default_time = Some((21, 0)),
            w => match DAY_NAMES.iter().position(|n| { let n = n.to_lowercase(); n == w || (w.len() == 3 && n.starts_with(w)) }) {
                Some(i) => days.push(i as u32),
                None => hit = false,
            },
        }
        if !hit || word.is_empty() {
            break;
        }
        cut += skipped + word.len();
    }
    // the time: "at 9", "at 9:30 pm", "@ 7"
    let rest = &sched[cut..];
    let trimmed = rest.trim_start_matches([' ', ',']);
    let lead_ws = rest.len() - trimmed.len();
    let (hour, minute, time_len) = match trimmed.strip_prefix("at ").or_else(|| trimmed.strip_prefix("@")).or_else(|| trimmed.strip_prefix("around ")) {
        Some(t) => {
            let (h, m, used) = clock(t)?;
            (h, m, lead_ws + (trimmed.len() - t.len()) + used)
        }
        None => match clock(trimmed).filter(|(_, _, used)| trimmed[*used..].trim_start().starts_with(|c: char| !c.is_ascii_digit()) && (trimmed[..*used].contains("am") || trimmed[..*used].contains("pm") || trimmed[..*used].contains(':'))) {
            Some((h, m, used)) => (h, m, lead_ws + used),
            None => {
                let (h, m) = default_time?; // "every morning" with no time
                (h, m, 0)
            }
        },
    };
    // a bare "at 6" in the evening/night means PM
    let hour = if matches!(default_time, Some((18, _)) | Some((21, _)) | Some((15, _))) && hour < 12 && hour != 0 { hour + 12 } else { hour };
    let after = &raw[at + cut + time_len..];
    let task = tidy(&format!("{before} {after}"));
    // "set an alarm for every day at 7" leaves "set an alarm for": that's not a task by itself
    let dangling = ["for", "at", "to", "on", "in", "of", "and", "me"].iter().any(|w| task.to_lowercase().ends_with(&format!(" {w}")));
    if task.split_whitespace().count() < 2 || dangling {
        return None;
    }
    days.sort();
    days.dedup();
    Some(Routine { id: 0, name: name_for(&task), task, hour, minute, days, at_start: false, on: true, last: String::new() })
}

fn tidy(task: &str) -> String {
    let t = task.split_whitespace().collect::<Vec<_>>().join(" ");
    let t = t.trim_matches([',', ' ', '.', ':', ';']);
    let low = t.to_lowercase();
    let t = ["please ", "i want you to ", "you should ", "can you ", "could you ", "to "].iter().find(|p| low.starts_with(**p)).map(|p| &t[p.len()..]).unwrap_or(t);
    t.trim_matches([',', ' ']).to_string()
}

fn name_for(task: &str) -> String {
    let words: Vec<&str> = task.split_whitespace().take(5).collect();
    let mut n = words.join(" ");
    if task.split_whitespace().count() > 5 {
        n.push('…');
    }
    let mut c = n.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// Making and managing routines by voice or typing. None = this isn't about routines.
pub fn command(app: &AppHandle, text: &str) -> Option<String> {
    let l = text.to_lowercase();
    let l = l.trim().trim_end_matches(['?', '.', '!']).trim();
    let l = l.strip_prefix("ultron").map(|r| r.trim_start_matches([',', ' '])).unwrap_or(l);
    let about = l.contains("routine");
    if about && ["list", "show", "what", "which", "do i have", "tell me"].iter().any(|p| l.contains(p)) && !l.contains("every") {
        let all = list(app);
        if all.is_empty() {
            return Some("You don't have any routines yet. Try “every weekday at 9, open my work apps”.".into());
        }
        let lines: Vec<String> = all.iter().enumerate().map(|(i, r)| format!("{}: {}, {}{}", i + 1, r.task, when(r), if r.on { "" } else { " (paused)" })).collect();
        return Some(format!("You have {}. {}.", if all.len() == 1 { "one routine".to_string() } else { format!("{} routines", all.len()) }, lines.join(". ")));
    }
    let pick = |app: &AppHandle| -> Option<Routine> {
        let all = list(app);
        // by number ("routine 2"), or by words from its task
        if let Some(n) = l.split_whitespace().find_map(|w| w.parse::<usize>().ok()) {
            return all.get(n.checked_sub(1)?).cloned();
        }
        let skip = ["delete", "remove", "cancel", "stop", "pause", "resume", "run", "start", "the", "my", "routine", "routines", "now", "please", "turn", "off", "on", "do"];
        let words: Vec<&str> = l.split_whitespace().filter(|w| !skip.contains(w) && w.len() > 2).collect();
        if words.is_empty() {
            return (all.len() == 1).then(|| all[0].clone());
        }
        all.iter().max_by_key(|r| words.iter().filter(|w| r.task.to_lowercase().contains(**w)).count()).filter(|r| words.iter().any(|w| r.task.to_lowercase().contains(*w))).cloned()
    };
    if about && ["delete all", "remove all", "cancel all", "clear all", "clear my routines", "delete my routines"].iter().any(|p| l.contains(p)) {
        with(app, true, |x| x.clear());
        return Some("Done. All your routines are gone.".into());
    }
    if about && ["delete", "remove", "cancel"].iter().any(|p| l.starts_with(p)) {
        return Some(match pick(app) {
            Some(r) => {
                delete(app, r.id);
                format!("Deleted the routine “{}”.", r.task)
            }
            None => "I couldn't tell which routine you mean. Say “list my routines” to hear them.".into(),
        });
    }
    if about && ["pause", "stop", "turn off"].iter().any(|p| l.starts_with(p)) {
        return Some(match pick(app) {
            Some(r) => {
                set_on(app, r.id, false);
                format!("Paused “{}”.", r.task)
            }
            None => "I couldn't tell which routine you mean.".into(),
        });
    }
    if about && ["resume", "turn on", "unpause"].iter().any(|p| l.starts_with(p)) {
        return Some(match pick(app) {
            Some(r) => {
                set_on(app, r.id, true);
                format!("“{}” is back on.", r.task)
            }
            None => "I couldn't tell which routine you mean.".into(),
        });
    }
    if about && ["run", "start", "do"].iter().any(|p| l.starts_with(p)) {
        return Some(match pick(app) {
            Some(r) => {
                fire(app.clone(), r.clone());
                format!("Running “{}” now.", r.task)
            }
            None => "I couldn't tell which routine you mean.".into(),
        });
    }
    // a new one
    let mut r = parse(text)?;
    // one-off reminders with a time ("remind me at 5 to call mom") aren't routines: they have no "every"
    r.id = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(1);
    // said back from ULTRON's side: "tell me a joke" becomes "I'll tell you a joke"
    let mine: Vec<&str> = r.task.split(' ').map(|w| match w.to_lowercase().as_str() {
        "me" => "you",
        "my" => "your",
        "mine" => "yours",
        "myself" => "yourself",
        "i" => "you",
        _ => w,
    }).collect();
    let said = format!("Done. I'll {} {}.", lower_first(&mine.join(" ")), when(&r));
    with(app, true, |x| x.push(r));
    let _ = app.emit_to("main", "routines-changed", ());
    Some(said)
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) if !s.starts_with("I ") => f.to_lowercase().collect::<String>() + c.as_str(),
        Some(f) => f.to_string() + c.as_str(),
        None => String::new(),
    }
}

/// Run one now, in the background, and tell the page how it went.
pub fn fire(app: AppHandle, r: Routine) {
    std::thread::spawn(move || {
        crate::log_line(&format!("routine: {:?}", r.task));
        let _ = app.emit_to("main", "routine", serde_json::json!({ "name": r.name, "task": r.task, "state": "start" }));
        let reply = crate::handle(app.clone(), r.task.clone());
        crate::log_line(&format!("routine done (ok={}): {}", reply.ok, reply.reply));
        let _ = app.emit_to("main", "routine", serde_json::json!({ "name": r.name, "task": r.task, "state": "done", "reply": reply.reply, "ok": reply.ok }));
    });
}

fn now() -> (String, u32, u32, u32) {
    let st = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    (format!("{:04}-{:02}-{:02}", st.wYear, st.wMonth, st.wDay), st.wDayOfWeek as u32 % 7, st.wHour as u32, st.wMinute as u32)
}

/// Is this routine due? (At its time, or up to two hours after it if the PC was off or asleep; once a day.)
fn due(r: &Routine, today: &str, weekday: u32, hour: u32, minute: u32) -> bool {
    if !r.on || r.at_start || r.last == today || !(r.days.is_empty() || r.days.contains(&weekday)) {
        return false;
    }
    let (now_m, at_m) = (hour * 60 + minute, r.hour * 60 + r.minute);
    now_m >= at_m && now_m - at_m <= 120
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        // the ones that run when ULTRON starts, once everything has settled
        std::thread::sleep(Duration::from_secs(75));
        for r in list(&app).into_iter().filter(|r| r.on && r.at_start) {
            fire(app.clone(), r);
            std::thread::sleep(Duration::from_secs(20));
        }
        loop {
            let (today, weekday, hour, minute) = now();
            let ready: Vec<Routine> = with(&app, false, |l| l.iter().filter(|r| due(r, &today, weekday, hour, minute)).cloned().collect());
            for r in ready {
                with(&app, true, |l| l.iter_mut().filter(|x| x.id == r.id).for_each(|x| x.last = today.clone()));
                fire(app.clone(), r);
                std::thread::sleep(Duration::from_secs(25)); // one at a time
            }
            std::thread::sleep(Duration::from_secs(20));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    fn p(s: &str) -> (String, String) {
        let r = parse(s).unwrap_or_else(|| panic!("not a routine: {s}"));
        (when(&r), r.task)
    }
    #[test]
    fn schedules() {
        assert_eq!(p("every weekday at 9 am, open my work apps and read me the news"), ("every weekday at 9:00 AM".into(), "open my work apps and read me the news".into()));
        assert_eq!(p("Every day at 7:30 pm remind me to water the plants"), ("every day at 7:30 PM".into(), "remind me to water the plants".into()));
        assert_eq!(p("every monday and friday at 18:00 play my gym playlist on spotify"), ("Mondays and Fridays at 6:00 PM".into(), "play my gym playlist on spotify".into()));
        assert_eq!(p("every morning tell me the weather"), ("every day at 8:00 AM".into(), "tell me the weather".into()));
        assert_eq!(p("every night at 10 remind me to charge my phone"), ("every day at 10:00 PM".into(), "remind me to charge my phone".into()));
        assert_eq!(p("open spotify every day at 8am"), ("every day at 8:00 AM".into(), "open spotify".into()));
        assert_eq!(p("when you start, open spotify and outlook"), ("when ULTRON starts".into(), "open spotify and outlook".into()));
        assert_eq!(p("every weekend at noon summarize my emails"), ("every weekend at 12:00 PM".into(), "summarize my emails".into()));
        assert_eq!(p("Ultron, every sunday at 9 back up my documents folder"), ("Sundays at 9:00 AM".into(), "back up my documents folder".into()));
        for no in ["what is the capital of France", "open spotify", "remind me at 5 to call mom", "every now and then I feel tired", "tell me everything about mars"] {
            assert!(parse(no).is_none(), "{no}");
        }
        let r = parse("every weekday at 9 am, open my work apps").unwrap();
        assert!(due(&r, "2026-10-12", 1, 9, 0) && due(&r, "2026-10-12", 1, 10, 30) && !due(&r, "2026-10-12", 1, 8, 59) && !due(&r, "2026-10-12", 1, 11, 1) && !due(&r, "2026-10-11", 0, 9, 0));
    }
}
