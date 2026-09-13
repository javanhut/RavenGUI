//! Intra prediction: 16×16 luma (clause 8.3.3) and 8×8 chroma (8.3.4).
//!
//! Every function here is normative — the decoder predicts exactly this — so
//! the prediction must match to the sample. A mode whose neighbours are not
//! there is not available, and asking for one returns `None`.

/// The neighbours a block is predicted from: the row above, the column to the
/// left, and the sample above-left, each absent at the picture's edge.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Edges<const N: usize> {
    pub(crate) top: Option<[u8; N]>,
    pub(crate) left: Option<[u8; N]>,
    pub(crate) corner: Option<u8>,
}

pub(crate) const I16_VERTICAL: u8 = 0;
pub(crate) const I16_HORIZONTAL: u8 = 1;
pub(crate) const I16_DC: u8 = 2;
pub(crate) const I16_PLANE: u8 = 3;

pub(crate) const CHROMA_DC: u8 = 0;
pub(crate) const CHROMA_HORIZONTAL: u8 = 1;
pub(crate) const CHROMA_VERTICAL: u8 = 2;
pub(crate) const CHROMA_PLANE: u8 = 3;

fn clip(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

fn sum(samples: &[u8]) -> i32 {
    samples.iter().map(|&s| i32::from(s)).sum()
}

/// A 16×16 luma prediction, or `None` if `mode` needs a missing neighbour.
pub(crate) fn luma16(mode: u8, e: &Edges<16>) -> Option<[u8; 256]> {
    let mut out = [0u8; 256];
    match mode {
        I16_VERTICAL => {
            let top = e.top?;
            for row in out.as_chunks_mut::<16>().0 {
                *row = top;
            }
        }
        I16_HORIZONTAL => {
            let left = e.left?;
            for (row, &l) in out.as_chunks_mut::<16>().0.iter_mut().zip(&left) {
                row.fill(l);
            }
        }
        I16_DC => {
            let dc = match (e.top, e.left) {
                (Some(t), Some(l)) => (sum(&t) + sum(&l) + 16) >> 5,
                (None, Some(l)) => (sum(&l) + 8) >> 4,
                (Some(t), None) => (sum(&t) + 8) >> 4,
                (None, None) => 128,
            };
            out.fill(dc as u8);
        }
        I16_PLANE => {
            let (top, left, corner) = (e.top?, e.left?, i32::from(e.corner?));
            let t = |x: i32| {
                if x < 0 {
                    corner
                } else {
                    i32::from(top[x as usize])
                }
            };
            let l = |y: i32| {
                if y < 0 {
                    corner
                } else {
                    i32::from(left[y as usize])
                }
            };
            let h: i32 = (0..8).map(|x| (x + 1) * (t(8 + x) - t(6 - x))).sum();
            let v: i32 = (0..8).map(|y| (y + 1) * (l(8 + y) - l(6 - y))).sum();
            let a = 16 * (l(15) + t(15));
            let (b, c) = ((5 * h + 32) >> 6, (5 * v + 32) >> 6);
            for y in 0..16 {
                for x in 0..16 {
                    out[(y * 16 + x) as usize] = clip((a + b * (x - 7) + c * (y - 7) + 16) >> 5);
                }
            }
        }
        _ => return None,
    }
    Some(out)
}

/// An 8×8 chroma prediction, or `None` if `mode` needs a missing neighbour.
pub(crate) fn chroma8(mode: u8, e: &Edges<8>) -> Option<[u8; 64]> {
    let mut out = [0u8; 64];
    match mode {
        CHROMA_DC => {
            for (yo, xo) in [(0, 0), (0, 4), (4, 0), (4, 4)] {
                let top = e.top.map(|t| sum(&t[xo..xo + 4]));
                let left = e.left.map(|l| sum(&l[yo..yo + 4]));
                let dc = if xo == yo {
                    match (top, left) {
                        (Some(t), Some(l)) => (t + l + 4) >> 3,
                        (None, Some(l)) => (l + 2) >> 2,
                        (Some(t), None) => (t + 2) >> 2,
                        (None, None) => 128,
                    }
                } else if xo > 0 {
                    // The top-right block prefers the row above.
                    match (top, left) {
                        (Some(t), _) => (t + 2) >> 2,
                        (None, Some(l)) => (l + 2) >> 2,
                        (None, None) => 128,
                    }
                } else {
                    // The bottom-left block prefers the column to its left.
                    match (left, top) {
                        (Some(l), _) => (l + 2) >> 2,
                        (None, Some(t)) => (t + 2) >> 2,
                        (None, None) => 128,
                    }
                };
                for y in yo..yo + 4 {
                    out[y * 8 + xo..y * 8 + xo + 4].fill(dc as u8);
                }
            }
        }
        CHROMA_HORIZONTAL => {
            let left = e.left?;
            for (row, &l) in out.as_chunks_mut::<8>().0.iter_mut().zip(&left) {
                row.fill(l);
            }
        }
        CHROMA_VERTICAL => {
            let top = e.top?;
            for row in out.as_chunks_mut::<8>().0 {
                *row = top;
            }
        }
        CHROMA_PLANE => {
            let (top, left, corner) = (e.top?, e.left?, i32::from(e.corner?));
            let t = |x: i32| {
                if x < 0 {
                    corner
                } else {
                    i32::from(top[x as usize])
                }
            };
            let l = |y: i32| {
                if y < 0 {
                    corner
                } else {
                    i32::from(left[y as usize])
                }
            };
            let h: i32 = (0..4).map(|x| (x + 1) * (t(4 + x) - t(2 - x))).sum();
            let v: i32 = (0..4).map(|y| (y + 1) * (l(4 + y) - l(2 - y))).sum();
            let a = 16 * (l(7) + t(7));
            let (b, c) = ((34 * h + 32) >> 6, (34 * v + 32) >> 6);
            for y in 0..8 {
                for x in 0..8 {
                    out[(y * 8 + x) as usize] = clip((a + b * (x - 3) + c * (y - 3) + 16) >> 5);
                }
            }
        }
        _ => return None,
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_no_neighbours_only_dc_is_available_and_it_is_grey() {
        let none = Edges::<16> {
            top: None,
            left: None,
            corner: None,
        };
        assert_eq!(luma16(I16_DC, &none), Some([128; 256]));
        assert!(luma16(I16_VERTICAL, &none).is_none());
        assert!(luma16(I16_PLANE, &none).is_none());
        let none = Edges::<8> {
            top: None,
            left: None,
            corner: None,
        };
        assert_eq!(chroma8(CHROMA_DC, &none), Some([128; 64]));
    }

    #[test]
    fn plane_prediction_continues_a_gradient() {
        // A ramp rising two per sample to the right, continued from the edges.
        let top: [u8; 16] = std::array::from_fn(|x| 100 + 2 * x as u8);
        let left = [98u8; 16];
        let e = Edges {
            top: Some(top),
            left: Some(left),
            corner: Some(98),
        };
        let pred = luma16(I16_PLANE, &e).unwrap();
        // The top row is extended downwards, roughly: it is a plane fit.
        assert!((i32::from(pred[15]) - 130).abs() <= 2, "{}", pred[15]);
        assert!((i32::from(pred[0]) - 100).abs() <= 2, "{}", pred[0]);
    }

    #[test]
    fn chroma_dc_prefers_the_nearer_edge_per_block() {
        let e = Edges::<8> {
            top: Some([40; 8]),
            left: Some([200; 8]),
            corner: Some(0),
        };
        let pred = chroma8(CHROMA_DC, &e).unwrap();
        assert_eq!(pred[0], 120); // both edges
        assert_eq!(pred[7], 40); // top-right: the top
        assert_eq!(pred[56], 200); // bottom-left: the left
        assert_eq!(pred[63], 120); // both again
    }
}
