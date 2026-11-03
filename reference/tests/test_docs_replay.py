"""The cached replay and one-click judge path, pinned to frozen data.

The replay is a presentation of the 2026-10-26 run, never a live
call. These tests keep that label attached to the actual ledger,
asset, and receipt records it claims to play back — and keep the
plain-regeneration baseline slot empty until a real baseline run
exists.
"""

import hashlib
import json
import re
import shutil
import subprocess
from pathlib import Path

import pytest
from PIL import Image

from agenthand.assets import pixel_hash_of_image

DOCS = Path(__file__).resolve().parents[1] / "docs"
RECEIPT = json.loads((DOCS / "proof-receipt-v1.json").read_text(encoding="utf-8"))
REPLAY = json.loads((DOCS / "replay-run-v1.json").read_text(encoding="utf-8"))
INDEX = (DOCS / "index.html").read_text(encoding="utf-8")
REPLAY_PAGE = (DOCS / "replay-v1.html").read_text(encoding="utf-8")


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def test_replay_is_labelled_as_a_recording_not_a_live_call():
    assert REPLAY["presentation"] == "REPLAY OF A REAL RUN"
    assert REPLAY["is_live"] is False
    assert "does not call a model or the MCP server" in REPLAY["notice"]
    assert "REPLAY OF A REAL RUN · NOT LIVE" in REPLAY_PAGE
    assert "It does not call the MCP server, a model, or an API" in REPLAY_PAGE
    replay_js = (DOCS / "replay-v1.js").read_text(encoding="utf-8")
    assert "api.meta.ai" not in replay_js
    assert "META_API_KEY" not in replay_js


def test_replay_calls_match_the_receipt_and_frozen_ledger():
    assert [call["tool"] for call in REPLAY["calls"]] == [
        "identify",
        "cut_out",
        "draw",
    ]
    settled_costs = RECEIPT["provenance"]["settled_costs_usd"]
    total = 0.0
    for call in REPLAY["calls"]:
        tool = call["tool"]
        assert call["call_id"] == RECEIPT["provenance"]["call_ids"][tool]
        assert call["settled_cost_usd"] == pytest.approx(settled_costs[tool])
        total += call["settled_cost_usd"]
    assert total == pytest.approx(settled_costs["total"])
    assert REPLAY["run"]["total_settled_cost_usd"] == pytest.approx(
        settled_costs["total"]
    )

    ledger_lines = [
        json.loads(line)
        for line in (DOCS / "replay-ledger-v1.jsonl")
        .read_text(encoding="utf-8")
        .splitlines()
        if line.strip()
    ]
    settled = {
        row["call_id"]: row for row in ledger_lines if row["phase"] == "settled"
    }
    assert len(ledger_lines) == REPLAY["ledger"]["lines"] == 6
    for call in REPLAY["calls"]:
        row = settled[call["call_id"]]
        assert row["tool"] == call["tool"]
        assert row["model"] == call["model"]
        assert row["usd_estimate"] == pytest.approx(call["settled_cost_usd"])
        assert row["input_tokens"] == call["ledger_usage"]["input_tokens"]
        assert row["output_tokens"] == call["ledger_usage"]["output_tokens"]


def test_replay_assets_are_the_frozen_files_served_by_the_site():
    for asset in REPLAY["assets"]:
        path = DOCS / asset["site_file"]
        assert path.exists(), asset["site_file"]
        assert _sha256(path) == asset["sha256"]
        assert path.stat().st_size == asset["byte_size"]
        image = Image.open(path)
        assert image.size == (asset["width"], asset["height"])
        assert pixel_hash_of_image(image) == asset["pixel_sha256"]

    by_kind = {asset["kind"]: asset for asset in REPLAY["assets"]}
    assert by_kind["source_photo"]["sha256"] == RECEIPT["source_sha256"]
    assert by_kind["foreground_png"]["sha256"] == RECEIPT["foreground_sha256"]
    assert by_kind["background_plate"]["sha256"] == _sha256(
        DOCS / "proof-plate-v1.png"
    )
    assert by_kind["composite"]["sha256"] == RECEIPT["output_sha256"]
    assert by_kind["mask_png"]["pixel_sha256"] == (
        "e4f189b470f82430e15cd5060d100e555495c349aad7cfe5e824794955c18748"
    )


def test_replay_receipt_summary_matches_the_published_receipt():
    summary = REPLAY["receipt"]
    for field in (
        "source_asset",
        "foreground_asset",
        "mask_asset",
        "plate_asset",
        "output_asset",
        "opaque_pixels",
        "changed_opaque_pixels",
        "color_profile_preserved",
        "edge_band_pixels",
    ):
        assert summary[field] == RECEIPT[field]


def test_missing_payloads_are_disclosed_not_reconstructed():
    assert "Standalone identify JSON output" in REPLAY["missing_frozen_payloads"]
    assert "Exact draw scene prompt text" in REPLAY["missing_frozen_payloads"]
    identify = REPLAY["calls"][0]
    draw = REPLAY["calls"][2]
    assert identify["output"]["frozen_payload"] is None
    assert draw["input"]["scene_prompt"]["frozen_payload"] is None
    assert "does not invent or reconstruct one" in REPLAY_PAGE
    assert "its exact text was not frozen" in REPLAY_PAGE


def test_baseline_slot_stays_empty_without_a_baseline_run():
    baseline = REPLAY["baseline"]
    assert baseline["status"] == "not_measured"
    assert baseline["changed_opaque_pixels"] is None
    assert "TODO(baseline-regeneration)" in baseline["todo"]
    for page in (INDEX, REPLAY_PAGE):
        assert 'data-baseline-status="not-measured"' in page
        assert "NOT MEASURED" in page
        assert "TODO(baseline-regeneration)" in page


def test_one_click_judge_path_preloads_the_frozen_sample():
    verify_block = INDEX[
        INDEX.index('<div class="verify"') : INDEX.index('<div class="opencode">')
    ]
    assert "ONE-CLICK JUDGE PATH — FROZEN SAMPLE PRELOADED" in verify_block
    assert "No upload and no file picker" in verify_block
    for filename in (
        "proof-source-v1.png",
        "proof-mask-v1.png",
        "proof-layer-v1.png",
        "proof-output-v1.png",
    ):
        assert f'src="{filename}"' in verify_block
    assert '<input' not in verify_block
    assert "Verify frozen sample" in verify_block
    assert "Run one-pixel tamper check" in verify_block
    assert 'href="replay-v1.html"' in verify_block


def test_changed_site_assets_use_versioned_filenames():
    for page in (INDEX, REPLAY_PAGE):
        for ref in re.findall(r'<script[^>]+src="([^"]+)"', page):
            if ref.startswith("http"):
                continue
            assert re.search(r"-v\d+\.js$", ref), ref
        for ref in re.findall(r'<link[^>]+href="([^"]+\.css)"', page):
            assert re.search(r"-v\d+\.css$", ref), ref
    assert 'src="replay-v1.js"' in REPLAY_PAGE
    assert 'href="replay-v1.css"' in REPLAY_PAGE
    assert 'href="replay-run-v1.json"' in REPLAY_PAGE
    assert 'href="replay-ledger-v1.jsonl"' in REPLAY_PAGE


@pytest.mark.skipif(shutil.which("node") is None, reason="node not available")
def test_replay_helpers_under_node(tmp_path):
    script = tmp_path / "check-replay.js"
    script.write_text(
        """
const fs = require("fs");
const replay = require(process.argv[2]);
const data = JSON.parse(fs.readFileSync(process.argv[3], "utf8"));
const summary = replay.summarizeReplay(data);
if (summary.callCount !== 3) throw new Error("call count");
if (summary.tools.join(",") !== "identify,cut_out,draw") throw new Error("tools");
if (Math.abs(summary.totalSettledCostUsd - 0.020044) > 0.0000005)
  throw new Error("cost total");
if (summary.isLive !== false) throw new Error("live flag");
if (!replay.replayStepLabel(data, 1).includes("CUT_OUT"))
  throw new Error("step label");
const doctored = Object.assign({}, data, { is_live: true });
let threw = false;
try { replay.summarizeReplay(doctored); } catch (e) { threw = true; }
if (!threw) throw new Error("live-labelled data accepted");
console.log("node replay helpers ok");
""",
        encoding="utf-8",
    )
    result = subprocess.run(
        [
            "node",
            str(script),
            str(DOCS / "replay-v1.js"),
            str(DOCS / "replay-run-v1.json"),
        ],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert result.returncode == 0, result.stderr
    assert "node replay helpers ok" in result.stdout
