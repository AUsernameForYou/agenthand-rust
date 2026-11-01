//! Code-rendered backdrop plates (decisions 59/60).
//!
//! Two stage modes bypass the image model entirely: the plate is
//! rendered by code at the foreground's frame size, stored as a
//! plate asset, and composited through the same Fidelity Lock draw
//! path as a generated plate, so the receipt pins its hashes and
//! the run spends $0 on the stage.
//!
//! - `wallpaper`: white ground with the logo arc-circle motif
//!   (three 92-degree arcs, 28-degree gaps centred at 90/210/330
//!   degrees, stroke width 0.08 x diameter), diameter 72 px, brand
//!   cyan at 40% opacity, on a regular grid with offset rows. No
//!   contact shadow.
//! - `studio_gray`: a radial gradient, #F4F5F7 at the centre
//!   easing to #C6CAD0 at the edges.
//!
//! Rendering is deterministic: the same frame size always yields
//! the same pixels (the motif is supersampled 4x and box-averaged,
//! the gradient is evaluated per pixel with a smoothstep falloff).
//!
//! draw's own plate choice lives here too (`resolve_draw_plate`):
//! `source_edit` builds the plate from the original photo (see
//! `staging`), receipted as plate kind `source_edit`, or `source_blur`
//! when the edit call failed and the code-only plate stood in;
//! `generated` keeps the legacy text-only plate (`model_generated`).

use serde_json::{json, Value};

use crate::composite::alpha_composite;
use crate::image::RgbaImage;

/// Stage mode: the generated-scene draw (the default).
pub const STAGE_MODE_SCENE: &str = "scene";
/// Stage mode: white ground with the arc motif.
pub const STAGE_MODE_WALLPAPER: &str = "wallpaper";
/// Stage mode: radial studio gray.
pub const STAGE_MODE_STUDIO_GRAY: &str = "studio_gray";

/// Every stage mode the room runner accepts.
pub const STAGE_MODES: [&str; 3] = [
    STAGE_MODE_SCENE,
    STAGE_MODE_WALLPAPER,
    STAGE_MODE_STUDIO_GRAY,
];

/// Motif diameter for the wallpaper plate, in pixels.
pub const WALLPAPER_MOTIF_DIAMETER_PX: u32 = 72;
/// Motif opacity for the wallpaper plate (0..1).
pub const WALLPAPER_MOTIF_OPACITY: f64 = 0.4;
/// Brand cyan of the motif.
pub const WALLPAPER_MOTIF_RGB: [u8; 3] = [0, 175, 225];

const SUPERSAMPLE: u32 = 4;
/// Arc spans in degrees (PIL convention: clockwise from 3 o'clock,
/// y axis down). Each arc is 92 degrees; the gaps between them are
/// 28 degrees centred at 90, 210 and 330 degrees.
const ARCS: [(f64, f64); 3] = [(104.0, 196.0), (224.0, 316.0), (344.0, 436.0)];
const STROKE_RATIO: f64 = 0.08;
const GRID_PITCH_RATIO: f64 = 1.55;

fn angle_on_arc(angle_deg: f64) -> bool {
    ARCS.iter().any(|(start, end)| {
        let rel = (angle_deg - start).rem_euclid(360.0);
        rel <= end - start
    })
}

/// The wallpaper motif as a transparent overlay at supersampled
/// resolution, ready to be box-averaged down to the frame size.
fn motif_overlay_ss(width: u32, height: u32) -> RgbaImage {
    let ss = SUPERSAMPLE;
    let big_w = width * ss;
    let big_h = height * ss;
    let mut overlay = RgbaImage::solid(big_w, big_h, [0, 0, 0, 0]);
    let alpha = (255.0 * WALLPAPER_MOTIF_OPACITY).round() as u8; // 102, as in the reference
    let diam = WALLPAPER_MOTIF_DIAMETER_PX * ss;
    let radius = diam as f64 / 2.0;
    let stroke = ((STROKE_RATIO * diam as f64).round() as u32).max(2) as f64;
    let pitch =
        ((WALLPAPER_MOTIF_DIAMETER_PX as f64 * GRID_PITCH_RATIO).round() as i64) * ss as i64;
    if pitch <= 0 {
        return overlay;
    }
    let reach = radius + stroke;
    let mut row: i64 = 0;
    let mut cy = -pitch / 2;
    while cy < big_h as i64 + pitch {
        let xoff = if row % 2 == 1 { pitch / 2 } else { 0 };
        let mut cx = -pitch / 2 + xoff;
        while cx < big_w as i64 + pitch {
            let x0 = ((cx as f64 - reach).floor() as i64).max(0);
            let x1 = ((cx as f64 + reach).ceil() as i64).min(big_w as i64);
            let y0 = ((cy as f64 - reach).floor() as i64).max(0);
            let y1 = ((cy as f64 + reach).ceil() as i64).min(big_h as i64);
            for y in y0..y1 {
                for x in x0..x1 {
                    let dx = x as f64 + 0.5 - cx as f64;
                    let dy = y as f64 + 0.5 - cy as f64;
                    let dist = (dx * dx + dy * dy).sqrt();
                    if (dist - radius).abs() > stroke / 2.0 {
                        continue;
                    }
                    let angle = dy.atan2(dx).to_degrees().rem_euclid(360.0);
                    if !angle_on_arc(angle) {
                        continue;
                    }
                    let i = ((y as u32 * big_w + x as u32) * 4) as usize;
                    overlay.pixels[i..i + 4].copy_from_slice(&[
                        WALLPAPER_MOTIF_RGB[0],
                        WALLPAPER_MOTIF_RGB[1],
                        WALLPAPER_MOTIF_RGB[2],
                        alpha,
                    ]);
                }
            }
            cx += pitch;
        }
        cy += pitch;
        row += 1;
    }
    overlay
}

/// Box-average a supersampled RGBA image down by SUPERSAMPLE.
fn box_downsample(img: &RgbaImage) -> RgbaImage {
    let ss = SUPERSAMPLE as usize;
    let out_w = img.width / SUPERSAMPLE;
    let out_h = img.height / SUPERSAMPLE;
    let mut out = vec![0u8; (out_w * out_h * 4) as usize];
    let n = (ss * ss) as u32;
    for oy in 0..out_h as usize {
        for ox in 0..out_w as usize {
            let mut sums = [0u32; 4];
            for sy in 0..ss {
                for sx in 0..ss {
                    let src = (((oy * ss + sy) * img.width as usize) + ox * ss + sx) * 4;
                    for (c, sum) in sums.iter_mut().enumerate() {
                        *sum += img.pixels[src + c] as u32;
                    }
                }
            }
            let dst = (oy * out_w as usize + ox) * 4;
            for (c, sum) in sums.iter().enumerate() {
                out[dst + c] = ((sum + n / 2) / n) as u8;
            }
        }
    }
    RgbaImage::new(out_w, out_h, out)
}

/// The wallpaper plate at a frame size: white ground, cyan arc
/// motif, no shadow of any kind.
pub fn render_wallpaper(width: u32, height: u32) -> RgbaImage {
    let ground = RgbaImage::solid(width, height, [255, 255, 255, 255]);
    let motif = box_downsample(&motif_overlay_ss(width, height));
    alpha_composite(&ground, &motif)
}

/// The studio gray plate at a frame size: radial gradient from
/// #F4F5F7 at (0.5w, 0.42h) to #C6CAD0 at the edges, with the
/// smoothstep falloff of the reference renderer.
pub fn render_studio_gray(width: u32, height: u32) -> RgbaImage {
    let center = [244.0f64, 245.0, 247.0];
    let edge = [198.0f64, 202.0, 208.0];
    let cx = width as f64 * 0.5;
    let cy = height as f64 * 0.42;
    let nx = width as f64 * 0.72;
    let ny = height as f64 * 0.72;
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            let dx = (x as f64 - cx) / nx;
            let dy = (y as f64 - cy) / ny;
            let dist = (dx * dx + dy * dy).sqrt().clamp(0.0, 1.0);
            let t = dist * dist * (3.0 - 2.0 * dist);
            let i = ((y * width + x) * 4) as usize;
            for c in 0..3 {
                pixels[i + c] = (center[c] * (1.0 - t) + edge[c] * t) as u8;
            }
            pixels[i + 3] = 255;
        }
    }
    RgbaImage::new(width, height, pixels)
}

/// Render the plate for a stage mode; None for `scene` (the plate
/// there comes from the image model, not from code).
pub fn render_plate(stage_mode: &str, width: u32, height: u32) -> Option<RgbaImage> {
    match stage_mode {
        STAGE_MODE_WALLPAPER => Some(render_wallpaper(width, height)),
        STAGE_MODE_STUDIO_GRAY => Some(render_studio_gray(width, height)),
        _ => None,
    }
}

/// The receipt `backdrop` value for a stage mode: the motif
/// parameters for wallpaper, the kind alone for studio gray, and
/// null for the model-generated and placeholder paths.
pub fn backdrop_receipt_value(stage_mode: &str) -> Value {
    match stage_mode {
        STAGE_MODE_WALLPAPER => json!({
            "kind": "wallpaper",
            "motif_diameter_px": WALLPAPER_MOTIF_DIAMETER_PX,
            "opacity": WALLPAPER_MOTIF_OPACITY,
            "shadow": false,
        }),
        STAGE_MODE_STUDIO_GRAY => json!({"kind": "studio_gray"}),
        _ => Value::Null,
    }
}

/// True when the stage mode renders its plate in code.
pub fn is_backdrop_mode(stage_mode: &str) -> bool {
    matches!(stage_mode, STAGE_MODE_WALLPAPER | STAGE_MODE_STUDIO_GRAY)
}

/// draw plate mode: build the plate from the original photo.
pub const DRAW_PLATE_SOURCE_EDIT: &str = "source_edit";
/// draw plate mode: the legacy text-only generated scene.
pub const DRAW_PLATE_GENERATED: &str = "generated";
/// Every draw plate mode, default first.
pub const DRAW_PLATE_MODES: [&str; 2] = [DRAW_PLATE_SOURCE_EDIT, DRAW_PLATE_GENERATED];

/// Receipt plate kind: Muse Image edit of the item-masked original.
pub const PLATE_KIND_SOURCE_EDIT: &str = "source_edit";
/// Receipt plate kind: the code-only plate (masked original, filled,
/// blurred, lifted) used when the edit call failed.
pub const PLATE_KIND_SOURCE_BLUR: &str = "source_blur";

/// The plate mode for one draw call: the call's own `plate` argument,
/// else `AGENTHAND_DRAW_PLATE`, else `source_edit`. Unknown values
/// refuse (no silent fallback to a different staging method).
pub fn resolve_draw_plate(requested: Option<&str>) -> Result<&'static str, String> {
    let env = crate::config::get_draw_plate_mode();
    let raw = match requested.map(str::trim).filter(|s| !s.is_empty()) {
        Some(value) => value.to_string(),
        None if !env.is_empty() => env,
        None => return Ok(DRAW_PLATE_SOURCE_EDIT),
    };
    DRAW_PLATE_MODES
        .iter()
        .find(|m| **m == raw)
        .copied()
        .ok_or_else(|| {
            format!("plate {raw:?} is not a draw plate mode (expected one of {DRAW_PLATE_MODES:?})")
        })
}

/// The receipt `backdrop` value for a source-derived plate: what was
/// sent, what came back (or why the code plate stood in), and what
/// was drawn on the plate around the product.
pub fn source_plate_receipt_value(
    kind: &str,
    subject: Value,
    reference: Value,
    edit: Value,
    shadow: bool,
    matte_px: u32,
) -> Value {
    json!({
        "kind": kind,
        "reference": reference,
        "edit": edit,
        "contact_shadow": shadow,
        "edge_matte_px": matte_px,
        "subject": subject,
    })
}
