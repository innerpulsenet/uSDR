//! A headless benchmark of the client's per-frame work.
//!
//! Each case drives real code through `egui::Context::run` and tessellates
//! what it painted, so the times are the CPU cost of a frame short of the
//! GPU: the panafall alone in the states that cost differently (live, idle,
//! a palette change, a retune, held), the whole window, and the hot pieces
//! on their own. Texture bytes per frame are counted too, since those cross
//! to the GPU every frame they change.
//!
//! ```text
//! cargo test -p usdr-desktop --release bench -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `USDR_BENCH_FRAMES` sets the frames per case (default 600);
//! `USDR_BENCH_ONLY` runs only the panafall cases whose names contain it.

use crate::app::App;
use crate::audio::Player;
use crate::colormap::Colormap;
use crate::net::{Link, NetEvent};
use crate::prefs::Prefs;
use crate::spectrum::{Panafall, SliceFlag, View};
use std::time::{Duration, Instant};
use usdr_proto::{self as proto, FftFrame, Mode, Peak, Status};

/// A cheap repeatable noise source.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
}

/// A 2 MHz span at 146 MHz with `n` bins: a noisy floor, a few carriers
/// that wander a little frame to frame, holds, burst peaks and scope audio.
fn frame(n: usize, i: usize, rng: &mut Rng) -> FftFrame {
    let mut pwr: Vec<f32> = (0..n).map(|_| -102.0 + rng.next() * 8.0).collect();
    for (k, &(at, level, width)) in [(0.18, -45.0, 6), (0.52, -30.0, 14), (0.71, -60.0, 3), (0.86, -52.0, 40)].iter().enumerate() {
        let c = ((at + 0.002 * ((i + k * 7) % 11) as f32) * n as f32) as usize;
        for d in 0..width {
            let b = (c + d).min(n - 1);
            pwr[b] = pwr[b].max(level - d as f32 * 0.7);
        }
    }
    let max_hold = pwr.iter().map(|v| v + 4.0).collect();
    let wf_peak = pwr.iter().map(|v| v + 2.0 + rng.next() * 3.0).collect();
    let wf_row = pwr.iter().map(|v| v - 1.5 + rng.next() * 3.0).collect();
    let scope = (0..480).map(|k| ((k as f32 * 0.21).sin() * 9000.0 + rng.next() * 900.0) as i16).collect();
    FftFrame {
        center_hz: 146e6,
        rate_hz: 2e6,
        peak_iq: 0.3,
        inspect_hz: 146.04e6,
        scope_rate_hz: 12_000.0,
        channel_dbfs: -30.0,
        noise_dbfs: -100.0,
        scope,
        pwr,
        max_hold,
        wf_peak,
        wf_row,
        peaks: vec![
            Peak { freq_hz: 145.36e6, snr_db: 50.0, label: Some("NFM".into()) },
            Peak { freq_hz: 146.04e6, snr_db: 70.0, label: Some("P25".into()) },
            Peak { freq_hz: 146.72e6, snr_db: 30.0, label: None },
        ],
        ..Default::default()
    }
}

/// `f` as the server puts it on the wire (v5, no classification block).
fn encode(f: &FftFrame) -> Vec<u8> {
    let mut b = vec![proto::FFT_FRAME_MAGIC];
    b.extend_from_slice(&f.center_hz.to_le_bytes());
    b.extend_from_slice(&f.rate_hz.to_le_bytes());
    b.extend_from_slice(&f.peak_iq.to_le_bytes());
    b.extend_from_slice(&f.inspect_hz.to_le_bytes());
    b.extend_from_slice(&((f.scope_rate_hz / 10.0) as u16).to_le_bytes());
    b.push(0);
    b.extend_from_slice(&0u16.to_le_bytes());
    b.extend_from_slice(&f.channel_dbfs.to_le_bytes());
    b.extend_from_slice(&f.noise_dbfs.to_le_bytes());
    b.extend_from_slice(&(f.scope.len() as u32).to_le_bytes());
    for v in &f.scope {
        b.extend_from_slice(&v.to_le_bytes());
    }
    for n in [f.pwr.len(), f.max_hold.len(), f.peaks.len()] {
        b.extend_from_slice(&(n as u32).to_le_bytes());
    }
    for v in f.pwr.iter().chain(&f.max_hold) {
        b.extend_from_slice(&v.to_le_bytes());
    }
    for p in &f.peaks {
        b.extend_from_slice(&p.freq_hz.to_le_bytes());
        b.extend_from_slice(&p.snr_db.to_le_bytes());
        let l = p.label.as_deref().unwrap_or("");
        b.push(l.len() as u8);
        b.extend_from_slice(l.as_bytes());
    }
    b.push(0);
    b.extend_from_slice(&(f.wf_peak.len() as u32).to_le_bytes());
    for v in &f.wf_peak {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.extend_from_slice(&(f.wf_row.len() as u32).to_le_bytes());
    for v in &f.wf_row {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b
}

fn status() -> Status {
    Status {
        serial: "bench".into(),
        tuner: "RSP1A".into(),
        driver: "SDRplay".into(),
        gain_min_db: 0.0,
        gain_max_db: 96.0,
        gain_db: Some(22.0),
        rates_hz: vec![250e3, 500e3, 1e6, 2e6, 5e6],
        tune_min_hz: 1e3,
        tune_max_hz: 2.2e9,
        freq_hz: 146e6,
        rate_hz: 2e6,
        min_freq_hz: 145e6,
        max_freq_hz: 147e6,
        inspect_hz: 146.04e6,
        zoom: 1.0,
        bandwidth_hz: 12_500.0,
        mode: Mode::Nfm,
        ..Default::default()
    }
}

fn frames() -> usize {
    std::env::var("USDR_BENCH_FRAMES").ok().and_then(|s| s.parse().ok()).unwrap_or(600)
}

/// Times per frame, and texture bytes sent.
struct Tally {
    name: &'static str,
    times: Vec<Duration>,
    /// Of those, the time spent tessellating.
    tess: Duration,
    vertices: u64,
    tex_bytes: u64,
}

impl Tally {
    fn new(name: &'static str) -> Self {
        Tally { name, times: Vec::new(), tess: Duration::ZERO, vertices: 0, tex_bytes: 0 }
    }

    fn report(mut self) {
        self.times.sort();
        let n = self.times.len().max(1);
        let at = |p: f64| self.times[((n as f64 * p) as usize).min(n - 1)].as_secs_f64() * 1e3;
        let mean = self.times.iter().sum::<Duration>().as_secs_f64() * 1e3 / n as f64;
        println!(
            "BENCH {:<26} mean {:>7.3} ms  p50 {:>7.3}  p95 {:>7.3}  max {:>7.3}  tess {:>6.3}  vtx {:>6.0}  tex {:>8.1} KiB/frame",
            self.name,
            mean,
            at(0.5),
            at(0.95),
            at(1.0),
            self.tess.as_secs_f64() * 1e3 / n as f64,
            self.vertices as f64 / n as f64,
            self.tex_bytes as f64 / n as f64 / 1024.0,
        );
    }
}

fn input(t: f64, size: egui::Vec2) -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
        time: Some(t),
        predicted_dt: 1.0 / 60.0,
        ..Default::default()
    }
}

/// One frame: run the UI, tessellate it, count the texture bytes.
fn step(ctx: &egui::Context, t: f64, size: egui::Vec2, tally: Option<&mut Tally>, ui: impl FnMut(&mut egui::Ui)) {
    let t0 = Instant::now();
    let out = ctx.run_ui(input(t, size), ui);
    let t1 = Instant::now();
    let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
    let dt = t0.elapsed();
    if let Some(tally) = tally {
        tally.times.push(dt);
        tally.tess += t1.elapsed();
        tally.vertices += prims
            .iter()
            .map(|p| match &p.primitive {
                egui::epaint::Primitive::Mesh(m) => m.vertices.len() as u64,
                _ => 0,
            })
            .sum::<u64>();
        tally.tex_bytes += out
            .textures_delta
            .set
            .iter()
            .map(|(_, d)| (d.image.width() * d.image.height() * 4) as u64)
            .sum::<u64>();
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Case {
    /// A new spectrum every frame: the trace, a waterfall row.
    Live,
    /// No new spectrum: the display redrawn as it stands.
    Idle,
    /// A palette change every frame: the whole history recolored.
    Palette,
    /// A palette change every half second, crossfaded as the operator sees it.
    Fade,
    /// A retune every frame: the history shifted and recolored.
    Retune,
    /// The held waterfall, standing still while the spectrum arrives.
    Held,
    /// The held waterfall scrubbed back a row every frame.
    Scrub,
}

fn panafall_case(name: &'static str, case: Case, bins: usize) {
    if std::env::var("USDR_BENCH_ONLY").is_ok_and(|only| !name.contains(&only)) {
        return;
    }
    let ctx = egui::Context::default();
    let size = egui::vec2(1600.0, 800.0);
    let mut pan = Panafall::default();
    let mut view = View::default();
    let st = status();
    let flag = SliceFlag { freq_hz: 146.04e6, mode: Mode::Nfm, bandwidth_hz: 12_500.0, s_label: "S9".into(), ..Default::default() };
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut acts = Vec::new();
    let mut f = frame(bins, 0, &mut rng);
    // Fonts first, then fill the history so the waterfall is full.
    step(&ctx, 0.0, size, None, |ui| crate::theme::install(ui.ctx(), crate::theme::Ground::Carbon));
    for i in 0..800 {
        f = frame(bins, i, &mut rng);
        pan.push(&f, &view, i as f64 * 0.04);
    }
    if matches!(case, Case::Held | Case::Scrub) {
        pan.toggle_hold();
    }
    let mut tally = Tally::new(name);
    let n = frames();
    let start = 800.0 * 0.04;
    for i in 0..n + 30 {
        let t = start + i as f64 / 60.0;
        match case {
            Case::Live | Case::Held => {
                f = frame(bins, i, &mut rng);
                pan.push(&f, &view, t);
            }
            Case::Idle => {}
            Case::Scrub => pan.hold.as_mut().expect("held").back = i % 300,
            Case::Palette => {
                view.palette = if i % 2 == 0 { Colormap::Classic } else { Colormap::Ember };
            }
            Case::Fade => {
                f = frame(bins, i, &mut rng);
                pan.push(&f, &view, t);
                view.palette = if (i / 30) % 2 == 0 { Colormap::Classic } else { Colormap::Ember };
            }
            Case::Retune => {
                f = frame(bins, i, &mut rng);
                f.center_hz = 146e6 + (i % 2) as f64 * 20e3;
                pan.push(&f, &view, t);
            }
        }
        acts.clear();
        let tally = (i >= 30).then_some(&mut tally);
        step(&ctx, t, size, tally, |ui| {
            pan.ui(ui, &st, Some(&f), &mut view, &flag, 12_500.0, case == Case::Palette, &mut acts);
        });
    }
    tally.report();
}

#[test]
#[ignore]
fn bench_panafall() {
    panafall_case("panafall live 4096", Case::Live, 4096);
    panafall_case("panafall live 2048", Case::Live, 2048);
    panafall_case("panafall idle 4096", Case::Idle, 4096);
    panafall_case("panafall palette 4096", Case::Palette, 4096);
    panafall_case("panafall fade 4096", Case::Fade, 4096);
    panafall_case("panafall retune 4096", Case::Retune, 4096);
    panafall_case("panafall held 4096", Case::Held, 4096);
    panafall_case("panafall scrub 4096", Case::Scrub, 4096);
}

/// The whole window: masthead, receiver bar, side column, dock and scope,
/// a spectrum every other frame (the server's 25 fps under a 60 Hz display).
#[test]
#[ignore]
fn bench_window() {
    let dir = std::env::temp_dir().join(format!("usdr-bench-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    // SAFETY: set before anything reads it, on this test's own thread.
    unsafe { std::env::set_var("USDR_DESKTOP_PREFS", dir.join("desktop.toml")) };
    let ctx = egui::Context::default();
    let size = egui::vec2(1600.0, 1000.0);
    let prefs = Prefs { auto_connect: false, ..Default::default() };
    let mut app = App::build(&ctx, prefs, Player::silent(), Player::silent(), None);
    let (link, wire) = Link::wire();
    app.link = Some(link);
    let info = proto::ServerInfo { name: "usdr".into(), version: "bench".into(), api: 2, features: vec!["filter".into(), "rx2".into()] };
    wire.events.send(NetEvent::Connected(Box::new(status()), Some(info))).unwrap();
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let ground = app.prefs.ground;
    step(&ctx, 0.0, size, None, |ui| crate::theme::install(ui.ctx(), ground));
    for (bins, name) in [(2048, "window 2048"), (4096, "window 4096")] {
        let mut tally = Tally::new(name);
        let n = frames();
        for i in 0..n + 60 {
            let t = 1.0 + i as f64 / 60.0;
            if i % 2 == 0 {
                wire.events.send(NetEvent::Fft(Box::new(frame(bins, i, &mut rng)))).unwrap();
            }
            let tally = (i >= 60).then_some(&mut tally);
            step(&ctx, t, size, tally, |ui| {
                let ctx = ui.ctx().clone();
                app.tick(&ctx);
                app.draw(ui);
            });
            while wire.reqs.try_recv().is_ok() {}
        }
        tally.report();
    }
    // A busy digital channel: the decode log full, a few frames open.
    for k in 0..400 {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("nac".into(), "0x293".into());
        fields.insert("tgid".into(), format!("{}", 1000 + k));
        let event = proto::DecodeEvent {
            protocol: if k % 3 == 0 { "P25".into() } else { "DMR".into() },
            kind: "voice header".into(),
            summary: format!("group call TG {} from {}", 1000 + k, 4_000_000 + k * 17),
            valid: k % 11 != 0,
            fields,
        };
        wire.events.send(NetEvent::Decode { inspect_hz: 146.04e6, event, at: std::time::SystemTime::now() }).unwrap();
    }
    let mut tally = Tally::new("window 2048 decodes 400");
    let n = frames();
    for i in 0..n + 60 {
        let t = 30.0 + i as f64 / 60.0;
        if i % 2 == 0 {
            wire.events.send(NetEvent::Fft(Box::new(frame(2048, i, &mut rng)))).unwrap();
        }
        if i == 1 {
            for d in app.decodes.iter_mut().step_by(40) {
                d.open = true;
            }
        }
        let tally = (i >= 60).then_some(&mut tally);
        step(&ctx, t, size, tally, |ui| {
            let ctx = ui.ctx().clone();
            app.tick(&ctx);
            app.draw(ui);
        });
        while wire.reqs.try_recv().is_ok() {}
    }
    assert_eq!(app.decodes.len(), 400);
    assert!(app.pan.regions.is_some(), "the window drew the display");
    tally.report();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The pieces on their own: decoding a frame off the wire and taking it
/// into the display.
#[test]
#[ignore]
fn bench_pieces() {
    let mut rng = Rng(7);
    let frames: Vec<FftFrame> = (0..64).map(|i| frame(4096, i, &mut rng)).collect();
    let wire: Vec<Vec<u8>> = frames.iter().map(encode).collect();
    let n = self::frames() * 4;

    let mut tally = Tally::new("decode fft 4096");
    for i in 0..n {
        let t0 = Instant::now();
        std::hint::black_box(proto::decode_binary(&wire[i % wire.len()]).ok());
        tally.times.push(t0.elapsed());
    }
    tally.report();

    let mut pan = Panafall::default();
    let view = View::default();
    let mut tally = Tally::new("push 4096");
    for i in 0..n {
        let t0 = Instant::now();
        pan.push(&frames[i % frames.len()], &view, i as f64 * 0.04);
        tally.times.push(t0.elapsed());
    }
    tally.report();

    let view = View { wf_peak: true, ..Default::default() };
    let mut tally = Tally::new("push 4096 wf-peak");
    for i in 0..n {
        let t0 = Instant::now();
        pan.push(&frames[i % frames.len()], &view, i as f64 * 0.04);
        tally.times.push(t0.elapsed());
    }
    tally.report();
}

#[test]
#[ignore]
fn bench_startup_devices() {
    for _ in 0..3 {
        let t0 = Instant::now();
        let d = Player::devices();
        println!("BENCH audio device list: {} devices in {:.1} ms", d.len(), t0.elapsed().as_secs_f64() * 1e3);
    }
}

/// Renders of the whole window on the GPU, for looking at the layout: a
/// live NFM receiver at three window sizes, and one with receiver B on.
///
/// ```text
/// USDR_SHOTS=/some/dir cargo test -p usdr-desktop --release shots -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn shots() {
    use egui_kittest::Harness;
    let out = std::path::PathBuf::from(std::env::var("USDR_SHOTS").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into()));
    let dir = std::env::temp_dir().join(format!("usdr-shots-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    // SAFETY: set before anything reads it, on this test's own thread.
    unsafe { std::env::set_var("USDR_DESKTOP_PREFS", dir.join("desktop.toml")) };
    struct S {
        app: Option<App>,
        wire: Option<crate::net::Wire>,
        with_b: bool,
        wfm: bool,
        night: bool,
        shade: bool,
        title: bool,
    }
    // USDR_SHOTS_SIZES overrides the set: `1536x830,1600x980-b,1600x980-wfm`,
    // `1600x980-night` for the Night ground, `1000x56-shade` for the strip,
    // `1600x980-title` for the integrated title bar.
    let sizes: Vec<(f32, f32, bool, bool, bool, bool, bool)> = match std::env::var("USDR_SHOTS_SIZES") {
        Ok(v) => v
            .split(',')
            .filter_map(|s| {
                let (wh, rest) = s.trim().split_once('-').map_or((s.trim(), ""), |(a, b)| (a, b));
                let (w, h) = wh.split_once('x')?;
                let flags: Vec<&str> = rest.split('-').collect();
                Some((w.parse().ok()?, h.parse().ok()?, flags.contains(&"b"), flags.contains(&"wfm"), flags.contains(&"night"), flags.contains(&"shade"), flags.contains(&"title")))
            })
            .collect(),
        Err(_) => vec![
            (1600.0, 980.0, false, false, false, false, false),
            (1900.0, 980.0, false, false, false, false, false),
            (1400.0, 900.0, false, false, false, false, false),
            (1600.0, 980.0, true, false, false, false, false),
            (1600.0, 980.0, false, true, false, false, false),
        ],
    };
    for (w, h, with_b, wfm, night, shade, title) in sizes {
        let mut harness = Harness::builder().with_size(egui::vec2(w, h)).wgpu().build_ui_state(
            |ui, s: &mut S| {
                let ctx = ui.ctx().clone();
                if s.app.is_none() {
                    let ground = if s.night { crate::theme::Ground::Night } else { crate::theme::Ground::Carbon };
                    let mut prefs = Prefs { auto_connect: false, ground, ..Default::default() };
                    // USDR_SHOTS_STYLE=ember (any style's name) draws in that look.
                    if let Some(st) = std::env::var("USDR_SHOTS_STYLE")
                        .ok()
                        .and_then(|n| crate::spectrum::Style::ALL.into_iter().find(|st| st.label().eq_ignore_ascii_case(&n)))
                    {
                        prefs.view.style = st;
                        prefs.view.palette = st.look().palette;
                    }
                    // USDR_SHOTS_SPECTRUM=ridges draws the ridges.
                    if std::env::var("USDR_SHOTS_SPECTRUM").is_ok_and(|v| v.eq_ignore_ascii_case("ridges")) {
                        prefs.view.spectrum = crate::spectrum::SpecMode::Ridges;
                    }
                    let mut app = App::build(&ctx, prefs, Player::silent(), Player::silent(), None);
                    app.chrome.own_titlebar = s.title;
                    let (link, wire) = Link::wire();
                    app.link = Some(link);
                    let info = proto::ServerInfo { name: "usdr".into(), version: "shots".into(), api: 2, features: vec!["filter".into(), "rx2".into()] };
                    let mut st = status();
                    st.freq_error_hz = Some(1_900.0);
                    st.rf_notch = Some(false);
                    st.dab_notch = Some(false);
                    st.iq_correction = Some(true);
                    st.bias_t = Some(true);
                    st.lo_offset = true;
                    st.lo_offset_hz = 150e3;
                    st.agc_setpoint_dbfs = Some(-30.0);
                    st.rfgr_db = Some(8.0);
                    st.ifgr_db = Some(35.0);
                    st.if_bandwidths_hz = vec![200e3, 300e3, 600e3, 1536e3, 5e6, 6e6];
                    st.if_bandwidth_hz = Some(1536e3);
                    st.processing = Some(Default::default());
                    // The RSP under its IF AGC, the LNA settled where it
                    // follows it; overloaded at full LNA gain in the WFM shot.
                    st.gain_db = None;
                    st.tuner_agc = true;
                    st.lna_steps_db = vec![0.0, 6.0, 12.0, 18.0, 20.0, 26.0, 32.0, 38.0, 57.0];
                    st.lna_auto = true;
                    (st.rfgr_db, st.ifgr_db, st.overload, st.overload_count) =
                        if s.wfm { (Some(0.0), Some(59.0), Some(true), Some(3)) } else { (Some(7.0), Some(39.0), Some(false), Some(1)) };
                    st.avg = Some("off".into());
                    if s.with_b {
                        st.rx2 = Some(proto::SubRx { freq_hz: 146.52e6, mode: Mode::Nfm, bandwidth_hz: 12_500.0, passband_offset_hz: 0.0, note: None });
                    }
                    if s.wfm {
                        st.mode = Mode::Wfm;
                        st.bandwidth_hz = 150e3;
                        st.filter_low_hz = -75e3;
                        st.filter_high_hz = 75e3;
                        // A layout fixture, not a decode: the same synthetic
                        // station the engine's RDS tests transmit, unless
                        // USDR_SHOTS_STATUS names a status saved from a live
                        // server (`curl …/api/sdr/status`), whose RDS is then
                        // drawn instead.
                        let live = std::env::var_os("USDR_SHOTS_STATUS")
                            .and_then(|p| std::fs::read_to_string(p).ok())
                            .and_then(|j| serde_json::from_str::<proto::Status>(&j).ok());
                        st.rds = live.map(|l| l.rds).unwrap_or_else(|| Some(proto::Rds {
                            pi: "54A8".into(),
                            pi_call: Some("KXYZ".into()),
                            ps: Some("KXYZ FM".into()),
                            ps_steady: true,
                            pty: Some(9),
                            pty_name: Some("Top 40".into()),
                            tp: true,
                            rt: Some("Blue Horizon by The Examples".into()),
                            // The next text coming in, part received.
                            rt_partial: Some("Next: Glass Har\u{2591}\u{2591}\u{2591}\u{2591} by The Exam\u{2591}\u{2591}es".into()),
                            rt_progress: 0.72,
                            title: Some("Blue Horizon".into()),
                            artist: Some("The Examples".into()),
                            clock_utc: Some("2026-11-03T19:32Z".into()),
                            clock_offset_min: Some(-300),
                            locked: true,
                            pilot: true,
                            bler: 0.04,
                            updated_ms: 1_793_820_720_000,
                            ..Default::default()
                        }));
                        app.modes = vec![proto::ModeCaps {
                            mode: Mode::Wfm,
                            label: "WFM".into(),
                            bandwidth_hz: 150e3,
                            filter: Some(proto::FilterRange { min_width_hz: 50e3, max_width_hz: 250e3, max_edge_hz: 125e3, symmetric: true }),
                            ..Default::default()
                        }];
                        for k in 0..6 {
                            let mut fields = std::collections::BTreeMap::new();
                            fields.insert("nac".into(), "0x293".into());
                            let event = proto::DecodeEvent {
                                protocol: if k % 2 == 0 { "P25".into() } else { "DMR".into() },
                                kind: "voice header".into(),
                                summary: format!("group call TG {} from {}", 1000 + k, 4_000_000 + k * 17),
                                valid: true,
                                fields,
                            };
                            wire.events.send(NetEvent::Decode { inspect_hz: 146.04e6, event, at: std::time::SystemTime::now() }).unwrap();
                        }
                    }
                    wire.events.send(NetEvent::Connected(Box::new(st), Some(info))).unwrap();
                    crate::theme::install(&ctx, app.prefs.ground);
                    app.save_memories(vec![
                        crate::prefs::Memory { name: "NFM".into(), freq_hz: 146.04e6, mode: Mode::Nfm },
                        crate::prefs::Memory { name: "WFM".into(), freq_hz: 95.1e6, mode: Mode::Wfm },
                        crate::prefs::Memory { name: "NOAA".into(), freq_hz: 162.475e6, mode: Mode::Nfm },
                    ]);
                    s.app = Some(app);
                    s.wire = Some(wire);
                    return;
                }
                let app = s.app.as_mut().unwrap();
                if s.shade && app.shade.is_none() && app.have_status {
                    app.toggle_shade(&ctx);
                }
                app.tick(&ctx);
                app.draw(ui);
                if let Some(wire) = &s.wire {
                    while wire.reqs.try_recv().is_ok() {}
                }
            },
            S { app: None, wire: None, with_b, wfm, night, shade, title },
        );
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        harness.run_steps(2);
        for i in 0..360 {
            let mut f = frame(2048, i, &mut rng);
            f.classification = Some(proto::Classification { active: true, protocol: "Analog Voice (NFM)".into(), modulation: "FM".into(), snr_db: 38.0, rf_dbfs: -30.0, ..Default::default() });
            harness.state().wire.as_ref().unwrap().events.send(NetEvent::Fft(Box::new(f))).unwrap();
            harness.step();
        }
        harness.run_steps(20);
        let img = harness.render().expect("render");
        let name = format!(
            "shot-{}x{}{}{}{}{}{}.png",
            w as u32,
            h as u32,
            if with_b { "-b" } else { "" },
            if wfm { "-wfm" } else { "" },
            if night { "-night" } else { "" },
            if shade { "-shade" } else { "" },
            if title { "-title" } else { "" },
        );
        img.save(out.join(&name)).expect("save");
        println!("SHOT {}", out.join(&name).display());
    }
    let _ = std::fs::remove_dir_all(&dir);
}
