# StudioMic: Linux Audio Tuning & Virtual Mixing Suite

> **Architectural Specification & Interactive Prototype**  
> **Target OS:** Omarchy / Arch-based Linux (PipeWire & WirePlumber)  
> **Design Standard:** Google Material Design 3 (Material You) + Google Material Symbols  
> **Reference Benchmark:** Shure MOTIV Mix (Windows/macOS) Full Parity & Superiority

---

## Using Studio Mic (v0.2)

```bash
./install.sh            # asks to fetch PipeWire/LADSPA deps, builds, installs for your user
studiomic               # open the app (or `studiomic --background` for the tray only)
studio-mic --status     # control it from a terminal / Stream Deck (see `studio-mic --help`)
```

* **Buses for OBS / Discord:** `Studio Mic (Microphone Only)` for Discord, `Studio Mic (Stream Mix)`
  for OBS Track 1, `Studio Mic (VOD Track - DMCA Safe)` for Track 2, and `Studio Mic (Monitor Mix)`
  is what you hear. The old `studiomic_processed_mic` name still works.
* **Nothing interrupts audio:** sliders and toggles change the running chain in place.
* **Settings** live in `~/.config/studio-mic/state.json` (your old `~/.config/studiomic/config.json` is
  migrated automatically and kept as `config.json.bak`). Named profiles: `studio-mic --save-profile NAME`.
* **Stream Deck:** the StreamController plugin in `streamcontroller-plugin/` is linked by the installer.
* Docs: `docs/audit-report.md` (what changed), `docs/verified-apis.md`, `docs/perf.md`.

## Overview

**StudioMic** is an open-source native Linux application design engineered to deliver professional broadcast microphone tuning, automated acoustic calibration, and multi-channel virtual mixing. It addresses the lack of unified streaming audio software on Linux, replacing proprietary tools like Shure MOTIV Mix, Elgato Wave Link, and SteelSeries Sonar.

---

## Key Features

1. **Top 20 Streaming Microphones Database (Brand-Sorted):**
   - Microphones organized alphabetically by brand (`Audio-Technica`, `BEACN`, `Blue / Logitech G`, `Elgato`, `FIFINE`, `HyperX`, `NZXT`, `Razer`, `Rode`, `Sennheiser`, `Shure`, `SteelSeries`, `Universal`).
   - Clean names without redundant parenthetical tags.
   - Includes both **Rode PodMic** (XLR) and **Rode PodMic USB**.

2. **10-Band Parametric EQ by Default:**
   - 10 active parametric bands across all factory and community presets.
   - **Dynamic Band Controls (`+` and `-`):** Add bands up to a **maximum cap of 10 bands** with `[+] Add Band`, or remove down to 1 band with `[-] Remove` or per-row delete buttons.
   - Interactive 60 FPS HTML5 Canvas with draggable frequency/gain nodes (`B1`–`B10`) and mouse-wheel Q factor adjustment.

3. **MOTIV Mix-Style 5-Channel Virtual Mixer & OBS Matrix:**
   - **Channel 1 (Locked Hardware Identity):** Dynamically tracks active microphone model and loaded profile.
   - **Channels 2+ (Renamable Mixes):** Inline renaming with Enter/Escape keyboard handling.
   - **Game Audio Hooking (Channel 2):** Live display of hooked game title (e.g. *Counter-Strike 2*), PID, and PipeWire node ID with auto-hook active window mode.
   - **Discord Voice Hooking (Channel 3):** Dedicated voice communication bus.
   - **Browser Per-Tab Sub-Routing (Channel 4):** Isolate individual browser tabs (e.g. *Zen Browser - YouTube Music*) to dedicated channels via PipeWire stream nodes.
   - **OBS Matrix Routing:** Dedicated toggle matrix for OBS Stream, Track 2 (Twitch VOD Shield), Track 3 (Mic Only), and Track 4 (Chat Isolated).

4. **VoiceMatch Soundcheck & Acoustic Calibration:**
   - Automated 5-second acoustic level calibration.
   - Displays Pre-Calibration vs. Post-Calibration gain metrics.
   - Interactive on-screen Gain Reduction / Trim slider for fine-tuning.

5. **Pure Google Material Design 3:**
   - Official Google Material Symbols ligatures (`https://fonts.google.com/icons`).
   - Zero Unicode emojis.
   - Standard M3 surface hierarchy, pill controls, and state layers.

---

## Package Contents

| File | Description |
| :--- | :--- |
| **`mic-studio-mockup.html`** | Standalone interactive prototype with 60 FPS EQ canvas, mixer faders, and modals. |
| **`concept-spec.md`** | Comprehensive technical and architectural blueprint with PipeWire graphs and parameters. |
| **`AGENT_INSTRUCTIONS.md`** | Comprehensive developer & Antigravity agent handover instructions. |
| **`StudioMic-Package.zip`** | Compressed package ready for Google Drive drop-off. |

---

## Quickstart

To run the interactive prototype on Linux:
```bash
# In the terminal:
xdg-open mic-studio-mockup.html

# Or open directly in your preferred browser:
zen-browser mic-studio-mockup.html
google-chrome mic-studio-mockup.html
firefox mic-studio-mockup.html
```

---

## Google Drive Handover

The workspace is bundled as a single zip archive for easy sharing:
- **`StudioMic-Package.zip`**
- Drop this file directly into your Google Drive folder.
