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

const DEFAULT_VOICE:    &str = "Daniel";
const DEFAULT_RATE_WPM: u32  = 195;

pub struct SpeechHandle {
    /// PID for SIGKILL — usable from any thread without locking
    pid: Option<u32>,
    /// Child object — locked exclusively by wait_blocking
    child: Mutex<Option<Child>>,
}

impl SpeechHandle {
    /// Block until the subprocess exits (normal end of speech) OR until
    /// `kill()` is called from another thread.
    pub fn wait_blocking(&self) -> Result<()> {
        let opt = self.child.lock().take();
        if let Some(mut c) = opt {
            let _ = c.wait()?;
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
        return Ok(SpeechHandle { pid: None, child: Mutex::new(None) });
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
    Ok(SpeechHandle { pid: Some(pid), child: Mutex::new(Some(child)) })
}

fn truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes { return s; }
    // Walk back to the nearest char boundary so we don't split a UTF-8
    // multi-byte sequence (which would panic).
    let mut i = max_bytes;
    while i > 0 && !s.is_char_boundary(i) { i -= 1; }
    &s[..i]
}
