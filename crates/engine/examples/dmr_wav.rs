//! Run a DMR-mode channel capture through the classifier and the DMR
//! receiver offline: the server's `GET /api/sdr/capture.wav?kind=iq`
//! (16-bit stereo I/Q, the receiver at 0 Hz), fed in blocks the way DMR mode
//! drives them. Prints what the classifier called each active block, the
//! receiver's call events and how much audio it made.
//!
//!     cargo run --release -p scannerd-engine --example dmr_wav <capture.wav>

use num_complex::Complex32;
use scannerd_engine::classifier::SignalClassifier;
use scannerd_engine::dmr::{DmrChannelReceiver, DmrSpec};

fn main() {
    let path = std::env::args().nth(1).expect("capture.wav");
    let bytes = std::fs::read(&path).expect("read");
    let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap()) as f64;
    let iq: Vec<Complex32> = bytes[44..]
        .chunks_exact(4)
        .map(|c| {
            Complex32::new(
                i16::from_le_bytes([c[0], c[1]]) as f32,
                i16::from_le_bytes([c[2], c[3]]) as f32,
            ) / 32768.0
        })
        .collect();
    eprintln!("{:.1} s at {rate} Hz", iq.len() as f64 / rate);
    let block = (rate / 50.0) as usize;

    let mut cls = SignalClassifier::new(rate);
    let mut rx = DmrChannelReceiver::new_on_channel(
        DmrSpec { name: "wav".into(), freq_hz: 0.0, color_code: None, slot: None },
        rate,
    );
    let mut seen = std::collections::BTreeMap::<String, usize>::new();
    let (mut locked, mut in_call, mut audio) = (0usize, 0usize, 0usize);
    for (i, c) in iq.chunks(block).enumerate() {
        let r = cls.process(c, 154.0e6);
        if r.active {
            *seen.entry(r.protocol.clone()).or_default() += 1;
        }
        if let Some(ev) = rx.process(c, 20.0) {
            println!("{:5.2} s  {ev:?}", i as f64 * block as f64 / rate);
        }
        locked += usize::from(rx.locked());
        in_call += usize::from(rx.in_call());
        audio += rx.audio().len();
    }
    println!("classifier said: {seen:?}");
    println!("receiver: locked {locked} blocks, in call {in_call}, audio {audio} samples");
}
