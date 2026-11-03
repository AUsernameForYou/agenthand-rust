"""identify wired to the Meta Model API: offline tests, mocked transport.

A fake client stands in for the OpenAI-compatible transport; no network
is touched and no real key exists anywhere in this file. Each test
points the data dir at a tmp dir and sets a placeholder META_API_KEY so
the configured path runs end to end through the billing choke point.
"""

import base64
import json
import os
from io import BytesIO
from pathlib import Path
from types import SimpleNamespace

os.environ.setdefault("AGENTHAND_DATA_DIR", "/tmp/agenthand-test-placeholder")

import pytest  # noqa: E402
from PIL import Image  # noqa: E402

from agenthand import config, ledger, meta_client, pricing, server  # noqa: E402

FAKE_KEY = "placeholder-test-key-not-a-real-key"

GOOD_PAYLOAD = {
    "description": "A brown leather bifold wallet on a wooden table.",
    "concept": "brown leather wallet",
    "alternates": ["leather bifold wallet", "brown wallet"],
    "suggested_title": "Brown Leather Bifold Wallet - Genuine Leather",
    "listing_bullets": ["Genuine leather", "Six card slots", "Slim profile"],
    "condition_notes": "Light corner wear visible; no tears or stains.",
}


@pytest.fixture(autouse=True)
def scratch_env(tmp_path, monkeypatch):
    monkeypatch.setenv("AGENTHAND_DATA_DIR", str(tmp_path))
    monkeypatch.setenv("META_API_KEY", FAKE_KEY)
    return tmp_path


def _data_uri(size=(6, 4), color=(120, 80, 40, 255)) -> str:
    buffer = BytesIO()
    Image.new("RGBA", size, color).save(buffer, format="PNG")
    return "data:image/png;base64," + base64.b64encode(buffer.getvalue()).decode()


def _records():
    path = config.get_ledger_path()
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def _response(content, *, prompt_tokens=1500, completion_tokens=220, cached_tokens=0):
    usage = SimpleNamespace(
        prompt_tokens=prompt_tokens,
        completion_tokens=completion_tokens,
        prompt_tokens_details=SimpleNamespace(cached_tokens=cached_tokens),
    )
    message = SimpleNamespace(content=content)
    return SimpleNamespace(choices=[SimpleNamespace(message=message)], usage=usage)


class _FakeCompletions:
    def __init__(self, responses):
        self._responses = list(responses)
        self.calls: list[dict] = []

    def create(self, **kwargs):
        self.calls.append(kwargs)
        if not self._responses:
            raise AssertionError("fake transport ran out of scripted responses")
        return self._responses.pop(0)


class _FakeClient:
    def __init__(self, responses, sam_responses=()):
        self.completions = _FakeCompletions(responses)
        self.chat = SimpleNamespace(completions=self.completions)
        # cut_out is wired now: its SAM transport is client.responses.
        self.responses = _FakeCompletions(
            [SimpleNamespace(output_text=text) for text in sam_responses]
        )


def _install_fake(monkeypatch, responses) -> _FakeClient:
    client = _FakeClient(responses)
    monkeypatch.setattr(meta_client, "build_client", lambda: client)
    return client


def _good_content(**overrides) -> str:
    payload = dict(GOOD_PAYLOAD)
    payload.update(overrides)
    return json.dumps(payload)


# -- the configured happy path ----------------------------------------------


def test_identify_success_parses_plan_and_settles_with_real_usage(monkeypatch):
    client = _install_fake(
        monkeypatch,
        [_response(_good_content(), prompt_tokens=1500, completion_tokens=220, cached_tokens=300)],
    )
    result = server.identify(_data_uri())

    assert result["status"] == "ok"
    assert result["description"] == GOOD_PAYLOAD["description"]
    assert result["concept"] == "brown leather wallet"
    assert result["alternates"] == GOOD_PAYLOAD["alternates"]
    assert result["suggested_title"] == GOOD_PAYLOAD["suggested_title"]
    assert result["listing_bullets"] == GOOD_PAYLOAD["listing_bullets"]
    assert result["condition_notes"] == GOOD_PAYLOAD["condition_notes"]

    # The request that went out: bounded output, strict schema, vision model.
    (request,) = client.completions.calls
    assert request["model"] == "muse-spark-1.3"
    assert request["max_tokens"] == meta_client.IDENTIFY_MAX_TOKENS
    assert request["response_format"]["type"] == "json_schema"
    assert request["response_format"]["json_schema"]["strict"] is True

    # Settled cost is priced from the response usage, counts included.
    expected_cost = pricing.estimate_cost_usd(
        "muse-spark-1.3",
        input_tokens=1500,
        output_tokens=220,
        cached_input_tokens=300,
    )
    assert expected_cost > 0
    assert result["cost_usd"] == expected_cost
    assert result["usage"]["input_tokens"] == 1500
    assert result["usage"]["output_tokens"] == 220
    assert result["usage"]["cached_input_tokens"] == 300

    records = _records()
    assert [r["phase"] for r in records] == ["pending", "settled"]
    pending, settled = records
    # The reservation is the projected worst case for a 6x4 image.
    assert pending["usd_estimate"] == meta_client.project_identify_cost_usd(
        "muse-spark-1.3", 6, 4
    )
    assert pending["tool"] == "identify"
    assert settled["call_id"] == pending["call_id"] == result["call_id"]
    assert settled["usd_estimate"] == expected_cost
    assert settled["input_tokens"] == 1500
    assert settled["output_tokens"] == 220
    assert settled["cached_input_tokens"] == 300


def test_cut_out_plans_internally_with_the_real_identify(monkeypatch):
    client = _FakeClient(
        [_response(_good_content())], sam_responses=["", "", ""]
    )
    monkeypatch.setattr(meta_client, "build_client", lambda: client)
    result = server.cut_out(_data_uri())
    assert result["concept_source"] == "identify_internal"
    assert result["concept"] == "brown leather wallet"
    assert result["alternates"] == GOOD_PAYLOAD["alternates"]
    assert result["plan_error"] is None
    # The plan came from the real identify, and cut_out then spent its
    # two bounded SAM attempts on it (both zero-match here, scripted).
    assert result["status"] == "error"
    assert result["error"]["code"] == "NO_MATCH"
    assert len(client.responses.calls) == 3
    # Both the internal identify call and cut_out's SAM call billed.
    tools = [r["tool"] for r in _records() if r["phase"] == "pending"]
    assert tools == ["identify", "cut_out"]


# -- fail-closed paths --------------------------------------------------------


def test_identify_malformed_response_fails_closed_and_reservation_counts(monkeypatch):
    # A malformed body is a body-parse failure, so the bounded retry
    # burns all IDENTIFY_MAX_ATTEMPTS attempts before failing closed.
    _install_fake(monkeypatch, [_response("this is not json at all")] * 3)
    result = server.identify(_data_uri())

    assert result["status"] == "error"
    assert result["error"]["code"] == "BAD_MODEL_RESPONSE"
    # Every attempt's billed reservation stays counted; nothing settled.
    records = _records()
    assert [r["phase"] for r in records] == ["pending"] * 3
    projected = meta_client.project_identify_cost_usd("muse-spark-1.3", 6, 4)
    assert all(r["usd_estimate"] == projected for r in records)
    assert ledger.ledger_total_usd(config.get_ledger_path()) == pytest.approx(
        projected * 3
    )
    assert result["error"]["cost_counted_usd"] == projected * 3


def test_identify_truncated_response_saves_raw_to_debug_file(monkeypatch):
    # The first live failure mode: output cut off mid-string at the
    # token bound, so the body is a JSON prefix that never closes.
    full = _good_content()
    truncated = full[: len(full) // 2]
    assert truncated != full
    # Truncation is a body-parse failure: all 3 bounded attempts run.
    _install_fake(monkeypatch, [_response(truncated)] * 3)
    result = server.identify(_data_uri())

    assert result["status"] == "error"
    assert result["error"]["code"] == "BAD_MODEL_RESPONSE"

    # Each attempt's full raw body was saved under data/debug/, named
    # for its own ledger call; the error result points at the last one.
    records = _records()
    assert [r["phase"] for r in records] == ["pending"] * 3
    debug_path = result["error"].get("debug_path")
    assert debug_path
    path = Path(debug_path)
    assert path.parent == config.get_data_dir() / "debug"
    assert path.name == f"identify-{records[-1]['call_id']}.txt"
    assert path.read_text(encoding="utf-8") == truncated
    for record in records:
        attempt_file = path.parent / f"identify-{record['call_id']}.txt"
        assert attempt_file.read_text(encoding="utf-8") == truncated

    # The raw model text never lands in the ledger...
    assert truncated not in config.get_ledger_path().read_text(encoding="utf-8")
    # ...and each attempt's reservation is counted exactly once: not
    # dropped, not double-counted.
    projected = meta_client.project_identify_cost_usd("muse-spark-1.3", 6, 4)
    assert all(r["usd_estimate"] == projected for r in records)
    assert ledger.ledger_total_usd(config.get_ledger_path()) == pytest.approx(
        projected * 3
    )
    assert result["error"]["cost_counted_usd"] == projected * 3


# -- the bounded retry on empty / truncated bodies ---------------------------
#
# Live flake (dry-run gate, 2026-10-27): muse-spark-1.3 intermittently
# answers HTTP 200 with an empty body or a body cut mid-JSON. A body
# failure is re-rolled as a fresh billed_call, at most
# IDENTIFY_MAX_ATTEMPTS attempts; the ledger must show every attempt.


def test_identify_truncated_then_valid_succeeds_and_bills_each_attempt(monkeypatch):
    full = _good_content()
    truncated = full[: len(full) // 2]
    good = _response(
        full, prompt_tokens=1500, completion_tokens=220, cached_tokens=300
    )
    client = _install_fake(monkeypatch, [_response(truncated), good])
    result = server.identify(_data_uri())

    assert result["status"] == "ok"
    assert result["concept"] == GOOD_PAYLOAD["concept"]
    assert result["usage"]["input_tokens"] == 1500
    assert len(client.completions.calls) == 2

    # Attempt 1: reserved, raised, left counted (no settle). Attempt 2:
    # reserved + settled under its own call_id. No free retries.
    records = _records()
    assert [r["phase"] for r in records] == ["pending", "pending", "settled"]
    assert records[0]["call_id"] != records[1]["call_id"]
    assert records[2]["call_id"] == records[1]["call_id"] == result["call_id"]
    projected = meta_client.project_identify_cost_usd("muse-spark-1.3", 6, 4)
    assert records[0]["usd_estimate"] == projected
    expected_actual = pricing.estimate_cost_usd(
        "muse-spark-1.3",
        input_tokens=1500,
        output_tokens=220,
        cached_input_tokens=300,
    )
    assert records[2]["usd_estimate"] == expected_actual
    # The reported cost counts the failed attempt's reservation too,
    # and matches the ledger total (per call, a settled receipt
    # supersedes its own reservation; the failed attempt's stands).
    assert result["cost_usd"] == projected + expected_actual
    assert ledger.ledger_total_usd(config.get_ledger_path()) == (
        projected + expected_actual
    )


def test_identify_empty_body_then_valid_succeeds_and_bills_each_attempt(monkeypatch):
    client = _install_fake(monkeypatch, [_response(""), _response(_good_content())])
    result = server.identify(_data_uri())

    assert result["status"] == "ok"
    assert len(client.completions.calls) == 2
    records = _records()
    assert [r["phase"] for r in records] == ["pending", "pending", "settled"]
    # Even the empty body was captured to a debug file for attempt 1.
    debug_file = (
        config.get_data_dir() / "debug" / f"identify-{records[0]['call_id']}.txt"
    )
    assert debug_file.read_text(encoding="utf-8") == ""


def test_identify_all_attempts_bad_fails_closed_and_counts_every_attempt(monkeypatch):
    full = _good_content()
    truncated = full[: len(full) // 2]
    client = _install_fake(
        monkeypatch,
        [_response(truncated), _response(""), _response("this is not json at all")],
    )
    result = server.identify(_data_uri())

    assert result["status"] == "error"
    assert result["error"]["code"] == "BAD_MODEL_RESPONSE"
    # The bound held: exactly IDENTIFY_MAX_ATTEMPTS transport calls,
    # each with its own ledger reservation, nothing settled.
    assert meta_client.IDENTIFY_MAX_ATTEMPTS == 3
    assert len(client.completions.calls) == 3
    records = _records()
    assert [r["phase"] for r in records] == ["pending"] * 3
    assert len({r["call_id"] for r in records}) == 3
    projected = meta_client.project_identify_cost_usd("muse-spark-1.3", 6, 4)
    assert all(r["usd_estimate"] == projected for r in records)
    assert result["error"]["cost_counted_usd"] == projected * 3
    assert ledger.ledger_total_usd(config.get_ledger_path()) == pytest.approx(
        projected * 3
    )


def test_identify_missing_field_fails_closed_without_repair(monkeypatch):
    payload = dict(GOOD_PAYLOAD)
    del payload["concept"]
    client = _install_fake(monkeypatch, [_response(json.dumps(payload))])
    result = server.identify(_data_uri())
    assert result["status"] == "error"
    assert result["error"]["code"] == "BAD_MODEL_RESPONSE"
    # A missing field is not the alternates defect: no repair call is made.
    assert len(client.completions.calls) == 1


def test_identify_missing_key_is_not_configured_with_no_ledger_write(monkeypatch):
    monkeypatch.delenv("META_API_KEY")
    # No fake installed: if the key check regressed and the call path ran,
    # build_client would raise NotConfiguredError AFTER a reservation was
    # written — the empty-ledger assertion below catches that ordering.
    result = server.identify(_data_uri())
    assert result["status"] == "error"
    assert result["error"]["code"] == "NOT_CONFIGURED"
    assert result["concept"] is None
    assert _records() == []
    assert not config.get_ledger_path().exists()


def test_identify_refuses_over_cap_before_any_api_call(monkeypatch):
    client = _install_fake(monkeypatch, [_response(_good_content())])
    ledger.append_receipt(
        config.get_ledger_path(),
        ledger.Receipt(tool="identify", model="muse-wonder-1.0", usd_estimate=200.00),
    )
    with pytest.raises(ledger.SpendCapExceeded):
        server.identify(_data_uri())
    assert client.completions.calls == []  # the gate refused before the call


# -- the alternates contract: repair, then error ------------------------------


def test_identify_alternates_shortfall_is_repaired_once(monkeypatch):
    first = _response(
        _good_content(alternates=["leather wallet"]),
        prompt_tokens=1000,
        completion_tokens=200,
    )
    repair = _response(
        json.dumps({"alternates": ["bifold wallet", "men's leather wallet"]}),
        prompt_tokens=100,
        completion_tokens=30,
    )
    client = _install_fake(monkeypatch, [first, repair])
    result = server.identify(_data_uri())

    assert result["status"] == "ok"
    assert len(client.completions.calls) == 2
    assert result["alternates"] == [
        "leather wallet",
        "bifold wallet",
        "men's leather wallet",
    ]
    # Usage from BOTH calls is summed onto the settled receipt.
    assert result["usage"]["input_tokens"] == 1100
    assert result["usage"]["output_tokens"] == 230


def test_identify_alternates_repair_failure_fails_closed(monkeypatch):
    first = _response(_good_content(alternates=[]))
    repair = _response(json.dumps({"alternates": []}))
    _install_fake(monkeypatch, [first, repair])
    result = server.identify(_data_uri())
    assert result["status"] == "error"
    assert result["error"]["code"] == "BAD_MODEL_RESPONSE"
    assert [r["phase"] for r in _records()] == ["pending"]


# -- key configuration ---------------------------------------------------------


def test_get_meta_api_key_reads_env_and_blank_is_none(monkeypatch):
    assert config.get_meta_api_key() == FAKE_KEY
    monkeypatch.setenv("META_API_KEY", "   ")
    assert config.get_meta_api_key() is None
    monkeypatch.delenv("META_API_KEY")
    assert config.get_meta_api_key() is None


def test_get_meta_api_key_loads_dotenv_without_overriding_env(tmp_path, monkeypatch):
    monkeypatch.delenv("META_API_KEY")
    monkeypatch.chdir(tmp_path)
    (tmp_path / ".env").write_text("META_API_KEY=from-dotenv-file\n", encoding="utf-8")
    monkeypatch.setattr(config, "_DOTENV_LOADED", False)
    try:
        assert config.get_meta_api_key() == "from-dotenv-file"
        # A real environment variable beats the file.
        monkeypatch.setenv("META_API_KEY", "from-env")
        assert config.get_meta_api_key() == "from-env"
    finally:
        os.environ.pop("META_API_KEY", None)


# -- segmentation-phrase hygiene: no whole-frame phrases ----------------------
#
# Regression for the dry-run gate failure (2026-10-27): identify emitted
# "phone screen" / "smartphone display" / "smartphone screenshot" for a
# photo that IS a screenshot. SAM can never segment the medium itself, so
# cut_out burned its bounded retries and failed stochastically. The plan
# is now constrained deterministically, whatever the model returns.


def test_frame_phrase_detection_names_the_medium_not_the_object():
    for phrase in (
        "phone screen",
        "smartphone display",
        "smartphone screenshot",
        "the whole scene",
        "computer monitor",
        "photo",
        "screens",
    ):
        assert meta_client._is_frame_phrase(phrase), phrase
    for phrase in (
        "brown leather wallet",
        "leather bifold wallet",
        "framed print",
        "smartphone",
        "cell phone",
        "wallet on a wooden table",
    ):
        assert not meta_client._is_frame_phrase(phrase), phrase


def test_frame_phrase_rewrite_drops_the_frame_head_noun():
    assert meta_client._rewrite_frame_phrase("phone screen") == "phone"
    assert meta_client._rewrite_frame_phrase("smartphone display") == "smartphone"
    assert meta_client._rewrite_frame_phrase("smartphone screenshot") == "smartphone"
    assert meta_client._rewrite_frame_phrase("the phone screen") == "phone"
    assert meta_client._rewrite_frame_phrase("screen") is None
    assert meta_client._rewrite_frame_phrase("screenshot") is None


def test_phrase_from_description_takes_the_opening_object_phrase():
    assert (
        meta_client._phrase_from_description(
            "A brown leather wallet on a wooden table."
        )
        == "brown leather wallet"
    )
    assert meta_client._phrase_from_description("Screenshot of a chat.") is None


@pytest.mark.parametrize(
    "bad_concept",
    ["phone screen", "smartphone display", "smartphone screenshot"],
)
def test_parse_replaces_frame_concept_with_object_alternate(bad_concept):
    payload = dict(GOOD_PAYLOAD)
    payload["concept"] = bad_concept
    payload["alternates"] = ["smartphone", "cell phone", "mobile phone"]
    plan = meta_client._parse_identify_payload(json.dumps(payload))
    # The frame concept is gone; the first object alternate is promoted
    # and no frame phrase survives anywhere in the plan. (For
    # "phone screen" the rewritten salvage "phone" may trail the
    # alternates; for the other two it rewrites to "smartphone" and
    # dedups against the promoted concept.)
    assert plan["concept"] == "smartphone"
    assert plan["alternates"][:2] == ["cell phone", "mobile phone"]
    assert all(not meta_client._is_frame_phrase(p) for p in [plan["concept"], *plan["alternates"]])


def test_parse_frame_concept_whose_rewrite_collides_is_a_repairable_shortfall():
    # "smartphone display" rewrites to "smartphone" — already the
    # promoted alternate — so only one distinct alternate survives and
    # the plan lands in the repairable shortfall, not in cut_out.
    payload = dict(GOOD_PAYLOAD)
    payload["concept"] = "smartphone display"
    payload["alternates"] = ["smartphone", "cell phone"]
    with pytest.raises(meta_client.IdentifyResultError) as excinfo:
        meta_client._parse_identify_payload(json.dumps(payload))
    assert str(excinfo.value).startswith("ALTERNATES_SHORTFALL")


def test_parse_all_frame_phrases_fall_back_to_rewrites():
    payload = dict(GOOD_PAYLOAD)
    payload["concept"] = "smartphone display"
    payload["alternates"] = ["smartphone screenshot", "mobile phone", "cellular phone"]
    plan = meta_client._parse_identify_payload(json.dumps(payload))
    assert plan["concept"] == "mobile phone"
    assert plan["alternates"] == ["cellular phone", "smartphone"]


def test_parse_object_phrases_pass_through_untouched():
    plan = meta_client._parse_identify_payload(_good_content())
    assert plan["concept"] == GOOD_PAYLOAD["concept"]
    assert plan["alternates"] == GOOD_PAYLOAD["alternates"]


def test_parse_flags_a_rewritten_frame_concept():
    # Fable's case from the architecture review: a REAL product whose
    # name contains a frame noun — "computer monitor" — is rewritten
    # to "computer". The rewrite itself is the plan contract, but it
    # must not be silent: the plan names what the model actually said.
    payload = dict(GOOD_PAYLOAD)
    payload["concept"] = "computer monitor"
    payload["alternates"] = ["desktop monitor", "pc screen"]
    plan = meta_client._parse_identify_payload(json.dumps(payload))
    assert plan["concept"] == "computer"
    assert plan["concept_rewritten_from"] == "computer monitor"


def test_parse_flags_a_promoted_over_frame_concept():
    payload = dict(GOOD_PAYLOAD)
    payload["concept"] = "phone screen"
    payload["alternates"] = ["smartphone", "cell phone", "mobile phone"]
    plan = meta_client._parse_identify_payload(json.dumps(payload))
    assert plan["concept"] == "smartphone"
    assert plan["concept_rewritten_from"] == "phone screen"


def test_parse_object_concept_has_no_rewrite_flag():
    plan = meta_client._parse_identify_payload(_good_content())
    assert plan["concept_rewritten_from"] is None


def test_identify_result_surfaces_the_rewrite_flag(monkeypatch):
    client = _install_fake(
        monkeypatch,
        [
            _response(
                _good_content(
                    concept="phone screen",
                    alternates=["smartphone", "cell phone"],
                )
            )
        ],
    )
    result = server.identify(_data_uri())
    assert result["status"] == "ok"
    assert result["concept"] == "smartphone"
    assert result["concept_rewritten_from"] == "phone screen"
    assert len(client.completions.calls) == 1


def test_sanitize_falls_back_to_the_description_when_nothing_survives():
    concept, alternates = meta_client._sanitize_plan_phrases(
        "screen", ["display"], "A brown leather wallet on a wooden table."
    )
    assert concept == "brown leather wallet"
    assert alternates == []


def test_identify_frame_concept_is_promoted_over_without_a_repair_call(monkeypatch):
    client = _install_fake(
        monkeypatch,
        [
            _response(
                _good_content(
                    concept="phone screen",
                    alternates=["smartphone", "cell phone"],
                )
            )
        ],
    )
    result = server.identify(_data_uri())
    assert result["status"] == "ok"
    assert result["concept"] == "smartphone"
    assert result["alternates"] == ["cell phone", "phone"]
    # No repair call was needed: the plan was fixed deterministically.
    assert len(client.completions.calls) == 1


def test_identify_all_frame_plan_is_rescued_by_the_repair_call(monkeypatch):
    first = _response(
        _good_content(
            concept="smartphone screenshot",
            alternates=["smartphone display", "phone screen"],
        )
    )
    repair = _response(json.dumps({"alternates": ["smartphone", "mobile phone"]}))
    client = _install_fake(monkeypatch, [first, repair])
    result = server.identify(_data_uri())
    assert result["status"] == "ok"
    assert len(client.completions.calls) == 2
    assert result["concept"] == "smartphone"
    assert "mobile phone" in result["alternates"]
    assert all(
        not meta_client._is_frame_phrase(p)
        for p in [result["concept"], *result["alternates"]]
    )
