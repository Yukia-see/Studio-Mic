//! gap-probe: proves the live-change rule ("no slider ever interrupts audio").
//!
//!   gap-probe generate <out.wav> [--secs 30] [--freq 220] [--level -12]
//!   gap-probe analyze <file.wav> [--silence-ms 5] [--step 0.1]
//!   gap-probe live [--inject StudioMic_Ch_music] [--capture StudioMic_Stream_Mix] [--secs 20]
//!
//! `live` plays a sentinel sine into a Studio Mic channel, records a bus, and reports any dropout
//! (> 5 ms of digital silence inside the tone) or click (sample step > 0.1 on a tone whose largest
//! natural step is ~0.007). Move sliders, toggle stages and switch denoisers while it runs.
//! To test the mic chain, inject into its current input:
//!   gap-probe live --inject "$(studio-mic --status --json --runtime | jq -r .runtime.mic_input_node)"
//! Safety: `live` mutes StudioMic_Monitor_Mix and verifies the mute before playing anything
//! (restored afterwards); it aborts if the mute cannot be confirmed.
//! Exit code: 0 clean, 1 problems found, 2 usage / IO error.

use std::process::{Command, Stdio};
use std::time::Duration;

const RATE: u32 = 48_000;

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| default.to_string())
}

fn write_wav_f32(path: &str, rate: u32, channels: u16, samples: &[f32]) -> std::io::Result<()> {
    let data_len = (samples.len() * 4) as u32;
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * channels as u32 * 4).to_le_bytes());
    b.extend_from_slice(&(channels * 4).to_le_bytes());
    b.extend_from_slice(&32u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        b.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, b)
}

/// Reads PCM16/24/32 or float32 WAV. Returns (rate, channels, interleaved samples in [-1, 1]).
fn read_wav(path: &str) -> Result<(u32, usize, Vec<f32>), String> {
    let b = std::fs::read(path).map_err(|e| format!("{}: {}", path, e))?;
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let (mut fmt, mut chans, mut rate, mut bits) = (0u16, 0usize, 0u32, 0u16);
    let mut pos = 12;
    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let mut len = u32::from_le_bytes([b[pos + 4], b[pos + 5], b[pos + 6], b[pos + 7]]) as usize;
        let body = pos + 8;
        if id == b"fmt " {
            fmt = u16::from_le_bytes([b[body], b[body + 1]]);
            chans = u16::from_le_bytes([b[body + 2], b[body + 3]]) as usize;
            rate = u32::from_le_bytes([b[body + 4], b[body + 5], b[body + 6], b[body + 7]]);
            bits = u16::from_le_bytes([b[body + 14], b[body + 15]]);
            if fmt == 0xFFFE && len >= 26 {
                fmt = u16::from_le_bytes([b[body + 24], b[body + 25]]); // WAVE_FORMAT_EXTENSIBLE subformat
            }
        } else if id == b"data" {
            if len == 0 || len == u32::MAX as usize || body + len > b.len() {
                len = b.len() - body; // header not finalised (recording was interrupted)
            }
            let d = &b[body..body + len];
            let v: Vec<f32> = match (fmt, bits) {
                (3, 32) => d.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect(),
                (1, 16) => d.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect(),
                (1, 24) => d.chunks_exact(3).map(|c| (((c[2] as i32) << 24 | (c[1] as i32) << 16 | (c[0] as i32) << 8) >> 8) as f32 / 8_388_608.0).collect(),
                (1, 32) => d.chunks_exact(4).map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32 / 2_147_483_648.0).collect(),
                _ => return Err(format!("unsupported WAV format {} / {} bit", fmt, bits)),
            };
            return Ok((rate, chans.max(1), v));
        }
        pos = body + len + (len & 1);
    }
    Err("no data chunk".into())
}

#[derive(Default)]
struct Report {
    dropouts: Vec<(f64, f64)>,
    clicks: Vec<(f64, f32)>,
    analysed_secs: f64,
}

/// Mono analysis between the first and last non-silent sample.
fn analyse(rate: u32, mono: &[f32], silence_ms: f64, step: f32) -> Report {
    let thr = 1e-4f32;
    // Analyse from the sentinel's onset to its end (> -40 dBFS), so background noise before or
    // after the tone (e.g. a live mic flickering through a gate) is not mistaken for dropouts.
    // Onset/end = where the signal is SUSTAINED (every 10 ms window of the next 50 ms peaks above
    // -40 dBFS), so a single stray click before the tone does not open the analysis window early.
    let onset = 0.01f32;
    let win = (rate as usize / 100).max(1);
    let loud = |w: usize| mono[w * win..((w + 1) * win).min(mono.len())].iter().any(|s| s.abs() > onset);
    let windows = mono.len() / win;
    let sustained = |w: usize| w + 5 <= windows && (w..w + 5).all(loud);
    let first = (0..windows).find(|&w| sustained(w)).and_then(|w| (w * win..mono.len()).find(|&i| mono[i].abs() > onset));
    let last = (0..windows).rev().find(|&w| w >= 4 && (w - 4..=w).all(loud)).and_then(|w| (0..((w + 1) * win).min(mono.len())).rev().find(|&i| mono[i].abs() > onset));
    // Ignore the final 50 ms: the player stopping is the harness ending, not the system under test.
    let last = last.map(|l| l.saturating_sub(win * 5));
    let (Some(first), Some(last)) = (first, last) else { return Report::default() };
    let min_run = (silence_ms / 1000.0 * rate as f64) as usize;
    let mut r = Report { analysed_secs: (last - first) as f64 / rate as f64, ..Default::default() };
    let mut run_start: Option<usize> = None;
    for i in first..=last {
        let s = mono[i];
        if s.abs() <= thr {
            run_start.get_or_insert(i);
        } else if let Some(st) = run_start.take() {
            // A sine crosses zero briefly; only long runs are dropouts.
            if i - st >= min_run {
                r.dropouts.push((st as f64 / rate as f64, (i - st) as f64 * 1000.0 / rate as f64));
            }
        }
        if i > first {
            let d = (s - mono[i - 1]).abs();
            if d > step && r.clicks.last().map(|(t, _)| i as f64 / rate as f64 - t > 0.01).unwrap_or(true) {
                r.clicks.push((i as f64 / rate as f64, d));
            }
        }
    }
    r
}

fn print_report(r: &Report) -> i32 {
    println!("analysed {:.1} s of signal", r.analysed_secs);
    if r.analysed_secs == 0.0 {
        println!("RESULT: no signal captured (is the capture node correct and unmuted?)");
        return 1;
    }
    for (t, ms) in &r.dropouts {
        println!("  DROPOUT at {:8.3} s: {:.1} ms of silence", t, ms);
    }
    for (t, d) in &r.clicks {
        println!("  CLICK   at {:8.3} s: step {:.3}", t, d);
    }
    if r.dropouts.is_empty() && r.clicks.is_empty() {
        println!("RESULT: PASS (zero dropouts, zero clicks)");
        0
    } else {
        println!("RESULT: FAIL ({} dropouts, {} clicks)", r.dropouts.len(), r.clicks.len());
        1
    }
}

fn sentinel(secs: f64, freq: f64, level_db: f64) -> Vec<f32> {
    let amp = 10f64.powf(level_db / 20.0);
    let n = (secs * RATE as f64) as usize;
    let fade = (0.02 * RATE as f64) as usize;
    (0..n)
        .map(|i| {
            let env = (i.min(n - 1 - i) as f64 / fade as f64).min(1.0);
            (amp * env * (2.0 * std::f64::consts::PI * freq * i as f64 / RATE as f64).sin()) as f32
        })
        .collect()
}

fn wait_port(port: &str) -> bool {
    for _ in 0..60 {
        let out = Command::new("pw-link").args(["-o"]).output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        let inp = Command::new("pw-link").args(["-i"]).output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        if out.lines().chain(inp.lines()).any(|l| l.trim() == port) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn live(args: &[String]) -> i32 {
    let inject = arg(args, "--inject", "StudioMic_Ch_music");
    let mut capture = arg(args, "--capture", "StudioMic_Stream_Mix");
    // Safety: headphones are muted (and verified) for the whole test unless explicitly allowed.
    let allow_monitor = args.iter().any(|a| a == "--unsafe-allow-headphones");
    let saved = if allow_monitor {
        None
    } else {
        match mute_headphones_verified() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ABORT: {}", e);
                return 2;
            }
        }
    };
    if !allow_monitor && capture == "StudioMic_Monitor_Mix" {
        // The bus itself is muted now; record its limiter output (same mix, before the mute).
        capture = "StudioMic_Monitor_Mix_Lim".into();
        println!("(headphones muted: capturing StudioMic_Monitor_Mix_Lim instead)");
    }
    let code = live_inner(args, &inject, &capture);
    restore_headphones(saved);
    code
}

fn live_inner(args: &[String], inject: &str, capture: &str) -> i32 {
    let secs: f64 = arg(args, "--secs", "20").parse().unwrap_or(20.0);
    let dir = std::env::temp_dir();
    let tone = dir.join("gap-probe-sentinel.wav");
    let rec = dir.join("gap-probe-capture.wav");
    let tone_s = tone.to_string_lossy().to_string();
    let rec_s = rec.to_string_lossy().to_string();
    // 0.5 s of leading silence: links are made while nothing plays, so linking itself never clicks.
    let mut wav = vec![0f32; RATE as usize / 2];
    wav.extend(sentinel(secs + 1.0, 220.0, -12.0));
    if let Err(e) = write_wav_f32(&tone_s, RATE, 1, &wav) {
        eprintln!("cannot write sentinel: {}", e);
        return 2;
    }
    let _ = std::fs::remove_file(&rec);
    println!("Recording {} for {:.0} s while a 220 Hz sentinel plays into {} ...", capture, secs, inject);
    println!("Adjust sliders / toggles / denoiser now. Nothing may drop or click.");
    let Ok(mut recorder) = Command::new("pw-record")
        .args(["--target", capture, "--rate", "48000", "--format", "f32", "--channels", "1", "-P", "{ node.name = StudioMic_GapProbe_Rec }", &rec_s])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        eprintln!("pw-record not available");
        return 2;
    };
    std::thread::sleep(Duration::from_millis(400));
    let Ok(mut player) = Command::new("pw-play")
        .args(["-P", "{ node.name = StudioMic_GapProbe_Play node.autoconnect = false }", &tone_s])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        let _ = recorder.kill();
        eprintln!("pw-play not available");
        return 2;
    };
    if wait_port("StudioMic_GapProbe_Play:output_MONO") {
        // Channel sinks take playback_FL/FR; the mic pre-stage takes input_FL/FR.
        for side in ["FL", "FR"] {
            for dst in [format!("{}:playback_{}", inject, side), format!("{}:input_{}", inject, side)] {
                let _ = Command::new("pw-link").args(["StudioMic_GapProbe_Play:output_MONO", &dst]).stdout(Stdio::null()).stderr(Stdio::null()).status();
            }
        }
    } else {
        eprintln!("sentinel player did not appear");
    }
    let _ = player.wait();
    std::thread::sleep(Duration::from_millis(300));
    let _ = Command::new("kill").args(["-INT", &recorder.id().to_string()]).status();
    let _ = recorder.wait();
    match read_wav(&rec_s) {
        Ok((rate, ch, v)) => {
            let mono: Vec<f32> = v.chunks(ch).map(|c| c[0]).collect();
            print_report(&analyse(rate, &mono, 5.0, 0.1))
        }
        Err(e) => {
            eprintln!("capture unreadable: {}", e);
            2
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(|s| s.as_str()) {
        Some("generate") if args.len() >= 2 => {
            let secs: f64 = arg(&args, "--secs", "30").parse().unwrap_or(30.0);
            let freq: f64 = arg(&args, "--freq", "220").parse().unwrap_or(220.0);
            let lvl: f64 = arg(&args, "--level", "-12").parse().unwrap_or(-12.0);
            match write_wav_f32(&args[1], RATE, 1, &sentinel(secs, freq, lvl)) {
                Ok(_) => { println!("wrote {}", args[1]); 0 }
                Err(e) => { eprintln!("{}", e); 2 }
            }
        }
        Some("analyze") | Some("analyse") if args.len() >= 2 => match read_wav(&args[1]) {
            Ok((rate, ch, v)) => {
                let silence: f64 = arg(&args, "--silence-ms", "5").parse().unwrap_or(5.0);
                let step: f32 = arg(&args, "--step", "0.1").parse().unwrap_or(0.1);
                let mono: Vec<f32> = v.chunks(ch).map(|c| c[0]).collect();
                print_report(&analyse(rate, &mono, silence, step))
            }
            Err(e) => { eprintln!("{}", e); 2 }
        },
        Some("live") => live(&args),
        _ => {
            eprintln!("usage:\n  gap-probe generate <out.wav> [--secs 30] [--freq 220] [--level -12]\n  gap-probe analyze <file.wav> [--silence-ms 5] [--step 0.1]\n  gap-probe live [--inject StudioMic_Ch_music] [--capture StudioMic_Stream_Mix] [--secs 20] [--unsafe-allow-headphones]\n(live mutes and verifies the headphone bus first; it aborts if it cannot)");
            2
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_tone_passes() {
        let r = analyse(RATE, &sentinel(2.0, 220.0, -12.0), 5.0, 0.1);
        assert!(r.dropouts.is_empty() && r.clicks.is_empty());
    }

    #[test]
    fn detects_dropout_and_click() {
        let mut t = sentinel(2.0, 220.0, -12.0);
        for s in &mut t[48_000..48_000 + 480] {
            *s = 0.0; // 10 ms gap
        }
        t[70_000] += 0.5; // click
        let r = analyse(RATE, &t, 5.0, 0.1);
        assert_eq!(r.dropouts.len(), 1);
        assert!(!r.clicks.is_empty());
    }

    #[test]
    fn wav_roundtrip() {
        let p = std::env::temp_dir().join("gap-probe-test.wav");
        let tone = sentinel(0.1, 440.0, -6.0);
        write_wav_f32(p.to_str().unwrap(), RATE, 1, &tone).unwrap();
        let (rate, ch, v) = read_wav(p.to_str().unwrap()).unwrap();
        assert_eq!((rate, ch, v.len()), (RATE, 1, tone.len()));
        let _ = std::fs::remove_file(p);
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
