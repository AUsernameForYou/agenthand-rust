//! The Meta Model API client and the identify contract — port of the
//! live-client half of the Python `meta_client.py`.
//!
//! Transport: reqwest (blocking, rustls), retries DISABLED (reqwest
//! does not retry by default) — an SDK-level retry would re-run a
//! billable call behind the gate's back. Every function here performs
//! network I/O and must only run inside `billing::billed_call`.

use crate::config;
use crate::image::decode_image;
use crate::pricing::{self, Usage};
use base64::Engine;
use serde_json::{json, Value};

pub const META_BASE_URL: &str = "https://api.meta.ai/v1";
pub const IDENTIFY_MAX_TOKENS: i64 = 2048;
pub const IDENTIFY_MAX_ATTEMPTS: usize = 3;
pub const SAM_MAX_ATTEMPTS: usize = 2;

const IDENTIFY_PROMPT_TOKENS: i64 = 300;

pub const IDENTIFY_MODEL: &str = "muse-spark-1.3";
pub const CUT_OUT_MODEL: &str = "sam-3.1";
pub const DRAW_BACKGROUND_MODEL: &str = "muse-image-1.0";

/// Raised when a real call is attempted with no META_API_KEY. Raised
/// BEFORE any reservation is written.
pub fn require_meta_api_key() -> Result<String, String> {
    config::get_meta_api_key().ok_or_else(|| {
        "META_API_KEY is not set (environment or .env); the Meta Model API is not configured, so no billable call was made."
            .to_string()
    })
}

pub fn build_client() -> Result<reqwest::blocking::Client, String> {
    // Direct connections only — never the environment's HTTP proxy.
    // reqwest auto-detects proxy env vars, and a TLS-intercepting
    // proxy presents certificates issued by its own local CA, which
    // rustls (webpki roots) rightly rejects as UnknownIssuer; that
    // is exactly how every live call from this VM died at connect
    // time during the port's first live run (2026-10-27). Direct
    // egress to api.meta.ai presents the genuine DigiCert chain.
    // (Python's httpx stack never hit this: it validates against
    // the OS store, which trusts the local proxy CA.)
    reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .map_err(|e| format!("cannot build HTTP client: {e}"))
}

fn post_json(
    client: &reqwest::blocking::Client,
    key: &str,
    path: &str,
    body: &Value,
) -> Result<Value, String> {
    let response = client
        .post(format!("{META_BASE_URL}{path}"))
        .bearer_auth(key)
        .json(body)
        .send()
        .map_err(|e| format!("Meta API request to {path} failed: {e}"))?;
    let status = response.status();
    let text = response
        .text()
        .map_err(|e| format!("Meta API response read failed: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "Meta API {path} returned HTTP {status}: {}",
            text.chars().take(500).collect::<String>()
        ));
    }
    serde_json::from_str(&text)
        .map_err(|e| format!("Meta API {path} returned non-JSON: {e}"))
}

// ---------------------------------------------------------------------------
// identify: schema, instructions, parsing, phrase hygiene, repair.
// ---------------------------------------------------------------------------

fn identify_schema() -> Value {
    json!({
        "name": "identify_result",
        "strict": true,
        "schema": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "description": {"type": "string"},
                "concept": {"type": "string"},
                "alternates": {
                    "type": "array",
                    "items": {"type": "string"},
                    "minItems": 2
                },
                "suggested_title": {"type": "string"},
                "listing_bullets": {
                    "type": "array",
                    "items": {"type": "string"},
                    "minItems": 3,
                    "maxItems": 5
                },
                "condition_notes": {"type": "string"}
            },
            "required": [
                "description", "concept", "alternates", "suggested_title",
                "listing_bullets", "condition_notes"
            ]
        }
    })
}

const IDENTIFY_INSTRUCTIONS: &str = "You are the planning step of a listing-production pipeline. Look at the product photo and return the structured result. Be brief in every field; the length limits are hard. description: a short factual description of the product, at most 40 words. concept: ONE short noun phrase naming the product, suitable for text-prompted image segmentation (e.g. 'brown leather wallet') — the product only, no background, no adjectives about the scene. Name the physical object itself, never the screen, photo, or frame it appears in. alternates: at least 2 different fallback noun phrases for the same product, for use if the first phrase segments poorly. suggested_title: a marketplace listing title. listing_bullets: 3 to 5 short selling points a buyer can verify from the photo, each at most 12 words. condition_notes: visible wear, damage, or defects, in at most 25 words; say so plainly, or state that none are visible.";

/// An identify failure. `raw_response` is Some when the failure was an
/// unparseable response body (the retryable kind); `body_failure()`
/// mirrors `is_identify_body_failure`.
#[derive(Debug)]
pub struct IdentifyError {
    pub message: String,
    pub raw_response: Option<String>,
    pub debug_path: Option<String>,
}

impl IdentifyError {
    pub fn new(message: &str) -> Self {
        IdentifyError {
            message: message.to_string(),
            raw_response: None,
            debug_path: None,
        }
    }
    pub fn body_failure(&self) -> bool {
        self.raw_response.is_some()
    }
}

impl std::fmt::Display for IdentifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

// -- phrase hygiene (port of the deterministic sanitizer) -------------------

const FRAME_HEAD_NOUNS: [&str; 11] = [
    "screen", "display", "monitor", "photo", "photograph", "image", "picture", "background",
    "scene", "frame", "screenshot",
];

fn phrase_tokens(phrase: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let lower = phrase.to_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if c.is_ascii_alphanumeric() {
            current.push(*c);
        } else if *c == '\''
            && i + 1 < chars.len()
            && chars[i + 1].is_ascii_alphabetic()
            && !current.is_empty()
        {
            current.push(*c);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn is_frame_phrase(phrase: &str) -> bool {
    let tokens = phrase_tokens(phrase);
    if tokens.is_empty() {
        return true;
    }
    let mut head = tokens[tokens.len() - 1].as_str();
    if head.ends_with('s') && FRAME_HEAD_NOUNS.contains(&&head[..head.len() - 1]) {
        head = &head[..head.len() - 1];
    }
    FRAME_HEAD_NOUNS.contains(&head)
}

fn rewrite_frame_phrase(phrase: &str) -> Option<String> {
    let mut tokens = phrase_tokens(phrase);
    while !tokens.is_empty() && is_frame_phrase(&tokens.join(" ")) {
        tokens.pop();
    }
    while !tokens.is_empty() && ["the", "a", "an"].contains(&tokens[0].as_str()) {
        tokens.remove(0);
    }
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" "))
    }
}

const DESCRIPTION_CUT_TOKENS: [&str; 15] = [
    "on", "in", "against", "with", "beside", "near", "showing", "displaying", "featuring",
    "sits", "sitting", "placed", "lying", "standing", "resting",
];

fn phrase_from_description(description: &str) -> Option<String> {
    let head_text = description
        .split(['.', ',', ';', ':'])
        .next()
        .unwrap_or("");
    let mut tokens = phrase_tokens(head_text);
    while !tokens.is_empty() && ["the", "a", "an"].contains(&tokens[0].as_str()) {
        tokens.remove(0);
    }
    let mut kept: Vec<String> = Vec::new();
    for token in tokens {
        if DESCRIPTION_CUT_TOKENS.contains(&token.as_str()) || token == "of" {
            break;
        }
        if FRAME_HEAD_NOUNS.contains(&token.as_str())
            || (token.ends_with('s')
                && FRAME_HEAD_NOUNS.contains(&&token[..token.len() - 1]))
        {
            continue;
        }
        kept.push(token);
    }
    if kept.is_empty() || is_frame_phrase(&kept.join(" ")) {
        return None;
    }
    Some(kept.join(" "))
}

/// Returns (concept, alternates); concept None when nothing usable
/// survives (the repairable shortfall).
fn sanitize_plan_phrases(
    concept: &str,
    alternates: &[String],
    description: &str,
) -> (Option<String>, Vec<String>) {
    let mut pool: Vec<String> = Vec::new();
    let mut add = |pool: &mut Vec<String>, phrase: Option<String>| {
        if let Some(p) = phrase {
            if !pool.iter().any(|q| q.to_lowercase() == p.to_lowercase()) {
                pool.push(p);
            }
        }
    };
    let all: Vec<&str> = std::iter::once(concept)
        .chain(alternates.iter().map(|s| s.as_str()))
        .collect();
    for phrase in &all {
        if !is_frame_phrase(phrase) {
            add(&mut pool, Some(phrase.trim().to_string()));
        }
    }
    for phrase in &all {
        if is_frame_phrase(phrase) {
            let rewritten = rewrite_frame_phrase(phrase);
            add(&mut pool, rewritten);
        }
    }
    if pool.is_empty() {
        let derived = phrase_from_description(description);
        add(&mut pool, derived);
    }
    if pool.is_empty() {
        return (None, Vec::new());
    }
    let concept_out = pool.remove(0);
    (Some(concept_out), pool)
}

/// Parse and contract-check one identify response body.
pub fn parse_identify_payload(content: &str) -> Result<Value, IdentifyError> {
    let payload: Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(e) => {
            return Err(IdentifyError {
                message: format!("Identify response is not valid JSON: {e}"),
                raw_response: Some(content.to_string()),
                debug_path: None,
            })
        }
    };
    if !payload.is_object() {
        return Err(IdentifyError::new("Identify response is not a JSON object."));
    }
    for field in ["description", "concept", "suggested_title", "condition_notes"] {
        match payload.get(field).and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => {}
            _ => {
                return Err(IdentifyError::new(&format!(
                    "Identify response field {field:?} is missing or not a non-empty string."
                )))
            }
        }
    }
    let bullets = payload.get("listing_bullets").and_then(|v| v.as_array());
    let bullets_ok = bullets
        .map(|b| {
            (3..=5).contains(&b.len())
                && b.iter()
                    .all(|x| x.as_str().map(|s| !s.trim().is_empty()).unwrap_or(false))
        })
        .unwrap_or(false);
    if !bullets_ok {
        return Err(IdentifyError::new(
            "Identify response field 'listing_bullets' must be 3 to 5 non-empty strings.",
        ));
    }
    let concept = payload["concept"].as_str().unwrap().trim().to_string();
    let raw_alternates = payload.get("alternates").and_then(|v| v.as_array());
    let Some(raw_alternates) = raw_alternates else {
        return Err(IdentifyError::new(
            "ALTERNATES_SHORTFALL: 'alternates' is not a list.",
        ));
    };
    let mut cleaned: Vec<String> = Vec::new();
    for item in raw_alternates {
        if let Some(s) = item.as_str() {
            let phrase = s.trim().to_string();
            if !phrase.is_empty()
                && phrase.to_lowercase() != concept.to_lowercase()
                && !cleaned.contains(&phrase)
            {
                cleaned.push(phrase);
            }
        }
    }
    let description = payload["description"].as_str().unwrap();
    let (concept_out, alternates) = sanitize_plan_phrases(&concept, &cleaned, description);
    let Some(concept_out) = concept_out else {
        return Err(IdentifyError::new(&format!(
            "ALTERNATES_SHORTFALL: no segmentable object phrase in the plan — concept and alternates all name the frame or the medium (concept was {concept:?})."
        )));
    };
    if alternates.len() < 2 {
        return Err(IdentifyError::new(&format!(
            "ALTERNATES_SHORTFALL: fewer than 2 usable alternates after cleaning (got {}).",
            alternates.len()
        )));
    }
    Ok(json!({
        "description": payload["description"].as_str().unwrap().trim(),
        "concept": concept_out,
        "alternates": alternates,
        "suggested_title": payload["suggested_title"].as_str().unwrap().trim(),
        "listing_bullets": bullets
            .unwrap()
            .iter()
            .map(|b| b.as_str().unwrap().trim().to_string())
            .collect::<Vec<_>>(),
        "condition_notes": payload["condition_notes"].as_str().unwrap().trim(),
    }))
}

fn usage_of_chat(response: &Value) -> Usage {
    let usage = response.get("usage");
    let cached = usage
        .and_then(|u| u.get("prompt_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    Usage {
        input_tokens: usage
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        output_tokens: usage
            .and_then(|u| u.get("completion_tokens"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        cached_input_tokens: cached,
        images: 0,
    }
}

fn capture_raw_response(content: &str, call_id: &str) -> Option<String> {
    let dir = config::get_data_dir().join("debug");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("identify-{call_id}.txt"));
    std::fs::write(&path, content).ok()?;
    Some(path.display().to_string())
}

/// One real identify call. `call_id` names the debug capture of an
/// unparseable body (the Python passes the billed call's id via a
/// context var; here it is an explicit parameter).
pub fn vision_identify(
    client: &reqwest::blocking::Client,
    key: &str,
    png_bytes: &[u8],
    model: &str,
    call_id: &str,
) -> Result<(Value, Usage), IdentifyError> {
    let data_uri = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png_bytes)
    );
    let body = json!({
        "model": model,
        "max_tokens": IDENTIFY_MAX_TOKENS,
        "response_format": {"type": "json_schema", "json_schema": identify_schema()},
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": IDENTIFY_INSTRUCTIONS},
                {"type": "image_url", "image_url": {"url": data_uri}}
            ]
        }]
    });
    let response = post_json(client, key, "/chat/completions", &body)
        .map_err(|e| IdentifyError::new(&e))?;
    let mut usage = usage_of_chat(&response);
    let content = response
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let plan = match parse_identify_payload(&content) {
        Ok(plan) => return Ok((plan, usage)),
        Err(e) => e,
    };
    if plan.raw_response.is_some() {
        let mut e = plan;
        e.debug_path = capture_raw_response(&content, call_id);
        return Err(e);
    }
    if !plan.message.starts_with("ALTERNATES_SHORTFALL") {
        return Err(plan);
    }

    // Repair path: one text-only follow-up for replacement phrases.
    let partial: Value = serde_json::from_str(&content).unwrap_or(Value::Null);
    let concept = partial
        .get("concept")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let repair_body = json!({
        "model": model,
        "max_tokens": 256,
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "identify_alternates_repair",
                "strict": true,
                "schema": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "alternates": {
                            "type": "array",
                            "items": {"type": "string"},
                            "minItems": 2
                        }
                    },
                    "required": ["alternates"]
                }
            }
        },
        "messages": [{
            "role": "user",
            "content": format!(
                "A segmentation plan for a product photo uses the noun phrase {concept:?}. Give at least 2 different fallback noun phrases for the same product, each short and naming the concrete physical object itself — never the screen, display, photo, image, screenshot, background, scene, or frame."
            )
        }]
    });
    let repair = post_json(client, key, "/chat/completions", &repair_body)
        .map_err(|e| IdentifyError::new(&e))?;
    usage = usage.merge(&usage_of_chat(&repair));
    let repair_content = repair
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let repair_payload: Result<Value, _> = serde_json::from_str(&repair_content);
    let extra = repair_payload
        .as_ref()
        .ok()
        .and_then(|v| v.get("alternates"))
        .and_then(|v| v.as_array())
        .cloned();
    let Some(extra) = extra else {
        let mut e = IdentifyError {
            message: "Alternates repair response is unusable: 'alternates' missing or not a list"
                .to_string(),
            raw_response: Some(repair_content.clone()),
            debug_path: None,
        };
        e.debug_path = capture_raw_response(&repair_content, call_id);
        return Err(e);
    };
    let mut merged = partial.clone();
    let mut alts: Vec<Value> = partial
        .get("alternates")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    alts.extend(extra);
    merged["alternates"] = Value::Array(alts);
    let merged_text = serde_json::to_string(&merged).unwrap();
    match parse_identify_payload(&merged_text) {
        Ok(plan) => Ok((plan, usage)),
        Err(mut e) => {
            if e.raw_response.is_some() && e.debug_path.is_none() {
                e.debug_path = capture_raw_response(&repair_content, call_id);
            }
            Err(e)
        }
    }
}

pub fn estimate_image_input_tokens(width: Option<u32>, height: Option<u32>) -> i64 {
    match (width, height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => {
            let patches = ((w as f64) / 32.0).ceil() as i64 * ((h as f64) / 32.0).ceil() as i64;
            patches + 256
        }
        _ => 4096,
    }
}

pub fn project_identify_cost_usd(model: &str, width: Option<u32>, height: Option<u32>) -> Result<f64, String> {
    let input_tokens = estimate_image_input_tokens(width, height) + IDENTIFY_PROMPT_TOKENS;
    let one_call = pricing::estimate_cost_usd(
        model,
        &Usage {
            input_tokens,
            output_tokens: IDENTIFY_MAX_TOKENS,
            ..Default::default()
        },
    )?;
    Ok(crate::round6(2.0 * one_call))
}

pub fn project_cut_out_cost_usd(model: &str, max_attempts: usize) -> Result<f64, String> {
    pricing::estimate_cost_usd(
        model,
        &Usage {
            images: max_attempts as i64,
            ..Default::default()
        },
    )
}

pub fn project_draw_cost_usd(model: &str) -> Result<f64, String> {
    pricing::estimate_cost_usd(
        model,
        &Usage {
            images: 1,
            ..Default::default()
        },
    )
}

// ---------------------------------------------------------------------------
// SAM segmentation (Responses API transport).
// ---------------------------------------------------------------------------

fn responses_output_text(response: &Value) -> String {
    if let Some(text) = response.get("output_text").and_then(|v| v.as_str()) {
        return text.to_string();
    }
    let mut parts = String::new();
    if let Some(items) = response.get("output").and_then(|v| v.as_array()) {
        for item in items {
            if let Some(contents) = item.get("content").and_then(|v| v.as_array()) {
                for content in contents {
                    if let Some(text) = content.get("text").and_then(|v| v.as_str()) {
                        parts.push_str(text);
                    }
                }
            }
        }
    }
    parts
}

/// One real SAM segmentation call; (output_text, usage).
pub fn segment_image(
    client: &reqwest::blocking::Client,
    key: &str,
    png_bytes: &[u8],
    phrase: &str,
    model: &str,
) -> Result<(String, Usage), String> {
    let data_uri = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png_bytes)
    );
    let body = json!({
        "model": model,
        "input": [{
            "role": "user",
            "content": [
                {"type": "input_text", "text": phrase},
                {"type": "input_image", "image_url": data_uri}
            ]
        }]
    });
    let response = post_json(client, key, "/responses", &body)?;
    let mut usage = Usage {
        images: 1,
        ..Default::default()
    };
    if let Some(raw) = response.get("usage") {
        usage.input_tokens = raw.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
        usage.output_tokens = raw
            .get("output_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
    }
    Ok((responses_output_text(&response), usage))
}

// ---------------------------------------------------------------------------
// Muse Image background plates (images API transport).
// ---------------------------------------------------------------------------

const IMAGE_SIZE_CANDIDATES: [(u32, u32); 5] = [
    (1024, 1024),
    (1536, 1024),
    (1024, 1536),
    (1792, 1024),
    (1024, 1792),
];

const PLATE_INSTRUCTION: &str = "An empty scene photographed straight-on: the setting described below, with a clear, unobstructed flat surface in the center foreground where an object will be placed later. The scene is EMPTY: no product, no featured object on the central surface, no people, no animals, no text, no logos, no watermark. Scene: ";

pub fn wrap_plate_prompt(scene_prompt: &str) -> String {
    format!("{PLATE_INSTRUCTION}{scene_prompt}")
}

pub fn closest_image_size(width: Option<u32>, height: Option<u32>) -> String {
    let (Some(w), Some(h)) = (width, height) else {
        return "1024x1024".to_string();
    };
    if w == 0 || h == 0 {
        return "1024x1024".to_string();
    }
    let target = (w as f64 / h as f64).ln();
    let mut best = IMAGE_SIZE_CANDIDATES[0];
    let mut best_score = (f64::INFINITY, i64::MIN);
    for candidate in IMAGE_SIZE_CANDIDATES {
        let score = (
            ((candidate.0 as f64 / candidate.1 as f64).ln() - target).abs(),
            -((candidate.0 as i64) * (candidate.1 as i64)),
        );
        if score < best_score {
            best_score = score;
            best = candidate;
        }
    }
    format!("{}x{}", best.0, best.1)
}

/// Plate-call failure with a Python-compatible error code.
#[derive(Debug)]
pub struct ImageError {
    pub code: String,
    pub message: String,
}

fn plate_bytes_from_response(
    client: &reqwest::blocking::Client,
    response: &Value,
) -> Result<Vec<u8>, ImageError> {
    let data = response.get("data").and_then(|v| v.as_array());
    let Some(data) = data.filter(|d| !d.is_empty()) else {
        if let Some(refusal) = response
            .get("refusal")
            .or_else(|| response.get("error"))
            .filter(|v| !v.is_null())
        {
            return Err(ImageError {
                code: "IMAGE_REFUSED".to_string(),
                message: format!("The image model refused the plate prompt: {refusal}"),
            });
        }
        return Err(ImageError {
            code: "BAD_IMAGE_RESPONSE".to_string(),
            message: "The image model returned no image data for the plate prompt.".to_string(),
        });
    };
    let item = &data[0];
    if let Some(refusal) = item.get("refusal").filter(|v| !v.is_null()) {
        return Err(ImageError {
            code: "IMAGE_REFUSED".to_string(),
            message: format!("The image model refused the plate prompt: {refusal}"),
        });
    }
    let mut raw: Option<Vec<u8>> = None;
    if let Some(b64) = item.get("b64_json").and_then(|v| v.as_str()) {
        if !b64.is_empty() {
            raw = Some(
                base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|e| ImageError {
                        code: "BAD_IMAGE_RESPONSE".to_string(),
                        message: format!("The plate image payload is not valid base64: {e}"),
                    })?,
            );
        }
    }
    if raw.is_none() {
        if let Some(url) = item.get("url").and_then(|v| v.as_str()) {
            if !url.is_empty() {
                let resp = client.get(url).send().map_err(|e| ImageError {
                    code: "IMAGE_CALL_FAILED".to_string(),
                    message: format!("Fetching the generated plate image failed: {e}"),
                })?;
                raw = Some(resp.bytes().map_err(|e| ImageError {
                    code: "IMAGE_CALL_FAILED".to_string(),
                    message: format!("Fetching the generated plate image failed: {e}"),
                })?.to_vec());
            }
        }
    }
    let Some(raw) = raw else {
        return Err(ImageError {
            code: "BAD_IMAGE_RESPONSE".to_string(),
            message: "The image model returned an item with neither b64_json nor url."
                .to_string(),
        });
    };
    // Normalize to RGBA PNG bytes.
    let decoded = decode_image(&raw).map_err(|e| ImageError {
        code: "BAD_IMAGE_RESPONSE".to_string(),
        message: format!("The plate payload does not decode as an image: {e}"),
    })?;
    let rgba = decoded.into_rgba();
    Ok(crate::image::encode_png_rgba(&rgba))
}

/// One real muse-image call; (plate PNG bytes, usage).
pub fn request_plate_image(
    client: &reqwest::blocking::Client,
    key: &str,
    prompt: &str,
    width: Option<u32>,
    height: Option<u32>,
    model: &str,
) -> Result<(Vec<u8>, Usage), ImageError> {
    let size = closest_image_size(width, height);
    let body = json!({
        "model": model,
        "prompt": wrap_plate_prompt(prompt),
        "size": size,
        "n": 1
    });
    let response = post_json(client, key, "/images/generations", &body).map_err(|e| {
        if e.contains("refus") {
            ImageError {
                code: "IMAGE_REFUSED".to_string(),
                message: e,
            }
        } else {
            ImageError {
                code: "IMAGE_CALL_FAILED".to_string(),
                message: format!("The muse-image plate call failed: {e}"),
            }
        }
    })?;
    let png_bytes = plate_bytes_from_response(client, &response)?;
    let mut usage = Usage {
        images: 1,
        ..Default::default()
    };
    if let Some(raw) = response.get("usage") {
        usage.input_tokens = raw.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
        usage.output_tokens = raw
            .get("output_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
    }
    Ok((png_bytes, usage))
}
