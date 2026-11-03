//! Web answers, from the web's open sources: Wikipedia (its search and the opening of the articles it finds) and
//! DuckDuckGo's Instant Answer API, asked at the same time. Both are free, need no key, and are made to be asked by
//! apps; nothing is read off a search engine's pages. Only the lines that bear on the question are kept, a few hundred
//! characters, and the sentence among them that answers it is said as it is (or, failing that, the brain answers from
//! them). With no internet, nothing waits on it: the local brain answers at once.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{mpsc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tauri::AppHandle;

/// The last look found no internet (neither source could be reached)
pub static OFFLINE: AtomicBool = AtomicBool::new(false);
/// What was found for each question in the last ten minutes
static SEEN: LazyLock<Mutex<std::collections::HashMap<String, (Instant, String)>>> = LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));
/// (one client, its connections kept open between questions: the second question skips the handshakes)
static NET: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(2600))
        .timeout_connect(Duration::from_millis(2500))
        .user_agent(&format!("SYLVER/{} (desktop assistant; install {})", env!("CARGO_PKG_VERSION"), install_id()))
        .build()
});

/// This PC's own id for the web's sources (made once, at random, and kept beside SYLVER's settings)
fn install_id() -> String {
    let dir = std::path::PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join("com.ultron.notch");
    let f = dir.join("install-id");
    if let Ok(s) = std::fs::read_to_string(&f) {
        if s.trim().len() >= 8 {
            return s.trim().to_string();
        }
    }
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0) ^ (std::process::id() as u128) << 64;
    let id = format!("{:016x}", (seed as u64) ^ ((seed >> 64) as u64).rotate_left(29) ^ 0x9e37_79b9_7f4a_7c15);
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&f, &id);
    id
}

pub struct Facts {
    pub text: String,
    pub direct: Option<String>,
    /// the article the answer came from (for its picture)
    pub source: Option<String>,
}

/// The article a sentence came from, among the "Title\tbody" lines
fn source_of(text: &str, said: &str) -> Option<String> {
    let head: String = said.chars().take(40).collect();
    text.lines().find_map(|l| l.split_once('\t').filter(|(_, b)| unparen(b).contains(&head)).map(|(t, _)| t.to_string()))
}

/// "define ubiquitous", "what does ubiquitous mean", "meaning of serendipity", "serendipity meaning": the word asked about
pub fn word_asked(q: &str) -> Option<String> {
    let l = q.trim().trim_end_matches(['?', '.', '!']).trim().to_lowercase();
    let ok = |w: &str| {
        let w = w.trim().trim_matches(['"', '\'', '“', '”']);
        (!w.is_empty() && w.split_whitespace().count() <= 2 && w.chars().all(|c| c.is_alphabetic() || c == ' ' || c == '-' || c == '\'')).then(|| w.to_string())
    };
    for (a, b) in [("define ", ""), ("definition of ", ""), ("what is the meaning of ", ""), ("what's the meaning of ", ""), ("meaning of ", ""), ("what does ", " mean"),
        ("what is the definition of ", ""), ("synonyms for ", ""), ("synonyms of ", ""), ("synonym for ", ""), ("synonym of ", "")] {
        if let Some(rest) = l.strip_prefix(a) {
            if let Some(w) = rest.strip_suffix(b).and_then(ok) {
                return Some(w);
            }
        }
    }
    l.strip_suffix(" meaning").or_else(|| l.strip_suffix(" definition")).and_then(ok)
}

/// A dictionary's entry for a word (dictionaryapi.dev, free and open): what's said, and the card the page draws
pub fn word(w: &str) -> Option<(String, Value)> {
    let v = json(&format!("https://api.dictionaryapi.dev/api/v2/entries/en/{}", enc(w))).ok()?;
    let e = v.get(0)?;
    let word = e["word"].as_str()?.to_string();
    let phons = e["phonetics"].as_array().cloned().unwrap_or_default();
    let phon = e["phonetic"].as_str().filter(|s| !s.is_empty()).map(String::from)
        .or_else(|| phons.iter().find_map(|p| p["text"].as_str().filter(|s| !s.is_empty()).map(String::from))).unwrap_or_default();
    let audio = phons.iter().filter_map(|p| p["audio"].as_str().filter(|s| !s.is_empty())).max_by_key(|a| a.ends_with("-us.mp3") as u8 * 2 + a.ends_with("-uk.mp3") as u8).unwrap_or("").to_string();
    let mut meanings = Vec::new();
    for m in e["meanings"].as_array().into_iter().flatten().take(3) {
        let defs: Vec<Value> = m["definitions"].as_array().into_iter().flatten().take(3)
            .filter_map(|d| Some(serde_json::json!({"d": d["definition"].as_str()?, "ex": d["example"].as_str().unwrap_or("")}))).collect();
        let mut syn: Vec<String> = m["synonyms"].as_array().into_iter().flatten().filter_map(|s| s.as_str().map(String::from)).collect();
        for d in m["definitions"].as_array().into_iter().flatten() {
            syn.extend(d["synonyms"].as_array().into_iter().flatten().filter_map(|s| s.as_str().map(String::from)));
        }
        let mut seen = HashSet::new();
        syn.retain(|s| seen.insert(s.to_lowercase()));
        syn.truncate(6);
        if !defs.is_empty() {
            meanings.push(serde_json::json!({"pos": m["partOfSpeech"].as_str().unwrap_or(""), "defs": defs, "syn": syn}));
        }
    }
    let first = meanings.first()?;
    let say = format!("{} ({}): {}", word.chars().next()?.to_uppercase().collect::<String>() + &word[word.chars().next()?.len_utf8()..], first["pos"].as_str().unwrap_or(""), first["defs"][0]["d"].as_str().unwrap_or(""));
    crate::log_line(&format!("web: the dictionary's entry for {word:?}"));
    Some((say, serde_json::json!({"kind": "word", "word": word, "phon": phon, "audio": audio, "meanings": meanings})))
}

/// Wikipedia's picture for the article an answer came from, for animals, people, places and things (not for ideas,
/// processes or lists, where a picture adds nothing)
pub fn thing(title: &str) -> Option<Value> {
    let summary = |t: &str| json(&format!("https://en.wikipedia.org/api/rest_v1/page/summary/{}", enc(&t.replace(' ', "_")))).ok().filter(|v| v["type"].as_str() == Some("standard"));
    // (a name as another source writes it, "Red Panda": Wikipedia's search finds the article, "Red panda")
    let v = summary(title).or_else(|| {
        let s = json(&format!("https://en.wikipedia.org/w/api.php?action=query&list=search&srsearch={}&srlimit=1&format=json", enc(title))).ok()?;
        summary(s["query"]["search"][0]["title"].as_str()?)
    })?;
    let desc = v["description"].as_str().unwrap_or("");
    let d = desc.to_lowercase();
    if ["concept", "process", "theory", "field of", "branch of", "method", "technique", "study of", "list of", "term ", "phenomenon", "philosophy", "idea", "word", "letter",
        "number", "unit of", "mathematical", "property", "disambiguation", "abstract"].iter().any(|a| d.contains(a)) {
        return None;
    }
    let t = &v["thumbnail"];
    let src = t["source"].as_str()?;
    Some(serde_json::json!({"kind": "thing", "title": v["title"], "desc": desc, "img": src, "w": t["width"], "h": t["height"]}))
}

/// The connections to both opened ahead, as the notch opens or typing starts (a question then saves the handshakes)
pub fn warm(_app: &AppHandle) {
    for u in ["https://en.wikipedia.org/w/api.php?action=query&meta=siteinfo&format=json", "https://api.duckduckgo.com/?q=ultron&format=json&no_html=1"] {
        let _ = NET.get(u).call();
    }
}

fn json(url: &str) -> Result<Value, bool> {
    // (Err(true): no answer at all, the internet may be down; Err(false): an answer, but not one to use)
    let r = NET.get(url).call().map_err(|e| matches!(e, ureq::Error::Transport(_)))?;
    r.into_json().map_err(|_| false)
}

/// What a question is about, for searching: "what is the capital of Australia?" -> "capital of Australia"
fn subject(q: &str) -> String {
    let mut s = q.trim().trim_end_matches(['?', '.', '!']).trim().to_string();
    for lead in ["who is", "who was", "who are", "who wrote", "what is", "what's", "what are", "what was", "where is", "where are", "when was", "when did", "when is",
        "how tall is", "how old is", "how big is", "how far is", "how many", "how much", "tell me about", "define", "the", "a", "an"] {
        let l = s.to_lowercase();
        if l.starts_with(lead) && l[lead.len()..].starts_with(' ') && !(lead.starts_with("who wrote") || lead.starts_with("how ")) {
            s = s[lead.len()..].trim().to_string();
        }
    }
    if s.is_empty() { q.trim().to_string() } else { s }
}

/// Wikipedia: the three articles its search finds first, and the opening of each
fn wiki(q: &str) -> Result<Option<Facts>, bool> {
    let url = format!("https://en.wikipedia.org/w/api.php?action=query&format=json&formatversion=2&generator=search&gsrsearch={}&gsrlimit=3&prop=extracts&exintro=1&explaintext=1&exsentences=8&redirects=1", enc(&subject(q)));
    let v = json(&url)?;
    let mut pages: Vec<&Value> = v["query"]["pages"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    pages.sort_by_key(|p| p["index"].as_i64().unwrap_or(99));
    let lines: Vec<String> = pages.iter().filter_map(|p| Some(format!("{}\t{}", p["title"].as_str()?, p["extract"].as_str()?.replace('\n', " ")))).collect();
    // (all three openings kept: the answer is often the second article's first line)
    let text = boil(q, &lines.join("\n"), 1800);
    Ok((!text.is_empty()).then(|| {
        let direct = answer_in(q, &text);
        Facts { source: direct.as_deref().and_then(|d| source_of(&text, d)), direct, text }
    }))
}

/// DuckDuckGo's Instant Answer API: an answer it has outright (a sum, a conversion), or its summary of the subject
fn ddg(q: &str) -> Result<Option<Facts>, bool> {
    let v = json(&format!("https://api.duckduckgo.com/?q={}&format=json&no_html=1&skip_disambig=1&t=ultron", enc(&subject(q))))?;
    let direct = v["Answer"].as_str().filter(|a| !a.is_empty() && a.len() < 300).map(|a| a.to_string());
    let mut lines = Vec::new();
    if let (Some(h), Some(a)) = (v["Heading"].as_str(), v["AbstractText"].as_str()) {
        if !a.is_empty() {
            lines.push(format!("{h}\t{a}"));
        }
    }
    if let Some(d) = v["Definition"].as_str().filter(|d| !d.is_empty()) {
        lines.push(d.to_string());
    }
    for t in v["RelatedTopics"].as_array().into_iter().flatten().take(3) {
        if let Some(s) = t["Text"].as_str() {
            lines.push(s.to_string());
        }
    }
    let text = boil(q, &lines.join("\n"), 700);
    if text.is_empty() && direct.is_none() {
        return Ok(None);
    }
    let direct = direct.or_else(|| answer_in(q, &text));
    let source = v["Heading"].as_str().filter(|h| !h.is_empty() && direct.is_some()).map(String::from);
    Ok(Some(Facts { direct, text, source }))
}

/// The web's facts for a question: Wikipedia and DuckDuckGo asked at once. The first with a sentence that answers it
/// is used; failing that, Wikipedia's lines if they came, else DuckDuckGo's.
pub fn facts(_app: &AppHandle, q: &str) -> Option<Facts> {
    let key = q.trim().to_lowercase();
    if let Some((at, text)) = SEEN.lock().unwrap().get(&key) {
        if at.elapsed() < Duration::from_secs(600) {
            let direct = answer_in(q, text);
            return Some(Facts { source: direct.as_deref().and_then(|d| source_of(text, d)), direct, text: text.clone() });
        }
    }
    let t0 = Instant::now();
    let (tx, rx) = mpsc::channel::<(bool, Result<Option<Facts>, bool>)>();
    for wiki_side in [true, false] {
        let (tx, q) = (tx.clone(), q.to_string());
        std::thread::spawn(move || {
            let _ = tx.send((wiki_side, if wiki_side { wiki(&q) } else { ddg(&q) }));
        });
    }
    drop(tx);
    let (mut kept, mut from_wiki, mut down) = (None::<Facts>, false, 0);
    while let Some(left) = Duration::from_millis(2800).checked_sub(t0.elapsed()) {
        let Ok((is_wiki, r)) = rx.recv_timeout(left) else { break };
        let f = match r {
            Ok(Some(f)) => f,
            Ok(None) => continue,
            Err(no_net) => {
                down += usize::from(no_net);
                continue;
            }
        };
        if f.direct.is_some() {
            kept = Some(f);
            break;
        }
        if kept.is_none() || (is_wiki && !from_wiki) {
            from_wiki = is_wiki;
            kept = Some(f);
        }
    }
    OFFLINE.store(down == 2, SeqCst);
    crate::log_line(&format!("web: {} in {} ms{}", if kept.is_some() { "found" } else { "nothing found" }, t0.elapsed().as_millis(), if kept.as_ref().is_some_and(|f| f.direct.is_some()) { ", a sentence that answers it" } else { "" }));
    if let Some(f) = &kept {
        let mut seen = SEEN.lock().unwrap();
        seen.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(600));
        seen.insert(key, (Instant::now(), f.text.clone()));
    }
    kept
}

fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else if b == b' ' { "+".into() } else { format!("%{b:02X}") }).collect()
}

/// The lines of the results that bear on the question, in their order, about `most` characters in all (Google's own
/// answers, first on the page, count for a little more).
fn boil(q: &str, text: &str, most: usize) -> String {
    const NOISE: [&str; 9] = ["People also ask", "Videos", "Images", "Feedback", "Translate this page", "Missing:", "Show more", "Related searches", "Top stories"];
    let keys: Vec<String> = q.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 2 && !["the", "and", "what", "who", "how", "was", "are", "for", "does", "did", "with", "this", "that", "from", "when", "where", "which", "why", "much", "many"].contains(w)).map(String::from).collect();
    let mut seen = HashSet::new();
    let mut lines: Vec<(usize, f32, String)> = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let l = raw.trim();
        if l.chars().count() < 25 || NOISE.iter().any(|n| l.starts_with(n)) || l.contains(" › ") || !seen.insert(l.to_lowercase()) {
            continue;
        }
        let low = l.to_lowercase();
        let score = keys.iter().filter(|k| low.contains(k.as_str())).count() as f32 + if i < 3 { 1.5 } else { 0.0 };
        lines.push((i, score, l.chars().take(600).collect()));
    }
    let mut best = lines.clone();
    best.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut keep = HashSet::new();
    let mut size = 0;
    for (i, _, l) in &best {
        if size + l.len() > most && size > 0 {
            continue;
        }
        size += l.len();
        keep.insert(*i);
    }
    lines.into_iter().filter(|(i, _, _)| keep.contains(i)).map(|(_, _, l)| l).collect::<Vec<_>>().join("\n")
}

/// Is there internet at all (asked before saying there isn't: Brave's page failing to fetch isn't always that)
pub fn online() -> bool {
    ["1.1.1.1:443", "8.8.8.8:53"].iter().any(|a| a.parse().is_ok_and(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_millis(700)).is_ok()))
}

/// The sentences in a piece of text (not cut at "Dr." or "U.S.")
fn sentences(t: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (b, mut from) = (t.as_bytes(), 0);
    for i in 0..b.len() {
        if matches!(b[i], b'.' | b'!' | b'?') && b.get(i + 1) == Some(&b' ') && b.get(i + 2).is_some_and(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'"') {
            let word = t[from..i].rsplit(' ').next().unwrap_or("");
            if word.len() >= 3 && !word.contains('.') && !["Mr", "Mrs", "Ms", "Dr", "St", "Jr", "Sr", "vs", "No"].contains(&word) {
                out.push(t[from..=i].trim());
                from = i + 1;
            }
        }
    }
    if from < t.len() {
        out.push(t[from..].trim());
    }
    out
}

/// A sentence without what's in brackets (a pronunciation, "ⓘ", a footnote mark): said aloud, they're noise
fn unparen(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0;
    for c in s.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ").replace(" ,", ",").replace(" .", ".").replace(" ;", ";")
}

/// The sentence in the results that answers the question, said as it is when one clearly does: at once, with no wait
/// on the brain (on a laptop holding its graphics card back, the brain took ten seconds to retell it, and sometimes
/// got it wrong). None: nothing answers it clearly, and the brain reads the results instead.
pub fn answer_in(q: &str, text: &str) -> Option<String> {
    const SKIP: [&str; 53] = ["the", "and", "what", "who", "how", "was", "were", "are", "is", "for", "does", "did", "with", "this", "that", "from", "when", "where",
        "which", "why", "much", "many", "tell", "about", "me", "you", "your", "can", "could", "please", "of", "in", "on", "to", "do", "it", "its", "whats",
        "define", "meaning", "mean", "an", "a", "be", "book", "books", "novel", "novels", "movie", "film", "song", "series", "name"];
    // (one word for the forms a question and its answer put it in: "who wrote" is answered by "written by", "author")
    let stem = |w: &str| -> String {
        let w = w.trim_end_matches("'s").trim_end_matches('’');
        let w = if w.len() > 3 { w.trim_end_matches('s') } else { w };
        match w {
            "wrote" | "write" | "written" | "writer" | "author" | "authored" => "write",
            "directed" | "director" | "direct" => "direct",
            "invented" | "inventor" | "invent" | "invention" => "invent",
            "founded" | "founder" | "found" => "found",
            "discovered" | "discoverer" | "discover" | "discovery" => "discover",
            "painted" | "painter" | "paint" => "paint",
            "sang" | "sung" | "singer" | "sing" => "sing",
            "built" | "builder" | "build" => "build",
            "born" | "birth" => "born",
            "tall" | "height" | "high" | "elevation" | "altitude" => "height",
            "chief" | "ceo" => "ceo",
            "died" | "die" | "death" | "dead" => "die",
            w => w,
        }
        .to_string()
    };
    let ql = q.to_lowercase().replace('’', "'");
    let keys: Vec<String> = ql.split(|c: char| !c.is_alphanumeric() && c != '\'').filter(|w| w.len() > 1 && !SKIP.contains(w)).map(stem).collect();
    if keys.is_empty() || keys.len() > 9 {
        return None;
    }
    let num = ["how many", "how much", "how old", "how long", "how far", "how tall", "how big", "what year", "when ", "population", "price", "cost", "age of"].iter().any(|w| ql.contains(w));
    // ("who is the president of France", "what is the capital of Australia": asks for someone or something by its
    //  role; "who was Albert Einstein", "what is photosynthesis": asks what something is)
    let role = ["who is the ", "who was the ", "what is the ", "what's the ", "what was the ", "who's the "].iter().any(|r| ql.starts_with(r));
    let define = ql.starts_with("what is") || ql.starts_with("what are") || ql.starts_with("what's") || ql.contains("meaning") || ql.starts_with("define") || ql.starts_with("who is") || ql.starts_with("who was");
    let mut best: Option<(f32, String, Option<String>)> = None;
    for (rank, line) in text.lines().enumerate() {
        let body = line.split_once('\t').map_or(line, |(_, b)| b);
        for (k, s) in sentences(body).into_iter().enumerate() {
            let s = unparen(s);
            let s = s.as_str();
            let n = s.chars().count();
            // (a whole sentence only: a line cut short ends in a piece of one)
            if !(20..=280).contains(&n) || s.contains("...") || s.contains('…') || s.ends_with('?') || s.contains(" | ") || !s.ends_with(['.', '!', '"', ')']) {
                continue;
            }
            let low = s.to_lowercase();
            let words: Vec<String> = low.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(stem).collect();
            let hit = keys.iter().filter(|k| words.contains(k)).count() as f32;
            let cover = hit / keys.len() as f32;
            // (every idea in the question there, when it has more than one: a sentence about half of it answers the
            //  wrong question, and a wrong sentence said at once is worse than a right one a few seconds later)
            if cover < if keys.len() >= 2 { 0.99 } else { 0.6 } {
                continue;
            }
            let digits = s.chars().any(|c| c.is_ascii_digit());
            let mut score = cover * 3.0 + hit * 0.3 - rank as f32 * 0.35 - k as f32 * 0.25;
            if num {
                if !digits {
                    continue;
                }
                score += 0.8;
            }
            // ("the capital of Australia": "Canberra is the capital city of Australia", the answer before "is" and the
            //  question after it, not "The Australian Capital Territory is an internal territory of Australia")
            // ("the largest planet": not "the second largest")
            if ["second", "third", "fourth", "fifth"].iter().any(|o| !ql.contains(o) && keys.iter().any(|k| low.contains(&format!("{o} {k}")) || low.contains(&format!("{o}-{k}")))) {
                continue;
            }
            if keys.len() >= 2 && role {
                if let Some(at) = [" is ", " was ", " are ", " were "].iter().filter_map(|v| low.find(v)).min() {
                    let count = |part: &str| keys.iter().filter(|k| part.split(|c: char| !c.is_alphanumeric()).any(|w| stem(w) == **k)).count();
                    let (subj, pred) = (count(&low[..at]), count(&low[at..]));
                    if pred < keys.len() {
                        continue;
                    }
                    if subj == 0 {
                        score += 1.2;
                    }
                }
            }
            if define && (low.contains(" is ") || low.contains(" are ") || low.contains(" was ") || low.contains(" refers to ")) {
                score += 0.5;
                // ("what is photosynthesis": "Photosynthesis is ...", the thing itself first)
                if keys.len() == 1 && words.first().is_some_and(|w| *w == keys[0]) {
                    score += 0.6;
                }
            }
            if !s.starts_with(|c: char| c.is_uppercase() || c.is_ascii_digit()) {
                score -= 1.0;
            }
            // ("He was the second person to walk on the Moon": true, and no answer to who)
            let first = low.split_whitespace().next().unwrap_or("");
            if ["he", "she", "it", "they", "this", "these", "those", "his", "her", "its", "their", "that", "there"].contains(&first) {
                continue;
            }
            // (a site's blurb about itself: "Learn about the history, powers and duties of ...")
            if ["learn", "find", "discover", "read", "explore", "see", "get", "check", "click", "watch", "browse", "visit", "shop", "search", "view", "sign", "join",
                "download", "subscribe", "welcome", "here"].contains(&first) {
                continue;
            }
            if n > 180 {
                score -= 0.5;
            }
            // ("who is the president of France": a sentence that names someone, "Emmanuel Macron is ..." or "The current
            //  president is Emmanuel Macron", not "The president of France is the co-prince of Andorra")
            if role && ql.starts_with("who") {
                let toks: Vec<&str> = s.split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '.')).collect();
                let cap = |w: &str| w.starts_with(|c: char| c.is_uppercase()) && !["The", "A", "An", "In", "On", "This", "As", "It", "He", "She"].contains(&w);
                let named_at = toks.windows(2).position(|p| cap(p[0]) && cap(p[1]) && !keys.contains(&stem(&p[0].to_lowercase())) && !keys.contains(&stem(&p[1].to_lowercase())));
                let Some(at) = named_at else { continue };
                score += if at == 0 { 1.0 } else { 0.6 };
            }
            if best.as_ref().map_or(true, |b| score > b.0) {
                // (and the sentence after it, a little more of the answer, when it's a whole one of a reasonable length)
                let next = sentences(body).get(k + 1).map(|n| unparen(n)).filter(|n| (20..=220).contains(&n.chars().count()) && n.ends_with(['.', '!'])
                    && !n.contains("...") && n.starts_with(|c: char| c.is_uppercase() || c.is_ascii_digit()));
                best = Some((score, s.to_string(), next));
            }
        }
    }
    let (score, mut s, next) = best.filter(|b| b.0 >= 1.8)?;
    // (the little marks results carry: "[1]", a trailing dash or colon)
    while let Some(a) = s.rfind('[').filter(|&a| s[a..].len() <= 5 && s.ends_with(']')) {
        s.truncate(a);
    }
    let s = s.trim().trim_end_matches([',', ';', ':', '-', '—']).trim().to_string();
    let s = if s.ends_with(['.', '!']) { s } else { s + "." };
    let s = match next { Some(n) => format!("{s} {n}"), None => s };
    crate::log_line(&format!("web: answered from the results themselves (sure {score:.1})"));
    Some(s)
}
