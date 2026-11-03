"""The Fidelity Lock invariant, tested the way the J1 Gauntlet demanded:
with a solid-color fake plate and a known foreground, zero API calls.
"""

import inspect

import pytest
from PIL import Image

from agenthand import composite
from agenthand.assets import AssetIntegrityError, AssetStore


def _foreground() -> Image.Image:
    """6x4: opaque red block, one edge column (alpha 128), transparent rest."""
    img = Image.new("RGBA", (6, 4), (0, 0, 0, 0))
    px = img.load()
    for y in range(4):
        for x in range(3):
            px[x, y] = (255, 0, 0, 255)
        px[3, y] = (0, 255, 0, 128)
    return img


def _photo() -> Image.Image:
    photo = Image.new("RGBA", (6,4), (90,90,90,255))
    fg = _foreground()
    for y in range(4):
        for x in range(4):
            r,g,b,_ = fg.getpixel((x,y))
            photo.putpixel((x,y),(r,g,b,255))
    return photo


def _store_with_foreground(tmp_path, with_mask=False):
    store = AssetStore(tmp_path)
    photo = store.put_image(_photo(), kind="source_photo")
    parent = AssetStore.parse_handle(photo)
    if with_mask:
        mask_img = _foreground().getchannel("A")
        mask = store.put_image(mask_img, kind="mask_png", source_asset_id=parent)
        parent = AssetStore.parse_handle(mask)
    fg = store.put_image(_foreground(), kind="foreground_png", source_asset_id=parent)
    return store, photo, fg


def test_invariant_opaque_pixels_unchanged_edge_pixels_counted(tmp_path):
    plate = Image.new("RGBA", (6, 4), (0, 0, 255, 255))
    out, counts, transformations = composite.composite_images(_foreground(), plate)

    assert transformations == []
    assert counts["opaque_pixels"] == 12
    assert counts["edge_band_pixels"] == 4
    assert counts["transparent_pixels"] == 8
    # THE invariant: not one fully opaque foreground pixel changed.
    assert counts["changed_opaque_pixels"] == 0

    px = out.load()
    assert px[0, 0] == (255, 0, 0, 255)  # opaque foreground, untouched
    assert px[5, 0] == (0, 0, 255, 255)  # transparent: plate shows through
    edge = px[3, 0]
    assert edge[3] == 255 and 100 < edge[1] < 160 and 100 < edge[2] < 160  # blended


def test_plate_is_fitted_to_the_product_never_the_reverse(tmp_path):
    big_plate = Image.new("RGBA", (24, 16), (0, 0, 255, 255))
    out, counts, transformations = composite.composite_images(_foreground(), big_plate)
    assert out.size == (6, 4)  # product dimensions win
    assert transformations  # and the fitting is on the record


def test_plate_signature_takes_no_product_pixels():
    params = list(inspect.signature(composite.generate_background_plate).parameters)
    assert params == ["prompt", "width", "height"]
    a = composite.generate_background_plate("a regal room", 6, 4)
    b = composite.generate_background_plate("a regal room", 6, 4)
    assert a.tobytes() == b.tobytes()  # deterministic placeholder


def test_draw_product_receipt_and_chain(tmp_path):
    store, photo, fg = _store_with_foreground(tmp_path)
    handle, receipt = composite.draw_product(store, fg, "a regal room")

    assert store.get_record(handle).kind == "composite"
    assert receipt["changed_opaque_pixels"] == 0
    assert receipt["edge_band_pixels"] == 4
    assert receipt["product_layer_scaled"] is False
    assert receipt["plate_source"] == "local_placeholder"
    assert receipt["source_asset"] == photo
    assert receipt["foreground_asset"] == fg
    assert receipt["foreground_sha256"] == store.get_record(fg).sha256
    assert receipt["foreground_pixel_sha256"] == store.get_record(fg).pixel_sha256
    assert receipt["mask_asset"] is None
    assert receipt["mask_source"] == "foreground_alpha"
    assert receipt["mask_sha256"]  # derived mask is still pinned by hash


def test_draw_product_uses_mask_asset_when_chained(tmp_path):
    store, photo, fg = _store_with_foreground(tmp_path, with_mask=True)
    _, receipt = composite.draw_product(store, fg, "a regal room")
    assert receipt["mask_source"] == "asset"
    assert receipt["mask_asset"] is not None


def test_draw_rejects_foreground_chained_through_a_composite(tmp_path):
    # The architecture Gauntlet's reproduced attack: draw a product
    # into a scene, then cut a "foreground" out of the GENERATED
    # composite and draw that. The second receipt would name the
    # original photo as source while protecting generated plate
    # pixels. Any generated asset anywhere in the ancestry refuses.
    store, photo, fg = _store_with_foreground(tmp_path)
    composite_handle, _ = composite.draw_product(store, fg, "a regal room")
    fg2 = store.put_image(
        _foreground(),
        kind="foreground_png",
        source_asset_id=AssetStore.parse_handle(composite_handle),
    )
    with pytest.raises(composite.FidelityLockError):
        composite.draw_product(store, fg2, "another room")


def test_draw_rejects_foreground_with_an_overlay_in_its_chain(tmp_path):
    store = AssetStore(tmp_path)
    photo = store.put_image(_photo(), kind="source_photo")
    overlay = store.put_image(
        _photo(),
        kind="mask_overlay",
        source_asset_id=AssetStore.parse_handle(photo),
    )
    fg = store.put_image(
        _foreground(),
        kind="foreground_png",
        source_asset_id=AssetStore.parse_handle(overlay),
    )
    with pytest.raises(composite.FidelityLockError):
        composite.draw_product(store, fg, "a regal room")


def test_draw_rejects_a_foreground_with_no_parent(tmp_path):
    store = AssetStore(tmp_path)
    fg = store.put_image(_foreground(), kind="foreground_png")
    with pytest.raises(composite.FidelityLockError):
        composite.draw_product(store, fg, "a regal room")


def test_draw_rejects_a_non_foreground_asset(tmp_path):
    store, photo, fg = _store_with_foreground(tmp_path)
    with pytest.raises(composite.FidelityLockError):
        composite.draw_product(store, photo, "a regal room")


def test_draw_rejects_foreground_without_source_photo_root(tmp_path):
    store = AssetStore(tmp_path)
    plate = store.put_image(_photo(), kind="background_plate")
    fg = store.put_image(
        _foreground(),
        kind="foreground_png",
        source_asset_id=AssetStore.parse_handle(plate),
    )
    with pytest.raises(composite.FidelityLockError):
        composite.draw_product(store, fg, "a regal room")


def test_draw_rejects_a_tampered_foreground(tmp_path):
    store, photo, fg = _store_with_foreground(tmp_path)
    store.resolve(fg).write_bytes(b"tampered bytes, not the stored PNG")
    with pytest.raises(AssetIntegrityError):
        composite.draw_product(store, fg, "a regal room")


# -- J2 Gauntlet receipt fields: the claim's terms, measured --------------


def test_receipt_reports_mask_footprint_edge_band_and_terms(tmp_path):
    store, photo, fg = _store_with_foreground(tmp_path)
    _, receipt = composite.draw_product(store, fg, "a regal room")

    # The 6x4 fixture: 12 opaque + 4 edge + 8 transparent. The receipt
    # must state the whole footprint the "0 changed" claim covers, so
    # the covered set cannot quietly shrink (soft-mask attack).
    assert receipt["mask_pixels_total"] == 16
    assert receipt["opaque_fraction"] == 0.75
    assert receipt["edge_band_max_width_px"] == 1
    assert receipt["opaque_alpha_threshold"] == 255
    assert receipt["output_format"] == "PNG"
    # No profiles anywhere in this fixture: both sides agree on none.
    assert receipt["color_profile_source_sha256"] is None
    assert receipt["color_profile_output_sha256"] is None
    assert receipt["color_profile_preserved"] is True


def test_feathered_edge_band_is_measured_not_hidden(tmp_path):
    # 12x3: 4 opaque columns, a 4-wide feathered ramp, 4 transparent.
    img = Image.new("RGBA", (12, 3), (0, 0, 0, 0))
    px = img.load()
    for y in range(3):
        for x in range(4):
            px[x, y] = (255, 0, 0, 255)
        for i, a in enumerate((51, 102, 153, 204)):
            px[4 + i, y] = (255, 0, 0, a)
    store = AssetStore(tmp_path)
    photo = store.put_image(img.convert("RGB").convert("RGBA"), kind="source_photo")
    fg = store.put_image(
        img, kind="foreground_png", source_asset_id=AssetStore.parse_handle(photo)
    )
    _, receipt = composite.draw_product(store, fg, "a regal room")

    assert receipt["edge_band_pixels"] == 12
    assert receipt["edge_band_max_width_px"] == 4
    assert receipt["mask_pixels_total"] == 24
    assert receipt["opaque_fraction"] == 0.5
    assert receipt["changed_opaque_pixels"] == 0


def test_receipt_publishes_the_excluded_set_and_its_terms(tmp_path):
    # J2 item 3: the zero never appears alone — its predicate,
    # tolerance, sample, and the delivered file's own hashes ride
    # beside it, so a reader can tell exactly what was NOT counted.
    store, photo, fg = _store_with_foreground(tmp_path)
    handle, receipt = composite.draw_product(store, fg, "a regal room")

    assert receipt["pixel_tolerance"] == 0
    assert "differs at all" in receipt["changed_predicate"]
    assert "excluded" in receipt["changed_predicate"]
    assert receipt["count_sample"] == "decoded_delivered_file"
    assert receipt["output_asset"] == handle
    assert receipt["output_sha256"] == store.get_record(handle).sha256
    assert receipt["output_pixel_sha256"] == store.get_record(handle).pixel_sha256


def test_hostile_plate_with_an_alpha_ramp_cannot_smuggle_changes(tmp_path):
    # Grok's hostile plate: alpha ramps across the product area and
    # its colours ape the plate the count would hide behind. Alpha
    # compositing must still leave every opaque product pixel exact —
    # and the negative-control twin below proves the counter notices
    # when one is not.
    store, photo, fg = _store_with_foreground(tmp_path)
    hostile = Image.new("RGBA", (6, 4), (0, 0, 0, 0))
    px = hostile.load()
    for y in range(4):
        for x in range(6):
            px[x, y] = (255, 0, 0, x * 51)  # red, alpha ramped 0..255
    plate = store.put_image(hostile, kind="background_plate")

    _handle, receipt = composite.draw_product(
        store, fg, "ignored", plate_handle=plate
    )
    assert receipt["changed_opaque_pixels"] == 0
    assert receipt["plate_source"] == "asset"


def test_alpha_250_ring_is_published_as_excluded_not_covered(tmp_path):
    # Grok's alpha-250 ring: pixels one step below opaque are where a
    # blend hides. They must appear in the published excluded set
    # (edge band), never inside the zero's covered set.
    img = Image.new("RGBA", (6, 6), (0, 0, 0, 0))
    px = img.load()
    for y in range(1, 5):
        for x in range(1, 5):
            px[x, y] = (0, 200, 0, 250)  # the ring
    for y in range(2, 4):
        for x in range(2, 4):
            px[x, y] = (255, 0, 0, 255)  # the opaque core
    store = AssetStore(tmp_path)
    photo = store.put_image(img.convert("RGB").convert("RGBA"), kind="source_photo")
    fg = store.put_image(
        img, kind="foreground_png", source_asset_id=AssetStore.parse_handle(photo)
    )
    _, receipt = composite.draw_product(store, fg, "a regal room")

    assert receipt["opaque_pixels"] == 4
    assert receipt["edge_band_pixels"] == 12  # the whole ring, excluded
    assert receipt["mask_pixels_total"] == 16
    assert receipt["opaque_fraction"] == 0.25
    assert receipt["changed_opaque_pixels"] == 0


def test_checkerboard_foreground_survives_exactly(tmp_path):
    # Fable's resampling tripwire: a 1-px checkerboard foreground.
    # Any resample, blur, or re-quantization of the product layer
    # destroys the alternation and the count must show it; intact,
    # every opaque checker survives byte-exact.
    img = Image.new("RGBA", (8, 8), (0, 0, 0, 0))
    px = img.load()
    for y in range(8):
        for x in range(8):
            if (x + y) % 2 == 0:
                px[x, y] = (10 + x * 20, 10 + y * 20, 99, 255)
    store = AssetStore(tmp_path)
    photo = store.put_image(img.convert("RGB").convert("RGBA"), kind="source_photo")
    fg = store.put_image(
        img, kind="foreground_png", source_asset_id=AssetStore.parse_handle(photo)
    )
    handle, receipt = composite.draw_product(store, fg, "a regal room")

    assert receipt["opaque_pixels"] == 32
    assert receipt["changed_opaque_pixels"] == 0
    out = store.read_image(handle).load()
    assert out[0, 0] == (10, 10, 99, 255)
    assert out[2, 0] == (50, 10, 99, 255)


def test_draw_preserves_the_source_colour_profile(tmp_path):
    import hashlib

    from PIL import ImageCms

    from agenthand import segmentation
    from agenthand.assets import describe_color_profile

    profile = ImageCms.ImageCmsProfile(ImageCms.createProfile("XYZ")).tobytes()
    photo_img = _photo()
    photo_img.info["icc_profile"] = profile
    store = AssetStore(tmp_path)
    photo = store.put_image(photo_img, kind="source_photo")
    assert store.get_record(photo).color_profile_sha256 == hashlib.sha256(
        profile
    ).hexdigest()

    mask = Image.new("L", (6, 4), 0)
    for y in range(4):
        for x in range(3):
            mask.putpixel((x, y), 255)
    foreground = segmentation.extract_foreground(store.read_image(photo), mask)
    fg = store.put_image(
        foreground,
        kind="foreground_png",
        source_asset_id=AssetStore.parse_handle(photo),
    )
    handle, receipt = composite.draw_product(store, fg, "a regal room")

    expected = hashlib.sha256(profile).hexdigest()
    assert receipt["color_profile_source_sha256"] == expected
    assert receipt["color_profile_output_sha256"] == expected
    assert receipt["color_profile_preserved"] is True
    assert receipt["color_space"] == describe_color_profile(profile)
    assert store.get_record(handle).color_profile_sha256 == expected
