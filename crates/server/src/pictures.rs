//! Finished pictures, kept by the daemon so that nothing depends on a desktop
//! being open. The decoder hands over each picture as it ends. A thread of its
//! own judges it, writes the PNG and files a row in the station log, so neither
//! the encoding nor the disk can hold up the radio.
//!
//! A station sends each chart on several frequencies at once, and wideband
//! channels can be on more than one of them. A radiofax picture waits a little
//! for its copies, and only the best of them is kept.

use std::fs;
use std::io::BufWriter;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::schedule::Catalog;
use crate::stationlog::StationLog;

/// Pictures waiting for the writer. They arrive minutes apart.
const QUEUE: usize = 8;
/// Below this a radiofax "picture" is a false start or a fragment. A chart at
/// 120 lines a minute runs to over a thousand.
const WEFAX_MIN_LINES: u32 = 500;
/// How long a radiofax picture waits for copies of itself from the station's
/// other frequencies. Copies end within seconds of each other.
const COPY_WAIT: Duration = Duration::from_secs(90);
/// Copies of one broadcast begin no further apart than this.
const COPY_START_S: u64 = 120;
/// How much of an SSTV frame has to have arrived.
const SSTV_MIN_SHARE: f32 = 0.9;
/// Below this the picture has no contrast at all: a carrier or dead air.
const MIN_SPREAD: f32 = 0.04;
/// Adjacent rows of a real picture resemble each other and rows of static do
/// not. Static measures about 0 and the charts measured so far well above this.
const MIN_ROW_CORR: f32 = 0.25;
/// A row varying less than this is margin, which says nothing either way.
const FLAT_ROW_VARIANCE: f32 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// One byte a pixel.
    Wefax,
    /// Red, green and blue bytes a pixel.
    Sstv,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Wefax => "WEFAX",
            Kind::Sstv => "SSTV",
        }
    }

    fn channels(self) -> usize {
        match self {
            Kind::Wefax => 1,
            Kind::Sstv => 3,
        }
    }
}

/// A picture as it ended.
#[derive(Clone, Debug)]
pub struct Finished {
    pub kind: Kind,
    pub started_utc: u64,
    pub ended_utc: u64,
    pub dial_hz: u64,
    pub width: u32,
    pub rows: u32,
    /// What the mode says a full frame is, where it says.
    pub full_rows: Option<u32>,
    pub pixels: Vec<u8>,
}

/// What became of a picture, as the log files it.
#[derive(Clone, Debug, PartialEq)]
pub struct PictureRow {
    pub start_utc: u64,
    pub end_utc: u64,
    pub mode: &'static str,
    pub dial_hz: u64,
    pub width: u32,
    pub lines: u32,
    pub station: Option<String>,
    /// From the timetable, since a chart does not state its own title.
    pub scheduled_title: Option<String>,
    pub file: Option<String>,
    pub bytes: u64,
    pub row_corr: f32,
    pub spread: f32,
    /// `publish`, `hold` or `reject`.
    pub decision: &'static str,
    pub why: &'static str,
}

/// How a picture measures.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quality {
    /// The mean correlation between adjacent rows that have any content.
    pub row_corr: f32,
    /// The standard deviation of brightness, 0 to 1.
    pub spread: f32,
}

/// Brightness of one pixel.
fn luma(p: &Finished, x: usize, y: usize) -> f32 {
    let n = p.kind.channels();
    let at = (y * p.width as usize + x) * n;
    let sum: u32 = p.pixels[at..at + n].iter().map(|&v| u32::from(v)).sum();
    sum as f32 / n as f32
}

fn correlation(a: &[f32], b: &[f32]) -> Option<f32> {
    let n = a.len() as f32;
    let (ma, mb) = (a.iter().sum::<f32>() / n, b.iter().sum::<f32>() / n);
    let (mut cov, mut va, mut vb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        cov += (x - ma) * (y - mb);
        va += (x - ma) * (x - ma);
        vb += (y - mb) * (y - mb);
    }
    if va / n < FLAT_ROW_VARIANCE || vb / n < FLAT_ROW_VARIANCE {
        return None;
    }
    Some(cov / (va * vb).sqrt())
}

pub fn measure(p: &Finished) -> Quality {
    let (w, rows) = (p.width as usize, p.rows as usize);
    if w == 0 || rows < 2 || p.pixels.len() < w * rows * p.kind.channels() {
        return Quality { row_corr: 0.0, spread: 0.0 };
    }
    let (step_x, step_y) = ((w / 320).max(1), (rows / 240).max(1));
    let row = |y: usize| (0..w).step_by(step_x).map(|x| luma(p, x, y)).collect::<Vec<f32>>();
    let (mut total, mut counted) = (0.0, 0_u32);
    let (mut sum, mut square, mut n) = (0.0_f32, 0.0_f32, 0.0_f32);
    for y in (0..rows - 1).step_by(step_y) {
        let (a, b) = (row(y), row(y + 1));
        for v in &a {
            sum += v;
            square += v * v;
            n += 1.0;
        }
        if let Some(c) = correlation(&a, &b) {
            total += c;
            counted += 1;
        }
    }
    let mean = sum / n;
    Quality {
        row_corr: if counted == 0 { 0.0 } else { total / counted as f32 },
        spread: (square / n - mean * mean).max(0.0).sqrt() / 255.0,
    }
}

/// Why a picture is not worth keeping, if it is not.
fn rejected(p: &Finished, q: Quality) -> Option<&'static str> {
    let short = match (p.kind, p.full_rows) {
        (Kind::Wefax, _) => p.rows < WEFAX_MIN_LINES,
        (Kind::Sstv, Some(full)) => (p.rows as f32) < full as f32 * SSTV_MIN_SHARE,
        (Kind::Sstv, None) => true,
    };
    if short {
        Some("short")
    } else if q.spread < MIN_SPREAD {
        Some("blank")
    } else if q.row_corr < MIN_ROW_CORR {
        Some("static")
    } else {
        None
    }
}

/// Whether a kept picture may go out unseen.
fn decide(kind: Kind) -> (&'static str, &'static str) {
    match kind {
        Kind::Sstv => ("hold", "anyone can send any image"),
        Kind::Wefax => ("publish", "weather chart"),
    }
}

fn write_png(p: &Finished, path: &Path) -> Result<u64> {
    let temporary = path.with_extension("tmp");
    let file = fs::File::create(&temporary).with_context(|| format!("creating {}", temporary.display()))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), p.width, p.rows);
    encoder.set_color(match p.kind {
        Kind::Wefax => png::ColorType::Grayscale,
        Kind::Sstv => png::ColorType::Rgb,
    });
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().context("writing the PNG header")?;
    let len = p.width as usize * p.rows as usize * p.kind.channels();
    writer.write_image_data(&p.pixels[..len]).context("writing the PNG rows")?;
    writer.finish().context("finishing the PNG")?;
    fs::rename(&temporary, path).with_context(|| format!("moving {}", path.display()))?;
    Ok(fs::metadata(path)?.len())
}

/// Judge a picture, write it if it is kept, and say what happened.
pub fn file(p: &Finished, catalog: &Catalog, dir: &Path) -> Result<PictureRow> {
    let quality = measure(p);
    let mut row = row_for(p, catalog, quality);
    if let Some(why) = rejected(p, quality) {
        row.why = why;
        return Ok(row);
    }
    (row.decision, row.why) = decide(p.kind);
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = format!("{}-{}-{}.png", p.kind.name().to_ascii_lowercase(), p.started_utc, p.dial_hz);
    row.bytes = write_png(p, &dir.join(&name))?;
    row.file = Some(name);
    Ok(row)
}

/// The log's row for a picture, rejected until something says otherwise.
fn row_for(p: &Finished, catalog: &Catalog, quality: Quality) -> PictureRow {
    let chart = (p.kind == Kind::Wefax).then(|| catalog.chart_at(p.dial_hz, p.started_utc)).flatten();
    let station = chart.map(|(s, _)| s.to_string());
    PictureRow {
        start_utc: p.started_utc,
        end_utc: p.ended_utc,
        mode: p.kind.name(),
        dial_hz: p.dial_hz,
        width: p.width,
        lines: p.rows,
        scheduled_title: chart.map(|(_, t)| t.to_string()),
        station,
        file: None,
        bytes: 0,
        row_corr: quality.row_corr,
        spread: quality.spread,
        decision: "reject",
        why: "",
    }
}

/// Whether two radiofax pictures are one broadcast heard on two of the
/// station's frequencies.
fn same_broadcast(a: &Finished, b: &Finished, catalog: &Catalog) -> bool {
    a.kind == Kind::Wefax
        && b.kind == Kind::Wefax
        && a.dial_hz != b.dial_hz
        && a.started_utc.abs_diff(b.started_utc) <= COPY_START_S
        && catalog.station_of(a.dial_hz).is_some_and(|s| catalog.station_of(b.dial_hz) == Some(s))
}

/// The copy to keep: one that passes before one that does not, then the
/// cleanest rows, then the most of them.
fn best_copy(copies: &[Finished]) -> usize {
    let score = |p: &Finished| {
        let q = measure(p);
        (rejected(p, q).is_none(), q.row_corr, p.rows)
    };
    let scores: Vec<_> = copies.iter().map(score).collect();
    (0..copies.len())
        .max_by(|&a, &b| {
            let (x, y) = (scores[a], scores[b]);
            x.0.cmp(&y.0).then(x.1.total_cmp(&y.1)).then(x.2.cmp(&y.2))
        })
        .unwrap_or(0)
}

/// File the best of a broadcast's copies, and the rest as copies.
fn settle(copies: Vec<Finished>, catalog: &Catalog, dir: &Path, log: &StationLog) {
    let best = best_copy(&copies);
    for (i, p) in copies.iter().enumerate() {
        let row = if i == best {
            match file(p, catalog, dir) {
                Ok(row) => row,
                Err(e) => {
                    eprintln!("pictures: filing a {} picture: {e:#}", p.kind.name());
                    continue;
                }
            }
        } else {
            PictureRow { why: "copy", ..row_for(p, catalog, measure(p)) }
        };
        log.add_picture(row);
    }
}

static SINK: OnceLock<SyncSender<Finished>> = OnceLock::new();
static LOST: AtomicU64 = AtomicU64::new(0);

/// Hand a finished picture to the writer. Never blocks; a full queue loses the
/// picture and counts it.
pub fn submit(picture: Finished) {
    let Some(sink) = SINK.get() else { return };
    if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) = sink.try_send(picture) {
        LOST.fetch_add(1, Ordering::Relaxed);
        eprintln!("pictures: lost one, {} so far", LOST.load(Ordering::Relaxed));
    }
}

fn write_loop(rx: Receiver<Finished>, log: Arc<StationLog>, catalog: Catalog) {
    let dir = log.pictures_dir();
    let mut waiting: Vec<(Instant, Finished)> = Vec::new();
    loop {
        let closed = match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(p) if p.kind == Kind::Wefax && catalog.station_of(p.dial_hz).is_some() => {
                waiting.push((Instant::now(), p));
                false
            }
            Ok(p) => {
                settle(vec![p], &catalog, &dir, &log);
                false
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => true,
        };
        // The longest waiting settles with whatever copies of it have come.
        while let Some(i) = waiting.iter().position(|(at, _)| closed || at.elapsed() >= COPY_WAIT) {
            let first = waiting.remove(i).1;
            let (copies, rest): (Vec<_>, Vec<_>) = waiting.drain(..).partition(|(_, p)| same_broadcast(&first, p, &catalog));
            waiting = rest;
            settle(std::iter::once(first).chain(copies.into_iter().map(|(_, p)| p)).collect(), &catalog, &dir, &log);
        }
        if closed {
            return;
        }
    }
}

/// Start keeping pictures in the station log's folder.
pub fn start(log: Arc<StationLog>, catalog: Catalog) -> Result<()> {
    let (tx, rx) = sync_channel(QUEUE);
    SINK.set(tx).map_err(|_| anyhow::anyhow!("pictures already started"))?;
    std::thread::Builder::new().name("station-pictures".into()).spawn(move || write_loop(rx, log, catalog)).context("starting the picture writer")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: u64 = 1_800_000_000;

    fn gray(rows: u32, f: impl Fn(u32, u32) -> u8) -> Finished {
        let width = 640;
        let pixels = (0..rows).flat_map(|y| (0..width).map(move |x| (x, y))).map(|(x, y)| f(x, y)).collect();
        Finished { kind: Kind::Wefax, started_utc: AT, ended_utc: AT + 600, dial_hz: 8_501_100, width, rows, full_rows: None, pixels }
    }

    /// Thick lines and margins, like a chart.
    fn chart_like(x: u32, y: u32) -> u8 {
        let curve = (120.0 * ((f64::from(y) / 90.0).sin() + 1.0)) as u32;
        if x % 80 < 3 || x.abs_diff(curve) < 4 || y % 150 < 2 { 0 } else { 255 }
    }

    /// Repeatable noise with nothing shared between rows.
    fn noise(x: u32, y: u32) -> u8 {
        let mut v = x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503).rotate_left(13);
        v ^= v >> 15;
        v = v.wrapping_mul(2_246_822_519);
        v ^= v >> 13;
        (v & 0xff) as u8
    }

    fn temporary(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("usdr-pictures-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn of_one_broadcast_on_several_frequencies_the_cleanest_copy_is_kept() {
        let catalog = Catalog::builtin();
        let at = |dial_hz: u64, f: fn(u32, u32) -> u8, rows: u32| Finished { dial_hz, ..gray(rows, f) };
        // NMF on 6340.5 and 9110 kHz, NMG on 8503.9.
        let (clean, noisy, other) = (at(9_108_100, chart_like, 900), at(6_338_600, noise, 900), at(8_502_000, chart_like, 900));
        assert!(same_broadcast(&clean, &noisy, &catalog));
        assert!(!same_broadcast(&clean, &other, &catalog), "another station");
        assert!(!same_broadcast(&clean, &Finished { started_utc: AT + 600, ..at(6_338_600, chart_like, 900) }, &catalog), "a later chart");
        assert!(!same_broadcast(&clean, &at(9_108_100, chart_like, 900), &catalog), "the same dial is not a copy");
        assert_eq!(best_copy(&[noisy.clone(), clean.clone()]), 1);
        assert_eq!(best_copy(&[at(9_108_100, chart_like, 600), at(6_338_600, chart_like, 1_100)]), 1, "equal rows: the longer");
    }

    #[test]
    fn a_chart_measures_as_a_picture_and_static_does_not() {
        let chart = gray(900, chart_like);
        let q = measure(&chart);
        assert!(q.row_corr > 0.7, "{q:?}");
        assert_eq!(rejected(&chart, q), None);
        let hiss = gray(900, noise);
        let q = measure(&hiss);
        assert!(q.row_corr.abs() < 0.1, "{q:?}");
        assert_eq!(rejected(&hiss, q), Some("static"));
    }

    #[test]
    fn a_blank_or_short_picture_is_not_kept() {
        let blank = gray(900, |_, _| 200);
        assert_eq!(rejected(&blank, measure(&blank)), Some("blank"));
        let short = gray(WEFAX_MIN_LINES - 1, chart_like);
        assert_eq!(rejected(&short, measure(&short)), Some("short"));
    }

    #[test]
    fn a_partial_sstv_frame_is_not_kept() {
        let frame = |rows: u32| {
            let width = 320_u32;
            let pixels = (0..rows).flat_map(|y| (0..width).flat_map(move |x| [(x / 2) as u8, (y / 2) as u8, ((x + y) / 4) as u8])).collect();
            Finished { kind: Kind::Sstv, started_utc: AT, ended_utc: AT + 110, dial_hz: 14_230_000, width, rows, full_rows: Some(256), pixels }
        };
        assert_eq!(rejected(&frame(200), measure(&frame(200))), Some("short"));
        assert_eq!(rejected(&frame(256), measure(&frame(256))), None);
    }

    #[test]
    fn charts_publish_unseen_and_only_sstv_waits_for_review() {
        assert_eq!(decide(Kind::Wefax).0, "publish");
        assert_eq!(decide(Kind::Sstv).0, "hold");
    }

    #[test]
    fn a_kept_picture_is_a_readable_png_and_a_rejected_one_leaves_no_file() {
        let dir = temporary("file");
        let row = file(&gray(900, chart_like), &Catalog::default(), &dir).unwrap();
        assert_eq!(row.decision, "publish", "a chart needs no timetable to go out");
        let name = row.file.expect("a file");
        assert_eq!(name, format!("wefax-{AT}-8501100.png"));
        let mut reader = png::Decoder::new(std::io::BufReader::new(fs::File::open(dir.join(&name)).unwrap())).read_info().unwrap();
        let mut out = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut out).unwrap();
        assert_eq!((info.width, info.height, info.color_type), (640, 900, png::ColorType::Grayscale));
        assert_eq!(out[..640], gray(1, chart_like).pixels[..640]);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "no temporary left behind");

        let row = file(&gray(900, noise), &Catalog::default(), &dir).unwrap();
        assert_eq!((row.decision, row.why, row.file), ("reject", "static", None));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
