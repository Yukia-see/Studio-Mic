# Studio Mic bug report

Review date: 2026-11-02. No code was changed as part of this review. A few findings were checked against PipeWire on the dev machine; the rest come from reading the code. One suspicion was ruled out: fader and ducking changes don't flood the Monitor sync loop with events.

## Build problems (fix before compiling for Arch and Fedora)

1. **The Fedora build will fail.** `Cargo.toml` includes the `pipewire` crate, which needs PipeWire's development headers (`libpipewire-0.3`). `packaging/fedora/studio-mic.spec:10` doesn't list them, and without them the crate stops with "Cannot find libpipewire" (confirmed). Nothing in `src/` uses this crate, so removing it fixes Fedora and also drops the need for `clang`.
2. **`slint` and `slint-build` are unused** but still compile a whole second UI toolkit (`ui/studiomic.slint`, called from `build.rs`). Removing them cuts build time and binary size a lot.
3. **The Arch package can ship an old binary.** `packaging/arch/PKGBUILD:31` only runs `cargo build` if no binary exists yet, so stale code gets packaged. This is probably why `dist/` was out of date.
4. **Building the Arch package changes your home folder.** `PKGBUILD:26` runs `setup-deps.sh`, which also installs plugins into `~/.local/lib/ladspa` and writes `~/.config/environment.d`. Arch's default `debug` option also builds a `-debug` package, and `install.sh` installs that too (`pacman -U studio-mic-*.pkg.tar.zst`).
5. **The minimum Rust version isn't declared.** `File::try_lock` (`src/ipc.rs:46`) needs Rust 1.89 or newer. Adding `rust-version = "1.89"` to `Cargo.toml` turns an older Fedora toolchain's confusing failure into a clear error.
6. **Smaller items:**
   - `tokio = "full"` is only used to catch Ctrl+C and quit signals; `["rt", "signal"]` is enough.
   - The version number is typed separately in `Cargo.toml`, the PKGBUILD and the spec.
   - Offline builds (mock or COPR) would need `setup-deps.sh --vendor` and the plugin downloads done ahead of time.

## Bugs, most serious first

### Medium

1. **Discord can lose your mic for up to 2 s after a denoiser or EQ filter-type switch.** The maintenance pass every 2 s can run in the middle of that switch. Its Voice_Out guard only knows the old chain, so it cuts the new chain's Voice_Out links (`src/audio/daemon.rs:662`, `:1710`).
2. **The same kind of switch can double the mic or click when auto-duck is on.** The ducking loop sets the old chain back to full volume while it's fading out (`src/audio/daemon.rs:1132`). Both bugs share one fix: make the switch hold a lock that relinking and mixer updates wait for.
3. **Unassigning an app doesn't move it back.** The app keeps playing into its old channel until it restarts, because the "routed by Studio Mic" record is cleared, so the stream is never handed back (`src/routing/wireplumber_hook.rs:315`, `:287`). The same happens when you remove an app from a channel's list.
4. **Silent channels show a meter bar when their fader is above 0 dB.** For example, +12 dB shows about 20% on silence (`src/audio/daemon.rs:1328`). This came from the meter fix in the previous session.
5. **Turning Clipguard off with the Tab 3 checkbox doesn't restore the mic's hardware level.** Only the CLI and tray do that (`src/app.rs:483`). The UI also always sends a −0.5 dB ceiling, overwriting any other setting (`mic-studio-mockup.html:6108`).
6. **Turning on "Launch on Login" always starts minimized**, ignoring your setting (`src/app.rs:1328`, which calls `src/autostart.rs:59`).
7. **A "NaN" calibration trim can wipe your settings.** It gets saved as `null`, the next launch can't read `state.json`, and settings reset to defaults (a `.corrupt` copy is kept) (`src/app.rs:1195`).

### Low

8. **If the saved mic is unplugged, its hardware meter likely records the default mic instead**, so the mic card can flash red from a different device (`src/audio/daemon.rs:1229`).
9. **On PCs with a PCI capture card, every mic is probably listed as that capture card** (`src/audio/device_discovery.rs:183`).
10. **Autostart has three smaller problems:**
    - If `HOME` is unset, the path falls back to `/home/yukia` (`src/autostart.rs:8`).
    - The program path isn't quoted, so a path with spaces breaks it.
    - After a package upgrade, the recorded path can end in " (deleted)" (`src/autostart.rs:27`).
11. **Loading a profile or changing the mic doesn't restore the auto-pad hardware level** for the old mic (`src/app.rs:759`).
12. **A channel name containing a backslash breaks that channel's config**, so the channel won't start (`src/audio/filter_chain.rs:600`).
13. **On quit, the last change can occasionally go unsaved** because of a small timing race in the save code (`src/config.rs:942`).
14. **When the app window opens, it calls `pw-dump` four times for each change to the app list** (`src/main.rs:249`). This is only a performance cost.

## Suggested order

1. Build problems 1–3: remove the unused crates, always rebuild in the PKGBUILD, and add the missing Fedora build requirement (or drop the `pipewire` crate so it isn't needed).
2. Bugs 1–3: the chain-switch races and app unassigning.
3. Everything else.

## Status (fixed 2026-11-02, following Antigravity's `bugfix_plan.md`)

All 6 build problems and 14 bugs above are fixed. Workspace builds with no warnings; 29 tests pass.

Deviations from the plan:
- **Locking:** a new `swap_guard` lock (held for the whole chain swap, taken by `relink_all` and `apply_mixer`) instead of reusing `ops`. Several callers of those two already hold `ops`, and Rust's mutex isn't re-entrant, so the plan as written would have deadlocked the engine.
- **tokio:** needs `["rt", "signal", "macros"]` (`select!` is a macro). `ksni` also needed its `blocking` feature, which the removed crates had been turning on indirectly.
- **Bug 11:** changing the mic already restored auto-pad; only profile loading needed the fix.
- **Bug 3:** besides keeping the routing record, Studio Mic now reads `target.object` from WirePlumber's metadata once at startup, so apps routed before a restart are released too.
- **Bug 8:** meters on a device now use `node.dont-fallback`/`node.dont-reconnect`. Verified on this PC: without them, a meter aimed at a missing mic recorded the default mic.

Found in the recheck and fixed:
15. **Dragging an EQ band past a band of another type rebuilt the whole mic chain** (Mic 2 restarted its process). The UI re-sorts bands by frequency, so slot types shifted. Bands are now fitted onto the running slots (`fit_to_slots`); only a real change in the set of filter types rebuilds.
16. **Gate auto-set with no mic signal set the gate to −70 dB** (always open) from a fake −80 dB reading. It now reports "no signal" and leaves the gate alone.

Still to verify by hand (needs the new build running, which takes over the audio session): denoiser and EQ swaps while talking with Discord/auto-duck, unassigning an app, Clipguard restore, and the autostart file.

## Third pass: lag and remaining bugs (2026-11-02)

Lag fixes (measured here: `pw-dump` ≈ 50–65 ms, `wpctl inspect` ≈ 42 ms, `pw-metadata` ≈ 9 ms):
- **Webview messages no longer run on the GTK thread.** Device refresh (~300 ms), the live app list, preview start and more used to freeze the window while they queried PipeWire. They now run in order on a `ui-messages` thread.
- **Tray and Stream Deck refreshes are coalesced** to at most one per 120 ms. Every fader or slider message used to rebuild the tray menu and rewrite the deck state file on the UI thread.
- **Default device lookup uses `pw-metadata`** instead of `wpctl inspect` (runs on every 2 s maintenance pass).
- **Device lists use the shared node cache** instead of a full `pw-dump` (1 MB of JSON), and the capture-card scan reuses the mic list instead of scanning again.
- **Startup scans run in parallel**, and the player/browser scans read the `.desktop` folders once instead of once per app name.
- **UI:**
  - the 20 Hz peak handler skips Tab 3 widgets while that tab is hidden, and only writes text and classes that changed;
  - the waveform uses one gradient per canvas instead of one per bar, and skips idle canvases that are already drawn;
  - each peak push is parsed once instead of twice.

Bugs found and fixed in this pass:
17. **Gate auto-set with no mic signal** (also listed as 16 above): now reports "no signal" in the UI and restores the button.
18. **Crash recovery kept the old EQ slot layout.** After a mic chain or Mic 2 crash, the rebuilt chain didn't update its recorded slot layout. That became reachable once re-sorted bands stopped forcing a rebuild (fixed together with 15).

## The Fedora meter bug (fixed in build 4)

19. **Every meter read garbage on Fedora.** Meters read `pw-record ... -` through a pipe without `--raw`, so pw-record wrote an AU (.snd) audio file. Fedora's PipeWire writes standard big-endian AU, which was decoded as little-endian floats: values near full scale on every meter. Result: red meters while OBS was fine, and Clipguard auto-pad stepping the mic down to −6 dB even at 1% volume. Arch writes little-endian AU, which hid the bug on the dev PC. This was very likely the original "peaking" bug from the start. Fix: `--raw` (headerless, native samples), verified in a Fedora 44 container (PipeWire 1.6.11).
20. **The meter colour gradient turned red near full scale** regardless of clipping. It now stops at amber; red is only for real clipping (build 3).
