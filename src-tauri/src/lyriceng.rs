//! The SYLVER Lyric Engine, part 1: vocal isolation. Music drowns out a singer for
//! speech recognition, so before listening SYLVER pulls the voice out of the mix with
//! a neural source-separation model (MDX-Net "Kim Vocal 2", run with ONNX Runtime).
//! Then part 2 (songs.rs) finds each sung phrase and recognises it from the clean voice.
//!
//! The model works on a stereo spectrogram of ~5.9 s of 44.1 kHz audio:
//! STFT (7680-point, hop 1024, Hann) -> [real, imag] x 2 channels x 3072 bins x 256 frames
//! -> model -> the vocals' spectrogram -> inverse STFT.

use std::sync::Arc;

use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

pub const SR: usize = 44_100;
const NFFT: usize = 7680;
const HOP: usize = 1024;
const DIMF: usize = 3072;
const DIMT: usize = 256;
const NB: usize = NFFT / 2 + 1;
/// samples per channel the model looks at
pub const CHUNK: usize = HOP * (DIMT - 1);
/// context dropped from each end (the edges of a window are less clean)
pub const TRIM: usize = NFFT / 2;
/// clean samples each window gives back
pub const GEN: usize = CHUNK - 2 * TRIM;
const COMPENSATE: f32 = 1.009;

pub struct Separator {
    session: Session,
    fft: Arc<dyn RealToComplex<f32>>,
    ifft: Arc<dyn ComplexToReal<f32>>,
    win: Vec<f32>,
}

impl Separator {
    pub fn load(path: &std::path::Path, threads: usize) -> Option<Separator> {
        let session = Session::builder()
            .ok()?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .ok()?
            .with_intra_threads(threads)
            .ok()?
            .commit_from_file(path)
            .ok()?;
        let mut planner = RealFftPlanner::<f32>::new();
        let win = (0..NFFT).map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / NFFT as f32).cos()).collect();
        Some(Separator { session, fft: planner.plan_fft_forward(NFFT), ifft: planner.plan_fft_inverse(NFFT), win })
    }

    /// One channel's spectrogram, centred frames with reflect padding: [DIMT][NB].
    fn stft(&self, x: &[f32]) -> Vec<Vec<Complex32>> {
        let half = NFFT / 2;
        let at = |i: isize| -> f32 {
            // reflect padding at both ends
            let n = x.len() as isize;
            let j = if i < 0 { -i } else if i >= n { 2 * (n - 1) - i } else { i };
            x[j.clamp(0, n - 1) as usize]
        };
        let mut frames = Vec::with_capacity(DIMT);
        let mut buf = self.fft.make_input_vec();
        let mut spec = self.fft.make_output_vec();
        for t in 0..DIMT {
            let start = (t * HOP) as isize - half as isize;
            for (k, b) in buf.iter_mut().enumerate() {
                *b = at(start + k as isize) * self.win[k];
            }
            let _ = self.fft.process(&mut buf, &mut spec);
            frames.push(spec.clone());
        }
        frames
    }

    /// One channel back to audio from its spectrogram ([DIMT][NB]), overlap-added and normalised.
    fn istft(&self, frames: &mut [Vec<Complex32>], len: usize) -> Vec<f32> {
        let half = NFFT / 2;
        let total = NFFT + HOP * (DIMT - 1);
        let mut out = vec![0f32; total];
        let mut norm = vec![0f32; total];
        let mut buf = self.ifft.make_output_vec();
        for (t, f) in frames.iter_mut().enumerate() {
            f[0].im = 0.0;
            f[NB - 1].im = 0.0;
            let _ = self.ifft.process(f, &mut buf);
            let o = t * HOP;
            for k in 0..NFFT {
                out[o + k] += buf[k] / NFFT as f32 * self.win[k];
                norm[o + k] += self.win[k] * self.win[k];
            }
        }
        (0..len).map(|i| out[half + i] / norm[half + i].max(1e-8)).collect()
    }

    /// Vocals of a stereo window of exactly CHUNK samples per channel. Returns the clean middle
    /// GEN samples, mixed to mono, at 44.1 kHz.
    pub fn isolate(&mut self, l: &[f32], r: &[f32]) -> Option<Vec<f32>> {
        if l.len() != CHUNK || r.len() != CHUNK {
            return None;
        }
        let specs = [self.stft(l), self.stft(r)];
        // [1, 4, DIMF, DIMT]: left real, left imag, right real, right imag
        let mut input = vec![0f32; 4 * DIMF * DIMT];
        for (c, s) in specs.iter().enumerate() {
            for (t, frame) in s.iter().enumerate() {
                for f in 0..DIMF {
                    input[(c * 2) * DIMF * DIMT + f * DIMT + t] = frame[f].re;
                    input[(c * 2 + 1) * DIMF * DIMT + f * DIMT + t] = frame[f].im;
                }
            }
        }
        let tensor = Tensor::from_array(([1usize, 4, DIMF, DIMT], input)).ok()?;
        let y: Vec<f32> = {
            let outputs = self.session.run(ort::inputs!["input" => tensor]).ok()?;
            let (_, y) = outputs["output"].try_extract_tensor::<f32>().ok()?;
            y.to_vec()
        };
        let mut mono = vec![0f32; GEN];
        for c in 0..2 {
            let mut frames: Vec<Vec<Complex32>> = (0..DIMT)
                .map(|t| {
                    let mut v = vec![Complex32::new(0.0, 0.0); NB];
                    for f in 0..DIMF {
                        v[f] = Complex32::new(y[(c * 2) * DIMF * DIMT + f * DIMT + t], y[(c * 2 + 1) * DIMF * DIMT + f * DIMT + t]);
                    }
                    v
                })
                .collect();
            let wave = self.istft(&mut frames, CHUNK);
            for i in 0..GEN {
                mono[i] += wave[TRIM + i] * 0.5 * COMPENSATE;
            }
        }
        Some(mono)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// cargo test --release --bin ultron isolates -- --ignored --nocapture  (SYLVER_MIX = a 44.1 kHz stereo wav)
    #[test]
    #[ignore]
    fn isolates() {
        let path = std::env::var("SYLVER_MIX").unwrap();
        let b = std::fs::read(&path).unwrap();
        let data = b.windows(4).position(|w| w == b"data").unwrap() + 8;
        let s: Vec<f32> = b[data..].chunks(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect();
        let (l, r): (Vec<f32>, Vec<f32>) = (s.iter().step_by(2).copied().collect(), s.iter().skip(1).step_by(2).copied().collect());
        let model = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources").join("models").join("Kim_Vocal_2.onnx");
        let mut sep = Separator::load(&model, 8).unwrap();
        let n = l.len();
        let pad = GEN - n % GEN;
        let padv = |x: &[f32]| [vec![0f32; TRIM], x.to_vec(), vec![0f32; pad + TRIM]].concat();
        let (pl, pr) = (padv(&l), padv(&r));
        let mut out = Vec::new();
        let t0 = std::time::Instant::now();
        let mut i = 0;
        while i < n + pad {
            out.extend(sep.isolate(&pl[i..i + CHUNK], &pr[i..i + CHUNK]).unwrap());
            i += GEN;
        }
        out.truncate(n);
        println!("isolated {:.1}s in {} ms", n as f32 / SR as f32, t0.elapsed().as_millis());
        // write it next to the input as 16 kHz mono for Whisper
        let mut rs = crate::voice::Resampler::new(16_000);
        let mut v16 = Vec::new();
        rs.push(&out, SR as u32, &mut v16);
        let o = path.replace(".wav", "-rust16.wav");
        let pcm: Vec<u8> = v16.iter().flat_map(|x| ((x.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes()).collect();
        let mut h = Vec::new();
        h.extend(b"RIFF"); h.extend(((36 + pcm.len()) as u32).to_le_bytes()); h.extend(b"WAVEfmt ");
        h.extend(16u32.to_le_bytes()); h.extend(1u16.to_le_bytes()); h.extend(1u16.to_le_bytes()); h.extend(16000u32.to_le_bytes());
        h.extend(32000u32.to_le_bytes()); h.extend(2u16.to_le_bytes()); h.extend(16u16.to_le_bytes()); h.extend(b"data"); h.extend((pcm.len() as u32).to_le_bytes());
        h.extend(pcm);
        std::fs::write(&o, h).unwrap();
        println!("wrote {o}");
    }
}
