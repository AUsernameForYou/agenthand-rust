#!/usr/bin/env python3
"""Generate the dry-run gate's photo fixture. One-off; spends ~$0.01.

    python scripts/generate_chair_fixture.py

Why this fixture exists: the gate used to default to the verified
run's screenshot (data/assets/e3f0428f...png). A full-bleed screenshot
contains no physical object, so SAM returned NO_MATCH for every phrase
the plan produced ("smartphone" / "mobile phone" included) — the gate
kept failing on the fixture, not on the chain. The gate needs a photo
OF something segmentable: one wooden dining chair in a cluttered
garage, generated here with muse-image-1.0 through the normal billing
choke point — the repo .env key, the real data/ ledger, the spend
cap, exactly like any tool call — and saved as
tests/fixtures/chair_photo.jpg.
"""

from __future__ import annotations

import sys
from io import BytesIO
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO / "src"))

from PIL import Image  # noqa: E402

from agenthand import meta_client  # noqa: E402

PROMPT = (
    "A plain photograph of a single wooden dining chair in a "
    "cluttered garage, natural light."
)
MODEL = "muse-image-1.0"
OUT = REPO / "tests" / "fixtures" / "chair_photo.jpg"


def main() -> int:
    # Key check first: a missing key must fail before any reservation.
    meta_client.require_meta_api_key()
    client = meta_client.build_client()
    outcome: dict = {}

    def _call() -> dict:
        response = client.images.generate(
            model=MODEL, prompt=PROMPT, size="1536x1024", n=1
        )
        outcome["png"] = meta_client._plate_bytes_from_response(response)
        return {"images": 1}

    billing = meta_client.billed_call(
        "fixture", MODEL, meta_client.project_draw_cost_usd(MODEL), _call
    )
    with Image.open(BytesIO(outcome["png"])) as img:
        rgb = img.convert("RGB")
        OUT.parent.mkdir(parents=True, exist_ok=True)
        rgb.save(OUT, format="JPEG", quality=90)
        size = rgb.size
    print(f"wrote {OUT} ({size[0]}x{size[1]})")
    print(f"settled cost: ${billing['actual_usd']:.6f} (call {billing['call_id']})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
