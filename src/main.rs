mod audio;
mod autostart;
mod config;
mod models;
mod routing;
mod tray;

use models::microphone::MicDatabase;
use tao::{
    event::{Event, StartCause, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
};
use wry::WebViewBuilder;

#[derive(Debug, Clone)]
pub enum AppEvent {
    Peak(f32),
    ChannelPeaks(String),
    LiveStreams(String),
    BrowserTabs(String),
    ShowWindow,
    ToggleWindow,
    Quit,
    SetMicMute(bool),
    SetMonitoring(bool),
    SetCompressor(bool),
    SetDeepFilter(bool),
    SetChannelMute(String, bool),
    ToggleAutostart(bool),
}

fn parse_client_bands(json_str: &str) -> Result<Vec<models::microphone::EqBand>, Box<dyn std::error::Error>> {
    #[derive(serde::Deserialize)]
    struct ClientBand {
        #[serde(rename = "type")]
        filter_type: String,
        #[serde(deserialize_with = "de_flexible_float")]
        freq: f32,
        #[serde(deserialize_with = "de_flexible_float")]
        gain: f32,
        #[serde(deserialize_with = "de_flexible_float")]
        q: f32,
    }
    fn de_flexible_float<'de, D>(deserializer: D) -> Result<f32, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        use serde::Deserialize;
        let v = serde_json::Value::deserialize(deserializer)?;
        match v {
            serde_json::Value::Number(n) => n.as_f64().map(|f| f as f32).ok_or_else(|| D::Error::custom("invalid float")),
            serde_json::Value::String(s) => s.parse::<f32>().map_err(D::Error::custom),
            _ => Err(D::Error::custom("expected number or string")),
        }
    }
    let client_bands = serde_json::from_str::<Vec<ClientBand>>(json_str)?;
    let eq_bands = client_bands.into_iter().map(|b| {
        let ft = match b.filter_type.to_lowercase().as_str() {
            "high pass" | "highpass" => models::microphone::FilterType::HighPass,
            "low pass" | "lowpass" => models::microphone::FilterType::LowPass,
            "low shelf" | "lowshelf" => models::microphone::FilterType::LowShelf,
            "high shelf" | "highshelf" => models::microphone::FilterType::HighShelf,
            "notch" => models::microphone::FilterType::Notch,
            _ => models::microphone::FilterType::Peaking,
        };
        models::microphone::EqBand {
            freq: b.freq,
            gain_db: b.gain,
            q: b.q,
            filter_type: ft,
        }
    }).collect();
    Ok(eq_bands)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting StudioMic (Native Wry / WebKit Engine)...");

    // Load persistent user configuration
    let persistent_config = config::StudioMicConfig::load();

    // Discover physical hardware microphones
    let hardware_mics = audio::device_discovery::DeviceDiscovery::get_input_devices();
    println!("Discovered {} physical audio input devices:", hardware_mics.len());
    for mic in &hardware_mics {
        println!("  - [{}] {} (Default: {})", mic.id, mic.description, mic.is_default);
    }

    // Discover active audio streams in real time
    let active_streams = routing::wireplumber_hook::StreamWatcher::scan_live_streams();
    println!("Discovered {} active PipeWire audio output streams:", active_streams.len());
    for s in &active_streams {
        println!("  - Node #{}: App '{}' (PID {}) | Media: '{}'", s.node_id, s.app_name, s.pid, s.media_name);
    }

    // Scan installed media players & applications
    let installed_players = routing::wireplumber_hook::StreamWatcher::scan_installed_media_players();
    println!("Discovered {} installed common Linux media players:", installed_players.len());
    for p in &installed_players {
        println!("  - [{}] {}", p.id, p.name);
    }

    let installed_browsers = routing::wireplumber_hook::StreamWatcher::scan_installed_web_browsers();
    println!("Discovered {} installed web browsers:", installed_browsers.len());
    for b in &installed_browsers {
        println!("  - [{}] {}", b.id, b.name);
    }

    let active_browser_tabs = routing::wireplumber_hook::StreamWatcher::scan_active_browser_tabs();
    println!("Discovered {} active web browser audio tabs.", active_browser_tabs.len());

    let installed_apps = routing::wireplumber_hook::StreamWatcher::scan_installed_apps();
    println!("Discovered {} installed .desktop applications.", installed_apps.len());

    // Determine target hardware mic 1 to bind
    let target_mic_name = persistent_config.active_hw_mic.clone().or_else(|| {
        hardware_mics.iter().find(|d| d.is_default).map(|d| d.name.clone())
            .or_else(|| hardware_mics.first().map(|d| d.name.clone()))
    });

    let target_mic_desc = hardware_mics.iter()
        .find(|d| Some(&d.name) == target_mic_name.as_ref())
        .map(|d| d.description.as_str())
        .unwrap_or("Default System Microphone");

    // Initialize PipeWire daemon
    let mic2_bands_initial = if persistent_config.dual_mic.enabled {
        Some(persistent_config.dual_mic.eq_bands.clone())
    } else {
        None
    };

    let daemon = match audio::filter_chain::FilterChainConfigGenerator::write_config_to_runtime(
        &persistent_config.eq_bands,
        &persistent_config.virtual_channels,
        mic2_bands_initial.as_deref(),
        Some(&persistent_config.noise_clean),
        "studiomic-graph.conf",
    ) {
        Ok(path) => {
            let d = audio::daemon::PipeWireDaemon::new(
                path,
                persistent_config.eq_bands.clone(),
                persistent_config.virtual_channels.clone(),
                mic2_bands_initial,
                persistent_config.dual_mic.enabled,
                persistent_config.noise_clean.clone(),
            );
            if let Some(ref hw2) = persistent_config.dual_mic.hw_mic {
                d.set_mic2_hw(hw2.clone());
            }
            match d.start() {
                Ok(_) => {
                    println!("StudioMic PipeWire engine spawned successfully.");
                    if let Some(ref mic_name) = target_mic_name {
                        println!("Linking physical audio source '{}' to StudioMic capture engine...", mic_name);
                        match audio::device_discovery::DeviceDiscovery::link_source_to_studiomic(mic_name) {
                            Ok(_) => println!("Successfully configured hardware link for '{}'", target_mic_desc),
                            Err(e) => eprintln!("Notice: Link to source failed (may already be routed): {}", e),
                        }
                    }
                    if persistent_config.dual_mic.enabled {
                        if let Some(ref hw2) = persistent_config.dual_mic.hw_mic {
                            println!("Linking physical audio source 2 '{}' to StudioMic Mic 2 capture engine...", hw2);
                            let _ = audio::device_discovery::DeviceDiscovery::link_source_to_studiomic_target(
                                hw2,
                                "studiomic_mic2.capture",
                            );
                        }
                    }
                    if persistent_config.calibration.preamp_gain_db.abs() > 0.05 {
                        audio::daemon::PipeWireDaemon::set_preamp_trim(persistent_config.calibration.preamp_gain_db);
                    }
                    Some(d)
                }
                Err(e) => {
                    eprintln!("Warning: Failed to spawn PipeWire daemon: {}", e);
                    None
                }
            }
        }
        Err(e) => {
            eprintln!("Notice: Could not write runtime config: {}", e);
            None
        }
    };

    let daemon = std::sync::Arc::new(std::sync::Mutex::new(daemon));
    let daemon_ipc = daemon.clone();
    let config_arc = std::sync::Arc::new(std::sync::Mutex::new(persistent_config.clone()));
    let config_ipc = config_arc.clone();

    // Initialize Tao EventLoop with UserEvent for live audio peak telemetry & stream updates
    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let proxy_ipc = proxy.clone();

    // Start background peak detector reading live PCM levels from default physical mic
    let p_peak = proxy.clone();
    audio::daemon::PipeWireDaemon::start_peak_detector(move |db| {
        let _ = p_peak.send_event(AppEvent::Peak(db));
    });

    // Start background multichannel peak detector reading real-time PCM levels from all mixer channels
    let p_peaks = proxy.clone();
    audio::daemon::PipeWireDaemon::start_multichannel_peak_detector(move |peaks_map| {
        if let Ok(json) = serde_json::to_string(&peaks_map) {
            let _ = p_peaks.send_event(AppEvent::ChannelPeaks(json));
        }
    });

    // Start background periodic poller for active PipeWire audio streams, browser tabs, and dynamic auto-routing
    let p_streams = proxy.clone();
    std::thread::spawn(move || {
        // Initial route sync for default channels
        routing::wireplumber_hook::StreamWatcher::auto_route_assigned_channels();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(1800));
            routing::wireplumber_hook::StreamWatcher::auto_route_assigned_channels();
            let streams = routing::wireplumber_hook::StreamWatcher::scan_live_streams();
            if let Ok(json) = serde_json::to_string(&streams) {
                let _ = p_streams.send_event(AppEvent::LiveStreams(json));
            }
            let tabs = routing::wireplumber_hook::StreamWatcher::scan_active_browser_tabs();
            if let Ok(json) = serde_json::to_string(&tabs) {
                let _ = p_streams.send_event(AppEvent::BrowserTabs(json));
            }
        }
    });

    let args: Vec<String> = std::env::args().collect();
    let start_in_background = args.iter().any(|a| a == "--background" || a == "--tray" || a == "-b");

    let is_autostart = autostart::AutostartManager::is_enabled();
    if persistent_config.autostart && !is_autostart {
        let _ = autostart::AutostartManager::set_enabled(true);
    }

    let tray_state = std::sync::Arc::new(std::sync::Mutex::new(tray::TrayState {
        mic_muted: false,
        sidetone_enabled: persistent_config.sidetone_default,
        compressor_enabled: persistent_config.noise_clean.compressor_enabled,
        deep_filter_enabled: persistent_config.noise_clean.deep_filter_enabled,
        game_muted: false,
        voice_muted: false,
        music_muted: false,
        alerts_muted: false,
        autostart: autostart::AutostartManager::is_enabled(),
    }));
    let tray_state_ipc = tray_state.clone();
    let tray_state_loop = tray_state.clone();

    let tray_handle = tray::spawn_tray(proxy.clone(), tray_state.clone());
    let tray_handle_arc = std::sync::Arc::new(std::sync::Mutex::new(tray_handle.ok()));
    let tray_handle_ipc = tray_handle_arc.clone();
    let tray_handle_loop = tray_handle_arc.clone();

    let window = WindowBuilder::new()
        .with_title("StudioMic · Material Design 3")
        .with_inner_size(tao::dpi::LogicalSize::new(1440.0, 920.0))
        .with_visible(!start_in_background)
        .build(&event_loop)?;

    // Embed the complete, verified M3 HTML mockup
    let html_content = include_str!("../mic-studio-mockup.html");

    // Inject saved config and discovered system assets into webview before DOM load
    let initial_config_json = serde_json::to_string(&*config_arc.lock().unwrap())?;
    let hw_mics_json = serde_json::to_string(&hardware_mics)?;
    let players_json = serde_json::to_string(&installed_players)?;
    let browsers_json = serde_json::to_string(&installed_browsers)?;
    let browser_tabs_json = serde_json::to_string(&active_browser_tabs)?;
    let apps_json = serde_json::to_string(&installed_apps)?;
    let live_streams_json = serde_json::to_string(&active_streams)?;
    let is_autostart_now = autostart::AutostartManager::is_enabled();

    let init_script = format!(
        "window.__INITIAL_TAB__ = 'tab-noise';\n\
         window.__SAVED_CONFIG__ = {};\n\
         window.__INITIAL_HARDWARE_MICS__ = {};\n\
         window.__INITIAL_INSTALLED_PLAYERS__ = {};\n\
         window.__INITIAL_INSTALLED_BROWSERS__ = {};\n\
         window.__INITIAL_BROWSER_TABS__ = {};\n\
         window.__INITIAL_INSTALLED_APPS__ = {};\n\
         window.__INITIAL_LIVE_STREAMS__ = {};\n\
         window.__INITIAL_AUTOSTART__ = {};",
        initial_config_json, hw_mics_json, players_json, browsers_json, browser_tabs_json, apps_json, live_streams_json, is_autostart_now
    );

    let builder = WebViewBuilder::new()
        .with_html(html_content)
        .with_initialization_script(&init_script)
        .with_ipc_handler(move |req| {
            let body = req.body();
            println!("StudioMic IPC received: {}", body);
            if body.starts_with("volume:") {
                let parts: Vec<&str> = body.split(':').collect();
                if parts.len() == 3 {
                    let sink = parts[1];
                    let vol: f32 = parts[2].parse().unwrap_or(1.0);
                    audio::daemon::PipeWireDaemon::set_volume(sink, vol);
                }
            } else if body.starts_with("mute:") {
                let parts: Vec<&str> = body.split(':').collect();
                if parts.len() == 3 {
                    let sink = parts[1];
                    let mute = parts[2] == "1";
                    audio::daemon::PipeWireDaemon::set_mute(sink, mute);
                    if sink == "studiomic_processed_mic" {
                        tray_state_ipc.lock().unwrap().mic_muted = mute;
                        if let Some(ref h) = *tray_handle_ipc.lock().unwrap() {
                            let _ = h.update(|_| {});
                        }
                    }
                }
            } else if body.starts_with("sidetone:") {
                let enable = body.ends_with(":1");
                if let Some(ref d) = *daemon_ipc.lock().unwrap() {
                    d.set_sidetone(enable);
                }
                tray_state_ipc.lock().unwrap().sidetone_enabled = enable;
                if let Some(ref h) = *tray_handle_ipc.lock().unwrap() {
                    let _ = h.update(|_| {});
                }
            } else if body.starts_with("toggle_compressor:") {
                let enable = body.ends_with(":1");
                {
                    let mut cfg = config_ipc.lock().unwrap();
                    cfg.noise_clean.compressor_enabled = enable;
                    let _ = cfg.save();
                }
                if let Some(ref d) = *daemon_ipc.lock().unwrap() {
                    let nc = config_ipc.lock().unwrap().noise_clean.clone();
                    let _ = d.reload_noise_clean(&nc);
                }
                tray_state_ipc.lock().unwrap().compressor_enabled = enable;
                if let Some(ref h) = *tray_handle_ipc.lock().unwrap() {
                    let _ = h.update(|_| {});
                }
                println!("Toggled Vocal Compressor: {}", enable);
            } else if body.starts_with("toggle_autostart:") {
                let enable = body.ends_with(":1");
                let _ = autostart::AutostartManager::set_enabled(enable);
                {
                    let mut cfg = config_ipc.lock().unwrap();
                    cfg.autostart = enable;
                    let _ = cfg.save();
                }
                tray_state_ipc.lock().unwrap().autostart = enable;
                if let Some(ref h) = *tray_handle_ipc.lock().unwrap() {
                    let _ = h.update(|_| {});
                }
                println!("Toggled Autostart on boot: {}", enable);
            } else if body == "hide_window" {
                let _ = proxy_ipc.send_event(AppEvent::ToggleWindow);
            } else if body.starts_with("monitor_channel:") {
                // monitor_channel:<ch_id>:<1|0>
                let parts: Vec<&str> = body.split(':').collect();
                if parts.len() == 3 {
                    let ch_id = parts[1];
                    let enable = parts[2] == "1";
                    if let Some(ref d) = *daemon_ipc.lock().unwrap() {
                        d.set_channel_monitor(ch_id, enable);
                    }
                }
            } else if body.starts_with("noise_clean:") {
                let json_str = &body["noise_clean:".len()..];
                if let Ok(nc) = serde_json::from_str::<config::NoiseCleanConfig>(json_str) {
                    {
                        let mut cfg = config_ipc.lock().unwrap();
                        cfg.noise_clean = nc.clone();
                        let _ = cfg.save();
                    }
                    if let Some(ref d) = *daemon_ipc.lock().unwrap() {
                        let _ = d.reload_noise_clean(&nc);
                    }
                    {
                        let mut st = tray_state_ipc.lock().unwrap();
                        st.compressor_enabled = nc.compressor_enabled;
                        st.deep_filter_enabled = nc.deep_filter_enabled;
                    }
                    if let Some(ref h) = *tray_handle_ipc.lock().unwrap() {
                        let _ = h.update(|_| {});
                    }
                    println!("Applied and persisted Tab 3 Noise Clean DSP profile!");
                }
            } else if body.starts_with("calib_trim:") {
                let val_str = &body["calib_trim:".len()..];
                if let Ok(gain_db) = val_str.parse::<f32>() {
                    audio::daemon::PipeWireDaemon::set_preamp_trim(gain_db);
                    let mut cfg = config_ipc.lock().unwrap();
                    cfg.calibration.preamp_gain_db = gain_db;
                    let _ = cfg.save();
                }
            } else if body.starts_with("save_calibration:") {
                let json_str = &body["save_calibration:".len()..];
                if let Ok(calib) = serde_json::from_str::<config::CalibrationConfig>(json_str) {
                    audio::daemon::PipeWireDaemon::set_preamp_trim(calib.preamp_gain_db);
                    let mut cfg = config_ipc.lock().unwrap();
                    cfg.calibration = calib;
                    let _ = cfg.save();
                    println!("Persisted Tab 4 VoiceMatch Calibration profile!");
                }
            } else if body.starts_with("dual_mic:") {
                let enable = body.ends_with(":1");
                config_ipc.lock().unwrap().dual_mic.enabled = enable;
                let _ = config_ipc.lock().unwrap().save();
                if let Some(ref d) = *daemon_ipc.lock().unwrap() {
                    let _ = d.set_dual_mic(enable);
                }
                println!("Dual microphone setup toggled: {}", enable);
            } else if body.starts_with("hardware_mic2:") {
                let dev_name = &body["hardware_mic2:".len()..];
                config_ipc.lock().unwrap().dual_mic.hw_mic = Some(dev_name.to_string());
                let _ = config_ipc.lock().unwrap().save();
                if let Some(ref d) = *daemon_ipc.lock().unwrap() {
                    d.set_mic2_hw(dev_name.to_string());
                } else if let Err(e) = audio::device_discovery::DeviceDiscovery::link_source_to_studiomic_target(
                    dev_name,
                    "studiomic_mic2.capture",
                ) {
                    eprintln!("Failed to link hardware mic 2: {}", e);
                }
                println!("Set Microphone 2 hardware interface: {}", dev_name);
            } else if body.starts_with("profile_mic2:") {
                let mic_id = &body["profile_mic2:".len()..];
                if let Some(prof) = MicDatabase::find_by_id(mic_id) {
                    if let Some(ref d) = *daemon_ipc.lock().unwrap() {
                        let _ = d.reload_mic2_bands(&prof.bands);
                    }
                    let mut cfg = config_ipc.lock().unwrap();
                    cfg.dual_mic.mic_id = mic_id.to_string();
                    cfg.dual_mic.preset_name = format!("{} {}", prof.brand, prof.model);
                    cfg.dual_mic.eq_bands = prof.bands.clone();
                    let _ = cfg.save();
                    println!("Set Microphone 2 DSP profile to {}", mic_id);
                }
            } else if body.starts_with("eq_mic2:") {
                let json_str = &body["eq_mic2:".len()..];
                if let Ok(eq_bands) = parse_client_bands(json_str) {
                    if let Some(ref d) = *daemon_ipc.lock().unwrap() {
                        let _ = d.reload_mic2_bands(&eq_bands);
                    }
                }
            } else if body.starts_with("save_default_eq_mic2:") {
                let json_str = &body["save_default_eq_mic2:".len()..];
                if let Ok(eq_bands) = parse_client_bands(json_str) {
                    let mut cfg = config_ipc.lock().unwrap();
                    cfg.dual_mic.eq_bands = eq_bands.clone();
                    let _ = cfg.save();
                    if let Some(ref d) = *daemon_ipc.lock().unwrap() {
                        let _ = d.reload_mic2_bands(&eq_bands);
                    }
                    println!("Successfully persisted new default Mic 2 EQ bands!");
                }
            } else if body.starts_with("linein_route:") {
                // linein_route:<ch_id>:<hw_device_name>
                let parts: Vec<&str> = body.splitn(3, ':').collect();
                if parts.len() == 3 {
                    let ch_id = parts[1];
                    let hw_name = parts[2];
                    let target_node = format!("studiomic_{}.capture", ch_id);
                    match audio::device_discovery::DeviceDiscovery::link_source_to_studiomic_target(hw_name, &target_node) {
                        Ok(_) => println!("Routed Line In '{}' -> {}", hw_name, target_node),
                        Err(e) => eprintln!("Failed to route Line In: {}", e),
                    }
                }
            } else if body.starts_with("hardware_mic:") {
                let dev_name = &body["hardware_mic:".len()..];
                if let Some(ref d) = *daemon_ipc.lock().unwrap() {
                    d.set_hw_mic(dev_name.to_string());
                } else if let Err(e) = audio::device_discovery::DeviceDiscovery::link_source_to_studiomic(dev_name) {
                    eprintln!("Failed to link hardware mic: {}", e);
                }
                config_ipc.lock().unwrap().active_hw_mic = Some(dev_name.to_string());
                let _ = config_ipc.lock().unwrap().save();
            } else if body.starts_with("channel1_mic:") {
                let dev_name = &body["channel1_mic:".len()..];
                if let Err(e) = audio::device_discovery::DeviceDiscovery::link_source_to_studiomic(dev_name) {
                    eprintln!("Failed to link Channel 1 mic: {}", e);
                }
            } else if body.starts_with("route_stream:") {
                // route_stream:<node_id>:<channel_id>
                let parts: Vec<&str> = body.split(':').collect();
                if parts.len() == 3 {
                    if let Ok(node_id) = parts[1].parse::<u32>() {
                        routing::wireplumber_hook::StreamWatcher::route_stream_to_channel(node_id, parts[2]);
                    }
                }
            } else if body.starts_with("route_app:") {
                // route_app:<app_query>:<channel_id>
                let parts: Vec<&str> = body.splitn(3, ':').collect();
                if parts.len() == 3 {
                    routing::wireplumber_hook::StreamWatcher::route_app_to_channel(parts[1], parts[2]);
                }
            } else if body == "get_live_streams" {
                let streams = routing::wireplumber_hook::StreamWatcher::scan_live_streams();
                if let Ok(json) = serde_json::to_string(&streams) {
                    let _ = proxy_ipc.send_event(AppEvent::LiveStreams(json));
                }
            } else if body.starts_with("profile:") {
                let mic_id = &body["profile:".len()..];
                if let Some(prof) = MicDatabase::find_by_id(mic_id) {
                    if let Some(ref d) = *daemon_ipc.lock().unwrap() {
                        let _ = d.reload_bands(&prof.bands);
                    }
                    let mut cfg = config_ipc.lock().unwrap();
                    cfg.default_mic_id = mic_id.to_string();
                    cfg.preset_name = format!("{} {}", prof.brand, prof.model);
                    cfg.eq_bands = prof.bands.clone();
                    let _ = cfg.save();
                }
            } else if body.starts_with("ab_compare:") {
                let is_raw = body.ends_with(":raw");
                if is_raw {
                    let flat_bands = vec![models::microphone::EqBand {
                        freq: 1000.0,
                        gain_db: 0.0,
                        q: 1.0,
                        filter_type: models::microphone::FilterType::Peaking,
                    }];
                    if let Some(ref d) = *daemon_ipc.lock().unwrap() {
                        let _ = d.reload_bands(&flat_bands);
                    }
                }
            } else if body.starts_with("eq:") {
                let json_str = &body["eq:".len()..];
                if let Ok(eq_bands) = parse_client_bands(json_str) {
                    if let Some(ref d) = *daemon_ipc.lock().unwrap() {
                        let _ = d.reload_bands(&eq_bands);
                    }
                }
            } else if body.starts_with("save_default_eq:") {
                let json_str = &body["save_default_eq:".len()..];
                if let Ok(eq_bands) = parse_client_bands(json_str) {
                    let mut cfg = config_ipc.lock().unwrap();
                    cfg.eq_bands = eq_bands.clone();
                    let _ = cfg.save();
                    if let Some(ref d) = *daemon_ipc.lock().unwrap() {
                        let _ = d.reload_bands(&eq_bands);
                    }
                    println!("Successfully persisted new default EQ bands!");
                }
            } else if body == "reset_default_eq" {
                if let Some(prof) = MicDatabase::find_by_id("am8") {
                    let mut cfg = config_ipc.lock().unwrap();
                    cfg.eq_bands = prof.bands.clone();
                    cfg.default_mic_id = "am8".into();
                    cfg.preset_name = "Recommended EQ for Fifine AM8".into();
                    let _ = cfg.save();
                    if let Some(ref d) = *daemon_ipc.lock().unwrap() {
                        let _ = d.reload_bands(&prof.bands);
                    }
                    println!("Reset EQ to factory AM8 defaults!");
                }
            } else if body.starts_with("update_channels:") || body.starts_with("save_default_channels:") {
                let is_save = body.starts_with("save_default_channels:");
                let prefix_len = if is_save { "save_default_channels:".len() } else { "update_channels:".len() };
                let json_str = &body[prefix_len..];
                if let Ok(channels) = serde_json::from_str::<Vec<config::VirtualChannelConfig>>(json_str) {
                    if is_save {
                        let mut cfg = config_ipc.lock().unwrap();
                        cfg.virtual_channels = channels.clone();
                        let _ = cfg.save();
                        println!("Saved new default virtual channels to config: {} channels", channels.len());
                    }
                    if let Some(ref d) = *daemon_ipc.lock().unwrap() {
                        let _ = d.reload_channels(&channels);
                    }
                }
            } else if body == "reset_default_channels" {
                let def = config::StudioMicConfig::default();
                let mut cfg = config_ipc.lock().unwrap();
                cfg.virtual_channels = def.virtual_channels.clone();
                let _ = cfg.save();
                if let Some(ref d) = *daemon_ipc.lock().unwrap() {
                    let _ = d.reload_channels(&def.virtual_channels);
                }
                println!("Reset virtual mixer channels to defaults!");
            }
        });

    #[cfg(not(any(
        target_os = "windows",
        target_os = "macos",
        target_os = "ios",
        target_os = "android"
    )))]
    let _webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        let vbox = window.default_vbox().expect("Failed to get default vbox on Linux");
        builder.build_gtk(vbox)?
    };

    #[cfg(any(
        target_os = "windows",
        target_os = "macos",
        target_os = "ios",
        target_os = "android"
    ))]
    let _webview = builder.build(&window)?;

    println!("StudioMic UI initialized successfully.");

    event_loop.run(move |event, _, control_flow| {
        let _keep_alive = (&window, &_webview);
        *control_flow = ControlFlow::Wait;

        match &event {
            Event::UserEvent(AppEvent::Peak(db)) => {
                let _ = _webview.evaluate_script(&format!(
                    "if (window.updatePeakMeter) {{ window.updatePeakMeter({:.1}); }}",
                    db
                ));
            }
            Event::UserEvent(AppEvent::ChannelPeaks(json)) => {
                let _ = _webview.evaluate_script(&format!(
                    "if (window.updateChannelPeaks) {{ window.updateChannelPeaks({}); }}",
                    json
                ));
            }
            Event::UserEvent(AppEvent::LiveStreams(json)) => {
                let _ = _webview.evaluate_script(&format!(
                    "if (window.updateLiveStreams) {{ window.updateLiveStreams({}); }}",
                    json
                ));
            }
            Event::UserEvent(AppEvent::BrowserTabs(json)) => {
                let _ = _webview.evaluate_script(&format!(
                    "if (window.updateBrowserTabs) {{ window.updateBrowserTabs({}); }}",
                    json
                ));
            }
            Event::UserEvent(AppEvent::ShowWindow) => {
                window.set_visible(true);
                window.set_focus();
            }
            Event::UserEvent(AppEvent::ToggleWindow) => {
                let vis = window.is_visible();
                window.set_visible(!vis);
                if !vis {
                    window.set_focus();
                }
            }
            Event::UserEvent(AppEvent::Quit) => {
                println!("Closing StudioMic via Taskbar Tray...");
                if let Some(ref d) = *daemon.lock().unwrap() {
                    d.stop();
                }
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(AppEvent::SetMicMute(mute)) => {
                audio::daemon::PipeWireDaemon::set_mute("studiomic_processed_mic", *mute);
                tray_state_loop.lock().unwrap().mic_muted = *mute;
                if let Some(ref h) = *tray_handle_loop.lock().unwrap() {
                    let _ = h.update(|_| {});
                }
                let _ = _webview.evaluate_script(&format!(
                    "if (window.setMicMuteUI) {{ window.setMicMuteUI({}); }}",
                    mute
                ));
            }
            Event::UserEvent(AppEvent::SetMonitoring(enable)) => {
                if let Some(ref d) = *daemon.lock().unwrap() {
                    d.set_sidetone(*enable);
                }
                tray_state_loop.lock().unwrap().sidetone_enabled = *enable;
                if let Some(ref h) = *tray_handle_loop.lock().unwrap() {
                    let _ = h.update(|_| {});
                }
                let _ = _webview.evaluate_script(&format!(
                    "if (window.setMonitoringUI) {{ window.setMonitoringUI({}); }}",
                    enable
                ));
            }
            Event::UserEvent(AppEvent::SetCompressor(enable)) => {
                {
                    let mut cfg = config_arc.lock().unwrap();
                    cfg.noise_clean.compressor_enabled = *enable;
                    let _ = cfg.save();
                    if let Some(ref d) = *daemon.lock().unwrap() {
                        let _ = d.reload_noise_clean(&cfg.noise_clean);
                    }
                }
                tray_state_loop.lock().unwrap().compressor_enabled = *enable;
                if let Some(ref h) = *tray_handle_loop.lock().unwrap() {
                    let _ = h.update(|_| {});
                }
                let _ = _webview.evaluate_script(&format!(
                    "if (window.setCompressorUI) {{ window.setCompressorUI({}); }}",
                    enable
                ));
            }
            Event::UserEvent(AppEvent::SetDeepFilter(enable)) => {
                {
                    let mut cfg = config_arc.lock().unwrap();
                    cfg.noise_clean.deep_filter_enabled = *enable;
                    let _ = cfg.save();
                    if let Some(ref d) = *daemon.lock().unwrap() {
                        let _ = d.reload_noise_clean(&cfg.noise_clean);
                    }
                }
                tray_state_loop.lock().unwrap().deep_filter_enabled = *enable;
                if let Some(ref h) = *tray_handle_loop.lock().unwrap() {
                    let _ = h.update(|_| {});
                }
                let _ = _webview.evaluate_script(&format!(
                    "if (window.setDeepFilterUI) {{ window.setDeepFilterUI({}); }}",
                    enable
                ));
            }
            Event::UserEvent(AppEvent::SetChannelMute(channel, mute)) => {
                audio::daemon::PipeWireDaemon::set_mute(&format!("studiomic_{}.capture", channel), *mute);
                {
                    let mut st = tray_state_loop.lock().unwrap();
                    match channel.as_str() {
                        "game" => st.game_muted = *mute,
                        "voice" => st.voice_muted = *mute,
                        "music" => st.music_muted = *mute,
                        "alerts" => st.alerts_muted = *mute,
                        _ => (),
                    }
                }
                if let Some(ref h) = *tray_handle_loop.lock().unwrap() {
                    let _ = h.update(|_| {});
                }
                let _ = _webview.evaluate_script(&format!(
                    "if (window.setChannelMuteUI) {{ window.setChannelMuteUI('{}', {}); }}",
                    channel, mute
                ));
            }
            Event::UserEvent(AppEvent::ToggleAutostart(enable)) => {
                let _ = autostart::AutostartManager::set_enabled(*enable);
                {
                    let mut cfg = config_arc.lock().unwrap();
                    cfg.autostart = *enable;
                    let _ = cfg.save();
                }
                tray_state_loop.lock().unwrap().autostart = *enable;
                if let Some(ref h) = *tray_handle_loop.lock().unwrap() {
                    let _ = h.update(|_| {});
                }
                let _ = _webview.evaluate_script(&format!(
                    "if (window.setAutostartUI) {{ window.setAutostartUI({}); }}",
                    enable
                ));
            }
            Event::NewEvents(StartCause::Init) => println!("StudioMic Tao event loop running."),
            Event::WindowEvent { event, .. } => {
                if let WindowEvent::CloseRequested = event {
                    println!("StudioMic window close requested: minimizing to taskbar tray (audio engine running in background).");
                    window.set_visible(false);
                }
            }
            Event::LoopDestroyed => {
                println!("Tao LoopDestroyed!");
            }
            _ => (),
        }
    });
}
