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
        Mode::Usb | Mode::Lsb => 100.0,
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

/// The two requests a retune makes: where the span center goes (if it must
/// move) and the receive frequency.
#[derive(Debug, PartialEq)]
pub struct Retune {
    pub center_hz: Option<f64>,
    pub inspect_hz: f64,
}

/// Plan a retune to `rx`. `lock_center` recenters the span on it (a dial
/// turn that scrolls the display). Otherwise the span moves only when `rx`
/// is outside it or within a channel plus 2% of its edge.
pub fn plan(
    rx: f64,
    center_hz: f64,
    span_hz: f64,
    bandwidth_hz: f64,
    lock_center: bool,
    bounds: (f64, f64),
) -> Retune {
    let rx = rx.clamp(bounds.0, bounds.1);
    let half = span_hz / 2.0;
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
    fn retune_moves_span_only_when_needed() {
        let b = (1e3, 2.2e9);
        // Inside the span, clear of the edges: span stays.
        let r = plan(100.5e6, 100e6, 2e6, 12.5e3, false, b);
        assert_eq!(r.center_hz, None);
        // Near the top edge: nudged so the channel and a guard fit.
        let r = plan(100.99e6, 100e6, 2e6, 12.5e3, false, b);
        let c = r.center_hz.unwrap();
        assert!((c + 1e6 - (100.99e6 + 6_250.0 + 40e3)).abs() < 1.0);
        // Outside: jump.
        assert_eq!(plan(150e6, 100e6, 2e6, 12.5e3, false, b).center_hz, Some(150e6));
        // Locked: recenter.
        assert_eq!(plan(100.2e6, 100e6, 2e6, 12.5e3, true, b).center_hz, Some(100.2e6));
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
