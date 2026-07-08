//! Tiny cross-platform helpers shared by the OS-specific code paths.
//!
//! Keep this module dependency-light and boring: home dir resolution,
//! process kill, and console-window suppression for child processes on
//! Windows (a GUI app spawning a console child otherwise flashes a window).

/// The user's home directory. `HOME` on unix; `USERPROFILE` on Windows
/// (with `HOME` honored if set, e.g. under Git Bash / MSYS environments).
pub fn home_dir() -> String {
    if let Ok(h) = std::env::var("HOME") {
        if !h.trim().is_empty() {
            return h;
        }
    }
    if let Ok(h) = std::env::var("USERPROFILE") {
        if !h.trim().is_empty() {
            return h;
        }
    }
    ".".to_string()
}

/// Expand a leading `~/` (or bare `~`) to the home directory. Accepts `~\`
/// too on Windows.
pub fn expand_home(p: &str) -> String {
    if p == "~" {
        return home_dir();
    }
    if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
        let home = home_dir();
        return format!("{home}{}{rest}", std::path::MAIN_SEPARATOR);
    }
    p.to_string()
}

/// Hard-kill a process by pid. SIGKILL on unix; TerminateProcess on Windows.
#[cfg(unix)]
pub fn kill_pid(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

#[cfg(windows)]
pub fn kill_pid(pid: u32) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) {
            let _ = TerminateProcess(h, 1);
            let _ = CloseHandle(h);
        }
    }
}

/// CREATE_NO_WINDOW — stops console children (cmd/powershell/npx) from
/// flashing a console window when Jarvis runs as a GUI app.
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Configure a std Command so it doesn't open a console window on Windows.
/// No-op elsewhere.
pub fn hide_console(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

/// Same, for tokio Commands (the MCP child processes).
pub fn hide_console_tokio(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_home_handles_tilde_forms() {
        let home = home_dir();
        assert_eq!(expand_home("~"), home);
        let expanded = expand_home("~/x");
        assert!(expanded.starts_with(&home) && expanded.ends_with('x'));
        assert_eq!(expand_home("/abs/path"), "/abs/path");
    }
}
