//! Light direction and the thin-structure guard (docs/LIGHT-AND-THIN.md),
//! plus the generated-plate scene exclusions and the fitted-plate hash.
//! Synthetic cases: (a) a ring-with-spokes product over a plate that is
//! busy only behind the wheel: the guard fires, the hole texture drops,
//! every product-covered plate pixel and every product pixel of the
//! composite stays bit-identical, and the receipt keeps its shape;
//! (b) a solid product never triggers; (c) a clear ground shadow to the
//! right states light from camera-left, an even ground states nothing;
//! (d) AGENTHAND_THIN=off disables the guard. Plus an opt-in run on a
//! real staged photo:
//!
//! ```text
//! AGENTHAND_THIN_ARTIFACTS=<dir with source.png foreground.png plate.png> \
//! AGENTHAND_THIN_OUT=<out dir> \
//! cargo test --release --test thin_light -- --ignored --nocapture
//! ```

use agenthand::assets::{AssetStore, KIND_FOREGROUND, KIND_MASK, KIND_PLATE, KIND_SOURCE};
use agenthand::composite::{
    self, apply_contact_shadow, apply_contact_shadow_offset, composite_images,
    contact_shadow_offset_px, soften_thin_holes, thin_trigger_for, THIN_ENV,
};
use agenthand::image::{GrayImage, RgbaImage};
use agenthand::meta::{
    self, generated_plate_scene, generated_plate_scene_dressed, light_clause, scene_exclusions,
    LIGHT_PHRASE_FROM_LEFT, LIGHT_PHRASE_FROM_RIGHT, THIN_PLAIN_CLAUSE,
};
use agenthand::segmentation::extract_foreground;
use agenthand::staging::{self, estimate_light_direction, LightSide};
use agenthand::{backdrop, resample};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

/// Every test that reads AGENTHAND_THIN holds this.
static ENV_LOCK: Mutex<()> = Mutex::new(());
static COUNTER: AtomicU32 = AtomicU32::new(0);

fn lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn build(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> RgbaImage {
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            px.extend_from_slice(&f(x, y));
        }
    }
    RgbaImage::new(w, h, px)
}

fn noise(x: u32, y: u32, seed: u32) -> i32 {
    let mut v = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77).rotate_left(13) ^ seed;
    v ^= v >> 16;
    v = v.wrapping_mul(0x85EB_CA6B);
    v ^= v >> 13;
    (v % 5) as i32 - 2
}

fn px(rgb: [i32; 3]) -> [u8; 4] {
    [
        rgb[0].clamp(0, 255) as u8,
        rgb[1].clamp(0, 255) as u8,
        rgb[2].clamp(0, 255) as u8,
        255,
    ]
}

// ---------------------------------------------------------------------------
// The wheel: a rim, eight spokes and a hub, centred in a 320 x 240 frame.
// ---------------------------------------------------------------------------

const W: u32 = 320;
const H: u32 = 240;
const C: (f64, f64) = (160.0, 120.0);
const RIM_OUT: f64 = 90.0;
const RIM_IN: f64 = 80.0;

fn on_wheel(x: u32, y: u32) -> bool {
    let (dx, dy) = (x as f64 + 0.5 - C.0, y as f64 + 0.5 - C.1);
    let r = (dx * dx + dy * dy).sqrt();
    if r > RIM_OUT {
        return false;
    }
    if r >= RIM_IN || r <= 8.0 {
        return true;
    }
    // Eight spokes, 3 px wide.
    (0..8).any(|k| {
        let a = k as f64 * std::f64::consts::PI / 8.0;
        (dx * a.sin() - dy * a.cos()).abs() <= 1.5
    })
}

fn wheel_mask() -> GrayImage {
    let mut pixels = Vec::with_capacity((W * H) as usize);
    for y in 0..H {
        for x in 0..W {
            pixels.push(if on_wheel(x, y) { 255 } else { 0 });
        }
    }
    GrayImage {
        width: W,
        height: H,
        pixels,
        icc_profile: None,
    }
}

/// The photo the wheel was cut from: dark tyre/spokes on a plain wall.
fn wheel_source() -> RgbaImage {
    build(W, H, |x, y| {
        if on_wheel(x, y) {
            px([28, 30, 34])
        } else {
            px([
                200 + noise(x, y, 1),
                196 + noise(x, y, 1),
                188 + noise(x, y, 1),
            ])
        }
    })
}

/// A plate that is plain around the product but shows dark blocks and
/// hard edges behind the wheel (a doorway, a parked car).
fn busy_plate() -> RgbaImage {
    build(W, H, |x, y| {
        let (dx, dy) = (x as f64 + 0.5 - C.0, y as f64 + 0.5 - C.1);
        let n = noise(x, y, 3);
        if (dx * dx + dy * dy).sqrt() < RIM_OUT + 2.0 && ((x / 14) + (y / 14)) % 2 == 0 {
            px([40 + n, 38 + n, 36 + n])
        } else {
            px([205 + n, 200 + n, 192 + n])
        }
    })
}

fn temp_store() -> (AssetStore, std::path::PathBuf) {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("agenthand-thin-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    (AssetStore::new(&dir), dir)
}

/// (store, dir, foreground handle, foreground) with source -> mask -> fg.
fn staged(
    source: &RgbaImage,
    mask: &GrayImage,
) -> (AssetStore, std::path::PathBuf, String, RgbaImage) {
    let (store, dir) = temp_store();
    let src = store.put_image(source, KIND_SOURCE, None).unwrap();
    let src_id = AssetStore::parse_handle(&src).unwrap();
    let mask_handle = store
        .put_gray_image(mask, KIND_MASK, Some(&src_id))
        .unwrap();
    let mask_id = AssetStore::parse_handle(&mask_handle).unwrap();
    let fg = extract_foreground(source, mask);
    let fg_handle = store
        .put_image(&fg, KIND_FOREGROUND, Some(&mask_id))
        .unwrap();
    (store, dir, fg_handle, fg)
}

fn sorted_keys(v: &Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    k.sort();
    k
}

#[test]
fn ring_with_spokes_over_a_busy_plate_is_softened_and_the_product_is_untouched() {
    let _guard = lock();
    std::env::remove_var(THIN_ENV);
    let source = wheel_source();
    let mask = wheel_mask();
    let (store, dir, fg_handle, fg) = staged(&source, &mask);
    let plate = busy_plate();

    // Triggered by the mask's enclosed holes alone (no concept known) ...
    let trigger = thin_trigger_for(&fg, None);
    assert!(trigger.triggered);
    assert_eq!(trigger.trigger, "holes");
    assert!(trigger.holes >= 8, "{trigger:?}");
    assert!(trigger.largest_hole_fraction >= staging::THIN_HOLE_TRIGGER_FRACTION);
    // ... and by the concept word, which wins the label.
    let by_concept = thin_trigger_for(&fg, Some("blue kids BMX bike"));
    assert_eq!(by_concept.trigger, "concept");
    assert_eq!(by_concept.concept_word, Some("bmx"));

    let (out, report) = soften_thin_holes(&plate, &fg, &trigger);
    let record = report.to_json();
    assert_eq!(record["triggered"], json!(true));
    assert_eq!(record["trigger"], json!("holes"));
    assert_eq!(record["holes_softened"], json!(true), "{record}");
    let before = report.texture_before.unwrap();
    let after = report.texture_after.unwrap();
    let ring = report.texture_ring.unwrap();
    assert!(before > 3.0 * ring.max(1.0), "{record}");
    assert!(after < 0.6 * before, "hole texture did not drop: {record}");
    assert!((1..=composite::THIN_MAX_STEPS).contains(&report.steps));
    assert!(report.pixels_written > 0);

    // Writes only outside the product mask: every plate pixel under the
    // product (alpha > 0) is unchanged, and so is everything outside the
    // wheel's disc.
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            let (dx, dy) = (x as f64 + 0.5 - C.0, y as f64 + 0.5 - C.1);
            if fg.pixels[i + 3] > 0 || (dx * dx + dy * dy).sqrt() > RIM_IN {
                assert_eq!(out.pixels[i..i + 4], plate.pixels[i..i + 4], "({x},{y})");
            }
        }
    }
    // Every foreground pixel reaches the composite bit-identical.
    let (shipped, counts, _) = composite_images(&fg, &out);
    assert_eq!(counts.changed_opaque_pixels, 0);
    for (i, a) in fg.alpha_plane().iter().enumerate() {
        if *a == 255 {
            assert_eq!(
                shipped.pixels[i * 4..i * 4 + 4],
                fg.pixels[i * 4..i * 4 + 4]
            );
        }
    }

    // Receipt shape unchanged: the same draw over the original and the
    // softened plate carries the same keys and verifies.
    let mut keys = Vec::new();
    for p in [&plate, &out] {
        let handle = store.put_image(p, KIND_PLATE, None).unwrap();
        let (composite_handle, receipt) = composite::draw_product_with_plate_kind(
            &store,
            &fg_handle,
            "porch",
            Some(&handle),
            None,
        )
        .unwrap();
        assert_eq!(receipt["changed_opaque_pixels"], json!(0));
        let verified =
            composite::verify_receipt_from_disk(&store, &composite_handle, Some(&receipt)).unwrap();
        assert_eq!(verified["ok"], json!(true), "{}", verified["problems"]);
        keys.push(sorted_keys(&receipt));
    }
    assert_eq!(keys[0], keys[1]);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_guard_works_through_the_cover_fit_of_a_generated_plate() {
    let _guard = lock();
    std::env::remove_var(THIN_ENV);
    let fg = extract_foreground(&wheel_source(), &wheel_mask());
    // A 3:2 plate at 1.5x the product frame's height: cropped at the sides.
    let big = build(540, 360, |x, y| {
        let (dx, dy) = (
            x as f64 / 1.5 - 20.0 + 0.5 - C.0,
            y as f64 / 1.5 + 0.5 - C.1,
        );
        let n = noise(x, y, 5);
        if (dx * dx + dy * dy).sqrt() < RIM_OUT && ((x / 21) + (y / 21)) % 2 == 0 {
            px([40 + n, 38 + n, 36 + n])
        } else {
            px([205 + n, 200 + n, 192 + n])
        }
    });
    let trigger = thin_trigger_for(&fg, None);
    let (out, report) = soften_thin_holes(&big, &fg, &trigger);
    assert!(report.holes_softened, "{}", report.to_json());
    assert!(report.texture_after.unwrap() < report.texture_before.unwrap());
    // Composited through the fit, the product is untouched.
    let (shipped, counts, transforms) = composite_images(&fg, &out);
    assert_eq!(counts.changed_opaque_pixels, 0);
    assert_eq!(transforms.len(), 1);
    // Plate pixels whose fitted footprint touches the product are unchanged.
    let fitted_before = resample::fit(&big, W, H);
    let fitted_after = resample::fit(&out, W, H);
    let mut differing_under_core = 0;
    for (i, a) in fg.alpha_plane().iter().enumerate() {
        if *a == 255 {
            assert_eq!(
                shipped.pixels[i * 4..i * 4 + 4],
                fg.pixels[i * 4..i * 4 + 4]
            );
        }
        let (x, y) = ((i as u32 % W) as f64, (i as u32 / W) as f64);
        let r = ((x + 0.5 - C.0).powi(2) + (y + 0.5 - C.1).powi(2)).sqrt();
        if r > RIM_OUT + 6.0
            && fitted_before.pixels[i * 4..i * 4 + 3] != fitted_after.pixels[i * 4..i * 4 + 3]
        {
            differing_under_core += 1;
        }
    }
    assert_eq!(differing_under_core, 0, "the guard wrote outside the wheel");
}

#[test]
fn a_solid_product_does_not_trigger() {
    let _guard = lock();
    std::env::remove_var(THIN_ENV);
    let disc = build(W, H, |x, y| {
        let r = ((x as f64 + 0.5 - C.0).powi(2) + (y as f64 + 0.5 - C.1).powi(2)).sqrt();
        if r <= RIM_OUT {
            [30, 60, 160, 255]
        } else {
            [0, 0, 0, 0]
        }
    });
    let trigger = thin_trigger_for(&disc, Some("ceramic vase"));
    assert!(!trigger.triggered);
    assert_eq!(trigger.trigger, "none");
    assert_eq!(trigger.holes, 0);
    let plate = busy_plate();
    let (out, report) = soften_thin_holes(&plate, &disc, &trigger);
    assert_eq!(out.pixels, plate.pixels);
    let record = report.to_json();
    assert_eq!(record["triggered"], json!(false));
    assert_eq!(record["holes_softened"], json!(false));
    assert_eq!(record["texture_before"], Value::Null);
}

#[test]
fn the_kill_switch_disables_the_guard() {
    let _guard = lock();
    let fg = extract_foreground(&wheel_source(), &wheel_mask());
    let plate = busy_plate();
    for off in ["off", "0", "false", "OFF"] {
        std::env::set_var(THIN_ENV, off);
        let trigger = thin_trigger_for(&fg, Some("bike"));
        assert!(!trigger.triggered);
        assert_eq!(trigger.trigger, "none");
        let (out, report) = soften_thin_holes(&plate, &fg, &trigger);
        assert_eq!(out.pixels, plate.pixels);
        let record = report.to_json();
        assert_eq!(record["triggered"], json!(false));
        assert_eq!(record["holes_softened"], json!(false));
        assert_eq!(record["note"], json!(format!("disabled by {THIN_ENV}")));
    }
    std::env::remove_var(THIN_ENV);
    assert!(thin_trigger_for(&fg, None).triggered);
}

// ---------------------------------------------------------------------------
// Light direction.
// ---------------------------------------------------------------------------

const LW: u32 = 240;
const LH: u32 = 160;
const BOX: (u32, u32, u32, u32) = (90, 40, 150, 110);

fn box_product() -> RgbaImage {
    build(LW, LH, |x, y| {
        if x >= BOX.0 && x < BOX.2 && y >= BOX.1 && y < BOX.3 {
            [40, 90, 200, 255]
        } else {
            [0, 0, 0, 0]
        }
    })
}

/// Ground photo; `shadow(x, y)` darkens a cast shadow by 0.5 (luma only).
fn light_source(shadow: impl Fn(u32, u32) -> bool) -> RgbaImage {
    build(LW, LH, |x, y| {
        if x >= BOX.0 && x < BOX.2 && y >= BOX.1 && y < BOX.3 {
            return [40, 90, 200, 255];
        }
        let n = noise(x, y, 9);
        let k = if shadow(x, y) { 0.5 } else { 1.0 };
        px([
            ((150 + n) as f64 * k) as i32,
            ((140 + n) as f64 * k) as i32,
            ((120 + n) as f64 * k) as i32,
        ])
    })
}

#[test]
fn a_ground_shadow_to_the_right_states_light_from_camera_left() {
    let fg = box_product();
    let right = light_source(|x, y| x >= BOX.2 && x < BOX.2 + 70 && y >= 82 && y < 122);
    let e = estimate_light_direction(&right, &fg);
    assert_eq!(e.method, "cast_shadow", "{e:?}");
    assert_eq!(e.side, Some(LightSide::FromLeft));
    assert_eq!(e.side.unwrap().shadow_side(), "camera-right");
    let phrase = light_clause(&e).unwrap();
    assert_eq!(phrase, LIGHT_PHRASE_FROM_LEFT);
    assert!(phrase.contains("shadows fall to camera-right"));

    // Mirrored: the shadow side is named either way.
    let left = light_source(|x, y| x < BOX.0 && x + 70 >= BOX.0 && y >= 82 && y < 122);
    let e = estimate_light_direction(&left, &fg);
    assert_eq!(e.side, Some(LightSide::FromRight), "{e:?}");
    assert!(light_clause(&e)
        .unwrap()
        .contains("shadows fall to camera-left"));
    assert_eq!(light_clause(&e).unwrap(), LIGHT_PHRASE_FROM_RIGHT);

    // The contact shadow moves away from the light, never under product.
    let plate = build(LW, LH, |_, _| [200, 200, 200, 255]);
    let centred = apply_contact_shadow(&plate, &fg);
    assert_eq!(
        apply_contact_shadow_offset(&plate, &fg, (0.0, 0.0)).pixels,
        centred.pixels
    );
    let offset = contact_shadow_offset_px(&fg, LightSide::FromLeft.shadow_step());
    assert!(offset.0 > 0.0 && offset.1 == 0.0);
    let moved = apply_contact_shadow_offset(&plate, &fg, offset);
    let centroid = |img: &RgbaImage| {
        let (mut s, mut n) = (0f64, 0f64);
        for (i, p) in img.pixels.chunks_exact(4).enumerate() {
            let dark = 200.0 - p[0] as f64;
            s += dark * (i as u32 % LW) as f64;
            n += dark;
        }
        s / n
    };
    assert!(centroid(&moved) > centroid(&centred));
    for (i, a) in fg.alpha_plane().iter().enumerate() {
        if *a == 255 {
            assert_eq!(
                moved.pixels[i * 4..i * 4 + 4],
                plate.pixels[i * 4..i * 4 + 4]
            );
        }
    }
}

#[test]
fn an_inconclusive_source_states_nothing() {
    let fg = box_product();
    let even = light_source(|_, _| false);
    let e = estimate_light_direction(&even, &fg);
    assert_eq!(e.side, None, "{e:?}");
    assert_eq!(e.method, "none");
    assert_eq!(light_clause(&e), None);
    // Scattered dark specks (no connected region at the contact) say
    // nothing either.
    let specks = light_source(|x, y| x % 23 == 0 && y % 19 == 0);
    let e = estimate_light_direction(&specks, &fg);
    assert_eq!(e.side, None, "{e:?}");
    // The generated scene gains no light phrase when nothing is stated.
    let angle = staging::estimate_shot_angle(&staging::analyze_subject(&fg));
    assert_eq!(
        generated_plate_scene_dressed("driveway", &angle, light_clause(&e), false, &[]),
        generated_plate_scene("driveway", &angle)
    );
}

// ---------------------------------------------------------------------------
// Generated-plate clauses: plainness and scene exclusions.
// ---------------------------------------------------------------------------

#[test]
fn generated_scene_carries_the_clauses_only_when_known() {
    let fg = box_product();
    let angle = staging::estimate_shot_angle(&staging::analyze_subject(&fg));
    let base = generated_plate_scene("premium driveway", &angle);

    let exclusions = scene_exclusions(Some("blue kids BMX bike"));
    assert_eq!(
        exclusions,
        vec![
            "blue kids bmx bike",
            "bikes",
            "bicycles or other vehicles",
            "people",
            "animals"
        ]
    );
    let dressed = generated_plate_scene_dressed(
        "premium driveway",
        &angle,
        Some(LIGHT_PHRASE_FROM_LEFT),
        true,
        &exclusions,
    );
    assert!(dressed.starts_with(&base));
    assert!(dressed.contains(LIGHT_PHRASE_FROM_LEFT));
    assert!(dressed.contains(THIN_PLAIN_CLAUSE));
    assert!(dressed.ends_with(
        "Empty scene, no blue kids bmx bike, no bikes, no bicycles or other vehicles, no people, no animals; the product will be placed afterwards."
    ));
    // The full prompt sent to the image model carries the exclusion.
    assert!(meta::wrap_plate_prompt(&dressed).contains("no bicycles or other vehicles"));

    // No concept: no exclusions and the prompt is unchanged.
    assert!(scene_exclusions(None).is_empty());
    assert!(scene_exclusions(Some("  ")).is_empty());
    assert_eq!(
        generated_plate_scene_dressed(
            "premium driveway",
            &angle,
            None,
            false,
            &scene_exclusions(None)
        ),
        base
    );
    // Head nouns: qualifiers cut, plurals formed, bicycles not repeated.
    assert_eq!(
        scene_exclusions(Some("bicycle"))[0],
        "bicycles or other vehicles"
    );
    assert_eq!(
        scene_exclusions(Some("office chair with wheels"))[1],
        "chairs"
    );
    assert_eq!(scene_exclusions(Some("bench"))[0], "benches");
}

// ---------------------------------------------------------------------------
// plate_fitted_pixel_sha256: generated plates only.
// ---------------------------------------------------------------------------

#[test]
fn generated_receipts_pin_the_fitted_plate_and_source_receipts_do_not() {
    let _guard = lock();
    let source = wheel_source();
    let mask = wheel_mask();
    let (store, dir, fg_handle, fg) = staged(&source, &mask);
    // Generated: a 3:2 plate the draw must cover-fit to 4:3.
    let generated = build(480, 320, |x, y| {
        px([100 + (x % 50) as i32, 120, 90 + (y % 40) as i32])
    });
    let handle = store.put_image(&generated, KIND_PLATE, None).unwrap();
    let (_, receipt) = composite::draw_product_with_plate_kind(
        &store,
        &fg_handle,
        "driveway",
        Some(&handle),
        None,
    )
    .unwrap();
    let fitted = resample::fit(&generated, W, H);
    assert_eq!(
        receipt["plate_fitted_pixel_sha256"],
        json!(fitted.pixel_sha256())
    );
    assert_ne!(
        receipt["plate_fitted_pixel_sha256"],
        receipt["plate_pixel_sha256"]
    );

    // Source edit: absent.
    let plate = busy_plate();
    let src_handle = store
        .lineage(&fg_handle)
        .unwrap()
        .last()
        .unwrap()
        .asset_id
        .clone();
    let plate_handle = store
        .put_image(&plate, KIND_PLATE, Some(&src_handle))
        .unwrap();
    let geometry = staging::analyze_subject(&fg);
    let kind = backdrop::PLATE_KIND_SOURCE_EDIT;
    let (_, receipt) = composite::draw_product_with_plate_kind(
        &store,
        &fg_handle,
        "porch",
        Some(&plate_handle),
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
        )),
    )
    .unwrap();
    assert!(receipt.get("plate_fitted_pixel_sha256").is_none());
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// Real fixture (opt-in): shipped staged | with hole softening, wheel crops.
// ---------------------------------------------------------------------------

fn load(path: &std::path::Path) -> RgbaImage {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    agenthand::image::decode_image(&bytes).unwrap().into_rgba()
}

fn to_rgb(img: &RgbaImage) -> image::RgbImage {
    let mut out = image::RgbImage::new(img.width, img.height);
    for (i, p) in img.pixels.chunks_exact(4).enumerate() {
        out.put_pixel(
            i as u32 % img.width,
            i as u32 / img.width,
            image::Rgb([p[0], p[1], p[2]]),
        );
    }
    out
}

#[test]
#[ignore]
fn real_fixture_bike_thin() {
    let _guard = lock();
    std::env::remove_var(THIN_ENV);
    let dir = std::path::PathBuf::from(
        std::env::var("AGENTHAND_THIN_ARTIFACTS").expect("AGENTHAND_THIN_ARTIFACTS"),
    );
    let out_dir =
        std::path::PathBuf::from(std::env::var("AGENTHAND_THIN_OUT").expect("AGENTHAND_THIN_OUT"));
    std::fs::create_dir_all(&out_dir).unwrap();
    let source = load(&dir.join("source.png"));
    let fg = load(&dir.join("foreground.png"));
    let plate = load(&dir.join("plate.png"));
    let concept = std::env::var("AGENTHAND_THIN_CONCEPT").ok();
    let t0 = std::time::Instant::now();
    let trigger = thin_trigger_for(&fg, concept.as_deref());
    let (softened, report) = soften_thin_holes(&plate, &fg, &trigger);
    let guard_secs = t0.elapsed().as_secs_f64();
    let light = estimate_light_direction(&source, &fg);
    // The shipped source_edit composite path, over the shipped plate and
    // over the softened plate: carried shadow -> edge fix -> product.
    let kind = Some(backdrop::PLATE_KIND_SOURCE_EDIT);
    let thin = staging::analyze_subject(&fg).thin;
    let stage = |p: &RgbaImage| {
        let carried = composite::carry_shadow_for_plate_kind(kind, &source, &fg, p);
        composite::composite_images_with_edge_fix(&fg, &carried.plate, &source, thin)
    };
    let (before, _, _, _) = stage(&plate);
    let (after, counts, _, _) = stage(&softened);
    let shipped = std::fs::read(dir.join("staged.png"))
        .ok()
        .and_then(|b| agenthand::image::decode_image(&b).ok())
        .map(|d| d.into_rgba());
    let shipped_matches = shipped.as_ref().map(|s| s.pixels == before.pixels);
    let mut identical = 0u64;
    let mut product = 0u64;
    let mut plate_under_mask_changed = 0u64;
    for (i, a) in fg.alpha_plane().iter().enumerate() {
        let j = i * 4;
        if *a == 255 {
            product += 1;
            identical += (after.pixels[j..j + 4] == fg.pixels[j..j + 4]
                && before.pixels[j..j + 4] == after.pixels[j..j + 4])
                as u64;
        }
        if *a > 0 && plate.pixels[j..j + 4] != softened.pixels[j..j + 4] {
            plate_under_mask_changed += 1;
        }
    }
    let changed_composite = before
        .pixels
        .chunks_exact(4)
        .zip(after.pixels.chunks_exact(4))
        .filter(|(a, b)| a != b)
        .count();
    let record = json!({
        "thin_structure": report.to_json(),
        "light_direction": {
            "side": light.side.map(|s| s.bucket()),
            "shadow_falls": light.side.map(|s| s.shadow_side()),
            "method": light.method,
            "phrase": light_clause(&light),
            "reason": light.reason,
            "signals": light.signals,
        },
        "product_pixels": product,
        "product_pixels_identical": identical,
        "changed_opaque_pixels": counts.changed_opaque_pixels,
        "plate_pixels_changed_under_mask": plate_under_mask_changed,
        "composite_pixels_changed": changed_composite,
        "guard_seconds": guard_secs,
        "recomposed_matches_shipped_staged_png": shipped_matches,
        "concept": concept,
    });
    println!("{}", serde_json::to_string_pretty(&record).unwrap());
    std::fs::write(
        out_dir.join("bike_thin_record.json"),
        serde_json::to_string_pretty(&record).unwrap(),
    )
    .unwrap();
    assert_eq!(identical, product);
    assert_eq!(plate_under_mask_changed, 0);

    // Panel: shipped staged | with hole softening (each at 1/4 scale),
    // then 2x crops of the rear wheel (left) and the frame triangle.
    let (w, h) = (fg.width, fg.height);
    let small = |img: &RgbaImage| {
        image::imageops::resize(
            &to_rgb(img),
            w / 4,
            h / 4,
            image::imageops::FilterType::Triangle,
        )
    };
    // Left: the shipped staged photo. Right: the same photo plus exactly
    // what the guard changed in the current composite path.
    let left = shipped.unwrap_or_else(|| before.clone());
    let mut right = left.clone();
    for (i, (p, q)) in before.pixels.iter().zip(&after.pixels).enumerate() {
        if p != q {
            right.pixels[i] = (left.pixels[i] as i32 + *q as i32 - *p as i32).clamp(0, 255) as u8;
        }
    }
    let (b, a) = (small(&left), small(&right));
    let crop_w = w / 4;
    let crop_h = h / 4;
    // Crop centres: the rear wheel (left disc of the bike), and the
    // middle of the plate region the guard changed (or the frame centre).
    let (mut cx0, mut cy0, mut cx1, mut cy1) = (u32::MAX, u32::MAX, 0, 0);
    for (i, (p, q)) in plate
        .pixels
        .chunks_exact(4)
        .zip(softened.pixels.chunks_exact(4))
        .enumerate()
    {
        if p != q {
            let (x, y) = (i as u32 % w, i as u32 / w);
            cx0 = cx0.min(x);
            cy0 = cy0.min(y);
            cx1 = cx1.max(x);
            cy1 = cy1.max(y);
        }
    }
    let changed_centre = if cx0 == u32::MAX {
        // Nothing changed: the largest enclosed hole of the mask.
        let (gw, gh) = staging::work_grid(w, h, staging::THIN_WORK_LONG_SIDE);
        let grid = staging::product_grid(&fg, gw, gh);
        let holes = staging::enclosed_holes(&grid, gw, gh, staging::THIN_HOLE_MIN_FRACTION);
        let biggest = (0..holes.areas.len())
            .max_by_key(|&k| holes.areas[k])
            .map(|k| k as u32 + 1);
        let (mut sx, mut sy, mut n) = (0u64, 0u64, 0u64);
        for (i, &l) in holes.label.iter().enumerate() {
            if Some(l) == biggest {
                sx += (i % gw) as u64;
                sy += (i / gw) as u64;
                n += 1;
            }
        }
        if n == 0 {
            (w / 2, h / 2)
        } else {
            (
                ((sx / n) as f64 * w as f64 / gw as f64) as u32,
                ((sy / n) as f64 * h as f64 / gh as f64) as u32,
            )
        }
    } else {
        ((cx0 + cx1) / 2, (cy0 + cy1) / 2)
    };
    let at = |(x, y): (u32, u32)| {
        (
            x.saturating_sub(crop_w / 4).min(w - crop_w / 2),
            y.saturating_sub(crop_h / 4).min(h - crop_h / 2),
        )
    };
    let crops: Vec<(u32, u32)> = vec![at((w * 22 / 100, h * 73 / 100)), at(changed_centre)];
    let mut panel = image::RgbImage::new(2 * (w / 4), h / 4 + crops.len() as u32 * crop_h);
    image::imageops::replace(&mut panel, &b, 0, 0);
    image::imageops::replace(&mut panel, &a, (w / 4) as i64, 0);
    for (k, &(cx, cy)) in crops.iter().enumerate() {
        for (col, img) in [&left, &right].iter().enumerate() {
            let rgb = to_rgb(img);
            let view = image::imageops::crop_imm(&rgb, cx, cy, crop_w / 2, crop_h / 2).to_image();
            let up = image::imageops::resize(
                &view,
                crop_w,
                crop_h,
                image::imageops::FilterType::Nearest,
            );
            image::imageops::replace(
                &mut panel,
                &up,
                (col as u32 * crop_w) as i64,
                (h / 4 + k as u32 * crop_h) as i64,
            );
        }
    }
    panel
        .save(out_dir.join("bike_thin_before_after.jpg"))
        .unwrap();
}
