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
    struct Count(std::sync::atomic::AtomicU64);
    impl net::AudioSink for Count {
        fn push(&self, _rx: u8, _rate: u32, s: &[i16]) {
            self.0.fetch_add(s.len() as u64, std::sync::atomic::Ordering::Relaxed);
        }
    }
    let audio = Arc::new(Count(Default::default()));
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
    println!("stream    {frames} FFT frames in 4 s · {status} status · {decodes} decodes · {} audio samples", audio.0.load(std::sync::atomic::Ordering::Relaxed));
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
