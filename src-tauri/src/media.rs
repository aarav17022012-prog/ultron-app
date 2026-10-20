//! Now playing, read from Windows' media sessions (the same source as the volume
//! flyout), so it works with Spotify, browsers, and any app that reports media.

use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session, GlobalSystemMediaTransportControlsSessionManager as Manager_,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows::Storage::Streams::DataReader;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

#[derive(Serialize, Clone, PartialEq, Debug)]
pub struct Sess {
    pub id: String,
    pub app: String,
    pub kind: &'static str, // "browser" | "app"
    pub title: String,
    pub artist: String,
    pub album: String,
    pub playing: bool,
    pub position: f64, // ms, at `updated`
    pub duration: f64, // ms
    pub updated: f64,  // unix ms
}

#[derive(Serialize, Clone, PartialEq, Debug, Default)]
pub struct MediaState {
    pub sessions: Vec<Sess>,
    pub current: Option<String>,
}

pub enum Cmd {
    Select(String),
    Toggle,
    Play,
    Pause,
    Next,
    Prev,
    Seek(f64),
}

pub struct Media {
    fast: std::sync::atomic::AtomicBool,
    tx: Mutex<Sender<Cmd>>,
    rx: Mutex<Option<Receiver<Cmd>>>,
    pub state: Mutex<MediaState>,
}

impl Default for Media {
    fn default() -> Self {
        let (tx, rx) = channel();
        Media { fast: Default::default(), tx: Mutex::new(tx), rx: Mutex::new(Some(rx)), state: Mutex::new(MediaState::default()) }
    }
}

impl Media {
    pub fn set_fast(&self, on: bool) {
        self.fast.store(on, std::sync::atomic::Ordering::SeqCst);
    }
    pub fn send(&self, c: Cmd) {
        let _ = self.tx.lock().unwrap().send(c);
    }
}

fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0)
}

pub fn app_name(aumid: &str) -> (String, &'static str) {
    let a = aumid.to_lowercase();
    let table: [(&str, &str, &str); 15] = [
        // (browsers the list didn't know were treated as apps: no song-and-artist split of video titles, no captions)
        ("thebrowsercompany.dia", "Dia", "browser"),
        ("thebrowsercompany.arc", "Arc", "browser"),
        ("zen", "Zen", "browser"),
        ("spotify", "Spotify", "app"),
        ("chrome", "Chrome", "browser"),
        ("msedge", "Edge", "browser"),
        ("firefox", "Firefox", "browser"),
        ("308046b0af4a39cb", "Firefox", "browser"),
        ("brave", "Brave", "browser"),
        ("opera", "Opera", "browser"),
        ("vivaldi", "Vivaldi", "browser"),
        ("zunemusic", "Media Player", "app"),
        ("vlc", "VLC", "app"),
        ("applemusic", "Apple Music", "app"),
        ("amazon", "Amazon Music", "app"),
    ];
    for (k, n, kind) in table {
        if a.contains(k) {
            return (n.into(), kind);
        }
    }
    let base = aumid.rsplit(['\\', '!']).next().unwrap_or(aumid).trim_end_matches(".exe");
    let base = base.split('_').next().unwrap_or(base);
    let base = base.rsplit('.').next().unwrap_or(base);
    (if base.is_empty() { "Media".into() } else { base.to_string() }, "app")
}

/// when each session's position was first seen (for apps that don't timestamp it)
static SEEN: Mutex<Option<std::collections::HashMap<String, (f64, f64)>>> = Mutex::new(None);

fn read_session(s: &Session, id: String) -> Option<Sess> {
    let aumid = s.SourceAppUserModelId().ok()?.to_string();
    let props = s.TryGetMediaPropertiesAsync().ok()?.get().ok()?;
    let title = props.Title().map(|h| h.to_string()).unwrap_or_default();
    if title.is_empty() {
        return None;
    }
    let tl = s.GetTimelineProperties().ok()?;
    let start = tl.StartTime().map(|t| t.Duration).unwrap_or(0) as f64 / 10_000.0;
    let end = tl.EndTime().map(|t| t.Duration).unwrap_or(0) as f64 / 10_000.0;
    let pos = tl.Position().map(|t| t.Duration).unwrap_or(0) as f64 / 10_000.0;
    let upd = tl.LastUpdatedTime().map(|t| t.UniversalTime).unwrap_or(0);
    // some apps never timestamp their position: remember when we first saw this value,
    // otherwise every poll looked "fresh" and the lyrics snapped back 1.5 s each time
    let updated = if upd > 0 {
        (upd - 116_444_736_000_000_000) as f64 / 10_000.0
    } else {
        let mut seen = SEEN.lock().unwrap();
        let e = seen.get_or_insert_with(Default::default).entry(id.clone()).or_insert((f64::NAN, now_ms()));
        if (e.0 - pos).abs() > 1.0 {
            *e = (pos, now_ms());
        }
        e.1
    };
    let status = s.GetPlaybackInfo().and_then(|p| p.PlaybackStatus()).ok();
    // (a player that has stopped or closed has nothing to show: browsers leave a closed tab's video listed like that)
    if matches!(status, Some(Status::Stopped) | Some(Status::Closed)) {
        return None;
    }
    let playing = status == Some(Status::Playing);
    let (app, kind) = app_name(&aumid);
    Some(Sess {
        id,
        app,
        kind,
        title,
        artist: props.Artist().map(|h| h.to_string()).unwrap_or_default(),
        album: props.AlbumTitle().map(|h| h.to_string()).unwrap_or_default(),
        playing,
        position: (pos - start).max(0.0),
        duration: (end - start).max(0.0),
        updated,
    })
}

fn thumbnail(s: &Session) -> Option<String> {
    let props = s.TryGetMediaPropertiesAsync().ok()?.get().ok()?;
    let stream = props.Thumbnail().ok()?.OpenReadAsync().ok()?.get().ok()?;
    let size = stream.Size().ok()? as u32;
    if size == 0 || size > 8_000_000 {
        return None;
    }
    let mime = stream.ContentType().map(|h| h.to_string()).unwrap_or_default();
    let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0).ok()?).ok()?;
    reader.LoadAsync(size).ok()?.get().ok()?;
    let mut buf = vec![0u8; size as usize];
    reader.ReadBytes(&mut buf).ok()?;
    let mime = if mime.starts_with("image/") { mime } else { "image/png".into() };
    Some(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(buf)))
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let media = app.state::<Media>();
        let rx = media.rx.lock().unwrap().take().unwrap();
        let mgr = loop {
            match Manager_::RequestAsync().and_then(|op| op.get()) {
                Ok(m) => break m,
                Err(_) => std::thread::sleep(Duration::from_secs(3)),
            }
        };
        let mut selected: Option<String> = None;
        let mut art_key = String::new();
        loop {
            // gather sessions
            let list: Vec<(String, Session)> = match mgr.GetSessions() {
                Ok(v) => {
                    let mut seen: std::collections::HashMap<String, usize> = Default::default();
                    (0..v.Size().unwrap_or(0))
                        .filter_map(|i| v.GetAt(i).ok())
                        .filter_map(|s| {
                            let a = s.SourceAppUserModelId().ok()?.to_string();
                            let n = seen.entry(a.clone()).or_default();
                            *n += 1;
                            Some((format!("{a}#{n}"), s))
                        })
                        .collect()
                }
                Err(_) => Vec::new(),
            };
            let mut sessions: Vec<Sess> = list.iter().filter_map(|(id, s)| read_session(s, id.clone())).collect();
            // a browser that has been closed can stay running unseen and keep its last video listed, paused: not shown
            if sessions.iter().any(|s| s.kind == "browser" && !s.playing) {
                let wins = app.state::<crate::apps::Apps>().open_windows();
                sessions.retain(|s| s.kind != "browser" || s.playing || wins.iter().any(|w| w.name.to_lowercase().contains(&s.app.to_lowercase())));
            }
            let system = mgr.GetCurrentSession().ok().and_then(|s| s.SourceAppUserModelId().ok()).map(|h| format!("{h}#1"));
            let sys_id = system.clone();
            let current = selected
                .clone()
                .filter(|id| sessions.iter().any(|s| &s.id == id))
                .or_else(|| sessions.iter().find(|s| s.playing).map(|s| s.id.clone()))
                .or_else(|| system.filter(|id| sessions.iter().any(|s| &s.id == id)))
                .or_else(|| sessions.first().map(|s| s.id.clone()));
            let state = MediaState { sessions, current: current.clone() };
            if *media.state.lock().unwrap() != state {
                let _ = app.emit_to("main", "media", &state);
                // album art only when the track changes
                if let Some(cur) = state.sessions.iter().find(|s| Some(&s.id) == current.as_ref()) {
                    let key = format!("{}|{}|{}", cur.id, cur.title, cur.artist);
                    if key != art_key {
                        art_key = key.clone();
                        let url = list.iter().find(|(id, _)| id == &cur.id).and_then(|(_, s)| thumbnail(s)).unwrap_or_default();
                        let _ = app.emit_to("main", "media-art", (key, url));
                    }
                }
                *media.state.lock().unwrap() = state;
            }

            // run commands, or wait for the next poll
            let session = |id: &Option<String>| list.iter().find(|(i, _)| Some(i) == id.as_ref()).map(|(_, s)| s.clone());
            let wait = if media.fast.load(std::sync::atomic::Ordering::SeqCst) { 350 } else { 1500 };
            match rx.recv_timeout(Duration::from_millis(wait)) {
                Ok(cmd) => {
                    let s = session(&current);
                    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_MEDIA_NEXT_TRACK, VK_MEDIA_PLAY_PAUSE, VK_MEDIA_PREV_TRACK};
                    let ok = |r: windows::core::Result<bool>| r.unwrap_or(false);
                    match cmd {
                        Cmd::Select(id) => selected = Some(id),
                        Cmd::Toggle | Cmd::Play | Cmd::Pause => {
                            let done = s
                                .as_ref()
                                .map(|s| {
                                    let playing = s.GetPlaybackInfo().and_then(|p| p.PlaybackStatus()).map(|st| st == Status::Playing).unwrap_or(false);
                                    let want_play = match cmd {
                                        Cmd::Play => true,
                                        Cmd::Pause => false,
                                        _ => !playing,
                                    };
                                    // explicit play / pause first: some players (Spotify) don't do "toggle"
                                    let first = if want_play {
                                        ok(s.TryPlayAsync().and_then(|o| o.get()))
                                    } else {
                                        ok(s.TryPauseAsync().and_then(|o| o.get()))
                                    };
                                    first || ok(s.TryTogglePlayPauseAsync().and_then(|o| o.get()))
                                })
                                .unwrap_or(false);
                            // the keyboard media key goes to Windows' own "current" player, so only
                            // fall back to it when that's the player ULTRON is showing
                            if !done && current.is_some() && current == sys_id {
                                crate::apps::send_keys(&[VK_MEDIA_PLAY_PAUSE]);
                            }
                        }
                        Cmd::Next => {
                            if let Some(s) = &s {
                                let can = s.GetPlaybackInfo().and_then(|p| p.Controls()).and_then(|c| c.IsNextEnabled()).unwrap_or(true);
                                let skipped = can && ok(s.TrySkipNextAsync().and_then(|o| o.get()));
                                if !skipped {
                                    if can && (current == sys_id || list.len() == 1) {
                                        crate::apps::send_keys(&[VK_MEDIA_NEXT_TRACK]);
                                    } else {
                                        // players that can't skip (a single YouTube video): jump to the end, and autoplay moves on
                                        let end = s.GetTimelineProperties().and_then(|t| t.EndTime()).map(|t| t.Duration).unwrap_or(0);
                                        if end > 50_000_000 {
                                            let _ = s.TryChangePlaybackPositionAsync(end - 8_000_000).and_then(|o| o.get());
                                        } else if current == sys_id {
                                            crate::apps::send_keys(&[VK_MEDIA_NEXT_TRACK]);
                                        }
                                    }
                                }
                            }
                        }
                        Cmd::Prev => {
                            if let Some(s) = &s {
                                // like every music player: a few seconds in, "previous" starts the song again
                                let pos = s.GetTimelineProperties().and_then(|t| t.Position()).map(|t| t.Duration).unwrap_or(0);
                                let can = s.GetPlaybackInfo().and_then(|p| p.Controls()).and_then(|c| c.IsPreviousEnabled()).unwrap_or(true);
                                if pos > 30_000_000 || !can {
                                    let _ = s.TryChangePlaybackPositionAsync(0).and_then(|o| o.get());
                                } else if !ok(s.TrySkipPreviousAsync().and_then(|o| o.get())) && (current == sys_id || list.len() == 1) {
                                    crate::apps::send_keys(&[VK_MEDIA_PREV_TRACK]);
                                }
                            }
                        }
                        Cmd::Seek(ms) => {
                            let _ = s.map(|s| s.TryChangePlaybackPositionAsync((ms * 10_000.0) as i64).and_then(|o| o.get()));
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
}

/// The icon of the app a song is playing from (Spotify's own, the browser's own), as a picture for the page.
/// Windows names the app either by an id it knows (Store apps, and programs that register one) or just by
/// its file name; in that case the icon comes from the program that is running under that name.
pub fn icon(id: &str) -> Option<String> {
    let app = id.rsplit_once('#').filter(|(_, n)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())).map(|(a, _)| a).unwrap_or(id);
    let picture = |source: String| crate::sys::icons(vec![source]).into_iter().next().filter(|u| !u.is_empty());
    if let Some(u) = picture(format!("shell:AppsFolder\\{app}")) {
        return Some(u);
    }
    let exe = app.rsplit(['\\', '!']).next().unwrap_or(app).to_lowercase();
    let exe = if exe.ends_with(".exe") { exe } else { format!("{exe}.exe") };
    picture(crate::kids::path_of_running(&exe)?)
}
