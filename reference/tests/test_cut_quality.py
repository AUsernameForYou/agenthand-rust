"""Fixed topology/source controls and the supplied hero regressions."""

from pathlib import Path
import numpy as np
import pytest
from PIL import Image
from agenthand import composite, segmentation
from agenthand.cut_quality import grade_cut
from agenthand.assets import AssetStore, KIND_SOURCE, KIND_FOREGROUND

FIXTURES = Path(__file__).parent / "fixtures" / "wonder"


def scene():
    a = np.zeros((100, 140), np.uint8)
    a[20:65, 25:115] = 255
    src = np.full((100, 140, 3), 180, np.uint8)
    return src, a


@pytest.mark.parametrize(
    "defect,code,expected",
    [
        ("clean", None, "PASS"),
        ("legs_included", None, "PASS"),
        ("legs_missing", "SUPPORT_NOT_CAPTURED", "REPAIRABLE"),
        ("fused_shadow", None, "PASS"),
        ("fused_leg_shadow", "SUPPORT_NOT_CAPTURED", "RESHOOT"),
        ("hole", "TOP_PERFORATED", "REPAIRABLE"),
        ("notch", "TOP_OCCLUDED", "RESHOOT"),
        ("island", "TOP_OCCLUDED", "RESHOOT"),
        ("fragmented", "MASK_FRAGMENTED", "RESHOOT"),
    ],
)
def test_fixed_controls(defect, code, expected):
    src, a = scene()
    if defect in ("legs_missing", "legs_included"):
        for x in (35, 100):
            src[65:87, x : x + 4] = 15
            if defect == "legs_included":
                a[65:87, x : x + 4] = 255
    if defect == "fused_shadow":
        src[65:87, 30:110] = 15
    if defect == "fused_leg_shadow":
        src[65:82, 40:44] = 15
        src[82:91, 30:110] = 15
    if defect == "hole":
        a[30:40, 50:65] = 0
    if defect == "notch":
        a[20:38, 55:80] = 0
    if defect == "island":
        a[8:15, 55:70] = 255
    if defect == "fragmented":
        a[:] = 0
        a[20:40, 20:40] = 255
        a[60:80, 100:120] = 255
    q = grade_cut(src, a)
    assert q["grade"] == expected, q
    if code:
        assert code in q["codes"]


@pytest.mark.parametrize("name", ["mask-1369.png", "mask-1370.png"])
def test_hero_refuses_before_plate_or_storage(tmp_path, monkeypatch, name):
    mask = Image.open(FIXTURES / name).convert("L")
    assert grade_cut(None, mask)["grade"] == "RESHOOT"
    store = AssetStore(tmp_path)
    source = Image.new("RGBA", mask.size, (180, 180, 180, 255))
    sh = store.put_image(source, kind=KIND_SOURCE)
    fg = store.put_image(
        segmentation.extract_foreground(source, mask),
        kind=KIND_FOREGROUND,
        source_asset_id=store.parse_handle(sh),
    )

    def forbidden(*a, **kw):
        raise AssertionError("plate generation/storage reached")

    monkeypatch.setattr(composite, "generate_background_plate", forbidden)
    monkeypatch.setattr(store, "put_image", forbidden)
    with pytest.raises(composite.StageabilityError):
        composite.draw_product(store, fg, "room")


def test_review_contains_source_grade():
    src, a = scene()
    assert segmentation.review_mask(Image.fromarray(a), Image.fromarray(src))[
        "cut_quality"
    ] == grade_cut(src, a)


def test_server_refuses_before_billing(tmp_path, monkeypatch):
    from agenthand import server, meta_client

    src, a = scene()
    a[20:42, 50:80] = 0
    store = AssetStore(tmp_path)
    sh = store.put_image(Image.fromarray(src).convert("RGBA"), kind=KIND_SOURCE)
    fh = store.put_image(
        segmentation.extract_foreground(Image.fromarray(src), Image.fromarray(a)),
        kind=KIND_FOREGROUND,
        source_asset_id=store.parse_handle(sh),
    )
    monkeypatch.setattr(server, "_store", lambda: store)

    def forbidden(*args, **kw):
        raise AssertionError("billing reached")

    monkeypatch.setattr(meta_client, "billed_call", forbidden)
    with pytest.raises(composite.StageabilityError):
        server.draw(fh, "room")


def test_export_verifier_reports_grade(tmp_path):
    from agenthand.verify import verify_files

    src, a = scene()
    paths = [tmp_path / n for n in ("source.png", "final.png", "mask.png")]
    Image.fromarray(src).save(paths[0])
    Image.fromarray(src).save(paths[1])
    Image.fromarray(a).save(paths[2])
    report = verify_files(*paths)
    assert report["ok"] and report["changed_pixels"] == 0
    assert report["cut_quality"]["grade"] == "PASS"


def test_retained_proof_is_a_designated_pass_fixture():
    docs = Path(__file__).resolve().parents[1] / "docs"
    source = Image.open(docs / "proof-source-v1.png")
    mask = Image.open(docs / "proof-mask-v1.png")
    quality = grade_cut(source, mask)
    assert quality["grade"] == "PASS", quality


def test_outer_taper_and_unbounded_dark_background_are_not_top_or_support_defects():
    from PIL import ImageDraw

    alpha = Image.new("L", (200, 200))
    ImageDraw.Draw(alpha).polygon(
        [(30, 10), (120, 10), (170, 70), (170, 120), (30, 120)], fill=255
    )
    # Concave side taper, rather than a hole flanked by product.
    ImageDraw.Draw(alpha).polygon([(120, 10), (120, 65), (170, 70)], fill=0)
    source = np.full((200, 200, 3), 180, np.uint8)
    source[120:, 30:170] = 10
    q = grade_cut(source, alpha)
    assert (
        "TOP_OCCLUDED" not in q["codes"] and "SUPPORT_NOT_CAPTURED" not in q["codes"]
    ), q


def opening_scene(shape):
    a = np.zeros((300, 360), np.uint8)
    if shape == "ladder_back":
        a[30:240, 60:78] = a[30:240, 282:300] = 255
        for y in (30, 75, 120, 190):
            a[y : y + 18, 60:300] = 255
    elif shape == "stretcher_table":
        a[30:65, 40:320] = 255
        a[65:250, 60:80] = a[65:250, 280:300] = 255
        a[210:230, 60:300] = 255
    elif shape == "two_post_chair":
        a[30:250, 60:80] = a[30:250, 280:300] = 255
        a[180:205, 60:300] = 255
    elif shape == "planter_handles":
        a[85:250, 85:275] = 255
        a[35:105, 65:105] = a[35:105, 255:295] = 255
    else:
        a[50:240, 50:230] = 255
        a[80:105, 230:300] = a[185:210, 230:300] = 255
        a[80:210, 280:305] = 255
    src = np.full((*a.shape, 3), 205, np.uint8)
    src[a > 0] = (95, 65, 40)
    return src, a


@pytest.mark.parametrize(
    "shape",
    [
        "ladder_back",
        "stretcher_table",
        "two_post_chair",
        "planter_handles",
        "mug_handle",
    ],
)
def test_real_openings_pass_without_paper(tmp_path, shape):
    src, a = opening_scene(shape)
    quality = grade_cut(src, a)
    assert quality["grade"] == "PASS", quality
    assert quality["metrics"]["open_geometry_pixels"] > 0
    from test_paperfill import _store_with_cut
    from agenthand import paperfill

    store, _, _, fh = _store_with_cut(tmp_path, src, a)
    assert (
        composite.prepare_draw(store, fh)["reconstruction_report"]["status"] == "none"
    )
    report = paperfill.build_reconstruction(store, fh)
    assert report["reconstruction_pixels"] == 0, report


def test_lower_source_supported_hole_is_not_top_perforation():
    src, a = scene()
    a[50:60, 50:65] = 0
    q = grade_cut(src, a)
    assert "TOP_PERFORATED" not in q["codes"]


def test_topology_without_source_does_not_authorize_defect_or_paper():
    _, a = scene()
    a[20:35, 50:65] = 0
    a[38:48, 50:65] = 0
    q = grade_cut(None, a)
    assert q["grade"] == "PASS", q
    assert q["metrics"]["open_geometry_pixels"] > 0


@pytest.mark.parametrize("scale", [1, 4])
def test_box_contact_shadow_passes_at_multiple_resolutions(scale):
    a = np.zeros((400, 500), np.uint8)
    a[40:290, 100:400] = 255
    src = np.full((400, 500, 3), 200, np.uint8)
    src[a > 0] = (110, 80, 50)
    src[290:305, 100:400] = 40
    a = np.repeat(np.repeat(a, scale, axis=0), scale, axis=1)
    src = np.repeat(np.repeat(src, scale, axis=0), scale, axis=1)
    assert grade_cut(src, a)["grade"] == "PASS"


@pytest.mark.parametrize(
    "floor,leg",
    [
        ((200, 200, 200), (15, 15, 15)),
        ((40, 40, 40), (230, 230, 230)),
        ((150, 100, 70), (70, 110, 200)),
    ],
)
def test_missing_supports_detect_both_contrast_directions_and_chroma(floor, leg):
    src, a = scene()
    src[:] = floor
    for x in (35, 100):
        src[65:87, x : x + 4] = leg
    q = grade_cut(src, a)
    assert q["grade"] == "REPAIRABLE", q
    assert "SUPPORT_NOT_CAPTURED" in q["codes"]
    from agenthand.mask_evidence import TypedMask, typed_regions
    from agenthand.segmentation import _classify_component

    support = np.zeros_like(a, bool)
    support[65:87, 35:39] = True
    assert (
        _classify_component(support, a > 0, src, (25, 20, 115, 65), "support")
        == "support"
    )
    regions, _, _ = typed_regions(src, a, [TypedMask("support", support)], [])
    assert any(r.provenance.get("basis") == "typed_support_mask" for r in regions)
    for x in (35, 100):
        a[65:87, x : x + 4] = 255
    assert grade_cut(src, a)["grade"] == "PASS"


def test_export_verifier_fails_reshoot_even_when_fidelity_holds(tmp_path, capsys):
    import json
    from agenthand.verify import main

    src, a = scene()
    a[20:40, 50:80] = 0
    paths = [tmp_path / n for n in ("source.png", "final.png", "mask.png")]
    for path, pixels in zip(paths, (src, src, a)):
        Image.fromarray(pixels).save(path)
    assert main([str(p) for p in paths]) == 1
    report = json.loads(capsys.readouterr().out)
    assert report["fidelity_ok"] and report["changed_pixels"] == 0
    assert report["cut_quality"]["grade"] == "RESHOOT"


def test_export_verifier_regrades_paper_like_preflight(tmp_path):
    from agenthand import paperfill
    from agenthand.verify import verify_files
    from test_paperfill import _store_with_cut

    src, a = scene()
    a[27:35, 50:65] = 0
    store, sh, mh, fh = _store_with_cut(tmp_path, src, a)
    report = paperfill.build_reconstruction(store, fh)
    assert report["reconstruction_pixels"] > 0
    final = tmp_path / "final.png"
    Image.fromarray(src).save(final)
    checked = verify_files(
        store.resolve(sh),
        final,
        store.resolve(mh),
        reconstruction_path=store.resolve(report["layer_handle"]),
    )
    assert checked["ok"], checked
    assert checked["cut_quality_before"]["grade"] == "REPAIRABLE"
    assert checked["cut_quality"] == composite.preflight_cut(
        store, fh, reconstruction_handle=report["layer_handle"], require_pass=True
    )
    assert checked["cut_quality"]["grade"] == "PASS"
