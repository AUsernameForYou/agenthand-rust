//! Market pricing from agent-supplied marketplace comps.
//!
//! The pricing rule is pure and deterministic (no network): an
//! agent supplies a comps document gathered elsewhere, and this
//! module turns the usable asking prices for an item into a range.
//! The same rule runs in the Python build; the numbers must match
//! bit-for-bit, so the percentile and rounding rules are spelled
//! out here and pinned by tests.
//!
//! Rule: usable comps are those with `is_new` falsy. Prices sort
//! ascending. With fewer than 2 usable comps the item is
//! `unpriced_thin_market`. Otherwise quick/market/premium are the
//! type-7 (linear interpolation) 25th/50th/75th percentiles, each
//! rounded to the nearest $5 (half away from zero, floor $5); the
//! suggested price is the rounded market value. Confidence is
//! `moderate` at 8+ comps, `low-moderate` at 4-7, `low` at 2-3.
//!
//! When comps are supplied to a room run, this module also renders
//! the run's priced-table artifacts (PNG, HTML, notes) from the
//! manifest items.

use std::path::Path;

use base64::Engine;
use serde_json::{json, Map, Value};

use crate::assets::AssetStore;
use crate::backdrop;
use crate::image::{encode_png_rgba, RgbaImage};

/// Source label stamped on every pricing record.
pub const PRICING_SOURCE: &str = "facebook_marketplace_comps";

/// The priced-table footnote, verbatim in the PNG and HTML outputs.
pub const PRICED_TABLE_FOOTNOTE: &str = "Thumbnails are presentation copies (may be rotated or resized for display). The Fidelity Lock receipt attaches to the listing composite, whose product pixels are untouched. Prices are asking-price ranges from Facebook Marketplace comps; advisory only \u{2014} a suggested price is a recommendation, never an order.";

/// Type-7 percentile: rank = p/100 * (n-1), linear interpolation
/// between the bracketing order statistics.
pub fn percentile_type7(sorted: &[f64], p: f64) -> f64 {
    let n = sorted.len();
    assert!(n > 0, "percentile of an empty slice");
    if n == 1 {
        return sorted[0];
    }
    let rank = p / 100.0 * (n - 1) as f64;
    let lo = rank.floor() as usize;
    let frac = rank - lo as f64;
    if lo + 1 < n {
        sorted[lo] + frac * (sorted[lo + 1] - sorted[lo])
    } else {
        sorted[lo]
    }
}

/// Round to the nearest $5, half away from zero, with a $5 floor.
pub fn round_to_nearest_5(value: f64) -> f64 {
    let rounded = (value / 5.0).round() * 5.0;
    if rounded < 5.0 {
        5.0
    } else {
        rounded
    }
}

fn round5_json(value: f64) -> Value {
    let rounded = round_to_nearest_5(value);
    if rounded.fract() == 0.0 {
        json!(rounded as i64)
    } else {
        json!(rounded)
    }
}

/// Usable asking prices from a comp list: `is_new` falsy only,
/// sorted ascending.
pub fn usable_prices(comps: &[Value]) -> Vec<f64> {
    let mut prices: Vec<f64> = comps
        .iter()
        .filter(|comp| {
            !comp
                .get("is_new")
                .map(crate::roomrun::py_truthy)
                .unwrap_or(false)
        })
        .filter_map(|comp| comp.get("price_usd").and_then(Value::as_f64))
        .collect();
    prices.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    prices
}

/// Confidence label for a usable-comp count (n >= 2 assumed).
pub fn confidence_for(n: usize) -> &'static str {
    if n >= 8 {
        "moderate"
    } else if n >= 4 {
        "low-moderate"
    } else {
        "low"
    }
}

fn normalize_name(name: &str) -> String {
    name.trim().to_lowercase()
}

/// The comp list supplied for an item name, matched exactly after
/// trimming and case-folding both sides.
pub fn comps_for_item<'a>(comps_doc: &'a Value, item_name: &str) -> Option<&'a Vec<Value>> {
    let items = comps_doc.get("items")?.as_object()?;
    let wanted = normalize_name(item_name);
    for (key, value) in items {
        if normalize_name(key) == wanted {
            return value.get("comps").and_then(Value::as_array);
        }
    }
    None
}

/// The manifest pricing record for one item.
///
/// `comps_doc` is the parsed comps document, or None when no comps
/// were supplied to the run at all (every item is then
/// `no_comps_supplied` with null pulled_at / radius).
pub fn pricing_for_item(comps_doc: Option<&Value>, item_name: &str) -> Value {
    let (pulled_at, radius) = match comps_doc {
        Some(doc) => (
            doc.get("pulled_at").cloned().unwrap_or(Value::Null),
            doc.get("radius_mi").cloned().unwrap_or(Value::Null),
        ),
        None => (Value::Null, Value::Null),
    };
    let base = |status: &str, n: usize, range: Value, suggested: Value, confidence: Value| {
        json!({
            "status": status,
            "comps_used": n,
            "range": range,
            "suggested": suggested,
            "confidence": confidence,
            "source": PRICING_SOURCE,
            "pulled_at": pulled_at,
            "radius_mi": radius,
        })
    };
    let Some(doc) = comps_doc else {
        return base(
            "no_comps_supplied",
            0,
            Value::Null,
            Value::Null,
            Value::Null,
        );
    };
    let Some(comps) = comps_for_item(doc, item_name) else {
        return base(
            "no_comps_supplied",
            0,
            Value::Null,
            Value::Null,
            Value::Null,
        );
    };
    if comps.is_empty() {
        // An entry with no comps at all is "no comps supplied",
        // not a thin market.
        return base(
            "no_comps_supplied",
            0,
            Value::Null,
            Value::Null,
            Value::Null,
        );
    }
    let prices = usable_prices(comps);
    let n = prices.len();
    if n < 2 {
        return base(
            "unpriced_thin_market",
            n,
            Value::Null,
            Value::Null,
            Value::Null,
        );
    }
    let quick = percentile_type7(&prices, 25.0);
    let market = percentile_type7(&prices, 50.0);
    let premium = percentile_type7(&prices, 75.0);
    let range = json!({
        "quick": round5_json(quick),
        "market": round5_json(market),
        "premium": round5_json(premium),
    });
    base(
        "priced",
        n,
        range,
        round5_json(market),
        json!(confidence_for(n)),
    )
}

/// Raw distribution figures for the pricing notes.
pub fn distribution(prices_sorted: &[f64]) -> Map<String, Value> {
    let mut out = Map::new();
    if prices_sorted.is_empty() {
        return out;
    }
    let grab = |p: f64| percentile_type7(prices_sorted, p);
    out.insert("min".to_string(), json!(prices_sorted[0]));
    out.insert("p25".to_string(), json!(grab(25.0)));
    out.insert("p50".to_string(), json!(grab(50.0)));
    out.insert("p75".to_string(), json!(grab(75.0)));
    out.insert(
        "max".to_string(),
        json!(prices_sorted[prices_sorted.len() - 1]),
    );
    out
}

// ----------------------------------------------------------------------
// Priced-table artifacts
// ----------------------------------------------------------------------

use crate::roomrun::{py_str, py_truthy};

/// Python `str(float)` for the values the notes print: integral
/// floats keep their `.0`, others use the shortest round-trip form.
fn py_float(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e16 {
        format!("{value:.1}")
    } else {
        format!("{value}")
    }
}

fn py_float_list(values: &[f64]) -> String {
    let parts: Vec<String> = values.iter().map(|v| py_float(*v)).collect();
    format!("[{}]", parts.join(", "))
}

/// One table row's display values, derived from the item record
/// (the reference table builder's row shape).
pub struct TableRow {
    pub title: String,
    pub sub: String,
    pub price_range: String,
    pub suggested: String,
    pub comps: String,
    pub confidence: String,
}

pub fn table_row(item: &Value) -> TableRow {
    let listing = item.get("listing").cloned().unwrap_or(Value::Null);
    let name = item.get("name").and_then(Value::as_str).unwrap_or("");
    let title = listing
        .get("title")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .unwrap_or(name)
        .to_string();
    let mut sub = if listing
        .get("title")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .is_some()
    {
        name.to_string()
    } else {
        listing
            .get("condition_notes")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let status = item.get("status").and_then(Value::as_str).unwrap_or("");
    if status != "staged" {
        let failure = item.get("failure").cloned().unwrap_or(Value::Null);
        let mut status_text = format!("status: {status}");
        let stage = failure.get("stage").cloned().unwrap_or(Value::Null);
        let code = failure.get("code").cloned().unwrap_or(Value::Null);
        if py_truthy(&stage) || py_truthy(&code) {
            status_text += &format!(" ({} / {})", py_str(&stage), py_str(&code));
        }
        sub = if sub.is_empty() {
            status_text
        } else {
            format!("{sub} \u{2014} {status_text}")
        };
    }
    let pricing = item.get("pricing").cloned().unwrap_or(Value::Null);
    let (price_range, suggested, confidence) = match pricing.get("status").and_then(Value::as_str) {
        Some("priced") => {
            let range = pricing.get("range").cloned().unwrap_or(Value::Null);
            (
                format!(
                    "${} \u{2013} ${}",
                    py_str(range.get("quick").unwrap_or(&Value::Null)),
                    py_str(range.get("premium").unwrap_or(&Value::Null))
                ),
                format!(
                    "${}",
                    py_str(pricing.get("suggested").unwrap_or(&Value::Null))
                ),
                pricing
                    .get("confidence")
                    .and_then(Value::as_str)
                    .unwrap_or("\u{2014}")
                    .to_string(),
            )
        }
        Some("unpriced_thin_market") => (
            "\u{2014}".to_string(),
            "not priced \u{2014} thin market".to_string(),
            "\u{2014}".to_string(),
        ),
        _ => (
            "\u{2014}".to_string(),
            "not priced \u{2014} no comps supplied".to_string(),
            "\u{2014}".to_string(),
        ),
    };
    TableRow {
        title,
        sub,
        price_range,
        suggested,
        comps: pricing
            .get("comps_used")
            .map(|n| n.to_string())
            .unwrap_or_else(|| "0".to_string()),
        confidence,
    }
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The table's header lines: (title, subtitle).
pub fn header_lines(manifest: &Value, comps_doc: &Value) -> (String, String) {
    let title = "agenthand \u{2014} the room, priced".to_string();
    let pricing_source = manifest
        .get("pricing_source")
        .cloned()
        .unwrap_or(Value::Null);
    let pick = |key: &str| -> Value {
        let from_doc = comps_doc.get(key).cloned().unwrap_or(Value::Null);
        if from_doc.is_null() {
            pricing_source.get(key).cloned().unwrap_or(Value::Null)
        } else {
            from_doc
        }
    };
    let area = pick("area");
    let area_text = if area.is_null() {
        "\u{2014}".to_string()
    } else {
        py_str(&area)
    };
    let radius = pick("radius_mi");
    let pulled = pick("pulled_at");
    let pulled_text = if pulled.is_null() {
        "\u{2014}".to_string()
    } else {
        py_str(&pulled)
    };
    let radius_text = if radius.is_null() {
        format!("area: {area_text}")
    } else {
        format!("within {} mi of {area_text}", py_str(&radius))
    };
    let items = manifest
        .get("items")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    let subtitle = format!(
        "Room {} \u{00B7} {} items \u{00B7} FB Marketplace asking comps {} \u{00B7} pulled {} \u{00B7} backdrop: studio gray",
        manifest
            .get("room_id")
            .and_then(Value::as_str)
            .unwrap_or(""),
        items,
        radius_text,
        pulled_text,
    );
    (title, subtitle)
}

/// Presentation copy of an item's receipted foreground on the
/// studio gray backdrop: the foreground cropped to its visible
/// box, scaled to fit the tile, set down on a soft contact shadow.
/// Presentation only; the receipted composite is never touched.
/// Items without a usable foreground get the bare backdrop tile.
pub fn presentation_thumbnail(store: &AssetStore, item: &Value, size: u32) -> RgbaImage {
    let tile = backdrop::render_studio_gray(size, size);
    let primary_index = item.get("primary_photo_index").and_then(Value::as_u64);
    let handle = item
        .get("sightings")
        .and_then(Value::as_array)
        .and_then(|sightings| {
            sightings.iter().find(|s| {
                s.get("photo_index").and_then(Value::as_u64) == primary_index
                    && s.get("status").and_then(Value::as_str) == Some("ok")
            })
        })
        .and_then(|s| s.get("foreground_asset"))
        .and_then(Value::as_str);
    let Some(handle) = handle else { return tile };
    let Ok(foreground) = store.read_image(handle) else {
        return tile;
    };
    // Bounding box of the visible (alpha > 10) foreground.
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    for y in 0..foreground.height {
        for x in 0..foreground.width {
            if foreground.pixel(x, y)[3] > 10 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x + 1);
                y1 = y1.max(y + 1);
            }
        }
    }
    if x0 == u32::MAX {
        return tile;
    }
    let crop = crate::roomrun::crop_image(&foreground, (x0, y0, x1, y1));
    let scale =
        (size as f64 * 0.86 / crop.width as f64).min(size as f64 * 0.82 / crop.height as f64);
    let item_w = ((crop.width as f64 * scale) as u32).max(1);
    let item_h = ((crop.height as f64 * scale) as u32).max(1);
    let placed_item = crate::resample::resize_bicubic_box(
        &crop,
        item_w,
        item_h,
        [0.0, 0.0, crop.width as f32, crop.height as f32],
    );
    let px0 = (size - item_w) / 2;
    let py1 = (size as f64 * 0.90) as u32;
    let py0 = py1.saturating_sub(item_h);
    // Soft elliptical contact shadow under the item, its alpha
    // forced to zero at every fully opaque product pixel.
    let mut canvas = RgbaImage::solid(size, size, [0, 0, 0, 0]);
    let cx = (px0 as f64 + (px0 + item_w) as f64) / 2.0;
    let cy = py1 as f64 + 0.015 * size as f64;
    let rx = item_w as f64 * 0.44;
    let ry = (item_h as f64 * 0.055).max(5.0);
    for y in 0..size {
        for x in 0..size {
            let d = (((x as f64 - cx) / rx).powi(2) + ((y as f64 - cy) / ry).powi(2)).sqrt();
            if d >= 1.5 {
                continue;
            }
            let mut alpha = (110.0 * (1.5 - d) / 1.5) as u8;
            let lx = x.wrapping_sub(px0);
            let ly = y.wrapping_sub(py0);
            if lx < item_w && ly < item_h && placed_item.pixel(lx, ly)[3] == 255 {
                alpha = 0;
            }
            if alpha > 0 {
                let i = ((y * size + x) * 4) as usize;
                canvas.pixels[i..i + 4].copy_from_slice(&[58, 50, 40, alpha]);
            }
        }
    }
    let mut out = crate::composite::alpha_composite(&tile, &canvas);
    // The item itself, placed on its own transparent canvas so the
    // composite math is the production alpha_composite.
    let mut item_canvas = RgbaImage::solid(size, size, [0, 0, 0, 0]);
    for y in 0..item_h {
        for x in 0..item_w {
            let si = ((y * item_w + x) * 4) as usize;
            let di = (((py0 + y) * size + (px0 + x)) * 4) as usize;
            if di + 4 <= item_canvas.pixels.len() {
                item_canvas.pixels[di..di + 4].copy_from_slice(&placed_item.pixels[si..si + 4]);
            }
        }
    }
    out = crate::composite::alpha_composite(&out, &item_canvas);
    out
}

fn thumb_data_uri(store: &AssetStore, item: &Value) -> String {
    let img = presentation_thumbnail(store, item, 460);
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(encode_png_rgba(&img))
    )
}

/// The priced table as a standalone HTML page, carrying the same
/// data and structure as the reference build's sheet.
pub fn priced_table_html(manifest: &Value, comps_doc: &Value, store: &AssetStore) -> String {
    let (title, subtitle) = header_lines(manifest, comps_doc);
    let items = manifest
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut body_rows: Vec<String> = Vec::new();
    for item in &items {
        let row = table_row(item);
        let sub = if row.sub.is_empty() {
            String::new()
        } else {
            format!("<br><small>{}</small>", escape_html(&row.sub))
        };
        body_rows.push(format!(
            "<tr><td><img alt=\"\" width=\"120\" height=\"120\" src=\"{}\"></td><td><strong>{}</strong>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            thumb_data_uri(store, item),
            escape_html(&row.title),
            sub,
            escape_html(&row.price_range),
            escape_html(&row.suggested),
            escape_html(&row.comps),
            escape_html(&row.confidence),
        ));
    }
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head><meta charset=\"utf-8\"><title>{}</title></head>\n<body>\n<h1>{}</h1>\n<p>{}</p>\n<table>\n<thead><tr><th></th><th>ITEM</th><th>PRICE RANGE</th><th>SUGGESTED</th><th>COMPS</th><th>CONFIDENCE</th></tr></thead>\n<tbody>\n{}\n</tbody>\n</table>\n<p>{}</p>\n</body>\n</html>\n",
        escape_html(&title),
        escape_html(&title),
        escape_html(&subtitle),
        body_rows.join("\n"),
        escape_html(PRICED_TABLE_FOOTNOTE),
    )
}

/// The pricing notes: the method first, then each item's comp
/// listings and the computed distribution behind its record.
pub fn pricing_notes_md(manifest: &Value, comps_doc: &Value) -> String {
    let mut lines: Vec<String> = vec![
        "# Pricing notes".to_string(),
        String::new(),
        "## Method".to_string(),
        String::new(),
        "- Prices are Facebook Marketplace ASKING prices, not sold prices. What a seller asks is a ceiling signal, not a record of what anyone paid.".to_string(),
        "- Listing age is a staleness signal: an ask that has sat for weeks is weaker evidence than a fresh one, and the comp listings below carry each listing's age so the reader can weigh it.".to_string(),
        "- Marketplace sales commonly close about 10\u{2013}20% under ask; the ranges in the table are asking-price ranges, and that negotiation discount assumption is the reader's to apply.".to_string(),
        "- Confidence is capped at moderate on asking data alone: n >= 8 usable comps is moderate, 4\u{2013}7 is low-moderate, 2\u{2013}3 is low, and under 2 usable comps the item is not priced.".to_string(),
        "- The range is the interquartile spread of usable (not new) comps by linear interpolation: quick = p25, market = p50, premium = p75, each rounded to the nearest $5 with a $5 floor. The suggested price is the rounded market value.".to_string(),
        String::new(),
        "## Items".to_string(),
        String::new(),
    ];
    let items = manifest
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for item in &items {
        let name = item.get("name").and_then(Value::as_str).unwrap_or("");
        let pricing = item.get("pricing").cloned().unwrap_or(Value::Null);
        lines.push(format!("### {name}"));
        lines.push(String::new());
        lines.push(format!(
            "- Run status: {}",
            item.get("status").and_then(Value::as_str).unwrap_or("")
        ));
        lines.push(format!(
            "- Pricing: {} (comps used: {})",
            py_str(pricing.get("status").unwrap_or(&Value::Null)),
            pricing
                .get("comps_used")
                .map(|n| n.to_string())
                .unwrap_or_else(|| "0".to_string())
        ));
        let comps = comps_for_item(comps_doc, name).cloned().unwrap_or_default();
        if !comps.is_empty() {
            lines.push("- Comps supplied:".to_string());
            for comp in &comps {
                if !comp.is_object() {
                    continue;
                }
                let field = |key: &str| py_str(comp.get(key).unwrap_or(&Value::Null));
                let excluded = if comp.get("is_new").map(py_truthy).unwrap_or(false) {
                    " \u{2014} excluded (listed as new)"
                } else {
                    ""
                };
                lines.push(format!(
                    "  - {} \u{2014} ask ${} \u{2014} condition: {} \u{2014} distance: {} mi \u{2014} age: {} days{}",
                    field("title"),
                    field("price_usd"),
                    field("condition"),
                    field("distance_mi"),
                    field("age_days"),
                    excluded,
                ));
            }
        } else {
            lines.push("- Comps supplied: none for this item.".to_string());
        }
        let prices = usable_prices(&comps);
        if !prices.is_empty() {
            lines.push(format!(
                "- Usable prices (sorted): {}",
                py_float_list(&prices)
            ));
            if prices.len() >= 2 {
                lines.push(format!(
                    "- Distribution: p25 {}, p50 {}, p75 {} before rounding",
                    py_float(percentile_type7(&prices, 25.0)),
                    py_float(percentile_type7(&prices, 50.0)),
                    py_float(percentile_type7(&prices, 75.0)),
                ));
                let range = pricing.get("range").cloned().unwrap_or(Value::Null);
                lines.push(format!(
                    "- Computed range: ${} \u{2013} ${}; suggested ${}; confidence {}",
                    py_str(range.get("quick").unwrap_or(&Value::Null)),
                    py_str(range.get("premium").unwrap_or(&Value::Null)),
                    py_str(pricing.get("suggested").unwrap_or(&Value::Null)),
                    py_str(pricing.get("confidence").unwrap_or(&Value::Null)),
                ));
            } else {
                lines.push(
                    "- Distribution: under 2 usable comps, so no range is computed (thin market)."
                        .to_string(),
                );
            }
        }
        lines.push(String::new());
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

// ----------------------------------------------------------------------
// The PNG table, drawn with a small embedded bitmap font.
// ----------------------------------------------------------------------

mod font {
    /// 3x5 glyphs, five rows of three bits ('#' = ink). Letters are
    /// drawn as capitals; digits and the punctuation the table needs
    /// are included. Unknown characters render blank.
    pub fn glyph(ch: char) -> [&'static str; 5] {
        match ch.to_ascii_uppercase() {
            'A' => [".#.", "#.#", "###", "#.#", "#.#"],
            'B' => ["##.", "#.#", "##.", "#.#", "##."],
            'C' => [".##", "#..", "#..", "#..", ".##"],
            'D' => ["##.", "#.#", "#.#", "#.#", "##."],
            'E' => ["###", "#..", "##.", "#..", "###"],
            'F' => ["###", "#..", "##.", "#..", "#.."],
            'G' => [".##", "#..", "#.#", "#.#", ".##"],
            'H' => ["#.#", "#.#", "###", "#.#", "#.#"],
            'I' => ["###", ".#.", ".#.", ".#.", "###"],
            'J' => ["..#", "..#", "..#", "#.#", ".#."],
            'K' => ["#.#", "#.#", "##.", "#.#", "#.#"],
            'L' => ["#..", "#..", "#..", "#..", "###"],
            'M' => ["#.#", "###", "#.#", "#.#", "#.#"],
            'N' => ["#.#", "#.#", "#.#", "#.#", "#.#"],
            'O' => [".#.", "#.#", "#.#", "#.#", ".#."],
            'P' => ["##.", "#.#", "##.", "#..", "#.."],
            'Q' => [".#.", "#.#", "#.#", "#.#", ".##"],
            'R' => ["##.", "#.#", "##.", "#.#", "#.#"],
            'S' => [".##", "#..", ".#.", "..#", "##."],
            'T' => ["###", ".#.", ".#.", ".#.", ".#."],
            'U' => ["#.#", "#.#", "#.#", "#.#", "###"],
            'V' => ["#.#", "#.#", "#.#", "#.#", ".#."],
            'W' => ["#.#", "#.#", "#.#", "###", "#.#"],
            'X' => ["#.#", "#.#", ".#.", "#.#", "#.#"],
            'Y' => ["#.#", "#.#", ".#.", ".#.", ".#."],
            'Z' => ["###", "..#", ".#.", "#..", "###"],
            '0' => ["###", "#.#", "#.#", "#.#", "###"],
            '1' => [".#.", "##.", ".#.", ".#.", "###"],
            '2' => ["##.", "..#", ".#.", "#..", "###"],
            '3' => ["##.", "..#", ".#.", "..#", "##."],
            '4' => ["#.#", "#.#", "###", "..#", "..#"],
            '5' => ["###", "#..", "##.", "..#", "##."],
            '6' => [".##", "#..", "##.", "#.#", ".#."],
            '7' => ["###", "..#", ".#.", ".#.", ".#."],
            '8' => [".#.", "#.#", ".#.", "#.#", ".#."],
            '9' => [".#.", "#.#", ".##", "..#", "##."],
            '.' => ["...", "...", "...", "...", ".#."],
            ',' => ["...", "...", "...", ".#.", "#.."],
            ':' => ["...", ".#.", "...", ".#.", "..."],
            ';' => ["...", ".#.", "...", ".#.", "#.."],
            '\'' => [".#.", ".#.", "...", "...", "..."],
            '"' => ["#.#", "#.#", "...", "...", "..."],
            '!' => [".#.", ".#.", ".#.", "...", ".#."],
            '?' => ["##.", "..#", ".#.", "...", ".#."],
            '$' => [".##", "##.", ".#.", ".##", "##."],
            '-' | '\u{2013}' | '\u{2014}' => ["...", "...", "###", "...", "..."],
            '(' => [".##", "#..", "#..", "#..", ".##"],
            ')' => ["##.", "..#", "..#", "..#", "##."],
            '/' => ["..#", "..#", ".#.", "#..", "#.."],
            '%' => ["#.#", "..#", ".#.", "#..", "#.#"],
            '&' => ["##.", "##.", ".#.", "#.#", ".##"],
            '+' => ["...", ".#.", "###", ".#.", "..."],
            '=' => ["...", "###", "...", "###", "..."],
            '*' => ["#.#", ".#.", "#.#", "...", "..."],
            '#' => ["#.#", "###", "#.#", "###", "#.#"],
            '_' => ["...", "...", "...", "...", "###"],
            '<' => ["..#", ".#.", "#..", ".#.", "..#"],
            '>' => ["#..", ".#.", "..#", ".#.", "#.."],
            _ => ["...", "...", "...", "...", "..."],
        }
    }
}

fn draw_text(img: &mut RgbaImage, x: u32, y: u32, text: &str, scale: u32, color: [u8; 4]) {
    let mut cx = x;
    for ch in text.chars() {
        let glyph = font::glyph(ch);
        for (row, line) in glyph.iter().enumerate() {
            for (col, bit) in line.chars().enumerate() {
                if bit != '#' {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        let px = cx + col as u32 * scale + dx;
                        let py = y + row as u32 * scale + dy;
                        if px < img.width && py < img.height {
                            let i = ((py * img.width + px) * 4) as usize;
                            img.pixels[i..i + 4].copy_from_slice(&color);
                        }
                    }
                }
            }
        }
        cx += 4 * scale;
    }
}

fn fill_rect(img: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: [u8; 4]) {
    for py in y..(y + h).min(img.height) {
        for px in x..(x + w).min(img.width) {
            let i = ((py * img.width + px) * 4) as usize;
            img.pixels[i..i + 4].copy_from_slice(&color);
        }
    }
}

fn paste(img: &mut RgbaImage, src: &RgbaImage, x: u32, y: u32) {
    for sy in 0..src.height {
        for sx in 0..src.width {
            let (px, py) = (x + sx, y + sy);
            if px < img.width && py < img.height {
                let si = ((sy * src.width + sx) * 4) as usize;
                let di = ((py * img.width + px) * 4) as usize;
                img.pixels[di..di + 4].copy_from_slice(&src.pixels[si..si + 4]);
            }
        }
    }
}

fn wrap_text(text: &str, max_chars: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split(' ') {
        if current.is_empty() {
            current = word.to_string();
        } else if current.len() + 1 + word.len() <= max_chars {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// The priced table as a PNG: thumbnail, item, range, suggested,
/// comps, confidence, with the footnote under the rows. It carries
/// the same rows and header lines as the HTML sheet.
pub fn priced_table_png(manifest: &Value, comps_doc: &Value, store: &AssetStore) -> RgbaImage {
    let items = manifest
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let (_title, subtitle) = header_lines(manifest, comps_doc);
    let width: u32 = 1240;
    let margin: u32 = 24;
    let title_h: u32 = 72;
    let header_h: u32 = 36;
    let row_h: u32 = 132;
    let scale: u32 = 3;
    let sub_scale: u32 = 2;
    let foot_scale: u32 = 2;
    let foot_chars = ((width - margin * 2) / (4 * foot_scale)) as usize;
    let foot_lines = wrap_text(PRICED_TABLE_FOOTNOTE, foot_chars.max(20));
    let foot_h = (foot_lines.len() as u32) * (5 * foot_scale + 4) + 24;
    let height = title_h + header_h + row_h * items.len() as u32 + foot_h + margin;
    let mut img = RgbaImage::solid(width, height, [255, 255, 255, 255]);
    let ink = [26, 26, 26, 255];
    let grey = [120, 120, 120, 255];
    let band = [244, 245, 247, 255];
    let line_color = [204, 204, 204, 255];

    draw_text(
        &mut img,
        margin,
        margin,
        "AGENTHAND \u{2014} THE ROOM, PRICED",
        4,
        ink,
    );
    let sub_lines = wrap_text(&subtitle.to_uppercase(), 100);
    for (i, line) in sub_lines.iter().take(2).enumerate() {
        draw_text(
            &mut img,
            margin,
            margin + 30 + i as u32 * 14,
            line,
            sub_scale,
            grey,
        );
    }

    // Columns: thumbnail | ITEM | PRICE RANGE | SUGGESTED | COMPS | CONFIDENCE
    let cols: [(u32, &str); 6] = [
        (margin, ""),
        (232, "ITEM"),
        (640, "PRICE RANGE"),
        (830, "SUGGESTED"),
        (1010, "COMPS"),
        (1090, "CONFIDENCE"),
    ];
    let header_y = title_h;
    fill_rect(&mut img, 0, header_y, width, header_h, band);
    for (x, label) in cols {
        if !label.is_empty() {
            draw_text(&mut img, x, header_y + 10, label, sub_scale, ink);
        }
    }
    fill_rect(&mut img, 0, header_y + header_h - 1, width, 1, line_color);

    for (row_idx, item) in items.iter().enumerate() {
        let y = title_h + header_h + row_idx as u32 * row_h;
        let thumb = presentation_thumbnail(store, item, 180);
        let thumb = crate::resample::fit(&thumb, 108, 108);
        paste(&mut img, &thumb, margin, y + 12);
        let row = table_row(item);
        let title_lines = wrap_text(&row.title.to_uppercase(), 30);
        for (i, line) in title_lines.iter().take(2).enumerate() {
            draw_text(
                &mut img,
                cols[1].0,
                y + 14 + i as u32 * 22,
                line,
                scale,
                ink,
            );
        }
        if !row.sub.is_empty() {
            let sub_lines = wrap_text(&row.sub.to_uppercase(), 48);
            for (i, line) in sub_lines.iter().take(2).enumerate() {
                draw_text(
                    &mut img,
                    cols[1].0,
                    y + 62 + i as u32 * 14,
                    line,
                    sub_scale,
                    grey,
                );
            }
        }
        let range_lines = wrap_text(&row.price_range.to_uppercase(), 13);
        for (i, line) in range_lines.iter().take(2).enumerate() {
            draw_text(
                &mut img,
                cols[2].0,
                y + 14 + i as u32 * 22,
                line,
                scale,
                ink,
            );
        }
        let sugg_lines = wrap_text(&row.suggested.to_uppercase(), 15);
        for (i, line) in sugg_lines.iter().take(3).enumerate() {
            draw_text(
                &mut img,
                cols[3].0,
                y + 14 + i as u32 * 22,
                line,
                scale,
                ink,
            );
        }
        draw_text(&mut img, cols[4].0, y + 14, &row.comps, scale, ink);
        draw_text(
            &mut img,
            cols[5].0,
            y + 14,
            &row.confidence.to_uppercase(),
            sub_scale,
            ink,
        );
        fill_rect(&mut img, 0, y + row_h - 1, width, 1, line_color);
    }

    let foot_y = title_h + header_h + row_h * items.len() as u32 + 12;
    for (i, line) in foot_lines.iter().enumerate() {
        draw_text(
            &mut img,
            margin,
            foot_y + i as u32 * (5 * foot_scale + 4),
            line,
            foot_scale,
            grey,
        );
    }
    img
}

/// Write priced-table.png, priced-table.html and pricing-notes.md
/// into the run's output directory.
pub fn write_priced_artifacts(
    out_dir: &Path,
    manifest: &Value,
    comps_doc: &Value,
    store: &AssetStore,
) -> Result<(), String> {
    let png = priced_table_png(manifest, comps_doc, store);
    std::fs::write(out_dir.join("priced-table.png"), encode_png_rgba(&png))
        .map_err(|e| e.to_string())?;
    std::fs::write(
        out_dir.join("priced-table.html"),
        priced_table_html(manifest, comps_doc, store),
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(
        out_dir.join("pricing-notes.md"),
        pricing_notes_md(manifest, comps_doc),
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}
