//! Tailor-grade flat-lay measurements for top-wear (jackets, shirts,
//! hoodies) on top of the scale-card estimator.
//!
//! Input: the product mask (the garment's own pixels) and the card
//! plane of an accepted `measure::estimate_with_plane`. The mask's
//! outline is mapped into the card plane through the card homography
//! (stated assumption: the garment lies flat on the same surface as
//! the card), smoothed, resampled, and read in the garment's own frame
//! (collar up, hem down) for the landmarks tailors and resellers use:
//! high point of shoulder at the collar base, shoulder corners,
//! underarm points, cuff corners and the hem.
//!
//! Every figure carries its method, the landmarks it rests on, and a
//! half range built from the card fit (the estimator's own range
//! model), the mask edge accuracy in mm, the contour step, and the
//! landmark ambiguity (how far the figure moves across the convention
//! band or detector scale). A landmark the pose hides is reported as
//! not measurable with the reason; nothing is filled in by guess.
//!
//! No model calls: the diagram is drawn in code with an embedded
//! bitmap font.

use crate::image::RgbaImage;
use crate::measure::{contour_area, external_contours, Plane};
use crate::measure_font;
use serde_json::{json, Value};

type Pt = [f64; 2];

/// The stated assumption every garment block carries.
pub const PLANE_ASSUMPTION: &str = "The garment lies flat on the same surface as the card. \
     Its outline (the supplied mask) is mapped into the card plane through the card \
     homography; anything raised off that surface (a thick collar, a rolled cuff) is \
     measured as its outline seen from above.";

/// Contour resampling step (mm along the outline).
const DS: f64 = 1.0;
/// Gaussian smoothing of the mapped outline (mm, one sigma): removes the
/// pixel staircase, far below the scale of any landmark.
const SMOOTH_MM: f64 = 2.0;
/// Core = opening of the silhouette with a disk of this fraction of
/// its largest inscribed radius: keeps the torso, drops the sleeves.
const CORE_FRAC: f64 = 0.6;
/// The axis is fitted to the torso below this fraction of the way from
/// the sleeve roots to the hem: clear of the armholes, where a folded
/// or raised sleeve makes the outline one-sided.
const LOWER_TORSO_FRAC: f64 = 0.5;
/// Corner scales (mm, chord half-length for the turning angle).
const SHOULDER_SCALES: [f64; 4] = [20.0, 30.0, 45.0, 60.0];
const SHOULDER_MAIN: usize = 1;
const ARMPIT_SCALES: [f64; 3] = [15.0, 20.0, 30.0];
const ARMPIT_MAIN: usize = 1;
const HPS_SCALES: [f64; 3] = [10.0, 15.0, 25.0];
const HPS_MAIN: usize = 1;
const CUFF_SCALES: [f64; 3] = [15.0, 20.0, 30.0];
const CUFF_MAIN: usize = 1;
/// Minimum turning (degrees) for a corner to count as visible.
const SHOULDER_MIN_TURN: f64 = 12.0;
const ARMPIT_MIN_TURN: f64 = 25.0;
const HPS_MIN_TURN: f64 = 25.0;
const CUFF_MIN_TURN: f64 = 35.0;
/// Chest line offsets below the underarm points (mm); the reported
/// figure uses CHEST_MAIN, the band is the landmark ambiguity.
const CHEST_OFFSETS: [f64; 5] = [5.0, 10.0, 15.0, 20.0, 25.0];
const CHEST_MAIN: usize = 1;
/// Hem width offsets above the hem line (mm).
const HEM_OFFSETS: [f64; 5] = [5.0, 10.0, 15.0, 20.0, 25.0];
const HEM_MAIN: usize = 1;

#[derive(Clone, Debug)]
pub struct GarmentOptions {
    /// Edge accuracy of the supplied mask, in source pixels (one
    /// endpoint). A mask upscaled from a model's working resolution is
    /// only as sharp as one working pixel: source width / mask width.
    pub mask_edge_px: f64,
}

impl Default for GarmentOptions {
    fn default() -> Self {
        GarmentOptions { mask_edge_px: 2.0 }
    }
}

#[derive(Clone, Debug)]
pub struct Measurement {
    pub key: &'static str,
    pub label: &'static str,
    pub value_mm: Option<f64>,
    pub plus_minus_mm: f64,
    pub card_mm: f64,
    pub mask_mm: f64,
    pub landmark_mm: f64,
    pub step_mm: f64,
    pub method: String,
    pub reason: Option<String>,
    /// Garment-frame (u, v) polyline drawn on the diagram.
    pub line: Vec<Pt>,
    pub notes: Vec<String>,
}

impl Measurement {
    fn missing(key: &'static str, label: &'static str, reason: String) -> Measurement {
        Measurement {
            key,
            label,
            value_mm: None,
            plus_minus_mm: 0.0,
            card_mm: 0.0,
            mask_mm: 0.0,
            landmark_mm: 0.0,
            step_mm: 0.0,
            method: String::new(),
            reason: Some(reason),
            line: Vec::new(),
            notes: Vec::new(),
        }
    }

    pub fn measurable(&self) -> bool {
        self.value_mm.is_some()
    }

    pub fn value_cm(&self) -> Option<f64> {
        self.value_mm.map(|v| (v).round() / 10.0)
    }

    /// Half range in cm, rounded up to 0.1 cm.
    pub fn plus_minus_cm(&self) -> f64 {
        (self.plus_minus_mm - 1e-9).max(0.0).ceil() / 10.0
    }

    fn to_json(&self) -> Value {
        match self.value_mm {
            None => json!({
                "measurable": false,
                "value_cm": Value::Null,
                "reason": self.reason.clone().unwrap_or_default(),
            }),
            Some(v) => {
                let vc = self.value_cm().unwrap();
                let pm = self.plus_minus_cm();
                let r1 = |x: f64| (x * 10.0).round() / 10.0;
                json!({
                    "measurable": true,
                    "value_cm": vc,
                    "plus_minus_cm": pm,
                    "range_cm": [r1(vc - pm), r1(vc + pm)],
                    "value_mm": r1(v),
                    "method": self.method,
                    "uncertainty_mm": {
                        "card_fit": r1(self.card_mm),
                        "mask_edge": r1(self.mask_mm),
                        "landmark": r1(self.landmark_mm),
                        "contour_step": r1(self.step_mm),
                        "combined": "card_fit + sqrt(mask_edge^2 + landmark^2 + contour_step^2), a half range",
                    },
                    "notes": self.notes,
                })
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Landmark {
    pub name: String,
    pub uv: Pt,
    pub card_mm: Pt,
    pub image_px: Pt,
}

#[derive(Clone, Debug)]
pub struct Garment {
    pub measurements: Vec<Measurement>,
    pub landmarks: Vec<Landmark>,
    /// The smoothed outline in the garment frame (u right, v down,
    /// collar at small v), mm.
    pub outline_uv: Vec<Pt>,
    /// Garment down axis (collar -> hem) in card-plane degrees
    /// (0 = card +x, 90 = card +y).
    pub axis_deg: f64,
    /// Mirror symmetry of the torso core about the axis (0..1).
    pub symmetry: f64,
    pub sleeves_found: usize,
    pub mask_pixels: usize,
    pub mask_edge_px: f64,
    /// Card-plane mm per image pixel at the garment's centre.
    pub mm_per_px: f64,
    pub notes: Vec<String>,
}

impl Garment {
    pub fn measurement(&self, key: &str) -> Option<&Measurement> {
        self.measurements.iter().find(|m| m.key == key)
    }

    pub fn to_json(&self) -> Value {
        let mut ms = serde_json::Map::new();
        for m in &self.measurements {
            ms.insert(m.key.to_string(), m.to_json());
        }
        let mut lm = serde_json::Map::new();
        let r1 = |x: f64| (x * 10.0).round() / 10.0;
        for l in &self.landmarks {
            lm.insert(
                l.name.clone(),
                json!({
                    "card_mm": [r1(l.card_mm[0]), r1(l.card_mm[1])],
                    "image_px": [r1(l.image_px[0]), r1(l.image_px[1])],
                }),
            );
        }
        json!({
            "kind": "top-wear flat lay (jacket, shirt, hoodie)",
            "estimated": true,
            "units": "cm",
            "assumption": PLANE_ASSUMPTION,
            "sides": "left and right as drawn on the diagram (collar up), not the wearer's",
            "measurements": Value::Object(ms),
            "not_attempted": {
                "across_back": "The back is face down on a front-up flat lay; its shoulder-blade line is not visible.",
            },
            "landmarks": Value::Object(lm),
            "orientation": {
                "axis_deg_in_card_plane": r1(self.axis_deg),
                "torso_symmetry": (self.symmetry * 1000.0).round() / 1000.0,
                "sleeves_found": self.sleeves_found,
            },
            "mask": {
                "pixels": self.mask_pixels,
                "edge_accuracy_px": self.mask_edge_px,
                "mm_per_px_at_garment": (self.mm_per_px * 1000.0).round() / 1000.0,
            },
            "notes": self.notes,
        })
    }
}

// ---------------------------------------------------------------------------
// Small geometry helpers

fn sub(a: Pt, b: Pt) -> Pt {
    [a[0] - b[0], a[1] - b[1]]
}
fn add(a: Pt, b: Pt) -> Pt {
    [a[0] + b[0], a[1] + b[1]]
}
fn scale(a: Pt, s: f64) -> Pt {
    [a[0] * s, a[1] * s]
}
fn dot(a: Pt, b: Pt) -> f64 {
    a[0] * b[0] + a[1] * b[1]
}
fn cross(a: Pt, b: Pt) -> f64 {
    a[0] * b[1] - a[1] * b[0]
}
fn norm(a: Pt) -> f64 {
    dot(a, a).sqrt()
}
fn dist(a: Pt, b: Pt) -> f64 {
    norm(sub(a, b))
}
fn unit(a: Pt) -> Pt {
    let n = norm(a);
    if n > 0.0 {
        scale(a, 1.0 / n)
    } else {
        [1.0, 0.0]
    }
}
fn rot(a: Pt, deg: f64) -> Pt {
    let (s, c) = deg.to_radians().sin_cos();
    [c * a[0] - s * a[1], s * a[0] + c * a[1]]
}
fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        0.5 * (v[n / 2 - 1] + v[n / 2])
    }
}

/// Closed-polyline Gaussian smoothing (sigma in points).
fn smooth_closed(pts: &[Pt], sigma: f64) -> Vec<Pt> {
    let n = pts.len();
    if sigma < 0.5 || n < 8 {
        return pts.to_vec();
    }
    let r = (3.0 * sigma).ceil() as isize;
    let kernel: Vec<f64> = (-r..=r)
        .map(|k| (-(k * k) as f64 / (2.0 * sigma * sigma)).exp())
        .collect();
    let ksum: f64 = kernel.iter().sum();
    (0..n)
        .map(|i| {
            let mut acc = [0.0, 0.0];
            for (j, w) in kernel.iter().enumerate() {
                let k = (i as isize + j as isize - r).rem_euclid(n as isize) as usize;
                acc[0] += w * pts[k][0];
                acc[1] += w * pts[k][1];
            }
            [acc[0] / ksum, acc[1] / ksum]
        })
        .collect()
}

/// Resample a closed polyline at a uniform arc step.
fn resample_closed(pts: &[Pt], step: f64) -> Vec<Pt> {
    let n = pts.len();
    let mut cum = vec![0.0; n + 1];
    for i in 0..n {
        cum[i + 1] = cum[i] + dist(pts[i], pts[(i + 1) % n]);
    }
    let total = cum[n];
    let m = (total / step).floor().max(3.0) as usize;
    let mut out = Vec::with_capacity(m);
    let mut seg = 0usize;
    for k in 0..m {
        let s = k as f64 * total / m as f64;
        while seg + 1 < n && cum[seg + 1] < s {
            seg += 1;
        }
        let a = pts[seg];
        let b = pts[(seg + 1) % n];
        let len = cum[seg + 1] - cum[seg];
        let t = if len > 0.0 { (s - cum[seg]) / len } else { 0.0 };
        out.push(add(a, scale(sub(b, a), t)));
    }
    out
}

/// Euclidean distance (pixels) from every pixel to the nearest seed
/// pixel (Felzenszwalb-Huttenlocher, exact).
fn edt(seed: &[bool], w: usize, h: usize) -> Vec<f64> {
    const BIG: f64 = 1e20;
    let mut f2 = vec![0.0f64; w * h];
    for i in 0..w * h {
        f2[i] = if seed[i] { 0.0 } else { BIG };
    }
    let n = w.max(h);
    let mut f = vec![0.0; n];
    let mut d = vec![0.0; n];
    let mut v = vec![0usize; n];
    let mut z = vec![0.0; n + 1];
    let pass = |f: &[f64], len: usize, d: &mut [f64], v: &mut [usize], z: &mut [f64]| {
        let mut k = 0usize;
        v[0] = 0;
        z[0] = f64::NEG_INFINITY;
        z[1] = f64::INFINITY;
        for q in 1..len {
            let qf = q as f64;
            loop {
                let p = v[k];
                let pf = p as f64;
                let s = ((f[q] + qf * qf) - (f[p] + pf * pf)) / (2.0 * qf - 2.0 * pf);
                if s <= z[k] {
                    if k == 0 {
                        // z[0] is -inf; unreachable, kept for safety.
                        break;
                    }
                    k -= 1;
                    continue;
                }
                k += 1;
                v[k] = q;
                z[k] = s;
                z[k + 1] = f64::INFINITY;
                break;
            }
        }
        k = 0;
        for q in 0..len {
            let qf = q as f64;
            while z[k + 1] < qf {
                k += 1;
            }
            let p = v[k] as f64;
            d[q] = (qf - p) * (qf - p) + f[v[k]];
        }
    };
    for x in 0..w {
        for y in 0..h {
            f[y] = f2[y * w + x];
        }
        pass(&f, h, &mut d, &mut v, &mut z);
        for y in 0..h {
            f2[y * w + x] = d[y];
        }
    }
    for y in 0..h {
        f[..w].copy_from_slice(&f2[y * w..y * w + w]);
        pass(&f, w, &mut d, &mut v, &mut z);
        f2[y * w..y * w + w].copy_from_slice(&d[..w]);
    }
    f2.iter().map(|x| x.sqrt()).collect()
}

/// 8-connected components of `on`; labels start at 1.
fn components(on: &[bool], w: usize, h: usize) -> (Vec<u32>, u32) {
    let mut lab = vec![0u32; w * h];
    let mut cur = 0u32;
    let mut stack: Vec<usize> = Vec::new();
    for s in 0..w * h {
        if !on[s] || lab[s] != 0 {
            continue;
        }
        cur += 1;
        lab[s] = cur;
        stack.push(s);
        while let Some(i) = stack.pop() {
            let (x, y) = ((i % w) as i64, (i / w) as i64);
            for dy in -1..=1i64 {
                for dx in -1..=1i64 {
                    let (nx, ny) = (x + dx, y + dy);
                    if nx < 0 || ny < 0 || nx >= w as i64 || ny >= h as i64 {
                        continue;
                    }
                    let j = ny as usize * w + nx as usize;
                    if on[j] && lab[j] == 0 {
                        lab[j] = cur;
                        stack.push(j);
                    }
                }
            }
        }
    }
    (lab, cur)
}

/// The silhouette raster in card mm (axis aligned with the card).
struct Raster {
    x0: f64,
    y0: f64,
    r: f64,
    w: usize,
    h: usize,
    inside: Vec<bool>,
}

impl Raster {
    fn from_polygon(poly: &[Pt]) -> Raster {
        let (mut lo, mut hi) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
        for p in poly {
            lo = [lo[0].min(p[0]), lo[1].min(p[1])];
            hi = [hi[0].max(p[0]), hi[1].max(p[1])];
        }
        let side = (hi[0] - lo[0]).max(hi[1] - lo[1]);
        let r = (side / 900.0).max(2.0);
        let pad = 4.0;
        let x0 = lo[0] - pad * r;
        let y0 = lo[1] - pad * r;
        let w = ((hi[0] - lo[0]) / r + 2.0 * pad).ceil() as usize + 1;
        let h = ((hi[1] - lo[1]) / r + 2.0 * pad).ceil() as usize + 1;
        let mut inside = vec![false; w * h];
        let n = poly.len();
        let mut xs: Vec<f64> = Vec::new();
        for j in 0..h {
            let yc = y0 + (j as f64 + 0.5) * r;
            xs.clear();
            for k in 0..n {
                let a = poly[k];
                let b = poly[(k + 1) % n];
                if (a[1] > yc) != (b[1] > yc) {
                    xs.push(a[0] + (yc - a[1]) * (b[0] - a[0]) / (b[1] - a[1]));
                }
            }
            xs.sort_by(|a, b| a.total_cmp(b));
            for pair in xs.chunks(2) {
                if pair.len() < 2 {
                    break;
                }
                let i0 = ((pair[0] - x0) / r - 0.5).ceil().max(0.0) as usize;
                let i1 = ((pair[1] - x0) / r - 0.5).floor();
                if i1 < 0.0 {
                    continue;
                }
                let i1 = (i1 as usize).min(w - 1);
                for i in i0..=i1 {
                    inside[j * w + i] = true;
                }
            }
        }
        Raster {
            x0,
            y0,
            r,
            w,
            h,
            inside,
        }
    }

    fn centre_mm(&self, idx: usize) -> Pt {
        [
            self.x0 + ((idx % self.w) as f64 + 0.5) * self.r,
            self.y0 + ((idx / self.w) as f64 + 0.5) * self.r,
        ]
    }

    fn index_of(&self, p: Pt) -> Option<usize> {
        let i = ((p[0] - self.x0) / self.r).floor();
        let j = ((p[1] - self.y0) / self.r).floor();
        if i < 0.0 || j < 0.0 || i >= self.w as f64 || j >= self.h as f64 {
            return None;
        }
        Some(j as usize * self.w + i as usize)
    }
}

struct SleeveComp {
    root_mm: Pt,
    pixels: Vec<usize>,
}

/// Turning angle (degrees) at contour index i with chord half-length k
/// points; positive = convex for the outline's own orientation.
fn turn_at(uv: &[Pt], i: isize, k: usize, conv: f64) -> f64 {
    let n = uv.len() as isize;
    let g = |j: isize| uv[j.rem_euclid(n) as usize];
    let d1 = sub(g(i), g(i - k as isize));
    let d2 = sub(g(i + k as isize), g(i));
    conv * cross(d1, d2).atan2(dot(d1, d2)).to_degrees()
}

fn wrap(i: isize, n: usize) -> usize {
    i.rem_euclid(n as isize) as usize
}

/// Everything a side (one sleeve) yields.
#[derive(Clone, Debug, Default)]
struct Side {
    present: bool,
    /// Shoulder point index per SHOULDER_SCALES (None = no corner).
    sp: Vec<Option<usize>>,
    sp_turn: f64,
    armpit: Vec<Option<usize>>,
    hps: Vec<Option<usize>>,
    cuff: Vec<Option<usize>>,
    /// Inner cuff corner per CUFF_SCALES, reached from the underarm
    /// along the sleeve's underside.
    cuff_inner: Vec<Option<usize>>,
    /// Direction (+1 / -1 in contour index) from the shoulder into the
    /// sleeve along its top edge.
    dir_s: isize,
    /// Sleeve width near the cuff (mm), from the silhouette.
    cuff_width_mm: f64,
}

/// Analyse the garment outline. `mask` is `width * height` 8-bit
/// (product >= 128), in the same pixel grid as the photo the plane was
/// fitted on.
pub fn analyse(
    width: usize,
    height: usize,
    mask: &[u8],
    plane: &Plane,
    opts: &GarmentOptions,
) -> Result<Garment, String> {
    if mask.len() != width * height {
        return Err("Mask size does not match the photo.".to_string());
    }
    let bin: Vec<u8> = mask
        .iter()
        .map(|&m| if m >= 128 { 255 } else { 0 })
        .collect();
    let mask_pixels = bin.iter().filter(|&&m| m != 0).count();
    if mask_pixels < 1000 {
        return Err("Garment mask is empty or too small to measure.".to_string());
    }
    let contours = external_contours(&bin, width, height, false);
    let areas: Vec<f64> = contours.iter().map(|c| contour_area(c)).collect();
    let (best, best_area) =
        areas.iter().enumerate().fold(
            (0usize, -1.0f64),
            |acc, (i, &a)| if a > acc.1 { (i, a) } else { acc },
        );
    let mut notes: Vec<String> = Vec::new();
    let other_area: f64 = areas.iter().sum::<f64>() - best_area;
    if other_area > 0.005 * best_area {
        notes.push(format!(
            "The mask has {} separate regions; only the largest (the garment) is measured, \
             the rest ({:.1}% of its area) is ignored.",
            contours.len(),
            100.0 * other_area / best_area
        ));
    }
    let chain = &contours[best];
    if chain.len() < 50 {
        return Err("Garment outline is too short to measure.".to_string());
    }
    // Pixel-centre boundary chain -> card mm.
    let chain_mm: Vec<Pt> = chain.iter().map(|&p| plane.to_mm(p)).collect();
    let n0 = chain_mm.len();
    let mean_step = (0..n0)
        .map(|i| dist(chain_mm[i], chain_mm[(i + 1) % n0]))
        .sum::<f64>()
        / n0 as f64;
    let smoothed = smooth_closed(&chain_mm, SMOOTH_MM / mean_step.max(1e-6));
    let outline = resample_closed(&smoothed, DS);
    let n = outline.len();

    // Garment centre scale (for the mask edge term and the note).
    let mut cx = [0.0, 0.0];
    for p in &outline {
        cx = add(cx, *p);
    }
    let centre_mm = scale(cx, 1.0 / n as f64);
    let mm_per_px = 1.0 / plane.px_per_mm(centre_mm);

    // --- Silhouette raster, torso core, sleeves -------------------------
    let ras = Raster::from_polygon(&outline);
    let (rw, rh) = (ras.w, ras.h);
    let not_inside: Vec<bool> = ras.inside.iter().map(|&b| !b).collect();
    let dt_in = edt(&not_inside, rw, rh);
    let (mut rmax, mut c0) = (0.0f64, 0usize);
    for i in 0..rw * rh {
        if dt_in[i] > rmax {
            rmax = dt_in[i];
            c0 = i;
        }
    }
    let rho = CORE_FRAC * rmax;
    let eroded: Vec<bool> = dt_in.iter().map(|&d| d > rho).collect();
    let dt_e = edt(&eroded, rw, rh);
    let core_raw: Vec<bool> = (0..rw * rh)
        .map(|i| ras.inside[i] && dt_e[i] <= rho)
        .collect();
    let (core_lab, _) = components(&core_raw, rw, rh);
    let core_id = core_lab[c0];
    let core: Vec<bool> = core_lab.iter().map(|&l| l == core_id && l != 0).collect();
    let dt_core = edt(&core, rw, rh);
    let residual: Vec<bool> = (0..rw * rh)
        .map(|i| ras.inside[i] && dt_core[i] > 2.0)
        .collect();
    let (res_lab, n_res) = components(&residual, rw, rh);
    let r = ras.r;
    let rmax_mm = rmax * r;
    let rho_mm = rho * r;
    let s_area = ras.inside.iter().filter(|&&b| b).count() as f64;
    let mut comp_pixels: Vec<Vec<usize>> = vec![Vec::new(); n_res as usize + 1];
    for i in 0..rw * rh {
        if res_lab[i] != 0 {
            comp_pixels[res_lab[i] as usize].push(i);
        }
    }
    let mut sleeves: Vec<SleeveComp> = Vec::new();
    for px in comp_pixels.iter().skip(1) {
        if (px.len() as f64) < 0.02 * s_area {
            continue;
        }
        let extend_mm = px.iter().map(|&i| dt_core[i]).fold(0.0, f64::max) * r;
        let roots: Vec<Pt> = px
            .iter()
            .filter(|&&i| dt_core[i] <= 4.0)
            .map(|&i| ras.centre_mm(i))
            .collect();
        if roots.is_empty() {
            continue;
        }
        let mut rc = [0.0, 0.0];
        let (mut lo, mut hi) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
        for p in &roots {
            rc = add(rc, *p);
            lo = [lo[0].min(p[0]), lo[1].min(p[1])];
            hi = [hi[0].max(p[0]), hi[1].max(p[1])];
        }
        let root_mm = scale(rc, 1.0 / roots.len() as f64);
        let root_w = dist(lo, hi);
        if extend_mm >= 0.4 * rmax_mm && extend_mm >= 0.4 * root_w {
            sleeves.push(SleeveComp {
                root_mm,
                pixels: px.clone(),
            });
        }
    }
    if sleeves.len() > 2 {
        // Sleeves are the farthest-apart pair (a hood or collar sits between).
        let mut best = (0usize, 1usize, -1.0f64);
        for a in 0..sleeves.len() {
            for b in a + 1..sleeves.len() {
                let d = dist(sleeves[a].root_mm, sleeves[b].root_mm);
                if d > best.2 {
                    best = (a, b, d);
                }
            }
        }
        let keep = [best.0, best.1];
        let mut k = 0;
        sleeves.retain(|_| {
            let r = keep.contains(&k);
            k += 1;
            r
        });
    }

    // --- Garment axis ----------------------------------------------------
    let core_px: Vec<usize> = (0..rw * rh).filter(|&i| core[i]).collect();
    let mut cc = [0.0, 0.0];
    for &i in &core_px {
        cc = add(cc, ras.centre_mm(i));
    }
    let cc = scale(cc, 1.0 / core_px.len().max(1) as f64);
    let (mut sxx, mut syy, mut sxy) = (0.0, 0.0, 0.0);
    for &i in &core_px {
        let d = sub(ras.centre_mm(i), cc);
        sxx += d[0] * d[0];
        syy += d[1] * d[1];
        sxy += d[0] * d[1];
    }
    let theta = 0.5 * (2.0 * sxy).atan2(sxx - syy);
    let e1 = [theta.cos(), theta.sin()];
    let e2 = [-theta.sin(), theta.cos()];
    let (down0, ref_pt) = match sleeves.len() {
        2 => {
            let across = unit(sub(sleeves[1].root_mm, sleeves[0].root_mm));
            (
                [-across[1], across[0]],
                scale(add(sleeves[0].root_mm, sleeves[1].root_mm), 0.5),
            )
        }
        1 => {
            let to_root = sub(sleeves[0].root_mm, cc);
            let across = if dot(to_root, e1).abs() >= dot(to_root, e2).abs() {
                e1
            } else {
                e2
            };
            ([-across[1], across[0]], sleeves[0].root_mm)
        }
        _ => (e1, cc),
    };
    // The hem is on the side where the torso reaches farther from the
    // sleeve roots.
    let (mut tmin, mut tmax) = (f64::INFINITY, f64::NEG_INFINITY);
    for &i in &core_px {
        let t = dot(sub(ras.centre_mm(i), ref_pt), down0);
        tmin = tmin.min(t);
        tmax = tmax.max(t);
    }
    let (down0, t_hem) = if tmax >= -tmin {
        (down0, tmax)
    } else {
        (scale(down0, -1.0), -tmin)
    };
    // Refine by the mirror symmetry of the lower torso (below the
    // sleeve roots, where a folded or raised sleeve does not reach),
    // about an axis through that region's centroid. The region is cut
    // across the current axis, so the cut is redone on the refined axis
    // (up to eight rounds, until it stops moving).
    let mut a_cur = down0;
    let mut cc = [0.0, 0.0];
    let mut symmetry = 0.0;
    for _round in 0..8 {
        let mut lower: Vec<usize> = core_px
            .iter()
            .copied()
            .filter(|&i| dot(sub(ras.centre_mm(i), ref_pt), a_cur) >= LOWER_TORSO_FRAC * t_hem)
            .collect();
        if lower.len() < 50 {
            lower = core_px.clone();
        }
        let mut cl = [0.0, 0.0];
        for &i in &lower {
            cl = add(cl, ras.centre_mm(i));
        }
        cc = scale(cl, 1.0 / lower.len() as f64);
        let step = (lower.len() / 60_000).max(1);
        let sym_score = |a: Pt| -> f64 {
            let (mut hit, mut tot) = (0usize, 0usize);
            for &i in lower.iter().step_by(step) {
                let p = sub(ras.centre_mm(i), cc);
                let q = add(cc, sub(scale(a, 2.0 * dot(p, a)), p));
                tot += 1;
                // Scored against the whole core: the region's top cut is
                // across the current axis, and an oblique cut must not
                // count as asymmetry.
                if let Some(j) = ras.index_of(q) {
                    if core[j] {
                        hit += 1;
                    }
                }
            }
            hit as f64 / tot.max(1) as f64
        };
        // Coarse (1 degree over +-40) then fine (0.1 degree over +-1.5).
        let search = |centre: Pt, half: f64, stepd: f64| -> (Pt, f64, f64) {
            let mut best = (centre, sym_score(centre), 0.0f64);
            let steps = (half / stepd).round() as i64;
            for k in -steps..=steps {
                let d = k as f64 * stepd;
                let a = rot(centre, d);
                let sc = sym_score(a);
                if sc > best.1 + 1e-9 || (sc >= best.1 - 1e-9 && d.abs() < best.2.abs()) {
                    best = (a, sc, d);
                }
            }
            best
        };
        let coarse = search(a_cur, 40.0, 1.0);
        let fine = search(coarse.0, 1.5, 0.1);
        let moved = coarse.2 + fine.2;
        a_cur = fine.0;
        symmetry = fine.1;
        if moved.abs() < 0.05 {
            break;
        }
    }
    let a_v = a_cur;
    let a_u = [a_v[1], -a_v[0]];
    let to_uv = |p: Pt| -> Pt {
        let q = sub(p, cc);
        [dot(q, a_u), dot(q, a_v)]
    };
    let to_mm = |uv: Pt| -> Pt { add(cc, add(scale(a_u, uv[0]), scale(a_v, uv[1]))) };
    let uv: Vec<Pt> = outline.iter().map(|&p| to_uv(p)).collect();
    let axis_deg = a_v[1].atan2(a_v[0]).to_degrees();
    // Sleeve labels per raster pixel and per outline point.
    let mut sleeve_of = vec![0u8; rw * rh];
    let mut side_of_sleeve: Vec<i32> = Vec::new(); // -1 left, +1 right
    for (k, s) in sleeves.iter().enumerate() {
        for &i in &s.pixels {
            sleeve_of[i] = (k + 1) as u8;
        }
        side_of_sleeve.push(if to_uv(s.root_mm)[0] < 0.0 { -1 } else { 1 });
    }
    if sleeves.len() == 2 && side_of_sleeve[0] == side_of_sleeve[1] {
        notes.push(
            "Both sleeve-like regions lie on the same side of the body axis; the second \
             one is not treated as a sleeve."
                .to_string(),
        );
        side_of_sleeve.pop();
        for v in sleeve_of.iter_mut() {
            if *v == 2 {
                *v = 0;
            }
        }
    }
    let n_sleeves = side_of_sleeve.len();
    let labels: Vec<u8> = outline
        .iter()
        .map(|&p| {
            let Some(c) = ras.index_of(p) else { return 0 };
            if ras.inside[c] {
                return sleeve_of[c];
            }
            let (ci, cj) = ((c % rw) as i64, (c / rw) as i64);
            for dj in -1..=1i64 {
                for di in -1..=1i64 {
                    let (x, y) = (ci + di, cj + dj);
                    if x >= 0 && y >= 0 && (x as usize) < rw && (y as usize) < rh {
                        let j = y as usize * rw + x as usize;
                        if ras.inside[j] {
                            return sleeve_of[j];
                        }
                    }
                }
            }
            0
        })
        .collect();

    let signed_area: f64 = (0..n).map(|i| cross(uv[i], uv[(i + 1) % n])).sum::<f64>() / 2.0;
    let conv = if signed_area >= 0.0 { 1.0 } else { -1.0 };

    // --- Per-side landmarks ----------------------------------------------
    let mut sides: [Side; 2] = [Side::default(), Side::default()];
    for (k, &side_sign) in side_of_sleeve.iter().enumerate() {
        let lab = (k + 1) as u8;
        let si = if side_sign < 0 { 0 } else { 1 };
        let mut side = Side {
            present: true,
            ..Side::default()
        };
        // Longest circular run of this sleeve's label (short gaps merged).
        let on: Vec<bool> = labels.iter().map(|&l| l == lab).collect();
        let Some((run_a, run_len)) = longest_run(&on, 30) else {
            side.present = false;
            sides[si] = side;
            continue;
        };
        let ja = run_a;
        let jb = wrap(run_a as isize + run_len as isize - 1, n);
        let (j_up, j_dn, dir_s) = if uv[ja][1] <= uv[jb][1] {
            (ja, jb, 1isize)
        } else {
            (jb, ja, -1isize)
        };
        side.dir_s = dir_s;
        let at = |base: usize, t: isize, dir: isize| wrap(base as isize + t * dir, n);
        let su = |i: usize| uv[i][0] * side_sign as f64;
        let k_of = |s: f64| ((s / DS).round() as usize).max(2);

        // Shoulder corner: most convex point near the upper junction,
        // on the body's top edge or the sleeve's top edge.
        let tw = ((rho_mm + 50.0) / DS) as isize;
        let ts = tw.min(run_len as isize / 3);
        let u_up = su(j_up);
        for &sc in SHOULDER_SCALES.iter() {
            let kk = k_of(sc);
            let mut best: Option<(usize, f64)> = None;
            for t in -tw..=ts {
                let i = at(j_up, t, dir_s);
                if labels[i] != 0 && labels[i] != lab {
                    continue;
                }
                if su(i) < u_up - 40.0 {
                    continue;
                }
                let tv = turn_at(&uv, i as isize, kk, conv);
                if best.is_none_or(|b| tv > b.1) {
                    best = Some((i, tv));
                }
            }
            match best {
                Some((i, tv)) if tv >= SHOULDER_MIN_TURN => {
                    side.sp.push(Some(i));
                    if side.sp.len() == SHOULDER_MAIN + 1 {
                        side.sp_turn = tv;
                    }
                }
                Some((_, tv)) => {
                    side.sp.push(None);
                    if side.sp.len() == SHOULDER_MAIN + 1 {
                        side.sp_turn = tv;
                    }
                }
                None => side.sp.push(None),
            }
        }
        let sp_main = side.sp[SHOULDER_MAIN];

        // Underarm point: most concave point near the lower junction.
        let ta = ((rho_mm + 60.0) / DS) as isize;
        let v_top = sp_main.map(|i| uv[i][1]).unwrap_or(uv[j_up][1]);
        for &sc in ARMPIT_SCALES.iter() {
            let kk = k_of(sc);
            let mut best: Option<(usize, f64)> = None;
            for t in -ta..=ta {
                let i = wrap(j_dn as isize + t, n);
                if labels[i] != 0 && labels[i] != lab {
                    continue;
                }
                if uv[i][1] <= v_top + 20.0 || su(i) <= 0.0 {
                    continue;
                }
                let tv = -turn_at(&uv, i as isize, kk, conv);
                if best.is_none_or(|b| tv > b.1) {
                    best = Some((i, tv));
                }
            }
            side.armpit.push(match best {
                Some((i, tv)) if tv >= ARMPIT_MIN_TURN => Some(i),
                _ => None,
            });
        }

        // High point of shoulder: walking from the shoulder corner toward
        // the collar, the first strong corner (collar base or neckline).
        let start = sp_main.unwrap_or(j_up);
        for &sc in HPS_SCALES.iter() {
            let kk = k_of(sc);
            let t0 = (30.0 / DS) as isize;
            let t1 = (450.0 / DS) as isize;
            let mut found: Option<usize> = None;
            let mut peak: Option<(usize, f64)> = None;
            for t in t0..=t1 {
                let i = at(start, t, -dir_s);
                if su(i) <= 0.0 {
                    break;
                }
                let tv = turn_at(&uv, i as isize, kk, conv).abs();
                match peak {
                    None if tv >= HPS_MIN_TURN => peak = Some((i, tv)),
                    Some((_, pv)) if tv > pv => peak = Some((i, tv)),
                    Some(_) => {
                        found = peak.map(|p| p.0);
                        break;
                    }
                    None => {}
                }
            }
            if found.is_none() {
                found = peak.map(|p| p.0);
            }
            side.hps.push(found);
        }

        // Cuff: farthest sleeve point from the shoulder, then the first
        // strong convex corner reached along the top edge.
        let mut far = (0isize, -1.0f64);
        let base = start;
        let sp_uv = uv[base];
        let span = run_len as isize + tw + (60.0 / DS) as isize;
        for t in 0..span {
            let i = at(base, t, dir_s);
            if labels[i] != lab {
                continue;
            }
            let d = dist(uv[i], sp_uv);
            if d > far.1 {
                far = (t, d);
            }
        }
        for &sc in CUFF_SCALES.iter() {
            let kk = k_of(sc);
            let lo = (far.0 - (250.0 / DS) as isize).max(1);
            let hi = far.0 + (30.0 / DS) as isize;
            let mut found: Option<usize> = None;
            let mut prev = f64::NEG_INFINITY;
            let mut cur_peak: Option<(isize, f64)> = None;
            for t in lo..=hi {
                let i = at(base, t, dir_s);
                let tv = turn_at(&uv, i as isize, kk, conv);
                if tv >= CUFF_MIN_TURN {
                    if cur_peak.is_none_or(|p| tv > p.1) {
                        cur_peak = Some((t, tv));
                    }
                } else if let Some(p) = cur_peak {
                    if prev < CUFF_MIN_TURN || tv < CUFF_MIN_TURN {
                        found = Some(at(base, p.0, dir_s));
                        break;
                    }
                }
                prev = tv;
            }
            if found.is_none() {
                found = cur_peak.map(|p| at(base, p.0, dir_s));
            }
            side.cuff.push(found);
        }
        // Inner cuff corner: from the underarm along the underside
        // (into the sleeve, against dir_s), the first strong convex
        // corner before the far end.
        if let Some(ai) = side.armpit[ARMPIT_MAIN] {
            let a_uv = uv[ai];
            let mut far_u = (0isize, -1.0f64);
            for t in 0..span {
                let i = at(ai, t, -dir_s);
                if labels[i] != lab && labels[i] != 0 {
                    continue;
                }
                let d = dist(uv[i], a_uv);
                if d > far_u.1 {
                    far_u = (t, d);
                }
            }
            for &sc in CUFF_SCALES.iter() {
                side.cuff_inner
                    .push(first_corner(&uv, conv, ai, -dir_s, far_u.0, k_of(sc)));
            }
        } else {
            side.cuff_inner = vec![None; CUFF_SCALES.len()];
        }
        // Sleeve width near the cuff, from the silhouette.
        if let Some(ci) = side.cuff[CUFF_MAIN] {
            let cm = outline[ci];
            let mut m = 0.0f64;
            for (i, &l) in sleeve_of.iter().enumerate() {
                if l == lab && dist(ras.centre_mm(i), cm) <= 150.0 {
                    m = m.max(dt_in[i]);
                }
            }
            side.cuff_width_mm = 2.0 * m * r;
        }
        sides[si] = side;
    }

    // --- Measurements ------------------------------------------------------
    let edge_px = opts.mask_edge_px;
    let mask_term = |pts: &[Pt]| -> f64 {
        pts.iter()
            .map(|&p| {
                let e = edge_px / plane.px_per_mm(to_mm(p));
                e * e
            })
            .sum::<f64>()
            .sqrt()
    };
    let step_term = |n_end: usize| -> f64 { (DS / 2.0) * (n_end as f64).sqrt() };
    let finish = |mut m: Measurement, len: f64, a: Pt, b: Pt, landmark: f64| -> Measurement {
        let mid = to_mm(scale(add(a, b), 0.5));
        m.value_mm = Some(len);
        m.card_mm = plane.card_half_range_mm(len, mid);
        m.mask_mm = mask_term(&[a, b]);
        m.step_mm = step_term(2);
        m.landmark_mm = landmark;
        m.plus_minus_mm =
            m.card_mm + (m.mask_mm.powi(2) + m.step_mm.powi(2) + landmark.powi(2)).sqrt();
        m
    };
    let blank = |key: &'static str, label: &'static str, method: String| Measurement {
        key,
        label,
        value_mm: None,
        plus_minus_mm: 0.0,
        card_mm: 0.0,
        mask_mm: 0.0,
        landmark_mm: 0.0,
        step_mm: 0.0,
        method,
        reason: None,
        line: Vec::new(),
        notes: Vec::new(),
    };
    let side_name = ["left", "right"];
    let no_sleeve_reason = |s: usize| -> String {
        format!(
            "No sleeve leaves the outline on the {} side (folded over or under the body, \
             or tucked): its shoulder seam, underarm and cuff are hidden in this photo.",
            side_name[s]
        )
    };
    let mut landmarks: Vec<Landmark> = Vec::new();
    let mut add_lm = |name: String, p: Pt| {
        let mm = to_mm(p);
        landmarks.push(Landmark {
            name,
            uv: p,
            card_mm: mm,
            image_px: plane.to_px(mm),
        });
    };

    // Crossings of the horizontal garment line v with the outline:
    // the body's edges either side of the axis.
    let span_at = |v: f64| -> Option<(f64, u8, f64, u8)> {
        let mut xs: Vec<(f64, u8)> = Vec::new();
        for i in 0..n {
            let a = uv[i];
            let b = uv[(i + 1) % n];
            if (a[1] > v) != (b[1] > v) {
                let t = (v - a[1]) / (b[1] - a[1]);
                let lab = if t < 0.5 {
                    labels[i]
                } else {
                    labels[(i + 1) % n]
                };
                xs.push((a[0] + t * (b[0] - a[0]), lab));
            }
        }
        let left: Vec<&(f64, u8)> = xs.iter().filter(|x| x.0 < 0.0).collect();
        if left.len() % 2 == 0 {
            return None; // the axis point is outside the outline
        }
        let l = left.iter().max_by(|a, b| a.0.total_cmp(&b.0))?;
        let rgt = xs
            .iter()
            .filter(|x| x.0 >= 0.0)
            .min_by(|a, b| a.0.total_cmp(&b.0))?;
        Some((l.0, l.1, rgt.0, rgt.1))
    };
    let v_hem_at = |u: f64| -> Option<f64> {
        let mut best: Option<f64> = None;
        for i in 0..n {
            if labels[i] == 0 && (uv[i][0] - u).abs() <= 10.0 {
                best = Some(best.map_or(uv[i][1], |b: f64| b.max(uv[i][1])));
            }
        }
        best
    };

    for s in 0..2 {
        let sd = &sides[s];
        if !sd.present {
            continue;
        }
        if let Some(i) = sd.sp[SHOULDER_MAIN] {
            add_lm(format!("shoulder_point_{}", side_name[s]), uv[i]);
        }
        if let Some(i) = sd.armpit[ARMPIT_MAIN] {
            add_lm(format!("underarm_{}", side_name[s]), uv[i]);
        }
        if let Some(i) = sd.hps[HPS_MAIN] {
            add_lm(format!("high_point_shoulder_{}", side_name[s]), uv[i]);
        }
        if let Some(i) = sd.cuff[CUFF_MAIN] {
            add_lm(format!("cuff_corner_{}", side_name[s]), uv[i]);
        }
    }

    let mut measurements: Vec<Measurement> = Vec::new();
    let no_orientation = n_sleeves == 0;
    let orient_reason = "No sleeve leaves the body outline, so the collar-to-hem direction \
                         cannot be read from the outline; nothing is measured from a guessed \
                         orientation."
        .to_string();

    // Body length: high point of shoulder straight down to the hem.
    {
        let key = "body_length";
        let label = "Body length";
        if no_orientation {
            measurements.push(Measurement::missing(key, label, orient_reason.clone()));
        } else {
            let mut per_side: Vec<(f64, Pt, Pt, f64)> = Vec::new();
            for s in 0..2 {
                let sd = &sides[s];
                if !sd.present {
                    continue;
                }
                let Some(hi) = sd.hps[HPS_MAIN] else { continue };
                let h = uv[hi];
                let Some(vh) = v_hem_at(h[0]) else { continue };
                let len = vh - h[1];
                let mut dev = 0.0f64;
                for alt in sd.hps.iter().flatten() {
                    if let Some(vh2) = v_hem_at(uv[*alt][0]) {
                        dev = dev.max(((vh2 - uv[*alt][1]) - len).abs());
                    }
                }
                per_side.push((len, h, [h[0], vh], dev));
            }
            if per_side.is_empty() {
                let reason = if sides.iter().any(|s| s.present) {
                    "The collar base (high point of shoulder) has no visible corner in the \
                     outline: the collar or hood is flipped, rolled, or merges into the shoulder \
                     line."
                        .to_string()
                } else {
                    no_sleeve_reason(0)
                };
                measurements.push(Measurement::missing(key, label, reason));
            } else {
                let mean = per_side.iter().map(|p| p.0).sum::<f64>() / per_side.len() as f64;
                let mut lmk = per_side.iter().map(|p| p.3).fold(0.0, f64::max);
                if per_side.len() == 2 {
                    lmk = lmk.max((per_side[0].0 - per_side[1].0).abs() / 2.0);
                }
                let mut m = blank(
                    key,
                    label,
                    "High point of shoulder (where the collar or neckline meets the shoulder \
                     line in the outline) straight down to the hem, parallel to the garment's \
                     centre axis; mean of both sides."
                        .to_string(),
                );
                let a = per_side[0].1;
                let b = per_side[0].2;
                m.line = vec![a, b];
                if per_side.len() == 2 {
                    m.line.push(per_side[1].1);
                    m.line.push(per_side[1].2);
                    m.notes.push(format!(
                        "Left {:.1} cm, right {:.1} cm.",
                        per_side[0].0 / 10.0,
                        per_side[1].0 / 10.0
                    ));
                } else {
                    m.notes.push(
                        "One side only: the other side's collar base is not visible.".to_string(),
                    );
                }
                m.notes.push(
                    "The shoulder seam at the neck sits under the collar; the outline gives the \
                     point where the collar meets the shoulder line."
                        .to_string(),
                );
                measurements.push(finish(m, mean, a, b, lmk));
            }
        }
    }

    // Chest, pit to pit.
    {
        let key = "chest_width";
        let label = "Chest (pit to pit)";
        let missing_side = (0..2).find(|&s| !sides[s].present);
        if no_orientation {
            measurements.push(Measurement::missing(key, label, orient_reason.clone()));
        } else if let Some(s) = missing_side {
            measurements.push(Measurement::missing(key, label, no_sleeve_reason(s)));
        } else if let (Some(al), Some(ar)) =
            (sides[0].armpit[ARMPIT_MAIN], sides[1].armpit[ARMPIT_MAIN])
        {
            let (pl, pr) = (uv[al], uv[ar]);
            // Below the lower of the two underarm points, so an uneven
            // lay does not put the line through a sleeve.
            let v_arm = pl[1].max(pr[1]);
            let check = |v: f64| -> Result<(f64, f64, f64), String> {
                let (ul, ll, ur, lr) = span_at(v)
                    .ok_or_else(|| "The chest line does not cross the body cleanly.".to_string())?;
                if ll != 0 || lr != 0 || (ul - pl[0]).abs() > 60.0 || (ur - pr[0]).abs() > 60.0 {
                    return Err(
                        "A sleeve lies against the body side below the underarm, so the \
                                side seam is not visible at the chest line."
                            .to_string(),
                    );
                }
                Ok((ur - ul, ul, ur))
            };
            match check(v_arm + CHEST_OFFSETS[CHEST_MAIN]) {
                Err(reason) => measurements.push(Measurement::missing(key, label, reason)),
                Ok((w0, ul, ur)) => {
                    let mut dev = 0.0f64;
                    for &o in CHEST_OFFSETS.iter() {
                        if let Ok((w, _, _)) = check(v_arm + o) {
                            dev = dev.max((w - w0).abs());
                        }
                    }
                    for k in 0..ARMPIT_SCALES.len() {
                        if let (Some(a), Some(b)) = (sides[0].armpit[k], sides[1].armpit[k]) {
                            let va = uv[a][1].max(uv[b][1]);
                            if let Ok((w, _, _)) = check(va + CHEST_OFFSETS[CHEST_MAIN]) {
                                dev = dev.max((w - w0).abs());
                            }
                        }
                    }
                    // The direct underarm-to-underarm distance is the other
                    // common reading of "pit to pit".
                    dev = dev.max((dist(pl, pr) - w0).abs());
                    let vc = v_arm + CHEST_OFFSETS[CHEST_MAIN];
                    let mut m = blank(
                        key,
                        label,
                        format!(
                            "Straight across the body {:.0} cm below the underarm points (the \
                             concave corners where each sleeve meets the side), side edge to \
                             side edge.",
                            CHEST_OFFSETS[CHEST_MAIN] / 10.0
                        ),
                    );
                    m.line = vec![[ul, vc], [ur, vc]];
                    if (pl[1] - pr[1]).abs() > 20.0 {
                        m.notes.push(format!(
                            "The underarm points sit {:.1} cm apart along the body (uneven \
                             lay); the line runs below the lower one.",
                            (pl[1] - pr[1]).abs() / 10.0
                        ));
                    }
                    m.notes.push(format!(
                        "Landmark term: the spread across {:.1} to {:.1} cm below the underarm, \
                         across corner scales, and against the direct underarm-to-underarm \
                         distance.",
                        CHEST_OFFSETS[0] / 10.0,
                        CHEST_OFFSETS[CHEST_OFFSETS.len() - 1] / 10.0
                    ));
                    measurements.push(finish(m, w0, [ul, vc], [ur, vc], dev));
                }
            }
        } else {
            measurements.push(Measurement::missing(
                key,
                label,
                "An underarm point has no visible corner in the outline (the sleeve hangs \
                 against the body or the armhole is folded)."
                    .to_string(),
            ));
        }
    }

    // Shoulder width, shoulder corner to shoulder corner.
    {
        let key = "shoulder_width";
        let label = "Shoulder width";
        let missing_side = (0..2).find(|&s| !sides[s].present);
        if no_orientation {
            measurements.push(Measurement::missing(key, label, orient_reason.clone()));
        } else if let Some(s) = missing_side {
            measurements.push(Measurement::missing(key, label, no_sleeve_reason(s)));
        } else if let (Some(a), Some(b)) = (sides[0].sp[SHOULDER_MAIN], sides[1].sp[SHOULDER_MAIN])
        {
            let w0 = dist(uv[a], uv[b]);
            let mut dev = 0.0f64;
            for k in 0..SHOULDER_SCALES.len() {
                if let (Some(x), Some(y)) = (sides[0].sp[k], sides[1].sp[k]) {
                    dev = dev.max((dist(uv[x], uv[y]) - w0).abs());
                }
            }
            let mut m = blank(
                key,
                label,
                "Straight line between the two shoulder corners of the outline (the convex \
                 corner where the shoulder line turns into the sleeve's top edge)."
                    .to_string(),
            );
            m.line = vec![uv[a], uv[b]];
            m.notes.push(format!(
                "Corner turn {:.0} and {:.0} degrees; landmark term is the spread across corner \
                 scales {:.0} to {:.0} cm.",
                sides[0].sp_turn,
                sides[1].sp_turn,
                SHOULDER_SCALES[0] / 10.0,
                SHOULDER_SCALES[SHOULDER_SCALES.len() - 1] / 10.0
            ));
            measurements.push(finish(m, w0, uv[a], uv[b], dev));
        } else {
            let weak: Vec<String> = (0..2)
                .filter(|&s| sides[s].sp[SHOULDER_MAIN].is_none())
                .map(|s| format!("{} {:.0} degrees", side_name[s], sides[s].sp_turn))
                .collect();
            measurements.push(Measurement::missing(
                key,
                label,
                format!(
                    "The shoulder seam point is not a corner of the outline (strongest turn: \
                     {}; at least {:.0} needed): the shoulder line runs into the sleeve without \
                     a corner, as on dropped or raglan shoulders. The seam may be visible in the \
                     photo, but the outline alone cannot place it.",
                    weak.join(", "),
                    SHOULDER_MIN_TURN
                ),
            ));
        }
    }

    // Sleeves, shoulder corner to cuff along the top edge.
    let mut sleeve_ms: Vec<Measurement> = Vec::new();
    for s in 0..2 {
        let key: &'static str = if s == 0 {
            "sleeve_length_left"
        } else {
            "sleeve_length_right"
        };
        let label: &'static str = if s == 0 {
            "Sleeve (left)"
        } else {
            "Sleeve (right)"
        };
        let sd = &sides[s];
        if no_orientation {
            sleeve_ms.push(Measurement::missing(key, label, orient_reason.clone()));
            continue;
        }
        if !sd.present {
            sleeve_ms.push(Measurement::missing(key, label, no_sleeve_reason(s)));
            continue;
        }
        let (Some(spi), Some(cui)) = (sd.sp[SHOULDER_MAIN], sd.cuff[CUFF_MAIN]) else {
            let reason = if sd.sp[SHOULDER_MAIN].is_none() {
                format!(
                    "The shoulder seam point is not a corner of the outline on this side \
                     (strongest turn {:.0} degrees; at least {:.0} needed), so the sleeve's \
                     start cannot be placed from the outline.",
                    sd.sp_turn, SHOULDER_MIN_TURN
                )
            } else {
                "The cuff end has no distinct corner in the outline (rolled, folded back, or \
                 tucked under)."
                    .to_string()
            };
            sleeve_ms.push(Measurement::missing(key, label, reason));
            continue;
        };
        let path_len = |a: usize, b: usize| -> Option<(f64, Vec<Pt>)> {
            let steps = ((b as isize - a as isize) * sd.dir_s).rem_euclid(n as isize);
            if steps <= 0 || steps as usize > n / 2 {
                return None;
            }
            let mut pts = Vec::with_capacity(steps as usize + 1);
            let mut len = 0.0;
            for t in 0..=steps {
                let i = wrap(a as isize + t * sd.dir_s, n);
                if t > 0 {
                    len += dist(uv[i], *pts.last().unwrap());
                }
                pts.push(uv[i]);
            }
            Some((len, pts))
        };
        let Some((len, path)) = path_len(spi, cui) else {
            sleeve_ms.push(Measurement::missing(
                key,
                label,
                "The sleeve's top edge could not be followed from the shoulder to the cuff."
                    .to_string(),
            ));
            continue;
        };
        let mut dev = 0.0f64;
        for &a in sd.sp.iter().flatten() {
            for &b in sd.cuff.iter().flatten() {
                if let Some((l2, _)) = path_len(a, b) {
                    dev = dev.max((l2 - len).abs());
                }
            }
        }
        // Bend: direction change along the top edge.
        let k15 = ((150.0 / DS) as usize).min(path.len() / 3).max(1);
        let d_start = sub(path[k15], path[0]);
        let d_end = sub(path[path.len() - 1], path[path.len() - 1 - k15]);
        let bend = cross(d_start, d_end).atan2(dot(d_start, d_end)).abs();
        let mut m = blank(
            key,
            label,
            "Along the sleeve's top edge, from the shoulder corner of the outline to the outer \
             corner of the cuff."
                .to_string(),
        );
        let mut lmk = dev;
        if bend.to_degrees() > 10.0 {
            let bend_term = 0.5 * sd.cuff_width_mm * bend;
            lmk = (lmk * lmk + bend_term * bend_term).sqrt();
            m.notes.push(format!(
                "The sleeve bends {:.0} degrees in this photo; the top edge and the sleeve's \
                 centre line differ by about {:.1} cm, included in the landmark term.",
                bend.to_degrees(),
                bend_term / 10.0
            ));
        }
        m.line = path.clone();
        let (a, b) = (path[0], path[path.len() - 1]);
        let mut m = finish(m, len, a, b, lmk);
        // The card term scales with the path length, not the chord.
        m.card_mm = plane.card_half_range_mm(len, to_mm(scale(add(a, b), 0.5)));
        m.plus_minus_mm =
            m.card_mm + (m.mask_mm.powi(2) + m.step_mm.powi(2) + m.landmark_mm.powi(2)).sqrt();
        sleeve_ms.push(m);
    }
    {
        let ok: Vec<&Measurement> = sleeve_ms.iter().filter(|m| m.measurable()).collect();
        let key = "sleeve_length";
        let label = "Sleeve length";
        if ok.is_empty() {
            let reason = sleeve_ms
                .iter()
                .filter_map(|m| m.reason.clone())
                .collect::<Vec<_>>()
                .join(" ");
            measurements.push(Measurement::missing(key, label, reason));
        } else {
            let mean = ok.iter().map(|m| m.value_mm.unwrap()).sum::<f64>() / ok.len() as f64;
            let mut m = blank(
                key,
                label,
                "Shoulder corner to cuff along the sleeve's top edge; mean of the measurable \
                 sleeves."
                    .to_string(),
            );
            m.value_mm = Some(mean);
            m.card_mm = ok.iter().map(|x| x.card_mm).fold(0.0, f64::max);
            m.mask_mm = ok.iter().map(|x| x.mask_mm).fold(0.0, f64::max);
            m.step_mm = ok.iter().map(|x| x.step_mm).fold(0.0, f64::max);
            let mut lmk = ok.iter().map(|x| x.landmark_mm).fold(0.0, f64::max);
            if ok.len() == 2 {
                lmk = lmk.max((ok[0].value_mm.unwrap() - ok[1].value_mm.unwrap()).abs() / 2.0);
            } else {
                m.notes.push(format!("One sleeve only ({}).", ok[0].label));
            }
            m.landmark_mm = lmk;
            m.plus_minus_mm =
                m.card_mm + (m.mask_mm.powi(2) + m.step_mm.powi(2) + lmk.powi(2)).sqrt();
            measurements.push(m);
        }
    }
    measurements.extend(sleeve_ms);

    // Underarm sleeve length: underarm point to the inner cuff corner
    // along the sleeve's underside (the sleeve's underarm seam). It
    // needs no shoulder seam, so it holds on dropped shoulders.
    let mut under_ms: Vec<Measurement> = Vec::new();
    for s in 0..2 {
        let key: &'static str = if s == 0 {
            "underarm_sleeve_left"
        } else {
            "underarm_sleeve_right"
        };
        let label: &'static str = if s == 0 {
            "Underarm sleeve (left)"
        } else {
            "Underarm sleeve (right)"
        };
        let sd = &sides[s];
        if no_orientation {
            under_ms.push(Measurement::missing(key, label, orient_reason.clone()));
            continue;
        }
        if !sd.present {
            under_ms.push(Measurement::missing(key, label, no_sleeve_reason(s)));
            continue;
        }
        let (Some(ai), Some(ci)) = (sd.armpit[ARMPIT_MAIN], sd.cuff_inner[CUFF_MAIN]) else {
            let reason = if sd.armpit[ARMPIT_MAIN].is_none() {
                "The underarm point has no visible corner in the outline (the sleeve hangs \
                 against the body or the armhole is folded)."
            } else {
                "The cuff end has no distinct inner corner in the outline (rolled, folded back, \
                 or tucked under)."
            };
            under_ms.push(Measurement::missing(key, label, reason.to_string()));
            continue;
        };
        let walk = |a: usize, b: usize| -> Option<(f64, Vec<Pt>)> {
            let steps = ((b as isize - a as isize) * -sd.dir_s).rem_euclid(n as isize);
            if steps <= 0 || steps as usize > n / 2 {
                return None;
            }
            let mut pts: Vec<Pt> = Vec::with_capacity(steps as usize + 1);
            let mut len = 0.0;
            for t in 0..=steps {
                let i = wrap(a as isize - t * sd.dir_s, n);
                if t > 0 {
                    len += dist(uv[i], *pts.last().unwrap());
                }
                pts.push(uv[i]);
            }
            Some((len, pts))
        };
        let Some((len, path)) = walk(ai, ci) else {
            under_ms.push(Measurement::missing(
                key,
                label,
                "The sleeve's underside could not be followed from the underarm to the cuff."
                    .to_string(),
            ));
            continue;
        };
        let mut dev = 0.0f64;
        for &a in sd.armpit.iter().flatten() {
            for &b in sd.cuff_inner.iter().flatten() {
                if let Some((l2, _)) = walk(a, b) {
                    dev = dev.max((l2 - len).abs());
                }
            }
        }
        let k15 = ((150.0 / DS) as usize).min(path.len() / 3).max(1);
        let d_start = sub(path[k15], path[0]);
        let d_end = sub(path[path.len() - 1], path[path.len() - 1 - k15]);
        let bend = cross(d_start, d_end).atan2(dot(d_start, d_end)).abs();
        let mut m = blank(
            key,
            label,
            "Along the sleeve's underside (the underarm seam), from the underarm point to the \
             inner corner of the cuff."
                .to_string(),
        );
        let mut lmk = dev;
        if bend.to_degrees() > 10.0 {
            let bend_term = 0.5 * sd.cuff_width_mm * bend;
            lmk = (lmk * lmk + bend_term * bend_term).sqrt();
            m.notes.push(format!(
                "The sleeve bends {:.0} degrees in this photo; the underside and the sleeve's \
                 centre line differ by about {:.1} cm, included in the landmark term.",
                bend.to_degrees(),
                bend_term / 10.0
            ));
        }
        m.line = path.clone();
        let (a, b) = (path[0], path[path.len() - 1]);
        let mut m = finish(m, len, a, b, lmk);
        m.card_mm = plane.card_half_range_mm(len, to_mm(scale(add(a, b), 0.5)));
        m.plus_minus_mm =
            m.card_mm + (m.mask_mm.powi(2) + m.step_mm.powi(2) + m.landmark_mm.powi(2)).sqrt();
        under_ms.push(m);
    }
    measurements.extend(under_ms);

    // Hem width.
    {
        let key = "hem_width";
        let label = "Hem width";
        if no_orientation {
            measurements.push(Measurement::missing(key, label, orient_reason.clone()));
        } else {
            let half_w = measurements
                .iter()
                .find(|m| m.key == "chest_width")
                .and_then(|m| m.value_mm)
                .unwrap_or_else(|| {
                    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
                    for &i in &core_px {
                        let u = to_uv(ras.centre_mm(i))[0];
                        lo = lo.min(u);
                        hi = hi.max(u);
                    }
                    hi - lo
                })
                / 2.0;
            let mut vs: Vec<f64> = Vec::new();
            let mut u = -half_w / 2.0;
            while u <= half_w / 2.0 {
                if let Some(v) = v_hem_at(u) {
                    vs.push(v);
                }
                u += 10.0;
            }
            if vs.is_empty() {
                measurements.push(Measurement::missing(
                    key,
                    label,
                    "The hem edge is not found below the body.".to_string(),
                ));
            } else {
                let v_med = median(&mut vs);
                // A gathered or uneven hem: put the line above the
                // highest point of the bottom edge across the middle 80%
                // of the hem, so it crosses the two sides and not the
                // bottom edge.
                let mut v_hem = v_med;
                let mut hem_spread = 0.0;
                if let Some((ul, _, ur, _)) = span_at(v_med - 30.0) {
                    let w = ur - ul;
                    let mut bottoms: Vec<f64> = Vec::new();
                    let mut u = ul + 0.1 * w;
                    while u <= ur - 0.1 * w {
                        if let Some(v) = v_hem_at(u) {
                            bottoms.push(v);
                        }
                        u += 5.0;
                    }
                    if !bottoms.is_empty() {
                        let lo = bottoms.iter().cloned().fold(f64::INFINITY, f64::min);
                        let hi = bottoms.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                        v_hem = lo;
                        hem_spread = hi - lo;
                    }
                }
                let check = |v: f64| -> Result<(f64, f64, f64), String> {
                    let (ul, ll, ur, lr) = span_at(v).ok_or_else(|| {
                        "The hem line does not cross the body cleanly.".to_string()
                    })?;
                    if ll != 0 || lr != 0 {
                        return Err(
                            "A sleeve lies against the hem, so the hem's side ends are not \
                                    visible."
                                .to_string(),
                        );
                    }
                    Ok((ur - ul, ul, ur))
                };
                match check(v_hem - HEM_OFFSETS[HEM_MAIN]) {
                    Err(reason) => measurements.push(Measurement::missing(key, label, reason)),
                    Ok((w0, ul, ur)) => {
                        let mut dev = 0.0f64;
                        for &o in HEM_OFFSETS.iter() {
                            if let Ok((w, _, _)) = check(v_hem - o) {
                                dev = dev.max((w - w0).abs());
                            }
                        }
                        let vl = v_hem - HEM_OFFSETS[HEM_MAIN];
                        let mut m = blank(
                            key,
                            label,
                            format!(
                                "Straight across the bottom of the body {:.0} cm above the \
                                 highest point of the hem edge (middle 80% of its width), side \
                                 edge to side edge.",
                                HEM_OFFSETS[HEM_MAIN] / 10.0
                            ),
                        );
                        if hem_spread > 10.0 {
                            m.notes.push(format!(
                                "The hem edge is uneven in this photo (it varies {:.1} cm along \
                                 the width: gathered or not pulled straight).",
                                hem_spread / 10.0
                            ));
                        }
                        m.line = vec![[ul, vl], [ur, vl]];
                        measurements.push(finish(m, w0, [ul, vl], [ur, vl], dev));
                    }
                }
            }
        }
    }

    // Order: body length, chest, shoulder, sleeve, sleeves per side, hem.
    let order = [
        "body_length",
        "chest_width",
        "shoulder_width",
        "sleeve_length",
        "sleeve_length_left",
        "sleeve_length_right",
        "underarm_sleeve_left",
        "underarm_sleeve_right",
        "hem_width",
    ];
    measurements.sort_by_key(|m| order.iter().position(|k| *k == m.key).unwrap_or(99));

    notes.push(format!(
        "Torso core: opening of the outline with a {:.0} mm disk (0.6 of its widest inscribed \
         radius, {:.0} mm); sleeves are what the opening removes and reaches out from the torso.",
        2.0 * rho_mm,
        rmax_mm
    ));
    Ok(Garment {
        measurements,
        landmarks,
        outline_uv: uv,
        axis_deg,
        symmetry,
        sleeves_found: n_sleeves,
        mask_pixels,
        mask_edge_px: edge_px,
        mm_per_px,
        notes,
    })
}

/// The first strong convex corner met walking from `base` in `dir`,
/// searched within 250 mm before (and 30 mm past) the far point at step
/// `far_t`.
fn first_corner(
    uv: &[Pt],
    conv: f64,
    base: usize,
    dir: isize,
    far_t: isize,
    kk: usize,
) -> Option<usize> {
    let n = uv.len();
    let lo = (far_t - (250.0 / DS) as isize).max(1);
    let hi = far_t + (30.0 / DS) as isize;
    let mut cur_peak: Option<(isize, f64)> = None;
    for t in lo..=hi {
        let i = wrap(base as isize + t * dir, n);
        let tv = turn_at(uv, i as isize, kk, conv);
        if tv >= CUFF_MIN_TURN {
            if cur_peak.is_none_or(|p| tv > p.1) {
                cur_peak = Some((t, tv));
            }
        } else if let Some(p) = cur_peak {
            return Some(wrap(base as isize + p.0 * dir, n));
        }
    }
    cur_peak.map(|p| wrap(base as isize + p.0 * dir, n))
}

/// Longest circular run of `true`, merging runs split by gaps of at
/// most `max_gap` points. Returns (start index, length).
fn longest_run(on: &[bool], max_gap: usize) -> Option<(usize, usize)> {
    let n = on.len();
    if !on.iter().any(|&b| b) {
        return None;
    }
    if on.iter().all(|&b| b) {
        return Some((0, n));
    }
    // Start scanning just after a false point so runs do not wrap.
    let s0 = (0..n).find(|&i| !on[i]).unwrap();
    let mut runs: Vec<(usize, usize)> = Vec::new(); // (start offset, len) in scan order
    let mut t = 0;
    while t < n {
        let i = (s0 + t) % n;
        if on[i] {
            let start = t;
            while t < n && on[(s0 + t) % n] {
                t += 1;
            }
            runs.push((start, t - start));
        } else {
            t += 1;
        }
    }
    // Merge.
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for r in runs {
        if let Some(last) = merged.last_mut() {
            let gap = r.0 - (last.0 + last.1);
            if gap <= max_gap {
                last.1 = r.0 + r.1 - last.0;
                continue;
            }
        }
        merged.push(r);
    }
    let best = merged.iter().max_by_key(|r| r.1)?;
    Some(((s0 + best.0) % n, best.1))
}

// ---------------------------------------------------------------------------
// Diagram

struct Canvas {
    w: usize,
    h: usize,
    px: Vec<f32>, // RGB 0..1
}

fn hex(c: u32) -> [f32; 3] {
    [
        ((c >> 16) & 255) as f32 / 255.0,
        ((c >> 8) & 255) as f32 / 255.0,
        (c & 255) as f32 / 255.0,
    ]
}

impl Canvas {
    fn new(w: usize, h: usize, bg: u32) -> Canvas {
        let c = hex(bg);
        let mut px = vec![0.0f32; w * h * 3];
        for i in 0..w * h {
            px[i * 3..i * 3 + 3].copy_from_slice(&c);
        }
        Canvas { w, h, px }
    }

    fn blend(&mut self, x: i64, y: i64, c: [f32; 3], a: f32) {
        if x < 0 || y < 0 || x >= self.w as i64 || y >= self.h as i64 || a <= 0.0 {
            return;
        }
        let a = a.min(1.0);
        let k = (y as usize * self.w + x as usize) * 3;
        for ch in 0..3 {
            self.px[k + ch] = self.px[k + ch] * (1.0 - a) + c[ch] * a;
        }
    }

    fn rect(&mut self, x0: f64, y0: f64, x1: f64, y1: f64, c: u32) {
        let c = hex(c);
        for y in y0.round() as i64..y1.round() as i64 {
            for x in x0.round() as i64..x1.round() as i64 {
                self.blend(x, y, c, 1.0);
            }
        }
    }

    /// Anti-aliased thick segment.
    fn line(&mut self, a: Pt, b: Pt, width: f64, c: u32) {
        let c = hex(c);
        let hw = width / 2.0;
        let x0 = (a[0].min(b[0]) - hw - 1.0).floor() as i64;
        let x1 = (a[0].max(b[0]) + hw + 1.0).ceil() as i64;
        let y0 = (a[1].min(b[1]) - hw - 1.0).floor() as i64;
        let y1 = (a[1].max(b[1]) + hw + 1.0).ceil() as i64;
        let ab = sub(b, a);
        let l2 = dot(ab, ab);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let p = [x as f64 + 0.5, y as f64 + 0.5];
                let t = if l2 > 0.0 {
                    (dot(sub(p, a), ab) / l2).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let d = dist(p, add(a, scale(ab, t)));
                let cov = (hw + 0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend(x, y, c, cov as f32);
                }
            }
        }
    }

    fn polyline(&mut self, pts: &[Pt], width: f64, c: u32) {
        for w in pts.windows(2) {
            self.line(w[0], w[1], width, c);
        }
    }

    fn disc(&mut self, p: Pt, r: f64, c: u32) {
        let col = hex(c);
        for y in (p[1] - r - 1.0).floor() as i64..=(p[1] + r + 1.0).ceil() as i64 {
            for x in (p[0] - r - 1.0).floor() as i64..=(p[0] + r + 1.0).ceil() as i64 {
                let d = dist([x as f64 + 0.5, y as f64 + 0.5], p);
                self.blend(x, y, col, (r + 0.5 - d).clamp(0.0, 1.0) as f32);
            }
        }
    }

    fn fill_polygon(&mut self, poly: &[Pt], c: u32) {
        let col = hex(c);
        let n = poly.len();
        let (mut ymin, mut ymax) = (f64::INFINITY, f64::NEG_INFINITY);
        for p in poly {
            ymin = ymin.min(p[1]);
            ymax = ymax.max(p[1]);
        }
        let mut xs: Vec<f64> = Vec::new();
        for y in ymin.floor() as i64..=ymax.ceil() as i64 {
            let yc = y as f64 + 0.5;
            xs.clear();
            for k in 0..n {
                let a = poly[k];
                let b = poly[(k + 1) % n];
                if (a[1] > yc) != (b[1] > yc) {
                    xs.push(a[0] + (yc - a[1]) * (b[0] - a[0]) / (b[1] - a[1]));
                }
            }
            xs.sort_by(|a, b| a.total_cmp(b));
            for pair in xs.chunks(2) {
                if pair.len() == 2 {
                    for x in (pair[0] - 0.5).ceil() as i64..=(pair[1] - 0.5).floor() as i64 {
                        self.blend(x, y, col, 1.0);
                    }
                }
            }
        }
    }

    fn text_width(s: &str, scale: usize) -> usize {
        s.chars().count() * (measure_font::GLYPH_W + 1) * scale
    }

    /// Bitmap text with its top-left at (x, y).
    fn text(&mut self, x: f64, y: f64, s: &str, scale: usize, c: u32) {
        let col = hex(c);
        let mut cx = x.round() as i64;
        let y0 = y.round() as i64;
        for ch in s.chars() {
            let g = measure_font::glyph(ch);
            for (row, line) in g.iter().enumerate() {
                for (colx, b) in line.bytes().enumerate() {
                    if b == b'#' {
                        for dy in 0..scale as i64 {
                            for dx in 0..scale as i64 {
                                self.blend(
                                    cx + colx as i64 * scale as i64 + dx,
                                    y0 + row as i64 * scale as i64 + dy,
                                    col,
                                    1.0,
                                );
                            }
                        }
                    }
                }
            }
            cx += ((measure_font::GLYPH_W + 1) * scale) as i64;
        }
    }

    fn label(&mut self, centre: Pt, s: &str, scale: usize, c: u32) {
        let tw = Self::text_width(s, scale) as f64;
        let th = (7 * scale) as f64;
        let pad = 2.0 * scale as f64;
        let (x0, y0) = (centre[0] - tw / 2.0, centre[1] - th / 2.0);
        self.rect(
            x0 - pad,
            y0 - pad,
            x0 + tw + pad - scale as f64,
            y0 + th + pad,
            0xFFFFFF,
        );
        self.line(
            [x0 - pad, y0 - pad],
            [x0 + tw + pad - scale as f64, y0 - pad],
            1.5,
            c,
        );
        self.line(
            [x0 - pad, y0 + th + pad],
            [x0 + tw + pad - scale as f64, y0 + th + pad],
            1.5,
            c,
        );
        self.line([x0 - pad, y0 - pad], [x0 - pad, y0 + th + pad], 1.5, c);
        self.line(
            [x0 + tw + pad - scale as f64, y0 - pad],
            [x0 + tw + pad - scale as f64, y0 + th + pad],
            1.5,
            c,
        );
        self.text(x0, y0, s, scale, c);
    }

    fn into_rgba(self) -> RgbaImage {
        let mut out = vec![0u8; self.w * self.h * 4];
        for i in 0..self.w * self.h {
            for ch in 0..3 {
                out[i * 4 + ch] = (self.px[i * 3 + ch] * 255.0).round().clamp(0.0, 255.0) as u8;
            }
            out[i * 4 + 3] = 255;
        }
        RgbaImage::new(self.w as u32, self.h as u32, out)
    }
}

fn colour_of(key: &str) -> u32 {
    match key {
        "body_length" => 0x1D4ED8,
        "chest_width" => 0xC2410C,
        "shoulder_width" => 0x047857,
        "sleeve_length" | "sleeve_length_left" | "sleeve_length_right" => 0x7E22CE,
        "underarm_sleeve_left" | "underarm_sleeve_right" => 0x0E7490,
        "hem_width" => 0xB91C1C,
        _ => 0x111827,
    }
}

fn fmt_cm(m: &Measurement) -> String {
    match m.value_cm() {
        Some(v) => format!("{:.1} \u{00b1} {:.1} cm", v, m.plus_minus_cm()),
        None => "not measurable".to_string(),
    }
}

/// Word-wrap to a pixel width at a text scale.
fn wrap_text(s: &str, max_px: usize, scale: usize) -> Vec<String> {
    let max_chars = (max_px / ((measure_font::GLYPH_W + 1) * scale)).max(8);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > max_chars {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Render the 1920 x 1080 measurement diagram: the rectified outline
/// (collar up), landmarks, measurement lines with cm labels, and a
/// legend that names every figure, including the ones the photo
/// cannot support.
pub fn render_diagram(g: &Garment, card_id: &str, note: Option<&str>) -> RgbaImage {
    let (cw, ch) = (1920usize, 1080usize);
    let mut cv = Canvas::new(cw, ch, 0xFFFFFF);
    let ink = 0x111827;
    let grey = 0x6B7280;

    // Fit the outline into the drawing area.
    let area = (60.0, 150.0, 1220.0, 1000.0);
    let (mut lo, mut hi) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
    for p in &g.outline_uv {
        lo = [lo[0].min(p[0]), lo[1].min(p[1])];
        hi = [hi[0].max(p[0]), hi[1].max(p[1])];
    }
    let sx = (area.2 - area.0) / (hi[0] - lo[0]).max(1.0);
    let sy = (area.3 - area.1) / (hi[1] - lo[1]).max(1.0);
    let s = sx.min(sy);
    let ox = area.0 + ((area.2 - area.0) - s * (hi[0] - lo[0])) / 2.0;
    let oy = area.1 + ((area.3 - area.1) - s * (hi[1] - lo[1])) / 2.0;
    let map = |p: Pt| -> Pt { [ox + (p[0] - lo[0]) * s, oy + (p[1] - lo[1]) * s] };

    let poly: Vec<Pt> = g.outline_uv.iter().map(|&p| map(p)).collect();
    cv.fill_polygon(&poly, 0xEEF0F3);
    let mut closed = poly.clone();
    if let Some(first) = poly.first() {
        closed.push(*first);
    }
    cv.polyline(&closed, 2.0, 0x9CA3AF);

    // Title.
    cv.text(60.0, 40.0, "Flat-lay measurements", 5, ink);
    cv.text(
        60.0,
        92.0,
        "ESTIMATED from one photo with a printed scale card",
        3,
        grey,
    );

    // Measurement lines and labels.
    let mut label_boxes: Vec<(Pt, String, u32)> = Vec::new();
    for m in &g.measurements {
        if !m.measurable() || m.key == "sleeve_length" {
            continue;
        }
        let c = colour_of(&m.key);
        let pts: Vec<Pt> = m.line.iter().map(|&p| map(p)).collect();
        match m.key {
            "body_length" => {
                for seg in pts.chunks(2) {
                    if seg.len() == 2 {
                        cv.line(seg[0], seg[1], 3.0, c);
                        for &e in seg {
                            cv.line([e[0] - 9.0, e[1]], [e[0] + 9.0, e[1]], 3.0, c);
                        }
                    }
                }
                let a = pts[0];
                let b = pts[1];
                label_boxes.push((
                    [a[0] + 0.0, (a[1] + b[1]) / 2.0],
                    format!("{:.1} cm", m.value_cm().unwrap()),
                    c,
                ));
            }
            k if k.starts_with("sleeve_length_") || k.starts_with("underarm_sleeve_") => {
                cv.polyline(&pts, 3.0, c);
                let mid = pts[pts.len() / 2];
                label_boxes.push((mid, format!("{:.1} cm", m.value_cm().unwrap()), c));
            }
            _ => {
                let (a, b) = (pts[0], pts[pts.len() - 1]);
                cv.line(a, b, 3.0, c);
                let d = unit(sub(b, a));
                let nrm = [-d[1], d[0]];
                for &e in &[a, b] {
                    cv.line(add(e, scale(nrm, -9.0)), add(e, scale(nrm, 9.0)), 3.0, c);
                }
                label_boxes.push((
                    scale(add(a, b), 0.5),
                    format!("{:.1} cm", m.value_cm().unwrap()),
                    c,
                ));
            }
        }
    }
    for l in &g.landmarks {
        let p = map(l.uv);
        cv.disc(p, 7.0, 0xFFFFFF);
        cv.disc(p, 5.0, ink);
    }
    // Labels last, nudged apart vertically when they would overlap.
    let mut placed: Vec<(f64, f64, f64, f64)> = Vec::new();
    for (centre, text, c) in label_boxes {
        let tw = Canvas::text_width(&text, 3) as f64 + 12.0;
        let th = 21.0 + 12.0;
        let mut ctr = centre;
        for _ in 0..12 {
            let bx = (
                ctr[0] - tw / 2.0,
                ctr[1] - th / 2.0,
                ctr[0] + tw / 2.0,
                ctr[1] + th / 2.0,
            );
            if placed
                .iter()
                .any(|p| bx.0 < p.2 && bx.2 > p.0 && bx.1 < p.3 && bx.3 > p.1)
            {
                ctr[1] += th + 4.0;
            } else {
                break;
            }
        }
        placed.push((
            ctr[0] - tw / 2.0,
            ctr[1] - th / 2.0,
            ctr[0] + tw / 2.0,
            ctr[1] + th / 2.0,
        ));
        cv.label(ctr, &text, 3, c);
    }

    // Legend panel.
    let px0 = 1290.0;
    let pw = 1880 - 1290;
    cv.rect(px0 - 20.0, 150.0, px0 - 18.0, 1000.0, 0xE5E7EB);
    let mut y = 160.0;
    for m in &g.measurements {
        if m.key == "sleeve_length" {
            continue;
        }
        let c = if m.measurable() {
            colour_of(&m.key)
        } else {
            0x9CA3AF
        };
        cv.rect(px0, y + 2.0, px0 + 18.0, y + 20.0, c);
        cv.text(
            px0 + 30.0,
            y,
            m.label,
            3,
            if m.measurable() { ink } else { grey },
        );
        y += 30.0;
        cv.text(
            px0 + 30.0,
            y,
            &fmt_cm(m),
            3,
            if m.measurable() { c } else { grey },
        );
        y += 30.0;
        if let Some(reason) = &m.reason {
            let short = short_reason(reason);
            for line in wrap_text(&short, pw - 30, 2) {
                cv.text(px0 + 30.0, y, &line, 2, grey);
                y += 20.0;
            }
        }
        y += 14.0;
    }
    y += 6.0;
    let foot = format!(
        "Card {card_id} assumed printed at 100%. Garment assumed flat on the card's surface. \
         Values are estimates with a half range (\u{00b1})."
    );
    for line in wrap_text(&foot, pw, 2) {
        cv.text(px0, y, &line, 2, grey);
        y += 20.0;
    }
    if let Some(note) = note {
        y += 8.0;
        for line in wrap_text(note, pw, 3) {
            cv.text(px0, y, &line, 3, 0xB91C1C);
            y += 28.0;
        }
    }
    let _ = y;
    cv.text(
        60.0,
        1040.0,
        "collar up, as laid flat; left and right as drawn",
        2,
        grey,
    );
    cv.into_rgba()
}

fn short_reason(reason: &str) -> String {
    if reason.starts_with("No sleeve leaves the outline") {
        "Sleeve folded or tucked: landmarks hidden in this photo.".to_string()
    } else if reason.starts_with("The shoulder seam point is not a corner") {
        "Shoulder seam is not a corner of the outline (rounded or dropped shoulder).".to_string()
    } else if reason.starts_with("The collar base") {
        "Collar base not visible in the outline.".to_string()
    } else if reason.starts_with("No sleeve leaves the body outline") {
        "Garment orientation not readable from the outline.".to_string()
    } else {
        let s: String = reason.chars().take(110).collect();
        s
    }
}
