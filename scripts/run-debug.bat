@echo off
setlocal
cd /d "%~dp0.."

echo === HiSpeed Viewer - Debug Run ===
echo.

cargo build
if %ERRORLEVEL% neq 0 (
    echo.
    echo BUILD FAILED
    exit /b %ERRORLEVEL%
)

:: Copy settings.json to output directory if it exists at repo root
if exist "settings.json" copy /y "settings.json" "target\debug\settings.json" >nul

echo.
echo === Launching with debug logging ===
echo Press Ctrl+C to stop
echo.
set RUST_LOG=hispeed_viewer=debug,info
target\debug\hispeed-viewer.exe
