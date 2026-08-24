//! Reed-Solomon protection used by P25 voice signalling.
//!
//! LDU1 Link Control uses RS(24,12,13), while LDU2 Encryption Sync uses
//! RS(24,16,9). Both are shortened systematic codes over GF(2^6), with the
//! primitive polynomial x^6+x+1 and generator roots alpha^1 onward.

use super::{gf_inv, gf_mul, gf_tables};

/// Correct a shortened 24-symbol P25 RS word and return the number of symbol
/// errors repaired. `information_symbols` is 12 for LC or 16 for ESS.
pub(super) fn correct_24(word: &mut [u8], information_symbols: usize) -> Option<u8> {
    if word.len() != 24 || !matches!(information_symbols, 12 | 16) {
        return None;
    }
    correct(word, information_symbols)
}

/// Correct any shortened systematic RS word over P25's GF(64). Phase 2 ESS
/// uses RS(44,16,29), in addition to the 24-symbol Phase 1 codewords above.
pub(super) fn correct(word: &mut [u8], information_symbols: usize) -> Option<u8> {
    if word.is_empty() || word.len() > 63 || information_symbols >= word.len() {
        return None;
    }
    if word.iter().any(|symbol| symbol & !0x3f != 0) {
        return None;
    }

    let parity_symbols = word.len() - information_symbols;
    let received_syndromes = syndromes(word, parity_symbols);
    if received_syndromes.iter().all(|&syndrome| syndrome == 0) {
        return Some(0);
    }

    let locator = locator_from_syndromes(&received_syndromes)?;
    let degree = locator.len() - 1;
    if degree == 0 || degree > parity_symbols / 2 {
        return None;
    }

    correct_with_locator(word, parity_symbols, &received_syndromes, &locator)
}

/// Correct a shortened RS word with known erased symbol positions and any
/// additional unknown errors within `2*errors + erasures <= parity`.
pub(super) fn correct_with_erasures(
    word: &mut [u8],
    information_symbols: usize,
    erasures: &[usize],
) -> Option<u8> {
    if word.is_empty()
        || word.len() > 63
        || information_symbols >= word.len()
        || erasures.len() > word.len() - information_symbols
        || word.iter().any(|symbol| symbol & !0x3f != 0)
    {
        return None;
    }
    let mut seen = vec![false; word.len()];
    for &position in erasures {
        if position >= word.len() || seen[position] {
            return None;
        }
        seen[position] = true;
        word[position] = 0;
    }

    let parity_symbols = word.len() - information_symbols;
    let received_syndromes = syndromes(word, parity_symbols);
    if received_syndromes.iter().all(|&syndrome| syndrome == 0) {
        return Some(0);
    }

    // Remove known erasure contributions before Berlekamp-Massey so its
    // degree counts only unknown errors (Forney syndromes).
    let mut forney = received_syndromes.clone();
    for &position in erasures {
        let location = gf_pow((word.len() - 1 - position) as isize);
        for index in 0..forney.len().saturating_sub(1) {
            forney[index] = gf_mul(forney[index], location) ^ forney[index + 1];
        }
        forney.pop();
    }
    let unknown = locator_from_syndromes(&forney)?;
    let unknown_degree = unknown.len() - 1;
    if 2 * unknown_degree + erasures.len() > parity_symbols {
        return None;
    }

    let mut erasure_locator = vec![1u8];
    for &position in erasures {
        let location = gf_pow((word.len() - 1 - position) as isize);
        erasure_locator = multiply(&erasure_locator, &[1, location]);
    }
    let locator = multiply(&unknown, &erasure_locator);
    correct_with_locator(word, parity_symbols, &received_syndromes, &locator)
}

/// Berlekamp-Massey produces the shortest error-locator polynomial for a
/// syndrome sequence. Coefficients are stored low degree first.
fn locator_from_syndromes(syndromes: &[u8]) -> Option<Vec<u8>> {
    if syndromes.iter().all(|&syndrome| syndrome == 0) {
        return Some(vec![1]);
    }
    let mut locator = vec![1u8];
    let mut previous = vec![1u8];
    let mut degree = 0usize;
    let mut shift = 1usize;
    let mut last_discrepancy = 1u8;
    for i in 0..syndromes.len() {
        let mut discrepancy = syndromes[i];
        for j in 1..=degree {
            if j < locator.len() {
                discrepancy ^= gf_mul(locator[j], syndromes[i - j]);
            }
        }
        if discrepancy == 0 {
            shift += 1;
            continue;
        }

        let old_locator = locator.clone();
        let scale = gf_mul(discrepancy, gf_inv(last_discrepancy));
        add_shifted_scaled(&mut locator, &previous, shift, scale);
        if 2 * degree <= i {
            degree = i + 1 - degree;
            previous = old_locator;
            last_discrepancy = discrepancy;
            shift = 1;
        } else {
            shift += 1;
        }
    }

    locator.resize(degree + 1, 0);
    Some(locator)
}

fn correct_with_locator(
    word: &mut [u8],
    parity_symbols: usize,
    received_syndromes: &[u8],
    locator: &[u8],
) -> Option<u8> {
    let degree = locator.len().saturating_sub(1);
    // A word symbol at polynomial degree p is erroneous when Lambda(alpha^-p)
    // is zero. A shortened word only searches its transmitted positions.
    let mut errors = Vec::with_capacity(degree);
    for power in 0..word.len() {
        let inverse_location = gf_pow(-(power as isize));
        if evaluate(locator, inverse_location) == 0 {
            errors.push((word.len() - 1 - power, gf_pow(power as isize)));
        }
    }
    if errors.len() != degree {
        return None;
    }

    // Forney error magnitudes. The generator starts at alpha^1, so the usual
    // X^(1-b) factor is one.
    let evaluator = multiply_truncated(received_syndromes, locator, parity_symbols);
    let derivative = derivative(locator);
    for &(index, location) in &errors {
        let inverse_location = gf_inv(location);
        let numerator = evaluate(&evaluator, inverse_location);
        let denominator = evaluate(&derivative, inverse_location);
        if denominator == 0 {
            return None;
        }
        word[index] ^= gf_mul(numerator, gf_inv(denominator));
    }

    // Never expose a plausible-looking miscorrection without checking that it
    // is actually a member of the code.
    if syndromes(word, parity_symbols)
        .iter()
        .any(|&syndrome| syndrome != 0)
    {
        return None;
    }
    Some(errors.len() as u8)
}

fn syndromes(word: &[u8], count: usize) -> Vec<u8> {
    (1..=count)
        .map(|root| {
            let alpha = gf_pow(root as isize);
            word.iter().fold(0u8, |accumulator, &symbol| {
                gf_mul(accumulator, alpha) ^ symbol
            })
        })
        .collect()
}

fn gf_pow(power: isize) -> u8 {
    let folded = power.rem_euclid(63) as usize;
    gf_tables().0[folded]
}

fn add_shifted_scaled(target: &mut Vec<u8>, source: &[u8], shift: usize, scale: u8) {
    target.resize(target.len().max(source.len() + shift), 0);
    for (i, &coefficient) in source.iter().enumerate() {
        target[i + shift] ^= gf_mul(scale, coefficient);
    }
}

fn evaluate(polynomial: &[u8], x: u8) -> u8 {
    let mut value = 0u8;
    let mut power = 1u8;
    for &coefficient in polynomial {
        value ^= gf_mul(coefficient, power);
        power = gf_mul(power, x);
    }
    value
}

fn multiply_truncated(a: &[u8], b: &[u8], degree: usize) -> Vec<u8> {
    let mut product = vec![0u8; degree];
    for (i, &left) in a.iter().enumerate() {
        for (j, &right) in b.iter().enumerate() {
            if i + j >= degree {
                break;
            }
            product[i + j] ^= gf_mul(left, right);
        }
    }
    product
}

fn multiply(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut product = vec![0u8; a.len() + b.len() - 1];
    for (i, &left) in a.iter().enumerate() {
        for (j, &right) in b.iter().enumerate() {
            product[i + j] ^= gf_mul(left, right);
        }
    }
    while product.len() > 1 && product.last() == Some(&0) {
        product.pop();
    }
    product
}

fn derivative(polynomial: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; polynomial.len().saturating_sub(1).max(1)];
    for i in (1..polynomial.len()).step_by(2) {
        out[i - 1] = polynomial[i];
    }
    out
}

#[cfg(test)]
fn encode(information: &[u8], parity_symbols: usize) -> Vec<u8> {
    // Build the monic generator in descending-degree order.
    let mut generator = vec![1u8];
    for root in 1..=parity_symbols {
        let alpha = gf_pow(root as isize);
        let mut next = vec![0u8; generator.len() + 1];
        for (i, &coefficient) in generator.iter().enumerate() {
            next[i] ^= coefficient;
            next[i + 1] ^= gf_mul(coefficient, alpha);
        }
        generator = next;
    }

    let mut remainder = information.to_vec();
    remainder.resize(information.len() + parity_symbols, 0);
    for i in 0..information.len() {
        let coefficient = remainder[i];
        for j in 1..=parity_symbols {
            remainder[i + j] ^= gf_mul(coefficient, generator[j]);
        }
    }
    let mut word = information.to_vec();
    word.extend_from_slice(&remainder[information.len()..]);
    word
}

#[cfg(test)]
pub(super) fn encode_for_test(information: &[u8], parity_symbols: usize) -> Vec<u8> {
    encode(information, parity_symbols)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exercise(information_symbols: usize) {
        let information: Vec<u8> = (0..information_symbols)
            .map(|i| ((i * 17 + 11) & 0x3f) as u8)
            .collect();
        let mut clean = encode(&information, 24 - information_symbols);
        assert!(
            syndromes(&clean, 24 - information_symbols)
                .iter()
                .all(|&value| value == 0)
        );
        assert_eq!(correct_24(&mut clean, information_symbols), Some(0));
        assert_eq!(&clean[..information_symbols], information);

        let capacity = (24 - information_symbols) / 2;
        for errors in 1..=capacity {
            let mut damaged = encode(&information, 24 - information_symbols);
            for n in 0..errors {
                let index = (n * 7 + 3) % damaged.len();
                damaged[index] ^= ((n * 9 + 1) & 0x3f) as u8;
            }
            assert_eq!(
                correct_24(&mut damaged, information_symbols),
                Some(errors as u8),
                "failed to correct {errors} symbol errors"
            );
            assert_eq!(&damaged[..information_symbols], information);
        }
    }

    #[test]
    fn link_control_corrects_six_symbol_errors() {
        exercise(12);
    }

    #[test]
    fn encryption_sync_corrects_four_symbol_errors() {
        exercise(16);
    }

    #[test]
    fn phase2_encryption_sync_corrects_fourteen_symbol_errors() {
        let information_symbols = 16;
        let information: Vec<u8> = (0..information_symbols)
            .map(|i| ((i * 17 + 11) & 0x3f) as u8)
            .collect();
        let mut damaged = encode(&information, 28);
        for n in 0..14 {
            let index = (n * 7 + 3) % damaged.len();
            damaged[index] ^= ((n * 9) % 63 + 1) as u8;
        }
        assert_eq!(correct(&mut damaged, information_symbols), Some(14));
        assert_eq!(&damaged[..information_symbols], information);
    }

    #[test]
    fn phase2_acch_corrects_erasures_and_unknown_errors_together() {
        let information: Vec<u8> = (0..35).map(|i| ((i * 11 + 3) & 0x3f) as u8).collect();
        let encoded = encode(&information, 28);
        let erasures = [0usize, 1, 2, 3, 4, 57, 58, 59, 60, 61, 62];
        let mut damaged = encoded.clone();
        for &position in &erasures {
            damaged[position] = 0x2a;
        }
        for (position, error) in [(9usize, 0x11), (24, 0x07), (45, 0x21)] {
            damaged[position] ^= error;
        }
        assert_eq!(
            correct_with_erasures(&mut damaged, 35, &erasures),
            Some((erasures.len() + 3) as u8)
        );
        assert_eq!(damaged, encoded);
    }

    #[test]
    fn encoder_matches_p25_generator_matrix_rows() {
        // With only the first information symbol set, systematic encoding's
        // parity is the first row of the TIA GLC/GES generator matrices.
        let mut lc = [0u8; 12];
        lc[0] = 1;
        assert_eq!(
            &encode(&lc, 12)[12..],
            &[
                0o62, 0o44, 0o03, 0o25, 0o14, 0o16, 0o27, 0o03, 0o53, 0o04, 0o36, 0o47
            ]
        );

        let mut ess = [0u8; 16];
        ess[0] = 1;
        assert_eq!(
            &encode(&ess, 8)[16..],
            &[0o51, 0o45, 0o67, 0o15, 0o64, 0o67, 0o52, 0o12]
        );
    }
}
