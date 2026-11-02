//! Real-photo false-accept regressions for the scale-card estimator.
//!
//! The synthetic battery (tests/measure.rs) only ever shows the
//! detector a clean card on a flat background. On Daniel's real
//! photos it accepted phantom cards:
//!
//! - `IMG_1403_preview.jpg` (the 640x480 intake preview of the fence
//!   shot, AH-BIG pinned to the fence; the card is ~25 px across).
//!   Accepted "~31.2 cm (20.9–41.5 cm) wide × ~127.2 cm (114.0–140.4
//!   cm) high", 12/24 ticks, card 146 px, RMS 2.21. The fitted quad
//!   sits on bare fence boards right of the card and the "object" is
//!   the sky.
//! - `IMG_1435_640.jpg` (640-wide render of IMG_1435.HEIC, the AH-35
//!   card on the floor below a denim jacket; the card is ~55 px
//!   across). Accepted "~132.0 cm (-88.1–352.2 cm)" with 21/24 ticks:
//!   the quad sits on the jacket's button placket.
//!
//! Root cause (M1): the seed anchor is the centroid of every
//! cyan-hued pixel in the frame (teal play equipment and sky in 1403,
//! denim in 1435), the seeded fit is verified only by bright runs at
//! tick spacing along the arm lines (periodic texture — fence boards,
//! denim seams — supplies those), the RMS gate measures the fit's
//! consistency with its own matches, and the pixel floor is applied to
//! the fitted quad, so a phantom fit certifies its own size.
//!
//! Provenance: IMG_1403_preview.jpg is byte-for-byte the preview the
//! 2026-11-01 estimator sweep ran (it reproduces that run's JSON
//! exactly; no EXIF). IMG_1435_640.jpg is IMG_1435.HEIC decoded by
//! examples/heic_png, resized to 640 px wide (Lanczos) and saved as a
//! JPEG (quality 85, no EXIF).

use agenthand::image::decode_image;
use agenthand::measure::{estimate, load_card_profile, MeasureFrame};
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run(path: &Path, card_id: &str) -> Result<agenthand::measure::Estimate, String> {
    let profile = root().join("tests/fixtures/layout-profile.json");
    let card = load_card_profile(&profile, card_id).unwrap();
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let img = decode_image(&bytes).unwrap().into_rgba();
    let frame = MeasureFrame::from_rgba(&img);
    estimate(&frame, &card, card_id, None, None)
}

fn assert_refused(path: &Path, card_id: &str) -> String {
    match run(path, card_id) {
        Ok(est) => panic!(
            "{}: accepted a phantom card: {} (ticks {}, card {} px, rms {})",
            path.display(),
            est.display,
            est.ticks_matched,
            est.card_width_px,
            est.reprojection_rms_px
        ),
        Err(e) => e,
    }
}

#[test]
#[ignore = "M1 reproduction: accepted a phantom fit on fence boards until the verification gate (M2)"]
fn fence_preview_1403_refuses() {
    let err = assert_refused(
        &root().join("tests/fixtures/measure/real/IMG_1403_preview.jpg"),
        "AH-BIG",
    );
    println!("IMG_1403 preview: {err}");
}

#[test]
#[ignore = "M1 reproduction: accepted a phantom fit on denim until the verification gate (M2)"]
fn jacket_preview_1435_refuses() {
    let err = assert_refused(
        &root().join("tests/fixtures/measure/real/IMG_1435_640.jpg"),
        "AH-35",
    );
    println!("IMG_1435 640: {err}");
}
