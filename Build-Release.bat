@echo off
setlocal
cd /d "%~dp0"
call "%~dp0msvc-env.bat"
rem Release build. The C runtime is linked statically, so the exe also runs on PCs
rem without the Visual C++ Redistributable installed.
set RUSTFLAGS=-C target-feature=+crt-static
cargo build --release
if errorlevel 1 goto end
echo.
echo Done: target\release\SimplePlayer.exe

rem Installer (needs NSIS: https://nsis.sourceforge.io)
for /f "tokens=2 delims== " %%v in ('findstr /b "version" Cargo.toml') do set VER=%%~v
set MAKENSIS=
where makensis >nul 2>nul && set MAKENSIS=makensis
if not defined MAKENSIS if exist "%ProgramFiles(x86)%\NSIS\makensis.exe" set MAKENSIS="%ProgramFiles(x86)%\NSIS\makensis.exe"
if not defined MAKENSIS if exist "%ProgramFiles%\NSIS\makensis.exe" set MAKENSIS="%ProgramFiles%\NSIS\makensis.exe"
if not defined MAKENSIS (
  echo NSIS not found - skipping the installer. Install NSIS to also build dist\SimplePlayer-Setup-%VER%.exe
  explorer /select,"%~dp0target\release\SimplePlayer.exe"
  goto end
)
%MAKENSIS% /V2 /DVERSION=%VER% installer\SimplePlayer.nsi
if errorlevel 1 goto end
echo.
echo Done: dist\SimplePlayer-Setup-%VER%.exe
explorer /select,"%~dp0dist\SimplePlayer-Setup-%VER%.exe"
:end
pause
