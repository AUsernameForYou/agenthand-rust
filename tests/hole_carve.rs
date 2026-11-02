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
//! (d) a ring whose interior matches the product colour: no-op.
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
    carve_holes, carve_holes_with, open_frame_keyword, HOLE_CARVE_ENV,
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
