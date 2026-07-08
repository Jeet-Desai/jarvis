//! Windows platform backend — the counterpart of `control/macos.rs`.
//!
//! Two halves:
//!   1. **UI Automation reader** (`read_ui_elements`) — mirrors the macOS
//!      AXUIElement reader: interactive elements of the FOCUSED app (buttons,
//!      fields, links, menu items…) with labels and screen coordinates, via
//!      the `uiautomation` crate (IUIAutomation COM API). Same contract:
//!      coordinates are global screen points that `control::click` accepts.
//!   2. **Window/app management + browser context** — the Windows versions of
//!      what tools.rs does with AppleScript on macOS: focused-app name,
//!      list/focus/quit apps (Win32 window enumeration), and reading the
//!      active browser tab (URL from the UIA address bar, page text from the
//!      UIA Document element). Consumed by `tools.rs` under `cfg(windows)`.
//!
//! COM note: `UIAutomation::new()` initializes COM (MTA) on the calling
//! thread. All callers here are background threads (the brain turn), which is
//! exactly where UIA clients are supposed to run — never the UI thread of the
//! target app.

use super::UiElement;
use anyhow::{anyhow, bail, Result};
use uiautomation::controls::ControlType;
use uiautomation::types::{Handle, TreeScope, UIProperty};
use uiautomation::variants::Variant;
use uiautomation::UIAutomation;
use windows::core::BOOL;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindow, GetWindowLongW, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, PostMessageW,
    SetForegroundWindow, ShowWindow, GWL_EXSTYLE, GW_OWNER, SW_RESTORE, WM_CLOSE,
    WS_EX_TOOLWINDOW,
};

fn ui_err(e: uiautomation::Error) -> anyhow::Error {
    anyhow!("ui-automation: {e}")
}

// ──────────────────────────────────────────────────────────────────────────
// UI Automation reader
// ──────────────────────────────────────────────────────────────────────────

/// Control types we surface to the brain — interactive controls plus the
/// static text that usually labels them (mirrors `is_interesting_role` on
/// macOS).
const INTERESTING: &[ControlType] = &[
    ControlType::Button,
    ControlType::Edit,
    ControlType::Hyperlink,
    ControlType::MenuItem,
    ControlType::CheckBox,
    ControlType::RadioButton,
    ControlType::ComboBox,
    ControlType::TabItem,
    ControlType::ListItem,
    ControlType::TreeItem,
    ControlType::SplitButton,
    ControlType::Slider,
];

/// Read interactive elements of the focused (foreground) application.
pub fn read_ui_elements(max: usize) -> Result<Vec<UiElement>> {
    let hwnd = foreground_hwnd()?;
    let automation = UIAutomation::new().map_err(ui_err)?;
    let root = automation
        .element_from_handle(Handle::from(hwnd.0 as isize))
        .map_err(ui_err)?;

    // Push the control-type filter into UIA (an OR condition) — hugely faster
    // than pulling the whole subtree of a browser and filtering in Rust.
    let mut cond = automation
        .create_property_condition(
            UIProperty::ControlType,
            Variant::from(INTERESTING[0] as i32),
            None,
        )
        .map_err(ui_err)?;
    for ct in &INTERESTING[1..] {
        let c = automation
            .create_property_condition(UIProperty::ControlType, Variant::from(*ct as i32), None)
            .map_err(ui_err)?;
        cond = automation.create_or_condition(cond, c).map_err(ui_err)?;
    }

    let found = root.find_all(TreeScope::Subtree, &cond).map_err(ui_err)?;
    let mut out = Vec::new();
    for el in found {
        if out.len() >= max {
            break;
        }
        let rect = match el.get_bounding_rectangle() {
            Ok(r) => r,
            Err(_) => continue,
        };
        let (x, y) = (rect.get_left(), rect.get_top());
        let w = rect.get_right() - rect.get_left();
        let h = rect.get_bottom() - rect.get_top();
        if w <= 0 || h <= 0 {
            continue; // off-screen / zero-size
        }
        let role = el
            .get_control_type()
            .map(|c| format!("{c:?}"))
            .unwrap_or_else(|_| "Unknown".into());
        let mut label = el.get_name().unwrap_or_default();
        if label.trim().is_empty() {
            // Fall back to the current value (e.g. an Edit's text) as a label.
            if let Ok(vp) = el.get_pattern::<uiautomation::patterns::UIValuePattern>() {
                if let Ok(v) = vp.get_value() {
                    label = v;
                }
            }
        }
        let label = clip_label(&label, 80);
        if label.is_empty() {
            continue; // unlabeled elements aren't actionable by the brain
        }
        let focused = el.has_keyboard_focus().unwrap_or(false);
        out.push(UiElement {
            role,
            label,
            x,
            y,
            width: w,
            height: h,
            center_x: x + w / 2,
            center_y: y + h / 2,
            focused,
        });
    }
    Ok(out)
}

fn clip_label(s: &str, max: usize) -> String {
    let t = s.trim().replace('\n', " ");
    if t.chars().count() <= max {
        return t;
    }
    let clipped: String = t.chars().take(max).collect();
    format!("{clipped}…")
}

// ──────────────────────────────────────────────────────────────────────────
// Window / app enumeration & control (Win32)
// ──────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TopWindow {
    pub hwnd: isize,
    pub title: String,
    pub exe_stem: String,
    pub app_name: String,
}

fn foreground_hwnd() -> Result<HWND> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        bail!("no foreground window");
    }
    Ok(hwnd)
}

fn window_title(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

fn window_exe_stem(hwnd: HWND) -> String {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return String::new();
        }
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            h,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(h);
        if ok.is_err() {
            return String::new();
        }
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        std::path::Path::new(&full)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// Friendly display name for a process stem ("chrome" → "Google Chrome"),
/// matching what a macOS user-facing app name would look like — the brain's
/// browser detection (Rule 2) matches on these.
fn friendly_app_name(exe_stem: &str) -> String {
    match exe_stem.to_ascii_lowercase().as_str() {
        "chrome" => "Google Chrome".into(),
        "msedge" => "Microsoft Edge".into(),
        "firefox" => "Firefox".into(),
        "brave" => "Brave Browser".into(),
        "opera" | "opera_gx" => "Opera".into(),
        "vivaldi" => "Vivaldi".into(),
        "arc" => "Arc".into(),
        "iexplore" => "Internet Explorer".into(),
        "code" => "Visual Studio Code".into(),
        "explorer" => "File Explorer".into(),
        "windowsterminal" | "wt" => "Terminal".into(),
        "powershell" | "pwsh" => "PowerShell".into(),
        "cmd" => "Command Prompt".into(),
        "notepad" => "Notepad".into(),
        "slack" => "Slack".into(),
        "discord" => "Discord".into(),
        "whatsapp" => "WhatsApp".into(),
        "telegram" => "Telegram".into(),
        "spotify" => "Spotify".into(),
        "olk" | "outlook" => "Outlook".into(),
        "teams" | "ms-teams" => "Microsoft Teams".into(),
        other => {
            // Capitalize the stem as a readable fallback ("obsidian" → "Obsidian").
            let mut c = other.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        }
    }
}

/// Is this hwnd a "real" top-level app window (visible, unowned, titled, not
/// a tool window)? Same intent as macOS "background only is false".
fn is_app_window(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return false;
        }
        if !GetWindow(hwnd, GW_OWNER).map(|h| h.is_invalid()).unwrap_or(true) {
            return false;
        }
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }
        GetWindowTextLengthW(hwnd) > 0
    }
}

/// Enumerate top-level app windows.
pub fn list_windows() -> Result<Vec<TopWindow>> {
    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let list = unsafe { &mut *(lparam.0 as *mut Vec<TopWindow>) };
        if is_app_window(hwnd) {
            let exe_stem = window_exe_stem(hwnd);
            if !exe_stem.is_empty() {
                list.push(TopWindow {
                    hwnd: hwnd.0 as isize,
                    title: window_title(hwnd),
                    app_name: friendly_app_name(&exe_stem),
                    exe_stem,
                });
            }
        }
        BOOL(1)
    }
    let mut list: Vec<TopWindow> = Vec::new();
    unsafe {
        EnumWindows(Some(cb), LPARAM(&mut list as *mut _ as isize))
            .map_err(|e| anyhow!("EnumWindows: {e}"))?;
    }
    Ok(list)
}

/// Name (+ title) of the app the user currently has in focus.
pub fn focused_app() -> Result<(String, String)> {
    let hwnd = foreground_hwnd()?;
    let stem = window_exe_stem(hwnd);
    if stem.is_empty() {
        bail!("couldn't resolve the foreground process");
    }
    Ok((friendly_app_name(&stem), window_title(hwnd)))
}

/// Unique running app names with visible windows.
pub fn list_app_names() -> Result<Vec<String>> {
    let mut names: Vec<String> = list_windows()?
        .into_iter()
        .map(|w| w.app_name)
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

fn find_window_for_app(name: &str) -> Result<Option<TopWindow>> {
    let target = name.trim().to_ascii_lowercase();
    let wins = list_windows()?;
    // Best match: exact app-name, then exe stem, then title substring.
    let exact = wins
        .iter()
        .find(|w| w.app_name.to_ascii_lowercase() == target || w.exe_stem.to_ascii_lowercase() == target);
    if let Some(w) = exact {
        return Ok(Some(w.clone()));
    }
    Ok(wins
        .iter()
        .find(|w| {
            w.app_name.to_ascii_lowercase().contains(&target)
                || w.title.to_ascii_lowercase().contains(&target)
        })
        .cloned())
}

/// Bring an app's window to the foreground; launch it if not running.
/// Returns the method used ("existing_window" | "launched").
pub fn focus_app(name: &str) -> Result<String> {
    if let Some(w) = find_window_for_app(name)? {
        let hwnd = HWND(w.hwnd as *mut _);
        unsafe {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            // SetForegroundWindow can be refused when we're a background
            // process; a synthetic key press (which enigo already sends
            // during flows) usually earns the right. Try it plainly first.
            let _ = SetForegroundWindow(hwnd);
        }
        return Ok("existing_window".into());
    }

    // Not running — try to launch by name via `start` (resolves App Paths
    // entries like "chrome", "msedge", "code" and anything on PATH).
    let candidates = launch_candidates(name);
    for cand in &candidates {
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "start", "", cand]);
        crate::platform::hide_console(&mut cmd);
        if let Ok(st) = cmd.status() {
            if st.success() {
                return Ok("launched".into());
            }
        }
    }
    bail!(
        "couldn't find or launch '{name}' — no matching window, and `start` failed for {candidates:?}. \
         Ask the user for the app's exact name."
    )
}

/// Map friendly names to launchable commands (App Paths / PATH names).
fn launch_candidates(name: &str) -> Vec<String> {
    let n = name.trim().to_ascii_lowercase();
    let mut cands: Vec<String> = Vec::new();
    let mapped: &[&str] = match n.as_str() {
        "google chrome" | "chrome" => &["chrome"],
        "microsoft edge" | "edge" => &["msedge"],
        "firefox" => &["firefox"],
        "brave" | "brave browser" => &["brave"],
        "visual studio code" | "vs code" | "vscode" => &["code"],
        "file explorer" | "explorer" | "finder" => &["explorer"],
        "notepad" => &["notepad"],
        "terminal" | "windows terminal" => &["wt"],
        "command prompt" => &["cmd"],
        "spotify" => &["spotify"],
        "slack" => &["slack"],
        "discord" => &["discord"],
        "whatsapp" => &["whatsapp"],
        _ => &[],
    };
    cands.extend(mapped.iter().map(|s| s.to_string()));
    // Also try the raw name — `start` searches App Paths and PATH itself.
    cands.push(name.trim().to_string());
    cands.dedup();
    cands
}

/// Graceful close: WM_CLOSE to every top-level window of the app (apps may
/// prompt to save — same semantics as macOS `tell app to quit`).
pub fn quit_app(name: &str) -> Result<()> {
    let target = name.trim().to_ascii_lowercase();
    let wins = list_windows()?;
    let mut n = 0;
    for w in wins {
        if w.app_name.to_ascii_lowercase() == target
            || w.exe_stem.to_ascii_lowercase() == target
            || w.app_name.to_ascii_lowercase().contains(&target)
        {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(w.hwnd as *mut _)),
                    WM_CLOSE,
                    Default::default(),
                    Default::default(),
                );
            }
            n += 1;
        }
    }
    if n == 0 {
        bail!("no running window matched '{name}'");
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────
// Browser context (URL + title + visible text) via UI Automation
// ──────────────────────────────────────────────────────────────────────────

/// Exe stems we recognize as browsers.
fn browser_display(exe_stem: &str) -> Option<&'static str> {
    Some(match exe_stem.to_ascii_lowercase().as_str() {
        "chrome" => "Google Chrome",
        "msedge" => "Microsoft Edge",
        "brave" => "Brave Browser",
        "vivaldi" => "Vivaldi",
        "opera" | "opera_gx" => "Opera",
        "arc" => "Arc",
        "firefox" => "Firefox",
        "chromium" => "Chromium",
        _ => return None,
    })
}

pub struct BrowserContext {
    pub browser: String,
    pub url: String,
    pub title: String,
    pub text: String,
}

/// Read the focused browser's active tab: URL from the UIA address bar,
/// title from the window title, page text from the UIA Document element.
/// Returns Ok(None) when the foreground app isn't a recognized browser.
pub fn browser_context() -> Result<Option<BrowserContext>> {
    let hwnd = foreground_hwnd()?;
    let stem = window_exe_stem(hwnd);
    let Some(browser) = browser_display(&stem) else {
        return Ok(None);
    };

    let automation = UIAutomation::new().map_err(ui_err)?;
    let root = automation
        .element_from_handle(Handle::from(hwnd.0 as isize))
        .map_err(ui_err)?;

    // URL: the first Edit control whose value looks like a URL — in Chrome /
    // Edge / Brave / Firefox that's the omnibox ("Address and search bar").
    let mut url = String::new();
    let edit_cond = automation
        .create_property_condition(
            UIProperty::ControlType,
            Variant::from(ControlType::Edit as i32),
            None,
        )
        .map_err(ui_err)?;
    if let Ok(edits) = root.find_all(TreeScope::Subtree, &edit_cond) {
        for e in edits.into_iter().take(8) {
            if let Ok(vp) = e.get_pattern::<uiautomation::patterns::UIValuePattern>() {
                if let Ok(v) = vp.get_value() {
                    let v = v.trim().to_string();
                    if looks_like_url(&v) {
                        url = normalize_url(&v);
                        break;
                    }
                }
            }
        }
    }

    // Title: the window title minus the browser suffix (" - Google Chrome").
    let mut title = window_title(hwnd);
    for sep in [" - ", " — "] {
        if let Some(idx) = title.rfind(sep) {
            let tail = &title[idx + sep.len()..];
            if tail.to_ascii_lowercase().contains(&stem.to_ascii_lowercase())
                || tail.contains("Chrome")
                || tail.contains("Edge")
                || tail.contains("Firefox")
                || tail.contains("Brave")
                || tail.contains("Opera")
                || tail.contains("Vivaldi")
            {
                title = title[..idx].to_string();
            }
            break;
        }
    }

    // Page text: the Document element's text (TextPattern), capped at ~4000.
    let mut text = String::new();
    let doc_cond = automation
        .create_property_condition(
            UIProperty::ControlType,
            Variant::from(ControlType::Document as i32),
            None,
        )
        .map_err(ui_err)?;
    if let Ok(doc) = root.find_first(TreeScope::Subtree, &doc_cond) {
        if let Ok(tp) = doc.get_pattern::<uiautomation::patterns::UITextPattern>() {
            if let Ok(range) = tp.get_document_range() {
                if let Ok(t) = range.get_text(4000) {
                    text = t;
                }
            }
        }
        if text.trim().is_empty() {
            // Fallback: concatenate the names of text-ish descendants.
            text = collect_text_names(&automation, &doc, 4000).unwrap_or_default();
        }
    }

    Ok(Some(BrowserContext {
        browser: browser.to_string(),
        url,
        title: title.trim().to_string(),
        text: text.trim().to_string(),
    }))
}

fn collect_text_names(
    automation: &UIAutomation,
    scope: &uiautomation::UIElement,
    cap: usize,
) -> Result<String> {
    let text_cond = automation
        .create_property_condition(
            UIProperty::ControlType,
            Variant::from(ControlType::Text as i32),
            None,
        )
        .map_err(ui_err)?;
    let mut out = String::new();
    if let Ok(texts) = scope.find_all(TreeScope::Subtree, &text_cond) {
        for t in texts {
            if let Ok(name) = t.get_name() {
                let name = name.trim();
                if !name.is_empty() {
                    out.push_str(name);
                    out.push(' ');
                    if out.len() >= cap {
                        break;
                    }
                }
            }
        }
    }
    out.truncate(cap);
    Ok(out)
}

fn looks_like_url(v: &str) -> bool {
    if v.is_empty() || v.contains(' ') || v.len() > 2048 {
        return false;
    }
    v.starts_with("http://")
        || v.starts_with("https://")
        || v.starts_with("file://")
        || v.starts_with("about:")
        || v.starts_with("chrome://")
        || v.starts_with("edge://")
        // Omniboxes often hide the scheme ("github.com/foo").
        || v.split('/').next().is_some_and(|host| host.contains('.') && !host.ends_with('.'))
}

fn normalize_url(v: &str) -> String {
    if v.contains("://") || v.starts_with("about:") || v.starts_with("chrome://") || v.starts_with("edge://") {
        v.to_string()
    } else {
        format!("https://{v}")
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Timezone offset (replaces libc tm_gmtoff on unix)
// ──────────────────────────────────────────────────────────────────────────

/// Current UTC offset in seconds (DST-aware).
pub fn tz_offset_seconds() -> i32 {
    use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    // GetTimeZoneInformation return codes (winbase.h): 1 = standard, 2 = daylight.
    const TIME_ZONE_ID_DAYLIGHT: u32 = 2;
    unsafe {
        let mut tzi = TIME_ZONE_INFORMATION::default();
        let id = GetTimeZoneInformation(&mut tzi);
        // Bias is in minutes, and UTC = local + Bias → local offset = -Bias.
        let mut bias = tzi.Bias;
        if id == TIME_ZONE_ID_DAYLIGHT {
            bias += tzi.DaylightBias;
        } else {
            bias += tzi.StandardBias;
        }
        -bias * 60
    }
}
