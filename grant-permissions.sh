#!/usr/bin/env bash
# One-shot permission warmup for Jarvis.
#
# Run this ONCE while you have admin access. It triggers every macOS TCC
# prompt that can be triggered programmatically, and opens each System
# Settings pane for the ones that require manually dragging the binary in.
#
# When a prompt appears: click "OK" / "Allow".
# When a System Settings pane opens: click the "+" button, navigate to the
# Jarvis binary (path printed at the bottom), and add it.

set -u  # do NOT use -e — we want to keep going even if one trigger fails

PROJECT_ROOT="$(cd "$(dirname "$0")" && pwd)"
DEV_BIN="$PROJECT_ROOT/src-tauri/target/debug/jarvis"
RELEASE_APP="$PROJECT_ROOT/src-tauri/target/release/bundle/macos/Jarvis.app"

# Stable code-signing identity used by both `tauri dev` and `tauri build`.
# Keep in sync with tauri.conf.json > bundle > macOS > signingIdentity.
# A self-signed cert with this Common Name must exist in the login keychain
# (Keychain Access → Certificate Assistant → Create a Certificate, type =
# Self Signed Root, Certificate Type = Code Signing). TCC keys grants against
# this identity's fingerprint, so rebuilds keep every permission.
SIGNING_IDENTITY="${JARVIS_SIGNING_IDENTITY:-Jarvis Code Signing}"

echo "──────────────────────────────────────────────────────────────"
echo "Jarvis permission warmup"
echo "──────────────────────────────────────────────────────────────"
echo ""

# ── Verify the signing identity exists before doing anything ───────────────
if ! security find-identity -v -p codesigning | grep -q "\"$SIGNING_IDENTITY\""; then
  echo "✗ No code-signing identity named \"$SIGNING_IDENTITY\" found in keychain."
  echo ""
  echo "  Create one (one-time setup):"
  echo "    1. Open Keychain Access"
  echo "    2. Menu: Keychain Access → Certificate Assistant → Create a Certificate"
  echo "    3. Name: $SIGNING_IDENTITY"
  echo "       Identity Type: Self Signed Root"
  echo "       Certificate Type: Code Signing"
  echo "    4. Click Create, then Done"
  echo ""
  echo "  Then re-run this script."
  exit 1
fi
echo "✓ Using signing identity: $SIGNING_IDENTITY"
echo ""

# ── Stable signature for dev binary ────────────────────────────────────────
# Sign with the named identity so TCC grants survive rebuilds.
if [[ -x "$DEV_BIN" ]]; then
  echo "→ Signing dev binary with \"$SIGNING_IDENTITY\"…"
  codesign --force --sign "$SIGNING_IDENTITY" --options runtime "$DEV_BIN" 2>/dev/null \
    && echo "  ✓ signed: $DEV_BIN" \
    || echo "  ⚠ codesign failed (non-fatal)"
  echo ""
fi

if [[ -d "$RELEASE_APP" ]]; then
  echo "→ Signing release bundle with \"$SIGNING_IDENTITY\"…"
  codesign --force --deep --sign "$SIGNING_IDENTITY" "$RELEASE_APP" 2>/dev/null \
    && echo "  ✓ signed: $RELEASE_APP" \
    || echo "  ⚠ codesign failed (non-fatal)"
  echo ""
fi

# ── Auto-prompt category: macOS pops a dialog on first attempt ─────────────
echo "→ Triggering Screen Recording prompt (screencapture)…"
screencapture -x /tmp/jarvis-warmup.png 2>/dev/null
rm -f /tmp/jarvis-warmup.png
echo "  ✓ if a prompt appeared, click 'Open System Settings' → Allow"
echo ""

echo "→ Triggering Automation prompts (osascript → System Events, Finder, browsers)…"
for app in "System Events" "Finder" "Google Chrome" "Safari" "Arc" "Brave Browser" "Microsoft Edge"; do
  osascript -e "tell application \"$app\" to get name" >/dev/null 2>&1 \
    && echo "  ✓ prompted/granted: $app" \
    || echo "  · skipped (not running/installed): $app"
done
echo ""

echo "→ Triggering AppleScript JavaScript-from-Apple-Events (browsers)…"
# Chromium-family — only fires if the browser has a front window
for app in "Google Chrome" "Arc" "Brave Browser" "Microsoft Edge"; do
  osascript -e "tell application \"$app\" to tell active tab of front window to execute javascript \"1\"" >/dev/null 2>&1 \
    && echo "  ✓ JS-from-AE: $app" \
    || echo "  · skipped: $app (not open, or JS-from-AE disabled in browser settings)"
done
# Safari has its own toggle — Develop menu → Allow JavaScript from Apple Events
osascript -e 'tell application "Safari" to do JavaScript "1" in front document' >/dev/null 2>&1 \
  && echo "  ✓ JS-from-AE: Safari" \
  || echo "  · skipped: Safari (enable Develop → Allow JavaScript from Apple Events)"
echo ""

# ── Manual-add category: macOS does NOT prompt; user must drag binary in ───
echo "──────────────────────────────────────────────────────────────"
echo "MANUAL STEPS — opening System Settings panes one by one."
echo "In each pane: click '+', then drag in the Jarvis binary printed below."
echo "──────────────────────────────────────────────────────────────"
echo ""

open_pane () {
  local label="$1" anchor="$2"
  echo "→ Opening: $label"
  open "x-apple.systempreferences:com.apple.preference.security?$anchor"
  sleep 1.5
}

open_pane "Accessibility"      "Privacy_Accessibility"
open_pane "Input Monitoring"   "Privacy_ListenEvent"
open_pane "Full Disk Access"   "Privacy_AllFiles"
open_pane "Screen Recording"   "Privacy_ScreenCapture"
open_pane "Automation"         "Privacy_Automation"
open_pane "Microphone"         "Privacy_Microphone"
open_pane "Camera"             "Privacy_Camera"
open_pane "Contacts"           "Privacy_Contacts"
open_pane "Calendars"          "Privacy_Calendars"
open_pane "Reminders"          "Privacy_Reminders"
open_pane "Photos"             "Privacy_Photos"
open_pane "Location Services"  "Privacy_LocationServices"
open_pane "Bluetooth"          "Privacy_Bluetooth"
open_pane "Local Network"      "Privacy_LocalNetwork"
open_pane "Files and Folders"  "Privacy_FilesAndFolders"

echo ""
echo "──────────────────────────────────────────────────────────────"
echo "Drag THIS binary into each pane that has a + button:"
echo ""
echo "  Dev build:     $DEV_BIN"
[[ -d "$RELEASE_APP" ]] && echo "  Release .app:  $RELEASE_APP"
echo ""
echo "Tip: in Finder, press Cmd+Shift+G and paste the path to jump to it."
echo "──────────────────────────────────────────────────────────────"
