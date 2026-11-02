//! Shadow carry-over (docs/SHADOW-CARRY.md): the product's own cast
//! shadow read from the ORIGINAL photo against the finished plate and
//! multiplied back onto the plate before the product goes down.
//! Synthetic cases: (a) a known soft cast shadow right of a product box
//! is carried; (b) incoherent pairs stay contact_only; (c) the kill
//! switch restores the previous composite exactly; (d) generated plate
//! kinds are contact_only "generated plate". Plus an opt-in run on a
//! real staged photo:
//!
//! ```text
//! AGENTHAND_SHADOW_ARTIFACTS=<dir with source.png foreground.png plate.png staged.png> \
//! AGENTHAND_SHADOW_OUT=<out dir> \
//! cargo test --release --test composite_shadow -- --ignored --nocapture
//! ```

use agenthand::assets::{AssetStore, KIND_FOREGROUND, KIND_MASK, KIND_PLATE, KIND_SOURCE};
use agenthand::composite::{
    self, alpha_composite, carry_shadow_for_plate_kind, carry_source_shadow, composite_images,
    composite_images_with_edge_fix, shadow_record_of, ShadowReport, SHADOW_COHERENCE_MIN,
    SHADOW_ENV, SHADOW_GENERATED_REASON, SHADOW_RATIO_MIN, SHADOW_RATIO_MIN_ATTACHED,
};
use agenthand::image::{GrayImage, RgbaImage};
use agenthand::segmentation::extract_foreground;
use agenthand::{backdrop, staging};
use serde_json::json;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

const W: u32 = 240;
const H: u32 = 160;
/// The product box (half-open).
const BOX: (u32, u32, u32, u32) = (60, 40, 120, 110);
/// The cast shadow's support: right of the box, in its lower half and
/// running past its bottom edge; 4 px linear ramps on the open edges.
const SHADOW: (u32, u32, u32, u32) = (120, 78, 210, 128);
const RAMP: f64 = 4.0;
const PRODUCT_RGB: [u8; 3] = [40, 90, 200];

fn build(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> RgbaImage {
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            px.extend_from_slice(&f(x, y));
        }
    }
    RgbaImage::new(w, h, px)
}

fn hash(x: u32, y: u32, seed: u32) -> u32 {
    // murmur3 fmix32 over a mixed key.
    let mut v = x.wrapping_mul(0x9E37_79B1)
        ^ y.wrapping_mul(0x85EB_CA77).rotate_left(13)
        ^ seed.wrapping_mul(0xC2B2_AE3D).rotate_left(7);
    v ^= v >> 16;
    v = v.wrapping_mul(0x85EB_CA6B);
    v ^= v >> 13;
    v = v.wrapping_mul(0xC2B2_AE35);
    v ^= v >> 16;
    v
}

fn luma(p: &[u8]) -> f64 {
    (p[0] as f64 * 19595.0 + p[1] as f64 * 38470.0 + p[2] as f64 * 7471.0) / 65536.0
}

fn in_box(x: u32, y: u32) -> bool {
    x >= BOX.0 && x < BOX.2 && y >= BOX.1 && y < BOX.3
}

/// Ground with a little luma-only texture (the same in photo and plate).
fn ground(x: u32, y: u32) -> [f64; 3] {
    let n = (hash(x, y, 7) % 7) as f64 - 3.0;
    [150.0 + n, 140.0 + n, 120.0 + n]
}

/// Shadow strength in [0, 1]: full inside, linear ramps on the three
/// open edges, attached to the box on the left.
fn shadow_strength(x: u32, y: u32) -> f64 {
    let (sx0, sy0, sx1, sy1) = SHADOW;
    if x < sx0 || x >= sx1 || y < sy0 || y >= sy1 {
        return 0.0;
    }
    let ramp = |d: f64| ((d + 0.5) / RAMP).clamp(0.0, 1.0);
    let rx = ramp((sx1 - 1 - x) as f64);
    let ry = ramp((y - sy0) as f64).min(ramp((sy1 - 1 - y) as f64));
    rx * ry
}

fn to_px(rgb: [f64; 3]) -> [u8; 4] {
    [
        rgb[0].round().clamp(0.0, 255.0) as u8,
        rgb[1].round().clamp(0.0, 255.0) as u8,
        rgb[2].round().clamp(0.0, 255.0) as u8,
        255,
    ]
}

/// (source photo, product layer, plate): the photo has the product and
/// its cast shadow (ratio 0.5 in the umbra); the plate is the same
/// ground without either (the model's fill under the product).
fn scene() -> (RgbaImage, RgbaImage, RgbaImage) {
    let source = build(W, H, |x, y| {
        if in_box(x, y) {
            let [r, g, b] = PRODUCT_RGB;
            [r, g, b, 255]
        } else {
            let k = 1.0 - 0.5 * shadow_strength(x, y);
            to_px(ground(x, y).map(|c| c * k))
        }
    });
    let fg = build(W, H, |x, y| {
        if in_box(x, y) {
            let [r, g, b] = PRODUCT_RGB;
            [r, g, b, 255]
        } else {
            [0, 0, 0, 0]
        }
    });
    let plate = build(W, H, |x, y| to_px(ground(x, y)));
    (source, fg, plate)
}

fn box_mask() -> GrayImage {
    GrayImage {
        width: W,
        height: H,
        pixels: (0..H)
            .flat_map(|y| (0..W).map(move |x| if in_box(x, y) { 255 } else { 0 }))
            .collect(),
        icc_profile: None,
    }
}

/// Every opaque product pixel of `fg` is bit-identical in `out`.
fn assert_product_untouched(fg: &RgbaImage, out: &RgbaImage) -> u64 {
    let mut n = 0u64;
    for i in 0..(fg.width * fg.height) as usize {
        if fg.pixels[i * 4 + 3] == 255 {
            assert_eq!(fg.pixels[i * 4..i * 4 + 4], out.pixels[i * 4..i * 4 + 4]);
            n += 1;
        }
    }
    n
}

fn chebyshev_to(x: u32, y: u32, (x0, y0, x1, y1): (u32, u32, u32, u32)) -> u32 {
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

// ---------------------------------------------------------------------------
// (a) A known soft cast shadow is carried.
// ---------------------------------------------------------------------------

#[test]
fn known_soft_cast_shadow_is_carried_and_the_product_is_untouched() {
    let (source, fg, plate) = scene();
    let carried = carry_source_shadow(&source, &fg, &plate);
    let r = &carried.report;
    eprintln!("synthetic cast shadow: {}", r.to_json());
    assert!(r.carried, "{:?}", r);
    assert_eq!(r.reason, None);
    let mean = r.mean_ratio.unwrap();
    assert!((0.45..=0.62).contains(&mean), "mean ratio {mean}");
    assert!(r.coherence.unwrap() >= SHADOW_COHERENCE_MIN, "{:?}", r);
    // The source ramp is 4 px: the feather follows it, never wider.
    assert!((2..=5).contains(&r.feather_px), "feather {}", r.feather_px);
    assert!(r.shadow_pixels > 3000, "{}", r.shadow_pixels);

    // The record's shape.
    let record = r.to_json();
    let mut keys: Vec<&str> = record
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "coherence",
            "feather_px",
            "mean_ratio",
            "mode",
            "reason",
            "regions",
            "rejected",
            "shadow_pixels"
        ]
    );
    assert_eq!(record["mode"], json!("carried"));
    // One surface, attached on the product's right, at the attached floor.
    assert_eq!(r.regions.len(), 1, "{record}");
    assert_eq!(r.regions[0].side, "right");
    assert_eq!(r.regions[0].floor, SHADOW_RATIO_MIN_ATTACHED);
    assert!(r.rejected.is_empty());

    // The matte: the carried plate matches the photo's shadow.
    let (mut err_sum, mut err_n, mut umbra_max) = (0f64, 0u64, 0f64);
    for y in 0..H {
        for x in 0..W {
            let s = shadow_strength(x, y);
            if s == 0.0 || in_box(x, y) {
                continue;
            }
            let i = ((y * W + x) * 4) as usize;
            let e = (luma(&carried.plate.pixels[i..]) - luma(&source.pixels[i..])).abs();
            err_sum += e;
            err_n += 1;
            if s == 1.0 && x >= SHADOW.0 + 2 {
                umbra_max = umbra_max.max(e);
            }
        }
    }
    let err_mean = err_sum / err_n as f64;
    assert!(
        err_mean <= 4.0,
        "mean |luma error| over the shadow {err_mean:.2}"
    );
    assert!(umbra_max <= 10.0, "max umbra |luma error| {umbra_max:.2}");

    // Far pixels: the plate exactly as it was.
    for y in 0..H {
        for x in 0..W {
            if in_box(x, y) || chebyshev_to(x, y, SHADOW) <= 8 {
                continue;
            }
            let i = ((y * W + x) * 4) as usize;
            assert_eq!(
                carried.plate.pixels[i..i + 4],
                plate.pixels[i..i + 4],
                "far pixel ({x}, {y}) changed"
            );
        }
    }
    // Never written under the product; product bit-identical after the
    // composite (and through the edge fix as well).
    for y in BOX.1..BOX.3 {
        for x in BOX.0..BOX.2 {
            let i = ((y * W + x) * 4) as usize;
            assert_eq!(carried.plate.pixels[i..i + 4], plate.pixels[i..i + 4]);
        }
    }
    let out = alpha_composite(&carried.plate, &fg);
    assert_eq!(assert_product_untouched(&fg, &out), 60 * 70);
    let (fixed, counts, _, _) = composite_images_with_edge_fix(&fg, &carried.plate, &source, false);
    assert_eq!(counts.changed_opaque_pixels, 0);
    assert_product_untouched(&fg, &fixed);

    // Deterministic.
    let again = carry_source_shadow(&source, &fg, &plate);
    assert_eq!(again.plate.pixels, carried.plate.pixels);
    assert_eq!(again.report, carried.report);
}

#[test]
fn the_rim_of_a_deeper_shadow_is_not_carried() {
    // The same cast shadow, deeper than the attached floor (ratio 0.15)
    // at its far end. The in-band part next to the product is carried only when the
    // deep core is a small part of the shadow, and it stops short of the
    // core instead of leaving a half-dark rim beside it.
    let (_, fg, plate) = scene();
    let source_with_core = |deep_from: u32| {
        build(W, H, |x, y| {
            if in_box(x, y) {
                let [r, g, b] = PRODUCT_RGB;
                return [r, g, b, 255];
            }
            let depth = if x >= deep_from { 0.85 } else { 0.5 };
            to_px(ground(x, y).map(|c| c * (1.0 - depth * shadow_strength(x, y))))
        })
    };

    // Small core: carried, the core untouched.
    let deep_from = 196;
    let source = source_with_core(deep_from);
    let carried = carry_source_shadow(&source, &fg, &plate);
    eprintln!("small deep core: {}", carried.report.to_json());
    assert!(carried.report.carried, "{:?}", carried.report);
    let (mut err_sum, mut err_n) = (0f64, 0u64);
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            if x >= deep_from {
                assert_eq!(
                    carried.plate.pixels[i..i + 4],
                    plate.pixels[i..i + 4],
                    "deep core ({x}, {y}) changed"
                );
            } else if (SHADOW.0 + 2..deep_from - 30).contains(&x)
                && (SHADOW.1 + 6..SHADOW.3 - 6).contains(&y)
            {
                err_sum += (luma(&carried.plate.pixels[i..]) - luma(&source.pixels[i..])).abs();
                err_n += 1;
            }
        }
    }
    let err_mean = err_sum / err_n as f64;
    assert!(
        err_mean <= 4.0,
        "mean |luma error| on the carried part {err_mean:.2}"
    );

    // Large core (a third of the shadow): nothing carried.
    let source = source_with_core(170);
    let out = carry_source_shadow(&source, &fg, &plate);
    eprintln!("large deep core: {}", out.report.to_json());
    assert_eq!(out.report.mode(), "contact_only");
    assert!(
        out.report
            .reason
            .as_deref()
            .unwrap()
            .starts_with("the region is the rim of a shadow deeper"),
        "{:?}",
        out.report.reason
    );
    assert_eq!(out.plate.pixels, plate.pixels);
}

/// (source, layer, plate) for a `w` x `h` frame with a product box and
/// a photo darkened by `shade(x, y)` (0 = lit, 0.7 = ratio 0.3).
fn scene_with(
    w: u32,
    h: u32,
    product: (u32, u32, u32, u32),
    shade: impl Fn(u32, u32) -> f64,
) -> (RgbaImage, RgbaImage, RgbaImage) {
    let inside =
        |x: u32, y: u32| x >= product.0 && x < product.2 && y >= product.1 && y < product.3;
    let [pr, pg, pb] = PRODUCT_RGB;
    let source = build(w, h, |x, y| {
        if inside(x, y) {
            [pr, pg, pb, 255]
        } else {
            to_px(ground(x, y).map(|c| c * (1.0 - shade(x, y))))
        }
    });
    let fg = build(w, h, |x, y| {
        if inside(x, y) {
            [pr, pg, pb, 255]
        } else {
            [0, 0, 0, 0]
        }
    });
    let plate = build(w, h, |x, y| to_px(ground(x, y)));
    (source, fg, plate)
}

/// A soft rectangle: 1 inside, 4 px linear ramps on every edge.
fn soft_rect(x: u32, y: u32, (x0, y0, x1, y1): (u32, u32, u32, u32)) -> f64 {
    if x < x0 || x >= x1 || y < y0 || y >= y1 {
        return 0.0;
    }
    let ramp = |d: u32| ((d as f64 + 0.5) / RAMP).clamp(0.0, 1.0);
    ramp(x - x0).min(ramp(x1 - 1 - x)) * ramp(y - y0).min(ramp(y1 - 1 - y))
}

#[test]
fn wall_shadow_behind_the_product_is_carried_on_any_side() {
    // A sunlit wall behind the product: its cast shadow (ratio 0.30,
    // below the ground floor) attaches to the box's upper LEFT side, in
    // its upper half. A detached darker patch far from the box (another
    // object's shadow the plate removed) must stay as the plate has it.
    const WALL: (u32, u32, u32, u32) = (2, 4, 64, 72);
    const DETACHED: (u32, u32, u32, u32) = (170, 16, 214, 52);
    let (source, fg, plate) = scene_with(W, H, BOX, |x, y| {
        let wall = if x >= BOX.0 {
            0.0
        } else {
            soft_rect(x, y, WALL)
        };
        (0.7 * wall).max(0.6 * soft_rect(x, y, DETACHED))
    });
    let carried = carry_source_shadow(&source, &fg, &plate);
    let r = &carried.report;
    eprintln!("wall shadow behind: {}", r.to_json());
    assert!(r.carried, "{:?}", r);
    assert_eq!(r.regions.len(), 1, "{}", r.to_json());
    assert_eq!(r.regions[0].side, "left");
    assert_eq!(r.regions[0].floor, SHADOW_RATIO_MIN_ATTACHED);
    // Umbra 0.30 (below the 0.35 ground floor) plus its 4 px rims.
    assert!(r.regions[0].mean_ratio < 0.45, "{:?}", r.regions[0]);
    // The wall shadow is back.
    let (mut err_sum, mut err_n) = (0f64, 0u64);
    for y in WALL.1 + 4..WALL.3 - 4 {
        for x in WALL.0 + 4..BOX.0 {
            let i = ((y * W + x) * 4) as usize;
            err_sum += (luma(&carried.plate.pixels[i..]) - luma(&source.pixels[i..])).abs();
            err_n += 1;
        }
    }
    let err_mean = err_sum / err_n as f64;
    assert!(
        err_mean <= 4.0,
        "mean |luma error| on the wall shadow {err_mean:.2}"
    );
    // The detached patch is untouched, and so is the product.
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            if chebyshev_to(x, y, DETACHED) <= 8 || in_box(x, y) {
                assert_eq!(
                    carried.plate.pixels[i..i + 4],
                    plate.pixels[i..i + 4],
                    "({x}, {y})"
                );
            }
        }
    }
    assert_product_untouched(&fg, &alpha_composite(&carried.plate, &fg));
}

#[test]
fn ground_band_only_regions_keep_the_035_floor() {
    // A tall product and a ground shadow under it that does not touch the
    // dilated mask (a lit gap of 8 px) but lies in the band under its
    // bottom edge: it is held to the 0.35 floor.
    const TALL: (u32, u32, u32, u32) = (60, 20, 120, 140);
    const STRIP: (u32, u32, u32, u32) = (30, 148, 150, 200);
    let (wt, ht) = (240, 200);
    for (depth, carried) in [(0.70, false), (0.55, true)] {
        let (source, fg, plate) = scene_with(wt, ht, TALL, |x, y| depth * soft_rect(x, y, STRIP));
        let out = carry_source_shadow(&source, &fg, &plate);
        let r = &out.report;
        eprintln!("ground band, ratio {:.2}: {}", 1.0 - depth, r.to_json());
        assert_eq!(r.carried, carried, "{}", r.to_json());
        if carried {
            assert_eq!(r.regions.len(), 1);
            assert_eq!(r.regions[0].side, "below");
            assert_eq!(r.regions[0].floor, SHADOW_RATIO_MIN);
        } else {
            assert_eq!(out.plate.pixels, plate.pixels);
        }
    }
}

// ---------------------------------------------------------------------------
// (b) Incoherent pairs: contact_only, plate unchanged.
// ---------------------------------------------------------------------------

#[test]
fn incoherent_pairs_stay_contact_only() {
    let (source, fg, _) = scene();
    // A random plate: random colour per 6 px tile, unrelated to the
    // photo. (Per-pixel noise would average to flat grey in the 4 px
    // block means, i.e. a plain plate, against which the photo's real
    // shadow IS coherent.)
    let random = build(W, H, |x, y| {
        let v = hash(x / 6, y / 6, 3);
        [v as u8, (v >> 8) as u8, (v >> 16) as u8, 255]
    });
    let out = carry_source_shadow(&source, &fg, &random);
    eprintln!("random plate: {}", out.report.to_json());
    assert_eq!(out.report.mode(), "contact_only", "{:?}", out.report);
    assert!(out.report.reason.is_some());
    assert_eq!(out.report.shadow_pixels, 0);
    assert_eq!(out.plate.pixels, random.pixels);

    // A connected region darker in the photo, but by scattered ratios
    // (8 px tiles drawn uniformly from 0.36..0.90): not one shadow.
    let (_, fg, plate) = scene();
    let scattered = build(W, H, |x, y| {
        if in_box(x, y) {
            let [r, g, b] = PRODUCT_RGB;
            return [r, g, b, 255];
        }
        let (sx0, sy0, sx1, sy1) = SHADOW;
        let k = if x >= sx0 && x < sx1 && y >= sy0 && y < sy1 {
            0.36 + 0.54 * (hash(x / 8, y / 8, 11) % 1000) as f64 / 999.0
        } else {
            1.0
        };
        to_px(ground(x, y).map(|c| c * k))
    });
    let out = carry_source_shadow(&scattered, &fg, &plate);
    let r = &out.report;
    eprintln!("scattered ratios: {}", r.to_json());
    assert_eq!(r.mode(), "contact_only", "{:?}", r);
    assert!(
        r.reason.as_deref().unwrap().starts_with("incoherent"),
        "{:?}",
        r.reason
    );
    assert!(r.coherence.unwrap() < SHADOW_COHERENCE_MIN);
    assert_eq!(out.plate.pixels, plate.pixels);
}

// ---------------------------------------------------------------------------
// (c) Kill switch, through the store: the previous composite exactly.
// ---------------------------------------------------------------------------

static ENV_LOCK: Mutex<()> = Mutex::new(());
static COUNTER_N: AtomicU32 = AtomicU32::new(0);

fn temp_store() -> (AssetStore, std::path::PathBuf) {
    let n = COUNTER_N.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("agenthand-shadow-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    (AssetStore::new(&dir), dir)
}

/// (store, dir, foreground handle, plate handle, source, layer, plate).
fn staged_store() -> (
    AssetStore,
    std::path::PathBuf,
    String,
    String,
    RgbaImage,
    RgbaImage,
    RgbaImage,
) {
    let (store, dir) = temp_store();
    let (source, _, plate) = scene();
    let src_handle = store.put_image(&source, KIND_SOURCE, None).unwrap();
    let src_id = AssetStore::parse_handle(&src_handle).unwrap();
    let mask_handle = store
        .put_gray_image(&box_mask(), KIND_MASK, Some(&src_id))
        .unwrap();
    let mask_id = AssetStore::parse_handle(&mask_handle).unwrap();
    let fg = extract_foreground(&source, &box_mask());
    let fg_handle = store
        .put_image(&fg, KIND_FOREGROUND, Some(&mask_id))
        .unwrap();
    let plate_handle = store.put_image(&plate, KIND_PLATE, Some(&src_id)).unwrap();
    (store, dir, fg_handle, plate_handle, source, fg, plate)
}

fn source_kind(kind: &'static str, fg: &RgbaImage) -> Option<(&'static str, serde_json::Value)> {
    let geometry = staging::analyze_subject(fg);
    Some((
        kind,
        backdrop::source_plate_receipt_value(
            kind,
            geometry.to_json(),
            json!({}),
            json!({}),
            geometry.shadow,
            geometry.matte_px,
        ),
    ))
}

#[test]
fn kill_switch_restores_the_previous_composite_and_the_record_says_so() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var(SHADOW_ENV).ok();
    let (store, dir, fg_handle, plate_handle, source, fg, plate) = staged_store();
    let thin = staging::analyze_subject(&fg).thin;
    let kind = backdrop::PLATE_KIND_SOURCE_EDIT;

    std::env::set_var(SHADOW_ENV, "off");
    let gated = carry_shadow_for_plate_kind(Some(kind), &source, &fg, &plate);
    assert_eq!(
        gated.report,
        ShadowReport::contact_only(&format!("disabled by {SHADOW_ENV}"))
    );
    assert_eq!(gated.plate.pixels, plate.pixels);
    let (off_handle, off_receipt) = composite::draw_product_with_plate_kind(
        &store,
        &fg_handle,
        "desk",
        Some(&plate_handle),
        source_kind(kind, &fg),
    )
    .unwrap();
    let record = &off_receipt["shadow"];
    assert_eq!(record["mode"], json!("contact_only"), "{record}");
    assert_eq!(record["reason"], json!(format!("disabled by {SHADOW_ENV}")));
    assert_eq!(shadow_record_of(&off_receipt), *record);
    // Exactly the composite the draw made before the carry-over existed.
    let (current, _, _, _) = composite_images_with_edge_fix(&fg, &plate, &source, thin);
    let off_pixels = store.read_image(&off_handle).unwrap().pixels;
    assert_eq!(off_pixels, current.pixels);

    // Switched back on: carried, product untouched, receipt verifies.
    for on in ["on", ""] {
        if on.is_empty() {
            std::env::remove_var(SHADOW_ENV);
        } else {
            std::env::set_var(SHADOW_ENV, on);
        }
        let (handle, receipt) = composite::draw_product_with_plate_kind(
            &store,
            &fg_handle,
            "desk",
            Some(&plate_handle),
            source_kind(kind, &fg),
        )
        .unwrap();
        assert_eq!(
            receipt["shadow"]["mode"],
            json!("carried"),
            "{}",
            receipt["shadow"]
        );
        assert_eq!(receipt["changed_opaque_pixels"], json!(0));
        assert_eq!(receipt["edge_fix"]["applied"], json!(true));
        let delivered = store.read_image(&handle).unwrap();
        assert_ne!(delivered.pixels, off_pixels);
        assert_product_untouched(&fg, &delivered);
        let report = composite::verify_receipt_from_disk(&store, &handle, Some(&receipt)).unwrap();
        assert_eq!(report["ok"], json!(true), "{}", report["problems"]);
    }

    match previous {
        Some(v) => std::env::set_var(SHADOW_ENV, v),
        None => std::env::remove_var(SHADOW_ENV),
    }
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// (d) Generated / code-rendered / local plates: contact_only.
// ---------------------------------------------------------------------------

#[test]
fn generated_plate_kinds_are_contact_only() {
    let (source, fg, plate) = scene();
    for kind in [
        None,
        Some("model_generated"),
        Some("local_placeholder"),
        Some("code_rendered"),
    ] {
        let out = carry_shadow_for_plate_kind(kind, &source, &fg, &plate);
        assert_eq!(
            out.report,
            ShadowReport::contact_only(SHADOW_GENERATED_REASON),
            "{kind:?}"
        );
        assert_eq!(out.report.to_json()["reason"], json!("generated plate"));
        assert_eq!(out.plate.pixels, plate.pixels);
    }
    // Through the store: the receipt is unchanged (no record), the draw
    // result's record is contact_only "generated plate", and the
    // composite is the plain one.
    let (store, dir, fg_handle, plate_handle, _, fg, plate) = staged_store();
    let (plain, _, _) = composite_images(&fg, &plate);
    for kind in [
        None,
        Some(("code_rendered", json!({"mode": "studio_gray"}))),
    ] {
        let (handle, receipt) = composite::draw_product_with_plate_kind(
            &store,
            &fg_handle,
            "desk",
            Some(&plate_handle),
            kind,
        )
        .unwrap();
        assert!(receipt.get("shadow").is_none());
        let record = shadow_record_of(&receipt);
        assert_eq!(record["mode"], json!("contact_only"));
        assert_eq!(record["reason"], json!(SHADOW_GENERATED_REASON));
        assert_eq!(store.read_image(&handle).unwrap().pixels, plain.pixels);
    }
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// Real fixture (opt-in): original | shipped staged | carried shadow.
// ---------------------------------------------------------------------------

fn load(path: &std::path::Path) -> RgbaImage {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    agenthand::image::decode_image(&bytes).unwrap().into_rgba()
}

fn third(img: &RgbaImage) -> image::RgbImage {
    let buf = image::RgbaImage::from_raw(img.width, img.height, img.pixels.clone()).unwrap();
    let small = image::imageops::resize(
        &buf,
        img.width / 3,
        img.height / 3,
        image::imageops::FilterType::Triangle,
    );
    image::DynamicImage::ImageRgba8(small).to_rgb8()
}

#[test]
#[ignore]
fn real_fixture_bike_shadow() {
    let Ok(dir) = std::env::var("AGENTHAND_SHADOW_ARTIFACTS") else {
        eprintln!("AGENTHAND_SHADOW_ARTIFACTS not set; skipping the real-fixture run");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let out_dir = std::path::PathBuf::from(
        std::env::var("AGENTHAND_SHADOW_OUT").unwrap_or_else(|_| "shadow-out".to_string()),
    );
    std::fs::create_dir_all(&out_dir).unwrap();
    let t = std::time::Instant::now();
    let source = load(&dir.join("source.png"));
    let fg = load(&dir.join("foreground.png"));
    let plate = load(&dir.join("plate.png"));
    let staged = load(&dir.join("staged.png"));
    eprintln!("decoded in {:.1}s", t.elapsed().as_secs_f64());

    let (plain, _, _) = composite_images(&fg, &plate);
    assert_eq!(
        plain.pixels, staged.pixels,
        "plain recomposite reproduces staged.png"
    );

    let t = std::time::Instant::now();
    let carried = carry_source_shadow(&source, &fg, &plate);
    let carry_s = t.elapsed().as_secs_f64();
    let rerun = carry_source_shadow(&source, &fg, &plate);
    assert_eq!(rerun.plate.pixels, carried.plate.pixels, "deterministic");

    // plate -> carried shadow -> product (no edge fix, so the only
    // difference from the shipped staged.png is the shadow).
    let mut after = alpha_composite(&carried.plate, &fg);
    after.icc_profile = fg.icc_profile.clone();
    let mut product = 0u64;
    let mut changed_outside = 0u64;
    for i in 0..(fg.width * fg.height) as usize {
        let j = i * 4;
        if fg.pixels[j + 3] == 255 {
            assert_eq!(after.pixels[j..j + 4], staged.pixels[j..j + 4]);
            product += 1;
        } else if after.pixels[j..j + 4] != staged.pixels[j..j + 4] {
            changed_outside += 1;
        }
    }
    // The production order (carried shadow -> edge fix -> product) keeps
    // the product too.
    let thin = staging::analyze_subject(&fg).thin;
    let (_, counts, _, edge) = composite_images_with_edge_fix(&fg, &carried.plate, &source, thin);
    assert_eq!(counts.changed_opaque_pixels, 0);

    let summary = json!({
        "shadow": carried.report.to_json(),
        "carry_seconds": (carry_s * 10.0).round() / 10.0,
        "product_pixels": product,
        "product_pixels_bit_identical_to_shipped_staged": true,
        "non_product_pixels_changed": changed_outside,
        "with_edge_fix": {"changed_opaque_pixels": counts.changed_opaque_pixels, "edge_fix_applied": edge.applied},
    });
    eprintln!("{}", serde_json::to_string_pretty(&summary).unwrap());
    let tag = std::env::var("AGENTHAND_SHADOW_TAG").unwrap_or_default();
    std::fs::write(
        out_dir.join(format!("bike_shadow_record{tag}.json")),
        serde_json::to_string_pretty(&summary).unwrap(),
    )
    .unwrap();

    // Optional full-resolution crop: AGENTHAND_SHADOW_CROP="x,y,w,h".
    if let Some((cx, cy, cw, ch)) = std::env::var("AGENTHAND_SHADOW_CROP").ok().and_then(|v| {
        let n: Vec<u32> = v.split(',').filter_map(|t| t.trim().parse().ok()).collect();
        (n.len() == 4).then(|| (n[0], n[1], n[2], n[3]))
    }) {
        let mut canvas =
            image::RgbImage::from_pixel(cw * 3 + 2 * 12, ch, image::Rgb([255, 255, 255]));
        for (k, img) in [&source, &staged, &after].iter().enumerate() {
            for y in 0..ch.min(img.height - cy) {
                for x in 0..cw.min(img.width - cx) {
                    let j = (((cy + y) * img.width + cx + x) * 4) as usize;
                    let p = image::Rgb([img.pixels[j], img.pixels[j + 1], img.pixels[j + 2]]);
                    canvas.put_pixel(k as u32 * (cw + 12) + x, y, p);
                }
            }
        }
        let mut file = std::io::BufWriter::new(
            std::fs::File::create(out_dir.join(format!("bike_shadow_crop{tag}.jpg"))).unwrap(),
        );
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut file, 90)
            .encode_image(&canvas)
            .unwrap();
    }

    let panels = [third(&source), third(&staged), third(&after)];
    let (pw, ph) = (panels[0].width(), panels[0].height());
    // Optional zoom row at 2x the overview (2/3 of full resolution):
    // AGENTHAND_SHADOW_ZOOM="x,y,w,h" in full-resolution pixels.
    let zoom: Option<Vec<image::RgbImage>> = std::env::var("AGENTHAND_SHADOW_ZOOM")
        .ok()
        .and_then(|v| {
            let n: Vec<u32> = v.split(',').filter_map(|t| t.trim().parse().ok()).collect();
            (n.len() == 4).then(|| (n[0], n[1], n[2], n[3]))
        })
        .map(|(zx, zy, zw, zh)| {
            [&source, &staged, &after]
                .iter()
                .map(|img| {
                    let buf = image::RgbaImage::from_raw(img.width, img.height, img.pixels.clone())
                        .unwrap();
                    let crop = image::imageops::crop_imm(&buf, zx, zy, zw, zh).to_image();
                    let big = image::imageops::resize(
                        &crop,
                        zw * 2 / 3,
                        zh * 2 / 3,
                        image::imageops::FilterType::Triangle,
                    );
                    image::DynamicImage::ImageRgba8(big).to_rgb8()
                })
                .collect()
        });
    let zoom_h = zoom.as_ref().map_or(0, |z| z[0].height() + 12);
    let zoom_w = zoom.as_ref().map_or(0, |z| z[0].width() * 3 + 2 * 12);
    let mut canvas = image::RgbImage::from_pixel(
        (pw * 3 + 2 * 12).max(zoom_w),
        ph + zoom_h,
        image::Rgb([255, 255, 255]),
    );
    for (k, panel) in panels.iter().enumerate() {
        image::imageops::replace(&mut canvas, panel, (k as u32 * (pw + 12)) as i64, 0);
    }
    if let Some(z) = &zoom {
        let zw = z[0].width();
        for (k, panel) in z.iter().enumerate() {
            image::imageops::replace(
                &mut canvas,
                panel,
                (k as u32 * (zw + 12)) as i64,
                (ph + 12) as i64,
            );
        }
    }
    let mut file = std::io::BufWriter::new(
        std::fs::File::create(out_dir.join(format!("bike_shadow_before_after{tag}.jpg"))).unwrap(),
    );
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut file, 88)
        .encode_image(&canvas)
        .unwrap();
}
