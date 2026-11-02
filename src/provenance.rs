//! Provenance signals: what a photo file says about itself.
//!
//! The Fidelity Lock receipt proves pixels are unchanged SINCE a
//! file reached us. This module reads the weaker, earlier layer:
//! the statements the file itself carries about its life before
//! that moment — the camera it names, the editing software it
//! names, the edit history Adobe-family tools record, whether its
//! dates agree, what its container actually holds, and whether it
//! carries location coordinates or a Content Credentials
//! manifest.
//!
//! Wording is the product here, and it is a hard rule of this
//! module: every signal is reported as a found / not-found
//! observation with an explicit statement of what it establishes
//! and what it does not establish. Nothing in this module emits
//! an edited / unedited verdict, and an absent signal is always
//! reported as meaningless, never as cleanliness. Metadata is
//! self-reported by software; an edited-then-stripped file reads
//! exactly like an untouched one, and the block says so.
//!
//! Everything is a local read over bytes already in hand at
//! ingest: no call, no cost. The block rides the source asset as
//! a PNG text chunk (written by `ingest`, see
//! `assets::PROVENANCE_PNG_KEY`) and is republished by draw into
//! the fidelity receipt and by the room run into KEEP ownership
//! records.
//!
//! The GPS half of the module is the seller-safety rule for
//! public surfaces (the AB 723 original-photo pages): when a
//! photo carries location coordinates, the block warns in plain
//! language and the ingest / keep paths mint a derived public
//! copy with the metadata removed and the pixels identical (see
//! `metadata_free_public_copy`). Originals and receipts are
//! never modified to make a copy.

use serde_json::{json, Map, Value};

/// The scope statement printed at the top of every block. It is
/// part of the product surface: a reader who sees only this and
/// the signal lines has the honest frame.
pub const SIGNALS_SCOPE: &str = "Observations about what the file itself contains and claims, read when the photo arrived. These are not a verdict: this record never calls a photo edited or unedited. Metadata is written by software and can be edited or stripped, so the absence of any signal below is not evidence that a file is untouched.";

/// Cheap label attached to every derived public copy, wherever
/// the copy surfaces.
pub const PUBLIC_COPY_LABEL: &str = "Derived public copy: all embedded metadata (including any location coordinates) was removed; the image pixels are identical to the original file, which is unchanged and still bound by its receipt.";

// ---------------------------------------------------------------------------
// Format detection and container walks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhotoFormat {
    Jpeg,
    Png,
    Heic,
    Unknown,
}

impl PhotoFormat {
    pub fn name(self) -> &'static str {
        match self {
            PhotoFormat::Jpeg => "jpeg",
            PhotoFormat::Png => "png",
            PhotoFormat::Heic => "heic",
            PhotoFormat::Unknown => "unknown",
        }
    }
}

const HEIF_BRANDS: [&str; 12] = [
    "heic", "heix", "hevc", "hevh", "heim", "heis", "hevm", "hevs", "mif1", "mif2", "msf1", "heif",
];

pub fn detect_format(data: &[u8]) -> PhotoFormat {
    if data.starts_with(&[0xFF, 0xD8]) {
        return PhotoFormat::Jpeg;
    }
    if data.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return PhotoFormat::Png;
    }
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        let brand = String::from_utf8_lossy(&data[8..12]).into_owned();
        if HEIF_BRANDS.contains(&brand.as_str()) {
            return PhotoFormat::Heic;
        }
    }
    PhotoFormat::Unknown
}

struct JpegSeg {
    marker: u8,
    /// Payload range (after the length field).
    payload: std::ops::Range<usize>,
    /// Whole segment range, marker through payload.
    span: std::ops::Range<usize>,
}

struct JpegWalk {
    segments: Vec<JpegSeg>,
    /// Bytes between the end of the SOS header segment and the
    /// start of the EOI marker (the entropy-coded data length is
    /// not reported; only whether trailing bytes exist matters).
    trailing_bytes: usize,
    has_scan: bool,
}

/// Walk JPEG segments up to SOS, then locate the EOI through the
/// entropy-coded data (byte-stuffed FF00 and restart markers are
/// data, not markers). Returns None for a non-JPEG or truncated
/// header walk.
fn jpeg_walk(data: &[u8]) -> Option<JpegWalk> {
    if !data.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut segments = Vec::new();
    let mut pos = 2usize;
    let mut trailing_bytes = 0usize;
    let mut has_scan = false;
    while pos + 2 <= data.len() {
        if data[pos] != 0xFF {
            return None;
        }
        let marker = data[pos + 1];
        if marker == 0xD9 {
            // EOI before any SOS: a header-only file.
            trailing_bytes = data.len().saturating_sub(pos + 2);
            return Some(JpegWalk {
                segments,
                trailing_bytes,
                has_scan,
            });
        }
        if marker == 0x01 || (0xD0..=0xD8).contains(&marker) {
            // Standalone markers carry no length field.
            pos += 2;
            continue;
        }
        if pos + 4 > data.len() {
            return None;
        }
        let seg_len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        if seg_len < 2 || pos + 2 + seg_len > data.len() {
            return None;
        }
        let span = pos..pos + 2 + seg_len;
        let payload = pos + 4..pos + 2 + seg_len;
        let is_sos = marker == 0xDA;
        segments.push(JpegSeg {
            marker,
            payload,
            span,
        });
        pos += 2 + seg_len;
        if is_sos {
            has_scan = true;
            // Entropy-coded data: find the real EOI.
            let mut i = pos;
            let mut eoi_end: Option<usize> = None;
            while i + 1 < data.len() {
                if data[i] != 0xFF {
                    i += 1;
                    continue;
                }
                match data[i + 1] {
                    0x00 | 0xD0..=0xD7 => i += 2, // stuffed byte / restart marker
                    0xFF => i += 1,             // fill byte
                    0xD9 => {
                        eoi_end = Some(i + 2);
                        break;
                    }
                    _ => i += 2, // any other marker inside scan data: tolerate
                }
            }
            trailing_bytes = match eoi_end {
                Some(end) => data.len().saturating_sub(end),
                None => 0,
            };
            break;
        }
    }
    Some(JpegWalk {
        segments,
        trailing_bytes,
        has_scan,
    })
}

struct PngChunk {
    kind: [u8; 4],
    payload: std::ops::Range<usize>,
    span: std::ops::Range<usize>,
}

struct PngWalk {
    chunks: Vec<PngChunk>,
    trailing_bytes: usize,
}

fn png_walk(data: &[u8]) -> Option<PngWalk> {
    if !data.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return None;
    }
    let mut chunks = Vec::new();
    let mut pos = 8usize;
    let mut trailing_bytes = 0usize;
    while pos + 12 <= data.len() {
        let len =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        let kind: [u8; 4] = [data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]];
        let end = pos.checked_add(12 + len)?;
        if end > data.len() {
            return None;
        }
        chunks.push(PngChunk {
            kind,
            payload: pos + 8..pos + 8 + len,
            span: pos..end,
        });
        pos = end;
        if &kind == b"IEND" {
            trailing_bytes = data.len().saturating_sub(pos);
            break;
        }
    }
    Some(PngWalk {
        chunks,
        trailing_bytes,
    })
}

struct TopBox {
    kind: [u8; 4],
    payload: std::ops::Range<usize>,
    span: std::ops::Range<usize>,
}

/// Top-level ISO-BMFF box walk (HEIC census support: trailing
/// data and the C2PA box scan). Best-effort: a malformed tail
/// just ends the walk.
fn top_level_boxes(data: &[u8]) -> (Vec<TopBox>, usize) {
    let mut boxes = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let size32 = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        let kind: [u8; 4] = [data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]];
        let (header, size) = if size32 == 1 {
            if pos + 16 > data.len() {
                break;
            }
            (
                16usize,
                u64::from_be_bytes([
                    data[pos + 8],
                    data[pos + 9],
                    data[pos + 10],
                    data[pos + 11],
                    data[pos + 12],
                    data[pos + 13],
                    data[pos + 14],
                    data[pos + 15],
                ]),
            )
        } else if size32 == 0 {
            (8usize, (data.len() - pos) as u64)
        } else {
            (8usize, size32 as u64)
        };
        if size < header as u64 {
            break;
        }
        let end = match pos.checked_add(size as usize) {
            Some(end) if end <= data.len() => end,
            _ => break,
        };
        boxes.push(TopBox {
            kind,
            payload: pos + header..end,
            span: pos..end,
        });
        pos = end;
        if size32 == 0 {
            break;
        }
    }
    let trailing = if boxes.is_empty() {
        0
    } else {
        data.len().saturating_sub(pos)
    };
    (boxes, trailing)
}

// ---------------------------------------------------------------------------
// TIFF reading (EXIF): the same bounded walks ingest uses for the
// capture time, widened to the tags the signals need. Nothing here
// trusts an offset without a bounds check.
// ---------------------------------------------------------------------------

const TIFF_TYPE_SIZES: [usize; 13] = [0, 1, 1, 2, 4, 8, 1, 1, 2, 4, 8, 4, 8];

struct Tiff<'a> {
    buf: &'a [u8],
    little: bool,
}

impl<'a> Tiff<'a> {
    fn new(buf: &'a [u8]) -> Option<Tiff<'a>> {
        if buf.len() < 8 {
            return None;
        }
        let little = match &buf[0..2] {
            b"II" => true,
            b"MM" => false,
            _ => return None,
        };
        let t = Tiff { buf, little };
        if t.u16_at(2)? != 42 {
            return None;
        }
        Some(t)
    }

    fn u16_at(&self, off: usize) -> Option<u16> {
        let b = self.buf.get(off..off + 2)?;
        Some(if self.little {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        })
    }

    fn u32_at(&self, off: usize) -> Option<u32> {
        let b = self.buf.get(off..off + 4)?;
        Some(if self.little {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        })
    }

    fn ifd0_offset(&self) -> Option<usize> {
        Some(self.u32_at(4)? as usize)
    }

    /// One IFD entry: (tag, type, count, file offset of the
    /// 4-byte value field).
    fn entry(&self, ifd: usize, tag: u16) -> Option<(u16, u32, usize)> {
        let count = self.u16_at(ifd)? as usize;
        for i in 0..count {
            let entry = ifd + 2 + i * 12;
            if self.u16_at(entry)? == tag {
                return Some((
                    self.u16_at(entry + 2)?,
                    self.u32_at(entry + 4)?,
                    entry + 8,
                ));
            }
        }
        None
    }

    /// The entry's value bytes (inline when they fit in the
    /// 4-byte field, else at the offset the field holds).
    fn value_bytes(&self, typ: u16, count: u32, field_off: usize) -> Option<&'a [u8]> {
        let size = TIFF_TYPE_SIZES.get(typ as usize).copied()?;
        let total = size.checked_mul(count as usize)?;
        if total <= 4 {
            self.buf.get(field_off..field_off + total)
        } else {
            let off = self.u32_at(field_off)? as usize;
            self.buf.get(off..off + total)
        }
    }

    fn ascii(&self, ifd: usize, tag: u16) -> Option<String> {
        let (typ, count, field) = self.entry(ifd, tag)?;
        if typ != 2 {
            return None;
        }
        let bytes = self.value_bytes(typ, count, field)?;
        let text = String::from_utf8_lossy(bytes)
            .trim_end_matches('\0')
            .trim()
            .to_string();
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// A SHORT or LONG count-1 value (pointers, dimensions).
    fn numeric(&self, ifd: usize, tag: u16) -> Option<u64> {
        let (typ, count, field) = self.entry(ifd, tag)?;
        if count != 1 {
            return None;
        }
        match typ {
            3 => self.u16_at(field).map(|v| v as u64),
            4 => self.u32_at(field).map(|v| v as u64),
            _ => None,
        }
    }

    /// An IFD pointer tag (0x8769 Exif, 0x8825 GPS): the offset
    /// it holds, validated as an IFD start.
    fn sub_ifd(&self, ifd: usize, tag: u16) -> Option<usize> {
        let off = self.numeric(ifd, tag)? as usize;
        self.u16_at(off)?;
        Some(off)
    }

    fn rationals(&self, ifd: usize, tag: u16) -> Option<Vec<(u32, u32)>> {
        let (typ, count, field) = self.entry(ifd, tag)?;
        if typ != 5 {
            return None;
        }
        let bytes = self.value_bytes(typ, count, field)?;
        let mut out = Vec::new();
        for i in 0..count as usize {
            let chunk = bytes.get(i * 8..i * 8 + 8)?;
            let rd = |b: &[u8]| {
                if self.little {
                    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
                } else {
                    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
                }
            };
            out.push((rd(&chunk[0..4]), rd(&chunk[4..8])));
        }
        Some(out)
    }

    /// Whether a next IFD follows this one (JPEG EXIF carries
    /// the thumbnail in IFD1).
    fn has_next_ifd(&self, ifd: usize) -> bool {
        let count = match self.u16_at(ifd) {
            Some(c) => c as usize,
            None => return false,
        };
        let next_field = ifd + 2 + count * 12;
        self.u32_at(next_field).map(|v| v != 0).unwrap_or(false)
    }
}

/// The EXIF statements the signals read, gathered in one walk.
#[derive(Default)]
struct ExifFacts {
    present: bool,
    make: Option<String>,
    model: Option<String>,
    software: Option<String>,
    datetime: Option<String>,
    datetime_original: Option<String>,
    pixel_dims: Option<(u64, u64)>,
    gps: Option<(f64, f64)>,
    has_thumbnail_ifd: bool,
}

fn dms_to_decimal(parts: &[(u32, u32)]) -> Option<f64> {
    if parts.len() < 3 {
        return None;
    }
    let val = |(n, d): (u32, u32)| -> Option<f64> {
        if d == 0 {
            None
        } else {
            Some(n as f64 / d as f64)
        }
    };
    Some(val(parts[0])? + val(parts[1])? / 60.0 + val(parts[2])? / 3600.0)
}

fn exif_facts(tiff: &Tiff) -> ExifFacts {
    let mut facts = ExifFacts {
        present: true,
        ..Default::default()
    };
    let Some(ifd0) = tiff.ifd0_offset() else {
        return facts;
    };
    facts.make = tiff.ascii(ifd0, 0x010F);
    facts.model = tiff.ascii(ifd0, 0x0110);
    facts.software = tiff.ascii(ifd0, 0x0131);
    facts.datetime = tiff.ascii(ifd0, 0x0132);
    facts.has_thumbnail_ifd = tiff.has_next_ifd(ifd0);
    if let Some(exif_ifd) = tiff.sub_ifd(ifd0, 0x8769) {
        facts.datetime_original = tiff.ascii(exif_ifd, 0x9003);
        if let (Some(w), Some(h)) = (
            tiff.numeric(exif_ifd, 0xA002),
            tiff.numeric(exif_ifd, 0xA003),
        ) {
            facts.pixel_dims = Some((w, h));
        }
    }
    if let Some(gps_ifd) = tiff.sub_ifd(ifd0, 0x8825) {
        let lat_ref = tiff.ascii(gps_ifd, 0x0001);
        let lon_ref = tiff.ascii(gps_ifd, 0x0003);
        if let (Some(lat_parts), Some(lon_parts)) = (
            tiff.rationals(gps_ifd, 0x0002),
            tiff.rationals(gps_ifd, 0x0004),
        ) {
            if let (Some(mut lat), Some(mut lon)) =
                (dms_to_decimal(&lat_parts), dms_to_decimal(&lon_parts))
            {
                if lat_ref.as_deref() == Some("S") {
                    lat = -lat;
                }
                if lon_ref.as_deref() == Some("W") {
                    lon = -lon;
                }
                facts.gps = Some((crate::round6(lat), crate::round6(lon)));
            }
        }
    }
    facts
}

/// Locate the TIFF for each container, trying the same
/// candidates in the same order as `ingest::exif_capture_time`.
fn tiff_for(data: &[u8], format: PhotoFormat) -> Option<Vec<u8>> {
    match format {
        PhotoFormat::Jpeg => {
            let walk = jpeg_walk(data)?;
            for seg in &walk.segments {
                if seg.marker == 0xE1 {
                    let payload = &data[seg.payload.clone()];
                    if let Some(tiff) = payload.strip_prefix(b"Exif\0\0") {
                        return Some(tiff.to_vec());
                    }
                }
            }
            None
        }
        PhotoFormat::Png => {
            let walk = png_walk(data)?;
            for chunk in &walk.chunks {
                if &chunk.kind == b"eXIf" {
                    return Some(data[chunk.payload.clone()].to_vec());
                }
            }
            None
        }
        PhotoFormat::Heic => crate::ingest::heic_exif_tiff_candidates(data)
            .into_iter()
            .find(|candidate| Tiff::new(candidate).is_some()),
        PhotoFormat::Unknown => None,
    }
}

// ---------------------------------------------------------------------------
// XMP: locate the packet per container, then extract the fields
// with a bounded text scan (the dive's sanctioned approach — the
// packet is a self-reported statement; we read its claims, we do
// not need an XML parser's opinion of its well-formedness).
// ---------------------------------------------------------------------------

const XMP_HEADER: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const XMP_EXT_HEADER: &[u8] = b"http://ns.adobe.com/xmp/extension/\0";
const XMP_PACKET_CAP: usize = 1024 * 1024;

/// The XMP packets in a file: the main packet, then any extended
/// packet segments in order (Photoshop splits large packets; the
/// edit history can live in the extension).
fn xmp_packets(data: &[u8], format: PhotoFormat) -> Vec<Vec<u8>> {
    let mut packets: Vec<Vec<u8>> = Vec::new();
    match format {
        PhotoFormat::Jpeg => {
            if let Some(walk) = jpeg_walk(data) {
                let mut extensions: Vec<Vec<u8>> = Vec::new();
                for seg in &walk.segments {
                    if seg.marker != 0xE1 {
                        continue;
                    }
                    let payload = &data[seg.payload.clone()];
                    if let Some(packet) = payload.strip_prefix(XMP_HEADER) {
                        packets.push(packet.to_vec());
                    } else if let Some(rest) = payload.strip_prefix(XMP_EXT_HEADER) {
                        // 32-byte GUID + 8-byte full length + 8-byte
                        // offset precede the packet bytes.
                        if let Some(packet) = rest.get(48..) {
                            extensions.push(packet.to_vec());
                        }
                    }
                }
                packets.extend(extensions);
            }
        }
        PhotoFormat::Png => {
            if let Some(walk) = png_walk(data) {
                for chunk in &walk.chunks {
                    let payload = &data[chunk.payload.clone()];
                    if &chunk.kind == b"iTXt" {
                        // keyword \0, compression flag, compression
                        // method, language \0, translated \0, text.
                        let Some(nul) = payload.iter().position(|&b| b == 0) else {
                            continue;
                        };
                        if &payload[..nul] != b"XML:com.adobe.xmp" {
                            continue;
                        }
                        let Some(rest) = payload.get(nul + 3..) else {
                            continue;
                        };
                        let Some(lang_end) = rest.iter().position(|&b| b == 0) else {
                            continue;
                        };
                        let Some(rest) = rest.get(lang_end + 1..) else {
                            continue;
                        };
                        let Some(tr_end) = rest.iter().position(|&b| b == 0) else {
                            continue;
                        };
                        let Some(text) = rest.get(tr_end + 1..) else {
                            continue;
                        };
                        let compressed = payload.get(nul + 1) == Some(&1);
                        if compressed {
                            use std::io::Read;
                            let mut out = Vec::new();
                            if flate2::read::ZlibDecoder::new(text)
                                .take(XMP_PACKET_CAP as u64)
                                .read_to_end(&mut out)
                                .is_ok()
                            {
                                packets.push(out);
                            }
                        } else {
                            packets.push(text.to_vec());
                        }
                    } else if &chunk.kind == b"tEXt" {
                        if let Some(nul) = payload.iter().position(|&b| b == 0) {
                            if &payload[..nul] == b"XML:com.adobe.xmp" {
                                packets.push(payload[nul + 1..].to_vec());
                            }
                        }
                    }
                }
            }
        }
        PhotoFormat::Heic => {
            if let Ok(heif) = crate::heic::container::parse(data) {
                for item in heif.items.iter().filter(|i| i.item_type == "mime") {
                    if item.total_extent_bytes() > XMP_PACKET_CAP as u64 {
                        continue;
                    }
                    let Ok(bytes) =
                        crate::heic::container::extract_item_data_for(data, item)
                    else {
                        continue;
                    };
                    // The packet may begin directly or behind the
                    // MIME content-type token; find its start.
                    let text = String::from_utf8_lossy(&bytes);
                    if let Some(start) = text.find("<x:xmpmeta").or_else(|| text.find("<?xpacket")) {
                        packets.push(text[start..].as_bytes().to_vec());
                    }
                }
            }
        }
        PhotoFormat::Unknown => {}
    }
    packets
}

fn xml_unescape(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn clean_field(text: &str) -> Option<String> {
    let text = xml_unescape(text).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text.chars().take(200).collect())
    }
}

/// All values of an attribute `name="..."` in document order.
fn attr_values(packet: &str, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = packet;
    while let Some(pos) = rest.find(name) {
        let after = &rest[pos + name.len()..];
        let after = after.trim_start();
        let Some(after) = after.strip_prefix('=') else {
            rest = &rest[pos + name.len()..];
            continue;
        };
        let after = after.trim_start();
        let quote = match after.chars().next() {
            Some(q @ ('"' | '\'')) => q,
            _ => {
                rest = &rest[pos + name.len()..];
                continue;
            }
        };
        let body = &after[1..];
        match body.find(quote) {
            Some(end) => {
                if let Some(value) = clean_field(&body[..end]) {
                    out.push(value);
                }
                rest = &body[end..];
            }
            None => break,
        }
    }
    out
}

/// All values of an element `<name>...</name>` in document order.
fn elem_values(packet: &str, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let open = format!("<{name}");
    let close = format!("</{name}>");
    let mut rest = packet;
    while let Some(pos) = rest.find(&open) {
        let after = &rest[pos + open.len()..];
        let Some(gt) = after.find('>') else { break };
        if after[..gt].ends_with('/') {
            rest = &after[gt + 1..];
            continue;
        }
        let body = &after[gt + 1..];
        match body.find(&close) {
            Some(end) => {
                if let Some(value) = clean_field(&body[..end]) {
                    out.push(value);
                }
                rest = &body[end + close.len()..];
            }
            None => break,
        }
    }
    out
}

fn first_field(packet: &str, name: &str) -> Option<String> {
    attr_values(packet, name)
        .into_iter()
        .next()
        .or_else(|| elem_values(packet, name).into_iter().next())
}

fn all_fields(packet: &str, name: &str) -> Vec<String> {
    let attrs = attr_values(packet, name);
    if !attrs.is_empty() {
        attrs
    } else {
        elem_values(packet, name)
    }
}

#[derive(Default)]
struct XmpFacts {
    present: bool,
    creator_tool: Option<String>,
    document_id: Option<String>,
    instance_id: Option<String>,
    history: Vec<(Option<String>, Option<String>, Option<String>)>, // (action, when, software agent)
}

fn xmp_facts(packets: &[Vec<u8>]) -> XmpFacts {
    let mut facts = XmpFacts::default();
    for packet_bytes in packets {
        let packet = String::from_utf8_lossy(packet_bytes);
        if !facts.present {
            facts.present = true;
            facts.creator_tool = first_field(&packet, "xmp:CreatorTool");
            facts.document_id = first_field(&packet, "xmpMM:DocumentID");
            facts.instance_id = first_field(&packet, "xmpMM:InstanceID");
        }
        if facts.history.is_empty() {
            let actions = all_fields(&packet, "stEvt:action");
            if !actions.is_empty() {
                let whens = all_fields(&packet, "stEvt:when");
                let agents = all_fields(&packet, "stEvt:softwareAgent");
                let n = actions.len().min(32);
                for i in 0..n {
                    facts.history.push((
                        actions.get(i).cloned(),
                        whens.get(i).cloned(),
                        agents.get(i).cloned(),
                    ));
                }
            }
        }
        if facts.creator_tool.is_some() && !facts.history.is_empty() {
            break;
        }
    }
    facts
}

// ---------------------------------------------------------------------------
// C2PA presence (item 5 of the dive, presence read only). We find
// the manifest's container; we do NOT open it, parse its claims,
// or check its signature — and the signal says exactly that.
// ---------------------------------------------------------------------------

fn c2pa_present(data: &[u8], format: PhotoFormat) -> bool {
    match format {
        PhotoFormat::Jpeg => match jpeg_walk(data) {
            Some(walk) => walk.segments.iter().any(|seg| {
                seg.marker == 0xEB
                    && data[seg.payload.clone()]
                        .windows(4)
                        .take(1024)
                        .any(|w| w == b"c2pa")
            }),
            None => false,
        },
        PhotoFormat::Png => match png_walk(data) {
            Some(walk) => walk.chunks.iter().any(|c| &c.kind == b"caBX"),
            None => false,
        },
        PhotoFormat::Heic => {
            let (boxes, _) = top_level_boxes(data);
            let box_hit = boxes.iter().any(|b| {
                &b.kind == b"jumb"
                    || (&b.kind == b"uuid"
                        && data[b.payload.clone()]
                            .windows(4)
                            .take(4096)
                            .any(|w| w == b"c2pa"))
            });
            if box_hit {
                return true;
            }
            match crate::heic::container::parse(data) {
                Ok(heif) => heif.items.iter().any(|i| i.item_type == "c2pa"),
                Err(_) => false,
            }
        }
        PhotoFormat::Unknown => false,
    }
}

// ---------------------------------------------------------------------------
// Container census + consistency
// ---------------------------------------------------------------------------

fn marker_name(marker: u8) -> String {
    match marker {
        0xE0..=0xEF => format!("APP{}", marker - 0xE0),
        0xDB => "DQT".to_string(),
        0xC4 => "DHT".to_string(),
        0xC0 => "SOF0".to_string(),
        0xC2 => "SOF2".to_string(),
        0xDA => "SOS".to_string(),
        0xFE => "COM".to_string(),
        0xDD => "DRI".to_string(),
        other => format!("FF{other:02X}"),
    }
}

fn jpeg_sof_dims(data: &[u8], walk: &JpegWalk) -> Option<(u64, u64)> {
    for seg in &walk.segments {
        if seg.marker == 0xC0 || seg.marker == 0xC2 {
            let p = &data[seg.payload.clone()];
            if p.len() >= 5 {
                let h = u16::from_be_bytes([p[1], p[2]]) as u64;
                let w = u16::from_be_bytes([p[3], p[4]]) as u64;
                return Some((w, h));
            }
        }
    }
    None
}

fn census(
    data: &[u8],
    format: PhotoFormat,
    exif: &ExifFacts,
    xmp_present: bool,
) -> (Option<Value>, usize) {
    match format {
        PhotoFormat::Jpeg => {
            let Some(walk) = jpeg_walk(data) else {
                return (None, 0);
            };
            if walk.segments.is_empty() {
                // A signature with no readable segment is a
                // truncated file, not a read container.
                return (None, 0);
            }
            let mut segments = Map::new();
            for seg in &walk.segments {
                let name = marker_name(seg.marker);
                let entry = segments.entry(name).or_insert(json!(0));
                *entry = json!(entry.as_u64().unwrap_or(0) + 1);
            }
            let dims = jpeg_sof_dims(data, &walk);
            (
                Some(json!({
                    "container": "jpeg",
                    "segments": segments,
                    "has_exif": exif.present,
                    "has_xmp": xmp_present,
                    "has_embedded_thumbnail": exif.has_thumbnail_ifd,
                    "stated_dimensions": dims.map(|(w, h)| json!([w, h])),
                })),
                walk.trailing_bytes,
            )
        }
        PhotoFormat::Png => {
            let Some(walk) = png_walk(data) else {
                return (None, 0);
            };
            if walk.chunks.is_empty() {
                return (None, 0);
            }
            let mut chunks = Map::new();
            for chunk in &walk.chunks {
                let name = String::from_utf8_lossy(&chunk.kind).into_owned();
                let entry = chunks.entry(name).or_insert(json!(0));
                *entry = json!(entry.as_u64().unwrap_or(0) + 1);
            }
            let dims = crate::image::png_dimensions(data);
            (
                Some(json!({
                    "container": "png",
                    "chunks": chunks,
                    "has_exif": walk.chunks.iter().any(|c| &c.kind == b"eXIf"),
                    "has_xmp": xmp_present,
                    "has_embedded_thumbnail": false,
                    "stated_dimensions": dims.map(|(w, h)| json!([w, h])),
                })),
                walk.trailing_bytes,
            )
        }
        PhotoFormat::Heic => {
            let Ok(heif) = crate::heic::container::parse(data) else {
                return (None, 0);
            };
            let (_, trailing) = top_level_boxes(data);
            let mut by_type = Map::new();
            for (kind, count) in heif.census() {
                by_type.insert(kind, json!(count));
            }
            let primary = heif.primary_item();
            let (primary_type, primary_dims) = match primary {
                Some(item) => (
                    json!(item.item_type),
                    heif.item_ispe(item).map(|(w, h)| json!([w, h])),
                ),
                None => (Value::Null, None),
            };
            let image_tiles = primary
                .map(|item| {
                    item.references
                        .iter()
                        .filter(|r| r.ref_type == "dimg")
                        .map(|r| r.to_item_ids.len())
                        .sum::<usize>()
                })
                .unwrap_or(0);
            let thumbnail_present = heif.items.iter().any(|item| {
                item.references.iter().any(|r| r.ref_type == "thmb")
            }) || heif
                .items
                .iter()
                .any(|i| i.references.iter().any(|r| r.ref_type == "thmb"));
            let mut aux_types: Vec<String> = Vec::new();
            for item in &heif.items {
                if let Some(aux) = heif.item_aux_type(item) {
                    if !aux_types.contains(&aux) {
                        aux_types.push(aux);
                    }
                }
            }
            let depth_present = aux_types.iter().any(|a| a.contains("depth"));
            (
                Some(json!({
                    "container": "heic",
                    "major_brand": heif.major_brand,
                    "items_total": heif.items.len(),
                    "items_by_type": by_type,
                    "primary_item_type": primary_type,
                    "stated_dimensions": primary_dims,
                    "image_tiles": image_tiles,
                    "has_exif": exif.present,
                    "has_xmp": xmp_present,
                    "has_embedded_thumbnail": thumbnail_present,
                    "aux_item_types": aux_types,
                    "depth_item_present": depth_present,
                    "hidden_items": heif.items.iter().filter(|i| i.hidden).count(),
                })),
                trailing,
            )
        }
        PhotoFormat::Unknown => (None, 0),
    }
}

// ---------------------------------------------------------------------------
// The block
// ---------------------------------------------------------------------------

fn signal(
    id: &str,
    finding: &str,
    detail: String,
    establishes: &str,
    does_not_establish: &str,
    extra: Option<Value>,
) -> Value {
    let mut out = json!({
        "signal": id,
        "finding": finding,
        "detail": detail,
        "establishes": establishes,
        "does_not_establish": does_not_establish,
    });
    if let Some(extra) = extra {
        if let (Some(base), Some(add)) = (out.as_object_mut(), extra.as_object()) {
            for (k, v) in add {
                base.insert(k.clone(), v.clone());
            }
        }
    }
    out
}

fn plausible_datetime(s: &str) -> bool {
    let b = s.as_bytes();
    s.len() >= 19
        && b[..4].iter().all(|c| c.is_ascii_digit())
        && b[4] == b':'
        && b[7] == b':'
        && b[10] == b' '
        && b[13] == b':'
        && b[16] == b':'
}

/// Build the provenance signals block for one photo file.
/// `decoded_dims` is the size of the image as decoded at ingest
/// (after orientation is applied); it feeds only the dimensions
/// consistency comparison, whose wording allows the stored /
/// displayed swap orientation produces.
pub fn provenance_block(data: &[u8], decoded_dims: Option<(u32, u32)>) -> Value {
    let format = detect_format(data);
    let tiff = tiff_for(data, format).and_then(|bytes| {
        let owned = bytes;
        // Parse from a leaked-free local: Tiff borrows, so run the
        // facts walk inside this scope and keep the facts.
        Tiff::new(&owned).map(|t| exif_facts(&t))
    });
    let exif = tiff.unwrap_or_default();
    let packets = xmp_packets(data, format);
    let xmp = xmp_facts(&packets);
    let (census_json, trailing) = census(data, format, &exif, xmp.present);
    let metadata_read = census_json.is_some() || exif.present || xmp.present;

    let mut signals: Vec<Value> = Vec::new();

    // Cameras write the EXIF Software tag too, and what they
    // write is a bare OS version ("17.0"), not an editor. A
    // version-only value is the capture system's version: it is
    // reported on the capture_device signal, never as an editor
    // name (see the editor_software signal).
    let version_like = |s: &str| -> bool {
        !s.is_empty()
            && s.chars().all(|c| c.is_ascii_digit() || c == '.')
            && s.chars().any(|c| c.is_ascii_digit())
    };
    let capture_os_version = exif
        .software
        .as_ref()
        .filter(|sw| version_like(sw))
        .cloned();

    // Capture device.
    match (&exif.make, &exif.model) {
        (None, None) => signals.push(signal(
            "capture_device",
            "not_stated",
            "the file names no capture device in its metadata".to_string(),
            "nothing; most files that pass through editing or messaging tools lose these tags",
            "a missing camera name is not evidence about the photo either way",
            capture_os_version
                .clone()
                .map(|v| json!({"capture_software_version": v})),
        )),
        (make, model) => {
            let named = [make.clone(), model.clone()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            signals.push(signal(
                "capture_device",
                "stated",
                format!("the file names its capture device as {named}"),
                "the file's own statement of what captured it",
                "metadata is written by software and can be edited or stripped, so a named device does not prove the photo came from that device",
                Some(json!({"make": make, "model": model, "capture_software_version": capture_os_version})),
            ));
        }
    }

    // Editor software named anywhere (EXIF Software + XMP
    // CreatorTool), with the capture-OS-version trap filtered
    // out above.
    let mut editors: Vec<String> = Vec::new();
    if let Some(sw) = &exif.software {
        if !version_like(sw) {
            editors.push(sw.clone());
        }
    }
    if let Some(tool) = &xmp.creator_tool {
        if !editors.contains(tool) {
            editors.push(tool.clone());
        }
    }
    if editors.is_empty() {
        signals.push(signal(
            "editor_software",
            "none_named",
            "no editing software is named anywhere in the file's metadata".to_string(),
            "nothing; this is the expected state of an untouched camera file",
            "silence is not cleanliness: an edited file whose metadata was stripped reads exactly like this, so this line can never clear a file",
            None,
        ));
    } else {
        signals.push(signal(
            "editor_software",
            "named",
            format!(
                "the file's metadata names this software: {}",
                editors.join("; ")
            ),
            "at least one metadata system in the file states that this software wrote or processed it; that is a trace the tools recorded themselves",
            "naming software never says which pixels changed, if any; plenty of named software only exports, resizes, or catalogs",
            Some(json!({"values": editors})),
        ));
    }

    // XMP edit history.
    if xmp.history.is_empty() {
        signals.push(signal(
            "edit_history",
            "none_recorded",
            "no edit history is recorded in the file".to_string(),
            "nothing; most camera files carry no history packet at all",
            "an edit history appears only when Adobe-family tools wrote one and the packet survived; its absence says nothing about edits by other tools or after a strip",
            None,
        ));
    } else {
        let shown: Vec<String> = xmp
            .history
            .iter()
            .take(8)
            .map(|(action, when, agent)| {
                let mut parts = vec![action.clone().unwrap_or_else(|| "event".to_string())];
                if let Some(agent) = agent {
                    parts.push(format!("by {agent}"));
                }
                if let Some(when) = when {
                    parts.push(format!("at {when}"));
                }
                parts.join(" ")
            })
            .collect();
        let entries: Vec<Value> = xmp
            .history
            .iter()
            .map(|(action, when, agent)| {
                json!({"action": action, "when": when, "software_agent": agent})
            })
            .collect();
        signals.push(signal(
            "edit_history",
            "recorded",
            format!(
                "{} recorded event(s): {}",
                xmp.history.len(),
                shown.join("; ")
            ),
            "Adobe-family tools wrote this chain as the file moved through them; a recorded chain is a genuine self-report of those steps",
            "the chain covers only what Adobe-family tools chose to record while the packet survived; steps by other tools, or after a metadata strip, leave no trace here",
            Some(json!({"entries": entries})),
        ));
    }

    // XMP document identifiers.
    match (&xmp.document_id, &xmp.instance_id) {
        (None, None) => signals.push(signal(
            "document_ids",
            "absent",
            "no XMP document identifiers are present".to_string(),
            "nothing",
            "identifiers speak only to whether copies can be recognised as related; their absence is the normal state of camera files",
            None,
        )),
        (doc, inst) => signals.push(signal(
            "document_ids",
            "present",
            "the file carries XMP document identifiers".to_string(),
            "derivative copies carrying the same identifiers can be recognised as related to this file",
            "identifiers say nothing about whether any pixel was edited",
            Some(json!({"document_id": doc, "instance_id": inst})),
        )),
    }

    // Date consistency (weak by design; wording carries that).
    let (dt, dto) = (&exif.datetime, &exif.datetime_original);
    let date_signal = match (dt, dto) {
        (Some(a), Some(b)) if a == b => signal(
            "date_consistency",
            "agree",
            format!("the file's modification date and stated capture time agree ({a})"),
            "the file's two dates tell one consistent story",
            "agreement is easy to preserve or to fake, and exports often rewrite both together; this line can never clear a file",
            None,
        ),
        (Some(a), Some(b)) => {
            let (finding, detail, establishes) = if plausible_datetime(a)
                && plausible_datetime(b)
                && a.as_str() > b.as_str()
            {
                (
                    "modified_after_capture",
                    format!(
                        "the file states a modification time ({a}) later than its capture time ({b})"
                    ),
                    "the file's own dates say it was written again after the stated capture moment",
                )
            } else {
                (
                    "out_of_order",
                    format!(
                        "the file's dates do not order as expected: modification {a}, capture {b}"
                    ),
                    "the file's own dates are inconsistent with each other",
                )
            };
            signal(
                "date_consistency",
                finding,
                detail,
                establishes,
                "exports, AirDrop transfers, and photo-library round-trips legitimately rewrite a file and bump this date without changing a pixel; this line alone never shows an edit",
                None,
            )
        }
        (Some(only), None) | (None, Some(only)) => signal(
            "date_consistency",
            "single_date_stated",
            format!("the file states one date ({only}); there is no second date to compare it with"),
            "nothing beyond the date itself",
            "a single date cannot be checked for consistency, and dates are as editable as any other metadata",
            None,
        ),
        (None, None) => signal(
            "date_consistency",
            "no_dates_stated",
            "the file states no capture or modification date".to_string(),
            "nothing",
            "missing dates are common after exports and strips, and say nothing about the photo",
            None,
        ),
    };
    signals.push(date_signal);

    // Content Credentials presence (presence only, never a
    // validation claim).
    if c2pa_present(data, format) {
        signals.push(signal(
            "content_credentials",
            "present",
            "a Content Credentials (C2PA) manifest is embedded in the file. This build reports its presence only: the signature was not checked against a trust list, so the manifest's claims are recorded as unverified".to_string(),
            "an embedded manifest is a signed statement bound to the file by whichever device or tool signed it; presence is a fact about the file",
            "an unverified manifest proves nothing about the truth of its claims, and a verified one would still prove only that its signer said them",
            None,
        ));
    } else {
        signals.push(signal(
            "content_credentials",
            "absent",
            "no Content Credentials (C2PA) manifest is embedded. Most cameras add none, and no native iPhone photo carries one, so this absence means nothing".to_string(),
            "nothing; absence of credentials is the normal state of most honest photos",
            "absence must never be read as suspicion: the large majority of unedited photos on earth carry no credential",
            None,
        ));
    }

    // Dimensions consistency (metadata-stated vs decoded).
    match (exif.pixel_dims, decoded_dims) {
        (Some((w, h)), Some((dw, dh))) => {
            let consistent = (w, h) == (dw as u64, dh as u64) || (w, h) == (dh as u64, dw as u64);
            if consistent {
                signals.push(signal(
                    "dimensions_consistency",
                    "consistent",
                    format!(
                        "the metadata states {w}x{h} and the decoded image matches (orientation may swap the pair)"
                    ),
                    "the file's description of its size agrees with the image it contains",
                    "agreement is the normal state of untouched and competently exported files alike",
                    None,
                ));
            } else {
                signals.push(signal(
                    "dimensions_consistency",
                    "mismatch",
                    format!(
                        "the metadata states {w}x{h} but the decoded image is {dw}x{dh}"
                    ),
                    "the file's stated dimensions disagree with the image it actually contains, which happens when a file was re-encoded or re-framed after its metadata was written",
                    "a mismatch does not show the picture itself was altered; editors and exporters rewrite dimensions routinely",
                    None,
                ));
            }
        }
        _ => signals.push(signal(
            "dimensions_consistency",
            "not_stated",
            "the metadata states no pixel dimensions, so there is nothing to compare with the decoded image".to_string(),
            "nothing",
            "most formats state dimensions in their structure rather than their metadata; this check simply has nothing to bite on here",
            None,
        )),
    }

    // Trailing data after the image structure.
    if trailing > 0 {
        signals.push(signal(
            "trailing_data",
            "present",
            format!("{trailing} byte(s) follow the end of the image data"),
            "data sits outside the image structure, a place extra content can be hidden, and also a place some exporters park their own bookkeeping",
            "trailing bytes are common in honest exports and prove nothing on their own",
            Some(json!({"bytes": trailing})),
        ));
    } else {
        signals.push(signal(
            "trailing_data",
            "none",
            "no bytes follow the end of the image data".to_string(),
            "the file ends where its structure says it ends",
            "a clean ending says nothing about the image data itself",
            None,
        ));
    }

    // Location: structured, with the plain-language warning.
    let (location, location_warning) = match exif.gps {
        Some((lat, lon)) => {
            let warning = format!(
                "This photo's file carries GPS location coordinates (latitude {lat}, longitude {lon}), readable by anyone who receives the original file. Before the original is published anywhere public, for example on the original-photo page an AB 723 listing links to, publish the metadata-free public copy instead: it keeps every pixel and removes the location and the rest of the embedded metadata. The original file and its receipt are unchanged."
            );
            (
                json!({
                    "present": true,
                    "latitude": lat,
                    "longitude": lon,
                    "warning": warning,
                    "public_copy": Value::Null,
                }),
                json!(warning),
            )
        }
        None => (
            json!({
                "present": false,
                "latitude": Value::Null,
                "longitude": Value::Null,
                "warning": Value::Null,
                "public_copy": Value::Null,
            }),
            Value::Null,
        ),
    };

    json!({
        "version": 1,
        "format": format.name(),
        "metadata_read": metadata_read,
        "scope": SIGNALS_SCOPE,
        "signals": signals,
        "census": census_json,
        "location": location,
        "location_warning": location_warning,
    })
}

/// Patch the public-copy reference into a block's location
/// section after the caller has minted the copy into the store.
/// `info` must carry asset / sha256 / pixel_sha256 / format /
/// label; the block is otherwise untouched.
pub fn set_public_copy(block: &mut Value, info: Value) {
    if let Some(location) = block.get_mut("location").and_then(Value::as_object_mut) {
        location.insert("public_copy".to_string(), info);
    }
}

/// Record that a public copy could not be produced (the warning
/// still stands; the record must not pretend an offer exists).
pub fn set_public_copy_unavailable(block: &mut Value, reason: &str) {
    if let Some(location) = block.get_mut("location").and_then(Value::as_object_mut) {
        location.insert(
            "public_copy".to_string(),
            json!({"unavailable": reason}),
        );
    }
}

// ---------------------------------------------------------------------------
// The metadata-free public copy. JPEG and PNG copies are pure
// container surgery: the image data moves byte for byte, and only
// the segments/chunks on an explicit ALLOWLIST (decode and colour
// rendering needs) are kept; everything else, known metadata
// carrier or not, is dropped (fix pass F1/F2: the old denylist let
// C2PA, Photoshop IRB, private chunks and inter-scan APPn through). HEIC has no byte-level
// surgery available (the metadata lives in its own item inside a
// box structure we decode but do not rewrite), so its copy is a
// lossless decode re-encoded as PNG with no metadata at all; the
// pixels are identical, which the caller proves with the pixel
// hash. Either way the copy is a DERIVED asset: the original is
// never touched.
// ---------------------------------------------------------------------------

/// Strip all metadata from a PNG: every text chunk (the XMP and
/// comment carriers), the EXIF chunk, and the timestamp chunk go;
/// image and colour chunks move across byte for byte.
fn strip_png(data: &[u8]) -> Option<Vec<u8>> {
    let walk = png_walk(data)?;
    let mut out = Vec::with_capacity(data.len());
    out.extend_from_slice(&data[..8]);
    for chunk in &walk.chunks {
        match &chunk.kind {
            b"eXIf" | b"iTXt" | b"zTXt" | b"tEXt" | b"tIME" => continue,
            _ => out.extend_from_slice(&data[chunk.span.clone()]),
        }
    }
    Some(out)
}

/// Is a JPEG marker segment allowed in the public copy? An
/// ALLOWLIST: anything not named here (every other APPn, COM, and
/// any marker we do not recognise) is dropped.
///
/// Before the first scan the copy keeps only what decoding and
/// colour rendering need: quantisation and Huffman tables (DQT,
/// DHT), arithmetic-coding conditioning (DAC), restart interval
/// (DRI), the frame header (SOFn, 0xC0-0xCF minus the DHT / JPG /
/// DAC codes), JFIF/JFXX in APP0, the ICC profile in APP2 (colour
/// rendering, not provenance) and the Adobe colour-transform flag
/// in APP14. Between scans (progressive and multi-scan files) only
/// table segments (DHT, DQT, DAC, DRI, DNL) survive: every APPn
/// and COM there is dropped too.
fn jpeg_segment_allowed(marker: u8, payload: &[u8], after_first_scan: bool) -> bool {
    match marker {
        0xDB | 0xC4 | 0xCC | 0xDD => true, // DQT, DHT, DAC, DRI
        0xDC => after_first_scan,          // DNL only follows a scan
        _ if after_first_scan => false,
        0xC0..=0xCF => marker != 0xC8, // SOFn (0xC4 / 0xCC matched above; 0xC8 is JPG, reserved)
        0xE0 => payload.starts_with(b"JFIF\0") || payload.starts_with(b"JFXX\0"),
        0xE2 => payload.starts_with(b"ICC_PROFILE\0"),
        0xEE => payload.starts_with(b"Adobe"),
        _ => false,
    }
}

/// The JPEG public copy: an allowlist walk over the whole stream,
/// scans included. Segments are kept or dropped by
/// [`jpeg_segment_allowed`]; each scan's entropy-coded data moves
/// byte for byte (0xFF00 stuffing and RST0-7 are data, not
/// markers); the walk ends at EOI, which is emitted, and anything
/// after it (trailing data) is dropped. A stream that ends inside
/// scan data keeps the data it has and gains an EOI.
fn strip_jpeg(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out: Vec<u8> = Vec::with_capacity(data.len());
    out.extend_from_slice(&[0xFF, 0xD8]);
    let mut pos = 2usize;
    let mut after_first_scan = false;
    loop {
        if pos >= data.len() {
            // Ran out without an EOI: close the stream.
            if !after_first_scan {
                return Err("JPEG has no SOS segment".to_string());
            }
            out.extend_from_slice(&[0xFF, 0xD9]);
            return Ok(out);
        }
        if data[pos] != 0xFF {
            return Err(format!("JPEG marker expected at byte {pos}"));
        }
        // Fill bytes: any run of 0xFF before the marker code.
        while pos + 1 < data.len() && data[pos + 1] == 0xFF {
            pos += 1;
        }
        if pos + 1 >= data.len() {
            pos = data.len();
            continue;
        }
        let marker = data[pos + 1];
        match marker {
            0xD9 => {
                if !after_first_scan {
                    return Err("JPEG has no SOS segment".to_string());
                }
                out.extend_from_slice(&[0xFF, 0xD9]);
                return Ok(out);
            }
            // Standalone markers outside scan data (TEM, a stray
            // RSTn, a repeated SOI): no payload, not needed, dropped.
            0x01 | 0xD0..=0xD8 => {
                pos += 2;
                continue;
            }
            _ => {}
        }
        if pos + 4 > data.len() {
            return Err("JPEG segment header is truncated".to_string());
        }
        let seg_len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        if seg_len < 2 || pos + 2 + seg_len > data.len() {
            return Err("JPEG segment runs past the end of the file".to_string());
        }
        let seg_end = pos + 2 + seg_len;
        if marker == 0xDA {
            // SOS header, then its entropy-coded data up to the next
            // real marker.
            out.extend_from_slice(&data[pos..seg_end]);
            let mut i = seg_end;
            while i < data.len() {
                if data[i] == 0xFF {
                    if i + 1 >= data.len() {
                        break;
                    }
                    match data[i + 1] {
                        0x00 | 0xD0..=0xD7 => {
                            i += 2;
                            continue;
                        }
                        _ => break,
                    }
                }
                i += 1;
            }
            out.extend_from_slice(&data[seg_end..i]);
            after_first_scan = true;
            pos = i;
            continue;
        }
        if jpeg_segment_allowed(marker, &data[pos + 4..seg_end], after_first_scan) {
            out.extend_from_slice(&data[pos..seg_end]);
        }
        pos = seg_end;
    }
}

pub fn metadata_free_public_copy(data: &[u8]) -> Result<(Vec<u8>, &'static str), String> {
    match detect_format(data) {
        PhotoFormat::Jpeg => {
            let walk = jpeg_walk(data).ok_or("not a readable JPEG")?;
            if !walk.has_scan {
                return Err("JPEG carries no image scan".to_string());
            }
            Ok((strip_jpeg(data)?, "jpeg"))
        }
        PhotoFormat::Png => strip_png(data)
            .map(|bytes| (bytes, "png"))
            .ok_or_else(|| "not a readable PNG".to_string()),
        PhotoFormat::Heic => {
            let decoded = crate::image::decode_image(data)
                .map_err(|e| format!("HEIC copy needs a decode: {e}"))?;
            let mut rgba = match decoded {
                crate::image::DecodedImage::Rgba(img) => img,
                crate::image::DecodedImage::Gray(g) => g.to_rgba(),
            };
            rgba.icc_profile = None;
            Ok((crate::image::encode_png_rgba(&rgba), "png"))
        }
        PhotoFormat::Unknown => Err("unrecognised image format".to_string()),
    }
}

/// Mint the metadata-free public copy of `data` into the store
/// and build the reference the block carries: the copy's handle,
/// its file and pixel hashes, its format, and the label that
/// must travel with it. The pixel hash is recomputed from the
/// copy's own decoded pixels, so "pixels identical" is a checked
/// equality against the original's pixel hash, not a promise.
pub fn mint_public_copy(
    store: &crate::assets::AssetStore,
    data: &[u8],
    parent_asset_id: Option<&str>,
) -> Result<Value, String> {
    let (bytes, format) = metadata_free_public_copy(data)?;
    let suffix = if format == "jpeg" { ".jpg" } else { ".png" };
    let decoded = crate::image::decode_image(&bytes)
        .map_err(|e| format!("public copy does not decode: {e}"))?;
    let rgba = match decoded {
        crate::image::DecodedImage::Rgba(img) => img,
        crate::image::DecodedImage::Gray(g) => g.to_rgba(),
    };
    let pixel_sha256 = rgba.pixel_sha256();
    let sha256 = crate::image::sha256_hex(&bytes);
    let handle = store.put_bytes(
        &bytes,
        crate::assets::KIND_PUBLIC_COPY,
        suffix,
        parent_asset_id,
    )?;
    Ok(json!({
        "asset": handle,
        "sha256": sha256,
        "pixel_sha256": pixel_sha256,
        "format": format,
        "label": PUBLIC_COPY_LABEL,
    }))
}

/// When a block reports location coordinates, mint its public
/// copy and patch the reference in (or record honestly why no
/// copy could be produced). No-op for location-free blocks.
pub fn attach_public_copy(
    block: &mut Value,
    store: &crate::assets::AssetStore,
    data: &[u8],
    parent_asset_id: Option<&str>,
) {
    if block.pointer("/location/present") != Some(&Value::Bool(true)) {
        return;
    }
    match mint_public_copy(store, data, parent_asset_id) {
        Ok(info) => set_public_copy(block, info),
        Err(e) => set_public_copy_unavailable(block, &e),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Little-endian TIFF with IFD0 {Make, Model, Software,
    /// DateTime, Exif ptr, GPS ptr}, Exif IFD {DateTimeOriginal,
    /// PixelX/Y}, GPS IFD {45 30 0 N, 122 45 0 W}. Built by hand
    /// so offsets are exact.
    fn synthetic_tiff(software: &str, dt_modify: &str, dt_orig: &str) -> Vec<u8> {
        fn ascii_entry(out: &mut Vec<u8>, tag: u16, value_off: u32, len: u32) {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&2u16.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&value_off.to_le_bytes());
        }
        // Layout: header 8; IFD0 at 8 (6 entries: 2 + 72 + 4 = 86,
        // ends 86); Exif IFD at 86 (3 entries: 2+36+4=42, ends
        // 128); GPS IFD at 128 (4 entries: 2+48+4=54, ends 182);
        // strings/rationals after.
        let mut t: Vec<u8> = Vec::new();
        t.extend_from_slice(b"II");
        t.extend_from_slice(&42u16.to_le_bytes());
        t.extend_from_slice(&8u32.to_le_bytes());
        // Strings area starts at 182; assign offsets as we go.
        let mut strings: Vec<u8> = Vec::new();
        let soff = |s: &str, strings: &mut Vec<u8>| -> (u32, u32) {
            let off = 182 + strings.len() as u32;
            strings.extend_from_slice(s.as_bytes());
            strings.push(0);
            (off, s.len() as u32 + 1)
        };
        let (make_off, make_len) = soff("Apple", &mut strings);
        let (model_off, model_len) = soff("iPhone17,2", &mut strings);
        let (sw_off, sw_len) = soff(software, &mut strings);
        let (dt_off, dt_len) = soff(dt_modify, &mut strings);
        let (dto_off, dto_len) = soff(dt_orig, &mut strings);
        // IFD0
        t.extend_from_slice(&6u16.to_le_bytes());
        ascii_entry(&mut t, 0x010F, make_off, make_len);
        ascii_entry(&mut t, 0x0110, model_off, model_len);
        ascii_entry(&mut t, 0x0131, sw_off, sw_len);
        ascii_entry(&mut t, 0x0132, dt_off, dt_len);
        // Exif pointer
        t.extend_from_slice(&0x8769u16.to_le_bytes());
        t.extend_from_slice(&4u16.to_le_bytes());
        t.extend_from_slice(&1u32.to_le_bytes());
        t.extend_from_slice(&86u32.to_le_bytes());
        // GPS pointer
        t.extend_from_slice(&0x8825u16.to_le_bytes());
        t.extend_from_slice(&4u16.to_le_bytes());
        t.extend_from_slice(&1u32.to_le_bytes());
        t.extend_from_slice(&128u32.to_le_bytes());
        t.extend_from_slice(&0u32.to_le_bytes()); // next IFD
        debug_assert_eq!(t.len(), 86);
        // Exif IFD
        t.extend_from_slice(&3u16.to_le_bytes());
        ascii_entry(&mut t, 0x9003, dto_off, dto_len);
        t.extend_from_slice(&0xA002u16.to_le_bytes());
        t.extend_from_slice(&4u16.to_le_bytes());
        t.extend_from_slice(&1u32.to_le_bytes());
        t.extend_from_slice(&4000u32.to_le_bytes());
        t.extend_from_slice(&0xA003u16.to_le_bytes());
        t.extend_from_slice(&4u16.to_le_bytes());
        t.extend_from_slice(&1u32.to_le_bytes());
        t.extend_from_slice(&3000u32.to_le_bytes());
        t.extend_from_slice(&0u32.to_le_bytes());
        debug_assert_eq!(t.len(), 128);
        // GPS IFD: rationals live in the strings area too; compute
        // their offsets now (after the ASCII strings).
        let lat_off = 182 + strings.len() as u32;
        let lon_off = lat_off + 24;
        t.extend_from_slice(&4u16.to_le_bytes());
        ascii_entry(&mut t, 0x0001, 0, 2); // "N\0" inline below
        // fix the inline value: overwrite the last 4 bytes written
        let n = t.len();
        t[n - 4] = b'N';
        t.extend_from_slice(&0x0002u16.to_le_bytes());
        t.extend_from_slice(&5u16.to_le_bytes());
        t.extend_from_slice(&3u32.to_le_bytes());
        t.extend_from_slice(&lat_off.to_le_bytes());
        ascii_entry(&mut t, 0x0003, 0, 2);
        let n = t.len();
        t[n - 4] = b'W';
        t.extend_from_slice(&0x0004u16.to_le_bytes());
        t.extend_from_slice(&5u16.to_le_bytes());
        t.extend_from_slice(&3u32.to_le_bytes());
        t.extend_from_slice(&lon_off.to_le_bytes());
        t.extend_from_slice(&0u32.to_le_bytes());
        debug_assert_eq!(t.len(), 182);
        t.extend_from_slice(&strings);
        for (num, den) in [(45u32, 1u32), (30, 1), (0, 1)] {
            t.extend_from_slice(&num.to_le_bytes());
            t.extend_from_slice(&den.to_le_bytes());
        }
        for (num, den) in [(122u32, 1u32), (45, 1), (0, 1)] {
            t.extend_from_slice(&num.to_le_bytes());
            t.extend_from_slice(&den.to_le_bytes());
        }
        t
    }

    fn jpeg_with_segments(segments: &[(u8, Vec<u8>)], scan: &[u8]) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8];
        for (marker, payload) in segments {
            out.push(0xFF);
            out.push(*marker);
            out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
            out.extend_from_slice(payload);
        }
        // SOS + scan + EOI
        out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]);
        out.extend_from_slice(scan);
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }

    fn exif_app1(tiff: &[u8]) -> Vec<u8> {
        let mut payload = b"Exif\0\0".to_vec();
        payload.extend_from_slice(tiff);
        payload
    }

    fn signal_of<'a>(block: &'a Value, id: &str) -> &'a Value {
        block["signals"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["signal"] == id)
            .unwrap_or_else(|| panic!("signal {id} present"))
    }

    #[test]
    fn gps_rationals_convert_to_decimal() {
        let tiff = synthetic_tiff("17.0", "2025:06:15 12:30:45", "2025:06:15 12:30:45");
        let t = Tiff::new(&tiff).unwrap();
        let facts = exif_facts(&t);
        assert_eq!(facts.gps, Some((45.5, -122.75)));
        assert_eq!(facts.make.as_deref(), Some("Apple"));
        assert_eq!(facts.pixel_dims, Some((4000, 3000)));
    }

    #[test]
    fn jpeg_block_reports_editor_history_dates_and_location() {
        let tiff = synthetic_tiff(
            "Adobe Photoshop 26.0",
            "2026:01:02 03:04:05",
            "2025:06:15 12:30:45",
        );
        let xmp = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF><rdf:Description xmp:CreatorTool="Adobe Photoshop 26.0" xmpMM:DocumentID="xmp.did:1234"><xmpMM:History><rdf:Seq><rdf:li stEvt:action="created" stEvt:when="2025:06:15T12:30:45" stEvt:softwareAgent="Apple Camera"/><rdf:li stEvt:action="saved" stEvt:when="2026:01:02T03:04:05" stEvt:softwareAgent="Adobe Photoshop 26.0"/></rdf:Seq></xmpMM:History></rdf:Description></rdf:RDF></x:xmpmeta>"#;
        let mut xmp_seg = XMP_HEADER.to_vec();
        xmp_seg.extend_from_slice(xmp);
        let jpeg = jpeg_with_segments(
            &[(0xE1, exif_app1(&tiff)), (0xE1, xmp_seg)],
            &[0x11, 0xFF, 0x00, 0x22, 0xFF, 0xD0, 0x33],
        );
        let block = provenance_block(&jpeg, Some((4000, 3000)));
        assert_eq!(block["format"], "jpeg");
        assert_eq!(block["metadata_read"], true);
        assert_eq!(signal_of(&block, "capture_device")["finding"], "stated");
        assert_eq!(signal_of(&block, "editor_software")["finding"], "named");
        let history = signal_of(&block, "edit_history");
        assert_eq!(history["finding"], "recorded");
        assert_eq!(history["entries"].as_array().unwrap().len(), 2);
        assert_eq!(
            signal_of(&block, "date_consistency")["finding"],
            "modified_after_capture"
        );
        assert_eq!(signal_of(&block, "document_ids")["finding"], "present");
        assert_eq!(
            signal_of(&block, "dimensions_consistency")["finding"],
            "consistent"
        );
        assert_eq!(block["location"]["present"], true);
        assert_eq!(block["location"]["latitude"], 45.5);
        assert!(block["location_warning"]
            .as_str()
            .unwrap()
            .contains("metadata-free public copy"));
        assert_eq!(block["census"]["has_xmp"], true);
        // The wording rules, pinned: no verdict vocabulary, and
        // absence is labelled meaningless.
        let text = serde_json::to_string(&block).unwrap().to_lowercase();
        assert!(!text.contains("\"edited\""), "no bare verdict field");
        assert!(text.contains("not a verdict"));
    }

    #[test]
    fn clean_camera_file_reports_absences_as_meaningless() {
        // Software carries the capture OS version ("17.0"), the
        // standard iPhone shape: it must land as the capture
        // software version, never as an editor name.
        let tiff = synthetic_tiff("17.0", "2025:06:15 12:30:45", "2025:06:15 12:30:45");
        let jpeg = jpeg_with_segments(&[(0xE1, exif_app1(&tiff))], &[0x42]);
        let block = provenance_block(&jpeg, Some((4000, 3000)));
        assert_eq!(
            signal_of(&block, "capture_device")["capture_software_version"],
            "17.0"
        );
        assert_eq!(signal_of(&block, "editor_software")["finding"], "none_named");
        assert!(signal_of(&block, "editor_software")["does_not_establish"]
            .as_str()
            .unwrap()
            .contains("silence is not cleanliness"));
        assert_eq!(signal_of(&block, "content_credentials")["finding"], "absent");
        assert!(signal_of(&block, "content_credentials")["detail"]
            .as_str()
            .unwrap()
            .contains("no native iPhone photo carries one"));
        assert_eq!(signal_of(&block, "date_consistency")["finding"], "agree");
    }

    #[test]
    fn trailing_bytes_are_counted_and_stripped_copy_drops_them() {
        let tiff = synthetic_tiff("17.0", "2025:06:15 12:30:45", "2025:06:15 12:30:45");
        let mut jpeg = jpeg_with_segments(&[(0xE1, exif_app1(&tiff))], &[0x42]);
        jpeg.extend_from_slice(b"GARBAGE");
        let block = provenance_block(&jpeg, Some((4000, 3000)));
        let trailing = signal_of(&block, "trailing_data");
        assert_eq!(trailing["finding"], "present");
        assert_eq!(trailing["bytes"], 7);
        let (copy, kind) = metadata_free_public_copy(&jpeg).unwrap();
        assert_eq!(kind, "jpeg");
        assert!(!copy.windows(7).any(|w| w == b"GARBAGE"));
        // EXIF is gone in the copy: no location, no editor, and
        // the copy is shorter.
        let copy_block = provenance_block(&copy, Some((4000, 3000)));
        assert_eq!(copy_block["location"]["present"], false);
        assert!(copy.len() < jpeg.len());
    }

    #[test]
    fn strip_png_drops_metadata_chunks_and_keeps_pixels() {
        let img = crate::image::RgbaImage::solid(6, 4, [10, 20, 30, 255]);
        let mut png = crate::image::encode_png_rgba(&img);
        // Graft an eXIf chunk (GPS TIFF) after IHDR.
        let tiff = synthetic_tiff("17.0", "2025:06:15 12:30:45", "2025:06:15 12:30:45");
        let chunk = crate::image::png_chunk(b"eXIf", &tiff);
        let mut with_exif = png[..33].to_vec();
        with_exif.extend_from_slice(&chunk);
        with_exif.extend_from_slice(&png[33..]);
        png = with_exif;
        let before = provenance_block(&png, Some((6, 4)));
        assert_eq!(before["location"]["present"], true);
        let (copy, kind) = metadata_free_public_copy(&png).unwrap();
        assert_eq!(kind, "png");
        let after = provenance_block(&copy, Some((6, 4)));
        assert_eq!(after["location"]["present"], false);
        assert_eq!(after["census"]["has_exif"], false);
        // Pixels identical.
        let a = crate::image::decode_image(&png).unwrap();
        let b = crate::image::decode_image(&copy).unwrap();
        let (a, b) = (to_rgba(a), to_rgba(b));
        assert_eq!(a.pixel_sha256(), b.pixel_sha256());
    }

    fn to_rgba(d: crate::image::DecodedImage) -> crate::image::RgbaImage {
        match d {
            crate::image::DecodedImage::Rgba(img) => img,
            crate::image::DecodedImage::Gray(g) => g.to_rgba(),
        }
    }

    #[test]
    fn c2pa_presence_detected_in_jpeg_app11_and_png_cabx() {
        let mut app11 = Vec::new();
        app11.extend_from_slice(&[0x00, 0x01, 0x02]); // CI + En bytes
        app11.extend_from_slice(b"xxxxjumb");
        app11.extend_from_slice(b"....c2pa....");
        let jpeg = jpeg_with_segments(&[(0xEB, app11)], &[0x42]);
        let block = provenance_block(&jpeg, Some((10, 10)));
        assert_eq!(signal_of(&block, "content_credentials")["finding"], "present");
        assert!(signal_of(&block, "content_credentials")["detail"]
            .as_str()
            .unwrap()
            .contains("not checked against a trust list"));

        // PNG with a caBX chunk.
        let img = crate::image::RgbaImage::solid(4, 4, [1, 2, 3, 255]);
        let png = crate::image::encode_png_rgba(&img);
        let cabx = crate::image::png_chunk(b"caBX", b"jumb");
        let mut with_cabx = png[..33].to_vec();
        with_cabx.extend_from_slice(&cabx);
        with_cabx.extend_from_slice(&png[33..]);
        let block = provenance_block(&with_cabx, Some((4, 4)));
        assert_eq!(signal_of(&block, "content_credentials")["finding"], "present");
    }

    #[test]
    fn unknown_and_garbage_inputs_never_panic() {
        for data in [
            &b""[..],
            b"not an image",
            &[0xFF, 0xD8, 0xFF],
            &[0x89, b'P', b'N', b'G'],
        ] {
            let block = provenance_block(data, None);
            assert_eq!(block["metadata_read"], false);
            let _ = metadata_free_public_copy(data);
        }
    }
}
