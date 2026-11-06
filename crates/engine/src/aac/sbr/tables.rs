//! SBR table data and the Huffman decoders built from it.

use super::super::huffman::Huffman;
use std::sync::OnceLock;

#[path = "tables_data.rs"]
pub(crate) mod data;

pub(super) use data::{NOISE_TABLE, QMF_WINDOW};

/// The ten SBR codebooks, with the offset that centers each one's symbols
/// on zero.
pub(super) struct Books {
    pub t_env_15: (Huffman, i32),
    pub f_env_15: (Huffman, i32),
    pub t_bal_15: (Huffman, i32),
    pub f_bal_15: (Huffman, i32),
    pub t_env_30: (Huffman, i32),
    pub f_env_30: (Huffman, i32),
    pub t_bal_30: (Huffman, i32),
    pub f_bal_30: (Huffman, i32),
    pub t_noise_30: (Huffman, i32),
    pub t_noise_bal_30: (Huffman, i32),
}

pub(super) fn books() -> &'static Books {
    static B: OnceLock<Books> = OnceLock::new();
    B.get_or_init(|| {
        fn book(codes: &[(u32, u8)]) -> (Huffman, i32) {
            (Huffman::new(codes), (codes.len() as i32 - 1) / 2)
        }
        Books {
            t_env_15: book(&data::T_ENV_1_5DB),
            f_env_15: book(&data::F_ENV_1_5DB),
            t_bal_15: book(&data::T_ENV_BAL_1_5DB),
            f_bal_15: book(&data::F_ENV_BAL_1_5DB),
            t_env_30: book(&data::T_ENV_3_0DB),
            f_env_30: book(&data::F_ENV_3_0DB),
            t_bal_30: book(&data::T_ENV_BAL_3_0DB),
            f_bal_30: book(&data::F_ENV_BAL_3_0DB),
            t_noise_30: book(&data::T_NOISE_3_0DB),
            t_noise_bal_30: book(&data::T_NOISE_BAL_3_0DB),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codebooks_build_and_window_is_the_standard_one() {
        let b = books();
        assert_eq!(b.t_env_15.1, 60);
        assert_eq!(b.f_bal_30.1, 12);
        // Spot values of table 4.A.89.
        for (i, v) in [(1, -0.000_552_528_6), (320, 0.853_738_546_4), (384, -0.361_158_996_8), (639, -0.000_552_528_6)] {
            assert!((f64::from(QMF_WINDOW[i]) - v).abs() < 1e-9, "c[{i}]");
        }
    }
}
