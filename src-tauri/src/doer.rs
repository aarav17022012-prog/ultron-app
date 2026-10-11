//! The task agent: for jobs bigger than one command ("sort my downloads", "research budget phones
//! and put them in Excel", "read this PDF and write me a summary in Word"). The smart brain plans
//! one step at a time: it picks a tool (tools.rs, or ULTRON's own hands in agent.rs), ULTRON runs
//! it, and the brain reads the result before choosing the next one, so a step that goes differently
//! than expected gets corrected instead of derailing the whole task. Each call is held to a JSON
//! schema, so the brain can only ever name a real tool with well-formed details.

use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::agent::{self, Step};
use crate::commands::Reply;
use crate::tools;

const MAX_STEPS: usize = 14;

/// (name, what it does, its details as JSON-schema properties, which are required)
const TOOLS: &[(&str, &str, &str, &[&str])] = &[
    ("find_files", "search for files by words in the name, kind (pdf, images, documents, spreadsheets, presentations, videos, music, archives, installers, code) and age in days; sort newest or biggest",
     r#"{"folder":{"type":"string"},"name":{"type":"string"},"type":{"type":"string"},"days":{"type":"integer"},"sort":{"enum":["newest","biggest"]},"limit":{"type":"integer"}}"#, &["folder"]),
    ("list_folder", "what's in a folder (counts by kind, newest files)", r#"{"folder":{"type":"string"}}"#, &["folder"]),
    ("organize_folder", "sort a folder's loose files into subfolders by kind (Images, PDFs, Documents...) or by month; can be undone", r#"{"folder":{"type":"string"},"by":{"enum":["type","date"]}}"#, &["folder", "by"]),
    ("find_duplicates", "find identical files in a folder (oldest copy is the one to keep)", r#"{"folder":{"type":"string"}}"#, &["folder"]),
    ("move_files", "move files (exact paths, or patterns like downloads/*.pdf) into a folder (created if needed)", r#"{"files":{"type":"array","items":{"type":"string"}},"to":{"type":"string"}}"#, &["files", "to"]),
    ("copy_files", "copy files into a folder", r#"{"files":{"type":"array","items":{"type":"string"}},"to":{"type":"string"}}"#, &["files", "to"]),
    ("rename_file", "rename one file or folder", r#"{"file":{"type":"string"},"new_name":{"type":"string"}}"#, &["file", "new_name"]),
    ("recycle_files", "delete files by moving them to the Recycle Bin", r#"{"files":{"type":"array","items":{"type":"string"}}}"#, &["files"]),
    ("make_folder", "create a folder", r#"{"path":{"type":"string"}}"#, &["path"]),
    ("zip_files", "compress files or folders into a .zip", r#"{"files":{"type":"array","items":{"type":"string"}},"to":{"type":"string"}}"#, &["files", "to"]),
    ("extract_zip", "unzip a .zip file", r#"{"file":{"type":"string"},"to":{"type":"string"}}"#, &["file"]),
    ("read_file", "read a file's text (txt, csv, md, code, Word, PDF, Excel)", r#"{"file":{"type":"string"}}"#, &["file"]),
    ("write_file", "save text as a file (.txt .md .csv .html...)", r#"{"file":{"type":"string"},"text":{"type":"string"}}"#, &["file", "text"]),
    ("excel_table", "build a formatted Excel workbook from a table in one go (numbers like 1,299 or Rs 999 become real numbers), optional chart and totals row; opens it",
     r#"{"title":{"type":"string"},"columns":{"type":"array","items":{"type":"string"}},"rows":{"type":"array","items":{"type":"array","items":{"anyOf":[{"type":"string"},{"type":"number"}]}}},"chart":{"enum":["none","column","bar","line","pie"]},"totals":{"type":"boolean"},"file":{"type":"string"}}"#,
     &["title", "columns", "rows"]),
    ("word_doc", "write a formatted Word document (title, sections with headings; lines starting with - become bullets); opens it",
     r#"{"title":{"type":"string"},"sections":{"type":"array","items":{"type":"object","properties":{"heading":{"type":"string"},"text":{"type":"string"}},"required":["heading","text"],"additionalProperties":false}},"file":{"type":"string"}}"#,
     &["title", "sections"]),
    ("slides", "make a designed PowerPoint presentation about a topic", r#"{"topic":{"type":"string"},"count":{"type":"integer"}}"#, &["topic", "count"]),
    ("email_draft", "open a ready-to-send email draft (the user sends it)", r#"{"to":{"type":"string"},"subject":{"type":"string"},"body":{"type":"string"}}"#, &["to", "subject", "body"]),
    ("message", "type a chat message to someone; it is sent only after the user confirms", r#"{"app":{"enum":["whatsapp","telegram","discord"]},"to":{"type":"string"},"text":{"type":"string"}}"#, &["app", "to", "text"]),
    ("web_search", "search the web; returns titles, links and snippets", r#"{"query":{"type":"string"}}"#, &["query"]),
    ("read_webpage", "read the text of a web page", r#"{"url":{"type":"string"}}"#, &["url"]),
    ("wikipedia", "the Wikipedia summary of a topic", r#"{"topic":{"type":"string"}}"#, &["topic"]),
    ("calc", "exact arithmetic, e.g. 1299*3 - 15%*1299 or sqrt(2)", r#"{"expression":{"type":"string"}}"#, &["expression"]),
    ("open", "open an app by name, or a file, folder or web link", r#"{"target":{"type":"string"}}"#, &["target"]),
    ("type_text", "type exact text into the app in front", r#"{"text":{"type":"string"}}"#, &["text"]),
    ("write_in_app", "compose and type longer writing (an essay, a poem, a letter) into the app in front", r#"{"what":{"type":"string"}}"#, &["what"]),
    ("press_keys", "press a key or shortcut, e.g. ctrl+s, enter, alt+tab", r#"{"keys":{"type":"string"}}"#, &["keys"]),
    ("click", "click something on screen: its number from look (best), its words, or what it looks like (the gear icon, the blue Send button)", r#"{"target":{"type":"string"}}"#, &["target"]),
    ("type_in_field", "type into a box on screen: its number from look (best), or its name (search box, address bar...)", r#"{"field":{"type":"string"},"text":{"type":"string"}}"#, &["field", "text"]),
    ("read_screen", "read or answer a question about what's on screen", r#"{"question":{"type":"string"}}"#, &["question"]),
    ("shop_search", "search a shopping or video site", r#"{"site":{"enum":["amazon","flipkart","youtube","google"]},"query":{"type":"string"}}"#, &["site", "query"]),
    ("command", "a quick PC command: volume, play/pause/next song, play X on spotify, timers and reminders, brightness, dark mode, settings pages, lock the PC",
     r#"{"text":{"type":"string"}}"#, &["text"]),
    ("remember", "save a fact about the user for later", r#"{"fact":{"type":"string"}}"#, &["fact"]),
    ("undo", "undo the last file move, sort or rename", r#"{}"#, &[]),
    ("system_info", "this PC right now: battery, memory, disk space, processor, graphics, Wi-Fi, and the heaviest apps", r#"{}"#, &[]),
    ("run_powershell", "run a PowerShell command and read its output, for anything about this PC that no other tool covers (processes, services, network, startup apps, installed software, settings). Commands that only read run at once; ones that change something are shown to the user first",
     r#"{"command":{"type":"string"}}"#, &["command"]),
    ("list_apps", "the apps and windows that are open right now", r#"{}"#, &[]),
    ("close_app", "close an open app by name (force only if it is frozen)", r#"{"name":{"type":"string"},"force":{"type":"boolean"}}"#, &["name"]),
    ("window", "minimize, maximize, close or snap the window in front (left / right), or switch to another open app", r#"{"action":{"enum":["minimize","maximize","close","left","right","switch"]},"app":{"type":"string"}}"#, &["action"]),
    ("look", "see the app in front the way a person does: a numbered list of everything on it (buttons, boxes, tabs, links, and any text), with where each thing is", r#"{}"#, &[]),
    ("scroll", "scroll the app in front up or down", r#"{"direction":{"enum":["up","down"]},"amount":{"type":"integer"}}"#, &["direction"]),
    ("wait", "wait a few seconds for something to load", r#"{"seconds":{"type":"integer"}}"#, &["seconds"]),
    ("clipboard", "read what is on the clipboard, or put text on it", r#"{"action":{"enum":["read","write"]},"text":{"type":"string"}}"#, &["action"]),
    ("screenshot", "save a picture of the screen in Pictures\\Screenshots", r#"{}"#, &[]),
    ("set_wallpaper", "use an image file as the desktop wallpaper", r#"{"file":{"type":"string"}}"#, &["file"]),
    ("find_software", "search for an app that can be installed, to get its exact id", r#"{"name":{"type":"string"}}"#, &["name"]),
    ("install_app", "install an app by its id from find_software (the user is asked first)", r#"{"id":{"type":"string"}}"#, &["id"]),
    ("uninstall_app", "uninstall an app by name (the user is asked first)", r#"{"name":{"type":"string"}}"#, &["name"]),
    ("power", "lock, sleep, restart, shut down or sign out (everything but lock asks the user first)", r#"{"action":{"enum":["lock","sleep","restart","shutdown","sign_out"]}}"#, &["action"]),
    ("ask_user", "ask the user one question when something essential is missing (ends the task)", r#"{"question":{"type":"string"}}"#, &["question"]),
    ("done", "finish: tell the user, in 1-2 spoken sentences, what you did and where things are", r#"{"answer":{"type":"string"}}"#, &["answer"]),
];

fn system_prompt() -> String {
    let mut s = String::from(
        "You are ULTRON's task agent on the user's Windows PC. You complete the task by calling one tool per turn, as JSON: \
{\"step\": 2 to 6 words for the user about this step, \"tool\": name, \"with\": {...its details}}. After each call you get its result; then choose the next call. \
Finish with done.\n\
Rules:\n\
- Pick the most direct tool: organize_folder sorts a whole folder at once; excel_table builds a whole sheet at once (pass the rows straight in, no CSV first; first column = the names); word_doc writes a whole document at once.\n\
- If the user says where to save something, pass that folder as file.\n\
- Write step in plain English.\n\
- Never invent files, prices, facts or numbers. Look first (find_files, list_folder, read_file, web_search, read_webpage), then act on what you saw.\n\
- Research: web_search, then read_webpage on the 1-3 most useful results, then build the answer or file from what you read.\n\
- Folders can be named desktop, documents, downloads, pictures, music, videos, home, or full paths. Listings and searches name what they found by its path inside the folder they looked in: its full path is that folder, a backslash, then the name.\n\
- Deleting always goes to the Recycle Bin. System folders and code projects are off limits. Messages wait for the user to confirm; emails open as drafts.\n\
- If a step fails, read the error and try a different way once. If it still can't be done, call done and say why.\n\
- Don't repeat a step that already worked. When the task is complete, call done right away.\n\
- Use ask_user only if the task can't start without an answer.\n\
- To work inside any app: open it, call look to see everything on it by number, then click or type_in_field using those numbers (or type_text / press_keys), and look again to check it worked. Numbers change after every click: always use the newest look.\n\
- For questions about the PC itself use system_info, or run_powershell with a command that only reads. Never use run_powershell to delete files or download things.\n\nTools:\n",
    );
    for (name, about, props, req) in TOOLS {
        let args: Vec<String> = serde_json::from_str::<Value>(props).ok().and_then(|v| v.as_object().cloned()).map(|o| {
            o.keys().map(|k| if req.contains(&k.as_str()) { k.clone() } else { format!("{k}?") }).collect()
        }).unwrap_or_default();
        s.push_str(&format!("- {name}({}): {about}\n", args.join(", ")));
    }
    s.push_str(
        "\nExample. Task: put the PDFs from my downloads into a Bills folder on the desktop\n\
{\"step\":\"Finding the PDFs in Downloads\",\"tool\":\"find_files\",\"with\":{\"folder\":\"downloads\",\"type\":\"pdf\"}}\n\
Result: 3 matching file(s) in C:\\Users\\me\\Downloads ...\n\
{\"step\":\"Moving them to Bills\",\"tool\":\"move_files\",\"with\":{\"files\":[\"downloads/*.pdf\"],\"to\":\"desktop/Bills\"}}\n\
Result: Moved 3 of 3 item(s) to C:\\Users\\me\\Desktop\\Bills.\n\
{\"step\":\"Done\",\"tool\":\"done\",\"with\":{\"answer\":\"I moved your 3 PDFs from Downloads into a new Bills folder on your desktop.\"}}\n",
    );
    s
}

/// Every call must be one of the tools, with its own details: the brain can't write anything else.
fn schema() -> Value {
    let alts: Vec<Value> = TOOLS.iter().map(|(name, _, props, req)| {
        let props: Value = serde_json::from_str(props).unwrap_or(json!({}));
        json!({
            "type": "object",
            "properties": {
                // keys in alphabetical order on purpose: the server writes them that way, and this way
                // the brain says what it's doing, then names the tool, then fills in its details.
                // The step is a short plain label: no quotes, backslashes or brackets (with those allowed,
                // the brain once carried on writing the rest of the call inside it), and short because
                // every word of it is a fraction of a second on a processor.
                "step": { "type": "string", "pattern": "^[ !#-Z^-~]{1,56}$" },
                "tool": { "const": name },
                "with": { "type": "object", "properties": props, "required": req, "additionalProperties": false }
            },
            "required": ["step", "tool", "with"],
            "additionalProperties": false
        })
    }).collect();
    json!({ "anyOf": alts })
}

/// Requests that need more than one fixed command.
pub fn wants(text: &str) -> bool {
    let l = format!(" {} ", text.to_lowercase());
    let has = |ws: &[&str]| ws.iter().any(|w| l.contains(w));
    let files = has(&[" file", " folder", "downloads", "desktop", " documents", " pdf", "screenshots", " photos", " pictures", "duplicate", ".zip", " zip ", " unzip", " invoices", " receipts"]);
    let file_verbs = has(&["organi", "sort ", "clean up", "tidy", " move ", " copy ", "rename", "delete", "remove ", "recycle", " zip", "compress", "extract", "how many", "biggest", "largest", "oldest", "find ", "list ", "back up", "backup", "group "]);
    let office = has(&["excel", "spreadsheet", " sheet", "workbook", " table", " chart", " graph", " csv", "word document", "word doc", " docx", "in word", " report", " document "]);
    let research = has(&["research", "compare", "comparison", "look up", "find out", "gather", "collect", " best "]) && has(&[" make ", " create ", " put ", " save ", " write ", " build ", " list ", " table", " excel", " doc", " file"]);
    let reading = has(&["read ", "summari", "go through", "analy"]) && has(&[" file", " pdf", ".txt", ".docx", ".xlsx", " document", " csv", " spreadsheet"]);
    // the PC itself: its battery, space, memory, what's running, installing things, power
    let asks = has(&["how much", "how many", "what's", "what is", "whats", "which", "show me", "show my", "check", "tell me", "do i have", "is my", "what are", "how's", "how is"]);
    let mine = has(&[" my ", "this pc", "this computer", "this laptop", "do i have", " left", " free", " using", "usage", " i have"]);
    let pc = has(&[" battery", "disk space", "free space", "space left", "storage", " ram", "memory usage", "much memory", "free memory", " cpu", "processor", " gpu", "graphics card", "ip address", "wi-fi name",
        "wifi name", "uptime", " specs", "system info"]);
    let apps = has(&["which apps", "what apps", "running apps", "open apps", "apps are running", "apps are open", "what's running", "what is running", "using the most", "close all", "close every", "taking up",
        "is slowing", "slowing down my"]);
    let software = has(&[" install ", "uninstall ", "startup apps", "start-up apps", "powershell", "run the command", "run command", "command prompt", "on my clipboard", "to the clipboard", "to my clipboard",
        "as my wallpaper", "as wallpaper", "as the wallpaper", "wallpaper to "]);
    let power = has(&["restart", "reboot", "shut down", "shutdown", "sign out", "log off", "log out", " to sleep"]) && has(&[" pc", "computer", "laptop", "system", "windows"]);
    (files && file_verbs) || (asks && pc && mine) || apps || software || power || office || research || reading || has(&["step by step", " and then ", "undo that", "undo the last"]) && !has(&["what is", "who is"])
}

/// Long jobs inside a website or an app ("go to ... and build me ...", "sign me up on ..."). They need the
/// stronger planner, so they are only taken on when your own key is switched on (main.rs asks).
pub fn wants_big(text: &str) -> bool {
    let l = format!(" {} ", text.to_lowercase());
    let has = |ws: &[&str]| ws.iter().any(|w| l.contains(w));
    let place = has(&[" go to ", " go in ", " go into ", " go on ", " open ", " in vs code", " in dia ", " in chrome", " in the browser", " on linkedin", " website", " ai studio"]);
    (place && has(&[" and ", " then "]))
        || has(&[" sign me up", " sign up ", " create an account", " make an account", " register me", " build me ", " build an app", " build a website", " make me an app", " make me a website", " make me a game",
            " make an app", " fill in the form", " fill out ", " message everyone", " message all ", " text everyone", " book me ", " apply for ", " apply to "])
}

/// "Make me an app / a game / a website": something ULTRON can build itself by writing the file. The brain on
/// this PC can do that (slowly: a few minutes), so these are taken on with or without a key.
pub fn wants_build(text: &str) -> bool {
    let l = format!(" {} ", text.to_lowercase());
    if [" excel", " word doc", " in word", " powerpoint", " presentation", " slides", " folder", " shortcut", " account", " review", " email", " message "].iter().any(|w| l.contains(w)) {
        return false;
    }
    let words: Vec<&str> = l.split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric())).collect();
    let made = |w: &&str| ["app", "application", "game", "website", "webpage", "site", "page", "calculator", "clock", "timer", "quiz"].contains(w);
    words.iter().position(|w| ["make", "build", "create", "code", "develop", "program"].contains(w)).is_some_and(|v| words[v + 1..].iter().take(7).any(made))
}

/// How a build is done (added to the request, so the plan is the same every time).
const BUILD_HOW: &str = "(How: write the whole thing yourself as ONE complete, working .html file, with its CSS and JavaScript inside it and no outside files, using write_file with a short file name ending in .html; then open that file; then call done and say where it is. It must really work: every button does its job. Make it look good: a centred card, rounded corners, a dark background, clear type.)";

fn emit(app: &AppHandle, i: usize, label: &str, state: &str) {
    let _ = app.emit_to("main", "agent-step", json!({ "i": i, "n": i + 1, "label": label, "state": state }));
}

fn short(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n).collect::<String>() + "…" }
}

/// Run one tool. GUI work goes through ULTRON's own hands (its eyed cursor), the rest straight to tools.rs.
fn run_tool(app: &AppHandle, sess: &mut agent::Session, tool: &str, a: &Value) -> Result<String, String> {
    let st = |k: &str| a.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    match tool {
        "find_files" => tools::find_files(a),
        "list_folder" => tools::list_folder(a),
        "organize_folder" => tools::organize_folder(a),
        "find_duplicates" => tools::find_duplicates(a),
        "move_files" => tools::move_files(a),
        "copy_files" => tools::copy_files(a),
        "rename_file" => tools::rename_file(a),
        "recycle_files" => tools::recycle_files(a),
        "make_folder" => tools::make_folder(a),
        "zip_files" => tools::zip_files(a),
        "extract_zip" => tools::extract_zip(a),
        "read_file" => tools::read_file(a),
        "write_file" => tools::write_file(app, a),
        "excel_table" => tools::excel_table(app, a),
        "word_doc" => tools::word_doc(app, a),
        "web_search" => tools::web_search(a),
        "read_webpage" => tools::read_webpage(a),
        "wikipedia" => tools::wikipedia(a),
        "calc" => tools::calc(a),
        "undo" => tools::undo(),
        "system_info" => tools::system_info(),
        "run_powershell" => tools::run_powershell(a),
        "list_apps" => tools::list_apps(),
        "close_app" => tools::close_app(a),
        "clipboard" => tools::clipboard(a),
        "screenshot" => tools::screenshot(),
        "set_wallpaper" => tools::set_wallpaper(a),
        "find_software" => tools::find_software(a),
        "install_app" => tools::install_app(a),
        "uninstall_app" => tools::uninstall_app(a),
        "power" => tools::power(a),
        "look" => sess.look(app),
        "scroll" => sess.scroll(app, st("direction") != "up", a.get("amount").and_then(|n| n.as_i64()).unwrap_or(5) as i32),
        "wait" => {
            std::thread::sleep(Duration::from_secs(a.get("seconds").and_then(|n| n.as_u64()).unwrap_or(2).clamp(1, 15)));
            Ok("Waited.".into())
        }
        "window" => match st("action").as_str() {
            "minimize" => sess.run(app, Step::Minimize),
            "maximize" => sess.run(app, Step::Maximize),
            "close" => sess.run(app, Step::CloseWindow),
            "left" => sess.run(app, Step::Snap(-1)),
            "right" => sess.run(app, Step::Snap(1)),
            "switch" if !st("app").is_empty() => sess.run(app, Step::SwitchTo(st("app"))),
            _ => Err("Say which app to switch to.".into()),
        },
        "remember" => {
            let f = st("fact");
            if f.is_empty() {
                return Err("Nothing to remember.".into());
            }
            crate::memory::add(app, &f);
            Ok(format!("Remembered: {f}"))
        }
        "open" => {
            let t = st("target");
            let looks_path = t.starts_with("http") || t.contains('\\') || t.contains('/') || t.contains(':') || std::path::Path::new(&t).extension().is_some();
            if looks_path { tools::open_path(a) } else { sess.run(app, Step::Open(t)) }
        }
        "slides" => {
            let n = a.get("count").and_then(|c| c.as_u64()).unwrap_or(6).clamp(1, 20) as usize;
            sess.run(app, Step::Present(st("topic"), n))
        }
        "email_draft" => sess.run(app, Step::Email(st("to"), st("subject"), st("body"))),
        "message" => sess.run(app, Step::Message(st("app"), st("to"), st("text"))),
        "type_text" => sess.run(app, Step::Write(st("text"))),
        "write_in_app" => sess.run(app, Step::Compose(st("what"))),
        "press_keys" => match agent::parse_step(&format!("press {}", st("keys").replace('+', " "))) {
            s @ Step::Keys(..) => sess.run(app, s),
            _ => Err(format!("I don't know the keys “{}”.", st("keys"))),
        },
        // by number (from look) when it's a number, by its words otherwise
        "click" => match st("target").trim().trim_start_matches('#').parse::<usize>() {
            Ok(n) => sess.click_mark(app, n),
            Err(_) => sess.run(app, Step::Click(st("target"))),
        },
        "type_in_field" => match st("field").trim().trim_start_matches('#').parse::<usize>() {
            Ok(n) => sess.type_at(app, n, &st("text")),
            Err(_) => sess.run(app, Step::TypeIn(st("field"), st("text"))),
        },
        "read_screen" => sess.run(app, Step::Screen(st("question"))),
        "shop_search" => sess.run(app, Step::SiteSearch(st("site"), st("query"))),
        "command" => {
            let t = st("text");
            match agent::parse_step(&t) {
                Step::Command(c) => sess.run(app, Step::Command(c)),
                s => sess.run(app, s),
            }
        }
        _ => Err(format!("There's no tool called {tool}.")),
    }
}

/// What the brain sees: the task, then each call it made and what came back. Older results are
/// shortened so a long task still fits.
fn messages(app: &AppHandle, goal: &str, log: &[(String, String)]) -> Vec<(String, String)> {
    let (time, day) = crate::commands::now_strings();
    let year = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() }.wYear;
    let who = crate::intro::user_name(app).map(|n| format!(" The user's name is {n}.")).unwrap_or_default();
    let who = format!("{who} Their own folder on this PC is {}.", std::env::var("USERPROFILE").unwrap_or_default());
    let facts = crate::memory::context(app);
    let mut m = vec![
        ("system".to_string(), system_prompt()),
        ("user".to_string(), format!("Task: {goal}\n(It is {day} {year}, {time}.{who}{})", if facts.is_empty() { String::new() } else { format!(" {facts}") })),
    ];
    // only if a long task outgrows the brain's memory are the oldest results shortened
    let total: usize = log.iter().map(|(c, r)| c.len() + r.len()).sum();
    let mut over = total.saturating_sub(11_000);
    for (call, result) in log.iter() {
        m.push(("assistant".into(), call.clone()));
        let r = if over > 0 && result.len() > 300 {
            over = over.saturating_sub(result.len() - 300);
            short(result, 300)
        } else {
            result.clone()
        };
        m.push(("user".into(), format!("Result: {r}")));
    }
    m
}

/// Read the instructions into the smart brain ahead of time, so the first step starts at once.
pub fn prefill(app: &AppHandle) {
    let a = messages(app, "a", &[]);
    let b = messages(app, "b", &[]);
    crate::ai::agent_ready(app, &a, &b, false);
}

/// Things that change the system wait for a yes: the exact command, install or shutdown is shown first.
/// Returns the question to ask and what the "yes" button says.
fn ask_first(tool: &str, a: &Value) -> Option<(String, &'static str)> {
    let st = |k: &str| a.get(k).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    match tool {
        "run_powershell" => matches!(tools::ps_risk(&st("command")), tools::Risk::Ask).then(|| (format!("Run this command?  {}", short(&st("command"), 220)), "Run it")),
        "install_app" => Some((format!("Install {} with winget?", st("id")), "Install")),
        "uninstall_app" => Some((format!("Uninstall {}?", st("name")), "Uninstall")),
        "power" => match st("action").as_str() {
            "lock" => None,
            "sleep" => Some(("Put the PC to sleep now?".into(), "Sleep")),
            "restart" => Some(("Restart the PC now? Unsaved work will be lost.".into(), "Restart")),
            "shutdown" => Some(("Shut the PC down now? Unsaved work will be lost.".into(), "Shut down")),
            _ => Some(("Sign out now? Unsaved work will be lost.".into(), "Sign out")),
        },
        "close_app" if a.get("force").and_then(|f| f.as_bool()).unwrap_or(false) => Some((format!("Force {} to close? Unsaved work in it will be lost.", st("name")), "Force close")),
        "recycle_files" => {
            let files: Vec<String> = a.get("files").and_then(|f| f.as_array()).map(|f| f.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            (files.len() > 25 || files.iter().any(|f| f.contains('*'))).then(|| (format!("Move these to the Recycle Bin?  {}", short(&files.join(", "), 200)), "Recycle"))
        }
        _ => None,
    }
}

/// A task that stopped to ask: everything needed to carry on once the user says yes.
struct Waiting {
    goal: String,
    log: Vec<(String, String)>,
    actions: Vec<String>,
    call: String,
    tool: String,
    args: Value,
    label: String,
    at: Instant,
}

static WAITING: std::sync::Mutex<Option<Waiting>> = std::sync::Mutex::new(None);

/// "yes" / "no" (typed, spoken or tapped) to the question a task asked. None when nothing is waiting,
/// or when the user moved on to something else.
pub fn answer_pending(app: &AppHandle, text: &str) -> Option<Reply> {
    let w = WAITING.lock().unwrap().take()?;
    if w.at.elapsed() > Duration::from_secs(600) {
        return None;
    }
    let t = text.to_lowercase();
    let t = t.trim_matches(|c: char| !c.is_alphanumeric()).trim();
    let yes = ["yes", "yeah", "yep", "yup", "sure", "ok", "okay", "go ahead", "do it", "run it", "confirm", "please do", "go on", "install", "install it", "restart", "shut down", "recycle"];
    let no = ["no", "nope", "cancel", "don't", "dont", "never mind", "nevermind", "stop", "leave it"];
    if no.iter().any(|n| t == *n || t.starts_with(&format!("{n} "))) {
        return Some(Reply { reply: "Okay, I left it alone.".into(), action: w.actions.join(" · "), icon: "agent", ok: true });
    }
    if !yes.iter().any(|y| t == *y || t.starts_with(&format!("{y} "))) {
        return None; // something else entirely: the question lapses
    }
    crate::log_line(&format!("doer: approved {}", short(&w.call, 200)));
    let mut sess = agent::Session::new();
    emit(app, w.actions.len(), &w.label, "run");
    let result = run_tool(app, &mut sess, &w.tool, &w.args);
    emit(app, w.actions.len(), &w.label, if result.is_ok() { "ok" } else { "fail" });
    let text = result.unwrap_or_else(|e| format!("Error: {e}"));
    crate::log_line(&format!("doer result: {}", short(&text.replace('\n', " | "), 300)));
    let (mut log, mut actions) = (w.log, w.actions);
    actions.push(w.label);
    log.push((w.call, short(&text, 3000)));
    run_from(app, &w.goal, log, actions)
}

/// "Make me an app": ULTRON writes it itself, as one file that runs when opened, saves it with the rest of
/// what it makes (Documents\ULTRON Files\Apps) and opens it. It is written as plain text, not inside a tool
/// call: held to the tool-call form the brain wrote a third as fast (measured: 1.9 tokens a second against
/// its usual 5 or so), because nearly every line of code had to be turned down and written again.
fn build(app: &AppHandle, goal: &str) -> Option<Reply> {
    let t0 = Instant::now();
    let system = "You write small web apps. Reply with ONE complete HTML document and nothing else: it starts with <!DOCTYPE html> and ends with </html>, with its CSS in a <style> tag and its JavaScript in a <script> tag, \
and it uses no outside files, libraries, fonts or images. It must really work: every button and field does its job, and nothing is left as a placeholder. Make it look good: a dark background, a centred card with rounded corners, \
clear type, comfortable spacing. Keep it compact (under 200 lines). No explanations and no code fences.";
    let msgs = vec![("system".to_string(), system.to_string()), ("user".to_string(), goal.to_string())];
    let _ = app.emit_to("main", "ai-phase", "Writing it…");
    emit(app, 0, "Writing the app", "run");
    let mut shown = Instant::now();
    let text = crate::ai::write_long(app, &msgs, 3600, |t| {
        if shown.elapsed() > Duration::from_millis(1500) {
            shown = Instant::now();
            emit(app, 0, &format!("Writing the app, {} lines so far", t.lines().count()), "run");
        }
    });
    if crate::ai::cancelled() {
        emit(app, 0, "Writing the app", "fail");
        return Some(Reply { reply: "Okay, I stopped.".into(), action: String::new(), icon: "agent", ok: true });
    }
    let text = text?; // (no brain to write with: the task agent takes it the long way)
    // the document itself, from its first line to its last, whatever was said around it
    let low = text.to_ascii_lowercase();
    let Some(from) = low.find("<!doctype").or_else(|| low.find("<html")) else {
        emit(app, 0, "Writing the app", "fail");
        crate::log_line(&format!("doer: the build came back without a page ({} characters)", text.len()));
        return Some(Reply { reply: "I couldn't write that one properly. Try asking for something a little simpler.".into(), action: String::new(), icon: "agent", ok: false });
    };
    let (html, whole) = match low.rfind("</html>") {
        Some(to) if to > from => (text[from..to + 7].to_string(), true),
        _ => (format!("{}\n</script></body></html>", &text[from..]), false),
    };
    emit(app, 0, &format!("Writing the app, {} lines", html.lines().count()), "ok");
    // a name from the request: "make me a tip calculator app" is tip-calculator.html
    let skip = ["make", "build", "create", "code", "develop", "program", "write", "me", "us", "a", "an", "the", "my", "new", "simple", "small", "working", "cool", "nice", "please", "app", "application", "website", "webpage",
        "site", "page", "web", "that", "which", "for", "to", "in", "on", "with", "and", "of", "go", "vs", "visual", "studio", "dia", "html", "can", "you", "i", "want", "need"];
    let words: Vec<String> = goal.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty() && !skip.contains(w)).take(3).map(String::from).collect();
    let slug = if words.is_empty() { "app".to_string() } else { words.join("-") };
    let dir = crate::rec::folder(app).parent()?.join("Apps");
    let _ = std::fs::create_dir_all(&dir);
    let (mut path, mut n) = (dir.join(format!("{slug}.html")), 2);
    while path.exists() {
        path = dir.join(format!("{slug} {n}.html"));
        n += 1;
    }
    emit(app, 1, "Saving it", "run");
    if std::fs::write(&path, &html).is_err() {
        emit(app, 1, "Saving it", "fail");
        return Some(Reply { reply: "I wrote it, but couldn't save the file.".into(), action: String::new(), icon: "agent", ok: false });
    }
    emit(app, 1, "Saving it", "ok");
    let name = path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
    let shown_path = path.to_string_lossy().to_string();
    // "...in VS Code": its code opens there as well (when VS Code is on this PC)
    let l = goal.to_lowercase();
    if l.contains("vs code") || l.contains("vscode") || l.contains("visual studio code") {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("cmd").args(["/c", "code", &shown_path]).creation_flags(0x0800_0000).spawn();
    }
    emit(app, 2, "Opening it", "run");
    let mut sess = agent::Session::new();
    let opened = run_tool(app, &mut sess, "open", &json!({ "target": shown_path })).is_ok();
    sess.finish(app, true);
    emit(app, 2, "Opening it", if opened { "ok" } else { "fail" });
    crate::log_line(&format!("doer: built {name} in {} s ({} lines{})", t0.elapsed().as_secs(), html.lines().count(), if whole { "" } else { ", cut short" }));
    Some(Reply {
        reply: format!("I built it{}. The file is {name}, in Documents, ULTRON Files, Apps.{}", if opened { " and it's open now" } else { "" },
            if whole { "" } else { " I ran out of room before the end, so it may not be complete: ask me for a simpler one." }),
        action: "Writing the app · Saving it · Opening it".into(),
        icon: "agent",
        ok: true,
    })
}

pub fn run(app: &AppHandle, goal: &str) -> Option<Reply> {
    *WAITING.lock().unwrap() = None;
    crate::log_line(&format!("doer: task {goal:?}"));
    if wants_build(goal) && !goal.contains(BUILD_HOW) {
        if let Some(r) = build(app, goal) {
            return Some(r);
        }
        return run_from(app, &format!("{goal}\n{BUILD_HOW}"), Vec::new(), Vec::new());
    }
    run_from(app, goal, Vec::new(), Vec::new())
}

fn run_from(app: &AppHandle, goal: &str, mut log: Vec<(String, String)>, mut actions: Vec<String>) -> Option<Reply> {
    let t0 = Instant::now();
    let schema = schema();
    let mut sess = agent::Session::new();
    let mut last_call = String::new();
    let mut repeats = 0;
    let mut unreadable = 0;
    // the agent's instructions come off disk in a moment rather than being read again
    crate::ai::agent_ready(app, &messages(app, "a", &[]), &messages(app, "b", &[]), true);
    // "...saved in that folder": new files go where the request said, whether or not the brain passes it on
    let save = tools::save_dir_in(goal);
    if let Some(d) = &save {
        crate::log_line(&format!("doer: saving into {}", d.display()));
    }
    tools::set_save_dir(save);
    // (a build is one long piece of writing: more room for it, more time, and a word about the wait)
    let building = goal.contains(BUILD_HOW);
    let _ = app.emit_to("main", "ai-phase", if building { "Writing it… this takes a few minutes" } else { "Planning…" });
    // (your own key's model can carry a long job: more steps, and more time, than this PC's brain gets)
    let big = crate::cloud::active(app).is_some();
    for i in 0..if big { 45 } else { MAX_STEPS } {
        if crate::ai::cancelled() {
            sess.finish(app, false);
            return Some(Reply { reply: "Okay, I stopped.".into(), action: actions.join(" · "), icon: "agent", ok: true });
        }
        if t0.elapsed() > Duration::from_secs(if big || building { 1500 } else { 480 }) {
            break;
        }
        let ts = Instant::now();
        let msgs = messages(app, goal, &log);
        let Some(out) = crate::ai::agent_call(app, &msgs, &schema, if building { 3200 } else { 1600 }) else {
            if i == 0 {
                return None; // no brain to plan with: the older single-shot route takes over
            }
            break;
        };
        if crate::ai::cancelled() {
            sess.finish(app, false);
            return Some(Reply { reply: "Okay, I stopped.".into(), action: actions.join(" · "), icon: "agent", ok: true });
        }
        let call: Value = match serde_json::from_str(out.trim()) {
            Ok(v) => v,
            Err(_) => {
                // try once more with a clean slate (the bad text isn't kept: it would only be copied again)
                crate::log_line(&format!("doer: unreadable call {:?}", short(&out, 200)));
                unreadable += 1;
                if unreadable >= 2 {
                    break;
                }
                continue;
            }
        };
        let tool = call.get("tool").and_then(|t| t.as_str()).unwrap_or("").to_string();
        let args = call.get("with").cloned().unwrap_or(json!({}));
        let say = call.get("step").and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
        let compact = serde_json::to_string(&call).unwrap_or_default();
        crate::log_line(&format!("doer step {i} ({} ms): {}", ts.elapsed().as_millis(), short(&compact, 300)));
        match tool.as_str() {
            "done" => {
                let answer = args.get("answer").and_then(|a| a.as_str()).unwrap_or("Done.").trim().to_string();
                sess.finish(app, true);
                crate::log_line(&format!("doer: finished in {} s after {i} step(s)", t0.elapsed().as_secs()));
                return Some(Reply { reply: answer, action: actions.join(" · "), icon: "agent", ok: true });
            }
            "ask_user" => {
                let q = args.get("question").and_then(|a| a.as_str()).unwrap_or("What should I do?").trim().to_string();
                sess.finish(app, true);
                return Some(Reply { reply: q, action: actions.join(" · "), icon: "agent", ok: true });
            }
            _ => {}
        }
        if compact == last_call {
            repeats += 1;
            if repeats >= 2 {
                break;
            }
            log.push((compact.clone(), "You already did exactly that and it worked. Do the next part of the task, or call done.".into()));
            continue;
        }
        repeats = 0;
        last_call = compact.clone();
        // (a label that ran into its length limit stops at its last whole word)
        let label = if say.is_empty() {
            tool.replace('_', " ")
        } else if say.len() >= 54 {
            format!("{}…", say.rsplit_once(' ').map(|(a, _)| a).unwrap_or(&say).trim_end_matches([',', ';', ':', ' ']))
        } else {
            say.clone()
        };
        // changing the system waits for a yes; the task picks up again from here
        if let Some((question, yes)) = ask_first(&tool, &args) {
            sess.finish(app, true);
            crate::log_line(&format!("doer: asking first: {question}"));
            *WAITING.lock().unwrap() = Some(Waiting { goal: goal.to_string(), log, actions: actions.clone(), call: compact, tool, args, label, at: Instant::now() });
            let _ = app.emit_to("main", "agent-confirm", json!({ "text": question, "yes": yes, "sys": true }));
            return Some(Reply { reply: format!("{question} Say yes to go ahead."), action: actions.join(" · "), icon: "agent", ok: true });
        }
        emit(app, actions.len(), &label, "run");
        let result = run_tool(app, &mut sess, &tool, &args);
        let ok = result.is_ok();
        emit(app, actions.len(), &label, if ok { "ok" } else { "fail" });
        let text = match result {
            Ok(t) => t,
            Err(e) if e == "Stopped." => {
                sess.finish(app, false);
                return Some(Reply { reply: "Okay, I stopped.".into(), action: actions.join(" · "), icon: "agent", ok: true });
            }
            Err(e) => format!("Error: {e}"),
        };
        crate::log_line(&format!("doer result: {}", short(&text.replace('\n', " | "), 300)));
        actions.push(label);
        // a chat message waits for the user's go-ahead: hand over now
        if tool == "message" && ok {
            sess.finish(app, true);
            return Some(Reply { reply: format!("{text} Say “send it” or tap Send when you're happy with it."), action: actions.join(" · "), icon: "agent", ok: true });
        }
        // (a long file it has just written isn't read back to it on every later step: that would crowd out the rest)
        let kept = if tool == "write_file" && compact.len() > 1200 {
            let mut c = call.clone();
            if let Some(t) = c.pointer_mut("/with/text") {
                *t = json!("(the text you wrote: saved)");
            }
            serde_json::to_string(&c).unwrap_or(compact)
        } else {
            compact
        };
        log.push((kept, short(&text, 3000)));
    }
    sess.finish(app, false);
    let did = if actions.is_empty() { String::new() } else { format!(" So far I: {}.", actions.join(", ").to_lowercase()) };
    Some(Reply { reply: format!("I couldn't finish that one.{did}"), action: actions.join(" · "), icon: "agent", ok: false })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn routing_and_schema() {
        for (q, yes) in [
            ("organize my downloads folder", true),
            ("find duplicate photos in my pictures and delete them", true),
            ("research the best budget phones under 20000 and put them in excel", true),
            ("make a table of the planets and their moons in excel", true),
            ("summarize the pdf report in my downloads", true),
            ("zip my screenshots folder", true),
            ("open spotify", false),
            ("what is the capital of australia", false),
            ("tell me a joke", false),
            ("set a timer for 5 minutes", false),
            ("how much battery do I have left", true),
            ("how much free disk space is left on my pc", true),
            ("which apps are using the most memory", true),
            ("install vlc", true),
            ("restart my computer", true),
            ("what is a cpu", false),
            ("what's the weather", false),
        ] {
            assert_eq!(wants(q), yes, "{q}");
        }
        let s = schema();
        assert_eq!(s["anyOf"].as_array().unwrap().len(), TOOLS.len());
        let p = system_prompt();
        println!("system prompt: {} chars, ~{} tokens", p.len(), p.len() / 4);
        let _ = std::fs::write(std::env::temp_dir().join("ultron-agent-system.txt"), &p);
    }
}
