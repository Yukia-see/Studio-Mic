use crate::models::microphone::{EqBand, FilterType};
use crate::config::{VirtualChannelConfig, NoiseCleanConfig};
use std::fs;
use std::path::{Path, PathBuf};

struct AudioNode {
    name: String,
    def: String,
    in_port: &'static str,
    out_port: &'static str,
}

pub struct FilterChainConfigGenerator;

impl FilterChainConfigGenerator {
    /// Generates the complete PipeWire SPA configuration string for the 7-stage broadcast pipeline:
    /// 1. Sub-Rumble High-Pass Filter (80-100Hz)
    /// 2. Acoustic Hiss Roll-Off Low-Pass Filter (16kHz)
    /// 3. Deep Noise Remover (DeepFilterNet CPU AI - EasyEffects Core)
    /// 4. Noise Gate (LSP Gate)
    /// 5. Sibilance De-Esser (4-7kHz)
    /// 6. Vocal Compressor & Automatic Leveler (LSP Compressor)
    /// 7. 10-Band Parametric Microphone Tuning (AM8, SM7B, etc.)
    /// 8. Brick Wall Limiter (-2.0 dB ceiling for OBS/Stream safety)
    pub fn generate_complete_spa_config(
        bands: &[EqBand],
        channels: &[VirtualChannelConfig],
        mic2_bands: Option<&[EqBand]>,
        noise_clean: Option<&NoiseCleanConfig>,
    ) -> String {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/yukia".into());
        let df_plugin_path = format!("{}/.ladspa/libdeep_filter_ladspa.so", home);
        let lsp_plugin_path = format!("{}/.ladspa/lsp-plugins-ladspa.so", home);

        let has_deep_filter = Path::new(&df_plugin_path).exists();
        let has_lsp = Path::new(&lsp_plugin_path).exists();

        let mut nodes: Vec<AudioNode> = Vec::new();

        if let Some(nc) = noise_clean {
            // Stage 1: Sub-Rumble High-Pass Filter
            if nc.rumble_hp_enabled {
                let freq = nc.rumble_freq.clamp(20.0, 300.0);
                nodes.push(AudioNode {
                    name: "sub_rumble_hp".into(),
                    def: format!(
r#"                    {{
                        type    = builtin
                        name    = sub_rumble_hp
                        label   = bq_highpass
                        control = {{ "Freq" = {:.1} "Q" = 0.707 }}
                    }}
"#,
                        freq
                    ),
                    in_port: "In",
                    out_port: "Out",
                });
            }

            // Stage 2: Acoustic Hiss Roll-Off Low-Pass Filter
            if nc.hiss_lp_enabled {
                let freq = nc.hiss_freq.clamp(6000.0, 22000.0);
                nodes.push(AudioNode {
                    name: "hiss_lp".into(),
                    def: format!(
r#"                    {{
                        type    = builtin
                        name    = hiss_lp
                        label   = bq_lowpass
                        control = {{ "Freq" = {:.1} "Q" = 0.707 }}
                    }}
"#,
                        freq
                    ),
                    in_port: "In",
                    out_port: "Out",
                });
            }

            // Stage 3: Deep Noise Remover (DeepFilterNet CPU AI - EasyEffects Core)
            if nc.deep_filter_enabled && has_deep_filter {
                let atten = nc.deep_filter_attenuation_db.clamp(0.0, 100.0);
                let thresh = nc.deep_filter_threshold_db.clamp(-30.0, 30.0);
                nodes.push(AudioNode {
                    name: "deep_filter".into(),
                    def: format!(
r#"                    {{
                        type    = ladspa
                        name    = deep_filter
                        plugin  = "{}"
                        label   = deep_filter_mono
                        control = {{
                            "Attenuation Limit (dB)" = {:.1}
                            "Min processing threshold (dB)" = {:.1}
                        }}
                    }}
"#,
                        df_plugin_path, atten, thresh
                    ),
                    in_port: "Audio In",
                    out_port: "Audio Out",
                });
            }

            // Stage 4: Noise Gate (LSP Gate)
            if nc.gate_enabled && has_lsp {
                let lin_thresh = 10.0_f32.powf(nc.gate_threshold_db / 20.0).clamp(0.0001, 1.0);
                nodes.push(AudioNode {
                    name: "gate".into(),
                    def: format!(
r#"                    {{
                        type    = ladspa
                        name    = gate
                        plugin  = "{}"
                        label   = "http://lsp-plug.in/plugins/ladspa/gate_mono"
                        control = {{
                            "Curve threshold (G)" = {:.6}
                            "Attack (ms)" = 20.0
                            "Hold time (ms)" = 150.0
                            "Release (ms)" = 150.0
                        }}
                    }}
"#,
                        lsp_plugin_path, lin_thresh
                    ),
                    in_port: "Input",
                    out_port: "Output",
                });
            }

            // Stage 5: Sibilance De-Esser
            if nc.deesser_enabled {
                let freq = nc.deesser_freq.clamp(3500.0, 8500.0);
                let gain = nc.deesser_amount_db.clamp(-18.0, 0.0);
                nodes.push(AudioNode {
                    name: "deesser".into(),
                    def: format!(
r#"                    {{
                        type    = builtin
                        name    = deesser
                        label   = bq_peaking
                        control = {{ "Freq" = {:.1} "Q" = 2.5 "Gain" = {:.2} }}
                    }}
"#,
                        freq, gain
                    ),
                    in_port: "In",
                    out_port: "Out",
                });
            }

            // Stage 6: Vocal Compressor & Automatic Leveler
            if nc.compressor_enabled && has_lsp {
                let lin_thresh = 10.0_f32.powf(nc.compressor_threshold_db / 20.0).clamp(0.001, 1.0);
                let lin_makeup = 10.0_f32.powf(nc.compressor_makeup_db / 20.0).clamp(0.1, 100.0);
                let ratio = nc.compressor_ratio.clamp(1.5, 10.0);
                nodes.push(AudioNode {
                    name: "compressor".into(),
                    def: format!(
r#"                    {{
                        type    = ladspa
                        name    = compressor
                        plugin  = "{}"
                        label   = "http://lsp-plug.in/plugins/ladspa/compressor_mono"
                        control = {{
                            "Attack threshold (G)" = {:.6}
                            "Ratio" = {:.2}
                            "Attack time (ms)" = 10.0
                            "Release time (ms)" = 80.0
                            "Makeup gain (G)" = {:.4}
                        }}
                    }}
"#,
                        lsp_plugin_path, lin_thresh, ratio, lin_makeup
                    ),
                    in_port: "Input",
                    out_port: "Output",
                });
            }
        }

        // Stage 7: 10-Band Parametric Microphone EQ Bands
        for (i, band) in bands.iter().enumerate() {
            let node_num = i + 1;
            let label = band.filter_type.pipewire_label();

            let control_str = if band.filter_type == FilterType::Peaking
                || band.filter_type == FilterType::HighShelf
                || band.filter_type == FilterType::LowShelf
            {
                format!("\"Freq\" = {:.1} \"Q\" = {:.2} \"Gain\" = {:.2}", band.freq, band.q, band.gain_db)
            } else {
                format!("\"Freq\" = {:.1} \"Q\" = {:.2}", band.freq, band.q)
            };

            nodes.push(AudioNode {
                name: format!("eq_band_{}", node_num),
                def: format!(
r#"                    {{
                        type    = builtin
                        name    = eq_band_{}
                        label   = {}
                        control = {{ {} }}
                    }}
"#,
                    node_num, label, control_str
                ),
                in_port: "In",
                out_port: "Out",
            });
        }

        // Stage 8: Brick Wall Limiter (-2.0 dB ceiling)
        if let Some(nc) = noise_clean {
            if nc.limiter_enabled && has_lsp {
                let lin_ceil = 10.0_f32.powf(nc.limiter_ceiling_db / 20.0).clamp(0.1, 1.0);
                nodes.push(AudioNode {
                    name: "limiter".into(),
                    def: format!(
r#"                    {{
                        type    = ladspa
                        name    = limiter
                        plugin  = "{}"
                        label   = "http://lsp-plug.in/plugins/ladspa/limiter_mono"
                        control = {{
                            "Threshold (G)" = {:.4}
                            "Attack time (ms)" = 1.0
                            "Release time (ms)" = 20.0
                        }}
                    }}
"#,
                        lsp_plugin_path, lin_ceil
                    ),
                    in_port: "Input",
                    out_port: "Output",
                });
            }
        }

        // Fallback safety: ensure at least one node exists
        if nodes.is_empty() {
            nodes.push(AudioNode {
                name: "eq_band_1".into(),
                def: r#"                    {
                        type    = builtin
                        name    = eq_band_1
                        label   = bq_peaking
                        control = { "Freq" = 1000.0 "Q" = 1.0 "Gain" = 0.0 }
                    }
"#.into(),
                in_port: "In",
                out_port: "Out",
            });
        }

        let mut nodes_str = String::new();
        let mut links_str = String::new();

        for (i, node) in nodes.iter().enumerate() {
            nodes_str.push_str(&node.def);
            if i > 0 {
                let prev = &nodes[i - 1];
                links_str.push_str(&format!(
                    "                    {{ output = \"{}:{}\" input = \"{}:{}\" }}\n",
                    prev.name, prev.out_port, node.name, node.in_port
                ));
            }
        }

        let first_node = &nodes[0];
        let last_node = &nodes[nodes.len() - 1];
        let inputs_str = format!("\"{}\"", format!("{}:{}", first_node.name, first_node.in_port));
        let outputs_str = format!("\"{}\"", format!("{}:{}", last_node.name, last_node.out_port));

        // Optional Mic 2 Processing Graph (Dual Microphone Setup)
        let mut mic2_module_str = String::new();
        if let Some(m2_bands) = mic2_bands {
            let mut m2_nodes = String::new();
            let mut m2_links = String::new();

            for (i, band) in m2_bands.iter().enumerate() {
                let node_num = i + 1;
                let label = band.filter_type.pipewire_label();

                let control_str = if band.filter_type == FilterType::Peaking
                    || band.filter_type == FilterType::HighShelf
                    || band.filter_type == FilterType::LowShelf
                {
                    format!("\"Freq\" = {:.1} \"Q\" = {:.2} \"Gain\" = {:.2}", band.freq, band.q, band.gain_db)
                } else {
                    format!("\"Freq\" = {:.1} \"Q\" = {:.2}", band.freq, band.q)
                };

                m2_nodes.push_str(&format!(
                    "                    {{\n                        type  = builtin\n                        name  = eq2_band_{}\n                        label = {}\n                        control = {{ {} }}\n                    }}\n",
                    node_num, label, control_str
                ));

                if i > 0 {
                    m2_links.push_str(&format!(
                        "                    {{ output = \"eq2_band_{}:Out\" input = \"eq2_band_{}:In\" }}\n",
                        i, node_num
                    ));
                }
            }

            mic2_module_str = format!(
r#"    # 2. Secondary Microphone Processing Filter-Chain (Mic 2 / Dual Mic Setup)
    {{ name = libpipewire-module-filter-chain
        args = {{
            node.description = "StudioMic Secondary Broadcast (Tuned)"
            media.name       = "StudioMic Secondary Microphone"
            filter.graph = {{
                nodes = [
{}                ]
                links = [
{}                ]
            }}
            audio.position = [ MONO ]
            capture.props = {{
                node.name    = "studiomic_mic2.capture"
                node.passive = true
            }}
            playback.props = {{
                node.name         = "studiomic_processed_mic2"
                media.class       = Audio/Source
                audio.position    = [ MONO ]
                node.autoconnect  = false
            }}
        }}
    }}
"#,
                m2_nodes, m2_links
            );
        }

        let mut loopbacks_str = String::new();
        for ch in channels {
            loopbacks_str.push_str(&format!(
r#"    # Virtual Isolated Input: {name} for OBS / System Sound
    {{ name = libpipewire-module-loopback
        args = {{
            node.description = "StudioMic {name} (Virtual Input)"
            capture.props = {{
                node.name         = "studiomic_{id}.capture"
                node.description  = "StudioMic {name} (Virtual Sink)"
                audio.position    = [ FL FR ]
                node.passive      = true
                node.autoconnect  = false
            }}
            playback.props = {{
                node.name         = "studiomic_{id}"
                media.class       = "Audio/Source"
                audio.position    = [ FL FR ]
                node.description  = "StudioMic {name} (OBS Source)"
                node.autoconnect  = false
            }}
        }}
    }}
"#,
                name = ch.name,
                id = ch.id
            ));
        }

        format!(
r#"# StudioMic Complete PipeWire Audio Graph Configuration
context.properties = {{
    log.level = 0
}}

context.spa-libs = {{
    audio.convert.* = audioconvert/libspa-audioconvert
    support.*       = support/libspa-support
}}

context.modules = [
    {{ name = libpipewire-module-protocol-native }}
    {{ name = libpipewire-module-client-node }}
    {{ name = libpipewire-module-adapter }}

    # 1. Primary Complete 7-Stage Broadcast Processing Filter-Chain
    {{ name = libpipewire-module-filter-chain
        args = {{
            node.description = "StudioMic Broadcast Master (Tuned)"
            media.name       = "StudioMic Tuned Microphone"
            filter.graph = {{
                nodes = [
{}                ]
                links = [
{}                ]
                inputs  = [ {} ]
                outputs = [ {} ]
            }}
            audio.position = [ MONO ]
            capture.props = {{
                node.name    = "studiomic.capture"
                node.passive = true
            }}
            playback.props = {{
                node.name        = "studiomic_processed_mic"
                media.class      = Audio/Source
                node.autoconnect = false
            }}
        }}
    }}

{}
{}
]
"#,
            nodes_str, links_str, inputs_str, outputs_str, mic2_module_str, loopbacks_str
        )
    }

    /// Writes configuration to runtime directory
    pub fn write_config_to_runtime(
        bands: &[EqBand],
        channels: &[VirtualChannelConfig],
        mic2_bands: Option<&[EqBand]>,
        noise_clean: Option<&NoiseCleanConfig>,
        filename: &str,
    ) -> std::io::Result<PathBuf> {
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| "/tmp".into());
        let config_path = Path::new(&runtime_dir).join("studiomic").join(filename);

        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let config_str = Self::generate_complete_spa_config(bands, channels, mic2_bands, noise_clean);
        fs::write(&config_path, config_str)?;

        Ok(config_path)
    }
}
