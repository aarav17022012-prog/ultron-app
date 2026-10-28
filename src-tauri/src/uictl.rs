//! Working any app's controls directly, through the accessibility interface every Windows app gives screen readers
//! (Win32, the Store's apps, Office, Chrome and Edge, Electron apps like WhatsApp, Discord, VS Code...). ULTRON can read
//! what's in a window (each button, switch, box, list and slider, with how it's set now) and act on one by its name:
//! press it, turn it on or off, type into it, set it to a number, choose one of its options, open or close it. All of
//! that goes to the app itself, without moving the pointer, so it works on a window that's half covered and takes a
//! few milliseconds; only when an app offers none of that is it clicked where it is.

use std::time::{Duration, Instant};

use windows::core::BSTR;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::UI::Accessibility::*;

/// What kind of thing it is, in plain words ("" for kinds that aren't something you act on).
fn kind(ct: UIA_CONTROLTYPE_ID, toggles: bool) -> &'static str {
    match ct {
        UIA_ButtonControlTypeId | UIA_SplitButtonControlTypeId => {
            if toggles {
                "switch"
            } else {
                "button"
            }
        }
        UIA_CheckBoxControlTypeId => "checkbox",
        UIA_RadioButtonControlTypeId => "option",
        UIA_ComboBoxControlTypeId => "dropdown",
        UIA_EditControlTypeId => "box",
        UIA_DocumentControlTypeId => "document",
        UIA_HyperlinkControlTypeId => "link",
        UIA_ListItemControlTypeId | UIA_DataItemControlTypeId | UIA_TreeItemControlTypeId => "item",
        UIA_MenuItemControlTypeId => "menu",
        UIA_SliderControlTypeId | UIA_SpinnerControlTypeId => "slider",
        UIA_TabItemControlTypeId => "tab",
        _ => "",
    }
}

struct Ctl {
    el: IUIAutomationElement,
    name: String,
    kind: &'static str,
    enabled: bool,
    offscreen: bool,
    state: String,
}

fn uia() -> Result<IUIAutomation, String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).map_err(|e| format!("The accessibility interface isn't available ({e})."))
    }
}

/// Every control in the window, with its name, kind and state, read in one go (one request to the app, not one per
/// control: an app with a thousand of them answers in a few milliseconds rather than seconds).
fn controls(u: &IUIAutomation, root: &IUIAutomationElement) -> Vec<Ctl> {
    unsafe {
        let Ok(req) = u.CreateCacheRequest() else { return Vec::new() };
        for p in [
            UIA_NamePropertyId,
            UIA_ControlTypePropertyId,
            UIA_IsEnabledPropertyId,
            UIA_IsOffscreenPropertyId,
            UIA_IsTogglePatternAvailablePropertyId,
            UIA_ToggleToggleStatePropertyId,
            UIA_ValueValuePropertyId,
            UIA_RangeValueValuePropertyId,
            UIA_SelectionItemIsSelectedPropertyId,
            UIA_ExpandCollapseExpandCollapseStatePropertyId,
        ] {
            let _ = req.AddProperty(p);
        }
        let Ok(cond) = u.ControlViewCondition() else { return Vec::new() };
        let Ok(all) = root.FindAllBuildCache(TreeScope_Descendants, &cond, &req) else { return Vec::new() };
        let n = all.Length().unwrap_or(0).min(3000);
        let mut out = Vec::new();
        for i in 0..n {
            let Ok(el) = all.GetElement(i) else { continue };
            let ct = el.CachedControlType().unwrap_or_default();
            let prop = |id| el.GetCachedPropertyValue(id).ok();
            let toggles = prop(UIA_IsTogglePatternAvailablePropertyId).and_then(|v| bool::try_from(&v).ok()).unwrap_or(false);
            let k = kind(ct, toggles);
            if k.is_empty() {
                continue;
            }
            let name = el.CachedName().map(|b| b.to_string()).unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ");
            let mut state = String::new();
            if toggles || ct == UIA_CheckBoxControlTypeId {
                if let Some(s) = prop(UIA_ToggleToggleStatePropertyId).and_then(|v| i32::try_from(&v).ok()) {
                    state = match s {
                        1 => "on".into(),
                        0 => "off".into(),
                        _ => "partly".into(),
                    };
                }
            }
            if matches!(k, "box" | "dropdown") {
                if let Some(v) = prop(UIA_ValueValuePropertyId).and_then(|v| BSTR::try_from(&v).ok()).map(|b| b.to_string()).filter(|s| !s.is_empty()) {
                    state = format!("\"{}\"", v.chars().take(60).collect::<String>());
                }
            }
            if k == "slider" {
                if let Some(v) = prop(UIA_RangeValueValuePropertyId).and_then(|v| f64::try_from(&v).ok()) {
                    state = format!("{}", (v * 10.0).round() / 10.0);
                }
            }
            if matches!(k, "tab" | "item" | "option") && prop(UIA_SelectionItemIsSelectedPropertyId).and_then(|v| bool::try_from(&v).ok()) == Some(true) {
                state = "selected".into();
            }
            if matches!(k, "dropdown" | "menu" | "item") {
                match prop(UIA_ExpandCollapseExpandCollapseStatePropertyId).and_then(|v| i32::try_from(&v).ok()) {
                    Some(1) => state = [state, "open".into()].join(" ").trim().to_string(),
                    _ => {}
                }
            }
            let enabled = prop(UIA_IsEnabledPropertyId).and_then(|v| bool::try_from(&v).ok()).unwrap_or(true);
            let offscreen = prop(UIA_IsOffscreenPropertyId).and_then(|v| bool::try_from(&v).ok()).unwrap_or(false);
            out.push(Ctl { el, name, kind: k, enabled, offscreen, state });
        }
        out
    }
}

/// What's in a window that can be acted on, one line each ("switch Wi-Fi: on", "box Search: \"cats\"").
pub fn read(h: HWND) -> Result<String, String> {
    let u = uia()?;
    let root = unsafe { u.ElementFromHandle(h) }.map_err(|e| format!("That window can't be read ({e})."))?;
    let mut seen = std::collections::HashSet::new();
    let mut lines = Vec::new();
    for c in controls(&u, &root) {
        if c.offscreen || (c.name.is_empty() && !matches!(c.kind, "box" | "document" | "slider")) {
            continue;
        }
        let line = format!(
            "{} {}{}{}",
            c.kind,
            if c.name.is_empty() { "(no name)" } else { &c.name },
            if c.state.is_empty() { String::new() } else { format!(": {}", c.state) },
            if c.enabled { "" } else { " (greyed out)" }
        );
        if seen.insert(line.clone()) {
            lines.push(line);
        }
        if lines.len() >= 220 {
            lines.push("(and more)".into());
            break;
        }
    }
    if lines.is_empty() {
        return Err("That window shows nothing that can be acted on by name (it may draw its own controls).".into());
    }
    Ok(lines.join("\n"))
}

fn low(s: &str) -> String {
    s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}

/// How well a control's name answers to what was asked for (0: not at all).
fn score(c: &Ctl, want: &str, action: &str) -> i32 {
    let (n, w) = (low(&c.name), low(want));
    if n.is_empty() || w.is_empty() {
        return 0;
    }
    let mut s = if n == w {
        100
    } else if n.starts_with(&w) || w.starts_with(&n) && n.len() > 2 {
        70
    } else if n.contains(&w) {
        55
    } else {
        let ws: Vec<&str> = w.split(' ').collect();
        let hit = ws.iter().filter(|x| n.split(' ').any(|y| y == **x)).count();
        if hit == 0 {
            return 0;
        }
        (40 * hit / ws.len()) as i32
    };
    // (the kind that suits what's to be done)
    let suits = match action {
        "on" | "off" => matches!(c.kind, "switch" | "checkbox" | "option"),
        "set" | "type" => matches!(c.kind, "box" | "slider" | "document" | "dropdown"),
        "select" => matches!(c.kind, "dropdown" | "item" | "option" | "tab"),
        _ => true,
    };
    if suits {
        s += 15;
    }
    if c.offscreen {
        s -= 25;
    }
    if !c.enabled {
        s -= 40;
    }
    s
}

fn center(el: &IUIAutomationElement) -> Option<(i32, i32)> {
    let r: RECT = unsafe { el.CurrentBoundingRectangle() }.ok()?;
    (r.right > r.left && r.bottom > r.top).then(|| ((r.left + r.right) / 2, (r.top + r.bottom) / 2))
}

fn click_at(x: i32, y: i32) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEINPUT};
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::SetCursorPos(x, y);
        let m = |f| INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dwFlags: f, ..Default::default() } } };
        SendInput(&[m(MOUSEEVENTF_LEFTDOWN), m(MOUSEEVENTF_LEFTUP)], std::mem::size_of::<INPUT>() as i32);
    }
}

/// Ctrl+A: what's in the box, chosen, so typing replaces it.
fn select_all() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY};
    let k = |vk: u16, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() }, ..Default::default() } },
    };
    unsafe { SendInput(&[k(0x11, false), k(0x41, false), k(0x41, true), k(0x11, true)], std::mem::size_of::<INPUT>() as i32) };
}

/// Press it the way it's meant to be pressed: invoked, toggled, chosen, opened, its default action, or at last clicked.
fn press(el: &IUIAutomationElement) -> Result<&'static str, String> {
    unsafe {
        if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId) {
            if p.Invoke().is_ok() {
                return Ok("pressed");
            }
        }
        if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId) {
            if p.Toggle().is_ok() {
                return Ok("switched");
            }
        }
        if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId) {
            if p.Select().is_ok() {
                return Ok("chose");
            }
        }
        if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(UIA_ExpandCollapsePatternId) {
            let open = p.CurrentExpandCollapseState().map(|s| s == ExpandCollapseState_Expanded).unwrap_or(false);
            if (if open { p.Collapse() } else { p.Expand() }).is_ok() {
                return Ok(if open { "closed" } else { "opened" });
            }
        }
        if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(UIA_LegacyIAccessiblePatternId) {
            if p.DoDefaultAction().is_ok() {
                return Ok("pressed");
            }
        }
    }
    let (x, y) = center(el).ok_or("It isn't on the screen to be clicked.")?;
    click_at(x, y);
    Ok("clicked")
}

/// Act on the control called `name` in this window: "click", "on", "off", "set" (text or a number), "select" (one of
/// its options, `value`), "expand", "collapse", "focus", "scroll" (into view) or "read".
pub fn act(h: HWND, name: &str, action: &str, value: &str) -> Result<String, String> {
    let u = uia()?;
    let root = unsafe { u.ElementFromHandle(h) }.map_err(|e| format!("That window can't be reached ({e})."))?;
    let all = controls(&u, &root);
    let best = all.iter().map(|c| (score(c, name, action), c)).filter(|(s, _)| *s >= 30).max_by_key(|(s, _)| *s).map(|(_, c)| c);
    let c = best.ok_or_else(|| {
        let near: Vec<String> = all.iter().filter(|c| !c.name.is_empty() && !c.offscreen).take(25).map(|c| format!("{} {}", c.kind, c.name)).collect();
        format!("There's nothing called “{name}” there. What is there: {}", near.join(", "))
    })?;
    let el = &c.el;
    let label = if c.name.is_empty() { name.to_string() } else { c.name.trim_end_matches([':', ' ']).to_string() };
    unsafe {
        match action {
            "on" | "off" => {
                let want = action == "on";
                if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId) {
                    let now = p.CurrentToggleState().map(|s| s == ToggleState_On).unwrap_or(!want);
                    if now == want {
                        return Ok(format!("{label} was already {action}."));
                    }
                    p.Toggle().map_err(|e| e.to_string())?;
                    std::thread::sleep(Duration::from_millis(150));
                    let after = p.CurrentToggleState().map(|s| s == ToggleState_On).unwrap_or(want);
                    return Ok(format!("{label} is {}.", if after { "on" } else { "off" }));
                }
                if want {
                    if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId) {
                        p.Select().map_err(|e| e.to_string())?;
                        return Ok(format!("Chose {label}."));
                    }
                }
                Err(format!("{label} isn't something that turns on and off."))
            }
            "set" | "type" => {
                if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationRangeValuePattern>(UIA_RangeValuePatternId) {
                    if let Ok(n) = value.trim().trim_end_matches('%').parse::<f64>() {
                        let (lo, hi) = (p.CurrentMinimum().unwrap_or(0.0), p.CurrentMaximum().unwrap_or(100.0));
                        p.SetValue(n.clamp(lo, hi)).map_err(|e| e.to_string())?;
                        return Ok(format!("{label} is set to {n}."));
                    }
                }
                if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) {
                    if !p.CurrentIsReadOnly().map(|b| b.as_bool()).unwrap_or(true) && p.SetValue(&BSTR::from(value)).is_ok() {
                        return Ok(format!("Typed into {label}."));
                    }
                }
                // (a box that won't take its words directly: clicked into, emptied, and typed into)
                let _ = el.SetFocus();
                if let Some((x, y)) = center(el) {
                    click_at(x, y);
                }
                std::thread::sleep(Duration::from_millis(120));
                select_all();
                crate::commands::type_text(value);
                Ok(format!("Typed into {label}."))
            }
            "select" => {
                // a list or dropdown: opened, and the option chosen from what shows
                if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(UIA_ExpandCollapsePatternId) {
                    let _ = p.Expand();
                    std::thread::sleep(Duration::from_millis(280));
                }
                let t0 = Instant::now();
                loop {
                    // (its options may be in a box of their own, outside the window: looked for there too)
                    let mut pool = controls(&u, el);
                    if pool.iter().all(|o| score(o, value, "select") < 30) {
                        pool.extend(controls(&u, &root));
                        // (a list that opened in a box of its own: around what has the keyboard now)
                        if let (Ok(f), Ok(walk)) = (u.GetFocusedElement(), u.ControlViewWalker()) {
                            if let Ok(up) = walk.GetParentElement(&f) {
                                pool.extend(controls(&u, &up));
                            }
                        }
                    }
                    if let Some(o) = pool.iter().filter(|o| !std::ptr::eq(*o, c) && o.name != c.name).map(|o| (score(o, value, "select"), o)).filter(|(s, _)| *s >= 40).max_by_key(|(s, _)| *s).map(|(_, o)| o) {
                        let how = press(&o.el)?;
                        if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(UIA_ExpandCollapsePatternId) {
                            if p.CurrentExpandCollapseState().map(|s| s == ExpandCollapseState_Expanded).unwrap_or(false) {
                                let _ = p.Collapse();
                            }
                        }
                        let _ = how;
                        return Ok(format!("Chose {} in {label}.", o.name));
                    }
                    if t0.elapsed() > Duration::from_millis(1200) {
                        return Err(format!("{label} has no option called “{value}”."));
                    }
                    std::thread::sleep(Duration::from_millis(150));
                }
            }
            "expand" | "collapse" => {
                let p = el.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(UIA_ExpandCollapsePatternId).map_err(|_| format!("{label} doesn't open or close."))?;
                (if action == "expand" { p.Expand() } else { p.Collapse() }).map_err(|e| e.to_string())?;
                Ok(format!("{label} {}.", if action == "expand" { "is open" } else { "is closed" }))
            }
            "focus" => {
                el.SetFocus().map_err(|e| e.to_string())?;
                Ok(format!("{label} has the keyboard."))
            }
            "scroll" => {
                let p = el.GetCurrentPatternAs::<IUIAutomationScrollItemPattern>(UIA_ScrollItemPatternId).map_err(|_| format!("{label} can't be scrolled to."))?;
                p.ScrollIntoView().map_err(|e| e.to_string())?;
                Ok(format!("Scrolled to {label}."))
            }
            "read" => Ok(format!("{} {}{}", c.kind, label, if c.state.is_empty() { String::new() } else { format!(": {}", c.state) })),
            _ => {
                let how = press(el)?;
                Ok(format!("{} {label}.", match how {
                    "switched" => "Switched",
                    "chose" => "Chose",
                    "opened" => "Opened",
                    "closed" => "Closed",
                    "clicked" => "Clicked",
                    _ => "Pressed",
                }))
            }
        }
    }
}

/// The window of an app by its name ("" or none open: the one in front, if it isn't ULTRON's).
pub fn window_of(app: &tauri::AppHandle, name: &str) -> Option<HWND> {
    use tauri::Manager;
    let n = name.trim().to_lowercase();
    if !n.is_empty() {
        let wins = app.state::<crate::apps::Apps>().open_windows();
        let hit = wins.iter().find(|w| w.name.to_lowercase() == n).or_else(|| wins.iter().find(|w| w.name.to_lowercase().contains(&n) || w.title.to_lowercase().contains(&n)));
        // (an app asked for by name and not open is never stood in for by whatever is in front)
        return hit.and_then(|w| w.id.parse::<isize>().ok()).map(|v| HWND(v as _));
    }
    let h = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
    let mut pid = 0u32;
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(h, Some(&mut pid)) };
    (!h.is_invalid() && pid != std::process::id()).then_some(h)
}
