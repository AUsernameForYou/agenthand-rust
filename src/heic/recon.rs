//! M3 reconstruction: retained slice syntax -> picture samples.
//!
//! Pipeline per TU, in retained (decoding) order: intra prediction
//! (§8.4.4, per transform block, mode of the PB at the TU origin)
//! from already-reconstructed samples, then dequantization
//! (§8.6.3) and the inverse transform (§8.6.4) added to the
//! prediction with clipping. Deblocking (§8.7.2) and SAO (§8.7.3)
//! are the second stage (`decode_picture` with filters on).
//!
//! Arithmetic follows the staged oracle (libde265 1.1.3 fallback
//! paths) bit for bit where the spec leaves implementation room:
//! transform matrices/shifts, dequant rounding, reference-sample
//! substitution and filtering, angular interpolation. No oracle
//! code is linked or copied at runtime; the tables below are the
//! spec's own constants (H.265 §8.6.4 transform matrices, §8.4.4
//! angle tables).

use sha2::{Digest, Sha256};

use crate::heic::params::{Pps, ScalingLists, SliceHeader, Sps};
use crate::heic::syntax::RetainedSlice;

/// A decoded picture (one slice segment's worth: a whole picture
/// for standalone items, one tile picture for grid tiles). Planes
/// are coded-size; samples are native-range values.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub bit_depth_luma: u32,
    pub bit_depth_chroma: u32,
    pub y: Vec<u16>,
    pub cb: Vec<u16>,
    pub cr: Vec<u16>,
}

impl Picture {
    pub fn plane(&self, cidx: usize) -> &[u16] {
        match cidx {
            0 => &self.y,
            1 => &self.cb,
            _ => &self.cr,
        }
    }

    /// SHA-256 of one plane's conformance-cropped samples, rows
    /// tightly packed: 1 byte per sample at 8-bit depth, 2 bytes
    /// little-endian (value in the low bits) above 8 bits. The
    /// gate-5 comparison layout.
    pub fn plane_hash(&self, sps: &Sps, cidx: usize) -> String {
        let (cw, ch) = if cidx == 0 {
            (sps.crop_width(), sps.crop_height())
        } else {
            (sps.crop_width() / 2, sps.crop_height() / 2)
        };
        let (fw, bd) = if cidx == 0 {
            (sps.width, sps.bit_depth_luma())
        } else {
            (sps.width / 2, sps.bit_depth_chroma())
        };
        let plane = self.plane(cidx);
        let mut hasher = Sha256::new();
        for row in 0..ch {
            let start = (row * fw) as usize;
            let line = &plane[start..start + cw as usize];
            if bd <= 8 {
                let bytes: Vec<u8> = line.iter().map(|&v| v as u8).collect();
                hasher.update(bytes);
            } else {
                let mut bytes = Vec::with_capacity(line.len() * 2);
                for &v in line {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
                hasher.update(bytes);
            }
        }
        let digest = hasher.finalize();
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }
}

const LEVEL_SCALE: [i64; 6] = [40, 45, 51, 57, 64, 72];

/// Spec default scaling lists in diagonal-scan order (§7.4.5
/// defaults, as tabulated in the oracle's sps.cc).
const DEFAULT_4X4: [i32; 16] = [16; 16];
const DEFAULT_8X8_INTRA: [i32; 64] = [
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 16, 17, 16, 17, 18, 17, 18, 18, 17, 18, 21, 19, 20,
    21, 20, 19, 21, 24, 22, 22, 24, 24, 22, 22, 24, 25, 25, 27, 30, 27, 25, 25, 29, 31, 35, 35, 31,
    29, 36, 41, 44, 41, 36, 47, 54, 54, 47, 65, 70, 65, 88, 88, 115,
];
const DEFAULT_8X8_INTER: [i32; 64] = [
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 17, 17, 17, 17, 18, 18, 18, 18, 18, 18, 20, 20, 20,
    20, 20, 20, 20, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 28, 28, 28, 28, 28,
    28, 33, 33, 33, 33, 33, 41, 41, 41, 41, 54, 54, 54, 71, 71, 91,
];

/// Diagonal scan order positions for an n x n block: scan index ->
/// (x, y). The HEVC up-right diagonal scan.
fn diag_scan(n: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(n * n);
    for s in 0..=(2 * n - 2) {
        let lo = s.saturating_sub(n - 1);
        let hi = s.min(n - 1);
        for x in lo..=hi {
            out.push((x, s - x));
        }
    }
    out
}

/// Scaling factor (raster order, n*n) for one TU (§8.6.2).
fn scaling_factors(sps: &Sps, pps: &Pps, log2: u32, cidx: usize) -> Vec<i32> {
    let n = 1usize << log2;
    if !sps.scaling_list_enabled {
        return vec![16; n * n];
    }
    let size_id = (log2 - 2) as usize;
    let matrix_id = if size_id == 3 { 0 } else { cidx };
    let lists: Option<&ScalingLists> = pps.scaling_lists.as_ref().or(sps.scaling_lists.as_ref());
    let (scan_list, dc): (Vec<i32>, i32) = match lists {
        Some(l) => {
            let dc = if size_id >= 2 && matrix_id < 2 {
                l.dc[size_id - 2][matrix_id]
            } else {
                16
            };
            (l.coef[size_id][matrix_id].clone(), dc)
        }
        None => {
            // Enabled but no data anywhere: the spec defaults.
            let t: &[i32] = match size_id {
                0 => &DEFAULT_4X4,
                _ => &DEFAULT_8X8_INTRA,
            };
            (t.to_vec(), 16)
        }
    };
    let mut out = vec![16i32; n * n];
    if size_id <= 1 {
        let scan = diag_scan(n);
        for (idx, &(x, y)) in scan.iter().enumerate() {
            out[y * n + x] = scan_list[idx];
        }
    } else {
        let sub = if size_id == 2 { 1usize } else { 2usize }; // log2 of replication block - 1
        let scan8 = diag_scan(8);
        let mut list8 = [0i32; 64];
        for (idx, &(x, y)) in scan8.iter().enumerate() {
            list8[y * 8 + x] = scan_list[idx];
        }
        for y in 0..n {
            for x in 0..n {
                out[y * n + x] = list8[(y >> sub) * 8 + (x >> sub)];
            }
        }
        out[0] = dc;
    }
    out
}

/// Dequantize one TU's levels (§8.6.3) into coefficients.
fn dequant(
    tu_levels: &[i32],
    factors: &[i32],
    qp_prime: i32,
    bit_depth: u32,
    log2: u32,
) -> Vec<i32> {
    let bd_shift = bit_depth as i32 + log2 as i32 - 5;
    let offset: i64 = 1 << (bd_shift - 1);
    let ls = LEVEL_SCALE[(qp_prime % 6) as usize];
    let shift_q = qp_prime / 6;
    tu_levels
        .iter()
        .zip(factors.iter())
        .map(|(&level, &f)| {
            let fact: i64 = (f as i64 * ls) << shift_q;
            let v = (level as i64 * fact + offset) >> bd_shift;
            v.clamp(-32768, 32767) as i32
        })
        .collect()
}

#[allow(dead_code)]
/// The 32x32 inverse-DCT matrix (§8.6.4.2); smaller sizes sample
/// it with stride 1 << (5 - log2).
#[rustfmt::skip]
const MAT_DCT: [[i32; 32]; 32] = [
    [64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64],
    [90, 90, 88, 85, 82, 78, 73, 67, 61, 54, 46, 38, 31, 22, 13, 4, -4, -13, -22, -31, -38, -46, -54, -61, -67, -73, -78, -82, -85, -88, -90, -90],
    [90, 87, 80, 70, 57, 43, 25, 9, -9, -25, -43, -57, -70, -80, -87, -90, -90, -87, -80, -70, -57, -43, -25, -9, 9, 25, 43, 57, 70, 80, 87, 90],
    [90, 82, 67, 46, 22, -4, -31, -54, -73, -85, -90, -88, -78, -61, -38, -13, 13, 38, 61, 78, 88, 90, 85, 73, 54, 31, 4, -22, -46, -67, -82, -90],
    [89, 75, 50, 18, -18, -50, -75, -89, -89, -75, -50, -18, 18, 50, 75, 89, 89, 75, 50, 18, -18, -50, -75, -89, -89, -75, -50, -18, 18, 50, 75, 89],
    [88, 67, 31, -13, -54, -82, -90, -78, -46, -4, 38, 73, 90, 85, 61, 22, -22, -61, -85, -90, -73, -38, 4, 46, 78, 90, 82, 54, 13, -31, -67, -88],
    [87, 57, 9, -43, -80, -90, -70, -25, 25, 70, 90, 80, 43, -9, -57, -87, -87, -57, -9, 43, 80, 90, 70, 25, -25, -70, -90, -80, -43, 9, 57, 87],
    [85, 46, -13, -67, -90, -73, -22, 38, 82, 88, 54, -4, -61, -90, -78, -31, 31, 78, 90, 61, 4, -54, -88, -82, -38, 22, 73, 90, 67, 13, -46, -85],
    [83, 36, -36, -83, -83, -36, 36, 83, 83, 36, -36, -83, -83, -36, 36, 83, 83, 36, -36, -83, -83, -36, 36, 83, 83, 36, -36, -83, -83, -36, 36, 83],
    [82, 22, -54, -90, -61, 13, 78, 85, 31, -46, -90, -67, 4, 73, 88, 38, -38, -88, -73, -4, 67, 90, 46, -31, -85, -78, -13, 61, 90, 54, -22, -82],
    [80, 9, -70, -87, -25, 57, 90, 43, -43, -90, -57, 25, 87, 70, -9, -80, -80, -9, 70, 87, 25, -57, -90, -43, 43, 90, 57, -25, -87, -70, 9, 80],
    [78, -4, -82, -73, 13, 85, 67, -22, -88, -61, 31, 90, 54, -38, -90, -46, 46, 90, 38, -54, -90, -31, 61, 88, 22, -67, -85, -13, 73, 82, 4, -78],
    [75, -18, -89, -50, 50, 89, 18, -75, -75, 18, 89, 50, -50, -89, -18, 75, 75, -18, -89, -50, 50, 89, 18, -75, -75, 18, 89, 50, -50, -89, -18, 75],
    [73, -31, -90, -22, 78, 67, -38, -90, -13, 82, 61, -46, -88, -4, 85, 54, -54, -85, 4, 88, 46, -61, -82, 13, 90, 38, -67, -78, 22, 90, 31, -73],
    [70, -43, -87, 9, 90, 25, -80, -57, 57, 80, -25, -90, -9, 87, 43, -70, -70, 43, 87, -9, -90, -25, 80, 57, -57, -80, 25, 90, 9, -87, -43, 70],
    [67, -54, -78, 38, 85, -22, -90, 4, 90, 13, -88, -31, 82, 46, -73, -61, 61, 73, -46, -82, 31, 88, -13, -90, -4, 90, 22, -85, -38, 78, 54, -67],
    [64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64],
    [61, -73, -46, 82, 31, -88, -13, 90, -4, -90, 22, 85, -38, -78, 54, 67, -67, -54, 78, 38, -85, -22, 90, 4, -90, 13, 88, -31, -82, 46, 73, -61],
    [57, -80, -25, 90, -9, -87, 43, 70, -70, -43, 87, 9, -90, 25, 80, -57, -57, 80, 25, -90, 9, 87, -43, -70, 70, 43, -87, -9, 90, -25, -80, 57],
    [54, -85, -4, 88, -46, -61, 82, 13, -90, 38, 67, -78, -22, 90, -31, -73, 73, 31, -90, 22, 78, -67, -38, 90, -13, -82, 61, 46, -88, 4, 85, -54],
    [50, -89, 18, 75, -75, -18, 89, -50, -50, 89, -18, -75, 75, 18, -89, 50, 50, -89, 18, 75, -75, -18, 89, -50, -50, 89, -18, -75, 75, 18, -89, 50],
    [46, -90, 38, 54, -90, 31, 61, -88, 22, 67, -85, 13, 73, -82, 4, 78, -78, -4, 82, -73, -13, 85, -67, -22, 88, -61, -31, 90, -54, -38, 90, -46],
    [43, -90, 57, 25, -87, 70, 9, -80, 80, -9, -70, 87, -25, -57, 90, -43, -43, 90, -57, -25, 87, -70, -9, 80, -80, 9, 70, -87, 25, 57, -90, 43],
    [38, -88, 73, -4, -67, 90, -46, -31, 85, -78, 13, 61, -90, 54, 22, -82, 82, -22, -54, 90, -61, -13, 78, -85, 31, 46, -90, 67, 4, -73, 88, -38],
    [36, -83, 83, -36, -36, 83, -83, 36, 36, -83, 83, -36, -36, 83, -83, 36, 36, -83, 83, -36, -36, 83, -83, 36, 36, -83, 83, -36, -36, 83, -83, 36],
    [31, -78, 90, -61, 4, 54, -88, 82, -38, -22, 73, -90, 67, -13, -46, 85, -85, 46, 13, -67, 90, -73, 22, 38, -82, 88, -54, -4, 61, -90, 78, -31],
    [25, -70, 90, -80, 43, 9, -57, 87, -87, 57, -9, -43, 80, -90, 70, -25, -25, 70, -90, 80, -43, -9, 57, -87, 87, -57, 9, 43, -80, 90, -70, 25],
    [22, -61, 85, -90, 73, -38, -4, 46, -78, 90, -82, 54, -13, -31, 67, -88, 88, -67, 31, 13, -54, 82, -90, 78, -46, 4, 38, -73, 90, -85, 61, -22],
    [18, -50, 75, -89, 89, -75, 50, -18, -18, 50, -75, 89, -89, 75, -50, 18, 18, -50, 75, -89, 89, -75, 50, -18, -18, 50, -75, 89, -89, 75, -50, 18],
    [13, -38, 61, -78, 88, -90, 85, -73, 54, -31, 4, 22, -46, 67, -82, 90, -90, 82, -67, 46, -22, -4, 31, -54, 73, -85, 90, -88, 78, -61, 38, -13],
    [9, -25, 43, -57, 70, -80, 87, -90, 90, -87, 80, -70, 57, -43, 25, -9, -9, 25, -43, 57, -70, 80, -87, 90, -90, 87, -80, 70, -57, 43, -25, 9],
    [4, -13, 22, -31, 38, -46, 54, -61, 67, -73, 78, -82, 85, -88, 90, -90, 90, -90, 88, -85, 82, -78, 73, -67, 61, -54, 46, -38, 31, -22, 13, -4],
];

/// The 4x4 inverse DST-VII matrix (§8.6.4.1).
const MAT_DST4: [[i32; 4]; 4] = [
    [29, 55, 74, 84],
    [74, 74, 0, -74],
    [84, -29, -74, 55],
    [55, -84, 74, -29],
];

/// Two-pass inverse transform (§8.6.4) of dequantized coefficients
/// into residual samples. `dst4` selects the DST-VII (4x4 intra
/// luma only). Mirrors the oracle's fallback arithmetic: vertical
/// pass with rounding 1<<6 and shift 7, clipped to i16 range;
/// horizontal pass with shift 20 - bit_depth.
fn inv_transform(coeffs: &[i32], log2: u32, bit_depth: u32, dst4: bool) -> Vec<i32> {
    let n = 1usize << log2;
    let bd_shift = 20 - bit_depth as i32;
    let rnd_h: i32 = 1 << (bd_shift - 1);
    let mut g = vec![0i32; n * n];
    if dst4 {
        for c in 0..4 {
            for i in 0..4 {
                let mut sum: i32 = 0;
                for j in 0..4 {
                    sum += MAT_DST4[j][i] * coeffs[c + j * 4];
                }
                g[c + i * 4] = ((sum + 64) >> 7).clamp(-32768, 32767);
            }
        }
        let mut out = vec![0i32; 16];
        for y in 0..4 {
            for i in 0..4 {
                let mut sum: i32 = 0;
                for j in 0..4 {
                    sum += MAT_DST4[j][i] * g[y * 4 + j];
                }
                out[y * 4 + i] = (sum + rnd_h) >> bd_shift;
            }
        }
        return out;
    }
    let fact = 1usize << (5 - log2);
    for c in 0..n {
        for i in 0..n {
            let mut sum: i32 = 0;
            for j in 0..n {
                sum += MAT_DCT[fact * j][i] * coeffs[c + j * n];
            }
            g[c + i * n] = ((sum + 64) >> 7).clamp(-32768, 32767);
        }
    }
    let mut out = vec![0i32; n * n];
    for y in 0..n {
        for i in 0..n {
            let mut sum: i32 = 0;
            for j in 0..n {
                sum += MAT_DCT[fact * j][i] * g[y * n + j];
            }
            out[y * n + i] = (sum + rnd_h) >> bd_shift;
        }
    }
    out
}

/// Transform-skip residual (§8.6.4.3 via the oracle's fallback):
/// residual = (coeff << (5 + log2) + rnd) >> (20 - bit_depth).
fn inv_transform_skip(coeffs: &[i32], log2: u32, bit_depth: u32) -> Vec<i32> {
    let bd_shift = 20 - bit_depth as i32;
    let rnd: i32 = 1 << (bd_shift - 1);
    let ts_shift = 5 + log2 as i32;
    coeffs
        .iter()
        .map(|&c| ((c << ts_shift) + rnd) >> bd_shift)
        .collect()
}

const INTRA_ANGLE: [i32; 35] = [
    0, 0, 32, 26, 21, 17, 13, 9, 5, 2, 0, -2, -5, -9, -13, -17, -21, -26, -32, -26, -21, -17, -13,
    -9, -5, -2, 0, 2, 5, 9, 13, 17, 21, 26, 32,
];
const INV_ANGLE: [i32; 15] = [
    -4096, -1638, -910, -630, -482, -390, -315, -256, -315, -390, -482, -630, -910, -1638, -4096,
];

pub struct Plane {
    pub data: Vec<i32>,
    pub done: Vec<bool>,
    pub w: usize,
    pub h: usize,
}

impl Plane {
    fn new(w: usize, h: usize) -> Self {
        Plane {
            data: vec![0; w * h],
            done: vec![false; w * h],
            w,
            h,
        }
    }
    #[inline]
    fn avail(&self, x: i32, y: i32) -> bool {
        x >= 0
            && y >= 0
            && (x as usize) < self.w
            && (y as usize) < self.h
            && self.done[y as usize * self.w + x as usize]
    }
    #[inline]
    fn at(&self, x: i32, y: i32) -> i32 {
        self.data[y as usize * self.w + x as usize]
    }
}

/// Gather the 4n+1 reference samples around a block (§8.4.4.2.1),
/// indexed like the oracle's border array: index 0 is the top-left
/// corner, positive indices run along the top row (to 2n), negative
/// indices down the left column (to -2n). Returned as a Vec with
/// index offset 2n, after substitution.
fn gather_refs(p: &Plane, x0: i32, y0: i32, n: i32, bit_depth: u32) -> Vec<i32> {
    let mut vals = vec![0i32; 4 * n as usize + 1];
    let mut avail = vec![false; 4 * n as usize + 1];
    let mut n_avail = 0usize;
    let mut first_value: Option<i32> = None;
    // Fill order (for the substitution seed): left column bottom to
    // top (indices -2n..-1), corner (0), top row (1..2n).
    for k in (-(2 * n))..=(2 * n) {
        let (sx, sy) = if k == 0 {
            (x0 - 1, y0 - 1)
        } else if k > 0 {
            (x0 + k - 1, y0 - 1)
        } else {
            (x0 - 1, y0 + (-k) - 1)
        };
        let idx = (k + 2 * n) as usize;
        if p.avail(sx, sy) {
            vals[idx] = p.at(sx, sy);
            avail[idx] = true;
            n_avail += 1;
            if first_value.is_none() {
                first_value = Some(vals[idx]);
            }
        }
    }
    if n_avail == 0 {
        return vec![1 << (bit_depth - 1); 4 * n as usize + 1];
    }
    if !avail[0] {
        vals[0] = first_value.unwrap();
    }
    for i in 1..vals.len() {
        if !avail[i] {
            vals[i] = vals[i - 1];
        }
    }
    vals
}

/// Reference-sample filtering (§8.4.4.2.2), luma only in 4:2:0.
fn filter_refs(refs: &mut [i32], n: i32, mode: u32, bit_depth: u32, strong_smoothing: bool) {
    if mode == 1 || n == 4 {
        return;
    }
    let min_dist = (mode as i32 - 26).abs().min((mode as i32 - 10).abs());
    let flag = match n {
        8 => min_dist > 7,
        16 => min_dist > 1,
        32 => min_dist > 0,
        _ => false,
    };
    if !flag {
        return;
    }
    let off = (2 * n) as usize;
    let strong = strong_smoothing
        && n == 32
        && (refs[off] + refs[off + 64] - 2 * refs[off + 32]).abs() < (1 << (bit_depth - 5))
        && (refs[off] + refs[off - 64] - 2 * refs[off - 32]).abs() < (1 << (bit_depth - 5));
    let src = refs.to_vec();
    if strong {
        for i in 1..=63usize {
            refs[off - i] = src[off] + ((i as i32 * (src[off - 64] - src[off]) + 32) >> 6);
            refs[off + i] = src[off] + ((i as i32 * (src[off + 64] - src[off]) + 32) >> 6);
        }
    } else {
        let len = refs.len();
        for i in 1..len - 1 {
            refs[i] = (src[i + 1] + 2 * src[i] + src[i - 1] + 2) >> 2;
        }
    }
}

/// Intra prediction for one transform block (§8.4.4), written into
/// the plane (the residual is added by the caller).
#[allow(clippy::too_many_arguments)]
fn predict(
    p: &mut Plane,
    x0: i32,
    y0: i32,
    log2: u32,
    mode: u32,
    cidx: usize,
    bit_depth: u32,
    strong_smoothing: bool,
) {
    let n = 1i32 << log2;
    let refs = gather_refs(p, x0, y0, n, bit_depth);
    let mut refs = refs;
    if cidx == 0 {
        filter_refs(&mut refs, n, mode, bit_depth, strong_smoothing);
    }
    let r = |k: i32| -> i32 { refs[(k + 2 * n) as usize] };
    let maxv = (1i32 << bit_depth) - 1;
    let w = p.w;
    let set = |p: &mut Plane, x: i32, y: i32, v: i32| {
        p.data[(y0 + y) as usize * w + (x0 + x) as usize] = v;
    };
    match mode {
        0 => {
            // Planar.
            for y in 0..n {
                for x in 0..n {
                    let v = ((n - 1 - x) * r(-1 - y)
                        + (x + 1) * r(1 + n)
                        + (n - 1 - y) * r(1 + x)
                        + (y + 1) * r(-1 - n)
                        + n)
                        >> (log2 + 1);
                    set(p, x, y, v);
                }
            }
        }
        1 => {
            // DC.
            let mut dc: i32 = 0;
            for i in 0..n {
                dc += r(i + 1) + r(-i - 1);
            }
            dc = (dc + n) >> (log2 + 1);
            for y in 0..n {
                for x in 0..n {
                    let v = if cidx == 0 && n < 32 {
                        if x == 0 && y == 0 {
                            (r(-1) + 2 * dc + r(1) + 2) >> 2
                        } else if y == 0 {
                            (r(x + 1) + 3 * dc + 2) >> 2
                        } else if x == 0 {
                            (r(-y - 1) + 3 * dc + 2) >> 2
                        } else {
                            dc
                        }
                    } else {
                        dc
                    };
                    set(p, x, y, v);
                }
            }
        }
        m => {
            // Angular (2..=34).
            let angle = INTRA_ANGLE[m as usize];
            // ref array in the oracle's layout: index by signed k.
            let mut refr = vec![0i32; 4 * n as usize + 1];
            if m >= 18 {
                for x in 0..=n {
                    refr[(x + 2 * n) as usize] = r(x);
                }
                if angle < 0 {
                    let inv = INV_ANGLE[(m - 11) as usize];
                    if (n * angle) >> 5 < -1 {
                        let mut x = (n * angle) >> 5;
                        while x <= -1 {
                            refr[(x + 2 * n) as usize] = r(-((x * inv + 128) >> 8));
                            x += 1;
                        }
                    }
                } else {
                    for x in (n + 1)..=(2 * n) {
                        refr[(x + 2 * n) as usize] = r(x);
                    }
                }
                let rf = |k: i32| -> i32 { refr[(k + 2 * n) as usize] };
                for y in 0..n {
                    for x in 0..n {
                        let iidx = ((y + 1) * angle) >> 5;
                        let ifact = ((y + 1) * angle) & 31;
                        let v = if ifact != 0 {
                            ((32 - ifact) * rf(x + iidx + 1) + ifact * rf(x + iidx + 2) + 16) >> 5
                        } else {
                            rf(x + iidx + 1)
                        };
                        set(p, x, y, v);
                    }
                }
                if m == 26 && cidx == 0 && n < 32 {
                    for y in 0..n {
                        let v = (r(1) + ((r(-1 - y) - r(0)) >> 1)).clamp(0, maxv);
                        set(p, 0, y, v);
                    }
                }
            } else {
                for x in 0..=n {
                    refr[(x + 2 * n) as usize] = r(-x);
                }
                if angle < 0 {
                    let inv = INV_ANGLE[(m - 11) as usize];
                    if (n * angle) >> 5 < -1 {
                        let mut x = (n * angle) >> 5;
                        while x <= -1 {
                            refr[(x + 2 * n) as usize] = r((x * inv + 128) >> 8);
                            x += 1;
                        }
                    }
                } else {
                    for x in (n + 1)..=(2 * n) {
                        refr[(x + 2 * n) as usize] = r(-x);
                    }
                }
                let rf = |k: i32| -> i32 { refr[(k + 2 * n) as usize] };
                for y in 0..n {
                    for x in 0..n {
                        let iidx = ((x + 1) * angle) >> 5;
                        let ifact = ((x + 1) * angle) & 31;
                        let v = if ifact != 0 {
                            ((32 - ifact) * rf(y + iidx + 1) + ifact * rf(y + iidx + 2) + 16) >> 5
                        } else {
                            rf(y + iidx + 1)
                        };
                        set(p, x, y, v);
                    }
                }
                if m == 10 && cidx == 0 && n < 32 {
                    for x in 0..n {
                        let v = (r(-1) + ((r(1 + x) - r(0)) >> 1)).clamp(0, maxv);
                        set(p, x, 0, v);
                    }
                }
            }
        }
    }
}

/// Decode one retained slice into a picture. `apply_filters` runs
/// deblocking + SAO (stage 2); the M3 first-image rung compares the
/// pre-filter stage.
pub fn decode_picture(
    sps: &Sps,
    pps: &Pps,
    hdr: &SliceHeader,
    retained: &RetainedSlice,
    apply_filters: bool,
) -> Picture {
    let _ = hdr;
    let w = sps.width as usize;
    let h = sps.height as usize;
    let bd_y = sps.bit_depth_luma();
    let bd_c = sps.bit_depth_chroma();
    let mut planes = [
        Plane::new(w, h),
        Plane::new(w / 2, h / 2),
        Plane::new(w / 2, h / 2),
    ];
    for ctb in &retained.ctbs {
        for cu in &ctb.cus {
            for tu in &cu.tus {
                let bd = if tu.cidx == 0 { bd_y } else { bd_c };
                // Mode of the PB containing this TU's origin (luma
                // coordinates for chroma TUs).
                let (lx, ly) = if tu.cidx == 0 {
                    (tu.x, tu.y)
                } else {
                    (tu.x * 2, tu.y * 2)
                };
                let mut luma_mode = 1u32;
                let mut chroma_mode = 1u32;
                for pb in &cu.pbs {
                    let sz = 1u32 << pb.log2;
                    if lx >= pb.x && lx < pb.x + sz && ly >= pb.y && ly < pb.y + sz {
                        luma_mode = pb.luma_mode;
                        chroma_mode = pb.chroma_mode;
                        break;
                    }
                }
                let mode = if tu.cidx == 0 { luma_mode } else { chroma_mode };
                let plane = &mut planes[tu.cidx];
                predict(
                    plane,
                    tu.x as i32,
                    tu.y as i32,
                    tu.log2,
                    mode,
                    tu.cidx,
                    bd,
                    sps.strong_intra_smoothing,
                );
                if tu.cbf {
                    let factors = scaling_factors(sps, pps, tu.log2, tu.cidx);
                    let coeffs = dequant(&tu.levels, &factors, tu.qp_prime, bd, tu.log2);
                    let residual = if tu.transform_skip {
                        inv_transform_skip(&coeffs, tu.log2, bd)
                    } else {
                        inv_transform(&coeffs, tu.log2, bd, tu.log2 == 2 && tu.cidx == 0)
                    };
                    let n = 1usize << tu.log2;
                    let maxv = (1i32 << bd) - 1;
                    for yy in 0..n {
                        for xx in 0..n {
                            let idx = (tu.y as usize + yy) * plane.w + (tu.x as usize + xx);
                            plane.data[idx] =
                                (plane.data[idx] + residual[yy * n + xx]).clamp(0, maxv);
                        }
                    }
                }
                // Mark the TU region reconstructed.
                let n = 1usize << tu.log2;
                for yy in 0..n {
                    for xx in 0..n {
                        let idx = (tu.y as usize + yy) * plane.w + (tu.x as usize + xx);
                        plane.done[idx] = true;
                    }
                }
            }
        }
    }
    if apply_filters {
        crate::heic::filters::apply_filters(sps, pps, hdr, retained, &mut planes);
    }
    let to_u16 = |p: &Plane| -> Vec<u16> { p.data.iter().map(|&v| v as u16).collect() };
    Picture {
        width: sps.crop_width(),
        height: sps.crop_height(),
        bit_depth_luma: bd_y,
        bit_depth_chroma: bd_c,
        y: to_u16(&planes[0]),
        cb: to_u16(&planes[1]),
        cr: to_u16(&planes[2]),
    }
}
