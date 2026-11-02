//! Application stream discovery and auto-routing.
//!
//! Streams are moved to a channel sink with WirePlumber's `target.object` metadata (the same
//! mechanism pavucontrol uses), so an app never plays to its old output AND the channel at once.
//! Studio Mic's own nodes (`StudioMic_*`, meters, preview player) are always ignored to prevent
//! monitor feedback loops.

use crate::audio::filter_chain::channel_sink;
use crate::audio::pw;
use crate::config::VirtualChannelConfig;
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
    /// Channel this stream is currently routed to by Studio Mic, if any.
    #[serde(default)]
    pub channel: Option<String>,
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

/// stream node id -> channel id that Studio Mic moved it to.
static ROUTED: LazyLock<Mutex<HashMap<u32, String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// stream node id -> channel id pinned explicitly by the user (e.g. one browser tab).
static PINNED: LazyLock<Mutex<HashMap<u32, String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Voice-chat clients: their channel is never auto-ducked.
pub const COMMS_APPS: [&str; 5] = ["discord", "vesktop", "webcord", "teamspeak", "mumble"];

pub struct StreamWatcher;

struct StreamProps {
    id: u32,
    pid: u32,
    app_name: String,
    binary: String,
    media_name: String,
    media_role: String,
}

impl StreamWatcher {
    /// Feedback guard: true for any node that belongs to Studio Mic itself.
    pub fn is_self_stream(props: &serde_json::Value) -> bool {
        let get = |k: &str| props.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let me = std::process::id() as u64;
        let pid = props.get("application.process.id").and_then(|v| v.as_u64()).unwrap_or(0);
        let lower = |s: String| s.to_lowercase();
        pid == me
            || ["node.name", "media.name", "application.name"].iter().any(|k| {
                let v = lower(get(k));
                v.starts_with("studiomic_") || v.starts_with("studiomic") || v.starts_with("studio mic")
            })
            || get("node.group").starts_with("studiomic")
    }

    fn output_streams() -> Vec<StreamProps> {
        pw::nodes()
            .into_iter()
            .filter(|n| n.class == "Stream/Output/Audio" && !Self::is_self_stream(&n.props))
            .map(|n| {
                let s = |k: &str| n.props.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
                StreamProps {
                    id: n.id,
                    pid: n.props.get("application.process.id").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                    app_name: s("application.name"),
                    binary: s("application.process.binary"),
                    media_name: s("media.name"),
                    media_role: s("media.role"),
                }
            })
            .collect()
    }

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
            || bin_lower.contains("gamescope")
            || app_lower.contains("cs2")
            || app_lower.contains("counter-strike")
    }

    /// Strips leading notification counts like (320) or [Live] from tab titles
    pub fn clean_tab_title(title: &str) -> String {
        let trimmed = title.trim();
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
        [
            "zen", "chrome", "firefox", "brave", "edge", "opera", "vivaldi", "chromium", "thorium", "bromite",
            "cromite", "qutebrowser", "helium", "librewolf", "tor browser", "waterfox", "mullvad", "floorp",
            "midori", "palemoon", "basilisk", "seamonkey", "epiphany", "falkon", "luakit", "ladybird",
        ]
        .iter()
        .any(|b| n.contains(b))
    }

    fn classify(app_name: &str, binary: &str, media_role: &str) -> (String, String) {
        let a = app_name.to_lowercase();
        let b = binary.to_lowercase();
        if COMMS_APPS.iter().any(|c| a.contains(c) || b.contains(c)) || a.contains("webrtc") {
            ("Voice Chat".into(), "forum".into())
        } else if Self::is_game_stream(app_name, media_role, binary) {
            ("Game Audio".into(), "sports_esports".into())
        } else if Self::is_browser_stream(app_name) || Self::is_browser_stream(binary) {
            ("Web Browser".into(), "language".into())
        } else if ["spotify", "mpv", "vlc", "feishin", "elisa", "rhythmbox", "amberol", "mpd"].iter().any(|m| a.contains(m) || b.contains(m)) {
            ("Media Player".into(), "music_note".into())
        } else if a.contains("obs") {
            ("Broadcast / OBS".into(), "videocam".into())
        } else {
            ("Audio Output".into(), "volume_up".into())
        }
    }

    /// Running applications producing audio (Studio Mic's own nodes excluded).
    pub fn scan_live_streams() -> Vec<HookedStream> {
        let routed = ROUTED.lock().unwrap().clone();
        let pinned = PINNED.lock().unwrap().clone();
        Self::output_streams()
            .into_iter()
            .map(|s| {
                let mut app_name = if s.app_name.is_empty() { s.binary.clone() } else { s.app_name.clone() };
                if app_name.contains("WEBRTC") || s.binary.to_lowercase().contains("discord") {
                    app_name = "Discord (Voice Chat)".into();
                }
                if app_name.is_empty() {
                    app_name = "Audio Stream".into();
                }
                let (stream_type, icon) = Self::classify(&s.app_name, &s.binary, &s.media_role);
                HookedStream {
                    node_id: s.id,
                    id: s.id,
                    pid: s.pid,
                    app_name: app_name.clone(),
                    name: app_name,
                    binary: s.binary,
                    media_name: s.media_name.clone(),
                    subtitle: s.media_name,
                    icon,
                    stream_type: stream_type.clone(),
                    app_type: stream_type,
                    is_live: true,
                    channel: pinned.get(&s.id).or_else(|| routed.get(&s.id)).cloned(),
                }
            })
            .collect()
    }

    fn matches_channel(ch: &VirtualChannelConfig, s: &StreamProps) -> bool {
        if ch.kind == "hardware" {
            return false;
        }
        let a = s.app_name.to_lowercase();
        let b = Path::new(&s.binary).file_name().and_then(|f| f.to_str()).unwrap_or(&s.binary).to_lowercase();
        let full_b = s.binary.to_lowercase();
        let listed = ch.apps.iter().any(|app| {
            let q = app.to_lowercase();
            !q.is_empty() && (a.contains(&q) || b.contains(&q) || full_b.contains(&q))
        });
        if listed {
            return true;
        }
        if ch.route_mode == "auto" {
            if ch.id == "game" && Self::is_game_stream(&s.app_name, &s.media_role, &s.binary) {
                return true;
            }
            if ch.is_comms() && (a.contains("webrtc") || COMMS_APPS.iter().any(|c| a.contains(c) || b.contains(c))) {
                return true;
            }
        }
        false
    }

    /// Picks the channel for a stream: explicit pin > voice chat > listed/auto rules > desktop catch-all.
    fn pick_channel(channels: &[VirtualChannelConfig], s: &StreamProps) -> Option<String> {
        if let Some(ch) = channels.iter().filter(|c| c.is_comms()).find(|c| Self::matches_channel(c, s)) {
            return Some(ch.id.clone());
        }
        if let Some(ch) = channels.iter().find(|c| Self::matches_channel(c, s)) {
            return Some(ch.id.clone());
        }
        channels.iter().find(|c| c.route_mode == "desktop" && c.kind != "hardware").map(|c| c.id.clone())
    }

    /// Periodic auto-router (called from the engine maintenance loop every ~2 s, and right after
    /// app assignments change).
    pub fn auto_route(channels: &[VirtualChannelConfig]) {
        static SEEDED: std::sync::Once = std::sync::Once::new();
        SEEDED.call_once(Self::seed_from_metadata);
        let streams = Self::output_streams();
        let live: std::collections::HashSet<u32> = streams.iter().map(|s| s.id).collect();
        ROUTED.lock().unwrap().retain(|id, _| live.contains(id));
        PINNED.lock().unwrap().retain(|id, _| live.contains(id));
        let pinned = PINNED.lock().unwrap().clone();

        for s in &streams {
            let wanted = match pinned.get(&s.id) {
                Some(ch) if channels.iter().any(|c| &c.id == ch) => Some(ch.clone()),
                _ => Self::pick_channel(channels, s),
            };
            let current = ROUTED.lock().unwrap().get(&s.id).cloned();
            match (wanted, current) {
                (Some(w), Some(c)) if w == c => {}
                (Some(w), _) => {
                    if pw::move_stream(s.id, &channel_sink(&w)) {
                        println!("Routed '{}' ({}) -> channel '{}'", s.app_name, s.binary, w);
                        ROUTED.lock().unwrap().insert(s.id, w);
                    }
                }
                (None, Some(_)) => {
                    // No longer assigned anywhere: hand it back to the default output.
                    pw::clear_stream_target(s.id);
                    ROUTED.lock().unwrap().remove(&s.id);
                }
                (None, None) => {}
            }
        }
    }

    /// Pins one specific stream (e.g. a single browser tab) to a channel.
    pub fn route_stream_to_channel(src_node_id: u32, channel_id: &str) -> bool {
        PINNED.lock().unwrap().insert(src_node_id, channel_id.to_string());
        let ok = pw::move_stream(src_node_id, &channel_sink(channel_id));
        if ok {
            ROUTED.lock().unwrap().insert(src_node_id, channel_id.to_string());
        }
        ok
    }

    /// Releases a pinned stream back to automatic rules.
    pub fn unpin_stream(src_node_id: u32) {
        PINNED.lock().unwrap().remove(&src_node_id);
        ROUTED.lock().unwrap().remove(&src_node_id);
        pw::clear_stream_target(src_node_id);
    }

    /// Streams moved into a channel by an earlier Studio Mic run keep their `target.object`
    /// after a restart. Remember them, so a stream that no longer matches any rule is handed
    /// back to the default output instead of staying in its old channel.
    fn seed_from_metadata() {
        let Ok(out) = Command::new("pw-metadata").args(["-n", "default"]).output() else { return };
        let text = String::from_utf8_lossy(&out.stdout);
        let mut routed = ROUTED.lock().unwrap();
        for (id, ch) in parse_channel_targets(&text) {
            routed.entry(id).or_insert(ch);
        }
    }

    /// Before a channel disappears, hand its streams back to the default output (make-before-break).
    pub fn release_channel(channel_id: &str) {
        let ids: Vec<u32> = ROUTED
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, c)| c.as_str() == channel_id)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            pw::clear_stream_target(id);
            ROUTED.lock().unwrap().remove(&id);
            PINNED.lock().unwrap().remove(&id);
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

        let desktop_files = desktop_file_names();
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
                found = desktop_files.iter().any(|f| f.contains(bin));
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

        let desktop_files = desktop_file_names();
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
                if desktop_files.iter().any(|f| f.contains(alias)) {
                    found = true;
                    break;
                }
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
        // Shares the 250 ms node cache with scan_live_streams (was a second pw-dump per call).
        for node in pw::nodes() {
            let props = &node.props;
            if node.class != "Stream/Output/Audio" || Self::is_self_stream(props) {
                continue;
            }

            let app_name = props.get("application.name").and_then(|v| v.as_str()).unwrap_or("");
            let binary = props.get("application.process.binary").and_then(|v| v.as_str()).unwrap_or("");
            let raw_media = props.get("media.name").and_then(|v| v.as_str()).unwrap_or("");

            if !Self::is_browser_stream(app_name) && !Self::is_browser_stream(binary) {
                continue;
            }

            let node_id = node.id;
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

/// `(stream id, channel id)` for every `target.object` that points at a Studio Mic channel sink,
/// from `pw-metadata -n default` output lines such as
/// `update: id:87 key:'target.object' value:'StudioMic_Ch_music' type:''`.
fn parse_channel_targets(text: &str) -> Vec<(u32, String)> {
    let prefix = channel_sink("");
    text.lines()
        .filter(|l| l.contains("key:'target.object'"))
        .filter_map(|l| {
            let id = l.split("id:").nth(1)?.split_whitespace().next()?.parse::<u32>().ok()?;
            let value = l.split("value:'").nth(1)?.split('\'').next()?;
            value.strip_prefix(&prefix).map(|ch| (id, ch.to_string()))
        })
        .collect()
}

/// Lower-cased names of every .desktop file in the system, user and Flatpak application dirs,
/// read once per scan (the player/browser scans used to re-read the dirs for every name).
fn desktop_file_names() -> Vec<String> {
    [
        PathBuf::from("/usr/share/applications"),
        dirs_fallback_local_applications(),
        PathBuf::from("/var/lib/flatpak/exports/share/applications"),
    ]
    .iter()
    .filter_map(|d| fs::read_dir(d).ok())
    .flat_map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_lowercase()))
    .filter(|f| f.ends_with(".desktop"))
    .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_targets_pointing_at_channels() {
        let text = "update: id:0 key:'default.audio.sink' value:'{\"name\":\"x\"}' type:'Spa:String:JSON'\n\
                    update: id:87 key:'target.object' value:'StudioMic_Ch_music' type:''\n\
                    update: id:90 key:'target.object' value:'alsa_output.usb-x' type:''\n";
        assert_eq!(parse_channel_targets(text), vec![(87, "music".to_string())]);
    }
}
