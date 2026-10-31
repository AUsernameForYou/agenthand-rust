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

fn png_chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut crc = flate2::Crc::new();
    crc.update(kind);
    crc.update(payload);
    let mut out = Vec::new();
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out.extend_from_slice(&crc.sum().to_be_bytes());
    out
}

#[test]
fn giant_ihdr_png_is_refused_before_allocation() {
    // Regression (2026-10-31 adversarial pass): a 69-byte PNG whose
    // IHDR claims 30000x30000 RGBA aborted the server on the 3.6 GB
    // frame-buffer allocation, before the ingest pixel cap (which
    // ran post-decode) could apply. Must be a clean error now.
    use std::io::Write;
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&30000u32.to_be_bytes());
    ihdr.extend_from_slice(&30000u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    png.extend(png_chunk(b"IHDR", &ihdr));
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&[0u8; 64]).unwrap();
    let idat = encoder.finish().unwrap();
    png.extend(png_chunk(b"IDAT", &idat));
    png.extend(png_chunk(b"IEND", &[]));
    let err = match decode_image(&png) {
        Ok(_) => panic!("giant PNG must be refused"),
        Err(e) => e,
    };
    assert!(err.contains("ingest cap"), "{err}");
}

#[test]
fn giant_sof_jpeg_is_refused_before_allocation() {
    // Regression (2026-10-31 adversarial pass): a valid JPEG with
    // its SOF dimensions patched to 60000x60000 aborted the server
    // inside the decoder's pixel-buffer allocation. The header
    // pre-check must refuse it with a clean error.
    let mut data = fixture("jpeg_exif_6.jpg");
    let mut i = 2usize;
    let patched = loop {
        assert_eq!(data[i], 0xFF);
        let marker = data[i + 1];
        if matches!(marker, 0xC0 | 0xC1 | 0xC2) {
            data[i + 5..i + 7].copy_from_slice(&60000u16.to_be_bytes());
            data[i + 7..i + 9].copy_from_slice(&60000u16.to_be_bytes());
            break true;
        }
        if marker == 0xDA {
            break false;
        }
        if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        i += 2 + len;
    };
    assert!(patched, "fixture must contain a SOF marker");
    let err = match decode_image(&data) {
        Ok(_) => panic!("giant JPEG must be refused"),
        Err(e) => e,
    };
    assert!(err.contains("ingest cap"), "{err}");
}
