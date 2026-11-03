"""Code-rendered backdrop plates for room runs (no model, no spend).

Two stage modes render their plate in code at the foreground's frame
size instead of calling the image model:

- ``wallpaper``: a white ground tiled with the logo arc-circle motif
  (three 92-degree arcs, 28-degree gaps centred at 90/210/330
  degrees, stroke width 0.08 x motif diameter), diameter 72 px, in
  #00AFE1 at 40% opacity, on a regular grid with offset rows. There
  is deliberately NO contact shadow in this mode.
- ``studio_gray``: a radial gradient, #F4F5F7 at the centre easing
  to #C6CAD0 at the edges (smoothstep falloff).

Both renderers are deterministic: the same size and mode always
produce the same bytes, so a plate stored as an asset and pinned in
a Fidelity Lock receipt can be re-rendered and compared.
"""

from __future__ import annotations

import numpy as np
from PIL import Image, ImageDraw

STAGE_SCENE = "scene"
STAGE_WALLPAPER = "wallpaper"
STAGE_STUDIO_GRAY = "studio_gray"
STAGE_MODES = (STAGE_SCENE, STAGE_WALLPAPER, STAGE_STUDIO_GRAY)
BACKDROP_STAGE_MODES = (STAGE_WALLPAPER, STAGE_STUDIO_GRAY)

WALLPAPER_MOTIF_DIAMETER_PX = 72
WALLPAPER_OPACITY = 0.4
WALLPAPER_CYAN = (0, 175, 225)  # #00AFE1
# PIL arc angles (degrees, clockwise from 3 o'clock): three 92-degree
# arcs separated by 28-degree gaps whose centres sit at 90/210/330.
WALLPAPER_ARCS = [(104, 196), (224, 316), (344, 436)]
WALLPAPER_STROKE_RATIO = 0.08
_WALLPAPER_SUPERSAMPLE = 4

STUDIO_CENTER_RGB = (244, 245, 247)  # #F4F5F7
STUDIO_EDGE_RGB = (198, 202, 208)  # #C6CAD0


def _motif_overlay(width: int, height: int, diameter: int, alpha: int) -> Image.Image:
    """The tiled arc motif as an RGBA overlay, supersampled then reduced."""
    ss = _WALLPAPER_SUPERSAMPLE
    big_w, big_h = width * ss, height * ss
    overlay = Image.new("RGBA", (big_w, big_h), (0, 0, 0, 0))
    draw = ImageDraw.Draw(overlay)
    big_diameter = diameter * ss
    radius = big_diameter / 2.0
    stroke = max(2, round(WALLPAPER_STROKE_RATIO * big_diameter))
    pitch = round(diameter * 1.55) * ss
    row = 0
    y = -pitch // 2
    while y < big_h + pitch:
        x_offset = (pitch // 2) if (row % 2) else 0
        x = -pitch // 2 + x_offset
        while x < big_w + pitch:
            bbox = [x - radius, y - radius, x + radius, y + radius]
            for start, end in WALLPAPER_ARCS:
                draw.arc(bbox, start, end, fill=WALLPAPER_CYAN + (alpha,), width=stroke)
            x += pitch
        y += pitch
        row += 1
    return overlay.resize((width, height), Image.LANCZOS)


def render_wallpaper(
    width: int,
    height: int,
    *,
    diameter: int = WALLPAPER_MOTIF_DIAMETER_PX,
    opacity: float = WALLPAPER_OPACITY,
) -> Image.Image:
    """The wallpaper plate: white ground plus the tiled motif, RGBA."""
    if width <= 0 or height <= 0:
        raise ValueError(f"plate size must be positive, got {width}x{height}")
    alpha = round(255 * opacity)
    plate = Image.new("RGB", (width, height), (255, 255, 255)).convert("RGBA")
    plate.alpha_composite(_motif_overlay(width, height, diameter, alpha))
    return plate


def render_studio_gray(width: int, height: int) -> Image.Image:
    """The studio gray plate: a radial gradient, RGB."""
    if width <= 0 or height <= 0:
        raise ValueError(f"plate size must be positive, got {width}x{height}")
    yy, xx = np.mgrid[0:height, 0:width]
    cx, cy = width * 0.5, height * 0.42
    dist = np.sqrt(
        ((xx - cx) / (width * 0.72)) ** 2 + ((yy - cy) / (height * 0.72)) ** 2
    )
    t = np.clip(dist, 0, 1)
    t = t * t * (3 - 2 * t)
    center = np.array(STUDIO_CENTER_RGB, float)
    edge = np.array(STUDIO_EDGE_RGB, float)
    arr = center[None, None, :] * (1 - t[:, :, None]) + edge[None, None, :] * t[:, :, None]
    return Image.fromarray(arr.astype(np.uint8), "RGB")


def render_plate(stage_mode: str, width: int, height: int) -> Image.Image:
    """Render the plate for a backdrop stage mode at an exact size."""
    if stage_mode == STAGE_WALLPAPER:
        return render_wallpaper(width, height)
    if stage_mode == STAGE_STUDIO_GRAY:
        return render_studio_gray(width, height)
    raise ValueError(f"no code-rendered plate for stage mode {stage_mode!r}")


def backdrop_info(stage_mode: str) -> dict | None:
    """The receipt's backdrop description for a stage mode.

    ``scene`` (the model path) and the local placeholder carry no
    backdrop description: None. The wallpaper entry pins the motif
    geometry the plate was rendered with, including the absence of a
    contact shadow.
    """
    if stage_mode == STAGE_WALLPAPER:
        return {
            "kind": "wallpaper",
            "motif_diameter_px": WALLPAPER_MOTIF_DIAMETER_PX,
            "opacity": WALLPAPER_OPACITY,
            "shadow": False,
        }
    if stage_mode == STAGE_STUDIO_GRAY:
        return {"kind": "studio_gray"}
    return None
