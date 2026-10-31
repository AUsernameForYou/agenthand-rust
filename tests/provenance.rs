//! Bonus package (provenance signals, container census, GPS
//! public copy) end to end: a real corpus HEIC reads its
//! container census; a GPS-carrying JPEG ingests with the block
//! inside the stored source, mints its metadata-free public
//! copy with identical pixels, and the draw receipt republishes
//! the block and still verifies. A source stored the old way
//! (no block) draws a receipt with no provenance field — the
//! change is additive.

use agenthand::assets::{png_text_of_bytes, AssetStore, KIND_FOREGROUND, PROVENANCE_PNG_KEY};
use agenthand::composite;
use agenthand::image::{self as ah_image, RgbaImage};
use agenthand::ingest;
use agenthand::provenance;
use base64::Engine;
use serde_json::Value;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);
const CORPUS: &str = "/home/hatch/workspace/meta-hackathon/rust-100/corpus";

fn temp_store() -> (AssetStore, std::path::PathBuf) {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "agenthand-rust-provenance-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    (AssetStore::new(&dir), dir)
}

/// GPS TIFF (little-endian): IFD0 {Make, Model, Software, GPS
/// ptr}, GPS IFD {45 30 0 N, 122 45 0 W}. Layout computed in
/// one pass so every offset is exact.
fn build_gps_tiff() -> Vec<u8> {
    fn entry(out: &mut Vec<u8>, tag: u16, ty: u16, count: u32, value: u32) {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    }
    // Layout: header 8; IFD0 at 8 with 4 entries ends at 62;
    // GPS IFD at 62 with 4 entries ends at 116; strings then
    // rationals.
    let make = b"Apple\0";
    let model = b"iPhone17,2\0";
    let sw = b"Adobe Photoshop 26.0\0";
    let data_at = 116u32;
    let make_off = data_at;
    let model_off = make_off + make.len() as u32;
    let sw_off = model_off + model.len() as u32;
    let lat_off = sw_off + sw.len() as u32;
    let lon_off = lat_off + 24;
    let mut t: Vec<u8> = Vec::new();
    t.extend_from_slice(b"II*\0");
    t.extend_from_slice(&8u32.to_le_bytes());
    t.extend_from_slice(&4u16.to_le_bytes());
    entry(&mut t, 0x010F, 2, make.len() as u32, make_off);
    entry(&mut t, 0x0110, 2, model.len() as u32, model_off);
    entry(&mut t, 0x0131, 2, sw.len() as u32, sw_off);
    entry(&mut t, 0x8825, 4, 1, 62);
    t.extend_from_slice(&0u32.to_le_bytes());
    debug_assert_eq!(t.len(), 62);
    t.extend_from_slice(&4u16.to_le_bytes());
    // LatRef "N" inline.
    let mut v = [0u8; 4];
    v[0] = b'N';
    entry(&mut t, 0x0001, 2, 2, u32::from_le_bytes(v));
    entry(&mut t, 0x0002, 5, 3, lat_off);
    let mut v = [0u8; 4];
    v[0] = b'W';
    entry(&mut t, 0x0003, 2, 2, u32::from_le_bytes(v));
    entry(&mut t, 0x0004, 5, 3, lon_off);
    t.extend_from_slice(&0u32.to_le_bytes());
    debug_assert_eq!(t.len(), 116);
    t.extend_from_slice(make);
    t.extend_from_slice(model);
    t.extend_from_slice(sw);
    for (num, den) in [(45u32, 1u32), (30, 1), (0, 1), (122, 1), (45, 1), (0, 1)] {
        t.extend_from_slice(&num.to_le_bytes());
        t.extend_from_slice(&den.to_le_bytes());
    }
    t
}

fn gps_jpeg(width: u32, height: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([(x * 3) as u8, (y * 5) as u8, 128])
    });
    let mut jpeg = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 92);
    enc.encode_image(&img).unwrap();
    // Graft the EXIF APP1 right after SOI.
    let tiff = build_gps_tiff();
    let mut app1 = vec![0xFF, 0xE1];
    app1.extend_from_slice(&((tiff.len() + 8) as u16).to_be_bytes());
    app1.extend_from_slice(b"Exif\0\0");
    app1.extend_from_slice(&tiff);
    let mut out = jpeg[..2].to_vec();
    out.extend_from_slice(&app1);
    out.extend_from_slice(&jpeg[2..]);
    out
}

fn to_rgba(d: ah_image::DecodedImage) -> RgbaImage {
    match d {
        ah_image::DecodedImage::Rgba(img) => img,
        ah_image::DecodedImage::Gray(g) => g.to_rgba(),
    }
}

#[test]
fn corpus_heic_reads_its_container_census() {
    let path = format!("{CORPUS}/real/IMG_1340.HEIC");
    let Ok(data) = std::fs::read(&path) else {
        return; // corpus-gated, like tests/heic_integration.rs
    };
    let decoded = ah_image::decode_image(&data).unwrap();
    let (w, h) = match &decoded {
        ah_image::DecodedImage::Rgba(img) => (img.width, img.height),
        ah_image::DecodedImage::Gray(g) => (g.width, g.height),
    };
    let block = provenance::provenance_block(&data, Some((w, h)));
    assert_eq!(block["format"], "heic");
    let dims_signal = block["signals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["signal"] == "dimensions_consistency")
        .unwrap();
    assert_eq!(dims_signal["finding"], "consistent");
    let census = &block["census"];
    assert_eq!(census["container"], "heic");
    assert!(census["items_total"].as_u64().unwrap() > 0);
    assert_eq!(block["metadata_read"], true);
    eprintln!(
        "IMG_1340 provenance block:\n{}",
        serde_json::to_string_pretty(&block).unwrap()
    );
}

#[test]
fn ingest_mints_public_copy_and_draw_receipt_carries_the_block() {
    let (store, _dir) = temp_store();
    let jpeg = gps_jpeg(64, 48);
    let uri = format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&jpeg)
    );
    let source_handle = ingest::ingest(&uri, &store).unwrap();

    // The stored source carries the block in its PNG text chunk.
    let source_bytes = store.read_bytes(&source_handle).unwrap();
    let texts = png_text_of_bytes(&source_bytes);
    let raw = texts.get(PROVENANCE_PNG_KEY).expect("block embedded");
    let block: Value = serde_json::from_str(raw).unwrap();
    assert_eq!(block["location"]["present"], true);
    assert_eq!(block["location"]["latitude"], 45.5);
    assert!(block["location_warning"]
        .as_str()
        .unwrap()
        .contains("GPS location coordinates"));

    // The public copy exists, is labelled, location-free, and
    // pixel-identical to the original.
    let copy_info = &block["location"]["public_copy"];
    assert_eq!(
        copy_info["label"].as_str().unwrap(),
        provenance::PUBLIC_COPY_LABEL
    );
    let copy_handle = copy_info["asset"].as_str().unwrap();
    let copy_record = store
        .load_record(&AssetStore::parse_handle(copy_handle).unwrap())
        .unwrap();
    assert_eq!(copy_record.kind, agenthand::assets::KIND_PUBLIC_COPY);
    let copy_bytes = store.read_bytes(copy_handle).unwrap();
    let copy_block = provenance::provenance_block(&copy_bytes, Some((64, 48)));
    assert_eq!(copy_block["location"]["present"], false);
    let original_pixels = to_rgba(ah_image::decode_image(&jpeg).unwrap());
    let copy_pixels = to_rgba(ah_image::decode_image(&copy_bytes).unwrap());
    assert_eq!(original_pixels.pixel_sha256(), copy_pixels.pixel_sha256());
    assert_eq!(copy_info["pixel_sha256"], original_pixels.pixel_sha256());

    // A chained foreground draws; the receipt republishes the
    // block and still verifies end to end.
    let parent = AssetStore::parse_handle(&source_handle).unwrap();
    let mut pixels = vec![0u8; 64 * 48 * 4];
    for i in 0..(64 * 48) {
        pixels[i * 4] = 200;
        pixels[i * 4 + 3] = if i % 3 == 0 { 255 } else { 0 };
    }
    let fg = RgbaImage::new(64, 48, pixels);
    let fg_handle = store.put_image(&fg, KIND_FOREGROUND, Some(&parent)).unwrap();
    let (composite_handle, receipt) =
        composite::draw_product(&store, &fg_handle, "a quiet room", None).unwrap();
    let carried = &receipt["provenance"];
    assert_eq!(carried["location"]["present"], true);
    assert_eq!(
        carried["location"]["public_copy"]["asset"],
        copy_info["asset"]
    );
    let report = composite::verify_receipt_from_disk(&store, &composite_handle, None).unwrap();
    assert_eq!(report["ok"], true, "receipt verifies: {report}");
}

#[test]
fn source_without_a_block_draws_a_receipt_without_provenance() {
    // The additive guarantee: a source stored the pre-package way
    // (plain put_image, no chunk) draws fine and its receipt has
    // no provenance field at all.
    let (store, _dir) = temp_store();
    let mut pixels = vec![0u8; 8 * 6 * 4];
    for i in 0..(8 * 6) {
        pixels[i * 4 + 3] = 255;
    }
    let photo = RgbaImage::new(8, 6, pixels.clone());
    let photo_handle = store
        .put_image(&photo, agenthand::assets::KIND_SOURCE, None)
        .unwrap();
    let parent = AssetStore::parse_handle(&photo_handle).unwrap();
    let fg = RgbaImage::new(8, 6, pixels);
    let fg_handle = store.put_image(&fg, KIND_FOREGROUND, Some(&parent)).unwrap();
    let (_handle, receipt) = composite::draw_product(&store, &fg_handle, "room", None).unwrap();
    assert!(receipt.get("provenance").is_none());
}
