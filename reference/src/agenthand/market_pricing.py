"""Market pricing from agent-supplied Marketplace comps (pure, offline).

The agent supplies comps it pulled from Facebook Marketplace as JSON:

    {"radius_mi": 40, "area": "Camas, WA", "pulled_at": "<iso>",
     "items": {"<item name>": {"comps": [
         {"title": str, "price_usd": number, "condition": str | None,
          "distance_mi": number | None, "age_days": number | None,
          "is_new": bool}]}}}

This module turns those asking prices into a per-item price record
with a deterministic rule, shared bit-for-bit with the sibling
implementation:

- Usable comps are the ones whose ``is_new`` is falsy and whose
  ``price_usd`` is a number. Their prices are sorted ascending;
  n is the usable count.
- n < 2 is a thin market: no range, no suggestion, no confidence.
- Percentiles use linear interpolation (type-7): for percentile p,
  h = p * (n - 1) and the value interpolates between the floor and
  ceil ranks. quick = p25, market = p50, premium = p75.
- Each percentile is rounded to the nearest $5 (halves away from
  zero) with a $5 floor. The suggestion is the rounded market value.
- Confidence is capped by the usable count, because the inputs are
  asking prices, not sold prices: n >= 8 "moderate", 4-7
  "low-moderate", 2-3 "low".

Item matching is exact on the item name, case-insensitive, with
surrounding whitespace trimmed. An item with no entry in the comps
file (or an entry with no comps at all) is "no_comps_supplied"; an
entry whose usable count is under 2 is "unpriced_thin_market".
"""

from __future__ import annotations

import math

PRICING_SOURCE = "facebook_marketplace_comps"

STATUS_PRICED = "priced"
STATUS_THIN = "unpriced_thin_market"
STATUS_NO_COMPS = "no_comps_supplied"


def normalize_name(name: str | None) -> str:
    """The matching key for an item name: trimmed, case-insensitive."""
    return (name or "").strip().lower()


def round_to_nearest_5(value: float) -> int:
    """Round to the nearest $5, halves away from zero, floored at $5."""
    magnitude = math.floor(abs(value) / 5.0 + 0.5) * 5
    rounded = -magnitude if value < 0 else magnitude
    return max(5, int(rounded))


def percentile_type7(sorted_values: list[float], p: float) -> float:
    """Linear-interpolation percentile over pre-sorted values."""
    n = len(sorted_values)
    if n == 0:
        raise ValueError("percentile of an empty list")
    if n == 1:
        return float(sorted_values[0])
    h = p * (n - 1)
    low = math.floor(h)
    high = math.ceil(h)
    frac = h - low
    return float(sorted_values[low] + frac * (sorted_values[high] - sorted_values[low]))


def confidence_for_count(n: int) -> str | None:
    """The confidence label for a usable-comp count (None when unpriced)."""
    if n >= 8:
        return "moderate"
    if n >= 4:
        return "low-moderate"
    if n >= 2:
        return "low"
    return None


def usable_prices(comps: list | None) -> list[float]:
    """Sorted usable asking prices from one item's raw comp list."""
    prices: list[float] = []
    for comp in comps or []:
        if not isinstance(comp, dict):
            continue
        if comp.get("is_new"):
            continue
        price = comp.get("price_usd")
        if isinstance(price, bool) or not isinstance(price, (int, float)):
            continue
        prices.append(float(price))
    return sorted(prices)


def price_from_prices(prices: list[float]) -> dict:
    """The core pricing outcome for a sorted usable-price list."""
    n = len(prices)
    if n < 2:
        return {
            "status": STATUS_THIN,
            "comps_used": n,
            "range": None,
            "suggested": None,
            "confidence": None,
        }
    quick = round_to_nearest_5(percentile_type7(prices, 0.25))
    market = round_to_nearest_5(percentile_type7(prices, 0.50))
    premium = round_to_nearest_5(percentile_type7(prices, 0.75))
    return {
        "status": STATUS_PRICED,
        "comps_used": n,
        "range": {"quick": quick, "market": market, "premium": premium},
        "suggested": market,
        "confidence": confidence_for_count(n),
    }


def find_comps(item_name: str, comps_data: dict | None) -> list | None:
    """The raw comp list supplied for an item, or None when not supplied."""
    if not isinstance(comps_data, dict):
        return None
    items = comps_data.get("items")
    if not isinstance(items, dict):
        return None
    wanted = normalize_name(item_name)
    for key, entry in items.items():
        if normalize_name(key) != wanted:
            continue
        if not isinstance(entry, dict):
            return None
        comps = entry.get("comps")
        return comps if isinstance(comps, list) else None
    return None


def pricing_for_item(item_name: str, comps_data: dict | None) -> dict:
    """One item's manifest pricing record.

    The record always carries the source label plus the pull context
    (``pulled_at`` / ``radius_mi`` from the comps file, None when no
    comps file was supplied at all).
    """
    record = {
        "status": STATUS_NO_COMPS,
        "comps_used": 0,
        "range": None,
        "suggested": None,
        "confidence": None,
        "source": PRICING_SOURCE,
        "pulled_at": comps_data.get("pulled_at") if isinstance(comps_data, dict) else None,
        "radius_mi": comps_data.get("radius_mi") if isinstance(comps_data, dict) else None,
    }
    comps = find_comps(item_name, comps_data)
    if not comps:
        return record
    record.update(price_from_prices(usable_prices(comps)))
    return record
