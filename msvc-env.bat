@echo off
rem Sets up the MSVC build tools (link.exe) for this console when Rust cannot find them on
rem its own (e.g. Visual Studio installed on another drive and its install record lost).
rem
rem Machine-specific path: put it in msvc-env.local.bat (git-ignored), e.g.
rem   set "VSDIR=D:\Visual Studio"
if defined VCINSTALLDIR goto :eof
set "VSDIR="
if exist "%~dp0msvc-env.local.bat" call "%~dp0msvc-env.local.bat"
if not defined VSDIR (
  set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
)
if not defined VSDIR if exist "%VSWHERE%" (
  for /f "usebackq delims=" %%i in (`"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "VSDIR=%%i"
)
if not defined VSDIR goto :eof
if not exist "%VSDIR%\VC\Auxiliary\Build\vcvars64.bat" goto :eof
call "%VSDIR%\VC\Auxiliary\Build\vcvars64.bat" >nul
