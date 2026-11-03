//! SYLVER's memory: facts about you, kept on this PC and used in every answer. It saves what's
//! worth knowing ("my sister is Priya", "I'm vegetarian", "remember that my exam is on Friday"),
//! not the conversation itself. You can ask what it remembers, tell it to forget something, or
//! see and delete everything in Settings.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

#[derive(Serialize, Deserialize, Clone)]
pub struct Fact {
    pub text: String,
    pub at: u64,
}

static LOCK: Mutex<()> = Mutex::new(());

fn file(app: &AppHandle) -> PathBuf {
    let d = app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir());
    let _ = std::fs::create_dir_all(&d);
    d.join("memory.json")
}

pub fn all(app: &AppHandle) -> Vec<Fact> {
    std::fs::read_to_string(file(app)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save(app: &AppHandle, facts: &[Fact]) {
    let _ = crate::keep(file(app), serde_json::to_string_pretty(facts).unwrap_or_default());
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn words(s: &str) -> Vec<String> {
    s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 2).map(str::to_string).collect()
}

/// Keep a fact; a newer fact about the same thing replaces the older one.
pub fn add(app: &AppHandle, text: &str) {
    let text = text.trim().trim_end_matches('.').to_string();
    if text.len() < 4 {
        return;
    }
    let _g = LOCK.lock().unwrap();
    let mut facts = all(app);
    let key: Vec<String> = words(&text).into_iter().take(3).collect();
    facts.retain(|f| {
        let w = words(&f.text);
        !(f.text.eq_ignore_ascii_case(&text) || (key.len() >= 2 && w.iter().take(3).cloned().collect::<Vec<_>>() == key))
    });
    facts.push(Fact { text: text.clone(), at: now() });
    if facts.len() > 200 {
        facts.remove(0);
    }
    save(app, &facts);
    crate::log_line(&format!("remembered: {text}"));
}

pub fn remove(app: &AppHandle, index: Option<usize>) {
    let _g = LOCK.lock().unwrap();
    let mut facts = all(app);
    match index {
        Some(i) if i < facts.len() => {
            facts.remove(i);
        }
        Some(_) => {}
        None => facts.clear(),
    }
    save(app, &facts);
}

/// Change one thing (by its place in the list) to what you've written; empty, it's forgotten.
pub fn edit(app: &AppHandle, index: usize, text: &str) {
    let _g = LOCK.lock().unwrap();
    let mut facts = all(app);
    if index < facts.len() {
        let t = text.trim();
        if t.is_empty() {
            facts.remove(index);
        } else {
            facts[index].text = t.to_string();
        }
        save(app, &facts);
    }
}

/// "forget that my sister is Priya" / "forget about my exam": drop the facts that match best.
fn forget_matching(app: &AppHandle, what: &str) -> usize {
    let want = words(what);
    if want.is_empty() {
        return 0;
    }
    let _g = LOCK.lock().unwrap();
    let mut facts = all(app);
    let before = facts.len();
    facts.retain(|f| {
        let w = words(&f.text);
        let hit = want.iter().filter(|x| w.contains(x)).count();
        hit * 2 < want.len().max(1) + 1 // keep unless most of the words match
    });
    let n = before - facts.len();
    save(app, &facts);
    n
}

/// For the brain: what it knows about you.
pub fn context(app: &AppHandle) -> String {
    let facts = all(app);
    if facts.is_empty() {
        return String::new();
    }
    let list: Vec<String> = facts.iter().rev().take(25).map(|f| f.text.clone()).collect();
    format!(" What you remember about the user (use it when it's relevant, don't recite it): {}.", list.join("; "))
}

fn cap(s: &str) -> String {
    let s = s.trim().trim_end_matches(['.', '!', '?']).trim();
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// Turn "I", "my", "me" into a note about the user ("my exam is Friday" -> "the user's exam is Friday").
fn about_user(s: &str) -> String {
    let mut out = Vec::new();
    for w in s.split_whitespace() {
        let lw = w.to_lowercase();
        let core = lw.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '\'');
        let trail = &w[core.len().min(w.len())..];
        let rep = match core {
            "i" => "the user",
            "i'm" | "im" => "the user is",
            "am" => "is",
            "my" => "the user's",
            "me" => "the user",
            "mine" => "the user's",
            "i've" => "the user has",
            "i'll" => "the user will",
            "i'd" => "the user would",
            "myself" => "the user",
            _ => "",
        };
        out.push(if rep.is_empty() { w.to_string() } else { format!("{rep}{trail}") });
    }
    out.join(" ")
}

/// Commands about memory itself. Returns SYLVER's answer when it handled one.
pub fn command(app: &AppHandle, text: &str) -> Option<String> {
    let l = text.trim().trim_end_matches(['.', '!', '?']).trim();
    let ll = l.to_lowercase();
    let ll = ll.strip_prefix("sylver").or_else(|| ll.strip_prefix("ultron")).map(|r| r.trim_start_matches([',', ' '])).unwrap_or(&ll).to_string();
    let after = |p: &str| ll.strip_prefix(p).map(|r| l[l.len() - r.len()..].trim().to_string());
    if ["what do you remember", "what do you know about me", "what have you remembered", "show my memories", "what's in your memory", "what is in your memory"]
        .iter()
        .any(|p| ll.starts_with(p))
    {
        let facts = all(app);
        return Some(if facts.is_empty() {
            "Nothing yet. Tell me things like “remember that my exam is on Friday” and I'll keep them, right here on this PC.".into()
        } else {
            let list: Vec<String> = facts.iter().rev().take(8).map(|f| f.text.clone()).collect();
            format!("Here's what I remember: {}.", list.join("; "))
        });
    }
    if ["forget everything", "forget all", "clear your memory", "erase your memory", "wipe your memory"].iter().any(|p| ll.starts_with(p)) {
        remove(app, None);
        crate::chats::clear(app);
        app.state::<crate::ai::Ai>().forget();
        return Some("Done. I've forgotten everything you told me, and our conversations.".into());
    }
    for p in ["forget that ", "forget about ", "forget "] {
        if let Some(what) = after(p) {
            if what.is_empty() || ["it", "this", "that"].contains(&what.as_str()) {
                return Some("Tell me what to forget, like “forget my exam date”.".into());
            }
            let n = forget_matching(app, &what);
            return Some(if n > 0 { "Okay, I've forgotten that.".into() } else { "I didn't have that remembered.".into() });
        }
    }
    for p in ["remember that ", "remember this: ", "remember: ", "please remember that ", "remember "] {
        if let Some(what) = after(p) {
            if what.split_whitespace().count() >= 2 && !what.to_lowercase().starts_with("to ") {
                let (_, date) = crate::commands::now_strings();
                add(app, &format!("{} (noted on {date})", cap(&about_user(&what))));
                return Some(format!("Got it. I'll remember that {}.", what.trim_end_matches('.')));
            }
        }
    }
    None
}

/// Quietly notice facts in what you say ("my name is…", "I love…", "my sister is…").
pub fn notice(app: &AppHandle, text: &str) {
    let l = text.trim().trim_end_matches(['.', '!']).trim();
    if l.ends_with('?') || l.len() > 160 {
        return;
    }
    let ll = l.to_lowercase();
    let rest = |p: &str| ll.find(p).filter(|i| *i == 0 || ll[..*i].ends_with(' ') || ll[..*i].ends_with(',')).map(|i| l[i + p.len()..].trim().to_string());
    let take = |s: String| s.split([',', '.', '!', ';']).next().unwrap_or("").trim().to_string();
    if let Some(n) = rest("my name is ").or_else(|| rest("call me ")).map(take).filter(|n| !n.is_empty() && n.split_whitespace().count() <= 3) {
        return add(app, &format!("The user's name is {}", cap(&n)));
    }
    for (p, verb) in [("i love ", "loves"), ("i really like ", "really likes"), ("i like ", "likes"), ("i prefer ", "prefers"), ("i hate ", "hates"),
        ("i don't like ", "doesn't like"), ("i dont like ", "doesn't like"), ("i'm allergic to ", "is allergic to"), ("i am allergic to ", "is allergic to")]
    {
        if let Some(x) = rest(p).map(take).filter(|x| !x.is_empty() && x.split_whitespace().count() <= 8) {
            return add(app, &format!("The user {verb} {x}"));
        }
    }
    for (p, verb) in [("i live in ", "lives in"), ("i work at ", "works at"), ("i work as ", "works as"), ("i study ", "studies"), ("i'm studying ", "is studying"),
        ("i'm a ", "is a"), ("i am a ", "is a"), ("i'm an ", "is an"), ("i am an ", "is an"), ("i was born in ", "was born in"), ("i was born on ", "was born on")]
    {
        if let Some(x) = rest(p).map(take).filter(|x| !x.is_empty() && x.split_whitespace().count() <= 6) {
            return add(app, &format!("The user {verb} {x}"));
        }
    }
    // "my sister is Priya", "my favourite colour is blue", "my birthday is 3 March"
    if let Some(x) = rest("my ").map(take) {
        if let Some((what, val)) = x.split_once(" is ").or_else(|| x.split_once("'s ")) {
            let (what, val) = (what.trim(), val.trim());
            if !what.is_empty() && what.split_whitespace().count() <= 3 && !val.is_empty() && val.split_whitespace().count() <= 6 {
                return add(app, &format!("The user's {what} is {val}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rewrites_for_the_user() {
        assert_eq!(cap(&about_user("my exam is on Friday")), "The user's exam is on Friday");
        assert_eq!(cap(&about_user("I'm vegetarian")), "The user is vegetarian");
        assert_eq!(cap(&about_user("I am allergic to peanuts.")), "The user is allergic to peanuts");
    }
}
