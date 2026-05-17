//! "Hey Jarvis" wake-word detection — whisper-based.
//!
//! We reuse the whisper model that's already loaded for STT. A background
//! thread wakes up every `STEP_MS`, snapshots the audio pipe's rolling
//! buffer, and runs whisper on it. If the transcript contains "jarvis" as
//! a standalone word, we transition the state machine to Listening — which
//! starts a fresh recording for the actual command.
//!
//! Why not Picovoice: that path needs an account signup, which is
//! exactly the kind of install-friction we're avoiding for the public
//! release of this project. Whisper is already paid for in CPU terms.
//!
//! Skip conditions:
//!   - State is not Idle (don't wake-trigger mid-conversation)
//!   - Cooldown active (avoid double-trigger / self-trigger on TTS audio)
//!   - Rolling buffer too quiet (no point running STT on silence)

use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::AppHandle;

use crate::state::{AppState, JarvisState};
use crate::stt;

/// How often we run a wake-word check. Lower = more responsive, higher = less
/// CPU. 600ms means at most ~0.6s latency before "Jarvis" wakes the assistant.
const STEP_MS: u64 = 600;

/// Don't run another check within this window of the last trigger (or of any
/// state change away from Idle). Suppresses re-triggering on Jarvis's own
/// audible reply leaking into the mic.
const COOLDOWN_MS: u64 = 1500;

/// Skip the wake check when the rolling window's average RMS is below this
/// (i.e. nothing meaningful was said). Saves the whisper call entirely.
const MIN_AUDIO_LEVEL: f32 = 0.006;

pub fn start_watcher(app: AppHandle, app_state: Arc<AppState>) {
    std::thread::Builder::new()
        .name("wake-watcher".into())
        .spawn(move || {
            tracing::info!("wake: watcher started — listening for 'jarvis' phrases");
            let mut last_action = Instant::now() - Duration::from_secs(60);

            loop {
                std::thread::sleep(Duration::from_millis(STEP_MS));

                if app_state.current() != JarvisState::Idle {
                    last_action = Instant::now();
                    continue;
                }
                // Don't wake-check during an active conversation session — the
                // mic is being used by the regular Listening flow, no need for
                // a wake word.
                if app_state.session_active() {
                    last_action = Instant::now();
                    continue;
                }
                if last_action.elapsed() < Duration::from_millis(COOLDOWN_MS) {
                    continue;
                }

                let Some(pipe) = app_state.pipe() else { continue };
                let snapshot = pipe.snapshot_rolling();
                if snapshot.is_empty() {
                    continue;
                }

                if rms(&snapshot) < MIN_AUDIO_LEVEL {
                    continue;
                }

                let text = match stt::transcribe(&snapshot, pipe.sample_rate, pipe.channels) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::debug!("wake: stt error: {e}");
                        continue;
                    }
                };

                if contains_wake_phrase(&text) {
                    tracing::info!("wake: detected → {:?} — starting conversation", text);
                    last_action = Instant::now();
                    // Clear so the wake phrase audio isn't re-fed into the
                    // recording buffer that begins on Listening transition.
                    pipe.clear_rolling();
                    // Conversation mode ON — subsequent turns won't need
                    // the wake word, just speak naturally.
                    app_state.start_session();
                    app_state.set(&app, JarvisState::Listening);
                }
            }
        })
        .expect("wake watcher spawn");
}

/// Word-boundary match for "jarvis" plus common Whisper mishearings.
/// Whisper occasionally drops or alters the name when only the single word
/// "Jarvis" is uttered (no "hey" prefix to anchor context). Accepting a few
/// near-matches makes the wake word feel more reliable in practice without
/// causing false positives in normal speech.
fn contains_wake_phrase(transcript: &str) -> bool {
    let cleaned: String = transcript
        .chars()
        .map(|c| if c.is_alphanumeric() || c.is_whitespace() { c } else { ' ' })
        .collect();
    let lower = cleaned.to_lowercase();
    lower.split_whitespace().any(|w| {
        matches!(w,
              "jarvis"
            | "jarvise"
            | "jarviz"
            | "jarvice"
            | "jarves"
            | "jarvas"
            | "javis"
            | "javris"
            | "jervis"
            | "jervice"
            | "yarvis"
            | "garvis"
        )
    })
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() { return 0.0; }
    // Sample a subset for speed — 96k samples × abs+sq is a lot per second
    let stride = (samples.len() / 4096).max(1);
    let mut sum = 0.0_f32;
    let mut count = 0;
    let mut i = 0;
    while i < samples.len() {
        let s = samples[i];
        sum += s * s;
        count += 1;
        i += stride;
    }
    if count == 0 { 0.0 } else { (sum / count as f32).sqrt() }
}
