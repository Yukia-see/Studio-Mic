//! Studio Mic audio engine.
//!
//! Stages (each its own `pipewire -c <conf>` child, so changing one never touches the others):
//!   * bus master: 4 safety limiters feeding the lingering Tier-1 buses
//!   * mic chain generation N: trim/mono-center ► denoiser ► gate ► filters ► de-esser ► EQ ►
//!     compressor ► Clipguard ► Voice/Stream/VOD/Monitor gains (one stage = lowest latency)
//!   * one strip per channel (raw passthrough, Stream/VOD/Monitor gains + delays), optional Mic 2
//!
//! Live-change rule: sliders, mutes and stage toggles NEVER restart anything; they are applied
//! with `pw-cli set-param` through the persistent writer in `pw.rs`. Structural changes (EQ filter
//! TYPE, denoiser engine) build mic chain N+1 next to N, wait until its links are active, crossfade
//! ~40 ms and retire N. If N+1 never appears (3 s), it is discarded and N keeps running.
//! Channel add/remove/rename only (re)starts that channel's strip.
//!
//! The Tier-1 buses (`StudioMic_Voice_Out`, `_Stream_Mix`, `_VOD_Mix`, `_Monitor_Mix`) and the
//! custom mix buses are lingering nodes owned by the main PipeWire daemon:
//! reused if present, recreated if missing, never torn down by Studio Mic. A maintenance pass
//! every 2 s re-creates missing buses, rebuilds stages that exited or were disconnected (PipeWire
//! restart), restores links and runs app auto-routing.

use crate::audio::filter_chain::{self as fc, Plugins, BUSES, MIC2_IN, MIC2_OUT};

/// The pre-v0.2 output name. No longer created; any copy left by an older version (or by its
/// login drop-in) is removed at startup so no stray "Studio Mic (Legacy Alias)" device remains.
const REMOVED_LEGACY_ALIAS: &str = "studiomic_processed_mic";
use crate::audio::pw::{self, db_to_lin, lin_to_db, PARAMS};
use crate::config::{cache_dir, StudioMicConfig};
use crate::models::microphone::FilterType;
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

pub const PREVIEW_WAV: &str = "/tmp/studio-mic-preview.wav";
const PREVIEW_EXPIRY: Duration = Duration::from_secs(15 * 60);
const RECORD_MAX_SECS: u64 = 10;
const SWAP_TIMEOUT: Duration = Duration::from_secs(3);

/// Things the UI / tray / socket want to hear about.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    Peaks(String),
    Recorder(String),
    Preview(String),
    MonitorMaster,
    Notice(String),
    /// Loudest keystroke (dB at the chain input), or None if the mic gave no signal at all.
    GateMeasured(Option<f32>),
    MicMissing(bool),
    /// LADSPA plugins were re-scanned (payload: `plugins_json`).
    PluginsChanged(String),
}

/// DeepFilterNet LADSPA download, pinned like setup-deps.sh (a test keeps the two in sync).
pub const DF_VERSION: &str = "0.5.6";
pub const DF_SHA256: &str = "2ca3205c2911d389604a826a240e745597d50252b5cab81c8248252b335e2236";
pub fn df_url() -> String {
    format!(
        "https://github.com/Rikorose/DeepFilterNet/releases/download/v{0}/libdeep_filter_ladspa-{0}-x86_64-unknown-linux-gnu.so",
        DF_VERSION
    )
}

struct Procs {
    map: HashMap<String, Child>,
    post_gen: u32,
    post_sig: Vec<FilterType>,
    dn_engine: String,
    ch_names: HashMap<String, String>,
    mic2_sig: Option<Vec<FilterType>>,
    gen_counter: u32,
    /// Send stage key ("send:voice", "send:<mix id>") -> signature of its source list.
    send_sigs: HashMap<String, String>,
}

/// A send stage: per-source gains into Voice Chat or into a custom mix.
#[derive(Clone, PartialEq)]
enum SendTarget {
    Voice,
    Custom { id: String, name: String },
}

impl SendTarget {
    fn key(&self) -> String {
        match self {
            SendTarget::Voice => "voice".into(),
            SendTarget::Custom { id, .. } => id.clone(),
        }
    }
    fn proc_key(&self) -> String {
        format!("send:{}", self.key())
    }
}

#[derive(Default)]
struct ParamCache {
    ids: HashMap<String, u32>,
    last: HashMap<String, f32>,
}

struct AutoPad {
    pad_db: f32,
    orig: Option<(u32, f32, usize)>,
    last_step: Instant,
    /// The device ignored a volume step: padding would only add digital gain, so it is off.
    unsupported: bool,
}

struct MeterProc {
    source: String,
    child: Child,
}

pub struct Engine {
    pub state: Arc<Mutex<StudioMicConfig>>,
    /// Re-scanned after an in-app plugin download (`rescan_plugins`).
    plugins: RwLock<Plugins>,
    ops: Mutex<()>,
    /// Held for the whole of `swap_mic_chain`. `relink_all` and `apply_mixer` wait on it, so the
    /// Voice_Out firewall never cuts the incoming chain and ducking never un-fades the old one.
    /// Separate from `ops` because callers of those two often hold `ops` already (non-reentrant).
    swap_guard: Mutex<()>,
    procs: Mutex<Procs>,
    cache: Mutex<ParamCache>,
    meters: Mutex<HashMap<String, MeterProc>>,
    peaks: Mutex<HashMap<String, (f32, Instant)>>,
    /// The mic's real resolution as detected from its samples (`BitDepth`): 16, 24 (24-bit or
    /// float), or 0 while unknown.
    pub mic_bits: std::sync::atomic::AtomicU8,
    peak_hold: Mutex<HashMap<String, f32>>,
    duck_db: Mutex<f32>,
    pub ui_visible: AtomicBool,
    preview_active: AtomicBool,
    preview_loop: AtomicBool,
    preview_child: Mutex<Option<Child>>,
    recorder: Mutex<Option<Child>>,
    measuring: AtomicBool,
    autopad: Mutex<AutoPad>,
    last_preview_use: Mutex<Instant>,
    running: AtomicBool,
    monitor_set_at: Mutex<Instant>,
    mic_missing: AtomicBool,
    raw_linked: AtomicBool,
    downloading: AtomicBool,
    events: Mutex<Option<Box<dyn Fn(EngineEvent) + Send>>>,
}

fn log_file() -> Stdio {
    let path = cache_dir().join("engine.log");
    let _ = std::fs::create_dir_all(cache_dir());
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null())
}

fn pids_path() -> PathBuf {
    cache_dir().join("pids.json")
}

fn proc_args(pid: u32) -> Option<Vec<String>> {
    let b = std::fs::read(format!("/proc/{}/cmdline", pid)).ok()?;
    Some(b.split(|c| *c == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).to_string()).collect())
}

fn proc_uid(pid: impl std::fmt::Display) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(format!("/proc/{}", pid)).ok().map(|m| m.uid())
}

/// True only for an engine child of ours: matched on the executable name AND its exact
/// arguments (never on text that merely appears in some other command line, e.g. a grep).
fn is_our_child(args: &[String]) -> bool {
    let Some(exe) = args.first().map(|a| a.rsplit('/').next().unwrap_or(a).to_string()) else { return false };
    match exe.as_str() {
        "pipewire" => args.windows(2).any(|w| {
            w[0] == "-c" && ((w[1].contains("/studio-mic/sm-") && w[1].ends_with(".conf")) || w[1].ends_with("/studiomic-graph.conf"))
        }),
        "pw-record" | "pw-play" | "pw-cat" => {
            let ours = args.iter().any(|a| {
                ["StudioMic_Meter_", "StudioMic_Preview", "StudioMic_Recorder", "StudioMic_GapProbe", "StudioMic_LatProbe"]
                    .iter()
                    .any(|n| a.contains(&format!("node.name = {}", n)) || a.contains(&format!("node.name={}", n)))
            });
            // v0 meters: `pw-record [--target X] --channels 1 --rate 8000 --format s16 -`
            let legacy_meter = exe == "pw-record"
                && args.last().map(|a| a == "-").unwrap_or(false)
                && args.windows(2).any(|w| w[0] == "--rate" && w[1] == "8000")
                && args.windows(2).any(|w| w[0] == "--format" && w[1] == "s16");
            ours || legacy_meter
        }
        _ => false,
    }
}

/// Downloads `url` to `dir/name` only if its SHA256 matches; the file appears atomically.
fn fetch_verified(url: &str, sha256: &str, dir: &std::path::Path, name: &str) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(".{}.part", name));
    let _ = std::fs::remove_file(&tmp);
    let ok = |c: &mut Command| c.stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
    let fetched = ok(Command::new("curl").args(["-fsSL", "--retry", "2", "-o"]).arg(&tmp).arg(url))
        || ok(Command::new("wget").args(["-q", "-O"]).arg(&tmp).arg(url));
    if !fetched {
        let _ = std::fs::remove_file(&tmp);
        return Err("could not download (needs curl or wget and a network connection)".into());
    }
    let got = Command::new("sha256sum")
        .arg(&tmp)
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().next().map(|s| s.to_string()))
        .unwrap_or_default();
    if got != sha256 {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("checksum mismatch (got {}), file discarded", if got.is_empty() { "nothing" } else { &got }));
    }
    let dest = dir.join(name);
    std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
    Ok(dest)
}

/// pw-record arguments for a meter that streams samples to stdout.
///
/// `--raw` is essential: without it, pw-record writes an AU (.snd) file to a pipe. Fedora's
/// PipeWire writes the standard BIG-endian AU, so reading the samples as little-endian floats
/// gave garbage near full scale on every meter (red meters, and Clipguard auto-pad stepping the
/// mic down even at 1% volume) while the audio itself was fine. Arch happens to write
/// little-endian AU, which hid the bug there. Raw output is headerless, CPU-native samples.
/// Float samples: no clamping at full scale, so real overs (> 0 dBFS) stay visible. Cleanup
/// scripts match meters by their StudioMic_Meter_ node name.
fn meter_format_args(rate: u32, channels: u32) -> Vec<String> {
    ["--raw", "--rate", &rate.to_string(), "--channels", &channels.to_string(), "--format", "f32"].iter().map(|s| s.to_string()).collect()
}

/// Silence floor of the meters.
const METER_FLOOR_DB: f32 = -80.0;

/// Peak of a block of little-endian f32 samples in dBFS, down to -80 (silence = exactly -80, so
/// a fader boost never lifts it into a visible level). Not clamped at 0 dB.
fn block_peak_db(buf: &[u8]) -> f32 {
    let max_abs = buf
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]).abs())
        .filter(|v| v.is_finite())
        .fold(0.0f32, f32::max);
    if max_abs <= 1e-4 { METER_FLOOR_DB } else { (20.0 * max_abs.log10()).max(METER_FLOOR_DB) }
}

/// Real clipping = the converter railed: the same non-trivial sample value repeats several times
/// in a row on one channel (a flat top). A loud but clean peak, or a level pushed past 0 dBFS by
/// software gain (still float, caught by the limiters), never does that.
///
/// A 16-bit converter is the exception: its samples sit on a coarse grid, so near the crest of a
/// loud, low sound (rumble, a plosive) a few neighbours round to the same value without any
/// clipping. For those mics (and until the mic's resolution is known) a flat top must be longer
/// and happen at least twice in the block: voice clipping does (two cycles of a voice fit in
/// 25 ms), a 10-40 Hz crest can't.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ClipRule {
    min_run: usize,
    min_runs: usize,
}

impl ClipRule {
    /// 24-bit and float mics.
    pub(crate) const FINE: ClipRule = ClipRule { min_run: 6, min_runs: 1 };
    /// 16-bit mics, and any mic until `BitDepth` knows better.
    pub(crate) const COARSE: ClipRule = ClipRule { min_run: 12, min_runs: 2 };
}

fn has_flat_top(buf: &[u8], channels: usize, rule: ClipRule) -> bool {
    let ch = channels.max(1);
    let samples: Vec<f32> = buf.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    (0..ch).any(|c| {
        let (mut run, mut runs) = (1usize, 0usize);
        let mut prev = f32::NAN;
        for &x in samples.iter().skip(c).step_by(ch) {
            if x.abs() > 0.5 && x == prev {
                run += 1;
                if run == rule.min_run {
                    runs += 1;
                    if runs >= rule.min_runs {
                        return true;
                    }
                }
            } else {
                run = 1;
            }
            prev = x;
        }
        false
    })
}

/// Works out the mic's real resolution from its samples, silently: a 16-bit converter's values
/// all sit on a grid of 1/32768 steps (times whatever volume is applied after it), so the
/// smallest change between neighbouring samples never gets below about 1.5e-5 (5e-6 even at
/// -16 dB of volume). A 24-bit or float mic shows far smaller steps within a second of any
/// sound, room noise included. Mics that claim 24-bit but deliver 16 are caught the same way.
#[derive(Debug)]
pub(crate) struct BitDepth {
    min_step: f32,
    blocks: u32,
}

impl Default for BitDepth {
    fn default() -> Self {
        BitDepth { min_step: f32::INFINITY, blocks: 0 }
    }
}

impl BitDepth {
    /// Blocks (~25 ms) with sound needed before deciding: about one second.
    const BLOCKS: u32 = 40;

    pub(crate) fn observe(&mut self, buf: &[u8], channels: usize) {
        let ch = channels.max(1);
        let samples: Vec<f32> = buf.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        if !samples.iter().any(|x| x.abs() > 1e-4) {
            return; // digital silence or a muted input says nothing about the converter
        }
        for c in 0..ch {
            let mut prev = f32::NAN;
            for &x in samples.iter().skip(c).step_by(ch) {
                let d = (x - prev).abs();
                if d > 0.0 && d < self.min_step {
                    self.min_step = d;
                }
                prev = x;
            }
        }
        self.blocks = self.blocks.saturating_add(1);
    }

    /// 16 or 24 (24-bit or float), or None while still listening.
    pub(crate) fn bits(&self) -> Option<u8> {
        if self.blocks < Self::BLOCKS {
            None
        } else if self.min_step > 5e-6 {
            Some(16)
        } else {
            Some(24)
        }
    }

    pub(crate) fn clip_rule(&self) -> ClipRule {
        if self.bits() == Some(24) { ClipRule::FINE } else { ClipRule::COARSE }
    }
}

fn signal(pid: u32, sig: &str) {
    let _ = Command::new("kill").args([sig, &pid.to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

fn stop_child(mut child: Child) {
    signal(child.id(), "-TERM");
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(400) {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Kills processes left behind by a previous run (crash, SIGKILL, old v0 engine).
pub fn cleanup_stale_processes() -> usize {
    let me = std::process::id();
    let mut killed = 0;
    let mut candidates: Vec<u32> = std::fs::read_to_string(pids_path())
        .ok()
        .and_then(|s| serde_json::from_str::<HashMap<String, u32>>(&s).ok())
        .map(|m| m.into_values().collect())
        .unwrap_or_default();
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            if let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() {
                candidates.push(pid);
            }
        }
    }
    candidates.sort();
    candidates.dedup();
    let my_uid = proc_uid("self");
    for pid in candidates {
        if pid == me || proc_uid(pid) != my_uid {
            continue;
        }
        if let Some(args) = proc_args(pid) {
            if is_our_child(&args) {
                signal(pid, "-TERM");
                killed += 1;
            }
        }
    }
    if killed > 0 {
        std::thread::sleep(Duration::from_millis(350));
    }
    let _ = std::fs::remove_file(pids_path());
    killed
}

impl Engine {
    // -----------------------------------------------------------------------------------------
    // Startup / shutdown
    // -----------------------------------------------------------------------------------------

    pub fn start(state: Arc<Mutex<StudioMicConfig>>) -> Arc<Engine> {
        let plugins = Plugins::discover();
        println!(
            "LADSPA: lsp={} deepfilter={} rnnoise={} speex={} (bundled in this build: {:?})",
            plugins.lsp.is_some(),
            plugins.deepfilter.is_some(),
            plugins.rnnoise.is_some(),
            plugins.speex.is_some(),
            crate::audio::plugins_bundle::embedded_names()
        );
        let _ = std::fs::write(cache_dir().join("engine.log"), b"");
        let engine = Arc::new(Engine {
            state,
            plugins: RwLock::new(plugins),
            ops: Mutex::new(()),
            swap_guard: Mutex::new(()),
            procs: Mutex::new(Procs {
                map: HashMap::new(),
                post_gen: 0,
                post_sig: Vec::new(),
                dn_engine: "off".into(),
                ch_names: HashMap::new(),
                mic2_sig: None,
                gen_counter: 0,
                send_sigs: HashMap::new(),
            }),
            cache: Mutex::new(ParamCache::default()),
            meters: Mutex::new(HashMap::new()),
            peaks: Mutex::new(HashMap::new()),
            mic_bits: std::sync::atomic::AtomicU8::new(0),
            peak_hold: Mutex::new(HashMap::new()),
            duck_db: Mutex::new(0.0),
            ui_visible: AtomicBool::new(true),
            preview_active: AtomicBool::new(false),
            preview_loop: AtomicBool::new(false),
            preview_child: Mutex::new(None),
            recorder: Mutex::new(None),
            measuring: AtomicBool::new(false),
            autopad: Mutex::new(AutoPad { pad_db: 0.0, orig: None, last_step: Instant::now(), unsupported: false }),
            last_preview_use: Mutex::new(Instant::now()),
            running: AtomicBool::new(true),
            monitor_set_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            mic_missing: AtomicBool::new(false),
            raw_linked: AtomicBool::new(false),
            downloading: AtomicBool::new(false),
            events: Mutex::new(None),
        });

        let killed = cleanup_stale_processes();
        if killed > 0 {
            println!("Cleaned up {} stale Studio Mic audio processes", killed);
        }
        // Remove warm-up drivers left behind by a crash mid-swap (only safe before any swap runs).
        for n in pw::nodes().into_iter().filter(|n| n.name.starts_with("StudioMic_Warmup_")) {
            pw::destroy_node(n.id);
        }
        engine.ensure_buses();
        engine.build_all();

        for (name, f) in [
            ("sm-maintenance", Engine::maintenance_loop as fn(Arc<Engine>)),
            ("sm-ducking", Engine::ducking_loop),
            ("sm-meters", Engine::meter_manager_loop),
            ("sm-peaks", Engine::peak_broadcast_loop),
            ("sm-monitor-sync", Engine::monitor_sync_loop),
            ("sm-preview-expiry", Engine::preview_expiry_loop),
        ] {
            let e = engine.clone();
            let _ = std::thread::Builder::new().name(name.into()).spawn(move || f(e));
        }
        engine
    }

    pub fn set_event_sink<F: Fn(EngineEvent) + Send + 'static>(&self, f: F) {
        *self.events.lock().unwrap() = Some(Box::new(f));
    }

    fn emit(&self, ev: EngineEvent) {
        if let Some(f) = self.events.lock().unwrap().as_ref() {
            f(ev);
        }
    }

    pub fn shutdown(&self) {
        if !self.running.swap(false, Ordering::SeqCst) {
            return;
        }
        self.preview_stop_internal(false);
        if let Some(c) = self.recorder.lock().unwrap().take() {
            stop_child(c);
        }
        self.restore_autopad();
        for (_, m) in self.meters.lock().unwrap().drain() {
            stop_child(m.child);
        }
        let _g = self.ops.lock().unwrap();
        let mut procs = self.procs.lock().unwrap();
        let keys: Vec<String> = procs.map.keys().cloned().collect();
        for k in keys {
            if let Some(c) = procs.map.remove(&k) {
                stop_child(c);
            }
        }
        drop(procs);
        PARAMS.shutdown();
        let _ = std::fs::remove_file(PREVIEW_WAV);
        let _ = std::fs::remove_file(pids_path());
        println!("Studio Mic engine stopped (Tier-1 buses left in place).");
    }

    fn snapshot(&self) -> StudioMicConfig {
        self.state.lock().unwrap().clone()
    }

    fn ladspa_path() -> String {
        fc::ladspa_search_path().join(":")
    }

    fn save_pids(procs: &Procs) {
        let map: HashMap<&String, u32> = procs.map.iter().map(|(k, c)| (k, c.id())).collect();
        if let Ok(s) = serde_json::to_string_pretty(&map) {
            let _ = std::fs::create_dir_all(cache_dir());
            let _ = std::fs::write(pids_path(), s);
        }
    }

    fn spawn_conf(procs: &mut Procs, key: &str, conf_name: &str, content: &str) -> std::io::Result<()> {
        let path = fc::write_conf(conf_name, content)?;
        let child = Command::new("pipewire")
            .arg("-c")
            .arg(&path)
            .env("LADSPA_PATH", Self::ladspa_path())
            .stdin(Stdio::null())
            .stdout(log_file())
            .stderr(log_file())
            .spawn()?;
        if let Some(old) = procs.map.insert(key.to_string(), child) {
            stop_child(old);
        }
        Self::save_pids(procs);
        Ok(())
    }

    fn kill_proc(procs: &mut Procs, key: &str) {
        if let Some(c) = procs.map.remove(key) {
            stop_child(c);
        }
        Self::save_pids(procs);
    }

    /// Creates any missing Tier-1 bus as a lingering node. Idempotent:
    /// existing nodes are reused, duplicates left by older builds are removed.
    pub fn ensure_buses(&self) {
        let nodes = pw::nodes();
        let mut wanted: Vec<(String, &str, String)> = BUSES
            .iter()
            .map(|(k, class, desc, _)| (fc::bus_node(k), *class, desc.to_string()))
            .collect();
        let custom = self.state.lock().unwrap().custom_mixes.clone();
        for m in &custom {
            wanted.push((fc::bus_node(&fc::mix_bus_key(&m.id)), "Audio/Source/Virtual", format!("Studio Mic · {}", m.name)));
        }
        // Lingering buses of mixes that were deleted (`StudioMic_Mix_<id>`; the _Sum/_Lim stage
        // nodes belong to the mix's own process and go away with it).
        for n in &nodes {
            if n.name == REMOVED_LEGACY_ALIAS {
                pw::destroy_node(n.id);
                println!("Removed the old {} alias (use \"Studio Mic (Microphone Only)\")", n.name);
            }
            if let Some(id) = n.name.strip_prefix("StudioMic_Mix_") {
                if !id.ends_with("_Sum") && !id.ends_with("_Lim") && !custom.iter().any(|m| m.id == id) {
                    pw::destroy_node(n.id);
                    println!("Removed bus of deleted mix {}", n.name);
                }
            }
        }
        for (name, class, desc) in wanted {
            let matching: Vec<&pw::NodeInfo> = nodes.iter().filter(|n| n.name == name).collect();
            if matching.is_empty() {
                if pw::create_lingering_null_node(&name, class, &desc) {
                    println!("Created lingering bus {}", name);
                }
            } else if matching.len() > 1 {
                for dup in &matching[1..] {
                    pw::destroy_node(dup.id);
                }
            }
        }
    }

    /// Spawns every stage for the current state and wires the graph.
    fn mic_chain_conf(&self, s: &StudioMicConfig, generation: u32, engine: &str, routes: (f32, f32, f32, f32)) -> String {
        fc::mic_post_conf(&self.plugins(), generation, engine, &s.denoiser, &s.eq_bands, &s.noise_clean, &s.mic, self.trim_db(s), routes)
    }

    fn build_all(&self) {
        let _g = self.ops.lock().unwrap();
        let s = self.snapshot();
        let mut procs = self.procs.lock().unwrap();

        let _ = Self::spawn_conf(&mut procs, "buses", "buses", &fc::bus_master_conf(&self.plugins()));

        procs.gen_counter += 1;
        let generation = procs.gen_counter;
        let engine = self.effective_denoiser(&s);
        let routes = self.mic_route_gains(&s);
        let _ = Self::spawn_conf(&mut procs, "post", &format!("mic-g{}", generation), &self.mic_chain_conf(&s, generation, &engine, routes));
        procs.post_gen = generation;
        procs.post_sig = fc::eq_signature(&s.eq_bands);
        procs.dn_engine = engine.clone();

        let solo = Self::solo_active(&s);
        for ch in &s.virtual_channels {
            let gains = fc::route_gains(&ch.mix, 0.0, solo);
            let _ = Self::spawn_conf(&mut procs, &format!("ch:{}", ch.id), &format!("ch-{}", ch.id), &fc::channel_conf(ch, gains));
            procs.ch_names.insert(ch.id.clone(), ch.name.clone());
        }
        if s.dual_mic.enabled {
            let gains = fc::route_gains(&s.dual_mic.mix, 0.0, solo);
            let _ = Self::spawn_conf(&mut procs, "mic2", "mic2", &fc::mic2_conf(&s.dual_mic.eq_bands, &s.dual_mic.mix, gains));
            procs.mic2_sig = Some(fc::eq_signature(&s.dual_mic.eq_bands));
        }

        self.spawn_sends_locked(&mut procs, &s);

        let expected = self.expected_nodes(&procs, &s);
        drop(procs);
        let mut found = pw::wait_for_nodes(&expected, Duration::from_secs(6));
        let chain_missing = !found.contains_key(&fc::post_in(generation));
        if chain_missing && engine != "off" {
            // A broken denoiser must never take the voice down: rebuild the chain without it.
            eprintln!("Mic chain with '{}' did not start; rebuilding without the noise remover", engine);
            let mut procs = self.procs.lock().unwrap();
            let routes = self.mic_route_gains(&s);
            let _ = Self::spawn_conf(&mut procs, "post", &format!("mic-g{}", generation), &self.mic_chain_conf(&s, generation, "off", routes));
            procs.dn_engine = "off".into();
            drop(procs);
            found.extend(pw::wait_for_nodes(&[fc::post_in(generation), fc::post_out(generation)], Duration::from_secs(4)));
            self.emit(EngineEvent::Notice("Noise remover failed to load; running without it.".into()));
        }
        if found.len() < expected.len() {
            let missing: Vec<&String> = expected.iter().filter(|n| !found.contains_key(*n)).collect();
            eprintln!("Engine: nodes did not appear in time: {:?} (see ~/.cache/studio-mic/engine.log)", missing);
        }
        self.cache.lock().unwrap().ids = found;
        self.relink_all();
        self.apply_all_locked();
    }

    fn expected_nodes(&self, procs: &Procs, s: &StudioMicConfig) -> Vec<String> {
        let mut v: Vec<String> = BUSES.iter().flat_map(|(k, ..)| [fc::bus_sum(k), fc::bus_lim(k)]).collect();
        v.push(fc::post_in(procs.post_gen));
        v.push(fc::post_out(procs.post_gen));
        for ch in &s.virtual_channels {
            v.push(fc::channel_sink(&ch.id));
            v.push(fc::channel_out(&ch.id));
        }
        if procs.mic2_sig.is_some() {
            v.push(MIC2_IN.into());
            v.push(MIC2_OUT.into());
        }
        for t in Self::send_targets(s) {
            if procs.map.contains_key(&t.proc_key()) {
                v.push(fc::send_in(&t.key()));
                v.push(fc::send_out(&t.key()));
                if let SendTarget::Custom { id, .. } = &t {
                    v.push(fc::bus_sum(&fc::mix_bus_key(id)));
                    v.push(fc::bus_lim(&fc::mix_bus_key(id)));
                }
            }
        }
        v
    }

    // -----------------------------------------------------------------------------------------
    // Sends (custom mixes, extra Voice Chat sources)
    // -----------------------------------------------------------------------------------------

    /// Every send stage the current state needs.
    fn send_targets(s: &StudioMicConfig) -> Vec<SendTarget> {
        let mut v = Vec::new();
        if !Self::send_sources(s, &SendTarget::Voice).is_empty() {
            v.push(SendTarget::Voice);
        }
        for m in &s.custom_mixes {
            v.push(SendTarget::Custom { id: m.id.clone(), name: m.name.clone() });
        }
        v
    }

    /// Sources of a send stage, in port order. Voice Chat extras: Mic 2 and every channel except
    /// the voice-chat one (echo guard; the main mic has its own direct path). Custom mixes:
    /// every source.
    fn send_sources(s: &StudioMicConfig, t: &SendTarget) -> Vec<String> {
        let mut v = Vec::new();
        if matches!(t, SendTarget::Custom { .. }) {
            v.push("mic".to_string());
        }
        if s.dual_mic.enabled {
            v.push("mic2".to_string());
        }
        for ch in &s.virtual_channels {
            if matches!(t, SendTarget::Custom { .. }) || !ch.is_comms() {
                v.push(ch.id.clone());
            }
        }
        v
    }

    /// Linear gain of `src` into a send target (0 = not routed / muted). While the isolated
    /// preview runs, the main mic is silent in every send (as it is in the built-in mixes).
    fn send_gain(s: &StudioMicConfig, t: &SendTarget, src: &str, preview: bool) -> f32 {
        if preview && src == "mic" {
            return 0.0;
        }
        let Some(m) = s.mix(src) else { return 0.0 };
        // The mic's master mute (Stream Mute) silences it in every broadcast mix.
        if (src == "mic" || src == "mic2") && m.stream_muted {
            return 0.0;
        }
        match t {
            SendTarget::Voice => {
                if m.voice_routed && !m.voice_muted { db_to_lin(m.voice_gain_db) } else { 0.0 }
            }
            SendTarget::Custom { id, .. } => match m.sends.get(id) {
                Some(c) if !c.muted => db_to_lin(c.gain_db),
                _ => 0.0,
            },
        }
    }

    fn send_gains(s: &StudioMicConfig, t: &SendTarget, preview: bool) -> Vec<(String, f32)> {
        Self::send_sources(s, t).into_iter().map(|src| {
            let g = Self::send_gain(s, t, &src, preview);
            (src, g)
        }).collect()
    }

    /// Output port(s) a send source is tapped from (left, right). Mono taps feed both sides.
    fn send_source_ports(procs: &Procs, src: &str) -> (String, String) {
        match src {
            "mic" => {
                let p = format!("{}:output_AUX8", fc::post_out(procs.post_gen));
                (p.clone(), p)
            }
            "mic2" => {
                let p = format!("{}:output_AUX6", MIC2_OUT);
                (p.clone(), p)
            }
            id => (format!("{}:monitor_FL", fc::channel_sink(id)), format!("{}:monitor_FR", fc::channel_sink(id))),
        }
    }

    /// Spawns / restarts / removes send stages to match state (caller holds `ops` and `procs`).
    /// A stage only restarts when its source list changes (a channel or Mic 2 added/removed);
    /// routing changes are live gains.
    fn spawn_sends_locked(&self, procs: &mut Procs, s: &StudioMicConfig) -> Vec<String> {
        let targets = Self::send_targets(s);
        let lsp = self.plugins().lsp.is_some();
        let mut started = Vec::new();
        let running: Vec<String> = procs.map.keys().filter(|k| k.starts_with("send:")).cloned().collect();
        for k in running {
            if !targets.iter().any(|t| t.proc_key() == k) {
                let key = k.trim_start_matches("send:").to_string();
                let mut names = vec![fc::send_in(&key), fc::send_out(&key)];
                if key != "voice" {
                    names.push(fc::bus_sum(&fc::mix_bus_key(&key)));
                    names.push(fc::bus_lim(&fc::mix_bus_key(&key)));
                }
                Self::detach_nodes(&names);
                Self::kill_proc(procs, &k);
                procs.send_sigs.remove(&k);
                for n in &names {
                    self.forget_node(n);
                }
                println!("Removed send stage {}", k);
            }
        }
        for t in targets {
            let gains = Self::send_gains(s, &t, self.preview_active.load(Ordering::SeqCst));
            let sig = format!(
                "{}|{}|{}",
                gains.iter().map(|(src, _)| src.as_str()).collect::<Vec<_>>().join(","),
                lsp,
                match &t { SendTarget::Custom { name, .. } => name.as_str(), SendTarget::Voice => "" }
            );
            let key = t.proc_key();
            if procs.map.contains_key(&key) && procs.send_sigs.get(&key) == Some(&sig) {
                continue;
            }
            let conf = match &t {
                SendTarget::Voice => fc::voice_send_conf(&gains),
                SendTarget::Custom { id, name } => fc::custom_mix_conf(&self.plugins(), id, name, &gains),
            };
            if Self::spawn_conf(procs, &key, &format!("send-{}", t.key()), &conf).is_ok() {
                procs.send_sigs.insert(key, sig);
                for n in [fc::send_in(&t.key()), fc::send_out(&t.key())] {
                    self.forget_node(&n);
                    started.push(n);
                }
                if let SendTarget::Custom { id, .. } = &t {
                    started.push(fc::bus_sum(&fc::mix_bus_key(id)));
                    started.push(fc::bus_lim(&fc::mix_bus_key(id)));
                }
            }
        }
        started
    }

    /// Brings send stages, custom buses and their links in line with state.
    pub fn sync_sends(&self) {
        let _g = self.ops.lock().unwrap();
        let s = self.snapshot();
        let started = {
            let mut p = self.procs.lock().unwrap();
            self.spawn_sends_locked(&mut p, &s)
        };
        if !started.is_empty() {
            let found = pw::wait_for_nodes(&started, Duration::from_secs(3));
            self.cache.lock().unwrap().ids.extend(found);
        }
        drop(_g);
        self.ensure_buses();
        self.relink_all();
        self.apply_mixer(false);
    }

    fn effective_denoiser(&self, s: &StudioMicConfig) -> String {
        let e = s.denoiser.engine.as_str();
        if self.plugins().denoiser_available(e) { e.to_string() } else { "off".to_string() }
    }

    // -----------------------------------------------------------------------------------------
    // Linking
    // -----------------------------------------------------------------------------------------

    fn hw_mic_present(name: &str) -> bool {
        pw::nodes().iter().any(|n| n.name == name && n.class.starts_with("Audio/Source"))
    }

    /// Desired links for the whole graph.
    fn desired_links(&self, s: &StudioMicConfig, procs: &Procs) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = Vec::new();
        let p = |n: &str, port: &str| format!("{}:{}", n, port);

        // Hardware mic -> mic chain (unless the isolated preview is feeding it)
        let post_in = fc::post_in(procs.post_gen);
        let post_out = fc::post_out(procs.post_gen);
        if !self.preview_active.load(Ordering::SeqCst) {
            if let Some(hw) = &s.active_hw_mic {
                let ports = pw::ports_of(hw, true);
                if !ports.is_empty() {
                    v.extend(pw::stereo_pairs(hw, &ports, &post_in, "input_FL", "input_FR"));
                }
            }
        }
        // Raw monitor: direct hardware -> headphone bus link (no filter stage = lowest latency)
        v.extend(self.raw_monitor_links(s));
        // Mic chain AUX0..7 -> Voice / Stream / VOD / Monitor sums (the mic is the ONLY Voice_Out feed)
        for (i, key) in ["Voice_Out", "Stream_Mix", "VOD_Mix", "Monitor_Mix"].iter().enumerate() {
            v.push((p(&post_out, &format!("output_AUX{}", i * 2)), p(&fc::bus_sum(key), "input_FL")));
            v.push((p(&post_out, &format!("output_AUX{}", i * 2 + 1)), p(&fc::bus_sum(key), "input_FR")));
        }
        // Channels AUX0..5 -> Stream / VOD / Monitor (raw passthrough; never Voice_Out)
        let strip_links = |out: &str, v: &mut Vec<(String, String)>| {
            for (i, key) in ["Stream_Mix", "VOD_Mix", "Monitor_Mix"].iter().enumerate() {
                v.push((p(out, &format!("output_AUX{}", i * 2)), p(&fc::bus_sum(key), "input_FL")));
                v.push((p(out, &format!("output_AUX{}", i * 2 + 1)), p(&fc::bus_sum(key), "input_FR")));
            }
        };
        for ch in &s.virtual_channels {
            strip_links(&fc::channel_out(&ch.id), &mut v);
            if ch.kind == "hardware" {
                if let Some(src) = &ch.hw_source {
                    let ports = pw::ports_of(src, true);
                    if !ports.is_empty() {
                        v.extend(pw::stereo_pairs(src, &ports, &fc::channel_sink(&ch.id), "playback_FL", "playback_FR"));
                    }
                }
            }
        }
        if procs.mic2_sig.is_some() {
            strip_links(MIC2_OUT, &mut v);
            if let Some(hw2) = &s.dual_mic.hw_mic {
                let ports = pw::ports_of(hw2, true);
                if let Some((o, _)) = pw::stereo_pairs(hw2, &ports, MIC2_IN, "input_MONO", "input_MONO").into_iter().next() {
                    v.push((o, p(MIC2_IN, "input_MONO")));
                }
            }
        }
        // Bus limiters -> lingering buses
        for (key, ..) in BUSES.iter() {
            let (il, ir) = fc::bus_inputs(key);
            v.push((p(&fc::bus_lim(key), "output_FL"), p(&fc::bus_node(key), il)));
            v.push((p(&fc::bus_lim(key), "output_FR"), p(&fc::bus_node(key), ir)));
        }
        // Monitor mix -> headphones
        if let Some(out) = self.monitor_target(s) {
            v.extend(Self::output_device_links(&fc::bus_node("Monitor_Mix"), "monitor", &out));
        }
        // Sends: sources -> send stage -> target (PipeWire sums every output into the target).
        for t in Self::send_targets(s) {
            if !procs.map.contains_key(&t.proc_key()) {
                continue;
            }
            let key = t.key();
            let target = match &t {
                SendTarget::Voice => fc::bus_sum("Voice_Out"),
                SendTarget::Custom { id, .. } => fc::bus_sum(&fc::mix_bus_key(id)),
            };
            for (i, src) in Self::send_sources(s, &t).iter().enumerate() {
                let (in_l, in_r, out_l, out_r) = fc::send_ports(i);
                let (src_l, src_r) = Self::send_source_ports(procs, src);
                v.push((src_l, p(&fc::send_in(&key), &in_l)));
                v.push((src_r, p(&fc::send_in(&key), &in_r)));
                v.push((p(&fc::send_out(&key), &out_l), p(&target, "input_FL")));
                v.push((p(&fc::send_out(&key), &out_r), p(&target, "input_FR")));
            }
            if let SendTarget::Custom { id, .. } = &t {
                let k = fc::mix_bus_key(id);
                v.push((p(&fc::bus_lim(&k), "output_FL"), p(&fc::bus_node(&k), "input_FL")));
                v.push((p(&fc::bus_lim(&k), "output_FR"), p(&fc::bus_node(&k), "input_FR")));
            }
        }
        // Optional physical outputs of the other mixes (e.g. a Speaker mix).
        for (bus, dev) in Self::mix_outputs(s) {
            v.extend(Self::output_device_links(&bus, "capture", &dev));
        }
        v
    }

    /// (bus node, physical sink) for every mix other than My Mix that has an output device.
    fn mix_outputs(s: &StudioMicConfig) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = [("stream", "Stream_Mix"), ("vod", "VOD_Mix"), ("voice", "Voice_Out")]
            .iter()
            .filter_map(|(k, bus)| s.mix_output_devices.get(*k).map(|d| (fc::bus_node(bus), d.clone())))
            .collect();
        for m in &s.custom_mixes {
            if let Some(d) = &m.output_device {
                v.push((fc::bus_node(&fc::mix_bus_key(&m.id)), d.clone()));
            }
        }
        // Feedback guard: a mix never plays into one of our own nodes.
        v.retain(|(_, d)| !d.starts_with("StudioMic_") && !d.starts_with("studiomic"));
        v
    }

    /// Stereo links from a bus (`<prefix>_FL/_FR` ports) to a playback device.
    fn output_device_links(bus: &str, prefix: &str, out: &str) -> Vec<(String, String)> {
        let ports = pw::ports_of(out, false);
        let pick = |want: &[&str]| ports.iter().find(|x| want.iter().any(|w| x.ends_with(w))).cloned();
        let l = pick(&["_FL", "_AUX0", "_MONO"]).or_else(|| ports.first().cloned());
        let r = pick(&["_FR", "_AUX1", "_MONO"]).or_else(|| ports.get(1).cloned()).or(l.clone());
        match (l, r) {
            (Some(l), Some(r)) => vec![
                (format!("{}:{}_FL", bus, prefix), format!("{}:{}", out, l)),
                (format!("{}:{}_FR", bus, prefix), format!("{}:{}", out, r)),
            ],
            _ => Vec::new(),
        }
    }

    /// Raw monitor = hardware capture ports linked straight into the Monitor_Mix bus (mono-center
    /// sends the left capture port to both sides). Only while "raw" mode is audible.
    fn raw_monitor_links(&self, s: &StudioMicConfig) -> Vec<(String, String)> {
        let m = &s.mic.mix;
        let audible = s.mic.monitor_mode == "raw"
            && !m.monitor_muted
            && !(Self::solo_active(s) && !m.solo)
            && !self.preview_active.load(Ordering::SeqCst);
        let Some(hw) = s.active_hw_mic.as_ref().filter(|_| audible) else { return Vec::new() };
        let ports = pw::ports_of(hw, true);
        let (il, ir) = fc::bus_inputs("Monitor_Mix");
        let mon = fc::bus_node("Monitor_Mix");
        let mut pairs = pw::stereo_pairs(hw, &ports, &mon, il, ir);
        if s.mic.mono_center && pairs.len() == 2 {
            pairs[1].0 = pairs[0].0.clone();
        }
        pairs
    }

    /// Adds/removes only the raw monitor links (cheap; used on monitor mute / solo / mode changes).
    fn sync_raw_links(&self, s: &StudioMicConfig) {
        let want = self.raw_monitor_links(s);
        let had = self.raw_linked.swap(!want.is_empty(), Ordering::SeqCst);
        if want.is_empty() && !had {
            return;
        }
        pw::ensure_links(&want);
        if let Some(hw) = &s.active_hw_mic {
            let prefix = format!("{}:", hw);
            let mon = format!("{}:", fc::bus_node("Monitor_Mix"));
            for (o, i) in pw::links() {
                if o.starts_with(&prefix) && i.starts_with(&mon) && !want.contains(&(o.clone(), i.clone())) {
                    pw::unlink(&o, &i);
                }
            }
        }
    }

    /// Selected headphones, or the default sink. Never one of our own nodes (feedback guard).
    fn monitor_target(&self, s: &StudioMicConfig) -> Option<String> {
        let candidate = s.monitor_output.clone().or_else(|| pw::default_node_name("@DEFAULT_AUDIO_SINK@"))?;
        if candidate.starts_with("StudioMic_") || candidate.starts_with("studiomic") {
            return None;
        }
        Some(candidate)
    }

    pub fn relink_all(&self) {
        let _swap = self.swap_guard.lock().unwrap();
        let s = self.snapshot();
        let procs = self.procs.lock().unwrap();
        let desired = self.desired_links(&s, &procs);
        drop(procs);
        pw::ensure_links(&desired);
        // Drop stale feeds into the mic pre-stage, buses and headphones (old device choices).
        let keep_into = |node: &str| -> Vec<(String, String)> {
            desired.iter().filter(|(_, i)| i.starts_with(&format!("{}:", node))).cloned().collect()
        };
        let mic_in = fc::post_in(self.procs.lock().unwrap().post_gen);
        if !self.preview_active.load(Ordering::SeqCst) {
            // Drop old hardware feeds, but keep Studio Mic's own injectors (gap/latency probes).
            let mut keep = keep_into(&mic_in);
            let prefix = format!("{}:", mic_in);
            keep.extend(pw::links().into_iter().filter(|(o, i)| i.starts_with(&prefix) && o.starts_with("StudioMic_")));
            pw::unlink_inputs_of(&mic_in, &keep);
        }
        self.sync_raw_links(&s);
        let mon = fc::bus_node("Monitor_Mix");
        let mon_prefix = format!("{}:", mon);
        let mut keep_mon: Vec<(String, String)> = desired.iter().filter(|(o, _)| o.starts_with(&mon_prefix)).cloned().collect();
        // Keep the headphone meter tap (`bus:monitor`); it's linked by WirePlumber, not by us.
        keep_mon.extend(pw::links().into_iter().filter(|(o, i)| o.starts_with(&mon_prefix) && i.starts_with("StudioMic_Meter_")));
        pw::unlink_outputs_of(&mon, &keep_mon);
        let nodes = pw::nodes();
        // Hardware inputs feeding a channel strip or Mic 2 that are no longer chosen (switching a
        // Line In / capture card channel to another input, and back, used to leave the old link
        // in place, so both played until a restart). Only links whose source is a physical
        // capture device are touched: app streams into a channel sink stay.
        let hw_sources: std::collections::HashSet<String> = nodes
            .iter()
            .filter(|n| n.class.starts_with("Audio/Source") && !n.name.starts_with("StudioMic_") && !n.name.starts_with("studiomic"))
            .map(|n| n.name.clone())
            .collect();
        let mut hw_fed: Vec<String> = s.virtual_channels.iter().map(|c| fc::channel_sink(&c.id)).collect();
        hw_fed.push(MIC2_IN.to_string());
        for (o, i) in pw::links() {
            let from_hw = o.split_once(':').is_some_and(|(node, _)| hw_sources.contains(node));
            let into_strip = i.split_once(':').is_some_and(|(node, _)| hw_fed.iter().any(|n| n == node));
            if from_hw && into_strip && !desired.contains(&(o.clone(), i.clone())) {
                pw::unlink(&o, &i);
            }
        }
        // Other mixes: drop only links into physical outputs that are no longer chosen. Never
        // touch app links (OBS, Discord, recorders): their ports aren't Audio/Sink devices.
        let sinks: std::collections::HashSet<String> = nodes
            .into_iter()
            .filter(|n| n.class == "Audio/Sink" && !n.name.starts_with("StudioMic_"))
            .map(|n| n.name)
            .collect();
        let mut buses: Vec<String> = ["Stream_Mix", "VOD_Mix", "Voice_Out"].iter().map(|b| fc::bus_node(b)).collect();
        buses.extend(s.custom_mixes.iter().map(|m| fc::bus_node(&fc::mix_bus_key(&m.id))));
        for (o, i) in pw::links() {
            let from_bus = buses.iter().any(|b| o.starts_with(&format!("{}:", b)));
            let to_device = i.split_once(':').is_some_and(|(node, _)| sinks.contains(node));
            if from_bus && to_device && !desired.contains(&(o.clone(), i.clone())) {
                pw::unlink(&o, &i);
            }
        }
        // Hard firewall: only the Voice Chat limiter (mic + allowed inputs) may feed Voice_Out.
        for node in [fc::bus_node("Voice_Out"), fc::bus_sum("Voice_Out")] {
            pw::unlink_inputs_of(&node, &keep_into(&node));
        }
        let mic_missing = s.active_hw_mic.as_ref().map(|h| !Self::hw_mic_present(h)).unwrap_or(true);
        if self.mic_missing.swap(mic_missing, Ordering::SeqCst) != mic_missing {
            self.emit(EngineEvent::MicMissing(mic_missing));
        }
    }

    // -----------------------------------------------------------------------------------------
    // Live parameters
    // -----------------------------------------------------------------------------------------

    fn node_id(&self, name: &str) -> Option<u32> {
        if let Some(id) = self.cache.lock().unwrap().ids.get(name) {
            return Some(*id);
        }
        let id = pw::find_node(name)?.id;
        self.cache.lock().unwrap().ids.insert(name.to_string(), id);
        Some(id)
    }

    fn forget_node(&self, name: &str) {
        let mut c = self.cache.lock().unwrap();
        c.ids.remove(name);
        let prefix = format!("{}|", name);
        c.last.retain(|k, _| !k.starts_with(&prefix));
    }

    /// Sends only changed controls. Gain steps larger than ~2 dB are ramped over ~12 ms.
    fn send(&self, node: &str, controls: &[(String, f32)], ramp: bool) {
        let Some(id) = self.node_id(node) else { return };
        let mut changed: Vec<(String, f32, f32)> = Vec::new();
        {
            let c = self.cache.lock().unwrap();
            for (k, v) in controls {
                let key = format!("{}|{}", node, k);
                match c.last.get(&key) {
                    Some(old) if (old - v).abs() < 1e-5 => {}
                    Some(old) => changed.push((k.clone(), *old, *v)),
                    None => changed.push((k.clone(), *v, *v)),
                }
            }
        }
        if changed.is_empty() {
            return;
        }
        let needs_ramp = ramp
            && changed.iter().any(|(k, old, new)| k.ends_with(":Gain 1") && (old - new).abs() > 0.2 * old.max(*new).max(0.05));
        if needs_ramp {
            for step in [0.25f32, 0.5, 0.75] {
                let inter: Vec<(String, f32)> = changed
                    .iter()
                    .filter(|(k, ..)| k.ends_with(":Gain 1"))
                    .map(|(k, o, n)| (k.clone(), o + (n - o) * step))
                    .collect();
                PARAMS.set_controls(id, &inter);
                std::thread::sleep(Duration::from_millis(4));
            }
        }
        let fin: Vec<(String, f32)> = changed.iter().map(|(k, _, n)| (k.clone(), *n)).collect();
        if PARAMS.set_controls(id, &fin) {
            let mut c = self.cache.lock().unwrap();
            for (k, v) in fin {
                c.last.insert(format!("{}|{}", node, k), v);
            }
        }
    }

    fn solo_active(s: &StudioMicConfig) -> bool {
        s.mic.mix.solo || s.dual_mic.mix.solo || s.virtual_channels.iter().any(|c| c.mix.solo)
    }

    fn trim_db(&self, s: &StudioMicConfig) -> f32 {
        s.calibration.preamp_gain_db + self.autopad.lock().unwrap().pad_db
    }

    /// (voice, stream, vod, monitor) for the mic, honouring mute, solo and preview isolation.
    fn mic_route_gains(&self, s: &StudioMicConfig) -> (f32, f32, f32, f32) {
        let m = &s.mic.mix;
        if self.preview_active.load(Ordering::SeqCst) {
            // Isolated preview: broadcast buses muted, headphones forced on.
            return (0.0, 0.0, 0.0, db_to_lin(m.monitor_gain_db).max(0.05));
        }
        let (stream, vod, monitor) = fc::route_gains(m, 0.0, Self::solo_active(s));
        // Voice Chat: master mic mute (Stream Mute), then the matrix cell's own mute and fader.
        let voice = if m.stream_muted || m.voice_muted { 0.0 } else { db_to_lin(m.voice_gain_db) };
        let monitor = if s.mic.monitor_mode == "raw" { 0.0 } else { monitor };
        (voice, stream, vod, monitor)
    }

    fn duck_for(&self, ch: &crate::config::VirtualChannelConfig) -> f32 {
        if ch.mix.autoduck && !ch.is_comms() { *self.duck_db.lock().unwrap() } else { 0.0 }
    }

    fn apply_all_locked(&self) {
        self.apply_pre();
        self.apply_denoiser_params();
        self.apply_dsp();
        self.apply_eq_params();
        self.apply_mixer(true);
        self.apply_monitor_master();
    }

    pub fn apply_pre(&self) {
        let s = self.snapshot();
        let (g1, g2) = fc::pre_gains(&s.mic, self.trim_db(&s));
        let generation = self.procs.lock().unwrap().post_gen;
        self.send(&fc::post_in(generation), &[("mono:Gain 1".into(), g1), ("mono:Gain 2".into(), g2)], true);
        self.sync_raw_links(&s);
    }

    pub fn apply_denoiser_params(&self) {
        let s = self.snapshot();
        let (generation, engine) = {
            let p = self.procs.lock().unwrap();
            (p.post_gen, p.dn_engine.clone())
        };
        if engine != "off" {
            self.send(&fc::post_in(generation), &fc::denoiser_controls(&s.denoiser, &engine), false);
        }
    }

    pub fn apply_dsp(&self) {
        let s = self.snapshot();
        let generation = self.procs.lock().unwrap().post_gen;
        self.send(&fc::post_in(generation), &fc::post_dsp_controls(&s.noise_clean, &s.mic, self.plugins().lsp.is_some()), false);
    }

    /// The bands laid out on the running chain's slots (see `fc::fit_to_slots`).
    fn slot_bands(bands: &[crate::models::microphone::EqBand], sig: &[FilterType]) -> Vec<crate::models::microphone::EqBand> {
        fc::fit_to_slots(bands, sig).unwrap_or_else(|| bands.to_vec())
    }

    fn apply_eq_params(&self) {
        let s = self.snapshot();
        let (generation, sig, sig2) = {
            let p = self.procs.lock().unwrap();
            (p.post_gen, p.post_sig.clone(), p.mic2_sig.clone())
        };
        self.send(&fc::post_in(generation), &fc::eq_controls("eq", &Self::slot_bands(&s.eq_bands, &sig)), false);
        if s.dual_mic.enabled {
            let b2 = sig2.map(|sig2| Self::slot_bands(&s.dual_mic.eq_bands, &sig2)).unwrap_or_else(|| s.dual_mic.eq_bands.clone());
            self.send(MIC2_IN, &fc::eq_controls("m2eq", &b2), false);
        }
    }

    /// A/B audition: bypass every post-chain stage live (raw capsule), or restore the saved state.
    pub fn ab_bypass(&self, raw: bool) {
        if !raw {
            self.apply_dsp();
            self.apply_eq_params();
            return;
        }
        let s = self.snapshot();
        let mut nc = s.noise_clean.clone();
        nc.gate_enabled = false;
        nc.deesser_enabled = false;
        nc.compressor_enabled = false;
        nc.rumble_hp_enabled = false;
        nc.hiss_lp_enabled = false;
        let mut mic = s.mic.clone();
        mic.clipguard = false;
        let generation = self.procs.lock().unwrap().post_gen;
        let mut c = fc::post_dsp_controls(&nc, &mic, self.plugins().lsp.is_some());
        c.extend(fc::eq_controls("eq", &[]).into_iter().filter(|(k, _)| k.ends_with(":Gain")));
        // Flat peaking slots are transparent; HP/LP slots are pushed out of the audible band.
        // Uses the RUNNING slot layout (the saved band order may differ from it).
        let running_sig = self.procs.lock().unwrap().post_sig.clone();
        for (i, ft) in running_sig.iter().enumerate() {
            match ft {
                FilterType::HighPass => c.push((format!("eq{}:Freq", i + 1), 10.0)),
                FilterType::LowPass => c.push((format!("eq{}:Freq", i + 1), 23500.0)),
                _ => {}
            }
        }
        self.send(&fc::post_in(generation), &c, false);
    }

    /// Mic + channel route gains. `ramp` smooths large steps (mutes); ducking passes false.
    pub fn apply_mixer(&self, ramp: bool) {
        let _swap = self.swap_guard.lock().unwrap();
        let s = self.snapshot();
        let generation = self.procs.lock().unwrap().post_gen;
        let (voice, stream, vod, mon) = self.mic_route_gains(&s);
        self.send(&fc::post_in(generation), &fc::post_route_controls(voice, stream, vod, mon), ramp);
        if ramp {
            // Not from the ducking loop: mute/solo/mode changes may add or remove the raw links.
            self.sync_raw_links(&s);
        }
        let solo = Self::solo_active(&s);
        for ch in &s.virtual_channels {
            let (st, vd, mo) = fc::route_gains(&ch.mix, self.duck_for(ch), solo);
            self.send(&fc::channel_sink(&ch.id), &fc::channel_controls(st, vd, mo, &ch.mix), ramp);
        }
        if s.dual_mic.enabled {
            let (st, vd, mo) = fc::route_gains(&s.dual_mic.mix, 0.0, solo);
            self.send(MIC2_IN, &fc::channel_controls(st, vd, mo, &s.dual_mic.mix), ramp);
        }
        let preview = self.preview_active.load(Ordering::SeqCst);
        for t in Self::send_targets(&s) {
            self.send(&fc::send_in(&t.key()), &fc::send_controls(&Self::send_gains(&s, &t, preview)), ramp);
        }
    }

    /// Master Monitor Gain = the Monitor_Mix node volume (LINEAR, not wpctl's cubic scale).
    pub fn apply_monitor_master(&self) {
        let s = self.snapshot();
        if let Some(id) = self.node_id(&fc::bus_node("Monitor_Mix")) {
            *self.monitor_set_at.lock().unwrap() = Instant::now();
            PARAMS.set_volume(id, 2, db_to_lin(s.monitor_gain_db).max(0.0), s.monitor_muted);
        }
    }

    // -----------------------------------------------------------------------------------------
    // Structural changes (make-before-break)
    // -----------------------------------------------------------------------------------------

    /// EQ edit: live params unless a filter TYPE changed, which swaps the post chain.
    pub fn apply_eq(&self) {
        let _g = self.ops.lock().unwrap();
        let s = self.snapshot();
        // Only a real change in the set of filter types is structural; a re-sorted list is live.
        let need_swap = fc::fit_to_slots(&s.eq_bands, &self.procs.lock().unwrap().post_sig).is_none();
        if need_swap {
            let engine = self.procs.lock().unwrap().dn_engine.clone();
            if self.swap_mic_chain(&engine, false) {
                self.relink_all(); // custom mixes tap the mic chain: follow the new generation now
            }
        } else {
            self.apply_eq_params();
        }
        if s.dual_mic.enabled {
            let fits = self.procs.lock().unwrap().mic2_sig.as_ref().is_some_and(|sig2| fc::fit_to_slots(&s.dual_mic.eq_bands, sig2).is_some());
            if !fits {
                drop(_g);
                self.sync_mic2(true);
            }
        }
    }

    /// Denoiser engine switch: swaps the Tier-2 chain behind the buses with a crossfade.
    pub fn apply_denoiser(&self) -> Result<String, String> {
        let _g = self.ops.lock().unwrap();
        let s = self.snapshot();
        let wanted = s.denoiser.engine.clone();
        if !self.plugins().denoiser_available(&wanted) {
            return Err(format!("{} plugin is not installed (run setup-deps.sh)", wanted));
        }
        let current = self.procs.lock().unwrap().dn_engine.clone();
        if current == wanted {
            drop(_g);
            self.apply_denoiser_params();
            return Ok(wanted);
        }
        if self.swap_mic_chain(&wanted, true) {
            self.relink_all(); // custom mixes tap the mic chain: follow the new generation now
            Ok(wanted)
        } else {
            Err("new noise remover did not start; previous chain kept".into())
        }
    }

    /// Builds a new mic chain generation next to the running one, verifies it (3 s), links it with
    /// zero gain, crossfades ~40 ms, then retires the old one. Returns false (old chain untouched)
    /// if the new chain never appears.
    fn swap_mic_chain(&self, engine: &str, _new_denoiser: bool) -> bool {
        let _swap = self.swap_guard.lock().unwrap();
        let s = self.snapshot();
        let (generation, old_gen) = {
            let mut p = self.procs.lock().unwrap();
            p.gen_counter += 1;
            (p.gen_counter, p.post_gen)
        };
        let key = format!("post-g{}", generation);
        let new_in = fc::post_in(generation);
        let new_out = fc::post_out(generation);
        let old_in = fc::post_in(old_gen);
        let old_out = fc::post_out(old_gen);

        // 1. Start the new chain unlinked, all route gains at zero.
        {
            let mut p = self.procs.lock().unwrap();
            if Self::spawn_conf(&mut p, &key, &format!("mic-g{}", generation), &self.mic_chain_conf(&s, generation, engine, (0.0, 0.0, 0.0, 0.0))).is_err() {
                return false;
            }
        }
        // 2. Verify readiness.
        let names = vec![new_in.clone(), new_out.clone()];
        let found = pw::wait_for_nodes(&names, SWAP_TIMEOUT);
        if found.len() < names.len() {
            let mut p = self.procs.lock().unwrap();
            Self::kill_proc(&mut p, &key);
            eprintln!("Chain swap aborted: {:?} not ready; keeping the running chain", names);
            self.emit(EngineEvent::Notice("Swap failed; previous audio chain kept running.".into()));
            return false;
        }
        // Note: warming the new chain on a private driver was tried and made things worse (period
        // size fights with the live graph); an unlinked chain is not scheduled, so no warm-up here.
        {
            let mut c = self.cache.lock().unwrap();
            c.ids.extend(found);
            for (k, v) in fc::post_route_controls(0.0, 0.0, 0.0, 0.0) {
                c.last.insert(format!("{}|{}", new_in, k), v);
            }
        }

        // 3. Link outputs first, then feed the input (hardware mic or the preview player).
        let p = |n: &str, port: &str| format!("{}:{}", n, port);
        let mut links = Vec::new();
        for (i, k) in ["Voice_Out", "Stream_Mix", "VOD_Mix", "Monitor_Mix"].iter().enumerate() {
            links.push((p(&new_out, &format!("output_AUX{}", i * 2)), p(&fc::bus_sum(k), "input_FL")));
            links.push((p(&new_out, &format!("output_AUX{}", i * 2 + 1)), p(&fc::bus_sum(k), "input_FR")));
        }
        // Whatever feeds the old input (mic, preview, probes) also feeds the new one.
        let old_prefix = format!("{}:", old_in);
        for (o, i) in pw::links() {
            if let Some(port) = i.strip_prefix(&old_prefix) {
                links.push((o, p(&new_in, port)));
            }
        }
        pw::ensure_links(&links);
        // Only fade once the new chain is really running (links negotiated + a few periods of audio).
        if !pw::wait_links_active(&[new_in.clone(), new_out.clone()], Duration::from_millis(1500)) {
            eprintln!("Chain swap: new links not active after 1.5 s; crossfading anyway");
        }
        // Prime: let look-ahead / FFT buffers (DeepFilterNet ~30 ms, limiters) fill with live audio
        // while the new outputs are still at zero gain, so the fade never meets warm-up silence.
        std::thread::sleep(Duration::from_millis(80));

        // 4. Crossfade ~40 ms.
        let (voice, stream, vod, mon) = self.mic_route_gains(&s);
        for step in 1..=4 {
            let t = step as f32 / 4.0;
            self.send(&new_in, &fc::post_route_controls(voice * t, stream * t, vod * t, mon * t), false);
            self.send(&old_in, &fc::post_route_controls(voice * (1.0 - t), stream * (1.0 - t), vod * (1.0 - t), mon * (1.0 - t)), false);
            std::thread::sleep(Duration::from_millis(10));
        }
        self.send(&new_in, &fc::post_dsp_controls(&s.noise_clean, &s.mic, self.plugins().lsp.is_some()), false);
        self.send(&new_in, &fc::eq_controls("eq", &s.eq_bands), false);

        // 5. Retire the old generation.
        pw::unlink_inputs_of(&old_in, &[]);
        pw::unlink_outputs_of(&old_out, &[]);
        // Take the old nodes out of the graph server-side BEFORE stopping the process: a client
        // that is slow to exit (DeepFilterNet threads) would otherwise stall the live cycle.
        Self::detach_nodes(&[old_in.clone(), old_out.clone()]);
        {
            let mut pr = self.procs.lock().unwrap();
            if let Some(newp) = pr.map.remove(&key) {
                if let Some(old) = pr.map.insert("post".into(), newp) {
                    stop_child(old);
                }
            }
            pr.post_gen = generation;
            pr.post_sig = fc::eq_signature(&s.eq_bands);
            pr.dn_engine = engine.to_string();
            Self::save_pids(&pr);
        }
        self.forget_node(&old_in);
        self.forget_node(&old_out);
        self.apply_denoiser_params();
        self.restart_meter("mic");
        println!("Swapped mic chain -> g{} (denoiser: {})", generation, engine);
        true
    }

    /// Destroys nodes server-side (removed at a cycle boundary) and waits briefly for it.
    fn detach_nodes(names: &[String]) {
        pw::invalidate_cache();
        let ids: Vec<u32> = pw::nodes().into_iter().filter(|n| names.contains(&n.name)).map(|n| n.id).collect();
        for id in &ids {
            pw::destroy_node(*id);
        }
        if !ids.is_empty() {
            std::thread::sleep(Duration::from_millis(60));
        }
    }

    /// Adds / removes / renames channel strips to match state. Other channels are untouched.
    pub fn sync_channels(&self) {
        let _g = self.ops.lock().unwrap();
        let s = self.snapshot();
        let solo = Self::solo_active(&s);
        let mut started = Vec::new();
        {
            let mut p = self.procs.lock().unwrap();
            let running: Vec<String> = p.map.keys().filter_map(|k| k.strip_prefix("ch:").map(|x| x.to_string())).collect();
            for id in running {
                if s.channel(&id).is_none() {
                    Self::detach_nodes(&[fc::channel_sink(&id), fc::channel_out(&id)]);
                    Self::kill_proc(&mut p, &format!("ch:{}", id));
                    p.ch_names.remove(&id);
                    self.forget_node(&fc::channel_sink(&id));
                    self.forget_node(&fc::channel_out(&id));
                    println!("Removed channel strip {}", id);
                }
            }
            for ch in &s.virtual_channels {
                let key = format!("ch:{}", ch.id);
                let renamed = p.ch_names.get(&ch.id).map(|n| n != &ch.name).unwrap_or(false);
                if !p.map.contains_key(&key) || renamed {
                    let gains = fc::route_gains(&ch.mix, self.duck_for(ch), solo);
                    if Self::spawn_conf(&mut p, &key, &format!("ch-{}", ch.id), &fc::channel_conf(ch, gains)).is_ok() {
                        p.ch_names.insert(ch.id.clone(), ch.name.clone());
                        self.forget_node(&fc::channel_sink(&ch.id));
                        self.forget_node(&fc::channel_out(&ch.id));
                        started.push(fc::channel_sink(&ch.id));
                        started.push(fc::channel_out(&ch.id));
                    }
                }
            }
        }
        if !started.is_empty() {
            let found = pw::wait_for_nodes(&started, Duration::from_secs(3));
            self.cache.lock().unwrap().ids.extend(found);
        }
        drop(_g);
        self.relink_all();
        self.apply_mixer(false);
    }

    pub fn sync_mic2(&self, force_restart: bool) {
        let _g = self.ops.lock().unwrap();
        let s = self.snapshot();
        {
            let mut p = self.procs.lock().unwrap();
            let running = p.map.contains_key("mic2");
            if !s.dual_mic.enabled {
                if running {
                    Self::detach_nodes(&[MIC2_IN.to_string(), MIC2_OUT.to_string()]);
                    Self::kill_proc(&mut p, "mic2");
                    p.mic2_sig = None;
                    self.forget_node(MIC2_IN);
                    self.forget_node(MIC2_OUT);
                }
                return;
            }
            if running && !force_restart {
                drop(p);
                drop(_g);
                self.relink_all();
                self.apply_eq_params();
                self.apply_mixer(false);
                return;
            }
            let gains = fc::route_gains(&s.dual_mic.mix, 0.0, Self::solo_active(&s));
            let _ = Self::spawn_conf(&mut p, "mic2", "mic2", &fc::mic2_conf(&s.dual_mic.eq_bands, &s.dual_mic.mix, gains));
            p.mic2_sig = Some(fc::eq_signature(&s.dual_mic.eq_bands));
            self.forget_node(MIC2_IN);
            self.forget_node(MIC2_OUT);
        }
        let found = pw::wait_for_nodes(&[MIC2_IN.to_string(), MIC2_OUT.to_string()], Duration::from_secs(3));
        self.cache.lock().unwrap().ids.extend(found);
        drop(_g);
        self.relink_all();
    }

    // -----------------------------------------------------------------------------------------
    // Ducking (stream-only, relative to each channel's Stream Gain)
    // -----------------------------------------------------------------------------------------

    fn ducking_loop(self: Arc<Self>) {
        let mut cur = 0.0f32;
        let mut applied = 0.0f32;
        let mut hold_until = Instant::now();
        let mut last = Instant::now();
        while self.running.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(20));
            let now = Instant::now();
            let dt = now.duration_since(last).as_secs_f32();
            last = now;
            let (any_duck, mic_muted) = {
                let s = self.state.lock().unwrap();
                (s.virtual_channels.iter().any(|c| c.mix.autoduck && !c.is_comms()), s.mic.mix.stream_muted)
            };
            if !any_duck && applied == 0.0 {
                cur = 0.0;
                continue;
            }
            let suspended = mic_muted || self.preview_active.load(Ordering::SeqCst);
            let level = self.peak("mic");
            let target = if !suspended && level > -35.0 { ((level + 35.0) / 1.5).min(12.0) } else { 0.0 };
            if target > 0.0 {
                hold_until = now + Duration::from_millis(400);
            }
            if target > cur {
                cur += (target - cur) * (1.0 - (-dt / 0.050).exp()); // 50 ms attack
            } else if now >= hold_until {
                cur = (cur - 12.0 * dt / 0.8).max(target); // 800 ms release from full depth
            }
            if cur < 0.05 {
                cur = 0.0;
            }
            if (cur - applied).abs() >= 0.25 || (cur == 0.0 && applied != 0.0) {
                *self.duck_db.lock().unwrap() = cur;
                self.apply_mixer(false);
                applied = cur;
            }
        }
    }

    pub fn duck_db(&self) -> f32 {
        *self.duck_db.lock().unwrap()
    }

    // -----------------------------------------------------------------------------------------
    // Metering
    // -----------------------------------------------------------------------------------------

    pub fn peak(&self, key: &str) -> f32 {
        match self.peaks.lock().unwrap().get(key) {
            Some((db, t)) if t.elapsed() < Duration::from_millis(300) => *db,
            _ => -80.0,
        }
    }

    fn desired_meters(&self) -> HashMap<String, (String, Option<String>, u32, u32)> {
        // key -> (source port or "", target node, channels, rate)
        let s = self.snapshot();
        let visible = self.ui_visible.load(Ordering::SeqCst);
        let generation = self.procs.lock().unwrap().post_gen;
        // Meters record at 48 kHz float. The old 8 kHz s16 taps resampled the signal and clamped at
        // full scale, so audio limited to -0.5 dBFS read as 0.0 dB ("clipping" that OBS never saw).
        let mut m = HashMap::new();
        m.insert("mic".to_string(), (format!("{}:output_AUX8", fc::post_out(generation)), None, 1, 48000));
        // The raw meter stays linked for the whole session: spawning a capture client on the
        // hardware mic mid-monitoring (e.g. for the gate test) caused graph renegotiation crackle.
        // Not while the saved mic is unplugged: there is nothing to meter (and see spawn_meter).
        if let Some(hw) = s.active_hw_mic.as_ref().filter(|_| !self.mic_missing.load(Ordering::SeqCst)) {
            m.insert("raw".to_string(), (String::new(), Some(hw.clone()), 2, 48000));
        }
        if visible {
            for ch in &s.virtual_channels {
                m.insert(format!("ch:{}", ch.id), (format!("{}:output_AUX6", fc::channel_out(&ch.id)), None, 1, 48000));
            }
            if s.dual_mic.enabled {
                m.insert("mic2".to_string(), (format!("{}:output_AUX6", MIC2_OUT), None, 1, 48000));
            }
            // What OBS actually receives (after the bus limiters), recorded the same way OBS does.
            for (key, bus) in [("bus:stream", "Stream_Mix"), ("bus:vod", "VOD_Mix"), ("bus:monitor", "Monitor_Mix")] {
                m.insert(key.to_string(), (String::new(), Some(fc::bus_node(bus)), 2, 48000));
            }
        }
        m
    }

    /// `studiomic --diagnose`: the mic, its detected resolution, Clipguard, the mic chain and
    /// every meter (running, linked, last reading), as short lines for a bug report.
    pub fn diagnose(&self) -> String {
        let s = self.snapshot();
        let mut out = vec![format!(
            "Studio Mic {}{}",
            env!("CARGO_PKG_VERSION"),
            option_env!("STUDIOMIC_BUILD").map(|b| format!(" build {b}")).unwrap_or_default()
        )];
        pw::invalidate_cache();
        let nodes = pw::nodes();
        let links = pw::links();
        let node = |name: &str| nodes.iter().find(|n| n.name == name);
        let links_into = |name: &str| links.iter().filter(|(_, i)| i.split_once(':').is_some_and(|(n, _)| n == name)).count();
        let links_from = |name: &str| links.iter().filter(|(o, _)| o.split_once(':').is_some_and(|(n, _)| n == name)).count();
        let yes = |b: bool| if b { "yes" } else { "NO" };

        match &s.active_hw_mic {
            None => out.push("mic: none selected".into()),
            Some(hw) => {
                let n = node(hw);
                out.push(format!("mic: {hw}"));
                out.push(format!("  present: {}  marked missing: {}", yes(n.is_some()), self.mic_missing.load(Ordering::SeqCst)));
                if let Some(n) = n {
                    let f = &n.params["Format"][0];
                    out.push(format!(
                        "  PipeWire format: {} {} Hz {} ch, class {}",
                        f["format"].as_str().unwrap_or("?"),
                        f["rate"].as_u64().unwrap_or(0),
                        f["channels"].as_u64().unwrap_or(0),
                        n.class
                    ));
                    if let Some((v, _)) = pw::node_volume(n) {
                        out.push(format!("  input volume: {:.1} dB", lin_to_db(v)));
                    }
                }
            }
        }
        out.push(format!(
            "  resolution: {}",
            match self.mic_bits.load(Ordering::SeqCst) {
                16 => "16-bit (strict clip check)",
                24 => "24-bit or float",
                _ => "detecting (needs about 1 s of sound)",
            }
        ));
        out.push(format!("  clipguard: {}  auto-pad: -{:.0} dB", if s.mic.clipguard { "on" } else { "off" }, self.autopad_db()));

        let generation = self.procs.lock().unwrap().post_gen;
        let (pin, pout) = (fc::post_in(generation), fc::post_out(generation));
        out.push(format!(
            "mic chain g{generation}: in {} ({} links in), out {} ({} links out)",
            yes(node(&pin).is_some()),
            links_into(&pin),
            yes(node(&pout).is_some()),
            links_from(&pout)
        ));
        let p = self.plugins_json();
        out.push(format!(
            "plugins: lsp {} deepfilter {} rnnoise {} speex {}  denoiser: {}",
            p["lsp"], p["deepfilter"], p["rnnoise"], p["speex"], s.denoiser.engine
        ));

        out.push("meters (key: running, links in, last reading):".into());
        let peaks = self.peaks.lock().unwrap().clone();
        let mut meters: Vec<(String, String, bool)> = {
            let mut m = self.meters.lock().unwrap();
            m.iter_mut().map(|(k, mp)| (k.clone(), mp.source.clone(), matches!(mp.child.try_wait(), Ok(None)))).collect()
        };
        meters.sort();
        let wanted = self.desired_meters();
        for k in wanted.keys().filter(|k| !meters.iter().any(|(m, ..)| m == *k)) {
            out.push(format!("  {k}: NOT RUNNING"));
        }
        for (k, src, alive) in meters {
            let meter_node = format!("StudioMic_Meter_{}", k.replace(':', "_"));
            let last = peaks
                .get(&k)
                .map(|(db, t)| format!("{:.1} dB, {:.1} s ago", db, t.elapsed().as_secs_f32()))
                .unwrap_or_else(|| "never".into());
            out.push(format!("  {k}: {}, {} links, {last}  [{src}]", if alive { "running" } else { "EXITED" }, links_into(&meter_node)));
        }
        if let Some((_, t)) = peaks.get("raw_clip") {
            out.push(format!("last real clipping on the mic input: {:.1} s ago", t.elapsed().as_secs_f32()));
        } else {
            out.push("no real clipping seen on the mic input".into());
        }
        out.join("\n")
    }

    fn restart_meter(&self, key: &str) {
        if let Some(m) = self.meters.lock().unwrap().remove(key) {
            stop_child(m.child);
        }
    }

    fn meter_manager_loop(self: Arc<Self>) {
        while self.running.load(Ordering::SeqCst) {
            let desired = self.desired_meters();
            let mut meters = self.meters.lock().unwrap();
            // Stop meters that are no longer wanted, changed source, or died.
            let keys: Vec<String> = meters.keys().cloned().collect();
            for k in keys {
                let dead = meters.get_mut(&k).map(|m| !matches!(m.child.try_wait(), Ok(None))).unwrap_or(true);
                let src_changed = desired.get(&k).map(|(src, tgt, ..)| {
                    let want = if src.is_empty() { tgt.clone().unwrap_or_default() } else { src.clone() };
                    meters.get(&k).map(|m| m.source != want).unwrap_or(true)
                });
                if dead || src_changed != Some(false) {
                    if let Some(m) = meters.remove(&k) {
                        stop_child(m.child);
                    }
                }
            }
            for (key, (src, target, channels, rate)) in desired {
                if meters.contains_key(&key) {
                    continue;
                }
                if let Some(mp) = self.spawn_meter(&key, &src, target.as_deref(), channels, rate) {
                    meters.insert(key, mp);
                }
            }
            drop(meters);
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn spawn_meter(self: &Arc<Self>, key: &str, src_port: &str, target: Option<&str>, channels: u32, rate: u32) -> Option<MeterProc> {
        let node_name = format!("StudioMic_Meter_{}", key.replace(':', "_"));
        let mut cmd = Command::new("pw-record");
        cmd.args(meter_format_args(rate, channels));
        if let Some(t) = target {
            if pw::find_node(t).is_none() {
                return None;
            }
            // Without dont-fallback a meter whose device vanished is silently moved to the
            // default source (measured: a missing mic's meter recorded the default mic).
            let props = if key == "bus:monitor" {
                // Monitor_Mix is a sink: record its monitor ports (post master volume / mute).
                // WirePlumber never links a capture-of-sink stream that has dont-fallback or
                // dont-reconnect (measured), and the bus is a lingering node checked just above.
                format!("{{ node.name = {} stream.capture.sink = true }}", node_name)
            } else {
                format!("{{ node.name = {} node.dont-fallback = true node.dont-reconnect = true }}", node_name)
            };
            cmd.args(["--target", t, "-P", &props]);
        } else {
            if src_port.is_empty() {
                return None;
            }
            // Port must exist before we can link the tap.
            if !pw::wait_for_port(src_port, Duration::from_millis(50)) {
                return None;
            }
            cmd.args(["-P", &format!("{{ node.name = {} node.autoconnect = false }}", node_name)]);
        }
        cmd.arg("-").stdout(Stdio::piped()).stderr(Stdio::null()).stdin(Stdio::null());
        let mut child = cmd.spawn().ok()?;
        let stdout = child.stdout.take()?;
        if target.is_none() {
            let in_port = format!("{}:input_MONO", node_name);
            if pw::wait_for_port(&in_port, Duration::from_millis(1500)) {
                pw::link(src_port, &in_port);
            }
        }
        let engine = self.clone();
        let k = key.to_string();
        let ch = channels as usize;
        let block = (rate as usize / 40).max(64) * ch * 4; // ~25 ms of f32 audio
        let _ = std::thread::Builder::new().name(format!("meter-{}", key)).spawn(move || {
            let mut stdout = stdout;
            let mut buf = vec![0u8; block];
            let detect_clip = k == "raw";
            // A new meter (mic switched, PipeWire restarted) starts detection over.
            let mut depth = BitDepth::default();
            if detect_clip {
                engine.mic_bits.store(0, Ordering::SeqCst);
            }
            while stdout.read_exact(&mut buf).is_ok() {
                if detect_clip {
                    let known = depth.bits();
                    depth.observe(&buf, ch);
                    if known.is_none() {
                        if let Some(bits) = depth.bits() {
                            engine.mic_bits.store(bits, Ordering::SeqCst);
                            println!("Microphone resolution: {}", if bits == 16 { "16-bit" } else { "24-bit or float" });
                        }
                    }
                    if has_flat_top(&buf, ch, depth.clip_rule()) {
                        engine.peaks.lock().unwrap().insert("raw_clip".into(), (1.0, Instant::now()));
                        // Only real clipping lowers the hardware gain (see clipguard_autopad).
                        if !engine.measuring.load(Ordering::SeqCst) {
                            engine.clipguard_autopad();
                        }
                    }
                }
                engine.on_peak(&k, block_peak_db(&buf));
            }
        });
        Some(MeterProc { source: target.map(|t| t.to_string()).unwrap_or_else(|| src_port.to_string()), child })
    }

    fn on_peak(&self, key: &str, db: f32) {
        self.peaks.lock().unwrap().insert(key.to_string(), (db, Instant::now()));
        if self.measuring.load(Ordering::SeqCst) {
            let mut h = self.peak_hold.lock().unwrap();
            let e = h.entry(key.to_string()).or_insert(-80.0);
            if db > *e {
                *e = db;
            }
        }
    }

    fn peak_broadcast_loop(self: Arc<Self>) {
        let mut last_json = String::new();
        while self.running.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(50));
            if !self.ui_visible.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(250));
                continue;
            }
            let map = self.peak_map();
            let json = serde_json::to_string(&map).unwrap_or_default();
            // Skip redundant pushes while silent (saves webview CPU).
            if json != last_json {
                self.emit(EngineEvent::Peaks(json.clone()));
                last_json = json;
            }
        }
    }

    /// Linear Stream-bus gain of every strip (mic, mic2, channels): fader, mute, solo-free, ducking.
    fn stream_send_gains(&self) -> Vec<(String, f32)> {
        let s = self.state.lock().unwrap();
        let solo = Self::solo_active(&s);
        let mut v = vec![("mic".to_string(), self.mic_route_gains(&s).1)];
        if s.dual_mic.enabled {
            v.push(("mic2".to_string(), fc::route_gains(&s.dual_mic.mix, 0.0, solo).0));
        }
        for ch in &s.virtual_channels {
            v.push((ch.id.clone(), fc::route_gains(&ch.mix, self.duck_for(ch), solo).0));
        }
        v
    }

    /// Everything the meters show, keyed for the UI:
    ///   `<strip>`      level entering the strip (pre-fader; the mic is after Clipguard)
    ///   `out:<strip>`  what that strip sends to the Stream bus (post-fader, before the bus limiter)
    ///   `raw`          hardware mic input level
    ///   `raw_clip`     1 while the hardware input is really clipping (flat tops), else -80
    ///   `bus:stream`, `bus:vod`  the buses OBS records (after their limiters); `bus:monitor` the headphone mix
    ///   `pad`          Clipguard hardware auto-pad in dB (0 = none)
    pub fn peak_map(&self) -> BTreeMap<String, f32> {
        let round = |v: f32| (v * 10.0).round() / 10.0;
        let gains = self.stream_send_gains();
        let mut map = BTreeMap::new();
        {
            let peaks = self.peaks.lock().unwrap();
            for (k, (db, t)) in peaks.iter() {
                let v = if t.elapsed() < Duration::from_millis(300) { *db } else { -80.0 };
                map.insert(k.strip_prefix("ch:").unwrap_or(k).to_string(), round(v));
            }
        }
        for (id, g) in gains {
            if let Some(&pre) = map.get(&id) {
                let out = if g > 0.0 && pre > METER_FLOOR_DB { (pre + lin_to_db(g)).max(METER_FLOOR_DB) } else { METER_FLOOR_DB };
                map.insert(format!("out:{}", id), round(out));
            }
        }
        map.insert("duck".into(), round(self.duck_db()));
        // Clipguard hardware auto-pad currently applied (0 = none), for the "PAD -N dB" badge.
        map.insert("pad".into(), round(self.autopad_db()));
        map
    }

    /// Records the loudest raw peak for `secs` (gate auto-set keyboard test).
    pub fn measure_raw_peak(self: &Arc<Self>, secs: f32) {
        let e = self.clone();
        std::thread::spawn(move || {
            e.peak_hold.lock().unwrap().clear();
            e.measuring.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_secs_f32(secs + 0.6));
            e.measuring.store(false, Ordering::SeqCst);
            // No reading (mic unplugged / no raw meter) must not become a "measured" -80 dB that
            // silently sets the gate to its lowest threshold.
            let raw = e.peak_hold.lock().unwrap().get("raw").copied().filter(|db| *db > METER_FLOOR_DB);
            let trim = { let s = e.state.lock().unwrap(); e.trim_db(&s) };
            e.emit(EngineEvent::GateMeasured(raw.map(|r| r + trim)));
        });
    }

    // -----------------------------------------------------------------------------------------
    // Clipguard stage B: hardware auto-pad with a -6 dB floor
    // -----------------------------------------------------------------------------------------

    /// Called when the raw mic input really clipped (flat tops, `has_flat_top`), never on level
    /// alone: interfaces with lots of gain or their own limiter (Elgato Wave XLR) produce clean
    /// peaks right at 0 dBFS, which used to step the gain down and announce "input clipping"
    /// although nothing distorted. Not while the gate test measures (the user is making noise).
    fn clipguard_autopad(&self) {
        let (enabled, hw) = {
            let s = self.state.lock().unwrap();
            (s.mic.clipguard, s.active_hw_mic.clone())
        };
        if !enabled {
            return;
        }
        let Some(hw) = hw else { return };
        let mut pad = self.autopad.lock().unwrap();
        if pad.unsupported || pad.pad_db >= 6.0 || pad.last_step.elapsed() < Duration::from_millis(400) {
            return;
        }
        if pad.pad_db > 0.0 {
            pw::invalidate_cache();
        }
        let node = match pw::find_node(&hw) {
            Some(n) => n,
            None => return,
        };
        // Before stepping again, check the last hardware step really happened. Some USB mics and
        // webcams accept the set-param but keep their level; the digital makeup would then be
        // pure extra gain driving the chain harder (the "always clipping" fresh-install bug).
        if let (Some((_, orig_vol, chans)), true) = (pad.orig, pad.pad_db > 0.0) {
            let expected = orig_vol * db_to_lin(-pad.pad_db);
            let applied = pw::node_volume(&node).map(|(v, _)| (lin_to_db(v) - lin_to_db(expected)).abs() < 0.5).unwrap_or(false);
            if !applied {
                PARAMS.set_volume(node.id, chans, orig_vol, false);
                pad.orig = None;
                pad.pad_db = 0.0;
                pad.unsupported = true;
                drop(pad);
                println!("Clipguard auto-pad: device ignored the volume step; auto-pad disabled for this input");
                self.apply_pre();
                self.emit(EngineEvent::Notice(
                    "Clipguard: this microphone's input level can't be lowered from here. Lower its gain on the device or in the Calibration tab.".into(),
                ));
                return;
            }
        }
        if pad.orig.is_none() {
            let Some((vol, _)) = pw::node_volume(&node) else {
                // Hardware node does not expose software volume properties; cannot safely pad.
                return;
            };
            let chans = node.props.get("audio.channels").and_then(|v| v.as_u64()).unwrap_or(2) as usize;
            pad.orig = Some((node.id, vol, chans));
        }
        let (_, orig_vol, chans) = pad.orig.unwrap();
        pad.pad_db = (pad.pad_db + 1.0).min(6.0);
        pad.last_step = Instant::now();
        let new_vol = orig_vol * db_to_lin(-pad.pad_db);
        PARAMS.set_volume(node.id, chans, new_vol, false);
        let pad_db = pad.pad_db;
        drop(pad);
        println!("Clipguard auto-pad: hardware -{:.0} dB, digital makeup +{:.0} dB", pad_db, pad_db);
        self.apply_pre();
        self.emit(EngineEvent::Notice(format!("Clipguard: input clipping, hardware gain lowered {:.0} dB", pad_db)));
    }

    /// Restores the hardware capture level (Clipguard OFF / quit).
    pub fn restore_autopad(&self) {
        let mut pad = self.autopad.lock().unwrap();
        if let Some((id, vol, chans)) = pad.orig.take() {
            PARAMS.set_volume(id, chans, vol, false);
        }
        let had = pad.pad_db > 0.0;
        pad.pad_db = 0.0;
        pad.unsupported = false;
        drop(pad);
        if had {
            self.apply_pre();
        }
    }

    pub fn autopad_db(&self) -> f32 {
        self.autopad.lock().unwrap().pad_db
    }

    // -----------------------------------------------------------------------------------------
    // Voice test recorder & isolated live preview
    // -----------------------------------------------------------------------------------------

    pub fn record_start(self: &Arc<Self>) -> Result<(), String> {
        let hw = self.state.lock().unwrap().active_hw_mic.clone().ok_or("No microphone selected")?;
        if self.preview_active.load(Ordering::SeqCst) {
            self.preview_stop();
        }
        if let Some(c) = self.recorder.lock().unwrap().take() {
            stop_child(c);
        }
        let _ = std::fs::remove_file(PREVIEW_WAV);
        // Dry (unprocessed) capture at the graph rate.
        let child = Command::new("pw-record")
            .args(["--target", &hw, "--rate", "48000", "--format", "f32", "--channels", "1", "-P", "{ node.name = StudioMic_Recorder }", PREVIEW_WAV])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("pw-record failed: {}", e))?;
        let pid = child.id();
        *self.recorder.lock().unwrap() = Some(child);
        *self.last_preview_use.lock().unwrap() = Instant::now();
        let e = self.clone();
        std::thread::spawn(move || {
            for sec in 0..=RECORD_MAX_SECS {
                let still = e.recorder.lock().unwrap().as_ref().map(|c| c.id() == pid).unwrap_or(false);
                if !still {
                    return;
                }
                e.emit(EngineEvent::Recorder(format!("{{\"state\":\"recording\",\"elapsed\":{},\"max\":{}}}", sec, RECORD_MAX_SECS)));
                if sec == RECORD_MAX_SECS {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            e.record_stop();
        });
        Ok(())
    }

    pub fn record_stop(&self) {
        if let Some(mut c) = self.recorder.lock().unwrap().take() {
            signal(c.id(), "-INT"); // SIGINT lets pw-record finalise the WAV header
            let start = Instant::now();
            while start.elapsed() < Duration::from_millis(1500) {
                if let Ok(Some(_)) = c.try_wait() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
            let _ = c.kill();
            let _ = c.wait();
            *self.last_preview_use.lock().unwrap() = Instant::now();
            let ok = std::path::Path::new(PREVIEW_WAV).exists();
            self.emit(EngineEvent::Recorder(format!("{{\"state\":\"{}\"}}", if ok { "done" } else { "failed" })));
        }
    }

    pub fn preview_available(&self) -> bool {
        std::path::Path::new(PREVIEW_WAV).exists()
    }

    pub fn preview_is_active(&self) -> bool {
        self.preview_active.load(Ordering::SeqCst)
    }

    pub fn recording_is_active(&self) -> bool {
        self.recorder.lock().unwrap().is_some()
    }

    /// Plays the dry recording through the LIVE filter chain to the Monitor mix only.
    pub fn preview_start(self: &Arc<Self>, looped: bool) -> Result<(), String> {
        if !self.preview_available() {
            return Err("No test recording yet: record a phrase on Tab 4 first".into());
        }
        self.preview_loop.store(looped, Ordering::SeqCst);
        if self.preview_active.swap(true, Ordering::SeqCst) {
            return Ok(()); // already playing; loop flag updated
        }
        *self.last_preview_use.lock().unwrap() = Instant::now();
        // Isolate: mute Voice/Stream/VOD for the mic, force monitor on, unlink the hardware mic.
        self.apply_mixer(true);
        let mic_in = fc::post_in(self.procs.lock().unwrap().post_gen);
        pw::unlink_inputs_of(&mic_in, &[]);
        self.emit(EngineEvent::Preview("{\"state\":\"playing\"}".into()));
        let e = self.clone();
        std::thread::spawn(move || {
            while e.preview_active.load(Ordering::SeqCst) {
                let child = Command::new("pw-play")
                    .args(["-P", "{ node.name = StudioMic_Preview node.autoconnect = false }", PREVIEW_WAV])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
                let Ok(child) = child else { break };
                let pid = child.id();
                *e.preview_child.lock().unwrap() = Some(child);
                for port in ["StudioMic_Preview:output_MONO", "StudioMic_Preview:output_FL"] {
                    if pw::wait_for_port(port, Duration::from_millis(1500)) {
                        let mic_in = fc::post_in(e.procs.lock().unwrap().post_gen);
                        pw::link(port, &format!("{}:input_FL", mic_in));
                        pw::link(port, &format!("{}:input_FR", mic_in));
                        break;
                    }
                }
                // Wait for playback to end.
                loop {
                    std::thread::sleep(Duration::from_millis(100));
                    let mut guard = e.preview_child.lock().unwrap();
                    match guard.as_mut() {
                        Some(c) if c.id() == pid => {
                            if let Ok(Some(_)) = c.try_wait() {
                                guard.take();
                                break;
                            }
                        }
                        _ => break,
                    }
                }
                *e.last_preview_use.lock().unwrap() = Instant::now();
                if !e.preview_loop.load(Ordering::SeqCst) {
                    break;
                }
            }
            if e.preview_active.load(Ordering::SeqCst) {
                e.preview_stop();
            }
        });
        Ok(())
    }

    pub fn preview_stop(&self) {
        self.preview_stop_internal(true);
    }

    fn preview_stop_internal(&self, restore: bool) {
        let was = self.preview_active.swap(false, Ordering::SeqCst);
        if let Some(c) = self.preview_child.lock().unwrap().take() {
            stop_child(c);
        }
        if was && restore {
            self.relink_all();
            self.apply_mixer(true);
            self.emit(EngineEvent::Preview("{\"state\":\"stopped\"}".into()));
        }
    }

    fn preview_expiry_loop(self: Arc<Self>) {
        while self.running.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_secs(20));
            let idle = self.last_preview_use.lock().unwrap().elapsed();
            if idle > PREVIEW_EXPIRY && self.preview_available() && !self.preview_is_active() && !self.recording_is_active() {
                let _ = std::fs::remove_file(PREVIEW_WAV);
                self.emit(EngineEvent::Recorder("{\"state\":\"expired\"}".into()));
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Maintenance: keep buses, processes and links healthy; hot-plug; app auto-routing
    // -----------------------------------------------------------------------------------------

    /// The node that proves a stage is connected to the graph.
    fn sentinel_node(procs: &Procs, key: &str) -> Option<String> {
        match key {
            "buses" => Some(fc::bus_sum("Voice_Out")),
            "post" => Some(fc::post_in(procs.post_gen)),
            "mic2" => Some(MIC2_IN.to_string()),
            k if k.starts_with("send:") => Some(fc::send_in(k.trim_start_matches("send:"))),
            k => k.strip_prefix("ch:").map(fc::channel_sink),
        }
    }

    fn maintenance_loop(self: Arc<Self>) {
        let mut tick = 0u64;
        let mut missing_streak: HashMap<String, u32> = HashMap::new();
        while self.running.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(2000));
            if !self.running.load(Ordering::SeqCst) {
                break;
            }
            tick += 1;
            // A stage is "crashed" if its process exited OR its node has been missing from the
            // graph for two passes (after a PipeWire restart the child stays alive but disconnected).
            let present: std::collections::HashSet<String> = pw::nodes().into_iter().map(|n| n.name).collect();
            let crashed: Vec<String> = {
                let _ops = self.ops.lock().unwrap(); // never judge a stage mid-swap
                let mut p = self.procs.lock().unwrap();
                let keys: Vec<String> = p.map.keys().cloned().collect();
                let mut out = Vec::new();
                for k in keys {
                    let exited = p.map.get_mut(&k).map(|c| !matches!(c.try_wait(), Ok(None))).unwrap_or(true);
                    let gone = Self::sentinel_node(&p, &k).map(|n| !present.contains(&n)).unwrap_or(false);
                    let streak = missing_streak.entry(k.clone()).or_insert(0);
                    *streak = if gone { *streak + 1 } else { 0 };
                    if exited || *streak >= 2 {
                        if !exited {
                            if let Some(c) = p.map.remove(&k) {
                                stop_child(c); // disconnected zombie: replace it
                            }
                        }
                        missing_streak.remove(&k);
                        out.push(k);
                    }
                }
                out
            };
            let server_restart = crashed.len() > 1 || crashed.iter().any(|k| k == "buses");
            let mut crashed = crashed;
            if server_restart {
                // Every client connection died with the server: rebuild ALL stages, not only the
                // ones that happened to be noticed first.
                let mut p = self.procs.lock().unwrap();
                let rest: Vec<String> = p.map.keys().filter(|k| !crashed.contains(k)).cloned().collect();
                for k in rest {
                    if let Some(c) = p.map.remove(&k) {
                        stop_child(c);
                    }
                    crashed.push(k);
                }
                drop(p);
                missing_streak.clear();
                // Looks like PipeWire itself restarted: reconnect the parameter writer too.
                eprintln!("PipeWire restart detected; rebuilding Studio Mic stages {:?}", crashed);
                PARAMS.shutdown();
                self.ensure_buses();
            }
            if !crashed.is_empty() {
                eprintln!("Engine stages exited unexpectedly: {:?}; rebuilding them", crashed);
                if crashed.iter().any(|k| k.starts_with("ch:")) {
                    let mut p = self.procs.lock().unwrap();
                    for k in crashed.iter().filter(|k| k.starts_with("ch:")) {
                        p.map.remove(k);
                    }
                    drop(p);
                    self.sync_channels();
                }
                if crashed.iter().any(|k| k.starts_with("send:")) {
                    let mut p = self.procs.lock().unwrap();
                    for k in crashed.iter().filter(|k| k.starts_with("send:")) {
                        p.map.remove(k);
                        p.send_sigs.remove(k);
                    }
                    drop(p);
                    self.sync_sends();
                }
                if crashed.iter().any(|k| !k.starts_with("ch:") && !k.starts_with("send:")) {
                    let _g = self.ops.lock().unwrap();
                    let mut p = self.procs.lock().unwrap();
                    for k in crashed.iter().filter(|k| !k.starts_with("ch:") && !k.starts_with("send:")) {
                        p.map.remove(k);
                    }
                    let s = self.snapshot();
                    if !p.map.contains_key("buses") {
                        let _ = Self::spawn_conf(&mut p, "buses", "buses", &fc::bus_master_conf(&self.plugins()));
                    }
                    if !p.map.contains_key("post") {
                        // After a server restart, rebuild exactly as configured. If the chain itself
                        // crashed, never retry the denoiser in a loop: recover the voice without it.
                        if !server_restart && p.dn_engine != "off" {
                            p.dn_engine = "off".into();
                            self.emit(EngineEvent::Notice("Mic chain crashed; restarted without the noise remover.".into()));
                        }
                        let engine = if server_restart { self.effective_denoiser(&s) } else { p.dn_engine.clone() };
                        p.dn_engine = engine.clone();
                        let generation = p.post_gen;
                        let routes = self.mic_route_gains(&s);
                        let _ = Self::spawn_conf(&mut p, "post", &format!("mic-g{}", generation), &self.mic_chain_conf(&s, generation, &engine, routes));
                        // The rebuilt chain uses the saved band order as its slot layout.
                        p.post_sig = fc::eq_signature(&s.eq_bands);
                    }
                    if s.dual_mic.enabled && !p.map.contains_key("mic2") {
                        let gains = fc::route_gains(&s.dual_mic.mix, 0.0, Self::solo_active(&s));
                        let _ = Self::spawn_conf(&mut p, "mic2", "mic2", &fc::mic2_conf(&s.dual_mic.eq_bands, &s.dual_mic.mix, gains));
                        p.mic2_sig = Some(fc::eq_signature(&s.dual_mic.eq_bands));
                    }
                    let expected = self.expected_nodes(&p, &s);
                    drop(p);
                    let mut c = self.cache.lock().unwrap();
                    c.ids.clear();
                    c.last.clear();
                    drop(c);
                    let found = pw::wait_for_nodes(&expected, Duration::from_secs(4));
                    self.cache.lock().unwrap().ids = found;
                    self.apply_all_locked();
                }
            }
            if tick % 2 == 0 {
                self.ensure_buses();
            }
            self.relink_all();
            let channels = self.state.lock().unwrap().virtual_channels.clone();
            crate::routing::wireplumber_hook::StreamWatcher::auto_route(&channels);
        }
    }

    /// Two-way sync of the Master Monitor Gain with external volume changes (pavucontrol, keys).
    fn monitor_sync_loop(self: Arc<Self>) {
        while self.running.load(Ordering::SeqCst) {
            let child = Command::new("pactl").arg("subscribe").stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
            let Ok(mut child) = child else {
                std::thread::sleep(Duration::from_secs(10));
                continue;
            };
            let Some(stdout) = child.stdout.take() else { break };
            let reader = std::io::BufReader::new(stdout);
            use std::io::BufRead;
            for line in reader.lines().map_while(Result::ok) {
                if !self.running.load(Ordering::SeqCst) {
                    break;
                }
                if !(line.contains("'change' on sink #") || line.contains("'change' on server")) {
                    continue;
                }
                if self.monitor_set_at.lock().unwrap().elapsed() < Duration::from_millis(1500) {
                    continue; // our own change echoing back
                }
                pw::invalidate_cache(); // must see the volume that just changed
                let Some(node) = pw::find_node(&fc::bus_node("Monitor_Mix")) else { continue };
                let Some((vol, mute)) = pw::node_volume(&node) else { continue };
                let db = lin_to_db(vol);
                let changed = {
                    let mut s = self.state.lock().unwrap();
                    if (s.monitor_gain_db - db).abs() > 0.2 || s.monitor_muted != mute {
                        s.monitor_gain_db = crate::config::clamp_gain(db);
                        s.monitor_muted = mute;
                        let _ = s.save();
                        true
                    } else {
                        false
                    }
                };
                if changed {
                    self.emit(EngineEvent::MonitorMaster);
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    // -----------------------------------------------------------------------------------------
    // Runtime status
    // -----------------------------------------------------------------------------------------

    pub fn plugins(&self) -> Plugins {
        self.plugins.read().unwrap().clone()
    }

    /// `{lsp, deepfilter, rnnoise, speex}` availability, as the UI's `window.__PLUGINS__`.
    pub fn plugins_json(&self) -> serde_json::Value {
        let p = self.plugins();
        serde_json::json!({
            "lsp": p.lsp.is_some(),
            "deepfilter": p.deepfilter.is_some(),
            "rnnoise": p.rnnoise.is_some(),
            "speex": p.speex.is_some(),
        })
    }

    /// Looks for LADSPA plugins again (after a download). Running stages keep their plugins;
    /// newly found denoisers become selectable immediately.
    pub fn rescan_plugins(&self) -> Plugins {
        let p = Plugins::discover();
        *self.plugins.write().unwrap() = p.clone();
        p
    }

    /// Downloads the pinned DeepFilterNet plugin into ~/.local/lib/ladspa in the background,
    /// verifies its SHA256, then re-scans so it can be selected straight away.
    pub fn download_deepfilter(self: &Arc<Self>) -> Result<String, String> {
        if self.plugins().deepfilter.is_some() {
            return Ok("DeepFilterNet is already installed".into());
        }
        if std::env::consts::ARCH != "x86_64" {
            return Err("The DeepFilterNet download is x86_64 only; install it from your distro".into());
        }
        if self.downloading.swap(true, Ordering::SeqCst) {
            return Ok("DeepFilterNet download already running".into());
        }
        let e = self.clone();
        std::thread::spawn(move || {
            let res = fetch_verified(&df_url(), DF_SHA256, &crate::audio::plugins_bundle::user_ladspa_dir(), "libdeep_filter_ladspa.so");
            e.downloading.store(false, Ordering::SeqCst);
            match res {
                Ok(path) => {
                    println!("DeepFilterNet installed: {}", path.display());
                    e.rescan_plugins();
                    e.emit(EngineEvent::PluginsChanged(e.plugins_json().to_string()));
                    e.emit(EngineEvent::Notice("DeepFilterNet installed. It can be selected now.".into()));
                }
                Err(err) => e.emit(EngineEvent::Notice(format!("DeepFilterNet download failed: {}", err))),
            }
        });
        Ok("Downloading DeepFilterNet (about 54 MB)…".into())
    }

    pub fn runtime_status(&self) -> serde_json::Value {
        let p = self.procs.lock().unwrap();
        serde_json::json!({
            "denoiser_active": p.dn_engine,
            "post_generation": p.post_gen,
            "mic_input_node": fc::post_in(p.post_gen),
            "mic_output_node": fc::post_out(p.post_gen),
            "plugins": self.plugins_json(),
            "preview_available": self.preview_available(),
            "mic_missing": self.mic_missing.load(Ordering::SeqCst),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{block_peak_db, has_flat_top, is_our_child, meter_format_args, BitDepth, ClipRule, Engine, SendTarget};
    use crate::config::{CustomMixConfig, SendCell, StudioMicConfig};

    #[test]
    fn preview_silences_the_mic_in_custom_mixes() {
        let mut s = StudioMicConfig::default();
        s.custom_mixes.push(CustomMixConfig { id: "guest".into(), name: "Guest".into(), icon: "alt_route".into(), color: "#fff".into(), output_device: None });
        s.mic.mix.sends.insert("guest".into(), SendCell { gain_db: 0.0, muted: false });
        let t = SendTarget::Custom { id: "guest".into(), name: "Guest".into() };
        assert_eq!(Engine::send_gain(&s, &t, "mic", false), 1.0);
        assert_eq!(Engine::send_gain(&s, &t, "mic", true), 0.0, "preview must not leak into a mix");
    }

    #[test]
    fn meters_read_raw_native_samples() {
        // Without --raw, Fedora's pw-record pipes a big-endian AU file and every meter misreads.
        let a = meter_format_args(48000, 2);
        assert_eq!(a[0], "--raw");
        assert!(a.windows(2).any(|w| w[0] == "--format" && w[1] == "f32"));
    }

    fn bytes(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    fn sine(f: f32, amp: f32, n: usize, phase: f32) -> Vec<f32> {
        (0..n).map(|i| amp * (2.0 * std::f32::consts::PI * f * i as f32 / 48000.0 + phase).sin()).collect()
    }

    fn q16(v: &[f32]) -> Vec<f32> {
        v.iter().map(|x| (x * 32767.0).round() / 32767.0).collect()
    }

    #[test]
    fn clip_detector_needs_flat_tops() {
        for rule in [ClipRule::FINE, ClipRule::COARSE] {
            // Clean, loud sines quantised like a 16-bit USB mic: never "clipping".
            for f in [50.0f32, 100.0, 1000.0] {
                assert!(!has_flat_top(&bytes(&q16(&sine(f, 0.99, 1200, 0.0))), 1, rule), "{f} Hz sine flagged");
            }
            // Software gain past 0 dBFS (float, not railed): not clipping either.
            assert!(!has_flat_top(&bytes(&sine(200.0, 1.6, 1200, 0.0)), 1, rule));
            // A railed converter: clipped (also at 16-bit and with a voice-pitch tone).
            let railed: Vec<f32> = sine(200.0, 1.6, 1200, 0.0).iter().map(|x| x.clamp(-1.0, 1.0)).collect();
            assert!(has_flat_top(&bytes(&railed), 1, rule));
            let voice: Vec<f32> = q16(&sine(110.0, 1.4, 1200, 0.0).iter().map(|x| x.clamp(-1.0, 1.0)).collect::<Vec<_>>());
            assert!(has_flat_top(&bytes(&voice), 1, rule), "clipped 16-bit voice missed");
            // Stereo interleaved: the clipped right channel is found.
            let st: Vec<f32> = railed.iter().flat_map(|&r| [0.1, r]).collect();
            assert!(has_flat_top(&bytes(&st), 2, rule));
        }
    }

    #[test]
    fn sixteen_bit_rumble_is_not_clipping() {
        // A loud 10-40 Hz crest (desk bump, plosive) on a 16-bit converter: neighbours round to
        // the same value. The fine rule calls that clipping; the coarse rule doesn't.
        let mut fooled_fine = false;
        for f in [10.0f32, 20.0, 30.0, 40.0] {
            for amp in [0.55f32, 0.7, 0.9] {
                for phase in [0.0f32, 0.7, 1.4, 2.1] {
                    let b = bytes(&q16(&sine(f, amp, 1200, phase)));
                    assert!(!has_flat_top(&b, 1, ClipRule::COARSE), "{f} Hz {amp} flagged as clipping");
                    fooled_fine |= has_flat_top(&b, 1, ClipRule::FINE);
                }
            }
        }
        assert!(fooled_fine, "the fine rule should be what 16-bit mics trip over");
    }

    #[test]
    fn mic_resolution_is_detected_from_the_samples() {
        // ~1.2 s of voice-like sound plus room noise, as 25 ms blocks.
        let noise = |i: usize| ((i as f32 * 12.9898).sin() * 43758.547).fract() * 2e-4;
        let signal: Vec<f32> = (0..48000 * 6 / 5).map(|i| 0.2 * (2.0 * std::f32::consts::PI * 180.0 * i as f32 / 48000.0).sin() + noise(i)).collect();
        let run = |v: &[f32]| {
            let mut d = BitDepth::default();
            for block in v.chunks(1200) {
                d.observe(&bytes(block), 1);
            }
            d
        };
        let d16 = run(&q16(&signal));
        assert_eq!(d16.bits(), Some(16));
        assert_eq!(d16.clip_rule(), ClipRule::COARSE);
        // A 16-bit mic behind -12 dB of volume is still recognised.
        let scaled: Vec<f32> = q16(&signal).iter().map(|x| x * 0.25).collect();
        assert_eq!(run(&scaled).bits(), Some(16));
        let d24 = run(&signal);
        assert_eq!(d24.bits(), Some(24));
        assert_eq!(d24.clip_rule(), ClipRule::FINE);
        // Not enough sound yet (or only silence): unknown, so the careful rule applies.
        let short = run(&signal[..12000]);
        assert_eq!(short.bits(), None);
        assert_eq!(short.clip_rule(), ClipRule::COARSE);
        assert_eq!(run(&vec![0.0; 96000]).bits(), None);
    }

    #[test]
    fn deepfilter_pin_matches_setup_deps() {
        let script = include_str!("../../setup-deps.sh");
        assert!(script.contains(&format!("DF_VERSION=\"{}\"", super::DF_VERSION)));
        assert!(script.contains(&format!("DF_SHA256=\"{}\"", super::DF_SHA256)));
        assert!(super::df_url().contains("libdeep_filter_ladspa-0.5.6-x86_64"));
    }

    #[test]
    fn meter_reads_float_peaks_without_clamping() {
        let bytes = |v: &[f32]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
        // A signal limited to -0.5 dBFS must read -0.5, not 0.0 (the old s16 meter's mistake).
        let lim = 10f32.powf(-0.5 / 20.0);
        assert!((block_peak_db(&bytes(&[0.1, -lim, 0.3])) + 0.5).abs() < 0.01);
        // Real overs stay visible above 0 dB.
        assert!(block_peak_db(&bytes(&[1.5])) > 3.0);
        assert_eq!(block_peak_db(&bytes(&[f32::NAN, 0.0])), super::METER_FLOOR_DB);
        assert!((block_peak_db(&bytes(&[0.001])) + 60.0).abs() < 0.01);
    }

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn stale_sweep_matches_only_real_children() {
        assert!(is_our_child(&v(&["pipewire", "-c", "/run/user/1000/studio-mic/sm-mic-post-g3.conf"])));
        assert!(is_our_child(&v(&["pipewire", "-c", "/run/user/1000/studiomic/studiomic-graph.conf"])));
        assert!(is_our_child(&v(&["pw-record", "--rate", "8000", "-P", "{ node.name = StudioMic_Meter_mic node.autoconnect = false }", "-"])));
        assert!(is_our_child(&v(&["pw-record", "--channels", "1", "--rate", "8000", "--format", "s16", "-"])));
        // Shells, editors and greps that merely mention the names must never be killed.
        assert!(!is_our_child(&v(&["bash", "-c", "grep studiomic-graph.conf; pgrep -f /studio-mic/sm-"])));
        assert!(!is_our_child(&v(&["nvim", "/run/user/1000/studio-mic/sm-buses.conf"])));
        assert!(!is_our_child(&v(&["pipewire"])));
        assert!(!is_our_child(&v(&["pw-record", "--rate", "8000", "my-recording.wav"])));
    }
}
