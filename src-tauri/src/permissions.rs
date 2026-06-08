//! One-shot permission warmup. Runs at startup but only prompts for things
//! that aren't already granted.
//!
//! Strategy:
//!   - Categories with a non-prompting "preflight" API (Accessibility, Input
//!     Monitoring read/post, Screen Recording) → check first, only prompt
//!     if not granted. Silent on subsequent launches.
//!   - Categories without a preflight (per-app Automation, Files folders)
//!     → gated behind a marker file written after the first warmup run.
//!     Delete the marker to re-prompt (path printed in logs).

use std::ffi::{c_void, CString};
use std::path::PathBuf;
use std::process::Command;

// ─── ApplicationServices (Accessibility) ─────────────────────────────────────
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
}

// ─── CoreFoundation (for the AX options dict) ────────────────────────────────
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        num_values: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *const c_void;
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        c_str: *const i8,
        encoding: u32,
    ) -> *const c_void;
    fn CFRelease(cf: *const c_void);
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    static kCFBooleanTrue: *const c_void;
}

// ─── CoreGraphics (Input Monitoring + Screen Recording preflight) ────────────
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightListenEventAccess() -> bool;
    fn CGRequestListenEventAccess() -> bool;
    fn CGPreflightPostEventAccess() -> bool;
    fn CGRequestPostEventAccess() -> bool;
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

const KCF_STRING_ENCODING_UTF8: u32 = 0x08000100;

fn marker_path() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(|h| {
        PathBuf::from(h).join("Library/Application Support/com.jeet.jarvis/permissions_warmed.v1")
    })
}

/// Idempotent. Runs the full prompt sweep on first launch only — a marker
/// file is written after, and all subsequent launches early-return.
///
/// Why a marker instead of pure preflight: ad-hoc-signed dev builds get a
/// new CDHash on every rebuild, which invalidates the csreq stored in TCC
/// for strict-csreq services (Accessibility, Screen Recording, Input
/// Monitoring). Preflight would always return false on those, re-prompting
/// every launch. The marker assumes "if the user granted once, trust them"
/// — to redo, delete `~/Library/Application Support/com.jeet.jarvis/
/// permissions_warmed.v1`.
pub fn warmup() {
    if let Some(p) = marker_path() {
        if p.exists() {
            tracing::debug!("permissions: marker exists, skipping warmup");
            return;
        }
    }

    let mut prompted = 0u32;

    // 1) Accessibility — preflight, then prompt-via-settings if missing.
    if unsafe { AXIsProcessTrusted() } {
        tracing::debug!("permissions: accessibility already granted");
    } else {
        unsafe {
            let key_cstr = CString::new("AXTrustedCheckOptionPrompt").unwrap();
            let key = CFStringCreateWithCString(
                std::ptr::null(),
                key_cstr.as_ptr(),
                KCF_STRING_ENCODING_UTF8,
            );
            if !key.is_null() {
                let keys = [key as *const c_void];
                let values = [kCFBooleanTrue];
                let opts = CFDictionaryCreate(
                    std::ptr::null(),
                    keys.as_ptr(),
                    values.as_ptr(),
                    1,
                    &kCFTypeDictionaryKeyCallBacks,
                    &kCFTypeDictionaryValueCallBacks,
                );
                AXIsProcessTrustedWithOptions(opts);
                if !opts.is_null() { CFRelease(opts); }
                CFRelease(key);
            }
        }
        prompted += 1;
        tracing::info!("permissions: accessibility prompt triggered");
    }

    // 2) Input Monitoring (read + post) — preflight, request only if missing.
    if unsafe { CGPreflightListenEventAccess() } {
        tracing::debug!("permissions: input listen already granted");
    } else {
        unsafe { CGRequestListenEventAccess() };
        prompted += 1;
        tracing::info!("permissions: input listen prompt triggered");
    }
    if unsafe { CGPreflightPostEventAccess() } {
        tracing::debug!("permissions: input post already granted");
    } else {
        unsafe { CGRequestPostEventAccess() };
        prompted += 1;
        tracing::info!("permissions: input post prompt triggered");
    }

    // 3) Screen Recording — preflight, request only if missing.
    if unsafe { CGPreflightScreenCaptureAccess() } {
        tracing::debug!("permissions: screen capture already granted");
    } else {
        unsafe { CGRequestScreenCaptureAccess() };
        prompted += 1;
        tracing::info!("permissions: screen capture prompt triggered");
    }

    // 4) Side-effect category (per-app Automation + Files folders).
    //    No preflight API — gate behind a marker file so we only run on
    //    first launch. Delete the marker to redo.
    let marker = marker_path();
    let already_warmed = marker.as_ref().is_some_and(|p| p.exists());
    if !already_warmed {
        for app in [
            "System Events", "Finder",
            "Google Chrome", "Safari", "Arc", "Brave Browser", "Microsoft Edge",
        ] {
            let _ = Command::new("osascript")
                .arg("-e")
                .arg(format!("tell application \"{app}\" to get name"))
                .output();
        }
        if let Ok(home) = std::env::var("HOME") {
            for sub in ["Desktop", "Documents", "Downloads"] {
                let _ = std::fs::read_dir(format!("{home}/{sub}"));
            }
        }
        if let Some(p) = marker.as_ref() {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(p, "1");
            tracing::info!(
                "permissions: first-run warmup done, marker = {}",
                p.display()
            );
        }
        prompted += 1;
    } else {
        tracing::debug!("permissions: automation + files-folders warmup already done");
    }

    if prompted == 0 {
        tracing::info!("permissions: all granted — nothing to prompt");
    } else {
        tracing::info!("permissions: {prompted} category(ies) prompted");
    }
}
