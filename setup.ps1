# Jarvis setup (Windows) — checks prereqs, downloads models, prepares .env.
# Run once after `git clone` from a PowerShell prompt:
#   powershell -ExecutionPolicy Bypass -File setup.ps1
# Idempotent — safe to re-run.

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

Write-Host "──────────────────────────────────────────────────────────────"
Write-Host " Jarvis · setup (Windows)"
Write-Host "──────────────────────────────────────────────────────────────"

function Test-Cmd($name) {
    return [bool](Get-Command $name -ErrorAction SilentlyContinue)
}

# ── 1. Build prerequisites ──────────────────────────────────────────────────
# Rust (MSVC), Visual Studio Build Tools (C++ workload), CMake and LLVM
# (libclang, needed by whisper-rs's bindgen) are required to build.
$missing = @()
if (-not (Test-Cmd cargo))  { $missing += "Rust        →  winget install Rustlang.Rustup" }
if (-not (Test-Cmd cmake))  { $missing += "CMake       →  winget install Kitware.CMake" }
if (-not (Test-Path "C:\Program Files\LLVM\bin\libclang.dll")) {
    $missing += "LLVM        →  winget install LLVM.LLVM"
}
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$hasVC = (Test-Path $vswhere) -and (& $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath)
if (-not $hasVC) {
    $missing += "VS BuildTools (C++) →  winget install Microsoft.VisualStudio.2022.BuildTools --override `"--add Microsoft.VisualStudio.Workload.VCTools --includeRecommended --passive`""
}
if ($missing.Count -gt 0) {
    Write-Host "❌  Missing build tools:" -ForegroundColor Red
    $missing | ForEach-Object { Write-Host "    $_" }
    Write-Host "    Install them (new terminal afterward for PATH), then re-run."
    exit 1
}
Write-Host "✓ Rust, CMake, LLVM, VS Build Tools"

# ── 2. Node + pnpm ──────────────────────────────────────────────────────────
if (-not (Test-Cmd node)) {
    Write-Host "❌  Node missing →  winget install OpenJS.NodeJS.LTS" -ForegroundColor Red
    exit 1
}
if (-not (Test-Cmd pnpm)) {
    Write-Host "→ Installing pnpm…"
    npm install -g pnpm
}
Write-Host "✓ Node $(node --version), pnpm $(pnpm --version)"

# ── 3. Frontend deps ────────────────────────────────────────────────────────
Write-Host "→ Installing frontend deps (pnpm install)…"
pnpm install | Out-Null
Write-Host "✓ frontend deps"

# ── 4. Whisper model ────────────────────────────────────────────────────────
# Windows default is base.en: whisper runs on CPU here (no Metal), and base.en
# keeps the always-on wake-word loop responsive. If you have a beefy CPU, set
# JARVIS_WHISPER_MODEL=small (or higher) and re-run to download a better model.
$modelDir = "models"
New-Item -ItemType Directory -Force $modelDir | Out-Null
$whisperModel = if ($env:JARVIS_WHISPER_MODEL) { $env:JARVIS_WHISPER_MODEL } else { "base.en" }
$modelName = if ($whisperModel.EndsWith(".bin")) { $whisperModel } else { "ggml-$whisperModel.bin" }
$modelFile = Join-Path $modelDir $modelName
if (-not (Test-Path $modelFile)) {
    Write-Host "→ Downloading whisper model $modelName…"
    curl.exe -L --fail -o $modelFile "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/$modelName"
}
Write-Host "✓ whisper model ($modelName)"

# ── 5. Kokoro neural TTS model + voices ─────────────────────────────────────
# The natural local voice. Without it Jarvis falls back to Windows SAPI.
$kokoroBase = "https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0"
foreach ($kf in @("kokoro-v1.0.onnx", "voices-v1.0.bin")) {
    $kfPath = Join-Path $modelDir $kf
    if (-not (Test-Path $kfPath)) {
        Write-Host "→ Downloading Kokoro $kf…"
        curl.exe -L --fail -o $kfPath "$kokoroBase/$kf"
    }
}
Write-Host "✓ kokoro TTS (kokoro-v1.0.onnx + voices-v1.0.bin)"

# ── 6. .env ─────────────────────────────────────────────────────────────────
if (-not (Test-Path ".env")) {
    Copy-Item ".env.example" ".env"
    Write-Host "✓ created .env (from .env.example)"
    Write-Host ""
    Write-Host "🔑 NEXT STEP: open .env and add at least one LLM provider key."
    Write-Host "   The fastest path is Gemini (free): https://aistudio.google.com/apikey"
} else {
    Write-Host "✓ .env already exists (leaving it untouched)"
}

Write-Host ""
Write-Host "──────────────────────────────────────────────────────────────"
Write-Host " Setup complete."
Write-Host "──────────────────────────────────────────────────────────────"
Write-Host ""
Write-Host " Next:"
Write-Host "   1. Fill in at least one LLM provider key in .env"
Write-Host "   2. Run:  pnpm tauri dev    # first build is 10-20 min (whisper.cpp compile)"
Write-Host "   3. Allow microphone access if Windows prompts"
Write-Host "   4. Say `"Jarvis, what time is it?`"  (hotkeys use Ctrl+Alt, e.g. Ctrl+Alt+J)"
Write-Host ""
