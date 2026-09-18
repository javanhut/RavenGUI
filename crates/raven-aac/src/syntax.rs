//! The bitstream: `raw_data_block()` and what it holds (ISO/IEC 14496-3,
//! 4.4.2).
//!
//! A mono frame is one single channel element; a stereo one is one channel
//! pair element with a common window, so that both channels share window
//! sequence, grouping and bands and mid/side can be used between them. Every
//! frame ends with the terminator and is padded to a whole byte.
//!
//! The writers here and the bit counts the rate loop uses (`frame_bits`)
//! describe the same syntax; `lib.rs` checks in debug builds that every
//! frame written is exactly as long as it was counted to be.

use crate::bits::BitWriter;
use crate::huffman::SCALEFACTORS;
use crate::psy::Channel;
use crate::quant::write_spectral;
use crate::{Layout, Sequence};

const ID_SCE: u32 = 0;
const ID_CPE: u32 = 1;
const ID_END: u32 = 7;

/// `ics_info()`'s length, long or short.
fn ics_info_bits(layout: &Layout) -> usize {
    if layout.short { 15 } else { 11 }
}

/// `ms_mask_present`: 0 none, 1 per band, 2 every band.
pub(crate) fn ms_mode(layout: &Layout, ms: &[bool], max_sfb: usize) -> u32 {
    let used = (0..layout.groups)
        .flat_map(|g| (0..max_sfb).map(move |sfb| g * layout.swb + sfb))
        .map(|b| ms[b]);
    let (mut any, mut all) = (false, true);
    for u in used {
        any |= u;
        all &= u;
    }
    match (any, all) {
        (false, _) => 0,
        (true, true) => 2,
        _ => 1,
    }
}

/// The frame's length in bits, padding included, given each channel's
/// `code` bits (section, scalefactor and spectral data).
pub(crate) fn frame_bits(layout: &Layout, coded: &[usize], ms_mode: u32, max_sfb: usize) -> usize {
    // Per channel: global_gain, then the pulse, TNS and gain control flags.
    let ics: usize = coded.iter().map(|c| 8 + c + 3).sum();
    let bits = if coded.len() == 1 {
        3 + 4 + ics_info_bits(layout) + ics + 3
    } else {
        let mask = match ms_mode {
            1 => layout.groups * max_sfb,
            _ => 0,
        };
        3 + 4 + 1 + ics_info_bits(layout) + 2 + mask + ics + 3
    };
    bits.div_ceil(8) * 8
}

fn ics_info(w: &mut BitWriter, layout: &Layout, max_sfb: usize) {
    w.put(1, 0); // ics_reserved_bit
    w.put(2, layout.sequence as u32);
    w.put(1, 0); // window_shape: sine
    if layout.sequence == Sequence::EightShort {
        w.put(4, max_sfb as u32);
        // scale_factor_grouping: a one where a window joins the one before.
        for g in 0..layout.groups {
            for i in 0..layout.group_len[g] {
                if g + usize::from(i) > 0 {
                    w.flag(i > 0);
                }
            }
        }
    } else {
        w.put(6, max_sfb as u32);
        w.put(1, 0); // predictor_data_present
    }
}

fn ics(w: &mut BitWriter, ch: &Channel, layout: &Layout, max_sfb: usize, common: bool) {
    w.put(8, ch.global_gain as u32);
    if !common {
        ics_info(w, layout, max_sfb);
    }
    // section_data
    let (len_bits, escape) = if layout.short { (3, 7) } else { (5, 31) };
    for g in 0..layout.groups {
        let books = &ch.book[g * layout.swb..g * layout.swb + max_sfb];
        let mut start = 0;
        while start < max_sfb {
            let n = books[start];
            let end = start + books[start..].iter().take_while(|&&b| b == n).count();
            w.put(4, u32::from(n));
            let mut len = end - start;
            while len >= escape {
                w.put(len_bits, escape as u32);
                len -= escape;
            }
            w.put(len_bits, len as u32);
            start = end;
        }
    }
    // scale_factor_data
    let mut last = ch.global_gain;
    for g in 0..layout.groups {
        for sfb in 0..max_sfb {
            let b = g * layout.swb + sfb;
            if ch.book[b] != 0 {
                let i = (ch.sf[b] - last + 60) as usize;
                w.put(u32::from(SCALEFACTORS.lens[i]), SCALEFACTORS.codes[i]);
                last = ch.sf[b];
            }
        }
    }
    w.put(1, 0); // pulse_data_present
    w.put(1, 0); // tns_data_present
    w.put(1, 0); // gain_control_data_present
    // spectral_data
    for g in 0..layout.groups {
        for sfb in 0..max_sfb {
            let b = g * layout.swb + sfb;
            if ch.book[b] != 0 {
                write_spectral(w, ch.book[b], &ch.q[layout.band(b)]);
            }
        }
    }
}

/// One `raw_data_block()`: a single channel element, or a channel pair
/// element with a common window.
pub(crate) fn frame(
    channels: &[Channel],
    layout: &Layout,
    ms: &[bool],
    max_sfb: usize,
    bytes: usize,
) -> Vec<u8> {
    let mut w = BitWriter::with_capacity(bytes);
    if let [mono] = channels {
        w.put(3, ID_SCE);
        w.put(4, 0);
        ics(&mut w, mono, layout, max_sfb, false);
    } else {
        w.put(3, ID_CPE);
        w.put(4, 0);
        w.flag(true); // common_window
        ics_info(&mut w, layout, max_sfb);
        let mode = ms_mode(layout, ms, max_sfb);
        w.put(2, mode);
        if mode == 1 {
            for g in 0..layout.groups {
                for sfb in 0..max_sfb {
                    w.flag(ms[g * layout.swb + sfb]);
                }
            }
        }
        for ch in channels {
            ics(&mut w, ch, layout, max_sfb, true);
        }
    }
    w.put(3, ID_END);
    w.finish()
}
