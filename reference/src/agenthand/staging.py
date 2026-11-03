"""Source-derived staging geometry and the angle lock (mirror of Rust staging.rs + the plate prompt builders of meta.rs).

Two halves:

- Subject geometry: ``analyze_subject`` measures a foreground's
  opaque coverage, bbox, frame-edge contact and thin structure —
  the facts the plate steps (and the shot-angle estimator) read.

- Angle lock (ANGLE-LOCK-SPEC, decision 123): every source_edit
  plate prompt ends with the base clause "Keep the exact same camera
  angle and perspective as the original photo, same horizon position,
  same framing." plus, when the mask geometry supports one, a stated
  angle ("The original photo was <phrase>."). Mask geometry alone is
  a weak cue for camera pitch, so the estimator's thresholds are
  conservative: a specific angle is stated only when every cue
  agrees, and eye level is the unstated default. The draw result
  carries ``angle_lock {bucket, clause, stated, reason, signals}``;
  the record never enters the fidelity receipt.

Python's live draw builds its plate from text (the StageSpec
template in meta_client.py), so on the live path the angle appears
as the generated-mode steering words (``generated_angle_steer``),
exactly as the Rust build records it for its legacy ``generated``
mode. The source_edit prompt builders below are the reference port
of the Rust prompt code and are pinned by tests; Python's draw does
not call an image-edit endpoint.
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum

import numpy as np
from PIL import Image

# ---------------------------------------------------------------------------
# Subject geometry
# ---------------------------------------------------------------------------

#: A subject covering more than this fraction of the frame, or
#: spanning it (touching opposite edges: a lawn, a fence run), is a
#: surface / frame-filling subject.
FILLS_FRAME_COVERAGE = 0.45
#: A run of product pixels (row or column) shorter than this is thin.
THIN_RUN_PX = 6
#: The subject has thin structure when at least this fraction of its
#: product pixels sit in a thin run.
THIN_FRACTION = 0.01
#: Edge-matte width, and the cap for thin subjects.
MATTE_PX = 2
MATTE_PX_THIN = 1

#: The honest note for surface / frame-filling subjects.
SURFACE_NOTE = "subject is a surface or fills the frame; staging kept its pixels and replaced only what was around it"


@dataclass
class SubjectGeometry:
    """What the plate steps need to know about the subject."""

    width: int
    height: int
    #: Fully opaque product pixels (the receipt's product pixels).
    opaque_pixels: int
    coverage: float
    #: Half-open bbox of the opaque pixels.
    bbox: tuple[int, int, int, int] | None
    touches: list[str]
    fills_frame: bool
    thin_fraction: float
    thin: bool
    shadow: bool
    matte_px: int

    def to_json(self) -> dict:
        return {
            "opaque_pixels": self.opaque_pixels,
            "coverage": round(self.coverage, 6),
            "bbox": list(self.bbox) if self.bbox is not None else None,
            "touches_frame_edges": list(self.touches),
            "fills_frame": self.fills_frame,
            "thin_fraction": round(self.thin_fraction, 6),
            "thin_structure": self.thin,
        }


def _axis_run_lengths(binary: np.ndarray, axis: int) -> np.ndarray:
    """Per-pixel length of the run of True values each pixel belongs
    to, along ``axis``. Fully vectorised: a pixel's run length is the
    distance between the False neighbours bracketing it."""
    b = np.moveaxis(binary, axis, -1)
    n = b.shape[-1]
    idx = np.arange(n)
    false_pos = np.where(b, -1, idx)
    prev_false = np.maximum.accumulate(false_pos, axis=-1)
    next_false = np.minimum.accumulate(
        np.where(b, n, idx)[:, ::-1], axis=-1
    )[:, ::-1]
    lengths = next_false - prev_false - 1
    out = np.where(b, lengths, 0)
    return np.moveaxis(out, -1, axis)


def _thin_fraction(
    alpha: np.ndarray, bbox: tuple[int, int, int, int], opaque: int
) -> float:
    """Fraction of opaque pixels lying in a row or column run of
    opaque pixels shorter than THIN_RUN_PX (min(h-run, v-run) < 6)."""
    if opaque == 0:
        return 0.0
    x0, y0, x1, y1 = bbox
    on = alpha[y0:y1, x0:x1] == 255
    h_len = _axis_run_lengths(on, axis=1)
    v_len = _axis_run_lengths(on, axis=0)
    thin = on & ((h_len < THIN_RUN_PX) | (v_len < THIN_RUN_PX))
    return float(np.count_nonzero(thin)) / opaque


def analyze_subject(foreground: Image.Image) -> SubjectGeometry:
    """Measure the subject from the foreground's alpha."""
    fg = foreground.convert("RGBA")
    width, height = fg.size
    alpha = np.asarray(fg.getchannel("A"))
    opaque = int(np.count_nonzero(alpha == 255))
    frame = max(1, width * height)
    coverage = opaque / frame
    bbox: tuple[int, int, int, int] | None = None
    if opaque:
        ys, xs = np.nonzero(alpha == 255)
        bbox = (int(xs.min()), int(ys.min()), int(xs.max()) + 1, int(ys.max()) + 1)
    touches: list[str] = []
    if bbox is not None:
        x0, y0, x1, y1 = bbox
        for hit, edge in (
            (x0 == 0, "left"),
            (y0 == 0, "top"),
            (x1 == width, "right"),
            (y1 == height, "bottom"),
        ):
            if hit:
                touches.append(edge)
    thin_fraction = (
        _thin_fraction(alpha, bbox, opaque) if bbox is not None else 0.0
    )
    thin = thin_fraction >= THIN_FRACTION
    spans = ("left" in touches and "right" in touches) or (
        "top" in touches and "bottom" in touches
    )
    fills_frame = coverage > FILLS_FRAME_COVERAGE or spans
    return SubjectGeometry(
        width=width,
        height=height,
        opaque_pixels=opaque,
        coverage=coverage,
        bbox=bbox,
        touches=touches,
        fills_frame=fills_frame,
        thin_fraction=thin_fraction,
        thin=thin,
        # A contact shadow under a surface (a lawn, a fence run), or
        # under something the frame cuts at the bottom, would be
        # invented ground.
        shadow=opaque > 0 and not fills_frame and "bottom" not in touches,
        matte_px=MATTE_PX_THIN if thin else MATTE_PX,
    )


def check_stageable(geometry: SubjectGeometry) -> str | None:
    """Refuse to stage a foreground with no product pixels: a plate with
    nothing on it is not staging, and a receipt over 0 pixels vouches
    for nothing. Returns the refusal text, or None when stageable."""
    if geometry.opaque_pixels == 0:
        return (
            "The foreground has no fully opaque product pixels (an empty "
            "or fully soft mask), so there is nothing to stage and nothing "
            "a receipt could vouch for. Run cut_out again with a more "
            "specific phrase, or retake the photo closer to the subject."
        )
    return None


# ---------------------------------------------------------------------------
# Shot-angle estimate (angle lock)
# ---------------------------------------------------------------------------

#: SteepDown: the ground-contact line (bbox bottom) sits at least this
#: fraction of the frame above the bottom edge ("ground line high": a
#: clear band of ground in front of the subject).
ANGLE_STEEP_MIN_GROUND_BELOW = 0.15
#: SteepDown: the subject's top edge reaches at least this high (y/h).
ANGLE_STEEP_MAX_TOP = 0.33
#: SteepDown: footprint wide (bbox width / height).
ANGLE_STEEP_MIN_ASPECT = 1.25
#: SteepDown: a substantial subject (bbox height / frame height).
ANGLE_STEEP_MIN_HEIGHT = 0.25
#: LowUp: the ground-contact line sits in the bottom 12% of the frame.
ANGLE_LOW_MIN_CONTACT = 0.88
#: LowUp: the bbox centre sits above the frame midline (y/h).
ANGLE_LOW_MAX_CENTRE = 0.46
#: LowUp: towering (bbox width / height).
ANGLE_LOW_MAX_ASPECT = 0.85


class ShotAngle(Enum):
    """The three shot-angle buckets."""

    STEEP_DOWN = "steep_down"
    EYE_LEVEL = "eye_level"
    LOW_UP = "low_up"

    @property
    def bucket(self) -> str:
        return self.value


@dataclass
class AngleSignals:
    """The normalised mask geometry the estimate reads (y grows downward)."""

    #: bbox top edge / frame height.
    top_y: float
    #: bbox centre / frame height (0.5 is the midline).
    centre_y: float
    #: Ground-contact line (bbox bottom) / frame height.
    contact_y: float
    #: Frame below the contact line (1 - contact_y).
    ground_below: float
    #: bbox width / bbox height, in pixels.
    aspect: float
    #: bbox height / frame height.
    height_frac: float
    #: Opaque product pixels / frame pixels.
    coverage: float

    def to_json(self) -> dict:
        return {
            "top_y": round(self.top_y, 6),
            "centre_y": round(self.centre_y, 6),
            "contact_y": round(self.contact_y, 6),
            "ground_below": round(self.ground_below, 6),
            "aspect": round(self.aspect, 6),
            "height_frac": round(self.height_frac, 6),
            "coverage": round(self.coverage, 6),
        }


@dataclass
class AngleEstimate:
    """The estimate and why. ``stated`` is true only when the geometry
    supports a specific angle (SteepDown or LowUp); the EyeLevel
    default is never stated in a source_edit prompt."""

    angle: ShotAngle
    stated: bool
    signals: AngleSignals | None
    reason: str

    def signals_json(self) -> dict | None:
        return None if self.signals is None else self.signals.to_json()


def estimate_shot_angle(geometry: SubjectGeometry) -> AngleEstimate:
    """Estimate the shot angle from the subject geometry (pure; no pixels)."""
    return estimate_shot_angle_from(
        geometry.width,
        geometry.height,
        geometry.bbox,
        geometry.coverage,
        geometry.fills_frame,
        geometry.touches,
    )


def estimate_shot_angle_from(
    width: int,
    height: int,
    bbox: tuple[int, int, int, int] | None,
    coverage: float,
    fills_frame: bool,
    touches: list[str],
) -> AngleEstimate:
    """The estimator over primitives (bbox half-open, in frame pixels).
    Rules, all cues required:
    - SteepDown: contact line at least 15% above the bottom edge, top
      edge in the upper third, bbox at least 1.25x wider than tall and
      at least 25% of the frame tall.
    - LowUp: contact line in the bottom 12%, bbox centre above 0.46,
      bbox at most 0.85x as wide as tall.
    - Otherwise EyeLevel, unstated. Also unstated whenever there is no
      bbox, the subject fills the frame (a surface), or the subject is
      cut by the bottom edge (the ground-contact line is not visible)."""

    def default(signals: AngleSignals | None, reason: str) -> AngleEstimate:
        return AngleEstimate(ShotAngle.EYE_LEVEL, False, signals, reason)

    if bbox is None:
        return default(None, "no product bbox; angle not estimated")
    x0, y0, x1, y1 = bbox
    if width == 0 or height == 0 or x1 <= x0 or y1 <= y0:
        return default(None, "degenerate bbox; angle not estimated")
    h = float(height)
    signals = AngleSignals(
        top_y=y0 / h,
        centre_y=(y0 + y1) / 2.0 / h,
        contact_y=y1 / h,
        ground_below=(h - y1) / h,
        aspect=(x1 - x0) / (y1 - y0),
        height_frac=(y1 - y0) / h,
        coverage=coverage,
    )
    if fills_frame:
        return default(
            signals,
            "subject fills the frame (a surface); no angle claimed beyond the base clause",
        )
    if "bottom" in touches:
        return default(
            signals,
            "subject is cut by the bottom edge, so the ground-contact line is not visible; no angle claimed beyond the base clause",
        )
    s = signals
    if (
        s.ground_below >= ANGLE_STEEP_MIN_GROUND_BELOW
        and s.top_y <= ANGLE_STEEP_MAX_TOP
        and s.aspect >= ANGLE_STEEP_MIN_ASPECT
        and s.height_frac >= ANGLE_STEEP_MIN_HEIGHT
    ):
        return AngleEstimate(
            ShotAngle.STEEP_DOWN,
            True,
            s,
            "wide subject resting on a clear band of ground and reaching into the upper third: camera tilted down",
        )
    if (
        s.contact_y >= ANGLE_LOW_MIN_CONTACT
        and s.centre_y <= ANGLE_LOW_MAX_CENTRE
        and s.aspect <= ANGLE_LOW_MAX_ASPECT
    ):
        return AngleEstimate(
            ShotAngle.LOW_UP,
            True,
            s,
            "tall subject rising above the midline from a thin strip of ground at the bottom: camera low, looking up",
        )
    return default(
        s,
        "geometry inconclusive; eye level assumed, no angle claimed beyond the base clause",
    )


# ---------------------------------------------------------------------------
# Plate prompts (mirror of Rust meta.rs: the Mac probe's wording plus
# the angle lock)
# ---------------------------------------------------------------------------

PLATE_EDIT_OBJECT = "Edit @Image1. It is a real photo in which one region has been filled flat grey. Fill the grey region with the continuous background that would be behind it: the same ground, floor, wall and surfaces, continuing their lines, textures and lighting. Keep the camera angle, framing, ground plane, horizon line and light direction exactly as in @Image1; do not move, zoom, rotate or re-frame anything. Make the scene clean, uncluttered and listing-ready: tidy, evenly lit, nothing new added. Do not place any object in the grey region. No people, animals, text, logos or watermark."

PLATE_EDIT_SURFACE = "Edit @Image1. It is a real photo in which the main surface has been filled flat grey. Continue the surroundings: keep everything outside the grey region where it is, in the same camera angle, framing, ground plane, horizon line and light direction, and make it clean, uncluttered and listing-ready. Fill the grey region with a plain, even continuation of what borders it. Do not move, zoom, rotate or re-frame anything. No people, animals, text, logos or watermark."

#: Styling for an object plate when the caller gives no scene text
#: (decision 119: the tidy default).
PLATE_EDIT_DEFAULT_STYLE = "a clean, tidy, bright version of the same place: surfaces fresh and uncluttered, no moss or debris, soft even daylight, same camera height and same ground plane, listing photo quality"

#: The base angle-lock clause: the owner's exact text, appended to
#: every source_edit plate prompt.
ANGLE_LOCK_CLAUSE = "Keep the exact same camera angle and perspective as the original photo, same horizon position, same framing."

#: Shot-angle phrases, one per ShotAngle bucket.
ANGLE_PHRASE_STEEP_DOWN = "shot from standing height looking down at the surface at a steep angle"
ANGLE_PHRASE_EYE_LEVEL = "eye-level shot"
ANGLE_PHRASE_LOW_UP = "shot from a low angle looking slightly up"

_PHRASES = {
    ShotAngle.STEEP_DOWN: ANGLE_PHRASE_STEEP_DOWN,
    ShotAngle.EYE_LEVEL: ANGLE_PHRASE_EYE_LEVEL,
    ShotAngle.LOW_UP: ANGLE_PHRASE_LOW_UP,
}


def angle_phrase(angle: ShotAngle) -> str:
    return _PHRASES[angle]


def angle_lock_clause(estimate: AngleEstimate | None) -> str:
    """The angle-lock text a source_edit prompt ends with: the base clause,
    plus "The original photo was <phrase>." only when the geometry
    states an angle. The unstated eye-level default adds nothing: an
    "eye-level" claim on a downward photo the estimator missed would
    pull the edit away from the photo it must match."""
    if estimate is not None and estimate.stated:
        return f"{ANGLE_LOCK_CLAUSE} The original photo was {angle_phrase(estimate.angle)}."
    return ANGLE_LOCK_CLAUSE


def generated_angle_steer(estimate: AngleEstimate) -> str:
    """Legacy ``generated`` mode (no reference photo): the angle lock becomes
    steering words from the estimated angle, the eye-level default
    included, since a text-only plate must be told some angle. Weaker by
    nature: the model never sees the photo."""
    return (
        f"Camera angle and perspective: {angle_phrase(estimate.angle)}, "
        "with the horizon and ground plane placed to match a product "
        "photographed that way."
    )


def _sentence_end(text: str) -> str:
    """"." when ``text`` does not already end a sentence, so an appended
    clause never runs on from caller text."""
    return "" if text.endswith((".", "!", "?")) else "."


def generated_plate_scene(scene_prompt: str, estimate: AngleEstimate) -> str:
    """The scene text for a legacy generated plate: the caller's scene,
    then the angle steering words."""
    scene = scene_prompt.strip()
    steer = generated_angle_steer(estimate)
    if not scene:
        return steer
    return f"{scene}{_sentence_end(scene)} {steer}"


def plate_edit_prompt(scene_prompt: str, fills_frame: bool) -> str:
    """The geometry-aware edit prompt: ``plate_edit_prompt_locked``
    with no angle estimate (base clause only)."""
    return plate_edit_prompt_locked(scene_prompt, fills_frame, None)


def plate_edit_prompt_locked(
    scene_prompt: str,
    fills_frame: bool,
    estimate: AngleEstimate | None,
) -> str:
    """The source_edit plate prompt. ``fills_frame`` switches to the
    "continue the surroundings" wording for surfaces and subjects cut
    by the frame. The caller's scene text rides as styling only: the
    geometry is the photo's. The base angle-lock clause is always
    last, after any scene text, so a scene change cannot pull the
    camera."""
    base = PLATE_EDIT_SURFACE if fills_frame else PLATE_EDIT_OBJECT
    lock = angle_lock_clause(estimate)
    scene = scene_prompt.strip()
    # No scene text on an object plate: the tidy default styling.
    styling = PLATE_EDIT_DEFAULT_STYLE if (not scene and not fills_frame) else scene
    if not styling:
        return f"{base} {lock}"
    return f"{base} Styling only (keep the geometry): {styling}{_sentence_end(styling)} {lock}"


def angle_lock_record(estimate: AngleEstimate, clause: str) -> dict:
    """The draw result's ``angle_lock`` record: the estimated bucket, the
    exact clause appended to the plate prompt, whether a specific angle
    was stated, and the geometry it was read from."""
    return {
        "bucket": estimate.angle.bucket,
        "clause": clause,
        "stated": estimate.stated,
        "reason": estimate.reason,
        "signals": estimate.signals_json(),
    }
