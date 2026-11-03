"""Property-based tests for the Fidelity Lock counting core.

The badass Gauntlet's converged item 1, second half: named
properties over small generated images, checked against an
INDEPENDENT oracle — a plain per-pixel Python loop that shares no
code with the numpy counter it grades (calling _measure to compute
both sides would reproduce its mistakes in stereo). Minimized
failures from these properties become permanent unit regressions in
test_verify.py; the four contract defects this suite guards were
found that way by hand first.

Deterministic in CI: the profile below derandomizes generation, so
every run exercises the same examples and a failure is a failure,
not weather.
"""

import uuid

import pytest
from hypothesis import HealthCheck, assume, given, settings, strategies as st
from PIL import Image

from agenthand import composite, segmentation, verify
from agenthand.assets import AssetStore

settings.register_profile(
    "ci",
    derandomize=True,
    max_examples=50,
    deadline=None,
    database=None,
    # tmp_path is shared across a test's examples BY DESIGN here:
    # stores live in per-example uuid subdirectories and the file
    # verifier's three PNGs are overwritten wholesale each example,
    # so no state can leak from one example into the next.
    suppress_health_check=[HealthCheck.function_scoped_fixture],
)
settings.load_profile("ci")

# Alpha values cluster on the class boundaries the counter must
# separate: fully transparent, almost-transparent, half, almost
# opaque, fully opaque.
_ALPHAS = st.sampled_from([0, 1, 128, 254, 255])
_CHANNEL = st.integers(0, 255)


_SIZES = st.tuples(st.integers(1, 10), st.integers(1, 8))


@st.composite
def rgba_images(draw, *, alphas=_ALPHAS, max_w=10, max_h=8):
    width = draw(st.integers(1, max_w))
    height = draw(st.integers(1, max_h))
    return draw(_rgba_of_size(width, height, alphas))


@st.composite
def _rgba_of_size(draw, width, height, alphas):
    pixels = draw(
        st.lists(
            st.tuples(_CHANNEL, _CHANNEL, _CHANNEL, alphas),
            min_size=width * height,
            max_size=width * height,
        )
    )
    img = Image.new("RGBA", (width, height))
    img.putdata(pixels)
    return img


def _pair(data, size, alphas_a=_ALPHAS, alphas_b=_ALPHAS):
    """Two generated images of ONE drawn size (pairs, not discards)."""
    width, height = size
    return (
        data.draw(_rgba_of_size(width, height, alphas_a)),
        data.draw(_rgba_of_size(width, height, alphas_b)),
    )


def oracle_counts(foreground: Image.Image, output: Image.Image) -> dict:
    """The independent oracle: recount the claim with plain loops.

    A foreground pixel at alpha 255 is product: it must appear in
    the output byte-identical. Any other alpha is edge band or
    outside, counted but never compared. No numpy, no _measure.
    """
    fg = foreground.convert("RGBA")
    out = output.convert("RGBA")
    assert fg.size == out.size
    fg_px = fg.load()
    out_px = out.load()
    opaque = edge_band = transparent = changed = 0
    for y in range(fg.height):
        for x in range(fg.width):
            alpha = fg_px[x, y][3]
            if alpha == 255:
                opaque += 1
                if fg_px[x, y] != out_px[x, y]:
                    changed += 1
            elif alpha == 0:
                transparent += 1
            else:
                edge_band += 1
    total = opaque + edge_band
    return {
        "opaque_pixels": opaque,
        "edge_band_pixels": edge_band,
        "transparent_pixels": transparent,
        "changed_opaque_pixels": changed,
        "mask_pixels_total": total,
        "opaque_fraction": round(opaque / total, 6) if total else 0.0,
    }


# -- the counting core ---------------------------------------------------


@given(size=_SIZES, data=st.data())
def test_measure_agrees_with_the_independent_oracle(size, data):
    # _measure grades arbitrary pairs; the oracle must agree on
    # every count, for every generated pair.
    foreground, output = _pair(data, size)
    measured = composite._measure(foreground, output)
    expected = oracle_counts(foreground, output)
    for field, value in expected.items():
        assert measured[field] == value, field


@given(size=_SIZES, data=st.data())
def test_a_clean_composite_changes_zero_opaque_pixels(size, data):
    foreground, plate = _pair(data, size)
    out, _counts, _t = composite.composite_images(foreground, plate)
    assert oracle_counts(foreground, out)["changed_opaque_pixels"] == 0
    assert composite._measure(foreground, out)["changed_opaque_pixels"] == 0


@given(size=_SIZES, data=st.data())
def test_one_flipped_opaque_pixel_counts_exactly_one(size, data):
    foreground, plate = _pair(data, size)
    opaque_positions = [
        (x, y)
        for y in range(foreground.height)
        for x in range(foreground.width)
        if foreground.getpixel((x, y))[3] == 255
    ]
    assume(opaque_positions)
    out, _counts, _t = composite.composite_images(foreground, plate)
    x, y = data.draw(st.sampled_from(opaque_positions))
    r, g, b, a = out.getpixel((x, y))
    out.putpixel((x, y), (r ^ 0xFF, g, b, a))
    assert oracle_counts(foreground, out)["changed_opaque_pixels"] == 1
    assert composite._measure(foreground, out)["changed_opaque_pixels"] == 1


@given(size=_SIZES, data=st.data())
def test_edge_band_edits_never_count_as_changed(size, data):
    foreground, plate = _pair(data, size)
    band_positions = [
        (x, y)
        for y in range(foreground.height)
        for x in range(foreground.width)
        if 0 < foreground.getpixel((x, y))[3] < 255
    ]
    assume(band_positions)
    out, _counts, _t = composite.composite_images(foreground, plate)
    x, y = data.draw(st.sampled_from(band_positions))
    r, g, b, a = out.getpixel((x, y))
    out.putpixel((x, y), (r ^ 0xFF, g ^ 0xFF, b ^ 0xFF, a))
    # Edge-band pixels blend by design: editing one in the output
    # must not move the opaque-changed count off zero.
    assert oracle_counts(foreground, out)["changed_opaque_pixels"] == 0
    assert composite._measure(foreground, out)["changed_opaque_pixels"] == 0


# -- the standalone file verifier ---------------------------------------


@given(size=_SIZES, data=st.data())
def test_verify_files_counts_match_the_oracle(tmp_path, size, data):
    # Binary-alpha fixtures: the "foreground" IS the source photo
    # here, the mask is its alpha channel read as 255-or-nothing,
    # and the clean composite must verify with the oracle's counts.
    foreground, plate = _pair(data, size, st.sampled_from([0, 255]), st.just(255))
    mask = foreground.getchannel("A").point(lambda a: 255 if a == 255 else 0)
    final, _counts, _t = composite.composite_images(foreground, plate)
    paths = {}
    for name, img in (
        ("source", foreground),
        ("final", final),
        ("mask", mask),
    ):
        path = tmp_path / f"{name}.png"
        img.save(path, format="PNG")
        paths[name] = path

    report = verify.verify_files(paths["source"], paths["final"], paths["mask"])
    expected_checked = oracle_counts(foreground, foreground)["opaque_pixels"]
    assert report["checked_pixels"] == expected_checked
    if expected_checked == 0:
        # An empty mask refuses: a check of nothing proves nothing.
        assert report["ok"] is False
        return
    assert report["fidelity_ok"] is True
    assert report["ok"] == (report["cut_quality"]["grade"] != "RESHOOT")
    assert report["changed_pixels"] == 0

    # Flip exactly one fully-masked pixel in the delivered file:
    # the count must move to exactly 1 and the verdict must fall.
    full_positions = [
        (x, y)
        for y in range(size[1])
        for x in range(size[0])
        if mask.getpixel((x, y)) == 255
    ]
    x, y = data.draw(st.sampled_from(full_positions))
    tampered = final.copy()
    r, g, b, a = tampered.getpixel((x, y))
    tampered.putpixel((x, y), (r ^ 0xFF, g, b, a))
    tampered.save(paths["final"], format="PNG")
    report = verify.verify_files(paths["source"], paths["final"], paths["mask"])
    assert report["changed_pixels"] == 1
    assert report["ok"] is False


# -- receipts under mutation (the receipt-shape strategy) ----------------


def _small_run(store_dir):
    """A real masked draw on a tiny fixture; returns (store, handle, receipt)."""
    store = AssetStore(store_dir)
    photo = Image.new("RGBA", (6, 5), (0, 0, 0, 255))
    px = photo.load()
    for y in range(5):
        for x in range(6):
            px[x, y] = ((x * 41) % 256, (y * 67) % 256, ((x + y) * 31) % 256, 255)
    mask = Image.new("L", (6, 5), 0)
    mpx = mask.load()
    for y in range(1, 4):
        for x in range(1, 5):
            mpx[x, y] = 255
    photo_handle = store.put_image(photo, kind="source_photo")
    mask_handle = store.put_image(
        mask, kind="mask_png", source_asset_id=AssetStore.parse_handle(photo_handle)
    )
    foreground = segmentation.extract_foreground(photo, mask)
    fg_handle = store.put_image(
        foreground,
        kind="foreground_png",
        source_asset_id=AssetStore.parse_handle(mask_handle),
    )
    handle, receipt = composite.draw_product(store, fg_handle, "a small room")
    return store, handle, receipt


_CONTRACT_FIELDS = sorted(
    set(composite._RECEIPT_FIELDS_ANY_SCHEMA)
    | set(composite._RECEIPT_FIELDS_V2)
    | set(composite._RECEIPT_FIELDS_V4)
    | {"receipt_schema"}
)


@given(data=st.data())
@settings(max_examples=25)
def test_any_single_receipt_mutation_fails_closed(tmp_path, data):
    # The receipt-shape strategy as a property: for ANY contract
    # field and ANY mutation kind, the mutated receipt fails
    # verification structurally — ok False, and never an exception.
    store, handle, receipt = _small_run(tmp_path / uuid.uuid4().hex)
    assert composite.verify_receipt_from_disk(store, handle, receipt)["ok"]

    field = data.draw(st.sampled_from(_CONTRACT_FIELDS))
    mutation = data.draw(st.sampled_from(["drop", "null", "retype"]))
    mutated = dict(receipt)
    if mutation == "drop":
        del mutated[field]
    elif mutation == "null":
        assume(receipt[field] is not None)  # nulling a null is a no-op
        mutated[field] = None
    else:
        value = receipt[field]
        if isinstance(value, bool):
            mutated[field] = "yes"
        elif isinstance(value, int):
            mutated[field] = str(value)
        elif isinstance(value, float):
            mutated[field] = "0.5"
        elif isinstance(value, str):
            mutated[field] = 12345
        elif value is None:
            mutated[field] = 0
    report = composite.verify_receipt_from_disk(store, handle, mutated)
    assert report["ok"] is False, (field, mutation)


@given(data=st.data())
@settings(max_examples=25)
def test_an_honestly_forged_violation_still_fails(tmp_path, data):
    # Forge a violating composite and rewrite the receipt HONESTLY
    # from the oracle's counts and the new file's hashes: every
    # recount check passes, so only the zero-changed law can refuse
    # it — and it must.
    store, handle, receipt = _small_run(tmp_path / uuid.uuid4().hex)
    foreground = store.read_image(receipt["foreground_asset"])
    tampered = store.read_image(handle)
    opaque_positions = [
        (x, y)
        for y in range(foreground.height)
        for x in range(foreground.width)
        if foreground.getpixel((x, y))[3] == 255
    ]
    flips = data.draw(
        st.lists(
            st.sampled_from(opaque_positions),
            min_size=1,
            max_size=3,
            unique=True,
        )
    )
    for x, y in flips:
        r, g, b, a = tampered.getpixel((x, y))
        tampered.putpixel((x, y), (r ^ 0xFF, g, b, a))
    bad_handle = store.put_image(
        tampered,
        kind="composite",
        source_asset_id=AssetStore.parse_handle(receipt["foreground_asset"]),
    )
    bad_record = store.get_record(bad_handle)
    forged = dict(receipt)
    forged.update(oracle_counts(foreground, tampered))
    # The oracle has no edge-band width; the width is unchanged by
    # RGB flips inside the opaque region, so keep the receipt's.
    forged["output_asset"] = bad_handle
    forged["output_sha256"] = bad_record.sha256
    forged["output_pixel_sha256"] = bad_record.pixel_sha256
    forged["color_profile_output_sha256"] = bad_record.color_profile_sha256
    assert forged["changed_opaque_pixels"] == len(flips)

    report = composite.verify_receipt_from_disk(store, bad_handle, forged)
    assert report["recomputed"]["changed_opaque_pixels"] == len(flips)
    assert report["checks"]["zero_changed_pixels"] is False
    assert report["ok"] is False


@given(foreground=rgba_images(alphas=st.sampled_from([0, 1, 128, 254])))
@settings(max_examples=25)
def test_draw_refuses_any_foreground_without_opaque_pixels(tmp_path, foreground):
    # The draw/verify contract alignment as a property: ANY
    # foreground with no fully opaque pixel refuses at draw, not
    # just the all-zero fixture from the unit test.
    store = AssetStore(tmp_path / uuid.uuid4().hex)
    photo = Image.new("RGBA", foreground.size, (10, 20, 30, 255))
    photo_handle = store.put_image(photo, kind="source_photo")
    fg_handle = store.put_image(
        foreground,
        kind="foreground_png",
        source_asset_id=AssetStore.parse_handle(photo_handle),
    )
    with pytest.raises(composite.FidelityLockError):
        composite.draw_product(store, fg_handle, "a small room")
