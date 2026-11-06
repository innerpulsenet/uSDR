//! uSDR desktop: a native client for the uSDR server.
//!
//! `usdr-desktop [host:port]` opens the console, connecting to the server
//! given or the last one used. `usdr-desktop --probe host:port` connects
//! without a window, decodes a few seconds of the stream and prints what it
//! saw — a quick check that a server and this client agree on the wire.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod anim;
mod app;
mod audio;
mod bar;
mod bandplan;
#[cfg(test)]
mod bench;
mod brand;
mod chrome;
mod colormap;
mod decoders;
mod directory;
mod fax;
mod ft8view;
mod hfviews;
mod dock;
mod meter;
mod net;
mod panels;
mod prefs;
mod scope;
mod screens;
mod settings;
mod shade;
mod spectrum;
mod stations;
mod stations_data;
mod theme;
mod toolbar;
mod tuning;
mod utilviews;
mod vfo;
mod wav;
mod widgets;

fn icon() -> Option<egui::IconData> {
    let img = image::load_from_memory(include_bytes!("../assets/icon/usdr-256.png")).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    Some(egui::IconData { rgba: img.into_raw(), width: w, height: h })
}

fn probe(server: &str) -> anyhow::Result<()> {
    use std::sync::Arc;
    /// Samples heard from receivers A and B (as mono), the last second of
    /// each, and receiver A's last second of stereo with the decoder's
    /// readings over the run.
    #[derive(Default)]
    struct Count {
        heard: [std::sync::atomic::AtomicU64; 2],
        keep: std::sync::Mutex<[Vec<i16>; 2]>,
        stereo: std::sync::Mutex<(Vec<i16>, Vec<usdr_proto::StereoReading>)>,
    }
    impl net::AudioSink for Count {
        fn push(&self, f: &usdr_proto::AudioFrame) {
            let i = usize::from(f.rx.min(1));
            let mono: Vec<i16> = if f.channels() == 2 {
                f.samples.chunks_exact(2).map(|p| ((p[0] as i32 + p[1] as i32) / 2) as i16).collect()
            } else {
                f.samples.clone()
            };
            self.heard[i].fetch_add(mono.len() as u64, std::sync::atomic::Ordering::Relaxed);
            let mut keep = self.keep.lock().unwrap();
            keep[i].extend_from_slice(&mono);
            let n = keep[i].len();
            if n > 48_000 {
                keep[i].drain(..n - 48_000);
            }
            if let Some(r) = f.stereo {
                let mut st = self.stereo.lock().unwrap();
                st.1.push(r);
                if f.channels() == 2 {
                    st.0.extend_from_slice(&f.samples);
                    let n = st.0.len();
                    if n > 2 * 48_000 {
                        st.0.drain(..n - 2 * 48_000);
                    }
                }
            }
        }
    }
    let audio = Arc::new(Count::default());
    let link = net::Link::open(server, egui::Context::default(), audio.clone());
    let until = std::time::Instant::now() + std::time::Duration::from_secs(4);
    let (mut frames, mut status, mut decodes, mut presence) = (0, 0, 0, None);
    let mut last: Option<Box<usdr_proto::FftFrame>> = None;
    let mut hello = None;
    let mut latest: Option<Box<usdr_proto::Status>> = None;
    while std::time::Instant::now() < until {
        match link.events.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(net::NetEvent::Connected(s, info)) => {
                hello = Some(s);
                match info {
                    Some(i) => println!("server    {} {} · API {} · {}", i.name, i.version, i.api, i.features.join(", ")),
                    None => println!("server    predates API 2 (no presence, filter or shared memories)"),
                }
            }
            Ok(net::NetEvent::Fft(f)) => {
                link.stats.took_fft();
                frames += 1;
                last = Some(f);
            }
            Ok(net::NetEvent::Status(s)) => {
                status += 1;
                latest = Some(s);
            }
            Ok(net::NetEvent::Decode { .. }) => decodes += 1,
            Ok(net::NetEvent::Presence(p)) => presence = Some(p),
            Ok(net::NetEvent::Disconnected(r)) => println!("disconnected: {r}"),
            _ => {}
        }
    }
    let Some(s) = hello else { anyhow::bail!("no hello from {}", link.base) };
    println!("radio     {} · {} · mode {}", s.serial, s.radio_label(), s.mode.label());
    println!("tuning    center {:.6} MHz · receive {:.6} MHz · span {}", s.freq_hz / 1e6, s.inspect_hz / 1e6, tuning::fmt_hz(s.rate_hz));
    let heard = |i: usize| audio.heard[i].load(std::sync::atomic::Ordering::Relaxed);
    println!("stream    {frames} FFT frames in 4 s · {status} status · {decodes} decodes · audio A {} samples, B {}", heard(0), heard(1));
    {
        let keep = audio.keep.lock().unwrap();
        let rms = |v: &[i16]| (v.iter().map(|&x| (x as f64 / 32768.0).powi(2)).sum::<f64>() / v.len().max(1) as f64).sqrt();
        let db = |r: f64| 20.0 * r.max(1e-9).log10();
        let (a, b) = (&keep[0], &keep[1]);
        if !b.is_empty() {
            let n = a.len().min(b.len());
            let (a, b) = (&a[a.len() - n..], &b[b.len() - n..]);
            let dot: f64 = a.iter().zip(b).map(|(&x, &y)| x as f64 * y as f64).sum();
            let corr = dot / ((a.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() * b.iter().map(|&y| (y as f64).powi(2)).sum::<f64>()).sqrt()).max(1.0);
            println!("audio     A {:.1} dBFS rms · B {:.1} dBFS rms · A–B correlation {corr:+.2}", db(rms(a)), db(rms(b)));
        }
    }
    {
        // The stereo decoder, as the frames reported it, and what the last
        // second of L and R holds: the difference against the sum says how
        // much stereo the program carries.
        let st = audio.stereo.lock().unwrap();
        if let Some(last) = st.1.last() {
            let n = st.1.len().max(1) as f32;
            let lit = st.1.iter().filter(|r| r.pilot).count() as f32 / n;
            let (lo, hi) = st.1.iter().fold((f32::MAX, f32::MIN), |(a, b), r| (a.min(r.blend), b.max(r.blend)));
            println!(
                "stereo    {} · pilot in {:.0}% of frames{} · blend now {:.0}% (range {:.0}–{:.0}%){}",
                if last.channels == 2 { "2 channels" } else { "mono" },
                lit * 100.0,
                last.pilot_dev_hz.map(|h| format!(" at {:.2} kHz deviation", h / 1000.0)).unwrap_or_default(),
                last.blend * 100.0,
                lo * 100.0,
                hi * 100.0,
                last.snr_db.map(|d| format!(" · stereo S/N {d:.0} dB")).unwrap_or_default()
            );
            if st.0.len() >= 2 {
                let (mut m, mut d, mut lr) = (0f64, 0f64, 0f64);
                let (mut ll, mut rr) = (0f64, 0f64);
                for p in st.0.chunks_exact(2) {
                    let (l, r) = (p[0] as f64, p[1] as f64);
                    m += ((l + r) / 2.0).powi(2);
                    d += ((l - r) / 2.0).powi(2);
                    lr += l * r;
                    ll += l * l;
                    rr += r * r;
                }
                println!(
                    "l/r       L−R {:.1} dB against L+R · L–R correlation {:+.2} · L {:.1} dBFS, R {:.1} dBFS rms",
                    10.0 * (d.max(1e-9) / m.max(1e-9)).log10(),
                    lr / (ll * rr).sqrt().max(1.0),
                    10.0 * (ll / (st.0.len() / 2) as f64 / 32768f64.powi(2)).max(1e-12).log10(),
                    10.0 * (rr / (st.0.len() / 2) as f64 / 32768f64.powi(2)).max(1e-12).log10()
                );
            }
        } else if s.mode == usdr_proto::Mode::Wfm {
            println!("stereo    no reading in the audio (a server without stereo)");
        }
    }
    if let Some(b) = &s.rx2 {
        println!("rx B      {:.6} MHz · {} · {} wide{}", b.freq_hz / 1e6, b.mode.label(), tuning::fmt_hz(b.bandwidth_hz), b.note.as_ref().map(|n| format!(" · {n}")).unwrap_or_default());
    }
    // RDS, from the newest status the stream carried.
    let rds = latest.as_ref().map_or(s.rds.as_ref(), |l| l.rds.as_ref()).filter(|_| s.mode == usdr_proto::Mode::Wfm);
    match rds {
        Some(r) => {
            println!(
                "rds       PI {}{} · PS {} · {}{} · {:.0}% blocks unreadable{}",
                r.pi,
                r.pi_call.as_ref().map(|c| format!(" ({c} from PI)")).unwrap_or_default(),
                r.ps.as_deref().map(|p| format!("\"{p}\"")).unwrap_or_else(|| "not yet confirmed".into()),
                r.pty_name.as_deref().unwrap_or("no PTY"),
                if r.locked { " · locked" } else { " · lock lost" },
                r.bler * 100.0,
                if r.pilot { " · pilot carrier" } else { "" }
            );
            if let Some(rt) = &r.rt {
                println!("radiotext \"{rt}\"");
            }
            if r.title.is_some() || r.artist.is_some() {
                println!("rt+       title {} · artist {}", r.title.as_deref().unwrap_or("—"), r.artist.as_deref().unwrap_or("—"));
            }
            if let (Some(c), Some(o)) = (&r.clock_utc, r.clock_offset_min) {
                println!("clock     {c} (offset {o:+} min, as the station sent it)");
            }
        }
        None if s.mode == usdr_proto::Mode::Wfm => println!("rds       none confirmed on this station"),
        None => {}
    }
    if s.filter_high_hz > s.filter_low_hz {
        println!("filter    {:+.0} … {:+.0} Hz{}", s.filter_low_hz, s.filter_high_hz, if s.filter_custom { " (set by the operator)" } else { " (the mode's own)" });
    }
    if let Some(f) = last {
        let max = f.pwr.iter().copied().fold(f32::MIN, f32::max);
        let mut sorted = f.pwr.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        println!(
            "frame     {} bins over {:.6}–{:.6} MHz · floor {:.1} · max {:.1} dBFS · {} peaks · scope {} @ {} Hz ({:?}) · wf_peak {} · wf_row {}",
            f.pwr.len(),
            (f.center_hz - f.rate_hz / 2.0) / 1e6,
            (f.center_hz + f.rate_hz / 2.0) / 1e6,
            sorted[sorted.len() / 7],
            max,
            f.peaks.len(),
            f.scope.len(),
            f.scope_rate_hz,
            f.scope_kind,
            f.wf_peak.len(),
            f.wf_row.len()
        );
        println!("channel   {:.1} dBFS over a {:.1} dBFS floor", f.channel_dbfs, f.noise_dbfs);
    }
    match presence {
        Some(p) => println!("presence  {} client(s) · web {:?}", p.clients.len(), p.web),
        None => println!("presence  not reported (server predates it)"),
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--probe") {
        return probe(args.get(1).map(String::as_str).unwrap_or("127.0.0.1:8073"));
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("usdr-desktop [host:port]      open the console\nusdr-desktop --probe [host:port]   check the stream without a window");
        return Ok(());
    }
    let server = args.first().cloned();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("uSDR")
        .with_app_id("net.innerpulse.usdr-desktop")
        .with_inner_size([1600.0, 980.0])
        .with_min_inner_size([1100.0, 700.0]);
    if let Some(i) = icon() {
        viewport = viewport.with_icon(std::sync::Arc::new(i));
    }
    // The window's frame options take effect as it opens.
    let prefs = prefs::Prefs::load();
    if prefs.own_titlebar {
        viewport = viewport.with_decorations(false);
    }
    if cfg!(windows) && prefs.mica {
        viewport = viewport.with_transparent(true);
    }
    // QA: USDR_QA_SIZE=1740x1000 opens at that size, ignoring the saved one.
    let qa_size = std::env::var("USDR_QA_SIZE").ok().and_then(|v| {
        let (w, h) = v.split_once('x')?;
        Some([w.parse::<f32>().ok()?, h.parse::<f32>().ok()?])
    });
    if let Some(sz) = qa_size {
        // Its own app id, so the window manager does not restore the
        // everyday window's remembered size over it.
        viewport = viewport.with_inner_size(sz).with_maximized(false).with_app_id("net.innerpulse.usdr-desktop.qa");
    }
    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Wgpu,
        persist_window: qa_size.is_none(),
        ..Default::default()
    };
    eframe::run_native("uSDR", options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, server)))))
        .map_err(|e| anyhow::anyhow!("{e}"))
}
