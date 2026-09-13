//! RGBA to YUV 4:2:0, BT.709, limited range.
//!
//! BT.709 because a screen is an sRGB display and sRGB shares its primaries;
//! limited range because that is what players assume when nothing says
//! otherwise, and the stream says so anyway. The samples are the gamma-encoded
//! values as they are, which is what every video pipeline does.
//!
//! Fixed point, 16 bits of fraction, the coefficients rounded so grey stays
//! exactly grey. Chroma is the average of each 2×2 block.

/// `219/255 × (Kr, 1 − Kr − Kb, Kb)`, scaled by 2^16.
const Y: [i32; 3] = [11966, 40254, 4064];
/// `224/255 × (−Kr/(2(1−Kb)), −(1−Kr−Kb)/(2(1−Kb)), 1/2)`, scaled by 2^16.
const CB: [i32; 3] = [-6597, -22187, 28784];
/// `224/255 × (1/2, −(1−Kr−Kb)/(2(1−Kr)), −Kb/(2(1−Kr)))`, scaled by 2^16.
const CR: [i32; 3] = [28784, -26148, -2636];

/// A YUV 4:2:0 picture with even dimensions.
#[derive(Debug)]
pub(crate) struct I420 {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) y: Vec<u8>,
    pub(crate) cb: Vec<u8>,
    pub(crate) cr: Vec<u8>,
}

impl I420 {
    /// A picture for frames of `width` × `height`, rounded up to even: the
    /// last row or column is repeated rather than cut off.
    pub(crate) fn new(width: usize, height: usize) -> Self {
        let (w, h) = (width.next_multiple_of(2), height.next_multiple_of(2));
        Self {
            width: w,
            height: h,
            y: vec![0; w * h],
            cb: vec![0; w / 2 * h / 2],
            cr: vec![0; w / 2 * h / 2],
        }
    }

    /// Convert `rgba`, `width` × `height`, into this picture.
    pub(crate) fn fill(&mut self, rgba: &[u8], width: usize, height: usize) {
        debug_assert_eq!(rgba.len(), width * height * 4);
        let pixel = |x: usize, y: usize| {
            let at = (y.min(height - 1) * width + x.min(width - 1)) * 4;
            [
                i32::from(rgba[at]),
                i32::from(rgba[at + 1]),
                i32::from(rgba[at + 2]),
            ]
        };
        let dot = |k: &[i32; 3], p: [i32; 3]| k[0] * p[0] + k[1] * p[1] + k[2] * p[2];

        for row in 0..self.height {
            for col in 0..self.width {
                let luma = (dot(&Y, pixel(col, row)) + 32_768) >> 16;
                self.y[row * self.width + col] = (16 + luma) as u8;
            }
        }
        let cw = self.width / 2;
        for row in 0..self.height / 2 {
            for col in 0..cw {
                let (mut cb, mut cr) = (0, 0);
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let p = pixel(col * 2 + dx, row * 2 + dy);
                    cb += dot(&CB, p);
                    cr += dot(&CR, p);
                }
                // Four samples and 16 bits of fraction: shift by 18.
                self.cb[row * cw + col] = (128 + ((cb + 131_072) >> 18)) as u8;
                self.cr[row * cw + col] = (128 + ((cr + 131_072) >> 18)) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
        let mut picture = I420::new(2, 2);
        picture.fill(&[r, g, b, 255].repeat(4), 2, 2);
        (picture.y[0], picture.cb[0], picture.cr[0])
    }

    #[test]
    fn the_extremes_land_on_the_limited_range() {
        assert_eq!(solid(255, 255, 255), (235, 128, 128));
        assert_eq!(solid(0, 0, 0), (16, 128, 128));
        assert_eq!(solid(128, 128, 128), (126, 128, 128));
    }

    #[test]
    fn primaries_match_bt709() {
        // Red: Y = 16 + 219 × 0.2126, Cb = 128 − 224 × 0.1146, Cr = 128 + 112.
        assert_eq!(solid(255, 0, 0), (63, 102, 240));
        // Blue: Cb at the top of its range.
        assert_eq!(solid(0, 0, 255).1, 240);
    }

    #[test]
    fn odd_sizes_round_up_by_repeating_the_edge() {
        let mut picture = I420::new(3, 1);
        assert_eq!((picture.width, picture.height), (4, 2));
        let rgba = [[0, 0, 0, 255], [0, 0, 0, 255], [255, 255, 255, 255]].concat();
        picture.fill(&rgba, 3, 1);
        assert_eq!(picture.y, [16, 16, 235, 235, 16, 16, 235, 235]);
    }
}
