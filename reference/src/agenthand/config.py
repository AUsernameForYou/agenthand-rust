"""Runtime configuration for agenthand.

Everything is read from the environment at call time (no caching), so tests
and wrappers can point the server at a scratch data dir or a different spend
cap simply by setting variables before a call.
"""

from __future__ import annotations

import math
import os
from pathlib import Path

# The hackathon credit amount, and the absolute ceiling for the spend cap.
# This is the ONLY authorized Meta Model API spend unless Daniel raises it
# explicitly; the environment variable below may only ever LOWER the cap
# toward zero, never raise it past this ceiling.
HARD_CEILING_USD = 150.00

# The working cap the server actually runs with by default: below the
# credit amount, so estimate drift cannot spend the last of the credits
# and reach the card on file.
DEFAULT_TOTAL_SPEND_CAP_USD = 120.00


def get_data_dir() -> Path:
    """Local data dir holding the asset store and the cost ledger."""
    _load_dotenv()
    raw = os.environ.get("AGENTHAND_DATA_DIR", "").strip()
    if raw:
        return Path(raw).expanduser()
    return Path(__file__).resolve().parents[2] / "data"


def get_cumulative_ledger_path() -> Path:
    """The CUMULATIVE ledger: where meta_client.billed_call reserves,
    settles, and checks the spend cap (fix pass F4).

    Selected by AGENTHAND_LEDGER, the same knob as the Rust build's
    ledger store: unset or ``jsonl`` means ``ledger.jsonl`` inside the
    data dir; ``jsonl:<path>`` moves the cumulative ledger to <path>
    (``~`` expands). The Python build keeps a JSONL ledger only, so
    ``sqlite`` / ``sqlite:<path>`` (the Rust build's SQLite store) and
    any other value raise ValueError: a store this build cannot check
    the cap against must stop spend, not silently pick a different one.

    Every billed call, room runs included, reserves and settles here,
    so the cap is checked against all spend, never against one run's
    slice. AGENTHAND_LEDGER_PATH does NOT relocate this ledger (it
    names a mirror; see get_mirror_ledger_path). Before F4 it did, and
    the room runner pinned it to the room's own file, so every room
    run started at $0 against a full cap. The dry-run gate isolates
    its asset store in a temp data dir and points this ledger at the
    real one with ``AGENTHAND_LEDGER=jsonl:<path>``, so gate spend
    counts toward the same cumulative cap (architecture Gauntlet fix 3).
    """
    _load_dotenv()
    raw = os.environ.get("AGENTHAND_LEDGER", "").strip()
    if raw in ("", "jsonl"):
        return get_data_dir() / "ledger.jsonl"
    if raw.startswith("jsonl:"):
        path = raw[len("jsonl:") :].strip()
        if not path:
            raise ValueError(
                "AGENTHAND_LEDGER=jsonl: has an empty path; refusing to "
                "spend with an unknown ledger store."
            )
        return Path(path).expanduser()
    if raw == "sqlite" or raw.startswith("sqlite:"):
        raise ValueError(
            f"AGENTHAND_LEDGER={raw!r} selects the SQLite ledger store, "
            "which only the Rust build has; the Python build keeps its "
            "cumulative ledger as JSONL (jsonl or jsonl:<path>). Refusing "
            "to spend against a ledger this build cannot check."
        )
    raise ValueError(
        f"AGENTHAND_LEDGER={raw!r} is not understood (expected jsonl or "
        "jsonl:<path>); refusing to spend with an unknown ledger store."
    )


def get_mirror_ledger_path() -> Path | None:
    """The mirror ledger (fix pass F4), or None.

    AGENTHAND_LEDGER_PATH, when set and non-empty (``~`` expands),
    names a JSONL file that meta_client.billed_call copies every
    pending and settled row into, IN ADDITION to the cumulative
    ledger. The room runner points it at the room's own ledger, so a
    run's cost stays provable from the room directory alone (room
    totals read back from that file), while every reservation is
    checked against the one cumulative ledger, so the cap cannot
    reset per run. Before F4 this variable REPLACED the ledger path,
    which let each room run start at $0.
    """
    _load_dotenv()
    raw = os.environ.get("AGENTHAND_LEDGER_PATH", "").strip()
    if raw:
        return Path(raw).expanduser()
    return None


def get_ledger_path() -> Path:
    """The JSONL ledger this context reads its own receipts from: the
    mirror ledger (AGENTHAND_LEDGER_PATH) when one is set, otherwise
    the cumulative ledger. This is NOT where the spend cap is checked
    when a mirror is set: the cap always runs against the cumulative
    ledger (get_cumulative_ledger_path)."""
    mirror = get_mirror_ledger_path()
    if mirror is not None:
        return mirror
    return get_cumulative_ledger_path()


def get_total_spend_cap_usd() -> float:
    """The working spend cap in USD.

    AGENTHAND_TOTAL_SPEND_CAP_USD may lower the cap (down to 0). A value
    above the hard ceiling is clamped to the ceiling. A value that is
    unparseable, non-finite (NaN/inf), or negative raises ValueError:
    a cap we cannot interpret must stop spend, not wave it through.
    """
    _load_dotenv()
    raw = os.environ.get("AGENTHAND_TOTAL_SPEND_CAP_USD", "").strip()
    if not raw:
        return DEFAULT_TOTAL_SPEND_CAP_USD
    try:
        value = float(raw)
    except ValueError as exc:
        raise ValueError(
            f"AGENTHAND_TOTAL_SPEND_CAP_USD={raw!r} is not a number; "
            "refusing to run with an uninterpretable spend cap."
        ) from exc
    if not math.isfinite(value):
        raise ValueError(
            f"AGENTHAND_TOTAL_SPEND_CAP_USD={raw!r} is not finite; "
            "refusing to run with an unbounded spend cap."
        )
    if value < 0:
        raise ValueError(
            f"AGENTHAND_TOTAL_SPEND_CAP_USD={raw!r} is negative; "
            "a spend cap cannot be negative."
        )
    return min(value, HARD_CEILING_USD)


_DOTENV_LOADED = False


def _load_dotenv() -> None:
    """Load KEY=VALUE pairs from a local .env file into os.environ.

    The current directory and the project root are checked. A variable
    already present in the environment always wins: the file never
    overwrites it. Loaded at most once per process. The .env file is
    operator-local state (it is gitignored) and is where the Meta API key
    lives on a dev machine; committed code never carries key material.
    """
    global _DOTENV_LOADED
    if _DOTENV_LOADED:
        return
    _DOTENV_LOADED = True
    candidates = [Path.cwd() / ".env", Path(__file__).resolve().parents[2] / ".env"]
    seen: set[Path] = set()
    for path in candidates:
        if path in seen or not path.is_file():
            continue
        seen.add(path)
        for line in path.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            name, _, value = line.partition("=")
            name = name.strip()
            value = value.strip().strip('"').strip("'")
            if name:
                os.environ.setdefault(name, value)


def get_comps_file() -> Path:
    """The category comps file the pricing post-pass reads.

    AGENTHAND_COMPS_FILE when set and non-empty (``~`` expands), else
    ``pricing-comps.json`` inside the data dir. The file is operator
    data, never committed: a missing or malformed file degrades
    pricing to a noted no_comps block, never an error (see comps.py).
    """
    _load_dotenv()
    raw = os.environ.get("AGENTHAND_COMPS_FILE", "").strip()
    if raw:
        return Path(raw).expanduser()
    return get_data_dir() / "pricing-comps.json"


def get_meta_api_key() -> str | None:
    """The Meta Model API key, from META_API_KEY (environment or .env).

    Returns None when no key is configured. Callers that need to make a
    real API call must treat None as "not configured" and refuse before
    any money moves; the key value itself is never logged or stored.
    """
    _load_dotenv()
    raw = os.environ.get("META_API_KEY", "").strip()
    return raw or None


def use_placeholder_plate() -> bool:
    """True when draw may use the local placeholder plate (dev only).

    AGENTHAND_DRAW_PLACEHOLDER_PLATE=1 (also true/yes/on) puts draw in
    its offline mode: the background plate is the deterministic local
    placeholder from composite.py and no image-model call is made. This
    exists ONLY for offline tests and no-key demos. Live mode is the
    default: without the flag, draw calls muse-image-1.0 and never
    silently substitutes the placeholder for a real plate.
    """
    _load_dotenv()
    raw = os.environ.get("AGENTHAND_DRAW_PLACEHOLDER_PLATE", "").strip().lower()
    return raw in ("1", "true", "yes", "on")
