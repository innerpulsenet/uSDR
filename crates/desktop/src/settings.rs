//! The settings window: a rail of pages on the left, the page on the
//! right. What belongs to this desktop (audio, recordings, the interface)
//! is saved with its preferences; what belongs to the server (its station
//! and spot reporting, its web page) is sent to it and comes back on the
//! socket, so every client sees the same.

use crate::app::App;
use crate::brand;
use crate::theme::{self, alpha};
use crate::widgets::{self, Tone};
use egui::{Align2, CornerRadius, RichText, Sense, Stroke, Ui, pos2, vec2};
use serde_json::json;
use usdr_proto::{ReporterState, ReporterStatus, Station, WebPolicy, grid_center, normalize_callsign, normalize_grid};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Page {
    #[default]
    Station,
    Connection,
    Audio,
    Recordings,
    Interface,
    About,
}

impl Page {
    const ALL: [Page; 6] = [Page::Station, Page::Connection, Page::Audio, Page::Recordings, Page::Interface, Page::About];

    fn label(self) -> &'static str {
        match self {
            Page::Station => "Station",
            Page::Connection => "Connection",
            Page::Audio => "Audio",
            Page::Recordings => "Recordings",
            Page::Interface => "Interface",
            Page::About => "About",
        }
    }

    fn blurb(self) -> &'static str {
        match self {
            Page::Station => "Your callsign and locator, kept on the server, and the FT8 and FT4 spots it reports under them.",
            Page::Connection => "The server this app drives, and what its web page may do meanwhile.",
            Page::Audio => "Where this app plays the receivers.",
            Page::Recordings => "Where recordings and saved pictures go on this computer.",
            Page::Interface => "How this app looks and which keys it answers.",
            Page::About => "This app and the server it is talking to.",
        }
    }
}

/// The station as it is being edited, against what the server last said.
#[derive(Clone, Debug)]
pub struct StationForm {
    pub callsign: String,
    pub grid: String,
    pub antenna: String,
    /// The fields were filled from the server and not edited since.
    pub synced: bool,
}

impl Default for StationForm {
    /// Empty and following the server, so the first station it sends fills it.
    fn default() -> Self {
        StationForm { callsign: String::new(), grid: String::new(), antenna: String::new(), synced: true }
    }
}

impl StationForm {
    fn from_station(s: &Station) -> Self {
        StationForm { callsign: s.callsign.clone(), grid: s.grid.clone(), antenna: s.antenna.clone(), synced: true }
    }

    /// The edited station, normalized, or the first field that does not pass.
    fn parsed(&self, report: bool) -> Result<Station, (&'static str, &'static str)> {
        let callsign = match self.callsign.trim() {
            "" => String::new(),
            c => normalize_callsign(c).map_err(|e| ("callsign", e))?,
        };
        let grid = match self.grid.trim() {
            "" => String::new(),
            g => normalize_grid(g).map_err(|e| ("grid", e))?,
        };
        Ok(Station { callsign, grid, antenna: self.antenna.trim().to_string(), report_spots: report })
    }

    fn differs(&self, s: &Station) -> bool {
        self.parsed(s.report_spots).map_or(true, |p| p != *s)
    }
}

const RAIL_W: f32 = 150.0;
const LABEL_W: f32 = 118.0;

pub fn window(app: &mut App, ctx: &egui::Context) {
    if !app.settings_open {
        return;
    }
    // The form follows the server until the operator types in it.
    if let Some((st, _)) = &app.station
        && (app.station_form.synced || !app.station_form.differs(st))
    {
        app.station_form = StationForm::from_station(st);
    }
    let mut open = true;
    let screen = ctx.content_rect();
    let size = vec2((screen.width() - 80.0).clamp(560.0, 760.0), (screen.height() - 120.0).clamp(380.0, 560.0));
    egui::Window::new(RichText::new("Settings").font(theme::headline(16.0)))
        .id(egui::Id::new("settings-window"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_size(size)
        .min_size(vec2(540.0, 360.0))
        .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.set_min_size(ui.available_size());
            ui.horizontal_top(|ui| {
                rail(app, ui);
                ui.add_space(14.0);
                ui.vertical(|ui| {
                    let page = app.settings_page;
                    ui.label(RichText::new(page.label()).font(theme::headline(15.0)).color(theme::text()));
                    ui.label(RichText::new(page.blurb()).font(theme::sans(12.0)).color(theme::text_3()));
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical().id_salt(("settings-page", page as u8)).auto_shrink([false, false]).show(ui, |ui| {
                        ui.set_width(ui.available_width() - 6.0);
                        match page {
                            Page::Station => station_page(app, ui),
                            Page::Connection => connection_page(app, ui),
                            Page::Audio => audio_page(app, ui),
                            Page::Recordings => recordings_page(app, ui),
                            Page::Interface => interface_page(app, ui, ctx),
                            Page::About => about_page(app, ui),
                        }
                    });
                });
            });
        });
    if !open {
        // A finished edit is not lost by closing the window on it.
        if let Some((st, _)) = &app.station
            && let Ok(edited) = app.station_form.parsed(st.report_spots)
            && edited != *st
        {
            send_station(app, edited);
        }
        app.settings_open = false;
        app.prefs.save();
    }
}

/// The pages, one per row, the lit one marked by an amber bar that slides
/// to it.
fn rail(app: &mut App, ui: &mut Ui) {
    let (outer, _) = ui.allocate_exact_size(vec2(RAIL_W, ui.available_height()), Sense::hover());
    let p = ui.painter().clone();
    theme::well(&p, outer, None);
    let row_h = 30.0;
    let top = outer.top() + 6.0;
    let rm = app.reduce_motion();
    let idx = Page::ALL.iter().position(|p| *p == app.settings_page).unwrap_or(0) as f32;
    let y = if rm { idx } else { ui.ctx().animate_value_with_time(egui::Id::new("settings-rail"), idx, 0.16) };
    let lit = egui::Rect::from_min_size(pos2(outer.left() + 4.0, top + y * row_h), vec2(RAIL_W - 8.0, row_h - 2.0));
    p.rect_filled(lit, CornerRadius::same(4), alpha(theme::AMBER, 0.12));
    p.rect_filled(egui::Rect::from_min_size(lit.left_top() + vec2(0.0, 6.0), vec2(2.0, row_h - 14.0)), CornerRadius::same(1), theme::AMBER);
    for (i, page) in Page::ALL.into_iter().enumerate() {
        let r = egui::Rect::from_min_size(pos2(outer.left() + 4.0, top + i as f32 * row_h), vec2(RAIL_W - 8.0, row_h - 2.0));
        let resp = ui.interact(r, egui::Id::new(("settings-page-key", i)), Sense::click());
        let on = app.settings_page == page;
        resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, on, page.label()));
        if resp.hovered() && !on {
            p.rect_filled(r, CornerRadius::same(4), alpha(theme::surface_4(), 0.5));
        }
        let c = if on { theme::text() } else if resp.hovered() { theme::text_2() } else { theme::text_3() };
        p.text(pos2(r.left() + 14.0, r.center().y), Align2::LEFT_CENTER, page.label(), theme::sans(13.0), c);
        // A lamp on the Station page says how reporting is doing.
        if page == Page::Station
            && let Some((_, rep)) = &app.station
        {
            let (tone, _) = reporter_tone(rep);
            if tone != Tone::Idle {
                let col = match tone {
                    Tone::Ok => theme::OK,
                    Tone::Warn => theme::WARN,
                    _ => theme::ERR,
                };
                theme::lamp(&p, pos2(r.right() - 12.0, r.center().y), 3.4, col, 1.0, theme::LampShape::Round);
            }
        }
        if resp.clicked() {
            app.settings_page = page;
        }
    }
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(12.0);
    theme::engraved(ui, &title.to_uppercase(), theme::mono(10.5), theme::text_3(), 1.2, Sense::hover());
    let r = ui.available_rect_before_wrap();
    ui.painter().line_segment([pos2(r.left(), r.top() + 2.0), pos2(r.right(), r.top() + 2.0)], Stroke::new(1.0, theme::line()));
    ui.add_space(8.0);
}

/// A labeled row: the label in its column, the control beside it.
fn row<R>(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(LABEL_W, 22.0), Sense::hover());
        ui.painter().text(pos2(r.left(), r.center().y), Align2::LEFT_CENTER, label, theme::sans(12.5), theme::text_2());
        add(ui)
    })
    .inner
}

/// A note under a row, set in from the label column.
fn note(ui: &mut Ui, text: impl Into<String>, color: egui::Color32) {
    ui.horizontal(|ui| {
        ui.add_space(LABEL_W + ui.spacing().item_spacing.x);
        ui.add(egui::Label::new(RichText::new(text).font(theme::sans(11.5)).color(color)).wrap());
    });
}

fn send_station(app: &mut App, s: Station) {
    app.post("/api/station", json!(s));
    // Shown at once; the server's answer on the socket confirms it.
    if let Some((st, _)) = &mut app.station {
        *st = s;
    }
    app.station_form.synced = true;
}

/// How the reporter is doing, as a lamp and a word.
fn reporter_tone(rep: &ReporterStatus) -> (Tone, &'static str) {
    match rep.state {
        ReporterState::Off => (Tone::Idle, "OFF"),
        ReporterState::Incomplete => (Tone::Warn, "NEEDS CALLSIGN AND LOCATOR"),
        ReporterState::Waiting => (Tone::Ok, "REPORTING"),
        ReporterState::Error => (Tone::Err, "NOT REACHING PSKREPORTER"),
    }
}

fn station_page(app: &mut App, ui: &mut Ui) {
    let Some((st, rep)) = app.station.clone() else {
        section(ui, "Station");
        let why = if app.link.is_none() {
            "Connect to a server to set its station. The station is kept on the server, which does the decoding and the reporting, so it reports whether or not this app is open."
        } else {
            "This server does not keep a station; it needs a newer uSDR server."
        };
        ui.add(egui::Label::new(RichText::new(why).font(theme::sans(12.0)).color(theme::text_2())).wrap());
        return;
    };
    section(ui, "Station");
    let parsed = app.station_form.parsed(st.report_spots);
    let field_w = 150.0;
    let mut submit = false;
    let r = row(ui, "Callsign", |ui| {
        let mut s = app.station_form.callsign.clone();
        let r = ui.add(egui::TextEdit::singleline(&mut s).font(theme::mono(13.0)).desired_width(field_w).hint_text("N0CALL").char_limit(12));
        if r.changed() {
            app.station_form.callsign = s.to_ascii_uppercase();
            app.station_form.synced = false;
        }
        r
    });
    submit |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    match normalize_callsign(&app.station_form.callsign) {
        Ok(_) => {}
        Err(_) if app.station_form.callsign.trim().is_empty() => note(ui, "The call PSKReporter shows as the receiver.", theme::text_3()),
        Err(e) => note(ui, e, theme::ERR),
    }
    ui.add_space(4.0);
    let r = row(ui, "Locator", |ui| {
        let mut s = app.station_form.grid.clone();
        let r = ui.add(egui::TextEdit::singleline(&mut s).font(theme::mono(13.0)).desired_width(field_w).hint_text("FN31pr").char_limit(8));
        if r.changed() {
            app.station_form.grid = s;
            app.station_form.synced = false;
        }
        if let Some((lat, lon)) = grid_center(&app.station_form.grid) {
            let ns = if lat >= 0.0 { "N" } else { "S" };
            let ew = if lon >= 0.0 { "E" } else { "W" };
            ui.label(RichText::new(format!("{:.2}° {ns}  {:.2}° {ew}", lat.abs(), lon.abs())).font(theme::mono(11.5)).color(theme::text_3()));
        }
        r
    });
    submit |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    match normalize_grid(&app.station_form.grid) {
        Ok(g) if g.len() == 4 => note(ui, "Six characters place you on the map more closely.", theme::text_3()),
        Ok(_) => {}
        Err(_) if app.station_form.grid.trim().is_empty() => note(ui, "The Maidenhead square the receiver is in, 4 or 6 characters.", theme::text_3()),
        Err(e) => note(ui, e, theme::ERR),
    }
    ui.add_space(4.0);
    let r = row(ui, "Antenna", |ui| {
        let mut s = app.station_form.antenna.clone();
        let r = ui.add(egui::TextEdit::singleline(&mut s).font(theme::sans(12.5)).desired_width(ui.available_width().min(320.0)).hint_text("optional, e.g. 40 m end-fed wire").char_limit(64));
        if r.changed() {
            app.station_form.antenna = s;
            app.station_form.synced = false;
        }
        r
    });
    submit |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    ui.add_space(8.0);
    let dirty = app.station_form.differs(&st);
    ui.horizontal(|ui| {
        ui.add_space(LABEL_W + ui.spacing().item_spacing.x);
        let can = dirty && parsed.is_ok();
        let save = ui.add_enabled_ui(can, |ui| widgets::primary(ui, "SAVE")).inner;
        if (save.clicked() || (submit && can))
            && let Ok(s) = &parsed
        {
            send_station(app, s.clone());
        }
        if dirty && widgets::action_small(ui, "REVERT").clicked() {
            app.station_form = StationForm::from_station(&st);
        }
        if dirty {
            ui.label(RichText::new("unsaved").font(theme::mono(10.5)).color(theme::WARN));
        }
    });

    section(ui, "Spot reporting");
    let mut on = st.report_spots;
    let complete = parsed.as_ref().is_ok_and(|s| s.complete());
    if ui
        .checkbox(&mut on, RichText::new("Report FT8 and FT4 decodes to PSKReporter").font(theme::sans(12.5)))
        .on_hover_text("Each station heard goes on the map at pskreporter.info, with its signal report, under your callsign.")
        .changed()
    {
        // Turning it on saves the fields as they stand, if they pass.
        let s = match &parsed {
            Ok(p) => Station { report_spots: on, ..p.clone() },
            Err(_) => Station { report_spots: on, ..st.clone() },
        };
        send_station(app, s);
    }
    ui.add(
        egui::Label::new(
            RichText::new(
                "The server keeps one spot per station, band and mode and uploads every five minutes; a station is reported again within the hour only when its locator becomes known. Decodes the receiver hears while this app is closed are reported too.",
            )
            .font(theme::sans(11.5))
            .color(theme::text_3()),
        )
        .wrap(),
    );
    ui.add_space(8.0);
    reporter_card(app, ui, &st, &rep, complete);
}

/// What the reporter has done and will do next, on one recessed card.
fn reporter_card(app: &App, ui: &mut Ui, st: &Station, rep: &ReporterStatus, complete: bool) {
    let (tone, word) = reporter_tone(rep);
    egui::Frame::new()
        .fill(alpha(theme::ink(), 0.6))
        .stroke(Stroke::new(1.0, theme::line()))
        .corner_radius(CornerRadius::same(5))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                widgets::chip(ui, tone, word);
                if rep.state == ReporterState::Incomplete && complete {
                    ui.label(RichText::new("save the fields above").font(theme::sans(11.5)).color(theme::text_3()));
                }
            });
            ui.add_space(6.0);
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let utc = |t: u64| format!("{:02}:{:02} UTC", (t / 3600) % 24, (t / 60) % 60);
            let stat = |ui: &mut Ui, k: &str, v: String| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(k).font(theme::mono(9.5)).color(theme::text_3()).extra_letter_spacing(0.8));
                    ui.label(RichText::new(v).font(theme::mono_bold(13.0)).color(theme::text()));
                });
                ui.add_space(14.0);
            };
            ui.horizontal(|ui| {
                stat(ui, "WAITING", rep.queued.to_string());
                stat(ui, "THIS HOUR", rep.heard_hour.to_string());
                stat(ui, "SENT", rep.spots_sent.to_string());
                stat(ui, "UPLOADS", rep.datagrams.to_string());
                let next = match (rep.state, rep.next_send) {
                    (ReporterState::Waiting | ReporterState::Error, Some(t)) if rep.queued > 0 => {
                        let s = t.saturating_sub(now);
                        format!("{}:{:02}", s / 60, s % 60)
                    }
                    (ReporterState::Waiting, _) => "when heard".into(),
                    _ => "—".into(),
                };
                stat(ui, "NEXT UPLOAD", next);
            });
            if let Some(t) = rep.last_sent {
                ui.label(RichText::new(format!("last upload {}", utc(t))).font(theme::mono(10.5)).color(theme::text_3()));
            }
            if let Some(e) = &rep.last_error {
                ui.add(egui::Label::new(RichText::new(e).font(theme::mono(10.5)).color(theme::ERR)).wrap());
            }
            if rep.state != ReporterState::Off {
                // The card counts down while it is open.
                ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
            }
            if normalize_callsign(&st.callsign).is_ok() {
                ui.add_space(4.0);
                if widgets::action_small(ui, "SEE YOUR SPOTS ON THE MAP").on_hover_text("pskreporter.info, in your browser").clicked() {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(format!("https://pskreporter.info/pskmap.html?preset&callsign={}&timerange=3600", st.callsign)));
                }
            }
            let _ = app;
        });
}

fn connection_page(app: &mut App, ui: &mut Ui) {
    section(ui, "Server");
    match &app.link {
        Some(l) => {
            let base = l.base.clone();
            row(ui, "Address", |ui| {
                ui.label(RichText::new(base).font(theme::mono(12.5)).color(theme::text()));
                if widgets::action_small(ui, "DISCONNECT").clicked() {
                    app.disconnect();
                }
            });
        }
        None => {
            row(ui, "Address", |ui| ui.label(RichText::new("not connected").font(theme::mono(12.0)).color(theme::text_3())));
        }
    }
    ui.add_space(4.0);
    ui.checkbox(&mut app.prefs.auto_connect, "Connect to the last server on start");

    section(ui, "Web page on this server");
    ui.add(egui::Label::new(RichText::new("What the browser page may do while this app is connected. Receiver state is shared either way: a change made in one shows in the other.").font(theme::sans(12.0)).color(theme::text_3())).wrap());
    ui.add_space(4.0);
    let cur = app.presence.web;
    ui.add_enabled_ui(app.link.is_some(), |ui| {
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
    });
    let web = app.presence.web_count();
    let desk = app.presence.desktop_count();
    ui.label(RichText::new(format!("{web} web page(s) and {desk} app(s) connected now")).font(theme::mono(11.0)).color(theme::text_3()));
}

fn audio_page(app: &mut App, ui: &mut Ui) {
    section(ui, "Output");
    row(ui, "Device", |ui| {
        let cur = app.prefs.audio_device.clone().unwrap_or_else(|| "System default".into());
        widgets::selector(ui, "audiodev", &cur, theme::text(), 300.0, |ui| {
            if ui.selectable_label(app.prefs.audio_device.is_none(), "System default").clicked() {
                app.prefs.audio_device = None;
                app.player.open(None);
                app.player_b.open(None);
                ui.close();
            }
            for d in app.audio_devices.get_or_insert_with(crate::audio::Player::devices).clone() {
                if ui.selectable_label(app.prefs.audio_device.as_deref() == Some(&d), &d).clicked() {
                    app.player.open(Some(&d));
                    app.player_b.open(Some(&d));
                    app.prefs.audio_device = Some(d);
                    ui.close();
                }
            }
        });
        if crate::app::icon_button(ui, "↻", "Look for audio devices again").clicked() {
            app.audio_devices = Some(crate::audio::Player::devices());
        }
    });
    note(ui, format!("playing on {}", app.player.device_name.lock().unwrap()), theme::text_3());
}

fn recordings_page(app: &mut App, ui: &mut Ui) {
    section(ui, "Folder");
    row(ui, "Save to", |ui| {
        let mut dir = app.prefs.record_dir().display().to_string();
        if ui.add(egui::TextEdit::singleline(&mut dir).font(theme::mono(11.5)).desired_width((ui.available_width() - 90.0).max(160.0))).changed() {
            app.prefs.record_dir = Some(dir.into());
        }
        if widgets::action_small(ui, "CHOOSE…").clicked() {
            crate::app::pick(app, "folder");
        }
    });
    note(ui, "Audio and I/Q recordings, and radiofax pictures as PNG.", theme::text_3());
    section(ui, "Radiofax");
    ui.checkbox(&mut app.prefs.fax_autosave, "Save each finished radiofax picture here");
}

fn interface_page(app: &mut App, ui: &mut Ui, ctx: &egui::Context) {
    use crate::theme::Ground;
    section(ui, "Look");
    row(ui, "Ground", |ui| {
        let labels: Vec<&str> = Ground::ALL.iter().map(|g| g.label()).collect();
        let cur = Ground::ALL.iter().position(|g| *g == app.prefs.ground);
        if let Some(i) = widgets::segmented(ui, "ground", &labels, cur, theme::AMBER) {
            app.prefs.ground = Ground::ALL[i];
            crate::theme::set_ground(ctx, Ground::ALL[i]);
        }
    });
    ui.add_space(4.0);
    row(ui, "Motion", |ui| {
        use crate::prefs::Motion;
        let all = [Motion::System, Motion::Full, Motion::Reduced];
        let cur = all.iter().position(|m| *m == app.prefs.motion);
        if let Some(i) = widgets::segmented(ui, "motion", &["FOLLOW THE DESKTOP", "FULL", "REDUCED"], cur, theme::AMBER) {
            app.prefs.motion = all[i];
        }
    });
    section(ui, "Window");
    ui.checkbox(&mut app.prefs.own_titlebar, "Integrated title bar: the masthead moves and sizes the window (on the next start)")
        .on_hover_text("Replaces the system's title bar and frame. On Windows 11 this gives up the snap-layout menu on the maximize key.");
    if cfg!(windows) {
        ui.checkbox(&mut app.prefs.mica, "Mica backdrop behind the chrome, Windows 11 (on the next start)");
    }
    ui.checkbox(&mut app.prefs.show_scope, "Show the scope in the dock");
    section(ui, "Keys");
    ui.checkbox(&mut app.prefs.shortcuts, "Keyboard tuning shortcuts (arrows, PgUp/PgDn, M, C, R, digits)");
}

fn about_page(app: &mut App, ui: &mut Ui) {
    section(ui, "This app");
    row(ui, "Version", |ui| ui.label(RichText::new(format!("uSDR desktop {}", env!("CARGO_PKG_VERSION"))).font(theme::mono(12.0)).color(theme::text())));
    section(ui, "Server");
    match &app.server {
        Some(s) => {
            row(ui, "Version", |ui| ui.label(RichText::new(format!("{} {} · API {}", s.name, s.version, s.api)).font(theme::mono(12.0)).color(theme::text())));
            row(ui, "Offers", |ui| ui.add(egui::Label::new(RichText::new(s.features.join(" · ")).font(theme::mono(11.0)).color(theme::text_2())).wrap()));
        }
        None => {
            row(ui, "Version", |ui| ui.label(RichText::new(if app.link.is_some() { "an older server" } else { "not connected" }).font(theme::mono(12.0)).color(theme::text_3())));
        }
    }
    ui.add_space(16.0);
    ui.horizontal(brand::credit);
}
