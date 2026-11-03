"""Bounded multi-resolution segmentation with source-only extraction.

The fixed ensemble uses the identify concept and both alternates, followed by
capped top/support tiles. Every SAM record retains phrase/tile provenance.
Consensus, geometry and source evidence choose the foreground; size alone does
not. All acquisition runs inside billed_call. No model pixels enter product.
"""

from __future__ import annotations

import math
from dataclasses import dataclass
from io import BytesIO

import cv2
import numpy as np
from PIL import Image

from . import meta_client
from .sam_masks import DecodedMask, SamOutputError, parse_sam_masks

# QA thresholds. Coverage is mask pixels / frame pixels.
MIN_COVERAGE = 0.001  # below this the mask is effectively empty
MAX_COVERAGE = 0.98  # at or above this the "mask" is ~the whole frame
MIN_DOMINANT_FRACTION = 0.75  # largest component / mask pixels

# The SAM working-resolution cap (decision 68). SAM is sent the stored
# photo as-is up to this many pixels; past it, only the SAM-bound copy
# is downscaled (prepare_sam_png) and the returned mask is mapped back
# onto the full frame (map_mask_to_frame) before anything is cut.
#
# The number is the pipeline's proven envelope, not a guessed provider
# limit: every still the room runs ever sent SAM was 1206x1608 =
# 1,939,248 px (dozens of accepted calls across the v1/v2, narrated,
# and acceptance runs in both builds, plus the M5 working-resolution
# pass), and every video frame was 720x1280, below it. The failure this
# cap answers is measured too: the 4284x5712 (24 MP) original of
# IMG_1340 drew HTTP 413 from SAM's /responses endpoint (M5 live
# proof). The cap math below maps that exact original back onto
# 1206x1608, the frame size already proven live.
SAM_MAX_PIXELS = 1_939_248  # 1206 * 1608

# Mask provenance values recorded on the mask asset and in draw's
# receipt. "none": SAM saw the source frame itself, the mask needed no
# mapping. "bilinear_threshold_128": SAM saw a downscaled copy and its
# mask was mapped back onto the source frame by a bilinear resize
# thresholded at 128 (Fable's masking detail from the M5 junction
# Gauntlet: bilinear on the soft mask + threshold, never nearest on
# binary, so the mapped edge is a measured blend, not a guess).
MASK_SCALE_NONE = "none"
MASK_SCALE_BILINEAR_THRESHOLD = "bilinear_threshold_128"


def sam_frame_size(
    width: int, height: int, max_pixels: int | None = None
) -> tuple[int, int]:
    """The frame SAM sees for a source of (width, height).

    Unchanged when the source fits the working cap; otherwise the
    largest same-aspect frame that fits it. The decrement loop after
    the float math keeps the result inside the cap even when rounding
    lands a pixel over, and makes the answer an integer fact both
    builds reproduce exactly. The cap default resolves at call time
    (not def time) so the module constant stays the single knob.
    """
    if max_pixels is None:
        max_pixels = SAM_MAX_PIXELS
    if width * height <= max_pixels:
        return width, height
    scale = math.sqrt(max_pixels / (width * height))
    w = max(1, int(width * scale))
    h = max(1, int(height * scale))
    while w * h > max_pixels:
        if w >= h:
            w -= 1
        else:
            h -= 1
    return w, h


def prepare_sam_png(
    source_image: Image.Image,
    stored_png_bytes: bytes,
    max_pixels: int | None = None,
) -> dict:
    """The SAM-bound payload for one source photo.

    Returns png_bytes (what segment_image sends), sam_width /
    sam_height (the frame the returned masks will be decoded against),
    and scale_method (the mask-provenance value for the receipt).
    Inside the cap the stored bytes pass through byte-identical, so
    the common path is exactly the pre-scaling pipeline; past it, a
    Lanczos downscale of the decoded source becomes the SAM copy. The
    stored photo itself is never resized — the downscale exists only
    in this payload.
    """
    width, height = source_image.size
    sam_width, sam_height = sam_frame_size(width, height, max_pixels)
    if (sam_width, sam_height) == (width, height):
        return {
            "png_bytes": stored_png_bytes,
            "sam_width": width,
            "sam_height": height,
            "scale_method": MASK_SCALE_NONE,
        }
    small = source_image.convert("RGBA").resize((sam_width, sam_height), Image.LANCZOS)
    buffer = BytesIO()
    small.save(buffer, format="PNG")
    return {
        "png_bytes": buffer.getvalue(),
        "sam_width": sam_width,
        "sam_height": sam_height,
        "scale_method": MASK_SCALE_BILINEAR_THRESHOLD,
    }


def map_mask_to_frame(
    mask_image: Image.Image, frame_width: int, frame_height: int
) -> Image.Image:
    """Map a SAM-frame mask onto the full source frame.

    Identity when the sizes already match. Otherwise a bilinear
    upscale of the (binary) mask, thresholded at 128 back to binary:
    a full-frame pixel is on exactly when its bilinear sample of the
    SAM mask is at least half on. The foreground is then cut from the
    full-resolution source through this mask, so the receipt binds
    full-resolution pixels, not the working copy's.
    """
    if mask_image.size == (frame_width, frame_height):
        return mask_image
    soft = mask_image.convert("L").resize((frame_width, frame_height), Image.BILINEAR)
    return soft.point(lambda v: 255 if v >= 128 else 0)


class CutOutResultError(RuntimeError):
    """Raised when no attempt produced a usable mask.

    Attributes:
        code: "NO_MATCH" when the final attempt returned no mask at
            all, "POOR_MASK" when the final attempt returned a mask
            that failed QA, "BAD_MODEL_RESPONSE" when SAM output
            could not be parsed/decoded.
        attempts: The full attempt trace (see run_attempts).
    """

    def __init__(
        self,
        message: str,
        *,
        code: str,
        attempts: list[dict],
        records: list[dict] | None = None,
    ):
        super().__init__(message)
        self.code = code
        self.attempts = attempts
        self.images = len(attempts)
        self.records = records or []


@dataclass
class MaskAssessment:
    mask_pixels: int
    coverage: float  # mask pixels / frame pixels
    components: int  # connected components in the mask
    dominant_fraction: float  # largest component / mask pixels
    accepted: bool
    rejection: str | None  # "empty" | "full_frame" | "fragmented" | None


def _label_components(binary: np.ndarray, width: int, height: int) -> tuple[int, int]:
    """(component_count, largest_component_size) of a 0/1 raster.

    4-connectivity labelling via OpenCV — validated count-for-count
    against the flood fill it replaced, over randomized rasters.
    ``binary`` is a uint8 array of any shape whose product is
    width * height; nonzero counts as set.
    """
    raster = (binary.reshape(height, width) != 0).astype(np.uint8)
    count, _labels, stats, _centroids = cv2.connectedComponentsWithStats(
        raster, connectivity=4
    )
    if count <= 1:
        return 0, 0
    return int(count - 1), int(stats[1:, cv2.CC_STAT_AREA].max())


def assess_mask(mask_image: Image.Image) -> MaskAssessment:
    """Quality-check one full-frame binary mask ("L", 255 = on)."""
    width, height = mask_image.size
    frame_pixels = width * height
    values = np.frombuffer(mask_image.tobytes(), dtype=np.uint8)
    mask_pixels = int(np.count_nonzero(values))
    coverage = mask_pixels / frame_pixels if frame_pixels else 0.0
    if mask_pixels == 0:
        return MaskAssessment(0, 0.0, 0, 0.0, False, "empty")
    components, largest = _label_components(values, width, height)
    dominant = largest / mask_pixels
    if coverage < MIN_COVERAGE:
        rejection: str | None = "empty"
    elif coverage >= MAX_COVERAGE:
        rejection = "full_frame"
    elif dominant < MIN_DOMINANT_FRACTION:
        rejection = "fragmented"
    else:
        rejection = None
    return MaskAssessment(
        mask_pixels=mask_pixels,
        coverage=coverage,
        components=components,
        dominant_fraction=dominant,
        accepted=rejection is None,
        rejection=rejection,
    )


# The mask outline colour on review overlays: the brand cyan, chosen
# to be visible against most product photos and impossible to mistake
# for a pixel the pipeline produced (the pipeline never paints).
OVERLAY_OUTLINE_RGBA = (0, 175, 225, 255)


def mask_overlay_image(source: Image.Image, mask_image: Image.Image) -> Image.Image:
    """The source photo with the mask's outline drawn over it.

    The review artifact for the lanes' top risk — a wrong mask under a
    perfect receipt. The outline is the set of mask-on pixels with a
    4-neighbour that is mask-off (or outside the frame), painted in
    OVERLAY_OUTLINE_RGBA; every other pixel is the source untouched,
    so a human or an agent can see exactly what the mask selected.
    """
    source_rgba = source.convert("RGBA")
    mask = mask_image.convert("L")
    if mask.size != source_rgba.size:
        raise ValueError(
            f"Mask {mask.size} does not cover the source frame {source_rgba.size}."
        )
    width, height = mask.size
    on = np.frombuffer(mask.tobytes(), dtype=np.uint8).reshape(height, width) != 0
    # Outline = on-pixels with a 4-neighbour off or outside the frame
    # (the zero padding stands in for "outside").
    padded = np.pad(on, 1, constant_values=False)
    surrounded = (
        padded[:-2, 1:-1] & padded[2:, 1:-1] & padded[1:-1, :-2] & padded[1:-1, 2:]
    )
    outline = on & ~surrounded
    outline_image = Image.fromarray((outline * 255).astype(np.uint8), mode="L")
    source_rgba.paste(OVERLAY_OUTLINE_RGBA, (0, 0), outline_image)
    return source_rgba


def review_mask(
    mask_image: Image.Image,
    source: Image.Image | None = None,
    *,
    dropped_component_pixels: int = 0,
) -> dict:
    """Sanity signals for one accepted mask, for the cut_out result.

    A receipt can only prove the SELECTED pixels survived; whether the
    selection is the product is a judgement the pipeline must support,
    not silently make (the J2 Gauntlet's mask-correctness gate).
    identify emits no spatial extent to compare against, so the gate
    reports the mask's own geometry — bounding box, the box's fill
    ratio, coverage, which frame edges the mask touches — and flags
    the shapes that are wrong in practice: a mask touching all four
    frame edges is usually the background selected by mistake, and a
    bounding box that dwarfs its filled area is a scattered selection
    wearing one object's name. The overlay asset is the visual half of
    the same gate; these numbers ride beside it in the result.
    """
    mask = mask_image.convert("L")
    width, height = mask.size
    on = np.frombuffer(mask.tobytes(), dtype=np.uint8).reshape(height, width)
    mask_pixels = int(np.count_nonzero(on))
    warnings: list[str] = []
    if dropped_component_pixels:
        warnings.append(
            f"{dropped_component_pixels} pixels in non-body SAM components were omitted; inspect product completeness in the overlay"
        )
    bbox = None
    bbox_fill = None
    touches: list[str] = []
    if mask_pixels:
        ys, xs = np.nonzero(on)
        x0, x1, y0, y1 = int(xs.min()), int(xs.max()), int(ys.min()), int(ys.max())
        bbox = [x0, y0, x1 + 1, y1 + 1]
        bbox_fill = round(mask_pixels / ((x1 - x0 + 1) * (y1 - y0 + 1)), 6)
        if x0 == 0:
            touches.append("left")
        if x1 == width - 1:
            touches.append("right")
        if y0 == 0:
            touches.append("top")
        if y1 == height - 1:
            touches.append("bottom")
        if len(touches) == 4:
            warnings.append(
                "mask touches all four frame edges; this usually means "
                "the background was selected instead of the product"
            )
        if bbox_fill is not None and bbox_fill < 0.25:
            warnings.append(
                "mask fills less than a quarter of its bounding box; "
                "the selection is scattered or frame-like, review the "
                "overlay before listing"
            )
    from .cut_quality import grade_cut

    return {
        "cut_quality": grade_cut(source, mask),
        "dropped_component_pixels": dropped_component_pixels,
        "mask_pixels": mask_pixels,
        "coverage": round(mask_pixels / (width * height), 6) if width * height else 0.0,
        "bbox": bbox,
        "bbox_fill": bbox_fill,
        "touches_frame_edges": touches,
        "warnings": warnings,
    }


def mask_iou(a: Image.Image, b: Image.Image) -> float:
    """Intersection over union of two masks' on-pixels.

    The golden-fixture gate's metric: a decoded mask is compared
    against the mask it should have produced, so a segmentation
    regression (or the wrong object winning the attempt loop) shows
    up as a number in tests instead of a wrong listing in production.
    """
    if a.size != b.size:
        raise ValueError(f"Mask sizes differ: {a.size} vs {b.size}.")
    a_on = np.frombuffer(a.convert("L").tobytes(), dtype=np.uint8) != 0
    b_on = np.frombuffer(b.convert("L").tobytes(), dtype=np.uint8) != 0
    intersection = int(np.count_nonzero(a_on & b_on))
    union = int(np.count_nonzero(a_on | b_on))
    return intersection / union if union else 1.0


def extract_foreground(source: Image.Image, mask_image: Image.Image) -> Image.Image:
    """The product layer: source pixels through the mask, nothing else.

    Binary masks select exactly: where the mask is fully on, the
    source pixel is copied byte-for-byte; where it is off, the pixel
    becomes transparent black. A partial mask value keeps the source
    RGB straight and scales only alpha (never produced by the SAM
    path, whose rasters are binary by construction). The former
    ``Image.composite`` blend against transparent black also scaled
    RGB — a premultiplied colour stored as straight alpha, which
    composited as a dark fringe. Mirror of the Rust
    ``extract_foreground``.
    """
    from .assets import carry_color_profile

    source_rgba = source.convert("RGBA")
    if mask_image.size != source_rgba.size:
        raise ValueError(
            f"Mask size {mask_image.size} differs from source size "
            f"{source_rgba.size}."
        )
    src = np.asarray(source_rgba).astype(np.uint32)
    m = np.asarray(mask_image.convert("L")).astype(np.uint32)
    out = np.zeros_like(src)
    full = m == 255
    out[full] = src[full]
    partial = (m > 0) & ~full
    if partial.any():
        out[partial, :3] = src[partial, :3]
        out[partial, 3] = (src[partial, 3] * m[partial]) // 255
    foreground = Image.fromarray(out.astype(np.uint8), "RGBA")
    # fromarray drops the source's ICC profile; re-attach it so the
    # product layer keeps the photo's colour space (see
    # assets.carry_color_profile).
    return carry_color_profile(source_rgba, foreground)


PIPELINE_VERSION = "agenthand.cut-pipeline/3"
MIN_TILE_FRAME_SIDE = 128  # sub-thumbnail controls contain no native thin detail


def _body_score(record: dict, records: list[dict]) -> tuple:
    """Agreement, compactness and centering; area alone never chooses a product."""
    from .mask_evidence import geometry

    image = record["image"]
    metrics, _, _ = geometry(np.asarray(image))
    box = metrics["body_bbox"]
    cx = (box[0] + box[2]) / 2 / image.width
    cy = (box[1] + box[3]) / 2 / image.height
    agreement = max(
        (
            mask_iou(image, r["image"])
            for r in records
            if r["phrase"] != record["phrase"]
        ),
        default=0.0,
    )
    defects = (metrics["top_concavity_pixels"] + metrics["top_island_pixels"]) / max(
        1, metrics["mask_pixels"]
    )
    score = (
        agreement + metrics["bbox_fill"] - abs(cx - 0.5) - abs(cy - 0.5) - 2 * defects
    )
    return (score, metrics["dominant_fraction"])


def band_tiles(
    size: tuple[int, int], bbox: tuple[int, int, int, int], slots: int
) -> list[dict]:
    """Fixed bottom/top coverage, overlapping horizontally; every request fits cap.

    Full native resolution whenever the band fits its allocation. Very large
    bands get minimally scaled within each allotted crop, explicitly recorded.
    """
    w, h = size
    x0, y0, x1, y1 = bbox
    bw, bh = x1 - x0, y1 - y0
    if min(w, h) < MIN_TILE_FRAME_SIDE or not bw or not bh or slots < 2:
        return []
    bands = [
        ("support", max(0, y1 - int(0.1 * bh)), min(h, y1 + max(16, int(0.5 * bh)))),
        ("top", max(0, y0 - int(0.1 * bh)), min(h, y0 + max(16, int(0.35 * bh)))),
    ]
    result = []
    for index, (kind, ya, yb) in enumerate(bands):
        allotted = slots // 2 + (slots % 2 if index == 0 else 0)
        needed = max(1, math.ceil(bw * (yb - ya) / (0.85 * SAM_MAX_PIXELS)))
        count = min(needed, allotted)
        tile_w = (
            bw if count == 1 else min(bw, math.ceil(bw / (count - 0.12 * (count - 1))))
        )
        for i in range(count):
            xa = x0 if count == 1 else x0 + round(i * (bw - tile_w) / (count - 1))
            result.append({"kind": kind, "bbox": [xa, ya, xa + tile_w, yb]})
    return result


def _classify_component(
    binary: np.ndarray, body: np.ndarray, source_rgb: np.ndarray, bbox: tuple, kind: str
) -> str:
    """Only narrow, observed members touching the body may expand foreground."""
    from .mask_evidence import _bbox_of, support_min_height, floor_contrast

    x0, y0, x1, y1 = bbox
    bw, bh = x1 - x0, y1 - y0
    cx0, cy0, cx1, cy1 = _bbox_of(binary)
    area = int(binary.sum())
    overlap = int((binary & body).sum())
    if not area:
        return "debris"
    if overlap / area > 0.5:
        return "body"
    if cy1 <= y0 + 0.35 * bh:
        return "top_band_candidate"
    height = cy1 - cy0
    mean_width = area / max(1, height)
    touching = (
        cv2.dilate(body.astype(np.uint8), np.ones((3, 3), np.uint8), iterations=3) > 0
    )
    in_zone = (
        cy0 >= y1 - 0.15 * bh
        and cy1 <= y1 + 0.6 * bh
        and cx0 >= x0 - 0.05 * bw
        and cx1 <= x1 + 0.05 * bw
    )
    floor = source_rgb[min(y1, body.shape[0] - 1) :, x0:x1]
    observed = (
        float(floor_contrast(source_rgb, floor.reshape(-1, 3))[binary].mean()) >= 0.65
    )
    if (
        kind == "support"
        and in_zone
        and (binary & touching).any()
        and height >= support_min_height(bh)
        and height / max(1, mean_width) >= 4
        and mean_width <= 0.06 * bw
        and cy1 < body.shape[0]
        and observed
    ):
        return "support"
    return "debris"


def run_cut_pipeline(
    source: Image.Image,
    stored_png_bytes: bytes,
    phrases: list[str],
    *,
    model: str,
    client=None,
) -> dict:
    """Acquire the fixed phrase ensemble and bounded top/support detail.

    Must be invoked inside billed_call. Every decoded record is returned,
    including rejected/debris records, with source coordinates and provenance.
    """
    from .mask_evidence import geometry, _components

    phrases = list(dict.fromkeys(phrases[:3]))
    if not phrases:
        raise ValueError("at least one fixed phrase is required")
    records = []
    attempts = []
    payload = prepare_sam_png(source, stored_png_bytes)
    fullbox = [0, 0, source.width, source.height]

    def acquire(phrase, tile, data):
        output, _ = meta_client.segment_image(
            data["png_bytes"], phrase, model=model, client=client
        )
        trace = {
            "phrase": phrase,
            "tile": tile,
            "records": 0,
            "sam_width": data["sam_width"],
            "sam_height": data["sam_height"],
            "scale_method": data["scale_method"],
        }
        try:
            masks = parse_sam_masks(output, data["sam_width"], data["sam_height"])
        except SamOutputError as exc:
            trace["outcome"] = "unparseable"
            attempts.append(trace)
            raise CutOutResultError(
                str(exc), code="BAD_MODEL_RESPONSE", attempts=attempts, records=records
            ) from exc
        trace["records"] = len(masks)
        trace["outcome"] = "no_match" if not masks else "poor_mask"
        for mask in masks:
            box = tile["bbox"]
            local = map_mask_to_frame(mask.image, box[2] - box[0], box[3] - box[1])
            mapped = Image.new("L", source.size)
            mapped.paste(local, (box[0], box[1]))
            qa = assess_mask(mapped)
            records.append(
                {
                    "image": mapped,
                    "sam_bounds": list(mask.bounds),
                    "sam_mask_pixels": mask.mask_pixels,
                    "object_id": mask.object_id,
                    "phrase": phrase,
                    "tile": tile,
                    "sam_width": data["sam_width"],
                    "sam_height": data["sam_height"],
                    "scale_method": data["scale_method"],
                    "assessment": qa,
                    "role": "debris",
                    "components": [],
                }
            )
            if qa.accepted:
                trace.update(
                    object_id=mask.object_id,
                    coverage=round(qa.coverage, 6),
                    mask_pixels=qa.mask_pixels,
                    components=qa.components,
                    dominant_fraction=round(qa.dominant_fraction, 6),
                    outcome="accepted",
                )
            else:
                trace.update(
                    coverage=round(qa.coverage, 6),
                    mask_pixels=qa.mask_pixels,
                    components=qa.components,
                    dominant_fraction=round(qa.dominant_fraction, 6),
                )
                trace["rejection"] = qa.rejection
        attempts.append(trace)

    for phrase in phrases:
        acquire(phrase, {"kind": "global", "bbox": fullbox}, payload)
    usable = [r for r in records if r["assessment"].accepted]
    if not usable:
        code = "POOR_MASK" if records else "NO_MATCH"
        raise CutOutResultError(
            "No usable global body in fixed phrase ensemble.",
            code=code,
            attempts=attempts,
            records=records,
        )
    selected = max(usable, key=lambda r: _body_score(r, usable))
    metrics, body, _ = geometry(np.asarray(selected["image"]))
    bbox = tuple(metrics["body_bbox"])
    for tile in band_tiles(
        source.size, bbox, meta_client.SAM_MAX_ATTEMPTS - len(attempts)
    ):
        crop = source.crop(tuple(tile["bbox"]))
        b = BytesIO()
        crop.save(b, format="PNG")
        data = prepare_sam_png(crop, b.getvalue())
        acquire(selected["phrase"], tile, data)
    rgb = np.asarray(source.convert("RGB"))
    merged = body.copy()
    for r in records:
        roles = []
        for box, local in _components(np.asarray(r["image"]) > 0):
            binary = np.zeros_like(body)
            binary[box[1] : box[3], box[0] : box[2]] = local
            role = _classify_component(binary, body, rgb, bbox, r["tile"]["kind"])
            roles.append({"role": role, "bbox": list(box), "pixels": int(local.sum())})
            if role == "support":
                merged |= binary
            if r["tile"]["kind"] == "support" and role == "body":
                # SAM may return a leg attached to a body fragment; assess
                # the new pixels independently, rather than discarding them.
                for extra_box, extra_local in _components(binary & ~body):
                    extra = np.zeros_like(body)
                    extra[extra_box[1] : extra_box[3], extra_box[0] : extra_box[2]] = (
                        extra_local
                    )
                    extra_role = _classify_component(extra, body, rgb, bbox, "support")
                    roles.append(
                        {
                            "role": extra_role,
                            "bbox": list(extra_box),
                            "pixels": int(extra_local.sum()),
                        }
                    )
                    if extra_role == "support":
                        merged |= extra
        r["components"] = roles
        r["role"] = (
            "body"
            if r is selected
            else (
                roles[0]["role"]
                if roles and len({p["role"] for p in roles}) == 1
                else "mixed"
            )
        )
    mask = Image.fromarray((merged * 255).astype(np.uint8))
    return {
        "mask_image": mask,
        # Count unique omitted pixels in the selected global record. Do not
        # sum overlapping alternatives or imply ownership of discarded parts.
        "dropped_component_pixels": int(
            ((np.asarray(selected["image"]) > 0) & ~merged).sum()
        ),
        "phrase": selected["phrase"],
        "attempts": attempts,
        "records": records,
        "pipeline_version": PIPELINE_VERSION,
        "usage": {"images": len(attempts)},
    }


def run_attempts(
    png_bytes: bytes,
    phrases: list[str],
    *,
    model: str,
    frame_width: int,
    frame_height: int,
    client=None,
) -> dict:
    """Compatibility entry point; uses the same ensemble policy as cut_out."""
    with Image.open(BytesIO(png_bytes)) as image:
        source = image.convert("RGBA")
    if source.size != (frame_width, frame_height):
        raise ValueError("SAM frame mismatch")
    return run_cut_pipeline(source, png_bytes, phrases, model=model, client=client)


# ---------------------------------------------------------------------------
# Edge band decontamination (docs/EDGE-FIX.md; mirror of the Rust
# segmentation.rs EdgeField). A cut-out's soft edge pixels
# (0 < alpha < 255) are blends of the product and the ORIGINAL
# background; over a darker scene that sliver of original light reads
# as a bright rim. The band is un-blended against the local original
# background (sampled from the source just outside the footprint) so
# it carries product colour only. Product pixels (alpha 255) are
# never read for writing and never written: the receipt's pixel set
# is the same set, byte for byte.
# ---------------------------------------------------------------------------

#: Distances in an EdgeField saturate here (chessboard px).
EDGE_DIST_CAP = 32
#: Margin around the footprint bbox that the edge field covers.
EDGE_FIELD_MARGIN = 16
#: Original-background sample ring: source pixels outside the
#: footprint at this chessboard distance range (ring 1 is skipped: in
#: the photo it is itself part of the soft edge).
BACKGROUND_RING = (2, 4)
#: Product-colour sample depth inside the product set (1 = boundary).
#: The boundary rings carry the original background's blend (and
#: SAM's staircase misfit), so the estimate prefers pixels 3-5 deep.
PRODUCT_DEPTH = (3, 5)
#: Window radius for the local colour estimates (px), and the wider
#: fallback when the near window holds no sample.
EDGE_SAMPLE_RADIUS = 6
EDGE_SAMPLE_RADIUS_WIDE = 12


def _chessboard_distance(target: np.ndarray, border_is_target: bool) -> np.ndarray:
    """Exact chessboard distance (8-connected, unit steps) to the nearest
    target pixel, two-pass chamfer, saturating at EDGE_DIST_CAP. With
    ``border_is_target``, the area beyond the window is target.

    The Rust implementation walks pixels sequentially; here each pass
    is vectorised per row (the within-row dependency is a running
    minimum, computed with an accumulate over ``value - index``), and
    the result is identical: saturation at the cap commutes with the
    min-plus steps because every step adds exactly 1.
    """
    cap = EDGE_DIST_CAP
    h, w = target.shape
    d = np.where(target, 0, cap).astype(np.int64)
    if border_is_target and h and w:
        border = np.ones((h, w), dtype=bool)
        if h > 2 and w > 2:
            border[1:-1, 1:-1] = False
        d = np.where(border & (d > 1), 1, d)
    idx = np.arange(w)
    # Forward pass: neighbours left, up, up-left, up-right.
    for y in range(h):
        row = d[y]
        if y > 0:
            up = d[y - 1]
            row = np.minimum(row, up + 1)
            if w > 1:
                row[1:] = np.minimum(row[1:], up[:-1] + 1)
                row[:-1] = np.minimum(row[:-1], up[1:] + 1)
        row = np.minimum.accumulate(row - idx) + idx
        d[y] = np.minimum(row, cap)
    # Backward pass: neighbours right, down, down-right, down-left.
    for y in range(h - 1, -1, -1):
        row = d[y]
        if y + 1 < h:
            down = d[y + 1]
            row = np.minimum(row, down + 1)
            if w > 1:
                row[1:] = np.minimum(row[1:], down[:-1] + 1)
                row[:-1] = np.minimum(row[:-1], down[1:] + 1)
        row = np.minimum.accumulate((row + idx)[::-1])[::-1] - idx
        d[y] = np.minimum(row, cap)
    return d.astype(np.uint8)


@dataclass
class EdgeField:
    """Distance maps of a foreground's alpha, over the footprint bbox grown
    by EDGE_FIELD_MARGIN (window coordinates; ``x0``/``y0`` place the
    window in the frame). Footprint = alpha > 0, product set = alpha 255.
    """

    x0: int
    y0: int
    w: int
    h: int
    frame_w: int
    frame_h: int
    #: Foreground alpha in the window.
    alpha: np.ndarray
    #: Chessboard distance to the footprint (0 on it).
    out_fp: np.ndarray
    #: Chessboard distance to the product set (0 on it).
    out_o: np.ndarray
    #: Depth inside the product set (1 = boundary pixel; 0 outside).
    #: Beyond the window counts as outside.
    in_o: np.ndarray

    @classmethod
    def create(cls, foreground: Image.Image) -> "EdgeField | None":
        """Build the field, or None when the foreground has no visible pixel."""
        fg = foreground.convert("RGBA")
        frame_w, frame_h = fg.size
        alpha_full = np.asarray(fg.getchannel("A"))
        visible = alpha_full > 0
        if not visible.any():
            return None
        ys, xs = np.nonzero(visible)
        bx0, by0 = int(xs.min()), int(ys.min())
        bx1, by1 = int(xs.max()) + 1, int(ys.max()) + 1
        m = EDGE_FIELD_MARGIN
        x0, y0 = max(0, bx0 - m), max(0, by0 - m)
        x1, y1 = min(frame_w, bx1 + m), min(frame_h, by1 + m)
        alpha = alpha_full[y0:y1, x0:x1].copy()
        fp = alpha > 0
        o = alpha == 255
        return cls(
            x0=x0,
            y0=y0,
            w=x1 - x0,
            h=y1 - y0,
            frame_w=frame_w,
            frame_h=frame_h,
            alpha=alpha,
            out_fp=_chessboard_distance(fp, border_is_target=False),
            out_o=_chessboard_distance(o, border_is_target=False),
            in_o=_chessboard_distance(~o, border_is_target=True),
        )

    def local_mean(
        self,
        img: np.ndarray,
        cx: int,
        cy: int,
        r: int,
        keep: np.ndarray,
    ) -> tuple[int, int, int] | None:
        """Rounded mean RGB of ``img`` (frame-sized RGBA array) over the
        window pixels within Chebyshev radius ``r`` of (cx, cy) whose
        window position is set in ``keep`` (a window-shaped boolean
        array); None when there are none. Integer math: (sum + n/2) / n,
        like the Rust implementation."""
        x_lo, x_hi = max(0, cx - r), min(self.w, cx + r + 1)
        y_lo, y_hi = max(0, cy - r), min(self.h, cy + r + 1)
        if x_hi <= x_lo or y_hi <= y_lo:
            return None
        mask = keep[y_lo:y_hi, x_lo:x_hi]
        n = int(np.count_nonzero(mask))
        if n == 0:
            return None
        region = img[
            self.y0 + y_lo : self.y0 + y_hi, self.x0 + x_lo : self.x0 + x_hi, :3
        ]
        sums = region[mask].astype(np.uint64).sum(axis=0)
        return tuple(int((int(s) + n // 2) // n) for s in sums)

    def product_colour(
        self, foreground: np.ndarray, cx: int, cy: int
    ) -> tuple[int, int, int] | None:
        """Local product colour near (cx, cy) from the product layer: the
        mean of product pixels PRODUCT_DEPTH deep, falling back to 2+
        deep, then any product pixel (thin parts have no depth), all in
        the EDGE_SAMPLE_RADIUS window."""
        lo, hi = PRODUCT_DEPTH
        return (
            self.local_mean(
                foreground, cx, cy, EDGE_SAMPLE_RADIUS,
                (self.in_o >= lo) & (self.in_o <= hi),
            )
            or self.local_mean(
                foreground, cx, cy, EDGE_SAMPLE_RADIUS, self.in_o >= 2
            )
            or self.local_mean(
                foreground, cx, cy, EDGE_SAMPLE_RADIUS, self.in_o >= 1
            )
        )

    def background_colour(
        self, source: np.ndarray, cx: int, cy: int
    ) -> tuple[int, int, int] | None:
        """Local ORIGINAL background colour near (cx, cy): the mean of the
        source photo over the BACKGROUND_RING outside the footprint,
        near window first, then the wide one."""
        lo, hi = BACKGROUND_RING
        ring = (self.out_fp >= lo) & (self.out_fp <= hi)
        return self.local_mean(
            source, cx, cy, EDGE_SAMPLE_RADIUS, ring
        ) or self.local_mean(source, cx, cy, EDGE_SAMPLE_RADIUS_WIDE, ring)


def unblend_edge_pixel(
    observed: tuple[int, int, int],
    alpha: int,
    background: tuple[int, int, int],
    product: tuple[int, int, int] | None,
) -> tuple[int, int, int]:
    """Un-blend one soft edge pixel: ``observed = a*F + (1-a)*B``, so
    ``F = (observed*255 - (255-a)*B) / a``, clamped. Low-coverage pixels
    amplify noise by 255/a, so the result leans on the local product
    colour there: weight ``min(255, 2a)`` on the un-blend, the rest on
    ``product`` (when known). Integer math throughout."""
    if alpha == 0:
        return product if product is not None else observed
    a = alpha
    w = min(255, 2 * a)
    out = []
    for c in range(3):
        num = observed[c] * 255 - (255 - a) * background[c]
        f = min(255, max(0, (num + a // 2) // a))
        if product is not None:
            out.append((f * w + product[c] * (255 - w) + 127) // 255)
        else:
            out.append(f)
    return tuple(out)


def decontaminate_edge_band(
    source: Image.Image,
    foreground: Image.Image,
    field: "EdgeField | None" = None,
) -> tuple[Image.Image, int, int]:
    """The product layer with its soft edge band decontaminated: every
    pixel with 0 < alpha < 255 keeps its alpha and gets the un-blended
    colour (``unblend_edge_pixel`` against the local original background
    from ``source``); product pixels (alpha 255) and transparent pixels
    are copied unchanged. Returns (layer, edge band pixels,
    decontaminated pixels). A band pixel with no background sample in
    reach is left as it was. ``source`` must share the frame."""
    from .assets import carry_color_profile

    if source.size != foreground.size:
        raise ValueError("source and foreground frames differ")
    fg_rgba = foreground.convert("RGBA")
    src = np.asarray(source.convert("RGBA"))
    fg = np.asarray(fg_rgba)
    out = fg.copy()
    if field is None:
        field = EdgeField.create(foreground)
        if field is None:
            return fg_rgba, 0, 0
    band = 0
    fixed = 0
    ys, xs = np.nonzero((field.alpha > 0) & (field.alpha < 255))
    for y, x in zip(ys.tolist(), xs.tolist()):
        band += 1
        bg = field.background_colour(src, x, y)
        if bg is None:
            continue
        product = field.product_colour(fg, x, y)
        gy, gx = field.y0 + y, field.x0 + x
        observed = (int(fg[gy, gx, 0]), int(fg[gy, gx, 1]), int(fg[gy, gx, 2]))
        out[gy, gx, :3] = unblend_edge_pixel(
            observed, int(field.alpha[y, x]), bg, product
        )
        fixed += 1
    layer = Image.fromarray(out, "RGBA")
    return carry_color_profile(fg_rgba, layer), band, fixed
