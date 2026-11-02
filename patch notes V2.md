We need to refactor and upgrade our PipeWire Linux audio software, **Studio Mic**, incorporating all recent architectural adjustments, contradiction fixes, **Tab Order 2** (`Tab 1 = Mixer`, `Tab 2 = EQ`, `Tab 3 = Manual Fine-Tune`, `Tab 4 = Test/Optimize`), **Option A stacked dual sliders** per channel card, and **Raw Audio Passthrough** for all non-microphone channels.

### IMPORTANT: CODEBASE AUDIT & NON-DESTRUCTIVE UPGRADE RULE
Before writing code, inspect the existing **Studio Mic** codebase to see which features are already partially or fully implemented (such as the Noise Gate/Expander, Dedicated Virtual Channel Presets, EQ, De-Esser, and Waveform views).
- **If a feature is NOT implemented yet:** Implement it cleanly according to the specifications below.
- **If a feature IS already implemented:** Do NOT duplicate it or break working functionality—instead, **upgrade and refactor the existing implementation in place** to incorporate all architectural improvements, bug fixes, real-time PipeWire parameter updates, and UI refinements detailed in this document.

---

### 1. Two-Stage Installer, Dependency Script & StreamController Plugin Setup (`install.sh` & `setup-deps.sh`)
- **Requirement:** When the user runs `./install.sh`, it must prompt the user asking if they want to download and install required dependencies (PipeWire tools, DeepFilterNet, RNNoise, Speex, LADSPA plugins). If confirmed, `install.sh` must automatically execute `./setup-deps.sh`.
- **Implementation:**
  1. **`install.sh`:**
     - Inspect system binaries (`pw-cli`, `pw-dump`, `pw-mon`, `pw-loopback`, `pw-play`, `pw-record`, `wpctl`) and prompt: `"Would you like to download and install required PipeWire and Noise Suppression dependencies for Studio Mic (DeepFilterNet, RNNoise, Speex)? [Y/n]"`. If yes, run `chmod +x ./setup-deps.sh && ./setup-deps.sh`.
     - Detect if **StreamController** is installed natively (`~/.local/share/StreamController/plugins/`) or via Flatpak (`~/.var/app/com.core447.StreamController/data/plugins/`) and install/symlink the bundled `com.studiomic.controller` plugin.
  2. **`setup-deps.sh`:** Detect distro via `/etc/os-release` (Arch/CachyOS, Fedora, Debian/Ubuntu), install native packages (`pipewire`, `wireplumber`, `ladspa`, `swh-plugins`, `lsp-plugins-ladspa`, `speexdsp`, `noise-suppression-for-voice`), and automatically download precompiled `.so` binaries (`libdeep_filter_ladspa.so`, `librnnoise_ladspa.so`) into `~/.local/lib/ladspa/` if missing.
  3. **Runtime `LADSPA_PATH` Resolution:** Ensure `/usr/lib/ladspa:/usr/lib64/ladspa:/usr/local/lib/ladspa:$HOME/.local/lib/ladspa` is exported to `LADSPA_PATH` on startup.

---

### 2. CRITICAL: Two-Tier Multi-Bus PipeWire Architecture & Strict DSP Separation
- **Problem:** Adjusting settings currently causes audio delay or disconnects PipeWire, and routing game/console audio through voice filters adds latency and garbles game music.
- **Fix (Two-Tier Persistent Multi-Bus Architecture & Strict DSP Isolation):**
  1. **Tier 1 - Four Persistent Output Buses (`node.linger=true`):**
     - Spawn four permanent virtual audio buses on startup that NEVER restart or disconnect while **Studio Mic** is open:
       - **`StudioMic_Voice_Out` (`Audio/Source/Virtual` - "Studio Mic (Microphone Only)"):** Outputs ONLY the post-DSP microphone signal for **Discord** / in-game voice chat. **HARD FIREWALL RULE:** ONLY the Microphone channel connects here. Non-mic channels (Capture Card, Game, Music, Browser, Active Apps) are strictly forbidden from linking to `StudioMic_Voice_Out`.
       - **`StudioMic_Stream_Mix` (`Audio/Source/Virtual` - "Studio Mic (Stream Mix)"):** Master broadcast mix for **OBS Studio** (Live Stream Track 1).
       - **`StudioMic_VOD_Mix` (`Audio/Source/Virtual` - "Studio Mic (VOD Track - DMCA Safe)"):** Secondary broadcast mix for **OBS Studio Twitch VOD Track 2** that excludes any channel with its "VOD Track" toggle unchecked (unchecked by default on `Music`).
       - **`StudioMic_Monitor_Mix` (`support.null-audio-sink` looped to user's selected physical Headphones/Output sink):** Private headphone monitoring bus controlled by a dedicated **Master Monitor Gain** slider.
     - **Dual Mix + VOD Routing Per Channel:** Every channel splits its output into independent branches feeding `StudioMic_Stream_Mix` (Stream Gain + Stream Mute), `StudioMic_Monitor_Mix` (Monitor Gain + Monitor Mute + Solo), and `StudioMic_VOD_Mix` (VOD toggle).
  2. **Strict DSP Firewall (Microphone Only):**
     - **DSP Filter-Chain (Denoiser ➔ Gate ➔ De-Esser ➔ EQ ➔ Compressor ➔ Clipguard):** Applies **STRICTLY to the Microphone**.
     - **Raw Audio Passthrough:** All other channels (Capture Card / Nintendo Switch, Game, Active Apps, Browser, Music, Voice Chat) bypass the DSP filter-chain entirely. They provide 100% raw stereo passthrough (zero DSP latency) with independent gain and mute controls for Stream and Monitor mixes.
  3. **Tier 2 - Live Parameter Updates (`pw-cli set-param`) for ALL Sliders:**
     - NEVER kill or reload the `filter-chain` when adjusting sliders or toggles. Update all active Noise Remover sliders, Smart Gate/Expander sliders, De-Esser sliders, Compressor controls, Clipguard, and EQ bands live in real time using:
       `pw-cli set-param <filter_node_id> Props '{ params = [ "<node_name>:<port_name>" <float_value> ] }'`
       (Throttle rapid slider drag updates to ~16–30ms intervals).
  4. **Selectable Noise Suppression Engines & Exact Port Mappings (Single-Engine Loading for Zero CPU Waste):**
     - Load ONLY the 1 active Noise Remover engine in the mic `filter-chain` at a time. When switching engines in the dropdown (`Deep Noise Remover` vs. `RNNoise` vs. `Speex`), swap only the internal Tier 2 chain behind the scenes and immediately re-link to the Tier 1 buses so external apps never disconnect.
     - **Deep Noise Remover (`libdeep_filter_ladspa`):** Map sliders to `"Attenuation Limit (dB)"` (`0.0` to `100.0`), `"Min processing threshold (dB)"`, and `"Max ERB processing threshold (dB)"`.
     - **RNNoise (`librnnoise_ladspa`):** Map sliders to `"VAD Threshold (%)"` (`0.0` to `99.0`), `"VAD Grace Period (ms)"`, and `"Retroactive VAD Grace (ms)"`.
     - **Speex (`speex_preprocess` / SpeexDSP):** Map slider to `"Noise Suppression (dB)"` (`-1` to `-60` dB).
     - **Parametric EQ & Compressor:** Map sliders to biquad (`"Freq"`, `"Q"`, `"Gain"`) and compressor (`"Threshold"`, `"Ratio"`, `"Attack"`, `"Release"`, `"Makeup Gain"`) ports.
  5. **Upgrade Existing Downward Expander / Smart Noise Gate:**
     - Upgrade the existing Gate/Expander implementation in place with:
       - **Hysteresis Dual-Threshold:** When the user adjusts `"Click / Gate Threshold"` (`Open_dB`) in Tab 3, automatically set the Close threshold `5.0 dB` lower (`Open_dB - 5.0 dB`) to prevent gate chattering on mechanical keyboards and preserve word tails.
       - **Vocal Key Filter (`150 Hz - 3500 Hz`):** Filter the gate sidechain detector between `150 Hz` and `3500 Hz` so low desk thuds and high mouse clicks don't false-trigger the gate.
       - **"Auto-Set Gate (3s Keyboard Test)" Button in Tab 3:** Listens for 3 seconds while the user types without speaking, then sets the Gate Threshold `+3.0 dB` above the peak typing noise.
  6. **Toggleable Software "Clipguard" (Brickwall Limiter + Hardware Auto-Pad):**
     - Add a **"Clipguard" toggle switch** on the Mic channel (persisted in state).
     - **Stage A (32-Bit Float Brickwall Limiter):** Place `fast_lookahead_limiter_1913` (`swh-plugins`) at the end of the mic `filter-chain` (`-0.5 dB` ceiling, `5ms` release), toggled live via `pw-cli set-param`.
     - **Stage B (Hardware Preamp Auto-Pad):** When Clipguard is ON and raw hardware input exceeds `-0.5 dBFS` (98%), step down hardware ALSA/PipeWire capture volume slightly via `wpctl set-volume` while compensating with digital makeup gain in the 32-bit float chain.
  7. **Toggleable Stream-Only Relative Auto-Ducking (With Discord, Preview & Mute Guard):**
     - Add an **"Auto-Duck" toggle switch** on Active Apps, Auxiliary Hardware / Capture Card channels, and preset channels (ON by default for `Game`, `Capture Card`, `Music`, `Browser`; OFF by default for `Voice Chat`).
     - **Stream & VOD Mix Only:** Apply ducking attenuation ONLY to the branches feeding `StudioMic_Stream_Mix` and `StudioMic_VOD_Mix` so `StudioMic_Monitor_Mix` (user's headphones—including console gameplay!) remains at steady 100% set volume.
     - **Exemptions:** Automatically exempt voice chat streams (`discord`, `vesktop`, `webcord`, `teamspeak`, `mumble`) from Auto-Ducking, and **suspend Auto-Ducking entirely while Tab 2/3 Preview Playback is running**.
     - **Relative Ducking Math:** Subtract ducking offset relative to the channel's current Stream Gain slider (`G_base_dB`):
       - Trigger Threshold: `-35.0 dB`. When `L_mic_dB > -35.0 dB`, `duck_offset_dB = min(12.0, (L_mic_dB - (-35.0)) / 1.5)` (max `-12.0 dB` reduction).
       - Envelope: `50ms` attack, `400ms` hold, `800ms` smooth release back to `G_base_dB`. Keep UI Gain slider handle stationary at `G_base_dB`.

---

### 3. Real-Time Gain Sync, Unified dB-to-Percentage Math & UI Renaming
- **Rename:** Rename all UI labels from "Fader" / "Faders" to **"Gain"**.
- **Add Master Monitor Gain Control:** Add a dedicated **Monitor Gain** control (`dB` / `%`) for the local headphone monitor mix.
- **Unified dB <-> Percentage Conversion Helper:**
  - Standard amplitude conversion: `linear_amplitude = 10 ^ (dB / 20)`, `percentage = linear_amplitude * 100`, `dB = 20 * log10(percentage / 100)` (where `0.0 dB = 100%`, `-6.0 dB = 50%`, and `0% = -Infinity / -60 dB`). Ensure consistency across all UI components and PipeWire volume sync.
- **Two-Way Real-Time Sync & Boot State:**
  - On startup, query actual current PipeWire node volumes (`pw-dump` / `wpctl get-volume`) so Gain sliders reflect real system volume.
  - Run a background listener (`pw-mon` / `pactl subscribe`) for live two-way volume sync.
  - **Initial Launch Defaults:** On first boot, the Microphone's `📡 Stream Gain` defaults to `0.0 dB (100%, Unmuted)`, while its `🎧 Monitor Gain` defaults to **Muted (`[🎧 OFF]`)** to prevent self-echo. All other channels (Capture Card, Game, Music, Apps, Voice Chat) default to **Unmuted** on both mixes.

---

### 4. State Persistence, Input Rescanning, Auxiliary Hardware Inputs (Capture Cards) & Virtual Channels
- **Unified State Persistence (`~/.config/studio-mic/state.json`):**
  - Save and restore: selected hardware Input `node.name`, Auxiliary Hardware Input / Capture Card channels (`node.name`, independent Stream/Monitor Gain, Mute, Solo, and Auto-Duck states), Clipguard toggle, VOD Track toggles, active Noise Remover engine & slider values, Gate threshold, active EQ Preset name, custom EQ band values, Compressor settings, Simple/Normal View mode, and routed Active Apps.
  - **Stop Defaulting to System Input:** Bind to the user's saved input `node.name` on launch.
  - **New Channel Initialization:** Initialize new channels with clean default values.
  - **Preset EQ UI:** Highlight the currently applied EQ preset on startup.
- **Missing Input Handling & Rescan Button:**
  - If a saved input `node.name` (mic or capture card) is missing/unplugged on launch, DO NOT switch IDs—display `"Missing"` in the input selector and provide a **"Refresh"** button to rescan devices on demand.
- **Auxiliary Hardware Input / Capture Card Channels (Raw Console Monitoring):**
  - Allow adding **Hardware Input / Capture Card channels** (for Nintendo Switch HDMI USB Capture Cards, Line-In consoles, or secondary audio interfaces) selectable from any physical `Audio/Source` device.
  - **Zero-Latency Clean Stereo Passthrough:** Route as clean stereo passthrough (bypassing the voice DSP chain).
  - **Independent Monitor vs. Stream Control:** Independent **Monitor Gain & Mute** (`StudioMic_Monitor_Mix`) and **Stream Gain & Mute** (`StudioMic_Stream_Mix`), plus **Auto-Ducking** (ducking Stream Mix only when speaking) and drift-free adaptive resampling (`pw-loopback`).
- **Upgrade Existing Dedicated Virtual Channel Presets (`Game`, `Music`, `Voice Chat`, `Browser`, `SFX`):**
  - **Smart Auto-Routing Rules** (`pw-dump` `application.process.binary` / `application.name` matching):
    - `Game`: `steam`, `wine`, `proton`, `gamescope`, `lutris`
    - `Music`: `spotify`, `feishin`, `elisa`, `rhythmbox`, `mpd`, `amberol` (VOD Track OFF by default)
    - `Voice Chat`: `discord`, `vesktop`, `webcord`, `teamspeak`, `mumble` (Auto-Duck OFF by default)
    - `Browser`: `firefox`, `chromium`, `brave`, `zen`, `vivaldi`, `chrome`
  - Add a **Solo ("S")** toggle on every channel that mutes all other channels strictly in `StudioMic_Monitor_Mix` (Headphones).

---

### 5. Active Apps Routing, Dropdown & Auto App Mode UI
- **Persist Active Apps:** Save routed applications and restore their links automatically.
- **Active Apps Dropdown:** Show a scrollable dropdown of currently running system apps actively producing audio (`Stream/Output/Audio`), plus **"Auto (Desktop Audio)"**.
- **Auto App Mode Browser Icon & Switcher:**
  - Display a small icon for the active web browser or application currently producing audio.
  - On hover, show a tooltip description. Clicking the icon opens a modal to switch applications while keeping **"Add Application"** functional.

---

### 6. Explicit 4-Tab Order (Tab 1 & Tab 2 Swapped), Tab 4 Recording Timers & Isolated Live Preview
- **Explicit 4-Tab Order (Order 2):**
  - **Tab 1 (First Tab):** **Monitoring & Channel Mixer** (Houses Normal Waveform Mode & Simple Mode with Option A stacked dual sliders, Capture Card raw passthrough, and Active Apps).
  - **Tab 2 (Second Tab):** **EQ, Compression & Sound Shaping** (+ Isolated Live Preview Player bar).
  - **Tab 3 (Third Tab):** **Manual Fine-Tune: Noise Removal (DeepFilter/RNNoise/Speex), Smart Gate & De-Esser** (Enabled by default with unlock button removed + Isolated Live Preview Player bar).
  - **Tab 4 (Fourth Tab):** **Voice Test Recorder, De-Esser Sibilance Test & One-Click Optimize** (with "One-Click Optimize" placed here as a separate option alongside the De-Esser Test).
- **Tab 4 De-Esser Test Prompt & Recording Timers:**
  - Set De-Esser test prompt text to:
    `"She sells seashells by the seashore, and six swiss wristwatches swiftly buzzed past the station."`
  - **Temporary Dry Voice Recording (`/tmp/studio-mic-preview.wav`):**
    - Capture unprocessed dry audio from the mic with a **10-second maximum recording countdown timer** (and manual stop button).
    - Maintain a **15-minute idle expiration timer** (resetting whenever the user plays preview on Tab 2 or 3), overwrite on new recording, and delete `/tmp/studio-mic-preview.wav` on app exit.
  - **De-Esser Redirection:** When the De-Essing test completes, ask if the user wants to adjust the **Manual Control in Tab 3** instead of EQ settings.
- **Isolated Live Real-Time DSP Preview Player on Tab 2 and Tab 3:**
  - Add a **"Preview Recording" (Play / Loop / Stop)** control bar on both **Tab 2** (EQ) and **Tab 3** (Manual Fine-Tune).
  - **Isolated Routing During Preview:**
    1. Temporarily **mute/unlink outputs to `StudioMic_Stream_Mix`, `StudioMic_VOD_Mix`, and `StudioMic_Voice_Out`**, suspend Auto-Ducking, and temporarily unlink the physical microphone from the DSP chain.
    2. Stream `/tmp/studio-mic-preview.wav` into the input of **Studio Mic's live PipeWire `filter-chain`** and route the processed output strictly to `StudioMic_Monitor_Mix` (Headphones), temporarily overriding the Mic's monitor mute state so the user hears the preview.
    3. Adjusting any **EQ, Compressor, Clipguard, De-Esser, Smart Gate, or Denoiser slider** on Tab 2 or Tab 3 updates the `filter-chain` live via `pw-cli set-param`.
    4. On **Stop**, immediately stop playback, relink the physical mic, and restore broadcast buses and mute states without disconnecting PipeWire.

---

### 7. Performance / Simple View Mode: Option A Stacked Sliders & Audio Meters
- **Objective:** Reduce single-core CPU usage from ~40% down to **5% – 10%** (max benchmark: 20%).
- **View Modes:**
  - **Normal Mode:** Standard waveform view running at capped **30 FPS**.
  - **Performance / Simple Mode Layout (Option A Stacked Dual Sliders):**
    - Convert all channel strips (Mic, Capture Card / Hardware Inputs, Virtual Channels, Active Apps) into **horizontal rows** that **stack vertically from top to bottom**.
    - Inside each stacked channel row (**Option A**):
      1. **Top Header Row:** Channel Name, Hardware Input Dropdown + Refresh button (for Mic & Capture Card rows), **Clipguard Toggle** (on Mic), **Auto-Duck & VOD Toggles** (on capture card / secondary channels), **Solo ("S")**, **Stream / Monitor Mute Toggles**, and live `dB / %` readouts.
      2. **Middle Row 1:** **Horizontal Material 3 Stream Gain Slider** (`📡 Stream Gain`).
      3. **Middle Row 2:** **Horizontal Material 3 Monitor Gain Slider** (`🎧 Monitor Gain`).
      4. **Bottom Row:** **Horizontal Material 3 Audio Level Meter** (Green -> Yellow -> Red gradient, 4-second peak hold line/curve, and 1-second solid red flash on peak fading back to green).
- **CPU Optimization:** Skip redraws when audio is idle/silent, batch UI updates, and use lightweight GPU/rect blitting to maintain 5%–10% single-core CPU usage.

---

### 8. Native StreamController Integration, Sandbox-Safe Unix Socket IPC & System Tray
- **Sandbox-Safe Unix Socket (`~/.cache/studio-mic/studio-mic.sock`):**
  - Place the Unix domain socket in `~/.cache/studio-mic/studio-mic.sock` (accessible by both native apps and Flatpak StreamController sandboxes).
  - Support non-blocking CLI flags via `studio-mic`:
    - `studio-mic --toggle-mic-mute`
    - `studio-mic --toggle-monitor-mute`
    - `studio-mic --channel-mute <ChannelName> <stream|monitor|vod|all>`
    - `studio-mic --channel-solo <ChannelName>`
    - `studio-mic --channel-gain <ChannelName> <stream|monitor> <+dB|-dB|exact_dB>`
    - `studio-mic --toggle-clipguard`
    - `studio-mic --toggle-autoduck <ChannelName>`
    - `studio-mic --set-denoiser <deepfilter|rnnoise|speex|off>`
    - `studio-mic --set-preset <EQPresetName>`
    - `studio-mic --toggle-view`
    - `studio-mic --status --json`
- **Bundled StreamController Plugin (`streamcontroller-plugin/com.studiomic.controller`):**
  - Build or update the StreamController plugin (`manifest.json`, `main.py`, action classes) connecting to `~/.cache/studio-mic/studio-mic.sock` for drag-and-drop actions inside StreamController (Mic Mute Toggle, Channel Stream/Monitor/VOD Mute including Capture Card, Channel Gain Dial/Button `+dB/-dB`, and DSP Quick Toggles), plus live state icons written to `~/.cache/studio-mic/streamcontroller/`.
- **System Tray Icon (`StatusNotifierItem` / AppIndicator):**
  - Display live state colors (Green = Live, Red = Muted, Amber = Preview Mode), support **Middle-Click** to toggle Mic Mute, **Scroll-Wheel** to adjust Master Monitor Gain, and `--start-minimized` for login autostart.