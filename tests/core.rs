//! Fidelity-core tests, ported from the Python suite
//! (tests/test_composite.py) plus the one-pixel negative control.

use agenthand::assets::{AssetStore, KIND_FOREGROUND, KIND_MASK, KIND_PLATE, KIND_SOURCE};
use agenthand::composite;
use agenthand::image::{encode_png_gray, encode_png_rgba, GrayImage, RgbaImage};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_store() -> (AssetStore, std::path::PathBuf) {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("agenthand-rust-test-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    (AssetStore::new(&dir), dir)
}

fn bare_id(handle: &str) -> String {
    AssetStore::parse_handle(handle).unwrap()
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

fn set_px(img: &mut RgbaImage, x: u32, y: u32, rgba: [u8; 4]) {
    let i = ((y * img.width + x) * 4) as usize;
    img.pixels[i..i + 4].copy_from_slice(&rgba);
}

/// The Python fixture: 6x4 foreground, columns 0-2 opaque red,
/// column 3 half-alpha green, columns 4-5 transparent.
fn edge_foreground() -> RgbaImage {
    build(6, 4, |x, _y| match x {
        0..=2 => [255, 0, 0, 255],
        3 => [0, 255, 0, 128],
        _ => [0, 0, 0, 0],
    })
}

fn solid_plate() -> RgbaImage {
    RgbaImage::solid(6, 4, [0, 0, 255, 255])
}

#[test]
fn composite_counts_opaque_edge_and_transparent_separately() {
    let (_out, counts, _t) = composite::composite_images(&edge_foreground(), &solid_plate());
    assert_eq!(counts.opaque_pixels, 12);
    assert_eq!(counts.edge_band_pixels, 4);
    assert_eq!(counts.transparent_pixels, 8);
    assert_eq!(counts.changed_opaque_pixels, 0);
}

#[test]
fn composite_pastes_plate_through_transparent_pixels() {
    let (out, _c, _t) = composite::composite_images(&edge_foreground(), &solid_plate());
    assert_eq!(out.pixel(5, 0), [0, 0, 255, 255]);
    assert_eq!(out.pixel(0, 0), [255, 0, 0, 255]);
}

#[test]
fn edge_band_pixel_is_blended_once_and_never_counted_as_changed() {
    let (out, counts, _t) = composite::composite_images(&edge_foreground(), &solid_plate());
    let px = out.pixel(3, 0);
    assert_eq!(px[3], 255);
    assert!((100..160).contains(&px[1]), "green channel {px:?}");
    assert!((100..160).contains(&px[2]), "blue channel {px:?}");
    assert_eq!(counts.opaque_pixels + counts.edge_band_pixels, 16);
}

#[test]
fn one_pixel_negative_control_reports_exactly_one() {
    // The honesty check: flip exactly ONE opaque pixel in a copy of
    // a known-good composite and the counter must say exactly 1.
    let (mut out, counts, _t) = composite::composite_images(&edge_foreground(), &solid_plate());
    assert_eq!(counts.changed_opaque_pixels, 0);
    let before = out.pixel(1, 1);
    set_px(
        &mut out,
        1,
        1,
        [before[0] ^ 0xFF, before[1], before[2], before[3]],
    );
    let recount = composite::measure(&edge_foreground(), &out);
    assert_eq!(recount.changed_opaque_pixels, 1);
    assert_eq!(recount.opaque_pixels, 12);
}

#[test]
fn alpha_250_ring_is_edge_band_not_opaque() {
    let fg = build(6, 6, |x, y| {
        if (2..=3).contains(&x) && (2..=3).contains(&y) {
            [255, 0, 0, 255]
        } else if (1..=4).contains(&x) && (1..=4).contains(&y) {
            [0, 200, 0, 250]
        } else {
            [0, 0, 0, 0]
        }
    });
    let plate = RgbaImage::solid(6, 6, [9, 9, 9, 255]);
    let (_out, counts, _t) = composite::composite_images(&fg, &plate);
    assert_eq!(counts.opaque_pixels, 4);
    assert_eq!(counts.edge_band_pixels, 12);
    assert_eq!(counts.mask_pixels_total, 16);
    assert_eq!(counts.changed_opaque_pixels, 0);
}

#[test]
fn feathered_edge_band_width_is_measured_in_pixels() {
    let ramp = [51u8, 102, 153, 204];
    let fg = build(12, 3, |x, _y| {
        if x < 4 {
            [255, 0, 0, 255]
        } else if x < 8 {
            [255, 0, 0, ramp[(x - 4) as usize]]
        } else {
            [0, 0, 0, 0]
        }
    });
    let plate = RgbaImage::solid(12, 3, [0, 0, 255, 255]);
    let (_out, counts, _t) = composite::composite_images(&fg, &plate);
    assert_eq!(counts.edge_band_pixels, 12);
    assert_eq!(counts.edge_band_max_width_px, 4);
    assert_eq!(counts.mask_pixels_total, 24);
    assert_eq!(counts.changed_opaque_pixels, 0);
}

#[test]
fn checkerboard_opaque_pixels_match_exactly() {
    let fg = build(8, 8, |x, y| {
        if (x + y) % 2 == 0 {
            [(10 + 20 * x) as u8, (10 + 20 * y) as u8, 99, 255]
        } else {
            [0, 0, 0, 0]
        }
    });
    let plate = RgbaImage::solid(8, 8, [200, 200, 200, 255]);
    let (out, counts, _t) = composite::composite_images(&fg, &plate);
    assert_eq!(counts.opaque_pixels, 32);
    assert_eq!(counts.changed_opaque_pixels, 0);
    assert_eq!(out.pixel(0, 0), [10, 10, 99, 255]);
    assert_eq!(out.pixel(2, 0), [50, 10, 99, 255]);
}

#[test]
fn hostile_plate_alpha_cannot_change_the_product() {
    let plate = build(6, 4, |x, _y| [255, 0, 0, (x * 51) as u8]);
    let (_out, counts, _t) = composite::composite_images(&edge_foreground(), &plate);
    assert_eq!(counts.changed_opaque_pixels, 0);
}

#[test]
fn receipt_carries_the_full_fidelity_fields() {
    let (store, dir) = temp_store();
    let source = RgbaImage::solid(6, 4, [0, 0, 0, 255]);
    let source_handle = store.put_image(&source, KIND_SOURCE, None).unwrap();
    let fg_handle = store
        .put_image(
            &edge_foreground(),
            KIND_FOREGROUND,
            Some(&bare_id(&source_handle)),
        )
        .unwrap();
    let plate_handle = store.put_image(&solid_plate(), KIND_PLATE, None).unwrap();

    let (_handle, receipt) =
        composite::draw_product(&store, &fg_handle, "a blue room", Some(&plate_handle)).unwrap();
    assert_eq!(receipt["changed_opaque_pixels"], 0);
    assert_eq!(receipt["opaque_pixels"], 12);
    assert_eq!(receipt["edge_band_pixels"], 4);
    assert_eq!(receipt["transparent_pixels"], 8);
    assert_eq!(receipt["mask_pixels_total"], 16);
    assert_eq!(receipt["opaque_fraction"], 0.75);
    assert_eq!(receipt["edge_band_max_width_px"], 1);
    assert_eq!(receipt["opaque_alpha_threshold"], 255);
    assert_eq!(receipt["pixel_tolerance"], 0);
    assert_eq!(receipt["count_sample"], "decoded_delivered_file");
    assert_eq!(receipt["output_format"], "PNG");
    assert_eq!(receipt["plate_source"], "asset");
    assert_eq!(receipt["source_asset"], source_handle);
    assert_eq!(receipt["foreground_asset"], fg_handle);
    assert_eq!(receipt["mask_asset"], serde_json::Value::Null);
    assert_eq!(receipt["mask_source"], "foreground_alpha");
    assert!(receipt["claim"]
        .as_str()
        .unwrap()
        .contains("reached the composite unchanged"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn draw_uses_mask_asset_when_chained_through_lineage() {
    let (store, dir) = temp_store();
    let source = store
        .put_image(&RgbaImage::solid(6, 4, [0, 0, 0, 255]), KIND_SOURCE, None)
        .unwrap();
    let mask = GrayImage {
        width: 6,
        height: 4,
        pixels: vec![255; 24],
        icc_profile: None,
    };
    let mask_handle = store
        .put_gray_image(&mask, KIND_MASK, Some(&bare_id(&source)))
        .unwrap();
    let fg_handle = store
        .put_image(
            &edge_foreground(),
            KIND_FOREGROUND,
            Some(&bare_id(&mask_handle)),
        )
        .unwrap();
    let plate_handle = store.put_image(&solid_plate(), KIND_PLATE, None).unwrap();
    let (_h, receipt) =
        composite::draw_product(&store, &fg_handle, "a blue room", Some(&plate_handle)).unwrap();
    assert_eq!(receipt["mask_asset"], mask_handle);
    assert_eq!(receipt["mask_source"], "asset");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn draw_rejects_a_tampered_foreground() {
    let (store, dir) = temp_store();
    let source = store
        .put_image(&RgbaImage::solid(6, 4, [0, 0, 0, 255]), KIND_SOURCE, None)
        .unwrap();
    let fg_handle = store
        .put_image(&edge_foreground(), KIND_FOREGROUND, Some(&bare_id(&source)))
        .unwrap();
    // Flip bytes in the stored file after the checksums were recorded.
    let path = store.resolve(&fg_handle).unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    let n = bytes.len();
    bytes[n - 20] ^= 0xFF;
    std::fs::write(&path, bytes).unwrap();
    let err = composite::draw_product(&store, &fg_handle, "x", None).unwrap_err();
    assert!(
        err.contains("file hash mismatch"),
        "unexpected error: {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn draw_rejects_a_non_foreground_asset() {
    let (store, dir) = temp_store();
    let plate_handle = store.put_image(&solid_plate(), KIND_PLATE, None).unwrap();
    let err = composite::draw_product(&store, &plate_handle, "x", None).unwrap_err();
    assert!(
        err.contains("draw requires a foreground_png asset"),
        "unexpected error: {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn plate_is_fitted_to_the_product_size() {
    let big_plate = RgbaImage::solid(12, 8, [0, 0, 255, 255]);
    let (out, counts, transformations) =
        composite::composite_images(&edge_foreground(), &big_plate);
    assert_eq!((out.width, out.height), (6, 4));
    assert_eq!(counts.changed_opaque_pixels, 0);
    assert_eq!(
        transformations,
        vec!["plate_resized_and_center_cropped_to_6x4".to_string()]
    );
}

#[test]
fn icc_profile_survives_and_its_hash_is_on_the_receipt() {
    let (store, dir) = temp_store();
    // A stand-in ICC blob: preservation is byte-level, so any stable
    // byte string proves the chain; PIL interop of real profile
    // bytes is covered by the parity gate (the real Meta profile).
    let profile: Vec<u8> = (0..504u32).map(|i| (i % 251) as u8).collect();
    let mut source = RgbaImage::solid(6, 4, [0, 0, 0, 255]);
    source.icc_profile = Some(profile.clone());
    let source_handle = store.put_image(&source, KIND_SOURCE, None).unwrap();
    let mut fg = edge_foreground();
    fg.icc_profile = Some(profile.clone());
    let fg_handle = store
        .put_image(&fg, KIND_FOREGROUND, Some(&bare_id(&source_handle)))
        .unwrap();
    let plate_handle = store.put_image(&solid_plate(), KIND_PLATE, None).unwrap();
    let (handle, receipt) =
        composite::draw_product(&store, &fg_handle, "room", Some(&plate_handle)).unwrap();
    assert_eq!(receipt["color_profile_preserved"], true);
    let expected = agenthand::image::sha256_hex(&profile);
    assert_eq!(receipt["color_profile_source_sha256"], expected);
    assert_eq!(receipt["color_profile_output_sha256"], expected);
    let record = store.get_record(&handle).unwrap();
    assert_eq!(
        record.color_profile_sha256.as_deref(),
        Some(expected.as_str())
    );
    // And the delivered file itself carries the profile bytes.
    let delivered = store.read_image(&handle).unwrap();
    assert_eq!(delivered.icc_profile.as_deref(), Some(profile.as_slice()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn verify_files_flags_a_tampered_pixel() {
    let dir = std::env::temp_dir().join(format!("agenthand-verify-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let source = RgbaImage::solid(4, 2, [10, 20, 30, 255]);
    let mask_gray = GrayImage {
        width: 4,
        height: 2,
        pixels: vec![255, 0, 0, 0, 0, 255, 0, 0],
        icc_profile: None,
    };
    let mut final_ = source.clone();
    std::fs::write(dir.join("s.png"), encode_png_rgba(&source)).unwrap();
    std::fs::write(dir.join("m.png"), encode_png_gray(&mask_gray)).unwrap();
    std::fs::write(dir.join("f.png"), encode_png_rgba(&final_)).unwrap();
    let report =
        composite::verify_files(&dir.join("s.png"), &dir.join("f.png"), &dir.join("m.png"))
            .unwrap();
    assert_eq!(report["ok"], true);
    assert_eq!(report["checked_pixels"], 2);
    assert_eq!(report["changed_pixels"], 0);

    set_px(&mut final_, 0, 0, [11, 20, 30, 255]);
    std::fs::write(dir.join("f.png"), encode_png_rgba(&final_)).unwrap();
    let report =
        composite::verify_files(&dir.join("s.png"), &dir.join("f.png"), &dir.join("m.png"))
            .unwrap();
    assert_eq!(report["ok"], false);
    assert_eq!(report["changed_pixels"], 1);
    let _ = std::fs::remove_dir_all(&dir);
}
