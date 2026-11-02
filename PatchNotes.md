We need to refactor our PipeWire Linux audio software, **Studio Mic**, to fix several critical backend, state persistence, math, and UI bugs, integrate the automated dependency installer scripts, and implement the high-performance CPU-optimized audio meter view. Execute the following architectural changes across the codebase:

### 1. Two-Stage Installer & Dependency Script Execution (`install.sh` & `setup-deps.sh`)
- **Requirement:** When the user runs `./install.sh`, it must prompt the user asking if they want to download and install required dependencies (PipeWire tools, DeepFilterNet, RNNoise, Speex, LADSPA plugins). If confirmed, `install.sh` must automatically execute `./setup-deps.sh`.
- **Implementation:**
  1. **`install.sh`:** Inspect system binaries (`pw-cli`, `pw-dump`, `pw-mon`, `pw-loopback`, `wpctl`) and prompt: `"Would you like to download and install required PipeWire and Noise Suppression dependencies for Studio Mic (DeepFilterNet, RNNoise, Speex)? [Y/n]"`. If yes, run `chmod +x ./setup-deps.sh && ./setup-deps.sh`.
  2. **`setup-deps.sh`:** Detect distro via `/etc/os-release` (Arch/CachyOS, Fedora, Debian/Ubuntu), install native packages (`pipewire`, `wireplumber`, `ladspa`, `swh-plugins`, `lsp-plugins-ladspa`, `speexdsp`, `noise-suppression-for-voice`), and automatically download precompiled `.so` binaries (`libdeep_filter_ladspa.so`, `librnnoise_ladspa.so`) into `~/.local/lib/ladspa/` if missing.
  3. **Runtime `LADSPA_PATH` Resolution:** Ensure `/usr/lib/ladspa:/usr/lib64/ladspa:$HOME/.local/lib/ladspa` is exported to `LADSPA_PATH` on startup.

### 2. CRITICAL: Decoupled Persistent Virtual Sink & Real-Time Slider Updates
- **Problem:** Adjusting sliders (Denoiser controls, EQ, De-Esser) or changing settings currently restarts the PipeWire filter-chain and disconnects the virtual audio device, causing audio delays and dropping connections in external apps.
- **Fix (Two-Tier PipeWire Architecture):**
  1. **Tier 1 - Persistent Output Anchor (`support.null-audio-sink`):** Spawn a permanent virtual sink/source when **Studio Mic** launches (`node.linger=true`). External apps (OBS, Discord, browsers) connect ONLY to this node. This node must NEVER be destroyed or restarted when settings change.
  2. **Tier 2 - Live Parameter Updates (`pw-cli set-param`) for ALL Sliders:** Do NOT reload the filter-chain when moving any slider. Update all active Noise Remover sliders, De-Esser sliders, and EQ bands in real time using:
     `pw-cli set-param <filter_node_id> Props '{ params = [ "<node_name>:<port_name>" <float_value> ] }'`
     (Throttle slider updates to ~16-30ms intervals while dragging).
  3. **Exact Real-Time Denoiser Control Port Mappings:**
     - **Deep Noise Remover (`libdeep_filter_ladspa`):** Map strength slider to `"Attenuation Limit (dB)"` (`0.0` to `100.0`), thresholds to `"Min processing threshold (dB)"` and `"Max ERB processing threshold (dB)"`.
     - **RNNoise (`librnnoise_ladspa`):** Map sliders to `"VAD Threshold (%)"` (`0.0` to `99.0`), `"VAD Grace Period (ms)"`, and `"Retroactive VAD Grace (ms)"`.
     - **Speex:** Map to `"Noise Suppression (dB)"` (`-1` to `-60` dB).
     - **Parametric EQ:** Map to `"Freq"`, `"Q"`, and `"Gain"`.
  4. **Single-Engine Loading (Zero CPU Waste):** Only load the 1 currently selected Noise Remover engine in the `filter-chain` at a time. Swapping engines only reloads the internal Tier 2 chain behind the scenes while keeping the Tier 1 Persistent Sink connected.

### 3. Real-Time Gain Sync, dB-to-Percentage Math & UI Renaming
- **Rename:** Change all UI labels and references from "Fader" / "Faders" to **"Gain"**.
- **dB <-> Percentage Math Fix:** Fix the conversion formula between dB, linear amplitude, and system percentage to match PipeWire's cubic volume scaling:
  - `linear = 10 ^ (dB / 20)`
  - `percentage = (10 ^ (dB / 60)) * 100`
  - `dB = 60 * log10(percentage / 100)` (clamp at -Infinity / -60dB at 0%).
  - Ensure 100% strictly equals `0.0 dB`.
- **Two-Way Real-Time Sync & Boot State:** Query actual PipeWire node volume (`wpctl get-volume` or `pw-dump`) on startup. Run a background listener (`pw-mon` or `pactl subscribe`) for live two-way volume sync.

### 4. State Persistence & Hardware Input Rescanning
- **Unified Config Persistence (`~/.config/studio-mic/state.json`):** Save and restore selected hardware Input `node.name`, active Noise Remover engine & slider values, active EQ Preset name, custom EQ band values, Channel states, and routed Active Apps.
- **Stop Defaulting to System Input:** Bind to the user's saved input `node.name` on launch instead of system default.
- **Missing Input Handling & Rescan Button:** If the saved input `node.name` is missing/unplugged, display `"Missing"` in the input selector and provide a **"Refresh"** button to rescan devices on demand.

### 5. Active Apps Routing, Dropdown & Auto App Mode UI
- **Persist Active Apps:** Save routed applications by `application.process.binary` / `application.name`.
- **Active Apps Dropdown & Auto Mode:** When "Active Apps" is selected, display a dropdown of currently running system apps actively producing audio (`Stream/Output/Audio`), alongside **"Auto (Desktop Audio)"**. When in Auto App Mode, display a small icon for the active browser/app with a tooltip on hover, and clicking it opens a selector box.

### 6. UI Layout, Tab Reorganization & De-Esser Workflow
- **De-Esser Test Prompt Text:** Change test script text to a sibilance test phrase (`"She sells seashells by the seashore..."`).
- **Tab 4 & Manual Control Changes:** Move **"One-Click Optimize"** to the **4th Tab** as a separate option alongside the **De-Esser Test**. Enable **Manual Fine-Tune Control** by default and remove its toggle button.
- **De-Esser Prompt Redirection:** Prompt the user to adjust the **Manual Control in Tab 3** when finishing the De-Essing tool.

### 7. Performance / Simple View Waveform & Audio Meter Mode (New CPU Optimization)
- **Objective:** Reduce CPU usage from ~40% on a single core down to **5% – 10%** (benchmark target: max 20%).
- **View Modes:**
  - **Normal Mode:** Standard waveform rendering running at capped **30 FPS** with full visual settings.
  - **Performance / Simple Mode:** A lightweight OBS-style audio meter replacing complex waveforms with a single smooth vertical/horizontal bar that scales dynamically with volume.
- **Material 3 Styling & Peak Dynamics:**
  - Styled following **Google Material 3** design principles (matching the gain control aesthetics).
  - **Color Gradient:** Transitions smoothly from **Green -> Yellow -> Red** based on amplitude level.
  - **Peak Indicator Line/Curve:** Includes a trailing peak indicator line/curve showing the last maximum level reached, holding with a **4-second decay/delay**.
  - **Peak Red Flash:** If clipping/peaking occurs, the meter flashes **completely solid red for exactly 1 second**, then smoothly fades back down into the green/yellow/red gradient.
  - **Ultra-Lightweight Rendering:** Optimize painting loops, disable unnecessary canvas redraws when audio is idle, and batch UI repaints to achieve ultra-low CPU overhead (5%–10% single-core target).

### 8. New Features to Implement
1. **Monitor Gain Control:** Add a dedicated **Monitor Gain** slider (`dB` / `%`) controlling a local monitoring loopback stream (`pw-loopback`) independently of broadcast output.
2. **Selectable Noise Suppression Engines (RNNoise & Speex):** Add a dropdown in Noise Remover settings for **RNNoise** or **Speex** alongside Deep Noise Remover, dynamically mapping Tab 3 real-time sliders to the active engine.