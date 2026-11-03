//! The animation clock.
//!
//! Two kinds of thing want a frame. Data (a spectrum frame, a status, a
//! decode) asks for one at once through [`now`]: it is painted the moment
//! it arrives, never held for a tick. Motion (a moving needle, a palette
//! crossfade, a hover) asks through [`more`], and a ticker wakes the UI for
//! it at the frame rate, measured from the last paint, so an animation runs
//! at a steady rate and a frame painted for data counts toward it rather
//! than being followed by a second one a few milliseconds later. Nothing
//! wanting a frame, the window idles.
//!
//! A short `request_repaint_after` would spin eframe's event loop on this
//! platform (a full core at the display's 120 Hz for a 25 fps picture),
//! which is why the ticker is a thread of its own.

use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

static WANT: AtomicBool = AtomicBool::new(false);
static START: Once = Once::new();
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
    WANT.store(false, Ordering::Relaxed);
    LAST_PAINT_US.store(now_us(), Ordering::Relaxed);
}

/// Paint now: something arrived that the picture should show.
pub fn now(ctx: &egui::Context) {
    ctx.request_repaint();
}

/// Ask for another frame, at the motion rate.
pub fn more(ctx: &egui::Context) {
    WANT.store(true, Ordering::Relaxed);
    start(ctx);
}

/// Ask for a frame in `d` from now (a clock that shows seconds, a change
/// held back to settle). egui's own `request_repaint_after` spins the
/// loop on this platform; the ticker keeps the appointment instead.
pub fn after(ctx: &egui::Context, d: Duration) {
    let at = now_us().saturating_add(d.as_micros() as u64);
    DEADLINE_US.fetch_min(at, Ordering::Relaxed);
    start(ctx);
}

fn start(ctx: &egui::Context) {
    START.call_once(|| {
        let ctx = ctx.clone();
        let _ = std::thread::Builder::new().name("usdr-anim".into()).spawn(move || loop {
            let interval = INTERVAL_US.load(Ordering::Relaxed);
            let now = now_us();
            let motion = if WANT.load(Ordering::Relaxed) { LAST_PAINT_US.load(Ordering::Relaxed) + interval } else { u64::MAX };
            let deadline = DEADLINE_US.load(Ordering::Relaxed);
            let due = motion.min(deadline);
            if due == u64::MAX {
                // Nothing wanted: look again in a frame's time.
                std::thread::sleep(Duration::from_micros(interval));
                continue;
            }
            if now < due {
                std::thread::sleep(Duration::from_micros(due - now));
                continue;
            }
            if deadline <= now {
                let _ = DEADLINE_US.compare_exchange(deadline, u64::MAX, Ordering::Relaxed, Ordering::Relaxed);
            }
            if motion <= now {
                WANT.store(false, Ordering::Relaxed);
            }
            ctx.request_repaint();
            // The frame it brings about records itself; until then, no
            // second wake-up for the same want.
            LAST_PAINT_US.store(now, Ordering::Relaxed);
        });
    });
}
