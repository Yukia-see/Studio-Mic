//! PipeWire graph configuration generators.
//!
//! Topology (every box is its own `pipewire -c <conf>` process so a structural change to one
//! stage never touches the others):
//!
//! ```text
//! HW mic ─► StudioMic_Mic_Pre (mono-center + trim) ─► [Denoiser slot] ─► StudioMic_Mic_Post_g<N>
//!   Post: Gate ► 80 Hz HPF ► Hiss LPF ► De-Esser ► EQ x10 ► Compressor ► Clipguard ► split
//!         └► Voice / Stream / VOD / Monitor gain pairs (AUX0..7) + meter tap (AUX8)
//! App sinks StudioMic_Ch_<id> (raw passthrough) ─► Stream / VOD / Monitor gains (AUX0..5) + tap (AUX6)
//! Bus master: StudioMic_<Bus>_Sum ─► safety limiter ─► lingering StudioMic_<Bus> node
//! ```
//!
//! All live parameters are changed afterwards with `pw-cli set-param` (see `daemon.rs`).

use crate::config::{ChannelMix, DenoiserConfig, MicState, NoiseCleanConfig, VirtualChannelConfig};
use crate::models::microphone::{EqBand, FilterType};
use std::fs;
use std::path::{Path, PathBuf};

pub const EQ_SLOTS: usize = 10;
/// Gate closes this far below where it opens (keyboard-chatter guard).
pub const GATE_HYSTERESIS_DB: f32 = 7.0;
pub const LEGACY_MIC_ALIAS: &str = "studiomic_processed_mic";
pub const MIC2_IN: &str = "StudioMic_Mic2_In";
pub const MIC2_OUT: &str = "StudioMic_Mic2";

/// (key, media.class, description, ceiling dB)
pub const BUSES: [(&str, &str, &str, f32); 4] = [
    ("Voice_Out", "Audio/Source/Virtual", "Studio Mic (Microphone Only)", -1.0),
    ("Stream_Mix", "Audio/Source/Virtual", "Studio Mic (Stream Mix)", -1.0),
    ("VOD_Mix", "Audio/Source/Virtual", "Studio Mic (VOD Track - DMCA Safe)", -1.0),
    ("Monitor_Mix", "Audio/Sink", "Studio Mic (Monitor Mix)", -3.0),
];

pub fn bus_node(key: &str) -> String { format!("StudioMic_{}", key) }
pub fn bus_sum(key: &str) -> String { format!("StudioMic_{}_Sum", key) }
pub fn bus_lim(key: &str) -> String { format!("StudioMic_{}_Lim", key) }
/// Input port names of a lingering bus node.
pub fn bus_inputs(key: &str) -> (&'static str, &'static str) {
    if key == "Monitor_Mix" { ("playback_FL", "playback_FR") } else { ("input_FL", "input_FR") }
}
pub fn channel_sink(id: &str) -> String { format!("StudioMic_Ch_{}", id) }
pub fn channel_out(id: &str) -> String { format!("StudioMic_Ch_{}_Out", id) }
/// Input / output of mic chain generation N (a new generation is built for every swap).
pub fn post_in(generation: u32) -> String { format!("StudioMic_Mic_g{}_In", generation) }
pub fn post_out(generation: u32) -> String { format!("StudioMic_Mic_g{}", generation) }

// ---------------------------------------------------------------------------------------------
// Plugin discovery
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct Plugins {
    pub lsp: Option<String>,
    pub deepfilter: Option<String>,
    pub rnnoise: Option<String>,
    pub speex: Option<String>,
}

pub fn ladspa_search_path() -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut dirs: Vec<String> = std::env::var("LADSPA_PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    for d in [
        format!("{}/.local/lib/ladspa", home),
        format!("{}/.ladspa", home),
        "/usr/lib/ladspa".to_string(),
        "/usr/lib64/ladspa".to_string(),
        "/usr/local/lib/ladspa".to_string(),
    ] {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    dirs
}

fn find_ladspa(names: &[&str]) -> Option<String> {
    for dir in ladspa_search_path() {
        for n in names {
            let p = format!("{}/{}", dir, n);
            if Path::new(&p).exists() {
                return Some(p);
            }
        }
    }
    None
}

impl Plugins {
    pub fn discover() -> Self {
        // Bundled Speex / RNNoise go to ~/.local/lib/ladspa first if the system lacks them.
        crate::audio::plugins_bundle::ensure_embedded_plugins(&ladspa_search_path());
        Self {
            lsp: find_ladspa(&["lsp-plugins-ladspa.so", "lsp-plugins/lsp-plugins-ladspa.so"]),
            deepfilter: find_ladspa(&["libdeep_filter_ladspa.so"]),
            rnnoise: find_ladspa(&["librnnoise_ladspa.so"]),
            // No distro packages a Speex LADSPA wrapper; Studio Mic bundles its own (plugins/speex-ladspa).
            speex: find_ladspa(&["libspeex_ladspa.so", "speex_preprocess.so"]),
        }
    }

    pub fn denoiser_available(&self, engine: &str) -> bool {
        match engine {
            "deepfilter" => self.deepfilter.is_some(),
            "rnnoise" => self.rnnoise.is_some(),
            "speex" => self.speex.is_some(),
            "off" => true,
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

fn header() -> &'static str {
    r#"# Generated by Studio Mic. Do not edit: rewritten on every launch.
context.properties = {
    log.level = 0
    default.clock.rate = 48000
}
context.spa-libs = {
    audio.convert.* = audioconvert/libspa-audioconvert
    support.*       = support/libspa-support
}
context.modules = [
    { name = libpipewire-module-rt args = { nice.level = -11 } flags = [ ifexists nofail ] }
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-adapter }
"#
}

fn lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

fn aux_positions(n: usize) -> String {
    (0..n).map(|i| format!("AUX{}", i)).collect::<Vec<_>>().join(" ")
}

pub struct Node {
    pub name: String,
    pub def: String,
    pub inp: &'static str,
    pub out: &'static str,
}

fn builtin(name: &str, label: &str, controls: &str) -> Node {
    Node {
        name: name.into(),
        def: format!("{{ type = builtin name = {} label = {} control = {{ {} }} }}", name, label, controls),
        inp: "In",
        out: "Out",
    }
}

fn mixer(name: &str, controls: &str) -> String {
    format!("{{ type = builtin name = {} label = mixer control = {{ {} }} }}", name, controls)
}

fn ladspa(name: &str, plugin: &str, label: &str, controls: &str, inp: &'static str, out: &'static str) -> Node {
    Node {
        name: name.into(),
        def: format!(
            "{{ type = ladspa name = {} plugin = \"{}\" label = \"{}\" control = {{ {} }} }}",
            name, plugin, label, controls
        ),
        inp,
        out,
    }
}

fn filter_chain_module(desc: &str, nodes: &[String], links: &[String], inputs: &[String], outputs: &[String], capture: &str, playback: &str) -> String {
    format!(
        r#"    {{ name = libpipewire-module-filter-chain
        args = {{
            node.description = "{desc}"
            audio.rate = 48000
            filter.graph = {{
                nodes = [
                    {nodes}
                ]
                links = [
                    {links}
                ]
                inputs  = [ {inputs} ]
                outputs = [ {outputs} ]
            }}
            capture.props = {{
                {capture}
            }}
            playback.props = {{
                {playback}
            }}
        }}
    }}
"#,
        desc = desc,
        nodes = nodes.join("\n                    "),
        links = links.join("\n                    "),
        inputs = inputs.iter().map(|s| format!("\"{}\"", s)).collect::<Vec<_>>().join(" "),
        outputs = outputs.iter().map(|s| format!("\"{}\"", s)).collect::<Vec<_>>().join(" "),
        capture = capture,
        playback = playback,
    )
}

/// Text that is safe inside a quoted SPA-JSON string: a stray backslash or quote in a channel
/// name would otherwise break the whole config (the strip would not start).
fn spa_string_safe(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '"' => '\'',
            '\\' => '/',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect()
}

fn link(a: &str, b: &str) -> String {
    format!("{{ output = \"{}\" input = \"{}\" }}", a, b)
}

/// Stream-node property block shared by all internal nodes: never auto-linked, never suspended.
fn stream_props(name: &str, desc: &str, positions: &str, extra: &str) -> String {
    format!(
        "node.name = \"{}\"\n                node.description = \"{}\"\n                audio.position = [ {} ]\n                node.autoconnect = false\n                node.pause-on-idle = false\n                node.group = studiomic\n                {}",
        name, desc, positions, extra
    )
}

// ---------------------------------------------------------------------------------------------
// Gain math shared by the generators and the live engine
// ---------------------------------------------------------------------------------------------

/// Linear gains (stream, vod, monitor) for a channel given ducking and solo state.
pub fn route_gains(mix: &ChannelMix, duck_db: f32, solo_active: bool) -> (f32, f32, f32) {
    let duck = lin(-duck_db.max(0.0));
    let stream_level = if mix.stream_muted { 0.0 } else { crate::audio::pw::db_to_lin(mix.stream_gain_db) * duck };
    let stream = if mix.stream_routed { stream_level } else { 0.0 };
    // VOD always follows Stream Gain and auto-mutes with Stream Mute (spec), independent of
    // whether the channel is routed into the Stream mix itself.
    let vod = if mix.vod { stream_level } else { 0.0 };
    let soloed_out = solo_active && !mix.solo;
    let monitor = if !mix.monitor_routed || mix.monitor_muted || soloed_out { 0.0 } else { crate::audio::pw::db_to_lin(mix.monitor_gain_db) };
    (stream, vod, monitor)
}

// ---------------------------------------------------------------------------------------------
// 1. Bus master: 4 safety limiters feeding the lingering Tier-1 buses
// ---------------------------------------------------------------------------------------------

pub fn bus_master_conf(p: &Plugins) -> String {
    let mut s = String::from(header());
    for (key, _class, desc, ceiling) in BUSES.iter() {
        let (nodes, links, inputs, outputs) = match &p.lsp {
            Some(lsp) => (
                vec![format!(
                    "{{ type = ladspa name = lim plugin = \"{}\" label = \"http://lsp-plug.in/plugins/ladspa/limiter_stereo\" control = {{ \"Threshold (G)\" = {:.6} \"Lookahead (ms)\" = 2.0 \"Attack time (ms)\" = 1.0 \"Release time (ms)\" = 20.0 \"Gain boost\" = 0 \"Automatic level regulation\" = 0 }} }}",
                    lsp, lin(*ceiling)
                )],
                vec![],
                vec!["lim:Input L".to_string(), "lim:Input R".to_string()],
                vec!["lim:Output L".to_string(), "lim:Output R".to_string()],
            ),
            // No LSP: a builtin hard clamp at the ceiling, so the bus still never exceeds it.
            None => (
                vec![
                    format!("{{ type = builtin name = l label = clamp control = {{ \"Min\" = {:.6} \"Max\" = {:.6} }} }}", -lin(*ceiling), lin(*ceiling)),
                    format!("{{ type = builtin name = r label = clamp control = {{ \"Min\" = {:.6} \"Max\" = {:.6} }} }}", -lin(*ceiling), lin(*ceiling)),
                ],
                vec![],
                vec!["l:In".to_string(), "r:In".to_string()],
                vec!["l:Out".to_string(), "r:Out".to_string()],
            ),
        };
        s.push_str(&filter_chain_module(
            &format!("{} safety limiter ({:+.1} dB)", desc, ceiling),
            &nodes,
            &links,
            &inputs,
            &outputs,
            &stream_props(&bus_sum(key), &format!("{} (mix input)", desc), "FL FR", ""),
            &stream_props(&bus_lim(key), &format!("{} (limited)", desc), "FL FR", ""),
        ));
    }
    s.push_str("]\n");
    s
}

// ---------------------------------------------------------------------------------------------
// 2. Mic pre-stage: mono-center upmix + preamp trim
// ---------------------------------------------------------------------------------------------

/// (FL gain, FR gain) for the pre-stage mono mixer.
pub fn pre_gains(mic: &MicState, trim_db: f32) -> (f32, f32) {
    let t = lin(trim_db.clamp(-24.0, 24.0));
    if mic.mono_center { (t, 0.0) } else { (0.5 * t, 0.5 * t) }
}

// ---------------------------------------------------------------------------------------------
// 3. Denoiser slot (single engine loaded at a time)
// ---------------------------------------------------------------------------------------------

pub fn denoiser_controls(d: &DenoiserConfig, engine: &str) -> Vec<(String, f32)> {
    match engine {
        "deepfilter" => vec![
            ("dn:Attenuation Limit (dB)".into(), d.df_attenuation_db.clamp(0.0, 100.0)),
            ("dn:Min processing threshold (dB)".into(), d.df_min_threshold_db.clamp(-15.0, 35.0)),
            ("dn:Max ERB processing threshold (dB)".into(), d.df_max_erb_threshold_db.clamp(-15.0, 35.0)),
        ],
        "rnnoise" => vec![
            ("dn:VAD Threshold (%)".into(), d.rn_vad_threshold.clamp(0.0, 99.0)),
            ("dn:VAD Grace Period (ms)".into(), d.rn_vad_grace_ms.clamp(0.0, 1000.0)),
            ("dn:Retroactive VAD Grace (ms)".into(), d.rn_retro_grace_ms.clamp(0.0, 200.0)),
        ],
        "speex" => vec![("dn:Noise Suppression (dB)".into(), d.speex_suppress_db.clamp(-60.0, -1.0))],
        _ => vec![],
    }
}

/// The denoiser LADSPA node for the mic chain (None for "off" or a missing plugin).
fn denoiser_node(p: &Plugins, d: &DenoiserConfig, engine: &str) -> Option<Node> {
    let controls = denoiser_controls(d, engine)
        .iter()
        .map(|(k, v)| format!("\"{}\" = {:.3}", k.trim_start_matches("dn:"), v))
        .collect::<Vec<_>>()
        .join(" ");
    match engine {
        "deepfilter" => Some(ladspa("dn", p.deepfilter.as_ref()?, "deep_filter_mono", &controls, "Audio In", "Audio Out")),
        "rnnoise" => Some(ladspa("dn", p.rnnoise.as_ref()?, "noise_suppressor_mono", &controls, "Input", "Output")),
        "speex" => Some(ladspa("dn", p.speex.as_ref()?, "speex_preprocess", &controls, "Input", "Output")),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// 3. Mic chain (ONE stage, so voice latency is a single hop):
//    Mono-center/trim ► [Denoiser] ► Gate ► HPF ► Hiss LPF ► De-Esser ► EQ ► Compressor ► Clipguard ► routes
// ---------------------------------------------------------------------------------------------

/// Filter types per EQ slot. Unused slots are flat peaking filters, so band count changes and
/// peaking/shelf edits never need a rebuild; only a change of filter TYPE does.
pub fn eq_signature(bands: &[EqBand]) -> Vec<FilterType> {
    (0..EQ_SLOTS).map(|i| bands.get(i).map(|b| b.filter_type).unwrap_or(FilterType::Peaking)).collect()
}

/// Places `bands` into a running chain whose slots have the filter types `sig`, so a re-sorted
/// band list (the UI sorts by frequency after a drag) is applied live instead of rebuilding the
/// chain. Series biquads commute, so slot order doesn't change the sound. Unused Peaking slots
/// become flat. `None` = the set of filter types really changed (a swap is needed).
pub fn fit_to_slots(bands: &[EqBand], sig: &[FilterType]) -> Option<Vec<EqBand>> {
    if bands.len() > sig.len() {
        return None;
    }
    let mut used = vec![false; bands.len()];
    let mut out = Vec::with_capacity(sig.len());
    for t in sig {
        match bands.iter().enumerate().find(|(i, b)| !used[*i] && b.filter_type == *t) {
            Some((i, b)) => {
                used[i] = true;
                out.push(b.clone());
            }
            None if *t == FilterType::Peaking => out.push(EqBand { freq: 1000.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking }),
            None => return None,
        }
    }
    used.iter().all(|u| *u).then_some(out)
}

pub fn eq_controls(prefix: &str, bands: &[EqBand]) -> Vec<(String, f32)> {
    let mut v = Vec::new();
    for i in 0..EQ_SLOTS {
        let n = format!("{}{}", prefix, i + 1);
        match bands.get(i) {
            Some(b) => {
                v.push((format!("{}:Freq", n), b.freq.clamp(10.0, 23500.0)));
                v.push((format!("{}:Q", n), b.q.clamp(0.1, 18.0)));
                v.push((format!("{}:Gain", n), b.gain_db.clamp(-24.0, 24.0)));
            }
            None => {
                v.push((format!("{}:Freq", n), 1000.0));
                v.push((format!("{}:Q", n), 1.0));
                v.push((format!("{}:Gain", n), 0.0));
            }
        }
    }
    v
}

/// Every live DSP control of the post chain (excluding EQ and route gains).
pub fn post_dsp_controls(nc: &NoiseCleanConfig, mic: &MicState, has_lsp: bool) -> Vec<(String, f32)> {
    let mut v = vec![
        ("hpf:Freq".to_string(), if nc.rumble_hp_enabled { nc.rumble_freq.clamp(20.0, 300.0) } else { 5.0 }),
        ("lpf:Freq".to_string(), if nc.hiss_lp_enabled { nc.hiss_freq.clamp(4000.0, 22000.0) } else { 23500.0 }),
    ];
    if has_lsp {
        let open = nc.gate_threshold_db.clamp(-90.0, -5.0);
        let close = open - GATE_HYSTERESIS_DB; // hysteresis: close 7 dB below open
        v.extend([
            ("gate:Bypass".to_string(), if nc.gate_enabled { 0.0 } else { 1.0 }),
            ("gate:Curve threshold (G)".to_string(), lin(open).clamp(1.6e-5, 1.0)),
            ("gate:Hysteresis threshold (G)".to_string(), lin(close).clamp(1.6e-5, 1.0)),
            ("deess:Bypass".to_string(), if nc.deesser_enabled { 0.0 } else { 1.0 }),
            ("deess:High-pass filter frequency (Hz)".to_string(), nc.deesser_freq.clamp(2000.0, 12000.0)),
            ("deess:Attack threshold (G)".to_string(), lin(nc.deesser_threshold_db.clamp(-60.0, 0.0)).clamp(0.001, 1.0)),
            ("deess:Ratio".to_string(), (1.0 + nc.deesser_amount_db.abs() * 0.75).clamp(1.0, 20.0)),
            ("comp:Bypass".to_string(), if nc.compressor_enabled { 0.0 } else { 1.0 }),
            ("comp:Attack threshold (G)".to_string(), lin(nc.compressor_threshold_db.clamp(-60.0, 0.0)).clamp(0.001, 1.0)),
            ("comp:Ratio".to_string(), nc.compressor_ratio.clamp(1.0, 20.0)),
            ("comp:Attack time (ms)".to_string(), nc.compressor_attack_ms.clamp(0.1, 200.0)),
            ("comp:Release time (ms)".to_string(), nc.compressor_release_ms.clamp(5.0, 2000.0)),
            ("comp:Makeup gain (G)".to_string(), lin(nc.compressor_makeup_db.clamp(-12.0, 24.0))),
            ("clip:Bypass".to_string(), if mic.clipguard { 0.0 } else { 1.0 }),
            ("clip:Threshold (G)".to_string(), lin(nc.limiter_ceiling_db.clamp(-12.0, -0.5))),
        ]);
    } else {
        // Static de-esser fallback when LSP is missing.
        v.push(("deess:Freq".to_string(), nc.deesser_freq.clamp(2000.0, 12000.0)));
        v.push(("deess:Gain".to_string(), if nc.deesser_enabled { nc.deesser_amount_db.clamp(-18.0, 0.0) } else { 0.0 }));
        // Clipguard fallback: builtin clamp at the ceiling (opened wide when Clipguard is off).
        let c = if mic.clipguard { lin(nc.limiter_ceiling_db.clamp(-12.0, -0.5)) } else { 1000.0 };
        v.push(("clip:Min".to_string(), -c));
        v.push(("clip:Max".to_string(), c));
    }
    v
}

/// Route gains of the mic post chain: Voice (Discord), Stream, VOD, Monitor.
pub fn post_route_controls(voice: f32, stream: f32, vod: f32, monitor: f32) -> Vec<(String, f32)> {
    vec![
        ("vl:Gain 1".into(), voice),
        ("vr:Gain 1".into(), voice),
        ("sl:Gain 1".into(), stream),
        ("sr:Gain 1".into(), stream),
        ("dl:Gain 1".into(), vod),
        ("dr:Gain 1".into(), vod),
        ("ml:Gain 1".into(), monitor),
        ("mr:Gain 1".into(), monitor),
    ]
}

fn controls_str(c: &[(String, f32)], node: &str) -> String {
    c.iter()
        .filter_map(|(k, v)| k.strip_prefix(&format!("{}:", node)).map(|port| format!("\"{}\" = {:.6}", port, v)))
        .collect::<Vec<_>>()
        .join(" ")
}

#[allow(clippy::too_many_arguments)]
pub fn mic_post_conf(
    p: &Plugins,
    generation: u32,
    engine: &str,
    denoiser: &DenoiserConfig,
    bands: &[EqBand],
    nc: &NoiseCleanConfig,
    mic: &MicState,
    trim_db: f32,
    routes: (f32, f32, f32, f32),
) -> String {
    let dsp = post_dsp_controls(nc, mic, p.lsp.is_some());
    let eq = eq_controls("eq", bands);
    let mut chain: Vec<Node> = Vec::new();
    let (g1, g2) = pre_gains(mic, trim_db);
    // Mono-center + calibration trim: FL/FR in, mono out.
    chain.push(Node {
        name: "mono".into(),
        def: mixer("mono", &format!("\"Gain 1\" = {:.6} \"Gain 2\" = {:.6}", g1, g2)),
        inp: "In 1",
        out: "Out",
    });
    if let Some(dn) = denoiser_node(p, denoiser, engine) {
        chain.push(dn);
    }

    if let Some(lsp) = &p.lsp {
        chain.push(ladspa(
            "gate",
            lsp,
            "http://lsp-plug.in/plugins/ladspa/gate_mono",
            &format!(
                // Lookahead + soft knee + longer hold: opens smoothly before a transient instead
                // of slicing it (no crackle), and does not flap between keystrokes or syllables.
                "{} \"Hysteresis\" = 1 \"Sidechain lookahead (ms)\" = 3.0 \
                 \"Curve zone size (G)\" = 0.707 \"Hysteresis zone size (G)\" = 0.707 \
                 \"High-pass filter mode\" = 2 \"High-pass filter frequency (Hz)\" = 150 \
                 \"Low-pass filter mode\" = 2 \"Low-pass filter frequency (Hz)\" = 3500 \"Attack (ms)\" = 8.0 \
                 \"Hold time (ms)\" = 250 \"Release (ms)\" = 200 \"Reduction (G)\" = 0.05",
                controls_str(&dsp, "gate")
            ),
            "Input",
            "Output",
        ));
    }
    chain.push(builtin("hpf", "bq_highpass", &format!("{} \"Q\" = 0.707", controls_str(&dsp, "hpf"))));
    chain.push(builtin("lpf", "bq_lowpass", &format!("{} \"Q\" = 0.707", controls_str(&dsp, "lpf"))));
    match &p.lsp {
        Some(lsp) => chain.push(ladspa(
            "deess",
            lsp,
            "http://lsp-plug.in/plugins/ladspa/compressor_mono",
            &format!(
                "{} \"High-pass filter mode\" = 3 \"Attack time (ms)\" = 1 \"Release time (ms)\" = 40 \"Makeup gain (G)\" = 1",
                controls_str(&dsp, "deess")
            ),
            "Input",
            "Output",
        )),
        None => chain.push(builtin("deess", "bq_peaking", &format!("{} \"Q\" = 2.5", controls_str(&dsp, "deess")))),
    }
    for (i, ft) in eq_signature(bands).iter().enumerate() {
        let n = format!("eq{}", i + 1);
        chain.push(builtin(&n, ft.pipewire_label(), &controls_str(&eq, &n)));
    }
    if let Some(lsp) = &p.lsp {
        chain.push(ladspa(
            "comp",
            lsp,
            "http://lsp-plug.in/plugins/ladspa/compressor_mono",
            &controls_str(&dsp, "comp"),
            "Input",
            "Output",
        ));
        chain.push(ladspa(
            "clip",
            lsp,
            "http://lsp-plug.in/plugins/ladspa/limiter_mono",
            &format!(
                "{} \"Threshold (G)\" = {:.6} \"Lookahead (ms)\" = 5 \"Attack time (ms)\" = 1 \"Release time (ms)\" = 5 \"Gain boost\" = 0 \"Automatic level regulation\" = 0",
                controls_str(&dsp, "clip"),
                lin(nc.limiter_ceiling_db.clamp(-12.0, -0.5))
            ),
            "Input",
            "Output",
        ));
    } else {
        chain.push(builtin("clip", "clamp", &controls_str(&dsp, "clip")));
    }
    chain.push(Node { name: "split".into(), def: "{ type = builtin name = split label = copy }".into(), inp: "In", out: "Out" });

    let mut nodes: Vec<String> = chain.iter().map(|n| n.def.clone()).collect();
    let mut links: Vec<String> = chain
        .windows(2)
        .map(|w| link(&format!("{}:{}", w[0].name, w[0].out), &format!("{}:{}", w[1].name, w[1].inp)))
        .collect();

    let (voice, stream, vod, monitor) = routes;
    let route = post_route_controls(voice, stream, vod, monitor);
    let mut outputs = Vec::new();
    for m in ["vl", "vr", "sl", "sr", "dl", "dr", "ml", "mr"] {
        nodes.push(mixer(m, &controls_str(&route, m)));
        links.push(link("split:Out", &format!("{}:In 1", m)));
        outputs.push(format!("{}:Out", m));
    }
    nodes.push(mixer("tap", "\"Gain 1\" = 1.0"));
    links.push(link("split:Out", "tap:In 1"));
    outputs.push("tap:Out".into());

    let mut s = String::from(header());
    s.push_str(&filter_chain_module(
        "Studio Mic voice chain",
        &nodes,
        &links,
        &["mono:In 1".to_string(), "mono:In 2".to_string()],
        &outputs,
        &stream_props(&post_in(generation), "Studio Mic input (hardware)", "FL FR", ""),
        &stream_props(&post_out(generation), "Studio Mic processed voice", &aux_positions(9), ""),
    ));
    s.push_str("]\n");
    s
}

// ---------------------------------------------------------------------------------------------
// 5. Channel strips (raw passthrough) and the Mic 2 strip
// ---------------------------------------------------------------------------------------------

pub fn channel_controls(stream: f32, vod: f32, monitor: f32, mix: &ChannelMix) -> Vec<(String, f32)> {
    let ds = (mix.delay_stream_ms / 1000.0).clamp(0.0, 1.0);
    let dm = (mix.delay_monitor_ms / 1000.0).clamp(0.0, 1.0);
    vec![
        ("sl:Gain 1".into(), stream),
        ("sr:Gain 1".into(), stream),
        ("dl:Gain 1".into(), vod),
        ("dr:Gain 1".into(), vod),
        ("ml:Gain 1".into(), monitor),
        ("mr:Gain 1".into(), monitor),
        ("ds_l:Delay (s)".into(), ds),
        ("ds_r:Delay (s)".into(), ds),
        ("dm_l:Delay (s)".into(), dm),
        ("dm_r:Delay (s)".into(), dm),
    ]
}

fn strip_routing(nodes: &mut Vec<String>, links: &mut Vec<String>, src_l: &str, src_r: &str, c: &[(String, f32)]) -> Vec<String> {
    for d in ["ds_l", "ds_r", "dm_l", "dm_r"] {
        nodes.push(format!(
            "{{ type = builtin name = {} label = delay config = {{ \"max-delay\" = 1.0 }} control = {{ {} }} }}",
            d,
            controls_str(c, d)
        ));
    }
    links.push(link(src_l, "ds_l:In"));
    links.push(link(src_r, "ds_r:In"));
    links.push(link(src_l, "dm_l:In"));
    links.push(link(src_r, "dm_r:In"));
    let wiring = [("sl", "ds_l"), ("sr", "ds_r"), ("dl", "ds_l"), ("dr", "ds_r"), ("ml", "dm_l"), ("mr", "dm_r")];
    let mut outputs = Vec::new();
    for (m, d) in wiring {
        nodes.push(mixer(m, &controls_str(c, m)));
        links.push(link(&format!("{}:Out", d), &format!("{}:In 1", m)));
        outputs.push(format!("{}:Out", m));
    }
    nodes.push(mixer("tap", "\"Gain 1\" = 0.5 \"Gain 2\" = 0.5"));
    links.push(link(src_l, "tap:In 1"));
    links.push(link(src_r, "tap:In 2"));
    outputs.push("tap:Out".into());
    outputs
}

pub fn channel_conf(ch: &VirtualChannelConfig, gains: (f32, f32, f32)) -> String {
    let c = channel_controls(gains.0, gains.1, gains.2, &ch.mix);
    let mut nodes = vec![
        "{ type = builtin name = inl label = copy }".to_string(),
        "{ type = builtin name = inr label = copy }".to_string(),
    ];
    let mut links = Vec::new();
    let outputs = strip_routing(&mut nodes, &mut links, "inl:Out", "inr:Out", &c);
    let desc = format!("Studio Mic · {}", spa_string_safe(&ch.name));
    let mut s = String::from(header());
    s.push_str(&filter_chain_module(
        &desc,
        &nodes,
        &links,
        &["inl:In".into(), "inr:In".into()],
        &outputs,
        // A real Audio/Sink so apps (and pavucontrol) can target it. No node.dont-fallback / linger:
        // if Studio Mic stops, apps fall back to the default output.
        &format!(
            "node.name = \"{}\"\n                node.description = \"{}\"\n                media.class = Audio/Sink\n                audio.position = [ FL FR ]\n                node.pause-on-idle = false\n                node.group = studiomic\n                priority.session = 0",
            channel_sink(&ch.id),
            desc
        ),
        &stream_props(&channel_out(&ch.id), &format!("{} (routing)", desc), &aux_positions(7), ""),
    ));
    s.push_str("]\n");
    s
}

pub fn mic2_conf(bands: &[EqBand], mix: &ChannelMix, gains: (f32, f32, f32)) -> String {
    let c = channel_controls(gains.0, gains.1, gains.2, mix);
    let eq = eq_controls("m2eq", bands);
    let mut nodes = Vec::new();
    let mut links = Vec::new();
    let sig = eq_signature(bands);
    for (i, ft) in sig.iter().enumerate() {
        let n = format!("m2eq{}", i + 1);
        nodes.push(format!(
            "{{ type = builtin name = {} label = {} control = {{ {} }} }}",
            n,
            ft.pipewire_label(),
            controls_str(&eq, &n)
        ));
        if i > 0 {
            links.push(link(&format!("m2eq{}:Out", i), &format!("{}:In", n)));
        }
    }
    nodes.push("{ type = builtin name = split label = copy }".to_string());
    links.push(link(&format!("m2eq{}:Out", EQ_SLOTS), "split:In"));
    let outputs = strip_routing(&mut nodes, &mut links, "split:Out", "split:Out", &c);
    let mut s = String::from(header());
    s.push_str(&filter_chain_module(
        "Studio Mic · Microphone 2",
        &nodes,
        &links,
        &["m2eq1:In".into()],
        &outputs,
        &stream_props(MIC2_IN, "Studio Mic Microphone 2 input", "MONO", ""),
        &stream_props(MIC2_OUT, "Studio Mic Microphone 2", &aux_positions(7), ""),
    ));
    s.push_str("]\n");
    s
}

// ---------------------------------------------------------------------------------------------
// File output
// ---------------------------------------------------------------------------------------------

pub fn runtime_dir() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(base).join("studio-mic")
}

/// Writes `sm-<name>.conf` into $XDG_RUNTIME_DIR/studio-mic/ and returns its path.
pub fn write_conf(name: &str, content: &str) -> std::io::Result<PathBuf> {
    let dir = runtime_dir();
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("sm-{}.conf", name));
    fs::write(&path, content)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bands() -> Vec<EqBand> {
        vec![
            EqBand { freq: 85.0, gain_db: 0.0, q: 0.7, filter_type: FilterType::HighPass },
            EqBand { freq: 3000.0, gain_db: 2.5, q: 1.2, filter_type: FilterType::Peaking },
        ]
    }

    #[test]
    fn eq_signature_pads_with_peaking() {
        let sig = eq_signature(&bands());
        assert_eq!(sig.len(), EQ_SLOTS);
        assert_eq!(sig[0], FilterType::HighPass);
        assert!(sig[2..].iter().all(|t| *t == FilterType::Peaking));
    }

    #[test]
    fn vod_follows_stream_and_mute() {
        let mut m = ChannelMix::default();
        m.stream_gain_db = -6.0;
        let (s, v, mon) = route_gains(&m, 0.0, false);
        assert!((s - v).abs() < 1e-6 && (mon - 1.0).abs() < 1e-6);
        m.stream_muted = true;
        let (s, v, _) = route_gains(&m, 0.0, false);
        assert_eq!((s, v), (0.0, 0.0));
        m.stream_muted = false;
        m.vod = false;
        assert_eq!(route_gains(&m, 0.0, false).1, 0.0);
    }

    #[test]
    fn ducking_never_touches_monitor_and_solo_mutes_others() {
        let m = ChannelMix::default();
        let (s, _, mon) = route_gains(&m, 12.0, false);
        assert!((s - 0.2512).abs() < 0.01);
        assert!((mon - 1.0).abs() < 1e-6);
        assert_eq!(route_gains(&m, 0.0, true).2, 0.0);
    }

    #[test]
    fn post_conf_contains_all_stages_and_outputs() {
        let p = Plugins { lsp: Some("/x/lsp.so".into()), ..Default::default() };
        let p = Plugins { deepfilter: Some("/x/df.so".into()), ..p };
        let conf = mic_post_conf(&p, 3, "deepfilter", &DenoiserConfig::default(), &bands(), &NoiseCleanConfig::default(), &MicState::default(), 0.0, (1.0, 1.0, 1.0, 0.0));
        for needle in ["deep_filter_mono", "name = mono label = mixer", "gate_mono", "bq_highpass", "compressor_mono", "limiter_mono", "\"Hysteresis\" = 1", "\"Sidechain lookahead (ms)\" = 3.0", "\"Hysteresis zone size (G)\" = 0.707", "\"Curve zone size (G)\" = 0.707", "\"Hold time (ms)\" = 250", "StudioMic_Mic_g3_In", "AUX8", "\"tap:Out\"", "\"Gain boost\" = 0"] {
            assert!(conf.contains(needle), "missing {}", needle);
        }
        // Without LSP the chain degrades gracefully to builtin-only stages.
        let conf2 = mic_post_conf(&Plugins::default(), 1, "deepfilter", &DenoiserConfig::default(), &bands(), &NoiseCleanConfig::default(), &MicState::default(), 0.0, (1.0, 1.0, 1.0, 0.0));
        assert!(!conf2.contains("ladspa"));
        assert!(conf2.contains("name = deess label = bq_peaking"));
    }

    #[test]
    fn bus_limiters_never_boost_to_full_scale() {
        // LSP limiters default to "Gain boost" = 1, which re-normalises output to 0 dBFS.
        let conf = bus_master_conf(&Plugins { lsp: Some("/x/lsp.so".into()), ..Default::default() });
        assert_eq!(conf.matches("\"Gain boost\" = 0").count(), 4);
        assert_eq!(conf.matches("\"Automatic level regulation\" = 0").count(), 4);
    }

    #[test]
    fn matrix_routing_is_separate_from_mute() {
        let mut m = ChannelMix::default();
        m.stream_routed = false;
        let (s, v, mo) = route_gains(&m, 0.0, false);
        assert_eq!(s, 0.0, "unrouted from Stream");
        assert!((v - 1.0).abs() < 1e-6, "VOD still follows Stream Gain");
        assert!((mo - 1.0).abs() < 1e-6);
        m.stream_routed = true;
        m.stream_muted = true;
        assert_eq!(route_gains(&m, 0.0, false).1, 0.0, "VOD mutes with Stream Mute");
        m.stream_muted = false;
        m.monitor_routed = false;
        assert_eq!(route_gains(&m, 0.0, false).2, 0.0);
    }

    #[test]
    fn resorted_bands_fit_the_running_slots() {
        let running = eq_signature(&bands()); // [HighPass, Peaking, Peaking x8]
        // The UI re-sorted: the peaking band dragged below the high-pass.
        let resorted = vec![
            EqBand { freq: 60.0, gain_db: 2.5, q: 1.2, filter_type: FilterType::Peaking },
            EqBand { freq: 85.0, gain_db: 0.0, q: 0.7, filter_type: FilterType::HighPass },
        ];
        let fit = fit_to_slots(&resorted, &running).expect("same types: no swap");
        assert_eq!(eq_signature(&fit), running);
        assert_eq!(fit[0].freq, 85.0);
        assert_eq!(fit[1].freq, 60.0);
        // A new filter type still needs a swap.
        let mut changed = resorted.clone();
        changed[0].filter_type = FilterType::LowShelf;
        assert!(fit_to_slots(&changed, &running).is_none());
        // Removing a band leaves its Peaking slot flat.
        let fewer = fit_to_slots(&resorted[1..], &running).unwrap();
        assert_eq!(fewer[1].gain_db, 0.0);
    }

    #[test]
    fn channel_names_cannot_break_the_config() {
        let mut ch = crate::config::preset_channel("music");
        ch.name = "Back\\slash \"quoted\"\nline".into();
        let conf = channel_conf(&ch, (1.0, 1.0, 1.0));
        assert!(conf.contains("Studio Mic · Back/slash 'quoted' line"));
    }

    #[test]
    fn without_lsp_buses_and_mic_still_have_a_ceiling() {
        // No LSP: buses and Clipguard fall back to the builtin clamp instead of passing overs.
        let conf = bus_master_conf(&Plugins::default());
        assert_eq!(conf.matches("label = clamp").count(), 8);
        assert!(conf.contains(&format!("\"Max\" = {:.6}", lin(-1.0))));
        assert!(conf.contains(&format!("\"Max\" = {:.6}", lin(-3.0))));
        let conf = mic_post_conf(&Plugins::default(), 1, "off", &DenoiserConfig::default(), &bands(), &NoiseCleanConfig::default(), &MicState::default(), 0.0, (1.0, 1.0, 1.0, 0.0));
        assert!(conf.contains("name = clip label = clamp"));
        assert!(conf.contains("output = \"clip:Out\" input = \"split:In\""));
        // Clipguard off opens the clamp wide.
        let mut mic = MicState::default();
        mic.clipguard = false;
        let c = post_dsp_controls(&NoiseCleanConfig::default(), &mic, false);
        assert!(c.iter().any(|(k, v)| k == "clip:Max" && *v >= 100.0));
        mic.clipguard = true;
        let c = post_dsp_controls(&NoiseCleanConfig::default(), &mic, false);
        assert!(c.iter().any(|(k, v)| k == "clip:Max" && *v < 1.0));
    }

    #[test]
    fn hysteresis_closes_seven_db_below_open() {
        let mut nc = NoiseCleanConfig::default();
        nc.gate_threshold_db = -40.0;
        let c = post_dsp_controls(&nc, &MicState::default(), true);
        let get = |k: &str| c.iter().find(|(n, _)| n == k).unwrap().1;
        let open_db = 20.0 * get("gate:Curve threshold (G)").log10();
        let close_db = 20.0 * get("gate:Hysteresis threshold (G)").log10();
        assert!((open_db - close_db - GATE_HYSTERESIS_DB).abs() < 0.01);
    }
}
