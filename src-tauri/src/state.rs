use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tauri::{AppHandle, Emitter, Manager};

use crate::audio::{self, AudioPipe};
use crate::brain::{self, Brain, Message};
use crate::stt;
use crate::tts::{self, SpeechHandle};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JarvisState {
    Idle,
    Listening,
    Thinking,
    Speaking,
    Error,
}

pub struct AppState {
    pub state:        RwLock<JarvisState>,
    /// Normalized conversation history — works across any provider.
    pub conversation: Mutex<Vec<Message>>,
    /// Always-on mic. Set during startup.
    pipe:       OnceLock<Arc<AudioPipe>>,
    /// Pluggable LLM brain. Built from env at startup.
    brain:      OnceLock<Arc<Brain>>,
    /// User-controlled orb brightness multiplier.
    brightness: Mutex<f32>,
    /// Currently-playing TTS subprocess handle, so barge-in can kill it.
    speech:     Mutex<Option<Arc<SpeechHandle>>>,
}

impl AppState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state:        RwLock::new(JarvisState::Idle),
            conversation: Mutex::new(Vec::new()),
            pipe:         OnceLock::new(),
            brain:        OnceLock::new(),
            brightness:   Mutex::new(1.0),
            speech:       Mutex::new(None),
        })
    }

    pub fn init_pipe(&self, pipe: Arc<AudioPipe>) {
        let _ = self.pipe.set(pipe);
    }

    pub fn init_brain(&self, brain: Arc<Brain>) {
        let _ = self.brain.set(brain);
    }

    pub fn current(&self) -> JarvisState {
        *self.state.read()
    }

    pub fn pipe(&self) -> Option<&AudioPipe> {
        self.pipe.get().map(|a| a.as_ref())
    }

    pub fn brain(&self) -> Option<&Brain> {
        self.brain.get().map(|a| a.as_ref())
    }

    pub fn set(&self, app: &AppHandle, next: JarvisState) {
        let prev = *self.state.read();
        if prev == next { return; }
        *self.state.write() = next;
        let _ = app.emit("jarvis:state", next);
        tracing::info!("state {:?} → {:?}", prev, next);

        if let Some(pipe) = self.pipe.get() {
            pipe.clear_rolling();
        }

        if next == JarvisState::Listening && prev != JarvisState::Listening {
            self.begin_recording(app);
        } else if prev == JarvisState::Listening && next != JarvisState::Listening {
            self.end_recording(app);
        }
    }

    fn begin_recording(&self, _app: &AppHandle) {
        let Some(pipe) = self.pipe.get() else {
            tracing::error!("mic: pipe not initialized");
            return;
        };
        pipe.begin_recording();
    }

    fn end_recording(&self, app: &AppHandle) {
        let Some(pipe) = self.pipe.get() else { return };
        let result = pipe.end_recording();

        let app = app.clone();
        std::thread::spawn(move || {
            let _ = app.emit("jarvis:level", 0.0_f32);

            // Reach the shared AppState from inside this background thread
            let st: tauri::State<'_, Arc<AppState>> = app.state();

            if result.samples.is_empty() {
                tracing::warn!("mic: no samples captured");
                st.set(&app, JarvisState::Idle);
                return;
            }

            st.set(&app, JarvisState::Thinking);

            if let Err(e) = save_recording_to_tmp(&result) {
                tracing::warn!("wav save: {e}");
            }

            let transcript = match stt::transcribe(&result.samples, result.sample_rate, result.channels) {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!("stt: {e}");
                    st.set(&app, JarvisState::Error);
                    return;
                }
            };

            if transcript.is_empty() {
                tracing::info!("stt: empty transcript");
                st.set(&app, JarvisState::Idle);
                return;
            }
            let _ = app.emit("jarvis:transcript", transcript.clone());

            let Some(brain) = st.brain.get() else {
                tracing::error!("brain not initialized");
                let _ = app.emit(
                    "jarvis:response",
                    "No LLM provider configured, sir. Check the env file.".to_string(),
                );
                st.set(&app, JarvisState::Idle);
                return;
            };

            let reply = {
                let mut conv = st.conversation.lock();
                let r = brain.ask(&mut conv, &transcript);
                drop(conv);
                r
            };

            let reply = match reply {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!("brain: {e}");
                    // Speak the friendly error through the normal Speaking
                    // path so the user hears what's wrong and the UI returns
                    // to Idle properly.
                    friendly_brain_error(&e.to_string())
                }
            };

            let _ = app.emit("jarvis:response", reply.clone());
            st.set(&app, JarvisState::Speaking);

            // Start TTS as a killable subprocess; store the handle so the
            // barge-in handler in another thread can interrupt it.
            let speech_handle = match tts::start_speaking(&reply) {
                Ok(h) => Arc::new(h),
                Err(e) => {
                    tracing::warn!("tts: {e}");
                    st.set(&app, JarvisState::Idle);
                    return;
                }
            };
            *st.speech.lock() = Some(speech_handle.clone());

            // NOTE: Automatic acoustic barge-in is intentionally disabled —
            // mic kept picking up Jarvis's own voice via speakers and
            // self-interrupting. We're sticking with manual interrupt via
            // ⇧⌘J for v1 until we ship proper AEC (acoustic echo
            // cancellation). The is_speaking_mode flag stays false → the
            // detector in audio.rs never fires.
            //
            // The SpeechHandle is still stored in st.speech so the toggle
            // hotkey can kill it on demand. See `AppState::toggle`.

            // Block until speech finishes naturally OR is killed by toggle.
            let _ = speech_handle.wait_blocking();

            *st.speech.lock() = None;

            // Only transition to Idle if barge-in didn't already move us
            // elsewhere (e.g. to Listening). Comparing the CURRENT state
            // is reliable now that every transition goes through set().
            if st.current() == JarvisState::Speaking {
                st.set(&app, JarvisState::Idle);
            }
        });
    }

    /// Reserved for when automatic barge-in is re-enabled (after we ship
    /// proper AEC). Currently the audio pipe never fires barge_in events
    /// because is_speaking_mode stays false.
    #[allow(dead_code)]
    pub fn on_barge_in(&self, app: &AppHandle) {
        if self.current() != JarvisState::Speaking { return; }
        self.kill_current_speech();
        self.set(app, JarvisState::Listening);
    }

    pub fn toggle(&self, app: &AppHandle) {
        let current = *self.state.read();
        let next = match current {
            JarvisState::Idle      => JarvisState::Listening,
            JarvisState::Speaking  => {
                // Manual interrupt while Jarvis is talking: kill the TTS
                // subprocess and jump straight into Listening so the user's
                // next utterance is captured.
                self.kill_current_speech();
                JarvisState::Listening
            }
            _ => JarvisState::Idle,
        };
        self.set(app, next);
    }

    /// SIGKILL the currently-playing TTS subprocess, if any.
    fn kill_current_speech(&self) {
        let handle = self.speech.lock().take();
        if let Some(h) = handle {
            tracing::info!("killing active TTS subprocess");
            h.kill();
        }
    }

    /// Hard stop from any state. Kills TTS, returns to Idle. In-flight LLM
    /// calls may still complete on their thread but their result is dropped.
    pub fn stop(&self, app: &AppHandle) {
        let current = self.current();
        tracing::info!("stop: requested from {:?}", current);
        self.kill_current_speech();
        if let Some(pipe) = self.pipe.get() {
            pipe.stop_speaking_mode();
        }
        self.set(app, JarvisState::Idle);
    }

    pub fn on_vad_stop(&self, app: &AppHandle) {
        if *self.state.read() == JarvisState::Listening {
            self.set(app, JarvisState::Idle);
        }
    }

    pub fn reset_context(&self, app: &AppHandle) {
        let n = self.conversation.lock().len();
        self.conversation.lock().clear();
        tracing::info!("brain: conversation reset ({n} entries dropped)");
        let _ = app.emit("jarvis:response", "Memory cleared, sir.".to_string());
    }

    pub fn adjust_brightness(&self, app: &AppHandle, delta: f32) {
        let mut b = self.brightness.lock();
        *b = (*b + delta).clamp(0.30, 2.00);
        let v = *b;
        drop(b);
        tracing::info!("brightness: {:.2}×", v);
        let _ = app.emit("jarvis:brightness", v);
    }

    pub fn dev_cycle(&self, app: &AppHandle) {
        let next = match *self.state.read() {
            JarvisState::Idle      => JarvisState::Listening,
            JarvisState::Listening => JarvisState::Thinking,
            JarvisState::Thinking  => JarvisState::Speaking,
            JarvisState::Speaking  => JarvisState::Idle,
            JarvisState::Error     => JarvisState::Idle,
        };
        self.set(app, next);
    }
}

fn save_recording_to_tmp(result: &audio::RecordingResult) -> anyhow::Result<PathBuf> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let path = std::env::temp_dir().join(format!("jarvis_{}.wav", ts));
    audio::write_wav(result, &path)?;
    tracing::info!("mic: saved → {}", path.display());
    Ok(path)
}

/// Turn an upstream error into a short, speakable sentence.
/// Strips URLs / JSON / parens so the TTS doesn't read out garbage.
fn friendly_brain_error(e: &str) -> String {
    let lc = e.to_lowercase();
    if lc.contains("429") || lc.contains("resource_exhausted") || lc.contains("rate limit") || lc.contains("quota") {
        "Every brain provider is rate-limited at the moment, sir. Try again in a minute.".to_string()
    } else if lc.contains("no llm provider configured") || lc.contains("api_key") || lc.contains("api key") || lc.contains("gemini_api_key") || lc.contains("openai_api_key") {
        "No brain is configured, sir. Check the env file.".to_string()
    } else if lc.contains("error sending request") || lc.contains("dns") || lc.contains("tls") || lc.contains("ssl") || lc.contains("connection refused") || lc.contains("connection reset") {
        // Often a network filter blocking an
        // uncategorized LLM endpoint. Be specific so the user knows what to fix.
        "I couldn't reach the brain provider, sir. The endpoint may be blocked by your network.".to_string()
    } else if lc.contains("timeout") || lc.contains("timed out") {
        "The request timed out, sir.".to_string()
    } else if lc.contains("connect") || lc.contains("network") || lc.contains("unreachable") {
        "Network issue, sir. I couldn't reach the brain.".to_string()
    } else if lc.contains("503") || lc.contains("502") || lc.contains("504") {
        "The brain provider is having trouble, sir. Try again shortly.".to_string()
    } else {
        // Generic — pull just the first line and strip URLs / brackets / JSON noise
        let first = e.lines().next().unwrap_or(e);
        let cleaned = clean_for_speech(first);
        if cleaned.is_empty() {
            "Something went wrong, sir.".to_string()
        } else {
            format!("Something went wrong, sir. {}", cleaned)
        }
    }
}

/// Strip URLs, parenthesized blocks, JSON braces — anything that sounds bad
/// when read aloud.
fn clean_for_speech(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut paren_depth: i32 = 0;
    let mut brace_depth: i32 = 0;
    for word in s.split_whitespace() {
        // Skip URLs entirely
        if word.contains("http://") || word.contains("https://") {
            continue;
        }
        let mut buf = String::with_capacity(word.len());
        for c in word.chars() {
            match c {
                '(' => paren_depth += 1,
                ')' => paren_depth = (paren_depth - 1).max(0),
                '{' => brace_depth += 1,
                '}' => brace_depth = (brace_depth - 1).max(0),
                _ if paren_depth == 0 && brace_depth == 0 => buf.push(c),
                _ => {} // inside ( ) or { } — skip
            }
        }
        let trimmed = buf.trim_matches(|c: char| !c.is_alphanumeric() && c != '.');
        if trimmed.is_empty() { continue; }
        if !out.is_empty() { out.push(' '); }
        out.push_str(trimmed);
        if out.len() > 120 { break; } // cap length so we don't ramble
    }
    out
}

// Silence dead-code warning for brain re-export
#[allow(dead_code)]
fn _ensure_brain_used() { let _ = brain::SYSTEM_PROMPT; }
