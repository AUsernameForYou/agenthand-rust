//! Bit-level reading for HEVC RBSP data (M2).
//!
//! MSB-first bit order, Exp-Golomb ue(v)/se(v), and emulation
//! prevention removal (0x000003 -> 0x0000) per H.265 §7.3 / Annex B.
//! Written from the spec; no external codec code.

/// Remove emulation-prevention bytes: every 0x03 that follows two
/// zero bytes is dropped. Returns the RBSP bytes.
pub fn remove_emulation_prevention(nal: &[u8]) -> Vec<u8> {
    map_rbsp(nal).0
}

/// RBSP conversion with full provenance: returns the RBSP bytes, the
/// raw NAL index of every RBSP byte, and the raw NAL indices of the
/// dropped emulation-prevention bytes. Byte accounting that must
/// translate between the raw byte stream and the RBSP (entry-point
/// offsets count raw bytes — see `syntax`) needs the map; plain
/// parsing only needs `remove_emulation_prevention`.
pub fn map_rbsp(nal: &[u8]) -> (Vec<u8>, Vec<usize>, Vec<usize>) {
    let mut out = Vec::with_capacity(nal.len());
    let mut raw_of_rbsp = Vec::with_capacity(nal.len());
    let mut drops = Vec::new();
    let mut zeros = 0usize;
    for (ri, &b) in nal.iter().enumerate() {
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            drops.push(ri);
            continue;
        }
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        raw_of_rbsp.push(ri);
        out.push(b);
    }
    (out, raw_of_rbsp, drops)
}

#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// absolute bit position
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    pub fn bits_read(&self) -> usize {
        self.pos
    }

    pub fn bits_remaining(&self) -> usize {
        self.data.len() * 8usize.saturating_sub(self.pos)
    }

    pub fn byte_aligned(&self) -> bool {
        self.pos.is_multiple_of(8)
    }

    /// Read one bit; past-the-end reads return 0 (callers that care
    /// check bits_remaining).
    pub fn u1(&mut self) -> u32 {
        let byte = self.data.get(self.pos / 8).copied().unwrap_or(0);
        let bit = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        u32::from(bit)
    }

    /// Read n bits (n <= 32), MSB first.
    pub fn u(&mut self, n: u32) -> u32 {
        let mut v: u32 = 0;
        for _ in 0..n {
            v = (v << 1) | self.u1();
        }
        v
    }

    pub fn u64_bits(&mut self, n: u32) -> u64 {
        let mut v: u64 = 0;
        for _ in 0..n {
            v = (v << 1) | u64::from(self.u1());
        }
        v
    }

    /// Unsigned Exp-Golomb ue(v).
    pub fn ue(&mut self) -> u32 {
        let mut leading_zeros = 0u32;
        loop {
            let b = self.u1();
            if b == 1 || leading_zeros >= 31 {
                break;
            }
            leading_zeros += 1;
        }
        if leading_zeros == 0 {
            return 0;
        }
        let suffix = self.u(leading_zeros);
        // No overflow is possible: the loop caps leading_zeros at
        // 31, so the largest value is (2^31 - 1) + (2^31 - 1) =
        // 2^32 - 2, which fits u32. (Verified 2026-10-31 against
        // the Gauntlet GPT lane's checked_shl/overflow flag; the
        // boundary value is pinned by a unit test.)
        (1u32 << leading_zeros) - 1 + suffix
    }

    /// Signed Exp-Golomb se(v).
    pub fn se(&mut self) -> i32 {
        // Compute on the exact code number in i64: the old
        // `ue() as i32` wrapped code numbers above i32::MAX to
        // negative (flipping their parity, hence the decoded
        // value) and `(code_num + 1)` panicked on overflow at
        // code_num == i32::MAX — a panic a crafted SPS could
        // reach through any se(v) field (found by the A6 mutation
        // harness, 2026-10-31). The exact result always fits i32:
        // |se| <= (2^32 - 1) / 2.
        let code_num = i64::from(self.ue());
        let v = if code_num % 2 == 0 {
            -(code_num / 2)
        } else {
            (code_num + 1) / 2
        };
        v as i32
    }

    /// Skip to the next byte boundary (alignment bits are not
    /// validated here beyond position; callers assert as needed).
    pub fn align_to_byte(&mut self) {
        let rem = self.pos % 8;
        if rem != 0 {
            self.pos += 8 - rem;
        }
    }

    /// Peek: are all remaining bits zero after the first set bit
    /// pattern of rbsp trailing bits? Used for end-of-data checks on
    /// non-CABAC structures.
    pub fn more_rbsp_data(&self) -> bool {
        // More data exists if any set bit appears after the last
        // set bit position that would be the trailing stop bit.
        let total = self.data.len() * 8;
        if self.pos >= total {
            return false;
        }
        // Find the last set bit in the buffer.
        let mut last_set: Option<usize> = None;
        for i in (self.pos..total).rev() {
            let byte = self.data[i / 8];
            if (byte >> (7 - (i % 8))) & 1 == 1 {
                last_set = Some(i);
                break;
            }
        }
        match last_set {
            None => false,
            Some(ls) => ls > self.pos,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ue_se_roundtrip_values() {
        // bit pattern: 1 | 010 | 011 | 00100 -> ue 0, 1, 2, 3
        let bytes = [0b1010_0110, 0b0100_0000];
        let mut br = BitReader::new(&bytes);
        assert_eq!(br.ue(), 0);
        assert_eq!(br.ue(), 1);
        assert_eq!(br.ue(), 2);
        assert_eq!(br.ue(), 3);
        // se: codeNum 0->0, 1->1, 2->-1, 3->2
        let bytes2 = [0b1010_0110, 0b0100_0000];
        let mut br2 = BitReader::new(&bytes2);
        assert_eq!(br2.se(), 0);
        assert_eq!(br2.se(), 1);
        assert_eq!(br2.se(), -1);
        assert_eq!(br2.se(), 2);
    }

    #[test]
    fn emulation_prevention_removed() {
        let nal = [
            0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x03, 0x00,
        ];
        let out = remove_emulation_prevention(&nal);
        assert_eq!(
            out,
            vec![0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00]
        );
    }

    #[test]
    fn fixed_width_reads() {
        let bytes = [0xAB, 0xC0];
        let mut br = BitReader::new(&bytes);
        assert_eq!(br.u(4), 0xA);
        assert_eq!(br.u(4), 0xB);
        assert_eq!(br.u(2), 0b11);
        assert_eq!(br.bits_read(), 10);
    }

    /// A crafted ue(v) at the reader's boundary: 31 leading zeros
    /// and an all-ones suffix names (2^31 - 1) + (2^31 - 1) =
    /// 2^32 - 2. That fits u32 (the leading-zero cap at 31 is what
    /// keeps it in range), so the reader must return the exact
    /// value — no panic, no wrap, no saturation.
    #[test]
    fn ue_at_the_u32_boundary_is_exact() {
        // Bit layout: positions 0..30 zero (the 31 leading zeros),
        // position 31 = 1 (stop bit), positions 32..62 all ones
        // (the 31-bit suffix).
        let mut bits = vec![0u8; 63];
        bits[31] = 1;
        for b in bits.iter_mut().skip(32) {
            *b = 1;
        }
        let mut packed = vec![0u8; 8];
        for (i, &b) in bits.iter().enumerate() {
            if b == 1 {
                packed[i / 8] |= 1 << (7 - (i % 8));
            }
        }
        let mut br = BitReader::new(&packed);
        assert_eq!(br.ue(), u32::MAX - 1);
    }

    /// Ordinary ue(v) values are unaffected.
    #[test]
    fn ue_small_values_unchanged() {
        // "1" -> 0, "010" -> 1, "011" -> 2, "00100" -> 3.
        let bytes = [0b1010_0110, 0b0100_0000];
        let mut br = BitReader::new(&bytes);
        assert_eq!(br.ue(), 0);
        assert_eq!(br.ue(), 1);
        assert_eq!(br.ue(), 2);
        assert_eq!(br.ue(), 3);
    }

    /// se(v) at the code-number extremes (A6 mutation-harness
    /// find): code_num == i32::MAX used to panic on
    /// `(code_num + 1)` overflow, and code numbers above i32::MAX
    /// wrapped through `as i32`, flipping parity and value. Both
    /// must now decode to the exact mathematical result.
    #[test]
    fn se_at_code_number_extremes_is_exact() {
        fn pack(bits: &[u8]) -> Vec<u8> {
            let mut packed = vec![0u8; bits.len().div_ceil(8)];
            for (i, &b) in bits.iter().enumerate() {
                if b == 1 {
                    packed[i / 8] |= 1 << (7 - (i % 8));
                }
            }
            packed
        }
        // code_num = 2^31 - 1 (i32::MAX): 31 leading zeros, stop,
        // suffix 0 (31 zero bits). se = (2^31 - 1 + 1) / 2 = 2^30.
        let mut bits = vec![0u8; 63];
        bits[31] = 1;
        let packed = pack(&bits);
        let mut br = BitReader::new(&packed);
        assert_eq!(br.se(), 1 << 30);
        // code_num = 2^32 - 2 (ue maximum): suffix all ones.
        // se = -(2^32 - 2) / 2 = -(2^31 - 1).
        let mut bits = vec![0u8; 63];
        bits[31] = 1;
        for b in bits.iter_mut().skip(32) {
            *b = 1;
        }
        let packed = pack(&bits);
        let mut br = BitReader::new(&packed);
        assert_eq!(br.se(), -(i32::MAX));
    }
}
