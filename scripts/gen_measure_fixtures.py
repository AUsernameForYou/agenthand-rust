#!/usr/bin/env python3
"""Generate the Rust estimator parity fixtures from the Python build.

Renders the exact scenes the Python batteries use (tests/test_measure.py
and tests/test_measure_card_sizes.py in ~/workspace/agenthand), saves
them as PNGs under tests/fixtures/measure/, runs the Python estimator
on each, and records the outcomes in expected.json. The Rust tests
then assert the same behavioural contract on the same input bytes:
containment of the truth inside the stated range, the same refusals
with the same plain reasons, comparable centres/ranges.

Also records which detection seed paths fired per scene (sheet seed /
hough fallback), by wrapping the measure module's seed functions.

Run with the Python build's venv:
    ~/workspace/agenthand/.venv/bin/python scripts/gen_measure_fixtures.py
"""

from __future__ import annotations

import importlib.util
import json
import sys
from pathlib import Path

import cv2
import numpy as np

PY_ROOT = Path.home() / "workspace" / "agenthand"
sys.path.insert(0, str(PY_ROOT / "src"))

from agenthand import measure  # noqa: E402

OUT = Path(__file__).resolve().parent.parent / "tests" / "fixtures" / "measure"
OUT.mkdir(parents=True, exist_ok=True)


def load_module(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


tm = load_module("test_measure", PY_ROOT / "tests" / "test_measure.py")
tb = load_module(
    "test_measure_card_sizes", PY_ROOT / "tests" / "test_measure_card_sizes.py"
)

# --- seed-path instrumentation -------------------------------------------
seed_log: dict[str, dict] = {}
_current = {"scene": None}


def _wrap(name):
    orig = getattr(measure, name)

    def wrapper(*args, **kwargs):
        result = orig(*args, **kwargs)
        entry = seed_log.setdefault(_current["scene"], {})
        entry[name] = result is not None
        return result

    setattr(measure, name, wrapper)


_wrap("_sheet_seed")
_wrap("_hough_quad_seed")

expected: dict[str, dict] = {}


def record(name: str, path: Path, card_id: str, profile: Path, truth, **kw):
    row = {"file": path.name, "card_id": card_id, "truth_mm": list(truth)}
    _current["scene"] = name
    try:
        result = measure.estimate(str(path), card_id, profile, **kw)
        row["outcome"] = "ok"
        row["result"] = result
    except measure.MeasureError as exc:
        row["outcome"] = "refused"
        row["error"] = str(exc)
    row["seeds"] = seed_log.get(name, {})
    expected[name] = row
    print(name, row["outcome"], flush=True)


def save(img: np.ndarray, name: str) -> Path:
    path = OUT / f"{name}.png"
    cv2.imwrite(str(path), img)
    return path


# --- card-size battery scenes --------------------------------------------
for name, profile, card_id, pw, ph, jitter, degrade, layout in tb.SCENES:
    card = measure.load_card_profile(profile, card_id)
    photo, card_px = tb.render_scene(
        card, pw, ph, jitter=jitter, degrade=degrade, **layout
    )
    path = save(photo, name)
    record(name, path, card_id, profile, layout["obj_wh"])
    expected[name]["renderer_card_px"] = card_px

# --- test_measure scenes ---------------------------------------------------
# clean case 1 is pixel-identical to battery ah46_close; add the rest.
for i, (obj_xy, obj_wh, jitter) in enumerate(tm.CLEAN_CASES):
    if i == 0:
        continue
    photo, truth, _H = tm.render_scene(obj_xy, obj_wh, jitter)
    record(f"tm_clean{i + 1}", save(photo, f"tm_clean{i + 1}"), "AH-46",
           tm.PROFILE_PATH, truth)

for i, (obj_xy, obj_wh, jitter, degrade) in enumerate(tm.DEGRADED_CASES):
    photo, truth, _H = tm.render_scene(obj_xy, obj_wh, jitter, degrade)
    record(f"tm_deg{i + 1}", save(photo, f"tm_deg{i + 1}"), "AH-46",
           tm.PROFILE_PATH, truth)

# mask and bbox variants of the clean-1 scene (== ah46_close bytes).
photo, truth, H = tm.render_scene((262.0, 90.0), (180.0, 110.0), 30)
quad = tm._object_quad_photo(H)
mask = np.zeros(photo.shape[:2], np.uint8)
cv2.fillPoly(mask, [quad.astype(np.int32)], 255)
mask_path = OUT / "tm_mask.png"
cv2.imwrite(str(mask_path), mask)
mask_arr = cv2.imread(str(mask_path), cv2.IMREAD_GRAYSCALE)
record("tm_mask_path", OUT / "ah46_close.png", "AH-46", tm.PROFILE_PATH,
       truth, mask=mask_arr)
expected["tm_mask_path"]["mask_file"] = mask_path.name

x, y = quad.min(axis=0)
x2, y2 = quad.max(axis=0)
record(
    "tm_bbox_path",
    OUT / "ah46_close.png",
    "AH-46",
    tm.PROFILE_PATH,
    truth,
    bbox=(float(x), float(y), float(x2 - x), float(y2 - y)),
)
expected["tm_bbox_path"]["bbox"] = [float(x), float(y), float(x2 - x),
                                    float(y2 - y)]

# blank scene: no card at all -> honest refusal.
blank = np.full((600, 900, 3), tm.SURFACE_BGR, np.uint8)
record("tm_blank", save(blank, "tm_blank"), "AH-46", tm.PROFILE_PATH, (0, 0))

(OUT / "expected.json").write_text(json.dumps(expected, indent=2) + "\n")
print("wrote", OUT / "expected.json", "scenes:", len(expected))
