//! The animation clock.
//!
//! Three kinds of thing want a frame. Data asks for one as it arrives: a
//! spectrum frame through [`frame`], painted once and held only long
//! enough to stay clear of the frame before it; a status, a decode or a
//! connection through [`now`]. Motion (a
//! moving needle, a palette crossfade, a hover) asks through [`more`], and
//! a ticker wakes the UI for it at the frame rate, measured from the last
//! paint, so an animation runs at a steady rate and a frame painted for
//! data counts toward it rather than being followed by a second one a few
//! milliseconds later. Drift (an afterglow fading, a scale settling its
//! last fraction of a pixel) asks through [`drift`]: it rides the frames
//! data brings and only gets its own, slowly, when nothing else paints.
//! Nothing wanting a frame, the window idles.
//!
//! A short `request_repaint_after` would spin eframe's event loop on this
//! platform (a full core at the display's 120 Hz for a 25 fps picture),
//! which is why the ticker is a thread of its own. The loop also spins
//! from any repaint request until the compositor is ready for the frame,
//! so a request that lands right behind a paint burns the wait: measured
//! at 55 frames a second, a fifth of the UI thread went to that spin.
//! That is the reason for the hold in [`frame`] and for [`drift`].

use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The shortest interval motion was asked for this frame, in microseconds;
/// none is MAX.
static WANT_US: AtomicU64 = AtomicU64::new(u64::MAX);
static START: Once = Once::new();
/// Drift's own rate when nothing else paints: 20 frames a second.
const DRIFT_US: u64 = 50_000;
/// A data frame is held until the paint before it is this old, by when
/// the compositor has taken that frame (a refresh at 120 Hz, most of one
/// at 60) and the next can be drawn without the loop spinning for it.
const DATA_GAP_US: u64 = 9_000;
/// Microseconds between frames for motion: 16 667 for 60 fps, 33 333 for 30.
static INTERVAL_US: AtomicU64 = AtomicU64::new(16_667);
/// When the last frame began, microseconds since the clock was first read.
static LAST_PAINT_US: AtomicU64 = AtomicU64::new(0);
/// The earliest moment a frame was asked for by [`after`]; none is MAX.
static DEADLINE_US: AtomicU64 = AtomicU64::new(u64::MAX);

fn epoch() -> Instant {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn now_us() -> u64 {
    epoch().elapsed().as_micros() as u64
}

/// Cap the rate motion is drawn at. Half the frames costs about half the CPU.
pub fn set_max_fps(fps: u32) {
    let us = if fps <= 30 { 33_333 } else { 16_667 };
    INTERVAL_US.store(us, Ordering::Relaxed);
}

/// A frame is being painted: motion that still wants one asks again
/// during it, and the ticker counts its interval from here.
pub fn frame_begin() {
    WANT_US.store(u64::MAX, Ordering::Relaxed);
    LAST_PAINT_US.store(now_us(), Ordering::Relaxed);
}

/// One paint, and only one. egui answers a plain `request_repaint` with
/// two frames, the second to let a layout that changed settle; a request
/// with any delay at all gets a single one. A spectrum frame changes no
/// layout, and the second frame for each made a 25 fps picture cost 50
/// paints a second.
fn once(ctx: &egui::Context) {
    ctx.request_repaint_after(Duration::from_nanos(1));
}

/// Paint now: something arrived that may change what is laid out (a
/// status, a decode, a connection). Painted twice, as egui does, so the
/// layout settles even if nothing follows it.
pub fn now(ctx: &egui::Context) {
    ctx.request_repaint();
}

/// A spectrum frame arrived: paint it, once. Right behind another paint
/// it waits out the rest of [`DATA_GAP_US`].
pub fn frame(ctx: &egui::Context) {
    let since = now_us().saturating_sub(LAST_PAINT_US.load(Ordering::Relaxed));
    if since >= DATA_GAP_US || !STARTED.load(Ordering::Relaxed) {
        once(ctx);
    } else {
        after(ctx, Duration::from_micros(DATA_GAP_US - since));
    }
}

/// Ask for another frame, at the motion rate.
#[track_caller]
pub fn more(ctx: &egui::Context) {
    let us = INTERVAL_US.load(Ordering::Relaxed);
    let before = WANT_US.fetch_min(us, Ordering::Relaxed);
    note(std::panic::Location::caller());
    start(ctx);
    if before > us {
        wake();
    }
}

/// Keep moving, but there is no hurry: the frames data brings will do,
/// and with none arriving a slow rate of its own. For motion too small or
/// too slow to be worth a frame to itself (a fade, a last half pixel).
pub fn drift(ctx: &egui::Context) {
    let us = DRIFT_US.max(INTERVAL_US.load(Ordering::Relaxed));
    let before = WANT_US.fetch_min(us, Ordering::Relaxed);
    start(ctx);
    if before > us {
        wake();
    }
}

/// Who asked for motion, counted while `USDR_QA_FPS` is set: the frame-rate
/// line it prints names what kept the window painting.
static ASKED: std::sync::Mutex<Option<std::collections::BTreeMap<(&'static str, u32), u32>>> = std::sync::Mutex::new(None);

fn note(at: &'static std::panic::Location<'static>) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("USDR_QA_FPS").is_some()) {
        *ASKED.lock().unwrap().get_or_insert_default().entry((at.file(), at.line())).or_default() += 1;
    }
}

/// The motion requests since the last call, most frequent first.
pub fn asked() -> Vec<(String, u32)> {
    let mut v: Vec<(String, u32)> = ASKED
        .lock()
        .unwrap()
        .take()
        .unwrap_or_default()
        .into_iter()
        .map(|((f, l), n)| (format!("{}:{l}", f.rsplit('/').next().unwrap_or(f)), n))
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v
}

/// Ask for a frame in `d` from now (a clock that shows seconds, a change
/// held back to settle). egui's own `request_repaint_after` spins the
/// loop on this platform; the ticker keeps the appointment instead.
pub fn after(ctx: &egui::Context, d: Duration) {
    let at = now_us().saturating_add(d.as_micros() as u64);
    let before = DEADLINE_US.fetch_min(at, Ordering::Relaxed);
    start(ctx);
    if before > at {
        wake();
    }
}

static STARTED: AtomicBool = AtomicBool::new(false);
/// The ticker, to be woken when an appointment nearer than the one it is
/// sleeping toward is made.
static TICKER: std::sync::OnceLock<std::thread::Thread> = std::sync::OnceLock::new();

fn wake() {
    if let Some(t) = TICKER.get() {
        t.unpark();
    }
}

fn start(ctx: &egui::Context) {
    START.call_once(|| {
        STARTED.store(true, Ordering::Relaxed);
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new().name("usdr-anim".into()).spawn(move || loop {
            let now = now_us();
            let want = WANT_US.load(Ordering::Relaxed);
            let motion = if want != u64::MAX { LAST_PAINT_US.load(Ordering::Relaxed) + want } else { u64::MAX };
            let deadline = DEADLINE_US.load(Ordering::Relaxed);
            let due = motion.min(deadline);
            if due == u64::MAX {
                // Nothing wanted: asleep until something is.
                std::thread::park_timeout(Duration::from_secs(1));
                continue;
            }
            if now < due {
                std::thread::park_timeout(Duration::from_micros(due - now));
                continue;
            }
            if deadline <= now {
                let _ = DEADLINE_US.compare_exchange(deadline, u64::MAX, Ordering::Relaxed, Ordering::Relaxed);
            }
            if motion <= now {
                WANT_US.store(u64::MAX, Ordering::Relaxed);
            }
            once(&ctx);
            // The frame it brings about records itself; until then, no
            // second wake-up for the same want.
            LAST_PAINT_US.store(now, Ordering::Relaxed);
        });
        if let Ok(h) = spawned {
            let _ = TICKER.set(h.thread().clone());
        }
    });
}
