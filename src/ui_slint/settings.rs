//! Settings dialog: theme, startup, Mic 2, native window by default, and profiles (named
//! snapshots of the whole state in ~/.config/studio-mic/profiles, see `config::save_profile`).

use super::*;

const THEMES: [&str; 4] = ["material_dark", "oled_black", "midnight_slate", "light"];

impl Ctx {
    pub(super) fn refresh_settings(&self, s: &StudioMicConfig) {
        let theme = if THEMES.contains(&s.theme.as_str()) { s.theme.as_str() } else { "material_dark" };
        self.win.global::<Theme>().set_name(theme.into());
        let st = self.win.global::<Settings>();
        st.set_theme(THEMES.iter().position(|t| *t == theme).unwrap_or(0) as i32);
        st.set_autostart(s.autostart);
        st.set_start_minimized(s.start_minimized);
        st.set_mic2(s.dual_mic.enabled);
        st.set_native_default(s.native_ui);
        if st.get_open() {
            self.refresh_profiles();
        }
    }

    fn refresh_profiles(&self) {
        let list = StudioMicConfig::list_profiles();
        self.win.global::<Settings>().set_profiles(Rc::new(VecModel::from(list.iter().map(SharedString::from).collect::<Vec<_>>())).into());
    }

    fn profile_at(&self, i: i32) -> Option<String> {
        self.win.global::<Settings>().get_profiles().row_data(i as usize).map(|s| s.to_string())
    }
}

pub(super) fn wire(ctx: &Rc<Ctx>) {
    let st = ctx.win.global::<Settings>();
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
    let w = Rc::downgrade(ctx);
    ctx.win.global::<Settings>().on_toggle(move |name| {
        let Some(c) = w.upgrade() else { return };
        let s = c.controller.state.lock().unwrap().clone();
        let b = |on: bool| on as u8;
        match name.as_str() {
            "autostart" => c.post(format!("toggle_autostart:{}", b(!s.autostart))),
            "minimized" => c.post(format!("toggle_start_minimized:{}", b(!s.start_minimized))),
            "mic2" => c.post(format!("dual_mic:{}", b(!s.dual_mic.enabled))),
            "native" => {
                c.post(format!("native_ui:{}", b(!s.native_ui)));
                c.notice(if s.native_ui {
                    "Studio Mic will open the classic window next time it starts."
                } else {
                    "Studio Mic will open this native window next time it starts."
                });
            }
            _ => {}
        }
    });
    st.on_opened(cb!(|c| c.refresh_profiles()));
    st.on_set_theme(cb!(|c, i| {
        if let Some(t) = THEMES.get(i as usize) {
            c.win.global::<Theme>().set_name((*t).into());
            c.post(format!("theme:{t}"));
        }
    }));
    st.on_save_profile(cb!(|c| {
        let st = c.win.global::<Settings>();
        let name = st.get_profile_name().trim().to_string();
        if name.is_empty() {
            return;
        }
        if crate::config::sanitize_profile_name(&name).is_none() {
            c.notice("Choose a profile name of 1 to 80 characters.");
            return;
        }
        c.post(format!("profile_save:{name}"));
        st.set_profile_name(SharedString::new());
        c.notice(&format!("Saved profile \"{name}\""));
    }));
    st.on_load_profile(cb!(|c, i| {
        if let Some(p) = c.profile_at(i) {
            c.ask(
                &format!("Load the profile \"{p}\"? It replaces your current settings (EQ, processing, channels, mixes); this computer's devices stay as they are."),
                "Load",
                &format!("profile_load:{p}"),
            );
        }
    }));
    st.on_delete_profile(cb!(|c, i| {
        if let Some(p) = c.profile_at(i) {
            c.ask(&format!("Delete the profile \"{p}\"?"), "Delete", &format!("profile_delete:{p}"));
        }
    }));
}
