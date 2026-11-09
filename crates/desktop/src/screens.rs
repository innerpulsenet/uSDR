//! The screens around the console: connecting, settings, shortcuts.

use crate::app::{App, Conn};
use crate::brand;
use crate::theme::{self, alpha};
use crate::widgets::{self, Tone};
use egui::{Align2, RichText, Ui, pos2, vec2};

pub fn connect_screen(app: &mut App, ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    let full = ui.max_rect();
    let rm = app.reduce_motion();
    // The receive splash in a window across the lower part of the plate.
    let splash = egui::Rect::from_min_max(pos2(full.left() + 12.0, full.top() + full.height() * 0.42), pos2(full.right() - 12.0, full.bottom() - 34.0));
    theme::well(ui.painter(), splash, None);
    app.splash.paint(ui, splash, rm);

    let card_w = 440.0;
    let card = egui::Rect::from_center_size(pos2(full.center().x, full.top() + full.height() * 0.30), vec2(card_w, 330.0));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(card));
    egui::Frame::new()
        .fill(alpha(theme::plate(), 0.98))
        .stroke(egui::Stroke::new(1.0, theme::line_2()))
        .corner_radius(egui::CornerRadius::same(crate::theme::R_WINDOW))
        .inner_margin(egui::Margin::same(24))
        .shadow(egui::Shadow { offset: [0, 18], blur: 40, spread: 0, color: egui::Color32::from_black_alpha(160) })
        .show(&mut child, |ui| {
            ui.set_width(card_w - 48.0);
            ui.horizontal(|ui| {
                let connecting = matches!(app.conn, Conn::Connecting { .. });
                // Beat at the working tempo while connecting; rest otherwise.
                brand::mark(ui, 56.0, if connecting { 2.4 } else { 4.5 }, rm);
                ui.vertical(|ui| {
                    ui.add_space(4.0);
                    theme::steel_label(ui, "uSDR", theme::wordmark(34.0));
                    ui.label(RichText::new("Receiver console for a uSDR server").font(theme::sans(13.0)).color(theme::text_2()));
                });
            });
            ui.add_space(14.0);
            ui.label(RichText::new("SERVER").font(theme::mono(crate::theme::ty::LABEL)).color(theme::text_3()));
            let resp = ui.add(
                egui::TextEdit::singleline(&mut app.connect_input)
                    .font(theme::mono(14.0))
                    .hint_text("host:8073")
                    .desired_width(f32::INFINITY),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.add_space(4.0);
            let recent: Vec<String> = app.prefs.servers.iter().filter(|s| **s != app.connect_input).take(4).cloned().collect();
            if !recent.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("recent").font(theme::mono(crate::theme::ty::LABEL)).color(theme::text_3()));
                    for s in recent {
                        if ui.add(egui::Button::new(RichText::new(&s).font(theme::mono(crate::theme::ty::SMALL))).small()).clicked() {
                            app.connect_input = s;
                        }
                    }
                });
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let busy = matches!(app.conn, Conn::Connecting { .. });
                if busy {
                    if widgets::action(ui, "CANCEL").clicked() {
                        app.disconnect();
                    }
                } else if widgets::primary(ui, "Connect").clicked() || enter {
                    let s = app.connect_input.clone();
                    app.connect(&ctx, &s);
                }
                ui.checkbox(&mut app.prefs.auto_connect, RichText::new("Connect on start").font(theme::sans(crate::theme::ty::BODY)));
            });
            ui.add_space(8.0);
            match &app.conn {
                Conn::Connecting { attempt, reason } => {
                    let text = match reason {
                        Some(r) => format!("{r} · retrying"),
                        None => format!("connecting · attempt {attempt}"),
                    };
                    ui.label(RichText::new(text).font(theme::mono(crate::theme::ty::SMALL)).color(theme::WARN));
                }
                Conn::Idle => {
                    ui.label(RichText::new("The radio stays with the server; this app streams its spectrum, audio and decodes.").font(theme::sans(crate::theme::ty::BODY)).color(theme::text_3()));
                }
                Conn::Live => {
                    ui.label(RichText::new("waiting for the receiver").font(theme::mono(crate::theme::ty::SMALL)).color(theme::text_2()));
                }
            }
        });
    // Settings and the credit stay reachable before connecting.
    let br = egui::Rect::from_min_max(pos2(full.right() - 220.0, full.bottom() - 30.0), full.max);
    let mut c = ui.new_child(egui::UiBuilder::new().max_rect(br).layout(egui::Layout::right_to_left(egui::Align::Center)));
    c.add_space(12.0);
    brand::credit(&mut c);
    if crate::app::icon_button(&mut c, "⚙", "Settings").clicked() {
        app.settings_open = !app.settings_open;
    }
}

pub fn reconnect_veil(app: &mut App, ctx: &egui::Context) {
    let Conn::Connecting { reason, .. } = &app.conn else { return };
    let text = reason.clone().unwrap_or_else(|| "connecting".into());
    let rm = app.reduce_motion();
    egui::Area::new("reconnect".into())
        .anchor(Align2::CENTER_CENTER, vec2(-140.0, -60.0))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(alpha(theme::plate(), 0.97))
                .stroke(egui::Stroke::new(1.0, alpha(theme::WARN, 0.5)))
                .corner_radius(egui::CornerRadius::same(crate::theme::R_WINDOW))
                .inner_margin(egui::Margin::symmetric(16, 10))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        brand::mark(ui, 48.0, 2.4, rm);
                        ui.vertical(|ui| {
                            ui.label(RichText::new("Reconnecting").font(theme::headline(15.0)).color(theme::text()));
                            ui.label(RichText::new(text).font(theme::mono(crate::theme::ty::SMALL)).color(theme::text_2()));
                            ui.label(RichText::new("The display below is the last frame received.").font(theme::sans(crate::theme::ty::VALUE)).color(theme::text_3()));
                        });
                    });
                });
        });
}

pub fn settings_window(app: &mut App, ctx: &egui::Context) {
    crate::settings::window(app, ctx);
}

pub fn help_window(app: &mut App, ctx: &egui::Context) {
    if !app.help_open {
        return;
    }
    let mut open = true;
    egui::Window::new(RichText::new("Shortcuts and gestures").font(theme::headline(16.0)))
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
        .show(ctx, |ui| {
            let rows: &[(&str, &str)] = &[
                ("Click the display", "tune, snapped to the step (Alt: exact)"),
                ("Shift-click", "move the span there; the receiver stays"),
                ("Double-click", "tune and center"),
                ("Drag the display", "pan the span"),
                ("Wheel over the display", "tune by the step (Alt: a tenth of the span)"),
                ("Ctrl + wheel", "zoom"),
                ("Wheel or drag the dB scale", "set the range by hand"),
                ("Drag the bar under the spectrum", "resize spectrum and waterfall"),
                ("Right-click the display", "tune, center, store, copy, display options"),
                ("Wheel or click a digit", "turn that digit (upper half up, lower half down)"),
                ("Double-click the readout, Space, or type", "enter a frequency: 146.52 · 146520k · 7.2m"),
                ("↑ ↓ / ← →", "turn / select the readout digit"),
                ("PgUp PgDn (Shift ×10)", "step"),
                ("+ / −", "zoom in / out"),
                ("C", "center the span on the receiver"),
                ("L", "span lock: tuning moves only the receiver"),
                ("H, or HOLD on the waterfall", "hold the waterfall; the wheel scrolls back five minutes"),
                ("Middle-click, or right-click › Put receiver B here", "start or move receiver B"),
                ("Click a receiver's flag, or B", "choose which receiver the gestures drive"),
                ("M", "mute the receiver being driven"),
                ("R", "record what the receiver being driven plays"),
                ("Ctrl+1 … 9", "recall a memory"),
                ("S / D", "show or hide the right column / the dock"),
                ("F11", "full screen"),
                ("F9", "roll up to a strip (shade)"),
                ("Ctrl+,", "settings"),
                ("Drop a .iq + .json", "decode it on the server"),
            ];
            egui::Grid::new("help").num_columns(2).spacing(vec2(18.0, 6.0)).show(ui, |ui| {
                for (k, v) in rows {
                    ui.label(RichText::new(*k).font(theme::mono(crate::theme::ty::VALUE)).color(theme::AMBER));
                    ui.label(RichText::new(*v).color(theme::text_2()));
                    ui.end_row();
                }
            });
            if !app.prefs.shortcuts {
                widgets::chip(ui, Tone::Warn, "keyboard shortcuts are off in Settings");
            }
        });
    if !open {
        app.help_open = false;
    }
}

/// What the link and the receiver are doing, in numbers, with the last two
/// minutes drawn as sparklines.
pub fn diagnostics_window(app: &mut App, ctx: &egui::Context) {
    if !app.diagnostics_open {
        return;
    }
    let mut open = true;
    egui::Window::new(RichText::new("Connection diagnostics").font(theme::headline(16.0)))
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .default_width(440.0)
        .show(ctx, |ui| {
            let mono = |t: String| RichText::new(t).font(theme::mono(crate::theme::ty::VALUE)).color(theme::text());
            let label = |t: &str| RichText::new(t).font(theme::sans(crate::theme::ty::BODY)).color(theme::text_3());
            egui::Grid::new("diag").num_columns(2).spacing(vec2(16.0, 4.0)).show(ui, |ui| {
                ui.label(label("Server"));
                ui.label(mono(match (&app.link, &app.server) {
                    (Some(l), Some(i)) => format!("{} · {} {} · API {}", l.base, i.name, i.version, i.api),
                    (Some(l), None) => format!("{} · before API 2", l.base),
                    _ => "not connected".into(),
                }));
                ui.end_row();
                if let Some(i) = &app.server {
                    ui.label(label("Features"));
                    ui.label(mono(i.features.join(", ")));
                    ui.end_row();
                }
                ui.label(label("Connected for"));
                ui.label(mono(app.connected_at.map_or("—".into(), |t| {
                    let s = t.elapsed().as_secs();
                    format!("{}:{:02}:{:02} · {} reconnect(s)", s / 3600, (s / 60) % 60, s % 60, app.reconnects)
                })));
                ui.end_row();
                if let Some(l) = &app.link {
                    let st = &l.stats;
                    use std::sync::atomic::Ordering::Relaxed;
                    ui.label(label("Received"));
                    ui.label(mono(format!(
                        "{} FFT frames · {} audio frames · {:.1} MiB",
                        st.fft_frames.load(Relaxed),
                        st.audio_frames.load(Relaxed),
                        st.bytes.load(Relaxed) as f64 / 1_048_576.0
                    )));
                    ui.end_row();
                    ui.label(label("Undecodable"));
                    ui.label(mono(format!("{} message(s)", st.decode_errors.load(Relaxed))));
                    ui.end_row();
                }
                let s = &app.status;
                ui.label(label("Receiver stream"));
                let tone = if s.dropped_blocks + s.lagged_blocks + s.lost_samples > 0 { theme::WARN } else { theme::OK };
                ui.label(RichText::new(format!(
                    "{} dropped · {} lagged blocks · {} lost samples in {} gaps",
                    s.dropped_blocks, s.lagged_blocks, s.lost_samples, s.gap_events
                )).font(theme::mono(crate::theme::ty::VALUE)).color(tone));
                ui.end_row();
                if app.have_status {
                    let s = &app.status;
                    ui.label(label("Receiver"));
                    ui.label(mono(format!(
                        "FFT {} · crystal {:+.2} ppm · channel {}S/s",
                        s.fft_size,
                        s.ppm,
                        if s.inspect_rate_hz > 0.0 { crate::tuning::fmt_short_hz(s.inspect_rate_hz) } else { "—".into() }
                    )));
                    ui.end_row();
                }
                for (name, p) in [("Audio A", &app.player), ("Audio B", &app.player_b)] {
                    let r = p.reading();
                    ui.label(label(name));
                    ui.label(mono(if r.queue_ms > 0.0 {
                        format!("{:.0} ms queued · clock {:+.0} ppm · {} underruns", r.queue_ms, r.drift_ppm, r.underruns)
                    } else {
                        "idle".into()
                    }));
                    ui.end_row();
                }
            });
            ui.add_space(8.0);
            let hist: Vec<(f32, f32, f32)> = app.history.iter().copied().collect();
            for (title, idx, unit, color) in [("Frames a second", 0usize, "fps", theme::AMBER), ("Throughput", 1, "KiB/s", theme::CYAN), ("API round trip", 2, "ms", theme::VIOLET)] {
                let vals: Vec<f32> = hist.iter().map(|h| [h.0, h.1, h.2][idx]).collect();
                let max = vals.iter().copied().fold(0.0f32, f32::max);
                let last = vals.last().copied().unwrap_or(0.0);
                ui.horizontal(|ui| {
                    ui.label(label(title));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(mono(format!("{last:.1} {unit} · max {max:.1}")));
                    });
                });
                let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), egui::Sense::hover());
                ui.painter().rect_filled(r, egui::CornerRadius::same(crate::theme::R_KEY), theme::ink());
                if vals.len() > 1 && max > 0.0 {
                    let pts: Vec<egui::Pos2> = vals
                        .iter()
                        .enumerate()
                        .map(|(i, v)| pos2(r.left() + i as f32 / 119.0 * r.width(), r.bottom() - 3.0 - v / max * (r.height() - 6.0)))
                        .collect();
                    crate::spectrum::fill_under(ui.painter(), &pts, r.bottom(), color, 0.3);
                    ui.painter().add(egui::Shape::line(pts, egui::Stroke::new(1.3, color)));
                }
            }
        });
    if !open {
        app.diagnostics_open = false;
    }
}
