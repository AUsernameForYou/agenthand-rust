import numpy as np
import pytest
from PIL import Image
from agenthand import composite, paperfill
from agenthand.mask_evidence import (
    TypedMask,
    top_footprints,
    skeleton_support_cores,
    typed_regions,
)
from test_paperfill import _store_with_cut, _png_bytes
from agenthand.assets import KIND_MASK


def setup_scene():
    src = np.full((100, 140, 3), 180, np.uint8)
    a = np.zeros((100, 140), np.uint8)
    a[20:75, 25:115] = 255
    return src, a


def test_top_concavity_requires_foreign_evidence_and_product_flanks(tmp_path):
    src, a = setup_scene()
    a[20:38, 55:80] = 0
    occ = np.zeros_like(a, bool)
    occ[15:38, 55:80] = True
    src[occ] = 10
    assert not top_footprints(a, [])
    assert len(top_footprints(a, [TypedMask("occluder", occ)], src)) == 1
    side = np.zeros_like(a, bool)
    side[20:38, 10:25] = True
    assert not top_footprints(a, [TypedMask("occluder", side)], src)
    store, sh, mh, fh = _store_with_cut(tmp_path, src, a)
    oh = store.put_bytes(
        _png_bytes(Image.fromarray(occ.astype("uint8") * 255)),
        kind=KIND_MASK,
        suffix=".png",
        source_asset_id=store.parse_handle(sh),
    )
    report = paperfill.build_reconstruction(store, fh, occluder_mask_handles=(oh,))
    assert report["status"] == "applied"
    layer = store.read_image(report["layer_handle"])
    on = np.asarray(layer.getchannel("A")) > 0
    assert not on[:20].any()  # the foreign object above the top is not papered
    assert report["regions"][0]["provenance"]["basis"] == "bounded_top_footprint"
    assert composite.preflight_cut(store, fh)["grade"] == "PASS"
    assert (
        composite.preflight_cut(store, fh, occluder_mask_handles=(oh,))["grade"]
        == "REPAIRABLE"
    )
    assert (
        composite.preflight_cut(
            store,
            fh,
            reconstruction_handle=report["layer_handle"],
            require_pass=True,
            occluder_mask_handles=(oh,),
        )["grade"]
        == "PASS"
    )
    handle, receipt = composite.draw_product(
        store,
        fh,
        "room",
        reconstruction_handle=report["layer_handle"],
        region_map_handle=report["region_map_handle"],
        reconstruction_report=report,
    )
    assert receipt["changed_opaque_pixels"] == 0


def test_typed_support_uses_cylinder_and_observed_source():
    src, a = setup_scene()
    support = np.zeros_like(a, bool)
    support[75:96, 40:44] = True
    src[support] = 10
    regions, refusals, _ = typed_regions(src, a, [TypedMask("support", support)], [])
    typed = [r for r in regions if r.provenance["basis"] == "typed_support_mask"]
    assert len(typed) == 1 and typed[0].cls == "support"
    paperfill.render_reconstruction(src, a, typed)
    assert "axis_deg" in typed[0].provenance
    src[support] = 180
    regions, refusals, _ = typed_regions(src, a, [TypedMask("support", support)], [])
    assert not any(r.provenance["basis"] == "typed_support_mask" for r in regions)
    assert any(r.code == "SUPPORT_EVIDENCE_AMBIGUOUS" for r in refusals)


def test_fused_shadow_keeps_only_narrow_observed_core():
    src, a = setup_scene()
    src[75:93, 40:44] = 10
    src[93:98, 35:65] = 10
    cores = skeleton_support_cores(src, a)
    assert len(cores) == 1
    assert cores[0].bbox[1] == 75 and cores[0].bbox[3] == 93
    assert cores[0].silhouette.sum() == 72
    # Wide shadow attached directly to the product cannot be called a leg.
    src, a = setup_scene()
    src[75:95, 35:85] = 10
    assert not skeleton_support_cores(src, a)


def test_failed_repair_refuses_before_plate(tmp_path, monkeypatch):
    src, a = setup_scene()
    src[75:90, 40:43] = 10
    src[90:98, 35:85] = 10
    store, _, _, fh = _store_with_cut(tmp_path, src, a)
    monkeypatch.setattr(
        composite, "generate_background_plate", lambda *a: pytest.fail("plate reached")
    )
    with pytest.raises(composite.StageabilityError):
        composite.draw_product(store, fh, "room")


def test_typed_evidence_cannot_bypass_support_count_cap():
    src, a = setup_scene()
    support = np.zeros_like(a, bool)
    for x in range(30, 100, 10):
        support[75:96, x : x + 2] = True
    src[support] = 10
    regions, refusals, _ = typed_regions(src, a, [TypedMask("support", support)], [])
    assert not any(r.cls == "support" for r in regions)
    assert any(r.code == "SUPPORT_COUNT_OUT_OF_RANGE" for r in refusals)


def test_product_piece_cannot_authorize_foreign_footprint():
    from agenthand.cut_quality import grade_cut
    from agenthand.segmentation import _classify_component

    src, a = setup_scene()
    a[20:38, 55:80] = 0
    piece = np.zeros_like(a, bool)
    piece[15:38, 55:80] = True
    body = a > 0
    role = _classify_component(piece, body, src, (25, 20, 115, 75), "top")
    assert role == "top_band_candidate"
    assert not top_footprints(a, [TypedMask("occluder", piece)], src)
    quality = grade_cut(src, a, occluders=[TypedMask("occluder", piece)])
    assert quality["grade"] == "RESHOOT", quality
    src[piece] = 10
    assert not top_footprints(a, [TypedMask(role, piece)], src)


def test_foreign_top_hole_is_a_perforation_with_bounded_paper():
    from agenthand.cut_quality import grade_cut

    src, a = setup_scene()
    a[25:35, 55:80] = 0
    foreign = np.zeros_like(a, bool)
    foreign[25:35, 55:80] = True
    src[foreign] = 10
    evidence = [TypedMask("occluder", foreign)]
    q = grade_cut(src, a, occluders=evidence)
    assert q["grade"] == "REPAIRABLE" and q["codes"] == ["TOP_PERFORATED"], q
    regions, _, _ = typed_regions(src, a, [], evidence)
    layer, _, _ = paperfill.render_reconstruction(src, a, regions)
    paper = np.asarray(layer.getchannel("A")) > 0
    assert paper.any() and not (paper & ~foreign).any()
    assert not (paper & (a > 0)).any()


def test_partial_foreign_record_does_not_paper_product_coloured_majority():
    src, a = setup_scene()
    a[20:38, 55:80] = 0
    foreign = np.zeros_like(a, bool)
    foreign[20:38, 55:80] = True
    src[20:38, 55:63] = 10
    assert not top_footprints(a, [TypedMask("occluder", foreign)], src)
