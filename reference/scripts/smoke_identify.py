#!/usr/bin/env python3
"""Live smoke test for the wired identify tool — run manually, never in CI.

Usage:
    META_API_KEY=... .venv/bin/python scripts/smoke_identify.py <image-path>

Reads the Meta Model API key from the environment (or a local .env via
agenthand's config) — never from a file argument, never printed. Runs
one real identify call through the MCP tool layer, prints the
structured result, and prints the ledger receipt lines the call wrote.

This makes a REAL billable API call (a few tenths of a cent at most,
reserved and settled through the spend-cap choke point). Do not run it
without a key configured and the credits/cap state understood.
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
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    image_path = Path(sys.argv[1])
    data = image_path.read_bytes()
    mime = mimetypes.guess_type(image_path.name)[0] or "image/png"
    data_uri = f"data:{mime};base64," + base64.b64encode(data).decode()

    ledger_path = config.get_ledger_path()
    lines_before = (
        len(ledger_path.read_text(encoding="utf-8").splitlines())
        if ledger_path.exists()
        else 0
    )

    result = server.identify(data_uri)
    print(json.dumps(result, indent=2))

    if ledger_path.exists():
        new_lines = ledger_path.read_text(encoding="utf-8").splitlines()[lines_before:]
        for line in new_lines:
            if line.strip():
                print("ledger receipt:", line)
    return 0 if result.get("status") == "ok" else 1


if __name__ == "__main__":
    raise SystemExit(main())
