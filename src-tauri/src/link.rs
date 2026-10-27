//! The phone link, laptop side: protocol version 1 as written in the phone project's docs/LINK_PROTOCOL.md (the phone's
//! LinkMessage / LinkCodec / PairingCode). The laptop finds the phone on the local network (mDNS), opens a WebSocket
//! to it (one JSON message per text frame), both send `hello`, and the highest common version is used. Pairing: the
//! phone shows a 6-digit code, you type it here, the laptop sends `pair_request` and the phone answers `pair_accept`
//! (its token, kept) or `pair_reject`. Then "on my phone, open WhatsApp" goes as a `command`, and the phone's
//! `command_result` is shown. A `ping` every 15 s while idle; a `ping` from the phone is answered with a `pong`.
//!
//! Not in the protocol yet, so set here and to be agreed with the phone side (LINK_PROTOCOL.md says discovery and
//! sockets aren't built): the mDNS service type the phone advertises (SERVICE), and the WebSocket path (PATH). The
//! port is whatever the phone's mDNS record gives.

use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

/// (assumed, to be agreed with the phone side: not in LINK_PROTOCOL.md v1)
pub const SERVICE: &str = "_ultron-link._tcp.local.";
/// (assumed, likewise)
pub const PATH: &str = "/";
const MIN_VERSION: i64 = 1;
const MAX_VERSION: i64 = 1;

#[derive(Serialize, Clone, Debug)]
pub struct Phone {
    pub name: String,
    pub host: String,
    pub port: u16,
}

#[derive(Serialize, Clone, Default)]
pub struct State {
    /// "off" | "connecting" | "connected" (hello done) | "paired"
    pub state: String,
    pub phone: String,
    pub version: i64,
}

enum Out {
    Send(Value),
    Close,
}

struct Live {
    out: Sender<Out>,
    waiting: std::sync::Arc<Mutex<HashMap<String, Sender<Value>>>>,
}

static LIVE: Mutex<Option<Live>> = Mutex::new(None);
static STATE: Mutex<Option<State>> = Mutex::new(None);
static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn state() -> State {
    STATE.lock().unwrap().clone().unwrap_or(State { state: "off".into(), ..Default::default() })
}

fn set_state(app: &AppHandle, s: State) {
    *STATE.lock().unwrap() = Some(s.clone());
    let _ = app.emit_to("main", "link-state", s);
}

fn data(app: &AppHandle) -> std::path::PathBuf {
    app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir()).join("link.json")
}

/// This laptop's stable id for `hello` (made once, kept).
fn device_id(app: &AppHandle) -> String {
    let p = data(app);
    let mut v: Value = std::fs::read_to_string(&p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({}));
    if let Some(id) = v["deviceId"].as_str() {
        return id.to_string();
    }
    let mut b = [0u8; 8];
    let _ = getrandom::fill(&mut b);
    let id: String = b.iter().map(|x| format!("{x:02x}")).collect();
    v["deviceId"] = json!(id);
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let _ = std::fs::write(&p, v.to_string());
    id
}

fn keep_token(app: &AppHandle, phone: &str, token: &str) {
    let p = data(app);
    let mut v: Value = std::fs::read_to_string(&p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({}));
    v["tokens"][phone] = json!(token);
    let _ = std::fs::write(&p, v.to_string());
}

/// Phones on this network, for up to `wait`.
pub fn find(wait: Duration) -> Result<Vec<Phone>, String> {
    let d = mdns_sd::ServiceDaemon::new().map_err(|e| format!("couldn't look on the network ({e})"))?;
    let rx = d.browse(SERVICE).map_err(|e| format!("couldn't look on the network ({e})"))?;
    let t0 = Instant::now();
    let mut out: Vec<Phone> = Vec::new();
    while t0.elapsed() < wait {
        let Ok(ev) = rx.recv_timeout(Duration::from_millis(200)) else { continue };
        if let mdns_sd::ServiceEvent::ServiceResolved(info) = ev {
            let Some(addr) = info.get_addresses().iter().find(|a| a.is_ipv4()).or(info.get_addresses().iter().next()) else { continue };
            let name = info.get_fullname().trim_end_matches(SERVICE).trim_end_matches('.').to_string();
            if !out.iter().any(|p| p.name == name) {
                out.push(Phone { name, host: addr.to_string(), port: info.get_port() });
            }
        }
    }
    let _ = d.shutdown();
    Ok(out)
}

/// The version both speak (`theirs`: their hello), or None when there's no overlap.
fn version(theirs: &Value) -> Option<i64> {
    let max = theirs["protocolVersion"].as_i64()?;
    let min = theirs["minProtocolVersion"].as_i64().unwrap_or(max);
    let v = MAX_VERSION.min(max);
    (v >= MIN_VERSION.max(min)).then_some(v)
}

/// Connect to a phone found by `find`: both say hello, the version is agreed, and the link stays open.
pub fn connect(app: &AppHandle, phone: &Phone) -> Result<State, String> {
    close();
    set_state(app, State { state: "connecting".into(), phone: phone.name.clone(), version: 0 });
    let host = if phone.host.contains(':') { format!("[{}]", phone.host) } else { phone.host.clone() };
    let url = format!("ws://{host}:{}{PATH}", phone.port);
    let stream = TcpStream::connect_timeout(&format!("{host}:{}", phone.port).parse().map_err(|_| "bad address".to_string())?, Duration::from_secs(4)).map_err(|e| {
        set_state(app, State { state: "off".into(), ..Default::default() });
        format!("couldn't reach the phone ({e})")
    })?;
    let _ = stream.set_nodelay(true);
    let (mut ws, _) = tungstenite::client(url.as_str(), stream).map_err(|e| format!("the phone didn't take the connection ({e})"))?;
    let me = json!({"type": "hello", "deviceId": device_id(app), "deviceName": format!("{}'s laptop", whoami()), "kind": "laptop", "protocolVersion": MAX_VERSION, "minProtocolVersion": MIN_VERSION});
    ws.send(tungstenite::Message::Text(me.to_string().into())).map_err(|e| e.to_string())?;
    // their hello first (anything else before it is dropped)
    let theirs = loop {
        match ws.read().map_err(|e| e.to_string())? {
            tungstenite::Message::Text(t) => match serde_json::from_str::<Value>(&t) {
                Ok(v) if v["type"] == "hello" && v["deviceId"].as_str().is_some_and(|s| !s.is_empty()) && v["protocolVersion"].is_i64() => break v,
                _ => continue,
            },
            tungstenite::Message::Close(_) => return Err("the phone closed the connection".into()),
            _ => continue,
        }
    };
    let Some(v) = version(&theirs) else {
        let _ = ws.send(tungstenite::Message::Text(json!({"type": "error", "code": "unsupported_version", "message": format!("laptop speaks {MIN_VERSION}..{MAX_VERSION}")}).to_string().into()));
        let _ = ws.close(None);
        set_state(app, State { state: "off".into(), ..Default::default() });
        return Err("the phone speaks a different version of the link".into());
    };
    let name = theirs["deviceName"].as_str().filter(|s| !s.is_empty()).unwrap_or(&phone.name).to_string();
    let phone_id = theirs["deviceId"].as_str().unwrap_or("").to_string();
    let _ = ws.get_ref().set_read_timeout(Some(Duration::from_millis(50)));
    let (tx, rx) = channel::<Out>();
    let waiting: std::sync::Arc<Mutex<HashMap<String, Sender<Value>>>> = Default::default();
    *LIVE.lock().unwrap() = Some(Live { out: tx, waiting: waiting.clone() });
    let st = State { state: "connected".into(), phone: name, version: v };
    set_state(app, st.clone());
    let app2 = app.clone();
    std::thread::spawn(move || pump(app2, ws, rx, waiting, phone_id));
    Ok(st)
}

/// The link's own thread: what's to be sent goes out, what comes in is answered or handed to whoever waits for it,
/// and a ping goes every 15 s while nothing else does.
fn pump(app: AppHandle, mut ws: tungstenite::WebSocket<TcpStream>, rx: Receiver<Out>, waiting: std::sync::Arc<Mutex<HashMap<String, Sender<Value>>>>, phone_id: String) {
    let mut last_out = Instant::now();
    loop {
        while let Ok(o) = rx.try_recv() {
            match o {
                Out::Send(v) => {
                    if ws.send(tungstenite::Message::Text(v.to_string().into())).is_err() {
                        return gone(&app);
                    }
                    last_out = Instant::now();
                }
                Out::Close => {
                    let _ = ws.close(None);
                    let _ = ws.flush();
                    return gone(&app);
                }
            }
        }
        if last_out.elapsed() > Duration::from_secs(15) {
            let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
            if ws.send(tungstenite::Message::Text(json!({"type": "ping", "t": t}).to_string().into())).is_err() {
                return gone(&app);
            }
            last_out = Instant::now();
        }
        match ws.read() {
            Ok(tungstenite::Message::Text(t)) => {
                let Ok(v) = serde_json::from_str::<Value>(&t) else {
                    let _ = ws.send(tungstenite::Message::Text(json!({"type": "error", "code": "bad_message", "message": "not JSON"}).to_string().into()));
                    continue;
                };
                match v["type"].as_str().unwrap_or("") {
                    "ping" if v["t"].is_i64() => {
                        let _ = ws.send(tungstenite::Message::Text(json!({"type": "pong", "t": v["t"]}).to_string().into()));
                    }
                    "pong" => {}
                    "pair_accept" if v["token"].as_str().is_some_and(|s| !s.is_empty()) => {
                        keep_token(&app, &phone_id, v["token"].as_str().unwrap());
                        let mut s = state();
                        s.state = "paired".into();
                        set_state(&app, s);
                        deliver(&waiting, "pair", v);
                    }
                    "pair_reject" => deliver(&waiting, "pair", v),
                    "command_result" if v["id"].is_string() && v["ok"].is_boolean() => {
                        let id = v["id"].as_str().unwrap().to_string();
                        deliver(&waiting, &id, v);
                    }
                    "command" if v["id"].is_string() && v["text"].is_string() => {
                        // (the phone asking the laptop: run as if said here, and answered)
                        let (id, text) = (v["id"].as_str().unwrap().to_string(), v["text"].as_str().unwrap().to_string());
                        let _ = app.emit_to("main", "link-command", json!({"id": id, "text": text}));
                    }
                    "error" => crate::log_line(&format!("phone link: the phone says {} ({})", v["code"], v["message"].as_str().unwrap_or(""))),
                    _ => {
                        let _ = ws.send(tungstenite::Message::Text(json!({"type": "error", "code": "bad_message", "message": "unknown or malformed message"}).to_string().into()));
                    }
                }
            }
            Ok(tungstenite::Message::Close(_)) => return gone(&app),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(_) => return gone(&app),
        }
    }
}

fn deliver(waiting: &Mutex<HashMap<String, Sender<Value>>>, id: &str, v: Value) {
    if let Some(tx) = waiting.lock().unwrap().remove(id) {
        let _ = tx.send(v);
    }
}

fn gone(app: &AppHandle) {
    *LIVE.lock().unwrap() = None;
    set_state(app, State { state: "off".into(), ..Default::default() });
    crate::log_line("phone link: closed");
}

pub fn close() {
    if let Some(l) = LIVE.lock().unwrap().take() {
        let _ = l.out.send(Out::Close);
    }
}

/// Send a message and wait for the answer filed under `key` (None: not connected, or no answer in time).
fn ask(key: &str, msg: Value, wait: Duration) -> Result<Value, String> {
    let (tx, rx) = channel::<Value>();
    {
        let g = LIVE.lock().unwrap();
        let l = g.as_ref().ok_or("Your phone isn't connected.")?;
        l.waiting.lock().unwrap().insert(key.to_string(), tx);
        l.out.send(Out::Send(msg)).map_err(|_| "The link to your phone has closed.".to_string())?;
    }
    rx.recv_timeout(wait).map_err(|_| "Your phone didn't answer in time.".to_string())
}

/// The code shown on the phone (spaces and dashes left out). Ok: paired; Err: why not.
pub fn pair(code: &str) -> Result<(), String> {
    let digits: String = code.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
    if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err("The code is the 6 digits on your phone.".into());
    }
    let v = ask("pair", json!({"type": "pair_request", "code": digits}), Duration::from_secs(10))?;
    if v["type"] == "pair_accept" {
        return Ok(());
    }
    Err(match v["reason"].as_str().unwrap_or("") {
        "wrong_code" => "That code isn't the one on your phone.",
        "expired" => "That code has run out. Show a new one on your phone.",
        "too_many_attempts" => "Too many tries. Show a new code on your phone.",
        "not_pairing" => "Your phone isn't showing a code right now.",
        _ => "Your phone didn't pair.",
    }
    .into())
}

/// Run something on the phone as if it had been said there; the phone's reply (ok, text).
pub fn command(text: &str) -> Result<(bool, String), String> {
    if state().state != "paired" {
        return Err("Pair your phone first: type the code it shows.".into());
    }
    let id = format!("c-{}", SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst));
    let v = ask(&id, json!({"type": "command", "id": id, "text": text}), Duration::from_secs(30))?;
    Ok((v["ok"].as_bool().unwrap_or(false), v["text"].as_str().unwrap_or("").to_string()))
}

/// The laptop answering a command the phone sent.
pub fn answer(id: &str, ok: bool, text: &str) {
    if let Some(l) = LIVE.lock().unwrap().as_ref() {
        let _ = l.out.send(Out::Send(json!({"type": "command_result", "id": id, "ok": ok, "text": text})));
    }
}

fn whoami() -> String {
    std::env::var("USERNAME").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "My".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions() {
        assert_eq!(version(&json!({"protocolVersion": 1})), Some(1));
        assert_eq!(version(&json!({"protocolVersion": 3, "minProtocolVersion": 1})), Some(1));
        assert_eq!(version(&json!({"protocolVersion": 3, "minProtocolVersion": 2})), None);
    }
}
