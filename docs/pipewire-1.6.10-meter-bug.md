# Why the meters broke on PipeWire 1.6.10

Date: 2026-11-02. Fixed in Studio Mic build 4 (`studio-mic-0.2.0-4`).

## Summary

On PipeWire 1.6.10 and newer, every Studio Mic meter read garbage near full scale. The meters showed red "clipping" while OBS showed a normal level, and Clipguard auto-pad kept lowering the mic even at 1% input volume. The audio itself was never affected.

The cause: Studio Mic read `pw-record`'s output as raw little-endian samples, but without `--raw`, `pw-record` writing to a pipe produces an **AU audio file**. PipeWire 1.6.10 changed that file from little-endian to **big-endian** on x86, so every sample decoded as nonsense. The fix is `pw-record --raw`, which writes plain samples on every version.

## What Studio Mic did

Each meter is a `pw-record` process whose stdout is a pipe that Studio Mic reads (`src/audio/daemon.rs`, `spawn_meter`):

```
pw-record --rate 48000 --channels 1 --format f32 -P '{ node.name = StudioMic_Meter_mic ... }' -
```

The reader treats the stream as headerless little-endian `f32` samples and takes the peak of each 25 ms block.

## What pw-record actually writes to a pipe

In `pw-cat.c` (`pw-record` is `pw-cat`), output to `-` gets the AU container unless `--raw` is given:

```c
if (spa_streq(filename, "-"))
        extension = container ? container : "au";
```

So the pipe carries a 24-byte AU header followed by samples in whatever byte order the AU writer picks. Only `--raw` (`setup_raw` → `fwrite` to stdout) skips the file writer and emits plain, CPU-native samples.

## What changed in 1.6.10

PipeWire commit **`5aca348d`** (2026-10-19, "pw-cat: always let libsndfile pick the file byte order"), first released in 1.6.10, changed `format_from_filename`:

```diff
-	switch (format & SF_FORMAT_TYPEMASK) {
-	case SF_FORMAT_OGG:
-	case SF_FORMAT_FLAC:
-	case SF_FORMAT_MPEG:
-	case SF_FORMAT_AIFF:
-	case SF_FORMAT_WAVEX:
-		info->format |= SF_ENDIAN_FILE;
-		break;
-	default:
-		info->format |= SF_ENDIAN_CPU;
-		break;
-	}
-	info->format |= format;
+	/* Let libsndfile pick the container's native byte order; many
+	 * containers (W64, RF64, WAVEX, FLAC, ...) reject SF_ENDIAN_CPU. */
+	info->format |= SF_ENDIAN_FILE | format;
```

- **Up to 1.6.9:** AU wasn't in the list, so it got `SF_ENDIAN_CPU`, which is little-endian on x86. That's the non-standard AU variant, magic `dns.` (`646e732e`). Studio Mic's little-endian reader happened to decode it correctly; the 24-byte header only produced one bogus block at start-up.
- **1.6.10 and later:** AU gets `SF_ENDIAN_FILE`, its standard big-endian byte order, magic `.snd` (`2e736e64`). Decoded as little-endian, a quiet −20 dB test tone read as 3.3 × 10³⁸.

This isn't a PipeWire bug: a big-endian AU is the standard form. Studio Mic relied on an accident of the old behaviour.

`libsndfile` is 1.2.2 on every distro tested, so it's not the cause.

## Evidence

Same test on each distro, in containers running that distro's PipeWire and WirePlumber. A 0.1-amplitude (−20 dBFS) sine is played into `pw-record ... -` through a pipe, and the output is decoded the way Studio Mic does (little-endian `f32` from byte 0):

| Distro | PipeWire | Header without `--raw` | Without `--raw` (builds 1–3) | With `--raw` (build 4) |
|---|---|---|---|---|
| This dev PC (Arch) | 1.6.9 | `dns.` little-endian | 0.1 correct | 0.1 correct |
| Manjaro | 1.6.9 | `dns.` little-endian | 0.1 correct | 0.1 correct |
| Arch (current) | 1.6.10 | `.snd` big-endian | 3.3e38 **wrong** | 0.1 correct |
| CachyOS | 1.6.10 | `.snd` big-endian | 3.3e38 **wrong** | 0.1 correct |
| Fedora 44 | 1.6.11 | `.snd` big-endian | 3.3e38 **wrong** | 0.1 correct |

Reading the 1.6.11 output as big-endian gives 0.1, which confirms it's purely a byte-order problem.

## Why it looked like a "visual bug"

Only the meters were wrong; the audio path never touched `pw-record`:

- **Red meters:** every strip, and the mic's hardware input meter, read near or above 0 dBFS all the time.
- **OBS fine:** OBS records the real buses, which were at normal levels.
- **Auto-pad at 1% volume:** Clipguard auto-pad steps the mic's hardware volume down when the hardware meter reads ≥ −0.1 dB. The reading was garbage, so it stepped all the way to −6 dB regardless of the real level (`Clipguard auto-pad: hardware -6 dB, digital makeup +6 dB`).

It never showed on the dev PC because that PC is still on 1.6.9. Fresh installs of Arch, CachyOS and Fedora ship 1.6.10 or newer, so they hit it immediately. This was very likely the original "peaking on the other PC" report.

## The fix (build 4)

- Meters run `pw-record --raw ...` (`meter_format_args` in `src/audio/daemon.rs`): headerless, CPU-native samples on every PipeWire version.
- The regression test `meters_read_raw_native_samples` fails if `--raw` is ever removed.
- `gap-probe` and `latency-probe` were checked and are unaffected: they write WAV files and read them with a WAV parser.

## Related meter fixes (builds 2–3)

These were separate issues found while chasing the same symptom:

- **Build 1:** meters moved from 8 kHz / 16-bit to 48 kHz float. The old format clamped and resampled, so audio limited to −0.5 dBFS read 0.0 dB.
- **Build 2:** red now means real clipping (a flat-topped waveform), not just a high level.
- **Build 3:** the meter's colour gradient no longer ends in red.

## Takeaways

- Never parse a CLI tool's default output format implicitly. Ask for the exact format (`--raw`), or parse the header.
- Test audio I/O on the newest PipeWire as well as the dev machine's. Fedora's and Arch's current versions differ from the dev PC's.
- When this PC updates to PipeWire 1.6.10, any pre-build-4 Studio Mic install here breaks the same way.
