"""Shared, source-frame mask evidence. Detector thresholds are versioned in code.

Grading and rendering share source-backed observations; empty topology
alone never authorizes paper. No model calls or product pixel synthesis.
"""

from __future__ import annotations
from dataclasses import dataclass, field
import cv2
import numpy as np

# Hole gates (fractions of the mask's own area).
MIN_HOLE_FRACTION = 0.0005
MAX_HOLE_FRACTION = 0.40

# Support gates, applied to contrasting components in the band below the
# product (see detect_regions).
SUPPORT_TOUCH_DILATE_PX = 3
SUPPORT_MIN_ASPECT = 4.0
SUPPORT_MAX_WIDTH_FRACTION = 0.06
SUPPORT_MAX_HEIGHT_FRACTION = 0.6
SUPPORT_MAX_COUNT = 6
SUPPORT_MIN_HEIGHT_PX = 12
SUPPORT_MIN_HEIGHT_FRACTION = 0.08


def support_min_height(body_height):
    return max(
        SUPPORT_MIN_HEIGHT_PX, int(np.ceil(SUPPORT_MIN_HEIGHT_FRACTION * body_height))
    )


# Receipt hygiene: support refusals below this size merge into one
# record per code with summed pixels, so a noisy photo cannot flood
# the receipt (floor grain on a real photo produces hundreds). The
# threshold is the larger of an absolute floor and a fraction of the
# mask area, so it scales with the product like the other gates.
REFUSAL_AGGREGATE_FLOOR_PX = 64
REFUSAL_AGGREGATE_FRACTION = 0.001
# Region-map class codes (the region map asset is an L image).
REGION_CLASS_CODES = {"hole": 1, "support": 2, "occluder": 3}

# Motif sampling tile: the backdrop motif's lattice periods are
# pitch (112 px) horizontally and 2 * pitch vertically, so a tile of
# 2 * pitch in both axes sampled modulo reproduces the infinite
# tiling exactly.


@dataclass
class Region:
    """One accepted reconstruction region, in source-frame pixels."""

    cls: str  # "hole" | "support" | "occluder"
    silhouette: np.ndarray  # bool, cropped to bbox
    provenance: dict = field(default_factory=dict)
    pixels: int = 0  # visible paper pixels, set by the renderer
    bbox: tuple[int, int, int, int] = (0, 0, 0, 0)  # half-open


@dataclass
class Refusal:
    code: str
    detail: str
    pixels: int = 0


def _bbox_of(silhouette: np.ndarray) -> tuple[int, int, int, int]:
    ys, xs = np.where(silhouette)
    if len(xs) == 0:
        return (0, 0, 0, 0)
    return (int(xs.min()), int(ys.min()), int(xs.max()) + 1, int(ys.max()) + 1)


def _components(binary: np.ndarray):
    """Yield (bbox, mask) per 8-connected component, mask cropped.

    Full-frame masks per component are not an option at 24 MP: a
    few dozen components would cost gigabytes. Cropped masks keep
    each component at its own size; Region silhouettes follow the
    same convention (silhouette is cropped to Region.bbox).
    """
    count, labels, stats, _centroids = cv2.connectedComponentsWithStats(
        binary.astype(np.uint8), connectivity=8
    )
    for i in range(1, count):
        x, y, w, h, _area = (int(v) for v in stats[i])
        yield (x, y, x + w, y + h), labels[y : y + h, x : x + w] == i


# Fixed source-appearance gates, versioned by cut_quality.VERSION.
PRODUCT_COLOUR_DISTANCE = 35.0
PRODUCT_MATCH_FRACTION = 0.65


def repaired_alpha(alpha, paper):
    """Bridge the paper guard seam for grading only; never alter delivered alpha."""
    bridged = cv2.dilate(paper.astype(np.uint8), np.ones((3, 3), np.uint8)) > 0
    top = np.flatnonzero((alpha > 0).any(axis=1))
    if len(top):
        bridged[: int(top[0])] = False
    return ((alpha > 0) | bridged).astype(np.uint8) * 255


def floor_contrast(rgb, floor_samples):
    """Source contrast against the local floor, for either brightness polarity.

    Chroma distance also preserves coloured members with similar luminance.
    Large uniform backgrounds alone remain non-authorizing geometry.
    """
    if not len(floor_samples):
        return np.zeros(rgb.shape[:2], bool)
    colour = np.median(floor_samples.reshape(-1, 3), axis=0)
    colour = colour.astype(np.float32)
    weights = np.array([0.2126, 0.7152, 0.0722], dtype=np.float32)
    floor_luma = float(colour @ weights)
    floor_chroma = colour - colour.mean()
    result = np.zeros(rgb.shape[:2], bool)
    for start in range(0, rgb.shape[0], 128):
        values = rgb[start : start + 128].astype(np.float32)
        luma = values @ weights
        chroma = values - values.mean(axis=-1, keepdims=True)
        result[start : start + 128] = (
            np.abs(luma - floor_luma) > max(30, 0.45 * floor_luma)
        ) | (np.linalg.norm(chroma - floor_chroma, axis=-1) > 60)
    return result


def resembles_product(source_rgb, product, bbox, local) -> bool:
    """Require most gap pixels to resemble nearby visible product colour.

    Use a local product collar, falling back to the visible body for large
    openings. This is appearance evidence, not semantic ownership proof.
    """
    if source_rgb is None or not product.any() or not local.any():
        return False
    x0, y0, x1, y1 = bbox
    pad = max(4, int(0.05 * max(product.shape)))
    xa, ya = max(0, x0 - pad), max(0, y0 - pad)
    xb, yb = min(product.shape[1], x1 + pad), min(product.shape[0], y1 + pad)
    samples = source_rgb[ya:yb, xa:xb][product[ya:yb, xa:xb]]
    if not len(samples):
        samples = source_rgb[product]
    colour = np.median(samples, axis=0)
    distance = np.linalg.norm(
        source_rgb[y0:y1, x0:x1][local].astype(float) - colour, axis=1
    )
    return float(np.mean(distance <= PRODUCT_COLOUR_DISTANCE)) >= PRODUCT_MATCH_FRACTION


def gap_evidence(source_rgb, alpha, body, gaps):
    """Split topology into source-supported damage and ordinary open geometry."""
    holes = np.zeros_like(body)
    h, w = body.shape
    for box, local in _components(alpha == 0):
        if box[0] and box[1] and box[2] < w and box[3] < h:
            holes[box[1] : box[3], box[0] : box[2]] |= local
    proven = []
    for kind, candidates in (("hole", holes), ("concavity", gaps)):
        for box, local in _components(candidates):
            if resembles_product(source_rgb, body, box, local):
                proven.append((kind, box, local))
    return holes, proven


def detect_regions(
    source_rgb: np.ndarray, fg_alpha: np.ndarray
) -> tuple[list[Region], list[Refusal], int]:
    """Find the regions paper may fill: enclosed holes and supports.

    ``source_rgb`` is the decoded source photo (H, W, 3); ``fg_alpha``
    the foreground's alpha channel. Returns (regions, refusals,
    unfilled_gap_pixels). Everything not accepted stays empty and is
    accounted: either as a recorded refusal or in the unfilled count.
    """
    height, width = fg_alpha.shape
    product_any = fg_alpha > 0
    mask_area = int(np.count_nonzero(product_any))
    regions: list[Region] = []
    refusals: list[Refusal] = []
    unfilled = 0
    if mask_area == 0:
        return regions, refusals, unfilled

    # ---- Holes: empty components the border flood cannot reach. ----
    empty = ~product_any
    hole_candidates: list[tuple[tuple[int, int, int, int], np.ndarray]] = []
    too_small = 0
    for bbox, comp in _components(empty):
        touches_border = (
            bbox[0] == 0 or bbox[1] == 0 or bbox[2] == width or bbox[3] == height
        )
        if touches_border:
            continue
        if not resembles_product(source_rgb, product_any, bbox, comp):
            # A handle, frame opening or bay stays transparent.
            continue
        area = int(np.count_nonzero(comp))
        if area < MIN_HOLE_FRACTION * mask_area:
            too_small += area
            continue
        hole_candidates.append((bbox, comp))
    if too_small:
        refusals.append(
            Refusal(
                "HOLE_TOO_SMALL",
                f"enclosed gaps under {MIN_HOLE_FRACTION} of the mask "
                "area are not papered",
                too_small,
            )
        )
        unfilled += too_small
    hole_total = sum(int(np.count_nonzero(c)) for _b, c in hole_candidates)
    if hole_total > MAX_HOLE_FRACTION * mask_area:
        refusals.append(
            Refusal(
                "HOLES_TOO_LARGE",
                f"enclosed gaps cover {hole_total} px, over "
                f"{MAX_HOLE_FRACTION} of the mask; the mask is wrong, "
                "not the product",
                hole_total,
            )
        )
        unfilled += hole_total
    else:
        for bbox, comp in hole_candidates:
            regions.append(
                Region(
                    cls="hole",
                    silhouette=comp,
                    provenance={"basis": "source_product_appearance"},
                    bbox=bbox,
                )
            )

    # ---- Supports: observed contrasting thin components below the body. ----
    bx0, by0, bx1, by1 = _bbox_of(product_any)
    body_w, body_h = bx1 - bx0, by1 - by0
    widen = int(0.05 * body_w)
    cx0, cx1 = max(0, bx0 - widen), min(width, bx1 + widen)
    # Per-column bottom of the product; the band is what hangs below
    # it in the same column. Columns with no product have no band, so
    # scenery beside the object (the fireplace in the hero photo) is
    # excluded by construction.
    band = np.zeros((height, width), bool)
    bottoms = np.full(width, -1, dtype=np.int64)
    for x in range(cx0, cx1):
        rows = np.where(product_any[:, x])[0]
        if len(rows):
            bottoms[x] = rows.max()
            band[bottoms[x] + 1 :, x] = True
    if band.any():
        candidates = band & floor_contrast(source_rgb, source_rgb[band])
        dilated_product = (
            cv2.dilate(
                product_any.astype(np.uint8),
                np.ones((3, 3), np.uint8),
                iterations=SUPPORT_TOUCH_DILATE_PX,
            )
            > 0
        )
        accepted: list[Region] = []
        for cbox, comp in _components(candidates):
            area = int(np.count_nonzero(comp))
            touch_zone = dilated_product[cbox[1] : cbox[3], cbox[0] : cbox[2]]
            if not (comp & touch_zone).any():
                refusals.append(
                    Refusal(
                        "SUPPORT_NO_TOUCH",
                        "contrasting component below the product does not "
                        "touch it; not evidence of a support",
                        area,
                    )
                )
                unfilled += area
                continue
            comp_h = cbox[3] - cbox[1]
            mean_width = area / max(comp_h, 1)
            aspect = comp_h / max(mean_width, 1e-9)
            reason = None
            if comp_h < support_min_height(body_h):
                reason = (
                    f"height {comp_h} px under {support_min_height(body_h)}; "
                    "too short to be a support"
                )
            elif aspect < SUPPORT_MIN_ASPECT:
                reason = (
                    f"aspect {aspect:.2f} under {SUPPORT_MIN_ASPECT}; not a thin member"
                )
            elif mean_width > SUPPORT_MAX_WIDTH_FRACTION * body_w:
                reason = (
                    f"mean width {mean_width:.1f} px over "
                    f"{SUPPORT_MAX_WIDTH_FRACTION} of the body width; "
                    "too wide to be a support"
                )
            elif comp_h > SUPPORT_MAX_HEIGHT_FRACTION * body_h:
                reason = (
                    f"height {comp_h} px over "
                    f"{SUPPORT_MAX_HEIGHT_FRACTION} of the body height"
                )
            elif cbox[3] >= height:
                reason = (
                    "component reaches the frame bottom; its foot is not in the photo"
                )
            if reason is not None:
                refusals.append(Refusal("SUPPORT_REJECTED", reason, area))
                unfilled += area
                continue
            accepted.append(
                Region(
                    cls="support",
                    silhouette=comp,
                    provenance={
                        "basis": "source_silhouette",
                        "mean_width_px": round(float(mean_width), 2),
                        "aspect": round(float(aspect), 2),
                    },
                    bbox=cbox,
                )
            )
        if len(accepted) > SUPPORT_MAX_COUNT:
            total = sum(int(np.count_nonzero(r.silhouette)) for r in accepted)
            refusals.append(
                Refusal(
                    "SUPPORT_COUNT_OUT_OF_RANGE",
                    f"{len(accepted)} support candidates passed the "
                    f"gates; the cap is {SUPPORT_MAX_COUNT}",
                    total,
                )
            )
            unfilled += total
        else:
            regions.extend(accepted)

    # Receipt hygiene: speck-sized support refusals merge into one
    # record per code with summed pixels, so the refusal list stays
    # readable and the accounting is unchanged.
    aggregate_under = max(
        REFUSAL_AGGREGATE_FLOOR_PX,
        int(REFUSAL_AGGREGATE_FRACTION * mask_area),
    )
    merged: list[Refusal] = []
    small: dict[str, list[Refusal]] = {}
    for refusal in refusals:
        if refusal.pixels < aggregate_under and refusal.code.startswith("SUPPORT"):
            small.setdefault(refusal.code, []).append(refusal)
        else:
            merged.append(refusal)
    for code, group in small.items():
        if len(group) == 1:
            merged.append(group[0])
        else:
            merged.append(
                Refusal(
                    code,
                    f"{len(group)} small components aggregated",
                    sum(r.pixels for r in group),
                )
            )
    refusals = merged

    return regions, refusals, unfilled


def geometry(alpha: np.ndarray) -> tuple[dict, np.ndarray, np.ndarray]:
    """Measured geometry, dominant body and upper hull gaps at source resolution.

    Hull gaps are evidence of missing selection, not permission to invent surface.
    Keeping cropped components avoids allocating one full raster per component.
    """
    on = alpha > 0
    h, w = on.shape
    parts = list(_components(on))
    area = int(on.sum())
    body = np.zeros_like(on)
    if parts:
        box, local = max(parts, key=lambda p: int(p[1].sum()))
        x0, y0, x1, y1 = box
        body[y0:y1, x0:x1] = local
    else:
        box = (0, 0, 0, 0)
    x0, y0, x1, y1 = box
    bw, bh = x1 - x0, y1 - y0
    top_gaps = np.zeros_like(on)
    if bw and bh:
        contours, _ = cv2.findContours(
            body.astype(np.uint8), cv2.RETR_EXTERNAL, cv2.CHAIN_APPROX_SIMPLE
        )
        hull = cv2.convexHull(np.concatenate(contours))
        cv2.fillConvexPoly(top_gaps.view(np.uint8), hull, 1)
        top_gaps &= ~on
        top_gaps[y0 + max(1, int(0.35 * bh)) :] = False
    holes = np.zeros_like(on)
    for b, c in _components(~on):
        if b[0] and b[1] and b[2] < w and b[3] < h:
            holes[b[1] : b[3], b[0] : b[2]] |= c
    top_gaps &= ~holes
    # A convex hull also spans ordinary outer tapers. A top defect must
    # have product on BOTH sides in the same row, rather than merely
    # lying between a silhouette and its convex hull.
    for y in range(y0, min(h, y0 + max(1, int(0.35 * bh)))):
        xs = np.flatnonzero(body[y])
        if not len(xs):
            top_gaps[y] = False
        else:
            top_gaps[y, : int(xs[0]) + 1] = False
            top_gaps[y, int(xs[-1]) :] = False
    # A contact is a separate narrow member below the broad body envelope.
    row_width = body.sum(axis=1)
    broad = np.where(row_width >= max(1, 0.55 * row_width.max()))[0]
    body_bottom = int(broad[-1]) + 1 if len(broad) else y1
    contacts = 0
    if bh and body_bottom < y1:
        tail = body[body_bottom:]
        contacts = sum(
            1 for b, c in _components(tail) if c.sum() >= max(2, 0.0001 * area)
        )
    islands = sum(int(c.sum()) for b, c in parts if b != box and b[1] < y0 + 0.35 * bh)
    ax0, ay0, ax1, ay1 = _bbox_of(on)
    metrics = {
        "mask_pixels": area,
        "coverage": round(area / (h * w), 6),
        "components": len(parts),
        "dominant_fraction": round(int(body.sum()) / area, 6) if area else 0.0,
        "bbox": list(_bbox_of(on)),
        "body_bbox": list(box),
        "bbox_fill": round(area / max(1, (ax1 - ax0) * (ay1 - ay0)), 6),
        "enclosed_hole_pixels": int(holes.sum()),
        "top_concavity_pixels": int(top_gaps.sum()),
        "top_island_pixels": islands,
        "captured_support_components": contacts,
    }
    return metrics, body, top_gaps


@dataclass
class TypedMask:
    """Observed mask with an explicit role; never itself a repair instruction."""

    kind: str  # support | occluder
    silhouette: np.ndarray
    provenance: dict = field(default_factory=dict)


def top_footprints(
    alpha: np.ndarray, occluders: list[TypedMask], source_rgb=None
) -> list[Region]:
    """Bounded top gaps require overlapping, appearance-distinct foreign evidence.

    A product-phrase top-band candidate never authorizes paper. Explicit foreign
    masks also need source appearance at the footprint distinct from the body.
    """
    if source_rgb is None:
        return []
    metrics, body, gaps = geometry(alpha)
    bx0, by0, bx1, by1 = metrics["body_bbox"]
    holes, _ = gap_evidence(None, alpha, body, gaps)
    holes[by0 + max(1, int(0.35 * (by1 - by0))) :] = False
    output = []
    for kind, candidates in (("concavity", gaps), ("hole", holes)):
        for box, local in _components(candidates):
            x0, y0, x1, y1 = box
            area = int(local.sum())
            minimum = (
                max(4, 0.0005 * metrics["mask_pixels"])
                if kind == "hole"
                else max(12, 0.005 * metrics["mask_pixels"])
            )
            if area < minimum:
                continue
            left = body[y0:y1, max(bx0, x0 - 4) : x0].any()
            right = body[y0:y1, x1 : min(bx1, x1 + 4)].any()
            if not (left and right):
                continue
            pad = max(4, int(0.05 * max(body.shape)))
            xa, ya = max(0, x0 - pad), max(0, y0 - pad)
            xb, yb = min(body.shape[1], x1 + pad), min(body.shape[0], y1 + pad)
            samples = source_rgb[ya:yb, xa:xb][body[ya:yb, xa:xb]]
            if not len(samples):
                continue
            colour = np.median(samples, axis=0)
            for occ in occluders:
                if occ.kind != "occluder":
                    continue
                overlap = local & occ.silhouette[y0:y1, x0:x1]
                if int(overlap.sum()) < 0.5 * area:
                    continue
                distances = np.linalg.norm(
                    source_rgb[y0:y1, x0:x1][local].astype(float) - colour, axis=1
                )
                if float(np.mean(distances > 60)) < PRODUCT_MATCH_FRACTION:
                    continue
                output.append(
                    Region(
                        "occluder",
                        local.copy(),
                        {
                            "basis": "bounded_top_footprint",
                            "topology_kind": kind,
                            "appearance": "distinct_from_body",
                            "evidence": occ.provenance,
                        },
                        bbox=box,
                    )
                )
                break
    return output


def skeleton_support_cores(source_rgb: np.ndarray, alpha: np.ndarray) -> list[Region]:
    """Extract only a narrow observed stem before it fuses into wide shadow.

    The distance-ridge width and row width must both stay narrow. Branching,
    disconnected rows, clipped feet and wide attachments cannot supply a core.
    This does not invent the hidden foot or authorize the surrounding shadow.
    """
    metrics, body, _ = geometry(alpha)
    x0, y0, x1, y1 = metrics["body_bbox"]
    bw, bh = x1 - x0, y1 - y0
    if not bw or y1 >= alpha.shape[0]:
        return []
    band = source_rgb[y1:, x0:x1]
    dark = floor_contrast(band, band.reshape(-1, 3))
    results = []
    for box, local in _components(dark):
        xa, ya, xb, yb = box
        if ya > 3 or yb >= band.shape[0]:
            continue
        distance = cv2.distanceTransform(
            np.pad(local.astype(np.uint8), 1), cv2.DIST_L2, 5
        )[1:-1, 1:-1]
        core = np.zeros_like(local)
        for row in range(local.shape[0]):
            xs = np.where(local[row])[0]
            if not len(xs) or np.any(np.diff(xs) > 1):
                break
            if len(xs) > 0.06 * bw or 2 * distance[row].max() > 0.06 * bw + 1:
                break
            core[row] = local[row]
        height = int(np.count_nonzero(core.any(axis=1)))
        if height < support_min_height(bh) or height > SUPPORT_MAX_HEIGHT_FRACTION * bh:
            continue
        if height == local.shape[0]:
            continue  # ordinary supports use legacy detector
        if height / (int(core.sum()) / height) < SUPPORT_MIN_ASPECT:
            continue
        box2 = (x0 + xa, y1 + ya, x0 + xb, y1 + ya + height)
        results.append(
            Region(
                "support",
                core[:height],
                {
                    "basis": "source_skeleton_width_core",
                    "max_width_px": int(core.sum(axis=1).max()),
                    "shadow_excluded": True,
                },
                bbox=box2,
            )
        )
    return results


def typed_regions(
    source_rgb: np.ndarray,
    alpha: np.ndarray,
    supports: list[TypedMask],
    occluders: list[TypedMask],
) -> tuple[list[Region], list[Refusal], int]:
    regions, refusals, unfilled = detect_regions(source_rgb, alpha)
    metrics, body, _ = geometry(alpha)
    bx0, by0, bx1, by1 = metrics["body_bbox"]
    bw, bh = bx1 - bx0, by1 - by0
    touch = (
        cv2.dilate(body.astype(np.uint8), np.ones((3, 3), np.uint8), iterations=3) > 0
    )
    floor = source_rgb[min(by1, alpha.shape[0] - 1) :, bx0:bx1]
    observed_pixels = floor_contrast(source_rgb, floor.reshape(-1, 3))
    for item in supports:
        for box, local in _components(item.silhouette & (alpha == 0)):
            x0, y0, x1, y1 = box
            area = int(local.sum())
            height = y1 - y0
            width = area / max(1, height)
            observed = float(observed_pixels[y0:y1, x0:x1][local].mean()) >= 0.65
            valid = (
                y0 >= by1 - 0.15 * bh
                and y1 < alpha.shape[0]
                and height >= support_min_height(bh)
                and height <= 0.6 * bh
                and height / max(1, width) >= 4
                and width <= 0.06 * bw
                and (local & touch[y0:y1, x0:x1]).any()
                and observed
            )
            if valid:
                regions.append(
                    Region(
                        "support",
                        local,
                        {"basis": "typed_support_mask", **item.provenance},
                        bbox=box,
                    )
                )
            else:
                refusals.append(
                    Refusal(
                        "SUPPORT_EVIDENCE_AMBIGUOUS",
                        "typed support lacks narrow, touching source evidence",
                        area,
                    )
                )
                unfilled += area
    regions.extend(skeleton_support_cores(source_rgb, alpha))
    footprints = top_footprints(alpha, occluders, source_rgb)
    regions.extend(footprints)
    for occ in occluders:
        matched = any(
            (
                occ.silhouette[r.bbox[1] : r.bbox[3], r.bbox[0] : r.bbox[2]]
                & r.silhouette
            ).any()
            for r in footprints
        )
        if not matched:
            refusals.append(
                Refusal(
                    "OCCLUDER_WITHOUT_FOOTPRINT",
                    "foreign-object evidence has no bounded top footprint; the object itself is not papered",
                    int(occ.silhouette.sum()),
                )
            )
    support_union = np.zeros_like(alpha, dtype=bool)
    for region in regions:
        if region.cls == "support":
            x0, y0, x1, y1 = region.bbox
            support_union[y0:y1, x0:x1] |= region.silhouette
    if sum(1 for _ in _components(support_union)) > SUPPORT_MAX_COUNT:
        refused = int(support_union.sum())
        regions = [r for r in regions if r.cls != "support"]
        refusals.append(
            Refusal(
                "SUPPORT_COUNT_OUT_OF_RANGE",
                "typed and inferred supports exceed the shared count cap",
                refused,
            )
        )
        unfilled += refused
    return regions, refusals, unfilled
