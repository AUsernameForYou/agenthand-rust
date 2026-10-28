//! Ingest orientation parity against the Python build.
//!
//! Fixtures in `tests/fixtures/ingest/` were produced by
//! `scripts/gen_ingest_fixtures.py` running the Python normalizer:
//! `expected.json` records the normalized dimensions and the RGBA
//! pixel sha256 for each input. PNG is lossless, so normalized
//! pixels must match the Python build byte-exactly; JPEG is lossy
//! (Pillow and jpeg-decoder round IDCT differently), so the JPEG
//! case asserts dimensions only — the same thing the Python suite
//! asserts for it.

use agenthand::image::{decode_image, sha256_hex};
use agenthand::ingest::normalize_image_bytes;
use serde_json::Value;
use std::path::PathBuf;

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/ingest")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn expected() -> Value {
    let text = String::from_utf8(fixture("expected.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

#[test]
fn png_exif_orientation_matches_python() {
    let expected = expected();
    for name in [
        "png_exif_2.png",
        "png_exif_3.png",
        "png_exif_5.png",
        "png_exif_6.png",
        "png_exif_7.png",
        "png_exif_8.png",
        "png_plain.png",
    ] {
        let data = fixture(name);
        let normalized = normalize_image_bytes(&data)
            .unwrap_or_else(|e| panic!("{name}: normalize failed: {e}"));
        let img = decode_image(&normalized)
            .unwrap_or_else(|e| panic!("{name}: re-decode failed: {e}"))
            .into_rgba();
        let want = &expected[name];
        assert_eq!(
            img.width as u64,
            want["width"].as_u64().unwrap(),
            "{name}: width"
        );
        assert_eq!(
            img.height as u64,
            want["height"].as_u64().unwrap(),
            "{name}: height"
        );
        assert_eq!(
            sha256_hex(&img.pixels),
            want["pixel_sha256"].as_str().unwrap(),
            "{name}: normalized pixels differ from the Python build"
        );
    }
}

#[test]
fn jpeg_exif_orientation_swaps_dimensions() {
    let data = fixture("jpeg_exif_6.jpg");
    let normalized = normalize_image_bytes(&data).unwrap();
    let img = decode_image(&normalized).unwrap().into_rgba();
    // 4x2 tagged orientation 6 is stored as the framed 2x4.
    assert_eq!((img.width, img.height), (2, 4));
}
