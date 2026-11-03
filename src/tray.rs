//! StatusNotifierItem tray.
//! Colour = state: green LIVE, red MUTED, amber PREVIEW. Left-click shows/hides the window,
//! middle-click toggles the mic mute, the scroll wheel nudges the Master Monitor Gain.

use crate::app::{Controller, UiPush};
use ksni::blocking::{Handle, TrayMethods};
use ksni::{menu::*, MenuItem, Tray};
use std::sync::Arc;

pub struct StudioMicTray {
    pub controller: Arc<Controller>,
    pub ui: Box<dyn Fn(UiPush) + Send>,
}

#[derive(Clone, Copy, PartialEq)]
enum Live {
    Live,
    Muted,
    Preview,
}

impl StudioMicTray {
    fn live(&self) -> Live {
        if self.controller.engine.preview_is_active() {
            Live::Preview
        } else if self.controller.mic_muted() {
            Live::Muted
        } else {
            Live::Live
        }
    }
}

/// 22x22 ARGB32 (network byte order) mic glyph on a coloured disc.
fn state_icon(live: Live) -> ksni::Icon {
    let (r, g, b) = match live {
        Live::Live => (0x34u8, 0xC7u8, 0x59u8),
        Live::Muted => (0xE5, 0x39, 0x35),
        Live::Preview => (0xFF, 0xB3, 0x00),
    };
    let size = 22i32;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    let c = (size as f32 - 1.0) / 2.0;
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32 - c, y as f32 - c);
            let d = (fx * fx + fy * fy).sqrt();
            // white mic capsule + stand
            let capsule = fx.abs() <= 2.6 && fy >= -6.5 && fy <= 1.5 && !(fy < -4.0 && (fx * fx + (fy + 4.0) * (fy + 4.0)).sqrt() > 2.6);
            let stand = (fx.abs() <= 0.8 && fy > 3.5 && fy <= 6.0) || (fy > 5.2 && fy <= 6.4 && fx.abs() <= 3.0);
            let cup = (d - 0.0).abs() < 100.0 && fy > -0.5 && fy < 4.0 && ((fx * fx + (fy - 0.5) * (fy - 0.5)).sqrt() - 4.6).abs() < 0.8;
            let px = if d > c + 0.5 {
                [0u8, 0, 0, 0]
            } else if capsule || stand || cup {
                [255, 255, 255, 255]
            } else {
                let a = if d > c - 0.5 { 160 } else { 255 };
                [a, r, g, b]
            };
            data.extend_from_slice(&px);
        }
    }
    ksni::Icon { width: size, height: size, data }
}

impl Tray for StudioMicTray {
    fn id(&self) -> String {
        "studiomic".into()
    }

    fn title(&self) -> String {
        "Studio Mic".into()
    }

    fn icon_name(&self) -> String {
        // Themes that ignore pixmaps still get a sensible symbolic icon.
        match self.live() {
            Live::Muted => "microphone-sensitivity-muted".into(),
            _ => "audio-input-microphone".into(),
        }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![state_icon(self.live())]
    }

    fn status(&self) -> ksni::Status {
        match self.live() {
            Live::Live => ksni::Status::Active,
            _ => ksni::Status::NeedsAttention,
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let s = self.controller.state.lock().unwrap().clone();
        let mic = match self.live() {
            Live::Live => "LIVE",
            Live::Muted => "MUTED",
            Live::Preview => "PREVIEW (headphones only)",
        };
        ksni::ToolTip {
            title: format!("Studio Mic · {}", mic),
            description: format!(
                "Monitor {:+.1} dB{} · Denoiser {} · Clipguard {}\nMiddle-click: mute · Scroll: monitor gain",
                s.monitor_gain_db,
                if s.monitor_muted { " (muted)" } else { "" },
                s.denoiser.engine,
                if s.mic.clipguard { "on" } else { "off" }
            ),
            icon_name: String::new(),
            icon_pixmap: vec![state_icon(self.live())],
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        (self.ui)(UiPush::ToggleWindow);
    }

    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        let _ = self.controller.toggle_mic_mute();
    }

    fn scroll(&mut self, delta: i32, orientation: ksni::Orientation) {
        if orientation == ksni::Orientation::Vertical && delta != 0 {
            // Most compositors send ±120 per notch; normalise to 1 dB steps.
            let steps = if delta.abs() >= 60 { delta / 120 } else { delta.signum() };
            let steps = if steps == 0 { delta.signum() } else { steps };
            self.controller.nudge_monitor_gain(-(steps as f32));
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let s = self.controller.state.lock().unwrap().clone();
        let mic_muted = s.mic.mix.stream_muted;
        let mut channel_items: Vec<MenuItem<Self>> = Vec::new();
        for ch in &s.virtual_channels {
            let id = ch.id.clone();
            let muted = ch.mix.stream_muted;
            channel_items.push(
                CheckmarkItem {
                    label: format!("Mute {} (stream)", ch.name),
                    checked: muted,
                    activate: Box::new(move |t: &mut Self| {
                        let _ = t.controller.channel_mute(&id, "stream");
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        let denoisers: Vec<MenuItem<Self>> = ["deepfilter", "rnnoise", "speex", "off"]
            .iter()
            .filter(|e| self.controller.engine.plugins().denoiser_available(e))
            .map(|e| {
                let e = e.to_string();
                CheckmarkItem {
                    label: match e.as_str() {
                        "deepfilter" => "Deep Noise Remover (DeepFilterNet)".into(),
                        "rnnoise" => "RNNoise".into(),
                        "speex" => "Speex".into(),
                        _ => "Off".into(),
                    },
                    checked: s.denoiser.engine == e,
                    activate: Box::new(move |t: &mut Self| {
                        let _ = t.controller.set_denoiser(&e);
                        t.controller.sync_ui();
                    }),
                    ..Default::default()
                }
                .into()
            })
            .collect();

        vec![
            StandardItem {
                label: "Show / Hide Studio Mic".into(),
                icon_name: "window-maximize".into(),
                activate: Box::new(|t: &mut Self| (t.ui)(UiPush::ToggleWindow)),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Mini Widget".into(),
                checked: s.widget_mode,
                activate: Box::new(|t: &mut Self| {
                    let _ = t.controller.set_widget(None);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            CheckmarkItem {
                label: "Mute Microphone".into(),
                icon_name: "microphone-sensitivity-muted".into(),
                checked: mic_muted,
                activate: Box::new(|t: &mut Self| {
                    let _ = t.controller.toggle_mic_mute();
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Hear Myself (Mic Monitor)".into(),
                icon_name: "audio-headphones".into(),
                checked: !s.mic.mix.monitor_muted,
                activate: Box::new(|t: &mut Self| {
                    let _ = t.controller.channel_mute("mic", "monitor");
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Clipguard".into(),
                checked: s.mic.clipguard,
                activate: Box::new(|t: &mut Self| {
                    let _ = t.controller.toggle_clipguard();
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Mute Monitor (Headphones)".into(),
                checked: s.monitor_muted,
                activate: Box::new(|t: &mut Self| {
                    let _ = t.controller.toggle_monitor_mute();
                }),
                ..Default::default()
            }
            .into(),
            SubMenu { label: "Noise Remover".into(), submenu: denoisers, ..Default::default() }.into(),
            SubMenu { label: "Channel Mutes".into(), submenu: channel_items, ..Default::default() }.into(),
            MenuItem::Separator,
            CheckmarkItem {
                label: "Start Minimized".into(),
                checked: s.start_minimized,
                activate: Box::new(move |t: &mut Self| {
                    t.controller.handle_ui(&format!("toggle_start_minimized:{}", if s.start_minimized { 0 } else { 1 }));
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Launch on Login".into(),
                checked: s.autostart,
                activate: Box::new(move |t: &mut Self| {
                    t.controller.handle_ui(&format!("toggle_autostart:{}", if s.autostart { 0 } else { 1 }));
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit Studio Mic".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| (t.ui)(UiPush::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

pub fn spawn_tray<F: Fn(UiPush) + Send + 'static>(controller: Arc<Controller>, ui: F) -> Result<Handle<StudioMicTray>, ksni::Error> {
    StudioMicTray { controller, ui: Box::new(ui) }.spawn()
}
