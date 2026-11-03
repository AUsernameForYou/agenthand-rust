#!/usr/bin/env python3
"""Build evidence.db from verified-runs-inventory.json + schema.sql.

Reference implementation (stdlib only). The shipped interface will be the
Rust binary's rusqlite `agenthand evidence` subcommand; this script exists so
the database is reproducible from the inventory by anyone, anywhere:

    python3 import_inventory.py [--inventory verified-runs-inventory.json]
                                [--schema schema.sql] [--db evidence.db]

Fidelity rules (from EVIDENCE-LAYER.md):
  * Inventory nulls import as NULL. Nothing is invented, ever.
  * UNSOURCED / MISMATCH flags are preserved verbatim in runs.notes, so both
    sides of a mismatch stay visible next to the figures they qualify.
  * Graph edges are DERIVED from the foreign keys after the base tables load.
"""

import argparse
import hashlib
import json
import os
import sqlite3
import sys

HERE = os.path.dirname(os.path.abspath(__file__))

# Video chapters of the demo film (BUILD-REPORT-v10.2 chapter map).
CHAPTERS = [(1, 0.0, 14.93, 'Ch0 baseline: suitcase still, Muse Image edit vs agenthand 0 changed', 5), (2, 14.93, 37.08, 'Ch1 switchboard: yard, phone, board, run-1443-v102 results', None), (3, 37.08, 51.85, 'Ch2 hero: run-1443-v102, 5,072,763 product pixels checked, 0 changed', 1), (4, 51.85, 66.62, 'Ch3a buyer/seller: verifier PASS, deliberate tamper FAIL', None), (5, 66.62, 81.39, 'Ch3b fence bid packet: card printed width 10 in, CARD_SOFT on, no size number', None), (6, 81.39, 96.16, 'Ch4 moving-day narration: what was said / what the run returned', None), (7, 96.16, 110.93, 'Ch5 open-house fill: 1,128,708 placed pixels, 0 changed; QR proof run-1443-fixed', 4), (8, 110.93, 125.7, 'Ch6 walkaround and dashboard counters', None), (9, 125.7, 140.47, 'Ch7 under the hood: Python/Rust parity, 580,613 checked, 0 changed, $0.020044', 2), (10, 140.47, 155.24, 'Ch8 real screen recording 19:06 take sped up 20x; final card run-1443-v102, not the recorded take', 3), (11, 155.24, 170.35, 'Ch9 cast and prints', None), (12, 170.35, 179.0, 'ChClose lockup, recorded-run call times (wall time per call, client-printed, 19:06 recorded take: 26.9 / 31.9 / 44.7 s), total Meta spend $2.93 settled Nov 1', None)]

TOOLS = (  # inventory field prefixes, in chain order
    ("identify", "identify_model", "identify_cost_usd", None, None),
    ("cut_out", "cut_out_model", "cut_out_cost_usd", "cut_out_mask_px", None),
    ("draw", "draw_model", "draw_cost_usd", None, "draw_mode"),
)

# Exact re-check commands documented in a receipt's own package files.
# Keys are inventory row indexes. A receipt is listed only when both the
# command and its documented success line appear in that package; all other
# receipts keep NULL rather than receiving an inferred command.
RECEIPT_CHECKS = {
    0: (
        "env -u META_API_KEY python3 ~/workspace/agenthand/scripts/replay_verified_run.py",
        "REPLAY: PASS",
    ),
    1: (
        "agenthand-0.3.0-macos-arm64 verify source.png staged.png mask.png",
        "verify PASS : checked 3,422,482 px, changed 0 px, ok=True",
    ),
    2: (
        "agenthand-0.3.0-macos-arm64 verify source.png staged.png mask.png",
        "verify PASS : checked 5,654,781 px, changed 0 px, ok=True",
    ),
    5: (
        "agenthand verify source.png staged.png mask.png",
        "verify PASS : checked 5,072,763 px, changed 0 px, ok=True",
    ),
}

# The recorded take documents its checker output but not the command used
# to produce it, so only the expected line is carried.
RECEIPT_EXPECTED_LINES = {
    4: "verify PASS : checked 5,659,218 px, changed 0 px, ok=True",
}


def resolve_source_file(row):
    """Receipt file path as stored in receipts.source_file.

    The inventory gives either a meta-hackathon-root-relative path, an
    absolute path, or (run-1443-fixed) a bare filename inside the package.
    """
    rf = row.get("receipt_file")
    if not rf:
        return None
    if os.path.isabs(rf) or "/" in rf:
        return rf
    pkg = (row.get("package_path") or "").split(" (")[0].rstrip("/")
    return (pkg + "/" + rf) if pkg else rf


def receipt_kind(row):
    label = row.get("label") or ""
    if "KEEP record" in label and row.get("receipt_checked_px") is None:
        return "keep_record"  # ownership record; carries no pixel counts
    if not row.get("receipt_file"):
        if "recount" in (row.get("source") or ""):
            return "recount"  # figures from an independent recount file
        return "fidelity_receipt"  # receipt exists but is not in the local package
    return "fidelity_receipt"


def has_receipt(row):
    return bool(
        row.get("receipt_file")
        or row.get("receipt_checked_px") is not None
        or row.get("receipt_changed_px") is not None
        or row.get("receipt_sha256")
    )


def row_notes(row):
    parts = []
    if row.get("flags"):
        parts.append("; ".join(row["flags"]))
    if row.get("notes"):
        parts.append(row["notes"])
    return "\n".join(parts) or None


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--inventory", default=os.path.join(HERE, "verified-runs-inventory.json"))
    ap.add_argument("--schema", default=os.path.join(HERE, "schema.sql"))
    ap.add_argument("--db", default=os.path.join(HERE, "evidence.db"))
    args = ap.parse_args()

    with open(args.inventory) as f:
        inventory = json.load(f)
    rows = inventory["rows"]
    with open(args.schema) as f:
        schema = f.read()

    if os.path.exists(args.db):
        os.remove(args.db)
    con = sqlite3.connect(args.db)
    con.executescript(schema)
    cur = con.cursor()

    run_ids = {}    # inventory index -> run_id
    receipt_ids = {}  # inventory index -> receipt_id

    for idx, row in enumerate(rows):
        cur.execute(
            """INSERT INTO runs (label, run_date, build_hash, item, package_path,
                                 total_settled_usd, verify_result, verify_by, source, notes)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
            (
                row.get("label"),
                row.get("date"),
                row.get("build"),
                row.get("item"),
                row.get("package_path"),
                row.get("total_settled_usd"),
                row.get("verify_result"),
                row.get("verify_by"),
                row.get("source"),
                row_notes(row),
            ),
        )
        run_id = cur.lastrowid
        run_ids[idx] = run_id

        for tool, f_model, f_cost, f_mask, f_mode in TOOLS:
            model = row.get(f_model)
            cost = row.get(f_cost)
            mask = row.get(f_mask) if f_mask else None
            mode = row.get(f_mode) if f_mode else None
            if model is None and cost is None and mask is None and mode is None:
                continue  # the package records no such call for this run
            cur.execute(
                """INSERT INTO calls (run_id, tool, model, settled_usd, duration_s, mask_px, mode)
                   VALUES (?, ?, ?, ?, NULL, ?, ?)""",
                (run_id, tool, model, cost, mask, mode),
            )

        if has_receipt(row):
            verify_cmd, expected_line = RECEIPT_CHECKS.get(
                idx, (None, RECEIPT_EXPECTED_LINES.get(idx))
            )
            cur.execute(
                """INSERT INTO receipts
                       (run_id, kind, checked_px, changed_px, file_sha256,
                        source_file, verify_cmd, expected_line)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?)""",
                (
                    run_id,
                    receipt_kind(row),
                    row.get("receipt_checked_px"),
                    row.get("receipt_changed_px"),
                    row.get("receipt_sha256") or None,
                    resolve_source_file(row),
                    verify_cmd,
                    expected_line,
                ),
            )
            receipt_ids[idx] = cur.lastrowid

    # --- Claims seed -------------------------------------------------------
    # Seed only figures carried by verified-runs-inventory.json or by
    # film-assets/NUMBERS-CHECK.md. Scope is stored as fields so figures
    # from different photos, builds, or dates can never merge into one
    # apparently shared number.
    def row_by_label(label):
        return next((i for i, r in enumerate(rows) if r.get("label") == label), None)

    hero_idx = row_by_label("Hero run IMG_1443 (final Rust master, hole-carve)")
    parity_idx = row_by_label("Frozen offline replay (live run frozen 2026-10-26)")
    take_idx = row_by_label("Recorded hero take (screen recording, build 380eff0)")
    fill_idx = row_by_label("P13 open-house fill run (two placed items on IMG_1408 canvas)")
    suitcase_idx = row_by_label("Suitcase still chain (Oct 28 room run)")

    required = {
        "hero": hero_idx,
        "parity": parity_idx,
        "take": take_idx,
        "fill": fill_idx,
        "suitcase": suitcase_idx,
    }
    assert all(v is not None for v in required.values()), \
        f"inventory labels changed; missing claim-seed rows: {required}"

    hero = rows[hero_idx]
    parity = rows[parity_idx]
    take = rows[take_idx]
    fill = rows[fill_idx]
    suitcase = rows[suitcase_idx]

    assert (hero.get("receipt_checked_px"), hero.get("receipt_changed_px")) == (5072763, 0)
    assert (parity.get("receipt_checked_px"), parity.get("receipt_changed_px"),
            parity.get("total_settled_usd")) == (580613, 0, 0.020044)
    assert (take.get("receipt_checked_px"), take.get("receipt_changed_px"),
            take.get("total_settled_usd")) == (5659218, 0, 0.017611)
    assert (fill.get("receipt_checked_px"), fill.get("receipt_changed_px")) == (1128708, 0)
    assert (suitcase.get("receipt_checked_px"), suitcase.get("receipt_changed_px")) == (223600, 0)

    claim_specs = [
        {
            "text": ("Fidelity Lock receipt for the hero bike run (IMG_1443, build 166ea6b): "
                     "5,072,763 pixels checked, 0 changed."),
            "status": "verified",
            "scope_photo": "IMG_1443",
            "scope_build": hero.get("build"),
            "scope_date": hero.get("date"),
            "reason": ("Scope is this run only; its package note records a separate Mac run "
                       "at 5,072,730 checked px (33 px SAM variance)."),
            "source_url": hero.get("receipt_file"),
            "criterion": "Technical Depth & Execution",
            "row_idx": hero_idx,
        },
        {
            "text": ("ch7-parity: Frozen replay run: 580,613 pixels checked, 0 changed; "
                     "settled cost $0.020044."),
            "status": "verified",
            "scope_photo": "Frozen replay source photo (unnamed in the package)",
            "scope_build": parity.get("build"),
            "scope_date": parity.get("date"),
            "reason": ("The replay package does not state an item name or per-tool model "
                       "names; its scope is the frozen replay package and receipt."),
            "source_url": parity.get("receipt_file"),
            "criterion": "Technical Depth & Execution",
            "row_idx": parity_idx,
        },
        {
            "text": ("ch8-take: Recorded hero take: 5,659,218 pixels checked, 0 changed; "
                     "settled cost $0.017611."),
            "status": "verified",
            "scope_photo": "IMG_1443",
            "scope_build": take.get("build"),
            "scope_date": take.get("date"),
            "reason": 'Wall time per call, client-printed, 19:06 recorded take: identify 26.9 s / cut_out 31.9 s / draw 44.7 s, from session-transcript-1906-recorded-take.txt in film-assets/P08-RUN-RECORDING (Mac run dir take-1443-20261101-190623). The 28.3 / 34.0 / 49.0 s set is the separate 18:00 acceptance run take-rust-180027 on 380eff0, NOT recorded; same wall-time measurement, different run. Receipt figures sourced from take-note.txt.',
            "source_url": 'film-assets/P08-RUN-RECORDING/session-transcript-1906-recorded-take.txt',
            "criterion": "Demo",
            "row_idx": take_idx,
        },
        {
            "text": ("ch5-fill: P13 open-house fill placed 1,128,708 opaque pixels, "
                     "0 changed."),
            "status": "verified",
            "scope_photo": "IMG_1408 canvas",
            "scope_build": fill.get("build"),
            "scope_date": fill.get("date"),
            "reason": ("Placed-pixel recount (slot A 823,277 plus slot B 305,431), "
                       "not a draw receipt count."),
            "source_url": "film-assets/P13/recount.json",
            "criterion": "Demo",
            "row_idx": fill_idx,
        },
        {
            "text": ("ch0-baseline: On the orange hard-shell suitcase still, a Muse Image "
                     "full-frame edit changed 223,599-223,600 of 223,600 product pixels "
                     "across 3 runs; agenthand's Fidelity Lock composite changed 0."),
            "status": "verified",
            "scope_photo": "Orange hard-shell suitcase still (film-assets/P23-SUITCASE-STILL/)",
            "scope_build": suitcase.get("build"),
            "scope_date": suitcase.get("date"),
            "reason": ("The contrast figure is sourced from NUMBERS-CHECK.md (L29); the "
                       "agenthand side is the Oct 28 suitcase receipt mapped here."),
            "source_url": "film-assets/NUMBERS-CHECK.md",
            "criterion": "Use of Meta AI Tooling",
            "row_idx": suitcase_idx,
        },
        {
            "text": ("The estimator measured a fence-height extent of ~127.2 cm "
                     "on IMG_1403."),
            "status": "withdrawn",
            "scope_photo": "IMG_1403",
            "scope_build": "pre-hardening estimator",
            "scope_date": "2026-11-01",
            "reason": ("False accept: the card fit was not on the printed card (the card "
                       "is ~25 px wide in the 640x480 preview); caught by relay C-9, "
                       "with evidence overlay evidence-1403.jpg."),
            "source_url": "film-assets/P10/fence-length/",
            "criterion": "Technical Depth & Execution",
            "row_idx": None,
        },
        {
            "text": ("estimator-refusal-1403: The hardened estimator refuses IMG_1403 "
                     "under strict tick verification."),
            "status": "refused",
            "scope_photo": "IMG_1403",
            "scope_build": "Python main c5f5929",
            "scope_date": "2026-11-02",
            "reason": ("The phantom fence-board fit's ticks vanish under strict matching "
                       "at its own homography; the fixtures pin the IMG_1403 and "
                       "IMG_1405 refusals and the IMG_1491 acceptance."),
            "source_url": "evidence-layer/measure-hardening-fixtures.py",
            "criterion": "Technical Depth & Execution",
            "row_idx": None,
        },
        {
            "text": ("f5-overcap: An over-cap settle is recorded over_cap, the call "
                     "fails loudly, and later calls refuse until the cap is raised."),
            "status": "refused",
            "scope_photo": "not photo-scoped (billing ledger behavior)",
            "scope_build": "Python main 075bb39; Rust 166ea6b",
            "scope_date": "2026-11-02",
            "reason": ("Backing tests: Python "
                       "test_settle_over_the_cap_is_recorded_and_fails_loudly; Rust "
                       "settle_over_the_cap_is_recorded_and_fails_loudly_in_both_stores. "
                       "Both assert over_cap recording and refusal of the next reservation."),
            "source_url": ("~/workspace/agenthand/tests/test_meta_client.py::"
                           "test_settle_over_the_cap_is_recorded_and_fails_loudly; "
                           "~/workspace/meta-hackathon/fixpass-verify2/rust-166ea6b/tests/"
                           "billing_cap.rs::"
                           "settle_over_the_cap_is_recorded_and_fails_loudly_in_both_stores"),
            "criterion": "Technical Depth & Execution",
            "row_idx": None,
        },
    ]

    claims_seeded = 0
    for spec in claim_specs:
        cur.execute(
            """INSERT INTO claims
                   (text, status, scope_photo, scope_build, scope_date, reason, source_url)
               VALUES (?, ?, ?, ?, ?, ?, ?)""",
            (
                spec["text"], spec["status"], spec["scope_photo"], spec["scope_build"],
                spec["scope_date"], spec["reason"], spec["source_url"],
            ),
        )
        claim_id = cur.lastrowid
        row_idx = spec["row_idx"]
        cur.execute(
            """INSERT INTO evidence_map (claim_id, rubric_criterion, run_id, receipt_id)
               VALUES (?, ?, ?, ?)""",
            (
                claim_id,
                spec["criterion"],
                run_ids.get(row_idx) if row_idx is not None else None,
                receipt_ids.get(row_idx) if row_idx is not None else None,
            ),
        )
        claims_seeded += 1

    # --- Video chapters seed -------------------------------------------------
    # Seeded from BUILD-REPORT-v10.2 (the v10.2 chapter map, contiguous
    # 0.0-179.0 s). The supports edges derive from these rows below.
    for ch_id, t0, t1, line, claim in CHAPTERS:
        cur.execute(
            "INSERT INTO video_chapters (chapter_id, t_start_s, t_end_s, narration_line, claim_id) "
            "VALUES (?, ?, ?, ?, ?)",
            (ch_id, t0, t1, line, claim),
        )

    # --- Receipt source paths, repo-relative --------------------------------
    # File-backed receipts ship in the repo at evidence/receipts/<id>.json
    # (finding 1). The inventory's scattered working paths do not ship.
    cur.execute(
        "UPDATE receipts SET source_file = 'evidence/receipts/' || receipt_id || '.json' "
        "WHERE file_sha256 IS NOT NULL"
    )

    # --- Receipts root ------------------------------------------------------
    # Construction (also stated in schema.sql): sort every rooted receipt's
    # sha256 hex digest ascending, concatenate the UTF-8 hex strings with
    # no separators, and store sha256 of that concatenation.
    rooted_hashes = sorted(
        r[0] for r in cur.execute(
            "SELECT file_sha256 FROM receipts WHERE file_sha256 IS NOT NULL"
        )
    )
    receipts_root = hashlib.sha256("".join(rooted_hashes).encode("utf-8")).hexdigest()
    cur.execute(
        "INSERT INTO meta (key, value) VALUES ('receipts_root', ?)",
        (receipts_root,),
    )
    cur.execute(
        "INSERT INTO meta (key, value) VALUES ('receipt_count_rooted', ?)",
        (str(len(rooted_hashes)),),
    )

    # --- Graph edges, derived from the foreign keys ------------------------
    edge_sql = [
        ("INSERT INTO edges (from_node, to_node, relation, source) "
         "SELECT 'claim:' || claim_id, 'run:' || run_id, 'evidenced_by', 'evidence_map.run_id' "
         "FROM evidence_map WHERE run_id IS NOT NULL"),
        ("INSERT INTO edges (from_node, to_node, relation, source) "
         "SELECT 'claim:' || claim_id, 'receipt:' || receipt_id, 'evidenced_by', 'evidence_map.receipt_id' "
         "FROM evidence_map WHERE receipt_id IS NOT NULL"),
        ("INSERT INTO edges (from_node, to_node, relation, source) "
         "SELECT 'claim:' || claim_id, 'criterion:' || rubric_criterion, 'addresses', 'evidence_map.rubric_criterion' "
         "FROM evidence_map"),
        ("INSERT INTO edges (from_node, to_node, relation, source) "
         "SELECT 'run:' || run_id, 'call:' || call_id, 'has_call', 'calls.run_id' FROM calls"),
        ("INSERT INTO edges (from_node, to_node, relation, source) "
         "SELECT 'run:' || run_id, 'receipt:' || receipt_id, 'has_receipt', 'receipts.run_id' FROM receipts"),
        ("INSERT INTO edges (from_node, to_node, relation, source) "
         "SELECT 'call:' || call_id, 'model:' || model, 'uses_model', 'calls.model' "
         "FROM calls WHERE model IS NOT NULL"),
        ("INSERT INTO edges (from_node, to_node, relation, source) "
         "SELECT 'run:' || run_id, 'build:' || build_hash, 'built_with', 'runs.build_hash' "
         "FROM runs WHERE build_hash IS NOT NULL AND build_hash <> 'unknown'"),
        ("INSERT INTO edges (from_node, to_node, relation, source) "
         "SELECT 'chapter:' || chapter_id, 'claim:' || claim_id, 'supports', 'video_chapters.claim_id' "
         "FROM video_chapters WHERE claim_id IS NOT NULL"),
    ]
    for sql in edge_sql:
        cur.execute(sql)

    con.commit()

    # --- Summary ------------------------------------------------------------
    def count(table):
        return cur.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0]

    print(f"imported {args.inventory} -> {args.db}")
    for table in ("runs", "calls", "receipts", "claims", "evidence_map", "video_chapters", "meta", "edges"):
        print(f"  {table:<14} {count(table):>4} rows")
    print(f"  claims seeded: {claims_seeded}")
    print(f"  receipts_root: {receipts_root} ({len(rooted_hashes)} rooted receipts)")
    node_rows = cur.execute(
        "SELECT node_type, COUNT(*) FROM nodes GROUP BY node_type ORDER BY node_type"
    ).fetchall()
    print("  nodes (view):  " + ", ".join(f"{t}={n}" for t, n in node_rows))
    edge_rows = cur.execute(
        "SELECT relation, COUNT(*) FROM edges GROUP BY relation ORDER BY relation"
    ).fetchall()
    print("  edges by relation: " + ", ".join(f"{r}={n}" for r, n in edge_rows))
    con.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
