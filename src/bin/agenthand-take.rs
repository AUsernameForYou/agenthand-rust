//! `agenthand-take` — the filmed-demo MCP client, a Rust port of
//! `hero1443-merged.py` (the driver behind the recorded Terminal take).
//!
//! It spawns the shipped server binary (`<bin> serve`), speaks raw
//! JSON-RPC over stdio, and runs identify -> cut_out -> draw -> look-test
//! draw -> independent `<bin> verify`, printing the on-screen transcript
//! line by line and teeing it to `<out>/session-transcript.txt`. The
//! artifacts land in `--out` under the same names the Python wrote.
//!
//! Every printed line and every JSON file is formatted the way Python
//! formats it (`%s` of a float is `repr`, `format(n, ",")`, `json.dumps`
//! separators and `ensure_ascii`, `indent=2`), so a take from this binary
//! reads exactly like a take from the Python driver.
//!
//! The Meta key is read ONLY from the environment (`META_API_KEY`, which
//! the launcher exports by sourcing `~/.config/agenthand/env`); it is
//! passed to the server through the inherited environment and never
//! printed or written anywhere.
//!
//! `--dry-run` performs initialize and tools/list against the server and
//! stops before identify: no Meta call, nothing billed, no key needed.
//!
//! The formatting helpers are `pub` so `tests/take_transcript.rs` can
//! include this file as a module and check the transcript lines against
//! fixed sample responses.

use serde_json::{Number, Value};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Scene text for the main draw: none, so the server's merged-build
/// defaults pick the plate prompt (Python `SCENE = ""`).
pub const SCENE: &str = "";
/// Scene text for the extra look-test draw (Python `LOOK_SCENE`).
pub const LOOK_SCENE: &str = "the same place, tidy and bright, with a large round \
cyan-blue circular logo sign on the back wall";
/// Per-response wait, as the Python `_read_line(timeout=600)`.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(600);
/// Fallback server binary, relative to $HOME (the Python `BIN`).
pub const RELEASE_FALLBACK: &str = "agenthand-release/agenthand-0.3.0-macos-arm64";
/// clientInfo sent on initialize (same as the Python driver).
pub const CLIENT_NAME: &str = "hero-1443-take";
pub const CLIENT_VERSION: &str = "1";

// ---------------------------------------------------------------------
// Python-compatible value formatting
// ---------------------------------------------------------------------

/// `repr(float)` as Python 3 prints it: shortest round-trip digits,
/// positional for exponents -4..16 (always with a fractional part),
/// otherwise `1e-05` / `1.5e+16` style.
pub fn py_float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let sci = format!("{f:e}");
    let (mant, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if (-4..16).contains(&exp) {
        let s = format!("{f}");
        if s.contains('.') {
            s
        } else {
            format!("{s}.0")
        }
    } else {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{mant}e{sign}{:02}", exp.unsigned_abs())
    }
}

fn py_number(n: &Number) -> String {
    if let Some(i) = n.as_i64() {
        i.to_string()
    } else if let Some(u) = n.as_u64() {
        u.to_string()
    } else {
        py_float_repr(n.as_f64().unwrap_or(f64::NAN))
    }
}

/// `repr(str)`: single quotes unless the text holds a `'` and no `"`.
pub fn py_str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `repr(value)` for a JSON-decoded Python value.
pub fn py_repr(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) => py_number(n),
        Value::String(s) => py_str_repr(s),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(py_repr).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", py_str_repr(k), py_repr(v)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

/// `"%s" % value` where value came from `dict.get(key)` (missing -> None).
pub fn py_str(v: Option<&Value>) -> String {
    match v {
        None => "None".into(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => py_repr(other),
    }
}

/// Python truthiness of an optional JSON value.
pub fn py_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(m)) => !m.is_empty(),
    }
}

fn group_thousands(digits: &str) -> String {
    let (sign, body) = match digits.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", digits),
    };
    let mut out = String::with_capacity(body.len() + body.len() / 3);
    for (i, c) in body.chars().enumerate() {
        if i > 0 && (body.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("{sign}{out}")
}

/// `format(d.get(key, 0), ",")`. `None` where Python would raise
/// (a present-but-non-numeric value).
pub fn py_commas_checked(v: Option<&Value>) -> Option<String> {
    match v {
        None => Some("0".into()),
        Some(Value::Bool(b)) => Some(if *b { "1".into() } else { "0".into() }),
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                Some(group_thousands(&i.to_string()))
            } else if let Some(u) = n.as_u64() {
                Some(group_thousands(&u.to_string()))
            } else {
                let r = py_float_repr(n.as_f64().unwrap_or(f64::NAN));
                if r.contains('e') || r.contains("inf") || r.contains("nan") {
                    Some(r)
                } else {
                    let (int_part, frac) = r.split_once('.').unwrap_or((r.as_str(), "0"));
                    Some(format!("{}.{}", group_thousands(int_part), frac))
                }
            }
        }
        Some(_) => None,
    }
}

/// `format(d.get(key, 0), ",")`, falling back to `%s` of the value.
pub fn py_commas(v: Option<&Value>) -> String {
    py_commas_checked(v).unwrap_or_else(|| py_str(v))
}

/// A JSON value whose object key order is chosen by the caller (Python
/// dicts keep insertion order; serde_json's map is sorted).
pub enum PyJson {
    V(Value),
    Obj(Vec<(String, PyJson)>),
}

/// Ordered object helper.
pub fn obj(pairs: Vec<(&str, PyJson)>) -> PyJson {
    PyJson::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// Plain value helper.
pub fn val(v: Value) -> PyJson {
    PyJson::V(v)
}

fn json_escape_into(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            c => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

fn json_number(n: &Number) -> String {
    if n.as_i64().is_some() || n.as_u64().is_some() {
        return py_number(n);
    }
    let f = n.as_f64().unwrap_or(f64::NAN);
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else {
        py_float_repr(f)
    }
}

fn newline_indent(out: &mut String, indent: Option<usize>, level: usize) {
    if let Some(width) = indent {
        out.push('\n');
        out.push_str(&" ".repeat(width * level));
    }
}

fn write_seq<'a, I>(out: &mut String, open: char, close: char, items: I, indent: Option<usize>, level: usize)
where
    I: ExactSizeIterator<Item = (Option<&'a str>, PyRef<'a>)>,
{
    if items.len() == 0 {
        out.push(open);
        out.push(close);
        return;
    }
    out.push(open);
    let sep = if indent.is_some() { "," } else { ", " };
    for (i, (key, item)) in items.enumerate() {
        if i > 0 {
            out.push_str(sep);
        }
        newline_indent(out, indent, level + 1);
        if let Some(k) = key {
            json_escape_into(out, k);
            out.push_str(": ");
        }
        write_ref(out, item, indent, level + 1);
    }
    newline_indent(out, indent, level);
    out.push(close);
}

#[derive(Clone, Copy)]
enum PyRef<'a> {
    V(&'a Value),
    P(&'a PyJson),
}

fn write_ref(out: &mut String, item: PyRef<'_>, indent: Option<usize>, level: usize) {
    match item {
        PyRef::P(PyJson::V(v)) | PyRef::V(v) => write_value(out, v, indent, level),
        PyRef::P(PyJson::Obj(pairs)) => write_seq(
            out,
            '{',
            '}',
            pairs.iter().map(|(k, v)| (Some(k.as_str()), PyRef::P(v))),
            indent,
            level,
        ),
    }
}

fn write_value(out: &mut String, v: &Value, indent: Option<usize>, level: usize) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&json_number(n)),
        Value::String(s) => json_escape_into(out, s),
        Value::Array(items) => write_seq(
            out,
            '[',
            ']',
            items.iter().map(|v| (None, PyRef::V(v))),
            indent,
            level,
        ),
        Value::Object(map) => write_seq(
            out,
            '{',
            '}',
            map.iter().map(|(k, v)| (Some(k.as_str()), PyRef::V(v))),
            indent,
            level,
        ),
    }
}

/// `json.dumps(value)` (separators `", "` / `": "`, ensure_ascii).
pub fn py_json_dumps(v: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, v, None, 0);
    out
}

/// `json.dumps(value, indent=2)` (no trailing newline).
pub fn py_json_dumps_indent(v: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, v, Some(2), 0);
    out
}

/// `json.dumps` of an ordered structure; `indent` as Python's.
pub fn py_json_dumps_ordered(v: &PyJson, indent: Option<usize>) -> String {
    let mut out = String::new();
    write_ref(&mut out, PyRef::P(v), indent, 0);
    out
}

/// First `n` characters (Python `s[:n]`).
pub fn py_slice(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Standard base64 with padding (Python `base64.b64encode`).
pub fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

/// `handle.replace("asset://", "")`.
pub fn hex_of(handle: &str) -> String {
    handle.replace("asset://", "")
}

// ---------------------------------------------------------------------
// Transcript lines (pure; unit-tested against sample responses)
// ---------------------------------------------------------------------

/// The serve banner: the real binary's file name, no display alias.
pub fn banner_line(bin_file_name: &str, version: &str) -> String {
    format!("$ {bin_file_name} serve   # agenthand {version} (Rust), MCP stdio")
}

/// `initialize -> serverInfo name=.. version=..` from the full response.
pub fn init_line(init: &Value) -> Result<String, String> {
    let info = init
        .get("result")
        .and_then(|r| r.get("serverInfo"))
        .ok_or_else(|| format!("initialize returned no result.serverInfo: {}", py_slice(&py_json_dumps(init), 600)))?;
    Ok(format!(
        "initialize -> serverInfo name={} version={}",
        py_str(info.get("name")),
        py_str(info.get("version"))
    ))
}

/// `tools/list -> ['identify', 'cut_out', 'draw']` from the full response.
pub fn tools_list_line(resp: &Value) -> Result<String, String> {
    let tools = resp
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .ok_or_else(|| format!("tools/list returned no result.tools: {}", py_slice(&py_json_dumps(resp), 600)))?;
    let names: Vec<Value> = tools
        .iter()
        .map(|t| t.get("name").cloned().unwrap_or(Value::Null))
        .collect();
    Ok(format!("tools/list -> {}", py_repr(&Value::Array(names))))
}

pub fn identify_call_lines(src_name: &str, src_bytes: u64) -> Vec<String> {
    vec![
        String::new(),
        format!(
            "$ tools/call identify  image={} ({} bytes)",
            src_name,
            group_thousands(&src_bytes.to_string())
        ),
    ]
}

pub fn identify_result_lines(ident: &Value, secs: f64) -> Vec<String> {
    vec![
        format!("  muse-spark-1.3 thinking ... {secs:.1}s"),
        format!("  model        : {}", py_str(ident.get("model"))),
        format!("  request id   : {}", py_str(ident.get("call_id"))),
        format!("  concept      : {}", py_str(ident.get("concept"))),
        format!("  title        : {}", py_str(ident.get("suggested_title"))),
        format!("  condition    : {}", py_str(ident.get("condition_notes"))),
        format!("  settled cost : ${}", py_str(ident.get("cost_usd"))),
    ]
}

pub fn cut_call_lines(src_handle: &str) -> Vec<String> {
    vec![String::new(), format!("$ tools/call cut_out  image={src_handle}")]
}

pub fn cut_result_lines(cut: &Value, secs: f64) -> Vec<String> {
    let mask_pixels = cut.get("mask_review").and_then(|m| m.get("mask_pixels"));
    vec![
        format!("  sam-3.1 segmenting ... {secs:.1}s"),
        format!("  model        : {}", py_str(cut.get("model"))),
        format!("  request id   : {}", py_str(cut.get("call_id"))),
        format!("  mask pixels  : {}", py_commas(mask_pixels)),
        format!("  settled cost : ${}", py_str(cut.get("cost_usd"))),
    ]
}

pub fn draw_call_lines(fg: &str, scene: &str) -> Vec<String> {
    vec![
        String::new(),
        format!("$ tools/call draw  asset={fg}"),
        format!(
            "  scene: {}",
            if scene.is_empty() { "(none - merged-build defaults)" } else { scene }
        ),
    ]
}

pub fn draw_result_lines(draw: &Value, secs: f64) -> Vec<String> {
    vec![
        format!("  muse-image-1.0 generating background plate only ... {secs:.1}s"),
        format!("  model        : {}", py_str(draw.get("model"))),
        format!("  request id   : {}", py_str(draw.get("call_id"))),
        format!("  settled cost : ${}", py_str(draw.get("cost_usd"))),
    ]
}

/// `<tool> FAILED: <json.dumps(result)[:300]>`.
pub fn failed_line(tool: &str, result: &Value) -> String {
    format!("{tool} FAILED: {}", py_slice(&py_json_dumps(result), 300))
}

pub fn receipt_lines(draw: &Value) -> Vec<String> {
    let receipt = draw.get("fidelity_receipt");
    let get = |k: &str| receipt.and_then(|r| r.get(k));
    vec![
        String::new(),
        "  FIDELITY LOCK RECEIPT".into(),
        format!("  product pixels checked : {}", py_commas(get("opaque_pixels"))),
        format!("  product pixels changed : {}", py_str(get("changed_opaque_pixels"))),
        format!("  verify: {}", py_str(draw.get("verify_url"))),
    ]
}

pub fn look_call_lines(fg: &str, scene: &str) -> Vec<String> {
    vec![
        String::new(),
        format!("$ tools/call draw  asset={fg}   # LOOK-TEST"),
        format!("  scene: \"{scene}\""),
    ]
}

pub fn look_result_lines(look: &Value, secs: f64) -> Vec<String> {
    let receipt = look.get("fidelity_receipt");
    let get = |k: &str| receipt.and_then(|r| r.get(k));
    let mut lines = vec![
        format!("  muse-image-1.0 ... {secs:.1}s"),
        format!("  plate kind   : {}", py_str(look.get("plate_kind"))),
        format!("  settled cost : ${}", py_str(look.get("cost_usd"))),
        format!(
            "  look receipt : {} checked / {} changed",
            py_commas(get("opaque_pixels")),
            py_str(get("changed_opaque_pixels"))
        ),
    ];
    if look.get("status").and_then(|s| s.as_str()) != Some("ok") {
        lines.push(format!("  LOOK-TEST FAILED: {}", py_slice(&py_json_dumps(look), 300)));
    }
    lines
}

pub fn verify_call_lines(bin_file_name: &str) -> Vec<String> {
    vec![
        String::new(),
        format!(
            "$ {bin_file_name} verify source.png staged.png mask.png   # independent checker, same shipped build"
        ),
    ]
}

/// Verdict and printed line from the verify subcommand's stdout/stderr,
/// with the Python driver's PASS rule (ok and changed_pixels == 0).
pub fn verify_outcome(stdout: &str, stderr: &str) -> (String, String) {
    let raw = if stdout.is_empty() { stderr } else { stdout };
    let raw_line = format!("  verify raw: {}", py_slice(raw, 280));
    let Ok(Value::Object(rv)) = serde_json::from_str::<Value>(stdout) else {
        return ("UNPARSED".into(), raw_line);
    };
    let changed_is_zero = match rv.get("changed_pixels") {
        Some(Value::Number(n)) => n.as_f64() == Some(0.0),
        Some(Value::Bool(b)) => !*b,
        _ => false,
    };
    let verdict = if py_truthy(rv.get("ok")) && changed_is_zero { "PASS" } else { "CHECK OUTPUT" };
    let Some(checked) = py_commas_checked(rv.get("checked_pixels")) else {
        return (verdict.into(), raw_line);
    };
    let line = format!(
        "  verify {} : checked {} px, changed {} px, ok={}",
        verdict,
        checked,
        py_str(rv.get("changed_pixels")),
        py_str(rv.get("ok"))
    );
    (verdict.into(), line)
}

/// `sum((s.get("cost_usd") or 0) for s in steps)`: the float total and
/// the value Python would store (an int when no truthy float was added).
pub fn total_settled(steps: &[&Value]) -> (f64, Value) {
    let mut total = 0.0f64;
    let mut int_total: i64 = 0;
    let mut is_float = false;
    for step in steps {
        let cost = step.get("cost_usd");
        if !py_truthy(cost) {
            continue;
        }
        match cost {
            Some(Value::Number(n)) if n.as_i64().is_some() => {
                let i = n.as_i64().unwrap_or(0);
                int_total = int_total.saturating_add(i);
                total += i as f64;
            }
            Some(Value::Bool(true)) => {
                int_total = int_total.saturating_add(1);
                total += 1.0;
            }
            Some(Value::Number(n)) => {
                is_float = true;
                total += n.as_f64().unwrap_or(0.0);
            }
            _ => {}
        }
    }
    let stored = if is_float {
        Number::from_f64(total).map(Value::Number).unwrap_or(Value::Null)
    } else {
        Value::from(int_total)
    };
    (total, stored)
}

pub fn result_line(changed: Option<&Value>, total: f64) -> String {
    format!(
        "RESULT: {} product pixels changed | TOTAL SETTLED: ${:.6}",
        py_str(changed),
        total
    )
}

/// Copy of a tools/call response with image block data replaced by
/// `<stripped N chars>` (what the Python saves as `<tool>-response.json`).
pub fn strip_images(resp: &Value) -> Value {
    let mut saved = resp.clone();
    if let Some(content) = saved
        .get_mut("result")
        .and_then(|r| r.get_mut("content"))
        .and_then(|c| c.as_array_mut())
    {
        for block in content.iter_mut() {
            if let Some(map) = block.as_object_mut() {
                if map.get("type").and_then(|t| t.as_str()) == Some("image") {
                    let n = map
                        .get("data")
                        .map(|d| match d {
                            Value::String(s) => s.chars().count(),
                            Value::Array(a) => a.len(),
                            Value::Object(o) => o.len(),
                            _ => 0,
                        })
                        .unwrap_or(0);
                    map.insert("data".into(), Value::String(format!("<stripped {n} chars>")));
                }
            }
        }
    }
    saved
}

/// The serve-banner version: this build's crate version (the default
/// server binary is the sibling `agenthand` from the same build; the
/// initialize line that follows reports the server's own version).
pub fn build_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// ---------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------

pub struct Opts {
    pub src: PathBuf,
    pub out: PathBuf,
    pub bin: Option<PathBuf>,
    pub dry_run: bool,
    pub no_look_test: bool,
}

pub const USAGE: &str = "usage: agenthand-take [-h] [--src SRC] --out OUT [--bin BIN] [--dry-run] [--no-look-test]";

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

pub enum ArgsError {
    Help,
    Bad(String),
}

pub fn parse_args(args: &[String]) -> Result<Opts, ArgsError> {
    let mut src = home_dir().join("Downloads").join("IMG_1443.heic");
    let mut out: Option<PathBuf> = None;
    let mut bin: Option<PathBuf> = None;
    let mut dry_run = false;
    let mut no_look_test = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (arg, None),
        };
        let mut value = |name: &str| -> Result<String, ArgsError> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| ArgsError::Bad(format!("argument {name}: expected one argument")))
        };
        match flag {
            "-h" | "--help" => return Err(ArgsError::Help),
            "--src" => src = PathBuf::from(value("--src")?),
            "--out" => out = Some(PathBuf::from(value("--out")?)),
            "--bin" => bin = Some(PathBuf::from(value("--bin")?)),
            "--dry-run" => dry_run = true,
            "--no-look-test" => no_look_test = true,
            other => return Err(ArgsError::Bad(format!("unrecognized arguments: {other}"))),
        }
        i += 1;
    }
    let out = out.ok_or_else(|| ArgsError::Bad("the following arguments are required: --out".into()))?;
    Ok(Opts { src, out, bin, dry_run, no_look_test })
}

/// `--bin`, else the sibling `agenthand` next to this executable, else
/// `~/agenthand-release/agenthand-0.3.0-macos-arm64`.
pub fn resolve_bin(explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("agenthand");
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    home_dir().join(RELEASE_FALLBACK)
}

// ---------------------------------------------------------------------
// MCP stdio client
// ---------------------------------------------------------------------

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<Option<String>>,
    next_id: u64,
}

impl Server {
    fn spawn(bin: &Path, data_dir: &Path) -> Result<Server, String> {
        // META_API_KEY reaches the server through the inherited
        // environment only; this process never reads its value.
        let mut child = Command::new(bin)
            .arg("serve")
            .env("AGENTHAND_DATA_DIR", data_dir)
            .env_remove("AGENTHAND_DRAW_PLACEHOLDER_PLATE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("cannot start {} serve: {e}", bin.display()))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or("server stdout unavailable")?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut buf = Vec::new();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(n) if n > 0 && buf.last() == Some(&b'\n') => {
                        buf.pop();
                        let line = String::from_utf8_lossy(&buf).into_owned();
                        if tx.send(Some(line)).is_err() {
                            break;
                        }
                    }
                    _ => {
                        let _ = tx.send(None);
                        break;
                    }
                }
            }
        });
        Ok(Server { child, stdin, rx, next_id: 0 })
    }

    fn send(&mut self, msg: &PyJson) -> Result<(), String> {
        let mut line = py_json_dumps_ordered(msg, None);
        line.push('\n');
        let stdin = self.stdin.as_mut().ok_or("server stdin closed")?;
        stdin
            .write_all(line.as_bytes())
            .and_then(|_| stdin.flush())
            .map_err(|e| format!("cannot write to server: {e}"))
    }

    fn read_line(&mut self) -> Result<String, String> {
        match self.rx.recv_timeout(RESPONSE_TIMEOUT) {
            Ok(Some(line)) => Ok(line),
            Ok(None) | Err(RecvTimeoutError::Disconnected) => {
                let deadline = Instant::now() + Duration::from_secs(1);
                while Instant::now() < deadline {
                    if let Ok(Some(status)) = self.child.try_wait() {
                        return Err(format!("server closed stdout (exited with {status})"));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err("server closed stdout".into())
            }
            Err(RecvTimeoutError::Timeout) => Err("timed out waiting for server response".into()),
        }
    }

    fn call(&mut self, method: &str, params: PyJson) -> Result<Value, String> {
        self.next_id += 1;
        let req = obj(vec![
            ("jsonrpc", val(Value::from("2.0"))),
            ("id", val(Value::from(self.next_id))),
            ("method", val(Value::from(method))),
            ("params", params),
        ]);
        self.send(&req)?;
        let line = self.read_line()?;
        serde_json::from_str(&line)
            .map_err(|e| format!("{method}: server reply is not JSON ({e}): {}", py_slice(&line, 300)))
    }

    fn notify(&mut self, method: &str) -> Result<(), String> {
        let req = obj(vec![
            ("jsonrpc", val(Value::from("2.0"))),
            ("method", val(Value::from(method))),
        ]);
        self.send(&req)
    }

    /// tools/call; saves `<name>-response.json` (images stripped) and
    /// returns structuredContent, or an error when there is none.
    fn tool(&mut self, name: &str, arguments: PyJson, out: &Path) -> Result<Value, String> {
        let resp = self.call("tools/call", obj(vec![("name", val(Value::from(name))), ("arguments", arguments)]))?;
        let structured = resp
            .get("result")
            .and_then(|r| r.get("structuredContent"))
            .filter(|s| !s.is_null())
            .cloned();
        let saved = strip_images(&resp);
        write_file(&out.join(format!("{name}-response.json")), &py_json_dumps_indent(&saved))?;
        structured.ok_or_else(|| {
            format!(
                "{name} returned no structuredContent: {}",
                py_slice(&py_json_dumps(&resp), 600)
            )
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        self.stdin.take();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------

struct Transcript {
    file: File,
}

impl Transcript {
    fn open(path: &Path) -> Result<Transcript, String> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        Ok(Transcript { file })
    }

    /// print + append, line-flushed (the Python `emit`).
    fn emit(&mut self, text: &str) {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let _ = writeln!(lock, "{text}");
        let _ = lock.flush();
        let _ = writeln!(self.file, "{text}");
        let _ = self.file.flush();
    }

    fn emit_all(&mut self, lines: &[String]) {
        for line in lines {
            self.emit(line);
        }
    }
}

fn write_file(path: &Path, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn rename(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::rename(from, to)
        .map_err(|e| format!("cannot rename {} -> {}: {e}", from.display(), to.display()))
}

/// Captured stdout of a command, trimmed (`subprocess.run(...).stdout.strip()`).
fn command_stdout(cmd: &mut Command) -> String {
    cmd.output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn file_name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn str_field(v: &Value, key: &str, tool: &str) -> Result<String, String> {
    v.get(key)
        .and_then(|h| h.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("{tool} result has no {key:?}"))
}

fn run(opts: &Opts) -> Result<u8, String> {
    let bin = resolve_bin(opts.bin.as_deref());
    let bin_name = file_name_of(&bin);
    // Fail before anything prints, so a camera never rolls on a
    // half-transcript. The key's value is never read here.
    if !opts.dry_run
        && std::env::var("META_API_KEY").map(|k| k.trim().is_empty()).unwrap_or(true)
    {
        return Err("META_API_KEY is not set in the environment (export it by sourcing \
~/.config/agenthand/env: set -a; . ~/.config/agenthand/env; set +a)"
            .into());
    }
    if !bin.is_file() {
        return Err(format!("server binary not found: {} (pass --bin)", bin.display()));
    }

    let src = &opts.src;
    let out = &opts.out;
    std::fs::create_dir_all(out).map_err(|e| format!("cannot create {}: {e}", out.display()))?;
    let data_dir = out.join("run-data");
    std::fs::create_dir_all(&data_dir)
        .map_err(|e| format!("cannot create {}: {e}", data_dir.display()))?;
    let mut t = Transcript::open(&out.join("session-transcript.txt"))?;

    let src_name = file_name_of(src);
    let src_parent = match src.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };

    t.emit("$ date");
    t.emit(&command_stdout(&mut Command::new("date")));
    t.emit("");
    t.emit(&format!("$ ls -l {src_name}"));
    t.emit(&command_stdout(Command::new("ls").arg("-l").arg(&src_name).current_dir(&src_parent)));
    t.emit("");
    t.emit(&banner_line(&bin_name, build_version()));
    let mut srv = Server::spawn(&bin, &data_dir)?;

    let init = srv.call(
        "initialize",
        obj(vec![
            ("protocolVersion", val(Value::from("2024-11-05"))),
            ("capabilities", obj(vec![])),
            (
                "clientInfo",
                obj(vec![
                    ("name", val(Value::from(CLIENT_NAME))),
                    ("version", val(Value::from(CLIENT_VERSION))),
                ]),
            ),
        ]),
    )?;
    t.emit(&init_line(&init)?);
    srv.notify("notifications/initialized")?;
    let tools = srv.call("tools/list", obj(vec![]))?;
    t.emit(&tools_list_line(&tools)?);

    if opts.dry_run {
        t.emit("");
        t.emit("DRY RUN: initialize + tools/list only; stopped before identify (no Meta call, nothing billed)");
        return Ok(0);
    }

    // --- identify
    let src_bytes = std::fs::read(src).map_err(|e| format!("cannot read {}: {e}", src.display()))?;
    let data_uri = format!("data:image/heic;base64,{}", base64_encode(&src_bytes));
    let src_size = std::fs::metadata(src).map(|m| m.len()).unwrap_or(src_bytes.len() as u64);
    drop(src_bytes);
    t.emit_all(&identify_call_lines(&src_name, src_size));
    let started = Instant::now();
    let ident = srv.tool("identify", obj(vec![("image", val(Value::String(data_uri)))]), out)?;
    t.emit_all(&identify_result_lines(&ident, started.elapsed().as_secs_f64()));
    if ident.get("status").and_then(|s| s.as_str()) != Some("ok") {
        t.emit(&failed_line("identify", &ident));
        return Ok(1);
    }
    let src_handle = str_field(&ident, "image", "identify")?;

    // --- cut_out
    t.emit_all(&cut_call_lines(&src_handle));
    let started = Instant::now();
    let cut = srv.tool("cut_out", obj(vec![("image", val(Value::from(src_handle.as_str())))]), out)?;
    t.emit_all(&cut_result_lines(&cut, started.elapsed().as_secs_f64()));
    if cut.get("status").and_then(|s| s.as_str()) != Some("ok") {
        t.emit(&failed_line("cut_out", &cut));
        return Ok(1);
    }
    let fg = str_field(&cut, "output_asset", "cut_out")?;

    // --- draw (no scene text: merged-build defaults)
    t.emit_all(&draw_call_lines(&fg, SCENE));
    let started = Instant::now();
    let draw = srv.tool(
        "draw",
        obj(vec![
            ("asset", val(Value::from(fg.as_str()))),
            ("scene_prompt", val(Value::from(SCENE))),
        ]),
        out,
    )?;
    t.emit_all(&draw_result_lines(&draw, started.elapsed().as_secs_f64()));
    if draw.get("status").and_then(|s| s.as_str()) != Some("ok") {
        t.emit(&failed_line("draw", &draw));
        return Ok(1);
    }
    t.emit_all(&receipt_lines(&draw));

    // --- exports
    let assets = data_dir.join("assets");
    let export = |handle: &str, dest: &str| -> bool {
        let p = assets.join(format!("{}.png", hex_of(handle)));
        p.is_file() && std::fs::copy(&p, out.join(dest)).is_ok()
    };
    let opt_str = |v: &Value, k: &str| -> Option<String> {
        if py_truthy(v.get(k)) {
            v.get(k).and_then(|h| h.as_str()).map(str::to_string)
        } else {
            None
        }
    };
    export(&src_handle, "source.png");
    export(&fg, "foreground.png");
    if let Some(h) = opt_str(&cut, "mask_asset") {
        export(&h, "mask.png");
    }
    if let Some(h) = opt_str(&cut, "mask_overlay_asset") {
        export(&h, "mask-overlay.png");
    }
    for key in ["plate_asset", "background_asset", "background_plate_asset"] {
        if let Some(h) = opt_str(&draw, key) {
            export(&h, "plate.png");
        }
    }
    let staged = str_field(&draw, "output_asset", "draw")?;
    export(&staged, "staged.png");
    let rj = assets.join(format!("{}.receipt.json", hex_of(&staged)));
    if rj.is_file() {
        std::fs::copy(&rj, out.join("receipt.json"))
            .map_err(|e| format!("cannot copy receipt: {e}"))?;
    }
    if let Some(h) = opt_str(&draw, "qr_asset") {
        export(&h, "verify_qr.png");
    }
    let src_copy = out.join(&src_name);
    let same_file = match (std::fs::canonicalize(src), std::fs::canonicalize(&src_copy)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if !same_file {
        std::fs::copy(src, &src_copy).map_err(|e| format!("cannot copy source: {e}"))?;
    }

    // --- LOOK-TEST: same cutout, one extra draw with the logo-wall
    // scene. Look-test only, not a default change; its response file is
    // renamed so draw-response.json stays the record of the chain.
    let look = if opts.no_look_test {
        None
    } else {
        t.emit_all(&look_call_lines(&fg, LOOK_SCENE));
        let main_resp = out.join("draw-response.json");
        let main_tmp = out.join("draw-main-response.tmp");
        rename(&main_resp, &main_tmp)?;
        let started = Instant::now();
        let look = srv.tool(
            "draw",
            obj(vec![
                ("asset", val(Value::from(fg.as_str()))),
                ("scene_prompt", val(Value::from(LOOK_SCENE))),
            ]),
            out,
        )?;
        rename(&main_resp, &out.join("draw-look-test-response.json"))?;
        rename(&main_tmp, &main_resp)?;
        t.emit_all(&look_result_lines(&look, started.elapsed().as_secs_f64()));
        if look.get("status").and_then(|s| s.as_str()) == Some("ok") {
            if let Some(h) = look.get("output_asset").and_then(|h| h.as_str()) {
                export(h, "logo-look-test.png");
            }
        }
        Some(look)
    };

    // --- independent verify: the server binary's own subcommand
    t.emit_all(&verify_call_lines(&bin_name));
    let verify = Command::new(&bin)
        .arg("verify")
        .arg(out.join("source.png"))
        .arg(out.join("staged.png"))
        .arg(out.join("mask.png"))
        .output()
        .map_err(|e| format!("cannot run {} verify: {e}", bin.display()))?;
    let v_stdout = String::from_utf8_lossy(&verify.stdout).into_owned();
    let v_stderr = String::from_utf8_lossy(&verify.stderr).into_owned();
    write_file(
        &out.join("verify-output.json"),
        if v_stdout.is_empty() { &v_stderr } else { &v_stdout },
    )?;
    let (verdict, verify_line) = verify_outcome(&v_stdout, &v_stderr);
    t.emit(&verify_line);

    // --- result + summary
    let (total, total_value) = total_settled(&[&ident, &cut, &draw]);
    let changed = draw.get("fidelity_receipt").and_then(|r| r.get("changed_opaque_pixels"));
    t.emit("");
    t.emit(&result_line(changed, total));
    let machine = command_stdout(Command::new("uname").arg("-n"));
    let mut summary = vec![
        ("source_file", val(Value::from(src.display().to_string()))),
        ("machine", val(Value::from(machine))),
        ("binary", val(Value::from(bin.display().to_string()))),
        ("verify_verdict", val(Value::from(verdict))),
        (
            "steps",
            obj(vec![
                ("identify", val(ident.clone())),
                ("cut_out", val(cut.clone())),
                ("draw", val(draw.clone())),
            ]),
        ),
    ];
    if let Some(look) = &look {
        summary.push(("look_test", val(look.clone())));
        summary.push(("look_test_cost_usd", val(look.get("cost_usd").cloned().unwrap_or(Value::Null))));
    }
    summary.push(("total_cost_usd", val(total_value)));
    write_file(&out.join("chain-summary.json"), &py_json_dumps_ordered(&obj(summary), Some(2)))?;
    t.emit("");
    t.emit("$ date");
    t.emit(&command_stdout(&mut Command::new("date")));
    Ok(0)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match parse_args(&args) {
        Ok(o) => o,
        Err(ArgsError::Help) => {
            println!("{USAGE}");
            println!();
            println!("Filmed-demo MCP client: identify -> cut_out -> draw -> look-test draw -> verify.");
            println!("  --src SRC        photo to stage (default ~/Downloads/IMG_1443.heic)");
            println!("  --out OUT        output folder (required; created if missing)");
            println!("  --bin BIN        server binary (default: sibling `agenthand`, else ~/{RELEASE_FALLBACK})");
            println!("  --dry-run        initialize + tools/list only; stop before identify (no key needed)");
            println!("  --no-look-test   skip the extra logo-wall look-test draw");
            println!("META_API_KEY must be in the environment for a real take; it is never printed.");
            return ExitCode::SUCCESS;
        }
        Err(ArgsError::Bad(msg)) => {
            eprintln!("{USAGE}");
            eprintln!("agenthand-take: error: {msg}");
            return ExitCode::from(2);
        }
    };
    match run(&opts) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("agenthand-take: {e}");
            ExitCode::from(1)
        }
    }
}
