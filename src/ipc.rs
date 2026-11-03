//! Sandbox-safe control socket at `~/.cache/studio-mic/studio-mic.sock` (reachable from Flatpak
//! StreamController, which can see ~/.cache). Protocol: one JSON array of CLI arguments per line
//! in, one JSON object `{"ok": bool, "message": "..."}` per line out.
//!
//! Security: the directory is created 0700 and the socket chmod 0600, so only the owning user can
//! connect (std does not expose SO_PEERCRED on stable Rust; file permissions enforce the same UID).

use crate::app::Controller;
use crate::config::cache_dir;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Unix socket paths are limited to 108 bytes (SUN_LEN); stay well below it.
const MAX_SOCKET_PATH: usize = 100;

fn runtime_dir() -> PathBuf {
    PathBuf::from(std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into())).join("studio-mic")
}

/// Preferred socket (~/.cache/studio-mic/studio-mic.sock, visible to Flatpak apps) and the
/// short fallback in $XDG_RUNTIME_DIR used when the cache path is too long for a Unix socket.
pub fn socket_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    let primary = cache_dir().join("studio-mic.sock");
    if primary.as_os_str().len() <= MAX_SOCKET_PATH {
        v.push(primary);
    }
    v.push(runtime_dir().join("studio-mic.sock"));
    v
}

pub fn socket_path() -> PathBuf {
    socket_candidates().into_iter().next().unwrap_or_else(|| runtime_dir().join("studio-mic.sock"))
}

/// Exclusive per-session instance lock. Held for the life of the process (the OS drops it on
/// exit or crash), so a second engine can never start even if the control socket is broken.
pub fn acquire_instance_lock() -> Option<std::fs::File> {
    let dir = runtime_dir();
    let _ = std::fs::create_dir_all(&dir);
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("instance.lock")).ok()?;
    match f.try_lock() {
        Ok(()) => Some(f),
        Err(_) => None,
    }
}

/// True if some process holds the instance lock (without taking it).
pub fn instance_locked() -> bool {
    match acquire_instance_lock() {
        Some(f) => {
            let _ = f.unlock();
            false
        }
        None => true,
    }
}

/// Every flag that runs as a one-shot client instead of starting the app.
pub const CLI_COMMANDS: &[&str] = &[
    "--toggle-mic-mute",
    "--set-mic-mute",
    "--toggle-monitor-mute",
    "--monitor-gain",
    "--channel-mute",
    "--channel-solo",
    "--channel-gain",
    "--cell-delay",
    "--toggle-clipguard",
    "--mic-monitor",
    "--toggle-autoduck",
    "--set-denoiser",
    "--download-deepfilter",
    "--set-preset",
    "--set-theme",
    "--load-profile",
    "--save-profile",
    "--list-profiles",
    "--toggle-view",
    "--toggle-widget",
    "--set-size",
    "--status",
    "--create-channel",
    "--remove-channel",
    "--rename-channel",
    "--assign-app",
    "--unassign-app",
    "--add-foreground-app",
    "--preview",
    "--record-test",
    "--ui",
    "--quit",
    "--show",
];

pub fn start_server(controller: Arc<Controller>) -> std::io::Result<()> {
    let path = socket_path();
    let dir = path.parent().map(|p| p.to_path_buf()).unwrap_or_else(cache_dir);
    std::fs::create_dir_all(&dir)?;
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    // Remove a stale socket left by a crash (a live instance was already ruled out by main).
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    println!("Control socket listening on {}", path.display());
    std::thread::Builder::new().name("ipc-server".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let c = controller.clone();
            std::thread::spawn(move || handle_client(c, stream));
        }
    })?;
    Ok(())
}

fn handle_client(controller: Arc<Controller>, stream: UnixStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let Ok(read_half) = stream.try_clone() else { return };
    let mut reader = BufReader::new(read_half);
    let mut writer = stream;
    let mut line = String::new();
    while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
        let args: Vec<String> = serde_json::from_str(line.trim()).unwrap_or_else(|_| {
            line.split_whitespace().map(|s| s.to_string()).collect()
        });
        let (ok, message) = controller.handle_cli(&args);
        let reply = serde_json::json!({ "ok": ok, "message": message });
        if writeln!(writer, "{}", reply).is_err() {
            break;
        }
        line.clear();
    }
}

/// Sends one command. Returns None when no instance is listening.
pub fn send(args: &[String]) -> Option<(bool, String)> {
    let mut stream = socket_candidates().into_iter().find_map(|p| UnixStream::connect(p).ok())?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    writeln!(stream, "{}", serde_json::to_string(args).ok()?).ok()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    Some((v["ok"].as_bool().unwrap_or(false), v["message"].as_str().unwrap_or("").to_string()))
}

pub fn instance_running() -> bool {
    socket_candidates().into_iter().any(|p| UnixStream::connect(p).is_ok())
}

/// CLI entry: talk to the running instance, auto-starting a background one if needed.
pub fn run_client(args: &[String]) -> i32 {
    if let Some((ok, msg)) = send(args) {
        if !msg.is_empty() {
            if ok { println!("{}", msg) } else { eprintln!("studiomic: {}", msg) }
        }
        return if ok { 0 } else { 1 };
    }
    if args.first().map(|a| a == "--quit").unwrap_or(false) {
        eprintln!("studiomic: not running");
        return 0;
    }
    if instance_locked() {
        // An instance is running but its socket is unreachable: never start a second engine.
        eprintln!("studiomic: Studio Mic is running but its control socket is unreachable ({})", socket_path().display());
        return 2;
    }
    // Headless wake: start the engine in the background (no window) and retry.
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("studiomic"));
    let spawned = std::process::Command::new(exe)
        .arg("--background")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn();
    if spawned.is_err() {
        eprintln!("studiomic: could not start the Studio Mic engine");
        return 2;
    }
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        std::thread::sleep(Duration::from_millis(250));
        if let Some((ok, msg)) = send(args) {
            if !msg.is_empty() {
                if ok { println!("{}", msg) } else { eprintln!("studiomic: {}", msg) }
            }
            return if ok { 0 } else { 1 };
        }
    }
    eprintln!("studiomic: engine did not come up (see ~/.cache/studio-mic/engine.log)");
    2
}

pub fn print_help() {
    println!(
        "Studio Mic - broadcast mic processing & virtual mixer for PipeWire

USAGE
  studiomic                       open the window (or focus the running one)
  studiomic --background          start in the tray (also: --start-minimized, --tray, -b, --daemon)
  studiomic --ui-backend slint|web   native window or the webview (also STUDIOMIC_UI);
                                  default: the 'native window' setting in Settings (off = webview)
  studiomic --eq | --noise | --calib    open on a specific tab

CONTROL (talks to the running instance over ~/.cache/studio-mic/studio-mic.sock,
starting a background instance first if none is running)
  --toggle-mic-mute               --set-mic-mute <on|off>
  --toggle-monitor-mute           --monitor-gain <+dB|-dB|=dB>
  --channel-mute <Channel> <stream|monitor|vod|all>
  --channel-solo <Channel>
  --channel-gain <Channel> <stream|monitor> <+dB|-dB|=dB>   (+/- relative, = or unsigned absolute)
  --cell-delay <Channel> <stream|monitor> <ms>
  --toggle-clipguard              --toggle-autoduck <Channel>
  --mic-monitor <processed|raw|on|off>   hear yourself through the chain or the zero-latency tap
  --set-denoiser <deepfilter|rnnoise|speex|off>
  --download-deepfilter           fetch the pinned DeepFilterNet plugin (SHA256-checked, ~54 MB)
  --set-preset <EQPresetName>     --set-theme <oled_black|material_dark|midnight_slate|light>
  --load-profile <Name>           --save-profile <Name>      --list-profiles
  --toggle-widget                 mini floating widget on/off (--toggle-view is an alias)
  --set-size <small|medium|large> control size             --status [--json] [--runtime]
  --create-channel <Name> [preset]  --remove-channel <Name>  --rename-channel <Old> <New>
  --assign-app <Channel> <app>    --unassign-app <Channel> <app>   --add-foreground-app <Channel>
  --preview <play|loop|stop>      --record-test
  --show                          --quit
  --ui '<message>'                send a raw UI message (scripting/tests), e.g. --ui 'noise_clean:{{...}}'
  --cleanup-pipewire              stop Studio Mic and remove its PipeWire nodes (uninstall)

Channel names are matched by name or id, case-insensitively (\"Capture Card\", \"capture\", \"Microphone\")."
    );
}
