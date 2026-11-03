"""Paper-fill reconstruction: generated paper only where the cut is empty.

Decision 93 (Daniel, 2026-10-29) is the spec: when a cut loses part of
an object — thin legs SAM missed, holes where objects sitting on the
product were excluded — fill ONLY the missing regions with the brand
wallpaper motif, shaped like the missing thing and placed from the
source photo's own evidence, so a viewer can tell at a glance what is
real (receipted, untouched) and what is reconstruction (obviously
generated). Paper never lands on a pixel the foreground occupies at
any alpha: the edge band blends with the plate exactly as it does
without paper, and a 1 px guard ring keeps paper off the silhouette
entirely.

The design follows the paper-fill Gauntlet (2026-10-29, 4 of 4 lanes
SHIP WITH CHANGES; synthesis in the hackathon workspace):

- Detection is deterministic code over the stored source and mask.
  No model calls happen here or anywhere in the draw path.
- Holes are topology: enclosed empty components of the foreground
  alpha (flood-fill complement), i.e. the footprints occluders left
  in the product's surface.
- Supports are OBSERVED silhouettes: dark, thin, elongated components
  in the source band below the product that touch it. Nothing is
  mirrored, transferred, or invented; a support the photo does not
  show is refused, and the refusal is recorded.
- Rendering samples the backdrop motif's own geometry (72 px,
  #00AFE1) at full strength, so paper can never be mistaken for the
  40% backdrop plate. Circles stay true in paper coordinates:
  supports use a measured width-profile cylindrical chart
  (U = r(v) * theta), flat regions use screen-space sampling.
  Shading is clamped to [SHADE_MIN, SHADE_MAX] and derived from a
  heavily blurred luminance envelope, so no printed detail from the
  source ghosts into the paper, and paper never reaches the plate's
  brightness (which would camouflage it) or black.
- The layer is stored as its own asset (KIND_RECONSTRUCTION) with a
  region map (KIND_REGION_MAP); the draw receipt discloses both,
  plus per-region provenance and every refusal. The verifier
  recounts the layer and rebuilds the delivered file from its three
  named layers.

All thresholds are pinned constants, not per-object tuning.
"""

from __future__ import annotations

import cv2
import numpy as np
from PIL import Image

from . import backdrop
from .assets import (
    HANDLE_PREFIX,
    KIND_MASK,
    KIND_RECONSTRUCTION,
    KIND_REGION_MAP,
    AssetStore,
)

RENDERER_VERSION = "agenthand.paperfill/2"
MAX_RECONSTRUCTION_FRACTION = 0.40

# The paper print: the backdrop motif's geometry at full strength.
PAPER_MOTIF_DIAMETER_PX = backdrop.WALLPAPER_MOTIF_DIAMETER_PX
PAPER_MOTIF_OPACITY = 1.0
PAPER_MOTIF_COLOR = "#00AFE1"

# Shading clamp: paper is always visibly paper (never as bright as
# the plate's ground, never crushed to black).
SHADE_MIN = 0.55
SHADE_MAX = 0.92

# Paper keeps this many pixels away from any foreground pixel, so the
# edge band's blend partner is provably the plate alone.
GUARD_RING_PX = 1

from .mask_evidence import (
    Region,
    Refusal,
    _bbox_of,
    _components,
    detect_regions,
    TypedMask,
    typed_regions,
    MIN_HOLE_FRACTION,
    MAX_HOLE_FRACTION,
    SUPPORT_TOUCH_DILATE_PX,
    SUPPORT_MIN_ASPECT,
    SUPPORT_MAX_WIDTH_FRACTION,
    SUPPORT_MAX_HEIGHT_FRACTION,
    SUPPORT_MAX_COUNT,
    SUPPORT_MIN_HEIGHT_PX,
    REFUSAL_AGGREGATE_FLOOR_PX,
    REFUSAL_AGGREGATE_FRACTION,
    REGION_CLASS_CODES,
)

_MOTIF_TILE_PX = 2 * round(PAPER_MOTIF_DIAMETER_PX * 1.55)


def _motif_tile() -> np.ndarray:
    """One periodic tile of the backdrop motif, RGBA float in [0, 1].

    The paper print is the backdrop's own arc geometry at full
    strength: white ground, cyan arcs. Sampled modulo the tile size
    it reproduces backdrop._motif_overlay's infinite tiling.
    """
    overlay = backdrop._motif_overlay(
        _MOTIF_TILE_PX, _MOTIF_TILE_PX, PAPER_MOTIF_DIAMETER_PX, 255
    )
    return np.asarray(overlay).astype(np.float64) / 255.0


def _paper_rgb(tile: np.ndarray, u: np.ndarray, v: np.ndarray) -> np.ndarray:
    """Motif colour at paper coordinates (u, v): white + cyan arcs."""
    xi = np.mod(np.rint(u).astype(np.int64), _MOTIF_TILE_PX)
    yi = np.mod(np.rint(v).astype(np.int64), _MOTIF_TILE_PX)
    sample = tile[yi, xi]  # (..., 4) RGBA in [0, 1]
    arc = sample[:, :, 3:4]
    cyan = np.array(backdrop.WALLPAPER_CYAN, dtype=np.float64) / 255.0
    return (1.0 - arc) * 1.0 + arc * cyan[None, None, :]


def _render_region(
    region: Region,
    source_rgb: np.ndarray,
    tile: np.ndarray,
    blurred_luma: np.ndarray | None = None,
) -> np.ndarray:
    """Render one region's paper as an RGB float array over its bbox.

    Shading is clamped to [SHADE_MIN, SHADE_MAX] in every path:
    supports take a Lambert term across their measured width profile;
    flat regions take a heavily blurred luminance envelope of the
    source, normalised per region so no printed detail survives.
    """
    x0, y0, x1, y1 = region.bbox
    local = region.silhouette
    h, w = local.shape
    ys, xs = np.mgrid[y0:y1, x0:x1].astype(np.float64)
    if region.cls == "support":
        coords = np.column_stack(np.where(region.silhouette)).astype(np.float64)
        coords[:, 0] += y0  # silhouette coords are bbox-local
        coords[:, 1] += x0
        centroid = coords.mean(axis=0)
        centered = coords - centroid
        _, _, vt = np.linalg.svd(centered, full_matrices=False)
        axis = vt[0]
        if axis[0] < 0:  # axis points down the member
            axis = -axis
        perp = np.array([-axis[1], axis[0]])
        rel_y = (ys - centroid[0]) * axis[0] + (xs - centroid[1]) * axis[1]
        rel_x = (ys - centroid[0]) * perp[0] + (xs - centroid[1]) * perp[1]
        # Measured width profile: half-width per axial bin.
        bins = np.rint(rel_y).astype(np.int64)
        half = np.zeros_like(rel_y)
        centre_off = np.zeros_like(rel_y)
        for b in np.unique(bins[local]):
            sel = local & (bins == b)
            if sel.any():
                lats = rel_x[sel]
                half[sel] = max((lats.max() - lats.min()) / 2.0, 0.5)
                centre_off[sel] = (lats.max() + lats.min()) / 2.0
        ratio = np.divide(
            rel_x - centre_off, half, out=np.zeros_like(half), where=half > 0
        )
        s = np.clip(ratio, -1.0, 1.0)
        theta = np.arcsin(s)
        u = half * theta
        v = rel_y - rel_y[local].min()
        key = np.deg2rad(-35.0)
        shade = 0.55 + 0.45 * np.maximum(0.0, np.cos(theta - key))
        shade = np.clip(shade, SHADE_MIN, SHADE_MAX)
        provenance_axis = float(np.rad2deg(np.arctan2(axis[1], axis[0])))
        region.provenance.setdefault("axis_deg", round(provenance_axis, 2))
    else:
        u, v = xs, ys
        if blurred_luma is None:
            luma = (
                0.2126 * source_rgb[:, :, 0].astype(np.float64)
                + 0.7152 * source_rgb[:, :, 1].astype(np.float64)
                + 0.0722 * source_rgb[:, :, 2].astype(np.float64)
            ) / 255.0
            blurred_luma = cv2.GaussianBlur(
                luma, (0, 0), sigmaX=PAPER_MOTIF_DIAMETER_PX / 2
            )
        env = blurred_luma[y0:y1, x0:x1]
        vals = env[local]
        lo, hi = float(vals.min()), float(vals.max())
        if hi - lo < 1e-9:
            norm = np.full_like(env, 0.925)
        else:
            norm = 0.85 + 0.15 * (env - lo) / (hi - lo)
        shade = np.clip(norm, SHADE_MIN, SHADE_MAX)
    rgb = _paper_rgb(tile, u, v) * shade[:, :, None]
    out = np.zeros((h, w, 4), dtype=np.uint8)
    out[:, :, :3] = np.clip(np.rint(rgb * 255.0), 0, 255).astype(np.uint8)
    out[:, :, 3] = np.where(local, 255, 0)
    return out


def render_reconstruction(
    source_rgb: np.ndarray,
    fg_alpha: np.ndarray,
    regions: list[Region],
) -> tuple[Image.Image, Image.Image, int]:
    """Render the paper layer and region map for accepted regions.

    The clip is the last operation: paper alpha is zeroed wherever
    the foreground has any alpha, plus a GUARD_RING_PX ring around
    it, so the delivered edge band blends with the plate alone.
    Returns (layer RGBA, region map L, unfilled_pixels) where
    unfilled counts region pixels the clip removed.
    """
    height, width = fg_alpha.shape
    allowed = fg_alpha == 0
    if GUARD_RING_PX:
        near_product = (
            cv2.dilate(
                (fg_alpha > 0).astype(np.uint8),
                np.ones((3, 3), np.uint8),
                iterations=GUARD_RING_PX,
            )
            > 0
        )
        allowed &= ~near_product
    tile = _motif_tile()
    # The flat-region light envelope is a full-frame blur; compute
    # it once for all flat regions, not once per region.
    blurred_luma = None
    if any(r.cls != "support" for r in regions):
        luma = (
            0.2126 * source_rgb[:, :, 0].astype(np.float64)
            + 0.7152 * source_rgb[:, :, 1].astype(np.float64)
            + 0.0722 * source_rgb[:, :, 2].astype(np.float64)
        ) / 255.0
        blurred_luma = cv2.GaussianBlur(
            luma, (0, 0), sigmaX=PAPER_MOTIF_DIAMETER_PX / 2
        )
    layer = np.zeros((height, width, 4), dtype=np.uint8)
    region_map = np.zeros((height, width), dtype=np.uint8)
    claimed = np.zeros((height, width), bool)
    unfilled = 0
    # Claim order: occluders, then supports, then holes — a pixel is
    # paper of exactly one class, so the region map is a partition.
    order = {"occluder": 0, "support": 1, "hole": 2}
    for region in sorted(regions, key=lambda r: order[r.cls]):
        rendered = _render_region(region, source_rgb, tile, blurred_luma)
        x0, y0, x1, y1 = region.bbox
        take = (rendered[:, :, 3] > 0) & allowed[y0:y1, x0:x1] & ~claimed[y0:y1, x0:x1]
        want = int(np.count_nonzero(rendered[:, :, 3] > 0))
        got = int(np.count_nonzero(take))
        unfilled += want - got
        if got == 0:
            region.pixels = 0
            continue
        block = layer[y0:y1, x0:x1]
        block[take] = rendered[take]
        layer[y0:y1, x0:x1] = block
        region_map[y0:y1, x0:x1][take] = REGION_CLASS_CODES[region.cls]
        claimed[y0:y1, x0:x1] |= take
        region.pixels = got
    assert not ((layer[:, :, 3] > 0) & (fg_alpha > 0)).any()
    return (
        Image.fromarray(layer, "RGBA"),
        Image.fromarray(region_map, "L"),
        unfilled,
    )


def plan_reconstruction(
    source_rgb: np.ndarray,
    fg_alpha: np.ndarray,
    supports: list[TypedMask],
    occluders: list[TypedMask],
    *,
    support_mask_handles: tuple[str, ...] = (),
    occluder_mask_handles: tuple[str, ...] = (),
) -> tuple[Image.Image, Image.Image, dict]:
    """Pure reconstruction plan/render for authoring and independent recounts."""
    regions, refusals, unfilled = typed_regions(
        source_rgb, fg_alpha, supports, occluders
    )
    union = np.zeros_like(fg_alpha, dtype=bool)
    for r in regions:
        x0, y0, x1, y1 = r.bbox
        union[y0:y1, x0:x1] |= r.silhouette
    wanted = int(union.sum())
    if (
        wanted / max(1, wanted + int((fg_alpha == 255).sum()))
        > MAX_RECONSTRUCTION_FRACTION
    ):
        refusals.append(
            Refusal(
                "RECONSTRUCTION_TOO_LARGE",
                "evidence exceeds maximum reconstruction fraction",
                wanted,
            )
        )
        unfilled += wanted
        regions = []

    layer_image, region_map_image, clip_unfilled = render_reconstruction(
        source_rgb, fg_alpha, regions
    )
    unfilled += clip_unfilled
    live_regions = [r for r in regions if r.pixels > 0]
    total_pixels = sum(r.pixels for r in live_regions)

    report: dict = {
        "status": "none",
        "renderer": RENDERER_VERSION,
        "evidence": {
            "support_mask_handles": list(support_mask_handles),
            "occluder_mask_handles": list(occluder_mask_handles),
        },
        "layer_handle": None,
        "region_map_handle": None,
        "guard_ring_px": GUARD_RING_PX,
        "motif": {
            "diameter_px": PAPER_MOTIF_DIAMETER_PX,
            "opacity": PAPER_MOTIF_OPACITY,
            "color": PAPER_MOTIF_COLOR,
        },
        "regions": [
            {
                "id": i + 1,
                "class": r.cls,
                "pixels": r.pixels,
                "bbox": list(r.bbox),
                "provenance": r.provenance,
            }
            for i, r in enumerate(live_regions)
        ],
        "refusals": [
            {"code": r.code, "detail": r.detail, "pixels": r.pixels} for r in refusals
        ],
        "reconstruction_pixels": total_pixels,
        "unfilled_gap_pixels": unfilled,
    }
    if total_pixels == 0:
        report["status"] = "refused" if refusals else "none"
        return layer_image, region_map_image, report

    report["status"] = "applied"
    return layer_image, region_map_image, report


def build_reconstruction(
    store: AssetStore,
    foreground_handle: str,
    occluder_mask_handles: tuple[str, ...] = (),
    *,
    support_mask_handles: tuple[str, ...] = (),
) -> dict:
    """Detect, render, and store the paper layer for a foreground.

    Walks the foreground's lineage exactly as draw does (the same
    admission rule), reads the source photo and foreground from the
    store, and stores the layer (parented to the mask record, or to
    the foreground when the mask is its own alpha) and the region map
    (parented to the layer). Occluder masks, when supplied, must be
    mask assets chained to the same source; their silhouettes become
    occluder regions. No model calls; no billing.

    Returns the report draw stamps into the receipt: status
    ("applied" when any paper was laid, else "refused" when refusals
    were recorded, else "none"), the stored handles (None when no
    paper), per-region and refusal records, and counts.
    """
    from .composite import FidelityLockError, validate_foreground

    chain = validate_foreground(store, foreground_handle)
    foreground_record = chain[0]
    source_record = chain[-1]
    mask_record = next((r for r in chain if r.kind == KIND_MASK), None)

    foreground = store.read_image(foreground_handle)
    source = store.read_image(HANDLE_PREFIX + source_record.asset_id).convert("RGB")
    if source.size != foreground.size:
        raise FidelityLockError(
            "source and foreground frames differ; paper-fill works on "
            "the product's own frame only"
        )
    source_rgb = np.asarray(source)
    fg_alpha = np.asarray(foreground.getchannel("A"))

    supports, occluders = load_typed_evidence(
        store,
        foreground_handle,
        support_mask_handles=support_mask_handles,
        occluder_mask_handles=occluder_mask_handles,
    )
    layer_image, region_map_image, report = plan_reconstruction(
        source_rgb,
        fg_alpha,
        supports,
        occluders,
        support_mask_handles=support_mask_handles,
        occluder_mask_handles=occluder_mask_handles,
    )
    if report["reconstruction_pixels"] == 0:
        return report

    parent_id = (
        mask_record.asset_id if mask_record is not None else foreground_record.asset_id
    )
    layer_handle = store.put_image(
        layer_image, kind=KIND_RECONSTRUCTION, source_asset_id=parent_id
    )
    layer_record = store.get_record(layer_handle)
    region_map_handle = store.put_image(
        region_map_image, kind=KIND_REGION_MAP, source_asset_id=layer_record.asset_id
    )
    report["status"] = "applied"
    report["layer_handle"] = layer_handle
    report["region_map_handle"] = region_map_handle
    return report


def load_typed_evidence(
    store: AssetStore,
    foreground_handle: str,
    *,
    support_mask_handles: tuple[str, ...] = (),
    occluder_mask_handles: tuple[str, ...] = (),
) -> tuple[list[TypedMask], list[TypedMask]]:
    """Load explicit/automatic masks, with integrity, frame and source checks."""
    import json
    from .assets import png_text_of_bytes
    from .composite import validate_foreground, FidelityLockError

    chain = validate_foreground(store, foreground_handle)
    source = chain[-1]
    fg = store.read_image(foreground_handle)
    supports = []
    occluders = []
    requests = [(h, "support", None) for h in support_mask_handles] + [
        (h, "occluder", None) for h in occluder_mask_handles
    ]
    mask_record = next((r for r in chain if r.kind == KIND_MASK), None)
    if mask_record:
        text = png_text_of_bytes(store.read_bytes(HANDLE_PREFIX + mask_record.asset_id))
        pipeline = json.loads(text.get("agenthand_cut_pipeline", "{}"))
        for record in pipeline.get("records", []):
            for comp in record.get("components", []):
                if comp["role"] in ("support", "occluder"):
                    requests.append((record["mask_asset"], comp["role"], comp["bbox"]))
    for handle, kind, box in requests:
        record = store.get_record(handle)
        if record.kind != KIND_MASK or record.source_asset_id != source.asset_id:
            raise FidelityLockError(
                "typed evidence must be a mask chained to the same source"
            )
        store.verify_integrity(handle)
        image = store.read_image(handle).convert("L")
        if image.size != fg.size:
            raise FidelityLockError("typed evidence frame differs from source")
        on = np.asarray(image) > 127
        if box is not None:
            clipped = np.zeros_like(on)
            x0, y0, x1, y1 = box
            clipped[y0:y1, x0:x1] = on[y0:y1, x0:x1]
            on = clipped
        typed = TypedMask(kind, on, {"mask_asset": handle, "component_bbox": box})
        (supports if kind == "support" else occluders).append(typed)
    return supports, occluders
