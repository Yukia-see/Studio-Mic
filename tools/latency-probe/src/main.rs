//! latency-probe: measures mic -> headphone (Monitor Mix) latency through Studio Mic.
//!
//!   latency-probe [--mode processed|raw|both] [--runs 3] [--cli studio-mic]
//!
//! An impulse burst is injected into the current mic chain input and recorded twice by ONE stereo
//! recorder: left = the injected reference, right = the Monitor Mix limiter output. Because both
//! channels share one node and one clock, the onset difference is the true path latency.
//! The median of the runs is written to ~/.cache/studio-mic/latency.json (shown in the UI).
//! Your monitor mode and mute are restored afterwards. A loud room can disturb the measurement.

use std::process::{Command, Stdio};
use std::time::Duration;

const RATE: usize = 48_000;
const REC: &str = "StudioMic_LatProbe_Rec";
const PLAY: &str = "StudioMic_LatProbe_Play";

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| default.to_string())
}

fn cli(bin: &str, a: &[&str]) -> Result<String, String> {
    let o = Command::new(bin).args(a).output().map_err(|e| format!("{}: {}", bin, e))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&o.stderr).to_string())
    }
}

fn write_wav_f32(path: &str, samples: &[f32]) -> std::io::Result<()> {
    let data_len = (samples.len() * 4) as u32;
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&3u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&(RATE as u32).to_le_bytes());
    b.extend_from_slice(&(RATE as u32 * 4).to_le_bytes());
    b.extend_from_slice(&4u16.to_le_bytes());
    b.extend_from_slice(&32u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        b.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, b)
}

/// Interleaved float32 samples of a WAV written by pw-record (header may be unfinalised).
fn read_f32_wav(path: &str) -> Option<Vec<f32>> {
    let b = std::fs::read(path).ok()?;
    let mut pos = 12;
    while pos + 8 <= b.len() {
        let len = u32::from_le_bytes([b[pos + 4], b[pos + 5], b[pos + 6], b[pos + 7]]) as usize;
        if &b[pos..pos + 4] == b"data" {
            let end = if len == 0 || pos + 8 + len > b.len() { b.len() } else { pos + 8 + len };
            return Some(b[pos + 8..end].chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect());
        }
        pos += 8 + len + (len & 1);
    }
    None
}

fn port_exists(port: &str) -> bool {
    for _ in 0..60 {
        let mut all = String::new();
        for f in ["-o", "-i"] {
            if let Ok(o) = Command::new("pw-link").arg(f).output() {
                all.push_str(&String::from_utf8_lossy(&o.stdout));
            }
        }
        if all.lines().any(|l| l.trim() == port) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn link(a: &str, b: &str) {
    let _ = Command::new("pw-link").args([a, b]).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// Latency of one run in ms, or None if the burst never reached the monitor bus.
fn measure_once(wav: &str, mic_in: &str) -> Option<f64> {
    let rec_path = std::env::temp_dir().join("latency-probe-capture.wav");
    let rec_s = rec_path.to_string_lossy().to_string();
    let _ = std::fs::remove_file(&rec_path);
    let mut rec = Command::new("pw-record")
        .args(["--rate", "48000", "--format", "f32", "--channels", "2", "-P",
               &format!("{{ node.name = {} node.autoconnect = false audio.position = [ FL FR ] }}", REC), &rec_s])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if !port_exists(&format!("{}:input_FR", REC)) {
        let _ = rec.kill();
        return None;
    }
    // Tap the monitor bus at its limiter output: the headphones themselves stay muted.
    link("StudioMic_Monitor_Mix_Lim:output_FL", &format!("{}:input_FR", REC));
    let mut play = Command::new("pw-play")
        .args(["-P", &format!("{{ node.name = {} node.autoconnect = false }}", PLAY), wav])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let out = format!("{}:output_MONO", PLAY);
    if port_exists(&out) {
        link(&out, &format!("{}:input_FL", REC));
        link(&out, &format!("{}:input_FL", mic_in));
        link(&out, &format!("{}:input_FR", mic_in));
    }
    let _ = play.wait();
    std::thread::sleep(Duration::from_millis(400));
    let _ = Command::new("kill").args(["-INT", &rec.id().to_string()]).status();
    let _ = rec.wait();

    let v = read_f32_wav(&rec_s)?;
    let left: Vec<f32> = v.chunks_exact(2).map(|c| c[0]).collect();
    let right: Vec<f32> = v.chunks_exact(2).map(|c| c[1]).collect();
    let ref_on = left.iter().position(|s| s.abs() > 0.1)?;
    let peak = right[ref_on..].iter().fold(0f32, |m, s| m.max(s.abs()));
    if peak < 0.01 {
        return None;
    }
    let out_on = ref_on + right[ref_on..].iter().position(|s| s.abs() > 0.35 * peak)?;
    Some((out_on - ref_on) as f64 * 1000.0 / RATE as f64)
}

/// Current graph quantum (clock.force-quantum if set, else clock.quantum).
fn graph_quantum() -> usize {
    let out = Command::new("pw-metadata").args(["-n", "settings"]).output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    let get = |k: &str| -> Option<usize> {
        out.lines().find(|l| l.contains(&format!("key:'{}'", k)))?.split("value:'").nth(1)?.split('\'').next()?.parse().ok()
    };
    get("clock.force-quantum").filter(|q| *q > 0).or_else(|| get("clock.quantum")).unwrap_or(1024)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("usage: latency-probe [--mode processed|raw|both] [--runs 3] [--cli studio-mic]");
        return;
    }
    let mode = arg(&args, "--mode", "both");
    let runs: usize = arg(&args, "--runs", "3").parse().unwrap_or(3).max(1);
    let bin = arg(&args, "--cli", "studio-mic");

    let status: serde_like::Json = match cli(&bin, &["--status", "--json"]) {
        Ok(s) => serde_like::Json(s),
        Err(e) => {
            eprintln!("Studio Mic is not reachable via '{}': {}", bin, e.trim());
            std::process::exit(2);
        }
    };
    let orig_mode = status.str_field("monitor_mode").unwrap_or_else(|| "processed".into());
    let orig_muted = status.mic_monitor_muted();
    let orig_master_muted = status.master_monitor_muted();

    // Impulse: 0.5 s silence, 10 ms 1 kHz burst at -6 dBFS (survives gates / limiters), 1 s tail.
    let mut wav = vec![0f32; RATE / 2];
    wav.extend((0..RATE / 100).map(|i| 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / RATE as f32).sin()));
    wav.extend(vec![0f32; RATE]);
    let wav_path = std::env::temp_dir().join("latency-probe-impulse.wav");
    let wav_s = wav_path.to_string_lossy().to_string();
    if write_wav_f32(&wav_s, &wav).is_err() {
        eprintln!("cannot write impulse file");
        std::process::exit(2);
    }

    let modes: Vec<&str> = match mode.as_str() {
        "processed" => vec!["processed"],
        "raw" => vec!["raw"],
        _ => vec!["processed", "raw"],
    };
    let mut results: Vec<(String, f64)> = Vec::new();
    // Silence the headphones while impulses play (measured before the mute) and PROVE it.
    let _ = cli(&bin, &["--ui", "monitor_master:mute:on"]);
    if let Err(e) = mute_headphones_verified() {
        eprintln!("ABORT: {}", e);
        let _ = cli(&bin, &["--ui", if orig_master_muted { "monitor_master:mute:on" } else { "monitor_master:mute:off" }]);
        std::process::exit(2);
    }
    let _ = cli(&bin, &["--mic-monitor", "on"]);
    for m in &modes {
        if *m == "raw" {
            // Raw monitoring is a direct hardware -> headphone-bus link: no Studio Mic stage is in
            // the path, so it adds one graph period (the hardware's own buffering is not counted).
            let q = graph_quantum();
            let ms = q as f64 * 1000.0 / RATE as f64;
            println!("{:<9} monitor latency: {:6.1} ms  (direct link: one {}-sample graph period, no processing)", m, ms, q);
            results.push((m.to_string(), ms));
            continue;
        }
        let _ = cli(&bin, &["--mic-monitor", m]);
        std::thread::sleep(Duration::from_millis(400));
        // The mic chain input changes name with every swap: ask Studio Mic for the current one.
        let rt = cli(&bin, &["--status", "--json", "--runtime"]).unwrap_or_default();
        let mic_in = serde_like::Json(rt).str_field("mic_input_node").unwrap_or_else(|| "StudioMic_Mic_g1_In".into());
        let mut v: Vec<f64> = (0..runs).filter_map(|_| measure_once(&wav_s, &mic_in)).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        match v.get(v.len() / 2) {
            Some(ms) => {
                println!("{:<9} monitor latency: {:6.1} ms  (median of {} runs: {:?})", m, ms, v.len(), v.iter().map(|x| (x * 10.0).round() / 10.0).collect::<Vec<_>>());
                results.push((m.to_string(), *ms));
            }
            None => println!("{:<9} monitor latency: not measurable (burst suppressed? try `studio-mic --set-denoiser off`)", m),
        }
    }
    // Restore the user's monitoring.
    let _ = cli(&bin, &["--mic-monitor", &orig_mode]);
    let _ = cli(&bin, &["--mic-monitor", if orig_muted { "off" } else { "on" }]);
    let _ = cli(&bin, &["--ui", if orig_master_muted { "monitor_master:mute:on" } else { "monitor_master:mute:off" }]);
    let _ = std::fs::remove_file(&wav_path);

    if !results.is_empty() {
        let home = std::env::var("HOME").unwrap_or_default();
        let dir = std::path::Path::new(&home).join(".cache/studio-mic");
        let path = dir.join("latency.json");
        let mut existing = std::fs::read_to_string(&path).unwrap_or_default();
        for (m, ms) in &results {
            existing = serde_like::set_number(&existing, &format!("{}_ms", m), *ms);
        }
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(&path, existing);
        println!("saved {}", path.display());
    }
}

/// Minimal JSON helpers (keeps this tool dependency-free).
mod serde_like {
    pub struct Json(pub String);
    impl Json {
        pub fn str_field(&self, key: &str) -> Option<String> {
            let k = format!("\"{}\"", key);
            let i = self.0.find(&k)? + k.len();
            let rest = self.0[i..].trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
            Some(rest[..rest.find('"')?].to_string())
        }
        /// mic.mix.monitor_muted from `--status --json` (first "monitor_muted" after "mic").
        pub fn mic_monitor_muted(&self) -> bool {
            let Some(mic) = self.0.find("\"mic\"") else { return true };
            let tail = &self.0[mic..];
            let Some(i) = tail.find("\"monitor_muted\"") else { return true };
            tail[i..].split(':').nth(1).map(|v| v.trim_start().starts_with("true")).unwrap_or(true)
        }
    }

    impl Json {
        /// monitor.muted (the Master Monitor) from `--status --json`.
        pub fn master_monitor_muted(&self) -> bool {
            let Some(m) = self.0.find("\"monitor\": {") else { return false };
            let tail = &self.0[m..];
            let Some(i) = tail.find("\"muted\"") else { return false };
            tail[i..].split(':').nth(1).map(|v| v.trim_start().starts_with("true")).unwrap_or(false)
        }
    }

    pub fn set_number(json: &str, key: &str, value: f64) -> String {
        let mut pairs: Vec<(String, String)> = Vec::new();
        let body = json.trim().trim_start_matches('{').trim_end_matches('}');
        for part in body.split(',') {
            if let Some((k, v)) = part.split_once(':') {
                let k = k.trim().trim_matches('"').to_string();
                if !k.is_empty() && k != key {
                    pairs.push((k, v.trim().to_string()));
                }
            }
        }
        pairs.push((key.to_string(), format!("{:.1}", value)));
        let inner: Vec<String> = pairs.iter().map(|(k, v)| format!("  \"{}\": {}", k, v)).collect();
        format!("{{\n{}\n}}\n", inner.join(",\n"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn merges_numbers() {
            let a = set_number("", "raw_ms", 6.0);
            let b = set_number(&a, "processed_ms", 42.25);
            assert!(b.contains("\"raw_ms\": 6.0") && b.contains("\"processed_ms\": 42.2"));
        }
        #[test]
        fn reads_fields() {
            let j = Json(r#"{"mic": {"monitor_mode": "raw", "mix": {"monitor_muted": false}}}"#.into());
            assert_eq!(j.str_field("monitor_mode").as_deref(), Some("raw"));
            assert!(!j.mic_monitor_muted());
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Headphone safety guard: tools that play test tones must PROVE the Monitor bus is muted first.
// ---------------------------------------------------------------------------------------------

/// Node id of StudioMic_Monitor_Mix (the bus your headphones listen to), if it exists.
fn monitor_bus_id() -> Option<u32> {
    let out = std::process::Command::new("pw-cli").args(["ls", "Node"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut current: Option<u32> = None;
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("id ") {
            current = rest.split(',').next().and_then(|n| n.trim().parse().ok());
        } else if t == "node.name = \"StudioMic_Monitor_Mix\"" {
            return current;
        }
    }
    None
}

fn monitor_muted(id: u32) -> Option<bool> {
    let out = std::process::Command::new("wpctl").args(["get-volume", &id.to_string()]).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).contains("[MUTED]"))
}

/// Mutes the headphone bus and verifies it. Returns the previous mute state to restore, or an
/// error if the mute cannot be confirmed (the caller must then NOT play anything).
pub fn mute_headphones_verified() -> Result<Option<(u32, bool)>, String> {
    let Some(id) = monitor_bus_id() else { return Ok(None) }; // Studio Mic not running: nothing routes to headphones
    let was = monitor_muted(id).ok_or("cannot read the Monitor bus mute state")?;
    let _ = std::process::Command::new("wpctl").args(["set-mute", &id.to_string(), "1"]).status();
    for _ in 0..20 {
        if monitor_muted(id) == Some(true) {
            return Ok(Some((id, was)));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Err("could not confirm that StudioMic_Monitor_Mix is muted; refusing to play test audio".into())
}

pub fn restore_headphones(saved: Option<(u32, bool)>) {
    if let Some((id, was)) = saved {
        let _ = std::process::Command::new("wpctl").args(["set-mute", &id.to_string(), if was { "1" } else { "0" }]).status();
    }
}
