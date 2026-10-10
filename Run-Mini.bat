@echo off
setlocal
cd /d "%~dp0"
call "%~dp0msvc-env.bat"
rem Test the mini player: drag audio files (or a folder) onto this file,
rem or run: Run-Mini.bat "F:\Music\Album\01 song.flac"
cargo run -- --mini %*
