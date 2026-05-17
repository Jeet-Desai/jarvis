//! Persistent mic pipe.
//!
//! Why "persistent": with the upcoming wake-word detector we can't keep
//! starting/stopping streams per utterance — Porcupine needs a continuously-
//! running audio source. So the cpal Stream now opens at app launch and stays
//! alive until app exit. The state machine just flips an `is_recording`
//! AtomicBool: when true, the callback also appends samples into a shared
//! buffer; when false, samples are discarded but level + VAD still run.
//!
//! Threading: the cpal Stream is !Send on macOS (CoreAudio callback runs on
//! a CFRunLoop), so it lives on a dedicated thread that owns it via RAII.
//! All sharing with the rest of the app is through `Arc<PipeInner>`.

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use tauri::{AppHandle, Emitter};

/// How many seconds of mic audio to keep in the rolling buffer for wake-word
/// detection. Whisper handles ≥0.5s clips well; 2s gives reliable headroom
/// for short phrases like "Hey Jarvis".
const ROLLING_WINDOW_SECS: f32 = 2.0;

// VAD tuning.
//
// Algorithm: exponential-moving-average smoothing of RMS, then a tiny state
// machine: Silent → Speaking (when smoothed RMS crosses voice threshold for
// at least MIN_SPEECH_MS cumulative) → Ended (when smoothed RMS stays under
// the threshold for SILENCE_TO_STOP_MS continuous). Plus a hard
// MAX_UTTERANCE_MS ceiling so a stuck mic never leaves the user waiting.
//
// Removed the old two-threshold deadband — it could freeze the silence
// timer if room ambient sat between the two thresholds.
//
// Industry refs: ChatGPT Voice ~500-700ms silence-to-stop, Gemini Live 700ms
// default. We use 700ms with EMA smoothing for stability.
const VOICE_RMS_THRESHOLD: f32 = 0.018;   // smoothed RMS above this = speech
const EMA_ALPHA:           f32 = 0.3;     // higher = react faster to changes
const MIN_SPEECH_MS:       u64 = 250;     // total speech needed before VAD may fire
const SILENCE_TO_STOP_MS:  u64 = 700;     // continuous silence after speech → end
const MAX_UTTERANCE_MS:    u64 = 30_000;  // hard ceiling — force-end if recording runs this long

// Barge-in tuning. We do "AEC lite": for the first GRACE_MS of speech,
// triggering is suppressed AND we sample the ambient RMS (which is
// dominated by Jarvis's own TTS leaking back via the speakers). After
// that, the trigger threshold becomes `baseline + MARGIN` — so the user
// must speak meaningfully louder than Jarvis's own playback. This prevents
// Jarvis from interrupting itself while keeping real interruptions
// responsive after ~1.2s.
const BARGE_IN_GRACE_MS:     u64 = 1200; // suppress trigger for first N ms
const BARGE_IN_BASELINE_MS:  u64 = 1000; // sample baseline RMS during first M ms
const BARGE_IN_MIN_FRAMES:   u32 = 5;    // ≈50-100 ms sustained above threshold
const BARGE_IN_MIN_THRESH:   f32 = 0.07; // floor if baseline is very low
const BARGE_IN_MARGIN:       f32 = 0.04; // user must exceed baseline by this

pub struct AudioPipe {
    inner: Arc<PipeInner>,
    _shutdown_tx: mpsc::Sender<()>,
    _thread: JoinHandle<()>,
    pub sample_rate: u32,
    pub channels: u16,
}

struct PipeInner {
    is_recording: AtomicBool,
    /// True while Jarvis is speaking aloud. While true, the callback runs
    /// the barge-in detector and fires `jarvis:barge_in` if the user
    /// starts talking over Jarvis.
    is_speaking_mode: AtomicBool,
    buffer:       Mutex<Vec<f32>>,
    vad:          Mutex<VadTracker>,
    barge_in:     Mutex<BargeInDetector>,
    /// Always-on ring buffer of the last ~ROLLING_WINDOW_SECS of audio.
    /// Used by the wake-word watcher; cleared on state transitions to
    /// avoid re-triggering on Jarvis's own voice.
    rolling:      Mutex<VecDeque<f32>>,
    rolling_max:  std::sync::OnceLock<usize>,
}

#[derive(Default)]
struct BargeInDetector {
    /// When speaking began. None when not in speaking mode.
    started_at: Option<std::time::Instant>,
    /// Rolling sum + count during the BASELINE_MS window.
    baseline_sum:   f32,
    baseline_count: u32,
    /// Computed once after BASELINE_MS — average RMS of Jarvis's own
    /// audio as observed by the mic.
    baseline:          f32,
    baseline_computed: bool,
    above_count: u32,
    triggered:   bool,
}

impl BargeInDetector {
    /// Called when Jarvis starts speaking. Resets all state.
    fn start(&mut self) {
        self.started_at        = Some(std::time::Instant::now());
        self.baseline_sum      = 0.0;
        self.baseline_count    = 0;
        self.baseline          = 0.0;
        self.baseline_computed = false;
        self.above_count       = 0;
        self.triggered         = false;
    }

    /// Called when Jarvis stops speaking (naturally or interrupted).
    fn stop(&mut self) {
        self.started_at = None;
    }

    /// Returns true exactly once when a real barge-in is detected.
    fn process(&mut self, rms: f32) -> bool {
        if self.triggered { return false; }
        let Some(started) = self.started_at else { return false; };
        let elapsed_ms = started.elapsed().as_millis() as u64;

        // Phase 1: while in the baseline window, accumulate RMS samples
        if elapsed_ms < BARGE_IN_BASELINE_MS {
            self.baseline_sum   += rms;
            self.baseline_count += 1;
        }

        // Phase 2: don't allow any triggering until grace period is over
        if elapsed_ms < BARGE_IN_GRACE_MS {
            return false;
        }

        // Finalize baseline + threshold once
        if !self.baseline_computed {
            if self.baseline_count > 0 {
                self.baseline = self.baseline_sum / self.baseline_count as f32;
            }
            self.baseline_computed = true;
            let thr = (self.baseline + BARGE_IN_MARGIN).max(BARGE_IN_MIN_THRESH);
            tracing::info!(
                "barge-in: armed (baseline={:.3}, threshold={:.3})",
                self.baseline, thr
            );
        }

        let threshold = (self.baseline + BARGE_IN_MARGIN).max(BARGE_IN_MIN_THRESH);

        if rms > threshold {
            self.above_count += 1;
            if self.above_count >= BARGE_IN_MIN_FRAMES {
                self.triggered = true;
                return true;
            }
        } else {
            self.above_count = 0;
        }
        false
    }
}

/// Voice-activity detector with EMA smoothing + a 3-state machine.
/// One threshold (no deadband). Tracks cumulative speech ms and continuous
/// silence ms separately so a brief mid-utterance breath doesn't reset speech,
/// and ambient noise can't freeze silence detection.
#[derive(Default)]
struct VadTracker {
    smoothed_rms:  f32,
    started_at:    Option<std::time::Instant>,
    speech_ms:     u64,                    // cumulative voiced duration this utterance
    last_voice_at: Option<std::time::Instant>,
    last_sample_at: Option<std::time::Instant>,
    triggered:     bool,
}

impl VadTracker {
    fn reset(&mut self) {
        self.smoothed_rms   = 0.0;
        self.started_at     = None;
        self.speech_ms      = 0;
        self.last_voice_at  = None;
        self.last_sample_at = None;
        self.triggered      = false;
    }

    fn process(&mut self, rms: f32) -> bool {
        if self.triggered { return false; }
        let now = std::time::Instant::now();

        // First-sample bootstrap
        if self.started_at.is_none() {
            self.started_at     = Some(now);
            self.last_sample_at = Some(now);
            self.smoothed_rms   = rms;
            return false;
        }

        // EMA smoothing — flattens noise spikes that would otherwise reset speech
        self.smoothed_rms = self.smoothed_rms * (1.0 - EMA_ALPHA) + rms * EMA_ALPHA;

        let dt_ms = self.last_sample_at
            .map(|t| now.duration_since(t).as_millis() as u64)
            .unwrap_or(0);
        self.last_sample_at = Some(now);

        let is_voice = self.smoothed_rms > VOICE_RMS_THRESHOLD;
        if is_voice {
            self.speech_ms = self.speech_ms.saturating_add(dt_ms);
            self.last_voice_at = Some(now);
        }

        // Hard timeout — force-end no matter what so user never gets stuck
        if let Some(start) = self.started_at {
            if now.duration_since(start).as_millis() as u64 >= MAX_UTTERANCE_MS {
                tracing::warn!("vad: hit MAX_UTTERANCE_MS, forcing end");
                self.triggered = true;
                return true;
            }
        }

        // End-of-utterance: enough cumulative speech, then enough continuous silence
        if self.speech_ms >= MIN_SPEECH_MS {
            if let Some(last_v) = self.last_voice_at {
                let silence_ms = now.duration_since(last_v).as_millis() as u64;
                if !is_voice && silence_ms >= SILENCE_TO_STOP_MS {
                    self.triggered = true;
                    return true;
                }
            }
        }
        false
    }
}

#[derive(Debug)]
pub struct RecordingResult {
    pub samples:     Vec<f32>,
    pub sample_rate: u32,
    pub channels:    u16,
}

/// Open the default input device and run a persistent capture stream.
/// Returns once the stream is up and producing samples, or errors out.
pub fn start_pipe(app: AppHandle) -> Result<AudioPipe> {
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(u32, u16), String>>();

    let inner = Arc::new(PipeInner {
        is_recording:     AtomicBool::new(false),
        is_speaking_mode: AtomicBool::new(false),
        buffer:           Mutex::new(Vec::with_capacity(48_000 * 30)),
        vad:              Mutex::new(VadTracker::default()),
        barge_in:         Mutex::new(BargeInDetector::default()),
        rolling:          Mutex::new(VecDeque::with_capacity(48_000 * 3)),
        rolling_max:      std::sync::OnceLock::new(),
    });
    let inner_for_thread = inner.clone();

    let thread = std::thread::spawn(move || {
        let host = cpal::default_host();
        let device = match host.default_input_device() {
            Some(d) => d,
            None => { let _ = ready_tx.send(Err("no default input device".into())); return; }
        };
        let supported = match device.default_input_config() {
            Ok(c) => c,
            Err(e) => { let _ = ready_tx.send(Err(format!("no input config: {e}"))); return; }
        };

        let sample_rate   = supported.sample_rate().0;
        let channels      = supported.channels();
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();

        tracing::info!(
            "mic (persistent): {} {} Hz, {} ch, {:?}",
            device.name().unwrap_or_else(|_| "(unknown)".into()),
            sample_rate, channels, sample_format,
        );

        let err_fn = |err| tracing::warn!("audio stream error: {err}");

        // Lock in the rolling-buffer cap now that we know the device rate
        let rolling_max = (sample_rate as f32 * ROLLING_WINDOW_SECS) as usize
            * (channels as usize).max(1);
        let _ = inner_for_thread.rolling_max.set(rolling_max);

        macro_rules! handle_batch {
            ($lvl:expr, $append:expr) => {{
                let lvl: f32 = $lvl;
                let _ = app.emit("jarvis:level", lvl);

                // Always push into the rolling buffer (for wake detection)
                {
                    let mut roll = inner_for_thread.rolling.lock();
                    for s in $append { roll.push_back(s); }
                    while roll.len() > rolling_max {
                        roll.pop_front();
                    }
                }

                // Buffer for active recording if state machine asked
                let recording = inner_for_thread.is_recording.load(Ordering::Acquire);
                if recording {
                    inner_for_thread.buffer.lock().extend($append);
                    if inner_for_thread.vad.lock().process(lvl) {
                        tracing::info!("vad: end-of-utterance");
                        let _ = app.emit("jarvis:vad_stop", ());
                    }
                }

                // Barge-in: only while Jarvis is speaking aloud
                if inner_for_thread.is_speaking_mode.load(Ordering::Acquire) {
                    if inner_for_thread.barge_in.lock().process(lvl) {
                        tracing::info!("barge-in: user interrupted (level={:.3})", lvl);
                        let _ = app.emit("jarvis:barge_in", ());
                    }
                }
            }};
        }

        let stream_result = match sample_format {
            cpal::SampleFormat::F32 => device.build_input_stream(
                &config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    handle_batch!(rms_f32(data), data.iter().copied());
                },
                err_fn,
                None,
            ).map_err(|e| format!("build f32: {e}")),
            cpal::SampleFormat::I16 => device.build_input_stream(
                &config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    handle_batch!(
                        rms_i16(data),
                        data.iter().map(|&x| x as f32 / i16::MAX as f32)
                    );
                },
                err_fn,
                None,
            ).map_err(|e| format!("build i16: {e}")),
            cpal::SampleFormat::U16 => device.build_input_stream(
                &config,
                move |data: &[u16], _: &cpal::InputCallbackInfo| {
                    handle_batch!(
                        rms_u16(data),
                        data.iter().map(|&x| (x as f32 - 32768.0) / 32768.0)
                    );
                },
                err_fn,
                None,
            ).map_err(|e| format!("build u16: {e}")),
            other => Err(format!("unsupported sample format: {other:?}")),
        };

        let stream = match stream_result {
            Ok(s) => s,
            Err(e) => { let _ = ready_tx.send(Err(e)); return; }
        };

        if let Err(e) = stream.play() {
            let _ = ready_tx.send(Err(format!("stream play: {e}")));
            return;
        }

        let _ = ready_tx.send(Ok((sample_rate, channels)));

        // Block until app shutdown — keeps the stream alive
        let _ = shutdown_rx.recv();
        drop(stream);
        tracing::info!("audio pipe: shut down");
    });

    match ready_rx.recv() {
        Ok(Ok((sample_rate, channels))) => Ok(AudioPipe {
            inner,
            _shutdown_tx: shutdown_tx,
            _thread: thread,
            sample_rate,
            channels,
        }),
        Ok(Err(e)) => Err(anyhow!("audio init: {e}")),
        Err(e)     => Err(anyhow!("audio thread didn't report: {e}")),
    }
}

impl AudioPipe {
    pub fn begin_recording(&self) {
        self.inner.buffer.lock().clear();
        self.inner.vad.lock().reset();
        self.inner.is_recording.store(true, Ordering::Release);
        tracing::info!("mic: recording begin");
    }

    pub fn end_recording(&self) -> RecordingResult {
        self.inner.is_recording.store(false, Ordering::Release);
        self.inner.vad.lock().reset();
        let samples = std::mem::take(&mut *self.inner.buffer.lock());
        let secs = samples.len() as f32
            / (self.sample_rate.max(1) as f32 * self.channels.max(1) as f32);
        tracing::info!("mic: recording end — {} samples ({:.2}s)", samples.len(), secs);
        RecordingResult {
            samples,
            sample_rate: self.sample_rate,
            channels:    self.channels,
        }
    }

    /// Copy out the current rolling buffer (last ~ROLLING_WINDOW_SECS of audio).
    /// Used by the wake-word watcher.
    pub fn snapshot_rolling(&self) -> Vec<f32> {
        self.inner.rolling.lock().iter().copied().collect()
    }

    /// Wipe the rolling buffer. Called on state transitions so the wake
    /// watcher doesn't re-trigger on Jarvis's own voice or on already-handled
    /// utterances.
    pub fn clear_rolling(&self) {
        self.inner.rolling.lock().clear();
    }

    /// Arm the barge-in detector. Call when Jarvis starts speaking.
    pub fn start_speaking_mode(&self) {
        self.inner.barge_in.lock().start();
        self.inner.is_speaking_mode.store(true, Ordering::Release);
    }

    /// Disarm the barge-in detector. Call when Jarvis stops speaking
    /// (whether naturally or because the user interrupted).
    pub fn stop_speaking_mode(&self) {
        self.inner.is_speaking_mode.store(false, Ordering::Release);
        self.inner.barge_in.lock().stop();
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

fn rms_f32(data: &[f32]) -> f32 {
    if data.is_empty() { return 0.0; }
    let sum: f32 = data.iter().map(|x| x * x).sum();
    (sum / data.len() as f32).sqrt()
}

fn rms_i16(data: &[i16]) -> f32 {
    if data.is_empty() { return 0.0; }
    let sum: f64 = data.iter().map(|&x| { let f = x as f64 / i16::MAX as f64; f * f }).sum();
    (sum / data.len() as f64).sqrt() as f32
}

fn rms_u16(data: &[u16]) -> f32 {
    if data.is_empty() { return 0.0; }
    let sum: f64 = data.iter().map(|&x| {
        let f = (x as f64 - 32768.0) / 32768.0; f * f
    }).sum();
    (sum / data.len() as f64).sqrt() as f32
}

pub fn write_wav(result: &RecordingResult, path: &std::path::Path) -> Result<()> {
    let spec = hound::WavSpec {
        channels:        result.channels,
        sample_rate:     result.sample_rate,
        bits_per_sample: 16,
        sample_format:   hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for &sample in &result.samples {
        let clamped = sample.clamp(-1.0, 1.0);
        writer.write_sample((clamped * i16::MAX as f32) as i16)?;
    }
    writer.finalize()?;
    Ok(())
}
