; Simple Player for Windows — NSIS installer
;
; Build (from the repo root, after Build-Release.bat):
;   makensis installer\SimplePlayer.nsi      (version comes from Cargo.toml)
; Output: dist\SimplePlayer-Setup-<version>.exe
;
; The program goes to Program Files; settings and caches stay in the user's AppData
; (%APPDATA%\SimplePlayer, %LOCALAPPDATA%\SimplePlayer), written by the app itself.

Unicode true
SetCompressor /SOLID lzma

; Version: read from Cargo.toml ([package] version is the first `version = "..."` line).
; Build-Release.bat passes /DVERSION too; either way there is nothing to edit here.
!ifndef VERSION
  !searchparse /file "..\Cargo.toml" 'version = "' VERSION '"'
!endif
!define APPNAME   "Simple Player"
!define EXENAME   "SimplePlayer.exe"
!define PUBLISHER "Akiraoo"
!define WEBSITE   "https://github.com/Akiraoo/SimplePlayer-Windows-App"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\SimplePlayer"
!define ROOT      ".."

Name "${APPNAME}"
!system 'if not exist "${ROOT}\dist" mkdir "${ROOT}\dist"'
OutFile "${ROOT}\dist\SimplePlayer-Setup-${VERSION}.exe"
InstallDir "$PROGRAMFILES64\${APPNAME}"
InstallDirRegKey HKLM "Software\SimplePlayer" "InstallDir"
RequestExecutionLevel admin
BrandingText "${APPNAME} ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey /LANG=0 "ProductName" "${APPNAME}"
VIAddVersionKey /LANG=0 "FileDescription" "${APPNAME} Setup"
VIAddVersionKey /LANG=0 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=0 "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=0 "CompanyName" "${PUBLISHER}"
VIAddVersionKey /LANG=0 "LegalCopyright" "Apache-2.0"

!include "MUI2.nsh"
!include "x64.nsh"
!include "FileFunc.nsh"

!define MUI_ICON   "${ROOT}\assets\app.ico"
!define MUI_UNICON "${ROOT}\assets\app.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_FUNCTION LaunchApp

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${ROOT}\LICENSE"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_COMPONENTS
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "TradChinese"
!insertmacro MUI_LANGUAGE "English"

LangString SecMain       ${LANG_TRADCHINESE} "Simple Player（必要）"
LangString SecMain       ${LANG_ENGLISH}     "Simple Player (required)"
LangString SecDesktop    ${LANG_TRADCHINESE} "桌面捷徑"
LangString SecDesktop    ${LANG_ENGLISH}     "Desktop shortcut"
LangString SecStart      ${LANG_TRADCHINESE} "開始功能表捷徑"
LangString SecStart      ${LANG_ENGLISH}     "Start menu shortcut"
LangString SecUnData     ${LANG_TRADCHINESE} "同時刪除設定與快取（AppData）"
LangString SecUnData     ${LANG_ENGLISH}     "Also delete settings and caches (AppData)"
LangString Need64        ${LANG_TRADCHINESE} "Simple Player 需要 64 位元的 Windows。"
LangString Need64        ${LANG_ENGLISH}     "Simple Player requires 64-bit Windows."
LangString SecFFmpeg     ${LANG_TRADCHINESE} "FFmpeg（播放 Opus、APE、WavPack、DSD、Dolby 等格式）"
LangString SecFFmpeg     ${LANG_ENGLISH}     "FFmpeg (plays Opus, APE, WavPack, DSD, Dolby and more)"
LangString SecAssoc      ${LANG_TRADCHINESE} "加入音訊檔的「開啟檔案」選單（迷你播放器）"
LangString SecAssoc      ${LANG_ENGLISH}     "Add to $\"Open with$\" for audio files (mini player)"
LangString AudioFile     ${LANG_TRADCHINESE} "音訊檔"
LangString AudioFile     ${LANG_ENGLISH}     "Audio file"
LangString UninstName    ${LANG_TRADCHINESE} "解除安裝 Simple Player"
LangString UninstName    ${LANG_ENGLISH}     "Uninstall Simple Player"

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "$(Need64)"
    Abort
  ${EndIf}
  SetRegView 64
FunctionEnd

Function un.onInit
  SetRegView 64
FunctionEnd

; Close a running copy so its exe can be replaced.
!macro CloseApp
  nsExec::Exec 'taskkill /IM "${EXENAME}" /F'
  Pop $0
  Sleep 400
!macroend

; The installer runs elevated; start the app through Explorer so it runs as the normal user
; (same AppData, Discord and drag & drop work as usual).
Function LaunchApp
  Exec '"$WINDIR\explorer.exe" "$INSTDIR\${EXENAME}"'
FunctionEnd

Section "$(SecMain)" SEC_MAIN
  SectionIn RO
  !insertmacro CloseApp
  SetOutPath "$INSTDIR"
  File "${ROOT}\target\release\${EXENAME}"
  File "${ROOT}\LICENSE"
  File "${ROOT}\README.md"
  WriteUninstaller "$INSTDIR\Uninstall.exe"

  WriteRegStr HKLM "Software\SimplePlayer" "InstallDir" "$INSTDIR"
  WriteRegStr HKLM "${UNINSTKEY}" "DisplayName" "${APPNAME}"
  WriteRegStr HKLM "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINSTKEY}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKLM "${UNINSTKEY}" "URLInfoAbout" "${WEBSITE}"
  WriteRegStr HKLM "${UNINSTKEY}" "DisplayIcon" '"$INSTDIR\${EXENAME}",0'
  WriteRegStr HKLM "${UNINSTKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINSTKEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegStr HKLM "${UNINSTKEY}" "QuietUninstallString" '"$INSTDIR\Uninstall.exe" /S'
  WriteRegDWORD HKLM "${UNINSTKEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINSTKEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKLM "${UNINSTKEY}" "EstimatedSize" "$0"
SectionEnd

; FFmpeg is bundled only when the build machine has vendor\ffmpeg.exe (Build-Release.bat
; copies it there). The app looks for ffmpeg.exe next to SimplePlayer.exe first.
!if /FileExists "${ROOT}\vendor\ffmpeg.exe"
Section "$(SecFFmpeg)" SEC_FFMPEG
  SetOutPath "$INSTDIR"
  File "${ROOT}\vendor\ffmpeg.exe"
  !if /FileExists "${ROOT}\vendor\FFmpeg-LICENSE.txt"
    File "${ROOT}\vendor\FFmpeg-LICENSE.txt"
  !endif
SectionEnd
!endif

; ---------------- "Open with" ----------------
; Double-clicking an audio file (once the user picks Simple Player) opens the mini player:
;   SimplePlayer.exe --mini "<file>"
; Windows 10/11 don't let installers take over the default app; the user chooses it in
; "Open with" or Settings > Default apps, where Simple Player is now listed.
!define PROGID "SimplePlayer.Audio"
!define CAPS   "Software\SimplePlayer\Capabilities"

!macro AssocExt EXT
  WriteRegStr HKLM "Software\Classes\.${EXT}\OpenWithProgids" "${PROGID}" ""
  WriteRegStr HKLM "Software\Classes\Applications\${EXENAME}\SupportedTypes" ".${EXT}" ""
  WriteRegStr HKLM "${CAPS}\FileAssociations" ".${EXT}" "${PROGID}"
!macroend

!macro UnassocExt EXT
  DeleteRegValue HKLM "Software\Classes\.${EXT}\OpenWithProgids" "${PROGID}"
!macroend

Section "$(SecAssoc)" SEC_ASSOC
  WriteRegStr HKLM "Software\Classes\${PROGID}" "" "$(AudioFile) (Simple Player)"
  WriteRegStr HKLM "Software\Classes\${PROGID}\DefaultIcon" "" '"$INSTDIR\${EXENAME}",0'
  WriteRegStr HKLM "Software\Classes\${PROGID}\shell\open\command" "" '"$INSTDIR\${EXENAME}" --mini "%1"'
  WriteRegStr HKLM "Software\Classes\Applications\${EXENAME}" "FriendlyAppName" "${APPNAME}"
  WriteRegStr HKLM "Software\Classes\Applications\${EXENAME}\DefaultIcon" "" '"$INSTDIR\${EXENAME}",0'
  WriteRegStr HKLM "Software\Classes\Applications\${EXENAME}\shell\open\command" "" '"$INSTDIR\${EXENAME}" --mini "%1"'
  WriteRegStr HKLM "${CAPS}" "ApplicationName" "${APPNAME}"
  WriteRegStr HKLM "${CAPS}" "ApplicationDescription" "Simple Player"
  WriteRegStr HKLM "${CAPS}" "ApplicationIcon" '"$INSTDIR\${EXENAME}",0'
  !insertmacro AssocExt "mp3"
  !insertmacro AssocExt "mp2"
  !insertmacro AssocExt "flac"
  !insertmacro AssocExt "m4a"
  !insertmacro AssocExt "m4b"
  !insertmacro AssocExt "aac"
  !insertmacro AssocExt "alac"
  !insertmacro AssocExt "ogg"
  !insertmacro AssocExt "oga"
  !insertmacro AssocExt "opus"
  !insertmacro AssocExt "wav"
  !insertmacro AssocExt "w64"
  !insertmacro AssocExt "wma"
  !insertmacro AssocExt "ape"
  !insertmacro AssocExt "wv"
  !insertmacro AssocExt "tta"
  !insertmacro AssocExt "mpc"
  !insertmacro AssocExt "dsf"
  !insertmacro AssocExt "dff"
  !insertmacro AssocExt "aiff"
  !insertmacro AssocExt "aif"
  !insertmacro AssocExt "aifc"
  !insertmacro AssocExt "caf"
  !insertmacro AssocExt "mka"
  !insertmacro AssocExt "ac3"
  !insertmacro AssocExt "eac3"
  !insertmacro AssocExt "ec3"
  !insertmacro AssocExt "dts"
  !insertmacro AssocExt "thd"
  WriteRegStr HKLM "Software\RegisteredApplications" "${APPNAME}" "${CAPS}"
  ; tell Explorer the associations changed (SHCNE_ASSOCCHANGED)
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'
SectionEnd

Section "$(SecStart)" SEC_START
  SetShellVarContext all
  CreateDirectory "$SMPROGRAMS\${APPNAME}"
  CreateShortcut "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk" "$INSTDIR\${EXENAME}"
  CreateShortcut "$SMPROGRAMS\${APPNAME}\$(UninstName).lnk" "$INSTDIR\Uninstall.exe"
SectionEnd

Section "$(SecDesktop)" SEC_DESKTOP
  SetShellVarContext all
  CreateShortcut "$DESKTOP\${APPNAME}.lnk" "$INSTDIR\${EXENAME}"
SectionEnd

; ---------------- uninstall ----------------

Section "un.$(SecMain)" UNSEC_MAIN
  SectionIn RO
  !insertmacro CloseApp
  SetShellVarContext all
  Delete "$DESKTOP\${APPNAME}.lnk"
  Delete "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk"
  Delete "$SMPROGRAMS\${APPNAME}\$(UninstName).lnk"
  RMDir "$SMPROGRAMS\${APPNAME}"

  Delete "$INSTDIR\${EXENAME}"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\ffmpeg.exe"
  Delete "$INSTDIR\FFmpeg-LICENSE.txt"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"

  ; "Open with" registration
  !insertmacro UnassocExt "mp3"
  !insertmacro UnassocExt "mp2"
  !insertmacro UnassocExt "flac"
  !insertmacro UnassocExt "m4a"
  !insertmacro UnassocExt "m4b"
  !insertmacro UnassocExt "aac"
  !insertmacro UnassocExt "alac"
  !insertmacro UnassocExt "ogg"
  !insertmacro UnassocExt "oga"
  !insertmacro UnassocExt "opus"
  !insertmacro UnassocExt "wav"
  !insertmacro UnassocExt "w64"
  !insertmacro UnassocExt "wma"
  !insertmacro UnassocExt "ape"
  !insertmacro UnassocExt "wv"
  !insertmacro UnassocExt "tta"
  !insertmacro UnassocExt "mpc"
  !insertmacro UnassocExt "dsf"
  !insertmacro UnassocExt "dff"
  !insertmacro UnassocExt "aiff"
  !insertmacro UnassocExt "aif"
  !insertmacro UnassocExt "aifc"
  !insertmacro UnassocExt "caf"
  !insertmacro UnassocExt "mka"
  !insertmacro UnassocExt "ac3"
  !insertmacro UnassocExt "eac3"
  !insertmacro UnassocExt "ec3"
  !insertmacro UnassocExt "dts"
  !insertmacro UnassocExt "thd"
  DeleteRegKey HKLM "Software\Classes\${PROGID}"
  DeleteRegKey HKLM "Software\Classes\Applications\${EXENAME}"
  DeleteRegValue HKLM "Software\RegisteredApplications" "${APPNAME}"
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'

  DeleteRegKey HKLM "${UNINSTKEY}"
  DeleteRegKey HKLM "Software\SimplePlayer"
SectionEnd

; Off by default: keeps the library cache, folders and Discord settings for a reinstall.
Section /o "un.$(SecUnData)" UNSEC_DATA
  ; Settings belong to the user who runs the uninstaller (the elevated user is normally
  ; the same account).
  SetShellVarContext current
  RMDir /r "$APPDATA\SimplePlayer"
  RMDir /r "$LOCALAPPDATA\SimplePlayer"
SectionEnd
