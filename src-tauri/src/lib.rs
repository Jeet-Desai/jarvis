// Reload trigger: 2026-05-17m (non-technical error messages — logs-only details)
#[cfg(target_os = "macos")]
#[macro_use]
extern crate objc;

mod audio;
mod brain;
mod control;
mod gate;
mod mcp;
mod mcp_server;
#[cfg(target_os = "macos")]
mod permissions;
mod policy;
mod state;
mod stt;
mod tools;
mod tts;
mod wake;

use std::sync::Arc;
use tauri::{AppHandle, Listener, Manager};
use tauri_plugin_global_shortcut::{
    Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState,
};

#[cfg(target_os = "macos")]
use tauri_nspanel::{
    tauri_panel, CollectionBehavior, PanelLevel, StyleMask, WebviewWindowExt,
};

use crate::state::{AppState, JarvisState};

// NSPanel definitions — converted from the regular Tauri windows in setup().
// can_become_key_window: true on the HUD so transcript / response panels
// can scroll if needed; false on the corner orb so clicks pass through.
// (Both panel types defined in a single macro invocation — the macro emits
// duplicate imports if called twice.)
#[cfg(target_os = "macos")]
tauri_panel! {
    panel!(OrbPanel { config: { can_become_key_window: false, can_become_main_window: false } })
    panel!(HudPanel { config: { can_become_key_window: true,  can_become_main_window: false } })
}

// ──────────────────────────────────────────────────────────────────────────────
// Tauri commands (callable from frontend)
// ──────────────────────────────────────────────────────────────────────────────

#[tauri::command]
fn jarvis_toggle(app: AppHandle, st: tauri::State<'_, Arc<AppState>>) {
    st.toggle(&app);
}

#[tauri::command]
fn jarvis_set(app: AppHandle, st: tauri::State<'_, Arc<AppState>>, new_state: String) {
    let next = match new_state.as_str() {
        "idle" => JarvisState::Idle,
        "listening" => JarvisState::Listening,
        "thinking" => JarvisState::Thinking,
        "speaking" => JarvisState::Speaking,
        "error" => JarvisState::Error,
        _ => return,
    };
    st.set(&app, next);
}

#[tauri::command]
fn toggle_hud(app: AppHandle) {
    toggle_hud_internal(&app);
}

#[tauri::command]
fn toggle_close(app: AppHandle) {
    toggle_close_internal(&app);
}

#[tauri::command]
fn jarvis_stop(app: AppHandle, st: tauri::State<'_, Arc<AppState>>) {
    st.stop(&app);
}

/// Resolve a pending computer-control confirmation from the HUD buttons.
#[tauri::command]
fn jarvis_resolve_confirm(id: u64, approved: bool) {
    gate::resolve(id, approved);
}

/// Expose the current safety-policy mode to the HUD (read-only).
#[tauri::command]
fn jarvis_policy_mode() -> String {
    policy::global().mode.as_str().to_string()
}

// ──────────────────────────────────────────────────────────────────────────────
// Dev-agent orchestrator helpers (Phase 1.2/1.3)
// ──────────────────────────────────────────────────────────────────────────────

/// Where the Python orchestrator sidecar lives. Override with
/// JARVIS_ORCHESTRATOR_DIR; otherwise the in-repo `orchestrator/` beside
/// src-tauri (resolved from CARGO_MANIFEST_DIR, same trick as the .env loader).
fn orchestrator_dir() -> String {
    if let Ok(d) = std::env::var("JARVIS_ORCHESTRATOR_DIR") {
        if !d.trim().is_empty() {
            return d;
        }
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("orchestrator"))
        .unwrap_or_else(|| "orchestrator".into())
        .to_string_lossy()
        .into_owned()
}

/// Normalize the orchestrator's `list_dev_agents` reply into a plain JSON array
/// of agent summaries for the HUD.
///
/// Shape gotcha: FastMCP serializes a Python `list` return as ONE text content
/// block PER element, which our MCP layer (`extract_text_content`) then joins
/// with newlines — so a 2-agent reply arrives as `{ "text": "{..}\n{..}" }`,
/// which is NOT valid single JSON. We therefore parse a *stream* of top-level
/// JSON values, not one. We also tolerate a clean array, a `{result|agents:[…]}`
/// envelope, and a single agent object. Anything else → empty array.
fn extract_agent_list(v: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
        // Try a clean single parse first (covers a real JSON array / object).
        if let Ok(val) = serde_json::from_str::<Value>(t) {
            return normalize_agent_value(val);
        }
        // Fall back: several concatenated JSON objects ("{..}\n{..}").
        let items: Vec<Value> = serde_json::Deserializer::from_str(t)
            .into_iter::<Value>()
            .filter_map(Result::ok)
            .collect();
        return Value::Array(items);
    }
    normalize_agent_value(v.clone())
}

/// Coerce one parsed value into an array of agent summaries.
fn normalize_agent_value(val: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match val {
        Value::Array(_) => val,
        Value::Object(ref o) => {
            for key in ["result", "agents"] {
                if let Some(Value::Array(a)) = o.get(key) {
                    return Value::Array(a.clone());
                }
            }
            // A lone agent summary (has id+status) → wrap it; an envelope
            // (e.g. a CallToolResult) we don't recognize → empty.
            if o.contains_key("id") && o.contains_key("status") {
                Value::Array(vec![val])
            } else {
                Value::Array(vec![])
            }
        }
        _ => Value::Array(vec![]),
    }
}

/// Diff a freshly-polled agent roster against the previous statuses and return
/// the spoken one-line updates to announce. Updates `prev` in place.
///
/// We announce ONLY meaningful, finished-work transitions of a LIVE agent:
///   • working → idle  ("finished its turn")
///   • anything → error ("ran into an error")
/// We deliberately ignore spawning/working (just-started) transitions — the
/// brain already gives a one-line confirmation when it spawns an agent.
///
/// The FIRST poll seeds `prev` SILENTLY (`seeded == false`): otherwise every
/// agent restored from the persistence registry, or already running when the
/// app launched, would announce itself on startup. Detached (live=false)
/// agents never announce — their status is frozen history.
fn diff_agent_updates(
    prev: &mut std::collections::HashMap<String, String>,
    agents: &serde_json::Value,
    seeded: bool,
) -> Vec<String> {
    let mut out = Vec::new();
    let Some(arr) = agents.as_array() else { return out };
    for a in arr {
        let Some(id) = a.get("id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) else {
            continue;
        };
        let status = a.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("an agent");
        let live = a.get("live").and_then(|v| v.as_bool()).unwrap_or(true);
        let prev_status = prev.insert(id.to_string(), status.to_string());

        if !seeded || !live {
            continue;
        }
        let Some(ps) = prev_status.as_deref() else { continue };
        if ps == status {
            continue;
        }
        if ps == "working" && status == "idle" {
            out.push(format!("Sir, the {name} agent finished its turn."));
        } else if status == "error" && ps != "error" {
            out.push(format!("Sir, the {name} agent ran into an error."));
        }
    }
    out
}

// ──────────────────────────────────────────────────────────────────────────────
// HUD <-> orb mutual exclusivity
// ──────────────────────────────────────────────────────────────────────────────

/// Cmd+Shift+C — hide every Jarvis surface (orb + HUD). Also ends any
/// active conversation session so Jarvis isn't still listening in the
/// background while hidden. Press again to bring Jarvis back (just the orb
/// returns; HUD has to be re-opened explicitly).
fn toggle_close_internal(app: &AppHandle) {
    let orb = app.get_webview_window("orb");
    let hud = app.get_webview_window("hud");

    let orb_visible = orb.as_ref().and_then(|w| w.is_visible().ok()).unwrap_or(false);
    let hud_visible = hud.as_ref().and_then(|w| w.is_visible().ok()).unwrap_or(false);
    let anything_visible = orb_visible || hud_visible;

    // Always end the conversation session on a close action — user is
    // explicitly stepping away.
    let st: tauri::State<'_, Arc<AppState>> = app.state();
    st.stop(app);

    if anything_visible {
        if let Some(o) = &orb { let _ = o.hide(); }
        if let Some(h) = &hud { let _ = h.hide(); }
        tracing::info!("Jarvis hidden completely (Cmd+Shift+C)");
    } else {
        if let Some(o) = &orb { let _ = o.show(); }
        tracing::info!("Jarvis restored — orb shown");
    }
}

fn toggle_hud_internal(app: &AppHandle) {
    let Some(hud) = app.get_webview_window("hud") else {
        tracing::warn!("hud window missing");
        return;
    };
    let orb = app.get_webview_window("orb");

    let hud_visible = hud.is_visible().unwrap_or(false);
    if hud_visible {
        let _ = hud.hide();
        if let Some(orb) = &orb {
            let _ = orb.show();
        }
        tracing::info!("HUD hidden → orb shown");
    } else {
        if let Some(orb) = &orb {
            let _ = orb.hide();
        }
        let _ = hud.show();
        let _ = hud.set_focus();
        tracing::info!("HUD shown → orb hidden");
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Orb window setup (positioning + macOS overlay behavior)
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum OrbPreset {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

const ORB_MARGIN: f64 = 24.0;

fn position_orb(app: &AppHandle) -> tauri::Result<()> {
    let orb = app
        .get_webview_window("orb")
        .ok_or_else(|| tauri::Error::WindowNotFound)?;

    // Try restoring the user's last-known orb position; otherwise default to
    // the top-right corner.
    if let Some((lx, ly)) = load_orb_position(app) {
        orb.set_position(tauri::LogicalPosition::new(lx, ly))?;
        tracing::info!("orb restored to saved position ({:.0}, {:.0})", lx, ly);
    } else {
        move_orb_to_preset(&orb, OrbPreset::TopRight)?;
    }

    orb.show()?;
    orb.set_always_on_top(true)?;
    orb.set_visible_on_all_workspaces(true)?;

    if let Some(hud) = app.get_webview_window("hud") {
        let _ = hud.set_always_on_top(true);
        let _ = hud.set_visible_on_all_workspaces(true);
    }

    Ok(())
}

fn move_orb_to_preset(orb: &tauri::WebviewWindow, preset: OrbPreset) -> tauri::Result<()> {
    let Some(monitor) = orb.current_monitor()? else { return Ok(()); };
    let scale = monitor.scale_factor();
    let mon_w = monitor.size().width as f64 / scale;
    let mon_h = monitor.size().height as f64 / scale;
    let win = orb.outer_size()?;
    let win_w = win.width as f64 / scale;
    let win_h = win.height as f64 / scale;

    let (x, y) = match preset {
        OrbPreset::TopLeft     => (ORB_MARGIN,                ORB_MARGIN),
        OrbPreset::TopRight    => (mon_w - win_w - ORB_MARGIN, ORB_MARGIN),
        OrbPreset::BottomLeft  => (ORB_MARGIN,                mon_h - win_h - ORB_MARGIN),
        OrbPreset::BottomRight => (mon_w - win_w - ORB_MARGIN, mon_h - win_h - ORB_MARGIN),
    };
    orb.set_position(tauri::LogicalPosition::new(x, y))?;
    tracing::info!("orb → {:?} at ({:.0}, {:.0})", preset, x, y);
    Ok(())
}

fn orb_position_file(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("orb_position.json"))
}

fn save_orb_position(app: &AppHandle, lx: f64, ly: f64) {
    let Some(path) = orb_position_file(app) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let json = serde_json::json!({ "x": lx, "y": ly });
    if let Err(e) = std::fs::write(&path, json.to_string()) {
        tracing::warn!("save orb pos: {e}");
    }
}

fn load_orb_position(app: &AppHandle) -> Option<(f64, f64)> {
    let path = orb_position_file(app)?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Some((v.get("x")?.as_f64()?, v.get("y")?.as_f64()?))
}

/// Convert orb + HUD windows into NSPanels and set them as ScreenSaver-level
/// fullscreen-auxiliary overlays. This is what makes them appear over
/// fullscreen apps (browsers, video, VS Code, etc.) reliably on macOS.
#[cfg(target_os = "macos")]
fn convert_to_panels(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(orb) = app.get_webview_window("orb") {
        match orb.to_panel::<OrbPanel>() {
            Ok(panel) => {
                panel.set_level(PanelLevel::Custom(1000).value());
                panel.set_collection_behavior(
                    CollectionBehavior::new()
                        .can_join_all_spaces()
                        .full_screen_auxiliary()
                        .stationary()
                        .into(),
                );
                panel.set_style_mask(StyleMask::empty().nonactivating_panel().into());
                panel.set_hides_on_deactivate(false);
                panel.show();
                tracing::info!("orb converted to NSPanel (level 1000, fullscreen-auxiliary)");
            }
            Err(e) => tracing::error!("orb.to_panel failed: {e:?}"),
        }
    }
    if let Some(hud) = app.get_webview_window("hud") {
        match hud.to_panel::<HudPanel>() {
            Ok(panel) => {
                panel.set_level(PanelLevel::Custom(1000).value());
                panel.set_collection_behavior(
                    CollectionBehavior::new()
                        .can_join_all_spaces()
                        .full_screen_auxiliary()
                        .stationary()
                        .into(),
                );
                panel.set_style_mask(StyleMask::empty().nonactivating_panel().into());
                panel.set_hides_on_deactivate(false);
                tracing::info!("hud converted to NSPanel (level 1000, fullscreen-auxiliary)");
            }
            Err(e) => tracing::error!("hud.to_panel failed: {e:?}"),
        }
    }
    Ok(())
}

// (Old NSWindow-based raise_above_fullscreen() removed — replaced by the
// NSPanel conversion in convert_to_panels() above. NSWindow can never
// truly overlay a fullscreen app's Space; NSPanel can.)

// ──────────────────────────────────────────────────────────────────────────────
// Entry point
// ──────────────────────────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
/// macOS GUI apps opened from Finder / the Dock / the Applications folder are
/// started by `launchd`, which hands them a BARE environment — `PATH=/usr/bin`
/// — and does NOT source the user's shell profile (`~/.zshrc` etc.). So
/// user-installed CLIs vanish: `claude` (the subscription brain, in
/// `~/.local/bin`) and `node`/`npx` (the calendar MCP, in `/opt/homebrew/bin`)
/// become unreachable, silently breaking the brain and the MCP servers. A
/// launch from a terminal only works because it inherits the shell's richer
/// PATH. (This is why the same `.app` worked from a terminal but failed from
/// the Dock with "Something went sideways" — `binary_on_path("claude")` was
/// false, so the subscription brain dropped out.)
///
/// Fix (run ONCE at startup, before any provider/MCP/subprocess init): prepend
/// the common user + Homebrew + toolchain install dirs that actually exist and
/// aren't already on PATH. Same "fix the PATH on launch" step shipped by VS
/// Code, GitHub Desktop, and friends — it makes the bundled `.app` behave the
/// same however it was opened.
///
/// Safe to mutate the process env here: this is the first thing `run()` does,
/// on the main thread, before any other thread is spawned.
fn augment_path_for_gui_launch() {
    let home = std::env::var("HOME").unwrap_or_default();
    // User-local first (where `claude` installs), then Homebrew (Apple Silicon
    // + Intel), then language toolchains.
    let candidates = [
        format!("{home}/.local/bin"),
        "/opt/homebrew/bin".to_string(),
        "/opt/homebrew/sbin".to_string(),
        "/usr/local/bin".to_string(),
        format!("{home}/.cargo/bin"),
        format!("{home}/.bun/bin"),
        format!("{home}/.deno/bin"),
        format!("{home}/.volta/bin"),
        format!("{home}/bin"),
    ];
    let existing = std::env::var("PATH").unwrap_or_default();
    let (new_path, added) =
        compute_path_augmentation(&existing, &candidates, |d| std::path::Path::new(d).is_dir());
    if !added.is_empty() {
        std::env::set_var("PATH", new_path);
        tracing::info!(
            "startup: augmented PATH for GUI launch (+{} dirs: {})",
            added.len(),
            added.join(", ")
        );
    }
}

/// Pure core of `augment_path_for_gui_launch`, factored out for testing:
/// returns the new PATH and the dirs added (front-to-back). Prepends each
/// candidate that `exists` and isn't already present, preserving candidate
/// order at the front. `exists` is injected so tests need not touch the disk.
fn compute_path_augmentation(
    existing: &str,
    candidates: &[String],
    exists: impl Fn(&str) -> bool,
) -> (String, Vec<String>) {
    let mut entries: Vec<String> =
        existing.split(':').filter(|s| !s.is_empty()).map(str::to_string).collect();
    let mut added: Vec<String> = Vec::new();
    // Prepend in reverse so the candidate order is preserved at the front.
    for dir in candidates.iter().rev() {
        if exists(dir) && !entries.iter().any(|e| e == dir) {
            entries.insert(0, dir.clone());
            added.push(dir.clone());
        }
    }
    added.reverse(); // report front-to-back
    (entries.join(":"), added)
}

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "jarvis=info,tauri=warn".into()),
        )
        .init();

    // Repair PATH for Finder/Dock launches BEFORE anything reads it (brain
    // provider detection, MCP spawns). See the fn doc for the launchd caveat.
    augment_path_for_gui_launch();

    // Load .env from project root (jarvis/.env). Silently ignored if absent.
    let _ = dotenvy::from_path(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .map(|p| p.join(".env"))
            .unwrap_or_else(|| ".env".into()),
    );

    let app_state = AppState::new();

    // Shortcuts:
    //   Cmd+Shift+J = toggle Jarvis on/off (listening ↔ idle)
    //   Cmd+Shift+H = toggle HUD (mutually exclusive with orb)
    //   Cmd+Shift+C = close everything (orb + HUD hidden); press again → orb returns
    let jarvis_sc    = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyJ);
    let hud_sc       = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyH);
    let close_sc     = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyC);
    let reset_sc     = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyR);
    let stop_sc      = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyS);
    let brighter_sc  = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::Equal);
    let dimmer_sc    = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::Minus);
    // Orb position presets (⇧⌘1-4 = TL TR BR BL — clockwise from top-left)
    let pos_tl_sc    = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::Digit1);
    let pos_tr_sc    = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::Digit2);
    let pos_br_sc    = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::Digit3);
    let pos_bl_sc    = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::Digit4);
    // DEV: preview every state
    let preview_sc   = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyP);
    // Approve a pending computer-control confirmation (deny = ⇧⌘S / HUD / timeout)
    let confirm_yes_sc = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyY);

    let app_state_for_handler   = app_state.clone();
    let app_state_for_preview   = app_state.clone();
    let app_state_for_reset     = app_state.clone();
    let app_state_for_stop      = app_state.clone();
    let app_state_for_bright    = app_state.clone();
    let jarvis_sc_for_handler   = jarvis_sc.clone();
    let hud_sc_for_handler      = hud_sc.clone();
    let close_sc_for_handler    = close_sc.clone();
    let reset_sc_for_handler    = reset_sc.clone();
    let stop_sc_for_handler     = stop_sc.clone();
    let brighter_sc_for_handler = brighter_sc.clone();
    let dimmer_sc_for_handler   = dimmer_sc.clone();
    let pos_tl_sc_for_handler   = pos_tl_sc.clone();
    let pos_tr_sc_for_handler   = pos_tr_sc.clone();
    let pos_br_sc_for_handler   = pos_br_sc.clone();
    let pos_bl_sc_for_handler   = pos_bl_sc.clone();
    let preview_sc_for_handler  = preview_sc.clone();
    let confirm_yes_sc_for_handler = confirm_yes_sc.clone();

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init());
    #[cfg(target_os = "macos")]
    let builder = builder.plugin(tauri_nspanel::init());

    builder
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, shortcut, event| {
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    if shortcut == &jarvis_sc_for_handler {
                        tracing::info!("hotkey: Cmd+Shift+J — toggle Jarvis");
                        app_state_for_handler.toggle(app);
                    } else if shortcut == &hud_sc_for_handler {
                        tracing::info!("hotkey: Cmd+Shift+H — toggle HUD");
                        toggle_hud_internal(app);
                    } else if shortcut == &close_sc_for_handler {
                        tracing::info!("hotkey: Cmd+Shift+C — close Jarvis");
                        toggle_close_internal(app);
                    } else if shortcut == &reset_sc_for_handler {
                        tracing::info!("hotkey: Cmd+Shift+R — reset conversation");
                        app_state_for_reset.reset_context(app);
                    } else if shortcut == &stop_sc_for_handler {
                        tracing::info!("hotkey: Cmd+Shift+S — stop");
                        app_state_for_stop.stop(app);
                    } else if shortcut == &brighter_sc_for_handler {
                        app_state_for_bright.adjust_brightness(app, 0.15);
                    } else if shortcut == &dimmer_sc_for_handler {
                        app_state_for_bright.adjust_brightness(app, -0.15);
                    } else if shortcut == &pos_tl_sc_for_handler {
                        if let Some(orb) = app.get_webview_window("orb") {
                            let _ = move_orb_to_preset(&orb, OrbPreset::TopLeft);
                        }
                    } else if shortcut == &pos_tr_sc_for_handler {
                        if let Some(orb) = app.get_webview_window("orb") {
                            let _ = move_orb_to_preset(&orb, OrbPreset::TopRight);
                        }
                    } else if shortcut == &pos_br_sc_for_handler {
                        if let Some(orb) = app.get_webview_window("orb") {
                            let _ = move_orb_to_preset(&orb, OrbPreset::BottomRight);
                        }
                    } else if shortcut == &pos_bl_sc_for_handler {
                        if let Some(orb) = app.get_webview_window("orb") {
                            let _ = move_orb_to_preset(&orb, OrbPreset::BottomLeft);
                        }
                    } else if shortcut == &preview_sc_for_handler {
                        tracing::info!("hotkey: Cmd+Shift+P — preview cycle (dev)");
                        app_state_for_preview.dev_cycle(app);
                    } else if shortcut == &confirm_yes_sc_for_handler {
                        if gate::resolve_latest(true) {
                            tracing::info!("hotkey: Cmd+Shift+Y — approved pending action");
                        }
                    }
                })
                .build(),
        )
        .manage(app_state.clone())
        .invoke_handler(tauri::generate_handler![
            jarvis_toggle,
            jarvis_set,
            toggle_hud,
            toggle_close,
            jarvis_stop,
            jarvis_resolve_confirm,
            jarvis_policy_mode
        ])
        .setup(move |app| {
            // Make the app handle globally reachable so tool handlers can emit
            // activity / confirmation events to the UI from any thread.
            gate::set_app_handle(app.handle().clone());

            // Load the configurable safety policy (writes a documented default
            // policy.toml on first run) and open the append-only audit log.
            policy::init(app.path().app_config_dir().ok());
            if let Ok(data_dir) = app.path().app_data_dir() {
                gate::init_audit(&data_dir);
            }

            // Fire every TCC prompt that has a programmatic trigger so the
            // user grants permissions to Jarvis (not osascript / not a shell).
            // Runs on a background thread — UI shouldn't block on user dialogs.
            #[cfg(target_os = "macos")]
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(800));
                permissions::warmup();
            });

            position_orb(app.handle()).ok();

            // Convert orb + HUD to NSPanels for reliable fullscreen overlay.
            // Must happen AFTER the windows exist (i.e. inside setup), and
            // BEFORE any window shows, so the subclass takes effect on the
            // first display cycle.
            #[cfg(target_os = "macos")]
            if let Err(e) = convert_to_panels(app.handle()) {
                tracing::error!("panel conversion failed: {e}");
            }

            // Start the persistent mic pipe. This is what triggers the
            // macOS microphone permission prompt on first launch.
            match audio::start_pipe(app.handle().clone()) {
                Ok(pipe) => {
                    app_state.init_pipe(Arc::new(pipe));
                    tracing::info!("audio: persistent mic pipe up");

                    // Start the whisper-based "Hey Jarvis" wake watcher.
                    // It only fires while state is Idle.
                    wake::start_watcher(app.handle().clone(), app_state.clone());
                }
                Err(e) => {
                    tracing::error!("audio: failed to start pipe: {e} — mic features unavailable");
                }
            }

            // Host Jarvis's own tools as an MCP server on localhost so an agent
            // (Claude Code on the user's subscription) can be the brain and call
            // them through the same safety gate. Non-fatal — if it can't bind,
            // the agent brain just won't have an endpoint and the brain falls
            // back to the API providers below.
            if let Err(e) = mcp_server::start() {
                tracing::warn!("mcp_server: {e}");
            }

            // Build the pluggable LLM brain (agent-driver / Gemini / OpenAI / Ollama)
            match brain::Brain::from_env() {
                Ok(brain) => {
                    app_state.init_brain(Arc::new(brain));
                }
                Err(e) => {
                    tracing::error!("brain: {e}");
                    // App still runs (mic + STT + TTS work), but Jarvis can't reply
                    // until at least one provider is configured.
                }
            }

            // Optional: start MCP servers. Only initialize Google Calendar if
            // the user supplied an OAuth credentials path. Failure is non-fatal
            // — the calendar tools just report a friendly setup error when used.
            match mcp::McpManager::new() {
                Ok(manager) => {
                    manager.install_global();
                    if let Ok(creds) = std::env::var("GOOGLE_OAUTH_CREDENTIALS") {
                        if !creds.trim().is_empty() {
                            if let Err(e) = manager.init_calendar(creds.trim()) {
                                tracing::warn!("mcp[calendar]: {e}");
                            }
                        }
                    }

                    // Dev-agent orchestrator sidecar. Look it up via
                    // JARVIS_ORCHESTRATOR_DIR, else assume the in-repo
                    // `orchestrator/` next to src-tauri. Non-fatal: if the venv
                    // isn't set up the dev-agent tools just report a setup hint.
                    let orch_dir = orchestrator_dir();
                    if let Err(e) = manager.init_orchestrator(&orch_dir) {
                        tracing::warn!("mcp[orchestrator]: {e}");
                    } else {
                        // Background poll: push the live agent roster to the HUD
                        // "AGENTS" panel every ~2s. Lives on its own thread and
                        // calls the sidecar directly (NOT through tools::dispatch),
                        // so it never enters the activity feed or takes the
                        // conversation lock.
                        //
                        // It ALSO drives PROACTIVE VOICE: it diffs each poll's
                        // statuses and, when a dev agent finishes a turn or
                        // errors, speaks a one-line update — but only while
                        // Jarvis is fully at rest (`try_announce` gates on Idle /
                        // no session / no pending confirm). Pending updates are
                        // queued locally and drained one-per-poll, so a long
                        // conversation just defers them, and speaking never
                        // blocks the roster for more than one announcement.
                        let poll_mgr = manager.clone();
                        let announce_state = app_state.clone();
                        let announce_app = app.handle().clone();
                        std::thread::spawn(move || {
                            use std::collections::{HashMap, VecDeque};
                            // Cap the backlog so a user returning after many
                            // finished turns hears a few recent updates, not a
                            // long monologue; drop the oldest when full.
                            const MAX_QUEUED: usize = 5;
                            let mut prev: HashMap<String, String> = HashMap::new();
                            let mut seeded = false;
                            let mut queue: VecDeque<String> = VecDeque::new();
                            loop {
                                std::thread::sleep(std::time::Duration::from_secs(2));
                                if !poll_mgr.orchestrator_ready() {
                                    continue;
                                }
                                match poll_mgr.orchestrator_call("list_dev_agents", serde_json::json!({})) {
                                    Ok(v) => {
                                        let agents = extract_agent_list(&v);
                                        gate::emit_agents(agents.clone());
                                        for msg in diff_agent_updates(&mut prev, &agents, seeded) {
                                            queue.push_back(msg);
                                            while queue.len() > MAX_QUEUED {
                                                queue.pop_front();
                                            }
                                        }
                                        seeded = true;
                                    }
                                    Err(e) => tracing::debug!("mcp[orchestrator]: poll failed: {e}"),
                                }
                                // Drain at most ONE update per poll, and only if
                                // Jarvis is at rest. If it isn't, the update stays
                                // queued for a later, quieter moment.
                                if let Some(msg) = queue.front().cloned() {
                                    if announce_state.try_announce(&announce_app, &msg) {
                                        queue.pop_front();
                                    }
                                }
                            }
                        });
                    }
                }
                Err(e) => tracing::warn!("mcp: failed to start manager: {e}"),
            }

            let gs = app.global_shortcut();
            gs.register(jarvis_sc)?;
            gs.register(hud_sc)?;
            gs.register(close_sc)?;
            gs.register(reset_sc)?;
            gs.register(stop_sc)?;
            gs.register(brighter_sc)?;
            gs.register(dimmer_sc)?;
            gs.register(pos_tl_sc)?;
            gs.register(pos_tr_sc)?;
            gs.register(pos_br_sc)?;
            gs.register(pos_bl_sc)?;
            gs.register(preview_sc)?;
            gs.register(confirm_yes_sc)?;

            // Persist orb position whenever the user drags or hotkey-moves it.
            if let Some(orb) = app.get_webview_window("orb") {
                let app_handle = app.handle().clone();
                let orb_clone = orb.clone();
                orb.on_window_event(move |evt| {
                    if let tauri::WindowEvent::Moved(_) = evt {
                        // Capture as logical coords (DPI-independent)
                        if let Ok(pos) = orb_clone.outer_position() {
                            let scale = orb_clone.scale_factor().unwrap_or(2.0);
                            let lx = pos.x as f64 / scale;
                            let ly = pos.y as f64 / scale;
                            save_orb_position(&app_handle, lx, ly);
                        }
                    }
                });
            }

            // VAD listener: auto-end the utterance after silence
            let vad_app    = app.handle().clone();
            let vad_state  = app_state.clone();
            app.listen("jarvis:vad_stop", move |_evt| {
                vad_state.on_vad_stop(&vad_app);
            });

            // Barge-in listener: user started speaking while Jarvis was replying
            let bi_app   = app.handle().clone();
            let bi_state = app_state.clone();
            app.listen("jarvis:barge_in", move |_evt| {
                bi_state.on_barge_in(&bi_app);
            });

            // Pre-warm the whisper model in the background so the first
            // transcription doesn't pay the ~1-2s model-load cost.
            stt::prewarm();

            // Optional input-backend self-test (set JARVIS_SELFTEST_INPUT=1):
            // exercises enigo from a BACKGROUND thread — exactly the path that
            // crashed via Text-Input-Services-on-non-main-thread (SIGTRAP) before
            // the main-thread fix. screen_size forces Enigo::new (builds the
            // keymap → TSM) and key(Escape) drives the keyboard TSM path; both
            // now marshal to the main thread. If this logs OK without aborting,
            // the fix holds. Escape is benign; no text is typed anywhere.
            #[cfg(target_os = "macos")]
            if std::env::var("JARVIS_SELFTEST_INPUT").ok().as_deref() == Some("1") {
                std::thread::spawn(|| {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    tracing::info!("selftest: exercising input backend from a background thread…");
                    match control::screen_size() {
                        Ok((w, h)) => tracing::info!("selftest: screen_size OK = {w}x{h}"),
                        Err(e) => tracing::error!("selftest: screen_size FAILED: {e}"),
                    }
                    match control::cursor_position() {
                        Ok((x, y)) => tracing::info!("selftest: cursor_position OK = ({x},{y})"),
                        Err(e) => tracing::error!("selftest: cursor_position FAILED: {e}"),
                    }
                    match control::press_keys("escape", &[]) {
                        Ok(_) => tracing::info!("selftest: key(Escape) OK — keyboard TSM path ran on main"),
                        Err(e) => tracing::error!("selftest: key(Escape) FAILED: {e}"),
                    }
                    tracing::info!("selftest: input backend did NOT crash — TSM main-thread fix verified");
                });
            }

            tracing::info!(
                "Jarvis ready.  Hotkeys: \
                 Cmd+Shift+J on/off | \
                 Cmd+Shift+S stop | \
                 Cmd+Shift+H HUD | \
                 Cmd+Shift+C close | \
                 Cmd+Shift+R reset | \
                 Cmd+Shift+(+/-) brightness | \
                 Cmd+Shift+(1..4) corner | \
                 Cmd+Shift+P preview (dev)"
            );
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod proactive_tests {
    use super::diff_agent_updates;
    use serde_json::json;
    use std::collections::HashMap;

    fn agents(items: &[(&str, &str, &str, bool)]) -> serde_json::Value {
        // (id, name, status, live)
        serde_json::Value::Array(
            items
                .iter()
                .map(|(id, name, status, live)| {
                    json!({ "id": id, "name": name, "status": status, "live": live })
                })
                .collect(),
        )
    }

    #[test]
    fn first_poll_seeds_silently() {
        // The very first poll must NOT announce — otherwise every agent already
        // running, or restored from the persistence registry, blurts on launch.
        let mut prev = HashMap::new();
        let now = agents(&[("a1", "site", "working", true), ("a2", "api", "idle", true)]);
        let out = diff_agent_updates(&mut prev, &now, false);
        assert!(out.is_empty(), "first (unseeded) poll spoke: {out:?}");
        assert_eq!(prev.get("a1").map(String::as_str), Some("working"));
        assert_eq!(prev.get("a2").map(String::as_str), Some("idle"));
    }

    #[test]
    fn working_to_idle_announces_once() {
        let mut prev = HashMap::new();
        diff_agent_updates(&mut prev, &agents(&[("a1", "site", "working", true)]), false);
        // Turn finishes → one update.
        let out = diff_agent_updates(&mut prev, &agents(&[("a1", "site", "idle", true)]), true);
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("site") && out[0].contains("finished"), "{out:?}");
        // Stable across the next identical poll — no repeat.
        let again = diff_agent_updates(&mut prev, &agents(&[("a1", "site", "idle", true)]), true);
        assert!(again.is_empty(), "repeated on an unchanged status: {again:?}");
    }

    #[test]
    fn transition_into_error_announces() {
        let mut prev = HashMap::new();
        diff_agent_updates(&mut prev, &agents(&[("a1", "site", "working", true)]), false);
        let out = diff_agent_updates(&mut prev, &agents(&[("a1", "site", "error", true)]), true);
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("error"), "{out:?}");
    }

    #[test]
    fn just_started_transitions_are_silent() {
        // spawning→working and idle→working (re-instructed) are NOT announced.
        let mut prev = HashMap::new();
        diff_agent_updates(&mut prev, &agents(&[("a1", "site", "spawning", true)]), false);
        let a = diff_agent_updates(&mut prev, &agents(&[("a1", "site", "working", true)]), true);
        assert!(a.is_empty(), "announced a start: {a:?}");
        diff_agent_updates(&mut prev, &agents(&[("a1", "site", "idle", true)]), true);
        let b = diff_agent_updates(&mut prev, &agents(&[("a1", "site", "working", true)]), true);
        assert!(b.is_empty(), "announced a re-instruct: {b:?}");
    }

    #[test]
    fn detached_agents_never_announce() {
        // A live=false agent is frozen history (from the persistence registry);
        // even a working→idle-looking change on it must stay silent.
        let mut prev = HashMap::new();
        diff_agent_updates(&mut prev, &agents(&[("a1", "site", "working", false)]), false);
        let out = diff_agent_updates(&mut prev, &agents(&[("a1", "site", "idle", false)]), true);
        assert!(out.is_empty(), "announced a detached agent: {out:?}");
    }
}

#[cfg(test)]
mod path_tests {
    use super::compute_path_augmentation;

    fn cands(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn finder_launch_prepends_existing_dirs_in_order() {
        // The launchd / Finder case: a bare PATH, and the real install dirs get
        // prepended in candidate order (so ~/.local/bin wins the `claude` lookup).
        let candidates = cands(&["/u/.local/bin", "/opt/homebrew/bin", "/missing"]);
        let (new_path, added) =
            compute_path_augmentation("/usr/bin", &candidates, |d| d != "/missing");
        assert_eq!(new_path, "/u/.local/bin:/opt/homebrew/bin:/usr/bin");
        assert_eq!(added, vec!["/u/.local/bin", "/opt/homebrew/bin"]);
    }

    #[test]
    fn terminal_launch_is_a_noop_when_already_present() {
        // The terminal case: the dirs are already on PATH, so nothing changes.
        let candidates = cands(&["/u/.local/bin", "/opt/homebrew/bin"]);
        let existing = "/u/.local/bin:/opt/homebrew/bin:/usr/bin";
        let (new_path, added) = compute_path_augmentation(existing, &candidates, |_| true);
        assert_eq!(new_path, existing);
        assert!(added.is_empty(), "should not re-add present dirs: {added:?}");
    }

    #[test]
    fn missing_dirs_are_skipped() {
        // Candidate dirs that don't exist on disk are never added.
        let candidates = cands(&["/nope1", "/nope2"]);
        let (new_path, added) = compute_path_augmentation("/usr/bin", &candidates, |_| false);
        assert_eq!(new_path, "/usr/bin");
        assert!(added.is_empty());
    }

    #[test]
    fn partial_overlap_only_adds_the_missing_one() {
        let candidates = cands(&["/u/.local/bin", "/opt/homebrew/bin"]);
        // /opt/homebrew/bin already present; only ~/.local/bin should prepend.
        let (new_path, added) =
            compute_path_augmentation("/opt/homebrew/bin:/usr/bin", &candidates, |_| true);
        assert_eq!(new_path, "/u/.local/bin:/opt/homebrew/bin:/usr/bin");
        assert_eq!(added, vec!["/u/.local/bin"]);
    }
}
