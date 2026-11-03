"""The single pricing table for Meta Model API usage.

All USD estimates in the cost ledger come from here. When Meta changes a
price, edit the number and bump PRICING_TABLE_VERSION; old receipts keep the
version they were written with, so historical spend stays auditable.

Prices verified against the Meta Model API pricing page, 2026-10-26.

Contract notes (hardened at the J1 Gauntlet):
- An unknown model RAISES. Pricing a model we do not know at $0 would let
  real spend sail past the spend cap unrecorded, so there is no silent
  fallback.
- input_tokens is the TOTAL input token count, and cached_input_tokens is
  the cached SUBSET of it (the convention the OpenAI-compatible usage
  object uses). Cached tokens are billed at the cached rate; the rest of
  the input is billed at the fresh rate. Passing only one of the two is
  fine; passing a cached count larger than the input count is a caller bug
  and raises.
- All counts must be non-negative; negative usage raises.
"""

from __future__ import annotations

PRICING_TABLE_VERSION = "2026-10-26.2"

# USD per 1M tokens.
TOKEN_PRICES_USD_PER_1M: dict[str, dict[str, float]] = {
    "muse-spark-1.3": {"input": 1.25, "output": 4.25, "cached_input": 0.15},
    "muse-wonder-1.0": {"input": 4.00, "output": 20.00, "cached_input": 0.20},
    # Baseten Spark Open, the narration model (roomrun.NARRATION_MODEL):
    # the published $1.25/M in, $4.25/M out that
    # roomrun.narration_cost_usd and the narration projection already
    # price at. Baseten reports no cached tokens; any that ever appear
    # are priced as fresh input (no discount assumed), so the ledger
    # never under-counts. Added in fix pass F3 so narration can run
    # inside meta_client.billed_call; no existing rate changed, so the
    # table version (shared with the Rust build) is unchanged.
    "meta-models/muse-spark-open-1026": {
        "input": 1.25,
        "output": 4.25,
        "cached_input": 1.25,
    },
}

# USD per generated image.
PER_IMAGE_PRICES_USD: dict[str, float] = {
    "muse-image-1.0": 0.01,
}

# USD per 1,000 processed images.
PER_1000_IMAGES_PRICES_USD: dict[str, float] = {
    "sam-3.1": 2.50,
}


class UnknownModelError(ValueError):
    """Raised when a cost estimate is requested for an unpriced model."""


def is_known_model(model: str) -> bool:
    return (
        model in TOKEN_PRICES_USD_PER_1M
        or model in PER_IMAGE_PRICES_USD
        or model in PER_1000_IMAGES_PRICES_USD
    )


def _check_count(name: str, value: int) -> None:
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise ValueError(f"{name} must be a non-negative integer, got {value!r}")


def estimate_cost_usd(
    model: str,
    *,
    input_tokens: int = 0,
    output_tokens: int = 0,
    cached_input_tokens: int = 0,
    images: int = 0,
) -> float:
    """Estimate the USD cost of one model call.

    Token-priced models take token counts. input_tokens is the total input;
    cached_input_tokens is the cached subset of that input, billed at the
    cached rate instead of the fresh rate. Per-image and per-1000-image
    models take an image count. A model present in more than one table
    would be charged under each; none currently is.

    Raises UnknownModelError for a model in no table, and ValueError for
    negative counts or a cached count larger than the input count.
    """
    if not is_known_model(model):
        raise UnknownModelError(
            f"No price for model {model!r}; refusing to estimate it as $0. "
            "Add it to the pricing table (with a version bump) first."
        )
    _check_count("input_tokens", input_tokens)
    _check_count("output_tokens", output_tokens)
    _check_count("cached_input_tokens", cached_input_tokens)
    _check_count("images", images)
    if cached_input_tokens > input_tokens:
        raise ValueError(
            f"cached_input_tokens ({cached_input_tokens}) cannot exceed "
            f"input_tokens ({input_tokens}); cached tokens are a subset of input."
        )

    total = 0.0

    token_prices = TOKEN_PRICES_USD_PER_1M.get(model)
    if token_prices is not None:
        fresh_input = input_tokens - cached_input_tokens
        total += fresh_input * token_prices["input"] / 1_000_000
        total += cached_input_tokens * token_prices["cached_input"] / 1_000_000
        total += output_tokens * token_prices["output"] / 1_000_000

    if model in PER_IMAGE_PRICES_USD:
        total += images * PER_IMAGE_PRICES_USD[model]

    if model in PER_1000_IMAGES_PRICES_USD:
        total += images * PER_1000_IMAGES_PRICES_USD[model] / 1000

    return round(total, 6)
