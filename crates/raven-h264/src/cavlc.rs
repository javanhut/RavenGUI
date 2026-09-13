//! CAVLC: how a block of levels becomes bits, clause 9.2.
//!
//! The tables are the spec's (Tables 9-5, 9-7, 9-8, 9-9 and 9-10), each code
//! stored as its length and value. They are checked by the conformance tests:
//! a wrong entry is a stream ffmpeg decodes differently or not at all.

use crate::bits::BitWriter;

/// `coeff_token` lengths, by `nC` class, indexed `TotalCoeff * 4 + TrailingOnes`.
const TOKEN_LEN: [[u8; 68]; 4] = [
    [
        1, 0, 0, 0, 6, 2, 0, 0, 8, 6, 3, 0, 9, 8, 7, 5, 10, 9, 8, 6, 11, 10, 9, 7, 13, 11, 10, 8,
        13, 13, 11, 9, 13, 13, 13, 10, 14, 14, 13, 11, 14, 14, 14, 13, 15, 15, 14, 14, 15, 15, 15,
        14, 16, 15, 15, 15, 16, 16, 16, 15, 16, 16, 16, 16, 16, 16, 16, 16,
    ],
    [
        2, 0, 0, 0, 6, 2, 0, 0, 6, 5, 3, 0, 7, 6, 6, 4, 8, 6, 6, 4, 8, 7, 7, 5, 9, 8, 8, 6, 11, 9,
        9, 6, 11, 11, 11, 7, 12, 11, 11, 9, 12, 12, 12, 11, 12, 12, 12, 11, 13, 13, 13, 12, 13, 13,
        13, 13, 13, 14, 13, 13, 14, 14, 14, 13, 14, 14, 14, 14,
    ],
    [
        4, 0, 0, 0, 6, 4, 0, 0, 6, 5, 4, 0, 6, 5, 5, 4, 7, 5, 5, 4, 7, 5, 5, 4, 7, 6, 6, 4, 7, 6,
        6, 4, 8, 7, 7, 5, 8, 8, 7, 6, 9, 8, 8, 7, 9, 9, 8, 8, 9, 9, 9, 8, 10, 9, 9, 9, 10, 10, 10,
        10, 10, 10, 10, 10, 10, 10, 10, 10,
    ],
    [6; 68],
];

/// `coeff_token` values, laid out like [`TOKEN_LEN`].
const TOKEN_BITS: [[u8; 68]; 4] = [
    [
        1, 0, 0, 0, 5, 1, 0, 0, 7, 4, 1, 0, 7, 6, 5, 3, 7, 6, 5, 3, 7, 6, 5, 4, 15, 6, 5, 4, 11,
        14, 5, 4, 8, 10, 13, 4, 15, 14, 9, 4, 11, 10, 13, 12, 15, 14, 9, 12, 11, 10, 13, 8, 15, 1,
        9, 12, 11, 14, 13, 8, 7, 10, 9, 12, 4, 6, 5, 8,
    ],
    [
        3, 0, 0, 0, 11, 2, 0, 0, 7, 7, 3, 0, 7, 10, 9, 5, 7, 6, 5, 4, 4, 6, 5, 6, 7, 6, 5, 8, 15,
        6, 5, 4, 11, 14, 13, 4, 15, 10, 9, 4, 11, 14, 13, 12, 8, 10, 9, 8, 15, 14, 13, 12, 11, 10,
        9, 12, 7, 11, 6, 8, 9, 8, 10, 1, 7, 6, 5, 4,
    ],
    [
        15, 0, 0, 0, 15, 14, 0, 0, 11, 15, 13, 0, 8, 12, 14, 12, 15, 10, 11, 11, 11, 8, 9, 10, 9,
        14, 13, 9, 8, 10, 9, 8, 15, 14, 13, 13, 11, 14, 10, 12, 15, 10, 13, 12, 11, 14, 9, 12, 8,
        10, 13, 8, 13, 7, 9, 12, 9, 12, 11, 10, 5, 8, 7, 6, 1, 4, 3, 2,
    ],
    // nC >= 8 is a fixed-length code; see `coeff_token`.
    [0; 68],
];

/// `coeff_token` for chroma DC (`nC` = -1), indexed like the others.
const DC_TOKEN_LEN: [u8; 20] = [2, 0, 0, 0, 6, 1, 0, 0, 6, 6, 3, 0, 6, 7, 7, 6, 6, 8, 8, 7];
const DC_TOKEN_BITS: [u8; 20] = [1, 0, 0, 0, 7, 1, 0, 0, 4, 6, 1, 0, 3, 3, 2, 5, 2, 3, 2, 0];

/// `total_zeros` for 4×4 blocks, indexed `[TotalCoeff - 1][total_zeros]`.
const ZEROS_LEN: [[u8; 16]; 15] = [
    [1, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 9],
    [3, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 6, 6, 6, 6, 0],
    [4, 3, 3, 3, 4, 4, 3, 3, 4, 5, 5, 6, 5, 6, 0, 0],
    [5, 3, 4, 4, 3, 3, 3, 4, 3, 4, 5, 5, 5, 0, 0, 0],
    [4, 4, 4, 3, 3, 3, 3, 3, 4, 5, 4, 5, 0, 0, 0, 0],
    [6, 5, 3, 3, 3, 3, 3, 3, 4, 3, 6, 0, 0, 0, 0, 0],
    [6, 5, 3, 3, 3, 2, 3, 4, 3, 6, 0, 0, 0, 0, 0, 0],
    [6, 4, 5, 3, 2, 2, 3, 3, 6, 0, 0, 0, 0, 0, 0, 0],
    [6, 6, 4, 2, 2, 3, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0],
    [5, 5, 3, 2, 2, 2, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [4, 4, 3, 3, 1, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [4, 4, 2, 1, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 3, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [2, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
];
const ZEROS_BITS: [[u8; 16]; 15] = [
    [1, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 1],
    [7, 6, 5, 4, 3, 5, 4, 3, 2, 3, 2, 3, 2, 1, 0, 0],
    [5, 7, 6, 5, 4, 3, 4, 3, 2, 3, 2, 1, 1, 0, 0, 0],
    [3, 7, 5, 4, 6, 5, 4, 3, 3, 2, 2, 1, 0, 0, 0, 0],
    [5, 4, 3, 7, 6, 5, 4, 3, 2, 1, 1, 0, 0, 0, 0, 0],
    [1, 1, 7, 6, 5, 4, 3, 2, 1, 1, 0, 0, 0, 0, 0, 0],
    [1, 1, 5, 4, 3, 3, 2, 1, 1, 0, 0, 0, 0, 0, 0, 0],
    [1, 1, 1, 3, 3, 2, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 0, 1, 3, 2, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 0, 1, 3, 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 1, 2, 1, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
];

/// `total_zeros` for chroma DC, indexed `[TotalCoeff - 1][total_zeros]`.
const DC_ZEROS_LEN: [[u8; 4]; 3] = [[1, 2, 3, 3], [1, 2, 2, 0], [1, 1, 0, 0]];
const DC_ZEROS_BITS: [[u8; 4]; 3] = [[1, 1, 1, 0], [1, 1, 0, 0], [1, 0, 0, 0]];

/// `run_before`, indexed `[min(zerosLeft, 7) - 1][run_before]`.
const RUN_LEN: [[u8; 15]; 7] = [
    [1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 2, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [2, 2, 2, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [2, 2, 2, 3, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [2, 2, 3, 3, 3, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [2, 3, 3, 3, 3, 3, 3, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 3, 3, 3, 3, 3, 3, 4, 5, 6, 7, 8, 9, 10, 11],
];
const RUN_BITS: [[u8; 15]; 7] = [
    [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 2, 3, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 0, 1, 3, 2, 5, 4, 0, 0, 0, 0, 0, 0, 0, 0],
    [7, 6, 5, 4, 3, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1],
];

fn coeff_token(w: &mut BitWriter, nc: i32, total: usize, ones: usize) {
    let index = total * 4 + ones;
    let (len, bits) = match nc {
        -1 => (DC_TOKEN_LEN[index], DC_TOKEN_BITS[index]),
        0..=1 => (TOKEN_LEN[0][index], TOKEN_BITS[0][index]),
        2..=3 => (TOKEN_LEN[1][index], TOKEN_BITS[1][index]),
        4..=7 => (TOKEN_LEN[2][index], TOKEN_BITS[2][index]),
        _ => {
            let code = if total == 0 {
                3
            } else {
                ((total as u32 - 1) << 2) | ones as u32
            };
            w.put(6, code);
            return;
        }
    };
    w.put(u32::from(len), u32::from(bits));
}

/// Write one `residual_block_cavlc()`.
///
/// `coeffs` are the block's levels in scan order, as many as the block holds:
/// 16, 15 for an AC block whose DC is coded elsewhere, 4 for chroma DC. `nc`
/// is the predicted count from the neighbours, or -1 for chroma DC. Returns
/// the block's `TotalCoeff`, which its neighbours' `nc` is worked out from.
pub(crate) fn residual_block(w: &mut BitWriter, coeffs: &[i32], nc: i32) -> u8 {
    let max = coeffs.len();
    // Non-zero levels, highest frequency first: the order they are coded in.
    let mut levels = [(0usize, 0i32); 16];
    let mut total = 0;
    for (pos, &level) in coeffs.iter().enumerate().rev() {
        if level != 0 {
            levels[total] = (pos, level);
            total += 1;
        }
    }
    let levels = &levels[..total];
    let ones = levels
        .iter()
        .take(3)
        .take_while(|&&(_, level)| level.abs() == 1)
        .count();

    coeff_token(w, nc, total, ones);
    if total == 0 {
        return 0;
    }

    let mut suffix_len: u32 = if total > 10 && ones < 3 { 1 } else { 0 };
    for (i, &(_, level)) in levels.iter().enumerate() {
        if i < ones {
            w.flag(level < 0);
            continue;
        }
        let mut code = if level > 0 {
            2 * level - 2
        } else {
            -2 * level - 1
        };
        if i == ones && ones < 3 {
            code -= 2;
        }
        let code = code as u32;
        let (prefix, size, suffix) = if suffix_len == 0 {
            if code < 14 {
                (code, 0, 0)
            } else if code < 30 {
                (14, 4, code - 14)
            } else {
                (15, 12, code - 30)
            }
        } else if code < 15 << suffix_len {
            (
                code >> suffix_len,
                suffix_len,
                code & ((1 << suffix_len) - 1),
            )
        } else {
            (15, 12, code - (15 << suffix_len))
        };
        debug_assert!(suffix < 1 << 12, "a level past MAX_LEVEL");
        w.put(prefix, 0);
        w.put(1, 1);
        w.put(size, suffix);

        if suffix_len == 0 {
            suffix_len = 1;
        }
        if level.unsigned_abs() > 3 << (suffix_len - 1) && suffix_len < 6 {
            suffix_len += 1;
        }
    }

    let highest = levels[0].0;
    let total_zeros = highest + 1 - total;
    if total < max {
        let (len, bits) = if max == 4 {
            (
                DC_ZEROS_LEN[total - 1][total_zeros],
                DC_ZEROS_BITS[total - 1][total_zeros],
            )
        } else {
            (
                ZEROS_LEN[total - 1][total_zeros],
                ZEROS_BITS[total - 1][total_zeros],
            )
        };
        w.put(u32::from(len), u32::from(bits));
    }

    let mut zeros_left = total_zeros;
    for pair in levels.windows(2) {
        if zeros_left == 0 {
            break;
        }
        let run = pair[0].0 - pair[1].0 - 1;
        let table = zeros_left.min(7) - 1;
        w.put(
            u32::from(RUN_LEN[table][run]),
            u32::from(RUN_BITS[table][run]),
        );
        zeros_left -= run;
    }
    total as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coded(coeffs: &[i32], nc: i32) -> String {
        let mut w = BitWriter::new();
        residual_block(&mut w, coeffs, nc);
        w.bit_string()
    }

    #[test]
    fn the_textbook_block_codes_as_published() {
        // Richardson's worked example: 0 3 -1 0 / 0 -1 1 0 / 1 0 0 0 / 0 0 0 0
        // in zig-zag order.
        let scan = [0, 3, 0, 1, -1, -1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(coded(&scan, 0), "000010001110010111101101");
    }

    #[test]
    fn an_empty_block_is_just_its_token() {
        assert_eq!(coded(&[0; 16], 0), "1");
        assert_eq!(coded(&[0; 16], 2), "11");
        assert_eq!(coded(&[0; 16], 8), "000011");
        assert_eq!(coded(&[0; 4], -1), "01");
    }

    #[test]
    fn big_levels_use_the_escape() {
        // Must not panic, and must end on a whole code.
        let mut scan = [0; 16];
        scan[0] = super::super::transform::MAX_LEVEL;
        scan[3] = -MAXISH;
        let bits = coded(&scan, 0);
        assert!(bits.len() > 20);
    }

    const MAXISH: i32 = 1500;
}
