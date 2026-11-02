//! The animation clock.
//!
//! A short `request_repaint_after` keeps eframe's event loop spinning on
//! this platform (a full core at the display's 120 Hz for a 25 fps
//! picture), so everything that wants a frame — a new spectrum, a moving
//! needle, the waterfall's sub-row scroll — asks this ticker instead. It
//! wakes the UI at most 30 times a second, and egui paints two passes per
//! wake-up, so the window draws at up to 60 fps and idles when nothing moves.

use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static WANT: AtomicBool = AtomicBool::new(false);
static START: Once = Once::new();

/// Ask for another frame within about 33 ms.
pub fn more(ctx: &egui::Context) {
    WANT.store(true, Ordering::Relaxed);
    START.call_once(|| {
        let ctx = ctx.clone();
        let _ = std::thread::Builder::new().name("usdr-anim".into()).spawn(move || loop {
            std::thread::sleep(Duration::from_millis(33));
            if WANT.swap(false, Ordering::Relaxed) {
                ctx.request_repaint();
            }
        });
    });
}
