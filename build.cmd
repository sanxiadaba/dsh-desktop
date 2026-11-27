@echo off
REM Build the release binary and put desk.exe at the repo root (gitignored).
setlocal
cd /d "%~dp0"
cargo build --release
if errorlevel 1 exit /b 1
copy /y "target\release\desk.exe" "desk.exe" >nul
echo desk.exe ready: %~dp0desk.exe
