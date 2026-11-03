"""SAM 3.1 mask wire format: THE PARSER SWAP BOUNDARY.

Everything agenthand knows about how SAM 3.1 returns segmentation —
special-token text with base85 mask payloads — lives in this module and
nowhere else. Behind the boundary sits Meta's official parser package,
``meta-sam-parser`` (PyPI, by Meta Platforms, Inc.; the Python
implementation of the SAM 3 segmentation protocol, companion to the
TypeScript parser). If Meta revises the wire format, or the parser is
ever swapped for another implementation, this module is the only file
that changes: the rest of agenthand consumes ``DecodedMask`` objects —
a full-frame binary raster plus record metadata — and never sees a
payload string.

Wire format (as parsed by the official package): the model's output
text is one line per frame, ``<Nf>`` then comma-separated records
``id<|box;x1=..;y1=..;x2=..;y2=..;w=<frameW>;h=<frameH>|><|mask;x=0;y=0;data=<H>,<W>,<payload>|>``.
Box corners are inclusive on the wire; the parser normalizes them to
half-open bounds. The mask raster is BOX-LOCAL: its own height/width,
placed at the record's bounds inside the frame. SAM caps the raster's
long side at 1024 px, so the raster may be SMALLER than its box; it is
scaled to the box (bilinear, on the unthresholded coverage) and
thresholded once at >= 129 before placement (`_fit_raster_to_box`).
The payload's first character selects the encoding: ``~`` lossless
(coverage raster, thresholded at >= 129 by the decoder) or ``!``
one_bit. An empty lane (no records) is a valid response meaning
"nothing matched".

Assumption recorded for the live verification: image responses use the
same ``<0f>`` frame-zero line as video frame 0 (the parser's image
format requires frame zero), and the segmentation text arrives as the
Responses API's ``output_text``.
"""

from __future__ import annotations

import math
from dataclasses import dataclass

import numpy as np
from PIL import Image

# Byte-translation table: any nonzero raster byte becomes 255.
_TO_FULL = bytes(0 if i == 0 else 255 for i in range(256))

# Coverage at or above this value is "on" (more than half covered);
# the one threshold both encodings share, applied once after the
# raster is scaled to its box. Mirrors the Rust MASK_ON_COVERAGE.
MASK_ON_COVERAGE = 129

# Largest box area `_fit_raster_to_box` will scale to. No honest SAM
# frame holds a bigger box, and scaling to it would allocate without
# bound. Mirrors the Rust MAX_MASK_RASTER_PIXELS.
MAX_MASK_RASTER_PIXELS = 1 << 26  # 67,108,864

# A SAM record joins the union in `_union_masks` only when it alone
# would clear this coverage floor (a fraction of the frame); smaller
# records are specks (a bolt, a leaf, a glint) and are dropped.
# Mirrors the Rust REGION_MIN_COVERAGE (= MIN_COVERAGE).
REGION_MIN_COVERAGE = 0.001


class SamOutputError(RuntimeError):
    """Raised when SAM output text cannot be parsed or decoded at all.

    Distinct from a zero-match: the model answered, but its answer is
    not usable SAM wire text (or a mask payload failed the parser's
    strict decode). Callers treat this like identify's unparseable
    response — the billed reservation stays counted.
    """


@dataclass
class DecodedMask:
    """One mask record decoded and placed into the full frame."""

    object_id: str
    # Half-open bounds inside the frame, in pixels: (left, top, right, bottom).
    bounds: tuple[int, int, int, int]
    # Mask pixels in the record's raster after it was scaled to its
    # box (before frame placement).
    mask_pixels: int
    # Full-frame binary mask, mode "L": 255 where the mask is on, else 0.
    image: Image.Image


def _fit_raster_to_box(mask, bounds: tuple[int, int, int, int]) -> bytes | None:
    """Scale a record's box-local raster to its box, then threshold once.

    Mirror of the Rust `fit_coverage_to_box`. SAM caps the raster's
    long side at 1024 px; placing the raster 1:1 at the box origin (the
    old behaviour) put a shrunken mask in the box's top-left corner.
    The scaling runs on the UNTHRESHOLDED coverage (Pillow BILINEAR,
    the same filter the SAM-frame-to-photo mapping uses) and the
    >= 129 threshold is applied once, afterwards. A raster that
    already matches its box is thresholded unchanged, so those records
    decode exactly as before.

    Returns the 0/1 raster at box size, or None when the box area is
    zero or over MAX_MASK_RASTER_PIXELS — no honest SAM frame holds
    such a box, and scaling to it would allocate without bound; the
    caller skips the record, as the Rust parse path does. Decode
    failures raise, like the strict decoder this replaces.
    """
    # Behind the swap boundary like every other wire-format detail:
    # the lossless coverage decoder is the official package's own,
    # the same function its public decode_mask_to_raster thresholds.
    from meta_sam_parser import decode_mask_to_raster
    from meta_sam_parser._mask_codec import _decode_lossless_raster

    width, height = int(mask.width), int(mask.height)
    if width <= 0 or height <= 0:
        raise ValueError("Mask dimensions must be positive safe integers.")
    box_width = bounds[2] - bounds[0]
    box_height = bounds[3] - bounds[1]
    if mask.encoding == "lossless":
        if not mask.payload.startswith("~"):
            raise ValueError("lossless mask payloads must start with ~.")
        coverage = _decode_lossless_raster(mask.payload, width, height)
    else:
        # one_bit coverage is 0/255 by construction; the public
        # decoder also enforces the canonical-payload check.
        coverage = bytes(255 if v else 0 for v in decode_mask_to_raster(mask))
    area = width * height
    box_area = box_width * box_height
    if box_area == 0 or box_area > MAX_MASK_RASTER_PIXELS:
        return None
    if len(coverage) != area:
        coverage = (coverage + bytes(area))[:area]
    if (width, height) != (box_width, box_height):
        coverage = (
            Image.frombytes("L", (width, height), coverage)
            .resize((box_width, box_height), Image.BILINEAR)
            .tobytes()
        )
    return bytes(1 if v >= MASK_ON_COVERAGE else 0 for v in coverage)


def _place_in_frame(
    raster: bytes,
    width: int,
    height: int,
    bounds: tuple[int, int, int, int],
    frame_width: int,
    frame_height: int,
) -> Image.Image:
    """Place a box-local 0/1 raster into a full-frame "L" image (0/255).

    The raster is clipped against the frame edges explicitly (PIL's
    paste does not clip out-of-frame boxes), so a record whose bounds
    overhang the frame still lands correctly.
    """
    left, top, right, bottom = bounds
    local = Image.frombytes("L", (width, height), raster.translate(_TO_FULL))
    frame = Image.new("L", (frame_width, frame_height), 0)
    # Intersect the destination box with the frame.
    dst_left, dst_top = max(left, 0), max(top, 0)
    dst_right, dst_bottom = min(right, frame_width), min(bottom, frame_height)
    if dst_right <= dst_left or dst_bottom <= dst_top:
        return frame
    src_box = (
        dst_left - left,
        dst_top - top,
        dst_left - left + (dst_right - dst_left),
        dst_top - top + (dst_bottom - dst_top),
    )
    frame.paste(local.crop(src_box), (dst_left, dst_top))
    return frame


def _union_masks(
    masks: list[DecodedMask], frame_width: int, frame_height: int
) -> dict:
    """Union every record that clears the speck floor.

    Mirror of the Rust ``union_masks`` (segmentation.rs), replacing
    the largest-record ``max()``: one phrase can legitimately return
    several regions of one item (a bike's two wheels and frame), and
    taking only the largest record discards the rest. When no record
    clears the floor, the largest record alone is returned so the
    quality gate rejects it with its real numbers instead of the
    attempt reading as a zero-match. ``masks`` must be non-empty.

    Returns {"image": full-frame "L" union (0/255), "object_ids":
    kept records' ids, largest first, "dropped": records dropped
    under the speck floor, "bounds": half-open union of the kept
    records' bounds}.
    """
    if not masks:
        raise ValueError("_union_masks requires at least one mask record")
    frame_pixels = frame_width * frame_height
    floor = max(1, math.ceil(REGION_MIN_COVERAGE * frame_pixels))
    counts = [
        int(np.count_nonzero(np.asarray(m.image))) for m in masks
    ]
    # Largest first; ties keep wire order (a stable sort).
    order = sorted(range(len(masks)), key=lambda i: (-counts[i], i))
    kept = [i for i in order if counts[i] >= floor]
    dropped = len(masks) - len(kept)
    if not kept:
        kept = [order[0]]
    union = np.zeros((frame_height, frame_width), dtype=bool)
    bounds = masks[kept[0]].bounds
    for i in kept:
        m = masks[i]
        union |= np.asarray(m.image) != 0
        bounds = (
            min(bounds[0], m.bounds[0]),
            min(bounds[1], m.bounds[1]),
            max(bounds[2], m.bounds[2]),
            max(bounds[3], m.bounds[3]),
        )
    return {
        "image": Image.fromarray(
            np.where(union, 255, 0).astype(np.uint8), "L"
        ),
        "object_ids": [masks[i].object_id for i in kept],
        "dropped": dropped - 1 if dropped == len(masks) else dropped,
        "bounds": bounds,
    }


def parse_sam_masks(
    output_text: str, frame_width: int, frame_height: int
) -> list[DecodedMask]:
    """Decode every mask record in one SAM image response.

    Returns one DecodedMask per mask record, in response order; an
    empty list is a normal zero-match outcome, not an error. Raises
    SamOutputError when the text cannot be parsed or a payload fails
    strict decoding.
    """
    # Imported here so the swap point is a single, obvious place: to
    # replace the parser, replace what happens inside this function.
    from meta_sam_parser import (
        CompletedOutcome,
        SegmentationMaskRecord,
        image_segmentation_format,
    )

    parser = image_segmentation_format().create_parser()
    try:
        parser.push(output_text or "")
        result = parser.finish(CompletedOutcome()).result
    except Exception as exc:
        raise SamOutputError(
            f"SAM output could not be parsed as segmentation text: {exc}"
        ) from exc

    masks: list[DecodedMask] = []
    for record in result.records:
        if not isinstance(record, SegmentationMaskRecord):
            continue
        bounds = (
            int(record.bounds.left),
            int(record.bounds.top),
            int(record.bounds.right),
            int(record.bounds.bottom),
        )
        try:
            raster = _fit_raster_to_box(record.mask, bounds)
        except Exception as exc:
            raise SamOutputError(
                f"SAM mask for object {record.object_id!r} failed strict "
                f"decoding: {exc}"
            ) from exc
        if raster is None:
            # A box no honest frame can hold; the Rust parse path
            # skips the record the same way.
            continue
        masks.append(
            DecodedMask(
                object_id=record.object_id,
                bounds=bounds,
                mask_pixels=sum(raster),
                image=_place_in_frame(
                    raster,
                    bounds[2] - bounds[0],
                    bounds[3] - bounds[1],
                    bounds,
                    frame_width,
                    frame_height,
                ),
            )
        )
    return masks
