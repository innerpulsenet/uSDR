//! Demodulation, squelch, and call segmentation for scannerd.

pub mod adsb;
pub mod afc;
pub mod afsk;
pub mod aprs;
pub mod biquad;
pub mod channel;
pub mod classifier;
pub mod cqpsk;
pub mod crypto;
pub mod ctcss;
pub mod dcs;
pub mod detect;
pub mod dmr;
pub mod flex;
pub mod legacy_digital;
pub mod leveler;
pub mod ltr;
pub mod mdc;
pub mod nbfm;
pub mod noisegate;
pub mod nxdn;
pub mod p25;
pub mod pager_parser;
pub mod passport;
pub mod pocsag;
pub mod rs;
pub mod same;
pub mod smartnet;
pub mod squelch;
pub mod timing;
pub mod uat;

pub use afc::Afc;
pub use channel::{
    AUDIO_RATE, CallEvent, CallSummary, ChannelReceiver, ChannelSpec, DigitalCallTelemetry,
    SquelchCode, ToneCode,
};
pub use classifier::{ClassificationResult, DecodeEvent, SignalClassifier};
pub use cqpsk::CqpskDemodulator;
pub use ctcss::{CtcssDetector, Detection as CtcssDetection};
pub use dcs::{DcsDetector, Detection as DcsDetection};
pub use detect::SpanDetector;
pub use flex::{FlexDecoder, FlexDiagnostics, FlexFormat, FlexFragment, FlexMessage};
pub use leveler::Leveler;
pub use mdc::MdcPacket;
pub use nbfm::{Demodulated, NbfmDemod};
pub use noisegate::NoiseGate;
pub use pager_parser::parse_pager_text;
pub use pocsag::{PocsagFormat, PocsagMessage};
pub use squelch::Squelch;
