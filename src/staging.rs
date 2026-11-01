//! Source-derived staging plates (plate kinds `source_edit` and
//! `source_blur`).
//!
//! The plate is built from the ORIGINAL photo, so its camera angle,
//! ground plane and light already match the product, which never
//! moves or scales:
//! - `masked_reference`: the photo at the edit size with every visible
//!   product pixel (alpha > 0) plus an 8 px ring filled flat grey. This
//!   is the only image the edit model receives; product pixels are
//!   never sent.
//! - `fallback_plate`: the code-only plate when the edit call fails:
//!   the same masked photo, hole filled from its surroundings
//!   (push-pull), blurred and lifted toward white.
//! - `finish_plate`: contact shadow (skipped for surfaces and subjects
//!   cut by the bottom edge) and the 1-2 px plate-side edge matte.
//!
//! Subjects are anything SAM can select, surfaces included (a lawn, a
//! fence run, a patio set): `analyze_subject` measures coverage, frame
//! edge contact and thin structure, and the plate steps adapt to it.

use crate::composite::{
    apply_contact_shadow, apply_edge_matte, dilate_square, opaque_bbox, OPAQUE_ALPHA_THRESHOLD,
};
use crate::image::RgbaImage;
use crate::resample::{resize_rgba, ResampleFilter};
use serde_json::{json, Value};

/// A subject covering more than this fraction of the frame, or
/// spanning it (touching opposite edges: a lawn, a fence run), is a
/// surface / frame-filling subject: "continue the surroundings"
/// wording, no contact shadow, the honest `SURFACE_NOTE`. Touching one
/// edge or two adjacent edges is cropping (a bike cut by the bottom,
/// a car cut by the side): object wording, reported in
/// `touches_frame_edges`, shadow skipped only at the bottom edge.
pub const FILLS_FRAME_COVERAGE: f64 = 0.45;
/// A run of product pixels (row or column) shorter than this is thin.
pub const THIN_RUN_PX: u32 = 6;
/// The subject has thin structure when at least this fraction of its
/// product pixels sit in a thin run (a robust "thinnest run": one
/// pointed tip or a ragged edge pixel does not trip it, a fence of
/// pickets or a wheel of spokes does).
pub const THIN_FRACTION: f64 = 0.01;
/// Edge-matte width, and the cap for thin subjects.
pub const MATTE_PX: u32 = 2;
pub const MATTE_PX_THIN: u32 = 1;
/// Grey fill of the masked reference and its dilation (reference px).
pub const REFERENCE_GREY: [u8; 3] = [128, 128, 128];
pub const REFERENCE_DILATE_PX: usize = 8;
/// Fallback plate: working long side, blur radius (work px, three box
/// passes) and lift toward white.
pub const FALLBACK_WORK_LONG_SIDE: u32 = 640;
pub const FALLBACK_BLUR_RADIUS: usize = 5;
pub const FALLBACK_LIFT: f32 = 0.35;

/// The honest note for surface / frame-filling subjects.
pub const SURFACE_NOTE: &str = "subject is a surface or fills the frame; staging kept its pixels and replaced only what was around it";

/// What the plate steps need to know about the subject.
#[derive(Debug, Clone)]
pub struct SubjectGeometry {
    pub width: u32,
    pub height: u32,
    /// Fully opaque product pixels (the receipt's product pixels).
    pub opaque_pixels: u64,
    pub coverage: f64,
    /// Half-open bbox of the opaque pixels.
    pub bbox: Option<(u32, u32, u32, u32)>,
    pub touches: Vec<&'static str>,
    pub fills_frame: bool,
    pub thin_fraction: f64,
    pub thin: bool,
    pub shadow: bool,
    pub matte_px: u32,
}

impl SubjectGeometry {
    pub fn to_json(&self) -> Value {
        json!({
            "opaque_pixels": self.opaque_pixels,
            "coverage": crate::round6(self.coverage),
            "bbox": self.bbox.map(|b| [b.0, b.1, b.2, b.3]),
            "touches_frame_edges": self.touches,
            "fills_frame": self.fills_frame,
            "thin_fraction": crate::round6(self.thin_fraction),
            "thin_structure": self.thin,
        })
    }
}

/// Fraction of opaque pixels lying in a row or column run of opaque
/// pixels shorter than `THIN_RUN_PX` (min(h-run, v-run) < 6).
fn thin_fraction(fg: &RgbaImage, bbox: (u32, u32, u32, u32), opaque: u64) -> f64 {
    if opaque == 0 {
        return 0.0;
    }
    let (x0, y0, x1, y1) = (
        bbox.0 as usize,
        bbox.1 as usize,
        bbox.2 as usize,
        bbox.3 as usize,
    );
    let (w, h, fw) = (x1 - x0, y1 - y0, fg.width as usize);
    let on =
        |x: usize, y: usize| fg.pixels[((y0 + y) * fw + x0 + x) * 4 + 3] == OPAQUE_ALPHA_THRESHOLD;
    let thin_len = THIN_RUN_PX as usize;
    let mut thin = vec![false; w * h];
    for y in 0..h {
        let mut x = 0;
        while x < w {
            if !on(x, y) {
                x += 1;
                continue;
            }
            let start = x;
            while x < w && on(x, y) {
                x += 1;
            }
            if x - start < thin_len {
                thin[y * w + start..y * w + x]
                    .iter_mut()
                    .for_each(|t| *t = true);
            }
        }
    }
    for x in 0..w {
        let mut y = 0;
        while y < h {
            if !on(x, y) {
                y += 1;
                continue;
            }
            let start = y;
            while y < h && on(x, y) {
                y += 1;
            }
            if y - start < thin_len {
                (start..y).for_each(|yy| thin[yy * w + x] = true);
            }
        }
    }
    thin.iter().filter(|&&t| t).count() as f64 / opaque as f64
}

/// Measure the subject from the foreground's alpha.
pub fn analyze_subject(foreground: &RgbaImage) -> SubjectGeometry {
    let (w, h) = (foreground.width, foreground.height);
    let opaque = foreground
        .pixels
        .chunks_exact(4)
        .filter(|p| p[3] == OPAQUE_ALPHA_THRESHOLD)
        .count() as u64;
    let frame = (w as u64 * h as u64).max(1) as f64;
    let coverage = opaque as f64 / frame;
    let bbox = opaque_bbox(foreground);
    let mut touches = Vec::new();
    if let Some((x0, y0, x1, y1)) = bbox {
        for (hit, edge) in [
            (x0 == 0, "left"),
            (y0 == 0, "top"),
            (x1 == w, "right"),
            (y1 == h, "bottom"),
        ] {
            if hit {
                touches.push(edge);
            }
        }
    }
    let thin_fraction = bbox.map_or(0.0, |b| thin_fraction(foreground, b, opaque));
    let thin = thin_fraction >= THIN_FRACTION;
    let spans = |a: &str, b: &str| touches.contains(&a) && touches.contains(&b);
    let fills_frame =
        coverage > FILLS_FRAME_COVERAGE || spans("left", "right") || spans("top", "bottom");
    SubjectGeometry {
        width: w,
        height: h,
        opaque_pixels: opaque,
        coverage,
        bbox,
        // A contact shadow under a surface (a lawn, a fence run), or
        // under something the frame cuts at the bottom, would be
        // invented ground.
        shadow: opaque > 0 && !fills_frame && !touches.contains(&"bottom"),
        touches,
        fills_frame,
        thin_fraction,
        thin,
        matte_px: if thin { MATTE_PX_THIN } else { MATTE_PX },
    }
}

/// Refuse to stage a foreground with no product pixels: a plate with
/// nothing on it is not staging, and a receipt over 0 pixels vouches
/// for nothing. Runs before any spend.
pub fn check_stageable(geometry: &SubjectGeometry) -> Result<(), String> {
    if geometry.opaque_pixels == 0 {
        return Err("The foreground has no fully opaque product pixels (an empty or fully soft mask), so there is nothing to stage and nothing a receipt could vouch for. Run cut_out again with a more specific phrase, or retake the photo closer to the subject.".to_string());
    }
    Ok(())
}

/// The product footprint at (out_w, out_h): a reference pixel is in
/// the hole when any source pixel under it is visible product
/// (alpha > 0), then grown by `dilate` px.
fn footprint(foreground: &RgbaImage, out_w: u32, out_h: u32, dilate: usize) -> Vec<u8> {
    let (w, h) = (foreground.width as u64, foreground.height as u64);
    let mut hole = vec![0u8; (out_w * out_h) as usize];
    for y in 0..h {
        let ry = (y * out_h as u64 / h) as usize;
        for x in 0..w {
            if foreground.pixels[((y * w + x) * 4 + 3) as usize] > 0 {
                hole[ry * out_w as usize + (x * out_w as u64 / w) as usize] = 1;
            }
        }
    }
    dilate_square(&hole, out_w as usize, out_h as usize, dilate)
}

/// The edit reference: the source at (out_w, out_h) with the product
/// footprint plus `REFERENCE_DILATE_PX` filled `REFERENCE_GREY`.
/// Returns (reference, hole pixel count).
pub fn masked_reference(
    source: &RgbaImage,
    foreground: &RgbaImage,
    out_w: u32,
    out_h: u32,
) -> (RgbaImage, u64) {
    let mut reference = resize_rgba(source, out_w, out_h, ResampleFilter::Lanczos);
    reference.icc_profile = None;
    let hole = footprint(foreground, out_w, out_h, REFERENCE_DILATE_PX);
    let mut hole_px = 0u64;
    for (i, px) in reference.pixels.chunks_exact_mut(4).enumerate() {
        if hole[i] != 0 {
            px[..3].copy_from_slice(&REFERENCE_GREY);
            hole_px += 1;
        }
        px[3] = 255;
    }
    (reference, hole_px)
}

/// Fill unknown pixels from known neighbours across scales (push-pull).
fn push_pull(rgb: &mut [[f32; 3]], known: &[bool], w: usize, h: usize) {
    if known.iter().all(|&k| k) {
        return;
    }
    if !known.iter().any(|&k| k) {
        rgb.iter_mut().for_each(|p| *p = [200.0; 3]);
        return;
    }
    let (cw, ch) = ((w + 1) / 2, (h + 1) / 2);
    let mut coarse = vec![[0f32; 3]; cw * ch];
    let mut coarse_known = vec![false; cw * ch];
    for cy in 0..ch {
        for cx in 0..cw {
            let (mut sum, mut n) = ([0f32; 3], 0f32);
            for y in 2 * cy..(2 * cy + 2).min(h) {
                for x in 2 * cx..(2 * cx + 2).min(w) {
                    if known[y * w + x] {
                        (0..3).for_each(|c| sum[c] += rgb[y * w + x][c]);
                        n += 1.0;
                    }
                }
            }
            if n > 0.0 {
                coarse[cy * cw + cx] = [sum[0] / n, sum[1] / n, sum[2] / n];
                coarse_known[cy * cw + cx] = true;
            }
        }
    }
    push_pull(&mut coarse, &coarse_known, cw, ch);
    for y in 0..h {
        for x in 0..w {
            if !known[y * w + x] {
                rgb[y * w + x] = coarse[(y / 2) * cw + x / 2];
            }
        }
    }
}

/// Three separable box passes (about a Gaussian), edges clamped.
fn box_blur(rgb: &mut Vec<[f32; 3]>, w: usize, h: usize, r: usize) {
    let norm = 1.0 / (2 * r + 1) as f32;
    for _ in 0..3 {
        for (len, stride, lines, step) in [(w, 1usize, h, w), (h, w, w, 1usize)] {
            let mut out = rgb.clone();
            for line in 0..lines {
                let base = line * step;
                for i in 0..len {
                    let mut acc = [0f32; 3];
                    for k in -(r as isize)..=(r as isize) {
                        let j = (i as isize + k).clamp(0, len as isize - 1) as usize;
                        let p = rgb[base + j * stride];
                        (0..3).for_each(|c| acc[c] += p[c]);
                    }
                    out[base + i * stride] = [acc[0] * norm, acc[1] * norm, acc[2] * norm];
                }
            }
            *rgb = out;
        }
    }
}

/// The code-only plate: the source with the product region filled from
/// its surroundings, blurred and lifted, at the source size. Free and
/// deterministic; geometry and light are the photo's own.
pub fn fallback_plate(source: &RgbaImage, foreground: &RgbaImage) -> RgbaImage {
    let (w, h) = (source.width, source.height);
    let scale = (FALLBACK_WORK_LONG_SIDE as f64 / w.max(h) as f64).min(1.0);
    let ww = ((w as f64 * scale).round() as u32).max(1);
    let wh = ((h as f64 * scale).round() as u32).max(1);
    let small = resize_rgba(source, ww, wh, ResampleFilter::Bilinear);
    let hole = footprint(foreground, ww, wh, 2);
    let known: Vec<bool> = hole.iter().map(|&v| v == 0).collect();
    let mut rgb: Vec<[f32; 3]> = small
        .pixels
        .chunks_exact(4)
        .map(|p| [p[0] as f32, p[1] as f32, p[2] as f32])
        .collect();
    push_pull(&mut rgb, &known, ww as usize, wh as usize);
    box_blur(&mut rgb, ww as usize, wh as usize, FALLBACK_BLUR_RADIUS);
    let mut pixels = Vec::with_capacity(rgb.len() * 4);
    for p in &rgb {
        for c in p {
            pixels.push(
                (c * (1.0 - FALLBACK_LIFT) + 255.0 * FALLBACK_LIFT)
                    .round()
                    .clamp(0.0, 255.0) as u8,
            );
        }
        pixels.push(255);
    }
    let work = RgbaImage::new(ww, wh, pixels);
    let mut plate = resize_rgba(&work, w, h, ResampleFilter::Bicubic);
    plate.icc_profile = source.icc_profile.clone();
    plate
}

/// An edit-model plate at the source size: decoded, resized back to
/// the source frame (the reference was the same frame at the edit
/// size, so this restores the geometry), made opaque.
pub fn plate_from_edit(edit: &RgbaImage, width: u32, height: u32) -> RgbaImage {
    let mut plate = resize_rgba(edit, width, height, ResampleFilter::Bicubic);
    plate.pixels.chunks_exact_mut(4).for_each(|p| p[3] = 255);
    plate.icc_profile = None;
    plate
}

/// Contact shadow (when the geometry allows one) and the plate-side
/// edge matte. Plate pixels only.
pub fn finish_plate(
    plate: &RgbaImage,
    foreground: &RgbaImage,
    geometry: &SubjectGeometry,
) -> RgbaImage {
    let shadowed = if geometry.shadow {
        apply_contact_shadow(plate, foreground)
    } else {
        plate.clone()
    };
    apply_edge_matte(&shadowed, foreground, geometry.matte_px)
}
