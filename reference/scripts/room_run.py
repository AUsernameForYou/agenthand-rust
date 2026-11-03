#!/usr/bin/env python3
"""ROOM RUN CLI — the multi-item room intake (see agenthand.roomrun).

    python scripts/room_run.py --photos ROOM_DIR \
        --video WALKTHROUGH.mp4 \
        --items "straw hat, black hat, orange suitcase" \
        --out ~/roomrun-out [--budget-usd 1.00]

Stages every named item found in the room's photos through the
shipped chain (identify -> cut_out -> draw), one Fidelity Lock
receipt per item, each item then re-verified independently from the
stored files, and writes manifest.json + summary.txt to --out.
--stage-mode wallpaper|studio_gray stages on code-rendered plates
instead of a generated scene; --comps FILE prices every item from
agent-supplied Marketplace comps and adds the priced table
(priced-table.png, priced-table.html, pricing-notes.md) to --out.
Items can also be named by the seller's voice: --narrated-video /
--narrated-audio transcribe the walkthrough narration with one
Baseten Muse Spark Open call (Baseten partner pool, accounted in
the manifest separately from the Meta total).
This spends REAL Meta credits through the existing billed_call choke
point; the per-run budget (default $1.00) is projection-checked
before the first call and enforced as the ledger delta grows.
Receipts land in the run's own ledger (<out>/ledger.jsonl) by
default, isolated from the server's cumulative ledger.
"""

from __future__ import annotations

import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO / "src"))

from agenthand.roomrun import main  # noqa: E402

if __name__ == "__main__":
    raise SystemExit(main())
