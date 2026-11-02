//! The metadata-free public copy is an ALLOWLIST, not a denylist
//! (fix pass F1 / F2). A location-carrying photo's public copy must
//! drop every metadata carrier the copy does not explicitly need:
//! for JPEG, every APPn segment except JFIF (APP0), ICC (APP2) and
//! Adobe (APP14), every COM segment, and every APPn/COM between the
//! scans of a progressive file; for PNG, every chunk outside the
//! image/colour set (C2PA caBX, eXIf, the text chunks, tIME and any
//! private chunk). Pixels must come through identical.

use agenthand::provenance::metadata_free_public_copy;
use base64::Engine;

/// One length-prefixed JPEG marker segment.
fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0xFF, marker];
    out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// True when the two-byte marker sequence FF `marker` occurs
/// anywhere in `data`.
fn has_marker(data: &[u8], marker: u8) -> bool {
    data.windows(2).any(|w| w[0] == 0xFF && w[1] == marker)
}

fn contains(data: &[u8], needle: &[u8]) -> bool {
    data.windows(needle.len()).any(|w| w == needle)
}

fn count(data: &[u8], needle: &[u8]) -> usize {
    data.windows(needle.len()).filter(|w| *w == needle).count()
}

/// Decode with jpeg_decoder: (width, height, pixel bytes).
fn jpeg_pixels(data: &[u8]) -> (u16, u16, Vec<u8>) {
    let mut decoder = jpeg_decoder::Decoder::new(data);
    let pixels = decoder.decode().expect("JPEG decodes");
    let info = decoder.info().expect("JPEG info");
    (info.width, info.height, pixels)
}

/// A real baseline JPEG from the image crate, guaranteed to open
/// with a JFIF APP0 (spliced in when the encoder omits it).
fn baseline_jpeg() -> Vec<u8> {
    let (w, h) = (16u32, 16u32);
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            rgb.extend_from_slice(&[(x * 16) as u8, (y * 16) as u8, ((x + y) * 8) as u8]);
        }
    }
    let mut buf = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 90)
        .encode(&rgb, w, h, image::ExtendedColorType::Rgb8)
        .expect("baseline JPEG encodes");
    if !contains(&buf, b"JFIF\0") {
        let mut with_jfif = buf[..2].to_vec();
        with_jfif.extend_from_slice(&segment(
            0xE0,
            b"JFIF\0\x01\x01\x00\x00\x01\x00\x01\x00\x00",
        ));
        with_jfif.extend_from_slice(&buf[2..]);
        buf = with_jfif;
    }
    buf
}

/// Offset just past the leading APP0 JFIF segment (or SOI).
fn after_app0(data: &[u8]) -> usize {
    if data[2] == 0xFF && data[3] == 0xE0 {
        4 + u16::from_be_bytes([data[4], data[5]]) as usize
    } else {
        2
    }
}

/// The metadata segments every test splices in: EXIF (APP1), C2PA
/// JUMBF (APP11), Ducky (APP12), Photoshop IRB (APP13), a comment.
fn metadata_segments() -> Vec<u8> {
    let mut out = Vec::new();
    let mut exif = b"Exif\0\0".to_vec();
    exif.extend_from_slice(b"II*\0\x08\0\0\0\0\0\0\0\0\0");
    out.extend_from_slice(&segment(0xE1, &exif));
    out.extend_from_slice(&segment(0xEB, b"JP\0\0\0\x01\0\0\0\x20jumbc2pa.manifest"));
    out.extend_from_slice(&segment(0xEC, b"Ducky\0\x01\0\x04\0\0\0\x50"));
    out.extend_from_slice(&segment(0xED, b"Photoshop 3.0\08BIM\x04\x04\0\0\0\0\0\0"));
    out.extend_from_slice(&segment(0xFE, b"a free-text comment"));
    out
}

fn icc_segment() -> Vec<u8> {
    let mut icc = b"ICC_PROFILE\0\x01\x01".to_vec();
    icc.extend_from_slice(&[0x42; 48]);
    segment(0xE2, &icc)
}

fn assert_no_metadata_markers(copy: &[u8]) {
    for marker in [0xE1u8, 0xEB, 0xEC, 0xED, 0xFE] {
        assert!(
            !has_marker(copy, marker),
            "marker FF{marker:02X} survived into the public copy"
        );
    }
    for needle in [
        &b"Exif\0\0"[..],
        b"jumb",
        b"Ducky",
        b"Photoshop 3.0",
        b"a free-text comment",
    ] {
        assert!(
            !contains(copy, needle),
            "metadata payload {:?} survived into the public copy",
            String::from_utf8_lossy(needle)
        );
    }
}

#[test]
fn jpeg_public_copy_keeps_only_allowlisted_segments() {
    let clean = baseline_jpeg();
    let at = after_app0(&clean);
    let mut dirty = clean[..at].to_vec();
    dirty.extend_from_slice(&icc_segment());
    dirty.extend_from_slice(&metadata_segments());
    dirty.extend_from_slice(&clean[at..]);
    dirty.extend_from_slice(b"TRAILING-BYTES");

    let (copy, kind) = metadata_free_public_copy(&dirty).unwrap();
    assert_eq!(kind, "jpeg");
    assert_no_metadata_markers(&copy);
    assert!(contains(&copy, b"JFIF\0"), "JFIF APP0 must be kept");
    assert!(contains(&copy, b"ICC_PROFILE\0"), "ICC APP2 must be kept");
    assert_eq!(count(&copy, &[0xFF, 0xDA]), count(&clean, &[0xFF, 0xDA]));
    assert!(copy.ends_with(&[0xFF, 0xD9]), "the copy ends at EOI");
    assert!(!contains(&copy, b"TRAILING-BYTES"));
    // Same pixels as the clean original.
    assert_eq!(jpeg_pixels(&copy), jpeg_pixels(&clean));
}

/// A real progressive JPEG (16x16, 10 scans, a DHT before most
/// scans), made once with Pillow (quality 90, progressive=True,
/// no EXIF, no ICC) and inlined so the test needs no encoder.
const PROGRESSIVE_JPEG_B64: &str = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAMCAgMCAgMDAwMEAwMEBQgFBQQEBQoHBwYIDAoMDAsKCwsNDhIQDQ4RDgsLEBYQERMUFRUVDA8XGBYUGBIUFRT/2wBDAQMEBAUEBQkFBQkUDQsNFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBT/wgARCAAQABADASIAAhEBAxEB/8QAFQABAQAAAAAAAAAAAAAAAAAABgf/xAAUAQEAAAAAAAAAAAAAAAAAAAAA/9oADAMBAAIQAxAAAAGPvGTw/8QAFhAAAwAAAAAAAAAAAAAAAAAAAAQF/9oACAEBAAEFAkpAlIEpAlIP/8QAFxEBAAMAAAAAAAAAAAAAAAAABgAhMf/aAAgBAwEBPwE8hy5//8QAFREBAQAAAAAAAAAAAAAAAAAAAwD/2gAIAQIBAT8BFr//xAAWEAADAAAAAAAAAAAAAAAAAAAAASH/2gAIAQEABj8CUFBQUP/EABQQAQAAAAAAAAAAAAAAAAAAACD/2gAIAQEAAT8hCqq//9oADAMBAAIAAwAAABBT/8QAFBEBAAAAAAAAAAAAAAAAAAAAAP/aAAgBAwEBPxB//8QAFhEAAwAAAAAAAAAAAAAAAAAAACEx/9oACAECAQE/EIM//8QAFRABAQAAAAAAAAAAAAAAAAAAAPH/2gAIAQEAAT8QgoKCgv/Z";

#[test]
fn progressive_jpeg_public_copy_drops_metadata_between_scans() {
    let clean = base64::engine::general_purpose::STANDARD
        .decode(PROGRESSIVE_JPEG_B64)
        .unwrap();
    let scans = count(&clean, &[0xFF, 0xDA]);
    assert_eq!(scans, 10, "fixture is the 10-scan progressive file");
    // Metadata before the first scan, and again between scans:
    // right before the 2nd and the 5th SOS headers.
    let sos_at: Vec<usize> = clean
        .windows(2)
        .enumerate()
        .filter(|(_, w)| w[0] == 0xFF && w[1] == 0xDA)
        .map(|(i, _)| i)
        .collect();
    let at = after_app0(&clean);
    let mut dirty = clean[..at].to_vec();
    dirty.extend_from_slice(&metadata_segments());
    dirty.extend_from_slice(&clean[at..sos_at[1]]);
    dirty.extend_from_slice(&metadata_segments());
    dirty.extend_from_slice(&clean[sos_at[1]..sos_at[4]]);
    let mut exif = b"Exif\0\0".to_vec();
    exif.extend_from_slice(b"MM\0*\0\0\0\x08\0\0");
    dirty.extend_from_slice(&segment(0xE1, &exif));
    dirty.extend_from_slice(&clean[sos_at[4]..]);
    dirty.extend_from_slice(b"TRAILING-BYTES");

    let (copy, kind) = metadata_free_public_copy(&dirty).unwrap();
    assert_eq!(kind, "jpeg");
    assert_no_metadata_markers(&copy);
    assert_eq!(count(&copy, &[0xFF, 0xDA]), scans, "every scan is kept");
    assert!(contains(&copy, b"JFIF\0"));
    assert!(!contains(&copy, b"TRAILING-BYTES"));
    // Nothing but the spliced metadata was removed.
    assert_eq!(copy, clean);
    assert_eq!(jpeg_pixels(&copy), jpeg_pixels(&clean));
}

/// A hand-built two-scan stream (structure only; not meant to
/// decode): APP1 and COM between the scans go, the table segment
/// between the scans stays, both scans' entropy data (stuffed
/// FF00 and an RST marker included) move byte for byte, and the
/// bytes after EOI are dropped.
#[test]
fn hand_built_two_scan_stream_drops_app1_between_scans() {
    let jfif = segment(0xE0, b"JFIF\0\x01\x01\x00\x00\x01\x00\x01\x00\x00");
    let dqt = segment(0xDB, &[0u8; 65]);
    let sof = segment(0xC0, &[8, 0, 8, 0, 8, 1, 1, 0x11, 0]);
    let dht1 = segment(0xC4, &[0x00, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x05]);
    let sos = segment(0xDA, &[1, 1, 0x00, 0, 0x3F, 0]);
    let scan1 = [0x12u8, 0xFF, 0x00, 0x34, 0xFF, 0xD0, 0x56];
    let mut exif = b"Exif\0\0".to_vec();
    exif.extend_from_slice(b"II*\0\x08\0\0\0");
    let app1 = segment(0xE1, &exif);
    let com = segment(0xFE, b"between the scans");
    let dht2 = segment(0xC4, &[0x10, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01]);
    let scan2 = [0x78u8, 0xFF, 0x00, 0x9A];

    let build = |with_metadata: bool| -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8];
        for part in [&jfif, &dqt, &sof, &dht1, &sos] {
            out.extend_from_slice(part);
        }
        out.extend_from_slice(&scan1);
        if with_metadata {
            out.extend_from_slice(&app1);
            out.extend_from_slice(&com);
        }
        out.extend_from_slice(&dht2);
        out.extend_from_slice(&sos);
        out.extend_from_slice(&scan2);
        out.extend_from_slice(&[0xFF, 0xD9]);
        if with_metadata {
            out.extend_from_slice(b"GARBAGE");
        }
        out
    };
    let (copy, _) = metadata_free_public_copy(&build(true)).unwrap();
    assert!(!has_marker(&copy, 0xE1), "APP1 between scans survived");
    assert!(!has_marker(&copy, 0xFE), "COM between scans survived");
    assert_eq!(copy, build(false));
}
