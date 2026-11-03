//! Which brains this PC runs, picked from what it has (memory, cores, graphics card).
//!   Light    (under 12 GB, or few cores): Gemma 3 1B to think, Qwen2.5-VL 3B to see
//!   Standard (most PCs):                  Qwen3 4B Instruct 2507 to think, Qwen2.5-VL 3B to see
//!     (head to head on 32 questions it fixed every slip Phi-4-mini made: 17x24, 3+24-7, feathers vs steel...)
//!   Power    (8 GB+ graphics card):       Qwen3 8B to think (Microsoft Phi-4 14B with 12 GB+), Qwen2.5-VL 7B to see
//! A small quick brain on the graphics card handles everyday chat and the AI tools (quick.rs).
//! Requests are routed too: fixed commands and actions never wait for a brain, questions go to
//! the talking brain, and anything about what's on screen goes to the seeing one.

use std::sync::OnceLock;

use serde::Serialize;

#[derive(Clone, Serialize)]
pub struct Specs {
    pub ram_gb: f64,
    pub cores: usize,
    pub vram_gb: f64,
    pub gpu: String,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tier {
    Light,
    Standard,
    Power,
}

pub struct Model {
    pub name: &'static str,
    pub file: &'static str,
    pub url: &'static str,
    pub min: f64, // a complete download is at least this big
    pub mmproj: Option<(&'static str, &'static str)>,
}

pub const GEMMA_1B: Model = Model {
    name: "Light · Gemma 3 1B",
    file: "gemma-3-1b-it-Q4_K_M.gguf",
    url: "https://huggingface.co/ggml-org/gemma-3-1b-it-GGUF/resolve/main/gemma-3-1b-it-Q4_K_M.gguf",
    min: 700e6,
    mmproj: None,
};
/// shipped beside the installer as SYLVER-Brain.gguf
pub const GRANITE: Model = Model {
    name: "Smart · Granite 4.0",
    file: "SYLVER-Brain.gguf",
    url: "https://huggingface.co/ibm-granite/granite-4.0-h-tiny-GGUF/resolve/main/granite-4.0-h-tiny-Q4_K_M.gguf",
    min: 2e9,
    mmproj: None,
};
pub const QWEN3_4B_FILE: &str = "Qwen3-4B-Instruct-2507-Q4_K_M.gguf";
/// Qwen3 4B Instruct (2507): the most accurate brain of its size, 2.3 GB, answers without "thinking" first
pub const QWEN3_4B: Model = Model {
    name: "Smart · Qwen3 4B",
    file: QWEN3_4B_FILE,
    url: "https://huggingface.co/unsloth/Qwen3-4B-Instruct-2507-GGUF/resolve/main/Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
    min: 2.2e9,
    mmproj: None,
};
/// Microsoft Phi-4-mini (3.8B): strong reasoning for its size, 2.5 GB
pub const PHI4_MINI: Model = Model {
    name: "Smart · Phi-4-mini",
    file: "microsoft_Phi-4-mini-instruct-Q4_K_M.gguf",
    url: "https://huggingface.co/bartowski/microsoft_Phi-4-mini-instruct-GGUF/resolve/main/microsoft_Phi-4-mini-instruct-Q4_K_M.gguf",
    min: 2.3e9,
    mmproj: None,
};
/// Microsoft Phi-4 (14B), for PCs whose graphics card can hold it
pub const PHI4: Model = Model {
    name: "Power · Phi-4",
    file: "phi-4-Q4_K_S.gguf",
    url: "https://huggingface.co/microsoft/phi-4-gguf/resolve/main/phi-4-Q4_K_S.gguf",
    min: 8e9,
    mmproj: None,
};
pub const QWEN3_8B: Model = Model {
    name: "Power · Qwen3 8B",
    file: "Qwen3-8B-Q4_K_M.gguf",
    url: "https://huggingface.co/Qwen/Qwen3-8B-GGUF/resolve/main/Qwen3-8B-Q4_K_M.gguf",
    min: 4.5e9,
    mmproj: None,
};
pub const VL_3B: Model = Model {
    name: "Vision · Qwen2.5-VL 3B",
    file: "Qwen2.5-VL-3B-Instruct-Q4_K_M.gguf",
    url: "https://huggingface.co/ggml-org/Qwen2.5-VL-3B-Instruct-GGUF/resolve/main/Qwen2.5-VL-3B-Instruct-Q4_K_M.gguf",
    min: 1.8e9,
    mmproj: Some(("mmproj-Qwen2.5-VL-3B-Instruct-Q8_0.gguf", "https://huggingface.co/ggml-org/Qwen2.5-VL-3B-Instruct-GGUF/resolve/main/mmproj-Qwen2.5-VL-3B-Instruct-Q8_0.gguf")),
};
pub const VL_7B: Model = Model {
    name: "Vision · Qwen2.5-VL 7B",
    file: "Qwen2.5-VL-7B-Instruct-Q4_K_M.gguf",
    url: "https://huggingface.co/ggml-org/Qwen2.5-VL-7B-Instruct-GGUF/resolve/main/Qwen2.5-VL-7B-Instruct-Q4_K_M.gguf",
    min: 4.4e9,
    mmproj: Some(("mmproj-Qwen2.5-VL-7B-Instruct-Q8_0.gguf", "https://huggingface.co/ggml-org/Qwen2.5-VL-7B-Instruct-GGUF/resolve/main/mmproj-Qwen2.5-VL-7B-Instruct-Q8_0.gguf")),
};

/// The biggest graphics card's dedicated memory and name (software adapters skipped).
fn graphics() -> (f64, String) {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE};
    let mut best = (0.0, String::new());
    unsafe {
        let Ok(f) = CreateDXGIFactory1::<IDXGIFactory1>() else { return best };
        let mut i = 0;
        while let Ok(a) = f.EnumAdapters1(i) {
            i += 1;
            let Ok(d) = a.GetDesc1() else { continue };
            if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            let gb = d.DedicatedVideoMemory as f64 / (1u64 << 30) as f64;
            if gb > best.0 {
                let n = String::from_utf16_lossy(&d.Description);
                best = (gb, n.trim_end_matches('\0').trim().to_string());
            }
        }
    }
    best
}

pub fn specs() -> &'static Specs {
    static S: OnceLock<Specs> = OnceLock::new();
    S.get_or_init(|| {
        use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        let ram = if unsafe { GlobalMemoryStatusEx(&mut m) }.is_ok() { m.ullTotalPhys as f64 / (1u64 << 30) as f64 } else { 8.0 };
        let (vram_gb, gpu) = graphics();
        let s = Specs { ram_gb: ram, cores: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4), vram_gb, gpu };
        crate::log_line(&format!("this PC: {:.1} GB RAM, {} threads, {} ({:.1} GB)", s.ram_gb, s.cores, s.gpu, s.vram_gb));
        s
    })
}

pub fn tier_of(s: &Specs) -> Tier {
    if s.ram_gb < 11.5 || s.cores < 6 || s.vram_gb < 4.5 {
        Tier::Light
    } else if s.vram_gb >= 7.5 && s.ram_gb >= 15.5 {
        Tier::Power
    } else {
        Tier::Standard
    }
}

pub fn tier() -> Tier {
    tier_of(specs())
}

/// The talking brain this PC should have.
pub fn text_model() -> &'static Model {
    match tier() {
        Tier::Light => &GEMMA_1B,
        Tier::Standard => &QWEN3_4B,
        Tier::Power if specs().vram_gb >= 11.5 => &PHI4,
        Tier::Power => &QWEN3_8B,
    }
}

/// Talking brains this PC can run well, best first (whichever is on disk wins).
pub fn text_order() -> Vec<&'static str> {
    match tier() {
        Tier::Light => vec![GEMMA_1B.file],
        Tier::Standard => vec![QWEN3_4B_FILE, PHI4_MINI.file, GRANITE.file, GEMMA_1B.file],
        Tier::Power => vec![PHI4.file, QWEN3_8B.file, QWEN3_4B_FILE, PHI4_MINI.file, GRANITE.file],
    }
}

/// The seeing brain this PC should have.
pub fn vision_model() -> &'static Model {
    if tier() == Tier::Power || specs().ram_gb >= 23.5 { &VL_7B } else { &VL_3B }
}

/// A friendly name for a brain file.
pub fn name_of(file: &str) -> &'static str {
    [&GEMMA_1B, &GRANITE, &QWEN3_8B, &QWEN3_4B, &PHI4_MINI, &PHI4, &VL_3B, &VL_7B].iter().find(|m| m.file == file).map(|m| m.name).unwrap_or("Fast · Qwen2.5 1.5B")
}

/// Questions about the screen that need eyes, not just its text.
pub fn wants_eyes(q: &str) -> bool {
    let l = q.to_lowercase();
    ["image", "picture", "photo", "see", "look", "looks like", "color", "colour", "describe", "who is", "what is this", "what's this", "diagram", "chart", "graph",
        "logo", "icon", "video", "game", "design", "screenshot", "meme", "drawing", "map"]
        .iter()
        .any(|w| l.split(|c: char| !c.is_alphanumeric() && c != '\'').collect::<Vec<_>>().join(" ").contains(w))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tiers() {
        let s = |ram, cores, vram| Specs { ram_gb: ram, cores, vram_gb: vram, gpu: String::new() };
        assert_eq!(tier_of(&s(8.0, 8, 0.0)), Tier::Light);
        assert_eq!(tier_of(&s(16.0, 4, 0.0)), Tier::Light);
        assert_eq!(tier_of(&s(15.8, 16, 4.0)), Tier::Light);
        assert_eq!(tier_of(&s(15.8, 16, 6.0)), Tier::Standard);
        assert_eq!(tier_of(&s(32.0, 16, 12.0)), Tier::Power);
        assert!(wants_eyes("what's in this picture"));
        assert!(!wants_eyes("summarize this page"));
        let me = specs();
        println!("this PC: {:.1} GB, {} threads, {} {:.1} GB -> {:?}", me.ram_gb, me.cores, me.gpu, me.vram_gb, tier());
    }
}
