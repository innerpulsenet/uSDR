//! Decode a directory tree of dumped slots (`USDR_FT8_DUMP`, 12 kHz f32) and
//! total what came out, for comparing decoder settings on the same audio:
//!
//!   ft8_corpus <dir> [ft4] [plain] [budget=MS] [searches=N] [distinct=0|1]
//!              [ap=0|1] [osd=0|1] [checks=N] [hard=N] [spread=N] [reverse] [list]
//!
//! `plain` is the decoder before the aided passes. `reverse` plays every
//! slot backwards: the band's noise and crowding with no valid transmission
//! in it, so whatever decodes is false. `list` prints every decode.
//!
//! A decode made with help ("ap", "osd") counts as corroborated when its
//! sender is also heard, anywhere in the corpus, by propagation alone.

use scannerd_engine::ft8::{CallHashes, Options, Protocol, decode_slot_with};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn slots_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(read) = std::fs::read_dir(dir) else { return };
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            slots_under(&path, out);
        } else if path.extension().is_some_and(|e| e == "f32") {
            out.push(path);
        }
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(dir) = args.first() else { anyhow::bail!("usage: ft8_corpus <dir> [options]") };
    let has = |name: &str| args.iter().any(|a| a == name);
    let value = |name: &str| args.iter().find_map(|a| a.strip_prefix(name)?.strip_prefix('=')?.parse::<u64>().ok());
    let protocol = if has("ft4") { Protocol::Ft4 } else { Protocol::Ft8 };
    let mut options = if has("plain") { Options::plain() } else { Options::default() };
    if let Some(ms) = value("budget") {
        options.budget = std::time::Duration::from_millis(ms);
    }
    if let Some(n) = value("searches") {
        options.searches = n as usize;
    }
    if let Some(v) = value("distinct") {
        options.distinct = v != 0;
    }
    if let Some(v) = value("ap") {
        options.ap_cq = v != 0;
    }
    if let Some(v) = value("osd") {
        options.osd = v != 0;
    }
    if let Some(n) = value("checks") {
        options.osd_max_checks = n as usize;
    }
    if let Some(n) = value("spread") {
        options.llr_spread = n as f32;
    }
    if let Some(n) = value("hard") {
        options.osd_max_hard = n as usize;
    }
    let mut slots = Vec::new();
    slots_under(Path::new(dir), &mut slots);
    slots.sort();
    // One table of heard calls per channel, as the server keeps them.
    let mut hashes: BTreeMap<PathBuf, CallHashes> = BTreeMap::new();
    let mut by_method: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut unaided_calls: BTreeSet<String> = BTreeSet::new();
    let mut aided: Vec<(String, &'static str, String, f32)> = Vec::new();
    let (mut total, mut worst_ms, mut sum_ms) = (0usize, 0u128, 0u128);
    for path in &slots {
        let mut audio: Vec<f32> = std::fs::read(path)?.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        if has("reverse") {
            audio.reverse();
        }
        let table = hashes.entry(path.parent().unwrap_or(Path::new("")).to_path_buf()).or_default();
        let t = std::time::Instant::now();
        let decodes = decode_slot_with(protocol, &audio, table, 100.0, 3_100.0, &options);
        let ms = t.elapsed().as_millis();
        worst_ms = worst_ms.max(ms);
        sum_ms += ms;
        total += decodes.len();
        let name = path.strip_prefix(dir).unwrap_or(path).display().to_string();
        for d in decodes {
            *by_method.entry(d.method).or_default() += 1;
            if has("list") {
                println!("{name} {:>5} {:>4.0} {:>5.1} {:>5.0}  {}", d.method, d.snr_db, d.dt_s, d.freq_hz, d.text);
            }
            match d.method {
                "ap" | "osd" => aided.push((name.clone(), d.method, d.text.clone(), d.snr_db)),
                _ => {
                    unaided_calls.extend(d.message.de.clone());
                }
            }
        }
    }
    println!("{} slots, {total} decodes, {:.1} a slot; {} ms a slot, worst {worst_ms} ms", slots.len(), total as f64 / slots.len().max(1) as f64, sum_ms / slots.len().max(1) as u128);
    println!("by method: {by_method:?}");
    let sender = |text: &str| text.split_whitespace().rev().find(|w| w.len() >= 3 && w.chars().any(|c| c.is_ascii_digit()) && !w.starts_with(['+', '-', 'R'])).map(str::to_string);
    let (mut backed, mut alone) = (0, Vec::new());
    for (name, method, text, snr) in &aided {
        // The sender is the second call of a standard message.
        let de = text.split_whitespace().nth(if text.starts_with("CQ ") && text.split_whitespace().count() > 3 { 2 } else { 1 }).map(str::to_string).or_else(|| sender(text));
        if de.is_some_and(|c| unaided_calls.contains(&c)) {
            backed += 1;
        } else {
            alone.push(format!("{name} {method} {snr:.0} {text}"));
        }
    }
    println!("aided: {} decodes, {backed} from senders also heard unaided, {} not", aided.len(), alone.len());
    for line in alone {
        println!("  uncorroborated: {line}");
    }
    Ok(())
}
