use ksni::{Tray, MenuItem, menu::*};
use ksni::blocking::{TrayMethods, Handle};
use std::sync::{Arc, Mutex};
use tao::event_loop::EventLoopProxy;
use crate::AppEvent;

#[derive(Debug, Clone)]
pub struct TrayState {
    pub mic_muted: bool,
    pub sidetone_enabled: bool,
    pub compressor_enabled: bool,
    pub deep_filter_enabled: bool,
    pub game_muted: bool,
    pub voice_muted: bool,
    pub music_muted: bool,
    pub capture_muted: bool,
    pub autostart: bool,
    pub start_minimized: bool,
}

impl Default for TrayState {
    fn default() -> Self {
        Self {
            mic_muted: false,
            sidetone_enabled: false,
            compressor_enabled: true,
            deep_filter_enabled: true,
            game_muted: false,
            voice_muted: false,
            music_muted: false,
            capture_muted: false,
            autostart: true,
            start_minimized: false,
        }
    }
}

pub struct StudioMicTray {
    pub proxy: EventLoopProxy<AppEvent>,
    pub state: Arc<Mutex<TrayState>>,
}

impl Tray for StudioMicTray {
    fn id(&self) -> String {
        "studiomic".into()
    }

    fn icon_name(&self) -> String {
        let st = self.state.lock().unwrap();
        if st.mic_muted {
            "microphone-sensitivity-muted".into()
        } else {
            "audio-input-microphone".into()
        }
    }

    fn title(&self) -> String {
        "StudioMic Broadcast Engine".into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let st = self.state.lock().unwrap();
        let mic_status = if st.mic_muted { "MUTED" } else { "LIVE" };
        let comp_status = if st.compressor_enabled { "ON" } else { "OFF" };
        let mon_status = if st.sidetone_enabled { "ON" } else { "OFF" };
        ksni::ToolTip {
            title: "StudioMic · Audio Engine".into(),
            description: format!("Mic: {} | Comp: {} | Mon: {}", mic_status, comp_status, mon_status),
            icon_name: if st.mic_muted { "microphone-sensitivity-muted".into() } else { "audio-input-microphone".into() },
            icon_pixmap: Vec::new(),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.proxy.send_event(AppEvent::ToggleWindow);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let st = self.state.lock().unwrap().clone();
        let proxy_open = self.proxy.clone();
        let proxy_mute = self.proxy.clone();
        let proxy_comp = self.proxy.clone();
        let proxy_df = self.proxy.clone();
        let proxy_side = self.proxy.clone();
        let proxy_game = self.proxy.clone();
        let proxy_voice = self.proxy.clone();
        let proxy_music = self.proxy.clone();
        let proxy_capture = self.proxy.clone();
        let proxy_start_min = self.proxy.clone();
        let proxy_auto = self.proxy.clone();
        let proxy_quit = self.proxy.clone();

        vec![
            StandardItem {
                label: "Show / Hide StudioMic".into(),
                icon_name: "window-maximize".into(),
                activate: Box::new(move |_| {
                    let _ = proxy_open.send_event(AppEvent::ToggleWindow);
                }),
                ..Default::default()
            }.into(),
            MenuItem::Separator,
            CheckmarkItem {
                label: "Mute Microphone".into(),
                icon_name: "microphone-sensitivity-muted".into(),
                checked: st.mic_muted,
                activate: Box::new(move |_| {
                    let _ = proxy_mute.send_event(AppEvent::SetMicMute(!st.mic_muted));
                }),
                ..Default::default()
            }.into(),
            CheckmarkItem {
                label: "Vocal Compressor (Auto Leveler)".into(),
                icon_name: "audio-speakers".into(),
                checked: st.compressor_enabled,
                activate: Box::new(move |_| {
                    let _ = proxy_comp.send_event(AppEvent::SetCompressor(!st.compressor_enabled));
                }),
                ..Default::default()
            }.into(),
            CheckmarkItem {
                label: "AI Noise Remover (DeepFilterNet)".into(),
                icon_name: "audio-card".into(),
                checked: st.deep_filter_enabled,
                activate: Box::new(move |_| {
                    let _ = proxy_df.send_event(AppEvent::SetDeepFilter(!st.deep_filter_enabled));
                }),
                ..Default::default()
            }.into(),
            CheckmarkItem {
                label: "Headphone Monitoring".into(),
                icon_name: "audio-headphones".into(),
                checked: st.sidetone_enabled,
                activate: Box::new(move |_| {
                    let _ = proxy_side.send_event(AppEvent::SetMonitoring(!st.sidetone_enabled));
                }),
                ..Default::default()
            }.into(),
            MenuItem::Separator,
            SubMenu {
                label: "Quick Channel Mutes".into(),
                submenu: vec![
                    CheckmarkItem {
                        label: "Mute Game Audio".into(),
                        checked: st.game_muted,
                        activate: Box::new(move |_| {
                            let _ = proxy_game.send_event(AppEvent::SetChannelMute("game".into(), !st.game_muted));
                        }),
                        ..Default::default()
                    }.into(),
                    CheckmarkItem {
                        label: "Mute Discord Voice".into(),
                        checked: st.voice_muted,
                        activate: Box::new(move |_| {
                            let _ = proxy_voice.send_event(AppEvent::SetChannelMute("voice".into(), !st.voice_muted));
                        }),
                        ..Default::default()
                    }.into(),
                    CheckmarkItem {
                        label: "Mute Music / Media".into(),
                        checked: st.music_muted,
                        activate: Box::new(move |_| {
                            let _ = proxy_music.send_event(AppEvent::SetChannelMute("music".into(), !st.music_muted));
                        }),
                        ..Default::default()
                    }.into(),
                    CheckmarkItem {
                        label: "Mute Capture Card (Video In)".into(),
                        checked: st.capture_muted,
                        activate: Box::new(move |_| {
                            let _ = proxy_capture.send_event(AppEvent::SetChannelMute("capture".into(), !st.capture_muted));
                        }),
                        ..Default::default()
                    }.into(),
                ],
                ..Default::default()
            }.into(),
            MenuItem::Separator,
            CheckmarkItem {
                label: "Start Minimized (Background Mode)".into(),
                icon_name: "window-minimize".into(),
                checked: st.start_minimized,
                activate: Box::new(move |_| {
                    let _ = proxy_start_min.send_event(AppEvent::ToggleStartMinimized(!st.start_minimized));
                }),
                ..Default::default()
            }.into(),
            CheckmarkItem {
                label: "Launch on System Startup".into(),
                icon_name: "system-run".into(),
                checked: st.autostart,
                activate: Box::new(move |_| {
                    let _ = proxy_auto.send_event(AppEvent::ToggleAutostart(!st.autostart));
                }),
                ..Default::default()
            }.into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit StudioMic".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(move |_| {
                    let _ = proxy_quit.send_event(AppEvent::Quit);
                }),
                ..Default::default()
            }.into(),
        ]
    }
}

pub fn spawn_tray(
    proxy: EventLoopProxy<AppEvent>,
    state: Arc<Mutex<TrayState>>,
) -> Result<Handle<StudioMicTray>, ksni::Error> {
    StudioMicTray { proxy, state }.spawn()
}
