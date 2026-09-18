//! Quantisation, and choosing the Huffman books that code it cheapest.
//!
//! Each band is quantised with its own step, `2^((sf - 100) / 4)`, through a
//! ¾ power: `q = ⌊(|x| / step)^¾ + 0.4054⌋`. The power compresses loud lines,
//! so their error grows more slowly than they do, and the 0.4054 (rather than
//! a half) rounds so as to minimise the error after the decoder's ⁴⁄₃ power
//! undoes it.
//!
//! The quantised bands are then split into sections, each coded with one of
//! eleven spectral books: small books for bands whose values are all ±1,
//! bigger ones for louder bands, book 11 with its escape codes for anything.
//! A section costs its header, so the split is a trade — a band that would be
//! cheapest in book 3 may be better folded into its neighbours' book 5. The
//! split here is found by dynamic programming over the bands, which is exact:
//! no split of this frame costs fewer bits.

use std::ops::Range;
use std::sync::OnceLock;

use crate::Layout;
use crate::huffman::{Book, SCALEFACTORS, SPECTRAL};
use crate::psy::Channel;

/// The largest value a spectral line can take.
pub(crate) const MAX_QUANT: i32 = 8191;

/// Adjacent coded scalefactors may differ by at most this much.
const MAX_SF_STEP: i32 = 60;

/// `2^(-3(sf - 100)/16)`: the reciprocal step, to the ¾ power, per `sf`.
fn inverse_steps() -> &'static [f32; 256] {
    static TABLE: OnceLock<[f32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0.0; 256];
        for (sf, value) in table.iter_mut().enumerate() {
            *value = (-3.0 * (sf as f64 - 100.0) / 16.0).exp2() as f32;
        }
        table
    })
}

/// Quantise band `range` at `sf`. Returns whether any value is nonzero.
fn quantise_band(ch: &mut Channel, range: Range<usize>, sf: i32) -> bool {
    let step = inverse_steps()[sf as usize];
    let mut any = 0i32;
    for ((q, &p), &x) in ch.q[range.clone()]
        .iter_mut()
        .zip(&ch.pow[range.clone()])
        .zip(&ch.spec[range])
    {
        let v = ((p * step + 0.4054) as i32).min(MAX_QUANT);
        any |= v;
        *q = if x < 0.0 { -v } else { v } as i16;
    }
    any != 0
}

/// Quantise every band with the scalefactors the mask asks for, raised by
/// `offset`. Sets `q`, `sf`, `nonzero` and `max_sfb`.
pub(crate) fn quantise(ch: &mut Channel, layout: &Layout, offset: i32) {
    let bands = layout.bands();
    let mut loudest = i32::MIN;
    for b in 0..bands {
        let range = layout.band(b);
        let off = offset as f32;
        if layout.sfb(b) >= layout.coded || off >= ch.zero_from[b] {
            ch.q[range].fill(0);
            ch.nonzero[b] = false;
            continue;
        }
        let sf = ((ch.base[b] + off).round() as i32)
            .max(ch.sf_min[b])
            .clamp(0, 255);
        ch.sf[b] = sf;
        ch.nonzero[b] = quantise_band(ch, range, sf);
        if ch.nonzero[b] {
            loudest = loudest.max(sf);
        }
    }
    // Scalefactors are coded as differences of at most 60. Keeping every
    // coded one within 60 of the largest guarantees it; the bands raised are
    // 90 dB below the loudest step, where nothing is heard.
    let floor = loudest.saturating_sub(MAX_SF_STEP);
    for b in 0..bands {
        if ch.nonzero[b] && ch.sf[b] < floor {
            ch.sf[b] = floor;
            ch.nonzero[b] = quantise_band(ch, layout.band(b), floor);
        }
    }
    ch.max_sfb = 0;
    for b in 0..bands {
        if ch.nonzero[b] {
            ch.max_sfb = ch.max_sfb.max(layout.sfb(b) + 1);
        }
    }
}

/// The widest value each book holds, books 1 to 11 (11 escapes beyond 15).
const BOOK_MAX: [i32; 12] = [0, 1, 1, 2, 2, 4, 4, 7, 7, 12, 12, MAX_QUANT];

/// The books worth trying for a band whose largest value is `peak`: the
/// smallest pair that can hold it, the next pair up, and the escape book.
fn candidates(peak: i32) -> &'static [u8] {
    match peak {
        0 | 1 => &[1, 2, 3, 4],
        2 => &[3, 4, 5, 6],
        3 | 4 => &[5, 6, 7, 8],
        5..=7 => &[7, 8, 9, 10, 11],
        8..=12 => &[9, 10, 11],
        _ => &[11],
    }
}

fn book(n: u8) -> &'static Book {
    &SPECTRAL[usize::from(n) - 1]
}

/// Bits for an escape: `n - 4` ones, a zero, and `n` bits, where `2^n` is
/// the value's top bit.
fn escape_bits(v: u32) -> u32 {
    if v < 16 {
        0
    } else {
        let n = 31 - v.leading_zeros();
        2 * n - 3
    }
}

/// The index of a tuple in its book, as section 4.6.3.3 numbers them.
fn index(n: u8, t: &[i16]) -> usize {
    let a = |i: usize| i32::from(t[i]);
    let u = |i: usize| t[i].unsigned_abs() as usize;
    match n {
        1 | 2 => (27 * (a(0) + 1) + 9 * (a(1) + 1) + 3 * (a(2) + 1) + (a(3) + 1)) as usize,
        3 | 4 => 27 * u(0) + 9 * u(1) + 3 * u(2) + u(3),
        5 | 6 => (9 * (a(0) + 4) + (a(1) + 4)) as usize,
        7 | 8 => 8 * u(0) + u(1),
        9 | 10 => 13 * u(0) + u(1),
        _ => 17 * u(0).min(16) + u(1).min(16),
    }
}

fn tuple_len(n: u8) -> usize {
    if n <= 4 { 4 } else { 2 }
}

fn signed(n: u8) -> bool {
    matches!(n, 1 | 2 | 5 | 6)
}

/// Bits to code `q` with book `n`.
pub(crate) fn spectral_bits(n: u8, q: &[i16]) -> u32 {
    let lens = book(n).lens;
    let mut bits = 0u32;
    for t in q.chunks_exact(tuple_len(n)) {
        bits += u32::from(lens[index(n, t)]);
        if !signed(n) {
            for &v in t {
                bits += u32::from(v != 0);
                if n == 11 {
                    bits += escape_bits(u32::from(v.unsigned_abs()));
                }
            }
        }
    }
    bits
}

/// Write `q` with book `n`: each tuple's code, then the signs of its nonzero
/// values, then (book 11) the escapes.
pub(crate) fn write_spectral(w: &mut crate::bits::BitWriter, n: u8, q: &[i16]) {
    let b = book(n);
    for t in q.chunks_exact(tuple_len(n)) {
        let i = index(n, t);
        w.put(u32::from(b.lens[i]), b.codes[i]);
        if signed(n) {
            continue;
        }
        for &v in t {
            if v != 0 {
                w.flag(v < 0);
            }
        }
        if n == 11 {
            for &v in t {
                let v = u32::from(v.unsigned_abs());
                if v >= 16 {
                    let top = 31 - v.leading_zeros();
                    w.put(top - 4, (1 << (top - 4)) - 1);
                    w.put(1, 0);
                    w.put(top, v - (1 << top));
                }
            }
        }
    }
}

const NO: u32 = u32::MAX / 4;

/// Choose each band's book for bands below `max_sfb`, filling in `book`,
/// the scalefactors of silent bands that end up coded, and `global_gain`.
/// Returns the bits for section, scalefactor and spectral data together.
pub(crate) fn code(ch: &mut Channel, layout: &Layout, max_sfb: usize) -> usize {
    let (len_bits, escape) = if layout.short { (3, 7) } else { (5, 31) };
    let mut total = 0usize;
    let mut cost = [[NO; 12]; 64];
    let mut best = [0u32; 65];
    let mut from = [(0usize, 0u8); 65];
    for g in 0..layout.groups {
        let first = g * layout.swb;
        for (sfb, row) in cost.iter_mut().enumerate().take(max_sfb) {
            let b = first + sfb;
            let range = layout.band(b);
            *row = [NO; 12];
            if ch.nonzero[b] {
                let q = &ch.q[range];
                let peak = q
                    .iter()
                    .map(|v| i32::from(v.unsigned_abs()))
                    .max()
                    .unwrap_or(0);
                for &n in candidates(peak) {
                    if peak <= BOOK_MAX[usize::from(n)] {
                        row[usize::from(n)] = spectral_bits(n, q);
                    }
                }
            } else {
                // Silence costs nothing as book 0 — or, folded into a
                // neighbour's section, its zeros plus a one-bit scalefactor.
                row[0] = 0;
                for n in 1..=11u8 {
                    let zero = if signed(n) { 40 } else { 0 };
                    let tuples = (range.len() / tuple_len(n)) as u32;
                    row[usize::from(n)] = tuples * u32::from(book(n).lens[zero]) + 1;
                }
            }
        }
        // best[i]: the cheapest coding of bands 0..i; from[i]: its last section.
        best[0] = 0;
        for i in 1..=max_sfb {
            best[i] = NO;
            for n in 0..12u8 {
                let mut run = 0u32;
                for j in (0..i).rev() {
                    let c = cost[j][usize::from(n)];
                    if c == NO {
                        break;
                    }
                    run += c;
                    let header = 4 + len_bits * ((i - j) / escape + 1) as u32;
                    let total = best[j] + run + header;
                    if total < best[i] {
                        best[i] = total;
                        from[i] = (j, n);
                    }
                }
            }
        }
        total += best[max_sfb] as usize;
        let mut i = max_sfb;
        while i > 0 {
            let (j, n) = from[i];
            ch.book[first + j..first + i].fill(n);
            i = j;
        }
    }
    // Scalefactors, as differences along the coded bands. A silent band
    // folded into a section repeats the scalefactor before it (one bit).
    let mut global = None;
    for g in 0..layout.groups {
        for sfb in 0..max_sfb {
            let b = g * layout.swb + sfb;
            if ch.book[b] != 0 && ch.nonzero[b] && global.is_none() {
                global = Some(ch.sf[b]);
            }
        }
    }
    ch.global_gain = global.unwrap_or(100);
    let mut last = ch.global_gain;
    for g in 0..layout.groups {
        for sfb in 0..max_sfb {
            let b = g * layout.swb + sfb;
            if ch.book[b] == 0 {
                continue;
            }
            if !ch.nonzero[b] {
                // Its one bit is in the section cost already.
                ch.sf[b] = last;
                continue;
            }
            total += usize::from(SCALEFACTORS.lens[(ch.sf[b] - last + 60) as usize]);
            last = ch.sf[b];
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_count_what_they_write() {
        for v in [16u32, 17, 31, 32, 100, 1000, 8191] {
            let mut w = crate::bits::BitWriter::default();
            write_spectral(&mut w, 11, &[v as i16, 0]);
            let code = u32::from(book(11).lens[17 * 16]);
            assert_eq!(w.bits() as u32, code + 1 + escape_bits(v), "{v}");
        }
    }

    #[test]
    fn counting_matches_writing_in_every_book() {
        let values: Vec<i16> = (0..64).map(|i| ((i * 37) % 25) as i16 - 12).collect();
        for n in 1..=11u8 {
            let q: Vec<i16> = values
                .iter()
                .map(|&v| v.clamp(-BOOK_MAX[n as usize] as i16, BOOK_MAX[n as usize] as i16))
                .collect();
            let mut w = crate::bits::BitWriter::default();
            write_spectral(&mut w, n, &q);
            assert_eq!(w.bits() as u32, spectral_bits(n, &q), "book {n}");
        }
    }
}
