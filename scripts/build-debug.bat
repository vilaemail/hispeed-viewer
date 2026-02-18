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

echo.
echo === Build successful ===
echo EXE: target\debug\hispeed-viewer.exe
