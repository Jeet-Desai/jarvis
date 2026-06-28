//! Configurable safety boundaries for autonomous computer control.
//!
//! This is the spine of JARVIS 2.0's "take over my PC" capability. Every tool
//! carries a [`DangerLevel`]; before a tool runs, `tools::dispatch` asks the
//! global [`Policy`] for a [`Decision`] — `Allow`, `Confirm`, or `Deny`. The
//! engine is PURE (no Tauri, no async, no I/O beyond loading its config file)
//! so it is trivially portable to Windows and easy to reason about.
//!
//! Configuration precedence (later wins):
//!   1. compiled-in defaults (sensible, safe-by-default)
//!   2. `policy.toml` in the app config dir (auto-written on first run)
//!   3. environment overrides: `JARVIS_POLICY_MODE`, `JARVIS_UNRESTRICTED=1`
//!
//! Design notes:
//!   - The ALLOWLIST is the trust boundary. The destructive-command DENYLIST
//!     is an explicitly-advisory *second* layer — denylists are evadable
//!     (base64, subshells, write-then-exec), so confirmation is the real gate.
//!   - Prompt-injection guard: any mutating/destructive tool issued in a turn
//!     that already read UNTRUSTED content (a web page, the clipboard, the
//!     screen) is force-confirmed, because on-screen / web text is DATA, never
//!     instructions. See `tools::turn_is_tainted`.

use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::OnceLock;

// ──────────────────────────────────────────────────────────────────────────
// Danger classification
// ──────────────────────────────────────────────────────────────────────────

/// How risky a tool is. Set on every `tools::Tool` in the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DangerLevel {
    /// Observes only — no side effects on the machine (read clipboard, see
    /// screen, read UI tree, list calendar, get time).
    ReadOnly,
    /// Changes app or system state; generally recoverable (type text, click,
    /// focus an app, press a key, set clipboard, create a calendar event).
    Mutating,
    /// Irreversible or high blast-radius (run a shell command, delete/overwrite
    /// a file, send a message, submit a form, quit an app with unsaved work).
    Destructive,
}

impl DangerLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            DangerLevel::ReadOnly => "read_only",
            DangerLevel::Mutating => "mutating",
            DangerLevel::Destructive => "destructive",
        }
    }
}

/// Operating posture for the policy engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Read-only auto; everything that mutates the machine asks first.
    #[default]
    Safe,
    /// Read-only + mutations into allow-listed apps run silently; anything
    /// else (other apps, destructive) asks first. The everyday driving mode.
    Trusted,
    /// Owner / unrestricted: everything runs, audited, with a loud warning.
    /// This is the "do whatever, it's my machine" developer posture.
    Dev,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Safe => "safe",
            Mode::Trusted => "trusted",
            Mode::Dev => "dev",
        }
    }
    fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_lowercase().as_str() {
            "safe" => Some(Mode::Safe),
            "trusted" => Some(Mode::Trusted),
            "dev" | "unrestricted" | "owner" => Some(Mode::Dev),
            _ => None,
        }
    }
}

/// The verdict for one tool call.
#[derive(Debug, Clone)]
pub enum Decision {
    /// Run it now, no prompt.
    Allow,
    /// Ask the user first. `summary` is spoken / shown.
    Confirm { summary: String, danger: DangerLevel },
    /// Refuse. `reason` is returned to the model and the user.
    Deny { reason: String },
}

// ──────────────────────────────────────────────────────────────────────────
// Policy config
// ──────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub mode: Mode,
    /// App-name globs that may be mutated without a prompt in `trusted` mode.
    pub allowed_apps: Vec<String>,
    /// App-name globs that are never allowed (hard deny in every mode).
    pub denied_apps: Vec<String>,
    /// Path prefixes/globs the agent may touch (empty = no allowlist restriction).
    pub allowed_paths: Vec<String>,
    /// Path prefixes/globs that are off-limits (credentials, keys). Deny in
    /// safe/trusted, force-confirm in dev.
    pub denied_paths: Vec<String>,
    /// Substring/regex fragments that mark a shell command dangerous. Advisory
    /// second layer — Deny in safe/trusted, force-confirm in dev.
    pub denied_command_patterns: Vec<String>,
    /// How long to wait for a confirmation before defaulting to Deny.
    pub confirm_timeout_secs: u64,
    /// If true, even `ReadOnly` tools are announced in the activity feed.
    pub verbose_activity: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            mode: Mode::Safe,
            allowed_apps: vec![],
            denied_apps: vec![],
            allowed_paths: vec![],
            denied_paths: default_denied_paths(),
            denied_command_patterns: default_denied_commands(),
            confirm_timeout_secs: 45,
            verbose_activity: true,
        }
    }
}

/// Credential / secret locations the agent must never read or clobber by
/// default. Home-relative; matched against absolute paths after `~` expansion.
fn default_denied_paths() -> Vec<String> {
    [
        "~/.ssh",
        "~/.aws",
        "~/.gnupg",
        "~/.config/gcloud",
        "~/.kube",
        "~/.netrc",
        "~/.npmrc",
        "~/Library/Keychains",
        "~/Library/Application Support/com.apple.TCC",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Shell-command fragments that scream "irreversible damage". ADVISORY only —
/// confirmation is the real gate; this just refuses the obvious foot-guns.
fn default_denied_commands() -> Vec<String> {
    [
        r"rm\s+-[a-z]*r[a-z]*f",   // rm -rf / -fr / -Rf …
        r"rm\s+-[a-z]*f[a-z]*r",
        r"\bmkfs",
        r"\bdiskutil\s+(erase|reformat|partition)",
        r"\bdd\s+.*of=/dev/",
        r">\s*/dev/(disk|rdisk|sd)",
        r":\(\)\s*\{.*\};:",       // fork bomb
        r"\bshutdown\b",
        r"\breboot\b",
        r"\bsudo\b",
        r"curl\s+.*\|\s*(sh|bash|zsh)",
        r"wget\s+.*\|\s*(sh|bash|zsh)",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// HARD protected-paths rule (always on, every mode, not config-overridable).
/// This project (Jarvis) is for the user's PERSONAL work only — it must never
/// read, write, or run against the user's protected files. We block anything
/// whose path args OR (for `run_shell`) command text references a forbidden
/// marker.
/// Markers are supplied via `JARVIS_PROTECTED_PATHS` (comma-separated); there are
/// no built-in names.
fn protected_path_guard(tool: &str, args: &Value) -> Option<String> {
    let mut needles: Vec<String> = Vec::new();
    if let Ok(extra) = std::env::var("JARVIS_PROTECTED_PATHS") {
        needles.extend(
            extra
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty()),
        );
    }

    let mut hay: Vec<String> = Vec::new();
    for key in ["path", "workdir", "dir", "app_name", "project_dir"] {
        if let Some(v) = arg_str(args, key) {
            hay.push(v);
        }
    }
    if let Some(arr) = args.get("paths").and_then(|v| v.as_array()) {
        hay.extend(arr.iter().filter_map(|v| v.as_str().map(String::from)));
    }
    if tool == "run_shell" {
        if let Some(c) = arg_str(args, "command") {
            hay.push(c);
        }
    }

    for h in &hay {
        let lc = h.to_lowercase();
        if let Some(n) = needles.iter().find(|n| lc.contains(n.as_str())) {
            return Some(format!(
                "Refused: this project must never touch protected files (matched \"{n}\"). \
                 That is a hard rule in every mode — use a separate Claude Code session for those."
            ));
        }
    }
    None
}

static GLOBAL: OnceLock<Policy> = OnceLock::new();

/// The active policy. Falls back to `Policy::default()` if `init` was never
/// called (e.g. during unit tests) so callers never see `None`.
pub fn global() -> &'static Policy {
    GLOBAL.get_or_init(Policy::default)
}

/// Load policy from `config_dir/policy.toml` (writing a documented default if
/// absent), then apply env overrides. Idempotent; call once at startup.
pub fn init(config_dir: Option<PathBuf>) {
    let mut policy = match config_dir {
        Some(dir) => load_or_create(&dir),
        None => Policy::default(),
    };
    apply_env_overrides(&mut policy);
    tracing::info!(
        "policy: mode={} allowed_apps={} denied_paths={} denied_cmds={}",
        policy.mode.as_str(),
        policy.allowed_apps.len(),
        policy.denied_paths.len(),
        policy.denied_command_patterns.len(),
    );
    if policy.mode == Mode::Dev {
        tracing::warn!(
            "policy: DEV / UNRESTRICTED mode — every tool runs WITHOUT confirmation. \
             Set JARVIS_POLICY_MODE=safe to re-enable guardrails."
        );
    }
    let _ = GLOBAL.set(policy);
}

fn load_or_create(config_dir: &PathBuf) -> Policy {
    let path = config_dir.join("policy.toml");
    if let Ok(raw) = std::fs::read_to_string(&path) {
        match toml::from_str::<Policy>(&raw) {
            Ok(p) => {
                tracing::info!("policy: loaded {}", path.display());
                return p;
            }
            Err(e) => {
                tracing::warn!("policy: {} is invalid ({e}); using defaults", path.display());
                return Policy::default();
            }
        }
    }
    // First run — write a documented default so the user can edit it.
    let _ = std::fs::create_dir_all(config_dir);
    if let Err(e) = std::fs::write(&path, DEFAULT_POLICY_TOML) {
        tracing::warn!("policy: couldn't write default {} ({e})", path.display());
    } else {
        tracing::info!("policy: wrote default {}", path.display());
    }
    Policy::default()
}

fn apply_env_overrides(policy: &mut Policy) {
    if std::env::var("JARVIS_UNRESTRICTED").ok().as_deref() == Some("1") {
        policy.mode = Mode::Dev;
    }
    if let Ok(m) = std::env::var("JARVIS_POLICY_MODE") {
        if let Some(mode) = Mode::parse(&m) {
            policy.mode = mode;
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// The decision engine
// ──────────────────────────────────────────────────────────────────────────

impl Policy {
    /// The core gate. `tainted` = this turn already ingested untrusted content
    /// (web/clipboard/screen), so a mutation could be prompt-injection-driven.
    pub fn decide(
        &self,
        tool: &str,
        danger: DangerLevel,
        args: &Value,
        tainted: bool,
    ) -> Decision {
        // 0) PROTECTED-PATHS HARD RULE — this project must NEVER touch the
        //    user's protected files, in ANY mode (not even dev), and this
        //    cannot be softened or disabled via config. Jarvis is used on
        //    personal work only; that work uses a separate Claude Code
        //    session. See `protected_path_guard`.
        if let Some(reason) = protected_path_guard(tool, args) {
            return Decision::Deny { reason };
        }

        // 1) Hard denials, evaluated before mode. In dev these soften to a
        //    confirm so the owner is never fully blocked, just slowed down.
        if let Some(reason) = self.app_hard_denied(args) {
            return Decision::Deny { reason };
        }
        if let Some(reason) = self.path_denied(args) {
            return match self.mode {
                Mode::Dev => Decision::Confirm {
                    summary: format!("{reason} — proceed anyway?"),
                    danger: DangerLevel::Destructive,
                },
                _ => Decision::Deny { reason },
            };
        }
        if let Some(reason) = self.command_denied(tool, args) {
            return match self.mode {
                Mode::Dev => Decision::Confirm {
                    summary: format!("{reason} — proceed anyway?"),
                    danger: DangerLevel::Destructive,
                },
                _ => Decision::Deny { reason },
            };
        }

        // 2) Mode + danger matrix.
        match self.mode {
            Mode::Dev => Decision::Allow,
            Mode::Trusted => match danger {
                DangerLevel::ReadOnly => Decision::Allow,
                DangerLevel::Mutating => {
                    if !tainted && self.app_allowed(args) {
                        Decision::Allow
                    } else {
                        Decision::Confirm {
                            summary: self.summarize(tool, args, tainted),
                            danger,
                        }
                    }
                }
                DangerLevel::Destructive => Decision::Confirm {
                    summary: self.summarize(tool, args, tainted),
                    danger,
                },
            },
            Mode::Safe => match danger {
                DangerLevel::ReadOnly => Decision::Allow,
                _ => Decision::Confirm {
                    summary: self.summarize(tool, args, tainted),
                    danger,
                },
            },
        }
    }

    /// Human-readable one-liner for the confirmation prompt.
    fn summarize(&self, tool: &str, args: &Value, tainted: bool) -> String {
        let base = describe(tool, args);
        if tainted {
            format!("{base} (based on untrusted on-screen/web content)")
        } else {
            base
        }
    }

    fn app_hard_denied(&self, args: &Value) -> Option<String> {
        let app = arg_str(args, "app_name")?;
        if self.denied_apps.iter().any(|g| glob_match(g, &app)) {
            Some(format!("'{app}' is on the denied-apps list"))
        } else {
            None
        }
    }

    fn app_allowed(&self, args: &Value) -> bool {
        // No app arg → not app-scoped → treat as allowed for the allowlist gate.
        let Some(app) = arg_str(args, "app_name") else {
            return true;
        };
        self.allowed_apps.is_empty() || self.allowed_apps.iter().any(|g| glob_match(g, &app))
    }

    fn path_denied(&self, args: &Value) -> Option<String> {
        let mut paths: Vec<String> = Vec::new();
        if let Some(p) = arg_str(args, "path") {
            paths.push(p);
        }
        if let Some(arr) = args.get("paths").and_then(|v| v.as_array()) {
            paths.extend(arr.iter().filter_map(|v| v.as_str().map(String::from)));
        }
        for p in paths {
            // (1) Expand ~, then make ABSOLUTE: a relative path like
            //     `../.ssh/id_rsa` resolves against Jarvis's cwd at fs-time, so
            //     we must resolve it the same way before the prefix check.
            let expanded = expand_home(&p);
            let pb = std::path::PathBuf::from(&expanded);
            let abs_buf = if pb.is_absolute() {
                pb
            } else {
                std::env::current_dir().map(|c| c.join(&pb)).unwrap_or(pb)
            };
            // (2) Resolve `.`/`..` (and symlinks, when the path exists) before
            //     comparing, so `~/.ssh-backup/../ssh/id_rsa` or a symlink can't
            //     slip past and hit a protected location at runtime.
            let norm = std::fs::canonicalize(&abs_buf)
                .unwrap_or_else(|_| lexical_normalize(&abs_buf.to_string_lossy()));
            for denied in &self.denied_paths {
                let d = expand_home(denied);
                let dn = std::fs::canonicalize(&d).unwrap_or_else(|_| lexical_normalize(&d));
                // (3) Case-insensitive compare on macOS/Windows default volumes,
                //     so `~/.SSH/id_rsa` can't dodge a `~/.ssh` rule.
                if path_starts_with(&norm, &dn) {
                    return Some(format!("'{p}' is in a protected location ({denied})"));
                }
            }
        }
        None
    }

    fn command_denied(&self, tool: &str, args: &Value) -> Option<String> {
        if tool != "run_shell" {
            return None;
        }
        let cmd = arg_str(args, "command")?;
        for pat in &self.denied_command_patterns {
            if regex_lite_match(pat, &cmd) {
                return Some(format!("command matches a blocked pattern (`{pat}`)"));
            }
        }
        None
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Small helpers (no extra deps beyond the already-present `regex`)
// ──────────────────────────────────────────────────────────────────────────

/// A short, human-readable description of a tool call — used for the spoken
/// confirmation prompt AND the HUD activity feed (so both read the same way).
pub fn describe(tool: &str, args: &Value) -> String {
    let detail = match tool {
        "run_shell" => arg_str(args, "command").map(|c| format!("run: {}", clip(&c, 120))),
        "write_file" | "edit_file" | "create_file" => {
            arg_str(args, "path").map(|p| format!("write to {p}"))
        }
        "delete_path" => arg_str(args, "path").map(|p| format!("delete {p}")),
        "read_file" => arg_str(args, "path").map(|p| format!("read {p}")),
        "type_text" => arg_str(args, "text").map(|t| {
            // Deliberately DON'T echo the literal text: this summary is spoken
            // aloud in the confirmation prompt, so repeating it could leak a
            // password or other secret to anyone within earshot.
            let app = arg_str(args, "app_name").unwrap_or_else(|| "the focused app".into());
            let n = t.chars().count();
            format!("type {n} character{} into {app}", if n == 1 { "" } else { "s" })
        }),
        "click_element" => arg_str(args, "label").map(|l| format!("click \u{201c}{l}\u{201d}")),
        "click" => Some(format!(
            "click at ({}, {})",
            arg_str(args, "x").unwrap_or_else(|| "cursor".into()),
            arg_str(args, "y").unwrap_or_else(|| "pos".into())
        )),
        "move_mouse" => Some(format!(
            "move mouse to ({}, {})",
            arg_str(args, "x").unwrap_or_default(),
            arg_str(args, "y").unwrap_or_default()
        )),
        "focus_app" | "open_app" | "quit_app" => {
            arg_str(args, "app_name").map(|a| format!("{}: {a}", tool.replace('_', " ")))
        }
        "send_shortcut" | "press_key" => arg_str(args, "key").map(|k| format!("press {k}")),
        "create_calendar_event" => arg_str(args, "summary").map(|s| format!("add calendar event: {s}")),
        "spawn_dev_agent" => arg_str(args, "task").map(|t| format!("spawn dev agent: {}", clip(&t, 100))),
        "instruct_dev_agent" => arg_str(args, "instruction").map(|i| {
            let who = arg_str(args, "agent_id").unwrap_or_else(|| "agent".into());
            format!("instruct {who}: {}", clip(&i, 80))
        }),
        "stop_dev_agent" => arg_str(args, "agent_id").map(|a| format!("stop dev agent {a}")),
        // Browser control (Jarvis's own Chrome). Deliberately DON'T echo typed
        // text (browser_type) for the same reason as type_text — the summary is
        // spoken aloud and could leak a password.
        "browser_navigate" => arg_str(args, "url").map(|u| format!("browse to {}", clip(&u, 80))),
        "browser_navigate_back" => Some("browser: go back".into()),
        "browser_snapshot" => Some("read the browser page".into()),
        "browser_click" => Some(format!(
            "click {} (browser)",
            arg_str(args, "element").unwrap_or_else(|| "element".into())
        )),
        "browser_type" => Some(format!(
            "type into {} (browser)",
            arg_str(args, "element").unwrap_or_else(|| "a field".into())
        )),
        "browser_press_key" => arg_str(args, "key").map(|k| format!("browser: press {k}")),
        "browser_select_option" => Some(format!(
            "select in {} (browser)",
            arg_str(args, "element").unwrap_or_else(|| "a dropdown".into())
        )),
        "browser_hover" => Some(format!(
            "hover {} (browser)",
            arg_str(args, "element").unwrap_or_else(|| "element".into())
        )),
        "browser_wait_for" => Some("wait in the browser".into()),
        "browser_tabs" => arg_str(args, "action").map(|a| format!("browser tab: {a}")),
        _ => None,
    };
    detail.unwrap_or_else(|| tool.replace('_', " "))
}

/// Pull a string-ish arg, coercing numbers to strings so `x`/`y` work.
fn arg_str(args: &Value, key: &str) -> Option<String> {
    match args.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.replace('\n', " ");
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{}…", truncated.replace('\n', " "))
}

fn expand_home(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    } else if p == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return home;
        }
    }
    p.to_string()
}

/// Component-wise prefix check, case-insensitive on platforms whose default
/// filesystem is case-insensitive (macOS APFS/HFS+, Windows NTFS) so an
/// upper/lower-case variant can't dodge a denied-path rule.
fn path_starts_with(path: &std::path::Path, prefix: &std::path::Path) -> bool {
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        let fold = |p: &std::path::Path| std::path::PathBuf::from(p.to_string_lossy().to_lowercase());
        fold(path).starts_with(fold(prefix))
    } else {
        path.starts_with(prefix)
    }
}

/// Resolve `.`/`..` components without touching the filesystem (so it also
/// normalizes paths that don't exist yet). Used to defeat `..` traversal past
/// the denied-path list.
fn lexical_normalize(p: &str) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for comp in std::path::Path::new(p).components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Tiny case-insensitive glob supporting only `*` wildcards — enough for app
/// names and path fragments without pulling in `globset`.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p = pattern.to_lowercase();
    let t = text.to_lowercase();
    if !p.contains('*') {
        return p == t || t.contains(&p);
    }
    let parts: Vec<&str> = p.split('*').collect();
    let mut idx = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match t[idx..].find(part) {
            Some(found) => {
                // First part must anchor at the start unless pattern began with '*'.
                if i == 0 && found != 0 {
                    return false;
                }
                idx += found + part.len();
            }
            None => return false,
        }
    }
    // Last part must anchor at the end unless pattern ended with '*'.
    if let Some(last) = parts.last() {
        if !last.is_empty() && !p.ends_with('*') {
            return t.ends_with(last);
        }
    }
    true
}

/// Match a command against a denylist pattern using the already-present
/// `regex` crate, case-insensitively. Falls back to substring on bad regex.
fn regex_lite_match(pattern: &str, text: &str) -> bool {
    match regex::RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
    {
        Ok(re) => re.is_match(text),
        Err(_) => text.to_lowercase().contains(&pattern.to_lowercase()),
    }
}

const DEFAULT_POLICY_TOML: &str = r#"# Jarvis safety policy. Controls what the assistant may do on your machine
# without asking. Edit and restart Jarvis to apply. Env vars override this file:
#   JARVIS_POLICY_MODE=safe|trusted|dev      JARVIS_UNRESTRICTED=1  (= dev)

# mode:
#   "safe"    — read-only actions run silently; ANYTHING that changes your
#               machine (typing, clicking, running commands) asks first.
#   "trusted" — read-only + mutations into allowed_apps run silently; other
#               apps and destructive actions still ask. Recommended daily mode.
#   "dev"     — UNRESTRICTED. Everything runs without asking (still audited).
mode = "safe"

# In "trusted" mode, these apps can be typed into / clicked without a prompt.
# Glob '*' supported, matched case-insensitively. Empty = all apps allowed.
allowed_apps = []

# Apps Jarvis may never touch (hard deny in every mode).
denied_apps = []

# Paths Jarvis may read/write. Empty = no extra restriction beyond denied_paths.
allowed_paths = []

# Protected locations (credentials/keys). Denied in safe/trusted, confirmed in dev.
denied_paths = [
  "~/.ssh", "~/.aws", "~/.gnupg", "~/.config/gcloud", "~/.kube",
  "~/.netrc", "~/.npmrc", "~/Library/Keychains",
]

# Shell-command patterns (regex, case-insensitive) treated as dangerous.
# ADVISORY second layer only — confirmation is the real safeguard.
denied_command_patterns = [
  "rm\\s+-[a-z]*r[a-z]*f", "\\bmkfs", "\\bdd\\s+.*of=/dev/",
  ":\\(\\)\\s*\\{.*\\};:", "\\bsudo\\b", "curl\\s+.*\\|\\s*(sh|bash|zsh)",
]

# Seconds to wait for a confirmation before defaulting to "deny".
confirm_timeout_secs = 45

# Announce read-only actions in the activity feed too (not just mutations).
verbose_activity = true
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn safe_mode_gates_mutations() {
        let p = Policy::default();
        assert!(matches!(
            p.decide("get_clipboard", DangerLevel::ReadOnly, &json!({}), false),
            Decision::Allow
        ));
        assert!(matches!(
            p.decide("type_text", DangerLevel::Mutating, &json!({"text":"hi"}), false),
            Decision::Confirm { .. }
        ));
    }

    #[test]
    fn dev_mode_allows_everything() {
        let mut p = Policy::default();
        p.mode = Mode::Dev;
        assert!(matches!(
            p.decide("run_shell", DangerLevel::Destructive, &json!({"command":"ls"}), true),
            Decision::Allow
        ));
    }

    #[test]
    fn protected_paths_hard_denied_even_in_dev() {
        // No built-in names: supply the markers this test exercises.
        std::env::set_var("JARVIS_PROTECTED_PATHS", "private-code,private-projects");
        let mut p = Policy::default();
        p.mode = Mode::Dev; // the most permissive mode — must STILL deny.
        // read_file under private-projects
        assert!(matches!(
            p.decide("read_file", DangerLevel::ReadOnly,
                     &json!({"path": "~/private-projects/secret/Main.java"}), false),
            Decision::Deny { .. }
        ));
        // a run_shell command that references a private-code path
        assert!(matches!(
            p.decide("run_shell", DangerLevel::Destructive,
                     &json!({"command": "cat ~/dev/private-code-frontend/.env"}), false),
            Decision::Deny { .. }
        ));
        // write_file into a private-code dir
        assert!(matches!(
            p.decide("write_file", DangerLevel::Destructive,
                     &json!({"path": "/Users/x/work/private-code/app.ts", "content": "x"}), false),
            Decision::Deny { .. }
        ));
        // a normal personal path is unaffected
        assert!(matches!(
            p.decide("read_file", DangerLevel::ReadOnly,
                     &json!({"path": "~/claude/jarvis-workspace/projects/site/index.html"}), false),
            Decision::Allow
        ));
    }

    #[test]
    fn denied_command_blocked_in_safe() {
        let p = Policy::default();
        match p.decide("run_shell", DangerLevel::Destructive, &json!({"command":"rm -rf /"}), false) {
            Decision::Deny { .. } => {}
            other => panic!("expected deny, got {other:?}"),
        }
    }

    #[test]
    fn tainted_mutation_confirms_in_trusted() {
        let mut p = Policy::default();
        p.mode = Mode::Trusted;
        p.allowed_apps = vec!["*".into()];
        // untainted into an allowed app → allow
        assert!(matches!(
            p.decide("type_text", DangerLevel::Mutating, &json!({"app_name":"Notes","text":"x"}), false),
            Decision::Allow
        ));
        // tainted → confirm even though app is allowed
        assert!(matches!(
            p.decide("type_text", DangerLevel::Mutating, &json!({"app_name":"Notes","text":"x"}), true),
            Decision::Confirm { .. }
        ));
    }

    #[test]
    fn path_traversal_is_blocked() {
        let p = Policy::default(); // safe mode, default denied_paths incl ~/.ssh
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        // `..` traversal that resolves into the protected ~/.ssh must be denied.
        let args = json!({ "path": format!("{home}/.ssh-backup/../.ssh/id_rsa") });
        match p.decide("read_file", DangerLevel::ReadOnly, &args, false) {
            Decision::Deny { .. } => {}
            other => panic!("expected deny for traversal, got {other:?}"),
        }
    }

    #[test]
    fn lexical_normalize_resolves_dotdot() {
        assert_eq!(
            lexical_normalize("/a/b/../c/./d"),
            std::path::PathBuf::from("/a/c/d")
        );
    }

    #[test]
    fn case_variant_path_blocked_on_apple() {
        // On case-insensitive volumes (macOS default), an upper-case variant of
        // a protected dir must still be denied.
        if cfg!(any(target_os = "macos", target_os = "windows")) {
            let p = Policy::default();
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            let args = json!({ "path": format!("{home}/.SSH/id_rsa") });
            match p.decide("read_file", DangerLevel::ReadOnly, &args, false) {
                Decision::Deny { .. } => {}
                other => panic!("expected deny for case variant, got {other:?}"),
            }
        }
    }

    #[test]
    fn path_starts_with_case_insensitive_on_apple() {
        if cfg!(target_os = "macos") {
            assert!(path_starts_with(
                std::path::Path::new("/Users/x/.SSH/id_rsa"),
                std::path::Path::new("/Users/x/.ssh")
            ));
        }
        // case-sensitive direction always holds
        assert!(path_starts_with(
            std::path::Path::new("/a/b/c"),
            std::path::Path::new("/a/b")
        ));
    }

    #[test]
    fn glob_matches() {
        assert!(glob_match("Visual Studio*", "Visual Studio Code"));
        assert!(glob_match("*chrome*", "Google Chrome"));
        assert!(glob_match("Slack", "slack"));
        assert!(!glob_match("Safari", "Google Chrome"));
    }
}
