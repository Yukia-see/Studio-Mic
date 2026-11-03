//! Persistent state for Studio Mic.
//!
//! State lives in `~/.config/studio-mic/state.json` (schema_version 1). On first boot an older
//! `~/.config/studiomic/config.json` is migrated in and backed up as `config.json.bak`.
//! Writes are debounced (500 ms) and atomic (`state.json.tmp` -> fsync -> rename); `flush_now()`
//! writes synchronously and is called on quit / SIGINT / SIGTERM.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Condvar, LazyLock, Mutex};
use std::time::Duration;
use crate::models::microphone::{EqBand, FilterType};

pub const SCHEMA_VERSION: u32 = 1;
pub const THEMES: [&str; 4] = ["oled_black", "material_dark", "midnight_slate", "light"];
pub const DENOISERS: [&str; 4] = ["deepfilter", "rnnoise", "speex", "off"];

// ---------------------------------------------------------------------------------------------
// Per-channel mixer state (shared by the microphone, Mic 2 and every virtual / hardware channel)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelMix {
    #[serde(default)]
    pub stream_gain_db: f32,
    #[serde(default)]
    pub monitor_gain_db: f32,
    #[serde(default)]
    pub stream_muted: bool,
    #[serde(default)]
    pub monitor_muted: bool,
    /// Include this channel in the VOD (DMCA-safe) mix. Follows Stream Gain and Stream Mute.
    #[serde(default = "default_true")]
    pub vod: bool,
    /// Duck the Stream + VOD branches while the mic is talking. Monitor is never ducked.
    #[serde(default)]
    pub autoduck: bool,
    /// Solo affects the Monitor mix only.
    #[serde(default)]
    pub solo: bool,
    /// Per-route sync delays (cell delay) in milliseconds.
    #[serde(default)]
    pub delay_stream_ms: f32,
    #[serde(default)]
    pub delay_monitor_ms: f32,
    /// Routing-matrix cells: is this channel in the Stream / Monitor mix at all? Separate from
    /// mute (a temporary silence). VOD keeps following Stream Gain/Mute even when Stream is unrouted.
    #[serde(default = "default_true")]
    pub stream_routed: bool,
    #[serde(default = "default_true")]
    pub monitor_routed: bool,
    /// Microphone only: the Voice Chat (Voice_Out / Discord) cell. Mic Stream Mute stays the
    /// master mic mute and also silences Voice Chat.
    #[serde(default)]
    pub voice_gain_db: f32,
    #[serde(default)]
    pub voice_muted: bool,
    /// Voice Chat cell for Mic 2 and hardware-input channels (the main mic always feeds it;
    /// app channels never do).
    #[serde(default)]
    pub voice_routed: bool,
    /// Sends into user-created mixes: mix id -> cell. Present = routed into that mix.
    #[serde(default)]
    pub sends: std::collections::BTreeMap<String, SendCell>,
}

/// One routing-matrix cell of a custom mix.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SendCell {
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default)]
    pub muted: bool,
}

/// A user-created destination mix (its own lingering PipeWire bus, `StudioMic_Mix_<id>`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CustomMixConfig {
    pub id: String,
    pub name: String,
    #[serde(default = "default_mix_icon")]
    pub icon: String,
    #[serde(default = "default_mix_color")]
    pub color: String,
    /// Physical output this mix also plays to (node.name); None = capture-only (OBS, apps).
    #[serde(default)]
    pub output_device: Option<String>,
}

fn default_mix_icon() -> String { "alt_route".into() }
fn default_mix_color() -> String { "#22d3ee".into() }

impl Default for ChannelMix {
    fn default() -> Self {
        Self {
            stream_gain_db: 0.0,
            monitor_gain_db: 0.0,
            stream_muted: false,
            monitor_muted: false,
            vod: true,
            autoduck: false,
            solo: false,
            delay_stream_ms: 0.0,
            delay_monitor_ms: 0.0,
            stream_routed: true,
            monitor_routed: true,
            voice_gain_db: 0.0,
            voice_muted: false,
            voice_routed: false,
            sends: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualChannelConfig {
    pub id: String,
    pub name: String,
    #[serde(default = "default_icon")]
    pub icon: String,
    /// "virtual" (app sink), "hardware" (capture card / line in), or "comms" (voice chat; never ducked).
    #[serde(default = "default_kind")]
    pub kind: String,
    /// Bound hardware `node.name` for hardware channels.
    #[serde(default)]
    pub hw_source: Option<String>,
    /// Application binaries / names routed to this channel (matched against
    /// `application.process.binary` and `application.name`).
    #[serde(default)]
    pub apps: Vec<String>,
    /// "auto" (smart rules + apps list), "manual" (apps list only) or "desktop" (catch-all).
    #[serde(default = "default_route_mode")]
    pub route_mode: String,
    #[serde(default)]
    pub mix: ChannelMix,
}

fn default_icon() -> String { "instant_mix".into() }
fn default_kind() -> String { "virtual".into() }
fn default_route_mode() -> String { "auto".into() }

impl VirtualChannelConfig {
    pub fn is_comms(&self) -> bool {
        self.kind == "comms" || self.id == "voice"
    }

    /// Clean defaults for a newly created channel, using smart presets when the name matches one.
    pub fn new_with_defaults(id: &str, name: &str) -> Self {
        let key = format!("{} {}", id, name).to_lowercase();
        let preset = if key.contains("capture") || key.contains("switch") || key.contains("console") || key.contains("line") {
            "capture"
        } else if key.contains("voice") || key.contains("discord") || key.contains("chat") {
            "voice"
        } else if key.contains("music") || key.contains("spotify") || key.contains("media") {
            "music"
        } else if key.contains("browser") || key.contains("web") {
            "browser"
        } else if key.contains("game") {
            "game"
        } else if key.contains("sfx") || key.contains("sound") || key.contains("alert") {
            "sfx"
        } else {
            "custom"
        };
        let mut ch = preset_channel(preset);
        ch.id = id.to_string();
        ch.name = name.to_string();
        if preset == "custom" {
            ch.apps.clear();
            ch.route_mode = "manual".into();
        }
        ch
    }
}

/// Smart auto-routing rules for the dedicated virtual channel presets.
pub fn preset_apps(preset: &str) -> Vec<String> {
    let list: &[&str] = match preset {
        "game" => &["steam", "wine", "proton", "gamescope", "lutris"],
        "music" => &["spotify", "feishin", "elisa", "rhythmbox", "mpd", "amberol"],
        "voice" => &["discord", "vesktop", "webcord", "teamspeak", "mumble"],
        "browser" => &["firefox", "chromium", "brave", "zen", "vivaldi", "chrome"],
        _ => &[],
    };
    list.iter().map(|s| s.to_string()).collect()
}

pub fn preset_channel(preset: &str) -> VirtualChannelConfig {
    let (id, name, icon, kind) = match preset {
        "game" => ("game", "Game", "sports_esports", "virtual"),
        "music" => ("music", "Music", "music_note", "virtual"),
        "voice" => ("voice", "Voice Chat", "forum", "comms"),
        "browser" => ("browser", "Browser", "language", "virtual"),
        "sfx" => ("sfx", "SFX", "notifications", "virtual"),
        "capture" => ("capture", "Capture Card", "videocam", "hardware"),
        _ => ("custom", "Custom", "instant_mix", "virtual"),
    };
    let mut mix = ChannelMix::default();
    // Auto-Duck: ON for Game, Capture Card, Music, Browser (and SFX); OFF for Voice Chat.
    mix.autoduck = matches!(preset, "game" | "capture" | "music" | "browser" | "sfx");
    // Music is excluded from the VOD track by default (DMCA safety).
    mix.vod = preset != "music";
    VirtualChannelConfig {
        id: id.into(),
        name: name.into(),
        icon: icon.into(),
        kind: kind.into(),
        hw_source: None,
        apps: preset_apps(preset),
        route_mode: if preset == "capture" { "manual".into() } else { "auto".into() },
        mix,
    }
}

// ---------------------------------------------------------------------------------------------
// Microphone, denoiser and DSP state
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MicState {
    #[serde(default = "default_mic_mix")]
    pub mix: ChannelMix,
    /// Upmix a 1-channel (FL only) XLR interface to centred dual-mono.
    #[serde(default = "default_true")]
    pub mono_center: bool,
    /// Brickwall limiter (-0.5 dB) + hardware auto-pad.
    #[serde(default = "default_true")]
    pub clipguard: bool,
    /// "processed" (hear the full chain) or "raw" (zero-latency hardware tap, no DSP).
    #[serde(default = "default_monitor_mode")]
    pub monitor_mode: String,
}

fn default_monitor_mode() -> String { "processed".into() }

/// First-boot mic defaults: Stream 0 dB unmuted, Monitor muted (prevents self-echo).
fn default_mic_mix() -> ChannelMix {
    ChannelMix { monitor_muted: true, ..ChannelMix::default() }
}

impl Default for MicState {
    fn default() -> Self {
        Self { mix: default_mic_mix(), mono_center: true, clipguard: true, monitor_mode: default_monitor_mode() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DenoiserConfig {
    #[serde(default = "default_denoiser_engine")]
    pub engine: String,
    #[serde(default = "default_deep_attenuation")]
    pub df_attenuation_db: f32,
    #[serde(default = "default_deep_threshold")]
    pub df_min_threshold_db: f32,
    #[serde(default = "default_df_max_erb")]
    pub df_max_erb_threshold_db: f32,
    #[serde(default = "default_rn_vad")]
    pub rn_vad_threshold: f32,
    #[serde(default = "default_rn_grace")]
    pub rn_vad_grace_ms: f32,
    #[serde(default)]
    pub rn_retro_grace_ms: f32,
    #[serde(default = "default_speex_db")]
    pub speex_suppress_db: f32,
}

fn default_denoiser_engine() -> String { "deepfilter".into() }
fn default_df_max_erb() -> f32 { 30.0 }
fn default_rn_vad() -> f32 { 50.0 }
fn default_rn_grace() -> f32 { 200.0 }
fn default_speex_db() -> f32 { -30.0 }

impl Default for DenoiserConfig {
    fn default() -> Self {
        Self {
            engine: default_denoiser_engine(),
            df_attenuation_db: default_deep_attenuation(),
            df_min_threshold_db: default_deep_threshold(),
            df_max_erb_threshold_db: default_df_max_erb(),
            rn_vad_threshold: default_rn_vad(),
            rn_vad_grace_ms: default_rn_grace(),
            rn_retro_grace_ms: 0.0,
            speex_suppress_db: default_speex_db(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DualMicConfig {
    #[serde(default)]
    pub enabled: bool,
    pub hw_mic: Option<String>,
    pub mic_id: String,
    pub preset_name: String,
    pub eq_bands: Vec<EqBand>,
    #[serde(default)]
    pub mix: ChannelMix,
}

impl Default for DualMicConfig {
    fn default() -> Self {
        let sm7b_bands = crate::models::microphone::MicDatabase::find_by_id("sm7b")
            .map(|m| m.bands)
            .unwrap_or_default();
        Self {
            enabled: false,
            hw_mic: None,
            mic_id: "sm7b".into(),
            preset_name: "Warm Broadcast SM7B".into(),
            eq_bands: sm7b_bands,
            mix: ChannelMix::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoiseCleanConfig {
    #[serde(default = "default_true")]
    pub auto_mode: bool,
    #[serde(default = "default_true")]
    pub rumble_hp_enabled: bool,
    #[serde(default = "default_rumble_freq")]
    pub rumble_freq: f32,
    #[serde(default = "default_true")]
    pub hiss_lp_enabled: bool,
    #[serde(default = "default_hiss_freq")]
    pub hiss_freq: f32,
    /// Legacy toggle (pre-v1). The active engine now lives in `DenoiserConfig::engine`.
    #[serde(default = "default_false")]
    pub deep_filter_enabled: bool,
    #[serde(default = "default_deep_attenuation")]
    pub deep_filter_attenuation_db: f32,
    #[serde(default = "default_deep_threshold")]
    pub deep_filter_threshold_db: f32,
    #[serde(default = "default_true")]
    pub gate_enabled: bool,
    /// Gate OPEN threshold. The close threshold is always 7 dB lower (hysteresis).
    #[serde(default = "default_gate_threshold")]
    pub gate_threshold_db: f32,
    #[serde(default = "default_true")]
    pub deesser_enabled: bool,
    #[serde(default = "default_deesser_freq")]
    pub deesser_freq: f32,
    #[serde(default = "default_deesser_amount")]
    pub deesser_amount_db: f32,
    #[serde(default = "default_deesser_threshold")]
    pub deesser_threshold_db: f32,
    #[serde(default = "default_true")]
    pub compressor_enabled: bool,
    #[serde(default = "default_comp_threshold")]
    pub compressor_threshold_db: f32,
    #[serde(default = "default_comp_ratio")]
    pub compressor_ratio: f32,
    #[serde(default = "default_comp_attack")]
    pub compressor_attack_ms: f32,
    #[serde(default = "default_comp_release")]
    pub compressor_release_ms: f32,
    #[serde(default = "default_comp_makeup")]
    pub compressor_makeup_db: f32,
    /// Legacy limiter toggle; mirrored into `MicState::clipguard`.
    #[serde(default = "default_true")]
    pub limiter_enabled: bool,
    #[serde(default = "default_limiter_ceiling")]
    pub limiter_ceiling_db: f32,
}

fn default_true() -> bool { true }
fn default_false() -> bool { false }
fn default_rumble_freq() -> f32 { 80.0 }
fn default_hiss_freq() -> f32 { 16000.0 }
fn default_deep_attenuation() -> f32 { 80.0 }
fn default_deep_threshold() -> f32 { -15.0 }
fn default_gate_threshold() -> f32 { -40.0 }
fn default_deesser_freq() -> f32 { 5800.0 }
fn default_deesser_amount() -> f32 { -4.5 }
fn default_deesser_threshold() -> f32 { -30.0 }
fn default_comp_threshold() -> f32 { -20.0 }
fn default_comp_ratio() -> f32 { 3.5 }
fn default_comp_attack() -> f32 { 10.0 }
fn default_comp_release() -> f32 { 80.0 }
fn default_comp_makeup() -> f32 { 3.5 }
fn default_limiter_ceiling() -> f32 { -1.0 }

impl Default for NoiseCleanConfig {
    fn default() -> Self {
        Self {
            auto_mode: true,
            rumble_hp_enabled: true,
            rumble_freq: 80.0,
            hiss_lp_enabled: true,
            hiss_freq: 16000.0,
            deep_filter_enabled: true,
            deep_filter_attenuation_db: 80.0,
            deep_filter_threshold_db: -15.0,
            gate_enabled: true,
            gate_threshold_db: -40.0,
            deesser_enabled: true,
            deesser_freq: 5800.0,
            deesser_amount_db: -4.5,
            deesser_threshold_db: -30.0,
            compressor_enabled: true,
            compressor_threshold_db: -20.0,
            compressor_ratio: 3.5,
            compressor_attack_ms: 10.0,
            compressor_release_ms: 80.0,
            compressor_makeup_db: 3.5,
            limiter_enabled: true,
            limiter_ceiling_db: -1.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationConfig {
    #[serde(default = "default_preamp_gain")]
    pub preamp_gain_db: f32,
    #[serde(default = "default_calib_mic")]
    pub calibrated_mic_id: String,
    #[serde(default = "default_warmth")]
    pub voice_warmth_db: f32,
    #[serde(default = "default_presence")]
    pub voice_presence_db: f32,
}

fn default_preamp_gain() -> f32 { 0.0 }
fn default_calib_mic() -> String { "am8".into() }
fn default_warmth() -> f32 { 2.0 }
fn default_presence() -> f32 { 1.5 }

impl Default for CalibrationConfig {
    fn default() -> Self {
        Self {
            preamp_gain_db: 0.0,
            calibrated_mic_id: "am8".into(),
            voice_warmth_db: 2.0,
            voice_presence_db: 1.5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPreset {
    pub id: u64,
    pub title: String,
    pub mic: String,
    pub date: String,
    pub bands: Vec<EqBand>,
    #[serde(default = "default_true")]
    pub is_custom: bool,
    #[serde(default)]
    pub summary: Option<String>,
}

pub fn default_user_presets() -> Vec<UserPreset> {
    let b = |freq: f32, gain_db: f32, q: f32, filter_type: FilterType| EqBand { freq, gain_db, q, filter_type };
    vec![
        UserPreset {
            id: 1,
            title: "Late Night Warm Broadcast".into(),
            mic: "Fifine AM8".into(),
            date: "Factory Custom".into(),
            bands: vec![
                b(85.0, 0.0, 0.7, FilterType::HighPass),
                b(200.0, 2.5, 1.1, FilterType::Peaking),
                b(2800.0, 2.0, 1.4, FilterType::Peaking),
                b(11000.0, 1.5, 0.8, FilterType::HighShelf),
            ],
            is_custom: true,
            summary: Some("Bass +2.5 dB @ 200 Hz · Presence +2.0 dB @ 2.8 kHz · Air +1.5 dB @ 11 kHz".into()),
        },
        UserPreset {
            id: 2,
            title: "Twitch Gaming Voice Clarity".into(),
            mic: "Shure MV7".into(),
            date: "Factory Custom".into(),
            bands: vec![
                b(95.0, 0.0, 0.7, FilterType::HighPass),
                b(300.0, -3.5, 1.3, FilterType::Peaking),
                b(3800.0, 4.0, 1.6, FilterType::Peaking),
            ],
            is_custom: true,
            summary: Some("Mud Cut -3.5 dB @ 300 Hz · Speech Clarity +4.0 dB @ 3.8 kHz".into()),
        },
        UserPreset {
            id: 3,
            title: "Voiceover Narration Master".into(),
            mic: "Shure SM7B".into(),
            date: "Factory Custom".into(),
            bands: vec![
                b(80.0, 0.0, 0.7, FilterType::HighPass),
                b(240.0, -2.0, 1.2, FilterType::Peaking),
                b(4200.0, 3.5, 1.5, FilterType::Peaking),
                b(11500.0, 4.0, 0.7, FilterType::HighShelf),
            ],
            is_custom: true,
            summary: Some("Boom Cut -2.0 dB @ 240 Hz · Articulation +3.5 dB @ 4.2 kHz · Air +4.0 dB @ 11.5 kHz".into()),
        },
    ]
}

// ---------------------------------------------------------------------------------------------
// Top-level state
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StudioMicConfig {
    #[serde(default)]
    pub schema_version: u32,

    // 1. UI & theme
    #[serde(default = "default_theme")]
    pub theme: String,
    /// "normal" (waveform cards) or "simple" (performance stacked rows).
    #[serde(default = "default_view_mode")]
    pub view_mode: String,
    #[serde(default = "default_last_tab")]
    pub last_tab: String,
    #[serde(default = "default_window_w")]
    pub window_width: f64,
    #[serde(default = "default_window_h")]
    pub window_height: f64,

    // 2. Hardware devices (node.name strings, never numeric ids)
    pub default_mic_id: String,
    pub active_hw_mic: Option<String>,
    #[serde(default)]
    pub monitor_output: Option<String>,
    #[serde(default)]
    pub monitor_gain_db: f32,
    #[serde(default)]
    pub monitor_muted: bool,

    // 3. Microphone DSP chain
    #[serde(default)]
    pub mic: MicState,
    #[serde(default)]
    pub denoiser: DenoiserConfig,
    pub preset_name: String,
    pub eq_bands: Vec<EqBand>,
    #[serde(default)]
    pub noise_clean: NoiseCleanConfig,
    #[serde(default)]
    pub calibration: CalibrationConfig,

    // 4. Per-channel mixer state
    pub virtual_channels: Vec<VirtualChannelConfig>,
    #[serde(default)]
    pub dual_mic: DualMicConfig,
    /// User-created destination mixes (routing matrix "+ Add Mix").
    #[serde(default)]
    pub custom_mixes: Vec<CustomMixConfig>,
    /// Physical output per built-in mix ("stream", "vod", "voice"); My Mix uses `monitor_output`.
    #[serde(default)]
    pub mix_output_devices: std::collections::BTreeMap<String, String>,

    // 5. Application routing
    #[serde(default = "default_apps_mode")]
    pub active_apps_mode: String,
    #[serde(default = "default_assigned_routes")]
    pub assigned_channel_routes: std::collections::BTreeMap<String, String>,

    // Misc / legacy
    #[serde(default)]
    pub sidetone_default: bool,
    #[serde(default)]
    pub start_minimized: bool,
    #[serde(default = "default_autostart")]
    pub autostart: bool,
    #[serde(default = "default_true")]
    pub autostart_minimized: bool,
    #[serde(default = "default_user_presets")]
    pub user_presets: Vec<UserPreset>,
    #[serde(default)]
    pub capture_card_source: Option<String>,
}

fn default_theme() -> String { "material_dark".into() }
fn default_view_mode() -> String { "normal".into() }
fn default_last_tab() -> String { "tab-mixer".into() }
fn default_window_w() -> f64 { 1100.0 }
fn default_window_h() -> f64 { 760.0 }
fn default_apps_mode() -> String { "auto".into() }
fn default_autostart() -> bool { true }

pub fn default_assigned_routes() -> std::collections::BTreeMap<String, String> {
    let mut m = std::collections::BTreeMap::new();
    m.insert("game".into(), "steam".into());
    m.insert("voice".into(), "discord".into());
    m.insert("music".into(), "media_player".into());
    m.insert("capture".into(), "capture".into());
    m
}

pub fn default_channels() -> Vec<VirtualChannelConfig> {
    ["game", "voice", "music", "browser", "capture"].iter().map(|p| preset_channel(p)).collect()
}

impl Default for StudioMicConfig {
    fn default() -> Self {
        let default_bands = crate::models::microphone::MicDatabase::find_by_id("am8")
            .map(|m| m.bands)
            .unwrap_or_default();

        Self {
            schema_version: SCHEMA_VERSION,
            theme: default_theme(),
            view_mode: default_view_mode(),
            last_tab: default_last_tab(),
            window_width: default_window_w(),
            window_height: default_window_h(),
            default_mic_id: "am8".into(),
            active_hw_mic: None,
            monitor_output: None,
            monitor_gain_db: 0.0,
            monitor_muted: false,
            mic: MicState::default(),
            denoiser: DenoiserConfig::default(),
            preset_name: "Recommended EQ for Fifine AM8".into(),
            eq_bands: default_bands,
            noise_clean: NoiseCleanConfig::default(),
            calibration: CalibrationConfig::default(),
            virtual_channels: default_channels(),
            dual_mic: DualMicConfig::default(),
            custom_mixes: Vec::new(),
            mix_output_devices: Default::default(),
            active_apps_mode: default_apps_mode(),
            assigned_channel_routes: default_assigned_routes(),
            sidetone_default: false,
            start_minimized: false,
            autostart: true,
            autostart_minimized: true,
            user_presets: default_user_presets(),
            capture_card_source: None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------------------------

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
}

pub fn config_dir() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".config"))
        .join("studio-mic")
}

pub fn cache_dir() -> PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".cache"))
        .join("studio-mic")
}

pub fn profiles_dir() -> PathBuf {
    config_dir().join("profiles")
}

fn legacy_config_path() -> PathBuf {
    home().join(".config").join("studiomic").join("config.json")
}

/// Profile names become file names: keep them filesystem-safe.
pub fn sanitize_profile_name(name: &str) -> Option<String> {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' || c == '.' { c } else { '_' })
        .collect();
    let cleaned = cleaned.trim_matches('.').trim().to_string();
    if cleaned.is_empty() || cleaned.len() > 80 { None } else { Some(cleaned) }
}

// ---------------------------------------------------------------------------------------------
// Load / migrate / validate
// ---------------------------------------------------------------------------------------------

impl StudioMicConfig {
    pub fn config_path() -> PathBuf {
        config_dir().join("state.json")
    }

    /// Loads state.json, migrating the legacy config.json on first boot. Never overwrites a
    /// state file it could not parse (the broken file is kept as state.json.corrupt).
    pub fn load() -> Self {
        let path = Self::config_path();
        if path.exists() {
            match fs::read_to_string(&path).map(|s| serde_json::from_str::<Self>(&s)) {
                Ok(Ok(mut cfg)) => {
                    cfg.normalize();
                    println!("Loaded state from {:?}", path);
                    return cfg;
                }
                Ok(Err(e)) => {
                    eprintln!("state.json is invalid ({}); keeping a copy as state.json.corrupt", e);
                    let _ = fs::copy(&path, path.with_extension("json.corrupt"));
                }
                Err(e) => eprintln!("Could not read {:?}: {}", path, e),
            }
        }

        let legacy = legacy_config_path();
        if legacy.exists() {
            if let Ok(content) = fs::read_to_string(&legacy) {
                match Self::migrate_from_legacy(&content) {
                    Ok(cfg) => {
                        let _ = fs::copy(&legacy, legacy.with_extension("json.bak"));
                        let _ = cfg.write_atomic();
                        println!("Migrated legacy {:?} -> {:?} (backup: config.json.bak)", legacy, path);
                        return cfg;
                    }
                    Err(e) => eprintln!("Legacy config migration failed: {}", e),
                }
            }
        }

        let def = Self::default();
        let _ = def.write_atomic();
        def
    }

    /// Converts a pre-v1 config.json into the v1 schema without losing user data.
    pub fn migrate_from_legacy(content: &str) -> Result<Self, serde_json::Error> {
        let raw: serde_json::Value = serde_json::from_str(content)?;
        let mut cfg: Self = serde_json::from_value(raw.clone())?;
        cfg.schema_version = SCHEMA_VERSION;

        // Mic monitor default followed the old "sidetone" flag.
        cfg.mic.mix.monitor_muted = !cfg.sidetone_default;
        cfg.mic.clipguard = cfg.noise_clean.limiter_enabled;
        cfg.noise_clean.limiter_ceiling_db = -0.5;
        cfg.denoiser.engine = if cfg.noise_clean.deep_filter_enabled { "deepfilter".into() } else { "off".into() };
        cfg.denoiser.df_attenuation_db = cfg.noise_clean.deep_filter_attenuation_db;
        cfg.denoiser.df_min_threshold_db = cfg.noise_clean.deep_filter_threshold_db;

        // Old channels had only id/name/icon: derive kind, apps and mixer defaults.
        let routes = cfg.assigned_channel_routes.clone();
        for ch in &mut cfg.virtual_channels {
            if ch.id == "alerts" || ch.name.contains("Sound Alerts") {
                ch.id = "capture".into();
                ch.name = "Capture Card".into();
                ch.icon = "videocam".into();
            }
            if ch.name.contains("VOD Shield") {
                ch.name = "Music / Media".into();
            }
            let route = routes.get(&ch.id).cloned().unwrap_or_default();
            let preset = match (ch.id.as_str(), route.as_str()) {
                ("capture", _) | (_, "capture") | (_, "linein") => "capture",
                ("voice", _) | (_, "discord") => "voice",
                ("music", _) | (_, "media_player") => "music",
                ("game", _) | (_, "steam") | (_, "browse") => "game",
                ("browser", _) | (_, "browser") => "browser",
                _ => "custom",
            };
            let fresh = preset_channel(preset);
            ch.kind = fresh.kind;
            ch.mix = fresh.mix;
            ch.route_mode = fresh.route_mode;
            ch.apps = if preset == "custom" { Vec::new() } else { fresh.apps };
            if preset == "custom" {
                ch.route_mode = "manual".into();
            }
            // Keep explicit app names the user had picked (skip volatile node:<id> routes).
            let r = route.to_lowercase();
            if !r.is_empty()
                && !r.starts_with("node:")
                && !["steam", "discord", "media_player", "browser", "capture", "linein", "bus", "browse", "mic2"].contains(&r.as_str())
                && !ch.apps.contains(&r)
            {
                ch.apps.push(r);
            }
            if ch.kind == "hardware" && ch.hw_source.is_none() {
                ch.hw_source = cfg.capture_card_source.clone();
            }
        }
        cfg.normalize();
        Ok(cfg)
    }

    /// Clamps values and repairs anything a hand-edited or older file may contain.
    pub fn normalize(&mut self) {
        self.schema_version = SCHEMA_VERSION;
        if !THEMES.contains(&self.theme.as_str()) {
            self.theme = default_theme();
        }
        // "normal" = Cards, "simple" = Compact, "matrix" = routing matrix.
        if !["normal", "simple", "matrix"].contains(&self.view_mode.as_str()) {
            self.view_mode = "normal".into();
        }
        if !DENOISERS.contains(&self.denoiser.engine.as_str()) {
            self.denoiser.engine = "off".into();
        }
        if !["tab-mixer", "tab-eq", "tab-noise", "tab-calib"].contains(&self.last_tab.as_str()) {
            self.last_tab = default_last_tab();
        }
        self.window_width = self.window_width.clamp(640.0, 7680.0);
        self.window_height = self.window_height.clamp(480.0, 4320.0);
        self.monitor_gain_db = clamp_gain(self.monitor_gain_db);
        if self.user_presets.is_empty() {
            self.user_presets = default_user_presets();
        }
        if self.eq_bands.len() > 10 {
            self.eq_bands.truncate(10);
        }
        let mut seen = std::collections::HashSet::new();
        self.virtual_channels.retain(|c| !c.id.is_empty() && c.id != "mic" && seen.insert(c.id.clone()));
        for ch in &mut self.virtual_channels {
            ch.id = sanitize_channel_id(&ch.id);
            clamp_mix(&mut ch.mix);
            if ch.is_comms() {
                ch.mix.autoduck = false;
            }
        }
        let mut seen_mix = std::collections::HashSet::new();
        self.custom_mixes.retain_mut(|m| {
            m.id = sanitize_channel_id(&m.id);
            if m.name.trim().is_empty() {
                m.name = m.id.clone();
            }
            seen_mix.insert(m.id.clone())
        });
        let mix_ids: std::collections::HashSet<String> = self.custom_mixes.iter().map(|m| m.id.clone()).collect();
        for m in self.virtual_channels.iter_mut().map(|c| &mut c.mix).chain([&mut self.mic.mix, &mut self.dual_mic.mix]) {
            m.sends.retain(|id, _| mix_ids.contains(id));
            for cell in m.sends.values_mut() {
                cell.gain_db = clamp_gain(cell.gain_db);
            }
        }
        self.mix_output_devices.retain(|k, v| ["stream", "vod", "voice"].contains(&k.as_str()) && !v.is_empty() && !v.starts_with("StudioMic_"));
        for m in &mut self.custom_mixes {
            if m.output_device.as_deref().is_some_and(|d| d.is_empty() || d.starts_with("StudioMic_")) {
                m.output_device = None;
            }
        }
        clamp_mix(&mut self.mic.mix);
        clamp_mix(&mut self.dual_mic.mix);
        // The mic is never ducked.
        self.mic.mix.autoduck = false;
        if self.mic.monitor_mode != "raw" {
            self.mic.monitor_mode = "processed".into();
        }
        self.noise_clean.limiter_enabled = self.mic.clipguard;
        self.replace_non_finite();
    }

    /// NaN/inf would be saved as `null`, which makes the whole state.json unreadable on the next
    /// launch (settings reset). Any non-finite number goes back to its default here.
    fn replace_non_finite(&mut self) {
        fn fix(v: &mut f32, default: f32) {
            if !v.is_finite() {
                *v = default;
            }
        }
        let (d, n, c) = (DenoiserConfig::default(), NoiseCleanConfig::default(), CalibrationConfig::default());
        let dn = &mut self.denoiser;
        fix(&mut dn.df_attenuation_db, d.df_attenuation_db);
        fix(&mut dn.df_min_threshold_db, d.df_min_threshold_db);
        fix(&mut dn.df_max_erb_threshold_db, d.df_max_erb_threshold_db);
        fix(&mut dn.rn_vad_threshold, d.rn_vad_threshold);
        fix(&mut dn.rn_vad_grace_ms, d.rn_vad_grace_ms);
        fix(&mut dn.rn_retro_grace_ms, d.rn_retro_grace_ms);
        fix(&mut dn.speex_suppress_db, d.speex_suppress_db);
        let nc = &mut self.noise_clean;
        fix(&mut nc.rumble_freq, n.rumble_freq);
        fix(&mut nc.hiss_freq, n.hiss_freq);
        fix(&mut nc.deep_filter_attenuation_db, n.deep_filter_attenuation_db);
        fix(&mut nc.deep_filter_threshold_db, n.deep_filter_threshold_db);
        fix(&mut nc.gate_threshold_db, n.gate_threshold_db);
        fix(&mut nc.deesser_freq, n.deesser_freq);
        fix(&mut nc.deesser_amount_db, n.deesser_amount_db);
        fix(&mut nc.deesser_threshold_db, n.deesser_threshold_db);
        fix(&mut nc.compressor_threshold_db, n.compressor_threshold_db);
        fix(&mut nc.compressor_ratio, n.compressor_ratio);
        fix(&mut nc.compressor_attack_ms, n.compressor_attack_ms);
        fix(&mut nc.compressor_release_ms, n.compressor_release_ms);
        fix(&mut nc.compressor_makeup_db, n.compressor_makeup_db);
        fix(&mut nc.limiter_ceiling_db, n.limiter_ceiling_db);
        let cal = &mut self.calibration;
        fix(&mut cal.preamp_gain_db, c.preamp_gain_db);
        fix(&mut cal.voice_warmth_db, c.voice_warmth_db);
        fix(&mut cal.voice_presence_db, c.voice_presence_db);
        let presets = self.user_presets.iter_mut().map(|p| &mut p.bands);
        for bands in [&mut self.eq_bands, &mut self.dual_mic.eq_bands].into_iter().chain(presets) {
            for b in bands.iter_mut() {
                fix(&mut b.freq, 1000.0);
                fix(&mut b.gain_db, 0.0);
                fix(&mut b.q, 1.0);
            }
        }
        if !self.window_width.is_finite() || !self.window_height.is_finite() {
            self.window_width = default_window_w();
            self.window_height = default_window_h();
        }
    }

    /// True for sources allowed into Voice Chat besides the main mic: Mic 2 and hardware inputs.
    pub fn voice_eligible(&self, id: &str) -> bool {
        id == "mic2" || self.channel(id).is_some_and(|c| c.kind == "hardware")
    }

    pub fn channel(&self, id: &str) -> Option<&VirtualChannelConfig> {
        self.virtual_channels.iter().find(|c| c.id == id)
    }

    pub fn channel_mut(&mut self, id: &str) -> Option<&mut VirtualChannelConfig> {
        self.virtual_channels.iter_mut().find(|c| c.id == id)
    }

    /// Resolves a user-facing channel reference ("Capture Card", "capture", "Microphone", "mic").
    pub fn resolve_channel(&self, name: &str) -> Option<String> {
        let n = name.trim().to_lowercase();
        if ["mic", "microphone", "ch1", "mic1"].contains(&n.as_str()) {
            return Some("mic".into());
        }
        if ["mic2", "microphone 2"].contains(&n.as_str()) {
            return Some("mic2".into());
        }
        self.virtual_channels
            .iter()
            .find(|c| c.id.to_lowercase() == n || c.name.to_lowercase() == n)
            .or_else(|| self.virtual_channels.iter().find(|c| c.name.to_lowercase().starts_with(&n)))
            .map(|c| c.id.clone())
    }

    pub fn mix_mut(&mut self, id: &str) -> Option<&mut ChannelMix> {
        match id {
            "mic" => Some(&mut self.mic.mix),
            "mic2" => Some(&mut self.dual_mic.mix),
            other => self.channel_mut(other).map(|c| &mut c.mix),
        }
    }

    pub fn mix(&self, id: &str) -> Option<&ChannelMix> {
        match id {
            "mic" => Some(&self.mic.mix),
            "mic2" => Some(&self.dual_mic.mix),
            other => self.channel(other).map(|c| &c.mix),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Saving
    // -----------------------------------------------------------------------------------------

    /// Debounced save (>= 500 ms). Safe to call on every slider move.
    pub fn save(&self) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        let mut pending = SAVER.pending.lock().unwrap();
        *pending = Some(json);
        SAVER.cv.notify_one();
        Ok(())
    }

    /// Immediate synchronous write (quit, SIGINT/SIGTERM, window close).
    pub fn write_atomic(&self) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        write_atomic_str(&Self::config_path(), &json)
    }

    pub fn save_profile(&self, name: &str) -> std::io::Result<PathBuf> {
        let safe = sanitize_profile_name(name)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid profile name"))?;
        let path = profiles_dir().join(format!("{}.json", safe));
        write_atomic_str(&path, &serde_json::to_string_pretty(self)?)?;
        Ok(path)
    }

    /// Loads a named profile, keeping machine-specific settings (devices, window, autostart).
    pub fn load_profile(&self, name: &str) -> std::io::Result<Self> {
        let safe = sanitize_profile_name(name)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid profile name"))?;
        let path = profiles_dir().join(format!("{}.json", safe));
        let content = fs::read_to_string(&path)?;
        let mut p: Self = serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        p.active_hw_mic = self.active_hw_mic.clone();
        p.monitor_output = self.monitor_output.clone();
        p.window_width = self.window_width;
        p.window_height = self.window_height;
        p.autostart = self.autostart;
        p.autostart_minimized = self.autostart_minimized;
        p.start_minimized = self.start_minimized;
        p.normalize();
        Ok(p)
    }

    pub fn list_profiles() -> Vec<String> {
        let mut out: Vec<String> = fs::read_dir(profiles_dir())
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| {
                        let p = e.path();
                        if p.extension().and_then(|x| x.to_str()) == Some("json") {
                            p.file_stem().and_then(|s| s.to_str()).map(|s| s.to_string())
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }
}

pub fn sanitize_channel_id(id: &str) -> String {
    let mut s: String = id
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    while s.contains("__") {
        s = s.replace("__", "_");
    }
    let s = s.trim_matches('_').to_string();
    if s.is_empty() { "chan".into() } else { s }
}

pub fn clamp_gain(db: f32) -> f32 {
    if db.is_finite() { db.clamp(-60.0, 12.0) } else { 0.0 }
}

fn clamp_mix(m: &mut ChannelMix) {
    m.stream_gain_db = clamp_gain(m.stream_gain_db);
    m.monitor_gain_db = clamp_gain(m.monitor_gain_db);
    m.voice_gain_db = clamp_gain(m.voice_gain_db);
    m.delay_stream_ms = if m.delay_stream_ms.is_finite() { m.delay_stream_ms.clamp(0.0, 1000.0) } else { 0.0 };
    m.delay_monitor_ms = if m.delay_monitor_ms.is_finite() { m.delay_monitor_ms.clamp(0.0, 1000.0) } else { 0.0 };
}

// ---------------------------------------------------------------------------------------------
// Atomic, debounced writer
// ---------------------------------------------------------------------------------------------

struct Saver {
    pending: Mutex<Option<String>>,
    cv: Condvar,
    write_lock: Mutex<()>,
}

static SAVER: LazyLock<Saver> = LazyLock::new(|| {
    std::thread::Builder::new()
        .name("state-saver".into())
        .spawn(saver_loop)
        .expect("spawn state saver");
    Saver { pending: Mutex::new(None), cv: Condvar::new(), write_lock: Mutex::new(()) }
});

fn saver_loop() {
    // Wait until LazyLock has finished initialising.
    std::thread::sleep(Duration::from_millis(50));
    loop {
        {
            let mut pending = SAVER.pending.lock().unwrap();
            while pending.is_none() {
                pending = SAVER.cv.wait(pending).unwrap();
            }
        }
        // Debounce: keep absorbing changes until 500 ms pass without a new one.
        std::thread::sleep(Duration::from_millis(500));
        // Take the pending state only while holding the write lock, so flush_now() can never see
        // "nothing pending" while this thread still has an unwritten copy.
        let _g = SAVER.write_lock.lock().unwrap();
        let json = SAVER.pending.lock().unwrap().take();
        if let Some(json) = json {
            if let Err(e) = write_atomic_locked(&StudioMicConfig::config_path(), &json) {
                eprintln!("State save failed: {}", e);
            }
        }
    }
}

/// Writes any pending debounced save right now (and waits for an in-flight one).
pub fn flush_now() {
    let _g = SAVER.write_lock.lock().unwrap();
    let json = SAVER.pending.lock().unwrap().take();
    if let Some(json) = json {
        if let Err(e) = write_atomic_locked(&StudioMicConfig::config_path(), &json) {
            eprintln!("State flush failed: {}", e);
        }
    }
}

pub fn write_atomic_str(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    let _g = SAVER.write_lock.lock().unwrap();
    write_atomic_locked(path, content)
}

/// `write_atomic_str` for callers that already hold `SAVER.write_lock`.
fn write_atomic_locked(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        if let Ok(d) = fs::File::open(parent) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_migration_keeps_user_data() {
        let legacy = r#"{
          "default_mic_id": "am8",
          "active_hw_mic": "alsa_input.usb-Saramonic_MV_Pod-01.analog-stereo",
          "preset_name": "Live Streamer Pro (Music Mix)",
          "eq_bands": [{"freq": 85.0, "gain_db": 0.0, "q": 0.7, "filter_type": "HighPass"}],
          "virtual_channels": [
            {"id": "game", "name": "Game Audio", "icon": "instant_mix"},
            {"id": "voice", "name": "Discord Voice", "icon": "instant_mix"},
            {"id": "music", "name": "Music / Media", "icon": "instant_mix"},
            {"id": "capture", "name": "Capture Card", "icon": "videocam"},
            {"id": "mixer", "name": "Mixer", "icon": "instant_mix"}
          ],
          "sidetone_default": false,
          "capture_card_source": "alsa_input.usb-Brio-02.mono-fallback",
          "assigned_channel_routes": {"music": "node:229", "capture": "capture", "game": "steam", "voice": "discord"},
          "noise_clean": {"deep_filter_enabled": true, "deep_filter_attenuation_db": 70.0, "gate_threshold_db": -38.0, "limiter_enabled": true}
        }"#;
        let cfg = StudioMicConfig::migrate_from_legacy(legacy).unwrap();
        assert_eq!(cfg.schema_version, SCHEMA_VERSION);
        assert_eq!(cfg.active_hw_mic.as_deref(), Some("alsa_input.usb-Saramonic_MV_Pod-01.analog-stereo"));
        assert_eq!(cfg.preset_name, "Live Streamer Pro (Music Mix)");
        assert_eq!(cfg.virtual_channels.len(), 5);
        assert_eq!(cfg.denoiser.engine, "deepfilter");
        assert_eq!(cfg.denoiser.df_attenuation_db, 70.0);
        assert!(cfg.mic.mix.monitor_muted);
        let cap = cfg.channel("capture").unwrap();
        assert_eq!(cap.kind, "hardware");
        assert_eq!(cap.hw_source.as_deref(), Some("alsa_input.usb-Brio-02.mono-fallback"));
        let voice = cfg.channel("voice").unwrap();
        assert!(voice.is_comms() && !voice.mix.autoduck);
        let music = cfg.channel("music").unwrap();
        assert!(!music.mix.vod);
        assert!(cfg.channel("mixer").unwrap().apps.is_empty());
    }

    #[test]
    fn non_finite_numbers_never_reach_state_json() {
        let mut s = StudioMicConfig::default();
        s.calibration.preamp_gain_db = f32::NAN;
        s.denoiser.speex_suppress_db = f32::INFINITY;
        if let Some(b) = s.eq_bands.first_mut() {
            b.gain_db = f32::NAN;
        }
        s.normalize();
        let json = serde_json::to_string(&s).unwrap();
        assert!(serde_json::from_str::<StudioMicConfig>(&json).is_ok(), "state must reload");
        assert_eq!(s.calibration.preamp_gain_db, CalibrationConfig::default().preamp_gain_db);
    }

    #[test]
    fn custom_mixes_and_sends_are_normalized() {
        let mut s = StudioMicConfig::default();
        s.custom_mixes.push(CustomMixConfig { id: "Guest Mix!".into(), name: "Guest Mix".into(), icon: "alt_route".into(), color: "#fff".into(), output_device: Some("StudioMic_Monitor_Mix".into()) });
        s.mic.mix.sends.insert("guest_mix".into(), SendCell { gain_db: 40.0, muted: false });
        s.mic.mix.sends.insert("deleted".into(), SendCell::default());
        s.mix_output_devices.insert("stream".into(), "alsa_output.x".into());
        s.mix_output_devices.insert("bogus".into(), "alsa_output.x".into());
        s.normalize();
        assert_eq!(s.custom_mixes[0].id, "guest_mix");
        assert_eq!(s.custom_mixes[0].output_device, None, "a mix never plays into our own nodes");
        assert_eq!(s.mic.mix.sends.len(), 1, "sends into deleted mixes are dropped");
        assert_eq!(s.mic.mix.sends["guest_mix"].gain_db, 12.0);
        assert_eq!(s.mix_output_devices.len(), 1);
        assert!(s.voice_eligible("mic2") && !s.voice_eligible("game"));
        assert!(s.virtual_channels.iter().filter(|c| c.kind == "hardware").all(|c| s.voice_eligible(&c.id)));
    }

    #[test]
    fn matrix_fields_default_on_for_old_state_files() {
        // A state.json written before the routing matrix existed keeps every channel routed.
        let m: ChannelMix = serde_json::from_str(r#"{"stream_gain_db": -3.0}"#).unwrap();
        assert!(m.stream_routed && m.monitor_routed && !m.voice_muted);
        assert_eq!(m.voice_gain_db, 0.0);
        let mut s = StudioMicConfig::default();
        s.view_mode = "matrix".into();
        s.normalize();
        assert_eq!(s.view_mode, "matrix");
        s.view_mode = "bogus".into();
        s.normalize();
        assert_eq!(s.view_mode, "normal");
    }

    #[test]
    fn resolve_channel_by_name_or_id() {
        let cfg = StudioMicConfig::default();
        assert_eq!(cfg.resolve_channel("Capture Card").as_deref(), Some("capture"));
        assert_eq!(cfg.resolve_channel("Microphone").as_deref(), Some("mic"));
        assert_eq!(cfg.resolve_channel("music").as_deref(), Some("music"));
        assert_eq!(cfg.resolve_channel("nope"), None);
    }

    #[test]
    fn profile_names_are_sanitized() {
        assert_eq!(sanitize_profile_name("My Stream").as_deref(), Some("My Stream"));
        assert_eq!(sanitize_profile_name("../../etc/passwd").as_deref(), Some("_.._etc_passwd"));
        assert_eq!(sanitize_profile_name("   "), None);
    }
}
