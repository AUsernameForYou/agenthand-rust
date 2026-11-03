#!/usr/bin/env python3
"""The dry-run gate: N clean end-to-end runs before filming. Never in CI.

    python scripts/dry_run_gate.py [--photo PATH] [--runs 3]
                                   [--scene "..."] [--ceiling-usd 1.00]

Runs the full chain — identify -> cut_out -> draw — N times against one
photo (default: tests/fixtures/chair_photo.jpg, a generated photo of a
wooden chair in a cluttered garage; see
scripts/generate_chair_fixture.py). The previous default, the verified
run's screenshot, failed on the fixture rather than the chain: a
full-bleed screenshot holds no physical object, so SAM returned
NO_MATCH for every phrase ("smartphone" / "mobile phone" included).
Pass --photo data/assets/e3f0428fbc5e43c7826959a9eecbda50.png to
reproduce that. identify runs as its
own call and is checked; cut_out then runs WITHOUT a passed concept,
so its internal planning step supplies the concept AND the alternates
the bounded mask-recovery loop retries with. (First version of this
gate passed identify's concept in — SAM returned NO_MATCH for the
whole-frame phrases identify favours, and with no alternates the
recovery loop had nothing to retry with. The verified run succeeded
precisely because the internal plan's alternate saved it.) For every run it times
each tool, records the settled cost, surfaces cut_out's mask_review
warnings, and checks the draw receipt hard:

  * changed_opaque_pixels == 0
  * colour profile preserved
  * hashes consistent across identify / cut_out / draw
  * the independent from-disk recount (verify_receipt_from_disk)
    agrees with the receipt

GATE: PASS only if every run is clean. Anything else is FAIL, with the
failing run and check named. Runs use an isolated data dir (a temp
dir; --keep-data-dir DIR to keep it) so the verified store is never
polluted by gate runs. The LEDGER is deliberately NOT isolated: gate
spend lands in the repo's real ledger (AGENTHAND_LEDGER=jsonl:<path>,
the cumulative ledger since fix pass F4), so it counts toward the
same cumulative spend cap as everything else.
Earlier versions pointed the data dir at a temp folder and the ledger
went with it — every gate run started against an empty ledger with a
full cap (architecture Gauntlet fix 3). --keep-data-dir isolates the
ledger too; use it only for offline rehearsal. Per-tool costs are
read as ledger deltas across each call, so they are the amounts the
cap actually counted — including cut_out's internal identify call,
which cut_out's own cost_usd does not include.

This spends REAL Meta credits. The key comes from the repo .env via
agenthand's config — never from an argument, never printed. Before
any call, the script projects the spend (per-run ceiling: identify
$0.0246 x2 — the standalone call plus cut_out's internal planning
call — + SAM $0.0050 + draw $0.0100, the reservation-level highs
seen in the ledger) and HARD-STOPS if the projection exceeds the
ceiling (default $1.00). It also stops between runs if settled spend
crosses the ceiling.
"""

from __future__ import annotations

import argparse
import base64
import json
import mimetypes
import os
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO / "src"))

# Isolated store for gate runs — set BEFORE the tool layer is used.
# (Declared here; main() may replace it with --keep-data-dir.)
_GATE_DIR = Path(tempfile.mkdtemp(prefix="agenthand-gate-"))
os.environ["AGENTHAND_DATA_DIR"] = str(_GATE_DIR)

from agenthand import composite, config, ledger, server  # noqa: E402
from agenthand.assets import AssetStore  # noqa: E402

DEFAULT_PHOTO = REPO / "tests" / "fixtures" / "chair_photo.jpg"
DEFAULT_SCENE = "a regal, warmly lit reading room"
PER_RUN_PROJECTION_USD = 0.0246 * 2 + 0.0050 + 0.0100  # 2x identify + SAM + draw


def _data_uri(photo: Path) -> str:
    mime = mimetypes.guess_type(photo.name)[0] or "image/png"
    return f"data:{mime};base64," + base64.b64encode(photo.read_bytes()).decode()


def _ledger_total() -> float:
    return ledger.ledger_total_usd(config.get_cumulative_ledger_path())


def _run_once(run_no: int, data_uri: str, scene: str, store: AssetStore) -> dict:
    out: dict = {"run": run_no, "ok": False, "problems": [], "timings_s": {}, "costs_usd": {}}

    # Costs are ledger deltas: what the cap actually counted for the
    # step, reservations included on failure — not the tools' own
    # settled-cost fields (cut_out's excludes its internal identify).
    spent_before = _ledger_total()
    t0 = time.perf_counter()
    ident = server.identify(data_uri)
    out["timings_s"]["identify"] = round(time.perf_counter() - t0, 2)
    out["costs_usd"]["identify"] = round(_ledger_total() - spent_before, 6)
    if ident.get("status") != "ok":
        out["problems"].append(f"identify failed: {ident.get('error')}")
        return out
    out["concept"] = ident.get("concept")

    spent_before = _ledger_total()
    t0 = time.perf_counter()
    cut = server.cut_out(ident["image"])
    out["timings_s"]["cut_out"] = round(time.perf_counter() - t0, 2)
    out["costs_usd"]["cut_out"] = round(_ledger_total() - spent_before, 6)
    if cut.get("status") != "ok":
        error = cut.get("error") or {}
        out["problems"].append(f"cut_out failed: {error}")
        return out
    out["cut_out_concept"] = cut.get("concept")
    out["cut_out_attempts"] = cut.get("attempts")
    review = cut.get("mask_review") or {}
    out["mask_review_warnings"] = review.get("warnings", [])
    out["mask_review"] = {
        k: review.get(k)
        for k in ("bbox", "bbox_fill", "touches_frame_edges")
        if k in review
    }

    spent_before = _ledger_total()
    t0 = time.perf_counter()
    draw_out = server.draw(cut["output_asset"], scene)
    out["timings_s"]["draw"] = round(time.perf_counter() - t0, 2)
    draw = draw_out[0] if isinstance(draw_out, list) else draw_out
    out["costs_usd"]["draw"] = round(_ledger_total() - spent_before, 6)
    if draw.get("status") != "ok":
        out["problems"].append(f"draw failed: {draw.get('error')}")
        return out

    receipt = draw["fidelity_receipt"]
    out["receipt"] = {
        "opaque_pixels": receipt["opaque_pixels"],
        "changed_opaque_pixels": receipt["changed_opaque_pixels"],
        "edge_band_pixels": receipt["edge_band_pixels"],
        "color_profile_preserved": receipt["color_profile_preserved"],
    }
    if receipt["changed_opaque_pixels"] != 0:
        out["problems"].append(
            f"receipt changed_opaque_pixels = {receipt['changed_opaque_pixels']}, must be 0"
        )
    if receipt["color_profile_preserved"] is not True:
        out["problems"].append("receipt colour profile not preserved")

    # Hash consistency across the chain.
    cut_hashes = cut.get("hashes", {})
    fg_pixel = (cut_hashes.get("foreground") or {}).get("pixel_sha256")
    if fg_pixel and receipt.get("foreground_pixel_sha256") not in (None, fg_pixel):
        out["problems"].append("foreground pixel hash differs between cut_out and draw receipt")
    if receipt.get("source_asset") != ident["image"]:
        out["problems"].append("receipt source asset is not the identified photo")

    # The independent check: recount the delivered file from disk.
    report = composite.verify_receipt_from_disk(store, draw["output_asset"], receipt)
    out["independent_verify_ok"] = report["ok"]
    if not report["ok"]:
        out["problems"].append(f"independent verify failed: {report['problems']}")

    out["ok"] = not out["problems"]
    return out


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--photo", type=Path, default=DEFAULT_PHOTO)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--scene", default=DEFAULT_SCENE)
    parser.add_argument("--ceiling-usd", type=float, default=1.00)
    parser.add_argument("--keep-data-dir", type=Path, default=None)
    args = parser.parse_args()

    if args.keep_data_dir:
        os.environ["AGENTHAND_DATA_DIR"] = str(args.keep_data_dir)
    elif not os.environ.get("AGENTHAND_LEDGER"):
        # Isolated store, REAL ledger: the gate's asset store lives in
        # the temp dir set at import, but its spend must land in the
        # repo's ledger (the data dir config would use without the
        # gate's override) so the cumulative cap sees it. Since fix
        # pass F4 AGENTHAND_LEDGER_PATH only names a mirror, so the
        # cumulative ledger is moved with AGENTHAND_LEDGER instead.
        os.environ["AGENTHAND_LEDGER"] = "jsonl:" + str(REPO / "data" / "ledger.jsonl")
    print(f"ledger: {config.get_cumulative_ledger_path()}")

    projected = PER_RUN_PROJECTION_USD * args.runs
    print(f"photo: {args.photo}")
    print(f"runs: {args.runs}  scene: {args.scene!r}")
    print(f"projected spend ceiling: ${projected:.4f} (hard stop at ${args.ceiling_usd:.2f})")
    if projected > args.ceiling_usd:
        print("GATE: FAIL — projected spend exceeds the ceiling; no calls made.")
        return 1
    if not config.get_meta_api_key():
        print("GATE: FAIL — no META_API_KEY configured (.env); no calls made.")
        return 1

    store = AssetStore(config.get_data_dir())
    data_uri = _data_uri(args.photo)
    results = []
    total_spent = 0.0
    for run_no in range(1, args.runs + 1):
        if total_spent > args.ceiling_usd:
            print(f"settled spend ${total_spent:.4f} crossed the ceiling; stopping early.")
            break
        result = _run_once(run_no, data_uri, args.scene, store)
        results.append(result)
        total_spent += sum(result["costs_usd"].values())
        print(f"\n--- run {run_no} ---")
        print(json.dumps(result, indent=2, default=str))

    gate_ok = len(results) == args.runs and all(r["ok"] for r in results)
    print("\n== dry-run gate ==")
    for r in results:
        timings = " / ".join(f"{k} {v}s" for k, v in r["timings_s"].items())
        spent = sum(r["costs_usd"].values())
        print(f"run {r['run']}: {'clean' if r['ok'] else 'FAILED'} — {timings} — ${spent:.6f}")
        for warning in r.get("mask_review_warnings", []):
            print(f"  mask_review warning: {warning}")
        for problem in r["problems"]:
            print(f"  problem: {problem}")
    print(f"total spent: ${total_spent:.6f} across {len(results)} run(s)")
    print("GATE:", "PASS" if gate_ok else "FAIL")
    return 0 if gate_ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
