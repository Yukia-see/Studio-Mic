# Studio Mic: Implementation Plan for Claude

This plan outlines the sequential tasks to fix all build and engine bugs identified in `bugreport.md` and implement the Elgato Wave Link routing table matrix for Tab 1 in `mic-studio-mockup.html`.

Refer to [`CLAUDE.md`](file:///home/yukia/Documents/StudioMic-Package/CLAUDE.md) for architectural invariants (especially process topology, the live-change rule, and lock ordering).

---

## Task Overview

1. **Phase 1: Build System & Packaging Fixes** (Fedora, Arch, Cargo dependencies)
2. **Phase 2: Core Engine Bug Fixes** (Races, routing, metering, state corruption)
3. **Phase 3: Tab 1 Wave Link Routing Matrix UI** (Interactive cross-point matrix)
4. **Phase 4: Verification & Regression Testing**

---

## Phase 1: Build System & Packaging Cleanup

### 1.1 `Cargo.toml`
- Remove unused dependency: `pipewire = "0.10.1"` (resolves Fedora build failure and removes need for `clang`/`libpipewire-devel`).
- Remove unused dependency: `slint = "1.18.1"`.
- Remove unused build dependency: `slint-build = "1.18.1"`.
- Declare minimum Rust version: `rust-version = "1.89"` (required by `File::try_lock` in `src/ipc.rs:46`).
- Minimize tokio features: change `tokio = { version = "1.54.1", features = ["full"] }` to `features = ["rt", "signal"]`.

### 1.2 `build.rs` & `ui/`
- In `build.rs`, remove `println!("cargo:rerun-if-changed=ui/studiomic.slint");` and `slint_build::compile("ui/studiomic.slint").expect(...);`.
- Delete the unused file `ui/studiomic.slint`.

### 1.3 `packaging/arch/PKGBUILD`
- Line 31: Remove `if [ ! -f "target/release/StudioMic-Package" ]; then` wrapper so `cargo build --release --locked` always runs to prevent stale binaries.
- Line 16: Remove `clang` from `makedepends`.
- Line 20: Add `!debug` to options: `options=('!lto' '!debug')` to prevent building unwanted `-debug` packages.

### 1.4 `packaging/fedora/studio-mic.spec`
- Line 10: Remove `clang` from `BuildRequires`.
- Lines 29–31: Remove the conditional file check around `cargo build --release --locked`.

### 1.5 `setup-deps.sh` & `install.sh`
- In `setup-deps.sh`: When `--stage DIR` is passed, do NOT install plugins into `~/.local/lib/ladspa` and do NOT create `~/.config/environment.d/60-studio-mic-ladspa.conf`. Only stage into `DIR`.
- In `install.sh` line 80: Adjust the pacman glob to `studio-mic-[0-9]*.pkg.tar.zst` to avoid matching debug packages.

---

## Phase 2: Core Engine & Routing Bug Fixes

### 2.1 Concurrency & Mic Chain Swap (`src/audio/daemon.rs`)
- **Bug 1 (Voice_Out dropout)** & **Bug 2 (Auto-duck mic doubling/click)**:
  - In `src/audio/daemon.rs`, `swap_mic_chain()` is executed under `self.ops`.
  - Ensure `relink_all()` and `apply_mixer()` synchronize with the swap lock (`let _ops = self.ops.lock().unwrap();` or dedicated `swap_guard` as noted in `CLAUDE.md`).
  - This prevents the 2-second maintenance relink from cutting the incoming chain's `Voice_Out` links while crossfading, and stops ducking from restoring full volume on the retiring chain.

### 2.2 WirePlumber App Unassignment (`src/routing/wireplumber_hook.rs` & `src/app.rs`)
- **Bug 3 (Unassigning apps leaves them in old channel)**:
  - In `src/routing/wireplumber_hook.rs:315`, remove `ROUTED.lock().unwrap().clear()` inside `invalidate()`.
  - Keeping `ROUTED` populated allows `auto_route()` to recognize when `wanted == None` and `current == Some(old_ch)`. It then invokes `pw::clear_stream_target(s.id)` and removes the stream from `ROUTED`.
  - In `src/app.rs:691` (`unassign_app`) and `:668` (`assign_app`), trigger `StreamWatcher::auto_route(&s.virtual_channels)` and `self.sync_ui()` immediately.

### 2.3 Metering Fixes (`src/audio/daemon.rs`)
- **Bug 4 (Silent channels show 20% meter when fader is >0 dB)**:
  - In `block_peak_db(buf)` (`daemon.rs:196`), floor true silence (`max_abs <= 0.0001` or `max_abs == 0.0`) at `-80.0` instead of calling `lin_to_db` which was clamping at `-60.0`.
  - In `peak_map()` (`daemon.rs:1328`), compute post-fader send `out` only when `pre > -60.0` (flooring at `-80.0`).
- **Bug 8 (Unplugged mic meters default input)**:
  - In `desired_meters()` (`daemon.rs:1166`), only insert `raw` meter if `Self::hw_mic_present(hw)` is true. In `spawn_meter()`, verify `pw::find_node(t).is_some()`.

### 2.4 Clipguard & Auto-pad Restores (`src/app.rs` & `mic-studio-mockup.html`)
- **Bug 5 (Clipguard toggle on Tab 3 fails to restore hardware level & UI hardcodes ceiling)**:
  - In `src/app.rs:483` (`set_noise_clean`), if `!nc.limiter_enabled`, enqueue `Job::RestoreAutopad`.
  - In `mic-studio-mockup.html:6108`, preserve `prev.limiter_ceiling_db ?? -0.5` instead of hardcoding `-0.5`.
- **Bug 11 (Auto-pad not restored on mic change or profile load)**:
  - In `src/app.rs:545` (`set_hw_mic`), invoke `self.engine.restore_autopad()` before mutating `s.active_hw_mic`.
  - In `src/app.rs:759` (`load_profile`), invoke `self.engine.restore_autopad()` before replacing state and enqueuing jobs.

### 2.5 Autostart Configuration (`src/app.rs` & `src/autostart.rs`)
- **Bug 6 (Launch on Login always starts minimized)**:
  - In `src/app.rs:1326` (`toggle_autostart`), pass `s.autostart_minimized` to `AutostartManager::set_enabled_with_minimized(on, s.autostart_minimized)`.
  - In `toggle_start_minimized`, update the desktop entry if autostart is already active.
- **Bug 10 (Autostart path edge cases)**:
  - In `src/autostart.rs`:
    - Remove hardcoded `/home/yukia` fallback; resolve `HOME` cleanly or gracefully handle absence.
    - Quote `current_exe` in desktop `Exec="<path>" ...` to handle paths with spaces.
    - Strip trailing `" (deleted)"` from `current_exe()` after package upgrades.

### 2.6 State Resilience & Performance Edge Cases
- **Bug 7 (NaN in calibration trim corrupts state.json)**:
  - In `src/app.rs:1195` (`calib_trim`) and `:1204` (`save_calibration`), verify floats with `.is_finite()`, falling back to `0.0`.
- **Bug 9 (PCI capture card misidentifies all microphones)**:
  - In `src/audio/device_discovery.rs:183`, verify that `audio_dev` specifically belongs to the sound card under `canonical_parent`, rather than matching all audio devices when any PCI capture card exists.
- **Bug 12 (Backslash in channel name breaks PipeWire filter-chain config)**:
  - In `src/audio/filter_chain.rs:600`, escape backslashes (`\\`) and double quotes (`\"`) in channel names before embedding into PipeWire SPA-JSON configuration.
- **Bug 13 (Race on quit in state saver)**:
  - In `src/config.rs:942` (`flush_now`), coordinate `SAVER.write_lock` across checking and taking `SAVER.pending` so in-flight saves are never skipped.
- **Bug 14 (Redundant pw-dump calls on window open)**:
  - In `src/main.rs:249`, scan streams and browser tabs once, check for changes, and push the pre-scanned data to the UI using a helper rather than re-scanning twice.

---

## Phase 3: Tab 1 Elgato Wave Link Routing Matrix UI

Reference the working interactive prototype in [`wave_matrix_example.html`](file:///home/yukia/Documents/StudioMic-Package/wave_matrix_example.html).

### 3.1 Tab 1 Architecture: Matrix by Default (Cards View Removed)
- **Cards Section Removed**: The legacy card-based layout in Tab 1 is completely removed. The 2D Routing Matrix table is the primary, default view when Studio Mic opens.
- **Top Bar Button Size Adjuster**: A dedicated button / toggle on the top header (`[Button Size: Small | Medium | Large]`) allows users to scale button padding, font-size, icon dimensions, and fader heights dynamically across the interface.
- **Clean Slate (Empty by Default except Microphone)**:
  - On launch or clean state, only the primary **Broadcast Mic** (`AM8 / Mic 1`) is loaded in the matrix.
  - No dummy/pre-created channels are instantiated, preventing interface clutter and saving PipeWire node overhead.
  - An empty-state card prompts the user: *"No extra virtual channels created. Choose only what you need."* with a prominent **`+ Add Channel`** button.
- **On-Demand Port Creation Modal**:
  - Clicking `+ Add Channel` opens an interactive modal with 1-click presets:
    1. `💬 Discord / Voice Chat` (PipeWire sink: `StudioMic_Ch_voice`)
    2. `🎮 Games` (PipeWire sink: `StudioMic_Ch_game`)
    3. `🌐 Web Browser` (PipeWire sink: `StudioMic_Ch_browser`)
    4. `🎵 Music` (PipeWire sink: `StudioMic_Ch_music`)
    5. `📱 Apps & Media` (PipeWire sink: `StudioMic_Ch_apps`)
    6. `🎙️ Second Mic / Line In` (Hardware XLR/USB input node)
    7. `⚙️ Custom Virtual Port` (User-defined custom sink name)
  - Selected ports are instantiated on demand in PipeWire and added as rows in the matrix table.
  - Each created channel features a trash/delete icon on its row to remove the virtual port when no longer needed.

### 3.2 2D Matrix Grid Structure & Zero-Restriction Routing
Implement `renderMixerMatrix()` in the v1 runtime script of `mic-studio-mockup.html`:
- **Columns (Destinations)**:
  1. `Audio Channels (Inputs)` (Source name, DSP badge, sink node name, trash button)
  2. `🎧 My Mix` (`Monitor_Mix` / Headphones) &rarr; binds to `ch.mix.monitor_gain_db` and `ch.mix.monitor_muted`
  3. `📡 Stream` (`Stream_Mix` / OBS Track 1) &rarr; binds to `ch.mix.stream_gain_db` and `ch.mix.stream_muted`
  4. `🎬 Recording · VOD` (`VOD_Mix` / OBS Track 2 DMCA) &rarr; binds to `ch.mix.vod`
  5. `👥 Voice Chat` (`Voice_Out` / Discord) &rarr; **100% Unrestricted**: any input channel (including Web Browser, Games, Music, 2nd Mic) can route into Discord!
  6. **`+ Add Mix` Header Button**: Positioned directly beside `Voice Chat`, opens `openAddMixModal()` to create new destination output mixes/buses.

- **Cross-Point Cell Interaction Design**:
  - **Active (Routed) State**:
    - **Mute Button**: Click toggles mute. In `Recording · VOD` (or when muted), displays a **glowing red badge** with `volume_off` and red-tinted cell background (signature Elgato DMCA protection styling).
    - **Fader Slider**: Horizontal dual-track slider with glowing real-time audio meter integrated above the slider track.
    - **Small `x` Remove Button**: A visible small `x` in the upper corner of each active cell to disconnect that channel from that mix with one click (right-click also supported).
  - **Inactive (Unrouted) State**:
    - Cell is **clean and blank with a centered `+` button**.
    - **Left-Click to Activate**: Clicking the `+` activates that input for that mix, revealing the active fader, mute button, and small `x`.

- **Comprehensive Monitoring Controls**:
  - **Master Monitor Controls (Header Bar & My Mix Header)**:
    - Master Monitor Mute button: One-click master headphone mute toggle (`headphones` / `headset_off`), syncing with `s.monitor_muted`.
    - Master Monitor Gain fader: Controls master headphone volume (`s.monitor_gain_db`), with 0.0 dB snap and dB readout.
    - Output Device Selector: Dropdown to choose the physical DAC/headphones for `My Mix`.
    - Real-time 2-way sync with OS multimedia keys via `pactl subscribe`.
  - **Per-Channel Monitor Controls**:
    - Every active channel under `My Mix` features its own channel monitor fader (`ch.mix.monitor_gain_db`) and mute button (`ch.mix.monitor_muted`).

- **Total Routing Freedom (Web Browser to Discord, etc.)**:
  - In `src/audio/daemon.rs:desired_links`: When `ch.mix.voice_routed` is true for ANY channel (whether Web Browser, Music, Games, or 2nd Mic), establish PipeWire links from `fc::channel_out(&ch.id)` into `fc::bus_sum("Voice_Out")`.
  - Zero restrictions on which audio sources can be sent to Discord.

- **Per-Mix Independent Hardware Output Device Routing**:
  - Allow each mix (built-in or custom) to target a specific hardware audio output device (e.g., Headphones for games/voice on Mix 1, Desktop Speakers for music on Mix 2) rather than only sharing the default device.
  - In `src/config.rs`: Persist `output_device: Option<String>` per custom mix, and `mix_output_devices: BTreeMap<String, Option<String>>`.
  - In `src/audio/daemon.rs:desired_links`: If a mix has an assigned physical `output_device`, establish PipeWire stereo pairs directly between that mix bus's output ports and the target device's playback ports.
  - In `mic-studio-mockup.html`: Display an Output Device selector dropdown in each mix column header, populated with all detected output sinks.

- **Small Adjustable Floating HUD Widget & De-duplicated Top Bar**:
  - **Eliminate Duplications**: Consolidate the redundant Quick Access Bar and Active Broadcast Microphone Hero Card (currently taking ~260px) into a single unified 68px studio strip in full app mode.
    - Remove duplicate `Comp` status pill (merge into a single DSP badge with live GR readout).
    - Remove duplicate `Hear Myself` button (merge into the dedicated Headphone Monitor strip).
    - Remove duplicate textual `Peak: -inf dB` badge (merge into the Wave Link dual-track meter).
  - **Wave Link-Style Dual-Track Fader**:
    - Left: 36x36px tactile Mute button (glowing red when muted).
    - Top track: 6px live LED VU meter with gradient (green -> amber -> red clip).
    - Bottom track: Precision hardware/digital gain slider with circular knob.
  - **Mini HUD Floating Widget Mode**:
    - Compact floating window preset (~420px–560px wide, 180px tall) containing:
      1. Primary Mic strip (Tactile Mute, AM8 name, dB display, top VU meter, bottom Gain slider).
      2. Headphone Monitor strip (Tactile Mute, Hear Myself toggle badge, top VU meter, bottom Volume slider).
      3. Non-duplicated Quick Mute row (`[MIC] [GAME] [VOICE] [MUSIC] [CAPTURE]`).
      4. DSP status badges (`[CLIPGUARD]`, `[RNNOISE]`, `[COMP]`).
    - Tao window management in `src/main.rs`: IPC message `toggle_widget_mode` shrinks Tao window to `480x185` using `window.set_inner_size()`, caches prior window dimensions, and supports `window.set_always_on_top()`.
    - Persist in `src/config.rs`: `widget_mode: bool`, `widget_width: f64`, `widget_height: f64`, `always_on_top: bool`.
  - **Dynamic Peak & Clip Detection on Status / Mute Buttons**:
    - The status dots/buttons (`[• MIC]`, `[• GAME]`, etc.) and the tactile mute button ring dynamically reflect signal levels:
      - Normal ($< -6.0\text{ dBFS}$): Solid green (`#10b981`).
      - Loud / Hot ($-6.0\text{ dBFS}$ to $-1.0\text{ dBFS}$): Amber warning (`#f59e0b`).
      - Peaking / Clip ($\ge -1.0\text{ dBFS}$): Flashing crimson red (`#ef4444 animate-ping`), label changes to `PEAK!` with an 800ms peak-hold timer, and triggers the Clipguard auto-pad badge (`PAD -3dB`).
      - Muted: Solid red (`#ef4444`) with slash icon and `MUTED` text.

---

## Phase 4: Verification & Commands to Run

```bash
# 1. Verify build without pipewire or slint crates
cargo check --workspace
cargo build --release

# 2. Run existing unit tests
cargo test --workspace

# 3. Test latency and audio integrity probes
./target/release/latency-probe
./target/release/gap-probe live --inject StudioMic_Ch_music --capture StudioMic_Stream_Mix

# 4. Packaging validation
# Verify setup-deps staging does not touch home directory:
./setup-deps.sh --no-packages --stage /tmp/pkg-test -y
ls -la /tmp/pkg-test
```
