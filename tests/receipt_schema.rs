//! Receipt schema marker parity with the Python build.
//!
//! Python behaviour under test (composite.RECEIPT_SCHEMA_VERSION /
//! composite._validate_receipt): draw stamps `receipt_schema: 2`
//! on every receipt it issues; the verifier grades a stamped
//! receipt as current, grades an unmarked receipt under the legacy
//! v1 policy, and refuses a receipt that carries current-schema
//! fields under a missing or invalid marker (the downgrade guard:
//! dropping the marker must not buy a weaker grading). A genuine
//! v1 receipt — the frozen 2026-10-26 replay receipt is the live
//! example — carries no marker and none of the v2-only fields, and
//! still verifies.

use agenthand::assets::{AssetStore, KIND_FOREGROUND, KIND_MASK, KIND_SOURCE};
use agenthand::composite;
use agenthand::image::{GrayImage, RgbaImage};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// The fields that exist only in the current schema (the Python
/// `_V2_ONLY_FIELDS` set: the v2 field set minus `mask_asset`).
const V2_ONLY_FIELDS: [&str; 9] = [
    "color_profile_foreground_sha256",
    "mask_sha256",
    "mask_pixel_sha256",
    "mask_source",
    "opaque_alpha_threshold",
    "pixel_tolerance",
    "count_sample",
    "product_layer_scaled",
    "output_format",
];

fn temp_store() -> (AssetStore, std::path::PathBuf) {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("agenthand-rust-schema-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    (AssetStore::new(&dir), dir)
}

fn build(width: u32, height: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> RgbaImage {
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            let i = ((y * width + x) * 4) as usize;
            pixels[i..i + 4].copy_from_slice(&f(x, y));
        }
    }
    RgbaImage::new(width, height, pixels)
}

/// Store with a source photo and a chained foreground whose mask
/// comes from its alpha (draw's `foreground_alpha` path);
/// returns (photo handle, foreground handle).
fn seeded_store_alpha_mask() -> (AssetStore, std::path::PathBuf, String, String) {
    let (store, dir) = temp_store();
    let photo = build(6, 4, |_, _| [90, 90, 90, 255]);
    let photo_handle = store.put_image(&photo, KIND_SOURCE, None).unwrap();
    let fg = build(6, 4, |x, _| match x {
        0..=2 => [255, 0, 0, 255],
        3 => [0, 255, 0, 128],
        _ => [0, 0, 0, 0],
    });
    let parent = AssetStore::parse_handle(&photo_handle).unwrap();
    let fg_handle = store
        .put_image(&fg, KIND_FOREGROUND, Some(&parent))
        .unwrap();
    (store, dir, photo_handle, fg_handle)
}

/// Store whose foreground chains through a stored mask asset
/// (draw's `asset` mask path); returns the foreground handle.
fn seeded_store_mask_asset() -> (AssetStore, std::path::PathBuf, String) {
    let (store, dir) = temp_store();
    let photo = build(6, 4, |_, _| [90, 90, 90, 255]);
    let photo_handle = store.put_image(&photo, KIND_SOURCE, None).unwrap();
    let photo_id = AssetStore::parse_handle(&photo_handle).unwrap();
    let mask = GrayImage {
        width: 6,
        height: 4,
        pixels: vec![255u8; 24],
        icc_profile: None,
    };
    let mask_handle = store
        .put_gray_image(&mask, KIND_MASK, Some(&photo_id))
        .unwrap();
    let mask_id = AssetStore::parse_handle(&mask_handle).unwrap();
    let fg = build(6, 4, |x, _| match x {
        0..=2 => [255, 0, 0, 255],
        _ => [0, 0, 0, 0],
    });
    let fg_handle = store
        .put_image(&fg, KIND_FOREGROUND, Some(&mask_id))
        .unwrap();
    (store, dir, fg_handle)
}

#[test]
fn draw_stamps_the_current_schema_marker() {
    let (store, _dir, _photo, fg) = seeded_store_alpha_mask();
    let (handle, receipt) = composite::draw_product(&store, &fg, "a regal room", None).unwrap();
    assert_eq!(
        receipt["receipt_schema"],
        json!(composite::RECEIPT_SCHEMA_VERSION)
    );
    assert_eq!(receipt["receipt_schema"], json!(2));
    // The persisted copy carries the marker too.
    let persisted = store.load_receipt(&handle).unwrap().unwrap();
    assert_eq!(persisted["receipt_schema"], json!(2));
    // And the stamped receipt verifies as current.
    let report = composite::verify_receipt_from_disk(&store, &handle, Some(&receipt)).unwrap();
    assert_eq!(
        report["ok"],
        json!(true),
        "problems: {}",
        report["problems"]
    );
}

#[test]
fn stripped_marker_on_a_current_receipt_is_refused() {
    let (store, _dir, _photo, fg) = seeded_store_alpha_mask();
    let (handle, mut receipt) = composite::draw_product(&store, &fg, "a regal room", None).unwrap();
    // The downgrade attack: remove ONLY the marker, keep every
    // current-schema field. The legacy policy must not apply.
    receipt.as_object_mut().unwrap().remove("receipt_schema");
    let report = composite::verify_receipt_from_disk(&store, &handle, Some(&receipt)).unwrap();
    assert_eq!(report["ok"], json!(false));
    assert_eq!(report["checks"]["receipt_shape"], json!(false));
    let problems = report["problems"].to_string();
    assert!(
        problems.contains("current-schema fields but no valid receipt_schema marker"),
        "problems: {problems}"
    );

    // Same refusal on the mask-asset path (mask pins present).
    let (store2, _dir2, fg2) = seeded_store_mask_asset();
    let (handle2, mut receipt2) =
        composite::draw_product(&store2, &fg2, "a regal room", None).unwrap();
    receipt2.as_object_mut().unwrap().remove("receipt_schema");
    let report2 = composite::verify_receipt_from_disk(&store2, &handle2, Some(&receipt2)).unwrap();
    assert_eq!(report2["ok"], json!(false));
    assert_eq!(report2["checks"]["receipt_shape"], json!(false));
}

#[test]
fn nulled_marker_on_a_current_receipt_is_refused() {
    let (store, _dir, _photo, fg) = seeded_store_alpha_mask();
    let (handle, mut receipt) = composite::draw_product(&store, &fg, "a regal room", None).unwrap();
    // An explicit null is the unmarked state in the Python
    // verifier (receipt.get returns None): the guard still fires,
    // and no "unsupported" problem is raised for it.
    receipt["receipt_schema"] = Value::Null;
    let report = composite::verify_receipt_from_disk(&store, &handle, Some(&receipt)).unwrap();
    assert_eq!(report["ok"], json!(false));
    let problems = report["problems"].to_string();
    assert!(
        problems.contains("current-schema fields but no valid receipt_schema marker"),
        "problems: {problems}"
    );
    assert!(
        !problems.contains("unsupported receipt_schema"),
        "problems: {problems}"
    );
}

#[test]
fn lowered_or_raised_marker_is_unsupported() {
    let (store, _dir, _photo, fg) = seeded_store_alpha_mask();
    let (handle, receipt) = composite::draw_product(&store, &fg, "a regal room", None).unwrap();
    for bad in [json!(1), json!(3), json!("2"), json!(2.0), json!(true)] {
        let mut forged = receipt.clone();
        forged["receipt_schema"] = bad.clone();
        let report = composite::verify_receipt_from_disk(&store, &handle, Some(&forged)).unwrap();
        assert_eq!(report["ok"], json!(false), "marker {bad} must fail");
        assert_eq!(report["checks"]["receipt_shape"], json!(false));
        let problems = report["problems"].to_string();
        assert!(
            problems.contains("unsupported receipt_schema"),
            "marker {bad}, problems: {problems}"
        );
    }
}

#[test]
fn genuine_v1_shape_still_verifies() {
    // Alpha-mask path: strip the marker AND every v2-only field —
    // the genuine v1 shape (the frozen receipt's shape).
    let (store, _dir, _photo, fg) = seeded_store_alpha_mask();
    let (handle, mut receipt) = composite::draw_product(&store, &fg, "a regal room", None).unwrap();
    let obj = receipt.as_object_mut().unwrap();
    obj.remove("receipt_schema");
    for field in V2_ONLY_FIELDS {
        obj.remove(field);
    }
    let report = composite::verify_receipt_from_disk(&store, &handle, Some(&receipt)).unwrap();
    assert_eq!(
        report["ok"],
        json!(true),
        "problems: {}",
        report["problems"]
    );

    // Mask-asset path: the v1 shape names the mask asset without
    // pinning its hashes, so the verifier falls back to the mask
    // file's integrity against its own record.
    let (store2, _dir2, fg2) = seeded_store_mask_asset();
    let (handle2, mut receipt2) =
        composite::draw_product(&store2, &fg2, "a regal room", None).unwrap();
    let obj2 = receipt2.as_object_mut().unwrap();
    obj2.remove("receipt_schema");
    for field in V2_ONLY_FIELDS {
        obj2.remove(field);
    }
    assert!(receipt2["mask_asset"].is_string());
    let report2 = composite::verify_receipt_from_disk(&store2, &handle2, Some(&receipt2)).unwrap();
    assert_eq!(
        report2["ok"],
        json!(true),
        "problems: {}",
        report2["problems"]
    );
    assert_eq!(report2["checks"]["mask_asset_integrity"], json!(true));
}

/// The source-derived plate kinds (`source_edit`, and `source_blur`
/// when the code plate stood in) ride in the same schema-2 receipt:
/// stamped, carrying plate_kind and the backdrop record, and the
/// verifier grades them exactly like any other plate (it never
/// recomputes plates; it pins their hashes and recounts the product).
#[test]
fn source_plate_kinds_stamp_schema_2_and_verify() {
    use agenthand::assets::KIND_PLATE;
    use agenthand::{backdrop, staging};
    for kind in [
        backdrop::PLATE_KIND_SOURCE_EDIT,
        backdrop::PLATE_KIND_SOURCE_BLUR,
    ] {
        let (store, _dir, photo, fg) = seeded_store_alpha_mask();
        let source = store.read_image(&photo).unwrap();
        let foreground = store.read_image(&fg).unwrap();
        let geometry = staging::analyze_subject(&foreground);
        let plate = staging::finish_plate(
            &staging::fallback_plate(&source, &foreground),
            &foreground,
            &geometry,
        );
        let plate_handle = store.put_image(&plate, KIND_PLATE, None).unwrap();
        let backdrop_value = backdrop::source_plate_receipt_value(
            kind,
            geometry.to_json(),
            json!({"size": "1536x1024", "dilate_px": 8}),
            json!({"model": "muse-image-1.0"}),
            geometry.shadow,
            geometry.matte_px,
        );
        let (handle, receipt) = composite::draw_product_with_plate_kind(
            &store,
            &fg,
            "a clean driveway",
            Some(&plate_handle),
            Some((kind, backdrop_value)),
        )
        .unwrap();
        assert_eq!(receipt["receipt_schema"], json!(2));
        assert_eq!(receipt["plate_kind"], json!(kind));
        assert_eq!(receipt["backdrop"]["kind"], json!(kind));
        assert_eq!(receipt["plate_source"], json!("asset"));
        assert_eq!(receipt["changed_opaque_pixels"], json!(0));
        assert_eq!(receipt["product_layer_scaled"], json!(false));
        let report = composite::verify_receipt_from_disk(&store, &handle, Some(&receipt)).unwrap();
        assert_eq!(report["ok"], json!(true), "{kind}: {}", report["problems"]);
    }
}
