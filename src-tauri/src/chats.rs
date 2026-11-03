//! Past conversations, kept on this PC, so SYLVER can pick up where you left off, even after a
//! restart. Two things use them:
//!   - the last few exchanges ride along with every question (follow-ups like "and how tall is he?"
//!     work across restarts and long pauses);
//!   - older ones are searched for whatever the new question is about, and the best match or two is
//!     handed to the brain ("what was that phone I asked about last week?").
//! "What did we talk about?" is answered straight from the list. Facts about you live in memory.rs;
//! this is the conversation itself. It never leaves the PC, and "forget our chats" clears it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

const KEEP: usize = 800;

#[derive(Serialize, Deserialize, Clone)]
pub struct Turn {
    pub at: u64,
    pub user: String,
    pub reply: String,
}

static CACHE: Mutex<Option<Vec<Turn>>> = Mutex::new(None);

fn path(app: &AppHandle) -> PathBuf {
    let d = app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir());
    let _ = std::fs::create_dir_all(&d);
    d.join("chats.json")
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn with<T>(app: &AppHandle, f: impl FnOnce(&mut Vec<Turn>) -> T) -> T {
    let mut g = CACHE.lock().unwrap();
    let list = g.get_or_insert_with(|| std::fs::read_to_string(path(app)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default());
    f(list)
}

fn clip(s: &str, n: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= n { s } else { s.chars().take(n).collect::<String>() + "…" }
}

/// Keep one exchange.
pub fn add(app: &AppHandle, user: &str, reply: &str) {
    let (u, r) = (clip(user, 300), clip(reply.split("\n>>").next().unwrap_or(reply), 500));
    if u.len() < 2 || r.is_empty() || r == "Stopped." || r == "Okay, I stopped." {
        return;
    }
    let json = with(app, |l| {
        l.push(Turn { at: now(), user: u, reply: r });
        let n = l.len();
        if n > KEEP {
            l.drain(..n - KEEP);
        }
        serde_json::to_string(l).unwrap_or_default()
    });
    let _ = crate::keep(path(app), json);
}

pub fn clear(app: &AppHandle) {
    with(app, |l| l.clear());
    let _ = std::fs::write(path(app), "[]");
}

pub fn count(app: &AppHandle) -> usize {
    with(app, |l| l.len())
}

/// The last `n` exchanges, oldest first, as chat messages (for picking up after a restart).
pub fn recent(app: &AppHandle, n: usize) -> Vec<(String, String)> {
    with(app, |l| {
        l.iter().rev().take(n).rev().flat_map(|t| [("user".to_string(), t.user.clone()), ("assistant".to_string(), t.reply.clone())]).collect()
    })
}

fn when(at: u64) -> String {
    let d = now().saturating_sub(at);
    match d {
        0..=3599 => "earlier".into(),
        3600..=43_199 => "a few hours ago".into(),
        43_200..=129_599 => "yesterday".into(),
        _ => {
            let days = d / 86_400;
            if days < 14 { format!("{days} days ago") } else if days < 60 { format!("{} weeks ago", days / 7) } else { format!("{} months ago", days / 30) }
        }
    }
}

const STOP: &[&str] = &[
    "the", "and", "for", "that", "this", "with", "you", "your", "are", "was", "were", "what", "when", "where", "which", "who", "why", "how", "can", "could", "would", "should", "did", "does",
    "have", "has", "had", "about", "tell", "from", "into", "its", "it's", "our", "out", "not", "but", "any", "all", "one", "get", "got", "make", "made", "please", "sylver", "ultron", "hey", "just",
    "like", "want", "need", "know", "there", "their", "them", "then", "than", "will", "been", "being", "some", "more", "most", "also", "very", "much", "many", "said", "say", "ask", "asked",
    "talk", "talked", "remember",
];

fn words(s: &str) -> HashSet<String> {
    s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() >= 3 && !STOP.contains(w)).map(|w| w.trim_end_matches('s').to_string()).filter(|w| w.len() >= 3).collect()
}

/// Earlier exchanges that bear on this question (not the last `skip`, which the brain already has).
pub fn recall(app: &AppHandle, query: &str, skip: usize) -> Option<String> {
    let q = words(query);
    if q.is_empty() {
        return None;
    }
    let l = query.to_lowercase();
    // "what did I say about…", "that thing we talked about": the user is pointing at the past
    let asking_back = ["we talked", "we discussed", "i told you", "i asked", "i said", "you said", "you told", "last time", "earlier", "yesterday", "before", "again", "remember"].iter().any(|p| l.contains(p));
    with(app, |list| {
        let n = list.len().saturating_sub(skip);
        if n == 0 {
            return None;
        }
        let bags: Vec<HashSet<String>> = list[..n].iter().map(|t| words(&format!("{} {}", t.user, t.reply))).collect();
        let mut df: HashMap<&String, usize> = HashMap::new();
        for w in &q {
            df.insert(w, bags.iter().filter(|b| b.contains(w)).count());
        }
        let mut scored: Vec<(f64, usize)> = Vec::new();
        for (i, b) in bags.iter().enumerate() {
            let hits: Vec<&String> = q.iter().filter(|w| b.contains(*w)).collect();
            if hits.is_empty() {
                continue;
            }
            // rare words count for more; one rare, specific word is enough on its own
            let score: f64 = hits.iter().map(|w| (1.0 + n as f64 / (1.0 + df[*w] as f64)).ln()).sum();
            let rare = hits.iter().any(|w| w.len() >= 5 && df[*w] <= (n / 40).max(2));
            if hits.len() >= 2 || rare || (asking_back && !hits.is_empty()) {
                scored.push((score + i as f64 / n as f64 * 0.3, i));
            }
        }
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let top: Vec<String> = scored.iter().take(2).map(|(_, i)| {
            let t = &list[*i];
            format!("({}) the user said “{}” and you answered “{}”", when(t.at), clip(&t.user, 140), clip(&t.reply, 220))
        }).collect();
        (!top.is_empty()).then(|| format!("From your earlier conversations with the user, in case it helps: {}.", top.join("; ")))
    })
}

/// "What did we talk about?", "what was my last question?", "forget our chats".
pub fn command(app: &AppHandle, text: &str) -> Option<String> {
    let l = text.to_lowercase();
    let l = l.trim().trim_end_matches(['?', '.', '!']).trim();
    let l = l.strip_prefix("sylver").or_else(|| l.strip_prefix("ultron")).map(|r| r.trim_start_matches([',', ' '])).unwrap_or(l);
    if ["forget our chat", "forget our conversation", "forget the chat", "forget this conversation", "clear our chat", "clear the chat", "clear chat history", "delete our chat", "delete chat history",
        "forget what we talked about", "clear our conversation"].iter().any(|p| l.starts_with(p)) {
        clear(app);
        app.state::<crate::ai::Ai>().forget();
        return Some("Done. I've forgotten our conversations.".into());
    }
    let last_q = ["what was my last question", "what did i just ask", "what did i ask you last", "what was the last thing i asked", "what did i last ask"].iter().any(|p| l.starts_with(p));
    let about = ["what did we talk about", "what have we talked about", "what were we talking about", "what did we discuss", "what did i ask you", "what have i asked you", "do you remember our last",
        "do you remember what we talked", "what did we chat about", "summarize our conversation", "summarise our conversation", "what did we talk"].iter().any(|p| l.starts_with(p));
    if !last_q && !about {
        return None;
    }
    with(app, |list| {
        if list.is_empty() {
            return Some("We haven't talked about anything yet.".to_string());
        }
        if last_q {
            let t = list.last().unwrap();
            return Some(format!("You asked “{}”, and I said: {}", clip(&t.user, 160), clip(&t.reply, 240)));
        }
        // the topics, newest first, grouped by when
        let day = |at: u64| if l.contains("yesterday") { (43_200..172_800).contains(&now().saturating_sub(at)) } else if l.contains("today") { now().saturating_sub(at) < 64_800 } else { true };
        let picks: Vec<&Turn> = list.iter().rev().filter(|t| day(t.at)).take(6).collect();
        if picks.is_empty() {
            return Some("I don't have anything from then.".to_string());
        }
        let newest = when(picks[0].at);
        let topics: Vec<String> = picks.iter().rev().map(|t| format!("“{}”", clip(&t.user, 70))).collect();
        let listed = match topics.len() {
            1 => topics[0].clone(),
            n => format!("{} and {}", topics[..n - 1].join(", "), topics[n - 1]),
        };
        Some(format!("Most recently ({newest}) you asked me {listed}. I remember {} exchanges in all.", list.len()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn word_bags() {
        let w = words("What was that budget phone you recommended for my sister?");
        assert!(w.contains("budget") && w.contains("phone") && w.contains("sister") && !w.contains("what"));
        assert_eq!(when(now() - 100), "earlier");
        assert_eq!(when(now() - 3 * 86_400), "3 days ago");
    }
}
