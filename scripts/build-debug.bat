@echo off
setlocal
cd /d "%~dp0.."

echo === HiSpeed Viewer - Debug Build ===
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
echo === Build successful ===
echo EXE: target\debug\hispeed-viewer.exe
