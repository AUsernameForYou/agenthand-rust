//! Stage 2 of M3 reconstruction: deblocking (§8.7.2) then SAO
//! (§8.7.3). Mirrors the staged oracle's structure (edge flags
//! from TU/PB boundaries, boundary strength, per-segment
//! decisions and kernels) — the subset here is intra-only, so
//! every marked edge has boundary strength 2.

use crate::heic::params::{Pps, SliceHeader, Sps};
use crate::heic::recon::Plane;
use crate::heic::syntax::RetainedSlice;
use crate::heic::values;

const BETA_TABLE: [i32; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18,
    20, 22, 24, 26, 28, 30, 32, 34, 36, 38, 40, 42, 44, 46, 48, 50, 52, 54, 56, 58, 60, 62, 64,
];
const TC_TABLE: [i32; 54] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 3,
    3, 3, 3, 4, 4, 4, 5, 5, 6, 6, 7, 8, 9, 10, 11, 13, 14, 16, 18, 20, 22, 24,
];

fn clip3(lo: i32, hi: i32, v: i32) -> i32 {
    v.clamp(lo, hi)
}

/// Edge flags + per-unit luma QP, derived from the retained walk.
struct EdgeInfo {
    /// Vertical-edge flags, indexed [y_unit][x_unit] in 4-sample
    /// luma units: flag at (x, y) means a candidate edge between
    /// luma columns x*4-1 and x*4 covering rows y*4..
    ver: Vec<bool>,
    hor: Vec<bool>,
    w4: usize,
    h4: usize,
    /// Unprimed QpY per 4x4 luma unit.
    qp: Vec<i32>,
}

fn derive_edges(sps: &Sps, retained: &RetainedSlice) -> EdgeInfo {
    let w = sps.width as usize;
    let h = sps.height as usize;
    let w4 = w.div_ceil(4);
    let h4 = h.div_ceil(4);
    let mut ver = vec![false; w4 * h4];
    let mut hor = vec![false; w4 * h4];
    let mut qp = vec![0i32; w4 * h4];
    for ctb in &retained.ctbs {
        for cu in &ctb.cus {
            let cu_size = 1usize << cu.log2;
            // PB internal edges (intra NxN split only in subset).
            if cu.pbs.len() == 4 {
                let half = cu_size / 2;
                for k in (0..cu_size).step_by(4) {
                    ver[((cu.y0 as usize + k) / 4) * w4 + (cu.x0 as usize + half) / 4] = true;
                    hor[((cu.y0 as usize + half) / 4) * w4 + (cu.x0 as usize + k) / 4] = true;
                }
            }
            for tu in &cu.tus {
                if tu.cidx != 0 {
                    continue;
                }
                let size = 1usize << tu.log2;
                // TU leaf edges: internal edges always filterable;
                // the CU's own left/top edges inherit the CU flag
                // (suppressed at the picture's left/top border in
                // this per-slice decode).
                let left_ok = tu.x != cu.x0 || cu.x0 > 0;
                let top_ok = tu.y != cu.y0 || cu.y0 > 0;
                if left_ok {
                    for k in (0..size).step_by(4) {
                        ver[((tu.y as usize + k) / 4) * w4 + (tu.x as usize) / 4] = true;
                    }
                }
                if top_ok {
                    for k in (0..size).step_by(4) {
                        hor[(tu.y as usize / 4) * w4 + (tu.x as usize + k) / 4] = true;
                    }
                }
            }
            // QP map: per CU, the CU-end QpY state over the whole
            // CU area — the oracle's set_QPY last-write value
            // (pred + the QG's delta if coded in this CU or an
            // earlier CU of the QG, else pred alone).
            {
                let q = cu.qp_y;
                let cs = cu_size;
                for yy in (cu.y0 as usize / 4)..((cu.y0 as usize + cs) / 4) {
                    for xx in (cu.x0 as usize / 4)..((cu.x0 as usize + cs) / 4) {
                        qp[yy * w4 + xx] = q;
                    }
                }
            }
        }
    }
    EdgeInfo {
        ver,
        hor,
        w4,
        h4,
        qp,
    }
}

impl EdgeInfo {
    #[inline]
    fn qp_at(&self, x: i32, y: i32) -> i32 {
        let ux = (x.max(0) as usize / 4).min(self.w4 - 1);
        let uy = (y.max(0) as usize / 4).min(self.h4 - 1);
        self.qp[uy * self.w4 + ux]
    }
}

/// One luma edge segment (4 lines): decision (§8.7.2.4.3) then the
/// strong/weak kernel (§8.7.2.4.4). `get`/`set` access the plane
/// along/across the edge; p-side samples are at negative offsets.
#[allow(clippy::too_many_arguments)]
fn deblock_luma_segment(
    plane: &mut Plane,
    vertical: bool,
    x: usize,
    y: usize,
    qp_p: i32,
    qp_q: i32,
    beta_offset: i32,
    tc_offset: i32,
    bit_depth: u32,
) {
    let w = plane.w;
    let at = |plane: &Plane, dx: i32, dy: i32| -> i32 {
        // vertical: dx across edge, dy along; horizontal: swapped
        let (px, py) = if vertical {
            (x as i32 + dx, y as i32 + dy)
        } else {
            (x as i32 + dy, y as i32 + dx)
        };
        plane.data[py as usize * w + px as usize]
    };
    // p[k][i]: k along the edge (0..4), i into the p side (0..4);
    // q[k][i]: into the q side.
    let mut p = [[0i32; 4]; 4];
    let mut q = [[0i32; 4]; 4];
    for k in 0..4i32 {
        for i in 0..4i32 {
            q[k as usize][i as usize] = at(plane, i, k);
            p[k as usize][i as usize] = at(plane, -i - 1, k);
        }
    }
    let qp_l = (qp_q + qp_p + 1) >> 1;
    let q_beta = clip3(0, 51, qp_l + beta_offset);
    let beta = BETA_TABLE[q_beta as usize] * (1 << (bit_depth - 8) as i32);
    let bs = 2i32;
    let q_tc = clip3(0, 53, qp_l + 2 * (bs - 1) + tc_offset);
    let tc = TC_TABLE[q_tc as usize] * (1 << (bit_depth - 8) as i32);

    let dp0 = (p[0][2] - 2 * p[0][1] + p[0][0]).abs();
    let dp3 = (p[3][2] - 2 * p[3][1] + p[3][0]).abs();
    let dq0 = (q[0][2] - 2 * q[0][1] + q[0][0]).abs();
    let dq3 = (q[3][2] - 2 * q[3][1] + q[3][0]).abs();
    let dpq0 = dp0 + dq0;
    let dpq3 = dp3 + dq3;
    let d = dpq0 + dpq3;
    if d >= beta {
        return;
    }
    let d_sam0 = 2 * dpq0 < (beta >> 2)
        && (p[0][3] - p[0][0]).abs() + (q[0][0] - q[0][3]).abs() < (beta >> 3)
        && (p[0][0] - q[0][0]).abs() < ((5 * tc + 1) >> 1);
    let d_sam3 = 2 * dpq3 < (beta >> 2)
        && (p[3][3] - p[3][0]).abs() + (q[3][0] - q[3][3]).abs() < (beta >> 3)
        && (p[3][0] - q[3][0]).abs() < ((5 * tc + 1) >> 1);
    let d_e = if d_sam0 && d_sam3 { 2 } else { 1 };
    let dp = dp0 + dp3;
    let dq = dq0 + dq3;
    let d_ep = dp < ((beta + (beta >> 1)) >> 3);
    let d_eq = dq < ((beta + (beta >> 1)) >> 3);

    let maxv = (1i32 << bit_depth) - 1;
    let set = |plane: &mut Plane, dx: i32, dy: i32, v: i32| {
        let (px, py) = if vertical {
            (x as i32 + dx, y as i32 + dy)
        } else {
            (x as i32 + dy, y as i32 + dx)
        };
        plane.data[py as usize * w + px as usize] = v;
    };
    for k in 0..4i32 {
        let (p0, p1, p2, p3) = (
            p[k as usize][0],
            p[k as usize][1],
            p[k as usize][2],
            p[k as usize][3],
        );
        let (q0, q1, q2, q3) = (
            q[k as usize][0],
            q[k as usize][1],
            q[k as usize][2],
            q[k as usize][3],
        );
        if d_e == 2 {
            let pn = [
                clip3(
                    p0 - 2 * tc,
                    p0 + 2 * tc,
                    (p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3,
                ),
                clip3(p1 - 2 * tc, p1 + 2 * tc, (p2 + p1 + p0 + q0 + 2) >> 2),
                clip3(
                    p2 - 2 * tc,
                    p2 + 2 * tc,
                    (2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3,
                ),
            ];
            let qn = [
                clip3(
                    q0 - 2 * tc,
                    q0 + 2 * tc,
                    (p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3,
                ),
                clip3(q1 - 2 * tc, q1 + 2 * tc, (p0 + q0 + q1 + q2 + 2) >> 2),
                clip3(
                    q2 - 2 * tc,
                    q2 + 2 * tc,
                    (p0 + q0 + q1 + 3 * q2 + 2 * q3 + 4) >> 3,
                ),
            ];
            for i in 0..3i32 {
                set(plane, -i - 1, k, pn[i as usize]);
                set(plane, i, k, qn[i as usize]);
            }
        } else {
            let delta = (9 * (q0 - p0) - 3 * (q1 - p1) + 8) >> 4;
            if delta.abs() < tc * 10 {
                let delta = clip3(-tc, tc, delta);
                set(plane, -1, k, (p0 + delta).clamp(0, maxv));
                set(plane, 0, k, (q0 - delta).clamp(0, maxv));
                if d_ep {
                    let delta_p = clip3(
                        -(tc >> 1),
                        tc >> 1,
                        (((p2 + p0 + 1) >> 1) - p1 + delta) >> 1,
                    );
                    set(plane, -2, k, (p1 + delta_p).clamp(0, maxv));
                }
                if d_eq {
                    let delta_q = clip3(
                        -(tc >> 1),
                        tc >> 1,
                        (((q2 + q0 + 1) >> 1) - q1 - delta) >> 1,
                    );
                    set(plane, 1, k, (q1 + delta_q).clamp(0, maxv));
                }
            }
        }
    }
}

/// One chroma edge segment (4 lines, §8.7.2.4.5): only the two
/// samples adjacent to the edge are adjusted.
#[allow(clippy::too_many_arguments)]
fn deblock_chroma_segment(
    plane: &mut Plane,
    vertical: bool,
    x: usize,
    y: usize,
    tc: i32,
    bit_depth: u32,
) {
    let w = plane.w;
    let at = |plane: &Plane, dx: i32, dy: i32| -> i32 {
        let (px, py) = if vertical {
            (x as i32 + dx, y as i32 + dy)
        } else {
            (x as i32 + dy, y as i32 + dx)
        };
        plane.data[py as usize * w + px as usize]
    };
    let maxv = (1i32 << bit_depth) - 1;
    for k in 0..4i32 {
        let q0 = at(plane, 0, k);
        let q1 = at(plane, 1, k);
        let p0 = at(plane, -1, k);
        let p1 = at(plane, -2, k);
        let delta = clip3(-tc, tc, (((q0 - p0) * 4) + p1 - q1 + 4) >> 3);
        let (pidx, qidx) = if vertical {
            (
                (y as i32 + k) as usize * w + (x - 1),
                (y as i32 + k) as usize * w + x,
            )
        } else {
            ((y - 1) * w + (x + k as usize), y * w + (x + k as usize))
        };
        plane.data[pidx] = (p0 + delta).clamp(0, maxv);
        plane.data[qidx] = (q0 - delta).clamp(0, maxv);
    }
}

fn deblock(
    sps: &Sps,
    pps: &Pps,
    hdr: &SliceHeader,
    retained: &RetainedSlice,
    planes: &mut [Plane; 3],
) {
    let edges = derive_edges(sps, retained);
    let beta_offset = hdr.beta_offset_div2 * 2;
    let tc_offset = hdr.tc_offset_div2 * 2;
    let bd_y = sps.bit_depth_luma();
    let bd_c = sps.bit_depth_chroma();
    let w = sps.width as usize;
    let h = sps.height as usize;

    // Luma: vertical edges first (whole picture), then horizontal.
    for vertical in [true, false] {
        if vertical {
            let mut x = 8usize;
            while x < w {
                let mut y = 0usize;
                while y < h {
                    if edges.ver[(y / 4) * edges.w4 + x / 4] {
                        let qp_p = edges.qp_at(x as i32 - 1, y as i32);
                        let qp_q = edges.qp_at(x as i32, y as i32);
                        deblock_luma_segment(
                            &mut planes[0],
                            true,
                            x,
                            y,
                            qp_p,
                            qp_q,
                            beta_offset,
                            tc_offset,
                            bd_y,
                        );
                    }
                    y += 4;
                }
                x += 8;
            }
        } else {
            let mut y = 8usize;
            while y < h {
                let mut x = 0usize;
                while x < w {
                    if edges.hor[(y / 4) * edges.w4 + x / 4] {
                        let qp_p = edges.qp_at(x as i32, y as i32 - 1);
                        let qp_q = edges.qp_at(x as i32, y as i32);
                        deblock_luma_segment(
                            &mut planes[0],
                            false,
                            x,
                            y,
                            qp_p,
                            qp_q,
                            beta_offset,
                            tc_offset,
                            bd_y,
                        );
                    }
                    x += 4;
                }
                y += 8;
            }
        }
    }

    // Chroma: edges on the 8-sample chroma grid, driven by the
    // luma flag/bS at the doubled position; bS == 2 required.
    let cw = w / 2;
    let ch = h / 2;
    for vertical in [true, false] {
        for cidx in 1..=2usize {
            let pps_off = if cidx == 1 {
                pps.cb_qp_offset
            } else {
                pps.cr_qp_offset
            };
            if vertical {
                let mut cx = 8usize;
                while cx < cw {
                    let mut cy = 0usize;
                    while cy < ch {
                        // luma flag at (2cx, 2cy)
                        if edges.ver[(2 * cy / 4) * edges.w4 + (2 * cx) / 4] {
                            let qp_q = edges.qp_at(2 * cx as i32, 2 * cy as i32);
                            let qp_p = edges.qp_at(2 * cx as i32 - 1, 2 * cy as i32);
                            let qp_i = ((qp_q + qp_p + 1) >> 1) + pps_off;
                            let qp_c = values::chroma_qp_from_qpi(qp_i);
                            let qq = clip3(0, 53, qp_c + 2 + tc_offset);
                            let tc = TC_TABLE[qq as usize] * (1 << (bd_c - 8) as i32);
                            deblock_chroma_segment(&mut planes[cidx], true, cx, cy, tc, bd_c);
                        }
                        cy += 4;
                    }
                    cx += 8;
                }
            } else {
                let mut cy = 8usize;
                while cy < ch {
                    let mut cx = 0usize;
                    while cx < cw {
                        if edges.hor[(2 * cy / 4) * edges.w4 + (2 * cx) / 4] {
                            let qp_q = edges.qp_at(2 * cx as i32, 2 * cy as i32);
                            let qp_p = edges.qp_at(2 * cx as i32, 2 * cy as i32 - 1);
                            let qp_i = ((qp_q + qp_p + 1) >> 1) + pps_off;
                            let qp_c = values::chroma_qp_from_qpi(qp_i);
                            let qq = clip3(0, 53, qp_c + 2 + tc_offset);
                            let tc = TC_TABLE[qq as usize] * (1 << (bd_c - 8) as i32);
                            deblock_chroma_segment(&mut planes[cidx], false, cx, cy, tc, bd_c);
                        }
                        cx += 4;
                    }
                    cy += 8;
                }
            }
        }
    }
}

pub fn apply_filters(
    sps: &Sps,
    pps: &Pps,
    hdr: &SliceHeader,
    retained: &RetainedSlice,
    planes: &mut [Plane; 3],
) {
    if !hdr.deblocking_disabled {
        deblock(sps, pps, hdr, retained, planes);
    }
    crate::heic::sao::apply_sao(sps, hdr, retained, planes);
}
