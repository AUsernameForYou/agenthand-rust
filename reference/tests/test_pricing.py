"""Pricing table math, pinned to the verified Meta Model API prices."""

import pytest

from agenthand import pricing


def test_spark_token_prices():
    # 1M fresh input + 1M output on muse-spark-1.3 = $1.25 + $4.25.
    assert pricing.estimate_cost_usd(
        "muse-spark-1.3", input_tokens=1_000_000, output_tokens=1_000_000
    ) == 5.50


def test_spark_cached_input_is_cheaper():
    # All 1M input tokens cached: billed entirely at the cached rate.
    assert pricing.estimate_cost_usd(
        "muse-spark-1.3", input_tokens=1_000_000, cached_input_tokens=1_000_000
    ) == 0.15


def test_cached_tokens_are_a_subset_of_input():
    # 1M input, half of it cached: 500k fresh at $1.25/1M + 500k cached at
    # $0.15/1M = $0.625 + $0.075 = $0.70. Billing the cached half at the
    # fresh rate as well (double count) would wrongly give $1.325.
    assert pricing.estimate_cost_usd(
        "muse-spark-1.3", input_tokens=1_000_000, cached_input_tokens=500_000
    ) == 0.70


def test_wonder_token_prices():
    assert pricing.estimate_cost_usd(
        "muse-wonder-1.0", input_tokens=1_000_000, output_tokens=1_000_000
    ) == 24.00
    assert pricing.estimate_cost_usd(
        "muse-wonder-1.0", input_tokens=1_000_000, cached_input_tokens=1_000_000
    ) == 0.20


def test_image_model_per_image():
    assert pricing.estimate_cost_usd("muse-image-1.0", images=3) == 0.03


def test_sam_per_thousand_images():
    assert pricing.estimate_cost_usd("sam-3.1", images=1000) == 2.50
    assert pricing.estimate_cost_usd("sam-3.1", images=1) == 0.0025


def test_unknown_model_raises():
    # An unpriced model must never silently estimate as $0: that is how
    # real spend would slip past the cap unrecorded.
    with pytest.raises(pricing.UnknownModelError):
        pricing.estimate_cost_usd("no-such-model", input_tokens=10**9)
    assert pricing.is_known_model("muse-spark-1.3")
    assert not pricing.is_known_model("no-such-model")


def test_cached_cannot_exceed_input():
    with pytest.raises(ValueError):
        pricing.estimate_cost_usd(
            "muse-spark-1.3", input_tokens=100, cached_input_tokens=101
        )


def test_negative_counts_raise():
    with pytest.raises(ValueError):
        pricing.estimate_cost_usd("muse-spark-1.3", input_tokens=-1)
    with pytest.raises(ValueError):
        pricing.estimate_cost_usd("muse-image-1.0", images=-2)


def test_partial_token_counts():
    # 500k input tokens on Spark = half of $1.25.
    assert pricing.estimate_cost_usd("muse-spark-1.3", input_tokens=500_000) == 0.625
