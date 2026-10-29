use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use crate::models::microphone::EqBand;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualChannelConfig {
    pub id: String,
    pub name: String,
    #[serde(default = "default_icon")]
    pub icon: String,
}

fn default_icon() -> String {
    "instant_mix".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DualMicConfig {
    #[serde(default)]
    pub enabled: bool,
    pub hw_mic: Option<String>,
    pub mic_id: String,
    pub preset_name: String,
    pub eq_bands: Vec<EqBand>,
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
    #[serde(default = "default_true")]
    pub deep_filter_enabled: bool,
    #[serde(default = "default_deep_attenuation")]
    pub deep_filter_attenuation_db: f32,
    #[serde(default = "default_deep_threshold")]
    pub deep_filter_threshold_db: f32,
    #[serde(default = "default_true")]
    pub gate_enabled: bool,
    #[serde(default = "default_gate_threshold")]
    pub gate_threshold_db: f32,
    #[serde(default = "default_true")]
    pub deesser_enabled: bool,
    #[serde(default = "default_deesser_freq")]
    pub deesser_freq: f32,
    #[serde(default = "default_deesser_amount")]
    pub deesser_amount_db: f32,
    #[serde(default = "default_true")]
    pub compressor_enabled: bool,
    #[serde(default = "default_comp_threshold")]
    pub compressor_threshold_db: f32,
    #[serde(default = "default_comp_ratio")]
    pub compressor_ratio: f32,
    #[serde(default = "default_comp_makeup")]
    pub compressor_makeup_db: f32,
    #[serde(default = "default_true")]
    pub limiter_enabled: bool,
    #[serde(default = "default_limiter_ceiling")]
    pub limiter_ceiling_db: f32,
}

fn default_true() -> bool { true }
fn default_rumble_freq() -> f32 { 80.0 }
fn default_hiss_freq() -> f32 { 16000.0 }
fn default_deep_attenuation() -> f32 { 80.0 }
fn default_deep_threshold() -> f32 { -15.0 }
fn default_gate_threshold() -> f32 { -40.0 }
fn default_deesser_freq() -> f32 { 5800.0 }
fn default_deesser_amount() -> f32 { -4.5 }
fn default_comp_threshold() -> f32 { -20.0 }
fn default_comp_ratio() -> f32 { 3.5 }
fn default_comp_makeup() -> f32 { 3.5 }
fn default_limiter_ceiling() -> f32 { -2.0 }

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
            compressor_enabled: true,
            compressor_threshold_db: -20.0,
            compressor_ratio: 3.5,
            compressor_makeup_db: 3.5,
            limiter_enabled: true,
            limiter_ceiling_db: -2.0,
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
    vec![
        UserPreset {
            id: 1,
            title: "Late Night Warm Broadcast".into(),
            mic: "Fifine AM8".into(),
            date: "Factory Custom".into(),
            bands: vec![
                EqBand { freq: 85.0, gain_db: 0.0, q: 0.7, filter_type: crate::models::microphone::FilterType::HighPass },
                EqBand { freq: 200.0, gain_db: 2.5, q: 1.1, filter_type: crate::models::microphone::FilterType::Peaking },
                EqBand { freq: 2800.0, gain_db: 2.0, q: 1.4, filter_type: crate::models::microphone::FilterType::Peaking },
                EqBand { freq: 11000.0, gain_db: 1.5, q: 0.8, filter_type: crate::models::microphone::FilterType::HighShelf },
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
                EqBand { freq: 95.0, gain_db: 0.0, q: 0.7, filter_type: crate::models::microphone::FilterType::HighPass },
                EqBand { freq: 300.0, gain_db: -3.5, q: 1.3, filter_type: crate::models::microphone::FilterType::Peaking },
                EqBand { freq: 3800.0, gain_db: 4.0, q: 1.6, filter_type: crate::models::microphone::FilterType::Peaking },
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
                EqBand { freq: 80.0, gain_db: 0.0, q: 0.7, filter_type: crate::models::microphone::FilterType::HighPass },
                EqBand { freq: 240.0, gain_db: -2.0, q: 1.2, filter_type: crate::models::microphone::FilterType::Peaking },
                EqBand { freq: 4200.0, gain_db: 3.5, q: 1.5, filter_type: crate::models::microphone::FilterType::Peaking },
                EqBand { freq: 11500.0, gain_db: 4.0, q: 0.7, filter_type: crate::models::microphone::FilterType::HighShelf },
            ],
            is_custom: true,
            summary: Some("Boom Cut -2.0 dB @ 240 Hz · Articulation +3.5 dB @ 4.2 kHz · Air +4.0 dB @ 11.5 kHz".into()),
        },
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StudioMicConfig {
    pub default_mic_id: String,
    pub active_hw_mic: Option<String>,
    pub preset_name: String,
    pub eq_bands: Vec<EqBand>,
    pub virtual_channels: Vec<VirtualChannelConfig>,
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
    #[serde(default)]
    pub dual_mic: DualMicConfig,
    #[serde(default)]
    pub noise_clean: NoiseCleanConfig,
    #[serde(default)]
    pub calibration: CalibrationConfig,
}

fn default_autostart() -> bool { true }

impl Default for StudioMicConfig {
    fn default() -> Self {
        let default_bands = crate::models::microphone::MicDatabase::find_by_id("am8")
            .map(|m| m.bands)
            .unwrap_or_default();

        Self {
            default_mic_id: "am8".into(),
            active_hw_mic: None,
            preset_name: "Recommended EQ for Fifine AM8".into(),
            eq_bands: default_bands,
            virtual_channels: vec![
                VirtualChannelConfig { id: "game".into(), name: "Game Audio".into(), icon: "sports_esports".into() },
                VirtualChannelConfig { id: "voice".into(), name: "Discord Voice".into(), icon: "forum".into() },
                VirtualChannelConfig { id: "music".into(), name: "Music / Media".into(), icon: "music_note".into() },
                VirtualChannelConfig { id: "capture".into(), name: "Capture Card".into(), icon: "videocam".into() },
            ],
            sidetone_default: false,
            start_minimized: false,
            autostart: true,
            autostart_minimized: true,
            user_presets: default_user_presets(),
            capture_card_source: None,
            dual_mic: DualMicConfig::default(),
            noise_clean: NoiseCleanConfig::default(),
            calibration: CalibrationConfig::default(),
        }
    }
}

impl StudioMicConfig {
    pub fn config_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".config").join("studiomic").join("config.json")
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        if path.exists() {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(mut cfg) = serde_json::from_str::<Self>(&content) {
                    // Clean up legacy VOD Shield / alerts channel titles if present
                    for ch in &mut cfg.virtual_channels {
                        if ch.name.contains("VOD Shield") {
                            ch.name = "Music / Media".into();
                        }
                        if ch.id == "alerts" || ch.name.contains("Sound Alerts") {
                            ch.id = "capture".into();
                            ch.name = "Capture Card".into();
                            ch.icon = "videocam".into();
                        }
                    }
                    if cfg.user_presets.is_empty() {
                        cfg.user_presets = default_user_presets();
                    }
                    println!("Loaded persistent configuration from {:?}", path);
                    return cfg;
                }
            }
        }
        let def = Self::default();
        let _ = def.save();
        def
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        fs::write(&path, json)?;
        println!("Saved persistent configuration to {:?}", path);
        Ok(())
    }
}
