"""Fail-closed stageability policy, separate from pixel-preservation proof.

Version 5 also requires appearance-distinct evidence for foreign footprints. It is a bounded
heuristic, not a claim of semantic recognition or corpus-wide calibration.
"""

from __future__ import annotations
import numpy as np
from PIL import Image
from . import mask_evidence as evidence

VERSION = "agenthand.cut-quality/5"
INSTRUCTIONS = {
    "SUPPORT_NOT_CAPTURED": "Move bins and clutter away from under the product. Photograph from a lower angle with every leg and foot visible against unobstructed, contrasting floor.",
    "TOP_OCCLUDED": "Remove everything resting on the product. Photograph its complete top and edges without objects overlapping them.",
    "TOP_PERFORATED": "Clear the top and photograph from an angle that shows the entire top surface and its edges without gaps or overlapping clutter.",
    "MASK_FRAGMENTED": "Place one complete product against a plain contrasting background, leave space around every edge, and keep the whole product in frame.",
}


def grade_cut(
    source: Image.Image | np.ndarray | None,
    mask: Image.Image | np.ndarray,
    *,
    occluders: list[evidence.TypedMask] | None = None,
) -> dict:
    alpha = np.asarray(mask.convert("L") if isinstance(mask, Image.Image) else mask)
    if alpha.ndim != 2:
        raise ValueError("cut mask must be a single-channel source-frame raster")
    metrics, body, gaps = evidence.geometry(alpha)
    rgb = None
    if source is not None:
        rgb = np.asarray(
            source.convert("RGB") if isinstance(source, Image.Image) else source
        )
        if rgb.shape != (*alpha.shape, 3):
            raise ValueError("source and cut mask frames must match")
    holes, proven = evidence.gap_evidence(rgb, alpha, body, gaps)
    area = metrics["mask_pixels"]
    codes: list[str] = []
    fatal = False
    if (
        not area
        or metrics["coverage"] >= 0.98
        or metrics["coverage"] < 0.001
        or metrics["dominant_fraction"] < 0.75
    ):
        codes.append("MASK_FRAGMENTED")
        fatal = True
    if metrics["top_island_pixels"] > max(4, 0.001 * area):
        codes.append("TOP_OCCLUDED")
        fatal = True
    footprints = (
        evidence.top_footprints(alpha, occluders or [], rgb) if occluders else []
    )
    proven_top = sum(int(r.silhouette.sum()) for r in footprints)
    metrics["proven_top_footprint_pixels"] = proven_top
    supported_concavity = sum(
        int(local.sum()) for kind, box, local in proven if kind == "concavity"
    )
    top_limit = metrics["body_bbox"][1] + max(
        1, int(0.35 * (metrics["body_bbox"][3] - metrics["body_bbox"][1]))
    )
    supported_holes = sum(
        int(local[: max(0, top_limit - box[1])].sum())
        for kind, box, local in proven
        if kind == "hole"
    )
    metrics["source_supported_top_concavity_pixels"] = supported_concavity
    metrics["source_supported_top_hole_pixels"] = supported_holes
    proven_gap_pixels = sum(int(local.sum()) for _, _, local in proven)
    metrics["open_geometry_pixels"] = max(
        0, int(holes.sum()) + int(gaps.sum()) - proven_gap_pixels - proven_top
    )
    foreign_concavity = sum(
        int(r.silhouette.sum())
        for r in footprints
        if r.provenance["topology_kind"] == "concavity"
    )
    foreign_holes = proven_top - foreign_concavity
    metrics["foreign_top_hole_pixels"] = foreign_holes
    metrics["foreign_top_concavity_pixels"] = foreign_concavity
    top_defect = max(supported_concavity, foreign_concavity)
    if top_defect > max(12, 0.005 * area):
        codes.append("TOP_OCCLUDED")
        fatal |= supported_concavity - foreign_concavity > max(12, 0.005 * area)
    if max(supported_holes, foreign_holes) > max(4, 0.0005 * area):
        codes.append("TOP_PERFORATED")
        fatal |= max(supported_holes, foreign_holes) > 0.4 * area
    fatal |= proven_top > 0.4 * area
    metrics["observed_support_pixels"] = 0
    metrics["uncaptured_support_pixels"] = 0
    metrics["ambiguous_support_pixels"] = 0
    if source is not None:
        regions, refusals, _ = evidence.detect_regions(rgb, alpha)
        supports = sum(int(r.silhouette.sum()) for r in regions if r.cls == "support")
        ambiguous = 0
        # A bounded dark mass can hide a support. Broad, border-connected
        # background alone is not evidence that a product has legs: applying
        # that assumption to screens/flat objects is a false reshoot.
        if area:
            x0, y0, x1, y1 = metrics["body_bbox"]
            if y1 < alpha.shape[0]:
                margin = max(1, int(0.05 * (x1 - x0)))
                band_x0 = max(0, x0 - margin)
                band = rgb[y1:, band_x0 : min(alpha.shape[1], x1 + margin)]
                dark = evidence.floor_contrast(band, band.reshape(-1, 3))
                for b, c in evidence._components(dark):
                    height = b[3] - b[1]
                    mean_width = int(c.sum()) / max(1, height)
                    if b[1] > 3 or height < evidence.support_min_height(y1 - y0):
                        continue
                    thin = (
                        height / max(1, mean_width) >= evidence.SUPPORT_MIN_ASPECT
                        and mean_width
                        <= evidence.SUPPORT_MAX_WIDTH_FRACTION * (x1 - x0)
                    )
                    inside = c[
                        :, max(0, x0 - band_x0 - b[0]) : max(0, x1 - band_x0 - b[0])
                    ]
                    if int(inside.sum()) < 0.5 * int(c.sum()):
                        continue
                    bounded = b[3] < band.shape[0]
                    horizontally_bounded = b[0] > 0 and b[2] < band.shape[1]
                    # A wide flat band within the body envelope is ordinary
                    # contact shadow. Hidden supports need extra geometry.
                    extends_body = band_x0 + b[0] < x0 or band_x0 + b[2] > x1
                    core_rows = 0
                    core_pixels = 0
                    for row in c:
                        xs = np.flatnonzero(row)
                        if (
                            not len(xs)
                            or np.any(np.diff(xs) > 1)
                            or len(xs) > evidence.SUPPORT_MAX_WIDTH_FRACTION * (x1 - x0)
                        ):
                            break
                        core_rows += 1
                        core_pixels += len(xs)
                    stem = (
                        core_rows >= evidence.support_min_height(y1 - y0)
                        and core_rows / max(1, core_pixels / max(1, core_rows))
                        >= evidence.SUPPORT_MIN_ASPECT
                    )
                    if (
                        not thin
                        and bounded
                        and horizontally_bounded
                        and (extends_body or stem)
                    ) or (
                        thin
                        and (
                            not bounded
                            or height > evidence.SUPPORT_MAX_HEIGHT_FRACTION * (y1 - y0)
                        )
                    ):
                        ambiguous += int(c.sum())
        metrics["observed_support_pixels"] = supports
        metrics["uncaptured_support_pixels"] = supports
        metrics["ambiguous_support_pixels"] = ambiguous
        if supports > max(4, 0.0005 * area) or ambiguous > max(64, 0.01 * area):
            codes.append("SUPPORT_NOT_CAPTURED")
            fatal |= ambiguous > max(64, 0.01 * area)
    codes = list(dict.fromkeys(codes))
    return {
        "version": VERSION,
        "grade": "RESHOOT" if fatal else "REPAIRABLE" if codes else "PASS",
        "codes": codes,
        "instructions": [INSTRUCTIONS[c] for c in codes],
        "source_evidence": source is not None,
        "metrics": metrics,
    }
