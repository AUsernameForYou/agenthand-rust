//! Prototype dimension estimator — port of the Python `measure.py`.
//!
//! EXPERIMENT ONLY, exactly as in the Python build: not a fourth MCP
//! tool, not wired into the server. A flat object lying in the same
//! plane as a printed agenthand card is photographed; this module
//! estimates the object's height and width from the card's tick
//! geometry via a seeded tick/corner homography.
//!
//! The port follows the Python algorithm step for step (same seeds,
//! same verification gates, same range propagation, same refusal
//! messages) with the OpenCV primitives it uses re-implemented here:
//! median blur + Gaussian adaptive threshold for the ink mask, HSV
//! ring anchoring, external contours for the sheet seed, Canny +
//! Hough lines for the fallback seed, a normalized-DLT homography
//! solver, and a deterministic RANSAC (OpenCV's is RNG-driven, so
//! inlier selection can differ on borderline points; the weighted
//! least-squares fit over the survivors is the same).
//!
//! `getRectSubPix` sampling is bilinear with round-half-up back to
//! 8-bit, verified against OpenCV. The pixels-across gate is the
//! Python one: below MIN_CARD_PX the module refuses outright, the
//! 130–170 px caution band widens the range by a systematic term,
//! and cards with a narrower demonstrated envelope (the AH-35 mini:
//! close-ups at ~190 px across or more) refuse outside it.

// Notation in this module follows the reference implementation it
// ports: `H` is always a homography, and the `H_*` names are its
// variants (inverse, refined, seeded, ...). Snake-casing them would
// make side-by-side reading against the Python/OpenCV original
// harder, so the naming lint is allowed module-wide.
#![allow(non_snake_case)]

use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Method identifier recorded in estimator output.
pub const METHOD: &str = "card-homography v0 (seeded tick/corner homography, prototype)";

/// Pixel floor: below this many pixels across the card, the estimator
/// refuses rather than guessing.
pub const MIN_CARD_PX: f64 = 130.0;
/// At or above this card width in pixels, estimates carry the standard
/// (narrower) ranges.
pub const COMFORT_CARD_PX: f64 = 170.0;
const SMALL_CARD_SYS_MAX: f64 = 0.055;

/// Per-card envelope floors: the universal floor is not enough for
/// every card. The 3x2 mini (AH-35) is measurably noisier than the
/// size it replaced; the one envelope the battery demonstrates as
/// dependable is close-ups at ~200 px across in clean light, so
/// AH-35 refuses below 190 px.
fn card_envelope_min_px(card_id: &str) -> Option<f64> {
    match card_id {
        "AH-35" => Some(190.0),
        _ => None,
    }
}

/// Default location of the card-profile JSON.
pub fn default_profile_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join("workspace/meta-hackathon/scale-card/print/layout-profile.json")
}

type Pt = [f64; 2];
type H3 = [f64; 9];

fn pt(x: f64, y: f64) -> Pt {
    [x, y]
}
fn add(a: Pt, b: Pt) -> Pt {
    [a[0] + b[0], a[1] + b[1]]
}
fn sub(a: Pt, b: Pt) -> Pt {
    [a[0] - b[0], a[1] - b[1]]
}
fn scale(a: Pt, s: f64) -> Pt {
    [a[0] * s, a[1] * s]
}
fn dot(a: Pt, b: Pt) -> f64 {
    a[0] * b[0] + a[1] * b[1]
}
fn norm(a: Pt) -> f64 {
    dot(a, a).sqrt()
}

// ---------------------------------------------------------------------------
// Python-compatible rounding / statistics helpers

/// numpy linspace (endpoint included).
fn linspace(start: f64, stop: f64, n: usize) -> Vec<f64> {
    if n == 1 {
        return vec![start];
    }
    let step = (stop - start) / (n - 1) as f64;
    let mut out: Vec<f64> = (0..n).map(|i| start + step * i as f64).collect();
    if let Some(last) = out.last_mut() {
        *last = stop;
    }
    out
}

/// numpy percentile, default linear interpolation.
fn percentile(values: &[f64], p: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let n = sorted.len();
    if n == 1 {
        return sorted[0];
    }
    let rank = p / 100.0 * (n - 1) as f64;
    let lo = rank.floor() as usize;
    let frac = rank - lo as f64;
    if lo + 1 < n {
        sorted[lo] + frac * (sorted[lo + 1] - sorted[lo])
    } else {
        sorted[lo]
    }
}

fn argmin_first(values: &[f64]) -> usize {
    let mut best = 0;
    for i in 1..values.len() {
        if values[i] < values[best] {
            best = i;
        }
    }
    best
}

fn argmax_first(values: &[f64]) -> usize {
    let mut best = 0;
    for i in 1..values.len() {
        if values[i] > values[best] {
            best = i;
        }
    }
    best
}

// ---------------------------------------------------------------------------
// Homography (normalized DLT, the cv2.findHomography method-0 model)

fn project(H: &H3, points: &[Pt]) -> Vec<Pt> {
    points
        .iter()
        .map(|&[x, y]| {
            let w = H[6] * x + H[7] * y + H[8];
            [
                (H[0] * x + H[1] * y + H[2]) / w,
                (H[3] * x + H[4] * y + H[5]) / w,
            ]
        })
        .collect()
}

fn project1(H: &H3, p: Pt) -> Pt {
    project(H, &[p])[0]
}

fn mat3_mul(a: &H3, b: &H3) -> H3 {
    let mut out = [0.0; 9];
    for r in 0..3 {
        for c in 0..3 {
            out[r * 3 + c] = a[r * 3] * b[c] + a[r * 3 + 1] * b[3 + c] + a[r * 3 + 2] * b[6 + c];
        }
    }
    out
}

fn mat3_inv(m: &H3) -> Option<H3> {
    let (a, b, c) = (m[0], m[1], m[2]);
    let (d, e, f) = (m[3], m[4], m[5]);
    let (g, h, i) = (m[6], m[7], m[8]);
    let det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
    if det == 0.0 || !det.is_finite() {
        return None;
    }
    let s = 1.0 / det;
    Some([
        (e * i - f * h) * s,
        (c * h - b * i) * s,
        (b * f - c * e) * s,
        (f * g - d * i) * s,
        (a * i - c * g) * s,
        (c * d - a * f) * s,
        (d * h - e * g) * s,
        (b * g - a * h) * s,
        (a * e - b * d) * s,
    ])
}

/// Full eigendecomposition of a symmetric 9x9 matrix (cyclic
/// Jacobi, the method cv::eigen uses): (eigenvalues, eigenvectors
/// as columns of the returned matrix), unsorted.
fn jacobi_eigen_9(a_in: &[[f64; 9]; 9]) -> ([f64; 9], [[f64; 9]; 9]) {
    let mut a = *a_in;
    let mut v = [[0.0; 9]; 9];
    for i in 0..9 {
        v[i][i] = 1.0;
    }
    for _sweep in 0..100 {
        let mut off = 0.0;
        for p in 0..9 {
            for q in (p + 1)..9 {
                off += a[p][q] * a[p][q];
            }
        }
        if off.sqrt() < 1e-14 {
            break;
        }
        for p in 0..8 {
            for q in (p + 1)..9 {
                let apq = a[p][q];
                if apq == 0.0 {
                    continue;
                }
                let app = a[p][p];
                let aqq = a[q][q];
                let phi = 0.5 * (aqq - app) / apq;
                let t = if phi >= 0.0 {
                    1.0 / (phi + (1.0 + phi * phi).sqrt())
                } else {
                    -1.0 / (-phi + (1.0 + phi * phi).sqrt())
                };
                let c = 1.0 / (1.0 + t * t).sqrt();
                let s = t * c;
                for k in 0..9 {
                    let akp = a[k][p];
                    let akq = a[k][q];
                    a[k][p] = c * akp - s * akq;
                    a[k][q] = s * akp + c * akq;
                }
                for k in 0..9 {
                    let apk = a[p][k];
                    let aqk = a[q][k];
                    a[p][k] = c * apk - s * aqk;
                    a[q][k] = s * apk + c * aqk;
                }
                for k in 0..9 {
                    let vkp = v[k][p];
                    let vkq = v[k][q];
                    v[k][p] = c * vkp - s * vkq;
                    v[k][q] = s * vkp + c * vkq;
                }
            }
        }
    }
    let mut vals = [0.0; 9];
    for i in 0..9 {
        vals[i] = a[i][i];
    }
    (vals, v)
}

const FLT_EPSILON: f64 = 1.192_092_895_507_812_5e-7;
const DBL_EPSILON: f64 = 2.220_446_049_250_313e-16;
const DBL_MIN: f64 = 2.225_073_858_507_201_4e-308;

/// cv::scaleFor (double overload): 1/x when |x| > FLT_EPSILON.
fn scale_for(x: f64) -> f64 {
    if x.abs() > FLT_EPSILON {
        1.0 / x
    } else {
        1.0
    }
}

fn normalize_h(H: &mut H3) {
    let s = scale_for(H[8]);
    for v in H.iter_mut() {
        *v *= s;
    }
}

/// Round pair coordinates to float32: cv2.findHomography converts
/// its inputs to CV_32F before any kernel, error test, or LM step.
fn to_f32_pairs(pairs: &[(Pt, Pt)]) -> Vec<(Pt, Pt)> {
    pairs
        .iter()
        .map(|(a, b)| {
            (
                pt(a[0] as f32 as f64, a[1] as f32 as f64),
                pt(b[0] as f32 as f64, b[1] as f32 as f64),
            )
        })
        .collect()
}

/// OpenCV's HomographyEstimatorCallback::runKernel: DLT with
/// per-axis L1 normalization (shift to centroid, scale so the mean
/// L1 deviation per axis is 1), smallest eigenvector of A^T A,
/// result normalized by scaleFor(H[2][2]).
fn dlt_kernel(pairs: &[(Pt, Pt)]) -> Option<H3> {
    let count = pairs.len();
    if count == 0 {
        return None;
    }
    let n = count as f64;
    let (mut cmx, mut cmy, mut c_mx, mut c_my) = (0.0, 0.0, 0.0, 0.0);
    for (big_m, m) in pairs {
        cmx += m[0];
        cmy += m[1];
        c_mx += big_m[0];
        c_my += big_m[1];
    }
    cmx /= n;
    cmy /= n;
    c_mx /= n;
    c_my /= n;
    let (mut smx, mut smy, mut s_mx, mut s_my) = (0.0, 0.0, 0.0, 0.0);
    for (big_m, m) in pairs {
        smx += (m[0] - cmx).abs();
        smy += (m[1] - cmy).abs();
        s_mx += (big_m[0] - c_mx).abs();
        s_my += (big_m[1] - c_my).abs();
    }
    if smx.abs() < DBL_EPSILON
        || smy.abs() < DBL_EPSILON
        || s_mx.abs() < DBL_EPSILON
        || s_my.abs() < DBL_EPSILON
    {
        return None;
    }
    smx = n / smx;
    smy = n / smy;
    s_mx = n / s_mx;
    s_my = n / s_my;
    let mut ltl = [[0.0; 9]; 9];
    for (big_m, m) in pairs {
        let x = (m[0] - cmx) * smx;
        let y = (m[1] - cmy) * smy;
        let xx = (big_m[0] - c_mx) * s_mx;
        let yy = (big_m[1] - c_my) * s_my;
        let lx = [xx, yy, 1.0, 0.0, 0.0, 0.0, -x * xx, -x * yy, -x];
        let ly = [0.0, 0.0, 0.0, xx, yy, 1.0, -y * xx, -y * yy, -y];
        for j in 0..9 {
            for k in j..9 {
                ltl[j][k] += lx[j] * lx[k] + ly[j] * ly[k];
            }
        }
    }
    for j in 0..9 {
        for k in (j + 1)..9 {
            ltl[k][j] = ltl[j][k];
        }
    }
    let (vals, vecs) = jacobi_eigen_9(&ltl);
    let mut idx = 0;
    for i in 1..9 {
        if vals[i] < vals[idx] {
            idx = i;
        }
    }
    let h0: H3 = [
        vecs[0][idx],
        vecs[1][idx],
        vecs[2][idx],
        vecs[3][idx],
        vecs[4][idx],
        vecs[5][idx],
        vecs[6][idx],
        vecs[7][idx],
        vecs[8][idx],
    ];
    let inv_hnorm: H3 = [1.0 / smx, 0.0, cmx, 0.0, 1.0 / smy, cmy, 0.0, 0.0, 1.0];
    let hnorm2: H3 = [
        s_mx,
        0.0,
        -c_mx * s_mx,
        0.0,
        s_my,
        -c_my * s_my,
        0.0,
        0.0,
        1.0,
    ];
    let mut H = mat3_mul(&inv_hnorm, &mat3_mul(&h0, &hnorm2));
    normalize_h(&mut H);
    if H.iter().any(|v| !v.is_finite()) {
        return None;
    }
    Some(H)
}

/// HomographyRefineCallback: reprojection errors, and the
/// Jacobian rows when requested (2 rows per point, 9 params).
fn refine_compute(pairs: &[(Pt, Pt)], h: &[f64; 9], want_jac: bool) -> (Vec<f64>, Vec<[f64; 9]>) {
    let mut err = Vec::with_capacity(pairs.len() * 2);
    let mut jac: Vec<[f64; 9]> = Vec::new();
    for (big_m, m) in pairs {
        let (mx, my) = (big_m[0], big_m[1]);
        let ww_raw = h[6] * mx + h[7] * my + h[8];
        let ww = if ww_raw.abs() > DBL_EPSILON {
            1.0 / ww_raw
        } else {
            0.0
        };
        let xi = (h[0] * mx + h[1] * my + h[2]) * ww;
        let yi = (h[3] * mx + h[4] * my + h[5]) * ww;
        err.push(xi - m[0]);
        err.push(yi - m[1]);
        if want_jac {
            jac.push([
                mx * ww,
                my * ww,
                ww,
                0.0,
                0.0,
                0.0,
                -mx * ww * xi,
                -my * ww * xi,
                -ww * xi,
            ]);
            jac.push([
                0.0,
                0.0,
                0.0,
                mx * ww,
                my * ww,
                ww,
                -mx * ww * yi,
                -my * ww * yi,
                -ww * yi,
            ]);
        }
    }
    (err, jac)
}

fn jt_j(jac: &[[f64; 9]]) -> [[f64; 9]; 9] {
    let mut a = [[0.0; 9]; 9];
    for row in jac {
        for i in 0..9 {
            for j in 0..9 {
                a[i][j] += row[i] * row[j];
            }
        }
    }
    a
}

fn jt_r(jac: &[[f64; 9]], r: &[f64]) -> [f64; 9] {
    let mut v = [0.0; 9];
    for (row, &ri) in jac.iter().zip(r.iter()) {
        for i in 0..9 {
            v[i] += row[i] * ri;
        }
    }
    v
}

/// Pseudo-inverse of a symmetric 9x9 the way cv::solve /
/// cv::invert with DECOMP_EIG compute it: eigendecomposition,
/// eigenvalues at or below (sum of eigenvalues) * 2 * DBL_EPSILON
/// treated as zero (SVD::backSubst threshold).
fn eig_pinv(a: &[[f64; 9]; 9]) -> [[f64; 9]; 9] {
    let (vals, vecs) = jacobi_eigen_9(a);
    let threshold: f64 = vals.iter().sum::<f64>() * 2.0 * DBL_EPSILON;
    let mut out = [[0.0; 9]; 9];
    for i in 0..9 {
        if vals[i].abs() <= threshold {
            continue;
        }
        let inv = 1.0 / vals[i];
        for r in 0..9 {
            for c in 0..9 {
                out[r][c] += inv * vecs[r][i] * vecs[c][i];
            }
        }
    }
    out
}

fn eig_solve(a: &[[f64; 9]; 9], b: &[f64; 9]) -> [f64; 9] {
    let p = eig_pinv(a);
    let mut out = [0.0; 9];
    for r in 0..9 {
        for c in 0..9 {
            out[r] += p[r][c] * b[c];
        }
    }
    out
}

/// cv::LMSolver (levmarq.cpp) with maxIters = 10 and
/// eps = FLT_EPSILON, driving the homography refine callback —
/// the refinement cv2.findHomography applies for npoints > 4.
fn lm_refine(pairs: &[(Pt, Pt)], h: &mut H3) {
    let mut x: [f64; 9] = *h;
    let (mut r, mut jac) = refine_compute(pairs, &x, true);
    let mut s: f64 = r.iter().map(|val| val * val).sum();
    let mut a = jt_j(&jac);
    let mut v = jt_r(&jac, &r);
    // Marquardt D: the INITIAL A diagonal, never refreshed.
    let d_diag: [f64; 9] = [
        a[0][0], a[1][1], a[2][2], a[3][3], a[4][4], a[5][5], a[6][6], a[7][7], a[8][8],
    ];
    let (mut lambda, mut lc) = (1.0f64, 0.75f64);
    let mut iter = 0;
    loop {
        let mut ap = a;
        for i in 0..9 {
            ap[i][i] += lambda * d_diag[i];
        }
        let d = eig_solve(&ap, &v);
        let mut xd = [0.0; 9];
        for i in 0..9 {
            xd[i] = x[i] - d[i];
        }
        let (rd, _) = refine_compute(pairs, &xd, false);
        let sd: f64 = rd.iter().map(|val| val * val).sum();
        let mut ds = 0.0;
        for i in 0..9 {
            let ad: f64 = (0..9).map(|j| a[i][j] * d[j]).sum::<f64>();
            ds += d[i] * (2.0 * v[i] - ad);
        }
        let rr = (s - sd) / (if ds.abs() > DBL_EPSILON { ds } else { 1.0 });
        if rr > 0.75 {
            lambda *= 0.5;
            if lambda < lc {
                lambda = 0.0;
            }
        } else if rr < 0.25 {
            let t: f64 = (0..9).map(|i| d[i] * v[i]).sum();
            let mut nu = (sd - s) / (if t.abs() > DBL_EPSILON { t } else { 1.0 }) + 2.0;
            nu = nu.clamp(2.0, 10.0);
            if lambda == 0.0 {
                let apinv = eig_pinv(&a);
                let mut maxval = DBL_EPSILON;
                for i in 0..9 {
                    maxval = maxval.max(apinv[i][i].abs());
                }
                lambda = 1.0 / maxval;
                lc = lambda;
                nu *= 0.5;
            }
            lambda *= nu;
        }
        if sd < s {
            s = sd;
            x = xd;
            let (r2, jac2) = refine_compute(pairs, &x, true);
            r = r2;
            jac = jac2;
            a = jt_j(&jac);
            v = jt_r(&jac, &r);
        }
        iter += 1;
        let d_inf = d.iter().fold(0.0f64, |m, val| m.max(val.abs()));
        let r_inf = r.iter().fold(0.0f64, |m, val| m.max(val.abs()));
        let proceed = iter < 10 && d_inf >= FLT_EPSILON && r_inf >= FLT_EPSILON;
        if !proceed {
            break;
        }
    }
    *h = x;
}

/// cv2.findHomography(src, dst, 0): the DLT kernel, plus the LM
/// refinement when npoints > 4.
fn solve_homography(src: &[Pt], dst: &[Pt]) -> Option<H3> {
    if src.len() < 4 || src.len() != dst.len() {
        return None;
    }
    let raw: Vec<(Pt, Pt)> = src.iter().copied().zip(dst.iter().copied()).collect();
    let pairs = to_f32_pairs(&raw);
    let mut H = dlt_kernel(&pairs)?;
    if pairs.len() > 4 {
        lm_refine(&pairs, &mut H);
        normalize_h(&mut H);
    }
    Some(H)
}

// ---------------------------------------------------------------------------
// Card profile

#[derive(Clone, Debug)]
pub struct CardProfile {
    pub card_width_mm: f64,
    pub card_height_mm: f64,
    /// Frame corners (centreline), keyed TL/TR/BL/BR.
    pub frame: HashMap<String, Pt>,
    pub arm_len_mm: f64,
    pub stroke_mm: f64,
    pub ticks: Vec<Tick>,
    pub ring_centre_mm: Pt,
    pub ring_inner_mm: f64,
    pub ring_outer_mm: f64,
}

/// One tick mark on a card arm, in card space.
#[derive(Clone, Debug)]
pub struct Tick {
    arm: String,
    centre_mm: Pt,
    fraction_from_vertex: f64,
}

#[derive(Clone, Debug)]
struct Arm {
    #[allow(dead_code)]
    name: String,
    start_mm: Pt,
    end_mm: Pt,
    ticks_mm: Vec<Pt>,
}

/// Arm name -> (vertex key, direction in card-mm space).
fn arm_geometry(name: &str) -> Option<(&'static str, Pt)> {
    Some(match name {
        "TL-H" => ("TL", [1.0, 0.0]),
        "TL-V" => ("TL", [0.0, 1.0]),
        "TR-H" => ("TR", [-1.0, 0.0]),
        "TR-V" => ("TR", [0.0, 1.0]),
        "BL-V" => ("BL", [0.0, -1.0]),
        "BL-H" => ("BL", [1.0, 0.0]),
        "BR-V" => ("BR", [0.0, -1.0]),
        "BR-H" => ("BR", [-1.0, 0.0]),
        _ => return None,
    })
}

/// Load one card's profile (tick geometry and envelope gates) from
/// the profile file.
pub fn load_card_profile(profile_path: &Path, card_id: &str) -> Result<CardProfile, String> {
    let text = std::fs::read_to_string(profile_path)
        .map_err(|e| format!("Could not read profile {}: {e}", profile_path.display()))?;
    let data: Value = serde_json::from_str(&text)
        .map_err(|e| format!("Profile {} is not JSON: {e}", profile_path.display()))?;
    let cards = data["cards"].as_object().cloned().unwrap_or_default();
    let card = cards.get(card_id).ok_or_else(|| {
        let mut ids: Vec<&String> = cards.keys().collect();
        ids.sort();
        format!(
            "Card {card_id:?} not in profile {} (has: {})",
            profile_path.display(),
            ids.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    let get_pt =
        |v: &Value| -> Pt { pt(v[0].as_f64().unwrap_or(0.0), v[1].as_f64().unwrap_or(0.0)) };
    let mut frame = HashMap::new();
    for key in ["TL", "TR", "BL", "BR"] {
        frame.insert(
            key.to_string(),
            get_pt(&card["bracket_frame"]["corner_vertices_centreline_mm"][key]),
        );
    }
    let ticks = card["ticks"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|t| Tick {
                    arm: t["arm"].as_str().unwrap_or_default().to_string(),
                    centre_mm: get_pt(&t["centre_mm"]),
                    fraction_from_vertex: t["fraction_from_vertex"].as_f64().unwrap_or(0.0),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(CardProfile {
        card_width_mm: card["card_width_mm"].as_f64().unwrap_or(0.0),
        card_height_mm: card["card_height_mm"].as_f64().unwrap_or(0.0),
        frame,
        arm_len_mm: card["bracket_frame"]["arm_centreline_length_mm"]
            .as_f64()
            .unwrap_or(0.0),
        stroke_mm: card["bracket_frame"]["stroke_width_mm"]
            .as_f64()
            .unwrap_or(0.0),
        ticks,
        ring_centre_mm: get_pt(&card["ring"]["centre_mm"]),
        ring_inner_mm: card["ring"]["inner_radius_mm"].as_f64().unwrap_or(0.0),
        ring_outer_mm: card["ring"]["outer_radius_mm"].as_f64().unwrap_or(0.0),
    })
}

/// Bracket arms with their start vertex, direction, and tick list —
/// arms in name order, ticks ordered by fraction_from_vertex.
fn arms(card: &CardProfile) -> Vec<Arm> {
    let mut by_arm: HashMap<String, Vec<&Tick>> = HashMap::new();
    for tick in &card.ticks {
        by_arm.entry(tick.arm.clone()).or_default().push(tick);
    }
    let mut names: Vec<String> = by_arm.keys().cloned().collect();
    names.sort();
    let mut out = Vec::new();
    for name in names {
        let Some((vertex_key, direction)) = arm_geometry(&name) else {
            continue;
        };
        let start = card.frame[vertex_key];
        let mut ticks: Vec<&Tick> = by_arm.remove(&name).unwrap_or_default();
        ticks.sort_by(|a, b| a.fraction_from_vertex.total_cmp(&b.fraction_from_vertex));
        out.push(Arm {
            name,
            start_mm: start,
            end_mm: add(start, scale(direction, card.arm_len_mm)),
            ticks_mm: ticks.iter().map(|t| t.centre_mm).collect(),
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Image frame + sampling

/// A decoded photo in the layouts the estimator samples: OpenCV
/// grayscale and RGB triplets (the Python module loads BGR via cv2;
/// every formula here is applied per-channel-role, so the storage
/// order does not change results).
pub struct MeasureFrame {
    pub width: usize,
    pub height: usize,
    pub gray: Vec<u8>,
    pub rgb: Vec<u8>,
}

impl MeasureFrame {
    pub fn from_rgba(img: &crate::image::RgbaImage) -> MeasureFrame {
        let n = (img.width * img.height) as usize;
        let mut gray = vec![0u8; n];
        let mut rgb = vec![0u8; n * 3];
        for i in 0..n {
            let (r, g, b) = (
                img.pixels[i * 4] as u32,
                img.pixels[i * 4 + 1] as u32,
                img.pixels[i * 4 + 2] as u32,
            );
            // cv2.COLOR_BGR2GRAY fixed point: Y = (4899 R + 9617 G
            // + 1868 B + 8192) >> 14.
            gray[i] = ((4899 * r + 9617 * g + 1868 * b + 8192) >> 14) as u8;
            rgb[i * 3] = r as u8;
            rgb[i * 3 + 1] = g as u8;
            rgb[i * 3 + 2] = b as u8;
        }
        MeasureFrame {
            width: img.width as usize,
            height: img.height as usize,
            gray,
            rgb,
        }
    }

    /// (H in OpenCV 0-179 scale, S, V) of one pixel.
    fn hsv(&self, idx: usize) -> (f64, f64, f64) {
        let r = self.rgb[idx * 3] as f64;
        let g = self.rgb[idx * 3 + 1] as f64;
        let b = self.rgb[idx * 3 + 2] as f64;
        let v = r.max(g).max(b);
        let m = r.min(g).min(b);
        let diff = v - m;
        let s = if v == 0.0 {
            0.0
        } else {
            (255.0 * diff / v).round()
        };
        let h = if diff == 0.0 {
            0.0
        } else {
            let deg = if v == r {
                60.0 * (g - b) / diff
            } else if v == g {
                120.0 + 60.0 * (b - r) / diff
            } else {
                240.0 + 60.0 * (r - g) / diff
            };
            let deg = if deg < 0.0 { deg + 360.0 } else { deg };
            (deg / 2.0).round()
        };
        (h, s, v)
    }
}

/// cv2.getRectSubPix on an 8-bit image: exact bilinear sample,
/// rounded half-up back to an integer value (verified against
/// OpenCV), border replicated.
fn sample_u8(img: &[u8], w: usize, h: usize, x: f64, y: f64) -> f64 {
    let x0 = x.floor();
    let y0 = y.floor();
    let dx = x - x0;
    let dy = y - y0;
    let (xi, yi) = (x0 as i64, y0 as i64);
    let get = |xx: i64, yy: i64| -> f64 {
        let cx = xx.clamp(0, w as i64 - 1) as usize;
        let cy = yy.clamp(0, h as i64 - 1) as usize;
        img[cy * w + cx] as f64
    };
    let v = (1.0 - dx) * (1.0 - dy) * get(xi, yi)
        + dx * (1.0 - dy) * get(xi + 1, yi)
        + (1.0 - dx) * dy * get(xi, yi + 1)
        + dx * dy * get(xi + 1, yi + 1);
    (v + 0.5).floor()
}

fn sample_along(img: &[u8], w: usize, h: usize, p: Pt, q: Pt, n: usize) -> (Vec<f64>, Vec<Pt>) {
    let xs = linspace(p[0], q[0], n);
    let ys = linspace(p[1], q[1], n);
    let mut values = Vec::with_capacity(n);
    let mut points = Vec::with_capacity(n);
    for i in 0..n {
        values.push(sample_u8(img, w, h, xs[i], ys[i]));
        points.push(pt(xs[i], ys[i]));
    }
    (values, points)
}

fn bright_runs(values: &[f64]) -> Vec<(usize, usize)> {
    let low = percentile(values, 10.0);
    let high = percentile(values, 90.0);
    let threshold = (low + high) / 2.0;
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    for (i, &v) in values.iter().enumerate() {
        if v > threshold && start.is_none() {
            start = Some(i);
        } else if v <= threshold && start.is_some() {
            runs.push((start.unwrap(), i - 1));
            start = None;
        }
    }
    if let Some(s) = start {
        runs.push((s, values.len() - 1));
    }
    runs
}

/// Bright tick-cut centres along one arm, matched to expectations
/// (port of `_arm_tick_centres`). A bright run counts only if its
/// centre's path parameter lands within tolerance of an expected
/// tick's parameter.
#[allow(clippy::too_many_arguments)]
fn arm_tick_centres(
    frame: &MeasureFrame,
    start_img: Pt,
    end_img: Pt,
    expected_img: &[Pt],
    tol: f64,
    max_run_frac: f64,
) -> Option<Vec<Pt>> {
    let (values, points) = sample_along(
        &frame.gray,
        frame.width,
        frame.height,
        start_img,
        end_img,
        240,
    );
    // np.convolve(values, ones(5)/5, 'same'): zero-padded moving
    // average (kernel symmetric, so flip direction is immaterial).
    let n = values.len();
    let mut smooth = vec![0.0; n];
    for i in 0..n {
        let mut acc = 0.0;
        for k in -2i64..=2 {
            let j = i as i64 + k;
            if j >= 0 && (j as usize) < n {
                acc += values[j as usize];
            }
        }
        smooth[i] = acc / 5.0;
    }
    let runs = bright_runs(&smooth);
    let low = percentile(&smooth, 10.0);
    let max_len = ((max_run_frac * n as f64) as usize).max(6);
    let plausible: Vec<(usize, usize)> = runs
        .into_iter()
        .filter(|(a, b)| {
            let len = b - a + 1;
            len >= 2 && len <= max_len
        })
        .collect();
    let axis = sub(end_img, start_img);
    let denom = dot(axis, axis);
    if denom <= 0.0 {
        return None;
    }
    let param = |p: Pt| -> f64 { dot(sub(p, start_img), axis) / denom };
    let mut run_centres: Vec<(f64, Pt)> = Vec::new();
    for (a, b) in plausible {
        let mut wsum = 0.0;
        let mut cx = 0.0;
        let mut cy = 0.0;
        for i in a..=b {
            let wgt = (smooth[i] - low).max(0.0);
            wsum += wgt;
            cx += points[i][0] * wgt;
            cy += points[i][1] * wgt;
        }
        if wsum <= 0.0 {
            continue;
        }
        let centre = pt(cx / wsum, cy / wsum);
        run_centres.push((param(centre), centre));
    }
    let mut centres = Vec::new();
    let mut used: Vec<bool> = vec![false; run_centres.len()];
    for expected_pt in expected_img {
        let target = param(*expected_pt);
        let mut best: Option<usize> = None;
        let mut best_d = tol;
        for (i, (p, _)) in run_centres.iter().enumerate() {
            if used[i] {
                continue;
            }
            let d = (p - target).abs();
            if d < best_d {
                best = Some(i);
                best_d = d;
            }
        }
        let best = best?;
        used[best] = true;
        centres.push(run_centres[best].1);
    }
    Some(centres)
}

/// Centreline points of one arm, measured from the ink mask
/// (port of `_arm_centreline_points`).
fn arm_centreline_points(
    mask: &[u8],
    w: usize,
    h: usize,
    start_img: Pt,
    end_img: Pt,
    half_width: f64,
) -> Option<Vec<Pt>> {
    let axis = sub(end_img, start_img);
    let length = norm(axis);
    if length <= 0.0 {
        return None;
    }
    let direction = scale(axis, 1.0 / length);
    let normal = pt(-direction[1], direction[0]);
    let mut points = Vec::new();
    for t in linspace(0.08, 0.92, 22) {
        let base = add(start_img, scale(axis, t));
        let ts = linspace(-half_width, half_width, 33);
        let mut values = Vec::with_capacity(33);
        let mut sum = 0.0;
        for &off in &ts {
            let p = add(base, scale(normal, off));
            let v = sample_u8(mask, w, h, p[0], p[1]);
            values.push(v);
            sum += v;
        }
        if sum < 0.25 * 255.0 * 33.0 * 0.3 {
            continue;
        }
        let mut weighted = 0.0;
        for (off, v) in ts.iter().zip(values.iter()) {
            weighted += off * v;
        }
        let offset = weighted / sum;
        points.push(add(base, scale(normal, offset)));
    }
    if points.len() < 6 {
        return None;
    }
    Some(points)
}

/// (centroid, principal direction) of a point set — the SVD line
/// fit; direction sign is irrelevant to every consumer.
fn fit_line(points: &[Pt]) -> (Pt, Pt) {
    let n = points.len() as f64;
    let cx = points.iter().map(|p| p[0]).sum::<f64>() / n;
    let cy = points.iter().map(|p| p[1]).sum::<f64>() / n;
    let (mut sxx, mut sxy, mut syy) = (0.0, 0.0, 0.0);
    for p in points {
        let (dx, dy) = (p[0] - cx, p[1] - cy);
        sxx += dx * dx;
        sxy += dx * dy;
        syy += dy * dy;
    }
    // Eigenvector of [[sxx, sxy], [sxy, syy]] for the larger
    // eigenvalue.
    let theta = 0.5 * (2.0 * sxy).atan2(sxx - syy);
    let dir = pt(theta.cos(), theta.sin());
    // atan2 param form gives the major axis directly only when the
    // covariance is not axis-flipped; verify and fall back to the
    // analytic eigenvector if it points along the minor axis.
    let q = |d: Pt| -> f64 {
        points
            .iter()
            .map(|p| {
                let dx = p[0] - cx;
                let dy = p[1] - cy;
                (dx * d[0] + dy * d[1]).powi(2)
            })
            .sum()
    };
    let alt = pt(-dir[1], dir[0]);
    let dir = if q(alt) > q(dir) { alt } else { dir };
    (pt(cx, cy), dir)
}

fn line_intersection(p1: Pt, d1: Pt, p2: Pt, d2: Pt) -> Option<Pt> {
    // Solve p1 + t*d1 = p2 + s*d2 via the 2x2 system [d1, -d2].
    let det = d1[0] * (-d2[1]) - d1[1] * (-d2[0]);
    if det.abs() < 1e-9 {
        return None;
    }
    let rhs = sub(p2, p1);
    let t = (rhs[0] * (-d2[1]) - rhs[1] * (-d2[0])) / det;
    Some(add(p1, scale(d1, t)))
}

struct TickMatches {
    matched: Vec<(Pt, Pt)>,
    arms_hit: usize,
    corners: Vec<(Pt, Pt)>,
}

const EDGE_ARMS: [(&str, [&str; 2]); 4] = [
    ("top", ["TL-H", "TR-H"]),
    ("bottom", ["BL-H", "BR-H"]),
    ("left", ["TL-V", "BL-V"]),
    ("right", ["TR-V", "BR-V"]),
];

/// (tick pairs, arms hit, corner pairs) under H — port of
/// `_tick_matches`. Edge lines are only measured when `with_lines`.
fn tick_matches(
    frame: &MeasureFrame,
    mask: &[u8],
    card: &CardProfile,
    H: &H3,
    strict: bool,
    with_lines: bool,
) -> TickMatches {
    let (tol, max_run_frac) = if strict { (0.11, 0.12) } else { (0.20, 0.30) };
    let mut arms_hit = 0usize;
    let mut arm_points: HashMap<String, Vec<Pt>> = HashMap::new();
    let mut arm_lines: HashMap<String, (Pt, Pt)> = HashMap::new();
    let mut pending: Vec<(Arm, Vec<Pt>)> = Vec::new();
    for arm in arms(card) {
        let start_img = project1(H, arm.start_mm);
        let end_img = project1(H, arm.end_mm);
        let direction = sub(end_img, start_img);
        let arm_len_img = norm(direction);
        let half_width = 0.75 * card.stroke_mm * (arm_len_img / card.arm_len_mm);
        if with_lines {
            if let Some(pts) = arm_centreline_points(
                mask,
                frame.width,
                frame.height,
                start_img,
                end_img,
                half_width,
            ) {
                arm_lines.insert(arm.name.clone(), fit_line(&pts));
                arm_points.insert(arm.name.clone(), pts);
            }
        }
        let (mask_values, _) =
            sample_along(mask, frame.width, frame.height, start_img, end_img, 120);
        let frac_on =
            mask_values.iter().filter(|&&v| v > 0.0).count() as f64 / mask_values.len() as f64;
        if frac_on < 0.55 {
            continue;
        }
        let expected_img: Vec<Pt> = arm.ticks_mm.iter().map(|t| project1(H, *t)).collect();
        let centres =
            match arm_tick_centres(frame, start_img, end_img, &expected_img, tol, max_run_frac) {
                Some(c) => c,
                None => continue,
            };
        arms_hit += 1;
        pending.push((arm, centres));
    }
    let mut matched: Vec<(Pt, Pt)> = Vec::new();
    for (arm, centres) in &pending {
        let line = arm_lines.get(&arm.name);
        for (mm_pt, img_pt) in arm.ticks_mm.iter().zip(centres.iter()) {
            let mut img_pt = *img_pt;
            if let Some((line_pt, line_dir)) = line {
                // Keep the accurate along-path coordinate; take the
                // lateral coordinate from the measured centreline.
                let along = dot(sub(img_pt, *line_pt), *line_dir);
                img_pt = add(*line_pt, scale(*line_dir, along));
            }
            matched.push((*mm_pt, img_pt));
        }
    }
    // Frame edges from merged arm centreline points; corners from
    // intersecting them.
    let mut corners: Vec<(Pt, Pt)> = Vec::new();
    if with_lines {
        let mut edges: HashMap<&str, (Pt, Pt)> = HashMap::new();
        for (edge, arm_names) in EDGE_ARMS.iter() {
            let mut merged: Vec<Pt> = Vec::new();
            for name in arm_names {
                if let Some(pts) = arm_points.get(*name) {
                    merged.extend_from_slice(pts);
                }
            }
            if merged.len() >= 8 {
                edges.insert(edge, fit_line(&merged));
            }
        }
        for (vertex, e_a, e_b) in [
            ("TL", "top", "left"),
            ("TR", "top", "right"),
            ("BL", "bottom", "left"),
            ("BR", "bottom", "right"),
        ] {
            if let (Some(a), Some(b)) = (edges.get(e_a), edges.get(e_b)) {
                if let Some(corner) = line_intersection(a.0, a.1, b.0, b.1) {
                    corners.push((card.frame[vertex], corner));
                }
            }
        }
    }
    TickMatches {
        matched,
        arms_hit,
        corners,
    }
}

// ---------------------------------------------------------------------------
// Ink mask + ring anchor (the OpenCV front end)

/// cv2.medianBlur(gray, 3) followed by cv2.adaptiveThreshold
/// (GAUSSIAN, blockSize 51, C 12, BINARY_INV): the card's ink mask.
fn ink_mask(frame: &MeasureFrame) -> Vec<u8> {
    let (w, h) = (frame.width, frame.height);
    // Median 3x3, border replicated.
    let mut blurred = vec![0u8; w * h];
    for y in 0..h as i64 {
        for x in 0..w as i64 {
            let mut vals = [0u8; 9];
            let mut k = 0;
            for dy in -1..=1i64 {
                for dx in -1..=1i64 {
                    let cx = (x + dx).clamp(0, w as i64 - 1) as usize;
                    let cy = (y + dy).clamp(0, h as i64 - 1) as usize;
                    vals[k] = frame.gray[cy * w + cx];
                    k += 1;
                }
            }
            vals.sort_unstable();
            blurred[y as usize * w + x as usize] = vals[4];
        }
    }
    // Gaussian kernel, blockSize 51: sigma = 0.3*((51-1)*0.5 - 1) + 0.8 = 8.
    let sigma = 8.0f64;
    let mut kernel = [0.0f64; 51];
    let mut ksum = 0.0;
    for i in 0..51 {
        let d = i as f64 - 25.0;
        kernel[i] = (-(d * d) / (2.0 * sigma * sigma)).exp();
        ksum += kernel[i];
    }
    for v in kernel.iter_mut() {
        *v /= ksum;
    }
    // Separable weighted mean, border replicated, in f64.
    let mut tmp = vec![0.0f64; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0.0;
            for (i, &kw) in kernel.iter().enumerate() {
                let cx = (x as i64 + i as i64 - 25).clamp(0, w as i64 - 1) as usize;
                acc += kw * blurred[y * w + cx] as f64;
            }
            tmp[y * w + x] = acc;
        }
    }
    let mut mask = vec![0u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0.0;
            for (i, &kw) in kernel.iter().enumerate() {
                let cy = (y as i64 + i as i64 - 25).clamp(0, h as i64 - 1) as usize;
                acc += kw * tmp[cy * w + x];
            }
            // OpenCV's adaptiveThreshold rounds the Gaussian mean
            // to 8U first, then subtracts C with saturation, then
            // compares in integers (BINARY_INV: fg iff src <= T).
            // Comparing against the unrounded f64 mean erodes the
            // mask by a pixel on soft edges — enough to shear the
            // sheet seed on small cards.
            let thresh = ((acc.round() as i32) - 12).max(0);
            mask[y * w + x] = if (blurred[y * w + x] as i32) <= thresh {
                255
            } else {
                0
            };
        }
    }
    mask
}

// ---------------------------------------------------------------------------
// Ring candidates (the seed anchors)
//
// The Python port anchored every seed at the centroid of ALL cyan-hued
// pixels in the frame (`_ring_centroid`). Sky, teal play equipment and
// denim pull that centroid off the card, so on real photos the search
// never looked at the card (IMG_1403, IMG_1435, IMG_1491). Seeds now
// come from ring-shaped cyan blobs: connected components of the ring
// cyan, grouped when they are similar in size and close together (the
// printed ring is three 92-degree arcs, i.e. three components), each
// group tested for ring geometry, and each surviving ring anchoring
// its own seed search at its own centre and scale.

/// A ring-shaped cyan blob: centre, outer radius (px, geometric mean
/// of the ellipse axes) and pixel count.
#[derive(Clone, Copy, Debug)]
struct RingCandidate {
    centre: Pt,
    outer_px: f64,
    n_px: usize,
}

/// Components smaller than this are speckle, not ring arcs.
const RING_COMP_MIN_PX: usize = 3;
/// Only the largest components take part in grouping (bounds the
/// pairwise pass on frames full of cyan speckle).
const RING_MAX_COMPONENTS: usize = 3000;
/// Two components join one group when the gap between their boxes is at
/// most this fraction of the larger box side and their box sides differ
/// by at most RING_LINK_MAX_SIZE_RATIO (the arcs of one ring are equal;
/// a big background blob must not swallow a nearby ring).
const RING_LINK_FRAC: f64 = 0.5;
const RING_LINK_MAX_SIZE_RATIO: f64 = 2.5;
/// A ring candidate needs this many pixels and this outer radius. A
/// card at the 130 px floor carries a ring of ~8 px outer radius.
const RING_MIN_PX: usize = 20;
const RING_MIN_OUTER_PX: f64 = 5.0;
/// Ellipse axis-length ratio limit (eigenvalue ratio 4: a tilt of
/// up to 60 degrees).
const RING_MAX_EIG_RATIO: f64 = 4.0;
/// After whitening, at most this fraction of the blob may lie inside
/// the hole or outside the outer edge (with a margin of a quarter of
/// the ring width, at least 1 px).
const RING_MAX_STRAY_FRAC: f64 = 0.05;
/// Pixel count over the annulus area: 1.0 for a full ring, 0.77 for
/// the printed three-arc ring.
const RING_FILL_MIN: f64 = 0.5;
const RING_FILL_MAX: f64 = 1.2;
/// At least this many of 12 thirty-degree sectors hold ring pixels
/// (each 28-degree gap of the printed ring can empty at most one).
const RING_MIN_SECTORS_OF_12: usize = 9;
/// At most this many ring candidates are fitted and verified.
const RING_MAX_CANDIDATES: usize = 4;

struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> UnionFind {
        UnionFind {
            parent: (0..n).collect(),
        }
    }
    fn find(&mut self, mut i: usize) -> usize {
        while self.parent[i] != i {
            self.parent[i] = self.parent[self.parent[i]];
            i = self.parent[i];
        }
        i
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            // Deterministic: the smaller root wins.
            let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
            self.parent[hi] = lo;
        }
    }
}

/// Ring geometry of one pixel set, or None when it is not ring-shaped.
/// `k` is the profile's inner/outer radius ratio.
fn ring_shape(points: &[(f64, f64)], k: f64) -> Option<RingCandidate> {
    let n = points.len();
    if n < RING_MIN_PX {
        return None;
    }
    let nf = n as f64;
    let mx = points.iter().map(|p| p.0).sum::<f64>() / nf;
    let my = points.iter().map(|p| p.1).sum::<f64>() / nf;
    let (mut sxx, mut sxy, mut syy) = (0.0, 0.0, 0.0);
    for p in points {
        let (dx, dy) = (p.0 - mx, p.1 - my);
        sxx += dx * dx;
        sxy += dx * dy;
        syy += dy * dy;
    }
    let (sxx, sxy, syy) = (sxx / nf, sxy / nf, syy / nf);
    // Eigen-decomposition of the 2x2 covariance.
    let tr = sxx + syy;
    let det = sxx * syy - sxy * sxy;
    let disc = (tr * tr / 4.0 - det).max(0.0).sqrt();
    let (l1, l2) = (tr / 2.0 + disc, tr / 2.0 - disc);
    if l2 <= 0.0 || l1 / l2 > RING_MAX_EIG_RATIO {
        return None;
    }
    let theta = 0.5 * (2.0 * sxy).atan2(sxx - syy);
    let (c, s) = (theta.cos(), theta.sin());
    // Area-preserving whitening: the major axis shrinks, the minor
    // stretches, both to the geometric mean.
    let (f1, f2) = ((l2 / l1).powf(0.25), (l1 / l2).powf(0.25));
    let whiten = |p: &(f64, f64)| -> (f64, f64) {
        let (dx, dy) = (p.0 - mx, p.1 - my);
        let (a, b) = (c * dx + s * dy, -s * dx + c * dy);
        (a * f1, b * f2)
    };
    let mut m2 = 0.0;
    let mut whitened: Vec<(f64, f64)> = Vec::with_capacity(n);
    for p in points {
        let q = whiten(p);
        m2 += q.0 * q.0 + q.1 * q.1;
        whitened.push(q);
    }
    m2 /= nf;
    // Uniform annulus: E[r^2] = (inner^2 + outer^2) / 2.
    let outer = (2.0 * m2 / (1.0 + k * k)).sqrt();
    let inner = k * outer;
    if outer < RING_MIN_OUTER_PX {
        return None;
    }
    let margin = (0.25 * (outer - inner)).max(1.0);
    let (mut inside, mut outside) = (0usize, 0usize);
    let mut sectors = [false; 12];
    for q in &whitened {
        let r = (q.0 * q.0 + q.1 * q.1).sqrt();
        if r < inner - margin {
            inside += 1;
        } else if r > outer + margin {
            outside += 1;
        }
        let a = q.1.atan2(q.0).rem_euclid(std::f64::consts::TAU);
        sectors[((a / std::f64::consts::TAU * 12.0) as usize).min(11)] = true;
    }
    let fill = nf / (std::f64::consts::PI * (outer * outer - inner * inner));
    if inside as f64 > RING_MAX_STRAY_FRAC * nf
        || outside as f64 > RING_MAX_STRAY_FRAC * nf
        || !(RING_FILL_MIN..=RING_FILL_MAX).contains(&fill)
        || sectors.iter().filter(|&&b| b).count() < RING_MIN_SECTORS_OF_12
    {
        return None;
    }
    Some(RingCandidate {
        centre: pt(mx, my),
        outer_px: outer,
        n_px: n,
    })
}

/// Every ring-cyan pixel of a photo, summarised.
struct CyanSurvey {
    /// Pixel count and centroid of ALL ring-cyan pixels (the Python
    /// port's `_ring_centroid`, used only as an extra anchor when it
    /// sits on a ring candidate).
    count: usize,
    centroid: Pt,
    /// Ring-shaped blobs, largest first, at most RING_MAX_CANDIDATES.
    candidates: Vec<RingCandidate>,
}

fn ring_candidates(frame: &MeasureFrame, card: &CardProfile) -> CyanSurvey {
    let (w, h) = (frame.width, frame.height);
    let cyan: Vec<usize> = (0..w * h).filter(|&i| is_ring_cyan(frame, i)).collect();
    if cyan.is_empty() {
        return CyanSurvey {
            count: 0,
            centroid: pt(0.0, 0.0),
            candidates: Vec::new(),
        };
    }
    // 8-connected components over the sparse cyan set (scan order).
    let mut uf = UnionFind::new(cyan.len());
    let pos = |idx: usize| cyan.binary_search(&idx).ok();
    for (i, &idx) in cyan.iter().enumerate() {
        let (x, y) = (idx % w, idx / w);
        let mut neighbours: [Option<usize>; 4] = [None; 4];
        if x > 0 {
            neighbours[0] = Some(idx - 1);
        }
        if y > 0 {
            let up = idx - w;
            neighbours[1] = Some(up);
            if x > 0 {
                neighbours[2] = Some(up - 1);
            }
            if x + 1 < w {
                neighbours[3] = Some(up + 1);
            }
        }
        for nb in neighbours.iter().flatten() {
            if let Some(j) = pos(*nb) {
                uf.union(i, j);
            }
        }
    }
    struct Comp {
        members: Vec<usize>,
        x0: usize,
        y0: usize,
        x1: usize,
        y1: usize,
    }
    let mut by_root: HashMap<usize, usize> = HashMap::new();
    let mut comps: Vec<Comp> = Vec::new();
    for (i, &idx) in cyan.iter().enumerate() {
        let root = uf.find(i);
        let (x, y) = (idx % w, idx / w);
        let ci = *by_root.entry(root).or_insert_with(|| {
            comps.push(Comp {
                members: Vec::new(),
                x0: x,
                y0: y,
                x1: x,
                y1: y,
            });
            comps.len() - 1
        });
        let c = &mut comps[ci];
        c.members.push(idx);
        c.x0 = c.x0.min(x);
        c.y0 = c.y0.min(y);
        c.x1 = c.x1.max(x);
        c.y1 = c.y1.max(y);
    }
    // comps is in order of first pixel (scan order): deterministic.
    let mut kept: Vec<usize> = (0..comps.len())
        .filter(|&i| comps[i].members.len() >= RING_COMP_MIN_PX)
        .collect();
    kept.sort_by(|&a, &b| {
        comps[b]
            .members
            .len()
            .cmp(&comps[a].members.len())
            .then(a.cmp(&b))
    });
    kept.truncate(RING_MAX_COMPONENTS);
    kept.sort_unstable();
    let side = |c: &Comp| (c.x1 - c.x0 + 1).max(c.y1 - c.y0 + 1) as f64;
    let mut groups = UnionFind::new(kept.len());
    for a in 0..kept.len() {
        for b in (a + 1)..kept.len() {
            let (ca, cb) = (&comps[kept[a]], &comps[kept[b]]);
            let (sa, sb) = (side(ca), side(cb));
            if sa.max(sb) > RING_LINK_MAX_SIZE_RATIO * sa.min(sb) {
                continue;
            }
            let gap_x = (cb.x0 as f64 - ca.x1 as f64 - 1.0)
                .max(ca.x0 as f64 - cb.x1 as f64 - 1.0)
                .max(0.0);
            let gap_y = (cb.y0 as f64 - ca.y1 as f64 - 1.0)
                .max(ca.y0 as f64 - cb.y1 as f64 - 1.0)
                .max(0.0);
            if gap_x.max(gap_y) <= RING_LINK_FRAC * sa.max(sb) {
                groups.union(a, b);
            }
        }
    }
    let mut members_of: Vec<Vec<usize>> = vec![Vec::new(); kept.len()];
    for a in 0..kept.len() {
        let r = groups.find(a);
        members_of[r].push(a);
    }
    let k = if card.ring_outer_mm > 0.0 && card.ring_inner_mm > 0.0 {
        card.ring_inner_mm / card.ring_outer_mm
    } else {
        0.62
    };
    let mut out: Vec<RingCandidate> = Vec::new();
    for group in members_of.iter().filter(|g| !g.is_empty()) {
        let points: Vec<(f64, f64)> = group
            .iter()
            .flat_map(|&a| comps[kept[a]].members.iter())
            .map(|&idx| ((idx % w) as f64, (idx / w) as f64))
            .collect();
        if let Some(c) = ring_shape(&points, k) {
            out.push(c);
        }
    }
    out.sort_by(|a, b| {
        b.n_px
            .cmp(&a.n_px)
            .then(a.centre[1].total_cmp(&b.centre[1]))
            .then(a.centre[0].total_cmp(&b.centre[0]))
    });
    out.truncate(RING_MAX_CANDIDATES);
    let nf = cyan.len() as f64;
    let centroid = pt(
        cyan.iter().map(|&i| (i % w) as f64).sum::<f64>() / nf,
        cyan.iter().map(|&i| (i / w) as f64).sum::<f64>() / nf,
    );
    CyanSurvey {
        count: cyan.len(),
        centroid,
        candidates: out,
    }
}

// ---------------------------------------------------------------------------
// Contours (cv2.findContours RETR_EXTERNAL + CHAIN_APPROX_SIMPLE)

const DIRS8: [(i64, i64); 8] = [
    (1, 0),
    (1, 1),
    (0, 1),
    (-1, 1),
    (-1, 0),
    (-1, -1),
    (0, -1),
    (1, -1),
];

/// Moore-neighbour trace of one component's outer boundary,
/// starting at its topmost-leftmost pixel.
fn trace_outer(mask: &[u8], w: usize, h: usize, start: (i64, i64)) -> Vec<Pt> {
    let fg = |x: i64, y: i64| -> bool {
        x >= 0
            && y >= 0
            && (x as usize) < w
            && (y as usize) < h
            && mask[y as usize * w + x as usize] > 0
    };
    let mut chain: Vec<Pt> = vec![pt(start.0 as f64, start.1 as f64)];
    let mut p = start;
    // The pixel above the start is background (start is topmost);
    // it is the initial backtrack.
    let mut backtrack = (start.0, start.1 - 1);
    // Jacob's stopping criterion: the trace is done when, standing
    // on the start pixel again, the next step would repeat the very
    // first step. (Comparing backtracks on re-entry never fires:
    // the trace re-enters the start from a different side than the
    // initial backtrack, and cycles forever.)
    let mut first_dir: Option<usize> = None;
    // A boundary trace visits each boundary pixel a bounded number
    // of times; the cap is a defensive backstop only.
    let max_steps = 2 * (w * h) + 64;
    for _ in 0..max_steps {
        // Direction index of the backtrack relative to p.
        let bd = (backtrack.0 - p.0, backtrack.1 - p.1);
        let b_idx = DIRS8.iter().position(|&d| d == bd).unwrap_or(6);
        let mut moved = false;
        for k in 1..=8usize {
            let d_idx = (b_idx + k) % 8;
            let d = DIRS8[d_idx];
            let q = (p.0 + d.0, p.1 + d.1);
            if fg(q.0, q.1) {
                if p == start && first_dir == Some(d_idx) {
                    return chain;
                }
                if first_dir.is_none() {
                    first_dir = Some(d_idx);
                }
                let prev_d = DIRS8[(b_idx + k - 1) % 8];
                backtrack = (p.0 + prev_d.0, p.1 + prev_d.1);
                chain.push(pt(q.0 as f64, q.1 as f64));
                p = q;
                moved = true;
                break;
            }
        }
        if !moved {
            break; // isolated pixel
        }
    }
    chain
}

/// Compress a closed boundary chain the way CHAIN_APPROX_SIMPLE
/// does: keep a point only where the step direction changes.
fn approx_simple(chain: &[Pt]) -> Vec<Pt> {
    let n = chain.len();
    if n <= 2 {
        return chain.to_vec();
    }
    let dir_of = |a: Pt, b: Pt| -> (i64, i64) {
        // NB: f64::signum returns 1.0 for +0.0, which would give
        // axis-aligned steps a phantom direction; spell it out.
        let sign = |d: f64| -> i64 {
            if d > 0.0 {
                1
            } else if d < 0.0 {
                -1
            } else {
                0
            }
        };
        (sign(b[0] - a[0]), sign(b[1] - a[1]))
    };
    let mut out = Vec::new();
    for i in 0..n {
        let prev = chain[(i + n - 1) % n];
        let cur = chain[i];
        let next = chain[(i + 1) % n];
        if dir_of(prev, cur) != dir_of(cur, next) {
            out.push(cur);
        }
    }
    if out.is_empty() {
        chain.to_vec()
    } else {
        out
    }
}

/// External contours of the mask, as pixel-centre chains in
/// raster order of each component's start pixel; `simple` applies
/// the CHAIN_APPROX_SIMPLE direction-change compression, otherwise
/// the full chain (CHAIN_APPROX_NONE) is returned.
fn external_contours(mask: &[u8], w: usize, h: usize, simple: bool) -> Vec<Vec<Pt>> {
    // Label components (8-connectivity) in raster order.
    let mut labels = vec![0i32; w * h];
    let mut starts: Vec<(i64, i64)> = Vec::new();
    let mut current = 0i32;
    for y in 0..h {
        for x in 0..w {
            let idx = y * w + x;
            if mask[idx] == 0 || labels[idx] != 0 {
                continue;
            }
            current += 1;
            starts.push((x as i64, y as i64));
            labels[idx] = current;
            let mut stack = vec![(x, y)];
            while let Some((cx, cy)) = stack.pop() {
                for dy in -1..=1i64 {
                    for dx in -1..=1i64 {
                        let (nx, ny) = (cx as i64 + dx, cy as i64 + dy);
                        if nx < 0 || ny < 0 || nx as usize >= w || ny as usize >= h {
                            continue;
                        }
                        let nidx = ny as usize * w + nx as usize;
                        if mask[nidx] > 0 && labels[nidx] == 0 {
                            labels[nidx] = current;
                            stack.push((nx as usize, ny as usize));
                        }
                    }
                }
            }
        }
    }
    starts
        .iter()
        .map(|&s| {
            let chain = trace_outer(mask, w, h, s);
            if simple {
                approx_simple(&chain)
            } else {
                chain
            }
        })
        .collect()
}

fn arc_length(chain: &[Pt], closed: bool) -> f64 {
    let mut total = 0.0;
    let n = chain.len();
    if n < 2 {
        return 0.0;
    }
    let pairs = if closed { n } else { n - 1 };
    for i in 0..pairs {
        total += norm(sub(chain[(i + 1) % n], chain[i]));
    }
    total
}

/// cv2.approxPolyDP on a closed chain — a transliteration of
/// OpenCV's approx.cpp (Ramer-Douglas-Peucker with its diametral
/// seed pair, cross-product distance test, and final cleanup pass),
/// because the seed quad is only accepted at exactly 4 points and
/// textbook DP anchors differently.
fn approx_poly_dp(chain: &[Pt], eps_in: f64) -> Vec<Pt> {
    let count = chain.len();
    if count == 0 {
        return Vec::new();
    }
    let eps = eps_in * eps_in; // OpenCV squares it up front
    let src = chain;
    let mut dst: Vec<Pt> = Vec::with_capacity(count);
    // Slices are (start, end) index pairs along the wrapped order.
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut right_start: usize = 0;
    let slice_start: usize;
    let mut le_eps: bool;
    // 1. Approximately two farthest points (3 refinement rounds
    // starting from src[0]).
    let mut pos: usize = 0;
    let mut start_pt = pt(0.0, 0.0);
    right_start = 0;
    le_eps = false;
    for _ in 0..3 {
        let mut max_dist = 0.0f64;
        pos = (pos + right_start) % count;
        // READ_PT(start_pt, pos)
        start_pt = src[pos];
        pos = if pos + 1 >= count { 0 } else { pos + 1 };
        for j in 1..count {
            let p = src[pos];
            pos = if pos + 1 >= count { 0 } else { pos + 1 };
            let dx = p[0] - start_pt[0];
            let dy = p[1] - start_pt[1];
            let dist = dx * dx + dy * dy;
            if dist > max_dist {
                max_dist = dist;
                right_start = j;
            }
        }
        le_eps = max_dist <= eps;
    }
    if !le_eps {
        // right_slice = (farthest, anchor); slice = (anchor, farthest)
        let anchor = pos % count;
        slice_start = anchor;
        let farthest = (right_start + anchor) % count;
        stack.push((farthest, anchor)); // right_slice pushed first
        stack.push((anchor, farthest)); // slice processed first (LIFO)
    } else {
        dst.push(start_pt);
    }
    // 2. Stack DP: write slice start points whose slice is within eps.
    while let Some((s, e)) = stack.pop() {
        let end_pt = src[e];
        let mut ppos = s;
        // READ_PT(start_pt, ppos)
        let mut spt = src[ppos];
        ppos = if ppos + 1 >= count { 0 } else { ppos + 1 };
        if ppos != e {
            let dx = end_pt[0] - spt[0];
            let dy = end_pt[1] - spt[1];
            debug_assert!(dx != 0.0 || dy != 0.0);
            let mut max_dist = 0.0f64;
            let mut split = right_start;
            while ppos != e {
                let p = src[ppos];
                ppos = if ppos + 1 >= count { 0 } else { ppos + 1 };
                let dist = ((p[1] - spt[1]) * dx - (p[0] - spt[0]) * dy).abs();
                if dist > max_dist {
                    max_dist = dist;
                    split = (ppos + count - 1) % count;
                }
            }
            le_eps = max_dist * max_dist <= eps * (dx * dx + dy * dy);
            if le_eps {
                dst.push(spt);
            } else {
                stack.push((split, e)); // right_slice
                stack.push((s, split)); // slice
            }
        } else {
            spt = src[s];
            dst.push(spt);
        }
    }
    // 3. Cleanup pass: drop points lying almost on the segment
    // between their neighbours (in-place, wrapped).
    let total = dst.len();
    if total <= 2 {
        return dst;
    }
    let mut new_count = total;
    let mut buf = dst.clone();
    let mut pos2: usize = total - 1; // closed: start at last point
    let read_dst = |buf: &[Pt], pos: &mut usize| -> Pt {
        let p = buf[*pos];
        *pos = if *pos + 1 >= total { 0 } else { *pos + 1 };
        p
    };
    let mut start_pt = read_dst(&buf, &mut pos2);
    let mut wpos = pos2;
    let mut pt_cur = read_dst(&buf, &mut pos2);
    let mut i = 0usize;
    while i < total && new_count > 2 {
        let end_pt = read_dst(&buf, &mut pos2);
        let dx = end_pt[0] - start_pt[0];
        let dy = end_pt[1] - start_pt[1];
        let dist = ((pt_cur[0] - start_pt[0]) * dy - (pt_cur[1] - start_pt[1]) * dx).abs();
        let sip = (pt_cur[0] - start_pt[0]) * (end_pt[0] - pt_cur[0])
            + (pt_cur[1] - start_pt[1]) * (end_pt[1] - pt_cur[1]);
        if dist * dist <= 0.5 * eps * (dx * dx + dy * dy) && dx != 0.0 && dy != 0.0 && sip >= 0.0 {
            new_count -= 1;
            buf[wpos] = end_pt;
            start_pt = end_pt;
            wpos = if wpos + 1 >= total { 0 } else { wpos + 1 };
            pt_cur = read_dst(&buf, &mut pos2);
            i += 2;
            continue;
        }
        buf[wpos] = pt_cur;
        start_pt = pt_cur;
        wpos = if wpos + 1 >= total { 0 } else { wpos + 1 };
        pt_cur = end_pt;
        i += 1;
    }
    buf.truncate(new_count);
    buf
}

fn order_quad(pts: &[Pt]) -> [Pt; 4] {
    let sums: Vec<f64> = pts.iter().map(|p| p[0] + p[1]).collect();
    let diffs: Vec<f64> = pts.iter().map(|p| p[0] - p[1]).collect();
    [
        pts[argmin_first(&sums)],  // TL
        pts[argmax_first(&diffs)], // TR
        pts[argmax_first(&sums)],  // BR
        pts[argmin_first(&diffs)], // BL
    ]
}

/// Homography seed from the card sheet's own quad, or None (port
/// of `_sheet_seed`).
/// The six longest external ink contours, the sheet seed's input
/// (computed once per photo and shared by every ring candidate).
fn sheet_contours(mask: &[u8], w: usize, h: usize) -> Vec<Vec<Pt>> {
    let mut contours = external_contours(mask, w, h, true);
    contours.sort_by(|a, b| arc_length(b, true).total_cmp(&arc_length(a, true)));
    contours.truncate(6);
    contours
}

fn sheet_seed(contours: &[Vec<Pt>], card: &CardProfile, ring_centroid: Pt) -> Option<H3> {
    let (cw, ch) = (card.card_width_mm, card.card_height_mm);
    let sheet_mm = [pt(0.0, 0.0), pt(cw, 0.0), pt(cw, ch), pt(0.0, ch)];
    for contour in contours {
        let peri = arc_length(contour, true);
        if peri < 400.0 {
            continue;
        }
        let mut approx: Option<Vec<Pt>> = None;
        for eps_factor in [0.01, 0.02, 0.03] {
            let candidate = approx_poly_dp(contour, eps_factor * peri);
            if candidate.len() == 4 {
                approx = Some(candidate);
                break;
            }
        }
        let Some(approx) = approx else { continue };
        let ordered = order_quad(&approx);
        let Some(H) = solve_homography(&sheet_mm, &ordered) else {
            continue;
        };
        let projected_ring = project1(&H, card.ring_centre_mm);
        if norm(sub(projected_ring, ring_centroid)) <= 30.0 {
            return Some(H);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Canny + Hough (for the fallback quad seed)

/// cv2.Canny(img, 50, 150): Sobel L1 magnitude on the raw input
/// (OpenCV's Canny has NO internal blur), sector non-max
/// suppression over the full gradient field, hysteresis. NMS ties
/// use OpenCV's asymmetric rule (strictly greater than one
/// neighbour, >= the other) so a 2-px magnitude plateau collapses
/// to one pixel instead of two.
fn canny_edges(img: &[u8], w: usize, h: usize) -> Vec<u8> {
    let at = |x: i64, y: i64| -> f64 {
        let cx = x.clamp(0, w as i64 - 1) as usize;
        let cy = y.clamp(0, h as i64 - 1) as usize;
        img[cy * w + cx] as f64
    };
    let mut mag = vec![0.0f64; w * h];
    let mut ang = vec![0.0f64; w * h];
    for y in 0..h as i64 {
        for x in 0..w as i64 {
            let gx = -at(x - 1, y - 1) + at(x + 1, y - 1) - 2.0 * at(x - 1, y) + 2.0 * at(x + 1, y)
                - at(x - 1, y + 1)
                + at(x + 1, y + 1);
            let gy = -at(x - 1, y - 1) - 2.0 * at(x, y - 1) - at(x + 1, y - 1)
                + at(x - 1, y + 1)
                + 2.0 * at(x, y + 1)
                + at(x + 1, y + 1);
            let idx = y as usize * w + x as usize;
            mag[idx] = gx.abs() + gy.abs();
            let mut a = gy.atan2(gx);
            if a < 0.0 {
                a += std::f64::consts::PI;
            }
            ang[idx] = a;
        }
    }
    let mut candidate = vec![false; w * h];
    for y in 0..h as i64 {
        for x in 0..w as i64 {
            let idx = y as usize * w + x as usize;
            let m = mag[idx];
            if m <= 50.0 {
                continue;
            }
            let sector = ((ang[idx] + std::f64::consts::PI / 8.0) / (std::f64::consts::PI / 4.0))
                .floor() as usize
                % 4;
            // (strict-side neighbour, weak-side neighbour) along
            // the gradient direction.
            let (n1, n2) = match sector {
                0 => ((x - 1, y), (x + 1, y)),
                1 => ((x - 1, y - 1), (x + 1, y + 1)),
                2 => ((x, y - 1), (x, y + 1)),
                _ => ((x - 1, y + 1), (x + 1, y - 1)),
            };
            let get = |p: (i64, i64)| -> f64 {
                if p.0 < 0 || p.1 < 0 || p.0 as usize >= w || p.1 as usize >= h {
                    0.0
                } else {
                    mag[p.1 as usize * w + p.0 as usize]
                }
            };
            if m > get(n1) && m >= get(n2) {
                candidate[idx] = true;
            }
        }
    }
    // Hysteresis: strong pixels plus candidate pixels connected
    // (8-conn) to a strong one.
    let mut out = vec![0u8; w * h];
    let mut stack: Vec<usize> = Vec::new();
    for i in 0..w * h {
        if candidate[i] && mag[i] > 150.0 {
            out[i] = 255;
            stack.push(i);
        }
    }
    while let Some(i) = stack.pop() {
        let (x, y) = ((i % w) as i64, (i / w) as i64);
        for dy in -1..=1i64 {
            for dx in -1..=1i64 {
                let (nx, ny) = (x + dx, y + dy);
                if nx < 0 || ny < 0 || nx as usize >= w || ny as usize >= h {
                    continue;
                }
                let nidx = ny as usize * w + nx as usize;
                if candidate[nidx] && out[nidx] == 0 {
                    out[nidx] = 255;
                    stack.push(nidx);
                }
            }
        }
    }
    out
}

#[derive(Clone)]
struct HoughSeg {
    length: f64,
    rho: f64,
    normal: Pt,
    direction: Pt,
}

/// Deterministic stand-in for cv2.HoughLinesP(edges, 1, pi/180,
/// threshold=80, minLineLength=110, maxLineGap=20). OpenCV's
/// progressive Hough is randomized, so exact segment parity is
/// impossible; this keeps the same accumulator geometry and
/// thresholds and extracts runs along each strong line, which is
/// all the quad seed consumes (length, rho, normal, direction).
fn hough_segments(edges: &[u8], w: usize, h: usize) -> Vec<HoughSeg> {
    let mut points: Vec<(i64, i64)> = Vec::new();
    for y in 0..h {
        for x in 0..w {
            if edges[y * w + x] > 0 {
                points.push((x as i64, y as i64));
            }
        }
    }
    if points.is_empty() {
        return Vec::new();
    }
    let diag = ((w * w + h * h) as f64).sqrt().ceil() as i64;
    let rho_bins = (2 * diag + 1) as usize;
    let mut accum = vec![0u32; 180 * rho_bins];
    let mut trig: Vec<(f64, f64)> = Vec::with_capacity(180);
    for k in 0..180 {
        let theta = k as f64 * std::f64::consts::PI / 180.0;
        trig.push((theta.cos(), theta.sin()));
    }
    for &(x, y) in &points {
        for (k, &(c, s)) in trig.iter().enumerate() {
            let rho = (x as f64 * c + y as f64 * s).round() as i64 + diag;
            accum[k * rho_bins + rho as usize] += 1;
        }
    }
    let mut cells: Vec<(u32, usize, i64)> = Vec::new();
    for k in 0..180 {
        for r in 0..rho_bins {
            let votes = accum[k * rho_bins + r];
            if votes > 80 {
                cells.push((votes, k, r as i64 - diag));
            }
        }
    }
    cells.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let mut used = vec![false; points.len()];
    let mut segments: Vec<HoughSeg> = Vec::new();
    for &(_, k, rho) in cells.iter() {
        if segments.len() >= 300 {
            break;
        }
        let (c, s) = trig[k];
        let mut members: Vec<(f64, usize)> = Vec::new(); // (t, point idx)
        for (i, &(x, y)) in points.iter().enumerate() {
            if used[i] {
                continue;
            }
            let r = (x as f64 * c + y as f64 * s).round() as i64;
            if r == rho {
                members.push((-(x as f64) * s + y as f64 * c, i));
            }
        }
        if members.len() < 2 {
            continue;
        }
        members.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut run_start = 0usize;
        for j in 1..=members.len() {
            let end_run = j == members.len() || members[j].0 - members[j - 1].0 > 20.0;
            if !end_run {
                continue;
            }
            let run = &members[run_start..j];
            run_start = j;
            if run.len() < 2 {
                continue;
            }
            let extent = run[run.len() - 1].0 - run[0].0;
            if extent < 110.0 {
                continue;
            }
            // HoughLinesP reports endpoints ON the accumulator
            // line (normal (c, s), rho = bin): the extreme run
            // parameters projected onto the ideal line, not the
            // raw edge pixels. Using raw pixels instead adds
            // ~1.4 px of endpoint noise, which skews the line
            // direction and every quad built from it.
            let d0 = pt(-s, c);
            let base = pt(c * rho as f64, s * rho as f64);
            let p = add(base, scale(d0, run[0].0));
            let q = add(base, scale(d0, run[run.len() - 1].0));
            let length = norm(sub(q, p));
            if length < 110.0 {
                continue;
            }
            let mut direction = d0;
            if direction[0] < 0.0 {
                direction = scale(direction, -1.0);
            }
            let normal = pt(-direction[1], direction[0]);
            let seg_rho = dot(normal, p);
            for &(_, idx) in run {
                used[idx] = true;
            }
            segments.push(HoughSeg {
                length,
                rho: seg_rho,
                normal,
                direction,
            });
        }
    }
    segments
}

/// Line pairs in one orientation family whose rho interval
/// brackets the ring centre at a plausible sheet separation.
fn pairs_bracketing<'a>(
    fam: &[&'a HoughSeg],
    card: &CardProfile,
    ring_centroid: Pt,
    scale0: f64,
) -> Vec<(&'a HoughSeg, &'a HoughSeg)> {
    let n_mean = pt(
        fam.iter().map(|s| s.normal[0]).sum::<f64>() / fam.len() as f64,
        fam.iter().map(|s| s.normal[1]).sum::<f64>() / fam.len() as f64,
    );
    let expected_px = if n_mean[1].abs() > n_mean[0].abs() {
        card.card_height_mm
    } else {
        card.card_width_mm
    } * scale0;
    let mut out = Vec::new();
    for i in 0..fam.len() {
        for j in (i + 1)..fam.len() {
            let (r1, r2) = (fam[i].rho, fam[j].rho);
            let centre_side = dot(fam[i].normal, ring_centroid);
            let (lo, hi) = (r1.min(r2), r1.max(r2));
            if lo < centre_side
                && centre_side < hi
                && 0.45 * expected_px < hi - lo
                && hi - lo < 2.2 * expected_px
            {
                out.push((fam[i], fam[j]));
            }
        }
    }
    out
}

/// Projective seed from Hough line quads, or None (port of
/// `_hough_quad_seed`).
fn hough_quad_seed(
    frame: &MeasureFrame,
    mask: &[u8],
    card: &CardProfile,
    all_segs: &[HoughSeg],
    ring_centroid: Pt,
    scale0: f64,
) -> Option<H3> {
    let mut segs: Vec<HoughSeg> = all_segs.to_vec();
    if segs.len() < 4 {
        return None;
    }
    segs.sort_by(|a, b| b.length.total_cmp(&a.length));
    segs.truncate(28);
    let ref_ang = segs[0].normal[1].atan2(segs[0].normal[0]);
    let (mut fam_a, mut fam_b): (Vec<&HoughSeg>, Vec<&HoughSeg>) = (Vec::new(), Vec::new());
    for s in &segs {
        let ang = s.normal[1].atan2(s.normal[0]);
        if (ref_ang - ang).sin().abs() < 0.35 {
            fam_a.push(s);
        } else {
            fam_b.push(s);
        }
    }
    if fam_a.len() < 2 || fam_b.len() < 2 {
        return None;
    }
    let sheet_mm = [
        pt(0.0, 0.0),
        pt(card.card_width_mm, 0.0),
        pt(card.card_width_mm, card.card_height_mm),
        pt(0.0, card.card_height_mm),
    ];
    let pairs_a = pairs_bracketing(&fam_a, card, ring_centroid, scale0);
    let pairs_b = pairs_bracketing(&fam_b, card, ring_centroid, scale0);
    let mut quads: Vec<H3> = Vec::new();
    for &(a1, a2) in &pairs_a {
        for &(b1, b2) in &pairs_b {
            let mut corners = Vec::with_capacity(4);
            let mut ok = true;
            for s_a in [a1, a2] {
                for s_b in [b1, b2] {
                    let pa = scale(s_a.normal, s_a.rho);
                    let pb = scale(s_b.normal, s_b.rho);
                    match line_intersection(pa, s_a.direction, pb, s_b.direction) {
                        Some(p) => corners.push(p),
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok {
                    break;
                }
            }
            if !ok || corners.len() != 4 {
                continue;
            }
            let ordered = order_quad(&corners);
            let Some(H) = solve_homography(&sheet_mm, &ordered) else {
                continue;
            };
            let projected_ring = project1(&H, card.ring_centre_mm);
            if norm(sub(projected_ring, ring_centroid)) > 30.0 {
                continue;
            }
            quads.push(H);
        }
    }
    // Python scores only the first 12 ring-passing quads, in an
    // order set by HoughLinesP's randomized extraction; which
    // quads land in the 12 is chance, and the function's own
    // docstring states the intent ("the survivor with the best
    // loose tick score wins"). Score every ring-passing quad
    // (bounded) and take the best: deterministic, and faithful
    // to the documented selection rule.
    let mut best: Option<(f64, H3)> = None;
    for H in quads.iter().take(512) {
        let tm = tick_matches(frame, mask, card, H, false, false);
        let score = tm.arms_hit as f64 + 0.02 * tm.matched.len() as f64;
        if best.is_none() || score > best.unwrap().0 {
            best = Some((score, *H));
        }
    }
    best.map(|(_, H)| H)
}

/// Similarity mapping card-mm -> image, anchored at the ring.
fn similarity(ring_mm: Pt, centre_img: Pt, theta: f64, scale: f64) -> H3 {
    let (c, s) = (theta.cos(), theta.sin());
    let r_ring = pt(
        c * ring_mm[0] - s * ring_mm[1],
        s * ring_mm[0] + c * ring_mm[1],
    );
    let t = sub(centre_img, pt(r_ring[0] * scale, r_ring[1] * scale));
    [
        scale * c,
        -scale * s,
        t[0],
        scale * s,
        scale * c,
        t[1],
        0.0,
        0.0,
        1.0,
    ]
}

// ---------------------------------------------------------------------------
// Homography fitting: RANSAC (OpenCV clone) + weighted least squares

/// cv::RNG as seeded by RANSACPointSetRegistrator::run:
/// RNG((uint64)-1), fresh for every call, hence deterministic.
struct CvRng {
    state: u64,
}

impl CvRng {
    fn new() -> CvRng {
        CvRng { state: u64::MAX }
    }
    fn next(&mut self) -> u32 {
        self.state = (self.state as u32 as u64)
            .wrapping_mul(4164903690u64)
            .wrapping_add((self.state >> 32) as u32 as u64);
        self.state as u32
    }
    fn uniform(&mut self, a: i32, b: i32) -> i32 {
        if a == b {
            a
        } else {
            (self.next() % ((b - a) as u32)) as i32 + a
        }
    }
}

/// precomp.hpp haveCollinearPoints: only triples containing the
/// LAST point are tested (an OpenCV quirk, kept faithfully).
fn have_collinear_points(pts: &[Pt; 4]) -> bool {
    let i = 3;
    for j in 0..i {
        let dx1 = pts[j][0] - pts[i][0];
        let dy1 = pts[j][1] - pts[i][1];
        for k in 0..j {
            let dx2 = pts[k][0] - pts[i][0];
            let dy2 = pts[k][1] - pts[i][1];
            if (dx2 * dy1 - dy2 * dx1).abs()
                <= FLT_EPSILON * (dx1.abs() + dy1.abs() + dx2.abs() + dy2.abs())
            {
                return true;
            }
        }
    }
    false
}

/// HomographyEstimatorCallback::checkSubset for a 4-subset:
/// no collinear points, and the four triples' orientation signs
/// must agree between src and dst (all same or all flipped).
fn check_subset(src: &[Pt; 4], dst: &[Pt; 4]) -> bool {
    if have_collinear_points(src) || have_collinear_points(dst) {
        return false;
    }
    let det3 = |p: &[Pt; 4], t: [usize; 3]| -> f64 {
        let (a, b, c) = (p[t[0]], p[t[1]], p[t[2]]);
        a[0] * (b[1] - c[1]) + b[0] * (c[1] - a[1]) + c[0] * (a[1] - b[1])
    };
    let mut negative = 0;
    for t in [[0, 1, 2], [1, 2, 3], [0, 2, 3], [0, 1, 3]] {
        if det3(src, t) * det3(dst, t) < 0.0 {
            negative += 1;
        }
    }
    negative == 0 || negative == 4
}

/// cv::RANSACUpdateNumIters.
fn ransac_update_num_iters(p: f64, ep: f64, model_points: i32, max_iters: i64) -> i64 {
    let p = p.clamp(0.0, 1.0);
    let ep = ep.clamp(0.0, 1.0);
    let num = (1.0 - p).max(DBL_MIN);
    let denom = 1.0 - (1.0 - ep).powi(model_points);
    if denom < DBL_MIN {
        return 0;
    }
    let num = num.ln();
    let denom = denom.ln();
    if denom >= 0.0 || -num >= max_iters as f64 * (-denom) {
        max_iters
    } else {
        (num / denom).round_ties_even() as i64
    }
}

/// findInliers with the callback's float32 error: squared forward
/// reprojection error <= threshold^2 (9.0 for the 3.0 default).
fn find_inliers_f32(pairs: &[(Pt, Pt)], H: &H3) -> (Vec<bool>, usize) {
    let hf: [f32; 9] = std::array::from_fn(|i| H[i] as f32);
    let mut mask = vec![false; pairs.len()];
    let mut count = 0usize;
    for (i, (big_m, m)) in pairs.iter().enumerate() {
        let (x, y) = (big_m[0] as f32, big_m[1] as f32);
        let ww = 1.0f32 / (hf[6] * x + hf[7] * y + hf[8]);
        let dx = (hf[0] * x + hf[1] * y + hf[2]) * ww - m[0] as f32;
        let dy = (hf[3] * x + hf[4] * y + hf[5]) * ww - m[1] as f32;
        let err = dx * dx + dy * dy;
        if err <= 9.0f32 {
            mask[i] = true;
            count += 1;
        }
    }
    (mask, count)
}

/// cv2.findHomography(src, dst, RANSAC, 3.0) at the defaults
/// (confidence 0.995, maxIters 2000): RANSACPointSetRegistrator
/// with the homography callback — fresh RNG per call, getSubset
/// with checkSubset (10000 attempts), the adaptive iteration
/// update, and findHomography's post-loop inlier kernel refit +
/// LM refinement. The returned mask is the SAMPLING mask.
fn ransac_homography(pairs_in: &[(Pt, Pt)]) -> Option<(H3, Vec<bool>)> {
    let pairs = to_f32_pairs(pairs_in);
    let n = pairs.len();
    if n < 4 {
        return None;
    }
    if n == 4 {
        let H = dlt_kernel(&pairs)?;
        return Some((H, vec![true; 4]));
    }
    let mut rng = CvRng::new();
    let mut niters: i64 = 2000;
    let mut max_good = 0usize;
    let mut best: Option<(H3, Vec<bool>)> = None;
    let mut iter: i64 = 0;
    while iter < niters {
        // getSubset: rejection-sampled distinct indices until
        // checkSubset passes (10000 attempts).
        let mut subset: Option<[usize; 4]> = None;
        for _attempt in 0..10000 {
            let mut idx = [0usize; 4];
            for i in 0..4 {
                loop {
                    let cand = rng.uniform(0, n as i32) as usize;
                    if !idx[..i].contains(&cand) {
                        idx[i] = cand;
                        break;
                    }
                }
            }
            let s4: [Pt; 4] = std::array::from_fn(|i| pairs[idx[i]].0);
            let d4: [Pt; 4] = std::array::from_fn(|i| pairs[idx[i]].1);
            if check_subset(&s4, &d4) {
                subset = Some(idx);
                break;
            }
        }
        let Some(idx) = subset else {
            if iter == 0 {
                return None;
            }
            break;
        };
        iter += 1;
        let sample: Vec<(Pt, Pt)> = idx.iter().map(|&i| pairs[i]).collect();
        let Some(H) = dlt_kernel(&sample) else {
            continue;
        };
        let (mask, good) = find_inliers_f32(&pairs, &H);
        if good > max_good.max(3) {
            max_good = good;
            best = Some((H, mask));
            niters = ransac_update_num_iters(0.995, (n - good) as f64 / n as f64, 4, niters);
        }
    }
    if max_good == 0 {
        return None;
    }
    let (best_H, mask) = best?;
    // findHomography post-process: compress to inliers, kernel
    // refit, LM refinement, scaleFor normalization.
    let kept: Vec<(Pt, Pt)> = pairs
        .iter()
        .zip(mask.iter())
        .filter(|(_, &k)| k)
        .map(|(p, _)| *p)
        .collect();
    let mut H = best_H;
    if !kept.is_empty() {
        if kept.len() >= 4 {
            if let Some(Hk) = dlt_kernel(&kept) {
                H = Hk;
            }
        }
        lm_refine(&kept, &mut H);
        normalize_h(&mut H);
    }
    Some((H, mask))
}

fn solve_homography_pairs(pairs: &[(Pt, Pt)]) -> Option<H3> {
    let src: Vec<Pt> = pairs.iter().map(|p| p.0).collect();
    let dst: Vec<Pt> = pairs.iter().map(|p| p.1).collect();
    solve_homography(&src, &dst)
}

// ---------------------------------------------------------------------------
// Card detection

fn fit_homography(tick_pairs: &[(Pt, Pt)], corner_pairs: &[(Pt, Pt)]) -> Option<H3> {
    let mut pool: Vec<(Pt, Pt)> = tick_pairs.to_vec();
    pool.extend_from_slice(corner_pairs);
    let (H_r, inliers) = ransac_homography(&pool)?;
    let n_inliers: usize = inliers.iter().filter(|&&b| b).count();
    let (ticks_kept, corners_kept): (Vec<(Pt, Pt)>, Vec<(Pt, Pt)>) = if n_inliers >= 8 {
        let n_ticks = tick_pairs.len();
        (
            tick_pairs
                .iter()
                .zip(inliers[..n_ticks].iter())
                .filter(|(_, &k)| k)
                .map(|(p, _)| *p)
                .collect(),
            corner_pairs
                .iter()
                .zip(inliers[n_ticks..].iter())
                .filter(|(_, &k)| k)
                .map(|(p, _)| *p)
                .collect(),
        )
    } else {
        (tick_pairs.to_vec(), corner_pairs.to_vec())
    };
    let mut weighted: Vec<(Pt, Pt)> = ticks_kept;
    for _ in 0..6 {
        weighted.extend_from_slice(&corners_kept);
    }
    match solve_homography_pairs(&weighted) {
        Some(H) => Some(H),
        None => Some(H_r),
    }
}

/// A successful card detection: the fitted homography, the tick pairs
/// it rests on, the projected frame quad, and the fit RMS.
pub struct Detection {
    pub H: H3,
    pub tick_pairs: Vec<(Pt, Pt)>,
    /// Projected frame vertices, TL/TR/BR/BL order.
    pub quad: [Pt; 4],
    pub rms: f64,
    /// Strict re-observation at the accepted fit: tick cuts and arms
    /// the strict matcher finds at `H` (see `STRICT_COLLAPSE_*`).
    pub strict_ticks: usize,
    pub strict_arms: usize,
}

/// Strict re-verification after the size gates. The fit pipeline
/// adopts a strict re-observation only when it confirms (>= 10 strict
/// ticks) and otherwise grades its gates on the loose pairs it kept,
/// so a fit can stand on loose evidence alone. At or below this many
/// strict tick cuts AND arms at the accepted fit, the card's tick
/// structure is not there and the estimate refuses (Python parity:
/// agenthand main c5f5929, `estimate`).
const STRICT_COLLAPSE_MAX_TICKS: usize = 2;
const STRICT_COLLAPSE_MAX_ARMS: usize = 1;

/// Fit + strict refits; the span gate lives here (port of the
/// Python `pipeline` closure inside `_detect_card`).
fn fit_pipeline(
    frame: &MeasureFrame,
    mask: &[u8],
    card: &CardProfile,
    mut tick_pairs: Vec<(Pt, Pt)>,
    mut corner_pairs: Vec<(Pt, Pt)>,
) -> Option<(H3, Vec<(Pt, Pt)>, f64)> {
    let mut H = fit_homography(&tick_pairs, &corner_pairs)?;
    for _i in 0..2 {
        let tm = tick_matches(frame, mask, card, &H, true, true);
        if tm.matched.len() >= 10 {
            tick_pairs = tm.matched;
            corner_pairs = tm.corners;
        }
        match fit_homography(&tick_pairs, &corner_pairs) {
            Some(H_new) => {
                H = H_new;
            }
            None => break,
        }
    }
    let tl = card.frame["TL"];
    let br = card.frame["BR"];
    let frame_span = sub(br, tl);
    let mut lo = pt(f64::INFINITY, f64::INFINITY);
    let mut hi = pt(f64::NEG_INFINITY, f64::NEG_INFINITY);
    for (src, _) in tick_pairs.iter().chain(corner_pairs.iter()) {
        lo = pt(lo[0].min(src[0]), lo[1].min(src[1]));
        hi = pt(hi[0].max(src[0]), hi[1].max(src[1]));
    }
    let span = sub(hi, lo);
    if span[0] < 0.6 * frame_span[0] || span[1] < 0.6 * frame_span[1] || tick_pairs.len() < 12 {
        return None;
    }
    let mut sse = 0.0;
    for (src, dst) in &tick_pairs {
        let p = project1(&H, *src);
        let d = norm(sub(p, *dst));
        sse += d * d;
    }
    let rms = (sse / tick_pairs.len() as f64).sqrt();
    Some((H, tick_pairs, rms))
}

// ---------------------------------------------------------------------------
// Card verification (the acceptance gate)
//
// The seeded fit proves only that some quad's arm lines cross
// bright/dark runs at tick-like spacing, and periodic texture does
// that too: IMG_1403's 640-px preview fitted 12/24 "ticks" on bare
// fence boards (the card itself is ~25 px across), and the IMG_1435 /
// IMG_1491 jacket frames fitted up to 21/24 on denim seams. The ring
// anchor that seeds the search is the centroid of every cyan-hued
// pixel in the frame, so sky, teal play equipment or denim pull it
// off the card. The winning fit is therefore accepted only when it
// shows the card's unique printed feature, the cyan ring concentrated
// at the ring position, and then, as secondary checks, a bright paper
// border around dark arms, at least half the tick cuts, and the
// card's aspect ratio. Anything else refuses; nothing is re-fitted.

/// Minimum fraction of the profile's tick cuts a verified fit matches.
const VERIFY_MIN_TICK_FRAC: f64 = 0.5;
/// Ring: on the best circle about the ring centre, at least this
/// fraction of samples is ring-cyan (the printed ring is three 92-deg
/// arcs, so a clean ring reads ~0.77 at its mid radius), spread over
/// at least this many of six sectors.
const VERIFY_MIN_RING_FRAC: f64 = 0.3;
const VERIFY_MIN_RING_SECTORS: usize = 4;
/// Circles just outside the ring may carry at most this share of the
/// ring's cyan: on a card the cyan is concentrated on the ring; sky,
/// denim or painted surfaces spread it evenly.
const VERIFY_MAX_RING_SPILL: f64 = 0.35;
const RING_SAMPLES: usize = 96;
const RING_SECTORS: usize = 6;
/// Paper border vs arm ink (8-bit grey medians): minimum ratio and
/// minimum difference.
const VERIFY_MIN_PAPER_RATIO: f64 = 2.0;
const VERIFY_MIN_PAPER_DIFF: f64 = 40.0;
/// The projected sheet's width:height may differ from the card's by
/// at most this factor either way.
const VERIFY_MAX_ASPECT_FACTOR: f64 = 1.5;

/// Pixel index of the nearest pixel to `p`, or None off-frame.
fn pixel_index(frame: &MeasureFrame, p: Pt) -> Option<usize> {
    let (x, y) = (p[0].round(), p[1].round());
    if !(x >= 0.0 && y >= 0.0 && x < frame.width as f64 && y < frame.height as f64) {
        return None;
    }
    Some(y as usize * frame.width + x as usize)
}

/// The ring's cyan test (HSV inRange (88,70,70)-(104,255,255)).
fn is_ring_cyan(frame: &MeasureFrame, idx: usize) -> bool {
    let (hh, ss, vv) = frame.hsv(idx);
    (88.0..=104.0).contains(&hh) && ss >= 70.0 && vv >= 70.0
}

/// Measurements the acceptance gate reads off one fitted homography.
#[derive(Clone, Copy, Debug)]
struct CardEvidence {
    tick_frac: f64,
    /// Best cyan fraction along one circle about the ring centre, and
    /// how many of the circle's six sectors hold cyan at that radius.
    ring_frac: f64,
    ring_sectors: usize,
    /// Cyan fraction on circles just outside the profile's ring.
    spill_frac: f64,
    /// Median grey of the paper border and of the arm ink.
    paper_grey: f64,
    ink_grey: f64,
    /// Projected sheet aspect / card aspect.
    aspect_factor: f64,
}

fn card_evidence(frame: &MeasureFrame, card: &CardProfile, H: &H3, n_ticks: usize) -> CardEvidence {
    let (cw, ch) = (card.card_width_mm, card.card_height_mm);
    let on_sheet = |q: Pt| q[0] >= 0.0 && q[1] >= 0.0 && q[0] <= cw && q[1] <= ch;

    // Ring: cyan along circles about the ring centre. The radius is
    // searched (0.25-0.95 of the profile's outer radius) rather than
    // taken from the profile. The printed ring's band is 0.62-1.0 of
    // its outer radius; until 2026-11-02 the print profile carried a
    // ring 2.33x the printed one (AH-BIG outer 36.75 mm against 15.79
    // printed), and the synthetic battery is still rendered and
    // checked with those radii (tests/fixtures/scaled-card-profiles.json),
    // so the search covers both. Off-frame samples count as not cyan.
    let c = card.ring_centre_mm;
    let r_out = card.ring_outer_mm;
    let cyan_at =
        |q: Pt| pixel_index(frame, project1(H, q)).is_some_and(|i| is_ring_cyan(frame, i));
    let circle = |r: f64| -> Vec<bool> {
        (0..RING_SAMPLES)
            .map(|k| {
                let a = k as f64 * std::f64::consts::TAU / RING_SAMPLES as f64;
                cyan_at(pt(c[0] + r * a.cos(), c[1] + r * a.sin()))
            })
            .collect()
    };
    let (mut ring_frac, mut ring_sectors) = (0.0f64, 0usize);
    for step in 5..=19 {
        let hits = circle(step as f64 * 0.05 * r_out);
        let f = hits.iter().filter(|&&b| b).count() as f64 / hits.len() as f64;
        if f > ring_frac {
            ring_frac = f;
            ring_sectors = hits
                .chunks(RING_SAMPLES / RING_SECTORS)
                .filter(|chunk| chunk.iter().any(|&b| b))
                .count();
        }
    }
    // Spill: the same test just outside the profile's ring, on the
    // sheet only.
    let (mut spill_n, mut spill_c) = (0usize, 0usize);
    for f in [1.2, 1.35, 1.5] {
        let r = f * r_out;
        for k in 0..RING_SAMPLES {
            let a = k as f64 * std::f64::consts::TAU / RING_SAMPLES as f64;
            let q = pt(c[0] + r * a.cos(), c[1] + r * a.sin());
            if on_sheet(q) {
                spill_n += 1;
                spill_c += cyan_at(q) as usize;
            }
        }
    }
    let spill_frac = if spill_n == 0 {
        0.0
    } else {
        spill_c as f64 / spill_n as f64
    };

    // Paper: the midline of the white border between the trim and
    // the bracket frame's outer edge, all four sides.
    let half = card.stroke_mm / 2.0;
    let (tl, tr, bl, br) = (
        card.frame["TL"],
        card.frame["TR"],
        card.frame["BL"],
        card.frame["BR"],
    );
    let xl = (tl[0].min(bl[0]) - half) / 2.0;
    let xr = (tr[0].max(br[0]) + half + cw) / 2.0;
    let yt = (tl[1].min(tr[1]) - half) / 2.0;
    let yb = (bl[1].max(br[1]) + half + ch) / 2.0;
    let mut paper_pts: Vec<Pt> = Vec::new();
    for t in linspace(0.0, 1.0, 9) {
        let x = xl + t * (xr - xl);
        let y = yt + t * (yb - yt);
        paper_pts.extend_from_slice(&[pt(x, yt), pt(x, yb), pt(xl, y), pt(xr, y)]);
    }
    // Ink: arm centrelines, clear of the tick cuts.
    let mut ink_pts: Vec<Pt> = Vec::new();
    for arm in arms(card) {
        let axis = sub(arm.end_mm, arm.start_mm);
        for f in [0.1, 0.375, 0.625, 0.9] {
            ink_pts.push(add(arm.start_mm, scale(axis, f)));
        }
    }
    let greys = |pts: &[Pt]| -> (Vec<f64>, usize) {
        let vals: Vec<f64> = pts
            .iter()
            .filter_map(|q| pixel_index(frame, project1(H, *q)).map(|i| frame.gray[i] as f64))
            .collect();
        let n = vals.len();
        (vals, n)
    };
    let (paper_vals, paper_seen) = greys(&paper_pts);
    let (ink_vals, ink_seen) = greys(&ink_pts);
    // A card mostly out of frame cannot be verified.
    let (paper_grey, ink_grey) = if 2 * paper_seen < paper_pts.len() || 2 * ink_seen < ink_pts.len()
    {
        (0.0, 255.0)
    } else {
        (percentile(&paper_vals, 50.0), percentile(&ink_vals, 50.0))
    };

    // Aspect of the projected sheet.
    let corners = [
        project1(H, pt(0.0, 0.0)),
        project1(H, pt(cw, 0.0)),
        project1(H, pt(cw, ch)),
        project1(H, pt(0.0, ch)),
    ];
    let across = norm(sub(corners[1], corners[0])) + norm(sub(corners[2], corners[3]));
    let down = norm(sub(corners[3], corners[0])) + norm(sub(corners[2], corners[1]));
    let aspect_factor = if down > 0.0 && ch > 0.0 {
        (across / down) / (cw / ch)
    } else {
        0.0
    };

    let total_ticks = card.ticks.len().max(1);
    CardEvidence {
        tick_frac: n_ticks as f64 / total_ticks as f64,
        ring_frac,
        ring_sectors,
        spill_frac,
        paper_grey,
        ink_grey,
        aspect_factor,
    }
}

/// Ok when the fitted quad shows the printed card; otherwise the
/// first failing check, worded for the refusal message.
fn verify_card(
    frame: &MeasureFrame,
    card: &CardProfile,
    H: &H3,
    n_ticks: usize,
) -> Result<(), String> {
    let ev = card_evidence(frame, card, H, n_ticks);
    // The ring is the card's one unique printed feature: it anchors
    // acceptance. Ticks, paper and aspect are secondary checks.
    if ev.ring_frac < VERIFY_MIN_RING_FRAC
        || ev.ring_sectors < VERIFY_MIN_RING_SECTORS
        || ev.spill_frac > VERIFY_MAX_RING_SPILL * ev.ring_frac
    {
        return Err("no cyan ring at the card's ring position".to_string());
    }
    if ev.paper_grey < VERIFY_MIN_PAPER_RATIO * ev.ink_grey.max(1.0)
        || ev.paper_grey - ev.ink_grey < VERIFY_MIN_PAPER_DIFF
    {
        return Err("no white paper border around dark bracket arms".to_string());
    }
    if ev.tick_frac < VERIFY_MIN_TICK_FRAC {
        return Err(format!(
            "only {n_ticks}/{} tick cuts matched",
            card.ticks.len()
        ));
    }
    if !(1.0 / VERIFY_MAX_ASPECT_FACTOR..=VERIFY_MAX_ASPECT_FACTOR).contains(&ev.aspect_factor) {
        return Err(format!(
            "the fitted quad's aspect is {:.2}x the card's",
            ev.aspect_factor
        ));
    }
    Ok(())
}

/// Why one ring candidate produced no accepted fit.
enum AnchorFailure {
    /// No seed passed loose tick observation.
    NoObservation,
    /// Seeds observed, but no fit passed the span/tick/RMS gates.
    NoConvergence,
    /// Fits converged; the best one failed the acceptance gate.
    NotVerified(String),
}

/// Shared per-photo inputs for the seed search (computed once).
struct SeedInputs<'a> {
    mask: &'a [u8],
    contours: &'a [Vec<Pt>],
    hough: std::cell::OnceCell<Vec<HoughSeg>>,
}

/// Seed search, fit and verification for one ring candidate (the body
/// of the Python `_detect_card`, run from each of the candidate's
/// anchors instead of the all-cyan centroid). An anchor is a ring
/// centre in the image and a px-per-card-mm scale. Returns the best
/// fit that passes the acceptance gate.
fn fit_at_anchors(
    frame: &MeasureFrame,
    card: &CardProfile,
    inputs: &SeedInputs,
    anchors: &[(Pt, f64)],
) -> Result<(H3, Vec<(Pt, Pt)>, f64), AnchorFailure> {
    let mask = inputs.mask;
    let ring_mm = card.ring_centre_mm;

    let objective = |centre: Pt, params: &[f64; 4]| -> (f64, Vec<(Pt, Pt)>, usize) {
        let anchor = add(centre, pt(params[0], params[1]));
        let H = similarity(ring_mm, anchor, params[2], params[3].exp());
        let tm = tick_matches(frame, mask, card, &H, false, false);
        let mut pairs = tm.matched.clone();
        pairs.extend_from_slice(&tm.corners);
        (
            tm.arms_hit as f64 + 0.02 * (tm.matched.len() + tm.corners.len()) as f64,
            pairs,
            tm.arms_hit,
        )
    };
    let loose_score = |H: &H3| -> f64 {
        let tm = tick_matches(frame, mask, card, H, false, false);
        tm.arms_hit as f64 + 0.02 * (tm.matched.len() + tm.corners.len()) as f64
    };
    // Coarse grid over rotation x scale about one anchor: its top
    // seeds in distinct rotation buckets.
    let grid_seeds = |centre: Pt, scale0: f64| -> Vec<[f64; 4]> {
        let mut grid: Vec<(f64, [f64; 4])> = Vec::new();
        for theta_deg in (0..360).step_by(10) {
            for factor in [0.86, 0.92, 0.98, 1.04, 1.10] {
                let trial = [
                    0.0,
                    0.0,
                    (theta_deg as f64).to_radians(),
                    (scale0 * factor).ln(),
                ];
                let (value, _, _) = objective(centre, &trial);
                grid.push((value, trial));
            }
        }
        grid.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut seeds: Vec<[f64; 4]> = Vec::new();
        for (_, trial) in &grid {
            let theta_deg = trial[2].to_degrees().rem_euclid(360.0);
            let distinct = seeds.iter().all(|s: &[f64; 4]| {
                let other = s[2].to_degrees().rem_euclid(360.0);
                let d = (theta_deg - other).abs();
                d.min(360.0 - d) >= 20.0
            });
            if distinct {
                seeds.push(*trial);
            }
            if seeds.len() >= 3 {
                break;
            }
        }
        seeds
    };

    let frame_keys = ["TL", "TR", "BR", "BL"];
    let frame_src: Vec<Pt> = frame_keys.iter().map(|k| card.frame[*k]).collect();

    // Similarity ascent + projective vertex search from one seed.
    let refine_seed = |centre: Pt, params0: &[f64; 4]| -> [H3; 2] {
        let mut params = *params0;
        let (mut value, _, _) = objective(centre, &params);
        let mut steps = [8.0, 8.0, 1.5f64.to_radians(), 0.03];
        for _round in 0..4 {
            let mut improved = true;
            while improved {
                improved = false;
                for i in 0..4 {
                    for sign in [1.0, -1.0] {
                        let mut trial = params;
                        trial[i] += sign * steps[i];
                        let (trial_value, _, _) = objective(centre, &trial);
                        if trial_value > value + 1e-9 {
                            params = trial;
                            value = trial_value;
                            improved = true;
                        }
                    }
                }
            }
            for s in steps.iter_mut() {
                *s /= 2.0;
            }
        }
        let anchor = add(centre, pt(params[0], params[1]));
        let H_sim = similarity(ring_mm, anchor, params[2], params[3].exp());
        let mut vertices: Vec<Pt> = frame_src.iter().map(|p| project1(&H_sim, *p)).collect();
        let mut best_val = loose_score(&H_sim);
        for step in [16.0, 8.0, 4.0, 2.0] {
            let mut improved = true;
            while improved {
                improved = false;
                for vi in 0..4 {
                    for axis in 0..2 {
                        for sign in [1.0, -1.0] {
                            let mut trial_v = vertices.clone();
                            trial_v[vi][axis] += sign * step;
                            let Some(H_try) = solve_homography(&frame_src, &trial_v) else {
                                continue;
                            };
                            let val = loose_score(&H_try);
                            if val > best_val + 1e-9 {
                                vertices = trial_v;
                                best_val = val;
                                improved = true;
                            }
                        }
                    }
                }
            }
        }
        let H_proj = solve_homography(&frame_src, &vertices).unwrap_or(H_sim);
        [H_sim, H_proj]
    };

    let observe = |H: &H3| -> Option<(Vec<(Pt, Pt)>, Vec<(Pt, Pt)>)> {
        let tm = tick_matches(frame, mask, card, H, false, false);
        if tm.arms_hit >= 4 && tm.matched.len() >= 10 {
            Some((tm.matched, tm.corners))
        } else {
            None
        }
    };
    // Each anchor runs the Python seed procedure on its own (sheet
    // seed, refined grid seeds, and the Hough fallback when those
    // observe nothing), so the observations are a superset of what the
    // Python port finds whenever its anchor is among them.
    let mut seed_observations: Vec<(Vec<(Pt, Pt)>, Vec<(Pt, Pt)>)> = Vec::new();
    for &(centre, scale0) in anchors {
        let mut found: Vec<(Vec<(Pt, Pt)>, Vec<(Pt, Pt)>)> = Vec::new();
        if let Some(H_sheet) = sheet_seed(inputs.contours, card, centre) {
            if let Some(obs) = observe(&H_sheet) {
                found.push(obs);
            }
        }
        for seed in grid_seeds(centre, scale0) {
            for H_seed in refine_seed(centre, &seed) {
                if let Some(obs) = observe(&H_seed) {
                    found.push(obs);
                }
            }
        }
        if found.is_empty() {
            let segs = inputs.hough.get_or_init(|| {
                let edges = canny_edges(mask, frame.width, frame.height);
                hough_segments(&edges, frame.width, frame.height)
            });
            if let Some(H_hough) = hough_quad_seed(frame, mask, card, segs, centre, scale0) {
                if let Some(obs) = observe(&H_hough) {
                    found.push(obs);
                }
            }
        }
        seed_observations.extend(found);
    }
    if seed_observations.is_empty() {
        return Err(AnchorFailure::NoObservation);
    }

    let mut results: Vec<(H3, Vec<(Pt, Pt)>, f64)> = Vec::new();
    for (t0, c0) in &seed_observations {
        if let Some(result) = fit_pipeline(frame, mask, card, t0.clone(), c0.clone()) {
            if result.2 <= 4.0 {
                results.push(result);
            }
        }
    }
    if results.is_empty() {
        return Err(AnchorFailure::NoConvergence);
    }
    results.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.2.total_cmp(&b.2)));
    // Acceptance gate: a fit must show the printed card (its ring
    // first), not just tick-spaced texture. The best fit that passes
    // wins; when none does, the best fit's reason is reported.
    let mut first_reason: Option<String> = None;
    for (H, tick_pairs, rms) in results {
        match verify_card(frame, card, &H, tick_pairs.len()) {
            Ok(()) => return Ok((H, tick_pairs, rms)),
            Err(reason) => {
                first_reason.get_or_insert(reason);
            }
        }
    }
    Err(AnchorFailure::NotVerified(first_reason.unwrap_or_default()))
}

/// Find and verify the card (port of `_detect_card`, with ring-shaped
/// seeding). Every ring-shaped cyan blob anchors its own seed search;
/// the best verified fit over all of them wins; no verified fit
/// refuses.
pub fn detect_card(frame: &MeasureFrame, card: &CardProfile) -> Result<Detection, String> {
    let survey = ring_candidates(frame, card);
    let (cyan_px, candidates) = (survey.count, &survey.candidates);
    // moments m00 sums pixel VALUES (255 per cyan pixel); the Python
    // gate is m00 < 200.
    if (cyan_px as f64) * 255.0 < 200.0 {
        return Err("Cyan ring not found; card not detectable.".to_string());
    }
    if candidates.is_empty() {
        return Err(format!(
            "Card not verified in this photo (no cyan ring found: nothing \
             ring-shaped at least {RING_MIN_OUTER_PX:.0} px in radius) — shoot \
             closer with the card flat and fully in frame."
        ));
    }
    let mask = ink_mask(frame);
    if mask.iter().filter(|&&v| v > 0).count() < 500 {
        return Err("Too little ink; card not detectable.".to_string());
    }
    let contours = sheet_contours(&mask, frame.width, frame.height);
    let inputs = SeedInputs {
        mask: &mask,
        contours: &contours,
        hough: std::cell::OnceCell::new(),
    };

    let mut best: Option<(H3, Vec<(Pt, Pt)>, f64)> = None;
    let mut not_verified: Option<String> = None;
    let mut any_observed = false;
    let ring_area_mm2 =
        std::f64::consts::PI * (card.ring_outer_mm.powi(2) - card.ring_inner_mm.powi(2));
    for cand in candidates {
        // Anchor groups, tried in order; the first group with a
        // verified fit gives this candidate's fit.
        //
        // 1. The Python port's anchor (centroid and area of ALL cyan),
        //    only when it lies on this ring (within two outer radii:
        //    the ring is the only cyan in the photo, as in every
        //    synthetic battery scene). Its seed procedure then runs
        //    unchanged, so clean photos keep the Python fits.
        // 2. The blob's own centre with two px-per-card-mm hypotheses:
        //    its outer radius from the annulus second moment
        //    (independent of how much of the circle is inked: right for
        //    the printed three-arc ring) and its area over the
        //    profile's full annulus (exact for a full ring; 0.88x on
        //    the printed ring).
        let mut groups: Vec<Vec<(Pt, f64)>> = Vec::new();
        if norm(sub(survey.centroid, cand.centre)) <= 2.0 * cand.outer_px {
            groups.push(vec![(
                survey.centroid,
                (cyan_px as f64 / ring_area_mm2).sqrt(),
            )]);
        }
        groups.push(vec![
            (cand.centre, cand.outer_px / card.ring_outer_mm),
            (cand.centre, (cand.n_px as f64 / ring_area_mm2).sqrt()),
        ]);
        for anchors in &groups {
            match fit_at_anchors(frame, card, &inputs, anchors) {
                Ok(fit) => {
                    let better = match &best {
                        None => true,
                        Some(b) => {
                            fit.1.len() > b.1.len() || (fit.1.len() == b.1.len() && fit.2 < b.2)
                        }
                    };
                    if better {
                        best = Some(fit);
                    }
                    break;
                }
                Err(AnchorFailure::NotVerified(reason)) => {
                    any_observed = true;
                    not_verified.get_or_insert(reason);
                }
                Err(AnchorFailure::NoConvergence) => any_observed = true,
                Err(AnchorFailure::NoObservation) => {}
            }
        }
    }
    let Some((H, tick_pairs, rms)) = best else {
        if let Some(reason) = not_verified {
            return Err(format!(
                "Card not verified in this photo ({reason}) — shoot closer \
                 with the card flat and fully in frame."
            ));
        }
        if any_observed {
            return Err("Card fit did not converge to a trustworthy homography \
                 (coverage or residual gates failed)."
                .to_string());
        }
        return Err("Tick verification failed (card too degraded or occluded).".to_string());
    };
    let quad = [
        project1(&H, card.frame["TL"]),
        project1(&H, card.frame["TR"]),
        project1(&H, card.frame["BR"]),
        project1(&H, card.frame["BL"]),
    ];
    let strict = tick_matches(frame, &mask, card, &H, true, false);
    Ok(Detection {
        H,
        tick_pairs,
        quad,
        rms,
        strict_ticks: strict.matched.len(),
        strict_arms: strict.arms_hit,
    })
}

// ---------------------------------------------------------------------------
// Object extent

fn extent_from_points(points: &[Pt]) -> (f64, f64, Pt) {
    let mut lo = pt(f64::INFINITY, f64::INFINITY);
    let mut hi = pt(f64::NEG_INFINITY, f64::NEG_INFINITY);
    for p in points {
        lo = pt(lo[0].min(p[0]), lo[1].min(p[1]));
        hi = pt(hi[0].max(p[0]), hi[1].max(p[1]));
    }
    (
        hi[0] - lo[0],
        hi[1] - lo[1],
        pt((lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0),
    )
}

fn contour_area(chain: &[Pt]) -> f64 {
    let n = chain.len();
    if n < 3 {
        return 0.0;
    }
    let mut acc = 0.0;
    for i in 0..n {
        let a = chain[i];
        let b = chain[(i + 1) % n];
        acc += a[0] * b[1] - b[0] * a[1];
    }
    (acc / 2.0).abs()
}

/// Flat erosion (min over the in-bounds kernel window, matching
/// OpenCV's morphology border semantics).
fn erode(mask: &[u8], w: usize, h: usize, k: usize) -> Vec<u8> {
    let r = (k / 2) as i64;
    let mut out = vec![0u8; w * h];
    for y in 0..h as i64 {
        for x in 0..w as i64 {
            let mut m = 255u8;
            for dy in -r..=r {
                for dx in -r..=r {
                    let (nx, ny) = (x + dx, y + dy);
                    if nx >= 0 && ny >= 0 && (nx as usize) < w && (ny as usize) < h {
                        m = m.min(mask[ny as usize * w + nx as usize]);
                    }
                }
            }
            out[y as usize * w + x as usize] = m;
        }
    }
    out
}

fn dilate(mask: &[u8], w: usize, h: usize, k: usize) -> Vec<u8> {
    let r = (k / 2) as i64;
    let mut out = vec![0u8; w * h];
    for y in 0..h as i64 {
        for x in 0..w as i64 {
            let mut m = 0u8;
            for dy in -r..=r {
                for dx in -r..=r {
                    let (nx, ny) = (x + dx, y + dy);
                    if nx >= 0 && ny >= 0 && (nx as usize) < w && (ny as usize) < h {
                        m = m.max(mask[ny as usize * w + nx as usize]);
                    }
                }
            }
            out[y as usize * w + x as usize] = m;
        }
    }
    out
}

/// Is the pixel centre inside the (vertex-truncated) quad, or on
/// its boundary — cv2.fillPoly semantics, approximated.
fn point_in_quad(px: f64, py: f64, quad: &[Pt; 4]) -> bool {
    // Boundary: within half a pixel of any edge segment.
    for i in 0..4 {
        let a = quad[i];
        let b = quad[(i + 1) % 4];
        let ab = sub(b, a);
        let len2 = dot(ab, ab);
        let t = if len2 > 0.0 {
            (dot(pt(px - a[0], py - a[1]), ab) / len2).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let closest = add(a, scale(ab, t));
        if norm(pt(px - closest[0], py - closest[1])) <= 0.5 {
            return true;
        }
    }
    // Even-odd ray casting.
    let mut inside = false;
    for i in 0..4 {
        let a = quad[i];
        let b = quad[(i + 1) % 4];
        if (a[1] > py) != (b[1] > py) {
            let x_int = a[0] + (py - a[1]) * (b[0] - a[0]) / (b[1] - a[1]);
            if px < x_int {
                inside = !inside;
            }
        }
    }
    inside
}

/// Port of `_object_extent`.
#[allow(clippy::too_many_arguments)]
fn object_extent(
    frame: &MeasureFrame,
    H: &H3,
    quad_img: &[Pt; 4],
    bbox: Option<(f64, f64, f64, f64)>,
    mask_obj: Option<&[u8]>,
    warnings: &mut Vec<String>,
) -> Result<(f64, f64, Pt), String> {
    let H_inv =
        mat3_inv(H).ok_or_else(|| "Degenerate homography at the object location.".to_string())?;
    if let Some(mask) = mask_obj {
        let mut points = Vec::new();
        for y in 0..frame.height {
            for x in 0..frame.width {
                if mask[y * frame.width + x] != 0 {
                    points.push(pt(x as f64, y as f64));
                }
            }
        }
        if points.len() < 10 {
            return Err("Supplied mask is empty.".to_string());
        }
        let projected: Vec<Pt> = points.iter().map(|p| project1(&H_inv, *p)).collect();
        return Ok(extent_from_points(&projected));
    }
    if let Some((x, y, bw, bh)) = bbox {
        let corners = [pt(x, y), pt(x + bw, y), pt(x + bw, y + bh), pt(x, y + bh)];
        warnings.push(
            "Extent from a caller-supplied image bbox: a bbox around a \
             rotated object overestimates; a mask is tighter."
                .to_string(),
        );
        let projected: Vec<Pt> = corners.iter().map(|p| project1(&H_inv, *p)).collect();
        return Ok(extent_from_points(&projected));
    }
    // Prototype fallback: colour segmentation. The object is the
    // largest saturated, non-card contour outside the card quad.
    let (w, h) = (frame.width, frame.height);
    let mut candidate = vec![0u8; w * h];
    for i in 0..w * h {
        let (_, s, v) = frame.hsv(i);
        if s > 70.0 && v > 40.0 {
            candidate[i] = 255;
        }
    }
    let quad_i: [Pt; 4] = [
        pt(quad_img[0][0].trunc(), quad_img[0][1].trunc()),
        pt(quad_img[1][0].trunc(), quad_img[1][1].trunc()),
        pt(quad_img[2][0].trunc(), quad_img[2][1].trunc()),
        pt(quad_img[3][0].trunc(), quad_img[3][1].trunc()),
    ];
    let (mut qlo_x, mut qlo_y) = (i64::MAX, i64::MAX);
    let (mut qhi_x, mut qhi_y) = (i64::MIN, i64::MIN);
    for q in quad_i {
        qlo_x = qlo_x.min(q[0] as i64);
        qlo_y = qlo_y.min(q[1] as i64);
        qhi_x = qhi_x.max(q[0] as i64);
        qhi_y = qhi_y.max(q[1] as i64);
    }
    for y in qlo_y.max(0)..=qhi_y.min(h as i64 - 1) {
        for x in qlo_x.max(0)..=qhi_x.min(w as i64 - 1) {
            if point_in_quad(x as f64, y as f64, &quad_i) {
                candidate[y as usize * w + x as usize] = 0;
            }
        }
    }
    let opened = dilate(&erode(&candidate, w, h, 5), w, h, 5);
    let closed = erode(&dilate(&opened, w, h, 9), w, h, 9);
    let contours = external_contours(&closed, w, h, false);
    if contours.is_empty() {
        return Err("Auto extent found no object (colour segmentation); \
             supply a bbox or mask."
            .to_string());
    }
    let largest = contours
        .iter()
        .max_by(|a, b| contour_area(a).total_cmp(&contour_area(b)))
        .unwrap();
    if contour_area(largest) < 0.0005 * (w * h) as f64 {
        return Err("Auto extent contour is implausibly small.".to_string());
    }
    // A region that reaches the frame edge runs out of the photo (sky,
    // floor, a wall): its extent is unknown and it is background, not
    // an object lying in the card's plane. IMG_1403's "object" was the
    // sky. Refuse rather than measure the part that happens to be in
    // frame, and do not fall back to a smaller region.
    let (last_x, last_y) = ((w - 1) as f64, (h - 1) as f64);
    if largest
        .iter()
        .any(|p| p[0] <= 0.0 || p[1] <= 0.0 || p[0] >= last_x || p[1] >= last_y)
    {
        return Err(
            "Auto extent reached the frame edge (the colour region runs \
             out of the photo: sky, floor or wall, not a whole object) — \
             supply a bbox or mask, or reshoot with the whole object in frame."
                .to_string(),
        );
    }
    warnings.push(
        "Extent by colour segmentation (prototype fallback): the object \
         must be colour-distinct from the surface and the card."
            .to_string(),
    );
    let projected: Vec<Pt> = largest.iter().map(|p| project1(&H_inv, *p)).collect();
    Ok(extent_from_points(&projected))
}

/// The card sheet's width in image pixels under H (port of
/// `_card_width_px`).
fn card_width_px(H: &H3, card: &CardProfile) -> f64 {
    let (cw, ch) = (card.card_width_mm, card.card_height_mm);
    let corners = [
        project1(H, pt(0.0, 0.0)),
        project1(H, pt(cw, 0.0)),
        project1(H, pt(cw, ch)),
        project1(H, pt(0.0, ch)),
    ];
    let top = norm(sub(corners[1], corners[0]));
    let bottom = norm(sub(corners[2], corners[3]));
    (top + bottom) / 2.0
}

// ---------------------------------------------------------------------------
// Public estimate

fn round_n(x: f64, digits: i32) -> f64 {
    let f = 10f64.powi(digits);
    (x * f).round_ties_even() / f
}

#[derive(Clone, Debug)]
pub struct Estimate {
    pub width_mm: f64,
    pub height_mm: f64,
    pub range_pct: f64,
    pub width_range_mm: [f64; 2],
    pub height_range_mm: [f64; 2],
    pub display: String,
    pub method: &'static str,
    pub card_id: String,
    pub ticks_matched: usize,
    pub card_width_px: i64,
    pub reprojection_rms_px: f64,
    pub warnings: Vec<String>,
}

impl Estimate {
    /// JSON in the Python dict's insertion order, indent 2.
    pub fn to_json_pretty(&self) -> String {
        let num = |x: f64| -> String { serde_json::to_string(&serde_json::json!(x)).unwrap() };
        let warnings: Vec<String> = self
            .warnings
            .iter()
            .map(|w| serde_json::to_string(w).unwrap())
            .collect();
        format!(
            "{{\n  \"width_mm\": {},\n  \"height_mm\": {},\n  \"range_pct\": {},\n  \
             \"width_range_mm\": [{}, {}],\n  \"height_range_mm\": [{}, {}],\n  \
             \"display\": {},\n  \"method\": {},\n  \"card_id\": {},\n  \
             \"ticks_matched\": {},\n  \"card_width_px\": {},\n  \
             \"reprojection_rms_px\": {},\n  \"warnings\": [{}]\n}}",
            num(self.width_mm),
            num(self.height_mm),
            num(self.range_pct),
            num(self.width_range_mm[0]),
            num(self.width_range_mm[1]),
            num(self.height_range_mm[0]),
            num(self.height_range_mm[1]),
            serde_json::to_string(&self.display).unwrap(),
            serde_json::to_string(self.method).unwrap(),
            serde_json::to_string(&self.card_id).unwrap(),
            self.ticks_matched,
            self.card_width_px,
            num(self.reprojection_rms_px),
            warnings
                .iter()
                .map(|w| format!("\n    {w}"))
                .collect::<Vec<_>>()
                .join(",")
                + if warnings.is_empty() { "" } else { "\n  " },
        )
    }
}

/// Port of Python `estimate` (frame already decoded; see the CLI
/// for source loading, including asset:// handles).
pub fn estimate(
    frame: &MeasureFrame,
    card: &CardProfile,
    card_id: &str,
    bbox: Option<(f64, f64, f64, f64)>,
    mask_obj: Option<&[u8]>,
) -> Result<Estimate, String> {
    let mut warnings: Vec<String> =
        vec!["Prototype: flat object in the same plane as the card only.".to_string()];
    let det = detect_card(frame, card)?;
    let n_ticks = det.tick_pairs.len();

    let card_px = card_width_px(&det.H, card);
    if let Some(envelope_min) = card_envelope_min_px(card_id) {
        if card_px < envelope_min {
            return Err(format!(
                "Card too small in frame for the {card_id} mini \
                 (~{card_px:.0} px across; this card is only validated \
                 for close-ups at ~{envelope_min:.0} px across or more, \
                 where its ranges demonstrably cover the true size) — \
                 Retake the shot or use a bigger card."
            ));
        }
    }
    if card_px < MIN_CARD_PX {
        return Err(format!(
            "Card too small in frame (~{card_px:.0} px across; the \
             fit stays quiet but the range stops covering the true \
             size below ~{MIN_CARD_PX:.0} px) — Retake the shot \
             or use a bigger card."
        ));
    }
    if det.strict_ticks <= STRICT_COLLAPSE_MAX_TICKS && det.strict_arms <= STRICT_COLLAPSE_MAX_ARMS
    {
        return Err(format!(
            "Card failed strict tick verification at its own fit ({} tick \
             cuts on {} arms under strict matching; the fit stood on loose \
             matches only) — Retake the shot closer, with the card flat, \
             sharp and fully in frame.",
            det.strict_ticks, det.strict_arms
        ));
    }
    let mut sys_frac = 0.0;
    if card_px < COMFORT_CARD_PX {
        let band_t = (COMFORT_CARD_PX - card_px) / (COMFORT_CARD_PX - MIN_CARD_PX);
        sys_frac = SMALL_CARD_SYS_MAX * band_t * band_t;
        warnings.push(format!(
            "Card is small in frame (~{card_px:.0} px across): the \
             range is widened for small-card systematic error — a \
             bigger card or a closer shot would tighten it."
        ));
    }
    if n_ticks < 24 {
        warnings.push(format!(
            "Only {n_ticks}/24 tick cuts matched; the range reflects \
             the fit residual, but a cleaner photo would tighten it."
        ));
    }

    let (width_mm, height_mm, centre_mm) =
        object_extent(frame, &det.H, &det.quad, bbox, mask_obj, &mut warnings)?;
    if width_mm <= 0.0 || height_mm <= 0.0 {
        return Err("Degenerate object extent.".to_string());
    }

    let centre_img = project1(&det.H, centre_mm);
    let jx = scale(
        sub(
            project1(&det.H, pt(centre_mm[0] + 1.0, centre_mm[1])),
            centre_img,
        ),
        1.0,
    );
    let jy = sub(
        project1(&det.H, pt(centre_mm[0], centre_mm[1] + 1.0)),
        centre_img,
    );
    let px_per_mm = (norm(jx) + norm(jy)) / 2.0;
    if px_per_mm <= 0.0 {
        return Err("Degenerate homography at the object location.".to_string());
    }
    let sigma_mm = det.rms / px_per_mm;
    let frame_vals = [
        card.frame["TL"],
        card.frame["TR"],
        card.frame["BR"],
        card.frame["BL"],
    ];
    let frame_centroid = pt(
        frame_vals.iter().map(|p| p[0]).sum::<f64>() / 4.0,
        frame_vals.iter().map(|p| p[1]).sum::<f64>() / 4.0,
    );
    let frame_diag = norm(sub(card.frame["BR"], card.frame["TL"]));
    let lever = norm(sub(centre_mm, frame_centroid)) / frame_diag;
    let half_range = |dim: f64| -> f64 {
        let u = sigma_mm * (1.0 + lever).powi(2);
        2.0 * u + 0.01 * dim + sys_frac * dim
    };
    let err_w = half_range(width_mm);
    let err_h = half_range(height_mm);
    let range_pct_raw = (err_w / width_mm).max(err_h / height_mm) * 100.0;
    let range_pct = (range_pct_raw * 2.0).ceil() / 2.0;
    let fmt_dim = |dim: f64, err: f64| -> String {
        let (lo, hi) = (dim - err, dim + err);
        if dim >= 100.0 {
            format!(
                "~{:.1} cm ({:.1}–{:.1} cm)",
                dim / 10.0,
                lo / 10.0,
                hi / 10.0
            )
        } else {
            format!("~{:.0} mm ({:.0}–{:.0} mm)", dim, lo, hi)
        }
    };
    Ok(Estimate {
        width_mm: round_n(width_mm, 1),
        height_mm: round_n(height_mm, 1),
        range_pct,
        width_range_mm: [round_n(width_mm - err_w, 1), round_n(width_mm + err_w, 1)],
        height_range_mm: [round_n(height_mm - err_h, 1), round_n(height_mm + err_h, 1)],
        display: format!(
            "{} wide × {} high",
            fmt_dim(width_mm, err_w),
            fmt_dim(height_mm, err_h)
        ),
        method: METHOD,
        card_id: card_id.to_string(),
        ticks_matched: n_ticks,
        card_width_px: card_px.round_ties_even() as i64,
        reprojection_rms_px: round_n(det.rms, 2),
        warnings,
    })
}
