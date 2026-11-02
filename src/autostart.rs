use std::fs;
use std::path::PathBuf;

pub struct AutostartManager;

impl AutostartManager {
    pub fn autostart_dir() -> PathBuf {
        // $HOME, or the passwd entry when it is unset (std::env::home_dir handles both).
        let home = std::env::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        home.join(".config").join("autostart")
    }

    pub fn desktop_entry_path() -> PathBuf {
        Self::autostart_dir().join("studiomic.desktop")
    }

    pub fn is_enabled() -> bool {
        Self::desktop_entry_path().exists()
    }

    pub fn set_enabled_with_minimized(enabled: bool, minimized: bool) -> std::io::Result<()> {
        let path = Self::desktop_entry_path();
        if enabled {
            fs::create_dir_all(Self::autostart_dir())?;
            let exe = current_exe_path();
            let exec_cmd = if minimized { format!("{} --background", desktop_exec_quote(&exe)) } else { desktop_exec_quote(&exe) };
            let desktop_content = format!(
r#"[Desktop Entry]
Type=Application
Name=StudioMic
Comment=Professional Broadcast Microphone Tuning & Virtual Mixing Suite
Exec={}
Icon=audio-input-microphone
Terminal=false
Categories=AudioVideo;Audio;Mixer;Recorder;
StartupWMClass=studiomic
X-GNOME-Autostart-enabled=true
StartupNotify=false
"#,
                exec_cmd
            );
            fs::write(&path, desktop_content)?;
            println!("Installed StudioMic autostart entry: {:?}", path);
        } else if path.exists() {
            fs::remove_file(&path)?;
            println!("Removed StudioMic autostart entry: {:?}", path);
        }
        Ok(())
    }
}

/// The running binary's path. After a package upgrade replaces the file, Linux reports it as
/// "/usr/bin/studiomic (deleted)"; the new binary lives at the original path.
fn current_exe_path() -> String {
    let p = std::env::current_exe().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| "studiomic".into());
    p.strip_suffix(" (deleted)").map(|s| s.to_string()).unwrap_or(p)
}

/// One argument for a desktop-entry `Exec=` key: quoted when needed, with the spec's escapes
/// (`"`, `` ` ``, `$`, `\` inside quotes; `\` doubled again at the value level; `%` as `%%`).
fn desktop_exec_quote(arg: &str) -> String {
    let arg = arg.replace('%', "%%");
    if !arg.chars().any(|c| c.is_whitespace() || "\"'\\><~|&;$*?#()`".contains(c)) {
        return arg;
    }
    let mut out = String::from("\"");
    for c in arg.chars() {
        match c {
            '"' | '`' | '$' => {
                out.push_str("\\\\");
                out.push(c);
            }
            '\\' => out.push_str("\\\\\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::desktop_exec_quote;

    #[test]
    fn exec_paths_are_quoted_per_spec() {
        assert_eq!(desktop_exec_quote("/usr/bin/studiomic"), "/usr/bin/studiomic");
        assert_eq!(desktop_exec_quote("/home/a b/studiomic"), "\"/home/a b/studiomic\"");
        assert_eq!(desktop_exec_quote("/x/100%"), "/x/100%%");
        assert_eq!(desktop_exec_quote("/x/$y"), "\"/x/\\\\$y\"");
    }
}
