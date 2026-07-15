<p align="center">
  <img src="assets/logo.svg" alt="Astroshel Lean Display logo" width="96" height="96">
</p>

<h1 align="center">Astroshel Lean Display</h1>

<p align="center">
  <strong>A tiny, fast, open-source replacement for the Jungle Leopard Astroshel LCD AIO cooler's <code>Smart Screen V28</code> software.</strong><br>
  Drive your AIO liquid cooler's 320×320 screen with a sensor dashboard, images, GIFs, or video — using ~10&nbsp;MB instead of hundreds.
</p>

<p align="center">
  <img alt="Platform: Windows" src="https://img.shields.io/badge/platform-Windows%2010%20%2F%2011-0078D6">
  <img alt="Built with Rust" src="https://img.shields.io/badge/built%20with-Rust-DEA584">
  <img alt="Footprint ~10 MB" src="https://img.shields.io/badge/footprint-~10%20MB-brightgreen">
  <a href="https://github.com/Duskrosy/astroshel-lcd/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/Duskrosy/astroshel-lcd?label=download"></a>
</p>

---

## What is this?

The **Jungle Leopard Astroshel LCD 240 ARGB AIO** liquid cooler ships with a little **320×320 LCD** on the pump head and a bundled Windows app, **`Smart Screen V28.exe`**, to drive it. That stock app is heavy — a ~370&nbsp;MB install that continuously software-encodes full-motion video to the screen, burning **hundreds of megabytes of RAM and thousands of CPU-seconds** in the background just to show a gauge.

**Astroshel Lean Display** is a clean-room, reverse-engineered, native replacement. It drives the exact same LCD from a small system-tray app: a customizable **sensor dashboard**, or your own **images / GIFs / MP4 (H.264 & H.265/HEVC) video** — at a fraction of the resource cost.

> Not affiliated with, or endorsed by, Jungle Leopard or the makers of Smart Screen V28. "Jungle Leopard" and "Astroshel" are used only to identify the compatible hardware.

## Astroshel Lean Display vs. Smart Screen V28

| | Smart Screen V28 (stock) | **Astroshel Lean Display** |
|---|---|---|
| Install size | ~370 MB (bundles full FFmpeg + Qt) | **~10 MB** (6.7 MB app + 3.1 MB trimmed ffmpeg) |
| Idle CPU | Constant full-motion video encode | **Near-zero** (static frames + keepalive) |
| RAM | Hundreds of MB | **Tens of MB** |
| UI | Cramped, hard-to-read | Clean two-pane, live 320×320 WYSIWYG preview |
| Customization | Fixed themes | Templates, colors, widget types, freeform editor, profiles |
| Source | Closed | **Open source** |

## Features

- **Customizable sensor dashboard** — GPU temperature, GPU / CPU / RAM usage, and clock/date, drawn as **arc gauges, rings, bars, big numbers, sparklines, or an analog clock**. Preset templates, color themes, and a **freeform drag-and-resize editor** to lay widgets out exactly how you want.
- **Media mode** — show any **image, animated GIF, or video** (MP4/MOV/MKV, **H.264 or H.265/HEVC**). Fit with **Contain / Cover / Stretch**, plus **zoom and drag-to-pan**, against a live 320×320 preview (what you see is exactly what the cooler shows).
- **Smooth video, done once** — videos are imported and cached to a compact, device-ready format a single time, then play back with almost no CPU (no live decoding loop).
- **Recent Media** quick-select with thumbnails, and unlimited saveable **Profiles** (snapshot your whole setup and switch in one click).
- **Adjustable bitrate**, **live brightness** (1–100%), starts at logon, auto-reconnects, and tucks into the **system tray**.

## Compatibility

- **Windows 10 / 11** (64-bit).
- Cooler LCD exposed as a USB CDC serial device — **`USB\VID_1F3A&PID_0007`** (Allwinner), the same device `Smart Screen V28` talks to. To check: Device Manager → Ports (COM & LPT) or run `Get-PnpDevice -Class Ports` in PowerShell.
- Verified on the **Jungle Leopard Astroshel LCD 240 ARGB AIO** (320×320). Other AIO coolers whose LCD uses the same Smart Screen V28 app / the same USB ID are likely compatible — reports welcome via [Issues](https://github.com/Duskrosy/astroshel-lcd/issues).
- The sensor dashboard's GPU stats use NVIDIA's NVML (an NVIDIA driver is required for GPU temperature/usage widgets).

## Install

**Recommended — winget:**

```
winget install Duskrosy.AstroshelLeanDisplay
```

**Or the installer:**

1. Download **`astroshel-lean-display-*-setup.exe`** from the [**Releases**](https://github.com/Duskrosy/astroshel-lcd/releases/latest) page and run it — a simple Next → Finish wizard, **no admin rights** needed. It adds a Start Menu shortcut and starts with Windows. Uninstall any time from **Settings → Apps** (or the Start Menu "Uninstall" shortcut).
2. **Close `Smart Screen V28`** first (both apps can't share the LCD's COM port; remove it from Task Manager → Startup so it doesn't relaunch).
3. Done — the app runs in the system tray. Double-click the tray icon to open it.

The app **checks for updates** on its own and can download + run the new installer in one click (⚙ Settings shows when one's available).

## "Windows protected your PC" / antivirus warnings

Astroshel Lean Display is a new, open-source app that isn't code-signed yet, so Windows may warn you the first time you run it. **This is about signing and download reputation — not because the app is harmful.** The full source is right here in this repo, and you can verify your download hasn't been tampered with (below).

- **Blue "Windows protected your PC" (SmartScreen):** click **More info → Run anyway**.
- **Red "threat found" (Windows Defender / other antivirus):** this is a false positive — small, unsigned Rust apps sometimes trip heuristic scanners. Allow/restore the file in your antivirus, and (optionally) report the false positive at **https://www.microsoft.com/wdsi/filesubmission** so it gets cleared for everyone.

**Verify your download (recommended).** Every release includes a `SHA256SUMS.txt`. Check that your file matches:

```
certutil -hashfile astroshel-lean-display-v0.4.0-setup.exe SHA256
```

Compare the printed hash to the one in `SHA256SUMS.txt` on the [release page](https://github.com/Duskrosy/astroshel-lcd/releases/latest). If they match, the download is authentic.

## Usage

- **Double-click the tray icon** (or right-click → Open) to open the window.
- Pick **Dashboard** or **Media** mode.
  - *Dashboard:* choose a template/theme, tweak colors, add/move/resize widgets.
  - *Media:* click **Add Media…**, frame it with fit / zoom / drag, and **Apply**. Videos show an import progress bar the first time, then play from cache.
- Save the current look as a **Profile**, or click a **Recent** thumbnail to reload it. The **⚙** button clears the video cache.
- **Right-click the tray → Quit** to exit.

Settings and profiles persist to `%LOCALAPPDATA%\AstroshelLcd\config.toml`; the video cache lives in `%LOCALAPPDATA%\AstroshelLcd\cache`; logs in `astroshel-lcd.log`.

## How it works

The LCD is an Allwinner display behind a USB CDC serial port (1,000,000 baud, 8N1). After a small handshake, frames are sent as **H.264 Annex-B** video wrapped in a simple `5A A5 85 00 | <len> | <payload>` framing, with brightness set via a `0x80` message. Rather than streaming full-motion video like the stock app, this app encodes **one self-contained keyframe** when the display actually changes (~1/sec for the dashboard) and lets the panel hold it — which is why idle CPU is essentially zero. Motion media (GIF/video) is streamed as low-bitrate H.264 with periodic keyframes, exactly within the panel's decoder budget.

## Build from source

Requires the [Rust](https://www.rust-lang.org/) toolchain (MSVC or GNU). For video import, keep a `ffmpeg.exe` next to the built binary (or on `PATH`).

```sh
cargo build --release
```

Output is a single self-contained `astroshel-lcd.exe` in `target/release`.

## FAQ

**The Astroshel / Smart Screen V28 app uses tons of CPU and RAM — is there a lightweight alternative?**
Yes — that's exactly why this exists. It replaces `Smart Screen V28` with a ~10 MB tray app that idles at near-zero CPU.

**Can I put my own image, GIF, or video on the cooler's screen?**
Yes — images, GIFs, and H.264/H.265 (HEVC) MP4/MOV/MKV video, with fit/zoom/pan and a live preview.

**Will killing Smart Screen V28 break my ARGB lighting or fans?**
No. On these coolers the LCD is a separate USB device; lighting and fan/pump control are handled elsewhere (typically your motherboard software). This app only touches the LCD.

**Does the screen keep working after I close the app?**
The last frame stays until the port closes; the app keeps the port open and holds the image with near-zero CPU. Closing the app blanks the screen.

**Is my cooler supported?** If Windows shows the LCD as `USB\VID_1F3A&PID_0007` and Smart Screen V28 drove it, it should work. Open an issue with your model if unsure.

## Keywords

Jungle Leopard Astroshel LCD 240 ARGB AIO · Astroshel LCD cooler software · Smart Screen V28 replacement / alternative · AIO liquid cooler LCD screen app · 320×320 pump-head display · custom image / GIF / video on cooler screen · lightweight low-CPU cooler LCD software · open-source Allwinner USB LCD (VID_1F3A PID_0007) driver for Windows.

## Disclaimer

Unofficial, community-built software provided as-is. Use at your own risk. Not affiliated with Jungle Leopard or the Smart Screen V28 authors.
