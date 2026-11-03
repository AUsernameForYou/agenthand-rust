"""Edge fix (mirror of the Rust tests/composite_edge.rs): kill the
cut-out rim on source-derived plates without writing a product pixel.

The fixtures reproduce the defect: a dark product photographed on a
bright kitchen counter, cut with SAM's soft edge band, placed on a
dark plate. Before the fix the band shows the counter (rim luma ~79);
after, it shows the product (rim luma < 50). $0: all synthetic.
"""

import numpy as np
from PIL import Image

from agenthand import composite, segmentation
from agenthand.segmentation import EdgeField, decontaminate_edge_band, extract_foreground

PRODUCT = (40, 42, 46)
COUNTER = (232, 226, 214)
DARK_PLATE = (30, 30, 34, 255)
RECT = (24, 16, 64, 46)  # x0, y0, x1 (exclusive), y1 (exclusive)


def rect_dist(x, y):
    x0, y0, x1, y1 = RECT
    dx = x0 - x if x < x0 else (x - x1 + 1 if x >= x1 else 0)
    dy = y0 - y if y < y0 else (y - y1 + 1 if y >= y1 else 0)
    return max(dx, dy)


def _image(w, h, f):
    return Image.fromarray(
        np.array([[f(x, y) for x in range(w)] for y in range(h)], dtype=np.uint8),
        "RGBA",
    )


def charger_source(ramp):
    """The photo: dark product on a bright counter, blended over
    `ramp` px outside the rect (cov 256 inside)."""

    def f(x, y):
        d = rect_dist(x, y)
        if d == 0:
            cov = 256
        elif d <= ramp:
            cov = 256 * (ramp + 1 - d) // (ramp + 1)
        else:
            cov = 0
        px = [(PRODUCT[c] * cov + COUNTER[c] * (256 - cov) + 128) >> 8 for c in range(3)]
        if d > ramp:
            px[0] -= (x * 7 + y * 3) % 5
        return tuple(px) + (255,)

    return _image(88, 62, f)


def rect_mask(ramp):
    """The SAM-style mask: 255 inside, soft rings outside."""

    def f(x, y):
        d = rect_dist(x, y)
        return {0: 255, 1: 170, 2: 85}.get(d, 0) if d <= ramp else 0

    arr = np.array([[f(x, y) for x in range(88)] for y in range(62)], dtype=np.uint8)
    return Image.fromarray(arr, "L")


def luma(px):
    return (int(px[0]) * 19595 + int(px[1]) * 38470 + int(px[2]) * 7471 + 0x8000) >> 16


def band_pixels(fg):
    a = np.asarray(fg.getchannel("A"))
    return [(x, y) for y in range(a.shape[0]) for x in range(a.shape[1]) if 0 < a[y, x] < 255]


def max_rim_luma(img, band):
    return max(luma(img.getpixel((x, y))) for x, y in band)


# ------------------------------------------------------------ pixel math


def test_composite_pixel_matches_pillow():
    # composite_pixel must be Image.alpha_composite's own arithmetic:
    # the band/feather writes have to blend into a whole-image
    # composite seamlessly.
    for sa in (0, 1, 8, 30, 77, 128, 200, 254, 255):
        for da in (0, 90, 255):
            for v in (0, 13, 128, 255):
                s = (v, 255 - v, (v * 7) % 256, sa)
                d = ((v * 3) % 256, v, 200 - v if v <= 200 else 0, da)
                simg = Image.new("RGBA", (1, 1), s)
                dimg = Image.new("RGBA", (1, 1), d)
                want = Image.alpha_composite(dimg, simg).getpixel((0, 0))
                got = composite.composite_pixel(d, s)
                assert got == want, f"s={s} d={d}: {got} != {want}"


def test_unblend_recovers_the_product_colour():
    # Observed = product at a=128 over the counter.
    observed = tuple((PRODUCT[c] * 128 + COUNTER[c] * 127 + 127) // 255 for c in range(3))
    assert observed == (136, 134, 130)
    f = segmentation.unblend_edge_pixel(observed, 128, COUNTER, PRODUCT)
    for c in range(3):
        assert abs(f[c] - PRODUCT[c]) <= 3, f
    # A nearly transparent pixel leans on the product colour, never on
    # the bright background it barely covers.
    faint = tuple((PRODUCT[c] * 10 + COUNTER[c] * 245 + 127) // 255 for c in range(3))
    f = segmentation.unblend_edge_pixel(faint, 10, COUNTER, PRODUCT)
    assert f[0] < 120, f


# ------------------------------------------------------------ acceptance


def test_the_rim_is_gone_and_the_receipt_set_is_unchanged():
    source = charger_source(2)
    fg = extract_foreground(source, rect_mask(2))
    plate = Image.new("RGBA", (88, 62), DARK_PLATE)
    band = band_pixels(fg)
    assert band, "the soft cut has an edge band"
    before, counts_before, _ = composite.composite_images(fg, plate)
    after, counts_after, transforms, report = composite.composite_images_with_edge_fix(
        fg, plate, source, False
    )
    assert transforms == []
    rim_before = max_rim_luma(before, band)
    rim_after = max_rim_luma(after, band)
    assert rim_before > 70, f"fixture must show the rim: {rim_before}"
    assert rim_after < 50, f"rim after fix: {rim_after}"
    # The receipt's product set and count are the same pixels; the fix
    # changed none of them (changed == 0 is the draw gate).
    assert counts_after["opaque_pixels"] == counts_before["opaque_pixels"]
    assert counts_after["changed_opaque_pixels"] == 0
    fga = np.asarray(fg)
    ata = np.asarray(after)
    assert (ata[fga[:, :, 3] == 255] == fga[fga[:, :, 3] == 255]).all()
    assert report["applied"] is True
    assert report["product_pixels_written"] == 0
    assert report["edge_band"]["pixels"] == len(band)
    assert report["edge_band"]["decontaminated"] == len(band)


def test_a_binary_cut_gets_the_plate_side_fix():
    # A hard mask (no soft band at all): the rim used to come from the
    # plate itself, the bright matte ring the plate kept around the
    # greyed product shape. The contact band rebuild wipes it.
    source = charger_source(0)
    fg = extract_foreground(source, rect_mask(0))
    assert band_pixels(fg) == []
    plate_arr = np.tile(np.array(DARK_PLATE, dtype=np.uint8), (62, 88, 1))
    for y in range(62):
        for x in range(88):
            if rect_dist(x, y) in (1, 2):
                plate_arr[y, x] = COUNTER + (255,)
    plate = Image.fromarray(plate_arr, "RGBA")
    ring = [(x, y) for y in range(62) for x in range(88) if rect_dist(x, y) in (1, 2)]
    before, _, _ = composite.composite_images(fg, plate)
    assert max(luma(before.getpixel(p)) for p in ring) > 170
    after, counts, _, report = composite.composite_images_with_edge_fix(
        fg, plate, source, False
    )
    assert max(luma(after.getpixel(p)) for p in ring) < 50
    assert counts["changed_opaque_pixels"] == 0
    assert report["applied"] is True
    assert report["contact_band"]["rim_rebuilt_pixels"] > 0


def test_a_contaminated_layer_stays_contaminated_where_it_is_opaque():
    # The fix dresses the band and the plate; it does not launder a
    # layer whose OPAQUE pixels are already wrong (that is what the
    # receipt is for): opaque pixels pass through untouched.
    source = charger_source(0)
    fg = extract_foreground(source, rect_mask(0))
    arr = np.asarray(fg).copy()
    arr[30, 40] = (250, 250, 250, 255)  # a wrong opaque pixel inside
    bad = Image.fromarray(arr, "RGBA")
    plate = Image.new("RGBA", (88, 62), DARK_PLATE)
    out, counts, _, _ = composite.composite_images_with_edge_fix(bad, plate, source, False)
    assert out.getpixel((40, 30)) == (250, 250, 250, 255)
    assert counts["changed_opaque_pixels"] == 0  # measured against the stored layer


# ---------------------------------------------------------------- feather


def test_feather_alpha_follows_the_source_edge():
    assert composite.feather_alpha_for_width(256) == 0  # pixel-hard
    assert composite.feather_alpha_for_width(composite.EDGE_WIDTH_HARD_FX) == 0
    # A ~2 px source ramp (fx 448..576): alpha in (90, 128].
    for fx in (448, 512, 576):
        a = composite.feather_alpha_for_width(fx)
        assert 90 <= a <= 128, (fx, a)
    # A very soft source (4 px ramp) still caps at the max: about
    # 1 px of feather, never more (Daniel: do not blur too much).
    assert composite.feather_alpha_for_width(1024) == composite.FEATHER_MAX_ALPHA


def test_a_hard_source_edge_gets_no_feather():
    source = charger_source(0)
    fg = extract_foreground(source, rect_mask(0))
    plate = Image.new("RGBA", (88, 62), DARK_PLATE)
    _, _, _, report = composite.composite_images_with_edge_fix(fg, plate, source, False)
    assert report["feather"]["alpha"] == 0
    assert report["feather"]["pixels"] == 0
    assert report["source_edge"]["samples"] >= composite.EDGE_WIDTH_MIN_SAMPLES


def test_a_soft_source_edge_gets_a_capped_feather():
    # Binary cut, soft PHOTO edge: the width is measured on the
    # source, and the ring hugging the product gains the feather.
    source = charger_source(3)
    fg = extract_foreground(source, rect_mask(0))
    plate = Image.new("RGBA", (88, 62), DARK_PLATE)
    _, _, _, report = composite.composite_images_with_edge_fix(fg, plate, source, False)
    assert report["feather"]["alpha"] > 0
    assert report["feather"]["alpha"] <= composite.FEATHER_MAX_ALPHA
    assert report["feather"]["pixels"] > 0
    assert report["source_edge"]["ramp_width_px"] > 1.125


# ------------------------------------------------------------ contact band


def test_thin_structures_get_the_narrower_band():
    # A picket fence: 3 px bars. The band narrows to 2 px so it stays
    # on the plate side of narrow gaps.
    w, h = 90, 60
    src_arr = np.zeros((h, w, 4), dtype=np.uint8)
    src_arr[:, :] = (200, 200, 200, 255)
    mask_arr = np.zeros((h, w), dtype=np.uint8)
    for y in range(h):
        for x in range(w):
            if x % 9 < 3:
                src_arr[y, x] = (60, 60, 60, 255)
                mask_arr[y, x] = 255
    source = Image.fromarray(src_arr, "RGBA")
    fg = extract_foreground(source, Image.fromarray(mask_arr, "L"))
    plate = Image.new("RGBA", (w, h), (240, 240, 240, 255))
    _, _, _, report = composite.composite_images_with_edge_fix(fg, plate, source, True)
    assert report["contact_band"]["width_px"] == 2
    assert report["thin_structure"] is True
    # 3 px outside a picket (the middle of a 6 px gap) is beyond the
    # thin band: untouched plate.
    out_plate = composite.apply_edge_fix(source, fg, plate, True).plate
    assert out_plate.getpixel((14, 30)) == (240, 240, 240, 255)


def test_determinism_and_a_far_plate():
    # Same inputs, same outputs; a plate nothing like the source
    # geometry still fixes cleanly (the fix reads the source only for
    # colours and softness, the plate only for its own rim).
    source = charger_source(2)
    fg = extract_foreground(source, rect_mask(2))
    plate = Image.new("RGBA", (88, 62), (200, 30, 30, 255))
    a, _, _, ra = composite.composite_images_with_edge_fix(fg, plate, source, False)
    b, _, _, rb = composite.composite_images_with_edge_fix(fg, plate, source, False)
    assert a.tobytes() == b.tobytes()
    assert ra == rb


# ------------------------------------------------------------ decontaminate


def test_decontaminate_alone_leaves_alpha_and_product_untouched():
    source = charger_source(2)
    fg = extract_foreground(source, rect_mask(2))
    field = EdgeField.create(fg)
    out, band, fixed = decontaminate_edge_band(source, fg, field)
    assert band == fixed and band > 0
    fa = np.asarray(fg)
    oa = np.asarray(out)
    assert (oa[:, :, 3] == fa[:, :, 3]).all(), "alpha plane unchanged"
    assert (oa[fa[:, :, 3] == 255] == fa[fa[:, :, 3] == 255]).all()
    assert (oa[fa[:, :, 3] == 0] == fa[fa[:, :, 3] == 0]).all()


# ------------------------------------------------------- draw integration


def _seed_fg_with_soft_edge(store):
    """A 24x18 foreground cut from its photo: dark product block with
    one soft edge column, on a bright counter (the charger defect in
    miniature), plus a dark plate asset. Returns (fg handle, plate
    handle, plate image)."""
    photo = Image.new("RGBA", (24, 18), COUNTER + (255,))
    px = photo.load()
    for y in range(18):
        for x in range(10):
            px[x, y] = PRODUCT + (255,)
        px[10, y] = (136, 134, 130, 255)  # the blended edge column
    photo_handle = store.put_image(photo, kind="source_photo")
    fg = Image.new("RGBA", (24, 18), (0, 0, 0, 0))
    px = fg.load()
    for y in range(18):
        for x in range(10):
            px[x, y] = PRODUCT + (255,)
        px[10, y] = (136, 134, 130, 128)
    fg_handle = store.put_image(
        fg,
        kind="foreground_png",
        source_asset_id=store.parse_handle(photo_handle),
    )
    plate = Image.new("RGBA", (24, 18), DARK_PLATE)
    plate_handle = store.put_image(plate, kind="background_plate")
    return fg_handle, plate_handle, plate


def test_draw_source_edit_receipt_carries_the_edge_fix_record(tmp_path):
    from agenthand import quality_receipt
    from agenthand.assets import AssetStore

    store = AssetStore(tmp_path)
    handle, plate_handle, _ = _seed_fg_with_soft_edge(store)
    out_handle, receipt = composite.draw_product(
        store, handle, "the same counter, tidied",
        plate_handle=plate_handle, plate_kind="source_edit",
    )
    assert receipt["plate_kind"] == "source_edit"
    assert receipt["edge_fix"]["applied"] is True
    assert receipt["edge_fix"]["product_pixels_written"] == 0
    assert receipt["edge_fix"]["edge_band"]["pixels"] > 0
    assert receipt["opaque_pixels"] == 10 * 18
    assert receipt["changed_opaque_pixels"] == 0
    # The verifier reproduces the edge-fixed composite exactly.
    _data, checks = quality_receipt.verify_quality(store, handle, receipt)
    failed = {k: v for k, v in checks.items() if not v[0]}
    assert not failed, failed


def test_draw_legacy_plate_kind_has_no_edge_fix_record(tmp_path):
    from agenthand.assets import AssetStore

    store = AssetStore(tmp_path)
    handle, plate_handle, plate = _seed_fg_with_soft_edge(store)
    out_handle, receipt = composite.draw_product(
        store, handle, "a generated room",
        plate_handle=plate_handle, plate_kind="model_generated",
    )
    assert "edge_fix" not in receipt
    # The plain composite path, byte for byte.
    fg = store.read_image(handle)
    want, _, _ = composite.composite_images(fg, plate)
    got = store.read_image(out_handle)
    assert got.tobytes() == want.tobytes()


def test_draw_kill_switch_restores_the_old_composite(tmp_path, monkeypatch):
    from agenthand.assets import AssetStore

    monkeypatch.setenv("AGENTHAND_EDGE_FIX", "off")
    store = AssetStore(tmp_path)
    handle, plate_handle, plate = _seed_fg_with_soft_edge(store)
    out_handle, receipt = composite.draw_product(
        store, handle, "the same counter, tidied",
        plate_handle=plate_handle, plate_kind="source_edit",
    )
    assert receipt["edge_fix"]["applied"] is False
    assert "AGENTHAND_EDGE_FIX" in receipt["edge_fix"]["reason"]
    fg = store.read_image(handle)
    want, _, _ = composite.composite_images(fg, plate)
    got = store.read_image(out_handle)
    assert got.tobytes() == want.tobytes()
