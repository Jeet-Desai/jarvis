use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Conversation mode: when true, after each turn Jarvis automatically
    /// re-arms the mic for the next user utterance — no need to re-say
    /// "Jarvis". Set by wake word or first Cmd+Shift+J; cleared by Cmd+Shift+S
    /// or Cmd+Shift+C, or by toggling off mid-Listening.
    session:    AtomicBool,
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
            session:      AtomicBool::new(false),
        })
    }

    pub fn session_active(&self) -> bool {
        self.session.load(Ordering::Acquire)
    }

    pub fn start_session(&self) {
        if !self.session_active() {
            self.session.store(true, Ordering::Release);
            tracing::info!("session: conversation mode ON");
        }
    }

    pub fn end_session(&self) {
        if self.session_active() {
            self.session.store(false, Ordering::Release);
            tracing::info!("session: conversation mode OFF");
        }
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

    /// Terminal end of a turn that produced no continuation — empty/garbage
    /// transcript, no samples, brain or TTS failure. Ends conversation mode
    /// FIRST, then drops to the given resting state.
    ///
    /// Use this — never a bare `set(Idle)` — whenever a turn dead-ends.
    /// Otherwise `session` stays stuck `true` while we sit in Idle, and the
    /// wake watcher (wake.rs) treats "session active" as "mic is busy" and
    /// refuses to re-arm the wake word — so the user gets wedged: not
    /// auto-listening, and "Jarvis" no longer responds. The one transition
    /// to Idle that must NOT end the session is the transient stop signal in
    /// `on_vad_stop`, which immediately hands off to `end_recording`.
    fn end_turn(&self, app: &AppHandle, resting: JarvisState) {
        self.end_session();
        self.set(app, resting);
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
                st.end_turn(&app, JarvisState::Idle);
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
                    st.end_turn(&app, JarvisState::Error);
                    return;
                }
            };

            if transcript.is_empty() {
                tracing::info!("stt: empty transcript");
                st.end_turn(&app, JarvisState::Idle);
                return;
            }
            let _ = app.emit("jarvis:transcript", transcript.clone());

            let Some(brain) = st.brain.get() else {
                tracing::error!("brain not initialized");
                let _ = app.emit(
                    "jarvis:response",
                    "No LLM provider configured, sir. Check the env file.".to_string(),
                );
                st.end_turn(&app, JarvisState::Idle);
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
                    st.end_turn(&app, JarvisState::Idle);
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

            // After TTS finishes naturally:
            //   - If we're in conversation mode → re-arm the mic immediately
            //     (Speaking → Listening) so the user can speak the next turn
            //     without needing to re-trigger the wake word.
            //   - Otherwise → back to Idle.
            // Skip if barge-in already moved us elsewhere.
            if st.current() == JarvisState::Speaking {
                if st.session_active() {
                    st.set(&app, JarvisState::Listening);
                } else {
                    st.set(&app, JarvisState::Idle);
                }
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
            JarvisState::Idle => {
                // Manual start of a conversation session
                self.start_session();
                JarvisState::Listening
            }
            JarvisState::Speaking => {
                // Manual interrupt while Jarvis is talking: kill the TTS
                // and jump back to Listening. Session stays active so the
                // user can immediately speak.
                self.kill_current_speech();
                JarvisState::Listening
            }
            _ => {
                // Listening / Thinking / Error → cancel current activity and
                // end the conversation session.
                self.end_session();
                JarvisState::Idle
            }
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

    /// Hard stop from any state. Kills TTS, ends the conversation session,
    /// returns to Idle. In-flight LLM calls may still complete on their
    /// thread but their result is dropped on the floor.
    pub fn stop(&self, app: &AppHandle) {
        let current = self.current();
        tracing::info!("stop: requested from {:?}", current);
        self.end_session();
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

/// Map any upstream error to a SHORT, NON-TECHNICAL spoken acknowledgement.
/// The real error (provider name, URL, HTTP code, JSON body) goes only to
/// the tracing log — it never reaches the user. Three principles:
///   1. Don't leak implementation details ("brain provider", "API", URLs).
///   2. Don't suggest configuration steps to non-dev users.
///   3. Don't be verbose — one short sentence so the user just knows
///      something didn't go through and they can retry.
fn friendly_brain_error(_e: &str) -> String {
    // Rotate through a few phrasings so back-to-back errors don't sound
    // like a broken record. (Cheap deterministic rotation by clock seconds.)
    let phrasings = [
        "Apologies, sir — I couldn't get to that one. Please try again.",
        "I'm having a moment of trouble, sir. Could you repeat that?",
        "Something went sideways, sir. Try once more.",
    ];
    let i = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as usize)
        .unwrap_or(0)
        % phrasings.len();
    phrasings[i].to_string()
}

// Silence dead-code warning for brain re-export
#[allow(dead_code)]
fn _ensure_brain_used() { let _ = brain::SYSTEM_PROMPT; }
