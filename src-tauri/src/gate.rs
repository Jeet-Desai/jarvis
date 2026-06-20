//! Runtime side of the safety gate: the global app handle (so tool handlers
//! can talk to the UI), the live activity feed, the append-only audit log, and
//! the human-in-the-loop confirmation flow.
//!
//! `policy.rs` decides; `gate.rs` enacts. `tools::dispatch` ties them together.
//!
//! Confirmation is resolvable, whichever comes first:
//!   1. the HUD Approve/Deny buttons      → `jarvis_resolve_confirm` command
//!   2. the ⌘⇧Y global hotkey (approve)   → `resolve_latest`
//!   3. the ⌘⇧S global hotkey (stop)      → `cancel_all` (denies ALL pending)
//!   4. a timeout (`policy.confirm_timeout_secs`) → defaults to DENY
//! So the user is never wedged behind a prompt.

use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tauri::{AppHandle, Emitter};

use crate::policy::DangerLevel;

// ── global app handle ───────────────────────────────────────────────────────

static APP: OnceLock<AppHandle> = OnceLock::new();

pub fn set_app_handle(app: AppHandle) {
    let _ = APP.set(app);
}

/// Reserved accessor for tool handlers that need to reach `AppState` directly.
#[allow(dead_code)]
pub fn app() -> Option<AppHandle> {
    APP.get().cloned()
}

fn emit(event: &str, payload: Value) {
    if let Some(a) = APP.get() {
        let _ = a.emit(event, payload);
    }
}

// ── activity feed ─────────────────────────────────────────────────────────

/// Push a step to the HUD activity feed. `status` is one of
/// "awaiting" | "running" | "ok" | "error" | "denied".
pub fn emit_activity(id: u64, tool: &str, summary: &str, danger: DangerLevel, status: &str) {
    emit(
        "jarvis:activity",
        json!({
            "id": id,
            "tool": tool,
            "summary": summary,
            "danger": danger.as_str(),
            "status": status,
        }),
    );
}

// ── audit log (append-only JSONL) ──────────────────────────────────────────

static AUDIT: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

pub fn init_audit(dir: &Path) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!("audit: can't create {} ({e})", dir.display());
        return;
    }
    let path = dir.join("audit.jsonl");
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => {
            let _ = AUDIT.set(Mutex::new(f));
            tracing::info!("audit: logging to {}", path.display());
        }
        Err(e) => tracing::warn!("audit: can't open {} ({e})", path.display()),
    }
}

/// Write one audit record. Secrets are redacted by the caller (`tools::dispatch`).
pub fn audit(record: Value) {
    if let Some(f) = AUDIT.get() {
        if let Ok(mut line) = serde_json::to_string(&record) {
            line.push('\n');
            let _ = f.lock().write_all(line.as_bytes());
        }
    }
}

// ── confirmation flow ───────────────────────────────────────────────────────

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static LATEST: AtomicU64 = AtomicU64::new(0);

fn pending() -> &'static Mutex<HashMap<u64, Sender<bool>>> {
    static P: OnceLock<Mutex<HashMap<u64, Sender<bool>>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Block the current (brain) thread until the user approves/denies `summary`,
/// or until the configured timeout elapses (→ deny). Announces the request on
/// the HUD and speaks it aloud so the user notices even when the HUD is hidden.
pub fn request_confirmation(id: u64, summary: &str, danger: DangerLevel, timeout_secs: u64) -> bool {
    let (tx, rx) = mpsc::channel::<bool>();
    pending().lock().insert(id, tx);
    LATEST.store(id, Ordering::Release);

    emit(
        "jarvis:confirm",
        json!({ "id": id, "summary": summary, "danger": danger.as_str() }),
    );

    // Speak the prompt (fire-and-forget); kill it once the user responds so it
    // doesn't bleed into the spoken answer.
    let spoken = crate::tts::start_speaking(&format!(
        "Sir, may I {}? Say or press yes to proceed.",
        summary
    ))
    .ok()
    .map(Arc::new);

    let approved = rx
        .recv_timeout(Duration::from_secs(timeout_secs.clamp(5, 600)))
        .unwrap_or(false);

    if let Some(s) = spoken {
        s.kill();
    }
    pending().lock().remove(&id);
    LATEST.compare_exchange(id, 0, Ordering::AcqRel, Ordering::Relaxed).ok();
    emit("jarvis:confirm_resolved", json!({ "id": id, "approved": approved }));
    approved
}

/// Resolve a specific pending confirmation (HUD buttons via Tauri command).
pub fn resolve(id: u64, approved: bool) {
    if let Some(tx) = pending().lock().remove(&id) {
        let _ = tx.send(approved);
    }
}

/// Resolve the most recent pending confirmation (the ⌘⇧Y / ⌘⇧N hotkeys).
/// Returns true if there was one to resolve.
pub fn resolve_latest(approved: bool) -> bool {
    let id = LATEST.load(Ordering::Acquire);
    if id == 0 {
        return false;
    }
    if let Some(tx) = pending().lock().remove(&id) {
        let _ = tx.send(approved);
        true
    } else {
        false
    }
}

/// Deny every outstanding confirmation — used by the global stop (⌘⇧S) so a
/// pending prompt can never wedge the assistant.
pub fn cancel_all() {
    let mut p = pending().lock();
    for (_, tx) in p.drain() {
        let _ = tx.send(false);
    }
}

#[allow(dead_code)]
pub fn has_pending() -> bool {
    !pending().lock().is_empty()
}
