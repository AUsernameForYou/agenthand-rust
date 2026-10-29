//! SAM working-resolution scaling (decisions 68/69): the SAM-bound
//! copy is downscaled past the proven cap, the returned mask is
//! mapped back onto the full frame, and the receipt records the
//! mask's provenance. Offline units; no network, no key.

use agenthand::assets::{AssetStore, KIND_FOREGROUND, KIND_MASK, KIND_PLATE, KIND_SOURCE};
use agenthand::composite;
use agenthand::image::{encode_png_gray_with_text, GrayImage, RgbaImage};
use agenthand::segmentation;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_store() -> (AssetStore, std::path::PathBuf) {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("agenthand-sam-scaling-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    (AssetStore::new(&dir), dir)
}

fn bare_id(handle: &str) -> String {
    AssetStore::parse_handle(handle).unwrap()
}

fn rect_mask(width: u32, height: u32, rect: (u32, u32, u32, u32)) -> GrayImage {
    let mut pixels = vec![0u8; (width * height) as usize];
    for y in rect.1..rect.3 {
        for x in rect.0..rect.2 {
            pixels[(y * width + x) as usize] = 255;
        }
    }
    GrayImage {
        width,
        height,
        pixels,
        icc_profile: None,
    }
}

fn px(mask: &GrayImage, x: u32, y: u32) -> u8 {
    mask.pixels[(y * mask.width + x) as usize]
}

// -- cap math -----------------------------------------------------------------

#[test]
fn sam_frame_size_keeps_the_proven_frame_unchanged() {
    assert_eq!(segmentation::sam_frame_size(1206, 1608, None), (1206, 1608));
    assert_eq!(segmentation::sam_frame_size(720, 1280, None), (720, 1280));
}

#[test]
fn sam_frame_size_maps_the_24mp_original_to_the_proven_frame() {
    // IMG_1340's stored geometry (4284x5712), the photo whose
    // full-size cut_out SAM refused with HTTP 413.
    assert_eq!(segmentation::sam_frame_size(4284, 5712, None), (1206, 1608));
}

#[test]
fn sam_frame_size_respects_the_cap_for_other_aspects() {
    let (w, h) = segmentation::sam_frame_size(6000, 2000, Some(1_000_000));
    assert!((w * h) <= 1_000_000);
    assert!((w as f64 / h as f64 - 3.0).abs() < 0.01);
}

// -- mask mapping geometry ------------------------------------------------------

#[test]
fn map_mask_to_frame_uniform_scale_lands_on_the_right_pixels() {
    let mask = rect_mask(200, 150, (50, 30, 100, 60));
    let mapped = segmentation::map_mask_to_frame(&mask, 400, 300);
    assert_eq!((mapped.width, mapped.height), (400, 300));
    assert_eq!(px(&mapped, 150, 90), 255);
    assert_eq!(px(&mapped, 105, 65), 255);
    assert_eq!(px(&mapped, 95, 55), 0);
    assert_eq!(px(&mapped, 205, 125), 0);
    assert_eq!(px(&mapped, 399, 299), 0);
}

#[test]
fn map_mask_to_frame_nonuniform_scale_lands_on_the_right_pixels() {
    let mask = rect_mask(200, 150, (50, 30, 100, 60));
    let mapped = segmentation::map_mask_to_frame(&mask, 400, 450);
    assert_eq!(px(&mapped, 150, 135), 255);
    assert_eq!(px(&mapped, 150, 60), 0);
    assert_eq!(px(&mapped, 60, 135), 0);
}

#[test]
fn map_mask_to_frame_same_size_is_identity() {
    let mask = rect_mask(8, 6, (0, 0, 4, 6));
    let mapped = segmentation::map_mask_to_frame(&mask, 8, 6);
    assert_eq!(mapped.pixels, mask.pixels);
}

// -- SAM payload ------------------------------------------------------------------

#[test]
fn prepare_sam_png_passes_small_frames_through_byte_identical() {
    let source = RgbaImage::solid(8, 6, [10, 20, 30, 255]);
    let stored = b"the stored bytes, untouched".to_vec();
    let prepared = segmentation::prepare_sam_png(&source, &stored, None);
    assert_eq!(prepared.png_bytes, stored);
    assert_eq!((prepared.sam_width, prepared.sam_height), (8, 6));
    assert_eq!(prepared.scale_method, "none");
}

#[test]
fn prepare_sam_png_scales_oversize_frames_to_the_cap() {
    let source = RgbaImage::solid(400, 300, [10, 20, 30, 255]);
    let prepared = segmentation::prepare_sam_png(&source, b"unused", Some(10_000));
    assert_eq!((prepared.sam_width, prepared.sam_height), (115, 86));
    assert_eq!(prepared.scale_method, "bilinear_threshold_128");
    let decoded = agenthand::image::decode_image(&prepared.png_bytes).unwrap();
    match decoded {
        agenthand::image::DecodedImage::Rgba(img) => {
            assert_eq!((img.width, img.height), (115, 86));
        }
        _ => panic!("SAM payload must decode as an image"),
    }
}

// -- receipt provenance -----------------------------------------------------------

fn draw_with_mask_bytes(
    store: &AssetStore,
    mask_bytes: &[u8],
) -> (String, serde_json::Value, String) {
    let source = store
        .put_image(&RgbaImage::solid(6, 4, [0, 0, 0, 255]), KIND_SOURCE, None)
        .unwrap();
    let mask_handle = store
        .put_bytes(mask_bytes, KIND_MASK, ".png", Some(&bare_id(&source)))
        .unwrap();
    let fg_handle = store
        .put_image(
            &RgbaImage::solid(6, 4, [200, 30, 30, 255]),
            KIND_FOREGROUND,
            Some(&bare_id(&mask_handle)),
        )
        .unwrap();
    let plate_handle = store
        .put_image(&RgbaImage::solid(6, 4, [0, 0, 255, 255]), KIND_PLATE, None)
        .unwrap();
    let (handle, receipt) =
        composite::draw_product(store, &fg_handle, "a blue room", Some(&plate_handle)).unwrap();
    (handle, receipt, fg_handle)
}

#[test]
fn draw_receipt_records_mask_provenance_from_the_mask_asset() {
    let (store, dir) = temp_store();
    let mask = GrayImage {
        width: 6,
        height: 4,
        pixels: vec![255; 24],
        icc_profile: None,
    };
    let provenance = "{\"sam_height\":1608,\"sam_width\":1206,\"scale_method\":\"bilinear_threshold_128\",\"source_height\":5712,\"source_width\":4284}";
    let bytes = encode_png_gray_with_text(
        &mask,
        agenthand::assets::MASK_PROVENANCE_PNG_KEY,
        provenance,
    );
    let (handle, receipt, _fg) = draw_with_mask_bytes(&store, &bytes);
    assert_eq!(receipt["mask_source_width"], 4284);
    assert_eq!(receipt["mask_source_height"], 5712);
    assert_eq!(receipt["mask_sam_width"], 1206);
    assert_eq!(receipt["mask_sam_height"], 1608);
    assert_eq!(receipt["mask_scale_method"], "bilinear_threshold_128");
    // The provenance-carrying receipt still verifies clean.
    let report = composite::verify_receipt_from_disk(&store, &handle, None).unwrap();
    assert_eq!(report["ok"], true, "problems: {}", report["problems"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn draw_receipt_without_provenance_chunk_carries_no_provenance_fields() {
    let (store, dir) = temp_store();
    let mask = GrayImage {
        width: 6,
        height: 4,
        pixels: vec![255; 24],
        icc_profile: None,
    };
    let bytes = agenthand::image::encode_png_gray(&mask);
    let (handle, receipt, _fg) = draw_with_mask_bytes(&store, &bytes);
    assert!(receipt.get("mask_source_width").is_none());
    assert!(receipt.get("mask_scale_method").is_none());
    let report = composite::verify_receipt_from_disk(&store, &handle, None).unwrap();
    assert_eq!(report["ok"], true, "problems: {}", report["problems"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn verify_rejects_a_partial_provenance_set() {
    let (store, dir) = temp_store();
    let mask = GrayImage {
        width: 6,
        height: 4,
        pixels: vec![255; 24],
        icc_profile: None,
    };
    let bytes = agenthand::image::encode_png_gray(&mask);
    let (handle, receipt, _fg) = draw_with_mask_bytes(&store, &bytes);
    let mut forged = receipt.clone();
    forged["mask_source_width"] = serde_json::json!(4284);
    let report = composite::verify_receipt_from_disk(&store, &handle, Some(&forged)).unwrap();
    assert_eq!(report["ok"], false);
    assert_eq!(report["checks"]["receipt_shape"], false);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn verify_rejects_provenance_that_contradicts_its_frames() {
    let (store, dir) = temp_store();
    let mask = GrayImage {
        width: 6,
        height: 4,
        pixels: vec![255; 24],
        icc_profile: None,
    };
    let bytes = agenthand::image::encode_png_gray(&mask);
    let (handle, receipt, _fg) = draw_with_mask_bytes(&store, &bytes);
    let mut forged = receipt.clone();
    // "none" with a SAM frame smaller than the source frame is a
    // contradiction: the method must agree with the recorded frames.
    forged["mask_source_width"] = serde_json::json!(4284);
    forged["mask_source_height"] = serde_json::json!(5712);
    forged["mask_sam_width"] = serde_json::json!(1206);
    forged["mask_sam_height"] = serde_json::json!(1608);
    forged["mask_scale_method"] = serde_json::json!("none");
    let report = composite::verify_receipt_from_disk(&store, &handle, Some(&forged)).unwrap();
    assert_eq!(report["ok"], false);
    assert_eq!(report["checks"]["receipt_shape"], false);
    let _ = std::fs::remove_dir_all(&dir);
}
