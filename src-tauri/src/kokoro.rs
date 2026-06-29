//! Kokoro neural TTS — a natural local voice that replaces macOS `say`.
//!
//! Runs `kokoro-v1.0.onnx` via `ort` (onnxruntime), phonemising text with
//! `misaki-rs` (the pure-Rust port of Kokoro's reference G2P — no native
//! espeak-ng, nothing to install). Fully offline; the model + voice pack live
//! in `models/` alongside the whisper model.
//!
//! The engine is expensive to build (ONNX session + 28 MB voice pack + G2P
//! data) so it's loaded ONCE behind a `Mutex` — `ort::Session::run` takes
//! `&mut self`, and speaking is serialised by the state machine anyway. If the
//! models are missing or init fails, `available()` reports false and the TTS
//! layer falls back to `say`, so Kokoro is a strict upgrade, never a regression.
//!
//! Model I/O (read off the file, see `kokoro` de-risk): inputs `tokens`
//! (int64 `[1,N]`), `style` (f32 `[1,256]`), `speed` (f32 `[1]`) → output
//! `audio` (f32, 24 kHz mono). NOTE the input is named `tokens`, not
//! `input_ids` — this export differs from some docs.

use anyhow::{anyhow, Context, Result};
use ndarray::Array3;
use ndarray_npy::NpzReader;
use ort::session::Session;
use ort::value::Tensor;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::kokoro_vocab::KOKORO_VOCAB;

/// Kokoro emits 24 kHz mono.
pub const SAMPLE_RATE: u32 = 24_000;
/// Default voice (British male; user-selected). Override with `JARVIS_TTS_VOICE`.
const DEFAULT_VOICE: &str = "bm_lewis";
/// Kokoro's phoneme budget (the voice pack has a style row per length 0..510).
const MAX_TOKENS: usize = 510;

struct Engine {
    session: Session,
    g2p: misaki_rs::G2P,
    vocab: HashMap<char, i64>,
    /// Selected voice's style tensor `[510, 1, 256]`; row indexed by token count.
    style: Array3<f32>,
    voice: String,
}

impl Engine {
    /// text → 24 kHz mono f32 samples. Empty if nothing phonemised.
    fn synthesize(&mut self, text: &str) -> Result<Vec<f32>> {
        // 1. text → IPA phoneme string (reference G2P)
        let (phonemes, _toks) = self.g2p.g2p(text).map_err(|e| anyhow!("g2p: {e}"))?;

        // 2. phonemes → Kokoro token ids (unmapped chars, e.g. ZWJ joiners, skipped)
        let mut ids: Vec<i64> = phonemes.chars().filter_map(|c| self.vocab.get(&c).copied()).collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        ids.truncate(MAX_TOKENS);
        let n = ids.len();

        // 3. voice style row for this token count → flat [1,256]
        let idx = n.min(509);
        let style_vec: Vec<f32> = self.style.slice(ndarray::s![idx, .., ..]).iter().copied().collect();

        // 4. pad-wrap tokens: [0, …ids, 0]
        let mut tokens = Vec::with_capacity(n + 2);
        tokens.push(0i64);
        tokens.extend_from_slice(&ids);
        tokens.push(0i64);
        let tok_len = tokens.len();

        // 5. inference (plain (shape, Vec) tensors — no ndarray-version coupling)
        let outputs = self.session.run(ort::inputs![
            "tokens" => Tensor::from_array(([1usize, tok_len], tokens))?,
            "style"  => Tensor::from_array(([1usize, 256usize], style_vec))?,
            "speed"  => Tensor::from_array(([1usize], vec![speed()]))?,
        ])?;
        let (_shape, audio) = outputs["audio"].try_extract_tensor::<f32>()?;
        Ok(audio.to_vec())
    }
}

/// `None` = engine unavailable (models missing / init failed) → caller uses `say`.
static ENGINE: OnceLock<Option<Mutex<Engine>>> = OnceLock::new();

/// The `models/` dir, one level above `src-tauri/` (mirrors `stt::models_dir`).
fn models_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("models"))
        .unwrap_or_else(|| PathBuf::from("models"))
}

/// Default speaking rate. >1 because Kokoro at 1.0 reads a touch slowly for a
/// voice assistant. Override with `JARVIS_TTS_SPEED`.
const DEFAULT_SPEED: f32 = 1.15;

/// Speaking rate multiplier (1.0 = normal). Override with `JARVIS_TTS_SPEED`.
fn speed() -> f32 {
    std::env::var("JARVIS_TTS_SPEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|s: &f32| *s > 0.1 && *s < 3.0)
        .unwrap_or(DEFAULT_SPEED)
}

/// Trim near-silence from the head and tail of a clip (Kokoro pads ~100-250ms
/// of quiet around an utterance). Tightens the gap between streamed chunks so
/// sentence transitions don't drag. Keeps a small pad so onsets aren't clipped.
fn trim_silence(samples: &[f32]) -> &[f32] {
    const THRESH: f32 = 0.008;
    let first = samples.iter().position(|s| s.abs() > THRESH);
    let last = samples.iter().rposition(|s| s.abs() > THRESH);
    match (first, last) {
        (Some(a), Some(b)) if a <= b => {
            let pad = SAMPLE_RATE as usize / 200; // ~5ms
            let s = a.saturating_sub(pad);
            let e = (b + 1 + pad).min(samples.len());
            &samples[s..e]
        }
        _ => &[], // all silence
    }
}

fn build_engine() -> Result<Engine> {
    let dir = models_dir();
    let onnx = dir.join("kokoro-v1.0.onnx");
    let voices = dir.join("voices-v1.0.bin");
    if !onnx.exists() || !voices.exists() {
        return Err(anyhow!(
            "kokoro models missing in {} (need kokoro-v1.0.onnx + voices-v1.0.bin)",
            dir.display()
        ));
    }

    let session = Session::builder()?
        .commit_from_file(&onnx)
        .with_context(|| format!("loading {}", onnx.display()))?;

    let voice = std::env::var("JARVIS_TTS_VOICE").unwrap_or_else(|_| DEFAULT_VOICE.into());
    let mut npz = NpzReader::new(std::fs::File::open(&voices)?)?;
    // np.savez stores arrays as "<name>.npy"; accept either spelling.
    let style: Array3<f32> = npz
        .by_name(&format!("{voice}.npy"))
        .or_else(|_| npz.by_name(&voice))
        .with_context(|| format!("voice '{voice}' not found in voice pack"))?;

    let g2p = misaki_rs::G2P::new(misaki_rs::Language::EnglishUS);
    let vocab: HashMap<char, i64> = KOKORO_VOCAB.iter().copied().collect();

    Ok(Engine { session, g2p, vocab, style, voice })
}

fn engine() -> Option<&'static Mutex<Engine>> {
    ENGINE
        .get_or_init(|| match build_engine() {
            Ok(e) => {
                tracing::info!("kokoro: engine ready (voice {})", e.voice);
                Some(Mutex::new(e))
            }
            Err(e) => {
                tracing::warn!("kokoro: disabled, will use `say` — {e}");
                None
            }
        })
        .as_ref()
}

/// Is Kokoro usable? Triggers (one-time) engine init if not already warmed.
pub fn available() -> bool {
    engine().is_some()
}

/// Load the engine on a background thread so the first utterance is instant.
pub fn prewarm() {
    std::thread::spawn(|| {
        let _ = engine();
    });
}

/// Synthesize `text` and write a **stereo** 24 kHz WAV (mono duplicated to L/R
/// so playback isn't single-speaker). Returns the clip duration in seconds, or
/// `None` if Kokoro is unavailable or produced no audio (→ caller falls back to
/// `say`).
pub fn synth_to_wav(text: &str, path: &Path) -> Result<Option<f32>> {
    let Some(eng) = engine() else {
        return Ok(None);
    };
    let mono = eng.lock().synthesize(text)?;
    let mono = trim_silence(&mono);
    if mono.is_empty() {
        return Ok(None);
    }
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    for s in mono {
        w.write_sample(*s)?; // L
        w.write_sample(*s)?; // R
    }
    w.finalize()?;
    Ok(Some(mono.len() as f32 / SAMPLE_RATE as f32))
}
