//! Hole carving (docs/HOLE-CARVE.md): SAM returns a wheel as a solid
//! disc; the background seen between the spokes must go back to the
//! plate while every spoke, the hub and the outer silhouette stay
//! product. Synthetic cases:
//!
//! (a) ring + 12 spokes + hub over a textured bright background:
//!     >= 95% of the enclosed background removed, >= 98% of the spoke
//!     pixels kept, the outer silhouette unchanged;
//! (b) a solid box: no-op;
//! (c) `AGENTHAND_HOLE_CARVE=off`: the identical mask;
//! (d) a ring whose interior matches the product colour: no-op;
//! (e) the completion pass (step 4e): a wheel whose lower interior is
//!     shaded paving, dark and close to the tyre in colour, so the colour
//!     test proves almost none of it; the completion releases it while the
//!     spokes, the tyre and the hub stay;
//! (f) `AGENTHAND_HOLE_COMPLETE=off`: the colour-proven carve only;
//! (g) the enclosed-remainder gate: the colour-only carve of (e) leaves
//!     most of the shaded interior and is refused with a retake;
//! (h) the pale-part guard: a reflector plate with a brass bolt inside the
//!     wheel of (e) stays product while the rest completes;
//! (i) faint lines: spokes dimmed under the thin contrast floor in the
//!     shade stay continuous while the shade completes.
//!
//! Plus an opt-in run on a real cut_out (mask.png + source.png):
//!
//! ```text
//! AGENTHAND_CARVE_FIXTURE=<dir with source.png mask.png> \
//! AGENTHAND_CARVE_OUT=<out dir> [AGENTHAND_CARVE_CONCEPT="blue kids BMX bike"] \
//! cargo test --release --test hole_carve real_fixture -- --ignored --nocapture
//! ```

use agenthand::image::{GrayImage, RgbaImage};
use agenthand::segmentation::{
    carve_holes, carve_holes_opts, carve_holes_with, hole_carve_gate_refusal, open_frame_keyword,
    HOLE_CARVE_ENV, HOLE_CARVE_GATE_CODE, HOLE_COMPLETE_ENV,
};
use serde_json::{json, Value};
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

const W: u32 = 640;
const H: u32 = 640;
const CX: f64 = 320.0;
const CY: f64 = 320.0;
const TYRE_OUT: f64 = 270.0;
const RIM_IN: f64 = 228.0;
const HUB: f64 = 26.0;
const SPOKES: usize = 12;
const SPOKE_HALF_WIDTH: f64 = 2.6;

/// Deterministic per-pixel noise in [-1, 1].
fn noise(x: u32, y: u32, salt: u32) -> f64 {
    let mut v = x.wrapping_mul(374_761_393) ^ y.wrapping_mul(668_265_263) ^ salt.wrapping_mul(2_246_822_519);
    v = (v ^ (v >> 13)).wrapping_mul(1_274_126_177);
    v ^= v >> 16;
    (v % 2001) as f64 / 1000.0 - 1.0
}

/// Bright textured backyard: sunlit stone with grain and darker mortar
/// courses every 48 px (lines with real line contrast).
fn background(x: u32, y: u32) -> [u8; 4] {
    let n = noise(x, y, 1) * 14.0 + noise(x / 3, y / 3, 2) * 10.0;
    let mortar = (y % 48) < 3;
    let base = if mortar { [168.0, 150.0, 118.0] } else { [214.0, 196.0, 158.0] };
    [
        (base[0] + n).clamp(0.0, 255.0) as u8,
        (base[1] + n).clamp(0.0, 255.0) as u8,
        (base[2] + n * 0.8).clamp(0.0, 255.0) as u8,
        255,
    ]
}

fn polar(x: u32, y: u32) -> (f64, f64) {
    let (dx, dy) = (x as f64 + 0.5 - CX, y as f64 + 0.5 - CY);
    ((dx * dx + dy * dy).sqrt(), dy.atan2(dx))
}

/// Distance from (x, y) to the nearest spoke centre line (spokes run
/// from the hub to the rim).
fn spoke_distance(x: u32, y: u32) -> f64 {
    let (dx, dy) = (x as f64 + 0.5 - CX, y as f64 + 0.5 - CY);
    let mut best = f64::MAX;
    for k in 0..SPOKES {
        let a = (k as f64 + 0.25) * std::f64::consts::TAU / SPOKES as f64;
        let (ux, uy) = (a.cos(), a.sin());
        let along = dx * ux + dy * uy;
        if along < 0.0 {
            continue;
        }
        let across = (dx * uy - dy * ux).abs();
        best = best.min(across);
    }
    best
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Part {
    Background,
    Tyre,
    Spoke,
    Hub,
    /// Background seen through the wheel (inside the rim, not a spoke or the hub).
    Enclosed,
}

fn part(x: u32, y: u32) -> Part {
    let (r, _) = polar(x, y);
    if r > TYRE_OUT {
        Part::Background
    } else if r >= RIM_IN {
        Part::Tyre
    } else if r <= HUB {
        Part::Hub
    } else if spoke_distance(x, y) <= SPOKE_HALF_WIDTH {
        Part::Spoke
    } else {
        Part::Enclosed
    }
}

fn wheel_scene(interior_matches_product: bool) -> (RgbaImage, GrayImage) {
    let mut px = Vec::with_capacity((W * H * 4) as usize);
    let mut mask = Vec::with_capacity((W * H) as usize);
    for y in 0..H {
        for x in 0..W {
            let n = noise(x, y, 7) * 4.0;
            let c = match part(x, y) {
                Part::Background => background(x, y),
                Part::Tyre => [(28.0 + n) as u8, (28.0 + n) as u8, (32.0 + n) as u8, 255],
                Part::Hub => [(52.0 + n) as u8, (52.0 + n) as u8, (58.0 + n) as u8, 255],
                Part::Spoke => [(88.0 + n) as u8, (90.0 + n) as u8, (96.0 + n) as u8, 255],
                Part::Enclosed if interior_matches_product => {
                    [(28.0 + n) as u8, (28.0 + n) as u8, (32.0 + n) as u8, 255]
                }
                Part::Enclosed => background(x, y),
            };
            px.extend_from_slice(&c);
            // SAM-style solid disc a little past the tyre.
            let (r, _) = polar(x, y);
            mask.push(if r <= TYRE_OUT + 2.0 { 255 } else { 0 });
        }
    }
    (
        RgbaImage::new(W, H, px),
        GrayImage { width: W, height: H, pixels: mask, icc_profile: None },
    )
}

fn count_where(mask: &GrayImage, f: impl Fn(u32, u32, u8) -> bool) -> u64 {
    let mut n = 0;
    for y in 0..mask.height {
        for x in 0..mask.width {
            if f(x, y, mask.pixels[(y * mask.width + x) as usize]) {
                n += 1;
            }
        }
    }
    n
}

fn assert_never_raised(before: &GrayImage, after: &GrayImage) {
    assert_eq!(before.pixels.len(), after.pixels.len());
    for (i, (&b, &a)) in before.pixels.iter().zip(after.pixels.iter()).enumerate() {
        assert!(a <= b, "pixel {i} raised from {b} to {a}");
        if b != 255 {
            assert_eq!(a, b, "pixel {i} outside the product set changed");
        }
    }
}

// ---------------------------------------------------------------------------
// (a) Wheel over a textured bright background.
// ---------------------------------------------------------------------------

#[test]
fn wheel_background_is_carved_and_spokes_hub_and_silhouette_stay() {
    let (source, mask) = wheel_scene(false);
    let (out, record) = carve_holes_with(&source, &mask, "bicycle wheel", true);
    assert_eq!(record["applied"], json!(true), "{record:#}");
    assert_never_raised(&mask, &out);

    let enclosed = count_where(&mask, |x, y, _| part(x, y) == Part::Enclosed);
    let removed = count_where(&out, |x, y, v| part(x, y) == Part::Enclosed && v < 255);
    let cleared = count_where(&out, |x, y, v| part(x, y) == Part::Enclosed && v == 0);
    let removed_frac = removed as f64 / enclosed as f64;
    let cleared_frac = cleared as f64 / enclosed as f64;
    assert!(
        removed_frac >= 0.95,
        "only {:.2}% of the enclosed background removed (cleared {:.2}%): {record:#}",
        removed_frac * 100.0,
        cleared_frac * 100.0
    );

    let spokes = count_where(&mask, |x, y, _| part(x, y) == Part::Spoke);
    let spokes_kept = count_where(&out, |x, y, v| part(x, y) == Part::Spoke && v == 255);
    let spoke_frac = spokes_kept as f64 / spokes as f64;
    assert!(
        spoke_frac >= 0.98,
        "only {:.2}% of {spokes} spoke pixels kept: {record:#}",
        spoke_frac * 100.0
    );
    let hub = count_where(&mask, |x, y, _| part(x, y) == Part::Hub);
    let hub_kept = count_where(&out, |x, y, v| part(x, y) == Part::Hub && v == 255);
    assert!(hub_kept as f64 >= 0.98 * hub as f64, "hub {hub_kept}/{hub}");

    // Outer silhouette: the tyre and everything outside it are untouched.
    for y in 0..H {
        for x in 0..W {
            let i = (y * W + x) as usize;
            if matches!(part(x, y), Part::Tyre | Part::Background) {
                assert_eq!(out.pixels[i], mask.pixels[i], "silhouette pixel ({x},{y}) changed");
            }
        }
    }

    // The record: additive fields, counts that match the mask.
    for key in ["applied", "regions", "carved_pixels", "protected_thin_pixels", "reason"] {
        assert!(record.get(key).is_some(), "record lacks {key}: {record:#}");
    }
    assert!(record["regions"].as_u64().unwrap() >= 1);
    let carved = count_where(&out, |_, _, v| v < 255) - count_where(&mask, |_, _, v| v < 255);
    assert_eq!(record["carved_pixels"].as_u64().unwrap(), carved, "{record:#}");
    let product_after = count_where(&out, |_, _, v| v == 255);
    assert_eq!(record["product_pixels_after"].as_u64().unwrap(), product_after);
    assert_eq!(
        record["product_pixels_before"].as_u64().unwrap(),
        count_where(&mask, |_, _, v| v == 255)
    );
    assert!(record["protected_thin_pixels"].as_u64().unwrap() > 0, "{record:#}");
    assert!(record["reason"].as_str().unwrap().contains("open frame"), "{record:#}");

    // Soft inner edge: partial alpha only on the carved side, capped.
    assert!(out.pixels.iter().all(|&v| v == 0 || v == 255 || v <= 128));

    // Deterministic.
    let (again, record2) = carve_holes_with(&source, &mask, "bicycle wheel", true);
    assert_eq!(again.pixels, out.pixels);
    assert_eq!(record2, record);
}

#[test]
fn wheel_is_carved_without_a_keyword_when_it_is_an_open_frame() {
    let (source, mask) = wheel_scene(false);
    let (out, record) = carve_holes_with(&source, &mask, "product", true);
    assert_eq!(record["applied"], json!(true), "{record:#}");
    assert_eq!(record["open_frame_keyword"], Value::Null);
    let frac = record["open_frame"]["enclosed_candidate_fraction"].as_f64().unwrap();
    assert!(frac >= 0.1, "{record:#}");
    assert!(out.pixels.iter().filter(|&&v| v < 255).count() > mask.pixels.iter().filter(|&&v| v < 255).count());
}

// ---------------------------------------------------------------------------
// (b) A solid box: nothing enclosed, no-op.
// ---------------------------------------------------------------------------

#[test]
fn solid_box_is_a_no_op() {
    let (w, h) = (480u32, 360u32);
    let inside = |x: u32, y: u32| (90..390).contains(&x) && (70..290).contains(&y);
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    let mut mask = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            if inside(x, y) {
                let n = noise(x, y, 3) * 6.0;
                // A blue box with a darker label stripe (product line work).
                let stripe = (180..186).contains(&y);
                let base = if stripe { [20.0, 30.0, 70.0] } else { [40.0, 80.0, 190.0] };
                px.extend_from_slice(&[
                    (base[0] + n) as u8,
                    (base[1] + n) as u8,
                    (base[2] + n) as u8,
                    255,
                ]);
                mask.push(255);
            } else {
                px.extend_from_slice(&background(x, y));
                mask.push(0);
            }
        }
    }
    let source = RgbaImage::new(w, h, px);
    let mask = GrayImage { width: w, height: h, pixels: mask, icc_profile: None };
    for concept in ["cardboard box", "bike rack box"] {
        let (out, record) = carve_holes_with(&source, &mask, concept, true);
        assert_eq!(record["applied"], json!(false), "{concept}: {record:#}");
        assert_eq!(record["carved_pixels"], json!(0));
        assert_eq!(record["regions"], json!(0));
        assert!(record["reason"].as_str().is_some_and(|r| !r.is_empty()), "{record:#}");
        assert_eq!(out.pixels, mask.pixels, "{concept}");
    }
}

// ---------------------------------------------------------------------------
// (c) Kill switch.
// ---------------------------------------------------------------------------

#[test]
fn kill_switch_returns_the_identical_mask() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var(HOLE_CARVE_ENV).ok();
    let (source, mask) = wheel_scene(false);

    for off in ["off", "OFF", "0", "false", "no"] {
        std::env::set_var(HOLE_CARVE_ENV, off);
        let (out, record) = carve_holes(&source, &mask, "bicycle wheel");
        assert_eq!(out.pixels, mask.pixels, "{off}");
        assert_eq!(out.width, mask.width);
        assert_eq!(out.height, mask.height);
        assert_eq!(record["applied"], json!(false));
        assert_eq!(record["carved_pixels"], json!(0));
        assert!(record["reason"].as_str().unwrap().contains(HOLE_CARVE_ENV), "{record:#}");
    }
    // Explicitly disabled through the API too.
    let (out, _) = carve_holes_with(&source, &mask, "bicycle wheel", false);
    assert_eq!(out.pixels, mask.pixels);

    // Unset or "on": the carve runs.
    for on in ["on", ""] {
        if on.is_empty() {
            std::env::remove_var(HOLE_CARVE_ENV);
        } else {
            std::env::set_var(HOLE_CARVE_ENV, on);
        }
        let (out, record) = carve_holes(&source, &mask, "bicycle wheel");
        assert_eq!(record["applied"], json!(true), "{on:?}: {record:#}");
        assert_ne!(out.pixels, mask.pixels);
    }

    match previous {
        Some(v) => std::env::set_var(HOLE_CARVE_ENV, v),
        None => std::env::remove_var(HOLE_CARVE_ENV),
    }
}

// ---------------------------------------------------------------------------
// (d) A ring whose interior is product-coloured: nothing to carve.
// ---------------------------------------------------------------------------

#[test]
fn ring_with_product_coloured_interior_is_a_no_op() {
    let (source, mask) = wheel_scene(true);
    let (out, record) = carve_holes_with(&source, &mask, "bicycle wheel", true);
    assert_eq!(record["applied"], json!(false), "{record:#}");
    assert_eq!(record["carved_pixels"], json!(0));
    assert_eq!(out.pixels, mask.pixels);
}

// ---------------------------------------------------------------------------
// (e) Completion pass: a wheel half in shade.
// ---------------------------------------------------------------------------

/// Rows at or below this y are in a cast shadow, inside and outside the
/// wheel (the ring outside the tyre sees the shaded paving too).
const SHADE_Y: u32 = 340;

fn shaded(_x: u32, y: u32) -> bool {
    y >= SHADE_Y
}

/// Shaded paving: dark, nearly neutral, textured, a few levels above the
/// tyre (28, 28, 32). The colour test cannot prove it background.
fn shade(x: u32, y: u32) -> [u8; 4] {
    let tex = 8.0;
    let n = noise(x, y, 11) * tex + noise(x / 4, y / 4, 12) * tex * 0.6;
    [
        (40.0 + n).clamp(0.0, 255.0) as u8,
        (38.0 + n).clamp(0.0, 255.0) as u8,
        (39.0 + n * 0.9).clamp(0.0, 255.0) as u8,
        255,
    ]
}

fn shaded_wheel_scene() -> (RgbaImage, GrayImage) {
    let mut px = Vec::with_capacity((W * H * 4) as usize);
    let mut mask = Vec::with_capacity((W * H) as usize);
    for y in 0..H {
        for x in 0..W {
            let n = noise(x, y, 7) * 4.0;
            let ground = if shaded(x, y) { shade(x, y) } else { background(x, y) };
            let c = match part(x, y) {
                Part::Background | Part::Enclosed => ground,
                Part::Tyre => [(28.0 + n) as u8, (28.0 + n) as u8, (32.0 + n) as u8, 255],
                Part::Hub => [(70.0 + n) as u8, (72.0 + n) as u8, (82.0 + n) as u8, 255],
                Part::Spoke => [(88.0 + n) as u8, (90.0 + n) as u8, (96.0 + n) as u8, 255],
            };
            px.extend_from_slice(&c);
            let (r, _) = polar(x, y);
            mask.push(if r <= TYRE_OUT + 2.0 { 255 } else { 0 });
        }
    }
    (
        RgbaImage::new(W, H, px),
        GrayImage { width: W, height: H, pixels: mask, icc_profile: None },
    )
}

/// Share of the enclosed (seen-through) pixels on the shaded / sunlit side
/// that the carve removed (alpha below 255).
fn removed_share(out: &GrayImage, want_shade: bool) -> f64 {
    let total = count_where(out, |x, y, _| part(x, y) == Part::Enclosed && shaded(x, y) == want_shade);
    let removed =
        count_where(out, |x, y, v| part(x, y) == Part::Enclosed && shaded(x, y) == want_shade && v < 255);
    removed as f64 / total as f64
}

fn kept_share(out: &GrayImage, which: Part) -> f64 {
    let total = count_where(out, |x, y, _| part(x, y) == which);
    let kept = count_where(out, |x, y, v| part(x, y) == which && v == 255);
    kept as f64 / total as f64
}

#[test]
fn shaded_wheel_interior_is_completed_and_spokes_tyre_hub_stay() {
    let (source, mask) = shaded_wheel_scene();

    // The colour-proven carve alone (completion off): the sunlit windows
    // go, the shaded paving stays product.
    let (base, base_rec) = carve_holes_opts(&source, &mask, "bicycle wheel", true, false);
    assert_eq!(base_rec["applied"], json!(true), "{base_rec:#}");
    assert_eq!(base_rec["completion"]["enabled"], json!(false));
    assert_eq!(base_rec["completion"]["released_pixels"], json!(0));
    let base_shade = removed_share(&base, true);
    assert!(base_shade <= 0.10, "colour-only carve removed {:.1}% of the shade", base_shade * 100.0);
    assert!(removed_share(&base, false) >= 0.95, "{base_rec:#}");

    // With the completion pass.
    let (out, record) = carve_holes_opts(&source, &mask, "bicycle wheel", true, true);
    assert_eq!(record["applied"], json!(true), "{record:#}");
    assert_never_raised(&mask, &out);
    let shade_removed = removed_share(&out, true);
    assert!(
        shade_removed >= 0.70,
        "completion removed only {:.1}% of the shaded interior (colour-only: {:.1}%): {record:#}",
        shade_removed * 100.0,
        base_shade * 100.0
    );
    assert!(removed_share(&out, false) >= 0.95, "{record:#}");
    let released = record["completion"]["released_pixels"].as_u64().unwrap();
    assert!(released > 0 && record["completion"]["enabled"] == json!(true), "{record:#}");
    assert!(record["completion"]["cells_released"].as_u64().unwrap() >= 1, "{record:#}");

    // Structure stays: spokes (also the ones crossing the shade), the hub,
    // and the tyre and everything outside it byte for byte.
    let spokes = kept_share(&out, Part::Spoke);
    assert!(spokes >= 0.98, "only {:.2}% of spoke pixels kept: {record:#}", spokes * 100.0);
    let shaded_spokes = count_where(&out, |x, y, _| part(x, y) == Part::Spoke && shaded(x, y));
    let shaded_spokes_kept =
        count_where(&out, |x, y, v| part(x, y) == Part::Spoke && shaded(x, y) && v == 255);
    assert!(shaded_spokes_kept as f64 >= 0.98 * shaded_spokes as f64, "{shaded_spokes_kept}/{shaded_spokes}");
    assert!(kept_share(&out, Part::Hub) >= 0.98, "{record:#}");
    for y in 0..H {
        for x in 0..W {
            let i = (y * W + x) as usize;
            if matches!(part(x, y), Part::Tyre | Part::Background) {
                assert_eq!(out.pixels[i], mask.pixels[i], "tyre/outside pixel ({x},{y}) changed");
            }
        }
    }
    // The completion only adds windows to the colour-only carve: every
    // pixel the colour-only carve removed outside the soft inner edge is
    // removed here too.
    for (i, (&b, &a)) in base.pixels.iter().zip(out.pixels.iter()).enumerate() {
        if b == 0 {
            assert!(a < 255, "pixel {i} carved by the colour-only pass is product again");
        }
    }

    // The record counts match the mask; the gate passes.
    let carved = count_where(&out, |_, _, v| v < 255) - count_where(&mask, |_, _, v| v < 255);
    assert_eq!(record["carved_pixels"].as_u64().unwrap(), carved, "{record:#}");
    assert_eq!(
        record["product_pixels_after"].as_u64().unwrap(),
        count_where(&out, |_, _, v| v == 255)
    );
    assert_eq!(record["gate"]["applies"], json!(true), "{record:#}");
    assert_eq!(record["gate"]["pass"], json!(true), "{record:#}");
    assert!(hole_carve_gate_refusal(&record).is_none());

    eprintln!(
        "shaded wheel: colour-only carve removed {:.1}% of the shade (gate {}), completion {:.1}% (gate {}); spokes kept {:.2}%",
        base_shade * 100.0,
        base_rec["gate"]["remainder_fraction"],
        shade_removed * 100.0,
        record["gate"]["remainder_fraction"],
        spokes * 100.0
    );

    // Deterministic.
    let (again, record2) = carve_holes_opts(&source, &mask, "bicycle wheel", true, true);
    assert_eq!(again.pixels, out.pixels);
    assert_eq!(record2, record);
}

/// A spoke reflector: a pale, compact plate (with prism texture and a
/// brass bolt in its middle) between two spokes, half over the sunlit
/// paving and half over the shade.
fn in_reflector(x: u32, y: u32) -> bool {
    // A 120 x 34 px plate centred at (400, 330), along the x axis.
    let (dx, dy) = (x as f64 + 0.5 - 400.0, y as f64 + 0.5 - 330.0);
    dx.abs() <= 60.0 && dy.abs() <= 17.0
}

fn in_bolt(x: u32, y: u32) -> bool {
    let (dx, dy) = (x as f64 + 0.5 - 400.0, y as f64 + 0.5 - 330.0);
    dx * dx + dy * dy <= 10.0 * 10.0
}

fn reflector_wheel_scene() -> (RgbaImage, GrayImage) {
    let (source, mask) = shaded_wheel_scene();
    let mut px = source.pixels.clone();
    for y in 0..H {
        for x in 0..W {
            if !in_reflector(x, y) || part(x, y) == Part::Spoke {
                continue;
            }
            let i = ((y * W + x) * 4) as usize;
            let c = if in_bolt(x, y) {
                [196u8, 172, 104]
            } else {
                // Prism texture: a 6 px grid of slightly darker dots.
                let dot = (x % 6 < 2) && (y % 6 < 2);
                let v = if dot { 196.0 } else { 222.0 } + noise(x, y, 21) * 4.0;
                [v as u8, v as u8, (v + 6.0) as u8]
            };
            px[i..i + 3].copy_from_slice(&c);
        }
    }
    (RgbaImage::new(W, H, px), mask)
}

#[test]
fn reflector_and_bolt_stay_product_inside_a_completed_wheel() {
    let (source, mask) = reflector_wheel_scene();
    let (out, record) = carve_holes_opts(&source, &mask, "bicycle wheel", true, true);
    assert_eq!(record["applied"], json!(true), "{record:#}");
    assert_never_raised(&mask, &out);
    let plate = count_where(&out, |x, y, _| in_reflector(x, y) && part(x, y) != Part::Spoke);
    let lost = count_where(&out, |x, y, v| in_reflector(x, y) && part(x, y) != Part::Spoke && v < 255);
    assert_eq!(lost, 0, "{lost} of {plate} reflector pixels released: {record:#}");
    let bolt_lost = count_where(&out, |x, y, v| in_bolt(x, y) && v < 255);
    assert_eq!(bolt_lost, 0, "{bolt_lost} bolt pixels released: {record:#}");
    // The rest of the wheel still completes and the spokes stay.
    assert!(removed_share(&out, true) >= 0.60, "{record:#}");
    assert!(kept_share(&out, Part::Spoke) >= 0.98, "{record:#}");
    assert!(record["gate"]["pass"] == json!(true), "{record:#}");
}

/// A dark paver joint across the shaded part of the wheel: a straight,
/// non-radial band 12 px wide (about 5% of the rim radius, like the 40 px
/// joints at r = 760 px on IMG_1443), darker than the shade.
fn in_band(x: u32, y: u32) -> bool {
    (420..432).contains(&y) && part(x, y) == Part::Enclosed
}

#[test]
fn dark_joint_band_releases_while_spokes_reflector_and_bolt_stay() {
    let (source, mask) = reflector_wheel_scene();
    let mut px = source.pixels.clone();
    for y in 0..H {
        for x in 0..W {
            if in_band(x, y) {
                let i = ((y * W + x) * 4) as usize;
                let v = 24.0 + noise(x, y, 31) * 4.0;
                px[i..i + 3].copy_from_slice(&[v as u8, (v - 1.0) as u8, v as u8]);
            }
        }
    }
    let source = RgbaImage::new(W, H, px);
    let (out, record) = carve_holes_opts(&source, &mask, "bicycle wheel", true, true);
    assert_eq!(record["applied"], json!(true), "{record:#}");
    assert_never_raised(&mask, &out);
    let band = count_where(&out, |x, y, _| in_band(x, y));
    let band_released = count_where(&out, |x, y, v| in_band(x, y) && v < 255);
    assert!(
        band_released as f64 >= 0.6 * band as f64,
        "only {band_released} of {band} band pixels released: {record:#}"
    );
    assert!(record["completion"]["strip_pixels"].as_u64().unwrap() > 0, "{record:#}");
    let crossing = count_where(&out, |x, y, _| (420..432).contains(&y) && part(x, y) == Part::Spoke);
    let crossing_kept =
        count_where(&out, |x, y, v| (420..432).contains(&y) && part(x, y) == Part::Spoke && v == 255);
    assert!(crossing_kept as f64 >= 0.95 * crossing as f64, "{crossing_kept}/{crossing} spoke px kept in the band");
    assert!(kept_share(&out, Part::Spoke) >= 0.98, "{record:#}");
    assert_eq!(count_where(&out, |x, y, v| in_reflector(x, y) && part(x, y) != Part::Spoke && v < 255), 0);
    assert_eq!(count_where(&out, |x, y, v| in_bolt(x, y) && v < 255), 0);
    assert_eq!(record["gate"]["pass"], json!(true), "{record:#}");
    // With the completion off the band is left and counted as residue.
    let (_, off) = carve_holes_opts(&source, &mask, "bicycle wheel", true, false);
    assert!(off["gate"]["strip_residue_pixels"].as_u64().unwrap() > 0, "{off:#}");
}

/// The longest run of non-product pixels along each spoke's centre line,
/// from just outside the hub to just inside the rim, in the shade at least
/// one line square (13 px) from the shadow edge.
fn shaded_spoke_gaps(out: &GrayImage) -> Vec<usize> {
    (0..SPOKES)
        .map(|k| {
            let a = (k as f64 + 0.25) * std::f64::consts::TAU / SPOKES as f64;
            let (ux, uy) = (a.cos(), a.sin());
            let (mut run, mut worst) = (0usize, 0usize);
            let mut t = HUB + 4.0;
            while t <= RIM_IN - 4.0 {
                let (x, y) = ((CX + t * ux).floor() as u32, (CY + t * uy).floor() as u32);
                // Inside the shade, away from its edge: a spoke grazing the
                // shadow edge sits in line contrast from the edge itself and
                // is not protected (docs/HOLE-CARVE.md Limits).
                if shaded(x, y) && y >= SHADE_Y + 2 * 6 + 1 {
                    let kept = (-1i32..=1).any(|o| {
                        let (xx, yy) = ((x as i32 + o * uy.round() as i32) as u32, (y as i32 - o * ux.round() as i32) as u32);
                        out.pixels[(yy * W + xx) as usize] == 255
                    });
                    run = if kept { 0 } else { run + 1 };
                    worst = worst.max(run);
                }
                t += 1.0;
            }
            worst
        })
        .collect()
}

#[test]
fn dim_spokes_in_the_shade_stay_continuous() {
    // The shaded wheel with smooth shade (IMG_1443's shaded paving varies
    // a few levels) and the spokes in the shade dimmed to about 20 levels
    // above it (under the thin contrast floor).
    let (source, mask) = shaded_wheel_scene();
    let mut px = source.pixels.clone();
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            if shaded(x, y) && matches!(part(x, y), Part::Enclosed | Part::Background) {
                let v = 40.0 + noise(x, y, 11) * 3.0;
                px[i..i + 3].copy_from_slice(&[v as u8, (v - 2.0) as u8, (v - 1.0) as u8]);
            }
            if part(x, y) == Part::Spoke && shaded(x, y) {
                let i = ((y * W + x) * 4) as usize;
                let v = 60.0 + noise(x, y, 41) * 3.0;
                px[i..i + 3].copy_from_slice(&[v as u8, v as u8, (v + 3.0) as u8]);
            }
        }
    }
    let source = RgbaImage::new(W, H, px);
    let (out, record) = carve_holes_opts(&source, &mask, "bicycle wheel", true, true);
    assert_eq!(record["applied"], json!(true), "{record:#}");
    assert_never_raised(&mask, &out);
    // The shade still completes...
    assert!(removed_share(&out, true) >= 0.60, "{record:#}");
    // ...and every spoke is continuous through it.
    let gaps = shaded_spoke_gaps(&out);
    assert!(gaps.iter().all(|&g| g <= 2), "spoke gaps in the shade (px): {gaps:?}: {record:#}");
    let shaded_spokes = count_where(&out, |x, y, _| part(x, y) == Part::Spoke && shaded(x, y));
    let kept = count_where(&out, |x, y, v| part(x, y) == Part::Spoke && shaded(x, y) && v == 255);
    assert!(kept as f64 >= 0.9 * shaded_spokes as f64, "{kept}/{shaded_spokes}: {record:#}");
}

// ---------------------------------------------------------------------------
// (f) The completion kill switch.
// ---------------------------------------------------------------------------

#[test]
fn completion_kill_switch_keeps_the_colour_proven_carve() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev_carve = std::env::var(HOLE_CARVE_ENV).ok();
    let prev_complete = std::env::var(HOLE_COMPLETE_ENV).ok();
    std::env::remove_var(HOLE_CARVE_ENV);
    let (source, mask) = shaded_wheel_scene();
    let (colour_only, colour_rec) = carve_holes_opts(&source, &mask, "bicycle wheel", true, false);
    let (complete, complete_rec) = carve_holes_opts(&source, &mask, "bicycle wheel", true, true);
    assert_ne!(colour_only.pixels, complete.pixels);

    for off in ["off", "OFF", "0", "false", "no"] {
        std::env::set_var(HOLE_COMPLETE_ENV, off);
        let (out, record) = carve_holes(&source, &mask, "bicycle wheel");
        assert_eq!(out.pixels, colour_only.pixels, "{off}");
        assert_eq!(record, colour_rec, "{off}");
        assert_eq!(record["applied"], json!(true));
        assert_eq!(record["completion"]["enabled"], json!(false));
        assert_eq!(record["completion"]["released_pixels"], json!(0));
        // The gate still measures the colour-only carve (and fails it).
        assert_eq!(record["gate"]["applies"], json!(true));
        assert_eq!(record["gate"]["pass"], json!(false), "{record:#}");
    }
    // Unset or "on": the completion runs.
    for on in ["on", ""] {
        if on.is_empty() {
            std::env::remove_var(HOLE_COMPLETE_ENV);
        } else {
            std::env::set_var(HOLE_COMPLETE_ENV, on);
        }
        let (out, record) = carve_holes(&source, &mask, "bicycle wheel");
        assert_eq!(out.pixels, complete.pixels, "{on:?}");
        assert_eq!(record, complete_rec, "{on:?}");
        assert_eq!(record["completion"]["enabled"], json!(true));
    }
    // AGENTHAND_HOLE_CARVE=off still turns all carving off, completion too.
    std::env::remove_var(HOLE_COMPLETE_ENV);
    std::env::set_var(HOLE_CARVE_ENV, "off");
    let (out, record) = carve_holes(&source, &mask, "bicycle wheel");
    assert_eq!(out.pixels, mask.pixels);
    assert_eq!(record["applied"], json!(false));
    assert_eq!(record["completion"]["enabled"], json!(false));
    assert_eq!(record["gate"]["applies"], json!(false));

    for (name, prev) in [(HOLE_CARVE_ENV, prev_carve), (HOLE_COMPLETE_ENV, prev_complete)] {
        match prev {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
    }
}

// ---------------------------------------------------------------------------
// (g) The enclosed-remainder gate.
// ---------------------------------------------------------------------------

#[test]
fn gate_refuses_a_partial_open_frame_carve_with_a_retake() {
    let (source, mask) = shaded_wheel_scene();
    // The partial carve: windows proven by colour only, the shaded half of
    // the wheel left as product.
    let (_, record) = carve_holes_opts(&source, &mask, "bicycle wheel", true, false);
    let gate = &record["gate"];
    assert_eq!(gate["applies"], json!(true), "{record:#}");
    assert_eq!(gate["pass"], json!(false), "{record:#}");
    let fraction = gate["remainder_fraction"].as_f64().unwrap();
    let limit = gate["max_remainder_fraction"].as_f64().unwrap();
    assert!(fraction > limit, "{record:#}");
    let remainder = gate["remainder_pixels"].as_u64().unwrap();
    let enclosed = gate["enclosed_candidate_pixels"].as_u64().unwrap();
    assert_eq!(
        enclosed,
        gate["accepted_window_pixels"].as_u64().unwrap()
            + gate["completed_window_pixels"].as_u64().unwrap()
            + remainder
    );
    // Most of the shaded seen-through area is the remainder.
    let shade_px = count_where(&mask, |x, y, _| part(x, y) == Part::Enclosed && shaded(x, y));
    assert!(remainder as f64 >= 0.5 * shade_px as f64, "{remainder} of {shade_px}: {record:#}");

    // The structured refusal cut_out returns (decision 114: the failure
    // names the remainder and the retake that fixes it).
    let (code, message, extra) = hole_carve_gate_refusal(&record).expect("the gate failed");
    assert_eq!(code, HOLE_CARVE_GATE_CODE);
    assert_eq!(code, "HOLE_CARVE_INCOMPLETE");
    assert!(message.contains(&format!("{remainder} px")), "{message}");
    assert!(message.contains(&format!("{enclosed} px")), "{message}");
    assert!(message.contains("refused"), "{message}");
    assert!(message.contains("Retake:"), "{message}");
    assert!(message.contains("shade"), "{message}");
    assert_eq!(extra["remainder"]["remainder_pixels"], json!(remainder));
    assert_eq!(extra["remainder"]["enclosed_candidate_pixels"], json!(enclosed));
    assert_eq!(extra["remainder"]["remainder_fraction"], gate["remainder_fraction"]);
    assert_eq!(extra["remainder"]["max_remainder_fraction"], json!(limit));
    let retake = extra["retake"].as_array().unwrap();
    assert_eq!(retake.len(), 3);
    assert!(retake.iter().all(|r| r.as_str().is_some_and(|t| !t.is_empty())));
    assert!(extra["operator_override"].as_str().unwrap().contains(HOLE_CARVE_ENV));

    // The completed carve of the same photo passes; so does the sunlit
    // wheel; a carve that did not apply has no gate to fail.
    let (_, done) = carve_holes_opts(&source, &mask, "bicycle wheel", true, true);
    assert_eq!(done["gate"]["pass"], json!(true), "{done:#}");
    assert!(hole_carve_gate_refusal(&done).is_none());
    let (bright_src, bright_mask) = wheel_scene(false);
    let (_, bright) = carve_holes_opts(&bright_src, &bright_mask, "bicycle wheel", true, true);
    assert_eq!(bright["gate"]["pass"], json!(true), "{bright:#}");
    assert!(hole_carve_gate_refusal(&bright).is_none());
    let (ring_src, ring_mask) = wheel_scene(true);
    let (_, ring) = carve_holes_opts(&ring_src, &ring_mask, "bicycle wheel", true, true);
    assert_eq!(ring["applied"], json!(false));
    assert_eq!(ring["gate"]["applies"], json!(false));
    assert!(hole_carve_gate_refusal(&ring).is_none());
}

// ---------------------------------------------------------------------------
// Small pieces of the contract.
// ---------------------------------------------------------------------------

#[test]
fn open_frame_keywords_match_whole_word_prefixes() {
    assert_eq!(open_frame_keyword("blue kids BMX bike"), Some("bike"));
    assert_eq!(open_frame_keyword("Bicycles"), Some("bicycle"));
    assert_eq!(open_frame_keyword("wheelchair"), Some("wheel"));
    assert_eq!(open_frame_keyword("bar stool"), Some("stool"));
    assert_eq!(open_frame_keyword("track jacket"), None);
    assert_eq!(open_frame_keyword("wicker basket"), Some("basket"));
    assert_eq!(open_frame_keyword(""), None);
}

#[test]
fn empty_and_mismatched_masks_are_no_ops() {
    let (source, mask) = wheel_scene(false);
    let empty = GrayImage { width: W, height: H, pixels: vec![0; (W * H) as usize], icc_profile: None };
    let (out, record) = carve_holes_with(&source, &empty, "bike", true);
    assert_eq!(out.pixels, empty.pixels);
    assert_eq!(record["applied"], json!(false));

    let small = GrayImage { width: 10, height: 10, pixels: vec![255; 100], icc_profile: None };
    let (out, record) = carve_holes_with(&source, &small, "bike", true);
    assert_eq!(out.pixels, small.pixels);
    assert_eq!(record["applied"], json!(false));
    assert_eq!(mask.pixels.len(), empty.pixels.len());
}

// ---------------------------------------------------------------------------
// Opt-in: a real cut_out (before | after overlay, 2x crop of each wheel).
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn real_fixture() {
    let (Ok(dir), Ok(out_dir)) = (
        std::env::var("AGENTHAND_CARVE_FIXTURE"),
        std::env::var("AGENTHAND_CARVE_OUT"),
    ) else {
        eprintln!("set AGENTHAND_CARVE_FIXTURE and AGENTHAND_CARVE_OUT to run");
        return;
    };
    let concept =
        std::env::var("AGENTHAND_CARVE_CONCEPT").unwrap_or_else(|_| "blue kids BMX bike".into());
    let dir = std::path::Path::new(&dir);
    let out_dir = std::path::Path::new(&out_dir);
    std::fs::create_dir_all(out_dir).unwrap();
    let source = image::open(dir.join("source.png")).unwrap().to_rgba8();
    let mask = image::open(dir.join("mask.png")).unwrap().to_luma8();
    let (w, h) = source.dimensions();
    let src = RgbaImage::new(w, h, source.as_raw().clone());
    let m = GrayImage { width: w, height: h, pixels: mask.as_raw().clone(), icc_profile: None };

    let t = std::time::Instant::now();
    let (carved, mut record) = carve_holes_with(&src, &m, &concept, true);
    let elapsed = t.elapsed();
    record["elapsed_ms"] = json!(elapsed.as_millis() as u64);
    record["concept"] = json!(concept);
    std::fs::write(
        out_dir.join("bike_carve_record.json"),
        serde_json::to_string_pretty(&record).unwrap(),
    )
    .unwrap();
    image::GrayImage::from_raw(w, h, carved.pixels.clone())
        .unwrap()
        .save(out_dir.join("mask_carved.png"))
        .unwrap();

    // Overlay: product tinted blue; carved pixels (product before, not
    // after) in red.
    let overlay = |mm: &GrayImage, mark: Option<&GrayImage>| -> image::RgbImage {
        image::RgbImage::from_fn(w, h, |x, y| {
            let i = (y * w + x) as usize;
            let s = &src.pixels[i * 4..i * 4 + 3];
            let a = mm.pixels[i] as f64 / 255.0 * 0.45;
            let mut c = [0u8; 3];
            for k in 0..3 {
                let tint = [40.0, 90.0, 255.0][k];
                c[k] = (s[k] as f64 * (1.0 - a) + tint * a).round() as u8;
            }
            if let Some(before) = mark {
                if before.pixels[i] == 255 && mm.pixels[i] < 255 {
                    let r = 0.7 * (1.0 - mm.pixels[i] as f64 / 255.0);
                    for k in 0..3 {
                        let tint = [255.0, 40.0, 40.0][k];
                        c[k] = (s[k] as f64 * (1.0 - r) + tint * r).round() as u8;
                    }
                }
            }
            image::Rgb(c)
        })
    };
    let before = overlay(&m, None);
    let after = overlay(&carved, Some(&m));
    // Wheel boxes: the carved pixels' bbox split at the product's centre
    // column into a left and a right wheel, squared and padded.
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) as usize;
            if m.pixels[i] == 255 && carved.pixels[i] < 255 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    assert!(x0 < x1, "nothing carved: {record:#}");
    let mid = (x0 + x1) / 2;
    let side = |a: u32, b: u32| -> (u32, u32, u32) {
        let (mut l, mut r) = (u32::MAX, 0);
        let (mut t, mut btm) = (u32::MAX, 0);
        for y in y0..=y1 {
            for x in a..=b {
                let i = (y * w + x) as usize;
                if m.pixels[i] == 255 && carved.pixels[i] < 255 {
                    l = l.min(x);
                    r = r.max(x);
                    t = t.min(y);
                    btm = btm.max(y);
                }
            }
        }
        let size = ((r - l).max(btm - t) as f64 * 1.25) as u32;
        let cx = (l + r) / 2;
        let cy = (t + btm) / 2;
        (cx.saturating_sub(size / 2).min(w - size), cy.saturating_sub(size / 2).min(h - size), size)
    };
    let boxes = [side(x0, mid), side(mid + 1, x1)];

    // Layout: overview before | after on top (each 1/s of the photo),
    // then each wheel before | after at 2x the overview scale.
    let ov_w = 1200u32;
    let ov_h = ov_w * h / w;
    let scale2 = 2.0 * ov_w as f64 / w as f64;
    let crops: Vec<(image::RgbImage, image::RgbImage)> = boxes
        .iter()
        .map(|&(bx, by, size)| {
            let px = ((size as f64) * scale2).round() as u32;
            let cb = image::imageops::crop_imm(&before, bx, by, size, size).to_image();
            let ca = image::imageops::crop_imm(&after, bx, by, size, size).to_image();
            (
                image::imageops::resize(&cb, px, px, image::imageops::FilterType::Lanczos3),
                image::imageops::resize(&ca, px, px, image::imageops::FilterType::Lanczos3),
            )
        })
        .collect();
    let crop_h: u32 = crops.iter().map(|(c, _)| c.height()).sum::<u32>() + 12 * crops.len() as u32;
    let total_w = (2 * ov_w + 12).max(crops.iter().map(|(c, _)| 2 * c.width() + 12).max().unwrap_or(0));
    let mut sheet = image::RgbImage::from_pixel(total_w, ov_h + 12 + crop_h, image::Rgb([24, 24, 24]));
    let small_b = image::imageops::resize(&before, ov_w, ov_h, image::imageops::FilterType::Triangle);
    let small_a = image::imageops::resize(&after, ov_w, ov_h, image::imageops::FilterType::Triangle);
    image::imageops::overlay(&mut sheet, &small_b, 0, 0);
    image::imageops::overlay(&mut sheet, &small_a, (ov_w + 12) as i64, 0);
    let mut yy = ov_h + 12;
    for (cb, ca) in &crops {
        image::imageops::overlay(&mut sheet, cb, 0, yy as i64);
        image::imageops::overlay(&mut sheet, ca, (cb.width() + 12) as i64, yy as i64);
        yy += cb.height() + 12;
    }
    let file = std::fs::File::create(out_dir.join("bike_carve_before_after.jpg")).unwrap();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(std::io::BufWriter::new(file), 88);
    enc.encode_image(&sheet).unwrap();
    println!("{}", serde_json::to_string_pretty(&record).unwrap());
    println!("carve took {elapsed:?}; wheel boxes {boxes:?}");
}
