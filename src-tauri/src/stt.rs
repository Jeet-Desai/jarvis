//! Local speech-to-text via whisper.cpp (whisper-rs) with Metal acceleration.
//!
//! - The WhisperContext is heavy to load (~140MB model, model parse + GPU
//!   buffers). We load it lazily once and keep it for the lifetime of the
//!   process behind an `OnceLock`.
//! - cpal gives us audio at the device's native rate (typically 48 kHz on
//!   macOS) and channel count. whisper expects 16 kHz **mono f32**. We do:
//!     1) downmix to mono (average channels)
//!     2) resample to 16 kHz via rubato's `SincFixedIn` (high-quality, much
//!        better than naive decimation — matters for accuracy)
//! - Transcription is blocking and runs on whatever thread calls
//!   `transcribe()`. Callers should spawn a thread to avoid blocking the
//!   audio or UI threads.

use anyhow::{anyhow, Result};
use rubato::{Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

const WHISPER_SAMPLE_RATE: u32 = 16_000;

static MODEL: OnceLock<WhisperContext> = OnceLock::new();

/// Resolve the model path. Looks for `models/ggml-base.en.bin` relative to
/// the workspace root (one level above `src-tauri/`).
fn model_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("models").join("ggml-base.en.bin"))
        .unwrap_or_else(|| PathBuf::from("models/ggml-base.en.bin"))
}

/// Load (or return cached) WhisperContext. Idempotent.
fn get_or_init() -> Result<&'static WhisperContext> {
    if let Some(ctx) = MODEL.get() {
        return Ok(ctx);
    }
    let path = model_path();
    if !path.exists() {
        return Err(anyhow!(
            "whisper model missing at {} — run the model download step",
            path.display()
        ));
    }
    tracing::info!("whisper: loading model from {}", path.display());
    let mut params = WhisperContextParameters::default();
    params.use_gpu = true; // Metal on macOS
    let ctx = WhisperContext::new_with_params(
        path.to_str().ok_or_else(|| anyhow!("non-utf8 path"))?,
        params,
    )
    .map_err(|e| anyhow!("whisper init: {e}"))?;
    let _ = MODEL.set(ctx);
    tracing::info!("whisper: model loaded");
    Ok(MODEL.get().unwrap())
}

/// Pre-warm the model (load it on a background thread at startup so the
/// first transcription doesn't pay the ~1-2s init cost).
pub fn prewarm() {
    std::thread::spawn(|| {
        if let Err(e) = get_or_init() {
            tracing::warn!("whisper prewarm failed: {e}");
        }
    });
}

/// Transcribe captured audio. `samples` is interleaved f32, range [-1, 1].
pub fn transcribe(samples: &[f32], sample_rate: u32, channels: u16) -> Result<String> {
    if samples.is_empty() {
        return Ok(String::new());
    }
    let ctx = get_or_init()?;

    // 1) interleaved → mono
    let mono = downmix_to_mono(samples, channels);

    // 2) native rate → 16 kHz
    let resampled = if sample_rate == WHISPER_SAMPLE_RATE {
        mono
    } else {
        resample_to_16k(&mono, sample_rate)?
    };

    if resampled.len() < WHISPER_SAMPLE_RATE as usize / 2 {
        // Whisper needs at least ~0.5s of audio
        return Ok(String::new());
    }

    let mut state = ctx
        .create_state()
        .map_err(|e| anyhow!("whisper state: {e}"))?;

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(num_cpus());
    params.set_language(Some("en"));
    params.set_translate(false);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_suppress_blank(true);
    params.set_no_context(true);

    let started = std::time::Instant::now();
    state
        .full(params, &resampled)
        .map_err(|e| anyhow!("whisper full: {e}"))?;
    let elapsed = started.elapsed();

    let n = state.full_n_segments().map_err(|e| anyhow!(e))?;
    let mut out = String::new();
    for i in 0..n {
        let seg = state
            .full_get_segment_text(i)
            .map_err(|e| anyhow!("segment {i}: {e}"))?;
        out.push_str(&seg);
    }
    let text = out.trim().to_string();
    tracing::info!(
        "whisper: {} samples in {:.2}s → {:?}",
        resampled.len(),
        elapsed.as_secs_f32(),
        text
    );

    // Strip whisper noise tokens + common silence-hallucinations. Returning
    // an empty string here makes the caller short-circuit back to Idle
    // instead of feeding garbage to the LLM (which would hallucinate a reply).
    if is_whisper_noise(&text) {
        tracing::info!("whisper: filtered as noise/hallucination — discarding");
        return Ok(String::new());
    }

    Ok(text)
}

/// Whisper has known failure modes on silence or low-amplitude noise:
///   - emits its special "[BLANK_AUDIO]" / "[NULL]" / "[Music]" tokens
///   - hallucinates the most common training-data phrases like
///     "Thanks for watching", "Subscribe to the channel", a lone "you", etc.
/// All of these should be treated as "nothing was said" — silently bail.
fn is_whisper_noise(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() { return true; }

    // Bracketed special tokens — Whisper emits these wholesale
    let stripped = t.trim_matches(|c: char| !c.is_alphanumeric());
    if t.starts_with('[') && t.ends_with(']') {
        return true;
    }
    if t.starts_with('(') && t.ends_with(')') {
        return true;
    }
    if t.starts_with('*') && t.ends_with('*') {
        return true;
    }

    // Common silence-hallucinations
    let lc = stripped.to_lowercase();
    matches!(lc.as_str(),
          ""
        | "you"
        | "okay"
        | "ok"
        | "uh"
        | "um"
        | "hmm"
        | "mm"
        | "mmm"
        | "thank you"
        | "thanks"
        | "thanks for watching"
        | "thanks for watching the video"
        | "thanks for watching this video"
        | "subscribe to my channel"
        | "please subscribe"
        | "like and subscribe"
        | "see you next time"
        | "bye"
        | "goodbye"
        | "music"
        | "applause"
        | "silence"
        | "background noise"
    )
}

fn downmix_to_mono(interleaved: &[f32], channels: u16) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    let ch = channels as usize;
    let frames = interleaved.len() / ch;
    let mut mono = Vec::with_capacity(frames);
    for f in 0..frames {
        let mut sum = 0.0_f32;
        for c in 0..ch {
            sum += interleaved[f * ch + c];
        }
        mono.push(sum / ch as f32);
    }
    mono
}

fn resample_to_16k(samples: &[f32], from_rate: u32) -> Result<Vec<f32>> {
    let ratio = WHISPER_SAMPLE_RATE as f64 / from_rate as f64;

    // Tuned for speech, not music — sinc length is moderate, anti-aliasing
    // tuned for the 48k→16k case in particular.
    let params = SincInterpolationParameters {
        sinc_len: 128,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 128,
        window: WindowFunction::BlackmanHarris2,
    };

    // chunk size of 1024 is a reasonable balance for typical 1-30s utterances
    let mut resampler = SincFixedIn::<f32>::new(ratio, 2.0, params, 1024, 1)
        .map_err(|e| anyhow!("rubato init: {e}"))?;

    let mut output = Vec::with_capacity((samples.len() as f64 * ratio) as usize + 4096);
    let chunk = resampler.input_frames_next();
    let mut i = 0;
    while i + chunk <= samples.len() {
        let in_chunk = vec![samples[i..i + chunk].to_vec()];
        let out_chunk = resampler
            .process(&in_chunk, None)
            .map_err(|e| anyhow!("rubato process: {e}"))?;
        output.extend(out_chunk[0].iter().copied());
        i += chunk;
    }
    // Process trailing remainder (pad with zeros to chunk size)
    if i < samples.len() {
        let mut last = samples[i..].to_vec();
        last.resize(chunk, 0.0);
        let in_chunk = vec![last];
        let out_chunk = resampler
            .process_partial(Some(&in_chunk), None)
            .map_err(|e| anyhow!("rubato partial: {e}"))?;
        output.extend(out_chunk[0].iter().copied());
    }

    Ok(output)
}

fn num_cpus() -> std::os::raw::c_int {
    std::thread::available_parallelism()
        .map(|n| n.get() as i32)
        .unwrap_or(4)
        .min(8)
}
