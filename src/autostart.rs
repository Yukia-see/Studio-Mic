use std::fs;
use std::path::PathBuf;

pub struct AutostartManager;

impl AutostartManager {
    pub fn autostart_dir() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/yukia".into());
        PathBuf::from(home).join(".config").join("autostart")
    }

    pub fn desktop_entry_path() -> PathBuf {
        Self::autostart_dir().join("studiomic.desktop")
    }

    pub fn is_enabled() -> bool {
        Self::desktop_entry_path().exists()
    }

    pub fn set_enabled(enabled: bool) -> std::io::Result<()> {
        let path = Self::desktop_entry_path();
        if enabled {
            let dir = Self::autostart_dir();
            if !dir.exists() {
                fs::create_dir_all(&dir)?;
            }
            let desktop_content = r#"[Desktop Entry]
Type=Application
Name=StudioMic
Comment=Professional Broadcast Microphone Tuning & Virtual Mixing Suite
Exec=/home/yukia/.local/bin/studiomic --background
Icon=audio-input-microphone
Terminal=false
Categories=AudioVideo;Audio;Mixer;Recorder;
X-GNOME-Autostart-enabled=true
StartupNotify=false
"#;
            fs::write(&path, desktop_content)?;
            println!("Installed StudioMic autostart entry: {:?}", path);
        } else if path.exists() {
            fs::remove_file(&path)?;
            println!("Removed StudioMic autostart entry: {:?}", path);
        }
        Ok(())
    }
}
