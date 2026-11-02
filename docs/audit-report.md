# Audit: spec requirements vs. the code before this upgrade

Source spec: `studio_mic_master_upgrade_spec_verification_checklist.md`. Plan: the Antigravity
implementation plan (6 phases, 28 checks). "Before" = commit `b7a2424` plus the uncommitted
working tree.

## What existed and how it was changed

| Area | Before | Status | Change |
|---|---|---|---|
| Engine lifecycle | One `pipewire -c studiomic-graph.conf` child. **Every** EQ/DSP/channel change rewrote the file and killed + restarted it (`reload_graph`), cutting all audio for ~400 ms | PRESENT, NEEDS UPGRADE (interrupts audio) | Replaced. Stages are separate processes; sliders use `pw-cli set-param`; structural changes are make-before-break with a crossfade (`src/audio/daemon.rs`) |
| Buses | `studiomic_processed_mic` + `studiomic_<ch>` sources lived inside the restarting process | NEEDS UPGRADE | 4 lingering Tier-1 buses + legacy alias, reused if present, never torn down |
| Noise gate | LSP gate, single threshold | PRESENT, upgraded | Hysteresis (close = open - 7 dB, per the signed-off crackle-fix plan; the spec said 5 dB), 3 ms look-ahead, soft knee, 250 ms hold, 150 Hz - 3.5 kHz sidechain key filter, Auto-Set (3 s keyboard test, +4 dB margin) |
| De-esser | Static `bq_peaking` cut | PRESENT, upgraded | Dynamic: LSP compressor with a sidechain high-pass at the de-ess frequency, threshold + amount (static cut kept as no-LSP fallback) |
| Compressor | LSP, threshold/ratio/makeup | PRESENT, upgraded | + attack / release, live bypass |
| Limiter | LSP limiter, -2 dB, toggle | PRESENT, upgraded | Becomes Clipguard: -0.5 dB, 5 ms release, plus hardware auto-pad (max -6 dB, restored on off) |
| Denoiser | DeepFilterNet only, on/off | PRESENT, upgraded | Engine selector (DeepFilterNet / RNNoise / Speex / Off), one loaded at a time, hot-swap with crossfade, failed swaps keep the old chain |
| EQ | 10 bands, every edit restarted the engine | NEEDS UPGRADE | Live `Freq/Q/Gain`; only a filter-TYPE change swaps the post chain |
| Virtual channel presets | Game/Voice/Music/Capture with `pw-link` routing (apps played to BOTH the old sink and the channel) | PRESENT, upgraded | WirePlumber `target.object` moves (no double audio), smart rules per preset, persisted app lists, feedback guard |
| Mixer | One "fader" (source volume) + mute/solo per strip; monitor via ad-hoc `pw-link` to the default sink | NEEDS UPGRADE | Stream Gain + Monitor Gain per channel, Stream/Monitor mute, VOD toggle, Auto-Duck, Solo (monitor only), per-route delay |
| Theme engine | Fixed dark tokens | MISSING | 4 themes incl. OLED Black, live switch, persisted |
| Waveform view | 60 FPS-ish canvas strips | PRESENT, upgraded | Normal mode (30 FPS waveforms) + Simple mode (stacked rows, no canvases), idle frames skipped |
| State | `~/.config/studiomic/config.json`, written synchronously on every change, non-atomic | NEEDS UPGRADE | `~/.config/studio-mic/state.json` schema v1, migrated with `.bak`, 500 ms debounced atomic writes, flush on quit/SIGINT/SIGTERM, named profiles |
| Missing input | Fell back to the system default mic | NEEDS UPGRADE | Shows "Missing", never switches ids, Refresh rescans |
| Single instance / CLI | PID file + polled `.cmd` file | NEEDS UPGRADE | Unix socket (0600) with the full CLI, headless auto-start |
| Tray | Static icon, menu | PRESENT, upgraded | Live colour (green/red/amber), middle-click mute, scroll = monitor gain |
| Installer | Copies binary, no prompts | NEEDS UPGRADE | Two-stage (setup-deps.sh), sudo-safe, StreamController linking, native packages |
| Tab order | EQ, Mixer, Clean, Calib | NEEDS UPGRADE | Mixer, EQ, Manual Fine-Tune, Test & Optimize |
| Slint UI | Compiled by build.rs, never used | LEFT AS IS | Not part of the spec; still compiled |

## Deliberate interpretations

* **Voice Out firewall vs. plan item 4.** The plan's check 4 says capture-card loopbacks link into
  `StudioMic_Voice_Out`; the spec's hard firewall forbids it. The spec wins: only the mic chain
  reaches Voice Out (and `relink_all` actively removes anything else).
* **"Voice_In" in the plan** is the same node the spec calls `StudioMic_Voice_Out`
  ("Studio Mic (Microphone Only)"). One name is used: `StudioMic_Voice_Out`.
* **Mic mute** silences Voice Out, Stream and VOD; the mic's headphone branch has its own mute.
* **Channel rename** recreates that one channel's sink (its node description is fixed at
  creation). Other channels, the mic and the buses are untouched.
* **Wave Link matrix grid / Option B sub-mixers** from the plan are not built as a separate grid:
  the spec's Option A stacked rows already give every channel × bus cell (Stream, VOD-follows-
  Stream, Monitor, plus mic-only Voice) with gain, mute and delay. Per-app VCA sub-mixers are not
  implemented.
* **Speex** is selectable but needs a LADSPA wrapper that no distro ships (see
  `verified-apis.md`).
