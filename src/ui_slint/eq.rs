//! Tab 2 EQ: bands of the main mic or Mic 2, the response curve, and the same messages as the
//! HTML UI (`eq:` / `eq_mic2:` with `[{type, freq, gain, q}]`, `profile:`, `save_default_eq:`).
//!
//! While the user edits, the local band list is the truth (the backend echo arrives later and
//! would make points jump); 600 ms after the last edit the window follows the saved state again,
//! shown sorted by frequency (spec: bands are ordered by frequency).

use super::*;
use crate::models::microphone::{EqBand, FilterType, MicDatabase, MicrophoneProfile};

const TYPES: [FilterType; 6] = [FilterType::Peaking, FilterType::LowShelf, FilterType::HighShelf, FilterType::HighPass, FilterType::LowPass, FilterType::Notch];
const TYPE_NAMES: [&str; 6] = ["Peaking", "Low Shelf", "High Shelf", "High Pass", "Low Pass", "Notch"];
const MAX_BANDS: usize = 10;
const GAIN_RANGE: f32 = 18.0;
const EDIT_HOLD: Duration = Duration::from_millis(600);

/// Style presets (same curves as the HTML UI's community presets).
const STYLES: [(&str, &[(FilterType, f32, f32, f32)]); 3] = {
    use FilterType::*;
    [
        ("Warm Radio Host Depth", &[
            (HighPass, 70.0, 0.0, 0.7), (Peaking, 120.0, 2.0, 1.1), (Peaking, 180.0, 3.5, 1.0), (Peaking, 320.0, -1.2, 1.2),
            (Peaking, 400.0, -2.0, 1.4), (Peaking, 1000.0, 0.5, 1.0), (Peaking, 2800.0, 1.5, 1.2), (Peaking, 4500.0, 2.5, 1.3),
            (HighShelf, 10000.0, 1.5, 0.8), (HighShelf, 14000.0, 1.0, 0.9),
        ]),
        ("Live Streamer Pro (Music Mix)", &[
            (HighPass, 85.0, 0.0, 0.7), (Peaking, 140.0, 1.0, 1.1), (Peaking, 250.0, -1.5, 1.2), (Peaking, 350.0, -2.5, 1.4),
            (Peaking, 800.0, -1.0, 1.0), (Peaking, 1500.0, 1.2, 1.1), (Peaking, 2800.0, 2.5, 1.3), (Peaking, 3600.0, 3.8, 1.5),
            (Peaking, 6500.0, 2.0, 1.2), (HighShelf, 11000.0, 2.0, 0.8),
        ]),
        ("Clean Voiceover / Podcast", &[
            (HighPass, 80.0, 0.0, 0.7), (Peaking, 150.0, 0.5, 1.0), (Peaking, 250.0, -1.5, 1.0), (Peaking, 450.0, -2.0, 1.3),
            (Peaking, 1000.0, 0.5, 1.0), (Peaking, 1800.0, 1.0, 1.2), (Peaking, 3000.0, 1.5, 1.2), (Peaking, 5000.0, 1.8, 1.2),
            (Peaking, 8000.0, 1.2, 1.0), (HighShelf, 12000.0, 1.0, 0.8),
        ]),
    ]
};

static PROFILES: LazyLock<Vec<MicrophoneProfile>> = LazyLock::new(MicDatabase::get_all);

/// "Shure SM7B"; the database sometimes repeats the brand in the model name.
fn mic_name(m: &MicrophoneProfile) -> String {
    if m.model.to_lowercase().starts_with(&m.brand.to_lowercase()) {
        m.model.clone()
    } else {
        format!("{} {}", m.brand, m.model)
    }
}

#[derive(Default)]
pub(super) struct EqState {
    bands: RefCell<Vec<EqBand>>,
    edited: Cell<Option<Instant>>,
    model: Rc<VecModel<EqBandData>>,
    mic2: Cell<bool>,
    graph: Cell<(f64, f64)>,
}

fn type_index(t: FilterType) -> i32 {
    TYPES.iter().position(|x| *x == t).unwrap_or(0) as i32
}

fn has_gain(t: FilterType) -> bool {
    matches!(t, FilterType::Peaking | FilterType::LowShelf | FilterType::HighShelf)
}

/// Magnitude (dB) of one band at `f`, RBJ cookbook biquads at 48 kHz (what PipeWire's bq_* use).
fn band_db(b: &EqBand, f: f64) -> f64 {
    use std::f64::consts::PI;
    let fs = 48_000.0;
    let f0 = (b.freq as f64).clamp(10.0, fs * 0.49);
    let q = (b.q as f64).max(0.05);
    let w0 = 2.0 * PI * f0 / fs;
    let (cs, sn) = (w0.cos(), w0.sin());
    let alpha = sn / (2.0 * q);
    let a = 10f64.powf(b.gain_db as f64 / 40.0);
    let sa = 2.0 * a.sqrt() * alpha;
    let (b0, b1, b2, a0, a1, a2) = match b.filter_type {
        FilterType::Peaking => (1.0 + alpha * a, -2.0 * cs, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cs, 1.0 - alpha / a),
        FilterType::LowShelf => (
            a * ((a + 1.0) - (a - 1.0) * cs + sa),
            2.0 * a * ((a - 1.0) - (a + 1.0) * cs),
            a * ((a + 1.0) - (a - 1.0) * cs - sa),
            (a + 1.0) + (a - 1.0) * cs + sa,
            -2.0 * ((a - 1.0) + (a + 1.0) * cs),
            (a + 1.0) + (a - 1.0) * cs - sa,
        ),
        FilterType::HighShelf => (
            a * ((a + 1.0) + (a - 1.0) * cs + sa),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * cs),
            a * ((a + 1.0) + (a - 1.0) * cs - sa),
            (a + 1.0) - (a - 1.0) * cs + sa,
            2.0 * ((a - 1.0) - (a + 1.0) * cs),
            (a + 1.0) - (a - 1.0) * cs - sa,
        ),
        FilterType::HighPass => ((1.0 + cs) / 2.0, -(1.0 + cs), (1.0 + cs) / 2.0, 1.0 + alpha, -2.0 * cs, 1.0 - alpha),
        FilterType::LowPass => ((1.0 - cs) / 2.0, 1.0 - cs, (1.0 - cs) / 2.0, 1.0 + alpha, -2.0 * cs, 1.0 - alpha),
        FilterType::Notch => (1.0, -2.0 * cs, 1.0, 1.0 + alpha, -2.0 * cs, 1.0 - alpha),
    };
    let w = 2.0 * PI * f / fs;
    let mag2 = |c0: f64, c1: f64, c2: f64| {
        let re = c0 + c1 * w.cos() + c2 * (2.0 * w).cos();
        let im = -(c1 * w.sin() + c2 * (2.0 * w).sin());
        re * re + im * im
    };
    10.0 * (mag2(b0, b1, b2) / mag2(a0, a1, a2)).max(1e-12).log10()
}

/// Curve and filled area as SVG path commands in a `w` x `h` box (20 Hz - 20 kHz, ±18 dB).
fn curve(bands: &[EqBand], w: f64, h: f64) -> (String, String) {
    const N: usize = 240;
    let mut line = String::with_capacity(N * 16);
    for i in 0..N {
        let t = i as f64 / (N - 1) as f64;
        let f = 20.0 * 1000f64.powf(t);
        let db: f64 = bands.iter().map(|b| band_db(b, f)).sum();
        let y = ((0.5 - db / (2.0 * GAIN_RANGE as f64)) * h).clamp(0.0, h);
        line.push_str(&format!("{}{:.1} {:.1} ", if i == 0 { "M " } else { "L " }, t * w, y));
    }
    let fill = format!("{line}L {w:.1} {:.1} L 0 {:.1} Z", h / 2.0, h / 2.0);
    (line, fill)
}

fn band_data(b: &EqBand) -> EqBandData {
    let g = has_gain(b.filter_type);
    EqBandData {
        kind: type_index(b.filter_type),
        freq: b.freq,
        gain: b.gain_db,
        q: b.q,
        x: ((b.freq.max(20.0) / 20.0).log10() / 3.0).clamp(0.0, 1.0),
        y: if g { (0.5 - b.gain_db / (2.0 * GAIN_RANGE)).clamp(0.0, 1.0) } else { 0.5 },
        has_gain: g,
    }
}

fn sort_bands(b: &mut [EqBand]) {
    b.sort_by(|x, y| x.freq.total_cmp(&y.freq));
}

/// Frequency for a new band: the middle (on the log scale) of the widest free gap.
fn new_band_freq(bands: &[EqBand]) -> f32 {
    let mut f: Vec<f32> = bands.iter().map(|b| b.freq.clamp(20.0, 20_000.0).log10()).collect();
    f.push(20f32.log10());
    f.push(20_000f32.log10());
    f.sort_by(f32::total_cmp);
    let (lo, hi) = f.windows(2).map(|w| (w[0], w[1])).max_by(|a, b| (a.1 - a.0).total_cmp(&(b.1 - b.0))).unwrap_or((3.0, 3.0));
    (10f32.powf((lo + hi) / 2.0) / 5.0).round() * 5.0
}

fn payload(bands: &[EqBand]) -> serde_json::Value {
    bands
        .iter()
        .map(|b| serde_json::json!({ "type": TYPE_NAMES[type_index(b.filter_type) as usize], "freq": b.freq, "gain": b.gain_db, "q": b.q }))
        .collect()
}

impl Ctx {
    pub(super) fn init_eq(&self) {
        let eq = self.win.global::<Eq>();
        eq.set_bands(self.eq.model.clone().into());
        let names: Vec<String> = PROFILES.iter().map(mic_name).collect();
        eq.set_profiles(Rc::new(VecModel::from(names.iter().map(SharedString::from).collect::<Vec<_>>())).into());
    }

    pub(super) fn refresh_eq(&self, s: &StudioMicConfig) {
        let eq = self.win.global::<Eq>();
        if !s.dual_mic.enabled {
            self.eq.mic2.set(false);
        }
        let mic2 = self.eq.mic2.get();
        eq.set_mic2_enabled(s.dual_mic.enabled);
        eq.set_target_mic2(mic2);
        let (id, preset) = if mic2 { (&s.dual_mic.mic_id, &s.dual_mic.preset_name) } else { (&s.default_mic_id, &s.preset_name) };
        let idx = PROFILES.iter().position(|m| &m.id == id);
        eq.set_profile(idx.map_or(-1, |i| i as i32));
        eq.set_preset_name(preset.as_str().into());
        let p = idx.map(|i| &PROFILES[i]);
        eq.set_profile_title(p.map(|p| p.profile_title.as_str()).unwrap_or("").into());
        eq.set_profile_summary(p.map(|p| p.profile_summary.as_str()).unwrap_or("").into());

        let titles: Vec<String> = s.user_presets.iter().map(|p| p.title.clone()).collect();
        eq.set_my_presets(Rc::new(VecModel::from(titles.iter().map(SharedString::from).collect::<Vec<_>>())).into());
        let in_use = if mic2 { None } else { titles.iter().position(|t| s.preset_name == format!("Custom: {t}")) };
        eq.set_my_preset(in_use.map_or(-1, |i| i as i32));

        if self.eq.edited.get().is_some_and(|t| t.elapsed() < EDIT_HOLD) {
            return;
        }
        let mut bands = if mic2 { s.dual_mic.eq_bands.clone() } else { s.eq_bands.clone() };
        sort_bands(&mut bands);
        *self.eq.bands.borrow_mut() = bands;
        self.publish_eq();
    }

    /// Local bands → graph points, band rows and the curve.
    fn publish_eq(&self) {
        let bands = self.eq.bands.borrow();
        let data: Vec<EqBandData> = bands.iter().map(band_data).collect();
        if self.eq.model.row_count() == data.len() {
            for (i, d) in data.into_iter().enumerate() {
                if self.eq.model.row_data(i).as_ref() != Some(&d) {
                    self.eq.model.set_row_data(i, d);
                }
            }
        } else {
            self.eq.model.set_vec(data);
        }
        let (w, h) = self.eq.graph.get();
        let (line, fill) = curve(&bands, w.max(1.0), h.max(1.0));
        let eq = self.win.global::<Eq>();
        eq.set_curve(line.into());
        eq.set_curve_fill(fill.into());
    }

    /// Applies a local edit, redraws and sends it (throttled while dragging).
    fn eq_edit(&self, f: impl FnOnce(&mut Vec<EqBand>)) {
        f(&mut self.eq.bands.borrow_mut());
        self.eq.edited.set(Some(Instant::now()));
        self.publish_eq();
        let prefix = if self.eq.mic2.get() { "eq_mic2" } else { "eq" };
        let msg = format!("{prefix}:{}", payload(&self.eq.bands.borrow()));
        self.throttled("eq", msg);
    }

    fn eq_band(&self, i: i32, f: impl FnOnce(&mut EqBand)) {
        self.eq_edit(|b| {
            if let Some(b) = b.get_mut(i as usize) {
                f(b);
            }
        });
    }

    /// Replaces the whole curve with a named preset (main mic: saved as its preset name).
    fn eq_replace(&self, bands: Vec<EqBand>, name: &str) {
        self.eq.edited.set(Some(Instant::now()));
        *self.eq.bands.borrow_mut() = bands.clone();
        self.publish_eq();
        if self.eq.mic2.get() {
            self.post(format!("eq_mic2:{}", payload(&bands)));
        } else {
            self.post(format!("eq:{}", serde_json::json!({ "bands": payload(&bands), "preset_name": name })));
        }
    }
}

pub(super) fn wire(ctx: &Rc<Ctx>) {
    let eq = ctx.win.global::<Eq>();
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
    eq.on_graph_resized(cb!(|c, w, h| {
        c.eq.graph.set((w as f64, h as f64));
        c.publish_eq();
    }));
    eq.on_set_target(cb!(|c, mic2| {
        c.eq.mic2.set(mic2);
        c.eq.edited.set(None);
        c.refresh_state();
    }));
    eq.on_pick_profile(cb!(|c, i| {
        if let Some(m) = PROFILES.get(i as usize) {
            c.eq.edited.set(None);
            c.post(format!("{}:{}", if c.eq.mic2.get() { "profile_mic2" } else { "profile" }, m.id));
        }
    }));
    eq.on_style(cb!(|c, i| {
        if let Some((name, bands)) = STYLES.get(i as usize) {
            let bands = bands.iter().map(|&(t, f, g, q)| EqBand { freq: f, gain_db: g, q, filter_type: t }).collect();
            c.eq_replace(bands, name);
        }
    }));
    eq.on_band_drag(cb!(|c, i, x, y| {
        c.eq_band(i, |b| {
            b.freq = ((20.0 * 1000f32.powf(x.clamp(0.0, 1.0))) * 10.0).round() / 10.0;
            if has_gain(b.filter_type) {
                b.gain_db = (((0.5 - y) * 2.0 * GAIN_RANGE).clamp(-GAIN_RANGE, GAIN_RANGE) * 10.0).round() / 10.0;
            }
        })
    }));
    eq.on_band_released(cb!(|c, _i| c.eq_edit(|b| sort_bands(b))));
    eq.on_band_q_step(cb!(|c, i, k| c.eq_band(i, |b| b.q = ((b.q * k).clamp(0.1, 10.0) * 100.0).round() / 100.0)));
    eq.on_band_type(cb!(|c, i, k| c.eq_band(i, |b| b.filter_type = TYPES.get(k as usize).copied().unwrap_or(FilterType::Peaking))));
    eq.on_band_freq(cb!(|c, i, v| c.eq_band(i, |b| b.freq = (10f32.powf(v) * 10.0).round() / 10.0)));
    eq.on_band_gain(cb!(|c, i, v| c.eq_band(i, |b| b.gain_db = v)));
    eq.on_band_q(cb!(|c, i, v| c.eq_band(i, |b| b.q = v)));
    eq.on_add_band(cb!(|c| c.eq_edit(|b| {
        if b.len() < MAX_BANDS {
            b.push(EqBand { freq: new_band_freq(b), gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking });
            sort_bands(b);
        }
    })));
    eq.on_remove_band(cb!(|c, i| c.eq_edit(|b| {
        if b.len() > 1 && (i as usize) < b.len() {
            b.remove(i as usize);
        }
    })));
    eq.on_flat(cb!(|c| c.eq_replace(vec![EqBand { freq: 1000.0, gain_db: 0.0, q: 1.0, filter_type: FilterType::Peaking }], "Flat")));
    eq.on_apply_my_preset(cb!(|c, i| {
        let p = c.controller.state.lock().unwrap().user_presets.get(i as usize).cloned();
        if let Some(p) = p {
            c.eq_replace(p.bands.clone(), &format!("Custom: {}", p.title));
        }
    }));
    eq.on_save_my_preset(cb!(|c| {
        let eq = c.win.global::<Eq>();
        let title = eq.get_my_preset_name().trim().to_string();
        if title.is_empty() || c.eq.mic2.get() {
            return;
        }
        let id = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        let mic = c.controller.state.lock().unwrap().default_mic_id.clone();
        let preset = serde_json::json!({ "id": id, "title": title, "mic": mic, "date": "Saved preset", "bands": payload(&c.eq.bands.borrow()) });
        c.post(format!("save_user_preset:{preset}"));
        eq.set_my_preset_name(SharedString::new());
        c.notice(&format!("Saved EQ preset \"{title}\""));
    }));
    eq.on_delete_my_preset(cb!(|c, i| {
        let p = c.controller.state.lock().unwrap().user_presets.get(i as usize).cloned();
        if let Some(p) = p {
            c.ask(&format!("Delete the EQ preset \"{}\"? The current curve stays as it is.", p.title), "Delete", &format!("delete_user_preset:{}", p.id));
        }
    }));
    eq.on_factory(cb!(|c| {
        c.eq.edited.set(None);
        c.post("reset_default_eq");
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(t: FilterType, f: f32, g: f32, q: f32) -> EqBand {
        EqBand { freq: f, gain_db: g, q, filter_type: t }
    }

    #[test]
    fn biquad_response_matches_the_band() {
        // A peak reaches its gain at its frequency and is ~flat far away.
        let p = b(FilterType::Peaking, 1000.0, 6.0, 1.0);
        assert!((band_db(&p, 1000.0) - 6.0).abs() < 0.05);
        assert!(band_db(&p, 50.0).abs() < 0.2);
        // High-pass: -3 dB at the corner (Q 0.707), deep cut an octave+ below.
        let hp = b(FilterType::HighPass, 100.0, 0.0, 0.707);
        assert!((band_db(&hp, 100.0) + 3.0).abs() < 0.2);
        assert!(band_db(&hp, 25.0) < -20.0);
        // Shelves settle at their gain on their side.
        assert!((band_db(&b(FilterType::HighShelf, 5000.0, 4.0, 0.7), 18000.0) - 4.0).abs() < 0.5);
        assert!((band_db(&b(FilterType::LowShelf, 200.0, -3.0, 0.7), 30.0) + 3.0).abs() < 0.5);
    }

    #[test]
    fn graph_mapping_and_new_bands() {
        let d = band_data(&b(FilterType::Peaking, 632.5, 9.0, 1.0));
        assert!((d.x - 0.5).abs() < 0.01 && (d.y - 0.25).abs() < 0.001);
        // Pass filters sit on the 0 dB line.
        assert_eq!(band_data(&b(FilterType::HighPass, 80.0, 5.0, 0.7)).y, 0.5);
        // A new band goes into the widest gap, not on top of an existing one.
        let f = new_band_freq(&[b(FilterType::Peaking, 100.0, 0.0, 1.0), b(FilterType::Peaking, 200.0, 0.0, 1.0)]);
        assert!(f > 1000.0 && f < 10000.0, "{f}");
        let (line, fill) = curve(&[p0()], 1000.0, 300.0);
        assert!(line.starts_with("M 0.0 150.0") && fill.ends_with('Z'));
    }

    fn p0() -> EqBand {
        b(FilterType::Peaking, 1000.0, 0.0, 1.0)
    }

    #[test]
    fn payload_uses_the_type_names_the_backend_parses() {
        let bands = vec![b(FilterType::LowShelf, 120.0, 2.0, 0.7), b(FilterType::Notch, 60.0, 0.0, 8.0)];
        let back = crate::app::parse_client_bands(&payload(&bands).to_string()).unwrap();
        assert_eq!(back[0].filter_type, FilterType::LowShelf);
        assert_eq!(back[1].filter_type, FilterType::Notch);
        assert_eq!(back[0].freq, 120.0);
    }
}

/// Snapshot test helper: shows `bands` without a controller.
#[cfg(test)]
pub(super) fn show_sample(win: &MainWindow) {
    let (name, bands) = STYLES[1];
    let bands: Vec<EqBand> = bands.iter().map(|&(t, f, g, q)| EqBand { freq: f, gain_db: g, q, filter_type: t }).collect();
    let eq = win.global::<Eq>();
    eq.set_bands(Rc::new(VecModel::from(bands.iter().map(band_data).collect::<Vec<_>>())).into());
    let (line, fill) = curve(&bands, 1346.0, 312.0);
    eq.set_curve(line.into());
    eq.set_curve_fill(fill.into());
    eq.set_preset_name(name.into());
    let names: Vec<SharedString> = PROFILES.iter().map(|m| mic_name(m).into()).collect();
    eq.set_profiles(Rc::new(VecModel::from(names)).into());
    eq.set_profile(0);
    eq.set_profile_title(PROFILES[0].profile_title.as_str().into());
    eq.set_profile_summary(PROFILES[0].profile_summary.as_str().into());
}
