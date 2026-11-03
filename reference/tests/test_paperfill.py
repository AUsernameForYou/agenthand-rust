"""Paper-fill reconstruction (decision 93): detection, rendering, wiring.

Synthetic frames only: a "product" is a mask shape over a photo, and
the paper layer is graded on the properties the Gauntlet lanes made
load-bearing — paper only where the cut is empty (with the guard
ring), refusals recorded instead of guesses, circles unwarped on
supports, determinism, and a receipt/verifier pair that accounts for
every non-product pixel.
"""

from __future__ import annotations

import numpy as np
import pytest
from PIL import Image

from agenthand import composite, paperfill, roomrun, segmentation
from agenthand.assets import (
    KIND_MASK,
    KIND_SOURCE,
    AssetStore,
)


def _frame(width=120, height=90, color=(140, 110, 80)):
    """A flat 'photo' as an RGB uint8 array."""
    return np.zeros((height, width, 3), dtype=np.uint8) + np.array(
        color, dtype=np.uint8
    )


def _alpha_with(shape_list, width=120, height=90):
    """Foreground alpha: 255 inside the given (y0, y1, x0, x1) boxes."""
    alpha = np.zeros((height, width), dtype=np.uint8)
    for y0, y1, x0, x1 in shape_list:
        alpha[y0:y1, x0:x1] = 255
    return alpha


def _store_with_cut(tmp_path, source_rgb, alpha, mask=True):
    """Store source + mask + extracted foreground the way cut_out does."""
    store = AssetStore(tmp_path)
    source_img = Image.fromarray(source_rgb, "RGB").convert("RGBA")
    source = store.put_image(source_img, kind=KIND_SOURCE)
    mask_img = Image.fromarray(alpha, "L")
    if mask:
        mask_handle = store.put_bytes(
            _png_bytes(mask_img),
            kind=KIND_MASK,
            suffix=".png",
            source_asset_id=AssetStore.parse_handle(source),
        )
        parent = AssetStore.parse_handle(mask_handle)
    else:
        mask_handle = None
        parent = AssetStore.parse_handle(source)
    fg = segmentation.extract_foreground(source_img, mask_img)
    fg_handle = store.put_image(fg, kind="foreground_png", source_asset_id=parent)
    return store, source, mask_handle, fg_handle


def _png_bytes(img: Image.Image) -> bytes:
    import io

    buf = io.BytesIO()
    img.save(buf, format="PNG")
    return buf.getvalue()


# -- detection: holes ---------------------------------------------------


def test_enclosed_hole_is_found_open_bay_is_not():
    # A ring (donut) product: the centre is an enclosed hole; the
    # surround is an open bay connected to the frame border.
    alpha = _alpha_with([(20, 70, 30, 90)])
    alpha[35:55, 50:70] = 0  # enclosed hole, 20x20 = 400 px
    regions, refusals, _gaps = paperfill.detect_regions(_frame(), alpha)
    holes = [r for r in regions if r.cls == "hole"]
    assert len(holes) == 1
    # Detection counts live on the silhouette; Region.pixels is set
    # by the render (post-clip) count.
    assert int(np.count_nonzero(holes[0].silhouette)) == 400
    assert holes[0].provenance["basis"] == "source_product_appearance"


def test_hole_too_small_is_a_recorded_refusal():
    alpha = _alpha_with([(10, 80, 10, 110)])
    alpha[40:41, 40:42] = 0  # 2 px pinhole, below the min fraction
    regions, refusals, _gaps = paperfill.detect_regions(_frame(), alpha)
    assert [r for r in regions if r.cls == "hole"] == []
    assert any(r.code == "HOLE_TOO_SMALL" for r in refusals)


def test_holes_too_large_refuse_the_whole_fill():
    # Mask is a thin frame: the "hole" is most of the frame.
    alpha = np.zeros((90, 120), dtype=np.uint8)
    alpha[0:4, :] = 255
    alpha[86:90, :] = 255
    alpha[:, 0:4] = 255
    alpha[:, 116:120] = 255
    regions, refusals, _gaps = paperfill.detect_regions(_frame(), alpha)
    assert [r for r in regions if r.cls == "hole"] == []
    assert any(r.code == "HOLES_TOO_LARGE" for r in refusals)


def test_full_frame_mask_papers_nothing(tmp_path):
    # The whole frame is product: no empty pixel exists, so the
    # honest answer is status none with zero paper anywhere.
    alpha = np.full((90, 120), 255, dtype=np.uint8)
    store, _s, _m, fg = _store_with_cut(tmp_path, _frame(), alpha)
    report = paperfill.build_reconstruction(store, fg)
    assert report["status"] == "none"
    assert report["reconstruction_pixels"] == 0
    assert report["layer_handle"] is None


# -- detection: supports --------------------------------------------------


def _dresser_with_legs(leg_rgb=(25, 25, 25), leg_w=4, leg_h=34):
    """Body box with two dark tapered legs standing below it."""
    src = _frame(height=130, color=(170, 150, 120))
    alpha = _alpha_with([(10, 70, 20, 100)], height=130)  # body
    # Legs are SOURCE geometry the mask missed: dark, thin, tall.
    for x0 in (30, 84):
        for i in range(leg_h):
            w = leg_w - (i * (leg_w - 2) // leg_h)  # taper 4 -> 2
            src[70 + i, x0 : x0 + w] = leg_rgb
    return src, alpha


def test_dark_tapered_legs_become_supports():
    src, alpha = _dresser_with_legs()
    regions, refusals, _gaps = paperfill.detect_regions(src, alpha)
    supports = [r for r in regions if r.cls == "support"]
    assert len(supports) == 2
    for region in supports:
        assert region.provenance["aspect"] >= paperfill.SUPPORT_MIN_ASPECT


def test_wide_dark_blob_is_not_a_support():
    src = _frame(height=130, color=(170, 150, 120))
    alpha = _alpha_with([(10, 70, 20, 100)], height=130)
    src[70:110, 40:80] = (25, 25, 25)  # a cupboard, not a leg
    regions, refusals, _gaps = paperfill.detect_regions(src, alpha)
    assert [r for r in regions if r.cls == "support"] == []
    assert any(r.code == "SUPPORT_REJECTED" for r in refusals)


def test_support_reaching_the_frame_bottom_is_refused():
    src = _frame(color=(170, 150, 120))
    alpha = _alpha_with([(10, 40, 20, 100)])
    src[40:90, 30:34] = (25, 25, 25)  # runs off the frame: a crop, not a leg
    regions, refusals, _gaps = paperfill.detect_regions(src, alpha)
    assert [r for r in regions if r.cls == "support"] == []
    assert any(r.code == "SUPPORT_REJECTED" for r in refusals)


# -- rendering: disjointness, guard ring, determinism ---------------------


def _render(alpha, src=None):
    src = _frame() if src is None else src
    regions, _refusals, _gaps = paperfill.detect_regions(src, alpha)
    return paperfill.render_reconstruction(src, alpha, regions)


def test_paper_never_touches_product_and_keeps_the_guard_ring():
    alpha = _alpha_with([(20, 70, 30, 90)])
    alpha[35:55, 50:70] = 0
    layer, region_map, _unfilled = _render(alpha)
    layer_alpha = np.asarray(layer.getchannel("A"))
    assert np.count_nonzero((layer_alpha > 0) & (alpha > 0)) == 0
    # Guard ring: no paper within 1 px (Chebyshev) of product alpha.
    near = np.zeros_like(alpha, dtype=bool)
    solid = alpha > 0
    near[1:, :] |= solid[:-1, :]
    near[:-1, :] |= solid[1:, :]
    near[:, 1:] |= solid[:, :-1]
    near[:, :-1] |= solid[:, 1:]
    near |= solid
    diag = np.zeros_like(alpha, dtype=bool)
    diag[1:, 1:] |= solid[:-1, :-1]
    diag[1:, :-1] |= solid[:-1, 1:]
    diag[:-1, 1:] |= solid[1:, :-1]
    diag[:-1, :-1] |= solid[1:, 1:]
    near |= diag
    assert np.count_nonzero((layer_alpha > 0) & near) == 0
    # Region map covers exactly the paper.
    map_values = np.asarray(region_map)
    assert np.array_equal(map_values > 0, layer_alpha > 0)


def test_render_is_deterministic():
    src, alpha = _dresser_with_legs()
    layer1, map1, _ = _render(alpha, src)
    layer2, map2, _ = _render(alpha, src)
    assert layer1.tobytes() == layer2.tobytes()
    assert map1.tobytes() == map2.tobytes()


def test_support_motif_is_not_stretched():
    # On a straight vertical leg the wrap chart is arc-length based:
    # the motif phase down the leg's centre column must advance
    # uniformly, so equal steps down the leg see the pattern repeat
    # with the tile period, never squeezed. Sample the paper colour
    # at the leg centre for two equal-length windows 224 px apart in
    # a tall frame and require the same colour sequence.
    height, width = 1100, 300
    src = np.zeros((height, width, 3), dtype=np.uint8) + np.array(
        (170, 150, 120), dtype=np.uint8
    )
    alpha = np.zeros((height, width), dtype=np.uint8)
    alpha[10:610, 40:260] = 255  # body
    src[610:950, 147:153] = (25, 25, 25)  # one long straight leg, 6 px wide
    regions, _r, _gaps = paperfill.detect_regions(src, alpha)
    supports = [r for r in regions if r.cls == "support"]
    assert len(supports) == 1
    layer, _map, _u = paperfill.render_reconstruction(src, alpha, regions)
    arr = np.asarray(layer)
    centre_x = 150
    col_a = arr[650:710, centre_x, :3].astype(int)
    col_b = arr[650 + 224 : 710 + 224, centre_x, :3].astype(int)
    # Same phase one tile period later: colours match within the
    # shade envelope's rounding (shade is constant across the width
    # centre, so the match should be near-exact).
    assert np.abs(col_a - col_b).max() <= 2


# -- occluders -------------------------------------------------------------


def test_occluder_without_bounded_footprint_is_not_papered(tmp_path):
    src = _frame()
    alpha = _alpha_with([(20, 70, 30, 90)])
    store, source, _m, fg = _store_with_cut(tmp_path, src, alpha)
    occ_alpha = np.zeros((90, 120), dtype=np.uint8)
    occ_alpha[30:45, 80:100] = 255  # straddles the product's right edge
    occ_handle = store.put_bytes(
        _png_bytes(Image.fromarray(occ_alpha, "L")),
        kind=KIND_MASK,
        suffix=".png",
        source_asset_id=AssetStore.parse_handle(source),
    )
    report = paperfill.build_reconstruction(
        store, fg, occluder_mask_handles=(occ_handle,)
    )
    assert report["status"] == "refused"
    assert report["reconstruction_pixels"] == 0
    assert any(r["code"] == "OCCLUDER_WITHOUT_FOOTPRINT" for r in report["refusals"])


def test_occluder_from_another_source_is_refused(tmp_path):
    src = _frame()
    alpha = _alpha_with([(20, 70, 30, 90)])
    store, _s, _m, fg = _store_with_cut(tmp_path, src, alpha)
    other = store.put_image(
        Image.new("RGBA", (120, 90), (1, 2, 3, 255)), kind=KIND_SOURCE
    )
    occ_alpha = np.zeros((90, 120), dtype=np.uint8)
    occ_alpha[30:45, 40:60] = 255
    occ_handle = store.put_bytes(
        _png_bytes(Image.fromarray(occ_alpha, "L")),
        kind=KIND_MASK,
        suffix=".png",
        source_asset_id=AssetStore.parse_handle(other),
    )
    with pytest.raises(Exception):
        paperfill.build_reconstruction(store, fg, occluder_mask_handles=(occ_handle,))


# -- wiring: draw + receipt + verifier -------------------------------------


def test_draw_with_paper_fill_receipt_verifies(tmp_path):
    src, alpha = _dresser_with_legs()
    alpha[18:34, 45:61] = 0  # an enclosed hole in the body too
    store, _s, _m, fg = _store_with_cut(tmp_path, src, alpha)
    result = roomrun.draw_with_code_plate(store, fg, "wallpaper")
    assert result["status"] == "ok", result
    receipt = result["fidelity_receipt"]
    assert receipt["receipt_schema"] == 4
    block = receipt["reconstruction"]
    assert block["status"] == "applied"
    assert block["reconstruction_pixels"] > 0
    assert block["reconstruction_overlap_product_pixels"] == 0
    assert block["layer_asset"] is not None
    assert block["region_map_asset"] is not None
    assert sum(r["pixels"] for r in block["regions"]) == block["reconstruction_pixels"]
    assert receipt["changed_opaque_pixels"] == 0
    report = composite.verify_receipt_from_disk(store, result["output_asset"])
    assert report["ok"] is True, report["problems"]
    assert report["checks"]["layer_reproduction"] is True
    assert report["checks"]["foreground_matches_source"] is True
    assert (
        report["recomputed"]["reconstruction_pixels"] == block["reconstruction_pixels"]
    )


def test_draw_without_paper_records_status_none(tmp_path):
    alpha = _alpha_with([(20, 70, 30, 90)])
    store, _s, _m, fg = _store_with_cut(tmp_path, _frame(), alpha)
    result = roomrun.draw_with_code_plate(store, fg, "wallpaper", paper_fill=False)
    assert result["status"] == "ok"
    assert result["fidelity_receipt"]["reconstruction"]["status"] == "none"
    report = composite.verify_receipt_from_disk(store, result["output_asset"])
    assert report["ok"] is True, report["problems"]


def test_composite_images_refuses_overlapping_paper(tmp_path):
    fg_img = Image.new("RGBA", (10, 10), (0, 0, 0, 0))
    px = fg_img.load()
    for y in range(10):
        for x in range(5):
            px[x, y] = (200, 0, 0, 255)
    plate = Image.new("RGBA", (10, 10), (255, 255, 255, 255))
    layer = Image.new("RGBA", (10, 10), (0, 175, 225, 255))  # everywhere
    with pytest.raises(composite.FidelityLockError):
        composite.composite_images(fg_img, plate, layer)


def test_painted_pixel_outside_layers_fails_verification(tmp_path):
    # Attack: take an honest paper-filled draw, then paint one pixel
    # of the delivered composite (and repair its file hashes, as an
    # insider with store access could). Layer reproduction must fail.
    import hashlib

    src, alpha = _dresser_with_legs()
    store, _s, _m, fg = _store_with_cut(tmp_path, src, alpha)
    result = roomrun.draw_with_code_plate(store, fg, "wallpaper")
    assert result["status"] == "ok"
    handle = result["output_asset"]
    path = store.resolve(handle)
    img = Image.open(path).convert("RGBA")
    px = img.load()
    px[0, 0] = (1, 2, 3, 255)  # a pixel no layer explains
    img.save(path, format="PNG")
    # Repair the record hashes so only the reproduction check can tell.
    from dataclasses import replace

    from agenthand.assets import pixel_hash_of_image

    record = store.get_record(handle)
    data = path.read_bytes()
    file_hash = hashlib.sha256(data).hexdigest()
    pixel_hash = pixel_hash_of_image(img)
    store._save_record(
        replace(
            record,
            sha256=file_hash,
            pixel_sha256=pixel_hash,
            byte_size=len(data),
        )
    )
    # The receipt pins the composite's hashes too: rebuild them, so
    # the ONLY failing check can be layer reproduction.
    receipt = store.load_receipt(handle)
    receipt["composite_sha256"] = file_hash
    receipt["composite_pixel_sha256"] = pixel_hash
    report = composite.verify_receipt_from_disk(store, handle, receipt=receipt)
    assert report["ok"] is False
    assert report["checks"]["layer_reproduction"] is False


def test_new_output_cannot_downgrade_to_v2_with_or_without_paper(tmp_path):
    # Legacy policy is reserved for legacy output files. A stamped v4
    # output cannot shed its policy, even when all v4 fields are removed.
    alpha = _alpha_with([(20, 70, 30, 90)])
    store, _s, _m, fg = _store_with_cut(tmp_path, _frame(), alpha)
    result = roomrun.draw_with_code_plate(store, fg, "wallpaper", paper_fill=False)
    handle = result["output_asset"]
    receipt = store.load_receipt(handle)
    v2 = dict(receipt)
    v2["receipt_schema"] = 2
    v2.pop("reconstruction")
    v2.pop("cut_quality")
    v2.pop("plate_quality")
    report = composite.verify_receipt_from_disk(store, handle, receipt=v2)
    assert report["ok"] is False
    assert not report["checks"]["output_policy_stamp"]
    smuggled = dict(v2)
    smuggled["reconstruction"] = receipt["reconstruction"]
    report = composite.verify_receipt_from_disk(store, handle, receipt=smuggled)
    assert report["ok"] is False


def test_tiny_dark_dash_is_too_short_to_be_a_support():
    src = _frame(height=130, color=(170, 150, 120))
    alpha = _alpha_with([(10, 70, 20, 100)], height=130)
    src[70:76, 40:44] = (25, 25, 25)  # 6 px tall: a scuff, not a leg
    regions, refusals, _gaps = paperfill.detect_regions(src, alpha)
    assert [r for r in regions if r.cls == "support"] == []
    assert any(
        r.code == "SUPPORT_REJECTED" and "too short" in r.detail for r in refusals
    )


def test_speck_refusals_aggregate_by_code():
    # Grain: many tiny dark specks scattered on the floor band, none
    # touching the product. The receipt gets one aggregated record,
    # not one line per speck.
    src = _frame(height=130, color=(170, 150, 120))
    alpha = _alpha_with([(10, 40, 20, 100)], height=130)
    for i in range(30):
        y = 60 + (i % 5) * 12
        x = 24 + (i // 5) * 12
        src[y : y + 2, x : x + 2] = (25, 25, 25)
    _regions, refusals, _gaps = paperfill.detect_regions(src, alpha)
    no_touch = [r for r in refusals if r.code == "SUPPORT_NO_TOUCH"]
    assert len(no_touch) == 1
    assert no_touch[0].pixels == 30 * 4
    assert "aggregated" in no_touch[0].detail
