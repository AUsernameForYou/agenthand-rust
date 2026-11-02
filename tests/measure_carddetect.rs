//! Card detection on the card that was actually printed (CARDDETECT,
//! 2026-11-02): strict re-verification after the size gates, the
//! printed ring geometry, ring-shaped seeding, and the print-scale
//! assumption every result carries.

use agenthand::image::decode_image;
use agenthand::measure::{detect_card, estimate, load_card_profile, MeasureFrame};
use serde_json::Value;
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn project(h: &[f64; 9], p: [f64; 2]) -> [f64; 2] {
    let w = h[6] * p[0] + h[7] * p[1] + h[8];
    [
        (h[0] * p[0] + h[1] * p[1] + h[2]) / w,
        (h[3] * p[0] + h[4] * p[1] + h[5]) / w,
    ]
}

/// Paint a card-space rectangle (centre, half extents along x and y
/// in mm) into the image under H, densely enough to leave no holes.
fn paint_mm_rect(
    img: &mut agenthand::image::RgbaImage,
    h: &[f64; 9],
    centre: [f64; 2],
    half: [f64; 2],
    rgb: [u8; 3],
) {
    let (w, ht) = (img.width as i64, img.height as i64);
    let step = 0.05;
    let nx = (2.0 * half[0] / step).ceil() as i64;
    let ny = (2.0 * half[1] / step).ceil() as i64;
    for i in 0..=nx {
        for j in 0..=ny {
            let q = [
                centre[0] - half[0] + i as f64 * step,
                centre[1] - half[1] + j as f64 * step,
            ];
            let p = project(h, q);
            let (x, y) = (p[0].round() as i64, p[1].round() as i64);
            if x >= 0 && y >= 0 && x < w && y < ht {
                let k = ((y * w + x) * 4) as usize;
                img.pixels[k..k + 3].copy_from_slice(&rgb);
            }
        }
    }
}

/// The fit pipeline adopts a strict re-observation only when it
/// confirms (>= 10 strict ticks); otherwise it keeps the LOOSE pairs
/// and grades its gates on them. A card whose tick cuts only the loose
/// matcher accepts therefore measured on loose evidence alone. Here
/// every tick cut of a genuine card is widened to 13% of its arm:
/// still at the right place (the loose matcher takes runs up to 30% of
/// the arm), but no longer a tick-sized cut (strict takes at most
/// 12%). Strict re-verification at the accepted fit finds nothing, so
/// the estimate must refuse after the size gates.
#[test]
fn loose_only_ticks_refuse_under_strict_recheck() {
    let path = root().join("tests/fixtures/measure/ahbig_2x.png");
    let mut img = decode_image(&std::fs::read(&path).unwrap())
        .unwrap()
        .into_rgba();
    let profile_path = root().join("tests/fixtures/scaled-card-profiles.json");
    let card = load_card_profile(&profile_path, "AH-BIG").unwrap();
    let det = detect_card(&MeasureFrame::from_rgba(&img), &card).unwrap();

    let profile: Value =
        serde_json::from_str(&std::fs::read_to_string(&profile_path).unwrap()).unwrap();
    let c = &profile["cards"]["AH-BIG"];
    let arm_len = c["bracket_frame"]["arm_centreline_length_mm"]
        .as_f64()
        .unwrap();
    let stroke = c["bracket_frame"]["stroke_width_mm"].as_f64().unwrap();
    for tick in c["ticks"].as_array().unwrap() {
        let centre = [
            tick["centre_mm"][0].as_f64().unwrap(),
            tick["centre_mm"][1].as_f64().unwrap(),
        ];
        let along = 0.5 * 0.13 * arm_len;
        let across = 0.5 * stroke + 1.0;
        let horizontal = tick["arm"].as_str().unwrap().ends_with("-H");
        let half = if horizontal {
            [along, across]
        } else {
            [across, along]
        };
        paint_mm_rect(&mut img, &det.H, centre, half, [255, 255, 255]);
    }

    let frame = MeasureFrame::from_rgba(&img);
    match estimate(&frame, &card, "AH-BIG", None, None) {
        Ok(est) => panic!(
            "loose-only ticks measured: {} (ticks {}, card {} px, rms {})",
            est.display, est.ticks_matched, est.card_width_px, est.reprojection_rms_px
        ),
        Err(err) => {
            println!("widened cuts: {err}");
            assert!(
                err.starts_with("Card failed strict tick verification"),
                "{err}"
            );
        }
    }
}
