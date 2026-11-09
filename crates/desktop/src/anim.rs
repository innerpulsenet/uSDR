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
//! That is the reason for the hold in [`frame`] and for [`drift`]. And a
//! window that is not being shown is never given its frame at all, so the
//! spin would go on for as long as it is hidden: the ticker notices a paint
//! that is not coming and releases the loop (see [`release`]).

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
    ASKED_US.store(u64::MAX, Ordering::Relaxed);
}

/// The earliest paint asked for and not yet begun; none is MAX.
static ASKED_US: AtomicU64 = AtomicU64::new(u64::MAX);
/// When the event loop was last released, and how many times.
static RELEASED_US: AtomicU64 = AtomicU64::new(0);
static RELEASES: AtomicU64 = AtomicU64::new(0);
/// A paint asked for and not begun in this long is not coming: the window
/// is not being shown.
const STUCK_US: u64 = 300_000;
/// While that lasts, the loop is released again this often, in case
/// something set it spinning again.
const RELEASE_EVERY_US: u64 = 1_000_000;

fn asked_now() {
    ASKED_US.fetch_min(now_us(), Ordering::Relaxed);
}

/// When the loop is next due a release; MAX when no paint is outstanding.
fn stuck_due() -> u64 {
    if !cfg!(target_os = "linux") || std::env::var_os("USDR_NO_LOOP_RELEASE").is_some() {
        return u64::MAX;
    }
    match ASKED_US.load(Ordering::Relaxed) {
        u64::MAX => u64::MAX,
        asked => (asked + STUCK_US).max(RELEASED_US.load(Ordering::Relaxed) + RELEASE_EVERY_US),
    }
}

/// Stop eframe's event loop spinning for a frame the compositor will not
/// take.
///
/// On Wayland a window that is minimized, on another desktop or wholly
/// covered gets no frame callbacks, and winit cannot tell (it reports
/// neither visibility nor occlusion there). eframe answers every repaint
/// request by polling the event loop flat out until the redraw arrives,
/// so with a spectrum frame asking 25 times a second a hidden window held
/// a whole core, for as long as it was hidden. Measured: 101% CPU and not
/// one frame painted.
///
/// The loop goes back to waiting when eframe handles a repaint request it
/// has no window for, and a request for a viewport that does not exist is
/// one. The redraw already asked for stays asked for: the compositor's
/// next frame callback, when the window is shown again, wakes the loop
/// and it is painted then. The delay comes down by a second each time
/// because egui only passes on a request sooner than the last for the
/// same viewport. `USDR_NO_LOOP_RELEASE` turns this off.
fn release(ctx: &egui::Context) {
    let n = RELEASES.fetch_add(1, Ordering::Relaxed);
    let id = egui::ViewportId::from_hash_of(("usdr-loop-release", n / 3000));
    ctx.request_repaint_after_for(Duration::from_secs(3600 - n % 3000), id);
    RELEASED_US.store(now_us(), Ordering::Relaxed);
}

/// How many times the loop has been released: for the QA line and tests.
pub fn releases() -> u64 {
    RELEASES.load(Ordering::Relaxed)
}

/// One paint, and only one. egui answers a plain `request_repaint` with
/// two frames, the second to let a layout that changed settle; a request
/// with any delay at all gets a single one. A spectrum frame changes no
/// layout, and the second frame for each made a 25 fps picture cost 50
/// paints a second.
fn once(ctx: &egui::Context) {
    asked_now();
    ctx.request_repaint_after(Duration::from_nanos(1));
}

/// Paint now: something arrived that may change what is laid out (a
/// status, a decode, a connection). Painted twice, as egui does, so the
/// layout settles even if nothing follows it.
pub fn now(ctx: &egui::Context) {
    asked_now();
    ctx.request_repaint();
    start(ctx);
}

/// A spectrum frame arrived: paint it, once. Right behind another paint
/// it waits out the rest of [`DATA_GAP_US`].
pub fn frame(ctx: &egui::Context) {
    let since = now_us().saturating_sub(LAST_PAINT_US.load(Ordering::Relaxed));
    if since >= DATA_GAP_US || !STARTED.load(Ordering::Relaxed) {
        once(ctx);
        start(ctx);
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

/// How long the small motions take, in one place: a hover fade, a key going
/// down, a backlight sliding to another key, a wash of color coming up, and
/// the receiver edge's sweep.
pub const HOVER_S: f32 = 0.12;
pub const PRESS_S: f32 = 0.05;
pub const SLIDE_S: f32 = 0.16;
pub const WASH_S: f32 = 0.2;
pub const SWEEP_S: f32 = 0.45;

#[derive(Clone, Copy)]
struct Ease {
    from: f32,
    to: f32,
    t0: f64,
}

/// `on` as a value that takes `secs` to cross between 0 and 1. See
/// [`ease_value`].
#[track_caller]
pub fn ease_bool(ctx: &egui::Context, id: egui::Id, on: bool, secs: f32) -> f32 {
    ease_value(ctx, id, if on { 1.0 } else { 0.0 }, secs)
}

/// `target`, approached in a straight line over `secs` from wherever the
/// value stood when the target last changed; the target itself the first
/// time it is asked for, and with no time to take.
///
/// egui's own `animate_value_with_time` does the same but asks for its
/// frames with a plain `request_repaint`, which is two paints a request at
/// the display's full rate whatever the frame-rate setting says. This one
/// asks through [`more`], and only while the value is still moving.
#[track_caller]
pub fn ease_value(ctx: &egui::Context, id: egui::Id, target: f32, secs: f32) -> f32 {
    let now = ctx.input(|i| i.time);
    let at = |e: &Ease| {
        let k = if secs > 0.0 { ((now - e.t0) as f32 / secs).clamp(0.0, 1.0) } else { 1.0 };
        e.from + (e.to - e.from) * k
    };
    let mut e = ctx.data(|d| d.get_temp::<Ease>(id)).unwrap_or(Ease { from: target, to: target, t0: now });
    if e.to != target {
        e = Ease { from: at(&e), to: target, t0: now };
        ctx.data_mut(|d| d.insert_temp(id, e));
    } else if e.from == e.to && e.t0 == now {
        ctx.data_mut(|d| d.insert_temp(id, e));
    }
    let v = at(&e);
    if v != target {
        more(ctx);
    }
    v
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
            // A paint asked for that is not coming: the window is hidden.
            let stuck = stuck_due();
            if stuck <= now {
                release(&ctx);
                continue;
            }
            if due == u64::MAX {
                // Nothing wanted: asleep until something is, or until an
                // outstanding paint has had its time.
                std::thread::park_timeout(Duration::from_micros(stuck.saturating_sub(now).min(1_000_000)));
                continue;
            }
            if now < due {
                std::thread::park_timeout(Duration::from_micros((due - now).min(stuck.saturating_sub(now))));
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
