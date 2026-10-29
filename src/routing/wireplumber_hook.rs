use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{LazyLock, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookedStream {
    pub node_id: u32,
    pub id: u32,
    pub pid: u32,
    pub app_name: String,
    pub name: String,
    pub binary: String,
    pub media_name: String,
    pub subtitle: String,
    pub icon: String,
    pub stream_type: String,
    pub app_type: String,
    pub is_live: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledApp {
    pub id: u32,
    pub name: String,
    pub app_name: String,
    pub subtitle: String,
    pub icon: String,
    pub app_type: String,
    pub stream_type: String,
    pub path: String,
    pub binary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledMediaPlayer {
    pub id: String,
    pub name: String,
    pub icon: String,
    pub binary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledBrowser {
    pub id: String,
    pub name: String,
    pub icon: String,
    pub binary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveBrowserTab {
    pub node_id: u32,
    pub browser_name: String,
    pub raw_title: String,
    pub clean_title: String,
    pub display_title: String,
}

static ASSIGNED_ROUTES: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    m.insert("voice".to_string(), "discord".to_string());
    m.insert("game".to_string(), "steam".to_string());
    m.insert("music".to_string(), "media_player".to_string());
    m.insert("alerts".to_string(), "alerts".to_string());
    Mutex::new(m)
});

pub struct StreamWatcher;

impl StreamWatcher {
    /// Detects if an audio stream is a game (Wine/Proton, Steam, or tagged role)
    pub fn is_game_stream(app_name: &str, media_role: &str, binary_path: &str) -> bool {
        let app_lower = app_name.to_lowercase();
        let bin_lower = binary_path.to_lowercase();

        media_role.eq_ignore_ascii_case("game")
            || app_lower.contains("steam")
            || bin_lower.contains("steamapps")
            || bin_lower.contains("pressure-vessel")
            || bin_lower.contains("wine")
            || bin_lower.contains("proton")
            || app_lower.contains("cs2")
            || app_lower.contains("counter-strike")
            || app_lower.contains("cyberpunk")
            || app_lower.contains("helldivers")
    }

    /// Strips leading notification counts like (320) or [Live] from tab titles
    pub fn clean_tab_title(title: &str) -> String {
        let trimmed = title.trim();
        // Check for leading (123) or [123]
        if let Some(rest) = trimmed.strip_prefix('(') {
            if let Some(close_idx) = rest.find(')') {
                let inside = &rest[..close_idx];
                if inside.chars().all(|c| c.is_ascii_digit()) {
                    return rest[close_idx + 1..].trim().to_string();
                }
            }
        }
        if let Some(rest) = trimmed.strip_prefix('[') {
            if let Some(close_idx) = rest.find(']') {
                let inside = &rest[..close_idx];
                if inside.chars().all(|c| c.is_ascii_digit()) || inside.eq_ignore_ascii_case("live") {
                    return rest[close_idx + 1..].trim().to_string();
                }
            }
        }
        trimmed.to_string()
    }

    /// Detects if an audio stream is any common web browser
    pub fn is_browser_stream(app_name: &str) -> bool {
        let n = app_name.to_lowercase();
        n.contains("zen")
            || n.contains("chrome")
            || n.contains("firefox")
            || n.contains("brave")
            || n.contains("edge")
            || n.contains("opera")
            || n.contains("vivaldi")
            || n.contains("arc")
            || n.contains("chromium")
            || n.contains("thorium")
            || n.contains("bromite")
            || n.contains("cromite")
            || n.contains("qutebrowser")
            || n.contains("helium")
            || n.contains("librewolf")
            || n.contains("tor")
            || n.contains("waterfox")
            || n.contains("mullvad")
            || n.contains("floorp")
            || n.contains("midori")
            || n.contains("palemoon")
            || n.contains("basilisk")
            || n.contains("seamonkey")
            || n.contains("safari")
            || n.contains("orion")
            || n.contains("epiphany")
            || n.contains("falkon")
            || n.contains("luakit")
            || n.contains("icab")
            || n.contains("ladybird")
            || n.contains("dillo")
            || n.contains("lynx")
            || n.contains("elinks")
    }

    /// Scans active audio streams across PipeWire in real time using pw-dump
    pub fn scan_live_streams() -> Vec<HookedStream> {
        let mut streams = Vec::new();

        let output = match Command::new("pw-dump").arg("Node").output() {
            Ok(o) => o,
            Err(_) => return streams,
        };

        let json_val: serde_json::Value = match serde_json::from_slice(&output.stdout) {
            Ok(v) => v,
            Err(_) => return streams,
        };

        let nodes = match json_val.as_array() {
            Some(arr) => arr,
            None => return streams,
        };

        for node in nodes {
            let props = match node.pointer("/info/props") {
                Some(p) => p,
                None => continue,
            };

            let media_class = props.get("media.class").and_then(|v| v.as_str()).unwrap_or("");
            if media_class != "Stream/Output/Audio" {
                continue;
            }

            let node_id = node.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let pid = props.get("application.process.id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

            let raw_app_name = props.get("application.name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let binary = props.get("application.process.binary")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let media_name = props.get("media.name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let media_role = props.get("media.role")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            // Friendly display name & categorization
            let mut app_name = raw_app_name.clone();
            if app_name.contains("WEBRTC") || binary.to_lowercase().contains("discord") || app_name.to_lowercase().contains("discord") {
                app_name = "Discord (Voice Chat)".to_string();
            } else if app_name.is_empty() {
                app_name = binary.clone();
            }
            if app_name.is_empty() {
                app_name = "Audio Stream".to_string();
            }

            let is_game = Self::is_game_stream(&app_name, media_role, &binary);
            let is_browser = Self::is_browser_stream(&app_name);

            let (stream_type, icon) = if is_game {
                ("Game Audio".to_string(), "sports_esports".to_string())
            } else if is_browser {
                ("Web Browser".to_string(), "language".to_string())
            } else if app_name.to_lowercase().contains("discord") || app_name.to_lowercase().contains("voice") {
                ("Voice Chat".to_string(), "chat".to_string())
            } else if app_name.to_lowercase().contains("spotify") || app_name.to_lowercase().contains("mpv") || app_name.to_lowercase().contains("vlc") {
                ("Media Player".to_string(), "music_note".to_string())
            } else if app_name.to_lowercase().contains("obs") {
                ("Broadcast / OBS".to_string(), "notifications".to_string())
            } else {
                ("Audio Output".to_string(), "volume_up".to_string())
            };

            streams.push(HookedStream {
                node_id,
                id: node_id,
                pid,
                app_name: app_name.clone(),
                name: app_name,
                binary,
                media_name: media_name.clone(),
                subtitle: media_name,
                icon,
                stream_type: stream_type.clone(),
                app_type: stream_type,
                is_live: true,
            });
        }

        streams
    }

    /// Unlinks any PipeWire links feeding into `target_in_port_ids` whose output port is NOT in `allowed_out_port_ids`
    fn unlink_foreign_inputs_from_target(target_in_port_ids: &[u32], allowed_out_port_ids: &[u32]) {
        let links_out = match Command::new("pw-dump").arg("Link").output() {
            Ok(o) => o,
            Err(_) => return,
        };
        let links_json: serde_json::Value = match serde_json::from_slice(&links_out.stdout) {
            Ok(v) => v,
            Err(_) => return,
        };
        let links = match links_json.as_array() {
            Some(a) => a,
            None => return,
        };

        for link in links {
            let link_id = link.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let in_port = link.pointer("/info/input-port-id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let out_port = link.pointer("/info/output-port-id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

            if target_in_port_ids.contains(&in_port) && !allowed_out_port_ids.contains(&out_port) {
                if link_id > 0 {
                    let _ = Command::new("pw-link").args(["-d", &link_id.to_string()]).output();
                }
                let _ = Command::new("pw-link")
                    .args(["-d", &out_port.to_string(), &in_port.to_string()])
                    .output();
                println!("Isolated channel: unlinked foreign port #{} from target port #{}", out_port, in_port);
            }
        }
    }

    /// Isolates and links a single active PipeWire stream (e.g., an individual Browser Tab) to a virtual mixer channel
    pub fn route_stream_to_channel(src_node_id: u32, channel_id: &str) -> bool {
        let ch_clean = channel_id.trim_start_matches("studiomic_");
        {
            let mut routes = ASSIGNED_ROUTES.lock().unwrap();
            routes.insert(ch_clean.to_string(), format!("node:{}", src_node_id));
        }
        Self::link_node_to_channel_ports(src_node_id, ch_clean, true)
    }

    /// Internal port linker with optional single-stream isolation
    fn link_node_to_channel_ports(src_node_id: u32, ch_clean: &str, isolate_single: bool) -> bool {
        let target_capture_name = format!("studiomic_{}.capture", ch_clean);
        let target_fallback_name = format!("studiomic_{}", ch_clean);

        // 1. Fetch nodes and ports from PipeWire
        let nodes_out = match Command::new("pw-dump").arg("Node").output() {
            Ok(o) => o,
            Err(e) => {
                eprintln!("pw-dump Node failed: {}", e);
                return false;
            }
        };
        let ports_out = match Command::new("pw-dump").arg("Port").output() {
            Ok(o) => o,
            Err(e) => {
                eprintln!("pw-dump Port failed: {}", e);
                return false;
            }
        };

        let nodes_json: serde_json::Value = match serde_json::from_slice(&nodes_out.stdout) {
            Ok(v) => v,
            Err(_) => return false,
        };
        let ports_json: serde_json::Value = match serde_json::from_slice(&ports_out.stdout) {
            Ok(v) => v,
            Err(_) => return false,
        };

        let nodes = match nodes_json.as_array() {
            Some(a) => a,
            None => return false,
        };
        let ports = match ports_json.as_array() {
            Some(a) => a,
            None => return false,
        };

        // 2. Find target node ID (prefer studiomic_<ch>.capture, fallback to studiomic_<ch>)
        let mut target_node_id = None;
        for n in nodes {
            if let Some(props) = n.pointer("/info/props") {
                if let Some(name) = props.get("node.name").and_then(|v| v.as_str()) {
                    if name == target_capture_name {
                        target_node_id = n.get("id").and_then(|v| v.as_u64()).map(|id| id as u32);
                        break;
                    }
                }
            }
        }
        if target_node_id.is_none() {
            for n in nodes {
                if let Some(props) = n.pointer("/info/props") {
                    if let Some(name) = props.get("node.name").and_then(|v| v.as_str()) {
                        if name == target_fallback_name {
                            target_node_id = n.get("id").and_then(|v| v.as_u64()).map(|id| id as u32);
                            break;
                        }
                    }
                }
            }
        }

        let target_id = match target_node_id {
            Some(id) => id,
            None => {
                eprintln!("Target channel node '{}' not found in PipeWire graph", target_capture_name);
                return false;
            }
        };

        // 3. Find input ports of target node
        let mut target_in_ports: Vec<(u32, String)> = Vec::new();
        for p in ports {
            let p_node_id = p.pointer("/info/props/node.id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let dir = p.pointer("/info/props/port.direction").and_then(|v| v.as_str()).unwrap_or("");
            let p_name = p.pointer("/info/props/port.name").and_then(|v| v.as_str()).unwrap_or("");

            if p_node_id == target_id && dir == "in" && !p_name.contains("monitor") {
                let p_id = p.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                if p_id > 0 {
                    target_in_ports.push((p_id, p_name.to_string()));
                }
            }
        }
        target_in_ports.sort_by(|a, b| a.1.cmp(&b.1));

        // 4. Find output ports of source node
        let mut source_out_ports: Vec<(u32, String)> = Vec::new();
        for p in ports {
            let p_node_id = p.pointer("/info/props/node.id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let dir = p.pointer("/info/props/port.direction").and_then(|v| v.as_str()).unwrap_or("");
            let p_name = p.pointer("/info/props/port.name").and_then(|v| v.as_str()).unwrap_or("");

            if p_node_id == src_node_id && dir == "out" && !p_name.contains("monitor") {
                let p_id = p.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                if p_id > 0 {
                    source_out_ports.push((p_id, p_name.to_string()));
                }
            }
        }
        source_out_ports.sort_by(|a, b| a.1.cmp(&b.1));

        if target_in_ports.is_empty() || source_out_ports.is_empty() {
            return false;
        }

        // If isolating a single stream (such as an individual browser tab), unlink all other streams from this channel first
        if isolate_single {
            let in_ids: Vec<u32> = target_in_ports.iter().map(|(id, _)| *id).collect();
            let allowed_out_ids: Vec<u32> = source_out_ports.iter().map(|(id, _)| *id).collect();
            Self::unlink_foreign_inputs_from_target(&in_ids, &allowed_out_ids);
        }

        // 5. Establish links using port IDs
        let mut linked_count = 0;
        if source_out_ports.len() == 1 && target_in_ports.len() >= 2 {
            // Mono source to Stereo target
            let src_id = source_out_ports[0].0.to_string();
            let _ = Command::new("pw-link").args([&src_id, &target_in_ports[0].0.to_string()]).output();
            let _ = Command::new("pw-link").args([&src_id, &target_in_ports[1].0.to_string()]).output();
            linked_count += 2;
        } else {
            for (src_p, dst_p) in source_out_ports.iter().zip(target_in_ports.iter()) {
                let _ = Command::new("pw-link")
                    .args([&src_p.0.to_string(), &dst_p.0.to_string()])
                    .output();
                linked_count += 1;
            }
        }

        println!(
            "Routed Stream #{} -> Channel '{}' (isolated={}, {} port links active)",
            src_node_id, ch_clean, isolate_single, linked_count
        );
        true
    }

    /// Routes any running stream matching an app query or binary to a target mixer channel
    pub fn route_app_to_channel(app_query: &str, channel_id: &str) -> usize {
        let ch_clean = channel_id.trim_start_matches("studiomic_");
        if let Some(node_str) = app_query.strip_prefix("node:") {
            if let Ok(node_id) = node_str.parse::<u32>() {
                return if Self::route_stream_to_channel(node_id, ch_clean) { 1 } else { 0 };
            }
        }

        // Store in assigned routes table
        {
            let mut routes = ASSIGNED_ROUTES.lock().unwrap();
            routes.insert(ch_clean.to_string(), app_query.to_string());
        }

        let output = match Command::new("pw-dump").arg("Node").output() {
            Ok(o) => o,
            Err(_) => return 0,
        };

        let json_val: serde_json::Value = match serde_json::from_slice(&output.stdout) {
            Ok(v) => v,
            Err(_) => return 0,
        };

        let nodes = match json_val.as_array() {
            Some(a) => a,
            None => return 0,
        };

        let q = app_query.to_lowercase();
        let mut matching_nodes = Vec::new();

        for node in nodes {
            let props = match node.pointer("/info/props") {
                Some(p) => p,
                None => continue,
            };

            let media_class = props.get("media.class").and_then(|v| v.as_str()).unwrap_or("");
            if media_class != "Stream/Output/Audio" {
                continue;
            }

            let node_id = node.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let app_name = props.get("application.name").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
            let binary = props.get("application.process.binary").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
            let media_role = props.get("media.role").and_then(|v| v.as_str()).unwrap_or("");

            let is_match = if q == "discord" {
                app_name.contains("discord") || binary.contains("discord") || app_name.contains("webrtc") || binary.contains("vesktop") || binary.contains("webcord")
            } else if q == "steam" || q == "game" {
                Self::is_game_stream(&app_name, media_role, &binary)
            } else if q == "browser" {
                Self::is_browser_stream(&app_name) || binary.contains("browser")
            } else if q == "media_player" || q == "music" {
                app_name.contains("spotify") || binary.contains("spotify") || binary.contains("vlc") || binary.contains("mpv") || binary.contains("cider") || binary.contains("strawberry") || binary.contains("amberol") || binary.contains("rhythmbox")
            } else if q == "alerts" {
                app_name.contains("obs") || binary.contains("obs") || app_name.contains("soundboard") || app_name.contains("notification")
            } else {
                app_name.contains(&q) || binary.contains(&q)
            };

            if is_match {
                matching_nodes.push(node_id);
            }
        }

        // Unlink foreign streams only if we actually found matching streams for this app
        if !matching_nodes.is_empty() {
            if let Ok(ports_out) = Command::new("pw-dump").arg("Port").output() {
                if let Ok(ports_json) = serde_json::from_slice::<serde_json::Value>(&ports_out.stdout) {
                    if let Some(ports) = ports_json.as_array() {
                        let target_capture_name = format!("studiomic_{}.capture", ch_clean);
                        let mut target_id = None;
                        for n in nodes {
                            if n.pointer("/info/props/node.name").and_then(|v| v.as_str()) == Some(&target_capture_name) {
                                target_id = n.get("id").and_then(|v| v.as_u64()).map(|id| id as u32);
                                break;
                            }
                        }
                        if let Some(tid) = target_id {
                            let mut in_ids = Vec::new();
                            let mut allowed_out_ids = Vec::new();
                            for p in ports {
                                let p_node_id = p.pointer("/info/props/node.id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                let dir = p.pointer("/info/props/port.direction").and_then(|v| v.as_str()).unwrap_or("");
                                let p_id = p.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                if p_node_id == tid && dir == "in" {
                                    in_ids.push(p_id);
                                } else if matching_nodes.contains(&p_node_id) && dir == "out" {
                                    allowed_out_ids.push(p_id);
                                }
                            }
                            if !in_ids.is_empty() && !allowed_out_ids.is_empty() {
                                Self::unlink_foreign_inputs_from_target(&in_ids, &allowed_out_ids);
                            }
                        }
                    }
                }
            }
        }

        let mut routed_count = 0;
        for node_id in &matching_nodes {
            if Self::link_node_to_channel_ports(*node_id, ch_clean, false) {
                routed_count += 1;
            }
        }

        if routed_count > 0 {
            println!("route_app_to_channel('{}' -> '{}') linked {} active streams", app_query, channel_id, routed_count);
        }
        routed_count
    }

    /// Automatically checks running streams and ensures assigned routes remain active
    pub fn auto_route_assigned_channels() {
        let routes = {
            let r = ASSIGNED_ROUTES.lock().unwrap();
            r.clone()
        };

        for (channel_id, app_query) in routes {
            if let Some(node_str) = app_query.strip_prefix("node:") {
                if let Ok(node_id) = node_str.parse::<u32>() {
                    let _ = Self::link_node_to_channel_ports(node_id, &channel_id, true);
                }
            } else {
                let _ = Self::route_app_to_channel(&app_query, &channel_id);
            }
        }
    }

    /// Scans installed media players on the system and returns only the ones present
    pub fn scan_installed_media_players() -> Vec<InstalledMediaPlayer> {
        let common = [
            ("mpv", "MPV Media Player", "movie"),
            ("vlc", "VLC Media Player", "movie"),
            ("spotify", "Spotify Desktop", "music_note"),
            ("cider", "Cider (Apple Music)", "headphones"),
            ("rhythmbox", "Rhythmbox Music Player", "music_note"),
            ("amberol", "Amberol Music Player", "music_note"),
            ("audacious", "Audacious Audio Player", "music_note"),
            ("celluloid", "Celluloid Video Player", "movie"),
            ("totem", "GNOME Videos (Totem)", "movie"),
            ("strawberry", "Strawberry Music Player", "music_note"),
            ("clementine", "Clementine Music Player", "music_note"),
            ("elisa", "Elisa Music Player", "music_note"),
            ("lollypop", "Lollypop Music Player", "music_note"),
            ("deadbeef", "DeaDBeeF Audio Player", "music_note"),
            ("audacity", "Audacity Audio Editor", "mic"),
            ("feishin", "Feishin Subsonic Client", "music_note"),
            ("tauonmb", "Tauon Music Box", "music_note"),
        ];

        let path_dirs: Vec<PathBuf> = std::env::var("PATH")
            .unwrap_or_default()
            .split(':')
            .map(PathBuf::from)
            .collect();

        let desktop_dirs = [
            PathBuf::from("/usr/share/applications"),
            dirs_fallback_local_applications(),
            PathBuf::from("/var/lib/flatpak/exports/share/applications"),
        ];

        let mut installed = Vec::new();

        for &(bin, name, icon) in &common {
            let mut found = false;

            for dir in &path_dirs {
                let bin_path = dir.join(bin);
                if bin_path.is_file() {
                    found = true;
                    break;
                }
            }

            if !found {
                for ddir in &desktop_dirs {
                    if let Ok(entries) = fs::read_dir(ddir) {
                        for entry in entries.flatten() {
                            let file_name = entry.file_name().to_string_lossy().to_lowercase();
                            if file_name.contains(bin) && file_name.ends_with(".desktop") {
                                found = true;
                                break;
                            }
                        }
                    }
                    if found {
                        break;
                    }
                }
            }

            if found {
                installed.push(InstalledMediaPlayer {
                    id: bin.to_string(),
                    name: name.to_string(),
                    icon: icon.to_string(),
                    binary: bin.to_string(),
                });
            }
        }

        installed
    }

    /// Scans installed web browsers across the system covering all common Linux and standard browsers
    pub fn scan_installed_web_browsers() -> Vec<InstalledBrowser> {
        let common = [
            ("google-chrome", "Google Chrome", "public", &["google-chrome", "google-chrome-stable", "chrome"][..]),
            ("microsoft-edge", "Microsoft Edge", "public", &["microsoft-edge", "microsoft-edge-stable", "edge"][..]),
            ("brave", "Brave Browser", "shield", &["brave-browser", "brave"][..]),
            ("opera", "Opera", "public", &["opera"][..]),
            ("opera-gx", "Opera GX", "sports_esports", &["opera-gx"][..]),
            ("vivaldi", "Vivaldi", "public", &["vivaldi", "vivaldi-stable"][..]),
            ("arc", "Arc Browser", "travel_explore", &["arc-browser", "arc"][..]),
            ("chromium", "Chromium", "public", &["chromium", "chromium-browser"][..]),
            ("ungoogled-chromium", "Ungoogled Chromium", "security", &["ungoogled-chromium"][..]),
            ("thorium", "Thorium", "bolt", &["thorium-browser", "thorium"][..]),
            ("bromite", "Bromite", "public", &["bromite"][..]),
            ("cromite", "Cromite", "shield", &["cromite"][..]),
            ("qutebrowser", "qutebrowser", "terminal", &["qutebrowser"][..]),
            ("helium", "Helium", "public", &["helium"][..]),
            ("firefox", "Mozilla Firefox", "public", &["firefox", "firefox-esr"][..]),
            ("zen", "Zen Browser", "tab", &["zen-browser", "zen-bin", "zen"][..]),
            ("librewolf", "LibreWolf", "shield", &["librewolf"][..]),
            ("tor-browser", "Tor Browser", "vpn_lock", &["tor-browser", "torbrowser-launcher"][..]),
            ("waterfox", "Waterfox", "water_drop", &["waterfox"][..]),
            ("mullvad-browser", "Mullvad Browser", "vpn_lock", &["mullvad-browser"][..]),
            ("floorp", "Floorp", "public", &["floorp"][..]),
            ("midori", "Midori", "eco", &["midori"][..]),
            ("palemoon", "Pale Moon", "dark_mode", &["palemoon"][..]),
            ("basilisk", "Basilisk", "public", &["basilisk"][..]),
            ("seamonkey", "SeaMonkey", "public", &["seamonkey"][..]),
            ("safari", "Apple Safari", "explore", &["safari"][..]),
            ("orion", "Orion", "public", &["orion"][..]),
            ("epiphany", "GNOME Web (Epiphany)", "public", &["epiphany", "org.gnome.Epiphany"][..]),
            ("falkon", "Falkon", "public", &["falkon"][..]),
            ("luakit", "Luakit", "code", &["luakit"][..]),
            ("icab", "iCab", "public", &["icab"][..]),
            ("ladybird", "Ladybird", "flutter", &["ladybird"][..]),
            ("dillo", "Dillo", "public", &["dillo"][..]),
            ("lynx", "Lynx", "terminal", &["lynx"][..]),
            ("elinks", "ELinks", "terminal", &["elinks"][..]),
        ];

        let mut path_dirs: Vec<PathBuf> = std::env::var("PATH")
            .unwrap_or_default()
            .split(':')
            .map(PathBuf::from)
            .collect();
        path_dirs.push(PathBuf::from("/opt/zen-browser-bin"));
        path_dirs.push(PathBuf::from("/opt/google/chrome"));

        let desktop_dirs = [
            PathBuf::from("/usr/share/applications"),
            dirs_fallback_local_applications(),
            PathBuf::from("/var/lib/flatpak/exports/share/applications"),
        ];

        let mut installed = Vec::new();

        for &(id, name, icon, aliases) in &common {
            let mut found = false;

            for alias in aliases {
                for dir in &path_dirs {
                    let bin_path = dir.join(alias);
                    if bin_path.is_file() {
                        found = true;
                        break;
                    }
                }
                if found { break; }

                for ddir in &desktop_dirs {
                    if let Ok(entries) = fs::read_dir(ddir) {
                        for entry in entries.flatten() {
                            let file_name = entry.file_name().to_string_lossy().to_lowercase();
                            if file_name.contains(alias) && file_name.ends_with(".desktop") {
                                found = true;
                                break;
                            }
                        }
                    }
                    if found { break; }
                }
                if found { break; }
            }

            if found {
                installed.push(InstalledBrowser {
                    id: id.to_string(),
                    name: name.to_string(),
                    icon: icon.to_string(),
                    binary: aliases[0].to_string(),
                });
            }
        }

        installed
    }

    /// Scans active web browser audio tabs across PipeWire and formats with Tab Title First
    pub fn scan_active_browser_tabs() -> Vec<ActiveBrowserTab> {
        let mut tabs = Vec::new();

        let output = match Command::new("pw-dump").arg("Node").output() {
            Ok(o) => o,
            Err(_) => return tabs,
        };

        let json_val: serde_json::Value = match serde_json::from_slice(&output.stdout) {
            Ok(v) => v,
            Err(_) => return tabs,
        };

        let nodes = match json_val.as_array() {
            Some(a) => a,
            None => return tabs,
        };

        for node in nodes {
            let props = match node.pointer("/info/props") {
                Some(p) => p,
                None => continue,
            };

            let media_class = props.get("media.class").and_then(|v| v.as_str()).unwrap_or("");
            if media_class != "Stream/Output/Audio" {
                continue;
            }

            let app_name = props.get("application.name").and_then(|v| v.as_str()).unwrap_or("");
            let binary = props.get("application.process.binary").and_then(|v| v.as_str()).unwrap_or("");
            let raw_media = props.get("media.name").and_then(|v| v.as_str()).unwrap_or("");

            if !Self::is_browser_stream(app_name) && !Self::is_browser_stream(binary) {
                continue;
            }

            let node_id = node.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let clean_title = Self::clean_tab_title(raw_media);
            let display_tab = if clean_title.is_empty() || clean_title.eq_ignore_ascii_case("audiostream") {
                "Active Audio Stream".to_string()
            } else {
                clean_title.clone()
            };

            let browser_name = if app_name.is_empty() {
                "Web Browser".to_string()
            } else {
                let clean_app = app_name.trim();
                if clean_app.to_lowercase().ends_with("browser") {
                    clean_app.to_string()
                } else {
                    format!("{} Browser", clean_app)
                }
            };

            // Formatted with TAB TITLE FIRST for clear readability
            let display_title = format!("▶ \"{}\" · {}", display_tab, browser_name);

            tabs.push(ActiveBrowserTab {
                node_id,
                browser_name,
                raw_title: raw_media.to_string(),
                clean_title: display_tab,
                display_title,
            });
        }

        tabs
    }

    /// Scans .desktop files from system and user application directories
    pub fn scan_installed_apps() -> Vec<InstalledApp> {
        let mut apps = Vec::new();
        let mut id_counter = 100u32;

        let dirs = [
            PathBuf::from("/usr/share/applications"),
            dirs_fallback_local_applications(),
            PathBuf::from("/var/lib/flatpak/exports/share/applications"),
        ];

        for dir in &dirs {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|s| s.to_str()) != Some("desktop") {
                        continue;
                    }

                    if let Some(app) = parse_desktop_entry(&path, id_counter) {
                        id_counter += 1;
                        apps.push(app);
                    }
                }
            }
        }

        apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        apps
    }
}

fn dirs_fallback_local_applications() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local/share/applications")
    } else {
        PathBuf::from("/root/.local/share/applications")
    }
}

fn parse_desktop_entry(path: &Path, id: u32) -> Option<InstalledApp> {
    let content = fs::read_to_string(path).ok()?;
    let mut name = String::new();
    let mut comment = String::new();
    let mut categories = String::new();
    let mut exec = String::new();
    let mut in_main_section = false;

    for line in content.lines() {
        let line = line.trim();
        if line == "[Desktop Entry]" {
            in_main_section = true;
            continue;
        } else if line.starts_with('[') && in_main_section {
            break;
        }

        if in_main_section {
            if line.starts_with("Name=") && name.is_empty() {
                name = line["Name=".len()..].trim().to_string();
            } else if line.starts_with("Comment=") && comment.is_empty() {
                comment = line["Comment=".len()..].trim().to_string();
            } else if line.starts_with("GenericName=") && comment.is_empty() {
                comment = line["GenericName=".len()..].trim().to_string();
            } else if line.starts_with("Exec=") && exec.is_empty() {
                exec = line["Exec=".len()..].trim().to_string();
            } else if line.starts_with("Categories=") {
                categories = line["Categories=".len()..].trim().to_string();
            } else if line.starts_with("NoDisplay=true") {
                return None;
            }
        }
    }

    if name.is_empty() {
        return None;
    }

    let bin_name = exec
        .split_whitespace()
        .next()
        .map(|s| Path::new(s).file_name().and_then(|f| f.to_str()).unwrap_or(s))
        .unwrap_or("")
        .to_string();

    let cat_lower = categories.to_lowercase();
    let (app_type, icon) = if cat_lower.contains("audio") || cat_lower.contains("music") {
        ("Audio / Music", "music_note")
    } else if cat_lower.contains("video") || cat_lower.contains("player") {
        ("Media Player", "movie")
    } else if cat_lower.contains("game") {
        ("Game", "sports_esports")
    } else if cat_lower.contains("network") || cat_lower.contains("webbrowser") {
        ("Web Browser", "language")
    } else if cat_lower.contains("chat") || cat_lower.contains("telephony") {
        ("Voice Chat", "chat")
    } else {
        ("Application", "apps")
    };

    if comment.is_empty() {
        comment = format!("{}.desktop", path.file_stem().and_then(|s| s.to_str()).unwrap_or("app"));
    }

    Some(InstalledApp {
        id,
        name: name.clone(),
        app_name: name,
        subtitle: comment,
        icon: icon.to_string(),
        app_type: app_type.to_string(),
        stream_type: app_type.to_string(),
        path: path.to_string_lossy().to_string(),
        binary: bin_name,
    })
}
