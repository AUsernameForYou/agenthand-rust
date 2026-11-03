//! Ground shadow for generated plates (docs/GROUND-SHADOW.md).
//!
//! A generated plate is drawn without the product, so its ground runs
//! straight under the product with no shadow break and the product
//! reads as floating. `source_edit` plates keep the photo's own shadow
//! (shadow carry, contact ellipse); generated plates get this instead:
//! a contact shadow hugging the product's ground-contact points and a
//! short soft cast shadow, both multiplied onto PLATE pixels only.
//!
//! 1. Ground line: the lowest product pixel per column (alpha >= 128),
//!    read per grid column; the lower convex hull of that profile; its
//!    longest near-level edge (|slope| <= 0.35, at least a quarter of
//!    the product width) is the ground line, else a level line through
//!    the lowest point. Contact points are the column runs within
//!    `CONTACT_TOL` of the line (tyre bottoms, feet, a base).
//! 2. Contact shadow: per column, weight exp(-gap / g0) where gap is
//!    the product's clearance above the line (g0 = 3% of the product
//!    height), laid on the ground line as a thin band (wider on the
//!    side the shadow falls to) and blurred along it. It hugs what
//!    touches the ground and fades out under anything raised.
//! 3. Cast shadow: every product cell (alpha coverage on a grid of at
//!    most 1024 cells on the long side) is projected onto the ground
//!    away from the light, `h` = its height above the line:
//!    behind (front-lit) y' = L - 0.22 h; toward the camera
//!    y' = L + 0.30 h; to a side x' = x +- 0.50 h, y' = L - 0.06 h;
//!    overhead y' = L - 0.03 h. Density is normalised by the mapping's
//!    area scale (a solid occluder gives 1) and split in three height
//!    bands blurred wider with height (penumbra) and fainter with
//!    height (fill light). Thin parts (spokes) cover a fraction of a
//!    cell, so their shadow blurs out faint instead of forming a blob.
//! 4. plate RGB *= (1 - cast) (1 - contact), only where the product
//!    layer's alpha is below 255. No product pixel is ever written.
//!
//! The light side comes from `staging::estimate_light_direction`; when
//! it is inconclusive the shadow falls straight back (front-lit, light
//! from behind the camera). That default hides most of the cast shadow
//! behind the product, so a wrong guess costs little.

use serde_json::{json, Value};

use crate::composite::OPAQUE_ALPHA_THRESHOLD;
use crate::image::RgbaImage;
use crate::staging::LightSide;

/// Kill switch: `AGENTHAND_GROUND_SHADOW=off` (also 0, false, no).
pub const GROUND_SHADOW_ENV: &str = "AGENTHAND_GROUND_SHADOW";
pub const GROUND_SHADOW_VERSION: i64 = 1;

/// Analysis grid: long side at most this many cells.
pub const GROUND_GRID_MAX: usize = 1024;
/// A column counts as a contact when its lowest product pixel is
/// within this share of the product height of the ground line
/// (at least 1.5 grid cells).
pub const CONTACT_TOL: f64 = 0.012;
/// Contact falloff: weight exp(-gap / (this x product height)).
pub const CONTACT_FALLOFF: f64 = 0.03;
/// Contact band, x product height: vertical sigma on the side the
/// shadow falls to, on the other side (the crease toward the camera
/// for front light), and the horizontal blur.
pub const CONTACT_SIGMA_SHADOW_SIDE: f64 = 0.012;
pub const CONTACT_SIGMA_OTHER_SIDE: f64 = 0.005;
pub const CONTACT_BLUR_X: f64 = 0.012;
/// Peak darkening of the contact shadow (0..1).
pub const CONTACT_OPACITY: f64 = 0.6;
/// Peak darkening of the cast shadow (0..1).
pub const CAST_OPACITY: f64 = 0.40;
/// Height bands (x product height), their blur radius (x product
/// height) and their opacity weight.
pub const CAST_BANDS: [(f64, f64, f64); 3] = [(0.20, 0.008, 1.0), (0.50, 0.016, 0.8), (f64::INFINITY, 0.028, 0.6)];
/// Vertical blur as a share of the horizontal (the ground is
/// foreshortened in depth).
pub const CAST_BLUR_Y_SHARE: f64 = 0.5;
/// Projection per shadow side: (x per unit height, y per unit height),
/// y down; the shadow point is (x + ax h, L(x) + ay h).
pub const PROJECT_BEHIND: (f64, f64) = (0.0, -0.22);
pub const PROJECT_TOWARD: (f64, f64) = (0.0, 0.30);
pub const PROJECT_SIDE: (f64, f64) = (0.50, -0.06);
pub const PROJECT_OVERHEAD: (f64, f64) = (0.0, -0.03);
/// Near-level hull edges only; slopes above this are tyre flanks.
pub const GROUND_EDGE_MAX_SLOPE: f64 = 0.35;
pub const GROUND_EDGE_MIN_SHARE: f64 = 0.25;

/// Whether the ground shadow runs at all (the kill switch).
pub fn ground_shadow_enabled() -> bool {
    !matches!(
        std::env::var(GROUND_SHADOW_ENV)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "off" | "false" | "no"
    )
}

/// The light the shadow follows: the estimate's side (None when it
/// was inconclusive) and its method.
#[derive(Debug, Clone, PartialEq)]
pub struct GroundLight {
    pub side: Option<LightSide>,
    pub method: String,
}

impl GroundLight {
    pub fn from_estimate(estimate: &crate::staging::LightEstimate) -> Self {
        GroundLight {
            side: estimate.side,
            method: estimate.method.to_string(),
        }
    }

    /// The side the shadow follows: the estimate's, else front-lit.
    pub fn effective(&self) -> (LightSide, &'static str) {
        match self.side {
            Some(side) => (side, "light_estimate"),
            None => (LightSide::FromCamera, "default_front_lit"),
        }
    }
}

/// What the ground shadow did; `to_json` is the `ground_shadow` record.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroundReport {
    pub applied: bool,
    pub reason: Option<String>,
    pub light_side: Option<String>,
    pub light_method: Option<String>,
    pub direction_source: Option<String>,
    pub shadow_falls: Option<String>,
    /// Full-resolution ground line end points.
    pub ground_line: Option<[[f64; 2]; 2]>,
    /// Contact runs: [x_start, x_end, y] in pixels.
    pub contacts: Vec<[f64; 3]>,
    pub grid_cell_px: u32,
    pub product_height_px: u32,
    pub cast_blur_px: Vec<f64>,
    pub contact_blur_px: f64,
    /// Plate pixels the shadow changed.
    pub shadow_pixels: u64,
    /// Strongest darkening applied (0..1).
    pub max_darkening: f64,
    /// Pixel SHA-256 of the plate actually composited (with the shadow).
    pub plate_pixel_sha256: Option<String>,
}

impl GroundReport {
    pub fn skipped(reason: &str) -> Self {
        GroundReport {
            reason: Some(reason.to_string()),
            ..Default::default()
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "version": GROUND_SHADOW_VERSION,
            "applied": self.applied,
            "reason": self.reason,
            "light": {
                "side": self.light_side,
                "method": self.light_method,
                "direction_source": self.direction_source,
            },
            "shadow_falls": self.shadow_falls,
            "ground_line": self.ground_line.map(|l| l.iter().map(|p| [p[0].round(), p[1].round()]).collect::<Vec<_>>()),
            "contacts": self.contacts.iter().map(|c| json!({"x_start": c[0].round(), "x_end": c[1].round(), "y": c[2].round()})).collect::<Vec<_>>(),
            "contact": {"opacity": CONTACT_OPACITY, "blur_px": crate::round6(self.contact_blur_px)},
            "cast": {
                "opacity": CAST_OPACITY,
                "blur_px": self.cast_blur_px.iter().map(|b| crate::round6(*b)).collect::<Vec<_>>(),
                "projection": self.shadow_falls.as_deref().map(|s| {
                    let (ax, ay) = projection_for(s);
                    json!([ax, ay])
                }),
            },
            "grid_cell_px": self.grid_cell_px,
            "product_height_px": self.product_height_px,
            "shadow_pixels": self.shadow_pixels,
            "max_darkening": crate::round6(self.max_darkening),
            "product_pixels_written": 0,
            "plate_pixel_sha256": self.plate_pixel_sha256,
        })
    }
}

fn projection_for(shadow_falls: &str) -> (f64, f64) {
    match shadow_falls {
        "behind" => PROJECT_BEHIND,
        "toward_camera" => PROJECT_TOWARD,
        "camera-right" => PROJECT_SIDE,
        "camera-left" => (-PROJECT_SIDE.0, PROJECT_SIDE.1),
        _ => PROJECT_OVERHEAD,
    }
}

/// The grounded plate and its record.
pub struct GroundOutput {
    pub plate: RgbaImage,
    pub report: GroundReport,
}

/// The gated ground shadow: kill switch first, then `ground_shadow`.
pub fn ground_shadow_for_draw(
    plate: &RgbaImage,
    foreground: &RgbaImage,
    light: &GroundLight,
) -> GroundOutput {
    if !ground_shadow_enabled() {
        return GroundOutput {
            plate: plate.clone(),
            report: GroundReport::skipped(&format!("disabled by {GROUND_SHADOW_ENV}")),
        };
    }
    ground_shadow(plate, foreground, light)
}

/// Normalised 1D box blur along rows (horizontal) of a grid.
fn box_rows(data: &[f32], w: usize, h: usize, r: usize) -> Vec<f32> {
    if r == 0 {
        return data.to_vec();
    }
    let mut out = vec![0f32; data.len()];
    let norm = 1.0 / (2 * r + 1) as f32;
    for y in 0..h {
        let row = &data[y * w..(y + 1) * w];
        let mut acc = 0f32;
        // Zero padding outside the grid (the shadow fades off the edge).
        for x in 0..=r.min(w - 1) {
            acc += row[x];
        }
        for x in 0..w {
            out[y * w + x] = acc * norm;
            let add = x + r + 1;
            if add < w {
                acc += row[add];
            }
            if x >= r {
                acc -= row[x - r];
            }
        }
    }
    out
}

fn box_cols(data: &[f32], w: usize, h: usize, r: usize) -> Vec<f32> {
    if r == 0 {
        return data.to_vec();
    }
    let mut out = vec![0f32; data.len()];
    let norm = 1.0 / (2 * r + 1) as f32;
    for x in 0..w {
        let mut acc = 0f32;
        for y in 0..=r.min(h - 1) {
            acc += data[y * w + x];
        }
        for y in 0..h {
            out[y * w + x] = acc * norm;
            let add = y + r + 1;
            if add < h {
                acc += data[add * w + x];
            }
            if y >= r {
                acc -= data[(y - r) * w + x];
            }
        }
    }
    out
}

/// Three box passes (close to a Gaussian), radii in grid cells.
fn soft_blur(data: &[f32], w: usize, h: usize, rx: f64, ry: f64) -> Vec<f32> {
    let bx = (rx / 3f64.sqrt()).round().max(0.0) as usize;
    let by = (ry / 3f64.sqrt()).round().max(0.0) as usize;
    let mut cur = data.to_vec();
    for _ in 0..3 {
        cur = box_rows(&cur, w, h, bx);
        cur = box_cols(&cur, w, h, by);
    }
    cur
}

/// Bilinear splat of `value` at grid position (x, y) (cell centres at
/// integer + 0.5).
fn splat(buf: &mut [f32], w: usize, h: usize, x: f64, y: f64, value: f32) {
    let fx = x - 0.5;
    let fy = y - 0.5;
    let x0 = fx.floor();
    let y0 = fy.floor();
    let tx = (fx - x0) as f32;
    let ty = (fy - y0) as f32;
    for (dx, dy, wgt) in [
        (0i64, 0i64, (1.0 - tx) * (1.0 - ty)),
        (1, 0, tx * (1.0 - ty)),
        (0, 1, (1.0 - tx) * ty),
        (1, 1, tx * ty),
    ] {
        let gx = x0 as i64 + dx;
        let gy = y0 as i64 + dy;
        if gx >= 0 && gy >= 0 && (gx as usize) < w && (gy as usize) < h {
            buf[gy as usize * w + gx as usize] += value * wgt;
        }
    }
}

/// The ground line and contacts read off the product's bottom profile.
struct GroundGeometry {
    /// L(x) = y0 + slope * (x - x0), full-resolution pixels.
    x0: f64,
    y0: f64,
    slope: f64,
    line: [[f64; 2]; 2],
    contacts: Vec<[f64; 3]>,
    /// Per grid column: the lowest product pixel's bottom edge, if any.
    bottom: Vec<Option<f64>>,
}

impl GroundGeometry {
    fn at(&self, x: f64) -> f64 {
        self.y0 + self.slope * (x - self.x0)
    }
}

fn ground_geometry(
    foreground: &RgbaImage,
    bbox: (u32, u32, u32, u32),
    cell: usize,
    gw: usize,
) -> Option<GroundGeometry> {
    let (bx0, by0, bx1, by1) = bbox;
    let fw = foreground.width as usize;
    let height_px = (by1 - by0) as f64;
    let width_px = (bx1 - bx0) as f64;
    // Lowest product pixel per full-resolution column, folded per grid
    // column (the lowest in the cell).
    let mut bottom: Vec<Option<f64>> = vec![None; gw];
    for x in bx0 as usize..bx1 as usize {
        let mut y = by1 as usize;
        while y > by0 as usize {
            y -= 1;
            if foreground.pixels[(y * fw + x) * 4 + 3] >= 128 {
                let b = (y + 1) as f64;
                let g = x / cell;
                bottom[g] = Some(bottom[g].map_or(b, |c: f64| c.max(b)));
                break;
            }
        }
    }
    let pts: Vec<(f64, f64)> = bottom
        .iter()
        .enumerate()
        .filter_map(|(g, b)| b.map(|b| ((g as f64 + 0.5) * cell as f64, b)))
        .collect();
    if pts.is_empty() {
        return None;
    }
    // Lower hull (largest y), x ascending.
    let mut hull: Vec<(f64, f64)> = Vec::new();
    for &p in &pts {
        while hull.len() >= 2 {
            let (x1, y1) = hull[hull.len() - 2];
            let (x2, y2) = hull[hull.len() - 1];
            let cross = (x2 - x1) * (p.1 - y1) - (y2 - y1) * (p.0 - x1);
            if cross >= 0.0 {
                hull.pop();
            } else {
                break;
            }
        }
        hull.push(p);
    }
    let lowest = pts.iter().map(|p| p.1).fold(f64::MIN, f64::max);
    let mut best: Option<((f64, f64), (f64, f64))> = None;
    for pair in hull.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let dx = b.0 - a.0;
        if dx <= 0.0 {
            continue;
        }
        let slope = (b.1 - a.1) / dx;
        if slope.abs() > GROUND_EDGE_MAX_SLOPE {
            continue;
        }
        if best.map_or(true, |(p, q)| dx > q.0 - p.0) {
            best = Some((a, b));
        }
    }
    let (x0, y0, slope) = match best {
        Some((a, b)) if b.0 - a.0 >= GROUND_EDGE_MIN_SHARE * width_px => {
            (a.0, a.1, (b.1 - a.1) / (b.0 - a.0))
        }
        _ => (bx0 as f64, lowest, 0.0),
    };
    let mut geo = GroundGeometry {
        x0,
        y0,
        slope,
        line: [[bx0 as f64, 0.0], [bx1 as f64, 0.0]],
        contacts: Vec::new(),
        bottom,
    };
    geo.line[0][1] = geo.at(bx0 as f64);
    geo.line[1][1] = geo.at(bx1 as f64);
    // Contact runs: columns within tolerance of the line; gaps of up to
    // two columns merge.
    let tol = (CONTACT_TOL * height_px).max(1.5 * cell as f64);
    let mut run: Option<(usize, usize)> = None;
    let mut runs = Vec::new();
    for (g, b) in geo.bottom.iter().enumerate() {
        let xc = (g as f64 + 0.5) * cell as f64;
        let near = b.is_some_and(|b| geo.at(xc) - b <= tol);
        if near {
            run = match run {
                Some((s, e)) if g <= e + 3 => Some((s, g)),
                Some(r) => {
                    runs.push(r);
                    Some((g, g))
                }
                None => Some((g, g)),
            };
        }
    }
    if let Some(r) = run {
        runs.push(r);
    }
    for (s, e) in runs {
        let xa = (s * cell) as f64;
        let xb = ((e + 1) * cell) as f64;
        geo.contacts.push([xa, xb, geo.at((xa + xb) / 2.0)]);
    }
    Some(geo)
}

/// The product's ground line, as the ground shadow reads it: the
/// opaque bbox and L(x) = y0 + slope (x - x0), full-resolution pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroundLine {
    pub bbox: (u32, u32, u32, u32),
    pub x0: f64,
    pub y0: f64,
    pub slope: f64,
}

impl GroundLine {
    pub fn at(&self, x: f64) -> f64 {
        self.y0 + self.slope * (x - self.x0)
    }
}

/// The ground line of a product layer (None without an opaque pixel).
pub fn ground_line(foreground: &RgbaImage) -> Option<GroundLine> {
    let bbox = crate::composite::opaque_bbox(foreground)?;
    let (fw, fh) = (foreground.width as usize, foreground.height as usize);
    let cell = fw.max(fh).div_ceil(GROUND_GRID_MAX).max(1);
    let geo = ground_geometry(foreground, bbox, cell, fw.div_ceil(cell))?;
    Some(GroundLine {
        bbox,
        x0: geo.x0,
        y0: geo.y0,
        slope: geo.slope,
    })
}

/// Draw the contact and cast shadow onto the plate (same frame as the
/// foreground), plate pixels only.
pub fn ground_shadow(plate: &RgbaImage, foreground: &RgbaImage, light: &GroundLight) -> GroundOutput {
    let unchanged = |reason: &str| GroundOutput {
        plate: plate.clone(),
        report: GroundReport::skipped(reason),
    };
    if (plate.width, plate.height) != (foreground.width, foreground.height) {
        return unchanged("plate and product layer frames differ");
    }
    let subject = crate::staging::analyze_subject(foreground);
    let Some(bbox) = subject.bbox else {
        return unchanged("the product layer has no opaque pixel");
    };
    if subject.fills_frame {
        return unchanged("subject is a surface or fills the frame; no ground to shade");
    }
    if subject.touches.contains(&"bottom") {
        return unchanged("the product touches the bottom edge of the frame; its ground is not in the picture");
    }
    let (fw, fh) = (foreground.width as usize, foreground.height as usize);
    let cell = fw.max(fh).div_ceil(GROUND_GRID_MAX).max(1);
    let (gw, gh) = (fw.div_ceil(cell), fh.div_ceil(cell));
    let Some(geo) = ground_geometry(foreground, bbox, cell, gw) else {
        return unchanged("no product column reaches a ground line");
    };
    let (side, source) = light.effective();
    let falls = side.shadow_side();
    let (ax, ay) = projection_for(falls);
    let height_px = (bbox.3 - bbox.1) as f64;
    let hg = height_px / cell as f64; // product height in cells

    // Alpha coverage per cell over the product bbox.
    let (gx0, gy0) = (bbox.0 as usize / cell, bbox.1 as usize / cell);
    let (gx1, gy1) = ((bbox.2 as usize).div_ceil(cell), (bbox.3 as usize).div_ceil(cell));
    let mut cover = vec![0f32; gw * gh];
    for gy in gy0..gy1 {
        let ys = gy * cell..((gy + 1) * cell).min(fh);
        for gx in gx0..gx1 {
            let xs = gx * cell..((gx + 1) * cell).min(fw);
            let mut sum = 0u32;
            let mut n = 0u32;
            for y in ys.clone() {
                let row = y * fw;
                for x in xs.clone() {
                    sum += foreground.pixels[(row + x) * 4 + 3] as u32;
                    n += 1;
                }
            }
            cover[gy * gw + gx] = sum as f32 / (255.0 * n.max(1) as f32);
        }
    }

    // Cast shadow, per height band.
    let line_slope = geo.slope;
    let det = (ay.abs() + ax * line_slope).abs().max(0.03) as f32;
    let mut bands: Vec<Vec<f32>> = vec![vec![0f32; gw * gh]; CAST_BANDS.len()];
    for gy in gy0..gy1 {
        for gx in gx0..gx1 {
            let a = cover[gy * gw + gx];
            if a <= 0.0 {
                continue;
            }
            let xc = (gx as f64 + 0.5) * cell as f64;
            let yc = (gy as f64 + 0.5) * cell as f64;
            let ground = geo.at(xc);
            let h = (ground - yc).max(0.0);
            let rel = h / height_px.max(1.0);
            let band = CAST_BANDS.iter().position(|b| rel < b.0).unwrap_or(CAST_BANDS.len() - 1);
            let sx = xc + ax * h;
            let sy = geo.at(sx) + ay * h;
            splat(&mut bands[band], gw, gh, sx / cell as f64, sy / cell as f64, a);
        }
    }
    let mut cast_light = vec![1f32; gw * gh];
    let mut cast_blur_px = Vec::new();
    for (band, (_, blur, weight)) in bands.iter_mut().zip(CAST_BANDS.iter()) {
        for v in band.iter_mut() {
            *v = (*v * det).min(1.0);
        }
        let rx = blur * hg;
        cast_blur_px.push(blur * height_px);
        let blurred = soft_blur(band, gw, gh, rx, rx * CAST_BLUR_Y_SHARE);
        for (l, s) in cast_light.iter_mut().zip(blurred.iter()) {
            *l *= 1.0 - (*weight as f32) * s.min(1.0);
        }
    }

    // Contact shadow: a thin band on the ground line, weighted by the
    // product's clearance above it, blurred along the line.
    let mut contact = vec![0f32; gw * gh];
    let g0 = CONTACT_FALLOFF * height_px;
    // Up the image (behind) and down (toward the camera), in cells.
    let (up, down) = match side.shadow_step().1 {
        -1 => (CONTACT_SIGMA_SHADOW_SIDE, CONTACT_SIGMA_OTHER_SIDE),
        1 => (CONTACT_SIGMA_OTHER_SIDE, CONTACT_SIGMA_SHADOW_SIDE),
        _ => {
            let mid = (CONTACT_SIGMA_SHADOW_SIDE + CONTACT_SIGMA_OTHER_SIDE) / 2.0;
            (mid, mid)
        }
    };
    let (sigma_up, sigma_down) = ((up * hg).max(0.75), (down * hg).max(0.75));
    let reach = (3.0 * sigma_up.max(sigma_down)).ceil() as i64;
    for (g, b) in geo.bottom.iter().enumerate() {
        let Some(b) = b else { continue };
        let xc = (g as f64 + 0.5) * cell as f64;
        let ground = geo.at(xc);
        let gap = (ground - b).max(0.0);
        let wgt = (-gap / g0.max(1.0)).exp();
        if wgt < 0.01 {
            continue;
        }
        let gyc = ground / cell as f64 - 0.5;
        for dy in -reach..=reach {
            let gy = gyc.round() as i64 + dy;
            if gy < 0 || gy as usize >= gh {
                continue;
            }
            let off = gy as f64 - gyc;
            let d = off / if off < 0.0 { sigma_up } else { sigma_down };
            let v = (wgt * (-0.5 * d * d).exp()) as f32;
            let i = gy as usize * gw + g;
            contact[i] = contact[i].max(v);
        }
    }
    let contact_rx = CONTACT_BLUR_X * hg;
    let contact = soft_blur(&contact, gw, gh, contact_rx, 0.0);

    // Grid multiplier, then the bounding box of anything it darkens.
    let mut mult = vec![1f32; gw * gh];
    let (mut mx0, mut my0, mut mx1, mut my1) = (gw, gh, 0usize, 0usize);
    let mut max_dark = 0f64;
    for i in 0..gw * gh {
        let m = (1.0 - CAST_OPACITY as f32 * (1.0 - cast_light[i]))
            * (1.0 - CONTACT_OPACITY as f32 * contact[i].min(1.0));
        if m < 0.999 {
            let (x, y) = (i % gw, i / gw);
            mx0 = mx0.min(x);
            my0 = my0.min(y);
            mx1 = mx1.max(x + 1);
            my1 = my1.max(y + 1);
            max_dark = max_dark.max(1.0 - m as f64);
        }
        mult[i] = m;
    }
    let mut out = plate.clone();
    let mut shadow_pixels = 0u64;
    if mx1 > mx0 {
        // Bilinear between cell centres, one cell of margin.
        let px0 = (mx0.saturating_sub(1)) * cell;
        let py0 = (my0.saturating_sub(1)) * cell;
        let px1 = ((mx1 + 1) * cell).min(fw);
        let py1 = ((my1 + 1) * cell).min(fh);
        let at = |gx: i64, gy: i64| -> f32 {
            let gx = gx.clamp(0, gw as i64 - 1) as usize;
            let gy = gy.clamp(0, gh as i64 - 1) as usize;
            mult[gy * gw + gx]
        };
        for y in py0..py1 {
            let fy = (y as f64 + 0.5) / cell as f64 - 0.5;
            let y0 = fy.floor();
            let ty = (fy - y0) as f32;
            for x in px0..px1 {
                let i = (y * fw + x) * 4;
                if foreground.pixels[i + 3] == OPAQUE_ALPHA_THRESHOLD {
                    continue;
                }
                let fx = (x as f64 + 0.5) / cell as f64 - 0.5;
                let x0 = fx.floor();
                let tx = (fx - x0) as f32;
                let (ix, iy) = (x0 as i64, y0 as i64);
                let m = at(ix, iy) * (1.0 - tx) * (1.0 - ty)
                    + at(ix + 1, iy) * tx * (1.0 - ty)
                    + at(ix, iy + 1) * (1.0 - tx) * ty
                    + at(ix + 1, iy + 1) * tx * ty;
                if m >= 0.999 {
                    continue;
                }
                let mut changed = false;
                for c in 0..3 {
                    let v = plate.pixels[i + c];
                    let nv = (v as f32 * m).round().clamp(0.0, 255.0) as u8;
                    if nv != v {
                        out.pixels[i + c] = nv;
                        changed = true;
                    }
                }
                if changed {
                    shadow_pixels += 1;
                }
            }
        }
    }
    if shadow_pixels == 0 {
        return unchanged("the ground shadow changed no plate pixel");
    }
    let report = GroundReport {
        applied: true,
        reason: None,
        light_side: light.side.map(|s| s.bucket().to_string()),
        light_method: Some(light.method.clone()),
        direction_source: Some(source.to_string()),
        shadow_falls: Some(falls.to_string()),
        ground_line: Some(geo.line),
        contacts: geo.contacts.clone(),
        grid_cell_px: cell as u32,
        product_height_px: height_px as u32,
        cast_blur_px,
        contact_blur_px: CONTACT_BLUR_X * height_px,
        shadow_pixels,
        max_darkening: max_dark,
        plate_pixel_sha256: Some(out.pixel_sha256()),
    };
    GroundOutput { plate: out, report }
}
