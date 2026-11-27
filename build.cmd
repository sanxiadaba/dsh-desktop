@echo off
REM Build the release binary and put desk.exe at the repo root (gitignored).
setlocal
cd /d "%~dp0"
where cargo >nul 2>nul
if errorlevel 1 goto nocargo
cargo build --release
if errorlevel 1 exit /b 1
copy /y "target\release\desk.exe" "desk.exe" >nul
if errorlevel 1 goto nocopy
echo desk.exe ready: %~dp0desk.exe
exit /b 0
:nocargo
echo [desk] cargo not found. Install Rust 1.85+ from https://rustup.rs and VS Build Tools (Desktop development with C++).
exit /b 1
:nocopy
echo [desk] failed to copy desk.exe (file may be in use).
exit /b 1
