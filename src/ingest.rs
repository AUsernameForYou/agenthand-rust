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
//! reads both). HEIC inputs decode with the in-tree from-scratch
//! decoder (M5), which applies the container's `irot` itself, so
//! no EXIF step runs for them.

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

pub(crate) fn max_pixels() -> u64 {
    std::env::var("AGENTHAND_MAX_PIXELS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_MAX_PIXELS)
}

/// EXIF orientation (1-8) from a JPEG's APP1 Exif segment, if present.
fn jpeg_exif_tiff(data: &[u8]) -> Option<&[u8]> {
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
            return Some(&segment[6..]);
        }
        pos += 2 + seg_len;
    }
    None
}

fn jpeg_exif_orientation(data: &[u8]) -> Option<u16> {
    tiff_orientation(jpeg_exif_tiff(data)?)
}

/// The eXIf chunk payload (a bare TIFF header) from a PNG, if present.
fn png_exif_tiff(data: &[u8]) -> Option<&[u8]> {
    if !data.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return None;
    }
    let mut pos = 8usize;
    while pos + 8 <= data.len() {
        let len =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        let kind = &data[pos + 4..pos + 8];
        let start = pos + 8;
        let end = start.checked_add(len)?;
        if end > data.len() {
            return None;
        }
        if kind == b"eXIf" {
            return Some(&data[start..end]);
        }
        if kind == b"IEND" {
            return None;
        }
        pos = end + 4; // skip CRC
    }
    None
}

/// EXIF orientation (1-8) from a PNG's eXIf chunk, if present.
/// The chunk payload is a TIFF header with no "Exif\0\0" prefix.
fn png_exif_orientation(data: &[u8]) -> Option<u16> {
    tiff_orientation(png_exif_tiff(data)?)
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

/// Read one ASCII tag's string from a TIFF IFD (type 2; inline in
/// the 4-byte value field when count <= 4, else at the offset it
/// holds). Returns the trimmed string when it is non-empty.
fn tiff_ascii_tag(
    tiff: &[u8],
    ifd_offset: usize,
    wanted: u16,
    u16_at: &dyn Fn(&[u8], usize) -> Option<u16>,
    u32_at: &dyn Fn(&[u8], usize) -> Option<u32>,
) -> Option<String> {
    let count = u16_at(tiff, ifd_offset)? as usize;
    for i in 0..count {
        let entry = ifd_offset + 2 + i * 12;
        if u16_at(tiff, entry)? != wanted {
            continue;
        }
        if u16_at(tiff, entry + 2)? != 2 {
            return None; // not ASCII
        }
        let len = u32_at(tiff, entry + 4)? as usize;
        if len == 0 {
            return None;
        }
        let bytes: Vec<u8> = if len <= 4 {
            tiff.get(entry + 8..entry + 8 + len)?.to_vec()
        } else {
            let off = u32_at(tiff, entry + 8)? as usize;
            tiff.get(off..off + len)?.to_vec()
        };
        let text = String::from_utf8_lossy(&bytes)
            .trim_end_matches('\0')
            .trim()
            .to_string();
        return if text.is_empty() { None } else { Some(text) };
    }
    None
}

/// EXIF capture time from TIFF bytes: DateTimeOriginal (0x9003 in
/// the Exif IFD, reached via the 0x8769 pointer in IFD0) preferred,
/// IFD0 DateTime (0x0132) as the fallback. The string is returned
/// verbatim ("YYYY:MM:DD HH:MM:SS" as the camera wrote it): an
/// ownership receipt binds the FILE, and this is the file's own
/// statement about when it was captured — evidence, never a claim
/// the file is unedited.
fn tiff_capture_time(tiff: &[u8]) -> Option<String> {
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
    let plausible = |s: &str| -> bool {
        let b = s.as_bytes();
        s.len() >= 10 && b[..4].iter().all(|c| c.is_ascii_digit()) && b[4] == b':'
    };
    // Exif IFD pointer (0x8769, LONG, count 1: value is the offset).
    let count = u16_at(tiff, ifd0)? as usize;
    for i in 0..count {
        let entry = ifd0 + 2 + i * 12;
        if u16_at(tiff, entry)? == 0x8769 {
            let exif_ifd = u32_at(tiff, entry + 8)? as usize;
            if let Some(dt) = tiff_ascii_tag(tiff, exif_ifd, 0x9003, &u16_at, &u32_at) {
                if plausible(&dt) {
                    return Some(dt);
                }
            }
        }
    }
    match tiff_ascii_tag(tiff, ifd0, 0x0132, &u16_at, &u32_at) {
        Some(dt) if plausible(&dt) => Some(dt),
        _ => None,
    }
}

/// The capture time a photo file states about itself, across the
/// three containers the product ingests: JPEG (APP1 Exif), PNG
/// (eXIf chunk), HEIC (the Exif item; its payload opens with a
/// 4-byte offset to the TIFF header, which we locate by that
/// offset and, failing it, by the TIFF magic).
/// Candidate TIFF byte ranges inside a HEIC file's Exif item, in
/// the order `exif_capture_time` has always tried them: the
/// payload opens with a 4-byte offset to the TIFF header; try the
/// declared offset first (both readings of it), then scan the
/// first bytes for the TIFF magic. Shared with the provenance
/// reader so both walk the same candidates.
pub(crate) fn heic_exif_tiff_candidates(data: &[u8]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let Some(heif) = crate::heic::container::parse(data).ok() else {
        return out;
    };
    let Some(item) = heif.items.iter().find(|i| i.item_type == "Exif") else {
        return out;
    };
    let Ok(payload) = crate::heic::container::extract_item_data_for(data, item) else {
        return out;
    };
    if payload.len() >= 4 {
        let declared =
            u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
        for start in [4 + declared, declared] {
            if let Some(tiff) = payload.get(start..) {
                out.push(tiff.to_vec());
            }
        }
        // Fall back to scanning the first bytes for the TIFF magic.
        for start in 0..payload.len().saturating_sub(8).min(64) {
            if payload[start..].starts_with(b"II*\0") || payload[start..].starts_with(b"MM\0*") {
                out.push(payload[start..].to_vec());
            }
        }
    }
    out
}

pub fn exif_capture_time(data: &[u8]) -> Option<String> {
    if let Some(tiff) = jpeg_exif_tiff(data) {
        if let Some(dt) = tiff_capture_time(tiff) {
            return Some(dt);
        }
    }
    if let Some(tiff) = png_exif_tiff(data) {
        if let Some(dt) = tiff_capture_time(tiff) {
            return Some(dt);
        }
    }
    for tiff in heic_exif_tiff_candidates(data) {
        if let Some(dt) = tiff_capture_time(&tiff) {
            return Some(dt);
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
    let (out_w, out_h) = if orientation >= 5 { (h, w) } else { (w, h) };
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
    let decoded =
        decode_image(data).map_err(|e| format!("Input does not decode as an image: {e}"))?;
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
            "Only base64 image data URIs are accepted (data:image/<type>;base64,...).".to_string(),
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
    let bytes = response.bytes().map_err(|e| format!("fetch failed: {e}"))?;
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
            source.chars().take(40).collect::<String>()
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
    // Provenance signals: read what the original file says about
    // itself BEFORE normalization discards its metadata, mint the
    // metadata-free public copy when the file carries location
    // coordinates, and carry the block inside the stored PNG as a
    // text chunk (the mask-provenance carrier pattern) so draw
    // can republish it into the fidelity receipt under the
    // asset's own pinned hash. Best-effort by design: a file
    // whose block cannot be built or embedded simply stores
    // without one, exactly like a mask written before mask
    // provenance existed.
    let dims = crate::image::png_dimensions(&normalized);
    let mut block = crate::provenance::provenance_block(&data, dims);
    crate::provenance::attach_public_copy(&mut block, store, &data, None);
    let canonical = serde_json::to_string(&block).unwrap_or_default();
    let annotated =
        crate::image::inject_png_text(&normalized, crate::assets::PROVENANCE_PNG_KEY, &canonical)
            .unwrap_or(normalized);
    store.put_bytes(&annotated, KIND_SOURCE, ".png", None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Big-endian TIFF carrying only an IFD0 DateTime (0x0132):
    /// the fallback path when no Exif IFD / DateTimeOriginal
    /// exists.
    fn tiff_with_ifd0_datetime(dt: &str) -> Vec<u8> {
        let mut tiff: Vec<u8> = Vec::new();
        tiff.extend_from_slice(b"MM");
        tiff.extend_from_slice(&42u16.to_be_bytes());
        tiff.extend_from_slice(&8u32.to_be_bytes());
        tiff.extend_from_slice(&1u16.to_be_bytes());
        tiff.extend_from_slice(&0x0132u16.to_be_bytes());
        tiff.extend_from_slice(&2u16.to_be_bytes()); // ASCII
        tiff.extend_from_slice(&20u32.to_be_bytes());
        tiff.extend_from_slice(&26u32.to_be_bytes()); // string at 26
        tiff.extend_from_slice(&0u32.to_be_bytes());
        let mut s = dt.as_bytes().to_vec();
        s.push(0);
        while s.len() < 20 {
            s.push(0);
        }
        tiff.extend_from_slice(&s[..20]);
        tiff
    }

    #[test]
    fn capture_time_from_jpeg_app1_and_bare_tiff() {
        let tiff = tiff_with_ifd0_datetime("2025:06:15 12:30:45");
        // Wrap as a JPEG: SOI + APP1(Exif\0\0 + TIFF) + EOI. The
        // reader only walks segments; no pixel data is needed.
        let mut payload = b"Exif\0\0".to_vec();
        payload.extend_from_slice(&tiff);
        let mut jpeg: Vec<u8> = vec![0xFF, 0xD8, 0xFF, 0xE1];
        jpeg.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(&payload);
        jpeg.extend_from_slice(&[0xFF, 0xD9]);
        assert_eq!(
            exif_capture_time(&jpeg).as_deref(),
            Some("2025:06:15 12:30:45")
        );
        // Garbage and EXIF-less inputs return None, never panic.
        assert_eq!(exif_capture_time(b"not an image"), None);
        assert_eq!(exif_capture_time(&[0xFF, 0xD8, 0xFF, 0xD9]), None);
        assert_eq!(exif_capture_time(&[]), None);
    }
}
