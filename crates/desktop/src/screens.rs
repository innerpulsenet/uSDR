//! The screens around the console: connecting, settings, shortcuts.

use crate::app::{App, Conn};
use crate::brand;
use crate::theme::{self, alpha};
use crate::widgets::{self, Tone};
use egui::{Align2, RichText, Ui, pos2, vec2};
use serde_json::json;
use usdr_proto::WebPolicy;

pub fn connect_screen(app: &mut App, ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    let full = ui.max_rect();
    ui.painter().rect_filled(full, egui::CornerRadius::ZERO, theme::ink());
    let rm = app.reduce_motion();
    // The receive splash across the lower part of the window.
    let splash = egui::Rect::from_min_max(pos2(full.left(), full.top() + full.height() * 0.42), full.max);
    app.splash.paint(ui, splash, rm);

    let card_w = 440.0;
    let card = egui::Rect::from_center_size(pos2(full.center().x, full.top() + full.height() * 0.30), vec2(card_w, 330.0));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(card));
    egui::Frame::new()
        .fill(alpha(theme::surface_2(), 0.96))
        .stroke(egui::Stroke::new(1.0, theme::line_2()))
        .corner_radius(egui::CornerRadius::same(6))
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
            ui.label(RichText::new("SERVER").font(theme::mono(10.5)).color(theme::text_3()));
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
                    ui.label(RichText::new("recent").font(theme::mono(10.5)).color(theme::text_3()));
                    for s in recent {
                        if ui.add(egui::Button::new(RichText::new(&s).font(theme::mono(11.0))).small()).clicked() {
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
                ui.checkbox(&mut app.prefs.auto_connect, RichText::new("Connect on start").font(theme::sans(12.0)));
            });
            ui.add_space(8.0);
            match &app.conn {
                Conn::Connecting { attempt, reason } => {
                    let text = match reason {
                        Some(r) => format!("{r} · retrying"),
                        None => format!("connecting · attempt {attempt}"),
                    };
                    ui.label(RichText::new(text).font(theme::mono(11.0)).color(theme::WARN));
                }
                Conn::Idle => {
                    ui.label(RichText::new("The radio stays with the server; this app streams its spectrum, audio and decodes.").font(theme::sans(12.0)).color(theme::text_3()));
                }
                Conn::Live => {
                    ui.label(RichText::new("waiting for the receiver").font(theme::mono(11.0)).color(theme::text_2()));
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
                .fill(alpha(theme::surface_3(), 0.97))
                .stroke(egui::Stroke::new(1.0, alpha(theme::WARN, 0.5)))
                .corner_radius(egui::CornerRadius::same(6))
                .inner_margin(egui::Margin::symmetric(16, 10))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        brand::mark(ui, 48.0, 2.4, rm);
                        ui.vertical(|ui| {
                            ui.label(RichText::new("Reconnecting").font(theme::headline(15.0)).color(theme::text()));
                            ui.label(RichText::new(text).font(theme::mono(11.0)).color(theme::text_2()));
                            ui.label(RichText::new("The display below is the last frame received.").font(theme::sans(11.5)).color(theme::text_3()));
                        });
                    });
                });
        });
}

pub fn settings_window(app: &mut App, ctx: &egui::Context) {
    if !app.settings_open {
        return;
    }
    let mut open = true;
    egui::Window::new(RichText::new("Settings").font(theme::headline(16.0)))
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .default_width(460.0)
        .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
        .show(ctx, |ui| {
            egui::ScrollArea::vertical().max_height(560.0).show(ui, |ui| {
                section(ui, "Connection");
                if let Some(l) = &app.link {
                    ui.label(RichText::new(&l.base).font(theme::mono(12.0)).color(theme::text()));
                    if widgets::action(ui, "DISCONNECT").clicked() {
                        app.disconnect();
                    }
                }
                ui.checkbox(&mut app.prefs.auto_connect, "Connect to the last server on start");

                section(ui, "Web page on this server");
                ui.label(RichText::new("What the browser page may do while this app is connected. Receiver state is shared either way: a change made in one shows in the other.").color(theme::text_3()));
                let cur = app.presence.web;
                for p in [WebPolicy::Shared, WebPolicy::Yield, WebPolicy::Off] {
                    let desc = match p {
                        WebPolicy::Shared => "Shared — the page and this app both drive the receiver",
                        WebPolicy::Yield => "View-only — the page follows along but cannot change anything while an app is connected",
                        WebPolicy::Off => "Off — the server stops serving the page; the app keeps working",
                    };
                    if ui.radio(cur == p, desc).clicked() && cur != p {
                        app.post("/api/web", json!({ "policy": p }));
                        app.presence.web = p;
                    }
                }
                let web = app.presence.web_count();
                ui.label(RichText::new(format!("{web} web page(s) connected now")).font(theme::mono(11.0)).color(theme::text_3()));

                section(ui, "Audio");
                ui.horizontal(|ui| {
                    let cur = app.prefs.audio_device.clone().unwrap_or_else(|| "System default".into());
                    widgets::selector(ui, "audiodev", &cur, theme::text(), 320.0, |ui| {
                        if ui.selectable_label(app.prefs.audio_device.is_none(), "System default").clicked() {
                            app.prefs.audio_device = None;
                            app.player.open(None);
                            app.player_b.open(None);
                            ui.close();
                        }
                        for d in app.audio_devices.clone() {
                            if ui.selectable_label(app.prefs.audio_device.as_deref() == Some(&d), &d).clicked() {
                                app.player.open(Some(&d));
                                app.player_b.open(Some(&d));
                                app.prefs.audio_device = Some(d);
                                ui.close();
                            }
                        }
                    });
                    if crate::app::icon_button(ui, "↻", "Look for audio devices again").clicked() {
                        app.audio_devices = crate::audio::Player::devices();
                    }
                });
                ui.label(RichText::new(format!("playing on {}", app.player.device_name.lock().unwrap())).font(theme::mono(11.0)).color(theme::text_3()));

                section(ui, "Recordings");
                ui.horizontal(|ui| {
                    let mut dir = app.prefs.record_dir().display().to_string();
                    if ui.add(egui::TextEdit::singleline(&mut dir).font(theme::mono(11.5)).desired_width(340.0)).changed() {
                        app.prefs.record_dir = Some(dir.into());
                    }
                    if widgets::action(ui, "CHOOSE…").clicked() {
                        crate::app::pick(app, "folder");
                    }
                });

                section(ui, "Interface");
                ui.horizontal(|ui| {
                    ui.label("Ground");
                    use crate::theme::Ground;
                    for g in [Ground::Carbon, Ground::Ink] {
                        if ui.radio(app.prefs.ground == g, g.label()).clicked() && app.prefs.ground != g {
                            app.prefs.ground = g;
                            crate::theme::set_ground(ctx, g);
                        }
                    }
                });
                ui.checkbox(&mut app.prefs.shortcuts, "Keyboard tuning shortcuts (arrows, PgUp/PgDn, M, C, R, digits)");
                ui.checkbox(&mut app.prefs.show_scope, "Show the scope in the dock");
                ui.horizontal(|ui| {
                    ui.label("Motion");
                    use crate::prefs::Motion;
                    for (m, l) in [(Motion::System, "Follow the desktop"), (Motion::Full, "Full"), (Motion::Reduced, "Reduced")] {
                        ui.radio_value(&mut app.prefs.motion, m, l);
                    }
                });

                section(ui, "About");
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("uSDR desktop {}", env!("CARGO_PKG_VERSION"))).font(theme::mono(11.5)).color(theme::text_2()));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), brand::credit);
                });
            });
        });
    if !open {
        app.settings_open = false;
        app.prefs.save();
    }
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(title.to_uppercase()).font(theme::mono(10.5)).color(theme::text_3()).extra_letter_spacing(1.2));
    ui.separator();
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
                ("Ctrl+,", "settings"),
                ("Drop a .iq + .json", "decode it on the server"),
            ];
            egui::Grid::new("help").num_columns(2).spacing(vec2(18.0, 6.0)).show(ui, |ui| {
                for (k, v) in rows {
                    ui.label(RichText::new(*k).font(theme::mono(11.5)).color(theme::AMBER));
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
            let mono = |t: String| RichText::new(t).font(theme::mono(11.5)).color(theme::text());
            let label = |t: &str| RichText::new(t).font(theme::sans(12.0)).color(theme::text_3());
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
                )).font(theme::mono(11.5)).color(tone));
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
                ui.painter().rect_filled(r, egui::CornerRadius::same(3), theme::ink());
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
