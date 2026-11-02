# Verified APIs (PipeWire 1.6.9, Arch/Omarchy, 2026-11-02)

Everything below was checked on a live session before the engine was written. Port names were read
straight from the plugin binaries (LADSPA descriptors via `ctypes`; `strings` for the RNNoise
download, which was not executed).

## LADSPA control ports

| Plugin | Label | Audio ports | Controls used by Studio Mic |
|---|---|---|---|
| `libdeep_filter_ladspa.so` (0.5.6) | `deep_filter_mono` | `Audio In` / `Audio Out` | `Attenuation Limit (dB)` 0..100, `Min processing threshold (dB)` -15..35, `Max ERB processing threshold (dB)` -15..35 |
| `librnnoise_ladspa.so` (1.10) | `noise_suppressor_mono` | `Input` / `Output` | `VAD Threshold (%)`, `VAD Grace Period (ms)`, `Retroactive VAD Grace (ms)` |
| `lsp-plugins-ladspa.so` | `http://lsp-plug.in/plugins/ladspa/gate_mono` | `Input` / `Output` | `Bypass`, `Hysteresis`, `Curve threshold (G)`, `Hysteresis threshold (G)`, `High-pass filter mode/frequency (Hz)`, `Low-pass filter mode/frequency (Hz)` (sidechain = vocal key filter), `Attack (ms)`, `Hold time (ms)`, `Release (ms)`, `Reduction (G)` |
| same | `.../compressor_mono` | `Input` / `Output` | `Bypass`, `Attack threshold (G)` 0.001..1, `Ratio` 1..100, `Attack time (ms)`, `Release time (ms)`, `Makeup gain (G)`, sidechain `High-pass filter mode/frequency (Hz)` (used for the de-esser) |
| same | `.../limiter_mono` | `Input` / `Output` | `Bypass`, `Threshold (G)`, `Lookahead (ms)`, `Attack time (ms)`, `Release time (ms)` (Clipguard, -0.5 dB) |
| same | `.../limiter_stereo` | `Input L/R` / `Output L/R` | `Threshold (G)` (bus safety limiters: -1 dB Stream/VOD/Voice, -3 dB Monitor) |
| PipeWire builtin | `bq_*`, `mixer`, `copy`, `delay` | `In`/`Out`, mixer `In 1..8` | `Freq` `Q` `Gain`; mixer `Gain 1..8`; delay `Delay (s)` with `config = { "max-delay" = 1.0 }` |

Every LSP plugin has a `Bypass` port, so stage on/off is a live parameter, never a rebuild.

**Speex:** there is no packaged Speex LADSPA wrapper on Arch, Fedora or Debian (`speexdsp` is a C
library only). Studio Mic shows Speex as "not installed" unless a `libspeex_ladspa.so` or
`speex_preprocess.so` with a `Noise Suppression (dB)` port is on the LADSPA path.

## Live parameter changes

* `pw-cli set-param <id> Props '{ params = [ "eq1:Gain" 6.0 "gate:Bypass" 0.0 ] }'` on a running
  filter-chain node changes values in place in about 26 ms, with no node re-creation. Values read
  back through `pw-dump <id>` → `info.params.Props[1].params`.
* `pw-cli` reading commands from a pipe works when the pipe stays open (it does **not** exit on
  EOF). Studio Mic keeps one `pw-cli` child and writes `set-param` lines to it
  (`src/audio/pw.rs`, `ParamWriter`), so a slider move costs a pipe write instead of a process spawn.

## Nodes, links, routing

* `pw-cli create-node adapter '{ factory.name=support.null-audio-sink ... object.linger=true }'`
  creates a node that **survives the creating process**. `Audio/Source/Virtual` null nodes expose
  `input_FL/FR` + `capture_FL/FR`; `Audio/Sink` ones expose `playback_FL/FR` + `monitor_FL/FR`.
* Filter-chain stream nodes expose `input_<POS>` / `output_<POS>` ports. A graph output listed with
  `audio.position = [ AUX0 .. AUXn ]` gives one port per route, which is how per-bus gains work.
* Recording one tap channel: start `pw-record` with `node.autoconnect = false` and `pw-link` the
  single `output_AUX8` port to its `input_MONO`. (Targeting the multi-channel node with
  `audio.position = [ AUX2 ]` did not pick the right channel.)
* Moving an app stream like pavucontrol does:
  `pw-metadata -n default <stream-id> target.object <node.name>` (plain string, **no** type
  argument). The `Spa:String:JSON` form silently fell back to the default sink. The `object.serial`
  number also works. The move is clean: no duplicate links to the old sink.

## Volume scale (verification item 11)

`wpctl set-volume <id> 0.5` stores `channelVolumes = 0.125`: **wpctl uses a cubic scale**
(`linear = wpctl³`). Studio Mic therefore never uses `wpctl` for gains. Route gains are filter
`mixer` controls (linear), and the Master Monitor Gain is applied as
`pw-cli set-param <id> Props { channelVolumes = [ lin lin ] mute = bool }` (linear), so UI `dB` and
`%` (`% = 10^(dB/20) × 100`) never drift.

## Session facts on this machine

* The legacy node `studiomic_processed_mic` is the configured **default source**
  (`default.configured.audio.source`). Studio Mic keeps a lingering node with that exact name fed
  from the Voice Out limiter, so Discord and older OBS scenes keep working after the upgrade.
* LADSPA plugins were installed by hand in `~/.ladspa` (no pacman packages). Studio Mic searches
  `$LADSPA_PATH`, `~/.local/lib/ladspa`, `~/.ladspa`, `/usr/lib/ladspa`, `/usr/lib64/ladspa` and
  `/usr/local/lib/ladspa`.
