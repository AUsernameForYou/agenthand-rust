"""Deterministic floor/placement checks on the plate as actually fitted.

Classical image evidence rejects measurable obstacles; a pass does not prove
photographic plausibility or camera agreement. Policy thresholds are fixed.
"""

from __future__ import annotations
import cv2
import numpy as np
from PIL import Image, ImageOps

VERSION = "agenthand.plate-quality/2"


class PlateQualityError(RuntimeError):
    code = "PLATE_REJECTED"

    def __init__(self, quality: dict):
        self.quality = quality
        super().__init__("Plate rejected: " + ", ".join(quality["codes"]))


def placement_zone(
    bbox: list[int] | tuple[int, ...], frame_size: tuple[int, int]
) -> list[float]:
    w, h = frame_size
    x0, y0, x1, y1 = bbox
    bw, bh = x1 - x0, y1 - y0
    return [
        round(max(0, x0 - 0.08 * bw) / w, 6),
        round(max(0, y1 - 0.20 * bh) / h, 6),
        round(min(w, x1 + 0.08 * bw) / w, 6),
        round(min(h, y1 + 0.40 * bh) / h, 6),
    ]


def assess_plate(
    plate: Image.Image,
    foreground_bbox: list[int] | tuple[int, ...],
    frame_size: tuple[int, int],
    *,
    code_backdrop: dict | None = None,
) -> dict:
    zone = placement_zone(foreground_bbox, frame_size)
    if code_backdrop is not None:
        from . import backdrop
        from .assets import pixel_hash_of_image

        mode = code_backdrop.get("kind")
        if (
            mode not in backdrop.BACKDROP_STAGE_MODES
            or code_backdrop != backdrop.backdrop_info(mode)
        ):
            raise ValueError("unknown deterministic backdrop specification")
        expected = backdrop.render_plate(mode, *frame_size)
        matched = plate.size == frame_size and pixel_hash_of_image(
            plate.convert("RGBA")
        ) == pixel_hash_of_image(expected.convert("RGBA"))
        return {
            "version": VERSION,
            "grade": "PASS" if matched else "FAIL",
            "codes": [] if matched else ["CODE_BACKDROP_MISMATCH"],
            "metrics": {
                "placement_zone": zone,
                "deterministic_backdrop": mode,
                "renderer_matches": matched,
            },
        }
    # This is the same center fit used by composite_images, applied only to plate.
    fitted = ImageOps.fit(plate.convert("RGB"), frame_size)
    fitted.thumbnail((640, 640))
    rgb = np.asarray(fitted)
    h, w = rgb.shape[:2]
    gray = cv2.cvtColor(rgb, cv2.COLOR_RGB2GRAY)
    smooth = cv2.GaussianBlur(gray, (5, 5), 0)
    edges = cv2.Canny(smooth, 35, 100)
    lines = cv2.HoughLinesP(
        edges,
        1,
        np.pi / 180,
        threshold=max(12, int(0.12 * w)),
        minLineLength=max(8, int(0.18 * w)),
        maxLineGap=max(2, int(0.04 * w)),
    )
    horizontal = []
    if lines is not None:
        for x0, y0, x1, y1 in lines.reshape(-1, 4):
            dx = abs(int(x1) - int(x0))
            dy = abs(int(y1) - int(y0))
            if dx and dy / dx <= 0.06:
                horizontal.append(
                    [
                        round(float(min(x0, x1) / w), 6),
                        round(float((y0 + y1) / 2 / h), 6),
                        round(float(max(x0, x1) / w), 6),
                    ]
                )
    horizontal.sort(key=lambda l: (l[1], l[0], l[2]))
    # Principal broad architectural junction, away from ceiling and image edge.
    junctions = [l for l in horizontal if 0.30 <= l[1] <= 0.76 and l[2] - l[0] >= 0.30]
    junction = max(junctions, key=lambda l: l[2] - l[0], default=None)
    floor_y = junction[1] if junction else None
    codes = []
    uniform = float(np.max(np.std(rgb.astype(np.float32), axis=1))) < 2.0
    if floor_y is None and not uniform:
        codes.append("WALL_FLOOR_LINE_UNCLEAR")
    obstructing = []
    if floor_y is not None:
        for line in horizontal:
            left, y, right = line
            overlap = max(0, min(right, zone[2]) - max(left, zone[0]))
            if (
                y > floor_y + 0.055
                and zone[1] - 0.05 <= y
                and overlap >= 0.28 * max(0.01, zone[2] - zone[0])
            ):
                obstructing.append(line)
        # An empty stage needs continuous floor below the feet too; a
        # slab edge does not become acceptable just below the placement zone.
        if obstructing:
            codes.append("RAISED_SLAB_OR_INTERRUPTED_FLOOR")
        if floor_y > zone[1] + 0.10:
            codes.append("WALL_FLOOR_LINE_IN_PLACEMENT")
    # Enclosed central silhouettes on the floor, e.g. a pot or pedestal.
    central = 0
    contours, _ = cv2.findContours(edges, cv2.RETR_EXTERNAL, cv2.CHAIN_APPROX_SIMPLE)
    for c in contours:
        x, y, cw, ch = cv2.boundingRect(c)
        area = cv2.contourArea(c)
        if area < 0.012 * w * h or cw > 0.85 * w or ch > 0.6 * h:
            continue
        if (
            zone[0] < (x + cw / 2) / w < zone[2]
            and y / h > max(0.4, (floor_y or 0.4) - 0.05)
            and (y + ch) / h > zone[1]
        ):
            central += 1
    if central:
        codes.append("CENTRAL_OBJECT")
    return {
        "version": VERSION,
        "grade": "FAIL" if codes else "PASS",
        "codes": codes,
        "metrics": {
            "placement_zone": zone,
            "wall_floor_y": floor_y,
            "uniform_studio": uniform,
            "obstructing_lines": obstructing,
            "central_objects": central,
        },
    }
