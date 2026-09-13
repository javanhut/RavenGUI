//! The 4×4 integer transform, quantisation, and their inverses.
//!
//! The inverses are normative — clause 8.5 — and must be bit exact, because
//! they are how the encoder knows what a decoder will see. The forward side is
//! the encoder's own choice; this is the usual one, the H.264 core transform
//! with dead-zone quantisation.
//!
//! Blocks are 16 values in raster order, row first.

/// Zig-zag scan: scan index to raster position.
pub(crate) const ZIGZAG: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];

/// The largest level the stream can carry: Baseline caps `level_prefix` at
/// 15, and 2063 is the most that reaches when the suffix is shortest.
pub(crate) const MAX_LEVEL: i32 = 2000;

/// Forward quantiser multipliers by `qp % 6`: positions with both coordinates
/// even, both odd, and the rest.
const MF: [[i32; 3]; 6] = [
    [13107, 5243, 8066],
    [11916, 4660, 7490],
    [10082, 4194, 6554],
    [9362, 3647, 5825],
    [8192, 3355, 5243],
    [7282, 2893, 4559],
];

/// `normAdjust4x4` by `qp % 6`, in the same three position classes. With the
/// flat scaling matrix `LevelScale4x4` is sixteen times this.
const V: [[i32; 3]; 6] = [
    [10, 16, 13],
    [11, 18, 14],
    [13, 20, 16],
    [14, 23, 18],
    [16, 25, 20],
    [18, 29, 23],
];

fn class(pos: usize) -> usize {
    match (pos % 4 % 2, pos / 4 % 2) {
        (0, 0) => 0,
        (1, 1) => 1,
        _ => 2,
    }
}

/// `QPc` from `QPy`, Table 8-15, with a chroma offset of zero.
pub(crate) fn chroma_qp(qp: u8) -> u8 {
    const HIGH: [u8; 22] = [
        29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39, 39, 39,
    ];
    if qp < 30 {
        qp
    } else {
        HIGH[usize::from(qp.min(51) - 30)]
    }
}

/// The core transform of a residual block.
pub(crate) fn forward(x: &[i32; 16]) -> [i32; 16] {
    let one = |a: i32, b: i32, c: i32, d: i32| {
        let (s03, s12, d03, d12) = (a + d, b + c, a - d, b - c);
        [s03 + s12, 2 * d03 + d12, s03 - s12, d03 - 2 * d12]
    };
    let mut rows = [0; 16];
    for r in 0..4 {
        let o = one(x[r * 4], x[r * 4 + 1], x[r * 4 + 2], x[r * 4 + 3]);
        rows[r * 4..r * 4 + 4].copy_from_slice(&o);
    }
    let mut out = [0; 16];
    for c in 0..4 {
        let o = one(rows[c], rows[4 + c], rows[8 + c], rows[12 + c]);
        for r in 0..4 {
            out[r * 4 + c] = o[r];
        }
    }
    out
}

fn quantise(value: i32, mf: i32, bits: u32, round: i32) -> i32 {
    let level = ((value.abs() * mf + round) >> bits).min(MAX_LEVEL);
    if value < 0 { -level } else { level }
}

/// Quantise a transformed block. Position 0 is quantised like the rest; a
/// caller whose DC goes through its own transform ignores it.
pub(crate) fn quant(coef: &[i32; 16], qp: u8, intra: bool) -> [i32; 16] {
    let bits = 15 + u32::from(qp / 6);
    let round = (1 << bits) / if intra { 3 } else { 6 };
    let mf = &MF[usize::from(qp % 6)];
    std::array::from_fn(|pos| quantise(coef[pos], mf[class(pos)], bits, round))
}

/// Scale levels back to coefficients: clause 8.5.12.1 for a flat matrix.
pub(crate) fn dequant(levels: &[i32; 16], qp: u8) -> [i32; 16] {
    let v = &V[usize::from(qp % 6)];
    let shift = u32::from(qp / 6);
    std::array::from_fn(|pos| (levels[pos] * v[class(pos)]) << shift)
}

/// The inverse transform, clause 8.5.12.2: coefficients to residual.
pub(crate) fn inverse(d: &[i32; 16]) -> [i32; 16] {
    let one = |d0: i32, d1: i32, d2: i32, d3: i32| {
        let (e0, e1, e2, e3) = (d0 + d2, d0 - d2, (d1 >> 1) - d3, d1 + (d3 >> 1));
        [e0 + e3, e1 + e2, e1 - e2, e0 - e3]
    };
    let mut rows = [0; 16];
    for r in 0..4 {
        let o = one(d[r * 4], d[r * 4 + 1], d[r * 4 + 2], d[r * 4 + 3]);
        rows[r * 4..r * 4 + 4].copy_from_slice(&o);
    }
    let mut out = [0; 16];
    for c in 0..4 {
        let o = one(rows[c], rows[4 + c], rows[8 + c], rows[12 + c]);
        for r in 0..4 {
            out[r * 4 + c] = (o[r] + 32) >> 6;
        }
    }
    out
}

fn hadamard4(x: [i32; 4]) -> [i32; 4] {
    [
        x[0] + x[1] + x[2] + x[3],
        x[0] + x[1] - x[2] - x[3],
        x[0] - x[1] - x[2] + x[3],
        x[0] - x[1] + x[2] - x[3],
    ]
}

fn hadamard16(x: &[i32; 16]) -> [i32; 16] {
    let mut rows = [0; 16];
    for r in 0..4 {
        let o = hadamard4([x[r * 4], x[r * 4 + 1], x[r * 4 + 2], x[r * 4 + 3]]);
        rows[r * 4..r * 4 + 4].copy_from_slice(&o);
    }
    let mut out = [0; 16];
    for c in 0..4 {
        let o = hadamard4([rows[c], rows[4 + c], rows[8 + c], rows[12 + c]]);
        for r in 0..4 {
            out[r * 4 + c] = o[r];
        }
    }
    out
}

/// Levels for an Intra 16×16 macroblock's DC coefficients, `dcs` holding each
/// 4×4 block's transformed DC in raster order of blocks.
pub(crate) fn luma_dc_quant(dcs: &[i32; 16], qp: u8) -> [i32; 16] {
    let bits = 16 + u32::from(qp / 6);
    let round = (1 << bits) / 3;
    let mf = MF[usize::from(qp % 6)][0];
    let f = hadamard16(dcs);
    std::array::from_fn(|i| quantise(f[i] >> 1, mf, bits, round))
}

/// The DC values a decoder puts into each 4×4 block, clause 8.5.10.
pub(crate) fn luma_dc_dequant(levels: &[i32; 16], qp: u8) -> [i32; 16] {
    let f = hadamard16(levels);
    let scale = 16 * V[usize::from(qp % 6)][0];
    let per = u32::from(qp / 6);
    std::array::from_fn(|i| {
        if qp >= 36 {
            (f[i] * scale) << (per - 6)
        } else {
            (f[i] * scale + (1 << (5 - per))) >> (6 - per)
        }
    })
}

fn hadamard2(c: &[i32; 4]) -> [i32; 4] {
    [
        c[0] + c[1] + c[2] + c[3],
        c[0] - c[1] + c[2] - c[3],
        c[0] + c[1] - c[2] - c[3],
        c[0] - c[1] - c[2] + c[3],
    ]
}

/// Levels for a chroma component's four DC coefficients.
pub(crate) fn chroma_dc_quant(dcs: &[i32; 4], qpc: u8, intra: bool) -> [i32; 4] {
    let bits = 16 + u32::from(qpc / 6);
    let round = (1 << bits) / if intra { 3 } else { 6 };
    let mf = MF[usize::from(qpc % 6)][0];
    let f = hadamard2(dcs);
    std::array::from_fn(|i| quantise(f[i], mf, bits, round))
}

/// The DC values a decoder puts into each chroma 4×4 block, clause 8.5.11.
pub(crate) fn chroma_dc_dequant(levels: &[i32; 4], qpc: u8) -> [i32; 4] {
    let f = hadamard2(levels);
    let scale = 16 * V[usize::from(qpc % 6)][0];
    std::array::from_fn(|i| ((f[i] * scale) << u32::from(qpc / 6)) >> 5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flat_residual_survives_the_round_trip() {
        for qp in [0u8, 12, 24, 36, 51] {
            let x = [20; 16];
            let levels = quant(&forward(&x), qp, true);
            let back = inverse(&dequant(&levels, qp));
            let err = (back[5] - 20).abs();
            assert!(err <= 1 + i32::from(qp) / 2, "qp {qp}: {back:?}");
        }
    }

    #[test]
    fn low_qp_is_near_lossless() {
        let x: [i32; 16] = std::array::from_fn(|i| (i as i32 * 37 % 51) - 25);
        let back = inverse(&dequant(&quant(&forward(&x), 0, true), 0));
        for (a, b) in x.iter().zip(back) {
            assert!((a - b).abs() <= 1, "{x:?} came back as {back:?}");
        }
    }

    #[test]
    fn chroma_qp_follows_the_table() {
        assert_eq!(chroma_qp(29), 29);
        assert_eq!(chroma_qp(30), 29);
        assert_eq!(chroma_qp(34), 32);
        assert_eq!(chroma_qp(51), 39);
    }

    #[test]
    fn zigzag_is_a_permutation() {
        let mut seen = [false; 16];
        for p in ZIGZAG {
            seen[p] = true;
        }
        assert!(seen.iter().all(|&s| s));
    }
}
