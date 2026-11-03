"""Provenance package (signals core, container census, GPS
public copy) — mirror of the Rust build's tests/provenance.rs.

A GPS-carrying JPEG ingests with the signals block inside the
stored source, mints its metadata-free public copy with
identical pixels, and the draw receipt republishes the block
and still verifies. A source stored the old way draws a
receipt with no provenance field (the change is additive).
The corpus HEIC asserts the same findings the Rust build
recorded for IMG_1340.
"""

from __future__ import annotations

import base64
import json
import struct
from io import BytesIO
from pathlib import Path

import pytest
from PIL import Image

from agenthand import composite, ingest, provenance
from agenthand.assets import (
    KIND_PUBLIC_COPY,
    PROVENANCE_PNG_KEY,
    AssetStore,
    png_text_of_bytes,
)

CORPUS_HEIC = Path(
    "/home/hatch/workspace/meta-hackathon/rust-100/corpus/real/IMG_1340.HEIC"
)


def _entry(out: bytearray, tag: int, ty: int, count: int, value: int) -> None:
    out += struct.pack("<HHI", tag, ty, count) + struct.pack("<I", value)


def _gps_tiff() -> bytes:
    """Little-endian TIFF: IFD0 {Make, Model, Software, GPS ptr},
    GPS IFD {45 30 0 N, 122 45 0 W}. Layout computed in one pass."""
    make, model, sw = b"Apple\x00", b"iPhone17,2\x00", b"Adobe Photoshop 26.0\x00"
    data_at = 116
    make_off = data_at
    model_off = make_off + len(make)
    sw_off = model_off + len(model)
    lat_off = sw_off + len(sw)
    lon_off = lat_off + 24
    t = bytearray(b"II*\x00" + struct.pack("<I", 8))
    t += struct.pack("<H", 4)
    _entry(t, 0x010F, 2, len(make), make_off)
    _entry(t, 0x0110, 2, len(model), model_off)
    _entry(t, 0x0131, 2, len(sw), sw_off)
    _entry(t, 0x8825, 4, 1, 62)
    t += struct.pack("<I", 0)
    assert len(t) == 62
    t += struct.pack("<H", 4)
    _entry(t, 0x0001, 2, 2, struct.unpack("<I", b"N\x00\x00\x00")[0])
    _entry(t, 0x0002, 5, 3, lat_off)
    _entry(t, 0x0003, 2, 2, struct.unpack("<I", b"W\x00\x00\x00")[0])
    _entry(t, 0x0004, 5, 3, lon_off)
    t += struct.pack("<I", 0)
    assert len(t) == 116
    t += make + model + sw
    for num, den in [(45, 1), (30, 1), (0, 1), (122, 1), (45, 1), (0, 1)]:
        t += struct.pack("<II", num, den)
    return bytes(t)


def _gps_jpeg(width: int = 64, height: int = 48) -> bytes:
    img = Image.new("RGB", (width, height))
    px = img.load()
    for y in range(height):
        for x in range(width):
            px[x, y] = ((x * 3) % 256, (y * 5) % 256, 128)
    buf = BytesIO()
    img.save(buf, format="JPEG", quality=92)
    jpeg = buf.getvalue()
    tiff = _gps_tiff()
    app1 = b"\xff\xe1" + struct.pack(">H", len(tiff) + 8) + b"Exif\x00\x00" + tiff
    return jpeg[:2] + app1 + jpeg[2:]


def _signal(block: dict, signal_id: str) -> dict:
    return next(s for s in block["signals"] if s["signal"] == signal_id)


def test_gps_block_and_wording_rules():
    block = provenance.provenance_block(_gps_jpeg(), (64, 48))
    assert block["format"] == "jpeg"
    assert block["location"]["present"] is True
    assert block["location"]["latitude"] == 45.5
    assert block["location"]["longitude"] == -122.75
    assert "GPS location coordinates" in block["location_warning"]
    assert _signal(block, "editor_software")["finding"] == "named"
    assert _signal(block, "capture_device")["finding"] == "stated"
    text = json.dumps(block).lower()
    assert "not a verdict" in text  # the scope paragraph rides along
    assert '"edited"' not in text  # never a bare verdict field


def test_ingest_mints_public_copy_and_draw_receipt_carries_block(tmp_path):
    store = AssetStore(tmp_path)
    jpeg = _gps_jpeg()
    uri = "data:image/jpeg;base64," + base64.b64encode(jpeg).decode()
    source_handle = ingest.ingest(uri, store)

    texts = png_text_of_bytes(store.read_bytes(source_handle))
    block = json.loads(texts[PROVENANCE_PNG_KEY])
    assert block["location"]["present"] is True
    copy_info = block["location"]["public_copy"]
    assert copy_info["label"] == provenance.PUBLIC_COPY_LABEL
    assert store.get_record(copy_info["asset"]).kind == KIND_PUBLIC_COPY

    copy_bytes = store.read_bytes(copy_info["asset"])
    copy_block = provenance.provenance_block(copy_bytes, (64, 48))
    assert copy_block["location"]["present"] is False
    from agenthand.assets import pixel_hash_of_image

    with Image.open(BytesIO(jpeg)) as original:
        original.load()
        original_hash = pixel_hash_of_image(original)
    assert copy_info["pixel_sha256"] == original_hash

    parent = AssetStore.parse_handle(source_handle)
    # A real cutout: the source's own pixels under a clean
    # rectangular mask (opaque block, one edge column, clear
    # rest) — the shape draw's cut grading accepts.
    fg_img = store.read_image(source_handle)
    px = fg_img.load()
    for y in range(48):
        for x in range(64):
            r, g, b, _a = px[x, y]
            alpha = 255 if x < 24 else (128 if x == 24 else 0)
            px[x, y] = (r, g, b, alpha)
    fg = store.put_image(fg_img, kind="foreground_png", source_asset_id=parent)
    handle, receipt = composite.draw_product(store, fg, "a quiet room")
    assert receipt["provenance"]["location"]["present"] is True
    assert receipt["provenance"]["location"]["public_copy"]["asset"] == copy_info["asset"]
    report = composite.verify_receipt_from_disk(store, handle)
    assert report["ok"] is True


def test_source_without_block_draws_receipt_without_provenance(tmp_path):
    store = AssetStore(tmp_path)
    photo_img = Image.new("RGBA", (8, 6), (10, 20, 30, 255))
    photo = store.put_image(photo_img, kind="source_photo")
    parent = AssetStore.parse_handle(photo)
    fg_img = photo_img.copy()
    px = fg_img.load()
    for y in range(6):
        for x in range(8):
            r, g, b, _a = px[x, y]
            alpha = 255 if x < 4 else (128 if x == 4 else 0)
            px[x, y] = (r, g, b, alpha)
    fg = store.put_image(fg_img, kind="foreground_png", source_asset_id=parent)
    _handle, receipt = composite.draw_product(store, fg, "room")
    assert "provenance" not in receipt


def test_strip_png_drops_metadata_and_keeps_pixels(tmp_path):
    # Build a PNG carrying an eXIf chunk with GPS via Pillow's
    # exif API, then strip it byte-level.
    img = Image.new("RGB", (6, 4), (10, 20, 30))
    exif = img.getexif()
    exif[0x010F] = "Apple"
    buf = BytesIO()
    img.save(buf, format="PNG", exif=exif)
    png = buf.getvalue()
    copy_bytes, kind = provenance.metadata_free_public_copy(png)
    assert kind == "png"
    after = provenance.provenance_block(copy_bytes, (6, 4))
    assert after["census"]["has_exif"] is False
    from agenthand.assets import pixel_hash_of_image

    with Image.open(BytesIO(png)) as a, Image.open(BytesIO(copy_bytes)) as b:
        a.load()
        b.load()
        assert pixel_hash_of_image(a) == pixel_hash_of_image(b)


@pytest.mark.skipif(not CORPUS_HEIC.exists(), reason="corpus-gated")
def test_corpus_heic_matches_the_rust_findings():
    data = CORPUS_HEIC.read_bytes()
    with Image.open(BytesIO(data)) as img:
        img.load()
        dims = (img.width, img.height)
    block = provenance.provenance_block(data, dims)
    findings = {s["signal"]: s["finding"] for s in block["signals"]}
    assert findings == {
        "capture_device": "stated",
        "editor_software": "none_named",
        "edit_history": "none_recorded",
        "document_ids": "absent",
        "date_consistency": "agree",
        "content_credentials": "absent",
        "dimensions_consistency": "consistent",
        "trailing_data": "none",
    }
    device = _signal(block, "capture_device")
    assert device["make"] == "Apple"
    assert device["capture_software_version"] == "26.6.2"
    census = block["census"]
    assert census["items_total"] == 119
    assert census["primary_item_type"] == "grid"
    assert census["stated_dimensions"] == [5712, 4284]
    assert census["image_tiles"] == 45
