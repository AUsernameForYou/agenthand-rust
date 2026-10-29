//! Image primitives mirroring the Python (Pillow) semantics agenthand
//! relies on.
//!
//! - Decoding normalizes everything to 8-bit RGBA, the layout Pillow's
//!   ``convert("RGBA")`` produces: grayscale replicates into RGB with
//!   alpha 255, palette indices resolve through the palette (with tRNS
//!   alpha), 16-bit samples reduce to their high byte (what libpng's
//!   strip_16 does under Pillow), LA/RGB gain alpha by the obvious rule.
//! - The pixel hash is sha256 over the raw RGBA bytes, exactly
//!   ``pixel_hash_of_image`` in the Python assets module.
//! - The embedded ICC profile is carried as opaque bytes beside the
//!   pixels; its sha256 is the profile identity the receipt pins.
//! - Encoding is a small hand-rolled PNG writer (RGBA8 or L8, optional
//!   iCCP chunk) over flate2's zlib stream. Byte-parity with Pillow's
//!   encoder is NOT a goal: the store records whatever bytes we wrote,
//!   and fidelity is judged on decoded pixels, which are exact.

use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    /// Row-major RGBA bytes, width*height*4.
    pub pixels: Vec<u8>,
    pub icc_profile: Option<Vec<u8>>,
}

impl RgbaImage {
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Self {
        assert_eq!(pixels.len(), (width * height * 4) as usize);
        RgbaImage {
            width,
            height,
            pixels,
            icc_profile: None,
        }
    }

    pub fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Self {
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        for chunk in pixels.as_chunks_mut::<4>().0 {
            chunk.copy_from_slice(&rgba);
        }
        RgbaImage::new(width, height, pixels)
    }

    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [
            self.pixels[i],
            self.pixels[i + 1],
            self.pixels[i + 2],
            self.pixels[i + 3],
        ]
    }

    pub fn alpha_plane(&self) -> Vec<u8> {
        self.pixels.iter().skip(3).step_by(4).copied().collect()
    }

    /// sha256 of the decoded RGBA bytes — the Python `pixel_sha256`.
    pub fn pixel_sha256(&self) -> String {
        sha256_hex(&self.pixels)
    }

    pub fn color_profile_sha256(&self) -> Option<String> {
        self.icc_profile.as_ref().map(|icc| sha256_hex(icc))
    }
}

#[derive(Clone, Debug)]
pub struct GrayImage {
    pub width: u32,
    pub height: u32,
    /// Row-major single-channel bytes.
    pub pixels: Vec<u8>,
    pub icc_profile: Option<Vec<u8>>,
}

impl GrayImage {
    pub fn to_rgba(&self) -> RgbaImage {
        let mut out = Vec::with_capacity(self.pixels.len() * 4);
        for &v in &self.pixels {
            out.extend_from_slice(&[v, v, v, 255]);
        }
        RgbaImage {
            width: self.width,
            height: self.height,
            pixels: out,
            icc_profile: self.icc_profile.clone(),
        }
    }
}

/// Lowercase hex SHA-256 of the given bytes.
pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// Any decoded image, before RGBA normalization.
pub enum DecodedImage {
    Rgba(RgbaImage),
    Gray(GrayImage),
}

impl DecodedImage {
    pub fn into_rgba(self) -> RgbaImage {
        match self {
            DecodedImage::Rgba(img) => img,
            DecodedImage::Gray(gray) => gray.to_rgba(),
        }
    }
}

/// Decode PNG, JPEG, or HEIC bytes into a normalized image (Pillow
/// semantics: `Image.open(...).convert("RGBA")`, with the ICC
/// profile extracted). HEIC is sniffed by content (the `ftyp`
/// brands) and decoded by our own from-scratch decoder
/// (`heic::decode`); a HEIC file outside the decoder's subset
/// refuses with the subset's message — there is no external
/// fallback.
pub fn decode_image(data: &[u8]) -> Result<DecodedImage, String> {
    if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        decode_png(data).map(|png| match png {
            PngDecoded::Rgba(img) => DecodedImage::Rgba(img),
            PngDecoded::Gray(gray) => DecodedImage::Gray(gray),
        })
    } else if data.starts_with(&[0xFF, 0xD8]) {
        decode_jpeg(data).map(DecodedImage::Rgba)
    } else if let Some(img) = decode_heic_image(data) {
        img.map(DecodedImage::Rgba)
    } else {
        Err("unsupported image format (PNG, JPEG and HEIC only)".to_string())
    }
}

/// The HEIC branch of `decode_image`, returning `None` when the
/// bytes are not HEIC at all (the caller's unsupported-format
/// error then applies).
fn decode_heic_image(data: &[u8]) -> Option<Result<RgbaImage, String>> {
    // Untrusted-input boundary (M5, widened by the M5 fix lane):
    // seller photos are arbitrary bytes, so the ENTIRE HEIC path —
    // the `is_heic` sniff, the budget-checked decode, the `to_rgb8`
    // conversion, and the RGBA packing — runs inside this one
    // `catch_unwind`. The decoder is written to refuse malformed
    // input with error values; this catch is the guarantee that
    // even an unforeseen panic anywhere on that path surfaces as
    // an ordinary ingest error, never as a dead server. The
    // guarantee holds only under the unwind panic strategy, which
    // `lib.rs` pins with a `compile_error!` guard.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if !crate::heic::decode::is_heic(data) {
            return None;
        }
        Some(decode_heic_image_caught(data))
    }))
    .unwrap_or_else(|_| {
        Some(Err(
            "HEIC decode failed (decoder fault contained at the ingest boundary)".to_string(),
        ))
    })
}

fn decode_heic_image_caught(data: &[u8]) -> Result<RgbaImage, String> {
    // The product pixel budget is passed into the decoder, which
    // refuses an over-budget primary from its declared dimensions
    // (grid canvas / SPS conformance window) before decoding —
    // the post-decode cap in `ingest::normalize_image_bytes` is
    // the backstop, not the first check (M5 fix lane).
    let img = crate::heic::decode::decode_heic_with_budget(data, crate::ingest::max_pixels())
        .map_err(|e| format!("HEIC decode: {e}"))?;
    let rgb = crate::heic::decode::to_rgb8(&img);
    let (w, h) = (img.planes.width as u32, img.planes.height as u32);
    let mut pixels = Vec::with_capacity((w * h * 4) as usize);
    for px in rgb.as_chunks::<3>().0 {
        pixels.extend_from_slice(&[px[0], px[1], px[2], 255]);
    }
    Ok(RgbaImage {
        width: w,
        height: h,
        pixels,
        icc_profile: img.icc_profile,
    })
}

enum PngDecoded {
    Rgba(RgbaImage),
    Gray(GrayImage),
}

fn decode_png(data: &[u8]) -> Result<PngDecoded, String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("PNG decode failed: {e}"))?;
    let icc_profile = reader
        .info()
        .icc_profile
        .as_ref()
        .map(|cow| cow.as_ref().to_vec());
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("PNG decode failed: {e}"))?;
    buf.truncate(frame.buffer_size());
    let width = frame.width;
    let height = frame.height;
    // After EXPAND, output is 8-bit: Grayscale, GrayscaleAlpha, RGB or RGBA.
    match frame.color_type {
        png::ColorType::Grayscale => Ok(PngDecoded::Gray(GrayImage {
            width,
            height,
            pixels: buf,
            icc_profile,
        })),
        png::ColorType::GrayscaleAlpha => {
            // Pillow keeps LA as LA; convert("RGBA") maps L->RGB, A->A.
            let mut out = Vec::with_capacity((width * height * 4) as usize);
            for pair in buf.as_chunks::<2>().0 {
                out.extend_from_slice(&[pair[0], pair[0], pair[0], pair[1]]);
            }
            Ok(PngDecoded::Rgba(RgbaImage {
                width,
                height,
                pixels: out,
                icc_profile,
            }))
        }
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity((width * height * 4) as usize);
            for px in buf.as_chunks::<3>().0 {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            Ok(PngDecoded::Rgba(RgbaImage {
                width,
                height,
                pixels: out,
                icc_profile,
            }))
        }
        png::ColorType::Rgba => Ok(PngDecoded::Rgba(RgbaImage {
            width,
            height,
            pixels: buf,
            icc_profile,
        })),
        other => Err(format!("unexpected PNG color type after EXPAND: {other:?}")),
    }
}

fn decode_jpeg(data: &[u8]) -> Result<RgbaImage, String> {
    let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(data));
    let pixels = decoder
        .decode()
        .map_err(|e| format!("JPEG decode failed: {e}"))?;
    let info = decoder
        .info()
        .ok_or_else(|| "JPEG decode produced no info".to_string())?;
    let icc_profile = decoder.icc_profile();
    let width = info.width as u32;
    let height = info.height as u32;
    let rgba = match info.pixel_format {
        jpeg_decoder::PixelFormat::L8 => {
            let mut out = Vec::with_capacity(pixels.len() * 4);
            for &v in &pixels {
                out.extend_from_slice(&[v, v, v, 255]);
            }
            out
        }
        jpeg_decoder::PixelFormat::L16 => {
            let mut out = Vec::with_capacity(pixels.len() * 2);
            for pair in pixels.as_chunks::<2>().0 {
                out.extend_from_slice(&[pair[0], pair[0], pair[0], 255]);
            }
            out
        }
        jpeg_decoder::PixelFormat::RGB24 => {
            let mut out = Vec::with_capacity(pixels.len() / 3 * 4);
            for px in pixels.as_chunks::<3>().0 {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            out
        }
        jpeg_decoder::PixelFormat::CMYK32 => {
            // jpeg-decoder returns inverted CMYK (Adobe convention);
            // Pillow converts CMYK->RGB as 255*(1-min(1, c+k)) per channel
            // on the inverted values it exposes. Rare path for seller
            // photos; implemented per Pillow's formula on raw samples.
            let mut out = Vec::with_capacity(pixels.len());
            for px in pixels.as_chunks::<4>().0 {
                let (c, m, y, k) = (
                    255 - px[0] as u32,
                    255 - px[1] as u32,
                    255 - px[2] as u32,
                    255 - px[3] as u32,
                );
                let r = (255 - (c + k).min(255)) as u8;
                let g = (255 - (m + k).min(255)) as u8;
                let b = (255 - (y + k).min(255)) as u8;
                out.extend_from_slice(&[r, g, b, 255]);
            }
            out
        }
    };
    Ok(RgbaImage {
        width,
        height,
        pixels: rgba,
        icc_profile,
    })
}

// ---------------------------------------------------------------------------
// PNG encoding (hand-rolled; RGBA8 and L8; optional iCCP).
// ---------------------------------------------------------------------------

fn crc32(data: &[u8]) -> u32 {
    // PNG's CRC (ISO 3309 polynomial), table-driven.
    static mut TABLE: [u32; 256] = [0; 256];
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        for n in 0..256u32 {
            let mut c = n;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB88320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            TABLE[n as usize] = c;
        }
    });
    let table = unsafe { &*(&raw const TABLE) };
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_data = Vec::with_capacity(4 + data.len());
    crc_data.extend_from_slice(kind);
    crc_data.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_data).to_be_bytes());
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let a = a as i32;
    let b = b as i32;
    let c = c as i32;
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        a as u8
    } else if pb <= pc {
        b as u8
    } else {
        c as u8
    }
}

/// Filter one row with the given PNG filter type; `prev` is the raw row
/// above (zeros for the first row), `bpp` bytes per pixel.
fn filter_row(kind: u8, row: &[u8], prev: &[u8], bpp: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(row.len());
    match kind {
        0 => out.extend_from_slice(row),
        1 => {
            for (i, &v) in row.iter().enumerate() {
                let left = if i >= bpp { row[i - bpp] } else { 0 };
                out.push(v.wrapping_sub(left));
            }
        }
        2 => {
            for (i, &v) in row.iter().enumerate() {
                out.push(v.wrapping_sub(prev[i]));
            }
        }
        3 => {
            for (i, &v) in row.iter().enumerate() {
                let left = if i >= bpp { row[i - bpp] as u32 } else { 0 };
                let up = prev[i] as u32;
                out.push(v.wrapping_sub(((left + up) / 2) as u8));
            }
        }
        4 => {
            for (i, &v) in row.iter().enumerate() {
                let left = if i >= bpp { row[i - bpp] } else { 0 };
                let up = prev[i];
                let up_left = if i >= bpp { prev[i - bpp] } else { 0 };
                out.push(v.wrapping_sub(paeth(left, up, up_left)));
            }
        }
        _ => unreachable!(),
    }
    out
}

/// Encode filtered scanlines: pick per-row the filter minimizing the sum
/// of absolute signed filter outputs (the classic libpng heuristic).
fn encode_png(
    width: u32,
    height: u32,
    color_type: u8,
    bpp: usize,
    rows: &[u8],
    icc: Option<&[u8]>,
    text: Option<(&str, &str)>,
) -> Vec<u8> {
    let row_len = width as usize * bpp;
    assert_eq!(rows.len(), row_len * height as usize);
    let mut raw = Vec::with_capacity((row_len + 1) * height as usize);
    let zeros = vec![0u8; row_len];
    let mut prev: &[u8] = &zeros;
    for y in 0..height as usize {
        let row = &rows[y * row_len..(y + 1) * row_len];
        let mut best: Option<(u64, u8, Vec<u8>)> = None;
        for kind in 0..=4u8 {
            let filtered = filter_row(kind, row, prev, bpp);
            let score: u64 = filtered
                .iter()
                .map(|&b| (b as i8).unsigned_abs() as u64)
                .sum();
            if best.as_ref().map(|(s, _, _)| score < *s).unwrap_or(true) {
                best = Some((score, kind, filtered));
            }
        }
        let (_, kind, filtered) = best.unwrap();
        raw.push(kind);
        raw.extend_from_slice(&filtered);
        prev = row;
    }
    let compressed = {
        use std::io::Write;
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&raw).expect("zlib write");
        encoder.finish().expect("zlib finish")
    };
    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n']);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(color_type);
    ihdr.push(0); // compression
    ihdr.push(0); // filter
    ihdr.push(0); // interlace
    write_chunk(&mut out, b"IHDR", &ihdr);
    if let Some(profile) = icc {
        // iCCP: name (null-terminated), compression byte 0, zlib stream.
        use std::io::Write;
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(profile).expect("zlib write");
        let compressed_profile = z.finish().expect("zlib finish");
        let mut iccp = Vec::new();
        iccp.extend_from_slice(b"ICC PROFILE\0");
        iccp.push(0);
        iccp.extend_from_slice(&compressed_profile);
        write_chunk(&mut out, b"iCCP", &iccp);
    }
    if let Some((key, value)) = text {
        // tEXt: keyword, null separator, value (Latin-1; the one
        // caller writes ASCII JSON).
        let mut chunk = Vec::with_capacity(key.len() + 1 + value.len());
        chunk.extend_from_slice(key.as_bytes());
        chunk.push(0);
        chunk.extend_from_slice(value.as_bytes());
        write_chunk(&mut out, b"tEXt", &chunk);
    }
    write_chunk(&mut out, b"IDAT", &compressed);
    write_chunk(&mut out, b"IEND", &[]);
    out
}

/// Encode an RGBA image as PNG bytes.
pub fn encode_png_rgba(img: &RgbaImage) -> Vec<u8> {
    encode_png(
        img.width,
        img.height,
        6,
        4,
        &img.pixels,
        img.icc_profile.as_deref(),
        None,
    )
}

/// Encode a grayscale image as PNG bytes.
pub fn encode_png_gray(img: &GrayImage) -> Vec<u8> {
    encode_png(
        img.width,
        img.height,
        0,
        1,
        &img.pixels,
        img.icc_profile.as_deref(),
        None,
    )
}

/// Encode a grayscale image as PNG bytes carrying one tEXt chunk
/// (the mask provenance carrier; see `assets::MASK_PROVENANCE_PNG_KEY`).
pub fn encode_png_gray_with_text(img: &GrayImage, key: &str, value: &str) -> Vec<u8> {
    encode_png(
        img.width,
        img.height,
        0,
        1,
        &img.pixels,
        img.icc_profile.as_deref(),
        Some((key, value)),
    )
}

/// Human-readable colour-space label for receipts, mirroring
/// `describe_color_profile` (without the ICC description parse: the
/// profile hash beside it is the checked identity).
pub fn describe_color_profile(icc: Option<&[u8]>) -> String {
    match icc {
        None => "sRGB (no embedded ICC profile)".to_string(),
        Some(_) => "embedded ICC profile (description unreadable)".to_string(),
    }
}
