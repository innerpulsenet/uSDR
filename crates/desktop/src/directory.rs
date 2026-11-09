//! The HF station directory: who sends what, where and when, for the modes
//! whose stations keep fixed frequencies (radiofax, NAVTEX, HFDL, DSC, the
//! time stations, DRM). One menu draws every list the same way: stations
//! by region, each on one line with its frequencies as keys under it, the
//! key the receiver sits on lit, and for a station that keeps a schedule
//! what it is sending now and next.
//!
//! The lists are published data (each list names its source) and carry
//! the date they were checked. A station that is not on the air when its
//! list says is a list gone stale, not a fault in the receiver.

use crate::app::App;
use crate::theme;
use crate::widgets;
use egui::{RichText, Ui, vec2};
use usdr_proto::Mode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    Worldwide,
    NorthAmerica,
    Atlantic,
    Pacific,
    Europe,
    Asia,
    Elsewhere,
}

impl Region {
    pub fn label(self) -> &'static str {
        match self {
            Region::Worldwide => "WORLDWIDE",
            Region::NorthAmerica => "NORTH AMERICA",
            Region::Atlantic => "ATLANTIC",
            Region::Pacific => "PACIFIC",
            Region::Europe => "EUROPE",
            Region::Asia => "ASIA",
            Region::Elsewhere => "ELSEWHERE",
        }
    }
}

/// One scheduled transmission: when it starts (UTC, minutes past
/// midnight), how long it runs (0 where the schedule does not say), what
/// it is, and on which days (bit 0 Sunday to bit 6 Saturday).
#[derive(Clone, Copy, Debug)]
pub struct Slot {
    pub start_min: u16,
    pub minutes: u16,
    pub title: &'static str,
    pub days: u8,
}

/// One station: its call or ID, where it is, the frequencies it sends on
/// (kHz, as published: assigned or carrier frequencies), and its schedule
/// when it keeps one.
#[derive(Clone, Copy, Debug)]
pub struct Station {
    pub call: &'static str,
    pub name: &'static str,
    pub region: Region,
    pub freqs_khz: &'static [f64],
    /// A line about which frequency is on when, or what else to know.
    pub note: &'static str,
    pub schedule: &'static [Slot],
    /// The hours a frequency is used, (kHz, from, to) in minutes past
    /// midnight UTC, for one that is not on all day; the rest are.
    pub windows: &'static [(f64, u16, u16)],
}

impl Station {
    /// Whether a frequency is in use at `now` (minutes past midnight UTC),
    /// as the station publishes it.
    pub fn on_now(&self, khz: f64, now: u16) -> bool {
        let mut ws = self.windows.iter().filter(|w| (w.0 - khz).abs() < 0.05).peekable();
        if ws.peek().is_none() {
            return true;
        }
        ws.any(|&(_, a, b)| if a <= b { (a..=b).contains(&now) } else { now >= a || now <= b })
    }
}

/// A mode's list and how its published frequencies map to the dial.
pub struct Directory {
    pub mode: Mode,
    pub title: &'static str,
    /// The dial (the suppressed carrier) relative to a published frequency:
    /// -1900 Hz for radiofax, whose subcarrier center sits there.
    pub dial_offset_hz: f64,
    /// Where the list comes from and when it was checked.
    pub source: &'static str,
    pub stations: &'static [Station],
}

impl Directory {
    pub fn dial_hz(&self, khz: f64) -> f64 {
        khz * 1e3 + self.dial_offset_hz
    }

    /// The station and frequency the receiver is on, if it is on one.
    pub fn tuned<'a>(&'a self, app: &App) -> Option<(&'a Station, f64)> {
        if app.status.mode != self.mode {
            return None;
        }
        let rx = app.inspect_hz();
        self.stations.iter().find_map(|s| s.freqs_khz.iter().find(|k| (self.dial_hz(**k) - rx).abs() < 50.0).map(|k| (s, *k)))
    }
}

/// Minutes past midnight UTC, now.
pub fn utc_minute_now() -> u16 {
    utc_now().1
}

/// The UTC weekday (0 Sunday) and minutes past midnight, now.
pub fn utc_now() -> (u8, u16) {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    // 1 January 1970 was a Thursday.
    (((secs / 86_400 + 4) % 7) as u8, ((secs / 60) % 1_440) as u16)
}

/// What a schedule has on: a slot under way, or for a schedule that gives
/// no lengths, the one that started last within `quiet` minutes and has
/// not been followed yet (`since` is then true).
pub struct OnAir<'a> {
    pub slot: &'a Slot,
    pub since: bool,
}

/// What a schedule has on now, if anything, and the next to start, with
/// the minutes until it does, on `day` (0 Sunday) at `now`.
pub fn now_and_next(schedule: &[Slot], day: u8, now: u16) -> (Option<OnAir<'_>>, Option<(&Slot, u16)>) {
    let runs = |s: &Slot, d: u8| s.days & (1 << (d % 7)) != 0;
    // Minutes since each slot began, counting one that began yesterday.
    let ago = |s: &Slot| -> Option<u16> {
        if s.start_min <= now {
            runs(s, day).then_some(now - s.start_min)
        } else {
            runs(s, day + 6).then_some(now + 1_440 - s.start_min)
        }
    };
    let on = schedule
        .iter()
        .filter(|s| s.minutes > 0)
        .find(|s| ago(s).is_some_and(|a| a < s.minutes))
        .map(|slot| OnAir { slot, since: false })
        .or_else(|| {
            // No lengths: the latest to begin, within a quarter hour.
            let started = schedule.iter().filter(|s| s.minutes == 0).filter_map(|s| ago(s).map(|a| (s, a))).min_by_key(|(_, a)| *a);
            started.filter(|(_, a)| *a < 15).map(|(slot, _)| OnAir { slot, since: true })
        });
    let next = schedule
        .iter()
        .filter_map(|s| {
            let d = (s.start_min + 1_440 - now) % 1_440;
            let on_day = if s.start_min > now { day } else { day + 1 };
            (d > 0 && runs(s, on_day)).then_some((s, d))
        })
        .min_by_key(|(_, d)| *d);
    (on, next)
}

pub fn hhmm(min: u16) -> String {
    format!("{:02}{:02}", min / 60, min % 60)
}

pub fn until(d: u16) -> String {
    if d < 60 { format!("in {d} min") } else { format!("in {}h{:02}", d / 60, d % 60) }
}

/// Tune a directory frequency: the mode first, then the dial.
pub fn tune(app: &mut App, dir: &Directory, khz: f64) {
    if app.status.mode != dir.mode {
        app.set_mode(dir.mode);
    }
    app.tune(dir.dial_hz(khz), false);
}

const WIDTH: f32 = 470.0;
const CALL_W: f32 = 64.0;

/// The STATIONS menu for a directory.
pub fn menu(app: &mut App, ui: &mut Ui, dir: &Directory, tally: egui::Color32) {
    let lit = dir.tuned(app).is_some();
    widgets::menu_chip_lit(ui, "STATIONS", &[], tally, lit, |ui| {
        ui.set_width(WIDTH);
        list(app, ui, dir, tally, true);
    });
}

/// The list itself, for a menu or a panel. `closes` closes the menu a
/// frequency is picked from.
pub fn list(app: &mut App, ui: &mut Ui, dir: &Directory, tally: egui::Color32, closes: bool) {
    // ON AIR narrows the list to what is on or due within the hour; the
    // choice is kept per directory for the session.
    let soon_id = egui::Id::new(("directory-soon", dir.title));
    let mut soon_only = ui.ctx().data(|d| d.get_temp::<bool>(soon_id)).unwrap_or(false);
    let scheduled = dir.stations.iter().any(|s| !s.schedule.is_empty());
    ui.horizontal(|ui| {
        ui.label(RichText::new(dir.title).font(theme::mono_bold(crate::theme::ty::SMALL)).color(theme::text_2()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(format!("{} UTC", hhmm(utc_minute_now()))).font(theme::mono(crate::theme::ty::LABEL)).color(theme::text_3()));
            if scheduled {
                ui.spacing_mut().item_spacing.x = 3.0;
                if widgets::chip_key(ui, "ON AIR", soon_only, theme::OK).on_hover_text("Only stations on the air now or due within the hour").clicked() {
                    soon_only = true;
                }
                if widgets::chip_key(ui, "ALL", !soon_only, tally).clicked() {
                    soon_only = false;
                }
            }
        });
    });
    ui.ctx().data_mut(|d| d.insert_temp(soon_id, soon_only));
    let soon_only = soon_only && scheduled;
    if dir.dial_offset_hz != 0.0 {
        ui.label(
            RichText::new(format!("Published frequencies; the dial goes {:.1} kHz {}.", dir.dial_offset_hz.abs() / 1e3, if dir.dial_offset_hz < 0.0 { "below" } else { "above" }))
                .font(theme::sans(crate::theme::ty::SMALL))
                .color(theme::text_3()),
        );
    }
    ui.add_space(4.0);
    let rx = (app.status.mode == dir.mode).then(|| app.inspect_hz());
    let (day, now) = utc_now();
    let max_h = (ui.ctx().content_rect().height() * 0.7).max(240.0);
    let mut pick = None;
    egui::ScrollArea::vertical().id_salt(("directory", dir.title)).max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        // Rows wrap inside the list's width rather than widen the menu.
        ui.set_max_width(ui.available_width());
        let mut region = None;
        let mut hidden = 0usize;
        for st in dir.stations {
            let here = rx.is_some_and(|hz| st.freqs_khz.iter().any(|k| (dir.dial_hz(*k) - hz).abs() < 50.0));
            let (on, next) = now_and_next(st.schedule, day, now);
            // ON AIR keeps a station that is on now, starts within the hour,
            // or keeps no schedule to judge it by.
            if soon_only && !st.schedule.is_empty() && on.is_none() && next.is_none_or(|(_, d)| d > 60) {
                hidden += 1;
                continue;
            }
            if region != Some(st.region) {
                region = Some(st.region);
                ui.add_space(4.0);
                ui.label(RichText::new(st.region.label()).font(theme::mono(crate::theme::ty::MICRO)).color(theme::text_3()).extra_letter_spacing(0.8));
                let y = ui.cursor().top();
                ui.painter().line_segment([egui::pos2(ui.max_rect().left(), y), egui::pos2(ui.max_rect().right(), y)], egui::Stroke::new(1.0, theme::line()));
                ui.add_space(3.0);
            }
            // The schedule's text, worked out first so the name is cut to
            // the room it leaves rather than run under it.
            let what = |t: &str| if t == st.name { String::new() } else { format!(" · {t}") };
            let sched: Option<(String, egui::Color32, String)> = if let Some(OnAir { slot, since }) = on {
                let text = if since { format!("SINCE {}{}", hhmm(slot.start_min), what(slot.title)) } else { format!("ON AIR{}", what(slot.title)) };
                let tip = if since { format!("Began {} UTC; the schedule gives no length", hhmm(slot.start_min)) } else { format!("{} UTC, {} min", hhmm(slot.start_min), slot.minutes) };
                Some((text, theme::OK, tip))
            } else {
                next.map(|(s, d)| (format!("{} {}", hhmm(s.start_min), until(d)), theme::text_3(), s.title.to_string()))
            };
            ui.horizontal(|ui| {
                ui.add_sized(vec2(CALL_W, 16.0), egui::Label::new(RichText::new(st.call).font(theme::mono_bold(crate::theme::ty::VALUE)).color(if here { tally } else { theme::text() })).truncate());
                let gap = 10.0;
                let sched_w = sched.as_ref().map_or(0.0, |(t, ..)| {
                    let w = ui.painter().layout_no_wrap(t.clone(), theme::mono(crate::theme::ty::MICRO), egui::Color32::PLACEHOLDER).size().x;
                    w.min(ui.available_width() * 0.5) + gap
                });
                let name_w = (ui.available_width() - sched_w).max(40.0);
                ui.allocate_ui_with_layout(vec2(name_w, 16.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.set_width(name_w);
                    ui.add(egui::Label::new(RichText::new(st.name).font(theme::sans(crate::theme::ty::VALUE)).color(theme::text_2())).truncate())
                        .on_hover_text(st.name);
                });
                if let Some((text, color, tip)) = sched {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add(egui::Label::new(RichText::new(text).font(theme::mono(crate::theme::ty::MICRO)).color(color)).truncate()).on_hover_text(tip);
                    });
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.set_max_width(WIDTH - 12.0);
                ui.add_space(CALL_W + ui.spacing().item_spacing.x);
                ui.spacing_mut().item_spacing.x = 4.0;
                for khz in st.freqs_khz {
                    let on = rx.is_some_and(|hz| (dir.dial_hz(*khz) - hz).abs() < 50.0);
                    let label = if khz.fract() == 0.0 { format!("{khz:.0}") } else { format!("{khz:.1}") };
                    // A frequency the station does not use at this hour is
                    // drawn faint, and still tunes.
                    let live = st.on_now(*khz, now);
                    let mut tip = format!("{} kHz · dial {:.1} kHz", label, dir.dial_hz(*khz) / 1e3);
                    if let Some(w) = st.windows.iter().find(|w| (w.0 - khz).abs() < 0.05) {
                        tip += &format!(" · used {}–{} UTC", hhmm(w.1), hhmm(w.2));
                    }
                    let r = ui.scope(|ui| {
                        if !live {
                            ui.multiply_opacity(0.45);
                        }
                        widgets::chip_key_w(ui, &label, on, tally, 62.0)
                    });
                    if r.inner.on_hover_text(tip).clicked() {
                        pick = Some(*khz);
                    }
                }
            });
            if !st.note.is_empty() {
                ui.horizontal(|ui| {
                    ui.add_space(CALL_W + ui.spacing().item_spacing.x);
                    ui.add(egui::Label::new(RichText::new(st.note).font(theme::sans(crate::theme::ty::LABEL)).color(theme::text_3())).wrap());
                });
            }
            ui.add_space(5.0);
        }
        if hidden > 0 {
            ui.add_space(4.0);
            ui.label(RichText::new(format!("{hidden} more off the air for the next hour · ALL shows them")).font(theme::sans(crate::theme::ty::LABEL)).color(theme::text_3()));
        }
        ui.add_space(4.0);
        ui.label(RichText::new(dir.source).font(theme::sans(crate::theme::ty::MICRO)).color(theme::text_3()));
    });
    if let Some(khz) = pick {
        tune(app, dir, khz);
        if closes {
            ui.close();
        }
    }
}

/// The frequency to try for a station at `now`: one it uses at that
/// hour, by day the highest up to 13 MHz, by night the lowest from 4 MHz,
/// where the band is likeliest open over an ocean.
pub fn best_khz(st: &Station, now: u16) -> Option<f64> {
    let live: Vec<f64> = st.freqs_khz.iter().copied().filter(|k| st.on_now(*k, now)).collect();
    let pool = if live.is_empty() { st.freqs_khz.to_vec() } else { live };
    let day = (12 * 60..22 * 60).contains(&now);
    if day {
        pool.iter().copied().filter(|k| *k <= 13_000.0).fold(None, |a: Option<f64>, k| Some(a.map_or(k, |a| a.max(k)))).or_else(|| pool.first().copied())
    } else {
        pool.iter().copied().filter(|k| *k >= 4_000.0).fold(None, |a: Option<f64>, k| Some(a.map_or(k, |a| a.min(k)))).or_else(|| pool.first().copied())
    }
}

/// What every station in a directory sends in the next `hours`, in the
/// order it starts: (minutes from now, station, slot).
pub fn upcoming(dir: &Directory, day: u8, now: u16, hours: u16) -> Vec<(u16, &Station, &Slot)> {
    let mut out = Vec::new();
    for st in dir.stations {
        for sl in st.schedule {
            let d = (sl.start_min + 1_440 - now) % 1_440;
            let on_day = if sl.start_min >= now { day } else { day + 1 };
            if d < hours * 60 && sl.days & (1 << (on_day % 7)) != 0 {
                out.push((d, st, sl));
            }
        }
    }
    out.sort_by_key(|(d, st, _)| (*d, st.call));
    out
}

/// The SCHEDULE menu: the next few hours across every station, each line
/// tuning its station.
pub fn schedule_menu(app: &mut App, ui: &mut Ui, dir: &Directory, tally: egui::Color32) {
    widgets::menu_chip(ui, "SCHEDULE", theme::text_2(), |ui| {
        ui.set_width(WIDTH + 60.0);
        let (day, now) = utc_now();
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} · NEXT 3 HOURS", dir.title)).font(theme::mono_bold(crate::theme::ty::SMALL)).color(theme::text_2()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("{} UTC", hhmm(now))).font(theme::mono(crate::theme::ty::LABEL)).color(theme::text_3()));
            });
        });
        ui.add_space(4.0);
        let list = upcoming(dir, day, now, 3);
        let mut pick = None;
        let max_h = (ui.ctx().content_rect().height() * 0.7).max(240.0);
        egui::ScrollArea::vertical().id_salt(("schedule", dir.title)).max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            for (d, st, sl) in list {
                let khz = best_khz(st, (now + d) % 1_440);
                let r = ui
                    .horizontal(|ui| {
                        ui.set_min_height(18.0);
                        ui.add_sized(vec2(40.0, 16.0), egui::Label::new(RichText::new(hhmm(sl.start_min)).font(theme::mono_bold(crate::theme::ty::SMALL)).color(theme::text())));
                        ui.add_sized(vec2(64.0, 16.0), egui::Label::new(RichText::new(until(d).replace("in ", "")).font(theme::mono(crate::theme::ty::MICRO)).color(theme::text_3())));
                        ui.add_sized(vec2(64.0, 16.0), egui::Label::new(RichText::new(st.call).font(theme::mono_bold(crate::theme::ty::SMALL)).color(tally)).truncate());
                        if let Some(k) = khz {
                            ui.add_sized(vec2(58.0, 16.0), egui::Label::new(RichText::new(format!("{k:.1}")).font(theme::mono(crate::theme::ty::LABEL)).color(theme::AMBER)));
                        }
                        ui.add(egui::Label::new(RichText::new(sl.title).font(theme::sans(crate::theme::ty::SMALL)).color(theme::text_2())).truncate());
                    })
                    .response
                    .interact(egui::Sense::click());
                if r.hovered() {
                    ui.painter().rect_filled(r.rect, egui::CornerRadius::same(crate::theme::R_KEY), theme::alpha(tally, 0.08));
                }
                if r.on_hover_text(format!("{} · {}", st.name, st.call)).clicked()
                    && let Some(k) = khz
                {
                    pick = Some(k);
                }
            }
            ui.add_space(4.0);
            ui.label(RichText::new("Click a line to tune its station on a frequency it uses at that hour.").font(theme::sans(crate::theme::ty::MICRO)).color(theme::text_3()));
        });
        if let Some(k) = pick {
            tune(app, dir, k);
            ui.close();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAILY: u8 = 0x7f;

    #[test]
    fn now_and_next_wrap_midnight() {
        let s = [Slot { start_min: 23 * 60 + 50, minutes: 20, title: "a", days: DAILY }, Slot { start_min: 2 * 60, minutes: 10, title: "b", days: DAILY }];
        let (on, next) = now_and_next(&s, 3, 5);
        assert_eq!(on.map(|o| o.slot.title), Some("a"));
        assert_eq!(next.map(|(s, d)| (s.title, d)), Some(("b", 115)));
        let (on, next) = now_and_next(&s, 3, 23 * 60 + 40);
        assert!(on.is_none());
        assert_eq!(next.map(|(s, d)| (s.title, d)), Some(("a", 10)));
    }

    /// A Tuesday-and-Thursday broadcast is not next on a Wednesday, and
    /// one that started yesterday evening counts yesterday's day.
    #[test]
    fn days_are_kept() {
        let tue_thu = (1 << 2) | (1 << 4);
        let s = [Slot { start_min: 18 * 60, minutes: 60, title: "m4j", days: tue_thu }, Slot { start_min: 23 * 60 + 30, minutes: 60, title: "late", days: 1 << 2 }];
        assert!(now_and_next(&s, 3, 18 * 60 + 10).0.is_none());
        assert_eq!(now_and_next(&s, 4, 18 * 60 + 10).0.map(|o| o.slot.title), Some("m4j"));
        // Wednesday 00:10: Tuesday's 23:30 broadcast is still on.
        assert_eq!(now_and_next(&s, 3, 10).0.map(|o| o.slot.title), Some("late"));
        // Wednesday noon: nothing in the next day; Thursday's is further.
        assert_eq!(now_and_next(&s, 3, 12 * 60).1.map(|(s, d)| (s.title, d)), None);
    }

    /// A schedule without lengths says what began last, for a quarter
    /// hour, as begun rather than as on.
    #[test]
    fn a_schedule_without_lengths_says_since() {
        let s = [Slot { start_min: 600, minutes: 0, title: "a", days: DAILY }, Slot { start_min: 610, minutes: 0, title: "b", days: DAILY }];
        let (on, _) = now_and_next(&s, 0, 612);
        let on = on.expect("on");
        assert!(on.since && on.slot.title == "b");
        assert!(now_and_next(&s, 0, 640).0.is_none());
    }

    #[test]
    fn the_best_frequency_is_one_in_use() {
        let st = Station { call: "NMF", name: "", region: Region::NorthAmerica, freqs_khz: &[4_235.0, 6_340.5, 9_110.0, 12_750.0], note: "", schedule: &[], windows: &[(4_235.0, 150, 639), (12_750.0, 840, 1_359)] };
        // By day 12750 is on and highest up to 13 MHz; at night 4235.
        assert_eq!(best_khz(&st, 15 * 60), Some(12_750.0));
        assert_eq!(best_khz(&st, 3 * 60), Some(4_235.0));
        // Before 0230, 4235 is not yet on: the lowest from 4 MHz in use.
        assert_eq!(best_khz(&st, 60), Some(6_340.5));
    }

    #[test]
    fn windows_wrap_midnight() {
        let st = Station { call: "X", name: "", region: Region::Pacific, freqs_khz: &[1.0, 2.0], note: "", schedule: &[], windows: &[(1.0, 1_039, 236)] };
        assert!(st.on_now(1.0, 1_200) && st.on_now(1.0, 100) && !st.on_now(1.0, 600));
        assert!(st.on_now(2.0, 600));
    }
}
