//! macOS accessibility-tree reader.
//!
//! Reads the focused application's interactive UI elements (buttons, fields,
//! links, menu items…) with their on-screen coordinates via the AXUIElement
//! API, so the brain can "see" native UI and click it by label. We hand-declare
//! the handful of AX functions we need (same FFI style as `permissions.rs`) and
//! use `core-foundation` for safe CFString handling and correct Create-rule
//! `CFRelease` discipline (every `…Copy…` result is +1 retained and must be
//! released exactly once — `CFString::wrap_under_create_rule` and the explicit
//! `CFRelease`/`CFRetain` calls below honor that).

use anyhow::{anyhow, bail, Result};
use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetTypeID, CFArrayGetValueAtIndex};
use core_foundation_sys::base::{CFGetTypeID, CFRelease, CFRetain, CFTypeRef};
use core_foundation_sys::string::{CFStringGetTypeID, CFStringRef};
use std::ffi::c_void;

use super::{is_interesting_role, UiElement};

type AXUIElementRef = *mut c_void;
type AXError = i32;
const KAX_SUCCESS: AXError = 0;
const KAX_VALUE_CGPOINT: u32 = 1;
const KAX_VALUE_CGSIZE: u32 = 2;
/// AX trees can be deep; cap recursion and node count to keep the LLM payload
/// small and the read fast.
const MAX_DEPTH: usize = 16;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    fn AXValueGetValue(value: CFTypeRef, the_type: u32, out: *mut c_void) -> bool;
}

/// RAII handle for an AXUIElementRef (a CFType). Releases its +1 on drop.
struct AxElement(AXUIElementRef);
impl Drop for AxElement {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0 as CFTypeRef) }
        }
    }
}

fn ax_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

// ── public API (called via control::mod dispatch) ──────────────────────────

pub fn read_ui_elements(max: usize) -> Result<Vec<UiElement>> {
    if !ax_trusted() {
        bail!("I need Accessibility permission to read the screen's UI. Grant Jarvis under System Settings → Privacy & Security → Accessibility, then retry.");
    }
    unsafe {
        let app = focused_app()
            .ok_or_else(|| anyhow!("couldn't reach the focused app via accessibility (it may not be accessibility-aware)"))?;
        let mut out = Vec::new();
        collect(&app, 0, &mut out, max.max(1));
        Ok(out)
    }
}

// ── traversal ───────────────────────────────────────────────────────────────

unsafe fn collect(el: &AxElement, depth: usize, out: &mut Vec<UiElement>, max: usize) {
    if out.len() >= max || depth > MAX_DEPTH {
        return;
    }
    let role = str_attr(el.0, "AXRole").unwrap_or_default();
    if is_interesting_role(&role) {
        if let Some(ui) = to_ui_element(el, &role) {
            out.push(ui);
            if out.len() >= max {
                return;
            }
        }
    }
    for child in children(el.0) {
        collect(&child, depth + 1, out, max);
        if out.len() >= max {
            return;
        }
    }
}

unsafe fn to_ui_element(el: &AxElement, role: &str) -> Option<UiElement> {
    let size = size_attr(el.0, "AXSize")?;
    if size.width <= 0.0 || size.height <= 0.0 {
        return None; // hidden / offscreen
    }
    let pos = point_attr(el.0, "AXPosition").unwrap_or(CGPoint { x: 0.0, y: 0.0 });
    let label = str_attr(el.0, "AXTitle")
        .filter(|s| !s.trim().is_empty())
        .or_else(|| str_attr(el.0, "AXDescription").filter(|s| !s.trim().is_empty()))
        .or_else(|| {
            str_attr(el.0, "AXValue")
                .filter(|s| !s.trim().is_empty())
                .map(|s| clip(&s, 80))
        })
        .unwrap_or_default();

    let x = pos.x.round() as i32;
    let y = pos.y.round() as i32;
    let w = size.width.round() as i32;
    let h = size.height.round() as i32;
    Some(UiElement {
        role: role.to_string(),
        label,
        x,
        y,
        width: w,
        height: h,
        center_x: x + w / 2,
        center_y: y + h / 2,
        focused: false,
    })
}

// ── AX attribute readers (each honors the Create-rule: result is +1) ─────────

unsafe fn focused_app() -> Option<AxElement> {
    let sys = AXUIElementCreateSystemWide();
    if sys.is_null() {
        return None;
    }
    let sys_el = AxElement(sys);
    let raw = copy_attr_raw(sys_el.0, "AXFocusedApplication")?;
    // `raw` is itself an AXUIElement (CFType) at +1 — take ownership.
    Some(AxElement(raw as AXUIElementRef))
}

unsafe fn copy_attr_raw(el: AXUIElementRef, attr: &str) -> Option<CFTypeRef> {
    let cf_attr = CFString::new(attr);
    let mut out: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(el, cf_attr.as_concrete_TypeRef(), &mut out);
    if err == KAX_SUCCESS && !out.is_null() {
        Some(out)
    } else {
        None
    }
}

unsafe fn str_attr(el: AXUIElementRef, attr: &str) -> Option<String> {
    let raw = copy_attr_raw(el, attr)?;
    if CFGetTypeID(raw) == CFStringGetTypeID() {
        // wrap_under_create_rule consumes the +1 and releases on drop.
        let s = CFString::wrap_under_create_rule(raw as CFStringRef).to_string();
        Some(s)
    } else {
        CFRelease(raw);
        None
    }
}

unsafe fn point_attr(el: AXUIElementRef, attr: &str) -> Option<CGPoint> {
    let raw = copy_attr_raw(el, attr)?;
    let mut p = CGPoint { x: 0.0, y: 0.0 };
    let ok = AXValueGetValue(raw, KAX_VALUE_CGPOINT, &mut p as *mut _ as *mut c_void);
    CFRelease(raw);
    if ok {
        Some(p)
    } else {
        None
    }
}

unsafe fn size_attr(el: AXUIElementRef, attr: &str) -> Option<CGSize> {
    let raw = copy_attr_raw(el, attr)?;
    let mut s = CGSize {
        width: 0.0,
        height: 0.0,
    };
    let ok = AXValueGetValue(raw, KAX_VALUE_CGSIZE, &mut s as *mut _ as *mut c_void);
    CFRelease(raw);
    if ok {
        Some(s)
    } else {
        None
    }
}

unsafe fn children(el: AXUIElementRef) -> Vec<AxElement> {
    let mut out = Vec::new();
    let Some(raw) = copy_attr_raw(el, "AXChildren") else {
        return out;
    };
    if CFGetTypeID(raw) != CFArrayGetTypeID() {
        CFRelease(raw);
        return out;
    }
    let arr = raw as core_foundation_sys::array::CFArrayRef;
    let n = CFArrayGetCount(arr);
    for i in 0..n {
        let item = CFArrayGetValueAtIndex(arr, i);
        if item.is_null() {
            continue;
        }
        // Array elements are borrowed (+0); retain so our AxElement can own +1.
        CFRetain(item as CFTypeRef);
        out.push(AxElement(item as AXUIElementRef));
    }
    CFRelease(raw); // release the array itself (+1 from the copy)
    out
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.replace('\n', " ");
    }
    let t: String = s.chars().take(max).collect();
    format!("{}…", t.replace('\n', " "))
}
