//! The bit writer every syntax element goes through.

/// Writes bits most significant first, the order AAC's syntax is read in.
#[derive(Debug, Default)]
pub(crate) struct BitWriter {
    out: Vec<u8>,
    /// Bits not yet a whole byte, right-aligned.
    acc: u64,
    len: u32,
}

impl BitWriter {
    pub(crate) fn with_capacity(bytes: usize) -> Self {
        Self {
            out: Vec::with_capacity(bytes),
            ..Self::default()
        }
    }

    /// The low `n` bits of `value`, `n` at most 32.
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

    pub(crate) fn flag(&mut self, on: bool) {
        self.put(1, u32::from(on));
    }

    /// How many bits have been written.
    #[cfg(test)]
    pub(crate) fn bits(&self) -> usize {
        self.out.len() * 8 + self.len as usize
    }

    /// Zero bits up to the next byte boundary, where a raw data block ends.
    pub(crate) fn align(&mut self) {
        if self.len > 0 {
            self.put(8 - self.len, 0);
        }
    }

    /// The bytes written, once aligned.
    pub(crate) fn finish(mut self) -> Vec<u8> {
        self.align();
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_go_most_significant_first() {
        let mut w = BitWriter::default();
        w.put(3, 0b101);
        w.flag(true);
        w.put(12, 0xABC);
        assert_eq!(w.bits(), 16);
        w.put(1, 1);
        assert_eq!(w.finish(), [0b1011_1010, 0xBC, 0x80]);
    }
}
