//! Inter prediction: motion vector prediction (clause 8.4.1) and motion
//! compensation (8.4.2), for one 16×16 partition referring to the previous
//! frame.
//!
//! Vectors are in quarter samples, as the stream carries them, but this
//! encoder only ever chooses whole-sample vectors: screen content moves by
//! whole pixels. Every vector a decoder derives from whole-sample neighbours —
//! a median, a copy, zero — is whole-sample too, so luma never needs the
//! six-tap filter. Chroma, at half the resolution, lands on half samples
//! whenever luma moves an odd distance, and is interpolated exactly as the
//! spec says.

/// A motion vector, in quarter samples.
pub(crate) type Mv = (i32, i32);

/// What a neighbouring macroblock contributes to prediction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Neighbour {
    /// Outside the picture.
    Unavailable,
    /// Intra coded: reference index -1, vector zero.
    Intra,
    /// Predicted from the reference frame with this vector.
    Inter(Mv),
}

impl Neighbour {
    fn available(self) -> bool {
        self != Self::Unavailable
    }

    /// `(refIdx, mv)`, clause 8.4.1.3.2's substitution for anything that is
    /// not an inter partition.
    fn motion(self) -> (i32, Mv) {
        match self {
            Self::Inter(mv) => (0, mv),
            _ => (-1, (0, 0)),
        }
    }
}

/// `motion` holds one entry per macroblock of the picture, `None` for intra;
/// entries after the current macroblock are never read.
fn neighbour(motion: &[Option<Mv>], mb_w: usize, x: isize, y: isize) -> Neighbour {
    if x < 0 || y < 0 || x as usize >= mb_w {
        return Neighbour::Unavailable;
    }
    match motion[y as usize * mb_w + x as usize] {
        Some(mv) => Neighbour::Inter(mv),
        None => Neighbour::Intra,
    }
}

fn median(a: i32, b: i32, c: i32) -> i32 {
    a.min(b).max(a.max(b).min(c))
}

/// The predicted vector for a 16×16 partition, clause 8.4.1.3.
pub(crate) fn predict(motion: &[Option<Mv>], mb_w: usize, mbx: usize, mby: usize) -> Mv {
    let (x, y) = (mbx as isize, mby as isize);
    let a = neighbour(motion, mb_w, x - 1, y);
    let mut b = neighbour(motion, mb_w, x, y - 1);
    let mut c = neighbour(motion, mb_w, x + 1, y - 1);
    if !c.available() {
        c = neighbour(motion, mb_w, x - 1, y - 1);
    }
    if !b.available() && !c.available() && a.available() {
        b = a;
        c = a;
    }
    let (ra, ma) = a.motion();
    let (rb, mb) = b.motion();
    let (rc, mc) = c.motion();
    match (ra == 0, rb == 0, rc == 0) {
        (true, false, false) => ma,
        (false, true, false) => mb,
        (false, false, true) => mc,
        _ => (median(ma.0, mb.0, mc.0), median(ma.1, mb.1, mc.1)),
    }
}

/// The vector a P_Skip macroblock uses, clause 8.4.1.1.
pub(crate) fn skip(motion: &[Option<Mv>], mb_w: usize, mbx: usize, mby: usize) -> Mv {
    let (x, y) = (mbx as isize, mby as isize);
    let a = neighbour(motion, mb_w, x - 1, y);
    let b = neighbour(motion, mb_w, x, y - 1);
    if !a.available()
        || !b.available()
        || a == Neighbour::Inter((0, 0))
        || b == Neighbour::Inter((0, 0))
    {
        return (0, 0);
    }
    predict(motion, mb_w, mbx, mby)
}

/// A 16×16 luma prediction from `reference`, `stride` wide and `height` high,
/// for the macroblock at sample (`x0`, `y0`). `mv` must be whole-sample.
pub(crate) fn luma(
    reference: &[u8],
    stride: usize,
    height: usize,
    (x0, y0): (usize, usize),
    mv: Mv,
) -> [u8; 256] {
    debug_assert!(mv.0 % 4 == 0 && mv.1 % 4 == 0, "a sub-sample luma vector");
    let (sx, sy) = (x0 as i32 + (mv.0 >> 2), y0 as i32 + (mv.1 >> 2));
    let (max_x, max_y) = (stride as i32 - 16, height as i32 - 16);
    let mut out = [0u8; 256];
    if (0..=max_x).contains(&sx) && (0..=max_y).contains(&sy) {
        for y in 0..16 {
            let at = (sy as usize + y) * stride + sx as usize;
            out[y * 16..y * 16 + 16].copy_from_slice(&reference[at..at + 16]);
        }
    } else {
        // Off the edge: every sample clamps into the picture, clause 8.4.2.2.1.
        for y in 0..16 {
            let row = (sy + y as i32).clamp(0, height as i32 - 1) as usize * stride;
            for x in 0..16 {
                let col = (sx + x as i32).clamp(0, stride as i32 - 1) as usize;
                out[y * 16 + x] = reference[row + col];
            }
        }
    }
    out
}

/// An 8×8 chroma prediction for the macroblock whose chroma starts at
/// (`x0`, `y0`), with the luma vector `mv`: eighth samples of chroma, clause
/// 8.4.2.2.2.
pub(crate) fn chroma(
    reference: &[u8],
    stride: usize,
    height: usize,
    (x0, y0): (usize, usize),
    mv: Mv,
) -> [u8; 64] {
    let (xi, xf) = (mv.0 >> 3, mv.0 & 7);
    let (yi, yf) = (mv.1 >> 3, mv.1 & 7);
    let at = |x: i32, y: i32| {
        let x = x.clamp(0, stride as i32 - 1) as usize;
        let y = y.clamp(0, height as i32 - 1) as usize;
        i32::from(reference[y * stride + x])
    };
    let mut out = [0u8; 64];
    for y in 0..8 {
        for x in 0..8 {
            let (px, py) = (x0 as i32 + x + xi, y0 as i32 + y + yi);
            let value = (8 - xf) * (8 - yf) * at(px, py)
                + xf * (8 - yf) * at(px + 1, py)
                + (8 - xf) * yf * at(px, py + 1)
                + xf * yf * at(px + 1, py + 1);
            out[(y * 8 + x) as usize] = ((value + 32) >> 6) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_macroblock_predicts_zero() {
        let motion = vec![None; 4];
        assert_eq!(predict(&motion, 2, 0, 0), (0, 0));
        assert_eq!(skip(&motion, 2, 0, 0), (0, 0));
    }

    #[test]
    fn along_the_top_row_the_left_vector_is_copied() {
        let motion = vec![Some((8, -4)), None, None, None];
        assert_eq!(predict(&motion, 2, 1, 0), (8, -4));
        // But skip needs the top neighbour, which the top row lacks.
        assert_eq!(skip(&motion, 2, 1, 0), (0, 0));
    }

    #[test]
    fn one_matching_reference_wins_over_the_median() {
        // Left is intra, top is inter, top-right is intra: only B refers to
        // frame 0, so its vector is used whole.
        let mut motion = vec![None; 9];
        motion[1] = Some((12, 4));
        assert_eq!(predict(&motion, 3, 1, 1), (12, 4));
    }

    #[test]
    fn three_inter_neighbours_give_the_median() {
        let mut motion = vec![None; 9];
        motion[3] = Some((4, 40)); // left
        motion[1] = Some((40, 8)); // top
        motion[2] = Some((16, 0)); // top-right
        assert_eq!(predict(&motion, 3, 1, 1), (16, 8));
        assert_eq!(skip(&motion, 3, 1, 1), (16, 8));
    }

    #[test]
    fn a_still_neighbour_makes_skip_still() {
        let mut motion = vec![None; 9];
        motion[3] = Some((0, 0));
        motion[1] = Some((40, 8));
        motion[2] = Some((40, 8));
        assert_eq!(skip(&motion, 3, 1, 1), (0, 0));
    }

    #[test]
    fn luma_clamps_off_the_edge() {
        let reference: Vec<u8> = (0..32 * 32).map(|i| (i % 32) as u8).collect();
        let pred = luma(&reference, 32, 32, (0, 0), (-40, 0));
        // Ten samples left of the picture: the first eleven columns are
        // column 0.
        assert_eq!(&pred[..11], &[0; 11]);
        assert_eq!(pred[11], 1);
    }

    #[test]
    fn chroma_half_samples_average() {
        let reference: Vec<u8> = (0..16 * 16)
            .map(|i| if i % 16 < 8 { 0 } else { 100 })
            .collect();
        // One luma sample right is half a chroma sample.
        let pred = chroma(&reference, 16, 16, (4, 0), (4, 0));
        assert_eq!(pred[3], 50);
        assert_eq!(pred[2], 0);
    }
}
