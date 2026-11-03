//! Tab 1 routing matrix: builds rows / columns / cells from the saved state with the same rules
//! as the HTML UI (`mxCell`, `mxLevel`, `mxSourceHtml`), and turns clicks into the same messages.

use super::*;
use crate::audio::device_discovery::AudioDevice;
use crate::config::{ChannelMix, StudioMicConfig};
use crate::routing::wireplumber_hook::{HookedStream, InstalledApp};

/// One-click channel presets (`AC_PRESETS`): (preset, icon, title, default name, subtitle, detail).
/// detail: "" adds at once, "hw" asks for a hardware input, "name" asks for a name.
const PRESETS: [(&str, &str, &str, &str, &str, &str); 7] = [
    ("voice", "forum", "Discord / Voice Chat", "Voice Chat", "Discord, Vesktop, TeamSpeak, Mumble", ""),
    ("game", "sports_esports", "Games", "Game", "Steam, Proton, Wine, Lutris", ""),
    ("browser", "language", "Web Browser", "Browser", "Firefox, Chromium, Brave, Zen", ""),
    ("music", "music_note", "Music", "Music", "Spotify, Elisa, MPD (off the VOD track)", ""),
    ("apps", "apps", "Apps & Media", "Apps & Media", "VLC, mpv and other players", ""),
    ("capture", "settings_input_component", "Second Mic / Line In", "Line In", "A hardware input: USB, XLR, capture card", "hw"),
    ("", "tune", "Custom", "New Channel", "An empty virtual output you name", "name"),
];

const MIX_COLORS: [&str; 5] = ["#22d3ee", "#34d399", "#a78bfa", "#fbbf24", "#fb7185"];

#[derive(Default)]
pub(super) struct MatrixState {
    cols: Rc<VecModel<MxColData>>,
    rows: Rc<VecModel<MxRowData>>,
    cells: Rc<VecModel<MxCellData>>,
    meters: Rc<VecModel<f32>>,
    col_keys: RefCell<Vec<String>>,
    row_ids: RefCell<Vec<String>>,
    /// Per column: option index → output device (None = default / no output).
    col_outputs: RefCell<Vec<Vec<Option<String>>>>,
    /// Per row: option index → hardware input node.name.
    row_hw: RefCell<Vec<Vec<String>>>,
    pub(super) mics: RefCell<Vec<AudioDevice>>,
    pub(super) outs: RefCell<Vec<AudioDevice>>,
    pub(super) live: RefCell<Vec<HookedStream>>,
    ac_hw: RefCell<Vec<String>>,
    mix_outs: RefCell<Vec<String>>,
    /// Message to send when the confirm dialog is accepted.
    confirm: RefCell<Option<String>>,
    pub(super) installed: RefCell<Vec<InstalledApp>>,
    /// Channel whose apps picker is open.
    apps_for: RefCell<Option<String>>,
}

const ROUTE_MODES: [&str; 3] = ["auto", "manual", "desktop"];

/// The name an app is saved under: its executable (without path or arguments), else its name.
fn app_key(binary: &str, app_name: &str, name: &str) -> String {
    let bin = binary.split_whitespace().next().unwrap_or("").rsplit('/').next().unwrap_or("");
    let k = if !bin.is_empty() { bin } else if !app_name.is_empty() { app_name } else { name };
    k.trim().to_lowercase()
}

/// A mix column.
struct Col {
    key: String,
    name: String,
    sub: String,
    icon: String,
    color: String,
    custom: Option<String>,
}

fn cols(s: &StudioMicConfig) -> Vec<Col> {
    let b = |key: &str, name: &str, sub: &str, icon: &str, color: &str| Col {
        key: key.into(),
        name: name.into(),
        sub: sub.into(),
        icon: icon.into(),
        color: color.into(),
        custom: None,
    };
    let mut v = vec![
        b("monitor", "My Mix", "Monitor_Mix · headphones", "headphones", "#34c759"),
        b("stream", "Stream", "Stream_Mix · OBS Track 1", "sensors", "#c084fc"),
        b("vod", "Recording · VOD", "VOD_Mix · Track 2 (DMCA safe)", "movie", "#fbbf24"),
        b("voice", "Voice Chat", "Voice_Out · Discord", "forum", "#60a5fa"),
    ];
    v.extend(s.custom_mixes.iter().map(|m| Col {
        key: format!("x_{}", m.id),
        name: m.name.clone(),
        sub: format!("StudioMic_Mix_{}", m.id),
        icon: m.icon.clone(),
        color: m.color.clone(),
        custom: Some(m.id.clone()),
    }));
    v
}

/// A source row.
struct Row {
    id: String,
    name: String,
    kind: String,
    icon: String,
    comms: bool,
    mix: ChannelMix,
    hw_source: Option<String>,
    apps: Vec<String>,
    route_mode: String,
}

fn rows(s: &StudioMicConfig) -> Vec<Row> {
    let mut v = vec![Row {
        id: "mic".into(),
        name: "Microphone".into(),
        kind: "mic".into(),
        icon: "mic".into(),
        comms: false,
        mix: s.mic.mix.clone(),
        hw_source: s.active_hw_mic.clone(),
        apps: vec![],
        route_mode: String::new(),
    }];
    if s.dual_mic.enabled {
        v.push(Row {
            id: "mic2".into(),
            name: "Microphone 2".into(),
            kind: "mic2".into(),
            icon: "mic_external_on".into(),
            comms: false,
            mix: s.dual_mic.mix.clone(),
            hw_source: s.dual_mic.hw_mic.clone(),
            apps: vec![],
            route_mode: String::new(),
        });
    }
    v.extend(s.virtual_channels.iter().map(|c| Row {
        id: c.id.clone(),
        name: c.name.clone(),
        kind: c.kind.clone(),
        icon: c.icon.clone(),
        comms: c.is_comms(),
        mix: c.mix.clone(),
        hw_source: c.hw_source.clone(),
        apps: c.apps.clone(),
        route_mode: c.route_mode.clone(),
    }));
    v
}

/// What the engine does with (row, column): `mxCell`.
#[derive(Debug, Default, PartialEq)]
struct Cell {
    locked: bool,
    routed: bool,
    muted: bool,
    master_muted: bool,
    excluded: bool,
    gain: f32,
    editable: bool,
    removable: bool,
}

fn cell(r: &Row, key: &str) -> Cell {
    let m = &r.mix;
    let is_mic = r.kind == "mic";
    let master = (is_mic || r.kind == "mic2") && m.stream_muted;
    if let Some(mix) = key.strip_prefix("x_") {
        return match m.sends.get(mix) {
            Some(c) => Cell { routed: true, muted: c.muted || master, master_muted: master, gain: c.gain_db, editable: true, removable: true, ..Default::default() },
            None => Cell::default(),
        };
    }
    match key {
        "monitor" => Cell { routed: m.monitor_routed, muted: m.monitor_muted, gain: m.monitor_gain_db, editable: true, removable: true, ..Default::default() },
        "stream" => Cell { routed: m.stream_routed, muted: m.stream_muted, gain: m.stream_gain_db, editable: true, removable: true, ..Default::default() },
        "vod" => Cell { routed: true, muted: !m.vod || m.stream_muted, excluded: !m.vod, gain: m.stream_gain_db, ..Default::default() },
        "voice" if r.comms => Cell { locked: true, ..Default::default() },
        "voice" => Cell {
            routed: is_mic || m.voice_routed,
            muted: m.voice_muted || master,
            master_muted: master,
            gain: m.voice_gain_db,
            editable: true,
            removable: !is_mic,
            ..Default::default()
        },
        _ => Cell { locked: true, ..Default::default() },
    }
}

fn parse_color(hex: &str) -> slint::Color {
    let h = hex.trim_start_matches('#');
    let byte = |i: usize| h.get(i..i + 2).and_then(|x| u8::from_str_radix(x, 16).ok()).unwrap_or(0x88);
    if h.len() == 6 {
        slint::Color::from_rgb_u8(byte(0), byte(2), byte(4))
    } else {
        slint::Color::from_rgb_u8(0x22, 0xd3, 0xee)
    }
}

fn strings(v: &[String]) -> slint::ModelRc<SharedString> {
    Rc::new(VecModel::from(v.iter().map(SharedString::from).collect::<Vec<_>>())).into()
}

fn same_strings(m: &slint::ModelRc<SharedString>, v: &slint::ModelRc<SharedString>) -> bool {
    m.row_count() == v.row_count() && m.iter().zip(v.iter()).all(|(a, b)| a == b)
}

fn dev_label(d: &AudioDevice) -> String {
    if d.is_default {
        format!("{} [Default]", d.description)
    } else {
        d.description.clone()
    }
}

/// Hardware inputs a channel can use (`populateHardwareDevices`): no Studio Mic nodes, no monitors.
fn hw_inputs(mics: &[AudioDevice]) -> Vec<&AudioDevice> {
    let mut seen = std::collections::HashSet::new();
    mics.iter()
        .filter(|d| !d.name.is_empty() && !d.name.to_lowercase().starts_with("studiomic") && !d.name.contains(".monitor"))
        .filter(|d| seen.insert(d.name.clone()))
        .collect()
}

/// (labels, names, current, missing) for a device picker with an optional saved selection.
fn device_choice(devs: &[&AudioDevice], selected: Option<&str>) -> (Vec<String>, Vec<String>, i32, bool) {
    let mut labels = vec![];
    let mut names = vec![];
    let missing = selected.is_some_and(|sel| !sel.is_empty() && !devs.iter().any(|d| d.name == sel));
    if missing {
        let sel = selected.unwrap();
        labels.push(format!("Missing: {sel}"));
        names.push(sel.to_string());
    }
    for d in devs {
        labels.push(dev_label(d));
        names.push(d.name.clone());
    }
    let current = selected.and_then(|sel| names.iter().position(|n| n == sel)).map_or(-1, |i| i as i32);
    (labels, names, current, missing)
}

/// Everything the matrix shows, built from the saved state plus the device and app lists.
pub(super) struct Built {
    pub(super) cols: Vec<MxColData>,
    col_outputs: Vec<Vec<Option<String>>>,
    pub(super) rows: Vec<MxRowData>,
    row_hw: Vec<Vec<String>>,
    pub(super) cells: Vec<MxCellData>,
    keys: Vec<String>,
    ids: Vec<String>,
}

pub(super) fn build(s: &StudioMicConfig, mics: &[AudioDevice], outs: &[AudioDevice], live: &[HookedStream]) -> Built {
    let cols = cols(s);
    let rows = rows(s);

    // Columns, with the physical output picker of each mix.
    let mut col_outputs = vec![];
    let col_data: Vec<MxColData> = cols
        .iter()
        .map(|c| {
            let cur = match (c.key.as_str(), &c.custom) {
                ("monitor", _) => s.monitor_output.clone(),
                (_, Some(id)) => s.custom_mixes.iter().find(|m| &m.id == id).and_then(|m| m.output_device.clone()),
                (k, None) => s.mix_output_devices.get(k).cloned(),
            }
            .filter(|d| !d.is_empty());
            let first = if c.key == "monitor" { "Default output (follows system)" } else { "No output · capture in OBS / apps" };
            let mut labels = vec![first.to_string()];
            let mut names = vec![None];
            let missing = cur.as_ref().is_some_and(|cur| !outs.iter().any(|d| &d.name == cur));
            if missing {
                labels.push(format!("Missing: {}", cur.as_ref().unwrap()));
                names.push(cur.clone());
            }
            for d in outs.iter() {
                labels.push(dev_label(d));
                names.push(Some(d.name.clone()));
            }
            let output = names.iter().position(|n| *n == cur).unwrap_or(0) as i32;
            col_outputs.push(names);
            MxColData {
                key: c.key.as_str().into(),
                name: c.name.as_str().into(),
                sub: c.sub.as_str().into(),
                glyph: glyph(&c.icon),
                color: parse_color(&c.color),
                custom: c.custom.is_some(),
                outputs: strings(&labels),
                output,
                output_missing: missing,
            }
        })
        .collect();

    // Rows: source name, badge, hardware input picker or the apps routed there.
    let inputs = hw_inputs(mics);
    let mut row_hw = vec![];
    let row_data: Vec<MxRowData> = rows
        .iter()
        .map(|r| {
            let is_mic = r.kind == "mic" || r.kind == "mic2";
            let hw = is_mic || r.kind == "hardware";
            let (labels, names, current, missing) = if hw { device_choice(&inputs, r.hw_source.as_deref()) } else { Default::default() };
            row_hw.push(names);
            let live_bins: Vec<String> = live
                .iter()
                .filter(|x| x.channel.as_deref() == Some(r.id.as_str()))
                .map(|x| if x.binary.is_empty() { x.app_name.to_lowercase() } else { x.binary.to_lowercase() })
                .collect();
            let mut apps: Vec<AppTag> = r
                .apps
                .iter()
                .take(3)
                .map(|a| AppTag { name: a.as_str().into(), live: live_bins.iter().any(|b| b.contains(a.as_str())) })
                .collect();
            if r.apps.len() > 3 {
                apps.push(AppTag { name: format!("+{}", r.apps.len() - 3).into(), live: false });
            }
            let note = if hw || !r.apps.is_empty() {
                ""
            } else if r.route_mode == "desktop" {
                "Desktop audio"
            } else {
                "No apps"
            };
            MxRowData {
                id: r.id.as_str().into(),
                name: r.name.as_str().into(),
                glyph: glyph(&channel_icon(&r.id, &r.name, &r.icon)),
                badge: match r.kind.as_str() {
                    "mic" => "DSP",
                    "mic2" => "MIC 2",
                    "hardware" => "HW",
                    _ if r.comms => "CHAT",
                    _ => "APPS",
                }
                .into(),
                renamable: !is_mic,
                removable: !is_mic,
                hw,
                hw_options: strings(&labels),
                hw_current: current,
                hw_missing: missing,
                apps: Rc::new(VecModel::from(apps)).into(),
                note: note.into(),
            }
        })
        .collect();

    let cell_data: Vec<MxCellData> = rows
        .iter()
        .flat_map(|r| {
            cols.iter().map(move |c| {
                let x = cell(r, &c.key);
                let kind = if x.locked {
                    CellKind::Locked
                } else if x.routed {
                    CellKind::Routed
                } else {
                    CellKind::Empty
                };
                let tip = if c.key == "vod" {
                    if x.excluded {
                        "Excluded from the VOD track (DMCA safe). Click to include."
                    } else if x.muted {
                        "Muted: Stream Mute also mutes the VOD track"
                    } else {
                        "In the VOD track. Click to exclude (DMCA safe)."
                    }
                } else if x.master_muted {
                    "Microphone muted (its master mute silences every mix)"
                } else if x.muted {
                    "Muted. Click to unmute."
                } else {
                    "Click to mute"
                };
                MxCellData {
                    kind,
                    muted: x.muted,
                    gain: x.gain,
                    editable: x.editable,
                    removable: x.removable,
                    vod: c.key == "vod",
                    glyph: glyph(if x.muted {
                        "volume_off"
                    } else if c.key == "voice" {
                        "mic"
                    } else {
                        &c.icon
                    }),
                    tip: tip.into(),
                }
            })
        })
        .collect();

    let keys: Vec<String> = cols.iter().map(|c| c.key.clone()).collect();
    let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
    Built { cols: col_data, col_outputs, rows: row_data, row_hw, cells: cell_data, keys, ids }
}

impl Ctx {
    pub(super) fn init_matrix(&self) {
        let m = self.win.global::<Matrix>();
        m.set_cols(self.mx.cols.clone().into());
        m.set_rows(self.mx.rows.clone().into());
        m.set_cells(self.mx.cells.clone().into());
        m.set_meters(self.mx.meters.clone().into());
        m.set_mix_colors(Rc::new(VecModel::from(MIX_COLORS.iter().map(|c| parse_color(c)).collect::<Vec<_>>())).into());
    }

    /// Everything in the matrix that comes from the saved state (not the meters).
    pub(super) fn refresh_matrix(&self, s: &StudioMicConfig) {
        let m = self.win.global::<Matrix>();
        m.set_mon_muted(s.monitor_muted);
        m.set_mon_gain(s.monitor_gain_db);
        m.set_empty(s.virtual_channels.is_empty());

        let b = build(s, &self.mx.mics.borrow(), &self.mx.outs.borrow(), &self.mx.live.borrow());
        let (col_data, row_data, cell_data, keys, ids) = (b.cols, b.rows, b.cells, b.keys, b.ids);
        let (col_outputs, row_hw) = (b.col_outputs, b.row_hw);
        let structural = *self.mx.col_keys.borrow() != keys || *self.mx.row_ids.borrow() != ids;
        *self.mx.col_outputs.borrow_mut() = col_outputs;
        *self.mx.row_hw.borrow_mut() = row_hw;
        if structural {
            // Sources or mixes were added / removed: rebuild the grid.
            *self.mx.col_keys.borrow_mut() = keys;
            *self.mx.row_ids.borrow_mut() = ids;
            self.mx.meters.set_vec(vec![0.0; cell_data.len()]);
            self.mx.cells.set_vec(cell_data);
            self.mx.cols.set_vec(col_data);
            self.mx.rows.set_vec(row_data);
            return;
        }
        // Same grid: update in place, so a fader being dragged keeps its grab.
        for (i, c) in col_data.into_iter().enumerate() {
            let old = self.mx.cols.row_data(i).unwrap_or_default();
            let same = old.key == c.key
                && old.name == c.name
                && old.sub == c.sub
                && old.glyph == c.glyph
                && old.color == c.color
                && old.output == c.output
                && old.output_missing == c.output_missing
                && same_strings(&old.outputs, &c.outputs);
            if !same {
                self.mx.cols.set_row_data(i, c);
            }
        }
        for (i, r) in row_data.into_iter().enumerate() {
            let old = self.mx.rows.row_data(i).unwrap_or_default();
            let same = old.id == r.id
                && old.name == r.name
                && old.glyph == r.glyph
                && old.badge == r.badge
                && old.hw == r.hw
                && old.hw_current == r.hw_current
                && old.hw_missing == r.hw_missing
                && old.note == r.note
                && same_strings(&old.hw_options, &r.hw_options)
                && old.apps.row_count() == r.apps.row_count()
                && old.apps.iter().zip(r.apps.iter()).all(|(a, b)| a == b);
            if !same {
                self.mx.rows.set_row_data(i, r);
            }
        }
        for (i, c) in cell_data.into_iter().enumerate() {
            if self.mx.cells.row_data(i).as_ref() != Some(&c) {
                self.mx.cells.set_row_data(i, c);
            }
        }
    }

    /// Cell meters: what each source sends into each mix right now (`mxLevel`).
    pub(super) fn refresh_matrix_meters(&self, s: &StudioMicConfig) {
        let rows = rows(s);
        let cols = cols(s);
        if rows.len() * cols.len() != self.mx.meters.row_count() {
            return;
        }
        let any_solo = rows.iter().any(|r| r.mix.solo);
        let peaks = self.peaks.borrow();
        let get = |k: &str| peaks.get(k).copied().unwrap_or(-80.0);
        let duck = get("duck").max(0.0);
        let mut i = 0;
        for r in &rows {
            let pre = get(&r.id);
            for c in &cols {
                let x = cell(r, &c.key);
                let db = if x.locked || !x.routed || x.muted || pre <= -80.0 {
                    -80.0
                } else {
                    match c.key.as_str() {
                        "stream" => get(&format!("out:{}", r.id)),
                        "monitor" if any_solo && !r.mix.solo => -80.0,
                        "vod" => pre + x.gain - if r.mix.autoduck { duck } else { 0.0 },
                        _ => pre + x.gain,
                    }
                };
                let f = (meter_fraction(db) * 200.0).round() / 200.0;
                if self.mx.meters.row_data(i) != Some(f) {
                    self.mx.meters.set_row_data(i, f);
                }
                i += 1;
            }
        }
    }

    fn mx_target(&self, row: i32, col: i32) -> Option<(String, String)> {
        let id = self.mx.row_ids.borrow().get(row as usize)?.clone();
        let key = self.mx.col_keys.borrow().get(col as usize)?.clone();
        Some((id, key))
    }

    fn mx_cell_mute(&self, row: i32, col: i32) {
        let Some((id, key)) = self.mx_target(row, col) else { return };
        let s = self.controller.state.lock().unwrap().clone();
        let Some(m) = s.mix(&id) else { return };
        let onoff = |b: bool| if b { "on" } else { "off" };
        let master = (id == "mic" || id == "mic2") && m.stream_muted;
        let msg = if key == "vod" {
            format!("mix:{id}:vod:{}", onoff(!m.vod))
        } else if key == "stream" || (master && (key == "voice" || key.starts_with("x_"))) {
            format!("mix:{id}:stream_mute:{}", onoff(!m.stream_muted))
        } else if key == "monitor" {
            format!("mix:{id}:monitor_mute:{}", onoff(!m.monitor_muted))
        } else if key == "voice" {
            format!("mix:{id}:voice_mute:{}", onoff(!m.voice_muted))
        } else if let Some(mix) = key.strip_prefix("x_") {
            let muted = m.sends.get(mix).is_some_and(|c| c.muted);
            format!("mix:{id}:send_mute.{mix}:{}", onoff(!muted))
        } else {
            return;
        };
        self.post(msg);
    }

    fn mx_cell_route(&self, row: i32, col: i32, on: bool) {
        let Some((id, key)) = self.mx_target(row, col) else { return };
        let v = if on { "on" } else { "off" };
        self.post(match key.strip_prefix("x_") {
            Some(mix) => format!("mix:{id}:send_route.{mix}:{v}"),
            None => format!("mix:{id}:{key}_route:{v}"),
        });
    }

    fn mx_cell_gain(&self, row: i32, col: i32, v: f32) {
        let Some((id, key)) = self.mx_target(row, col) else { return };
        let msg = match key.strip_prefix("x_") {
            Some(mix) => format!("mix:{id}:send_gain.{mix}:={v}"),
            None if matches!(key.as_str(), "stream" | "monitor" | "voice") => format!("mix:{id}:{key}_gain:={v}"),
            None => return,
        };
        if key == "stream" {
            // The VOD cell follows the Stream fader: mirror it while dragging.
            if let Some(j) = self.mx.col_keys.borrow().iter().position(|k| k == "vod") {
                let i = row as usize * self.mx.col_keys.borrow().len() + j;
                if let Some(mut c) = self.mx.cells.row_data(i) {
                    c.gain = v;
                    self.mx.cells.set_row_data(i, c);
                }
            }
        }
        self.throttled(format!("gain:{id}:{key}"), msg);
    }

    fn mx_set_output(&self, col: i32, idx: i32) {
        let Some(key) = self.mx.col_keys.borrow().get(col as usize).cloned() else { return };
        let Some(dev) = self.mx.col_outputs.borrow().get(col as usize).and_then(|o| o.get(idx as usize).cloned()) else { return };
        let dev = dev.unwrap_or_default();
        self.post(match (key.as_str(), key.strip_prefix("x_")) {
            ("monitor", _) => format!("monitor_output:{}", if dev.is_empty() { "default" } else { &dev }),
            (_, Some(mix)) => format!("mix_output:{mix}:{dev}"),
            (k, None) => format!("mix_output:{k}:{dev}"),
        });
    }

    fn mx_set_hw(&self, row: i32, idx: i32) {
        let Some(id) = self.mx.row_ids.borrow().get(row as usize).cloned() else { return };
        let Some(node) = self.mx.row_hw.borrow().get(row as usize).and_then(|o| o.get(idx as usize).cloned()) else { return };
        self.post(match id.as_str() {
            "mic" => format!("hardware_mic:{node}"),
            "mic2" => format!("hardware_mic2:{node}"),
            _ => format!("channel_hw:{id}:{node}"),
        });
    }

    /// Confirm dialog for other tabs; `msg` is a backend message, or a `ui:` local action.
    pub(super) fn ask(&self, text: &str, ok: &str, msg: &str) {
        self.mx_confirm(text.to_string(), ok, msg.to_string());
    }

    fn mx_confirm(&self, text: String, ok: &str, msg: String) {
        *self.mx.confirm.borrow_mut() = Some(msg);
        let m = self.win.global::<Matrix>();
        m.set_confirm_ok(ok.into());
        m.set_confirm_text(text.into());
    }

    fn mx_open_add_channel(&self) {
        let s = self.controller.state.lock().unwrap().clone();
        let exists = |p: &str| s.virtual_channels.iter().any(|c| c.id == p) || (p == "voice" && s.virtual_channels.iter().any(|c| c.is_comms()));
        let presets: Vec<PresetData> = PRESETS
            .iter()
            .map(|(p, icon, name, _, sub, detail)| PresetData {
                glyph: glyph(icon),
                name: (*name).into(),
                sub: (*sub).into(),
                taken: detail.is_empty() && exists(p),
                detail: (*detail).into(),
            })
            .collect();
        let mics = self.mx.mics.borrow();
        let inputs = hw_inputs(&mics);
        let (labels, names, _, _) = device_choice(&inputs, None);
        *self.mx.ac_hw.borrow_mut() = names;
        let m = self.win.global::<Matrix>();
        m.set_presets(Rc::new(VecModel::from(presets)).into());
        m.set_hw_options(strings(&labels));
        m.set_ac_pick(-1);
        m.set_ac_hw(-1);
        m.set_ac_name(SharedString::new());
        m.set_add_channel_open(true);
    }

    fn mx_pick_preset(self: &Rc<Self>, i: i32) {
        let Some(&(p, _, name, def, _, detail)) = PRESETS.get(i as usize) else { return };
        let m = self.win.global::<Matrix>();
        if detail.is_empty() {
            self.post(format!("channel_add:{}", serde_json::json!({ "name": def, "preset": p })));
            m.set_add_channel_open(false);
            self.notice(&format!("Adding {name}..."));
            return;
        }
        m.set_ac_pick(i);
        m.set_ac_name(def.into());
        m.set_ac_hw(-1);
    }

    fn mx_commit_add_channel(&self) {
        let m = self.win.global::<Matrix>();
        let Some(&(p, _, _, _, _, detail)) = PRESETS.get(m.get_ac_pick() as usize) else { return };
        let name = m.get_ac_name().trim().to_string();
        if name.is_empty() {
            return;
        }
        let mut msg = serde_json::json!({ "name": name, "preset": p });
        if detail == "hw" {
            let Some(hw) = self.mx.ac_hw.borrow().get(m.get_ac_hw() as usize).cloned() else { return };
            msg["hw_source"] = hw.into();
        }
        self.post(format!("channel_add:{msg}"));
        m.set_add_channel_open(false);
    }

    /// Fills the apps picker for the channel it's open for (no-op when closed).
    pub(super) fn refresh_apps(&self) {
        let Some(id) = self.mx.apps_for.borrow().clone() else { return };
        let s = self.controller.state.lock().unwrap().clone();
        let Some(ch) = s.channel(&id) else {
            self.win.global::<Matrix>().set_apps_open(false);
            return;
        };
        let m = self.win.global::<Matrix>();
        m.set_apps_channel(ch.name.as_str().into());
        m.set_apps_mode(ROUTE_MODES.iter().position(|x| *x == ch.route_mode).unwrap_or(0) as i32);
        m.set_apps_current(strings(&ch.apps));
        let here = |k: &str| ch.apps.iter().any(|a| a == k);
        let channel_name = |cid: &Option<String>| match cid {
            Some(c) if c == &id => "Playing in this channel".to_string(),
            Some(c) => format!("Playing in {}", s.channel(c).map(|x| x.name.as_str()).unwrap_or(c)),
            None => "Playing on your default output".to_string(),
        };
        let mut seen = std::collections::HashSet::new();
        let live: Vec<AppItem> = self
            .mx
            .live
            .borrow()
            .iter()
            .filter_map(|x| {
                let key = app_key(&x.binary, &x.app_name, &x.name);
                if key.is_empty() || !seen.insert(key.clone()) {
                    return None;
                }
                let label = if x.app_name.is_empty() { key.clone() } else { x.app_name.clone() };
                Some(AppItem { here: here(&key), key: key.into(), label: label.into(), sub: channel_name(&x.channel).into() })
            })
            .collect();
        m.set_apps_live(Rc::new(VecModel::from(live)).into());
        let q = m.get_apps_filter().to_lowercase();
        let installed = self.mx.installed.borrow();
        m.set_apps_installed_total(installed.len() as i32);
        let mut list: Vec<&InstalledApp> = installed
            .iter()
            .filter(|a| q.is_empty() || a.name.to_lowercase().contains(&q) || a.app_name.to_lowercase().contains(&q) || a.binary.to_lowercase().contains(&q))
            .collect();
        list.sort_by_key(|a| a.name.to_lowercase());
        let items: Vec<AppItem> = list
            .into_iter()
            .take(100)
            .map(|a| {
                let key = app_key(&a.binary, &a.app_name, &a.name);
                let elsewhere = s.virtual_channels.iter().find(|c| c.id != id && c.apps.iter().any(|x| *x == key));
                AppItem {
                    here: here(&key),
                    label: a.name.as_str().into(),
                    sub: elsewhere.map(|c| format!("In {}", c.name)).unwrap_or_default().into(),
                    key: key.into(),
                }
            })
            .collect();
        m.set_apps_installed(Rc::new(VecModel::from(items)).into());
    }

    fn mx_open_apps(&self, row: i32) {
        let Some(id) = self.mx.row_ids.borrow().get(row as usize).cloned() else { return };
        *self.mx.apps_for.borrow_mut() = Some(id);
        let m = self.win.global::<Matrix>();
        m.set_apps_filter(SharedString::new());
        self.refresh_apps();
        m.set_apps_open(true);
        // Fresh list of what's playing.
        self.post("get_live_streams");
    }

    /// Saves this channel's app list / mode.
    fn mx_set_apps(&self, f: impl FnOnce(&mut Vec<String>, &mut String)) {
        let Some(id) = self.mx.apps_for.borrow().clone() else { return };
        let Some(ch) = self.controller.state.lock().unwrap().channel(&id).cloned() else { return };
        let (mut apps, mut mode) = (ch.apps, ch.route_mode);
        f(&mut apps, &mut mode);
        self.post(format!("channel_apps:{}", serde_json::json!({ "id": id, "apps": apps, "mode": mode })));
    }

    fn mx_open_add_mix(&self) {
        let outs = self.mx.outs.borrow();
        let mut labels = vec!["No output · capture in OBS / apps".to_string()];
        let mut names = vec![String::new()];
        for d in outs.iter() {
            labels.push(d.description.clone());
            names.push(d.name.clone());
        }
        *self.mx.mix_outs.borrow_mut() = names;
        let m = self.win.global::<Matrix>();
        m.set_mix_outputs(strings(&labels));
        m.set_mix_output(0);
        m.set_mix_color(0);
        m.set_mix_name("Guest Mix".into());
        m.set_add_mix_open(true);
    }

    fn mx_commit_add_mix(self: &Rc<Self>) {
        let m = self.win.global::<Matrix>();
        let name = m.get_mix_name().trim().to_string();
        if name.is_empty() {
            return;
        }
        let color = MIX_COLORS.get(m.get_mix_color() as usize).unwrap_or(&MIX_COLORS[0]);
        let output = self.mx.mix_outs.borrow().get(m.get_mix_output() as usize).cloned().unwrap_or_default();
        self.post(format!("mix_add:{}", serde_json::json!({ "name": name, "color": color, "output": output })));
        m.set_add_mix_open(false);
        self.notice(&format!("Creating mix \"{name}\" (a new PipeWire bus)..."));
    }
}

pub(super) fn wire(ctx: &Rc<Ctx>) {
    let m = ctx.win.global::<Matrix>();
    macro_rules! cb {
        (|$c:ident $(, $a:ident)*| $body:expr) => {{
            let w = Rc::downgrade(ctx);
            move |$($a),*| {
                if let Some($c) = w.upgrade() {
                    $body;
                }
            }
        }};
    }
    m.on_cell_mute(cb!(|c, i, j| c.mx_cell_mute(i, j)));
    m.on_cell_route(cb!(|c, i, j, on| c.mx_cell_route(i, j, on)));
    m.on_cell_gain(cb!(|c, i, j, v| c.mx_cell_gain(i, j, v)));
    m.on_set_output(cb!(|c, j, idx| c.mx_set_output(j, idx)));
    m.on_set_hw(cb!(|c, i, idx| c.mx_set_hw(i, idx)));
    m.on_rename(cb!(|c, i, name| {
        let v = name.trim().to_string();
        let row = c.mx.rows.row_data(i as usize);
        if let Some(row) = row {
            if !v.is_empty() && v != row.name.as_str() {
                c.post(format!("channel_rename:{}:{}", row.id, v));
            }
        }
    }));
    m.on_remove_channel(cb!(|c, i| {
        if let Some(row) = c.mx.rows.row_data(i as usize) {
            c.mx_confirm(
                format!("Remove \"{}\"? Its apps go back to your default output.", row.name),
                "Remove",
                format!("channel_remove:{}", row.id),
            );
        }
    }));
    m.on_remove_mix(cb!(|c, j| {
        let key = c.mx.col_keys.borrow().get(j as usize).cloned();
        if let (Some(col), Some(id)) = (c.mx.cols.row_data(j as usize), key.as_deref().and_then(|k| k.strip_prefix("x_"))) {
            c.mx_confirm(
                format!("Delete the mix \"{}\"? Its bus StudioMic_Mix_{id} disappears from OBS and other apps.", col.name),
                "Delete",
                format!("mix_remove:{id}"),
            );
        }
    }));
    m.on_master_mute(cb!(|c| c.win.global::<Strip>().invoke_mon_mute()));
    m.on_open_add_channel(cb!(|c| c.mx_open_add_channel()));
    m.on_pick_preset(cb!(|c, i| c.mx_pick_preset(i)));
    m.on_commit_add_channel(cb!(|c| c.mx_commit_add_channel()));
    m.on_open_add_mix(cb!(|c| c.mx_open_add_mix()));
    m.on_commit_add_mix(cb!(|c| c.mx_commit_add_mix()));
    m.on_confirm_accepted(cb!(|c| {
        let msg = c.mx.confirm.borrow_mut().take();
        match msg {
            Some(m) if m.starts_with("ui:") => c.local_action(&m),
            Some(m) => c.post(m),
            None => {}
        }
    }));
    m.on_open_apps(cb!(|c, i| c.mx_open_apps(i)));
    m.on_app_add(cb!(|c, key| {
        let id = c.mx.apps_for.borrow().clone();
        if let Some(id) = id {
            c.post(format!("assign_app:{key}:{id}"));
        }
    }));
    m.on_app_remove(cb!(|c, app| c.mx_set_apps(|apps, _| apps.retain(|a| a.as_str() != app.as_str()))));
    m.on_apps_set_mode(cb!(|c, i| c.mx_set_apps(|_, mode| *mode = ROUTE_MODES.get(i as usize).unwrap_or(&"auto").to_string())));
    m.on_apps_filter_changed(cb!(|c, _q| c.refresh_apps()));
    m.on_rescan(cb!(|c| {
        c.post("rescan_devices");
        c.notice("Rescanning audio devices...");
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, id: &str) -> Row {
        Row {
            id: id.into(),
            name: id.into(),
            kind: kind.into(),
            icon: String::new(),
            comms: kind == "comms",
            mix: ChannelMix::default(),
            hw_source: None,
            apps: vec![],
            route_mode: "auto".into(),
        }
    }

    #[test]
    fn matrix_cells_follow_the_routing_rules() {
        // Echo guard: the voice-chat channel can't feed Voice Chat.
        assert!(cell(&row("comms", "voice"), "voice").locked);
        // The main mic always feeds Voice Chat and can't be removed from it.
        let mic = cell(&row("mic", "mic"), "voice");
        assert!(mic.routed && !mic.removable);
        // App channels start outside Voice Chat (opt-in).
        assert!(!cell(&row("virtual", "game"), "voice").routed);
        // A mic's Stream Mute is its master mute: it silences Voice Chat and custom mixes too.
        let mut r = row("mic", "mic");
        r.mix.stream_muted = true;
        r.mix.sends.insert("guest".into(), Default::default());
        assert!(cell(&r, "voice").muted && cell(&r, "x_guest").muted);
        // VOD follows the Stream fader and is excluded by the vod toggle.
        let mut r = row("virtual", "music");
        r.mix.stream_gain_db = -6.0;
        r.mix.vod = false;
        let v = cell(&r, "vod");
        assert!(v.excluded && v.muted && v.gain == -6.0 && !v.editable);
        // Not in a custom mix until it's routed there.
        assert!(!cell(&r, "x_guest").routed);
    }

    #[test]
    fn app_keys_are_executable_names() {
        assert_eq!(app_key("/usr/bin/firefox %u", "Firefox", "x"), "firefox");
        assert_eq!(app_key("", "Spotify", "spotify-client"), "spotify");
        assert_eq!(app_key("", "", "VLC"), "vlc");
    }

    #[test]
    fn device_choice_keeps_a_missing_saved_device() {
        let d = |n: &str| AudioDevice { id: 1, name: n.into(), description: n.to_uppercase(), is_default: false, ports: vec![], alsa_card: None };
        let (a, b) = (d("alsa_input.a"), d("alsa_input.b"));
        let devs = vec![&a, &b];
        let (labels, names, cur, missing) = device_choice(&devs, Some("alsa_input.gone"));
        assert!(missing && cur == 0 && labels[0] == "Missing: alsa_input.gone" && names.len() == 3);
        let (_, _, cur, missing) = device_choice(&devs, Some("alsa_input.b"));
        assert!(!missing && cur == 1);
        assert_eq!(device_choice(&devs, None).2, -1);
        assert_eq!(parse_color("#34c759"), slint::Color::from_rgb_u8(0x34, 0xc7, 0x59));
    }
}
