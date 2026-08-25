//! uSDR — a single-radio spectrum inspector for RTL-SDR hardware.
//!
//! One dongle, one web page: a live FFT/waterfall over the tuned span, and a
//! narrowband inspect chain that runs the same digital classifiers and
//! protocol decoders the full scanner uses. There is no scanning, no call
//! recording, and no database — this exists to point a receiver at a signal
//! and find out what it is.

mod devices;
mod sdr;

use sdr::SdrEvent;

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router, body::Bytes};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio::sync::broadcast::error::RecvError;

/// The web client is compiled into the binary, so there is exactly one file to
/// copy around and no static directory to get out of step with the server.
const INDEX_HTML: &str = include_str!("../web/index.html");

/// Largest I/Q capture the replay endpoint will decode.
const REPLAY_LIMIT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(name = "usdr", about = "RTL-SDR spectrum inspector and digital classifier")]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:8073")]
    bind: SocketAddr,
    /// Dongle serial to use. Defaults to the first one found.
    #[arg(long)]
    serial: Option<String>,
    /// Centre frequency in MHz.
    #[arg(long)]
    freq: Option<f64>,
    /// Sample rate (span) in MHz.
    #[arg(long)]
    rate: Option<f64>,
    /// Tuner gain in dB. Omit for AGC.
    #[arg(long)]
    gain: Option<f64>,
    /// Settings file. Tuning changes made in the browser are saved here.
    #[arg(long)]
    config: Option<PathBuf>,
    /// List the dongles the driver can see, then exit.
    #[arg(long)]
    devices: bool,
}

// ---------- persisted settings ----------
//
// Only what the browser can change, so a restart comes back up where the last
// session left off. Anything given on the command line wins for this run and
// is then written back, which keeps the file and the running radio agreed.

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Settings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    serial: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    freq_hz: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rate_hz: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gain_db: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<sdr::SdrMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lo_offset: Option<bool>,
    /// Off by default: a gain set by hand should stay where it was put.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    clip_guard: Option<bool>,
}

fn settings_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/usdr/usdr.toml"))
}

fn load_settings(path: &PathBuf) -> Settings {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Settings::default();
    };
    match toml::from_str(&text) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ignoring unreadable {}: {e}", path.display());
            Settings::default()
        }
    }
}

fn save_settings(path: &PathBuf, s: &Settings) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let text = toml::to_string_pretty(s).context("serialising settings")?;
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

struct AppState {
    sdr: Mutex<Option<sdr::SdrRuntime>>,
    sdr_events: tokio::sync::broadcast::Sender<sdr::SdrEvent>,
    audio: tokio::sync::broadcast::Sender<sdr::AudioFrame>,
    settings: Mutex<Settings>,
    config_path: PathBuf,
    shutdown: Notify,
    /// Debounce for settings writes. Dragging a slider or clicking around the
    /// waterfall fires a REST call per step; serialising the TOML and hitting
    /// the disk on each one is wasted I/O, since only the last value matters.
    last_settings_write: std::sync::Mutex<Option<std::time::Instant>>,
}

/// Minimum spacing between settings-file writes.
const SETTINGS_WRITE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

impl AppState {
    /// Record a browser-made change and put it on disk, at most one write per
    /// [`SETTINGS_WRITE_INTERVAL`], carrying whatever the latest state is. A
    /// failed write is worth saying out loud but is never worth failing the
    /// tuning request the operator actually asked for.
    fn update_settings(&self, edit: impl FnOnce(&mut Settings)) {
        let to_save = {
            let mut s = self.settings.lock().expect("settings");
            edit(&mut s);
            let due = match *self.last_settings_write.lock().expect("settings debounce") {
                Some(at) => at.elapsed() >= SETTINGS_WRITE_INTERVAL,
                None => true,
            };
            if !due {
                // A pending change still lands: the next call past the
                // interval writes the whole current Settings, not a delta.
                return;
            }
            *self.last_settings_write.lock().expect("settings debounce") =
                Some(std::time::Instant::now());
            s.clone()
        };
        if let Err(e) = save_settings(&self.config_path, &to_save) {
            eprintln!("could not save settings: {e:#}");
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if args.devices {
        let found = scannerd_radio::device::enumerate()?;
        if found.is_empty() {
            println!("no RTL-SDR devices found");
        }
        for d in &found {
            println!("{}\t{}\t{}", d.serial, d.tuner, d.label);
        }
        return Ok(());
    }

    let bind = args.bind;
    let config_path = settings_path(args.config)?;
    let mut settings = load_settings(&config_path);
    if args.serial.is_some() {
        settings.serial = args.serial.clone();
    }
    if let Some(mhz) = args.freq {
        settings.freq_hz = Some(mhz * 1e6);
    }
    if let Some(mhz) = args.rate {
        settings.rate_hz = Some(mhz * 1e6);
    }
    if args.gain.is_some() {
        settings.gain_db = args.gain;
    }

    let cfg = sdr::SdrCfg {
        serial: settings.serial.clone(),
        freq_hz: settings.freq_hz.unwrap_or(sdr::DEFAULT_FREQ_HZ),
        rate_hz: settings.rate_hz.unwrap_or(sdr::DEFAULT_RATE_HZ),
        gain_db: settings.gain_db,
        ppm: 0.0,
        fft_size: sdr::DEFAULT_FFT_SIZE,
        mode: settings.mode.unwrap_or_default(),
        lo_offset: settings.lo_offset.unwrap_or(false),
        clip_guard: settings.clip_guard.unwrap_or(false),
    };

    let (sdr_events, _) = tokio::sync::broadcast::channel(256);
    // Audio is a live monitor: a client that falls behind should skip forward
    // to the present rather than play a growing delay, so the queue is short.
    let (audio_tx, _) = tokio::sync::broadcast::channel(32);
    let runtime = sdr::spawn(cfg, sdr_events.clone(), audio_tx.clone())?;

    let state = Arc::new(AppState {
        sdr: Mutex::new(Some(runtime)),
        sdr_events,
        audio: audio_tx,
        settings: Mutex::new(settings),
        config_path,
        shutdown: Notify::new(),
        last_settings_write: std::sync::Mutex::new(None),
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/devices", get(list_devices))
        .route("/api/sdr/tune", post(post_sdr_tune))
        .route("/api/sdr/inspect", post(post_sdr_inspect))
        .route("/api/sdr/device", post(post_sdr_device))
        .route("/api/sdr/gain", post(post_sdr_gain))
        .route("/api/sdr/rate", post(post_sdr_rate))
        .route("/api/sdr/mode", post(post_sdr_mode))
        .route("/api/sdr/frontend", post(post_sdr_frontend))
        .route("/api/sdr/spurs", post(post_sdr_spurs))
        .route("/api/sdr/ppm", post(post_sdr_ppm))
        .route("/api/sdr/zoom", post(post_sdr_zoom))
        .route("/api/sdr/avg", post(post_sdr_avg))
        .route("/api/sdr/calibrate", post(post_sdr_calibrate))
        .route("/api/sdr/status", get(get_sdr_status))
        .route("/api/sdr/calls", get(get_sdr_calls))
        .route("/api/sdr/calls/{id}/audio.wav", get(get_sdr_call_audio))
        .route("/api/sdr/capture.wav", get(get_sdr_capture))
        // A 20 s I/Q capture is a few MiB, well past axum's 2 MiB default.
        // The handler enforces the real ceiling; this just lets the body reach it.
        .route(
            "/api/sdr/replay",
            post(post_sdr_replay).layer(DefaultBodyLimit::max(REPLAY_LIMIT_BYTES)),
        )
        .route("/ws", get(ws_upgrade))
        .with_state(Arc::clone(&state));

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    println!("uSDR listening on http://{}", listener.local_addr()?);

    let shutdown_state = Arc::clone(&state);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            println!("\nshutting down");
            shutdown_state.shutdown.notify_waiters();
        })
        .await?;

    // Dropping the runtime stops the worker thread and releases the dongle.
    state.sdr.lock().expect("sdr").take();
    Ok(())
}

async fn index() -> impl IntoResponse {
    // Never let a browser keep an old client across a rebuild.
    ([(header::CACHE_CONTROL, "no-store")], Html(INDEX_HTML))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceBody {
    serial: String,
    label: String,
    tuner: String,
    /// `sdr` for the dongle this server is using, `free` for anything else the
    /// driver can see.
    role: &'static str,
}

async fn list_devices(State(st): State<Arc<AppState>>) -> Result<Json<Vec<DeviceBody>>, ApiError> {
    let found = tokio::task::block_in_place(scannerd_radio::device::enumerate)?;
    let in_use = st
        .sdr
        .lock()
        .expect("sdr")
        .as_ref()
        .map(|s| s.status.lock().expect("sdr status").serial.clone());
    let out = found
        .into_iter()
        .map(|d| {
            let role = if in_use.as_deref() == Some(d.serial.as_str()) {
                "sdr"
            } else {
                "free"
            };
            DeviceBody {
                serial: d.serial,
                label: d.label,
                tuner: d.tuner,
                role,
            }
        })
        .collect();
    Ok(Json(out))
}

#[derive(Deserialize)]
struct SdrTuneReq {
    freq_hz: f64,
}

async fn post_sdr_tune(
    State(st): State<Arc<AppState>>,
    Json(req): Json<SdrTuneReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if req.freq_hz < 1.0e6 || req.freq_hz > 2.2e9 {
        return Err(ApiError::BadRequest(
            "frequency outside 1 MHz – 2.2 GHz".into(),
        ));
    }
    st.update_settings(|s| s.freq_hz = Some(req.freq_hz));
    if let Some(sdr) = st.sdr.lock().expect("sdr").as_ref() {
        sdr.tune(req.freq_hz)?;
    }
    Ok(Json(
        serde_json::json!({ "ok": true, "freq_hz": req.freq_hz }),
    ))
}

#[derive(Deserialize)]
struct SdrInspectReq {
    freq_hz: f64,
}

async fn post_sdr_inspect(
    State(st): State<Arc<AppState>>,
    Json(req): Json<SdrInspectReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if let Some(sdr) = st.sdr.lock().expect("sdr").as_ref() {
        sdr.inspect(req.freq_hz)?;
    }
    Ok(Json(
        serde_json::json!({ "ok": true, "freq_hz": req.freq_hz }),
    ))
}

#[derive(Deserialize)]
struct SdrDeviceReq {
    serial: String,
}

async fn post_sdr_device(
    State(st): State<Arc<AppState>>,
    Json(req): Json<SdrDeviceReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    st.update_settings(|s| s.serial = Some(req.serial.clone()));
    if let Some(sdr) = st.sdr.lock().expect("sdr").as_ref() {
        sdr.switch_device(req.serial.clone())?;
    }
    Ok(Json(
        serde_json::json!({ "ok": true, "serial": req.serial }),
    ))
}

#[derive(Deserialize)]
struct SdrGainReq {
    gain_db: Option<f64>,
}

async fn post_sdr_gain(
    State(st): State<Arc<AppState>>,
    Json(req): Json<SdrGainReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    st.update_settings(|s| s.gain_db = req.gain_db);
    if let Some(sdr) = st.sdr.lock().expect("sdr").as_ref() {
        sdr.set_gain(req.gain_db)?;
    }
    Ok(Json(
        serde_json::json!({ "ok": true, "gain_db": req.gain_db }),
    ))
}

#[derive(Deserialize)]
struct SdrRateReq {
    rate_hz: f64,
}

async fn post_sdr_rate(
    State(st): State<Arc<AppState>>,
    Json(req): Json<SdrRateReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !(200_000.0..=3_200_000.0).contains(&req.rate_hz) {
        return Err(ApiError::BadRequest(
            "rate must be between 200 kHz and 3.2 MHz".into(),
        ));
    }
    st.update_settings(|s| s.rate_hz = Some(req.rate_hz));
    if let Some(sdr) = st.sdr.lock().expect("sdr").as_ref() {
        sdr.set_rate(req.rate_hz)?;
    }
    Ok(Json(
        serde_json::json!({ "ok": true, "rate_hz": req.rate_hz }),
    ))
}

#[derive(Deserialize)]
struct SdrModeReq {
    mode: sdr::SdrMode,
}

async fn post_sdr_mode(
    State(st): State<Arc<AppState>>,
    Json(req): Json<SdrModeReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    st.update_settings(|s| s.mode = Some(req.mode));
    if let Some(sdr) = st.sdr.lock().expect("sdr").as_ref() {
        sdr.set_mode(req.mode)?;
    }
    Ok(Json(
        serde_json::json!({ "ok": true, "mode": req.mode }),
    ))
}

#[derive(Deserialize)]
struct SdrFrontendReq {
    lo_offset: Option<bool>,
    clip_guard: Option<bool>,
}

async fn post_sdr_frontend(
    State(st): State<Arc<AppState>>,
    Json(req): Json<SdrFrontendReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if let Some(on) = req.lo_offset {
        st.update_settings(|s| s.lo_offset = Some(on));
    }
    if let Some(on) = req.clip_guard {
        st.update_settings(|s| s.clip_guard = Some(on));
    }
    if let Some(sdr) = st.sdr.lock().expect("sdr").as_ref() {
        if let Some(on) = req.lo_offset {
            sdr.set_lo_offset(on)?;
        }
        if let Some(on) = req.clip_guard {
            sdr.set_clip_guard(on)?;
        }
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct SdrSpursReq {
    /// Offsets from the display centre, in Hz.
    offsets_hz: Vec<f64>,
}

async fn post_sdr_spurs(
    State(st): State<Arc<AppState>>,
    Json(req): Json<SdrSpursReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if req.offsets_hz.len() > 32 {
        return Err(ApiError::BadRequest("too many spurs".into()));
    }
    if let Some(sdr) = st.sdr.lock().expect("sdr").as_ref() {
        sdr.set_spurs(req.offsets_hz.clone())?;
    }
    Ok(Json(
        serde_json::json!({ "ok": true, "count": req.offsets_hz.len() }),
    ))
}

#[derive(Deserialize)]
struct SdrZoomReq {
    zoom: f64,
}

#[derive(Deserialize)]
struct SdrAvgReq {
    /// "off", "slow" (~0.5 s), or "deep" (~2 s).
    mode: String,
}

async fn post_sdr_avg(
    State(st): State<Arc<AppState>>,
    Json(req): Json<SdrAvgReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mode = match req.mode.as_str() {
        "off" => sdr::AvgMode::Off,
        "slow" => sdr::AvgMode::Slow,
        "deep" => sdr::AvgMode::Deeper,
        other => {
            return Err(ApiError::BadRequest(format!(
                "avg mode must be off, slow, or deep — got {other}"
            )))
        }
    };
    if let Some(sdr) = st.sdr.lock().expect("sdr").as_ref() {
        sdr.set_avg(mode)?;
    }
    Ok(Json(serde_json::json!({ "ok": true, "mode": req.mode })))
}

async fn post_sdr_zoom(
    State(st): State<Arc<AppState>>,
    Json(req): Json<SdrZoomReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !(1.0..=sdr::ZOOM_MAX).contains(&req.zoom) {
        return Err(ApiError::BadRequest(format!(
            "zoom must be between 1 and {}",
            sdr::ZOOM_MAX
        )));
    }
    if let Some(sdr) = st.sdr.lock().expect("sdr").as_ref() {
        sdr.set_zoom(req.zoom)?;
    }
    Ok(Json(serde_json::json!({ "ok": true, "zoom": req.zoom })))
}

#[derive(Deserialize)]
struct SdrPpmReq {
    ppm: f64,
}

async fn post_sdr_ppm(
    State(st): State<Arc<AppState>>,
    Json(req): Json<SdrPpmReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !(-200.0..=200.0).contains(&req.ppm) {
        return Err(ApiError::BadRequest(
            "ppm must be between -200 and 200".into(),
        ));
    }
    if let Some(sdr) = st.sdr.lock().expect("sdr").as_ref() {
        sdr.set_ppm(req.ppm)?;
    }
    Ok(Json(serde_json::json!({ "ok": true, "ppm": req.ppm })))
}

/// Work out the crystal error from a signal whose true frequency is known.
///
/// The receiver measures where the tuned channel's energy actually sits; the
/// difference against a reference the operator vouches for is the error, and
/// the correction is that difference as a fraction of the frequency. Anything
/// with a known, accurate carrier will do — a broadcast station, a P25 control
/// channel, a commercial repeater.
async fn post_sdr_calibrate(
    State(st): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let guard = st.sdr.lock().expect("sdr");
    let sdr = guard
        .as_ref()
        .ok_or_else(|| ApiError::BadRequest("the receiver is not running".into()))?;
    let (err_hz, inspect_hz, ppm_now) = {
        let s = sdr.status.lock().expect("sdr status");
        (s.freq_error_hz, s.inspect_hz, s.ppm)
    };
    let err_hz = err_hz.ok_or_else(|| {
        ApiError::BadRequest(
            "no signal to calibrate against — tune to a carrier you know the frequency of".into(),
        )
    })?;
    if inspect_hz <= 0.0 {
        return Err(ApiError::BadRequest("nothing tuned".into()));
    }
    // A measured error of `err` at `f` means the receiver's idea of frequency
    // is off by err/f; fold that into whatever correction is already applied.
    let ppm = ppm_now - (err_hz / inspect_hz) * 1e6;
    if !(-200.0..=200.0).contains(&ppm) {
        return Err(ApiError::BadRequest(format!(
            "measured correction {ppm:.1} ppm is outside the plausible range — \
             is the reference frequency right?"
        )));
    }
    sdr.set_ppm(ppm)?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "ppm": ppm,
        "measuredErrorHz": err_hz,
        "referenceHz": inspect_hz,
    })))
}

async fn get_sdr_status(
    State(st): State<Arc<AppState>>,
) -> Result<Json<Option<sdr::SdrStatus>>, ApiError> {
    let s = st
        .sdr
        .lock()
        .expect("sdr")
        .as_ref()
        .map(|r| r.status.lock().expect("sdr status").clone());
    Ok(Json(s))
}

/// The digital voice calls heard recently, newest first.
async fn get_sdr_calls(
    State(st): State<Arc<AppState>>,
) -> Result<Json<Vec<sdr::RecordedCall>>, ApiError> {
    let calls = st
        .sdr
        .lock()
        .expect("sdr")
        .as_ref()
        .map(|r| {
            let ring = r.calls.lock().expect("calls");
            ring.iter().rev().cloned().collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(Json(calls))
}

async fn get_sdr_call_audio(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<u64>,
) -> Result<Response, ApiError> {
    let call = st
        .sdr
        .lock()
        .expect("sdr")
        .as_ref()
        .and_then(|r| {
            let ring = r.calls.lock().expect("calls");
            ring.iter().find(|c| c.id == id).cloned()
        })
        .ok_or(ApiError::NotFound)?;
    let wav = sdr::pcm_wav_public(&call.audio, call.rate_hz, 1);
    Ok((
        [
            (header::CONTENT_TYPE, "audio/wav"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        wav,
    )
        .into_response())
}

#[derive(Deserialize)]
struct SdrCaptureQuery {
    kind: Option<String>,
}

async fn get_sdr_capture(
    State(st): State<Arc<AppState>>,
    Query(query): Query<SdrCaptureQuery>,
) -> Result<Response, ApiError> {
    let kind = match query.kind.as_deref().unwrap_or("voice") {
        "voice" => sdr::CaptureKind::Voice,
        "discriminator" | "disc" | "symbols" => sdr::CaptureKind::Discriminator,
        "iq" => sdr::CaptureKind::Iq,
        _ => {
            return Err(ApiError::BadRequest(
                "capture kind must be voice, discriminator, symbols, or iq".into(),
            ));
        }
    };
    let (freq_hz, samples, wav) = st
        .sdr
        .lock()
        .expect("sdr")
        .as_ref()
        .map(|runtime| runtime.capture_wav(kind))
        .ok_or_else(|| ApiError::BadRequest("the receiver is not running".into()))?;
    Ok((
        [
            (header::CONTENT_TYPE, "audio/wav"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        [
            ("x-usdr-frequency-hz", freq_hz.round().to_string()),
            ("x-usdr-samples", samples.to_string()),
        ],
        wav,
    )
        .into_response())
}

#[derive(Deserialize)]
struct SdrReplayQuery {
    frequency_hz: Option<f64>,
}

async fn post_sdr_replay(
    State(st): State<Arc<AppState>>,
    Query(query): Query<SdrReplayQuery>,
    body: Bytes,
) -> Result<Json<sdr::ReplayResult>, ApiError> {
    if body.len() > REPLAY_LIMIT_BYTES {
        return Err(ApiError::BadRequest(format!(
            "capture exceeds the {} MiB replay limit",
            REPLAY_LIMIT_BYTES / (1024 * 1024)
        )));
    }
    let live_hz = st
        .sdr
        .lock()
        .expect("sdr")
        .as_ref()
        .map(|runtime| runtime.status.lock().expect("sdr status").inspect_hz)
        .unwrap_or(0.0);
    let frequency_hz = query.frequency_hz.unwrap_or(live_hz);
    let replay = sdr::replay_iq_wav(&body, frequency_hz)
        .map_err(|error| ApiError::BadRequest(error.to_string()))?;
    Ok(Json(replay))
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(st): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| ws_client(socket, st))
}

async fn ws_client(mut socket: WebSocket, st: Arc<AppState>) {
    let mut sdr_ev = st.sdr_events.subscribe();
    let mut audio = st.audio.subscribe();

    // Describe what the client is looking at before anything starts streaming.
    let sdr_st = st
        .sdr
        .lock()
        .expect("sdr")
        .as_ref()
        .map(|s| s.status.lock().expect("sdr status").clone());
    let Some(sdr_st) = sdr_st else { return };
    let hello = serde_json::json!({ "type": "hello", "sdr": sdr_st });
    if socket
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            ev = sdr_ev.recv() => match ev {
                Ok(ev) => match &ev {
                    SdrEvent::Fft { .. } => {
                        // The FFT frame is the 25 fps bulk of the stream; a
                        // JSON float array costs ~10 bytes/bin for values the
                        // client only ever compares and scales. Ship it as
                        // binary instead — layout documented in
                        // sdr::encode_fft_frame. Everything else (status,
                        // decode events) stays JSON.
                        if let Some(buf) = sdr::encode_fft_frame(&ev)
                            && socket.send(Message::Binary(buf.into())).await.is_err()
                        {
                            return;
                        }
                    }
                    _ => {
                        let Ok(text) = serde_json::to_string(&ev) else { continue };
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            return;
                        }
                    }
                }
                // A client that fell behind has missed frames it no longer
                // wants; the next FFT carries the whole picture, so it simply
                // resumes from the present.
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            },
            frame = audio.recv() => match frame {
                Ok(f) => {
                    // [0x01][u32 LE sample rate][i16 LE samples...]. Binary
                    // because a JSON float array costs about ten times the
                    // bytes for something that arrives forty times a second.
                    let mut buf = Vec::with_capacity(5 + f.samples.len() * 2);
                    buf.push(0x01u8);
                    buf.extend_from_slice(&f.rate_hz.to_le_bytes());
                    for s in &f.samples {
                        buf.extend_from_slice(&s.to_le_bytes());
                    }
                    if socket.send(Message::Binary(buf.into())).await.is_err() {
                        return;
                    }
                }
                // Audio is a live monitor. A client that fell behind wants the
                // present, not a backlog played late.
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            },
            // Without this, graceful shutdown waits forever on a socket that
            // would stream until the browser tab closes.
            () = st.shutdown.notified() => return,
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | None => return,
                Some(Err(_)) => return,
                _ => {}
            },
        }
    }
}

enum ApiError {
    NotFound,
    BadRequest(String),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::Internal(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
            ApiError::Internal(e) => {
                eprintln!("request failed: {e:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
            }
        }
    }
}
