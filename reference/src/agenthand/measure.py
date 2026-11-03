"""Prototype dimension estimator: one flat object, same plane as the card.

EXPERIMENT ONLY. This is not a fourth MCP tool and is not wired into the
server. It exists to enable the demo's tape-measure beat: Daniel photographs
a flat object lying in the same plane as a printed agenthand card, and this
module estimates the object's height and width.

Method (classical CV, no learned models):

1. Seeds. Several independent projective seeds are generated and each
   must prove itself against the ticks: the sheet's own quad (its
   boundary is a closed curve in the adaptive ink mask, validated by
   projecting the profile's ring centre onto the detected cyan-ring
   centroid); a ring-anchored similarity grid search (rotation x
   scale, then coordinate ascent, then a projective vertex search);
   and, as a fallback, quads built from Hough line pairs bracketing
   the ring.
2. Verification. Under a seed, each of the 8 bracket arms must show
   its 3 white tick cuts where the profile says they must be (bright
   runs along the arm, gated to their expected path parameter).
3. Homography. Tick correspondences are made effectively 1-D: the
   along-path coordinate comes from the bright-run centroid, the
   lateral coordinate from the arm's measured ink-band centreline.
   The four frame corners (intersections of the fitted edge lines)
   enter the fit weighted x6 — border points alone leave a
   homography's far field ill-conditioned. RANSAC rejects outliers;
   observation and fitting iterate twice. Gates refuse sparse
   coverage or a reprojection RMS above 4 px rather than report a
   wild number.
4. Extent. The object's extent (caller-supplied bbox/mask, or the
   prototype's colour-segmentation fallback) is mapped through the
   inverse homography into the card plane, where extents are true
   millimetres.

The range is propagated from the detection residual: the RMS (in px)
becomes a positional sigma in mm at the object's location via the
homography Jacobian, is amplified by (1 + lever)^2 — projective error
grows quadratically with normalized distance from the fiducial, as
the synthetic scenes confirm — and is reported at ~2 sigma plus a 1%
floor. The output always carries the range and a tilde display
string — a bare number is never a valid output of this module.

One more gate sits between the fit and the estimate: the card's
width in image pixels. The card-size battery
(tests/test_measure_card_sizes.py) found a silent lying band — at
117-126 px across, the tick fit still looked healthy (21/24, RMS
~0.25 px) and the module quoted ~+/-6%, but the true size landed
outside the range: below ~130 px the tick cuts are under ~1.3 px
wide and systematic error (warp resampling, quantisation) outgrows
what the RMS-derived sigma models, while the fit reports no sign of
it. So below MIN_CARD_PX the module refuses outright, and through
the caution band up to COMFORT_CARD_PX it widens the range by a
systematic term fitted to the battery's centre errors and says so
in the warnings. A quoted range that can miss is not an acceptable
output of this module.

Limits: same plane only, flat objects only, the card must be fully
visible and unoccluded, and the estimate is only as honest as the
synthetic tests in tests/test_measure.py — no photo-real validation
exists yet.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from pathlib import Path

import cv2
import numpy as np

DEFAULT_PROFILE_PATH = (
    Path.home()
    / "workspace"
    / "meta-hackathon"
    / "scale-card"
    / "print"
    / "layout-profile.json"
)

METHOD = "card-homography v0 (seeded tick/corner homography, prototype)"

# Pixels-across gate (see the module docstring and the card-size
# battery in tests/test_measure_card_sizes.py): containment held at
# 134 px across the card and broke at 117-126 px, where the centre
# error reached 5-6% while the quoted range said ~6% on the wrong
# dimension — the misses were near, quiet, and invisible to the fit
# statistics. MIN_CARD_PX sits inside the empirical break bracket
# (126 missed / 134 contained). Through the caution band the range
# gains a systematic term that peaks at _SMALL_CARD_SYS_MAX at the
# floor and decays quadratically to zero at COMFORT_CARD_PX; the
# battery's worst centre error inside the band is 2.2% (134 px),
# well under the widened range.
MIN_CARD_PX = 130.0
COMFORT_CARD_PX = 170.0
_SMALL_CARD_SYS_MAX = 0.055

# Per-card envelope floors (architecture Gauntlet fix 5): the
# universal floor is not enough for every card. The 3x2 mini (AH-35)
# is measurably noisier than the size it replaced: the architecture
# review reproduced centre errors beyond the quoted ranges at
# 134-138 px across with healthy-looking fit statistics, and
# multi-seed probes missed at points across 130-210 px. The one
# envelope the battery demonstrates as dependable is close-ups at
# ~200 px across in clean light, so AH-35 refuses below 190 px —
# inside its demonstrated close-up envelope or not at all. Other
# cards keep the universal floor until their own batteries say
# otherwise.
CARD_ENVELOPE_MIN_PX = {"AH-35": 190.0}

# Arm name -> (vertex key, direction in card-mm space). Ticks in the
# profile are ordered by fraction_from_vertex ascending, which is the
# start->end order along every arm under these directions.
_ARM_GEOMETRY = {
    "TL-H": ("TL", (1.0, 0.0)),
    "TL-V": ("TL", (0.0, 1.0)),
    "TR-H": ("TR", (-1.0, 0.0)),
    "TR-V": ("TR", (0.0, 1.0)),
    "BL-V": ("BL", (0.0, -1.0)),
    "BL-H": ("BL", (1.0, 0.0)),
    "BR-V": ("BR", (0.0, -1.0)),
    "BR-H": ("BR", (-1.0, 0.0)),
}


class MeasureError(RuntimeError):
    """Raised when the card or the object cannot be measured honestly."""


# --------------------------------------------------------------------------
# Profile


def load_card_profile(profile_path: Path, card_id: str) -> dict:
    data = json.loads(Path(profile_path).read_text(encoding="utf-8"))
    cards = data.get("cards", {})
    if card_id not in cards:
        raise MeasureError(
            f"Card {card_id!r} not in profile {profile_path} "
            f"(has: {', '.join(sorted(cards)) or 'none'})"
        )
    return cards[card_id]


def _frame_mm(card: dict) -> dict[str, np.ndarray]:
    verts = card["bracket_frame"]["corner_vertices_centreline_mm"]
    return {k: np.array(v, dtype=np.float64) for k, v in verts.items()}


def _arms(card: dict) -> list[dict]:
    """Bracket arms with their start vertex, direction, and tick list."""
    frame = _frame_mm(card)
    arm_len = float(card["bracket_frame"]["arm_centreline_length_mm"])
    by_arm: dict[str, list[dict]] = {}
    for tick in card["ticks"]:
        by_arm.setdefault(tick["arm"], []).append(tick)
    arms = []
    for name, ticks in sorted(by_arm.items()):
        vertex_key, direction = _ARM_GEOMETRY[name]
        start = frame[vertex_key]
        dirv = np.array(direction, dtype=np.float64)
        arms.append(
            {
                "name": name,
                "start_mm": start,
                "end_mm": start + dirv * arm_len,
                "ticks_mm": [
                    np.array(t["centre_mm"], dtype=np.float64)
                    for t in sorted(ticks, key=lambda t: t["fraction_from_vertex"])
                ],
            }
        )
    return arms


# --------------------------------------------------------------------------
# Sampling helpers


def _project(H: np.ndarray, points: np.ndarray) -> np.ndarray:
    pts = np.asarray(points, dtype=np.float64).reshape(-1, 1, 2)
    return cv2.perspectiveTransform(pts, H).reshape(-1, 2)


def _sample_along(gray: np.ndarray, p: np.ndarray, q: np.ndarray, n: int = 240):
    xs = np.linspace(p[0], q[0], n)
    ys = np.linspace(p[1], q[1], n)
    values = np.array(
        [
            cv2.getRectSubPix(gray, (1, 1), (float(x), float(y)))[0, 0]
            for x, y in zip(xs, ys)
        ],
        dtype=np.float64,
    )
    return values, np.stack([xs, ys], axis=1)


def _bright_runs(values: np.ndarray) -> list[tuple[int, int]]:
    low, high = np.percentile(values, 10), np.percentile(values, 90)
    threshold = (low + high) / 2
    above = values > threshold
    runs = []
    start = None
    for i, flag in enumerate(above):
        if flag and start is None:
            start = i
        elif not flag and start is not None:
            runs.append((start, i - 1))
            start = None
    if start is not None:
        runs.append((start, len(values) - 1))
    return runs


def _arm_tick_centres(
    gray: np.ndarray,
    start_img: np.ndarray,
    end_img: np.ndarray,
    expected_img: list[np.ndarray],
    tol: float = 0.10,
    max_run_frac: float = 0.12,
) -> list[np.ndarray] | None:
    """Bright tick-cut centres along one arm, matched to expectations.

    ``expected_img`` are the tick positions projected under the current
    transform. A bright run counts only if its centre's path parameter
    lands within tolerance of an expected tick's parameter — this
    rejects the bright paper beyond the arm's end and any off-by-one
    labelling, both of which otherwise poison the homography fit.
    """
    values, points = _sample_along(gray, start_img, end_img)
    kernel = np.ones(5) / 5
    smooth = np.convolve(values, kernel, mode="same")
    runs = _bright_runs(smooth)
    low = float(np.percentile(smooth, 10))
    n = len(values)
    plausible = [
        (a, b)
        for a, b in runs
        if 2 <= (b - a + 1) <= max(6, int(max_run_frac * n))
    ]
    axis = end_img - start_img
    denom = float(axis @ axis)
    if denom <= 0:
        return None

    def param(pt: np.ndarray) -> float:
        return float((pt - start_img) @ axis) / denom

    run_centres = []
    for a, b in plausible:
        weights = np.clip(smooth[a : b + 1] - low, 0, None)
        if weights.sum() <= 0:
            continue
        centre = (points[a : b + 1] * weights[:, None]).sum(axis=0) / weights.sum()
        run_centres.append((param(centre), centre))
    centres: list[np.ndarray] = []
    used: set[int] = set()
    for expected_pt in expected_img:
        target = param(expected_pt)
        best_i, best_d = None, tol
        for i, (p, centre) in enumerate(run_centres):
            if i in used:
                continue
            if abs(p - target) < best_d:
                best_i, best_d = i, abs(p - target)
        if best_i is None:
            return None
        used.add(best_i)
        centres.append(run_centres[best_i][1])
    return centres


def _arm_centreline_points(
    mask: np.ndarray,
    start_img: np.ndarray,
    end_img: np.ndarray,
    half_width: float,
) -> np.ndarray | None:
    """Centreline points of one arm, measured from the ink mask.

    At each path parameter the ink band's lateral centroid is one
    centreline point; tick cuts register as no-band parameters and
    are skipped. These points average hundreds of mask pixels, so
    they are far more precise than any single sampled feature.
    """
    axis = end_img - start_img
    length = float(np.linalg.norm(axis))
    if length <= 0:
        return None
    direction = axis / length
    normal = np.array([-direction[1], direction[0]])
    points = []
    for t in np.linspace(0.08, 0.92, 22):
        base = start_img + t * axis
        ts = np.linspace(-half_width, half_width, 33)
        pts = base[None, :] + ts[:, None] * normal[None, :]
        values = np.array(
            [
                cv2.getRectSubPix(mask, (1, 1), (float(x), float(y)))[0, 0]
                for x, y in pts
            ],
            dtype=np.float64,
        )
        if values.sum() < 0.25 * 255 * len(ts) * 0.3:
            continue
        offset = float((ts * values).sum() / values.sum())
        points.append(base + offset * normal)
    if len(points) < 6:
        return None
    return np.array(points)


def _fit_line(points: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    centroid = points.mean(axis=0)
    _, _, vt = np.linalg.svd(points - centroid, full_matrices=False)
    return centroid, vt[0]


def _line_intersection(
    p1: np.ndarray, d1: np.ndarray, p2: np.ndarray, d2: np.ndarray
) -> np.ndarray | None:
    A = np.stack([d1, -d2], axis=1)
    if abs(np.linalg.det(A)) < 1e-9:
        return None
    t = np.linalg.solve(A, p2 - p1)
    return p1 + t[0] * d1


_EDGE_ARMS = {
    "top": ("TL-H", "TR-H"),
    "bottom": ("BL-H", "BR-H"),
    "left": ("TL-V", "BL-V"),
    "right": ("TR-V", "BR-V"),
}


def _tick_matches(
    gray: np.ndarray,
    mask: np.ndarray,
    card: dict,
    H: np.ndarray,
    strict: bool = True,
    with_lines: bool = False,
) -> tuple[list, int, list, dict]:
    """(tick pairs, arms hit, corner pairs, edge lines) under H.

    Edge lines (the frame's four sides, fitted from merged arm
    centreline points) are only measured when ``with_lines`` — they
    cost real sampling and the similarity search does not need them.

    ``strict`` gates runs tightly to their expected path parameter —
    right for scoring a fitted homography. The similarity search uses
    loose gating (perspective error alone can exceed the strict gate);
    RANSAC in the fit guards against the looser matches.
    """
    tol, max_run_frac = (0.11, 0.12) if strict else (0.20, 0.30)
    matched: list[tuple[np.ndarray, np.ndarray]] = []
    arms_hit = 0
    arm_points: dict[str, np.ndarray] = {}
    arm_lines: dict[str, tuple[np.ndarray, np.ndarray]] = {}
    stroke_mm = float(card["bracket_frame"]["stroke_width_mm"])
    arm_len_mm = float(card["bracket_frame"]["arm_centreline_length_mm"])
    pending: list[tuple[dict, list[np.ndarray]]] = []
    for arm in _arms(card):
        start_img = _project(H, arm["start_mm"][None])[0]
        end_img = _project(H, arm["end_mm"][None])[0]
        direction = end_img - start_img
        arm_len_img = float(np.linalg.norm(direction))
        half_width = 0.75 * stroke_mm * (arm_len_img / arm_len_mm)
        if with_lines:
            pts = _arm_centreline_points(mask, start_img, end_img, half_width)
            if pts is not None:
                arm_points[arm["name"]] = pts
                arm_lines[arm["name"]] = _fit_line(pts)
        mask_values, _ = _sample_along(mask, start_img, end_img, n=120)
        if float((mask_values > 0).mean()) < 0.55:
            continue
        expected_img = [
            _project(H, t[None])[0] for t in arm["ticks_mm"]
        ]
        centres = _arm_tick_centres(
            gray, start_img, end_img, expected_img,
            tol=tol, max_run_frac=max_run_frac,
        )
        if centres is None:
            continue
        arms_hit += 1
        pending.append((arm, centres))
    for arm, centres in pending:
        line = arm_lines.get(arm["name"])
        for mm_pt, img_pt in zip(arm["ticks_mm"], centres):
            if line is not None:
                # Keep the accurate along-path coordinate; take the
                # lateral coordinate from the measured centreline.
                line_pt, line_dir = line
                along = float((img_pt - line_pt) @ line_dir)
                img_pt = line_pt + along * line_dir
            matched.append((mm_pt, img_pt))
    # Frame edges from merged arm centreline points; corners from
    # intersecting them. Tick points alone (interior to the edges,
    # sometimes clustered) leave the homography's perspective terms
    # ill-conditioned; the edges and corners pin the configuration.
    frame = _frame_mm(card)
    edges: dict[str, tuple[np.ndarray, np.ndarray]] = {}
    corners: list[tuple[np.ndarray, np.ndarray]] = []
    if with_lines:
        for edge, arm_names in _EDGE_ARMS.items():
            pts = [arm_points[a] for a in arm_names if a in arm_points]
            if pts:
                merged = np.concatenate(pts, axis=0)
                if len(merged) >= 8:
                    edges[edge] = _fit_line(merged)
        for vertex, (e_a, e_b) in {
            "TL": ("top", "left"),
            "TR": ("top", "right"),
            "BL": ("bottom", "left"),
            "BR": ("bottom", "right"),
        }.items():
            if e_a in edges and e_b in edges:
                corner = _line_intersection(
                    edges[e_a][0], edges[e_a][1], edges[e_b][0], edges[e_b][1]
                )
                if corner is not None:
                    corners.append((frame[vertex], corner))
    return matched, arms_hit, corners, edges


# --------------------------------------------------------------------------
# Card detection: ring anchor + similarity search + tick homography


def _ink_mask(gray: np.ndarray) -> np.ndarray:
    blurred = cv2.medianBlur(gray, 3)
    return cv2.adaptiveThreshold(
        blurred,
        255,
        cv2.ADAPTIVE_THRESH_GAUSSIAN_C,
        cv2.THRESH_BINARY_INV,
        51,
        12,
    )


def _nearest_px(v: float) -> int:
    # Round half away from zero (Rust f64::round). Python's round()
    # is banker's rounding and would disagree with the Rust gate on
    # .5 samples — the two builds must sample the same pixels.
    return int(math.floor(abs(v) + 0.5)) * (1 if v >= 0 else -1)


def _verify_card_fit(
    image_bgr: np.ndarray, H: np.ndarray, card: dict, tick_count: int
) -> None:
    """M2 acceptance gate on the winning fit (ported from the Rust
    measurefix, 2026-11-02). Tick counts are a periodic feature:
    fence boards counterfeited 12 on IMG_1403 and denim seams
    counterfeited 15 on IMG_1491, and both fits then certified
    themselves against the pixel floor. So the fit must show the
    card's own, non-periodic features — in order, first failure
    reported: a cyan ring at the ring position, a white paper border
    around dark bracket arms, at least half the tick cuts, and the
    card's aspect. Raises MeasureError on the first failed check."""
    hsv = cv2.cvtColor(image_bgr, cv2.COLOR_BGR2HSV)
    cyan = cv2.inRange(
        hsv, np.array([88, 70, 70]), np.array([104, 255, 255])
    )
    gray = cv2.cvtColor(image_bgr, cv2.COLOR_BGR2GRAY)
    h_img, w_img = gray.shape
    W = float(card["card_width_mm"])
    Hh = float(card["card_height_mm"])
    ring = card["ring"]
    cx, cy = (float(v) for v in ring["centre_mm"])
    R = float(ring["outer_radius_mm"])
    refusal = (
        " — shoot closer with the card flat and fully in frame."
    )

    def sample_cyan(mm_pt: tuple[float, float]) -> bool:
        q = _project(H, np.array(mm_pt, dtype=np.float64)[None])[0]
        x, y = _nearest_px(q[0]), _nearest_px(q[1])
        return 0 <= x < w_img and 0 <= y < h_img and cyan[y, x] > 0

    # 1. Ring coverage, radius searched (the printed v4 ring is
    # ~0.43x the profile radius, so the profile radius is not the
    # answer — the search spans 0.25-0.95 R).
    best_f, best_hits = 0.0, [False] * 96
    for k in range(5, 20):
        r = k * 0.05 * R
        hits = [
            sample_cyan(
                (
                    cx + r * math.cos(j * 2 * math.pi / 96),
                    cy + r * math.sin(j * 2 * math.pi / 96),
                )
            )
            for j in range(96)
        ]
        f = sum(hits) / 96
        if f > best_f:
            best_f, best_hits = f, hits
    sectors = sum(
        1 for s in range(6) if any(best_hits[s * 16 : (s + 1) * 16])
    )
    spill_pts = []
    for rr in (1.2 * R, 1.35 * R, 1.5 * R):
        for j in range(96):
            a = j * 2 * math.pi / 96
            mm = (cx + rr * math.cos(a), cy + rr * math.sin(a))
            if 0 <= mm[0] <= W and 0 <= mm[1] <= Hh:
                spill_pts.append(mm)
    spill = (
        sum(1 for mm in spill_pts if sample_cyan(mm)) / len(spill_pts)
        if spill_pts
        else 0.0
    )
    if not (best_f >= 0.30 and sectors >= 4 and spill <= 0.35 * best_f):
        raise MeasureError(
            "Card not verified in this photo (no cyan ring at the "
            "card's ring position)" + refusal
        )

    # 2. Paper border vs ink: the border band halfway between the
    # bracket frame and the sheet edge must read white while the
    # bracket arms read dark.
    frame = _frame_mm(card)
    half = float(card["bracket_frame"]["stroke_width_mm"]) / 2
    xl = (min(frame["TL"][0], frame["BL"][0]) - half) / 2
    xr = (max(frame["TR"][0], frame["BR"][0]) + half + W) / 2
    yt = (min(frame["TL"][1], frame["TR"][1]) - half) / 2
    yb = (max(frame["BL"][1], frame["BR"][1]) + half + Hh) / 2
    paper_pts: list[tuple[float, float]] = []
    for t in np.linspace(0, 1, 9):
        paper_pts.append((xl + t * (xr - xl), yt))
        paper_pts.append((xl + t * (xr - xl), yb))
        paper_pts.append((xl, yt + t * (yb - yt)))
        paper_pts.append((xr, yt + t * (yb - yt)))
    ink_pts: list[tuple[float, float]] = []
    for arm in _arms(card):
        s, e = arm["start_mm"], arm["end_mm"]
        for f in (0.1, 0.375, 0.625, 0.9):
            ink_pts.append(tuple(s + f * (e - s)))

    def sample_gray(mm_pt: tuple[float, float]) -> int | None:
        q = _project(H, np.array(mm_pt, dtype=np.float64)[None])[0]
        x, y = _nearest_px(q[0]), _nearest_px(q[1])
        return None if not (0 <= x < w_img and 0 <= y < h_img) else int(gray[y, x])

    paper_vals = [v for v in (sample_gray(p) for p in paper_pts) if v is not None]
    ink_vals = [v for v in (sample_gray(p) for p in ink_pts) if v is not None]
    paper_fail = (
        "Card not verified in this photo (no white paper border "
        "around dark bracket arms)" + refusal
    )
    if len(paper_vals) < len(paper_pts) / 2 or len(ink_vals) < len(ink_pts) / 2:
        raise MeasureError(paper_fail)
    paper = float(np.median(paper_vals))
    ink = float(np.median(ink_vals))
    if not (paper >= 2.0 * max(ink, 1) and paper - ink >= 40):
        raise MeasureError(paper_fail)

    # 3. Ticks, secondary.
    total = len(card["ticks"])
    if tick_count < 0.5 * total:
        raise MeasureError(
            f"Card not verified in this photo (only {tick_count}/"
            f"{total} tick cuts matched)" + refusal
        )

    # 4. Aspect.
    corners = _project(
        H, np.array([[0, 0], [W, 0], [W, Hh], [0, Hh]], dtype=np.float64)
    )
    c0, c1, c2, c3 = corners
    across = float(np.linalg.norm(c1 - c0) + np.linalg.norm(c2 - c3))
    down = float(np.linalg.norm(c3 - c0) + np.linalg.norm(c2 - c1))
    factor = (across / down) / (W / Hh)
    if not (1 / 1.5 <= factor <= 1.5):
        raise MeasureError(
            f"Card not verified in this photo (the fitted quad's "
            f"aspect is {factor:.2f}x the card's)" + refusal
        )


def _ring_centroid(image_bgr: np.ndarray) -> tuple[np.ndarray, float] | None:
    """(centroid, pixel area) of the lockup's cyan ring, or None."""
    hsv = cv2.cvtColor(image_bgr, cv2.COLOR_BGR2HSV)
    cyan = cv2.inRange(hsv, (88, 70, 70), (104, 255, 255))
    moments = cv2.moments(cyan)
    if moments["m00"] < 200:
        return None
    centroid = np.array(
        [moments["m10"] / moments["m00"], moments["m01"] / moments["m00"]]
    )
    # m00 sums pixel VALUES (255 per cyan pixel), not the pixel count.
    return centroid, float(moments["m00"]) / 255.0


def _ring_shape_candidates(
    image_bgr: np.ndarray, card: dict
) -> list[tuple[np.ndarray, float, int]]:
    """Ring-shaped cyan blobs, best first: (centre, outer_px, n).

    Port of Rust 96f51e9 (CARDDETECT, 2026-11-02). The all-cyan
    centroid is a denim/scenery average: on IMG_1491 it sits on the
    jacket, and under blur+noise its area mis-seeds the scale by
    ~40%. The printed ring is 3 arcs, so single components are arcs,
    not rings: nearby components are grouped first (union-find, bbox
    gap <= half the larger side, sides within 2.5x), then each group
    must look like an annulus under whitening (eigenvalue ratio <= 4,
    <= 5% of points outside the [inner-margin, outer+margin] band,
    fill in [0.5, 1.2], >= 9 of 12 sectors occupied, outer >= 5 px).
    Returns [] when nothing ring-shaped is present; callers fall
    back to the centroid path.
    """
    hsv = cv2.cvtColor(image_bgr, cv2.COLOR_BGR2HSV)
    mask = cv2.inRange(hsv, (88, 70, 70), (104, 255, 255))
    n_labels, labels, stats, _ = cv2.connectedComponentsWithStats(
        mask, connectivity=8
    )
    # stats rows: [left, top, width, height, area]; label 0 is background.
    comps = []
    for lab in range(1, n_labels):
        left, top, w, h, area = (int(v) for v in stats[lab])
        if area >= 3:
            comps.append((lab, left, top, w, h, area))
    if len(comps) > 3000:
        comps = sorted(comps, key=lambda c: -c[5])[:3000]
        comps = sorted(comps, key=lambda c: c[0])
    if not comps:
        return []
    parent = list(range(len(comps)))

    def find(a: int) -> int:
        while parent[a] != a:
            parent[a] = parent[parent[a]]
            a = parent[a]
        return a

    def union(a: int, b: int) -> None:
        ra, rb = find(a), find(b)
        if ra != rb:
            parent[rb] = ra

    for i in range(len(comps)):
        _, x0a, y0a, wa, ha, _ = comps[i]
        x1a, y1a = x0a + wa - 1, y0a + ha - 1
        side_a = max(wa, ha)
        for j in range(i + 1, len(comps)):
            _, x0b, y0b, wb, hb, _ = comps[j]
            side_b = max(wb, hb)
            if max(side_a, side_b) / min(side_a, side_b) > 2.5:
                continue
            x1b, y1b = x0b + wb - 1, y0b + hb - 1
            gap_x = max(0, x0b - x1a - 1, x0a - x1b - 1)
            gap_y = max(0, y0b - y1a - 1, y0a - y1b - 1)
            if max(gap_x, gap_y) <= 0.5 * max(side_a, side_b):
                union(i, j)
    groups: dict[int, list[int]] = {}
    for idx in range(len(comps)):
        groups.setdefault(find(idx), []).append(idx)
    r_out = float(card["ring"]["outer_radius_mm"])
    r_in = float(card["ring"]["inner_radius_mm"])
    k = r_in / r_out if r_out > 0 else 0.0
    candidates: list[tuple[np.ndarray, float, int]] = []
    for members in groups.values():
        labs = [comps[m][0] for m in members]
        pts = np.column_stack(np.where(np.isin(labels, labs))).astype(
            np.float64
        )[:, ::-1]  # (x, y)
        n = len(pts)
        if n < 20:
            continue
        mean = pts.mean(axis=0)
        centred = pts - mean
        cov = np.cov(centred.T)
        tr = float(cov[0, 0] + cov[1, 1])
        det = float(cov[0, 0] * cov[1, 1] - cov[0, 1] ** 2)
        disc = max(tr * tr / 4 - det, 0.0)
        l1 = tr / 2 + math.sqrt(disc)
        l2 = tr / 2 - math.sqrt(disc)
        if l2 <= 0 or l1 / l2 > 4:
            continue
        theta = 0.5 * math.atan2(2 * cov[0, 1], cov[0, 0] - cov[1, 1])
        ct, st = math.cos(theta), math.sin(theta)
        a = ct * centred[:, 0] + st * centred[:, 1]
        b = -st * centred[:, 0] + ct * centred[:, 1]
        a = a * (l2 / l1) ** 0.25
        b = b * (l1 / l2) ** 0.25
        m2 = float(np.mean(a**2 + b**2))
        outer = math.sqrt(2 * m2 / (1 + k * k)) if k < 1 else 0.0
        inner = k * outer
        if outer < 5:
            continue
        margin = max(1.0, 0.25 * (outer - inner))
        r = np.sqrt(a**2 + b**2)
        if float(np.mean(r < inner - margin)) > 0.05:
            continue
        if float(np.mean(r > outer + margin)) > 0.05:
            continue
        fill = n / (math.pi * (outer**2 - inner**2))
        if not (0.5 <= fill <= 1.2):
            continue
        sectors = set(
            min(int((math.atan2(bb, aa) % (2 * math.pi)) / (2 * math.pi) * 12), 11)
            for aa, bb in zip(a, b)
        )
        if len(sectors) < 9:
            continue
        candidates.append((mean, outer, n))
    candidates.sort(key=lambda c: (-c[2], c[0][1], c[0][0]))
    return candidates[:4]


def _sheet_seed(
    mask: np.ndarray, card: dict, ring_centroid: np.ndarray
) -> np.ndarray | None:
    """Homography seed from the card sheet's own quad, or None.

    The adaptive ink mask fires along the paper/surface step, so
    the sheet's boundary is a thin closed curve in the mask — local
    by construction, hence immune to lighting gradients that defeat
    absolute brightness thresholds. The longest mask contours are
    tried as quads; a candidate is accepted only if the profile's
    ring centre projects onto the detected ring centroid (the
    tabletop boundary and the object's outline also produce long
    contours, but they put the ring in the wrong place).
    """
    contours, _ = cv2.findContours(
        mask, cv2.RETR_EXTERNAL, cv2.CHAIN_APPROX_SIMPLE
    )
    contours = sorted(
        contours, key=lambda c: cv2.arcLength(c, True), reverse=True
    )[:6]
    w = float(card["card_width_mm"])
    h = float(card["card_height_mm"])
    sheet_mm = np.array([[0, 0], [w, 0], [w, h], [0, h]], dtype=np.float64)
    ring_mm = np.array(card["ring"]["centre_mm"], dtype=np.float64)
    for contour in contours:
        if cv2.arcLength(contour, True) < 400:
            continue
        approx = None
        for eps_factor in (0.01, 0.02, 0.03):
            candidate = cv2.approxPolyDP(
                contour, eps_factor * cv2.arcLength(contour, True), True
            )
            if len(candidate) == 4:
                approx = candidate
                break
        if approx is None:
            continue
        pts = approx.reshape(-1, 2).astype(np.float64)
        sums = pts.sum(axis=1)
        diffs = pts[:, 0] - pts[:, 1]
        ordered = np.array(
            [
                pts[np.argmin(sums)],   # TL
                pts[np.argmax(diffs)],  # TR
                pts[np.argmax(sums)],   # BR
                pts[np.argmin(diffs)],  # BL
            ]
        )
        H, _ = cv2.findHomography(sheet_mm, ordered, 0)
        if H is None:
            continue
        projected_ring = _project(H.astype(np.float64), ring_mm[None])[0]
        if np.linalg.norm(projected_ring - ring_centroid) <= 30:
            return H.astype(np.float64)
    return None


def _hough_quad_seed(
    gray: np.ndarray,
    mask: np.ndarray,
    card: dict,
    ring_centroid: np.ndarray,
    scale0: float,
) -> np.ndarray | None:
    """Projective seed from Hough line quads, or None.

    Fallback for scenes where both the similarity search and the
    sheet-contour seed fail (strong perspective + degradation). The
    sheet's sides are among the longest straight edges present;
    candidate quads are built from line pairs bracketing the ring,
    filtered by the ring-projection check, and the survivor with
    the best loose tick score wins. Decoys are harmless: a wrong
    rectangle puts the ring in the wrong place or scores no ticks.
    """
    edges = cv2.Canny(mask, 50, 150)
    segments = cv2.HoughLinesP(
        edges, 1, np.pi / 180, threshold=80, minLineLength=110, maxLineGap=20
    )
    if segments is None:
        return None
    segs = []
    for x1, y1, x2, y2 in segments.reshape(-1, 4):
        p = np.array([x1, y1], dtype=np.float64)
        q = np.array([x2, y2], dtype=np.float64)
        length = float(np.linalg.norm(q - p))
        if length < 110:
            continue
        direction = (q - p) / length
        if direction[0] < 0:
            direction = -direction
        normal = np.array([-direction[1], direction[0]])
        rho = float(normal @ p)
        segs.append((length, rho, normal, direction))
    if len(segs) < 4:
        return None
    segs.sort(key=lambda s: -s[0])
    segs = segs[:28]
    # Two orientation families by normal angle.
    ref = math.atan2(segs[0][2][1], segs[0][2][0])
    fam_a, fam_b = [], []
    for s in segs:
        ang = math.atan2(s[2][1], s[2][0])
        if abs(math.sin(ref - ang)) < 0.35:
            fam_a.append(s)
        else:
            fam_b.append(s)
    if len(fam_a) < 2 or len(fam_b) < 2:
        return None

    w = float(card["card_width_mm"])
    h = float(card["card_height_mm"])
    sheet_mm = np.array([[0, 0], [w, 0], [w, h], [0, h]], dtype=np.float64)
    ring_mm = np.array(card["ring"]["centre_mm"], dtype=np.float64)

    def pairs_bracketing(fam):
        # Expected separation: the sheet dimension along the
        # family's normal direction.
        n_mean = np.mean([s[2] for s in fam], axis=0)
        expected_px = (h if abs(n_mean[1]) > abs(n_mean[0]) else w) * scale0
        out = []
        for i in range(len(fam)):
            for j in range(i + 1, len(fam)):
                r1, r2 = fam[i][1], fam[j][1]
                n = fam[i][2]
                centre_side = float(n @ ring_centroid)
                lo, hi = min(r1, r2), max(r1, r2)
                if lo < centre_side < hi and 0.45 * expected_px < hi - lo < 2.2 * expected_px:
                    out.append((fam[i], fam[j]))
        return out

    quads = []
    for a1, a2 in pairs_bracketing(fam_a):
        for b1, b2 in pairs_bracketing(fam_b):
            corners = []
            ok = True
            for s_a in (a1, a2):
                for s_b in (b1, b2):
                    pt = _line_intersection(
                        s_a[2] * s_a[1], s_a[3], s_b[2] * s_b[1], s_b[3]
                    )
                    if pt is None:
                        ok = False
                        break
                    corners.append(pt)
                if not ok:
                    break
            if not ok or len(corners) != 4:
                continue
            pts = np.array(corners)
            sums = pts.sum(axis=1)
            diffs = pts[:, 0] - pts[:, 1]
            ordered = np.array(
                [
                    pts[np.argmin(sums)],
                    pts[np.argmax(diffs)],
                    pts[np.argmax(sums)],
                    pts[np.argmin(diffs)],
                ]
            )
            H, _ = cv2.findHomography(sheet_mm, ordered, 0)
            if H is None:
                continue
            H = H.astype(np.float64)
            projected_ring = _project(H, ring_mm[None])[0]
            if np.linalg.norm(projected_ring - ring_centroid) > 30:
                continue
            quads.append(H)
    best = None
    for H in quads[:12]:
        t, a, _, _ = _tick_matches(gray, mask, card, H, strict=False)
        score = a + 0.02 * len(t)
        if best is None or score > best[0]:
            best = (score, H)
    return best[1] if best is not None else None


def _similarity(
    ring_mm: np.ndarray, centre_img: np.ndarray, theta: float, scale: float
) -> np.ndarray:
    """3x3 similarity mapping card-mm -> image, anchored at the ring."""
    c, s = math.cos(theta), math.sin(theta)
    R = np.array([[c, -s], [s, c]])
    t = centre_img - scale * (R @ ring_mm)
    H = np.eye(3)
    H[:2, :2] = scale * R
    H[:2, 2] = t
    return H


def _detect_card(
    gray: np.ndarray, image_bgr: np.ndarray, card: dict, _use_shape: bool = False
) -> tuple[np.ndarray, list[tuple[np.ndarray, np.ndarray]], dict[str, np.ndarray]]:
    mask = _ink_mask(gray)
    ring = _ring_centroid(image_bgr)
    if ring is None:
        raise MeasureError("Cyan ring not found; card not detectable.")
    centre, ring_area_px = ring
    ring_mm = np.array(card["ring"]["centre_mm"], dtype=np.float64)
    frame = _frame_mm(card)

    # Coarse scale from the ring itself: its annulus area is known in
    # mm^2 from the profile, so the equivalent pixel radius gives
    # px-per-mm with no dependence on scene ink (decoy-proof).
    r_out = float(card["ring"]["outer_radius_mm"])
    r_in = float(card["ring"]["inner_radius_mm"])
    ring_area_mm2 = math.pi * (r_out**2 - r_in**2)
    scale0 = math.sqrt(ring_area_px / ring_area_mm2)
    if _use_shape:
        # Ring-shaped seed (Rust 96f51e9), used ONLY as a fallback
        # when the centroid-anchored search fails: the all-cyan
        # centroid is dragged off the card by denim/scenery cyan and
        # its area mis-seeds the scale under blur+noise (the
        # corrected thin ring is the worst case), but replacing the
        # anchor outright regressed a contained mid-distance scene
        # (ah42_degraded_mid: shape fit RMS 0.38, centre 5% off, range
        # missed truth; centroid fit RMS 0.31, contained). So the
        # centroid path stays primary and its result, when it exists,
        # is never displaced by the shape path.
        shapes = _ring_shape_candidates(image_bgr, card)
        if not shapes:
            raise MeasureError("Cyan ring not found; card not detectable.")
        centre, outer_px, _ = shapes[0]
        scale0 = outer_px / r_out if r_out > 0 else scale0

    if int((mask > 0).sum()) < 500:
        raise MeasureError("Too little ink; card not detectable.")

    def objective(params: np.ndarray) -> tuple[float, list, int]:
        dx, dy, theta, log_scale = params
        H = _similarity(
            ring_mm, centre + np.array([dx, dy]), theta, math.exp(log_scale)
        )
        ticks, arms_hit, corners, _ = _tick_matches(
            gray, mask, card, H, strict=False
        )
        score = arms_hit + 0.02 * (len(ticks) + len(corners))
        return score, ticks + corners, arms_hit

    # Coarse grid over rotation x scale, ring-anchored. The tick
    # objective is flat zero until the arms land within ~half a stroke
    # width, so a local ascent needs a seed inside the basin; ink
    # moments proved too ill-conditioned to provide one. The ring
    # anchor breaks the frame's 180-degree symmetry (the frame centre
    # is offset from the ring centre), so a full 360-degree sweep is
    # decisive, not ambiguous.
    grid: list[tuple[float, np.ndarray]] = []
    for theta_deg in range(0, 360, 10):
        for factor in (0.86, 0.92, 0.98, 1.04, 1.10):
            trial = np.array(
                [
                    0.0,
                    0.0,
                    math.radians(theta_deg),
                    math.log(scale0 * factor),
                ]
            )
            trial_value, _, _ = objective(trial)
            grid.append((trial_value, trial))
    grid.sort(key=lambda g: -g[0])
    # Top seeds in DISTINCT rotation buckets: under strong
    # perspective the single best similarity cell can sit in a weak
    # basin while the true basin scores slightly lower.
    seeds: list[np.ndarray] = []
    for _, trial in grid:
        theta_deg = math.degrees(trial[2]) % 360
        if all(
            min(abs(theta_deg - s), 360 - abs(theta_deg - s)) >= 20
            for s in (math.degrees(p[2]) % 360 for p in seeds)
        ):
            seeds.append(trial)
        if len(seeds) >= 3:
            break

    frame_keys = ("TL", "TR", "BR", "BL")
    frame_src = np.array([frame[k] for k in frame_keys])

    def loose_score(H_try: np.ndarray) -> float:
        t, a, c, _ = _tick_matches(gray, mask, card, H_try, strict=False)
        return a + 0.02 * (len(t) + len(c))

    def refine_seed(params0: np.ndarray) -> list[np.ndarray]:
        """Similarity ascent + projective vertex search from a seed.

        Returns [H_similarity, H_projective]. A similarity cannot
        express perspective; the vertex search (8 dof, same loose
        objective) can, starting from the similarity's projection.
        """
        params = params0.copy()
        value, _, _ = objective(params)
        steps = np.array([8.0, 8.0, math.radians(1.5), 0.03])
        for _round in range(4):
            improved = True
            while improved:
                improved = False
                for i in range(4):
                    for sign in (1.0, -1.0):
                        trial = params.copy()
                        trial[i] += sign * steps[i]
                        trial_value, _, _ = objective(trial)
                        if trial_value > value + 1e-9:
                            params, value = trial, trial_value
                            improved = True
            steps /= 2
        dx, dy, theta, log_scale = params
        H_sim = _similarity(
            ring_mm, centre + np.array([dx, dy]), theta, math.exp(log_scale)
        )
        vertices = _project(H_sim, frame_src)
        best_val = loose_score(H_sim)
        for step in (16.0, 8.0, 4.0, 2.0):
            improved = True
            while improved:
                improved = False
                for vi in range(4):
                    for axis in range(2):
                        for sign in (1.0, -1.0):
                            trial_v = vertices.copy()
                            trial_v[vi, axis] += sign * step
                            H_try, _ = cv2.findHomography(
                                frame_src, trial_v, 0
                            )
                            if H_try is None:
                                continue
                            val = loose_score(H_try.astype(np.float64))
                            if val > best_val + 1e-9:
                                vertices, best_val = trial_v, val
                                improved = True
        H_proj, _ = cv2.findHomography(frame_src, vertices, 0)
        return [H_sim, H_proj.astype(np.float64)]

    # Every seed's transforms run the full fit pipeline below and
    # the better strict result wins — a search can land in a false
    # basin, and a wild fit must never be reported when a sane one
    # exists.
    seed_observations = []
    H_sheet = _sheet_seed(mask, card, centre)
    if H_sheet is not None:
        t, a, c, _ = _tick_matches(gray, mask, card, H_sheet, strict=False)
        if a >= 4 and len(t) >= 10:
            seed_observations.append((t, c))
    for seed in seeds:
        for H_seed in refine_seed(seed):
            t, a, c, _ = _tick_matches(
                gray, mask, card, H_seed, strict=False
            )
            if a >= 4 and len(t) >= 10:
                seed_observations.append((t, c))
    if not seed_observations:
        H_hough = _hough_quad_seed(gray, mask, card, centre, scale0)
        if H_hough is not None:
            t, a, c, _ = _tick_matches(
                gray, mask, card, H_hough, strict=False
            )
            if a >= 4 and len(t) >= 10:
                seed_observations.append((t, c))
    if not seed_observations:
        raise MeasureError(
            "Tick verification failed (card too degraded or occluded)."
        )


    # Homography fitting. Points on a rectangle's border leave a
    # homography's far field ill-conditioned, and the four corners
    # (line intersections, the most precise measurements available)
    # must not be outvoted by fifteen tick centroids: RANSAC first
    # for outlier rejection, then least squares with each corner
    # repeated as a weight. Re-observe and refit twice.
    def fit(tick_pairs, corner_pairs):
        pool = list(tick_pairs) + list(corner_pairs)
        src = np.array([m for m, _ in pool], dtype=np.float64)
        dst = np.array([p for _, p in pool], dtype=np.float64)
        H_r, inliers = cv2.findHomography(src, dst, cv2.RANSAC, 3.0)
        if H_r is None:
            return None
        if inliers is not None and int(inliers.sum()) >= 8:
            keep = inliers.reshape(-1).astype(bool)
            n_ticks = len(tick_pairs)
            tick_pairs = [
                p for p, k in zip(tick_pairs, keep[:n_ticks]) if k
            ]
            corner_pairs = [
                p for p, k in zip(corner_pairs, keep[n_ticks:]) if k
            ]
        weighted = list(tick_pairs) + list(corner_pairs) * 6
        src = np.array([m for m, _ in weighted], dtype=np.float64)
        dst = np.array([p for _, p in weighted], dtype=np.float64)
        H_l, _ = cv2.findHomography(src, dst, 0)
        return (H_l if H_l is not None else H_r).astype(np.float64)

    def pipeline(tick_pairs, corner_pairs):
        """Fit + strict refits; returns (H, tick_pairs, rms) or None.

        The span gate lives here: points must cover the frame, or
        any extent away from them is extrapolation, not measurement.
        """
        H = fit(tick_pairs, corner_pairs)
        if H is None:
            return None
        for _ in range(2):
            new_ticks, _, new_corners, _ = _tick_matches(
                gray, mask, card, H, strict=True, with_lines=True
            )
            if len(new_ticks) >= 10:
                tick_pairs, corner_pairs = new_ticks, new_corners
            H_new = fit(tick_pairs, corner_pairs)
            if H_new is None:
                break
            H = H_new
        observed = tick_pairs + corner_pairs
        src = np.array([m for m, _ in observed], dtype=np.float64)
        span = src.max(axis=0) - src.min(axis=0)
        frame_span = frame["BR"] - frame["TL"]
        if (
            span[0] < 0.6 * frame_span[0]
            or span[1] < 0.6 * frame_span[1]
            or len(tick_pairs) < 12
        ):
            return None
        tick_src = np.array([m for m, _ in tick_pairs], dtype=np.float64)
        tick_dst = np.array([p for _, p in tick_pairs], dtype=np.float64)
        projected = _project(H, tick_src)
        rms = float(
            np.sqrt(np.mean(np.linalg.norm(projected - tick_dst, axis=1) ** 2))
        )
        return H, tick_pairs, rms

    results = []
    for t0, c0 in seed_observations:
        result = pipeline(t0, c0)
        if result is not None and result[2] <= 4.0:
            results.append(result)
    if not results:
        raise MeasureError(
            "Card fit did not converge to a trustworthy homography "
            "(coverage or residual gates failed)."
        )

    results.sort(key=lambda r: (-len(r[1]), r[2]))
    H, tick_pairs, rms = results[0]
    # NOTE: the M2 card-verification gate deliberately does NOT run
    # here. estimate() applies it after the size/envelope gates, so a
    # too-small card refuses with its actionable size reason instead
    # of a verification reason (the subfloor battery pins that
    # message). _detect_card stays a pure detector.
    quad = {
        k: _project(H, v[None])[0] for k, v in frame.items()
    }
    return H, tick_pairs, quad, rms


# --------------------------------------------------------------------------
# Object extent


def _extent_from_points_mm(points_mm: np.ndarray) -> tuple[float, float, np.ndarray]:
    lo = points_mm.min(axis=0)
    hi = points_mm.max(axis=0)
    return float(hi[0] - lo[0]), float(hi[1] - lo[1]), (lo + hi) / 2


def _object_extent(
    image_bgr: np.ndarray,
    H: np.ndarray,
    quad_img: dict[str, np.ndarray],
    bbox: tuple[float, float, float, float] | None,
    mask_obj: np.ndarray | None,
    warnings: list[str],
) -> tuple[float, float, np.ndarray]:
    H_inv = np.linalg.inv(H)
    if mask_obj is not None:
        ys, xs = np.nonzero(mask_obj)
        if len(xs) < 10:
            raise MeasureError("Supplied mask is empty.")
        pts = _project(H_inv, np.stack([xs, ys], axis=1).astype(np.float64))
        return _extent_from_points_mm(pts)
    if bbox is not None:
        x, y, w, h = bbox
        corners = np.array(
            [[x, y], [x + w, y], [x + w, y + h], [x, y + h]], dtype=np.float64
        )
        warnings.append(
            "Extent from a caller-supplied image bbox: a bbox around a "
            "rotated object overestimates; a mask is tighter."
        )
        return _extent_from_points_mm(_project(H_inv, corners))

    # Prototype fallback: colour segmentation. The object is the largest
    # saturated, non-card contour outside the card quad.
    hsv = cv2.cvtColor(image_bgr, cv2.COLOR_BGR2HSV)
    candidate = ((hsv[:, :, 1] > 70) & (hsv[:, :, 2] > 40)).astype(np.uint8) * 255
    quad = np.array(
        [quad_img[k] for k in ("TL", "TR", "BR", "BL")], dtype=np.int32
    )
    cv2.fillPoly(candidate, [quad], 0)
    candidate = cv2.morphologyEx(
        candidate, cv2.MORPH_OPEN, np.ones((5, 5), np.uint8)
    )
    candidate = cv2.morphologyEx(
        candidate, cv2.MORPH_CLOSE, np.ones((9, 9), np.uint8)
    )
    contours, _ = cv2.findContours(
        candidate, cv2.RETR_EXTERNAL, cv2.CHAIN_APPROX_NONE
    )
    if not contours:
        raise MeasureError(
            "Auto extent found no object (colour segmentation); "
            "supply a bbox or mask."
        )
    largest = max(contours, key=cv2.contourArea)
    if cv2.contourArea(largest) < 0.0005 * image_bgr.shape[0] * image_bgr.shape[1]:
        raise MeasureError("Auto extent contour is implausibly small.")
    # Frame-edge guard (false-accept hardening, 2026-11-02): a region
    # that runs off the frame is not an object extent — on IMG_1403 the
    # segmentation picked the sky/rooftop band along the top edge and
    # reported it as a ~1.9 x 2.7 m object. Refuse with the correction
    # path instead of measuring the scenery. (Mask and bbox callers
    # return above and are unaffected.)
    edge_pts = largest.reshape(-1, 2)
    img_h, img_w = image_bgr.shape[0], image_bgr.shape[1]
    if (
        int(edge_pts[:, 0].min()) <= 1
        or int(edge_pts[:, 1].min()) <= 1
        or int(edge_pts[:, 0].max()) >= img_w - 2
        or int(edge_pts[:, 1].max()) >= img_h - 2
    ):
        raise MeasureError(
            "Auto extent ran off the edge of the frame (the largest "
            "colour region touches the border, so it is scenery, not "
            "the object) — supply a bbox or mask, or retake with the "
            "whole object in frame."
        )
    warnings.append(
        "Extent by colour segmentation (prototype fallback): the object "
        "must be colour-distinct from the surface and the card."
    )
    pts = largest.reshape(-1, 2).astype(np.float64)
    return _extent_from_points_mm(_project(H_inv, pts))


def _card_width_px(H: np.ndarray, card: dict) -> float:
    """The card sheet's width in image pixels under H.

    Mean of the top and bottom edge lengths of the projected sheet
    corners — the same quantity the card-size battery records as its
    independent variable, so the gate thresholds and the battery's
    pixel counts are directly comparable.
    """
    w = float(card["card_width_mm"])
    h = float(card["card_height_mm"])
    corners = _project(
        H, np.array([[0, 0], [w, 0], [w, h], [0, h]], dtype=np.float64)
    )
    top = float(np.linalg.norm(corners[1] - corners[0]))
    bottom = float(np.linalg.norm(corners[2] - corners[3]))
    return (top + bottom) / 2


# --------------------------------------------------------------------------
# Public API


def _load_image(source: str) -> np.ndarray:
    path = source
    if source.startswith("asset://"):
        from .assets import AssetStore
        from .config import get_data_dir

        store = AssetStore(get_data_dir())
        path = str(store.resolve(source))
    img = cv2.imread(path, cv2.IMREAD_COLOR)
    if img is None:
        # HEIC and friends: fall back to Pillow (pillow-heif registers
        # itself in ingest; here a plain decode attempt is enough).
        try:
            from PIL import Image

            rgb = np.array(Image.open(path).convert("RGB"))
            img = cv2.cvtColor(rgb, cv2.COLOR_RGB2BGR)
        except Exception as exc:
            raise MeasureError(f"Could not decode image: {path}") from exc
    return img


def estimate(
    source: str,
    card_id: str = "AH-46",
    profile_path: Path = DEFAULT_PROFILE_PATH,
    bbox: tuple[float, float, float, float] | None = None,
    mask: np.ndarray | None = None,
) -> dict:
    """Estimate a flat object's size from a photo with the card in plane.

    Returns centre estimates (width_mm, height_mm), a single range_pct
    covering both dimensions, per-dimension ranges, a tilde display
    string, the method string, the detected card's width in image
    pixels (card_width_px), and warnings. The display string is the
    only user-facing form: it always carries the range. A card that
    spans too few pixels in frame raises MeasureError instead — see
    MIN_CARD_PX, and CARD_ENVELOPE_MIN_PX for cards (the AH-35 mini)
    whose validated envelope is narrower than the universal floor.
    """
    card = load_card_profile(profile_path, card_id)
    image = _load_image(source)
    gray = cv2.cvtColor(image, cv2.COLOR_BGR2GRAY)

    warnings: list[str] = ["Prototype: flat object in the same plane as the card only."]
    try:
        H, matched, quad_img, rms_px = _detect_card(gray, image, card)
    except MeasureError as first_err:
        # Ring-shaped rescue: if the centroid-anchored search found
        # nothing trustworthy, retry anchored on a ring-shaped cyan
        # blob. If that also fails, the original refusal stands, so
        # pinned refusal messages never change behind a rescue.
        try:
            H, matched, quad_img, rms_px = _detect_card(
                gray, image, card, _use_shape=True
            )
        except MeasureError:
            raise first_err from None
    n_ticks = len(matched)

    # Pixels-across gate: refuse the silent lying band outright, and
    # carry a systematic widening term through the caution band (see
    # the constants at the top of this module). The fit statistics
    # cannot see this failure mode — the RMS stays ~0.25 px while
    # the centre walks off — so the gate keys on card size alone.
    card_px = _card_width_px(H, card)
    envelope_min = CARD_ENVELOPE_MIN_PX.get(card_id)
    if envelope_min is not None and card_px < envelope_min:
        raise MeasureError(
            f"Card too small in frame for the {card_id} mini "
            f"(~{card_px:.0f} px across; this card is only validated "
            f"for close-ups at ~{envelope_min:.0f} px across or more, "
            "where its ranges demonstrably cover the true size) — "
            "Retake the shot or use a bigger card."
        )
    if card_px < MIN_CARD_PX:
        raise MeasureError(
            f"Card too small in frame (~{card_px:.0f} px across; the "
            f"fit stays quiet but the range stops covering the true "
            f"size below ~{MIN_CARD_PX:.0f} px) — Retake the shot "
            "or use a bigger card."
        )

    # M2 card verification (ported from the Rust measurefix,
    # 2026-11-02): the winning fit must show the card's own,
    # non-periodic features. Runs after the size gates on purpose,
    # so a too-small card keeps its actionable size reason. The
    # IMG_1491 fit that used to "accept" (15 ticks, 704 px) dies
    # here: its ring coverage is 0.021, a denim-seam phantom, not
    # the card.
    _verify_card_fit(image, H, card, n_ticks)

    # Strict re-verification of the winning fit (false-accept
    # hardening, 2026-11-02 — a second witness, placed after the
    # size gates on purpose). The pipeline's span/count/RMS gates are
    # computed on the pairs the refit KEPT, and the refit only
    # replaces its pairs when a strict re-observation finds >= 10
    # ticks; a phantom fit therefore keeps its non-strict evidence
    # and certifies itself, and its fitted width (IMG_1403: 135 px)
    # clears the pixel floor above. A strict recheck at the same H is
    # the independent witness: the IMG_1403 phantom (fence boards)
    # collapses to 0 ticks / 0 arms, while real fits hold or grow
    # (battery scenes 12-21 -> 15-21). The IMG_1491 fit (15 ticks)
    # is NOT such a witness: it is a denim-seam phantom, killed by
    # the M2 ring gate above, not by this recheck.
    # Degraded small cards can collapse too (AH-35 mini, blurred at
    # 133 px: 0 / 0) — which is why this check runs AFTER the size
    # and envelope gates: those refuse such cards first, with the
    # actionable reason, and this gate only speaks for fits big
    # enough to otherwise be believed.
    s_ticks, s_arms, _, _ = _tick_matches(
        gray, _ink_mask(gray), card, H, strict=True, with_lines=True
    )
    if len(s_ticks) <= 2 and s_arms <= 1:
        raise MeasureError(
            "Card fit failed strict tick verification (the fitted "
            "card's ticks vanish under strict matching, so the fit "
            "is not on the card) — Retake the shot with the whole "
            "card in frame, closer and in open shade."
        )
    sys_frac = 0.0
    if card_px < COMFORT_CARD_PX:
        band_t = (COMFORT_CARD_PX - card_px) / (COMFORT_CARD_PX - MIN_CARD_PX)
        sys_frac = _SMALL_CARD_SYS_MAX * band_t * band_t
        warnings.append(
            f"Card is small in frame (~{card_px:.0f} px across): the "
            "range is widened for small-card systematic error — a "
            "bigger card or a closer shot would tighten it."
        )
    if n_ticks < 24:
        warnings.append(
            f"Only {n_ticks}/24 tick cuts matched; the range reflects "
            "the fit residual, but a cleaner photo would tighten it."
        )

    width_mm, height_mm, centre_mm = _object_extent(
        image, H, quad_img, bbox, mask, warnings
    )
    if width_mm <= 0 or height_mm <= 0:
        raise MeasureError("Degenerate object extent.")

    # Propagate the detection residual into a range at the object.
    centre_img = _project(H, centre_mm[None])[0]
    eps = 1.0  # mm
    jx = (_project(H, (centre_mm + [eps, 0])[None])[0] - centre_img) / eps
    jy = (_project(H, (centre_mm + [0, eps])[None])[0] - centre_img) / eps
    px_per_mm = float((np.linalg.norm(jx) + np.linalg.norm(jy)) / 2)
    if px_per_mm <= 0:
        raise MeasureError("Degenerate homography at the object location.")
    sigma_mm = rms_px / px_per_mm
    frame = _frame_mm(card)
    frame_centroid = np.mean(list(frame.values()), axis=0)
    frame_diag = float(np.linalg.norm(frame["BR"] - frame["TL"]))
    lever = float(np.linalg.norm(centre_mm - frame_centroid)) / frame_diag

    def half_range(dim: float) -> float:
        # Projective error growth outside the fiducial hull is
        # quadratic in the normalized distance (the inverse map's
        # Jacobian grows with (1 + lever)^2) — verified against the
        # synthetic scenes: ~1px at the frame became ~7px at an
        # object 1.5 frame-diagonals away. A linear lever term
        # understates the range and the "contained" tests catch it.
        u = sigma_mm * (1.0 + lever) ** 2
        # sys_frac is a bias, not noise, so it adds linearly rather
        # than in quadrature; it is zero once the card is comfortably
        # large in frame.
        return 2.0 * u + 0.01 * dim + sys_frac * dim

    err_w, err_h = half_range(width_mm), half_range(height_mm)
    range_pct = max(err_w / width_mm, err_h / height_mm) * 100.0
    range_pct = float(np.ceil(range_pct * 2) / 2)  # round up to 0.5

    def fmt(dim: float, err: float) -> str:
        lo, hi = dim - err, dim + err
        if dim >= 100:
            return f"~{dim / 10:.1f} cm ({lo / 10:.1f}–{hi / 10:.1f} cm)"
        return f"~{dim:.0f} mm ({lo:.0f}–{hi:.0f} mm)"

    # Print-scale assumption (mirrors Rust b882176, 2026-11-02): the
    # profile describes a 100% print. Detection does not depend on it;
    # only the sizes do, so every result states it plus the ruler
    # check. No size ships until the bracket is measured or the card
    # is reprinted at 100%.
    frame = _frame_mm(card)
    stroke = float(card["bracket_frame"]["stroke_width_mm"])

    def _r1(v: float) -> float:
        # Round to 1 decimal, .x5 up (Rust's epsilon trick): 135.85
        # must print as 135.9, not banker's 135.8.
        return math.floor(v * 10.0 + 1e-6 + 0.5) / 10.0

    across = abs(float(frame["TR"][0] - frame["TL"][0])) + stroke
    down = abs(float(frame["BL"][1] - frame["TL"][1])) + stroke
    bracket = [_r1(max(across, down)), _r1(min(across, down))]
    card_print_check = (
        f"Sizes assume the {card_id} card was printed at 100%: its outer bracket "
        f"frame (the black corner marks) measures {bracket[0]:.1f} mm along its long side and "
        f"{bracket[1]:.1f} mm along its short side. Check the long side with a ruler; if it "
        f"measures M mm instead, the card was printed at M / {bracket[0]:.1f} and every size "
        f"here scales by that factor."
    )

    return {
        "width_mm": round(width_mm, 1),
        "height_mm": round(height_mm, 1),
        "range_pct": range_pct,
        "width_range_mm": [round(width_mm - err_w, 1), round(width_mm + err_w, 1)],
        "height_range_mm": [round(height_mm - err_h, 1), round(height_mm + err_h, 1)],
        "display": f"{fmt(width_mm, err_w)} wide × {fmt(height_mm, err_h)} high",
        "method": METHOD,
        "card_id": card_id,
        "ticks_matched": n_ticks,
        "card_width_px": round(card_px),
        "reprojection_rms_px": round(rms_px, 2),
        "card_print_scale_assumed": 1.0,
        "card_outer_bracket_mm": bracket,
        "card_print_check": card_print_check,
        "warnings": warnings,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="agenthand.measure",
        description="Prototype: estimate a flat object's size from a photo "
        "with an agenthand card in the same plane.",
    )
    parser.add_argument("image", help="Photo path or asset:// handle")
    parser.add_argument(
        "card", help="Card id from the layout profile (e.g. AH-46, AH-57)"
    )
    parser.add_argument(
        "--profile",
        type=Path,
        default=DEFAULT_PROFILE_PATH,
        help="Layout profile JSON (default: the scale-card print profile)",
    )
    parser.add_argument(
        "--bbox",
        type=str,
        default=None,
        help="Object bbox in image pixels, x,y,w,h (default: auto-detect)",
    )
    args = parser.parse_args(argv)
    bbox = None
    if args.bbox:
        parts = tuple(float(v) for v in args.bbox.split(","))
        if len(parts) != 4:
            parser.error("--bbox must be x,y,w,h")
        bbox = parts  # type: ignore[assignment]
    try:
        result = estimate(
            args.image, card_id=args.card, profile_path=args.profile, bbox=bbox
        )
    except MeasureError as exc:
        print(json.dumps({"error": str(exc)}), file=sys.stderr)
        return 1
    print(json.dumps(result, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
