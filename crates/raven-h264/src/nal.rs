//! NAL units: the header byte, and emulation prevention.

pub(crate) const SLICE: u8 = 1;
pub(crate) const SLICE_IDR: u8 = 5;
pub(crate) const SPS: u8 = 7;
pub(crate) const PPS: u8 = 8;

/// Wrap an RBSP in a NAL unit.
///
/// Emulation prevention: after two zero bytes, a byte of 3 or less gets a
/// 0x03 in front of it, so the payload can never contain a start code.
pub(crate) fn wrap(ref_idc: u8, kind: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 64 + 1);
    out.push((ref_idc << 5) | kind);
    let mut zeros = 0;
    for &byte in rbsp {
        if zeros >= 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// NAL units as an Annex B byte stream: each behind a four-byte start code.
pub fn annex_b(nals: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_code_emulation_is_escaped() {
        assert_eq!(
            wrap(3, SPS, &[0, 0, 1, 0, 0, 0, 0]),
            [0x67, 0, 0, 3, 1, 0, 0, 3, 0, 0]
        );
        // Four and up need no escape.
        assert_eq!(wrap(0, SLICE, &[0, 0, 4]), [0x01, 0, 0, 4]);
    }
}
