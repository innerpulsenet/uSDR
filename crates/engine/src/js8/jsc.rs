//! JSC, the dictionary compression of JS8 text.
//!
//! Text is cut into dictionary entries (whole words where it can, else
//! prefixes and single characters), and each entry's index is sent as a
//! (7, 9)-dense code in four-bit digits: digits 7 to 15 continue a number,
//! 0 to 6 end it, and an ending digit is followed by one bit saying whether
//! a space comes next. Common words take one or two digits.
//!
//! The dictionary is JS8Call's (GPL-3), 262 144 entries in index order,
//! one a line; entry 69, the line break, is the one empty line.

use std::sync::OnceLock;

const S: u32 = 7;
const C: u32 = 9;
const SIZE: u32 = 262_144;

fn words() -> &'static [&'static str] {
    static WORDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    WORDS.get_or_init(|| {
        let w: Vec<&'static str> = include_str!("jsc_words.txt").split('\n').take(SIZE as usize).map(|w| if w.is_empty() { "\n" } else { w }).collect();
        debug_assert_eq!(w.len(), SIZE as usize);
        w
    })
}

/// Entry `i` of the dictionary.
pub fn word(i: u32) -> Option<&'static str> {
    words().get(i as usize).copied()
}

/// The text a run of bits spells. Stops quietly where the bits stop
/// making a whole entry, as JS8Call does.
pub fn decompress(bits: &[bool]) -> String {
    // First index of the numbers that take k continuing digits.
    let mut base = [0u32; 8];
    for k in 1..8 {
        base[k] = base[k - 1] + S * C.pow(k as u32 - 1);
    }
    let mut digits: Vec<u32> = Vec::new();
    let mut spaces: Vec<usize> = Vec::new();
    let mut i = 0;
    while i + 4 <= bits.len() {
        let d = bits[i..i + 4].iter().fold(0u32, |v, &b| (v << 1) | b as u32);
        digits.push(d);
        i += 4;
        if d < S {
            if bits.get(i) == Some(&true) {
                spaces.push(digits.len() - 1);
            }
            i += 1;
        }
    }
    let mut out = String::new();
    let mut spaces = spaces.into_iter().peekable();
    let mut start = 0;
    while start < digits.len() {
        let mut k = 0;
        let mut j = 0u32;
        while start + k < digits.len() && digits[start + k] >= S && k < 8 {
            j = j * C + (digits[start + k] - S);
            k += 1;
        }
        if j >= SIZE || k >= 8 || start + k >= digits.len() {
            break;
        }
        let j = j * S + digits[start + k] + base[k];
        let Some(w) = word(j) else { break };
        out.push_str(w);
        if spaces.peek() == Some(&(start + k)) {
            out.push(' ');
            spaces.next();
        }
        start += k + 1;
    }
    out
}

/// For the test encoder: the code of entry `index`, with or without a
/// space after it.
#[cfg(test)]
pub fn codeword(index: u32, space: bool) -> Vec<bool> {
    let to_bits = |v: u32, n: usize| (0..n).rev().map(move |i| (v >> i) & 1 == 1);
    let mut groups: Vec<Vec<bool>> = vec![to_bits(((index % S) << 1) | space as u32, 5).collect()];
    let mut x = index / S;
    while x > 0 {
        x -= 1;
        groups.insert(0, to_bits(x % C + S, 4).collect());
        x /= C;
    }
    groups.concat()
}

/// For the test encoder: text as JSC codes, greedily by longest entry,
/// the way JS8Call's compressor cuts it, and how many characters each
/// code covers.
#[cfg(test)]
pub fn compress(text: &str) -> Vec<(Vec<bool>, usize)> {
    use std::collections::HashMap;
    static INDEX: OnceLock<HashMap<&'static str, u32>> = OnceLock::new();
    let index = INDEX.get_or_init(|| {
        let mut m = HashMap::new();
        for (i, w) in words().iter().enumerate() {
            m.entry(*w).or_insert(i as u32);
        }
        m
    });
    let mut out = Vec::new();
    let parts: Vec<&str> = text.split(' ').collect();
    for (pi, part) in parts.iter().enumerate() {
        let last_part = pi + 1 == parts.len();
        let (mut w, is_space) = if part.is_empty() && !last_part { (" ", true) } else { (*part, false) };
        while !w.is_empty() {
            let Some((len, idx)) = (1..=w.len()).rev().filter(|&n| w.is_char_boundary(n)).find_map(|n| index.get(&w[..n]).map(|&i| (n, i))) else {
                break;
            };
            w = &w[len..];
            let space = w.is_empty() && !is_space && !last_part;
            out.push((codeword(idx, space), len + usize::from(space)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dictionary_is_whole() {
        assert_eq!(words().len(), SIZE as usize);
        assert_eq!(word(0), Some("E"));
        assert_eq!(word(69), Some("\n"));
        assert_eq!(word(70), Some("JS8"));
        assert_eq!(word(SIZE - 1), Some("ROSIDS"));
    }

    #[test]
    fn text_round_trips_through_the_code() {
        for text in ["HELLO WORLD", "CQ CQ DE KN4CRD EM73", "TNX FER THE QSO 73", "THE QUICK BROWN FOX JUMPS"] {
            let bits: Vec<bool> = compress(text).into_iter().flat_map(|(b, _)| b).collect();
            assert_eq!(decompress(&bits), text);
        }
    }
}
