# FFmpeg lite

A decode-only `ffmpeg.exe` (about 2.7 MB instead of 100+ MB) that the installer bundles,
so Simple Player can play the formats Symphonia can't: Opus, APE, WavPack, TTA, Musepack,
TAK, DSD (DSF/DFF), WMA, Dolby AC-3 / E-AC-3, DTS, TrueHD and more.

* **License:** LGPL-2.1-or-later. It is built **without** `--enable-gpl` and
  `--enable-nonfree`; Simple Player itself stays Apache-2.0 and only runs `ffmpeg.exe`
  as a separate program. `vendor/FFmpeg-LICENSE.txt` (installed next to it) carries the
  notice, the exact source and the license text.
* **Source:** FFmpeg `n7.1.1`, unmodified: <https://github.com/FFmpeg/FFmpeg/tree/n7.1.1>
* **What is in it:** see `configure-flags.sh` (decoders, demuxers, parsers, a few audio
  filters, file/pipe/http/https; https through Windows' own schannel). No encoders except
  raw PCM output, no video.
* **Dependencies:** only DLLs that ship with Windows 10/11 (UCRT, Winsock, schannel).

## Building

On Linux or WSL with [llvm-mingw](https://github.com/mstorsjo/llvm-mingw) on `PATH`:

```sh
git clone --depth 1 --branch n7.1.1 https://github.com/FFmpeg/FFmpeg.git ffmpeg-src
sh tools/ffmpeg-lite/build.sh ffmpeg-src build-ffmpeg
cp build-ffmpeg/windows/bin/ffmpeg.exe vendor/
```

(`TARGET=linux` builds a copy for the build machine, handy for testing.)
`Build-Release.bat` puts `vendor/ffmpeg.exe` into the installer together with
`FFmpeg-LICENSE.txt` from this folder (update its build details if you build another version).
`vendor/` is not in git.

The bundled v1 build: llvm-mingw 20250114 (UCRT), SHA-256 `391081c95a1a42797edd7e94eb14c0c5d5531615f6cc79069b893230856975fb`.
