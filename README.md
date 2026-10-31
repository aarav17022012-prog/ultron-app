# ULTRON (Tauri)

A notch at the top of the screen with a local voice assistant, plus a macOS-style
dock at the bottom. Both are always-on-top, transparent windows; clicks outside
what they draw pass straight through to the desktop.

## Notch
- Hover to grow it, click to expand. It stays on screen (Alt+F4 included); quit from the tray icon.
- **Assistant:** speech recognition runs on this PC with Whisper (whisper.cpp, `small.en`). No API key,
  no cloud. The model (~190 MB) downloads once on first launch to `%APPDATA%\com.ultron.notch\models`.
  - When the notch is closed it only reacts to “Ultron …” (turn off *Wake word* in Settings to react to everything).
  - Commands: open apps / sites / folders, search Google or YouTube, “play X (on Spotify)”, pause / resume /
    next / previous, volume up / down / to N% / mute, “type …” into the focused app, screenshot, show desktop,
    lock the PC, close an app, time and date. Set alarms with a clock time (“set an alarm for 7:30 pm”) or a
    duration (“set an alarm in 10 minutes”); spoken number times such as “seven thirty pm” and “tomorrow morning”
    are understood too. Anything else is reported back, never guessed at.
  - Replies are spoken with an on-device Windows voice (toggle *Speak replies*).
- **Lyrics:** follows whatever Windows reports as playing (Spotify, browsers, media apps) with album art,
  play / pause / skip / seek, and time-synced lyrics from LRCLIB (free, no key).
- **Activity:** a log of what ULTRON actually did.

## Menu bar
- A macOS-style bar across the top with the notch in the middle. Shows the app in front ("Finder" for the
  desktop and File Explorer), battery, Wi-Fi / quick settings, search, the mic state, and the clock.
- File / Edit / View / Window menus act on the app you were using (Save, Copy, Zoom, Minimize, snap, …);
  Go opens folders; the ULTRON menu has Settings, Task Manager, Lock Screen, and Quit.
- Registered as a Windows app bar, so maximized windows start below it. It slides away for fullscreen apps.
- The taskbar wheel opens from its chevron or configured shortcut. Use **Up/Down** to choose apps or windows,
  **Right** to enter an app’s open windows, **Left** to return, **Enter** to open/focus, and **Esc** to close.
- Sylver’s Start/search opens with **Ctrl+Space**; when selected in Settings, the **Windows key** opens it too.

## Building
Needs Rust, Node, and the VS C++ Build Tools (its bundled CMake is used for whisper.cpp).
bindgen needs libclang; the lightest way to get it:

```
pip install --user libclang
```

`src-tauri/.cargo/config.toml` points `LIBCLANG_PATH` and `CMAKE` at those; adjust if your paths differ.

```
npm install
npm run dev      # development
npm run build    # release exe + installer in src-tauri/target/release/bundle/nsis
```

Pages: `src/index.html` (notch), `src/dock.html` (dock).
Backend: `src-tauri/src/` — `voice.rs` (mic, VAD, Whisper), `commands.rs` (what you say → actions),
`media.rs` (now playing), `lyrics.rs`, `apps.rs` (app list, windows, icons), `appbar.rs` (screen space),
`hit.rs` (click-through).

## What's not in this repository
The app needs these to build and run. They are other projects' binaries or models, and too big for git:
- `src-tauri/resources/llama/`: llama.cpp's `llama-server` (build b11136) and its DLLs
- `src-tauri/resources/models/`, `src-tauri/resources/voices/`, `src-tauri/resources/piper/`: bundled models and the voice engine
- `src-tauri/resources/screen/`: the OmniParser v2 icon detector (`omni-icon-detect.onnx`, AGPL-3.0)
- `src-tauri/binaries/`: the `ultron-llm` helper, built from `src-tauri/llm`
