//! The fixed numbers of ISO/IEC 14496-3 that depend on the sample rate.
//!
//! AAC codes a spectrum in scalefactor bands: runs of coefficients that share
//! one quantiser step, narrow at low frequencies and wide at high ones, the
//! way the ear's critical bands are. Which runs depends on the sample rate —
//! these are Tables 4.129 to 4.147 — and a decoder reads them from the same
//! tables, so they cannot be anything else.

/// The rates an `AudioSpecificConfig` can name by index, in index order.
pub(crate) const SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

#[rustfmt::skip]
static LONG_96K: [u16; 42] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 64, 72, 80, 88, 96, 108, 120, 132,
    144, 156, 172, 188, 212, 240, 276, 320, 384, 448, 512, 576, 640, 704, 768, 832, 896, 960, 1024,
];

#[rustfmt::skip]
static LONG_64K: [u16; 48] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 64, 72, 80, 88, 100, 112, 124, 140,
    156, 172, 192, 216, 240, 268, 304, 344, 384, 424, 464, 504, 544, 584, 624, 664, 704, 744, 784,
    824, 864, 904, 944, 984, 1024,
];

#[rustfmt::skip]
static LONG_48K: [u16; 50] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 48, 56, 64, 72, 80, 88, 96, 108, 120, 132, 144, 160,
    176, 196, 216, 240, 264, 292, 320, 352, 384, 416, 448, 480, 512, 544, 576, 608, 640, 672, 704,
    736, 768, 800, 832, 864, 896, 928, 1024,
];

#[rustfmt::skip]
static LONG_32K: [u16; 52] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 48, 56, 64, 72, 80, 88, 96, 108, 120, 132, 144, 160,
    176, 196, 216, 240, 264, 292, 320, 352, 384, 416, 448, 480, 512, 544, 576, 608, 640, 672, 704,
    736, 768, 800, 832, 864, 896, 928, 960, 992, 1024,
];

#[rustfmt::skip]
static LONG_24K: [u16; 48] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 52, 60, 68, 76, 84, 92, 100, 108, 116, 124, 136,
    148, 160, 172, 188, 204, 220, 240, 260, 284, 308, 336, 364, 396, 432, 468, 508, 552, 600, 652,
    704, 768, 832, 896, 960, 1024,
];

#[rustfmt::skip]
static LONG_16K: [u16; 44] = [
    0, 8, 16, 24, 32, 40, 48, 56, 64, 72, 80, 88, 100, 112, 124, 136, 148, 160, 172, 184, 196, 212,
    228, 244, 260, 280, 300, 320, 344, 368, 396, 424, 456, 492, 532, 572, 616, 664, 716, 772, 832,
    896, 960, 1024,
];

#[rustfmt::skip]
static LONG_8K: [u16; 41] = [
    0, 12, 24, 36, 48, 60, 72, 84, 96, 108, 120, 132, 144, 156, 172, 188, 204, 220, 236, 252, 268,
    288, 308, 328, 348, 372, 396, 420, 448, 476, 508, 544, 580, 620, 664, 712, 764, 820, 880, 944,
    1024,
];

static SHORT_96K: [u16; 13] = [0, 4, 8, 12, 16, 20, 24, 32, 40, 48, 64, 92, 128];
static SHORT_48K: [u16; 15] = [0, 4, 8, 12, 16, 20, 28, 36, 44, 56, 68, 80, 96, 112, 128];
static SHORT_24K: [u16; 16] = [
    0, 4, 8, 12, 16, 20, 24, 28, 36, 44, 52, 64, 76, 92, 108, 128,
];
static SHORT_16K: [u16; 16] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 40, 48, 60, 72, 88, 108, 128,
];
static SHORT_8K: [u16; 16] = [
    0, 4, 8, 12, 16, 20, 24, 28, 36, 44, 52, 60, 72, 88, 108, 128,
];

/// Band edges for long (1024-line) and short (128-line) transforms at the
/// rate with this `sampling_frequency_index`.
pub(crate) fn bands(rate_index: usize) -> (&'static [u16], &'static [u16]) {
    match rate_index {
        0 | 1 => (&LONG_96K, &SHORT_96K),
        2 => (&LONG_64K, &SHORT_96K),
        3 | 4 => (&LONG_48K, &SHORT_48K),
        5 => (&LONG_32K, &SHORT_48K),
        6 | 7 => (&LONG_24K, &SHORT_24K),
        8..=10 => (&LONG_16K, &SHORT_16K),
        _ => (&LONG_8K, &SHORT_8K),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_table_tiles_its_transform_in_multiples_of_four() {
        for index in 0..SAMPLE_RATES.len() {
            let (long, short) = bands(index);
            for (table, end) in [(long, 1024), (short, 128)] {
                assert_eq!(table[0], 0);
                assert_eq!(*table.last().unwrap(), end);
                assert!(
                    table
                        .windows(2)
                        .all(|w| w[1] > w[0] && (w[1] - w[0]) % 4 == 0)
                );
                // max_sfb is six bits long, four bits short.
                assert!(table.len() - 1 < if end == 1024 { 64 } else { 16 });
            }
        }
    }
}
