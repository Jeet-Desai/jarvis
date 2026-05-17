// Reload trigger: 2026-05-17h (clipboard fallback tool)
#[cfg(target_os = "macos")]
#[macro_use]
extern crate objc;

mod audio;
mod brain;
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

// ──────────────────────────────────────────────────────────────────────────────
// HUD <-> orb mutual exclusivity
// ──────────────────────────────────────────────────────────────────────────────

/// Cmd+Shift+C — hide every Jarvis surface (orb + HUD). Press again to
/// bring Jarvis back (just the orb returns; HUD has to be re-opened explicitly).
fn toggle_close_internal(app: &AppHandle) {
    let orb = app.get_webview_window("orb");
    let hud = app.get_webview_window("hud");

    let orb_visible = orb.as_ref().and_then(|w| w.is_visible().ok()).unwrap_or(false);
    let hud_visible = hud.as_ref().and_then(|w| w.is_visible().ok()).unwrap_or(false);
    let anything_visible = orb_visible || hud_visible;

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

fn position_orb(app: &AppHandle) -> tauri::Result<()> {
    let orb = app
        .get_webview_window("orb")
        .ok_or_else(|| tauri::Error::WindowNotFound)?;

    if let Some(monitor) = orb.current_monitor()? {
        let scale = monitor.scale_factor();
        let mon_w_logical = monitor.size().width as f64 / scale;
        // Window is 360x360 but the visible disc is centered ~280px wide,
        // so we offset by ~360px to keep the disc the same visual distance
        // from the screen edge as before.
        let x_logical = mon_w_logical - 360.0 + 8.0;
        let y_logical = 30.0;
        orb.set_position(tauri::LogicalPosition::new(x_logical, y_logical))?;
        tracing::info!(
            "orb at logical ({:.0}, {:.0}); monitor {}x{} scale {:.1}",
            x_logical,
            y_logical,
            monitor.size().width,
            monitor.size().height,
            scale
        );
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
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "jarvis=info,tauri=warn".into()),
        )
        .init();

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
    // DEV: preview every state
    let preview_sc   = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyP);

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
    let preview_sc_for_handler  = preview_sc.clone();

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
                    } else if shortcut == &preview_sc_for_handler {
                        tracing::info!("hotkey: Cmd+Shift+P — preview cycle (dev)");
                        app_state_for_preview.dev_cycle(app);
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
            jarvis_stop
        ])
        .setup(move |app| {
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

            // Build the pluggable LLM brain (Gemini / OpenAI-compat / Ollama)
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

            let gs = app.global_shortcut();
            gs.register(jarvis_sc)?;
            gs.register(hud_sc)?;
            gs.register(close_sc)?;
            gs.register(reset_sc)?;
            gs.register(stop_sc)?;
            gs.register(brighter_sc)?;
            gs.register(dimmer_sc)?;
            gs.register(preview_sc)?;

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

            tracing::info!(
                "Jarvis ready. ⇧⌘J on/off · ⇧⌘S stop · ⇧⌘H HUD · ⇧⌘C close · ⇧⌘R reset · ⇧⌘+/- brightness · ⇧⌘P preview (dev)"
            );
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
