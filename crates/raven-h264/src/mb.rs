//! Macroblock coding: choosing a prediction, coding the residual, and
//! reconstructing what the decoder will see.
//!
//! Every macroblock writes its reconstruction into the picture as soon as it
//! is coded, because the macroblocks after it predict from those samples, not
//! from the source.
//!
//! In a P frame each macroblock is tried as a skip first — the vector a
//! decoder would infer, and no residual — since on a screen recording that is
//! most of them. Failing that, a short motion search picks a vector, intra
//! prediction gets a say, and whichever is cheaper is coded.

// A block index here is a position (`b % 4`, `b / 4`) and an index into
// several parallel arrays at once — levels, DCs, predictions, counts — and a
// counted loop says that more plainly than zipping four iterators.
#![allow(clippy::needless_range_loop)]

use crate::bits::BitWriter;
use crate::inter::{self, Mv};
use crate::intra::{self, Edges};
use crate::transform::{self as t, ZIGZAG};
use crate::{Planes, cavlc};

/// `luma4x4BlkIdx` to the block's position in the macroblock, in blocks.
const BLOCK_XY: [(usize, usize); 16] = [
    (0, 0),
    (1, 0),
    (0, 1),
    (1, 1),
    (2, 0),
    (3, 0),
    (2, 1),
    (3, 1),
    (0, 2),
    (1, 2),
    (0, 3),
    (1, 3),
    (2, 2),
    (3, 2),
    (2, 3),
    (3, 3),
];

/// Table 9-4, inter column: `coded_block_pattern` for each `me(v)` codeNum.
const CODE_TO_INTER_CBP: [u8; 48] = [
    0, 16, 1, 2, 4, 8, 32, 3, 5, 10, 12, 15, 47, 7, 11, 13, 14, 6, 9, 31, 35, 37, 42, 44, 33, 34,
    36, 40, 39, 43, 45, 46, 17, 18, 20, 24, 19, 21, 26, 28, 23, 27, 29, 30, 22, 25, 38, 41,
];

const fn invert(table: &[u8; 48]) -> [u8; 48] {
    let mut out = [0u8; 48];
    let mut code = 0;
    while code < 48 {
        out[table[code] as usize] = code as u8;
        code += 1;
    }
    out
}

/// The `me(v)` codeNum for an inter `coded_block_pattern`.
const INTER_CBP_CODE: [u8; 48] = invert(&CODE_TO_INTER_CBP);

/// How far, in quarter samples, a vector may reach. Vertical is the level
/// limit for 3.1 and up, horizontal the limit for every level.
const MV_X: (i32, i32) = (-8192, 8188);
const MV_Y: (i32, i32) = (-2048, 2044);

/// Per-picture macroblock state.
#[derive(Debug)]
pub(crate) struct Macroblocks {
    mb_w: usize,
    mb_h: usize,
    /// Luma coefficient counts, one per 4×4 block, `mb_w * 4` to a row: what
    /// CAVLC predicts its neighbours' tables from.
    nnz_y: Vec<u8>,
    /// Cb and Cr, one per 4×4 block, `mb_w * 2` to a row.
    nnz_c: [Vec<u8>; 2],
    /// This picture's vectors so far, `None` for intra macroblocks.
    motion: Vec<Option<Mv>>,
    /// The last picture's, as search candidates: things keep moving.
    previous: Vec<Option<Mv>>,
}

fn clip(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

fn edges<const N: usize>(plane: &[u8], stride: usize, x0: usize, y0: usize) -> Edges<N> {
    Edges {
        top: (y0 > 0).then(|| std::array::from_fn(|i| plane[(y0 - 1) * stride + x0 + i])),
        left: (x0 > 0).then(|| std::array::from_fn(|i| plane[(y0 + i) * stride + x0 - 1])),
        corner: (x0 > 0 && y0 > 0).then(|| plane[(y0 - 1) * stride + x0 - 1]),
    }
}

/// Sum of absolute differences between a prediction and the source block.
fn sad(pred: &[u8], plane: &[u8], stride: usize, x0: usize, y0: usize, n: usize) -> u32 {
    let mut total = 0;
    for y in 0..n {
        let row = &plane[(y0 + y) * stride + x0..(y0 + y) * stride + x0 + n];
        for (p, s) in pred[y * n..y * n + n].iter().zip(row) {
            total += u32::from(p.abs_diff(*s));
        }
    }
    total
}

/// Residual of the 4×4 block at (`bx`, `by`) blocks into an `n`-wide
/// prediction whose top-left sample is (`x0`, `y0`) in `plane`.
#[allow(clippy::too_many_arguments)]
fn residual(
    plane: &[u8],
    stride: usize,
    x0: usize,
    y0: usize,
    pred: &[u8],
    n: usize,
    bx: usize,
    by: usize,
) -> [i32; 16] {
    std::array::from_fn(|i| {
        let (x, y) = (bx * 4 + i % 4, by * 4 + i / 4);
        i32::from(plane[(y0 + y) * stride + x0 + x]) - i32::from(pred[y * n + x])
    })
}

/// Add a reconstructed residual to the prediction and store it.
#[allow(clippy::too_many_arguments)]
fn store(
    plane: &mut [u8],
    stride: usize,
    x0: usize,
    y0: usize,
    pred: &[u8],
    n: usize,
    bx: usize,
    by: usize,
    r: &[i32; 16],
) {
    for i in 0..16 {
        let (x, y) = (bx * 4 + i % 4, by * 4 + i / 4);
        plane[(y0 + y) * stride + x0 + x] = clip(i32::from(pred[y * n + x]) + r[i]);
    }
}

fn scan(levels: &[i32; 16]) -> [i32; 16] {
    std::array::from_fn(|i| levels[ZIGZAG[i]])
}

fn nonzero(levels: &[i32]) -> u8 {
    levels.iter().filter(|&&l| l != 0).count() as u8
}

/// `nC` for a block at (`x`, `y`) in a grid `stride` blocks wide.
fn nc(counts: &[u8], stride: usize, x: usize, y: usize) -> i32 {
    let a = (x > 0).then(|| i32::from(counts[y * stride + x - 1]));
    let b = (y > 0).then(|| i32::from(counts[(y - 1) * stride + x]));
    match (a, b) {
        (Some(a), Some(b)) => (a + b + 1) >> 1,
        (Some(n), None) | (None, Some(n)) => n,
        (None, None) => 0,
    }
}

/// Bits in `se(v)` for `value`.
fn se_bits(value: i32) -> u32 {
    let code = if value > 0 {
        2 * value as u32 - 1
    } else {
        2 * value.unsigned_abs()
    };
    2 * (32 - (code + 1).leading_zeros()) - 1
}

/// How much a bit is worth in SAD, at `qp`: the usual `0.92 * 2^((qp-12)/6)`.
fn lambda(qp: u8) -> u32 {
    (0.92 * 2f64.powf((f64::from(qp) - 12.0) / 6.0))
        .round()
        .max(1.0) as u32
}

/// A chroma macroblock's residual: both components' levels and its pattern.
struct Chroma {
    ac: [[[i32; 16]; 4]; 2],
    dc: [[i32; 4]; 2],
    /// `CodedBlockPatternChroma`: 0 nothing, 1 DC only, 2 AC too.
    cbp: u32,
}

impl Chroma {
    fn new(
        src: [&[u8]; 2],
        stride: usize,
        (x0, y0): (usize, usize),
        pred: &[[u8; 64]; 2],
        qpc: u8,
        intra: bool,
    ) -> Self {
        let mut ac = [[[0i32; 16]; 4]; 2];
        let mut dc = [[0i32; 4]; 2];
        for c in 0..2 {
            let mut dcs = [0i32; 4];
            for b in 0..4 {
                let coef = t::forward(&residual(src[c], stride, x0, y0, &pred[c], 8, b % 2, b / 2));
                dcs[b] = coef[0];
                ac[c][b] = t::quant(&coef, qpc, intra);
                ac[c][b][0] = 0;
            }
            dc[c] = t::chroma_dc_quant(&dcs, qpc, intra);
        }
        let cbp = if ac.iter().flatten().flatten().any(|&l| l != 0) {
            2
        } else if dc.iter().flatten().any(|&l| l != 0) {
            1
        } else {
            0
        };
        Self { ac, dc, cbp }
    }

    fn record(&self, nnz: &mut [Vec<u8>; 2], cw: usize, (mbx, mby): (usize, usize)) {
        for c in 0..2 {
            for b in 0..4 {
                let at = (mby * 2 + b / 2) * cw + mbx * 2 + b % 2;
                nnz[c][at] = if self.cbp == 2 {
                    nonzero(&self.ac[c][b])
                } else {
                    0
                };
            }
        }
    }

    fn write(&self, w: &mut BitWriter, nnz: &[Vec<u8>; 2], cw: usize, (mbx, mby): (usize, usize)) {
        if self.cbp > 0 {
            for dc in &self.dc {
                cavlc::residual_block(w, dc, -1);
            }
        }
        if self.cbp == 2 {
            for (c, blocks) in self.ac.iter().enumerate() {
                for (b, block) in blocks.iter().enumerate() {
                    let levels = scan(block);
                    let n = nc(&nnz[c], cw, mbx * 2 + b % 2, mby * 2 + b / 2);
                    cavlc::residual_block(w, &levels[1..], n);
                }
            }
        }
    }

    fn reconstruct(
        &self,
        recon: &mut Planes,
        stride: usize,
        (x0, y0): (usize, usize),
        pred: &[[u8; 64]; 2],
        qpc: u8,
    ) {
        for c in 0..2 {
            let dcc = t::chroma_dc_dequant(&self.dc[c], qpc);
            let plane = if c == 0 { &mut recon.cb } else { &mut recon.cr };
            for b in 0..4 {
                let mut d = t::dequant(&self.ac[c][b], qpc);
                d[0] = dcc[b];
                store(
                    plane,
                    stride,
                    x0,
                    y0,
                    &pred[c],
                    8,
                    b % 2,
                    b / 2,
                    &t::inverse(&d),
                );
            }
        }
    }
}

/// Levels for all sixteen 4×4 luma blocks of an inter macroblock, raster
/// order of blocks, and the luma half of its coded block pattern.
fn inter_luma(
    src: &[u8],
    stride: usize,
    (x0, y0): (usize, usize),
    pred: &[u8; 256],
    qp: u8,
) -> ([[i32; 16]; 16], u32) {
    let mut levels = [[0i32; 16]; 16];
    let mut cbp = 0;
    for b in 0..16 {
        let (bx, by) = (b % 4, b / 4);
        levels[b] = t::quant(
            &t::forward(&residual(src, stride, x0, y0, pred, 16, bx, by)),
            qp,
            false,
        );
        if levels[b].iter().any(|&l| l != 0) {
            cbp |= 1 << ((by / 2) * 2 + bx / 2);
        }
    }
    (levels, cbp)
}

impl Macroblocks {
    pub(crate) fn new(mb_w: usize, mb_h: usize) -> Self {
        Self {
            mb_w,
            mb_h,
            nnz_y: vec![0; mb_w * 4 * mb_h * 4],
            nnz_c: [vec![0; mb_w * 2 * mb_h * 2], vec![0; mb_w * 2 * mb_h * 2]],
            motion: vec![None; mb_w * mb_h],
            previous: vec![None; mb_w * mb_h],
        }
    }

    /// Start a picture: this picture's vectors become the last picture's.
    pub(crate) fn begin_picture(&mut self) {
        std::mem::swap(&mut self.motion, &mut self.previous);
        self.motion.fill(None);
    }

    /// The best Intra 16×16 prediction for a macroblock, and its SAD.
    fn best_intra16(
        &self,
        src: &Planes,
        recon: &Planes,
        (mbx, mby): (usize, usize),
    ) -> (u8, [u8; 256], u32) {
        let (stride, x0, y0) = (self.mb_w * 16, mbx * 16, mby * 16);
        let e = edges::<16>(&recon.y, stride, x0, y0);
        [
            intra::I16_VERTICAL,
            intra::I16_HORIZONTAL,
            intra::I16_DC,
            intra::I16_PLANE,
        ]
        .into_iter()
        .filter_map(|mode| {
            let pred = intra::luma16(mode, &e)?;
            Some((mode, pred, sad(&pred, &src.y, stride, x0, y0, 16)))
        })
        .min_by_key(|&(_, _, cost)| cost)
        .expect("DC is always available")
    }

    /// Code the macroblock at `pos` as Intra 16×16, writing its
    /// reconstruction into `recon`. The caller has written any skip run.
    pub(crate) fn intra16(
        &mut self,
        w: &mut BitWriter,
        src: &Planes,
        recon: &mut Planes,
        pos: (usize, usize),
        qp: u8,
        p_slice: bool,
    ) {
        let (mbx, mby) = pos;
        let (stride, x0, y0) = (self.mb_w * 16, mbx * 16, mby * 16);
        let (mode, pred, _) = self.best_intra16(src, recon, pos);

        // Luma: each block's DC goes through the second transform.
        let mut ac = [[0i32; 16]; 16];
        let mut dcs = [0i32; 16];
        for b in 0..16 {
            let coef = t::forward(&residual(&src.y, stride, x0, y0, &pred, 16, b % 4, b / 4));
            dcs[b] = coef[0];
            ac[b] = t::quant(&coef, qp, true);
            ac[b][0] = 0;
        }
        let dc_levels = t::luma_dc_quant(&dcs, qp);
        let any_ac = ac.iter().any(|block| block.iter().any(|&l| l != 0));

        // Chroma, both components under one prediction mode.
        let (cstride, cpos) = (self.mb_w * 8, (mbx * 8, mby * 8));
        let qpc = t::chroma_qp(qp);
        let src_c = [src.cb.as_slice(), src.cr.as_slice()];
        let ce = [
            edges::<8>(&recon.cb, cstride, cpos.0, cpos.1),
            edges::<8>(&recon.cr, cstride, cpos.0, cpos.1),
        ];
        let (chroma_mode, cpred) = [
            intra::CHROMA_DC,
            intra::CHROMA_HORIZONTAL,
            intra::CHROMA_VERTICAL,
            intra::CHROMA_PLANE,
        ]
        .into_iter()
        .filter_map(|mode| {
            Some((
                mode,
                [intra::chroma8(mode, &ce[0])?, intra::chroma8(mode, &ce[1])?],
            ))
        })
        .min_by_key(|(_, p)| {
            sad(&p[0], src_c[0], cstride, cpos.0, cpos.1, 8)
                + sad(&p[1], src_c[1], cstride, cpos.0, cpos.1, 8)
        })
        .expect("chroma DC is always available");
        let chroma = Chroma::new(src_c, cstride, cpos, &cpred, qpc, true);

        // Coefficient counts, before any block is written, so each block's
        // nC can read its neighbours inside this macroblock too.
        let bw = self.mb_w * 4;
        for b in 0..16 {
            let at = (mby * 4 + b / 4) * bw + mbx * 4 + b % 4;
            self.nnz_y[at] = if any_ac { nonzero(&ac[b]) } else { 0 };
        }
        let cw = self.mb_w * 2;
        chroma.record(&mut self.nnz_c, cw, pos);
        self.motion[mby * self.mb_w + mbx] = None;

        // mb_type packs the prediction mode and both coded block patterns.
        let mb_type = 1 + u32::from(mode) + 4 * chroma.cbp + if any_ac { 12 } else { 0 };
        w.ue(mb_type + if p_slice { 5 } else { 0 });
        w.ue(u32::from(chroma_mode));
        w.se(0); // mb_qp_delta
        cavlc::residual_block(w, &scan(&dc_levels), nc(&self.nnz_y, bw, mbx * 4, mby * 4));
        if any_ac {
            for (bx, by) in BLOCK_XY {
                let levels = scan(&ac[by * 4 + bx]);
                let n = nc(&self.nnz_y, bw, mbx * 4 + bx, mby * 4 + by);
                cavlc::residual_block(w, &levels[1..], n);
            }
        }
        chroma.write(w, &self.nnz_c, cw, pos);

        // Reconstruction, exactly as a decoder does it.
        let dcy = t::luma_dc_dequant(&dc_levels, qp);
        for b in 0..16 {
            let mut d = t::dequant(&ac[b], qp);
            d[0] = dcy[b];
            store(
                &mut recon.y,
                stride,
                x0,
                y0,
                &pred,
                16,
                b % 4,
                b / 4,
                &t::inverse(&d),
            );
        }
        chroma.reconstruct(recon, cstride, cpos, &cpred, qpc);
    }

    /// Luma SAD of predicting the macroblock at (`x0`, `y0`) with `mv`.
    fn inter_sad(&self, src: &Planes, reference: &Planes, (x0, y0): (usize, usize), mv: Mv) -> u32 {
        let stride = self.mb_w * 16;
        let pred = inter::luma(&reference.y, stride, self.mb_h * 16, (x0, y0), mv);
        sad(&pred, &src.y, stride, x0, y0, 16)
    }

    /// Code the macroblock at `pos` of a P picture: skipped, inter, or intra.
    /// `skip_run` counts skipped macroblocks not yet written.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn p_macroblock(
        &mut self,
        w: &mut BitWriter,
        src: &Planes,
        reference: &Planes,
        recon: &mut Planes,
        pos: (usize, usize),
        qp: u8,
        skip_run: &mut u32,
    ) {
        let (mbx, mby) = pos;
        let index = mby * self.mb_w + mbx;
        let (stride, height, lpos) = (self.mb_w * 16, self.mb_h * 16, (mbx * 16, mby * 16));
        let (cstride, cheight, cpos) = (self.mb_w * 8, self.mb_h * 8, (mbx * 8, mby * 8));
        let qpc = t::chroma_qp(qp);
        let src_c = [src.cb.as_slice(), src.cr.as_slice()];

        let predict = |mv: Mv| {
            (
                inter::luma(&reference.y, stride, height, lpos, mv),
                [
                    inter::chroma(&reference.cb, cstride, cheight, cpos, mv),
                    inter::chroma(&reference.cr, cstride, cheight, cpos, mv),
                ],
            )
        };

        // A skip first: most of a screen recording is exactly this.
        let skip = inter::skip(&self.motion, self.mb_w, mbx, mby);
        let (pred, cpred) = predict(skip);
        let (levels, cbp_luma) = inter_luma(&src.y, stride, lpos, &pred, qp);
        let chroma = Chroma::new(src_c, cstride, cpos, &cpred, qpc, false);
        if cbp_luma == 0 && chroma.cbp == 0 {
            self.finish_skip(recon, pos, skip, &pred, &cpred);
            *skip_run += 1;
            return;
        }

        // Search, from every vector likely to be right already.
        let mvp = inter::predict(&self.motion, self.mb_w, mbx, mby);
        let lambda = lambda(qp);
        let cost = |mv: Mv| {
            self.inter_sad(src, reference, lpos, mv)
                + lambda * (se_bits(mv.0 - mvp.0) + se_bits(mv.1 - mvp.1))
        };
        let limit = |mv: Mv| (mv.0.clamp(MV_X.0, MV_X.1), mv.1.clamp(MV_Y.0, MV_Y.1));
        let mut candidates = vec![(0, 0), skip, mvp];
        if mbx > 0 {
            candidates.extend(self.motion[index - 1]);
        }
        if mby > 0 {
            candidates.extend(self.motion[index - self.mb_w]);
        }
        candidates.extend(self.previous[index]);
        let (mut best, mut best_cost) = candidates
            .into_iter()
            .map(|mv| {
                let mv = limit(mv);
                (mv, cost(mv))
            })
            .min_by_key(|&(_, c)| c)
            .expect("there are candidates");
        for step in [64, 16, 4] {
            for _ in 0..16 {
                let around = [(step, 0), (-step, 0), (0, step), (0, -step)]
                    .into_iter()
                    .map(|(dx, dy)| limit((best.0 + dx, best.1 + dy)))
                    .map(|mv| (mv, cost(mv)))
                    .min_by_key(|&(_, c)| c)
                    .expect("four neighbours");
                if around.1 >= best_cost {
                    break;
                }
                (best, best_cost) = around;
            }
        }

        // Intra, if prediction from the last frame is no good here.
        let (_, _, intra_sad) = self.best_intra16(src, recon, pos);
        if intra_sad + 24 * lambda < best_cost {
            w.ue(*skip_run);
            *skip_run = 0;
            self.intra16(w, src, recon, pos, qp, true);
            return;
        }

        let (pred, cpred, levels, cbp_luma, chroma) = if best == skip {
            (pred, cpred, levels, cbp_luma, chroma)
        } else {
            let (pred, cpred) = predict(best);
            let (levels, cbp_luma) = inter_luma(&src.y, stride, lpos, &pred, qp);
            let chroma = Chroma::new(src_c, cstride, cpos, &cpred, qpc, false);
            (pred, cpred, levels, cbp_luma, chroma)
        };

        // Record counts, then write.
        let bw = self.mb_w * 4;
        for b in 0..16 {
            let (bx, by) = (b % 4, b / 4);
            let at = (mby * 4 + by) * bw + mbx * 4 + bx;
            self.nnz_y[at] = nonzero(&levels[b]);
        }
        let cw = self.mb_w * 2;
        chroma.record(&mut self.nnz_c, cw, pos);
        self.motion[index] = Some(best);

        w.ue(*skip_run);
        *skip_run = 0;
        w.ue(0); // mb_type: P_L0_16x16
        w.se(best.0 - mvp.0);
        w.se(best.1 - mvp.1);
        let cbp = cbp_luma | (chroma.cbp << 4);
        w.ue(u32::from(INTER_CBP_CODE[cbp as usize]));
        if cbp > 0 {
            w.se(0); // mb_qp_delta
            for (blk, (bx, by)) in BLOCK_XY.into_iter().enumerate() {
                if cbp_luma & (1 << (blk / 4)) != 0 {
                    let n = nc(&self.nnz_y, bw, mbx * 4 + bx, mby * 4 + by);
                    cavlc::residual_block(w, &scan(&levels[by * 4 + bx]), n);
                }
            }
            chroma.write(w, &self.nnz_c, cw, pos);
        }

        for b in 0..16 {
            let r = t::inverse(&t::dequant(&levels[b], qp));
            store(
                &mut recon.y,
                stride,
                lpos.0,
                lpos.1,
                &pred,
                16,
                b % 4,
                b / 4,
                &r,
            );
        }
        chroma.reconstruct(recon, cstride, cpos, &cpred, qpc);
    }

    /// A skipped macroblock: the prediction is the picture.
    fn finish_skip(
        &mut self,
        recon: &mut Planes,
        (mbx, mby): (usize, usize),
        mv: Mv,
        pred: &[u8; 256],
        cpred: &[[u8; 64]; 2],
    ) {
        let (stride, cstride) = (self.mb_w * 16, self.mb_w * 8);
        for y in 0..16 {
            let at = (mby * 16 + y) * stride + mbx * 16;
            recon.y[at..at + 16].copy_from_slice(&pred[y * 16..y * 16 + 16]);
        }
        for (plane, pred) in [&mut recon.cb, &mut recon.cr].into_iter().zip(cpred) {
            for y in 0..8 {
                let at = (mby * 8 + y) * cstride + mbx * 8;
                plane[at..at + 8].copy_from_slice(&pred[y * 8..y * 8 + 8]);
            }
        }
        let bw = self.mb_w * 4;
        for b in 0..16 {
            self.nnz_y[(mby * 4 + b / 4) * bw + mbx * 4 + b % 4] = 0;
        }
        let cw = self.mb_w * 2;
        for nnz in &mut self.nnz_c {
            for b in 0..4 {
                nnz[(mby * 2 + b / 2) * cw + mbx * 2 + b % 2] = 0;
            }
        }
        self.motion[mby * self.mb_w + mbx] = Some(mv);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_inter_cbp_table_is_a_permutation() {
        let mut seen = [false; 48];
        for cbp in CODE_TO_INTER_CBP {
            assert!(!seen[cbp as usize], "{cbp} twice");
            seen[cbp as usize] = true;
        }
        assert_eq!(INTER_CBP_CODE[0], 0);
        assert_eq!(INTER_CBP_CODE[47], 12);
    }

    #[test]
    fn se_bits_match_exp_golomb_lengths() {
        assert_eq!(se_bits(0), 1);
        assert_eq!(se_bits(1), 3);
        assert_eq!(se_bits(-1), 3);
        assert_eq!(se_bits(2), 5);
        assert_eq!(se_bits(-4), 7);
    }
}
