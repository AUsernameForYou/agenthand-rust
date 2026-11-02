//! The Fidelity Lock engine — port of the Python `composite.py`.
//!
//! Invariants, enforced the same way:
//! - the product layer is the stored foreground asset, loaded after an
//!   integrity check; never scaled, re-encoded through a model, or
//!   regenerated;
//! - the plate is resized/cropped to the product's dimensions, never
//!   the reverse (`ImageOps.fit`, byte-exact via `resample.rs`);
//! - compositing is Pillow's `alpha_composite` integer math, ported
//!   from `AlphaComposite.c`;
//! - the receipt's counts are measured on the delivered file decoded
//!   back from the store, and the colour profile identity is pinned
//!   alongside the pixel hashes.

use crate::assets::{
    png_text_of_bytes, AssetRecord, AssetStore, HANDLE_PREFIX, KIND_COMPOSITE, KIND_FOREGROUND,
    KIND_MASK, KIND_PLATE, KIND_SOURCE, MASK_PROVENANCE_PNG_KEY, PROVENANCE_PNG_KEY,
};
use crate::image::{describe_color_profile, GrayImage, RgbaImage};
use serde_json::{json, Map, Value};

/// A foreground pixel counts as opaque product only at this alpha
/// (255: fully opaque), in receipts and fidelity checks alike.
pub const OPAQUE_ALPHA_THRESHOLD: u8 = 255;

/// Receipt schema marker, stamped by `draw_product` (the Python
/// reference's `RECEIPT_SCHEMA_VERSION`). Receipts without it are
/// the historical v1 shape (the frozen 2026-10-26 replay package
/// is the live example) and are graded under the compatibility
/// policy in `receipt_schema_problems`.
pub const RECEIPT_SCHEMA_VERSION: i64 = 2;

/// Receipt schemas this verifier grades (Item 11, cross-build
/// verification). This build still ISSUES schema 2; it VERIFIES:
///
/// - 2: this build's receipts (and the Python build before paper-fill);
/// - 3: the Python build's paper-fill receipts (python `main`,
///   ff72fcb): schema 2 plus a REQUIRED `reconstruction` block. Graded
///   on the shared core plus the schema-3 checks: the block's shape
///   and arithmetic, the paper layer / region map hashes, the paper
///   recount with zero overlap under any foreground alpha, the region
///   map partition, foreground authenticity against the source photo,
///   and the whole-delivery recomposition (plate, then paper, then
///   product, rebuilt from the stored layers);
/// - 4: ASSUMED to be schema 3 plus additive fields only, graded
///   exactly as schema 3 (unknown extra fields are ignored, as they
///   are in every schema).
///
/// TODO(schema-4): no real schema-4 receipt has been seen by this
/// build. A real sample must confirm that (a) every schema-3 field
/// is still present with the same meaning and type, (b) the
/// `reconstruction` block keeps its keys and arithmetic (overlap 0,
/// regions summing to reconstruction_pixels, the fraction formula),
/// (c) the recomposition order is still plate, paper, product with
/// no new layer, and (d) no new field changes how an existing check
/// must be graded. If any of those fails, schema 4 needs its own
/// branch here instead of riding on schema 3.
///
/// Any other marker (and a missing marker on a receipt carrying
/// current-schema fields) is refused, as before.
pub const VERIFIABLE_RECEIPT_SCHEMAS: [i64; 3] = [2, 3, 4];

/// Python `paperfill.REGION_CLASS_CODES`: the region map's pixel code
/// per region class (0 = no paper).
const REGION_CLASS_CODES: [(&str, u8); 3] = [("hole", 1), ("support", 2), ("occluder", 3)];

/// The reconstruction block's required keys (Python
/// `_RECONSTRUCTION_BLOCK_KEYS`). Extra keys are tolerated.
const RECONSTRUCTION_BLOCK_KEYS: [&str; 16] = [
    "status",
    "renderer",
    "layer_asset",
    "layer_sha256",
    "layer_pixel_sha256",
    "region_map_asset",
    "region_map_sha256",
    "region_map_pixel_sha256",
    "reconstruction_pixels",
    "reconstruction_overlap_product_pixels",
    "reconstruction_fraction",
    "guard_ring_px",
    "motif",
    "regions",
    "refusals",
    "unfilled_gap_pixels",
];

/// Pillow's fast divide-by-255: `(((a >> 8) + a) >> 8)`.
fn shift_for_div_255(a: u32) -> u32 {
    ((a >> 8) + a) >> 8
}

/// `Image.alpha_composite(dst, src)` — src over dst, Pillow's integer
/// implementation (PRECISION_BITS = 7) ported exactly.
pub fn alpha_composite(dst: &RgbaImage, src: &RgbaImage) -> RgbaImage {
    assert_eq!(dst.width, src.width);
    assert_eq!(dst.height, src.height);
    let mut out = vec![0u8; dst.pixels.len()];
    for i in (0..dst.pixels.len()).step_by(4) {
        let s = [
            src.pixels[i],
            src.pixels[i + 1],
            src.pixels[i + 2],
            src.pixels[i + 3],
        ];
        let d = [
            dst.pixels[i],
            dst.pixels[i + 1],
            dst.pixels[i + 2],
            dst.pixels[i + 3],
        ];
        if s[3] == 0 {
            out[i..i + 4].copy_from_slice(&d);
            continue;
        }
        let sa = s[3] as u32;
        let blend = d[3] as u32 * (255 - sa);
        let outa255 = sa * 255 + blend;
        let coef1 = sa * 255 * 255 * (1 << 7) / outa255;
        let coef2 = 255 * (1 << 7) - coef1;
        for c in 0..3 {
            let tmp = s[c] as u32 * coef1 + d[c] as u32 * coef2;
            out[i + c] = (shift_for_div_255(tmp + (0x80 << 7)) >> 7) as u8;
        }
        out[i + 3] = shift_for_div_255(outa255 + 0x80) as u8;
    }
    RgbaImage {
        width: dst.width,
        height: dst.height,
        pixels: out,
        icc_profile: src.icc_profile.clone(),
    }
}

/// The deterministic local placeholder plate (dev/offline only):
/// a vertical gradient whose colours derive from the prompt hash.
pub fn generate_background_plate(prompt: &str, width: u32, height: u32) -> RgbaImage {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(prompt.as_bytes());
    let top = [digest[0], digest[1], digest[2]];
    let bottom = [digest[3], digest[4], digest[5]];
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        let t = y as f64 / (height.saturating_sub(1)).max(1) as f64;
        let mut color = [0u8; 4];
        for c in 0..3 {
            color[c] = (top[c] as f64 + (bottom[c] as f64 - top[c] as f64) * t) as u8;
        }
        color[3] = 255;
        for x in 0..width {
            let i = ((y * width + x) * 4) as usize;
            pixels[i..i + 4].copy_from_slice(&color);
        }
    }
    RgbaImage::new(width, height, pixels)
}

/// A soft elliptical contact shadow on a transparent canvas: colour
/// (58, 50, 40), alpha 110 at the centre falling linearly to 0 at
/// 1.5 radii, forced to 0 wherever `opaque(x, y)` (a fully opaque
/// product pixel). Shared by the priced-table thumbnails and the
/// source-derived staging plates; only the ellipse's 1.5-radius box
/// is visited (every pixel outside it is 0 either way).
pub fn elliptical_contact_shadow(
    width: u32,
    height: u32,
    center: (f64, f64),
    radii: (f64, f64),
    opaque: impl Fn(u32, u32) -> bool,
) -> RgbaImage {
    let (cx, cy) = center;
    let (rx, ry) = radii;
    let mut canvas = RgbaImage::solid(width, height, [0, 0, 0, 0]);
    let span = |c: f64, r: f64, limit: u32| -> (u32, u32) {
        let lo = (c - 1.5 * r).floor().max(0.0).min(limit as f64) as u32;
        let hi = (c + 1.5 * r).ceil().max(0.0).min(limit as f64) as u32;
        (lo, hi.max(lo))
    };
    let (x0, x1) = span(cx, rx, width);
    let (y0, y1) = span(cy, ry, height);
    for y in y0..y1 {
        for x in x0..x1 {
            let d = (((x as f64 - cx) / rx).powi(2) + ((y as f64 - cy) / ry).powi(2)).sqrt();
            if d >= 1.5 {
                continue;
            }
            let mut alpha = (110.0 * (1.5 - d) / 1.5) as u8;
            if opaque(x, y) {
                alpha = 0;
            }
            if alpha > 0 {
                let i = ((y * width + x) * 4) as usize;
                canvas.pixels[i..i + 4].copy_from_slice(&[58, 50, 40, alpha]);
            }
        }
    }
    canvas
}

/// Bounding box (left, top, right, bottom; half-open) of the fully
/// opaque product pixels, or None when there are none.
pub fn opaque_bbox(foreground: &RgbaImage) -> Option<(u32, u32, u32, u32)> {
    let (w, h) = (foreground.width, foreground.height);
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    for y in 0..h {
        for x in 0..w {
            if foreground.pixels[((y * w + x) * 4 + 3) as usize] == OPAQUE_ALPHA_THRESHOLD {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x + 1);
                y1 = y1.max(y + 1);
            }
        }
    }
    (x0 != u32::MAX).then_some((x0, y0, x1, y1))
}

/// Draw the product's contact shadow onto a plate at the product's
/// own position (plate and foreground share the source frame): the
/// thumbnail ellipse, centred on the opaque bbox's bottom edge,
/// 0.88 x its width, 0.055 x its height tall (at least 5 px). Plate
/// pixels only; product pixels are composited over it untouched.
pub fn apply_contact_shadow(plate: &RgbaImage, foreground: &RgbaImage) -> RgbaImage {
    assert_eq!(
        (plate.width, plate.height),
        (foreground.width, foreground.height)
    );
    let Some((x0, y0, x1, y1)) = opaque_bbox(foreground) else {
        return plate.clone();
    };
    let (item_w, item_h) = ((x1 - x0) as f64, (y1 - y0) as f64);
    let w = foreground.width;
    let shadow = elliptical_contact_shadow(
        plate.width,
        plate.height,
        ((x0 as f64 + x1 as f64) / 2.0, y1 as f64),
        (item_w * 0.44, (item_h * 0.055).max(5.0)),
        |x, y| foreground.pixels[((y * w + x) * 4 + 3) as usize] == OPAQUE_ALPHA_THRESHOLD,
    );
    let mut out = alpha_composite(plate, &shadow);
    out.icc_profile = plate.icc_profile.clone();
    out
}

/// Square (Chebyshev) dilation of a 0/1 plane by `r`, separable.
pub fn dilate_square(plane: &[u8], width: usize, height: usize, r: usize) -> Vec<u8> {
    if r == 0 {
        return plane.to_vec();
    }
    let mut horizontal = vec![0u8; plane.len()];
    for y in 0..height {
        let row = &plane[y * width..(y + 1) * width];
        for x in 0..width {
            let lo = x.saturating_sub(r);
            let hi = (x + r + 1).min(width);
            horizontal[y * width + x] = u8::from(row[lo..hi].iter().any(|&v| v != 0));
        }
    }
    let mut out = vec![0u8; plane.len()];
    for x in 0..width {
        for y in 0..height {
            let lo = y.saturating_sub(r);
            let hi = (y + r + 1).min(height);
            out[y * width + x] = u8::from((lo..hi).any(|yy| horizontal[yy * width + x] != 0));
        }
    }
    out
}

/// Plate-side edge matte: the `matte_px` (1 or 2) rings of plate
/// pixels around the opaque product take a share of the local product
/// colour (ring 1: 50%, ring 2: 25%; mean of the opaque pixels in the
/// ring pixel's (2d+1)^2 window), so the hard SAM edge reads as a
/// photographed edge instead of a paste. Nothing is eroded and no
/// product pixel is written: the rings lie strictly outside the
/// opaque mask, so the Fidelity Lock count is untouched. Callers cap
/// `matte_px` at 1 for thin structures (pickets, spokes) so a thin
/// part is not haloed to several times its width.
pub fn apply_edge_matte(plate: &RgbaImage, foreground: &RgbaImage, matte_px: u32) -> RgbaImage {
    assert_eq!(
        (plate.width, plate.height),
        (foreground.width, foreground.height)
    );
    let mut out = plate.clone();
    let Some((bx0, by0, bx1, by1)) = opaque_bbox(foreground) else {
        return out;
    };
    let matte = matte_px.min(2) as usize;
    if matte == 0 {
        return out;
    }
    // Work inside the bbox grown by the matte width.
    let (fw, fh) = (foreground.width as usize, foreground.height as usize);
    let x0 = (bx0 as usize).saturating_sub(matte);
    let y0 = (by0 as usize).saturating_sub(matte);
    let x1 = (bx1 as usize + matte).min(fw);
    let y1 = (by1 as usize + matte).min(fh);
    let (w, h) = (x1 - x0, y1 - y0);
    let opaque_at =
        |x: usize, y: usize| foreground.pixels[(y * fw + x) * 4 + 3] == OPAQUE_ALPHA_THRESHOLD;
    let mut plane = vec![0u8; w * h];
    for y in 0..h {
        for x in 0..w {
            plane[y * w + x] = u8::from(opaque_at(x0 + x, y0 + y));
        }
    }
    let rings: Vec<Vec<u8>> = (1..=matte)
        .map(|r| dilate_square(&plane, w, h, r))
        .collect();
    const SHARE: [u32; 2] = [128, 64]; // 50%, 25% of 256
    for y in 0..h {
        for x in 0..w {
            if plane[y * w + x] != 0 {
                continue;
            }
            let Some(d) = (0..matte).find(|&k| rings[k][y * w + x] != 0) else {
                continue;
            };
            let r = d + 1;
            let (gx, gy) = (x0 + x, y0 + y);
            let mut sum = [0u32; 3];
            let mut n = 0u32;
            for yy in gy.saturating_sub(r)..(gy + r + 1).min(fh) {
                for xx in gx.saturating_sub(r)..(gx + r + 1).min(fw) {
                    if opaque_at(xx, yy) {
                        let j = (yy * fw + xx) * 4;
                        for c in 0..3 {
                            sum[c] += foreground.pixels[j + c] as u32;
                        }
                        n += 1;
                    }
                }
            }
            if n == 0 {
                continue;
            }
            let share = SHARE[d];
            let j = (gy * fw + gx) * 4;
            for c in 0..3 {
                let product = (sum[c] + n / 2) / n;
                let base = out.pixels[j + c] as u32;
                out.pixels[j + c] = ((base * (256 - share) + product * share + 128) >> 8) as u8;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Edge fix (docs/EDGE-FIX.md): kill the cut-out rim on source-derived
// plates. Three steps, none of which writes a product pixel (alpha 255):
// 1. decontaminate the layer's soft edge band against the ORIGINAL
//    background (`segmentation::decontaminate_edge_band`);
// 2. rebuild the plate's 2-3 px band hugging the product from the plate
//    just beyond it (wiping any plate-side rim, including the old
//    contaminated matte ring) and darken it softly with the contact
//    shadow's colour (a contact band);
// 3. feather the boundary by one outward ring of local product colour
//    at <= 50% (about 1 px), never softer than the source photo's own
//    edge, measured from the source.
// The receipt's pixel set and count are unchanged; the receipt carries
// an `edge_fix` record with every parameter and count.
// ---------------------------------------------------------------------------

use crate::segmentation::{decontaminate_edge_band, EdgeField};

/// Kill switch: `AGENTHAND_EDGE_FIX=off` (or 0 / false / no) restores
/// the previous composite exactly.
pub const EDGE_FIX_ENV: &str = "AGENTHAND_EDGE_FIX";
/// The `edge_fix` receipt record's version.
pub const EDGE_FIX_VERSION: i64 = 1;
/// Feather: one outward ring, at most this opacity (128 ~ 50%): the
/// composite boundary ramp spans at most 2 px, i.e. about 1 px of
/// feather beyond the hard cut.
pub const FEATHER_MAX_ALPHA: u8 = 128;
/// Source edge-width samples: a boundary profile needs at least this
/// luma step across it to count.
pub const EDGE_WIDTH_MIN_CONTRAST: u32 = 32;
/// Fewer qualifying samples than this and the source edge is treated
/// as hard (no feather): the cap never assumes softness it cannot see.
pub const EDGE_WIDTH_MIN_SAMPLES: usize = 64;
/// Upper clamp of one sample's ramp width (x256 fixed point, 5 px).
pub const EDGE_WIDTH_MAX_FX: u32 = 5 * 256;
/// A source ramp up to 1.125 px counts as pixel-hard (no feather): one
/// luma level of noise on a 32-level step reads as ~1.03 px.
pub const EDGE_WIDTH_HARD_FX: u32 = 288;
/// Contact band: the contact shadow's colour, alpha per ring (ring 1
/// hugs the product). Thin subjects get the 2-ring band.
pub const CONTACT_BAND_RGB: [u8; 3] = [58, 50, 40];
pub const CONTACT_BAND_ALPHA: [u8; 3] = [30, 18, 8];
pub const CONTACT_BAND_ALPHA_THIN: [u8; 2] = [18, 8];
/// Plate rim rebuild: share (of 256) of the clean plate colour (mean of
/// the plate `band+1 ..= band+3` px out) that replaces each band ring.
pub const RIM_REBUILD: [u32; 3] = [256, 256, 160];
pub const RIM_REBUILD_THIN: [u32; 2] = [256, 160];

/// Whether the edge fix runs at all (the kill switch).
pub fn edge_fix_enabled() -> bool {
    !matches!(
        std::env::var(EDGE_FIX_ENV)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "off" | "false" | "no"
    )
}

/// The plate kinds the edge fix applies to: the source-derived plates
/// (the product sits back in its own photo's geometry). Legacy,
/// placeholder and code-rendered draws keep their exact composite.
pub fn edge_fix_applies_to(plate_kind: Option<&str>) -> bool {
    matches!(
        plate_kind,
        Some(k) if k == crate::backdrop::PLATE_KIND_SOURCE_EDIT
            || k == crate::backdrop::PLATE_KIND_SOURCE_BLUR
    )
}

/// One pixel of `alpha_composite` (src over dst), the same integer math.
pub fn composite_pixel(d: [u8; 4], s: [u8; 4]) -> [u8; 4] {
    if s[3] == 0 {
        return d;
    }
    let sa = s[3] as u32;
    let blend = d[3] as u32 * (255 - sa);
    let outa255 = sa * 255 + blend;
    let coef1 = sa * 255 * 255 * (1 << 7) / outa255;
    let coef2 = 255 * (1 << 7) - coef1;
    let mut out = [0u8; 4];
    for c in 0..3 {
        let tmp = s[c] as u32 * coef1 + d[c] as u32 * coef2;
        out[c] = (shift_for_div_255(tmp + (0x80 << 7)) >> 7) as u8;
    }
    out[3] = shift_for_div_255(outa255 + 0x80) as u8;
    out
}

/// Pillow's 'L' luma.
#[inline]
fn luma(px: &[u8]) -> u32 {
    (px[0] as u32 * 19595 + px[1] as u32 * 38470 + px[2] as u32 * 7471 + 0x8000) >> 16
}

/// The source photo's own edge softness across the product boundary,
/// as a linear-ramp width (x256 fixed point; 256 = a pixel-hard step),
/// and the number of profiles it was read from. Each product boundary
/// pixel with a non-product 4-neighbour gives one luma profile along
/// that direction, 2 px inside to 3 px outside; a profile counts when
/// its end-to-end step C is at least `EDGE_WIDTH_MIN_CONTRAST` and it
/// is not texture (range <= 1.5 C). Width = C / steepest single step;
/// the median over all profiles is returned. Under
/// `EDGE_WIDTH_MIN_SAMPLES` profiles, 256 (hard) is returned.
pub fn estimate_source_edge_width(source: &RgbaImage, field: &EdgeField) -> (u32, usize) {
    let (fw, fh) = (field.frame_w as i64, field.frame_h as i64);
    let mut widths: Vec<u32> = Vec::new();
    for y in 0..field.h {
        for x in 0..field.w {
            if field.alpha[y * field.w + x] != 255 {
                continue;
            }
            for (dx, dy) in [(1i64, 0i64), (-1, 0), (0, 1), (0, -1)] {
                let (nx, ny) = (x as i64 + dx, y as i64 + dy);
                let neighbour_product = nx >= 0
                    && ny >= 0
                    && (nx as usize) < field.w
                    && (ny as usize) < field.h
                    && field.alpha[ny as usize * field.w + nx as usize] == 255;
                if neighbour_product {
                    continue;
                }
                let (gx, gy) = ((field.x0 + x) as i64, (field.y0 + y) as i64);
                let mut l = [0u32; 6];
                let mut inside = true;
                for (k, slot) in l.iter_mut().enumerate() {
                    let off = k as i64 - 2;
                    let (sx, sy) = (gx + off * dx, gy + off * dy);
                    if sx < 0 || sy < 0 || sx >= fw || sy >= fh {
                        inside = false;
                        break;
                    }
                    *slot = luma(&source.pixels[((sy * fw + sx) * 4) as usize..]);
                }
                if !inside {
                    continue;
                }
                let c = l[0].abs_diff(l[5]);
                if c < EDGE_WIDTH_MIN_CONTRAST {
                    continue;
                }
                let (lo, hi) = (*l.iter().min().unwrap(), *l.iter().max().unwrap());
                if 2 * (hi - lo) > 3 * c {
                    continue;
                }
                let g = l.windows(2).map(|p| p[0].abs_diff(p[1])).max().unwrap_or(0);
                if g == 0 {
                    continue;
                }
                widths.push((c * 256 / g).clamp(256, EDGE_WIDTH_MAX_FX));
            }
        }
    }
    let n = widths.len();
    if n < EDGE_WIDTH_MIN_SAMPLES {
        return (256, n);
    }
    widths.sort_unstable();
    (widths[n / 2], n)
}

/// Feather ring opacity for a source ramp width (x256): the composite
/// profile product | ring a | plate has ramp width 1 / (1 - a) for
/// a <= 0.5, so a = 1 - 1/w matches the source exactly and never
/// exceeds it; capped at `FEATHER_MAX_ALPHA` (about 1 px). A
/// pixel-hard source edge (<= `EDGE_WIDTH_HARD_FX`) gets no feather.
pub fn feather_alpha_for_width(width_fx: u32) -> u8 {
    if width_fx <= EDGE_WIDTH_HARD_FX {
        return 0;
    }
    let inv = (255 * 256 + width_fx / 2) / width_fx;
    (255u32.saturating_sub(inv)).min(FEATHER_MAX_ALPHA as u32) as u8
}

/// What the edge fix did; `to_json` is the receipt's `edge_fix` record.
#[derive(Debug, Clone, Default)]
pub struct EdgeFixReport {
    pub applied: bool,
    pub reason: Option<String>,
    pub thin_structure: bool,
    pub source_edge_width_fx: u32,
    pub source_edge_samples: usize,
    pub feather_alpha: u8,
    pub feather_pixels: u64,
    pub edge_band_pixels: u64,
    pub decontaminated_pixels: u64,
    pub contact_band_width_px: u32,
    pub contact_band_pixels: u64,
    pub rim_rebuilt_pixels: u64,
}

impl EdgeFixReport {
    fn skipped(reason: &str) -> Self {
        EdgeFixReport {
            applied: false,
            reason: Some(reason.to_string()),
            ..Default::default()
        }
    }

    pub fn to_json(&self) -> Value {
        if !self.applied {
            return json!({
                "version": EDGE_FIX_VERSION,
                "applied": false,
                "reason": self.reason,
            });
        }
        let band = self.contact_band_width_px as u8;
        let (alphas, rebuild): (Vec<u8>, Vec<u32>) = if self.thin_structure {
            (CONTACT_BAND_ALPHA_THIN.to_vec(), RIM_REBUILD_THIN.to_vec())
        } else {
            (CONTACT_BAND_ALPHA.to_vec(), RIM_REBUILD.to_vec())
        };
        json!({
            "version": EDGE_FIX_VERSION,
            "applied": true,
            "stage": "composite",
            "product_pixels_written": 0,
            "edge_band": {
                "pixels": self.edge_band_pixels,
                "decontaminated": self.decontaminated_pixels,
                "method": "un-blend against the local original background: F = (observed - (1-a) B) / a, B = mean source colour 2-4 px outside the footprint, leaning on the local product colour as a -> 0",
                "background_ring_px": [crate::segmentation::BACKGROUND_RING.0, crate::segmentation::BACKGROUND_RING.1],
            },
            "feather": {
                "rings": 1,
                "alpha": self.feather_alpha,
                "max_alpha": FEATHER_MAX_ALPHA,
                "pixels": self.feather_pixels,
                "colour": "local product colour (product pixels 3-5 px deep), never the original background",
            },
            "source_edge": {
                "ramp_width_px": crate::round6(self.source_edge_width_fx as f64 / 256.0),
                "samples": self.source_edge_samples,
                "min_contrast": EDGE_WIDTH_MIN_CONTRAST,
                "rule": "feather alpha = min(max_alpha, 1 - 1/ramp_width): never softer than the source edge",
            },
            "contact_band": {
                "width_px": self.contact_band_width_px,
                "rgb": CONTACT_BAND_RGB,
                "alpha": alphas,
                "rim_rebuild_share_256": rebuild,
                "rebuilt_from_ring_px": [band + 1, band + 3],
                "pixels": self.contact_band_pixels,
                "rim_rebuilt_pixels": self.rim_rebuilt_pixels,
            },
            "thin_structure": self.thin_structure,
        })
    }
}

/// The edge fix's outputs: the dressed plate (contact band, rim rebuild
/// and feather ring; plate-side pixels only) and the product layer with
/// its soft edge band decontaminated (product pixels copied unchanged).
pub struct EdgeFixOutput {
    pub plate: RgbaImage,
    pub foreground: RgbaImage,
    pub report: EdgeFixReport,
}

/// Run the edge fix. `source` is the original photo the layer was cut
/// from, `plate` is already at the layer's frame. `thin` (the
/// subject's thin-structure flag) narrows the contact band to 2 px.
pub fn apply_edge_fix(
    source: &RgbaImage,
    foreground: &RgbaImage,
    plate: &RgbaImage,
    thin: bool,
) -> EdgeFixOutput {
    let unchanged = |reason: &str| EdgeFixOutput {
        plate: plate.clone(),
        foreground: foreground.clone(),
        report: EdgeFixReport::skipped(reason),
    };
    if (source.width, source.height) != (foreground.width, foreground.height)
        || (plate.width, plate.height) != (foreground.width, foreground.height)
    {
        return unchanged("source, plate and product layer frames differ");
    }
    let Some(field) = EdgeField::new(foreground) else {
        return unchanged("the product layer has no visible pixel");
    };
    let (dressed_fg, edge_band_pixels, decontaminated_pixels) =
        decontaminate_edge_band(source, foreground, Some(&field));
    let (width_fx, samples) = estimate_source_edge_width(source, &field);
    let feather = feather_alpha_for_width(width_fx);

    let (alphas, rebuild): (&[u8], &[u32]) = if thin {
        (&CONTACT_BAND_ALPHA_THIN, &RIM_REBUILD_THIN)
    } else {
        (&CONTACT_BAND_ALPHA, &RIM_REBUILD)
    };
    let band = alphas.len() as u8;
    let mut out = plate.clone();
    let (mut band_px, mut rebuilt) = (0u64, 0u64);
    let (clean_lo, clean_hi) = (band + 1, band + 3);
    for y in 0..field.h {
        for x in 0..field.w {
            let i = y * field.w + x;
            let d = field.out_o[i];
            if d == 0 || d > band {
                continue;
            }
            band_px += 1;
            let j = field.frame_offset(x, y);
            let mut px = [
                plate.pixels[j],
                plate.pixels[j + 1],
                plate.pixels[j + 2],
                plate.pixels[j + 3],
            ];
            // Rim rebuild: read from the untouched plate, so the result
            // does not depend on visiting order.
            if let Some(clean) = field.local_mean(plate, x, y, clean_hi as usize, |k| {
                field.out_o[k] >= clean_lo && field.out_o[k] <= clean_hi
            }) {
                let s = rebuild[(d - 1) as usize];
                for c in 0..3 {
                    px[c] = ((px[c] as u32 * (256 - s) + clean[c] as u32 * s + 128) >> 8) as u8;
                }
                rebuilt += 1;
            }
            // Contact band: the contact shadow's colour over the ring,
            // per channel never brighter than the plate (on a plate
            // darker than the shadow colour it is a no-op, not a glow).
            let [r, g, b] = CONTACT_BAND_RGB;
            let shaded = composite_pixel(px, [r, g, b, alphas[(d - 1) as usize]]);
            for c in 0..3 {
                px[c] = px[c].min(shaded[c]);
            }
            out.pixels[j..j + 4].copy_from_slice(&px);
        }
    }

    let mut feather_px = 0u64;
    if feather > 0 {
        for y in 0..field.h {
            for x in 0..field.w {
                let i = y * field.w + x;
                if field.alpha[i] != 0 || field.out_o[i] != 1 {
                    continue;
                }
                // Coverage-weighted: a straight edge (3 product px in
                // the 3x3) gets the full ring alpha, a staircase corner
                // touched only diagonally a third, which rounds SAM's
                // steps instead of outlining them.
                let mut k = 0u32;
                for yy in y.saturating_sub(1)..(y + 2).min(field.h) {
                    for xx in x.saturating_sub(1)..(x + 2).min(field.w) {
                        k += u32::from(field.alpha[yy * field.w + xx] == 255);
                    }
                }
                let a = ((feather as u32 * k.min(3) + 1) / 3) as u8;
                if a == 0 {
                    continue;
                }
                let Some(colour) = field.product_colour(foreground, x, y) else {
                    continue;
                };
                let j = field.frame_offset(x, y);
                let d = [
                    out.pixels[j],
                    out.pixels[j + 1],
                    out.pixels[j + 2],
                    out.pixels[j + 3],
                ];
                let px = composite_pixel(d, [colour[0], colour[1], colour[2], a]);
                out.pixels[j..j + 4].copy_from_slice(&px);
                feather_px += 1;
            }
        }
    }

    EdgeFixOutput {
        plate: out,
        foreground: dressed_fg,
        report: EdgeFixReport {
            applied: true,
            reason: None,
            thin_structure: thin,
            source_edge_width_fx: width_fx,
            source_edge_samples: samples,
            feather_alpha: feather,
            feather_pixels: feather_px,
            edge_band_pixels,
            decontaminated_pixels,
            contact_band_width_px: band as u32,
            contact_band_pixels: band_px,
            rim_rebuilt_pixels: rebuilt,
        },
    }
}

/// `composite_images` with the edge fix: the plate is fitted first
/// (exactly as `composite_images` does), dressed, and the decontaminated
/// layer composited over it. Counts are measured against the STORED
/// layer, so the receipt's product set and count are the same as
/// without the fix.
pub fn composite_images_with_edge_fix(
    foreground: &RgbaImage,
    plate: &RgbaImage,
    source: &RgbaImage,
    thin: bool,
) -> (RgbaImage, Measure, Vec<String>, EdgeFixReport) {
    let (fitted, transformations) = fit_plate(plate, foreground.width, foreground.height);
    let fix = apply_edge_fix(source, foreground, &fitted, thin);
    let mut out = alpha_composite(&fix.plate, &fix.foreground);
    out.icc_profile = foreground.icc_profile.clone();
    let counts = measure(foreground, &out);
    (out, counts, transformations, fix.report)
}

/// Resize/crop the PLATE to the product's dimensions. Never the reverse.
fn fit_plate(plate: &RgbaImage, width: u32, height: u32) -> (RgbaImage, Vec<String>) {
    if plate.width == width && plate.height == height {
        return (plate.clone(), Vec::new());
    }
    let fitted = crate::resample::fit(plate, width, height);
    (
        fitted,
        vec![format!(
            "plate_resized_and_center_cropped_to_{}x{}",
            width, height
        )],
    )
}

/// Manhattan distance from every pixel to the nearest target pixel
/// (two-pass chamfer transform). When no pixel matches the target,
/// distance is measured to the image border instead.
fn distance_to_class(alpha: &[u8], width: usize, height: usize, target: u8) -> Vec<i64> {
    let total = width * height;
    if !alpha.contains(&target) {
        let mut out = vec![0i64; total];
        for y in 0..height {
            for x in 0..width {
                out[y * width + x] = ((x + 1).min(width - x).min(y + 1).min(height - y)) as i64;
            }
        }
        return out;
    }
    let inf = total as i64 + 1;
    let mut dist: Vec<i64> = alpha
        .iter()
        .map(|&a| if a == target { 0 } else { inf })
        .collect();
    for y in 0..height {
        let base = y * width;
        for x in 0..width {
            let i = base + x;
            if dist[i] != 0 {
                if x > 0 && dist[i - 1] + 1 < dist[i] {
                    dist[i] = dist[i - 1] + 1;
                }
                if y > 0 && dist[i - width] + 1 < dist[i] {
                    dist[i] = dist[i - width] + 1;
                }
            }
        }
    }
    for y in (0..height).rev() {
        let base = y * width;
        for x in (0..width).rev() {
            let i = base + x;
            if dist[i] != 0 {
                if x < width - 1 && dist[i + 1] + 1 < dist[i] {
                    dist[i] = dist[i + 1] + 1;
                }
                if y < height - 1 && dist[i + width] + 1 < dist[i] {
                    dist[i] = dist[i + width] + 1;
                }
            }
        }
    }
    dist
}

/// The edge band's maximum width: the longest shortest path through
/// band pixels from the opaque region to the transparent region.
fn edge_band_max_width(alpha: &[u8], width: usize, height: usize) -> i64 {
    if width == 0 || height == 0 {
        return 0;
    }
    if !alpha.iter().any(|&a| a > 0 && a < 255) {
        return 0;
    }
    let to_opaque = distance_to_class(alpha, width, height, 255);
    let to_transparent = distance_to_class(alpha, width, height, 0);
    alpha
        .iter()
        .enumerate()
        .filter(|(_, &a)| a > 0 && a < 255)
        .map(|(i, _)| to_opaque[i] + to_transparent[i] - 1)
        .max()
        .unwrap_or(0)
}

use crate::round6;

#[derive(Debug, Clone)]
pub struct Measure {
    pub opaque_pixels: u64,
    pub edge_band_pixels: u64,
    pub transparent_pixels: u64,
    pub changed_opaque_pixels: u64,
    pub mask_pixels_total: u64,
    pub opaque_fraction: f64,
    pub edge_band_max_width_px: i64,
}

/// Count opaque / edge-band / transparent pixels and verify the lock
/// (port of `_measure`).
pub fn measure(foreground: &RgbaImage, composite: &RgbaImage) -> Measure {
    assert_eq!(foreground.width, composite.width);
    assert_eq!(foreground.height, composite.height);
    let alpha = foreground.alpha_plane();
    let mut opaque = 0u64;
    let mut edge_band = 0u64;
    let mut transparent = 0u64;
    let mut changed_opaque = 0u64;
    for (i, &a) in alpha.iter().enumerate() {
        if a == OPAQUE_ALPHA_THRESHOLD {
            opaque += 1;
            let j = i * 4;
            if foreground.pixels[j..j + 4] != composite.pixels[j..j + 4] {
                changed_opaque += 1;
            }
        } else if a > 0 {
            edge_band += 1;
        } else {
            transparent += 1;
        }
    }
    let mask_total = opaque + edge_band;
    Measure {
        opaque_pixels: opaque,
        edge_band_pixels: edge_band,
        transparent_pixels: transparent,
        changed_opaque_pixels: changed_opaque,
        mask_pixels_total: mask_total,
        opaque_fraction: if mask_total > 0 {
            round6(opaque as f64 / mask_total as f64)
        } else {
            0.0
        },
        edge_band_max_width_px: edge_band_max_width(
            &alpha,
            foreground.width as usize,
            foreground.height as usize,
        ),
    }
}

/// Composite a foreground over a plate; (image, counts, transforms).
/// The composite carries the FOREGROUND's ICC profile (Pillow's
/// alpha_composite drops it; the Python pipeline re-attaches it).
pub fn composite_images(
    foreground: &RgbaImage,
    plate: &RgbaImage,
) -> (RgbaImage, Measure, Vec<String>) {
    let (fitted, transformations) = fit_plate(plate, foreground.width, foreground.height);
    let mut out = alpha_composite(&fitted, foreground);
    out.icc_profile = foreground.icc_profile.clone();
    let counts = measure(foreground, &out);
    (out, counts, transformations)
}

/// Check an asset may be composited; returns its lineage chain.
pub fn validate_foreground(
    store: &AssetStore,
    foreground_handle: &str,
) -> Result<Vec<AssetRecord>, String> {
    let record = store.get_record(foreground_handle)?;
    if record.kind != KIND_FOREGROUND {
        return Err(format!(
            "draw requires a {KIND_FOREGROUND} asset (the cut-out product layer); {foreground_handle:?} is kind {:?}. Compositing anything else would break the Fidelity Lock.",
            record.kind
        ));
    }
    let chain = store.lineage(foreground_handle)?;
    let source_record = chain.last().expect("lineage is non-empty");
    if source_record.kind != KIND_SOURCE {
        return Err(format!(
            "Foreground {foreground_handle:?} traces back to kind {:?}, not a {KIND_SOURCE}. Its provenance cannot be verified, so it cannot be composited.",
            source_record.kind
        ));
    }
    store.verify_integrity(foreground_handle)?;
    if source_record.asset_id != record.asset_id {
        store.verify_integrity(&(HANDLE_PREFIX.to_string() + &source_record.asset_id))?;
    }
    Ok(chain)
}

/// The mask scale methods cut_out can record
/// (`segmentation::MASK_SCALE_*`; the strings are receipt wire
/// values, pinned here like "asset" / "foreground_alpha" are for
/// mask_source).
pub const MASK_SCALE_METHODS: [&str; 2] = ["none", "bilinear_threshold_128"];

/// Optional current-schema receipt fields: mask provenance
/// (decisions 68/69). Receipts carry them when the mask asset
/// recorded provenance (every cut_out since the SAM scaling change
/// does); older receipts and foreground_alpha receipts do not.
/// Absence is accepted; presence is validated as a complete,
/// self-consistent set (see `receipt_schema_problems`).
pub const RECEIPT_FIELDS_V2_OPTIONAL: [&str; 5] = [
    "mask_source_width",
    "mask_source_height",
    "mask_sam_width",
    "mask_sam_height",
    "mask_scale_method",
];

/// A receipt count field's shape: a non-negative integer (the
/// Python verifier's `_is_count`; JSON bools and floats are not
/// integers here).
fn is_count(v: &Value) -> bool {
    v.as_u64().is_some()
}

/// The mask asset's recorded provenance, or None when absent.
///
/// cut_out writes the provenance (source frame, SAM frame, scale
/// method) as a PNG text chunk inside the mask file; draw republishes
/// it in the receipt as mask_source_width / mask_source_height /
/// mask_sam_width / mask_sam_height / mask_scale_method. Anything
/// missing or malformed reads as absent: provenance is evidence, not
/// a gate, and a mask written before it existed must still draw — the
/// receipt simply carries no provenance fields, which the verifier
/// accepts.
pub fn mask_provenance_of(store: &AssetStore, mask_handle: &str) -> Option<Value> {
    let bytes = store.read_bytes(mask_handle).ok()?;
    let raw = png_text_of_bytes(&bytes)
        .get(MASK_PROVENANCE_PNG_KEY)?
        .clone();
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    let obj = parsed.as_object()?;
    let mut provenance = Map::new();
    for name in ["source_width", "source_height", "sam_width", "sam_height"] {
        let value = obj.get(name)?;
        if !is_count(value) {
            return None;
        }
        provenance.insert(name.to_string(), value.clone());
    }
    let method = obj.get("scale_method")?.as_str()?;
    if !MASK_SCALE_METHODS.contains(&method) {
        return None;
    }
    provenance.insert(
        "scale_method".to_string(),
        Value::String(method.to_string()),
    );
    Some(Value::Object(provenance))
}

/// Composite a stored foreground into a scene; (handle, receipt).
pub fn draw_product(
    store: &AssetStore,
    foreground_handle: &str,
    scene_prompt: &str,
    plate_handle: Option<&str>,
) -> Result<(String, Value), String> {
    draw_product_with_plate_kind(store, foreground_handle, scene_prompt, plate_handle, None)
}

/// `draw_product` with an explicit plate classification for the
/// receipt's `plate_kind` / `backdrop` fields. Without an override
/// the classification follows the plate's origin: a stored plate is
/// `model_generated`, the local dev plate is `local_placeholder`,
/// and both carry a null `backdrop`. Code-rendered backdrop plates
/// pass `("code_rendered", backdrop_json)` so the receipt records
/// which backdrop was rendered and with what parameters.
pub fn draw_product_with_plate_kind(
    store: &AssetStore,
    foreground_handle: &str,
    scene_prompt: &str,
    plate_handle: Option<&str>,
    plate_kind_override: Option<(&str, Value)>,
) -> Result<(String, Value), String> {
    let chain = validate_foreground(store, foreground_handle)?;
    let record = chain[0].clone();
    let source_record = chain[chain.len() - 1].clone();

    let foreground = store.read_image(foreground_handle)?;

    let (mask_asset, mask_sha256, mask_pixel_sha256, mask_source) =
        match chain.iter().find(|r| r.kind == KIND_MASK) {
            Some(mask_record) => (
                Some(HANDLE_PREFIX.to_string() + &mask_record.asset_id),
                Some(mask_record.sha256.clone()),
                mask_record.pixel_sha256.clone(),
                "asset",
            ),
            None => {
                let mask_gray = GrayImage {
                    width: foreground.width,
                    height: foreground.height,
                    pixels: foreground.alpha_plane(),
                    icc_profile: None,
                };
                let bytes = crate::image::encode_png_gray(&mask_gray);
                (
                    None,
                    Some(crate::image::sha256_hex(&bytes)),
                    Some(mask_gray.to_rgba().pixel_sha256()),
                    "foreground_alpha",
                )
            }
        };

    let mask_provenance = if mask_source == "asset" {
        mask_asset
            .as_ref()
            .and_then(|h| mask_provenance_of(store, h))
    } else {
        None
    };

    let (plate, plate_source, plate_asset, plate_sha256, plate_pixel_sha256) = match plate_handle {
        Some(handle) => {
            let plate_record = store.get_record(handle)?;
            if plate_record.kind != KIND_PLATE {
                return Err(format!(
                    "Background plate {handle:?} is kind {:?}, not {KIND_PLATE}.",
                    plate_record.kind
                ));
            }
            let plate = store.read_image(handle)?;
            (
                plate,
                "asset",
                Some(handle.to_string()),
                Some(plate_record.sha256.clone()),
                plate_record.pixel_sha256.clone(),
            )
        }
        None => {
            let plate =
                generate_background_plate(scene_prompt, foreground.width, foreground.height);
            (plate, "local_placeholder", None, None, None)
        }
    };

    // Edge fix (docs/EDGE-FIX.md) for source-derived plates only; every
    // other draw keeps its exact composite. The source photo comes from
    // the layer's own lineage; the thin-structure flag from the staged
    // subject record when the caller passed one.
    let plate_kind = plate_kind_override.as_ref().map(|(kind, _)| *kind);
    let (composite, transformations, edge_fix) = if edge_fix_applies_to(plate_kind) {
        if edge_fix_enabled() {
            let source_image =
                store.read_image(&(HANDLE_PREFIX.to_string() + &source_record.asset_id))?;
            let thin = plate_kind_override
                .as_ref()
                .and_then(|(_, b)| b.pointer("/subject/thin_structure"))
                .and_then(Value::as_bool)
                .unwrap_or_else(|| crate::staging::analyze_subject(&foreground).thin);
            let (composite, _in_memory_counts, transformations, report) =
                composite_images_with_edge_fix(&foreground, &plate, &source_image, thin);
            (composite, transformations, Some(report.to_json()))
        } else {
            let (composite, _in_memory_counts, transformations) =
                composite_images(&foreground, &plate);
            let off = EdgeFixReport::skipped(&format!("disabled by {EDGE_FIX_ENV}"));
            (composite, transformations, Some(off.to_json()))
        }
    } else {
        let (composite, _in_memory_counts, transformations) = composite_images(&foreground, &plate);
        (composite, transformations, None)
    };
    let composite_handle = store.put_image(&composite, KIND_COMPOSITE, Some(&record.asset_id))?;
    let composite_record = store.get_record(&composite_handle)?;

    // Counts measured on the delivered file, decoded back from the store.
    let delivered_fg = store.read_image(foreground_handle)?;
    let delivered_out = store.read_image(&composite_handle)?;
    let counts = measure(&delivered_fg, &delivered_out);

    let color_profile_preserved =
        source_record.color_profile_sha256 == composite_record.color_profile_sha256;

    let mut receipt = json!({
        "receipt_schema": RECEIPT_SCHEMA_VERSION,
        "source_asset": HANDLE_PREFIX.to_string() + &source_record.asset_id,
        "source_sha256": source_record.sha256,
        "source_pixel_sha256": source_record.pixel_sha256,
        "foreground_asset": foreground_handle,
        "foreground_sha256": record.sha256,
        "foreground_pixel_sha256": record.pixel_sha256,
        "mask_asset": mask_asset,
        "mask_sha256": mask_sha256,
        "mask_pixel_sha256": mask_pixel_sha256,
        "mask_source": mask_source,
        "changed_opaque_pixels": counts.changed_opaque_pixels,
        "opaque_pixels": counts.opaque_pixels,
        "edge_band_pixels": counts.edge_band_pixels,
        "transparent_pixels": counts.transparent_pixels,
        "mask_pixels_total": counts.mask_pixels_total,
        "opaque_fraction": counts.opaque_fraction,
        "edge_band_max_width_px": counts.edge_band_max_width_px,
        "opaque_alpha_threshold": OPAQUE_ALPHA_THRESHOLD,
        "changed_predicate": "a pixel counts as changed when its RGBA value in the composite differs at all from the stored foreground at a position where the foreground alpha equals opaque_alpha_threshold; edge-band pixels (0 < alpha < threshold) blend by design, are excluded from the count, and are published above as edge_band_pixels and edge_band_max_width_px",
        "pixel_tolerance": 0,
        "count_sample": "decoded_delivered_file",
        "output_asset": composite_handle,
        "output_sha256": composite_record.sha256,
        "output_pixel_sha256": composite_record.pixel_sha256,
        "output_format": "PNG",
        "color_space": describe_color_profile(delivered_out.icc_profile.as_deref()),
        "color_profile_source_sha256": source_record.color_profile_sha256,
        "color_profile_foreground_sha256": record.color_profile_sha256,
        "color_profile_output_sha256": composite_record.color_profile_sha256,
        "color_profile_preserved": color_profile_preserved,
        "transformations": transformations,
        "product_layer_scaled": false,
        "plate_source": plate_source,
        "plate_asset": plate_asset,
        "plate_sha256": plate_sha256,
        "plate_pixel_sha256": plate_pixel_sha256,
        "plate_kind": match &plate_kind_override {
            Some((kind, _)) => json!(kind),
            None => {
                if plate_handle.is_some() {
                    json!("model_generated")
                } else {
                    json!("local_placeholder")
                }
            }
        },
        "backdrop": match &plate_kind_override {
            Some((_, backdrop)) => backdrop.clone(),
            None => Value::Null,
        },
        "claim": "The stored extracted foreground reached the composite unchanged: every fully opaque foreground pixel in the composite is identical to the stored foreground layer (foreground hashes above pin that layer). Edge-band pixels blend with the plate by design and are counted separately. The composite carries the source photo's colour profile unchanged (color_profile_preserved above); a profile change is a fidelity failure even when pixel values match."
    });
    if let Some(edge_fix) = edge_fix {
        // Additive record (never graded): what the edge fix drew around
        // the product. The counts above are over the stored layer's
        // pixel set, unchanged by it.
        receipt["edge_fix"] = edge_fix;
    }
    if let Some(provenance) = &mask_provenance {
        // Mask provenance (decisions 68/69): the receipt states the
        // frame the mask was cut from, the frame SAM actually saw,
        // and how the mask was mapped between them — so a "0 changed"
        // receipt never silently rests on a working-resolution
        // selection.
        receipt["mask_source_width"] = provenance["source_width"].clone();
        receipt["mask_source_height"] = provenance["source_height"].clone();
        receipt["mask_sam_width"] = provenance["sam_width"].clone();
        receipt["mask_sam_height"] = provenance["sam_height"].clone();
        receipt["mask_scale_method"] = provenance["scale_method"].clone();
    }
    // Provenance signals (bonus package): the source asset
    // carries the block ingest read from the original file in a
    // PNG text chunk; republishing it here binds those
    // observations to this receipt under the same pinned hash
    // that binds the pixel claim. Sources stored before the
    // block existed simply have no field, and receipts written
    // before it verify unchanged (the field is additive).
    let source_handle = HANDLE_PREFIX.to_string() + &source_record.asset_id;
    if let Ok(source_bytes) = store.read_bytes(&source_handle) {
        if let Some(raw) = png_text_of_bytes(&source_bytes).get(PROVENANCE_PNG_KEY) {
            if let Ok(block) = serde_json::from_str::<Value>(raw) {
                receipt["provenance"] = block;
            }
        }
    }
    // Persist the receipt beside the composite, as the Python
    // compositor does: a receipt that exists only in the tool's
    // return value is gone if the client drops it. The server layer
    // enriches this same file with the billed call's cost and
    // attribution once billing settles.
    store.save_receipt(&composite_handle, &receipt)?;
    Ok((composite_handle, receipt))
}

/// Fields that exist only in the current (schema 2) receipt shape:
/// the Python `_RECEIPT_FIELDS_V2` set minus `mask_asset` (v1
/// receipts name the mask asset too; they just do not pin its
/// hashes).
const V2_ONLY_FIELDS: [&str; 14] = [
    "color_profile_foreground_sha256",
    "mask_sha256",
    "mask_pixel_sha256",
    "mask_source",
    "opaque_alpha_threshold",
    "pixel_tolerance",
    "count_sample",
    "product_layer_scaled",
    "output_format",
    // The optional mask-provenance fields are current-schema too:
    // an unmarked receipt carrying them gets no legacy grading.
    "mask_source_width",
    "mask_source_height",
    "mask_sam_width",
    "mask_sam_height",
    "mask_scale_method",
];

fn is_hex64(v: &Value) -> bool {
    v.as_str()
        .map(|s| s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .unwrap_or(false)
}

fn is_hash_or_null(v: &Value) -> bool {
    v.is_null() || is_hex64(v)
}

fn is_handle_or_null(v: &Value) -> bool {
    v.is_null()
        || v.as_str()
            .map(|s| s.starts_with(HANDLE_PREFIX))
            .unwrap_or(false)
}

fn is_fraction(v: &Value) -> bool {
    !v.is_boolean() && v.as_f64().map(|f| (0.0..=1.0).contains(&f)).unwrap_or(false)
}

/// The verifiable schema a receipt is marked with: Some(2 | 3 | 4)
/// for a JSON integer marker in [`VERIFIABLE_RECEIPT_SCHEMAS`],
/// None otherwise (unmarked, or a marker this verifier refuses).
pub fn receipt_schema_version(receipt: &Value) -> Option<i64> {
    match receipt.get("receipt_schema") {
        Some(Value::Number(n)) => n
            .as_i64()
            .filter(|v| VERIFIABLE_RECEIPT_SCHEMAS.contains(v)),
        _ => None,
    }
}

/// Python `_is_reconstruction_block`: every required key present and
/// well-typed (status, assets, hashes, counts, fraction, motif,
/// regions, refusals).
fn is_reconstruction_block(value: &Value) -> bool {
    let Some(block) = value.as_object() else {
        return false;
    };
    if !RECONSTRUCTION_BLOCK_KEYS.iter().all(|k| block.contains_key(*k)) {
        return false;
    }
    if !matches!(block["status"].as_str(), Some("none" | "applied" | "refused")) {
        return false;
    }
    if !(block["renderer"].is_null() || block["renderer"].is_string()) {
        return false;
    }
    if !is_handle_or_null(&block["layer_asset"]) || !is_handle_or_null(&block["region_map_asset"]) {
        return false;
    }
    for name in [
        "layer_sha256",
        "layer_pixel_sha256",
        "region_map_sha256",
        "region_map_pixel_sha256",
    ] {
        if !is_hash_or_null(&block[name]) {
            return false;
        }
    }
    for name in [
        "reconstruction_pixels",
        "reconstruction_overlap_product_pixels",
        "unfilled_gap_pixels",
    ] {
        if !is_count(&block[name]) {
            return false;
        }
    }
    if !is_fraction(&block["reconstruction_fraction"]) {
        return false;
    }
    if !(block["guard_ring_px"].is_null() || is_count(&block["guard_ring_px"])) {
        return false;
    }
    let motif = &block["motif"];
    if !motif.is_null() {
        let Some(m) = motif.as_object() else {
            return false;
        };
        let opacity_ok = m
            .get("opacity")
            .map(|o| o.is_number() && !o.is_boolean())
            .unwrap_or(false);
        if !m.get("diameter_px").map(is_count).unwrap_or(false)
            || !opacity_ok
            || !m.get("color").map(Value::is_string).unwrap_or(false)
        {
            return false;
        }
    }
    let Some(regions) = block["regions"].as_array() else {
        return false;
    };
    for region in regions {
        let Some(r) = region.as_object() else {
            return false;
        };
        let class_ok = r
            .get("class")
            .and_then(Value::as_str)
            .map(|c| REGION_CLASS_CODES.iter().any(|(name, _)| *name == c))
            .unwrap_or(false);
        let bbox_ok = r
            .get("bbox")
            .and_then(Value::as_array)
            .map(|b| b.len() == 4 && b.iter().all(is_count))
            .unwrap_or(false);
        if !r.get("id").map(is_count).unwrap_or(false)
            || !class_ok
            || !r.get("pixels").map(is_count).unwrap_or(false)
            || !bbox_ok
            || !r.get("provenance").map(Value::is_object).unwrap_or(false)
        {
            return false;
        }
    }
    let Some(refusals) = block["refusals"].as_array() else {
        return false;
    };
    for refusal in refusals {
        let Some(r) = refusal.as_object() else {
            return false;
        };
        if !r.get("code").map(Value::is_string).unwrap_or(false)
            || !r.get("detail").map(Value::is_string).unwrap_or(false)
            || !r.get("pixels").map(is_count).unwrap_or(false)
        {
            return false;
        }
    }
    true
}

/// Schema 3+ shape problems of the reconstruction block (Python
/// `_validate_receipt`, `version >= 3` branches): presence and shape,
/// then the block's own arithmetic. Empty = acceptable.
fn reconstruction_problems(receipt: &Value) -> Vec<String> {
    let Some(block) = receipt.get("reconstruction") else {
        return vec!["missing field: reconstruction".to_string()];
    };
    if !is_reconstruction_block(block) {
        return vec![format!(
            "field reconstruction has the wrong type or shape: {}",
            serde_json::to_string(block).unwrap_or_default()
        )];
    }
    let mut problems = Vec::new();
    let pixels = block["reconstruction_pixels"].as_u64().unwrap_or(0);
    let region_sum: u64 = block["regions"]
        .as_array()
        .map(|rs| rs.iter().map(|r| r["pixels"].as_u64().unwrap_or(0)).sum())
        .unwrap_or(0);
    if block["reconstruction_overlap_product_pixels"].as_u64() != Some(0) {
        problems.push(
            "reconstruction_overlap_product_pixels must be 0; paper on product pixels is \
             not a weaker claim, it is a broken one"
                .to_string(),
        );
    }
    if region_sum != pixels {
        problems.push("reconstruction regions do not sum to reconstruction_pixels".to_string());
    }
    if block["status"].as_str() == Some("applied") {
        if block["layer_asset"].is_null() || pixels == 0 {
            problems.push(
                "reconstruction status is 'applied' but no layer asset or no paper pixels \
                 are claimed"
                    .to_string(),
            );
        }
        let opaque = receipt["opaque_pixels"].as_u64().unwrap_or(0);
        let total = pixels + opaque;
        let expected = if total > 0 {
            round6(pixels as f64 / total as f64)
        } else {
            0.0
        };
        let stated = block["reconstruction_fraction"].as_f64().unwrap_or(f64::NAN);
        if !((stated - expected).abs() <= 1e-9) {
            problems.push(
                "reconstruction_fraction disagrees with reconstruction_pixels / \
                 (reconstruction_pixels + opaque_pixels)"
                    .to_string(),
            );
        }
    } else if !block["layer_asset"].is_null() || pixels != 0 {
        problems.push(format!(
            "reconstruction status is {} but a layer asset or paper pixels are claimed",
            serde_json::to_string(&block["status"]).unwrap_or_default()
        ));
    }
    problems
}

/// The schema-marker policy of the Python verifier's
/// `_validate_receipt`, as the front door of
/// `verify_receipt_from_disk`: a receipt stamped with the current
/// schema version is graded as current; an unmarked receipt is the
/// v1 shape and is graded on the fields it carries; anything else
/// fails closed. The downgrade guard is the point: dropping or
/// lowering the marker of a current receipt must not buy it the
/// legacy policy, so a receipt carrying current-schema fields
/// under a missing or invalid marker is refused before any pixel
/// is recounted.
///
/// Returns the schema problems (empty = the marker state is
/// acceptable). Message wording tracks the Python verifier's; the
/// offending value is rendered as JSON where Python uses repr.
fn receipt_schema_problems(receipt: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    // Python: receipt.get("receipt_schema") — a missing key and an
    // explicit null are the same state (unmarked).
    let schema = match receipt.get("receipt_schema") {
        Some(Value::Null) | None => None,
        Some(v) => Some(v),
    };
    // Python: type(schema) is int and schema in the graded set
    // (VERIFIABLE_RECEIPT_SCHEMAS: 2, 3, 4). A JSON float (2.0),
    // string ("2"), or bool is NOT the marker.
    let version = receipt_schema_version(receipt);
    let strict = version.is_some();
    if let Some(v) = schema {
        if !strict {
            problems.push(format!(
                "unsupported receipt_schema {}; this verifier grades \
                 schema 2, 3 and 4 receipts and unmarked v1 receipts",
                serde_json::to_string(v).unwrap_or_default(),
            ));
        }
    }
    // Python: `0 < version < 3 and "reconstruction" in receipt`. Paper
    // disclosed on an unmarked or schema-2 receipt would otherwise be
    // carried as an ignored extra field: the blind spot schema 3
    // exists to close.
    if (schema.is_none() || version == Some(2)) && receipt.get("reconstruction").is_some() {
        problems.push(
            "receipt carries a reconstruction block but is not marked schema 3; \
             refusing to grade reconstruction under an older policy"
                .to_string(),
        );
    }
    if version.map(|v| v >= 3).unwrap_or(false) {
        problems.extend(reconstruction_problems(receipt));
    }
    if !strict && V2_ONLY_FIELDS.iter().any(|f| receipt.get(f).is_some()) {
        problems.push(
            "receipt carries current-schema fields but no valid \
             receipt_schema marker; refusing to grade it under the \
             legacy v1 policy"
                .to_string(),
        );
    }
    if strict
        && RECEIPT_FIELDS_V2_OPTIONAL
            .iter()
            .any(|f| receipt.get(f).is_some())
    {
        // Optional provenance fields: validated when present (the
        // Python `_validate_receipt` policy). Provenance is an
        // all-or-nothing, self-consistent statement: a partial or
        // contradictory set is a forged or torn receipt, not a
        // weaker claim.
        for name in RECEIPT_FIELDS_V2_OPTIONAL {
            let valid = match receipt.get(name) {
                Some(v) if name == "mask_scale_method" => v
                    .as_str()
                    .map(|s| MASK_SCALE_METHODS.contains(&s))
                    .unwrap_or(false),
                Some(v) => is_count(v),
                None => true,
            };
            if !valid {
                problems.push(format!(
                    "field {name} has the wrong type or shape: {}",
                    serde_json::to_string(&receipt[name]).unwrap_or_default()
                ));
            }
        }
        if problems.is_empty() {
            let missing: Vec<&str> = RECEIPT_FIELDS_V2_OPTIONAL
                .iter()
                .filter(|f| receipt.get(f).is_none())
                .copied()
                .collect();
            if !missing.is_empty() {
                let mut missing = missing;
                missing.sort_unstable();
                problems.push(format!(
                    "mask provenance is partial; missing fields: {}",
                    missing.join(", ")
                ));
            } else {
                let sam_w = receipt["mask_sam_width"].as_u64().unwrap_or(0);
                let sam_h = receipt["mask_sam_height"].as_u64().unwrap_or(0);
                let src_w = receipt["mask_source_width"].as_u64().unwrap_or(0);
                let src_h = receipt["mask_source_height"].as_u64().unwrap_or(0);
                if sam_w > src_w || sam_h > src_h {
                    problems.push(
                        "mask SAM frame exceeds the mask source frame; the \
                         SAM-bound copy is only ever downscaled"
                            .to_string(),
                    );
                }
                let unscaled = sam_w == src_w && sam_h == src_h;
                if (receipt["mask_scale_method"].as_str() == Some("none")) != unscaled {
                    problems.push(
                        "mask_scale_method disagrees with the recorded \
                         SAM and source frames"
                            .to_string(),
                    );
                }
            }
        }
    }
    problems
}

/// Independently re-verify a draw receipt from the stored FILES
/// (port of `verify_receipt_from_disk`). Returns a report object:
/// ok / checks / problems / recomputed.
///
/// The receipt may be omitted: draw persists it beside the
/// composite and it is loaded from there, so a composite found in
/// the store carries the claims to check with it. A supplied
/// receipt takes precedence. An error is returned when no receipt
/// is supplied and none is persisted for the composite.
pub fn verify_receipt_from_disk(
    store: &AssetStore,
    composite_handle: &str,
    receipt: Option<&Value>,
) -> Result<Value, String> {
    let loaded;
    let receipt = match receipt {
        Some(r) => r,
        None => {
            loaded = store.load_receipt(composite_handle)?.ok_or_else(|| {
                format!(
                    "No receipt supplied and none persisted beside \
                     '{composite_handle}'; there are no claims to verify."
                )
            })?;
            &loaded
        }
    };
    // 0. The schema marker, before anything is read from the
    //    receipt (the Python verifier's _validate_receipt front
    //    door, marker policy): a current receipt stripped of its
    //    marker fails here, structurally, instead of being graded
    //    under the legacy v1 policy. As in the Python report, the
    //    terms check is not reached on this path (receipt_terms
    //    stays true); receipt_shape carries the failure.
    let schema_problems = receipt_schema_problems(receipt);
    if !schema_problems.is_empty() {
        let mut checks: Map<String, Value> = Map::new();
        checks.insert("receipt_shape".to_string(), Value::Bool(false));
        checks.insert("receipt_terms".to_string(), Value::Bool(true));
        return Ok(json!({
            "ok": false,
            "checks": checks,
            "problems": [format!("receipt_shape: {}", schema_problems.join("; "))],
            "recomputed": {},
            "composite_asset": composite_handle,
            "foreground_asset": receipt["foreground_asset"].as_str(),
            "source_asset": receipt["source_asset"].as_str(),
        }));
    }
    let mut checks: Map<String, Value> = Map::new();
    let mut problems: Vec<String> = Vec::new();
    let set_check = |checks: &mut Map<String, Value>,
                     problems: &mut Vec<String>,
                     name: &str,
                     ok: bool,
                     detail: &str| {
        checks.insert(name.to_string(), Value::Bool(ok));
        if !ok {
            problems.push(if detail.is_empty() {
                name.to_string()
            } else {
                format!("{name}: {detail}")
            });
        }
    };

    let composite_record = store.get_record(composite_handle)?;
    let foreground_handle = receipt["foreground_asset"]
        .as_str()
        .ok_or("receipt has no foreground_asset")?
        .to_string();
    let source_handle = receipt["source_asset"]
        .as_str()
        .ok_or("receipt has no source_asset")?
        .to_string();

    for (label, handle) in [
        ("source", source_handle.as_str()),
        ("foreground", foreground_handle.as_str()),
        ("composite", composite_handle),
    ] {
        match store.verify_integrity(handle) {
            Ok(()) => set_check(
                &mut checks,
                &mut problems,
                &format!("{label}_file_integrity"),
                true,
                "",
            ),
            Err(e) => set_check(
                &mut checks,
                &mut problems,
                &format!("{label}_file_integrity"),
                false,
                &e,
            ),
        }
    }

    // 1b. The receipt's own hashes must name THESE files. Step 1
    // only proves each file matches its own asset record; without
    // this step, run A's receipt verifies against run B's composite
    // whenever the numbers happen to reproduce.
    let source_record = store.get_record(&source_handle)?;
    let foreground_record = store.get_record(&foreground_handle)?;
    set_check(
        &mut checks,
        &mut problems,
        "source_hash_matches_receipt",
        receipt["source_sha256"].as_str() == Some(source_record.sha256.as_str())
            && receipt["source_pixel_sha256"].as_str()
                == source_record.pixel_sha256.as_deref(),
        "the receipt's source hashes do not match the source asset's recorded hashes; this receipt was not issued for this source",
    );
    set_check(
        &mut checks,
        &mut problems,
        "foreground_hash_matches_receipt",
        receipt["foreground_sha256"].as_str() == Some(foreground_record.sha256.as_str())
            && receipt["foreground_pixel_sha256"].as_str()
                == foreground_record.pixel_sha256.as_deref(),
        "the receipt's foreground hashes do not match the foreground asset's recorded hashes",
    );
    set_check(
        &mut checks,
        &mut problems,
        "output_hash_matches_receipt",
        receipt["output_asset"].as_str() == Some(composite_handle)
            && receipt["output_sha256"].as_str() == Some(composite_record.sha256.as_str())
            && receipt["output_pixel_sha256"].as_str()
                == composite_record.pixel_sha256.as_deref(),
        "the receipt does not name this composite (its output_asset / output hashes point elsewhere); this receipt was issued for a different composite, even if the files happen to be byte-identical",
    );
    if let Some(mask_handle) = receipt["mask_asset"].as_str() {
        let pins_hashes = receipt["mask_sha256"].as_str().is_some()
            || receipt["mask_pixel_sha256"].as_str().is_some();
        if pins_hashes {
            match store.get_record(mask_handle) {
                Ok(mask_record) => set_check(
                    &mut checks,
                    &mut problems,
                    "mask_hash_matches_receipt",
                    receipt["mask_sha256"].as_str() == Some(mask_record.sha256.as_str())
                        && receipt["mask_pixel_sha256"].as_str()
                            == mask_record.pixel_sha256.as_deref(),
                    "the receipt's mask hashes do not match the mask asset's recorded hashes",
                ),
                Err(e) => set_check(
                    &mut checks,
                    &mut problems,
                    "mask_hash_matches_receipt",
                    false,
                    &e,
                ),
            }
        } else {
            // A receipt that names a mask asset without pinning its
            // hashes (issued before mask pinning — the frozen replay
            // package's receipt is the live example): the strongest
            // check available is the mask file's integrity against
            // its own asset record.
            match store.verify_integrity(mask_handle) {
                Ok(()) => set_check(&mut checks, &mut problems, "mask_asset_integrity", true, ""),
                Err(e) => set_check(
                    &mut checks,
                    &mut problems,
                    "mask_asset_integrity",
                    false,
                    &e,
                ),
            }
        }
    }

    // Schema 3+ (Item 11): the paper-fill reconstruction block's
    // assets must still match their records, and the receipt's hashes
    // must name them. Schema-2 receipts never reach this branch, so
    // their reports are unchanged.
    let recon_block: Option<&Map<String, Value>> = if receipt_schema_version(receipt)
        .map(|v| v >= 3)
        .unwrap_or(false)
    {
        receipt.get("reconstruction").and_then(Value::as_object)
    } else {
        None
    };
    if let Some(block) = recon_block {
        for (label, asset_key, sha_key, pixel_key, hash_check) in [
            (
                "reconstruction_layer",
                "layer_asset",
                "layer_sha256",
                "layer_pixel_sha256",
                "reconstruction_hash_matches_receipt",
            ),
            (
                "reconstruction_region_map",
                "region_map_asset",
                "region_map_sha256",
                "region_map_pixel_sha256",
                "region_map_hash_matches_receipt",
            ),
        ] {
            let Some(handle) = block.get(asset_key).and_then(Value::as_str) else {
                continue;
            };
            match store.verify_integrity(handle) {
                Ok(()) => set_check(
                    &mut checks,
                    &mut problems,
                    &format!("{label}_file_integrity"),
                    true,
                    "",
                ),
                Err(e) => set_check(
                    &mut checks,
                    &mut problems,
                    &format!("{label}_file_integrity"),
                    false,
                    &e,
                ),
            }
            match store.get_record(handle) {
                Ok(record) => set_check(
                    &mut checks,
                    &mut problems,
                    hash_check,
                    block.get(sha_key).and_then(Value::as_str) == Some(record.sha256.as_str())
                        && block.get(pixel_key).and_then(Value::as_str)
                            == record.pixel_sha256.as_deref(),
                    &format!(
                        "the receipt's {label} hashes do not match the asset's sidecar record"
                    ),
                ),
                Err(e) => set_check(&mut checks, &mut problems, hash_check, false, &e),
            }
        }
    }

    let foreground = store.read_image(&foreground_handle)?;
    let composite_image = store.read_image(composite_handle)?;
    set_check(
        &mut checks,
        &mut problems,
        "foreground_pixel_hash",
        foreground.pixel_sha256() == receipt["foreground_pixel_sha256"].as_str().unwrap_or(""),
        "decoded foreground pixels do not match the receipt's pinned hash",
    );
    set_check(
        &mut checks,
        &mut problems,
        "composite_pixel_hash",
        Some(composite_image.pixel_sha256()) == composite_record.pixel_sha256,
        "decoded composite pixels do not match the stored asset record",
    );

    let mut recomputed: Map<String, Value> = Map::new();
    if foreground.width != composite_image.width || foreground.height != composite_image.height {
        set_check(
            &mut checks,
            &mut problems,
            "recount",
            false,
            &format!(
                "foreground is {}x{}, composite is {}x{}; sizes must match exactly",
                foreground.width, foreground.height, composite_image.width, composite_image.height
            ),
        );
    } else {
        let counts = measure(&foreground, &composite_image);
        recomputed.insert(
            "changed_opaque_pixels".into(),
            json!(counts.changed_opaque_pixels),
        );
        recomputed.insert("opaque_pixels".into(), json!(counts.opaque_pixels));
        recomputed.insert("edge_band_pixels".into(), json!(counts.edge_band_pixels));
        recomputed.insert(
            "transparent_pixels".into(),
            json!(counts.transparent_pixels),
        );
        recomputed.insert("mask_pixels_total".into(), json!(counts.mask_pixels_total));
        recomputed.insert("opaque_fraction".into(), json!(counts.opaque_fraction));
        recomputed.insert(
            "edge_band_max_width_px".into(),
            json!(counts.edge_band_max_width_px),
        );
        for field in [
            "changed_opaque_pixels",
            "opaque_pixels",
            "edge_band_pixels",
            "transparent_pixels",
            "mask_pixels_total",
            "edge_band_max_width_px",
        ] {
            let mine = recomputed[field].as_u64().unwrap_or(0) as i64;
            let theirs = receipt[field].as_i64().unwrap_or(-1);
            set_check(
                &mut checks,
                &mut problems,
                &format!("recount_{field}"),
                mine == theirs,
                &format!("receipt says {theirs}, recount from disk finds {mine}"),
            );
        }
        let frac_diff = (counts.opaque_fraction
            - receipt["opaque_fraction"].as_f64().unwrap_or(f64::NAN))
        .abs();
        set_check(
            &mut checks,
            &mut problems,
            "recount_opaque_fraction",
            frac_diff < 1e-9,
            "receipt and recount disagree on opaque_fraction",
        );
    }

    // 2b. The verdict itself, stated as checks rather than implied
    // by agreement: a pass requires ZERO changed pixels — an honest
    // receipt reporting a real change reproduces perfectly under
    // the recount checks above and must still fail — and POSITIVE
    // coverage, because a receipt covering no opaque pixels vouches
    // for nothing.
    let show = |v: Option<i64>| match v {
        Some(n) => n.to_string(),
        None => "None".to_string(),
    };
    let recount_changed = recomputed["changed_opaque_pixels"].as_i64();
    let recount_opaque = recomputed["opaque_pixels"].as_i64();
    set_check(
        &mut checks,
        &mut problems,
        "zero_changed_pixels",
        recount_changed == Some(0)
            && receipt["changed_opaque_pixels"].as_i64() == Some(0),
        &format!(
            "changed opaque pixels: receipt says {}, recount from disk finds {}; the Fidelity Lock claim requires exactly 0",
            show(receipt["changed_opaque_pixels"].as_i64()),
            show(recount_changed)
        ),
    );
    set_check(
        &mut checks,
        &mut problems,
        "positive_coverage",
        recount_opaque.map(|v| v > 0).unwrap_or(false)
            && receipt["opaque_pixels"].as_i64().map(|v| v > 0).unwrap_or(false),
        "the receipt covers 0 fully opaque foreground pixels; there is no product evidence to verify",
    );

    let source_image = store.read_image(&source_handle)?;
    let source_profile = source_image.color_profile_sha256();
    let output_profile = composite_image.color_profile_sha256();
    recomputed.insert("color_profile_source_sha256".into(), json!(source_profile));
    recomputed.insert("color_profile_output_sha256".into(), json!(output_profile));
    set_check(
        &mut checks,
        &mut problems,
        "color_profile_matches_receipt",
        source_profile.as_deref() == receipt["color_profile_source_sha256"].as_str()
            && output_profile.as_deref() == receipt["color_profile_output_sha256"].as_str(),
        "profiles decoded from the files differ from the receipt's recorded profile hashes",
    );
    let preserved = source_profile == output_profile;
    recomputed.insert("color_profile_preserved".into(), json!(preserved));
    set_check(
        &mut checks,
        &mut problems,
        "color_profile_preserved",
        preserved && receipt["color_profile_preserved"].as_bool() == Some(true),
        "the delivered composite's colour profile differs from the source photo's; identical pixel values would display as different colours",
    );

    if let Some(block) = recon_block {
        schema3_pixel_checks(
            store,
            receipt,
            block,
            &foreground,
            &source_image,
            &composite_image,
            &mut checks,
            &mut problems,
            &mut recomputed,
        );
    }

    let ok = checks.values().all(|v| v.as_bool().unwrap_or(false));
    Ok(json!({
        "ok": ok,
        "checks": checks,
        "problems": problems,
        "recomputed": recomputed,
        "composite_asset": composite_handle,
        "foreground_asset": foreground_handle,
        "source_asset": source_handle,
    }))
}

/// Paper (alpha > 0 in the layer) and its overlap with the product
/// (any foreground alpha > 0): Python `_measure_reconstruction`.
fn measure_reconstruction(foreground: &RgbaImage, layer: &RgbaImage) -> (u64, u64) {
    let (mut paper, mut overlap) = (0u64, 0u64);
    for (fg, ly) in foreground
        .pixels
        .chunks_exact(4)
        .zip(layer.pixels.chunks_exact(4))
    {
        if ly[3] > 0 {
            paper += 1;
            if fg[3] > 0 {
                overlap += 1;
            }
        }
    }
    (paper, overlap)
}

/// The schema-3 pixel checks of the Python verifier (python `main`,
/// ff72fcb), run only for schema 3+ receipts:
///
/// - `foreground_matches_source`: every fully opaque foreground pixel
///   equals the source photo's pixel (fabricated product pixels under
///   a valid lineage otherwise pass every other check);
/// - `recount_reconstruction_pixels` / `recount_overlap_zero`: the
///   stored paper layer, recounted, holds the claimed paper and none
///   of it sits under any foreground alpha;
/// - `region_map_partition`: the region map covers exactly the paper
///   pixels, uses only known class codes, and each class's pixel
///   count equals the sum of that class's region counts;
/// - `layer_reproduction`: rebuilding plate, then paper, then product
///   from the stored layers reproduces the delivered file pixel for
///   pixel. Without a plate asset the check is a NAMED skip in
///   `recomputed`, never a silent pass.
#[allow(clippy::too_many_arguments)]
fn schema3_pixel_checks(
    store: &AssetStore,
    receipt: &Value,
    block: &Map<String, Value>,
    foreground: &RgbaImage,
    source_image: &RgbaImage,
    composite_image: &RgbaImage,
    checks: &mut Map<String, Value>,
    problems: &mut Vec<String>,
    recomputed: &mut Map<String, Value>,
) {
    let mut set_check = |name: &str, ok: bool, detail: &str| {
        checks.insert(name.to_string(), Value::Bool(ok));
        if !ok {
            problems.push(format!("{name}: {detail}"));
        }
    };

    // Foreground authenticity against the source photo.
    if (foreground.width, foreground.height) != (source_image.width, source_image.height) {
        set_check(
            "foreground_matches_source",
            false,
            &format!(
                "foreground is {}x{} but the source photo is {}x{}",
                foreground.width, foreground.height, source_image.width, source_image.height
            ),
        );
    } else {
        let same = foreground
            .pixels
            .chunks_exact(4)
            .zip(source_image.pixels.chunks_exact(4))
            .all(|(fg, src)| fg[3] != OPAQUE_ALPHA_THRESHOLD || fg == src);
        set_check(
            "foreground_matches_source",
            same,
            "the foreground differs from the source photo on fully opaque pixels; the \
             product layer is not the photograph it claims to be",
        );
    }

    // Paper recounts and the region-map partition.
    let mut layer_image: Option<RgbaImage> = None;
    if let Some(layer_handle) = block.get("layer_asset").and_then(Value::as_str) {
        let recount = (|| -> Result<(), String> {
            let layer = store.read_image(layer_handle)?;
            if (layer.width, layer.height) != (foreground.width, foreground.height) {
                return Err(format!(
                    "reconstruction layer is {}x{}, foreground is {}x{}",
                    layer.width, layer.height, foreground.width, foreground.height
                ));
            }
            let (paper, overlap) = measure_reconstruction(foreground, &layer);
            recomputed.insert("reconstruction_pixels".into(), json!(paper));
            recomputed.insert(
                "reconstruction_overlap_product_pixels".into(),
                json!(overlap),
            );
            let claimed = block["reconstruction_pixels"].as_u64().unwrap_or(0);
            set_check(
                "recount_reconstruction_pixels",
                paper == claimed,
                &format!("stored layer holds {paper} paper pixels; receipt says {claimed}"),
            );
            set_check(
                "recount_overlap_zero",
                overlap == 0,
                &format!(
                    "{overlap} paper pixels sit under foreground alpha; paper there changes \
                     the delivered product edge"
                ),
            );
            if let Some(map_handle) = block.get("region_map_asset").and_then(Value::as_str) {
                let map = store.read_gray_image(map_handle)?;
                if (map.width, map.height) != (layer.width, layer.height) {
                    return Err(format!(
                        "region map is {}x{}, layer is {}x{}",
                        map.width, map.height, layer.width, layer.height
                    ));
                }
                let partition_ok = map
                    .pixels
                    .iter()
                    .zip(layer.pixels.chunks_exact(4))
                    .all(|(&code, px)| (code > 0) == (px[3] > 0));
                let mut class_ok = map
                    .pixels
                    .iter()
                    .all(|&code| code == 0 || REGION_CLASS_CODES.iter().any(|(_, c)| *c == code));
                let regions = block["regions"].as_array().cloned().unwrap_or_default();
                for (class, code) in REGION_CLASS_CODES {
                    let expected: u64 = regions
                        .iter()
                        .filter(|r| r["class"].as_str() == Some(class))
                        .map(|r| r["pixels"].as_u64().unwrap_or(0))
                        .sum();
                    let found = map.pixels.iter().filter(|&&c| c == code).count() as u64;
                    if found != expected {
                        class_ok = false;
                    }
                }
                set_check(
                    "region_map_partition",
                    partition_ok && class_ok,
                    "the region map disagrees with the layer's paper pixels or the per-class \
                     counts",
                );
            }
            layer_image = Some(layer);
            Ok(())
        })();
        if let Err(e) = recount {
            set_check(
                "reconstruction_files_decodable",
                false,
                &format!("reconstruction assets could not be decoded: {e}"),
            );
        }
    }

    // Whole-delivery recomposition: plate, then paper, then product.
    match receipt.get("plate_asset").and_then(Value::as_str) {
        Some(plate_handle) => {
            let rebuilt = (|| -> Result<RgbaImage, String> {
                let plate = store.read_image(plate_handle)?;
                let (mut base, _) = fit_plate(&plate, foreground.width, foreground.height);
                if block.get("layer_asset").and_then(Value::as_str).is_some() {
                    let layer = layer_image
                        .as_ref()
                        .ok_or("the reconstruction layer could not be read")?;
                    let (_, overlap) = measure_reconstruction(foreground, layer);
                    if overlap > 0 {
                        return Err(format!(
                            "reconstruction layer places paper on {overlap} pixels the \
                             foreground occupies"
                        ));
                    }
                    base = alpha_composite(&base, layer);
                }
                Ok(alpha_composite(&base, foreground))
            })();
            match rebuilt {
                Ok(rebuilt) => set_check(
                    "layer_reproduction",
                    rebuilt.width == composite_image.width
                        && rebuilt.height == composite_image.height
                        && rebuilt.pixels == composite_image.pixels,
                    "the delivered file is not the composite of its stored plate, paper layer, \
                     and foreground; pixels outside the three layers are present",
                ),
                Err(e) => set_check(
                    "layer_reproduction",
                    false,
                    &format!("rebuild from stored layers failed: {e}"),
                ),
            }
        }
        None => {
            recomputed.insert(
                "layer_reproduction".into(),
                json!("skipped: no plate asset (local placeholder plate)"),
            );
        }
    }
}

/// Standalone verification of exported files (port of `verify.py`):
/// SOURCE photo, FINAL composite, MASK png; no store involved.
pub fn verify_files(
    source_path: &std::path::Path,
    final_path: &std::path::Path,
    mask_path: &std::path::Path,
) -> Result<Value, String> {
    let load_rgba = |p: &std::path::Path| -> Result<RgbaImage, String> {
        let data = std::fs::read(p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
        crate::image::decode_image(&data)
            .map(|d| d.into_rgba())
            .map_err(|e| format!("{}: {e}", p.display()))
    };
    let source = load_rgba(source_path)?;
    let final_img = load_rgba(final_path)?;
    let mask_data = std::fs::read(mask_path)
        .map_err(|e| format!("cannot read {}: {e}", mask_path.display()))?;
    let mask = crate::image::decode_image(&mask_data)
        .map_err(|e| format!("{}: {e}", mask_path.display()))?;
    let mask_gray: Vec<u8> = match &mask {
        crate::image::DecodedImage::Gray(g) => g.pixels.clone(),
        crate::image::DecodedImage::Rgba(r) => r
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|px| {
                ((px[0] as u32 * 19595 + px[1] as u32 * 38470 + px[2] as u32 * 7471 + 0x8000) >> 16)
                    as u8
            })
            .collect(),
    };
    let (mask_w, mask_h) = match &mask {
        crate::image::DecodedImage::Gray(g) => (g.width, g.height),
        crate::image::DecodedImage::Rgba(r) => (r.width, r.height),
    };

    let mut problems: Vec<String> = Vec::new();
    if (final_img.width, final_img.height) != (source.width, source.height) {
        problems.push(format!(
            "final is {}x{}, source is {}x{}; the product canvas must never be rescaled",
            final_img.width, final_img.height, source.width, source.height
        ));
    }
    if (mask_w, mask_h) != (source.width, source.height) {
        problems.push(format!(
            "mask is {}x{}, source is {}x{}; the mask must cover the source frame exactly",
            mask_w, mask_h, source.width, source.height
        ));
    }

    let (mut checked, mut changed, mut edge_band, mut outside) = (0u64, 0u64, 0u64, 0u64);
    if problems.is_empty() {
        for (i, &m) in mask_gray.iter().enumerate() {
            if m == 255 {
                checked += 1;
                let j = i * 4;
                if source.pixels[j..j + 4] != final_img.pixels[j..j + 4] {
                    changed += 1;
                }
            } else if m > 0 {
                edge_band += 1;
            } else {
                outside += 1;
            }
        }
        if changed > 0 {
            problems.push(format!(
                "{changed} of {checked} fully-masked product pixels differ between the source photo and the final image"
            ));
        }
        if checked == 0 {
            // A mask that selects nothing checks nothing: without
            // this refusal, two completely different images pass
            // with an empty mask because 0 of 0 pixels changed.
            problems.push(
                "the mask selects no fully-on pixels, so no product pixels were checked; a verification that compares nothing does not pass"
                    .to_string(),
            );
        }
    }

    let source_profile = source.color_profile_sha256();
    let output_profile = final_img.color_profile_sha256();
    let preserved = source_profile == output_profile;
    if !preserved {
        problems.push(
            "colour profile changed between the source photo and the final image; identical pixel values would display as different colours"
                .to_string(),
        );
    }

    Ok(json!({
        "ok": problems.is_empty(),
        "source": source_path.display().to_string(),
        "final": final_path.display().to_string(),
        "mask": mask_path.display().to_string(),
        "checked_pixels": checked,
        "changed_pixels": changed,
        "edge_band_pixels": edge_band,
        "outside_mask_pixels": outside,
        "color_profile_source_sha256": source_profile,
        "color_profile_output_sha256": output_profile,
        "color_profile_preserved": preserved,
        "problems": problems,
    }))
}
