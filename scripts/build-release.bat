@echo off
setlocal enabledelayedexpansion

cd /d "%~dp0.."

echo === HiSpeed Viewer - Release Build ===
echo.

if not exist "signing\release.pfx" (
    echo ERROR: signing\release.pfx not found.
    exit /b 1
)

:: Back up Cargo.toml so we can restore it no matter what
copy /y Cargo.toml Cargo.toml.bak >nul

:: Always use unofficial version for non-release builds
set "VERSION=0.0.1-unofficial"
echo Version: %VERSION%

:: Update version in Cargo.toml temporarily
powershell -NoProfile -Command "$c = Get-Content Cargo.toml; $c = $c -replace ('^version = ' + [char]34 + '.*?' + [char]34), ('version = ' + [char]34 + '%VERSION%' + [char]34); Set-Content Cargo.toml -Value $c"
echo.

for /f "usebackq delims=" %%p in (`powershell -NoProfile -Command ^
    "$pw = Read-Host 'Certificate password' -AsSecureString;" ^
    "[Runtime.InteropServices.Marshal]::PtrToStringAuto(" ^
    "[Runtime.InteropServices.Marshal]::SecureStringToBSTR($pw))"`) do set "CERT_PW=%%p"

if not defined CERT_PW (
    echo ERROR: Failed to capture certificate password.
    set "EXIT_CODE=1"
    goto :cleanup
)

cargo test
if %ERRORLEVEL% neq 0 (
    echo.
    echo TESTS FAILED - aborting release build
    set "EXIT_CODE=%ERRORLEVEL%"
    goto :cleanup
)

cargo clean --release -p hispeed-viewer
cargo build --release
if %ERRORLEVEL% neq 0 (
    echo.
    echo BUILD FAILED
    set "EXIT_CODE=%ERRORLEVEL%"
    goto :cleanup
)

if not exist "release" mkdir release
set "EXE=release\hispeed-viewer-v%VERSION%.exe"
copy /y "target\release\hispeed-viewer.exe" "%EXE%" >nul

:: Find signtool.exe - check PATH first, then search Windows Kits
set "SIGNTOOL="
set "KITSBIN=%ProgramFiles(x86)%\Windows Kits\10\bin"
where signtool.exe >nul 2>nul && set "SIGNTOOL=signtool.exe"
if not defined SIGNTOOL for /f "delims=" %%d in ('dir /b /o-n "%KITSBIN%" 2^>nul') do if exist "%KITSBIN%\%%d\x64\signtool.exe" if not defined SIGNTOOL set "SIGNTOOL=%KITSBIN%\%%d\x64\signtool.exe"
if not defined SIGNTOOL echo ERROR: signtool.exe not found in PATH or Windows Kits. & echo Install the Windows SDK or add signtool.exe to PATH. & set "EXIT_CODE=1" & goto :cleanup
echo Using: %SIGNTOOL%

echo Signing %EXE% ...
set "SIGN_RESULT=1"
for /l %%i in (1,1,3) do if !SIGN_RESULT! neq 0 (
    if %%i gtr 1 echo Retry %%i/3 ... & timeout /t 2 /nobreak >nul
    "%SIGNTOOL%" sign /f "signing\release.pfx" /p "!CERT_PW!" /fd sha256 /t http://timestamp.digicert.com "%EXE%"
    set "SIGN_RESULT=!ERRORLEVEL!"
)
set "CERT_PW="

if %SIGN_RESULT% neq 0 (
    echo.
    echo SIGNING FAILED
    set "EXIT_CODE=%SIGN_RESULT%"
    goto :cleanup
)

:: Copy settings.json to output directories if it exists at repo root
if exist "settings.json" (
    copy /y "settings.json" "target\release\settings.json" >nul
    copy /y "settings.json" "release\settings.json" >nul
)

echo.
echo === Build successful ===
echo EXE: %EXE%
set "EXIT_CODE=0"

:cleanup
set "CERT_PW="
copy /y Cargo.toml.bak Cargo.toml >nul
del /q Cargo.toml.bak >nul
exit /b %EXIT_CODE%
