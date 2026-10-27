//! Segmentation quality control and the bounded mask-recovery loop —
//! port of the Python `segmentation.py`.
//!
//! - `assess_mask` quality-checks one decoded mask (coverage sane,
//!   dominated by one connected region).
//! - `extract_foreground` builds the transparent foreground: source
//!   pixels exactly where the mask is on, transparent black elsewhere.
//! - `run_attempts` tries the concept phrase, retries ONCE with the
//!   first alternate, and traces every attempt.

use crate::image::{GrayImage, RgbaImage};
use crate::meta;
use crate::pricing::Usage;
use crate::sam::{parse_sam_masks, DecodedMask};
use serde_json::{json, Value};

pub const MIN_COVERAGE: f64 = 0.001;
pub const MAX_COVERAGE: f64 = 0.98;
pub const MIN_DOMINANT_FRACTION: f64 = 0.75;

pub const OVERLAY_OUTLINE_RGBA: [u8; 4] = [0, 175, 225, 255];

#[derive(Debug)]
pub struct CutOutError {
    pub code: String, // NO_MATCH | POOR_MASK | BAD_MODEL_RESPONSE
    pub message: String,
    pub attempts: Vec<Value>,
}

pub struct MaskAssessment {
    pub mask_pixels: u64,
    pub coverage: f64,
    pub components: u64,
    pub dominant_fraction: f64,
    pub accepted: bool,
    pub rejection: Option<&'static str>,
}

/// (component_count, largest_component_size) of a 0/1 raster,
/// 4-connectivity flood fill.
fn label_components(binary: &[u8], width: usize, height: usize) -> (u64, u64) {
    let mut visited = vec![false; binary.len()];
    let mut components = 0u64;
    let mut largest = 0u64;
    let mut stack: Vec<usize> = Vec::new();
    for start in 0..binary.len() {
        if binary[start] == 0 || visited[start] {
            continue;
        }
        components += 1;
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
        largest = largest.max(size);
    }
    let _ = height;
    (components, largest)
}

pub fn assess_mask(mask: &GrayImage) -> MaskAssessment {
    let width = mask.width as usize;
    let height = mask.height as usize;
    let frame_pixels = (width * height) as f64;
    let binary: Vec<u8> = mask.pixels.iter().map(|&v| if v != 0 { 1 } else { 0 }).collect();
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
    let (components, largest) = label_components(&binary, width, height);
    let dominant = largest as f64 / mask_pixels as f64;
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
/// Binary masks select exactly; a partial mask value blends per
/// Pillow's `Image.composite` (never produced by the SAM path, whose
/// rasters are binary by construction).
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
            // Image.composite blend against transparent black.
            for c in 0..4 {
                out[j + c] = ((source.pixels[j + c] as u32 * m) / 255) as u8;
            }
        }
    }
    RgbaImage {
        width: source.width,
        height: source.height,
        pixels: out,
        icc_profile: source.icc_profile.clone(),
    }
}

fn choose_mask(masks: &[DecodedMask]) -> &DecodedMask {
    // Python's max() keeps the FIRST maximum on ties; Rust's
    // max_by_key keeps the last, so scan manually.
    let mut best = &masks[0];
    for m in &masks[1..] {
        if m.mask_pixels > best.mask_pixels {
            best = m;
        }
    }
    best
}

pub struct AttemptsOutcome {
    pub mask_image: GrayImage,
    pub phrase: String,
    pub attempts: Vec<Value>,
    pub usage: Usage,
}

/// Run the bounded attempt loop. Performs network I/O; must only run
/// as the fn of `billed_call`.
pub fn run_attempts(
    client: &reqwest::blocking::Client,
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
    for phrase in phrases {
        let (output_text, _usage) =
            meta::segment_image(client, key, png_bytes, phrase, model)
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
        let chosen = choose_mask(&masks);
        let mask_gray = GrayImage {
            width: frame_width,
            height: frame_height,
            pixels: chosen.image.clone(),
            icc_profile: None,
        };
        let assessment = assess_mask(&mask_gray);
        trace["object_id"] = json!(chosen.object_id);
        trace["coverage"] = json!(crate::round6(assessment.coverage));
        trace["mask_pixels"] = json!(assessment.mask_pixels);
        trace["components"] = json!(assessment.components);
        trace["dominant_fraction"] = json!(crate::round6(assessment.dominant_fraction));
        if !assessment.accepted {
            trace["outcome"] = json!("poor_mask");
            trace["rejection"] = json!(assessment.rejection);
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
    Err(CutOutError {
        code: last_code.to_string(),
        message: format!(
            "No usable mask after {} attempt(s); final outcome: {last_code}.",
            attempts.len()
        ),
        attempts,
    })
}
