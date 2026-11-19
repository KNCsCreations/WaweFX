# WaweFX

**WaweFX** is a fast, lightweight, cross-platform desktop soundboard written in Rust for **Windows** and **macOS**. It features real-time virtual microphone audio mixing, live pitch/speed shifting with smooth ramp transitions, system-wide global hotkeys, customizable themes with saveable user presets, drag-and-drop pad & cover management, a self-contained AppData library, and instant browser-based mobile remote control over local Wi-Fi (iOS & Android).

---

## Download & Installation

Pre-built installers and binaries for **Windows** and **macOS** are available on the **[Releases page](https://github.com/KNCsCreations/WaweFX/releases)**:

1. Open the **[Releases](https://github.com/KNCsCreations/WaweFX/releases)** section on the right sidebar of this repository and select the latest release.
2. Under **Assets**, download the file for your operating system:
   - **Windows**: `WaweFX-Windows.zip` (extract and run `WaweFX.exe`)
   - **macOS**: `WaweFX-macOS.dmg` (open the `.dmg` and drag `WaweFX.app` into **Applications**)

### First-Launch Security Notes
Because WaweFX is currently self-distributed without a paid Apple Developer / Windows Authenticode certificate, your OS may show a security prompt on first launch:
- **Windows (SmartScreen)**: Click **More info** → **Run anyway**.
- **macOS (Gatekeeper)**: Try opening `WaweFX.app` once, click **Done**, then go to **System Settings → Privacy & Security**, scroll down to **Security**, and click **Open Anyway**.

*(Latest development builds are also generated automatically under the **Actions** tab → **Artifacts** on every commit).*

---

## Features

- **Dual-Stream Audio Routing & Mic Passthrough**
  - Mixes your physical microphone input with soundboard audio in real time via a lock-free ring buffer (`cpal` + `rodio`), outputting directly to a virtual audio cable while simultaneously monitoring through your headphones.
  - Independent rotary volume knobs (`0%–200%`) and instant mute toggles for both **Microphone Output** and **Headphones Monitoring**.
- **Real-Time Pitch & Speed Control + Smooth Global Hotkeys**
  - Vertical pitch/speed slider (`0.50x` to `2.00x`) using click-free fractional-index resampling—changes apply smoothly without affecting your voice passthrough.
  - Bind system-wide global hotkeys (`F1–F24` + modifiers) to individual sound pads as well as **Pitch Up (`+`)**, **Pitch Down (`-`)**, and **Pitch Reset**, with a configurable step increment and smooth ramp interpolation so pitch shifts never click or pop.
  - Interactive top seek/timeline bar (`MM:SS`) with muted scrubbing (commits cleanly on release without crackling), rewind (`-5s`), and fast-forward (`+5s`).
- **Soundboard Grid & Drag-and-Drop (Desktop & Mobile)**
  - Supports `.mp3`, `.wav`, `.ogg`, and `.flac` audio files with custom cover thumbnails (`.png`, `.jpg`, `.jpeg`, `.webp`, `.bmp`) decoded asynchronously on a background thread.
  - **Drag-and-Drop Reordering**: Rearrange sound pads by dragging and dropping them on either the desktop app or the mobile web remote, with instant cross-device synchronization.
  - **Drag-and-Drop Cover Art**: Drop any image file directly onto a sound pad card to assign its custom thumbnail.
- **Self-Contained AppData Library**
  - All imported audio files, custom pad covers, and `config.json` are automatically copied into your OS user data folder (`%APPDATA%\WaweFX\` on Windows, `~/Library/Application Support/WaweFX/` on macOS)—moving or deleting original files from your `Downloads` folder will never break your soundboard.
- **Instant Mobile Remote Control (iOS & Android — No App Install Needed)**
  - Embedded `tokio` + `axum` HTTP & WebSocket server with one-click **QR code pairing**.
  - Scanning the QR code opens a touch-optimized web soundboard in Safari or Chrome that syncs your sounds, cover images, playback state, pad order, and custom theme colors in real time.
- **Customizable Themes & Saveable User Presets**
  - Defaults to **Midnight Dark** alongside built-in presets (*Warm Beige*, *Nord Frost*, etc.) plus full custom RGB color pickers that you can save as named user presets and sync across desktop and mobile.
- **Automatic Update Checker**
  - Performs a non-blocking check against GitHub Releases on startup and displays a clickable **New version available** badge in the top toolbar whenever a newer release is published.

---

## Audio Setup Guide (Virtual Microphone)

To route soundboard audio into Discord, games, or voice chat apps alongside your voice, install a free virtual audio driver:

### Windows
1. Install **[VB-CABLE Virtual Audio Device](https://vb-audio.com/Cable/)** (or use the built-in setup helper in WaweFX Settings).
2. Open Windows Sound Settings (`mmsys.cpl`) and ensure your **Physical Microphone**, **CABLE Input**, and **CABLE Output** are all set to **48000 Hz** to prevent sample-rate pitch mismatch.
3. In **WaweFX Settings (Gear icon)**:
   - **Physical Mic Input**: Your hardware microphone
   - **Virtual Mic Output**: `CABLE Input (VB-Audio Virtual Cable)`
   - **Headphones Output**: Your headphones/speakers
4. In **Discord / Game Settings**, set your Input Device to `CABLE Output (VB-Audio Virtual Cable)`.

### macOS
1. Install **[BlackHole 2ch](https://existential.audio/blackhole/)** (or via Homebrew: `brew install blackhole-2ch`, then run `sudo killall coreaudiod` in Terminal).
2. In **WaweFX Settings (Gear icon)**:
   - **Physical Mic Input**: Your hardware microphone
   - **Virtual Mic Output**: `BlackHole 2ch`
   - **Headphones Output**: Your headphones/speakers
3. In **Discord / App Settings**, set your Input Device to `BlackHole 2ch`.

---

## Mobile Remote Pairing

1. Make sure your PC/Mac and phone are connected to the **same local Wi-Fi network**.
2. Click the **Phone icon** in the WaweFX top toolbar to display the pairing QR code.
3. Scan the QR code with your iPhone or Android camera to open the live remote control in your browser.
   *(On Windows, allow `WaweFX.exe` through Windows Defender Firewall for Private Networks when prompted).*

---

## Building from Source

### Prerequisites
- [Rust toolchain](https://rustup.rs/) (`stable`)

### Build & Run
```bash
git clone [https://github.com/KNCsCreations/WaweFX.git](https://github.com/KNCsCreations/WaweFX.git)
cd WaweFX
cargo run --release
