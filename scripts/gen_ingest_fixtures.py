#!/usr/bin/env python3
"""Generate ingest-orientation fixtures from the Python build.

Creates small images whose EXIF orientation lives in a PNG eXIf chunk
(and one JPEG APP1 case), runs the Python ingest normalizer on each,
and records the expected normalized dimensions + RGBA pixel sha256 in
tests/fixtures/ingest/expected.json. The Rust tests assert the same
normalized pixels.

Run with the Python build's venv:
    ~/workspace/agenthand/.venv/bin/python scripts/gen_ingest_fixtures.py
"""

from __future__ import annotations

import hashlib
import io
import json
import sys
from pathlib import Path

from PIL import Image

PY_ROOT = Path.home() / "workspace" / "agenthand"
sys.path.insert(0, str(PY_ROOT / "src"))

from agenthand.ingest import normalize_image_bytes  # noqa: E402

OUT = Path(__file__).resolve().parent.parent / "tests" / "fixtures" / "ingest"
OUT.mkdir(parents=True, exist_ok=True)

# A 4x2 RGB image whose every pixel is distinct, so any flip/rotate
# mistake changes the pixel hash.
PIXELS = [
    (255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 0),
    (0, 255, 255), (255, 0, 255), (128, 64, 32), (10, 20, 30),
]


def base_image() -> Image.Image:
    img = Image.new("RGB", (4, 2))
    img.putdata(PIXELS)
    return img


def expected_for(data: bytes) -> dict:
    normalized = normalize_image_bytes(data)
    with Image.open(io.BytesIO(normalized)) as img:
        rgba = img.convert("RGBA")
        return {
            "width": rgba.width,
            "height": rgba.height,
            "pixel_sha256": hashlib.sha256(rgba.tobytes()).hexdigest(),
        }


cases: dict[str, dict] = {}

for orientation in (2, 3, 5, 6, 7, 8):
    exif = Image.Exif()
    exif[274] = orientation
    buf = io.BytesIO()
    base_image().save(buf, format="PNG", exif=exif)
    data = buf.getvalue()
    name = f"png_exif_{orientation}.png"
    (OUT / name).write_bytes(data)
    cases[name] = expected_for(data)
    print(name, cases[name])

# A PNG with no EXIF at all: normalization must be a plain RGBA copy.
buf = io.BytesIO()
base_image().save(buf, format="PNG")
(OUT / "png_plain.png").write_bytes(buf.getvalue())
cases["png_plain.png"] = expected_for(buf.getvalue())
print("png_plain.png", cases["png_plain.png"])

# The JPEG case from the Python suite (orientation 6 on a red 4x2):
# stored image must be the 2x4 the photographer framed.
exif = Image.Exif()
exif[274] = 6
buf = io.BytesIO()
Image.new("RGB", (4, 2), (255, 0, 0)).save(buf, format="JPEG", exif=exif)
(OUT / "jpeg_exif_6.jpg").write_bytes(buf.getvalue())
cases["jpeg_exif_6.jpg"] = expected_for(buf.getvalue())
print("jpeg_exif_6.jpg", cases["jpeg_exif_6.jpg"])

(OUT / "expected.json").write_text(json.dumps(cases, indent=2) + "\n")
print("wrote", OUT / "expected.json")
