//! The pricing table, ported from the Python `pricing.py` (same
//! version string, same contract: unknown models raise, cached tokens
//! are a subset of input, negative counts raise).

/// Version of the pricing table below; bumped on any rate change.
pub const PRICING_TABLE_VERSION: &str = "2026-10-26.2";

#[derive(Default, Clone, Copy)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
    pub images: i64,
}

impl Usage {
    pub fn merge(&self, other: &Usage) -> Usage {
        Usage {
            input_tokens: self.input_tokens + other.input_tokens,
            output_tokens: self.output_tokens + other.output_tokens,
            cached_input_tokens: self.cached_input_tokens + other.cached_input_tokens,
            images: self.images + other.images,
        }
    }
}

fn token_prices(model: &str) -> Option<(f64, f64, f64)> {
    // (input, output, cached_input) USD per 1M tokens.
    match model {
        "muse-spark-1.3" => Some((1.25, 4.25, 0.15)),
        "muse-wonder-1.0" => Some((4.00, 20.00, 0.20)),
        _ => None,
    }
}

fn per_image_price(model: &str) -> Option<f64> {
    match model {
        "muse-image-1.0" => Some(0.01),
        _ => None,
    }
}

fn per_1000_images_price(model: &str) -> Option<f64> {
    match model {
        "sam-3.1" => Some(2.50),
        _ => None,
    }
}

/// True when the model has a pricing-table entry; unknown models are
/// refused, never priced at zero.
pub fn is_known_model(model: &str) -> bool {
    token_prices(model).is_some()
        || per_image_price(model).is_some()
        || per_1000_images_price(model).is_some()
}

/// Port of `estimate_cost_usd`. Errors mirror the Python raises.
pub fn estimate_cost_usd(model: &str, usage: &Usage) -> Result<f64, String> {
    if !is_known_model(model) {
        return Err(format!(
            "No price for model {model:?}; refusing to estimate it as $0. \
             Add it to the pricing table (with a version bump) first."
        ));
    }
    for (name, value) in [
        ("input_tokens", usage.input_tokens),
        ("output_tokens", usage.output_tokens),
        ("cached_input_tokens", usage.cached_input_tokens),
        ("images", usage.images),
    ] {
        if value < 0 {
            return Err(format!("{name} must be a non-negative integer, got {value}"));
        }
    }
    if usage.cached_input_tokens > usage.input_tokens {
        return Err(format!(
            "cached_input_tokens ({}) cannot exceed input_tokens ({}); \
             cached tokens are a subset of input.",
            usage.cached_input_tokens, usage.input_tokens
        ));
    }
    let mut total = 0.0f64;
    if let Some((input, output, cached)) = token_prices(model) {
        let fresh = usage.input_tokens - usage.cached_input_tokens;
        total += fresh as f64 * input / 1_000_000.0;
        total += usage.cached_input_tokens as f64 * cached / 1_000_000.0;
        total += usage.output_tokens as f64 * output / 1_000_000.0;
    }
    if let Some(price) = per_image_price(model) {
        total += usage.images as f64 * price;
    }
    if let Some(price) = per_1000_images_price(model) {
        total += usage.images as f64 * price / 1000.0;
    }
    // Python's round(total, 6): round-half-even at 6 decimals.
    Ok(crate::round6(total))
}
