"""The billing choke point: reserve-then-settle, races, failure modes."""

import json
import threading

import pytest

from agenthand import config, ledger, meta_client, pricing


@pytest.fixture(autouse=True)
def scratch_env(tmp_path, monkeypatch):
    monkeypatch.setenv("AGENTHAND_DATA_DIR", str(tmp_path))
    monkeypatch.setenv("AGENTHAND_TOTAL_SPEND_CAP_USD", "120")
    monkeypatch.delenv("AGENTHAND_LEDGER", raising=False)
    monkeypatch.delenv("AGENTHAND_LEDGER_PATH", raising=False)
    return tmp_path


def _records():
    path = config.get_ledger_path()
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def test_settle_records_actual_cost_and_usage():
    result = meta_client.billed_call(
        "identify",
        "muse-spark-1.3",
        1.00,
        lambda: {"input_tokens": 1_000_000, "cached_input_tokens": 1_000_000},
    )
    # Actual: 1M input tokens, all cached, at $0.15/1M.
    assert result["actual_usd"] == 0.15
    assert result["projected_usd"] == 1.00
    assert ledger.ledger_total_usd(config.get_ledger_path()) == 0.15

    records = _records()
    assert [r["phase"] for r in records] == ["pending", "settled"]
    settled = records[1]
    assert settled["call_id"] == records[0]["call_id"] == result["call_id"]
    # The usage the provider reported is stored on the settled receipt.
    assert settled["input_tokens"] == 1_000_000
    assert settled["cached_input_tokens"] == 1_000_000
    assert settled["output_tokens"] == 0
    assert settled["usd_estimate"] == 0.15


def test_missing_usage_settles_at_the_reservation_not_zero():
    # GPT's reproduction: the provider response carried no usage, the
    # call priced to $0, and the $0.25 reservation evaporated from the
    # total. Fail-closed: a call that cannot be priced settles at its
    # reserved projection — the reservation stays counted.
    result = meta_client.billed_call("identify", "muse-spark-1.3", 0.25, lambda: {})
    assert result["actual_usd"] == 0.25
    assert result["usage_missing"] is True
    assert ledger.ledger_total_usd(config.get_ledger_path()) == 0.25
    records = _records()
    assert [r["phase"] for r in records] == ["pending", "settled"]
    assert records[1]["usd_estimate"] == 0.25
    assert "usage_missing" in records[1]["note"]


def test_none_usage_also_settles_at_the_reservation():
    result = meta_client.billed_call("identify", "muse-spark-1.3", 0.25, lambda: None)
    assert result["actual_usd"] == 0.25
    assert ledger.ledger_total_usd(config.get_ledger_path()) == 0.25


def test_response_without_a_usage_object_is_unusable():
    # _usage_of feeds billed_call; a chat response with no usage
    # object at all cannot be priced, so it is an unusable response —
    # the reservation stays counted via the exception path.
    from types import SimpleNamespace

    with pytest.raises(meta_client.IdentifyResultError):
        meta_client._usage_of(SimpleNamespace(choices=[]))


def test_zero_projection_stub_call_still_settles_zero(tmp_path, monkeypatch):
    # The placeholder-plate dev path reserves $0 by design; the
    # fail-closed settle must not invent spend for it.
    result = meta_client.billed_call(
        "draw", "muse-image-1.0", 0.0, lambda: {}, status="stub"
    )
    assert result["actual_usd"] == 0.0
    assert result["usage_missing"] is False
    assert ledger.ledger_total_usd(config.get_ledger_path()) == 0.0


def test_exception_after_send_leaves_reservation_counted():
    def boom():
        raise RuntimeError("provider timed out; the call may have billed")

    with pytest.raises(RuntimeError):
        meta_client.billed_call("draw", "muse-image-1.0", 0.50, boom)

    # No settlement was written, so the reservation still counts in full:
    # a call that may have billed is treated as having billed.
    assert ledger.ledger_total_usd(config.get_ledger_path()) == 0.50
    records = _records()
    assert len(records) == 1
    assert records[0]["phase"] == "pending"


def test_unknown_model_raises_and_writes_nothing():
    with pytest.raises(pricing.UnknownModelError):
        meta_client.billed_call("identify", "muse-spark-9.9", 1.00, lambda: {})
    assert _records() == []


def test_projection_over_cap_refused_before_call_runs():
    ran = []
    with pytest.raises(ledger.SpendCapExceeded):
        meta_client.billed_call(
            "identify", "muse-spark-1.3", 121.00, lambda: ran.append(True)
        )
    assert ran == []  # fn never ran
    assert _records() == []  # and no reservation was left behind


def test_two_threads_racing_the_gate_cannot_both_fit():
    entered = threading.Event()
    release = threading.Event()
    errors: list[Exception] = []

    def slow_call():
        entered.set()  # reservation is already on disk when fn runs
        release.wait(timeout=10)
        return {}

    def run_a():
        try:
            meta_client.billed_call("identify", "muse-spark-1.3", 100.00, slow_call)
        except Exception as exc:  # pragma: no cover - failure path
            errors.append(exc)

    thread_a = threading.Thread(target=run_a)
    thread_a.start()
    assert entered.wait(timeout=10)

    # A holds a $100 reservation against the $120 cap; B's $30 cannot fit.
    with pytest.raises(ledger.SpendCapExceeded):
        meta_client.billed_call("identify", "muse-spark-1.3", 30.00, lambda: {})

    release.set()
    thread_a.join(timeout=10)
    assert errors == []


@pytest.mark.parametrize("code", ["NO_MATCH", "POOR_MASK", "BAD_MODEL_RESPONSE"])
def test_completed_cut_failure_settles_three_images_from_eight_image_reservation(code):
    from agenthand.segmentation import CutOutResultError

    failure = CutOutResultError("decoded failure", code=code, attempts=[{}] * 3)

    def run():
        raise failure

    with pytest.raises(CutOutResultError) as raised:
        meta_client.billed_call("cut_out", "sam-3.1", 8 * 0.0025, run)
    assert raised.value is failure and failure.images == 3
    assert failure.cost_counted_usd == 0.0075
    rows = _records()
    assert [r["phase"] for r in rows] == ["pending", "settled"]
    assert rows[-1]["images"] == 3 and rows[-1]["status"] == "error"
    assert ledger.ledger_total_usd(config.get_ledger_path()) == 0.0075


# ---------------------------------------------------------------------------
# Fix pass F0: the Meta HTTP client's timeout. The openai SDK default
# (600 s read, 5 s connect) is replaced by 180 s per call and 20 s to
# connect, the Rust build's values; AGENTHAND_HTTP_TIMEOUT_S (whole
# seconds, > 0) overrides the per-call value. Loopback only: no test
# here leaves the machine.
# ---------------------------------------------------------------------------


def _slow_server(delay_s: float) -> str:
    """A loopback HTTP server that accepts one connection, reads the
    request, and answers 200 only after `delay_s`."""
    import socket
    import time

    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.bind(("127.0.0.1", 0))
    srv.listen(1)
    port = srv.getsockname()[1]

    def serve():
        try:
            conn, _ = srv.accept()
            with conn:
                conn.recv(65536)
                time.sleep(delay_s)
                body = b'{"object":"list","data":[]}'
                conn.sendall(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
                    + f"Content-Length: {len(body)}\r\n".encode()
                    + b"Connection: close\r\n\r\n"
                    + body
                )
        except OSError:
            pass
        finally:
            srv.close()

    threading.Thread(target=serve, daemon=True).start()
    return f"http://127.0.0.1:{port}/v1"


def test_build_client_honours_the_env_timeout(monkeypatch):
    import time

    import openai

    for var in ("HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"):
        monkeypatch.delenv(var, raising=False)
    monkeypatch.setenv("META_API_KEY", "test-key-loopback-only")
    monkeypatch.setenv("AGENTHAND_HTTP_TIMEOUT_S", "1")
    client = meta_client.build_client()

    # A 1 s override must cut off a response that takes 4 s.
    url = _slow_server(4.0)
    started = time.monotonic()
    try:
        response = client.with_options(base_url=url).models.list()
    except openai.APITimeoutError:
        response = None
    elapsed = time.monotonic() - started
    assert response is None, (
        f"a 1 s timeout must cut off a 4 s response; got {response!r} "
        f"after {elapsed:.1f} s"
    )
    assert elapsed < 3.0, f"took {elapsed:.1f} s"

    assert client.timeout.read == 1.0
    assert client.timeout.connect == meta_client.HTTP_CONNECT_TIMEOUT_S == 20
    monkeypatch.setenv("AGENTHAND_HTTP_TIMEOUT_S", "7")
    assert meta_client.build_client().timeout.read == 7.0
    monkeypatch.delenv("AGENTHAND_HTTP_TIMEOUT_S")
    default = meta_client.build_client().timeout
    assert (default.connect, default.read, default.write, default.pool) == (
        20.0,
        180.0,
        180.0,
        180.0,
    )


def test_timeout_defaults_to_180_seconds():
    assert meta_client.DEFAULT_HTTP_TIMEOUT_S == 180
    assert meta_client.HTTP_TIMEOUT_ENV == "AGENTHAND_HTTP_TIMEOUT_S"
    # Unset, empty, unparseable, fractional, zero and negative values
    # keep the default; a positive whole number of seconds wins
    # (surrounding whitespace allowed), matching the Rust parser.
    for raw in (None, "", "   ", "abc", "1.5", "0", "-5", "1_000", "99999999999999999999999"):
        assert meta_client.http_timeout_from(raw) == 180, raw
    assert meta_client.http_timeout_from("1") == 1
    assert meta_client.http_timeout_from(" 42 ") == 42
    assert meta_client.http_timeout_from("+7") == 7


# ---------------------------------------------------------------------------
# Fix pass F4: one spend cap across room runs. A room run points
# AGENTHAND_LEDGER_PATH at its own ledger; that file must be a MIRROR of
# the run's rows, never the place the cap is checked: the cap always
# runs against the cumulative ledger, so two runs cannot each spend
# $0.90 under a $1.00 cap. (Mirror of the Rust build's
# room_ledgers_share_one_spend_cap_in_both_stores, JSONL store.)
# ---------------------------------------------------------------------------


def _jsonl_rows(path):
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def test_room_ledgers_share_one_spend_cap(tmp_path, monkeypatch):
    monkeypatch.setenv("AGENTHAND_TOTAL_SPEND_CAP_USD", "1.00")

    # Run 1: its own room ledger; settles $0.90.
    room1 = tmp_path / "room1" / "ledger.jsonl"
    monkeypatch.setenv("AGENTHAND_LEDGER_PATH", str(room1))
    first = meta_client.billed_call(
        "draw", "muse-image-1.0", 0.90, lambda: {"images": 90}
    )
    assert first["actual_usd"] == pytest.approx(0.90)

    # Run 2: a fresh room ledger; the $0.90 reservation must be refused
    # against the cumulative $0.90 already spent, before its call runs.
    room2 = tmp_path / "room2" / "ledger.jsonl"
    monkeypatch.setenv("AGENTHAND_LEDGER_PATH", str(room2))
    ran = []

    def second_call():
        ran.append(True)
        return {"images": 90}

    refused = None
    try:
        meta_client.billed_call("draw", "muse-image-1.0", 0.90, second_call)
    except ledger.SpendCapExceeded as exc:
        refused = exc
    assert refused is not None, (
        "run 2's $0.90 reservation passed a $1.00 cap after run 1 spent $0.90"
    )
    assert "Spend cap" in str(refused)
    assert ran == [], "the refused call must not run"

    # The cap ledger (the data dir's) holds run 1's spend only.
    cumulative = config.get_cumulative_ledger_path()
    assert cumulative == tmp_path / "ledger.jsonl"
    assert ledger.ledger_total_usd(cumulative) == pytest.approx(0.90)
    # Room totals stay readable from the room ledgers.
    rows1 = _jsonl_rows(room1)
    assert [r["phase"] for r in rows1] == ["pending", "settled"]
    assert rows1 == _jsonl_rows(cumulative)  # the same rows, mirrored
    assert ledger.ledger_total_usd(room1) == pytest.approx(0.90)
    assert ledger.ledger_total_usd(room2) == 0.0


def test_mirror_naming_the_cumulative_ledger_is_written_once(tmp_path, monkeypatch):
    monkeypatch.setenv("AGENTHAND_LEDGER_PATH", str(tmp_path / "ledger.jsonl"))
    meta_client.billed_call("draw", "muse-image-1.0", 0.01, lambda: {"images": 1})
    rows = _jsonl_rows(tmp_path / "ledger.jsonl")
    assert [r["phase"] for r in rows] == ["pending", "settled"]


# ---------------------------------------------------------------------------
# Fix pass F5: settle is cap-checked. A reservation is a projection and a
# call can cost more than projected; an over-cap settle is recorded
# truthfully (status over_cap, the overage in its note), billed_call
# raises naming the breach, and the next reservation is refused. (Mirror
# of the Rust build's settle_over_the_cap_is_recorded_and_fails_loudly
# and settle_within_the_cap_excludes_its_own_reservation, JSONL store.)
# ---------------------------------------------------------------------------


def test_settle_over_the_cap_is_recorded_and_fails_loudly(tmp_path, monkeypatch):
    monkeypatch.setenv("AGENTHAND_TOTAL_SPEND_CAP_USD", "1.00")
    mirror = tmp_path / "room" / "ledger.jsonl"
    monkeypatch.setenv("AGENTHAND_LEDGER_PATH", str(mirror))

    # 1000 in + 1,000,000 out on muse-spark-1.3:
    # 1000 x $1.25/M + 1,000,000 x $4.25/M = $4.25125.
    raised = None
    try:
        meta_client.billed_call(
            "identify",
            "muse-spark-1.3",
            0.01,
            lambda: {"input_tokens": 1000, "output_tokens": 1_000_000},
        )
    except ledger.SpendCapExceeded as exc:
        raised = exc
    assert raised is not None, "a $4.25125 settle under a $1.00 cap returned normally"
    message = str(raised)
    assert message.startswith("SPEND CAP EXCEEDED at settle:"), message
    assert "4.251250" in message and "over the $1.00 cap" in message, message
    assert isinstance(raised, ledger.SpendCapExceededAtSettle)
    assert raised.actual_usd == pytest.approx(4.25125)
    assert raised.overage_usd == pytest.approx(3.25125)

    # The settle row is present, truthful, and marked over_cap, in the
    # cumulative ledger and its mirror alike.
    cumulative = config.get_cumulative_ledger_path()
    rows = _jsonl_rows(cumulative)
    assert [r["phase"] for r in rows] == ["pending", "settled"]
    settled = rows[1]
    assert settled["status"] == ledger.STATUS_OVER_CAP == "over_cap"
    assert settled["usd_estimate"] == pytest.approx(4.25125)
    assert "3.251250 over the $1.00 cap" in settled["note"], settled["note"]
    assert _jsonl_rows(mirror) == rows
    assert ledger.ledger_total_usd(cumulative) == pytest.approx(4.25125)

    # Every later reservation is refused, before its call runs.
    ran = []

    def next_call():
        ran.append(True)
        return {"images": 1}

    with pytest.raises(ledger.SpendCapExceeded, match="Spend cap"):
        meta_client.billed_call("cut_out", "sam-3.1", 0.0025, next_call)
    assert ran == []


def test_settle_within_the_cap_excludes_its_own_reservation(monkeypatch):
    # $0.50 open elsewhere + a $0.40 call settling at $0.45 stays under
    # $1.00 and settles cleanly: the call's own reservation is not
    # counted twice.
    monkeypatch.setenv("AGENTHAND_TOTAL_SPEND_CAP_USD", "1.00")

    def boom():
        raise RuntimeError("simulated failure")

    with pytest.raises(RuntimeError):
        meta_client.billed_call("draw", "muse-image-1.0", 0.50, boom)
    ok = meta_client.billed_call("draw", "muse-image-1.0", 0.40, lambda: {"images": 45})
    assert ok["actual_usd"] == pytest.approx(0.45)
    rows = _records()
    assert rows[-1]["phase"] == "settled" and rows[-1]["status"] == "ok"
    assert ledger.ledger_total_usd(config.get_ledger_path()) == pytest.approx(0.95)


def test_apply_settle_cap_matches_the_rust_wording():
    receipt = ledger.Receipt(tool="draw", model="muse-image-1.0", usd_estimate=0.5, note="n")
    assert ledger.apply_settle_cap(receipt, 0.5, 1.0) is None  # landing on the cap is allowed
    assert receipt.status == "ok" and receipt.note == "n"
    over = ledger.apply_settle_cap(receipt, 0.75, 1.0)
    assert over == ledger.SettleOverage(total_usd=1.25, overage_usd=0.25, cap_usd=1.0)
    assert receipt.status == "over_cap"
    assert receipt.note == (
        "n; over_cap: settled $0.500000 brought the ledger to $1.250000, "
        "$0.250000 over the $1.00 cap"
    )
