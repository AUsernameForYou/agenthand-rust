#!/usr/bin/env python3
"""Deliberately regenerate the schema-4 sample package and verify it.

Why this exists (2026-11-01): the delivered Python schema-4 receipt was
stamped by tests/test_paperfill.py::test_draw_with_paper_fill_receipt_verifies
into a pytest tmp dir. The tmp dir was cleaned, the receipt's seven layer
files vanished with it, and the set had to be regenerated under deadline
pressure. It worked because the test case is deterministic, but no
deliverable should live only in a tmp dir again.

This script is the deliberate path:
  1. Runs the one test with --basetemp on durable disk (~/workspace/.tmp),
     never the 512 MB /tmp tmpfs.
  2. Finds the produced files BY HASH: every candidate PNG in the basetemp
     tree is sha256'd and matched against the pins in the delivered
     receipt (schema4-receipt-python.json in this folder).
  3. Copies the seven layers into this folder under the delivered asset
     IDs and writes REGENERATED-CHECK.json with the verdict.
  4. Exits nonzero and copies NOTHING if any pin fails to match.

Run:  python3 regenerate.py [--out DIR]   (default out: this folder)
"""
import argparse
import hashlib
import json
import shutil
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = Path.home() / "workspace" / "agenthand"
PY = REPO / ".venv" / "bin" / "python"
BASETEMP = Path.home() / "workspace" / ".tmp" / "schema4-basetemp"
TEST = "tests/test_paperfill.py::test_draw_with_paper_fill_receipt_verifies"

# delivered asset ID -> receipt field pinning that layer's file sha256
LAYERS = {
    "8fba80749ef24df6aea1d2785de7ae99": "source_sha256",
    "bbd01e13dc7747ec89d239a4d99ae706": "mask_sha256",
    "28ecca568ca34b44a400f159a417ade8": "foreground_sha256",
    "72a8160bbf9345f1a9acd987a6f2cddf": "plate_sha256",
    "6fedbf4929624a91975c28f751374106": "reconstruction/layer_sha256",
    "c061a600cb3a4bb68a983df9d69728a3": "reconstruction/region_map_sha256",
    "2b6419a3f3d042ecb1e0fd6136df0db2": "output_sha256",
}


def pin(receipt, field):
    node = receipt
    for part in field.split("/"):
        node = node[part]
    return node


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default=str(HERE))
    args = ap.parse_args()
    out = Path(args.out)
    receipt = json.loads((HERE / "schema4-receipt-python.json").read_text())
    pins = {aid: pin(receipt, f) for aid, f in LAYERS.items()}

    # sanity: the delivered package in --out must already verify, so a bad
    # regeneration can never silently replace good files.
    present = {}
    for aid, want in pins.items():
        p = out / f"{aid}.png"
        if p.exists():
            present[aid] = sha256(p) == want
    if present and not all(present.values()):
        print("ABORT: existing package files fail their pins:", present)
        return 2

    if BASETEMP.exists():
        shutil.rmtree(BASETEMP)
    BASETEMP.mkdir(parents=True)
    run = subprocess.run(
        [str(PY), "-m", "pytest", TEST, "-q", f"--basetemp={BASETEMP}"],
        cwd=REPO, capture_output=True, text=True)
    print(run.stdout[-1500:])
    if run.returncode != 0:
        print("ABORT: test run failed\n", run.stderr[-1500:])
        return 1

    by_hash = {}
    for p in BASETEMP.rglob("*.png"):
        by_hash.setdefault(sha256(p), p)
    missing = [aid for aid, want in pins.items() if want not in by_hash]
    if missing:
        print("ABORT: regenerated files do not match pins for:", missing)
        return 1

    out.mkdir(parents=True, exist_ok=True)
    for aid, want in pins.items():
        shutil.copyfile(by_hash[want], out / f"{aid}.png")
    check = {aid: sha256(out / f"{aid}.png") == want for aid, want in pins.items()}
    (out / "REGENERATED-CHECK.json").write_text(json.dumps(
        {"all_match": all(check.values()), "per_layer": check}, indent=1))
    print("REGENERATION VERIFIED:", check)
    return 0 if all(check.values()) else 1


if __name__ == "__main__":
    sys.exit(main())
