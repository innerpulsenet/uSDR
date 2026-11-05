//! Where a click, a wheel notch or a typed number puts the receiver.
//!
//! Same rules as the web page: the readout is the frequency being received,
//! clicks snap to the raster the mode implies, and the span only moves when
//! the receive frequency would leave it (or would sit within a channel of
//! its edge).

use usdr_proto::Mode;

/// The channel raster a mode's signals are allocated on.
pub fn mode_raster_hz(mode: Mode, near_hz: f64) -> f64 {
    match mode {
        Mode::Usb | Mode::Lsb | Mode::Ft8 | Mode::Ft4 | Mode::Wefax | Mode::Rtty | Mode::Navtex | Mode::Psk31 => 100.0,
        Mode::Wfm => 100_000.0,
        Mode::Packet | Mode::Flex => 2_500.0,
        Mode::Pager => 25_000.0,
        // Airband is on the 8.33 kHz raster; everything else AM on 10 kHz.
        Mode::Am if (118e6..=137e6).contains(&near_hz) => 8_333.333_333,
        Mode::Am => 10_000.0,
        _ => 6_250.0,
    }
}

pub const STEPS_HZ: [f64; 11] = [
    10.0, 100.0, 1_000.0, 2_500.0, 5_000.0, 6_250.0, 8_333.333_333, 12_500.0, 25_000.0, 100_000.0,
    200_000.0,
];

pub fn snap(hz: f64, step: f64) -> f64 {
    if step <= 1.0 {
        return hz.round();
    }
    (hz / step).round() * step
}

/// Where `n` steps from `hz` lands: on the step's raster, as a click does.
///
/// From a frequency on the raster that is `hz + n * step`. From one between
/// two raster points the first step is the short one to the next point that
/// way, so a receiver left off the raster (tuned in another mode, typed, set
/// by a finer step) is back on it at the first press. Adding the step
/// blindly kept it off by the same amount forever: 50 kHz beside every
/// broadcast station, the passband half off the signal and the audio rough.
pub fn step_from(hz: f64, n: f64, step: f64) -> f64 {
    if step <= 1.0 {
        return (hz + n * step.max(1.0)).round();
    }
    let pos = hz / step;
    let near = pos.round();
    if (pos - near).abs() * step < 0.5 {
        (near + n) * step
    } else if n > 0.0 {
        (pos.ceil() + n - 1.0) * step
    } else if n < 0.0 {
        (pos.floor() + n + 1.0) * step
    } else {
        hz
    }
}

/// The step a jog key or the dial takes: the tuning step, or with Shift a
/// tenth of it (never under 100 Hz).
pub fn jog_step(step: f64, fine: bool) -> f64 {
    if fine { (step / 10.0).round().max(100.0) } else { step }
}

/// Whether going from one mode to another changes the raster signals sit
/// on, so a step picked by hand for the first is wrong for the second.
pub fn raster_changes(from: Mode, to: Mode, near_hz: f64) -> bool {
    mode_raster_hz(from, near_hz) != mode_raster_hz(to, near_hz)
}

/// The two requests a retune makes: where the span center goes (if it must
/// move) and the receive frequency.
#[derive(Debug, PartialEq)]
pub struct Retune {
    pub center_hz: Option<f64>,
    pub inspect_hz: f64,
}

/// Plan a retune to `rx`. `lock_center` recenters the span on it (a dial
/// turn that scrolls the display). Otherwise the span moves only when `rx`
/// is outside it or within a channel plus 2% of its edge. With `span_lock`
/// the span stays put for anything inside it — recentering and the edge
/// nudge both off — and moves only to reach a frequency outside it.
pub fn plan(
    rx: f64,
    center_hz: f64,
    span_hz: f64,
    bandwidth_hz: f64,
    lock_center: bool,
    span_lock: bool,
    bounds: (f64, f64),
) -> Retune {
    let rx = rx.clamp(bounds.0, bounds.1);
    let half = span_hz / 2.0;
    if span_lock {
        let center = if (rx - center_hz).abs() > half - bandwidth_hz / 2.0 { rx } else { center_hz };
        return Retune { center_hz: ((center - center_hz).abs() > 1.0).then_some(center), inspect_hz: rx };
    }
    let guard = bandwidth_hz / 2.0 + span_hz * 0.02;
    let center = if lock_center || (rx - center_hz).abs() > half {
        rx
    } else if rx < center_hz - half + guard {
        rx + half - guard
    } else if rx > center_hz + half - guard {
        rx - half + guard
    } else {
        center_hz
    };
    Retune {
        center_hz: ((center - center_hz).abs() > 1.0).then_some(center),
        inspect_hz: rx,
    }
}

/// Parse what an operator types into the frequency field. A bare number is
/// read by size (`146.52` MHz, `146520` kHz, `146520000` Hz); a suffix
/// (`k`, `M`, `G`, `Hz`) says it outright; dotted groups (`146.520.000`)
/// read as Hz.
pub fn parse_freq(text: &str) -> Option<f64> {
    let t = text.trim().to_ascii_lowercase().replace([' ', '_', ','], "");
    if t.is_empty() {
        return None;
    }
    let (num, mult) = if let Some(n) = t.strip_suffix("ghz").or_else(|| t.strip_suffix('g')) {
        (n.to_string(), Some(1e9))
    } else if let Some(n) = t.strip_suffix("mhz").or_else(|| t.strip_suffix('m')) {
        (n.to_string(), Some(1e6))
    } else if let Some(n) = t.strip_suffix("khz").or_else(|| t.strip_suffix('k')) {
        (n.to_string(), Some(1e3))
    } else if let Some(n) = t.strip_suffix("hz") {
        (n.to_string(), Some(1.0))
    } else {
        (t.clone(), None)
    };
    if num.matches('.').count() > 1 {
        // 146.520.000 — digit groups, read as Hz.
        let v: f64 = num.replace('.', "").parse().ok()?;
        return (v > 0.0).then_some(v * mult.unwrap_or(1.0));
    }
    let v: f64 = num.parse().ok()?;
    if !v.is_finite() || v <= 0.0 {
        return None;
    }
    Some(match mult {
        Some(m) => v * m,
        None if v < 10_000.0 => v * 1e6,
        None if v < 10_000_000.0 => v * 1e3,
        None => v,
    })
}

/// `146.520 000` style: MHz with the sub-kHz digits set apart.
pub fn fmt_mhz(hz: f64, decimals: usize) -> String {
    format!("{:.*}", decimals, hz / 1e6)
}

/// A step or rate in short form: `6.25k`, `100`, `2.048M`.
pub fn fmt_short_hz(hz: f64) -> String {
    let trim = |s: String| s.trim_end_matches('0').trim_end_matches('.').to_string();
    if hz >= 1e6 {
        format!("{}M", trim(format!("{:.3}", hz / 1e6)))
    } else if hz >= 1e3 {
        format!("{}k", trim(format!("{:.2}", hz / 1e3)))
    } else {
        format!("{}", hz.round())
    }
}

/// `12.5 kHz`, `2.048 MHz`, `300 Hz`.
pub fn fmt_hz(hz: f64) -> String {
    let trim = |s: String| s.trim_end_matches('0').trim_end_matches('.').to_string();
    let a = hz.abs();
    if a >= 1e9 {
        format!("{} GHz", trim(format!("{:.4}", hz / 1e9)))
    } else if a >= 1e6 {
        format!("{} MHz", trim(format!("{:.4}", hz / 1e6)))
    } else if a >= 1e3 {
        format!("{} kHz", trim(format!("{:.2}", hz / 1e3)))
    } else {
        format!("{} Hz", hz.round())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rasters() {
        assert_eq!(mode_raster_hz(Mode::Am, 121.5e6), 8_333.333_333);
        assert_eq!(mode_raster_hz(Mode::Am, 1.0e6), 10_000.0);
        assert_eq!(snap(146_523_000.0, 6_250.0), 146_525_000.0);
    }

    #[test]
    fn a_step_lands_on_the_raster() {
        // On the raster: one step is one step.
        assert_eq!(step_from(99.9e6, 1.0, 100e3), 100.0e6);
        assert_eq!(step_from(99.9e6, -1.0, 100e3), 99.8e6);
        assert_eq!(step_from(99.9e6, 10.0, 100e3), 100.9e6);
        // Between two points (left there by 80 m's steps, or typed): the
        // first step goes to the next point that way, not 100 kHz on to
        // another frequency just as far off.
        assert_eq!(step_from(99.95e6, 1.0, 100e3), 100.0e6);
        assert_eq!(step_from(99.95e6, -1.0, 100e3), 99.9e6);
        assert_eq!(step_from(99.9123e6, -1.0, 100e3), 99.9e6);
        assert_eq!(step_from(99.9123e6, 10.0, 100e3), 100.9e6);
        assert_eq!(step_from(3.7005e6, -10.0, 1e3), 3.691e6);
        // And however many presses follow, it stays on it.
        let mut hz = 3_750_437.0;
        for _ in 0..7 {
            hz = step_from(hz, 1.0, 100e3);
            assert!((hz / 100e3 - (hz / 100e3).round()).abs() < 1e-9, "{hz}");
        }
        // A hertz of rounding is not "off the raster".
        assert_eq!(step_from(145.5e6 + 0.2, 1.0, 12.5e3), 145.5125e6);
        // No raster at one hertz.
        assert_eq!(step_from(7_074_000.4, 1.0, 1.0), 7_074_001.0);
        assert_eq!(jog_step(100e3, true), 10e3);
        assert_eq!(jog_step(100.0, true), 100.0);
    }

    #[test]
    fn sideband_and_broadcast_do_not_share_a_raster() {
        assert!(raster_changes(Mode::Lsb, Mode::Wfm, 99.9e6));
        assert!(raster_changes(Mode::Wfm, Mode::Nfm, 146e6));
        assert!(!raster_changes(Mode::Lsb, Mode::Usb, 7.2e6));
        assert!(!raster_changes(Mode::Usb, Mode::Ft8, 14.074e6));
    }

    #[test]
    fn retune_moves_span_only_when_needed() {
        let b = (1e3, 2.2e9);
        // Inside the span, clear of the edges: span stays.
        let r = plan(100.5e6, 100e6, 2e6, 12.5e3, false, false, b);
        assert_eq!(r.center_hz, None);
        // Near the top edge: nudged so the channel and a guard fit.
        let r = plan(100.99e6, 100e6, 2e6, 12.5e3, false, false, b);
        let c = r.center_hz.unwrap();
        assert!((c + 1e6 - (100.99e6 + 6_250.0 + 40e3)).abs() < 1.0);
        // Outside: jump.
        assert_eq!(plan(150e6, 100e6, 2e6, 12.5e3, false, false, b).center_hz, Some(150e6));
        // A dial turn: recenter.
        assert_eq!(plan(100.2e6, 100e6, 2e6, 12.5e3, true, false, b).center_hz, Some(100.2e6));
    }

    #[test]
    fn a_locked_span_stays_put_for_anything_inside_it() {
        let b = (1e3, 2.2e9);
        // Dial turns and edge-hugging clicks leave it where it is.
        assert_eq!(plan(100.2e6, 100e6, 2e6, 12.5e3, true, true, b).center_hz, None);
        assert_eq!(plan(100.99e6, 100e6, 2e6, 12.5e3, false, true, b).center_hz, None);
        // A frequency the span cannot hear still moves it.
        assert_eq!(plan(150e6, 100e6, 2e6, 12.5e3, false, true, b).center_hz, Some(150e6));
        assert_eq!(plan(100.998e6, 100e6, 2e6, 12.5e3, false, true, b).center_hz, Some(100.998e6));
    }

    #[test]
    fn typed_frequencies() {
        assert_eq!(parse_freq("146.52"), Some(146.52e6));
        assert_eq!(parse_freq("146520"), Some(146.52e6));
        assert_eq!(parse_freq("146520000"), Some(146.52e6));
        assert_eq!(parse_freq("146.520.000"), Some(146.52e6));
        assert_eq!(parse_freq("7.2m"), Some(7.2e6));
        assert_eq!(parse_freq("1090 MHz"), Some(1.09e9));
        assert_eq!(parse_freq("540k"), Some(540e3));
        assert_eq!(parse_freq("1.2g"), Some(1.2e9));
        assert_eq!(parse_freq("-3"), None);
        assert_eq!(parse_freq("abc"), None);
    }

    #[test]
    fn short_forms() {
        assert_eq!(fmt_short_hz(6_250.0), "6.25k");
        assert_eq!(fmt_short_hz(2_048_000.0), "2.048M");
        assert_eq!(fmt_hz(12_500.0), "12.5 kHz");
    }
}
