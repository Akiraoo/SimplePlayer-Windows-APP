# Simple Player for Windows

Simple Player 的 Windows 客戶端：本地音樂和 [Simple Player Web Server](https://github.com/Akiraoo/SimplePlayer-Web-Server) 的曲庫放在同一個播放器裡，內建 Discord 狀態顯示。用 Rust + [Slint](https://slint.dev) 寫成，不使用瀏覽器核心，記憶體占用低。

> 目前版本：v2.0.0。介面支援繁體中文與 English。

## 功能

* **本地音樂**：加入多個資料夾，子資料夾會自動成為歌單（和伺服器一樣的巢狀掃描），標籤逐檔讀取。掃描結果會快取，第二次開啟不用重掃
* **格式**：FLAC、ALAC、WAV/AIFF、MP3、AAC、Vorbis 直接解碼；Opus、APE、WavPack、TTA、Musepack、DSD（DSF/DFF）、WMA、Dolby（AC-3 / E-AC-3）、DTS、TrueHD 等透過 FFmpeg 播放（安裝版附帶精簡版 FFmpeg）。環繞聲會混成立體聲
* **無縫播放**：下一首會提前準備好，歌與歌之間沒有空隙（Live 專輯、連續組曲）
* **Simple Player Web Server**：全部歌曲和伺服器上的播放清單，用 HTTP Range 串流，可以直接跳轉；按重新整理會請伺服器重新掃描曲庫，新加入的歌馬上出現
* **歌曲列表**：標題、歌手、專輯、格式、時長、來源，點欄位標題排序
* **即時搜尋**：多個關鍵字用空白分隔
* **封面與同步歌詞**：內嵌封面、內嵌歌詞或同名 `.lrc`，點歌詞跳到該句
* **格式標籤**：正在播放區顯示編碼與規格，例如 `FLAC · 24-bit · 96 kHz`、`M4A · DD+ · 5.1 · 768 kbps · 48 kHz`
* **迷你播放器**：在檔案總管用「開啟檔案」選 Simple Player（安裝時可勾選加入），或把檔案、資料夾拖進視窗，就會開一個獨立的小播放器；會自動把同專輯或同編號系列的歌排成一次性的播放清單，關掉即清空，不影響主播放器的佇列
* **切歌動畫**：封面和歌詞讀取完成後才開始播放，切換時淡出淡入
* **分離播放器**：右側正在播放區可以一鍵分離成獨立小視窗，列表會自動延伸補滿；兩個視窗都能各自置頂
* **自繪介面**：自訂標題欄（Windows 11 視窗化時有圓角），深色／淺色主題，可自訂主題色
* **播放模式與佇列**：隨機播放、全部循環／單曲循環。滑鼠移到歌曲上可「加入佇列」；佇列是一個臨時歌單，在佇列裡聽完的歌會自動移除，播放其他歌單不會影響佇列
* **記住上次播放**：重新開啟時回到上次的播放佇列和位置（暫停狀態，按播放才會出聲）
* **系統列**：按 ✕ 會縮到系統列繼續播放（可在設定關閉），系統列圖示的選單可以播放／暫停、切歌、結束
* **音訊輸出**：在設定選擇輸出裝置（拔掉時自動改用系統預設）；可開啟 **WASAPI 獨佔模式**，依歌曲本身的取樣率和位元深度直接輸出給裝置（bit-perfect，期間其他程式無法使用該裝置發聲）。裝置不支援歌曲格式時會自動改用最接近的格式；只支援 16-bit 的裝置在降低位元深度時會加上 dither
* **緩衝大小**：標準／大／超大，網路不穩或電腦忙碌時可以調大；串流會在背景預先下載
* **音量平衡（ReplayGain）**：依標籤讓歌曲或專輯之間音量一致（可關閉）
* **高品質取樣率轉換**：共享模式下歌曲和裝置取樣率不同時（例如 44.1 kHz 的歌、48 kHz 的裝置），使用 windowed-sinc 轉換，不會有高頻失真
* **語言**：繁體中文／English，在設定中隨時切換
* **自動檢查更新**：啟動時到 GitHub 檢查新版本（可在設定關閉），「關於」也可以手動檢查
* **Windows 系統媒體控制**：音量浮窗、鎖定畫面、鍵盤媒體鍵
* **Discord 狀態**：直接推到本機的 Discord App，不用登入。操作（播放、暫停、跳轉、切歌）會立刻同步，平時由 Discord 自己跑進度條；暫停時顯示 ⏸ 並停住進度條。伺服器的歌會顯示封面（需要伺服器有公開的 https 網址）

## 編譯

需要：

1. [Rust](https://rustup.rs/)（安裝時選預設的 MSVC 工具鏈）
2. [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) 的「使用 C++ 的桌面開發」（含 Windows SDK，用來把圖示和版本資訊嵌進 exe）

```
Build-Start.bat              # 開發用
Build-Release.bat            # 產生 target\release\SimplePlayer.exe；有安裝 NSIS 時也產生 dist\SimplePlayer-Setup-<版本>.exe
Run-Mini.bat <檔案>          # 測試迷你播放器（也可以把檔案拖到這個 bat 上）
```

安裝包附帶的精簡版 FFmpeg（僅音訊解碼，LGPL）放在 `vendor\ffmpeg.exe`，編譯方式與授權說明見 `tools\ffmpeg-lite`。

## 快捷鍵

| 按鍵 | 功能 |
| --- | --- |
| 空白鍵 | 播放／暫停 |
| ← / → | 倒退／快轉 5 秒 |
| Ctrl + ← / → | 上一首／下一首 |
| ↑ / ↓ | 音量 |
| Ctrl + F | 搜尋（Esc 離開搜尋框） |
| S | 隨機播放 |
| R | 循環模式（關 → 全部 → 單曲） |
| Esc | 關閉設定／關於 |

## 設定

第一次開啟時按標題欄的 ⚙：

* **一般**：介面語言、啟動時自動檢查更新

* **Simple Player 伺服器**：填 Mobile API 位址，例如 `http://192.168.0.10:55555`，或反向代理後的 `https://music.example.com`
* **本地音樂資料夾**：可以加入多個
* **Discord**：開關，以及選填的公開 https 網址（留空時使用伺服器設定的 `publicOrigin`）
* **音訊輸出**：輸出裝置、WASAPI 獨佔、緩衝大小、音量平衡；「目前輸出」會顯示實際的輸出格式

設定檔在 `%APPDATA%\SimplePlayer\config.json`，曲庫和封面快取在 `%LOCALAPPDATA%\SimplePlayer`。

## License

本專案採用 Apache License 2.0，見 `LICENSE`。

### Slint

<a href="https://slint.dev">
  <img alt="#MadeWithSlint" src="https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-light.svg#gh-light-mode-only" height="60">
  <img alt="#MadeWithSlint" src="https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-dark.svg#gh-dark-mode-only" height="60">
</a>

介面使用 [Slint](https://slint.dev)，依 [Slint Royalty-free License](https://github.com/slint-ui/slint/blob/master/LICENSES/LicenseRef-Slint-Royalty-free-2.0.md) 使用；App 的「關於」畫面也有 Slint 標示。
