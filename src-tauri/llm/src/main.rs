//! ULTRON's local brain: runs a llama.cpp model on this PC and streams replies.
//! Protocol (JSON lines): stdin  {"messages":[["system","..."],["user","..."]],"max":200}
//!                        stdout {"ready":true} once, then {"t":"piece"}... {"done":true} per request.

use std::io::{BufRead, Write};
use std::num::NonZeroU32;

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{LlamaChatMessage, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::token::LlamaToken;
use serde_json::json;

const N_CTX: u32 = 4096;
const BATCH: usize = 512;

#[derive(serde::Deserialize)]
struct Req {
    messages: Vec<(String, String)>,
    #[serde(default = "default_max")]
    max: usize,
}
fn default_max() -> usize {
    220
}

fn out(v: serde_json::Value) {
    let mut o = std::io::stdout().lock();
    let _ = writeln!(o, "{v}");
    let _ = o.flush();
}

/// Emit only complete UTF-8 (a token can end mid-character).
fn drain_utf8(buf: &mut Vec<u8>) -> String {
    match std::str::from_utf8(buf) {
        Ok(s) => {
            let s = s.to_string();
            buf.clear();
            s
        }
        Err(e) => {
            let good = e.valid_up_to();
            let s = String::from_utf8_lossy(&buf[..good]).to_string();
            buf.drain(..good);
            s
        }
    }
}

fn chatml(msgs: &[(String, String)]) -> String {
    let mut s = String::new();
    for (r, c) in msgs {
        s.push_str(&format!("<|im_start|>{r}\n{c}<|im_end|>\n"));
    }
    s.push_str("<|im_start|>assistant\n");
    s
}

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        out(json!({"error": "no model path"}));
        return;
    };
    let mut backend = match LlamaBackend::init() {
        Ok(b) => b,
        Err(e) => return out(json!({"error": e.to_string()})),
    };
    backend.void_logs();
    let model = match LlamaModel::load_from_file(&backend, &path, &LlamaModelParams::default()) {
        Ok(m) => m,
        Err(e) => return out(json!({"error": e.to_string()})),
    };
    let threads = (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8) / 2).max(2) as i32;
    let params = LlamaContextParams::default()
        .with_n_ctx(NonZeroU32::new(N_CTX))
        .with_n_batch(BATCH as u32)
        .with_n_threads(threads)
        .with_n_threads_batch(threads);
    let mut ctx = match model.new_context(&backend, params) {
        Ok(c) => c,
        Err(e) => return out(json!({"error": e.to_string()})),
    };
    let tmpl = model.chat_template(None).ok();
    let vocab = model.vocab();
    let mut cached: Vec<LlamaToken> = Vec::new();
    let mut batch = LlamaBatch::new(BATCH, 1);
    out(json!({"ready": true}));

    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(req) = serde_json::from_str::<Req>(&line) else {
            out(json!({"done": true}));
            continue;
        };
        let msgs: Vec<LlamaChatMessage> =
            req.messages.iter().filter_map(|(r, c)| LlamaChatMessage::new(r.clone(), c.clone()).ok()).collect();
        let prompt = tmpl
            .as_ref()
            .and_then(|t| model.apply_chat_template(t, &msgs, true).ok())
            .unwrap_or_else(|| chatml(&req.messages));
        let mut toks = vocab.tokenize(prompt.as_bytes(), true, true);
        let room = (N_CTX as usize).saturating_sub(req.max + 8);
        if toks.len() > room {
            toks.drain(..toks.len() - room);
            cached.clear();
        }

        // reuse whatever the model has already read (system prompt, earlier turns)
        let mut common = cached.iter().zip(&toks).take_while(|(a, b)| a == b).count();
        if common >= toks.len() {
            common = toks.len() - 1;
        }
        if ctx.clear_kv_cache_seq(Some(0), Some(common as u32), None).is_err() {
            ctx.clear_kv_cache();
            common = 0;
        }
        let mut pos = common as i32;
        let mut last_idx = 0;
        let mut failed = false;
        for chunk in toks[common..].chunks(BATCH) {
            batch.clear();
            let end = pos as usize + chunk.len() == toks.len();
            for (i, t) in chunk.iter().enumerate() {
                let is_last = end && i == chunk.len() - 1;
                if batch.add(*t, pos, &[0], is_last).is_err() {
                    failed = true;
                }
                pos += 1;
            }
            if failed || ctx.decode(&mut batch).is_err() {
                failed = true;
                break;
            }
            last_idx = batch.n_tokens() - 1;
        }
        if failed {
            ctx.clear_kv_cache();
            cached.clear();
            out(json!({"done": true, "error": "decode failed"}));
            continue;
        }
        cached = toks.clone();

        let mut sampler = LlamaSampler::chain_simple([
            LlamaSampler::penalties(model.n_vocab(), 64, 1.08, 0.0, 0.0),
            LlamaSampler::top_k(30),
            LlamaSampler::top_p(0.85, 1),
            LlamaSampler::temp(0.3), // calm and factual: fewer made-up answers
            LlamaSampler::dist(1234),
        ]);
        let mut bytes: Vec<u8> = Vec::new();
        for _ in 0..req.max {
            let tok = sampler.sample(&ctx, last_idx);
            sampler.accept(tok);
            if vocab.is_eog(tok) {
                break;
            }
            bytes.extend(vocab.token_to_piece(tok, false, None));
            let piece = drain_utf8(&mut bytes);
            if !piece.is_empty() {
                out(json!({"t": piece}));
            }
            batch.clear();
            if batch.add(tok, pos, &[0], true).is_err() || ctx.decode(&mut batch).is_err() {
                break;
            }
            cached.push(tok);
            pos += 1;
            last_idx = 0;
            if pos as u32 >= N_CTX - 4 {
                break;
            }
        }
        let rest = String::from_utf8_lossy(&bytes).to_string();
        if !rest.is_empty() {
            out(json!({"t": rest}));
        }
        out(json!({"done": true}));
    }
}
