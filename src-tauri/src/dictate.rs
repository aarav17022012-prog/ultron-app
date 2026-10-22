//! Dictation: what you say, made ready to type.
//!
//! "Ultron, start dictation" turns it on: from then on everything heard is typed where the cursor is,
//! until "stop dictation". The recognizer is given longer pauses before it decides you've finished
//! and the last thing typed as its context (both from the page, through voice_config). Here the words
//! are tidied: the marks you say aloud ("comma", "question mark", "new line") become the marks, the
//! "um"s go, sentences start with a capital, and a piece that carries on a sentence doesn't.

use windows::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_RETURN, VK_SHIFT};

/// The marks that can be said aloud: (the words, what is typed, whether it sticks to the word before).
const MARKS: [(&[&str], &str, bool); 14] = [
    (&["new", "paragraph"], "\n\n", false),
    (&["next", "paragraph"], "\n\n", false),
    (&["new", "line"], "\n", false),
    (&["next", "line"], "\n", false),
    (&["newline"], "\n", false),
    (&["question", "mark"], "?", true),
    (&["exclamation", "mark"], "!", true),
    (&["exclamation", "point"], "!", true),
    (&["full", "stop"], ".", true),
    (&["semicolon"], ";", true),
    (&["semi", "colon"], ";", true),
    (&["colon"], ":", true),
    (&["comma"], ",", true),
    (&["ellipsis"], "…", true),
];

fn bare(w: &str) -> String {
    w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase()
}

/// The said-aloud marks turned into the marks themselves; nothing else about the words is touched.
/// ("type hello comma how are you question mark" types "hello, how are you?")
pub fn marks(heard: &str) -> String {
    let words: Vec<&str> = heard.split_whitespace().collect();
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
        let glue = text.is_empty() || text.ends_with('\n') || w.starts_with('\n') || (w.chars().count() == 1 && ",.?!;:…".contains(&w));
        if !glue {
            text.push(' ');
        }
        text.push_str(&w);
    }
    text
}

/// A piece of dictation ready to type. `after` is the last character already typed (None at the start):
/// it decides whether this piece opens a sentence or carries one on.
pub fn polish(heard: &str, after: Option<char>) -> String {
    const FILLERS: [&str; 7] = ["um", "uh", "uhm", "umm", "erm", "er", "hmm"];
    const CARRY_ON: [&str; 40] = [
        "and", "but", "so", "then", "the", "a", "an", "to", "of", "in", "on", "it", "is", "that", "this", "we", "you", "he", "she", "they", "there", "because", "which", "with", "for", "if",
        "or", "when", "what", "how", "at", "as", "by", "from", "my", "your", "our", "its", "also", "not",
    ];
    let kept: Vec<&str> = heard.split_whitespace().filter(|w| !FILLERS.contains(&bare(w).as_str())).collect();
    let mut text = marks(&kept.join(" "));
    let opens = after.map_or(true, |c| ".?!\n".contains(c));
    if !opens {
        // carrying on mid-sentence: the recognizer's capital on an ordinary word comes down
        let first: String = text.chars().take_while(|c| c.is_alphabetic()).collect();
        if CARRY_ON.contains(&first.to_lowercase().as_str()) && first.chars().next().is_some_and(|c| c.is_uppercase()) {
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
        } else if !c.is_whitespace() && !"\"'“”)".contains(c) {
            ended = false;
            cap = false;
        }
    }
    out
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

/// One heard piece, tidied (a space first when it follows other words), as it would be typed.
pub fn tidy(heard: &str, after: Option<char>) -> String {
    let text = polish(heard, after);
    if text.is_empty() {
        return text;
    }
    let lead = after.is_some_and(|c| !c.is_whitespace()) && !text.starts_with(['\n', ',', '.', '?', '!', ';', ':', '…']);
    if lead {
        format!(" {text}")
    } else {
        text
    }
}

/// One heard piece: tidied, typed, and given back as typed.
pub fn piece(heard: &str, after: Option<char>) -> String {
    let typed = tidy(heard, after);
    if !typed.is_empty() {
        type_out(&typed);
    }
    typed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn said_marks_become_marks() {
        assert_eq!(marks("hello comma how are you question mark"), "hello, how are you?");
        assert_eq!(marks("Hello, comma, how are you? Question mark."), "Hello, how are you?");
        assert_eq!(marks("first point full stop second point"), "first point. second point");
        assert_eq!(marks("dear sam comma new line thanks"), "dear sam,\nthanks");
        assert_eq!(marks("one new paragraph two"), "one\n\ntwo");
        assert_eq!(marks("git status"), "git status");
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
}
