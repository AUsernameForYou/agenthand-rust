//! HEVC slice-data syntax walk for I slices (H.265 §7.3.6–7.3.8, §9.3).
//!
//! This is the M2 verification walk: it decodes every CABAC syntax
//! element of an intra slice in order — SAO, coding quadtree, coding
//! units, transform trees, residual coding — maintaining the context
//! state, the WPP snapshot rule, tile substreams, and entry-point
//! boundaries, but it does NOT reconstruct pixels (that is M3). A walk
//! that consumes the slice exactly and finishes on a terminating bin
//! of 1 at the right byte position is the M2 exit evidence.
//!
//! Element order, context derivations, and binarizations follow the
//! standard; they were cross-checked line-by-line against reference
//! decoder behaviour during development (oracle duty only).
//!
//! Subset refusals (SPEC-SUBSET §5) surface as `SyntaxError::Refusal`:
//! transquant-bypass CUs and PCM CUs.

use sha2::{Digest, Sha256};

use crate::heic::cabac::{
    Cabac, CabacError, Contexts, CTX_CBF_CHROMA, CTX_CBF_LUMA, CTX_CU_QP_DELTA, CTX_GREATER1,
    CTX_GREATER2, CTX_INTRA_CHROMA, CTX_LAST_X, CTX_LAST_Y, CTX_PART_MODE, CTX_PREV_INTRA_LUMA,
    CTX_SAO_MERGE, CTX_SAO_TYPE, CTX_SIG_COEFF, CTX_SIG_GROUP, CTX_SPLIT_CU, CTX_SPLIT_TRANSFORM,
    CTX_TRANSFORM_SKIP, CTX_TRANSQUANT_BYPASS,
};
use crate::heic::params::{Pps, SliceHeader, Sps};

#[derive(Debug)]
pub enum SyntaxError {
    Cabac(CabacError),
    Refusal(String),
    Invalid(String),
}

impl From<CabacError> for SyntaxError {
    fn from(e: CabacError) -> Self {
        SyntaxError::Cabac(e)
    }
}

impl std::fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyntaxError::Cabac(e) => write!(f, "{e}"),
            SyntaxError::Refusal(what) => write!(f, "subset refusal: {what}"),
            SyntaxError::Invalid(what) => write!(f, "invalid stream: {what}"),
        }
    }
}

impl std::error::Error for SyntaxError {}

pub type SResult<T> = Result<T, SyntaxError>;

fn invalid<T>(msg: impl Into<String>) -> SResult<T> {
    Err(SyntaxError::Invalid(msg.into()))
}

#[derive(Debug, Clone, Default)]
pub struct WalkStats {
    pub bins: u64,
    pub ctbs: u32,
    pub cus: u32,
    pub split_cus: u32,
    pub pus: u32,
    pub tus: u32,
    pub tu_luma_residuals: u32,
    pub sig_coeffs: u64,
    pub transform_skip_tus: u32,
    pub sao_ctbs: u32,
    pub substreams: u32,
    /// Terminating bin value of the final end_of_slice_segment_flag.
    pub end_bin: u32,
    /// Byte position in the slice data just after the final terminating
    /// bin's last renormalization.
    pub end_consumed: usize,
    pub slice_data_len: usize,
    /// Per-substream end evidence: (substream index, terminate bin,
    /// consumed bytes at switch time, expected boundary).
    pub substream_ends: Vec<(u32, u32, usize, usize)>,
    /// Rolling trace hash snapshot at each substream end.
    pub trace_at_substream: Vec<String>,
    pub trace_final: String,
}

// ---------------------------------------------------------------------------
// Scan orders (§6.5.3): pattern generators for a k×k grid.

#[derive(Clone, Copy, PartialEq, Debug)]
enum Scan {
    Diag,
    Horiz,
    Vert,
}

/// Positions of a k×k grid in the given scan order.
fn scan_pattern(k: u32, scan: Scan) -> Vec<(u32, u32)> {
    let mut out = Vec::with_capacity((k * k) as usize);
    match scan {
        Scan::Horiz => {
            for y in 0..k {
                for x in 0..k {
                    out.push((x, y));
                }
            }
        }
        Scan::Vert => {
            for x in 0..k {
                for y in 0..k {
                    out.push((x, y));
                }
            }
        }
        Scan::Diag => {
            // §6.5.3 up-right diagonal scan: anti-diagonals in order,
            // x ascending within each diagonal (first element of
            // diagonal d is (0, d) clipped to the block).
            for d in 0..(2 * k - 1) {
                let x_hi = d.min(k - 1);
                let x_lo = d.saturating_sub(k - 1);
                for x in x_lo..=x_hi {
                    out.push((x, d - x));
                }
            }
        }
    }
    out
}

/// Full TU scan order: sub-block (4×4) scan composed with the in-block
/// scan, both in the TU's scan type.
fn tu_scan_order(log2: u32, scan: Scan) -> Vec<(u32, u32)> {
    if log2 == 2 {
        return scan_pattern(4, scan);
    }
    let cg_k = 1u32 << (log2 - 2);
    let cgs = scan_pattern(cg_k, scan);
    let inner = scan_pattern(4, scan);
    let mut out = Vec::with_capacity(1usize << (2 * log2));
    for (cgx, cgy) in cgs {
        for (ix, iy) in &inner {
            out.push((cgx * 4 + ix, cgy * 4 + iy));
        }
    }
    out
}

/// significant_coeff_flag context map (§9.3.3.5), [scan][row][pos]:
/// rows are the 4×4 map then prev_sig 0,1,2 maps then the all-2 row.
const CTX_IDX_MAP: [[[u8; 16]; 5]; 3] = [
    [
        // DIAG
        [0, 2, 1, 6, 3, 4, 7, 6, 4, 5, 7, 8, 5, 8, 8, 8],
        [1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        [2, 1, 2, 0, 1, 2, 0, 0, 1, 2, 0, 0, 1, 0, 0, 0],
        [2, 2, 1, 2, 1, 0, 2, 1, 0, 0, 1, 0, 0, 0, 0, 0],
        [2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2],
    ],
    [
        // HORIZ
        [0, 1, 4, 5, 2, 3, 4, 5, 6, 6, 8, 8, 7, 7, 8, 8],
        [1, 1, 1, 0, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0],
        [2, 2, 2, 2, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0],
        [2, 1, 0, 0, 2, 1, 0, 0, 2, 1, 0, 0, 2, 1, 0, 0],
        [2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2],
    ],
    [
        // VERT
        [0, 2, 6, 7, 1, 3, 6, 7, 4, 4, 8, 8, 5, 5, 8, 8],
        [1, 1, 1, 0, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0],
        [2, 1, 0, 0, 2, 1, 0, 0, 2, 1, 0, 0, 2, 1, 0, 0],
        [2, 2, 2, 2, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0],
        [2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2],
    ],
];

fn scan_index(scan: Scan) -> usize {
    match scan {
        Scan::Diag => 0,
        Scan::Horiz => 1,
        Scan::Vert => 2,
    }
}

// ---------------------------------------------------------------------------

struct Walker<'a> {
    sps: &'a Sps,
    pps: &'a Pps,
    hdr: &'a SliceHeader,
    data: &'a [u8],
    cabac: Cabac<'a>,
    stats: WalkStats,
    hasher: Sha256,
    // Per-picture derivation grids.
    depth_grid: Vec<u8>, // min-CB units, quadtree depth (0 = CTB size)
    min_cb_w: u32,
    ipm_grid: Vec<u8>, // min-PU units, intra luma modes (init DC)
    min_pu_w: u32,
    min_pu_log2: u32,
    // Current CU state.
    intra_split: bool,
    pb_modes: [u8; 4],
    chroma_mode_c: u8,
    max_trafo_depth: u32,
    is_qp_delta_coded: bool,
    // Tile being walked (CTB-unit bounds).
    tile_x0: u32,
    tile_y0: u32,
    tile_w_ctb: u32,
    // WPP snapshot taken after the 2nd CTB of a row in this tile.
    wpp_snapshot: Option<Contexts>,
    slice_ctx: Contexts,
    log2_max_tb: u32,
    // M3 retention (only populated when `retain` is set).
    retain: bool,
    ret_ctbs: Vec<CtbData>,
    ret_cur: Option<CtbData>,
    tu_sink: Vec<TuData>,
    /// Residual captures (levels, transform_skip) from the current
    /// transform-tree leaf's walk_residual calls, in call order
    /// (luma, Cb, Cr); drained by the leaf into TuData entries.
    res_captures: Vec<(Vec<i32>, bool)>,
    // QP derivation state (§8.6.1), QpY domain.
    qp_last_final: i32,
    qp_cur_qg: Option<(u32, u32)>,
    qp_pred_cur: i32,
    qp_delta_cur: i32,
    qp_final_cur: i32,
    /// Per-min-TB QP map (picture coords), the oracle's
    /// set_QPY/get_QPY model: written per CU at CU end, read at
    /// sample positions by the A/B prediction of later QGs.
    qp_map: Vec<i32>,
    qp_map_w: usize,
    /// Final QpY per quantization group (QG index -> QpY), for the
    /// post-walk TU qp' patch: TUs walked before their QG's delta
    /// is coded initially carry the predicted QP only.
    qg_finals: std::collections::HashMap<(u32, u32), i32>,
}

impl<'a> Walker<'a> {
    fn trace(&mut self, tag: u8, val: u32) {
        self.hasher.update([tag]);
        self.hasher.update(val.to_le_bytes());
        if std::env::var("AGENTHAND_CABAC_TRACE").is_ok() {
            eprintln!("  el tag={tag:#04x} val={val} bins={}", self.cabac.bins);
        }
    }

    fn trace_hash(&self) -> String {
        let h = self.hasher.clone().finalize();
        h.iter().take(8).map(|b| format!("{b:02x}")).collect()
    }

    // -- neighbour helpers ---------------------------------------------------

    fn depth_at(&self, x: u32, y: u32) -> u8 {
        let mc = 1u32 << self.sps.min_cb_log2();
        let gx = (x / mc).min(self.min_cb_w - 1);
        let gy = (y / mc).min((self.depth_grid.len() as u32 / self.min_cb_w.max(1)) - 1);
        self.depth_grid[(gy * self.min_cb_w + gx) as usize]
    }

    fn set_depth(&mut self, x0: u32, y0: u32, log2: u32, depth: u8) {
        let mc = 1u32 << self.sps.min_cb_log2();
        let n = (1u32 << log2) / mc;
        let gx = x0 / mc;
        let gy = y0 / mc;
        for dy in 0..n {
            for dx in 0..n {
                let idx = ((gy + dy) * self.min_cb_w + (gx + dx)) as usize;
                if idx < self.depth_grid.len() {
                    self.depth_grid[idx] = depth;
                }
            }
        }
    }

    fn ipm_at(&self, gx: i32, gy: i32) -> u8 {
        if gx < 0 || gy < 0 || gx as u32 >= self.min_pu_w {
            return 1; // DC
        }
        let idx = (gy as u32 * self.min_pu_w + gx as u32) as usize;
        self.ipm_grid.get(idx).copied().unwrap_or(1)
    }

    fn set_ipm(&mut self, x0: u32, y0: u32, size: u32, mode: u8) {
        let mp = 1u32 << self.min_pu_log2;
        let n = (size / mp).max(1);
        let gx = x0 >> self.min_pu_log2;
        let gy = y0 >> self.min_pu_log2;
        for dy in 0..n {
            for dx in 0..n {
                let idx = ((gy + dy) * self.min_pu_w + (gx + dx)) as usize;
                if idx < self.ipm_grid.len() {
                    self.ipm_grid[idx] = mode;
                }
            }
        }
    }

    // -- QP derivation (§8.6.1, retention only) ----------------------------

    /// Derive the predicted QP when a CU starts a new quantization
    /// group, finalizing the previous group. Mirrors libde265
    /// `decode_quantization_parameters` (transform.cc), including
    /// its fallbacks: the base prediction is SliceQpY for the first
    /// QG of the slice or tile (or of a CTB row under WPP), else the
    /// previous group's final QP; a left/above neighbour QG in the
    /// same CTB replaces the base on its side; the sides average.
    /// The group's delta lands later via `qp_apply_delta`.
    fn enter_cu_qp(&mut self, x0: u32, y0: u32) {
        if !self.retain {
            return;
        }
        if !self.pps.cu_qp_delta_enabled {
            self.qp_final_cur = self.hdr.slice_qp;
            return;
        }
        let ctb_log2 = self.sps.ctb_log2();
        let qg_log2 = ctb_log2 - self.pps.diff_cu_qp_delta_depth;
        let qg = (x0 >> qg_log2, y0 >> qg_log2);
        if self.qp_cur_qg == Some(qg) {
            return;
        }
        let first_in_slice = self.qp_cur_qg.is_none();
        if self.qp_cur_qg.is_some() {
            self.qp_last_final = self.qp_final_cur;
        }
        let (x_qg, y_qg) = (qg.0 << qg_log2, qg.1 << qg_log2);
        let first_in_tile =
            x_qg == (self.tile_x0 << ctb_log2) && y_qg == (self.tile_y0 << ctb_log2);
        let ctb_mask = (1u32 << ctb_log2) - 1;
        let first_in_ctb_row = x_qg == 0 && (y_qg & ctb_mask) == 0;
        let base = if first_in_slice
            || first_in_tile
            || (first_in_ctb_row && self.pps.entropy_coding_sync_enabled)
        {
            self.hdr.slice_qp
        } else {
            self.qp_last_final
        };
        // A/B read the QP map at the neighboring SAMPLE position
        // (the QP of whatever CU covers that sample), gated to the
        // same CTB — the oracle's get_QPY model. A per-QG index map
        // misses QGs that were never a CU's top-left (M3 find).
        let min_tb_log2 = self.sps.log2_min_tb_size;
        let qa = if x_qg > 0 && (x_qg & ctb_mask) != 0 {
            let idx = ((y_qg >> min_tb_log2) as usize) * self.qp_map_w
                + ((x_qg - 1) >> min_tb_log2) as usize;
            self.qp_map.get(idx).copied().unwrap_or(base)
        } else {
            base
        };
        let qb = if y_qg > 0 && (y_qg & ctb_mask) != 0 {
            let idx = (((y_qg - 1) >> min_tb_log2) as usize) * self.qp_map_w
                + (x_qg >> min_tb_log2) as usize;
            self.qp_map.get(idx).copied().unwrap_or(base)
        } else {
            base
        };
        self.qp_pred_cur = (qa + qb + 1) >> 1;
        self.qp_delta_cur = 0;
        self.qp_final_cur =
            crate::heic::values::qp_wrap(self.qp_pred_cur, 0, self.sps.bit_depth_luma());
        self.qp_cur_qg = Some(qg);
        self.qg_finals.insert(qg, self.qp_final_cur);
    }

    /// Record a decoded cu_qp_delta for the current QG (§8.6.1).
    fn qp_apply_delta(&mut self, delta: i32) {
        if !self.retain {
            return;
        }
        self.qp_delta_cur = delta;
        self.qp_final_cur =
            crate::heic::values::qp_wrap(self.qp_pred_cur, delta, self.sps.bit_depth_luma());
        if let Some(qg) = self.qp_cur_qg {
            self.qg_finals.insert(qg, self.qp_final_cur);
        }
    }

    /// Per-component qp' for a given final QpY (§8.6.1).
    fn qp_prime_for(&self, qp_y: i32, cidx: usize) -> i32 {
        let offset = match cidx {
            1 => self.pps.cb_qp_offset + self.hdr.slice_cb_qp_offset,
            2 => self.pps.cr_qp_offset + self.hdr.slice_cr_qp_offset,
            _ => 0,
        };
        crate::heic::values::tu_qp_prime(
            qp_y,
            cidx,
            offset,
            self.sps.bit_depth_luma(),
            self.sps.bit_depth_chroma(),
        )
    }

    /// Post-walk patch: every retained TU's qp' becomes its QG's
    /// FINAL value (the delta may be coded in a later TU of the
    /// QG than the one being retained). Dequantization is
    /// unaffected (TUs preceding the delta TU in a QG have no
    /// coefficients), but the deblocking QP map reads QpY at
    /// every sample position.
    fn patch_tu_qps(&self, ctbs: &mut [CtbData]) {
        if !self.pps.cu_qp_delta_enabled {
            return;
        }
        let qg_log2 = self.sps.ctb_log2() - self.pps.diff_cu_qp_delta_depth;
        for ctb in ctbs.iter_mut() {
            for cu in ctb.cus.iter_mut() {
                for tu in cu.tus.iter_mut() {
                    let (lx, ly) = if tu.cidx == 0 {
                        (tu.x, tu.y)
                    } else {
                        (tu.x * 2, tu.y * 2)
                    };
                    let qg = (lx >> qg_log2, ly >> qg_log2);
                    if let Some(&qp_y) = self.qg_finals.get(&qg) {
                        tu.qp_prime = self.qp_prime_for(qp_y, tu.cidx);
                    }
                }
            }
        }
    }

    /// Build the retained TuData entries for one transform-tree
    /// leaf (M3): one entry per component the leaf carries, in the
    /// order the residuals were walked (luma, Cb, Cr), including
    /// cbf = false entries — prediction is owed for every TU.
    #[allow(clippy::too_many_arguments)]
    fn retain_leaf_tu(
        &mut self,
        x0: u32,
        y0: u32,
        x_base: u32,
        y_base: u32,
        log2_trafo: u32,
        blk_idx: u32,
        cbf_luma: bool,
        cbf_cb: bool,
        cbf_cr: bool,
    ) {
        let mut caps = std::mem::take(&mut self.res_captures).into_iter();
        let mut take = |cbf: bool| -> (Vec<i32>, bool) {
            if cbf {
                caps.next().unwrap_or_default()
            } else {
                (Vec::new(), false)
            }
        };
        let (lv, ts) = take(cbf_luma);
        self.tu_sink.push(TuData {
            x: x0,
            y: y0,
            log2: log2_trafo,
            cidx: 0,
            qp_prime: self.tu_qp_prime_now(0),
            transform_skip: ts,
            cbf: cbf_luma,
            levels: lv,
        });
        if log2_trafo > 2 {
            let (lv, ts) = take(cbf_cb);
            self.tu_sink.push(TuData {
                x: x0 >> 1,
                y: y0 >> 1,
                log2: log2_trafo - 1,
                cidx: 1,
                qp_prime: self.tu_qp_prime_now(1),
                transform_skip: ts,
                cbf: cbf_cb,
                levels: lv,
            });
            let (lv, ts) = take(cbf_cr);
            self.tu_sink.push(TuData {
                x: x0 >> 1,
                y: y0 >> 1,
                log2: log2_trafo - 1,
                cidx: 2,
                qp_prime: self.tu_qp_prime_now(2),
                transform_skip: ts,
                cbf: cbf_cr,
                levels: lv,
            });
        } else if blk_idx == 3 {
            let (lv, ts) = take(cbf_cb);
            self.tu_sink.push(TuData {
                x: x_base >> 1,
                y: y_base >> 1,
                log2: 2,
                cidx: 1,
                qp_prime: self.tu_qp_prime_now(1),
                transform_skip: ts,
                cbf: cbf_cb,
                levels: lv,
            });
            let (lv, ts) = take(cbf_cr);
            self.tu_sink.push(TuData {
                x: x_base >> 1,
                y: y_base >> 1,
                log2: 2,
                cidx: 2,
                qp_prime: self.tu_qp_prime_now(2),
                transform_skip: ts,
                cbf: cbf_cr,
                levels: lv,
            });
        }
    }

    /// The final per-component QP (prime) for a TU coded now
    /// (§8.6.1): luma from the current QG's QpY; chroma mapped
    /// through the chroma table with PPS + slice offsets.
    fn tu_qp_prime_now(&self, cidx: usize) -> i32 {
        self.qp_prime_for(self.qp_final_cur, cidx)
    }

    // -- SAO (§7.3.8.1) ------------------------------------------------------

    fn sao_offset_abs(&mut self, bit_depth: u32) -> SResult<u32> {
        let c_max = (1u32 << (bit_depth.min(10) - 5)) - 1;
        let mut v = 0u32;
        while v < c_max {
            if self.cabac.decode_bypass()? == 0 {
                break;
            }
            v += 1;
        }
        Ok(v)
    }

    fn walk_sao(&mut self, rx: u32, ry: u32) -> SResult<SaoParams> {
        if !self.sps.sao_enabled || (!self.hdr.sao_luma && !self.hdr.sao_chroma) {
            return Ok(SaoParams::default());
        }
        self.stats.sao_ctbs += 1;
        let mut merge_left = 0u32;
        if rx > self.tile_x0 {
            merge_left = self.cabac.decode_bin(CTX_SAO_MERGE)?;
            self.trace(0xA0, merge_left);
        }
        let mut merge_up = 0u32;
        if ry > self.tile_y0 && merge_left == 0 {
            merge_up = self.cabac.decode_bin(CTX_SAO_MERGE)?;
            self.trace(0xA1, merge_up);
        }
        if merge_left == 1 {
            return Ok(SaoParams::MergeLeft);
        }
        if merge_up == 1 {
            return Ok(SaoParams::MergeUp);
        }
        let mut comps = [SaoComp::default(); 3];
        let mut type_idx = [0u32; 3];
        for c in 0..3usize {
            let enabled = if c == 0 {
                self.hdr.sao_luma
            } else {
                self.hdr.sao_chroma
            };
            if !enabled {
                continue;
            }
            if c == 2 {
                type_idx[2] = type_idx[1];
            } else {
                let b0 = self.cabac.decode_bin(CTX_SAO_TYPE)?;
                let t = if b0 == 0 {
                    0
                } else if self.cabac.decode_bypass()? == 0 {
                    1
                } else {
                    2
                };
                type_idx[c] = t;
                self.trace(0xA2, t);
            }
            if type_idx[c] == 0 {
                continue;
            }
            comps[c].ty = type_idx[c] as u8;
            let bd = if c == 0 {
                self.sps.bit_depth_luma()
            } else {
                self.sps.bit_depth_chroma()
            };
            let mut abs = [0u32; 4];
            for i in 0..4 {
                abs[i] = self.sao_offset_abs(bd)?;
                self.trace(0xA3, abs[i]);
            }
            if type_idx[c] == 1 {
                // Band offsets carry explicit signs.
                for i in 0..4 {
                    let mut v = abs[i] as i32;
                    if abs[i] > 0 {
                        let sign = self.cabac.decode_bypass()?;
                        self.trace(0xA4, sign);
                        if sign == 1 {
                            v = -v;
                        }
                    }
                    comps[c].offsets[i] = v;
                }
                let bp = self.cabac.decode_bypass_bits(5)?;
                self.trace(0xA5, bp);
                comps[c].aux = bp as u8;
            } else {
                // Edge offsets are magnitudes; the sign is fixed by
                // the category at application time (§8.7.3). Cr
                // inherits Cb's EO class (no bits are read for it).
                for i in 0..4 {
                    comps[c].offsets[i] = abs[i] as i32;
                }
                if c != 2 {
                    let eo = self.cabac.decode_bypass_bits(2)?;
                    self.trace(0xA6, eo);
                    comps[c].aux = eo as u8;
                }
            }
        }
        if comps[2].ty == 2 {
            comps[2].aux = comps[1].aux;
        }
        Ok(SaoParams::Explicit(comps))
    }

    // -- intra prediction unit (§7.3.8.4) ------------------------------------

    fn mpm_list(&self, x0: u32, y0: u32) -> [u8; 3] {
        let ctb_log2 = self.sps.ctb_log2();
        let x0b = x0 & ((1 << ctb_log2) - 1);
        let y0b = y0 & ((1 << ctb_log2) - 1);
        let gx = (x0 >> self.min_pu_log2) as i32;
        let gy = (y0 >> self.min_pu_log2) as i32;
        // Left: available within the CTB, or from the CTB to the left
        // when that CTB is in the same tile.
        let cand_left = if x0b == 0 && (x0 >> ctb_log2) <= self.tile_x0 {
            1
        } else {
            self.ipm_at(gx - 1, gy)
        };
        // Above: never taken across a CTB-row boundary.
        let cand_up = if y0b == 0 { 1 } else { self.ipm_at(gx, gy - 1) };
        if cand_left == cand_up {
            if cand_left < 2 {
                [0, 1, 26]
            } else {
                let c = cand_left as i32;
                [
                    cand_left,
                    (2 + ((c - 2 - 1 + 32) & 31)) as u8,
                    (2 + ((c - 2 + 1) & 31)) as u8,
                ]
            }
        } else {
            let mut list = [cand_left, cand_up, 0u8];
            list[2] = if cand_left != 0 && cand_up != 0 {
                0
            } else if cand_left != 1 && cand_up != 1 {
                1
            } else {
                26
            };
            list
        }
    }

    fn walk_intra_pu(&mut self, x0: u32, y0: u32, log2_cb: u32) -> SResult<()> {
        let n_pb: usize = if self.intra_split { 4 } else { 1 };
        let pb_size = if self.intra_split {
            1u32 << (log2_cb - 1)
        } else {
            1u32 << log2_cb
        };
        let mut prev_flags = [0u32; 4];
        for i in 0..n_pb {
            prev_flags[i] = self.cabac.decode_bin(CTX_PREV_INTRA_LUMA)?;
            self.trace(0xB0, prev_flags[i]);
        }
        let order = [(0u32, 0u32), (1, 0), (0, 1), (1, 1)];
        for i in 0..n_pb {
            let (ox, oy) = order[i];
            let px = x0 + ox * pb_size;
            let py = y0 + oy * pb_size;
            let list = self.mpm_list(px, py);
            let mode = if prev_flags[i] == 1 {
                // mpm_idx: truncated-unary bypass, max 2.
                let mut idx = 0u32;
                while idx < 2 {
                    if self.cabac.decode_bypass()? == 0 {
                        break;
                    }
                    idx += 1;
                }
                self.trace(0xB1, idx);
                list[idx as usize]
            } else {
                let rem = self.cabac.decode_bypass_bits(5)?;
                self.trace(0xB2, rem);
                let mut sorted = list;
                sorted.sort();
                let mut cnt = 0u32;
                let mut m = 0u8;
                loop {
                    if !sorted.contains(&m) {
                        if cnt == rem {
                            break;
                        }
                        cnt += 1;
                    }
                    m += 1;
                    if m > 34 {
                        return invalid("rem_intra mode out of range");
                    }
                }
                m
            };
            self.pb_modes[i] = mode;
            self.set_ipm(px, py, pb_size, mode);
            self.stats.pus += 1;
        }
        // intra_chroma_pred_mode (once per CU for 4:2:0)
        let b0 = self.cabac.decode_bin(CTX_INTRA_CHROMA)?;
        let bin_mode = if b0 == 0 {
            4u32
        } else {
            self.cabac.decode_bypass_bits(2)?
        };
        self.trace(0xB3, bin_mode);
        let luma0 = self.pb_modes[0];
        self.chroma_mode_c = crate::heic::values::resolve_chroma_mode(bin_mode, luma0 as u32) as u8;
        Ok(())
    }

    // -- residual coding (§7.3.8.10) ------------------------------------------

    fn last_prefix(&mut self, base: usize, log2: u32, c_idx: u32) -> SResult<u32> {
        let (ctx_offset, ctx_shift) = if c_idx == 0 {
            (
                3 * (log2 as i32 - 2) + ((log2 as i32 - 1) >> 2),
                (log2 + 1) >> 2,
            )
        } else {
            (15, log2 - 2)
        };
        let max = (log2 << 1) - 1;
        let mut i = 0u32;
        while i < max {
            let inc = ((i >> ctx_shift) as i32 + ctx_offset) as usize;
            let bin = self.cabac.decode_bin(base + inc)?;
            if bin == 0 {
                break;
            }
            i += 1;
        }
        Ok(i)
    }

    /// H.265 k-th order Exp-Golomb bypass binarization (§9.3.2.5):
    /// a run of 1 bits (each adds 1<<n, n starting at k and growing),
    /// terminated by a 0 bit, then n fixed-length bits. NOT the H.264
    /// leading-zeros form. Verified against libde265's
    /// decode_EGk_bypass on the smoke_640 QP-delta suffix (bits "0"
    /// -> value 0 in a single bin).
    fn eg_bypass(&mut self) -> SResult<u32> {
        let mut base: u64 = 0;
        let mut n: u32 = 0;
        loop {
            let b = self.cabac.decode_bypass()?;
            if b == 0 {
                break;
            }
            if n >= 31 {
                return invalid("exp-golomb bypass prefix too long");
            }
            base += 1u64 << n;
            n += 1;
        }
        // Range pin (shift-trap audit, N6): the cap above keeps
        // n <= 31, so base <= 2^31 - 1 and the n-bit suffix <=
        // 2^31 - 1; the sum fits u64 trivially and u32 exactly
        // (max 2^32 - 2). The closing `as u32` is lossless.
        let suffix = if n == 0 {
            0
        } else {
            self.cabac.decode_bypass_bits(n)?
        };
        Ok((base + suffix as u64) as u32)
    }

    fn coeff_remaining(&mut self, rice: u32) -> SResult<u32> {
        let mut prefix = 0u32;
        loop {
            let b = self.cabac.decode_bypass()?;
            if b == 0 {
                break;
            }
            prefix += 1;
            if prefix > 32 {
                return invalid("coeff_abs_level_remaining prefix too long");
            }
        }
        // Value assembly in u64 end to end (shift-trap audit, N6):
        // the old form truncated twice — `as u32` on the shifted
        // prefix term, and a u32 bypass read of k = prefix - 3 +
        // rice bits, which can reach 33 and silently drop the top
        // bit. Both losses were silent wrong levels for out-of-spec
        // streams. The exact value is computed here and refused
        // when it cannot be a u32 level; every in-spec stream is
        // bit-identical to the old result.
        let v: u64 = if prefix < 3 {
            u64::from(prefix << rice) + u64::from(self.cabac.decode_bypass_bits(rice)?)
        } else {
            let k = (prefix - 3) + rice;
            let mut suffix: u64 = 0;
            for _ in 0..k {
                suffix = (suffix << 1) | u64::from(self.cabac.decode_bypass()?);
            }
            (((1u64 << (prefix - 3)) + 2) << rice) + suffix
        };
        match u32::try_from(v) {
            Ok(v) => Ok(v),
            Err(_) => invalid("coeff_abs_level_remaining value exceeds u32 range"),
        }
    }

    fn walk_residual(&mut self, log2: u32, scan: Scan, c_idx: u32) -> SResult<()> {
        // Retained coefficient levels (M3), raster order of this TU.
        let mut raster: Vec<i32> = if self.retain {
            vec![0; 1usize << (2 * log2)]
        } else {
            Vec::new()
        };
        // transform_skip_flag (inside residual_coding, before last coeff)
        let mut transform_skip = false;
        if self.pps.transform_skip_enabled && log2 <= 2 {
            let b = self
                .cabac
                .decode_bin(CTX_TRANSFORM_SKIP + if c_idx > 0 { 1 } else { 0 })?;
            transform_skip = b == 1;
            if transform_skip {
                self.stats.transform_skip_tus += 1;
            }
            self.trace(0xC0, b);
        }
        let _ = transform_skip; // context derivation for TS TUs is a
                                // range-extension path (refused SPS);
                                // base-profile TS keeps the normal map.
                                // last significant coeff prefixes
        let px = self.last_prefix(CTX_LAST_X, log2, c_idx)?;
        let py = self.last_prefix(CTX_LAST_Y, log2, c_idx)?;
        let suffix_val = |cabac: &mut Cabac<'a>, prefix: u32| -> SResult<u32> {
            if prefix > 3 {
                let len = (prefix >> 1) - 1;
                Ok(cabac.decode_bypass_bits(len)?)
            } else {
                Ok(0)
            }
        };
        let sx = suffix_val(&mut self.cabac, px)?;
        let sy = suffix_val(&mut self.cabac, py)?;
        let val = |prefix: u32, suffix: u32| -> u32 {
            if prefix > 3 {
                (1u32 << ((prefix >> 1) - 1)) * (2 + (prefix & 1)) + suffix
            } else {
                prefix
            }
        };
        let (mut last_x, mut last_y) = (val(px, sx), val(py, sy));
        if scan == Scan::Vert {
            std::mem::swap(&mut last_x, &mut last_y);
        }
        let n = 1u32 << log2;
        if last_x >= n || last_y >= n {
            return invalid(format!(
                "last significant coeff ({last_x},{last_y}) outside {n}x{n} TU"
            ));
        }
        self.trace(0xC1, last_x);
        self.trace(0xC2, last_y);

        let order = tu_scan_order(log2, scan);
        // scan position index of every (x,y)
        let num = (n * n) as usize;
        let mut scan_pos = vec![0u32; num];
        for (i, &(x, y)) in order.iter().enumerate() {
            scan_pos[(y * n + x) as usize] = i as u32;
        }
        let last_cg_x = last_x >> 2;
        let last_cg_y = last_y >> 2;
        let cg_k = (n >> 2).max(1);
        let cg_order = scan_pattern(cg_k, scan);
        let mut cg_scan_idx = vec![0u32; (cg_k * cg_k) as usize];
        for (i, &(x, y)) in cg_order.iter().enumerate() {
            cg_scan_idx[(y * cg_k + x) as usize] = i as u32;
        }
        let num_last_subset = cg_scan_idx[(last_cg_y * cg_k + last_cg_x) as usize];
        let last_scan_pos_in_last_cg = scan_pos[(last_y * n + last_x) as usize] & 15;

        let mut group_flag = vec![0u32; (cg_k * cg_k) as usize];
        let mut greater1_ctx: u32 = 1;

        for i in (0..=num_last_subset).rev() {
            let (x_cg, y_cg) = cg_order[i as usize];
            let cg_cell = (y_cg * cg_k + x_cg) as usize;
            // coded_sub_block_flag
            let mut implicit_non_zero = false;
            if i < num_last_subset && i > 0 {
                let mut ctx_cg = 0u32;
                if x_cg + 1 < cg_k {
                    ctx_cg += group_flag[(y_cg * cg_k + x_cg + 1) as usize];
                }
                if y_cg + 1 < cg_k {
                    ctx_cg += group_flag[((y_cg + 1) * cg_k + x_cg) as usize] * 2;
                }
                let inc = ctx_cg.min(1) + if c_idx > 0 { 2 } else { 0 };
                group_flag[cg_cell] = self.cabac.decode_bin(CTX_SIG_GROUP + inc as usize)?;
                implicit_non_zero = true;
            } else {
                group_flag[cg_cell] =
                    u32::from((x_cg == last_cg_x && y_cg == last_cg_y) || (x_cg == 0 && y_cg == 0));
            }
            self.trace(0xC3, group_flag[cg_cell]);
            if group_flag[cg_cell] == 0 {
                continue;
            }
            // prev_sig for the ctx map (right + 2*below group flags)
            let mut prev_sig = 0usize;
            if x_cg + 1 < cg_k {
                prev_sig += group_flag[(y_cg * cg_k + x_cg + 1) as usize] as usize;
            }
            if y_cg + 1 < cg_k {
                prev_sig += 2 * group_flag[((y_cg + 1) * cg_k + x_cg) as usize] as usize;
            }
            // scf_offset + map row
            let map_row: &[u8; 16] = if log2 == 2 {
                &CTX_IDX_MAP[scan_index(scan)][0]
            } else {
                &CTX_IDX_MAP[scan_index(scan)][prev_sig + 1]
            };
            let mut scf_offset: i32 = if c_idx > 0 { 27 } else { 0 };
            if log2 > 2 {
                if c_idx == 0 {
                    if x_cg > 0 || y_cg > 0 {
                        scf_offset += 3;
                    }
                    if log2 == 3 {
                        scf_offset += if scan == Scan::Diag { 9 } else { 15 };
                    } else {
                        scf_offset += 21;
                    }
                } else if log2 == 3 {
                    scf_offset += 9;
                } else {
                    scf_offset += 12;
                }
            }
            // significant coeff flags within the CG (reverse scan order)
            let cg_positions: Vec<(u32, u32)> = {
                let inner = scan_pattern(4, scan);
                inner
                    .iter()
                    .map(|&(ix, iy)| (x_cg * 4 + ix, y_cg * 4 + iy))
                    .collect()
            };
            let n_end: i32 = if i == num_last_subset {
                last_scan_pos_in_last_cg as i32 - 1
            } else {
                15
            };
            let mut sig_idx: Vec<u32> = Vec::with_capacity(16);
            if i == num_last_subset {
                sig_idx.push(last_scan_pos_in_last_cg);
            }
            if n_end >= 0 {
                let nb0 = sig_idx.len();
                let mut n = n_end;
                while n > 0 {
                    let (gx, gy) = cg_positions[n as usize];
                    let _ = (gx, gy);
                    let inc = map_row[n as usize] as usize + scf_offset as usize;
                    let sig = self.cabac.decode_bin(CTX_SIG_COEFF + inc)?;
                    self.trace(0xC6, sig);
                    if sig == 1 {
                        sig_idx.push(n as u32);
                    }
                    n -= 1;
                }
                if sig_idx.len() != nb0 {
                    implicit_non_zero = false;
                }
                if !implicit_non_zero {
                    let off0 = if i == 0 {
                        if c_idx == 0 {
                            0usize
                        } else {
                            27usize
                        }
                    } else {
                        (scf_offset + 2) as usize
                    };
                    let sig0 = self.cabac.decode_bin(CTX_SIG_COEFF + off0)?;
                    self.trace(0xC6, sig0);
                    if sig0 == 1 {
                        sig_idx.push(0);
                    }
                } else {
                    sig_idx.push(0);
                }
            }
            let n_sig = sig_idx.len();
            if n_sig == 0 {
                continue;
            }
            self.stats.sig_coeffs += n_sig as u64;
            // greater1 pass (first min(n_sig, 8) coeffs in collected order)
            let mut ctx_set: u32 = if i > 0 && c_idx == 0 { 2 } else { 0 };
            if i != num_last_subset && greater1_ctx == 0 {
                ctx_set += 1;
            }
            greater1_ctx = 1;
            let n_g1 = n_sig.min(8);
            let mut g1_flags = [0u32; 8];
            let mut first_g1: Option<usize> = None;
            for m in 0..n_g1 {
                let inc = ((ctx_set << 2) + greater1_ctx) as usize + if c_idx > 0 { 16 } else { 0 };
                let flag = self.cabac.decode_bin(CTX_GREATER1 + inc)?;
                self.trace(0xC7, flag);
                g1_flags[m] = flag;
                if flag == 1 && first_g1.is_none() {
                    first_g1 = Some(m);
                }
                // ffmpeg: ctx = (ctx + (ctx-1 < 2)) & (flag-1) — once a
                // 1-flag drops it to 0, later 0-flags do NOT raise it
                // again within this CG; it only climbs 1 -> 2 -> 3.
                greater1_ctx = if flag == 1 {
                    0
                } else if (1..3).contains(&greater1_ctx) {
                    greater1_ctx + 1
                } else {
                    greater1_ctx
                };
            }
            // greater2 for the first greater1 coeff
            let mut g2_coeff: Option<usize> = None;
            if let Some(f) = first_g1 {
                let inc = ctx_set as usize + if c_idx > 0 { 4 } else { 0 };
                let g2 = self.cabac.decode_bin(CTX_GREATER2 + inc)?;
                self.trace(0xC8, g2);
                // The coeff is the greater2 coeff (and gets the level-3
                // remaining threshold) whether or not the flag is set;
                // only the level increment depends on the flag value.
                g2_coeff = Some(f);
                if g2 == 1 {
                    g1_flags[f] += 1;
                }
            }
            // signs (bypass), one hidden when sign-data-hiding applies
            let first_nz = sig_idx[0];
            let last_nz = sig_idx[n_sig - 1];
            let sign_hidden = crate::heic::values::sign_hidden(
                self.pps.sign_data_hiding_enabled,
                last_nz,
                first_nz,
            );
            let n_signs = if sign_hidden { n_sig - 1 } else { n_sig };
            let mut signs: Vec<u32> = Vec::with_capacity(n_sig);
            for _ in 0..n_signs {
                let s = self.cabac.decode_bypass()?;
                self.trace(0xC4, s);
                signs.push(s);
            }
            if sign_hidden {
                signs.push(0); // provisional; derived below
            }
            // remaining levels
            let mut rice: u32 = 0;
            let mut signed_levels: Vec<i32> = vec![0; n_sig];
            for m in 0..n_sig {
                let need = if m < 8 {
                    let base = 1 + g1_flags[m];
                    let thresh = if g2_coeff == Some(m) { 3 } else { 2 };
                    base == thresh
                } else {
                    true
                };
                let level = if need {
                    let rem = self.coeff_remaining(rice)?;
                    let level = if m < 8 {
                        1 + g1_flags[m] + rem
                    } else {
                        1 + rem
                    };
                    self.trace(0xC5, rem);
                    if level > (3 << rice) {
                        rice = (rice + 1).min(4);
                    }
                    level
                } else {
                    1 + g1_flags[m]
                };
                if self.retain {
                    signed_levels[m] = if signs[m] == 1 {
                        -(level as i32)
                    } else {
                        level as i32
                    };
                }
            }
            if self.retain {
                if sign_hidden {
                    // The hidden sign belongs to the coefficient with
                    // the smallest scan position in the sub-block:
                    // the last entry of sig_idx order (index n_sig-1).
                    crate::heic::values::apply_hidden_sign(&mut signed_levels, n_sig - 1);
                }
                let nn = 1usize << log2;
                for m in 0..n_sig {
                    let (gx, gy) = cg_positions[sig_idx[m] as usize];
                    raster[gy as usize * nn + gx as usize] = signed_levels[m];
                }
            }
        }
        if self.retain {
            self.res_captures.push((raster, transform_skip));
        }
        Ok(())
    }

    // -- transform tree (§7.3.8.7) ---------------------------------------------
    //
    // Returns (cbf_cb, cbf_cr) in effect for this node after parsing.

    #[allow(clippy::too_many_arguments)]
    fn walk_transform_tree(
        &mut self,
        x0: u32,
        y0: u32,
        x_base: u32,
        y_base: u32,
        log2_trafo: u32,
        depth: u32,
        blk_idx: u32,
        cbf_cb_in: bool,
        cbf_cr_in: bool,
    ) -> SResult<(bool, bool)> {
        let mut cbf_cb = cbf_cb_in;
        let mut cbf_cr = cbf_cr_in;
        // split_transform_flag
        let split = if log2_trafo <= self.log2_max_tb
            && log2_trafo > self.sps.log2_min_tb_size
            && depth < self.max_trafo_depth
            && !(self.intra_split && depth == 0)
        {
            let inc = (5i32 - log2_trafo as i32).clamp(0, 2) as usize;
            let b = self.cabac.decode_bin(CTX_SPLIT_TRANSFORM + inc)?;
            self.trace(0xD0, b);
            b == 1
        } else {
            log2_trafo > self.log2_max_tb || (self.intra_split && depth == 0)
        };
        // chroma cbfs (not for 4x4 luma TUs in 4:2:0)
        if log2_trafo > 2 {
            if depth == 0 || cbf_cb {
                let b = self.cabac.decode_bin(CTX_CBF_CHROMA + depth as usize)?;
                cbf_cb = b == 1;
                self.trace(0xD1, b);
            }
            if depth == 0 || cbf_cr {
                let b = self.cabac.decode_bin(CTX_CBF_CHROMA + depth as usize)?;
                cbf_cr = b == 1;
                self.trace(0xD2, b);
            }
        }
        if split {
            let half = 1u32 << (log2_trafo - 1);
            let subs = [
                (x0, y0, 0u32),
                (x0 + half, y0, 1),
                (x0, y0 + half, 2),
                (x0 + half, y0 + half, 3),
            ];
            for (sx, sy, bi) in subs {
                self.walk_transform_tree(
                    sx,
                    sy,
                    x0,
                    y0,
                    log2_trafo - 1,
                    depth + 1,
                    bi,
                    cbf_cb,
                    cbf_cr,
                )?;
            }
            return Ok((cbf_cb, cbf_cr));
        }
        // leaf TU
        let cbf_luma = {
            let inc = if depth == 0 { 1usize } else { 0usize };
            let b = self.cabac.decode_bin(CTX_CBF_LUMA + inc)?;
            self.trace(0xD3, b);
            b == 1
        };
        self.stats.tus += 1;
        if cbf_luma || cbf_cb || cbf_cr {
            // cu_qp_delta at the first coded TU of the quantization group
            if self.pps.cu_qp_delta_enabled && !self.is_qp_delta_coded {
                // truncated-unary prefix (ctx 0 for the first bin, 1
                // after), capped at 5 ones, then an EG0 bypass suffix.
                let mut prefix = 0u32;
                loop {
                    let inc = if prefix == 0 { 0usize } else { 1usize };
                    let b = self.cabac.decode_bin(CTX_CU_QP_DELTA + inc)?;
                    if b == 0 {
                        break;
                    }
                    prefix += 1;
                    if prefix == 5 {
                        break;
                    }
                }
                let mut value = prefix;
                if prefix == 5 {
                    value += self.eg_bypass()?;
                }
                if value != 0 {
                    let sign = self.cabac.decode_bypass()?;
                    if self.retain {
                        let delta = if sign == 1 {
                            -(value as i32)
                        } else {
                            value as i32
                        };
                        self.qp_apply_delta(delta);
                    }
                }
                self.is_qp_delta_coded = true;
                self.trace(0xD4, value);
            }
            // scan selection
            let tu_mode = if self.intra_split && depth >= 1 {
                self.pb_modes[blk_idx as usize]
            } else {
                self.pb_modes[0]
            };
            let scan_l = scan_for_mode(tu_mode, log2_trafo);
            let scan_c = scan_for_mode(self.chroma_mode_c, log2_trafo);
            if cbf_luma {
                self.stats.tu_luma_residuals += 1;
                self.walk_residual(log2_trafo, scan_l, 0)?;
            }
            if log2_trafo > 2 {
                if cbf_cb {
                    self.walk_residual(log2_trafo - 1, scan_c, 1)?;
                }
                if cbf_cr {
                    self.walk_residual(log2_trafo - 1, scan_c, 2)?;
                }
            } else if blk_idx == 3 {
                // shared 4x4 chroma TU of the parent 8x8 region
                if cbf_cb {
                    self.walk_residual(2, scan_c, 1)?;
                }
                if cbf_cr {
                    self.walk_residual(2, scan_c, 2)?;
                }
            }
        }
        // Retention is owed for EVERY leaf, including all-cbf-false
        // (prediction-only) leaves: reconstruction must predict them
        // too. (Found by the M3 smoke parity diff: prediction-only
        // leaves in flat areas emitted nothing and decoded as 0.)
        if self.retain {
            self.retain_leaf_tu(
                x0, y0, x_base, y_base, log2_trafo, blk_idx, cbf_luma, cbf_cb, cbf_cr,
            );
        }
        Ok((cbf_cb, cbf_cr))
    }

    // -- coding unit / quadtree (§7.3.8.5, §7.3.8.4) ---------------------------

    fn walk_coding_unit(&mut self, x0: u32, y0: u32, log2_cb: u32) -> SResult<()> {
        self.stats.cus += 1;
        self.enter_cu_qp(x0, y0);
        if self.retain {
            self.tu_sink = Vec::new();
        }
        if self.pps.transquant_bypass_enabled {
            let b = self.cabac.decode_bin(CTX_TRANSQUANT_BYPASS)?;
            if b == 1 {
                return Err(SyntaxError::Refusal("transquant-bypass CU".into()));
            }
        }
        // part_mode (intra): only signalled at minimum CB size
        self.intra_split = false;
        if log2_cb == self.sps.min_cb_log2() {
            let b = self.cabac.decode_bin(CTX_PART_MODE)?;
            self.intra_split = b == 0;
            self.trace(0xE0, b);
        }
        // pcm_flag (terminate-coded) when eligible
        if !self.intra_split
            && self.sps.pcm_enabled
            && log2_cb >= self.sps.log2_min_pcm_cb_size
            && log2_cb <= self.sps.log2_min_pcm_cb_size + self.sps.log2_diff_max_min_pcm_cb
        {
            let pcm = self.cabac.decode_terminate()?;
            if pcm == 1 {
                return Err(SyntaxError::Refusal("PCM CU".into()));
            }
        }
        self.walk_intra_pu(x0, y0, log2_cb)?;
        self.max_trafo_depth =
            self.sps.max_transform_hierarchy_depth_intra + if self.intra_split { 1 } else { 0 };
        self.walk_transform_tree(x0, y0, x0, y0, log2_cb, 0, 0, false, false)?;
        if self.retain {
            // Prediction blocks: one per CU, or four when the CU is
            // split into NxN PBs at minimum CB size.
            let mut pbs = Vec::new();
            if self.intra_split {
                let half = 1u32 << (log2_cb - 1);
                for (i, (ox, oy)) in [(0u32, 0u32), (1, 0), (0, 1), (1, 1)].iter().enumerate() {
                    pbs.push(PbData {
                        x: x0 + ox * half,
                        y: y0 + oy * half,
                        log2: log2_cb - 1,
                        luma_mode: self.pb_modes[i] as u32,
                        chroma_mode: self.chroma_mode_c as u32,
                    });
                }
            } else {
                pbs.push(PbData {
                    x: x0,
                    y: y0,
                    log2: log2_cb,
                    luma_mode: self.pb_modes[0] as u32,
                    chroma_mode: self.chroma_mode_c as u32,
                });
            }
            let cu = CuData {
                x0,
                y0,
                log2: log2_cb,
                pbs,
                tus: std::mem::take(&mut self.tu_sink),
                qp_y: self.qp_final_cur,
            };
            if let Some(ctb) = &mut self.ret_cur {
                ctb.cus.push(cu);
            }
            // Publish this CU's final QpY over its whole area (the
            // oracle's set_QPY): later QGs' A/B prediction reads
            // the map at sample positions.
            let min_tb_log2 = self.sps.log2_min_tb_size;
            let size = 1u32 << log2_cb;
            let cx0 = (x0 >> min_tb_log2) as usize;
            let cy0 = (y0 >> min_tb_log2) as usize;
            let cn = (size >> min_tb_log2) as usize;
            for cy in cy0..(cy0 + cn).min(self.qp_map.len() / self.qp_map_w.max(1)) {
                for cx in cx0..(cx0 + cn).min(self.qp_map_w) {
                    let idx = cy * self.qp_map_w + cx;
                    if idx < self.qp_map.len() {
                        self.qp_map[idx] = self.qp_final_cur;
                    }
                }
            }
        }
        Ok(())
    }

    fn walk_quadtree(&mut self, x0: u32, y0: u32, log2_cb: u32, depth: u8) -> SResult<()> {
        let size = 1u32 << log2_cb;
        let split = if x0 + size <= self.sps.width
            && y0 + size <= self.sps.height
            && log2_cb > self.sps.min_cb_log2()
        {
            let left_depth = if x0 > self.tile_x0 << self.sps.ctb_log2() {
                self.depth_at(x0 - 1, y0)
            } else {
                0
            };
            let up_depth = if y0 > self.tile_y0 << self.sps.ctb_log2() {
                self.depth_at(x0, y0 - 1)
            } else {
                0
            };
            let inc = usize::from(left_depth > depth) + usize::from(up_depth > depth);
            let b = self.cabac.decode_bin(CTX_SPLIT_CU + inc)?;
            self.trace(0xE1, b);
            b == 1
        } else {
            log2_cb > self.sps.min_cb_log2()
        };
        // quantization-group reset at QG-sized nodes
        if self.pps.cu_qp_delta_enabled
            && log2_cb >= self.sps.ctb_log2() - self.pps.diff_cu_qp_delta_depth
        {
            self.is_qp_delta_coded = false;
        }
        if split {
            self.stats.split_cus += 1;
            let half = size / 2;
            self.walk_quadtree(x0, y0, log2_cb - 1, depth + 1)?;
            if x0 + half < self.sps.width {
                self.walk_quadtree(x0 + half, y0, log2_cb - 1, depth + 1)?;
            }
            if y0 + half < self.sps.height {
                self.walk_quadtree(x0, y0 + half, log2_cb - 1, depth + 1)?;
            }
            if x0 + half < self.sps.width && y0 + half < self.sps.height {
                self.walk_quadtree(x0 + half, y0 + half, log2_cb - 1, depth + 1)?;
            }
        } else {
            self.walk_coding_unit(x0, y0, log2_cb)?;
            self.set_depth(x0, y0, log2_cb, depth);
        }
        Ok(())
    }
}

fn scan_for_mode(mode: u8, log2: u32) -> Scan {
    if log2 <= 3 {
        if (6..=14).contains(&mode) {
            return Scan::Vert;
        }
        if (22..=30).contains(&mode) {
            return Scan::Horiz;
        }
    }
    Scan::Diag
}

/// One tile's geometry in CTB units.
struct TileGeom {
    x0: u32,
    y0: u32,
    w: u32,
    h: u32,
}

/// One component's SAO parameters for a CTB (§7.3.8.1, §8.7.3).
#[derive(Debug, Clone, Copy, Default)]
pub struct SaoComp {
    /// 0 = off, 1 = band offset, 2 = edge offset.
    pub ty: u8,
    /// Signed offsets for the four bands / EO categories.
    pub offsets: [i32; 4],
    /// Band position (ty 1) or EO class (ty 2).
    pub aux: u8,
}

/// A CTB's SAO state (§7.3.8.1): explicit per-component parameters,
/// or a merge directive resolved against the neighbour CTB at
/// reconstruction time.
#[derive(Debug, Clone, Copy)]
pub enum SaoParams {
    Explicit([SaoComp; 3]),
    MergeLeft,
    MergeUp,
}

impl Default for SaoParams {
    fn default() -> Self {
        SaoParams::Explicit([SaoComp::default(); 3])
    }
}

/// One transform unit's retained data (M3): signed coefficient
/// levels in raster order of the TU's own component plane, after
/// the hidden-sign derivation. `qp_prime` is the final per-component
/// QP (§8.6.1) dequantization must use.
#[derive(Debug, Clone)]
pub struct TuData {
    /// Position in this component's plane.
    pub x: u32,
    pub y: u32,
    pub log2: u32,
    /// 0 = luma, 1 = Cb, 2 = Cr.
    pub cidx: usize,
    pub qp_prime: i32,
    pub transform_skip: bool,
    /// Whether this component of the TU coded any coefficients.
    /// Prediction happens for every TU regardless; the residual is
    /// added only when this is set.
    pub cbf: bool,
    pub levels: Vec<i32>,
}

/// One prediction block: position/size in luma samples and its
/// resolved intra modes (chroma already resolved per §8.4.2).
#[derive(Debug, Clone)]
pub struct PbData {
    pub x: u32,
    pub y: u32,
    pub log2: u32,
    pub luma_mode: u32,
    pub chroma_mode: u32,
}

/// One coding unit's retained data.
#[derive(Debug, Clone)]
pub struct CuData {
    pub x0: u32,
    pub y0: u32,
    pub log2: u32,
    pub pbs: Vec<PbData>,
    pub tus: Vec<TuData>,
    /// QpY state at the end of this CU (pred + delta if the QG's
    /// delta was coded in this CU or an earlier CU of the QG).
    /// This is what the oracle's QP map holds for the CU's area
    /// (set_QPY's last write), and what deblocking reads.
    pub qp_y: i32,
}

/// One CTB's retained data, in walk order.
#[derive(Debug, Clone)]
pub struct CtbData {
    /// Absolute CTB coordinates in the picture's CTB raster.
    pub rx: u32,
    pub ry: u32,
    pub sao: SaoParams,
    pub cus: Vec<CuData>,
}

/// Everything reconstruction needs from one slice, retained by
/// `walk_slice_retained` (M3): CTBs in walk (decoding) order.
#[derive(Debug, Clone, Default)]
pub struct RetainedSlice {
    pub ctbs: Vec<CtbData>,
}

/// Outcome of a diagnostic walk: the stats accumulated up to the
/// point of failure (or completion), plus the error if one occurred.
/// The plain `walk_slice` discards stats on error; settler/diagnostic
/// callers need them.
pub struct WalkOutcome {
    pub stats: WalkStats,
    pub error: Option<SyntaxError>,
    /// Retained reconstruction data (only for `walk_slice_retained`
    /// on a successful walk).
    pub retained: Option<RetainedSlice>,
}

/// Walk one complete intra slice's data (the bytes after the slice
/// header, starting at the first CABAC byte). `data` is RBSP;
/// `epb_drops` holds the raw-slice-data-relative positions of the
/// emulation-prevention bytes that were dropped to produce it —
/// entry-point offsets count raw bytes, EPBs included (proven at the
/// M3 settler checks; see `walk_impl`), so the walker needs the drop
/// positions to convert the offsets into RBSP coordinates.
pub fn walk_slice(
    sps: &Sps,
    pps: &Pps,
    hdr: &SliceHeader,
    data: &[u8],
    epb_drops: &[usize],
) -> SResult<WalkStats> {
    let out = walk_impl(sps, pps, hdr, data, epb_drops, None, false);
    match out.error {
        Some(e) => Err(e),
        None => Ok(out.stats),
    }
}

/// Walk one intra slice like `walk_slice`, additionally retaining
/// everything reconstruction needs (M3): per-CTB SAO parameters,
/// per-CU prediction blocks with resolved modes, and per-TU signed
/// coefficient levels with their final QPs. The walk itself is the
/// same code path; retention only copies values out.
pub fn walk_slice_retained(
    sps: &Sps,
    pps: &Pps,
    hdr: &SliceHeader,
    data: &[u8],
    epb_drops: &[usize],
) -> WalkOutcome {
    walk_impl(sps, pps, hdr, data, epb_drops, None, true)
}

/// Diagnostic walk with an explicit substream-start override: when
/// `starts` is given (one absolute byte offset per substream, in the
/// same RBSP coordinates as `data`), those starts are used verbatim
/// instead of the converted entry-point offsets, and stats are
/// returned even when the walk errors. Used by the M3 settler checks
/// to test byte-accounting hypotheses.
pub fn walk_slice_diag(
    sps: &Sps,
    pps: &Pps,
    hdr: &SliceHeader,
    data: &[u8],
    epb_drops: &[usize],
    starts: Option<&[usize]>,
) -> WalkOutcome {
    walk_impl(sps, pps, hdr, data, epb_drops, starts, false)
}

fn walk_impl(
    sps: &Sps,
    pps: &Pps,
    hdr: &SliceHeader,
    data: &[u8],
    epb_drops: &[usize],
    starts_override: Option<&[usize]>,
    retain: bool,
) -> WalkOutcome {
    // Substream layout: tiles in tile-scan order; within a tile, one
    // substream per CTB row when WPP is on, else one per tile.
    let col_starts = pps.tile_col_starts(sps);
    let row_starts = pps.tile_row_starts(sps);
    let n_tile_cols = col_starts.len() - 1;
    let n_tile_rows = row_starts.len() - 1;
    let mut tiles = Vec::new();
    for ty in 0..n_tile_rows {
        for tx in 0..n_tile_cols {
            tiles.push(TileGeom {
                x0: col_starts[tx],
                y0: row_starts[ty],
                w: col_starts[tx + 1] - col_starts[tx],
                h: row_starts[ty + 1] - row_starts[ty],
            });
        }
    }
    // substream boundaries: (tile_idx, first_row, row_count)
    let mut substreams: Vec<(usize, u32, u32)> = Vec::new();
    for (ti, t) in tiles.iter().enumerate() {
        if pps.entropy_coding_sync_enabled {
            for r in 0..t.h {
                substreams.push((ti, r, 1));
            }
        } else {
            substreams.push((ti, 0, t.h));
        }
    }
    if substreams.is_empty() {
        return WalkOutcome {
            stats: WalkStats::default(),
            error: Some(SyntaxError::Invalid("no substreams".into())),
            retained: None,
        };
    }
    if hdr.num_entry_point_offsets as usize != substreams.len() - 1 {
        return WalkOutcome {
            stats: WalkStats::default(),
            error: Some(SyntaxError::Invalid(format!(
                "entry point count {} != substreams-1 {}",
                hdr.num_entry_point_offsets,
                substreams.len() - 1
            ))),
            retained: None,
        };
    }
    // Absolute byte offset of each substream: entry_point_offset_minus1
    // values are substream LENGTHS (§7.4.7.1: the offset of entry point
    // i+1 counted from entry point i), so offsets accumulate.
    let mut offsets = vec![0usize; substreams.len()];
    for i in 1..substreams.len() {
        offsets[i] = offsets[i - 1] + (hdr.entry_point_offset_minus1[i - 1] as usize + 1);
    }
    // Entry-point offsets count RAW byte-stream bytes, emulation
    // prevention bytes included (M3 settler verdict, 2026-10-28:
    // converting by the EPB drops before each boundary terminates all
    // 12 formerly-residual tiles exactly, and the previously fitted
    // ≤2-byte start rule was this same accounting in disguise — it
    // reproduced the converted starts whenever the preceding engine
    // stopped cleanly and broke exactly where a span held ≥3 EPBs).
    // Convert each accumulated offset to RBSP coordinates by
    // subtracting the EPBs dropped before it in the raw slice data.
    if !epb_drops.is_empty() {
        for off in offsets.iter_mut().skip(1) {
            let d = epb_drops.iter().filter(|&&p| p < *off).count();
            *off -= d;
        }
    }
    // Diagnostic override: caller-supplied starts, used verbatim.
    if let Some(starts) = starts_override {
        if starts.len() == offsets.len() {
            offsets.copy_from_slice(starts);
        }
    }

    let mut slice_ctx = Contexts::default();
    slice_ctx.init_for_slice(hdr.slice_qp);

    let min_cb_w = sps.width.div_ceil(1 << sps.min_cb_log2());
    let min_cb_h = sps.height.div_ceil(1 << sps.min_cb_log2());
    let min_pu_log2 = sps.min_cb_log2().saturating_sub(1).max(1);
    let min_pu_w = sps.width.div_ceil(1 << min_pu_log2);
    let min_pu_h = sps.height.div_ceil(1 << min_pu_log2);

    let mut first_engine = match Cabac::new_substream(data, slice_ctx.clone()) {
        Ok(e) => e,
        Err(e) => {
            return WalkOutcome {
                stats: WalkStats::default(),
                error: Some(e.into()),
                retained: None,
            }
        }
    };
    first_engine.dump_chain = crate::heic::cabac::chain_dump_enabled()
        && crate::heic::cabac::chain_dump_sub().is_none_or(|s| s == 0);
    let mut w = Walker {
        sps,
        pps,
        hdr,
        data,
        cabac: first_engine,
        stats: WalkStats::default(),
        hasher: Sha256::new(),
        depth_grid: vec![0; (min_cb_w * min_cb_h) as usize],
        min_cb_w,
        ipm_grid: vec![1; (min_pu_w * min_pu_h) as usize],
        min_pu_w,
        min_pu_log2,
        intra_split: false,
        pb_modes: [1; 4],
        chroma_mode_c: 1,
        max_trafo_depth: 0,
        is_qp_delta_coded: false,
        tile_x0: 0,
        tile_y0: 0,
        tile_w_ctb: 0,
        wpp_snapshot: None,
        slice_ctx: slice_ctx.clone(),
        log2_max_tb: sps.log2_min_tb_size + sps.log2_diff_max_min_tb,
        retain,
        ret_ctbs: Vec::new(),
        ret_cur: None,
        tu_sink: Vec::new(),
        res_captures: Vec::new(),
        qp_last_final: hdr.slice_qp,
        qp_cur_qg: None,
        qp_pred_cur: hdr.slice_qp,
        qp_delta_cur: 0,
        qp_final_cur: hdr.slice_qp,
        qp_map: vec![
            hdr.slice_qp;
            (((sps.width >> sps.log2_min_tb_size) + 1) * ((sps.height >> sps.log2_min_tb_size) + 1))
                as usize
        ],
        qp_map_w: (sps.width >> sps.log2_min_tb_size) as usize + 1,
        qg_finals: std::collections::HashMap::new(),
    };
    w.stats.slice_data_len = data.len();

    let ctb_log2 = sps.ctb_log2();
    let pic_ctbs_total = sps.width.div_ceil(1 << ctb_log2) * sps.height.div_ceil(1 << ctb_log2);

    let mut cur_start = 0usize;
    let loop_error = run_substreams(
        &mut w,
        &substreams,
        &tiles,
        &offsets,
        &slice_ctx,
        ctb_log2,
        pic_ctbs_total,
        &mut cur_start,
    )
    .err();

    // end_of_slice_segment_flag was decoded after the final CTU above.
    w.stats.end_consumed = cur_start + w.cabac.consumed_bytes();
    w.stats.bins += w.cabac.bins;
    w.stats.trace_final = w.trace_hash();
    // Finalize the retained CTB list (M3) when this was a retained
    // walk and it succeeded.
    let retained = if retain && loop_error.is_none() {
        if let Some(ctb) = w.ret_cur.take() {
            w.ret_ctbs.push(ctb);
        }
        let mut ctbs = std::mem::take(&mut w.ret_ctbs);
        w.patch_tu_qps(&mut ctbs);
        Some(RetainedSlice { ctbs })
    } else {
        None
    };
    WalkOutcome {
        stats: w.stats,
        error: loop_error,
        retained,
    }
}

/// The substream/CTB walk loop (split out of `walk_impl` so a failure
/// can return its error while the caller keeps the stats). On entry
/// `*cur_start` is the byte offset the current engine started at; it
/// is updated at every substream switch.
#[allow(clippy::too_many_arguments)]
fn run_substreams(
    w: &mut Walker<'_>,
    substreams: &[(usize, u32, u32)],
    tiles: &[TileGeom],
    offsets: &[usize],
    slice_ctx: &Contexts,
    ctb_log2: u32,
    pic_ctbs_total: u32,
    cur_start: &mut usize,
) -> SResult<()> {
    let data = w.data;
    for (si, &(ti, first_row, row_count)) in substreams.iter().enumerate() {
        if std::env::var_os("AGENTHAND_CABAC_CHAIN").is_some() {
            eprintln!("S {si} {}", offsets[si]);
        }
        let tile = &tiles[ti];
        if si > 0 {
            // terminate the previous substream on the old engine
            let end_bin = w.cabac.decode_terminate()?;
            // Absolute end of the previous substream: measured from the
            // offset the engine actually started at (which can sit 1-2
            // bytes before the header entry offset, see below).
            let consumed = *cur_start + w.cabac.consumed_bytes();
            w.stats.bins += w.cabac.bins;
            w.stats
                .substream_ends
                .push(((si - 1) as u32, end_bin, consumed, offsets[si]));
            if end_bin != 1 {
                return invalid(format!(
                    "substream {} end_of_sub_stream bin = {end_bin} (expected 1); \
                     consumed {consumed} vs boundary {}; ctbs done {}, bins {}",
                    si - 1,
                    offsets[si],
                    w.stats.ctbs,
                    w.stats.bins + w.cabac.bins
                ));
            }
            w.stats.trace_at_substream.push(w.trace_hash());
            if std::env::var("AGENTHAND_CABAC_DEBUG").is_ok() {
                eprintln!(
                    "switch to {si}: prev_start {cur_start} consumed {consumed} boundary {}",
                    offsets[si]
                );
            }
            // The next substream starts exactly at its converted
            // entry-point offset (raw byte count minus the EPBs
            // dropped before it — see walk_impl). A correctly stopped
            // engine lands on that same byte, which is what the M2-era
            // fitted rule mistook for an encoder flush byte.
            let start_off = offsets[si];
            *cur_start = start_off;
            // start the new substream
            let new_ctx = if first_row == 0 {
                // new tile: fresh slice contexts
                slice_ctx.clone()
            } else if tile.w <= 1 {
                slice_ctx.clone()
            } else {
                w.wpp_snapshot.clone().unwrap_or_else(|| slice_ctx.clone())
            };
            if start_off >= data.len() {
                return invalid(format!(
                    "entry point offset {} beyond slice data {}",
                    start_off,
                    data.len()
                ));
            }
            w.cabac = Cabac::new_substream(&data[start_off..], new_ctx)?;
            w.cabac.dump_chain = crate::heic::cabac::chain_dump_enabled()
                && crate::heic::cabac::chain_dump_sub().is_none_or(|s| s == si);
        }
        if first_row == 0 {
            w.tile_x0 = tile.x0;
            w.tile_y0 = tile.y0;
            w.tile_w_ctb = tile.w;
            w.wpp_snapshot = None;
        }
        w.stats.substreams += 1;
        for r in first_row..(first_row + row_count) {
            let ry = tile.y0 + r;
            for cx in 0..tile.w {
                let rx = tile.x0 + cx;
                let dbg = std::env::var("AGENTHAND_CABAC_DEBUG").is_ok();
                let (b0, c0, t0, s0) = (w.cabac.bins, w.stats.cus, w.stats.tus, w.stats.sig_coeffs);
                let pos0 = w.cabac.consumed_bytes();
                let sao = w.walk_sao(rx, ry)?;
                if w.retain {
                    if let Some(prev) = w.ret_cur.take() {
                        w.ret_ctbs.push(prev);
                    }
                    w.ret_cur = Some(CtbData {
                        rx,
                        ry,
                        sao,
                        cus: Vec::new(),
                    });
                }
                w.walk_quadtree(rx << ctb_log2, ry << ctb_log2, ctb_log2, 0)?;
                w.stats.ctbs += 1;
                // end_of_slice_segment_flag (§7.3.8 slice_segment_data):
                // terminating-process bin after EVERY CTU — 0 for all
                // but the last CTU of the slice segment, 1 there.
                let eos = w.cabac.decode_terminate()?;
                if w.stats.ctbs >= pic_ctbs_total {
                    if eos != 1 {
                        return invalid(format!(
                            "end_of_slice_segment bin = {eos} at final CTB (expected 1)"
                        ));
                    }
                    w.stats.end_bin = eos;
                } else if eos != 0 {
                    return invalid(format!(
                        "end_of_slice_segment bin = 1 at CTB {} of {} (premature end)",
                        w.stats.ctbs, pic_ctbs_total
                    ));
                }
                if dbg {
                    eprintln!(
                        "ctb ({rx},{ry}) bins={} bytes={} cus={} tus={} sig={}",
                        w.cabac.bins - b0,
                        w.cabac.consumed_bytes() - pos0,
                        w.stats.cus - c0,
                        w.stats.tus - t0,
                        w.stats.sig_coeffs - s0
                    );
                }
                // WPP snapshot: state after the 2nd CTB of the row
                if w.pps.entropy_coding_sync_enabled && cx == 1 {
                    w.wpp_snapshot = Some(w.cabac.ctx.clone());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod scan_order_tests {
    use super::{tu_scan_order, Scan};

    /// Boundary pin for the TU scan-order shifts (shift-trap
    /// audit, N6): `tu_scan_order` computes `1u32 << (log2 - 2)`
    /// and sizes its vector with `1usize << (2 * log2)`. Those
    /// are exact for the whole reachable range — TU log2 sizes
    /// are bounded by the SPS range check (Log2MinTbSizeY >= 2,
    /// Log2MaxTbSizeY <= 5) — and this test pins the range so a
    /// future caller passing an unbounded log2 fails loudly here
    /// instead of shifting absurdly in production.
    #[test]
    fn tu_scan_order_is_exact_across_the_sps_range() {
        for log2 in 2..=5u32 {
            let n = 1u32 << log2;
            for scan in [Scan::Diag, Scan::Horiz, Scan::Vert] {
                let order = tu_scan_order(log2, scan);
                assert_eq!(order.len(), (n * n) as usize, "log2 {log2}");
                assert!(order.iter().all(|&(x, y)| x < n && y < n));
                // A scan order is a permutation of the TU.
                let mut sorted = order.clone();
                sorted.sort_unstable();
                sorted.dedup();
                assert_eq!(sorted.len(), (n * n) as usize, "log2 {log2} permutation");
            }
        }
    }
}
