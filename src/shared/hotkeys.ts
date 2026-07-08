/**
 * Platform-aware hotkey labels for the UI. The backend registers Cmd+Shift
 * on macOS and Ctrl+Alt on Windows/Linux (see lib.rs); these labels must
 * mirror that so on-screen hints match what actually works.
 */
const IS_MAC = /mac/i.test(navigator.platform || navigator.userAgent);

/** e.g. "⌘⇧" on macOS, "Ctrl+Alt+" elsewhere — prefix for a key letter. */
export const MOD_PREFIX = IS_MAC ? "⌘⇧" : "Ctrl+Alt+";

export const HK_TOGGLE = `${MOD_PREFIX}J`;
export const HK_APPROVE = `${MOD_PREFIX}Y`;
export const HK_DENY = `${MOD_PREFIX}S`;
