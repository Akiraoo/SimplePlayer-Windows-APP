# Simple Player for Windows

Simple Player 的 Windows 客戶端：本地音樂和 [Simple Player Web Server](https://github.com/Akiraoo/SimplePlayer-Web-Server) 的曲庫放在同一個播放器裡，內建 Discord 狀態顯示。用 Rust + [Slint](https://slint.dev) 寫成，不使用瀏覽器核心，記憶體占用低。

> 開發中（v0.1）。

## 功能

* 本地音樂資料夾（MP3、FLAC、M4A/AAC、OGG、Opus、WAV…），掃描結果會快取，第二次開啟不用重掃
* 連接 Simple Player Web Server：全部歌曲和伺服器上的播放清單，用 HTTP Range 串流，可以直接跳轉
* fb2k 風格的多欄歌曲列表：標題、歌手、專輯、時長、格式、來源，點欄位標題排序
* 即時搜尋（多個關鍵字用空白分隔）
* Windows 系統媒體控制：音量浮窗、鎖定畫面、鍵盤媒體鍵
* Discord 狀態：直接推到本機的 Discord App，不用登入；暫停時顯示 ⏸ 並停住進度條

## 編譯

需要：

1. [Rust](https://rustup.rs/)（安裝時選預設的 MSVC 工具鏈）
2. [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) 的「使用 C++ 的桌面開發」

```
cargo run              # 開發用
cargo build --release  # 產生 target\release\simpleplayer-windows.exe
```

## 設定

第一次開啟時按右上角的 ⚙：

* **Simple Player 伺服器**：填 Mobile API 位址，例如 `http://192.168.0.10:55555`，或反向代理後的 `https://music.example.com`
* **本地音樂資料夾**：可以加入多個

設定檔在 `%APPDATA%\SimplePlayer\config.json`，曲庫和封面快取在 `%LOCALAPPDATA%\SimplePlayer`。

## License

本專案採用 Apache License 2.0，見 `LICENSE`。

### Slint

<a href="https://slint.dev">
  <img alt="#MadeWithSlint" src="https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-light.svg#gh-light-mode-only" height="60">
  <img alt="#MadeWithSlint" src="https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-dark.svg#gh-dark-mode-only" height="60">
</a>

介面使用 [Slint](https://slint.dev)，依 [Slint Royalty-free License](https://github.com/slint-ui/slint/blob/master/LICENSES/LicenseRef-Slint-Royalty-free-2.0.md) 使用；App 的「關於」畫面也有 Slint 標示。
