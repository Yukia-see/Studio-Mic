use serde::{Deserialize, Serialize};
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioDevice {
    pub id: u32,
    pub name: String,
    pub description: String,
    pub is_default: bool,
    pub ports: Vec<String>,
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
    /// Discovers system physical audio input devices (microphones & audio interfaces) via pw-dump
    pub fn get_input_devices() -> Vec<AudioDevice> {
        let output = match Command::new("pw-dump").output() {
            Ok(o) => o,
            Err(e) => {
                eprintln!("Failed to execute pw-dump: {}", e);
                return Vec::new();
            }
        };

        let json_val: serde_json::Value = match serde_json::from_slice(&output.stdout) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Failed to parse pw-dump JSON: {}", e);
                return Vec::new();
            }
        };

        let default_source_name = Self::get_default_source_name();

        let mut ports_map: std::collections::HashMap<u64, Vec<String>> = std::collections::HashMap::new();

        if let Some(items) = json_val.as_array() {
            // First collect all output ports for nodes
            for item in items {
                if item.get("type").and_then(|t| t.as_str()) == Some("PipeWire:Interface:Port") {
                    let props = item.get("info").and_then(|i| i.get("props"));
                    let direction = item.get("info").and_then(|i| i.get("direction")).and_then(|d| d.as_str());

                    if direction == Some("output") {
                        if let Some(props) = props {
                            if let (Some(node_id), Some(port_name)) = (
                                props.get("node.id").and_then(|id| id.as_u64()),
                                props.get("port.name").and_then(|p| p.as_str()),
                            ) {
                                ports_map.entry(node_id).or_default().push(port_name.to_string());
                            }
                        }
                    }
                }
            }

            // Now extract Audio/Source nodes
            let mut devices = Vec::new();
            for item in items {
                if item.get("type").and_then(|t| t.as_str()) == Some("PipeWire:Interface:Node") {
                    let props = item.get("info").and_then(|i| i.get("props"));
                    if let Some(props) = props {
                        let media_class = props.get("media.class").and_then(|m| m.as_str()).unwrap_or("");
                        if media_class == "Audio/Source" {
                            let node_name = props.get("node.name").and_then(|n| n.as_str()).unwrap_or("");
                            // Skip StudioMic's own output source
                            if node_name.contains("studiomic") {
                                continue;
                            }

                            let id = item.get("id").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                            let desc = props.get("node.description")
                                .or_else(|| props.get("node.nick"))
                                .and_then(|d| d.as_str())
                                .unwrap_or(node_name)
                                .to_string();

                            let is_default = default_source_name.as_deref() == Some(node_name);
                            let ports = ports_map.get(&(id as u64)).cloned().unwrap_or_default();

                            devices.push(AudioDevice {
                                id,
                                name: node_name.to_string(),
                                description: desc,
                                is_default,
                                ports,
                            });
                        }
                    }
                }
            }
            return devices;
        }

        Vec::new()
    }

    /// Queries WirePlumber for the current default audio source name
    pub fn get_default_source_name() -> Option<String> {
        let output = Command::new("wpctl")
            .args(["inspect", "@DEFAULT_AUDIO_SOURCE@"])
            .output()
            .ok()?;
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

    /// Automatically links a chosen physical input device to a specific StudioMic capture port
    pub fn link_source_to_studiomic_target(device_name: &str, target_node: &str) -> Result<(), String> {
        println!("Linking physical audio source '{}' to '{}'...", device_name, target_node);

        // Find target device
        let devices = Self::get_input_devices();
        let target_device = devices.iter().find(|d| d.name == device_name || d.description == device_name)
            .ok_or_else(|| format!("Device '{}' not found in active sources", device_name))?;

        // 1. Unlink any previous physical links to target_node
        if let Ok(output) = Command::new("pw-link").args(["-l"]).output() {
            let links_str = String::from_utf8_lossy(&output.stdout);
            let mut current_source = "";
            let target_prefix = format!("{}:", target_node);
            for line in links_str.lines() {
                let trimmed = line.trim();
                if !trimmed.starts_with('|') && trimmed.contains(':') {
                    current_source = trimmed;
                } else if trimmed.contains(&target_prefix) {
                    let parts: Vec<&str> = trimmed.split_whitespace().collect();
                    if let Some(target_port) = parts.last() {
                        let _ = Command::new("pw-link")
                            .args(["-d", current_source, target_port])
                            .output();
                    }
                }
            }
        }

        // 2. Link selected source ports to target_node
        let ports_to_link = if target_device.ports.contains(&"capture_MONO".to_string()) {
            vec![("capture_MONO", "input_FL"), ("capture_MONO", "input_FR")]
        } else if target_device.ports.contains(&"capture_FL".to_string()) && target_device.ports.contains(&"capture_FR".to_string()) {
            vec![("capture_FL", "input_FL"), ("capture_FR", "input_FR")]
        } else if let Some(first) = target_device.ports.first() {
            vec![(first.as_str(), "input_FL"), (first.as_str(), "input_FR")]
        } else {
            vec![("capture_1", "input_FL")]
        };

        for (src_p, dst_p) in ports_to_link {
            let src_spec = format!("{}:{}", target_device.name, src_p);
            let dst_spec = format!("{}:{}", target_node, dst_p);
            let _ = Command::new("pw-link")
                .args([&src_spec, &dst_spec])
                .output();
            println!("Linked {} -> {}", src_spec, dst_spec);
        }

        println!("Successfully configured hardware link for '{}' -> {}", target_device.description, target_node);
        Ok(())
    }

    /// Automatically links a chosen physical input device to StudioMic's filter-chain capture port
    pub fn link_source_to_studiomic(device_name: &str) -> Result<(), String> {
        Self::link_source_to_studiomic_target(device_name, "studiomic.capture")
    }

    /// Automatically discovers hardware capture cards that provide BOTH video and audio
    pub fn get_capture_cards() -> Vec<CaptureCardDevice> {
        let input_devices = Self::get_input_devices();
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

                    // Check if parent hardware path matches an ALSA audio device
                    let device_link = entry.path().join("device");
                    let canonical_parent = device_link.canonicalize().ok();

                    for audio_dev in &input_devices {
                        let audio_lower = audio_dev.name.to_lowercase();
                        let desc_lower = audio_dev.description.to_lowercase();
                        let card_lower = card_name.to_lowercase();

                        // Check name correlation or hardware path match
                        let is_match = (card_lower.len() > 3 && desc_lower.contains(&card_lower))
                            || (card_lower.len() > 3 && audio_lower.contains(&card_lower))
                            || canonical_parent.as_ref().map_or(false, |cp| {
                                let _cp_str = cp.to_string_lossy();
                                // Check if this parent path appears in /sys/class/sound
                                std::fs::read_dir("/sys/class/sound").ok().map_or(false, |sound_entries| {
                                    sound_entries.flatten().any(|se| {
                                        se.path().canonicalize().ok().map_or(false, |sp| {
                                            sp.starts_with(cp.as_path()) || cp.starts_with(sp.as_path())
                                        })
                                    })
                                })
                            });

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
        for audio_dev in &input_devices {
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

    /// Links a capture card audio source to Channel 5 (studiomic_capture.capture)
    pub fn link_capture_card_to_channel(audio_source: &str, channel_id: &str) -> Result<(), String> {
        let target_node = format!("studiomic_{}.capture", channel_id);
        Self::link_source_to_studiomic_target(audio_source, &target_node)
    }
}
