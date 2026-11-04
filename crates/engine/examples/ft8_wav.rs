//! Decode FT8 or FT4 from a WAV file through the same slot decoder the
//! server runs: `ft8_wav <file.wav> <end UNIX ms> [ft4] [agc]`, or a slot the
//! server dumped, `ft8_wav <slot>.f32 [ft4]`. The end time
//! is when the file's last sample was heard (a capture's download time), so
//! the audio lands in the UTC slots it was sent in. A two-channel file is
//! the channel's I/Q, demodulated first as the server does it: upper
//! sideband over 100–3100 Hz, the decoder taking the audio before the AGC
//! (or after it, with `agc`).

use scannerd_engine::ft8::{Protocol, SlotDecoder};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // A slot the server dumped (USDR_FT8_DUMP): 12 kHz floats, decoded whole.
    if let Some(path) = args.first().filter(|p| p.ends_with(".f32")) {
        let audio: Vec<f32> = std::fs::read(path)?.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        let protocol = if args.iter().any(|a| a == "ft4") { Protocol::Ft4 } else { Protocol::Ft8 };
        let decodes = scannerd_engine::ft8::decode_slot(protocol, &audio, &mut Default::default());
        println!("{} decodes", decodes.len());
        for d in decodes {
            println!("{:>4.0} {:>5.1} {:>5.0}  {}", d.snr_db, d.dt_s, d.freq_hz, d.text);
        }
        return Ok(());
    }
    let (Some(path), Some(end)) = (args.first(), args.get(1)) else {
        anyhow::bail!("usage: ft8_wav <file.wav> <end UNIX ms> [ft4]");
    };
    let end_ms: u64 = end.parse()?;
    let protocol = if args.iter().any(|a| a == "ft4") { Protocol::Ft4 } else { Protocol::Ft8 };
    let after_agc = args.iter().any(|a| a == "agc");
    let bytes = std::fs::read(path)?;
    // A plain 16-bit mono PCM WAV: the rate at 24, the samples after "data".
    let channels = u16::from_le_bytes(bytes[22..24].try_into()?);
    let rate = u32::from_le_bytes(bytes[24..28].try_into()?) as f64;
    let data = bytes.windows(4).position(|w| w == b"data").ok_or_else(|| anyhow::anyhow!("no data chunk"))? + 8;
    let pcm: Vec<f32> = bytes[data..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect();
    let audio = if channels == 2 {
        let iq: Vec<num_complex::Complex32> = pcm.chunks_exact(2).map(|c| num_complex::Complex32::new(c[0], c[1])).collect();
        let mut ssb = scannerd_engine::ssb::SsbDemod::with_passband(rate, scannerd_engine::ssb::Sideband::Upper, 100.0, 3_100.0);
        let (mut out, mut linear) = (Vec::new(), Vec::new());
        ssb.process_with_linear(&iq, &mut out, &mut linear);
        if after_agc { out } else { linear }
    } else {
        pcm
    };
    let mut dec = SlotDecoder::new(protocol, rate);
    let block = (rate / 20.0) as usize;
    let n = audio.len();
    for (k, chunk) in audio.chunks(block).enumerate() {
        let last = (k * block + chunk.len()) as f64;
        dec.push(chunk, end_ms - ((n as f64 - last) / rate * 1000.0) as u64);
    }
    // Silence to the end of the slot the file stops in, so that slot is cut
    // too (the decoder needs most of a slot, not all of it).
    let slot_ms = (protocol.slot_secs() * 1000.0) as u64;
    let mut t = end_ms;
    while t < (end_ms / slot_ms + 1) * slot_ms + 500 {
        t += 50;
        dec.push(&vec![0.0; (rate / 20.0) as usize], t);
    }
    // Let the worker finish.
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        for (slot, decodes) in dec.take() {
            println!("slot {slot}: {} decodes", decodes.len());
            for d in decodes {
                println!("{:>4.0} {:>5.1} {:>5.0}  {}", d.snr_db, d.dt_s, d.freq_hz, d.text);
            }
        }
        if !dec.busy() {
            break;
        }
    }
    Ok(())
}
