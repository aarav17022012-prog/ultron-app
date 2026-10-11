//! Fresh facts for the local AI. A 1.5B model on your PC can't know about people
//! and events after it was made, so for "who is…", "what happened to…", news and
//! weather questions ULTRON looks the topic up first (Wikipedia, Google News, wttr.in;
//! all free, no account) and the model answers from that.

use std::time::Duration;

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn get(url: &str) -> Option<String> {
    crate::net::get_text(url, Duration::from_millis(2500))
}

struct Kind {
    news: bool,
}

/// What the question is about, if it's the kind that needs looking up.
fn subject(q: &str) -> Option<(String, Kind)> {
    let l = q.to_lowercase().replace(['’', '‘'], "'");
    let l = l.trim().trim_end_matches(['?', '.', '!']).trim();
    let l = l.trim_start_matches("ultron").trim_start_matches([',', ' ']).trim();
    let newsy = [
        "what happened to", "what happened with", "what happened in", "what happened at", "what's happening with", "what is happening with",
        "what's going on with", "what is going on with", "whats going on with", "what's going on in", "what is going on in", "news about",
        "news on", "latest news about", "latest news on", "latest on", "any news about", "any news on", "what's new with", "is there any news on",
        "when did", "did", "is",
    ];
    // people, fun facts and recent things need the web; general knowledge ("what is a black hole") the brain already has
    let recent = ["latest", "current", "currently", "today", "tonight", "yesterday", "this week", "this month", "this year", "right now", "recent",
        "recently", "2024", "2025", "2026", "2027", "score", "price", "stock", "election", "results", "released", "release date", "died", "dead", "alive"];
    let is_recent = recent.iter().any(|r| l.contains(r));
    let mut facts: Vec<&str> = vec![
        "tell me a fun fact about", "give me a fun fact about", "tell me an interesting fact about", "tell me a fact about", "a fun fact about",
        "fun fact about", "fun facts about", "facts about", "who is", "who's", "whos", "who was", "who are", "who were", "who won", "who owns", "how old is",
    ];
    // knowledge questions get the facts too: a small brain guesses names, dates and cases otherwise
    facts.extend(["tell me about", "tell me something about", "what do you know about", "do you know about", "do you know", "what is", "what's", "whats",
        "what are", "what was", "what were", "how did", "why did", "why was", "when was", "when is", "when did", "where is", "where was", "who invented",
        "who discovered", "who founded", "who created", "explain", "history of", "the story of", "what about"]);
    // "junko case", "the chernobyl disaster": a named event, even without a question word
    let eventy = ["case", "incident", "murder", "scandal", "disaster", "attack", "massacre", "war", "battle", "trial", "accident", "crash", "riot", "assassination"];
    let bare = (l.split_whitespace().count() <= 6 && eventy.iter().any(|e| l.split_whitespace().any(|w| w == *e))).then(|| (l, false));
    let (rest, news) = newsy
        .iter()
        .find_map(|p| l.strip_prefix(p).filter(|r| r.starts_with(' ')).map(|r| (r.trim(), true)))
        .or_else(|| facts.iter().find_map(|p| l.strip_prefix(p).filter(|r| r.starts_with(' ')).map(|r| (r.trim(), p.starts_with("who") || is_recent))))
        .or(bare)?;
    let mut s = rest.trim_start_matches("the ").to_string();
    for tail in [" dead", " alive", " still alive", " married", " die", " died", " doing", " up to", " these days", " now", " lately", " recently", " today"] {
        if let Some(r) = s.strip_suffix(tail) {
            s = r.trim().to_string();
        }
    }
    // things about you, your PC or ULTRON itself aren't on the web
    let personal = ["my ", "me", "you", "your", "it", "this", "that", "time", "date", "day", "the time", "the date", "up", "yourself", "ultron", "on my screen",
        "the weather", "weather", "going on", "happening"];
    if s.len() < 3 || personal.iter().any(|p| s == *p || s.starts_with(&format!("{p} ")) || s.starts_with("my ")) {
        return None;
    }
    // maths and plain definitions of everyday words don't need the web, but people, places and events do
    if s.chars().any(|c| c.is_ascii_digit()) && s.chars().any(|c| "+-*/^%".contains(c)) {
        return None;
    }
    Some((s, Kind { news }))
}

fn key_words(s: &str) -> Vec<String> {
    const SKIP: [&str; 14] = ["the", "and", "was", "what", "who", "about", "tell", "case", "with", "from", "that", "this", "were", "when"];
    s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 2 && !SKIP.contains(w)).map(str::to_string).collect()
}

pub fn wiki(q: &str) -> Option<String> {
    let url = format!(
        "https://en.wikipedia.org/w/api.php?action=query&generator=search&gsrsearch={}&gsrlimit=1&prop=extracts&exintro=1&explaintext=1&exsentences=5&format=json&redirects=1",
        enc(q)
    );
    let v: serde_json::Value = serde_json::from_str(&get(&url)?).ok()?;
    let page = v.get("query")?.get("pages")?.as_object()?.values().next()?.clone();
    let title = page.get("title")?.as_str()?.to_string();
    let extract = page.get("extract")?.as_str()?.trim().to_string();
    // only if it's really about what was asked (a wrong article would mislead the brain)
    let want = key_words(q);
    let have = key_words(&format!("{title} {}", extract.chars().take(400).collect::<String>()));
    if !want.is_empty() && !want.iter().any(|w| have.iter().any(|h| h == w || h.starts_with(w.as_str()) || w.starts_with(h.as_str()))) {
        crate::log_line(&format!("wikipedia “{title}” isn't about “{q}”: skipped"));
        return None;
    }
    (!extract.is_empty()).then(|| format!("Wikipedia, “{title}”: {}", extract.chars().take(700).collect::<String>()))
}

fn headlines(q: &str) -> Option<String> {
    let items = crate::sys::news_search(q);
    if items.is_empty() {
        return None;
    }
    let lines: Vec<String> = items
        .iter()
        .take(6)
        .map(|n| {
            // "Wed, 08 Oct 2026 09:12:00 GMT" -> "08 Oct 2026"
            let date = n.published.split_whitespace().skip(1).take(3).collect::<Vec<_>>().join(" ");
            format!("- {} ({}, {})", n.title, n.source, date)
        })
        .collect();
    Some(format!("Latest headlines:\n{}", lines.join("\n")))
}

fn weather(q: &str) -> Option<String> {
    let l = q.to_lowercase();
    if !(l.contains("weather") || l.contains("temperature outside") || l.contains("going to rain") || l.contains("forecast")) {
        return None;
    }
    let place = l.split(" in ").nth(1).map(|p| p.trim_end_matches(['?', '.', '!']).replace("today", "").replace("tomorrow", "").trim().to_string()).unwrap_or_default();
    let url = format!("https://wttr.in/{}?format=%l:+%C,+%t+(feels+like+%f),+humidity+%h,+wind+%w&m", enc(&place));
    let t = get(&url)?;
    let t = t.trim();
    (!t.is_empty() && !t.contains("Unknown location") && t.len() < 300).then(|| format!("Current weather from wttr.in: {t}"))
}

/// A translation from the web (MyMemory's free service: about a second, and right in the languages the
/// brains on this PC are slow or shaky in: measured, Hindi took 8 s here and Tamil and Urdu came out wrong).
/// Only used when "Look things up online" is on; the text is sent to that service to be translated.
pub fn translate(text: &str, from: &str, to: &str) -> Option<String> {
    let code = |l: &str| Some(match l {
        "English" => "en", "Hindi" => "hi", "Spanish" => "es", "French" => "fr", "German" => "de", "Italian" => "it", "Portuguese" => "pt", "Russian" => "ru", "Japanese" => "ja", "Korean" => "ko",
        "Chinese" => "zh-CN", "Arabic" => "ar", "Bengali" => "bn", "Urdu" => "ur", "Tamil" => "ta", "Telugu" => "te", "Marathi" => "mr", "Gujarati" => "gu", "Punjabi" => "pa", "Turkish" => "tr",
        "Indonesian" => "id", "Dutch" => "nl", "Auto" | "" => "autodetect", _ => return None,
    });
    let (f, t) = (code(from)?, code(to)?);
    if t == "autodetect" || text.trim().is_empty() || text.len() > 480 {
        return None; // (the service takes short texts; longer ones are translated on this PC)
    }
    let v: serde_json::Value = serde_json::from_str(&get(&format!("https://api.mymemory.translated.net/get?q={}&langpair={}%7C{}", enc(text.trim()), f, t))?).ok()?;
    let out = v.pointer("/responseData/translatedText")?.as_str()?.trim().to_string();
    let ok = v.get("responseStatus").map(|s| s.as_i64() == Some(200) || s.as_str() == Some("200")).unwrap_or(false);
    if !ok || out.is_empty() || out.to_uppercase().contains("MYMEMORY WARNING") || out.to_uppercase().contains("QUERY LENGTH LIMIT") || out.to_uppercase().contains("INVALID LANGUAGE") {
        return None;
    }
    Some(out.replace("&#39;", "'").replace("&quot;", "\"").replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">"))
}

static CACHE: std::sync::Mutex<Vec<(String, Option<String>)>> = std::sync::Mutex::new(Vec::new());

/// Facts to answer the question with, or None when it doesn't need (or couldn't get) any.
/// The same question again (a follow-up, a retry) is answered from memory instantly.
pub fn lookup(q: &str) -> Option<String> {
    let k = q.to_lowercase().trim().trim_end_matches(['?', '.', '!']).to_string();
    if let Some((_, v)) = CACHE.lock().unwrap().iter().find(|(c, _)| *c == k) {
        return v.clone();
    }
    let v = lookup_now(q);
    let mut c = CACHE.lock().unwrap();
    c.push((k, v.clone()));
    if c.len() > 60 {
        c.remove(0);
    }
    v
}

fn lookup_now(q: &str) -> Option<String> {
    if let Some(w) = weather(q) {
        return Some(w);
    }
    let (s, kind) = subject(q)?;
    let s1 = s.clone();
    let w = std::thread::spawn(move || wiki(&s1));
    let n = kind.news.then(|| {
        let s2 = s.clone();
        std::thread::spawn(move || headlines(&s2))
    });
    let mut parts: Vec<String> = Vec::new();
    if let Some(n) = n.and_then(|h| h.join().ok().flatten()) {
        parts.push(n);
    }
    if let Some(w) = w.join().ok().flatten() {
        parts.push(w);
    }
    crate::log_line(&format!("web lookup “{s}”: {} sources", parts.len()));
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn subjects() {
        for q in ["Tell me about the Junko case", "What was the Junko case?", "junko case", "who is Nana Patekar", "what is a black hole", "what's the time",
            "how are you", "what is your name", "tell me a joke", "explain photosynthesis", "what's 5 + 5", "what is on my screen"] {
            println!("{q:?} -> {:?}", subject(q).map(|s| s.0));
        }
        assert_eq!(subject("Tell me about the Junko case").map(|s| s.0).as_deref(), Some("junko case"));
        assert!(subject("what is your name").is_none() && subject("how are you").is_none());
        println!("{:?}", wiki("junko case").map(|w| w.chars().take(80).collect::<String>()));
    }
}
