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

// ---------------------------------------------------------------------------
// Hole carving (docs/HOLE-CARVE.md). SAM returns open-frame products as
// SOLID regions: a bicycle wheel comes back as a disc, so the pavers, wall
// and cast shadow seen between the spokes are counted as product pixels
// and shipped untouched. The staged image then shows the OLD scene through
// the wheels on top of the NEW plate, which reads as a pasted cut-out.
// Ruling (Daniel): the product is pixel-exact, the background is variable,
// so the background seen through an open frame belongs to the plate.
//
// This step runs on the full-frame mask before anything is cut, stored or
// reviewed. It only ever REMOVES pixels from the product set: it never adds
// one and never recolours one. The receipt counts whatever the final mask
// says is product (alpha 255), so the count shrinks and stays honest.
//
// 1. Candidates: mask pixels deeper than the outer band from every
//    non-mask pixel (band = max(12 px, 0.6% of the mask diagonal)), so the
//    outer silhouette is never touched.
// 2. Colour evidence: background samples from a ring 6-30 px outside the
//    mask, product samples from the outer band (6 < depth <= band: thick
//    mask that is not the enclosed disc itself). Both are binned (RGB, 32
//    levels per channel), box-smoothed, and modelled per tile (3x3 tile
//    neighbourhood, global model as prior), bilinearly blended between
//    tile centres. A candidate is background-like when its colour is at
//    least e times as likely under the local background model as under
//    the local product model (the margin) and is a real background colour.
// 3. Thin structures (spokes, chain, cables, the hub's edges): black-hat /
//    top-hat on luma with a 13x13 square (lines up to 12 px wide), contrast
//    >= 28, components at least 1% of the mask diagonal long. Protected.
// 4. Windows. Background-like cells (small pits filled) are judged by
//    their border: a WINDOW (a sector between spokes) is framed by narrow
//    product structure, background-coloured product (a grey rim in shade,
//    a brass screw on a reflector) by the rest of its part. A cell is
//    carved when at least half its border is narrow AND it is deep (a
//    13x13-or-larger opening, scaled with the product) or, when shallow,
//    lies across a spoke from a deep window and stays framed with specks
//    of shade merged into their part. Cells linked through narrow
//    structure form a region, carved when it holds >= 0.2% of its
//    component. Pieces hanging off an accepted cell by a hairline neck are
//    judged again on their own. A component is carved only when its
//    carved area is >= 2% of it AND it is an open frame (enclosed
//    candidate area >= 10%, or the concept names a bicycle / bike / wheel
//    / chair / rack / stool / ladder / basket).
// 5. Finish: line-contrast pixels of BACKGROUND colour inside a window
//    (wall grain beside a spoke) are released; small specks left inside
//    windows go too; 1 px gaps along kept structures are closed back to
//    product; carved pixels touching kept product get a soft alpha (<= 128)
//    from a local two-colour estimate, so the edge fix treats the new inner
//    edges like the outer one. Kill switch: AGENTHAND_HOLE_CARVE=off.
// ---------------------------------------------------------------------------

/// Kill switch: `AGENTHAND_HOLE_CARVE=off` (or 0 / false / no) leaves
/// SAM's mask exactly as mapped.
pub const HOLE_CARVE_ENV: &str = "AGENTHAND_HOLE_CARVE";
/// The `hole_carve` record's version.
pub const HOLE_CARVE_VERSION: i64 = 1;
/// Background sample ring outside the mask (chessboard px, inclusive).
pub const CARVE_BG_RING: (u16, u16) = (6, 30);
/// Product samples start deeper than this inside the mask (px).
pub const CARVE_PRODUCT_MIN_DEPTH: u16 = 6;
/// Outer band (never carved; the product sample band ends here):
/// max(`CARVE_BAND_MIN_PX`, `CARVE_BAND_DIAG_FRAC` x mask bbox diagonal).
pub const CARVE_BAND_MIN_PX: u16 = 12;
pub const CARVE_BAND_DIAG_FRAC: f64 = 0.006;
/// Colour bins: `CARVE_BIN_BITS` bits per RGB channel.
pub const CARVE_BIN_BITS: u32 = 5;
const CARVE_LEVELS: usize = 1 << CARVE_BIN_BITS;
const CARVE_BINS: usize = CARVE_LEVELS * CARVE_LEVELS * CARVE_LEVELS;
/// Colour-model tiles: max(`CARVE_TILE_MIN_PX`, frac x diagonal) square,
/// at most `CARVE_TILE_MAX_GRID` tiles along either side of the window.
pub const CARVE_TILE_MIN_PX: usize = 64;
pub const CARVE_TILE_DIAG_FRAC: f64 = 0.08;
pub const CARVE_TILE_MAX_GRID: usize = 12;
/// Pseudo-samples of the global model mixed into every tile's model.
pub const CARVE_TILE_PRIOR: f64 = 500.0;
/// Background-like: P(colour | background) > ratio x P(colour | product).
pub const CARVE_BG_RATIO: f64 = std::f64::consts::E;
/// ...and P(colour | background) at least this share of a uniform bin.
pub const CARVE_BG_MIN_DENSITY: f64 = 0.25;
/// Thin structures: square of side 2r+1 (lines up to 2r px wide), luma
/// contrast floor, minimum component extent (max of the px floor and the
/// diagonal share).
pub const CARVE_THIN_RADIUS: usize = 6;
pub const CARVE_THIN_CONTRAST: u8 = 28;
pub const CARVE_THIN_MIN_EXTENT_PX: usize = 24;
pub const CARVE_THIN_EXTENT_DIAG_FRAC: f64 = 0.01;
/// A line-contrast pixel of background colour is released (carved) when
/// at least this share of the (2R+1) box around it is carved window.
pub const CARVE_RELEASE_RADIUS: usize = 12;
pub const CARVE_RELEASE_FRAC: f64 = 0.5;
pub const CARVE_RELEASE_ROUNDS: usize = 6;
/// A region is carved when its background-like area is at least this
/// share of its mask component.
pub const CARVE_REGION_MIN_FRAC: f64 = 0.002;
/// Specks: an uncarved, non-thin candidate patch entirely surrounded by
/// carved or thin pixels is carved too when it is at most
/// max(`CARVE_SPECK_MIN_PX`, frac x component) px (wall texture pits,
/// burrs between spokes).
pub const CARVE_SPECK_MIN_PX: u64 = 64;
pub const CARVE_SPECK_FRAC: f64 = 0.0002;
/// A background-like cell counts only as a window framed by narrow
/// structure: at least this share of its border is narrow, and it is at
/// least max(`CARVE_CELL_MIN_PX`, frac x component) px.
pub const CARVE_CELL_FRAME_FRAC: f64 = 0.5;
/// Strict framing: the product-like set is closed with a (2c+1) square
/// before the opening, so a part in shade read as background in specks
/// stays one solid part.
pub const CARVE_SOLID_CLOSE_RADIUS: usize = 2;
/// Accepted cells are cut at necks narrower than 2k+1 px (k = this
/// radius) and each piece is judged again.
pub const CARVE_NECK_RADIUS: usize = 3;
/// A deep window holds a pixel at least max(`CARVE_CORE_DEPTH_MIN_PX`,
/// frac x mask bbox diagonal) px (chessboard) from anything that is not
/// background-like.
pub const CARVE_CORE_DEPTH_MIN_PX: u16 = 8;
pub const CARVE_CORE_DEPTH_DIAG_FRAC: f64 = 0.004;
/// Cell floor: max(`CARVE_CELL_MIN_PX`, frac x component) px.
pub const CARVE_CELL_MIN_PX: u64 = 64;
pub const CARVE_CELL_MIN_FRAC: f64 = 0.0001;
/// A component is carved only when its carved background is at least
/// this share of it...
pub const CARVE_MIN_ENCLOSED_FRAC: f64 = 0.02;
/// ...and it is an open frame: enclosed candidate area (carved regions,
/// thin structures included) at least this share, or a keyword below.
pub const CARVE_OPEN_FRAME_FRAC: f64 = 0.1;
pub const OPEN_FRAME_WORDS: [&str; 8] = [
    "bicycle", "bike", "wheel", "chair", "rack", "stool", "ladder", "basket",
];
/// Soft inner edge: carved pixels touching kept product get at most this
/// alpha (about 50%), and none below `CARVE_SOFT_MIN_ALPHA`.
pub const CARVE_SOFT_MAX_ALPHA: u8 = 128;
pub const CARVE_SOFT_MIN_ALPHA: u8 = 26;

/// Whether hole carving runs at all (the kill switch).
pub fn hole_carve_enabled() -> bool {
    !matches!(
        std::env::var(HOLE_CARVE_ENV)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "off" | "false" | "no"
    )
}

/// The open-frame keyword the concept names, if any: a word of the
/// concept that starts with one of `OPEN_FRAME_WORDS` ("bikes",
/// "wheelchair" match; "track" does not match "rack").
pub fn open_frame_keyword(concept: &str) -> Option<&'static str> {
    let lower = concept.to_ascii_lowercase();
    for word in lower.split(|c: char| !c.is_ascii_alphanumeric()) {
        if let Some(k) = OPEN_FRAME_WORDS.iter().find(|k| word.starts_with(*k)) {
            return Some(k);
        }
    }
    None
}

/// `carve_holes_with` behind the kill switch: the cut_out entry point.
pub fn carve_holes(source: &RgbaImage, mask: &GrayImage, concept: &str) -> (GrayImage, Value) {
    carve_holes_with(source, mask, concept, hole_carve_enabled())
}

/// Exact chessboard distance (u16, saturating at `cap`) to the nearest
/// `target` pixel; with `border_is_target` the area beyond the window
/// counts as target. Same two-pass chamfer as `chessboard_distance`.
fn chessboard_distance_u16(
    target: &[bool],
    w: usize,
    h: usize,
    border_is_target: bool,
    cap: u16,
) -> Vec<u16> {
    let mut d: Vec<u16> = target.iter().map(|&t| if t { 0 } else { cap }).collect();
    if border_is_target {
        for y in 0..h {
            for x in 0..w {
                if (x == 0 || y == 0 || x == w - 1 || y == h - 1) && d[y * w + x] > 1 {
                    d[y * w + x] = 1;
                }
            }
        }
    }
    let step = |v: u16| v.saturating_add(1).min(cap);
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

/// Sliding max (or min) over `[i - r, i + r]` of one line, clipped to the
/// line (van Herk / Gil-Werman on a neutrally padded copy: O(1) per px).
fn sliding_extreme_line(line: &[u8], r: usize, want_max: bool, out: &mut [u8]) {
    let n = line.len();
    if n == 0 {
        return;
    }
    let k = 2 * r + 1;
    let neutral = if want_max { 0u8 } else { 255u8 };
    let op = |a: u8, b: u8| if want_max { a.max(b) } else { a.min(b) };
    let len = n + 2 * r;
    let at = |i: usize| -> u8 {
        if i < r || i >= r + n {
            neutral
        } else {
            line[i - r]
        }
    };
    let mut pre = vec![0u8; len];
    let mut suf = vec![0u8; len];
    for i in 0..len {
        pre[i] = if i % k == 0 { at(i) } else { op(pre[i - 1], at(i)) };
    }
    for i in (0..len).rev() {
        suf[i] = if i == len - 1 || (i + 1) % k == 0 {
            at(i)
        } else {
            op(suf[i + 1], at(i))
        };
    }
    for (i, o) in out.iter_mut().enumerate().take(n) {
        *o = op(suf[i], pre[i + 2 * r]);
    }
}

/// Square (2r+1) grey dilation (`want_max`) or erosion of a plane: rows
/// with `sliding_extreme_line`, then columns with the same van Herk
/// scheme run over whole rows at once (sequential memory).
fn square_extreme(plane: &[u8], w: usize, h: usize, r: usize, want_max: bool) -> Vec<u8> {
    let mut tmp = vec![0u8; plane.len()];
    for y in 0..h {
        sliding_extreme_line(&plane[y * w..(y + 1) * w], r, want_max, &mut tmp[y * w..(y + 1) * w]);
    }
    if h == 0 || w == 0 {
        return tmp;
    }
    let k = 2 * r + 1;
    let neutral = if want_max { 0u8 } else { 255u8 };
    let len = h + 2 * r;
    let neutral_row = vec![neutral; w];
    let row = |yy: usize| -> &[u8] {
        if yy < r || yy >= r + h {
            &neutral_row
        } else {
            &tmp[(yy - r) * w..(yy - r + 1) * w]
        }
    };
    let combine = |dst: &mut [u8], a: &[u8], b: &[u8]| {
        if want_max {
            for ((d, &x), &y) in dst.iter_mut().zip(a).zip(b) {
                *d = x.max(y);
            }
        } else {
            for ((d, &x), &y) in dst.iter_mut().zip(a).zip(b) {
                *d = x.min(y);
            }
        }
    };
    let mut pre = vec![0u8; len * w];
    let mut suf = vec![0u8; len * w];
    for yy in 0..len {
        if yy % k == 0 {
            pre[yy * w..(yy + 1) * w].copy_from_slice(row(yy));
        } else {
            let (done, rest) = pre.split_at_mut(yy * w);
            combine(&mut rest[..w], &done[(yy - 1) * w..], row(yy));
        }
    }
    for yy in (0..len).rev() {
        if yy == len - 1 || (yy + 1) % k == 0 {
            suf[yy * w..(yy + 1) * w].copy_from_slice(row(yy));
        } else {
            let (cur, next) = suf.split_at_mut((yy + 1) * w);
            combine(&mut cur[yy * w..], &next[..w], row(yy));
        }
    }
    let mut out = vec![0u8; plane.len()];
    for y in 0..h {
        combine(
            &mut out[y * w..(y + 1) * w],
            &suf[y * w..(y + 1) * w],
            &pre[(y + 2 * r) * w..(y + 2 * r + 1) * w],
        );
    }
    out
}

/// 8-connected components of `set` (window plane): per-pixel labels
/// (0 = not in the set, else 1-based) and the component count.
fn label8(set: &[bool], w: usize, h: usize) -> (Vec<u32>, usize) {
    let mut labels = vec![0u32; set.len()];
    let mut n = 0u32;
    let mut stack: Vec<usize> = Vec::new();
    for start in 0..set.len() {
        if !set[start] || labels[start] != 0 {
            continue;
        }
        n += 1;
        labels[start] = n;
        stack.push(start);
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            for ny in y.saturating_sub(1)..(y + 2).min(h) {
                for nx in x.saturating_sub(1)..(x + 2).min(w) {
                    let j = ny * w + nx;
                    if set[j] && labels[j] == 0 {
                        labels[j] = n;
                        stack.push(j);
                    }
                }
            }
        }
    }
    (labels, n as usize)
}

/// Count of `set` pixels in the (2r+1) square around each pixel (clipped
/// to the window), separable running sums.
fn box_count(set: &[bool], w: usize, h: usize, r: usize) -> Vec<u16> {
    let mut rows = vec![0u16; set.len()];
    for y in 0..h {
        let line = &set[y * w..(y + 1) * w];
        let mut acc: u32 = line.iter().take(r.min(w)).map(|&v| v as u32).sum();
        for x in 0..w {
            if x + r < w && line[x + r] {
                acc += 1;
            }
            if x > r && line[x - r - 1] {
                acc -= 1;
            }
            rows[y * w + x] = acc as u16;
        }
    }
    let mut out = vec![0u16; set.len()];
    let mut acc = vec![0u16; w];
    for y in 0..r.min(h) {
        for (a, &v) in acc.iter_mut().zip(&rows[y * w..(y + 1) * w]) {
            *a += v;
        }
    }
    for y in 0..h {
        if y + r < h {
            for (a, &v) in acc.iter_mut().zip(&rows[(y + r) * w..(y + r + 1) * w]) {
                *a += v;
            }
        }
        if y > r {
            for (a, &v) in acc.iter_mut().zip(&rows[(y - r - 1) * w..(y - r) * w]) {
                *a -= v;
            }
        }
        out[y * w..(y + 1) * w].copy_from_slice(&acc);
    }
    out
}

/// Colour bin of an RGB(A) pixel: `CARVE_LEVELS` levels per channel.
#[inline]
fn carve_colour_bin(px: &[u8]) -> usize {
    let s = 8 - CARVE_BIN_BITS;
    let b = CARVE_BIN_BITS;
    (((px[0] >> s) as usize) << (2 * b)) | (((px[1] >> s) as usize) << b) | ((px[2] >> s) as usize)
}

/// 3x3x3 box sum over the colour cube (zero outside it), separable.
fn smooth_bins(hist: &[u32]) -> Vec<u32> {
    let l = CARVE_LEVELS;
    let mut cur: Vec<u32> = hist.to_vec();
    for stride in [l * l, l, 1] {
        let mut next = vec![0u32; CARVE_BINS];
        for (i, slot) in next.iter_mut().enumerate() {
            let coord = (i / stride) % l;
            let mut s = cur[i];
            if coord > 0 {
                s += cur[i - stride];
            }
            if coord + 1 < l {
                s += cur[i + stride];
            }
            *slot = s;
        }
        cur = next;
    }
    cur
}

/// Probability table: the smoothed local counts with `CARVE_TILE_PRIOR`
/// pseudo-samples of the (normalised) global model mixed in.
fn carve_model(local: &[u32], global: &[u32], global_sum: f64) -> Vec<f32> {
    let local_sum: f64 = local.iter().map(|&v| v as f64).sum();
    let denom = local_sum + CARVE_TILE_PRIOR;
    local
        .iter()
        .zip(global.iter())
        .map(|(&l, &g)| ((l as f64 + CARVE_TILE_PRIOR * g as f64 / global_sum) / denom) as f32)
        .collect()
}

/// Per-tile background and product colour models (docs/HOLE-CARVE.md,
/// step 2): each tile pools its 3x3 tile neighbourhood, the global
/// model is mixed in as a `CARVE_TILE_PRIOR`-sample prior, and the
/// models are blended bilinearly between tile centres.
struct CarveTileModels {
    tile: usize,
    ntx: usize,
    nty: usize,
    bg: Vec<Vec<f32>>,
    pr: Vec<Vec<f32>>,
}

impl CarveTileModels {
    /// Build the models over a `w` x `h` window. `sample(i)` names pixel
    /// i as a background sample (`Some((true, bin))`), a product sample
    /// (`Some((false, bin))`) or neither. Also returns the background
    /// and product sample counts.
    fn build(
        w: usize,
        h: usize,
        tile: usize,
        sample: impl Fn(usize) -> Option<(bool, usize)>,
    ) -> (Self, u64, u64) {
        let (ntx, nty) = (w.div_ceil(tile), h.div_ceil(tile));
        let mut raw_bg = vec![0u32; ntx * nty * CARVE_BINS];
        let mut raw_pr = vec![0u32; ntx * nty * CARVE_BINS];
        let mut glob_bg = vec![0u32; CARVE_BINS];
        let mut glob_pr = vec![0u32; CARVE_BINS];
        for i in 0..w * h {
            let Some((is_bg, c)) = sample(i) else {
                continue;
            };
            let t = ((i / w) / tile) * ntx + (i % w) / tile;
            if is_bg {
                raw_bg[t * CARVE_BINS + c] += 1;
                glob_bg[c] += 1;
            } else {
                raw_pr[t * CARVE_BINS + c] += 1;
                glob_pr[c] += 1;
            }
        }
        let (n_bg, n_pr): (u64, u64) = (
            glob_bg.iter().map(|&v| v as u64).sum(),
            glob_pr.iter().map(|&v| v as u64).sum(),
        );
        let g_bg = smooth_bins(&glob_bg);
        let g_pr = smooth_bins(&glob_pr);
        let g_bg_sum: f64 = g_bg.iter().map(|&v| v as f64).sum::<f64>().max(1.0);
        let g_pr_sum: f64 = g_pr.iter().map(|&v| v as f64).sum::<f64>().max(1.0);
        let mut bg: Vec<Vec<f32>> = Vec::with_capacity(ntx * nty);
        let mut pr: Vec<Vec<f32>> = Vec::with_capacity(ntx * nty);
        for ty in 0..nty {
            for tx in 0..ntx {
                let mut loc_bg = vec![0u32; CARVE_BINS];
                let mut loc_pr = vec![0u32; CARVE_BINS];
                for yy in ty.saturating_sub(1)..(ty + 2).min(nty) {
                    for xx in tx.saturating_sub(1)..(tx + 2).min(ntx) {
                        let t = yy * ntx + xx;
                        let (rb, rp) = (
                            &raw_bg[t * CARVE_BINS..(t + 1) * CARVE_BINS],
                            &raw_pr[t * CARVE_BINS..(t + 1) * CARVE_BINS],
                        );
                        for c in 0..CARVE_BINS {
                            loc_bg[c] += rb[c];
                            loc_pr[c] += rp[c];
                        }
                    }
                }
                bg.push(carve_model(&smooth_bins(&loc_bg), &g_bg, g_bg_sum));
                pr.push(carve_model(&smooth_bins(&loc_pr), &g_pr, g_pr_sum));
            }
        }
        (CarveTileModels { tile, ntx, nty, bg, pr }, n_bg, n_pr)
    }

    /// (P(colour | background), P(colour | product)) at pixel i of the
    /// window for colour bin `c`, blended between tile centres.
    fn evidence(&self, i: usize, w: usize, c: usize) -> (f64, f64) {
        let tile = self.tile;
        let tile_coord = |p: usize, count: usize| -> (usize, usize, f64) {
            let f = ((p as f64 + 0.5) / tile as f64 - 0.5).clamp(0.0, (count - 1) as f64);
            let i0 = (f.floor() as usize).min(count - 1);
            let i1 = (i0 + 1).min(count - 1);
            (i0, i1, f - i0 as f64)
        };
        let (ntx, nty) = (self.ntx, self.nty);
        let (tx0, tx1, wx) = tile_coord(i % w, ntx);
        let (ty0, ty1, wy) = tile_coord(i / w, nty);
        let mut pb = 0f64;
        let mut pp = 0f64;
        for (t, wt) in [
            (ty0 * ntx + tx0, (1.0 - wy) * (1.0 - wx)),
            (ty0 * ntx + tx1, (1.0 - wy) * wx),
            (ty1 * ntx + tx0, wy * (1.0 - wx)),
            (ty1 * ntx + tx1, wy * wx),
        ] {
            pb += wt * self.bg[t][c] as f64;
            pp += wt * self.pr[t][c] as f64;
        }
        (pb, pp)
    }
}

/// Pillow 'L' luma of an RGB(A) pixel.
#[inline]
fn carve_luma(px: &[u8]) -> u8 {
    ((px[0] as u32 * 19595 + px[1] as u32 * 38470 + px[2] as u32 * 7471 + 0x8000) >> 16) as u8
}



/// The no-op record (mask returned unchanged).
fn hole_carve_skipped(reason: &str, product_pixels: u64, keyword: Option<&str>) -> Value {
    json!({
        "version": HOLE_CARVE_VERSION,
        "applied": false,
        "reason": reason,
        "regions": 0,
        "carved_pixels": 0,
        "protected_thin_pixels": 0,
        "soft_edge_pixels": 0,
        "product_pixels_before": product_pixels,
        "product_pixels_after": product_pixels,
        "open_frame_keyword": keyword,
    })
}

/// Carve enclosed background out of open-frame products (see the block
/// comment above and docs/HOLE-CARVE.md). Returns the mask to use from
/// here on (unchanged unless `applied`) and the `hole_carve` record.
/// Only mask pixels at 255 (product) are ever lowered; nothing is raised.
pub fn carve_holes_with(
    source: &RgbaImage,
    mask: &GrayImage,
    concept: &str,
    enabled: bool,
) -> (GrayImage, Value) {
    let product_before = mask.pixels.iter().filter(|&&v| v == 255).count() as u64;
    let keyword = open_frame_keyword(concept);
    let unchanged = |reason: &str| {
        (
            mask.clone(),
            hole_carve_skipped(reason, product_before, keyword),
        )
    };
    if !enabled {
        return unchanged("disabled (AGENTHAND_HOLE_CARVE=off)");
    }
    if (source.width, source.height) != (mask.width, mask.height) {
        return unchanged("source and mask frames differ");
    }
    let (fw, fh) = (mask.width as usize, mask.height as usize);
    // Window: the product bbox grown by the background ring.
    let (mut bx0, mut by0, mut bx1, mut by1) = (usize::MAX, usize::MAX, 0usize, 0usize);
    for y in 0..fh {
        for x in 0..fw {
            if mask.pixels[y * fw + x] == 255 {
                bx0 = bx0.min(x);
                by0 = by0.min(y);
                bx1 = bx1.max(x + 1);
                by1 = by1.max(y + 1);
            }
        }
    }
    if bx0 == usize::MAX {
        return unchanged("the mask has no product pixel");
    }
    let diag = (((bx1 - bx0) as f64).powi(2) + ((by1 - by0) as f64).powi(2)).sqrt();
    let margin = CARVE_BG_RING.1 as usize + 1;
    let (x0, y0) = (bx0.saturating_sub(margin), by0.saturating_sub(margin));
    let (x1, y1) = ((bx1 + margin).min(fw), (by1 + margin).min(fh));
    let (w, h) = (x1 - x0, y1 - y0);
    let n = w * h;
    let src_at = |i: usize| -> &[u8] {
        let j = ((y0 + i / w) * fw + x0 + i % w) * 4;
        &source.pixels[j..j + 4]
    };
    let on: Vec<bool> = (0..n)
        .map(|i| mask.pixels[(y0 + i / w) * fw + x0 + i % w] == 255)
        .collect();

    // 1. Depth inside / distance outside, outer band, candidates.
    let band_px = CARVE_BAND_MIN_PX.max((CARVE_BAND_DIAG_FRAC * diag).round() as u16);
    let not_on: Vec<bool> = on.iter().map(|&v| !v).collect();
    let din = chessboard_distance_u16(&not_on, w, h, true, band_px + 1);
    let dout = chessboard_distance_u16(&on, w, h, false, CARVE_BG_RING.1 + 1);
    let cand: Vec<bool> = (0..n).map(|i| on[i] && din[i] > band_px).collect();
    if !cand.iter().any(|&c| c) {
        return unchanged("no mask pixel lies deeper than the outer band (nothing enclosed)");
    }

    // 2. Colour models, per tile with the global model as prior.
    let tile = CARVE_TILE_MIN_PX
        .max((CARVE_TILE_DIAG_FRAC * diag).round() as usize)
        .max(w.max(h).div_ceil(CARVE_TILE_MAX_GRID));
    let ring_at = |i: usize| !on[i] && dout[i] >= CARVE_BG_RING.0 && dout[i] <= CARVE_BG_RING.1;
    let band_at = |i: usize| on[i] && din[i] > CARVE_PRODUCT_MIN_DEPTH && din[i] <= band_px;
    let (models, n_bg, n_pr) = CarveTileModels::build(w, h, tile, |i| {
        if ring_at(i) {
            Some((true, carve_colour_bin(src_at(i))))
        } else if band_at(i) {
            Some((false, carve_colour_bin(src_at(i))))
        } else {
            None
        }
    });
    if n_bg < 64 || n_pr < 64 {
        return unchanged("too few background or product colour samples around the mask");
    }
    let min_density = CARVE_BG_MIN_DENSITY / CARVE_BINS as f64;
    let evidence = |i: usize| -> (f64, f64) { models.evidence(i, w, carve_colour_bin(src_at(i))) };
    let bg_like = |i: usize| -> bool {
        let (pb, pp) = evidence(i);
        pb >= min_density && pb > CARVE_BG_RATIO * pp
    };

    // Background-like candidates (cached: the thin step and the carve read it).
    let bgl: Vec<bool> = (0..n).map(|i| cand[i] && bg_like(i)).collect();

    // 3. Thin structures over the candidates' bbox (+ the square radius).
    // Colour consistency first: a line-contrast pixel of BACKGROUND colour
    // in a background-dominated box (grain and mortar in a wall, the
    // stone's own texture) is texture, not a product line, and is never
    // marked thin. Background-coloured line work inside product (tread on
    // a sunlit tyre, a brass screw on a reflector, a rusty hub nut) sits in
    // a product-dominated box and stays thin.
    // Dense at two scales: the near box and one three times wider, so a
    // compact background-coloured part (a brass screw filling the near
    // box) inside product is not taken for texture.
    let bg_dense: Vec<bool> = {
        let dense_at = |rr: usize| -> Vec<bool> {
            let need = (CARVE_RELEASE_FRAC * ((2 * rr + 1) * (2 * rr + 1)) as f64).ceil() as u16;
            box_count(&bgl, w, h, rr).iter().map(|&c| c >= need).collect()
        };
        let near = dense_at(CARVE_RELEASE_RADIUS);
        let wide = dense_at(3 * CARVE_RELEASE_RADIUS);
        near.iter().zip(wide.iter()).map(|(&a, &b)| a && b).collect()
    };
    let r = CARVE_THIN_RADIUS;
    let (mut cx0, mut cy0, mut cx1, mut cy1) = (usize::MAX, usize::MAX, 0usize, 0usize);
    for i in 0..n {
        if cand[i] {
            cx0 = cx0.min(i % w);
            cy0 = cy0.min(i / w);
            cx1 = cx1.max(i % w + 1);
            cy1 = cy1.max(i / w + 1);
        }
    }
    let (sx0, sy0) = (cx0.saturating_sub(r), cy0.saturating_sub(r));
    let (sx1, sy1) = ((cx1 + r).min(w), (cy1 + r).min(h));
    let (sw, sh) = (sx1 - sx0, sy1 - sy0);
    let luma: Vec<u8> = (0..sw * sh)
        .map(|k| carve_luma(src_at((sy0 + k / sw) * w + sx0 + k % sw)))
        .collect();
    let closing = square_extreme(&square_extreme(&luma, sw, sh, r, true), sw, sh, r, false);
    let opening = square_extreme(&square_extreme(&luma, sw, sh, r, false), sw, sh, r, true);
    let mut thin0 = vec![false; n];
    for k in 0..sw * sh {
        let i = (sy0 + k / sw) * w + sx0 + k % sw;
        if !cand[i] {
            continue;
        }
        let l = luma[k];
        let black = closing[k].saturating_sub(l);
        let white = l.saturating_sub(opening[k]);
        thin0[i] = black.max(white) >= CARVE_THIN_CONTRAST && !(bgl[i] && bg_dense[i]);
    }
    drop((luma, closing, opening, bg_dense));
    let min_extent = CARVE_THIN_MIN_EXTENT_PX
        .max((CARVE_THIN_EXTENT_DIAG_FRAC * diag).round() as usize);
    let (thin_labels, thin_count) = label8(&thin0, w, h);
    let mut ext = vec![(usize::MAX, usize::MAX, 0usize, 0usize); thin_count + 1];
    for i in 0..n {
        let l = thin_labels[i] as usize;
        if l != 0 {
            let e = &mut ext[l];
            e.0 = e.0.min(i % w);
            e.1 = e.1.min(i / w);
            e.2 = e.2.max(i % w);
            e.3 = e.3.max(i / w);
        }
    }
    let keep_line: Vec<bool> = ext
        .iter()
        .map(|e| e.0 != usize::MAX && (e.2 - e.0 + 1).max(e.3 - e.1 + 1) >= min_extent)
        .collect();
    let thin: Vec<bool> = (0..n)
        .map(|i| thin_labels[i] != 0 && keep_line[thin_labels[i] as usize])
        .collect();
    drop((thin0, thin_labels));

    // 4. Carve candidates, cells, regions, per-component guards.
    let mut carve_x: Vec<bool> = (0..n).map(|i| bgl[i] && !thin[i]).collect();
    let (comp_labels, comp_count) = label8(&on, w, h);
    let mut comp_area = vec![0u64; comp_count + 1];
    for &l in &comp_labels {
        comp_area[l as usize] += 1;
    }
    let speck_cap =
        |c: usize| CARVE_SPECK_MIN_PX.max((CARVE_SPECK_FRAC * comp_area[c] as f64) as u64);
    // 4a. Pits: small non-thin candidate patches entirely inside the
    // background-like set (texture pits in a wall) join it.
    {
        let pit_set: Vec<bool> = (0..n).map(|i| cand[i] && !carve_x[i] && !thin[i]).collect();
        let (pit_labels, pit_count) = label8(&pit_set, w, h);
        let mut area = vec![0u64; pit_count + 1];
        let mut open = vec![false; pit_count + 1];
        let mut comp = vec![0u32; pit_count + 1];
        for i in 0..n {
            let l = pit_labels[i] as usize;
            if l == 0 {
                continue;
            }
            area[l] += 1;
            comp[l] = comp_labels[i];
            if open[l] {
                continue;
            }
            let (x, y) = (i % w, i / w);
            for ny in y.saturating_sub(1)..(y + 2).min(h) {
                for nx in x.saturating_sub(1)..(x + 2).min(w) {
                    let j = ny * w + nx;
                    if pit_labels[j] as usize != l && !carve_x[j] {
                        open[l] = true;
                    }
                }
            }
        }
        for i in 0..n {
            let l = pit_labels[i] as usize;
            if l != 0 && !open[l] && area[l] <= speck_cap(comp[l] as usize) {
                carve_x[i] = true;
            }
        }
    }
    // 4b. Cells: connected background-like areas. Thin structures split
    // them (a sector between two spokes is one cell). A cell counts only
    // as a WINDOW framed by narrow structure (spokes, chain, cables), not
    // as background-coloured product (a grey rim or a crank in shade, a
    // brass reflector screw), which is bordered by the rest of its part:
    // - NARROW: product-like candidate pixels outside SOLID product (the
    //   product-like set opened with the thin square, so under 2r+1 px
    //   across; line work needs a part 4r+1 wide to count as solid, so
    //   tread and reflector prisms are solid, a spoke is not). Dark spokes
    //   over dark pavers fall below the line-contrast floor but are still
    //   narrow.
    // - STRICT narrow: the same with the product-like set first closed
    //   with a (2c+1) square, so a part in shade that reads as background
    //   in specks (the rim, a sidewall) stays one solid part.
    // A cell is a window when it is at least max(`CARVE_CELL_MIN_PX`,
    // `CARVE_CELL_MIN_FRAC` x component) px, at least
    // `CARVE_CELL_FRAME_FRAC` of its border is narrow, AND it is deep (it
    // holds a pixel `core_depth` px from anything else), or it lies within
    // 2r+1 px of a deep window (a fragment across a spoke from one) with
    // that share of its border STRICT narrow too: a real opening between
    // spokes, not a crescent along a shaded rim.
    let solid_narrow = |close: usize| -> Vec<bool> {
        let plane: Vec<u8> = (0..n).map(|i| if on[i] && !carve_x[i] { 255 } else { 0 }).collect();
        let closed = if close == 0 {
            plane
        } else {
            square_extreme(&square_extreme(&plane, w, h, close, true), w, h, close, false)
        };
        let eroded = square_extreme(&closed, w, h, r, false);
        let opened = square_extreme(&eroded, w, h, r, true);
        // Line work counts as frame unless it lies in a part twice as wide
        // (tread on a tyre, the prisms of a reflector).
        let wide = square_extreme(&square_extreme(&eroded, w, h, r, false), w, h, 2 * r, true);
        (0..n)
            .map(|i| {
                cand[i] && !carve_x[i] && if thin[i] { wide[i] == 0 } else { opened[i] == 0 }
            })
            .collect()
    };
    let narrow = solid_narrow(0);
    let strict = solid_narrow(CARVE_SOLID_CLOSE_RADIUS);
    let core_depth = CARVE_CORE_DEPTH_MIN_PX.max((CARVE_CORE_DEPTH_DIAG_FRAC * diag).round() as u16);
    let depth = {
        let not_x: Vec<bool> = carve_x.iter().map(|&v| !v).collect();
        chessboard_distance_u16(&not_x, w, h, true, core_depth)
    };
    let (cell_labels, cell_count) = label8(&carve_x, w, h);
    let mut cell_area = vec![0u64; cell_count + 1];
    let mut cell_thin = vec![0u64; cell_count + 1];
    let mut cell_other = vec![0u64; cell_count + 1];
    let mut cell_strict = vec![0u64; cell_count + 1];
    let mut cell_deep = vec![false; cell_count + 1];
    let mut cell_comp = vec![0u32; cell_count + 1];
    for i in 0..n {
        let l = cell_labels[i] as usize;
        if l == 0 {
            continue;
        }
        cell_area[l] += 1;
        cell_comp[l] = comp_labels[i];
        if depth[i] >= core_depth {
            cell_deep[l] = true;
        }
        let (x, y) = (i % w, i / w);
        let mut nb = |j: usize| {
            if cell_labels[j] as usize != l {
                if narrow[j] {
                    cell_thin[l] += 1;
                } else {
                    cell_other[l] += 1;
                }
                if strict[j] {
                    cell_strict[l] += 1;
                }
            }
        };
        if x > 0 {
            nb(i - 1);
        }
        if x + 1 < w {
            nb(i + 1);
        }
        if y > 0 {
            nb(i - w);
        }
        if y + 1 < h {
            nb(i + w);
        }
    }
    drop(depth);
    let region_floor =
        |c: usize| ((CARVE_REGION_MIN_FRAC * comp_area[c] as f64).ceil() as u64).max(1);
    let framed: Vec<bool> = (0..=cell_count)
        .map(|l| {
            if l == 0 {
                return false;
            }
            let c = cell_comp[l] as usize;
            let cell_min = CARVE_CELL_MIN_PX.max((CARVE_CELL_MIN_FRAC * comp_area[c] as f64) as u64);
            let border = (cell_thin[l] + cell_other[l]) as f64;
            cell_area[l] >= cell_min && cell_thin[l] as f64 >= CARVE_CELL_FRAME_FRAC * border
        })
        .collect();
    let cell_near_deep: Vec<bool> = {
        let plane: Vec<u8> = (0..n)
            .map(|i| {
                let l = cell_labels[i] as usize;
                if l != 0 && framed[l] && cell_deep[l] { 255 } else { 0 }
            })
            .collect();
        let near = square_extreme(&plane, w, h, 2 * r + 1, true);
        let mut v = vec![false; cell_count + 1];
        for i in 0..n {
            let l = cell_labels[i] as usize;
            if l != 0 && near[i] != 0 {
                v[l] = true;
            }
        }
        v
    };
    let cell_ok: Vec<bool> = (0..=cell_count)
        .map(|l| {
            let border = (cell_thin[l] + cell_other[l]) as f64;
            framed[l]
                && (cell_deep[l]
                    || (cell_near_deep[l]
                        && cell_strict[l] as f64 >= CARVE_CELL_FRAME_FRAC * border))
        })
        .collect();
    // 4c. Regions: counting cells linked through thin structures.
    let link: Vec<bool> = (0..n)
        .map(|i| narrow[i] || cell_ok[cell_labels[i] as usize])
        .collect();
    let (region_labels, region_count) = label8(&link, w, h);
    drop(link);
    let mut reg_x = vec![0u64; region_count + 1];
    let mut reg_t = vec![0u64; region_count + 1];
    let mut reg_comp = vec![0u32; region_count + 1];
    for i in 0..n {
        let l = region_labels[i] as usize;
        if l == 0 {
            continue;
        }
        reg_comp[l] = comp_labels[i];
        if cell_ok[cell_labels[i] as usize] {
            reg_x[l] += 1;
        } else {
            // A thin pixel framing a counting cell.
            let (x, y) = (i % w, i / w);
            let frames = (y.saturating_sub(1)..(y + 2).min(h)).any(|ny| {
                (x.saturating_sub(1)..(x + 2).min(w))
                    .any(|nx| cell_ok[cell_labels[ny * w + nx] as usize])
            });
            if frames {
                reg_t[l] += 1;
            }
        }
    }
    let mut region_ok = vec![false; region_count + 1];
    let mut enc_bg = vec![0u64; comp_count + 1];
    let mut enc_cand = vec![0u64; comp_count + 1];
    for l in 1..=region_count {
        let c = reg_comp[l] as usize;
        if reg_x[l] > 0 && reg_x[l] >= region_floor(c) {
            region_ok[l] = true;
            enc_bg[c] += reg_x[l];
            enc_cand[c] += reg_x[l] + reg_t[l];
        }
    }
    let mut comp_ok = vec![false; comp_count + 1];
    // The deciding component for the record: the one with the largest
    // enclosed background share.
    let (mut best_c, mut best_frac) = (0usize, -1f64);
    for c in 1..=comp_count {
        let area = comp_area[c] as f64;
        let bg_frac = enc_bg[c] as f64 / area;
        let cand_frac = enc_cand[c] as f64 / area;
        if bg_frac > best_frac {
            best_frac = bg_frac;
            best_c = c;
        }
        comp_ok[c] = bg_frac >= CARVE_MIN_ENCLOSED_FRAC
            && (cand_frac >= CARVE_OPEN_FRAME_FRAC || keyword.is_some());
    }
    let best_bg = if best_c > 0 { enc_bg[best_c] as f64 / comp_area[best_c] as f64 } else { 0.0 };
    let best_cand =
        if best_c > 0 { enc_cand[best_c] as f64 / comp_area[best_c] as f64 } else { 0.0 };
    let open_frame = json!({
        "keyword": keyword,
        "enclosed_background_fraction": crate::round6(best_bg),
        "enclosed_candidate_fraction": crate::round6(best_cand),
        "min_enclosed_background_fraction": CARVE_MIN_ENCLOSED_FRAC,
        "min_open_frame_fraction": CARVE_OPEN_FRAME_FRAC,
    });
    if !comp_ok.iter().any(|&v| v) {
        let reason = if best_bg < CARVE_MIN_ENCLOSED_FRAC {
            format!(
                "no enclosed background-like region: {:.2}% of the component, needs {}%",
                best_bg * 100.0,
                CARVE_MIN_ENCLOSED_FRAC * 100.0
            )
        } else {
            format!(
                "not an open frame: enclosed candidate area {:.2}% of the component (needs {}%) and the concept names no open-frame keyword",
                best_cand * 100.0,
                CARVE_OPEN_FRAME_FRAC * 100.0
            )
        };
        let (m, mut rec) = unchanged(&reason);
        rec["open_frame"] = open_frame;
        return (m, rec);
    }
    let mut carved = vec![false; n];
    let mut regions = 0u64;
    let mut protected = 0u64;
    let mut counted = vec![false; region_count + 1];
    for i in 0..n {
        let l = region_labels[i] as usize;
        if l == 0 || !region_ok[l] || !comp_ok[reg_comp[l] as usize] {
            continue;
        }
        if !counted[l] {
            counted[l] = true;
            regions += 1;
        }
        if cell_ok[cell_labels[i] as usize] {
            carved[i] = true;
        }
    }
    // 4d. Appendages: an accepted cell is cut at NECKS (its pixels opened
    // with a (2k+1) square). Each piece at least the cell floor is judged
    // like a cell again, the cut-away neck pixels counting as look-through
    // frame; a piece that fails goes back to the product with the neck
    // pixels that only touch it. A background-coloured part joined to a
    // window by a hairline (a brass reflector screw) stays.
    let mut appendage_px = 0u64;
    {
        let k = CARVE_NECK_RADIUS;
        let opened: Vec<bool> = {
            let plane: Vec<u8> = carved.iter().map(|&c| if c { 255 } else { 0 }).collect();
            let o = square_extreme(&square_extreme(&plane, w, h, k, false), w, h, k, true);
            o.iter().map(|&v| v != 0).collect()
        };
        let depth = {
            let not_c: Vec<bool> = carved.iter().map(|&v| !v).collect();
            chessboard_distance_u16(&not_c, w, h, true, core_depth)
        };
        let (pl, pc) = label8(&opened, w, h);
        let mut p_area = vec![0u64; pc + 1];
        let mut p_frame = vec![0u64; pc + 1];
        let mut p_strict = vec![0u64; pc + 1];
        let mut p_border = vec![0u64; pc + 1];
        let mut p_deep = vec![false; pc + 1];
        let mut p_comp = vec![0u32; pc + 1];
        for i in 0..n {
            let l = pl[i] as usize;
            if l == 0 {
                continue;
            }
            p_area[l] += 1;
            p_comp[l] = comp_labels[i];
            if depth[i] >= core_depth {
                p_deep[l] = true;
            }
            let (x, y) = (i % w, i / w);
            let mut nb = |j: usize| {
                if pl[j] as usize != l {
                    p_border[l] += 1;
                    let look = carved[j] && !opened[j];
                    if look || narrow[j] {
                        p_frame[l] += 1;
                    }
                    if look || strict[j] {
                        p_strict[l] += 1;
                    }
                }
            };
            if x > 0 {
                nb(i - 1);
            }
            if x + 1 < w {
                nb(i + 1);
            }
            if y > 0 {
                nb(i - w);
            }
            if y + 1 < h {
                nb(i + w);
            }
        }
        drop(depth);
        let drop_piece: Vec<bool> = (0..=pc)
            .map(|l| {
                if l == 0 {
                    return false;
                }
                let c = p_comp[l] as usize;
                let floor = CARVE_CELL_MIN_PX.max((CARVE_CELL_MIN_FRAC * comp_area[c] as f64) as u64);
                let b = p_border[l] as f64;
                let ok = p_frame[l] as f64 >= CARVE_CELL_FRAME_FRAC * b
                    && (p_deep[l] || p_strict[l] as f64 >= CARVE_CELL_FRAME_FRAC * b);
                p_area[l] >= floor && !ok
            })
            .collect();
        if drop_piece.iter().any(|&d| d) {
            let plane_of = |want_drop: bool| -> Vec<u8> {
                (0..n)
                    .map(|i| {
                        let l = pl[i] as usize;
                        if l != 0 && drop_piece[l] == want_drop { 255 } else { 0 }
                    })
                    .collect()
            };
            let near_drop = square_extreme(&plane_of(true), w, h, k, true);
            let near_keep = square_extreme(&plane_of(false), w, h, k, true);
            for i in 0..n {
                let l = pl[i] as usize;
                let neck = carved[i] && !opened[i] && near_drop[i] != 0 && near_keep[i] == 0;
                if (l != 0 && drop_piece[l]) || neck {
                    carved[i] = false;
                    appendage_px += 1;
                }
            }
        }
    }
    drop((region_labels, cell_labels, carve_x, strict));

    // 5a'. Colour consistency of the protected lines: a line-contrast pixel
    // of BACKGROUND colour (dark grain in a wall beside a spoke) is released
    // when most of the box around it is carved window. Line pixels of
    // product colour (the spoke itself) are never released, and background-
    // coloured product line work (tyre tread, a reflector's bolt, a rusty
    // hub nut) sits in product, not in a window, so it stays.
    // Rounds, so a cluster of grain is taken from its window side in. A
    // pixel of a SOLID kept part (the kept set opened with the thin
    // square: 2r+1 px across or more, like a brass reflector screw) is
    // never released, whatever its colour.
    let mut released = 0u64;
    {
        let kept_solid: Vec<bool> = {
            let plane: Vec<u8> = (0..n).map(|i| if on[i] && !carved[i] { 255 } else { 0 }).collect();
            let opened = square_extreme(&square_extreme(&plane, w, h, r, false), w, h, r, true);
            opened.iter().map(|&v| v != 0).collect()
        };
        let rr = CARVE_RELEASE_RADIUS;
        let need = (CARVE_RELEASE_FRAC * ((2 * rr + 1) * (2 * rr + 1)) as f64).ceil() as u32;
        for _ in 0..CARVE_RELEASE_ROUNDS {
            let cnt = box_count(&carved, w, h, rr);
            let mut changed = 0u64;
            for i in 0..n {
                if thin[i]
                    && bgl[i]
                    && !carved[i]
                    && !kept_solid[i]
                    && comp_ok[comp_labels[i] as usize]
                    && cnt[i] as u32 >= need
                {
                    carved[i] = true;
                    changed += 1;
                }
            }
            released += changed;
            if changed == 0 {
                break;
            }
        }
    }

    // 5a. Specks: small uncarved, non-thin candidate patches whose every
    // neighbour is carved or thin, touching at least one carved pixel, and
    // shorter than a protected line (`min_extent`), so an unmarked spoke
    // segment between two windows is never taken for a speck.
    let speck_set: Vec<bool> = (0..n).map(|i| cand[i] && !carved[i] && !thin[i]).collect();
    let (speck_labels, speck_count) = label8(&speck_set, w, h);
    drop(speck_set);
    let mut speck_area = vec![0u64; speck_count + 1];
    let mut speck_open = vec![false; speck_count + 1];
    let mut speck_carved_nb = vec![false; speck_count + 1];
    let mut speck_comp = vec![0u32; speck_count + 1];
    let mut speck_ext = vec![(usize::MAX, usize::MAX, 0usize, 0usize); speck_count + 1];
    for i in 0..n {
        let l = speck_labels[i] as usize;
        if l == 0 {
            continue;
        }
        speck_area[l] += 1;
        speck_comp[l] = comp_labels[i];
        let e = &mut speck_ext[l];
        e.0 = e.0.min(i % w);
        e.1 = e.1.min(i / w);
        e.2 = e.2.max(i % w);
        e.3 = e.3.max(i / w);
        if speck_open[l] {
            continue;
        }
        let (x, y) = (i % w, i / w);
        for ny in y.saturating_sub(1)..(y + 2).min(h) {
            for nx in x.saturating_sub(1)..(x + 2).min(w) {
                let j = ny * w + nx;
                if speck_labels[j] as usize == l {
                    continue;
                }
                if carved[j] {
                    speck_carved_nb[l] = true;
                } else if !thin[j] {
                    speck_open[l] = true;
                }
            }
        }
    }
    let mut speck_px = 0u64;
    for i in 0..n {
        let l = speck_labels[i] as usize;
        if l == 0 || speck_open[l] || !speck_carved_nb[l] {
            continue;
        }
        let c = speck_comp[l] as usize;
        let cap = CARVE_SPECK_MIN_PX.max((CARVE_SPECK_FRAC * comp_area[c] as f64) as u64);
        let e = speck_ext[l];
        let extent = (e.2 - e.0 + 1).max(e.3 - e.1 + 1);
        if comp_ok[c] && speck_area[l] <= cap && extent < min_extent {
            carved[i] = true;
            speck_px += 1;
        }
    }
    drop((speck_labels, comp_labels));

    // 5b. Close 1 px gaps along kept structures.
    let kept = |carved: &[bool], x: isize, y: isize| -> bool {
        x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h && {
            let j = y as usize * w + x as usize;
            on[j] && !carved[j]
        }
    };
    let mut restore: Vec<usize> = Vec::new();
    for i in 0..n {
        if !carved[i] {
            continue;
        }
        let (x, y) = ((i % w) as isize, (i / w) as isize);
        let pairs = [
            ((x - 1, y), (x + 1, y)),
            ((x, y - 1), (x, y + 1)),
            ((x - 1, y - 1), (x + 1, y + 1)),
            ((x + 1, y - 1), (x - 1, y + 1)),
        ];
        if pairs
            .iter()
            .any(|&(a, b)| kept(&carved, a.0, a.1) && kept(&carved, b.0, b.1))
        {
            restore.push(i);
        }
    }
    for &i in &restore {
        carved[i] = false;
    }
    protected += restore.len() as u64;
    // Narrow structure (marked lines and narrow product-like parts)
    // within the thin radius of a carved pixel: the spokes, chain and
    // cables the carve went around.
    let carved_plane: Vec<u8> = carved.iter().map(|&c| if c { 255 } else { 0 }).collect();
    let near = square_extreme(&carved_plane, w, h, CARVE_THIN_RADIUS, true);
    protected += (0..n)
        .filter(|&i| narrow[i] && !carved[i] && near[i] != 0)
        .count() as u64;
    drop((carved_plane, near));

    // 5c. Soft inner edge: carved pixels touching kept product.
    let touches_kept = |i: usize| -> bool {
        let (x, y) = ((i % w) as isize, (i / w) as isize);
        (-1..=1).any(|dy| (-1..=1).any(|dx| (dx, dy) != (0, 0) && kept(&carved, x + dx, y + dy)))
    };
    let edge: Vec<bool> = (0..n).map(|i| carved[i] && touches_kept(i)).collect();
    let mut out = mask.clone();
    let (mut carved_px, mut soft_px) = (0u64, 0u64);
    for i in 0..n {
        if !carved[i] {
            continue;
        }
        carved_px += 1;
        let (x, y) = (i % w, i / w);
        let mut alpha = 0u8;
        if edge[i] {
            let (mut fs, mut fnn, mut bs, mut bn) = ([0i64; 3], 0i64, [0i64; 3], 0i64);
            for yy in y.saturating_sub(2)..(y + 3).min(h) {
                for xx in x.saturating_sub(2)..(x + 3).min(w) {
                    let j = yy * w + xx;
                    let near = yy + 1 >= y && yy <= y + 1 && xx + 1 >= x && xx <= x + 1;
                    let px = src_at(j);
                    if near && on[j] && !carved[j] {
                        for c in 0..3 {
                            fs[c] += px[c] as i64;
                        }
                        fnn += 1;
                    } else if carved[j] && !edge[j] {
                        for c in 0..3 {
                            bs[c] += px[c] as i64;
                        }
                        bn += 1;
                    }
                }
            }
            if fnn > 0 && bn > 0 {
                let px = src_at(i);
                let (mut num, mut den) = (0i64, 0i64);
                for c in 0..3 {
                    let f = (fs[c] + fnn / 2) / fnn;
                    let b = (bs[c] + bn / 2) / bn;
                    num += (px[c] as i64 - b) * (f - b);
                    den += (f - b) * (f - b);
                }
                if den >= 300 {
                    let a = ((255 * num + den / 2).div_euclid(den)).clamp(0, 255);
                    if a >= CARVE_SOFT_MIN_ALPHA as i64 {
                        alpha = a.min(CARVE_SOFT_MAX_ALPHA as i64) as u8;
                    }
                }
            }
        }
        if alpha > 0 {
            soft_px += 1;
        }
        out.pixels[(y0 + y) * fw + x0 + x] = alpha;
    }
    let product_after = out.pixels.iter().filter(|&&v| v == 255).count() as u64;
    let reason_applied = format!(
        "open frame ({}): enclosed background {:.2}% of the component (needs {}%)",
        match keyword {
            Some(k) => format!("concept names '{k}'"),
            None => format!("enclosed candidate area {:.2}%", best_cand * 100.0),
        },
        best_bg * 100.0,
        CARVE_MIN_ENCLOSED_FRAC * 100.0
    );
    let record = json!({
        "version": HOLE_CARVE_VERSION,
        "applied": true,
        "reason": reason_applied,
        "regions": regions,
        "carved_pixels": carved_px,
        "protected_thin_pixels": protected,
        "soft_edge_pixels": soft_px,
        "speck_pixels": speck_px,
        "released_line_pixels": released,
        "appendage_pixels_kept": appendage_px,
        "product_pixels_before": product_before,
        "product_pixels_after": product_after,
        "open_frame_keyword": keyword,
        "open_frame": open_frame,
        "outer_band_px": band_px,
        "product_pixels_written": 0,
    });
    (out, record)
}
