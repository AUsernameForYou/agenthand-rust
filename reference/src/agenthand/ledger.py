"""Cost receipts and the fail-closed spend cap.

Every billable call writes receipts to a local JSONL ledger. The billing
choke point (meta_client.billed_call) uses reserve-then-settle accounting:

- Before the call it appends a "pending" receipt at the projected
  worst-case cost, tagged with a call_id.
- After the call it appends a "settled" receipt with the same call_id and
  the actual cost computed from the provider's usage counts.
- If the call raises, no settlement is written, and the pending
  reservation stays counted at its projected cost. Money that might have
  been spent is treated as spent.

ledger_total_usd therefore counts, per call_id, the settled actual when a
settlement exists and the pending projection otherwise. Receipts without a
call_id (phase "final") count at face value.

Settle is capped too (fix pass F5): a reservation is a projection, and
a call can cost more than projected. At settle, billed_call recomputes
everything else in the ledger (settled actuals plus other open
reservations, excluding the call's own rows) plus the call's actual
cost. Over the cap, the settle is STILL recorded truthfully, with
status "over_cap" and the overage in its note, billed_call raises
SpendCapExceededAtSettle naming the breach, and every later
reservation is refused because the ledger total exceeds the cap.

Fail-closed means exactly that: a missing ledger reads as $0.00 (nothing
has been spent yet), but a ledger line that cannot be parsed, or whose
amount is negative or non-finite, raises. When spend state is unknown or
invalid, the server does not spend.

Durability: appends are flushed and fsynced before append_receipt
returns, so a crash or power loss cannot silently drop a receipt the
caller believes is recorded. A crash DURING the write itself can still
tear the final line — and reads deliberately do not tolerate that: a
torn line's amount is unknowable, and a torn pending reservation, if
skipped, would understate spend exactly when the cap matters most. A
torn tail therefore keeps failing closed (the operator repairs the
file) rather than risking an undercount.
"""

from __future__ import annotations

import json
import math
import os
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path

from . import pricing

# Receipt phases.
PHASE_FINAL = "final"      # one-shot receipt; counts at face value
PHASE_PENDING = "pending"  # reservation written before a billable call
PHASE_SETTLED = "settled"  # actual cost written after the call returns

# Receipt status of a settle whose actual cost pushed the ledger over
# the spend cap (fix pass F5). The amount is recorded truthfully and
# counts like any other settle; the status (and the overage in the
# note) makes the breach visible in the ledger itself.
STATUS_OVER_CAP = "over_cap"


class SpendCapExceeded(RuntimeError):
    """Raised when a billable call would push total spend past the cap,
    or when spend state is invalid and no call may proceed."""


class SpendCapExceededAtSettle(SpendCapExceeded):
    """Raised AFTER a call ran, when its actual cost pushed the ledger
    over the cap (fix pass F5). The settle is already recorded
    (status over_cap); the call happened and its cost is counted.
    Every later reservation is refused."""

    def __init__(
        self,
        message: str,
        *,
        call_id: str,
        projected_usd: float,
        actual_usd: float,
        total_usd: float,
        overage_usd: float,
        cap_usd: float,
    ):
        super().__init__(message)
        self.call_id = call_id
        self.projected_usd = projected_usd
        self.actual_usd = actual_usd
        self.total_usd = total_usd
        self.overage_usd = overage_usd
        self.cap_usd = cap_usd


@dataclass(frozen=True)
class SettleOverage:
    """A settle-time cap breach: the ledger total after the settle, how
    far over the cap that is, and the cap."""

    total_usd: float
    overage_usd: float
    cap_usd: float


@dataclass
class Receipt:
    tool: str
    model: str | None
    usd_estimate: float
    pricing_table_version: str = pricing.PRICING_TABLE_VERSION
    input_tokens: int = 0
    output_tokens: int = 0
    cached_input_tokens: int = 0
    images: int = 0
    status: str = "ok"  # "ok" for real calls, "stub" while tools are unwired
    phase: str = PHASE_FINAL
    call_id: str | None = None  # links a pending reservation to its settlement
    note: str = ""
    timestamp: str = field(
        default_factory=lambda: datetime.now(timezone.utc).isoformat()
    )

    def to_json_line(self) -> str:
        return json.dumps(asdict(self), separators=(",", ":"))


def _validate_amount(value: object, where: str) -> float:
    """A ledger amount must be a finite, non-negative number.

    NaN is a real hazard here: JSON parses the literal NaN, and every
    comparison against NaN is False, which would silently disable the cap.
    Negative amounts would let a crafted line refund the budget. Both fail
    closed.
    """
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise SpendCapExceeded(f"{where}: amount {value!r} is not a number.")
    amount = float(value)
    if not math.isfinite(amount):
        raise SpendCapExceeded(f"{where}: amount {value!r} is not finite.")
    if amount < 0:
        raise SpendCapExceeded(f"{where}: amount {value!r} is negative.")
    return amount


def append_receipt(ledger_path: Path, receipt: Receipt) -> None:
    _validate_amount(receipt.usd_estimate, f"receipt for {receipt.tool!r}")
    ledger_path.parent.mkdir(parents=True, exist_ok=True)
    with ledger_path.open("a", encoding="utf-8") as fh:
        fh.write(receipt.to_json_line() + "\n")
        # Flush + fsync before returning: once a receipt is written,
        # a crash must not be able to un-spend it. (A crash mid-write
        # can still tear this line; ledger_total_usd fails closed on
        # it rather than guess at the amount — see module docstring.)
        fh.flush()
        os.fsync(fh.fileno())


def ledger_total_usd(ledger_path: Path) -> float:
    """Total recorded spend. Missing ledger = 0.0; invalid ledger = raise.

    Per call_id, a settled receipt supersedes its pending reservation;
    an unsettled reservation counts at its projected cost.
    """
    return _ledger_total(ledger_path, None)


def ledger_total_excluding_call_usd(ledger_path: Path, call_id: str) -> float:
    """The counted ledger total with every row of `call_id` left out:
    the settle-time cap check's "everything else" (fix pass F5)."""
    return _ledger_total(ledger_path, call_id)


def _ledger_total(ledger_path: Path, exclude_call: str | None) -> float:
    if not ledger_path.exists():
        return 0.0
    standalone = 0.0
    calls: dict[str, float] = {}
    settled: set[str] = set()
    with ledger_path.open("r", encoding="utf-8") as fh:
        for lineno, line in enumerate(fh, start=1):
            line = line.strip()
            if not line:
                continue
            where = f"Ledger {ledger_path} line {lineno}"
            try:
                record = json.loads(line)
            except json.JSONDecodeError as exc:
                raise SpendCapExceeded(
                    f"{where} is unreadable ({exc}); "
                    "refusing to spend with unknown spend state."
                ) from exc
            if not isinstance(record, dict) or "usd_estimate" not in record:
                raise SpendCapExceeded(
                    f"{where} has no usd_estimate; "
                    "refusing to spend with unknown spend state."
                )
            amount = _validate_amount(record["usd_estimate"], where)
            call_id = record.get("call_id")
            phase = record.get("phase", PHASE_FINAL)
            if exclude_call is not None and call_id == exclude_call:
                continue
            if call_id and phase == PHASE_SETTLED:
                calls[call_id] = amount
                settled.add(call_id)
            elif call_id and phase == PHASE_PENDING:
                if call_id not in settled and call_id not in calls:
                    calls[call_id] = amount
            else:
                standalone += amount
    return round(standalone + sum(calls.values()), 6)


def _round6_half_away(x: float) -> float:
    """Round to the micro-dollar the way the Rust build does
    ((x * 1e6).round() / 1e6, ties away from zero), so both builds
    compute the same over_cap totals and notes."""
    v = x * 1e6
    f = math.floor(abs(v))
    r = f + 1.0 if abs(v) - f >= 0.5 else f
    return math.copysign(r, v) / 1e6


def apply_settle_cap(receipt: Receipt, others_usd: float, cap_usd: float) -> SettleOverage | None:
    """The settle-time cap check (fix pass F5): `others_usd` is the
    ledger total EXCLUDING this call's own rows (every other settled
    actual plus every other open reservation). When others + actual
    exceeds the cap, the receipt is marked over_cap with the overage
    in its note, and the breach returned; the caller still records the
    receipt (truthfully) and then fails. Same rule and wording as the
    Rust build's ledger::apply_settle_cap."""
    total = _round6_half_away(others_usd + receipt.usd_estimate)
    if total <= cap_usd:
        return None
    overage = _round6_half_away(total - cap_usd)
    flag = (
        f"over_cap: settled ${receipt.usd_estimate:.6f} brought the ledger to "
        f"${total:.6f}, ${overage:.6f} over the ${cap_usd:.2f} cap"
    )
    receipt.status = STATUS_OVER_CAP
    receipt.note = f"{receipt.note}; {flag}" if receipt.note else flag
    return SettleOverage(total_usd=total, overage_usd=overage, cap_usd=cap_usd)


def check_spend_cap(ledger_path: Path, projected_usd: float, cap_usd: float) -> float:
    """Refuse a billable call that would exceed the cap.

    Returns the current ledger total when the call is allowed. Raises
    SpendCapExceeded when ledger total + projected cost would exceed the
    cap (landing exactly on the cap is allowed), or when the projection or
    the cap itself is not a finite, non-negative number.
    """
    projected = _validate_amount(projected_usd, "projected cost")
    cap = _validate_amount(cap_usd, "spend cap")
    total = ledger_total_usd(ledger_path)
    if total + projected > cap:
        raise SpendCapExceeded(
            f"Spend cap: ledger total ${total:.2f} + projected ${projected:.4f} "
            f"would exceed the ${cap:.2f} cap. Call refused before it ran."
        )
    return total
