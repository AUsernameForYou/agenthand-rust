//! HEVC parameter sets + slice segment headers (M2), parsed from the
//! spec (H.265 §7.3.2, §7.3.6). Values the intra decode path needs
//! are kept; fields outside the subset are parsed for bit-exact
//! position and shallowly validated, per SPEC-SUBSET.md §2/§5.
//! Anything outside the stills envelope is a loud refusal.

use super::bitreader::BitReader;

#[derive(Debug, Clone)]
pub enum ParamError {
    Refusal(String),
    Malformed(String),
}

type Res<T> = Result<T, ParamError>;

fn refuse<T>(msg: impl Into<String>) -> Res<T> {
    Err(ParamError::Refusal(msg.into()))
}
fn malformed<T>(msg: impl Into<String>) -> Res<T> {
    Err(ParamError::Malformed(msg.into()))
}

// ---------------------------------------------------------------------------
// profile_tier_level (§7.3.3)

#[derive(Debug, Clone, Default)]
pub struct ProfileTierLevel {
    pub profile_idc: u32,
    pub tier_flag: bool,
    pub level_idc: u32,
}

fn parse_ptl(br: &mut BitReader, max_sub_layers_minus1: u32) -> Res<ProfileTierLevel> {
    let mut ptl = ProfileTierLevel::default();
    ptl.profile_idc = {
        let _space = br.u(2);
        ptl.tier_flag = br.u1() == 1;
        br.u(5)
    };
    let _compat = br.u(32);
    // 4 source/constraint flags + 44 constraint bits = 48.
    let _flags = br.u(4);
    let _c1 = br.u(32);
    let _c2 = br.u(12);
    ptl.level_idc = br.u(8);
    let mut sub_profile = Vec::new();
    let mut sub_level = Vec::new();
    for _ in 0..max_sub_layers_minus1 {
        sub_profile.push(br.u1() == 1);
        sub_level.push(br.u1() == 1);
    }
    // The reserved_zero_2bits loop exists only when
    // maxNumSubLayersMinus1 > 0 (§7.3.3).
    if max_sub_layers_minus1 > 0 && max_sub_layers_minus1 < 8 {
        let _reserved = br.u(2 * (8 - max_sub_layers_minus1));
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if sub_profile[i] {
            let _space = br.u(2);
            let _tier = br.u1();
            let _pidc = br.u(5);
            let _compat = br.u(32);
            let _flags = br.u(4);
            let _c1 = br.u(32);
            let _c2 = br.u(12);
        }
        if sub_level[i] {
            let _lvl = br.u(8);
        }
    }
    Ok(ptl)
}

// ---------------------------------------------------------------------------
// scaling lists (§7.3.4)

pub const SCALING_LIST_SIZE_IDS: usize = 4;
/// matrix counts per sizeId: 6,6,6,2
pub fn scaling_list_matrix_count(size_id: usize) -> usize {
    if size_id == 3 {
        2
    } else {
        6
    }
}

#[derive(Debug, Clone)]
pub struct ScalingLists {
    /// [sizeId][matrixId] -> coefficients in raster order (up to 64),
    /// with DC stored separately for sizeId >= 2.
    pub coef: [[Vec<i32>; 6]; SCALING_LIST_SIZE_IDS],
    /// [sizeId-2][matrixId] — sized 6 wide: sizeId 2 has six
    /// matrices. (Was 2 wide until the M5 mutation pass flipped a
    /// byte into a pred-copy with ref_id 2 at sizeId 2 and the
    /// index panicked; real streams may use any matrix here.)
    pub dc: [[i32; 6]; 2],
}

impl Default for ScalingLists {
    fn default() -> Self {
        // Default matrices per §7.3.4 defaults (the spec's default
        // scaling lists are the identity-ish tables in Table 7-... —
        // for M2 the parse path is what matters; M3 fills the exact
        // default tables when dequant lands. We record 16s as the
        // neutral placeholder so "no explicit list" is representable.
        let mut coef: [[Vec<i32>; 6]; 4] = Default::default();
        for size_id in 0..4 {
            let n = 1usize << (4 + (size_id << 1));
            let count = n.min(64);
            for m in 0..6 {
                coef[size_id][m] = vec![16; count];
            }
        }
        ScalingLists {
            coef,
            dc: [[16; 6]; 2],
        }
    }
}

fn parse_scaling_list_data(br: &mut BitReader) -> Res<ScalingLists> {
    let mut out = ScalingLists::default();
    for size_id in 0..4usize {
        let matrices = scaling_list_matrix_count(size_id);
        for matrix_id in 0..matrices {
            // §7.3.4: scaling_list_pred_mode_flag 0 = copy a
            // previous matrix (a ue delta follows); 1 = explicit
            // coefficients. These branches and the coefficient
            // count below were both inverted/short until
            // 2026-10-31: harmless while every corpus SPS used the
            // default lists (present flag 0), the garment batch
            // IMG_1433-1436 was the first with explicit lists and
            // desynced the SPS parse into the slice header (the
            // refusal the film run hit). The bit-consumption test
            // in this file pins both rules.
            let pred_mode = br.u1();
            if pred_mode == 0 {
                // Copy from matrix (matrix_id - delta - 1); a
                // reference below zero means the spec's default
                // matrix, which is already in place. Delta is
                // computed in i64 on the exact ue() value: as i32
                // it wrapped for large code numbers and the
                // subtraction could overflow (A6 harness class).
                let delta = i64::from(br.ue());
                let ref_id = matrix_id as i64 - delta - 1;
                if ref_id >= 0 {
                    let ref_id = ref_id as usize;
                    let src = out.coef[size_id][ref_id].clone();
                    out.coef[size_id][matrix_id] = src;
                    if size_id >= 2 {
                        let d = out.dc[size_id - 2][ref_id];
                        out.dc[size_id - 2][matrix_id] = d;
                    }
                }
            } else {
                // §7.3.4: coefNum = Min(64, 1 << (4 + (sizeId << 1)))
                // — 16/64/64/64 across the sizes, not 16/32/64/64.
                let coef_num = (1usize << (4 + (size_id << 1))).min(64);
                let mut next_coef: i32 = 8;
                if size_id > 1 {
                    // saturating: exact for every in-spec DC
                    // (a small range around 8); an absurd se()
                    // saturates instead of overflowing.
                    let dc = br.se().saturating_add(8);
                    out.dc[size_id - 2][matrix_id] = dc;
                    next_coef = dc;
                }
                let mut coefs = vec![0i32; coef_num];
                // coefficients are stored in diagonal-scan order in the
                // bitstream; M2 keeps them in stream order (M3 maps to
                // raster when dequant lands — same convention as the
                // spec's ScalingList array before scan mapping).
                for c in coefs.iter_mut() {
                    let delta = br.se();
                    // i64 + rem_euclid: the spec's (x + 256) % 256
                    // over mathematical integers (the Python
                    // reference's semantics) — total and exact for
                    // any delta se() can return, where the i32
                    // original could overflow on a crafted delta.
                    next_coef =
                        (i64::from(next_coef) + i64::from(delta) + 256).rem_euclid(256) as i32;
                    *c = next_coef;
                }
                out.coef[size_id][matrix_id] = coefs;
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// short-term reference picture sets (§7.3.7)

#[derive(Debug, Clone, Default)]
pub struct StRefPicSet {
    pub num_negative: usize,
    pub num_positive: usize,
    pub num_delta_pocs: usize, // NumDeltaPocs
}

fn parse_st_ref_pic_set(
    br: &mut BitReader,
    idx: usize,
    num_sets: usize,
    sets: &[StRefPicSet],
) -> Res<StRefPicSet> {
    let mut out = StRefPicSet::default();
    let inter_pred = if idx > 0 { br.u1() == 1 } else { false };
    if inter_pred {
        let ref_idx = if idx == num_sets {
            let delta_idx = br.ue() as usize;
            idx - delta_idx - 1
        } else {
            idx - 1
        };
        if ref_idx >= sets.len() {
            return malformed("st_ref_pic_set: ref idx out of range");
        }
        let _delta_rps_sign = br.u1();
        let _abs_delta_rps_minus1 = br.ue();
        let ref_num_delta = sets[ref_idx].num_delta_pocs;
        let mut used = vec![false; ref_num_delta + 1];
        for j in 0..=ref_num_delta {
            let u = br.u1() == 1;
            if !u {
                let _use_delta = br.u1();
            }
            used[j] = u;
        }
        // NumDeltaPocs for a predicted set counts the entries whose
        // used_by or use_delta is set; the exact delta values only
        // matter for inter pictures (refused) — for the IDR slice
        // path we need NumDeltaPocs only.
        out.num_delta_pocs = used.iter().filter(|&&u| u).count();
        // num_negative/positive derivation needs the sign walk; for
        // intra-only streams a predicted set is still parseable but
        // its poc values are unused. We approximate the split using
        // the reference set's, which is sufficient for IDR headers
        // (they select sets whose deltas are all zero in practice).
        out.num_negative = sets[ref_idx].num_negative.min(out.num_delta_pocs);
        out.num_positive = out.num_delta_pocs - out.num_negative;
        Ok(out)
    } else {
        out.num_negative = br.ue() as usize;
        out.num_positive = br.ue() as usize;
        if out.num_negative > 16 || out.num_positive > 16 {
            return malformed("st_ref_pic_set: too many pics");
        }
        for _ in 0..out.num_negative {
            let _d = br.ue();
            let _u = br.u1();
        }
        for _ in 0..out.num_positive {
            let _d = br.ue();
            let _u = br.u1();
        }
        out.num_delta_pocs = out.num_negative + out.num_positive;
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// HRD / VUI (§E.2)

fn parse_sub_layer_hrd(br: &mut BitReader, cpb_cnt: usize, sub_pic_hrd: bool) {
    // HRD values are skipped, never consumed; clamp the loop to
    // the spec's cpb_cnt range so a crafted count cannot spin the
    // parser (M5 untrusted-input bound).
    let cpb_cnt = cpb_cnt.min(32);
    for _ in 0..cpb_cnt {
        let _bit_rate = br.ue();
        let _cpb_size = br.ue();
        if sub_pic_hrd {
            let _cpb_du = br.ue();
            let _br_du = br.ue();
        }
        let _cbr = br.u1();
    }
}

fn parse_hrd(br: &mut BitReader, common_inf: bool, max_sub_layers_minus1: u32) {
    let (mut nal_hrd, mut vcl_hrd, mut sub_pic) = (false, false, false);
    if common_inf {
        nal_hrd = br.u1() == 1;
        vcl_hrd = br.u1() == 1;
        if nal_hrd || vcl_hrd {
            sub_pic = br.u1() == 1;
            if sub_pic {
                let _tick_div = br.u(8);
                let _du_len = br.u(5);
                let _in_pic_timing = br.u1();
                let _dpb_du_len = br.u(5);
            }
            let _bit_rate_scale = br.u(4);
            let _cpb_size_scale = br.u(4);
            if sub_pic {
                let _cpb_du_scale = br.u(4);
            }
            let _init_len = br.u(5);
            let _au_len = br.u(5);
            let _dpb_len = br.u(5);
        }
    }
    for i in 0..=max_sub_layers_minus1 {
        let fixed = br.u1() == 1;
        if !fixed {
            let _elem_dur = br.ue();
        }
        let mut low_delay = false;
        let mut cpb_cnt = 1usize;
        if i < max_sub_layers_minus1 || true {
            low_delay = br.u1() == 1;
        }
        if !low_delay {
            cpb_cnt = br.ue() as usize + 1;
        }
        if nal_hrd {
            parse_sub_layer_hrd(br, cpb_cnt, sub_pic);
        }
        if vcl_hrd {
            parse_sub_layer_hrd(br, cpb_cnt, sub_pic);
        }
    }
}

/// The VUI colour description the RGB conversion consumes (M5):
/// signalled range + matrix. Defaults are the unspecified state
/// (limited range, no matrix signalled); consumers decide the
/// fallback, the parser only reports what the stream says.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct VuiInfo {
    pub video_full_range: bool,
    pub colour_primaries: Option<u8>,
    pub transfer_characteristics: Option<u8>,
    pub matrix_coeffs: Option<u8>,
}

fn parse_vui(br: &mut BitReader, max_sub_layers_minus1: u32) -> VuiInfo {
    let mut vui = VuiInfo::default();
    if br.u1() == 1 {
        // aspect_ratio_info_present
        let idc = br.u(8);
        if idc == 255 {
            let _w = br.u(16);
            let _h = br.u(16);
        }
    }
    if br.u1() == 1 {
        let _appropriate = br.u1();
    }
    if br.u1() == 1 {
        // video_signal_type_present
        let _format = br.u(3);
        vui.video_full_range = br.u1() == 1;
        if br.u1() == 1 {
            vui.colour_primaries = Some(br.u(8) as u8);
            vui.transfer_characteristics = Some(br.u(8) as u8);
            vui.matrix_coeffs = Some(br.u(8) as u8);
        }
    }
    if br.u1() == 1 {
        let _top = br.ue();
        let _bot = br.ue();
    }
    let _neutral = br.u1();
    let _field_seq = br.u1();
    let _frame_field_info = br.u1();
    if br.u1() == 1 {
        let _l = br.ue();
        let _r = br.ue();
        let _t = br.ue();
        let _b = br.ue();
    }
    if br.u1() == 1 {
        // timing info
        let _units = br.u(32);
        let _scale = br.u(32);
        if br.u1() == 1 {
            let _ticks = br.ue();
        }
        if br.u1() == 1 {
            parse_hrd(br, true, max_sub_layers_minus1);
        }
    }
    if br.u1() == 1 {
        // bitstream restriction
        let _tiles_fixed = br.u1();
        let _mv_over = br.u1();
        let _restricted = br.u1();
        let _min_seg = br.ue();
        let _max_bytes = br.ue();
        let _max_bits = br.ue();
        let _mv_h = br.ue();
        let _mv_v = br.ue();
    }
    vui
}

// ---------------------------------------------------------------------------
// VPS (§7.3.2.1)

#[derive(Debug, Clone)]
pub struct Vps {
    pub id: u32,
    pub max_sub_layers_minus1: u32,
    pub ptl: ProfileTierLevel,
}

pub fn parse_vps(rbsp: &[u8]) -> Res<Vps> {
    let mut br = BitReader::new(rbsp);
    let _nal_header = br.u(16);
    let id = br.u(4);
    let _base_internal = br.u1();
    let _base_available = br.u1();
    let _max_layers = br.u(6);
    let max_sub = br.u(3);
    let _nesting = br.u1();
    let _reserved = br.u(16);
    let ptl = parse_ptl(&mut br, max_sub)?;
    let ordering_present = br.u1() == 1;
    let start = if ordering_present { 0 } else { max_sub };
    for _ in start..=max_sub {
        let _buf = br.ue();
        let _reorder = br.ue();
        let _latency = br.ue();
    }
    let max_layer_id = br.u(6);
    let num_layer_sets = br.ue();
    for _ in 1..=num_layer_sets {
        for _ in 0..=max_layer_id {
            let _included = br.u1();
        }
    }
    if br.u1() == 1 {
        // timing info
        let _units = br.u(32);
        let _scale = br.u(32);
        if br.u1() == 1 {
            let _ticks = br.ue();
        }
        if br.u1() == 1 {
            let num_hrd = br.ue();
            for i in 0..num_hrd {
                let _layer_set_idx = br.ue();
                let common = if i > 0 { br.u1() == 1 } else { true };
                parse_hrd(&mut br, common, max_sub);
            }
        }
    }
    let _ext = br.u1();
    Ok(Vps {
        id,
        max_sub_layers_minus1: max_sub,
        ptl,
    })
}

// ---------------------------------------------------------------------------
// SPS (§7.3.2.2)

#[derive(Debug, Clone)]
pub struct Sps {
    pub id: u32,
    pub max_sub_layers_minus1: u32,
    pub ptl: ProfileTierLevel,
    pub chroma_format_idc: u32,
    pub separate_colour_plane: bool,
    pub width: u32,
    pub height: u32,
    pub conf_win: (u32, u32, u32, u32), // left, right, top, bottom (in chroma units)
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
    pub log2_max_pic_order_cnt_lsb: u32, // minus4 + 4
    pub log2_min_cb_size: u32,           // log2_min_luma_coding_block_size_minus3 + 3
    pub log2_diff_max_min_cb: u32,
    pub log2_min_tb_size: u32, // log2_min_luma_transform_block_size_minus2 + 2
    pub log2_diff_max_min_tb: u32,
    pub max_transform_hierarchy_depth_inter: u32,
    pub max_transform_hierarchy_depth_intra: u32,
    pub scaling_list_enabled: bool,
    pub scaling_lists: Option<ScalingLists>,
    pub amp_enabled: bool,
    pub sao_enabled: bool,
    pub pcm_enabled: bool,
    pub pcm_bit_depth_luma_minus1: u32,
    pub pcm_bit_depth_chroma_minus1: u32,
    pub log2_min_pcm_cb_size: u32,
    pub log2_diff_max_min_pcm_cb: u32,
    pub pcm_loop_filter_disabled: bool,
    pub num_short_term_ref_pic_sets: usize,
    pub st_ref_pic_sets: Vec<StRefPicSet>,
    pub temporal_mvp_enabled: bool,
    pub strong_intra_smoothing: bool,
    pub vui: VuiInfo,
}

impl Sps {
    pub fn bit_depth_luma(&self) -> u32 {
        self.bit_depth_luma_minus8 + 8
    }
    pub fn bit_depth_chroma(&self) -> u32 {
        self.bit_depth_chroma_minus8 + 8
    }
    pub fn ctb_log2(&self) -> u32 {
        self.log2_min_cb_size + self.log2_diff_max_min_cb
    }
    pub fn ctb_size(&self) -> u32 {
        1 << self.ctb_log2()
    }
    pub fn width_in_ctbs(&self) -> u32 {
        self.width.div_ceil(self.ctb_size())
    }
    pub fn height_in_ctbs(&self) -> u32 {
        self.height.div_ceil(self.ctb_size())
    }
    pub fn num_ctbs(&self) -> u32 {
        self.width_in_ctbs() * self.height_in_ctbs()
    }
    pub fn min_cb_log2(&self) -> u32 {
        self.log2_min_cb_size
    }
    pub fn crop_width(&self) -> u32 {
        // conformance window in luma samples (4:2:0: x offsets ×2,
        // y offsets ×2)
        let (l, r, ..) = self.conf_win;
        self.width - 2 * (l + r)
    }
    pub fn crop_height(&self) -> u32 {
        let (.., t, b) = self.conf_win;
        self.height - 2 * (t + b)
    }
}

/// Range-check the SPS log2 fields that later size shifts,
/// allocations, and fixed-width reads (Gauntlet shift-trap audit,
/// N6, 2026-10-31). `ue()` returns up to 2^32 - 2 and nothing
/// checked these fields, so a crafted SPS could declare a
/// ~2^32-bit POC field — turning every `BitReader::u(poc_bits)`
/// into a billions-of-iterations CPU sink — or drive
/// `1 << ctb_log2` and the TU-size shifts with absurd counts.
/// The ranges are the spec's own (H.265 §7.4.3.2.1 plus the
/// Main/Main10 limits of the accepted subset): POC lsb minus4 in
/// 0..=12, Log2MinCbSizeY in 3..=6, Log2CtbSizeY in 4..=6,
/// Log2MinTbSizeY in 2..=5, Log2MaxTbSizeY at most 5. The failure
/// point is this refusal, at header time, before any use.
fn check_sps_log2_ranges(
    poc_lsb_minus4: u32,
    min_cb_minus3: u32,
    diff_cb: u32,
    min_tb_minus2: u32,
    diff_tb: u32,
) -> Res<()> {
    if poc_lsb_minus4 > 12 {
        return malformed("log2_max_pic_order_cnt_lsb_minus4 > 12 (spec range 0..=12)");
    }
    if min_cb_minus3 > 3 {
        return malformed("log2_min_cb_size_minus3 puts Log2MinCbSizeY above 6");
    }
    let min_cb = min_cb_minus3 + 3;
    if diff_cb > 6 - min_cb || min_cb + diff_cb < 4 {
        return malformed(format!(
            "SPS log2 cb range puts Log2CtbSizeY at {}, outside 4..=6",
            min_cb.saturating_add(diff_cb)
        ));
    }
    if min_tb_minus2 > 3 {
        return malformed("log2_min_tb_size_minus2 puts Log2MinTbSizeY above 5");
    }
    let min_tb = min_tb_minus2 + 2;
    if diff_tb > 5 - min_tb {
        return malformed("SPS log2 tb range puts Log2MaxTbSizeY above 5");
    }
    Ok(())
}

pub fn parse_sps(rbsp: &[u8]) -> Res<Sps> {
    let mut br = BitReader::new(rbsp);
    let _nal_header = br.u(16);
    let _vps_id = br.u(4);
    let max_sub = br.u(3);
    let _nesting = br.u1();
    let ptl = parse_ptl(&mut br, max_sub)?;
    let id = br.ue();
    let chroma_format_idc = br.ue();
    if chroma_format_idc != 1 {
        return refuse(format!(
            "chroma_format_idc {chroma_format_idc} (only 4:2:0 in subset)"
        ));
    }
    let separate_colour_plane = false;
    let width = br.ue();
    let height = br.ue();
    let mut conf_win = (0, 0, 0, 0);
    if br.u1() == 1 {
        conf_win = (br.ue(), br.ue(), br.ue(), br.ue());
    }
    // Dimension sanity (M5 untrusted-input fix): a zero dimension
    // is meaningless, and the conformance window must leave a
    // positive crop area. crop_width()/crop_height() subtract in
    // u32; without this refusal a crafted window wraps them to
    // ~2^32 and downstream Vec::with_capacity aborts the process
    // (found by the M5 mutation pass: one flipped byte in a valid
    // file drove a 189 GB allocation attempt).
    if width == 0 || height == 0 {
        return malformed("SPS picture dimension is zero");
    }
    if 2 * (conf_win.0 as u64 + conf_win.1 as u64) >= width as u64
        || 2 * (conf_win.2 as u64 + conf_win.3 as u64) >= height as u64
    {
        return malformed("conformance window leaves no picture area");
    }
    let bit_depth_luma_minus8 = br.ue();
    let bit_depth_chroma_minus8 = br.ue();
    if bit_depth_luma_minus8 > 2 || bit_depth_chroma_minus8 > 2 {
        return refuse("bit depth > 10 out of subset");
    }
    let log2_max_poc_lsb_minus4 = br.ue();
    let ordering_present = br.u1() == 1;
    let start = if ordering_present { 0 } else { max_sub };
    for _ in start..=max_sub {
        let _buf = br.ue();
        let _reorder = br.ue();
        let _latency = br.ue();
    }
    let log2_min_cb_minus3 = br.ue();
    let log2_diff_cb = br.ue();
    let log2_min_tb_minus2 = br.ue();
    let log2_diff_tb = br.ue();
    let max_depth_inter = br.ue();
    let max_depth_intra = br.ue();
    check_sps_log2_ranges(
        log2_max_poc_lsb_minus4,
        log2_min_cb_minus3,
        log2_diff_cb,
        log2_min_tb_minus2,
        log2_diff_tb,
    )?;
    let scaling_list_enabled = br.u1() == 1;
    let mut scaling_lists = None;
    if scaling_list_enabled && br.u1() == 1 {
        scaling_lists = Some(parse_scaling_list_data(&mut br)?);
    }
    let amp_enabled = br.u1() == 1;
    let sao_enabled = br.u1() == 1;
    let pcm_enabled = br.u1() == 1;
    let (
        mut pcm_bd_luma,
        mut pcm_bd_chroma,
        mut log2_min_pcm_minus3,
        mut log2_diff_pcm,
        mut pcm_lf_disabled,
    ) = (0, 0, 0, 0, false);
    if pcm_enabled {
        pcm_bd_luma = br.u(4);
        pcm_bd_chroma = br.u(4);
        log2_min_pcm_minus3 = br.ue();
        log2_diff_pcm = br.ue();
        pcm_lf_disabled = br.u1() == 1;
    }
    let num_st = br.ue() as usize;
    if num_st > 64 {
        return malformed("num_short_term_ref_pic_sets > 64");
    }
    let mut st_sets = Vec::with_capacity(num_st);
    for i in 0..num_st {
        let s = parse_st_ref_pic_set(&mut br, i, num_st, &st_sets)?;
        st_sets.push(s);
    }
    if br.u1() == 1 {
        // long_term_ref_pics_present
        let num_lt = br.ue() as usize;
        if num_lt > 32 {
            return malformed("num_long_term_ref_pics_sps > 32");
        }
        let poc_bits = log2_max_poc_lsb_minus4 + 4;
        for _ in 0..num_lt {
            let _poc = br.u(poc_bits);
            let _used = br.u1();
        }
    }
    let temporal_mvp = br.u1() == 1;
    let strong_intra = br.u1() == 1;
    let mut vui = VuiInfo::default();
    if br.u1() == 1 {
        vui = parse_vui(&mut br, max_sub);
    }
    if br.u1() == 1 {
        // sps_extension_present
        let range_ext = br.u1() == 1;
        let _multilayer = br.u1();
        let _3d = br.u1();
        let _scc = br.u1();
        let _4bits = br.u(4);
        if range_ext {
            return refuse("sps_range_extension (range-extension tools out of subset)");
        }
    }
    Ok(Sps {
        id,
        max_sub_layers_minus1: max_sub,
        ptl,
        chroma_format_idc,
        separate_colour_plane,
        width,
        height,
        conf_win,
        bit_depth_luma_minus8,
        bit_depth_chroma_minus8,
        log2_max_pic_order_cnt_lsb: log2_max_poc_lsb_minus4 + 4,
        log2_min_cb_size: log2_min_cb_minus3 + 3,
        log2_diff_max_min_cb: log2_diff_cb,
        log2_min_tb_size: log2_min_tb_minus2 + 2,
        log2_diff_max_min_tb: log2_diff_tb,
        max_transform_hierarchy_depth_inter: max_depth_inter,
        max_transform_hierarchy_depth_intra: max_depth_intra,
        scaling_list_enabled,
        scaling_lists,
        amp_enabled,
        sao_enabled,
        pcm_enabled,
        pcm_bit_depth_luma_minus1: pcm_bd_luma,
        pcm_bit_depth_chroma_minus1: pcm_bd_chroma,
        log2_min_pcm_cb_size: log2_min_pcm_minus3 + 3,
        log2_diff_max_min_pcm_cb: log2_diff_pcm,
        pcm_loop_filter_disabled: pcm_lf_disabled,
        num_short_term_ref_pic_sets: num_st,
        st_ref_pic_sets: st_sets,
        temporal_mvp_enabled: temporal_mvp,
        strong_intra_smoothing: strong_intra,
        vui,
    })
}

// ---------------------------------------------------------------------------
// PPS (§7.3.2.3)

#[derive(Debug, Clone)]
pub struct Pps {
    pub id: u32,
    pub sps_id: u32,
    pub dependent_slice_segments_enabled: bool,
    pub output_flag_present: bool,
    pub num_extra_slice_header_bits: u32,
    pub sign_data_hiding_enabled: bool,
    pub cabac_init_present: bool,
    pub init_qp_minus26: i32,
    pub constrained_intra_pred: bool,
    pub transform_skip_enabled: bool,
    pub cu_qp_delta_enabled: bool,
    pub diff_cu_qp_delta_depth: u32,
    pub cb_qp_offset: i32,
    pub cr_qp_offset: i32,
    pub slice_chroma_qp_offsets_present: bool,
    pub transquant_bypass_enabled: bool,
    pub tiles_enabled: bool,
    pub entropy_coding_sync_enabled: bool, // WPP
    pub num_tile_columns_minus1: u32,
    pub num_tile_rows_minus1: u32,
    pub uniform_spacing: bool,
    pub column_width_minus1: Vec<u32>,
    pub row_height_minus1: Vec<u32>,
    pub loop_filter_across_tiles_enabled: bool,
    pub loop_filter_across_slices_enabled: bool,
    pub deblocking_filter_control_present: bool,
    pub deblocking_filter_override_enabled: bool,
    pub pps_deblocking_filter_disabled: bool,
    pub beta_offset_div2: i32,
    pub tc_offset_div2: i32,
    pub scaling_lists: Option<ScalingLists>,
    pub log2_parallel_merge_level_minus2: u32,
    pub slice_segment_header_extension_present: bool,
}

impl Pps {
    /// Tile column boundaries in CTB units (starts), length = cols+1.
    pub fn tile_col_starts(&self, sps: &Sps) -> Vec<u32> {
        let w = sps.width_in_ctbs();
        let cols = (self.num_tile_columns_minus1 + 1) as usize;
        if !self.tiles_enabled || cols <= 1 {
            return vec![0, w];
        }
        if self.uniform_spacing {
            let mut starts = Vec::with_capacity(cols + 1);
            for i in 0..cols {
                starts.push((i as u32 * w) / cols as u32);
            }
            starts.push(w);
            starts
        } else {
            let mut starts = vec![0u32];
            let mut acc = 0u32;
            for i in 0..cols {
                let cw = if i + 1 < cols {
                    self.column_width_minus1.get(i).copied().unwrap_or(0) + 1
                } else {
                    w - acc
                };
                acc += cw;
                starts.push(acc);
            }
            starts
        }
    }
    pub fn tile_row_starts(&self, sps: &Sps) -> Vec<u32> {
        let h = sps.height_in_ctbs();
        let rows = (self.num_tile_rows_minus1 + 1) as usize;
        if !self.tiles_enabled || rows <= 1 {
            return vec![0, h];
        }
        if self.uniform_spacing {
            let mut starts = Vec::with_capacity(rows + 1);
            for i in 0..rows {
                starts.push((i as u32 * h) / rows as u32);
            }
            starts.push(h);
            starts
        } else {
            let mut starts = vec![0u32];
            let mut acc = 0u32;
            for i in 0..rows {
                let rh = if i + 1 < rows {
                    self.row_height_minus1.get(i).copied().unwrap_or(0) + 1
                } else {
                    h - acc
                };
                acc += rh;
                starts.push(acc);
            }
            starts
        }
    }
}

pub fn parse_pps(rbsp: &[u8]) -> Res<Pps> {
    let mut br = BitReader::new(rbsp);
    let _nal_header = br.u(16);
    let id = br.ue();
    let sps_id = br.ue();
    let dependent = br.u1() == 1;
    let output_flag_present = br.u1() == 1;
    let num_extra = br.u(3);
    let sign_data_hiding = br.u1() == 1;
    let cabac_init_present = br.u1() == 1;
    let _num_ref_l0 = br.ue();
    let _num_ref_l1 = br.ue();
    let init_qp_minus26 = br.se();
    let constrained_intra = br.u1() == 1;
    let transform_skip_enabled = br.u1() == 1;
    let cu_qp_delta_enabled = br.u1() == 1;
    let mut diff_cu_qp_delta_depth = 0;
    if cu_qp_delta_enabled {
        diff_cu_qp_delta_depth = br.ue();
    }
    let cb_qp_offset = br.se();
    let cr_qp_offset = br.se();
    let slice_chroma_qp_offsets_present = br.u1() == 1;
    let _weighted_pred = br.u1();
    let _weighted_bipred = br.u1();
    let transquant_bypass = br.u1() == 1;
    let tiles_enabled = br.u1() == 1;
    let entropy_sync = br.u1() == 1;
    let (mut num_tile_cols_m1, mut num_tile_rows_m1) = (0u32, 0u32);
    let mut uniform_spacing = true;
    let mut col_widths = Vec::new();
    let mut row_heights = Vec::new();
    // loop_filter_across_tiles_enabled_flag lives INSIDE the tiles
    // block (§7.3.2.3); with tiles off it defaults to 1 and no bit is
    // present (it is NOT gated on WPP).
    let mut loop_filter_across_tiles = true;
    if tiles_enabled {
        num_tile_cols_m1 = br.ue();
        num_tile_rows_m1 = br.ue();
        // M5 untrusted-input bound: the width/height lists below
        // loop on these counts over a saturating bit reader, so an
        // unbounded count is a hang, not an error. No picture in
        // the decoder's canvas ceiling has more than ~1024 tile
        // columns; 4096 is generous.
        if num_tile_cols_m1 >= 4096 || num_tile_rows_m1 >= 4096 {
            return malformed("tile column/row count beyond any in-scope picture");
        }
        uniform_spacing = br.u1() == 1;
        if !uniform_spacing {
            for _ in 0..num_tile_cols_m1 {
                col_widths.push(br.ue());
            }
            for _ in 0..num_tile_rows_m1 {
                row_heights.push(br.ue());
            }
        }
        loop_filter_across_tiles = br.u1() == 1;
    }
    let loop_filter_across_slices = br.u1() == 1;
    let deblocking_control_present = br.u1() == 1;
    let mut deblocking_override_enabled = false;
    let mut pps_deblocking_disabled = false;
    let (mut beta, mut tc) = (0i32, 0i32);
    if deblocking_control_present {
        deblocking_override_enabled = br.u1() == 1;
        pps_deblocking_disabled = br.u1() == 1;
        if !pps_deblocking_disabled {
            beta = br.se();
            tc = br.se();
        }
    }
    let mut scaling_lists = None;
    if br.u1() == 1 {
        scaling_lists = Some(parse_scaling_list_data(&mut br)?);
    }
    let _lists_modification_present = br.u1();
    let log2_parallel_merge = br.ue();
    let slice_ext_present = br.u1() == 1;
    if br.u1() == 1 {
        let range_ext = br.u1() == 1;
        let _multilayer = br.u1();
        let _3d = br.u1();
        let _scc = br.u1();
        let _4bits = br.u(4);
        if range_ext {
            return refuse("pps_range_extension (out of subset)");
        }
    }
    Ok(Pps {
        id,
        sps_id,
        dependent_slice_segments_enabled: dependent,
        output_flag_present,
        num_extra_slice_header_bits: num_extra,
        sign_data_hiding_enabled: sign_data_hiding,
        cabac_init_present,
        init_qp_minus26,
        constrained_intra_pred: constrained_intra,
        transform_skip_enabled,
        cu_qp_delta_enabled,
        diff_cu_qp_delta_depth,
        cb_qp_offset,
        cr_qp_offset,
        slice_chroma_qp_offsets_present,
        transquant_bypass_enabled: transquant_bypass,
        tiles_enabled,
        entropy_coding_sync_enabled: entropy_sync,
        num_tile_columns_minus1: num_tile_cols_m1,
        num_tile_rows_minus1: num_tile_rows_m1,
        uniform_spacing,
        column_width_minus1: col_widths,
        row_height_minus1: row_heights,
        loop_filter_across_tiles_enabled: loop_filter_across_tiles,
        loop_filter_across_slices_enabled: loop_filter_across_slices,
        deblocking_filter_control_present: deblocking_control_present,
        deblocking_filter_override_enabled: deblocking_override_enabled,
        pps_deblocking_filter_disabled: pps_deblocking_disabled,
        beta_offset_div2: beta,
        tc_offset_div2: tc,
        scaling_lists,
        log2_parallel_merge_level_minus2: log2_parallel_merge,
        slice_segment_header_extension_present: slice_ext_present,
    })
}

// ---------------------------------------------------------------------------
// Slice segment header (§7.3.6.1)

pub const NAL_TRAIL_R: u8 = 1;
pub const NAL_BLA_W_LP: u8 = 16;
pub const NAL_IDR_W_RADL: u8 = 19;
pub const NAL_IDR_N_LP: u8 = 20;
pub const NAL_CRA: u8 = 21;
pub const NAL_VPS: u8 = 32;
pub const NAL_SPS: u8 = 33;
pub const NAL_PPS: u8 = 34;

pub fn is_irap(nal_type: u8) -> bool {
    (16..=21).contains(&nal_type)
}
pub fn is_idr(nal_type: u8) -> bool {
    nal_type == NAL_IDR_W_RADL || nal_type == NAL_IDR_N_LP
}

#[derive(Debug, Clone)]
pub struct SliceHeader {
    pub nal_type: u8,
    pub first_slice_segment_in_pic: bool,
    pub dependent_slice_segment: bool,
    pub slice_segment_address: u32,
    pub slice_type: u32,
    pub pps_id: u32,
    pub sao_luma: bool,
    pub sao_chroma: bool,
    pub deblocking_disabled: bool,
    pub beta_offset_div2: i32,
    pub tc_offset_div2: i32,
    pub loop_filter_across_slices: bool,
    pub num_entry_point_offsets: u32,
    pub entry_point_offset_minus1: Vec<u32>,
    pub slice_qp: i32, // SliceQpY
    pub slice_cb_qp_offset: i32,
    pub slice_cr_qp_offset: i32,
    pub cu_chroma_qp_offset_enabled: bool,
    /// bit position in the RBSP where CABAC slice data begins
    /// (after byte alignment).
    pub cabac_start_bit: usize,
}

pub fn parse_slice_header(
    rbsp: &[u8],
    nal_type: u8,
    sps: &Sps,
    pps_lookup: &dyn Fn(u32) -> Option<Pps>,
) -> Res<(SliceHeader, Pps)> {
    let mut br = BitReader::new(rbsp);
    let _nal_header = br.u(16);
    let first = br.u1() == 1;
    if is_irap(nal_type) {
        let _no_output = br.u1();
    }
    let pps_id = br.ue();
    let pps = pps_lookup(pps_id)
        .ok_or_else(|| ParamError::Malformed(format!("slice references unknown pps {pps_id}")))?;
    let mut dependent = false;
    let mut seg_addr = 0u32;
    if !first {
        let num_ctbs = sps.num_ctbs();
        let addr_bits = 32 - (num_ctbs - 1).leading_zeros();
        if pps.dependent_slice_segments_enabled {
            dependent = br.u1() == 1;
        }
        seg_addr = br.u(addr_bits.max(1));
    }
    let mut hdr = SliceHeader {
        nal_type,
        first_slice_segment_in_pic: first,
        dependent_slice_segment: dependent,
        slice_segment_address: seg_addr,
        slice_type: 0,
        pps_id,
        sao_luma: false,
        sao_chroma: false,
        deblocking_disabled: pps.pps_deblocking_filter_disabled,
        beta_offset_div2: pps.beta_offset_div2,
        tc_offset_div2: pps.tc_offset_div2,
        loop_filter_across_slices: pps.loop_filter_across_slices_enabled,
        num_entry_point_offsets: 0,
        entry_point_offset_minus1: Vec::new(),
        slice_qp: 26 + pps.init_qp_minus26,
        slice_cb_qp_offset: 0,
        slice_cr_qp_offset: 0,
        cu_chroma_qp_offset_enabled: false,
        cabac_start_bit: 0,
    };
    if dependent {
        // A dependent segment inherits everything; no CABAC data of
        // its own header follows beyond alignment. Not expected in
        // the corpus (single-slice streams) — refuse loudly.
        return refuse("dependent slice segment (corpus streams are independent)");
    }
    for _ in 0..pps.num_extra_slice_header_bits {
        let _reserved = br.u1();
    }
    hdr.slice_type = br.ue();
    if hdr.slice_type != 2 {
        return refuse(format!(
            "slice_type {} (only I slices in subset)",
            hdr.slice_type
        ));
    }
    if pps.output_flag_present {
        let _pic_output = br.u1();
    }
    if sps.separate_colour_plane {
        let _plane = br.u(2);
    }
    if !is_idr(nal_type) {
        let _poc_lsb = br.u(sps.log2_max_pic_order_cnt_lsb);
        let st_sps_flag = br.u1() == 1;
        if !st_sps_flag {
            let idx = sps.num_short_term_ref_pic_sets;
            parse_st_ref_pic_set(
                &mut br,
                idx,
                sps.num_short_term_ref_pic_sets,
                &sps.st_ref_pic_sets,
            )?;
        } else if sps.num_short_term_ref_pic_sets > 1 {
            let bits = 32 - (sps.num_short_term_ref_pic_sets as u32).leading_zeros();
            let _idx = br.u(bits);
        }
        if false {
            // long-term path unreachable for IRAP stills in subset
        }
        let _tmvp = if sps.temporal_mvp_enabled { br.u1() } else { 0 };
        let _ = _tmvp;
    }
    if sps.sao_enabled {
        hdr.sao_luma = br.u1() == 1;
        if sps.chroma_format_idc != 0 {
            hdr.sao_chroma = br.u1() == 1;
        }
    }
    // I slices: no ref-idx overrides / merge fields (§7.3.6.1 puts
    // them under slice_type != I), except cabac_init is not present
    // for I either (it is inside the same != I block? No — the
    // cabac_init_flag sits inside `if( slice_type == P || B )` in
    // effect via cabac_init_present && type!=I derivation).
    // slice_qp_delta is present for every slice type:
    let slice_qp_delta = br.se();
    hdr.slice_qp += slice_qp_delta;
    if pps.slice_chroma_qp_offsets_present {
        hdr.slice_cb_qp_offset = br.se();
        hdr.slice_cr_qp_offset = br.se();
    }
    if false {
        // cu_chroma_qp_offset_enabled_flag is a range-extension
        // (pps_scc/range) element; not present in Main-profile PPS.
    }
    // deblocking override (§7.3.6.1): the PPS-level enable gates a
    // per-slice override FLAG; the disabled flag and offsets are
    // present only when that flag is set.
    let deblocking_override =
        pps.deblocking_filter_control_present && pps.deblocking_filter_override_enabled;
    if deblocking_override {
        let override_flag = br.u1() == 1;
        if override_flag {
            hdr.deblocking_disabled = br.u1() == 1;
            if !hdr.deblocking_disabled {
                hdr.beta_offset_div2 = br.se();
                hdr.tc_offset_div2 = br.se();
            }
        }
    }
    if pps.loop_filter_across_slices_enabled
        && (hdr.sao_luma || hdr.sao_chroma || !hdr.deblocking_disabled)
    {
        hdr.loop_filter_across_slices = br.u1() == 1;
    }
    if pps.tiles_enabled || pps.entropy_coding_sync_enabled {
        hdr.num_entry_point_offsets = br.ue();
        // M5 untrusted-input bound: entry points cannot exceed the
        // CTB count of the largest in-ceiling picture (2^28 samples
        // at the 16x16 minimum CTB); an unbounded count over a
        // saturating reader is a hang.
        if hdr.num_entry_point_offsets > 1_048_576 {
            return malformed("num_entry_point_offsets beyond picture capacity");
        }
        if hdr.num_entry_point_offsets > 0 {
            let offset_len_minus1 = br.ue();
            let len = offset_len_minus1 + 1;
            if len > 32 {
                return malformed("entry_point offset_len > 32");
            }
            for _ in 0..hdr.num_entry_point_offsets {
                hdr.entry_point_offset_minus1.push(br.u(len));
            }
        }
    }
    if pps.slice_segment_header_extension_present {
        let len = br.ue();
        for _ in 0..len {
            let _b = br.u(8);
        }
    }
    // byte_alignment()
    let align_bit = br.u1();
    if align_bit != 1 {
        return malformed("slice header alignment bit != 1");
    }
    while !br.byte_aligned() {
        let b = br.u1();
        if b != 0 {
            return malformed("slice header alignment zero bit != 0");
        }
    }
    hdr.cabac_start_bit = br.bits_read();
    Ok((hdr, pps))
}

#[cfg(test)]
mod sps_log2_range_tests {
    use super::check_sps_log2_ranges;

    /// The honest envelope (iPhone/Canon streams: POC lsb 16,
    /// MinCb 16, Ctb 64, MinTb 4, MaxTb 32) passes.
    #[test]
    fn honest_ranges_pass() {
        assert!(check_sps_log2_ranges(0, 1, 2, 0, 3).is_ok());
        assert!(check_sps_log2_ranges(12, 0, 1, 0, 0).is_ok());
        // Largest legal combination on every axis at once.
        assert!(check_sps_log2_ranges(12, 3, 0, 3, 0).is_ok());
    }

    /// Each field one step past its spec range refuses — the
    /// crafted-SPS shape the shift-trap audit flagged (a ~2^32-bit
    /// POC field above all: every read of it is a CPU sink).
    #[test]
    fn one_past_each_range_refuses() {
        assert!(check_sps_log2_ranges(13, 1, 2, 0, 3).is_err());
        assert!(check_sps_log2_ranges(u32::MAX - 4, 1, 2, 0, 3).is_err());
        assert!(check_sps_log2_ranges(0, 4, 0, 0, 3).is_err()); // MinCb 128
        assert!(check_sps_log2_ranges(0, 1, 3, 0, 3).is_err()); // Ctb 128
        assert!(check_sps_log2_ranges(0, 0, 0, 0, 3).is_err()); // Ctb 8 < 16
        assert!(check_sps_log2_ranges(0, 1, 2, 4, 0).is_err()); // MinTb 64
        assert!(check_sps_log2_ranges(0, 1, 2, 0, 4).is_err()); // MaxTb 64
                                                                // Absurd ue() values refuse cleanly instead of overflowing
                                                                // the checker's own arithmetic.
        assert!(check_sps_log2_ranges(0, 1, u32::MAX - 1, 0, 3).is_err());
        assert!(check_sps_log2_ranges(0, 1, 2, 0, u32::MAX - 1).is_err());
    }
}

#[cfg(test)]
mod scaling_list_consumption_tests {
    use super::{parse_scaling_list_data, BitReader};

    /// Minimal MSB-first bit writer for building scaling_list_data
    /// streams whose exact bit cost is known by construction.
    struct BitW {
        bytes: Vec<u8>,
        cur: u8,
        used: u32,
    }

    impl BitW {
        fn new() -> Self {
            BitW {
                bytes: Vec::new(),
                cur: 0,
                used: 0,
            }
        }
        fn u1(&mut self, bit: u32) {
            self.cur = (self.cur << 1) | (bit as u8 & 1);
            self.used += 1;
            if self.used == 8 {
                self.bytes.push(self.cur);
                self.cur = 0;
                self.used = 0;
            }
        }
        /// ue(0) and se(0) are both the single bit "1".
        fn zero_golomb(&mut self) {
            self.u1(1);
        }
        /// se(+1): codeNum 1, ue "010".
        fn se_plus_one(&mut self) {
            self.u1(0);
            self.u1(1);
            self.u1(0);
        }
        fn finish(mut self) -> Vec<u8> {
            if self.used > 0 {
                self.cur <<= 8 - self.used;
                self.bytes.push(self.cur);
            }
            self.bytes
        }
    }

    /// The two §7.3.4 rules the garment batch (IMG_1433-1436) exposed:
    /// pred_mode_flag 0 = copy (ue delta), 1 = explicit coefficients;
    /// coefNum = Min(64, 1 << (4 + (sizeId << 1))). The stream below
    /// mixes both modes across all four sizes; the parser must land
    /// on the exact constructed bit count and values. Under either
    /// old deviation (inverted branches, or a 32-coefficient size-1
    /// matrix) the parse desyncs and both assertions break.
    #[test]
    fn explicit_and_copy_matrices_consume_exact_bits() {
        let mut w = BitW::new();
        // size 0 (6 matrices of 16): m0 explicit, first delta +1,
        // rest 0; m1..m5 copies of the previous with delta 0.
        w.u1(1);
        w.se_plus_one();
        for _ in 0..15 {
            w.zero_golomb();
        }
        for _ in 0..5 {
            w.u1(0);
            w.zero_golomb();
        }
        // size 1 (6 matrices of 64 by the spec count): m0 explicit
        // all-zero deltas; m1..m5 copies.
        w.u1(1);
        for _ in 0..64 {
            w.zero_golomb();
        }
        for _ in 0..5 {
            w.u1(0);
            w.zero_golomb();
        }
        // size 2 (6 matrices of 64 + DC): m0 explicit, DC delta 0,
        // coefficients all zero deltas; m1..m5 copies.
        w.u1(1);
        w.zero_golomb(); // DC coef minus8 = 0
        for _ in 0..64 {
            w.zero_golomb();
        }
        for _ in 0..5 {
            w.u1(0);
            w.zero_golomb();
        }
        // size 3 (2 matrices): both copies (delta 0).
        for _ in 0..2 {
            w.u1(0);
            w.zero_golomb();
        }
        // Bit budget: size0 19 + 5*2, size1 65 + 5*2, size2 66 + 5*2,
        // size3 2*2.
        let expected_bits = 19 + 10 + 65 + 10 + 66 + 10 + 4;
        let bytes = w.finish();
        let mut br = BitReader::new(&bytes);
        let lists = parse_scaling_list_data(&mut br).expect("parse");
        assert_eq!(br.bits_read(), expected_bits);
        // Values: explicit matrices carry the deltas; copies repeat
        // their reference; the size-1 matrix really has 64 entries.
        assert!(lists.coef[0][0].iter().all(|&c| c == 9));
        assert_eq!(lists.coef[0][1], lists.coef[0][0]);
        assert_eq!(lists.coef[1][0].len(), 64);
        assert!(lists.coef[1][0].iter().all(|&c| c == 8));
        assert_eq!(lists.dc[0][0], 8);
        assert_eq!(lists.coef[2][1], lists.coef[2][0]);
    }
}
