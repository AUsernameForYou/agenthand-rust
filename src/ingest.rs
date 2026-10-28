//! Image ingest — port of the Python `ingest.py`.
//!
//! Accepts an asset handle (passed through), an http(s) URL, or a
//! base64 data URI; enforces byte/pixel caps; normalizes to 8-bit
//! RGBA PNG with the source's ICC profile carried into the stored
//! bytes.
//!
//! EXIF orientation is applied for JPEG inputs (a minimal TIFF
//! parse of the APP1 segment) and for PNG inputs (the eXIf chunk,
//! whose payload is a bare TIFF header — Pillow's `exif_transpose`
//! reads both). HEIC inputs are not handled (HEIC needs pillow-heif
//! on the Python side and would break the single static binary).

use crate::assets::{AssetStore, HANDLE_PREFIX, KIND_SOURCE};
use crate::image::{decode_image, encode_png_rgba, DecodedImage, RgbaImage};
use base64::Engine;

/// Default ingest size limit for a source image, in bytes.
pub const DEFAULT_MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
/// Default ingest pixel-count limit for a source image.
pub const DEFAULT_MAX_PIXELS: u64 = 40_000_000;
const FETCH_TIMEOUT_SECONDS: u64 = 15;

fn max_image_bytes() -> usize {
    std::env::var("AGENTHAND_MAX_IMAGE_BYTES")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_MAX_IMAGE_BYTES)
}

fn max_pixels() -> u64 {
    std::env::var("AGENTHAND_MAX_PIXELS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_MAX_PIXELS)
}

/// EXIF orientation (1-8) from a JPEG's APP1 Exif segment, if present.
fn jpeg_exif_orientation(data: &[u8]) -> Option<u16> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut pos = 2usize;
    while pos + 4 <= data.len() {
        if data[pos] != 0xFF {
            return None;
        }
        let marker = data[pos + 1];
        if marker == 0xDA || marker == 0xD9 {
            return None;
        }
        let seg_len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        if seg_len < 2 || pos + 2 + seg_len > data.len() {
            return None;
        }
        let segment = &data[pos + 4..pos + 2 + seg_len];
        if marker == 0xE1 && segment.starts_with(b"Exif\0\0") {
            return tiff_orientation(&segment[6..]);
        }
        pos += 2 + seg_len;
    }
    None
}

/// EXIF orientation (1-8) from a PNG's eXIf chunk, if present.
/// The chunk payload is a TIFF header with no "Exif\0\0" prefix.
fn png_exif_orientation(data: &[u8]) -> Option<u16> {
    if !data.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return None;
    }
    let mut pos = 8usize;
    while pos + 8 <= data.len() {
        let len = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
            as usize;
        let kind = &data[pos + 4..pos + 8];
        let start = pos + 8;
        let end = start.checked_add(len)?;
        if end > data.len() {
            return None;
        }
        if kind == b"eXIf" {
            return tiff_orientation(&data[start..end]);
        }
        if kind == b"IEND" {
            return None;
        }
        pos = end + 4; // skip CRC
    }
    None
}

fn tiff_orientation(tiff: &[u8]) -> Option<u16> {
    if tiff.len() < 8 {
        return None;
    }
    let little = match &tiff[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |buf: &[u8], off: usize| -> Option<u16> {
        let bytes = buf.get(off..off + 2)?;
        Some(if little {
            u16::from_le_bytes([bytes[0], bytes[1]])
        } else {
            u16::from_be_bytes([bytes[0], bytes[1]])
        })
    };
    let u32_at = |buf: &[u8], off: usize| -> Option<u32> {
        let bytes = buf.get(off..off + 4)?;
        Some(if little {
            u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        } else {
            u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        })
    };
    if u16_at(tiff, 2)? != 42 {
        return None;
    }
    let ifd0 = u32_at(tiff, 4)? as usize;
    let count = u16_at(tiff, ifd0)? as usize;
    for i in 0..count {
        let entry = ifd0 + 2 + i * 12;
        if u16_at(tiff, entry)? == 0x0112 {
            // Orientation tag; type SHORT, count 1: value in the
            // first two bytes of the value field.
            return u16_at(tiff, entry + 8);
        }
    }
    None
}

/// Apply an EXIF orientation transform (Pillow's `exif_transpose`).
fn apply_orientation(img: &RgbaImage, orientation: u16) -> RgbaImage {
    if orientation <= 1 || orientation > 8 {
        return img.clone();
    }
    let (w, h) = (img.width as usize, img.height as usize);
    let (out_w, out_h) = if orientation >= 5 {
        (h, w)
    } else {
        (w, h)
    };
    let mut out = vec![0u8; img.pixels.len()];
    for oy in 0..out_h {
        for ox in 0..out_w {
            // Map output (ox, oy) back to input (ix, iy).
            let (ix, iy) = match orientation {
                2 => (w - 1 - ox, oy),
                3 => (w - 1 - ox, h - 1 - oy),
                4 => (ox, h - 1 - oy),
                5 => (oy, ox),
                6 => (oy, h - 1 - ox),
                7 => (w - 1 - oy, h - 1 - ox),
                8 => (w - 1 - oy, ox),
                _ => unreachable!(),
            };
            let src = (iy * w + ix) * 4;
            let dst = (oy * out_w + ox) * 4;
            out[dst..dst + 4].copy_from_slice(&img.pixels[src..src + 4]);
        }
    }
    RgbaImage {
        width: out_w as u32,
        height: out_h as u32,
        pixels: out,
        icc_profile: img.icc_profile.clone(),
    }
}

/// Decode, cap-check, normalize, and re-encode image bytes as PNG.
pub fn normalize_image_bytes(data: &[u8]) -> Result<Vec<u8>, String> {
    let is_jpeg = data.starts_with(&[0xFF, 0xD8]);
    let orientation = if is_jpeg {
        jpeg_exif_orientation(data)
    } else {
        png_exif_orientation(data)
    };
    let decoded = decode_image(data)
        .map_err(|e| format!("Input does not decode as an image: {e}"))?;
    let mut rgba = match decoded {
        DecodedImage::Rgba(img) => img,
        DecodedImage::Gray(g) => g.to_rgba(),
    };
    if (rgba.width as u64) * (rgba.height as u64) > max_pixels() {
        return Err(format!(
            "Image is {}x{} ({} pixels), over the {}-pixel ingest cap.",
            rgba.width,
            rgba.height,
            (rgba.width as u64) * (rgba.height as u64),
            max_pixels()
        ));
    }
    if let Some(orientation) = orientation {
        rgba = apply_orientation(&rgba, orientation);
    }
    Ok(encode_png_rgba(&rgba))
}

fn decode_data_uri(source: &str) -> Result<Vec<u8>, String> {
    let (header, payload) = source.split_once(',').ok_or_else(|| {
        "Only base64 image data URIs are accepted (data:image/<type>;base64,...).".to_string()
    })?;
    if !header.contains(";base64") || !header.starts_with("data:image/") {
        return Err(
            "Only base64 image data URIs are accepted (data:image/<type>;base64,...)."
                .to_string(),
        );
    }
    base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|e| format!("Data URI payload is not valid base64: {e}"))
}

fn fetch_url(source: &str) -> Result<Vec<u8>, String> {
    let limit = max_image_bytes();
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECONDS))
        .build()
        .map_err(|e| format!("cannot build HTTP client: {e}"))?;
    let response = client
        .get(source)
        .header("User-Agent", "agenthand-ingest/0.1")
        .send()
        .map_err(|e| format!("fetch failed: {e}"))?;
    let bytes = response
        .bytes()
        .map_err(|e| format!("fetch failed: {e}"))?;
    if bytes.len() > limit {
        return Err(format!(
            "Fetched image exceeds the {limit}-byte ingest cap."
        ));
    }
    Ok(bytes.to_vec())
}

/// Bring an image into the store; returns its asset:// handle.
pub fn ingest(source: &str, store: &AssetStore) -> Result<String, String> {
    if source.trim().is_empty() {
        return Err(format!(
            "Image source must be a non-empty string, got {source:?}"
        ));
    }
    if source.starts_with(HANDLE_PREFIX) {
        store.get_record(source)?;
        return Ok(source.to_string());
    }
    let data = if source.starts_with("data:") {
        decode_data_uri(source)?
    } else if source.starts_with("http://") || source.starts_with("https://") {
        fetch_url(source)?
    } else {
        return Err(format!(
            "Image source must be an asset:// handle, an http(s) URL, or a base64 data URI; got {:?}...",
            &source.chars().take(40).collect::<String>()
        ));
    };
    if data.len() > max_image_bytes() {
        return Err(format!(
            "Image is {} bytes, over the {}-byte ingest cap.",
            data.len(),
            max_image_bytes()
        ));
    }
    let normalized = normalize_image_bytes(&data)?;
    store.put_bytes(&normalized, KIND_SOURCE, ".png", None)
}
