//! The animation clock.
//!
//! A short `request_repaint_after` keeps eframe's event loop spinning on
//! this platform (a full core at the display's 120 Hz for a 25 fps
//! picture), so everything that wants a frame — a new spectrum, a moving
//! needle, a palette crossfade — asks this ticker instead. It
//! wakes the UI at most 30 times a second, and egui paints two passes per
//! wake-up, so the window draws at up to 60 fps and idles when nothing moves.

use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

static WANT: AtomicBool = AtomicBool::new(false);
static START: Once = Once::new();
/// Milliseconds between wake-ups: 33 draws up to 60 fps, 66 up to 30.
static INTERVAL_MS: AtomicU64 = AtomicU64::new(33);

/// Cap the drawing rate. Half the frames costs about half the CPU.
pub fn set_max_fps(fps: u32) {
    let ms = if fps <= 30 { 66 } else { 33 };
    INTERVAL_MS.store(ms, Ordering::Relaxed);
}

/// Ask for another frame within about 33 ms.
pub fn more(ctx: &egui::Context) {
    WANT.store(true, Ordering::Relaxed);
    START.call_once(|| {
        let ctx = ctx.clone();
        let _ = std::thread::Builder::new().name("usdr-anim".into()).spawn(move || loop {
            std::thread::sleep(Duration::from_millis(INTERVAL_MS.load(Ordering::Relaxed)));
            if WANT.swap(false, Ordering::Relaxed) {
                ctx.request_repaint();
            }
        });
    });
}
