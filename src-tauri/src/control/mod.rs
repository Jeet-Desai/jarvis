//! Computer control — the cross-platform actuation layer that lets Jarvis
//! drive the machine like a person: move/click the mouse, type, press key
//! combos, scroll, and (on macOS) read the focused app's accessibility tree
//! to understand and click on-screen UI.
//!
//! Architecture / cross-platform seam:
//!   - INPUT is `enigo` (CGEvent on macOS, SendInput on Windows, X11 on
//!     Linux) — already portable, lives entirely in this file.
//!   - READING the UI tree + label-clicking is platform-specific and lives in
//!     `macos.rs` (AXUIElement) / `windows.rs` (UI Automation, future). The
//!     `read_ui_elements` / `click_ui_element` fns below dispatch by `cfg`.
//!   - Everything above this layer (tools, the policy gate, the brain) is
//!     OS-agnostic, so the Windows port is a backend swap, not a rewrite.
//!
//! enigo is `Send + !Sync`, so the single instance lives behind a `Mutex`.
//! Tool calls are sequential within a turn, so contention is nil.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
pub(crate) mod windows;

use anyhow::{anyhow, Result};
use enigo::{
    Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings,
};
use parking_lot::Mutex;
use serde::Serialize;
use std::sync::OnceLock;

/// One interactive element read from the OS accessibility tree. Coordinates
/// are in global screen points (the same space `move_mouse` expects), so the
/// brain can read an element and then click its center.
#[derive(Debug, Clone, Serialize)]
pub struct UiElement {
    pub role: String,
    /// Best human label: title, else description, else (short) value.
    pub label: String,
    /// Top-left screen position.
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// Center point — the click target.
    pub center_x: i32,
    pub center_y: i32,
    pub focused: bool,
}

// ──────────────────────────────────────────────────────────────────────────
// enigo actuator (cross-platform input)
// ──────────────────────────────────────────────────────────────────────────

static ENIGO: OnceLock<Mutex<Enigo>> = OnceLock::new();

/// Run an input action against the enigo backend.
///
/// CRITICAL (macOS): enigo's keyboard path calls Text Input Services
/// (HIToolbox `TSMGetInputSourceProperty`) to map characters → keycodes, and
/// those APIs ASSERT they run on the main dispatch queue. Calling them from a
/// background thread tripped a `dispatch_assert_queue` SIGTRAP that aborted the
/// whole app (release builds are `panic=abort`). So on macOS we marshal every
/// input op — and the lazy `Enigo::new`, which also touches TSM — onto the main
/// thread (the run loop is idle during a turn). Other platforms run inline.
fn run_input<T: Send + 'static>(
    f: impl FnOnce(&mut Enigo) -> Result<T> + Send + 'static,
) -> Result<T> {
    let job = move || -> Result<T> {
        if ENIGO.get().is_none() {
            let e = Enigo::new(&Settings::default()).map_err(|e| {
                anyhow!("input backend unavailable — grant Accessibility to Jarvis in System Settings → Privacy & Security → Accessibility, then retry. ({e:?})")
            })?;
            let _ = ENIGO.set(Mutex::new(e));
        }
        let mut g = ENIGO.get().expect("enigo initialized above").lock();
        f(&mut g)
    };
    #[cfg(target_os = "macos")]
    {
        run_on_main(job)
    }
    #[cfg(not(target_os = "macos"))]
    {
        job()
    }
}

/// Move the cursor to an absolute screen point.
pub fn move_mouse(x: i32, y: i32) -> Result<()> {
    run_input(move |e| {
        e.move_mouse(x, y, Coordinate::Abs)
            .map_err(|err| anyhow!("move_mouse: {err:?}"))
    })
}

/// Click at `(x, y)` if given, else at the current cursor position.
pub fn click(x: Option<i32>, y: Option<i32>, button: &str, double: bool) -> Result<()> {
    let btn = match button.to_lowercase().as_str() {
        "right" | "secondary" => Button::Right,
        "middle" => Button::Middle,
        _ => Button::Left,
    };
    run_input(move |e| {
        if let (Some(x), Some(y)) = (x, y) {
            e.move_mouse(x, y, Coordinate::Abs)
                .map_err(|err| anyhow!("click move: {err:?}"))?;
        }
        e.button(btn, Direction::Click)
            .map_err(|err| anyhow!("click: {err:?}"))?;
        if double {
            e.button(btn, Direction::Click)
                .map_err(|err| anyhow!("double-click: {err:?}"))?;
        }
        Ok(())
    })
}

/// Scroll. `amount` lines; positive = down (vertical) / right (horizontal).
pub fn scroll(amount: i32, horizontal: bool) -> Result<()> {
    let axis = if horizontal { Axis::Horizontal } else { Axis::Vertical };
    run_input(move |e| e.scroll(amount, axis).map_err(|err| anyhow!("scroll: {err:?}")))
}

/// Type text via synthetic keystrokes (handles unicode/emoji/newlines). For
/// large pastes the clipboard path in `tools.rs` is gentler on slow apps, so
/// that is the default today; this native path is the cross-platform fallback
/// (e.g. the Windows port, where the osascript clipboard-paste doesn't exist).
#[allow(dead_code)]
pub fn type_text(text: &str) -> Result<()> {
    let owned = text.to_string();
    run_input(move |e| e.text(&owned).map_err(|err| anyhow!("type_text: {err:?}")))
}

/// Press a key, optionally with held modifiers (cmd/shift/option/control).
pub fn press_keys(key: &str, modifiers: &[String]) -> Result<()> {
    let k = parse_key(key).ok_or_else(|| anyhow!("unknown key: {key:?}"))?;
    let mods: Vec<Key> = modifiers
        .iter()
        .filter_map(|m| parse_modifier(m))
        .collect();
    run_input(move |e| {
        for m in &mods {
            e.key(*m, Direction::Press)
                .map_err(|err| anyhow!("modifier press: {err:?}"))?;
        }
        let press_res = e.key(k, Direction::Click).map_err(|err| anyhow!("key: {err:?}"));
        // Always release modifiers even if the key press failed, so we never
        // leave Cmd/Shift stuck down.
        for m in mods.iter().rev() {
            let _ = e.key(*m, Direction::Release);
        }
        press_res
    })
}

/// Current cursor position in screen points.
pub fn cursor_position() -> Result<(i32, i32)> {
    run_input(|e| e.location().map_err(|err| anyhow!("cursor_position: {err:?}")))
}

/// Main display size in points.
pub fn screen_size() -> Result<(i32, i32)> {
    run_input(|e| e.main_display().map_err(|err| anyhow!("screen_size: {err:?}")))
}

fn parse_modifier(m: &str) -> Option<Key> {
    match m.trim().to_lowercase().as_str() {
        "cmd" | "command" | "meta" | "super" | "win" => Some(Key::Meta),
        "shift" => Some(Key::Shift),
        "option" | "opt" | "alt" => Some(Key::Alt),
        "control" | "ctrl" => Some(Key::Control),
        _ => None,
    }
}

fn parse_key(name: &str) -> Option<Key> {
    let lc = name.trim().to_lowercase();
    let k = match lc.as_str() {
        "return" | "enter" => Key::Return,
        "tab" => Key::Tab,
        "escape" | "esc" => Key::Escape,
        "space" | "spacebar" => Key::Space,
        "delete" | "backspace" => Key::Backspace,
        "forwarddelete" | "fwddelete" => Key::Delete,
        "left" | "leftarrow" => Key::LeftArrow,
        "right" | "rightarrow" => Key::RightArrow,
        "up" | "uparrow" => Key::UpArrow,
        "down" | "downarrow" => Key::DownArrow,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdown" | "pgdn" => Key::PageDown,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        _ => {
            // Single character → Unicode key. Multi-char unknown → fail.
            let mut chars = name.chars();
            let first = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Key::Unicode(first)
        }
    };
    Some(k)
}

// ──────────────────────────────────────────────────────────────────────────
// Accessibility reading + label-clicking (platform-dispatched)
// ──────────────────────────────────────────────────────────────────────────

/// Read interactive elements (buttons, fields, links, menu items…) of the
/// focused application, capped at `max`, with their screen coordinates.
///
/// On macOS the AXUIElement reads run on the MAIN thread (dispatched there from
/// the brain's background thread) — AX is happiest on the main run loop, which
/// is otherwise idle during a turn — with an 8s timeout so an unresponsive
/// target app can never hang the turn.
pub fn read_ui_elements(max: usize) -> Result<Vec<UiElement>> {
    #[cfg(target_os = "macos")]
    {
        run_on_main(move || macos::read_ui_elements(max))
    }
    #[cfg(target_os = "windows")]
    {
        windows::read_ui_elements(max)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = max;
        anyhow::bail!("reading the UI tree is not supported on this platform yet")
    }
}

/// Find the interactive element whose label best matches `label` and click its
/// center. Platform-agnostic: reads elements (platform-specific) then clicks via
/// the cross-platform input path, so it works wherever `read_ui_elements` does.
pub fn click_ui_element(label: &str, role: Option<&str>) -> Result<UiElement> {
    let target = label.trim().to_lowercase();
    if target.is_empty() {
        anyhow::bail!("click_ui_element needs a non-empty label");
    }
    // Normalize an "AXButton"-style role hint so it matches both the macOS
    // ("AXButton") and Windows ("Button") role vocabularies.
    let role_lc = role.map(|r| {
        let lc = r.to_lowercase();
        lc.strip_prefix("ax").map(str::to_string).unwrap_or(lc)
    });
    let elements = read_ui_elements(400)?;
    let total = elements.len();

    let mut scored: Vec<(i32, UiElement)> = elements
        .iter()
        .filter_map(|e| {
            let l = e.label.trim().to_lowercase();
            if l.is_empty() {
                return None;
            }
            if let Some(rf) = &role_lc {
                if !e.role.to_lowercase().contains(rf) {
                    return None;
                }
            }
            let score = if l == target {
                0
            } else if l.starts_with(&target) {
                1
            } else if l.contains(&target) {
                2
            } else if target.contains(&l) {
                3
            } else {
                return None;
            };
            Some((score, e.clone()))
        })
        .collect();
    scored.sort_by_key(|(s, _)| *s);
    let n_matched = scored.len();

    let Some((score, chosen)) = scored.into_iter().next() else {
        // Log a sample of what WAS on screen so a miss is diagnosable — e.g. the
        // search-result link's real AX label differs from the text we asked for,
        // or the web links never surfaced in the accessibility tree at all.
        let sample: Vec<String> = elements
            .iter()
            .filter(|e| !e.label.trim().is_empty())
            .take(40)
            .map(|e| {
                let mut l = e.label.trim().replace('\n', " ");
                if l.chars().count() > 40 {
                    l = l.chars().take(40).collect::<String>() + "…";
                }
                format!("{l} [{}]", e.role)
            })
            .collect();
        tracing::warn!(
            "click_element: \u{201c}{label}\u{201d} (role={:?}) matched NONE of {total} elements; labels seen: {}",
            role,
            if sample.is_empty() { "(none had labels)".into() } else { sample.join(", ") }
        );
        anyhow::bail!("no clickable element labelled like \u{201c}{label}\u{201d} in the focused app — call read_screen_elements to see the available labels, or use locate_on_screen then click");
    };

    tracing::info!(
        "click_element: \u{201c}{label}\u{201d} → \u{201c}{}\u{201d} [{}] (score {score}, {n_matched} of {total} candidates) at ({}, {})",
        chosen.label, chosen.role, chosen.center_x, chosen.center_y
    );
    click(Some(chosen.center_x), Some(chosen.center_y), "left", false)?;
    Ok(chosen)
}

/// Run a closure on the macOS main thread and wait (bounded) for its result.
/// Only ever called from a background thread (the brain turn), so blocking on
/// the channel can't deadlock the main run loop.
#[cfg(target_os = "macos")]
fn run_on_main<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    use std::sync::mpsc;
    use std::time::Duration;
    let app = crate::gate::app().ok_or_else(|| anyhow!("app handle not ready for a UI read"))?;
    let (tx, rx) = mpsc::channel();
    app.run_on_main_thread(move || {
        let _ = tx.send(f());
    })
    .map_err(|e| anyhow!("main-thread dispatch failed: {e}"))?;
    match rx.recv_timeout(Duration::from_secs(8)) {
        Ok(inner) => inner,
        Err(_) => Err(anyhow!("UI read timed out — the focused app may be unresponsive")),
    }
}

/// Roles we surface to the brain — interactive controls plus the static text
/// that usually labels them. Keeps the tree small and LLM-relevant.
#[cfg(target_os = "macos")]
pub(crate) fn is_interesting_role(role: &str) -> bool {
    matches!(
        role,
        "AXButton"
            | "AXTextField"
            | "AXTextArea"
            | "AXLink"
            | "AXMenuItem"
            | "AXMenuButton"
            | "AXCheckBox"
            | "AXRadioButton"
            | "AXPopUpButton"
            | "AXComboBox"
            | "AXTabButton"
            | "AXSlider"
            | "AXDisclosureTriangle"
            | "AXSearchField"
    )
}
