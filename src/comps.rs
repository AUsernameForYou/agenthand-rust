//! Category comps pricing: a range of possible prices plus a
//! recommended price (and a confidence) for an identified item,
//! computed code-side from a comps file gathered elsewhere.
//!
//! This runs AFTER identify returns. It reads only fields identify
//! already produced (concept, alternates, suggested_title) or an item
//! name, and never adds a word to the identify prompt or a key to
//! the schema the model sees (longer identify wording was bisected as
//! the cause of HTTP 200 responses with empty bodies).
//!
//! The comps file (`AGENTHAND_COMPS_FILE`, default
//! `<data dir>/pricing-comps.json`) holds a list of categories, each
//! with a key, an item description, search queries, comp listings
//! (price, title, location), stats {count,min,p25,median,p75,max},
//! `recommended` (the median; `suggested` is an alias), `range`
//! [min,max], `percentile_band` [p25,p75] and `confidence`. The
//! numbers are passed through as written; this module only picks the
//! category.
//!
//! Matching: every text is normalised to tokens (lowercase, split on
//! non-alphanumerics and letter/digit boundaries, a light plural
//! strip, a few synonyms, stopwords and film production tags such as
//! "P12" dropped). Each category gets a token vocabulary: tokens of
//! its item text and queries weigh 1.0; tokens seen in its comp
//! titles weigh 0.5 + 0.5 x (share of titles containing the token).
//! A text's score for a category is the summed weight of its unique
//! tokens divided by their count. Gate: the head noun (last
//! non-numeric token) of at least one noun phrase (concept,
//! alternates, item name) must weigh at least HEAD_MIN_WEIGHT in the
//! category, so "bike helmet" never prices as a bike. The best
//! category over all candidate texts wins if it reaches
//! MIN_MATCH_SCORE; ties go to the category with more comps, then
//! the lower key. No match means `{"status":"no_comps"}`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// What the prices are, stamped on every priced block.
pub const PRICING_BASIS: &str = "asking prices, Facebook Marketplace comps";

/// The advisory sentence carried on every priced block.
pub const PRICING_FOOTNOTE: &str = "Prices are asking prices from local Facebook Marketplace comps, not sold prices. Advisory only: the recommended price is a suggestion, and the seller sets the price.";

/// Minimum match score for a category to price an item.
pub const MIN_MATCH_SCORE: f64 = 0.6;

/// Minimum vocabulary weight the head noun needs in a category: an
/// item/query token (1.0) or a token in at least 15% of the comp
/// titles (0.5 + 0.5 x 0.15).
pub const HEAD_MIN_WEIGHT: f64 = 0.575;

/// Default comps file name inside the data dir.
pub const DEFAULT_COMPS_FILE_NAME: &str = "pricing-comps.json";

const STOPWORDS: &[&str] = &[
    // function words
    "an", "and", "or", "the", "with", "without", "for", "of", "in", "on", "to", "at", "by",
    "from", "this", "that", "is", "it", "its", "as", "into", "plus",
    // marketplace filler that names no product
    "great", "good", "excellent", "condition", "works", "working", "used", "like", "new",
    "nice", "clean", "sale", "obo", "firm", "only", "perfect", "gently", "lightly", "barely",
    "pick", "pickup", "up", "must", "go", "price", "priced", "cheap", "free", "comes",
    "included", "includes", "brand", "listing", "item", "product",
    // production notes in the comps item text
    "second",
];

/// Lowercase, split, strip, map: the shared token normaliser.
pub fn tokens(text: &str) -> Vec<String> {
    let lowered = text.to_lowercase();
    let mut out: Vec<String> = Vec::new();
    for raw in lowered.split(|c: char| !c.is_alphanumeric()) {
        if raw.is_empty() || is_production_tag(raw) {
            continue;
        }
        for piece in split_letter_digit(raw) {
            if piece.chars().count() < 2 {
                continue;
            }
            let word = synonym(&stem(&piece));
            if word.chars().count() < 2 || STOPWORDS.contains(&word.as_str()) {
                continue;
            }
            out.push(word);
        }
    }
    out
}

/// Unique tokens in first-seen order.
fn unique_tokens(text: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    tokens(text)
        .into_iter()
        .filter(|t| seen.insert(t.clone()))
        .collect()
}

/// Film shot tags in the comps item text ("P12", "P16", "P18").
fn is_production_tag(word: &str) -> bool {
    let mut chars = word.chars();
    chars.next() == Some('p')
        && word.len() >= 2
        && word.len() <= 4
        && chars.all(|c| c.is_ascii_digit())
}

fn split_letter_digit(word: &str) -> Vec<String> {
    let mut pieces: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_digit: Option<bool> = None;
    for c in word.chars() {
        let digit = c.is_ascii_digit();
        if current_digit.is_some() && current_digit != Some(digit) {
            pieces.push(std::mem::take(&mut current));
        }
        current.push(c);
        current_digit = Some(digit);
    }
    if !current.is_empty() {
        pieces.push(current);
    }
    pieces
}

/// Light plural strip: "bikes" -> "bike", "kids" -> "kid"; leaves
/// "glass", "plus", "chassis" and short words alone.
fn stem(word: &str) -> String {
    if word.len() > 3
        && word.ends_with('s')
        && !word.ends_with("ss")
        && !word.ends_with("us")
        && !word.ends_with("is")
        && !word.chars().all(|c| c.is_ascii_digit())
    {
        word[..word.len() - 1].to_string()
    } else {
        word.to_string()
    }
}

fn synonym(word: &str) -> String {
    match word {
        "bicycle" | "bicycles" | "bicycl" => "bike",
        "child" | "children" | "childrens" | "kiddie" | "kiddo" | "toddler" | "youth" | "boy"
        | "girl" => "kid",
        "bbq" | "barbeque" => "barbecue",
        "grille" => "grill",
        "grey" => "gray",
        "automobile" | "vehicle" => "car",
        "woman" | "ladies" | "ladie" | "lady" | "womens" => "women",
        other => other,
    }
    .to_string()
}

/// The configured comps file: `AGENTHAND_COMPS_FILE` when set and
/// non-empty, else `<data dir>/pricing-comps.json`.
pub fn configured_comps_path() -> PathBuf {
    crate::config::get_comps_file()
}

/// One usable category, its priced fields fixed at load time.
#[derive(Debug, Clone)]
pub struct Category {
    pub key: String,
    pub item: String,
    pub comps_count: u64,
    vocab: BTreeMap<String, f64>,
    priced: Value,
}

impl Category {
    /// Vocabulary weight of a normalised token in this category.
    pub fn weight(&self, token: &str) -> f64 {
        self.vocab.get(token).copied().unwrap_or(0.0)
    }

    /// Weighted coverage score of a text against this category.
    pub fn score(&self, text: &str) -> f64 {
        let toks = unique_tokens(text);
        if toks.is_empty() {
            return 0.0;
        }
        toks.iter().map(|t| self.weight(t)).sum::<f64>() / toks.len() as f64
    }
}

/// A loaded comps file: the categories that passed validation, plus
/// a note naming any that were skipped.
#[derive(Debug, Clone)]
pub struct CompsBook {
    pub categories: Vec<Category>,
    pub skipped: Vec<String>,
}

/// The chosen category for an item.
#[derive(Debug, Clone, PartialEq)]
pub struct CategoryMatch {
    pub key: String,
    pub score: f64,
    /// Which candidate text won ("concept", "alternates[0]",
    /// "suggested_title", "item_name", "listing.title").
    pub on: String,
}

fn num(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|v| v.is_finite())
}

fn pair(value: Option<&Value>) -> Option<(Value, Value, f64, f64)> {
    let arr = value?.as_array()?;
    if arr.len() != 2 {
        return None;
    }
    let (a, b) = (num(arr.first())?, num(arr.get(1))?);
    Some((arr[0].clone(), arr[1].clone(), a, b))
}

/// The documented confidence rule, for a category that omits it:
/// high when n>=10 and (p75-p25)/median <= 0.8; medium when n>=6 and
/// the ratio <= 1.3; otherwise low.
pub fn confidence_rule(n: u64, p25: f64, p75: f64, median: f64) -> &'static str {
    if median <= 0.0 {
        return "low";
    }
    let ratio = (p75 - p25) / median;
    if n >= 10 && ratio <= 0.8 {
        "high"
    } else if n >= 6 && ratio <= 1.3 {
        "medium"
    } else {
        "low"
    }
}

fn parse_category(cat: &Value, generated: &Value) -> Result<Category, String> {
    let key = cat
        .get("key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .ok_or("category without a key")?
        .to_string();
    let item = cat
        .get("item")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let stats = cat.get("stats").cloned().unwrap_or(Value::Null);
    let comps: Vec<Value> = cat
        .get("comps")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let (range_lo_v, range_hi_v, range_lo, range_hi) = match pair(cat.get("range")) {
        Some(p) => p,
        None => {
            let lo = num(stats.get("min")).ok_or(format!("{key}: no range"))?;
            let hi = num(stats.get("max")).ok_or(format!("{key}: no range"))?;
            (stats["min"].clone(), stats["max"].clone(), lo, hi)
        }
    };
    let (recommended_v, recommended) = ["recommended", "suggested"]
        .iter()
        .find_map(|k| num(cat.get(*k)).map(|v| (cat[*k].clone(), v)))
        .or_else(|| num(stats.get("median")).map(|v| (stats["median"].clone(), v)))
        .ok_or(format!("{key}: no recommended price"))?;
    if !(range_lo <= recommended && recommended <= range_hi) {
        return Err(format!(
            "{key}: recommended {recommended} is outside the range [{range_lo}, {range_hi}]"
        ));
    }
    let band = match pair(cat.get("percentile_band")) {
        Some((a, b, _, _)) => json!([a, b]),
        None => match (num(stats.get("p25")), num(stats.get("p75"))) {
            (Some(_), Some(_)) => json!([stats["p25"].clone(), stats["p75"].clone()]),
            _ => Value::Null,
        },
    };
    let comps_count = stats
        .get("count")
        .and_then(Value::as_u64)
        .unwrap_or(comps.len() as u64);
    let confidence = match cat.get("confidence").and_then(Value::as_str) {
        Some(c @ ("high" | "medium" | "low")) => c.to_string(),
        _ => match (num(stats.get("p25")), num(stats.get("p75"))) {
            (Some(p25), Some(p75)) => {
                confidence_rule(comps_count, p25, p75, recommended).to_string()
            }
            _ => "low".to_string(),
        },
    };
    let searched_at = cat
        .get("searched_at")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| generated.clone());
    let area = cat.get("area").cloned().unwrap_or(Value::Null);

    // Vocabulary: item text and queries strong; comp titles by share.
    let mut vocab: BTreeMap<String, f64> = BTreeMap::new();
    let mut strong_text = item.clone();
    if let Some(queries) = cat.get("queries").and_then(Value::as_array) {
        for q in queries.iter().filter_map(Value::as_str) {
            strong_text.push(' ');
            strong_text.push_str(q);
        }
    }
    let titles: Vec<&str> = comps
        .iter()
        .filter_map(|c| c.get("title").and_then(Value::as_str))
        .collect();
    let mut df: BTreeMap<String, usize> = BTreeMap::new();
    for title in &titles {
        for tok in unique_tokens(title) {
            *df.entry(tok).or_insert(0) += 1;
        }
    }
    for (tok, n) in df {
        let w = 0.5 + 0.5 * n as f64 / titles.len().max(1) as f64;
        vocab.insert(tok, w);
    }
    for tok in tokens(&strong_text) {
        vocab.insert(tok, 1.0);
    }
    if vocab.is_empty() {
        return Err(format!("{key}: no item text or comp titles to match against"));
    }

    let priced = json!({
        "status": "priced",
        "category": key,
        "item": item,
        "range": [range_lo_v, range_hi_v],
        "recommended": recommended_v,
        "percentile_band": band,
        "confidence": confidence,
        "comps_count": comps_count,
        "searched_at": searched_at,
        "area": area,
        "basis": PRICING_BASIS,
        "advisory": true,
        "footnote": PRICING_FOOTNOTE,
    });
    Ok(Category {
        key,
        item,
        comps_count,
        vocab,
        priced,
    })
}

/// Parse a comps document already in memory.
pub fn parse_book(doc: &Value) -> Result<CompsBook, String> {
    let cats = doc
        .get("categories")
        .and_then(Value::as_array)
        .ok_or("comps file has no \"categories\" list")?;
    let generated = doc.get("generated").cloned().unwrap_or(Value::Null);
    let mut categories = Vec::new();
    let mut skipped = Vec::new();
    for cat in cats {
        match parse_category(cat, &generated) {
            Ok(c) => categories.push(c),
            Err(e) => skipped.push(e),
        }
    }
    if categories.is_empty() {
        return Err(format!(
            "comps file has no usable categories{}",
            if skipped.is_empty() {
                String::new()
            } else {
                format!(" ({})", skipped.join("; "))
            }
        ));
    }
    Ok(CompsBook {
        categories,
        skipped,
    })
}

fn one_line(text: String) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Load and parse a comps file. Errors are one-line notes.
pub fn load_book(path: &Path) -> Result<CompsBook, String> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        one_line(if e.kind() == std::io::ErrorKind::NotFound {
            format!("pricing comps file not found at {}", path.display())
        } else {
            format!("pricing comps file {} unreadable: {e}", path.display())
        })
    })?;
    let doc: Value = serde_json::from_str(&text).map_err(|e| {
        one_line(format!(
            "pricing comps file {} is not valid JSON: {e}",
            path.display()
        ))
    })?;
    parse_book(&doc).map_err(|e| one_line(format!("pricing comps file {}: {e}", path.display())))
}

/// Load the configured comps file (see `configured_comps_path`).
pub fn load_configured_book() -> Result<CompsBook, String> {
    load_book(&configured_comps_path())
}

/// The block for an item with no matching category.
pub fn no_comps() -> Value {
    json!({"status": "no_comps"})
}

/// The block when the comps file itself is missing or malformed.
pub fn no_comps_with_note(note: &str) -> Value {
    json!({"status": "no_comps", "note": note})
}

/// Head noun of a noun phrase: its last non-numeric token.
fn head_token(phrase: &str) -> Option<String> {
    tokens(phrase)
        .into_iter()
        .rev()
        .find(|t| !t.chars().all(|c| c.is_ascii_digit()))
}

impl CompsBook {
    pub fn category(&self, key: &str) -> Option<&Category> {
        self.categories.iter().find(|c| c.key == key)
    }

    /// Pick the category for an item. `phrases` are noun phrases
    /// (label, text) whose head nouns gate the categories; `titles`
    /// are extra texts (listing titles) that score but do not gate.
    /// With no usable head noun at all, every category is eligible.
    pub fn match_category(
        &self,
        phrases: &[(String, String)],
        titles: &[(String, String)],
    ) -> Option<CategoryMatch> {
        let heads: Vec<String> = phrases.iter().filter_map(|(_, p)| head_token(p)).collect();
        let mut best: Option<(f64, u64, &str, String)> = None;
        for cat in &self.categories {
            if !heads.is_empty() && !heads.iter().any(|h| cat.weight(h) >= HEAD_MIN_WEIGHT) {
                continue;
            }
            let mut cat_best: Option<(f64, &str)> = None;
            for (label, text) in phrases.iter().chain(titles.iter()) {
                let s = cat.score(text);
                if cat_best.map(|(b, _)| s > b + 1e-12).unwrap_or(true) {
                    cat_best = Some((s, label.as_str()));
                }
            }
            let Some((score, label)) = cat_best else {
                continue;
            };
            let better = match &best {
                None => true,
                Some((bs, bn, bk, _)) => {
                    if (score - bs).abs() > 1e-9 {
                        score > *bs
                    } else if cat.comps_count != *bn {
                        cat.comps_count > *bn
                    } else {
                        cat.key.as_str() < *bk
                    }
                }
            };
            if better {
                best = Some((score, cat.comps_count, cat.key.as_str(), label.to_string()));
            }
        }
        let (score, _, key, on) = best?;
        if score + 1e-12 < MIN_MATCH_SCORE {
            return None;
        }
        Some(CategoryMatch {
            key: key.to_string(),
            score,
            on,
        })
    }

    /// The pricing block for a match.
    pub fn priced_block(&self, m: &CategoryMatch) -> Value {
        let Some(cat) = self.category(&m.key) else {
            return no_comps();
        };
        let mut block = cat.priced.clone();
        block["match"] = json!({
            "score": (m.score * 1000.0).round() / 1000.0,
            "on": m.on,
        });
        block
    }

    fn price(&self, phrases: &[(String, String)], titles: &[(String, String)]) -> Value {
        match self.match_category(phrases, titles) {
            Some(m) => self.priced_block(&m),
            None => no_comps(),
        }
    }
}

fn nonempty(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Pricing for an identify result, from the concept, its alternates
/// and the suggested title already in that result.
pub fn pricing_for_identify(book: &Result<CompsBook, String>, result: &Value) -> Value {
    let book = match book {
        Ok(b) => b,
        Err(note) => return no_comps_with_note(note),
    };
    let mut phrases: Vec<(String, String)> = Vec::new();
    if let Some(concept) = nonempty(result.get("concept")) {
        phrases.push(("concept".to_string(), concept));
    }
    if let Some(alts) = result.get("alternates").and_then(Value::as_array) {
        for (i, alt) in alts.iter().enumerate() {
            if let Some(text) = nonempty(Some(alt)) {
                phrases.push((format!("alternates[{i}]"), text));
            }
        }
    }
    let mut titles: Vec<(String, String)> = Vec::new();
    if let Some(title) = nonempty(result.get("suggested_title")) {
        titles.push(("suggested_title".to_string(), title));
    }
    if phrases.is_empty() && titles.is_empty() {
        return no_comps();
    }
    book.price(&phrases, &titles)
}

/// Pricing for an item by name, with an optional listing title as
/// extra (non-gating) evidence.
pub fn pricing_for_item_name(
    book: &Result<CompsBook, String>,
    item_name: &str,
    listing_title: Option<&str>,
) -> Value {
    let book = match book {
        Ok(b) => b,
        Err(note) => return no_comps_with_note(note),
    };
    let mut phrases: Vec<(String, String)> = Vec::new();
    if !item_name.trim().is_empty() {
        phrases.push(("item_name".to_string(), item_name.trim().to_string()));
    }
    let mut titles: Vec<(String, String)> = Vec::new();
    if let Some(title) = listing_title.map(str::trim).filter(|t| !t.is_empty()) {
        titles.push(("listing.title".to_string(), title.to_string()));
    }
    if phrases.is_empty() && titles.is_empty() {
        return no_comps();
    }
    book.price(&phrases, &titles)
}

/// Add the `pricing` block to an identify tool result in place. Only
/// an ok result is priced; every existing field is left as it was.
pub fn attach_identify_pricing(result: &mut Value, book: &Result<CompsBook, String>) {
    if result.get("status").and_then(Value::as_str) != Some("ok") {
        return;
    }
    let block = pricing_for_identify(book, result);
    if let Some(obj) = result.as_object_mut() {
        obj.insert("pricing".to_string(), block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_normalise() {
        assert_eq!(
            tokens("Women's Cropped Denim Jackets (GAP light-wash, P18)"),
            vec!["women", "cropped", "denim", "jacket", "gap", "light", "wash"]
        );
        assert_eq!(tokens("Kids 20-inch BMX bike"), vec!["kid", "20", "inch", "bmx", "bike"]);
        assert_eq!(tokens("children's bicycle"), vec!["kid", "bike"]);
        assert_eq!(tokens("3pc Patio Set"), vec!["pc", "patio", "set"]);
        assert_eq!(tokens("glass top"), vec!["glass", "top"]);
        assert!(tokens("a rubber duck").contains(&"duck".to_string()));
    }

    #[test]
    fn confidence_rule_matches_the_documented_rule() {
        // gas grill: n=12, (85-50)/72.5 = 0.48 -> high
        assert_eq!(confidence_rule(12, 50.0, 85.0, 72.5), "high");
        // kids bmx: n=9 -> medium
        assert_eq!(confidence_rule(9, 44.0, 80.0, 50.0), "medium");
        // climbing dome: n=8, (462.5-68.75)/250 = 1.575 -> low
        assert_eq!(confidence_rule(8, 68.75, 462.5, 250.0), "low");
    }
}
