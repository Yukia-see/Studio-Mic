//! Controller: the single place that mutates state. The webview, the Unix-socket CLI and the
//! tray all call into it. Every change is persisted (debounced atomic write) and pushed to the
//! engine on a background worker so the UI thread never blocks on PipeWire.

use crate::audio::daemon::{Engine, EngineEvent};
use crate::audio::device_discovery::DeviceDiscovery;
use crate::audio::pw::{db_to_lin, lin_to_db};
use crate::config::{self, preset_channel, sanitize_channel_id, StudioMicConfig, VirtualChannelConfig, THEMES};
use crate::models::microphone::{EqBand, FilterType, MicDatabase};
use crate::routing::wireplumber_hook::StreamWatcher;
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex};

/// Work for the engine worker. Each job re-applies the CURRENT state, so duplicates coalesce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Job {
    Mixer,
    Pre,
    Dsp,
    Eq,
    Denoiser,
    DenoiserParams,
    Monitor,
    Channels,
    Mic2,
    Relink,
    RestoreAutopad,
    /// Custom mixes / send stages / output devices changed.
    Mixes,
}

#[derive(Debug, Clone)]
pub enum UiPush {
    /// Run this script in the webview.
    Js(String),
    ShowWindow,
    ToggleWindow,
    Quit,
    TrayRefresh,
    /// Enter / leave the mini floating widget (window size, always-on-top, title).
    Widget { on: bool, width: f64, height: f64, on_top: bool },
    // Typed events for the native (Slint) UI. The webview ignores them and uses the `Js` twins.
    /// The saved state changed: re-read `Controller::state`.
    State,
    /// Meter levels (`Engine::peak_map` as JSON), ~20 Hz while the window is visible.
    Peaks(String),
    /// Short message for the user (toast).
    Notice(String),
    /// The saved hardware mic is unplugged (true) or back (false).
    MicMissing(bool),
    /// Available LADSPA plugins changed (`Engine::plugins_json`).
    Plugins(String),
    /// Hardware inputs and outputs (after a rescan).
    Devices { mics: Vec<crate::audio::device_discovery::AudioDevice>, outs: Vec<crate::audio::device_discovery::AudioDevice> },
    /// App streams playing right now (which channel each one is routed to).
    LiveStreams(Vec<crate::routing::wireplumber_hook::HookedStream>),
}

pub struct Controller {
    pub state: Arc<Mutex<StudioMicConfig>>,
    pub engine: Arc<Engine>,
    jobs: Arc<(Mutex<(VecDeque<Job>, HashSet<Job>)>, Condvar)>,
    ui: Mutex<Option<Box<dyn Fn(UiPush) + Send>>>,
    /// Set by every state change; a background thread turns it into at most one tray rebuild and
    /// Stream Deck state write per 120 ms (each fader message used to do both on the UI thread).
    tray_dirty: std::sync::atomic::AtomicBool,
}

pub type Reply = Result<String, String>;

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_lowercase().as_str() {
        "1" | "on" | "true" | "yes" => Some(true),
        "0" | "off" | "false" | "no" => Some(false),
        _ => None,
    }
}

/// "+2dB" / "-1.5" are relative; "=−3", "exact:-3" or an unsigned number are absolute.
pub fn parse_gain_spec(spec: &str, current: f32) -> Option<f32> {
    let s = spec.trim().to_lowercase().replace("db", "").replace(' ', "");
    if let Some(rest) = s.strip_prefix('=').or_else(|| s.strip_prefix("exact:")) {
        return rest.parse::<f32>().ok().map(config::clamp_gain);
    }
    if s.starts_with('+') || s.starts_with('-') {
        return s.parse::<f32>().ok().map(|d| config::clamp_gain(current + d));
    }
    s.parse::<f32>().ok().map(config::clamp_gain)
}

impl Controller {
    pub fn new(state: Arc<Mutex<StudioMicConfig>>, engine: Arc<Engine>) -> Arc<Self> {
        let c = Arc::new(Self {
            state,
            engine,
            jobs: Arc::new((Mutex::new((VecDeque::new(), HashSet::new())), Condvar::new())),
            ui: Mutex::new(None),
            tray_dirty: std::sync::atomic::AtomicBool::new(true), // write the deck state once at startup
        });
        let weak_tray = Arc::downgrade(&c);
        std::thread::Builder::new()
            .name("tray-refresh".into())
            .spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_millis(120));
                let Some(c) = weak_tray.upgrade() else { return };
                if c.tray_dirty.swap(false, std::sync::atomic::Ordering::AcqRel) {
                    c.write_deck_state();
                    if let Some(f) = c.ui.lock().unwrap().as_ref() {
                        f(UiPush::TrayRefresh);
                    }
                }
            })
            .expect("spawn tray refresher");
        let worker = c.clone();
        std::thread::Builder::new()
            .name("engine-worker".into())
            .spawn(move || worker.worker_loop())
            .expect("spawn engine worker");

        // Engine events -> UI
        let weak = Arc::downgrade(&c);
        c.engine.set_event_sink(move |ev| {
            if let Some(c) = weak.upgrade() {
                c.on_engine_event(ev);
            }
        });
        c
    }

    pub fn set_ui_sink<F: Fn(UiPush) + Send + 'static>(&self, f: F) {
        *self.ui.lock().unwrap() = Some(Box::new(f));
    }

    fn push(&self, p: UiPush) {
        if matches!(p, UiPush::TrayRefresh) {
            self.tray_dirty.store(true, std::sync::atomic::Ordering::Release);
            return;
        }
        if let Some(f) = self.ui.lock().unwrap().as_ref() {
            f(p);
        }
    }

    /// Live state for the StreamController plugin icons (~/.cache/studio-mic/streamcontroller/state.json).
    /// Channels are keyed by both id and lower-cased name. Only written when it changes.
    pub fn write_deck_state(&self) {
        static LAST: std::sync::LazyLock<Mutex<String>> = std::sync::LazyLock::new(|| Mutex::new(String::new()));
        let s = self.state.lock().unwrap().clone();
        let mut channels = serde_json::Map::new();
        let mut put = |id: &str, name: &str, m: &config::ChannelMix| {
            let v = serde_json::json!({
                "stream_muted": m.stream_muted, "monitor_muted": m.monitor_muted, "vod": m.vod,
                "solo": m.solo, "autoduck": m.autoduck,
                "stream_gain_db": m.stream_gain_db, "monitor_gain_db": m.monitor_gain_db,
            });
            channels.insert(id.to_lowercase(), v.clone());
            channels.insert(name.to_lowercase(), v);
        };
        put("mic", "microphone", &s.mic.mix);
        for c in &s.virtual_channels {
            put(&c.id, &c.name, &c.mix);
        }
        let v = serde_json::json!({
            "mic_muted": s.mic.mix.stream_muted,
            "monitor_muted": s.monitor_muted,
            "monitor_gain_db": s.monitor_gain_db,
            "clipguard": s.mic.clipguard,
            "denoiser": s.denoiser.engine,
            "theme": s.theme,
            "view_mode": s.view_mode,
            "widget_mode": s.widget_mode,
            "preview": self.engine.preview_is_active(),
            "channels": channels,
        });
        let json = serde_json::to_string_pretty(&v).unwrap_or_default();
        let mut last = LAST.lock().unwrap();
        if *last == json {
            return;
        }
        let dir = config::cache_dir().join("streamcontroller");
        if std::fs::create_dir_all(&dir).is_ok() && std::fs::write(dir.join("state.json.tmp"), &json).is_ok() {
            let _ = std::fs::rename(dir.join("state.json.tmp"), dir.join("state.json"));
            *last = json;
        }
    }

    fn js(&self, script: String) {
        self.push(UiPush::Js(script));
    }

    fn notice(&self, msg: String) {
        self.js(format!("window.showNotice && window.showNotice({})", serde_json::to_string(&msg).unwrap()));
        self.push(UiPush::Notice(msg));
    }

    fn worker_loop(self: Arc<Self>) {
        let (lock, cv) = &*self.jobs;
        loop {
            let job = {
                let mut q = lock.lock().unwrap();
                while q.0.is_empty() {
                    q = cv.wait(q).unwrap();
                }
                let j = q.0.pop_front().unwrap();
                q.1.remove(&j);
                j
            };
            let e = &self.engine;
            match job {
                Job::Mixer => e.apply_mixer(true),
                Job::Pre => e.apply_pre(),
                Job::Dsp => e.apply_dsp(),
                Job::Eq => e.apply_eq(),
                Job::DenoiserParams => e.apply_denoiser_params(),
                Job::Denoiser => {
                    let r = e.apply_denoiser();
                    let active = e.runtime_status()["denoiser_active"].as_str().unwrap_or("off").to_string();
                    let msg = match r {
                        Ok(_) => format!("Noise remover: {}", active),
                        Err(err) => err,
                    };
                    self.js(format!(
                        "window.onDenoiserResult && window.onDenoiserResult({}, {})",
                        serde_json::to_string(&active).unwrap(),
                        serde_json::to_string(&msg).unwrap()
                    ));
                }
                Job::Monitor => e.apply_monitor_master(),
                // Channel and Mic 2 changes alter the send stages' source lists too.
                Job::Channels => {
                    e.sync_channels();
                    e.sync_sends();
                }
                Job::Mic2 => {
                    e.sync_mic2(false);
                    e.sync_sends();
                }
                // Relinks also follow channel-kind changes (hardware inputs may feed Voice Chat).
                Job::Relink | Job::Mixes => e.sync_sends(),
                Job::RestoreAutopad => e.restore_autopad(),
            }
        }
    }

    pub fn enqueue(&self, job: Job) {
        let (lock, cv) = &*self.jobs;
        let mut q = lock.lock().unwrap();
        if q.1.insert(job) {
            q.0.push_back(job);
            cv.notify_one();
        }
    }

    fn mutate<F: FnOnce(&mut StudioMicConfig) -> Reply>(&self, f: F) -> Reply {
        let mut s = self.state.lock().unwrap();
        let r = f(&mut s);
        if r.is_ok() {
            s.normalize();
            let _ = s.save();
        }
        r
    }

    /// Pushes the full state to the webview (used for changes that did not come from the UI).
    pub fn sync_ui(&self) {
        let json = serde_json::to_string(&*self.state.lock().unwrap()).unwrap_or_else(|_| "{}".into());
        self.js(format!("window.applyBackendState && window.applyBackendState({})", json));
        self.push(UiPush::State);
        self.push(UiPush::TrayRefresh);
    }

    fn on_engine_event(&self, ev: EngineEvent) {
        match ev {
            // 20 Hz: the JSON is embedded (and parsed) once per push.
            EngineEvent::Peaks(json) => {
                self.js(format!(
                    "(p => {{ window.updateChannelPeaks && window.updateChannelPeaks(p); window.updatePeakMeter && window.updatePeakMeter(p.mic); }})({})",
                    json
                ));
                self.push(UiPush::Peaks(json));
            }
            EngineEvent::Recorder(json) => self.js(format!("window.updateRecorderState && window.updateRecorderState({})", json)),
            EngineEvent::Preview(json) => {
                self.js(format!("window.updatePreviewState && window.updatePreviewState({})", json));
                self.push(UiPush::TrayRefresh);
            }
            EngineEvent::MonitorMaster => self.sync_ui(),
            EngineEvent::PluginsChanged(json) => {
                self.js(format!("window.onPluginsChanged && window.onPluginsChanged({})", json));
                self.push(UiPush::Plugins(json));
            }
            EngineEvent::Notice(msg) => self.notice(msg),
            EngineEvent::GateMeasured(None) => self.js(
                "window.onGateAutoSetFailed && window.onGateAutoSetFailed('No signal from the microphone: check that it is plugged in and selected. The gate was not changed.')"
                    .into(),
            ),
            EngineEvent::GateMeasured(Some(peak_db)) => {
                // +4 dB above the loudest keystroke; with 7 dB hysteresis the clicks stay shut out.
                let thr = (peak_db + 4.0).clamp(-70.0, -15.0);
                let _ = self.mutate(|s| {
                    s.noise_clean.gate_enabled = true;
                    s.noise_clean.gate_threshold_db = thr.round();
                    Ok(String::new())
                });
                self.enqueue(Job::Dsp);
                self.js(format!("window.onGateAutoSet && window.onGateAutoSet({:.1}, {:.1})", peak_db, thr.round()));
            }
            EngineEvent::MicMissing(missing) => {
                self.js(format!("window.setMicMissing && window.setMicMissing({})", missing));
                self.push(UiPush::MicMissing(missing));
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Mixer operations
    // -----------------------------------------------------------------------------------------

    pub fn set_mix(&self, ch: &str, param: &str, value: &str) -> Reply {
        let r = self.mutate(|s| {
            let id = s.resolve_channel(ch).ok_or_else(|| format!("unknown channel '{}'", ch))?;
            let voice_ok = id == "mic" || s.voice_eligible(&id);
            let mix_ids: Vec<String> = s.custom_mixes.iter().map(|m| m.id.clone()).collect();
            let m = s.mix_mut(&id).ok_or_else(|| format!("unknown channel '{}'", ch))?;
            let b = || parse_bool(value).ok_or_else(|| format!("expected on/off, got '{}'", value));
            // Sends into custom mixes: send_route.<mix> / send_mute.<mix> / send_gain.<mix>
            if let Some((what, mix)) = param.split_once('.') {
                if !mix_ids.iter().any(|x| x == mix) {
                    return Err(format!("unknown mix '{}'", mix));
                }
                match what {
                    "send_route" => {
                        if b()? {
                            m.sends.entry(mix.to_string()).or_default();
                        } else {
                            m.sends.remove(mix);
                        }
                    }
                    "send_mute" => m.sends.entry(mix.to_string()).or_default().muted = b()?,
                    "send_gain" => {
                        let cell = m.sends.entry(mix.to_string()).or_default();
                        cell.gain_db = parse_gain_spec(value, cell.gain_db).ok_or("bad gain")?;
                    }
                    _ => return Err(format!("unknown mix parameter '{}'", param)),
                }
                return Ok(format!("{} {} = {}", id, param, value));
            }
            match param {
                "stream_gain" => m.stream_gain_db = parse_gain_spec(value, m.stream_gain_db).ok_or("bad gain")?,
                "monitor_gain" => m.monitor_gain_db = parse_gain_spec(value, m.monitor_gain_db).ok_or("bad gain")?,
                "stream_mute" => m.stream_muted = b()?,
                "monitor_mute" => m.monitor_muted = b()?,
                "vod" => m.vod = b()?,
                "autoduck" => m.autoduck = b()?,
                "solo" => m.solo = b()?,
                // Routing-matrix cells
                "stream_route" => m.stream_routed = b()?,
                "monitor_route" => m.monitor_routed = b()?,
                // Voice Chat: the main mic always feeds it; any other source on request, except
                // the voice-chat channel itself (echo guard).
                "voice_route" if voice_ok && id != "mic" => m.voice_routed = b()?,
                "voice_mute" if voice_ok => m.voice_muted = b()?,
                "voice_gain" if voice_ok => m.voice_gain_db = parse_gain_spec(value, m.voice_gain_db).ok_or("bad gain")?,
                "voice_route" | "voice_mute" | "voice_gain" => {
                    return Err("Routing the voice-chat channel into Voice Chat would echo Discord back to itself".into())
                }
                "delay_stream" => m.delay_stream_ms = value.trim_end_matches("ms").parse::<f32>().map_err(|e| e.to_string())?,
                "delay_monitor" => m.delay_monitor_ms = value.trim_end_matches("ms").parse::<f32>().map_err(|e| e.to_string())?,
                _ => return Err(format!("unknown mix parameter '{}'", param)),
            }
            Ok(format!("{} {} = {}", id, param, value))
        });
        if r.is_ok() {
            self.enqueue(Job::Mixer);
            self.push(UiPush::TrayRefresh);
        }
        r
    }

    pub fn mic_muted(&self) -> bool {
        self.state.lock().unwrap().mic.mix.stream_muted
    }

    pub fn set_mic_mute(&self, mute: bool) -> Reply {
        let r = self.set_mix("mic", "stream_mute", if mute { "on" } else { "off" });
        self.sync_ui();
        r.map(|_| format!("Microphone {}", if mute { "MUTED" } else { "LIVE" }))
    }

    pub fn toggle_mic_mute(&self) -> Reply {
        let m = self.mic_muted();
        self.set_mic_mute(!m)
    }

    pub fn toggle_monitor_mute(&self) -> Reply {
        let r = self.mutate(|s| {
            s.monitor_muted = !s.monitor_muted;
            Ok(format!("Monitor {}", if s.monitor_muted { "muted" } else { "unmuted" }))
        });
        self.enqueue(Job::Monitor);
        self.sync_ui();
        r
    }

    pub fn set_monitor_master(&self, param: &str, value: &str) -> Reply {
        let r = self.mutate(|s| {
            match param {
                "gain" => s.monitor_gain_db = parse_gain_spec(value, s.monitor_gain_db).ok_or("bad gain")?,
                "mute" => s.monitor_muted = parse_bool(value).ok_or("expected on/off")?,
                _ => return Err("unknown monitor parameter".into()),
            }
            Ok(format!("Monitor gain {:+.1} dB{}", s.monitor_gain_db, if s.monitor_muted { " (muted)" } else { "" }))
        });
        self.enqueue(Job::Monitor);
        r
    }

    pub fn nudge_monitor_gain(&self, delta_db: f32) {
        let _ = self.set_monitor_master("gain", &format!("{:+}", delta_db));
        self.sync_ui();
    }

    pub fn channel_mute(&self, ch: &str, target: &str) -> Reply {
        let (id, mix) = {
            let s = self.state.lock().unwrap();
            let id = s.resolve_channel(ch).ok_or_else(|| format!("unknown channel '{}'", ch))?;
            (id.clone(), s.mix(&id).cloned().ok_or("unknown channel")?)
        };
        let flip = |b: bool| if b { "off" } else { "on" };
        let r = match target {
            "stream" => self.set_mix(&id, "stream_mute", flip(mix.stream_muted)),
            "monitor" => self.set_mix(&id, "monitor_mute", flip(mix.monitor_muted)),
            // VOD "mute" = exclude from the VOD track
            "vod" => self.set_mix(&id, "vod", if mix.vod { "off" } else { "on" }),
            "all" => {
                let to = !(mix.stream_muted && mix.monitor_muted);
                let v = if to { "on" } else { "off" };
                self.set_mix(&id, "stream_mute", v)?;
                self.set_mix(&id, "monitor_mute", v)
            }
            _ => Err("target must be stream|monitor|vod|all".into()),
        };
        self.sync_ui();
        r
    }

    pub fn channel_solo(&self, ch: &str) -> Reply {
        let solo = {
            let s = self.state.lock().unwrap();
            let id = s.resolve_channel(ch).ok_or_else(|| format!("unknown channel '{}'", ch))?;
            s.mix(&id).map(|m| m.solo).unwrap_or(false)
        };
        let r = self.set_mix(ch, "solo", if solo { "off" } else { "on" });
        self.sync_ui();
        r
    }

    pub fn channel_gain(&self, ch: &str, bus: &str, spec: &str) -> Reply {
        let param = match bus {
            "stream" => "stream_gain",
            "monitor" => "monitor_gain",
            _ => return Err("bus must be stream|monitor".into()),
        };
        let r = self.set_mix(ch, param, spec);
        self.sync_ui();
        let s = self.state.lock().unwrap();
        let id = s.resolve_channel(ch).unwrap_or_default();
        let m = s.mix(&id).cloned().unwrap_or_default();
        r.map(|_| {
            let db = if bus == "stream" { m.stream_gain_db } else { m.monitor_gain_db };
            format!("{} {} gain {:+.1} dB ({:.0}%)", id, bus, db, db_to_lin(db) * 100.0)
        })
    }

    pub fn cell_delay(&self, ch: &str, bus: &str, ms: &str) -> Reply {
        let param = match bus {
            "stream" | "vod" => "delay_stream",
            "monitor" => "delay_monitor",
            _ => return Err("bus must be stream|monitor".into()),
        };
        let r = self.set_mix(ch, param, ms);
        self.sync_ui();
        r
    }

    pub fn toggle_autoduck(&self, ch: &str) -> Reply {
        let (id, on, comms) = {
            let s = self.state.lock().unwrap();
            let id = s.resolve_channel(ch).ok_or_else(|| format!("unknown channel '{}'", ch))?;
            let comms = s.channel(&id).map(|c| c.is_comms()).unwrap_or(false);
            (id.clone(), s.mix(&id).map(|m| m.autoduck).unwrap_or(false), comms)
        };
        if comms || id == "mic" {
            return Err("voice chat and the microphone are exempt from auto-ducking".into());
        }
        let r = self.set_mix(&id, "autoduck", if on { "off" } else { "on" });
        self.sync_ui();
        r
    }

    pub fn set_mic_option(&self, name: &str, value: bool) -> Reply {
        let r = self.mutate(|s| {
            match name {
                "mono_center" => s.mic.mono_center = value,
                "clipguard" => s.mic.clipguard = value,
                "monitor_raw" => s.mic.monitor_mode = if value { "raw".into() } else { "processed".into() },
                _ => return Err(format!("unknown mic option '{}'", name)),
            }
            Ok(format!("{} {}", name, if value { "ON" } else { "OFF" }))
        });
        match name {
            "mono_center" => self.enqueue(Job::Pre),
            "monitor_raw" => self.enqueue(Job::Mixer),
            _ => {
                self.enqueue(Job::Dsp);
                if !value {
                    self.enqueue(Job::RestoreAutopad);
                }
            }
        }
        r
    }

    pub fn toggle_clipguard(&self) -> Reply {
        let on = self.state.lock().unwrap().mic.clipguard;
        let r = self.set_mic_option("clipguard", !on);
        self.sync_ui();
        r
    }

    /// "+ Add Mix": a new destination bus (`StudioMic_Mix_<id>`).
    pub fn create_mix(&self, name: &str, color: Option<String>, output: Option<String>) -> Reply {
        let name = name.trim();
        if name.is_empty() {
            return Err("mix name required".into());
        }
        let r = self.mutate(|s| {
            let base = sanitize_channel_id(name);
            let mut id = base.clone();
            let mut n = 2;
            while config::RESERVED_MIX_IDS.contains(&id.as_str()) || s.custom_mixes.iter().any(|m| m.id == id) {
                id = format!("{}_{}", base, n);
                n += 1;
            }
            s.custom_mixes.push(config::CustomMixConfig {
                id: id.clone(),
                name: name.to_string(),
                icon: "alt_route".into(),
                color: color.filter(|c| c.starts_with('#') && c.len() <= 9).unwrap_or_else(|| "#22d3ee".into()),
                output_device: output.filter(|o| !o.is_empty()),
            });
            Ok(id)
        });
        if r.is_ok() {
            self.enqueue(Job::Mixes);
            self.sync_ui();
        }
        r
    }

    pub fn remove_mix(&self, id: &str) -> Reply {
        let r = self.mutate(|s| {
            let before = s.custom_mixes.len();
            s.custom_mixes.retain(|m| m.id != id);
            if s.custom_mixes.len() == before {
                return Err(format!("unknown mix '{}'", id));
            }
            Ok(format!("Removed mix {}", id)) // normalize() drops the channels' sends into it
        });
        if r.is_ok() {
            self.enqueue(Job::Mixes);
            self.sync_ui();
        }
        r
    }

    /// Physical output of a mix: "monitor" (My Mix), "stream" / "vod" / "voice", or a custom id.
    pub fn set_mix_output(&self, key: &str, node: &str) -> Reply {
        if key == "monitor" {
            return self.set_monitor_output(node);
        }
        let node = node.trim();
        let r = self.mutate(|s| {
            let dev = (!node.is_empty() && node != "none").then(|| node.to_string());
            if ["stream", "vod", "voice"].contains(&key) {
                match dev {
                    Some(d) => s.mix_output_devices.insert(key.to_string(), d),
                    None => s.mix_output_devices.remove(key),
                };
            } else {
                let m = s.custom_mixes.iter_mut().find(|m| m.id == key).ok_or_else(|| format!("unknown mix '{}'", key))?;
                m.output_device = dev;
            }
            Ok(format!("{} output: {}", key, if node.is_empty() { "none" } else { node }))
        });
        if r.is_ok() {
            self.enqueue(Job::Mixes);
        }
        r
    }

    pub fn set_monitor_output(&self, node: &str) -> Reply {
        let r = self.mutate(|s| {
            s.monitor_output = if node.is_empty() || node == "default" { None } else { Some(node.to_string()) };
            Ok(format!("Monitor output: {}", node))
        });
        self.enqueue(Job::Relink);
        r
    }

    // -----------------------------------------------------------------------------------------
    // DSP operations
    // -----------------------------------------------------------------------------------------

    pub fn set_denoiser(&self, engine: &str) -> Reply {
        let e = engine.trim().to_lowercase();
        let e = match e.as_str() {
            "deep" | "deepfilternet" | "deep_filter" => "deepfilter".to_string(),
            other => other.to_string(),
        };
        if !config::DENOISERS.contains(&e.as_str()) {
            return Err("denoiser must be deepfilter|rnnoise|speex|off".into());
        }
        if !self.engine.plugins().denoiser_available(&e) {
            return Err(format!("{} plugin is not installed; run setup-deps.sh", e));
        }
        let r = self.mutate(|s| {
            s.denoiser.engine = e.clone();
            s.noise_clean.deep_filter_enabled = e == "deepfilter";
            Ok(format!("Noise remover: {}", e))
        });
        self.enqueue(Job::Denoiser);
        self.push(UiPush::TrayRefresh);
        r
    }

    pub fn set_denoiser_param(&self, name: &str, value: f32) -> Reply {
        let r = self.mutate(|s| {
            let d = &mut s.denoiser;
            match name {
                "df_attenuation_db" => d.df_attenuation_db = value,
                "rn_vad_threshold" => d.rn_vad_threshold = value,
                "rn_vad_grace_ms" => d.rn_vad_grace_ms = value,
                "speex_suppress_db" => d.speex_suppress_db = value,
                _ => return Err(format!("unknown denoiser parameter '{}'", name)),
            }
            Ok(String::new())
        });
        self.enqueue(Job::DenoiserParams);
        r
    }

    pub fn set_noise_clean(&self, nc: config::NoiseCleanConfig) -> Reply {
        let clipguard_off = !nc.limiter_enabled;
        let r = self.mutate(|s| {
            s.mic.clipguard = nc.limiter_enabled;
            s.noise_clean = nc;
            Ok("DSP updated".into())
        });
        self.enqueue(Job::Dsp);
        if clipguard_off {
            // Same as the CLI/tray toggle: Clipguard off gives the mic its hardware level back.
            self.enqueue(Job::RestoreAutopad);
        }
        self.push(UiPush::TrayRefresh);
        r
    }

    pub fn set_eq(&self, bands: Vec<EqBand>, preset: Option<String>, mic_id: Option<String>) -> Reply {
        let r = self.mutate(|s| {
            s.eq_bands = bands.into_iter().take(10).collect();
            if let Some(p) = preset.filter(|p| !p.is_empty()) {
                s.preset_name = p;
            }
            if let Some(m) = mic_id.filter(|m| !m.is_empty()) {
                s.default_mic_id = m;
            }
            Ok("EQ updated".into())
        });
        self.enqueue(Job::Eq);
        r
    }

    /// EQ preset by name: mic database id/model, "Recommended EQ for <mic>", user preset title, or "flat".
    pub fn set_preset(&self, name: &str) -> Reply {
        let n = name.trim().to_lowercase();
        if n == "flat" {
            let r = self.set_eq(vec![EqBand { freq: 1000.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking }], Some("Flat".into()), None);
            self.sync_ui();
            return r.map(|_| "EQ preset: Flat".into());
        }
        let users = self.state.lock().unwrap().user_presets.clone();
        if let Some(p) = users.iter().find(|p| p.title.to_lowercase() == n) {
            let r = self.set_eq(p.bands.clone(), Some(format!("Custom: {}", p.title)), None);
            self.sync_ui();
            return r.map(|_| format!("EQ preset: {}", p.title));
        }
        let wanted = n.trim_start_matches("recommended eq for ").to_string();
        let db = MicDatabase::get_all();
        let hit = db.iter().find(|m| {
            m.id.to_lowercase() == wanted
                || format!("{} {}", m.brand, m.model).to_lowercase() == wanted
                || m.model.to_lowercase() == wanted
                || m.profile_title.to_lowercase() == n
        });
        match hit {
            Some(m) => {
                let r = self.set_eq(m.bands.clone(), Some(format!("Recommended EQ for {} {}", m.brand, m.model)), Some(m.id.clone()));
                self.sync_ui();
                r.map(|_| format!("EQ preset: Recommended EQ for {} {}", m.brand, m.model))
            }
            None => Err(format!("unknown preset '{}'", name)),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Devices
    // -----------------------------------------------------------------------------------------

    pub fn set_hw_mic(&self, node: &str) -> Reply {
        let r = self.mutate(|s| {
            s.active_hw_mic = Some(node.to_string());
            Ok(format!("Microphone input: {}", node))
        });
        self.enqueue(Job::RestoreAutopad);
        self.enqueue(Job::Relink);
        r
    }

    pub fn rescan_devices(&self) {
        crate::audio::pw::invalidate_cache(); // "Refresh" must see devices plugged in just now
        let mics = DeviceDiscovery::get_input_devices();
        let outs = DeviceDiscovery::get_output_devices();
        let cards = DeviceDiscovery::get_capture_cards_from(&mics);
        self.js(format!(
            "window.populateHardwareDevices && window.populateHardwareDevices({}); window.updateOutputDevices && window.updateOutputDevices({}); window.updateCaptureCards && window.updateCaptureCards({})",
            serde_json::to_string(&mics).unwrap_or_default(),
            serde_json::to_string(&outs).unwrap_or_default(),
            serde_json::to_string(&cards).unwrap_or_default()
        ));
        self.push(UiPush::Devices { mics, outs });
        self.enqueue(Job::Relink);
    }

    /// Device lists for the native UI at startup (no relink, unlike `rescan_devices`).
    #[cfg_attr(not(feature = "ui-slint"), allow(dead_code))]
    pub fn push_devices(&self) {
        self.push(UiPush::Devices { mics: DeviceDiscovery::get_input_devices(), outs: DeviceDiscovery::get_output_devices() });
    }

    // -----------------------------------------------------------------------------------------
    // Channels
    // -----------------------------------------------------------------------------------------

    pub fn create_channel(&self, name: &str, preset: Option<&str>) -> Reply {
        self.create_channel_with_input(name, preset, None)
    }

    /// Like `create_channel`; `hw_source` makes it a hardware-input (Line In) channel.
    pub fn create_channel_with_input(&self, name: &str, preset: Option<&str>, hw_source: Option<String>) -> Reply {
        let name = name.trim();
        if name.is_empty() {
            return Err("channel name required".into());
        }
        let r = self.mutate(|s| {
            let base = sanitize_channel_id(preset.filter(|p| !p.is_empty() && *p != "custom").unwrap_or(name));
            let mut id = base.clone();
            let mut n = 2;
            while id == "mic" || id == "mic2" || s.channel(&id).is_some() {
                id = format!("{}_{}", base, n);
                n += 1;
            }
            let mut ch = match preset {
                Some(p) if !p.is_empty() => preset_channel(p),
                _ => VirtualChannelConfig::new_with_defaults(&id, name),
            };
            ch.id = id.clone();
            ch.name = name.to_string();
            if let Some(hw) = hw_source {
                ch.hw_source = Some(hw);
                ch.kind = "hardware".into();
                ch.route_mode = "manual".into();
                ch.apps.clear();
            }
            s.virtual_channels.push(ch);
            Ok(id)
        });
        if r.is_ok() {
            self.enqueue(Job::Channels);
            self.sync_ui();
        }
        r
    }

    pub fn remove_channel(&self, ch: &str) -> Reply {
        let id = self.state.lock().unwrap().resolve_channel(ch).ok_or_else(|| format!("unknown channel '{}'", ch))?;
        if id == "mic" || id == "mic2" {
            return Err("the microphone channel cannot be removed".into());
        }
        // Make-before-break: hand its apps back to the default output first.
        StreamWatcher::release_channel(&id);
        let r = self.mutate(|s| {
            s.virtual_channels.retain(|c| c.id != id);
            Ok(format!("Removed channel {}", id))
        });
        self.enqueue(Job::Channels);
        self.sync_ui();
        r
    }

    pub fn rename_channel(&self, ch: &str, new_name: &str) -> Reply {
        let new_name = new_name.trim().to_string();
        if new_name.is_empty() {
            return Err("name required".into());
        }
        let r = self.mutate(|s| {
            let id = s.resolve_channel(ch).ok_or_else(|| format!("unknown channel '{}'", ch))?;
            let c = s.channel_mut(&id).ok_or("the microphone channel cannot be renamed")?;
            c.name = new_name.clone();
            Ok(format!("Renamed {} -> {}", id, new_name))
        });
        if r.is_ok() {
            self.enqueue(Job::Channels);
            self.sync_ui();
        }
        r
    }

    pub fn set_channel_hw(&self, ch: &str, node: &str) -> Reply {
        let r = self.mutate(|s| {
            let id = s.resolve_channel(ch).ok_or_else(|| format!("unknown channel '{}'", ch))?;
            let c = s.channel_mut(&id).ok_or("unknown channel")?;
            c.kind = "hardware".into();
            c.hw_source = if node.is_empty() { None } else { Some(node.to_string()) };
            if id == "capture" || c.name.to_lowercase().contains("capture") {
                s.capture_card_source = Some(node.to_string());
            }
            Ok(format!("{} input: {}", id, node))
        });
        self.enqueue(Job::Relink);
        r
    }

    pub fn set_channel_apps(&self, ch: &str, apps: Vec<String>, mode: Option<String>) -> Reply {
        let r = self.mutate(|s| {
            let id = s.resolve_channel(ch).ok_or_else(|| format!("unknown channel '{}'", ch))?;
            let c = s.channel_mut(&id).ok_or("unknown channel")?;
            let mut seen = HashSet::new();
            c.apps = apps.into_iter().map(|a| a.trim().to_lowercase()).filter(|a| !a.is_empty() && seen.insert(a.clone())).collect();
            if let Some(m) = mode.filter(|m| ["auto", "manual", "desktop"].contains(&m.as_str())) {
                c.route_mode = m;
            }
            if c.kind == "hardware" && !c.apps.is_empty() {
                c.kind = "virtual".into();
            }
            Ok(format!("{} apps: {:?}", id, c.apps))
        });
        self.reroute_now();
        self.enqueue(Job::Relink); // a hardware channel may have become an app channel
        r
    }

    pub fn assign_app(&self, ch: &str, app: &str) -> Reply {
        let app = app.trim().to_lowercase();
        if app.is_empty() {
            return Err("app name required".into());
        }
        // An app lives in exactly one channel.
        let r = self.mutate(|s| {
            let id = s.resolve_channel(ch).ok_or_else(|| format!("unknown channel '{}'", ch))?;
            for c in s.virtual_channels.iter_mut() {
                c.apps.retain(|a| a != &app);
            }
            let c = s.channel_mut(&id).ok_or("unknown channel")?;
            c.apps.push(app.clone());
            if c.kind == "hardware" {
                c.kind = "virtual".into();
            }
            Ok(format!("Assigned '{}' to {}", app, id))
        });
        self.reroute_now();
        self.sync_ui();
        r
    }

    pub fn unassign_app(&self, ch: &str, app: &str) -> Reply {
        let app = app.trim().to_lowercase();
        let r = self.mutate(|s| {
            let id = s.resolve_channel(ch).ok_or_else(|| format!("unknown channel '{}'", ch))?;
            let c = s.channel_mut(&id).ok_or("unknown channel")?;
            c.apps.retain(|a| a != &app);
            Ok(format!("Removed '{}' from {}", app, id))
        });
        self.reroute_now();
        self.sync_ui();
        r
    }

    /// Adds the focused window's application (Hyprland / sway / X11) to a channel.
    pub fn add_foreground_app(&self, ch: &str) -> Reply {
        let pid_or_class = foreground_app().ok_or("could not detect the focused application")?;
        let streams = StreamWatcher::scan_live_streams();
        let app = streams
            .iter()
            .find(|s| pid_or_class.0 == Some(s.pid) && s.pid != 0)
            .map(|s| if s.binary.is_empty() { s.app_name.clone() } else { s.binary.clone() })
            .or(pid_or_class.1)
            .ok_or("focused application is not producing audio")?;
        self.assign_app(ch, &app)
    }

    /// Applies app-assignment changes to running streams now instead of on the next 2 s pass
    /// (off the calling thread: it queries PipeWire).
    fn reroute_now(&self) {
        let channels = self.state.lock().unwrap().virtual_channels.clone();
        std::thread::spawn(move || StreamWatcher::auto_route(&channels));
    }

    pub fn route_stream(&self, node_id: u32, ch: &str) -> Reply {
        let id = self.state.lock().unwrap().resolve_channel(ch).ok_or_else(|| format!("unknown channel '{}'", ch))?;
        if StreamWatcher::route_stream_to_channel(node_id, &id) { Ok(format!("stream {} -> {}", node_id, id)) } else { Err("routing failed".into()) }
    }

    // -----------------------------------------------------------------------------------------
    // UI prefs, profiles, preview
    // -----------------------------------------------------------------------------------------

    pub fn set_theme(&self, theme: &str) -> Reply {
        let t = theme.trim().to_lowercase();
        if !THEMES.contains(&t.as_str()) {
            return Err(format!("theme must be one of {:?}", THEMES));
        }
        let r = self.mutate(|s| {
            s.theme = t.clone();
            Ok(format!("Theme: {}", t))
        });
        self.js(format!("window.applyTheme && window.applyTheme('{}', true)", t));
        r
    }

    /// Mini floating widget on/off (`None` toggles). Tab 1 is matrix-only, so the old
    /// `--toggle-view` is an alias for this.
    pub fn set_widget(&self, on: Option<bool>) -> Reply {
        let r = self.mutate(|s| {
            s.widget_mode = on.unwrap_or(!s.widget_mode);
            Ok(format!("Widget mode: {}", if s.widget_mode { "on" } else { "off" }))
        });
        self.push_widget();
        self.push(UiPush::TrayRefresh);
        r
    }

    /// Tells the window loop the saved widget state (also used once at startup).
    pub fn push_widget(&self) {
        let s = self.state.lock().unwrap();
        let w = UiPush::Widget { on: s.widget_mode, width: s.widget_width, height: s.widget_height, on_top: s.always_on_top };
        drop(s);
        self.push(w);
    }

    pub fn set_always_on_top(&self, on: bool) -> Reply {
        let r = self.mutate(|s| {
            s.always_on_top = on;
            Ok(format!("Widget always on top: {}", if on { "on" } else { "off" }))
        });
        if self.state.lock().unwrap().widget_mode {
            self.push_widget();
        }
        r
    }

    /// Control size (Small / Medium / Large).
    pub fn set_ui_size(&self, size: &str) -> Reply {
        if !config::UI_SIZES.contains(&size) {
            return Err(format!("unknown size '{}' (small, medium, large)", size));
        }
        let r = self.mutate(|s| {
            s.ui_size = size.to_string();
            Ok(format!("Size: {}", size))
        });
        self.js(format!("window.applyUiSize && window.applyUiSize('{}')", size));
        r
    }

    pub fn save_profile(&self, name: &str) -> Reply {
        let s = self.state.lock().unwrap().clone();
        s.save_profile(name).map(|p| format!("Saved profile to {}", p.display())).map_err(|e| e.to_string())
    }

    pub fn load_profile(&self, name: &str) -> Reply {
        let current = self.state.lock().unwrap().clone();
        let loaded = current.load_profile(name).map_err(|e| format!("cannot load profile '{}': {}", name, e))?;
        {
            let mut s = self.state.lock().unwrap();
            *s = loaded;
            let _ = s.save();
        }
        self.reroute_now();
        // The profile may turn Clipguard off; any auto-pad is re-applied if it is still needed.
        for j in [Job::RestoreAutopad, Job::Channels, Job::Denoiser, Job::Eq, Job::Pre, Job::Dsp, Job::Mixer, Job::Monitor, Job::Mic2] {
            self.enqueue(j);
        }
        self.sync_ui();
        Ok(format!("Loaded profile '{}'", name))
    }

    pub fn preview(&self, action: &str) -> Reply {
        match action {
            "play" | "loop" => self.engine.preview_start(action == "loop").map(|_| "Preview playing (headphones only)".into()),
            "stop" => {
                self.engine.preview_stop();
                Ok("Preview stopped".into())
            }
            _ => Err("preview action must be play|loop|stop".into()),
        }
    }

    pub fn record(&self, action: &str) -> Reply {
        match action {
            "start" => self.engine.record_start().map(|_| "Recording (max 10 s)".into()),
            "stop" => {
                self.engine.record_stop();
                Ok("Recording stopped".into())
            }
            _ => Err("record action must be start|stop".into()),
        }
    }

    pub fn gate_autoset(&self) -> Reply {
        self.engine.measure_raw_peak(3.0);
        Ok("Listening for 3 s: type on your keyboard without speaking".into())
    }

    // -----------------------------------------------------------------------------------------
    // Status
    // -----------------------------------------------------------------------------------------

    /// Deterministic status: identical before and after a restart when nothing changed.
    pub fn status_json(&self, runtime: bool) -> serde_json::Value {
        let s = self.state.lock().unwrap().clone();
        let mix = |m: &config::ChannelMix| {
            serde_json::json!({
                "stream_gain_db": m.stream_gain_db,
                "stream_gain_pct": (db_to_lin(m.stream_gain_db) * 1000.0).round() / 10.0,
                "monitor_gain_db": m.monitor_gain_db,
                "stream_muted": m.stream_muted,
                "monitor_muted": m.monitor_muted,
                "vod": m.vod,
                "autoduck": m.autoduck,
                "solo": m.solo,
                "delay_stream_ms": m.delay_stream_ms,
                "delay_monitor_ms": m.delay_monitor_ms,
                "stream_routed": m.stream_routed,
                "monitor_routed": m.monitor_routed,
                "voice_gain_db": m.voice_gain_db,
                "voice_muted": m.voice_muted,
                "voice_routed": m.voice_routed,
                "sends": m.sends,
            })
        };
        let mut v = serde_json::json!({
            "schema_version": s.schema_version,
            "theme": s.theme,
            "view_mode": s.view_mode,
            "ui_size": s.ui_size,
            "widget_mode": s.widget_mode,
            "always_on_top": s.always_on_top,
            "mic": {
                "input": s.active_hw_mic,
                "mono_center": s.mic.mono_center,
                "clipguard": s.mic.clipguard,
                "monitor_mode": s.mic.monitor_mode,
                "mix": mix(&s.mic.mix),
            },
            "monitor": {
                "output": s.monitor_output,
                "gain_db": s.monitor_gain_db,
                "muted": s.monitor_muted,
            },
            "denoiser": s.denoiser,
            "gate": {
                "enabled": s.noise_clean.gate_enabled,
                "open_db": s.noise_clean.gate_threshold_db,
                "close_db": s.noise_clean.gate_threshold_db - crate::audio::filter_chain::GATE_HYSTERESIS_DB,
            },
            "deesser": {
                "enabled": s.noise_clean.deesser_enabled,
                "freq": s.noise_clean.deesser_freq,
                "amount_db": s.noise_clean.deesser_amount_db,
                "threshold_db": s.noise_clean.deesser_threshold_db,
            },
            "compressor": {
                "enabled": s.noise_clean.compressor_enabled,
                "threshold_db": s.noise_clean.compressor_threshold_db,
                "ratio": s.noise_clean.compressor_ratio,
                "attack_ms": s.noise_clean.compressor_attack_ms,
                "release_ms": s.noise_clean.compressor_release_ms,
                "makeup_db": s.noise_clean.compressor_makeup_db,
            },
            "eq": { "preset": s.preset_name, "bands": s.eq_bands },
            "channels": s.virtual_channels.iter().map(|c| serde_json::json!({
                "id": c.id, "name": c.name, "kind": c.kind, "input": c.hw_source,
                "apps": c.apps, "route_mode": c.route_mode, "mix": mix(&c.mix),
            })).collect::<Vec<_>>(),
            "mic2": { "enabled": s.dual_mic.enabled, "input": s.dual_mic.hw_mic, "mix": mix(&s.dual_mic.mix) },
            "active_apps_mode": s.active_apps_mode,
            "custom_mixes": s.custom_mixes,
            "mix_output_devices": s.mix_output_devices,
            "profiles": StudioMicConfig::list_profiles(),
        });
        if runtime {
            let mut rt = self.engine.runtime_status();
            rt["duck_db"] = serde_json::json!(self.engine.duck_db());
            rt["preview_active"] = serde_json::json!(self.engine.preview_is_active());
            rt["recording"] = serde_json::json!(self.engine.recording_is_active());
            rt["autopad_db"] = serde_json::json!(self.engine.autopad_db());
            rt["mic_peak_db"] = serde_json::json!(self.engine.peak("mic"));
            // Pre-fader strip levels, post-fader `out:*`, hardware `raw`, and the OBS buses `bus:*`.
            rt["meters"] = serde_json::to_value(self.engine.peak_map()).unwrap_or_default();
            rt["live_streams"] = serde_json::to_value(StreamWatcher::scan_live_streams()).unwrap_or_default();
            v["runtime"] = rt;
        }
        v
    }

    pub fn status_text(&self) -> String {
        let s = self.state.lock().unwrap().clone();
        let mut out = String::new();
        let mic = &s.mic.mix;
        out.push_str(&format!(
            "Studio Mic  ·  theme {}  ·  view {}\nMic   {}  stream {:+.1} dB{}  monitor {:+.1} dB{}  clipguard {}  mono-center {}\n",
            s.theme,
            s.view_mode,
            s.active_hw_mic.as_deref().unwrap_or("(none)"),
            mic.stream_gain_db,
            if mic.stream_muted { " MUTED" } else { "" },
            mic.monitor_gain_db,
            if mic.monitor_muted { " off" } else { "" },
            if s.mic.clipguard { "on" } else { "off" },
            if s.mic.mono_center { "on" } else { "off" },
        ));
        out.push_str(&format!(
            "Monitor  {:+.1} dB{}  ->  {}\nDenoiser {}  ·  EQ preset {}\n",
            s.monitor_gain_db,
            if s.monitor_muted { " MUTED" } else { "" },
            s.monitor_output.as_deref().unwrap_or("(default output)"),
            s.denoiser.engine,
            s.preset_name
        ));
        for c in &s.virtual_channels {
            out.push_str(&format!(
                "  {:<14} stream {:+6.1} dB{}  monitor {:+6.1} dB{}  vod {}  duck {}{}\n",
                c.name,
                c.mix.stream_gain_db,
                if c.mix.stream_muted { "M" } else { " " },
                c.mix.monitor_gain_db,
                if c.mix.monitor_muted { "M" } else { " " },
                if c.mix.vod { "on " } else { "off" },
                if c.mix.autoduck { "on " } else { "off" },
                if c.mix.solo { "  SOLO" } else { "" },
            ));
        }
        out
    }

    // -----------------------------------------------------------------------------------------
    // CLI dispatch (shared by the Unix socket)
    // -----------------------------------------------------------------------------------------

    pub fn handle_cli(&self, args: &[String]) -> (bool, String) {
        let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let need = |n: usize| -> Result<(), String> {
            if a.len() < n + 1 { Err(format!("{} needs {} argument(s)", a.first().unwrap_or(&""), n)) } else { Ok(()) }
        };
        let r: Reply = (|| match a.first().copied().unwrap_or("") {
            "--show" => {
                self.push(UiPush::ShowWindow);
                Ok("shown".into())
            }
            "--quit" => {
                self.push(UiPush::Quit);
                Ok("quitting".into())
            }
            "--toggle-mic-mute" => self.toggle_mic_mute(),
            "--set-mic-mute" => {
                need(1)?;
                self.set_mic_mute(parse_bool(a[1]).ok_or("expected on|off")?)
            }
            "--toggle-monitor-mute" => self.toggle_monitor_mute(),
            "--monitor-gain" => {
                need(1)?;
                let r = self.set_monitor_master("gain", a[1]);
                self.sync_ui();
                r
            }
            "--channel-mute" => {
                need(2)?;
                self.channel_mute(a[1], a[2])
            }
            "--channel-solo" => {
                need(1)?;
                self.channel_solo(a[1])
            }
            "--channel-gain" => {
                need(3)?;
                self.channel_gain(a[1], a[2], a[3])
            }
            "--cell-delay" => {
                need(3)?;
                self.cell_delay(a[1], a[2], a[3])
            }
            "--toggle-clipguard" => self.toggle_clipguard(),
            "--mic-monitor" => {
                // processed | raw | on | off  (what you hear of yourself in the headphones)
                need(1)?;
                let r = match a[1] {
                    "processed" => self.set_mic_option("monitor_raw", false),
                    "raw" => self.set_mic_option("monitor_raw", true),
                    "on" => self.set_mix("mic", "monitor_mute", "off"),
                    "off" => self.set_mix("mic", "monitor_mute", "on"),
                    _ => Err("expected processed|raw|on|off".into()),
                };
                self.sync_ui();
                r
            }
            "--toggle-autoduck" => {
                need(1)?;
                self.toggle_autoduck(a[1])
            }
            "--set-denoiser" => {
                need(1)?;
                let r = self.set_denoiser(a[1]);
                self.sync_ui();
                r
            }
            "--download-deepfilter" => self.engine.download_deepfilter(),
            "--set-preset" => {
                need(1)?;
                self.set_preset(&a[1..].join(" "))
            }
            "--set-theme" => {
                need(1)?;
                self.set_theme(a[1])
            }
            "--load-profile" => {
                need(1)?;
                self.load_profile(&a[1..].join(" "))
            }
            "--save-profile" => {
                need(1)?;
                self.save_profile(&a[1..].join(" "))
            }
            "--list-profiles" => Ok(StudioMicConfig::list_profiles().join("\n")),
            "--toggle-widget" | "--toggle-view" => self.set_widget(None),
            "--set-size" => {
                need(1)?;
                self.set_ui_size(a[1])
            }
            "--create-channel" => {
                need(1)?;
                self.create_channel(a[1], a.get(2).copied())
            }
            "--remove-channel" => {
                need(1)?;
                self.remove_channel(a[1])
            }
            "--rename-channel" => {
                need(2)?;
                self.rename_channel(a[1], a[2])
            }
            "--assign-app" => {
                need(2)?;
                self.assign_app(a[1], a[2])
            }
            "--unassign-app" => {
                need(2)?;
                self.unassign_app(a[1], a[2])
            }
            "--add-foreground-app" => {
                need(1)?;
                self.add_foreground_app(a[1])
            }
            "--preview" => {
                need(1)?;
                self.preview(a[1])
            }
            "--record-test" => self.record("start"),
            "--ui" => {
                // Sends one raw webview IPC message (scripting / tests): studiomic --ui 'eq:[...]'
                need(1)?;
                self.handle_ui(&a[1..].join(" "));
                Ok("sent".into())
            }
            "--status" => {
                if a.contains(&"--json") {
                    let v = self.status_json(a.contains(&"--runtime"));
                    Ok(serde_json::to_string_pretty(&v).unwrap_or_default())
                } else {
                    Ok(self.status_text())
                }
            }
            other => Err(format!("unknown command '{}' (see studiomic --help)", other)),
        })();
        match r {
            Ok(m) => (true, m),
            Err(e) => (false, e),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Webview IPC (colon-delimited messages from mic-studio-mockup.html)
    // -----------------------------------------------------------------------------------------

    pub fn handle_ui(&self, body: &str) {
        let (cmd, rest) = body.split_once(':').unwrap_or((body, ""));
        let parts: Vec<&str> = rest.splitn(3, ':').collect();
        let arg = |i: usize| parts.get(i).copied().unwrap_or("");
        let res: Reply = match cmd {
            // --- Mixer (Tab 1) ---
            "mix" => self.set_mix(arg(0), arg(1), arg(2)),
            "mic_option" => self.set_mic_option(arg(0), arg(1) == "1"),
            "monitor_master" => self.set_monitor_master(arg(0), arg(1)),
            "monitor_output" => self.set_monitor_output(rest),
            // Tab 1 is matrix-only; old UIs may still send this.
            "view_mode" => Ok(String::new()),
            "ui_size" => self.set_ui_size(rest),
            "toggle_widget_mode" => self.set_widget(if rest.is_empty() { None } else { Some(rest == "1") }),
            "always_on_top" => self.set_always_on_top(rest == "1"),
            "theme" => self.set_theme(rest),
            "last_tab" => self.mutate(|s| {
                s.last_tab = rest.to_string();
                Ok(String::new())
            }),
            "channel_add" => {
                #[derive(serde::Deserialize)]
                struct Add {
                    name: String,
                    #[serde(default)]
                    preset: Option<String>,
                    /// Hardware input for a Line In channel (node.name).
                    #[serde(default)]
                    hw_source: Option<String>,
                }
                match serde_json::from_str::<Add>(rest) {
                    Ok(a) => self.create_channel_with_input(&a.name, a.preset.as_deref(), a.hw_source.filter(|h| !h.is_empty())),
                    Err(_) => self.create_channel(rest, None),
                }
            }
            "channel_remove" => self.remove_channel(rest),
            // --- Custom mixes (routing matrix) ---
            "mix_add" => {
                #[derive(serde::Deserialize)]
                struct AddMix {
                    name: String,
                    #[serde(default)]
                    color: Option<String>,
                    #[serde(default)]
                    output: Option<String>,
                }
                match serde_json::from_str::<AddMix>(rest) {
                    Ok(a) => self.create_mix(&a.name, a.color, a.output),
                    Err(_) => self.create_mix(rest, None, None),
                }
            }
            "mix_remove" => self.remove_mix(rest),
            "mix_output" => self.set_mix_output(arg(0), rest.split_once(':').map(|x| x.1).unwrap_or("")),
            "channel_rename" => self.rename_channel(arg(0), &rest[arg(0).len().min(rest.len())..].trim_start_matches(':')),
            "channel_hw" => self.set_channel_hw(arg(0), &rest[arg(0).len().min(rest.len())..].trim_start_matches(':')),
            "channel_apps" => {
                #[derive(serde::Deserialize)]
                struct Apps {
                    id: String,
                    apps: Vec<String>,
                    #[serde(default)]
                    mode: Option<String>,
                }
                match serde_json::from_str::<Apps>(rest) {
                    Ok(x) => self.set_channel_apps(&x.id, x.apps, x.mode),
                    Err(e) => Err(e.to_string()),
                }
            }
            "assign_app" => self.assign_app(arg(1), arg(0)),
            "route_app" => self.assign_app(&rest.rsplit_once(':').map(|x| x.1).unwrap_or(""), rest.rsplit_once(':').map(|x| x.0).unwrap_or("")),
            "route_stream" => match (arg(0).parse::<u32>(), arg(1)) {
                (Ok(id), ch) => self.route_stream(id, ch),
                _ => Err("bad route_stream".into()),
            },
            "unpin_stream" => {
                if let Ok(id) = rest.parse::<u32>() {
                    StreamWatcher::unpin_stream(id);
                }
                Ok(String::new())
            }
            "get_live_streams" => {
                self.push_live_streams();
                Ok(String::new())
            }
            "rescan_devices" => {
                self.rescan_devices();
                Ok(String::new())
            }
            "mute" => {
                // legacy quick-mute pills: mute:<ch>:<1|0> mutes the channel everywhere
                let v = if arg(1) == "1" { "on" } else { "off" };
                let id = self.state.lock().unwrap().resolve_channel(arg(0)).unwrap_or_else(|| arg(0).to_string());
                let r = self.set_mix(&id, "stream_mute", v);
                if id != "mic" {
                    let _ = self.set_mix(&id, "monitor_mute", v);
                }
                r
            }
            // --- Mic & DSP (Tabs 2-3) ---
            "hardware_mic" => self.set_hw_mic(rest),
            "monitor_output_default" => self.set_monitor_output(""),
            "denoiser" => {
                let r = self.set_denoiser(rest);
                if let Err(ref e) = r {
                    self.js(format!("window.onDenoiserResult && window.onDenoiserResult(null, {})", serde_json::to_string(e).unwrap()));
                }
                r
            }
            "denoiser_download" => self.engine.download_deepfilter(),
            "denoiser_param" => match arg(1).parse::<f32>() {
                Ok(v) => self.set_denoiser_param(arg(0), v),
                Err(e) => Err(e.to_string()),
            },
            "noise_clean" => match serde_json::from_str::<config::NoiseCleanConfig>(rest) {
                Ok(nc) => self.set_noise_clean(nc),
                Err(e) => Err(e.to_string()),
            },
            "toggle_compressor" => {
                let on = rest.ends_with('1');
                let mut nc = self.state.lock().unwrap().noise_clean.clone();
                nc.compressor_enabled = on;
                self.set_noise_clean(nc)
            }
            "gate_autoset" => self.gate_autoset(),
            "eq" | "save_default_eq" => {
                #[derive(serde::Deserialize)]
                struct Payload {
                    bands: Vec<EqBand>,
                    mic_id: Option<String>,
                    preset_name: Option<String>,
                }
                if let Ok(p) = serde_json::from_str::<Payload>(rest) {
                    self.set_eq(p.bands, p.preset_name, p.mic_id)
                } else if let Ok(b) = parse_client_bands(rest) {
                    self.set_eq(b, None, None)
                } else {
                    Err("bad EQ payload".into())
                }
            }
            "profile" => match MicDatabase::find_by_id(rest) {
                Some(m) => self.set_eq(m.bands.clone(), Some(format!("Recommended EQ for {} {}", m.brand, m.model)), Some(m.id.clone())),
                None => Err("unknown mic".into()),
            },
            "reset_default_eq" => self.set_preset("am8"),
            "ab_compare" => {
                // A/B audition: live-only bypass of every stage (raw capsule); not persisted.
                let raw = rest == "raw";
                let e = self.engine.clone();
                std::thread::spawn(move || e.ab_bypass(raw));
                Ok(String::new())
            }
            "calib_trim" => {
                let v = rest.parse::<f32>().ok().filter(|v| v.is_finite()).unwrap_or(0.0);
                let r = self.mutate(|s| {
                    s.calibration.preamp_gain_db = v.clamp(-24.0, 12.0);
                    Ok(String::new())
                });
                self.enqueue(Job::Pre);
                r
            }
            "save_calibration" => {
                #[derive(serde::Deserialize)]
                struct Calib {
                    preamp_gain_db: f32,
                    calibrated_mic_id: Option<String>,
                    voice_warmth_db: Option<f32>,
                    voice_presence_db: Option<f32>,
                    bands: Option<Vec<EqBand>>,
                    profile_name: Option<String>,
                }
                match serde_json::from_str::<Calib>(rest) {
                    Ok(c) => {
                        let r = self.mutate(|s| {
                            s.calibration.preamp_gain_db = c.preamp_gain_db.clamp(-24.0, 12.0);
                            if let Some(m) = c.calibrated_mic_id.clone() {
                                s.calibration.calibrated_mic_id = m;
                            }
                            if let Some(w) = c.voice_warmth_db {
                                s.calibration.voice_warmth_db = w;
                            }
                            if let Some(p) = c.voice_presence_db {
                                s.calibration.voice_presence_db = p;
                            }
                            if let (Some(name), Some(bands)) = (c.profile_name.clone(), c.bands.clone()) {
                                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
                                s.user_presets.retain(|p| p.title != name);
                                s.user_presets.insert(0, config::UserPreset {
                                    id: now,
                                    title: name.clone(),
                                    mic: c.calibrated_mic_id.clone().unwrap_or_else(|| s.default_mic_id.clone()),
                                    date: "Calibrated Profile".into(),
                                    bands: bands.clone(),
                                    is_custom: true,
                                    summary: None,
                                });
                                s.preset_name = name;
                                s.eq_bands = bands.into_iter().take(10).collect();
                            }
                            Ok("Calibration saved".into())
                        });
                        self.enqueue(Job::Pre);
                        self.enqueue(Job::Eq);
                        r
                    }
                    Err(e) => Err(e.to_string()),
                }
            }
            "save_user_preset" => match serde_json::from_str::<config::UserPreset>(rest) {
                Ok(p) => {
                    let r = self.mutate(|s| {
                        if !p.bands.is_empty() {
                            s.eq_bands = p.bands.clone().into_iter().take(10).collect();
                        }
                        s.preset_name = format!("Custom: {}", p.title);
                        s.user_presets.retain(|x| x.id != p.id && x.title != p.title);
                        s.user_presets.insert(0, p);
                        Ok("Preset saved".into())
                    });
                    self.enqueue(Job::Eq);
                    r
                }
                Err(e) => Err(e.to_string()),
            },
            "delete_user_preset" => {
                let id = rest.parse::<u64>().unwrap_or(0);
                self.mutate(|s| {
                    s.user_presets.retain(|p| p.id != id);
                    Ok(String::new())
                })
            }
            // --- Dual mic ---
            "dual_mic" => {
                let on = rest == "1";
                let r = self.mutate(|s| {
                    s.dual_mic.enabled = on;
                    Ok(String::new())
                });
                self.enqueue(Job::Mic2);
                r
            }
            "hardware_mic2" => {
                let r = self.mutate(|s| {
                    s.dual_mic.hw_mic = Some(rest.to_string());
                    Ok(String::new())
                });
                self.enqueue(Job::Relink);
                r
            }
            "profile_mic2" => match MicDatabase::find_by_id(rest) {
                Some(m) => {
                    let r = self.mutate(|s| {
                        s.dual_mic.mic_id = m.id.clone();
                        s.dual_mic.preset_name = format!("{} {}", m.brand, m.model);
                        s.dual_mic.eq_bands = m.bands.clone();
                        Ok(String::new())
                    });
                    self.enqueue(Job::Eq);
                    r
                }
                None => Err("unknown mic".into()),
            },
            "eq_mic2" | "save_default_eq_mic2" => match parse_client_bands(rest) {
                Ok(b) => {
                    let r = self.mutate(|s| {
                        s.dual_mic.eq_bands = b.into_iter().take(10).collect();
                        Ok(String::new())
                    });
                    self.enqueue(Job::Eq);
                    r
                }
                Err(e) => Err(e),
            },
            // --- Preview / recorder ---
            "preview" => self.preview(rest),
            "record" => self.record(rest),
            // --- Profiles ---
            "profile_save" => self.save_profile(rest),
            "profile_load" => self.load_profile(rest),
            "profiles_list" => {
                self.js(format!("window.updateProfilesList && window.updateProfilesList({})", serde_json::to_string(&StudioMicConfig::list_profiles()).unwrap()));
                Ok(String::new())
            }
            // --- App prefs ---
            "toggle_autostart" => {
                let on = rest.ends_with('1');
                let r = self.mutate(|s| {
                    s.autostart = on;
                    // Honour the "Start Minimized" choice instead of always adding --background.
                    let _ = crate::autostart::AutostartManager::set_enabled_with_minimized(on, s.autostart_minimized);
                    Ok(String::new())
                });
                self.push(UiPush::TrayRefresh);
                r
            }
            "toggle_start_minimized" | "toggle_autostart_minimized" => {
                let on = rest.ends_with('1');
                let r = self.mutate(|s| {
                    s.start_minimized = on;
                    s.autostart_minimized = on;
                    let _ = crate::autostart::AutostartManager::set_enabled_with_minimized(s.autostart, on);
                    Ok(String::new())
                });
                self.push(UiPush::TrayRefresh);
                r
            }
            "hide_window" => {
                self.push(UiPush::ToggleWindow);
                Ok(String::new())
            }
            // Legacy no-ops kept so older UI builds don't error.
            "sidetone" => self.set_mix("mic", "monitor_mute", if rest.ends_with('1') { "off" } else { "on" }),
            "monitor_channel" => self.set_mix(arg(0), "monitor_mute", if arg(1) == "1" { "off" } else { "on" }),
            "volume" => {
                let db = lin_to_db(arg(1).parse::<f32>().unwrap_or(1.0));
                self.set_mix(arg(0), "stream_gain", &format!("={}", db))
            }
            "update_channels" | "save_default_channels" | "reset_default_channels" | "route_capture_card" | "linein_route" | "channel1_mic" => {
                self.legacy_channel_message(cmd, rest)
            }
            _ => Err(format!("unhandled UI message '{}'", cmd)),
        };
        if let Err(e) = res {
            eprintln!("UI message '{}' failed: {}", cmd, e);
            self.notice(e.clone());
        }
    }

    fn legacy_channel_message(&self, cmd: &str, rest: &str) -> Reply {
        match cmd {
            "reset_default_channels" => {
                let r = self.mutate(|s| {
                    s.virtual_channels = config::default_channels();
                    Ok(String::new())
                });
                self.enqueue(Job::Channels);
                self.sync_ui();
                r
            }
            "route_capture_card" => {
                let parts: Vec<&str> = rest.splitn(2, ':').collect();
                if parts.first() == Some(&"scan") {
                    self.rescan_devices();
                    return Ok(String::new());
                }
                let src = parts.first().copied().unwrap_or("");
                let ch = parts.get(1).copied().unwrap_or("capture");
                self.set_channel_hw(ch, src)
            }
            "linein_route" => {
                let (ch, hw) = rest.split_once(':').unwrap_or(("", ""));
                self.set_channel_hw(ch, hw)
            }
            _ => Ok(String::new()),
        }
    }

    pub fn push_live_streams(&self) {
        self.push_live_streams_data(&StreamWatcher::scan_live_streams(), &StreamWatcher::scan_active_browser_tabs());
    }

    /// Pushes an already-scanned stream / browser-tab list (avoids scanning twice).
    pub fn push_live_streams_data(
        &self,
        streams: &[crate::routing::wireplumber_hook::HookedStream],
        tabs: &[crate::routing::wireplumber_hook::ActiveBrowserTab],
    ) {
        self.js(format!(
            "window.updateLiveStreams && window.updateLiveStreams({}); window.updateBrowserTabs && window.updateBrowserTabs({})",
            serde_json::to_string(&streams).unwrap_or_default(),
            serde_json::to_string(&tabs).unwrap_or_default()
        ));
        self.push(UiPush::LiveStreams(streams.to_vec()));
    }
}

/// UI band format: { type: "Peaking", freq, gain, q } with numbers or strings.
pub fn parse_client_bands(json_str: &str) -> Result<Vec<EqBand>, String> {
    let v: Vec<serde_json::Value> = serde_json::from_str(json_str).map_err(|e| e.to_string())?;
    let num = |x: Option<&serde_json::Value>, d: f32| -> f32 {
        match x {
            Some(serde_json::Value::Number(n)) => n.as_f64().map(|f| f as f32).unwrap_or(d),
            Some(serde_json::Value::String(s)) => s.parse().unwrap_or(d),
            _ => d,
        }
    };
    Ok(v.iter()
        .map(|b| {
            let t = b.get("type").or_else(|| b.get("filter_type")).and_then(|t| t.as_str()).unwrap_or("Peaking").to_lowercase();
            let filter_type = match t.replace(' ', "").as_str() {
                "highpass" | "hp" => FilterType::HighPass,
                "lowpass" | "lp" => FilterType::LowPass,
                "lowshelf" => FilterType::LowShelf,
                "highshelf" => FilterType::HighShelf,
                "notch" => FilterType::Notch,
                _ => FilterType::Peaking,
            };
            EqBand {
                freq: num(b.get("freq"), 1000.0),
                gain_db: num(b.get("gain").or_else(|| b.get("gain_db")), 0.0),
                q: num(b.get("q"), 1.0),
                filter_type,
            }
        })
        .collect())
}

/// (pid, class/app name) of the focused window. Hyprland first, then sway, then X11.
fn foreground_app() -> Option<(Option<u32>, Option<String>)> {
    if let Ok(o) = std::process::Command::new("hyprctl").args(["activewindow", "-j"]).output() {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&o.stdout) {
            let pid = v.get("pid").and_then(|p| p.as_u64()).map(|p| p as u32);
            let class = v.get("class").and_then(|c| c.as_str()).map(|c| c.to_lowercase());
            if pid.is_some() || class.is_some() {
                return Some((pid, class));
            }
        }
    }
    if let Ok(o) = std::process::Command::new("swaymsg").args(["-t", "get_tree"]).output() {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&o.stdout) {
            fn find(n: &serde_json::Value) -> Option<&serde_json::Value> {
                if n.get("focused").and_then(|f| f.as_bool()) == Some(true) {
                    return Some(n);
                }
                for k in ["nodes", "floating_nodes"] {
                    if let Some(arr) = n.get(k).and_then(|a| a.as_array()) {
                        for c in arr {
                            if let Some(f) = find(c) {
                                return Some(f);
                            }
                        }
                    }
                }
                None
            }
            if let Some(f) = find(&v) {
                let pid = f.get("pid").and_then(|p| p.as_u64()).map(|p| p as u32);
                let app = f.get("app_id").and_then(|a| a.as_str()).map(|a| a.to_lowercase());
                return Some((pid, app));
            }
        }
    }
    if let Ok(o) = std::process::Command::new("xdotool").args(["getactivewindow", "getwindowpid"]).output() {
        if let Ok(pid) = String::from_utf8_lossy(&o.stdout).trim().parse::<u32>() {
            return Some((Some(pid), None));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_spec_relative_and_absolute() {
        assert_eq!(parse_gain_spec("+2dB", -1.0), Some(1.0));
        assert_eq!(parse_gain_spec("-3.5dB", 0.0), Some(-3.5));
        assert_eq!(parse_gain_spec("=-6", 3.0), Some(-6.0));
        assert_eq!(parse_gain_spec("exact:-12", 0.0), Some(-12.0));
        assert_eq!(parse_gain_spec("4", -10.0), Some(4.0));
        assert_eq!(parse_gain_spec("+40", 0.0), Some(12.0));
        assert_eq!(parse_gain_spec("abc", 0.0), None);
    }

    #[test]
    fn client_bands_accept_strings() {
        let b = parse_client_bands(r#"[{"type":"High Pass","freq":"85","gain":0,"q":"0.7"}]"#).unwrap();
        assert_eq!(b[0].filter_type, FilterType::HighPass);
        assert_eq!(b[0].freq, 85.0);
    }
}
