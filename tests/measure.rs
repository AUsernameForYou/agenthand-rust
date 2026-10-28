//! Estimator parity battery: the same synthetic scenes the Python
//! suite runs (tests/fixtures/measure in the Python repo, generated
//! by scripts/gen_measure_fixtures.py), with outcomes recorded from
//! the Python implementation. Assertions follow the Python
//! battery's own tolerances: the estimate must contain the truth
//! inside its stated range, the range must stay within the caps the
//! Python battery enforces, centres must sit close to Python's, and
//! refusals must match Python's error text (digits masked: the
//! embedded pixel counts may differ by a pixel or two).

use agenthand::image::decode_image;
use agenthand::measure::{estimate, load_card_profile, MeasureFrame};
use serde_json::Value;
use std::path::PathBuf;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/measure")
}

fn load_frame(path: &PathBuf) -> MeasureFrame {
    let bytes = std::fs::read(path).unwrap();
    let img = decode_image(&bytes).unwrap().into_rgba();
    MeasureFrame::from_rgba(&img)
}

fn load_mask(path: &PathBuf) -> Vec<u8> {
    let bytes = std::fs::read(path).unwrap();
    let img = decode_image(&bytes).unwrap().into_rgba();
    img.pixels.iter().step_by(4).map(|&r| r).collect()
}

/// Collapse every run of digits to a single '#', so error strings
/// compare equal when only the embedded measurement differs.
fn mask_digits(s: &str) -> String {
    let mut out = String::new();
    let mut in_digits = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push('#');
                in_digits = true;
            }
        } else {
            out.push(c);
            in_digits = false;
        }
    }
    out
}

#[test]
fn estimator_battery_matches_python() {
    let dir = fixture_dir();
    let expected: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("expected.json")).unwrap())
            .unwrap();
    let scenes = expected.as_object().unwrap();
    assert_eq!(scenes.len(), 22, "fixture set drifted");
    let mut names: Vec<&String> = scenes.keys().collect();
    names.sort();
    for name in names {
        let scene = &scenes[name];
        let card_id = scene["card_id"].as_str().unwrap();
        let profile_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(scene["profile"].as_str().unwrap());
        let card = load_card_profile(&profile_path, card_id)
            .unwrap_or_else(|e| panic!("{name}: profile load failed: {e}"));
        let frame = load_frame(&dir.join(scene["file"].as_str().unwrap()));
        let bbox = scene["bbox"].as_array().map(|b| {
            (
                b[0].as_f64().unwrap(),
                b[1].as_f64().unwrap(),
                b[2].as_f64().unwrap(),
                b[3].as_f64().unwrap(),
            )
        });
        let mask = scene["mask_file"]
            .as_str()
            .map(|f| load_mask(&dir.join(f)));
        let result = estimate(&frame, &card, card_id, bbox, mask.as_deref());
        let outcome = scene["outcome"].as_str().unwrap();
        let truth = scene["truth_mm"].as_array().unwrap();
        let (truth_w, truth_h) = (truth[0].as_f64().unwrap(), truth[1].as_f64().unwrap());
        match outcome {
            "refused" => {
                let err = result
                    .err()
                    .unwrap_or_else(|| panic!("{name}: Python refused, Rust measured"));
                let py_err = scene["error"].as_str().unwrap();
                assert_eq!(
                    mask_digits(&err),
                    mask_digits(py_err),
                    "{name}: refusal text differs\n rust: {err}\n   py: {py_err}"
                );
            }
            "ok" => {
                let est = result
                    .unwrap_or_else(|e| panic!("{name}: Python measured, Rust refused: {e}"));
                let py = &scene["result"];
                let py_ticks = py["ticks_matched"].as_u64().unwrap() as usize;
                assert!(
                    est.ticks_matched >= 18 && est.ticks_matched + 3 >= py_ticks,
                    "{name}: ticks {} vs Python {py_ticks}",
                    est.ticks_matched
                );
                if let Some(renderer_px) = scene["renderer_card_px"].as_f64() {
                    assert!(
                        (est.card_width_px as f64 - renderer_px).abs() <= 6.0,
                        "{name}: card_width_px {} vs renderer {renderer_px}",
                        est.card_width_px
                    );
                }
                // tm_bbox_path is exempt from truth-in-range:
                // Python's own bbox result overestimates the
                // rotated object (187.5 vs truth 180) and its
                // range excludes the truth too; the scene is
                // checked by its dedicated assertions below.
                if name != "tm_bbox_path" {
                    assert!(
                        est.width_range_mm[0] <= truth_w && truth_w <= est.width_range_mm[1],
                        "{name}: width range {:?} does not contain truth {truth_w}",
                        est.width_range_mm
                    );
                    assert!(
                        est.height_range_mm[0] <= truth_h && truth_h <= est.height_range_mm[1],
                        "{name}: height range {:?} does not contain truth {truth_h}",
                        est.height_range_mm
                    );
                }
                let py_range = py["range_pct"].as_f64().unwrap();
                assert!(
                    est.range_pct <= py_range + 2.5,
                    "{name}: range_pct {} vs Python {py_range}",
                    est.range_pct
                );
                let cap = if name.starts_with("tm_") { 35.0 } else { 10.0 };
                assert!(
                    est.range_pct <= cap,
                    "{name}: range_pct {} over the {cap} cap",
                    est.range_pct
                );
                let py_w = py["width_mm"].as_f64().unwrap();
                let py_h = py["height_mm"].as_f64().unwrap();
                assert!(
                    (est.width_mm - py_w).abs() <= (0.03 * truth_w).max(2.0),
                    "{name}: width {} vs Python {py_w}",
                    est.width_mm
                );
                assert!(
                    (est.height_mm - py_h).abs() <= (0.03 * truth_h).max(2.0),
                    "{name}: height {} vs Python {py_h}",
                    est.height_mm
                );
                assert!(est.display.contains('~'), "{name}: display lacks ~");
                if name == "tm_bbox_path" {
                    assert!(
                        est.width_mm >= truth_w && est.width_mm <= truth_w * 1.2,
                        "tm_bbox_path: bbox width {} vs truth {truth_w}",
                        est.width_mm
                    );
                    assert!(
                        est.warnings.iter().any(|w| w.contains("bbox")),
                        "tm_bbox_path: missing bbox warning"
                    );
                }
                if name == "ah42_far2x" {
                    assert!(
                        est.range_pct >= 8.0,
                        "ah42_far2x: caution-band range_pct {} < 8",
                        est.range_pct
                    );
                    assert!(
                        est.warnings.iter().any(|w| w.contains("small in frame")),
                        "ah42_far2x: missing small-in-frame warning"
                    );
                }
            }
            other => panic!("{name}: unknown outcome {other}"),
        }
    }
}
