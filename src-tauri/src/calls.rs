//! Incoming calls: when WhatsApp, Teams, Phone Link, Telegram, Discord (or anything else that rings
//! through Windows) announces a call, the notch opens a little and shows who is calling and from
//! which app, with Answer, Decline and "reply with a message".
//!
//! How it knows: Windows' own notification listener (the same list the notification centre shows),
//! so it works whether or not the pop-up banner is allowed on screen (Do Not Disturb hides those).
//! A notification counts as a call when its words say so ("Incoming voice call", "is calling you").
//! Answering presses the app's own Accept button, on its banner or its call window.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter};
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetClassNameW, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible};
use windows::UI::Notifications::Management::{UserNotificationListener, UserNotificationListenerAccessStatus};
use windows::UI::Notifications::{KnownNotificationBindings, NotificationKinds, UserNotification};

#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub id: u32,
    pub app: String,
    pub who: String,
    pub what: String,
}

static CURRENT: Mutex<Option<Call>> = Mutex::new(None);
/// calls already handled (answered, declined, dismissed): don't show them again while they linger in the list
static DONE: Mutex<Vec<u32>> = Mutex::new(Vec::new());

const CALL_WORDS: [&str; 12] = ["incoming call", "incoming voice call", "incoming video call", "voice call", "video call", "is calling", "calling you", "calling…", "calling...", "audio call", "wants to call", "ringing"];
const NOT_CALLS: [&str; 6] = ["missed", "ended", "declined", "you called", "call back", "voicemail"];

/// Does this notification announce a call that's ringing now? Returns (who, what).
pub fn read(texts: &[String]) -> Option<(String, String)> {
    let all = texts.join(" · ").to_lowercase();
    if !CALL_WORDS.iter().any(|w| all.contains(w)) || NOT_CALLS.iter().any(|w| all.contains(w)) {
        return None;
    }
    let is_call_line = |t: &str| { let l = t.to_lowercase(); CALL_WORDS.iter().any(|w| l.contains(w)) };
    let what_line = texts.iter().find(|t| is_call_line(t)).cloned().unwrap_or_default();
    // the caller: the line that isn't the "incoming call" line, or what comes before "is calling" in it
    let mut who = texts.iter().find(|t| !is_call_line(t) && !t.trim().is_empty()).cloned().unwrap_or_default();
    if who.is_empty() {
        let l = what_line.to_lowercase();
        for sep in [" is calling", " wants to call", " calling you"] {
            if let Some(i) = l.find(sep) {
                who = what_line[..i].trim().to_string();
            }
        }
    }
    let l = what_line.to_lowercase();
    let what = if l.contains("video") { "Incoming video call" } else if l.contains("voice") || l.contains("audio") { "Incoming voice call" } else { "Incoming call" };
    Some((who.trim().trim_end_matches([':', ',']).to_string(), what.to_string()))
}

/// The call that's ringing, if Windows has one in its notifications.
fn ringing(list: &[UserNotification]) -> Option<Call> {
    let done = DONE.lock().unwrap().clone();
    let mut best: Option<(i64, Call)> = None;
    for n in list {
        let Ok(id) = n.Id() else { continue };
        if done.contains(&id) {
            continue;
        }
        let created = n.CreationTime().map(|t| t.UniversalTime).unwrap_or(0);
        let app = n.AppInfo().and_then(|a| a.DisplayInfo()).and_then(|d| d.DisplayName()).map(|s| s.to_string()).unwrap_or_default();
        let Ok(binding) = n.Notification().and_then(|x| x.Visual()).and_then(|v| KnownNotificationBindings::ToastGeneric().and_then(|k| v.GetBinding(&k))) else { continue };
        let texts: Vec<String> = binding.GetTextElements().map(|els| els.into_iter().filter_map(|e| e.Text().ok().map(|t| t.to_string())).collect()).unwrap_or_default();
        if let Some((who, what)) = read(&texts) {
            if best.as_ref().map(|(t, _)| created > *t).unwrap_or(true) {
                best = Some((created, Call { id, app, who, what }));
            }
        }
    }
    // a call that's been sitting there for over a minute isn't ringing any more
    let (created, call) = best?;
    let age = filetime_now().saturating_sub(created) / 10_000_000;
    (age < 75).then_some(call)
}

/// Now, in the same 100-nanosecond ticks since 1601 that Windows stamps notifications with.
fn filetime_now() -> i64 {
    let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() / 100).unwrap_or(0) as i64;
    unix + 116_444_736_000_000_000
}

fn can_reply(app: &str) -> bool {
    let l = app.to_lowercase();
    ["whatsapp", "telegram", "discord"].iter().any(|a| l.contains(a))
}

fn announce(app: &AppHandle, c: &Call) {
    crate::log_line(&format!("call: {} on {} ({})", if c.who.is_empty() { "someone" } else { "a caller" }, c.app, c.what));
    let _ = app.emit_to("main", "call", serde_json::json!({ "id": c.id, "app": c.app, "who": c.who, "what": c.what, "canReply": can_reply(&c.app) && !c.who.is_empty() }));
}

/// A stamp of Windows' own notification store (its size and when it was last written, with its journal). It changes
/// when a notification comes or goes. Reading it costs next to nothing; asking Windows for every notification it
/// holds costs more the more it holds, and they pile up the longer the PC runs: asked every 0.9 s, that was a cost
/// that kept growing while ULTRON stayed open. 0 when the store can't be found (then Windows is asked as before).
fn store_stamp() -> u64 {
    let Ok(base) = std::env::var("LOCALAPPDATA") else { return 0 };
    let db = std::path::Path::new(&base).join("Microsoft").join("Windows").join("Notifications").join("wpndatabase.db");
    let mut s = 0u64;
    for f in [db.clone(), db.with_extension("db-wal")] {
        if let Ok(m) = std::fs::metadata(&f) {
            let t = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos() as u64).unwrap_or(0);
            s = s.wrapping_mul(31).wrapping_add(m.len()).wrapping_mul(31).wrapping_add(t);
        }
    }
    s
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        std::thread::sleep(Duration::from_secs(6));
        let Ok(listener) = UserNotificationListener::Current() else { return };
        let mut status = listener.GetAccessStatus().unwrap_or(UserNotificationListenerAccessStatus::Unspecified);
        if status != UserNotificationListenerAccessStatus::Allowed {
            status = listener.RequestAccessAsync().ok().and_then(|a| a.get().ok()).unwrap_or(status);
        }
        crate::log_line(&format!("calls: watching Windows notifications ({})", if status == UserNotificationListenerAccessStatus::Allowed { "allowed" } else { "not allowed: turn on notification access for ULTRON in Windows Settings › Privacy" }));
        if status != UserNotificationListenerAccessStatus::Allowed {
            return;
        }
        let (mut stamp, mut asked) = (0u64, std::time::Instant::now() - Duration::from_secs(60));
        loop {
            std::thread::sleep(Duration::from_millis(300));
            // Windows is asked when its store has changed; while a call rings, every 0.9 s as before (to see it end);
            // and every 20 s whatever happens, in case a change was missed
            let s = store_stamp();
            let rings = CURRENT.lock().unwrap().is_some();
            let due = if s == 0 || rings { asked.elapsed() >= Duration::from_millis(900) } else { s != stamp || asked.elapsed() >= Duration::from_secs(20) };
            if !due {
                continue;
            }
            stamp = s;
            asked = std::time::Instant::now();
            // (one look at Windows' list a turn, shared with ULTRON's own notifications when those are on)
            let list: Vec<UserNotification> = listener.GetNotificationsAsync(NotificationKinds::Toast).ok().and_then(|a| a.get().ok()).map(|v| v.into_iter().collect()).unwrap_or_default();
            crate::notes::poll(&app, &list);
            crate::coders::from_notes(&app, &list);
            let now = ringing(&list);
            let mut cur = CURRENT.lock().unwrap();
            if now != *cur {
                match &now {
                    Some(c) if cur.as_ref().map(|x| x.id) != Some(c.id) => announce(&app, c),
                    None => {
                        let _ = app.emit_to("main", "call-end", ());
                    }
                    _ => {}
                }
                *cur = now;
            }
        }
    });
}

/* ---------------- answering ---------------- */

fn exe_of(pid: u32) -> String {
    use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return String::new() };
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_FORMAT(0), windows::core::PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(h);
        if !ok {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..len as usize]).rsplit('\\').next().unwrap_or("").to_lowercase()
    }
}

unsafe extern "system" fn each(h: HWND, l: LPARAM) -> BOOL {
    let out = &mut *(l.0 as *mut Vec<HWND>);
    if IsWindowVisible(h).as_bool() {
        out.push(h);
    }
    BOOL(1)
}

/// Windows that could hold the call's Accept / Decline buttons: Windows' notification banner, and the calling app's own windows.
fn candidates(app_name: &str) -> Vec<HWND> {
    let mut all: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut all as *mut _ as isize));
    }
    let key: Vec<String> = app_name.to_lowercase().split_whitespace().filter(|w| w.len() >= 4 && *w != "microsoft" && *w != "desktop").map(String::from).collect();
    let mut out = Vec::new();
    for h in all {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
        let exe = exe_of(pid);
        let mut cls = [0u16; 80];
        let n = unsafe { GetClassNameW(h, &mut cls) }.max(0) as usize;
        let class = String::from_utf16_lossy(&cls[..n]);
        let banner = exe.contains("shellexperiencehost") && class.contains("CoreWindow");
        let theirs = !exe.is_empty() && (key.iter().any(|k| exe.contains(k.as_str())) || (app_name.to_lowercase().contains("phone link") && exe.contains("phoneexperiencehost")) || (app_name.to_lowercase().contains("teams") && exe.contains("teams")));
        if banner || theirs {
            let mut r = RECT::default();
            if unsafe { GetWindowRect(h, &mut r) }.is_ok() && r.right - r.left > 40 {
                out.push(h);
            }
        }
    }
    out
}

/// Press the call's own button: `accept` or decline.
fn press(app_name: &str, accept: bool) -> bool {
    let words: &[&str] = if accept { &["accept", "answer", "pick up", "join", "accept with audio", "audio"] } else { &["decline", "reject", "ignore", "dismiss", "hang up", "end call"] };
    let t0 = Instant::now();
    // the banner or call window can take a moment to appear after the notification
    while t0.elapsed() < Duration::from_millis(1800) {
        for h in candidates(app_name) {
            if crate::vision::press_button(h, words) {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

fn finish(app: &AppHandle) -> Option<Call> {
    let c = CURRENT.lock().unwrap().take()?;
    DONE.lock().unwrap().push(c.id);
    let _ = app.emit_to("main", "call-end", ());
    Some(c)
}

/// Answer the ringing call. False when the app's Accept button couldn't be found (then the app is brought up instead).
pub fn answer(app: &AppHandle) -> bool {
    let Some(c) = CURRENT.lock().unwrap().clone() else { return false };
    let ok = press(&c.app, true);
    crate::log_line(&format!("call: answer {}", if ok { "pressed" } else { "button not found, opening the app" }));
    if !ok {
        let _ = crate::commands::run(app, &format!("open {}", c.app.to_lowercase()));
    }
    finish(app);
    ok
}

pub fn decline(app: &AppHandle) -> bool {
    let Some(c) = CURRENT.lock().unwrap().clone() else { return false };
    let ok = press(&c.app, false);
    crate::log_line(&format!("call: decline {}", if ok { "pressed" } else { "button not found (dismissed here)" }));
    finish(app);
    ok
}

/// Decline and send the caller a message instead (you typed it and pressed send, so it goes without asking again).
pub fn reply(app: &AppHandle, text: &str) -> String {
    let Some(c) = CURRENT.lock().unwrap().clone() else { return "That call has ended.".into() };
    if text.trim().is_empty() || c.who.is_empty() || !can_reply(&c.app) {
        return "I can't message them from here.".into();
    }
    let _ = press(&c.app, false);
    finish(app);
    let chat = ["whatsapp", "telegram", "discord"].iter().find(|a| c.app.to_lowercase().contains(**a)).copied().unwrap_or("whatsapp");
    let r = crate::agent::execute(app, vec![crate::agent::Step::Message(chat.into(), c.who.clone(), text.trim().to_string())]);
    if !r.ok {
        return r.reply;
    }
    crate::agent::confirm(app, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn spots_calls() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(read(&s(&["Priya Sharma", "Incoming voice call"])), Some(("Priya Sharma".into(), "Incoming voice call".into())));
        assert_eq!(read(&s(&["Rahul is calling you"])), Some(("Rahul".into(), "Incoming call".into())));
        assert_eq!(read(&s(&["Mom", "Incoming video call"])), Some(("Mom".into(), "Incoming video call".into())));
        assert_eq!(read(&s(&["Priya Sharma", "Missed voice call"])), None);
        assert_eq!(read(&s(&["Priya Sharma", "Are you coming to the party?"])), None);
        assert_eq!(read(&s(&["Dev Team", "Rahul: can someone call the client back"])), None);
    }
}
