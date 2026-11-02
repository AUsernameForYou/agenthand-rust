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
use agenthand::measure::{detect_card, estimate, load_card_profile, MeasureFrame};
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

const NOT_VERIFIED: &str = "Card not verified in this photo (";

/// Every refusal the estimator can give on a photo before measuring.
/// On real photos the contract is that the estimate is REFUSED; which
/// gate fires first is not part of it. Float and JPEG-decode
/// differences between platforms move the first failing gate: on the
/// Mac (2026-11-02, after M2 landed) the IMG_1403 preview refused at
/// "Card fit did not converge to a trustworthy homography (coverage or
/// residual gates failed)." while this box refuses at the ring gate,
/// and `fence_preview_1403_refuses` panicked on the reason. Real-photo
/// tests therefore accept any reason in this set.
const KNOWN_REFUSALS: &[&str] = &[
    NOT_VERIFIED,
    "Card fit did not converge",
    "Tick verification failed",
    "Cyan ring not found",
    "Too little ink",
    "Card too small in frame",
    "Card failed strict tick verification",
];

fn assert_known_refusal(err: &str) {
    assert!(
        KNOWN_REFUSALS.iter().any(|prefix| err.starts_with(prefix)),
        "refused, but with an unexpected reason: {err}"
    );
}

#[test]
fn fence_preview_1403_refuses() {
    let err = assert_refused(
        &root().join("tests/fixtures/measure/real/IMG_1403_preview.jpg"),
        "AH-BIG",
    );
    println!("IMG_1403 preview: {err}");
    assert_known_refusal(&err);
}

#[test]
fn jacket_preview_1435_refuses() {
    let err = assert_refused(
        &root().join("tests/fixtures/measure/real/IMG_1435_640.jpg"),
        "AH-35",
    );
    println!("IMG_1435 640: {err}");
    assert_known_refusal(&err);
}

/// Synthetic version of the same failure: a genuine bracket frame
/// (every tick cut present) whose ring is gone, with the cyan that
/// anchors the seed search sitting off the card (two decoy patches
/// either side of the ring position, so the anchor and scale seed land
/// exactly where the ring was). Before the gate this measured the
/// orange object at 21/24 ticks; tick counts alone are not proof of a
/// card, the ring is.
#[test]
fn bracket_without_its_ring_refuses() {
    let path = root().join("tests/fixtures/measure/ahbig_2x.png");
    let mut img = decode_image(&std::fs::read(&path).unwrap())
        .unwrap()
        .into_rgba();
    let (w, h) = (img.width as usize, img.height as usize);
    let is_cyan = |p: &[u8]| p[2] > 150 && p[0] < 110 && p[1] > 100 && p[1] < 220;
    let (mut n, mut sx, mut sy) = (0usize, 0.0f64, 0.0f64);
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            if is_cyan(&img.pixels[i..i + 3]) {
                n += 1;
                sx += x as f64;
                sy += y as f64;
                img.pixels[i..i + 3].copy_from_slice(&[17, 17, 17]);
            }
        }
    }
    assert!(n > 2000, "fixture ring not found ({n} px)");
    let (cx, cy) = (sx / n as f64, sy / n as f64);
    let side = ((n as f64 / 2.0).sqrt()).round() as i64;
    for dx in [-220.0, 220.0] {
        let (x0, y0) = ((cx + dx) as i64 - side / 2, cy as i64 - side / 2);
        for y in y0..y0 + side {
            for x in x0..x0 + side {
                let i = (y as usize * w + x as usize) * 4;
                img.pixels[i..i + 3].copy_from_slice(&[0, 175, 225]);
            }
        }
    }
    // ahbig_2x was rendered from scaled-card-profiles.json (see
    // expected.json); the printed-card profile's ring is 0.43x its ring.
    let profile = root().join("tests/fixtures/scaled-card-profiles.json");
    let card = load_card_profile(&profile, "AH-BIG").unwrap();
    let frame = MeasureFrame::from_rgba(&img);
    match estimate(&frame, &card, "AH-BIG", None, None) {
        Ok(est) => panic!(
            "ringless bracket accepted: {} (ticks {}, card {} px)",
            est.display, est.ticks_matched, est.card_width_px
        ),
        Err(err) => {
            println!("ringless bracket: {err}");
            assert!(err.starts_with(NOT_VERIFIED), "{err}");
            assert!(err.contains("no cyan ring"), "{err}");
        }
    }
}

/// Frames that are not in the repo (run on the Mac):
/// AGENTHAND_MEASURE_IMG_1405=<IMG_1405 640-px intake preview>. Its
/// earlier "acceptance" was a phantom fit (a 278-px "card" in a 640-px
/// yard-wide frame), so it must refuse.
fn env_case(var: &str, card_id: &str) {
    let Some(path) = std::env::var_os(var) else {
        println!("{var} not set; skipped");
        return;
    };
    let err = assert_refused(Path::new(&path), card_id);
    println!("{var}: {err}");
    assert_known_refusal(&err);
}

/// IMG_1491: Daniel's AH-BIG print lies flat on the floor beside a
/// denim jacket, turned a quarter, on an untrimmed sheet. Every earlier
/// "acceptance" of this photo (both builds) was a phantom fit on the
/// jacket front, anchored at the centroid of all cyan (the denim); the
/// card itself was never fitted. Seeded from its ring, the card must be
/// found WHERE IT LIES: every fitted frame corner on the printed sheet.
/// `sheet` is the sheet's image box (x0, y0, x1, y1), `jacket` the
/// jacket's bbox (x, y, w, h).
fn assert_1491_on_the_card(path: &Path, sheet: [f64; 4], jacket: (f64, f64, f64, f64)) {
    let profile = root().join("tests/fixtures/layout-profile.json");
    let card = load_card_profile(&profile, "AH-BIG").unwrap();
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let frame = MeasureFrame::from_rgba(&decode_image(&bytes).unwrap().into_rgba());
    let det = detect_card(&frame, &card)
        .unwrap_or_else(|e| panic!("{}: printed card refused: {e}", path.display()));
    for q in det.quad {
        assert!(
            q[0] >= sheet[0] && q[0] <= sheet[2] && q[1] >= sheet[1] && q[1] <= sheet[3],
            "fitted frame corner ({:.0}, {:.0}) is off the printed sheet {sheet:?}",
            q[0],
            q[1]
        );
    }
    assert!(det.tick_pairs.len() >= 18, "ticks {}", det.tick_pairs.len());
    let est = estimate(&frame, &card, "AH-BIG", Some(jacket), None)
        .unwrap_or_else(|e| panic!("{}: boxed estimate refused: {e}", path.display()));
    println!(
        "{}: {} (ticks {}, card {} px, rms {}, print scale assumed {})",
        path.display(),
        est.display,
        est.ticks_matched,
        est.card_width_px,
        est.reprojection_rms_px,
        est.card_print_scale_assumed
    );
}

/// The 1600 x 1200 film copy (film/v8/html/bg/jacket1491_src.jpg, byte
/// for byte: JFIF only, no EXIF; turned upright from the 4284 x 5712
/// original). Sheet at x 700-905, y 665-808; the jacket bbox is the
/// cut_out (SAM) mask box of the original, mapped into this frame.
#[test]
fn jacket_1600_1491_accepts_on_the_printed_card() {
    assert_1491_on_the_card(
        &root().join("tests/fixtures/measure/real/IMG_1491_1600.jpg"),
        [695.0, 660.0, 910.0, 815.0],
        (31.0, 55.0, 1518.0, 584.0),
    );
}

/// Full resolution: AGENTHAND_MEASURE_IMG_1491=<IMG_1491.HEIC decoded
/// upright, or P18/reshoot-2026-11-01/source.jpg> (4284 x 5712; sheet
/// at x 1389-1926, y 2491-3217; jacket bbox from cut_out's mask review
/// [2003, 110, 4089, 5531]).
#[test]
#[ignore = "needs AGENTHAND_MEASURE_IMG_1491 (full-res photo, not in the repo)"]
fn jacket_full_res_1491_accepts_on_the_printed_card() {
    let Some(path) = std::env::var_os("AGENTHAND_MEASURE_IMG_1491") else {
        println!("AGENTHAND_MEASURE_IMG_1491 not set; skipped");
        return;
    };
    assert_1491_on_the_card(
        Path::new(&path),
        [1380.0, 2480.0, 1935.0, 3225.0],
        (2003.0, 110.0, 2087.0, 5422.0),
    );
}

#[test]
#[ignore = "needs AGENTHAND_MEASURE_IMG_1405 (intake preview, not in the repo)"]
fn yard_preview_1405_refuses() {
    env_case("AGENTHAND_MEASURE_IMG_1405", "AH-BIG");
}

/// M3: the colour-segmentation fallback must not measure a region
/// that runs out of the photo. IMG_1403's "object" was the sky (a
/// region touching the top, left and right frame edges). Here a
/// saturated sky-blue band across the top of a verified-card scene
/// (bigger than the real object, so it wins the largest-contour pick)
/// must refuse instead of being measured.
#[test]
fn auto_extent_touching_frame_edge_refuses() {
    let path = root().join("tests/fixtures/measure/ahbig_2x.png");
    let mut img = decode_image(&std::fs::read(&path).unwrap())
        .unwrap()
        .into_rgba();
    let w = img.width as usize;
    for y in 0..200 {
        for x in 0..w {
            let i = (y * w + x) * 4;
            img.pixels[i..i + 3].copy_from_slice(&[90, 150, 220]);
        }
    }
    // ahbig_2x was rendered from scaled-card-profiles.json (see
    // expected.json); the printed-card profile's ring is 0.43x its ring.
    let profile = root().join("tests/fixtures/scaled-card-profiles.json");
    let card = load_card_profile(&profile, "AH-BIG").unwrap();
    let frame = MeasureFrame::from_rgba(&img);
    match estimate(&frame, &card, "AH-BIG", None, None) {
        Ok(est) => panic!(
            "sky band measured as the object: {} (ticks {})",
            est.display, est.ticks_matched
        ),
        Err(err) => {
            println!("sky band: {err}");
            assert!(
                err.starts_with("Auto extent reached the frame edge"),
                "{err}"
            );
        }
    }
}
