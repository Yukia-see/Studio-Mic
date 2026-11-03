# Performance and latency (measured 2026-11-02)

Machine: Arch/Omarchy, PipeWire 1.6.9, graph quantum 256 @ 48 kHz, Saramonic MV Pod mic,
5 channels (Game, Voice, Music, Capture Card, Mixer), DeepFilterNet active, window hidden (tray).
Measured with `top -b -d 1 -n 11` per process, single-core %.

## CPU

| Process | CPU |
|---|---|
| Mic chain (`sm-mic-g1.conf`: trim, DeepFilterNet, gate, filters, de-esser, EQ, compressor, Clipguard) | 7.4 % |
| Studio Mic app (UI hidden: maintenance, routing, ducking, meters) | 2.6 – 3.6 % |
| Bus master (4 safety limiters) | 3.5 % |
| Each channel strip (idle) | ~1.3 % |
| Meter taps (`pw-record`, mic + raw) | ~0.7 % each |
| **Total** | **~22 %** |

History while building this:

* Separate pre / denoiser / post processes: DeepFilterNet process alone ~20 %, total ~40 %.
  Merging them into one mic-chain stage brought the total to ~22 %.
* App process 5.0 % → 2.6 % after caching graph queries (one `pw-dump` / `pw-link` snapshot per
  2 s maintenance pass instead of ~15 process spawns).
* The UI meters run at 30 FPS, skip DOM writes when values do not change, and stop entirely while
  the window is hidden or the Mixer tab is not visible. Simple mode has no waveform canvases.

With RNNoise instead of DeepFilterNet the mic chain drops to a few percent. Idle channel strips are
kept running on purpose (`node.pause-on-idle = false`) so the first sound of an app is never
clipped by a resume.

## Latency (mic → headphones)

`tools/latency-probe` injects an impulse into the mic chain input and records it with one stereo
recorder (reference + Monitor limiter output), so both share a clock.

| Monitor mode | Latency |
|---|---|
| Raw (direct hardware → Monitor bus link, no processing) | 5.3 ms (one 256-sample period) |
| Processed, noise remover off | 28.4 ms |
| Processed, DeepFilterNet | 58.2 ms (DeepFilterNet's own look-ahead is ~30 ms) |

Each filter stage costs about two graph periods, which is why the mic path is a single stage.

## Live-change continuity (gap-probe)

| Test | Result |
|---|---|
| 32 slider / mute / Clipguard / EQ-gain / gate / compressor changes, channel path (Stream Mix) | 0 dropouts, 0 clicks; node ids unchanged; 0 `removed:` events across 442 `changed:` events |
| 4 EQ filter-type changes (make-before-break chain swaps), mic path | 0 dropouts, 0 clicks over 17 s |
| Denoiser hot-swap (off ↔ DeepFilterNet, DeepFilterNet → RNNoise) | buses never removed; swap returns in ~60 ms |

Run the continuity test yourself while moving sliders:

```bash
cargo build --release --workspace
./target/release/gap-probe live --inject StudioMic_Ch_music --capture StudioMic_Stream_Mix --secs 20
# mic path (use a quiet room and turn the denoiser off, it removes test tones):
./target/release/gap-probe live --inject "$(studiomic --status --json --runtime | jq -r .runtime.mic_input_node)" \
    --capture StudioMic_Monitor_Mix_Lim --secs 20
```

## Follow-up fixes (signed-off plans: bug root-cause + keyboard-test crackle)

| Check | Result |
|---|---|
| Keyboard gate test (3 s) with a sentinel on the Monitor path | 0 dropouts, 0 clicks; no meter process spawned; 0 Clipguard auto-pad steps |
| Gate: 3 ms look-ahead, ~3 dB soft knee (zone 0.707), 7 dB hysteresis, attack 8 / hold 250 / release 200 ms, -26 dB floor | live in the chain (read back via `pw-dump`) |
| `--cleanup-pipewire` with a dummy process whose command line contains every trigger string | dummy survives |
| Second launch while running | "Studio Mic is already running", exit 0, one instance |
| Baseline, no changes, 2 × 26 s (OBS running) | 0 dropouts |
| Denoiser swaps off -> DeepFilterNet, 3 runs | 1 clean; 2 runs had ONE 21 ms glitch 12-17 s after DeepFilterNet loaded |

### Residual: DeepFilterNet's first seconds

A freshly loaded DeepFilterNet occasionally takes longer than real time for a frame (it logs
"Underrun detected (RTF ~1.5)"). The mic chain is scheduled synchronously with the rest of the graph,
so that frame delays the whole cycle and every node drops ~21 ms once. Steady-state DeepFilterNet
never did this in 52 s of testing; unloading is fixed (nodes are destroyed server-side before the
process stops).

Tried and rejected:

* `node.async = true` on the mic chain: isolates the hiccup completely, but adds ~24 ms to every
  word (58 -> 83 ms processed latency).
* Warm-up while unlinked: PipeWire does not schedule an unlinked chain, so nothing warms.
* Warm-up on a private null-sink driver: DeepFilterNet ran, but locking the period size fought OBS's
  own period changes and produced MORE xruns.
