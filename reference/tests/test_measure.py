"""Synthetic tests for the prototype dimension estimator.

Scenes are rendered at known geometry: a plane carrying the AH-46 card
(drawn from the real layout profile) and a rectangle of KNOWN mm size,
warped by a known homography into a photo, optionally degraded
(perspective + blur + noise + lighting gradient).

The bar for the prototype: the true size lands inside the stated range
in every case, and clean-case ranges stay at or under +/-10%. No
photo-real validation exists yet — Daniel's photos are not available —
so synthetic + degraded-synthetic is the whole of the evidence, and
ESTIMATOR.md says so.
"""

from __future__ import annotations

import json
from pathlib import Path

import cv2
import numpy as np
import pytest

from agenthand import measure

PROFILE_PATH = Path(__file__).parent / "fixtures" / "layout-profile.json"
# The layout profile is VENDORED (tests/fixtures/layout-profile.json,
# a verbatim copy of the print profile) so this battery runs on any
# clone. It used to point at ~/workspace/meta-hackathon/... and skip
# silently everywhere else — 28 estimator tests that existed only on
# Daniel's machines (architecture Gauntlet, all four lanes).
PRINT_PROFILE_PATH = (
    Path.home()
    / "workspace"
    / "meta-hackathon"
    / "scale-card"
    / "print"
    / "layout-profile.json"
)

S = 5  # plane render scale, px per mm
PLANE_W_MM, PLANE_H_MM = 560, 320
CARD_XY = (50.0, 90.0)  # card top-left on the plane, mm
PHOTO_W, PHOTO_H = 1800, 1250

INK = (17, 17, 17)
PAPER = (255, 255, 255)
CYAN = (225, 175, 0)  # BGR for #00AFE1
OBJ_BGR = (40, 80, 220)
SURFACE_BGR = (150, 160, 168)


def _mm(v: float) -> int:
    return int(round(v * S))


def render_scene(
    obj_xy_mm: tuple[float, float],
    obj_wh_mm: tuple[float, float],
    jitter: float,
    degrade: dict | None = None,
    seed: int = 7,
) -> tuple[np.ndarray, tuple[float, float], np.ndarray]:
    """Returns (photo BGR, true (w,h) mm, photo homography plane->photo)."""
    card = measure.load_card_profile(PROFILE_PATH, "AH-46")
    plane = np.zeros((_mm(PLANE_H_MM), _mm(PLANE_W_MM), 3), np.uint8)
    plane[:] = SURFACE_BGR
    cx, cy = CARD_XY

    def P(x_mm: float, y_mm: float) -> tuple[int, int]:
        return (_mm(cx + x_mm), _mm(cy + y_mm))

    # Card sheet.
    cv2.rectangle(
        plane,
        P(0, 0),
        P(card["card_width_mm"], card["card_height_mm"]),
        PAPER,
        -1,
    )
    # Bracket arms (ink), from the profile's own geometry.
    stroke_px = _mm(card["bracket_frame"]["stroke_width_mm"])
    for arm in measure._arms(card):
        cv2.line(
            plane,
            (_mm(cx + arm["start_mm"][0]), _mm(cy + arm["start_mm"][1])),
            (_mm(cx + arm["end_mm"][0]), _mm(cy + arm["end_mm"][1])),
            INK,
            stroke_px,
        )
    # White tick cuts across the arms.
    for tick in card["ticks"]:
        tx, ty = tick["centre_mm"]
        along = tick["cut_along_arm_mm"]
        across = tick["cut_across_mm"]
        horizontal = tick["arm"].endswith("-H")
        w_mm, h_mm = (along, across) if horizontal else (across, along)
        cv2.rectangle(
            plane,
            P(tx - w_mm / 2, ty - h_mm / 2),
            P(tx + w_mm / 2, ty + h_mm / 2),
            PAPER,
            -1,
        )
    # Simplified lockup furniture: cyan ring + dark hand blob + wordmark
    # bar. The detector keys on the brackets; these keep the scene honest
    # (real photos carry this interior ink too).
    ring = card["ring"]
    # True annulus: the profile radii are the ring's actual inner and
    # outer edges. (An outline circle with thickness R-r would spill
    # half the stroke beyond both radii.)
    cv2.circle(
        plane,
        P(ring["centre_mm"][0], ring["centre_mm"][1]),
        _mm(ring["outer_radius_mm"]),
        CYAN,
        -1,
    )
    cv2.circle(
        plane,
        P(ring["centre_mm"][0], ring["centre_mm"][1]),
        _mm(ring["inner_radius_mm"]),
        PAPER,
        -1,
    )
    cv2.rectangle(plane, P(52, 52), P(84, 78), INK, -1)  # hand blob
    cv2.rectangle(plane, P(38, 84), P(102, 92), INK, -1)  # wordmark bar

    # The object: a flat rectangle of known size, same plane.
    ox, oy = obj_xy_mm
    ow, oh = obj_wh_mm
    cv2.rectangle(
        plane, (_mm(ox), _mm(oy)), (_mm(ox + ow), _mm(oy + oh)), OBJ_BGR, -1
    )

    # Warp the plane into a photo with perspective: the whole tabletop
    # lands inside the photo frame, inset, with per-corner jitter.
    rng = np.random.default_rng(seed)
    src = np.array(
        [[0, 0], [plane.shape[1], 0], [plane.shape[1], plane.shape[0]], [0, plane.shape[0]]],
        np.float32,
    )
    inset_x, inset_y = 130, 110
    dst = np.array(
        [
            [inset_x, inset_y],
            [PHOTO_W - inset_x, inset_y],
            [PHOTO_W - inset_x, PHOTO_H - inset_y],
            [inset_x, PHOTO_H - inset_y],
        ],
        np.float32,
    )
    dst += rng.uniform(-jitter, jitter, dst.shape).astype(np.float32)
    H = cv2.getPerspectiveTransform(src, dst)
    photo = cv2.warpPerspective(plane, H, (PHOTO_W, PHOTO_H))
    # The warp leaves black wedges; fill them with the surface colour so
    # the scene reads as a tabletop extending past the plane card.
    wedge = (photo == 0).all(axis=2)
    photo[wedge] = SURFACE_BGR

    if degrade:
        if degrade.get("gradient"):
            g = degrade["gradient"]
            ramp = np.linspace(1 - g / 2, 1 + g / 2, PHOTO_W, dtype=np.float32)
            photo = np.clip(photo.astype(np.float32) * ramp[None, :, None], 0, 255)
            photo = photo.astype(np.uint8)
        if degrade.get("blur"):
            k = degrade["blur"]
            photo = cv2.GaussianBlur(photo, (k, k), 0)
        if degrade.get("noise"):
            noise = rng.normal(0, degrade["noise"], photo.shape)
            photo = np.clip(photo.astype(np.float32) + noise, 0, 255).astype(np.uint8)
    return photo, obj_wh_mm, H


def _save(tmp_path: Path, img: np.ndarray, name: str = "scene.png") -> str:
    path = tmp_path / name
    cv2.imwrite(str(path), img)
    return str(path)


def _assert_contained(result: dict, true_wh: tuple[float, float]) -> None:
    tw, th = true_wh
    wlo, whi = result["width_range_mm"]
    hlo, hhi = result["height_range_mm"]
    assert wlo <= tw <= whi, f"width: truth {tw} outside [{wlo}, {whi}]"
    assert hlo <= th <= hhi, f"height: truth {th} outside [{hlo}, {hhi}]"
    assert "~" in result["display"]


CLEAN_CASES = [
    # (obj_xy, obj_wh, jitter)
    ((262.0, 90.0), (180.0, 110.0), 30),
    ((262.0, 110.0), (95.0, 60.0), 110),
    ((330.0, 120.0), (60.0, 40.0), 50),
]


def test_vendored_profile_matches_the_print_profile_when_present():
    # Sync guard for the vendored fixture: on a machine that also has
    # the print profile (the scale-card print dir), the AH-46 entry
    # the battery renders from must be identical to the printed one,
    # so the fixture cannot drift from the physical card. On a clean
    # clone the print profile is absent and this guard alone skips —
    # the battery itself never does.
    if not PRINT_PROFILE_PATH.exists():
        pytest.skip("print profile not present on this machine")
    vendored = json.loads(PROFILE_PATH.read_text(encoding="utf-8"))
    printed = json.loads(PRINT_PROFILE_PATH.read_text(encoding="utf-8"))
    assert vendored["cards"]["AH-46"] == printed["cards"]["AH-46"]


@pytest.mark.parametrize("obj_xy,obj_wh,jitter", CLEAN_CASES)
def test_clean_scenes_contain_truth_within_10pct(tmp_path, obj_xy, obj_wh, jitter):
    photo, truth, _ = render_scene(obj_xy, obj_wh, jitter)
    result = measure.estimate(_save(tmp_path, photo), "AH-46", PROFILE_PATH)
    _assert_contained(result, truth)
    assert result["range_pct"] <= 10.0, result
    assert result["ticks_matched"] >= 18, result


DEGRADED_CASES = [
    ((262.0, 110.0), (95.0, 60.0), 110, {"blur": 5, "noise": 7, "gradient": 0.35}),
    ((262.0, 95.0), (140.0, 85.0), 140, {"blur": 7, "noise": 13, "gradient": 0.5}),
]


@pytest.mark.parametrize("obj_xy,obj_wh,jitter,degrade", DEGRADED_CASES)
def test_degraded_scenes_still_contain_truth(tmp_path, obj_xy, obj_wh, jitter, degrade):
    photo, truth, _ = render_scene(obj_xy, obj_wh, jitter, degrade)
    result = measure.estimate(_save(tmp_path, photo), "AH-46", PROFILE_PATH)
    _assert_contained(result, truth)
    assert result["range_pct"] <= 35.0, result


def _object_quad_photo(H) -> np.ndarray:
    corners = np.array(
        [[[262 * S, 90 * S], [442 * S, 90 * S], [442 * S, 200 * S], [262 * S, 200 * S]]],
        np.float32,
    )
    return cv2.perspectiveTransform(corners, H).reshape(-1, 2)


def test_mask_path(tmp_path):
    # The cut_out contract: a tight mask of the object.
    photo, truth, H = render_scene((262.0, 90.0), (180.0, 110.0), 30)
    quad = _object_quad_photo(H)
    mask = np.zeros(photo.shape[:2], np.uint8)
    cv2.fillPoly(mask, [quad.astype(np.int32)], 255)
    result = measure.estimate(
        _save(tmp_path, photo), "AH-46", PROFILE_PATH, mask=mask
    )
    _assert_contained(result, truth)


def test_bbox_path_overestimates_with_warning(tmp_path):
    # An axis-aligned image bbox around a perspective-skewed object
    # cannot be tight; the module's contract is to overestimate and
    # say so in the warnings, never to silently shrink.
    photo, truth, H = render_scene((262.0, 90.0), (180.0, 110.0), 30)
    quad = _object_quad_photo(H)
    x, y = quad.min(axis=0)
    x2, y2 = quad.max(axis=0)
    result = measure.estimate(
        _save(tmp_path, photo),
        "AH-46",
        PROFILE_PATH,
        bbox=(float(x), float(y), float(x2 - x), float(y2 - y)),
    )
    assert result["width_mm"] >= truth[0]
    assert result["height_mm"] >= truth[1]
    assert result["width_mm"] <= truth[0] * 1.2
    assert any("bbox" in w for w in result["warnings"])
    assert "~" in result["display"]


def test_cli(tmp_path, capsys):
    photo, truth, _ = render_scene((262.0, 110.0), (95.0, 60.0), 60)
    path = _save(tmp_path, photo)
    rc = measure.main([path, "AH-46", "--profile", str(PROFILE_PATH)])
    assert rc == 0
    result = json.loads(capsys.readouterr().out)
    _assert_contained(result, truth)
    assert result["method"].startswith("card-homography")


def test_missing_card_fails_honestly(tmp_path):
    blank = np.full((600, 900, 3), SURFACE_BGR, np.uint8)
    with pytest.raises(measure.MeasureError):
        measure.estimate(_save(tmp_path, blank), "AH-46", PROFILE_PATH)


def test_strict_recheck_refuses_a_fit_whose_ticks_vanish(tmp_path, monkeypatch):
    # False-accept regression (2026-11-02, IMG_1403): a phantom fit
    # can satisfy every non-strict gate on kept pairs, because the
    # refit only replaces pairs a strict re-observation confirms.
    # Pin the wiring of the post-size-gate strict recheck in
    # estimate(): when strict matching at the winning H finds
    # essentially nothing, the estimate must refuse with the strict
    # verification message instead of answering.
    photo, _, _ = render_scene((262.0, 110.0), (95.0, 60.0), 60)
    real_tick_matches = measure._tick_matches

    def vanishing(gray, mask, card, H, strict=False, with_lines=False):
        if strict:
            return [], 0, [], []
        return real_tick_matches(
            gray, mask, card, H, strict=strict, with_lines=with_lines
        )

    monkeypatch.setattr(measure, "_tick_matches", vanishing)
    with pytest.raises(measure.MeasureError, match="strict tick verification"):
        measure.estimate(_save(tmp_path, photo), "AH-46", PROFILE_PATH)


def test_fallback_extent_touching_the_frame_refuses(tmp_path):
    # Scenery-guard regression (2026-11-02, IMG_1403): the fallback
    # extent must refuse when the largest colour region runs out of
    # the frame. Here a verified synthetic card sits above a
    # frame-wide saturated band touching the bottom edge; the band
    # is the largest region, so the guard — not a measurement — must
    # answer.
    photo, _, _ = render_scene((262.0, 110.0), (95.0, 60.0), 60)
    photo[photo.shape[0] - 80 :, :] = OBJ_BGR
    with pytest.raises(
        measure.MeasureError, match="ran off the edge of the frame"
    ):
        measure.estimate(_save(tmp_path, photo), "AH-46", PROFILE_PATH)
