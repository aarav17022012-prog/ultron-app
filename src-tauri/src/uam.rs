//! ULTRON Action Model: a small neural network written and trained from scratch, only for
//! ULTRON. It reads a request and decides, in microseconds, what kind of thing you want:
//! one of ULTRON's actions (open, play, write, click, message, slides...) or just a chat.
//!
//! It's a softmax classifier over hashed word and letter patterns, trained when ULTRON starts
//! (well under a second) on thousands of example requests generated from templates below.
//! When it says "this is an action" but ULTRON's rules can't parse it, the local brain is
//! asked to rewrite the request into ULTRON's own command language, under a grammar that
//! makes anything else impossible (see `ai::plan_actions`).

use std::sync::OnceLock;

const DIM: usize = 1 << 15;
pub const LABELS: [&str; 22] = [
    "chat", "open", "close", "play", "media", "volume", "search", "shop", "write", "type_in", "click", "keys", "save", "slides",
    "message", "email", "rewrite", "screen", "timer", "window", "system", "files",
];

pub struct Model {
    w: Vec<f32>, // LABELS.len() x DIM
}

fn hash(s: &str, salt: u64) -> usize {
    let mut h: u64 = 0xcbf29ce484222325 ^ salt;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (h as usize) & (DIM - 1)
}

/// words, word pairs, and letter triples (so typos and new words still land near known ones)
fn features(text: &str) -> Vec<usize> {
    let t: String = text.to_lowercase().chars().map(|c| if c.is_alphanumeric() || c == '\'' { c } else { ' ' }).collect();
    let words: Vec<&str> = t.split_whitespace().collect();
    let mut f = Vec::with_capacity(words.len() * 8);
    for (i, w) in words.iter().enumerate() {
        f.push(hash(w, 1));
        if i == 0 {
            f.push(hash(w, 7)); // the first word matters most
        }
        if let Some(n) = words.get(i + 1) {
            f.push(hash(&format!("{w} {n}"), 2));
        }
        let padded: Vec<char> = format!("<{w}>").chars().collect();
        for g in padded.windows(3) {
            f.push(hash(&g.iter().collect::<String>(), 3));
        }
    }
    f.push(hash(&format!("len{}", words.len().min(12)), 4));
    f
}

impl Model {
    fn scores(&self, f: &[usize]) -> Vec<f32> {
        let mut s: Vec<f32> = (0..LABELS.len()).map(|k| f.iter().map(|&i| self.w[k * DIM + i]).sum()).collect();
        let m = s.iter().cloned().fold(f32::MIN, f32::max);
        let z: f32 = s.iter_mut().map(|x| { *x = (*x - m).exp(); *x }).sum();
        s.iter_mut().for_each(|x| *x /= z);
        s
    }

    /// (label, confidence)
    pub fn predict(&self, text: &str) -> (&'static str, f32) {
        let p = self.scores(&features(text));
        let (k, c) = p.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap();
        (LABELS[k], *c)
    }

    fn train(data: &[(String, usize)]) -> Model {
        let mut m = Model { w: vec![0.0; LABELS.len() * DIM] };
        let feats: Vec<(Vec<usize>, usize)> = data.iter().map(|(t, y)| (features(t), *y)).collect();
        let mut order: Vec<usize> = (0..feats.len()).collect();
        let mut seed = 0x2545F4914F6CDD1Du64;
        for epoch in 0..10 {
            // shuffle (xorshift, fixed seed: the same model every time)
            for i in (1..order.len()).rev() {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                order.swap(i, (seed as usize) % (i + 1));
            }
            let lr = 0.5 / (1.0 + epoch as f32 * 0.6);
            for &i in &order {
                let (f, y) = &feats[i];
                let p = m.scores(f);
                for k in 0..LABELS.len() {
                    let g = p[k] - if k == *y { 1.0 } else { 0.0 };
                    if g.abs() < 1e-4 {
                        continue;
                    }
                    for &j in f {
                        m.w[k * DIM + j] -= lr * g;
                    }
                }
            }
        }
        m
    }
}

/* ---------------- the examples it learns from ---------------- */

fn examples() -> Vec<(String, usize)> {
    let apps = ["notepad", "chrome", "spotify", "powerpoint", "word", "excel", "whatsapp", "discord", "calculator", "settings", "file explorer", "vs code",
        "youtube", "telegram", "edge", "paint", "camera", "steam", "outlook", "photos"];
    let songs = ["billie jean", "shape of you", "blinding lights", "lofi beats", "some jazz", "believer", "kesariya", "despacito", "my playlist", "bohemian rhapsody"];
    let topics = ["black holes", "climate change", "the solar system", "india", "artificial intelligence", "world war 2", "photosynthesis", "the roman empire", "mars", "cricket"];
    let things = ["wireless headphones", "a gaming mouse", "running shoes", "a phone case", "a laptop", "a smartwatch", "a water bottle"];
    let people = ["priya", "mom", "rahul", "dad", "aman", "sara", "my boss", "john"];
    let questions = ["what is the capital of france", "who won the world cup", "how are you", "tell me a joke", "what's the meaning of life", "explain quantum physics",
        "why is the sky blue", "how do i cook pasta", "what time is it", "who is elon musk", "what's 25 times 4", "are you there", "thank you", "good morning",
        "what can you do", "i'm bored", "how tall is mount everest", "what is love", "recommend me a movie", "how does a car engine work", "what's the weather like",
        "do you like music", "write me a poem in the chat", "summarize the french revolution", "what does api mean", "can you help me with my homework",
        "who are you", "what's your name", "translate hello to spanish", "is it going to rain", "how many planets are there", "what should i eat today",
        "that's cool", "nice", "lol", "you're awesome", "how old is the universe", "what happened to nana patekar", "tell me about black holes", "define gravity"];
    let mut d: Vec<(String, &str)> = Vec::new();
    let mut add = |label: &'static str, s: String| d.push((s, label));
    for a in apps {
        for t in ["open {}", "launch {}", "start {}", "can you open {}", "please open {}", "open up {} for me", "fire up {}", "bring up {}", "run {}", "get {} open", "i want to use {}", "go to {}"] {
            add("open", t.replace("{}", a));
        }
        for t in ["close {}", "quit {}", "exit {}", "shut {} down", "kill {}", "close the {} window"] {
            add("close", t.replace("{}", a));
        }
        for t in ["switch to {}", "go back to {}", "show me {}", "minimize {}", "maximize {}"] {
            add("window", t.replace("{}", a));
        }
    }
    for s in songs {
        for t in ["play {}", "play {} on spotify", "put on {}", "i want to hear {}", "can you play {}", "play {} on youtube", "queue up {}", "start playing {}", "blast {}", "let's listen to {}"] {
            add("play", t.replace("{}", s));
        }
    }
    for t in ["pause", "pause the music", "resume", "next song", "skip this song", "previous track", "go back a song", "stop the music", "play the next one", "unpause", "skip", "next track", "skip this track", "hit the next song", "go to the next track", "last song", "replay that song"] {
        add("media", t.into());
    }
    for t in ["volume up", "turn it up", "turn the volume down", "set volume to 40", "mute", "unmute", "make it louder", "quieter please", "set the volume to 70 percent", "lower the volume"] {
        add("volume", t.into());
    }
    for q in topics.iter().chain(["cat videos", "best pizza near me", "rust tutorials", "today's news", "how to tie a tie", "how to fix a flat tire", "what time the match starts", "how to make tea"].iter()) {
        for t in ["search for {}", "google {}", "look up {}", "search the web for {}", "search youtube for {}", "find {} online", "do a search on {}"] {
            add("search", t.replace("{}", q));
        }
    }
    for x in things {
        for t in ["find {} on amazon", "show me {} under 2000 on flipkart", "search amazon for {}", "buy {}", "shop for {}", "cheapest {} on amazon", "order {} online", "get me {} from flipkart"] {
            add("shop", t.replace("{}", x));
        }
    }
    for q in topics {
        for t in ["write an essay on {}", "write 100 words about {}", "type a paragraph on {} in notepad", "compose a poem about {}", "draft a story about {}", "write me something about {}",
            "write a short note on {}", "type out a summary of {}"] {
            add("write", t.replace("{}", q));
        }
        for t in ["make a presentation on {}", "create a ppt about {}", "make me a 2 slide presentation on {}", "build a slide deck on {}", "prepare slides about {}",
            "open powerpoint and make a presentation on {}", "i need a 5 slide ppt on {}", "design a deck about {}"] {
            add("slides", t.replace("{}", q));
        }
    }
    for w in ["hello world", "cats", "my name", "rust", "the address", "weather"] {
        for t in ["type {} in the search box", "enter {} in the search bar", "put {} in the name field", "write {} in the address bar", "type {} into the input"] {
            add("type_in", t.replace("{}", w));
        }
    }
    for b in ["send", "submit", "ok", "the play button", "next", "sign in", "the search icon", "accept", "new tab", "the first result", "cancel", "download"] {
        for t in ["click {}", "click on {}", "press the {} button", "tap {}", "hit {}", "select {}", "double click {}"] {
            add("click", t.replace("{}", b));
        }
    }
    for t in ["press ctrl s", "press enter", "hit escape", "press alt tab", "ctrl c", "press the windows key", "press f5", "copy that", "paste it", "undo", "select all", "redo", "new tab", "refresh the page"] {
        add("keys", t.into());
    }
    for t in ["save it", "save this as notes", "save the file", "save as report", "save the document as homework", "save it on the desktop"] {
        add("save", t.into());
    }
    for p in people {
        for t in ["message {} on whatsapp saying i'm late", "text {} that dinner is ready", "tell {} on telegram good night", "send {} a message saying hi", "whatsapp {} i'll call later",
            "dm {} on discord gg"] {
            add("message", t.replace("{}", p));
        }
        for t in ["email {} about the meeting", "send an email to {} saying thanks", "write an email to {} asking for leave", "mail {} the update"] {
            add("email", t.replace("{}", p));
        }
    }
    for t in ["rewrite this to sound formal", "fix the grammar of this", "make the selected text shorter", "translate the selected text to hindi", "make this sound more professional",
        "rephrase what i selected", "summarize the selected text", "proofread this", "make it friendlier", "correct my spelling"] {
        add("rewrite", t.into());
    }
    for t in ["what's on my screen", "summarize this page", "read my screen", "what am i looking at", "explain what's on the screen", "describe this picture", "what does this window say",
        "look at my screen", "what's in this image", "summarize this article"] {
        add("screen", t.into());
    }
    for t in ["set a timer for 5 minutes", "remind me in 10 minutes to call mom", "set an alarm for 7 am", "wake me up at 6", "remind me to drink water in an hour", "timer 30 seconds",
        "start a 20 minute timer", "set a reminder for 3 pm"] {
        add("timer", t.into());
    }
    for t in ["snap this window left", "snap it right", "move this to the other screen", "close this window", "minimize this", "maximize the window", "make it full screen", "show desktop"] {
        add("window", t.into());
    }
    for t in ["turn on dark mode", "light mode", "set brightness to 50", "dim the screen", "open bluetooth settings", "open wifi settings", "lock the pc", "take a screenshot",
        "brighter", "turn off the wifi", "open display settings", "shut down the computer", "restart my pc", "go to sleep"] {
        add("system", t.into());
    }
    for t in ["find file resume", "where is my file budget", "open my latest download", "create a folder called projects on the desktop", "new folder photos in documents",
        "open the downloads folder", "find my homework file", "make a new folder named work"] {
        add("files", t.into());
    }
    for q in questions {
        add("chat", q.into());
        add("chat", format!("ultron {q}"));
        add("chat", format!("hey {q}"));
    }
    // two-step requests are actions too
    for a in ["notepad", "chrome", "spotify"] {
        add("open", format!("open {a} and write hello"));
        add("open", format!("open {a} then search for cats"));
    }
    let label = |l: &str| LABELS.iter().position(|x| *x == l).unwrap();
    d.into_iter().map(|(s, l)| (s, label(l))).collect()
}

static MODEL: OnceLock<Model> = OnceLock::new();

/// Train it in the background when ULTRON starts.
pub fn start() {
    std::thread::spawn(|| {
        let t0 = std::time::Instant::now();
        let data = examples();
        let n = data.len();
        let _ = MODEL.set(Model::train(&data));
        crate::log_line(&format!("action model trained on {n} examples in {} ms", t0.elapsed().as_millis()));
    });
}

/// What kind of request this is, if the model is ready: (label, confidence).
pub fn classify(text: &str) -> Option<(&'static str, f32)> {
    MODEL.get().map(|m| m.predict(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn learns() {
        let t0 = std::time::Instant::now();
        let data = examples();
        let m = Model::train(&data);
        println!("{} examples, trained in {} ms", data.len(), t0.elapsed().as_millis());
        // phrasings it never saw
        let cases = [
            ("could you pull up notepad real quick", "open"), ("blast some arijit singh", "play"), ("hit the next track", "media"), ("crank the volume", "volume"),
            ("make me 3 slides about volcanoes", "slides"), ("shoot rahul a whatsapp saying on my way", "message"), ("who invented the telephone", "chat"),
            ("what's the best way to learn guitar", "chat"), ("jot down a haiku about the rain in notepad", "write"), ("hit the submit button", "click"),
            ("look for cheap earbuds on amazon", "shop"), ("drop me a reminder in 15 minutes to stretch", "timer"), ("tell me something interesting", "chat"),
            ("put my pc to sleep", "system"), ("translate the highlighted text into french", "rewrite"), ("what's this image showing", "screen"),
            ("google how to tie a tie", "search"), ("open up excel please", "open"), ("how far is the moon", "chat"), ("type my email in the login box", "type_in"),
        ];
        let mut ok = 0;
        for (q, want) in cases {
            let (got, c) = m.predict(q);
            println!("{q:?} -> {got} ({c:.2}){}", if got == want { String::new() } else { format!("   <-- wanted {want}") });
            ok += (got == want) as usize;
        }
        println!("{ok}/{} correct", cases.len());
        assert!(ok * 100 >= cases.len() * 80);
    }
}
