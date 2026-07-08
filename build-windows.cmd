@echo off
rem Build/dev wrapper for Windows — loads the MSVC environment (whisper-rs's
rem bindgen needs the Windows SDK include paths) plus LLVM/CMake, then runs
rem the given command in the repo root. Examples:
rem   build-windows.cmd cargo check --manifest-path src-tauri\Cargo.toml
rem   build-windows.cmd pnpm tauri dev
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul
set "LIBCLANG_PATH=C:\Program Files\LLVM\bin"
set "PATH=%USERPROFILE%\.cargo\bin;C:\Program Files\CMake\bin;C:\Program Files\LLVM\bin;%PATH%"
set "CARGO_NET_RETRY=20"
cd /d "%~dp0"
%*
