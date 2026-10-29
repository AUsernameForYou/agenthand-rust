//! HEVC CABAC arithmetic decoding engine + context models (H.265 §9.3).
//!
//! Hand-implemented from the standard. The normative tables (rangeTabLPS,
//! context initialization values, transition tables) are transcribed from
//! H.265 Tables 9-4..9-42; they were cross-checked against independent
//! reference transcriptions during development (oracle duty only — no
//! reference code is part of this implementation).
//!
//! Context storage uses the flat layout documented in `CONTEXT_COUNT` order:
//! each context is a (pStateIdx 0..=63, valMps 0/1) pair packed in one byte
//! as `state | (valMps << 6)`.

/// Number of CABAC contexts in the flat store (matches the §9.2 init tables).
pub const CONTEXT_COUNT: usize = 183;

// Flat context base offsets (number of contexts in parentheses).
pub const CTX_SAO_MERGE: usize = 0; // (1)
pub const CTX_SAO_TYPE: usize = 1; // (1)
pub const CTX_SPLIT_CU: usize = 2; // (3)
pub const CTX_TRANSQUANT_BYPASS: usize = 5; // (1)
pub const CTX_SKIP: usize = 6; // (3) inter only
pub const CTX_CU_QP_DELTA: usize = 9; // (3, first 2 used)
pub const CTX_PRED_MODE: usize = 12; // (1) inter only
pub const CTX_PART_MODE: usize = 13; // (4)
pub const CTX_PREV_INTRA_LUMA: usize = 17; // (1)
pub const CTX_INTRA_CHROMA: usize = 18; // (2, second is CNU padding in spec table)
pub const CTX_MERGE_FLAG: usize = 20; // inter only below
pub const CTX_MERGE_IDX: usize = 21;
pub const CTX_INTER_PRED_IDC: usize = 22; // (5)
pub const CTX_REF_IDX_L0: usize = 27; // (2)
pub const CTX_REF_IDX_L1: usize = 29; // (2)
pub const CTX_ABS_MVD_G0: usize = 31; // (2)
pub const CTX_ABS_MVD_G1: usize = 33; // (2)
pub const CTX_MVP_LX: usize = 35; // (1)
pub const CTX_NO_RESIDUAL: usize = 36; // (1)
pub const CTX_SPLIT_TRANSFORM: usize = 37; // (3)
pub const CTX_CBF_LUMA: usize = 40; // (2)
pub const CTX_CBF_CHROMA: usize = 42; // (5)
pub const CTX_TRANSFORM_SKIP: usize = 47; // (2)
pub const CTX_EXPLICIT_RDPCM_FLAG: usize = 49; // (2)
pub const CTX_EXPLICIT_RDPCM_DIR: usize = 51; // (2)
pub const CTX_LAST_X: usize = 53; // (18)
pub const CTX_LAST_Y: usize = 71; // (18)
pub const CTX_SIG_GROUP: usize = 89; // (4)
pub const CTX_SIG_COEFF: usize = 93; // (44)
pub const CTX_GREATER1: usize = 137; // (24)
pub const CTX_GREATER2: usize = 161; // (6)
pub const CTX_LOG2_RES_SCALE: usize = 167; // (8)
pub const CTX_RES_SCALE_SIGN: usize = 175; // (2)
pub const CTX_CHROMA_QP_OFFSET_FLAG: usize = 177; // (1)
pub const CTX_CHROMA_QP_OFFSET_IDX: usize = 178; // (1)
                                                 // 179..183: unused tail in the reference layout (kept for count parity).

/// initValue sentinel in the standard's tables: context not used (CNU).
const CNU: u16 = 154;

/// Slice-type-I (initType 0) initialization values, §9.2 Tables 9-4..9-31,
/// in the flat layout order above. Inter-only contexts carry their I-table
/// values too (never read on the intra path).
const INIT_I: [u16; CONTEXT_COUNT] = [
    // sao_merge (1)
    153, // sao_type_idx (1)
    200, // split_cu (3)
    139, 141, 157, // cu_transquant_bypass (1)
    CNU, // skip (3)
    CNU, CNU, CNU, // cu_qp_delta (3)
    CNU, CNU, CNU, // pred_mode (1)
    CNU, // part_mode (4)
    184, CNU, CNU, CNU, // prev_intra_luma_pred_flag (1)
    184, // intra_chroma_pred_mode (2)
    63, 139, // merge_flag (1)
    CNU, // merge_idx (1)
    CNU, // inter_pred_idc (5)
    CNU, CNU, CNU, CNU, CNU, // ref_idx_l0 (2)
    CNU, CNU, // ref_idx_l1 (2)
    CNU, CNU, // abs_mvd_greater0 (2)
    CNU, CNU, // abs_mvd_greater1 (2)
    CNU, CNU, // mvp_lx_flag (1)
    CNU, // no_residual_data (1)
    CNU, // split_transform (3)
    153, 138, 138, // cbf_luma (2)
    111, 141, // cbf_cb/cr (5)
    94, 138, 182, CNU, CNU, // transform_skip (2)
    139, 139, // explicit_rdpcm_flag (2)
    CNU, CNU, // explicit_rdpcm_dir (2)
    CNU, CNU, // last_significant_coeff_x_prefix (18)
    110, 110, 124, 125, 140, 153, 125, 127, 140, 109, 111, 143, 127, 111, 79, 108, 123, 63,
    // last_significant_coeff_y_prefix (18)
    110, 110, 124, 125, 140, 153, 125, 127, 140, 109, 111, 143, 127, 111, 79, 108, 123, 63,
    // significant_coeff_group_flag (4)
    91, 171, 134, 141, // significant_coeff_flag (44)
    111, 111, 125, 110, 110, 94, 124, 108, 124, 107, 125, 141, 179, 153, 125, 107, 125, 141, 179,
    153, 125, 107, 125, 141, 179, 153, 125, 140, 139, 182, 182, 152, 136, 152, 136, 153, 136, 139,
    111, 136, 139, 111, 141, 111, // coeff_abs_level_greater1_flag (24)
    140, 92, 137, 138, 140, 152, 138, 139, 153, 74, 149, 92, 139, 107, 122, 152, 140, 179, 166,
    182, 140, 227, 122, 197, // coeff_abs_level_greater2_flag (6)
    138, 153, 136, 167, 152, 152, // log2_res_scale_abs (8)
    CNU, CNU, CNU, CNU, CNU, CNU, CNU, CNU, // res_scale_sign_flag (2)
    CNU, CNU, // cu_chroma_qp_offset_flag (1)
    CNU, // cu_chroma_qp_offset_idx (1)
    CNU, // tail padding to CONTEXT_COUNT
    CNU, CNU, CNU, CNU,
];

/// ivlLpsRange table, H.265 Table 9-42: [pStateIdx][qRangeIdx], values >> 2
/// are NOT pre-shifted here; the table holds the raw spec values.
pub const RANGE_TAB_LPS: [[u8; 4]; 64] = [
    [128, 176, 208, 240],
    [128, 167, 197, 227],
    [128, 158, 187, 216],
    [123, 150, 178, 205],
    [116, 142, 169, 195],
    [111, 135, 160, 185],
    [105, 128, 152, 175],
    [100, 122, 144, 166],
    [95, 116, 137, 158],
    [90, 110, 130, 150],
    [85, 104, 123, 142],
    [81, 99, 117, 135],
    [77, 94, 111, 128],
    [73, 89, 105, 122],
    [69, 85, 100, 116],
    [66, 80, 95, 110],
    [62, 76, 90, 104],
    [59, 72, 86, 99],
    [56, 69, 81, 94],
    [53, 65, 77, 89],
    [51, 62, 73, 85],
    [48, 59, 69, 80],
    [46, 56, 66, 76],
    [43, 53, 63, 72],
    [41, 50, 59, 69],
    [39, 48, 56, 65],
    [37, 45, 54, 62],
    [35, 43, 51, 59],
    [33, 41, 48, 56],
    [32, 39, 46, 53],
    [30, 37, 43, 50],
    [29, 35, 41, 48],
    [27, 33, 39, 45],
    [26, 31, 37, 43],
    [24, 30, 35, 41],
    [23, 28, 33, 39],
    [22, 27, 32, 37],
    [21, 26, 30, 35],
    [20, 24, 29, 33],
    [19, 23, 27, 31],
    [18, 22, 26, 30],
    [17, 21, 25, 28],
    [16, 20, 23, 27],
    [15, 19, 22, 25],
    [14, 18, 21, 24],
    [14, 17, 20, 23],
    [13, 16, 19, 22],
    [12, 15, 18, 21],
    [12, 14, 17, 20],
    [11, 14, 16, 19],
    [11, 13, 15, 18],
    [10, 12, 15, 17],
    [10, 12, 14, 16],
    [9, 11, 13, 15],
    [9, 11, 12, 14],
    [8, 10, 12, 14],
    [8, 9, 11, 13],
    [7, 9, 11, 12],
    [7, 9, 10, 12],
    [7, 8, 10, 11],
    [6, 8, 9, 11],
    [6, 7, 9, 10],
    [6, 7, 8, 9],
    [2, 2, 2, 2],
];

/// State transition on an MPS bin (spec Table 9-43 transIdxMps).
pub const TRANS_IDX_MPS: [u8; 64] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50,
    51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 62, 63,
];

/// State transition on an LPS bin (spec Table 9-44 transIdxLps).
pub const TRANS_IDX_LPS: [u8; 64] = [
    0, 0, 1, 2, 2, 4, 4, 5, 6, 7, 8, 9, 9, 11, 11, 12, 13, 13, 15, 15, 16, 16, 18, 18, 19, 19, 21,
    21, 22, 22, 23, 24, 24, 25, 26, 26, 27, 27, 28, 29, 29, 30, 30, 30, 31, 32, 32, 33, 33, 33, 34,
    34, 35, 35, 35, 36, 36, 36, 37, 37, 37, 38, 38, 63,
];

#[derive(Debug)]
pub enum CabacError {
    /// Ran past the end of the (sub)stream while renormalizing.
    OutOfData,
    /// A decoded value violated a spec bound (desync evidence).
    InvalidValue(&'static str),
}

impl std::fmt::Display for CabacError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CabacError::OutOfData => write!(f, "CABAC ran out of input bytes"),
            CabacError::InvalidValue(what) => write!(f, "CABAC invalid value: {what}"),
        }
    }
}

impl std::error::Error for CabacError {}

pub type CResult<T> = Result<T, CabacError>;

/// AGENTHAND_CABAC_BINS gate, cached (it is consulted per bin).
pub fn bins_dump_enabled() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::env::var_os("AGENTHAND_CABAC_BINS").is_some())
}

/// AGENTHAND_CABAC_CHAIN gate, cached.
pub fn chain_dump_enabled() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::env::var_os("AGENTHAND_CABAC_CHAIN").is_some())
}

/// AGENTHAND_CHAIN_SUB: restrict the chain dump to one substream
/// index (None = all substreams).
pub fn chain_dump_sub() -> Option<usize> {
    static F: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *F.get_or_init(|| {
        std::env::var("AGENTHAND_CHAIN_SUB")
            .ok()
            .and_then(|v| v.parse().ok())
    })
}

/// One context model: packed (pStateIdx | valMps << 6).
#[derive(Clone)]
pub struct Contexts {
    pub state: [u8; CONTEXT_COUNT],
}

impl Default for Contexts {
    fn default() -> Self {
        Contexts {
            state: [0; CONTEXT_COUNT],
        }
    }
}

impl Contexts {
    /// §9.2.1 initialization for slice QP and init type (0 = I, 1 = P, 2 = B).
    /// This build only decodes I slices; P/B tables are not carried.
    /// Note: the value 154 in the tables is a genuine initValue (it is
    /// also the "not used" marker for contexts a slice type never
    /// touches, but the initialization formula applies to it uniformly
    /// — e.g. cu_qp_delta_abs initializes from 154 in I slices).
    pub fn init_for_slice(&mut self, slice_qp_y: i32) {
        let qp = slice_qp_y.clamp(0, 51);
        for (i, &init_value) in INIT_I.iter().enumerate() {
            let iv = init_value as i32;
            let m = ((iv >> 4) * 5) - 45;
            let n = ((iv & 15) << 3) - 16;
            let pre = ((m * qp) >> 4) + n;
            let pre = pre.clamp(1, 126);
            let (p_state, val_mps) = if pre <= 63 {
                (63 - pre, 0u8)
            } else {
                (pre - 64, 1u8)
            };
            self.state[i] = (p_state as u8) | (val_mps << 6);
        }
    }

    #[inline]
    pub fn get(&self, idx: usize) -> (u8, u8) {
        let v = self.state[idx];
        (v & 0x3F, v >> 6)
    }

    #[inline]
    pub fn set(&mut self, idx: usize, p_state: u8, val_mps: u8) {
        self.state[idx] = (p_state & 0x3F) | (val_mps << 6);
    }
}

/// CABAC decoder: engine + contexts + bit cursor within the current byte.
///
/// The engine's offset refill in the standard reads whole bytes once the
/// initial 9 bits are taken; this implementation reads bit by bit, which is
/// arithmetically identical (the byte stream is consumed MSB-first either
/// way) and keeps substream bookkeeping exact.
pub struct Cabac<'a> {
    pub engine_data: &'a [u8],
    pub byte_pos: usize,
    bit_pos: u32,
    pub range: u32,
    pub offset: u32,
    pub ctx: Contexts,
    pub bins: u64,
    /// Bits read past the end of the data (zero-filled).
    pub over_bits: u64,
    /// Emit the per-bin chain dump for this engine (diagnostics).
    pub dump_chain: bool,
}

impl<'a> Cabac<'a> {
    /// Start decoding a substream whose first byte is `data[0]`.
    pub fn new_substream(data: &'a [u8], ctx: Contexts) -> CResult<Self> {
        if data.len() < 2 {
            return Err(CabacError::OutOfData);
        }
        let offset = (((data[0] as u32) << 8) | data[1] as u32) >> 7;
        Ok(Cabac {
            engine_data: data,
            byte_pos: 1,
            bit_pos: 1,
            range: 510,
            offset,
            ctx,
            bins: 0,
            over_bits: 0,
            dump_chain: false,
        })
    }

    /// Absolute byte offset (from the substream start) of the next unread
    /// bit. After the final terminate bin this is where the encoder stopped
    /// (modulo the trailing flush bytes).
    pub fn consumed_bytes(&self) -> usize {
        self.byte_pos + if self.bit_pos > 0 { 1 } else { 0 }
    }

    #[inline]
    fn read_bit(&mut self) -> CResult<u32> {
        if self.byte_pos >= self.engine_data.len() {
            // Zero-fill past the end of the slice data, as production
            // decoders' bit readers do: the final substream's flush
            // region can require window bits beyond the last byte.
            // The position does not advance, so consumed_bytes()
            // clamps at the data length and the termination checks
            // (end bin + per-substream ends) remain the arbiters.
            self.over_bits += 1;
            return Ok(0);
        }
        let bit = ((self.engine_data[self.byte_pos] >> (7 - self.bit_pos)) & 1) as u32;
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
        Ok(bit)
    }

    #[inline]
    fn renorm(&mut self) -> CResult<()> {
        while self.range < 256 {
            self.range <<= 1;
            let bit = self.read_bit()?;
            self.offset = (self.offset << 1) | bit;
        }
        Ok(())
    }

    /// Diagnostic chain dump (AGENTHAND_CABAC_CHAIN): one line per
    /// bin with the engine state BEFORE the bin — kind, range, offset,
    /// and the reader cursor (byte/bit within this substream). Used
    /// for oracle chain comparisons; the walker enables it per
    /// substream (`dump_chain`), inert otherwise.
    #[inline]
    fn chain_dump(&self, kind: char) {
        if self.dump_chain {
            eprintln!(
                "C {} {} {} {} {} {}",
                self.bins, kind, self.range, self.offset, self.byte_pos, self.bit_pos
            );
        }
    }

    /// Regular (context-coded) bin, §9.3.4.3.2.
    #[inline]
    pub fn decode_bin(&mut self, ctx_idx: usize) -> CResult<u32> {
        self.chain_dump('R');
        let (p_state, val_mps) = self.ctx.get(ctx_idx);
        if bins_dump_enabled() {
            eprintln!(
                "RB r={} s={} m={} ctx={} o={}",
                self.range, p_state, val_mps, ctx_idx, self.offset
            );
        }
        let q_range_idx = (self.range >> 6) & 3;
        let ivl_lps_range = RANGE_TAB_LPS[p_state as usize][q_range_idx as usize] as u32;
        self.range -= ivl_lps_range;
        let bin: u32;
        if self.offset >= self.range {
            // LPS path: offset drops by the MPS sub-range, range becomes
            // the LPS range; valMps flips iff the state BEFORE the
            // transition is 0.
            self.offset -= self.range;
            bin = 1 - val_mps as u32;
            let new_val = if p_state == 0 { 1 - val_mps } else { val_mps };
            let new_state = TRANS_IDX_LPS[p_state as usize];
            self.range = ivl_lps_range;
            self.ctx.set(ctx_idx, new_state, new_val);
        } else {
            bin = val_mps as u32;
            let new_state = TRANS_IDX_MPS[p_state as usize];
            self.ctx.set(ctx_idx, new_state, val_mps);
        }
        self.bins += 1;
        self.renorm()?;
        Ok(bin)
    }

    /// Bypass bin, §9.3.4.3.4.
    #[inline]
    pub fn decode_bypass(&mut self) -> CResult<u32> {
        self.chain_dump('B');
        if bins_dump_enabled() {
            eprintln!("BP r={} o={}", self.range, self.offset);
        }
        let bit = self.read_bit()?;
        self.offset = (self.offset << 1) | bit;
        self.bins += 1;
        if self.offset >= self.range {
            self.offset -= self.range;
            Ok(1)
        } else {
            Ok(0)
        }
    }

    /// Terminating bin, §9.3.4.3.3 (end_of_slice_segment / end_of_sub_stream).
    pub fn decode_terminate(&mut self) -> CResult<u32> {
        self.chain_dump('T');
        // §9.3.4.3.5: range -= 2; bin = 1 iff offset >= range. On bin 0,
        // if the reduced range is below 256 it is doubled exactly once
        // (with one refill bit); otherwise it carries on as-is.
        self.range -= 2;
        self.bins += 1;
        if self.offset >= self.range {
            Ok(1)
        } else {
            if self.range < 256 {
                self.range <<= 1;
                self.offset = (self.offset << 1) | self.read_bit()?;
            }
            Ok(0)
        }
    }

    /// Bypass bins as an unsigned value of `n` bits (first bin = MSB).
    pub fn decode_bypass_bits(&mut self, n: u32) -> CResult<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.decode_bypass()?;
        }
        Ok(v)
    }

    /// Truncated unary with bypass bins (sao_offset_abs style).
    pub fn decode_unary_bypass(&mut self, c_max: u32) -> CResult<u32> {
        let mut v = 0u32;
        while v < c_max {
            if self.decode_bypass()? == 0 {
                break;
            }
            v += 1;
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_table_shape() {
        assert_eq!(RANGE_TAB_LPS[0], [128, 176, 208, 240]);
        assert_eq!(RANGE_TAB_LPS[63], [2, 2, 2, 2]);
        // Monotonic decrease down each column.
        for col in 0..4 {
            for row in 1..63 {
                assert!(RANGE_TAB_LPS[row][col] <= RANGE_TAB_LPS[row - 1][col]);
            }
        }
    }

    #[test]
    fn init_formula_endpoints() {
        let mut ctx = Contexts::default();
        ctx.init_for_slice(26);
        // Every context must hold a valid state.
        for i in 0..CONTEXT_COUNT {
            let (p, _v) = ctx.get(i);
            assert!(p <= 63, "ctx {i} state {p}");
        }
        // Hand check: sao_type initValue 200 -> m = (12*5)-45 = 15,
        // n = (8<<3)-16 = 48; pre = ((15*26)>>4)+48 = 24+48 = 72 -> state 8, mps 1.
        let (p, v) = ctx.get(CTX_SAO_TYPE);
        assert_eq!((p, v), (8, 1));
        // split_cu[0] initValue 139 -> m = (8*5)-45 = -5, n = (11<<3)-16 = 72;
        // pre = ((-5*26)>>4)+72 = -9+72 = 63 -> state 0, mps 0.
        let (p, v) = ctx.get(CTX_SPLIT_CU);
        assert_eq!((p, v), (0, 0));
    }

    #[test]
    fn transitions_match_spec_anchors() {
        assert_eq!(TRANS_IDX_MPS[0], 1);
        assert_eq!(TRANS_IDX_MPS[61], 62);
        assert_eq!(TRANS_IDX_MPS[62], 62);
        assert_eq!(TRANS_IDX_MPS[63], 63);
        assert_eq!(TRANS_IDX_LPS[0], 0);
        assert_eq!(TRANS_IDX_LPS[1], 0);
        assert_eq!(TRANS_IDX_LPS[63], 63);
    }

    /// Known-answer decode vector, worked by hand from §9.3.4 for one
    /// context (CTX_SPLIT_CU at QP 26: init state 0, valMps 0) over the
    /// byte stream [0x40, 0x00, 0x00, 0x00]:
    ///   init: range 510, offset = first 9 bits = 0b010000000 = 128.
    ///   A: state0 q3 lps240 range->270; 128<270 MPS -> bin 0; state 1.
    ///   B: state1 q0 lps128 range->142; 128<142 MPS -> bin 0; state 2;
    ///      renorm: offset = 256 + bit9(0) = 256, range 284.
    ///   C: state2 q0 lps128 range->156; 256>=156 LPS -> bin 1;
    ///      offset 100, range 128, state LPS[2]=1; renorm offset 200.
    ///   D: state1 q0 lps128 range->128; 200>=128 LPS -> bin 1;
    ///      offset 72, range 128, state LPS[1]=0 (no flip, old state 1);
    ///      renorm offset 144, range 256.
    ///   E: state0 q0 lps128 range->128; 144>=128 LPS -> bin 1;
    ///      offset 16, range 128, state 0, valMps flips to 1;
    ///      renorm offset 32, range 256.
    ///   F: state0 valMps1 q0 lps128 range->128; 32<128 MPS -> bin 1;
    ///      state MPS[0]=1; renorm offset 64, range 256.
    /// Expected bins: [0, 0, 1, 1, 1, 1]; final context (state 1, mps 1).
    #[test]
    fn hand_computed_decode_vector() {
        let data = [0x40u8, 0x00, 0x00, 0x00];
        let mut ctx = Contexts::default();
        ctx.init_for_slice(26);
        let mut dec = Cabac::new_substream(&data, ctx).expect("substream");
        let want = [0u32, 0, 1, 1, 1, 1];
        for (i, &w) in want.iter().enumerate() {
            let got = dec.decode_bin(CTX_SPLIT_CU).expect("bin");
            assert_eq!(got, w, "bin {i}");
        }
        assert_eq!(dec.ctx.get(CTX_SPLIT_CU), (1, 1));
        assert_eq!(dec.range, 256);
        assert_eq!(dec.offset, 64);
    }
}
