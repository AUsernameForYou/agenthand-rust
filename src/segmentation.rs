//! Segmentation quality control and the bounded mask-recovery loop —
//! port of the Python `segmentation.py`.
//!
//! - `assess_mask` quality-checks one decoded mask (coverage sane,
//!   dominated by one connected region).
//! - `extract_foreground` builds the transparent foreground: source
//!   pixels exactly where the mask is on, transparent black elsewhere.
//! - `union_masks` merges every SAM record for the phrase that clears
//!   the speck floor (several lawns, fence runs, patio pieces).
//! - `run_attempts` tries the concept phrase, retries ONCE with the
//!   first alternate, and traces every attempt.
//! - `EdgeField` / `decontaminate_edge_band` un-blend the original
//!   background out of a cut-out's soft edge band (docs/EDGE-FIX.md);
//!   product pixels (alpha 255) are never written.

use crate::image::{GrayImage, RgbaImage};
use crate::meta;
use crate::pricing::Usage;
use crate::sam::{parse_sam_masks, DecodedMask};
use serde_json::{json, Value};

/// Smallest mask coverage (fraction of the frame) that can be accepted.
pub const MIN_COVERAGE: f64 = 0.001;
/// Largest acceptable mask coverage; above it the "item" is the photo.
pub const MAX_COVERAGE: f64 = 0.98;
/// The largest connected component must hold at least this fraction
/// of the mask.
pub const MIN_DOMINANT_FRACTION: f64 = 0.75;

/// Outline color of the mask overlay (the logo cyan, #00AFE1).
pub const OVERLAY_OUTLINE_RGBA: [u8; 4] = [0, 175, 225, 255];

/// The SAM working-resolution cap (decisions 68/69). SAM is sent the
/// stored photo as-is up to this many pixels; past it, only the
/// SAM-bound copy is downscaled (`prepare_sam_png`) and the returned
/// mask is mapped back onto the full frame (`map_mask_to_frame`)
/// before anything is cut.
///
/// The number is the pipeline's proven envelope, not a guessed
/// provider limit: every still the room runs ever sent SAM was
/// 1206x1608 = 1,939,248 px (dozens of accepted calls across the
/// v1/v2, narrated, and acceptance runs in both builds, plus the M5
/// working-resolution pass), and every video frame was 720x1280,
/// below it. The failure this cap answers is measured too: the
/// 4284x5712 (24 MP) original of IMG_1340 drew HTTP 413 from SAM's
/// /responses endpoint (M5 live proof). The cap math below maps that
/// exact original back onto 1206x1608, the frame size already proven
/// live.
pub const SAM_MAX_PIXELS: u64 = 1_939_248; // 1206 * 1608

/// Mask provenance value: SAM saw the source frame itself, the mask
/// needed no mapping.
pub const MASK_SCALE_NONE: &str = "none";
/// Mask provenance value: SAM saw a downscaled copy and its mask was
/// mapped back onto the source frame by a bilinear resize
/// thresholded at 128 (bilinear on the soft mask + threshold, never
/// nearest on binary, so the mapped edge is a measured blend).
pub const MASK_SCALE_BILINEAR_THRESHOLD: &str = "bilinear_threshold_128";

/// The frame SAM sees for a source of (width, height): unchanged
/// when the source fits the working cap; otherwise the largest
/// same-aspect frame that fits it. Mirrors the Python
/// `sam_frame_size` exactly, including the decrement loop that keeps
/// the result inside the cap when float rounding lands a pixel over.
pub fn sam_frame_size(width: u32, height: u32, max_pixels: Option<u64>) -> (u32, u32) {
    let max_pixels = max_pixels.unwrap_or(SAM_MAX_PIXELS);
    if (width as u64) * (height as u64) <= max_pixels {
        return (width, height);
    }
    let scale = ((max_pixels as f64) / ((width as u64 * height as u64) as f64)).sqrt();
    let mut w = ((width as f64 * scale) as u32).max(1);
    let mut h = ((height as f64 * scale) as u32).max(1);
    while (w as u64) * (h as u64) > max_pixels {
        if w >= h {
            w -= 1;
        } else {
            h -= 1;
        }
    }
    (w, h)
}

/// The SAM-bound payload for one source photo.
pub struct SamPayload {
    /// What `segment_image` sends.
    pub png_bytes: Vec<u8>,
    /// The frame the returned masks are decoded against.
    pub sam_width: u32,
    pub sam_height: u32,
    /// The mask-provenance value for the receipt.
    pub scale_method: &'static str,
}

/// Build the SAM-bound payload for one source photo (port of the
/// Python `prepare_sam_png`). Inside the cap the stored bytes pass
/// through byte-identical, so the common path is exactly the
/// pre-scaling pipeline; past it, a Lanczos downscale of the decoded
/// source becomes the SAM copy. The stored photo itself is never
/// resized — the downscale exists only in this payload.
pub fn prepare_sam_png(
    source_image: &RgbaImage,
    stored_png_bytes: &[u8],
    max_pixels: Option<u64>,
) -> SamPayload {
    let (width, height) = (source_image.width, source_image.height);
    let (sam_width, sam_height) = sam_frame_size(width, height, max_pixels);
    if (sam_width, sam_height) == (width, height) {
        return SamPayload {
            png_bytes: stored_png_bytes.to_vec(),
            sam_width: width,
            sam_height: height,
            scale_method: MASK_SCALE_NONE,
        };
    }
    let small = crate::resample::resize_rgba(
        source_image,
        sam_width,
        sam_height,
        crate::resample::ResampleFilter::Lanczos,
    );
    SamPayload {
        png_bytes: crate::image::encode_png_rgba(&small),
        sam_width,
        sam_height,
        scale_method: MASK_SCALE_BILINEAR_THRESHOLD,
    }
}

/// Map a SAM-frame mask onto the full source frame (port of the
/// Python `map_mask_to_frame`). Identity when the sizes already
/// match. Otherwise a bilinear upscale of the (binary) mask,
/// thresholded at 128 back to binary: a full-frame pixel is on
/// exactly when its bilinear sample of the SAM mask is at least half
/// on. The foreground is then cut from the full-resolution source
/// through this mask, so the receipt binds full-resolution pixels,
/// not the working copy's.
pub fn map_mask_to_frame(mask: &GrayImage, frame_width: u32, frame_height: u32) -> GrayImage {
    if mask.width == frame_width && mask.height == frame_height {
        return mask.clone();
    }
    let soft = crate::resample::resize_gray(
        mask,
        frame_width,
        frame_height,
        crate::resample::ResampleFilter::Bilinear,
    );
    GrayImage {
        width: frame_width,
        height: frame_height,
        pixels: soft
            .pixels
            .iter()
            .map(|&v| if v >= 128 { 255 } else { 0 })
            .collect(),
        icc_profile: None,
    }
}

#[derive(Debug)]
pub struct CutOutError {
    pub code: String, // NO_MATCH | POOR_MASK | BAD_MODEL_RESPONSE
    pub message: String,
    pub attempts: Vec<Value>,
}

/// Quality verdict for one candidate mask; `accepted` is true only
/// when every gate passed.
pub struct MaskAssessment {
    pub mask_pixels: u64,
    pub coverage: f64,
    pub components: u64,
    pub dominant_fraction: f64,
    pub accepted: bool,
    pub rejection: Option<&'static str>,
}

/// Component sizes of a 0/1 raster, largest first (4-connectivity
/// flood fill).
fn component_sizes(binary: &[u8], width: usize, height: usize) -> Vec<u64> {
    let mut visited = vec![false; binary.len()];
    let mut sizes: Vec<u64> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    for start in 0..binary.len() {
        if binary[start] == 0 || visited[start] {
            continue;
        }
        let mut size = 0u64;
        stack.clear();
        stack.push(start);
        visited[start] = true;
        while let Some(i) = stack.pop() {
            size += 1;
            let x = i % width;
            if x > 0 && binary[i - 1] != 0 && !visited[i - 1] {
                visited[i - 1] = true;
                stack.push(i - 1);
            }
            if x < width - 1 && binary[i + 1] != 0 && !visited[i + 1] {
                visited[i + 1] = true;
                stack.push(i + 1);
            }
            if i >= width && binary[i - width] != 0 && !visited[i - width] {
                visited[i - width] = true;
                stack.push(i - width);
            }
            if i < binary.len() - width && binary[i + width] != 0 && !visited[i + width] {
                visited[i + width] = true;
                stack.push(i + width);
            }
        }
        sizes.push(size);
    }
    let _ = height;
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    sizes
}

/// Assess a mask against the coverage and dominance gates.
pub fn assess_mask(mask: &GrayImage) -> MaskAssessment {
    assess_mask_regions(mask, 1)
}

/// `assess_mask` for a union of `regions` SAM records: the dominance
/// gate counts the `regions` largest connected components together
/// (each record is one intended region), so two patio chairs or three
/// fence runs pass while a single record shattered into specks still
/// fails. `regions == 1` is exactly `assess_mask`.
pub fn assess_mask_regions(mask: &GrayImage, regions: usize) -> MaskAssessment {
    let width = mask.width as usize;
    let height = mask.height as usize;
    let frame_pixels = (width * height) as f64;
    let binary: Vec<u8> = mask
        .pixels
        .iter()
        .map(|&v| if v != 0 { 1 } else { 0 })
        .collect();
    let mask_pixels: u64 = binary.iter().map(|&v| v as u64).sum();
    let coverage = if frame_pixels > 0.0 {
        mask_pixels as f64 / frame_pixels
    } else {
        0.0
    };
    if mask_pixels == 0 {
        return MaskAssessment {
            mask_pixels: 0,
            coverage: 0.0,
            components: 0,
            dominant_fraction: 0.0,
            accepted: false,
            rejection: Some("empty"),
        };
    }
    let sizes = component_sizes(&binary, width, height);
    let components = sizes.len() as u64;
    let dominant_pixels: u64 = sizes.iter().take(regions.max(1)).sum();
    let dominant = dominant_pixels as f64 / mask_pixels as f64;
    let rejection = if coverage < MIN_COVERAGE {
        Some("empty")
    } else if coverage >= MAX_COVERAGE {
        Some("full_frame")
    } else if dominant < MIN_DOMINANT_FRACTION {
        Some("fragmented")
    } else {
        None
    };
    MaskAssessment {
        mask_pixels,
        coverage,
        components,
        dominant_fraction: dominant,
        accepted: rejection.is_none(),
        rejection,
    }
}

/// The source photo with the mask's outline drawn over it.
pub fn mask_overlay_image(source: &RgbaImage, mask: &GrayImage) -> Result<RgbaImage, String> {
    if mask.width != source.width || mask.height != source.height {
        return Err(format!(
            "Mask {}x{} does not cover the source frame {}x{}.",
            mask.width, mask.height, source.width, source.height
        ));
    }
    let width = mask.width as usize;
    let height = mask.height as usize;
    let on: Vec<bool> = mask.pixels.iter().map(|&v| v != 0).collect();
    let mut out = source.clone();
    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            if !on[i] {
                continue;
            }
            let boundary = x == 0
                || !on[i - 1]
                || x == width - 1
                || !on[i + 1]
                || y == 0
                || !on[i - width]
                || y == height - 1
                || !on[i + width];
            if boundary {
                let j = i * 4;
                out.pixels[j..j + 4].copy_from_slice(&OVERLAY_OUTLINE_RGBA);
            }
        }
    }
    Ok(out)
}

/// Sanity signals for one accepted mask (port of `review_mask`).
pub fn review_mask(mask: &GrayImage) -> Value {
    let width = mask.width as usize;
    let height = mask.height as usize;
    let on: Vec<bool> = mask.pixels.iter().map(|&v| v != 0).collect();
    let mask_pixels: u64 = on.iter().filter(|&&v| v).count() as u64;
    let mut warnings: Vec<String> = Vec::new();
    let mut bbox: Option<[i64; 4]> = None;
    let mut bbox_fill: Option<f64> = None;
    let mut touches: Vec<&str> = Vec::new();
    if mask_pixels > 0 {
        let (mut x0, mut x1, mut y0, mut y1) = (usize::MAX, 0usize, usize::MAX, 0usize);
        for (i, &v) in on.iter().enumerate() {
            if v {
                let x = i % width;
                let y = i / width;
                x0 = x0.min(x);
                x1 = x1.max(x);
                y0 = y0.min(y);
                y1 = y1.max(y);
            }
        }
        bbox = Some([x0 as i64, y0 as i64, (x1 + 1) as i64, (y1 + 1) as i64]);
        bbox_fill = Some(crate::round6(
            mask_pixels as f64 / ((x1 - x0 + 1) * (y1 - y0 + 1)) as f64,
        ));
        if x0 == 0 {
            touches.push("left");
        }
        if x1 == width - 1 {
            touches.push("right");
        }
        if y0 == 0 {
            touches.push("top");
        }
        if y1 == height - 1 {
            touches.push("bottom");
        }
        if touches.len() == 4 {
            warnings.push(
                "mask touches all four frame edges; this usually means the background was selected instead of the product"
                    .to_string(),
            );
        }
        if bbox_fill.map(|f| f < 0.25).unwrap_or(false) {
            warnings.push(
                "mask fills less than a quarter of its bounding box; the selection is scattered or frame-like, review the overlay before listing"
                    .to_string(),
            );
        }
    }
    json!({
        "mask_pixels": mask_pixels,
        "coverage": if width * height > 0 {
            crate::round6(mask_pixels as f64 / (width * height) as f64)
        } else {
            0.0
        },
        "bbox": bbox,
        "bbox_fill": bbox_fill,
        "touches_frame_edges": touches,
        "warnings": warnings,
    })
}

/// Intersection over union of two masks' on-pixels.
pub fn mask_iou(a: &GrayImage, b: &GrayImage) -> Result<f64, String> {
    if a.width != b.width || a.height != b.height {
        return Err(format!(
            "Mask sizes differ: {}x{} vs {}x{}.",
            a.width, a.height, b.width, b.height
        ));
    }
    let mut intersection = 0u64;
    let mut union = 0u64;
    for (x, y) in a.pixels.iter().zip(b.pixels.iter()) {
        let x_on = *x != 0;
        let y_on = *y != 0;
        if x_on && y_on {
            intersection += 1;
        }
        if x_on || y_on {
            union += 1;
        }
    }
    Ok(if union > 0 {
        intersection as f64 / union as f64
    } else {
        1.0
    })
}

/// The product layer: source pixels through the mask, nothing else.
/// Binary masks select exactly. A partial mask value keeps the
/// source RGB straight and scales only alpha (never produced by the
/// SAM path, whose rasters are binary by construction). The former
/// `Image.composite` blend against transparent black also scaled RGB,
/// a premultiplied colour stored as straight alpha, which composited
/// as a dark fringe; Python mirror: `putalpha(multiply(A, mask))`
/// with RGB zeroed only where the mask is 0.
pub fn extract_foreground(source: &RgbaImage, mask: &GrayImage) -> RgbaImage {
    assert_eq!(source.width, mask.width);
    assert_eq!(source.height, mask.height);
    let mut out = vec![0u8; source.pixels.len()];
    for i in 0..mask.pixels.len() {
        let m = mask.pixels[i] as u32;
        let j = i * 4;
        if m == 255 {
            out[j..j + 4].copy_from_slice(&source.pixels[j..j + 4]);
        } else if m > 0 {
            // Straight alpha: colour unchanged, coverage in alpha.
            out[j..j + 3].copy_from_slice(&source.pixels[j..j + 3]);
            out[j + 3] = ((source.pixels[j + 3] as u32 * m) / 255) as u8;
        }
    }
    RgbaImage {
        width: source.width,
        height: source.height,
        pixels: out,
        icc_profile: source.icc_profile.clone(),
    }
}

// ---------------------------------------------------------------------------
// Edge band decontamination (docs/EDGE-FIX.md). A cut-out's soft edge
// pixels (0 < alpha < 255) are blends of the product and the ORIGINAL
// background; over a darker scene that sliver of original light reads
// as a bright rim. The band is un-blended against the local original
// background (sampled from the source just outside the footprint) so it
// carries product colour only. Product pixels (alpha 255) are never
// read for writing and never written: the receipt's pixel set is the
// same set, byte for byte.
// ---------------------------------------------------------------------------

/// Distances in an `EdgeField` saturate here (chessboard px).
pub const EDGE_DIST_CAP: u8 = 32;
/// Margin around the footprint bbox that the edge field covers.
pub const EDGE_FIELD_MARGIN: usize = 16;
/// Original-background sample ring: source pixels outside the
/// footprint at this chessboard distance range (ring 1 is skipped: in
/// the photo it is itself part of the soft edge).
pub const BACKGROUND_RING: (u8, u8) = (2, 4);
/// Product-colour sample depth inside the product set (1 = boundary).
/// The boundary rings carry the original background's blend (and
/// SAM's staircase misfit), so the estimate prefers pixels 3-5 deep.
pub const PRODUCT_DEPTH: (u8, u8) = (3, 5);
/// Window radius for the local colour estimates (px), and the wider
/// fallback when the near window holds no sample.
pub const EDGE_SAMPLE_RADIUS: usize = 6;
pub const EDGE_SAMPLE_RADIUS_WIDE: usize = 12;

/// Distance maps of a foreground's alpha, over the footprint bbox grown
/// by `EDGE_FIELD_MARGIN` (window coordinates; `x0`/`y0` place the
/// window in the frame). Footprint = alpha > 0, product set = alpha 255.
pub struct EdgeField {
    pub x0: usize,
    pub y0: usize,
    pub w: usize,
    pub h: usize,
    pub frame_w: usize,
    pub frame_h: usize,
    /// Foreground alpha in the window.
    pub alpha: Vec<u8>,
    /// Chessboard distance to the footprint (0 on it).
    pub out_fp: Vec<u8>,
    /// Chessboard distance to the product set (0 on it).
    pub out_o: Vec<u8>,
    /// Depth inside the product set (1 = boundary pixel; 0 outside).
    /// Beyond the window counts as outside.
    pub in_o: Vec<u8>,
}

/// Exact chessboard distance (8-connected, unit steps) to the nearest
/// `target` pixel, two-pass chamfer, saturating at `EDGE_DIST_CAP`.
/// With `border_is_target`, the area beyond the window is target.
fn chessboard_distance(target: &[bool], w: usize, h: usize, border_is_target: bool) -> Vec<u8> {
    let cap = EDGE_DIST_CAP;
    let mut d: Vec<u8> = target.iter().map(|&t| if t { 0 } else { cap }).collect();
    if border_is_target {
        for y in 0..h {
            for x in 0..w {
                if (x == 0 || y == 0 || x == w - 1 || y == h - 1) && d[y * w + x] > 1 {
                    d[y * w + x] = 1;
                }
            }
        }
    }
    let step = |v: u8| v.saturating_add(1).min(cap);
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if d[i] == 0 {
                continue;
            }
            let mut m = d[i];
            if x > 0 {
                m = m.min(step(d[i - 1]));
            }
            if y > 0 {
                m = m.min(step(d[i - w]));
                if x > 0 {
                    m = m.min(step(d[i - w - 1]));
                }
                if x + 1 < w {
                    m = m.min(step(d[i - w + 1]));
                }
            }
            d[i] = m;
        }
    }
    for y in (0..h).rev() {
        for x in (0..w).rev() {
            let i = y * w + x;
            if d[i] == 0 {
                continue;
            }
            let mut m = d[i];
            if x + 1 < w {
                m = m.min(step(d[i + 1]));
            }
            if y + 1 < h {
                m = m.min(step(d[i + w]));
                if x + 1 < w {
                    m = m.min(step(d[i + w + 1]));
                }
                if x > 0 {
                    m = m.min(step(d[i + w - 1]));
                }
            }
            d[i] = m;
        }
    }
    d
}

impl EdgeField {
    /// None when the foreground has no visible pixel.
    pub fn new(foreground: &RgbaImage) -> Option<Self> {
        let (fw, fh) = (foreground.width as usize, foreground.height as usize);
        let (mut bx0, mut by0, mut bx1, mut by1) = (usize::MAX, usize::MAX, 0usize, 0usize);
        for y in 0..fh {
            for x in 0..fw {
                if foreground.pixels[(y * fw + x) * 4 + 3] > 0 {
                    bx0 = bx0.min(x);
                    by0 = by0.min(y);
                    bx1 = bx1.max(x + 1);
                    by1 = by1.max(y + 1);
                }
            }
        }
        if bx0 == usize::MAX {
            return None;
        }
        let m = EDGE_FIELD_MARGIN;
        let (x0, y0) = (bx0.saturating_sub(m), by0.saturating_sub(m));
        let (x1, y1) = ((bx1 + m).min(fw), (by1 + m).min(fh));
        let (w, h) = (x1 - x0, y1 - y0);
        let mut alpha = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                alpha[y * w + x] = foreground.pixels[((y0 + y) * fw + x0 + x) * 4 + 3];
            }
        }
        let fp: Vec<bool> = alpha.iter().map(|&a| a > 0).collect();
        let o: Vec<bool> = alpha.iter().map(|&a| a == 255).collect();
        let not_o: Vec<bool> = o.iter().map(|&v| !v).collect();
        let out_fp = chessboard_distance(&fp, w, h, false);
        let out_o = chessboard_distance(&o, w, h, false);
        let in_o = chessboard_distance(&not_o, w, h, true);
        Some(EdgeField {
            x0,
            y0,
            w,
            h,
            frame_w: fw,
            frame_h: fh,
            alpha,
            out_fp,
            out_o,
            in_o,
        })
    }

    /// Frame RGBA byte offset of window pixel (x, y).
    #[inline]
    pub fn frame_offset(&self, x: usize, y: usize) -> usize {
        ((self.y0 + y) * self.frame_w + self.x0 + x) * 4
    }

    /// Rounded mean RGB of `img` over the window pixels within
    /// Chebyshev radius `r` of (cx, cy) that satisfy `keep(window
    /// index)`; None when there are none. Integer math:
    /// `(sum + n/2) / n`.
    pub fn local_mean(
        &self,
        img: &RgbaImage,
        cx: usize,
        cy: usize,
        r: usize,
        keep: impl Fn(usize) -> bool,
    ) -> Option<[u8; 3]> {
        let (mut sum, mut n) = ([0u64; 3], 0u64);
        for y in cy.saturating_sub(r)..(cy + r + 1).min(self.h) {
            for x in cx.saturating_sub(r)..(cx + r + 1).min(self.w) {
                let i = y * self.w + x;
                if keep(i) {
                    let j = self.frame_offset(x, y);
                    for c in 0..3 {
                        sum[c] += img.pixels[j + c] as u64;
                    }
                    n += 1;
                }
            }
        }
        (n > 0).then(|| {
            [
                ((sum[0] + n / 2) / n) as u8,
                ((sum[1] + n / 2) / n) as u8,
                ((sum[2] + n / 2) / n) as u8,
            ]
        })
    }

    /// Local product colour near (cx, cy) from the product layer: the
    /// mean of product pixels `PRODUCT_DEPTH` deep, falling back to 2+
    /// deep, then any product pixel (thin parts have no depth), all in
    /// the `EDGE_SAMPLE_RADIUS` window.
    pub fn product_colour(&self, foreground: &RgbaImage, cx: usize, cy: usize) -> Option<[u8; 3]> {
        let r = EDGE_SAMPLE_RADIUS;
        let (lo, hi) = PRODUCT_DEPTH;
        self.local_mean(foreground, cx, cy, r, |i| {
            self.in_o[i] >= lo && self.in_o[i] <= hi
        })
        .or_else(|| self.local_mean(foreground, cx, cy, r, |i| self.in_o[i] >= 2))
        .or_else(|| self.local_mean(foreground, cx, cy, r, |i| self.in_o[i] >= 1))
    }

    /// Local ORIGINAL background colour near (cx, cy): the mean of the
    /// source photo over the `BACKGROUND_RING` outside the footprint,
    /// near window first, then the wide one.
    pub fn background_colour(&self, source: &RgbaImage, cx: usize, cy: usize) -> Option<[u8; 3]> {
        let (lo, hi) = BACKGROUND_RING;
        let ring = |i: usize| self.out_fp[i] >= lo && self.out_fp[i] <= hi;
        self.local_mean(source, cx, cy, EDGE_SAMPLE_RADIUS, ring)
            .or_else(|| self.local_mean(source, cx, cy, EDGE_SAMPLE_RADIUS_WIDE, ring))
    }
}

/// Un-blend one soft edge pixel: `observed = a*F + (1-a)*B`, so
/// `F = (observed*255 - (255-a)*B) / a`, clamped. Low-coverage pixels
/// amplify noise by 255/a, so the result leans on the local product
/// colour there: weight `min(255, 2a)` on the un-blend, the rest on
/// `product` (when known). Integer math throughout.
pub fn unblend_edge_pixel(
    observed: [u8; 3],
    alpha: u8,
    background: [u8; 3],
    product: Option<[u8; 3]>,
) -> [u8; 3] {
    if alpha == 0 {
        return product.unwrap_or(observed);
    }
    let a = alpha as i64;
    let w = (2 * a).min(255);
    let mut out = [0u8; 3];
    for c in 0..3 {
        let num = observed[c] as i64 * 255 - (255 - a) * background[c] as i64;
        let f = ((num + a / 2).div_euclid(a)).clamp(0, 255);
        out[c] = match product {
            Some(p) => ((f * w + p[c] as i64 * (255 - w) + 127) / 255) as u8,
            None => f as u8,
        };
    }
    out
}

/// The product layer with its soft edge band decontaminated: every
/// pixel with 0 < alpha < 255 keeps its alpha and gets the un-blended
/// colour (`unblend_edge_pixel` against the local original background
/// from `source`); product pixels (alpha 255) and transparent pixels
/// are copied unchanged. Returns (layer, edge band pixels,
/// decontaminated pixels). A band pixel with no background sample in
/// reach is left as it was. `source` must share the frame.
pub fn decontaminate_edge_band(
    source: &RgbaImage,
    foreground: &RgbaImage,
    field: Option<&EdgeField>,
) -> (RgbaImage, u64, u64) {
    assert_eq!(
        (source.width, source.height),
        (foreground.width, foreground.height)
    );
    let mut out = foreground.clone();
    let owned;
    let field = match field {
        Some(f) => f,
        None => match EdgeField::new(foreground) {
            Some(f) => {
                owned = f;
                &owned
            }
            None => return (out, 0, 0),
        },
    };
    let (mut band, mut fixed) = (0u64, 0u64);
    for y in 0..field.h {
        for x in 0..field.w {
            let a = field.alpha[y * field.w + x];
            if a == 0 || a == 255 {
                continue;
            }
            band += 1;
            let Some(bg) = field.background_colour(source, x, y) else {
                continue;
            };
            let product = field.product_colour(foreground, x, y);
            let j = field.frame_offset(x, y);
            let observed = [
                foreground.pixels[j],
                foreground.pixels[j + 1],
                foreground.pixels[j + 2],
            ];
            out.pixels[j..j + 3].copy_from_slice(&unblend_edge_pixel(observed, a, bg, product));
            fixed += 1;
        }
    }
    (out, band, fixed)
}

/// A SAM record joins the union only when it alone would clear the
/// coverage floor (`MIN_COVERAGE` of the frame); smaller records are
/// specks (a bolt, a leaf, a glint) and are dropped.
pub const REGION_MIN_COVERAGE: f64 = MIN_COVERAGE;

/// Every record for one phrase, merged.
pub struct MaskUnion {
    /// Full-frame 0/255 union of the kept records.
    pub image: Vec<u8>,
    /// Kept records' ids, largest first.
    pub object_ids: Vec<String>,
    /// Records dropped under the speck floor.
    pub dropped: usize,
    /// Half-open union of the kept records' bounds.
    pub bounds: (i64, i64, i64, i64),
}

/// Union every record that clears the speck floor (port target for
/// the Python `_union_masks`, replacing the largest-record `max()`).
/// When no record clears the floor, the largest record alone is
/// returned so the quality gate rejects it with its real numbers
/// instead of the attempt reading as a zero-match. `masks` must be
/// non-empty.
pub fn union_masks(masks: &[DecodedMask], frame_width: usize, frame_height: usize) -> MaskUnion {
    let frame_pixels = (frame_width * frame_height) as f64;
    let floor = ((REGION_MIN_COVERAGE * frame_pixels).ceil() as u64).max(1);
    let on_pixels = |m: &DecodedMask| m.image.iter().filter(|&&v| v != 0).count() as u64;
    let mut order: Vec<(u64, usize)> = masks
        .iter()
        .enumerate()
        .map(|(i, m)| (on_pixels(m), i))
        .collect();
    // Largest first; ties keep wire order (Python's stable sort).
    order.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut kept: Vec<usize> = order
        .iter()
        .filter(|(n, _)| *n >= floor)
        .map(|&(_, i)| i)
        .collect();
    let dropped = masks.len() - kept.len();
    if kept.is_empty() {
        kept.push(order[0].1);
    }
    let mut image = vec![0u8; frame_width * frame_height];
    let mut bounds = masks[kept[0]].bounds;
    for &i in &kept {
        let m = &masks[i];
        for (dst, &src) in image.iter_mut().zip(m.image.iter()) {
            if src != 0 {
                *dst = 255;
            }
        }
        bounds = (
            bounds.0.min(m.bounds.0),
            bounds.1.min(m.bounds.1),
            bounds.2.max(m.bounds.2),
            bounds.3.max(m.bounds.3),
        );
    }
    MaskUnion {
        image,
        object_ids: kept.iter().map(|&i| masks[i].object_id.clone()).collect(),
        dropped: if dropped == masks.len() {
            dropped - 1
        } else {
            dropped
        },
        bounds,
    }
}

/// Plain-words reason for a rejected mask. A non-empty mask under the
/// coverage floor carries the shared "empty" wire code; the text says
/// which it is.
pub fn rejection_reason(assessment: &MaskAssessment) -> String {
    match assessment.rejection {
        Some("empty") if assessment.mask_pixels > 0 => format!(
            "too small ({} px, under {}% of the frame), nothing worth staging",
            assessment.mask_pixels,
            MIN_COVERAGE * 100.0
        ),
        Some("empty") => "empty (SAM selected no pixels)".to_string(),
        Some("full_frame") => {
            "it covers the whole frame, so there is nothing around it to stage".to_string()
        }
        Some("fragmented") => "fragmented (no dominant region)".to_string(),
        Some(other) => other.to_string(),
        None => "not accepted".to_string(),
    }
}

/// The winning attempt's mask, the full attempt log, and billed usage.
pub struct AttemptsOutcome {
    pub mask_image: GrayImage,
    pub phrase: String,
    pub attempts: Vec<Value>,
    pub usage: Usage,
}

/// Run the bounded attempt loop. Performs network I/O; must only run
/// as the fn of `billed_call`.
pub fn run_attempts(
    client: &meta::MetaClient,
    key: &str,
    png_bytes: &[u8],
    phrases: &[String],
    model: &str,
    frame_width: u32,
    frame_height: u32,
) -> Result<AttemptsOutcome, CutOutError> {
    let mut attempts: Vec<Value> = Vec::new();
    let mut images = 0i64;
    let mut last_code = "NO_MATCH";
    // N28: the last rejected mask's full geometry rides into the
    // final error — the model output cannot be regenerated, so the
    // error text is the only durable copy of what was rejected.
    let mut last_rejection: Option<String> = None;
    for phrase in phrases {
        let (output_text, _usage) = meta::segment_image(client, key, png_bytes, phrase, model)
            .map_err(|e| CutOutError {
                code: "BAD_MODEL_RESPONSE".to_string(),
                message: e,
                attempts: attempts.clone(),
            })?;
        images += 1;
        let mut trace = json!({"phrase": phrase, "records": 0});
        let masks = match parse_sam_masks(&output_text, frame_width as usize, frame_height as usize)
        {
            Ok(m) => m,
            Err(exc) => {
                trace["outcome"] = json!("unparseable");
                attempts.push(trace);
                return Err(CutOutError {
                    code: "BAD_MODEL_RESPONSE".to_string(),
                    message: format!("SAM output for phrase {phrase:?} could not be used: {exc}"),
                    attempts,
                });
            }
        };
        trace["records"] = json!(masks.len());
        if masks.is_empty() {
            trace["outcome"] = json!("no_match");
            attempts.push(trace);
            last_code = "NO_MATCH";
            continue;
        }
        let chosen = union_masks(&masks, frame_width as usize, frame_height as usize);
        let mask_gray = GrayImage {
            width: frame_width,
            height: frame_height,
            pixels: chosen.image,
            icc_profile: None,
        };
        let assessment = assess_mask_regions(&mask_gray, chosen.object_ids.len());
        trace["object_id"] = json!(chosen.object_ids[0]);
        trace["object_ids"] = json!(chosen.object_ids);
        trace["regions"] = json!(chosen.object_ids.len());
        trace["specks_dropped"] = json!(chosen.dropped);
        // N28: full geometry of the chosen mask in the trace
        // (bounds are inclusive [left, top, right, bottom] in the
        // frame named by frame_width/frame_height).
        trace["mask_bounds"] = json!(chosen.bounds);
        trace["coverage"] = json!(crate::round6(assessment.coverage));
        trace["mask_pixels"] = json!(assessment.mask_pixels);
        trace["components"] = json!(assessment.components);
        trace["dominant_fraction"] = json!(crate::round6(assessment.dominant_fraction));
        if !assessment.accepted {
            trace["outcome"] = json!("poor_mask");
            trace["rejection"] = json!(assessment.rejection);
            let reason = rejection_reason(&assessment);
            last_rejection = Some(format!(
                "mask bounds {:?} (inclusive) in a {frame_width}x{frame_height} frame, coverage {}, rejected: {reason}",
                chosen.bounds,
                crate::round6(assessment.coverage),
            ));
            attempts.push(trace);
            last_code = "POOR_MASK";
            continue;
        }
        trace["outcome"] = json!("accepted");
        attempts.push(trace);
        return Ok(AttemptsOutcome {
            mask_image: mask_gray,
            phrase: phrase.clone(),
            attempts,
            usage: Usage {
                images,
                ..Default::default()
            },
        });
    }
    // Decision 114: every failure carries its correction path.
    // N28: when a mask was rejected, its geometry is in the text.
    let (detail, remedy) = match (last_code, &last_rejection) {
        ("POOR_MASK", Some(geo)) => (
            format!(" Last rejected {geo}."),
            " Retake the photo closer to the object with less of the frame covered by other things, or name the object more specifically.",
        ),
        ("POOR_MASK", None) => (
            String::new(),
            " Retake the photo closer to the object, or name the object more specifically.",
        ),
        _ => (
            String::new(),
            " Name the object differently, or retake the photo so the object is clearly visible.",
        ),
    };
    Err(CutOutError {
        code: last_code.to_string(),
        message: format!(
            "No usable mask after {} attempt(s); final outcome: {last_code}.{detail}{remedy}",
            attempts.len()
        ),
        attempts,
    })
}
