//! Receipt persistence parity with the Python build.
//!
//! Python behaviour under test (assets.save_receipt /
//! composite.draw_product / composite.verify_receipt_from_disk):
//! draw persists `<asset_id>.receipt.json` beside the composite via
//! temp file + atomic rename; the verifier loads the persisted
//! receipt when none is supplied; a receipt binds to the exact
//! files it names (a different run's receipt fails); a pass
//! requires exactly 0 changed pixels and positive coverage; the
//! standalone file verifier refuses an empty mask.

use agenthand::assets::{AssetStore, KIND_FOREGROUND, KIND_MASK, KIND_SOURCE};
use agenthand::composite;
use agenthand::image::{GrayImage, RgbaImage};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_store() -> (AssetStore, std::path::PathBuf) {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("agenthand-rust-receipt-{}-{n}", std::process::id()));
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

/// Store with a source photo and a chained foreground; returns
/// (photo handle, foreground handle).
fn seeded_store() -> (AssetStore, std::path::PathBuf, String, String) {
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

#[test]
fn draw_persists_the_receipt_beside_the_composite() {
    let (store, _dir, _photo, fg) = seeded_store();
    let (handle, receipt) = composite::draw_product(&store, &fg, "a regal room", None).unwrap();

    let path = store.receipt_path(&handle).unwrap();
    let bare = AssetStore::parse_handle(&handle).unwrap();
    assert_eq!(
        path.file_name().unwrap().to_string_lossy(),
        format!("{bare}.receipt.json")
    );
    // Beside the composite's data file.
    assert_eq!(
        path.parent().unwrap(),
        store.resolve(&handle).unwrap().parent().unwrap()
    );
    let text = std::fs::read_to_string(&path).unwrap();
    let persisted: Value = serde_json::from_str(&text).unwrap();
    // Every field of the returned receipt is in the file, unchanged.
    assert_eq!(persisted, receipt);
    assert_eq!(persisted["changed_opaque_pixels"], json!(0));
    // Sorted-key, indent-2 JSON (Python json.dumps(indent=2,
    // sort_keys=True)): re-serializing the parsed value reproduces
    // the file byte for byte, and no torn tmp file is left behind.
    assert_eq!(text, serde_json::to_string_pretty(&persisted).unwrap());
    let leftovers: Vec<_> = std::fs::read_dir(store.assets_dir.clone())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "tmp files left: {leftovers:?}");
    // load_receipt round-trips.
    assert_eq!(store.load_receipt(&handle).unwrap(), Some(receipt));
}

#[test]
fn load_receipt_is_none_when_nothing_was_persisted() {
    let (store, _dir, photo, _fg) = seeded_store();
    assert_eq!(store.load_receipt(&photo).unwrap(), None);
}

#[test]
fn verify_loads_the_persisted_receipt_when_none_supplied() {
    let (store, _dir, _photo, fg) = seeded_store();
    let (handle, _receipt) = composite::draw_product(&store, &fg, "a regal room", None).unwrap();
    let report = composite::verify_receipt_from_disk(&store, &handle, None).unwrap();
    assert_eq!(
        report["ok"],
        json!(true),
        "problems: {}",
        report["problems"]
    );
}

#[test]
fn verify_without_any_receipt_is_an_error() {
    let (store, _dir, photo, _fg) = seeded_store();
    let err = composite::verify_receipt_from_disk(&store, &photo, None).unwrap_err();
    assert!(
        err.contains("No receipt supplied and none persisted beside"),
        "unexpected error: {err}"
    );
}

#[test]
fn a_receipt_does_not_verify_a_different_runs_composite() {
    let (store, _dir, _photo, fg) = seeded_store();
    let (handle_a, receipt_a) = composite::draw_product(&store, &fg, "a regal room", None).unwrap();
    let (handle_b, _receipt_b) =
        composite::draw_product(&store, &fg, "a regal room", None).unwrap();
    assert_ne!(handle_a, handle_b);
    let report = composite::verify_receipt_from_disk(&store, &handle_b, Some(&receipt_a)).unwrap();
    assert_eq!(report["ok"], json!(false));
    assert_eq!(
        report["checks"]["output_hash_matches_receipt"],
        json!(false)
    );
}

#[test]
fn a_receipt_claiming_a_change_fails_the_verdict_checks() {
    let (store, _dir, _photo, fg) = seeded_store();
    let (handle, mut receipt) = composite::draw_product(&store, &fg, "a regal room", None).unwrap();
    // An "honest" receipt that reports a changed pixel: the recount
    // checks were never the whole verdict — zero_changed_pixels
    // must fail even where a recount would agree.
    receipt["changed_opaque_pixels"] = json!(1);
    let report = composite::verify_receipt_from_disk(&store, &handle, Some(&receipt)).unwrap();
    assert_eq!(report["ok"], json!(false));
    assert_eq!(report["checks"]["zero_changed_pixels"], json!(false));

    let (_h, mut receipt2) = composite::draw_product(&store, &fg, "a regal room", None).unwrap();
    receipt2["opaque_pixels"] = json!(0);
    let handle2 = receipt2["output_asset"].as_str().unwrap().to_string();
    let report2 = composite::verify_receipt_from_disk(&store, &handle2, Some(&receipt2)).unwrap();
    assert_eq!(report2["checks"]["positive_coverage"], json!(false));
}

#[test]
fn legacy_receipt_without_mask_hashes_uses_mask_integrity() {
    let (store, _dir) = temp_store();
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
    let (handle, mut receipt) =
        composite::draw_product(&store, &fg_handle, "a regal room", None).unwrap();
    assert_eq!(receipt["mask_asset"], json!(mask_handle));
    // Strip the mask hash pins: the pre-pinning receipt shape.
    receipt["mask_sha256"] = Value::Null;
    receipt["mask_pixel_sha256"] = Value::Null;
    let report = composite::verify_receipt_from_disk(&store, &handle, Some(&receipt)).unwrap();
    assert_eq!(report["checks"]["mask_asset_integrity"], json!(true));
    assert_eq!(
        report["ok"],
        json!(true),
        "problems: {}",
        report["problems"]
    );
}

#[test]
fn verify_files_refuses_an_empty_mask() {
    let (_store, dir) = temp_store();
    std::fs::create_dir_all(&dir).unwrap();
    let source = build(4, 2, |x, _| [10 * x as u8, 0, 0, 255]);
    let other = build(4, 2, |x, _| [200 - 10 * x as u8, 99, 99, 255]);
    let mask = GrayImage {
        width: 4,
        height: 2,
        pixels: vec![0u8; 8],
        icc_profile: None,
    };
    let sp = dir.join("source.png");
    let fp = dir.join("final.png");
    let mp = dir.join("mask.png");
    std::fs::write(&sp, agenthand::image::encode_png_rgba(&source)).unwrap();
    std::fs::write(&fp, agenthand::image::encode_png_rgba(&other)).unwrap();
    std::fs::write(&mp, agenthand::image::encode_png_gray(&mask)).unwrap();
    // Two completely different images, empty mask: 0 of 0 pixels
    // changed must NOT pass.
    let report = composite::verify_files(&sp, &fp, &mp).unwrap();
    assert_eq!(report["ok"], json!(false));
    let problems = report["problems"].to_string();
    assert!(
        problems.contains("selects no fully-on pixels"),
        "problems: {problems}"
    );
}
