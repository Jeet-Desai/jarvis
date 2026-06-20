//! Text-to-speech via macOS `say`.
//!
//! Returns a `SpeechHandle` that lets the caller wait OR kill the subprocess
//! independently — the kill path is what makes barge-in actually work.
//!
//! The handle stores the child's PID separately from the `Child` object.
//! That's because `Child::kill()` needs `&mut Child`, but `wait()` consumes
//! the `Child` mutex-guarded data while it blocks. If we only had the Child,
//! the kill path would lose the race against wait_blocking — kill arrives,
//! Mutex is empty (wait holds it), kill does nothing, speech runs to its
//! natural end. Storing the PID separately means `kill()` can fire a SIGKILL
//! at any time, regardless of whether wait_blocking is currently holding
//! the Child mutex.

use anyhow::Result;
use parking_lot::Mutex;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const DEFAULT_VOICE:    &str = "Daniel";
const DEFAULT_RATE_WPM: u32  = 195;

pub struct SpeechHandle {
    /// PID for SIGKILL — usable from any thread without locking
    pid: Option<u32>,
    /// Child object — locked exclusively by wait_blocking
    child: Mutex<Option<Child>>,
    /// Hard ceiling (seconds) on how long we'll wait for `say` to finish, so a
    /// wedged TTS subprocess can never hang a turn — critical now that turns
    /// chain autonomously in conversation mode.
    max_secs: u64,
}

impl SpeechHandle {
    /// Block until the subprocess exits (normal end of speech), until `kill()`
    /// is called from another thread, OR until `max_secs` elapses (then we
    /// force-kill and return). Polls so the kill paths stay responsive.
    pub fn wait_blocking(&self) -> Result<()> {
        let opt = self.child.lock().take();
        if let Some(mut c) = opt {
            let start = Instant::now();
            let max = Duration::from_secs(self.max_secs.max(1));
            loop {
                match c.try_wait()? {
                    Some(_) => break,
                    None => {
                        if start.elapsed() > max {
                            tracing::warn!("tts: exceeded {}s ceiling — killing `say`", self.max_secs);
                            let _ = c.kill();
                            let _ = c.wait();
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(40));
                    }
                }
            }
        }
        Ok(())
    }

    /// Cut speech short. Safe to call from any thread, even while
    /// wait_blocking holds the Child. SIGKILL terminates immediately so
    /// audio output stops within milliseconds.
    pub fn kill(&self) {
        if let Some(pid) = self.pid {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
            tracing::info!("tts: SIGKILL → pid {}", pid);
        }
    }
}

pub fn start_speaking(text: &str) -> Result<SpeechHandle> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(SpeechHandle { pid: None, child: Mutex::new(None), max_secs: 0 });
    }

    let voice = std::env::var("JARVIS_VOICE").unwrap_or_else(|_| DEFAULT_VOICE.into());
    let rate: u32 = std::env::var("JARVIS_VOICE_RATE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_RATE_WPM);

    tracing::info!("tts: speaking ({voice} @ {rate} wpm) — {:?}", truncate(text, 80));

    let child = Command::new("say")
        .arg("-v").arg(&voice)
        .arg("-r").arg(rate.to_string())
        .arg(text)
        .spawn()?;

    let pid = child.id();
    // Generous ceiling from length: ~16 chars/sec at default rate, so /8 gives
    // ~2× headroom, +8s slack, capped at 3 minutes.
    let max_secs = ((text.chars().count() as u64) / 8 + 8).min(180);
    Ok(SpeechHandle { pid: Some(pid), child: Mutex::new(Some(child)), max_secs })
}

fn truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes { return s; }
    // Walk back to the nearest char boundary so we don't split a UTF-8
    // multi-byte sequence (which would panic).
    let mut i = max_bytes;
    while i > 0 && !s.is_char_boundary(i) { i -= 1; }
    &s[..i]
}
