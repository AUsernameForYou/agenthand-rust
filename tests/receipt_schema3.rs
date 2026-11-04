//! Cross-build receipt verification (Item 11): the Rust verifier
//! grades the Python build's schema-3 paper-fill receipts (python
//! `main`, ff72fcb) and schema-4 receipts (assumed: schema 3 plus
//! additive fields), and keeps refusing unknown markers.
//!
//! The schema-3 receipt here is built the way the Python
//! `draw_product` builds one: a source photo, a foreground cut from
//! it, a background plate, a paper reconstruction layer that fills
//! only where the foreground is fully transparent, a region map
//! coding each paper pixel's class, and a composite of plate, then
//! paper, then product. Every field and the `reconstruction` block
//! carry the Python names, types and arithmetic.

// The receipt literal below is one large json! invocation.
#![recursion_limit = "256"]

use agenthand::assets::{AssetStore, KIND_COMPOSITE, KIND_FOREGROUND, KIND_PLATE, KIND_SOURCE};
use agenthand::composite::{self, alpha_composite, measure};
use agenthand::image::{GrayImage, RgbaImage};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

const W: u32 = 8;
const H: u32 = 6;

fn temp_store() -> AssetStore {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "agenthand-rust-schema3-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    AssetStore::new(&dir)
}

fn build(f: impl Fn(u32, u32) -> [u8; 4]) -> RgbaImage {
    let mut pixels = vec![0u8; (W * H * 4) as usize];
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            pixels[i..i + 4].copy_from_slice(&f(x, y));
        }
    }
    RgbaImage::new(W, H, pixels)
}

fn source_px(x: u32, y: u32) -> [u8; 4] {
    [(30 + x * 20) as u8, (40 + y * 25) as u8, 90, 255]
}

/// Paper sits in the hole x 5..=6, y 2..=3 (where the cut is empty).
fn is_paper(x: u32, y: u32) -> bool {
    (5..=6).contains(&x) && (2..=3).contains(&y)
}

struct Draw {
    store: AssetStore,
    composite: String,
    receipt: Value,
}

/// Build a schema-3 draw as the Python `draw_product` would. `paint`
/// optionally alters the delivered composite after compositing (a
/// pixel painted outside the three stored layers); `paper_on_product`
/// moves one paper pixel under the foreground's edge band.
fn schema3_draw(paint: Option<(u32, u32, [u8; 4])>, paper_on_product: bool) -> Draw {
    let store = temp_store();
    let source = build(source_px);
    let source_handle = store.put_image(&source, KIND_SOURCE, None).unwrap();
    let source_id = AssetStore::parse_handle(&source_handle).unwrap();
    // Product: columns 0..=2 opaque (exact source pixels), column 3
    // an edge band, the rest transparent.
    let fg = build(|x, y| match x {
        0..=2 => source_px(x, y),
        3 => [10, 10, 10, 128],
        _ => [0, 0, 0, 0],
    });
    let fg_handle = store.put_image(&fg, KIND_FOREGROUND, Some(&source_id)).unwrap();
    let fg_id = AssetStore::parse_handle(&fg_handle).unwrap();
    let plate = build(|_, _| [200, 190, 170, 255]);
    let plate_handle = store.put_image(&plate, KIND_PLATE, None).unwrap();
    let layer = build(|x, y| {
        if is_paper(x, y) || (paper_on_product && x == 3 && y == 0) {
            [240, 235, 220, 255]
        } else {
            [0, 0, 0, 0]
        }
    });
    let layer_handle = store.put_image(&layer, "reconstruction", Some(&fg_id)).unwrap();
    let mut map_pixels = vec![0u8; (W * H) as usize];
    for y in 0..H {
        for x in 0..W {
            if is_paper(x, y) || (paper_on_product && x == 3 && y == 0) {
                map_pixels[(y * W + x) as usize] = 1; // "hole"
            }
        }
    }
    let map = GrayImage {
        width: W,
        height: H,
        pixels: map_pixels,
        icc_profile: None,
    };
    let map_handle = store
        .put_gray_image(&map, "reconstruction_region_map", Some(&fg_id))
        .unwrap();

    // Plate, then paper, then product (Python composite_images).
    let mut out = alpha_composite(&alpha_composite(&plate, &layer), &fg);
    out.icc_profile = fg.icc_profile.clone();
    if let Some((x, y, px)) = paint {
        let i = ((y * W + x) * 4) as usize;
        out.pixels[i..i + 4].copy_from_slice(&px);
    }
    let composite_handle = store.put_image(&out, KIND_COMPOSITE, Some(&fg_id)).unwrap();

    let source_record = store.get_record(&source_handle).unwrap();
    let fg_record = store.get_record(&fg_handle).unwrap();
    let plate_record = store.get_record(&plate_handle).unwrap();
    let layer_record = store.get_record(&layer_handle).unwrap();
    let map_record = store.get_record(&map_handle).unwrap();
    let composite_record = store.get_record(&composite_handle).unwrap();
    let counts = measure(
        &store.read_image(&fg_handle).unwrap(),
        &store.read_image(&composite_handle).unwrap(),
    );
    let paper_pixels: u64 = if paper_on_product { 5 } else { 4 };
    let recon_total = paper_pixels + counts.opaque_pixels;
    let fraction = agenthand::round6(paper_pixels as f64 / recon_total as f64);

    let receipt = json!({
        "receipt_schema": 3,
        "source_asset": source_handle,
        "source_sha256": source_record.sha256,
        "source_pixel_sha256": source_record.pixel_sha256,
        "foreground_asset": fg_handle,
        "foreground_sha256": fg_record.sha256,
        "foreground_pixel_sha256": fg_record.pixel_sha256,
        "mask_asset": null,
        "mask_sha256": fg_record.sha256,
        "mask_pixel_sha256": fg_record.pixel_sha256,
        "mask_source": "foreground_alpha",
        "changed_opaque_pixels": counts.changed_opaque_pixels,
        "opaque_pixels": counts.opaque_pixels,
        "edge_band_pixels": counts.edge_band_pixels,
        "transparent_pixels": counts.transparent_pixels,
        "mask_pixels_total": counts.mask_pixels_total,
        "opaque_fraction": counts.opaque_fraction,
        "edge_band_max_width_px": counts.edge_band_max_width_px,
        "opaque_alpha_threshold": 255,
        "changed_predicate": "a pixel counts as changed when ...",
        "pixel_tolerance": 0,
        "count_sample": "decoded_delivered_file",
        "output_asset": composite_handle,
        "output_sha256": composite_record.sha256,
        "output_pixel_sha256": composite_record.pixel_sha256,
        "output_format": "PNG",
        "color_space": "unspecified",
        "color_profile_source_sha256": source_record.color_profile_sha256,
        "color_profile_foreground_sha256": fg_record.color_profile_sha256,
        "color_profile_output_sha256": composite_record.color_profile_sha256,
        "color_profile_preserved": source_record.color_profile_sha256
            == composite_record.color_profile_sha256,
        "transformations": [],
        "product_layer_scaled": false,
        "plate_source": "generated",
        "plate_asset": plate_handle,
        "plate_sha256": plate_record.sha256,
        "plate_pixel_sha256": plate_record.pixel_sha256,
        "plate_kind": "scene",
        "backdrop": null,
        "claim": "The stored extracted foreground reached the composite unchanged ...",
        "reconstruction": {
            "status": "applied",
            "renderer": "paperfill-v1",
            "layer_asset": layer_handle,
            "layer_sha256": layer_record.sha256,
            "layer_pixel_sha256": layer_record.pixel_sha256,
            "region_map_asset": map_handle,
            "region_map_sha256": map_record.sha256,
            "region_map_pixel_sha256": map_record.pixel_sha256,
            "reconstruction_pixels": paper_pixels,
            "reconstruction_overlap_product_pixels": 0,
            "reconstruction_fraction": fraction,
            "guard_ring_px": 2,
            "motif": {"diameter_px": 72, "opacity": 0.35, "color": "#d8cfc0"},
            "regions": [{
                "id": 1,
                "class": "hole",
                "pixels": paper_pixels,
                // Half-open [x0, y0, x1, y1], as paperfill reports it.
                "bbox": if paper_on_product { json!([3, 0, 7, 4]) } else { json!([5, 2, 7, 4]) },
                "provenance": {"rule": "enclosed_transparent_gap"},
            }],
            "refusals": [],
            "unfilled_gap_pixels": 0,
        },
    });
    store.save_receipt(&composite_handle, &receipt).unwrap();
    Draw {
        store,
        composite: composite_handle,
        receipt,
    }
}

fn verify(draw: &Draw, receipt: &Value) -> Value {
    composite::verify_receipt_from_disk(&draw.store, &draw.composite, Some(receipt)).unwrap()
}

const SCHEMA3_CHECKS: [&str; 9] = [
    "reconstruction_layer_file_integrity",
    "reconstruction_region_map_file_integrity",
    "reconstruction_hash_matches_receipt",
    "region_map_hash_matches_receipt",
    "foreground_matches_source",
    "recount_reconstruction_pixels",
    "recount_overlap_zero",
    "region_map_partition",
    "layer_reproduction",
];

#[test]
fn python_schema3_receipt_verifies_with_the_reconstruction_checks() {
    let draw = schema3_draw(None, false);
    let report = verify(&draw, &draw.receipt);
    assert_eq!(report["ok"], json!(true), "problems: {}", report["problems"]);
    for check in SCHEMA3_CHECKS {
        assert_eq!(report["checks"][check], json!(true), "check {check}: {report}");
    }
    // The shared core ran too.
    assert_eq!(report["checks"]["zero_changed_pixels"], json!(true));
    assert_eq!(report["recomputed"]["reconstruction_pixels"], json!(4));
    // The persisted copy verifies on its own.
    let from_disk =
        composite::verify_receipt_from_disk(&draw.store, &draw.composite, None).unwrap();
    assert_eq!(from_disk["ok"], json!(true), "{}", from_disk["problems"]);
}

#[test]
fn schema4_is_graded_as_schema3_plus_additive_fields() {
    // Decision 150: a schema-4 receipt must carry its quality
    // blocks; with them present, v4 grades as schema 3 plus
    // tolerated additive fields, exactly as before.
    let draw = schema3_draw(None, false);
    let mut receipt = draw.receipt.clone();
    receipt["receipt_schema"] = json!(4);
    receipt["cut_quality"] = json!({"grade": "PASS"});
    receipt["plate_quality"] = json!({"grade": "PASS"});
    receipt["a_future_additive_field"] = json!({"anything": [1, 2, 3]});
    receipt["reconstruction"]["another_additive_key"] = json!("tolerated");
    let report = verify(&draw, &receipt);
    assert_eq!(report["ok"], json!(true), "problems: {}", report["problems"]);
    assert_eq!(report["checks"]["layer_reproduction"], json!(true));
}

#[test]
fn schema4_without_quality_blocks_fails_closed() {
    // Decision 150: the same v4 stamp stripped of either quality
    // block is refused at the shape door with a missing-field
    // problem, matching the Python validator; it is not graded
    // under the v3-era policy.
    let draw = schema3_draw(None, false);
    for strip in ["cut_quality", "plate_quality"] {
        let mut receipt = draw.receipt.clone();
        receipt["receipt_schema"] = json!(4);
        receipt["cut_quality"] = json!({"grade": "PASS"});
        receipt["plate_quality"] = json!({"grade": "PASS"});
        receipt.as_object_mut().unwrap().remove(strip);
        let report = verify(&draw, &receipt);
        assert_eq!(report["ok"], json!(false), "stripped {strip}: {report}");
        assert_eq!(report["checks"]["receipt_shape"], json!(false));
        assert!(
            report["problems"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p.as_str().unwrap_or("").contains(&format!("missing field: {strip}"))),
            "stripped {strip}: problems: {}",
            report["problems"]
        );
    }
}

#[test]
fn recomposition_catches_pixels_outside_the_three_layers() {
    // One plate pixel (no paper, no product there) repainted after
    // compositing: every count still reproduces, only the whole-
    // delivery rebuild can see it.
    let draw = schema3_draw(Some((7, 5, [255, 0, 255, 255])), false);
    let report = verify(&draw, &draw.receipt);
    assert_eq!(report["ok"], json!(false));
    assert_eq!(report["checks"]["layer_reproduction"], json!(false), "{report}");
    assert_eq!(report["checks"]["zero_changed_pixels"], json!(true));
    assert_eq!(report["checks"]["recount_reconstruction_pixels"], json!(true));
}

#[test]
fn paper_under_the_product_edge_fails_the_overlap_recount() {
    // The receipt claims 0 overlap (shape passes); the stored layer
    // puts one paper pixel under the edge band.
    let draw = schema3_draw(None, true);
    let report = verify(&draw, &draw.receipt);
    assert_eq!(report["ok"], json!(false));
    assert_eq!(report["checks"]["recount_overlap_zero"], json!(false), "{report}");
    assert_eq!(report["checks"]["layer_reproduction"], json!(false));
}

#[test]
fn schema3_shape_and_marker_failures_are_refused() {
    let draw = schema3_draw(None, false);
    let refused = |receipt: &Value, needle: &str| {
        let report = verify(&draw, receipt);
        assert_eq!(report["ok"], json!(false), "{needle}");
        assert_eq!(report["checks"]["receipt_shape"], json!(false), "{needle}");
        let problems = report["problems"].to_string();
        assert!(problems.contains(needle), "wanted {needle:?} in {problems}");
    };
    // Unknown markers stay refused.
    for bad in [json!(5), json!(1), json!(0), json!(3.0), json!("3")] {
        let mut r = draw.receipt.clone();
        r["receipt_schema"] = bad;
        refused(&r, "unsupported receipt_schema");
    }
    // Schema 3 without its required block.
    let mut r = draw.receipt.clone();
    r.as_object_mut().unwrap().remove("reconstruction");
    refused(&r, "missing field: reconstruction");
    // Paper disclosed under an older marker.
    let mut r = draw.receipt.clone();
    r["receipt_schema"] = json!(2);
    refused(&r, "not marked schema 3");
    // A malformed block.
    let mut r = draw.receipt.clone();
    r["reconstruction"]["status"] = json!("painted");
    refused(&r, "field reconstruction has the wrong type or shape");
    // The block's own arithmetic.
    let mut r = draw.receipt.clone();
    r["reconstruction"]["reconstruction_overlap_product_pixels"] = json!(1);
    refused(&r, "reconstruction_overlap_product_pixels must be 0");
    let mut r = draw.receipt.clone();
    r["reconstruction"]["regions"][0]["pixels"] = json!(3);
    refused(&r, "regions do not sum");
    let mut r = draw.receipt.clone();
    r["reconstruction"]["reconstruction_fraction"] = json!(0.5);
    refused(&r, "reconstruction_fraction disagrees");
}
