# WaweFX

**WaweFX** is a fast, lightweight, cross-platform desktop soundboard written in Rust for **Windows** and **macOS**. It features real-time virtual microphone audio mixing, live pitch/speed shifting, customizable themes, drag-and-drop pad management, and instant browser-based mobile remote control over local Wi-Fi (iOS & Android).

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
- **Real-Time Pitch & Speed Control**
  - Vertical pitch/speed slider (`0.5x` to `2.0x`) using click-free fractional-index resampling—changes apply instantaneously mid-playback without affecting your voice passthrough.
  - Interactive top seek/timeline bar (`MM:SS`) with instant scrubbing, rewind (`-5s`), and fast-forward (`+5s`).
- **Soundboard Grid & Drag-and-Drop**
  - Supports `.mp3`, `.wav`, `.ogg`, and `.flac` audio files with custom cover thumbnails (`.png`, `.jpg`, `.webp`) decoded asynchronously on a background thread.
  - Drag and drop sound pads to reorder them, or drop audio/image files directly from your file manager onto the window.
- **Instant Mobile Remote Control (iOS & Android — No App Install Needed)**
  - Embedded `tokio` + `axum` HTTP & WebSocket server with one-click **QR code pairing**.
  - Scanning the QR code opens a touch-optimized web soundboard in Safari or Chrome that syncs your sounds, cover images, playback state, and custom theme colors in real time.
- **Customizable Themes**
  - Includes built-in presets (*Classic WaweFX Lilac*, *Midnight Dark*, *Nord Frost*) plus full custom RGB color pickers persisted locally.

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
