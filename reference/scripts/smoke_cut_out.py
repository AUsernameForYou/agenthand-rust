#!/usr/bin/env python3
"""Live smoke test for the wired cut_out tool — run manually, never in CI.

Usage:
    META_API_KEY=... .venv/bin/python scripts/smoke_cut_out.py <image-path> [concept]

Reads the Meta Model API key from the environment (or a local .env via
agenthand's config) — never from a file argument, never printed. Runs
one real cut_out through the MCP tool layer: with a concept argument it
segments that phrase directly; without one, the identify planning step
runs first (a second, separately billed call) to produce the concept
and alternates. Prints the structured result — foreground and mask
asset handles, hashes, the attempt trace — and the ledger receipt
lines the calls wrote.

This makes REAL billable API calls (SAM 3.1 is $2.50 per 1,000 images:
each attempt is a quarter of a cent, reserved and settled through the
spend-cap choke point; the recovery loop makes at most 2 attempts).
Do not run it without a key configured and the credits/cap state
understood.
"""

from __future__ import annotations

import base64
import json
import mimetypes
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from agenthand import config, server  # noqa: E402


def main() -> int:
    if len(sys.argv) not in (2, 3):
        print(__doc__)
        return 2
    image_path = Path(sys.argv[1])
    concept = sys.argv[2] if len(sys.argv) == 3 else None
    data = image_path.read_bytes()
    mime = mimetypes.guess_type(image_path.name)[0] or "image/png"
    data_uri = f"data:{mime};base64," + base64.b64encode(data).decode()

    ledger_path = config.get_ledger_path()
    lines_before = (
        len(ledger_path.read_text(encoding="utf-8").splitlines())
        if ledger_path.exists()
        else 0
    )

    result = server.cut_out(data_uri, concept)
    print(json.dumps(result, indent=2))

    if ledger_path.exists():
        new_lines = ledger_path.read_text(encoding="utf-8").splitlines()[lines_before:]
        for line in new_lines:
            if line.strip():
                print("ledger receipt:", line)
    return 0 if result.get("status") == "ok" else 1


if __name__ == "__main__":
    raise SystemExit(main())
