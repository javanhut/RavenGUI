//! The bit writer every syntax element goes through.

/// Writes bits most significant first, the order H.264 syntax is read in.
#[derive(Debug, Default)]
pub(crate) struct BitWriter {
    out: Vec<u8>,
    /// Bits not yet a whole byte, right-aligned.
    acc: u64,
    len: u32,
}

impl BitWriter {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// `u(n)`: the low `n` bits of `value`, `n` at most 32.
    pub(crate) fn put(&mut self, n: u32, value: u32) {
        debug_assert!(n <= 32);
        if n == 0 {
            return;
        }
        let mask = (1u64 << n) - 1;
        self.acc = (self.acc << n) | (u64::from(value) & mask);
        self.len += n;
        while self.len >= 8 {
            self.len -= 8;
            self.out.push((self.acc >> self.len) as u8);
        }
        self.acc &= (1u64 << self.len) - 1;
    }

    /// `u(1)`.
    pub(crate) fn flag(&mut self, on: bool) {
        self.put(1, u32::from(on));
    }

    /// `ue(v)`: unsigned Exp-Golomb.
    pub(crate) fn ue(&mut self, value: u32) {
        let coded = u64::from(value) + 1;
        let bits = 64 - coded.leading_zeros();
        let zeros = bits - 1;
        // Zeros, then `coded` in `bits` bits; split so no `put` exceeds 32.
        let mut pending = zeros;
        while pending > 0 {
            let n = pending.min(32);
            self.put(n, 0);
            pending -= n;
        }
        if bits > 32 {
            self.put(bits - 32, (coded >> 32) as u32);
            self.put(32, coded as u32);
        } else {
            self.put(bits, coded as u32);
        }
    }

    /// `se(v)`: signed Exp-Golomb.
    pub(crate) fn se(&mut self, value: i32) {
        let mapped = if value > 0 {
            (value as u32) * 2 - 1
        } else {
            value.unsigned_abs() * 2
        };
        self.ue(mapped);
    }

    pub(crate) fn byte_aligned(&self) -> bool {
        self.len == 0
    }

    /// Zero bits up to the next byte boundary: `pcm_alignment_zero_bit`.
    pub(crate) fn align_zero(&mut self) {
        if self.len > 0 {
            self.put(8 - self.len, 0);
        }
    }

    /// `rbsp_trailing_bits()`: a one, then zeros to the byte boundary.
    pub(crate) fn trailing(&mut self) {
        self.put(1, 1);
        self.align_zero();
    }

    /// Every bit written so far, as `0`s and `1`s.
    #[cfg(test)]
    pub(crate) fn bit_string(&self) -> String {
        let mut s: String = self.out.iter().map(|b| format!("{b:08b}")).collect();
        if self.len > 0 {
            s.push_str(&format!("{:0width$b}", self.acc, width = self.len as usize));
        }
        s
    }

    /// The bytes written. The writer must be byte aligned.
    pub(crate) fn finish(self) -> Vec<u8> {
        debug_assert!(self.byte_aligned(), "finish before trailing bits");
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(write: impl FnOnce(&mut BitWriter)) -> String {
        let mut w = BitWriter::new();
        write(&mut w);
        let len = w.len;
        let mut s: String = w.out.iter().map(|b| format!("{b:08b}")).collect();
        if len > 0 {
            s.push_str(&format!("{:0width$b}", w.acc, width = len as usize));
        }
        s
    }

    #[test]
    fn exp_golomb_matches_the_spec_table() {
        assert_eq!(bits(|w| w.ue(0)), "1");
        assert_eq!(bits(|w| w.ue(1)), "010");
        assert_eq!(bits(|w| w.ue(2)), "011");
        assert_eq!(bits(|w| w.ue(3)), "00100");
        assert_eq!(bits(|w| w.ue(8)), "0001001");
        assert_eq!(bits(|w| w.se(1)), "010");
        assert_eq!(bits(|w| w.se(-1)), "011");
        assert_eq!(bits(|w| w.se(2)), "00100");
        assert_eq!(bits(|w| w.se(-2)), "00101");
    }

    #[test]
    fn huge_values_split_across_puts() {
        let s = bits(|w| w.ue(u32::MAX));
        // 2^32 is 33 bits: 32 zeros, then a one and 32 zeros.
        assert_eq!(s.len(), 65);
        assert_eq!(&s[..33], &format!("{}1", "0".repeat(32)));
    }

    #[test]
    fn trailing_bits_align() {
        assert_eq!(
            bits(|w| {
                w.put(3, 0b101);
                w.trailing();
            }),
            "10110000"
        );
    }
}
