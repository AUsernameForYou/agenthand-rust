//! Byte-exact differential tests against Pillow 13.0.0: the fixture
//! PNGs in tests/fixtures/ were produced by PIL itself (Image.resize
//! BICUBIC, ImageOps.fit, Image.alpha_composite) over seeded random
//! images. The Rust ports must reproduce them pixel for pixel.

use agenthand::image::{decode_image, RgbaImage};
use agenthand::resample;
use serde_json::Value;
use std::path::PathBuf;

fn fixture_dir(kind: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(kind)
}

fn load_rgba(path: &std::path::Path) -> RgbaImage {
    let bytes = std::fs::read(path).unwrap();
    decode_image(&bytes).unwrap().into_rgba()
}

#[test]
fn resample_matches_pillow_byte_exactly() {
    let dir = fixture_dir("resample");
    let cases: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("cases.json")).unwrap()).unwrap();
    for case in cases.as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let input = load_rgba(&dir.join(format!("{name}_in.png")));
        let expected = load_rgba(&dir.join(format!("{name}_expected.png")));
        let out_w = case["out"][0].as_u64().unwrap() as u32;
        let out_h = case["out"][1].as_u64().unwrap() as u32;
        let got = match case["mode"].as_str().unwrap() {
            "resize" => resample::resize_bicubic_box(
                &input,
                out_w,
                out_h,
                [0.0, 0.0, input.width as f32, input.height as f32],
            ),
            "fit" => resample::fit(&input, out_w, out_h),
            other => panic!("unknown mode {other}"),
        };
        assert_eq!(
            (got.width, got.height),
            (expected.width, expected.height),
            "case {name}: size"
        );
        assert_eq!(got.pixels, expected.pixels, "case {name}: pixels differ");
    }
}

#[test]
fn alpha_composite_matches_pillow_byte_exactly() {
    let dir = fixture_dir("composite");
    let cases: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("cases.json")).unwrap()).unwrap();
    for name in cases.as_array().unwrap() {
        let name = name.as_str().unwrap();
        let bg = load_rgba(&dir.join(format!("{name}_bg.png")));
        let fg = load_rgba(&dir.join(format!("{name}_fg.png")));
        let expected = load_rgba(&dir.join(format!("{name}_expected.png")));
        let got = agenthand::composite::alpha_composite(&bg, &fg);
        assert_eq!(got.pixels, expected.pixels, "case {name}: pixels differ");
    }
}
