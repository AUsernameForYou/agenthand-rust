//! Room runs: one listing per named item, staged from a whole room.
//!
//! THE ROOM IS THE PRODUCT SURFACE (decision 51): the seller walks a
//! room, filming a narrated tour and/or shooting room photos, and says
//! what goes ("this cup, this bear, this chair"). The room run turns
//! that into one staged listing per item, each with its own Fidelity
//! Lock receipt, plus a room manifest that records what was probed,
//! what failed, and exactly what the run cost.
//!
//! This module is a port of the Python room runner (commits 3544ff7 /
//! 9457b0c / 7eac806) and produces the same manifest fields and
//! semantics. It is NOT a fourth MCP tool: it is a pipeline over the
//! three shipped tool functions (identify, cut_out, draw) through the
//! same billed_call choke point, plus an independent verifier pass per
//! staged item with the recount recorded in the manifest. Narration is
//! a separate pool (Baseten partner credits for Spark Open, which can
//! take audio) and is accounted in its own manifest section, never in
//! the Meta room total.
//!
//! Failure is first-class data throughout: a photo that does not
//! contain the item, a narration track that cannot be transcribed, a
//! draw the verifier refuses — all are rows in the manifest, never
//! run-killers. The only hard stops are the worst-case spend
//! projection and the live budget check, both of which refuse BEFORE
//! further calls.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde_json::{json, Value};

use crate::assets::{AssetStore, KIND_CROP, KIND_PLATE};
use crate::meta::{
    project_cut_out_cost_usd, project_draw_cost_usd, project_identify_cost_usd, CUT_OUT_MODEL,
    DRAW_BACKGROUND_MODEL, IDENTIFY_MAX_ATTEMPTS, IDENTIFY_MODEL,
};
use crate::{backdrop, composite, config, ingest, ledger, market_pricing};

pub const MAX_PHOTOS_PER_ITEM: usize = 3;
pub const MAX_SAM_CALLS_PER_ITEM: usize = 6;
pub const MAX_VARIANTS_PER_PHOTO: usize = 2;
pub const MERGE_SIMILARITY: f64 = 0.6;
pub const GREY_ZONE_LOW: f64 = 0.4;
pub const MASK_IOU_FLAG: f64 = 0.9;
/// Two primary masks on one photo are the same physical object when
/// their overlap reaches this fraction of the smaller mask.
pub const COLLISION_MASK_OVERLAP: f64 = 0.95;
pub const DEFAULT_SCENE: &str = "a bright, calm room with a plain warm-white wall and a light wooden table surface, soft daylight";
pub const DEFAULT_BUDGET_USD: f64 = 1.00;

// -- narration constants ---------------------------------------------
// Narration runs on Baseten partner credits (a separate pool from the
// Meta $150 gate): Muse Spark Open is the only model Baseten serves on
// this account and it accepts audio input. The prompt extracts the
// transcript AND the sale intent in one call: narration is where a
// video tour turns into the seller's item list.
pub const NARRATION_MODEL: &str = "meta-models/muse-spark-open-1026";
/// Spark Open always reasons before it answers, and the reasoning
/// grows with the clip. The voice spike showed a 1024-token cap fully
/// consumed by reasoning, leaving an empty reply; 4096 covered the
/// short spikes but not a real walkthrough: on Daniel's 58.9 s
/// backyard clip the model's reasoning alone ran 5,474 tokens (5,668
/// completion total) before any answer text, so the shipped 4096 cap
/// returned an empty reply (film run P12, 2026-10-31; the same call
/// at an 8000 cap recovered the full transcript and sort). The cap is
/// 16384 PER PIECE: narration splits any clip longer than
/// NARRATION_CHUNK_SECONDS into pieces (below), so no single call
/// ever faces more than a minute of audio. A minute at the worst
/// observed reasoning rate (~121 tokens/s, the 24 s voice spike)
/// needs ~7,300 reasoning tokens plus its share of transcript, so
/// 16384 carries >2x headroom per piece; for the 58.9 s diagnostic
/// clip in one piece it is ~2.9x the whole observed completion. The
/// cap is a fixed constant rather than duration-scaled on purpose:
/// billing is per token actually used, so the cap only feeds the
/// cost projection; a formula would add cross-build parity surface
/// for no billing benefit. A piece whose reasoning still eats the
/// whole budget fails loudly with a correction path (see
/// interpret_narration_body), never a silent empty result.
pub const NARRATION_MAX_TOKENS: u32 = 16384;
/// Pieces are a minute because that is the proven envelope: the
/// diagnostic clip (58.9 s, one call) is the longest single call ever
/// observed end to end, request size and latency included. A trailing
/// remainder shorter than the merge bound joins the previous piece
/// instead of becoming a sliver call of its own.
pub const NARRATION_CHUNK_SECONDS: f64 = 60.0;
pub const NARRATION_CHUNK_MERGE_REMAINDER_S: f64 = 15.0;
/// The narration limit: clips up to 5 minutes are accepted (chunked
/// into per-minute calls); a longer clip gets the polite
/// NARRATION_TOO_LONG refusal naming this limit (Daniel, 2026-10-31).
/// Baseten caps files at 5 minutes too, so the limit also matches
/// the upstream ceiling.
pub const NARRATION_MAX_DURATION_S: f64 = 300.0;
/// A narration file whose worst-case projection exceeds this is
/// refused before any call is made. The projection is honestly
/// worst-case (every piece priced at the full token cap): a
/// 5-minute file projects to ~$0.41 worst case against ~$0.13 of
/// expected actual usage at observed rates, so $0.50 covers the
/// whole supported envelope with headroom and the gate only trips
/// on a pathological input or a misprobed duration.
pub const NARRATION_DEFAULT_MAX_PROJECTED_USD: f64 = 0.50;
pub const NARRATION_TOKENS_PER_SECOND: f64 = 150.0;
pub const NARRATION_PROMPT_TOKENS_FLOOR: f64 = 512.0;
pub const NARRATION_SILENCE_PEAK: f64 = 0.01; // ~ -40 dBFS
pub const BASETEN_BASE_URL: &str = "https://inference.baseten.co/v1";
pub const NARRATION_PROMPT: &str = "Transcribe this recording verbatim, word for word, and output it under the heading TRANSCRIPT. Then, under the heading SELLING, list each physical item the speaker explicitly says they want to sell (wording like 'sell this', 'this one's for sale', 'get rid of'), one per line, naming each item the way the speaker referred to it; if the speaker marks no item for sale, write NONE under that heading. Then, under the heading KEEP, list each physical item the speaker says they are keeping or not selling (wording like 'the bike stays', 'keep it', 'not for sale', 'keep it for insurance'), one per line; if there are none, write NONE. Then, under the heading MENTIONED, list each other physical item the speaker refers to in passing without saying they want to sell it or keep it, one per line; if there are none, write NONE. Use exactly those four headings, in that order, and do not add any other text. Do not invent items the speaker did not refer to.";

// ----------------------------------------------------------------------
// Small Python-semantics helpers
// ----------------------------------------------------------------------

/// Python's round(): round-half-to-even on the decimal digit, like
/// `round(x, ndigits)`. Python rounds the float's true binary value,
/// so this scales, rounds half-even at the integer, and scales back.
pub fn python_round(x: f64, ndigits: i32) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let factor = 10f64.powi(ndigits);
    let scaled = x * factor;
    let floor = scaled.floor();
    let diff = scaled - floor;
    let rounded = if diff > 0.5 {
        floor + 1.0
    } else if diff < 0.5 {
        floor
    } else if (floor as i64) % 2 == 0 {
        floor
    } else {
        floor + 1.0
    };
    rounded / factor
}

/// `datetime.now(timezone.utc).isoformat()` — microseconds, +00:00.
pub fn iso_now_micros() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let micros = now.subsec_micros();
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}+00:00",
        year,
        month,
        day,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        micros
    )
}

/// Howard Hinnant's civil-from-days algorithm (same math the ledger's
/// seconds-precision timestamp uses).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Python truthiness for the JSON shapes this module handles.
pub fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python f-string rendering of a value (`str(x)`): None/True/False
/// spellings, shortest-round-trip numbers, Python list/dict reprs.
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(b) => {
            if *b {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Value::Number(_) => serde_json::to_string(v).unwrap_or_else(|_| "0".to_string()),
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(py_repr).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, val)| format!("{}: {}", py_repr(&Value::String(k.clone())), py_repr(val)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

/// Python `repr(s)` for a string: single quotes, switching to double
/// quotes when the text has a single quote but no double quote.
pub fn py_repr_str(s: &str) -> String {
    if s.contains('\'') && !s.contains('"') {
        format!("\"{}\"", s.replace('\\', "\\\\"))
    } else {
        format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

/// Python `repr(x)` for the scalar/list shapes the summary renders.
fn py_repr(v: &Value) -> String {
    match v {
        Value::String(s) => py_repr_str(s),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(py_repr).collect();
            format!("[{}]", parts.join(", "))
        }
        other => py_str(other),
    }
}

fn which(program: &str) -> Option<PathBuf> {
    if program.is_empty() {
        return None;
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn resolve_ffmpeg(param: Option<&str>) -> Option<PathBuf> {
    match param {
        Some(p) => {
            if p.is_empty() {
                None
            } else {
                Some(PathBuf::from(p))
            }
        }
        None => which("ffmpeg"),
    }
}

// ----------------------------------------------------------------------
// Text matching (the ranker's whole vocabulary)
// ----------------------------------------------------------------------

/// Maximal `[a-z0-9]+` runs of the lowercased text — spelled out
/// because Python's `re.findall(r"[a-z0-9]+", text.lower())` is the
/// exact contract the tests pin.
pub fn tokens_of(text: Option<&str>) -> Vec<String> {
    let Some(text) = text else { return Vec::new() };
    if text.is_empty() {
        return Vec::new();
    }
    let lower = text.to_lowercase();
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in lower.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            cur.push(ch);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub fn token_set(text: Option<&str>) -> HashSet<String> {
    tokens_of(text).into_iter().collect()
}

/// Loose token match: equal, or one is a prefix of the other with the
/// shorter side at least 4 characters ("hats" matches "hat" would be
/// wrong at 3; "suitcase" matches "suitcases"). Single place so the
/// similarity, the merge rule and the head-noun bonus can never drift.
pub fn token_matches(token: &str, others: &HashSet<String>) -> bool {
    others.iter().any(|other| {
        token == other.as_str()
            || (token.len() >= 4 && other.starts_with(token))
            || (other.len() >= 4 && token.starts_with(other.as_str()))
    })
}

pub fn token_similarity(a: Option<&str>, b: Option<&str>) -> f64 {
    let set_a = token_set(a);
    let set_b = token_set(b);
    if set_a.is_empty() || set_b.is_empty() {
        return 0.0;
    }
    let shared = set_a.iter().filter(|t| token_matches(t, &set_b)).count();
    let union = set_a.len() + set_b.len() - shared;
    if union == 0 {
        0.0
    } else {
        shared as f64 / union as f64
    }
}

fn plan_get_str<'a>(plan: &'a Value, key: &str) -> &'a str {
    plan.get(key).and_then(Value::as_str).unwrap_or("")
}

/// The prose an identify plan carries about a photo: description,
/// concept, suggested title, listing bullets, condition notes. The
/// ranker and the concept matcher read plans through this one lens.
pub fn plan_text(plan: Option<&Value>) -> String {
    let Some(plan) = plan else {
        return String::new();
    };
    let bullets = plan
        .get("listing_bullets")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    [
        plan_get_str(plan, "description"),
        plan_get_str(plan, "concept"),
        plan_get_str(plan, "suggested_title"),
        bullets.as_str(),
        plan_get_str(plan, "condition_notes"),
    ]
    .join(" ")
}

pub fn plan_matches_item(plan: Option<&Value>, item_name: &str) -> bool {
    let Some(plan) = plan else { return false };
    let concept = plan.get("concept").and_then(Value::as_str).unwrap_or("");
    if concept.is_empty() {
        return false;
    }
    token_similarity(Some(concept), Some(item_name)) >= MERGE_SIMILARITY
}

const STRIPPABLE_LEADERS: [&str; 20] = [
    "black", "white", "gray", "grey", "red", "orange", "yellow", "green", "blue", "purple", "pink",
    "brown", "wooden", "wood", "large", "small", "big", "little", "vintage", "antique",
];

/// Concept phrasings to try for an item on one photo, best first.
///
/// The seller's phrasing, the photo's own plan concept (only when the
/// plan actually describes this item), the seller's phrasing with a
/// leading modifier stripped ("black hat" -> "hat": segmenters often
/// answer the bare noun better), and the bare head noun. Capped
/// downstream by MAX_VARIANTS_PER_PHOTO and the per-item SAM budget,
/// so a layered description never becomes an unbounded search.
pub fn concept_variants(item_name: &str, plan: Option<&Value>) -> Vec<String> {
    let mut variants: Vec<String> = Vec::new();
    let push = |v: String, variants: &mut Vec<String>| {
        if !v.is_empty() && !variants.contains(&v) {
            variants.push(v);
        }
    };
    let words = tokens_of(Some(item_name));
    let name = words.join(" ");
    push(name.clone(), &mut variants);
    if plan_matches_item(plan, item_name) {
        if let Some(plan) = plan {
            let concept = tokens_of(plan.get("concept").and_then(Value::as_str)).join(" ");
            push(concept, &mut variants);
        }
    }
    if words.len() > 1 && STRIPPABLE_LEADERS.contains(&words[0].as_str()) {
        push(words[1..].join(" "), &mut variants);
    }
    if words.len() > 1 {
        push(words[words.len() - 1].clone(), &mut variants);
    }
    variants
}

fn bbox_vec(bbox: Option<&Value>) -> Option<[f64; 4]> {
    let arr = bbox?.as_array()?;
    if arr.len() < 4 {
        return None;
    }
    let mut out = [0.0; 4];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = arr[i].as_f64()?;
    }
    Some(out)
}

pub fn bbox_iou(a: Option<&Value>, b: Option<&Value>) -> f64 {
    let (Some(a), Some(b)) = (bbox_vec(a), bbox_vec(b)) else {
        return 0.0;
    };
    let ix0 = a[0].max(b[0]);
    let iy0 = a[1].max(b[1]);
    let ix1 = a[2].min(b[2]);
    let iy1 = a[3].min(b[3]);
    if ix1 <= ix0 || iy1 <= iy0 {
        return 0.0;
    }
    let inter = (ix1 - ix0) * (iy1 - iy0);
    let area = |r: [f64; 4]| ((r[2] - r[0]).max(0.0)) * ((r[3] - r[1]).max(0.0));
    let union = area(a) + area(b) - inter;
    if union <= 0.0 {
        0.0
    } else {
        inter / union
    }
}

pub fn normalized_bbox(bbox: Option<&Value>, width: Option<u32>, height: Option<u32>) -> Value {
    let (Some(b), Some(w), Some(h)) = (bbox_vec(bbox), width, height) else {
        return Value::Null;
    };
    if w == 0 || h == 0 {
        return Value::Null;
    }
    json!([
        b[0] / w as f64,
        b[1] / h as f64,
        b[2] / w as f64,
        b[3] / h as f64
    ])
}

// ----------------------------------------------------------------------
// Source pool: stills + video frames + narration tracks
// ----------------------------------------------------------------------

pub struct PhotoSource {
    pub path: PathBuf,
    pub origin: &'static str, // "still" | "video_frame"
    pub video_path: Option<String>,
    pub timestamp_s: Option<f64>,
    pub handle: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub plan: Option<Value>,
    pub plan_error: Option<Value>,
}

impl PhotoSource {
    fn manifest_entry(&self, index: usize) -> Value {
        let plan = self.plan.as_ref().map(|p| {
            json!({
                "description": p.get("description").cloned().unwrap_or(Value::Null),
                "concept": p.get("concept").cloned().unwrap_or(Value::Null),
                "suggested_title": p.get("suggested_title").cloned().unwrap_or(Value::Null),
            })
        });
        json!({
            "index": index,
            "path": self.path.to_string_lossy(),
            "origin": self.origin,
            "video": self.video_path,
            "timestamp_s": self.timestamp_s,
            "handle": self.handle,
            "width": self.width,
            "height": self.height,
            "plan": plan,
            "plan_error": self.plan_error,
        })
    }
}

const IMAGE_SUFFIXES: [&str; 7] = [".jpg", ".jpeg", ".png", ".webp", ".heic", ".heif", ".gif"];

fn suffix_lower(path: &Path) -> String {
    path.extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
        .unwrap_or_default()
}

fn collect_dir_images(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = Vec::new();
    if let Ok(read) = std::fs::read_dir(dir) {
        for entry in read.flatten() {
            entries.push(entry.path());
        }
    }
    for path in entries {
        if path.is_dir() {
            collect_dir_images(&path, out);
        } else if path.is_file() && IMAGE_SUFFIXES.contains(&suffix_lower(&path).as_str()) {
            out.push(path);
        }
    }
}

/// Stills for a set of paths: directories are walked (recursively,
/// path-sorted); plain files are taken as-is regardless of suffix.
pub fn collect_stills(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for path in paths {
        if path.is_dir() {
            let mut found = Vec::new();
            collect_dir_images(path, &mut found);
            found.sort_by(|a, b| a.as_os_str().cmp(b.as_os_str()));
            out.extend(found);
        } else if path.is_file() {
            out.push(path.clone());
        }
    }
    out
}

// ----------------------------------------------------------------------
// Sampled-frame dedupe (token saver, 2026-10-31): frames are compared
// against the last KEPT frame as 16x16 grayscale box-mean thumbnails;
// a candidate whose mean absolute thumbnail difference is at most
// FRAME_DEDUPE_MAX_MEAN_ABS_DIFF gray levels is a near-duplicate and
// is dropped before any paid identify call. The bar is deliberately
// strict: the P12 run showed a second angle of the same item is
// information, not duplication, and a moved camera or repositioned
// object scores far above this bound (see the test pair in
// tests/roomrun.rs). Every drop is logged with its similarity score
// in the run record ("frames_dropped").
// ----------------------------------------------------------------------

pub const FRAME_DEDUPE_CELLS: usize = 16;
pub const FRAME_DEDUPE_MAX_MEAN_ABS_DIFF: f64 = 3.0;

/// 16x16 grayscale box-mean thumbnail of an image file, or None.
/// Gray uses integer Rec. 601 luma ((299R + 587G + 114B) / 1000);
/// each cell averages the source box [c*W/16, (c+1)*W/16) on both
/// axes. The Python build computes the identical values.
pub fn frame_gray_thumbnail(path: &Path) -> Option<[f64; 256]> {
    let data = std::fs::read(path).ok()?;
    let img = crate::image::decode_image(&data).ok()?.into_rgba();
    let (w, h) = (img.width as usize, img.height as usize);
    if w == 0 || h == 0 {
        return None;
    }
    let cells = FRAME_DEDUPE_CELLS;
    let mut thumb = [0.0f64; 256];
    for cy in 0..cells {
        let y0 = cy * h / cells;
        let y1 = ((cy + 1) * h / cells).max(y0 + 1);
        for cx in 0..cells {
            let x0 = cx * w / cells;
            let x1 = ((cx + 1) * w / cells).max(x0 + 1);
            let mut sum = 0.0f64;
            let mut count = 0.0f64;
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * w + x) * 4;
                    let gray = (299 * img.pixels[i] as u32
                        + 587 * img.pixels[i + 1] as u32
                        + 114 * img.pixels[i + 2] as u32)
                        / 1000;
                    sum += gray as f64;
                    count += 1.0;
                }
            }
            thumb[cy * cells + cx] = sum / count;
        }
    }
    Some(thumb)
}

/// Mean absolute difference between two grayscale thumbnails.
pub fn frame_mean_abs_diff(a: &[f64; 256], b: &[f64; 256]) -> f64 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (x - y).abs())
        .sum::<f64>()
        / 256.0
}

/// Extract stills from a seller's room video with the system ffmpeg.
///
/// The room pool deserves more than a poster frame: items are named
/// against the whole walk-through. Frames are sampled sparsely; an
/// un-sampleable video is a note in the manifest, never a run error.
/// Returns (kept frames, note, dropped-frame records with similarity
/// scores) — near-duplicates are dropped during sampling, before any
/// paid identify call.
pub fn sample_video_frames(
    video_path: &Path,
    out_dir: &Path,
    every_seconds: f64,
    max_frames: usize,
    ffmpeg: Option<&str>,
) -> (Vec<(PathBuf, f64)>, Option<String>, Vec<Value>) {
    let Some(ffmpeg_bin) = resolve_ffmpeg(ffmpeg) else {
        return (
            Vec::new(),
            Some("ffmpeg not found on PATH; video frames not sampled".to_string()),
            Vec::new(),
        );
    };
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_format",
            "-of",
            "json",
            video_path.to_string_lossy().as_ref(),
        ])
        .output();
    let duration = (|| -> Option<f64> {
        let output = probe.ok()?;
        if !output.status.success() {
            return None;
        }
        let parsed: Value = serde_json::from_slice(&output.stdout).ok()?;
        let raw = parsed.get("format")?.get("duration")?;
        raw.as_f64()
            .or_else(|| raw.as_str().and_then(|s| s.parse::<f64>().ok()))
    })();
    let Some(duration) = duration else {
        return (
            Vec::new(),
            Some(format!(
                "ffprobe could not read {}; frames not sampled",
                video_path.display()
            )),
            Vec::new(),
        );
    };
    let _ = std::fs::create_dir_all(out_dir);
    let stem = video_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut frames: Vec<(PathBuf, f64)> = Vec::new();
    let mut dropped: Vec<Value> = Vec::new();
    let mut last_thumb: Option<[f64; 256]> = None;
    let mut t = 0.0f64;
    while t < duration && frames.len() < max_frames {
        let frame_path = out_dir.join(format!("{stem}-t{t:05.1}s.jpg"));
        let ran = Command::new(&ffmpeg_bin)
            .args([
                "-v",
                "error",
                "-ss",
                &t.to_string(),
                "-i",
                video_path.to_string_lossy().as_ref(),
                "-frames:v",
                "1",
                "-q:v",
                "3",
                frame_path.to_string_lossy().as_ref(),
                "-y",
            ])
            .output();
        let ok = matches!(&ran, Ok(o) if o.status.success())
            && frame_path.is_file()
            && std::fs::metadata(&frame_path)
                .map(|m| m.len() > 0)
                .unwrap_or(false);
        if ok {
            let thumb = frame_gray_thumbnail(&frame_path);
            let diff = match (&thumb, &last_thumb) {
                (Some(a), Some(b)) => Some(frame_mean_abs_diff(a, b)),
                _ => None,
            };
            if let Some(diff) = diff {
                if diff <= FRAME_DEDUPE_MAX_MEAN_ABS_DIFF {
                    dropped.push(json!({
                        "timestamp_s": python_round(t, 2),
                        "similarity": python_round(1.0 - diff / 255.0, 4),
                    }));
                    let _ = std::fs::remove_file(&frame_path);
                    t += every_seconds;
                    continue;
                }
            }
            frames.push((frame_path, python_round(t, 2)));
            if thumb.is_some() {
                last_thumb = thumb;
            }
        }
        t += every_seconds;
    }
    if frames.is_empty() {
        return (
            Vec::new(),
            Some(format!(
                "ffmpeg extracted no frames from {}",
                video_path.display()
            )),
            dropped,
        );
    }
    (frames, None, dropped)
}

// ----------------------------------------------------------------------
// Narration: Baseten Spark Open (partner-credit pool)
// ----------------------------------------------------------------------

/// Split a clip's timeline into narration pieces.
///
/// Pure. A clip at or under NARRATION_CHUNK_SECONDS is one piece (the
/// only shape older runs ever made). Longer clips cut into
/// chunk-sized windows; a trailing remainder under
/// NARRATION_CHUNK_MERGE_REMAINDER_S joins the previous window
/// instead of becoming a sliver call. A 5-minute clip plans to
/// exactly 5 pieces.
pub fn plan_narration_chunks(duration_s: f64) -> Vec<(f64, f64)> {
    if duration_s <= NARRATION_CHUNK_SECONDS {
        return vec![(0.0, duration_s)];
    }
    let mut chunks: Vec<(f64, f64)> = Vec::new();
    let mut start = 0.0;
    while duration_s - start > NARRATION_CHUNK_SECONDS {
        chunks.push((start, start + NARRATION_CHUNK_SECONDS));
        start += NARRATION_CHUNK_SECONDS;
    }
    if !chunks.is_empty() && duration_s - start < NARRATION_CHUNK_MERGE_REMAINDER_S {
        let last = chunks.last_mut().expect("chunks is non-empty");
        last.1 = duration_s;
    } else {
        chunks.push((start, duration_s));
    }
    chunks
}

/// Worst-case price of narrating one file at the published rates.
///
/// Output is priced per planned piece at the full token cap: with
/// chunking, a long file's worst case is pieces x cap, not one cap.
pub fn project_narration_cost_usd(duration_s: f64, max_tokens: u32) -> f64 {
    let prompt_tokens = duration_s * NARRATION_TOKENS_PER_SECOND + NARRATION_PROMPT_TOKENS_FLOOR;
    let pieces = plan_narration_chunks(duration_s).len() as f64;
    prompt_tokens * 1.25 / 1_000_000.0 + pieces * max_tokens as f64 * 4.25 / 1_000_000.0
}

/// Real cost from reported usage; None when usage is absent (the
/// projector's estimate stands in, and the manifest says so by
/// keeping the projected figure alongside).
pub fn narration_cost_usd(usage: Option<&Value>) -> Option<f64> {
    let usage = usage?;
    let grab = |key: &str| -> f64 {
        usage
            .get(key)
            .and_then(Value::as_f64)
            .filter(|v| *v != 0.0)
            .unwrap_or(0.0)
    };
    let prompt = grab("prompt_tokens");
    let completion = grab("completion_tokens");
    if prompt == 0.0 && completion == 0.0 {
        return None;
    }
    Some(prompt * 1.25 / 1_000_000.0 + completion * 4.25 / 1_000_000.0)
}

/// Structured view parsed from a narration reply (transcript lines,
/// SELLING names, MENTIONED names).
pub struct NarrationParse {
    pub transcript: String,
    pub selling: Vec<String>,
    pub keep: Vec<String>,
    pub mentioned: Vec<String>,
}

/// Split a narration reply into transcript / selling / keep /
/// mentioned. A reply with no recognizable heading is treated as
/// a bare transcript — the seller's words still matter; the run
/// just gains no item names from it.
pub fn parse_narration_reply(text: Option<&str>) -> NarrationParse {
    let text = text.unwrap_or("");
    let mut sections: [Vec<String>; 4] = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    let mut current: Option<usize> = None;
    let mut found = false;
    for raw_line in text.split('\n') {
        let line = raw_line.trim();
        let key = line.trim_end_matches(':').trim().to_lowercase();
        match key.as_str() {
            "transcript" => {
                current = Some(0);
                found = true;
                continue;
            }
            "selling" => {
                current = Some(1);
                found = true;
                continue;
            }
            "keep" => {
                current = Some(2);
                found = true;
                continue;
            }
            "mentioned" => {
                current = Some(3);
                found = true;
                continue;
            }
            _ => {}
        }
        if let Some(idx) = current {
            sections[idx].push(line.to_string());
        }
    }
    if !found {
        return NarrationParse {
            transcript: text.trim().to_string(),
            selling: Vec::new(),
            keep: Vec::new(),
            mentioned: Vec::new(),
        };
    }
    let clean_items = |lines: &[String]| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for line in lines {
            let mut item = line
                .trim_start_matches(|c| c == '-' || c == '\u{2022}' || c == '*')
                .trim()
                .to_string();
            // re.sub(r"^\d+[.)]\s*", "", item)
            let bytes = item.as_bytes();
            let mut digits = 0;
            while digits < bytes.len() && bytes[digits].is_ascii_digit() {
                digits += 1;
            }
            if digits > 0
                && digits < bytes.len()
                && (bytes[digits] == b'.' || bytes[digits] == b')')
            {
                let rest = &item[digits + 1..];
                item = rest.trim_start().to_string();
            }
            let item = item.trim().to_string();
            if item.is_empty() || item.to_uppercase() == "NONE" {
                continue;
            }
            if !out.contains(&item) {
                out.push(item);
            }
        }
        out
    };
    NarrationParse {
        transcript: sections[0].join("\n").trim().to_string(),
        selling: clean_items(&sections[1]),
        keep: clean_items(&sections[2]),
        mentioned: clean_items(&sections[3]),
    }
}

// ----------------------------------------------------------------------
// Narration audio prep (token saver, 2026-10-31): trim silence and
// downsample before the narration call. Detection is deliberately
// conservative: a 10 ms frame counts as speech when its peak reaches
// the same -40 dBFS floor the silence gate uses, every kept region
// is padded 200 ms on both sides, and gaps up to 800 ms are bridged,
// so nothing at or above the floor can be clipped. Any prep failure
// falls back to the original WAV: prep is an optimization, never a
// new failure mode. The 5-minute refusal and the cost projection
// stay priced on the ORIGINAL duration; this only shrinks the send.
// ----------------------------------------------------------------------

pub const NARRATION_PREP_TARGET_RATE: u32 = 16000;
const NARRATION_PREP_FRAME_S: f64 = 0.01;
const NARRATION_PREP_PAD_S: f64 = 0.2;
const NARRATION_PREP_BRIDGE_S: f64 = 0.8;

/// Trim silence from a narration WAV and downsample it for the call.
/// Returns (path to send, prepared duration in seconds when a shorter
/// prepared file was written).
pub fn prepare_narration_audio(wav_path: &Path) -> (PathBuf, Option<f64>) {
    let fallback = || (wav_path.to_path_buf(), None);
    let Ok(data) = std::fs::read(wav_path) else {
        return fallback();
    };
    if data.len() < 44 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return fallback();
    }
    let mut fmt: Option<(u16, u16, u32, usize)> = None; // (format, channels, rate, sampwidth)
    let mut pcm: Option<&[u8]> = None;
    let mut pos = 12usize;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let size =
            u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                as usize;
        let start = pos + 8;
        let end = (start + size).min(data.len());
        match id {
            b"fmt " => {
                if end - start >= 16 {
                    let audio_format = u16::from_le_bytes([data[start], data[start + 1]]);
                    let channels = u16::from_le_bytes([data[start + 2], data[start + 3]]);
                    let rate = u32::from_le_bytes([
                        data[start + 4],
                        data[start + 5],
                        data[start + 6],
                        data[start + 7],
                    ]);
                    let bits = u16::from_le_bytes([data[start + 14], data[start + 15]]);
                    fmt = Some((audio_format, channels, rate, (bits as usize + 7) / 8));
                }
            }
            b"data" => pcm = Some(&data[start..end]),
            _ => {}
        }
        pos = start + size + (size % 2);
    }
    let Some((audio_format, channels, rate, sampwidth)) = fmt else {
        return fallback();
    };
    if audio_format != 1 || sampwidth != 2 || !(1..=2).contains(&channels) || rate == 0 {
        return fallback();
    }
    let Some(raw) = pcm else { return fallback() };
    let n_pairs = raw.len() / 2;
    let mut samples: Vec<i16> = Vec::with_capacity(n_pairs);
    for i in 0..n_pairs {
        samples.push(i16::from_le_bytes([raw[2 * i], raw[2 * i + 1]]));
    }
    if channels == 2 {
        let mut mono: Vec<i16> = Vec::with_capacity(samples.len() / 2);
        let mut i = 0;
        while i + 1 < samples.len() {
            // i32 division truncates toward zero, as in the Python build.
            mono.push(((samples[i] as i32 + samples[i + 1] as i32) / 2) as i16);
            i += 2;
        }
        samples = mono;
    }
    let total = samples.len();
    if total == 0 {
        return fallback();
    }
    let frame_len = ((rate as f64 * NARRATION_PREP_FRAME_S) as usize).max(1);
    let floor_peak = NARRATION_SILENCE_PEAK * 32767.0;
    let mut speech: Vec<bool> = Vec::new();
    let mut start = 0usize;
    while start < total {
        let end = (start + frame_len).min(total);
        let mut peak = 0i32;
        for s in &samples[start..end] {
            peak = peak.max((*s as i32).abs());
        }
        speech.push(peak as f64 >= floor_peak);
        start += frame_len;
    }
    if !speech.iter().any(|&s| s) {
        return fallback();
    }
    let pad = (rate as f64 * NARRATION_PREP_PAD_S) as usize;
    let bridge = (rate as f64 * NARRATION_PREP_BRIDGE_S) as usize;
    let mut regions: Vec<(usize, usize)> = Vec::new();
    for (index, &is_speech) in speech.iter().enumerate() {
        if !is_speech {
            continue;
        }
        let lo = (index * frame_len).saturating_sub(pad);
        let hi = ((index + 1) * frame_len + pad).min(total);
        if let Some(last) = regions.last_mut() {
            if lo <= last.1 + bridge {
                last.1 = hi;
                continue;
            }
        }
        regions.push((lo, hi));
    }
    let mut kept: Vec<i16> = Vec::new();
    for (lo, hi) in &regions {
        kept.extend_from_slice(&samples[*lo..*hi]);
    }
    let mut out_rate = rate;
    if rate % NARRATION_PREP_TARGET_RATE == 0 {
        let factor = (rate / NARRATION_PREP_TARGET_RATE) as usize;
        if factor > 1 {
            let usable = kept.len() - kept.len() % factor;
            let mut down: Vec<i16> = Vec::with_capacity(usable / factor);
            let mut i = 0;
            while i < usable {
                let sum: i32 = kept[i..i + factor].iter().map(|&s| s as i32).sum();
                let mean = (sum as f64 / factor as f64).round();
                down.push(mean.clamp(-32768.0, 32767.0) as i16);
                i += factor;
            }
            kept = down;
            out_rate = rate / factor as u32;
        }
    }
    if kept.len() >= total && out_rate == rate {
        return fallback();
    }
    let stem = wav_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let out_path = wav_path.with_file_name(format!("{stem}-prepared.wav"));
    let mut bytes = Vec::with_capacity(kept.len() * 2);
    for s in &kept {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    if write_wav_piece(&out_path, 1, out_rate, 2, &bytes).is_err() {
        return fallback();
    }
    let duration = kept.len() as f64 / out_rate as f64;
    (out_path, Some(duration))
}

/// (duration_seconds, peak_fraction) for a PCM WAV, measured with a
/// hand-rolled RIFF parse — the same semantics the Python stdlib
/// `wave` module gives: uncompressed PCM only, frame count from the
/// data chunk, peak over the raw samples (widths 1/2/4; 24-bit files
/// report peak 0 and ride the caller's judgement, as in Python).
pub fn probe_wav(path: &Path) -> Result<(f64, f64), String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    if data.len() < 44 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return Err("file does not start with RIFF id".to_string());
    }
    let mut fmt: Option<(u16, u32)> = None; // (channels, sample_rate)
    let mut sampwidth = 0usize;
    let mut audio_format = 0u16;
    let mut pcm: Option<&[u8]> = None;
    let mut pos = 12usize;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let size = u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
            as usize;
        let start = pos + 8;
        let end = (start + size).min(data.len());
        match id {
            b"fmt " => {
                if end - start >= 16 {
                    audio_format = u16::from_le_bytes([data[start], data[start + 1]]);
                    let channels = u16::from_le_bytes([data[start + 2], data[start + 3]]);
                    let rate = u32::from_le_bytes([
                        data[start + 4],
                        data[start + 5],
                        data[start + 6],
                        data[start + 7],
                    ]);
                    let bits = u16::from_le_bytes([data[start + 14], data[start + 15]]);
                    sampwidth = (bits as usize + 7) / 8;
                    fmt = Some((channels, rate));
                }
            }
            b"data" => {
                pcm = Some(&data[start..end]);
            }
            _ => {}
        }
        pos = start + size + (size % 2);
    }
    let (channels, rate) = fmt.ok_or("fmt chunk not found")?;
    if audio_format != 1 {
        return Err(format!("unknown format: {audio_format}"));
    }
    let raw = pcm.ok_or("data chunk not found")?;
    let framesize = sampwidth.max(1) * (channels as usize).max(1);
    let frames = raw.len() / framesize;
    let duration = if rate != 0 {
        frames as f64 / rate as f64
    } else {
        0.0
    };
    let mut peak = 0.0f64;
    match sampwidth {
        2 => {
            let n = raw.len() / 2;
            if n > 0 {
                let mut max_v = i32::MIN;
                let mut min_v = i32::MAX;
                for i in 0..n {
                    let v = i16::from_le_bytes([raw[2 * i], raw[2 * i + 1]]) as i32;
                    max_v = max_v.max(v);
                    min_v = min_v.min(v);
                }
                peak = (max_v.max(-min_v) as f64) / 32768.0;
            }
        }
        1 => {
            if !raw.is_empty() {
                let mut m = 0i32;
                for b in raw {
                    m = m.max((*b as i32 - 128).abs());
                }
                peak = m as f64 / 128.0;
            }
        }
        4 => {
            let n = raw.len() / 4;
            if n > 0 {
                let mut max_v = i64::MIN;
                let mut min_v = i64::MAX;
                for i in 0..n {
                    let v = i32::from_le_bytes([
                        raw[4 * i],
                        raw[4 * i + 1],
                        raw[4 * i + 2],
                        raw[4 * i + 3],
                    ]) as i64;
                    max_v = max_v.max(v);
                    min_v = min_v.min(v);
                }
                peak = (max_v.max(-min_v) as f64) / 2_147_483_648.0;
            }
        }
        _ => {}
    }
    Ok((duration, peak))
}

fn has_audio_stream(path: &Path) -> bool {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a",
            "-show_entries",
            "stream=index",
            "-of",
            "json",
            path.to_string_lossy().as_ref(),
        ])
        .output();
    let Ok(output) = output else { return true };
    if !output.status.success() {
        return true;
    }
    let Ok(parsed) = serde_json::from_slice::<Value>(&output.stdout) else {
        return true;
    };
    parsed
        .get("streams")
        .and_then(Value::as_array)
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

/// Normalize any narration input to a quiet, model-friendly WAV
/// (mono 48 kHz) with the system ffmpeg — the same decode the frame
/// sampler relies on, so the room layer carries no media stack of its
/// own. A plain WAV with no ffmpeg on the machine is used as-is.
pub fn extract_narration_audio(
    source_path: &Path,
    kind: &str,
    work_dir: &Path,
    ffmpeg: Option<&str>,
) -> (Option<PathBuf>, Option<Value>) {
    let file_name = source_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ffmpeg_bin = resolve_ffmpeg(ffmpeg);
    if kind == "audio" && suffix_lower(source_path) == ".wav" && ffmpeg_bin.is_none() {
        return (Some(source_path.to_path_buf()), None);
    }
    let Some(ffmpeg_bin) = ffmpeg_bin else {
        return (
            None,
            Some(json!({
                "code": "AUDIO_EXTRACT_FAILED",
                "message": "ffmpeg not found on PATH; the narration track could not be decoded",
            })),
        );
    };
    let _ = std::fs::create_dir_all(work_dir);
    let stem = source_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let wav_path = work_dir.join(format!("{stem}.wav"));
    let ran = Command::new(&ffmpeg_bin)
        .args([
            "-v",
            "error",
            "-i",
            source_path.to_string_lossy().as_ref(),
            "-vn",
            "-ac",
            "1",
            "-ar",
            "48000",
            wav_path.to_string_lossy().as_ref(),
            "-y",
        ])
        .output();
    let (status_ok, stderr) = match &ran {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        ),
        Err(e) => (false, e.to_string()),
    };
    let size = std::fs::metadata(&wav_path).map(|m| m.len()).unwrap_or(0);
    // ffmpeg emits a 44-byte header-only file when there is nothing to
    // decode; that is "no audio", not a WAV.
    if !status_ok || !wav_path.is_file() || size <= 44 {
        if kind == "video" && !has_audio_stream(source_path) {
            return (
                None,
                Some(json!({
                    "code": "NO_AUDIO_TRACK",
                    "message": format!("{file_name} has no audio stream; nothing to transcribe"),
                })),
            );
        }
        let detail: String = stderr.trim().chars().take(300).collect();
        return (
            None,
            Some(json!({
                "code": "AUDIO_EXTRACT_FAILED",
                "message": format!(
                    "ffmpeg could not decode an audio track from {file_name}: {detail}"
                ),
            })),
        );
    }
    (Some(wav_path), None)
}

// ----------------------------------------------------------------------
// The production narration call (Baseten), with failure strings in the
// Python "{TypeName}: {message}" shape the manifest records.
// ----------------------------------------------------------------------

type NarrationResult = Result<Value, String>;

/// One authd surrogate round-trip, mirroring Muse's
/// dynamic_credentials helper: POST {"name": "custom.baseten"} to the
/// auth daemon's unix socket and pick the access_token surrogate.
fn fetch_baseten_surrogate() -> Result<String, String> {
    let sock_path = std::env::var("JARVIS_AUTHD_SOCK")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/run/hatch/auth/authd.sock".to_string());
    let helper_unavailable = |detail: String| {
        format!(
            "RuntimeError: the Baseten credential helper is unavailable ({detail}); \
             narration cannot run on this machine"
        )
    };
    let mut stream = std::os::unix::net::UnixStream::connect(&sock_path)
        .map_err(|e| helper_unavailable(e.to_string()))?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(30)));
    let body = serde_json::to_vec(&json!({ "name": "custom.baseten" }))
        .map_err(|e| helper_unavailable(e.to_string()))?;
    let mut request = format!(
        "POST /v1/credentials/surrogate HTTP/1.1\r\nHost: authd.local\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(&body);
    stream
        .write_all(&request)
        .map_err(|e| helper_unavailable(e.to_string()))?;
    // Read the response head, then exactly Content-Length body bytes
    // (the daemon keeps the connection open; read-to-EOF would hang).
    let mut raw: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    let header_end = loop {
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream
            .read(&mut buf)
            .map_err(|e| helper_unavailable(e.to_string()))?;
        if n == 0 {
            return Err(helper_unavailable(
                "credential helper closed the connection".to_string(),
            ));
        }
        raw.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&raw[..header_end]).into_owned();
    let content_length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("content-length") {
                value.trim().parse().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    while raw.len() < header_end + content_length {
        let n = stream
            .read(&mut buf)
            .map_err(|e| helper_unavailable(e.to_string()))?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
    }
    let resp_body = String::from_utf8_lossy(&raw[header_end..]).into_owned();
    let resp_body = resp_body.as_str();
    let status: u32 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if status != 200 {
        return Err(format!(
            "DynamicCredentialError: credential helper rejected the request with HTTP {status}"
        ));
    }
    let parsed: Value = serde_json::from_str(resp_body).map_err(|_| {
        "DynamicCredentialError: credential helper response was not valid JSON".to_string()
    })?;
    let entries = parsed
        .get("credentials")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let entry = entries
        .iter()
        .find(|c| c.get("name").and_then(Value::as_str) == Some("access_token"))
        .ok_or_else(|| {
            "DynamicCredentialError: missing access_token surrogate for credential \
             custom.baseten"
                .to_string()
        })?;
    let placement = entry
        .get("placement")
        .and_then(Value::as_str)
        .unwrap_or("bearer_header");
    if placement != "bearer_header" {
        return Err(format!(
            "DynamicCredentialError: unsupported surrogate placement {placement:?} for credential custom.baseten"
        ));
    }
    let surrogate = entry.get("surrogate").and_then(Value::as_str).unwrap_or("");
    if !surrogate.starts_with("hsurr:") {
        return Err(
            "DynamicCredentialError: credential helper returned a non-surrogate credential \
             for custom.baseten"
                .to_string(),
        );
    }
    Ok(surrogate.to_string())
}

/// The narration client: through the environment proxy (the egress
/// proxy is where the hsurr surrogate is swapped for the real key, and
/// direct egress to Baseten does not connect from this machine), with
/// the OS trust bundle added because the proxy intercepts TLS with a
/// local CA that webpki roots do not carry — the same trust Python's
/// httpx gets from the OS store.
fn build_narration_client() -> Result<reqwest::blocking::Client, String> {
    let mut builder = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(300));
    let bundle_path = std::env::var("SSL_CERT_FILE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/etc/ssl/certs/ca-certificates.crt".to_string());
    if let Ok(bundle) = std::fs::read_to_string(&bundle_path) {
        for block in bundle.split("-----BEGIN CERTIFICATE-----").skip(1) {
            let Some(end) = block.find("-----END CERTIFICATE-----") else {
                continue;
            };
            let pem = format!(
                "-----BEGIN CERTIFICATE-----{}-----END CERTIFICATE-----",
                &block[..end]
            );
            if let Ok(cert) = reqwest::Certificate::from_pem(pem.as_bytes()) {
                builder = builder.add_root_certificate(cert);
            }
        }
    }
    builder
        .build()
        .map_err(|e| format!("URLError: cannot build HTTP client: {e}"))
}

/// The narration request body. Pure, so tests can pin the budget.
///
/// The max_tokens value is the fix for the P12 failure: it must stay
/// well above the reasoning a real walkthrough generates before any
/// answer text (5,474 tokens on the 58.9 s diagnostic clip).
pub fn narration_payload(data_url: &str) -> Value {
    json!({
        "model": NARRATION_MODEL,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": NARRATION_PROMPT},
                {"type": "audio_url", "audio_url": {"url": data_url}},
            ],
        }],
        "max_tokens": NARRATION_MAX_TOKENS,
    })
}

/// Usage parenthetical for the budget error, when it was reported.
fn narration_budget_remedy(usage: &Value) -> String {
    match usage.get("completion_tokens").and_then(Value::as_i64) {
        Some(completion) => {
            format!(" (usage: {completion} completion tokens, budget {NARRATION_MAX_TOKENS})")
        }
        None => String::new(),
    }
}

/// Turn a Baseten chat-completions body into the narration record.
///
/// Pure, so tests can drive it with stubbed bodies. The failure this
/// guards is silent degradation: a reply whose reasoning ate the
/// whole budget comes back empty (or cut off mid-answer), and an
/// empty transcript would otherwise flow into the manifest as if
/// the seller said nothing. Both shapes error with the cause and a
/// correction path (decision 114); narrate_one records the failure
/// as a NARRATION_FAILED row and the stills run continues.
pub fn interpret_narration_body(body: &Value, latency_ms: i64) -> NarrationResult {
    let choice = body.pointer("/choices/0").cloned().unwrap_or_else(|| json!({}));
    let content = choice
        .pointer("/message/content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let usage = body.get("usage").cloned().unwrap_or_else(|| json!({}));
    let remedy = "Trim the recording to about a minute or less and narrate that, or re-record a shorter walkthrough.";
    if content.trim().is_empty() {
        return Err(format!(
            "RuntimeError: Baseten narration call returned an empty reply: the model's reasoning spent the token budget before any transcript was written{}. {remedy}",
            narration_budget_remedy(&usage)
        ));
    }
    if choice.get("finish_reason").and_then(Value::as_str) == Some("length") {
        return Err(format!(
            "RuntimeError: Baseten narration call was cut off by the token budget before the reply finished (finish_reason 'length'{}); the transcript would be incomplete. {remedy}",
            narration_budget_remedy(&usage)
        ));
    }
    Ok(json!({
        "reply": content,
        "usage": usage,
        "latency_ms": latency_ms,
        "model": body.get("model").cloned().unwrap_or(json!(NARRATION_MODEL)),
    }))
}

/// Build the data: URL for an audio file (the wire shape the spike
/// proved: base64 bytes behind an audio_url content part).
fn narration_data_url(audio_path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(audio_path)
        .map_err(|e| format!("RuntimeError: could not read narration audio: {e}"))?;
    let mime = match suffix_lower(audio_path).as_str() {
        ".wav" => "audio/wav",
        ".mp3" => "audio/mpeg",
        ".flac" => "audio/flac",
        ".ogg" => "audio/ogg",
        ".m4a" => "audio/mp4",
        ".aac" => "audio/aac",
        ".opus" => "audio/ogg",
        _ => "application/octet-stream",
    };
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:{mime};base64,{encoded}"))
}

/// Write one WAV per planned chunk, beside the source WAV.
///
/// Walks the RIFF chunks the way probe_wav does and re-frames the
/// PCM data under a canonical header carrying the source's format.
/// The caller owns the piece files and deletes them; pieces are
/// transport, never deliverables.
pub fn slice_wav_pieces(
    wav_path: &Path,
    chunks: &[(f64, f64)],
) -> Result<Vec<PathBuf>, String> {
    let data = std::fs::read(wav_path).map_err(|e| e.to_string())?;
    if data.len() < 44 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return Err("file does not start with RIFF id".to_string());
    }
    let mut fmt: Option<(u16, u16, u32, usize)> = None; // (format, channels, rate, sampwidth)
    let mut pcm: Option<(usize, usize)> = None; // (start, len)
    let mut pos = 12usize;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let size =
            u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                as usize;
        let start = pos + 8;
        let end = (start + size).min(data.len());
        match id {
            b"fmt " => {
                if end - start >= 16 {
                    let audio_format = u16::from_le_bytes([data[start], data[start + 1]]);
                    let channels = u16::from_le_bytes([data[start + 2], data[start + 3]]);
                    let rate = u32::from_le_bytes([
                        data[start + 4],
                        data[start + 5],
                        data[start + 6],
                        data[start + 7],
                    ]);
                    let bits = u16::from_le_bytes([data[start + 14], data[start + 15]]);
                    fmt = Some((audio_format, channels, rate, (bits as usize + 7) / 8));
                }
            }
            b"data" => {
                pcm = Some((start, end - start));
            }
            _ => {}
        }
        pos = start + size + (size % 2);
    }
    let (audio_format, channels, rate, sampwidth) = fmt.ok_or("fmt chunk not found")?;
    if audio_format != 1 {
        return Err(format!("unknown format: {audio_format}"));
    }
    let (data_start, data_len) = pcm.ok_or("data chunk not found")?;
    let frame_bytes = sampwidth.max(1) * (channels as usize).max(1);
    let stem = wav_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut pieces = Vec::new();
    for (i, (start_s, end_s)) in chunks.iter().enumerate() {
        let start_f = (start_s * rate as f64).round() as usize;
        let end_f = (end_s * rate as f64).round() as usize;
        let byte_from = data_start + (start_f * frame_bytes).min(data_len);
        let byte_to = data_start + (end_f * frame_bytes).min(data_len);
        let piece = wav_path.with_file_name(format!("{stem}.piece{:02}.wav", i + 1));
        write_wav_piece(
            &piece,
            channels,
            rate,
            sampwidth,
            &data[byte_from..byte_to.max(byte_from)],
        )?;
        pieces.push(piece);
    }
    Ok(pieces)
}

fn write_wav_piece(
    path: &Path,
    channels: u16,
    rate: u32,
    sampwidth: usize,
    pcm: &[u8],
) -> Result<(), String> {
    let block_align = channels * sampwidth as u16;
    let byte_rate = rate * block_align as u32;
    let data_len = pcm.len() as u32;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&((sampwidth * 8) as u16).to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    std::fs::write(path, out).map_err(|e| e.to_string())
}

/// Fold per-piece narration records into the file's one record.
///
/// Replies join in timeline order; parse_narration_reply reads the
/// repeated headings as one document, so the transcript reads
/// continuously (a sentence can straddle a piece boundary) and the
/// item lists union. Usage sums the pieces' prompt/completion
/// tokens, which is what the cost estimate prices.
pub fn merge_narration_replies(results: &[Value]) -> Value {
    let token_sum = |key: &str| -> i64 {
        results
            .iter()
            .map(|r| {
                r.pointer(&format!("/usage/{key}"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
            })
            .sum()
    };
    let any_usage = results.iter().any(|r| {
        r.get("usage")
            .and_then(Value::as_object)
            .map(|o| !o.is_empty())
            .unwrap_or(false)
    });
    let usage = if any_usage {
        json!({
            "prompt_tokens": token_sum("prompt_tokens"),
            "completion_tokens": token_sum("completion_tokens"),
        })
    } else {
        json!({})
    };
    json!({
        "reply": results
            .iter()
            .map(|r| r["reply"].as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n"),
        "usage": usage,
        "latency_ms": results
            .iter()
            .map(|r| r["latency_ms"].as_i64().unwrap_or(0))
            .sum::<i64>(),
        "model": results[0]
            .get("model")
            .cloned()
            .unwrap_or(json!(NARRATION_MODEL)),
    })
}

/// Production narration for one extracted WAV, with the per-piece
/// call injected (tests drive it with a fake; production passes the
/// real Baseten call).
///
/// Clips up to a minute make the single call older runs always made.
/// Longer clips are sliced into planned pieces, one call per piece,
/// and the replies merge into the file's one record, so a 5-minute
/// walkthrough narrates end to end instead of dying on the first
/// call's token budget. A WAV the slicer cannot cut (a non-PCM
/// original used as-is) falls back to the single call, which then
/// succeeds or fails loudly on its own budget.
pub fn narrate_with_caller(
    audio_path: &Path,
    call: &dyn Fn(&str) -> NarrationResult,
) -> NarrationResult {
    let duration = probe_wav(audio_path).map(|(d, _)| d).unwrap_or(0.0);
    let chunks = plan_narration_chunks(duration);
    if chunks.len() <= 1 {
        return call(&narration_data_url(audio_path)?);
    }
    let pieces = match slice_wav_pieces(audio_path, &chunks) {
        Ok(pieces) => pieces,
        Err(_) => return call(&narration_data_url(audio_path)?),
    };
    let total = pieces.len();
    let mut results: Vec<Value> = Vec::new();
    let mut failure: Option<String> = None;
    for (i, piece) in pieces.iter().enumerate() {
        let step = narration_data_url(piece).and_then(|url| call(&url));
        match step {
            Ok(result) => results.push(result),
            Err(e) => {
                let inner = e.strip_prefix("RuntimeError: ").unwrap_or(&e);
                failure = Some(format!(
                    "RuntimeError: narration piece {} of {} failed: {}",
                    i + 1,
                    total,
                    inner
                ));
                break;
            }
        }
    }
    for piece in &pieces {
        let _ = std::fs::remove_file(piece);
    }
    if let Some(failure) = failure {
        return Err(failure);
    }
    Ok(merge_narration_replies(&results))
}

/// ONE Muse Spark Open call via Baseten for one piece of audio.
fn call_narration_once(
    client: &reqwest::blocking::Client,
    surrogate: &str,
    data_url: &str,
) -> NarrationResult {
    let payload = narration_payload(data_url);
    let started = Instant::now();
    let response = client
        .post(format!("{BASETEN_BASE_URL}/chat/completions"))
        .bearer_auth(surrogate)
        .json(&payload)
        .send()
        .map_err(|e| format!("URLError: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        let detail = response.text().unwrap_or_default();
        let detail: String = detail.chars().take(500).collect();
        return Err(format!(
            "RuntimeError: Baseten narration call failed: HTTP {}: {detail}",
            status.as_u16()
        ));
    }
    let body: Value = response
        .json()
        .map_err(|_| "DynamicCredentialError: provider response was not valid JSON".to_string())?;
    let latency_ms = python_round(started.elapsed().as_secs_f64() * 1000.0, 0) as i64;
    interpret_narration_body(&body, latency_ms)
}

/// The production narration entry (Baseten Spark Open, partner pool).
///
/// The key lives behind Muse's credential helper and is never read
/// here: the helper issues an hsurr surrogate that the egress proxy
/// swaps for the real key, exactly the discipline the Python helper
/// (send_audio.py in the voice spike) proved on 2026-10-28.
pub fn default_narration_fn(audio_path: &Path) -> NarrationResult {
    let surrogate = fetch_baseten_surrogate()?;
    let client = build_narration_client()?;
    narrate_with_caller(audio_path, &|data_url: &str| {
        call_narration_once(&client, &surrogate, data_url)
    })
}

fn empty_narration_record(path: &Path, kind: &str) -> Value {
    json!({
        "path": path.to_string_lossy(),
        "kind": kind,
        "audio_file": Value::Null,
        "duration_s": Value::Null,
        "prepared_duration_s": Value::Null,
        "peak_fraction": Value::Null,
        "transcript": Value::Null,
        "named_items": [],
        "kept_items": [],
        "mentioned_items": [],
        "usage": Value::Null,
        "projected_cost_usd": Value::Null,
        "cost_usd_estimate": Value::Null,
        "latency_ms": Value::Null,
        "error": Value::Null,
    })
}

/// One narration file end to end. Every failure mode is a manifest
/// row (a narration failure degrades the run to stills-only; it never
/// kills it).
fn narrate_one(
    source_path: &Path,
    kind: &str,
    work_dir: &Path,
    narration_fn: &NarrationFn,
    max_projected_usd: f64,
) -> Value {
    let mut record = empty_narration_record(source_path, kind);
    let (wav_path, error) = extract_narration_audio(source_path, kind, work_dir, None);
    if let Some(error) = error {
        record["error"] = error;
        return record;
    }
    let wav_path = wav_path.expect("extract returns a path when there is no error");
    record["audio_file"] = json!(wav_path.to_string_lossy());
    let (duration, peak) = match probe_wav(&wav_path) {
        Ok(pair) => pair,
        Err(exc) => {
            record["error"] = json!({
                "code": "AUDIO_PROBE_FAILED",
                "message": format!("could not measure the narration track: {exc}"),
            });
            return record;
        }
    };
    record["duration_s"] = json!(python_round(duration, 2));
    record["peak_fraction"] = json!(python_round(peak, 4));
    if duration > NARRATION_MAX_DURATION_S {
        record["error"] = json!({
            "code": "NARRATION_TOO_LONG",
            "message": format!(
                "narration runs {duration:.0}s, over the 5-minute limit for a narrated clip; upload a shorter video (5 minutes or less), or trim this one to the part that matters, and run it again"
            ),
        });
        return record;
    }
    let projected = project_narration_cost_usd(duration, NARRATION_MAX_TOKENS);
    record["projected_cost_usd"] = json!(python_round(projected, 6));
    if projected > max_projected_usd {
        record["error"] = json!({
            "code": "NARRATION_COST_REFUSED",
            "message": format!(
                "projected narration cost ${projected:.4} exceeds the per-file cap \
                 ${max_projected_usd:.2}; the narration was refused before any call was made"
            ),
        });
        return record;
    }
    if peak < NARRATION_SILENCE_PEAK {
        record["error"] = json!({
            "code": "SILENT_TRACK",
            "message": format!(
                "the narration track peaks at {peak:.4} of full scale, under the \
                 -40 dBFS floor; there is no speech to transcribe"
            ),
        });
        return record;
    }
    let (send_path, prepared_s) = prepare_narration_audio(&wav_path);
    if let Some(prepared) = prepared_s {
        record["prepared_duration_s"] = json!(python_round(prepared, 2));
    }
    let result = match narration_fn(&send_path) {
        Ok(result) => result,
        Err(message) => {
            record["error"] = json!({
                "code": "NARRATION_FAILED",
                "message": message,
            });
            return record;
        }
    };
    let parsed = parse_narration_reply(result.get("reply").and_then(Value::as_str));
    record["transcript"] = json!(parsed.transcript);
    record["named_items"] = json!(parsed.selling);
    record["kept_items"] = json!(parsed.keep);
    record["mentioned_items"] = json!(parsed.mentioned);
    record["usage"] = result.get("usage").cloned().unwrap_or_else(|| json!({}));
    record["latency_ms"] = result.get("latency_ms").cloned().unwrap_or(Value::Null);
    let usage_ref = if record["usage"].is_null() {
        None
    } else {
        Some(&record["usage"])
    };
    let cost = narration_cost_usd(usage_ref);
    record["cost_usd_estimate"] = match cost {
        Some(c) => json!(python_round(c, 6)),
        None => record["projected_cost_usd"].clone(),
    };
    record
}

// ----------------------------------------------------------------------
// The tool chain (injectable, exactly like the Python room runner)
// ----------------------------------------------------------------------

pub type ChainFn1 = Box<dyn Fn(&str) -> Result<Value, String>>;
pub type ChainFn2 = Box<dyn Fn(&str, &str) -> Result<Value, String>>;
pub type VerifyFn = Box<dyn Fn(&AssetStore, &str) -> Result<Value, String>>;
pub type SpendProbe = Box<dyn Fn() -> Result<f64, String>>;
pub type NarrationFn = Box<dyn Fn(&Path) -> NarrationResult>;

pub struct RoomChain {
    pub identify: ChainFn1,
    pub cut_out: ChainFn2,
    pub draw: ChainFn2,
}

pub fn default_chain() -> RoomChain {
    RoomChain {
        identify: Box::new(|image_ref: &str| crate::server::tool_identify(image_ref)),
        cut_out: Box::new(|image_ref: &str, concept: &str| {
            crate::server::tool_cut_out(image_ref, Some(concept.to_string()))
        }),
        draw: Box::new(|asset: &str, scene: &str| {
            crate::server::tool_draw(asset, scene).map(|(result, _bytes)| result)
        }),
    }
}

pub fn default_verify_fn() -> VerifyFn {
    Box::new(|store: &AssetStore, handle: &str| {
        composite::verify_receipt_from_disk(store, handle, None)
    })
}

pub struct RunContext<'a> {
    pub store: AssetStore,
    pub chain: RoomChain,
    pub scene: String,
    pub stage_mode: String,
    pub verify_fn: &'a VerifyFn,
    pub max_photos_per_item: usize,
    pub max_sam_calls_per_item: usize,
    pub sam_calls: std::cell::Cell<usize>,
    /// P1 step log (tracker event 372): durable per-step state
    /// for crash resume. None = unlogged run (pre-P1 behavior).
    pub step_log: Option<std::rc::Rc<crate::sqlledger::RunStepLog>>,
    /// The run id the step log keys on (empty when unlogged).
    pub run_id: String,
    /// Billed call ids returned by chain calls since the last
    /// drain; the step logger records them against the open step
    /// so a pending step's settled calls stay attributable.
    pub call_ids: std::cell::RefCell<Vec<String>>,
}

/// Record a chain result's billed call id, when it carries one,
/// for the step log.
fn note_call_id(ctx: &RunContext, result: &Value) {
    if let Some(cid) = result.get("call_id").and_then(Value::as_str) {
        ctx.call_ids.borrow_mut().push(cid.to_string());
    }
}

/// Drain the accumulated call ids, returning the most recent.
fn take_last_call_id(ctx: &RunContext) -> Option<String> {
    let mut ids = ctx.call_ids.borrow_mut();
    let last = ids.last().cloned();
    ids.clear();
    last
}

/// What a chain result cost, billing-honestly: its own settled cost
/// when it has one, else the amount its error says was counted.
pub fn call_cost(result: &Value) -> f64 {
    if !result.is_object() {
        return 0.0;
    }
    if let Some(cost) = result.get("cost_usd") {
        if !cost.is_null() {
            return cost.as_f64().unwrap_or(0.0);
        }
    }
    result
        .get("error")
        .and_then(|e| e.get("cost_counted_usd"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
}

fn opaque_pixels(store: &AssetStore, handle: Option<&str>) -> Option<i64> {
    let handle = handle?;
    let image = store.read_image(handle).ok()?;
    Some(image.alpha_plane().iter().filter(|a| **a > 0).count() as i64)
}

/// Rank the pool for one item: photos whose plan text mentions the
/// item come first (head noun beats modifiers), stills before video
/// frames at equal evidence, original order as the final tiebreak.
pub fn rank_photos(item_name: &str, sources: &[PhotoSource]) -> Vec<usize> {
    let item_tokens = token_set(Some(item_name));
    let item_token_vec = tokens_of(Some(item_name));
    let head = item_token_vec.last().map(String::as_str).unwrap_or("");
    let scored: Vec<(usize, f64)> = sources
        .iter()
        .enumerate()
        .map(|(idx, source)| {
            let plan = source.plan.as_ref();
            let text_tokens = token_set(Some(plan_text(plan).as_str()));
            let mut score = 0.0;
            if !item_tokens.is_empty() && !text_tokens.is_empty() {
                let shared = item_tokens
                    .iter()
                    .filter(|t| token_matches(t, &text_tokens))
                    .count();
                let mut bonus = shared as f64 / item_tokens.len() as f64;
                // Head-noun rule (learned on the v1 room run): a photo
                // whose plan names the item's head noun ("black hat")
                // outranks one that merely shares a modifier ("black
                // hat" in the plan for a "black box" item). The shared
                // token fraction alone cannot tell those apart.
                if token_matches(&head, &text_tokens) {
                    bonus += 1.0;
                }
                score = bonus;
            }
            (idx, score)
        })
        .collect();
    let mut order: Vec<usize> = (0..sources.len()).collect();
    order.sort_by(|&a, &b| {
        let sa = scored[a].1;
        let sb = scored[b].1;
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                let a_still = sources[a].origin == "still";
                let b_still = sources[b].origin == "still";
                b_still.cmp(&a_still)
            })
            .then_with(|| a.cmp(&b))
    });
    order
}

/// Apply the per-item candidate cap with still coverage: every
/// still photo is always in the candidate set, and video frames
/// fill the remaining cap slots in rank order. The returned list
/// keeps the ranking's probe order. (The acceptance run's black
/// hat: the cap filled with video frames and the one still holding
/// the fedora was never probed, so the item staged a different hat
/// from a frame instead.)
pub fn apply_candidate_coverage(
    ranked: &[usize],
    sources: &[PhotoSource],
    cap: usize,
) -> Vec<usize> {
    let mut chosen: HashSet<usize> = ranked
        .iter()
        .copied()
        .filter(|&idx| sources[idx].origin == "still")
        .collect();
    let mut count = chosen.len();
    for &idx in ranked {
        if count >= cap {
            break;
        }
        if sources[idx].origin != "still" {
            chosen.insert(idx);
            count += 1;
        }
    }
    ranked
        .iter()
        .copied()
        .filter(|idx| chosen.contains(idx))
        .collect()
}

/// Probe one item against one photo: SAM gets a small, ordered set of
/// concept phrasings and a hard per-item call budget shared across
/// photos. A "photo does not contain it" answer is a sighting row.
fn probe_photo(
    ctx: &RunContext,
    item_name: &str,
    source: &PhotoSource,
    photo_index: usize,
) -> Result<Value, String> {
    let Some(handle) = source.handle.clone() else {
        return Ok(json!({
            "photo_index": photo_index,
            "status": "error",
            "concept_used": Value::Null,
            "foreground_asset": Value::Null,
            "mask_asset": Value::Null,
            "mask_overlay_asset": Value::Null,
            "opaque_pixels": Value::Null,
            "bbox_normalized": Value::Null,
            "mask_warnings": [],
            "cost_usd": 0.0,
            "error": {"code": "NO_HANDLE", "message": "photo was not ingested"},
        }));
    };
    let variants = concept_variants(item_name, source.plan.as_ref());
    let mut tried = 0usize;
    let mut photo_cost = 0.0f64;
    let mut success: Option<(String, Value)> = None;
    let mut last_error: Option<Value> = None;
    for variant in &variants {
        if tried >= MAX_VARIANTS_PER_PHOTO || ctx.sam_calls.get() >= ctx.max_sam_calls_per_item {
            break;
        }
        tried += 1;
        ctx.sam_calls.set(ctx.sam_calls.get() + 1);
        // A raise-class failure (spend cap, integrity) propagates and
        // refuses the run, exactly as the Python exception does.
        let result = (ctx.chain.cut_out)(&handle, variant)?;
        note_call_id(ctx, &result);
        photo_cost += call_cost(&result);
        if result.get("status").and_then(Value::as_str) == Some("ok") {
            success = Some((variant.clone(), result));
            break;
        }
        let error = result.get("error").cloned().unwrap_or(Value::Null);
        last_error = Some(if !py_truthy(&error) {
            json!({"code": "UNKNOWN", "message": result.to_string()})
        } else {
            error
        });
    }
    if let Some((variant, result)) = success {
        let review = result
            .get("mask_review")
            .filter(|r| py_truthy(r))
            .cloned()
            .unwrap_or_else(|| json!({}));
        let fg = result.get("output_asset").and_then(Value::as_str);
        return Ok(json!({
            "photo_index": photo_index,
            "status": "ok",
            "concept_used": variant,
            "foreground_asset": result.get("output_asset").cloned().unwrap_or(Value::Null),
            "mask_asset": result.get("mask_asset").cloned().unwrap_or(Value::Null),
            "mask_overlay_asset": result.get("mask_overlay_asset").cloned().unwrap_or(Value::Null),
            "opaque_pixels": opaque_pixels(&ctx.store, fg),
            "bbox_normalized": normalized_bbox(review.get("bbox"), source.width, source.height),
            "mask_warnings": review.get("warnings").cloned().unwrap_or_else(|| json!([])),
            "cost_usd": photo_cost,
            "error": Value::Null,
        }));
    }
    Ok(json!({
        "photo_index": photo_index,
        "status": "error",
        // A failed sighting still records the phrasing it was probed
        // under: the FIRST variant (the item's primary phrasing).
        "concept_used": variants.first().cloned().map(Value::String).unwrap_or(Value::Null),
        "foreground_asset": Value::Null,
        "mask_asset": Value::Null,
        "mask_overlay_asset": Value::Null,
        "opaque_pixels": Value::Null,
        "bbox_normalized": Value::Null,
        "mask_warnings": [],
        "cost_usd": photo_cost,
        "variants_tried": tried,
        "error": last_error.unwrap_or(Value::Null),
    }))
}

/// The independent verifier, wrapped so a verifier that cannot run is
/// a failed verification with an honest problem line — not a room
/// failure and never a silent pass.
fn run_verification(ctx: &RunContext, handle: &str) -> Value {
    let report = match (ctx.verify_fn)(&ctx.store, handle) {
        Ok(report) => report,
        Err(exc) => {
            return json!({
                "ok": false,
                "changed_opaque_pixels": Value::Null,
                "opaque_pixels": Value::Null,
                "color_profile_preserved": Value::Null,
                "problems": [format!("the independent verifier could not run: {exc}")],
                "checks_failed": [],
            });
        }
    };
    let recomputed = report
        .get("recomputed")
        .filter(|r| r.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let checks = report
        .get("checks")
        .filter(|c| c.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut checks_failed: Vec<String> = checks
        .as_object()
        .map(|map| {
            map.iter()
                .filter(|(_, v)| !py_truthy(v))
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default();
    checks_failed.sort();
    json!({
        "ok": report.get("ok").map(py_truthy).unwrap_or(false),
        "changed_opaque_pixels": recomputed
            .get("changed_opaque_pixels")
            .cloned()
            .unwrap_or(Value::Null),
        "opaque_pixels": recomputed.get("opaque_pixels").cloned().unwrap_or(Value::Null),
        "color_profile_preserved": recomputed
            .get("color_profile_preserved")
            .cloned()
            .unwrap_or(Value::Null),
        "problems": report.get("problems").cloned().unwrap_or_else(|| json!([])),
        "checks_failed": checks_failed,
    })
}

/// Pixel rectangle for an item crop: the foreground's normalized
/// bbox expanded by a 10% margin on each side (10% of the bbox's
/// own width horizontally, of its height vertically), clamped to
/// the frame. Computed in pixel space, as the reference build
/// does. Returns (x0, y0, x1, y1) in pixels, or None when the bbox
/// is missing or the rectangle is empty.
pub fn expanded_crop_rect(
    bbox_normalized: Option<&Value>,
    width: u32,
    height: u32,
) -> Option<(u32, u32, u32, u32)> {
    let b = bbox_vec(bbox_normalized)?;
    let (w, h) = (width as f64, height as f64);
    let box_w = (b[2] - b[0]) * w;
    let box_h = (b[3] - b[1]) * h;
    let px0 = ((b[0] * w - 0.10 * box_w).floor()).max(0.0) as u32;
    let py0 = ((b[1] * h - 0.10 * box_h).floor()).max(0.0) as u32;
    let px1 = ((b[2] * w + 0.10 * box_w).ceil() as u32).min(width);
    let py1 = ((b[3] * h + 0.10 * box_h).ceil() as u32).min(height);
    if px1 <= px0 || py1 <= py0 {
        return None;
    }
    Some((px0, py0, px1, py1))
}

/// Crop an RGBA image to a pixel rectangle.
pub fn crop_image(
    img: &crate::image::RgbaImage,
    rect: (u32, u32, u32, u32),
) -> crate::image::RgbaImage {
    let (x0, y0, x1, y1) = rect;
    let w = x1 - x0;
    let h = y1 - y0;
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        let src = (((y0 + y) * img.width + x0) * 4) as usize;
        let dst = (y * w * 4) as usize;
        pixels[dst..dst + (w * 4) as usize]
            .copy_from_slice(&img.pixels[src..src + (w * 4) as usize]);
    }
    crate::image::RgbaImage {
        width: w,
        height: h,
        pixels,
        icc_profile: img.icc_profile.clone(),
    }
}

/// The no-copy note every empty listing carries; the crop path
/// appends its specific failure reason to it.
pub const NO_COPY_NOTE: &str = "the source photo's identify plan described a different primary \
    subject; per-item listing copy was not generated rather than \
    borrowed from another item";

fn none_listing(reason: &str) -> Value {
    json!({
        "title": Value::Null,
        "bullets": [],
        "condition_notes": Value::Null,
        "source": "none",
        "note": format!("{NO_COPY_NOTE}; {reason}"),
        "crop_asset": Value::Null,
    })
}

/// Listing copy from the item's own crop: crop the primary source
/// image to the item foreground's bbox (plus margin), store the
/// crop, and run the chain's identify on it. Returns the listing
/// record, the identify cost, and the crop-identify plan itself
/// when it supplied the copy (the collision post-pass reads it as
/// the item's listing evidence); any failure degrades to the
/// honest "none" record with the reason appended to its note, and
/// a billed failure's counted cost still rides back so the item's
/// cost stays honest.
fn crop_listing(
    ctx: &RunContext,
    primary: &Value,
    primary_source: &PhotoSource,
) -> (Value, f64, Option<Value>) {
    let bbox = primary.get("bbox_normalized");
    let bbox_missing = bbox
        .and_then(Value::as_array)
        .map(|a| a.is_empty())
        .unwrap_or(true);
    if bbox_missing || primary_source.handle.is_none() {
        return (
            none_listing("no foreground box was recorded for the item crop"),
            0.0,
            None,
        );
    }
    let handle = primary_source.handle.as_deref().expect("checked above");
    let Ok(source_image) = ctx.store.read_image(handle) else {
        return (
            none_listing("the item crop could not be read from the source photo"),
            0.0,
            None,
        );
    };
    let Some(rect) = expanded_crop_rect(bbox, source_image.width, source_image.height) else {
        return (none_listing("the item crop box was empty"), 0.0, None);
    };
    let cropped = crop_image(&source_image, rect);
    let source_asset_id = AssetStore::parse_handle(handle).ok();
    let Ok(crop_handle) = ctx
        .store
        .put_image(&cropped, KIND_CROP, source_asset_id.as_deref())
    else {
        return (none_listing("the item crop could not be stored"), 0.0, None);
    };
    let result = match (ctx.chain.identify)(&crop_handle) {
        Ok(result) => result,
        Err(_) => {
            return (
                none_listing("the item-crop identify attempt failed"),
                0.0,
                None,
            )
        }
    };
    note_call_id(ctx, &result);
    let cost = call_cost(&result);
    let title = result.get("suggested_title");
    if result.get("status").and_then(Value::as_str) == Some("ok")
        && title.map(py_truthy).unwrap_or(false)
    {
        let bullets = result
            .get("listing_bullets")
            .filter(|b| py_truthy(b))
            .cloned()
            .unwrap_or_else(|| json!([]));
        return (
            json!({
                "title": title.cloned().unwrap_or(Value::Null),
                "bullets": bullets,
                "condition_notes": result.get("condition_notes").cloned().unwrap_or(Value::Null),
                "source": "item_crop",
                "note": Value::Null,
                "crop_asset": crop_handle,
            }),
            cost,
            Some(result),
        );
    }
    (
        none_listing("the item-crop identify attempt failed"),
        cost,
        None,
    )
}

/// Draw one foreground against a code-rendered backdrop plate:
/// the plate is rendered at the foreground's frame size, stored as
/// a plate asset, and composited through the production draw path
/// so the receipt pins its hashes. No model call; the returned
/// result is shaped like the chain's draw result with cost 0.
fn backdrop_draw(ctx: &RunContext, fg_handle: &str) -> Value {
    let fail = |message: String| {
        json!({
            "status": "error",
            "error": {"code": "DRAW_FAILED", "message": message},
        })
    };
    let foreground = match ctx.store.read_image(fg_handle) {
        Ok(img) => img,
        Err(e) => return fail(e),
    };
    let Some(plate) = backdrop::render_plate(&ctx.stage_mode, foreground.width, foreground.height)
    else {
        return fail(format!("unknown stage mode {:?}", ctx.stage_mode));
    };
    let plate_handle = match ctx.store.put_image(&plate, KIND_PLATE, None) {
        Ok(handle) => handle,
        Err(e) => return fail(e),
    };
    match composite::draw_product_with_plate_kind(
        &ctx.store,
        fg_handle,
        &ctx.scene,
        Some(&plate_handle),
        Some((
            "code_rendered",
            backdrop::backdrop_receipt_value(&ctx.stage_mode),
        )),
    ) {
        Ok((handle, receipt)) => json!({
            "status": "ok",
            "output_asset": handle,
            "fidelity_receipt": receipt,
            "cost_usd": 0.0,
        }),
        Err(e) => fail(e),
    }
}

pub struct WorkItem {
    pub name: String,
    pub candidate_photo_indices: Vec<usize>,
    /// True when the candidate list is a full rank_photos ordering
    /// (named and narration items): the still-coverage rule applies
    /// to those. Auto-mode groups carry only the photos whose plans
    /// named the concept, and keep their list as built.
    pub ranked_candidates: bool,
    pub flags: Vec<String>,
    pub narration: Option<Value>,
    /// How the item entered the run: "named" (typed by the seller
    /// or found by auto mode), "narration_selling" (named for sale
    /// in the narration), or "narration_mentioned" (mentioned in
    /// passing; the visual pipeline confirms or refutes it).
    pub origin: String,
}

/// The probe phase's outcome: the record so far (sightings and
/// primary set, or the failure filled in), the primary sighting
/// when one was chosen, and what the phase spent.
struct ProbeOutcome {
    record: Value,
    primary: Option<Value>,
    cost: f64,
}

/// One item's probe phase: probe the candidate photos, pick the
/// strongest sighting. `prior_sightings` seeds the phase with
/// checkpointed sightings from a crashed attempt (P1 resume):
/// those photos are not re-probed and not re-billed. `on_photo`,
/// when given, fires after each NEW sighting with the accumulated
/// sightings + cost, so the caller can checkpoint progress.
fn probe_item(
    ctx: &RunContext,
    item: &WorkItem,
    sources: &[PhotoSource],
    probe_all_candidates: bool,
    prior_sightings: &[Value],
    mut on_photo: Option<&mut dyn FnMut(&[Value], f64)>,
) -> Result<ProbeOutcome, String> {
    let name = item.name.clone();
    let mut record = json!({
        "name": name,
        "status": "failed",
        "stage_reached": "probe",
        "failure": Value::Null,
        "narration": item.narration,
        "origin": item.origin,
        "intent": intent_of_origin(&item.origin),
        "stage_mode": ctx.stage_mode,
        "pricing": Value::Null,
        "sightings": [],
        "primary_photo_index": Value::Null,
        "also_seen_in": [],
        "output_asset": Value::Null,
        "receipt": Value::Null,
        "receipt_file": Value::Null,
        "verification": Value::Null,
        "listing": Value::Null,
        "ownership": Value::Null,
        "possible_duplicate_of": [],
        "flags": item.flags,
        "cost_usd": 0.0,
    });
    let mut sightings: Vec<Value> = prior_sightings.to_vec();
    let mut item_cost: f64 = sightings
        .iter()
        .map(|s| s.get("cost_usd").and_then(Value::as_f64).unwrap_or(0.0))
        .sum();
    let probed: std::collections::HashSet<i64> = sightings
        .iter()
        .filter_map(|s| s.get("photo_index").and_then(Value::as_i64))
        .collect();
    // A checkpointed ok sighting already satisfied the
    // stop-at-first-ok rule in the crashed attempt; do not probe
    // further on resume.
    let prior_satisfied = !probe_all_candidates
        && sightings
            .iter()
            .any(|s| s.get("status").and_then(Value::as_str) == Some("ok"));
    if !prior_satisfied {
        for &photo_index in &item.candidate_photo_indices {
            if probed.contains(&(photo_index as i64)) {
                continue;
            }
            let sighting = probe_photo(ctx, &name, &sources[photo_index], photo_index)?;
            item_cost += sighting
                .get("cost_usd")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let is_ok = sighting.get("status").and_then(Value::as_str) == Some("ok");
            sightings.push(sighting);
            if let Some(cb) = on_photo.as_deref_mut() {
                cb(&sightings, item_cost);
            }
            if is_ok && !probe_all_candidates {
                break;
            }
        }
    }
    record["sightings"] = json!(sightings);
    // Primary = the sighting with the most opaque foreground pixels
    // (the fullest view of the item); ties go to a still over a video
    // frame, then to the earlier photo.
    let ok_sightings: Vec<&Value> = sightings
        .iter()
        .filter(|s| s.get("status").and_then(Value::as_str) == Some("ok"))
        .collect();
    if ok_sightings.is_empty() {
        // The failure takes the first error sighting's code (Python
        // dict.get semantics: a present-but-null code stays null, a
        // missing code defaults to NO_MATCH).
        let first_error = sightings
            .iter()
            .find(|s| s.get("status").and_then(Value::as_str) == Some("error"))
            .and_then(|s| s.get("error"))
            .filter(|e| py_truthy(e));
        let mut code = match first_error {
            Some(error) => error.get("code").cloned().unwrap_or(json!("NO_MATCH")),
            None => json!("NO_MATCH"),
        };
        if item.origin == "narration_mentioned" {
            // A mentioned item the photos refute is a NO_MATCH by
            // definition: it was probed and not found. The
            // underlying probe code stays visible in the sightings.
            code = json!("NO_MATCH");
        }
        record["failure"] = json!({
            "stage": "cut_out",
            "code": code,
            "message": format!(
                "no candidate photo yielded a cut-out for {} ({} photo(s) probed)",
                py_repr_str(&name),
                sightings.len()
            ),
        });
        record["cost_usd"] = json!(item_cost);
        return Ok(ProbeOutcome {
            record,
            primary: None,
            cost: item_cost,
        });
    }
    // Python max() semantics: the FIRST sighting with the greatest
    // (opaque_pixels, is_still) key wins; strictly-greater replaces.
    let sighting_key = |s: &Value| -> (i64, bool) {
        let idx = s.get("photo_index").and_then(Value::as_u64).unwrap_or(0) as usize;
        (
            s.get("opaque_pixels").and_then(Value::as_i64).unwrap_or(0),
            sources[idx].origin == "still",
        )
    };
    let mut primary: &Value = ok_sightings[0];
    for s in &ok_sightings[1..] {
        if sighting_key(s) > sighting_key(primary) {
            primary = s;
        }
    }
    let primary_index = primary
        .get("photo_index")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    record["primary_photo_index"] = json!(primary_index);
    record["also_seen_in"] = json!(ok_sightings
        .iter()
        .map(|s| s.get("photo_index").cloned().unwrap_or(Value::Null))
        .filter(|idx| *idx != json!(primary_index))
        .collect::<Vec<_>>());
    Ok(ProbeOutcome {
        record,
        primary: Some(primary.clone()),
        cost: item_cost,
    })
}

/// One item end to end, unlogged: probe, pick the strongest
/// sighting, draw, verify. The record is manifest-shaped from
/// birth; every exit is a filled record, not an exception.
fn process_item(
    ctx: &RunContext,
    item: &WorkItem,
    sources: &[PhotoSource],
    probe_all_candidates: bool,
) -> Result<(Value, Option<Value>), String> {
    let outcome = probe_item(ctx, item, sources, probe_all_candidates, &[], None)?;
    let Some(primary) = outcome.primary else {
        return Ok((outcome.record, None));
    };
    let mut record = outcome.record;
    if item.origin == "narration_keep" {
        // KEEP: identify + cut_out only. No listing, no staging,
        // no draw — the ownership record is the deliverable.
        let keep_cost = keep_from_primary(ctx, &mut record, &item.name, sources, &primary)?;
        record["cost_usd"] = json!(outcome.cost + keep_cost);
        return Ok((record, None));
    }
    let stage = stage_from_primary(ctx, &mut record, &item.name, sources, &primary)?;
    record["cost_usd"] = json!(outcome.cost + stage.cost);
    Ok((record, stage.evidence))
}

/// The manifest intent vocabulary, identical in both builds:
/// selling | keep | mentioned, derived from the item's origin.
/// Typed and auto items are the seller's sale list by default.
pub fn intent_of_origin(origin: &str) -> &'static str {
    match origin {
        "narration_mentioned" => "mentioned",
        "narration_keep" => "keep",
        _ => "selling",
    }
}

/// Grade a kept item's cut from the sighting's review signals.
/// In this build the mask review carries the signals the Python
/// build's cut-quality gate grades RESHOOT on: a mask whose box
/// runs off a frame edge is the occlusion signature (the photo
/// itself cut the item off), and review warnings mean the cut is
/// not clean. For keep items the grade rides INSIDE the ownership
/// record with its retake instruction instead of blocking the
/// record (decision 114: the failure carries its correction path;
/// the record is the deliverable, the instruction improves it).
fn keep_cut_quality(primary: &Value) -> Value {
    let mut touches: Vec<&str> = Vec::new();
    if let Some(b) = primary.get("bbox_normalized").and_then(Value::as_array) {
        if b.len() == 4 {
            let g = |i: usize| b[i].as_f64().unwrap_or(-1.0);
            if g(0) == 0.0 {
                touches.push("left");
            }
            if g(1) == 0.0 {
                touches.push("top");
            }
            if g(2) == 1.0 {
                touches.push("right");
            }
            if g(3) == 1.0 {
                touches.push("bottom");
            }
        }
    }
    let warnings = primary
        .get("mask_warnings")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let warn_count = warnings.as_array().map(|a| a.len()).unwrap_or(0);
    if touches.is_empty() && warn_count == 0 {
        return json!({
            "grade": "PASS",
            "reshoot_instruction": Value::Null,
            "mask_warnings": warnings,
        });
    }
    let edge_text = if touches.is_empty() {
        "the mask review flagged this cut".to_string()
    } else {
        format!(
            "it currently runs off the {} edge{}",
            touches.join(" and "),
            if touches.len() > 1 { "s" } else { "" }
        )
    };
    json!({
        "grade": "RESHOOT",
        "reshoot_instruction": format!(
            "Retake the photo a step further back so the whole item sits inside \
             the frame — {edge_text}. The ownership record stands as issued; a \
             cleaner photo can re-issue it."
        ),
        "mask_warnings": warnings,
    })
}

/// Independent verification of an ownership record, in the
/// composite verifier's shape (ok / problems / checks): re-derive
/// the crop from the stored source photo's pixels and compare
/// pixel hashes. "0 changed" here is a recomputation: the crop
/// provably comes from exactly the stored source pixels, and the
/// receipt's source file hash binds those pixels to the original
/// file as ingested.
pub fn verify_ownership_record(store: &AssetStore, receipt: &Value) -> Value {
    let mut problems: Vec<String> = Vec::new();
    let mut checks: Vec<Value> = Vec::new();
    let mut note = |name: &str, ok: bool, detail: String, problems: &mut Vec<String>| {
        if !ok {
            problems.push(format!("{name}: {detail}"));
        }
        checks.push(json!({"name": name, "ok": ok, "detail": detail}));
    };
    let mut crop_pixels_checked: u64 = 0;
    let mut crop_pixels_changed: u64 = 0;
    let source_handle = receipt
        .pointer("/source/asset")
        .and_then(Value::as_str)
        .unwrap_or("");
    let source_image = store.read_image(source_handle);
    match &source_image {
        Ok(img) => {
            let actual = img.pixel_sha256();
            let claimed = receipt
                .pointer("/source/pixel_sha256")
                .and_then(Value::as_str)
                .unwrap_or("");
            note(
                "source_pixels_match",
                actual == claimed,
                format!("stored source pixels hash {actual}"),
                &mut problems,
            );
        }
        Err(e) => note(
            "source_pixels_match",
            false,
            format!("source photo unreadable: {e}"),
            &mut problems,
        ),
    }
    if let Ok(img) = &source_image {
        let rect = receipt
            .pointer("/crop/rect_px")
            .and_then(Value::as_array)
            .map(|a| {
                (
                    a.first().and_then(Value::as_u64).unwrap_or(0) as u32,
                    a.get(1).and_then(Value::as_u64).unwrap_or(0) as u32,
                    a.get(2).and_then(Value::as_u64).unwrap_or(0) as u32,
                    a.get(3).and_then(Value::as_u64).unwrap_or(0) as u32,
                )
            });
        match rect {
            Some((x0, y0, x1, y1)) if x1 > x0 && y1 > y0 && x1 <= img.width && y1 <= img.height => {
                let recomputed = crop_image(img, (x0, y0, x1, y1));
                crop_pixels_checked = recomputed.width as u64 * recomputed.height as u64;
                let claimed = receipt
                    .pointer("/crop/pixel_sha256")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let actual = recomputed.pixel_sha256();
                if actual == claimed {
                    note(
                        "crop_recomputed_from_source",
                        true,
                        "re-cropping the stored source at the receipt's rect reproduces the crop pixel hash".to_string(),
                        &mut problems,
                    );
                } else {
                    // Hash differs: count the actual differing
                    // pixels against the stored crop when it has
                    // the same shape, so "changed" stays a count,
                    // never a guess.
                    let stored = receipt
                        .pointer("/crop/asset")
                        .and_then(Value::as_str)
                        .and_then(|h| store.read_image(h).ok());
                    if let Some(stored) = stored {
                        if stored.width == recomputed.width && stored.height == recomputed.height
                        {
                            crop_pixels_changed = stored
                                .pixels
                                .chunks_exact(4)
                                .zip(recomputed.pixels.chunks_exact(4))
                                .filter(|(a, b)| a != b)
                                .count() as u64;
                        }
                    }
                    note(
                        "crop_recomputed_from_source",
                        false,
                        format!("recomputed crop hash {actual} != receipt {claimed}"),
                        &mut problems,
                    );
                }
            }
            _ => note(
                "crop_recomputed_from_source",
                false,
                "receipt crop rect is missing or outside the source frame".to_string(),
                &mut problems,
            ),
        }
    }
    json!({
        "ok": problems.is_empty(),
        "method": "ownership_recompute",
        "checks": checks,
        "problems": problems,
        "crop_pixels_checked": crop_pixels_checked,
        "crop_pixels_changed": crop_pixels_changed,
    })
}

/// Build the ownership record for one keep item from its primary
/// sighting: crop the source photo to the item, ask identify about
/// the crop (the same single call the listing path rides — its
/// condition focus supplies the condition notes, no second call),
/// and bind everything into a receipt: the full source FILE's hash
/// and its EXIF capture time (both taken from the file at ingest),
/// the crop's own proof, and the cut-quality grade. The claim the
/// receipt makes is exactly: this crop comes from exactly this
/// file, captured at the time the file states, unchanged since —
/// never that the file was unedited at capture, which no tool can
/// know. Fills the record; returns what the pass cost.
fn keep_from_primary(
    ctx: &RunContext,
    record: &mut Value,
    name: &str,
    sources: &[PhotoSource],
    primary: &Value,
) -> Result<f64, String> {
    record["status"] = json!("failed");
    record["failure"] = Value::Null;
    let primary_index = primary
        .get("photo_index")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let primary_source = &sources[primary_index];
    let fail = |record: &mut Value, code: &str, message: String| {
        record["failure"] = json!({
            "stage": "ownership",
            "code": code,
            "message": message,
        });
    };
    let Some(source_handle) = primary_source.handle.clone() else {
        fail(record, "OWNERSHIP_INCOMPLETE", format!(
            "no stored source photo backs the sighting of {name}; the ownership record cannot bind a file"
        ));
        return Ok(0.0);
    };
    let source_record = ctx.store.get_record(&source_handle)?;
    let source_image = match ctx.store.read_image(&source_handle) {
        Ok(img) => img,
        Err(e) => {
            fail(record, "OWNERSHIP_INCOMPLETE", format!(
                "the source photo for {name} could not be read back ({e}); the ownership record cannot be issued"
            ));
            return Ok(0.0);
        }
    };
    let Some(rect) = expanded_crop_rect(
        primary.get("bbox_normalized"),
        source_image.width,
        source_image.height,
    ) else {
        fail(record, "OWNERSHIP_INCOMPLETE", format!(
            "no foreground box was recorded for {name}; there is no crop to bind into an ownership record"
        ));
        return Ok(0.0);
    };
    let cropped = crop_image(&source_image, rect);
    let crop_handle = ctx
        .store
        .put_image(&cropped, KIND_CROP, Some(&source_record.asset_id))?;
    let crop_record = ctx.store.get_record(&crop_handle)?;
    // Condition notes ride the crop identify (one billed call).
    let mut cost = 0.0f64;
    let identify_result = (ctx.chain.identify)(&crop_handle)?;
    note_call_id(ctx, &identify_result);
    cost += call_cost(&identify_result);
    // The identify contract is the flattened result (the plan IS
    // the result); accept a nested "plan" too so either chain
    // shape supplies the notes.
    let plan = if identify_result.get("status").and_then(Value::as_str) == Some("ok") {
        Some(
            identify_result
                .get("plan")
                .cloned()
                .unwrap_or_else(|| identify_result.clone()),
        )
    } else {
        None
    };
    let condition_notes = plan
        .as_ref()
        .and_then(|p| p.get("condition_notes"))
        .cloned()
        .unwrap_or(Value::Null);
    let condition_note = if plan.is_some() {
        Value::Null
    } else {
        json!(format!(
            "identify did not return a plan for the crop of {name}; the record \
             carries no condition notes rather than borrowed ones"
        ))
    };
    // The file-level binding: hash + stated capture time of the
    // original file, read at ingest from the seller's own bytes.
    let file_bytes = std::fs::read(&primary_source.path).unwrap_or_default();
    let (file_sha256, exif_capture_time) = if file_bytes.is_empty() {
        (Value::Null, Value::Null)
    } else {
        (
            json!(crate::image::sha256_hex(&file_bytes)),
            crate::ingest::exif_capture_time(&file_bytes)
                .map(|s| json!(s))
                .unwrap_or(Value::Null),
        )
    };
    let mut receipt = json!({
        "receipt_type": "ownership",
        "receipt_schema": 1,
        "item": name,
        "intent": "keep",
        "recorded_at": iso_now_micros(),
        "claim": "this crop comes from exactly this file, captured at the time the file states, unchanged since",
        "source": {
            "asset": source_handle,
            "file_name": primary_source.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            "file_sha256": file_sha256,
            "pixel_sha256": source_record.pixel_sha256,
            "exif_capture_time": exif_capture_time,
            "width": source_image.width,
            "height": source_image.height,
        },
        "crop": {
            "asset": crop_handle,
            "rect_px": [rect.0, rect.1, rect.2, rect.3],
            "pixel_sha256": crop_record.pixel_sha256,
            "sha256": crop_record.sha256,
        },
        "foreground_asset": primary.get("foreground_asset").cloned().unwrap_or(Value::Null),
        "mask_asset": primary.get("mask_asset").cloned().unwrap_or(Value::Null),
        "condition_notes": condition_notes,
        "condition_source": "identify (muse-spark-1.3) over the item crop",
        "condition_note": condition_note,
        "cut_quality": keep_cut_quality(primary),
        "identify_call_id": identify_result.get("call_id").cloned().unwrap_or(Value::Null),
    });
    // Provenance signals (bonus package): read from the same
    // original file bytes the binding above hashes, so the
    // record's observations describe exactly the file its claim
    // pins. A keeper whose file carries location coordinates
    // also gets its metadata-free public copy minted here,
    // parented to the source asset. Additive:
    // verify_ownership_record reads named fields only, and a
    // record minted before this field existed verifies as
    // before.
    if !file_bytes.is_empty() {
        let mut block = crate::provenance::provenance_block(
            &file_bytes,
            Some((source_image.width, source_image.height)),
        );
        crate::provenance::attach_public_copy(
            &mut block,
            &ctx.store,
            &file_bytes,
            Some(&source_record.asset_id),
        );
        receipt["provenance"] = block;
    }
    ctx.store.save_receipt(&crop_handle, &receipt)?;
    record["output_asset"] = json!(crop_handle);
    record["receipt"] = receipt.clone();
    record["receipt_file"] = match ctx.store.receipt_path(&crop_handle) {
        Ok(path) if path.exists() => json!(path.to_string_lossy()),
        _ => Value::Null,
    };
    record["ownership"] = receipt;
    record["stage_reached"] = json!("ownership");
    let verification = verify_ownership_record(&ctx.store, &record["receipt"].clone());
    record["verification"] = verification.clone();
    if verification.get("ok").map(py_truthy).unwrap_or(false) {
        record["status"] = json!("kept");
        record["stage_reached"] = json!("done");
    } else {
        record["status"] = json!("verify_failed");
        let problems = verification
            .get("problems")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|p| p.as_str().unwrap_or_default().to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        record["failure"] = json!({
            "stage": "verify",
            "code": "VERIFY_FAILED",
            "message": format!(
                "the ownership record did not pass independent verification: {}",
                problems.join("; ")
            ),
        });
    }
    Ok(cost)
}

/// The second phase of an item, after its probe: stage it (sell
/// path) or issue its ownership record (keep path). Returns the
/// phase cost and the listing evidence (keep items have none).
fn run_second_phase(
    ctx: &RunContext,
    item: &WorkItem,
    record: &mut Value,
    sources: &[PhotoSource],
    primary: &Value,
) -> Result<(f64, Option<Value>), String> {
    if item.origin == "narration_keep" {
        let cost = keep_from_primary(ctx, record, &item.name, sources, primary)?;
        Ok((cost, None))
    } else {
        let stage = stage_from_primary(ctx, record, &item.name, sources, primary)?;
        Ok((stage.cost, stage.evidence))
    }
}

/// Collect a run's kept items into the ownership inventory: a
/// machine-readable inventory.json plus a human-readable
/// inventory.html sheet (crop, name, condition notes, capture
/// time, receipt reference per item — the sheet a person could
/// hand an insurer), both rendered from the same entries. Writes
/// the files into the room directory and returns the manifest
/// section; Null when the run kept nothing.
fn build_keep_inventory(
    store: &AssetStore,
    out_dir: &Path,
    room_id: &str,
    items: &Value,
) -> Result<Value, String> {
    let esc = |s: &str| -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let mut entries: Vec<Value> = Vec::new();
    let mut rows: Vec<String> = Vec::new();
    for item in items.as_array().cloned().unwrap_or_default() {
        if item.get("intent").and_then(Value::as_str) != Some("keep")
            || item.get("status").and_then(Value::as_str) != Some("kept")
        {
            continue;
        }
        let ownership = item.get("ownership").cloned().unwrap_or(Value::Null);
        let receipt_sha256 =
            crate::image::sha256_hex(serde_json::to_string(&ownership).unwrap_or_default().as_bytes());
        let crop_asset = item
            .get("output_asset")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let entry = json!({
            "name": item.get("name").cloned().unwrap_or(Value::Null),
            "crop_asset": crop_asset,
            "condition_notes": ownership.get("condition_notes").cloned().unwrap_or(Value::Null),
            "recorded_at": ownership.get("recorded_at").cloned().unwrap_or(Value::Null),
            "source_file": ownership.pointer("/source/file_name").cloned().unwrap_or(Value::Null),
            "source_file_sha256": ownership.pointer("/source/file_sha256").cloned().unwrap_or(Value::Null),
            "exif_capture_time": ownership.pointer("/source/exif_capture_time").cloned().unwrap_or(Value::Null),
            "cut_grade": ownership.pointer("/cut_quality/grade").cloned().unwrap_or(Value::Null),
            "location_warning": ownership.pointer("/provenance/location_warning").cloned().unwrap_or(Value::Null),
            "public_copy": ownership.pointer("/provenance/location/public_copy").cloned().unwrap_or(Value::Null),
            "receipt_sha256": receipt_sha256,
            "receipt_file": item.get("receipt_file").cloned().unwrap_or(Value::Null),
        });
        entries.push(entry.clone());
        let img_tag = match store.read_bytes(&crop_asset) {
            Ok(bytes) => format!(
                "<img alt=\"crop\" style=\"max-width:240px\" src=\"data:image/png;base64,{}\">",
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes)
            ),
            Err(_) => "<em>crop image unavailable</em>".to_string(),
        };
        let text_of = |v: &Value| -> String {
            v.as_str().map(|s| s.to_string()).unwrap_or_else(|| "—".to_string())
        };
        rows.push(format!(
            "<tr><td>{}</td><td><strong>{}</strong></td><td>{}</td><td>{}</td><td class=\"mono\">{}</td></tr>",
            img_tag,
            esc(&text_of(&entry["name"])),
            esc(&text_of(&entry["condition_notes"])),
            esc(&text_of(&entry["exif_capture_time"])),
            esc(&receipt_sha256),
        ));
        if let Some(warning) = entry["location_warning"].as_str() {
            rows.push(format!(
                "<tr><td></td><td colspan=\"4\"><strong>Location warning:</strong> {}</td></tr>",
                esc(warning),
            ));
        }
    }
    if entries.is_empty() {
        return Ok(Value::Null);
    }
    let inventory = json!({
        "room_id": room_id,
        "generated_at": iso_now_micros(),
        "items": entries,
    });
    std::fs::write(
        out_dir.join("inventory.json"),
        serde_json::to_string_pretty(&inventory).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let sheet = format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <title>Ownership inventory — room {}</title>\n<style>\n\
         body {{ font-family: system-ui, sans-serif; color: #111; max-width: 60rem; margin: 2rem auto; padding: 0 1rem; }}\n\
         table {{ border-collapse: collapse; width: 100%; }} td {{ border-top: 1px solid #ddd; padding: 0.5rem; vertical-align: top; }}\n\
         .mono {{ font-family: ui-monospace, Menlo, monospace; font-size: 0.8em; word-break: break-all; }}\n</style>\n</head>\n<body>\n\
         <h1>Ownership inventory</h1>\n\
         <p>Items the owner marked to keep, recorded during room run <span class=\"mono\">{}</span>. \
         Each crop is bound by its receipt to the exact source photo file it came from, unchanged since ingest.</p>\n\
         <table>\n<tr><td><strong>Crop</strong></td><td><strong>Item</strong></td><td><strong>Condition notes</strong></td><td><strong>Captured (per file)</strong></td><td><strong>Receipt SHA-256</strong></td></tr>\n{}\n</table>\n</body>\n</html>\n",
        esc(room_id),
        esc(room_id),
        rows.join("\n")
    );
    std::fs::write(out_dir.join("inventory.html"), sheet).map_err(|e| e.to_string())?;
    Ok(json!({
        "json_file": "inventory.json",
        "sheet_file": "inventory.html",
        "items": entries.len(),
    }))
}

/// Parse a step row's JSON result_ref (Null when absent/corrupt —
/// a corrupt ref re-runs the step rather than trusting it).
fn step_ref(row: &crate::sqlledger::StepRow) -> Value {
    row.result_ref
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(Value::Null)
}

/// One item end to end, step-logged (P1, tracker event 372).
///
/// Resume semantics, by what a previous attempt left behind:
/// - `draw` complete → replay the record + evidence verbatim; no
///   billed call runs for this item.
/// - `cut_out` failed → replay the failure record; the item is
///   done (its failure, with correction text, is in the record).
/// - `cut_out` complete → resume at the stage: stage from the
///   recorded primary sighting; only the draw step runs live.
/// - `cut_out` pending with a checkpoint → the checkpointed
///   sightings (each written only after its probe returned, so
///   each is fully billed) seed the probe phase; only unprobed
///   photos run live. This is the reconciliation rule for the
///   executed-but-unrecorded window: a sighting in the checkpoint
///   is never re-billed, and the room ledger holds its settled
///   receipts under the recorded call ids.
/// - A `draw` step left pending by a crash re-runs the stage:
///   draw is a single composed call whose outputs cannot be
///   attributed before it returns, so its executed-but-unrecorded
///   window is one draw call wide. That is the documented residue
///   of the window; every other step closes it.
#[allow(clippy::too_many_arguments)]
fn process_item_logged(
    ctx: &RunContext,
    item: &WorkItem,
    sources: &[PhotoSource],
    probe_all_candidates: bool,
    log: &crate::sqlledger::RunStepLog,
    run_id: &str,
    item_index: i64,
) -> Result<(Value, Option<Value>), String> {
    // A keep item's second step is its ownership record, not a
    // draw: same pending-before / complete-after discipline, its
    // own step name so a resume never confuses the two.
    let second_step = if item.origin == "narration_keep" {
        "ownership"
    } else {
        "draw"
    };
    let draw_row = log.get(run_id, item_index, second_step)?;
    let cut_row = log.get(run_id, item_index, "cut_out")?;
    if let Some(row) = &draw_row {
        if row.status == "complete" {
            let refv = step_ref(row);
            let mut record = refv["record"].clone();
            record["replayed"] = json!(true);
            // Evidence is an object when present; a stored null
            // is the None case (stage evidence is never a bare
            // null value when it exists).
            let evidence = if refv["evidence"].is_null() {
                None
            } else {
                Some(refv["evidence"].clone())
            };
            return Ok((record, evidence));
        }
    }
    if let Some(row) = &cut_row {
        if row.status == "failed" {
            let refv = step_ref(row);
            let mut record = refv["record"].clone();
            record["replayed"] = json!(true);
            return Ok((record, None));
        }
        if row.status == "complete" {
            let refv = step_ref(row);
            let mut record = refv["record"].clone();
            let primary = refv["primary_sighting"].clone();
            let probe_cost = refv["cost_usd"].as_f64().unwrap_or(0.0);
            log.mark_pending(run_id, item_index, second_step, None)?;
            // An Err here (infrastructure, not a typed failure)
            // leaves the step pending — the documented one-call
            // orphan window — and propagates, as an unlogged run
            // would.
            let (phase_cost, evidence) =
                run_second_phase(ctx, item, &mut record, sources, &primary)?;
            record["cost_usd"] = json!(probe_cost + phase_cost);
            let call_id = take_last_call_id(ctx);
            log.mark_complete(
                run_id,
                item_index,
                second_step,
                call_id.as_deref(),
                &json!({"record": record, "evidence": evidence}).to_string(),
            )?;
            return Ok((record, evidence));
        }
    }
    // Fresh attempt, or a crash mid-probe: seed from the
    // checkpoint the crashed attempt left, if any.
    let prior: Vec<Value> = match &cut_row {
        Some(row) if row.status == "pending" => step_ref(row)
            .get("sightings")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    log.mark_pending(run_id, item_index, "cut_out", None)?;
    let outcome = {
        let mut checkpoint = |sightings: &[Value], cost: f64| {
            let call_id = take_last_call_id(ctx);
            let _ = log.update_checkpoint(
                run_id,
                item_index,
                "cut_out",
                call_id.as_deref(),
                &json!({"sightings": sightings, "cost_usd": cost}).to_string(),
            );
        };
        probe_item(
            ctx,
            item,
            sources,
            probe_all_candidates,
            &prior,
            Some(&mut checkpoint),
        )?
    };
    let Some(primary) = outcome.primary else {
        log.mark_failed(
            run_id,
            item_index,
            "cut_out",
            &json!({"record": outcome.record}).to_string(),
        )?;
        return Ok((outcome.record, None));
    };
    let probe_call_id = take_last_call_id(ctx);
    log.mark_complete(
        run_id,
        item_index,
        "cut_out",
        probe_call_id.as_deref(),
        &json!({
            "record": outcome.record.clone(),
            "primary_sighting": primary,
            "cost_usd": outcome.cost,
        })
        .to_string(),
    )?;
    log.mark_pending(run_id, item_index, second_step, None)?;
    let mut record = outcome.record;
    let (phase_cost, evidence) =
        run_second_phase(ctx, item, &mut record, sources, &primary)?;
    record["cost_usd"] = json!(outcome.cost + phase_cost);
    let call_id = take_last_call_id(ctx);
    log.mark_complete(
        run_id,
        item_index,
        second_step,
        call_id.as_deref(),
        &json!({"record": record, "evidence": evidence}).to_string(),
    )?;
    Ok((record, evidence))
}

/// What one staging pass spent, and the plan that supplied the
/// item's listing copy (the photo plan for a photo_plan listing,
/// the crop-identify plan for an item_crop listing, None when the
/// listing is empty). The collision post-pass reads that plan as
/// the item's own listing evidence.
struct StageOutcome {
    cost: f64,
    evidence: Option<Value>,
}

/// Stage one item from one chosen sighting: listing copy from that
/// sighting alone, then draw in the run's stage mode, the Fidelity
/// Lock receipt, and the independent verification. Fills the
/// record's listing, output, receipt and verification fields and
/// its staged / verify_failed / draw-failure outcome; returns what
/// the pass cost. Used for the primary sighting here and for a
/// re-selected sighting in the collision post-pass, so both paths
/// stage identically.
fn stage_from_primary(
    ctx: &RunContext,
    record: &mut Value,
    name: &str,
    sources: &[PhotoSource],
    primary: &Value,
) -> Result<StageOutcome, String> {
    let mut cost = 0.0f64;
    let evidence: Option<Value>;
    // Fresh-record staging state: a re-stage starts from the same
    // not-staged-yet status process_item's new record carries, so
    // a draw or verify failure below can never inherit "staged".
    record["status"] = json!("failed");
    record["failure"] = Value::Null;
    // Listing copy comes only from this item's own chosen
    // sighting. First route: that photo's own plan, when the plan
    // actually describes THIS item. Second route: crop the source
    // image to the item's foreground and identify the crop. If
    // neither yields copy, the listing stays empty with the reason
    // recorded — inventing or borrowing copy is exactly the
    // failure mode the whole product exists to refuse.
    let primary_index = primary
        .get("photo_index")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let primary_source = &sources[primary_index];
    if plan_matches_item(primary_source.plan.as_ref(), name) {
        let plan = primary_source.plan.as_ref().expect("plan matched");
        let bullets = plan
            .get("listing_bullets")
            .filter(|b| py_truthy(b))
            .cloned()
            .unwrap_or_else(|| json!([]));
        record["listing"] = json!({
            "title": plan.get("suggested_title").cloned().unwrap_or(Value::Null),
            "bullets": bullets,
            "condition_notes": plan.get("condition_notes").cloned().unwrap_or(Value::Null),
            "source": "photo_plan",
            "note": Value::Null,
            "crop_asset": Value::Null,
        });
        evidence = Some(plan.clone());
    } else {
        let (listing, crop_cost, plan) = crop_listing(ctx, primary, primary_source);
        cost += crop_cost;
        evidence = plan;
        record["listing"] = listing;
    }
    // Draw stage: the receipt is the product; the picture is evidence.
    record["stage_reached"] = json!("draw");
    let fg_handle = primary
        .get("foreground_asset")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let draw_result = if backdrop::is_backdrop_mode(&ctx.stage_mode) {
        backdrop_draw(ctx, &fg_handle)
    } else {
        (ctx.chain.draw)(&fg_handle, &ctx.scene)?
    };
    note_call_id(ctx, &draw_result);
    cost += call_cost(&draw_result);
    if draw_result.get("status").and_then(Value::as_str) != Some("ok") {
        // Python dict.get semantics: the error dict (or {} when the
        // result carries none), code defaulting to DRAW_FAILED and
        // message to "draw failed" only when the KEY is absent.
        let error = draw_result
            .get("error")
            .filter(|e| py_truthy(e))
            .cloned()
            .unwrap_or_else(|| json!({}));
        record["failure"] = json!({
            "stage": "draw",
            "code": error.get("code").cloned().unwrap_or(json!("DRAW_FAILED")),
            "message": error.get("message").cloned().unwrap_or(json!("draw failed")),
        });
        return Ok(StageOutcome { cost, evidence });
    }
    let receipt_src = draw_result
        .get("fidelity_receipt")
        .filter(|r| py_truthy(r))
        .cloned()
        .unwrap_or_else(|| json!({}));
    record["output_asset"] = draw_result
        .get("output_asset")
        .cloned()
        .unwrap_or(Value::Null);
    record["receipt"] = json!({
        "source_asset": receipt_src.get("source_asset").cloned().unwrap_or(Value::Null),
        "foreground_pixel_sha256": receipt_src
            .get("foreground_pixel_sha256")
            .cloned()
            .unwrap_or(Value::Null),
        "opaque_pixels": receipt_src.get("opaque_pixels").cloned().unwrap_or(Value::Null),
        "changed_opaque_pixels": receipt_src
            .get("changed_opaque_pixels")
            .cloned()
            .unwrap_or(Value::Null),
        "color_profile_preserved": receipt_src
            .get("color_profile_preserved")
            .cloned()
            .unwrap_or(Value::Null),
        "plate_kind": receipt_src
            .get("plate_kind")
            .cloned()
            .unwrap_or(Value::Null),
        "backdrop": receipt_src.get("backdrop").cloned().unwrap_or(Value::Null),
        "cost_usd": draw_result.get("cost_usd").cloned().unwrap_or(Value::Null),
    });
    let output_handle = record["output_asset"].as_str().unwrap_or("").to_string();
    record["receipt_file"] = match ctx.store.receipt_path(&output_handle) {
        Ok(path) if path.exists() => json!(path.to_string_lossy()),
        _ => Value::Null,
    };
    // Verify stage: an independent pass over the bytes on disk. A
    // staged item the verifier cannot re-prove is reported as
    // verify_failed — visible, priced, and never quietly "staged".
    record["stage_reached"] = json!("verify");
    let verification = run_verification(ctx, &output_handle);
    record["verification"] = verification.clone();
    if verification.get("ok").map(py_truthy).unwrap_or(false) {
        record["status"] = json!("staged");
        record["stage_reached"] = json!("done");
    } else {
        record["status"] = json!("verify_failed");
        let problems = verification
            .get("problems")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|p| p.as_str().unwrap_or_default().to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let detail = if problems.is_empty() {
            "verifier reported failure".to_string()
        } else {
            problems.join("; ")
        };
        record["failure"] = json!({
            "stage": "verify",
            "code": "VERIFY_FAILED",
            "message": format!(
                "the staged output did not pass independent verification: {detail}"
            ),
        });
    }
    Ok(StageOutcome { cost, evidence })
}

/// The successful sighting a record staged from, if any.
fn primary_sighting_of(record: &Value) -> Option<Value> {
    let primary_index = record.get("primary_photo_index").and_then(Value::as_u64)?;
    record
        .get("sightings")
        .and_then(Value::as_array)?
        .iter()
        .find(|s| {
            s.get("photo_index").and_then(Value::as_u64) == Some(primary_index)
                && s.get("status").and_then(Value::as_str) == Some("ok")
        })
        .cloned()
}

/// A record's successful sightings, in record order.
fn ok_sightings_of(record: &Value) -> Vec<Value> {
    record
        .get("sightings")
        .and_then(Value::as_array)
        .map(|sightings| {
            sightings
                .iter()
                .filter(|s| s.get("status").and_then(Value::as_str) == Some("ok"))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn sighting_photo(sighting: &Value) -> Option<u64> {
    sighting.get("photo_index").and_then(Value::as_u64)
}

/// Whether two sightings are the same object instance: same photo,
/// and their foreground masks overlap at least
/// COLLISION_MASK_OVERLAP of the smaller mask. An unreadable or
/// unmeasurable pair does not prove a collision.
fn sightings_collide(store: &AssetStore, a: &Value, b: &Value) -> bool {
    let (Some(photo_a), Some(photo_b)) = (sighting_photo(a), sighting_photo(b)) else {
        return false;
    };
    if photo_a != photo_b {
        return false;
    }
    matches!(
        mask_overlap_ratio(
            store,
            a.get("foreground_asset").and_then(Value::as_str),
            b.get("foreground_asset").and_then(Value::as_str),
        ),
        Some(ratio) if ratio >= COLLISION_MASK_OVERLAP
    )
}

/// Overlap of two foreground masks as a fraction of the smaller
/// mask, computed from the stored foreground assets' alpha planes.
/// None when either asset is unreadable, the frames differ in size
/// (foregrounds from one photo share the photo's dimensions), or a
/// mask has no opaque pixels.
pub fn mask_overlap_ratio(
    store: &AssetStore,
    fg_a: Option<&str>,
    fg_b: Option<&str>,
) -> Option<f64> {
    let a = store.read_image(fg_a?).ok()?;
    let b = store.read_image(fg_b?).ok()?;
    if a.width != b.width || a.height != b.height {
        return None;
    }
    let (mut overlap, mut count_a, mut count_b) = (0u64, 0u64, 0u64);
    for (alpha_a, alpha_b) in a.alpha_plane().iter().zip(b.alpha_plane().iter()) {
        if *alpha_a > 0 {
            count_a += 1;
            if *alpha_b > 0 {
                overlap += 1;
            }
        }
        if *alpha_b > 0 {
            count_b += 1;
        }
    }
    let smaller = count_a.min(count_b);
    if smaller == 0 {
        return None;
    }
    Some(overlap as f64 / smaller as f64)
}

fn record_name(record: &Value) -> String {
    record
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn push_flag(record: &mut Value, flag: String) {
    if let Some(flags) = record.get_mut("flags").and_then(Value::as_array_mut) {
        if !flags.iter().any(|f| f.as_str() == Some(flag.as_str())) {
            flags.push(json!(flag));
        }
    }
}

/// Link two records as possible duplicates of each other (the
/// de-dup record's shape: a name in each record's
/// possible_duplicate_of, never a merge).
fn link_possible_duplicates(records: &mut [Value], a: usize, b: usize) {
    let name_a = record_name(&records[a]);
    let name_b = record_name(&records[b]);
    for (idx, other) in [(a, name_b), (b, name_a)] {
        let current = records[idx]
            .get("possible_duplicate_of")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !current.contains(&other) {
            let mut next = current;
            next.push(other);
            records[idx]["possible_duplicate_of"] = json!(next);
        }
    }
}

/// Cross-item duplicate flags. Same foreground hash on two items, or
/// near-identical masks on the same photo, means the room probably
/// double-counted one physical object — flagged for a human, NEVER
/// silently merged: the seller named two things, and deleting one on
/// a heuristic is how listings lose items.
fn flag_cross_item_duplicates(records: &mut [Value]) {
    let staged: Vec<usize> = records
        .iter()
        .enumerate()
        .filter(|(_, r)| r.get("status").and_then(Value::as_str) == Some("staged"))
        .map(|(i, _)| i)
        .collect();
    // Identical primary foreground hashes.
    let mut by_hash: Vec<(String, Vec<usize>)> = Vec::new();
    for &i in &staged {
        let digest = records[i]
            .pointer("/receipt/foreground_pixel_sha256")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if digest.is_empty() {
            continue;
        }
        if let Some((_, group)) = by_hash.iter_mut().find(|(k, _)| *k == digest) {
            group.push(i);
        } else {
            by_hash.push((digest, vec![i]));
        }
    }
    for (_, group) in &by_hash {
        if group.len() <= 1 {
            continue;
        }
        let names: Vec<String> = group
            .iter()
            .map(|&i| {
                records[i]
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            })
            .collect();
        for (pos, &i) in group.iter().enumerate() {
            let others: Vec<&String> = names
                .iter()
                .enumerate()
                .filter(|(p, _)| *p != pos)
                .map(|(_, n)| n)
                .collect();
            let mut merged: Vec<String> = records[i]
                .get("possible_duplicate_of")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            for other in &others {
                if !merged.contains(other) {
                    merged.push(other.to_string());
                }
            }
            merged.sort();
            records[i]["possible_duplicate_of"] = json!(merged);
            let joined = others
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            if let Some(flags) = records[i].get_mut("flags").and_then(Value::as_array_mut) {
                flags.push(json!(format!(
                    "primary foreground pixel hash is identical to {joined}; \
                     listings kept separate pending a human call"
                )));
            }
        }
    }
    // Near-identical primary masks on the same photo.
    let mut primaries: Vec<(String, usize, Value)> = Vec::new(); // (name, record idx, primary sighting)
    for &i in &staged {
        let name = records[i]
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let primary_index = records[i]
            .get("primary_photo_index")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let primary = records[i]
            .get("sightings")
            .and_then(Value::as_array)
            .and_then(|sightings| {
                sightings.iter().find(|s| {
                    s.get("photo_index").and_then(Value::as_u64) == Some(primary_index as u64)
                        && s.get("status").and_then(Value::as_str) == Some("ok")
                })
            })
            .cloned()
            .unwrap_or(Value::Null);
        if let Some(slot) = primaries.iter_mut().find(|(n, _, _)| *n == name) {
            slot.1 = i;
            slot.2 = primary;
        } else {
            primaries.push((name, i, primary));
        }
    }
    for a in 0..primaries.len() {
        for b in (a + 1)..primaries.len() {
            let (name_a, rec_a, sighting_a) = &primaries[a];
            let (name_b, rec_b, sighting_b) = &primaries[b];
            let photo_a = sighting_a.get("photo_index").and_then(Value::as_u64);
            let photo_b = sighting_b.get("photo_index").and_then(Value::as_u64);
            if photo_a.is_none() || photo_a != photo_b {
                continue;
            }
            let iou = bbox_iou(
                sighting_a.get("bbox_normalized"),
                sighting_b.get("bbox_normalized"),
            );
            if iou < MASK_IOU_FLAG {
                continue;
            }
            let photo_index = photo_a.unwrap_or(0);
            for (rec, other) in [(*rec_a, name_b.clone()), (*rec_b, name_a.clone())] {
                let current = records[rec]
                    .get("possible_duplicate_of")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if !current.contains(&other) {
                    let mut next = current.clone();
                    next.push(other.clone());
                    records[rec]["possible_duplicate_of"] = json!(next);
                }
                if let Some(flags) = records[rec].get_mut("flags").and_then(Value::as_array_mut) {
                    flags.push(json!(format!(
                        "primary masks on photo {photo_index} overlap {} \
                         with IoU {iou:.2}; kept separate",
                        py_repr_str(&other)
                    )));
                }
            }
        }
    }
    // One item's primary against the OTHER item's non-primary
    // sightings on the same photo. The primary-only comparison
    // above misses the case where the duplicate's own primary sits
    // on a different photo while one of its sightings is the very
    // mask the other item staged (the acceptance run's two hats:
    // the straw hat's primary was a still, its frame sighting was
    // the black hat item's primary). The reference build flags the
    // pair whenever the two names segment one object; this is the
    // same evidence read across sightings.
    for &a in &staged {
        let Some(primary_a) = primary_sighting_of(&records[a]) else {
            continue;
        };
        let photo_a = primary_a
            .get("photo_index")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        for &b in &staged {
            if a == b {
                continue;
            }
            let primary_b_index = records[b]
                .get("primary_photo_index")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let sightings_b = records[b]
                .get("sightings")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for sighting in &sightings_b {
                if sighting.get("status").and_then(Value::as_str) != Some("ok") {
                    continue;
                }
                let photo = sighting
                    .get("photo_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if photo != photo_a || photo == primary_b_index {
                    continue;
                }
                let iou = bbox_iou(
                    sighting.get("bbox_normalized"),
                    primary_a.get("bbox_normalized"),
                );
                if iou < MASK_IOU_FLAG {
                    continue;
                }
                let name_a = record_name(&records[a]);
                let name_b = record_name(&records[b]);
                link_possible_duplicates(records, a, b);
                push_flag(
                    &mut records[a],
                    format!(
                        "a sighting of {} on photo {photo} overlaps this item's \
                         primary mask with IoU {iou:.2}; kept separate",
                        py_repr_str(&name_b)
                    ),
                );
                push_flag(
                    &mut records[b],
                    format!(
                        "this item's sighting on photo {photo} overlaps {}'s \
                         primary mask with IoU {iou:.2}; kept separate",
                        py_repr_str(&name_a)
                    ),
                );
                break;
            }
        }
    }
}

/// Whether an item's listing evidence (the plan that supplied its
/// listing copy) is about the given item name, judged by the same
/// plan_matches_item machinery the listing routes use.
fn evidence_matches(evidence: &Option<Value>, item_name: &str) -> bool {
    evidence
        .as_ref()
        .map(|plan| plan_matches_item(Some(plan), item_name))
        .unwrap_or(false)
}

/// Projected cost of one re-stage (listing identify plus draw),
/// with the run gate's own projection conventions.
fn projected_restage_cost(
    ctx: &RunContext,
    sources: &[PhotoSource],
    name: &str,
    sighting: &Value,
) -> f64 {
    let photo_index = sighting
        .get("photo_index")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let source = &sources[photo_index];
    let listing_cost = if plan_matches_item(source.plan.as_ref(), name) {
        0.0
    } else {
        project_identify_cost_usd(IDENTIFY_MODEL, source.width, source.height).unwrap_or(0.0)
            * IDENTIFY_MAX_ATTEMPTS as f64
    };
    let draw_cost = if backdrop::is_backdrop_mode(&ctx.stage_mode) {
        0.0
    } else {
        project_draw_cost_usd(DRAW_BACKGROUND_MODEL).unwrap_or(0.0)
    };
    listing_cost + draw_cost
}

/// The displaced item's re-selection candidate: its largest
/// successful non-primary sighting, in primary-selection order
/// (most opaque pixels; ties to a still over a video frame, then
/// the lower photo index) whose foreground does not collide with
/// ANY of the keeper's successful sightings. A collision needs the
/// same photo, so a candidate on a photo where the keeper has no
/// sighting cannot collide; one sharing a photo with a keeper
/// sighting must not overlap that sighting's mask at the collision
/// bar.
fn choose_alternate_sighting(
    store: &AssetStore,
    record: &Value,
    sources: &[PhotoSource],
    old_primary_photo: u64,
    keeper_sightings: &[Value],
) -> Option<Value> {
    let mut alternates: Vec<Value> = ok_sightings_of(record)
        .into_iter()
        .filter(|s| sighting_photo(s) != Some(old_primary_photo))
        .collect();
    alternates.sort_by(|s1, s2| {
        let key = |s: &Value| {
            let idx = s.get("photo_index").and_then(Value::as_u64).unwrap_or(0) as usize;
            (
                s.get("opaque_pixels").and_then(Value::as_i64).unwrap_or(0),
                sources[idx].origin == "still",
            )
        };
        key(s2).cmp(&key(s1)).then_with(|| {
            s1.get("photo_index")
                .and_then(Value::as_u64)
                .cmp(&s2.get("photo_index").and_then(Value::as_u64))
        })
    });
    alternates.into_iter().find(|sighting| {
        !keeper_sightings
            .iter()
            .any(|keeper_sighting| sightings_collide(store, sighting, keeper_sighting))
    })
}

/// A re-selected primary that collides with a third staged item is
/// flagged as a possible duplicate of that item, never iterated:
/// the post-pass runs once.
fn flag_third_item_collisions(
    store: &AssetStore,
    records: &mut [Value],
    staged: &[usize],
    displaced: usize,
    keeper: usize,
    new_photo: u64,
) {
    let new_primary = primary_sighting_of(&records[displaced]);
    for &other in staged {
        if other == displaced || other == keeper {
            continue;
        }
        let Some(new_sight) = new_primary.clone() else {
            continue;
        };
        // Sighting level, like the contest detection itself: the
        // new primary against every successful sighting of the
        // third item, not only its primary.
        let collides = ok_sightings_of(&records[other])
            .iter()
            .any(|other_sight| sightings_collide(store, &new_sight, other_sight));
        if collides {
            let other_name = record_name(&records[other]);
            link_possible_duplicates(records, displaced, other);
            push_flag(
                &mut records[displaced],
                format!(
                    "re-selected primary on photo {new_photo} also \
                     overlaps {}; flagged as a possible duplicate, \
                     not re-selected again",
                    py_repr_str(&other_name)
                ),
            );
        }
    }
}

/// Collision re-selection post-pass (run level, after de-dup).
///
/// Collision detection is sighting-level: for every pair of STAGED
/// items, every pair of successful sightings on the same photo
/// whose masks overlap at least COLLISION_MASK_OVERLAP of the
/// smaller mask is the same physical object, and any such pair
/// links the two items as possible duplicates. A CONTEST exists
/// over that object when at least one side's PRIMARY is one of the
/// colliding sightings. (Two non-primary sightings colliding is
/// probing ambiguity: flagged, never contested.)
///
/// When exactly one item's listing evidence matches its own name
/// and not the other's, that item is the keeper. The displaced
/// item is the one whose primary is contested and which is not
/// the keeper: it re-stages from its largest successful sighting
/// that does not collide with any of the keeper's sightings, with
/// a fresh receipt, independent verification and recomputed
/// listing. The keeper is never moved, including when its own
/// primary is not the contested object. When the evidence is
/// ambiguous (neither side or both qualify as keeper), or the
/// contested primary belongs to the keeper alone, or no clean
/// alternate exists, or the re-stage would cross the run budget,
/// the primaries stand and the flags say why. A failed re-stage
/// reverts to the original staged result. The duplicate flags are
/// not removed by a successful re-selection: they describe the
/// probing ambiguity, and the re-selection flag and note record
/// the resolution. The pass runs once: contests are read from the
/// sightings as staged, re-selected primaries are not
/// re-contested, and a re-selected primary that collides with a
/// third item is flagged as a possible duplicate, not iterated.
/// Returns the run-level notes the pass recorded.
#[allow(clippy::too_many_arguments)]
fn resolve_primary_collisions(
    ctx: &RunContext,
    records: &mut [Value],
    sources: &[PhotoSource],
    evidence: &mut [Option<Value>],
    spend_probe: &SpendProbe,
    spend_before: f64,
    budget_usd: f64,
) -> Result<Vec<String>, String> {
    let mut notes: Vec<String> = Vec::new();
    let staged: Vec<usize> = records
        .iter()
        .enumerate()
        .filter(|(_, r)| r.get("status").and_then(Value::as_str) == Some("staged"))
        .map(|(i, _)| i)
        .collect();
    // Sighting-level collisions and contests, all read from the
    // sightings as staged (before any re-selection below).
    struct Contest {
        a: usize,
        b: usize,
        a_primary_contested: bool,
        b_primary_contested: bool,
    }
    let mut contests: Vec<Contest> = Vec::new();
    let mut colliding_pairs: Vec<(usize, usize)> = Vec::new();
    for x in 0..staged.len() {
        for y in (x + 1)..staged.len() {
            let (a, b) = (staged[x], staged[y]);
            let primary_a = records[a]
                .get("primary_photo_index")
                .and_then(Value::as_u64);
            let primary_b = records[b]
                .get("primary_photo_index")
                .and_then(Value::as_u64);
            let mut any_collision = false;
            let (mut a_contested, mut b_contested) = (false, false);
            for sight_a in ok_sightings_of(&records[a]) {
                for sight_b in ok_sightings_of(&records[b]) {
                    if !sightings_collide(&ctx.store, &sight_a, &sight_b) {
                        continue;
                    }
                    any_collision = true;
                    if sighting_photo(&sight_a) == primary_a {
                        a_contested = true;
                    }
                    if sighting_photo(&sight_b) == primary_b {
                        b_contested = true;
                    }
                }
            }
            if any_collision {
                colliding_pairs.push((a, b));
            }
            if a_contested || b_contested {
                contests.push(Contest {
                    a,
                    b,
                    a_primary_contested: a_contested,
                    b_primary_contested: b_contested,
                });
            }
        }
    }
    // Any colliding pair is de-dup evidence in its own right,
    // whatever the contest judgments below decide, and a later
    // re-selection does not remove it.
    for (a, b) in colliding_pairs {
        link_possible_duplicates(records, a, b);
    }
    let mut displaced_attempted: HashSet<usize> = HashSet::new();
    let mut reselected: HashSet<usize> = HashSet::new();
    for contest in contests {
        let (a, b) = (contest.a, contest.b);
        if reselected.contains(&a) || reselected.contains(&b) {
            continue; // a re-selected primary is not re-contested
        }
        let name_a = record_name(&records[a]);
        let name_b = record_name(&records[b]);
        let a_keeper =
            evidence_matches(&evidence[a], &name_a) && !evidence_matches(&evidence[a], &name_b);
        let b_keeper =
            evidence_matches(&evidence[b], &name_b) && !evidence_matches(&evidence[b], &name_a);
        let keeper = match (a_keeper, b_keeper) {
            (true, false) => a,
            (false, true) => b,
            _ => continue, // neither or both: flags stand, no re-selection
        };
        // The displaced item is the non-keeper whose primary is
        // contested. When only the keeper's primary is contested,
        // the keeper keeps it and the other item's primary is a
        // different object, so nothing moves.
        let primary_contested = |idx: usize| {
            if idx == a {
                contest.a_primary_contested
            } else {
                contest.b_primary_contested
            }
        };
        let displaced = [a, b]
            .into_iter()
            .find(|&idx| idx != keeper && primary_contested(idx));
        let Some(displaced) = displaced else {
            continue;
        };
        if displaced_attempted.contains(&displaced) {
            continue;
        }
        displaced_attempted.insert(displaced);
        let keeper_name = record_name(&records[keeper]);
        let displaced_name = record_name(&records[displaced]);
        let keeper_sightings = ok_sightings_of(&records[keeper]);
        let old_primary_photo = records[displaced]
            .get("primary_photo_index")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let chosen = choose_alternate_sighting(
            &ctx.store,
            &records[displaced],
            sources,
            old_primary_photo,
            &keeper_sightings,
        );
        let Some(chosen) = chosen else {
            push_flag(
                &mut records[displaced],
                format!(
                    "primary on photo {old_primary_photo} is the same object as a \
                     sighting of {}, but no non-colliding alternate sighting exists; \
                     primary kept",
                    py_repr_str(&keeper_name)
                ),
            );
            notes.push(format!(
                "item {}: primary collides with {} and no non-colliding alternate \
                 sighting exists; primary kept",
                py_repr_str(&displaced_name),
                py_repr_str(&keeper_name)
            ));
            continue;
        };
        let new_photo = chosen
            .get("photo_index")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        // The re-stage spends real money; it counts into the item
        // and the run budget, and is skipped when it would cross.
        let projected = projected_restage_cost(ctx, sources, &displaced_name, &chosen);
        if (spend_probe()? - spend_before) + projected > budget_usd {
            push_flag(
                &mut records[displaced],
                format!(
                    "primary re-selection after the collision with {} was skipped: \
                     the projected re-stage cost would exceed the run budget; \
                     primary kept",
                    py_repr_str(&keeper_name)
                ),
            );
            notes.push(format!(
                "item {}: primary re-selection after the collision with {} skipped \
                 (projected re-stage cost would exceed the ${budget_usd:.2} run budget)",
                py_repr_str(&displaced_name),
                py_repr_str(&keeper_name)
            ));
            continue;
        }
        // Re-stage from the alternate sighting. A failure reverts
        // to the original staged result; its spend still counts.
        let snapshot = records[displaced].clone();
        let old_cost = snapshot
            .get("cost_usd")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        records[displaced]["primary_photo_index"] = json!(new_photo);
        records[displaced]["also_seen_in"] = json!(snapshot
            .get("sightings")
            .and_then(Value::as_array)
            .map(|sightings| {
                sightings
                    .iter()
                    .filter(|s| {
                        s.get("status").and_then(Value::as_str) == Some("ok")
                            && s.get("photo_index").and_then(Value::as_u64) != Some(new_photo)
                    })
                    .map(|s| s.get("photo_index").cloned().unwrap_or(Value::Null))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default());
        let attempt = stage_from_primary(
            ctx,
            &mut records[displaced],
            &displaced_name,
            sources,
            &chosen,
        );
        match attempt {
            Ok(outcome) if records[displaced]["status"] == json!("staged") => {
                records[displaced]["cost_usd"] = json!(old_cost + outcome.cost);
                evidence[displaced] = outcome.evidence;
                reselected.insert(displaced);
                push_flag(
                    &mut records[displaced],
                    "primary_reselected_after_duplicate_collision".to_string(),
                );
                notes.push(format!(
                    "item {}: primary re-selected from photo {old_primary_photo} to \
                     photo {new_photo} after a duplicate collision with {}",
                    py_repr_str(&displaced_name),
                    py_repr_str(&keeper_name)
                ));
                flag_third_item_collisions(
                    &ctx.store, records, &staged, displaced, keeper, new_photo,
                );
            }
            attempt => {
                let detail = match &attempt {
                    Ok(_) => records[displaced]
                        .pointer("/failure/message")
                        .and_then(Value::as_str)
                        .unwrap_or("the re-stage did not verify")
                        .to_string(),
                    Err(exc) => exc.clone(),
                };
                let attempt_cost = match &attempt {
                    Ok(outcome) => outcome.cost,
                    Err(_) => 0.0,
                };
                records[displaced] = snapshot;
                records[displaced]["cost_usd"] = json!(old_cost + attempt_cost);
                push_flag(
                    &mut records[displaced],
                    format!(
                        "primary re-selection after the collision with {} failed \
                         ({detail}); the original staged result was kept",
                        py_repr_str(&keeper_name)
                    ),
                );
                notes.push(format!(
                    "item {}: primary re-selection after the collision with {} \
                     failed ({detail}); the original staged result was kept",
                    py_repr_str(&displaced_name),
                    py_repr_str(&keeper_name)
                ));
            }
        }
    }
    Ok(notes)
}

pub struct Candidate {
    pub name: String,
    pub photo_index: usize,
    pub plan: Value,
}

pub struct CandidateGroup {
    pub name: String,
    pub photo_indices: Vec<usize>,
    pub flags: Vec<String>,
}

/// Group auto-mode candidates into items. A plan concept joins the
/// first group whose name it is similar enough to (>= 0.6, the same
/// bar as plan_matches_item); the [0.4, 0.6) grey zone keeps the
/// groups separate and flags both sides as possible duplicates.
pub fn group_candidates(candidates: &[Candidate]) -> Vec<CandidateGroup> {
    let mut groups: Vec<CandidateGroup> = Vec::new();
    for candidate in candidates {
        let mut joined: Option<usize> = None;
        let mut grey: Vec<usize> = Vec::new();
        for (gi, group) in groups.iter().enumerate() {
            let sim = token_similarity(Some(candidate.name.as_str()), Some(group.name.as_str()));
            if sim >= MERGE_SIMILARITY {
                joined = Some(gi);
                break;
            }
            if sim >= GREY_ZONE_LOW {
                grey.push(gi);
            }
        }
        if let Some(gi) = joined {
            if !groups[gi].photo_indices.contains(&candidate.photo_index) {
                groups[gi].photo_indices.push(candidate.photo_index);
            }
            continue;
        }
        let mut flags: Vec<String> = Vec::new();
        for gi in grey {
            let other_name = groups[gi].name.clone();
            flags.push(format!(
                "possible duplicate of {} (name similarity in the grey zone; kept separate)",
                py_repr_str(&other_name)
            ));
            groups[gi].flags.push(format!(
                "possible duplicate of {} (name similarity in the grey zone; kept separate)",
                py_repr_str(&candidate.name)
            ));
        }
        groups.push(CandidateGroup {
            name: candidate.name.clone(),
            photo_indices: vec![candidate.photo_index],
            flags,
        });
    }
    groups
}

// ----------------------------------------------------------------------
// The run itself
// ----------------------------------------------------------------------

pub struct RoomRunOptions {
    pub item_names: Vec<String>,
    pub auto: bool,
    pub scene: String,
    pub room_id: Option<String>,
    pub budget_usd: f64,
    pub frame_every_seconds: f64,
    pub max_frames_per_video: usize,
    pub max_photos_per_item: usize,
    pub probe_all_candidates: bool,
    pub store: Option<AssetStore>,
    pub chain: Option<RoomChain>,
    pub spend_probe: Option<SpendProbe>,
    pub verify_fn: Option<VerifyFn>,
    pub ledger_path: Option<PathBuf>,
    pub narrated_videos: Vec<PathBuf>,
    pub narrated_audios: Vec<PathBuf>,
    pub narration_fn: Option<NarrationFn>,
    pub narration_max_projected_usd: f64,
    /// Stage mode: "scene" (model-generated plate, the default),
    /// "wallpaper" or "studio_gray" (code-rendered plates, $0).
    pub stage_mode: String,
    /// Parsed comps document for market pricing, when supplied.
    pub comps: Option<Value>,
}

impl Default for RoomRunOptions {
    fn default() -> Self {
        RoomRunOptions {
            item_names: Vec::new(),
            auto: false,
            scene: DEFAULT_SCENE.to_string(),
            room_id: None,
            budget_usd: DEFAULT_BUDGET_USD,
            frame_every_seconds: 2.0,
            max_frames_per_video: 8,
            max_photos_per_item: MAX_PHOTOS_PER_ITEM,
            probe_all_candidates: true,
            store: None,
            chain: None,
            spend_probe: None,
            verify_fn: None,
            ledger_path: None,
            narrated_videos: Vec::new(),
            narrated_audios: Vec::new(),
            narration_fn: None,
            narration_max_projected_usd: NARRATION_DEFAULT_MAX_PROJECTED_USD,
            stage_mode: backdrop::STAGE_MODE_SCENE.to_string(),
            comps: None,
        }
    }
}

/// The identify call takes an image reference; a local file rides
/// as a base64 data URI, sniffed by suffix like Python's mimetypes.
fn data_uri_for_path(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let mime = match suffix_lower(path).as_str() {
        ".jpg" | ".jpeg" => "image/jpeg",
        ".png" => "image/png",
        ".webp" => "image/webp",
        ".heic" => "image/heic",
        ".heif" => "image/heif",
        ".gif" => "image/gif",
        _ => "application/octet-stream",
    };
    Ok(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    ))
}

fn image_dims(path: &Path) -> (u32, u32) {
    // PIL parity: any decode failure falls back to a mid-range phone
    // size so the projection stays honest rather than optimistic.
    let fallback = (1200, 1600);
    let Ok(bytes) = std::fs::read(path) else {
        return fallback;
    };
    match crate::image::decode_image(&bytes) {
        Ok(decoded) => {
            let rgba = decoded.into_rgba();
            (rgba.width, rgba.height)
        }
        Err(_) => fallback,
    }
}

fn attach_narrated(
    items: &mut Vec<WorkItem>,
    sources: &[PhotoSource],
    name: &str,
    source_path: &str,
    kind: &str,
) {
    for existing in items.iter_mut() {
        if token_similarity(Some(existing.name.as_str()), Some(name)) >= MERGE_SIMILARITY {
            let current_kind = existing
                .narration
                .as_ref()
                .and_then(|n| n.get("kind"))
                .and_then(Value::as_str)
                .map(str::to_string);
            // Intent precedence when one name earns several kinds:
            // an explicit "named" (sell) beats "keep", and both
            // explicit intents beat a passing "mentioned".
            let replace = match current_kind.as_deref() {
                None => true,
                Some("mentioned") if kind == "named" || kind == "keep" => true,
                Some("keep") if kind == "named" => true,
                _ => false,
            };
            if replace {
                existing.narration = Some(json!({
                    "kind": kind,
                    "source": source_path,
                }));
                if kind == "named"
                    && (existing.origin == "narration_mentioned"
                        || existing.origin == "narration_keep")
                {
                    existing.origin = "narration_selling".to_string();
                } else if kind == "keep" && existing.origin == "narration_mentioned" {
                    existing.origin = "narration_keep".to_string();
                }
            }
            return;
        }
    }
    let mut flags = Vec::new();
    if kind == "mentioned" {
        flags.push(
            "mentioned in passing in the seller's narration, not marked for sale; \
             included so the visual pipeline can confirm or refute it against the photos"
                .to_string(),
        );
    }
    if kind == "keep" {
        flags.push(
            "marked keep in the seller's narration: identified and cut out for an \
             ownership record; never staged or listed for sale"
                .to_string(),
        );
    }
    items.push(WorkItem {
        name: name.to_string(),
        candidate_photo_indices: rank_photos(name, sources),
        ranked_candidates: true,
        flags,
        narration: Some(json!({
            "kind": kind,
            "source": source_path,
        })),
        origin: match kind {
            "mentioned" => "narration_mentioned".to_string(),
            "keep" => "narration_keep".to_string(),
            _ => "narration_selling".to_string(),
        },
    });
}

/// Run one room. Returns the manifest value (also written to
/// out_dir/manifest.json, with summary.txt alongside).
pub fn run_room(
    still_paths: &[PathBuf],
    video_paths: &[PathBuf],
    out_dir: &Path,
    mut opts: RoomRunOptions,
) -> Result<Value, String> {
    let store = opts
        .store
        .take()
        .unwrap_or_else(|| AssetStore::new(&config::get_data_dir()));
    std::fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
    let room_id = opts.room_id.clone().unwrap_or_else(|| {
        let uuid = uuid::Uuid::new_v4().simple().to_string();
        format!("room-{}", &uuid[..8])
    });
    // The room spends against its OWN ledger: the run's cost must be
    // provable from the room directory alone, and a room run must
    // never move the operator's cumulative production ledger.
    //
    // Ledger-knob precedence (consolidation, tracker event 166):
    // `AGENTHAND_LEDGER` selects the store and `AGENTHAND_LEDGER_PATH`
    // overrides the JSONL store's path — the two are orthogonal. The
    // room ledger is an isolated JSONL file by design (Python parity:
    // the run's totals are read back from that file), so the run pins
    // BOTH knobs for its duration: the store selection to `jsonl`
    // (otherwise the shipped SQLite default would resolve to the
    // data-dir ledger and the path override would not apply) and the
    // path to the room's own file. Both are restored afterwards.
    let room_ledger = opts
        .ledger_path
        .clone()
        .unwrap_or_else(|| out_dir.join("ledger.jsonl"));
    let previous = std::env::var_os("AGENTHAND_LEDGER_PATH");
    std::env::set_var("AGENTHAND_LEDGER_PATH", &room_ledger);
    let previous_store = std::env::var_os("AGENTHAND_LEDGER");
    std::env::set_var("AGENTHAND_LEDGER", "jsonl");
    let result = run_room_body(
        still_paths,
        video_paths,
        out_dir,
        opts,
        store,
        &room_id,
        &room_ledger,
    );
    match previous {
        Some(value) => std::env::set_var("AGENTHAND_LEDGER_PATH", value),
        None => std::env::remove_var("AGENTHAND_LEDGER_PATH"),
    }
    match previous_store {
        Some(value) => std::env::set_var("AGENTHAND_LEDGER", value),
        None => std::env::remove_var("AGENTHAND_LEDGER"),
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn run_room_body(
    still_paths: &[PathBuf],
    video_paths: &[PathBuf],
    out_dir: &Path,
    mut opts: RoomRunOptions,
    store: AssetStore,
    room_id: &str,
    room_ledger: &Path,
) -> Result<Value, String> {
    let chain = opts.chain.take().unwrap_or_else(default_chain);
    let verify_fn = opts.verify_fn.take().unwrap_or_else(default_verify_fn);
    let narration_fn_owned: Option<NarrationFn> =
        if !opts.narrated_videos.is_empty() || !opts.narrated_audios.is_empty() {
            Some(
                opts.narration_fn
                    .take()
                    .unwrap_or_else(|| Box::new(|p: &Path| default_narration_fn(p)) as NarrationFn),
            )
        } else {
            None
        };
    let spend_probe: SpendProbe = match opts.spend_probe.take() {
        Some(probe) => probe,
        None => {
            let path = room_ledger.to_path_buf();
            Box::new(move || ledger::ledger_total_usd(&path).map_err(|e| e.to_string()))
        }
    };
    let spend_before = spend_probe()?;

    // ---- source pool -------------------------------------------------
    let mut sources: Vec<PhotoSource> = Vec::new();
    for path in collect_stills(still_paths) {
        sources.push(PhotoSource {
            path,
            origin: "still",
            video_path: None,
            timestamp_s: None,
            handle: None,
            width: None,
            height: None,
            plan: None,
            plan_error: None,
        });
    }
    let mut video_notes: Vec<Value> = Vec::new();
    for video in video_paths {
        let stem = video
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (frames, note, dropped) = sample_video_frames(
            video,
            &out_dir.join("frames").join(&stem),
            opts.frame_every_seconds,
            opts.max_frames_per_video,
            None,
        );
        for (frame_path, ts) in &frames {
            sources.push(PhotoSource {
                path: frame_path.clone(),
                origin: "video_frame",
                video_path: Some(video.to_string_lossy().into_owned()),
                timestamp_s: Some(*ts),
                handle: None,
                width: None,
                height: None,
                plan: None,
                plan_error: None,
            });
        }
        video_notes.push(json!({
            "path": video.to_string_lossy(),
            "frames_sampled": frames.len(),
            "frames_dropped": dropped,
            "frame_every_seconds": opts.frame_every_seconds,
            "note": note,
        }));
    }

    // ---- narration (a separate pool; never blocks the stills path) ---
    let mut narration_records: Vec<Value> = Vec::new();
    if let Some(narration_fn) = &narration_fn_owned {
        let work_dir = out_dir.join("narration");
        for video in &opts.narrated_videos {
            narration_records.push(narrate_one(
                video,
                "video",
                &work_dir,
                narration_fn,
                opts.narration_max_projected_usd,
            ));
        }
        for audio in &opts.narrated_audios {
            narration_records.push(narrate_one(
                audio,
                "audio",
                &work_dir,
                narration_fn,
                opts.narration_max_projected_usd,
            ));
        }
    }
    let mut narrated_names: Vec<(String, String, &'static str)> = Vec::new(); // (name, source, kind)
    for record in &narration_records {
        let source = record
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        for name in record
            .get("named_items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if let Some(name) = name.as_str() {
                narrated_names.push((name.to_string(), source.clone(), "named"));
            }
        }
        for name in record
            .get("kept_items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if let Some(name) = name.as_str() {
                narrated_names.push((name.to_string(), source.clone(), "keep"));
            }
        }
        for name in record
            .get("mentioned_items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if let Some(name) = name.as_str() {
                narrated_names.push((name.to_string(), source.clone(), "mentioned"));
            }
        }
    }
    let mut transcript_file: Option<String> = None;
    {
        let spoken: Vec<&Value> = narration_records
            .iter()
            .filter(|r| {
                r.get("transcript")
                    .and_then(Value::as_str)
                    .map(|t| !t.is_empty())
                    .unwrap_or(false)
            })
            .collect();
        if !spoken.is_empty() {
            let blocks: Vec<String> = spoken
                .iter()
                .map(|r| {
                    format!(
                        "# Narration transcript — {}\n{}",
                        r.get("path").and_then(Value::as_str).unwrap_or(""),
                        r.get("transcript").and_then(Value::as_str).unwrap_or("")
                    )
                })
                .collect();
            let path = out_dir.join("transcript.txt");
            let text = format!("{}\n", blocks.join("\n\n"));
            std::fs::write(&path, text).map_err(|e| e.to_string())?;
            transcript_file = Some(path.to_string_lossy().into_owned());
        }
    }

    // ---- worst-case projection gate (before ANY billable call) -------
    let mut notes: Vec<String> = Vec::new();
    let mut identify_projection = 0.0f64;
    for source in sources.iter_mut() {
        let (w, h) = image_dims(&source.path);
        source.width = Some(w);
        source.height = Some(h);
        identify_projection += project_identify_cost_usd(IDENTIFY_MODEL, Some(w), Some(h))?
            * IDENTIFY_MAX_ATTEMPTS as f64;
    }
    let mut n_items_estimate = opts.item_names.len() + narrated_names.len();
    if n_items_estimate == 0 {
        n_items_estimate = sources.len();
    }
    let draw_projection = if backdrop::is_backdrop_mode(&opts.stage_mode) {
        0.0 // code-rendered plates make no model call
    } else {
        project_draw_cost_usd(DRAW_BACKGROUND_MODEL)?
    };
    let per_item_projection = MAX_SAM_CALLS_PER_ITEM as f64
        * project_cut_out_cost_usd(CUT_OUT_MODEL, 1)?
        + draw_projection;
    let worst_case = identify_projection + n_items_estimate as f64 * per_item_projection;
    if worst_case > opts.budget_usd {
        return Err(format!(
            "Room run refused before any call: worst-case projection ${worst_case:.4} \
             exceeds the ${:.2} run budget ({} photos, ~{} items). Reduce the item \
             set or the photo pool, or raise --budget-usd.",
            opts.budget_usd,
            sources.len(),
            n_items_estimate
        ));
    }
    notes.push(format!(
        "worst-case projection ${worst_case:.4} within the ${:.2} run budget",
        opts.budget_usd
    ));

    // ---- step log (P1, tracker event 372) ------------------------------
    // Durable per-step state for crash resume, in the room's own
    // directory beside its ledger. Rows are keyed by the run id the
    // caller supplied (or run_room generated); a rerun with the
    // same id replays what a crashed attempt settled.
    let step_log = std::rc::Rc::new(
        crate::sqlledger::RunStepLog::open(&out_dir.join("room-steps.sqlite3"))?,
    );
    let resumed = step_log.run_exists(room_id)?;

    // ---- plan pass: one identify per source photo --------------------
    for (photo_index, source) in sources.iter_mut().enumerate() {
        // A plan settled in a previous attempt replays from the
        // step log: no second identify bill for the same photo.
        if let Some(row) = step_log.get(room_id, photo_index as i64, "identify")? {
            if row.status == "complete" || row.status == "failed" {
                let refv = step_ref(&row);
                source.plan = refv.get("plan").cloned().filter(|v| !v.is_null());
                source.plan_error = refv
                    .get("plan_error")
                    .cloned()
                    .filter(|v| !v.is_null());
                source.handle = refv
                    .get("handle")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                continue;
            }
        }
        step_log.mark_pending(room_id, photo_index as i64, "identify", None)?;
        let data_uri = data_uri_for_path(&source.path)?;
        let result = (chain.identify)(&data_uri)?;
        if result.get("status").and_then(Value::as_str) == Some("ok") {
            source.plan = Some(result.clone());
            source.handle = result
                .get("image")
                .and_then(Value::as_str)
                .map(str::to_string);
        } else {
            let error = result.get("error").filter(|e| py_truthy(e)).cloned();
            source.plan_error = Some(error.unwrap_or_else(
                || json!({"code": "IDENTIFY_FAILED", "message": result.to_string()}),
            ));
            // A failed plan must not orphan the photo: ingest it
            // directly so item probing can still find handles.
            match ingest::ingest(&data_uri, &store) {
                Ok(handle) => source.handle = Some(handle),
                Err(exc) => {
                    let prev_message = source
                        .plan_error
                        .as_ref()
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    source.plan_error = Some(json!({
                        "code": "INGEST_FAILED",
                        "message": format!("{prev_message} | ingest: {exc}"),
                    }));
                }
            }
        }
        let refj = json!({
            "plan": source.plan,
            "plan_error": source.plan_error,
            "handle": source.handle,
        });
        let call_id = result.get("call_id").and_then(Value::as_str);
        if source.plan.is_some() {
            step_log.mark_complete(
                room_id,
                photo_index as i64,
                "identify",
                call_id,
                &refj.to_string(),
            )?;
        } else {
            step_log.mark_failed(room_id, photo_index as i64, "identify", &refj.to_string())?;
        }
    }

    // ---- the work list ------------------------------------------------
    let had_typed_items = !opts.item_names.is_empty();
    let mut items_work: Vec<WorkItem> = Vec::new();
    let mut mode = "named";
    if opts.auto {
        mode = "auto";
        let candidates: Vec<Candidate> = sources
            .iter()
            .enumerate()
            .filter_map(|(idx, source)| {
                let plan = source.plan.as_ref()?;
                let concept = plan.get("concept").and_then(Value::as_str)?;
                if concept.is_empty() {
                    return None;
                }
                Some(Candidate {
                    name: concept.to_string(),
                    photo_index: idx,
                    plan: plan.clone(),
                })
            })
            .collect();
        for group in group_candidates(&candidates) {
            items_work.push(WorkItem {
                name: group.name,
                candidate_photo_indices: group.photo_indices,
                ranked_candidates: false,
                flags: group.flags,
                narration: None,
                origin: "named".to_string(),
            });
        }
    } else {
        for name in &opts.item_names {
            items_work.push(WorkItem {
                candidate_photo_indices: rank_photos(name, &sources),
                ranked_candidates: true,
                name: name.clone(),
                flags: Vec::new(),
                narration: None,
                origin: "named".to_string(),
            });
        }
    }
    // Narration names join the work list: "named" items are the
    // seller's own sale list; "mentioned" items are hypotheses the
    // visual pipeline confirms or refutes. A name that matches an
    // existing item annotates it instead of duplicating the work.
    // For-sale names attach first, then passing mentions (the
    // reference build's order).
    for (name, source_path, kind) in &narrated_names {
        if *kind == "named" {
            attach_narrated(&mut items_work, &sources, name, source_path, kind);
        }
    }
    for (name, source_path, kind) in &narrated_names {
        if *kind == "keep" {
            attach_narrated(&mut items_work, &sources, name, source_path, kind);
        }
    }
    for (name, source_path, kind) in &narrated_names {
        if *kind == "mentioned" {
            attach_narrated(&mut items_work, &sources, name, source_path, kind);
        }
    }
    if !narration_records.is_empty() && !opts.auto && !had_typed_items {
        mode = "narrated";
    }
    let candidate_cap = opts.max_photos_per_item.min(MAX_PHOTOS_PER_ITEM);
    for item in items_work.iter_mut() {
        if item.ranked_candidates {
            item.candidate_photo_indices =
                apply_candidate_coverage(&item.candidate_photo_indices, &sources, candidate_cap);
        } else {
            item.candidate_photo_indices.truncate(candidate_cap);
        }
    }

    // ---- per-item processing ------------------------------------------
    let ctx = RunContext {
        store,
        chain,
        scene: opts.scene.clone(),
        stage_mode: opts.stage_mode.clone(),
        verify_fn: &verify_fn,
        max_photos_per_item: opts.max_photos_per_item,
        max_sam_calls_per_item: MAX_SAM_CALLS_PER_ITEM,
        sam_calls: std::cell::Cell::new(0),
        step_log: Some(step_log.clone()),
        run_id: room_id.to_string(),
        call_ids: std::cell::RefCell::new(Vec::new()),
    };
    let mut item_records: Vec<Value> = Vec::new();
    let mut item_evidence: Vec<Option<Value>> = Vec::new();
    let mut budget_stopped = false;
    for (item_index, item) in items_work.iter().enumerate() {
        if (spend_probe()? - spend_before) > opts.budget_usd {
            budget_stopped = true;
        }
        if budget_stopped {
            item_records.push(json!({
                "name": item.name,
                "status": "failed",
                "stage_reached": "none",
                "failure": {
                    "stage": "budget",
                    "code": "BUDGET_STOP",
                    "message": format!(
                        "run spend crossed the ${:.2} budget before this item was attempted",
                        opts.budget_usd
                    ),
                },
                "narration": item.narration,
                "origin": item.origin,
                "intent": intent_of_origin(&item.origin),
                "stage_mode": opts.stage_mode,
                "pricing": Value::Null,
                "sightings": [],
                "primary_photo_index": Value::Null,
                "also_seen_in": [],
                "output_asset": Value::Null,
                "receipt": Value::Null,
                "receipt_file": Value::Null,
                "verification": Value::Null,
                "listing": Value::Null,
                "ownership": Value::Null,
                "possible_duplicate_of": [],
                "flags": item.flags,
                "cost_usd": 0.0,
            }));
            item_evidence.push(None);
            continue;
        }
        // The SAM budget is per item, not per run.
        ctx.sam_calls.set(0);
        let (record, evidence) = match &ctx.step_log {
            Some(log) => process_item_logged(
                &ctx,
                item,
                &sources,
                opts.probe_all_candidates,
                log,
                room_id,
                item_index as i64,
            )?,
            None => process_item(&ctx, item, &sources, opts.probe_all_candidates)?,
        };
        item_records.push(record);
        item_evidence.push(evidence);
    }
    flag_cross_item_duplicates(&mut item_records);
    // Collision re-selection: two staged items contest one
    // physical object when a primary sighting collides with any
    // sighting of the other item; they get one chance to separate
    // when the listing evidence names a keeper and the displaced
    // item has a clean alternate sighting.
    let collision_notes = resolve_primary_collisions(
        &ctx,
        &mut item_records,
        &sources,
        &mut item_evidence,
        &spend_probe,
        spend_before,
        opts.budget_usd,
    )?;
    notes.extend(collision_notes);
    // Market pricing: every item record carries a pricing entry,
    // priced from the supplied comps or marked as unsupplied.
    for record in item_records.iter_mut() {
        let item_name = record
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        record["pricing"] = market_pricing::pricing_for_item(opts.comps.as_ref(), &item_name);
    }
    let total_cost = python_round(spend_probe()? - spend_before, 6);

    // ---- narration section (Baseten pool, kept OUT of the room total) -
    let mut narration_section = Value::Null;
    let mut baseten_cost: Option<f64> = None;
    if !narration_records.is_empty() {
        let mut totals = [0i64; 3];
        for record in &narration_records {
            if let Some(usage) = record.get("usage").filter(|u| u.is_object()) {
                for (i, key) in ["prompt_tokens", "completion_tokens", "total_tokens"]
                    .iter()
                    .enumerate()
                {
                    totals[i] += usage.get(key).and_then(Value::as_i64).unwrap_or(0);
                }
            }
        }
        let cost = python_round(
            narration_records
                .iter()
                .map(|r| {
                    r.get("cost_usd_estimate")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0)
                })
                .sum(),
            6,
        );
        baseten_cost = Some(cost);
        narration_section = json!({
            "model": NARRATION_MODEL,
            "pool": "baseten_partner_credits",
            "files": narration_records,
            "transcript_file": transcript_file,
            "usage": {
                "prompt_tokens": totals[0],
                "completion_tokens": totals[1],
                "total_tokens": totals[2],
            },
            "cost_usd_estimate": cost,
        });
        for record in &narration_records {
            let path = record.get("path").and_then(Value::as_str).unwrap_or("");
            if let Some(error) = record.get("error").filter(|e| !e.is_null()) {
                notes.push(format!(
                    "narration from {path} failed [{}]: {}",
                    error.get("code").and_then(Value::as_str).unwrap_or(""),
                    error.get("message").and_then(Value::as_str).unwrap_or("")
                ));
            } else {
                notes.push(format!(
                    "narration from {path}: {} item(s) named for sale, {} kept, {} mentioned in passing",
                    record
                        .get("named_items")
                        .and_then(Value::as_array)
                        .map(|a| a.len())
                        .unwrap_or(0),
                    record
                        .get("kept_items")
                        .and_then(Value::as_array)
                        .map(|a| a.len())
                        .unwrap_or(0),
                    record
                        .get("mentioned_items")
                        .and_then(Value::as_array)
                        .map(|a| a.len())
                        .unwrap_or(0)
                ));
            }
        }
    }

    let pricing_source = opts.comps.as_ref().map(|doc| {
        json!({
            "source": market_pricing::PRICING_SOURCE,
            "area": doc.get("area").cloned().unwrap_or(Value::Null),
            "radius_mi": doc.get("radius_mi").cloned().unwrap_or(Value::Null),
            "pulled_at": doc.get("pulled_at").cloned().unwrap_or(Value::Null),
        })
    });
    let mut manifest = json!({
        "room_id": room_id,
        "created_at": iso_now_micros(),
        "mode": mode,
        "stage_mode": opts.stage_mode,
        "pricing_source": pricing_source,
        "room_style": opts.scene,
        "budget_usd": opts.budget_usd,
        "ledger_path": room_ledger.to_string_lossy(),
        "total_cost_usd": total_cost,
        "baseten_cost_usd_estimate": baseten_cost,
        "narration": narration_section,
        "sources": sources
            .iter()
            .enumerate()
            .map(|(i, s)| s.manifest_entry(i))
            .collect::<Vec<_>>(),
        "videos": video_notes,
        "items": item_records,
        "notes": notes,
    });
    if let Some(comps_doc) = &opts.comps {
        market_pricing::write_priced_artifacts(out_dir, &manifest, comps_doc, &ctx.store)?;
    }
    // P1 honesty (spec point 4): a resumed run says so, and only
    // then — a fresh run's manifest is byte-identical to the
    // pre-P1 shape. Replayed items carry "replayed": true on
    // their records (set in process_item_logged).
    if resumed {
        manifest["resumed"] = json!(true);
    }
    manifest["inventory"] =
        build_keep_inventory(&ctx.store, out_dir, room_id, &manifest["items"])?;
    let manifest_path = out_dir.join("manifest.json");
    let serialized = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?;
    std::fs::write(&manifest_path, serialized).map_err(|e| e.to_string())?;
    let summary_path = out_dir.join("summary.txt");
    std::fs::write(&summary_path, summarize_manifest(&manifest)).map_err(|e| e.to_string())?;
    Ok(manifest)
}

// ----------------------------------------------------------------------
// Summary rendering (the readable half of the manifest — byte-parity
// with the Python renderer, pinned by a test against the frozen
// Python runs' summary.txt files)
// ----------------------------------------------------------------------

fn get_f(manifest: &Value, key: &str) -> f64 {
    manifest.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn get_str_or<'a>(value: &'a Value, key: &str, fallback: &'a str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or(fallback)
}

pub fn summarize_manifest(manifest: &Value) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "ROOM RUN {} — {}",
        get_str_or(manifest, "room_id", ""),
        get_str_or(manifest, "created_at", "")
    ));
    let sources = manifest
        .get("sources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let items = manifest
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    lines.push(format!(
        "mode: {} | sources: {} | items: {} | total cost: ${:.6}",
        get_str_or(manifest, "mode", ""),
        sources.len(),
        items.len(),
        get_f(manifest, "total_cost_usd")
    ));
    lines.push(format!(
        "room style: {}",
        get_str_or(manifest, "room_style", "")
    ));
    lines.push(String::new());
    for entry in &sources {
        let mut origin = get_str_or(entry, "origin", "").to_string();
        if let Some(ts) = entry.get("timestamp_s").filter(|t| !t.is_null()) {
            origin += &format!(" @ {}s", py_str(ts));
        }
        let concept = entry
            .get("plan")
            .and_then(|p| p.get("concept"))
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .unwrap_or("—");
        lines.push(format!(
            "  photo {}: [{}] {} — plan: {}",
            py_str(entry.get("index").unwrap_or(&Value::Null)),
            origin,
            get_str_or(entry, "path", ""),
            concept
        ));
    }
    if let Some(narration) = manifest.get("narration").filter(|n| py_truthy(n)) {
        lines.push(String::new());
        lines.push("NARRATION (Baseten pool — separate from the room total above):".to_string());
        let files = narration
            .get("files")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for record in &files {
            let path = get_str_or(record, "path", "");
            if let Some(error) = record.get("error").filter(|e| !e.is_null()) {
                lines.push(format!(
                    "  {path}: ERROR [{}] {}",
                    get_str_or(error, "code", ""),
                    get_str_or(error, "message", "")
                ));
                continue;
            }
            let named = record
                .get("named_items")
                .filter(|v| py_truthy(v))
                .map(py_str)
                .unwrap_or_else(|| "none".to_string());
            let mentioned = record
                .get("mentioned_items")
                .filter(|v| py_truthy(v))
                .map(py_str)
                .unwrap_or_else(|| "none".to_string());
            // Kept renders only when the record carries the field,
            // so summaries of pre-KEEP frozen manifests stay
            // byte-identical (the parity fixtures).
            let kept_part = match record.get("kept_items") {
                Some(kept) => format!(
                    "kept: {}; ",
                    if py_truthy(kept) {
                        py_str(kept)
                    } else {
                        "none".to_string()
                    }
                ),
                None => String::new(),
            };
            lines.push(format!(
                "  {path}: {}s of speech — named for sale: {named}; {kept_part}mentioned: {mentioned}; est. ${:.6}",
                py_str(record.get("duration_s").unwrap_or(&Value::Null)),
                record
                    .get("cost_usd_estimate")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
            ));
            lines.push("  --- transcript ---".to_string());
            let transcript = record
                .get("transcript")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !transcript.is_empty() {
                for line in transcript.split('\n') {
                    lines.push(format!("  {line}"));
                }
            }
            lines.push("  --- end transcript ---".to_string());
        }
        let usage = narration.get("usage").cloned().unwrap_or(Value::Null);
        let usage_repr = format!(
            "{{'prompt_tokens': {}, 'completion_tokens': {}, 'total_tokens': {}}}",
            py_str(usage.get("prompt_tokens").unwrap_or(&Value::Null)),
            py_str(usage.get("completion_tokens").unwrap_or(&Value::Null)),
            py_str(usage.get("total_tokens").unwrap_or(&Value::Null))
        );
        lines.push(format!(
            "  Baseten total: ${:.6} (usage {usage_repr}); transcript: {}",
            narration
                .get("cost_usd_estimate")
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
            py_str(narration.get("transcript_file").unwrap_or(&Value::Null))
        ));
    }
    lines.push(String::new());
    let mut staged = 0usize;
    for item in &items {
        let name = get_str_or(item, "name", "");
        let tag = item
            .get("narration")
            .filter(|n| py_truthy(n))
            .and_then(|n| n.get("kind"))
            .and_then(Value::as_str)
            .filter(|k| !k.is_empty())
            .map(|kind| format!(" [narration: {kind}]"))
            .unwrap_or_default();
        let status = get_str_or(item, "status", "");
        let cost = item.get("cost_usd").and_then(Value::as_f64).unwrap_or(0.0);
        if status == "staged" {
            staged += 1;
            let receipt = item.get("receipt").cloned().unwrap_or(Value::Null);
            let seen = item
                .get("also_seen_in")
                .filter(|v| py_truthy(v))
                .map(|v| format!(" (also seen in photos {})", py_str(v)))
                .unwrap_or_default();
            lines.push(format!(
                "  STAGED  {name}{tag}: photo {}{seen} — {} opaque px, {} changed, profile preserved: {} — ${cost:.6}",
                py_str(item.get("primary_photo_index").unwrap_or(&Value::Null)),
                py_str(receipt.get("opaque_pixels").unwrap_or(&Value::Null)),
                py_str(receipt.get("changed_opaque_pixels").unwrap_or(&Value::Null)),
                py_str(receipt.get("color_profile_preserved").unwrap_or(&Value::Null)),
            ));
            lines.push(format!(
                "          receipt: {}",
                py_str(item.get("receipt_file").unwrap_or(&Value::Null))
            ));
            let verification = item.get("verification").cloned().unwrap_or(Value::Null);
            lines.push(format!(
                "          verified independently: {} opaque px recounted from disk, {} changed",
                py_str(verification.get("opaque_pixels").unwrap_or(&Value::Null)),
                py_str(
                    verification
                        .get("changed_opaque_pixels")
                        .unwrap_or(&Value::Null)
                ),
            ));
            // Every staged item carries its listing title and its
            // pricing outcome (the reference renderer's lines).
            let listing_title = item
                .get("listing")
                .and_then(|l| l.get("title"))
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .unwrap_or("\u{2014}");
            lines.push(format!("          listing title: {listing_title}"));
            let pricing = item.get("pricing").cloned().unwrap_or(Value::Null);
            match pricing.get("status").and_then(Value::as_str) {
                Some("priced") => {
                    let range = pricing.get("range").cloned().unwrap_or(Value::Null);
                    lines.push(format!(
                        "          pricing: ${}\u{2013}${} range, suggested ${} ({}, {} comps)",
                        py_str(range.get("quick").unwrap_or(&Value::Null)),
                        py_str(range.get("premium").unwrap_or(&Value::Null)),
                        py_str(pricing.get("suggested").unwrap_or(&Value::Null)),
                        py_str(pricing.get("confidence").unwrap_or(&Value::Null)),
                        py_str(pricing.get("comps_used").unwrap_or(&Value::Null)),
                    ));
                }
                Some("unpriced_thin_market") => {
                    lines.push(format!(
                        "          pricing: not priced \u{2014} thin market ({} comps)",
                        py_str(pricing.get("comps_used").unwrap_or(&Value::Null)),
                    ));
                }
                _ => {
                    lines.push("          pricing: no comps supplied".to_string());
                }
            }
        } else if status == "verify_failed" {
            let failure = item.get("failure").cloned().unwrap_or(Value::Null);
            lines.push(format!(
                "  VERIFY FAILED  {name}{tag}: the staged output did not pass independent verification — {}",
                py_str(failure.get("message").unwrap_or(&Value::Null))
            ));
            lines.push(format!(
                "          receipt: {}",
                py_str(item.get("receipt_file").unwrap_or(&Value::Null))
            ));
        } else {
            let failure = item.get("failure").cloned().unwrap_or(Value::Null);
            lines.push(format!(
                "  FAILED  {name}{tag}: stage {} [{}] {}",
                py_str(failure.get("stage").unwrap_or(&Value::Null)),
                py_str(failure.get("code").unwrap_or(&Value::Null)),
                py_str(failure.get("message").unwrap_or(&Value::Null)),
            ));
        }
        if let Some(flags) = item.get("flags").and_then(Value::as_array) {
            for flag in flags {
                lines.push(format!("          flag: {}", py_str(flag)));
            }
        }
    }
    lines.push(String::new());
    lines.push(format!("{staged} of {} items staged.", items.len()));
    let mut text = lines.join("\n");
    text.push('\n');
    text
}
