"""Ledger receipts and the fail-closed spend cap."""

import json

import pytest

from agenthand import ledger


def _seed_receipt(path, usd, tool="identify"):
    ledger.append_receipt(
        path, ledger.Receipt(tool=tool, model="muse-spark-1.3", usd_estimate=usd)
    )


def test_append_and_total(tmp_path):
    path = tmp_path / "ledger.jsonl"
    _seed_receipt(path, 1.25)
    _seed_receipt(path, 2.50, tool="cut_out")
    assert ledger.ledger_total_usd(path) == 3.75

    lines = path.read_text(encoding="utf-8").strip().splitlines()
    assert len(lines) == 2
    record = json.loads(lines[0])
    assert record["tool"] == "identify"
    assert record["pricing_table_version"]  # version is always recorded


def test_missing_ledger_is_zero(tmp_path):
    assert ledger.ledger_total_usd(tmp_path / "nope.jsonl") == 0.0


def test_cap_allows_call_under_cap(tmp_path):
    path = tmp_path / "ledger.jsonl"
    _seed_receipt(path, 100.00)
    total = ledger.check_spend_cap(path, projected_usd=49.99, cap_usd=150.00)
    assert total == 100.00


def test_cap_allows_landing_exactly_on_cap(tmp_path):
    path = tmp_path / "ledger.jsonl"
    _seed_receipt(path, 100.00)
    ledger.check_spend_cap(path, projected_usd=50.00, cap_usd=150.00)


def test_cap_refuses_when_seeded_ledger_exceeds_cap(tmp_path):
    path = tmp_path / "ledger.jsonl"
    _seed_receipt(path, 149.00)
    _seed_receipt(path, 5.00)  # ledger now at $154, over the $150 cap
    with pytest.raises(ledger.SpendCapExceeded):
        ledger.check_spend_cap(path, projected_usd=0.01, cap_usd=150.00)


def test_cap_refuses_projected_overshoot(tmp_path):
    path = tmp_path / "ledger.jsonl"
    _seed_receipt(path, 149.50)
    with pytest.raises(ledger.SpendCapExceeded):
        ledger.check_spend_cap(path, projected_usd=1.00, cap_usd=150.00)


def test_corrupt_ledger_fails_closed(tmp_path):
    path = tmp_path / "ledger.jsonl"
    path.write_text("{not json}\n", encoding="utf-8")
    with pytest.raises(ledger.SpendCapExceeded):
        ledger.check_spend_cap(path, projected_usd=0.01, cap_usd=150.00)


def test_negative_ledger_line_fails_closed(tmp_path):
    # A crafted negative line would "refund" the budget if it were summed.
    path = tmp_path / "ledger.jsonl"
    path.write_text(
        json.dumps({"tool": "x", "model": None, "usd_estimate": -50.0}) + "\n",
        encoding="utf-8",
    )
    with pytest.raises(ledger.SpendCapExceeded):
        ledger.ledger_total_usd(path)


def test_nan_ledger_line_fails_closed(tmp_path):
    # JSON accepts the NaN literal; comparisons against NaN are all False,
    # which would silently disable the cap if the line were summed.
    path = tmp_path / "ledger.jsonl"
    path.write_text(
        '{"tool":"x","model":null,"usd_estimate":NaN}\n', encoding="utf-8"
    )
    with pytest.raises(ledger.SpendCapExceeded):
        ledger.ledger_total_usd(path)


def test_nan_projection_refused(tmp_path):
    path = tmp_path / "ledger.jsonl"
    with pytest.raises(ledger.SpendCapExceeded):
        ledger.check_spend_cap(path, projected_usd=float("nan"), cap_usd=150.00)
    with pytest.raises(ledger.SpendCapExceeded):
        ledger.check_spend_cap(path, projected_usd=1.0, cap_usd=float("inf"))


def test_append_fsyncs_before_returning(tmp_path, monkeypatch):
    # A receipt the caller believes is recorded must survive a crash:
    # append flushes and fsyncs the ledger file before returning.
    fsynced = []
    real_fsync = ledger.os.fsync
    monkeypatch.setattr(
        ledger.os, "fsync", lambda fd: (fsynced.append(fd), real_fsync(fd))[1]
    )
    path = tmp_path / "ledger.jsonl"
    _seed_receipt(path, 1.25)
    _seed_receipt(path, 2.50)
    assert len(fsynced) == 2  # one fsync per append
    assert ledger.ledger_total_usd(path) == 3.75


def test_crash_mid_append_torn_tail_fails_closed(tmp_path):
    # Simulate a crash in the middle of an append: one complete,
    # fsynced receipt, then a torn final line (truncated JSON, no
    # newline). Reads must NOT skip the torn line and carry on: its
    # amount is unknowable, and if it was a pending reservation,
    # skipping it would understate spend precisely when the cap is
    # the only thing standing between us and real money. Fail closed;
    # a human repairs the file.
    path = tmp_path / "ledger.jsonl"
    _seed_receipt(path, 10.00)
    with path.open("a", encoding="utf-8") as fh:
        fh.write('{"tool":"identify","model":"muse-spark-1.3","usd_estimate":7.')
    with pytest.raises(ledger.SpendCapExceeded):
        ledger.ledger_total_usd(path)
    with pytest.raises(ledger.SpendCapExceeded):
        ledger.check_spend_cap(path, projected_usd=0.01, cap_usd=150.00)


def test_pending_reservation_counts_at_projection(tmp_path):
    path = tmp_path / "ledger.jsonl"
    ledger.append_receipt(
        path,
        ledger.Receipt(
            tool="identify",
            model="muse-spark-1.3",
            usd_estimate=3.25,
            phase=ledger.PHASE_PENDING,
            call_id="call-1",
        ),
    )
    assert ledger.ledger_total_usd(path) == 3.25


def test_settlement_supersedes_reservation(tmp_path):
    path = tmp_path / "ledger.jsonl"
    ledger.append_receipt(
        path,
        ledger.Receipt(
            tool="identify",
            model="muse-spark-1.3",
            usd_estimate=3.25,
            phase=ledger.PHASE_PENDING,
            call_id="call-1",
        ),
    )
    ledger.append_receipt(
        path,
        ledger.Receipt(
            tool="identify",
            model="muse-spark-1.3",
            usd_estimate=1.10,
            phase=ledger.PHASE_SETTLED,
            call_id="call-1",
        ),
    )
    # The actual replaces the projection; it is not added to it.
    assert ledger.ledger_total_usd(path) == 1.10


def test_usage_fields_round_trip(tmp_path):
    path = tmp_path / "ledger.jsonl"
    ledger.append_receipt(
        path,
        ledger.Receipt(
            tool="identify",
            model="muse-spark-1.3",
            usd_estimate=0.42,
            input_tokens=1000,
            output_tokens=250,
            cached_input_tokens=400,
            images=2,
        ),
    )
    record = json.loads(path.read_text(encoding="utf-8").strip())
    assert record["input_tokens"] == 1000
    assert record["output_tokens"] == 250
    assert record["cached_input_tokens"] == 400
    assert record["images"] == 2
