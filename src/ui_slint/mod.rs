//! Native UI (Slint), chosen with `--ui-backend slint` (or `STUDIOMIC_UI=slint`).
//!
//! Same backend as the webview. User actions become the same colon messages (`mute:mic:1`,
//! `calib_trim:-3.0`, ...) handled by `Controller::handle_ui` on a `ui-messages` thread, in order.
//! The backend answers with typed `UiPush` events (`State`, `Peaks`, ...); the `Js` ones are for
//! the webview and ignored here. After every batch of messages the thread pushes `State`, so the
//! window always settles on what the backend saved (faders hold it until released).

slint::include_modules!();

mod check;
mod eq;
mod matrix;
mod tune;

use crate::app::{Controller, UiPush};
use crate::audio::daemon::Engine;
use crate::config::StudioMicConfig;
use slint::winit_030::{winit::window::WindowLevel, WinitWindowAccessor};
use slint::{ComponentHandle, Model, SharedString, VecModel};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc, LazyLock};
use std::time::{Duration, Instant};

type TrayHandle = ksni::blocking::Handle<crate::tray::StudioMicTray>;

/// Must be called on the main thread before anything pushes to the UI: it starts the Slint
/// backend, which `invoke_from_event_loop` (used by [`push`]) needs.
pub fn create_window() -> Result<MainWindow, slint::PlatformError> {
    let win = MainWindow::new()?;
    // Wayland app id / X11 class, matching StartupWMClass in studiomic.desktop. Only works once
    // the backend exists (MainWindow::new starts it) and before the window is first shown.
    slint::set_xdg_app_id("studiomic")?;
    Ok(win)
}

/// The `UiPush` sink for the controller, tray and signal handler (any thread).
pub fn push(p: UiPush) {
    if matches!(p, UiPush::Js(_)) {
        return;
    }
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(c) = CTX.with(|c| c.borrow().clone()) {
            c.handle(p);
        }
    });
}

thread_local! {
    static CTX: RefCell<Option<Rc<Ctx>>> = const { RefCell::new(None) };
}

/// Runs the window until Quit. `shutdown` flushes state and stops the engine.
pub fn run(
    win: MainWindow,
    controller: Arc<Controller>,
    engine: Arc<Engine>,
    background: bool,
    tray: Option<TrayHandle>,
    shutdown: Box<dyn FnMut()>,
) -> Result<(), Box<dyn std::error::Error>> {
    // UI messages run on their own thread, in order (several query PipeWire for 50-300 ms).
    let (tx, rx) = mpsc::channel::<String>();
    {
        // Device and app lists for the matrix pickers (the webview gets them baked into the page).
        let c = controller.clone();
        std::thread::Builder::new().name("ui-devices".into()).spawn(move || {
            c.push_devices();
            c.push_live_streams();
            c.push_installed_apps();
        })?;
        let c = controller.clone();
        std::thread::Builder::new().name("ui-messages".into()).spawn(move || {
            while let Ok(first) = rx.recv() {
                c.handle_ui(&first);
                while let Ok(more) = rx.try_recv() {
                    c.handle_ui(&more);
                }
                push(UiPush::State);
            }
        })?;
    }

    let cfg = controller.state.lock().unwrap().clone();
    let lsp = engine.plugins_json()["lsp"].as_bool().unwrap_or(false);
    let ctx = Rc::new(Ctx {
        win: win.clone_strong(),
        controller,
        engine,
        tx,
        tray,
        shutdown: RefCell::new(Some(shutdown)),
        pills: Rc::new(VecModel::default()),
        badges: Rc::new(VecModel::default()),
        peaks: RefCell::new(BTreeMap::new()),
        last_peaks_at: Cell::new(None),
        lsp: Cell::new(lsp),
        holds: RefCell::new(HashMap::new()),
        peak_until: RefCell::new(HashMap::new()),
        widget_saved: Cell::new(None),
        throttles: RefCell::new(HashMap::new()),
        mx: Default::default(),
        eq: Default::default(),
        tune: Default::default(),
        check: Default::default(),
        visible: Cell::new(false),
        mic_missing: Cell::new(false),
        notice_timer: slint::Timer::default(),
        tick: slint::Timer::default(),
    });
    CTX.with(|c| *c.borrow_mut() = Some(ctx.clone()));

    let strip = win.global::<Strip>();
    strip.set_pills(ctx.pills.clone().into());
    strip.set_badges(ctx.badges.clone().into());
    ctx.init_matrix();
    ctx.init_eq();
    ctx.init_check();
    wire_callbacks(&ctx);
    matrix::wire(&ctx);
    eq::wire(&ctx);
    tune::wire(&ctx);
    check::wire(&ctx);
    ctx.refresh_state();

    win.on_widget_fit(widget_scale);
    win.window().set_size(slint::LogicalSize::new(cfg.window_width as f32, cfg.window_height as f32));
    {
        let w = Rc::downgrade(&ctx);
        win.window().on_close_requested(move || {
            // Closing the window keeps the audio engine running in the tray.
            if let Some(c) = w.upgrade() {
                c.visible.set(false);
                c.engine.ui_visible.store(false, Ordering::SeqCst);
                let _ = c.controller.state.lock().unwrap().write_atomic();
            }
            slint::CloseRequestResponse::HideWindow
        });
    }
    {
        // Meters: hold release, idle fall-off and the PEAK! hold need ticks between pushes.
        let w = Rc::downgrade(&ctx);
        ctx.tick.start(slint::TimerMode::Repeated, Duration::from_millis(50), move || {
            if let Some(c) = w.upgrade() {
                if c.visible.get() {
                    c.refresh_levels();
                }
            }
        });
    }

    if !background {
        ctx.set_visible(true);
    }
    if cfg.widget_mode {
        ctx.controller.push_widget();
    }

    slint::run_event_loop_until_quit()?;
    ctx.run_shutdown();
    CTX.with(|c| c.borrow_mut().take());
    Ok(())
}

/// Widget mode: the largest scale at which the widget fits a `w` x `h` (logical px) area with
/// `pills` quick-mute pills. The card is `w / scale` units wide; that sets the pills per row
/// (100 + 6 units each, inside 8-unit padding) and whether the two fader cards sit side by side
/// (from 560 units), which sets the height in units. Height × scale grows with the scale, so
/// bisection finds the answer. Matches WidgetStrip's layout in app.slint.
pub fn widget_scale(w: f32, h: f32, pills: i32) -> f32 {
    if w <= 0.0 || h <= 0.0 {
        return 1.0;
    }
    let units_high = |s: f32| {
        let units_wide = w / s;
        let per_row = ((units_wide - 16.0 + 6.0) / 106.0).floor().max(1.0);
        let rows = (pills.max(0) as f32 / per_row).ceil();
        let cards = if units_wide >= 560.0 { 62.0 } else { 130.0 };
        // padding + header + gaps + badges, fader cards, pill rows
        100.0 + cards + rows * 32.0
    };
    let fits = |s: f32| s * units_high(s) <= h;
    let (mut lo, mut hi) = (0.2f32, (w / 400.0).max(0.2));
    if fits(hi) {
        return hi;
    }
    for _ in 0..30 {
        let mid = (lo + hi) / 2.0;
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

struct Hold {
    val: f32,
    at: Instant,
}

/// Leading + trailing throttle (~30 ms) so fader drags stream smoothly without flooding.
struct Throttle {
    last: Cell<Instant>,
    pending: RefCell<Option<String>>,
    timer: slint::Timer,
}

struct Ctx {
    win: MainWindow,
    controller: Arc<Controller>,
    engine: Arc<Engine>,
    tx: mpsc::Sender<String>,
    tray: Option<TrayHandle>,
    shutdown: RefCell<Option<Box<dyn FnMut()>>>,
    pills: Rc<VecModel<PillData>>,
    badges: Rc<VecModel<BadgeData>>,
    peaks: RefCell<BTreeMap<String, f32>>,
    last_peaks_at: Cell<Option<Instant>>,
    lsp: Cell<bool>,
    holds: RefCell<HashMap<&'static str, Hold>>,
    peak_until: RefCell<HashMap<String, Instant>>,
    widget_saved: Cell<Option<slint::LogicalSize>>,
    throttles: RefCell<HashMap<String, Rc<Throttle>>>,
    mx: matrix::MatrixState,
    eq: eq::EqState,
    tune: tune::TuneState,
    check: check::CheckState,
    visible: Cell<bool>,
    mic_missing: Cell<bool>,
    notice_timer: slint::Timer,
    tick: slint::Timer,
}

const THROTTLE: Duration = Duration::from_millis(30);
const SIZES: [&str; 3] = ["small", "medium", "large"];

impl Ctx {
    fn post(&self, msg: impl Into<String>) {
        let _ = self.tx.send(msg.into());
    }

    fn throttled(&self, key: impl Into<String>, msg: String) {
        let t = self
            .throttles
            .borrow_mut()
            .entry(key.into())
            .or_insert_with(|| {
                Rc::new(Throttle { last: Cell::new(Instant::now() - THROTTLE), pending: RefCell::new(None), timer: slint::Timer::default() })
            })
            .clone();
        let since = t.last.get().elapsed();
        if since >= THROTTLE && !t.timer.running() {
            t.last.set(Instant::now());
            self.post(msg);
            return;
        }
        *t.pending.borrow_mut() = Some(msg);
        if !t.timer.running() {
            let tx = self.tx.clone();
            let t2 = t.clone();
            t.timer.start(slint::TimerMode::SingleShot, THROTTLE.saturating_sub(since), move || {
                if let Some(m) = t2.pending.borrow_mut().take() {
                    t2.last.set(Instant::now());
                    let _ = tx.send(m);
                }
            });
        }
    }

    fn handle(self: &Rc<Self>, p: UiPush) {
        match p {
            UiPush::Js(_) => {}
            UiPush::ShowWindow => self.set_visible(true),
            UiPush::ToggleWindow => self.set_visible(!self.visible.get()),
            UiPush::TrayRefresh => {
                if let Some(h) = &self.tray {
                    let _ = h.update(|_| {});
                }
            }
            UiPush::Widget { on, width, height, on_top } => self.set_widget(on, width, height, on_top),
            UiPush::Quit => {
                println!("Quitting Studio Mic...");
                let _ = self.win.hide();
                self.run_shutdown();
                let _ = slint::quit_event_loop();
            }
            UiPush::State => self.refresh_state(),
            UiPush::Peaks(json) => {
                if let Ok(map) = serde_json::from_str::<BTreeMap<String, f32>>(&json) {
                    *self.peaks.borrow_mut() = map;
                    self.last_peaks_at.set(Some(Instant::now()));
                    self.refresh_levels();
                }
            }
            UiPush::Notice(msg) => self.notice(&msg),
            UiPush::MicMissing(missing) => {
                self.mic_missing.set(missing);
                self.refresh_state();
            }
            UiPush::Plugins(json) => {
                let lsp = serde_json::from_str::<serde_json::Value>(&json).ok().and_then(|v| v["lsp"].as_bool());
                self.lsp.set(lsp.unwrap_or(false));
            }
            UiPush::Devices { mics, outs } => {
                *self.mx.mics.borrow_mut() = mics;
                *self.mx.outs.borrow_mut() = outs;
                self.refresh_state();
            }
            UiPush::InstalledApps(apps) => {
                *self.mx.installed.borrow_mut() = apps;
                self.refresh_apps();
            }
            UiPush::Recorder(json) => self.on_recorder(&json),
            UiPush::Preview(json) => self.on_preview(&json),
            UiPush::LiveStreams(streams) => {
                *self.mx.live.borrow_mut() = streams;
                self.refresh_state();
            }
        }
    }

    fn run_shutdown(&self) {
        if let Some(mut f) = self.shutdown.borrow_mut().take() {
            f();
        }
    }

    fn set_visible(&self, vis: bool) {
        self.visible.set(vis);
        self.engine.ui_visible.store(vis, Ordering::SeqCst);
        if vis {
            let _ = self.win.show();
            self.win.window().with_winit_window(|w| w.focus_window());
            self.refresh_state();
        } else {
            let _ = self.win.hide();
        }
    }

    fn notice(self: &Rc<Self>, msg: &str) {
        self.win.set_notice(msg.into());
        let w = Rc::downgrade(self);
        self.notice_timer.start(slint::TimerMode::SingleShot, Duration::from_secs(4), move || {
            if let Some(c) = w.upgrade() {
                c.win.set_notice(SharedString::new());
            }
        });
    }

    fn set_on_top(&self, on: bool) {
        self.win.window().with_winit_window(|w| w.set_window_level(if on { WindowLevel::AlwaysOnTop } else { WindowLevel::Normal }));
    }

    /// Mini floating widget: remember the normal size, resize, keep on top (Hyprland also floats/pins).
    fn set_widget(&self, on: bool, width: f64, height: f64, on_top: bool) {
        crate::hyprland_widget(on, width, height, on_top);
        let window = self.win.window();
        if on {
            if self.widget_saved.get().is_none() {
                self.widget_saved.set(Some(window.size().to_logical(window.scale_factor())));
            }
            window.set_size(slint::LogicalSize::new(width as f32, height as f32));
            self.set_on_top(on_top);
        } else if let Some(size) = self.widget_saved.take() {
            self.set_on_top(false);
            window.set_size(size);
        }
        let strip = self.win.global::<Strip>();
        strip.set_widget_mode(on);
        strip.set_on_top(on_top);
    }

    /// Saves the window (or widget) size the user dragged to.
    fn on_resized(&self, w: f32, h: f32) {
        let (w, h) = (w as f64, h as f64);
        if w <= 100.0 || h <= 60.0 {
            return;
        }
        let mut s = self.controller.state.lock().unwrap();
        let widget = self.widget_saved.get().is_some();
        let (cw, ch) = if widget { (s.widget_width, s.widget_height) } else { (s.window_width, s.window_height) };
        if (cw - w).abs() <= 1.0 && (ch - h).abs() <= 1.0 {
            return;
        }
        if widget {
            s.widget_width = w.round();
            s.widget_height = h.round();
        } else if h > 100.0 {
            s.window_width = w.round();
            s.window_height = h.round();
        } else {
            return;
        }
        s.normalize();
        let _ = s.save();
    }

    /// Everything that comes from the saved state (not the meters).
    fn refresh_state(&self) {
        let s = self.controller.state.lock().unwrap().clone();
        let strip = self.win.global::<Strip>();
        strip.set_mic_muted(s.mic.mix.stream_muted);
        strip.set_mic_trim(s.calibration.preamp_gain_db);
        strip.set_mic_name(if self.mic_missing.get() { "Mic unplugged".into() } else { "Microphone".into() });
        strip.set_mon_muted(s.monitor_muted);
        strip.set_mon_gain(s.monitor_gain_db);
        strip.set_hear_myself(!s.mic.mix.monitor_muted);
        strip.set_on_top(s.always_on_top);
        strip.set_widget_mode(s.widget_mode);
        let nav = self.win.global::<Nav>();
        nav.set_simple(s.mixer_simple);
        nav.set_collapsed(s.strip_collapsed);
        let size = if SIZES.contains(&s.ui_size.as_str()) { s.ui_size.as_str() } else { "medium" };
        strip.set_size_label(size[..1].to_uppercase().into());
        self.win.set_size_factor(match size {
            "small" => 0.85,
            "large" => 1.2,
            _ => 1.0,
        });

        let pills: Vec<PillData> = channels(&s)
            .into_iter()
            .map(|(id, name, icon)| PillData {
                level: self.status(&id, &s),
                label: short_name(&id, &name).into(),
                glyph: glyph(&channel_icon(&id, &name, &icon)),
                id: id.into(),
            })
            .collect();
        let same = self.pills.row_count() == pills.len()
            && self.pills.iter().zip(&pills).all(|(a, b)| a.id == b.id && a.label == b.label && a.glyph == b.glyph && a.level == b.level);
        if !same {
            self.pills.set_vec(pills);
        }

        let engine = s.denoiser.engine.as_str();
        let badges = vec![
            BadgeData { id: "clipguard".into(), label: "CLIPGUARD".into(), on: s.mic.clipguard },
            BadgeData {
                id: "denoiser".into(),
                label: match engine {
                    "off" | "" => "NOISE OFF".into(),
                    "deepfilter" => "DEEPFILTER".into(),
                    "rnnoise" => "RNNOISE".into(),
                    "speex" => "SPEEX".into(),
                    other => other.to_uppercase().into(),
                },
                on: !matches!(engine, "off" | ""),
            },
            BadgeData { id: "comp".into(), label: "COMP".into(), on: s.noise_clean.compressor_enabled },
        ];
        if self.badges.iter().ne(badges.iter().cloned()) {
            self.badges.set_vec(badges);
        }
        self.refresh_matrix(&s);
        self.refresh_eq(&s);
        self.refresh_tune(&s);
        drop(s);
        self.refresh_apps();
        self.refresh_levels();
    }

    /// Meters, mute rings and pill status from the latest peaks.
    fn refresh_levels(&self) {
        if self.last_peaks_at.get().is_none_or(|t| t.elapsed() > Duration::from_millis(1500)) {
            // No pushes while silent or hidden: let everything fall back to idle.
            self.peaks.borrow_mut().clear();
        }
        let s = self.controller.state.lock().unwrap();
        let strip = self.win.global::<Strip>();

        let mic = self.level_state("mic", &s);
        let frac = meter_fraction(mic.db);
        strip.set_mic_meter(frac);
        strip.set_mic_hold(self.held("mic", frac, Duration::from_millis(1500)));
        strip.set_mic_level(self.status("mic", &s));
        let pk = self.held("mic-peak", self.peak("mic"), Duration::from_millis(1500));
        strip.set_mic_peak_text(if pk <= -79.9 { "-∞ dBFS".into() } else { format!("{:.1} dBFS", pk).into() });
        let pad = self.peak("pad");
        strip.set_pad_text(if pad > 0.05 { format!("PAD -{:.0} dB", pad).into() } else { SharedString::new() });

        let mon = self.level_state("bus:monitor", &s);
        let frac = meter_fraction(mon.db);
        strip.set_mon_meter(frac);
        strip.set_mon_hold(self.held("mon", frac, Duration::from_millis(1500)));
        strip.set_mon_level(self.status("bus:monitor", &s));

        for i in 0..self.pills.row_count() {
            let Some(mut p) = self.pills.row_data(i) else { continue };
            let level = self.status(&p.id, &s);
            if p.level != level {
                p.level = level;
                self.pills.set_row_data(i, p);
            }
        }
        self.refresh_matrix_meters(&s);
        self.refresh_tune_level();
    }

    fn peak(&self, key: &str) -> f32 {
        self.peaks.borrow().get(key).copied().unwrap_or(-80.0)
    }

    /// Peak hold: keeps the highest value for `dur`, then follows the signal again.
    fn held(&self, key: &'static str, v: f32, dur: Duration) -> f32 {
        let mut holds = self.holds.borrow_mut();
        let h = holds.entry(key).or_insert(Hold { val: v, at: Instant::now() });
        if v >= h.val || h.at.elapsed() > dur {
            h.val = v;
            h.at = Instant::now();
        }
        h.val
    }

    /// Same meaning as `smLevelState` in the HTML UI. Meters show what a strip sends to the
    /// Stream bus (so they match OBS); red = real distortion, amber = a limiter catching peaks
    /// or a hot mic input. See CLAUDE.md "Meters".
    fn level_state(&self, id: &str, s: &StudioMicConfig) -> LevelState {
        let lsp = self.lsp.get();
        if id.starts_with("bus:") {
            let db = self.peak(id);
            return LevelState { db, clip: db > 0.0, limit: lsp && db >= -1.5 };
        }
        let pre = self.peak(id);
        let muted = s.mix(id).is_some_and(|m| m.stream_muted);
        let db = if muted { pre } else { self.peaks.borrow().get(&format!("out:{id}")).copied().unwrap_or(pre) };
        let is_mic = id == "mic";
        let clipguard = is_mic && s.mic.clipguard;
        let clip = (is_mic && self.peak("raw_clip") > 0.0) || (!lsp && db > 0.0);
        let limit = !clip && ((!muted && db >= -1.5) || (clipguard && pre >= -1.5));
        let hot = !clip && is_mic && self.peak("raw") >= -1.0;
        LevelState { db, clip, limit: limit || hot }
    }

    /// Pill / mute-ring status (`smStatus`): muted, PEAK! (real clipping, held 800 ms), HOT, ok.
    fn status(&self, id: &str, s: &StudioMicConfig) -> Level {
        let muted = if id == "bus:monitor" { s.monitor_muted } else { s.mix(id).is_some_and(|m| m.stream_muted) };
        if muted {
            return Level::Muted;
        }
        let ls = self.level_state(id, s);
        let now = Instant::now();
        let mut until = self.peak_until.borrow_mut();
        if ls.clip {
            until.insert(id.to_string(), now + Duration::from_millis(800));
        }
        if until.get(id).is_some_and(|&t| now < t) {
            Level::Peak
        } else if ls.db >= -6.0 || ls.limit {
            Level::Hot
        } else {
            Level::Ok
        }
    }
}

struct LevelState {
    db: f32,
    clip: bool,
    limit: bool,
}

fn meter_fraction(db: f32) -> f32 {
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}

/// (id, name, icon) of every strip, in UI order: mic, mic 2 (when on), channels.
fn channels(s: &StudioMicConfig) -> Vec<(String, String, String)> {
    let mut v = vec![("mic".to_string(), "Microphone".to_string(), "mic".to_string())];
    if s.dual_mic.enabled {
        v.push(("mic2".into(), "Microphone 2".into(), "mic_external_on".into()));
    }
    v.extend(s.virtual_channels.iter().map(|c| (c.id.clone(), c.name.clone(), c.icon.clone())));
    v
}

fn short_name(id: &str, name: &str) -> String {
    match id {
        "mic" => "MIC".into(),
        "mic2" => "MIC 2".into(),
        _ => {
            let first = name.split(|c: char| c.is_whitespace() || c == '/' || c == '·').find(|w| !w.is_empty()).unwrap_or(id);
            first.chars().take(9).collect::<String>().to_uppercase()
        }
    }
}

/// The channel's own icon, or one guessed from its id / name (`getChannelIcon` in the HTML UI).
fn channel_icon(id: &str, name: &str, icon: &str) -> String {
    if !icon.is_empty() && icon != "instant_mix" && GLYPHS.contains_key(icon) {
        return icon.to_string();
    }
    let s = format!("{} {}", id, name).to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| s.contains(w));
    if has(&["capture", "cam", "video", "hdmi"]) {
        "videocam"
    } else if has(&["game", "steam", "app"]) {
        "sports_esports"
    } else if has(&["voice", "discord", "chat"]) {
        "forum"
    } else if has(&["music", "spotify", "media", "vod"]) {
        "music_note"
    } else if has(&["browser", "chrome", "zen"]) {
        "language"
    } else if has(&["mic"]) {
        "mic"
    } else {
        "developer_board"
    }
    .to_string()
}

/// Material Symbols name → glyph (the bundled font's codepoints; no ligatures needed).
static GLYPHS: LazyLock<HashMap<&'static str, char>> = LazyLock::new(|| {
    include_str!("../../ui/material-symbols-outlined.codepoints")
        .lines()
        .filter_map(|l| {
            let (name, cp) = l.split_once(' ')?;
            Some((name, char::from_u32(u32::from_str_radix(cp.trim(), 16).ok()?)?))
        })
        .collect()
});

fn glyph(name: &str) -> SharedString {
    GLYPHS.get(name).or_else(|| GLYPHS.get("instant_mix")).map(|c| c.to_string().into()).unwrap_or_default()
}

/// Strip callbacks → backend messages. Each also updates the window at once (optimistic); the
/// `State` push after the message is handled confirms it.
fn wire_callbacks(ctx: &Rc<Ctx>) {
    let strip = ctx.win.global::<Strip>();
    macro_rules! cb {
        (|$c:ident $(, $a:ident)*| $body:block) => {{
            let w = Rc::downgrade(ctx);
            move |$($a),*| {
                if let Some($c) = w.upgrade() $body
            }
        }};
    }
    let state = |c: &Ctx| c.controller.state.lock().unwrap().clone();

    strip.on_mic_mute(cb!(|c| {
        let next = !state(&c).mic.mix.stream_muted;
        c.win.global::<Strip>().set_mic_muted(next);
        c.post(format!("mute:mic:{}", next as u8));
    }));
    strip.on_mic_trim_moved(cb!(|c, v| {
        c.throttled("calib_trim", format!("calib_trim:{:.1}", v));
    }));
    strip.on_mon_mute(cb!(|c| {
        let next = !state(&c).monitor_muted;
        c.win.global::<Strip>().set_mon_muted(next);
        c.post(format!("monitor_master:mute:{}", if next { "on" } else { "off" }));
    }));
    strip.on_mon_gain_moved(cb!(|c, v| {
        c.throttled("monitor_gain", format!("monitor_master:gain:={}", v));
    }));
    strip.on_hear_myself_toggle(cb!(|c| {
        // "Hear Myself" = the mic's Monitor branch.
        let hear = state(&c).mic.mix.monitor_muted;
        c.win.global::<Strip>().set_hear_myself(hear);
        c.post(format!("mix:mic:monitor_mute:{}", if hear { "off" } else { "on" }));
    }));
    strip.on_pill_toggle(cb!(|c, id| {
        let muted = state(&c).mix(&id).is_some_and(|m| m.stream_muted);
        c.post(format!("mute:{}:{}", id, (!muted) as u8));
    }));
    strip.on_badge_clicked(cb!(|c, id| {
        let s = state(&c);
        match id.as_str() {
            "clipguard" => c.post(format!("mic_option:clipguard:{}", (!s.mic.clipguard) as u8)),
            "comp" => c.post(format!("toggle_compressor:{}", (!s.noise_clean.compressor_enabled) as u8)),
            _ => c.notice("Noise remover settings are in the full interface for now (--ui-backend web)."),
        }
    }));
    strip.on_cycle_size(cb!(|c| {
        let cur = state(&c).ui_size;
        let i = SIZES.iter().position(|s| *s == cur).unwrap_or(1);
        c.post(format!("ui_size:{}", SIZES[(i + 1) % SIZES.len()]));
    }));
    strip.on_toggle_widget(cb!(|c| {
        c.post(format!("toggle_widget_mode:{}", (!state(&c).widget_mode) as u8));
    }));
    strip.on_toggle_on_top(cb!(|c| {
        c.post(format!("always_on_top:{}", (!state(&c).always_on_top) as u8));
    }));
    let nav = ctx.win.global::<Nav>();
    nav.on_set_simple(cb!(|c, simple| {
        c.post(format!("mixer_view:{}", if simple { "simple" } else { "matrix" }));
    }));
    nav.on_set_collapsed(cb!(|c, on| {
        c.post(format!("strip_collapsed:{}", on as u8));
    }));
    ctx.win.on_resized(cb!(|c, w, h| {
        c.on_resized(w, h);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_resolve_from_bundled_codepoints() {
        assert_eq!(glyph("mic").as_str(), "\u{e31d}");
        assert_eq!(glyph("headphones").as_str(), "\u{f01f}");
        // Unknown names fall back instead of showing nothing.
        assert_eq!(glyph("no_such_icon").as_str(), "\u{e026}");
    }

    #[test]
    fn channel_icons_and_short_names() {
        assert_eq!(channel_icon("game", "Game Audio", ""), "sports_esports");
        assert_eq!(channel_icon("x", "Discord Voice", "instant_mix"), "forum");
        assert_eq!(channel_icon("x", "Whatever", "music_note"), "music_note");
        assert_eq!(short_name("music", "Music / Media"), "MUSIC");
        assert_eq!(short_name("mic", "anything"), "MIC");
        assert_eq!(short_name("cap", "Capture Card"), "CAPTURE");
    }

    #[test]
    fn widget_uses_the_largest_scale_that_fits() {
        // Wide and short (the size reported): cards side by side, six pills in two rows of
        // five, limited by the height and far bigger than the 400-unit width alone would give.
        let s = widget_scale(1287.0, 481.0, 6);
        assert!(s * (100.0 + 62.0 + 2.0 * 32.0) <= 481.0 + 0.01);
        assert!(s > 2.1, "{s}");
        // Narrow and tall: limited by the 400-unit width.
        assert!((widget_scale(348.0, 700.0, 6) - 0.87).abs() < 0.01);
    }

    #[test]
    fn meter_scale_matches_html_ui() {
        assert_eq!(meter_fraction(-80.0), 0.0);
        assert_eq!(meter_fraction(-30.0), 0.5);
        assert_eq!(meter_fraction(3.0), 1.0);
    }
}

/// Offscreen renders of the native UI with sample data, for checking layout and scaling without
/// starting the engine: `SM_SNAPSHOT_DIR=/tmp/x cargo test --release snapshots -- --ignored`.
#[cfg(test)]
mod snapshot {
    use super::*;
    use slint::platform::software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType};
    use slint::platform::{Platform, WindowAdapter, WindowEvent};

    struct Offscreen(Rc<MinimalSoftwareWindow>);
    impl Platform for Offscreen {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }

    fn sample(win: &MainWindow, widget: bool) {
        let strip = win.global::<Strip>();
        strip.set_widget_mode(widget);
        strip.set_mic_trim(-6.0);
        strip.set_mic_meter(0.72);
        strip.set_mic_hold(0.81);
        strip.set_mic_level(Level::Ok);
        strip.set_mic_peak_text("-11.4 dBFS".into());
        strip.set_pad_text("PAD -6 dB".into());
        strip.set_mon_gain(12.0);
        strip.set_mon_meter(0.55);
        strip.set_mon_level(Level::Hot);
        strip.set_hear_myself(true);
        let pills = [("mic", "MIC", "mic", Level::Ok), ("game", "GAME", "sports_esports", Level::Hot), ("discord", "DISCORD", "forum", Level::Ok),
            ("music", "MUSIC", "music_note", Level::Peak), ("cap", "CAPTURE", "videocam", Level::Muted), ("mixer", "MIXER", "developer_board", Level::Ok)];
        let pills: Vec<PillData> = pills.iter().map(|(id, l, g, lv)| PillData { id: (*id).into(), label: (*l).into(), glyph: glyph(g), level: *lv }).collect();
        strip.set_pills(Rc::new(VecModel::from(pills)).into());
        let badges = vec![
            BadgeData { id: "clipguard".into(), label: "CLIPGUARD".into(), on: true },
            BadgeData { id: "denoiser".into(), label: "RNNOISE".into(), on: true },
            BadgeData { id: "comp".into(), label: "COMP".into(), on: false },
        ];
        strip.set_badges(Rc::new(VecModel::from(badges)).into());

        // Matrix: mic + three channels + a custom mix, with a missing headphone output.
        let mut cfg = StudioMicConfig::default();
        let ch = |id: &str, name: &str, kind: &str, apps: &[&str]| {
            serde_json::from_value::<crate::config::VirtualChannelConfig>(serde_json::json!({ "id": id, "name": name, "kind": kind, "apps": apps })).unwrap()
        };
        cfg.virtual_channels = vec![
            ch("game", "Game", "virtual", &["steam", "proton"]),
            ch("voice", "Voice Chat", "comms", &["discord", "vesktop", "teamspeak", "mumble"]),
            ch("music", "Music", "virtual", &[]),
        ];
        cfg.virtual_channels[0].mix.voice_routed = true;
        cfg.virtual_channels[2].mix.vod = false;
        cfg.virtual_channels[2].mix.stream_gain_db = -12.0;
        cfg.mic.mix.monitor_muted = true;
        cfg.custom_mixes.push(serde_json::from_value(serde_json::json!({ "id": "guest", "name": "Guest Mix", "color": "#fb7185" })).unwrap());
        cfg.virtual_channels[0].mix.sends.insert("guest".into(), Default::default());
        cfg.active_hw_mic = Some("alsa_input.usb-Shure_MV7".into());
        cfg.monitor_output = Some("alsa_output.gone".into());
        let dev = |n: &str, d: &str| crate::audio::device_discovery::AudioDevice { id: 1, name: n.into(), description: d.into(), is_default: false, ports: vec![], alsa_card: None };
        let b = matrix::build(&cfg, &[dev("alsa_input.usb-Shure_MV7", "Shure MV7")], &[dev("alsa_output.hp", "Headphones")], &[]);
        let m = win.global::<Matrix>();
        let n = b.cells.len();
        m.set_cols(Rc::new(VecModel::from(b.cols)).into());
        m.set_rows(Rc::new(VecModel::from(b.rows)).into());
        m.set_cells(Rc::new(VecModel::from(b.cells)).into());
        m.set_meters(Rc::new(VecModel::from((0..n).map(|i| [0.0, 0.7, 0.55, 0.4][i % 4]).collect::<Vec<f32>>())).into());
        m.set_mon_gain(-3.0);
    }

    fn shot(window: &MinimalSoftwareWindow, dir: &std::path::Path, name: &str, w: u32, h: u32, scale: f32) {
        window.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: scale });
        window.set_size(slint::PhysicalSize::new(w, h));
        // Let bindings settle and animations finish.
        for _ in 0..4 {
            slint::platform::update_timers_and_animations();
            std::thread::sleep(Duration::from_millis(60));
        }
        window.request_redraw();
        let mut buf = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
        window.draw_if_needed(|r| {
            r.render(&mut buf, w as usize);
        });
        let file = std::fs::File::create(dir.join(format!("{name}.png"))).unwrap();
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let data: Vec<u8> = buf.iter().flat_map(|p| [p.red, p.green, p.blue]).collect();
        enc.write_header().unwrap().write_image_data(&data).unwrap();
    }

    #[test]
    #[ignore]
    fn snapshots() {
        let dir = std::path::PathBuf::from(std::env::var("SM_SNAPSHOT_DIR").expect("set SM_SNAPSHOT_DIR"));
        std::fs::create_dir_all(&dir).unwrap();
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Offscreen(window.clone()))).unwrap();
        let win = MainWindow::new().unwrap();
        win.on_widget_fit(widget_scale);
        win.show().unwrap();
        sample(&win, false);
        shot(&window, &dir, "main-1400x900@1.0", 1400, 900, 1.0);
        shot(&window, &dir, "main-775x860@1.6", 1240, 1376, 1.6);
        shot(&window, &dir, "main-2240x1100@1.6", 2240, 1100, 1.6);
        shot(&window, &dir, "main-700x800@1.0", 700, 800, 1.0);
        win.set_size_factor(1.2);
        shot(&window, &dir, "main-1400x700-large", 1400, 700, 1.0);
        shot(&window, &dir, "size-large-835x864@1.6", 1336, 1382, 1.6);
        win.set_size_factor(0.85);
        shot(&window, &dir, "size-small-835x864@1.6", 1336, 1382, 1.6);
        win.set_size_factor(1.0);
        win.global::<Nav>().set_simple(true);
        shot(&window, &dir, "simple-775x860@1.6", 1240, 1376, 1.6);
        win.global::<Nav>().set_collapsed(true);
        shot(&window, &dir, "simple-collapsed-1400x900", 1400, 900, 1.0);
        win.global::<Nav>().set_simple(false);
        shot(&window, &dir, "matrix-collapsed-775x860@1.6", 1240, 1376, 1.6);
        win.global::<Nav>().set_collapsed(false);
        eq::show_sample(&win);
        win.global::<Nav>().set_tab(1);
        shot(&window, &dir, "eq-1400x900", 1400, 900, 1.0);
        shot(&window, &dir, "eq-775x860@1.6", 1240, 1376, 1.6);
        let t = win.global::<Tune>();
        let nc = crate::config::NoiseCleanConfig::default();
        t.set_rumble_on(true);
        t.set_rumble_freq(nc.rumble_freq);
        t.set_hiss_on(false);
        t.set_hiss_freq(nc.hiss_freq);
        t.set_engine(1);
        t.set_engine_status("ACTIVE".into());
        t.set_rn_vad(50.0);
        t.set_rn_grace(200.0);
        t.set_gate_on(true);
        t.set_gate_thr(-40.0);
        t.set_gate_level(0.45);
        t.set_deess_on(true);
        t.set_deess_freq(5800.0);
        t.set_deess_thr(-30.0);
        t.set_comp_on(true);
        t.set_comp_ratio(3.5);
        t.set_comp_thr(-20.0);
        t.set_comp_makeup(3.5);
        t.set_limiter_on(true);
        win.global::<Nav>().set_tab(2);
        shot(&window, &dir, "tune-1400x900", 1400, 900, 1.0);
        shot(&window, &dir, "tune-775x860@1.6", 1240, 1376, 1.6);
        let k = win.global::<Check>();
        k.set_preview_available(true);
        k.set_rec_state("done".into());
        k.set_rec_progress(1.0);
        k.set_rec_status("Recorded. Preview it on EQ or Fine-Tune.".into());
        k.set_check_done(true);
        k.set_check_gate("-43 dB".into());
        k.set_check_deess("-4.5 dB".into());
        k.set_check_peak("-14.2 dBFS".into());
        k.set_check_trim("+3.0 dB".into());
        k.set_optimize_result("Optimized: room floor -48.3 dBFS → gate -43 dB, de-esser -4.5 dB, makeup +3.5 dB.".into());
        win.global::<Nav>().set_tab(3);
        shot(&window, &dir, "check-1400x900", 1400, 900, 1.0);
        shot(&window, &dir, "check-775x860@1.6", 1240, 1376, 1.6);
        win.global::<Nav>().set_tab(1);
        shot(&window, &dir, "eq-preview-775x860@1.6", 1240, 1376, 1.6);
        win.global::<Nav>().set_tab(0);
        win.global::<Matrix>().set_empty(true);
        shot(&window, &dir, "main-1400x900-empty", 1400, 900, 1.0);
        win.global::<Matrix>().set_empty(false);
        win.global::<Matrix>().set_add_mix_open(true);
        win.global::<Matrix>().set_mix_name("Guest Mix".into());
        shot(&window, &dir, "main-1400x900-addmix", 1400, 900, 1.0);
        win.global::<Matrix>().set_add_mix_open(false);
        {
            let m = win.global::<Matrix>();
            let item = |k: &str, l: &str, sub: &str, here: bool| AppItem { key: k.into(), label: l.into(), sub: sub.into(), here };
            m.set_apps_channel("Game".into());
            m.set_apps_current(Rc::new(VecModel::from(vec![SharedString::from("steam"), "proton".into()])).into());
            m.set_apps_live(Rc::new(VecModel::from(vec![item("firefox", "Firefox", "Playing on your default output", false), item("steam", "Steam", "Playing in this channel", true)])).into());
            let inst: Vec<AppItem> = ["Audacity", "Discord", "Elisa", "Firefox", "Heroic", "Lutris", "OBS Studio", "Spotify"].iter().map(|n| item(&n.to_lowercase(), n, if *n == "Discord" { "In Voice Chat" } else { "" }, false)).collect();
            m.set_apps_installed_total(inst.len() as i32);
            m.set_apps_installed(Rc::new(VecModel::from(inst)).into());
            m.set_apps_open(true);
            shot(&window, &dir, "apps-picker-835x864@1.6", 1336, 1382, 1.6);
            m.set_apps_open(false);
        }
        sample(&win, true);
        shot(&window, &dir, "widget-420x320", 420, 320, 1.0);
        shot(&window, &dir, "widget-260x200", 260, 200, 1.0);
        shot(&window, &dir, "widget-900x600", 900, 600, 1.0);
        shot(&window, &dir, "widget-672x512@1.6", 672, 512, 1.6);
        shot(&window, &dir, "widget-834x491@1.6", 1334, 786, 1.6);
        shot(&window, &dir, "widget-1200x300", 1200, 300, 1.0);
        shot(&window, &dir, "widget-360x700", 360, 700, 1.0);
    }
}
