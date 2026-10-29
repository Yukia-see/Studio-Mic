use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum FilterType {
    HighPass,
    LowPass,
    Peaking,
    HighShelf,
    LowShelf,
    Notch,
}

impl FilterType {
    pub fn pipewire_label(&self) -> &'static str {
        match self {
            FilterType::HighPass => "bq_highpass",
            FilterType::LowPass => "bq_lowpass",
            FilterType::Peaking => "bq_peaking",
            FilterType::HighShelf => "bq_highshelf",
            FilterType::LowShelf => "bq_lowshelf",
            FilterType::Notch => "bq_notch",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EqBand {
    pub freq: f32,
    pub gain_db: f32,
    pub q: f32,
    pub filter_type: FilterType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MicrophoneProfile {
    pub id: String,
    pub brand: String,
    pub model: String,
    pub connectivity: String,
    pub weakness_desc: String,
    pub profile_title: String,
    pub profile_summary: String,
    pub bands: Vec<EqBand>,
}

pub struct MicDatabase;

impl MicDatabase {
    pub fn get_all() -> Vec<MicrophoneProfile> {
        vec![
            MicrophoneProfile {
                id: "at2020".into(),
                brand: "Audio-Technica".into(),
                model: "Audio-Technica AT2020".into(),
                connectivity: "XLR".into(),
                weakness_desc: "Lean body; presence bite at 5.8kHz".into(),
                profile_title: "Recommended EQ for Audio-Technica AT2020".into(),
                profile_summary: "Supplements chest warmth and softens high-frequency presence bite.".into(),
                bands: vec![
                    EqBand { freq: 80.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::HighPass },
                    EqBand { freq: 160.0, gain_db: 1.8, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 350.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 700.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 1500.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 3000.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 5800.0, gain_db: -2.2, q: 2.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 8200.0, gain_db: -2.5, q: 2.5, filter_type: FilterType::Peaking },
                    EqBand { freq: 12000.0, gain_db: 1.8, q: 0.7, filter_type: FilterType::HighShelf },
                    EqBand { freq: 16000.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                ],
            },
            MicrophoneProfile {
                id: "am8".into(),
                brand: "FIFINE".into(),
                model: "FIFINE AmpliGame AM8".into(),
                connectivity: "USB-C / XLR".into(),
                weakness_desc: "Boomy proximity effect; boxiness at 280Hz; sibilance at 7.1kHz".into(),
                profile_title: "Recommended EQ for Fifine AM8".into(),
                profile_summary: "Scuffs out low-mid mud, pushes vocal bite, smooths sibilance.".into(),
                bands: vec![
                    EqBand { freq: 85.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::HighPass },
                    EqBand { freq: 160.0, gain_db: 1.2, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 280.0, gain_db: -3.5, q: 1.4, filter_type: FilterType::Peaking },
                    EqBand { freq: 550.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 1200.0, gain_db: 1.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 2400.0, gain_db: 2.0, q: 1.6, filter_type: FilterType::Peaking },
                    EqBand { freq: 4500.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 7100.0, gain_db: -2.8, q: 3.2, filter_type: FilterType::Peaking },
                    EqBand { freq: 11000.0, gain_db: 2.0, q: 0.7, filter_type: FilterType::HighShelf },
                    EqBand { freq: 16000.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                ],
            },
            MicrophoneProfile {
                id: "sm7b".into(),
                brand: "Shure".into(),
                model: "Shure SM7B".into(),
                connectivity: "XLR".into(),
                weakness_desc: "Dark top end; needs presence lift; low output".into(),
                profile_title: "Recommended EQ for Shure SM7B".into(),
                profile_summary: "Opens up high air frequencies and eliminates sub-rumble.".into(),
                bands: vec![
                    EqBand { freq: 80.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::HighPass },
                    EqBand { freq: 150.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 240.0, gain_db: -2.0, q: 1.2, filter_type: FilterType::Peaking },
                    EqBand { freq: 500.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 1000.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 2500.0, gain_db: 1.5, q: 1.2, filter_type: FilterType::Peaking },
                    EqBand { freq: 4200.0, gain_db: 3.5, q: 1.5, filter_type: FilterType::Peaking },
                    EqBand { freq: 7500.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 11500.0, gain_db: 4.0, q: 0.7, filter_type: FilterType::HighShelf },
                    EqBand { freq: 16000.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                ],
            },
            MicrophoneProfile {
                id: "mv7plus".into(),
                brand: "Shure".into(),
                model: "Shure MV7+".into(),
                connectivity: "USB-C / XLR".into(),
                weakness_desc: "Pronounced proximity boom at 220Hz; 6.2kHz bite".into(),
                profile_title: "Recommended EQ for Shure MV7+".into(),
                profile_summary: "Tames the 220Hz chest boom while adding broadcast shine.".into(),
                bands: vec![
                    EqBand { freq: 75.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::HighPass },
                    EqBand { freq: 150.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 220.0, gain_db: -3.0, q: 1.1, filter_type: FilterType::Peaking },
                    EqBand { freq: 500.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 1200.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 3200.0, gain_db: 2.5, q: 1.3, filter_type: FilterType::Peaking },
                    EqBand { freq: 6200.0, gain_db: -2.0, q: 2.8, filter_type: FilterType::Peaking },
                    EqBand { freq: 8500.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 11000.0, gain_db: 3.0, q: 0.7, filter_type: FilterType::HighShelf },
                    EqBand { freq: 16000.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                ],
            },
            MicrophoneProfile {
                id: "podmic".into(),
                brand: "Rode".into(),
                model: "Rode PodMic USB".into(),
                connectivity: "USB-C / XLR".into(),
                weakness_desc: "Thick low-end boom and harsh 3.8kHz edge".into(),
                profile_title: "Recommended EQ for Rode PodMic USB".into(),
                profile_summary: "Sculpts low-end clarity, warms chest, and smooths harsh upper edge.".into(),
                bands: vec![
                    EqBand { freq: 80.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::HighPass },
                    EqBand { freq: 200.0, gain_db: -3.2, q: 1.2, filter_type: FilterType::Peaking },
                    EqBand { freq: 400.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 800.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 1200.0, gain_db: 1.5, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 2500.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 3800.0, gain_db: -2.5, q: 2.2, filter_type: FilterType::Peaking },
                    EqBand { freq: 6500.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 10500.0, gain_db: 2.8, q: 0.7, filter_type: FilterType::HighShelf },
                    EqBand { freq: 16000.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                ],
            },
            MicrophoneProfile {
                id: "generic".into(),
                brand: "Universal".into(),
                model: "Generic Microphone".into(),
                connectivity: "Universal".into(),
                weakness_desc: "Balanced starting curve for generic mics".into(),
                profile_title: "Recommended EQ for Generic Microphone".into(),
                profile_summary: "Neutral flat reference with rumble cut and speech clarity.".into(),
                bands: vec![
                    EqBand { freq: 80.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::HighPass },
                    EqBand { freq: 160.0, gain_db: 1.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 300.0, gain_db: -1.0, q: 1.2, filter_type: FilterType::Peaking },
                    EqBand { freq: 600.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 1200.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 2500.0, gain_db: 1.5, q: 1.2, filter_type: FilterType::Peaking },
                    EqBand { freq: 4500.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 7000.0, gain_db: -1.0, q: 2.0, filter_type: FilterType::Peaking },
                    EqBand { freq: 11000.0, gain_db: 1.5, q: 0.7, filter_type: FilterType::HighShelf },
                    EqBand { freq: 16000.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking },
                ],
            },
        ]
    }

    pub fn find_by_id(id: &str) -> Option<MicrophoneProfile> {
        Self::get_all().into_iter().find(|m| m.id == id)
    }
}
