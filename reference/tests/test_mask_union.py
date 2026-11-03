"""The _union_masks multi-region mirror and the extract_foreground
straight-alpha (premultiply) mirror, ports of the Rust sam.rs /
segmentation.rs behaviour landed in the Nov 1 merge follow-up. $0:
synthetic masks only.
"""

import numpy as np
from PIL import Image

from agenthand.sam_masks import DecodedMask, _union_masks
from agenthand.segmentation import extract_foreground


def mask_from_rects(w, h, object_id, rects):
    """A DecodedMask whose placed image covers the given rects (x0, y0,
    x1, y1 exclusive), as a full-frame SAM record would."""
    arr = np.zeros((h, w), dtype=np.uint8)
    for x0, y0, x1, y1 in rects:
        arr[y0:y1, x0:x1] = 255
    bounds = (
        min(r[0] for r in rects),
        min(r[1] for r in rects),
        max(r[2] for r in rects),
        max(r[3] for r in rects),
    )
    return DecodedMask(
        object_id=object_id,
        bounds=bounds,
        mask_pixels=int(np.count_nonzero(arr)),
        image=Image.fromarray(arr, "L"),
    )


# ---------------------------------------------------------------- union


def test_union_keeps_real_regions_and_drops_specks():
    # 100x60 frame: floor = ceil(0.001 * 6000) = 6 px.
    a = mask_from_rects(100, 60, "0", [(5, 5, 25, 25)])      # 400 px
    b = mask_from_rects(100, 60, "1", [(60, 30, 80, 50)])    # 400 px
    speck = mask_from_rects(100, 60, "2", [(50, 5, 52, 7)])  # 4 px speck
    out = _union_masks([a, b, speck], 100, 60)
    assert out["object_ids"] == ["0", "1"]
    assert out["dropped"] == 1
    assert out["bounds"] == (5, 5, 80, 50)
    assert int(np.count_nonzero(np.asarray(out["image"]))) == 800


def test_union_falls_back_to_the_largest_when_all_are_specks():
    a = mask_from_rects(100, 60, "0", [(5, 5, 7, 7)])     # 4 px
    b = mask_from_rects(100, 60, "1", [(60, 30, 62, 32)])  # 4 px
    out = _union_masks([a, b], 100, 60)
    # Largest first (stable order on the tie): only the first survives.
    # Rust's dropped rule: when every record was under the floor, the
    # fallback record is not counted as dropped (len - 1 here).
    assert out["object_ids"] == ["0"]
    assert out["dropped"] == 1
    assert int(np.count_nonzero(np.asarray(out["image"]))) == 4
    # A single speck record: nothing counts as dropped.
    out = _union_masks([a], 100, 60)
    assert out["object_ids"] == ["0"]
    assert out["dropped"] == 0


def test_union_single_mask_passthrough():
    a = mask_from_rects(100, 60, "0", [(5, 5, 25, 25)])
    out = _union_masks([a], 100, 60)
    assert out["object_ids"] == ["0"]
    assert out["dropped"] == 0
    assert out["bounds"] == (5, 5, 25, 25)


def test_union_empty_input_is_rejected():
    # The Rust union indexes order[0] (callers guarantee non-empty);
    # the Python port raises instead. Callers check for zero-match
    # before unioning.
    import pytest

    with pytest.raises(ValueError):
        _union_masks([], 100, 60)


# ------------------------------------------------------- extract_foreground


def test_extract_foreground_partial_pixels_keep_straight_colour():
    # A soft mask pixel: the stored colour is the SOURCE colour
    # (straight alpha), not a blend against transparency or black;
    # alpha scales by the mask. Mirrors Rust extract_foreground.
    src = Image.new("RGBA", (2, 1))
    src.putpixel((0, 0), (200, 100, 50, 255))
    src.putpixel((1, 0), (10, 20, 30, 128))
    mask = Image.new("L", (2, 1))
    mask.putpixel((0, 0), 128)
    mask.putpixel((1, 0), 255)
    fg = extract_foreground(src, mask)
    assert fg.getpixel((0, 0)) == (200, 100, 50, 128)
    assert fg.getpixel((1, 0)) == (10, 20, 30, 128)


def test_extract_foreground_binary_mask_is_unchanged_behaviour():
    src = Image.new("RGBA", (2, 1))
    src.putpixel((0, 0), (200, 100, 50, 255))
    src.putpixel((1, 0), (10, 20, 30, 255))
    mask = Image.new("L", (2, 1))
    mask.putpixel((0, 0), 255)
    mask.putpixel((1, 0), 0)
    fg = extract_foreground(src, mask)
    assert fg.getpixel((0, 0)) == (200, 100, 50, 255)
    assert fg.getpixel((1, 0)) == (0, 0, 0, 0)
