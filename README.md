# Astroshel Lean Display

A lean, native Windows app for the **Jungle Leopard Astroshel LCD 240 ARGB AIO** liquid
cooler's built-in 320×320 screen — a lightweight replacement for the bundled
`Smart Screen V28` software.

The stock app streams full-motion video to the little screen continuously, using hundreds
of MB and thousands of CPU-seconds. This app drives the same screen from a small native
binary that sits in the tray.

## Features

- **Sensor dashboard** — GPU temperature gauge, CPU / GPU usage bars, and a 12- or 24-hour clock.
- **Media mode** — display any image or animated GIF, with **Contain / Cover / Stretch**
  fitting plus **zoom and drag-to-pan**, shown against a live 320×320 preview (what you see
  is exactly what the cooler shows).
- **Live brightness** control (1–100%).
- Lives in the **system tray**; the window opens on click and hides back to the tray on close.
- Starts at logon; reconnects automatically if the cooler is unplugged.

## Usage

Run `astroshel-lean-display.exe`. It starts minimized to the tray (look for the disc icon).

- **Double-click the tray icon** (or right-click → Open) to open the window.
- Pick **Dashboard** or **Media** mode. In Media mode, click **Open image/GIF…**, frame it
  with the fit buttons / zoom / drag, then **Apply to cooler**.
- **Right-click the tray → Quit** to exit.

Settings persist to `%LOCALAPPDATA%\AstroshelLcd\config.toml`; logs to
`%LOCALAPPDATA%\AstroshelLcd\astroshel-lcd.log`.

## Build

Requires the Rust toolchain (and, for the sensor dashboard, an NVIDIA driver for GPU stats).

```
cargo build --release
```

The result is a single self-contained `astroshel-lcd.exe` in `target/release`.
