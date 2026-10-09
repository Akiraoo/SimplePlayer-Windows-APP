@echo off
cd /d "%~dp0"
call "%~dp0msvc-env.bat"
cargo run
pause
