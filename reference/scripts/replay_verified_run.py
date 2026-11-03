#!/usr/bin/env python3
"""Replay the frozen verified run — offline, no API key, no network.

    python scripts/replay_verified_run.py [--package DIR]

The package (default: ~/workspace/meta-hackathon/replay-package) freezes
the verified 2026-10-26 end-to-end run: its five assets, its ledger
lines, and its fidelity receipt. This script proves the freeze is
honest, on any machine, with zero network:

1. Every frozen file must match the package manifest (sha256).
2. The asset store over the frozen files must pass integrity
   (file bytes, decoded pixels, colour profile) — checked inside
   verify_receipt_from_disk.
3. composite.verify_receipt_from_disk re-derives the receipt from the
   files on disk: hashes, a full recount of changed opaque pixels,
   and colour-profile identity.
4. agenthand.verify.verify_files independently checks the exported
   source / composite / mask files with no store involved.

Prints the fidelity counts and PASS or FAIL. Exit 0 = PASS.

This is a REPLAY of a real recorded run, never a fresh live call — see
the package README for when to cut to it and how it is labelled.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from agenthand import verify as standalone_verify  # noqa: E402
from agenthand.assets import AssetStore  # noqa: E402
from agenthand.composite import verify_receipt_from_disk  # noqa: E402

DEFAULT_PACKAGE = Path.home() / "workspace" / "meta-hackathon" / "replay-package"


def _check_manifest(package: Path) -> list[str]:
    manifest = json.loads((package / "manifest.json").read_text(encoding="utf-8"))
    problems = []
    for rel, expected in manifest.items():
        path = package / rel
        if not path.exists():
            problems.append(f"missing frozen file: {rel}")
            continue
        actual = hashlib.sha256(path.read_bytes()).hexdigest()
        if actual != expected:
            problems.append(f"manifest mismatch: {rel}")
    return problems


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--package",
        type=Path,
        default=DEFAULT_PACKAGE,
        help=f"replay package directory (default: {DEFAULT_PACKAGE})",
    )
    args = parser.parse_args()
    package = args.package

    receipt = json.loads((package / "receipt.json").read_text(encoding="utf-8"))
    problems: list[str] = []

    print("== manifest ==")
    manifest_problems = _check_manifest(package)
    problems += manifest_problems
    print("frozen files match manifest:", not manifest_problems)

    print("== independent verify from disk (asset store) ==")
    store = AssetStore(package / "run")
    report = verify_receipt_from_disk(store, receipt["output_asset"], receipt)
    for name, ok in report["checks"].items():
        print(f"  {'ok ' if ok else 'FAIL'} {name}")
    problems += report["problems"]
    recomputed = report["recomputed"]

    print("== standalone verify (exported files, no store) ==")
    assets_dir = package / "run" / "assets"
    file_report = standalone_verify.verify_files(
        assets_dir / "e3f0428fbc5e43c7826959a9eecbda50.png",
        assets_dir / "45deaf1acea14ab5839c641eee1cd277.png",
        assets_dir / "7dc19362be4c47abadac2edd7e69129e.png",
    )
    print(
        f"  checked {file_report['checked_pixels']:,} fully-masked pixels, "
        f"changed {file_report['changed_pixels']}, "
        f"edge band {file_report['edge_band_pixels']}, "
        f"profile preserved: {file_report['color_profile_preserved']}"
    )
    if not file_report["ok"]:
        problems += file_report["problems"]

    print("== fidelity counts (recomputed from the frozen files) ==")
    print(f"  opaque pixels checked : {recomputed['opaque_pixels']:,}")
    print(f"  changed opaque pixels : {recomputed['changed_opaque_pixels']}")
    print(f"  edge band pixels      : {recomputed['edge_band_pixels']:,}")
    print(f"  colour profile kept   : {recomputed['color_profile_preserved']}")

    ok = not problems and report["ok"] and file_report["ok"]
    if problems:
        print("problems:")
        for problem in problems:
            print("  -", problem)
    print("REPLAY:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
