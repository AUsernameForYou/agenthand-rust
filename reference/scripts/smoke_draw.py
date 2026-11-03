#!/usr/bin/env python3
"""Live smoke test for the wired draw tool — run manually, never in CI.

Usage:
    python scripts/smoke_draw.py <photo> "<scene prompt>"
    python scripts/smoke_draw.py --asset asset://<id> "<scene prompt>"

Reads the Meta Model API key from the environment (or a local .env via
agenthand's config) — never from a file argument, never printed.

Photo mode runs the full chain through the MCP tool layer: cut_out
first (its identify planning step runs internally, a separately billed
call, to produce the concept and alternates), then draw on the
resulting foreground handle. Asset mode skips straight to draw with an
existing foreground handle (asset://<id>, kind foreground_png).

Prints each tool's structured result — for draw: the composite handle,
the plate handle, and the fidelity receipt (source / foreground / mask
/ plate hashes, transformations, changed opaque pixel count) — plus
the ledger receipt lines the calls wrote.

This makes REAL billable API calls (muse-image-1.0 is $0.01 per plate;
the photo-mode chain also bills SAM 3.1 at $2.50 per 1,000 images and
the identify plan at token rates). Every call is reserved and settled
through the spend-cap choke point. Do not run it without a key
configured and the credits/cap state understood.
"""

from __future__ import annotations

import base64
import json
import mimetypes
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from agenthand import config, server  # noqa: E402


def _ledger_lines_since(lines_before: int) -> list[str]:
    ledger_path = config.get_ledger_path()
    if not ledger_path.exists():
        return []
    lines = ledger_path.read_text(encoding="utf-8").splitlines()[lines_before:]
    return [line for line in lines if line.strip()]


def _ledger_line_count() -> int:
    ledger_path = config.get_ledger_path()
    if not ledger_path.exists():
        return 0
    return len(ledger_path.read_text(encoding="utf-8").splitlines())


def _print_draw_result(out) -> dict:
    """draw returns [result, preview] on success, an error dict otherwise."""
    result = out[0] if isinstance(out, list) else out
    print(json.dumps(result, indent=2))
    if isinstance(out, list):
        print(f"(plus an image preview of {result.get('output_asset')})")
    return result


def main() -> int:
    args = sys.argv[1:]
    lines_before = _ledger_line_count()

    if len(args) == 3 and args[0] == "--asset":
        foreground_handle, scene_prompt = args[1], args[2]
    elif len(args) == 2 and args[0] != "--asset":
        image_path = Path(args[0])
        scene_prompt = args[1]
        data = image_path.read_bytes()
        mime = mimetypes.guess_type(image_path.name)[0] or "image/png"
        data_uri = f"data:{mime};base64," + base64.b64encode(data).decode()

        cut = server.cut_out(data_uri)
        print(json.dumps(cut, indent=2))
        if cut.get("status") != "ok":
            print("cut_out failed; draw not attempted.")
            for line in _ledger_lines_since(lines_before):
                print("ledger receipt:", line)
            return 1
        foreground_handle = cut["output_asset"]
        print(f"\nforeground: {foreground_handle}\n")
    else:
        print(__doc__)
        return 2

    result = _print_draw_result(server.draw(foreground_handle, scene_prompt))
    for line in _ledger_lines_since(lines_before):
        print("ledger receipt:", line)
    return 0 if result.get("status") == "ok" else 1


if __name__ == "__main__":
    raise SystemExit(main())
