//! Review aaa44dc defect 7 (perf, generated mode): a model-generated
//! plate is cover-fitted to the product's frame once per draw. The
//! receipt's `plate_fitted_pixel_sha256` hashes that same fitted plate
//! instead of running a second fit (about 1.3 s at 24 MP), and its value
//! is unchanged: the pixel hash of `resample::fit(plate)`.
//!
//! One test in this binary: the resample counter is process-wide.

use agenthand::assets::{AssetStore, KIND_FOREGROUND, KIND_MASK, KIND_PLATE, KIND_SOURCE};
use agenthand::composite::{self, plate_fit_resamples};
use agenthand::image::{GrayImage, RgbaImage};
use agenthand::resample;
use agenthand::segmentation::extract_foreground;
use serde_json::json;

const W: u32 = 320;
const H: u32 = 240;

fn build(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> RgbaImage {
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            px.extend_from_slice(&f(x, y));
        }
    }
    RgbaImage::new(w, h, px)
}

#[test]
fn generated_draw_fits_the_plate_once_and_pins_the_same_hash() {
    let dir = std::env::temp_dir().join(format!("agenthand-fitonce-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = AssetStore::new(&dir);
    let on = |x: u32, y: u32| (100..220).contains(&x) && (60..200).contains(&y);
    let source = build(W, H, |x, y| {
        if on(x, y) {
            [30, 32, 36, 255]
        } else {
            [200, (150 + x % 40) as u8, (120 + y % 50) as u8, 255]
        }
    });
    let mask = GrayImage {
        width: W,
        height: H,
        pixels: (0..H)
            .flat_map(|y| (0..W).map(move |x| if on(x, y) { 255 } else { 0 }))
            .collect(),
        icc_profile: None,
    };
    let src = store.put_image(&source, KIND_SOURCE, None).unwrap();
    let src_id = AssetStore::parse_handle(&src).unwrap();
    let mask_handle = store
        .put_gray_image(&mask, KIND_MASK, Some(&src_id))
        .unwrap();
    let mask_id = AssetStore::parse_handle(&mask_handle).unwrap();
    let fg = extract_foreground(&source, &mask);
    let fg_handle = store
        .put_image(&fg, KIND_FOREGROUND, Some(&mask_id))
        .unwrap();
    // A 3:2 generated plate the draw must cover-fit to 4:3.
    let generated = build(480, 320, |x, y| {
        [(100 + x % 50) as u8, 120, (90 + y % 40) as u8, 255]
    });
    let handle = store.put_image(&generated, KIND_PLATE, None).unwrap();

    let before = plate_fit_resamples();
    let (_, receipt) = composite::draw_product_with_plate_kind(
        &store,
        &fg_handle,
        "driveway",
        Some(&handle),
        None,
    )
    .unwrap();
    let fits = plate_fit_resamples() - before;

    let fitted = resample::fit(&generated, W, H);
    assert_eq!(
        receipt["plate_fitted_pixel_sha256"],
        json!(fitted.pixel_sha256()),
        "the pinned hash is the fitted plate's"
    );
    assert_ne!(
        receipt["plate_fitted_pixel_sha256"],
        receipt["plate_pixel_sha256"]
    );
    assert_eq!(
        receipt["transformations"],
        json!([format!("plate_resized_and_center_cropped_to_{W}x{H}")])
    );
    assert_eq!(receipt["changed_opaque_pixels"], json!(0));
    assert_eq!(fits, 1, "a generated draw resampled its plate {fits} times");
    let _ = std::fs::remove_dir_all(dir);
}
