//! Source-derived staging plates (plate kinds `source_edit` and
//! `source_blur`).
//!
//! The plate is built from the ORIGINAL photo, so its camera angle,
//! ground plane and light already match the product, which never
//! moves or scales:
//! - `edit_reference`: the photo at the edit size with every visible
//!   product pixel (alpha > 0) plus an 8 px ring (wider for upscaled
//!   small sources) removed and filled from the pixels OUTSIDE that
//!   footprint (`inpaint_diffusion`: a coarse-to-fine harmonic fill
//!   plus a faint copy of the surround's fine texture), so the model
//!   sees no object-shaped silhouette.
//!   This is the only image the edit model receives; product pixels
//!   are never sent. `AGENTHAND_REFERENCE_FILL=grey` restores the
//!   legacy flat-grey hole (`masked_reference`) for comparison.
//!   docs/REFERENCE-FILL.md has the why.
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
/// Grey fill of the legacy masked reference (`AGENTHAND_REFERENCE_FILL=grey`)
/// (also the product pixels' stand-in before the resize, both fills)
/// and the product footprint's minimum dilation (reference px, both
/// fills; `reference_dilate_px` widens it for upscaled sources).
pub const REFERENCE_GREY: [u8; 3] = [128, 128, 128];
pub const REFERENCE_DILATE_PX: usize = 8;
/// Reference fill switch: `grey` (or `gray`) restores the flat-grey
/// hole for comparison; unset or anything else is `inpaint_diffusion`.
pub const REFERENCE_FILL_ENV: &str = "AGENTHAND_REFERENCE_FILL";
/// Inpaint pyramid: halve (known-weighted 2x2 means) until the long
/// side is at most this, solve there, then refine level by level back
/// up to the reference size.
pub const INPAINT_COARSE_LONG_SIDE: usize = 8;
/// Gauss-Seidel (over-relaxed) sweeps per level: at most this many,
/// stopping early once no hole value moves more than the tolerance
/// (0-255 units) in a sweep.
pub const INPAINT_MAX_SWEEPS: usize = 200;
pub const INPAINT_TOLERANCE: f32 = 0.02;
pub const INPAINT_SOR: f32 = 1.5;
/// Texture: the surround's fine grain (pixel minus its known-only box
/// mean of this radius), clamped to +-`CLAMP` (0-255 units) so edges
/// are not echoed, mirrored into each hole row from both run ends and
/// added at `GAIN`. Tuned on IMG_1443: a wider radius or clamp mirrors
/// fence edges and spokes into the hole as fan-shaped echoes, which is
/// a silhouette of its own (docs/REFERENCE-FILL.md).
pub const INPAINT_TEXTURE_RADIUS: usize = 2;
pub const INPAINT_TEXTURE_CLAMP: f32 = 10.0;
pub const INPAINT_TEXTURE_GAIN: f32 = 1.0;
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

// ---------------------------------------------------------------------------
// Shot-angle estimate (angle lock). Mask geometry alone is a weak cue for
// camera pitch, so the thresholds are conservative: a specific angle is
// stated only when every cue agrees, and `EyeLevel` is the unstated
// default. Calibrated against the repo's real photos: car and jacket (shot
// from standing height looking down) land in `SteepDown`; the bike (mild
// downward, ground line near the bottom) stays at the unstated default.
// ---------------------------------------------------------------------------

/// SteepDown: the ground-contact line (bbox bottom) sits at least this
/// fraction of the frame above the bottom edge ("ground line high": a
/// clear band of ground in front of the subject).
pub const ANGLE_STEEP_MIN_GROUND_BELOW: f64 = 0.15;
/// SteepDown: the subject's top edge reaches at least this high (y/h).
/// At eye level a ground object no taller than the camera sits wholly
/// below the horizon (the frame midline); reaching into the upper third
/// while resting on visible ground needs the camera tilted down.
pub const ANGLE_STEEP_MAX_TOP: f64 = 0.33;
/// SteepDown: footprint wide (bbox width / height).
pub const ANGLE_STEEP_MIN_ASPECT: f64 = 1.25;
/// SteepDown: a substantial subject (bbox height / frame height).
pub const ANGLE_STEEP_MIN_HEIGHT: f64 = 0.25;
/// LowUp: the ground-contact line sits in the bottom 12% of the frame.
pub const ANGLE_LOW_MIN_CONTACT: f64 = 0.88;
/// LowUp: the bbox centre sits above the frame midline (y/h).
pub const ANGLE_LOW_MAX_CENTRE: f64 = 0.46;
/// LowUp: towering (bbox width / height).
pub const ANGLE_LOW_MAX_ASPECT: f64 = 0.85;

/// The three shot-angle buckets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShotAngle {
    /// Shot from standing height looking down at the surface.
    SteepDown,
    /// Eye level; also the default when the geometry is inconclusive.
    EyeLevel,
    /// Shot from a low angle looking slightly up.
    LowUp,
}

impl ShotAngle {
    pub fn bucket(self) -> &'static str {
        match self {
            ShotAngle::SteepDown => "steep_down",
            ShotAngle::EyeLevel => "eye_level",
            ShotAngle::LowUp => "low_up",
        }
    }
}

/// The normalised mask geometry the estimate reads (y grows downward).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AngleSignals {
    /// bbox top edge / frame height.
    pub top_y: f64,
    /// bbox centre / frame height (0.5 is the midline).
    pub centre_y: f64,
    /// Ground-contact line (bbox bottom, where the contact shadow sits)
    /// / frame height.
    pub contact_y: f64,
    /// Frame below the contact line (1 - contact_y).
    pub ground_below: f64,
    /// bbox width / bbox height, in pixels.
    pub aspect: f64,
    /// bbox height / frame height.
    pub height_frac: f64,
    /// Opaque product pixels / frame pixels.
    pub coverage: f64,
}

/// The estimate and why. `stated` is true only when the geometry
/// supports a specific angle (SteepDown or LowUp); the EyeLevel
/// default is never stated in a source_edit prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct AngleEstimate {
    pub angle: ShotAngle,
    pub stated: bool,
    pub signals: Option<AngleSignals>,
    pub reason: &'static str,
}

impl AngleEstimate {
    pub fn signals_json(&self) -> Value {
        match self.signals {
            None => Value::Null,
            Some(s) => json!({
                "top_y": crate::round6(s.top_y),
                "centre_y": crate::round6(s.centre_y),
                "contact_y": crate::round6(s.contact_y),
                "ground_below": crate::round6(s.ground_below),
                "aspect": crate::round6(s.aspect),
                "height_frac": crate::round6(s.height_frac),
                "coverage": crate::round6(s.coverage),
            }),
        }
    }
}

/// Estimate the shot angle from the subject geometry (pure; no pixels).
pub fn estimate_shot_angle(g: &SubjectGeometry) -> AngleEstimate {
    estimate_shot_angle_from(g.width, g.height, g.bbox, g.coverage, g.fills_frame, &g.touches)
}

/// The estimator over primitives (bbox half-open, in frame pixels).
/// Rules, all cues required:
/// - SteepDown: contact line at least 15% above the bottom edge, top
///   edge in the upper third, bbox at least 1.25x wider than tall and
///   at least 25% of the frame tall.
/// - LowUp: contact line in the bottom 12%, bbox centre above 0.46,
///   bbox at most 0.85x as wide as tall.
/// - Otherwise EyeLevel, unstated. Also unstated whenever there is no
///   bbox, the subject fills the frame (a surface), or the subject is
///   cut by the bottom edge (the ground-contact line is not visible).
pub fn estimate_shot_angle_from(
    width: u32,
    height: u32,
    bbox: Option<(u32, u32, u32, u32)>,
    coverage: f64,
    fills_frame: bool,
    touches: &[&str],
) -> AngleEstimate {
    let default = |signals: Option<AngleSignals>, reason: &'static str| AngleEstimate {
        angle: ShotAngle::EyeLevel,
        stated: false,
        signals,
        reason,
    };
    let Some((x0, y0, x1, y1)) = bbox else {
        return default(None, "no product bbox; angle not estimated");
    };
    if width == 0 || height == 0 || x1 <= x0 || y1 <= y0 {
        return default(None, "degenerate bbox; angle not estimated");
    }
    let h = height as f64;
    let signals = AngleSignals {
        top_y: y0 as f64 / h,
        centre_y: (y0 as f64 + y1 as f64) / 2.0 / h,
        contact_y: y1 as f64 / h,
        ground_below: (h - y1 as f64) / h,
        aspect: (x1 - x0) as f64 / (y1 - y0) as f64,
        height_frac: (y1 - y0) as f64 / h,
        coverage,
    };
    if fills_frame {
        return default(
            Some(signals),
            "subject fills the frame (a surface); no angle claimed beyond the base clause",
        );
    }
    if touches.contains(&"bottom") {
        return default(
            Some(signals),
            "subject is cut by the bottom edge, so the ground-contact line is not visible; no angle claimed beyond the base clause",
        );
    }
    let s = signals;
    if s.ground_below >= ANGLE_STEEP_MIN_GROUND_BELOW
        && s.top_y <= ANGLE_STEEP_MAX_TOP
        && s.aspect >= ANGLE_STEEP_MIN_ASPECT
        && s.height_frac >= ANGLE_STEEP_MIN_HEIGHT
    {
        return AngleEstimate {
            angle: ShotAngle::SteepDown,
            stated: true,
            signals: Some(s),
            reason: "wide subject resting on a clear band of ground and reaching into the upper third: camera tilted down",
        };
    }
    if s.contact_y >= ANGLE_LOW_MIN_CONTACT
        && s.centre_y <= ANGLE_LOW_MAX_CENTRE
        && s.aspect <= ANGLE_LOW_MAX_ASPECT
    {
        return AngleEstimate {
            angle: ShotAngle::LowUp,
            stated: true,
            signals: Some(s),
            reason: "tall subject rising above the midline from a thin strip of ground at the bottom: camera low, looking up",
        };
    }
    default(
        Some(s),
        "geometry inconclusive; eye level assumed, no angle claimed beyond the base clause",
    )
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

/// The footprint's dilation (reference px) for a `src_w` x `src_h`
/// source sent at `out_w` x `out_h`: `REFERENCE_DILATE_PX`, widened
/// when the source is UPSCALED so it always covers the Lanczos resize
/// support (3 source px = 3 x upscale reference px past a product
/// pixel's covered area) plus 2 px: max(8, ceil(3 x upscale) + 2).
/// Edit sizes are at least 1024 px, so a small web photo (300x225 at
/// 1536x1024 is a 5.1x upscale) gets 18 px; at a 2x upscale or any
/// downscale it is the plain 8.
pub fn reference_dilate_px(src_w: u32, src_h: u32, out_w: u32, out_h: u32) -> usize {
    let upscale = (out_w as f64 / src_w.max(1) as f64).max(out_h as f64 / src_h.max(1) as f64);
    REFERENCE_DILATE_PX.max((3.0 * upscale).ceil() as usize + 2)
}

/// The product footprint at (out_w, out_h): a reference pixel is in
/// the hole when any visible product pixel (alpha > 0) lies under it,
/// then grown by `dilate` px. Along an axis that is downscaled a
/// source pixel marks the reference pixel under its leading edge; along
/// an upscaled axis it marks every reference pixel it overlaps (a
/// 300 px source at 1536 covers 5-6 reference px per source px).
fn footprint(foreground: &RgbaImage, out_w: u32, out_h: u32, dilate: usize) -> Vec<u8> {
    let (w, h) = (foreground.width as u64, foreground.height as u64);
    // Half-open reference span covered by source index i of n at size out.
    let span = |i: u64, n: u64, out: u64| -> (usize, usize) {
        let a = i * out / n;
        let b = if out > n {
            ((i + 1) * out).div_ceil(n)
        } else {
            a + 1
        };
        (a as usize, b.min(out) as usize)
    };
    let ow = out_w as usize;
    let cols: Vec<(usize, usize)> = (0..w).map(|x| span(x, w, out_w as u64)).collect();
    let mut hole = vec![0u8; (out_w * out_h) as usize];
    for y in 0..h {
        let (ry0, ry1) = span(y, h, out_h as u64);
        for x in 0..w {
            if foreground.pixels[((y * w + x) * 4 + 3) as usize] > 0 {
                let (rx0, rx1) = cols[x as usize];
                for ry in ry0..ry1 {
                    hole[ry * ow + rx0..ry * ow + rx1].fill(1);
                }
            }
        }
    }
    dilate_square(&hole, ow, out_h as usize, dilate)
}

/// The source with every visible product pixel (alpha > 0) replaced by
/// opaque `REFERENCE_GREY` BEFORE any resize: the reference is resized
/// from this copy, so no product value can reach it whatever the
/// resampling support. (The dilated footprint then also keeps this
/// constant out of the ring the inpaint reads.)
fn product_blanked(source: &RgbaImage, foreground: &RgbaImage) -> RgbaImage {
    let mut blanked = source.clone();
    let stand_in = [REFERENCE_GREY[0], REFERENCE_GREY[1], REFERENCE_GREY[2], 255];
    for (p, f) in blanked
        .pixels
        .chunks_exact_mut(4)
        .zip(foreground.pixels.chunks_exact(4))
    {
        if f[3] > 0 {
            p.copy_from_slice(&stand_in);
        }
    }
    blanked
}

/// Shared by both fills: the product-blanked source resized to
/// (out_w, out_h) with Lanczos, the dilated footprint, and its
/// dilation.
fn reference_base(
    source: &RgbaImage,
    foreground: &RgbaImage,
    out_w: u32,
    out_h: u32,
) -> (RgbaImage, Vec<u8>, usize) {
    let dilate = reference_dilate_px(source.width, source.height, out_w, out_h);
    let mut reference = resize_rgba(
        &product_blanked(source, foreground),
        out_w,
        out_h,
        ResampleFilter::Lanczos,
    );
    reference.icc_profile = None;
    let hole = footprint(foreground, out_w, out_h, dilate);
    (reference, hole, dilate)
}

/// The legacy edit reference (`AGENTHAND_REFERENCE_FILL=grey`): the
/// source at (out_w, out_h) with the product footprint plus its
/// dilation (`reference_dilate_px`) filled `REFERENCE_GREY`. The edit
/// model reads that object-shaped grey silhouette as a shadow and
/// paints it (docs/REFERENCE-FILL.md), so it is kept for comparison
/// only. Returns (reference, hole pixel count).
pub fn masked_reference(
    source: &RgbaImage,
    foreground: &RgbaImage,
    out_w: u32,
    out_h: u32,
) -> (RgbaImage, u64) {
    let (mut reference, hole, _) = reference_base(source, foreground, out_w, out_h);
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

/// How the product hole in the edit reference is filled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceFill {
    /// Default: harmonic fill from the non-product pixels plus a faint
    /// copy of their fine texture. No silhouette.
    InpaintDiffusion,
    /// Legacy flat `REFERENCE_GREY` (comparison only).
    Grey,
}

impl ReferenceFill {
    /// `grey` / `gray` (any case) selects the legacy fill; anything
    /// else, or nothing, is the inpaint.
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(|v| v.trim().to_ascii_lowercase()) {
            Some(v) if v == "grey" || v == "gray" => ReferenceFill::Grey,
            _ => ReferenceFill::InpaintDiffusion,
        }
    }

    /// From `AGENTHAND_REFERENCE_FILL`.
    pub fn from_env() -> Self {
        Self::parse(std::env::var(REFERENCE_FILL_ENV).ok().as_deref())
    }

    /// The receipt's `fill` value.
    pub fn name(self) -> &'static str {
        match self {
            ReferenceFill::InpaintDiffusion => "inpaint_diffusion",
            ReferenceFill::Grey => "grey",
        }
    }
}

/// The image sent to the edit model and what the receipt says about it.
#[derive(Debug, Clone)]
pub struct EditReference {
    pub image: RgbaImage,
    /// Reference pixels in the dilated product footprint.
    pub hole_pixels: u64,
    /// The footprint's dilation in reference px (`reference_dilate_px`:
    /// 8, wider for upscaled small sources).
    pub dilate_px: usize,
    pub fill: ReferenceFill,
    /// Total Gauss-Seidel sweeps over all pyramid levels (0 for grey,
    /// or when nothing outside the footprint was left to fill from).
    pub fill_iterations: u64,
}

impl EditReference {
    /// The fill fields of the receipt's `backdrop.reference` block
    /// (additive: `fill_rgb` stays, `null` for the inpaint), plus the
    /// dilation actually used: the server merges these over its own
    /// keys, so `dilate_px` reports 18 for a 300x225 source, not 8.
    pub fn fill_record(&self) -> Value {
        match self.fill {
            ReferenceFill::Grey => json!({
                "fill": self.fill.name(),
                "fill_rgb": REFERENCE_GREY,
                "fill_iterations": 0,
                "fill_texture": Value::Null,
                "dilate_px": self.dilate_px,
            }),
            ReferenceFill::InpaintDiffusion => json!({
                "fill": self.fill.name(),
                "fill_rgb": Value::Null,
                "fill_iterations": self.fill_iterations,
                "dilate_px": self.dilate_px,
                "fill_texture": {
                    "kind": "row_mirror_residual",
                    "radius_px": INPAINT_TEXTURE_RADIUS,
                    "clamp": INPAINT_TEXTURE_CLAMP,
                    "gain": INPAINT_TEXTURE_GAIN,
                },
            }),
        }
    }

    /// The receipt's `product_region` sentence for this fill.
    pub fn product_region_note(&self) -> &'static str {
        match self.fill {
            ReferenceFill::Grey => "every visible product pixel's footprint plus the dilation was filled grey before upload; no product pixel was sent",
            ReferenceFill::InpaintDiffusion => "every visible product pixel's footprint plus the dilation was filled from the photo pixels outside it before upload; no product pixel was sent",
        }
    }
}

/// The edit reference for `fill`: the source at (out_w, out_h) with
/// the product footprint plus its dilation replaced. No product pixel
/// reaches the image, at any source size:
/// - the reference is resized from a copy of the source whose product
///   pixels (alpha > 0) are already the constant `REFERENCE_GREY`;
/// - the dilation (`reference_dilate_px`) covers the Lanczos support
///   (3 output px when downscaling, 3 x upscale when upscaling), so
///   that constant does not reach the ring outside the footprint;
/// - the inpaint reads ONLY reference pixels outside the footprint.
pub fn edit_reference(
    source: &RgbaImage,
    foreground: &RgbaImage,
    out_w: u32,
    out_h: u32,
    fill: ReferenceFill,
) -> EditReference {
    let dilate_px = reference_dilate_px(source.width, source.height, out_w, out_h);
    if fill == ReferenceFill::Grey {
        let (image, hole_pixels) = masked_reference(source, foreground, out_w, out_h);
        return EditReference {
            image,
            hole_pixels,
            dilate_px,
            fill,
            fill_iterations: 0,
        };
    }
    let (mut reference, hole, _) = reference_base(source, foreground, out_w, out_h);
    let (w, h) = (out_w as usize, out_h as usize);
    let known: Vec<bool> = hole.iter().map(|&v| v == 0).collect();
    let hole_pixels = known.iter().filter(|&&k| !k).count() as u64;
    let mut fill_iterations = 0u64;
    if hole_pixels > 0 {
        let mut rgb: Vec<[f32; 3]> = reference
            .pixels
            .chunks_exact(4)
            .zip(&known)
            .map(|(p, &k)| {
                // Hole values are never read: zero them so the fill
                // provably starts from nothing inside the footprint.
                if k {
                    [p[0] as f32, p[1] as f32, p[2] as f32]
                } else {
                    [0.0; 3]
                }
            })
            .collect();
        if known.iter().any(|&k| k) {
            let texture = mirrored_texture(&rgb, &known, w, h);
            fill_iterations = diffuse_fill(&mut rgb, &known, w, h);
            for (i, t) in texture.iter().enumerate() {
                if !known[i] {
                    (0..3).for_each(|c| rgb[i][c] += t[c]);
                }
            }
        } else {
            // The footprint covers the whole frame: nothing outside to
            // fill from, so the hole is the neutral grey.
            rgb.iter_mut()
                .for_each(|p| *p = REFERENCE_GREY.map(|v| v as f32));
        }
        for (i, px) in reference.pixels.chunks_exact_mut(4).enumerate() {
            if !known[i] {
                for c in 0..3 {
                    px[c] = rgb[i][c].round().clamp(0.0, 255.0) as u8;
                }
            }
        }
    }
    reference
        .pixels
        .chunks_exact_mut(4)
        .for_each(|p| p[3] = 255);
    EditReference {
        image: reference,
        hole_pixels,
        dilate_px,
        fill,
        fill_iterations,
    }
}

/// One pyramid level of the inpaint.
struct FillLevel {
    rgb: Vec<[f32; 3]>,
    known: Vec<bool>,
    w: usize,
    h: usize,
}

/// Harmonic (Laplace) fill of the unknown pixels from the known ones,
/// coarse to fine: known-weighted 2x2 means build the pyramid; the
/// coarsest level starts from the known mean; each finer level starts
/// its unknowns from the bilinear upsample of the level below; every
/// level runs over-relaxed Gauss-Seidel (4-neighbour mean, known
/// pixels fixed, image borders reflecting). Reads nothing but known
/// pixels. Returns the total sweeps.
fn diffuse_fill(rgb: &mut [[f32; 3]], known: &[bool], w: usize, h: usize) -> u64 {
    let mut levels = vec![FillLevel {
        rgb: rgb.to_vec(),
        known: known.to_vec(),
        w,
        h,
    }];
    loop {
        let fine = levels.last().expect("non-empty");
        if fine.w.max(fine.h) <= INPAINT_COARSE_LONG_SIDE || fine.known.iter().all(|&k| k) {
            break;
        }
        let (cw, ch) = ((fine.w + 1) / 2, (fine.h + 1) / 2);
        let mut coarse = FillLevel {
            rgb: vec![[0.0; 3]; cw * ch],
            known: vec![false; cw * ch],
            w: cw,
            h: ch,
        };
        for cy in 0..ch {
            for cx in 0..cw {
                let (mut sum, mut n) = ([0f32; 3], 0f32);
                for y in 2 * cy..(2 * cy + 2).min(fine.h) {
                    for x in 2 * cx..(2 * cx + 2).min(fine.w) {
                        if fine.known[y * fine.w + x] {
                            (0..3).for_each(|c| sum[c] += fine.rgb[y * fine.w + x][c]);
                            n += 1.0;
                        }
                    }
                }
                if n > 0.0 {
                    coarse.rgb[cy * cw + cx] = sum.map(|s| s / n);
                    coarse.known[cy * cw + cx] = true;
                }
            }
        }
        levels.push(coarse);
    }
    let mut total = 0u64;
    let mut below: Option<FillLevel> = None;
    while let Some(mut level) = levels.pop() {
        match &below {
            None => {
                let (mut sum, mut n) = ([0f64; 3], 0f64);
                for (p, &k) in level.rgb.iter().zip(&level.known) {
                    if k {
                        (0..3).for_each(|c| sum[c] += p[c] as f64);
                        n += 1.0;
                    }
                }
                let mean = if n > 0.0 {
                    sum.map(|s| (s / n) as f32)
                } else {
                    REFERENCE_GREY.map(|v| v as f32)
                };
                for (p, &k) in level.rgb.iter_mut().zip(&level.known) {
                    if !k {
                        *p = mean;
                    }
                }
            }
            Some(c) => {
                for y in 0..level.h {
                    for x in 0..level.w {
                        let i = y * level.w + x;
                        if !level.known[i] {
                            level.rgb[i] = bilinear(
                                &c.rgb,
                                c.w,
                                c.h,
                                (x as f32 + 0.5) / 2.0 - 0.5,
                                (y as f32 + 0.5) / 2.0 - 0.5,
                            );
                        }
                    }
                }
            }
        }
        total += gauss_seidel(&mut level.rgb, &level.known, level.w, level.h);
        below = Some(level);
    }
    rgb.copy_from_slice(&below.expect("level 0 solved").rgb);
    total
}

/// Bilinear sample with clamped edges.
fn bilinear(rgb: &[[f32; 3]], w: usize, h: usize, fx: f32, fy: f32) -> [f32; 3] {
    let fx = fx.clamp(0.0, (w - 1) as f32);
    let fy = fy.clamp(0.0, (h - 1) as f32);
    let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
    let mut out = [0f32; 3];
    for (c, o) in out.iter_mut().enumerate() {
        let top = rgb[y0 * w + x0][c] * (1.0 - tx) + rgb[y0 * w + x1][c] * tx;
        let bottom = rgb[y1 * w + x0][c] * (1.0 - tx) + rgb[y1 * w + x1][c] * tx;
        *o = top * (1.0 - ty) + bottom * ty;
    }
    out
}

/// Over-relaxed Gauss-Seidel on the unknown pixels (row-major order):
/// each moves toward the mean of its in-image 4-neighbours. Stops at
/// `INPAINT_MAX_SWEEPS` or when the largest move in a sweep is under
/// `INPAINT_TOLERANCE`. Returns the sweeps run.
fn gauss_seidel(rgb: &mut [[f32; 3]], known: &[bool], w: usize, h: usize) -> u64 {
    let unknown: Vec<usize> = (0..w * h).filter(|&i| !known[i]).collect();
    if unknown.is_empty() || w * h < 2 {
        return 0;
    }
    let mut sweeps = 0u64;
    while (sweeps as usize) < INPAINT_MAX_SWEEPS {
        sweeps += 1;
        let mut max_move = 0f32;
        for &i in &unknown {
            let (x, y) = (i % w, i / w);
            let (mut acc, mut n) = ([0f32; 3], 0f32);
            let mut add = |j: usize| {
                (0..3).for_each(|c| acc[c] += rgb[j][c]);
                n += 1.0;
            };
            if x > 0 {
                add(i - 1);
            }
            if x + 1 < w {
                add(i + 1);
            }
            if y > 0 {
                add(i - w);
            }
            if y + 1 < h {
                add(i + w);
            }
            for c in 0..3 {
                let step = INPAINT_SOR * (acc[c] / n - rgb[i][c]);
                rgb[i][c] += step;
                max_move = max_move.max(step.abs());
            }
        }
        if max_move < INPAINT_TOLERANCE {
            break;
        }
    }
    sweeps
}

/// The texture added over the harmonic fill: the known pixels'
/// residual against their known-only box mean (radius
/// `INPAINT_TEXTURE_RADIUS`), clamped to +-`INPAINT_TEXTURE_CLAMP`;
/// each unknown pixel of a row's hole run [a, b) takes the residual
/// mirrored across both run ends (x -> 2a-1-x and 2b-1-x, used only
/// where that pixel is known), cross-faded by distance to each end
/// (variance-normalised), times `INPAINT_TEXTURE_GAIN`. Mirroring
/// keeps the texture continuous at the run ends and on the same image
/// row (the same depth on a ground plane). Zero where nothing known
/// is reachable. Reads nothing but known pixels.
fn mirrored_texture(rgb: &[[f32; 3]], known: &[bool], w: usize, h: usize) -> Vec<[f32; 3]> {
    let r = INPAINT_TEXTURE_RADIUS;
    // Known-only box sums, separable: horizontal then vertical.
    let mut hsum = vec![[0f32; 4]; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut s = [0f32; 4];
            for xx in x.saturating_sub(r)..(x + r + 1).min(w) {
                let j = y * w + xx;
                if known[j] {
                    (0..3).for_each(|c| s[c] += rgb[j][c]);
                    s[3] += 1.0;
                }
            }
            hsum[y * w + x] = s;
        }
    }
    let mut residual = vec![[0f32; 3]; w * h];
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if !known[i] {
                continue;
            }
            let mut s = [0f32; 4];
            for yy in y.saturating_sub(r)..(y + r + 1).min(h) {
                let t = hsum[yy * w + x];
                (0..4).for_each(|c| s[c] += t[c]);
            }
            for c in 0..3 {
                residual[i][c] =
                    (rgb[i][c] - s[c] / s[3]).clamp(-INPAINT_TEXTURE_CLAMP, INPAINT_TEXTURE_CLAMP);
            }
        }
    }
    drop(hsum);
    let mut texture = vec![[0f32; 3]; w * h];
    for y in 0..h {
        let row = y * w;
        let mut x = 0;
        while x < w {
            if known[row + x] {
                x += 1;
                continue;
            }
            let a = x;
            while x < w && !known[row + x] {
                x += 1;
            }
            let b = x;
            for xi in a..b {
                let (dl, dr) = ((xi - a + 1) as f32, (b - xi) as f32);
                let left = (2 * a)
                    .checked_sub(xi + 1)
                    .filter(|&xl| known[row + xl])
                    .map(|xl| (residual[row + xl], dr / (dl + dr)));
                let right = Some(2 * b - 1 - xi)
                    .filter(|&xr| xr < w && known[row + xr])
                    .map(|xr| (residual[row + xr], dl / (dl + dr)));
                let (mut acc, mut norm) = ([0f32; 3], 0f32);
                for (res, wt) in [left, right].into_iter().flatten() {
                    (0..3).for_each(|c| acc[c] += res[c] * wt);
                    norm += wt * wt;
                }
                if norm > 0.0 {
                    let scale = INPAINT_TEXTURE_GAIN / norm.sqrt();
                    texture[row + xi] = acc.map(|v| v * scale);
                }
            }
        }
    }
    texture
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

// ---------------------------------------------------------------------------
// Light direction (docs/LIGHT-AND-THIN.md). Same discipline as the shot
// angle: a direction is claimed only when the source photo shows it
// plainly (a cast shadow, else a broad brightness gradient across the
// ground); otherwise nothing is stated.
// ---------------------------------------------------------------------------

/// The source is averaged over square cells so its long side is at most
/// this many cells.
pub const LIGHT_GRID_LONG_SIDE: u32 = 256;
/// A cast-shadow cell is darker than the ground's median luma by a
/// ratio in [MIN, MAX] ...
pub const LIGHT_SHADOW_RATIO_MIN: f64 = 0.20;
pub const LIGHT_SHADOW_RATIO_MAX: f64 = 0.72;
/// ... and hue-neutral: its chromaticity (r and g shares of r+g+b)
/// within this of the ground's median.
pub const LIGHT_CHROMA_TOL: f64 = 0.05;
/// Seeds: shadow cells within this many cells of the product's lower
/// half (the region must touch the product where it meets the ground).
pub const LIGHT_SEED_REACH_CELLS: usize = 2;
/// The region covers at least this share of the product bbox area ...
pub const LIGHT_MIN_AREA_FRACTION: f64 = 0.03;
/// ... at most this share of the ground region (a dark ground is not a
/// shadow) ...
pub const LIGHT_MAX_GROUND_FRACTION: f64 = 0.5;
/// ... and fills at least this share of its own bounding box (a network
/// of dark paving joints does not).
pub const LIGHT_MIN_FILL: f64 = 0.30;
/// Shadow centroid offset from the contact point (bbox bottom centre),
/// in product bbox widths (x) and heights (y): a side shadow is at least
/// LIGHT_SIDE_MIN off-centre and more sideways than deep; a front/back
/// shadow at least LIGHT_DEPTH_MIN deep; an overhead shadow within
/// LIGHT_OVERHEAD_MAX on both axes. Anything else is inconclusive.
pub const LIGHT_SIDE_MIN: f64 = 0.15;
pub const LIGHT_DEPTH_MIN: f64 = 0.10;
pub const LIGHT_OVERHEAD_MAX: f64 = 0.06;
/// Gradient fallback: the ground's left and right thirds differ by at
/// least this share of the ground median, the middle third between
/// them, over a ground region of at least LIGHT_GRADIENT_MIN_GROUND of
/// the frame.
pub const LIGHT_GRADIENT_MIN: f64 = 0.20;
pub const LIGHT_GRADIENT_MIN_GROUND: f64 = 0.04;

/// Where the light comes from, read off the photo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LightSide {
    /// Shadow falls to camera-right.
    FromLeft,
    /// Shadow falls to camera-left.
    FromRight,
    /// From behind the camera: the shadow falls behind the product.
    FromCamera,
    /// From behind the product: the shadow falls toward the camera.
    FromBehind,
    /// High light: a short shadow straight under the product.
    Overhead,
}

impl LightSide {
    pub fn bucket(self) -> &'static str {
        match self {
            LightSide::FromLeft => "from_camera_left",
            LightSide::FromRight => "from_camera_right",
            LightSide::FromCamera => "from_behind_camera",
            LightSide::FromBehind => "from_behind_subject",
            LightSide::Overhead => "overhead",
        }
    }

    /// The side the shadow falls to.
    pub fn shadow_side(self) -> &'static str {
        match self {
            LightSide::FromLeft => "camera-right",
            LightSide::FromRight => "camera-left",
            LightSide::FromCamera => "behind",
            LightSide::FromBehind => "toward_camera",
            LightSide::Overhead => "beneath",
        }
    }

    /// Unit step the shadow falls along in the frame (y grows downward,
    /// so -1 is behind the product, +1 toward the camera).
    pub fn shadow_step(self) -> (i8, i8) {
        match self {
            LightSide::FromLeft => (1, 0),
            LightSide::FromRight => (-1, 0),
            LightSide::FromCamera => (0, -1),
            LightSide::FromBehind => (0, 1),
            LightSide::Overhead => (0, 0),
        }
    }
}

/// The light estimate and why. `side` is Some only when conclusive.
#[derive(Debug, Clone, PartialEq)]
pub struct LightEstimate {
    pub side: Option<LightSide>,
    /// "cast_shadow" | "gradient" | "none".
    pub method: &'static str,
    pub reason: &'static str,
    pub signals: Value,
}

fn cell_luma(rgb: [f64; 3]) -> f64 {
    0.299 * rgb[0] + 0.587 * rgb[1] + 0.114 * rgb[2]
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() / 2]
}

/// Estimate the source photo's light direction (pure; source and
/// foreground share the frame). Cast shadow first: a darker, hue-neutral
/// connected region of the ground beside the product's lower half,
/// outside the mask, read on a coarse cell grid; its centroid relative
/// to the contact point names the side. Otherwise the left/right
/// brightness gradient across the ground. Otherwise nothing.
pub fn estimate_light_direction(source: &RgbaImage, foreground: &RgbaImage) -> LightEstimate {
    let none = |reason: &'static str, signals: Value| LightEstimate {
        side: None,
        method: "none",
        reason,
        signals,
    };
    if (source.width, source.height) != (foreground.width, foreground.height) {
        return none(
            "source and foreground frames differ; light not estimated",
            Value::Null,
        );
    }
    let Some((x0, y0, x1, y1)) = opaque_bbox(foreground) else {
        return none("no product bbox; light not estimated", Value::Null);
    };
    let (w, h) = (source.width as usize, source.height as usize);
    let (x0, y0, x1, y1) = (x0 as usize, y0 as usize, x1 as usize, y1 as usize);
    let (bw, bh) = (x1 - x0, y1 - y0);
    let cell = ((w.max(h) as u32).div_ceil(LIGHT_GRID_LONG_SIDE)).max(1) as usize;
    let (gw, gh) = (w.div_ceil(cell), h.div_ceil(cell));
    let mut sum = vec![[0f64; 3]; gw * gh];
    let mut count = vec![0u32; gw * gh];
    let mut product = vec![0u8; gw * gh];
    for y in 0..h {
        let row = (y / cell) * gw;
        for x in 0..w {
            let i = (y * w + x) * 4;
            let g = row + x / cell;
            if foreground.pixels[i + 3] > 0 {
                product[g] = 1;
            } else {
                for c in 0..3 {
                    sum[g][c] += source.pixels[i + c] as f64;
                }
                count[g] += 1;
            }
        }
    }
    let product = dilate_square(&product, gw, gh, 1);
    let mean = |g: usize| -> [f64; 3] {
        let n = count[g].max(1) as f64;
        [sum[g][0] / n, sum[g][1] / n, sum[g][2] / n]
    };
    let chroma = |rgb: [f64; 3]| -> [f64; 2] {
        let s = (rgb[0] + rgb[1] + rgb[2]).max(1.0);
        [rgb[0] / s, rgb[1] / s]
    };
    // Ground region: from the product's vertical middle to the bottom of
    // the frame, one product width either side.
    let gy0 = ((y0 + y1) / 2) / cell;
    let gx0 = x0.saturating_sub(bw) / cell;
    let gx1 = ((x1 + bw).min(w - 1)) / cell + 1;
    let in_ground = |g: usize| -> bool { g / gw >= gy0 && g % gw >= gx0 && g % gw < gx1.min(gw) };
    let ground: Vec<usize> = (0..gw * gh)
        .filter(|&g| in_ground(g) && product[g] == 0 && count[g] > 0)
        .collect();
    if ground.len() < 16 {
        return none(
            "too little ground beside the product; light not estimated",
            Value::Null,
        );
    }
    let ref_luma = median(ground.iter().map(|&g| cell_luma(mean(g))).collect());
    let ref_chroma = [
        median(ground.iter().map(|&g| chroma(mean(g))[0]).collect()),
        median(ground.iter().map(|&g| chroma(mean(g))[1]).collect()),
    ];
    let mut signals = json!({
        "grid_cell_px": cell,
        "ground_cells": ground.len(),
        "ground_median_luma": crate::round6(ref_luma),
    });
    if ref_luma < 20.0 {
        return none(
            "ground too dark to read a shadow; light not estimated",
            signals,
        );
    }
    let candidate = |g: usize| -> bool {
        if !in_ground(g) || product[g] != 0 || count[g] == 0 {
            return false;
        }
        let rgb = mean(g);
        let ratio = cell_luma(rgb) / ref_luma;
        let c = chroma(rgb);
        (LIGHT_SHADOW_RATIO_MIN..=LIGHT_SHADOW_RATIO_MAX).contains(&ratio)
            && (c[0] - ref_chroma[0]).abs() <= LIGHT_CHROMA_TOL
            && (c[1] - ref_chroma[1]).abs() <= LIGHT_CHROMA_TOL
    };
    // Seeds: candidates near the product's lower half.
    let mut lower = vec![0u8; gw * gh];
    for g in 0..gw * gh {
        if product[g] != 0 && g / gw >= gy0 {
            lower[g] = 1;
        }
    }
    let near_lower = dilate_square(&lower, gw, gh, LIGHT_SEED_REACH_CELLS);
    let mut in_region = vec![false; gw * gh];
    let mut stack: Vec<usize> = (0..gw * gh)
        .filter(|&g| near_lower[g] != 0 && candidate(g))
        .collect();
    for &g in &stack {
        in_region[g] = true;
    }
    let mut region = Vec::new();
    while let Some(g) = stack.pop() {
        region.push(g);
        let (gx, gy) = (g % gw, g / gw);
        let mut visit = |n: usize| {
            if !in_region[n] && candidate(n) {
                in_region[n] = true;
                stack.push(n);
            }
        };
        if gx > 0 {
            visit(g - 1);
        }
        if gx + 1 < gw {
            visit(g + 1);
        }
        if gy > 0 {
            visit(g - gw);
        }
        if gy + 1 < gh {
            visit(g + gw);
        }
    }
    let bbox_area = (bw * bh).max(1) as f64;
    let area_px = (region.len() * cell * cell) as f64;
    let (contact_x, contact_y) = ((x0 + x1) as f64 / 2.0, y1 as f64);
    let mut cast: Option<LightSide> = None;
    if !region.is_empty() {
        let (mut rx0, mut ry0, mut rx1, mut ry1) = (usize::MAX, usize::MAX, 0, 0);
        let (mut sx, mut sy) = (0f64, 0f64);
        for &g in &region {
            let (gx, gy) = (g % gw, g / gw);
            rx0 = rx0.min(gx);
            ry0 = ry0.min(gy);
            rx1 = rx1.max(gx + 1);
            ry1 = ry1.max(gy + 1);
            sx += (gx as f64 + 0.5) * cell as f64;
            sy += (gy as f64 + 0.5) * cell as f64;
        }
        let n = region.len() as f64;
        let fill = n / ((rx1 - rx0) * (ry1 - ry0)) as f64;
        let nx = (sx / n - contact_x) / bw.max(1) as f64;
        let ny = (sy / n - contact_y) / bh.max(1) as f64;
        signals["shadow_area_fraction"] = json!(crate::round6(area_px / bbox_area));
        signals["shadow_fill"] = json!(crate::round6(fill));
        signals["shadow_offset_x"] = json!(crate::round6(nx));
        signals["shadow_offset_y"] = json!(crate::round6(ny));
        let sized = area_px / bbox_area >= LIGHT_MIN_AREA_FRACTION
            && n <= LIGHT_MAX_GROUND_FRACTION * ground.len() as f64;
        if sized && fill >= LIGHT_MIN_FILL {
            cast = if nx.abs() >= LIGHT_SIDE_MIN && nx.abs() >= ny.abs() {
                Some(if nx > 0.0 {
                    LightSide::FromLeft
                } else {
                    LightSide::FromRight
                })
            } else if ny <= -LIGHT_DEPTH_MIN && -ny > nx.abs() {
                Some(LightSide::FromCamera)
            } else if ny >= LIGHT_DEPTH_MIN && ny > nx.abs() {
                Some(LightSide::FromBehind)
            } else if nx.abs() <= LIGHT_OVERHEAD_MAX && ny.abs() <= LIGHT_OVERHEAD_MAX {
                Some(LightSide::Overhead)
            } else {
                None
            };
        }
    }
    if let Some(side) = cast {
        return LightEstimate {
            side: Some(side),
            method: "cast_shadow",
            reason: "a darker, hue-neutral region attached to the product's lower half; its centroid relative to the contact point names the side",
            signals,
        };
    }
    // Gradient fallback over the ground outside the shadow region.
    let span = (gx1.min(gw) - gx0).max(1);
    let mut thirds = [(0f64, 0u32); 3];
    for &g in &ground {
        if in_region[g] {
            continue;
        }
        let t = (((g % gw) - gx0) * 3 / span).min(2);
        thirds[t].0 += cell_luma(mean(g));
        thirds[t].1 += 1;
    }
    let ground_fraction = (ground.len() * cell * cell) as f64 / (w * h) as f64;
    if thirds.iter().all(|t| t.1 > 0) {
        let [l, m, r] = thirds.map(|t| t.0 / t.1 as f64);
        let rel = (r - l) / ref_luma;
        signals["gradient"] = json!(crate::round6(rel));
        let monotonic = (l < m && m < r) || (l > m && m > r);
        if rel.abs() >= LIGHT_GRADIENT_MIN
            && monotonic
            && ground_fraction >= LIGHT_GRADIENT_MIN_GROUND
        {
            return LightEstimate {
                side: Some(if rel > 0.0 {
                    LightSide::FromRight
                } else {
                    LightSide::FromLeft
                }),
                method: "gradient",
                reason: "no conclusive cast shadow; the ground brightens steadily toward one side",
                signals,
            };
        }
    }
    none(
        "no conclusive cast shadow and no clear brightness gradient across the ground; no light direction claimed",
        signals,
    )
}

// ---------------------------------------------------------------------------
// Thin-structure trigger (docs/LIGHT-AND-THIN.md): the product is an
// open-frame good (a bike, a chair, a rack) by its identify concept, or its
// mask encloses large transparent holes (wheel interiors, frame
// triangles). Extends `analyze_subject`'s thin-run flag, which a bike
// whose wheels SAM filled solid never trips.
// ---------------------------------------------------------------------------

/// Concept words that mark an open-frame product (a word or its plural,
/// as a whole token of the concept).
pub const THIN_CONCEPT_WORDS: [&str; 18] = [
    "bicycle",
    "bike",
    "bmx",
    "tricycle",
    "trike",
    "scooter",
    "wheel",
    "wheelchair",
    "stroller",
    "chair",
    "stool",
    "rack",
    "ladder",
    "frame",
    "easel",
    "tripod",
    "trellis",
    "cart",
];
/// Hole analysis grid: long side at most this many cells.
pub const THIN_WORK_LONG_SIDE: u32 = 1024;
/// An enclosed hole this large (share of the mask bbox area) triggers
/// the guard on its own.
pub const THIN_HOLE_TRIGGER_FRACTION: f64 = 0.005;
/// Enclosed holes from this size up are counted (and softened).
pub const THIN_HOLE_MIN_FRACTION: f64 = 0.0005;

/// The first open-frame word of a concept, if any.
pub fn thin_concept_word(concept: &str) -> Option<&'static str> {
    let lower = concept.to_ascii_lowercase();
    for token in lower.split(|c: char| !c.is_ascii_alphanumeric()) {
        if token.is_empty() {
            continue;
        }
        for word in THIN_CONCEPT_WORDS {
            if token == word || token.strip_suffix('s') == Some(word) {
                return Some(word);
            }
        }
    }
    None
}

/// Enclosed background components of a product grid (1 = product): the
/// non-product cells inside the product's bbox that no 4-connected path
/// of non-product cells joins to the bbox border.
#[derive(Debug, Clone)]
pub struct HoleMap {
    pub width: usize,
    pub height: usize,
    /// 0 outside every counted hole, else 1 + its index in `areas`.
    pub label: Vec<u32>,
    /// Cell areas of the counted holes (>= the min fraction).
    pub areas: Vec<usize>,
    /// The product bbox area in cells; fractions are of this.
    pub bbox_area: usize,
}

impl HoleMap {
    pub fn largest_fraction(&self) -> f64 {
        let largest = self.areas.iter().copied().max().unwrap_or(0);
        largest as f64 / self.bbox_area.max(1) as f64
    }
}

/// Find the enclosed holes of a product grid, counting those of at
/// least `min_fraction` of the product bbox area.
pub fn enclosed_holes(product: &[u8], width: usize, height: usize, min_fraction: f64) -> HoleMap {
    let mut map = HoleMap {
        width,
        height,
        label: vec![0; width * height],
        areas: Vec::new(),
        bbox_area: 0,
    };
    let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0usize, 0usize);
    for (i, &p) in product.iter().enumerate() {
        if p != 0 {
            let (x, y) = (i % width, i / width);
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x + 1);
            y1 = y1.max(y + 1);
        }
    }
    if x0 == usize::MAX {
        return map;
    }
    map.bbox_area = (x1 - x0) * (y1 - y0);
    // 0 = unvisited background, 1 = product, 2 = outside, 3+ = hole id.
    let mut state: Vec<u32> = product.iter().map(|&p| (p != 0) as u32).collect();
    let flood = |seeds: Vec<usize>, mark: u32, state: &mut Vec<u32>| -> usize {
        let mut stack = seeds;
        let mut area = 0usize;
        while let Some(i) = stack.pop() {
            area += 1;
            let (x, y) = (i % width, i / width);
            let mut push = |n: usize, stack: &mut Vec<usize>| {
                if state[n] == 0 {
                    state[n] = mark;
                    stack.push(n);
                }
            };
            if x > x0 {
                push(i - 1, &mut stack);
            }
            if x + 1 < x1 {
                push(i + 1, &mut stack);
            }
            if y > y0 {
                push(i - width, &mut stack);
            }
            if y + 1 < y1 {
                push(i + width, &mut stack);
            }
        }
        area
    };
    let mut border = Vec::new();
    for y in y0..y1 {
        for x in x0..x1 {
            if x == x0 || x + 1 == x1 || y == y0 || y + 1 == y1 {
                let i = y * width + x;
                if state[i] == 0 {
                    state[i] = 2;
                    border.push(i);
                }
            }
        }
    }
    flood(border, 2, &mut state);
    let min_area = ((min_fraction * map.bbox_area as f64).ceil() as usize).max(1);
    // Component id (3, 4, ...) -> counted label (1, 2, ...; 0 = too small).
    let mut id_label: Vec<u32> = Vec::new();
    for y in y0..y1 {
        for x in x0..x1 {
            let i = y * width + x;
            if state[i] == 0 {
                let id = 3 + id_label.len() as u32;
                state[i] = id;
                let area = flood(vec![i], id, &mut state);
                if area >= min_area {
                    map.areas.push(area);
                    id_label.push(map.areas.len() as u32);
                } else {
                    id_label.push(0);
                }
            }
        }
    }
    for y in y0..y1 {
        for x in x0..x1 {
            let i = y * width + x;
            if state[i] >= 3 {
                map.label[i] = id_label[(state[i] - 3) as usize];
            }
        }
    }
    map
}

/// The analysis grid for a frame: long side at most `long_side` cells.
pub fn work_grid(width: u32, height: u32, long_side: u32) -> (usize, usize) {
    let s = (long_side as f64 / width.max(height).max(1) as f64).min(1.0);
    (
        ((width as f64 * s).ceil() as usize).max(1),
        ((height as f64 * s).ceil() as usize).max(1),
    )
}

/// Forward-rasterise the visible product (alpha > 0) onto a `gw` x `gh`
/// grid over the frame: a cell is product when any product pixel falls
/// in it.
pub fn product_grid(foreground: &RgbaImage, gw: usize, gh: usize) -> Vec<u8> {
    let (w, h) = (foreground.width as usize, foreground.height as usize);
    let mut grid = vec![0u8; gw * gh];
    for y in 0..h {
        let gy = y * gh / h;
        for x in 0..w {
            if foreground.pixels[(y * w + x) * 4 + 3] > 0 {
                grid[gy * gw + x * gw / w] = 1;
            }
        }
    }
    grid
}

/// Why the thin-structure guard fires (or not).
#[derive(Debug, Clone, PartialEq)]
pub struct ThinTrigger {
    pub triggered: bool,
    /// "concept" | "holes" | "none".
    pub trigger: &'static str,
    /// Enclosed mask holes of at least THIN_HOLE_MIN_FRACTION.
    pub holes: usize,
    pub largest_hole_fraction: f64,
    pub concept_word: Option<&'static str>,
    /// Set when the guard is switched off.
    pub disabled: Option<String>,
}

impl ThinTrigger {
    pub fn disabled(reason: &str) -> Self {
        ThinTrigger {
            triggered: false,
            trigger: "none",
            holes: 0,
            largest_hole_fraction: 0.0,
            concept_word: None,
            disabled: Some(reason.to_string()),
        }
    }
}

/// Decide the thin-structure trigger from the identify concept (when
/// known) and the mask's enclosed holes.
pub fn thin_structure_trigger(foreground: &RgbaImage, concept: Option<&str>) -> ThinTrigger {
    let (gw, gh) = work_grid(foreground.width, foreground.height, THIN_WORK_LONG_SIDE);
    let grid = product_grid(foreground, gw, gh);
    let holes = enclosed_holes(&grid, gw, gh, THIN_HOLE_MIN_FRACTION);
    let largest = holes.largest_fraction();
    let concept_word = concept.and_then(thin_concept_word);
    let trigger = if concept_word.is_some() {
        "concept"
    } else if largest >= THIN_HOLE_TRIGGER_FRACTION {
        "holes"
    } else {
        "none"
    };
    ThinTrigger {
        triggered: trigger != "none",
        trigger,
        holes: holes.areas.len(),
        largest_hole_fraction: largest,
        concept_word,
        disabled: None,
    }
}

/// `finish_plate` with the contact shadow moved `offset` px (away from
/// the light; see `composite::contact_shadow_offset_px`). A zero offset
/// is exactly `finish_plate`.
pub fn finish_plate_lit(
    plate: &RgbaImage,
    foreground: &RgbaImage,
    geometry: &SubjectGeometry,
    offset: (f64, f64),
) -> RgbaImage {
    let shadowed = if geometry.shadow {
        crate::composite::apply_contact_shadow_offset(plate, foreground, offset)
    } else {
        plate.clone()
    };
    apply_edge_matte(&shadowed, foreground, geometry.matte_px)
}
