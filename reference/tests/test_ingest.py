"""Ingest: data URIs, handle passthrough, caps, EXIF normalization."""

import base64
from io import BytesIO

import pytest
from PIL import Image

from agenthand import ingest
from agenthand.assets import AssetStore


def _data_uri(data: bytes, mime: str = "image/png") -> str:
    return f"data:{mime};base64," + base64.b64encode(data).decode()


def _png_bytes(size=(6, 4), color=(10, 120, 200, 255)) -> bytes:
    buffer = BytesIO()
    Image.new("RGBA", size, color).save(buffer, format="PNG")
    return buffer.getvalue()


def test_data_uri_ingests_to_a_working_handle(tmp_path):
    store = AssetStore(tmp_path)
    handle = ingest.ingest(_data_uri(_png_bytes()), store)

    record = store.get_record(handle)
    assert record.kind == "source_photo"
    assert (record.width, record.height) == (6, 4)
    # Stored copy is a normalized RGBA PNG that decodes to the same pixels.
    decoded = Image.open(BytesIO(store.read_bytes(handle)))
    assert decoded.mode == "RGBA"
    assert decoded.getpixel((0, 0)) == (10, 120, 200, 255)
    store.verify_integrity(handle)


def test_handle_passes_through_after_validation(tmp_path):
    store = AssetStore(tmp_path)
    handle = store.put_bytes(b"already stored", kind="source_photo")
    assert ingest.ingest(handle, store) == handle
    with pytest.raises(KeyError):
        ingest.ingest("asset://missing", store)


def test_non_image_data_uri_rejected(tmp_path):
    store = AssetStore(tmp_path)
    with pytest.raises(ingest.IngestError):
        ingest.ingest(_data_uri(b"this is not an image"), store)
    with pytest.raises(ingest.IngestError):
        ingest.ingest("data:text/plain;base64,aGVsbG8=", store)
    with pytest.raises(ingest.IngestError):
        ingest.ingest("ftp://example.com/photo.png", store)


def test_byte_cap_rejects_oversize(tmp_path, monkeypatch):
    monkeypatch.setenv("AGENTHAND_MAX_IMAGE_BYTES", "16")
    store = AssetStore(tmp_path)
    with pytest.raises(ingest.IngestError):
        ingest.ingest(_data_uri(_png_bytes()), store)


def test_pixel_cap_rejects_huge_images(tmp_path, monkeypatch):
    monkeypatch.setenv("AGENTHAND_MAX_PIXELS", "10")
    store = AssetStore(tmp_path)
    with pytest.raises(ingest.IngestError):
        ingest.ingest(_data_uri(_png_bytes(size=(6, 4))), store)  # 24 pixels


def test_ingest_preserves_the_embedded_colour_profile(tmp_path):
    # A camera JPEG carrying a non-sRGB profile (the Display P3 case):
    # normalization to PNG must carry the profile through unchanged,
    # or the product's colours silently re-interpret downstream while
    # every pixel value stays the same.
    import hashlib

    from PIL import ImageCms

    profile = ImageCms.ImageCmsProfile(ImageCms.createProfile("XYZ")).tobytes()
    buffer = BytesIO()
    img = Image.new("RGB", (6, 4), (200, 30, 30))
    img.save(buffer, format="JPEG", icc_profile=profile)

    store = AssetStore(tmp_path)
    handle = ingest.ingest(_data_uri(buffer.getvalue(), "image/jpeg"), store)

    record = store.get_record(handle)
    assert record.color_profile_sha256 == hashlib.sha256(profile).hexdigest()
    with Image.open(BytesIO(store.read_bytes(handle))) as stored:
        assert stored.info.get("icc_profile") == profile
    store.verify_integrity(handle)


def test_ingest_without_a_profile_records_none(tmp_path):
    store = AssetStore(tmp_path)
    handle = ingest.ingest(_data_uri(_png_bytes()), store)
    assert store.get_record(handle).color_profile_sha256 is None


def test_ingest_accepts_heic_the_iphone_default(tmp_path):
    # Fable's lane-unique catch: iPhones save HEIC by default. The
    # fixture is a REAL HEIF file, encoded by pillow-heif itself
    # (ftyp heic), not a renamed PNG. HEIC is lossy, so pixels are
    # compared within codec tolerance; geometry must be exact.
    import pillow_heif

    pillow_heif.register_heif_opener()
    buffer = BytesIO()
    Image.new("RGB", (16, 12), (180, 40, 40)).save(buffer, format="HEIF", quality=95)
    raw = buffer.getvalue()
    assert raw[4:8] == b"ftyp" and b"heic" in raw[8:12]

    store = AssetStore(tmp_path)
    handle = ingest.ingest(_data_uri(raw, "image/heic"), store)

    record = store.get_record(handle)
    assert record.kind == "source_photo"
    assert (record.width, record.height) == (16, 12)
    decoded = Image.open(BytesIO(store.read_bytes(handle)))
    assert decoded.mode == "RGBA"  # normalized like every other format
    r, g, b, _a = decoded.getpixel((8, 6))
    assert abs(r - 180) <= 8 and abs(g - 40) <= 8 and abs(b - 40) <= 8
    store.verify_integrity(handle)


def test_exif_orientation_is_applied_at_ingest(tmp_path):
    # A 4x2 JPEG tagged "rotate 90° CW" (EXIF orientation 6) must be
    # stored as the 2x4 image the photographer actually framed, so every
    # downstream hash and mask is computed on the right pixels.
    buffer = BytesIO()
    img = Image.new("RGB", (4, 2), (255, 0, 0))
    exif = Image.Exif()
    exif[274] = 6
    img.save(buffer, format="JPEG", exif=exif)

    store = AssetStore(tmp_path)
    handle = ingest.ingest(_data_uri(buffer.getvalue(), "image/jpeg"), store)
    record = store.get_record(handle)
    assert (record.width, record.height) == (2, 4)
