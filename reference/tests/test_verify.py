"""Independent verification (J2 Gauntlet must-fixes 2, 3, 5).

Two paths, both independent of the compositor's in-memory numbers:

- composite.verify_receipt_from_disk re-reads the stored files and
  re-derives every receipt number, so a receipt is no longer graded
  only by the code that produced it.
- agenthand.verify.verify_files (the agenthand-verify CLI) checks
  exported source/final/mask files with no store at all.

The adversarial fixtures are the lane's list: a tampered output file,
a 1-px shift, a JPEG re-encode, a plate swapped over the product, a
feathered mask, and a stripped colour profile whose pixels are
byte-identical (the profile attack: values match, colours change).
"""

import hashlib
from io import BytesIO

import pytest
from PIL import Image, ImageCms

from agenthand import composite, segmentation, verify
from agenthand.assets import AssetStore


def _profile_bytes(kind: str = "XYZ") -> bytes:
    """A real, parseable ICC profile's bytes.

    Stands in for a camera's embedded wide-gamut profile (e.g. Display
    P3): what the pipeline must preserve is the byte identity, and
    what it must detect is any change to it.
    """
    return ImageCms.ImageCmsProfile(ImageCms.createProfile(kind)).tobytes()


def _pattern_photo(size=(8, 6)) -> Image.Image:
    """A photo with per-pixel varied colours (JPEG-hostile on purpose)."""
    img = Image.new("RGBA", size, (0, 0, 0, 255))
    px = img.load()
    for y in range(size[1]):
        for x in range(size[0]):
            px[x, y] = ((x * 37) % 256, (y * 53) % 256, ((x + y) * 29) % 256, 255)
    return img


def _block_mask(size=(8, 6)) -> Image.Image:
    """Binary mask over x in 1..4, y in 1..4 (16 product pixels)."""
    mask = Image.new("L", size, 0)
    px = mask.load()
    for y in range(1, 5):
        for x in range(1, 5):
            px[x, y] = 255
    return mask


def _export_run(tmp_path):
    """Build a real run and export source/final/mask as plain files.

    Returns (paths dict, expected checked-pixel count). The final is
    produced by the real extraction + compositing path, so a passing
    verify is evidence about the pipeline, not about a fixture faked
    to agree with itself.
    """
    photo = _pattern_photo()
    mask = _block_mask()
    foreground = segmentation.extract_foreground(photo, mask)
    plate = Image.new("RGBA", photo.size, (0, 0, 255, 255))
    final, _counts, _t = composite.composite_images(foreground, plate)
    paths = {}
    for name, img, mode in (
        ("source", photo, None),
        ("final", final, None),
        ("mask", mask, "L"),
    ):
        path = tmp_path / f"{name}.png"
        img.save(path, format="PNG")
        paths[name] = path
    return paths, 16


def _rewrite_png(path, mutate):
    with Image.open(path) as img:
        img.load()
        out = img.convert("RGBA")
    mutate(out)
    out.save(path, format="PNG")


# -- verify_files: the happy path and the pixel attacks -----------------


def test_verify_files_passes_on_a_real_run(tmp_path):
    paths, checked = _export_run(tmp_path)
    report = verify.verify_files(paths["source"], paths["final"], paths["mask"])
    assert report["ok"] is True
    assert report["checked_pixels"] == checked
    assert report["changed_pixels"] == 0
    assert report["color_profile_preserved"] is True
    assert report["problems"] == []


def test_verify_files_detects_a_one_pixel_shift(tmp_path):
    paths, _ = _export_run(tmp_path)
    final = Image.open(paths["final"]).convert("RGBA")
    shifted = Image.new("RGBA", final.size, (0, 0, 0, 255))
    shifted.paste(final.crop((1, 0) + final.size), (0, 0))
    shifted.save(paths["final"], format="PNG")

    report = verify.verify_files(paths["source"], paths["final"], paths["mask"])
    assert report["ok"] is False
    assert report["changed_pixels"] > 0


def test_verify_files_detects_a_jpeg_reencode(tmp_path):
    paths, _ = _export_run(tmp_path)
    final = Image.open(paths["final"]).convert("RGB")
    buffer = BytesIO()
    final.save(buffer, format="JPEG", quality=88)
    buffer.seek(0)
    Image.open(buffer).convert("RGBA").save(paths["final"], format="PNG")

    report = verify.verify_files(paths["source"], paths["final"], paths["mask"])
    assert report["ok"] is False
    assert report["changed_pixels"] > 0


def test_verify_files_detects_the_plate_swapped_over_the_product(tmp_path):
    paths, _ = _export_run(tmp_path)
    # The "final" is just the plate: every product pixel was replaced.
    Image.new("RGBA", (8, 6), (0, 0, 255, 255)).save(paths["final"], format="PNG")

    report = verify.verify_files(paths["source"], paths["final"], paths["mask"])
    assert report["ok"] is False
    assert report["changed_pixels"] == report["checked_pixels"] == 16


def test_verify_files_counts_the_edge_band_without_comparing_it(tmp_path):
    paths, _ = _export_run(tmp_path)
    # Feather the mask: one column at half strength is edge band —
    # counted separately, neither checked nor silently ignored.
    mask = Image.open(paths["mask"]).convert("L")
    px = mask.load()
    for y in range(1, 5):
        px[5, y] = 128
    mask.save(paths["mask"], format="PNG")

    report = verify.verify_files(paths["source"], paths["final"], paths["mask"])
    assert report["edge_band_pixels"] == 4
    assert report["checked_pixels"] == 16


def test_verify_cli_exit_codes(tmp_path):
    paths, _ = _export_run(tmp_path)
    args = [str(paths["source"]), str(paths["final"]), str(paths["mask"])]
    assert verify.main(args) == 0

    _rewrite_png(paths["final"], lambda img: img.putpixel((2, 2), (1, 2, 3, 255)))
    assert verify.main(args) == 1


# -- verify_files: the colour-profile attack ----------------------------


def test_verify_files_fails_on_a_stripped_profile_with_identical_pixels(tmp_path):
    # THE J2 attack: strip the ICC profile from the final. Every pixel
    # value stays byte-identical, so any pixel-only check passes —
    # verification must still fail, on the profile check alone.
    profile = _profile_bytes()
    photo = _pattern_photo()
    photo.info["icc_profile"] = profile
    mask = _block_mask()
    foreground = segmentation.extract_foreground(photo, mask)
    plate = Image.new("RGBA", photo.size, (0, 0, 255, 255))
    final, _counts, _t = composite.composite_images(foreground, plate)
    assert final.info.get("icc_profile") == profile  # the chain preserved it

    source_path = tmp_path / "source.png"
    photo.save(source_path, format="PNG", icc_profile=profile)
    final_path = tmp_path / "final.png"
    stripped = final.copy()
    stripped.info.pop("icc_profile", None)  # same pixels, NO profile written
    stripped.save(final_path, format="PNG")
    mask_path = tmp_path / "mask.png"
    mask.save(mask_path, format="PNG")

    report = verify.verify_files(source_path, final_path, mask_path)
    assert report["changed_pixels"] == 0  # pixels really are identical
    assert report["color_profile_preserved"] is False
    assert report["color_profile_source_sha256"] == hashlib.sha256(profile).hexdigest()
    assert report["color_profile_output_sha256"] is None
    assert report["ok"] is False


def test_verify_files_passes_when_the_profile_is_preserved(tmp_path):
    profile = _profile_bytes()
    photo = _pattern_photo()
    photo.info["icc_profile"] = profile
    mask = _block_mask()
    foreground = segmentation.extract_foreground(photo, mask)
    plate = Image.new("RGBA", photo.size, (0, 0, 255, 255))
    final, _counts, _t = composite.composite_images(foreground, plate)

    source_path = tmp_path / "source.png"
    photo.save(source_path, format="PNG", icc_profile=profile)
    final_path = tmp_path / "final.png"
    final.save(final_path, format="PNG", icc_profile=profile)
    mask_path = tmp_path / "mask.png"
    mask.save(mask_path, format="PNG")

    report = verify.verify_files(source_path, final_path, mask_path)
    assert report["ok"] is True
    assert report["color_profile_preserved"] is True


# -- verify_receipt_from_disk: the receipt, re-graded from the files ----


def _store_run(tmp_path, *, profiled=False):
    """A full draw through the store; returns (store, handle, receipt)."""
    store = AssetStore(tmp_path)
    photo = _pattern_photo()
    if profiled:
        photo.info["icc_profile"] = _profile_bytes()
    photo_handle = store.put_image(photo, kind="source_photo")
    foreground = segmentation.extract_foreground(photo, _block_mask())
    fg_handle = store.put_image(
        foreground,
        kind="foreground_png",
        source_asset_id=AssetStore.parse_handle(photo_handle),
    )
    handle, receipt = composite.draw_product(store, fg_handle, "a regal room")
    return store, handle, receipt


def test_verify_receipt_from_disk_passes_on_a_clean_run(tmp_path):
    store, handle, receipt = _store_run(tmp_path)
    report = composite.verify_receipt_from_disk(store, handle, receipt)
    assert report["ok"] is True, report["problems"]
    assert report["recomputed"]["changed_opaque_pixels"] == 0
    assert all(report["checks"].values())


def test_verify_receipt_from_disk_loads_the_persisted_receipt(tmp_path):
    # draw_product persists the receipt beside the composite, so the
    # verifier can be handed nothing but the composite handle.
    store, handle, receipt = _store_run(tmp_path)
    assert store.load_receipt(handle) == receipt
    report = composite.verify_receipt_from_disk(store, handle)
    assert report["ok"] is True, report["problems"]
    assert report["recomputed"]["changed_opaque_pixels"] == 0


# -- mask provenance fields (decision 68): optional, validated when present --

_PROVENANCE_NONE = {
    "mask_source_width": 8,
    "mask_source_height": 6,
    "mask_sam_width": 8,
    "mask_sam_height": 6,
    "mask_scale_method": "none",
}


def test_verify_accepts_a_receipt_with_consistent_mask_provenance(tmp_path):
    store, handle, receipt = _store_run(tmp_path)
    report = composite.verify_receipt_from_disk(
        store, handle, {**receipt, **_PROVENANCE_NONE}
    )
    assert report["ok"] is True, report["problems"]


def test_verify_accepts_a_receipt_with_scaled_mask_provenance(tmp_path):
    store, handle, receipt = _store_run(tmp_path)
    scaled = {
        **_PROVENANCE_NONE,
        "mask_sam_width": 4,
        "mask_sam_height": 3,
        "mask_scale_method": "bilinear_threshold_128",
    }
    report = composite.verify_receipt_from_disk(store, handle, {**receipt, **scaled})
    assert report["ok"] is True, report["problems"]


def test_verify_rejects_a_receipt_with_a_bogus_scale_method(tmp_path):
    store, handle, receipt = _store_run(tmp_path)
    bad = {**receipt, **_PROVENANCE_NONE, "mask_scale_method": "nearest_magic"}
    report = composite.verify_receipt_from_disk(store, handle, bad)
    assert report["ok"] is False
    assert report["checks"]["receipt_shape"] is False


def test_verify_rejects_partial_mask_provenance(tmp_path):
    store, handle, receipt = _store_run(tmp_path)
    bad = {**receipt, "mask_sam_width": 4}
    report = composite.verify_receipt_from_disk(store, handle, bad)
    assert report["ok"] is False
    assert report["checks"]["receipt_shape"] is False


def test_verify_rejects_inconsistent_mask_provenance(tmp_path):
    store, handle, receipt = _store_run(tmp_path)
    # SAM frame larger than the source frame is impossible under the
    # downscale-only rule, and "none" with differing dims contradicts
    # itself; both must fail shape validation.
    bigger = {
        **receipt,
        **_PROVENANCE_NONE,
        "mask_sam_width": 16,
        "mask_scale_method": "bilinear_threshold_128",
    }
    report = composite.verify_receipt_from_disk(store, handle, bigger)
    assert report["ok"] is False
    assert report["checks"]["receipt_shape"] is False
    mismatched = {**receipt, **_PROVENANCE_NONE, "mask_sam_width": 4}
    report = composite.verify_receipt_from_disk(store, handle, mismatched)
    assert report["ok"] is False
    assert report["checks"]["receipt_shape"] is False


def test_verify_refuses_provenance_under_a_stripped_schema_marker(tmp_path):
    store, handle, receipt = _store_run(tmp_path)
    stripped = {
        k: v
        for k, v in receipt.items()
        if k
        not in {
            "receipt_schema",
            "color_profile_foreground_sha256",
            "mask_sha256",
            "mask_pixel_sha256",
            "mask_source",
            "opaque_alpha_threshold",
            "pixel_tolerance",
            "count_sample",
            "product_layer_scaled",
            "output_format",
        }
    }
    stripped.update(_PROVENANCE_NONE)
    report = composite.verify_receipt_from_disk(store, handle, stripped)
    assert report["ok"] is False
    assert report["checks"]["receipt_shape"] is False
    assert any("current-schema fields" in p for p in report["problems"])


def test_verify_receipt_from_disk_fails_on_a_tampered_output_file(tmp_path):
    store, handle, receipt = _store_run(tmp_path)
    # One pixel changed in the DELIVERED file, after the receipt was
    # issued. The receipt's own numbers still say 0 — the independent
    # recount from disk must refuse to agree.
    path = store.resolve(handle)
    _rewrite_png(path, lambda img: img.putpixel((2, 2), (9, 9, 9, 255)))

    report = composite.verify_receipt_from_disk(store, handle, receipt)
    assert report["ok"] is False
    assert report["checks"]["composite_file_integrity"] is False
    # The one-pixel NEGATIVE CONTROL (Grok): the recount itself must
    # see the tint — a counter that only ever reports 0 proves nothing
    # about whether it is wired to the delivered file.
    assert report["recomputed"]["changed_opaque_pixels"] == 1
    assert report["checks"]["recount_changed_opaque_pixels"] is False


def test_verify_files_refuses_an_empty_mask(tmp_path):
    # GPT's reproduction: two completely different images and a mask
    # that selects nothing verify "ok" today, because 0 of 0 checked
    # pixels changed. A pass that checked nothing proves nothing.
    paths, _ = _export_run(tmp_path)
    Image.new("RGBA", (8, 6), (250, 250, 250, 255)).save(paths["final"], format="PNG")
    Image.new("L", (8, 6), 0).save(paths["mask"], format="PNG")

    report = verify.verify_files(paths["source"], paths["final"], paths["mask"])
    assert report["checked_pixels"] == 0
    assert report["ok"] is False
    assert any("no" in p and "checked" in p for p in report["problems"])


def test_verify_receipt_from_disk_fails_when_the_receipt_reports_a_change(tmp_path):
    # An HONEST violation receipt — the files really do differ at one
    # opaque pixel and the receipt says so (changed == 1, counts
    # matching the files) — must not verify ok. Before the hardening,
    # every recount check passed and ok came back True.
    store = AssetStore(tmp_path)
    photo = _pattern_photo()
    photo_handle = store.put_image(photo, kind="source_photo")
    foreground = segmentation.extract_foreground(photo, _block_mask())
    fg_handle = store.put_image(
        foreground,
        kind="foreground_png",
        source_asset_id=AssetStore.parse_handle(photo_handle),
    )
    handle, receipt = composite.draw_product(store, fg_handle, "a regal room")
    # Forge a violating composite + honest receipt by hand (draw never
    # produces one): flip one opaque pixel in a copy of the composite
    # and re-derive every count and hash from the tampered files.
    tampered = store.read_image(handle)
    tampered.putpixel((2, 2), (9, 9, 9, 255))
    bad_handle = store.put_image(
        tampered,
        kind="composite",
        source_asset_id=AssetStore.parse_handle(fg_handle),
    )
    bad_record = store.get_record(bad_handle)
    counts = composite._measure(store.read_image(fg_handle), tampered)
    assert counts["changed_opaque_pixels"] == 1
    bad_receipt = dict(receipt)
    bad_receipt.update(counts)
    bad_receipt["output_asset"] = bad_handle
    bad_receipt["output_sha256"] = bad_record.sha256
    bad_receipt["output_pixel_sha256"] = bad_record.pixel_sha256
    bad_receipt["color_profile_output_sha256"] = bad_record.color_profile_sha256
    store.save_receipt(bad_handle, bad_receipt)

    report = composite.verify_receipt_from_disk(store, bad_handle, bad_receipt)
    assert report["recomputed"]["changed_opaque_pixels"] == 1
    assert report["checks"]["zero_changed_pixels"] is False
    assert report["ok"] is False


def test_verify_receipt_from_disk_fails_for_receipt_a_against_composite_b(tmp_path):
    # Two clean runs, identical fixtures, ONE store: receipt A names
    # run A's assets, but the composite under test is run B's. The
    # numbers all reproduce (the fixtures are identical), so only
    # comparing the receipt's own output hash to the file catches
    # the swap.
    store = AssetStore(tmp_path)
    _h, receipt_a = None, None
    handles = []
    for _ in range(2):
        photo = _pattern_photo()
        photo_handle = store.put_image(photo, kind="source_photo")
        foreground = segmentation.extract_foreground(photo, _block_mask())
        fg_handle = store.put_image(
            foreground,
            kind="foreground_png",
            source_asset_id=AssetStore.parse_handle(photo_handle),
        )
        handle, receipt = composite.draw_product(store, fg_handle, "a regal room")
        handles.append((handle, receipt))
    (handle_a, receipt_a), (handle_b, _receipt_b) = handles
    assert handle_a != handle_b

    report = composite.verify_receipt_from_disk(store, handle_b, receipt_a)
    assert report["checks"]["output_hash_matches_receipt"] is False
    assert report["ok"] is False


def test_verify_receipt_from_disk_fails_on_an_edited_output_hash(tmp_path):
    store, handle, receipt = _store_run(tmp_path)
    edited = dict(receipt)
    edited["output_sha256"] = "0" * 64
    report = composite.verify_receipt_from_disk(store, handle, edited)
    assert report["checks"]["output_hash_matches_receipt"] is False
    assert report["ok"] is False

    edited = dict(receipt)
    edited["source_pixel_sha256"] = "0" * 64
    report = composite.verify_receipt_from_disk(store, handle, edited)
    assert report["checks"]["source_hash_matches_receipt"] is False
    assert report["ok"] is False


def test_verify_receipt_from_disk_fails_on_an_edited_mask_hash(tmp_path):
    # A run whose foreground is chained through its stored mask asset
    # (the production shape after fix 1): the receipt's mask pin must
    # be compared against the mask asset's record, not trusted.
    store = AssetStore(tmp_path)
    photo = _pattern_photo()
    photo_handle = store.put_image(photo, kind="source_photo")
    mask_handle = store.put_image(
        _block_mask(),
        kind="mask_png",
        source_asset_id=AssetStore.parse_handle(photo_handle),
    )
    foreground = segmentation.extract_foreground(photo, _block_mask())
    fg_handle = store.put_image(
        foreground,
        kind="foreground_png",
        source_asset_id=AssetStore.parse_handle(mask_handle),
    )
    handle, receipt = composite.draw_product(store, fg_handle, "a regal room")
    assert receipt["mask_source"] == "asset"

    clean = composite.verify_receipt_from_disk(store, handle, receipt)
    assert clean["ok"] is True, clean["problems"]

    edited = dict(receipt)
    edited["mask_pixel_sha256"] = "0" * 64
    report = composite.verify_receipt_from_disk(store, handle, edited)
    assert report["checks"]["mask_hash_matches_receipt"] is False
    assert report["ok"] is False


def test_draw_refuses_a_zero_opaque_foreground(tmp_path):
    # Draw and verify share one contract (badass Gauntlet defect 4):
    # a fully transparent foreground covers zero product pixels, the
    # verifier refuses such receipts — so draw must refuse to issue
    # one. Before the alignment, draw returned a success receipt for
    # exactly this input and the verifier then rejected it.
    store = AssetStore(tmp_path)
    photo_handle = store.put_image(_pattern_photo(), kind="source_photo")
    empty_fg = Image.new("RGBA", (8, 6), (0, 0, 0, 0))
    fg_handle = store.put_image(
        empty_fg,
        kind="foreground_png",
        source_asset_id=AssetStore.parse_handle(photo_handle),
    )
    with pytest.raises(composite.FidelityLockError):
        composite.draw_product(store, fg_handle, "a regal room")


def test_verify_receipt_from_disk_fails_with_zero_opaque_coverage(tmp_path):
    # The verifier-side half of the same contract: a hand-forged
    # zero-coverage receipt (draw will no longer issue one) whose
    # counts honestly match the files must still fail, on the
    # coverage check alone.
    store, handle, receipt = _store_run(tmp_path)
    empty_fg = Image.new("RGBA", (8, 6), (0, 0, 0, 0))
    plate = Image.new("RGBA", (8, 6), (0, 0, 255, 255))
    forged_final, counts, _t = composite.composite_images(empty_fg, plate)
    assert counts["opaque_pixels"] == 0
    bad_handle = store.put_image(
        forged_final,
        kind="composite",
        source_asset_id=AssetStore.parse_handle(receipt["foreground_asset"]),
    )
    bad_record = store.get_record(bad_handle)
    forged = dict(receipt)
    forged.update(counts)
    forged["output_asset"] = bad_handle
    forged["output_sha256"] = bad_record.sha256
    forged["output_pixel_sha256"] = bad_record.pixel_sha256
    forged["color_profile_output_sha256"] = bad_record.color_profile_sha256
    store.save_receipt(bad_handle, forged)

    report = composite.verify_receipt_from_disk(store, bad_handle, forged)
    assert report["checks"]["positive_coverage"] is False
    assert report["ok"] is False


def test_verify_receipt_from_disk_fails_on_a_stripped_output_profile(tmp_path):
    store, handle, receipt = _store_run(tmp_path, profiled=True)
    assert receipt["color_profile_preserved"] is True
    clean = composite.verify_receipt_from_disk(store, handle, receipt)
    assert clean["ok"] is True, clean["problems"]

    # Rewrite the delivered file with identical pixels but no ICC
    # profile. Pixel recount still finds 0 changed; the profile check
    # is what fails.
    path = store.resolve(handle)
    with Image.open(path) as img:
        img.load()
        stripped = img.convert("RGBA")
    stripped.info.pop("icc_profile", None)
    stripped.save(path, format="PNG")

    report = composite.verify_receipt_from_disk(store, handle, receipt)
    assert report["ok"] is False
    assert report["checks"]["color_profile_preserved"] is False
    assert report["recomputed"]["color_profile_preserved"] is False


# -- verify_receipt_from_disk: the badass Gauntlet contract defects -----
# Reproduced by three lanes against the pre-hardening verifier:
# malformed receipts raised KeyError/TypeError instead of failing,
# a swapped mask FILE still verified (the pin was compared against
# the mask's record, never the file), and loosened receipt terms
# (threshold 254, tolerance 99, an invented count_sample) passed as
# decoration. Each is now a structured failure.


def _masked_store_run(tmp_path, *, profiled=False):
    """A draw chained through its stored mask asset (production shape)."""
    store = AssetStore(tmp_path)
    photo = _pattern_photo()
    if profiled:
        photo.info["icc_profile"] = _profile_bytes()
    photo_handle = store.put_image(photo, kind="source_photo")
    mask_handle = store.put_image(
        _block_mask(),
        kind="mask_png",
        source_asset_id=AssetStore.parse_handle(photo_handle),
    )
    foreground = segmentation.extract_foreground(photo, _block_mask())
    fg_handle = store.put_image(
        foreground,
        kind="foreground_png",
        source_asset_id=AssetStore.parse_handle(mask_handle),
    )
    handle, receipt = composite.draw_product(store, fg_handle, "a regal room")
    assert receipt["mask_source"] == "asset"
    return store, handle, receipt


_CONTRACT_FIELDS = sorted(
    set(composite._RECEIPT_FIELDS_ANY_SCHEMA)
    | set(composite._RECEIPT_FIELDS_V2)
    | set(composite._RECEIPT_FIELDS_V4)
    | {"receipt_schema"}
)


def test_malformed_receipts_fail_structured_never_raise(tmp_path):
    # The receipt-shape strategy: drop, null, or retype EVERY
    # contract field, one at a time. Each mutation must return a
    # report with ok False — never an exception, never a pass.
    # (Prose and attribution fields — claim, changed_predicate,
    # plate info — are not graded and are out of scope by design.)
    store, handle, receipt = _masked_store_run(tmp_path, profiled=True)
    assert composite.verify_receipt_from_disk(store, handle, receipt)["ok"]

    def retyped(value):
        if isinstance(value, bool):
            return "yes"
        if isinstance(value, int):
            return str(value)
        if isinstance(value, float):
            return "0.5"
        if isinstance(value, str):
            return 12345
        if value is None:
            return 0
        return {"not": "a scalar"}

    for field in _CONTRACT_FIELDS:
        dropped = dict(receipt)
        del dropped[field]
        report = composite.verify_receipt_from_disk(store, handle, dropped)
        assert report["ok"] is False, f"dropping {field} passed"
        assert report["checks"]["receipt_shape"] is False, field

        if receipt[field] is not None:
            nulled = dict(receipt)
            nulled[field] = None
            report = composite.verify_receipt_from_disk(store, handle, nulled)
            assert report["ok"] is False, f"nulling {field} passed"

        retyped_receipt = dict(receipt)
        retyped_receipt[field] = retyped(receipt[field])
        report = composite.verify_receipt_from_disk(store, handle, retyped_receipt)
        assert report["ok"] is False, f"retyping {field} passed"


def test_verify_receipt_from_disk_rejects_non_dict_receipts(tmp_path):
    store, handle, _receipt = _masked_store_run(tmp_path)
    for bad in (["not", "a", "receipt"], "receipt", 42, 0.5, True):
        report = composite.verify_receipt_from_disk(store, handle, bad)
        assert report["ok"] is False
        assert report["checks"]["receipt_shape"] is False


def test_verify_receipt_from_disk_fails_on_a_swapped_mask_file(tmp_path):
    # GPT's reproduction: replace the mask FILE with an all-zero
    # mask of the same size. The record still says the original
    # hashes — verification must re-hash the file and fail.
    store, handle, receipt = _masked_store_run(tmp_path)
    mask_path = store.resolve(receipt["mask_asset"])
    Image.new("L", (8, 6), 0).save(mask_path, format="PNG")

    report = composite.verify_receipt_from_disk(store, handle, receipt)
    assert report["checks"]["mask_file_integrity"] is False
    assert report["ok"] is False


def test_verify_receipt_from_disk_enforces_the_receipt_terms(tmp_path):
    # The terms printed in the receipt are law: the verifier grades
    # only under threshold 255 / tolerance 0 / the delivered-file
    # count. A receipt stating anything looser is invalid, even
    # though every pixel and hash is untouched.
    store, handle, receipt = _masked_store_run(tmp_path)
    for field, loosened in (
        ("opaque_alpha_threshold", 254),
        ("opaque_alpha_threshold", 1),
        ("pixel_tolerance", 99),
        ("pixel_tolerance", 1),
        ("count_sample", "invented"),
        ("count_sample", "in_memory_buffer"),
    ):
        edited = dict(receipt)
        edited[field] = loosened
        report = composite.verify_receipt_from_disk(store, handle, edited)
        assert report["checks"]["receipt_terms"] is False, (field, loosened)
        assert report["ok"] is False


def test_verify_receipt_from_disk_fails_on_unknown_receipt_handles(tmp_path):
    # A receipt naming assets that do not exist: structured failure,
    # not the KeyError the pre-hardening code propagated.
    store, handle, receipt = _masked_store_run(tmp_path)
    edited = dict(receipt)
    edited["foreground_asset"] = "asset://" + "f" * 32
    report = composite.verify_receipt_from_disk(store, handle, edited)
    assert report["checks"]["foreground_asset_resolves"] is False
    assert report["ok"] is False


def test_new_receipts_carry_the_schema_marker(tmp_path):
    _store, _handle, receipt = _masked_store_run(tmp_path)
    assert receipt["receipt_schema"] == composite.RECEIPT_SCHEMA_VERSION
    assert receipt["opaque_alpha_threshold"] == 255
    assert receipt["pixel_tolerance"] == 0
    assert receipt["count_sample"] == "decoded_delivered_file"


# -- verify_files: the paper-fill layer ----------------------------------


def _export_paper_run(tmp_path):
    """A run with a real paper layer through the production path.

    The mask's product block has an enclosed hole; paperfill renders
    the layer; composite_images stacks plate, paper, product.
    """
    import numpy as np

    from agenthand import paperfill

    photo = _pattern_photo(size=(48, 36))
    # The omitted patch contains product-coloured source pixels, not open space.
    photo.paste((140, 110, 80, 255), (8, 6, 40, 30))
    mask = Image.new("L", (48, 36), 0)
    px = mask.load()
    for y in range(6, 30):
        for x in range(8, 40):
            px[x, y] = 255
    for y in range(14, 22):
        for x in range(20, 28):
            px[x, y] = 0  # enclosed hole in the product block
    foreground = segmentation.extract_foreground(photo, mask)
    alpha = np.asarray(foreground.getchannel("A"))
    src_rgb = np.asarray(photo.convert("RGB"))
    regions, _refusals, _gaps = paperfill.detect_regions(src_rgb, alpha)
    layer, _map, _u = paperfill.render_reconstruction(src_rgb, alpha, regions)
    plate = Image.new("RGBA", photo.size, (245, 245, 245, 255))
    final, _counts, _t = composite.composite_images(foreground, plate, layer)
    paths = {}
    for name, img in (
        ("source", photo),
        ("final", final),
        ("mask", mask),
        ("layer", layer),
        ("plate", plate),
    ):
        path = tmp_path / f"{name}.png"
        img.save(path, format="PNG")
        paths[name] = path
    return paths


def test_verify_files_without_layer_reports_non_product_unverified(tmp_path):
    paths, _checked = _export_run(tmp_path)
    report = verify.verify_files(paths["source"], paths["final"], paths["mask"])
    assert report["ok"] is True
    # The three-file check vouches for product pixels only, and now
    # says so instead of implying the whole frame was verified.
    assert report["non_product_pixels_verified"] is False
    assert report["reconstruction_pixels"] is None


def test_verify_files_with_layer_and_plate_verifies_non_product(tmp_path):
    paths = _export_paper_run(tmp_path)
    report = verify.verify_files(
        paths["source"],
        paths["final"],
        paths["mask"],
        reconstruction_path=paths["layer"],
        plate_path=paths["plate"],
    )
    assert report["ok"] is True, report["problems"]
    assert report["non_product_pixels_verified"] is True
    assert report["recomposite_identical"] is True
    assert report["reconstruction_pixels"] > 0
    assert report["reconstruction_overlap_product_pixels"] == 0


def test_verify_files_detects_paper_on_the_product(tmp_path):
    paths = _export_paper_run(tmp_path)

    def paint(img):
        px = img.load()
        px[10, 10] = (0, 175, 225, 255)  # inside the mask block

    _rewrite_png(paths["layer"], paint)
    report = verify.verify_files(
        paths["source"],
        paths["final"],
        paths["mask"],
        reconstruction_path=paths["layer"],
        plate_path=paths["plate"],
    )
    assert report["ok"] is False
    assert report["reconstruction_overlap_product_pixels"] > 0


def test_verify_files_detects_a_pixel_no_layer_explains(tmp_path):
    paths = _export_paper_run(tmp_path)

    def paint(img):
        px = img.load()
        px[0, 0] = (1, 2, 3, 255)  # outside the product entirely

    _rewrite_png(paths["final"], paint)
    report = verify.verify_files(
        paths["source"],
        paths["final"],
        paths["mask"],
        reconstruction_path=paths["layer"],
        plate_path=paths["plate"],
    )
    assert report["ok"] is False
    assert report["recomposite_identical"] is False
