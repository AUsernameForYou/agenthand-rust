"""Asset store: handles, hashes, integrity, sidecars, lineage."""

import hashlib
import json
from io import BytesIO

import pytest
from PIL import Image

from agenthand.assets import AssetIntegrityError, AssetStore


def _png_bytes(color=(200, 30, 30, 255), size=(6, 4)) -> bytes:
    buffer = BytesIO()
    Image.new("RGBA", size, color).save(buffer, format="PNG")
    return buffer.getvalue()


def test_put_and_read_roundtrip(tmp_path):
    store = AssetStore(tmp_path)
    payload = b"\x89PNG fake image bytes"
    handle = store.put_bytes(payload, kind="source_photo", suffix=".png")

    assert handle.startswith("asset://")
    assert store.read_bytes(handle) == payload
    assert store.resolve(handle).exists()


def test_sha256_recorded_at_ingest(tmp_path):
    store = AssetStore(tmp_path)
    payload = b"product photo bytes"
    handle = store.put_bytes(payload, kind="source_photo")
    assert store.sha256_of(handle) == hashlib.sha256(payload).hexdigest()


def test_record_is_a_sidecar_not_a_global_index(tmp_path):
    store = AssetStore(tmp_path)
    handle = store.put_bytes(b"bytes", kind="source_photo")
    asset_id = AssetStore.parse_handle(handle)
    assert (tmp_path / "assets" / f"{asset_id}.json").exists()
    assert not (tmp_path / "assets.json").exists()


def test_pixel_hash_recorded_for_real_images(tmp_path):
    store = AssetStore(tmp_path)
    handle = store.put_bytes(_png_bytes(), kind="source_photo", suffix=".png")
    record = store.get_record(handle)
    assert record.pixel_sha256 is not None
    assert (record.width, record.height) == (6, 4)


def test_verify_integrity_passes_then_raises_on_tampering(tmp_path):
    store = AssetStore(tmp_path)
    handle = store.put_bytes(_png_bytes(), kind="foreground_png", suffix=".png")
    store.verify_integrity(handle)  # does not raise

    store.resolve(handle).write_bytes(_png_bytes(color=(1, 2, 3, 255)))
    with pytest.raises(AssetIntegrityError):
        store.verify_integrity(handle)


def test_put_image_records_the_colour_profile(tmp_path):
    from PIL import ImageCms

    profile = ImageCms.ImageCmsProfile(ImageCms.createProfile("XYZ")).tobytes()
    store = AssetStore(tmp_path)

    plain = store.put_image(Image.new("RGBA", (6, 4), (1, 2, 3, 255)), kind="source_photo")
    assert store.get_record(plain).color_profile_sha256 is None

    img = Image.new("RGBA", (6, 4), (1, 2, 3, 255))
    img.info["icc_profile"] = profile
    tagged = store.put_image(img, kind="source_photo")
    assert store.get_record(tagged).color_profile_sha256 == hashlib.sha256(
        profile
    ).hexdigest()
    # The stored PNG itself carries the profile (not just the record).
    with Image.open(BytesIO(store.read_bytes(tagged))) as stored:
        assert stored.info.get("icc_profile") == profile


def test_verify_integrity_raises_when_only_the_profile_is_stripped(tmp_path):
    # Rewrite the stored file with identical pixels but no profile,
    # and update the record's file hash to match (an insider-grade
    # tamper the file hash alone cannot see). The recorded profile
    # identity is the tripwire.
    from PIL import ImageCms

    profile = ImageCms.ImageCmsProfile(ImageCms.createProfile("XYZ")).tobytes()
    buffer = BytesIO()
    img = Image.new("RGBA", (6, 4), (200, 30, 30, 255))
    img.save(buffer, format="PNG", icc_profile=profile)
    store = AssetStore(tmp_path)
    handle = store.put_bytes(buffer.getvalue(), kind="source_photo", suffix=".png")
    store.verify_integrity(handle)

    path = store.resolve(handle)
    stripped = _png_bytes(color=(200, 30, 30, 255))
    path.write_bytes(stripped)
    record = store.get_record(handle)
    record.sha256 = hashlib.sha256(stripped).hexdigest()
    record.byte_size = len(stripped)
    store._save_record(record)

    with pytest.raises(AssetIntegrityError, match="colour profile"):
        store.verify_integrity(handle)


def test_derived_asset_records_its_source(tmp_path):
    store = AssetStore(tmp_path)
    source = store.put_bytes(b"photo", kind="source_photo")
    source_id = AssetStore.parse_handle(source)
    cut = store.put_bytes(
        b"cutout", kind="foreground_png", suffix=".png", source_asset_id=source_id
    )
    assert store.get_record(cut).source_asset_id == source_id


def test_lineage_walks_to_the_root(tmp_path):
    store = AssetStore(tmp_path)
    photo = store.put_bytes(b"photo", kind="source_photo")
    photo_id = AssetStore.parse_handle(photo)
    mask = store.put_bytes(b"mask", kind="mask_png", source_asset_id=photo_id)
    fg = store.put_bytes(
        b"fg", kind="foreground_png", source_asset_id=AssetStore.parse_handle(mask)
    )
    chain = store.lineage(fg)
    assert [r.kind for r in chain] == ["foreground_png", "mask_png", "source_photo"]
    assert chain[-1].asset_id == photo_id


def test_legacy_index_is_still_readable(tmp_path):
    # A data dir written by the first scaffold (single assets.json index,
    # record without pixel fields) must keep working after the sidecar
    # migration.
    store = AssetStore(tmp_path)
    store.assets_dir.mkdir(parents=True)
    payload = b"legacy bytes"
    (store.assets_dir / "legacy1.png").write_bytes(payload)
    index = {
        "legacy1": {
            "asset_id": "legacy1",
            "sha256": hashlib.sha256(payload).hexdigest(),
            "kind": "source_photo",
            "filename": "legacy1.png",
            "byte_size": len(payload),
            "created_at": "2026-10-26T00:00:00+00:00",
            "source_asset_id": None,
        }
    }
    (tmp_path / "assets.json").write_text(json.dumps(index), encoding="utf-8")

    assert store.read_bytes("asset://legacy1") == payload
    record = store.get_record("asset://legacy1")
    assert record.kind == "source_photo"
    assert record.pixel_sha256 is None


def test_bad_handles_raise(tmp_path):
    store = AssetStore(tmp_path)
    with pytest.raises(ValueError):
        store.get_record("https://example.com/not-a-handle")
    with pytest.raises(KeyError):
        store.get_record("asset://does-not-exist")
