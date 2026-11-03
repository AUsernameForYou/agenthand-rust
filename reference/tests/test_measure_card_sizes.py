"""Card-size battery for the prototype dimension estimator.

Question: how small can the scale card get before accuracy breaks, and
how far does the big letter-sheet card reach? Scenes are rendered at
known geometry exactly like tests/test_measure.py, but the camera
distance varies (the physical plane grows while the photo stays
1800x1250, so the card's pixel width in frame is the independent
variable).

Profiles: tests/fixtures/scaled-card-profiles.json, generated from the
real AH-46 profile by scripts/make_scaled_card_profiles.py —
AH-42 (4.25x2.83 in, a tested size that is not printed), AH-35
(3x2 in, the printed mini) and AH-BIG (10x7.5 in, the printed
distance card). The AH-35 and AH-BIG fixture geometry is identical
to the printed AH-FAMILY v4 profile entries.

Findings this file pins down (synthetic evidence only; see
ESTIMATOR.md "How small can the card be"):
- Containment holds down to ~142 px across the card in this battery
  (the AH-42 caution-band scene, widened by the gate).
- At 117-126 px the pre-gate estimator answered with a +/-5-6% range
  but the centre missed by more than the range: the range stopped
  covering systematic error while the tick fit looked healthy — a
  silent lying band. The estimator now refuses below
  measure.MIN_CARD_PX (130 px) and widens its range through the
  130-170 px caution band; the sub-floor scenes are pinned as
  refusal regressions.
- The 3x2 mini is noisier than the size it replaced: multi-seed
  probes at 130-210 px across produced centre errors beyond the
  widened ranges on several seeds, clean and degraded, and one
  mid-distance scene flipped contained/missed when the plane
  height changed by 1 mm. The architecture review reproduced the
  misses at 134-138 px with healthy fit statistics, so the mini
  now REFUSES outside its demonstrated close-up envelope
  (measure.CARD_ENVELOPE_MIN_PX: ~190 px across or more) — the
  degraded mid-distance scene that used to answer with a ballooned
  +/-100% range is pinned as a refusal instead. No mid-distance
  AH-35 scene is pinned as contained — that would be
  seed-shopping; the battery pins its close-up and its refusals,
  and ESTIMATOR.md discloses the variance.
- AH-BIG holds +/-2.5-3.5% at 2-3x distance, degraded included,
  and contains at 4x inside the caution band.
"""

from __future__ import annotations

from pathlib import Path

import cv2
import numpy as np
import pytest

from agenthand import measure

REAL_PROFILE = Path(__file__).parent / "fixtures" / "layout-profile.json"
SCALED_PROFILE = (
    Path(__file__).parent / "fixtures" / "scaled-card-profiles.json"
)
# Both profiles are vendored under tests/fixtures, so the battery
# runs on any clone — no skip guard. (It previously keyed off a
# profile outside the repo and skipped silently on clean checkouts.)

PHOTO_W, PHOTO_H = 1800, 1250
INK = (17, 17, 17)
PAPER = (255, 255, 255)
CYAN = (225, 175, 0)  # BGR for #00AFE1
OBJ_BGR = (40, 80, 220)
SURFACE_BGR = (150, 160, 168)
AH46_FURNITURE = {
    "hand": (52.0, 52.0, 84.0, 78.0),
    "wordmark": (38.0, 84.0, 102.0, 92.0),
}
DEG_A = {"blur": 5, "noise": 7, "gradient": 0.35}
SMALL_LAYOUT = dict(
    card_xy=(50.0, 90.0), obj_xy=(262.0, 90.0), obj_wh=(180.0, 110.0)
)
BIG_LAYOUT = dict(
    card_xy=(40.0, 60.0), obj_xy=(430.0, 90.0), obj_wh=(420.0, 260.0)
)


def render_scene(
    card: dict,
    plane_w_mm: float,
    plane_h_mm: float,
    card_xy: tuple[float, float],
    obj_xy: tuple[float, float],
    obj_wh: tuple[float, float],
    jitter: float,
    degrade: dict | None = None,
    seed: int = 7,
) -> tuple[np.ndarray, float]:
    """(photo BGR, card pixel width in frame). Same renderer as
    test_measure.py, parametrized by card, plane size and furniture."""
    S = (
        5
        if plane_w_mm <= 700
        else 4
        if plane_w_mm <= 1200
        else 3
        if plane_w_mm <= 1800
        else 2
    )

    def mm(v: float) -> int:
        return int(round(v * S))

    plane = np.zeros((mm(plane_h_mm), mm(plane_w_mm), 3), np.uint8)
    plane[:] = SURFACE_BGR
    cx, cy = card_xy

    def P(x_mm: float, y_mm: float) -> tuple[int, int]:
        return (mm(cx + x_mm), mm(cy + y_mm))

    cv2.rectangle(
        plane,
        P(0, 0),
        P(card["card_width_mm"], card["card_height_mm"]),
        PAPER,
        -1,
    )
    stroke_px = mm(card["bracket_frame"]["stroke_width_mm"])
    for arm in measure._arms(card):
        cv2.line(
            plane,
            (mm(cx + arm["start_mm"][0]), mm(cy + arm["start_mm"][1])),
            (mm(cx + arm["end_mm"][0]), mm(cy + arm["end_mm"][1])),
            INK,
            stroke_px,
        )
    for tick in card["ticks"]:
        tx, ty = tick["centre_mm"]
        along, across = tick["cut_along_arm_mm"], tick["cut_across_mm"]
        w_mm, h_mm = (along, across) if tick["arm"].endswith("-H") else (across, along)
        cv2.rectangle(
            plane,
            P(tx - w_mm / 2, ty - h_mm / 2),
            P(tx + w_mm / 2, ty + h_mm / 2),
            PAPER,
            -1,
        )
    ring = card["ring"]
    cv2.circle(
        plane,
        P(ring["centre_mm"][0], ring["centre_mm"][1]),
        mm(ring["outer_radius_mm"]),
        CYAN,
        -1,
    )
    cv2.circle(
        plane,
        P(ring["centre_mm"][0], ring["centre_mm"][1]),
        mm(ring["inner_radius_mm"]),
        PAPER,
        -1,
    )
    furniture = card.get("test_furniture_mm", AH46_FURNITURE)
    hx1, hy1, hx2, hy2 = furniture["hand"]
    wx1, wy1, wx2, wy2 = furniture["wordmark"]
    cv2.rectangle(plane, P(hx1, hy1), P(hx2, hy2), INK, -1)
    cv2.rectangle(plane, P(wx1, wy1), P(wx2, wy2), INK, -1)

    ox, oy = obj_xy
    ow, oh = obj_wh
    cv2.rectangle(plane, (mm(ox), mm(oy)), (mm(ox + ow), mm(oy + oh)), OBJ_BGR, -1)

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

    cw, ch = card["card_width_mm"], card["card_height_mm"]
    corners = np.array(
        [
            [
                [cx * S, cy * S],
                [(cx + cw) * S, cy * S],
                [(cx + cw) * S, (cy + ch) * S],
                [cx * S, (cy + ch) * S],
            ]
        ],
        np.float32,
    )
    proj = cv2.perspectiveTransform(corners, H).reshape(-1, 2)
    card_px = float(
        (np.linalg.norm(proj[1] - proj[0]) + np.linalg.norm(proj[2] - proj[3])) / 2
    )
    return photo, card_px


# name, profile, card, plane_w, plane_h, jitter, degrade, layout
SCENES = [
    ("ah46_close", REAL_PROFILE, "AH-46", 560, 320, 30, None, SMALL_LAYOUT),
    ("ah46_far2x", REAL_PROFILE, "AH-46", 1120, 640, 50, None, SMALL_LAYOUT),
    ("ah42_close", SCALED_PROFILE, "AH-42", 560, 320, 30, None, SMALL_LAYOUT),
    ("ah42_mid", SCALED_PROFILE, "AH-42", 840, 480, 50, None, SMALL_LAYOUT),
    ("ah42_far2x", SCALED_PROFILE, "AH-42", 1120, 640, 50, None, SMALL_LAYOUT),
    ("ah42_probe225", SCALED_PROFILE, "AH-42", 1260, 720, 50, None, SMALL_LAYOUT),
    ("ah42_degraded_mid", SCALED_PROFILE, "AH-42", 840, 480, 60, DEG_A, SMALL_LAYOUT),
    ("ah35_close", SCALED_PROFILE, "AH-35", 560, 320, 30, None, SMALL_LAYOUT),
    ("ah35_probe175", SCALED_PROFILE, "AH-35", 980, 560, 50, None, SMALL_LAYOUT),
    ("ah35_far2x", SCALED_PROFILE, "AH-35", 1120, 640, 50, None, SMALL_LAYOUT),
    ("ah35_degraded_mid", SCALED_PROFILE, "AH-35", 840, 480, 60, DEG_A, SMALL_LAYOUT),
    ("ahbig_2x", SCALED_PROFILE, "AH-BIG", 1120, 640, 50, None, BIG_LAYOUT),
    ("ahbig_3x", SCALED_PROFILE, "AH-BIG", 1680, 960, 50, None, BIG_LAYOUT),
    ("ahbig_4x", SCALED_PROFILE, "AH-BIG", 2240, 1280, 60, None, BIG_LAYOUT),
    ("ahbig_degraded_3x", SCALED_PROFILE, "AH-BIG", 1680, 960, 60, DEG_A, BIG_LAYOUT),
]


@pytest.fixture(scope="module")
def battery(tmp_path_factory) -> dict:
    """Run every scene once; record outcomes including refusals."""
    out_dir = tmp_path_factory.mktemp("card_size_scenes")
    results = {}
    for name, profile, card_id, pw, ph, jitter, degrade, layout in SCENES:
        card = measure.load_card_profile(profile, card_id)
        photo, card_px = render_scene(
            card, pw, ph, jitter=jitter, degrade=degrade, **layout
        )
        path = out_dir / f"{name}.png"
        cv2.imwrite(str(path), photo)
        tw, th = layout["obj_wh"]
        row = {"card_px": card_px, "truth": (tw, th), "refused": False}
        try:
            r = measure.estimate(str(path), card_id, profile)
            wlo, whi = r["width_range_mm"]
            hlo, hhi = r["height_range_mm"]
            row.update(
                result=r,
                contained=bool(wlo <= tw <= whi and hlo <= th <= hhi),
            )
        except measure.MeasureError as exc:
            row.update(result=None, contained=None, refused=True, error=str(exc))
        results[name] = row
    return results


# Scenes at >= ~130 px across that must contain the truth, cleanly.
CONTAINED_SCENES = [
    "ah46_close",
    "ah46_far2x",
    "ah42_close",
    "ah42_mid",
    "ah42_far2x",
    "ah42_degraded_mid",
    "ah35_close",
    "ahbig_2x",
    "ahbig_3x",
    "ahbig_4x",
    "ahbig_degraded_3x",
]
# Sub-floor scenes (100-126 px): the silent lying band. The pre-gate
# estimator answered these with ~+/-6% ranges that missed the truth;
# the pixels-across gate must now refuse them, plainly. The AH-35
# scenes joined the band when the mini shrank to 3x2 in.
SUBFLOOR_SCENES = ["ah42_probe225", "ah35_probe175", "ah35_far2x"]


@pytest.mark.parametrize("scene", CONTAINED_SCENES)
def test_contained_scenes(battery, scene):
    row = battery[scene]
    assert not row["refused"], f"{scene}: refused ({row.get('error')})"
    assert row["contained"], (
        f"{scene}: truth {row['truth']} outside stated range "
        f"at {row['card_px']:.0f} px across"
    )
    assert row["result"]["range_pct"] <= 10.0, row["result"]


def test_ah46_close_reproduces_baseline(battery):
    row = battery["ah46_close"]
    assert row["card_px"] == pytest.approx(413, abs=15)
    assert row["result"]["ticks_matched"] >= 18
    assert row["result"]["range_pct"] <= 3.5
    # Comfortably large card: the gate reports its measured width
    # (matching the renderer's independent measurement) and does not
    # widen the range.
    assert row["result"]["card_width_px"] == pytest.approx(row["card_px"], abs=6)
    assert not any("small in frame" in w for w in row["result"]["warnings"])


@pytest.mark.parametrize("scene", SUBFLOOR_SCENES)
def test_subfloor_scenes_refuse(battery, scene):
    # Regression for the silent lying band: these scenes rendered at
    # 117-126 px across, where the pre-gate estimator quoted ~+/-6%
    # and the truth landed outside the range while the tick fit
    # looked healthy (21/24, RMS ~0.25 px). They must now refuse,
    # with the plain reason.
    row = battery[scene]
    assert row["card_px"] < measure.MIN_CARD_PX
    assert row["refused"]
    assert "too small in frame" in row["error"].lower()
    assert "bigger card" in row["error"].lower()


def test_caution_band_range_is_widened(battery):
    # AH-42 at ~142 px sits in the 130-170 px caution band: the
    # answer still contains the truth, but the quoted range must be
    # visibly wider than the +/-6.0% the ungated estimator quoted
    # for this scene, and the result must say why.
    row = battery["ah42_far2x"]
    r = row["result"]
    assert not row["refused"]
    assert row["contained"]
    assert r["range_pct"] >= 8.0, r
    assert r["card_width_px"] == pytest.approx(row["card_px"], abs=6)
    assert any("small in frame" in w for w in r["warnings"])


def test_degraded_mini_refuses_outside_its_envelope(battery):
    # AH-35 + blur/noise/gradient at mid distance: this scene used to
    # answer with a ballooned (+/-100%) range that contained the
    # truth on the battery's seed — but the architecture review
    # reproduced centre errors beyond the quoted ranges at this size
    # and distance band on other seeds, with healthy fit statistics.
    # Outside its demonstrated close-up envelope the mini refuses.
    row = battery["ah35_degraded_mid"]
    assert row["refused"]
    assert "AH-35" in row["error"]
    assert "close-up" in row["error"]
    assert "bigger card" in row["error"].lower()


def test_mini_close_up_still_answers_inside_its_envelope(battery):
    # The demonstrated envelope: ~200 px across, clean light. The
    # envelope floor must not refuse the one scene class the mini is
    # validated for.
    row = battery["ah35_close"]
    assert row["card_px"] >= measure.CARD_ENVELOPE_MIN_PX["AH-35"]
    assert not row["refused"]
    assert row["contained"]


def test_no_wild_numbers_anywhere(battery):
    # Every scene: honest refusal or truth inside the stated range.
    # Nothing else may occur — in particular, no answered scene may
    # miss its own range (the pre-gate sub-floor failure mode).
    for name, row in battery.items():
        ok = row["refused"] or row["contained"]
        assert ok, f"{name}: uncontained answer at {row['card_px']:.0f} px"


def test_gate_bracket_126_refuses_142_contains(battery):
    # The empirical bracket the gate threshold rests on: the last
    # lying-band scene (126 px) refuses, the first honest scenes
    # (142-148 px, caution band, ranges widened by the gate) still
    # answer and contain the truth. The AH-35 1.75x probe scene sat
    # at 134 px for the old 3.5x2.33 in mini; at 3x2 in it renders
    # at ~114 px and has crossed to the refusal side.
    assert battery["ah42_probe225"]["card_px"] < measure.MIN_CARD_PX
    assert battery["ah42_probe225"]["refused"]
    assert battery["ah35_probe175"]["card_px"] < measure.MIN_CARD_PX
    assert battery["ah35_probe175"]["refused"]
    assert battery["ah35_far2x"]["refused"]
    assert battery["ah42_far2x"]["card_px"] > measure.MIN_CARD_PX
    assert battery["ah42_far2x"]["contained"] is True
