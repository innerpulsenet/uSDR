//! Parametric stereo (HE-AAC v2): not implemented. The hook sits where the
//! SBR decoder hands over the QMF-domain mono signal, and for now the second
//! channel is a copy of the first, so a PS stream plays as mono on both
//! sides.

use super::bits::BitReader;
use num_complex::Complex32;

pub(super) struct Ps {}

impl Ps {
    pub fn new(_slots: usize) -> Self {
        Self {}
    }
    pub fn parse(&mut self, _br: &mut BitReader) {}
    pub fn conceal(&mut self) {}
    pub fn process(&mut self, x: &mut [[Complex32; 64]], right: &mut [[Complex32; 64]], _bands: usize) {
        right.copy_from_slice(x);
    }
}
