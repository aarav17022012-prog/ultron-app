//! Reliable web requests on networks where some of a site's addresses don't work
//! (e.g. no IPv6, or one of a CDN's IPs unreachable). The first time ULTRON talks
//! to a site it tries all of its addresses at once, then uses only the ones that
//! answer (remembered for 10 minutes). Without this, a request could sit for the
//! whole timeout on a dead address while a working one was right there.

use std::collections::HashMap;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Mutex;
use std::time::{Duration, Instant};

static GOOD: Mutex<Option<HashMap<String, (Vec<SocketAddr>, Instant)>>> = Mutex::new(None);

fn resolve(addr: &str) -> std::io::Result<Vec<SocketAddr>> {
    if let Some((v, at)) = GOOD.lock().unwrap().as_ref().and_then(|m| m.get(addr).cloned()) {
        if at.elapsed() < Duration::from_secs(600) && !v.is_empty() {
            return Ok(v);
        }
    }
    let all: Vec<SocketAddr> = addr.to_socket_addrs()?.collect();
    if all.len() <= 1 {
        return Ok(all);
    }
    // knock on every address at once; keep the ones that open, fastest first
    let (tx, rx) = std::sync::mpsc::channel();
    for a in all.iter().copied() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let t0 = Instant::now();
            if TcpStream::connect_timeout(&a, Duration::from_millis(1500)).is_ok() {
                let _ = tx.send((t0.elapsed(), a));
            }
        });
    }
    drop(tx);
    let mut deadline = Instant::now() + Duration::from_millis(1600);
    let mut ok: Vec<(Duration, SocketAddr)> = Vec::new();
    while let Ok(x) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        if ok.is_empty() {
            // one works: give the others a moment to show up, then go
            deadline = deadline.min(Instant::now() + Duration::from_millis(120));
        }
        ok.push(x);
        if ok.len() >= 2 {
            break;
        }
    }
    if ok.is_empty() {
        // nothing answered in time: hand over everything, IPv4 first
        let mut v = all;
        v.sort_by_key(|a| a.is_ipv6());
        return Ok(v);
    }
    ok.sort_by_key(|(d, _)| *d);
    let v: Vec<SocketAddr> = ok.into_iter().map(|(_, a)| a).collect();
    GOOD.lock().unwrap().get_or_insert_with(HashMap::new).insert(addr.to_string(), (v.clone(), Instant::now()));
    Ok(v)
}

/// A request agent that only uses addresses that actually work.
pub fn agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(timeout)
        .timeout_connect(Duration::from_millis(2500))
        .resolver(|addr: &str| resolve(addr))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) ULTRON/0.3")
        .build()
}

/// GET a URL as text, asked the way a command-line tool asks. (wttr.in answers anything that calls itself a
/// browser with a whole web page, where a one-line answer was wanted.)
pub fn get_plain(url: &str, timeout: Duration) -> Option<String> {
    agent(timeout).get(url).set("User-Agent", "curl/8.4.0").call().ok()?.into_string().ok()
}

/// GET a URL as text.
pub fn get_text(url: &str, timeout: Duration) -> Option<String> {
    agent(timeout).get(url).call().ok()?.into_string().ok()
}
