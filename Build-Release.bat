@echo off
rem Release build. The C runtime is linked statically, so the exe also runs on PCs
rem without the Visual C++ Redistributable installed.
set RUSTFLAGS=-C target-feature=+crt-static
cargo build --release
if errorlevel 1 goto end
echo.
echo Done: target\release\SimplePlayer.exe
explorer /select,"%~dp0target\release\SimplePlayer.exe"
:end
pause
