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
            let mut consecutive_errs: u32 = 0;

            loop {
                std::thread::sleep(Duration::from_millis(STEP_MS));

                if app_state.current() != JarvisState::Idle {
                    last_action = Instant::now();
                    continue;
                }
                // NOTE: we deliberately do NOT also skip on `session_active()`.
                // During a live conversation the state is Listening/Thinking/
                // Speaking (never Idle), so the check above already covers it.
                // The only way to be Idle *with* a session still active is the
                // old stuck-session bug — and `AppState::end_turn` now clears
                // the session on every dead-end before returning to Idle, so
                // that state shouldn't occur. Were it to occur anyway, skipping
                // here would wedge the wake word; refusing to skip self-heals.
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
                    Ok(t) => {
                        consecutive_errs = 0;
                        t
                    }
                    Err(e) => {
                        consecutive_errs = consecutive_errs.saturating_add(1);
                        if consecutive_errs <= 3 || consecutive_errs % 10 == 0 {
                            tracing::warn!("wake: stt error (#{consecutive_errs}): {e}");
                        }
                        // Back off so a persistent failure doesn't re-run STT
                        // every STEP_MS and peg the CPU.
                        let backoff = (consecutive_errs.min(10) as u64) * 300;
                        std::thread::sleep(Duration::from_millis(backoff));
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

/// Word-boundary match for "jarvis" plus Whisper mishearings.
///
/// Whisper (especially the CPU `base.en` build on accented speech) mangles
/// the bare name in creative ways — observed in the wild: "Jardvis",
/// "Jardivis", "Jarvise", "Jervis". A hardcoded variant list can't keep up,
/// so we FUZZY-match: any word that starts with j/y/g, is 5–9 letters, and
/// is within edit distance 2 of "jarvis" counts. The first-letter + length
/// guards keep everyday words (davis, juries, jarred, genius…) at distance
/// ≥3 or filtered, so false positives stay rare. Adjacent word pairs are
/// also joined ("jar vis" → "jarvis") since Whisper sometimes splits it.
fn contains_wake_phrase(transcript: &str) -> bool {
    let cleaned: String = transcript
        .chars()
        .map(|c| if c.is_alphanumeric() || c.is_whitespace() { c } else { ' ' })
        .collect();
    let lower = cleaned.to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    if words.iter().any(|w| is_wakeish(w)) {
        return true;
    }
    // Split-word rescue: "jar vis", "jarvi s". Joined pairs get a TIGHTER
    // cap (distance ≤1): a real split reassembles to distance 0, while
    // accidental joins of common words ("jury is" → "juryis") land at 2.
    words.windows(2).any(|pair| {
        let joined = format!("{}{}", pair[0], pair[1]);
        let n = joined.chars().count();
        (5..=8).contains(&n)
            && matches!(joined.chars().next(), Some('j') | Some('y') | Some('g'))
            && levenshtein_capped(&joined, "jarvis", 1) <= 1
    })
}

/// Is this (lowercase) word plausibly "jarvis" as misheard by Whisper?
fn is_wakeish(w: &str) -> bool {
    let n = w.chars().count();
    if !(5..=9).contains(&n) {
        return false;
    }
    if !matches!(w.chars().next(), Some('j') | Some('y') | Some('g')) {
        return false;
    }
    levenshtein_capped(w, "jarvis", 2) <= 2
}

/// Levenshtein distance, early-exiting once it must exceed `cap` (keeps the
/// per-word cost trivial for the 600ms wake loop).
fn levenshtein_capped(a: &str, b: &str, cap: usize) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > cap {
        return cap + 1;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=b.len() {
            let sub = prev[j - 1] + usize::from(a[i - 1] != b[j - 1]);
            cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
            row_min = row_min.min(cur[j]);
        }
        if row_min > cap {
            return cap + 1; // whole row already over the cap — can't recover
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::contains_wake_phrase;

    #[test]
    fn exact_and_listed_variants_wake() {
        for t in ["Jarvis", "jarvis,", "Hey Jarvis!", "JARVIS.", "Jervis", "yarvis"] {
            assert!(contains_wake_phrase(t), "should wake on {t:?}");
        }
    }

    #[test]
    fn observed_cpu_mishearings_wake() {
        // Real transcripts from the Windows base.en run.
        for t in ["Jardvis,", "Jardivis", "Jarvise.", "jarviz", "Jarves"] {
            assert!(contains_wake_phrase(t), "should wake on {t:?}");
        }
    }

    #[test]
    fn split_word_wakes() {
        assert!(contains_wake_phrase("jar vis"));
    }

    #[test]
    fn everyday_speech_does_not_wake() {
        for t in [
            "Service,", "and", "How are you?", "Audio, Woody.", "[BLANK_AUDIO]",
            "Davis is here", "the jury is out", "he jarred the door", "pure genius",
            "Jada", "javelin throw", "jealous much", "just visiting",
        ] {
            assert!(!contains_wake_phrase(t), "false wake on {t:?}");
        }
    }
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
