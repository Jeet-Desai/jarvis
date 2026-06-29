//! Text-to-speech: Kokoro neural voice (default) with macOS `say` fallback.
//!
//! Returns a `SpeechHandle` that lets the caller wait for OR kill speech — the
//! kill path is what makes barge-in work.
//!
//! ## Streaming (low time-to-first-audio)
//! Kokoro synthesis is per-utterance: synthesizing a whole multi-sentence reply
//! before any sound plays leaves an audible gap after the orb flips to Speaking.
//! So we **stream**: the reply is split into chunks (small first chunk for a
//! fast start, ~sentence-sized after), a producer thread synthesizes chunk N+1
//! while a consumer thread `afplay`s chunk N. Time-to-first-audio drops from
//! "whole reply" to "first short clause". `say` needs no streaming (it starts
//! speaking immediately) and runs as a single chunk.
//!
//! ## Kill/wait model
//! A `SpeechHandle` owns a coordinator thread plus an `Arc<SpeechInner>` shared
//! with it. `kill()` sets a cancel flag and SIGKILLs whatever child is playing
//! right now (safe from any thread, even while `wait_blocking` is parked on the
//! join handle). `wait_blocking()` joins the coordinator thread.

use anyhow::Result;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const DEFAULT_VOICE:    &str = "Daniel";
const DEFAULT_RATE_WPM: u32  = 195;

/// Replies at or below this length are spoken as ONE clip — no inter-chunk
/// gaps at all (Kokoro handles internal comma/clause pauses itself), and synth
/// is fast enough that first-audio latency stays low. Only longer replies
/// stream, where the latency win is worth a natural sentence-boundary pause.
const SINGLE_CHUNK_MAX: usize = 220;
/// Pack streamed chunks up to ~this many chars → few sentence-boundary gaps.
const CHUNK_TARGET: usize = 200;
/// Per-clip playback ceiling (s) so a wedged `afplay`/`say` can't hang a turn.
const CLIP_CEILING_SECS: u64 = 90;

/// Shared between a `SpeechHandle` and its coordinator thread.
struct SpeechInner {
    /// Set by `kill()` — stops synthesis and playback ASAP.
    cancel: AtomicBool,
    /// PID of the child playing right now, for SIGKILL from any thread.
    current_pid: Mutex<Option<u32>>,
}

impl SpeechInner {
    fn new() -> Self {
        Self { cancel: AtomicBool::new(false), current_pid: Mutex::new(None) }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Spawn a player child, register its pid, and block until it exits, until
    /// `cancel` is set, or until the ceiling elapses (poll so kill stays snappy).
    fn play(&self, mut cmd: Command, ceiling_secs: u64) {
        if self.cancelled() {
            return;
        }
        let child: Child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("tts: spawn failed: {e}");
                return;
            }
        };
        *self.current_pid.lock() = Some(child.id());
        let mut child = child;
        let start = Instant::now();
        let max = Duration::from_secs(ceiling_secs.max(1));
        loop {
            if self.cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if start.elapsed() > max {
                        tracing::warn!("tts: clip exceeded {ceiling_secs}s — killing");
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break,
            }
        }
        *self.current_pid.lock() = None;
    }
}

pub struct SpeechHandle {
    inner: Arc<SpeechInner>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl SpeechHandle {
    fn empty() -> Self {
        Self { inner: Arc::new(SpeechInner::new()), join: Mutex::new(None) }
    }

    fn spawn(inner: Arc<SpeechInner>, join: JoinHandle<()>) -> Self {
        Self { inner, join: Mutex::new(Some(join)) }
    }

    /// Block until speech finishes naturally OR is killed from another thread.
    pub fn wait_blocking(&self) -> Result<()> {
        let j = self.join.lock().take();
        if let Some(j) = j {
            let _ = j.join();
        }
        Ok(())
    }

    /// Cut speech short. Safe from any thread, even while `wait_blocking` is
    /// parked: SIGKILL stops the playing child within milliseconds and the
    /// cancel flag stops any further chunks/synthesis.
    pub fn kill(&self) {
        self.inner.cancel.store(true, Ordering::Relaxed);
        if let Some(pid) = *self.inner.current_pid.lock() {
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            tracing::info!("tts: SIGKILL → pid {}", pid);
        }
    }
}

pub fn start_speaking(text: &str) -> Result<SpeechHandle> {
    // Normalise Markdown/URLs/emoji into clean speakable text. This is the
    // single choke point for every audio path (replies, confirmations,
    // proactive blurbs); the HUD keeps the raw text it was given separately.
    let spoken = crate::speakable::to_speech(text);
    let text = spoken.trim();
    if text.is_empty() {
        return Ok(SpeechHandle::empty());
    }

    // Prefer Kokoro neural TTS unless explicitly disabled (JARVIS_TTS=say) or
    // the engine is unavailable (models missing / init failed).
    if use_kokoro() {
        if let Some(handle) = speak_kokoro_streaming(text) {
            return Ok(handle);
        }
    }

    Ok(speak_say(text))
}

/// Whether to use Kokoro for this utterance: default yes, unless `JARVIS_TTS=say`
/// or the engine couldn't initialise.
fn use_kokoro() -> bool {
    if std::env::var("JARVIS_TTS").map(|v| v.eq_ignore_ascii_case("say")).unwrap_or(false) {
        return false;
    }
    crate::kokoro::available()
}

/// Stream a Kokoro reply: a producer thread synthesizes one chunk ahead (each to
/// a stereo WAV) while the coordinator thread `afplay`s chunks in order. Returns
/// `None` if Kokoro isn't available (caller falls back to `say`).
fn speak_kokoro_streaming(text: &str) -> Option<SpeechHandle> {
    if !crate::kokoro::available() {
        return None;
    }
    let chunks = split_into_chunks(text);
    if chunks.is_empty() {
        return None;
    }
    tracing::info!(
        "tts: speaking (kokoro, {} chunk(s)) — {:?}",
        chunks.len(),
        truncate(text, 80)
    );

    let inner = Arc::new(SpeechInner::new());
    let inner_co = inner.clone();
    let join = thread::spawn(move || {
        // Synth-ahead-by-one: buffer of 1 keeps the producer at most one chunk
        // in front of playback (bounded disk/memory, hides synth latency).
        let (tx, rx) = sync_channel::<PathBuf>(1);
        let inner_prod = inner_co.clone();
        let producer = thread::spawn(move || {
            for (i, chunk) in chunks.iter().enumerate() {
                if inner_prod.cancelled() {
                    break;
                }
                let path = chunk_path(i);
                match crate::kokoro::synth_to_wav(chunk, &path) {
                    Ok(Some(_dur)) => {
                        if tx.send(path).is_err() {
                            break; // consumer gone
                        }
                    }
                    Ok(None) => {} // empty chunk — skip
                    Err(e) => tracing::warn!("tts: kokoro chunk {i} synth failed: {e}"),
                }
            }
            // tx dropped here → consumer's recv disconnects
        });

        // Consumer: play ready chunks in order, polling cancel between them.
        loop {
            if inner_co.cancelled() {
                break;
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(path) => {
                    let mut cmd = Command::new("afplay");
                    cmd.arg(&path);
                    inner_co.play(cmd, CLIP_CEILING_SECS);
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        drop(rx); // unblock a producer parked on a full send before we join
        let _ = producer.join();
    });

    Some(SpeechHandle::spawn(inner, join))
}

/// macOS `say` — the fallback voice. Single chunk (say streams its own audio).
fn speak_say(text: &str) -> SpeechHandle {
    let voice = std::env::var("JARVIS_VOICE").unwrap_or_else(|_| DEFAULT_VOICE.into());
    let rate: u32 = std::env::var("JARVIS_VOICE_RATE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_RATE_WPM);

    tracing::info!("tts: speaking (say {voice} @ {rate} wpm) — {:?}", truncate(text, 80));

    // Generous ceiling from length: ~16 chars/sec, so /8 gives ~2× headroom.
    let ceiling = ((text.chars().count() as u64) / 8 + 8).min(180);
    let mut cmd = Command::new("say");
    cmd.arg("-v").arg(&voice).arg("-r").arg(rate.to_string()).arg(text);

    let inner = Arc::new(SpeechInner::new());
    let inner_co = inner.clone();
    let join = thread::spawn(move || inner_co.play(cmd, ceiling));
    SpeechHandle::spawn(inner, join)
}

/// Split a reply into synthesis chunks. Short replies → one clip (smooth, no
/// gaps). Longer replies stream by **whole sentence** (never mid-sentence), so
/// the only inter-clip gaps fall at natural sentence pauses — NEVER at commas
/// (which Kokoro renders with its own short prosodic pause within a clip).
fn split_into_chunks(text: &str) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    if text.chars().count() <= SINGLE_CHUNK_MAX {
        return vec![text.to_string()];
    }

    let sentences = split_sentences(text);
    if sentences.is_empty() {
        return vec![text.to_string()];
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut buf = String::new();
    for s in &sentences {
        if !buf.is_empty() {
            buf.push(' ');
        }
        buf.push_str(s);
        // First chunk = the opening sentence (fast first audio); later chunks
        // pack whole sentences up to ~CHUNK_TARGET to minimise gaps.
        let flush = if chunks.is_empty() { true } else { buf.chars().count() >= CHUNK_TARGET };
        if flush {
            chunks.push(std::mem::take(&mut buf));
        }
    }
    if !buf.is_empty() {
        chunks.push(buf);
    }
    chunks
}

/// Break text at SENTENCE boundaries (. ! ? newline) followed by whitespace/end,
/// keeping the punctuation attached. Deliberately does NOT break on commas /
/// semicolons / colons — those stay inside a clip so they're spoken as natural
/// short pauses, not large between-clip gaps.
fn split_sentences(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        cur.push(c);
        let boundary = matches!(c, '.' | '!' | '?' | '\n');
        let next_ws = chars.get(i + 1).map_or(true, |n| n.is_whitespace());
        if boundary && next_ws {
            let t = cur.trim();
            if !t.is_empty() {
                out.push(t.to_string());
            }
            cur.clear();
        }
    }
    let t = cur.trim();
    if !t.is_empty() {
        out.push(t.to_string());
    }
    out
}

/// Per-chunk temp WAV. Speaking is serialised by the state machine, so reusing
/// `<pid>-<chunk>` names across utterances is race-free.
fn chunk_path(i: usize) -> PathBuf {
    std::env::temp_dir().join(format!("jarvis-tts-{}-{}.wav", std::process::id(), i))
}

fn truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes { return s; }
    // Walk back to the nearest char boundary so we don't split a UTF-8
    // multi-byte sequence (which would panic).
    let mut i = max_bytes;
    while i > 0 && !s.is_char_boundary(i) { i -= 1; }
    &s[..i]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_reply_is_one_smooth_clip() {
        // A comma-heavy short reply must NOT split — commas stay inside one clip.
        let text = "Yes, of course, I can help you with that, right away.";
        assert!(text.chars().count() <= SINGLE_CHUNK_MAX);
        assert_eq!(split_into_chunks(text), vec![text.to_string()]);
    }

    #[test]
    fn long_reply_splits_only_at_sentences() {
        let text = "The weather today is quite pleasant, sunny and warm, with a gentle breeze. \
                    Your three o'clock meeting has been moved, at the organiser's request, to four. \
                    Finally, the report you asked about is finished, reviewed, and ready for you now.";
        assert!(text.chars().count() > SINGLE_CHUNK_MAX);
        let chunks = split_into_chunks(text);
        assert!(chunks.len() >= 2, "long reply should stream: {chunks:?}");
        // Every chunk but the last ends at a sentence boundary — never on a comma.
        for c in &chunks[..chunks.len() - 1] {
            let last = c.trim().chars().last().unwrap();
            assert!(matches!(last, '.' | '!' | '?'), "chunk must end a sentence: {c:?}");
        }
        // First chunk is the opening sentence (fast start), commas intact.
        assert!(chunks[0].starts_with("The weather today"));
        assert!(chunks[0].contains("gentle breeze."));
        assert!(chunks.join(" ").contains("ready for you now"));
    }

    #[test]
    fn empty_text_no_chunks() {
        assert!(split_into_chunks("   ").is_empty());
    }
}
