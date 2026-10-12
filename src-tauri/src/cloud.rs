//! Bring your own key. Out of the box everything ULTRON's brain does happens on this PC. If you add a
//! key from an AI service (Anthropic's Claude, OpenAI, Google's Gemini, OpenRouter, or anything that
//! speaks the same language), answers and translations come from that service instead: quicker than a
//! laptop's processor, and better informed. No key, the switch off, or no connection: it works locally,
//! exactly as before.
//!
//!   - The key is kept by Windows (Credential Manager), never in a file, and never goes back to the page.
//!   - With a key, what's sent is the question, the last few exchanges, and what was looked up for it.
//!     Your screen, your files, your email, text you select and tasks are never sent: those stay with
//!     the brain on this PC.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct Cfg {
    pub on: bool,
    pub provider: String,
    pub model: String,
    /// only for "custom": where the service lives
    pub base: String,
}

/// (id, what it's called, where it lives, the model picked until you choose one)
const PROVIDERS: [(&str, &str, &str, &str); 5] = [
    ("anthropic", "Claude", "https://api.anthropic.com/v1", "claude-haiku-5-5"),
    ("openai", "OpenAI", "https://api.openai.com/v1", ""),
    ("gemini", "Gemini", "https://generativelanguage.googleapis.com/v1beta/openai", ""),
    ("openrouter", "OpenRouter", "https://openrouter.ai/api/v1", ""),
    ("custom", "Other", "", ""),
];

pub fn name_of(provider: &str) -> &'static str {
    PROVIDERS.iter().find(|p| p.0 == provider).map(|p| p.1).unwrap_or("the service")
}

fn path(app: &AppHandle) -> PathBuf {
    let d = app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir());
    let _ = std::fs::create_dir_all(&d);
    d.join("cloud.json")
}

pub fn cfg(app: &AppHandle) -> Cfg {
    std::fs::read_to_string(path(app)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn save(app: &AppHandle, c: &Cfg) {
    let _ = crate::keep(path(app), serde_json::to_string(c).unwrap_or_default());
    let _ = app.emit_to("main", "cloud-changed", status(app));
}

/* ---------------- the key, in Windows Credential Manager ---------------- */

fn target(provider: &str) -> Vec<u16> {
    format!("ULTRON key: {provider}").encode_utf16().chain(Some(0)).collect()
}

fn keep_key(provider: &str, key: &str) -> bool {
    use windows::core::PWSTR;
    use windows::Win32::Security::Credentials::{CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC};
    let mut t = target(provider);
    let mut user: Vec<u16> = "ULTRON".encode_utf16().chain(Some(0)).collect();
    let mut blob = key.as_bytes().to_vec();
    let c = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(t.as_mut_ptr()),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: PWSTR(user.as_mut_ptr()),
        ..Default::default()
    };
    unsafe { CredWriteW(&c, 0) }.is_ok()
}

fn key_of(provider: &str) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Security::Credentials::{CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC};
    let t = target(provider);
    let mut p: *mut CREDENTIALW = std::ptr::null_mut();
    unsafe {
        CredReadW(PCWSTR(t.as_ptr()), CRED_TYPE_GENERIC, None, &mut p).ok()?;
        let c = &*p;
        let out = String::from_utf8_lossy(std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize)).to_string();
        CredFree(p as _);
        (!out.is_empty()).then_some(out)
    }
}

fn forget_key(provider: &str) {
    use windows::core::PCWSTR;
    use windows::Win32::Security::Credentials::{CredDeleteW, CRED_TYPE_GENERIC};
    let t = target(provider);
    let _ = unsafe { CredDeleteW(PCWSTR(t.as_ptr()), CRED_TYPE_GENERIC, None) };
}

/* ---------------- talking to the service ---------------- */

/// A service to talk to right now: the switch is on and there's a key for it.
pub struct Active {
    pub provider: String,
    pub model: String,
    base: String,
    key: String,
}

fn base_of(c: &Cfg) -> Option<String> {
    // (a developer run may point any provider somewhere else, to test against a stand-in on this PC)
    let dev = std::env::var("ULTRON_DEV_INBOX").is_ok();
    let b = if c.provider == "custom" || (dev && !c.base.is_empty()) { c.base.trim().trim_end_matches('/').to_string() } else { PROVIDERS.iter().find(|p| p.0 == c.provider)?.2.to_string() };
    let local = b.starts_with("http://127.0.0.1") || b.starts_with("http://localhost");
    (b.starts_with("https://") || local).then_some(b)
}

pub fn active(app: &AppHandle) -> Option<Active> {
    let c = cfg(app);
    if !c.on || c.model.is_empty() {
        return None;
    }
    Some(Active { base: base_of(&c)?, key: key_of(&c.provider)?, provider: c.provider, model: c.model })
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(6)).timeout_read(Duration::from_secs(40)).build()
}

fn with_key(req: ureq::Request, provider: &str, key: &str) -> ureq::Request {
    if provider == "anthropic" {
        req.set("x-api-key", key).set("anthropic-version", "2023-06-01")
    } else {
        req.set("Authorization", &format!("Bearer {key}"))
    }
}

/// Why a call didn't work, in words for a person.
fn why(provider: &str, e: ureq::Error) -> String {
    let who = name_of(provider);
    match e {
        ureq::Error::Status(code, resp) => {
            let said = resp.into_string().ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| {
                v.pointer("/error/message").or_else(|| v.pointer("/message")).or_else(|| v.get("error")).and_then(|m| m.as_str()).map(|m| m.chars().take(140).collect::<String>())
            });
            match code {
                401 | 403 => format!("{who} refused that key. Check it was copied whole, and that it's still active."),
                404 => format!("{who} doesn't know that model{}", said.map(|s| format!(": {s}")).unwrap_or_else(|| ".".into())),
                402 => format!("{who} says the account has no credit left."),
                429 => format!("{who} says you've reached its limit for now. Try again in a little while."),
                500..=599 => format!("{who} is having trouble right now ({code})."),
                _ => format!("{who} said no ({code}){}", said.map(|s| format!(": {s}")).unwrap_or_else(|| ".".into())),
            }
        }
        ureq::Error::Transport(_) => format!("I couldn't reach {who}. Check your connection."),
    }
}

/// The models a key can use (this is also the test that the key works). Only ones that chat.
fn models(provider: &str, base: &str, key: &str) -> Result<Vec<String>, String> {
    let url = if provider == "anthropic" { format!("{base}/models?limit=100") } else { format!("{base}/models") };
    let v: Value = with_key(agent().get(&url), provider, key).call().map_err(|e| why(provider, e))?.into_json().map_err(|_| format!("{} answered with something I couldn't read.", name_of(provider)))?;
    let not_chat = ["embed", "tts", "whisper", "audio", "image", "dall", "moderation", "realtime", "transcri", "rerank", "guard", "speech", "vision-preview", "imagen", "veo", "aqa", "babbage", "davinci", "sora"];
    let mut ids: Vec<String> = v.get("data").and_then(|d| d.as_array()).map(|a| a.iter().filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from)).collect()).unwrap_or_default();
    ids.retain(|id| !not_chat.iter().any(|w| id.to_lowercase().contains(w)));
    ids.truncate(150);
    Ok(ids)
}

/// A good first choice from a service's list: its quick, capable everyday model.
fn pick(provider: &str, ids: &[String]) -> String {
    let fallback = PROVIDERS.iter().find(|p| p.0 == provider).map(|p| p.3).unwrap_or("");
    if !fallback.is_empty() && (ids.is_empty() || ids.iter().any(|i| i == fallback)) {
        return fallback.to_string();
    }
    for want in ["haiku", "flash", "mini", "fast", "small", "turbo"] {
        // (the newest of them: lists come newest first from most services, so the first match)
        if let Some(i) = ids.iter().find(|i| i.to_lowercase().contains(want) && !i.to_lowercase().contains("nano") && !i.to_lowercase().contains("lite")) {
            return i.clone();
        }
    }
    ids.first().cloned().unwrap_or_else(|| fallback.to_string())
}

/// Roles as a service wants them: every "system" message gathered into one (in order), and never two
/// messages from the same side in a row.
fn shape(msgs: &[(String, String)]) -> (String, Vec<(String, String)>) {
    let system = msgs.iter().filter(|(r, _)| r == "system").map(|(_, c)| c.as_str()).collect::<Vec<_>>().join("\n\n");
    let mut out: Vec<(String, String)> = Vec::new();
    for (r, c) in msgs.iter().filter(|(r, c)| r != "system" && !c.trim().is_empty()) {
        match out.last_mut() {
            Some((lr, lc)) if lr == r => {
                lc.push_str("\n\n");
                lc.push_str(c);
            }
            None if r != "user" => {} // (a conversation starts with the user)
            _ => out.push((r.clone(), c.clone())),
        }
    }
    (system, out)
}

fn body(a: &Active, msgs: &[(String, String)], max: usize, temp: f64) -> Value {
    let (system, turns) = shape(msgs);
    let mut list: Vec<Value> = turns.iter().map(|(r, c)| json!({ "role": r, "content": c })).collect();
    if a.provider == "anthropic" {
        return json!({ "model": a.model, "max_tokens": max, "temperature": temp, "stream": true, "system": system, "messages": list });
    }
    if !system.is_empty() {
        list.insert(0, json!({ "role": "system", "content": system }));
    }
    let mut b = json!({ "model": a.model, "stream": true, "messages": list });
    if a.provider == "openai" {
        // (its newer models count their private thinking against the limit and choose their own temperature)
        b["max_completion_tokens"] = json!(max * 4 + 400);
    } else {
        b["max_tokens"] = json!(max);
        b["temperature"] = json!(temp);
    }
    b
}

/// The words in one event of the reply, in either dialect; `Err` when the event says something went wrong.
fn words(provider: &str, v: &Value) -> Result<Option<String>, String> {
    if let Some(e) = v.get("error") {
        let m = e.get("message").and_then(|m| m.as_str()).or_else(|| e.as_str()).unwrap_or("it stopped part-way");
        return Err(format!("{}: {}", name_of(provider), m.chars().take(140).collect::<String>()));
    }
    let t = if provider == "anthropic" { v.pointer("/delta/text") } else { v.pointer("/choices/0/delta/content") };
    Ok(t.and_then(|t| t.as_str()).filter(|t| !t.is_empty()).map(String::from))
}

/// Ask the service. `on_piece` gets the whole reply so far as it grows; `sentences` ends it early the way
/// local answers are ended (0 = as long as it needs); `stop` is asked between pieces.
/// Ok with whatever arrived (even if it broke off part-way); Err only when nothing did.
pub fn chat(a: &Active, msgs: &[(String, String)], max: usize, temp: f64, sentences: usize, mut on_piece: impl FnMut(&str), stop: impl Fn() -> bool) -> Result<String, String> {
    let url = if a.provider == "anthropic" { format!("{}/messages", a.base) } else { format!("{}/chat/completions", a.base) };
    let t0 = Instant::now();
    let resp = with_key(agent().post(&url), &a.provider, &a.key).set("content-type", "application/json").send_json(body(a, msgs, max, temp)).map_err(|e| why(&a.provider, e))?;
    let mut text = String::new();
    let mut trouble = None;
    for line in BufReader::new(resp.into_reader()).lines() {
        let Ok(line) = line else { break };
        if stop() {
            break;
        }
        let Some(data) = line.strip_prefix("data:").map(str::trim) else { continue };
        if data == "[DONE]" {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
        match words(&a.provider, &v) {
            Ok(Some(t)) => {
                text.push_str(&t);
                if let Some(cut) = crate::ai::cut_point(&text, sentences) {
                    text.truncate(cut); // enough said: hang up
                    on_piece(&text);
                    break;
                }
                on_piece(&text);
            }
            Ok(None) => {
                if v.get("type").and_then(|t| t.as_str()) == Some("message_stop") {
                    break;
                }
            }
            Err(e) => {
                trouble = Some(e);
                break;
            }
        }
    }
    if text.trim().is_empty() {
        return Err(trouble.unwrap_or_else(|| format!("{} sent back nothing.", name_of(&a.provider))));
    }
    crate::log_line(&format!("{} ({}) answered in {} ms", name_of(&a.provider), a.model, t0.elapsed().as_millis()));
    Ok(text)
}

/* ---------------- what Settings and the notch show and change ---------------- */

pub fn status(app: &AppHandle) -> Value {
    let c = cfg(app);
    let keys: Vec<&str> = PROVIDERS.iter().filter(|p| key_of(p.0).is_some()).map(|p| p.0).collect();
    json!({
        "on": c.on && key_of(&c.provider).is_some() && !c.model.is_empty(),
        "provider": c.provider, "name": name_of(&c.provider), "model": c.model, "base": c.base,
        "keys": keys,
        "providers": PROVIDERS.iter().map(|p| json!({ "id": p.0, "name": p.1 })).collect::<Vec<_>>(),
    })
}

/// Check a key with its service, keep it, and switch over. `key` empty: use the one already kept for
/// that service (to fetch its models again, or to come back to it).
pub fn connect(app: &AppHandle, provider: &str, key: &str, base: &str, model: &str) -> Result<Value, String> {
    if !PROVIDERS.iter().any(|p| p.0 == provider) {
        return Err("I don't know that service.".into());
    }
    let key: String = key.chars().filter(|c| !c.is_whitespace()).collect();
    let key = if key.is_empty() { key_of(provider).ok_or("Paste your key first.")? } else { key };
    let mut c = Cfg { on: true, provider: provider.into(), model: model.trim().into(), base: base.trim().trim_end_matches('/').into() };
    let b = base_of(&c).ok_or(if provider == "custom" { "That address needs to start with https:// (or be on this PC)." } else { "That service has no address." })?;
    let ids = match models(provider, &b, &key) {
        Ok(ids) => ids,
        // some services of the "other" kind don't list their models: then the name you typed is tried directly
        Err(e) if provider == "custom" && !c.model.is_empty() => {
            let a = Active { provider: provider.into(), model: c.model.clone(), base: b.clone(), key: key.clone() };
            chat(&a, &[("user".into(), "Say OK.".into())], 8, 0.0, 0, |_| {}, || false).map_err(|_| e)?;
            Vec::new()
        }
        Err(e) => return Err(e),
    };
    if c.model.is_empty() || (!ids.is_empty() && !ids.contains(&c.model)) {
        // (the model you had with this service before, if it's still offered)
        let before = cfg(app);
        c.model = if before.provider == provider && ids.contains(&before.model) { before.model } else { pick(provider, &ids) };
    }
    if c.model.is_empty() {
        return Err("Type the name of the model to use, as the service lists it.".into());
    }
    if !keep_key(provider, &key) {
        return Err("The key works, but Windows wouldn't store it, so I can't keep using it.".into());
    }
    crate::log_line(&format!("your own key: connected to {} ({}), {} models offered", name_of(provider), c.model, ids.len()));
    save(app, &c);
    Ok(json!({ "models": ids, "model": c.model }))
}

/// Flip the switch, or change the model.
pub fn set(app: &AppHandle, on: Option<bool>, model: Option<String>) {
    let mut c = cfg(app);
    if let Some(v) = on {
        c.on = v && key_of(&c.provider).is_some();
    }
    if let Some(m) = model.filter(|m| !m.trim().is_empty()) {
        c.model = m.trim().to_string();
    }
    save(app, &c);
}

/// Throw a service's key away. If it was the one in use, ULTRON is back on this PC.
pub fn forget(app: &AppHandle, provider: &str) {
    forget_key(provider);
    let mut c = cfg(app);
    if c.provider == provider {
        c.on = false;
    }
    save(app, &c);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speaks_both_dialects() {
        let msgs: Vec<(String, String)> = [("system", "You are ULTRON."), ("user", "hi"), ("assistant", "Hello."), ("system", "It is 4 PM."), ("user", "what time is it?")]
            .iter().map(|(r, c)| (r.to_string(), c.to_string())).collect();
        let claude = Active { provider: "anthropic".into(), model: "claude-haiku-5-5".into(), base: "https://api.anthropic.com/v1".into(), key: "k".into() };
        let b = body(&claude, &msgs, 90, 0.4);
        assert_eq!(b["system"], "You are ULTRON.\n\nIt is 4 PM.");
        assert_eq!(b["messages"].as_array().unwrap().len(), 3);
        assert_eq!(b["messages"][0]["role"], "user");
        assert_eq!(b["max_tokens"], 90);
        let other = Active { provider: "openrouter".into(), model: "m".into(), base: "https://openrouter.ai/api/v1".into(), key: "k".into() };
        let b = body(&other, &msgs, 90, 0.4);
        assert_eq!(b["messages"][0]["role"], "system");
        assert_eq!(b["messages"].as_array().unwrap().len(), 4);
        assert!(body(&Active { provider: "openai".into(), ..other }, &msgs, 90, 0.4).get("max_tokens").is_none());
        // two user turns in a row become one; an answer with nothing before it is dropped
        let odd: Vec<(String, String)> = [("assistant", "x"), ("user", "a"), ("user", "b")].iter().map(|(r, c)| (r.to_string(), c.to_string())).collect();
        assert_eq!(shape(&odd).1, vec![("user".to_string(), "a\n\nb".to_string())]);
        // the words in an event
        assert_eq!(words("anthropic", &json!({ "type": "content_block_delta", "delta": { "type": "text_delta", "text": "Hi" } })).unwrap().as_deref(), Some("Hi"));
        assert_eq!(words("openai", &json!({ "choices": [{ "delta": { "content": " there" } }] })).unwrap().as_deref(), Some(" there"));
        assert_eq!(words("openai", &json!({ "choices": [{ "delta": {} }] })).unwrap(), None);
        assert!(words("anthropic", &json!({ "type": "error", "error": { "message": "Overloaded" } })).is_err());
        // first choices
        assert_eq!(pick("anthropic", &["claude-opus-5-5".into(), "claude-haiku-5-5".into()]), "claude-haiku-5-5");
        assert_eq!(pick("openai", &["big-model".into(), "some-mini".into()]), "some-mini");
        assert_eq!(pick("custom", &["only-one".into()]), "only-one");
        // where services live
        assert!(base_of(&Cfg { provider: "custom".into(), base: "http://example.com/v1".into(), ..Default::default() }).is_none());
        assert!(base_of(&Cfg { provider: "custom".into(), base: "http://127.0.0.1:8080/v1".into(), ..Default::default() }).is_some());
        assert_eq!(base_of(&Cfg { provider: "gemini".into(), ..Default::default() }).unwrap(), "https://generativelanguage.googleapis.com/v1beta/openai");
    }
}
