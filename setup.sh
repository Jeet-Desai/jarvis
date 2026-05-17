#!/usr/bin/env bash
# Jarvis setup — checks prereqs, downloads the whisper model, prepares .env.
# Run once after `git clone`. Idempotent — safe to re-run.

set -e

cd "$(dirname "$0")"

echo "──────────────────────────────────────────────────────────────"
echo " Jarvis · setup"
echo "──────────────────────────────────────────────────────────────"

# ── 1. macOS check ─────────────────────────────────────────────────────────
if [[ "$(uname)" != "Darwin" ]]; then
  echo "❌  v1 is macOS-only. (cross-platform is on the roadmap.)"
  exit 1
fi
echo "✓ macOS detected"

# ── 2. Xcode CLI tools ─────────────────────────────────────────────────────
if ! xcode-select -p >/dev/null 2>&1; then
  echo "❌  Xcode Command Line Tools missing."
  echo "    Run:  xcode-select --install"
  echo "    Then re-run this script."
  exit 1
fi
echo "✓ Xcode CLI tools"

# ── 3. Homebrew ────────────────────────────────────────────────────────────
if ! command -v brew >/dev/null 2>&1; then
  echo "❌  Homebrew missing. Install from https://brew.sh, then re-run."
  exit 1
fi
echo "✓ Homebrew"

# ── 4. cmake (for whisper.cpp build) ───────────────────────────────────────
if ! command -v cmake >/dev/null 2>&1; then
  echo "→ Installing cmake (needed by whisper-rs build)…"
  brew install cmake
fi
echo "✓ cmake"

# ── 5. Rust toolchain ──────────────────────────────────────────────────────
if ! command -v cargo >/dev/null 2>&1; then
  echo "→ Installing Rust via rustup…"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
  source "$HOME/.cargo/env"
fi
echo "✓ Rust ($(rustc --version 2>/dev/null | head -c 24)…)"

# ── 6. Node + pnpm ─────────────────────────────────────────────────────────
if ! command -v node >/dev/null 2>&1; then
  echo "→ Installing Node via Homebrew…"
  brew install node
fi
if ! command -v pnpm >/dev/null 2>&1; then
  echo "→ Installing pnpm…"
  npm install -g pnpm
fi
echo "✓ Node $(node --version), pnpm $(pnpm --version)"

# ── 7. Frontend deps ───────────────────────────────────────────────────────
echo "→ Installing frontend deps (pnpm install)…"
pnpm install >/dev/null
echo "✓ frontend deps"

# ── 8. Whisper model ───────────────────────────────────────────────────────
MODEL_DIR="models"
MODEL_FILE="$MODEL_DIR/ggml-base.en.bin"
MODEL_URL="https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin"
mkdir -p "$MODEL_DIR"
if [[ ! -f "$MODEL_FILE" ]]; then
  echo "→ Downloading whisper base.en model (~140 MB)…"
  curl -L --fail --progress-bar -o "$MODEL_FILE" "$MODEL_URL"
fi
echo "✓ whisper model ($(du -sh "$MODEL_FILE" | cut -f1))"

# ── 9. .env ────────────────────────────────────────────────────────────────
if [[ ! -f .env ]]; then
  cp .env.example .env
  echo "✓ created .env (from .env.example)"
  echo ""
  echo "🔑 NEXT STEP: open .env and add at least one LLM provider key."
  echo "   The fastest path is Gemini (free): https://aistudio.google.com/apikey"
else
  echo "✓ .env already exists (leaving it untouched)"
fi

# ── 10. Optional: Ollama check ─────────────────────────────────────────────
if command -v ollama >/dev/null 2>&1; then
  echo "✓ Ollama installed (Jarvis will auto-detect it if you run \`ollama serve\`)"
else
  echo "ℹ Ollama not installed (optional fallback brain)."
  echo "  Install:  brew install ollama"
  echo "  Then:     ollama pull qwen2.5:7b && ollama serve"
fi

echo ""
echo "──────────────────────────────────────────────────────────────"
echo " Setup complete."
echo "──────────────────────────────────────────────────────────────"
echo ""
echo " Next:"
echo "   1. Fill in at least one LLM provider key in .env"
echo "   2. Run:  NO_COLOR=1 pnpm tauri dev   # NO_COLOR keeps build output clean"
echo "   3. Grant the microphone permission when macOS prompts"
echo "   4. Say \"Jarvis, what time is it?\""
echo ""
