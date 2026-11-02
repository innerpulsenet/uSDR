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
mod bandplan;
mod brand;
mod colormap;
mod dock;
mod meter;
mod net;
mod panels;
mod prefs;
mod scope;
mod screens;
mod spectrum;
mod theme;
mod tuning;
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
    /// Samples heard from receivers A and B, and the last second of each.
    struct Count([std::sync::atomic::AtomicU64; 2], std::sync::Mutex<[Vec<i16>; 2]>);
    impl net::AudioSink for Count {
        fn push(&self, rx: u8, _rate: u32, s: &[i16]) {
            let i = usize::from(rx.min(1));
            self.0[i].fetch_add(s.len() as u64, std::sync::atomic::Ordering::Relaxed);
            let mut keep = self.1.lock().unwrap();
            keep[i].extend_from_slice(s);
            let n = keep[i].len();
            if n > 48_000 {
                keep[i].drain(..n - 48_000);
            }
        }
    }
    let audio = Arc::new(Count(Default::default(), Default::default()));
    let link = net::Link::open(server, egui::Context::default(), audio.clone());
    let until = std::time::Instant::now() + std::time::Duration::from_secs(4);
    let (mut frames, mut status, mut decodes, mut presence) = (0, 0, 0, None);
    let mut last: Option<Box<usdr_proto::FftFrame>> = None;
    let mut hello = None;
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
                frames += 1;
                last = Some(f);
            }
            Ok(net::NetEvent::Status(_)) => status += 1,
            Ok(net::NetEvent::Decode { .. }) => decodes += 1,
            Ok(net::NetEvent::Presence(p)) => presence = Some(p),
            Ok(net::NetEvent::Disconnected(r)) => println!("disconnected: {r}"),
            _ => {}
        }
    }
    let Some(s) = hello else { anyhow::bail!("no hello from {}", link.base) };
    println!("radio     {} · {} · mode {}", s.serial, s.radio_label(), s.mode.label());
    println!("tuning    center {:.6} MHz · receive {:.6} MHz · span {}", s.freq_hz / 1e6, s.inspect_hz / 1e6, tuning::fmt_hz(s.rate_hz));
    let heard = |i: usize| audio.0[i].load(std::sync::atomic::Ordering::Relaxed);
    println!("stream    {frames} FFT frames in 4 s · {status} status · {decodes} decodes · audio A {} samples, B {}", heard(0), heard(1));
    {
        let keep = audio.1.lock().unwrap();
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
    if let Some(b) = &s.rx2 {
        println!("rx B      {:.6} MHz · {} · {} wide{}", b.freq_hz / 1e6, b.mode.label(), tuning::fmt_hz(b.bandwidth_hz), b.note.as_ref().map(|n| format!(" · {n}")).unwrap_or_default());
    }
    if s.filter_high_hz > s.filter_low_hz {
        println!("filter    {:+.0} … {:+.0} Hz{}", s.filter_low_hz, s.filter_high_hz, if s.filter_custom { " (set by the operator)" } else { " (the mode's own)" });
    }
    if let Some(f) = last {
        let max = f.pwr.iter().copied().fold(f32::MIN, f32::max);
        let mut sorted = f.pwr.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        println!(
            "frame     {} bins over {:.6}–{:.6} MHz · floor {:.1} · max {:.1} dBFS · {} peaks · scope {} @ {} Hz ({:?}) · wf_peak {}",
            f.pwr.len(),
            (f.center_hz - f.rate_hz / 2.0) / 1e6,
            (f.center_hz + f.rate_hz / 2.0) / 1e6,
            sorted[sorted.len() / 7],
            max,
            f.peaks.len(),
            f.scope.len(),
            f.scope_rate_hz,
            f.scope_kind,
            f.wf_peak.len()
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
    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native("uSDR", options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, server)))))
        .map_err(|e| anyhow::anyhow!("{e}"))
}
