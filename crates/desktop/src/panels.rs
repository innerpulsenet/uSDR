//! The applet column: gain, front end, the recorder, the P25 key in P25,
//! the radio and the memories — one column, each a collapsible card, so the
//! recorder is in view while its 20 seconds are still worth saving.

use crate::app::App;
use crate::net::{Fetch, Get};
use crate::theme;
use crate::widgets::{self, Fader, Tone};
use egui::{RichText, Ui, vec2};
use serde_json::{Value, json};
use usdr_proto::Mode;

fn small(t: impl Into<String>) -> RichText {
    RichText::new(t).font(theme::mono(10.5)).color(theme::text_3())
}

pub fn side(app: &mut App, ui: &mut Ui) {
    ui.spacing_mut().item_spacing.y = 8.0;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        widgets::applet(ui, "gain", "Gain", |_| {}, |ui| gain(app, ui));
        widgets::applet(ui, "frontend", "Front end", |_| {}, |ui| front_end(app, ui));
        if app.status.processing.is_some() {
            widgets::applet(ui, "processing", "Noise and AGC", |_| {}, |ui| processing(app, ui));
        }
        widgets::applet(ui, "recorder", "Recorder · last 20 s", |_| {}, |ui| recorder(app, ui));
        if app.status.mode == Mode::P25 {
            widgets::applet(ui, "p25", "P25 channel key", |_| {}, |ui| p25(app, ui));
        }
        widgets::applet(ui, "radio", "Radio", |_| {}, |ui| radio(app, ui));
        // Recall is the memory row's; the bank's storing, renaming and
        // clearing open here when wanted.
        widgets::applet_folded(ui, "memory-bank", "Memory bank", |_| {}, |ui| memories(app, ui));
    });
}

/// The channel's level over its noise floor across the 20 s the server's
/// buffer holds, so a burst worth saving can be seen before it is saved.
fn activity(app: &App, ui: &mut Ui) {
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(vec2(w, 30.0), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(r, egui::CornerRadius::same(3), theme::ink());
    let now = ui.input(|i| i.time);
    let x_at = |t: f64| r.right() - ((now - t) / 20.0).clamp(0.0, 1.0) as f32 * r.width();
    // 0–40 dB over the floor, bottom to top.
    let y_at = |snr: f32| r.bottom() - 2.0 - (snr / 40.0).clamp(0.0, 1.0) * (r.height() - 4.0);
    let pts: Vec<egui::Pos2> = app.activity.iter().map(|&(t, ch, nz)| egui::pos2(x_at(t), y_at(ch - nz))).collect();
    if pts.len() > 1 {
        crate::spectrum::fill_under(p, &pts, r.bottom(), theme::AMBER, 0.5);
        p.add(egui::Shape::line(pts, egui::Stroke::new(1.0, theme::AMBER)));
    } else {
        p.text(r.center(), egui::Align2::CENTER_CENTER, "nothing measured on this channel yet", theme::mono(10.5), theme::text_3());
    }
    ui.horizontal(|ui| {
        ui.label(small("−20 s"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(small("now"));
            ui.add_space(ui.available_width() / 2.0 - 60.0);
            ui.label(small("level over the floor"));
        });
    });
}

/// The main display's trace across the passband, for the scope.
pub fn passband_slice(app: &App) -> Option<(Vec<f32>, (f32, f32))> {
    let f = app.frame.as_ref()?;
    let n = f.pwr.len();
    if n < 4 {
        return None;
    }
    let bw = if app.status.bandwidth_hz > 0.0 { app.status.bandwidth_hz } else { 12_500.0 };
    let lo = f.center_hz - f.rate_hz / 2.0;
    let a = (((f.inspect_hz - bw / 2.0 - lo) / f.rate_hz) * (n - 1) as f64).round().clamp(0.0, (n - 1) as f64) as usize;
    let b = (((f.inspect_hz + bw / 2.0 - lo) / f.rate_hz) * (n - 1) as f64).round().clamp(0.0, (n - 1) as f64) as usize;
    if b <= a + 2 {
        return None;
    }
    Some((f.pwr[a..=b].to_vec(), app.pan.range()))
}

fn memories(app: &mut App, ui: &mut Ui) {
    let armed = app.memory_armed;
    if widgets::key(ui, "STORE", armed, theme::AMBER).on_hover_text("Then pick a slot; Shift-click a slot also stores").clicked() {
        app.memory_armed = !armed;
    }
    let list = app.memories();
    let inspect = app.inspect_hz();
    let mut store_at: Option<usize> = None;
    let mut delete: Option<usize> = None;
    let w = (ui.available_width() - 3.0 * 4.0) / 4.0;
    egui::Grid::new("mem").spacing(vec2(4.0, 4.0)).show(ui, |ui| {
        let slots = list.len().max(8).div_ceil(4) * 4;
        for i in 0..slots {
            let m = list.get(i);
            let label = m.map_or("—".to_string(), |m| m.name.clone());
            let lit = m.is_some_and(|m| {
                m.mode == app.status.mode && (m.freq_hz - inspect).abs() <= (crate::tuning::mode_raster_hz(m.mode, m.freq_hz) / 2.0).max(1000.0)
            });
            let r = widgets::key_sized(ui, &label, lit, theme::AMBER, vec2(w, 26.0));
            let r = match m {
                Some(m) => r.on_hover_text(format!("{} MHz · {} · Ctrl+{}", crate::tuning::fmt_mhz(m.freq_hz, 5), m.mode.label(), i + 1)),
                None => r.on_hover_text("Empty: STORE, then this slot"),
            };
            if r.clicked() {
                if app.memory_armed || ui.input(|i| i.modifiers.shift) {
                    store_at = Some(i);
                } else if let Some(m) = m {
                    let (hz, mode) = (m.freq_hz, m.mode);
                    app.tune(hz, true);
                    if mode != app.status.mode {
                        app.set_mode(mode);
                    }
                }
            }
            r.context_menu(|ui| {
                if ui.button("Store the current channel here").clicked() {
                    store_at = Some(i);
                    ui.close();
                }
                if m.is_some() && ui.button("Rename…").clicked() {
                    app.renaming = Some((i, m.map(|m| m.name.clone()).unwrap_or_default()));
                    ui.close();
                }
                if m.is_some() && ui.button("Clear").clicked() {
                    delete = Some(i);
                    ui.close();
                }
            });
            if i % 4 == 3 {
                ui.end_row();
            }
        }
    });
    ui.label(small(if app.memory_armed {
        format!("pick a slot for {} {}", crate::tuning::fmt_mhz(inspect, 4), app.status.mode.label())
    } else if app.memories.is_some() {
        "shared with every client of this server".into()
    } else {
        "defaults · storing one shares the bank with every client".into()
    }));
    // Renaming a slot, in place under the bank.
    let mut rename_done: Option<(usize, String)> = None;
    if let Some((i, name)) = &mut app.renaming {
        let i = *i;
        ui.horizontal(|ui| {
            ui.label(small(format!("slot {}", i + 1)));
            let r = ui.add(egui::TextEdit::singleline(name).char_limit(24).desired_width(140.0).font(theme::mono(11.5)));
            r.request_focus();
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) || widgets::action(ui, "OK").clicked() {
                rename_done = Some((i, name.trim().to_string()));
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                rename_done = Some((usize::MAX, String::new()));
            }
        });
    }
    if let Some((i, name)) = rename_done {
        app.renaming = None;
        let mut l = list.clone();
        if let Some(m) = l.get_mut(i).filter(|_| !name.is_empty()) {
            m.name = name;
            app.save_memories(l);
        }
    }
    if let Some(i) = store_at {
        let mut l = list.clone();
        let m = crate::prefs::Memory { name: app.status.mode.label().into(), freq_hz: inspect.round(), mode: app.status.mode };
        if i < l.len() {
            l[i] = m;
        } else {
            l.push(m);
        }
        app.memory_armed = false;
        app.save_memories(l);
    }
    if let Some(i) = delete {
        let mut l = list;
        if i < l.len() {
            l.remove(i);
            app.save_memories(l);
        }
    }
}

fn gain(app: &mut App, ui: &mut Ui) {
    let s = app.status.clone();
    let auto = s.gain_db.is_none();
    let (lo, hi) = if s.gain_max_db > s.gain_min_db { (s.gain_min_db.floor(), s.gain_max_db.ceil()) } else { (0.0, 50.0) };
    let shown = if auto { s.gain_now_db.unwrap_or(app.gain_ui) } else { app.gain_ui };
    let mut v = shown;
    let ticks = (0..=4).map(|i| (i as f32 / 4.0, format!("{:.0}", lo + (hi - lo) * i as f64 / 4.0))).collect();
    let (ch, _) = Fader {
        value: &mut v,
        min: lo,
        max: hi,
        step: 1.0,
        fine: 1.0,
        label: "RF GAIN",
        text: |v| if auto { format!("auto · {shown:.0} dB") } else { format!("{v:.0} dB") },
        color: theme::AMBER,
        enabled: !auto,
        ticks,
    }
    .show(ui);
    if ch {
        app.gain_ui = v;
        app.post("/api/sdr/gain", json!({ "gain_db": v }));
    }
    if s.rfgr_db.is_some() || s.ifgr_db.is_some() {
        ui.label(small(format!(
            "LNA state {} · IF reduction {} dB",
            s.rfgr_db.map_or("—".into(), |v| format!("{v:.0}")),
            s.ifgr_db.map_or("—".into(), |v| format!("{v:.0}"))
        )));
    }
    if let Some(sp) = s.agc_setpoint_dbfs {
        let mut v = sp;
        let active = s.tuner_agc && auto;
        let (ch, _) = Fader {
            value: &mut v,
            min: -60.0,
            max: 0.0,
            step: 1.0,
            fine: 1.0,
            label: "IF AGC TARGET",
            text: |v| format!("{v:.0} dBFS"),
            color: theme::CYAN,
            enabled: active,
            ticks: vec![],
        }
        .show(ui);
        if ch {
            app.post("/api/sdr/frontend", json!({ "agc_setpoint": v }));
        }
    }
    ui.horizontal_wrapped(|ui| {
        if widgets::key(ui, "AGC", auto, theme::OK).on_hover_text("Automatic gain (clip-guarded loop or the tuner's own AGC)").clicked() {
            let body = if auto { json!({ "gain_db": app.gain_ui }) } else { json!({ "gain_db": null }) };
            app.post("/api/sdr/gain", body);
        }
        let agc_name = if s.is_sdrplay() { "IF AGC" } else { "TUNER AGC" };
        if widgets::key(ui, agc_name, s.tuner_agc, theme::CYAN).on_hover_text("With AGC: hand gain to the radio's own loop").clicked() {
            app.post("/api/sdr/frontend", json!({ "tuner_agc": !s.tuner_agc }));
        }
        if widgets::key(ui, "CLIP GUARD", s.clip_guard, theme::AMBER).on_hover_text("Let the guard back gain off when the ADC clips").clicked() {
            app.post("/api/sdr/frontend", json!({ "clip_guard": !s.clip_guard }));
        }
    });
}

fn front_end(app: &mut App, ui: &mut Ui) {
    let s = app.status.clone();
    ui.horizontal_wrapped(|ui| {
        if let Some(on) = s.rf_notch
            && widgets::key(ui, "AM/FM NOTCH", on, theme::VIOLET).clicked()
        {
            app.post("/api/sdr/frontend", json!({ "rf_notch": !on }));
        }
        if let Some(on) = s.dab_notch
            && widgets::key(ui, "DAB NOTCH", on, theme::VIOLET).clicked()
        {
            app.post("/api/sdr/frontend", json!({ "dab_notch": !on }));
        }
        let lo = if s.lo_offset { format!("LO {:+}", crate::tuning::fmt_short_hz(s.lo_offset_hz)) } else { "LO OFFSET".into() };
        if widgets::key(ui, &lo, s.lo_offset, theme::CYAN).on_hover_text("Park the tuner off-center so its DC spur stays out of the channel").clicked() {
            app.post("/api/sdr/frontend", json!({ "lo_offset": !s.lo_offset }));
        }
        if let Some(on) = s.iq_correction
            && widgets::key(ui, "IQ CORR", on, theme::CYAN).clicked()
        {
            app.post("/api/sdr/frontend", json!({ "iq_correction": !on }));
        }
        if let Some(on) = s.bias_t
            && widgets::key(ui, "BIAS-T", on, theme::WARN).on_hover_text("DC on the antenna feed, for an inline preamp; saved per radio").clicked()
        {
            app.post("/api/sdr/frontend", json!({ "bias_t": !on }));
        }
        let spur_on = !s.spurs_hz.is_empty();
        let checking = app.spur.is_some();
        let label = if checking { "CHECKING…".to_string() } else if spur_on { format!("SPUR NOTCH {}", s.spurs_hz.len()) } else { "SPUR NOTCH".into() };
        if widgets::key(ui, &label, spur_on || checking, theme::VIOLET)
            .on_hover_text("Find the receiver's own spurs by moving the span an eighth and back, then notch them. Click again to clear.")
            .clicked()
            && !checking
        {
            let now = ui.input(|i| i.time);
            app.start_spur_check(now);
        }
    });
    if !s.if_bandwidths_hz.is_empty() {
        let cur = s.if_bandwidth_hz.unwrap_or(0.0);
        let label = if s.if_bandwidth_manual { crate::tuning::fmt_hz(cur) } else { format!("AUTO · {}", crate::tuning::fmt_hz(cur)) };
        ui.horizontal(|ui| {
            ui.label(small("IF FILTER"));
            widgets::selector(ui, "iffilter", &label, theme::text(), 0.0, |ui| {
                if ui.selectable_label(!s.if_bandwidth_manual, "Auto · follow the span").clicked() {
                    app.post("/api/sdr/frontend", json!({ "if_bandwidth": 0 }));
                    ui.close();
                }
                for w in s.if_bandwidths_hz.iter().filter(|w| **w > 0.0 && **w <= s.rate_hz * 1.001) {
                    if ui.selectable_label(s.if_bandwidth_manual && (*w - cur).abs() < 1.0, crate::tuning::fmt_hz(*w)).clicked() {
                        app.post("/api/sdr/frontend", json!({ "if_bandwidth": w }));
                        ui.close();
                    }
                }
            });
        });
    }
    let n = [s.rf_notch, s.dab_notch, s.iq_correction, s.bias_t].iter().filter(|v| **v == Some(true)).count() + usize::from(s.lo_offset);
    ui.label(small(format!("{} · {}", s.tuner.trim_start_matches("Rafael Micro "), if n == 0 { "all off".into() } else { format!("{n} on") })));
}

fn processing(app: &mut App, ui: &mut Ui) {
    let Some(p) = app.status.processing.clone() else { return };
    let mode = app.status.mode;
    let voice = matches!(mode, Mode::Nfm | Mode::Am | Mode::Usb | Mode::Lsb);
    let row = |ui: &mut Ui, label: &str, hint: &str, add: &mut dyn FnMut(&mut Ui)| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            let r = ui.label(small(label)).on_hover_text(hint);
            ui.add_space((78.0 - r.rect.width()).max(0.0));
            add(ui);
        });
    };
    let mut post = None;
    row(ui, "NOISE RED.", "Spectral noise reduction on voice: NFM, AM, USB and LSB. Higher cuts more hiss and adds about 16 ms of delay.", &mut |ui| {
        for (v, l) in [("off", "OFF"), ("low", "LOW"), ("medium", "MED"), ("high", "HIGH")] {
            if widgets::chip_key(ui, l, p.nr == v, theme::CYAN).clicked() {
                post = Some(json!({ "nr": v }));
            }
        }
    });
    row(ui, "BLANKER", "Wideband impulse blanker, for ignition and power-line clicks. It also clips the start of strong packet bursts, so leave it off for APRS.", &mut |ui| {
        for (v, l) in [(0u8, "OFF"), (1, "LOW"), (2, "MED"), (3, "HIGH")] {
            if widgets::chip_key(ui, l, p.blanker == v, theme::VIOLET).clicked() {
                post = Some(json!({ "blanker": v }));
            }
        }
    });
    row(ui, "AGC", "AM and sideband AGC speed: fast follows flutter, slow rides through pauses and fades.", &mut |ui| {
        for (v, l) in [("fast", "FAST"), ("medium", "MED"), ("slow", "SLOW")] {
            if widgets::chip_key(ui, l, p.agc == v, theme::AMBER).clicked() {
                post = Some(json!({ "agc": v }));
            }
        }
    });
    if widgets::key(ui, "AUTO NOTCH", p.auto_notch, theme::CYAN)
        .on_hover_text("Find and null steady tones (heterodynes, carriers) in voice audio")
        .clicked()
    {
        post = Some(json!({ "auto_notch": !p.auto_notch }));
    }
    if let Some(body) = post {
        app.post("/api/sdr/processing", body);
    }
    let note = if voice {
        "applies to this mode's audio"
    } else if mode == Mode::Wfm {
        "WFM: broadcast audio is left as it is"
    } else {
        "noise reduction and AGC act on NFM, AM, USB and LSB"
    };
    ui.label(small(note));
}

fn recorder(app: &mut App, ui: &mut Ui) {
    let kinds: Vec<String> = app.mode_caps().map(|c| c.capture.clone()).unwrap_or_else(|| vec!["iq".into(), "voice".into(), "discriminator".into()]);
    let has = |k: &str| app.modes.is_empty() || kinds.iter().any(|x| x == k);
    let receiving = app.status.error.is_none() && app.status.rate_hz > 0.0 && !kinds.is_empty();
    ui.horizontal(|ui| {
        widgets::chip(ui, if receiving { Tone::Ok } else { Tone::Idle }, if receiving { "FILLING" } else { "OFF" });
        let head = if !receiving {
            format!("{} keeps no buffer", app.status.mode.label())
        } else {
            format!("{} · {}S/s", crate::tuning::fmt_mhz(app.inspect_hz(), 3), crate::tuning::fmt_short_hz(app.status.inspect_rate_hz))
        };
        ui.label(small(head));
    });
    if receiving {
        activity(app, ui);
    }
    let dir = app.prefs.record_dir();
    let busy = app.recorder_busy.clone();
    ui.horizontal_wrapped(|ui| {
        for (kind, label, tip) in [("iq", "SAVE I/Q", "CS16LE + JSON sidecar"), ("voice", "SAVE AUDIO", "voice WAV"), ("discriminator", "SAVE DISC", "discriminator WAV")] {
            ui.add_enabled_ui(has(kind) && busy.is_none(), |ui| {
                if widgets::action(ui, label).on_hover_text(tip).clicked()
                    && let Some(l) = &app.link
                {
                    let k = if kind == "discriminator" { "disc" } else { kind };
                    l.fetch(&format!("/api/sdr/capture.wav?kind={kind}"), Fetch::SaveCapture { kind: k.into(), stem: dir.join("x") });
                    app.recorder_busy = Some(kind.into());
                }
            });
        }
    });
    ui.horizontal_wrapped(|ui| {
        let playing = app.player.clip_progress().filter(|_| app.playing == Some(crate::app::Playing::Capture));
        if let Some(p) = playing {
            if widgets::action(ui, "■ STOP").on_hover_text("Stop and return to the live audio").clicked() {
                app.player.stop_clip();
                app.playing = None;
            }
            crate::dock::progress_bar(ui, p, 60.0);
        } else {
            ui.add_enabled_ui(has("voice") && busy.is_none(), |ui| {
                if widgets::action(ui, "PLAY 20 S").on_hover_text("Hear the last 20 s again, in place of the live audio").clicked()
                    && let Some(l) = &app.link
                {
                    l.fetch("/api/sdr/capture.wav?kind=voice", Fetch::PlayCapture);
                    app.recorder_busy = Some("play".into());
                }
            });
        }
        ui.add_enabled_ui(has("iq") && busy.is_none(), |ui| {
            if widgets::action(ui, "REPLAY I/Q").on_hover_text("Run the last 20 s of I/Q through the decoders again").clicked()
                && let Some(l) = &app.link
            {
                l.fetch("/api/sdr/capture.wav?kind=iq", Fetch::ReplayCapture);
                app.recorder_busy = Some("replay".into());
            }
        });
    });
    if let Some(b) = &busy {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(12.0).color(theme::AMBER));
            ui.label(small(format!("{b}…")));
        });
    }
    if let Some(n) = &app.replay_note {
        ui.label(small(n.clone()));
    }
    ui.horizontal_wrapped(|ui| {
        if widgets::action(ui, "LOAD I/Q…").on_hover_text("Decode a .iq (with its .json) or an I/Q WAV on the server").clicked() {
            crate::app::pick(app, "replay");
        }
        if widgets::action(ui, "FOLDER…").on_hover_text("Where saves and recordings go").clicked() {
            crate::app::pick(app, "folder");
        }
    });
    ui.label(small(format!("Saves to {} · or drop a .iq + .json on the window", dir.display())));
}

fn radio(app: &mut App, ui: &mut Ui) {
    let s = app.status.clone();
    ui.horizontal(|ui| {
        ui.label(small("DEVICE"));
        let cur = app.devices.iter().find(|d| d.serial == s.serial).map(device_label).unwrap_or_else(|| format!("{} · {}", s.serial, s.radio_label()));
        // The selector gets what the label and the refresh key leave, in a
        // space of its own: a truncating combo box fills whatever room its
        // row has, and a row wider than the column shifts every card in it.
        let w = (ui.available_width() - 34.0).max(100.0);
        widgets::selector(ui, "device", &cur, theme::text(), w, |ui| {
            for d in app.devices.clone() {
                if ui.selectable_label(d.serial == s.serial, device_label(&d)).clicked() {
                    if d.serial != s.serial {
                        app.post("/api/sdr/device", json!({ "serial": d.serial }));
                    }
                    ui.close();
                }
            }
        });
        if crate::app::icon_button(ui, "↻", "Look for radios again").clicked()
            && let Some(l) = &app.link
        {
            l.get("/api/devices", Get::Devices);
        }
    });
    ui.horizontal(|ui| {
        ui.label(small("PPM"));
        let mut ppm = s.ppm;
        let r = widgets::numeric(ui, &mut ppm, -200.0..=200.0, 0.05, 2, 76.0);
        if r.changed() {
            app.post("/api/sdr/ppm", json!({ "ppm": ppm }));
            app.status.ppm = ppm;
        }
        if widgets::action(ui, "CALIBRATE").on_hover_text("Measure the crystal error from the carrier you are tuned to").clicked()
            && let Some(l) = &app.link
        {
            l.post_get("/api/sdr/calibrate", json!({}), Get::Calibrate);
        }
    });
    let err = s.freq_error_hz.map_or("no carrier measured".into(), |e| format!("carrier error {e:+.0} Hz"));
    ui.label(small(format!("{err} · tune {}–{}", crate::tuning::fmt_hz(s.tune_min_hz), crate::tuning::fmt_hz(s.tune_max_hz))));
}

fn device_label(d: &usdr_proto::Device) -> String {
    let mut s = format!("{} · {}", d.serial, d.tuner);
    if let Some(h) = &d.host {
        s.push_str(&format!(" @ {h}"));
    }
    if d.role != "free" && !d.role.is_empty() {
        s.push_str(&format!(" ({})", d.role));
    }
    s
}

// ---------- P25 keys ----------

#[derive(Default)]
pub struct P25Form {
    pub freq: u64,
    pub requested: u64,
    pub entries: Vec<(f64, u8, u16)>,
    pub configured: bool,
    pub algorithm: u8,
    pub key_id: String,
    pub key: String,
    pub message: Option<(Tone, String)>,
}

impl P25Form {
    pub fn load(&mut self, v: Value) {
        self.configured = v.get("configured").and_then(Value::as_bool).unwrap_or(false);
        self.entries = v
            .get("entries")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|e| {
                        (
                            e.get("frequency_hz").and_then(Value::as_f64).unwrap_or(0.0),
                            e.get("algorithm").and_then(Value::as_u64).unwrap_or(0) as u8,
                            e.get("key_id").and_then(Value::as_u64).unwrap_or(0) as u16,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some(m) = v.get("message").and_then(Value::as_str) {
            self.message = Some((Tone::Ok, m.into()));
        }
    }
}

const ALGS: [(u8, &str, usize); 4] = [(170, "Motorola ADP / RC4", 10), (129, "DES-OFB", 16), (132, "AES-256-OFB", 64), (137, "AES-128-OFB", 32)];

fn alg_name(a: u8) -> String {
    match a {
        170 => "ADP".into(),
        129 => "DES-OFB".into(),
        132 => "AES-256".into(),
        137 => "AES-128".into(),
        x => format!("ALG 0x{x:02X}"),
    }
}

fn p25(app: &mut App, ui: &mut Ui) {
    let freq = app.inspect_hz().round() as u64;
    if app.p25.requested != freq
        && let Some(l) = &app.link
    {
        app.p25.requested = freq;
        app.p25.freq = freq;
        l.get(&format!("/api/sdr/p25/key?frequency_hz={freq}"), Get::P25Key);
    }
    let f = &mut app.p25;
    if f.algorithm == 0 {
        f.algorithm = 132;
    }
    ui.label(small(format!(
        "{} MHz · {}",
        crate::tuning::fmt_mhz(freq as f64, 6),
        if f.configured { "a key is configured" } else { "no key for this channel" }
    )));
    let alg_label = ALGS.iter().find(|a| a.0 == f.algorithm).map(|a| a.1).unwrap_or("?");
    widgets::selector(ui, "p25alg", alg_label, theme::text(), 0.0, |ui| {
        for (v, name, _) in ALGS {
            if ui.selectable_value(&mut f.algorithm, v, name).clicked() {
                ui.close();
            }
        }
    });
    let need = ALGS.iter().find(|a| a.0 == f.algorithm).map(|a| a.2).unwrap_or(64);
    ui.horizontal(|ui| {
        ui.label(small("KEY ID"));
        ui.add(egui::TextEdit::singleline(&mut f.key_id).desired_width(70.0).hint_text("hex").font(theme::mono(11.5)));
    });
    ui.add(egui::TextEdit::singleline(&mut f.key).password(true).hint_text(format!("{need} hex digits")).font(theme::mono(11.5)).desired_width(f32::INFINITY));
    let mut post: Option<Value> = None;
    ui.horizontal(|ui| {
        if widgets::action(ui, "SAVE KEY").clicked() {
            let kid = f.key_id.trim().trim_start_matches("0x").trim_start_matches("0X");
            let key: String = f.key.trim().chars().filter(|c| !c.is_whitespace()).collect();
            if kid.is_empty() || kid.len() > 4 || u16::from_str_radix(kid, 16).is_err() {
                f.message = Some((Tone::Err, "Key ID is 1–4 hex digits".into()));
            } else if key.len() != need || !key.chars().all(|c| c.is_ascii_hexdigit()) {
                f.message = Some((Tone::Err, format!("This algorithm takes {need} hex digits")));
            } else {
                post = Some(json!({
                    "frequency_hz": freq,
                    "action": "save",
                    "entry": { "algorithm": f.algorithm, "key_id": u16::from_str_radix(kid, 16).unwrap_or(0), "key": key }
                }));
            }
            // The key leaves the form the moment it is used.
            f.key.clear();
        }
        if f.configured && widgets::action(ui, "REMOVE").clicked() {
            post = Some(json!({ "frequency_hz": freq, "action": "remove" }));
        }
    });
    if let Some((tone, m)) = &f.message {
        widgets::chip(ui, *tone, m);
    }
    for (hz, alg, kid) in &f.entries {
        let cur = (*hz - freq as f64).abs() < 1.0;
        ui.label(RichText::new(format!("{} MHz · {} · KID 0x{kid:04X}", crate::tuning::fmt_mhz(*hz, 6), alg_name(*alg))).font(theme::mono(10.5)).color(if cur { theme::AMBER } else { theme::text_3() }));
    }
    if let (Some(body), Some(l)) = (post, &app.link) {
        l.post_get("/api/sdr/p25/key", body, Get::P25Key);
        app.p25.message = Some((Tone::Idle, "sent".into()));
    }
}
