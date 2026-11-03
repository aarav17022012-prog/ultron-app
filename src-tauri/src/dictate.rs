//! Dictation: what you say, made ready to type.
//!
//! "Sylver, start dictation" (or its shortcut) turns it on: from then on everything heard is typed where the cursor is,
//! or put on the dictation card, until "stop dictation". The recognizer is given longer pauses before it decides you've
//! finished, and the last things said as its context (both from the page, through voice_config). Here each heard piece
//! is shaped to follow what's already there (Settings > Dictation says how much):
//!
//! - the marks you say aloud ("comma", "question mark", "new line", "open quote") become the marks;
//! - "um"s, "uh"s, "you know"s and stutters ("I I think", "we should we should") go;
//! - a correction said aloud is made: "at three, no wait, four" is "at four", "the cake, I mean the cookies" is "the
//!   cookies"; "scratch that" alone takes back the last piece;
//! - a pause doesn't end the sentence: the recognizer ends every piece with a full stop, so that mark is held back
//!   until the next piece shows whether the sentence goes on ("we should go" ... "to the park") or a new one starts;
//! - a sentence that asks gets a question mark ("can you send it", "what time is it", "..., right");
//! - names and words you've listed are spelled your way.

use windows::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_BACK, VK_RETURN, VK_SHIFT};

/// The marks that can be said aloud: (the words, what is typed, whether it sticks to the word before).
const MARKS: &[(&[&str], &str, bool)] = &[
    (&["new", "paragraph"], "\n\n", false),
    (&["next", "paragraph"], "\n\n", false),
    (&["new", "line"], "\n", false),
    (&["next", "line"], "\n", false),
    (&["newline"], "\n", false),
    (&["bullet", "point"], "\n•", false),
    (&["new", "bullet"], "\n•", false),
    (&["question", "mark"], "?", true),
    (&["exclamation", "mark"], "!", true),
    (&["exclamation", "point"], "!", true),
    (&["full", "stop"], ".", true),
    (&["semicolon"], ";", true),
    (&["semi", "colon"], ";", true),
    (&["colon"], ":", true),
    (&["comma"], ",", true),
    (&["ellipsis"], "…", true),
    (&["open", "quote"], "“", false),
    (&["close", "quote"], "”", true),
    (&["end", "quote"], "”", true),
];

fn bare(w: &str) -> String {
    w.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'').to_lowercase()
}

/// The said-aloud marks turned into the marks themselves; nothing else about the words is touched.
/// ("type hello comma how are you question mark" types "hello, how are you?"; "period" said last is a full stop)
pub fn marks(heard: &str) -> String {
    let mut words: Vec<&str> = heard.split_whitespace().collect();
    let period = words.len() > 1 && words.last().is_some_and(|w| bare(w) == "period");
    if period {
        words.pop();
    }
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let hit = MARKS.iter().find(|(say, _, _)| i + say.len() <= words.len() && say.iter().enumerate().all(|(k, s)| bare(words[i + k]) == *s));
        match hit {
            Some((say, mark, sticks)) => {
                // (the recognizer often puts a mark of its own beside the word: "Hello, comma, how". Those go.)
                if *sticks {
                    if let Some(last) = out.last_mut() {
                        let kept = last.trim_end_matches([',', '.', ';', ':', '?', '!']).to_string();
                        if !kept.is_empty() && !last.ends_with('\n') {
                            *last = kept;
                        }
                    }
                }
                if *sticks && !out.is_empty() && !out.last().is_some_and(|l| l.ends_with('\n')) {
                    out.last_mut().unwrap().push_str(mark);
                } else {
                    out.push(mark.to_string());
                }
                i += say.len();
            }
            None => {
                out.push(words[i].to_string());
                i += 1;
            }
        }
    }
    let mut text = String::new();
    for w in out {
        let glue = text.is_empty() || text.ends_with('\n') || text.ends_with('“') || w.starts_with('\n') || (w.chars().count() == 1 && ",.?!;:…”".contains(&w));
        if !glue {
            text.push(' ');
        }
        text.push_str(&w);
    }
    if period {
        let kept = text.trim_end_matches([',', '.', ';', ':']).len();
        text.truncate(kept);
        text.push('.');
    }
    text
}

const FILLERS: &[&str] = &["um", "uh", "uhm", "umm", "erm", "er", "hmm", "mm", "mhm"];
/// Words the sentence can't end on: a piece that ends with one is carried on by the next.
const DANGLING: &[&str] = &[
    "a", "an", "the", "to", "of", "in", "on", "at", "for", "with", "from", "by", "and", "or", "but", "because", "that", "which", "who", "my", "your", "our", "their",
    "his", "her", "its", "is", "are", "was", "were", "be", "been", "will", "would", "can", "could", "should", "have", "has", "if", "when", "while", "as", "than",
    "very", "really", "about", "into", "this", "these", "those", "some",
];
/// Words a new piece can carry a sentence on with (not followed by a comma: "So, ..." starts one).
const JOINERS: &[&str] = &[
    "and", "but", "or", "so", "because", "which", "who", "whom", "whose", "that", "then", "than", "to", "with", "for", "of", "in", "on", "at", "by", "if", "when",
    "while", "as", "until", "unless", "although", "though", "plus", "including", "especially", "like", "into", "from",
];
/// Ordinary words: one that opens a piece carrying a sentence on loses the capital the recognizer gave it. (A name keeps
/// its own: it isn't here.)
const COMMON: &[&str] = &[
    "and", "but", "so", "then", "the", "a", "an", "to", "of", "in", "on", "it", "is", "that", "this", "we", "you", "he", "she", "they", "there", "because", "which",
    "with", "for", "if", "or", "when", "what", "how", "at", "as", "by", "from", "my", "your", "our", "its", "also", "not", "just", "maybe", "probably", "really",
    "very", "too", "all", "some", "any", "more", "most", "after", "before", "about", "into", "over", "under", "again", "still", "even", "only", "now", "today",
    "tomorrow", "tonight", "yesterday", "later", "soon", "here", "where", "who", "why", "was", "were", "are", "be", "been", "will", "would", "can", "could",
    "should", "have", "has", "had", "do", "does", "did", "go", "get", "make", "take", "send", "see", "know", "think", "want", "need", "like", "let's", "let",
    "please", "thanks", "until", "while", "though", "although", "unless", "since", "than", "both", "each", "every", "them", "him", "her", "us", "me", "these",
    "those", "their",
];

/// What dictation tidies, as chosen in Settings > Dictation.
#[derive(serde::Deserialize, Default, Clone)]
pub struct Opts {
    /// "um"s, "uh"s, "you know"s and stutters go
    #[serde(default)]
    pub fillers: bool,
    /// a correction said aloud is made, and "scratch that" takes the last piece back
    #[serde(default)]
    pub fixes: bool,
    /// questions get their question mark, and a pause doesn't end the sentence
    #[serde(default)]
    pub smart: bool,
    /// names and words spelled your way
    #[serde(default)]
    pub words: Vec<String>,
    /// people's names SYLVER has seen (who writes to you): a near miss on one is put right, as for your own words
    #[serde(default)]
    pub names: Vec<String>,
}

/// A heard piece, ready to go after what's already there.
#[derive(serde::Serialize, Default, Debug, PartialEq)]
pub struct Shaped {
    /// goes first: the mark that ends the sentence before (held back until now) and the space
    pub lead: String,
    pub text: String,
    /// the mark this piece's sentence ends with, held back: the next piece may carry the sentence on ("" when the
    /// piece ends with a mark of its own, or a new line)
    pub held: String,
    /// "scratch that": the last piece is taken back
    pub undo: bool,
}

/// Whether a sentence asks: "can you send it", "what time is it", "is John coming", "..., right". ("What I mean is",
/// "When I get home", "Do it now" and "Have a good day" tell.)
pub fn is_question(sentence: &str) -> bool {
    const OPENERS: &[&str] = &["so", "and", "but", "okay", "ok", "hey", "well", "also", "then", "now", "oh", "alright"];
    const PRONOUNS: &[&str] = &["you", "i", "we", "they", "he", "she", "it", "there", "anyone", "anybody", "someone", "somebody", "everyone", "everybody", "anything", "something", "everything", "this"];
    const DETERMINERS: &[&str] = &["that", "these", "those", "the", "a", "an", "my", "your", "our", "their", "his", "her", "its", "any", "all"];
    const AUX: &[&str] = &[
        "is", "are", "was", "were", "am", "can", "could", "would", "will", "should", "shall", "may", "might", "does", "did", "isn't", "aren't", "wasn't", "weren't",
        "can't", "couldn't", "wouldn't", "won't", "shouldn't", "doesn't", "didn't", "do", "don't", "have", "has", "had", "haven't", "hasn't",
    ];
    const WH: &[&str] = &["what", "why", "how", "where", "when", "who", "which", "whose", "whom"];
    let tail = sentence.trim().trim_end_matches(['.', '!', ' ']).to_lowercase();
    if [", right", ", isn't it", ", aren't you", ", don't you", ", won't you", ", okay", ", ok", ", correct", ", yeah", ", no", ", isn't that", ", didn't you"].iter().any(|t| tail.ends_with(t)) {
        return true;
    }
    let raw: Vec<&str> = sentence.split_whitespace().collect();
    let w: Vec<String> = raw.iter().map(|x| bare(x)).collect();
    let mut i = 0;
    while i < w.len() && OPENERS.contains(&w[i].as_str()) {
        i += 1;
    }
    let Some(a) = w.get(i).map(|s| s.as_str()) else { return false };
    let b = w.get(i + 1).map(|s| s.as_str()).unwrap_or("");
    let b_name = raw.get(i + 1).and_then(|x| x.chars().next()).is_some_and(|c| c.is_uppercase()) && b != "i";
    if ["what's", "where's", "how's", "who's", "when's", "why's", "what're", "how're", "where're"].contains(&a) {
        return true;
    }
    if WH.contains(&a) {
        return !matches!(b, "i" | "you" | "we" | "they" | "he" | "she" | "a" | "an" | "") || matches!(a, "who" | "why") && b != "";
    }
    if AUX.contains(&a) {
        if matches!(a, "do" | "don't" | "have" | "has" | "had" | "haven't" | "hasn't") {
            return matches!(b, "you" | "i" | "we" | "they" | "he" | "she");
        }
        return PRONOUNS.contains(&b) || DETERMINERS.contains(&b) || b_name;
    }
    false
}

/// "um"s and "uh"s, ", you know," and stutters ("I I think", "we should we should go") taken out.
fn unfill(text: &str) -> String {
    const STUTTER: &[&str] = &["i", "the", "a", "an", "to", "and", "we", "you", "it", "is", "so", "but", "my", "this", "in", "on", "of", "for", "he", "she", "they", "it's"];
    let mut w: Vec<String> = text.split_whitespace().filter(|w| !FILLERS.contains(&bare(w).as_str())).map(String::from).collect();
    // ", you know," (said in passing, between commas)
    let mut i = 1;
    while i + 1 < w.len() {
        if bare(&w[i]) == "you" && bare(&w[i + 1]) == "know" && w[i + 1].ends_with(',') && w[i - 1].ends_with(',') {
            w.drain(i..i + 2);
            let p = w[i - 1].trim_end_matches(',').to_string();
            w[i - 1] = p;
        } else {
            i += 1;
        }
    }
    let mut out: Vec<String> = Vec::with_capacity(w.len());
    let mut i = 0;
    while i < w.len() {
        // a pair said twice: the first goes
        if i + 3 < w.len() && bare(&w[i]) == bare(&w[i + 2]) && bare(&w[i + 1]) == bare(&w[i + 3]) && !bare(&w[i]).is_empty() && !w[i + 1].ends_with(['.', '?', '!']) {
            i += 2;
            continue;
        }
        // a small word said twice ("that that" and "had had" are left: they can be meant)
        if i + 1 < w.len() && STUTTER.contains(&bare(&w[i]).as_str()) && bare(&w[i]) == bare(&w[i + 1]) {
            i += 1;
            continue;
        }
        out.push(w[i].clone());
        i += 1;
    }
    out.join(" ")
}

/// A correction said aloud, made: "let's meet at three, no wait, four" is "let's meet at four"; "the cake, I mean the
/// cookies" is "the cookies"; "send it Monday scratch that send it Tuesday" is "send it Tuesday". (Only the plain ways of
/// saying it, between commas where they could be meant otherwise; the brain, when it's asked, does the rest.)
fn corrections(text: &str) -> String {
    let mut w: Vec<String> = text.split_whitespace().map(String::from).collect();
    for _ in 0..4 {
        // (where the cue is, how long it is, and whether what's before it goes altogether)
        let mut cue: Option<(usize, usize, bool)> = None;
        for i in 1..w.len() {
            let (a, b) = (bare(&w[i]), w.get(i + 1).map(|x| bare(x)).unwrap_or_default());
            let after_pause = w[i - 1].ends_with([',', '.', ';', '—', '-']);
            let found = match (a.as_str(), b.as_str()) {
                ("scratch", "that") | ("delete", "that") => Some((2, true)),
                ("no", "wait") | ("wait", "no") | ("or", "rather") | ("make", "that") => Some((2, false)),
                ("i", "mean") if after_pause => Some((2, false)),
                ("no", _) | ("correction", _) if after_pause && w[i].ends_with(',') => Some((1, false)),
                _ => None,
            };
            if let Some((len, all)) = found {
                if i + len < w.len() {
                    cue = Some((i, len, all));
                    break;
                }
            }
        }
        let Some((at, len, all)) = cue else { break };
        // the sentence the correction is in starts after the last full stop before it
        let start = w[..at].iter().rposition(|x| x.ends_with(['.', '?', '!'])).map_or(0, |p| p + 1);
        let y: Vec<String> = w[at + len..].iter().enumerate().map(|(k, x)| if k == 0 { x.trim_start_matches([',', '.']).to_string() } else { x.clone() }).filter(|x| !x.is_empty()).collect();
        let Some(y0) = y.first().map(|x| bare(x)) else { break };
        let keep = if all || at == start {
            start
        } else {
            // the word after the correction said earlier too ("at three, no, at four"): it starts again from there;
            // otherwise the word just before the correction is the one replaced
            let x = &w[start..at];
            match x.iter().rposition(|v| bare(v) == y0) {
                Some(p) if x.len() - p <= 6 => start + p,
                _ => at - 1,
            }
        };
        let mut nw: Vec<String> = w[..keep].to_vec();
        if let Some(last) = nw.last_mut() {
            if !last.ends_with(['.', '?', '!']) {
                *last = last.trim_end_matches([',', ';', '—', '-']).to_string();
            }
        }
        nw.extend(y);
        w = nw;
    }
    w.join(" ")
}

/// The words you've listed, spelled your way wherever they come up ("ultron" is "SYLVER").
fn respell(text: &str, words: &[String]) -> String {
    let mut out = text.to_string();
    for want in words.iter().map(|w| w.trim()).filter(|w| !w.is_empty()) {
        let (low, wl) = (out.to_lowercase(), want.to_lowercase());
        if low.len() != out.len() {
            continue; // (a letter that changes length when lowered: positions wouldn't line up)
        }
        let mut res = String::with_capacity(out.len());
        let mut from = 0;
        while let Some(p) = low[from..].find(&wl).map(|p| p + from) {
            let edge = |c: Option<char>| c.map_or(true, |c| !c.is_alphanumeric());
            if edge(low[..p].chars().last()) && edge(low[p + wl.len()..].chars().next()) {
                res.push_str(&out[from..p]);
                res.push_str(want);
            } else {
                res.push_str(&out[from..p + wl.len()]);
            }
            from = p + wl.len();
        }
        res.push_str(&out[from..]);
        out = res;
    }
    out
}

/// How a word sounds, roughly: its first sound, then its consonants, with the ones that are easily heard one for the
/// other made the same ("Danish" and "Denish", "Aarav" and "Arav", "Kristen" and "Christen" sound alike).
fn sound(w: &str) -> String {
    let mut out = String::new();
    for c in w.to_lowercase().chars() {
        let k = match c {
            'a' | 'e' | 'i' | 'o' | 'u' | 'y' => {
                if out.is_empty() { 'a' } else { continue }
            }
            'h' | 'w' => continue,
            'c' | 'k' | 'q' | 'g' => 'k',
            's' | 'z' | 'x' => 's',
            'b' | 'p' => 'p',
            'd' | 't' => 't',
            'f' | 'v' => 'f',
            'm' | 'n' => 'n',
            c if c.is_alphabetic() => c,
            _ => continue,
        };
        if !out.ends_with(k) {
            out.push(k);
        }
    }
    out
}

fn edits(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut prev = row[0];
        row[0] = i;
        for j in 1..=b.len() {
            let was = row[j];
            row[j] = (row[j] + 1).min(row[j - 1] + 1).min(prev + usize::from(a[i - 1] != b[j - 1]));
            prev = was;
        }
    }
    row[b.len()]
}

/// A name heard close but not right ("Denish" for "Danish") put right: one the recognizer took for a name (it gave it
/// a capital, and not just for starting a sentence), that sounds the same as a name you've listed or SYLVER has seen,
/// and is spelled nearly the same. An ordinary word is never changed into a name.
fn names_fix<'a>(text: &str, names: impl Iterator<Item = &'a String>, mut start: bool) -> String {
    const COMMON: &[&str] = &[
        "the", "and", "but", "can", "could", "would", "should", "will", "what", "when", "where", "why", "how", "who", "this", "that", "there", "then",
        "they", "these", "those", "yes", "yeah", "okay", "also", "just", "please", "thanks", "thank", "hello", "hey", "well", "sure", "maybe", "let",
        "have", "has", "had", "did", "does", "not", "now", "our", "your", "you", "she", "her", "his", "him", "its", "all", "any", "some", "one", "two",
        "for", "from", "with", "about", "after", "before", "because", "since", "while", "today", "tomorrow", "tonight", "sorry", "great", "good",
        "nice", "cool", "right", "see", "love", "hope", "need", "want", "make", "take", "send", "call", "tell", "ask", "get", "got", "give", "keep",
        "come", "going", "are", "was", "were", "may", "might", "must", "here", "once", "each", "every", "both", "many", "more", "most", "much", "very",
        "really", "actually", "anyway", "alright", "dear", "meet", "said", "say", "think", "know", "met", "met", "done", "next", "last", "first",
    ];
    let pool: Vec<&str> = names
        .flat_map(|n| n.split_whitespace())
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| w.chars().count() >= 3 && w.chars().next().is_some_and(|c| c.is_uppercase()) && w.chars().all(|c| c.is_alphabetic()))
        .collect();
    if pool.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 8);
    for (i, piece) in text.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let core = piece.trim_matches(|c: char| !c.is_alphanumeric());
        let low = core.to_lowercase();
        let named = core.chars().count() >= 3
            && core.chars().next().is_some_and(|c| c.is_uppercase())
            && core.chars().all(|c| c.is_alphabetic())
            && core.chars().skip(1).any(|c| c.is_lowercase())
            && (!start || piece.ends_with(',') || !COMMON.contains(&low.as_str()))
            && !pool.iter().any(|n| n.to_lowercase() == low);
        let key = if named { sound(core) } else { String::new() };
        let best = pool
            .iter()
            .filter(|n| named && sound(n) == key)
            .map(|n| (edits(&low, &n.to_lowercase()), *n))
            .filter(|(d, n)| *d <= (n.chars().count() / 3).max(1))
            .min_by_key(|(d, _)| *d);
        match best {
            Some((_, n)) => out.push_str(&piece.replacen(core, n, 1)),
            None => out.push_str(piece),
        }
        if !piece.is_empty() {
            start = piece.ends_with(['.', '?', '!']);
        }
    }
    out
}

/// Each sentence inside a piece that asks ends with a question mark, not the recognizer's full stop.
fn questions(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if c == '.' || c == '?' || c == '!' || c == '\n' {
            let s = &text[start..i];
            if c == '.' && is_question(s) && !text[i + 1..].starts_with(['.', '…']) {
                out.push_str(s);
                out.push('?');
            } else {
                out.push_str(&text[start..i + c.len_utf8()]);
            }
            start = i + c.len_utf8();
        }
    }
    out.push_str(&text[start..]);
    out
}

/// A piece of dictation ready to type. `after` is the last character already typed (None at the start):
/// it decides whether this piece opens a sentence or carries one on.
#[cfg(test)]
pub fn polish(heard: &str, after: Option<char>) -> String {
    polish_with(heard, after, true)
}

fn polish_with(heard: &str, after: Option<char>, fillers: bool) -> String {
    let kept: Vec<&str> = heard.split_whitespace().filter(|w| !fillers || !FILLERS.contains(&bare(w).as_str())).collect();
    let mut text = marks(&kept.join(" "));
    let opens = after.map_or(true, |c| ".?!\n".contains(c));
    if !opens {
        // carrying on mid-sentence: the recognizer's capital on an ordinary word comes down
        let first: String = text.chars().take_while(|c| c.is_alphabetic() || *c == '\'').collect();
        if COMMON.contains(&first.to_lowercase().as_str()) && first.chars().next().is_some_and(|c| c.is_uppercase()) && first.chars().skip(1).all(|c| !c.is_uppercase()) {
            text = format!("{}{}", first.to_lowercase(), &text[first.len()..]);
        }
    }
    // a capital at the start of each sentence
    let mut out = String::with_capacity(text.len());
    let (mut cap, mut ended) = (opens, false); // (ended: what came just before this space closed a sentence)
    for c in text.chars() {
        if cap && c.is_alphabetic() {
            out.extend(c.to_uppercase());
            cap = false;
            ended = false;
            continue;
        }
        out.push(c);
        if c == '\n' || (c.is_whitespace() && ended) {
            cap = true;
        } else if ".?!…".contains(c) {
            ended = true;
        } else if !c.is_whitespace() && !"\"'“”)•".contains(c) {
            ended = false;
            cap = false;
        }
    }
    out
}

/// One heard piece, shaped to go after `before` (what's already there in this dictation, without the mark held back
/// for its last sentence, `held`).
pub fn shape(heard: &str, before: &str, held: &str, o: &Opts) -> Shaped {
    let h = heard.trim();
    let said = |s: &str| h.split_whitespace().map(bare).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ") == s;
    if o.fixes && (said("scratch that") || said("delete that") || said("undo that") || said("take that back")) {
        return Shaped { undo: true, ..Default::default() };
    }
    let mut t = h.to_string();
    if o.fixes {
        t = corrections(&t);
    }
    if o.fillers {
        t = unfill(&t);
    }
    if t.trim().is_empty() {
        return Shaped::default();
    }
    // a mark said aloud at the end is meant; the recognizer's own full stop at a pause isn't (yet)
    let last_said = h.split_whitespace().last().map(bare).unwrap_or_default();
    let meant = matches!(last_said.as_str(), "period" | "stop" | "mark" | "point" | "ellipsis" | "colon" | "semicolon" | "comma");
    let fresh = before.trim().is_empty();
    // with a mark held back, the piece either carries the sentence on or starts the next one
    let carries = !fresh && !held.is_empty() && {
        let prev = before.split_whitespace().last().map(bare).unwrap_or_default();
        let first_raw = t.split_whitespace().next().unwrap_or("");
        let first = bare(first_raw);
        DANGLING.contains(&prev.as_str())
            || (JOINERS.contains(&first.as_str()) && !first_raw.ends_with(','))
            || first_raw.chars().next().is_some_and(|c| c.is_lowercase())
    };
    let after = if fresh { None } else if !held.is_empty() && !carries { Some('.') } else { before.trim_end_matches([' ', '\t']).chars().last() };
    let mut text = polish_with(&t, after, o.fillers);
    if o.smart {
        text = questions(&text);
    }
    text = respell(&text, &o.words);
    text = names_fix(&text, o.words.iter().chain(o.names.iter()), after.map_or(true, |c| matches!(c, '.' | '?' | '!' | '\n')));
    let mut lead = String::new();
    if !held.is_empty() && !carries {
        lead.push_str(held);
    }
    let joined = before.is_empty() || before.ends_with(char::is_whitespace);
    if (!joined || !lead.is_empty()) && !text.starts_with(['\n', ',', '.', '?', '!', ';', ':', '…', '”']) {
        lead.push(' ');
    }
    // the mark this piece's sentence ends with, held back until the next piece shows whether the sentence goes on
    let mut hold = String::new();
    if o.smart && !meant && !text.ends_with(['\n', '!', ',', ';', ':', '…', '”', '•']) {
        let asked = text.ends_with('?');
        let body = text.trim_end_matches(['.', '?']).to_string();
        // (the whole sentence, from where it started in what's there already, decides whether it asks)
        let sentence_before = if carries { before.rsplit(['.', '?', '!', '\n']).next().unwrap_or("") } else { "" };
        let tail = body.rsplit(['.', '?', '!', '\n']).next().unwrap_or("");
        let whole = if body.contains(['.', '?', '!', '\n']) { tail.to_string() } else { format!("{sentence_before} {tail}") };
        hold = if asked || is_question(&whole) { "?".into() } else { ".".into() };
        text = body;
    }
    Shaped { lead, text, held: hold, undo: false }
}

fn key(vk: u16, up: bool) -> INPUT {
    INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(vk), dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() }, ..Default::default() } } }
}

/// Type a piece of dictation where the cursor is. A new line is Shift+Enter: a line break in a
/// document, and in a chat too (where Enter alone would send what's been written so far).
pub fn type_out(text: &str) {
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            let keys = [key(VK_SHIFT.0, false), key(VK_RETURN.0, false), key(VK_RETURN.0, true), key(VK_SHIFT.0, true)];
            unsafe { SendInput(&keys, std::mem::size_of::<INPUT>() as i32) };
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        if !line.is_empty() {
            crate::commands::type_text(line);
        }
    }
}

/// The last `n` characters typed taken back ("scratch that"): Backspace, that many times.
pub fn erase(n: usize) {
    let keys: Vec<INPUT> = (0..n.min(600)).flat_map(|_| [key(VK_BACK.0, false), key(VK_BACK.0, true)]).collect();
    for chunk in keys.chunks(80) {
        unsafe { SendInput(chunk, std::mem::size_of::<INPUT>() as i32) };
        std::thread::sleep(std::time::Duration::from_millis(8));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on() -> Opts {
        Opts { fillers: true, fixes: true, smart: true, words: vec!["SYLVER".into(), "Tauri".into()], names: vec!["Danish Tillu".into(), "Aarav".into()] }
    }

    #[test]
    fn said_marks_become_marks() {
        assert_eq!(marks("hello comma how are you question mark"), "hello, how are you?");
        assert_eq!(marks("Hello, comma, how are you? Question mark."), "Hello, how are you?");
        assert_eq!(marks("first point full stop second point"), "first point. second point");
        assert_eq!(marks("dear sam comma new line thanks"), "dear sam,\nthanks");
        assert_eq!(marks("one new paragraph two"), "one\n\ntwo");
        assert_eq!(marks("git status"), "git status");
        assert_eq!(marks("that was the end period"), "that was the end.");
        assert_eq!(marks("the period of time"), "the period of time");
        assert_eq!(marks("he said open quote hi close quote"), "he said “hi”");
    }

    #[test]
    fn dictation_reads_like_writing() {
        assert_eq!(polish("um I think uh this works", None), "I think this works");
        assert_eq!(polish("dear sam comma new line thanks for the note full stop see you soon", None), "Dear sam,\nThanks for the note. See you soon");
        assert_eq!(polish("And then we left.", Some('t')), "and then we left.");
        assert_eq!(polish("And then we left.", Some('.')), "And then we left.");
        assert_eq!(polish("John said hello", Some('d')), "John said hello");
        assert_eq!(polish("is that right question mark yes", None), "Is that right? Yes");
        assert_eq!(polish("", None), "");
    }

    #[test]
    fn questions_are_asked() {
        for q in ["can you send me the file", "What time is it in Tokyo", "Is John coming", "so do you want to go", "where's the meeting", "We're done, right", "how about lunch", "Did it work"] {
            assert!(is_question(q), "{q}");
        }
        for s in ["What I mean is this", "When I get home I'll call", "Do it now", "Have a good day", "I wonder if it works", "What a day", "Thanks", "Can do"] {
            assert!(!is_question(s), "{s}");
        }
    }

    #[test]
    fn corrections_are_made() {
        assert_eq!(corrections("let's meet at three, no wait, four PM"), "let's meet at four PM");
        assert_eq!(corrections("I'll bring the cake, I mean the cookies"), "I'll bring the cookies");
        assert_eq!(corrections("we need five, no, six chairs"), "we need six chairs");
        assert_eq!(corrections("at three, no, at four"), "at four");
        assert_eq!(corrections("send it Monday scratch that send it Tuesday"), "send it Tuesday");
        assert_eq!(corrections("No, I don't think so"), "No, I don't think so");
        assert_eq!(corrections("what I mean is simple"), "what I mean is simple");
    }

    #[test]
    fn fillers_and_stutters_go() {
        assert_eq!(unfill("um I I think we should we should go"), "I think we should go");
        assert_eq!(unfill("It was, you know, fine"), "It was fine");
        assert_eq!(unfill("I know that that is true"), "I know that that is true");
    }

    #[test]
    fn a_pause_doesnt_end_the_sentence() {
        let o = on();
        let a = shape("I think we should go to the.", "", "", &o);
        assert_eq!(a, Shaped { lead: "".into(), text: "I think we should go to the".into(), held: ".".into(), undo: false });
        let b = shape("Park after lunch.", &a.text, &a.held, &o);
        assert_eq!((b.lead.as_str(), b.text.as_str(), b.held.as_str()), (" ", "Park after lunch", "."));
        let c = shape("Tomorrow it'll rain.", "I think we should go", ".", &o);
        assert_eq!((c.lead.as_str(), c.text.as_str()), (". ", "Tomorrow it'll rain"));
        let d = shape("And then we left.", "We ate", ".", &o);
        assert_eq!((d.lead.as_str(), d.text.as_str()), (" ", "and then we left"));
        let e = shape("Can you send me the file.", "", "", &o);
        assert_eq!((e.text.as_str(), e.held.as_str()), ("Can you send me the file", "?"));
        let f = shape("um ultron is built with tauri.", "", "", &o);
        assert_eq!(f.text, "SYLVER is built with Tauri");
        assert!(shape("Scratch that.", "x", "", &o).undo);
        let g = shape("That's the end period", "", "", &o);
        assert_eq!((g.text.as_str(), g.held.as_str()), ("That's the end.", ""));
    }

    #[test]
    fn names_put_right() {
        let names = [String::from("Danish Tillu"), String::from("Aarav"), String::from("Kristen")];
        assert_eq!(names_fix("I met Denish and Arav today.", names.iter(), true), "I met Danish and Aarav today.");
        assert_eq!(names_fix("Ask Christen about it.", names.iter(), true), "Ask Kristen about it.");
        // (ordinary words, and a sentence's own capital, are left alone)
        assert_eq!(names_fix("Can you see the dish there?", names.iter(), true), "Can you see the dish there?");
        assert_eq!(names_fix("Are you there?", names.iter(), true), "Are you there?");
        assert_eq!(names_fix("Arav, are you coming?", names.iter(), true), "Aarav, are you coming?");
    }
}
