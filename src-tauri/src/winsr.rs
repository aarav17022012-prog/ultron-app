//! Windows' own speech recognizer, listening under SYLVER's dictation: each thing you dictate is heard by Whisper and,
//! at the same time, by the recognizer built into Windows (Microsoft Speech Recognizer 8.0, on the PC, no internet).
//! Whisper's words are what's typed; Windows' are a second pair of ears:
//!
//! - Windows heard no speech at all, and Whisper a word or two: a noise Whisper made words of ("I", "And...",
//!   "Digital."), dropped.
//! - Whisper heard nothing (or only "okay", "thank you" and the like, which it often makes up from a breath), Windows
//!   clearly heard words: Windows' words are used.
//! - Windows heard one of your names (the ones SYLVER learned or you listed, given to it as words it should expect)
//!   where Whisper wrote something that sounds like it: the name is put in.
//!
//! The recognizer runs in a small PowerShell of its own (System.Speech), started with dictation and closed three quiet
//! minutes after; about half a second for each thing said, alongside Whisper, so nothing waits longer. English only.

use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

const SCRIPT: &str = r#"$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Speech
$R = 'System.Speech.Recognition'
$e = New-Object "$R.SpeechRecognitionEngine" ([Globalization.CultureInfo]'en-US')
$e.LoadGrammar((New-Object "$R.DictationGrammar"))
$e.InitialSilenceTimeout = [TimeSpan]::FromSeconds(3)
$e.EndSilenceTimeout = [TimeSpan]::FromMilliseconds(300)
$e.BabbleTimeout = [TimeSpan]::FromSeconds(0)
$named = $null; $had = ''
[Console]::Out.WriteLine('{"ready":true}'); [Console]::Out.Flush()
while ($true) {
  $line = [Console]::In.ReadLine(); if ($null -eq $line) { break }
  $out = @{ t = ''; c = 0; w = @() }
  try {
    $m = $line.Substring([math]::Max(0, $line.IndexOf([char]123))) | ConvertFrom-Json
    $nm = (@($m.names) | Where-Object { $_ }) -join '|'
    if ($nm -ne $had) {
      if ($named) { $e.UnloadGrammar($named); $named = $null }
      if ($nm) {
        $gb = New-Object "$R.GrammarBuilder"; $gb.Culture = [Globalization.CultureInfo]'en-US'
        $gb.AppendDictation(); $gb.Append((New-Object "$R.Choices" (,[string[]]@($m.names | Where-Object { $_ })))); $gb.AppendDictation()
        $named = New-Object "$R.Grammar" $gb; $named.Weight = 0.6; $e.LoadGrammar($named)
      }
      $had = $nm
    }
    $e.SetInputToWaveFile($m.wav)
    $words = New-Object System.Collections.ArrayList; $parts = @(); $cs = @()
    for ($i = 0; $i -lt 8; $i++) {
      try { $r = $e.Recognize() } catch { $r = $null }
      if ($null -eq $r) { break }
      $parts += $r.Text; $cs += $r.Confidence
      foreach ($w in $r.Words) { [void]$words.Add(@($w.Text, [math]::Round($w.Confidence, 2))) }
    }
    try { $e.SetInputToNull() } catch {}
    $out.t = ($parts -join ' ').Trim()
    if ($cs.Count) { $out.c = [math]::Round((($cs | Measure-Object -Average).Average), 2) }
    $out.w = $words.ToArray()
  } catch { $out.err = "$($_.Exception.Message)"; try { $e.SetInputToNull() } catch {} }
  [Console]::Out.WriteLine(($out | ConvertTo-Json -Compress -Depth 4)); [Console]::Out.Flush()
}
"#;

struct Helper {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

static HELPER: Mutex<Option<Helper>> = Mutex::new(None);
static USED: Mutex<Option<Instant>> = Mutex::new(None);

/// What Windows heard: its words, how sure it was of the whole (0-1), and of each word
pub struct Heard {
    pub text: String,
    pub conf: f32,
    pub words: Vec<(String, f32)>,
}

fn start() -> Option<Helper> {
    let dir = std::env::temp_dir();
    let script = dir.join("ultron-winsr.ps1");
    std::fs::write(&script, SCRIPT).ok()?;
    let mut child = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(0x0800_0000)
        .spawn()
        .ok()?;
    let input = child.stdin.take()?;
    let mut output = BufReader::new(child.stdout.take()?);
    let mut first = String::new();
    if output.read_line(&mut first).is_err() || !first.contains("ready") {
        let _ = child.kill();
        crate::log_line("windows speech: its recognizer couldn't start");
        return None;
    }
    // (closed after three quiet minutes: it holds about 80 MB while it's up)
    static TIDY: std::sync::Once = std::sync::Once::new();
    TIDY.call_once(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(Duration::from_secs(30));
            if USED.lock().unwrap().is_some_and(|t| t.elapsed() > Duration::from_secs(180)) {
                if let Ok(mut h) = HELPER.try_lock() {
                    if let Some(mut x) = h.take() {
                        let _ = x.child.kill();
                        let _ = x.child.wait();
                        crate::log_line("windows speech: put away (nothing dictated for 3 minutes)");
                    }
                }
            }
        });
    });
    Some(Helper { child, input, output })
}

/// Started ahead, as dictation begins, so the first thing said is heard by both
pub fn warm() {
    *USED.lock().unwrap() = Some(Instant::now());
    std::thread::spawn(|| {
        if let Ok(mut h) = HELPER.try_lock() {
            if h.is_none() {
                let t0 = Instant::now();
                *h = start();
                if h.is_some() {
                    crate::log_line(&format!("windows speech: ready under dictation in {} ms", t0.elapsed().as_millis()));
                }
            }
        }
    });
}

fn wav(samples: &[f32], rate: u32) -> Vec<u8> {
    let data: Vec<u8> = samples.iter().flat_map(|x| ((x.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes()).collect();
    let mut v = Vec::with_capacity(44 + data.len());
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * 2).to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(&data);
    v
}

/// What Windows hears in a clip (16 kHz). None: it isn't up (it's started for next time), or it's busy with the last one.
pub fn hear(samples: &[f32], names: &[String]) -> Option<Heard> {
    *USED.lock().unwrap() = Some(Instant::now());
    let mut h = HELPER.try_lock().ok()?;
    if h.is_none() {
        *h = start();
    }
    let x = h.as_mut()?;
    let path = std::env::temp_dir().join("ultron-winsr.wav");
    std::fs::write(&path, wav(samples, 16000)).ok()?;
    let ask = serde_json::json!({"wav": path.to_string_lossy(), "names": names});
    let mut line = String::new();
    if writeln!(x.input, "{ask}").and_then(|_| x.input.flush()).is_err() || x.output.read_line(&mut line).unwrap_or(0) == 0 {
        let _ = x.child.kill();
        *h = None;
        return None;
    }
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    if let Some(e) = v["err"].as_str() {
        crate::log_line(&format!("windows speech: {e}"));
    }
    let words = v["w"].as_array().map(|a| a.iter().filter_map(|w| Some((w[0].as_str()?.to_string(), w[1].as_f64().unwrap_or(0.0) as f32))).collect()).unwrap_or_default();
    Some(Heard { text: v["t"].as_str().unwrap_or("").to_string(), conf: v["c"].as_f64().unwrap_or(0.0) as f32, words })
}

fn key(w: &str) -> String {
    w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase()
}

/// How alike two words sound, roughly (0-1): their letters, with the ones that sound the same made the same
fn alike(a: &str, b: &str) -> f32 {
    let sound = |w: &str| -> Vec<char> {
        let mut out: Vec<char> = Vec::new();
        for c in w.chars().filter(|c| c.is_alphanumeric()) {
            let c = match c {
                'a' | 'e' | 'i' | 'o' | 'u' | 'y' | 'h' | 'w' => 'a',
                'c' | 'k' | 'q' | 'g' | 'x' => 'k',
                's' | 'z' => 's',
                'b' | 'p' => 'p',
                'd' | 't' => 't',
                'f' | 'v' => 'f',
                'm' | 'n' => 'n',
                c => c,
            };
            if out.last() != Some(&c) {
                out.push(c);
            }
        }
        out
    };
    let (a, b) = (sound(a), sound(b));
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let mut d: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut prev = d[0];
        d[0] = i;
        for j in 1..=b.len() {
            let cur = d[j];
            d[j] = (d[j] + 1).min(d[j - 1] + 1).min(prev + usize::from(a[i - 1] != b[j - 1]));
            prev = cur;
        }
    }
    1.0 - d[b.len()] as f32 / a.len().max(b.len()) as f32
}

/// Whisper's words, checked against Windows': what's typed (None: nothing was said).
pub fn fuse(whisper: &str, noise: bool, win: Option<&Heard>, names: &[String]) -> Option<String> {
    let Some(win) = win else { return (!noise).then(|| whisper.to_string()) };
    let w_words: Vec<&str> = whisper.split_whitespace().collect();
    let heard_nothing = win.text.trim().is_empty();
    // (a breath or a knock that Whisper made a word or two of)
    if heard_nothing && w_words.len() <= 2 {
        if !w_words.is_empty() {
            crate::log_line(&format!("windows speech: heard no speech where Whisper wrote {whisper:?}: dropped"));
        }
        return None;
    }
    // (Whisper's "okay" and "thank you" are kept when Windows heard them too; when Whisper heard nothing and Windows
    //  clearly heard words, theirs are used)
    if noise {
        let same = !w_words.is_empty() && w_words.iter().all(|w| win.words.iter().any(|(x, _)| key(x) == key(w)));
        if same && win.conf >= 0.5 {
            return Some(whisper.to_string());
        }
        if win.words.len() >= 3 && win.conf >= 0.6 {
            let mut t = win.text.trim().to_string();
            if let Some(f) = t.get(..1) {
                t = f.to_uppercase() + &t[1..];
            }
            crate::log_line(&format!("windows speech: Whisper heard nothing, Windows heard {t:?}"));
            return Some(t + ".");
        }
        return None;
    }
    // (a name of yours that Windows heard, where Whisper wrote something that sounds like it)
    let mut out: Vec<String> = w_words.iter().map(|s| s.to_string()).collect();
    let low = whisper.to_lowercase();
    for name in names {
        let parts: Vec<String> = name.split_whitespace().map(key).collect();
        let n = parts.len();
        if n == 0 || n > 3 || low.contains(&name.to_lowercase()) {
            continue;
        }
        let found = win.words.windows(n).position(|ws| ws.iter().zip(&parts).all(|((x, c), p)| key(x) == *p && *c >= 0.5));
        let Some(at) = found else { continue };
        // where in Whisper's words it would be: about the same share of the way through, and sounding like it
        let share = at as f32 / win.words.len().max(1) as f32;
        let mut best: Option<(f32, usize, usize)> = None;
        for len in 1..=(n + 1).min(out.len()) {
            for i in 0..=out.len() - len {
                let near = ((i as f32 / out.len() as f32) - share).abs();
                if near > 0.3 {
                    continue;
                }
                let said: String = out[i..i + len].iter().map(|w| key(w)).collect::<Vec<_>>().join("");
                let score = alike(&said, &parts.join(""));
                if score >= 0.6 && best.map_or(true, |b| score > b.0) {
                    best = Some((score, i, len));
                }
            }
        }
        if let Some((_, i, len)) = best {
            let tail: String = out[i + len - 1].chars().rev().take_while(|c| !c.is_alphanumeric()).collect::<Vec<_>>().into_iter().rev().collect();
            let was = out[i..i + len].join(" ");
            out.splice(i..i + len, [format!("{name}{tail}")]);
            crate::log_line(&format!("windows speech: heard the name {name:?} where Whisper wrote {was:?}"));
        }
    }
    Some(out.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heard(t: &str, c: f32) -> Heard {
        Heard { text: t.into(), conf: c, words: t.split_whitespace().map(|w| (w.to_string(), c)).collect() }
    }

    #[test]
    fn a_name_whisper_missed_is_put_in() {
        let names = vec!["Danish".to_string(), "Sylver".to_string()];
        let w = heard("please send a reporter Danish tomorrow morning", 0.8);
        assert_eq!(fuse("Please send the report to Danny's tomorrow morning.", false, Some(&w), &names).unwrap(), "Please send the report to Danish tomorrow morning.");
        assert_eq!(fuse("Please send the report to Danish tomorrow.", false, Some(&w), &names).unwrap(), "Please send the report to Danish tomorrow.");
    }

    #[test]
    fn noise_and_silence() {
        let none = heard("", 0.0);
        assert_eq!(fuse("Digital.", false, Some(&none), &[]), None);
        assert_eq!(fuse("Open the report tomorrow.", false, Some(&none), &[]).as_deref(), Some("Open the report tomorrow."));
        assert_eq!(fuse("Okay.", true, Some(&heard("okay", 0.7)), &[]).as_deref(), Some("Okay."));
        assert_eq!(fuse("Thank you.", true, Some(&heard("open my calendar please", 0.8)), &[]).as_deref(), Some("Open my calendar please."));
        assert_eq!(fuse("Okay.", true, None, &[]), None);
    }
}
