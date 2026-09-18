//! The filterbank: windows and a fast MDCT.
//!
//! AAC turns each block of 2048 samples into 1024 spectral lines with the
//! modified discrete cosine transform, and a block of 256 into 128 for each
//! of the eight short windows. Blocks overlap by half, and the windows are
//! shaped so that the overlapping halves cancel each other's aliasing when a
//! decoder adds them back together (time-domain aliasing cancellation): the
//! transform on its own loses nothing.
//!
//! Written out, the MDCT is a sum over every sample for every line: four
//! million multiplies a long block, per channel. It folds instead into a
//! DCT-IV of half the length, and that into a complex FFT of a quarter, with a
//! twiddle before and after — O(N log N), a few thousand butterflies a block.
//!
//! The scale is the standard's (ISO/IEC 14496-3, 4.6.11): `X[k] = 2 Σ z[n]
//! cos(2π/N (n + n0)(k + ½))`, which with its inverse's `2/N` puts a decoder's
//! output back at the input's level. Samples go in as 16-bit values, so a
//! decoder that reads the stream the standard's way plays back 16-bit values.
//!
//! Only the sine window is used. AAC also allows a Kaiser-Bessel derived one,
//! which rejects distant frequencies better and near ones worse; the sine
//! window's narrower main lobe suits speech and tonal music, and a single
//! shape means the window never has to be switched from frame to frame.

use std::f64::consts::PI;

#[derive(Debug, Clone, Copy, Default)]
struct Complex {
    re: f32,
    im: f32,
}

impl Complex {
    fn mul(self, o: Self) -> Self {
        Self {
            re: self.re * o.re - self.im * o.im,
            im: self.re * o.im + self.im * o.re,
        }
    }
}

/// A radix-2 complex FFT of one power-of-two size.
#[derive(Debug)]
struct Fft {
    /// `e^(-2πik/n)` for `k < n/2`.
    twiddle: Vec<Complex>,
    /// Where each input lands after the bit-reversal permutation.
    reversed: Vec<u32>,
}

impl Fft {
    fn new(n: usize) -> Self {
        debug_assert!(n.is_power_of_two());
        let bits = n.trailing_zeros();
        let twiddle = (0..n / 2)
            .map(|k| {
                let a = -2.0 * PI * k as f64 / n as f64;
                Complex {
                    re: a.cos() as f32,
                    im: a.sin() as f32,
                }
            })
            .collect();
        let reversed = (0..n as u32)
            .map(|i| {
                if bits == 0 {
                    0
                } else {
                    i.reverse_bits() >> (32 - bits)
                }
            })
            .collect();
        Self { twiddle, reversed }
    }

    /// In place, forward (negative exponent), unscaled.
    fn run(&self, data: &mut [Complex]) {
        let n = data.len();
        for (i, &r) in self.reversed.iter().enumerate() {
            let r = r as usize;
            if i < r {
                data.swap(i, r);
            }
        }
        let mut len = 2;
        while len <= n {
            let half = len / 2;
            let step = n / len;
            for chunk in data.chunks_exact_mut(len) {
                let (lo, hi) = chunk.split_at_mut(half);
                for (j, (a, b)) in lo.iter_mut().zip(hi.iter_mut()).enumerate() {
                    let t = b.mul(self.twiddle[j * step]);
                    *b = Complex {
                        re: a.re - t.re,
                        im: a.im - t.im,
                    };
                    *a = Complex {
                        re: a.re + t.re,
                        im: a.im + t.im,
                    };
                }
            }
            len *= 2;
        }
    }
}

/// An MDCT from `2m` windowed samples to `m` lines.
#[derive(Debug)]
pub(crate) struct Mdct {
    m: usize,
    fft: Fft,
    /// `e^(-iπ(n + ¼)/m)`, before the FFT.
    pre: Vec<Complex>,
    /// `2 e^(-iπk/m)`, after it; the 2 is the standard's scale.
    post: Vec<Complex>,
    folded: Vec<f32>,
    buf: Vec<Complex>,
}

impl Mdct {
    pub(crate) fn new(m: usize) -> Self {
        let q = m / 2;
        let angle = |x: f64| {
            let a = -PI * x / m as f64;
            (a.cos(), a.sin())
        };
        let pre = (0..q)
            .map(|n| {
                let (re, im) = angle(n as f64 + 0.25);
                Complex {
                    re: re as f32,
                    im: im as f32,
                }
            })
            .collect();
        let post = (0..q)
            .map(|k| {
                let (re, im) = angle(k as f64);
                Complex {
                    re: (2.0 * re) as f32,
                    im: (2.0 * im) as f32,
                }
            })
            .collect();
        Self {
            m,
            fft: Fft::new(q),
            pre,
            post,
            folded: vec![0.0; m],
            buf: vec![Complex::default(); q],
        }
    }

    /// `input` is `2m` windowed samples; `out` receives `m` lines.
    pub(crate) fn forward(&mut self, input: &[f32], out: &mut [f32]) {
        let m = self.m;
        let h = m / 2;
        debug_assert!(input.len() == 2 * m && out.len() == m);
        // Fold the four quarters (a, b, c, d) into the DCT-IV input
        // (-c_r - d, a - b_r): the aliasing terms the window will cancel.
        for n in 0..h {
            self.folded[n] = -input[3 * h + n] - input[3 * h - 1 - n];
            self.folded[h + n] = input[n] - input[m - 1 - n];
        }
        // The DCT-IV as a complex FFT of half its length.
        for (n, (slot, tw)) in self.buf.iter_mut().zip(&self.pre).enumerate() {
            *slot = Complex {
                re: self.folded[2 * n],
                im: self.folded[m - 1 - 2 * n],
            }
            .mul(*tw);
        }
        self.fft.run(&mut self.buf);
        for (k, (v, tw)) in self.buf.iter().zip(&self.post).enumerate() {
            let y = v.mul(*tw);
            out[2 * k] = y.re;
            out[m - 1 - 2 * k] = -y.im;
        }
    }
}

/// The rising half of a sine window `2 × len` long.
pub(crate) fn sine_half(len: usize) -> Vec<f32> {
    (0..len)
        .map(|n| ((n as f64 + 0.5) * PI / (2 * len) as f64).sin() as f32)
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The standard's MDCT, term by term.
    pub(crate) fn direct(input: &[f32]) -> Vec<f64> {
        let n = input.len();
        let n0 = (n as f64 / 2.0 + 1.0) / 2.0;
        (0..n / 2)
            .map(|k| {
                2.0 * input
                    .iter()
                    .enumerate()
                    .map(|(i, &x)| {
                        f64::from(x)
                            * (2.0 * PI / n as f64 * (i as f64 + n0) * (k as f64 + 0.5)).cos()
                    })
                    .sum::<f64>()
            })
            .collect()
    }

    /// The standard's IMDCT, term by term: `2/N Σ X[k] cos(...)`.
    pub(crate) fn inverse(spec: &[f32]) -> Vec<f64> {
        let n = spec.len() * 2;
        let n0 = (n as f64 / 2.0 + 1.0) / 2.0;
        (0..n)
            .map(|i| {
                2.0 / n as f64
                    * spec
                        .iter()
                        .enumerate()
                        .map(|(k, &x)| {
                            f64::from(x)
                                * (2.0 * PI / n as f64 * (i as f64 + n0) * (k as f64 + 0.5)).cos()
                        })
                        .sum::<f64>()
            })
            .collect()
    }

    fn noise(n: usize, seed: u32) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (s >> 8) as f32 / (1 << 24) as f32 * 2.0 - 1.0
            })
            .collect()
    }

    #[test]
    fn the_fast_transform_is_the_standards() {
        for m in [128, 1024] {
            let input: Vec<f32> = noise(2 * m, 7).iter().map(|x| x * 30_000.0).collect();
            let mut fast = vec![0.0; m];
            Mdct::new(m).forward(&input, &mut fast);
            let slow = direct(&input);
            let peak = slow.iter().fold(0.0f64, |a, b| a.max(b.abs()));
            for (a, b) in fast.iter().zip(&slow) {
                assert!((f64::from(*a) - b).abs() < peak * 1e-5, "{a} vs {b}");
            }
        }
    }

    #[test]
    fn overlapping_blocks_reconstruct_the_input() {
        // Windowed, transformed, inverted, windowed again and overlapped: the
        // middle block comes back exactly.
        let m = 128;
        let x = noise(4 * m, 3);
        let rise = sine_half(m);
        let window = |n: usize| if n < m { rise[n] } else { rise[2 * m - 1 - n] };
        let mut mdct = Mdct::new(m);
        let mut out = vec![0.0f64; 4 * m];
        for block in 0..3 {
            let start = block * m;
            let windowed: Vec<f32> = (0..2 * m).map(|n| x[start + n] * window(n)).collect();
            let mut spec = vec![0.0; m];
            mdct.forward(&windowed, &mut spec);
            for (n, y) in inverse(&spec).into_iter().enumerate() {
                out[start + n] += y * f64::from(window(n));
            }
        }
        for n in m..3 * m {
            assert!((out[n] - f64::from(x[n])).abs() < 1e-4, "{n}");
        }
    }
}
