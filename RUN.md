# Running ULTRON from source (Windows)

ULTRON is a Tauri 2 app: a Rust backend (`src-tauri/`) and one web page (`src/index.html`). It runs on Windows 10/11, 64-bit.

## 1. Install the tools (once)

| Tool | How | Why |
|---|---|---|
| Visual Studio 2022 Build Tools | Install with the **Desktop development with C++** workload | MSVC compiler, Windows SDK, and the CMake that builds whisper.cpp |
| Rust (stable, MSVC) | `winget install Rustlang.Rustup`, then `rustup default stable-x86_64-pc-windows-msvc` | The backend |
| Node.js LTS | `winget install OpenJS.NodeJS.LTS` | The Tauri CLI |
| Python 3 + libclang | `winget install Python.Python.3.13`, then `pip install --user libclang` | bindgen (whisper-rs) needs libclang |
| WebView2 Runtime | Already on Windows 11; on Windows 10: `winget install Microsoft.EdgeWebView2Runtime` | Draws the page |

Then open `src-tauri/.cargo/config.toml` and set the two paths to where they are on your PC:

- `CMAKE`: the `cmake.exe` inside your VS Build Tools (`...\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe`)
- `LIBCLANG_PATH`: the `clang\native` folder of the pip `libclang` package (`pip show -f libclang` shows where it is)

Leave the two `CMAKE_*_FLAGS_RELEASE` lines as they are (without them dictation is about 50x slower).

## 2. Put the large files in place

These are other projects' binaries and models, too big for git (see `.gitignore`). Get them from whoever shares the project with you and copy them to:

| Folder | What goes there |
|---|---|
| `src-tauri/resources/llama/` | llama.cpp's `llama-server.exe` (build b11136) and all its DLLs (`ggml-*.dll`, `llama.dll`, ...) |
| `src-tauri/resources/models/` | `ggml-small.en-q5_1.bin`, `ggml-small-q5_1.bin` (Whisper), `qwen2.5-1.5b-instruct-q4_k_m.gguf`, `Kim_Vocal_2.onnx` |
| `src-tauri/resources/voices/` | `en_US-lessac-high.onnx` (+ `.onnx.json`), `en_US-ryan-high.onnx` (+ `.onnx.json`) |
| `src-tauri/resources/piper/` | `piper.exe`, its DLLs and the `espeak-ng-data` folder |
| `src-tauri/resources/screen/` | `omni-icon-detect.onnx` (OmniParser v2 icon detector, AGPL-3.0) |
| `src-tauri/binaries/` | `ultron-llm-x86_64-pc-windows-msvc.exe` (build it: `cd src-tauri/llm` then `cargo build --release`, and copy the exe here with that name) |

ULTRON fetches its larger AI brains itself the first time it runs (Settings › Brain shows the progress).

## 3. Run it

From the project folder (`ultron-tauri`):

```
npm install
npm run dev
```

`npm run dev` builds the backend and opens ULTRON (the first build takes 10-20 minutes; later ones a minute or two).

To build the release app and its installer:

```
npm run build
```

The installer lands in `src-tauri/target/release/bundle/nsis/`. Tip: the build folder is several GB; if the project sits in OneDrive, put it outside with `set CARGO_TARGET_DIR=C:\ultron-build` first.

## 4. Keys and the `.env` file

**ULTRON needs no `.env` file and no keys to run.** Nothing is read from a `.env`; it answers on the PC by itself, and its web answers need no key.

Keys you can add later are typed into the app and kept in **Windows Credential Manager**, never in a file or the repository:

| Where in ULTRON | Key name |
|---|---|
| Settings › Brain › My own key | API key for the chosen provider: Anthropic, OpenAI, Gemini, OpenRouter, or a custom OpenAI-compatible service (plus its base URL and model name) |
| Notch › Email | Email address + app password (e.g. a Google app password) |
| Notch › Connect › Online accounts | GitHub personal access token, Notion integration secret, Todoist API token, Slack token, Home Assistant long-lived access token (+ its address), Discord webhook URL |

Environment variables the code reads (all optional, none secret):

- `ULTRON_DEV_INBOX=1`: developer mode (takes test requests from `%TEMP%\ultron-inbox.txt`)
- `ULTRON_MODEL`, `ULTRON_WAVS`, `ULTRON_LIMIT`, `ULTRON_CTX`, `ULTRON_GREEDY`, `ULTRON_MIX`: only for the speech and lyrics tests (`cargo test -- --ignored`)
