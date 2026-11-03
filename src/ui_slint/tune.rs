//! Tab 3 Manual Fine-Tune: the mic chain's stages. Sends the whole `noise_clean:` config like the
//! HTML UI (so Clipguard stays in sync with `mic.clipguard` in `set_noise_clean`), plus
//! `denoiser:` / `denoiser_param:` / `gate_autoset`.

use super::*;
use crate::config::NoiseCleanConfig;

const ENGINES: [&str; 4] = ["deepfilter", "rnnoise", "speex", "off"];
const EDIT_HOLD: Duration = Duration::from_millis(1000);

#[derive(Default)]
pub(super) struct TuneState {
    /// Local copy while the user edits, so quick changes to two sliders don't overwrite each
    /// other before the backend has saved the first.
    pending: RefCell<Option<(NoiseCleanConfig, Instant)>>,
    gate_timer: slint::Timer,
}

fn set_field(nc: &mut NoiseCleanConfig, name: &str, v: f32) {
    match name {
        "rumble_freq" => nc.rumble_freq = v,
        "hiss_freq" => nc.hiss_freq = v,
        "gate_threshold_db" => nc.gate_threshold_db = v,
        "deesser_freq" => nc.deesser_freq = v,
        "deesser_threshold_db" => nc.deesser_threshold_db = v,
        "compressor_ratio" => nc.compressor_ratio = v,
        "compressor_threshold_db" => nc.compressor_threshold_db = v,
        "compressor_makeup_db" => nc.compressor_makeup_db = v,
        _ => {}
    }
}

fn toggle_field(nc: &mut NoiseCleanConfig, name: &str) {
    let f = match name {
        "rumble" => &mut nc.rumble_hp_enabled,
        "hiss" => &mut nc.hiss_lp_enabled,
        "gate" => &mut nc.gate_enabled,
        "deess" => &mut nc.deesser_enabled,
        "comp" => &mut nc.compressor_enabled,
        "limiter" => &mut nc.limiter_enabled,
        _ => return,
    };
    *f = !*f;
}

impl Ctx {
    pub(super) fn refresh_tune(&self, s: &StudioMicConfig) {
        let t = self.win.global::<Tune>();
        let pending = self.tune.pending.borrow().as_ref().filter(|(_, at)| at.elapsed() < EDIT_HOLD).map(|(nc, _)| nc.clone());
        let nc = pending.unwrap_or_else(|| s.noise_clean.clone());
        t.set_rumble_on(nc.rumble_hp_enabled);
        t.set_rumble_freq(nc.rumble_freq);
        t.set_hiss_on(nc.hiss_lp_enabled);
        t.set_hiss_freq(nc.hiss_freq);
        t.set_gate_on(nc.gate_enabled);
        t.set_gate_thr(nc.gate_threshold_db);
        t.set_deess_on(nc.deesser_enabled);
        t.set_deess_freq(nc.deesser_freq);
        t.set_deess_thr(nc.deesser_threshold_db);
        t.set_comp_on(nc.compressor_enabled);
        t.set_comp_ratio(nc.compressor_ratio);
        t.set_comp_thr(nc.compressor_threshold_db);
        t.set_comp_makeup(nc.compressor_makeup_db);
        t.set_limiter_on(nc.limiter_enabled);
        let d = &s.denoiser;
        let engine = ENGINES.iter().position(|e| *e == d.engine).unwrap_or(3);
        t.set_engine(engine as i32);
        t.set_engine_status(if engine == 3 { SharedString::new() } else { "ACTIVE".into() });
        t.set_df_atten(d.df_attenuation_db);
        t.set_rn_vad(d.rn_vad_threshold);
        t.set_rn_grace(d.rn_vad_grace_ms);
        t.set_sx_suppress(d.speex_suppress_db);
    }

    pub(super) fn refresh_tune_level(&self) {
        // The gate listens to the mic input (after the trim), shown on the -60..0 dB scale.
        let raw = self.peaks.borrow().get("raw").copied();
        let db = raw.unwrap_or_else(|| self.peak("mic"));
        self.win.global::<Tune>().set_gate_level(meter_fraction(db));
    }

    /// Applies several Fine-Tune values at once (Optimize, soundcheck) and shows them.
    pub(super) fn tune_apply(&self, f: impl FnOnce(&mut NoiseCleanConfig)) {
        self.tune_edit(f);
        self.refresh_state();
    }

    fn tune_edit(&self, f: impl FnOnce(&mut NoiseCleanConfig)) {
        let mut nc = {
            let p = self.tune.pending.borrow();
            match p.as_ref().filter(|(_, at)| at.elapsed() < EDIT_HOLD) {
                Some((nc, _)) => nc.clone(),
                None => self.controller.state.lock().unwrap().noise_clean.clone(),
            }
        };
        f(&mut nc);
        let msg = format!("noise_clean:{}", serde_json::to_string(&nc).unwrap_or_default());
        *self.tune.pending.borrow_mut() = Some((nc, Instant::now()));
        self.throttled("noise_clean", msg);
    }
}

pub(super) fn wire(ctx: &Rc<Ctx>) {
    let t = ctx.win.global::<Tune>();
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
    t.on_set(cb!(|c, name, v| c.tune_edit(|nc| set_field(nc, &name, v))));
    t.on_toggle(cb!(|c, name| {
        c.tune_edit(|nc| toggle_field(nc, &name));
        c.refresh_state(); // switches flip at once
    }));
    t.on_set_engine(cb!(|c, i| {
        if let Some(e) = ENGINES.get(i as usize) {
            c.win.global::<Tune>().set_engine(i);
            c.win.global::<Tune>().set_engine_status(if *e == "off" { SharedString::new() } else { "SWITCHING...".into() });
            c.post(format!("denoiser:{e}"));
        }
    }));
    t.on_dn_param(cb!(|c, name, v| c.throttled(format!("dn:{name}"), format!("denoiser_param:{name}:{v}"))));
    t.on_gate_autoset(cb!(|c| {
        c.post("gate_autoset");
        c.notice("Type on your keyboard without speaking for 3 seconds...");
        let t = c.win.global::<Tune>();
        t.set_gate_measuring(true);
        let w = c.win.as_weak();
        c.tune.gate_timer.start(slint::TimerMode::SingleShot, Duration::from_millis(3500), move || {
            if let Some(win) = w.upgrade() {
                win.global::<Tune>().set_gate_measuring(false);
            }
        });
    }));
    t.on_reset(cb!(|c| {
        let nc = NoiseCleanConfig::default();
        *c.tune.pending.borrow_mut() = Some((nc.clone(), Instant::now()));
        c.post(format!("noise_clean:{}", serde_json::to_string(&nc).unwrap_or_default()));
        c.refresh_state();
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fine_tune_edits_reach_the_saved_fields() {
        let mut nc = NoiseCleanConfig::default();
        set_field(&mut nc, "gate_threshold_db", -52.0);
        set_field(&mut nc, "compressor_ratio", 4.5);
        toggle_field(&mut nc, "limiter");
        assert_eq!(nc.gate_threshold_db, -52.0);
        assert_eq!(nc.compressor_ratio, 4.5);
        assert!(!nc.limiter_enabled);
        // The message body is the whole config, which the backend parses back.
        let back: NoiseCleanConfig = serde_json::from_str(&serde_json::to_string(&nc).unwrap()).unwrap();
        assert_eq!(back.gate_threshold_db, -52.0);
    }
}
