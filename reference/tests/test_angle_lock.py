"""Angle lock (mirror of the Rust tests/angle_lock.rs): the shot-angle
estimator, the clause builders, the locked plate prompt, the generated
steering sentence, and the receipt rules. $0: pure functions and one
synthetic mask through staging.analyze_subject.
"""

from PIL import Image

from agenthand import staging
from agenthand.staging import ShotAngle

BASE = staging.ANGLE_LOCK_CLAUSE
STEEP = "The original photo was shot from standing height looking down at the surface at a steep angle."
LOW = "The original photo was shot from a low angle looking slightly up."


def est(frame_w, frame_h, bbox, coverage, fills_frame, touches):
    return staging.estimate_shot_angle_from(
        frame_w, frame_h, bbox, coverage, fills_frame, touches
    )


# ------------------------------------------------------------ estimator


def test_car_is_steep_down_and_stated():
    # The P16 car front frame (1920x1080, the run's own bbox): the car
    # rests low in the frame with ground below and its roof at the top
    # edge: the camera looked down at it.
    e = est(1920, 1080, (271, 0, 1419, 824), 0.309081, False, ["top"])
    assert e.angle is ShotAngle.STEEP_DOWN
    assert e.stated is True
    assert staging.angle_lock_clause(e) == f"{BASE} {STEEP}"
    assert e.signals.ground_below > 0.15


def test_jacket_flat_lay_is_steep_down_and_stated():
    # The P18 jacket reshoot (IMG_1491) cut bbox: wide, resting on the
    # floor below, reaching into the upper fifth.
    e = est(4284, 5712, (21, 1439, 4241, 4209), 0.240141, False, [])
    assert e.angle is ShotAngle.STEEP_DOWN
    assert e.stated is True


def test_bike_is_eye_level_and_not_stated():
    # The hero bike (IMG_1443) cut bbox: tall subject nearly filling
    # the frame height; no ground band below to measure.
    e = est(5712, 4284, (330, 419, 5176, 3890), 0.249170, False, [])
    assert e.angle is ShotAngle.EYE_LEVEL
    assert e.stated is False
    assert e.signals.contact_y == 3890 / 4284


def test_steep_down_needs_its_ground_band():
    # Same wide subject, but the ground band below shrinks under the
    # 0.15 bar: not stated as steep.
    e = est(1000, 1000, (50, 150, 990, 880), 0.30, False, [])
    assert e.signals.ground_below < 0.15
    assert e.angle is ShotAngle.EYE_LEVEL
    assert e.stated is False


def test_steep_down_needs_the_subject_high_in_frame():
    # Wide subject low in the frame (top below the upper third):
    # reads as eye level, not a look-down.
    e = est(1000, 1000, (50, 340, 1000, 590), 0.30, False, [])
    assert e.signals.top_y > 0.33
    assert e.angle is ShotAngle.EYE_LEVEL
    assert e.stated is False


def test_steep_down_needs_a_wide_subject():
    # Narrow subject, however placed: no steep claim.
    e = est(1000, 1000, (400, 100, 600, 700), 0.12, False, [])
    assert e.angle is ShotAngle.EYE_LEVEL
    assert e.stated is False


def test_steep_down_needs_a_substantial_subject():
    # A wide sliver high in the frame (a shelf edge, say): too small a
    # height share to claim an angle from.
    e = est(1000, 1000, (50, 100, 1000, 250), 0.14, False, [])
    assert e.angle is ShotAngle.EYE_LEVEL
    assert e.stated is False


def test_steep_edges_are_inclusive():
    # ground_below exactly 0.15 and top exactly 0.33 still count.
    e = est(1000, 1000, (100, 330, 1000, 850), 0.30, False, [])
    assert e.signals.ground_below == 0.15
    assert e.signals.top_y == 0.33
    assert e.angle is ShotAngle.STEEP_DOWN
    assert e.stated is True


def test_low_angle_needs_its_cues():
    # Tall narrow subject standing on a thin strip of ground, centre
    # at the 0.46 line: camera low, looking up.
    e = est(1000, 1000, (300, 20, 700, 900), 0.30, False, [])
    assert e.angle is ShotAngle.LOW_UP
    assert e.stated is True
    assert staging.angle_lock_clause(e) == f"{BASE} {LOW}"
    # Centre at or below the midline: no low claim.
    e = est(1000, 1000, (300, 300, 700, 920), 0.20, False, [])
    assert e.angle is ShotAngle.EYE_LEVEL
    assert e.stated is False
    # Ground contact above the 0.88 line: no low claim.
    e = est(1000, 1000, (300, 60, 700, 860), 0.24, False, [])
    assert e.signals.contact_y < 0.88
    assert e.angle is ShotAngle.EYE_LEVEL
    assert e.stated is False


def test_bottom_cut_subject_states_nothing():
    # Cut by the bottom edge (typical walk-past framing): the contact
    # line is not in the photo, so nothing is claimed beyond the base
    # clause, whatever the bucket would have been.
    e = est(1000, 1000, (100, 150, 1000, 1000), 0.40, False, ["bottom"])
    assert e.stated is False
    assert "bottom edge" in e.reason


def test_frame_filling_subject_states_nothing():
    e = est(1000, 800, (0, 0, 1000, 800), 0.9, True, ["left", "right", "top", "bottom"])
    assert e.stated is False
    assert "fills the frame" in e.reason


def test_no_bbox_and_degenerate_bbox_state_nothing():
    e = est(1000, 800, None, 0.0, False, [])
    assert e.stated is False
    assert e.signals is None
    e = est(1000, 800, (10, 10, 10, 60), 0.0, False, [])
    assert e.stated is False
    assert "degenerate" in e.reason


def test_centred_subject_is_eye_level_unstated():
    e = est(1000, 1000, (300, 300, 700, 700), 0.16, False, [])
    assert e.angle is ShotAngle.EYE_LEVEL
    assert e.stated is False
    assert "inconclusive" in e.reason


# ------------------------------------------------------------ clauses


def test_clause_text_is_the_locked_wording():
    assert BASE == (
        "Keep the exact same camera angle and perspective as the original "
        "photo, same horizon position, same framing."
    )
    steep = est(1920, 1080, (271, 0, 1419, 824), 0.309081, False, ["top"])
    assert staging.angle_lock_clause(steep) == f"{BASE} {STEEP}"
    low = est(1000, 1000, (300, 20, 700, 900), 0.30, False, [])
    assert staging.angle_lock_clause(low) == f"{BASE} {LOW}"
    bike = est(5712, 4284, (330, 419, 5176, 3890), 0.249170, False, [])
    assert staging.angle_lock_clause(bike) == BASE
    assert staging.angle_lock_clause(None) == BASE


def test_record_shape_matches_the_rust_fields():
    steep = est(1920, 1080, (271, 0, 1419, 824), 0.309081, False, ["top"])
    rec = staging.angle_lock_record(steep, staging.angle_lock_clause(steep))
    assert rec["bucket"] == "steep_down"
    assert rec["stated"] is True
    assert rec["clause"] == f"{BASE} {STEEP}"
    assert set(rec["signals"].keys()) == {
        "ground_below", "contact_y", "top_y", "centre_y",
        "aspect", "height_frac", "coverage",
    }
    bike = est(5712, 4284, (330, 419, 5176, 3890), 0.249170, False, [])
    rec = staging.angle_lock_record(bike, staging.angle_lock_clause(bike))
    assert rec["bucket"] == "eye_level"
    assert rec["stated"] is False


# ------------------------------------------------------ prompt builders


def test_bare_object_prompt_gets_styling_and_clause():
    # Empty scene, cut-out subject: the tidy-default styling (decision
    # 119) plus the clause, in that order.
    p = staging.plate_edit_prompt("", False)
    assert p == (
        f"{staging.PLATE_EDIT_OBJECT} Styling only (keep the geometry): "
        f"{staging.PLATE_EDIT_DEFAULT_STYLE}. {BASE}"
    )


def test_surface_prompt_needs_no_styling_sentence():
    # A frame-filling surface has no room to tidy: clause only beyond
    # the surface instruction.
    p = staging.plate_edit_prompt("", True)
    assert p == f"{staging.PLATE_EDIT_SURFACE} {BASE}"


def test_scene_text_keeps_its_own_words_and_ends_with_the_clause():
    steep = est(1920, 1080, (271, 0, 1419, 824), 0.309081, False, ["top"])
    p = staging.plate_edit_prompt_locked("a bright kitchen", False, steep)
    assert p.endswith(f"{BASE} {STEEP}")
    assert "a bright kitchen" in p
    # Scene text without terminal punctuation gains its full stop.
    p = staging.plate_edit_prompt_locked("a bright kitchen.", False, steep)
    assert ".. " not in p


def test_generated_steering_names_the_bucket():
    steep = est(1920, 1080, (271, 0, 1419, 824), 0.309081, False, ["top"])
    steer = staging.generated_angle_steer(steep)
    assert "steep angle" in steer and "Keep the" not in steer
    bike = est(5712, 4284, (330, 419, 5176, 3890), 0.249170, False, [])
    steer = staging.generated_angle_steer(bike)
    assert "eye-level shot" in steer
    scene = staging.generated_plate_scene("a showroom", bike)
    assert scene.endswith(steer)


# ------------------------------------------------------- subject geometry


def test_analyze_subject_on_a_flat_lay_mask():
    # A wide product lying on a floor band in a portrait frame: the
    # geometry the jacket reshoot produced, in miniature.
    img = Image.new("RGBA", (160, 120), (0, 0, 0, 0))
    px = img.load()
    for y in range(24, 90):
        for x in range(20, 140):
            px[x, y] = (10, 20, 30, 255)
    g = staging.analyze_subject(img)
    assert g.bbox == (20, 24, 140, 90)
    assert g.opaque_pixels == 120 * 66
    assert g.fills_frame is False
    assert g.thin is False
    e = staging.estimate_shot_angle(g)
    assert e.angle is ShotAngle.STEEP_DOWN
    assert e.stated is True
    # The geometry JSON is the receipt's staging block: no angle_lock
    # key ever appears in it (results only, never receipts).
    assert "angle_lock" not in g.to_json()
