# Simple Player for Windows

Simple Player 的 Windows 客戶端：本地音樂和 [Simple Player Web Server](https://github.com/Akiraoo/SimplePlayer-Web-Server) 的曲庫放在同一個播放器裡，內建 Discord 狀態顯示。用 Rust + [Slint](https://slint.dev) 寫成，不使用瀏覽器核心，記憶體占用低。

> 開發中（v0.1）。

## 功能

* **本地音樂**：加入多個資料夾，子資料夾會自動成為歌單（和伺服器一樣的巢狀掃描），標籤逐檔讀取；支援 MP3、FLAC、M4A/AAC、OGG、Opus、WAV 等。掃描結果會快取，第二次開啟不用重掃
* **Simple Player Web Server**：全部歌曲和伺服器上的播放清單，用 HTTP Range 串流，可以直接跳轉
* **歌曲列表**：標題、歌手、專輯、格式、時長、來源，點欄位標題排序；點一下就播放（VR 串流桌面時也好點）
* **即時搜尋**：多個關鍵字用空白分隔
* **封面與同步歌詞**：內嵌封面、內嵌歌詞或同名 `.lrc`，點歌詞跳到該句
* **切歌動畫**：封面和歌詞讀取完成後才開始播放，切換時淡出淡入
* **分離播放器**：右側正在播放區可以一鍵分離成獨立小視窗，列表會自動延伸補滿；兩個視窗都能各自置頂
* **自繪介面**：自訂標題欄（Windows 11 視窗化時有圓角），深色／淺色主題
* **Windows 系統媒體控制**：音量浮窗、鎖定畫面、鍵盤媒體鍵
* **Discord 狀態**：直接推到本機的 Discord App，不用登入。操作（播放、暫停、跳轉、切歌）會立刻同步，平時由 Discord 自己跑進度條；暫停時顯示 ⏸ 並停住進度條。伺服器的歌會顯示封面（需要伺服器有公開的 https 網址）

## 編譯

需要：

1. [Rust](https://rustup.rs/)（安裝時選預設的 MSVC 工具鏈）
2. [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) 的「使用 C++ 的桌面開發」（含 Windows SDK，用來把圖示和版本資訊嵌進 exe）

```
cargo run              # 開發用
cargo build --release  # 產生 target\release\SimplePlayer.exe
```

## 設定

第一次開啟時按標題欄的 ⚙：

* **Simple Player 伺服器**：填 Mobile API 位址，例如 `http://192.168.0.10:55555`，或反向代理後的 `https://music.example.com`
* **本地音樂資料夾**：可以加入多個
* **Discord**：開關，以及選填的公開 https 網址（留空時使用伺服器設定的 `publicOrigin`）

設定檔在 `%APPDATA%\SimplePlayer\config.json`，曲庫和封面快取在 `%LOCALAPPDATA%\SimplePlayer`。

## License

本專案採用 Apache License 2.0，見 `LICENSE`。

### Slint

<a href="https://slint.dev">
  <img alt="#MadeWithSlint" src="https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-light.svg#gh-light-mode-only" height="60">
  <img alt="#MadeWithSlint" src="https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-dark.svg#gh-dark-mode-only" height="60">
</a>

介面使用 [Slint](https://slint.dev)，依 [Slint Royalty-free License](https://github.com/slint-ui/slint/blob/master/LICENSES/LicenseRef-Slint-Royalty-free-2.0.md) 使用；App 的「關於」畫面也有 Slint 標示。
