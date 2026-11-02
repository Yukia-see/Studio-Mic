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

### 3.1 View Switcher in Tab 1
In `mic-studio-mockup.html`, expand the view toggle on Tab 1 to include:
- `[ ≡ Matrix ]` (Wave Link 2D Routing Grid)
- `[ 🎛️ Cards ]` (Current Normal view mode with waveforms)
- `[ ⚡ Compact ]` (Simple minimalist sliders)

### 3.2 2D Matrix Grid Structure
Implement `renderMixerMatrix()` in the v1 runtime script of `mic-studio-mockup.html`:
- **Columns (Destinations)**:
  1. `Audio Channels` (Source name, DSP badge, app chips)
  2. `🎧 My Mix` (`Monitor_Mix` / Headphones) &rarr; binds to `ch.mix.monitor_gain_db` and `ch.mix.monitor_muted`
  3. `📡 Stream` (`Stream_Mix` / OBS Track 1) &rarr; binds to `ch.mix.stream_gain_db` and `ch.mix.stream_muted`
  4. `🎬 Recording · VOD` (`VOD_Mix` / OBS Track 2 DMCA) &rarr; binds to `ch.mix.vod`
  5. `👥 Voice Chat` (`Voice_Out` / Discord) &rarr; microphone feeds exclusively; background channels unrouted
  6. **`+ ADD` Header Button**: Positioned directly beside `Voice Chat`, allows adding custom destination mix columns or virtual channels.

- **Cross-Point Cell Interaction Design**:
  - **Active (Routed) State**:
    - **Mute Button**: Click toggles mute. In `Recording · VOD` (or when muted), displays a **glowing red badge** with `volume_off` and red-tinted cell background (signature Elgato DMCA protection styling).
    - **Fader Slider**: Horizontal slider with glowing real-time audio meter integrated into the slider track.
    - **Right-Click to Remove**: Right-clicking an active cell deactivates that routing connection, restoring it to the blank `+` state.
  - **Inactive (Unrouted) State**:
    - Cell is **clean and blank with a centered `+` button**.
    - **Left-Click to Activate**: Clicking the `+` activates that input for that mix, revealing the active fader and mute button.

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
