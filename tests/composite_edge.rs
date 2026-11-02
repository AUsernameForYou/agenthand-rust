//! Edge fix (docs/EDGE-FIX.md): the cut-out rim on source-derived
//! plates. Synthetic cases for each step (edge band decontamination,
//! plate rim rebuild + contact band, feather capped by the source's own
//! edge softness), the guardrails (product pixels never written, same
//! pixel set and count, thin structures, determinism), the receipt
//! record, and an opt-in run on real artifacts:
//!
//! ```text
//! AGENTHAND_EDGE_ARTIFACTS=<dir with source.png mask.png foreground.png plate.png [staged.png]> \
//! AGENTHAND_EDGE_OUT=<out dir> \
//! cargo test --release --test composite_edge real_artifacts -- --nocapture
//! ```

use agenthand::assets::{AssetStore, KIND_FOREGROUND, KIND_MASK, KIND_PLATE, KIND_SOURCE};
use agenthand::composite::{
    self, alpha_composite, apply_edge_fix, apply_edge_matte, composite_images,
    composite_images_with_edge_fix, composite_pixel, estimate_source_edge_width,
    feather_alpha_for_width, CONTACT_BAND_ALPHA, FEATHER_MAX_ALPHA,
};
use agenthand::image::{GrayImage, RgbaImage};
use agenthand::segmentation::{
    decontaminate_edge_band, extract_foreground, unblend_edge_pixel, EdgeField,
};
use agenthand::{backdrop, staging};
use serde_json::json;
use std::sync::atomic::{AtomicU32, Ordering};

fn build(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> RgbaImage {
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            px.extend_from_slice(&f(x, y));
        }
    }
    RgbaImage::new(w, h, px)
}

fn gray(w: u32, h: u32, f: impl Fn(u32, u32) -> u8) -> GrayImage {
    GrayImage {
        width: w,
        height: h,
        pixels: (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .map(|(x, y)| f(x, y))
            .collect(),
        icc_profile: None,
    }
}

fn luma(p: [u8; 4]) -> u32 {
    (p[0] as u32 * 19595 + p[1] as u32 * 38470 + p[2] as u32 * 7471 + 0x8000) >> 16
}

/// Chebyshev distance from (x, y) to the axis-aligned rectangle.
fn rect_dist(x: u32, y: u32, (x0, y0, x1, y1): (u32, u32, u32, u32)) -> u32 {
    let dx = if x < x0 {
        x0 - x
    } else if x >= x1 {
        x - x1 + 1
    } else {
        0
    };
    let dy = if y < y0 {
        y0 - y
    } else if y >= y1 {
        y - y1 + 1
    } else {
        0
    };
    dx.max(dy)
}

const PRODUCT: [u8; 3] = [40, 42, 46];
const COUNTER: [u8; 3] = [232, 226, 214];
const DARK_PLATE: [u8; 4] = [30, 30, 34, 255];
const RECT: (u32, u32, u32, u32) = (24, 16, 64, 46);

/// The charger case in miniature: a dark product photographed on a
/// bright counter. `ramp` = the photo's edge softness: the source
/// blends product into counter over `ramp` px outside the rectangle
/// (0 = pixel-hard).
fn charger_source(ramp: u32) -> RgbaImage {
    build(88, 62, |x, y| {
        let d = rect_dist(x, y, RECT);
        // coverage of product colour: 1 inside, falling linearly.
        let cov = if d == 0 {
            256
        } else if d <= ramp {
            256 * (ramp + 1 - d) / (ramp + 1)
        } else {
            0
        };
        let mut p = [0u8; 4];
        for c in 0..3 {
            p[c] = ((PRODUCT[c] as u32 * cov + COUNTER[c] as u32 * (256 - cov) + 128) >> 8) as u8;
        }
        // a little counter texture
        if d > ramp {
            p[0] = p[0].saturating_sub(((x * 7 + y * 3) % 5) as u8);
        }
        p[3] = 255;
        p
    })
}

/// Opaque product pixels identical to `fg`, and the count unchanged.
fn assert_product_untouched(fg: &RgbaImage, out: &RgbaImage) -> u64 {
    let mut n = 0u64;
    for i in 0..(fg.width * fg.height) as usize {
        if fg.pixels[i * 4 + 3] == 255 {
            n += 1;
            assert_eq!(
                out.pixels[i * 4..i * 4 + 4],
                fg.pixels[i * 4..i * 4 + 4],
                "product pixel {i} was written"
            );
        }
    }
    n
}

#[test]
fn composite_pixel_is_alpha_composite_exactly() {
    let mut dst = Vec::new();
    let mut src = Vec::new();
    for a in [0u8, 1, 8, 30, 77, 128, 200, 254, 255] {
        for da in [0u8, 90, 255] {
            for v in [0u8, 13, 128, 255] {
                dst.extend_from_slice(&[v, 255 - v, 77, da]);
                src.extend_from_slice(&[200, v, 31, a]);
            }
        }
    }
    let n = (dst.len() / 4) as u32;
    let d = RgbaImage::new(n, 1, dst);
    let s = RgbaImage::new(n, 1, src);
    let whole = alpha_composite(&d, &s);
    for x in 0..n {
        assert_eq!(
            composite_pixel(d.pixel(x, 0), s.pixel(x, 0)),
            whole.pixel(x, 0)
        );
    }
}

#[test]
fn unblend_recovers_product_colour() {
    // 50% product over a bright counter.
    let observed = [136, 134, 130];
    let f = unblend_edge_pixel(observed, 128, COUNTER, None);
    for c in 0..3 {
        assert!(f[c].abs_diff(PRODUCT[c]) <= 3, "{f:?}");
    }
    // Low coverage leans on the local product colour instead of
    // amplifying noise.
    let f = unblend_edge_pixel([220, 214, 203], 10, COUNTER, Some(PRODUCT));
    assert!(f[0] < 120, "{f:?}");
}

/// The acceptance case: dark product, dark plate, bright original
/// background, soft (semi-transparent) edge band. The rim in the band
/// must go dark; product pixels identical; product count unchanged.
#[test]
fn dark_product_on_dark_plate_rim_drops_in_the_edge_band() {
    let source = charger_source(2);
    // A soft mask: product rectangle at 255, its 2 px source ramp soft.
    let mask = gray(88, 62, |x, y| match rect_dist(x, y, RECT) {
        0 => 255,
        1 => 170,
        2 => 85,
        _ => 0,
    });
    let fg = extract_foreground(&source, &mask);
    let plate = RgbaImage::solid(88, 62, DARK_PLATE);
    let band: Vec<usize> = (0..88 * 62)
        .filter(|&i| {
            let a = fg.pixels[i * 4 + 3];
            a > 0 && a < 255
        })
        .collect();
    assert!(!band.is_empty());
    let rim = |img: &RgbaImage| {
        band.iter()
            .map(|&i| luma(img.pixel((i % 88) as u32, (i / 88) as u32)))
            .max()
            .unwrap()
    };

    let (before, before_counts, _) = composite_images(&fg, &plate);
    let (after, after_counts, transformations, report) =
        composite_images_with_edge_fix(&fg, &plate, &source, false);
    assert!(transformations.is_empty());
    assert!(report.applied);
    assert_eq!(report.edge_band_pixels, band.len() as u64);
    assert_eq!(report.decontaminated_pixels, band.len() as u64);
    let (rb, ra) = (rim(&before), rim(&after));
    assert!(rb > 70, "the bug: a bright rim before the fix ({rb})");
    assert!(
        ra < 50,
        "rim after the fix must be dark ({ra}, before {rb})"
    );
    // Interior identical, same pixel set, same count, 0 changed.
    let n = assert_product_untouched(&fg, &after);
    assert_eq!(after_counts.opaque_pixels, n);
    assert_eq!(after_counts.opaque_pixels, before_counts.opaque_pixels);
    assert_eq!(
        after_counts.edge_band_pixels,
        before_counts.edge_band_pixels
    );
    assert_eq!(after_counts.changed_opaque_pixels, 0);
    // The stored layer itself is not modified (the fix works on copies).
    assert_eq!(fg.pixels, extract_foreground(&source, &mask).pixels);
}

/// Binary cut (the SAM path): no soft band exists, so the fix is
/// plate-side. A plate-side rim (a bright ring left by the image model,
/// or the old matte's contaminated ring) is rebuilt from the plate
/// beyond it and darkened as a contact band; the boundary gains one
/// feather ring of PRODUCT colour (never the counter's).
#[test]
fn binary_cut_plate_rim_is_rebuilt_and_boundary_feathered() {
    let source = charger_source(2);
    let mask = gray(
        88,
        62,
        |x, y| if rect_dist(x, y, RECT) == 0 { 255 } else { 0 },
    );
    let fg = extract_foreground(&source, &mask);
    // Dark plate with a bright 2 px rim hugging the footprint.
    let plate = build(88, 62, |x, y| match rect_dist(x, y, RECT) {
        1 | 2 => [190, 186, 178, 255],
        _ => DARK_PLATE,
    });
    let (before, _, _) = composite_images(&fg, &plate);
    let (after, counts, _, report) = composite_images_with_edge_fix(&fg, &plate, &source, false);
    assert!(report.applied);
    assert_eq!(report.edge_band_pixels, 0);
    assert_eq!(report.contact_band_width_px, 3);
    assert!(report.feather_alpha > 0 && report.feather_alpha <= FEATHER_MAX_ALPHA);
    // Mid-edge samples (away from corners): ring 1 and 2 above the top.
    let x = 44;
    for (y, ring) in [(15u32, 1), (14, 2)] {
        let b = luma(before.pixel(x, y));
        let a = luma(after.pixel(x, y));
        assert!(b > 170, "rim before at ring {ring}: {b}");
        assert!(a < 50, "rim after at ring {ring}: {a} (before {b})");
    }
    // Ring 1 carries the feather in product colour: between the plate
    // and the product, never the counter's light.
    let r1 = after.pixel(44, 15);
    let product_luma = luma([PRODUCT[0], PRODUCT[1], PRODUCT[2], 255]);
    assert!(luma(r1) <= product_luma.max(luma(DARK_PLATE)) + 1, "{r1:?}");
    assert!(luma(r1) > luma(DARK_PLATE), "feather present: {r1:?}");
    // Far plate untouched.
    assert_eq!(after.pixel(2, 2), DARK_PLATE);
    let n = assert_product_untouched(&fg, &after);
    assert_eq!(counts.opaque_pixels, n);
    assert_eq!(counts.changed_opaque_pixels, 0);
    assert_eq!(
        counts.edge_band_pixels, 0,
        "the receipt's band is the stored layer's"
    );
}

/// The contaminated product pixels of a binary cut are product pixels:
/// the fix never touches them, however bright (the receipt's claim).
#[test]
fn contaminated_product_pixels_stay_as_photographed() {
    let source = charger_source(2);
    // Mask grown 1 px past the product: its boundary ring is 2/3 counter.
    let mask = gray(
        88,
        62,
        |x, y| if rect_dist(x, y, RECT) <= 1 { 255 } else { 0 },
    );
    let fg = extract_foreground(&source, &mask);
    let plate = RgbaImage::solid(88, 62, DARK_PLATE);
    let (after, counts, _, _) = composite_images_with_edge_fix(&fg, &plate, &source, false);
    assert_product_untouched(&fg, &after);
    assert_eq!(after.pixel(44, 15), source.pixel(44, 15));
    assert_eq!(counts.changed_opaque_pixels, 0);
}

#[test]
fn feather_is_never_softer_than_the_source_edge() {
    let plate = RgbaImage::solid(88, 62, DARK_PLATE);
    let binary = gray(
        88,
        62,
        |x, y| if rect_dist(x, y, RECT) == 0 { 255 } else { 0 },
    );
    let width_of = |ramp: u32| {
        let src = charger_source(ramp);
        let fg = extract_foreground(&src, &binary);
        let field = EdgeField::new(&fg).unwrap();
        let (w, n) = estimate_source_edge_width(&src, &field);
        assert!(n >= 64, "samples {n}");
        (w, src, fg)
    };
    // Pixel-hard photo edge: ramp width 1, no feather at all.
    let (w0, src0, fg0) = width_of(0);
    assert!(w0 <= composite::EDGE_WIDTH_HARD_FX, "ramp {w0}");
    assert_eq!(feather_alpha_for_width(w0), 0);
    let fix = apply_edge_fix(&src0, &fg0, &plate, false);
    assert_eq!(fix.report.feather_pixels, 0);
    // A 1-ring soft photo edge: ramp ~2 px, feather at most the cap and
    // the composite ramp 1/(1-a) no wider than the source's.
    let (w1, _, _) = width_of(1);
    assert!((448..=576).contains(&w1), "ramp {w1}");
    let a1 = feather_alpha_for_width(w1);
    assert!(a1 > 90 && a1 <= FEATHER_MAX_ALPHA, "{a1}");
    assert!(256 * 255 / (255 - a1 as u32) <= w1 + 8);
    // A very soft photo edge (4 px): still capped at about 1 px.
    let (w4, _, _) = width_of(4);
    assert!(w4 > 700, "ramp {w4}");
    assert_eq!(feather_alpha_for_width(w4), FEATHER_MAX_ALPHA);
    // Too few qualifying profiles (flat product on a flat background of
    // the same colour): treated as hard.
    let flat = RgbaImage::solid(88, 62, [90, 90, 90, 255]);
    let fg = extract_foreground(&flat, &binary);
    let (w, n) = estimate_source_edge_width(&flat, &EdgeField::new(&fg).unwrap());
    assert_eq!((w, n), (256, 0));
}

/// Thin structures (3 px pickets): the contact band narrows to 2 px,
/// the third ring is left as the plate had it, nothing is eroded.
#[test]
fn thin_structures_keep_a_narrow_band_and_are_not_eroded() {
    let source = build(90, 60, |x, y| {
        if (10..50).contains(&y) && x % 9 < 3 {
            [50, 45, 40, 255]
        } else {
            [220, 215, 205, 255]
        }
    });
    let fence = gray(90, 60, |x, y| {
        if (10..50).contains(&y) && x % 9 < 3 {
            255
        } else {
            0
        }
    });
    let fg = extract_foreground(&source, &fence);
    let g = staging::analyze_subject(&fg);
    assert!(g.thin);
    let plate = RgbaImage::solid(90, 60, [240, 240, 240, 255]);
    let fix = apply_edge_fix(&source, &fg, &plate, g.thin);
    assert_eq!(fix.report.contact_band_width_px, 2);
    assert!(fix.report.thin_structure);
    let out = alpha_composite(&fix.plate, &fix.foreground);
    // Picket at x 9..12; ring 3 is x 14 (and 7): untouched plate.
    assert_eq!(out.pixel(14, 30), [240, 240, 240, 255]);
    assert_ne!(out.pixel(12, 30), [240, 240, 240, 255]);
    assert_product_untouched(&fg, &out);
    // The non-thin band reaches ring 3.
    let fix3 = apply_edge_fix(&source, &fg, &plate, false);
    assert_eq!(fix3.report.contact_band_width_px, 3);
    let ring3 = alpha_composite(&fix3.plate, &fix3.foreground).pixel(15, 30);
    assert!(ring3 != [240, 240, 240, 255] || CONTACT_BAND_ALPHA[2] == 0);
}

#[test]
fn edge_fix_is_deterministic_and_leaves_the_far_plate_alone() {
    let source = charger_source(1);
    let mask = gray(88, 62, |x, y| match rect_dist(x, y, RECT) {
        0 => 255,
        1 => 128,
        _ => 0,
    });
    let fg = extract_foreground(&source, &mask);
    let plate = build(88, 62, |x, y| [(x * 2) as u8, (y * 3) as u8, 90, 255]);
    let a = composite_images_with_edge_fix(&fg, &plate, &source, false).0;
    let b = composite_images_with_edge_fix(&fg, &plate, &source, false).0;
    assert_eq!(a.pixels, b.pixels);
    // Beyond the 3 px band from the product set nothing changes.
    let (plain, _, _) = composite_images(&fg, &plate);
    for y in 0..62 {
        for x in 0..88 {
            if rect_dist(x, y, RECT) > 3 {
                assert_eq!(a.pixel(x, y), plain.pixel(x, y), "({x},{y})");
            }
        }
    }
    // decontaminate_edge_band on its own: same alpha plane, product
    // pixels copied.
    let (layer, band, fixed) = decontaminate_edge_band(&source, &fg, None);
    assert_eq!(layer.alpha_plane(), fg.alpha_plane());
    assert_eq!(band, fixed);
    assert_product_untouched(&fg, &layer);
}

/// The plate-side matte from `finish_plate` spreads the boundary's
/// contaminated colour onto the plate; the edge fix's rim rebuild
/// replaces it with the plate beyond.
#[test]
fn old_matte_ring_is_rebuilt_from_the_clean_plate() {
    let source = charger_source(2);
    // Mask grown 1 px: boundary pixels carry the counter's light.
    let mask = gray(
        88,
        62,
        |x, y| if rect_dist(x, y, RECT) <= 1 { 255 } else { 0 },
    );
    let fg = extract_foreground(&source, &mask);
    let plate = RgbaImage::solid(88, 62, DARK_PLATE);
    let matted = apply_edge_matte(&plate, &fg, 2);
    let (before, _, _) = composite_images(&fg, &matted);
    let (after, _, _, _) = composite_images_with_edge_fix(&fg, &matted, &source, false);
    // Ring 2 outside the grown mask (y = 13 above the top edge).
    let (b, a) = (luma(before.pixel(44, 13)), luma(after.pixel(44, 13)));
    assert!(a < b, "matte ring {b} -> {a}");
    assert!(a <= luma(DARK_PLATE), "{a}");
}

// ---------------------------------------------------------------------------
// Through the store: the receipt.
// ---------------------------------------------------------------------------

static COUNTER_N: AtomicU32 = AtomicU32::new(0);

fn temp_store() -> (AssetStore, std::path::PathBuf) {
    let n = COUNTER_N.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("agenthand-edge-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    (AssetStore::new(&dir), dir)
}

#[test]
fn receipt_records_the_edge_fix_and_still_verifies() {
    let (store, dir) = temp_store();
    let source = charger_source(2);
    let src_handle = store.put_image(&source, KIND_SOURCE, None).unwrap();
    let src_id = AssetStore::parse_handle(&src_handle).unwrap();
    let mask = gray(
        88,
        62,
        |x, y| if rect_dist(x, y, RECT) == 0 { 255 } else { 0 },
    );
    let mask_handle = store
        .put_gray_image(&mask, KIND_MASK, Some(&src_id))
        .unwrap();
    let mask_id = AssetStore::parse_handle(&mask_handle).unwrap();
    let fg = extract_foreground(&source, &mask);
    let fg_handle = store
        .put_image(&fg, KIND_FOREGROUND, Some(&mask_id))
        .unwrap();
    let geometry = staging::analyze_subject(&fg);
    let plate = staging::finish_plate(&RgbaImage::solid(88, 62, DARK_PLATE), &fg, &geometry);
    let plate_handle = store.put_image(&plate, KIND_PLATE, Some(&src_id)).unwrap();

    for kind in [
        backdrop::PLATE_KIND_SOURCE_EDIT,
        backdrop::PLATE_KIND_SOURCE_BLUR,
    ] {
        let backdrop_value = backdrop::source_plate_receipt_value(
            kind,
            geometry.to_json(),
            json!({}),
            json!({}),
            geometry.shadow,
            geometry.matte_px,
        );
        let (handle, receipt) = composite::draw_product_with_plate_kind(
            &store,
            &fg_handle,
            "desk",
            Some(&plate_handle),
            Some((kind, backdrop_value)),
        )
        .unwrap();
        let ef = &receipt["edge_fix"];
        assert_eq!(ef["applied"], json!(true), "{ef}");
        assert_eq!(ef["product_pixels_written"], json!(0));
        assert_eq!(ef["thin_structure"], json!(false));
        assert!(ef["feather"]["alpha"].as_u64().unwrap() <= FEATHER_MAX_ALPHA as u64);
        assert_eq!(receipt["changed_opaque_pixels"], json!(0));
        assert_eq!(receipt["opaque_pixels"], json!(geometry.opaque_pixels));
        assert_eq!(receipt["edge_band_pixels"], json!(0));
        let report = composite::verify_receipt_from_disk(&store, &handle, Some(&receipt)).unwrap();
        assert_eq!(report["ok"], json!(true), "{}", report["problems"]);
        // The delivered composite really carries the fix.
        let delivered = store.read_image(&handle).unwrap();
        let (plain, _, _) = composite_images(&fg, &plate);
        assert_ne!(delivered.pixels, plain.pixels);
    }

    // Legacy draw (no plate kind): no record, exact old composite.
    let (handle, receipt) = composite::draw_product_with_plate_kind(
        &store,
        &fg_handle,
        "desk",
        Some(&plate_handle),
        None,
    )
    .unwrap();
    assert!(receipt.get("edge_fix").is_none());
    let delivered = store.read_image(&handle).unwrap();
    let (plain, _, _) = composite_images(&fg, &plate);
    assert_eq!(delivered.pixels, plain.pixels);
    // Code-rendered backdrops keep their exact composite too.
    let (handle, receipt) = composite::draw_product_with_plate_kind(
        &store,
        &fg_handle,
        "desk",
        Some(&plate_handle),
        Some(("code_rendered", json!({"mode": "studio_gray"}))),
    )
    .unwrap();
    assert!(receipt.get("edge_fix").is_none());
    assert_eq!(store.read_image(&handle).unwrap().pixels, plain.pixels);
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// Real artifacts (opt-in): before / after 3x edge crops.
// ---------------------------------------------------------------------------

fn load(path: &std::path::Path) -> RgbaImage {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    agenthand::image::decode_image(&bytes).unwrap().into_rgba()
}

/// The nearest product boundary pixel to (cx, cy) within 200 px.
fn nearest_boundary(fg: &RgbaImage, cx: u32, cy: u32) -> (u32, u32) {
    let (w, h) = (fg.width as i64, fg.height as i64);
    let opaque = |x: i64, y: i64| {
        x >= 0 && y >= 0 && x < w && y < h && fg.pixel(x as u32, y as u32)[3] == 255
    };
    let mut best = (i64::MAX, cx, cy);
    for y in (cy as i64 - 200).max(1)..(cy as i64 + 200).min(h - 1) {
        for x in (cx as i64 - 200).max(1)..(cx as i64 + 200).min(w - 1) {
            if opaque(x, y)
                && !(opaque(x - 1, y) && opaque(x + 1, y) && opaque(x, y - 1) && opaque(x, y + 1))
            {
                let d = (x - cx as i64).pow(2) + (y - cy as i64).pow(2);
                if d < best.0 {
                    best = (d, x as u32, y as u32);
                }
            }
        }
    }
    (best.1, best.2)
}

#[test]
fn real_artifacts_before_after() {
    let Ok(dir) = std::env::var("AGENTHAND_EDGE_ARTIFACTS") else {
        eprintln!("AGENTHAND_EDGE_ARTIFACTS not set; skipping the real-artifact run");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let out_dir = std::path::PathBuf::from(
        std::env::var("AGENTHAND_EDGE_OUT").unwrap_or_else(|_| "edge-out".to_string()),
    );
    std::fs::create_dir_all(&out_dir).unwrap();
    let t = std::time::Instant::now();
    let source = load(&dir.join("source.png"));
    let fg = load(&dir.join("foreground.png"));
    let plate = load(&dir.join("plate.png"));
    let mask_bytes = std::fs::read(dir.join("mask.png")).unwrap();
    let mask = match agenthand::image::decode_image(&mask_bytes).unwrap() {
        agenthand::image::DecodedImage::Gray(g) => g.pixels,
        other => other.into_rgba().alpha_plane(),
    };
    eprintln!("decoded in {:.1}s", t.elapsed().as_secs_f64());

    let t = std::time::Instant::now();
    let (before, before_counts, _) = composite_images(&fg, &plate);
    if dir.join("staged.png").exists() {
        let staged = load(&dir.join("staged.png"));
        assert_eq!(
            before.pixels, staged.pixels,
            "plain recomposite reproduces the shipped staged.png"
        );
    }
    let thin = staging::analyze_subject(&fg).thin;
    let (after, after_counts, _, report) =
        composite_images_with_edge_fix(&fg, &plate, &source, thin);
    eprintln!("edge fix + composite in {:.1}s", t.elapsed().as_secs_f64());
    let rerun = composite_images_with_edge_fix(&fg, &plate, &source, thin).0;
    assert_eq!(rerun.pixels, after.pixels, "deterministic");

    // Guardrails on the real thing: same pixel set, same count, 0
    // changed, and every mask-on pixel equals the source photo.
    let n = assert_product_untouched(&fg, &after);
    assert_eq!(after_counts.opaque_pixels, n);
    assert_eq!(after_counts.opaque_pixels, before_counts.opaque_pixels);
    assert_eq!(after_counts.changed_opaque_pixels, 0);
    let mut checked = 0u64;
    for (i, &m) in mask.iter().enumerate() {
        if m == 255 {
            checked += 1;
            assert_eq!(
                after.pixels[i * 4..i * 4 + 4],
                source.pixels[i * 4..i * 4 + 4]
            );
        }
    }
    assert_eq!(checked, n);

    // Rim metric: mean |luma step| between plate ring 1-2 and the clean
    // plate 4-6 px out, before vs after (plate-side rim strength).
    let field = EdgeField::new(&fg).unwrap();
    let ring_mean = |img: &RgbaImage, lo: u8, hi: u8| {
        let (mut s, mut k) = (0u64, 0u64);
        for y in 0..field.h {
            for x in 0..field.w {
                let d = field.out_o[y * field.w + x];
                if d >= lo && d <= hi {
                    let j = field.frame_offset(x, y);
                    s += luma([img.pixels[j], img.pixels[j + 1], img.pixels[j + 2], 255]) as u64;
                    k += 1;
                }
            }
        }
        s as f64 / k.max(1) as f64
    };
    let summary = json!({
        "product_pixels": n,
        "changed_product_pixels": after_counts.changed_opaque_pixels,
        "mask_on_pixels_equal_source": checked,
        "edge_fix": report.to_json(),
        "mean_luma": {
            "before": {"ring1": ring_mean(&before, 1, 1), "ring2": ring_mean(&before, 2, 2), "ring4_6": ring_mean(&before, 4, 6)},
            "after": {"ring1": ring_mean(&after, 1, 1), "ring2": ring_mean(&after, 2, 2), "ring4_6": ring_mean(&after, 4, 6)},
        },
    });
    eprintln!("{}", serde_json::to_string_pretty(&summary).unwrap());
    std::fs::write(
        out_dir.join("bike_edge_summary.json"),
        serde_json::to_string_pretty(&summary).unwrap(),
    )
    .unwrap();

    // 3x crops: rows of [source | before | after] around boundary points.
    let points: Vec<(u32, u32)> = std::env::var("AGENTHAND_EDGE_POINTS")
        .ok()
        .map(|s| {
            s.split(';')
                .filter_map(|p| {
                    let (x, y) = p.split_once(',')?;
                    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
                })
                .collect()
        })
        .unwrap_or_else(|| {
            vec![
                (2120, 1440),
                (1200, 2140),
                (3640, 400),
                (4480, 2200),
                (3100, 1600),
            ]
        });
    let (half, scale, gap) = (50u32, 3u32, 6u32);
    let tile = 2 * half * scale;
    let cols = 3u32;
    let (gw, gh) = (
        cols * tile + (cols + 1) * gap,
        points.len() as u32 * (tile + gap) + gap,
    );
    let mut grid = image::RgbImage::from_pixel(gw, gh, image::Rgb([24, 24, 24]));
    for (row, &(px, py)) in points.iter().enumerate() {
        let (cx, cy) = nearest_boundary(&fg, px, py);
        for (col, img) in [&source, &before, &after].into_iter().enumerate() {
            let ox = gap + col as u32 * (tile + gap);
            let oy = gap + row as u32 * (tile + gap);
            for ty in 0..tile {
                for tx in 0..tile {
                    let sx = (cx + tx / scale).saturating_sub(half).min(img.width - 1);
                    let sy = (cy + ty / scale).saturating_sub(half).min(img.height - 1);
                    let p = img.pixel(sx, sy);
                    grid.put_pixel(ox + tx, oy + ty, image::Rgb([p[0], p[1], p[2]]));
                }
            }
        }
        eprintln!("crop {row}: boundary point ({cx},{cy})");
    }
    let path = out_dir.join("bike_edge_crops_raw.png");
    grid.save(&path).unwrap();
    eprintln!("wrote {}", path.display());
    if std::env::var("AGENTHAND_EDGE_WRITE_FULL").is_ok() {
        let full =
            image::RgbaImage::from_raw(after.width, after.height, after.pixels.clone()).unwrap();
        full.save(out_dir.join("bike_edge_after.png")).unwrap();
    }
}
