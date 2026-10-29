use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::collections::HashSet;
use std::path::PathBuf;
use crate::models::microphone::EqBand;
use crate::config::{VirtualChannelConfig, NoiseCleanConfig};

pub struct PipeWireDaemon {
    child_process: Arc<Mutex<Option<Child>>>,
    config_path: PathBuf,
    current_hw_mic: Arc<Mutex<Option<String>>>,
    sidetone_active: Arc<Mutex<bool>>,
    monitored_channels: Arc<Mutex<HashSet<String>>>,
    current_bands: Arc<Mutex<Vec<EqBand>>>,
    current_channels: Arc<Mutex<Vec<VirtualChannelConfig>>>,
    pub current_mic2_bands: Arc<Mutex<Option<Vec<EqBand>>>>,
    pub current_mic2_hw: Arc<Mutex<Option<String>>>,
    pub dual_mic_enabled: Arc<Mutex<bool>>,
    pub current_noise_clean: Arc<Mutex<NoiseCleanConfig>>,
}

impl PipeWireDaemon {
    pub fn new(
        config_path: PathBuf,
        initial_bands: Vec<EqBand>,
        initial_channels: Vec<VirtualChannelConfig>,
        mic2_bands: Option<Vec<EqBand>>,
        dual_mic_enabled: bool,
        noise_clean: NoiseCleanConfig,
    ) -> Self {
        Self {
            child_process: Arc::new(Mutex::new(None)),
            config_path,
            current_hw_mic: Arc::new(Mutex::new(None)),
            sidetone_active: Arc::new(Mutex::new(false)),
            monitored_channels: Arc::new(Mutex::new(HashSet::new())),
            current_bands: Arc::new(Mutex::new(initial_bands)),
            current_channels: Arc::new(Mutex::new(initial_channels)),
            current_mic2_bands: Arc::new(Mutex::new(mic2_bands)),
            current_mic2_hw: Arc::new(Mutex::new(None)),
            dual_mic_enabled: Arc::new(Mutex::new(dual_mic_enabled)),
            current_noise_clean: Arc::new(Mutex::new(noise_clean)),
        }
    }

    /// Sets chosen physical microphone 1 and links it
    pub fn set_hw_mic(&self, name: String) {
        *self.current_hw_mic.lock().unwrap() = Some(name.clone());
        let _ = crate::audio::device_discovery::DeviceDiscovery::link_source_to_studiomic(&name);
    }

    /// Sets chosen physical microphone 2 and links it
    pub fn set_mic2_hw(&self, name: String) {
        *self.current_mic2_hw.lock().unwrap() = Some(name.clone());
        if *self.dual_mic_enabled.lock().unwrap() {
            let _ = crate::audio::device_discovery::DeviceDiscovery::link_source_to_studiomic_target(
                &name,
                "studiomic_mic2.capture",
            );
        }
    }

    /// Spawns the PipeWire filter-chain and virtual inputs instance, cleaning up any stale instances first
    pub fn start(&self) -> std::io::Result<()> {
        let mut proc_lock = self.child_process.lock().unwrap();
        if proc_lock.is_some() {
            println!("StudioMic PipeWire Daemon is already running.");
            return Ok(());
        }

        // Ensure no orphaned studiomic-graph.conf PipeWire processes exist from prior runs
        let _ = Command::new("pkill")
            .args(["-9", "-f", "studiomic-graph.conf"])
            .output();
        std::thread::sleep(std::time::Duration::from_millis(60));

        println!("Spawning StudioMic PipeWire virtual audio engine with config: {:?}", self.config_path);
        let child = Command::new("pipewire")
            .arg("-c")
            .arg(&self.config_path)
            .spawn()?;

        *proc_lock = Some(child);
        println!("StudioMic PipeWire engine spawned successfully.");
        Ok(())
    }

    /// Stops the child PipeWire engine cleanly
    pub fn stop(&self) {
        let mut proc_lock = self.child_process.lock().unwrap();
        if let Some(mut child) = proc_lock.take() {
            println!("Stopping StudioMic PipeWire engine...");
            let _ = child.kill();
            let _ = child.wait();
            println!("StudioMic PipeWire engine stopped.");
        }
        let _ = Command::new("pkill")
            .args(["-9", "-f", "studiomic-graph.conf"])
            .output();
    }

    /// Atomically reloads the PipeWire filter-chain with updated EQ bands, mic2 bands, noise clean, and/or channels
    pub fn reload_graph(
        &self,
        new_bands: Option<&[EqBand]>,
        new_channels: Option<&[VirtualChannelConfig]>,
        new_mic2_bands: Option<Option<&[EqBand]>>,
        new_dual_mic: Option<bool>,
    ) -> std::io::Result<()> {
        if let Some(b) = new_bands {
            *self.current_bands.lock().unwrap() = b.to_vec();
        }
        if let Some(c) = new_channels {
            *self.current_channels.lock().unwrap() = c.to_vec();
        }
        if let Some(m2) = new_mic2_bands {
            *self.current_mic2_bands.lock().unwrap() = m2.map(|b| b.to_vec());
        }
        if let Some(dm) = new_dual_mic {
            *self.dual_mic_enabled.lock().unwrap() = dm;
        }

        let bands = self.current_bands.lock().unwrap().clone();
        let channels = self.current_channels.lock().unwrap().clone();
        let is_dual = *self.dual_mic_enabled.lock().unwrap();
        let mic2_bands_guard = self.current_mic2_bands.lock().unwrap().clone();
        let noise_clean = self.current_noise_clean.lock().unwrap().clone();

        let mic2_opt = if is_dual {
            mic2_bands_guard.as_deref()
        } else {
            None
        };

        let _ = crate::audio::filter_chain::FilterChainConfigGenerator::write_config_to_runtime(
            &bands,
            &channels,
            mic2_opt,
            Some(&noise_clean),
            "studiomic-graph.conf",
        )?;
        self.stop();
        std::thread::sleep(std::time::Duration::from_millis(80));
        self.start()?;
        std::thread::sleep(std::time::Duration::from_millis(320));

        // Relink physical microphone 1
        let chosen = self.current_hw_mic.lock().unwrap().clone();
        if let Some(ref hw) = chosen {
            let _ = crate::audio::device_discovery::DeviceDiscovery::link_source_to_studiomic(hw);
        } else {
            let mics = crate::audio::device_discovery::DeviceDiscovery::get_input_devices();
            let target = mics.iter().find(|d| d.is_default).or_else(|| mics.first());
            if let Some(hw) = target {
                let _ = crate::audio::device_discovery::DeviceDiscovery::link_source_to_studiomic(&hw.name);
            }
        }

        // Relink physical microphone 2 if dual mic is active
        if is_dual {
            let chosen_m2 = self.current_mic2_hw.lock().unwrap().clone();
            if let Some(ref hw2) = chosen_m2 {
                let _ = crate::audio::device_discovery::DeviceDiscovery::link_source_to_studiomic_target(
                    hw2,
                    "studiomic_mic2.capture",
                );
            }
        }

        // Relink sidetone and any monitored mixer channels
        if *self.sidetone_active.lock().unwrap() {
            Self::set_channel_monitor_internal("mic", true);
        }
        let monitored: Vec<String> = self.monitored_channels.lock().unwrap().iter().cloned().collect();
        for ch_id in monitored {
            Self::set_channel_monitor_internal(&ch_id, true);
        }

        // Re-route assigned app streams after graph reload
        crate::routing::wireplumber_hook::StreamWatcher::auto_route_assigned_channels();

        println!("Successfully reloaded PipeWire engine: Mic 1, Mic 2 active: {}, {} virtual channels", is_dual, channels.len());
        Ok(())
    }

    /// Reloads the PipeWire filter-chain with updated EQ bands in real time
    pub fn reload_bands(&self, bands: &[EqBand]) -> std::io::Result<()> {
        self.reload_graph(Some(bands), None, None, None)
    }

    /// Reloads Mic 2 EQ bands
    pub fn reload_mic2_bands(&self, bands: &[EqBand]) -> std::io::Result<()> {
        self.reload_graph(None, None, Some(Some(bands)), None)
    }

    /// Reloads Noise Clean DSP settings (Tab 3)
    pub fn reload_noise_clean(&self, nc: &NoiseCleanConfig) -> std::io::Result<()> {
        *self.current_noise_clean.lock().unwrap() = nc.clone();
        self.reload_graph(None, None, None, None)
    }

    /// Applies live calibration preamp gain trim in dB (-15.0 to +15.0 dB) to the processed microphone
    pub fn set_preamp_trim(gain_db: f32) {
        let clamped_db = gain_db.clamp(-24.0, 12.0);
        let linear = 10.0_f32.powf(clamped_db / 20.0);
        let pct = (linear * 100.0).round().max(5.0) as u32;
        let _ = Command::new("pactl")
            .args(["set-source-volume", "studiomic_processed_mic", &format!("{}%", pct)])
            .spawn();
        println!("Applied VoiceMatch Calibration preamp trim: {:.1} dB ({}%)", clamped_db, pct);
    }

    /// Toggles dual mic setup
    pub fn set_dual_mic(&self, enabled: bool) -> std::io::Result<()> {
        self.reload_graph(None, None, None, Some(enabled))
    }

    /// Reloads the PipeWire engine with updated virtual mixer channels
    pub fn reload_channels(&self, channels: &[VirtualChannelConfig]) -> std::io::Result<()> {
        self.reload_graph(None, Some(channels), None, None)
    }

    /// Adjust volume of a virtual input source or mic using pactl
    pub fn set_volume(target_node_name: &str, volume: f32) {
        let node = if target_node_name.starts_with("studiomic_") {
            target_node_name.to_string()
        } else {
            match target_node_name {
                "ch1" | "mic" => "studiomic_processed_mic".to_string(),
                "ch2" | "game" => "studiomic_game".to_string(),
                "ch3" | "voice" => "studiomic_voice".to_string(),
                "ch4" | "music" => "studiomic_music".to_string(),
                "ch5" | "alerts" => "studiomic_alerts".to_string(),
                "mic2" => "studiomic_processed_mic2".to_string(),
                other => format!("studiomic_{}", other),
            }
        };
        let pct = (volume * 100.0).round().max(0.0) as u32;
        let _ = Command::new("pactl")
            .args(["set-source-volume", &node, &format!("{}%", pct)])
            .spawn();
    }

    /// Toggle mute on a virtual input source or mic using pactl
    pub fn set_mute(target_node_name: &str, mute: bool) {
        let node = if target_node_name.starts_with("studiomic_") {
            target_node_name.to_string()
        } else {
            match target_node_name {
                "ch1" | "mic" => "studiomic_processed_mic".to_string(),
                "ch2" | "game" => "studiomic_game".to_string(),
                "ch3" | "voice" => "studiomic_voice".to_string(),
                "ch4" | "music" => "studiomic_music".to_string(),
                "ch5" | "alerts" => "studiomic_alerts".to_string(),
                "mic2" => "studiomic_processed_mic2".to_string(),
                other => format!("studiomic_{}", other),
            }
        };
        let flag = if mute { "1" } else { "0" };
        let _ = Command::new("pactl")
            .args(["set-source-mute", &node, flag])
            .spawn();
    }

    /// Toggles zero-latency hardware sidetone (monitoring mic in headphones) and records state
    pub fn set_sidetone(&self, enable: bool) {
        *self.sidetone_active.lock().unwrap() = enable;
        self.set_channel_monitor("mic", enable);
    }

    /// Toggles real-time headphone monitoring for any mixer channel ("mic", "mic2", "game", "voice", "music", "alerts", etc.)
    pub fn set_channel_monitor(&self, ch_id: &str, enable: bool) {
        let ch_clean = match ch_id {
            "ch1" | "mic" | "studiomic_processed_mic" => "mic",
            "ch2" | "game" | "studiomic_game" => "game",
            "ch3" | "voice" | "studiomic_voice" => "voice",
            "ch4" | "music" | "studiomic_music" => "music",
            "ch5" | "alerts" | "studiomic_alerts" => "alerts",
            "mic2" | "studiomic_processed_mic2" => "mic2",
            other => other.trim_start_matches("studiomic_"),
        };

        if ch_clean == "mic" {
            *self.sidetone_active.lock().unwrap() = enable;
        }
        {
            let mut set = self.monitored_channels.lock().unwrap();
            if enable {
                set.insert(ch_clean.to_string());
            } else {
                set.remove(ch_clean);
            }
        }
        Self::set_channel_monitor_internal(ch_clean, enable);
    }

    fn set_channel_monitor_internal(ch_clean: &str, enable: bool) {
        let default_sink = Self::get_default_sink_name()
            .unwrap_or_else(|| "alsa_output.usb-FiiO_FIIO_BTR13_ABCDEF0123456789-00.analog-stereo".into());
        let sink_fl = format!("{}:playback_FL", default_sink);
        let sink_fr = format!("{}:playback_FR", default_sink);

        let src_node_name = match ch_clean {
            "mic" | "ch1" => "studiomic_processed_mic".to_string(),
            "mic2" => "studiomic_processed_mic2".to_string(),
            other => format!("studiomic_{}", other),
        };

        if ch_clean == "mic" || ch_clean == "ch1" || ch_clean == "mic2" {
            let src_mono = format!("{}:capture_MONO", src_node_name);
            if enable {
                let _ = Command::new("pw-link").args([&src_mono, &sink_fl]).output();
                let _ = Command::new("pw-link").args([&src_mono, &sink_fr]).output();
                println!("Real-time Monitoring ENABLED for '{}' -> {}", src_node_name, default_sink);
            } else {
                let _ = Command::new("pw-link").args(["-d", &src_mono, &sink_fl]).output();
                let _ = Command::new("pw-link").args(["-d", &src_mono, &sink_fr]).output();
                println!("Real-time Monitoring DISABLED for '{}'", src_node_name);
            }
        } else {
            let src_fl = format!("{}:capture_FL", src_node_name);
            let src_fr = format!("{}:capture_FR", src_node_name);
            if enable {
                let _ = Command::new("pw-link").args([&src_fl, &sink_fl]).output();
                let _ = Command::new("pw-link").args([&src_fr, &sink_fr]).output();
                println!("Real-time Channel Monitor ENABLED for '{}' -> {}", src_node_name, default_sink);
            } else {
                let _ = Command::new("pw-link").args(["-d", &src_fl, &sink_fl]).output();
                let _ = Command::new("pw-link").args(["-d", &src_fr, &sink_fr]).output();
                println!("Real-time Channel Monitor DISABLED for '{}'", src_node_name);
            }
        }
    }

    /// Helper to find default audio output sink
    fn get_default_sink_name() -> Option<String> {
        let output = Command::new("wpctl").args(["inspect", "@DEFAULT_AUDIO_SINK@"]).output().ok()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.contains("node.name =") {
                let parts: Vec<&str> = line.split('"').collect();
                if parts.len() >= 2 {
                    return Some(parts[1].to_string());
                }
            }
        }
        None
    }

    /// Spawns a background real-time peak detector reading PCM samples via pw-record
    pub fn start_peak_detector<F: Fn(f32) + Send + 'static>(callback: F) {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut child = match Command::new("pw-record")
                .args(["--channels", "1", "--rate", "8000", "--format", "s16", "-"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Failed to spawn pw-record for peak detection: {}", e);
                    return;
                }
            };

            if let Some(mut stdout) = child.stdout.take() {
                let mut buf = [0u8; 800];
                loop {
                    match stdout.read_exact(&mut buf) {
                        Ok(_) => {
                            let mut max_abs: i16 = 0;
                            for chunk in buf.chunks_exact(2) {
                                let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
                                let abs_s = sample.saturating_abs();
                                if abs_s > max_abs {
                                    max_abs = abs_s;
                                }
                            }
                            let normalized = (max_abs as f32) / 32768.0;
                            let db = if normalized > 0.0001 {
                                20.0 * normalized.log10()
                            } else {
                                -80.0
                            };
                            callback(db);
                        }
                        Err(_) => {
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                    }
                }
            }
        });
    }

    /// Spawns a background real-time peak detector reading PCM samples for all 5 mixer channels
    pub fn start_multichannel_peak_detector<F: Fn(std::collections::HashMap<String, f32>) + Send + 'static>(callback: F) {
        use std::collections::HashMap;
        use std::io::Read;
        use std::sync::{Arc, Mutex};

        let channels = [
            ("mic", "studiomic_processed_mic"),
            ("game", "studiomic_game"),
            ("voice", "studiomic_voice"),
            ("music", "studiomic_music"),
            ("alerts", "studiomic_alerts"),
        ];

        let peaks_map: Arc<Mutex<HashMap<String, f32>>> = Arc::new(Mutex::new(HashMap::new()));

        for (ch_key, target_node) in channels {
            let ch_name = ch_key.to_string();
            let target = target_node.to_string();
            let map_clone = Arc::clone(&peaks_map);

            std::thread::spawn(move || {
                loop {
                    let mut child = match Command::new("pw-record")
                        .args(["--target", &target, "--channels", "1", "--rate", "8000", "--format", "s16", "-"])
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                    {
                        Ok(c) => c,
                        Err(e) => {
                            eprintln!("Failed to spawn pw-record for channel {}: {}", ch_name, e);
                            std::thread::sleep(std::time::Duration::from_millis(1000));
                            continue;
                        }
                    };

                    if let Some(mut stdout) = child.stdout.take() {
                        let mut buf = [0u8; 800];
                        loop {
                            match stdout.read_exact(&mut buf) {
                                Ok(_) => {
                                    let mut max_abs: i16 = 0;
                                    for chunk in buf.chunks_exact(2) {
                                        let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
                                        let abs_s = sample.saturating_abs();
                                        if abs_s > max_abs {
                                            max_abs = abs_s;
                                        }
                                    }
                                    let normalized = (max_abs as f32) / 32768.0;
                                    let db = if normalized > 0.0001 {
                                        20.0 * normalized.log10()
                                    } else {
                                        -80.0
                                    };
                                    if let Ok(mut m) = map_clone.lock() {
                                        m.insert(ch_name.clone(), db);
                                    }
                                }
                                Err(_) => {
                                    // Stream closed (e.g., graph reloaded); kill old child and respawn
                                    let _ = child.kill();
                                    let _ = child.wait();
                                    break;
                                }
                            }
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(400));
                }
            });
        }

        // Broadcaster thread sending combined channel peaks every 60ms
        let map_broadcast = Arc::clone(&peaks_map);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(60));
                let snapshot = {
                    if let Ok(m) = map_broadcast.lock() {
                        m.clone()
                    } else {
                        HashMap::new()
                    }
                };
                if !snapshot.is_empty() {
                    callback(snapshot);
                }
            }
        });
    }
}
