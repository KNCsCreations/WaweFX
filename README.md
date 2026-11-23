<div align="center">

# WaweFX

**Fast, lightweight, cross-platform desktop soundboard & live audio FX for Windows and macOS.**

[![Latest Release](https://img.shields.io/github/v/release/KNCsCreations/WaweFX?style=for-the-badge&color=38BDF8&labelColor=0D1017)](https://github.com/KNCsCreations/WaweFX/releases/latest)
[![Platform](https://img.shields.io/badge/Platform-Windows%20%7C%20macOS-6366F1?style=for-the-badge&labelColor=0D1017)](#download--installation)
[![Built With Rust](https://img.shields.io/badge/Built%20With-Rust-F97316?style=for-the-badge&logo=rust&logoColor=white&labelColor=0D1017)](#building-from-source)

[Website](https://kncscreations.github.io/WaweFX/) • [Download Latest](https://github.com/KNCsCreations/WaweFX/releases/latest) • [Audio Setup Guide](#audio-setup-guide-virtual-microphone) • [Report an Issue](https://github.com/KNCsCreations/WaweFX/issues)

</div>

---

## Overview

**WaweFX** is a native desktop soundboard written in Rust designed for Discord, gaming, streaming, and voice chat. It mixes sound effects directly into your virtual microphone alongside your live voice while giving you real-time pitch shifting, visual waveform trimming, YouTube-to-MP3 importing, animated GIF pad covers, and wireless phone control over local Wi-Fi.

---

## Key Features

| Feature | Description |
| :--- | :--- |
| **Dual-Stream Audio Routing** | Mixes your physical microphone with soundboard audio in real time via a lock-free ring buffer (`cpal` + `rodio`), outputting to a virtual audio cable while monitoring on your headphones with independent `0%–200%` volume knobs and instant mutes. |
| **Overlap Mode & Smart Re-Trigger** | Toggle **Overlap: ON / OFF** directly in the top toolbar or mobile remote. Layer multiple distinct sounds together, while re-triggering an active sound smoothly restarts it from the beginning instead of duplicating the track. |
| **Visual Waveform Trim & Gain** | Non-destructive start/end trimming with draggable handles on a rendered audio waveform, paired with per-pad volume (`0%–200%`) and live scrubbing. |
| **YouTube & Video to MP3 Import** | Paste any YouTube video or Shorts URL—or drop `.mp4`, `.mov`, `.mkv`, `.webm`, and `.avi` files—to automatically extract and convert audio to MP3 (with automatic `yt-dlp` and `ffmpeg` bootstrapping). |
| **Animated GIF & Custom Covers** | Drag and drop `.gif`, `.png`, `.jpg`, `.webp`, or `.bmp` artwork directly onto any pad, reorder cards via drag-and-drop, and clone pads with one click using **Duplicate**. |
| **Real-Time Pitch & Global Hotkeys** | Smooth, click-free pitch/speed slider (`0.50x–2.00x`) with configurable step ramps and system-wide hotkeys (`F1–F24` + modifiers) for individual pads and pitch controls. |
| **Wireless Phone Web Remote** | Built-in HTTP & WebSocket server (`tokio` + `axum`) with one-click **QR code pairing**. Control pads, order, Overlap mode, and themes from Safari or Chrome on iOS and Android—no app install required. |
| **Custom Themes & AppData Library** | Built-in presets (*Midnight Dark*, *Warm Beige*, *Nord Frost*) plus full RGB color pickers with saveable user presets. All sounds and covers are safely stored in your OS AppData folder. |

---

## Download & Installation

Pre-built binaries for **Windows** and **macOS** are available on the **[Releases page](https://github.com/KNCsCreations/WaweFX/releases/latest)**:

- **Windows (`x64`)**: Download `WaweFX-Windows.zip`, extract it, and run `WaweFX.exe`.
- **macOS (`Apple Silicon & Intel`)**: Download `WaweFX-macOS.dmg`, open it, and drag `WaweFX.app` into **Applications**.

> *(Development builds are also compiled automatically under the **[Actions](https://github.com/KNCsCreations/WaweFX/actions)** tab on every commit).*

<details>
<summary><strong>First-Launch Security Notes (Windows SmartScreen & macOS Gatekeeper)</strong></summary>

<br>

Because WaweFX is currently self-distributed without a paid Apple Developer or Windows Authenticode certificate, your OS may show a security prompt on first launch:

- **Windows (SmartScreen)**: Click **More info** → **Run anyway**.
- **macOS (Gatekeeper)**: Attempt to open `WaweFX.app` once, click **Done**, then open **System Settings → Privacy & Security**, scroll down to **Security**, and click **Open Anyway**.

</details>

---

## Audio Setup Guide (Virtual Microphone)

To route soundboard audio into Discord, games, or voice chat apps alongside your voice, install a free virtual audio driver:

<details open>
<summary><strong>Windows Setup (VB-CABLE)</strong></summary>

<br>

1. Install **[VB-CABLE Virtual Audio Device](https://vb-audio.com/Cable/)** (or use the built-in setup helper inside WaweFX Settings).
2. Open Windows Sound Settings (`mmsys.cpl`) and ensure your **Physical Microphone**, **CABLE Input**, and **CABLE Output** are all set to **48000 Hz** to prevent sample-rate pitch mismatch.
3. Open **WaweFX Settings (Gear icon)** and configure:
   - **Physical Mic Input**: Your hardware microphone
   - **Virtual Mic Output**: `CABLE Input (VB-Audio Virtual Cable)`
   - **Headphones Output**: Your headphones or speakers
4. In **Discord / Game Settings**, set your Input Device to `CABLE Output (VB-Audio Virtual Cable)`.

</details>

<details>
<summary><strong>macOS Setup (BlackHole 2ch)</strong></summary>

<br>

1. Install **[BlackHole 2ch](https://existential.audio/blackhole/)** (or via Homebrew: `brew install blackhole-2ch`, then run `sudo killall coreaudiod` in Terminal).
2. Open **WaweFX Settings (Gear icon)** and configure:
   - **Physical Mic Input**: Your hardware microphone
   - **Virtual Mic Output**: `BlackHole 2ch`
   - **Headphones Output**: Your headphones or speakers
3. In **Discord / App Settings**, set your Input Device to `BlackHole 2ch`.

</details>

---

## Mobile Remote Pairing

1. Ensure your PC/Mac and phone are connected to the **same local Wi-Fi network**.
2. Click the **Phone icon** in the WaweFX top toolbar to display the pairing QR code.
3. Scan the QR code with your iPhone or Android camera to launch the live remote control in your browser.
   *(On Windows, allow `WaweFX.exe` through Windows Defender Firewall for **Private Networks** when prompted).*

---

## Building from Source

### Prerequisites
- [Rust toolchain](https://rustup.rs/) (`stable`)

### Build & Run
```bash
git clone https://github.com/KNCsCreations/WaweFX.git
cd WaweFX
cargo run --release
```