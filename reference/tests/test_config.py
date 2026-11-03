"""Spend-cap configuration: ceiling, default, fail-closed parsing."""

import pytest

from agenthand import config


def test_default_cap_is_the_working_cap(monkeypatch):
    monkeypatch.delenv("AGENTHAND_TOTAL_SPEND_CAP_USD", raising=False)
    assert config.get_total_spend_cap_usd() == 120.00
    assert config.DEFAULT_TOTAL_SPEND_CAP_USD == 120.00
    assert config.HARD_CEILING_USD == 150.00


def test_env_can_lower_the_cap(monkeypatch):
    monkeypatch.setenv("AGENTHAND_TOTAL_SPEND_CAP_USD", "25.50")
    assert config.get_total_spend_cap_usd() == 25.50


def test_env_cannot_raise_the_cap_past_the_ceiling(monkeypatch):
    monkeypatch.setenv("AGENTHAND_TOTAL_SPEND_CAP_USD", "99999")
    assert config.get_total_spend_cap_usd() == 150.00


@pytest.mark.parametrize("bad", ["nan", "inf", "-inf", "abc", "1.2.3"])
def test_uninterpretable_cap_fails_closed(monkeypatch, bad):
    monkeypatch.setenv("AGENTHAND_TOTAL_SPEND_CAP_USD", bad)
    with pytest.raises(ValueError):
        config.get_total_spend_cap_usd()


def test_negative_cap_fails_closed(monkeypatch):
    monkeypatch.setenv("AGENTHAND_TOTAL_SPEND_CAP_USD", "-5")
    with pytest.raises(ValueError):
        config.get_total_spend_cap_usd()


def test_ledger_path_defaults_inside_the_data_dir(monkeypatch, tmp_path):
    monkeypatch.delenv("AGENTHAND_LEDGER_PATH", raising=False)
    monkeypatch.delenv("AGENTHAND_LEDGER", raising=False)
    monkeypatch.setenv("AGENTHAND_DATA_DIR", str(tmp_path))
    assert config.get_ledger_path() == tmp_path / "ledger.jsonl"
    assert config.get_cumulative_ledger_path() == tmp_path / "ledger.jsonl"
    assert config.get_mirror_ledger_path() is None


def test_cumulative_ledger_override_separates_ledger_from_data_dir(
    monkeypatch, tmp_path
):
    # The dry-run gate isolates its asset store in a temp dir but must
    # spend against the REAL ledger, or gate runs reset the cumulative
    # cap accounting (architecture Gauntlet fix 3). Since fix pass F4
    # that is AGENTHAND_LEDGER=jsonl:<path> (AGENTHAND_LEDGER_PATH only
    # names a mirror).
    data_dir = tmp_path / "store"
    real_ledger = tmp_path / "real" / "ledger.jsonl"
    monkeypatch.delenv("AGENTHAND_LEDGER_PATH", raising=False)
    monkeypatch.setenv("AGENTHAND_DATA_DIR", str(data_dir))
    monkeypatch.setenv("AGENTHAND_LEDGER", f"jsonl:{real_ledger}")
    assert config.get_cumulative_ledger_path() == real_ledger
    assert config.get_ledger_path() == real_ledger


def test_mirror_ledger_does_not_relocate_the_cumulative_ledger(monkeypatch, tmp_path):
    # Fix pass F4: AGENTHAND_LEDGER_PATH (the room runner's room ledger)
    # names a mirror; the cap ledger stays the cumulative one.
    room = tmp_path / "room" / "ledger.jsonl"
    monkeypatch.delenv("AGENTHAND_LEDGER", raising=False)
    monkeypatch.setenv("AGENTHAND_DATA_DIR", str(tmp_path / "data"))
    monkeypatch.setenv("AGENTHAND_LEDGER_PATH", str(room))
    assert config.get_cumulative_ledger_path() == tmp_path / "data" / "ledger.jsonl"
    assert config.get_mirror_ledger_path() == room
    assert config.get_ledger_path() == room  # this context's own receipts


@pytest.mark.parametrize("value", ["jsonl:", "jsonl:   ", "sqlite", "sqlite:/x.db", "postgres"])
def test_unusable_ledger_store_fails_closed(monkeypatch, tmp_path, value):
    monkeypatch.setenv("AGENTHAND_DATA_DIR", str(tmp_path))
    monkeypatch.setenv("AGENTHAND_LEDGER", value)
    with pytest.raises(ValueError, match="AGENTHAND_LEDGER"):
        config.get_cumulative_ledger_path()


def test_plain_jsonl_store_is_the_data_dir_ledger(monkeypatch, tmp_path):
    monkeypatch.setenv("AGENTHAND_DATA_DIR", str(tmp_path))
    monkeypatch.setenv("AGENTHAND_LEDGER", "jsonl")
    assert config.get_cumulative_ledger_path() == tmp_path / "ledger.jsonl"
