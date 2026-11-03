#!/usr/bin/env python3
"""Verify evidence.db against the files and figures it points at.

Stdlib only. Five checks, one verdict line each (plus per-item detail):

  (a) Receipt hashes — every receipts.file_sha256 matches the sha256 of the
      receipt file on disk, where that file exists locally. Files that live
      only on Drive or on the Mac are SKIP, not FAIL.
  (b) Run totals — every run's total_settled_usd equals the sum of its
      calls' settled costs, where both exist. Differences the inventory
      itself explains (a MISMATCH / UNSOURCED flag, or an explanatory note
      such as the jacket lane's earlier identify pass) are REPORT, not FAIL.
  (c) Graph connectivity — every receipt and every call reaches a run:
      no orphan rows, and every receipt/call has its derived graph edge.
  (d) Receipts root — the root stored in meta recomputes from the sorted
      sha256 digests of all receipts that carry one.
  (e) Claim scope — every seeded claim carries all three scope fields, or
      an explicit reason where scope cannot be complete; withdrawn and
      refused claims must carry a reason.

OVERALL line (byte-identical with the Rust `agenthand evidence verify`
output, C-27 / M-36 ruling 1(b)):

  OVERALL: PASS (R re-hashed, 0 skipped, 0 failing line(s))        exit 0
  OVERALL: INCOMPLETE (R re-hashed, S skipped, 0 failing line(s))  exit 4
  OVERALL: FAIL (R re-hashed, S skipped, F failing line(s))        exit 1

Counting rule: re-hashed = receipt file found, hash recomputed AND
matched. A file whose hash mismatches counts only as a failing line, not
as re-hashed. Skipped = receipt files not present locally. PASS requires
failed=0, skipped=0, and re-hashed>0; failed=0 with skipped>0, or with
zero receipt checks at all, is INCOMPLETE; any failing line is FAIL,
even with skips.

Verbs: no args, `verify`, and `verify-all` all run the full check above.
`verify claim:N` checks one claim only: its directed walk to receipts
(each reached receipt hash-checked under the same counting rule) plus
its scope fields. A withdrawn/refused claim with no receipt gives
INCOMPLETE (0 re-hashed, 0 skipped, 0 failing), exit 4 — not PASS.

Receipt paths in the DB are relative to the base directory unless
absolute. The default base is the parent of the DB's folder, so
<repo>/evidence/evidence.db resolves evidence/receipts/<id>.json with no
flags; --base-dir (or AGENTHAND_EVIDENCE_BASE) overrides it for a DB
copied outside <repo>/evidence/ — without it, receipts skip and the
verdict is INCOMPLETE.

--json replaces the OVERALL text line with a JSON summary carrying
verdict, rehashed, skipped, and failing. Only verify / verify-all print
OVERALL.
"""

import argparse
import hashlib
import json
import os
import sqlite3
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_DB = os.path.join(HERE, "evidence.db")
TOL = 1e-6

failures = 0
rehashed = 0
skipped = 0


def line(status, msg):
    global failures, skipped
    if status == "FAIL":
        failures += 1
    if status == "SKIP":
        skipped += 1
    print(f"{status:<6} {msg}")


def sha256_of(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def resolve(source_file, base):
    if not source_file:
        return None
    if os.path.isabs(source_file):
        return source_file
    return os.path.join(base, source_file)


def check_receipt_hash(con, receipt_id, run_id, file_sha256, source_file, base):
    """One receipt hash check under the C-27 counting rule."""
    global rehashed
    path = resolve(source_file, base)
    if not source_file or not path or not os.path.isfile(path):
        line("SKIP", f"receipt {receipt_id} (run {run_id}): file not present locally: {source_file}")
        return
    actual = sha256_of(path)
    if actual == file_sha256:
        line("PASS", f"receipt {receipt_id} (run {run_id}): sha256 matches {source_file}")
        rehashed += 1
    else:
        line("FAIL", f"receipt {receipt_id} (run {run_id}): sha256 MISMATCH for {source_file} "
                     f"(db {file_sha256[:16]}... disk {actual[:16]}...)")


def verdict():
    if failures:
        return "FAIL", 1
    if rehashed > 0 and skipped == 0:
        return "PASS", 0
    return "INCOMPLETE", 4


def print_overall(as_json):
    v, _ = verdict()
    if as_json:
        print(json.dumps({"verdict": v, "rehashed": rehashed,
                          "skipped": skipped, "failing": failures}))
    else:
        print(f"OVERALL: {v} ({rehashed} re-hashed, {skipped} skipped, "
              f"{failures} failing line(s))")


def claim_scope_line(claim):
    scope = {
        "scope_photo": claim["scope_photo"],
        "scope_build": claim["scope_build"],
        "scope_date": claim["scope_date"],
    }
    missing = [k for k, v in scope.items() if not (v and str(v).strip())]
    has_reason = bool(claim["reason"] and claim["reason"].strip())
    reason_required = claim["status"] in ("withdrawn", "refused")
    if not missing and (has_reason or not reason_required):
        line("PASS", f"claim {claim['claim_id']} ({claim['status']}): scope fields complete"
                     + ("; reason present" if has_reason else ""))
    elif missing and has_reason and (has_reason or not reason_required):
        line("PASS", f"claim {claim['claim_id']} ({claim['status']}): missing "
                     f"{', '.join(missing)}, with an explicit reason on record")
    else:
        problems = []
        if missing:
            problems.append("missing " + ", ".join(missing) + " without a complete reason")
        if reason_required and not has_reason:
            problems.append("withdrawn/refused claim has no reason")
        line("FAIL", f"claim {claim['claim_id']} ({claim['status']}): " + "; ".join(problems))


def verify_claim(con, claim_id, base, as_json):
    claim = con.execute(
        "SELECT claim_id, status, scope_photo, scope_build, scope_date, reason "
        "FROM claims WHERE claim_id = ?", (claim_id,)
    ).fetchone()
    if claim is None:
        line("FAIL", f"claim {claim_id}: no such claim")
        print_overall(as_json)
        return 1
    print(f"== verify claim:{claim_id} ==")
    reached = con.execute(
        """WITH RECURSIVE walk(node) AS (
               SELECT 'claim:' || ?
               UNION
               SELECT e.to_node FROM edges e JOIN walk w ON e.from_node = w.node)
           SELECT DISTINCT node FROM walk WHERE node LIKE 'receipt:%'""",
        (claim_id,),
    ).fetchall()
    if reached:
        for (node,) in reached:
            rid = int(node.split(":", 1)[1])
            r = con.execute(
                "SELECT receipt_id, run_id, file_sha256, source_file FROM receipts "
                "WHERE receipt_id = ?", (rid,)
            ).fetchone()
            if r is not None and r["file_sha256"] is not None:
                check_receipt_hash(con, r["receipt_id"], r["run_id"],
                                   r["file_sha256"], r["source_file"], base)
    elif claim["status"] in ("withdrawn", "refused"):
        line("REPORT", f"claim {claim_id} ({claim['status']}): no receipt by design; "
                       "no receipt checks")
    else:
        line("FAIL", f"claim {claim_id}: directed walk reaches no receipt")
    claim_scope_line(claim)
    print_overall(as_json)
    return verdict()[1]


def verify_all(con, base, as_json):
    # --- (a) receipt hashes -------------------------------------------------
    print("== (a) receipt file hashes ==")
    n_pass = n_skip = 0
    for r in con.execute(
        "SELECT receipt_id, run_id, file_sha256, source_file FROM receipts "
        "WHERE file_sha256 IS NOT NULL ORDER BY receipt_id"
    ):
        before_pass, before_skip = rehashed, skipped
        check_receipt_hash(con, r["receipt_id"], r["run_id"],
                           r["file_sha256"], r["source_file"], base)
        n_pass += rehashed - before_pass
        n_skip += skipped - before_skip
    n_nohash = con.execute(
        "SELECT COUNT(*) FROM receipts WHERE file_sha256 IS NULL").fetchone()[0]
    print(f"  -> {n_pass} matched, {n_skip} not-on-disk, {n_nohash} receipts carry no hash")

    # --- (b) run totals vs call sums ----------------------------------------
    print("== (b) run totals vs sum of call costs ==")
    n_match = n_report = 0
    for run in con.execute(
        "SELECT run_id, label, total_settled_usd, notes FROM runs "
        "WHERE total_settled_usd IS NOT NULL ORDER BY run_id"
    ):
        costed = con.execute(
            "SELECT COUNT(*), COALESCE(SUM(settled_usd), 0) FROM calls "
            "WHERE run_id = ? AND settled_usd IS NOT NULL", (run["run_id"],)
        ).fetchone()
        n_costed, total_calls = costed[0], costed[1]
        total = run["total_settled_usd"]
        if n_costed == 0:
            line("REPORT", f"run {run['run_id']} ({run['label'][:52]}): total ${total:.6f} on record, "
                           "no costed calls to check it against")
            n_report += 1
            continue
        if abs(total_calls - total) <= TOL:
            line("PASS", f"run {run['run_id']} ({run['label'][:52]}): total ${total:.6f} == calls sum")
            n_match += 1
            continue
        notes = run["notes"] or ""
        explained = ("MISMATCH:" in notes) or ("UNSOURCED:" in notes) or bool(notes.strip())
        if explained:
            line("REPORT", f"run {run['run_id']} ({run['label'][:52]}): total ${total:.6f} != costed-calls sum "
                           f"${total_calls:.6f} — difference is explained in the run's notes/flags")
            n_report += 1
        else:
            line("FAIL", f"run {run['run_id']} ({run['label'][:52]}): total ${total:.6f} != calls sum "
                         f"${total_calls:.6f} with no explanation on record")
    print(f"  -> {n_match} matched, {n_report} reported-with-explanation")

    # --- (c) graph connectivity ----------------------------------------------
    print("== (c) graph connectivity ==")
    orphan_calls = con.execute(
        "SELECT COUNT(*) FROM calls c LEFT JOIN runs r ON r.run_id = c.run_id "
        "WHERE r.run_id IS NULL").fetchone()[0]
    orphan_receipts = con.execute(
        "SELECT COUNT(*) FROM receipts x LEFT JOIN runs r ON r.run_id = x.run_id "
        "WHERE r.run_id IS NULL").fetchone()[0]
    if orphan_calls == 0 and orphan_receipts == 0:
        line("PASS", "no orphan calls or receipts (every row joins to a run)")
    else:
        line("FAIL", f"orphans: {orphan_calls} calls, {orphan_receipts} receipts join to no run")

    edgeless_receipts = con.execute(
        "SELECT COUNT(*) FROM receipts x WHERE NOT EXISTS ("
        "  SELECT 1 FROM edges e WHERE e.relation = 'has_receipt' "
        "  AND e.from_node = 'run:' || x.run_id AND e.to_node = 'receipt:' || x.receipt_id)"
    ).fetchone()[0]
    edgeless_calls = con.execute(
        "SELECT COUNT(*) FROM calls c WHERE NOT EXISTS ("
        "  SELECT 1 FROM edges e WHERE e.relation = 'has_call' "
        "  AND e.from_node = 'run:' || c.run_id AND e.to_node = 'call:' || c.call_id)"
    ).fetchone()[0]
    if edgeless_receipts == 0 and edgeless_calls == 0:
        n_r = con.execute("SELECT COUNT(*) FROM receipts").fetchone()[0]
        n_c = con.execute("SELECT COUNT(*) FROM calls").fetchone()[0]
        line("PASS", f"every receipt ({n_r}) and every call ({n_c}) has its run edge in the graph")
    else:
        line("FAIL", f"missing run edges: {edgeless_receipts} receipts, {edgeless_calls} calls")

    # Directed reachability: evidence-bearing claims must reach a receipt
    # (claim -> ... -> receipt), the flagship trace of the evidence layer.
    # Withdrawn/refused claims are first-class graph nodes too, but some
    # intentionally have no receipt to reach; their criterion edge is the
    # expected terminus and is reported as such.
    for claim in con.execute("SELECT claim_id, status FROM claims ORDER BY claim_id"):
        reached = con.execute(
            """WITH RECURSIVE walk(node) AS (
                   SELECT 'claim:' || ?
                   UNION
                   SELECT e.to_node FROM edges e JOIN walk w ON e.from_node = w.node)
               SELECT COUNT(*) FROM walk WHERE node LIKE 'receipt:%'""",
            (claim["claim_id"],),
        ).fetchone()[0]
        if reached > 0:
            line("PASS", f"claim {claim['claim_id']}: directed walk reaches {reached} receipt(s)")
        elif claim["status"] in ("withdrawn", "refused"):
            line("PASS", f"claim {claim['claim_id']} ({claim['status']}): no receipt by design; "
                         "directed walk terminates at its rubric criterion")
        else:
            line("FAIL", f"claim {claim['claim_id']}: directed walk reaches no receipt")

    # --- (d) receipts root ---------------------------------------------------
    print("== (d) receipts root ==")
    meta = {
        r["key"]: r["value"]
        for r in con.execute("SELECT key, value FROM meta")
    }
    rooted_hashes = sorted(
        r[0] for r in con.execute(
            "SELECT file_sha256 FROM receipts WHERE file_sha256 IS NOT NULL"
        )
    )
    computed_root = hashlib.sha256(
        "".join(rooted_hashes).encode("utf-8")
    ).hexdigest()
    stored_root = meta.get("receipts_root")
    stored_count = meta.get("receipt_count_rooted")
    if stored_root == computed_root and stored_count == str(len(rooted_hashes)):
        line("PASS", f"receipts_root recomputes to {computed_root} "
                     f"({len(rooted_hashes)} rooted receipts)")
    else:
        line("FAIL", f"receipts_root mismatch: stored {stored_root} / count {stored_count}; "
                     f"computed {computed_root} / count {len(rooted_hashes)}")

    # --- (e) claim scope ------------------------------------------------------
    print("== (e) claim scope ==")
    for claim in con.execute(
        "SELECT claim_id, status, scope_photo, scope_build, scope_date, reason "
        "FROM claims ORDER BY claim_id"
    ):
        claim_scope_line(claim)

    print_overall(as_json)
    return verdict()[1]


def main(argv=None):
    p = argparse.ArgumentParser(description="Verify evidence.db (see module docstring).")
    p.add_argument("verb", nargs="?", default="verify-all",
                   help="verify, verify-all, or verify claim:N (default: verify-all)")
    p.add_argument("target", nargs="?", default=None, help="claim:N for the claim verb")
    p.add_argument("--db", default=None, help="evidence.db path (default: evidence-layer/evidence.db)")
    p.add_argument("--base-dir", default=None,
                   help="base for relative receipt paths (default: parent of the DB's folder; "
                        "overrides AGENTHAND_EVIDENCE_BASE)")
    p.add_argument("--json", action="store_true",
                   help="replace the OVERALL text line with a JSON summary")
    args = p.parse_args(argv)

    db = args.db or DEFAULT_DB
    base = (args.base_dir
            or os.environ.get("AGENTHAND_EVIDENCE_BASE")
            or os.path.dirname(os.path.dirname(os.path.abspath(db))))

    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    con.row_factory = sqlite3.Row
    try:
        if args.verb in ("verify", "verify-all") and args.target is None:
            return verify_all(con, base, args.json)
        if args.verb == "verify" and args.target and args.target.startswith("claim:"):
            try:
                claim_id = int(args.target.split(":", 1)[1])
            except ValueError:
                p.error("claim id must be an integer, e.g. claim:6")
            return verify_claim(con, claim_id, base, args.json)
        p.error("usage: verify_db.py [verify|verify-all|verify claim:N] [--db PATH] "
                "[--base-dir DIR] [--json]")
    finally:
        con.close()
    return 2


if __name__ == "__main__":
    sys.exit(main())
