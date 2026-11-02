use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioDevice {
    pub id: u32,
    pub name: String,
    pub description: String,
    pub is_default: bool,
    pub ports: Vec<String>,
    /// ALSA card number (`alsa.card`), used to tie a capture card's video and audio together.
    #[serde(default)]
    pub alsa_card: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureCardDevice {
    pub id: u32,
    pub name: String,
    pub description: String,
    pub video_device: String,
    pub audio_source: String,
    pub ports: Vec<String>,
}

pub struct DeviceDiscovery;

impl DeviceDiscovery {
    /// Audio inputs (mics and interfaces; Studio Mic's own nodes excluded). Uses the shared
    /// 250 ms node cache instead of a full `pw-dump` (~65 ms and 1 MB of JSON per call).
    pub fn get_input_devices() -> Vec<AudioDevice> {
        let default_source_name = Self::get_default_source_name();
        crate::audio::pw::nodes()
            .into_iter()
            .filter(|n| (n.class == "Audio/Source" || n.class == "Audio/Source/Virtual") && !n.name.to_lowercase().starts_with("studiomic"))
            .map(|n| {
                let props = &n.props;
                let card = |k: &str| match props.get(k) {
                    Some(serde_json::Value::Number(v)) => v.as_u64().map(|v| v as u32),
                    Some(serde_json::Value::String(v)) => v.parse().ok(),
                    _ => None,
                };
                AudioDevice {
                    id: n.id,
                    description: props
                        .get("node.description")
                        .or_else(|| props.get("node.nick"))
                        .and_then(|d| d.as_str())
                        .unwrap_or(&n.name)
                        .to_string(),
                    is_default: default_source_name.as_deref() == Some(n.name.as_str()),
                    ports: crate::audio::pw::ports_of(&n.name, true),
                    alsa_card: card("alsa.card").or_else(|| card("api.alsa.pcm.card")),
                    name: n.name,
                }
            })
            .collect()
    }

    /// Physical / virtual output sinks for the Master Monitor (headphones) selector.
    pub fn get_output_devices() -> Vec<AudioDevice> {
        let default_sink = crate::audio::pw::default_node_name("@DEFAULT_AUDIO_SINK@");
        crate::audio::pw::nodes()
            .into_iter()
            .filter(|n| n.class == "Audio/Sink" && !n.name.to_lowercase().starts_with("studiomic"))
            .map(|n| AudioDevice {
                id: n.id,
                description: n
                    .props
                    .get("node.description")
                    .or_else(|| n.props.get("node.nick"))
                    .and_then(|d| d.as_str())
                    .unwrap_or(&n.name)
                    .to_string(),
                is_default: default_sink.as_deref() == Some(n.name.as_str()),
                ports: Vec::new(),
                alsa_card: None,
                name: n.name,
            })
            .collect()
    }

    /// WirePlumber's current default audio source name.
    pub fn get_default_source_name() -> Option<String> {
        crate::audio::pw::default_node_name("@DEFAULT_AUDIO_SOURCE@")
    }

    /// Automatically discovers hardware capture cards that provide BOTH video and audio
    /// Capture cards among `input_devices` (pass the list you already scanned).
    pub fn get_capture_cards_from(input_devices: &[AudioDevice]) -> Vec<CaptureCardDevice> {
        let sound_cards = sysfs_sound_cards();
        let mut cards = Vec::new();
        let mut seen_audio_sources = std::collections::HashSet::new();

        // 1. Inspect V4L2 video devices from /sys/class/video4linux
        let v4l_path = std::path::Path::new("/sys/class/video4linux");
        if v4l_path.exists() {
            if let Ok(entries) = std::fs::read_dir(v4l_path) {
                for entry in entries.flatten() {
                    let dev_name = entry.file_name().to_string_lossy().to_string();
                    let name_file = entry.path().join("name");
                    let card_name = if name_file.exists() {
                        std::fs::read_to_string(&name_file)
                            .unwrap_or_else(|_| dev_name.clone())
                            .trim()
                            .to_string()
                    } else {
                        dev_name.clone()
                    };

                    // ALSA cards that live on the same physical device as this video node.
                    let same_device = entry
                        .path()
                        .join("device")
                        .canonicalize()
                        .map(|p| cards_on_device(&p, &sound_cards))
                        .unwrap_or_default();

                    for audio_dev in input_devices {
                        let audio_lower = audio_dev.name.to_lowercase();
                        let desc_lower = audio_dev.description.to_lowercase();
                        let card_lower = card_name.to_lowercase();

                        // Check name correlation or hardware path match
                        // The hardware check must name THIS audio device's card: it used to accept
                        // every input as soon as any sound card shared the video device's path.
                        let is_match = (card_lower.len() > 3 && desc_lower.contains(&card_lower))
                            || (card_lower.len() > 3 && audio_lower.contains(&card_lower))
                            || audio_dev.alsa_card.is_some_and(|c| same_device.contains(&c));

                        if is_match && !seen_audio_sources.contains(&audio_dev.name) {
                            seen_audio_sources.insert(audio_dev.name.clone());
                            cards.push(CaptureCardDevice {
                                id: audio_dev.id,
                                name: format!("{} (Video & Audio)", card_name),
                                description: format!("{} · /dev/{}", card_name, dev_name),
                                video_device: format!("/dev/{}", dev_name),
                                audio_source: audio_dev.name.clone(),
                                ports: audio_dev.ports.clone(),
                            });
                        }
                    }
                }
            }
        }

        // 2. Fallback check for any PipeWire audio source with capture card signatures
        for audio_dev in input_devices {
            if seen_audio_sources.contains(&audio_dev.name) {
                continue;
            }
            let desc_lower = audio_dev.description.to_lowercase();
            let name_lower = audio_dev.name.to_lowercase();
            let is_capture_signature = desc_lower.contains("cam link")
                || desc_lower.contains("capture")
                || desc_lower.contains("hdmi")
                || desc_lower.contains("elgato")
                || desc_lower.contains("avermedia")
                || desc_lower.contains("ripsaw")
                || desc_lower.contains("xr1")
                || desc_lower.contains("video grabber")
                || desc_lower.contains("uvc")
                || name_lower.contains("cam_link")
                || name_lower.contains("video_capture");

            if is_capture_signature {
                seen_audio_sources.insert(audio_dev.name.clone());
                cards.push(CaptureCardDevice {
                    id: audio_dev.id,
                    name: audio_dev.description.clone(),
                    description: format!("{} (Hardware Capture)", audio_dev.description),
                    video_device: "V4L2 Video Source".into(),
                    audio_source: audio_dev.name.clone(),
                    ports: audio_dev.ports.clone(),
                });
            }
        }

        cards
    }
}

/// `(N, canonical path)` of every /sys/class/sound/cardN.
fn sysfs_sound_cards() -> Vec<(u32, std::path::PathBuf)> {
    std::fs::read_dir("/sys/class/sound")
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().strip_prefix("card")?.parse::<u32>().ok()?;
                    Some((n, e.path().canonicalize().ok()?))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// ALSA card numbers on the same physical device as a video node's `device` path. A USB
/// video *interface* (`1-2:1.0`) is widened to its USB device (`1-2`), where the audio interface
/// of the same capture card sits.
fn cards_on_device(video_dev: &std::path::Path, sound_cards: &[(u32, std::path::PathBuf)]) -> Vec<u32> {
    let is_usb_interface = video_dev.file_name().map(|n| n.to_string_lossy().contains(':')).unwrap_or(false)
        && video_dev.to_string_lossy().contains("/usb");
    let root = if is_usb_interface { video_dev.parent().unwrap_or(video_dev) } else { video_dev };
    sound_cards.iter().filter(|(_, p)| p.starts_with(root)).map(|(n, _)| *n).collect()
}

#[cfg(test)]
mod tests {
    use super::cards_on_device;
    use std::path::{Path, PathBuf};

    #[test]
    fn capture_card_audio_is_matched_by_device_not_by_any_card() {
        let cards = vec![
            (0, PathBuf::from("/sys/devices/pci0000:00/0000:00:08.1/0000:01:00.1/sound/card0")),
            (1, PathBuf::from("/sys/devices/pci0000:00/0000:00:12.0/usb1/1-5/1-5:1.1/sound/card1")),
            (2, PathBuf::from("/sys/devices/pci0000:00/0000:00:1c.0/0000:03:00.0/sound/card2")),
        ];
        // PCI capture card: only its own sound card.
        assert_eq!(cards_on_device(Path::new("/sys/devices/pci0000:00/0000:00:1c.0/0000:03:00.0"), &cards), vec![2]);
        // USB capture card: video on interface :1.0, audio on :1.1 of the same device.
        assert_eq!(cards_on_device(Path::new("/sys/devices/pci0000:00/0000:00:12.0/usb1/1-5/1-5:1.0"), &cards), vec![1]);
        // Unrelated webcam: nothing.
        assert!(cards_on_device(Path::new("/sys/devices/pci0000:00/0000:00:14.0/usb3/3-1/3-1:1.0"), &cards).is_empty());
    }
}
