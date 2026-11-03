//! Tab 4 Test & Optimize, and the preview bar on Tabs 2-3. Recorder and preview state come from
//! the engine (`UiPush::Recorder` / `UiPush::Preview`); One-Click Optimize and the soundcheck
//! sample the mic level here (the same `mic` peak the HTML UI used) and apply the result through
//! the Fine-Tune path.

use super::*;

const OPTIMIZE_TICKS: u32 = 25; // 2.5 s
const CHECK_TICKS: u32 = 50; // 5 s

#[derive(Default)]
pub(super) struct CheckState {
    deess_pending: Cell<bool>,
    sampler: slint::Timer,
    samples: RefCell<Vec<f32>>,
    ticks: Cell<u32>,
    /// Soundcheck result waiting for "Apply": (gate dB, de-esser amount dB).
    result: Cell<Option<(f32, f32)>>,
}

/// Value at the given fraction of the sorted samples (0.1 = quiet end, 0.95 = loud end).
fn percentile(sorted: &[f32], p: f32, default: f32) -> f32 {
    if sorted.is_empty() {
        return default;
    }
    sorted[((sorted.len() as f32 * p).floor() as usize).min(sorted.len() - 1)]
}

/// De-esser amount from how loud the voice peaks (louder voice → more sibilance energy).
fn deess_for(voice_db: f32, hot: f32) -> f32 {
    if voice_db > -12.0 {
        -6.0
    } else if voice_db > hot {
        -4.5
    } else {
        -3.5
    }
}

impl Ctx {
    pub(super) fn init_check(&self) {
        self.win.global::<Check>().set_preview_available(self.engine.preview_available());
    }

    pub(super) fn on_recorder(self: &Rc<Self>, json: &str) {
        let v: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
        let state = v["state"].as_str().unwrap_or("");
        let c = self.win.global::<Check>();
        c.set_rec_state(state.into());
        match state {
            "recording" => {
                let (el, max) = (v["elapsed"].as_f64().unwrap_or(0.0), v["max"].as_f64().unwrap_or(10.0).max(1.0));
                c.set_rec_progress((el / max) as f32);
                c.set_rec_status(format!("Recording... {}s left", (max - el).max(0.0)).into());
            }
            "done" => {
                c.set_rec_progress(1.0);
                c.set_rec_status("Recorded. Preview it on EQ or Fine-Tune.".into());
                c.set_preview_available(true);
                if self.check.deess_pending.replace(false) {
                    c.set_deess_pending(false);
                    self.ask(
                        "Test recorded. Harsh S and SH sounds are best tamed with the De-Esser in Fine-Tune, not with EQ cuts (those dull your whole voice). Open Fine-Tune and loop the recording in your headphones while you adjust?",
                        "Open Fine-Tune",
                        "ui:deess-loop",
                    );
                }
            }
            "expired" => {
                c.set_rec_progress(0.0);
                c.set_rec_status("Recording expired after 15 minutes unused.".into());
                c.set_preview_available(false);
            }
            "failed" => {
                c.set_rec_progress(0.0);
                c.set_rec_status("Recording failed: check your microphone selection.".into());
                self.check.deess_pending.set(false);
                c.set_deess_pending(false);
            }
            _ => {}
        }
    }

    pub(super) fn on_preview(&self, json: &str) {
        let v: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
        self.win.global::<Check>().set_preview_playing(v["state"].as_str() == Some("playing"));
    }

    /// Local actions behind the confirm dialog (messages starting with `ui:`).
    pub(super) fn local_action(&self, action: &str) {
        if action == "ui:deess-loop" {
            self.win.global::<Nav>().set_tab(2);
            self.post("preview:loop");
            self.win.global::<Check>().set_preview_playing(true);
        }
    }

    /// Samples the mic level every 100 ms for `ticks`, then calls `done` with the sorted samples.
    fn sample_mic(self: &Rc<Self>, ticks: u32, progress: fn(&Ctx, f32), done: fn(&Rc<Ctx>, Vec<f32>)) {
        self.check.samples.borrow_mut().clear();
        self.check.ticks.set(0);
        let w = Rc::downgrade(self);
        self.check.sampler.start(slint::TimerMode::Repeated, Duration::from_millis(100), move || {
            let Some(c) = w.upgrade() else { return };
            let db = c.peak("mic");
            if db > -95.0 {
                c.check.samples.borrow_mut().push(db);
            }
            let n = c.check.ticks.get() + 1;
            c.check.ticks.set(n);
            progress(&c, n as f32 / ticks as f32);
            if n >= ticks {
                c.check.sampler.stop();
                let mut s = std::mem::take(&mut *c.check.samples.borrow_mut());
                s.sort_by(f32::total_cmp);
                done(&c, s);
            }
        });
    }
}

pub(super) fn wire(ctx: &Rc<Ctx>) {
    let ch = ctx.win.global::<Check>();
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
    ch.on_record(cb!(|c| c.post("record:start")));
    ch.on_stop_record(cb!(|c| c.post("record:stop")));
    ch.on_preview(cb!(|c, a| {
        c.post(format!("preview:{a}"));
        if a != "stop" {
            c.win.global::<Check>().set_preview_playing(true);
        }
    }));
    ch.on_deess_test(cb!(|c| {
        c.check.deess_pending.set(true);
        c.win.global::<Check>().set_deess_pending(true);
        c.post("record:start");
    }));
    ch.on_optimize(cb!(|c| {
        let k = c.win.global::<Check>();
        k.set_optimizing(true);
        k.set_optimize_result(SharedString::new());
        c.sample_mic(
            OPTIMIZE_TICKS,
            |c, p| c.win.global::<Check>().set_optimize_progress(p),
            |c, s| {
                let floor = percentile(&s, 0.15, -50.0);
                let voice = percentile(&s, 0.90, -18.0);
                let gate = (floor + 5.0).round().clamp(-55.0, -28.0);
                let deess = deess_for(voice, -22.0);
                let makeup = ((-6.0 - voice) * 0.5).clamp(1.0, 6.5);
                c.tune_apply(|nc| {
                    nc.rumble_hp_enabled = true;
                    nc.rumble_freq = 80.0;
                    nc.hiss_lp_enabled = true;
                    nc.hiss_freq = 16000.0;
                    nc.gate_enabled = true;
                    nc.gate_threshold_db = gate;
                    nc.deesser_enabled = true;
                    nc.deesser_freq = 5800.0;
                    nc.deesser_amount_db = deess;
                    nc.compressor_enabled = true;
                    nc.compressor_ratio = 3.5;
                    nc.compressor_threshold_db = -20.0;
                    nc.compressor_makeup_db = (makeup * 2.0).round() / 2.0;
                    nc.limiter_enabled = true;
                });
                let k = c.win.global::<Check>();
                k.set_optimizing(false);
                k.set_optimize_result(
                    format!("Optimized: room floor {floor:.1} dBFS → gate {gate:.0} dB, de-esser {deess:.1} dB, makeup +{makeup:.1} dB.").into(),
                );
            },
        );
    }));
    ch.on_soundcheck(cb!(|c| {
        let k = c.win.global::<Check>();
        k.set_checking(true);
        k.set_check_done(false);
        c.sample_mic(
            CHECK_TICKS,
            |c, p| c.win.global::<Check>().set_check_progress(p),
            |c, s| {
                let floor = percentile(&s, 0.10, -55.0);
                let peak = percentile(&s, 0.95, -18.0);
                let gate = (floor + 5.0).clamp(-55.0, -25.0).round();
                let deess = deess_for(peak, -20.0);
                // The measured peak already includes the current trim: move it by the difference
                // to a -6 dBFS voice peak (the Mic fader's range is -24..+12 dB).
                let current = c.controller.state.lock().unwrap().calibration.preamp_gain_db;
                let trim = if s.is_empty() { current } else { (current + (-6.0 - peak)).clamp(-24.0, 12.0) };
                let trim = (trim * 2.0).round() / 2.0;
                c.post(format!("calib_trim:{trim:.1}"));
                c.check.result.set(Some((gate, deess)));
                let k = c.win.global::<Check>();
                k.set_check_gate(format!("{gate:.0} dB").into());
                k.set_check_deess(format!("{deess:.1} dB").into());
                k.set_check_peak(if s.is_empty() { "--".into() } else { format!("{peak:.1} dBFS").into() });
                k.set_check_trim(format!("{}{trim:.1} dB", if trim >= 0.0 { "+" } else { "" }).into());
                k.set_checking(false);
                k.set_check_done(true);
                if s.is_empty() {
                    c.notice("No signal from the microphone: check that it is plugged in and selected.");
                }
            },
        );
    }));
    ch.on_apply_soundcheck(cb!(|c| {
        if let Some((gate, deess)) = c.check.result.get() {
            c.tune_apply(|nc| {
                nc.gate_enabled = true;
                nc.gate_threshold_db = gate;
                nc.deesser_enabled = true;
                nc.deesser_freq = 5800.0;
                nc.deesser_amount_db = deess;
                nc.limiter_enabled = true;
            });
            c.notice("Soundcheck applied: gate and de-esser set. Fine-tune them on the Fine-Tune tab.");
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_and_deesser_steps() {
        let s: Vec<f32> = (0..100).map(|i| -60.0 + i as f32 * 0.5).collect();
        assert_eq!(percentile(&s, 0.10, 0.0), -55.0);
        assert_eq!(percentile(&s, 0.95, 0.0), -12.5);
        assert_eq!(percentile(&[], 0.5, -18.0), -18.0);
        assert_eq!(deess_for(-10.0, -20.0), -6.0);
        assert_eq!(deess_for(-15.0, -20.0), -4.5);
        assert_eq!(deess_for(-30.0, -20.0), -3.5);
    }
}
