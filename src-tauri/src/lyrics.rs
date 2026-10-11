//! Real, time-synced lyrics from LRCLIB (free and open, no account or key).

use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Serialize, Default)]
pub struct Lyrics {
    pub synced: Vec<(f64, String)>, // (seconds, line)
    pub plain: Option<String>,
    pub instrumental: bool,
    /// the lyrics' timing matches what's playing (same length); otherwise ULTRON lines them up by listening
    pub exact: bool,
}

#[derive(Deserialize, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct Rec {
    track_name: Option<String>,
    artist_name: Option<String>,
    synced_lyrics: Option<String>,
    plain_lyrics: Option<String>,
    instrumental: Option<bool>,
    duration: Option<f64>,
}

const API: &str = "https://lrclib.net/api";

fn agent() -> ureq::Agent {
    crate::net::agent(Duration::from_secs(8))
}

fn parse_lrc(s: &str) -> Vec<(f64, String)> {
    let mut out = Vec::new();
    for line in s.lines() {
        let mut rest = line.trim();
        let mut stamps = Vec::new();
        while let Some(r) = rest.strip_prefix('[') {
            let Some(end) = r.find(']') else { break };
            let tag = &r[..end];
            rest = &r[end + 1..];
            let mut parts = tag.splitn(2, ':');
            if let (Some(m), Some(sec)) = (parts.next(), parts.next()) {
                if let (Ok(m), Ok(sec)) = (m.trim().parse::<f64>(), sec.trim().parse::<f64>()) {
                    stamps.push(m * 60.0 + sec);
                }
            }
        }
        for t in stamps {
            out.push((t, rest.trim().to_string()));
        }
    }
    out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    out
}

fn to_lyrics(r: Rec, duration: f64) -> Lyrics {
    let exact = duration <= 0.0 || r.duration.map(|d| (d - duration).abs() <= 3.0).unwrap_or(false);
    Lyrics {
        exact,
        synced: r.synced_lyrics.as_deref().map(parse_lrc).unwrap_or_default(),
        plain: r.plain_lyrics.filter(|p| !p.trim().is_empty()),
        instrumental: r.instrumental.unwrap_or(false),
    }
}

fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Is this record really the song that's playing? (its title words are in what's playing, and the artist matches)
fn same_song(r: &Rec, title: &str, artist: &str) -> bool {
    let have: Vec<String> = words(&format!("{title} {artist}"));
    let tw = words(r.track_name.as_deref().unwrap_or(""));
    if tw.is_empty() {
        return false;
    }
    let found = tw.iter().filter(|w| have.contains(w)).count() as f64 / tw.len() as f64;
    let aw = words(r.artist_name.as_deref().unwrap_or(""));
    let artist_ok = artist.trim().is_empty() || aw.iter().any(|w| w.len() > 1 && have.contains(w));
    found >= 0.75 && artist_ok
}

fn pick(recs: Vec<Rec>, duration: f64) -> Option<Rec> {
    let score = |r: &Rec| {
        let d = r.duration.map(|d| (d - duration).abs()).unwrap_or(30.0);
        let d = if duration > 0.0 { d } else { 0.0 };
        d - if r.synced_lyrics.is_some() { 20.0 } else { 0.0 }
    };
    let mut recs: Vec<Rec> = recs.into_iter().filter(|r| r.synced_lyrics.is_some() || r.plain_lyrics.is_some()).collect();
    recs.sort_by(|a, b| score(a).partial_cmp(&score(b)).unwrap());
    // music videos run longer than the track (intros, outros): keep the closest match and line it up by listening
    recs.into_iter().next()
}

/* ---- remembered lyrics: songs you replay load instantly, and known misses aren't searched again for a day ---- */

#[derive(Serialize, Deserialize, Clone)]
struct Cached {
    rec: Option<Rec>,
    at: u64,
}

static CACHE: std::sync::Mutex<Option<std::collections::HashMap<String, Cached>>> = std::sync::Mutex::new(None);

fn cache_path() -> Option<std::path::PathBuf> {
    std::env::var("APPDATA").ok().map(|d| std::path::PathBuf::from(d).join("com.ultron.notch").join("lyrics-cache.json"))
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn cache_get(key: &str) -> Option<Option<Rec>> {
    let mut g = CACHE.lock().unwrap();
    if g.is_none() {
        *g = Some(cache_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default());
    }
    let c = g.as_ref()?.get(key)?;
    // a miss is only trusted for a day (lyrics get added all the time)
    if c.rec.is_none() && now() - c.at > 86_400 {
        return None;
    }
    Some(c.rec.clone())
}

fn cache_put(key: &str, rec: Option<Rec>) {
    let mut g = CACHE.lock().unwrap();
    let m = g.get_or_insert_with(Default::default);
    m.insert(key.to_string(), Cached { rec, at: now() });
    if m.len() > 600 {
        let oldest = m.iter().min_by_key(|(_, v)| v.at).map(|(k, _)| k.clone());
        if let Some(k) = oldest {
            m.remove(&k);
        }
    }
    if let (Some(p), Ok(b)) = (cache_path(), serde_json::to_vec(m)) {
        let _ = std::fs::write(p, b);
    }
}

pub fn fetch(title: &str, artist: &str, album: &str, duration: f64) -> Option<Lyrics> {
    let key = format!("{}|{}", title.to_lowercase(), artist.to_lowercase());
    if let Some(hit) = cache_get(&key) {
        return hit.map(|r| to_lyrics(r, duration));
    }
    let a = agent();
    let mut get = a.get(&format!("{API}/get")).query("track_name", title).query("artist_name", artist);
    if !album.is_empty() {
        get = get.query("album_name", album);
    }
    if duration > 0.0 {
        get = get.query("duration", &format!("{}", duration.round()));
    }
    let mut tries: Vec<ureq::Request> = Vec::new();
    if !artist.is_empty() {
        tries.push(a.get(&format!("{API}/search")).query("track_name", title).query("artist_name", artist));
        // video titles come as "Artist - Song" or "Song - Artist": try the other way round too
        tries.push(a.get(&format!("{API}/search")).query("track_name", artist).query("artist_name", title));
        tries.push(a.get(&format!("{API}/search")).query("q", &format!("{artist} {title}")));
    }
    tries.push(a.get(&format!("{API}/search")).query("q", title));
    // everything at once; the most exact answer wins; never wait more than ~9 s
    let (tx, rx) = std::sync::mpsc::channel::<(usize, Option<Rec>)>();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let r = get.call().ok().and_then(|r| r.into_json::<Rec>().ok()).filter(|r| r.synced_lyrics.is_some() || r.plain_lyrics.is_some() || r.instrumental == Some(true));
            let _ = tx.send((0, r));
        });
    }
    let n = tries.len() + 1;
    for (i, req) in tries.into_iter().enumerate() {
        let (tx, t, ar) = (tx.clone(), title.to_string(), artist.to_string());
        std::thread::spawn(move || {
            let r = req.call().ok().and_then(|r| r.into_json::<Vec<Rec>>().ok()).and_then(|v| pick(v.into_iter().filter(|r| same_song(r, &t, &ar)).collect(), duration));
            let _ = tx.send((i + 1, r));
        });
    }
    drop(tx);
    let deadline = std::time::Instant::now() + Duration::from_secs(9);
    let mut got: Vec<Option<Rec>> = vec![None; n];
    let mut answered = vec![false; n];
    while let Ok((i, r)) = rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
        got[i] = r;
        answered[i] = true;
        // stop early once the best possible answer so far is settled
        if let Some(first) = (0..n).find(|j| got[*j].is_some()) {
            if (0..first).all(|j| answered[j]) {
                break;
            }
        }
        if answered.iter().all(|x| *x) {
            break;
        }
    }
    let best = got.into_iter().flatten().next();
    // remember hits always; remember a miss only when every lookup actually answered
    if best.is_some() || answered.iter().all(|x| *x) {
        cache_put(&key, best.clone());
    }
    best.map(|r| to_lyrics(r, duration))
}

#[cfg(test)]
mod tests {
    #[test]
    fn finds_songs() {
        for (t, a, d) in [("Old Town Road", "Lil Nas X", 251.0), ("ME JALO", "Fuerza Regida", 212.0), ("BBD Extension Ad Sep 2026", "BuyHatke", 57.0), ("Tum Hi Ho", "Arijit Singh", 300.0)] {
            let t0 = std::time::Instant::now();
            let r = super::fetch(t, a, "", d);
            println!("{t} / {a}: {} in {} ms", r.map(|l| format!("{} synced lines, exact={}", l.synced.len(), l.exact)).unwrap_or("none".into()), t0.elapsed().as_millis());
        }
    }
}
