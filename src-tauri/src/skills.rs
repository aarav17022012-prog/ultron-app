//! More things ULTRON can do on the PC, used by the agent: messaging (always asks
//! before sending), email drafts, writing tools on selected text, reading the
//! screen, timers and reminders, window snapping, brightness, dark mode, Settings
//! pages and files. Everything runs locally through Windows itself.

use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;

const NO_WINDOW: u32 = 0x0800_0000;

/* ---------------- understanding requests ---------------- */

fn lower(s: &str) -> String {
    s.to_ascii_lowercase()
}

/// "5 minutes", "an hour and a half", "90 seconds", "half an hour" -> seconds
pub fn duration(s: &str) -> Option<u64> {
    let l = lower(s).replace("half an hour", "30 minutes").replace("an hour and a half", "90 minutes").replace("a minute", "1 minute")
        .replace("an hour", "1 hour").replace("a second", "1 second").replace("a couple of", "2").replace("a few", "3");
    let words = [("one", 1.0), ("two", 2.0), ("three", 3.0), ("four", 4.0), ("five", 5.0), ("six", 6.0), ("seven", 7.0), ("eight", 8.0), ("nine", 9.0),
        ("ten", 10.0), ("eleven", 11.0), ("twelve", 12.0), ("thirteen", 13.0), ("fourteen", 14.0), ("fifteen", 15.0), ("sixteen", 16.0), ("seventeen", 17.0),
        ("eighteen", 18.0), ("nineteen", 19.0), ("twenty", 20.0), ("thirty", 30.0), ("forty", 40.0), ("fifty", 50.0), ("sixty", 60.0)];
    let toks: Vec<&str> = l.split(|c: char| c.is_whitespace() || c == ',').filter(|t| !t.is_empty()).collect();
    let mut total = 0.0;
    let mut i = 0;
    while i < toks.len() {
        let n = toks[i].parse::<f64>().ok().or_else(|| words.iter().find(|(w, _)| *w == toks[i]).map(|(_, v)| *v));
        if let (Some(n), Some(unit)) = (n, toks.get(i + 1)) {
            let mult = if unit.starts_with("sec") { 1.0 } else if unit.starts_with("min") { 60.0 } else if unit.starts_with("hour") || unit.starts_with("hr") { 3600.0 } else { 0.0 };
            if mult > 0.0 {
                total += n * mult;
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    (total >= 1.0).then_some(total as u64)
}

fn clock_number(s: &str) -> Option<u32> {
    s.parse().ok().or_else(|| {
        [
            ("zero", 0), ("one", 1), ("two", 2), ("three", 3), ("four", 4), ("five", 5), ("six", 6), ("seven", 7), ("eight", 8), ("nine", 9),
            ("ten", 10), ("eleven", 11), ("twelve", 12), ("thirteen", 13), ("fourteen", 14), ("fifteen", 15), ("sixteen", 16), ("seventeen", 17),
            ("eighteen", 18), ("nineteen", 19), ("twenty", 20), ("thirty", 30), ("forty", 40), ("fifty", 50),
        ]
        .iter()
        .find(|(word, _)| *word == s)
        .map(|(_, value)| *value)
    })
}

/// "7 pm", "seven thirty", "7:30am", "19:00" -> seconds from now until then
pub fn until_clock(s: &str) -> Option<u64> {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let l = lower(s).replace("a.m.", "am").replace("p.m.", "pm").replace([',', '!', '?', ';', '.'], " ");
    let t = l.split_whitespace().collect::<Vec<_>>();
    let daypart = if t.iter().any(|w| ["pm", "evening", "tonight", "afternoon"].contains(w)) { Some(true) }
        else if t.iter().any(|w| ["am", "morning"].contains(w)) { Some(false) }
        else { None };
    for (i, w) in t.iter().enumerate() {
        let (num, mut ampm) = if let Some(n) = w.strip_suffix("am") { (n, Some(false)) } else if let Some(n) = w.strip_suffix("pm") { (n, Some(true)) } else { (*w, None) };
        if ampm.is_none() {
            let next = t.get(i + 1).copied().unwrap_or("");
            let after_next = t.get(i + 2).copied().unwrap_or("");
            ampm = if next == "am" || next == "pm" { Some(next == "pm") }
                else if after_next == "am" || after_next == "pm" { Some(after_next == "pm") }
                else { daypart };
        }
        let next = t.get(i + 1).copied().unwrap_or("");
        let after_next = t.get(i + 2).copied().unwrap_or("");
        let parsed = if let Some((h, m)) = num.split_once(':') {
            h.parse::<u32>().ok().zip(m.parse::<u32>().ok())
        } else if let Some(h) = clock_number(num) {
            let spoken_minute = clock_number(next).filter(|m| *m < 60);
            if let Some(m) = spoken_minute.filter(|_| matches!(after_next, "am" | "pm") || daypart.is_some()) {
                Some((h, m))
            } else if num.bytes().all(|b| b.is_ascii_digit()) && (3..=4).contains(&num.len()) {
                num.parse::<u32>().ok().and_then(|hm| ((100..=2359).contains(&hm) && hm % 100 < 60).then_some((hm / 100, hm % 100)))
            } else {
                let cue = i > 0 && matches!(t[i - 1], "at" | "for");
                let duration_unit = next.starts_with("sec") || next.starts_with("min") || next.starts_with("hour") || next.starts_with("hr");
                (h < 24 && !duration_unit && (ampm.is_some() || l.contains("o'clock") || cue || daypart.is_some())).then_some((h, 0))
            }
        } else {
            None
        };
        let Some((h, m)) = parsed else { continue };
        if m >= 60 || h > 23 || (ampm.is_some() && !(1..=12).contains(&h)) { continue; }
        let mut h = h % 24;
        match ampm {
            Some(true) if h < 12 => h += 12,
            Some(false) if h == 12 => h = 0,
            _ => {}
        }
        let st = unsafe { GetLocalTime() };
        let now = st.wHour as i64 * 3600 + st.wMinute as i64 * 60 + st.wSecond as i64;
        let mut d = h as i64 * 3600 + m as i64 * 60 - now;
        if t.contains(&"tomorrow") {
            d += 86400;
        } else if d <= 0 {
            if t.contains(&"today") { return None; }
            d += 86400;
        }
        return Some(d as u64);
    }
    None
}

/// The time of day `secs` from now, as hour (0-23) and minute, rounded up to Clock's minute precision.
pub fn clock_after(secs: u64) -> (u32, u32) {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let st = unsafe { GetLocalTime() };
    let now = st.wHour as u64 * 3600 + st.wMinute as u64 * 60 + st.wSecond as u64;
    let minute = now.saturating_add(secs).saturating_add(59) / 60 % 1440;
    ((minute / 60) as u32, (minute % 60) as u32)
}

#[cfg(test)]
mod clock_tests {
    // (what's checked is the time of day it comes to, not how far off that is)
    fn at(s: &str) -> (u64, u64) {
        let d = super::until_clock(s).unwrap();
        let st = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
        let t = (st.wHour as u64 * 3600 + st.wMinute as u64 * 60 + st.wSecond as u64 + d) % 86400;
        (t / 3600, t % 3600 / 60)
    }
    #[test]
    fn times_of_day() {
        assert_eq!(at("set an alarm for 11:57 pm"), (23, 57));
        assert_eq!(at("set an alarm for 11 57 pm"), (23, 57));
        assert_eq!(at("set an alarm for 1157 pm"), (23, 57));
        assert_eq!(at("set an alarm for seven thirty pm"), (19, 30));
        assert_eq!(at("wake me up at 6 am"), (6, 0));
        assert_eq!(at("set an alarm for 7:05am"), (7, 5));
        assert_eq!(at("set an alarm for 12 pm"), (12, 0));
        assert_eq!(at("set an alarm for 12 30 am"), (0, 30));
        assert_eq!(at("set an alarm for 7 tomorrow morning"), (7, 0));
        assert!(super::until_clock("set an alarm for 7:60 pm").is_none());
    }
}

pub fn human(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    let mut p = Vec::new();
    if h > 0 {
        p.push(format!("{h} hour{}", if h == 1 { "" } else { "s" }));
    }
    if m > 0 {
        p.push(format!("{m} minute{}", if m == 1 { "" } else { "s" }));
    }
    if s > 0 && h == 0 {
        p.push(format!("{s} second{}", if s == 1 { "" } else { "s" }));
    }
    p.join(" ")
}

/// A request as it was meant: common slips of the keyboard and chat shorthand set right ("opne whatsap n mesage
/// danish pls" reads as "open whatsapp and message danish"). Only known slips are changed, never a real word.
pub fn tidy(raw: &str) -> String {
    const FIX: &[(&str, &str)] = &[
        ("opne", "open"), ("oepn", "open"), ("opn", "open"), ("colse", "close"), ("clsoe", "close"), ("serach", "search"), ("seach", "search"), ("saerch", "search"), ("serch", "search"),
        ("mesage", "message"), ("messge", "message"), ("mesege", "message"), ("messgae", "message"), ("msg", "message"), ("plya", "play"), ("paly", "play"), ("wirte", "write"), ("wrtie", "write"),
        ("tpye", "type"), ("tyep", "type"), ("clcik", "click"), ("clikc", "click"), ("sned", "send"), ("snd", "send"), ("tranlate", "translate"), ("translte", "translate"), ("transalte", "translate"),
        ("sumarize", "summarize"), ("summerize", "summarize"), ("summarise", "summarize"), ("remnd", "remind"), ("remid", "remind"), ("craete", "create"), ("creat", "create"), ("delte", "delete"),
        ("dlete", "delete"), ("renmae", "rename"), ("dowload", "download"), ("donwload", "download"), ("instal", "install"), ("lauch", "launch"), ("lanch", "launch"), ("youtub", "youtube"),
        ("utube", "youtube"), ("yt", "youtube"), ("whatsap", "whatsapp"), ("watsapp", "whatsapp"), ("whatsaap", "whatsapp"), ("whatapp", "whatsapp"), ("gogle", "google"), ("googel", "google"),
        ("chorme", "chrome"), ("crome", "chrome"), ("spotfy", "spotify"), ("spotifi", "spotify"), ("notpad", "notepad"), ("emial", "email"), ("emal", "email"),
        ("wether", "weather"), ("wheather", "weather"), ("tommorow", "tomorrow"), ("tomorow", "tomorrow"), ("tmrw", "tomorrow"), ("remeber", "remember"), ("rember", "remember"),
    ];
    // ("n" is "and" only when the request goes on with something to do: "open whatsapp n message danish", never "rice n dal" or "press ctrl n")
    const VERBS: &[&str] = &[
        "open", "close", "search", "message", "play", "write", "type", "click", "send", "translate", "summarize", "remind", "create", "delete", "rename", "download", "install", "launch", "tell", "go", "make",
        "text", "find", "show", "set", "turn", "start", "stop", "then",
    ];
    let bare = |w: &str| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string();
    let fixed = |w: &str| -> String {
        let core = bare(w);
        match FIX.iter().find(|(a, _)| *a == core.to_lowercase()) {
            Some((_, b)) => w.replace(&core, b),
            None => w.to_string(),
        }
    };
    let words: Vec<&str> = raw.split_whitespace().collect();
    let (mut out, mut quoted): (Vec<String>, bool) = (Vec::new(), false);
    for (i, w) in words.iter().enumerate() {
        // (what's written between quotes is theirs word for word)
        let edge = w.matches('"').count() % 2 == 1;
        if quoted || edge {
            out.push(w.to_string());
            quoted ^= edge;
            continue;
        }
        let l = bare(w).to_lowercase();
        if (i == 0 || i + 1 == words.len()) && words.len() > 1 && ["pls", "plz", "plss", "pleas", "plese"].contains(&l.as_str()) {
            continue; // ("please" at either end changes nothing about what's wanted)
        }
        if *w == "n" && i > 0 && words.get(i + 1).is_some_and(|n| VERBS.contains(&bare(&fixed(n)).to_lowercase().as_str())) {
            out.push("and".into());
            continue;
        }
        out.push(fixed(w));
    }
    out.join(" ")
}

/// Who and what for messages: "message priya on whatsapp saying I'll be late", "text mom that dinner's ready",
/// "send "on my way" to rahul on telegram", "tell sam on discord: gg", "message Danish "Hello"".
pub fn message(raw: &str) -> Option<(String, String, String)> {
    let raw = raw.trim().trim_end_matches('.').trim().replace(['“', '”', '„'], "\"").replace(['‘', '’'], "'");
    let l = lower(&raw);
    let apps = ["whatsapp", "telegram", "discord"];
    let mut app = None;
    let mut body = raw.clone();
    let mut bl = l.clone();
    for a in apps {
        for p in [format!(" on {a}"), format!(" via {a}"), format!(" in {a}"), format!(" using {a}"), format!(" through {a}")] {
            if let Some(i) = bl.find(&p) {
                app = Some(a.to_string());
                body = format!("{}{}", &body[..i], &body[i + p.len()..]);
                bl = lower(&body);
                break;
            }
        }
    }
    let verbs = ["send a whatsapp message to ", "send a whatsapp to ", "send a message to ", "send a text to ", "send message to ", "send a msg to ", "message to ", "message ", "msg ", "text ",
        "whatsapp ", "dm ", "ping ", "tell "];
    let (to, text) = match verbs.iter().find_map(|v| bl.strip_prefix(v).map(|_| (*v, body[v.len()..].to_string()))) {
        Some((verb, rest)) => {
            if verb.contains("whatsapp") {
                app.get_or_insert("whatsapp".into());
            }
            if verb == "tell " && app.is_none() {
                return None; // "tell me a joke" isn't a message
            }
            match who_what(&rest) {
                Some(x) => x,
                // just a name ("message Danish"): the chat is opened and ULTRON asks what to say
                None if ["message", "msg", "whatsapp", "dm"].iter().any(|v| verb.contains(v)) && rest.split_whitespace().count() <= 3 && rest.chars().all(|c| c.is_alphabetic() || c == ' ') => (rest.clone(), String::new()),
                None => return None,
            }
        }
        None => {
            // the message first, then who: "send hello to danish on whatsapp", "say good night to mom on telegram"
            // (only with the app named or the message in quotes: "send feedback to Microsoft" isn't a chat message)
            let rest = ["send ", "say ", "write "].iter().find_map(|v| bl.strip_prefix(v).map(|_| body[v.len()..].to_string()))?;
            let rl = lower(&rest);
            let i = rl.rfind(" to ")?;
            let (text, to) = (rest[..i].trim().to_string(), rest[i + 4..].trim().to_string());
            let quoted = text.starts_with('"') || text.starts_with('\'');
            if !(quoted || app.is_some()) || rl.contains("email") || rl.contains("mail ") || to.split_whitespace().count() > 4 {
                return None;
            }
            (to, text)
        }
    };
    let to = to.trim().trim_start_matches("to ").trim_matches(|c: char| matches!(c, ',' | '.' | ':' | ';' | '"' | '\'' | ' ')).to_string();
    let text = text.trim();
    // in quotes: the message is what's inside them (a "?" after the closing quote was said to ULTRON, not to them)
    let text = match text.strip_prefix('"').and_then(|t| t.rfind('"').map(|j| t[..j].to_string())) {
        Some(inner) => inner.trim().to_string(),
        None => text.trim_matches(|c| matches!(c, '"' | '\'')).trim().to_string(),
    };
    let first = lower(&to).split_whitespace().next().unwrap_or("").to_string();
    if to.is_empty() || ["me", "us", "you", "him", "her", "them", "a", "the", "my", "it", "this", "that"].contains(&first.as_str()) {
        return None;
    }
    Some((app.unwrap_or_else(|| "whatsapp".into()), to, text))
}

/// "priya saying I'll be late" -> ("priya", "I'll be late")
fn who_what(rest: &str) -> Option<(String, String)> {
    let rl = lower(rest);
    // the earliest of the words that lead into the message
    let sep = [" saying ", " that ", ": ", " to say ", " and say ", " asking ", " - "].iter().filter_map(|s| rl.find(s).map(|i| (i, s.len()))).min();
    if let Some((i, n)) = sep {
        return Some((rest[..i].to_string(), rest[i + n..].to_string()));
    }
    // message Danish "Hello"
    if let Some(i) = rest.find('"').filter(|i| *i > 0) {
        return Some((rest[..i].to_string(), rest[i..].to_string()));
    }
    // "message danish, are you coming?" (a comma straight after the name; one further in belongs to the message)
    if let Some(i) = rest.find(", ").filter(|i| rest[..*i].split_whitespace().count() == 1) {
        return Some((rest[..i].to_string(), rest[i + 2..].to_string()));
    }
    // "message Hi to Danish", "send a message good morning to mom": the message first, when it opens with a
    // word nobody is called (that one used to be read as a message "to Danish" for someone named Hi)
    const OPENERS: [&str; 44] = ["hi", "hii", "hello", "hey", "good", "thanks", "thank", "please", "ok", "okay", "yes", "no", "sorry", "happy", "congrats", "congratulations", "i", "i'm", "im", "i'll",
        "we", "can", "are", "is", "what", "where", "when", "how", "call", "come", "see", "on", "the", "a", "my", "your", "let's", "lets", "bye", "love", "did", "do", "will", "namaste"];
    let first = rl.split_whitespace().next().unwrap_or("").trim_matches(|c: char| !c.is_alphanumeric() && c != '\'').to_string();
    if OPENERS.contains(&first.as_str()) {
        if let Some(i) = rl.rfind(" to ") {
            let who = rest[i + 4..].trim();
            if (1..=3).contains(&who.split_whitespace().count()) {
                return Some((who.to_string(), rest[..i].to_string()));
            }
        }
        return Some((String::new(), rest.to_string())); // a message with nobody named: not one ULTRON can send
    }
    // "message Danish Khan": only a name (every word capitalised, none of them a greeting)
    let words: Vec<&str> = rest.split_whitespace().collect();
    if words.len() >= 2 && words.len() <= 3 && words.iter().all(|w| w.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) && !OPENERS.contains(&lower(w).as_str())) {
        return None;
    }
    // "message priya hello there": the first word is the name
    let mut it = rest.splitn(2, ' ');
    Some((it.next()?.to_string(), it.next()?.to_string()))
}

/// "Danish's chat is open. What should I say?": who that was asked about, so that what you say next is the message.
pub static ASKING: std::sync::Mutex<Option<(String, String, std::time::Instant)>> = std::sync::Mutex::new(None);

/// If ULTRON has just asked what to say to someone (within the last minute), this is that someone,
/// unless what was said now is "never mind".
pub fn asked_message(text: &str) -> Option<(String, String)> {
    let (app, to, at) = ASKING.lock().unwrap().take()?;
    let l = lower(text.trim().trim_end_matches(['.', '!']));
    if at.elapsed() > std::time::Duration::from_secs(60) || l.is_empty() || ["cancel", "never mind", "nevermind", "nothing", "stop", "no", "forget it", "leave it"].contains(&l.as_str()) {
        return None;
    }
    Some((app, to))
}

/// "email john@x.com about the meeting saying see you at 5", "write an email to my boss asking for leave"
pub fn email(raw: &str) -> Option<(String, String, String)> {
    let l = lower(raw.trim());
    let verbs = ["write an email to ", "send an email to ", "draft an email to ", "compose an email to ", "email ", "mail "];
    let v = verbs.iter().find(|v| l.starts_with(*v))?;
    let rest = &raw.trim()[v.len()..];
    let rl = lower(rest);
    let cut = |keys: &[&str]| keys.iter().find_map(|k| rl.find(k).map(|i| (i, k.len())));
    let (to_end, subject, body) = match cut(&[" about ", " regarding ", " re "]) {
        Some((i, n)) => {
            let after = &rest[i + n..];
            let al = lower(after);
            match [" saying ", " that ", " and say ", ": "].iter().find_map(|k| al.find(k).map(|j| (j, k.len()))) {
                Some((j, m)) => (i, after[..j].trim().to_string(), after[j + m..].trim().to_string()),
                None => (i, after.trim().to_string(), String::new()),
            }
        }
        None => match cut(&[" asking ", " saying ", " that ", " to "]) {
            // keep "asking ..." so the email is written as a request
            Some((i, _)) if rl[i..].starts_with(" asking ") => (i, String::new(), rest[i + 1..].trim().to_string()),
            Some((i, n)) => (i, String::new(), rest[i + n..].trim().to_string()),
            None => (rest.len(), String::new(), String::new()),
        },
    };
    let to = rest[..to_end].trim().to_string();
    (!to.is_empty()).then_some((to, subject, body))
}

/// Writing tools on the text you've selected. -> (instruction, paste the result back?)
pub fn writing(l: &str) -> Option<(String, bool)> {
    let sel = ["this", "selected", "selection", "my text", "highlighted", "it", "the text", "what i selected", "what i wrote"];
    if !sel.iter().any(|w| l.split(|c: char| !c.is_alphanumeric() && c != ' ').any(|p| p.contains(w))) {
        return None;
    }
    if ["page", "screen", "window", "article", "video", "website", "site", "tab"].iter().any(|w| l.split_whitespace().any(|x| x == *w)) {
        return None; // that's about the screen
    }
    let starts = |ps: &[&str]| ps.iter().any(|p| l.starts_with(p));
    if starts(&["reply to", "draft a reply", "write a reply", "respond to", "answer this"]) {
        return Some(("Write a short, natural reply to this message, ready to send. Reply with only the reply text.".to_string(), false));
    }
    if starts(&["summarize", "summarise", "tldr", "give me the gist", "explain"]) {
        return Some((format!("{l}. Summarize it in 2 or 3 sentences."), false));
    }
    if let Some(r) = l.strip_prefix("rewrite the selected text to ").or_else(|| l.strip_prefix("rewrite this to ")).or_else(|| l.strip_prefix("rewrite it to ")) {
        return Some((format!("Rewrite it to {r}."), true));
    }
    if starts(&["rewrite", "rephrase", "paraphrase", "reword", "fix", "correct", "proofread", "polish", "improve", "make", "shorten", "expand",
        "translate", "turn", "convert", "simplify", "clean up", "tidy"])
    {
        return Some((format!("{}.", l), true));
    }
    None
}

/// "summarize this page", "what's on my screen", "what does this error mean" -> the question to ask about the screen
pub fn screen(l: &str) -> Option<String> {
    if let Some(q) = l.strip_prefix("answer about the screen:").or_else(|| l.strip_prefix("answer about the screen")) {
        return Some(q.trim().to_string());
    }
    // whole words, so "what Windows version do I have" and "take a screenshot" aren't about the screen
    let words: Vec<&str> = l.split(|c: char| !c.is_alphanumeric()).collect();
    let about = ["this article", "this website", "this site", "this tab", "this error", "this email", "this document", "this chat"];
    if !["screen", "page", "webpage", "window"].iter().any(|w| words.contains(w)) && !about.iter().any(|w| l.contains(w)) {
        return None;
    }
    let asks = ["summarize", "summarise", "what", "explain", "read", "tell me", "tldr", "give me", "how", "why", "who", "is ", "does ", "can you", "translate",
        "look at ", "look on ", "see ", "check ", "describe", "count ", "which ", "where ", "are there", "do you see"];
    if !asks.iter().any(|a| l.starts_with(a)) {
        return None;
    }
    if l.starts_with("summarize") || l.starts_with("summarise") || l.starts_with("tldr") || l.contains("what's on my screen") || l.contains("what is on my screen") {
        return Some(String::new());
    }
    Some(l.to_string())
}

pub fn settings_page(l: &str) -> Option<(&'static str, &'static str)> {
    let l = l.trim_start_matches("open ").trim_start_matches("go to ").trim_start_matches("show ").trim_start_matches("the ");
    let name = l.strip_suffix(" settings").or_else(|| l.strip_suffix(" setting"))?;
    let pages: [(&[&str], &str, &str); 22] = [
        (&["wifi", "wi-fi", "wi fi", "network", "internet"], "Wi-Fi", "ms-settings:network-wifi"),
        (&["bluetooth", "devices"], "Bluetooth", "ms-settings:bluetooth"),
        (&["display", "screen", "resolution"], "Display", "ms-settings:display"),
        (&["sound", "audio", "volume"], "Sound", "ms-settings:sound"),
        (&["apps", "app", "installed apps"], "Apps", "ms-settings:appsfeatures"),
        (&["update", "windows update", "updates"], "Windows Update", "ms-settings:windowsupdate"),
        (&["battery", "power", "power and battery"], "Power & battery", "ms-settings:powersleep"),
        (&["privacy"], "Privacy", "ms-settings:privacy"),
        (&["personalization", "personalisation", "theme", "themes"], "Personalization", "ms-settings:personalization"),
        (&["background", "wallpaper"], "Background", "ms-settings:personalization-background"),
        (&["notification", "notifications"], "Notifications", "ms-settings:notifications"),
        (&["storage", "disk"], "Storage", "ms-settings:storagesense"),
        (&["mouse"], "Mouse", "ms-settings:mousetouchpad"),
        (&["keyboard", "typing"], "Typing", "ms-settings:typing"),
        (&["default apps", "default app"], "Default apps", "ms-settings:defaultapps"),
        (&["date", "time", "date and time", "clock"], "Date & time", "ms-settings:dateandtime"),
        (&["language", "region"], "Language", "ms-settings:regionlanguage"),
        (&["vpn"], "VPN", "ms-settings:network-vpn"),
        (&["hotspot", "mobile hotspot"], "Mobile hotspot", "ms-settings:network-mobilehotspot"),
        (&["night light"], "Night light", "ms-settings:nightlight"),
        (&["focus", "do not disturb", "focus assist"], "Focus", "ms-settings:quiethours"),
        (&["accounts", "account"], "Accounts", "ms-settings:yourinfo"),
    ];
    pages.iter().find(|(k, _, _)| k.contains(&name)).map(|(_, n, u)| (*n, *u))
}

/* ---------------- doing things ---------------- */

fn ps(script: &str) -> Option<String> {
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(NO_WINDOW)
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn brightness() -> Option<u32> {
    ps("(Get-CimInstance -Namespace root/WMI -ClassName WmiMonitorBrightness | Select-Object -First 1).CurrentBrightness").and_then(|s| s.parse().ok())
}

pub fn set_brightness(v: u32) -> bool {
    let v = v.min(100);
    ps(&format!("$m = Get-CimInstance -Namespace root/WMI -ClassName WmiMonitorBrightnessMethods; if ($m) {{ Invoke-CimMethod -InputObject $m -MethodName WmiSetBrightness -Arguments @{{Timeout=1; Brightness=[byte]{v}}} | Out-Null; 'ok' }}"))
        .map(|s| s.contains("ok"))
        .unwrap_or(false)
}

/// Apps are in dark mode (read from Windows' settings directly: no program started for it).
pub fn is_dark() -> bool {
    use windows::core::w;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let (mut v, mut n) = (1u32, 4u32);
    unsafe {
        let _ = RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut v as *mut u32 as *mut core::ffi::c_void),
            Some(&mut n),
        );
    }
    v == 0
}

/// Dark mode for apps and Windows (None = flip it).
pub fn dark_mode(on: Option<bool>) -> Option<bool> {
    let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
    let now_dark = ps(&format!("(Get-ItemProperty 'Registry::{key}').AppsUseLightTheme")).map(|s| s.trim() == "0").unwrap_or(false);
    let dark = on.unwrap_or(!now_dark);
    let v = if dark { 0 } else { 1 };
    for name in ["AppsUseLightTheme", "SystemUsesLightTheme"] {
        let _ = Command::new("reg").args(["add", key, "/v", name, "/t", "REG_DWORD", "/d", &v.to_string(), "/f"]).creation_flags(NO_WINDOW).output();
    }
    Some(dark)
}

pub fn latest_download() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var("USERPROFILE").ok()?).join("Downloads");
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_lowercase();
            !n.ends_with(".crdownload") && !n.ends_with(".part") && !n.ends_with(".tmp") && n != "desktop.ini"
        })
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

pub fn known_folder(name: &str) -> std::path::PathBuf {
    let home = std::path::PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default());
    // Desktop and Documents may live in OneDrive
    let pick = |sub: &str| {
        let od = std::env::var("OneDrive").map(|o| std::path::PathBuf::from(o).join(sub)).ok().filter(|p| p.exists());
        od.unwrap_or_else(|| home.join(sub))
    };
    match name {
        "documents" => pick("Documents"),
        "downloads" => home.join("Downloads"),
        "pictures" => pick("Pictures"),
        "music" => home.join("Music"),
        "videos" => home.join("Videos"),
        _ => pick("Desktop"),
    }
}

/// The readable text of a window (what's on screen), via UI Automation.
pub fn window_text(h: HWND, limit: usize) -> String {
    use windows::core::Interface;
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
    use windows::Win32::UI::Accessibility::*;
    let mut out = String::new();
    // Firefox-family browsers don't share their page text this way: reading the screen is faster there
    let mut class = [0u16; 64];
    let n = unsafe { windows::Win32::UI::WindowsAndMessaging::GetClassNameW(h, &mut class) };
    if String::from_utf16_lossy(&class[..n.max(0) as usize]) == "MozillaWindowClass" {
        return out;
    }
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let uia: IUIAutomation = match CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER).or_else(|_| CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)) {
            Ok(u) => u,
            Err(_) => return out,
        };
        if let Ok(u2) = uia.cast::<IUIAutomation2>() {
            let _ = u2.SetTransactionTimeout(2000);
        }
        let Ok(root) = uia.ElementFromHandle(h) else { return out };
        // a web page or a document: find it (a few levels down), then read all its text in one go
        if let Ok(walker) = uia.ControlViewWalker() {
            let mut queue = std::collections::VecDeque::from([root.clone()]);
            let (t0, mut seen_nodes) = (std::time::Instant::now(), 0);
            while let Some(e) = queue.pop_front() {
                seen_nodes += 1;
                if seen_nodes > 600 || t0.elapsed() > std::time::Duration::from_millis(1500) {
                    break;
                }
                if e.CurrentControlType().map(|c| c.0).unwrap_or(0) == 50030 && !e.CurrentIsOffscreen().map(|b| b.as_bool()).unwrap_or(true) {
                    if let Ok(tp) = e.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) {
                        if let Ok(t) = tp.DocumentRange().and_then(|r| r.GetText(limit as i32)) {
                            let t = t.to_string();
                            if t.split_whitespace().count() > 30 {
                                return t;
                            }
                        }
                    }
                }
                let mut c = walker.GetFirstChildElement(&e).ok();
                while let Some(x) = c {
                    c = walker.GetNextSiblingElement(&x).ok();
                    queue.push_back(x);
                }
            }
        }
        // anything else: one snapshot of the visible controls' names
        if let Ok(u2) = uia.cast::<IUIAutomation2>() {
            let _ = u2.SetTransactionTimeout(1500);
        }
        let Ok(cache) = uia.CreateCacheRequest() else { return out };
        for p in [UIA_NamePropertyId, UIA_ControlTypePropertyId, UIA_IsOffscreenPropertyId] {
            let _ = cache.AddProperty(p);
        }
        let Ok(cond) = uia.ControlViewCondition() else { return out };
        let Ok(all) = root.FindAllBuildCache(TreeScope_Subtree, &cond, &cache) else { return out };
        let n = all.Length().unwrap_or(0).min(9000);
        let mut seen = std::collections::HashSet::new();
        for i in 0..n {
            let Ok(e) = all.GetElement(i) else { continue };
            let ct = e.CachedControlType().map(|c| c.0).unwrap_or(0);
            // text, document, edit, hyperlink, list item, header, data item, group
            if !matches!(ct, 50020 | 50030 | 50004 | 50005 | 50007 | 50034 | 50029) || e.CachedIsOffscreen().map(|b| b.as_bool()).unwrap_or(false) {
                continue;
            }
            let s = e.CachedName().map(|b| b.to_string()).unwrap_or_default();
            let s = s.trim().to_string();
            if s.len() > 1 && seen.insert(s.clone()) {
                out.push_str(&s);
                out.push('\n');
                if out.len() > limit {
                    break;
                }
            }
        }
    }
    out
}

/* ---------------- Spotify: the exact track, without an account key ---------------- */

static TRACKS: Mutex<Option<std::collections::HashMap<String, (String, String, String)>>> = Mutex::new(None);

fn tracks_path() -> Option<std::path::PathBuf> {
    std::env::var("APPDATA").ok().map(|d| std::path::PathBuf::from(d).join("com.ultron.notch").join("spotify-tracks.json"))
}

fn tracks() -> std::sync::MutexGuard<'static, Option<std::collections::HashMap<String, (String, String, String)>>> {
    let mut g = TRACKS.lock().unwrap();
    if g.is_none() {
        *g = Some(tracks_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default());
    }
    g
}

/// Remember a song's Spotify track so it plays instantly next time.
pub fn remember_track(query: &str, id: &str, name: &str, artist: &str) {
    let mut g = tracks();
    let m = g.as_mut().unwrap();
    m.insert(lower(query.trim()), (id.into(), name.into(), artist.into()));
    if let (Some(p), Ok(b)) = (tracks_path(), serde_json::to_vec(m)) {
        let _ = std::fs::write(p, b);
    }
}

/// "billie jean" / "shape of you by ed sheeran" -> (track id, name, artist): remembered, or from
/// MusicBrainz (an open music database that lists many recordings' Spotify links).
pub fn spotify_track(query: &str) -> Option<(String, String, String)> {
    let q = lower(query.trim());
    if let Some(hit) = tracks().as_ref().and_then(|m| m.get(&q).cloned()) {
        return Some(hit);
    }
    let (title, artist) = match q.split_once(" by ") {
        Some((t, a)) => (t.trim().to_string(), a.trim().to_string()),
        None => (q.clone(), String::new()),
    };
    let mut mbq = format!("recording:\"{title}\"");
    if !artist.is_empty() {
        mbq.push_str(&format!(" AND artist:\"{artist}\""));
    }
    let enc = |s: &str| s.bytes().map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>();
    let agent = crate::net::agent(Duration::from_secs(5));
    let get = |url: &str| agent.get(url).set("User-Agent", "ULTRON/0.3 ( desktop assistant )").call().ok()?.into_json::<serde_json::Value>().ok();
    let found = get(&format!("https://musicbrainz.org/ws/2/recording?fmt=json&limit=4&query={}", enc(&mbq)))?;
    let t0 = Instant::now();
    for rec in found.get("recordings")?.as_array()?.iter().take(4) {
        if t0.elapsed() > Duration::from_millis(1200) {
            break;
        }
        let id = rec.get("id")?.as_str()?;
        std::thread::sleep(Duration::from_millis(1050)); // MusicBrainz asks for one request a second
        let Some(d) = get(&format!("https://musicbrainz.org/ws/2/recording/{id}?inc=url-rels+artist-credits&fmt=json")) else { continue };
        let link = d.get("relations").and_then(|r| r.as_array()).and_then(|rs| {
            rs.iter().find_map(|r| r.get("url")?.get("resource")?.as_str().filter(|u| u.contains("open.spotify.com/track/")).map(str::to_string))
        });
        if let Some(link) = link {
            let tid: String = link.split("/track/").nth(1)?.chars().take(22).collect();
            let name = d.get("title").and_then(|x| x.as_str()).unwrap_or(&title).to_string();
            let art = d.get("artist-credit").and_then(|a| a.get(0)).and_then(|a| a.get("name")).and_then(|x| x.as_str()).unwrap_or("").to_string();
            return Some((tid, name, art));
        }
    }
    None
}

/* ---------------- sending a message only after you say so ---------------- */

pub struct Pending {
    pub hwnd: isize,
    pub to: String,
    pub app: String,
    pub at: Instant,
}

pub static PENDING: Mutex<Option<Pending>> = Mutex::new(None);

#[cfg(test)]
mod tests {
    #[test]
    fn tidies() {
        for (a, b) in [("opne whatsap n mesage danish pls", "open whatsapp and message danish"), ("Serach youtube for lofi", "search youtube for lofi"), ("can u tranlate this", "can u translate this"), ("rice n dal", "rice n dal"), ("press ctrl n", "press ctrl n"), ("tell danish pls call me", "tell danish pls call me"),
            ("message danish \"opne teh door\"", "message danish \"opne teh door\""), ("pls opn chorme", "open chrome"),
            ("plan my day", "plan my day"), ("shop for shoes", "shop for shoes"), ("Open Notepad and write U and N", "Open Notepad and write U and N"), ("what is vitamin b", "what is vitamin b")] {
            assert_eq!(super::tidy(a).to_lowercase(), b.to_lowercase(), "{a}");
        }
    }

    #[test]
    fn messages() {
        let m = |q: &str| super::message(q).map(|(a, t, x)| format!("{a}|{t}|{x}"));
        for (q, want) in [
            ("message priya on whatsapp saying I'll be late", Some("whatsapp|priya|I'll be late")),
            ("message DANISH \"Hello\"?", Some("whatsapp|DANISH|Hello")),
            ("message Danish “Hello”", Some("whatsapp|Danish|Hello")),
            ("send hello to danish on whatsapp", Some("whatsapp|danish|hello")),
            ("send \"on my way\" to rahul on telegram", Some("telegram|rahul|on my way")),
            ("say good night to mom on whatsapp", Some("whatsapp|mom|good night")),
            ("tell sam on discord: gg", Some("discord|sam|gg")),
            ("text mom that dinner's ready", Some("whatsapp|mom|dinner's ready")),
            ("message danish hi how are you?", Some("whatsapp|danish|hi how are you?")),
            ("message danish hello, how are you", Some("whatsapp|danish|hello, how are you")),
            ("message danish khan saying hi", Some("whatsapp|danish khan|hi")),
            ("message to danish saying call me", Some("whatsapp|danish|call me")),
            ("Message Danish. Hello", Some("whatsapp|Danish|Hello")),
            ("whatsapp danish I'm outside", Some("whatsapp|danish|I'm outside")),
            ("message Hi to Danish", Some("whatsapp|Danish|Hi")),
            ("message good morning to mom on whatsapp", Some("whatsapp|mom|good morning")),
            ("message mom I'm going to school", Some("whatsapp|mom|I'm going to school")),
            ("MEssage Danish", Some("whatsapp|Danish|")),
            ("message Danish Khan", Some("whatsapp|Danish Khan|")),
            ("ping google.com", None),
            ("tell me a joke", None),
            ("send an email to bob about lunch", None),
            ("send feedback to microsoft", None),
            ("message me later", None),
        ] {
            assert_eq!(m(q).as_deref(), want, "{q}");
        }
    }

    #[test]
    fn screen_questions() {
        for (q, yes) in [
            ("what's on my screen", true),
            ("summarize this page", true),
            ("look at the window that is open right now and tell me how many buttons you can see on it", true),
            ("where is the save button on this screen", true),
            ("what does this error mean", true),
            ("what windows version do i have", false),
            ("how many pages does a novel have", false),
            ("look up the wikipedia page for india", false),
            ("check my email", false),
            ("tell me a joke", false),
        ] {
            assert_eq!(super::screen(q).is_some(), yes, "{q}");
        }
    }

    #[test]
    fn finds_tracks() {
        for q in ["shape of you by ed sheeran", "tum hi ho by arijit singh"] {
            let t0 = std::time::Instant::now();
            println!("{q:?} -> {:?} in {} ms", super::spotify_track(q), t0.elapsed().as_millis());
        }
    }
}

#[cfg(test)]
mod read_tests {
    #[test]
    fn reads_a_browser_quickly() {
        use windows::core::BOOL;
        use windows::Win32::Foundation::{HWND, LPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetClassNameW, GetWindowTextLengthW, IsWindowVisible};
        static mut FOUND: Vec<(isize, String)> = Vec::new();
        unsafe extern "system" fn each(h: HWND, _: LPARAM) -> BOOL {
            let mut buf = [0u16; 64];
            let n = GetClassNameW(h, &mut buf);
            let class = String::from_utf16_lossy(&buf[..n as usize]);
            if IsWindowVisible(h).as_bool() && GetWindowTextLengthW(h) > 0 && (class == "MozillaWindowClass" || class == "Chrome_WidgetWin_1") {
                (*std::ptr::addr_of_mut!(FOUND)).push((h.0 as isize, class));
            }
            BOOL(1)
        }
        unsafe {
            let _ = EnumWindows(Some(each), LPARAM(0));
            for (h, class) in (*std::ptr::addr_of!(FOUND)).iter().take(4) {
                let t0 = std::time::Instant::now();
                let t = super::window_text(HWND(*h as _), 7000);
                println!("{class}: {} words in {} ms", t.split_whitespace().count(), t0.elapsed().as_millis());
            }
        }
    }
}
