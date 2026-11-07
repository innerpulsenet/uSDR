//! The two conversations with the server, each on its own thread.
//!
//! The websocket carries the stream: FFT frames at ~25 fps, audio, status,
//! decodes and presence. Audio skips the UI thread entirely and goes straight
//! to the player, so a slow frame never becomes a click; everything else
//! asks for a paint the moment it arrives. A stream that goes quiet is
//! pinged, and one that does not answer is dropped and reopened, so a
//! server that vanished is noticed in seconds rather than when TCP gives
//! up. REST carries the operator's changes; slider drags and wheel spins
//! arrive faster than they are worth sending, so queued posts to the same
//! control collapse to the newest before they leave, and a capture being
//! fetched or saved runs on a thread of its own, so a tune never waits
//! behind twenty seconds of I/Q.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel as unbounded};
use serde_json::Value;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use usdr_proto as proto;

/// Something the UI should know about.
pub enum NetEvent {
    Connecting { attempt: u32 },
    Connected(Box<proto::Status>, Option<proto::ServerInfo>),
    Disconnected(String),
    Fft(Box<proto::FftFrame>),
    Status(Box<proto::Status>),
    Decode { inspect_hz: f64, event: proto::DecodeEvent, at: std::time::SystemTime },
    Presence(proto::Presence),
    Station(proto::Station, proto::ReporterStatus),
    FaxLine(proto::FaxLine),
    ImageLine(proto::ImageLine),
    HellColumns(proto::HellColumns),
    Reply { what: Get, result: Result<Value, String> },
    Saved { path: PathBuf, bytes: u64 },
    Bytes { what: Fetch, bytes: Vec<u8>, headers: Vec<(String, String)> },
    Failed { what: String, error: String },
    /// The server answered 409: another client holds the receiver.
    Refused(String),
}

/// GETs whose answers the UI is waiting on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Get {
    Devices,
    Calls,
    Modes,
    P25Key,
    Calibrate,
    Replay,
    Memories,
}

/// Binary fetches the UI uses in memory rather than saving.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fetch {
    /// The rolling voice capture, to play.
    PlayCapture,
    /// The rolling I/Q capture, to send back for a replay decode.
    ReplayCapture,
    CallAudio(u64),
    /// The radiofax picture the server is receiving, as rows so far.
    Fax,
    /// A rolling capture to write as .iq + .json, or as a WAV.
    SaveCapture { kind: String, stem: PathBuf },
}

#[derive(Debug)]
pub enum Req {
    Post { path: String, body: Value, coalesce: bool },
    Get { path: String, what: Get },
    PostGet { path: String, body: Value, what: Get },
    Download { path: String, dest: PathBuf, sidecar: Option<Value> },
    Fetch { path: String, what: Fetch },
    Upload { path: String, body: Vec<u8>, what: Get },
}

impl Req {
    /// Posts to the same endpoint that set the same fields supersede each
    /// other; `{"bias_t":…}` never swallows `{"rf_notch":…}`.
    fn coalesce_key(&self) -> Option<String> {
        match self {
            Req::Post { path, body, coalesce: true } => {
                let mut k = path.clone();
                if let Value::Object(m) = body {
                    for key in m.keys() {
                        k.push('|');
                        k.push_str(key);
                    }
                }
                Some(k)
            }
            _ => None,
        }
    }
}

/// Throughput and timing, shared with the status bar.
#[derive(Default)]
pub struct Stats {
    pub bytes: AtomicU64,
    pub fft_frames: AtomicU64,
    pub audio_frames: AtomicU64,
    pub decode_errors: AtomicU64,
    /// Round trip of the last REST call, microseconds.
    pub rtt_us: AtomicU64,
    /// Spectrum frames handed to the UI and not yet taken by it, and how
    /// many were let go because it had stopped taking them.
    pub fft_waiting: AtomicU64,
    pub fft_dropped: AtomicU64,
}

/// The most spectrum frames that wait for the UI: three seconds of them.
///
/// The UI only takes what the stream brings when it paints, and a window
/// that is minimized, on another desktop or covered is not painted. The
/// queue had no bound, so every frame of the stream waited in it for as
/// long as the window was hidden: about a megabyte a second, a gigabyte
/// after a quarter of an hour behind another window. Past this many the
/// newest is let go instead; the audio never came this way.
pub const FFT_BACKLOG: u64 = 75;

impl Stats {
    /// Whether the UI has room for another spectrum frame; counts it in if
    /// so, and as dropped if not.
    pub fn admit_fft(&self) -> bool {
        if self.fft_waiting.load(Ordering::Relaxed) >= FFT_BACKLOG {
            self.fft_dropped.fetch_add(1, Ordering::Relaxed);
            false
        } else {
            self.fft_waiting.fetch_add(1, Ordering::Relaxed);
            true
        }
    }

    /// The UI took a spectrum frame.
    pub fn took_fft(&self) {
        let _ = self.fft_waiting.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| Some(n.saturating_sub(1)));
    }
}

pub trait AudioSink: Send + Sync {
    /// Audio from receiver `f.rx` (0 the main one, 1 the second): mono, or
    /// interleaved L, R when `f.channels()` is 2.
    fn push(&self, f: &proto::AudioFrame);
}

pub struct Link {
    pub base: String,
    pub stats: Arc<Stats>,
    req: Sender<Req>,
    pub events: Receiver<NetEvent>,
    stop: Arc<AtomicBool>,
}

impl Drop for Link {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// `host:port`, `http://host:port/` and the like, as `http://host:port`.
pub fn normalize_base(input: &str) -> String {
    let s = input.trim().trim_end_matches('/');
    let s = s
        .strip_prefix("ws://")
        .map(|r| format!("http://{r}"))
        .unwrap_or_else(|| s.to_string());
    let s = if s.starts_with("http://") || s.starts_with("https://") {
        s
    } else {
        format!("http://{s}")
    };
    // A bare host gets the server's default port.
    let after = s.split("://").nth(1).unwrap_or("");
    if after.contains(':') || after.contains('[') {
        s
    } else {
        format!("{s}:8073")
    }
}

pub fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "desktop".into())
}

/// The other ends of a test link: what the app asked for, and a way to
/// play the server's side.
#[cfg(test)]
pub struct Wire {
    pub reqs: Receiver<Req>,
    pub events: Sender<NetEvent>,
}

impl Link {
    /// A link with no threads behind it, for tests.
    #[cfg(test)]
    pub fn wire() -> (Link, Wire) {
        let (req_tx, req_rx) = unbounded::<Req>();
        let (ev_tx, ev_rx) = unbounded::<NetEvent>();
        let link = Link {
            base: "http://test:8073".into(),
            stats: Arc::new(Stats::default()),
            req: req_tx,
            events: ev_rx,
            stop: Arc::new(AtomicBool::new(false)),
        };
        (link, Wire { reqs: req_rx, events: ev_tx })
    }

    pub fn open(base: &str, ctx: egui::Context, audio: Arc<dyn AudioSink>) -> Link {
        let base = normalize_base(base);
        let (req_tx, req_rx) = unbounded::<Req>();
        let (ev_tx, ev_rx) = unbounded::<NetEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Stats::default());
        let client_id = {
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            (t.as_nanos() as u64) ^ ((std::process::id() as u64) << 32)
        };
        {
            let (base, ev, stop, stats, ctx) =
                (base.clone(), ev_tx.clone(), stop.clone(), stats.clone(), ctx.clone());
            std::thread::Builder::new()
                .name("usdr-ws".into())
                .spawn(move || ws_loop(base, client_id, ev, stop, stats, ctx, audio))
                .expect("spawn websocket thread");
        }
        {
            let (base, ev, stop, stats) = (base.clone(), ev_tx, stop.clone(), stats.clone());
            std::thread::Builder::new()
                .name("usdr-rest".into())
                .spawn(move || rest_loop(base, client_id, req_rx, ev, stop, stats, ctx))
                .expect("spawn REST thread");
        }
        Link { base, stats, req: req_tx, events: ev_rx, stop }
    }

    /// A change the operator made; superseded by a newer one to the same
    /// control if both are still queued.
    pub fn post(&self, path: &str, body: Value) {
        let _ = self.req.send(Req::Post { path: path.into(), body, coalesce: true });
    }

    /// A command that must arrive every time it is sent (scan control).
    pub fn post_each(&self, path: &str, body: Value) {
        let _ = self.req.send(Req::Post { path: path.into(), body, coalesce: false });
    }

    pub fn get(&self, path: &str, what: Get) {
        let _ = self.req.send(Req::Get { path: path.into(), what });
    }

    pub fn post_get(&self, path: &str, body: Value, what: Get) {
        let _ = self.req.send(Req::PostGet { path: path.into(), body, what });
    }

    pub fn download(&self, path: &str, dest: PathBuf, sidecar: Option<Value>) {
        let _ = self.req.send(Req::Download { path: path.into(), dest, sidecar });
    }

    pub fn fetch(&self, path: &str, what: Fetch) {
        let _ = self.req.send(Req::Fetch { path: path.into(), what });
    }

    pub fn upload(&self, path: &str, body: Vec<u8>, what: Get) {
        let _ = self.req.send(Req::Upload { path: path.into(), body, what });
    }
}

fn ws_url(base: &str, id: u64) -> String {
    let host = base
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let scheme = if base.starts_with("https://") { "wss" } else { "ws" };
    let name: String = hostname()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
        .collect();
    // Broadcast FM in stereo, with the decoder's reading; a server older
    // than that ignores the parameter and sends mono.
    format!("{scheme}://{host}/ws?client=desktop&id={id}&name={name}&audio=stereo")
}

fn ws_loop(
    base: String,
    id: u64,
    ev: Sender<NetEvent>,
    stop: Arc<AtomicBool>,
    stats: Arc<Stats>,
    ctx: egui::Context,
    audio: Arc<dyn AudioSink>,
) {
    let mut attempt = 0u32;
    while !stop.load(Ordering::Relaxed) {
        attempt += 1;
        let _ = ev.send(NetEvent::Connecting { attempt });
        crate::anim::now(&ctx);
        let reason = match tungstenite::connect(ws_url(&base, id)) {
            Ok((mut ws, _)) => {
                if let tungstenite::stream::MaybeTlsStream::Plain(s) = ws.get_mut() {
                    let _ = s.set_nodelay(true);
                    let _ = s.set_read_timeout(Some(Duration::from_millis(400)));
                }
                let mut said_hello = false;
                // The server sends something every 40 ms while it is well.
                // Quiet for two seconds, it is pinged; quiet for six, it
                // is gone.
                let mut heard = Instant::now();
                let mut pinged = false;
                loop {
                    if stop.load(Ordering::Relaxed) {
                        let _ = ws.close(None);
                        return;
                    }
                    let msg = ws.read();
                    if msg.is_ok() {
                        heard = Instant::now();
                        pinged = false;
                    }
                    match msg {
                        Ok(tungstenite::Message::Binary(b)) => {
                            stats.bytes.fetch_add(b.len() as u64, Ordering::Relaxed);
                            match proto::decode_binary(&b) {
                                Ok(proto::Binary::Fft(f)) => {
                                    stats.fft_frames.fetch_add(1, Ordering::Relaxed);
                                    if stats.admit_fft() {
                                        let _ = ev.send(NetEvent::Fft(f));
                                    }
                                    crate::anim::frame(&ctx);
                                }
                                Ok(proto::Binary::Audio(a)) => {
                                    stats.audio_frames.fetch_add(1, Ordering::Relaxed);
                                    audio.push(&a);
                                }
                                Ok(proto::Binary::FaxLine(l)) => {
                                    let _ = ev.send(NetEvent::FaxLine(l));
                                    crate::anim::now(&ctx);
                                }
                                Ok(proto::Binary::HellColumns(h)) => {
                                    let _ = ev.send(NetEvent::HellColumns(h));
                                    crate::anim::now(&ctx);
                                }
                                Ok(proto::Binary::ImageLine(l)) => {
                                    let _ = ev.send(NetEvent::ImageLine(l));
                                    crate::anim::now(&ctx);
                                }
                                Err(_) => {
                                    stats.decode_errors.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                        Ok(tungstenite::Message::Text(t)) => {
                            stats.bytes.fetch_add(t.len() as u64, Ordering::Relaxed);
                            match proto::decode_text(&t) {
                                Ok(proto::Event::Hello(s, info)) => {
                                    attempt = 0;
                                    said_hello = true;
                                    let _ = ev.send(NetEvent::Connected(s, info));
                                }
                                Ok(proto::Event::Status(s)) => {
                                    let _ = ev.send(NetEvent::Status(s));
                                }
                                Ok(proto::Event::Decode { inspect_hz, event }) => {
                                    let _ = ev.send(NetEvent::Decode {
                                        inspect_hz,
                                        event,
                                        at: std::time::SystemTime::now(),
                                    });
                                }
                                Ok(proto::Event::Presence(p)) => {
                                    let _ = ev.send(NetEvent::Presence(p));
                                }
                                Ok(proto::Event::Station(s, r)) => {
                                    let _ = ev.send(NetEvent::Station(s, r));
                                }
                                Ok(proto::Event::Other(_)) => {}
                                Err(_) => {
                                    stats.decode_errors.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            crate::anim::now(&ctx);
                        }
                        Ok(tungstenite::Message::Close(f)) => {
                            break f
                                .map(|f| f.reason.to_string())
                                .filter(|r| !r.is_empty())
                                .unwrap_or_else(|| "the server closed the stream".into());
                        }
                        Ok(_) => {}
                        Err(tungstenite::Error::Io(e))
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            let quiet = heard.elapsed();
                            if quiet > Duration::from_secs(6) {
                                break "stream stalled".to_string();
                            }
                            if quiet > Duration::from_secs(2) && !pinged {
                                pinged = true;
                                if let Err(e) = ws.send(tungstenite::Message::Ping(Default::default())) {
                                    break format!("stream lost · {}", plain_error(&e, &base));
                                }
                            }
                        }
                        Err(e) => {
                            break if said_hello {
                                format!("stream lost · {}", plain_error(&e, &base))
                            } else {
                                plain_error(&e, &base)
                            };
                        }
                    }
                }
            }
            Err(tungstenite::Error::Http(resp)) => match resp.status().as_u16() {
                403 => "the server refused this client".into(),
                404 => "no uSDR server at this address".into(),
                s => format!("the server answered HTTP {s}"),
            },
            Err(e) => plain_error(&e, &base),
        };
        let _ = ev.send(NetEvent::Disconnected(reason));
        crate::anim::now(&ctx);
        let backoff = Duration::from_millis((250u64 << attempt.min(4)).min(4000));
        let until = Instant::now() + backoff;
        while Instant::now() < until && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// A socket failure in words, without the stream URL and its query.
fn plain_error(e: &tungstenite::Error, base: &str) -> String {
    let host = base.trim_start_matches("http://").trim_start_matches("https://");
    let io = match e {
        tungstenite::Error::Io(io) => Some(io.kind()),
        tungstenite::Error::Url(_) => None,
        _ => None,
    };
    let text = e.to_string();
    match io {
        Some(std::io::ErrorKind::ConnectionRefused) => format!("nothing is listening at {host}"),
        Some(std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted) => "the server closed the connection".into(),
        Some(std::io::ErrorKind::TimedOut) => format!("{host} did not answer"),
        _ if text.contains("Connection refused") || text.contains("Unable to connect") => format!("nothing is listening at {host}"),
        _ if text.contains("without closing handshake") || text.contains("reset") => "the server closed the connection".into(),
        _ if text.contains("failed to lookup") || text.contains("resolve") => format!("cannot find {host}"),
        _ => text,
    }
}

/// Which of a batch of queued requests to send: the newest of each control
/// that a later one supersedes, everything else, all in send order.
fn newest_only(batch: &[Req]) -> Vec<bool> {
    (0..batch.len())
        .map(|i| match batch[i].coalesce_key() {
            Some(k) => !batch[i + 1..].iter().any(|r| r.coalesce_key().as_deref() == Some(&k)),
            None => true,
        })
        .collect()
}

/// A request that moves a capture or a file: it runs on a thread of its
/// own, so the controls behind it in the queue are not held up by it.
fn heavy(req: &Req) -> bool {
    matches!(req, Req::Download { .. } | Req::Fetch { .. } | Req::Upload { .. })
}

/// How long a request may take: a control answers in milliseconds, so one
/// that has not answered in a few seconds is a server that is gone, and
/// the queue behind it should not wait on it; a capture may be large.
fn deadline(req: &Req) -> Duration {
    if heavy(req) { Duration::from_secs(300) } else { Duration::from_secs(5) }
}

/// Tell the UI how a request ended. A 409 (another client holds the
/// receiver) is noted once per few seconds, not once per wheel click.
fn report(out: Result<(), Fail>, ev: &Sender<NetEvent>, refused_note: &Mutex<Instant>, ctx: &egui::Context) {
    if let Err((what, status, error)) = out {
        if status == 409 {
            let mut last = refused_note.lock().unwrap();
            if last.elapsed() > Duration::from_secs(3) {
                *last = Instant::now();
                let _ = ev.send(NetEvent::Refused(error));
            }
        } else {
            let _ = ev.send(NetEvent::Failed { what, error });
        }
    }
    crate::anim::now(ctx);
}

fn rest_loop(
    base: String,
    id: u64,
    rx: Receiver<Req>,
    ev: Sender<NetEvent>,
    stop: Arc<AtomicBool>,
    stats: Arc<Stats>,
    ctx: egui::Context,
) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .build()
        .into();
    let client = Arc::new(format!("desktop:{id}"));
    let base = Arc::new(base);
    let refused_note = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(10)));
    loop {
        let first = match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                continue;
            }
            Err(_) => return,
        };
        let mut batch = vec![first];
        loop {
            match rx.try_recv() {
                Ok(r) => batch.push(r),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        let keep = newest_only(&batch);
        for (req, keep) in batch.into_iter().zip(keep) {
            if !keep {
                continue;
            }
            if heavy(&req) {
                let (agent, base, client, ev, refused_note, ctx) = (agent.clone(), base.clone(), client.clone(), ev.clone(), refused_note.clone(), ctx.clone());
                let _ = std::thread::Builder::new().name("usdr-fetch".into()).spawn(move || {
                    let out = run(&agent, &base, &client, req, &ev);
                    report(out, &ev, &refused_note, &ctx);
                });
                continue;
            }
            let t0 = Instant::now();
            let out = run(&agent, &base, &client, req, &ev);
            stats.rtt_us.store(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
            report(out, &ev, &refused_note, &ctx);
        }
    }
}

type Fail = (String, u16, String);

fn body_text(resp: &mut ureq::http::Response<ureq::Body>) -> String {
    resp.body_mut()
        .with_config()
        .limit(64 * 1024)
        .read_to_string()
        .unwrap_or_default()
}

fn run(agent: &ureq::Agent, base: &str, client: &str, req: Req, ev: &Sender<NetEvent>) -> Result<(), Fail> {
    let fail = |what: &str, status: u16, e: String| (what.to_string(), status, e);
    let within = Some(deadline(&req));
    match req {
        Req::Post { path, body, .. } => {
            let mut resp = agent
                .post(format!("{base}{path}"))
                .config()
                .timeout_global(within)
                .build()
                .header(proto::CLIENT_HEADER, client)
                .send_json(&body)
                .map_err(|e| fail(&path, 0, e.to_string()))?;
            let status = resp.status().as_u16();
            if status >= 400 {
                return Err(fail(&path, status, body_text(&mut resp)));
            }
            Ok(())
        }
        Req::Get { path, what } => {
            let r = agent
                .get(format!("{base}{path}"))
                .config()
                .timeout_global(within)
                .build()
                .header(proto::CLIENT_HEADER, client)
                .call();
            let result = json_result(r);
            let _ = ev.send(NetEvent::Reply { what, result });
            Ok(())
        }
        Req::PostGet { path, body, what } => {
            let r = agent
                .post(format!("{base}{path}"))
                .config()
                .timeout_global(within)
                .build()
                .header(proto::CLIENT_HEADER, client)
                .send_json(&body);
            let result = json_result(r);
            let _ = ev.send(NetEvent::Reply { what, result });
            Ok(())
        }
        Req::Upload { path, body, what } => {
            let r = agent
                .post(format!("{base}{path}"))
                .config()
                .timeout_global(within)
                .build()
                .header(proto::CLIENT_HEADER, client)
                .header("content-type", "audio/wav")
                .send(&body[..]);
            let result = json_result(r);
            let _ = ev.send(NetEvent::Reply { what, result });
            Ok(())
        }
        Req::Fetch { path, what } => {
            let mut resp = agent
                .get(format!("{base}{path}"))
                .config()
                .timeout_global(within)
                .build()
                .header(proto::CLIENT_HEADER, client)
                .call()
                .map_err(|e| fail(&path, 0, e.to_string()))?;
            let status = resp.status().as_u16();
            if status >= 400 {
                return Err(fail(&path, status, body_text(&mut resp)));
            }
            let headers = resp
                .headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                .collect();
            let bytes = resp
                .body_mut()
                .with_config()
                .limit(512 * 1024 * 1024)
                .read_to_vec()
                .map_err(|e| fail(&path, 0, e.to_string()))?;
            let _ = ev.send(NetEvent::Bytes { what, bytes, headers });
            Ok(())
        }
        Req::Download { path, dest, sidecar } => {
            let mut resp = agent
                .get(format!("{base}{path}"))
                .config()
                .timeout_global(within)
                .build()
                .header(proto::CLIENT_HEADER, client)
                .call()
                .map_err(|e| fail(&path, 0, e.to_string()))?;
            let status = resp.status().as_u16();
            if status >= 400 {
                return Err(fail(&path, status, body_text(&mut resp)));
            }
            let headers: serde_json::Map<String, Value> = resp
                .headers()
                .iter()
                .filter(|(k, _)| k.as_str().starts_with("x-usdr-"))
                .map(|(k, v)| (k.to_string(), Value::String(v.to_str().unwrap_or("").into())))
                .collect();
            if let Some(dir) = dest.parent() {
                std::fs::create_dir_all(dir).map_err(|e| fail(&path, 0, e.to_string()))?;
            }
            let mut file = std::fs::File::create(&dest).map_err(|e| fail(&path, 0, e.to_string()))?;
            let mut reader = resp.body_mut().with_config().limit(1 << 31).reader();
            let bytes = std::io::copy(&mut reader, &mut file).map_err(|e| fail(&path, 0, e.to_string()))?;
            if let Some(mut meta) = sidecar {
                if let Value::Object(m) = &mut meta {
                    m.insert("server_headers".into(), Value::Object(headers));
                    m.insert("bytes".into(), Value::from(bytes));
                }
                let json = dest.with_extension("json");
                let text = serde_json::to_string_pretty(&meta).unwrap_or_default();
                std::fs::write(&json, text).map_err(|e| fail(&path, 0, e.to_string()))?;
            }
            let _ = ev.send(NetEvent::Saved { path: dest, bytes });
            Ok(())
        }
    }
}

fn json_result(r: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<Value, String> {
    let mut resp = r.map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let mut text = String::new();
    resp.body_mut()
        .with_config()
        .limit(16 * 1024 * 1024)
        .reader()
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    if status >= 400 {
        return Err(if text.is_empty() { format!("HTTP {status}") } else { text });
    }
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(path: &str, body: Value) -> Req {
        Req::Post { path: path.into(), body, coalesce: true }
    }

    #[test]
    fn spectrum_frames_do_not_pile_up_behind_a_ui_that_is_not_painting() {
        // A quarter of an hour of frames with nobody taking them (a window
        // behind another): no more than the backlog ever waits.
        let stats = Stats::default();
        let mut sent = 0u64;
        for _ in 0..25 * 900 {
            if stats.admit_fft() {
                sent += 1;
            }
        }
        assert_eq!(sent, FFT_BACKLOG);
        assert_eq!(stats.fft_dropped.load(Ordering::Relaxed), 25 * 900 - FFT_BACKLOG);
        // Shown again, the UI takes what waited and the stream flows.
        for _ in 0..sent {
            stats.took_fft();
        }
        assert!(stats.admit_fft());
        // A frame taken that was never counted in (a test's own) is no debt.
        let fresh = Stats::default();
        fresh.took_fft();
        assert_eq!(fresh.fft_waiting.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_dragged_control_sends_only_its_last_value() {
        let batch = vec![
            post("/api/sdr/gain", serde_json::json!({"gain_db": 10})),
            post("/api/sdr/frontend", serde_json::json!({"bias_t": true})),
            post("/api/sdr/gain", serde_json::json!({"gain_db": 11})),
            post("/api/sdr/frontend", serde_json::json!({"rf_notch": true})),
            Req::Post { path: "/api/sdr/scan/control".into(), body: serde_json::json!({"action":"skip"}), coalesce: false },
            Req::Post { path: "/api/sdr/scan/control".into(), body: serde_json::json!({"action":"skip"}), coalesce: false },
            post("/api/sdr/gain", serde_json::json!({"gain_db": 12})),
        ];
        // Gain keeps its last; the two front-end posts set different
        // switches and both go; scan control is never merged.
        assert_eq!(newest_only(&batch), vec![false, true, false, true, true, true, true]);
    }

    #[test]
    fn captures_leave_the_control_queue() {
        let tune = post("/api/sdr/inspect", serde_json::json!({"freq_hz": 1e8}));
        let save = Req::Download { path: "/api/sdr/capture.wav?kind=iq".into(), dest: "/tmp/x.iq".into(), sidecar: None };
        assert!(!heavy(&tune) && heavy(&save));
        assert!(deadline(&tune) < Duration::from_secs(10), "a control that has not answered in seconds is a server that is gone");
        assert!(deadline(&save) > Duration::from_secs(60), "a capture may be large");
    }

    #[test]
    fn server_addresses() {
        assert_eq!(normalize_base("rsp"), "http://rsp:8073");
        assert_eq!(normalize_base("10.0.0.5:9000/"), "http://10.0.0.5:9000");
        assert_eq!(normalize_base("ws://h:1"), "http://h:1");
        assert_eq!(normalize_base("https://h.example"), "https://h.example:8073");
    }
}
