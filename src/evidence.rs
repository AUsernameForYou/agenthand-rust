//! `agenthand evidence` — the Proof Graph CLI.
//!
//! A read-only, offline reviewer interface over the evidence database
//! (SQLite, schema v2: `runs`, `calls`, `receipts`, `claims`, `meta`,
//! `evidence_map`, `video_chapters`, the `nodes` view and the derived
//! `edges` table). The database, its schema and its importer are
//! maintained outside this crate (evidence-layer `schema.sql`,
//! `import_inventory.py`); this module mirrors them and never writes:
//! the file is opened with `SQLITE_OPEN_READ_ONLY` and `query_only`.
//! Nothing here touches the network.
//!
//! `verify` re-runs the checks of the reference verifier
//! (`verify_db.py`) in the same order, with the same line format and
//! the same PASS / FAIL / REPORT / SKIP semantics, so the two can be
//! compared line by line. `root` recomputes the receipts root with the
//! importer's construction (see [`receipts_root`]).
//!
//! Exit codes: 0 = ok / OVERALL PASS, 1 = a FAIL (verify, root
//! mismatch, tamper precondition), 2 = usage or database error,
//! 3 = a planned verb that is not implemented.
//!
//! Database location (first match wins): `--db <path>`; the
//! `AGENTHAND_EVIDENCE_DB` environment variable; `evidence/evidence.db`
//! under the current directory; `evidence/evidence.db` beside the
//! binary or in any parent directory of the binary.
//!
//! Receipt base directory (relative `receipts.source_file` paths
//! resolve against it): `--base-dir <dir>`; `AGENTHAND_EVIDENCE_BASE`;
//! else the parent of the database's directory — the rule
//! `verify_db.py` uses (its ROOT is the parent of the directory that
//! holds `evidence.db`).

use rusqlite::types::ValueRef;
use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

/// stdout writers that ignore write errors (a closed pipe such as
/// `| head` ends the output instead of panicking).
macro_rules! outln {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

macro_rules! out {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = write!(std::io::stdout(), $($arg)*);
    }};
}

/// Environment variable naming the evidence database.
pub const ENV_DB: &str = "AGENTHAND_EVIDENCE_DB";
/// Environment variable naming the receipt base directory.
pub const ENV_BASE: &str = "AGENTHAND_EVIDENCE_BASE";

const TOL: f64 = 1e-6;
const EXIT_FAIL: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_PLANNED: u8 = 3;

const CLAIM_STATUSES: [&str; 4] = ["verified", "corrected", "withdrawn", "refused"];
const REQUIRED_RELATIONS: [&str; 9] = [
    "runs",
    "calls",
    "receipts",
    "claims",
    "meta",
    "evidence_map",
    "video_chapters",
    "edges",
    "nodes",
];

pub const HELP: &str = "\
agenthand evidence: the Proof Graph CLI (read-only, offline)

usage: agenthand evidence [--db <path>] [--base-dir <dir>] <verb> [args] [--json]

Implemented:
  list [--chapter <c>] [--status <s>]  claims with criterion, status and chapters
  show <claim-id>                      one claim: scope, runs, calls, receipts, chapters
  verify <claim-id> | --all            re-run the reference checks (verify_db.py) and
                                       print one line per check, then OVERALL
  verify-all                           same as verify --all
  trace <node-id>                      walk the derived edges from a node, as a tree
                                       (claim:N, chapter:N, receipt:N, run:N, call:N,
                                       model:X, build:X, criterion:X; a bare N is claim:N)
  chapter <time | id | ChN>            the film chapter at a timestamp (1:42, 102.5),
                                       traced to its claim, run and receipt
  root                                 recompute the receipts root from the receipt
                                       sha256 rows and compare it with meta
  graph [--format mermaid|dot]         the whole graph (default mermaid)
  rubric                               claims grouped by rubric criterion
  calls [--by-model] [--run <id>]      model-call rows, or per-model counts and sums
  tamper <receipt-id>                  flip one byte of a receipt file in memory and
                                       show its sha256 no longer matches (no writes)

Planned, not implemented:
  parity                               cross-build parity node (no parity rows in the
                                       database yet)
  export --dir <dir>                   replay pack export

Chapter selectors (<c>): a chapter id (7), chapter:7, a timestamp (1:42, 0:01:42,
102.5, 102.5s), or the narration label token (Ch5, ChClose).

Options:
  --db <path>       evidence database, opened read-only. Default: $AGENTHAND_EVIDENCE_DB,
                    else ./evidence/evidence.db, else evidence/evidence.db beside the
                    binary or in any parent directory of the binary
  --base-dir <dir>  base for relative receipt paths. Default: $AGENTHAND_EVIDENCE_BASE,
                    else the parent of the database's directory (verify_db.py's rule)
  --json            machine-readable output on stdout

Exit codes: 0 ok / OVERALL PASS; 1 FAIL; 2 usage or database error; 3 planned verb.
";

const PLANNED: [(&str, &str); 2] = [
    (
        "parity",
        "cross-build parity node; the database has no parity rows yet",
    ),
    ("export", "replay pack export (evidence export --dir <dir>)"),
];

// ---------------------------------------------------------------------------
// Entry point and argument parsing
// ---------------------------------------------------------------------------

struct CliError {
    code: u8,
    msg: String,
}

impl CliError {
    fn usage(msg: impl Into<String>) -> Self {
        CliError {
            code: EXIT_USAGE,
            msg: msg.into(),
        }
    }
}

impl From<rusqlite::Error> for CliError {
    fn from(e: rusqlite::Error) -> Self {
        CliError::usage(format!("database error: {e}"))
    }
}

type CliResult<T> = Result<T, CliError>;

#[derive(Default)]
struct Opts {
    db: Option<String>,
    base_dir: Option<String>,
    json: bool,
    all: bool,
    by_model: bool,
    chapter: Option<String>,
    status: Option<String>,
    format: Option<String>,
    run: Option<String>,
    dir: Option<String>,
    help: bool,
    used: Vec<&'static str>,
    positional: Vec<String>,
}

fn parse_opts(args: &[String]) -> CliResult<Opts> {
    let mut o = Opts::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_string())),
            _ => (arg.as_str(), None),
        };
        let mut value = |flag: &str| -> CliResult<String> {
            match inline.clone() {
                Some(v) => Ok(v),
                None => it
                    .next()
                    .cloned()
                    .ok_or_else(|| CliError::usage(format!("{flag} needs a value"))),
            }
        };
        match name {
            "--db" => {
                o.db = Some(value("--db")?);
                o.used.push("--db");
            }
            "--base-dir" => {
                o.base_dir = Some(value("--base-dir")?);
                o.used.push("--base-dir");
            }
            "--chapter" => {
                o.chapter = Some(value("--chapter")?);
                o.used.push("--chapter");
            }
            "--status" => {
                o.status = Some(value("--status")?);
                o.used.push("--status");
            }
            "--format" => {
                o.format = Some(value("--format")?);
                o.used.push("--format");
            }
            "--run" => {
                o.run = Some(value("--run")?);
                o.used.push("--run");
            }
            "--dir" => {
                o.dir = Some(value("--dir")?);
                o.used.push("--dir");
            }
            "--json" => {
                o.json = true;
                o.used.push("--json");
            }
            "--all" => {
                o.all = true;
                o.used.push("--all");
            }
            "--by-model" => {
                o.by_model = true;
                o.used.push("--by-model");
            }
            "-h" | "--help" => o.help = true,
            a if a.starts_with("--") => {
                return Err(CliError::usage(format!("unknown option {a:?}")));
            }
            _ => o.positional.push(arg.clone()),
        }
    }
    Ok(o)
}

/// Refuse options that do not apply to the verb instead of ignoring them.
fn allow(o: &Opts, verb: &str, allowed: &[&str]) -> CliResult<()> {
    const GLOBAL: [&str; 3] = ["--db", "--base-dir", "--json"];
    for flag in &o.used {
        if !GLOBAL.contains(flag) && !allowed.contains(flag) {
            return Err(CliError::usage(format!(
                "option {flag} does not apply to `evidence {verb}`"
            )));
        }
    }
    Ok(())
}

fn positional_count(o: &Opts, verb: &str, n: usize, what: &str) -> CliResult<()> {
    // positional[0] is the verb itself.
    if o.positional.len() != n + 1 {
        return Err(CliError::usage(format!("`evidence {verb}` takes {what}")));
    }
    Ok(())
}

/// `agenthand evidence ...` — `args` are the arguments after `evidence`.
pub fn run(args: &[String]) -> ExitCode {
    match run_inner(args) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("agenthand evidence: {}", e.msg);
            if e.code == EXIT_USAGE && !e.msg.starts_with("database") {
                eprintln!("(agenthand evidence --help lists the verbs and options)");
            }
            ExitCode::from(e.code)
        }
    }
}

fn run_inner(args: &[String]) -> CliResult<u8> {
    let o = parse_opts(args)?;
    let verb = o.positional.first().map(|s| s.as_str());
    if o.help || verb == Some("help") {
        out!("{HELP}");
        return Ok(0);
    }
    let Some(verb) = verb else {
        out!("{HELP}");
        return Ok(EXIT_USAGE);
    };
    if let Some((name, what)) = PLANNED.iter().find(|(n, _)| *n == verb) {
        if o.json {
            outln!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "verb": name,
                    "status": "planned, not implemented",
                    "detail": what,
                }))
                .unwrap_or_default()
            );
        } else {
            outln!("evidence {name}: planned, not implemented ({what})");
        }
        return Ok(EXIT_PLANNED);
    }
    match verb {
        "list" => {
            allow(&o, verb, &["--chapter", "--status"])?;
            positional_count(&o, verb, 0, "no positional arguments")?;
            cmd_list(&open(&o)?, &o)
        }
        "show" => {
            allow(&o, verb, &[])?;
            positional_count(&o, verb, 1, "one claim id (N or claim:N)")?;
            cmd_show(&open(&o)?, &o)
        }
        "verify" | "verify-all" => {
            allow(&o, verb, &["--all"])?;
            let scope = if verb == "verify-all" {
                positional_count(&o, verb, 0, "no positional arguments")?;
                Scope::All
            } else if o.all {
                positional_count(&o, verb, 0, "either a claim id or --all, not both")?;
                Scope::All
            } else {
                positional_count(&o, verb, 1, "a claim id (N or claim:N) or --all")?;
                Scope::Claim(parse_prefixed_id(&o.positional[1], "claim")?)
            };
            cmd_verify(&open(&o)?, &o, scope)
        }
        "trace" => {
            allow(&o, verb, &[])?;
            positional_count(
                &o,
                verb,
                1,
                "one node id (claim:N, chapter:N, receipt:N, ...)",
            )?;
            cmd_trace(&open(&o)?, &o)
        }
        "chapter" => {
            allow(&o, verb, &[])?;
            positional_count(
                &o,
                verb,
                1,
                "one chapter selector (1:42, 102.5, 7, chapter:7, Ch5)",
            )?;
            cmd_chapter(&open(&o)?, &o)
        }
        "root" => {
            allow(&o, verb, &[])?;
            positional_count(&o, verb, 0, "no positional arguments")?;
            cmd_root(&open(&o)?, &o)
        }
        "graph" => {
            allow(&o, verb, &["--format"])?;
            positional_count(&o, verb, 0, "no positional arguments")?;
            cmd_graph(&open(&o)?, &o)
        }
        "rubric" => {
            allow(&o, verb, &[])?;
            positional_count(&o, verb, 0, "no positional arguments")?;
            cmd_rubric(&open(&o)?, &o)
        }
        "calls" => {
            allow(&o, verb, &["--by-model", "--run"])?;
            positional_count(&o, verb, 0, "no positional arguments")?;
            cmd_calls(&open(&o)?, &o)
        }
        "tamper" => {
            allow(&o, verb, &[])?;
            positional_count(&o, verb, 1, "one receipt id (N or receipt:N)")?;
            cmd_tamper(&open(&o)?, &o)
        }
        other => Err(CliError::usage(format!("unknown verb {other:?}"))),
    }
}

// ---------------------------------------------------------------------------
// Database location and opening
// ---------------------------------------------------------------------------

struct Ctx {
    conn: Connection,
    db_path: PathBuf,
    base_dir: PathBuf,
    base_rule: &'static str,
}

/// Lexical absolute path (Python's `os.path.abspath`: no symlink resolution).
fn abspath(p: &Path) -> PathBuf {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(p)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The evidence database path, by the documented search order.
pub fn resolve_db_path(cli: Option<&str>) -> Result<PathBuf, String> {
    if let Some(p) = cli {
        let p = PathBuf::from(p);
        return if p.is_file() {
            Ok(p)
        } else {
            Err(format!(
                "evidence database not found: {} (from --db)",
                p.display()
            ))
        };
    }
    if let Ok(env) = std::env::var(ENV_DB) {
        if !env.trim().is_empty() {
            let p = PathBuf::from(env);
            return if p.is_file() {
                Ok(p)
            } else {
                Err(format!(
                    "evidence database not found: {} (from {ENV_DB})",
                    p.display()
                ))
            };
        }
    }
    let mut tried = Vec::new();
    let cwd_candidate = PathBuf::from("evidence").join("evidence.db");
    if cwd_candidate.is_file() {
        return Ok(cwd_candidate);
    }
    tried.push(abspath(&cwd_candidate));
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(Path::to_path_buf);
        while let Some(d) = dir {
            let candidate = d.join("evidence").join("evidence.db");
            if candidate.is_file() {
                return Ok(candidate);
            }
            tried.push(candidate);
            dir = d.parent().map(Path::to_path_buf);
        }
    }
    Err(format!(
        "no evidence database found. Pass --db <path> or set {ENV_DB}. Looked for: {}",
        tried
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn open(o: &Opts) -> CliResult<Ctx> {
    let db_path = resolve_db_path(o.db.as_deref()).map_err(CliError::usage)?;
    let conn = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| {
        CliError::usage(format!(
            "database error: cannot open {} read-only: {e}",
            db_path.display()
        ))
    })?;
    conn.execute_batch("PRAGMA query_only = ON;")?;
    let present: HashSet<String> = {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type IN ('table', 'view')")
            .map_err(|e| {
                CliError::usage(format!(
                    "database error: {} is not a readable SQLite database: {e}",
                    db_path.display()
                ))
            })?;
        let names = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        names
    };
    let missing: Vec<&str> = REQUIRED_RELATIONS
        .iter()
        .copied()
        .filter(|t| !present.contains(*t))
        .collect();
    if !missing.is_empty() {
        return Err(CliError::usage(format!(
            "database error: {} lacks schema v2 table(s)/view(s): {}",
            db_path.display(),
            missing.join(", ")
        )));
    }
    let (base_dir, base_rule) = if let Some(b) = &o.base_dir {
        (abspath(Path::new(b)), "--base-dir")
    } else if let Some(b) = std::env::var(ENV_BASE)
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        (abspath(Path::new(&b)), ENV_BASE)
    } else {
        let abs = abspath(&db_path);
        let db_dir = abs
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/"));
        let base = db_dir.parent().map(Path::to_path_buf).unwrap_or(db_dir);
        (base, "parent of the database's directory")
    };
    Ok(Ctx {
        conn,
        db_path,
        base_dir,
        base_rule,
    })
}

// ---------------------------------------------------------------------------
// Python-compatible value helpers (verify_db.py parity)
// ---------------------------------------------------------------------------

/// Python `str.isspace()` for one char: Unicode White_Space plus the
/// information separators U+001C..U+001F.
fn py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

fn py_blank(s: &str) -> bool {
    s.chars().all(py_space)
}

/// Python `str(v)` of a sqlite3 value (None for NULL).
fn py_str(v: ValueRef<'_>) -> String {
    match v {
        ValueRef::Null => "None".to_string(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => format!("{f:?}"),
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
        ValueRef::Blob(b) => format!("b'<{} bytes>'", b.len()),
    }
}

/// Python `bool(v and str(v).strip())`.
fn py_truthy_nonblank(v: ValueRef<'_>) -> bool {
    match v {
        ValueRef::Null => false,
        ValueRef::Integer(i) => i != 0,
        ValueRef::Real(f) => f != 0.0,
        ValueRef::Text(t) => !py_blank(&String::from_utf8_lossy(t)),
        ValueRef::Blob(b) => !b.is_empty(),
    }
}

/// Text value or None (NULL); numbers rendered as text.
fn opt_text(v: ValueRef<'_>) -> Option<String> {
    match v {
        ValueRef::Null => None,
        other => Some(py_str(other)),
    }
}

fn opt_f64(v: ValueRef<'_>) -> Option<f64> {
    match v {
        ValueRef::Integer(i) => Some(i as f64),
        ValueRef::Real(f) => Some(f),
        _ => None,
    }
}

fn opt_i64(v: ValueRef<'_>) -> Option<i64> {
    match v {
        ValueRef::Integer(i) => Some(i),
        _ => None,
    }
}

fn prefix_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The receipts root, exactly as `import_inventory.py` builds it and
/// `verify_db.py` re-checks it: sort the receipt sha256 hex digests
/// ascending (string order), concatenate them with no separator, and
/// take the sha256 hex digest of the UTF-8 bytes. One flat hash over
/// the sorted list: no per-leaf hashing, no pairing, no odd-node rule.
/// Duplicates are kept, as in the reference.
pub fn receipts_root(digests: &[String]) -> String {
    let mut sorted: Vec<&str> = digests.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let mut h = Sha256::new();
    for d in sorted {
        h.update(d.as_bytes());
    }
    hex(&h.finalize())
}

fn rooted_digests(conn: &Connection) -> CliResult<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT file_sha256 FROM receipts WHERE file_sha256 IS NOT NULL")?;
    let rows = stmt
        .query_map([], |r| Ok(py_str(r.get_ref(0)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn meta_map(conn: &Connection) -> CliResult<HashMap<String, String>> {
    let mut stmt = conn.prepare("SELECT key, value FROM meta")?;
    let rows = stmt
        .query_map([], |r| Ok((py_str(r.get_ref(0)?), py_str(r.get_ref(1)?))))?
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(rows)
}

fn resolve_receipt_path(base: &Path, source_file: &str) -> PathBuf {
    let p = Path::new(source_file);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

fn parse_prefixed_id(s: &str, prefix: &str) -> CliResult<i64> {
    let body = s.strip_prefix(&format!("{prefix}:")).unwrap_or(s);
    body.parse::<i64>().map_err(|_| {
        CliError::usage(format!(
            "{s:?} is not a {prefix} id (expected N or {prefix}:N)"
        ))
    })
}

fn print_json(v: &Value) {
    outln!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

fn fmt_t(s: f64) -> String {
    let ms = (s * 1000.0).round() as i64;
    let (m, rem) = (ms / 60_000, ms % 60_000);
    format!("{}:{:02}.{:03}", m, rem / 1000, rem % 1000)
}

fn null_or<T: ToString>(v: Option<T>) -> String {
    v.map(|x| x.to_string())
        .unwrap_or_else(|| "(null)".to_string())
}

// ---------------------------------------------------------------------------
// verify — verify_db.py parity
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Scope {
    All,
    Claim(i64),
}

enum Item {
    Header(String),
    Check {
        section: char,
        status: &'static str,
        msg: String,
    },
    Summary(String),
}

struct Verify {
    items: Vec<Item>,
    section: char,
    failures: usize,
}

impl Verify {
    fn header(&mut self, section: char, text: &str) {
        self.section = section;
        self.items.push(Item::Header(text.to_string()));
    }
    fn line(&mut self, status: &'static str, msg: String) {
        if status == "FAIL" {
            self.failures += 1;
        }
        self.items.push(Item::Check {
            section: self.section,
            status,
            msg,
        });
    }
    fn summary(&mut self, text: String) {
        self.items.push(Item::Summary(text));
    }
}

/// Nodes reached by the directed walk from `claim:<id>` (verify_db.py's CTE).
fn claim_walk(conn: &Connection, claim_id: i64) -> CliResult<Vec<String>> {
    let mut stmt = conn.prepare(
        "WITH RECURSIVE walk(node) AS (
             SELECT 'claim:' || ?1
             UNION
             SELECT e.to_node FROM edges e JOIN walk w ON e.from_node = w.node)
         SELECT node FROM walk",
    )?;
    let nodes = stmt
        .query_map(params![claim_id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(nodes)
}

fn ids_with_prefix(nodes: &[String], prefix: &str) -> HashSet<i64> {
    nodes
        .iter()
        .filter_map(|n| n.strip_prefix(prefix))
        .filter_map(|s| s.parse::<i64>().ok())
        .collect()
}

fn run_verify(ctx: &Ctx, scope: Scope) -> CliResult<Verify> {
    let conn = &ctx.conn;
    let mut v = Verify {
        items: Vec::new(),
        section: 'a',
        failures: 0,
    };
    let (scope_receipts, scope_runs) = match scope {
        Scope::All => (None, None),
        Scope::Claim(id) => {
            let exists: i64 = conn.query_row(
                "SELECT COUNT(*) FROM claims WHERE claim_id = ?1",
                params![id],
                |r| r.get(0),
            )?;
            if exists == 0 {
                return Err(CliError::usage(format!("no claim with id {id}")));
            }
            let walk = claim_walk(conn, id)?;
            (
                Some(ids_with_prefix(&walk, "receipt:")),
                Some(ids_with_prefix(&walk, "run:")),
            )
        }
    };
    let in_receipts = |id: i64| scope_receipts.as_ref().map_or(true, |s| s.contains(&id));
    let in_runs = |id: i64| scope_runs.as_ref().map_or(true, |s| s.contains(&id));

    // --- (a) receipt hashes ---------------------------------------------
    v.header('a', "== (a) receipt file hashes ==");
    let (mut n_pass, mut n_skip) = (0usize, 0usize);
    {
        let mut stmt = conn.prepare(
            "SELECT receipt_id, run_id, file_sha256, source_file FROM receipts \
             WHERE file_sha256 IS NOT NULL ORDER BY receipt_id",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let receipt_id = py_str(r.get_ref(0)?);
            if let Some(id) = opt_i64(r.get_ref(0)?) {
                if !in_receipts(id) {
                    continue;
                }
            }
            let run_id = py_str(r.get_ref(1)?);
            let db_sha = py_str(r.get_ref(2)?);
            let sf_ref = r.get_ref(3)?;
            let sf_disp = py_str(sf_ref);
            let sf = opt_text(sf_ref).filter(|s| !s.is_empty());
            let path = sf
                .as_deref()
                .map(|s| resolve_receipt_path(&ctx.base_dir, s));
            match path {
                Some(p) if p.is_file() => match sha256_file(&p) {
                    Ok(actual) if actual == db_sha => {
                        v.line(
                            "PASS",
                            format!(
                                "receipt {receipt_id} (run {run_id}): sha256 matches {sf_disp}"
                            ),
                        );
                        n_pass += 1;
                    }
                    Ok(actual) => v.line(
                        "FAIL",
                        format!(
                            "receipt {receipt_id} (run {run_id}): sha256 MISMATCH for {sf_disp} \
                             (db {}... disk {}...)",
                            prefix_chars(&db_sha, 16),
                            prefix_chars(&actual, 16)
                        ),
                    ),
                    Err(e) => v.line(
                        "FAIL",
                        format!("receipt {receipt_id} (run {run_id}): cannot read {sf_disp}: {e}"),
                    ),
                },
                _ => {
                    v.line(
                        "SKIP",
                        format!(
                            "receipt {receipt_id} (run {run_id}): file not present locally: {sf_disp}"
                        ),
                    );
                    n_skip += 1;
                }
            }
        }
    }
    let n_nohash: i64 = match &scope_receipts {
        None => conn.query_row(
            "SELECT COUNT(*) FROM receipts WHERE file_sha256 IS NULL",
            [],
            |r| r.get(0),
        )?,
        Some(set) => {
            let mut stmt =
                conn.prepare("SELECT receipt_id FROM receipts WHERE file_sha256 IS NULL")?;
            let ids = stmt
                .query_map([], |r| Ok(opt_i64(r.get_ref(0)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            ids.into_iter()
                .flatten()
                .filter(|id| set.contains(id))
                .count() as i64
        }
    };
    v.summary(format!(
        "  -> {n_pass} matched, {n_skip} not-on-disk, {n_nohash} receipts carry no hash"
    ));

    // --- (b) run totals vs call sums --------------------------------------
    v.header('b', "== (b) run totals vs sum of call costs ==");
    let (mut n_match, mut n_report) = (0usize, 0usize);
    {
        let mut stmt = conn.prepare(
            "SELECT run_id, label, total_settled_usd, notes FROM runs \
             WHERE total_settled_usd IS NOT NULL ORDER BY run_id",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let run_ref = r.get_ref(0)?;
            let run_id_disp = py_str(run_ref);
            if let Some(id) = opt_i64(run_ref) {
                if !in_runs(id) {
                    continue;
                }
            }
            let label = prefix_chars(&py_str(r.get_ref(1)?), 52);
            let total = opt_f64(r.get_ref(2)?).unwrap_or(f64::NAN);
            let notes_blank = match r.get_ref(3)? {
                ValueRef::Null => true,
                other => !py_truthy_nonblank(other),
            };
            let (n_costed, total_calls): (i64, f64) = conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(settled_usd), 0) FROM calls \
                 WHERE run_id = ? AND settled_usd IS NOT NULL",
                params![opt_i64(run_ref)],
                |c| Ok((c.get(0)?, opt_f64(c.get_ref(1)?).unwrap_or(0.0))),
            )?;
            if n_costed == 0 {
                v.line(
                    "REPORT",
                    format!(
                        "run {run_id_disp} ({label}): total ${total:.6} on record, \
                         no costed calls to check it against"
                    ),
                );
                n_report += 1;
                continue;
            }
            if (total_calls - total).abs() <= TOL {
                v.line(
                    "PASS",
                    format!("run {run_id_disp} ({label}): total ${total:.6} == calls sum"),
                );
                n_match += 1;
                continue;
            }
            if !notes_blank {
                v.line(
                    "REPORT",
                    format!(
                        "run {run_id_disp} ({label}): total ${total:.6} != costed-calls sum \
                         ${total_calls:.6} \u{2014} difference is explained in the run's notes/flags"
                    ),
                );
                n_report += 1;
            } else {
                v.line(
                    "FAIL",
                    format!(
                        "run {run_id_disp} ({label}): total ${total:.6} != calls sum \
                         ${total_calls:.6} with no explanation on record"
                    ),
                );
            }
        }
    }
    v.summary(format!(
        "  -> {n_match} matched, {n_report} reported-with-explanation"
    ));

    // --- (c) graph connectivity --------------------------------------------
    v.header('c', "== (c) graph connectivity ==");
    if scope == Scope::All {
        let orphan_calls: i64 = conn.query_row(
            "SELECT COUNT(*) FROM calls c LEFT JOIN runs r ON r.run_id = c.run_id \
             WHERE r.run_id IS NULL",
            [],
            |r| r.get(0),
        )?;
        let orphan_receipts: i64 = conn.query_row(
            "SELECT COUNT(*) FROM receipts x LEFT JOIN runs r ON r.run_id = x.run_id \
             WHERE r.run_id IS NULL",
            [],
            |r| r.get(0),
        )?;
        if orphan_calls == 0 && orphan_receipts == 0 {
            v.line(
                "PASS",
                "no orphan calls or receipts (every row joins to a run)".to_string(),
            );
        } else {
            v.line(
                "FAIL",
                format!("orphans: {orphan_calls} calls, {orphan_receipts} receipts join to no run"),
            );
        }
        let edgeless_receipts: i64 = conn.query_row(
            "SELECT COUNT(*) FROM receipts x WHERE NOT EXISTS (\
               SELECT 1 FROM edges e WHERE e.relation = 'has_receipt' \
               AND e.from_node = 'run:' || x.run_id AND e.to_node = 'receipt:' || x.receipt_id)",
            [],
            |r| r.get(0),
        )?;
        let edgeless_calls: i64 = conn.query_row(
            "SELECT COUNT(*) FROM calls c WHERE NOT EXISTS (\
               SELECT 1 FROM edges e WHERE e.relation = 'has_call' \
               AND e.from_node = 'run:' || c.run_id AND e.to_node = 'call:' || c.call_id)",
            [],
            |r| r.get(0),
        )?;
        if edgeless_receipts == 0 && edgeless_calls == 0 {
            let n_r: i64 = conn.query_row("SELECT COUNT(*) FROM receipts", [], |r| r.get(0))?;
            let n_c: i64 = conn.query_row("SELECT COUNT(*) FROM calls", [], |r| r.get(0))?;
            v.line(
                "PASS",
                format!(
                    "every receipt ({n_r}) and every call ({n_c}) has its run edge in the graph"
                ),
            );
        } else {
            v.line(
                "FAIL",
                format!("missing run edges: {edgeless_receipts} receipts, {edgeless_calls} calls"),
            );
        }
    }
    {
        let mut stmt = conn.prepare("SELECT claim_id, status FROM claims ORDER BY claim_id")?;
        let claims = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, py_str(r.get_ref(1)?))))?
            .collect::<Result<Vec<_>, _>>()?;
        for (claim_id, status) in claims {
            if let Scope::Claim(id) = scope {
                if id != claim_id {
                    continue;
                }
            }
            let reached: i64 = conn.query_row(
                "WITH RECURSIVE walk(node) AS (
                     SELECT 'claim:' || ?
                     UNION
                     SELECT e.to_node FROM edges e JOIN walk w ON e.from_node = w.node)
                 SELECT COUNT(*) FROM walk WHERE node LIKE 'receipt:%'",
                params![claim_id],
                |r| r.get(0),
            )?;
            if reached > 0 {
                v.line(
                    "PASS",
                    format!("claim {claim_id}: directed walk reaches {reached} receipt(s)"),
                );
            } else if status == "withdrawn" || status == "refused" {
                v.line(
                    "PASS",
                    format!(
                        "claim {claim_id} ({status}): no receipt by design; \
                         directed walk terminates at its rubric criterion"
                    ),
                );
            } else {
                v.line(
                    "FAIL",
                    format!("claim {claim_id}: directed walk reaches no receipt"),
                );
            }
        }
    }

    // --- (d) receipts root --------------------------------------------------
    v.header('d', "== (d) receipts root ==");
    let meta = meta_map(conn)?;
    let digests = rooted_digests(conn)?;
    let computed = receipts_root(&digests);
    let stored_root = meta.get("receipts_root");
    let stored_count = meta.get("receipt_count_rooted");
    if stored_root == Some(&computed) && stored_count == Some(&digests.len().to_string()) {
        v.line(
            "PASS",
            format!(
                "receipts_root recomputes to {computed} ({} rooted receipts)",
                digests.len()
            ),
        );
    } else {
        v.line(
            "FAIL",
            format!(
                "receipts_root mismatch: stored {} / count {}; computed {computed} / count {}",
                stored_root.map_or("None", String::as_str),
                stored_count.map_or("None", String::as_str),
                digests.len()
            ),
        );
    }

    // --- (e) claim scope ------------------------------------------------------
    v.header('e', "== (e) claim scope ==");
    {
        let mut stmt = conn.prepare(
            "SELECT claim_id, status, scope_photo, scope_build, scope_date, reason \
             FROM claims ORDER BY claim_id",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let claim_ref = r.get_ref(0)?;
            if let Scope::Claim(id) = scope {
                if opt_i64(claim_ref) != Some(id) {
                    continue;
                }
            }
            let claim_id = py_str(claim_ref);
            let status = py_str(r.get_ref(1)?);
            let mut missing = Vec::new();
            for (i, name) in [(2, "scope_photo"), (3, "scope_build"), (4, "scope_date")] {
                if !py_truthy_nonblank(r.get_ref(i)?) {
                    missing.push(name);
                }
            }
            let has_reason = py_truthy_nonblank(r.get_ref(5)?);
            let reason_required = status == "withdrawn" || status == "refused";
            if missing.is_empty() && (has_reason || !reason_required) {
                v.line(
                    "PASS",
                    format!(
                        "claim {claim_id} ({status}): scope fields complete{}",
                        if has_reason { "; reason present" } else { "" }
                    ),
                );
            } else if !missing.is_empty() && has_reason {
                v.line(
                    "PASS",
                    format!(
                        "claim {claim_id} ({status}): missing {}, with an explicit reason on record",
                        missing.join(", ")
                    ),
                );
            } else {
                let mut problems = Vec::new();
                if !missing.is_empty() {
                    problems.push(format!(
                        "missing {} without a complete reason",
                        missing.join(", ")
                    ));
                }
                if reason_required && !has_reason {
                    problems.push("withdrawn/refused claim has no reason".to_string());
                }
                v.line(
                    "FAIL",
                    format!("claim {claim_id} ({status}): {}", problems.join("; ")),
                );
            }
        }
    }
    Ok(v)
}

fn cmd_verify(ctx: &Ctx, o: &Opts, scope: Scope) -> CliResult<u8> {
    let v = run_verify(ctx, scope)?;
    let overall = if v.failures > 0 { "FAIL" } else { "PASS" };
    let scope_label = match scope {
        Scope::All => "all".to_string(),
        Scope::Claim(id) => format!("claim:{id}"),
    };
    if o.json {
        let items: Vec<Value> = v
            .items
            .iter()
            .map(|it| match it {
                Item::Header(t) => json!({"kind": "header", "text": t}),
                Item::Summary(t) => json!({"kind": "summary", "text": t.trim_start()}),
                Item::Check {
                    section,
                    status,
                    msg,
                } => json!({
                    "kind": "check",
                    "section": section.to_string(),
                    "status": status,
                    "message": msg,
                }),
            })
            .collect();
        print_json(&json!({
            "db": ctx.db_path.display().to_string(),
            "receipt_base_dir": ctx.base_dir.display().to_string(),
            "receipt_base_rule": ctx.base_rule,
            "scope": scope_label,
            "items": items,
            "failures": v.failures,
            "overall": overall,
        }));
    } else {
        outln!("# db: {}", ctx.db_path.display());
        outln!(
            "# receipt base dir: {} ({})",
            ctx.base_dir.display(),
            ctx.base_rule
        );
        if let Scope::Claim(id) = scope {
            outln!("# scope: claim:{id} (checks limited to the runs and receipts its directed walk reaches; (d) is global)");
        }
        for it in &v.items {
            match it {
                Item::Header(t) | Item::Summary(t) => outln!("{t}"),
                Item::Check { status, msg, .. } => outln!("{status:<6} {msg}"),
            }
        }
        outln!("OVERALL: {overall} ({} failing line(s))", v.failures);
    }
    Ok(if v.failures > 0 { EXIT_FAIL } else { 0 })
}

// ---------------------------------------------------------------------------
// root
// ---------------------------------------------------------------------------

fn cmd_root(ctx: &Ctx, o: &Opts) -> CliResult<u8> {
    let digests = rooted_digests(&ctx.conn)?;
    let computed = receipts_root(&digests);
    let meta = meta_map(&ctx.conn)?;
    let stored_root = meta.get("receipts_root");
    let stored_count = meta.get("receipt_count_rooted");
    let n_receipts: i64 = ctx
        .conn
        .query_row("SELECT COUNT(*) FROM receipts", [], |r| r.get(0))?;
    let root_state = match stored_root {
        None => "not stored",
        Some(s) if *s == computed => "matches",
        Some(_) => "differs",
    };
    let count_state = match stored_count {
        None => "not stored",
        Some(s) if *s == digests.len().to_string() => "matches",
        Some(_) => "differs",
    };
    let fail = root_state == "differs" || count_state == "differs";
    if o.json {
        let mut out = json!({
            "receipts_root": computed,
            "rooted_receipts": digests.len(),
            "receipts_total": n_receipts,
            "construction": "sha256(concat(sorted(receipts.file_sha256 hex digests, non-NULL)))",
            "stored_receipts_root": root_state,
            "stored_receipt_count_rooted": count_state,
        });
        if root_state == "differs" {
            out["stored_receipts_root_value"] = json!(stored_root);
        }
        if count_state == "differs" {
            out["stored_receipt_count_rooted_value"] = json!(stored_count);
        }
        print_json(&out);
    } else {
        outln!("receipts root: {computed}");
        outln!(
            "recomputed from {} receipt sha256 digest(s) ({} receipt rows; {} carry no sha256)",
            digests.len(),
            n_receipts,
            n_receipts - digests.len() as i64
        );
        outln!(
            "construction: sha256 of the sorted hex digests concatenated with no separator \
             (import_inventory.py / schema.sql)"
        );
        match root_state {
            "differs" => outln!(
                "meta.receipts_root: differs from the recomputed root (stored value {})",
                stored_root.map_or("", String::as_str)
            ),
            s => outln!("meta.receipts_root: {s}"),
        }
        match count_state {
            "differs" => outln!(
                "meta.receipt_count_rooted: differs (stored {}, recomputed {})",
                stored_count.map_or("", String::as_str),
                digests.len()
            ),
            "matches" => outln!("meta.receipt_count_rooted: matches ({})", digests.len()),
            s => outln!("meta.receipt_count_rooted: {s}"),
        }
    }
    Ok(if fail { EXIT_FAIL } else { 0 })
}

// ---------------------------------------------------------------------------
// Graph loading (nodes view + edges table)
// ---------------------------------------------------------------------------

struct NodeInfo {
    node_type: String,
    label: String,
}

struct Graph {
    order: Vec<String>,
    nodes: HashMap<String, NodeInfo>,
    edges: Vec<(i64, String, String, String, String)>,
    out_adj: HashMap<String, Vec<(String, String)>>,
    in_adj: HashMap<String, Vec<(String, String)>>,
}

fn load_graph(conn: &Connection) -> CliResult<Graph> {
    let mut order = Vec::new();
    let mut nodes = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT node_id, node_type, label FROM nodes")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let id = py_str(r.get_ref(0)?);
            if nodes.contains_key(&id) {
                continue;
            }
            order.push(id.clone());
            nodes.insert(
                id,
                NodeInfo {
                    node_type: py_str(r.get_ref(1)?),
                    label: opt_text(r.get_ref(2)?).unwrap_or_default(),
                },
            );
        }
    }
    let mut edges = Vec::new();
    let mut out_adj: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut in_adj: HashMap<String, Vec<(String, String)>> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT edge_id, from_node, to_node, relation, source FROM edges ORDER BY edge_id",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let e = (
                r.get::<_, i64>(0)?,
                py_str(r.get_ref(1)?),
                py_str(r.get_ref(2)?),
                py_str(r.get_ref(3)?),
                py_str(r.get_ref(4)?),
            );
            out_adj
                .entry(e.1.clone())
                .or_default()
                .push((e.2.clone(), e.3.clone()));
            in_adj
                .entry(e.2.clone())
                .or_default()
                .push((e.1.clone(), e.3.clone()));
            edges.push(e);
        }
    }
    for (_, from, to, _, _) in &edges {
        for n in [from, to] {
            if !nodes.contains_key(n) {
                order.push(n.clone());
                nodes.insert(
                    n.clone(),
                    NodeInfo {
                        node_type: n.split(':').next().unwrap_or("").to_string(),
                        label: String::new(),
                    },
                );
            }
        }
    }
    Ok(Graph {
        order,
        nodes,
        edges,
        out_adj,
        in_adj,
    })
}

/// One-line summary and attribute lines for a node, from its base row.
fn node_detail(
    conn: &Connection,
    g: &Graph,
    node_id: &str,
) -> CliResult<(String, Vec<(String, String)>)> {
    let info = g.nodes.get(node_id);
    let label = info.map(|i| i.label.clone()).unwrap_or_default();
    let (kind, rest) = node_id.split_once(':').unwrap_or((node_id, ""));
    let id: Option<i64> = rest.parse().ok();
    let mut attrs = Vec::new();
    let summary = match (kind, id) {
        ("claim", Some(id)) => {
            let status: Option<String> = conn
                .query_row(
                    "SELECT status FROM claims WHERE claim_id = ?1",
                    params![id],
                    |r| Ok(opt_text(r.get_ref(0)?)),
                )
                .unwrap_or(None);
            format!("[{}] {label}", null_or(status))
        }
        ("run", Some(id)) => {
            let row = conn.query_row(
                "SELECT build_hash, run_date, total_settled_usd FROM runs WHERE run_id = ?1",
                params![id],
                |r| {
                    Ok((
                        opt_text(r.get_ref(0)?),
                        opt_text(r.get_ref(1)?),
                        opt_f64(r.get_ref(2)?),
                    ))
                },
            );
            match row {
                Ok((build, date, total)) => format!(
                    "{label}  build={} date={} total_settled_usd={}",
                    null_or(build),
                    null_or(date),
                    total.map_or("(null)".to_string(), |t| format!("{t:.6}"))
                ),
                Err(_) => label,
            }
        }
        ("call", Some(id)) => {
            let row = conn.query_row(
                "SELECT settled_usd, mask_px, mode FROM calls WHERE call_id = ?1",
                params![id],
                |r| {
                    Ok((
                        opt_f64(r.get_ref(0)?),
                        opt_i64(r.get_ref(1)?),
                        opt_text(r.get_ref(2)?),
                    ))
                },
            );
            match row {
                Ok((usd, mask, mode)) => {
                    let mut s = format!(
                        "{label}  settled_usd={}",
                        usd.map_or("(null)".to_string(), |t| format!("{t:.6}"))
                    );
                    if let Some(m) = mask {
                        s.push_str(&format!(" mask_px={m}"));
                    }
                    if let Some(m) = mode {
                        s.push_str(&format!(" mode={m}"));
                    }
                    s
                }
                Err(_) => label,
            }
        }
        ("receipt", Some(id)) => {
            let row = conn.query_row(
                "SELECT file_sha256, source_file, verify_cmd, expected_line FROM receipts \
                 WHERE receipt_id = ?1",
                params![id],
                |r| {
                    Ok((
                        opt_text(r.get_ref(0)?),
                        opt_text(r.get_ref(1)?),
                        opt_text(r.get_ref(2)?),
                        opt_text(r.get_ref(3)?),
                    ))
                },
            );
            if let Ok((sha, sf, cmd, expected)) = row {
                attrs.push((
                    "sha256".to_string(),
                    sha.unwrap_or_else(|| "(none recorded)".to_string()),
                ));
                attrs.push(("source_file".to_string(), null_or(sf)));
                if let Some(c) = cmd {
                    attrs.push(("verify_cmd".to_string(), c));
                }
                if let Some(e) = expected {
                    attrs.push(("expected_line".to_string(), e));
                }
            }
            label
        }
        ("chapter", Some(id)) => {
            let row = conn.query_row(
                "SELECT t_start_s, t_end_s FROM video_chapters WHERE chapter_id = ?1",
                params![id],
                |r| Ok((opt_f64(r.get_ref(0)?), opt_f64(r.get_ref(1)?))),
            );
            match row {
                Ok((s, e)) => format!(
                    "[{}-{}] {label}",
                    s.map_or("?".to_string(), fmt_t),
                    e.map_or("".to_string(), fmt_t)
                ),
                Err(_) => label,
            }
        }
        _ => String::new(),
    };
    Ok((summary, attrs))
}

#[derive(Clone, Copy)]
enum Dir {
    Down,
    Up,
}

fn walk_tree(
    conn: &Connection,
    g: &Graph,
    node: &str,
    relation: Option<&str>,
    dir: Dir,
    seen: &mut HashSet<String>,
) -> CliResult<Value> {
    let (summary, attrs) = node_detail(conn, g, node)?;
    let mut obj = Map::new();
    obj.insert("node_id".into(), json!(node));
    obj.insert(
        "node_type".into(),
        json!(g
            .nodes
            .get(node)
            .map(|n| n.node_type.clone())
            .unwrap_or_default()),
    );
    if let Some(rel) = relation {
        obj.insert("relation".into(), json!(rel));
    }
    obj.insert("summary".into(), json!(summary));
    if !seen.insert(node.to_string()) {
        obj.insert("shown_above".into(), json!(true));
        obj.insert("children".into(), json!([]));
        return Ok(Value::Object(obj));
    }
    if !attrs.is_empty() {
        let list: Vec<Value> = attrs
            .into_iter()
            .map(|(k, v)| json!({"name": k, "value": v}))
            .collect();
        obj.insert("attrs".into(), json!(list));
    }
    let adj = match dir {
        Dir::Down => g.out_adj.get(node),
        Dir::Up => g.in_adj.get(node),
    };
    let mut children = Vec::new();
    if let Some(list) = adj {
        for (next, rel) in list {
            children.push(walk_tree(conn, g, next, Some(rel), dir, seen)?);
        }
    }
    obj.insert("children".into(), json!(children));
    Ok(Value::Object(obj))
}

fn render_tree(v: &Value, depth: usize, dir: Dir, out: &mut String) {
    let indent = "  ".repeat(depth);
    let node_id = v["node_id"].as_str().unwrap_or("");
    let summary = v["summary"].as_str().unwrap_or("");
    let head = match (v.get("relation").and_then(Value::as_str), dir) {
        (None, _) => node_id.to_string(),
        (Some(rel), Dir::Down) => format!("{rel} -> {node_id}"),
        (Some(rel), Dir::Up) => format!("<- {rel} -- {node_id}"),
    };
    if v.get("shown_above").and_then(Value::as_bool) == Some(true) {
        out.push_str(&format!("{indent}{head}  (shown above)\n"));
        return;
    }
    if summary.is_empty() {
        out.push_str(&format!("{indent}{head}\n"));
    } else {
        out.push_str(&format!("{indent}{head}  {summary}\n"));
    }
    if let Some(attrs) = v.get("attrs").and_then(Value::as_array) {
        for a in attrs {
            out.push_str(&format!(
                "{indent}    {}: {}\n",
                a["name"].as_str().unwrap_or(""),
                a["value"].as_str().unwrap_or("")
            ));
        }
    }
    if let Some(children) = v["children"].as_array() {
        for c in children {
            render_tree(c, depth + 1, dir, out);
        }
    }
}

fn trace_value(ctx: &Ctx, g: &Graph, node: &str) -> CliResult<Value> {
    let mut seen = HashSet::new();
    let down = walk_tree(&ctx.conn, g, node, None, Dir::Down, &mut seen)?;
    let mut seen_up = HashSet::new();
    let up = walk_tree(&ctx.conn, g, node, None, Dir::Up, &mut seen_up)?;
    Ok(json!({"start": node, "downstream": down, "upstream": up}))
}

fn render_trace(v: &Value) -> String {
    let mut out = String::new();
    out.push_str("downstream (outgoing edges):\n");
    render_tree(&v["downstream"], 1, Dir::Down, &mut out);
    let start = v["start"].as_str().unwrap_or("");
    match v["upstream"]["children"].as_array() {
        Some(children) if !children.is_empty() => {
            out.push_str(&format!("upstream (incoming edges into {start}):\n"));
            for c in children {
                render_tree(c, 1, Dir::Up, &mut out);
            }
        }
        _ => out.push_str(&format!("upstream (incoming edges into {start}): none\n")),
    }
    out
}

fn normalize_node_id(s: &str) -> String {
    if s.parse::<i64>().is_ok() {
        format!("claim:{s}")
    } else {
        s.to_string()
    }
}

fn cmd_trace(ctx: &Ctx, o: &Opts) -> CliResult<u8> {
    let g = load_graph(&ctx.conn)?;
    let node = normalize_node_id(&o.positional[1]);
    if !g.nodes.contains_key(&node) {
        return Err(CliError::usage(format!(
            "no node {node:?} in the graph (node ids: claim:N, run:N, call:N, receipt:N, \
             chapter:N, criterion:<name>, model:<name>, build:<hash>)"
        )));
    }
    let v = trace_value(ctx, &g, &node)?;
    if o.json {
        print_json(&v);
    } else {
        out!("{}", render_trace(&v));
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// chapters
// ---------------------------------------------------------------------------

struct Chapter {
    id: i64,
    start: f64,
    end: Option<f64>,
    line: Option<String>,
    claim: Option<i64>,
}

fn load_chapters(conn: &Connection) -> CliResult<Vec<Chapter>> {
    let mut stmt = conn.prepare(
        "SELECT chapter_id, t_start_s, t_end_s, narration_line, claim_id FROM video_chapters \
         ORDER BY t_start_s, chapter_id",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Chapter {
                id: r.get(0)?,
                start: opt_f64(r.get_ref(1)?).unwrap_or(0.0),
                end: opt_f64(r.get_ref(2)?),
                line: opt_text(r.get_ref(3)?),
                claim: opt_i64(r.get_ref(4)?),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn parse_time(s: &str) -> Option<f64> {
    let s = s.strip_suffix('s').unwrap_or(s);
    let parts: Vec<&str> = s.split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut total = 0.0;
    for (i, p) in parts.iter().enumerate() {
        let x: f64 = p.parse().ok()?;
        if x < 0.0 || !x.is_finite() {
            return None;
        }
        total = if i == 0 { x } else { total * 60.0 + x };
    }
    Some(total)
}

/// Resolve a chapter selector to a chapter id.
fn resolve_chapter(chapters: &[Chapter], spec: &str) -> CliResult<i64> {
    let body = spec.strip_prefix("chapter:").unwrap_or(spec);
    if !body.contains(':') && !body.ends_with('s') {
        if let Ok(id) = body.parse::<i64>() {
            return chapters
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.id)
                .ok_or_else(|| CliError::usage(format!("no chapter with id {id}")));
        }
    }
    let is_time = body.contains(':')
        || body.parse::<f64>().is_ok()
        || body
            .strip_suffix('s')
            .map_or(false, |b| b.parse::<f64>().is_ok());
    if is_time {
        let t = parse_time(body)
            .ok_or_else(|| CliError::usage(format!("{spec:?} is not a timestamp")))?;
        // Inclusive bounds; at a shared boundary the chapter that starts there wins.
        return chapters
            .iter()
            .filter(|c| c.start <= t && c.end.map_or(true, |e| t <= e))
            .max_by(|a, b| a.start.total_cmp(&b.start))
            .map(|c| c.id)
            .ok_or_else(|| CliError::usage(format!("no chapter covers t = {t} s")));
    }
    let token = |c: &Chapter| {
        c.line
            .as_deref()
            .and_then(|l| l.split_whitespace().next())
            .map(|t| t.trim_end_matches(':').to_string())
    };
    if let Some(c) = chapters
        .iter()
        .find(|c| token(c).map_or(false, |t| t.eq_ignore_ascii_case(body)))
    {
        return Ok(c.id);
    }
    let known: Vec<String> = chapters.iter().filter_map(token).collect();
    Err(CliError::usage(format!(
        "no chapter matches {spec:?} (ids, timestamps, or label tokens: {})",
        known.join(", ")
    )))
}

fn chapter_value(c: &Chapter) -> Value {
    json!({
        "chapter_id": c.id,
        "node_id": format!("chapter:{}", c.id),
        "t_start_s": c.start,
        "t_end_s": c.end,
        "narration_line": c.line,
        "claim_id": c.claim,
    })
}

fn cmd_chapter(ctx: &Ctx, o: &Opts) -> CliResult<u8> {
    let chapters = load_chapters(&ctx.conn)?;
    let id = resolve_chapter(&chapters, &o.positional[1])?;
    let c = chapters
        .iter()
        .find(|c| c.id == id)
        .expect("resolved chapter");
    let g = load_graph(&ctx.conn)?;
    let node = format!("chapter:{id}");
    let trace = trace_value(ctx, &g, &node)?;
    if o.json {
        print_json(&json!({"chapter": chapter_value(c), "trace": trace}));
    } else {
        outln!(
            "chapter:{id}  {}-{}  {}",
            fmt_t(c.start),
            c.end.map_or(String::new(), fmt_t),
            c.line.as_deref().unwrap_or("(no narration line)")
        );
        match c.claim {
            Some(claim) => {
                outln!("claim: claim:{claim}  (verify: agenthand evidence verify {claim})")
            }
            None => outln!("claim: none (this chapter has no claim_id and no supports edge)"),
        }
        out!("{}", render_trace(&trace));
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// list / show / rubric
// ---------------------------------------------------------------------------

fn claim_value(conn: &Connection, claim_id: i64) -> CliResult<Option<Value>> {
    let row = conn.query_row(
        "SELECT claim_id, text, status, scope_photo, scope_build, scope_date, reason, source_url \
         FROM claims WHERE claim_id = ?1",
        params![claim_id],
        |r| {
            Ok(json!({
                "claim_id": r.get::<_, i64>(0)?,
                "node_id": format!("claim:{}", r.get::<_, i64>(0)?),
                "text": opt_text(r.get_ref(1)?),
                "status": opt_text(r.get_ref(2)?),
                "scope_photo": opt_text(r.get_ref(3)?),
                "scope_build": opt_text(r.get_ref(4)?),
                "scope_date": opt_text(r.get_ref(5)?),
                "reason": opt_text(r.get_ref(6)?),
                "source_url": opt_text(r.get_ref(7)?),
            }))
        },
    );
    let mut v = match row {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut stmt = conn.prepare(
        "SELECT map_id, rubric_criterion, run_id, receipt_id FROM evidence_map \
         WHERE claim_id = ?1 ORDER BY map_id",
    )?;
    let maps = stmt
        .query_map(params![claim_id], |r| {
            Ok(json!({
                "map_id": r.get::<_, i64>(0)?,
                "rubric_criterion": opt_text(r.get_ref(1)?),
                "run_id": opt_i64(r.get_ref(2)?),
                "receipt_id": opt_i64(r.get_ref(3)?),
            }))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let chapters: Vec<Value> = load_chapters(conn)?
        .iter()
        .filter(|c| c.claim == Some(claim_id))
        .map(chapter_value)
        .collect();
    v["evidence_map"] = json!(maps);
    v["chapters"] = json!(chapters);
    Ok(Some(v))
}

fn cmd_list(ctx: &Ctx, o: &Opts) -> CliResult<u8> {
    if let Some(s) = &o.status {
        if !CLAIM_STATUSES.contains(&s.as_str()) {
            return Err(CliError::usage(format!(
                "--status must be one of {}",
                CLAIM_STATUSES.join(", ")
            )));
        }
    }
    let chapter_claims: Option<(i64, Option<i64>)> = match &o.chapter {
        Some(spec) => {
            let chapters = load_chapters(&ctx.conn)?;
            let id = resolve_chapter(&chapters, spec)?;
            let claim = chapters.iter().find(|c| c.id == id).and_then(|c| c.claim);
            Some((id, claim))
        }
        None => None,
    };
    let ids: Vec<i64> = {
        let mut stmt = ctx
            .conn
            .prepare("SELECT claim_id FROM claims ORDER BY claim_id")?;
        let ids = stmt
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids
    };
    let mut claims = Vec::new();
    for id in ids {
        if let Some((_, claim)) = chapter_claims {
            if claim != Some(id) {
                continue;
            }
        }
        let Some(v) = claim_value(&ctx.conn, id)? else {
            continue;
        };
        if let Some(s) = &o.status {
            if v["status"].as_str() != Some(s.as_str()) {
                continue;
            }
        }
        claims.push(v);
    }
    if o.json {
        print_json(&json!({
            "filters": {"chapter": o.chapter, "status": o.status},
            "chapter_id": chapter_claims.map(|c| c.0),
            "claims": claims,
        }));
        return Ok(0);
    }
    for c in &claims {
        let criteria: Vec<String> = c["evidence_map"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| m["rubric_criterion"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let chapters: Vec<String> = c["chapters"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|ch| format!("chapter:{}", ch["chapter_id"]))
                    .collect()
            })
            .unwrap_or_default();
        outln!(
            "{}  [{}]  criterion: {}  chapters: {}",
            c["node_id"].as_str().unwrap_or(""),
            c["status"].as_str().unwrap_or("(null)"),
            if criteria.is_empty() {
                "(none)".to_string()
            } else {
                criteria.join("; ")
            },
            if chapters.is_empty() {
                "(none)".to_string()
            } else {
                chapters.join(", ")
            },
        );
        outln!("    {}", c["text"].as_str().unwrap_or(""));
    }
    match chapter_claims {
        Some((ch, None)) => outln!("chapter:{ch} has no claim mapped; 0 claim(s)"),
        _ => outln!("{} claim(s)", claims.len()),
    }
    Ok(0)
}

fn run_value(conn: &Connection, run_id: i64) -> CliResult<Option<Value>> {
    let row = conn.query_row(
        "SELECT run_id, label, run_date, build_hash, item, package_path, total_settled_usd, \
         verify_result, verify_by, source, notes FROM runs WHERE run_id = ?1",
        params![run_id],
        |r| {
            Ok(json!({
                "run_id": r.get::<_, i64>(0)?,
                "label": opt_text(r.get_ref(1)?),
                "run_date": opt_text(r.get_ref(2)?),
                "build_hash": opt_text(r.get_ref(3)?),
                "item": opt_text(r.get_ref(4)?),
                "package_path": opt_text(r.get_ref(5)?),
                "total_settled_usd": opt_f64(r.get_ref(6)?),
                "verify_result": opt_text(r.get_ref(7)?),
                "verify_by": opt_text(r.get_ref(8)?),
                "source": opt_text(r.get_ref(9)?),
                "notes": opt_text(r.get_ref(10)?),
            }))
        },
    );
    let mut v = match row {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut stmt = conn.prepare(
        "SELECT call_id, tool, model, settled_usd, duration_s, mask_px, mode FROM calls \
         WHERE run_id = ?1 ORDER BY call_id",
    )?;
    let calls = stmt
        .query_map(params![run_id], call_row)?
        .collect::<Result<Vec<_>, _>>()?;
    v["calls"] = json!(calls);
    Ok(Some(v))
}

fn call_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "call_id": r.get::<_, i64>(0)?,
        "tool": opt_text(r.get_ref(1)?),
        "model": opt_text(r.get_ref(2)?),
        "settled_usd": opt_f64(r.get_ref(3)?),
        "duration_s": opt_f64(r.get_ref(4)?),
        "mask_px": opt_i64(r.get_ref(5)?),
        "mode": opt_text(r.get_ref(6)?),
    }))
}

fn receipt_value(conn: &Connection, receipt_id: i64) -> CliResult<Option<Value>> {
    let row = conn.query_row(
        "SELECT receipt_id, run_id, kind, checked_px, changed_px, file_sha256, source_file, \
         verify_cmd, expected_line FROM receipts WHERE receipt_id = ?1",
        params![receipt_id],
        |r| {
            Ok(json!({
                "receipt_id": r.get::<_, i64>(0)?,
                "run_id": opt_i64(r.get_ref(1)?),
                "kind": opt_text(r.get_ref(2)?),
                "checked_px": opt_i64(r.get_ref(3)?),
                "changed_px": opt_i64(r.get_ref(4)?),
                "file_sha256": opt_text(r.get_ref(5)?),
                "source_file": opt_text(r.get_ref(6)?),
                "verify_cmd": opt_text(r.get_ref(7)?),
                "expected_line": opt_text(r.get_ref(8)?),
            }))
        },
    );
    match row {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn s(v: &Value) -> String {
    match v {
        Value::Null => "(null)".to_string(),
        Value::String(x) => x.clone(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

fn usd(v: &Value) -> String {
    v.as_f64()
        .map_or("(null)".to_string(), |x| format!("{x:.6}"))
}

fn cmd_show(ctx: &Ctx, o: &Opts) -> CliResult<u8> {
    let id = parse_prefixed_id(&o.positional[1], "claim")?;
    let Some(mut claim) = claim_value(&ctx.conn, id)? else {
        return Err(CliError::usage(format!("no claim with id {id}")));
    };
    let mut maps = claim["evidence_map"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for m in maps.iter_mut() {
        if let Some(run) = m["run_id"].as_i64() {
            m["run"] = run_value(&ctx.conn, run)?.unwrap_or(Value::Null);
        }
        if let Some(rc) = m["receipt_id"].as_i64() {
            m["receipt"] = receipt_value(&ctx.conn, rc)?.unwrap_or(Value::Null);
        }
    }
    claim["evidence_map"] = json!(maps);
    claim["verify_command"] = json!(format!("agenthand evidence verify {id}"));
    if o.json {
        print_json(&claim);
        return Ok(0);
    }
    outln!("claim:{id}  [{}]", s(&claim["status"]));
    outln!("text:        {}", s(&claim["text"]));
    for k in [
        "scope_photo",
        "scope_build",
        "scope_date",
        "reason",
        "source_url",
    ] {
        outln!("{:<12} {}", format!("{k}:"), s(&claim[k]));
    }
    if maps.is_empty() {
        outln!("evidence_map: (no rows)");
    }
    for m in &maps {
        outln!(
            "evidence_map {}: criterion \"{}\", {}, {}",
            s(&m["map_id"]),
            s(&m["rubric_criterion"]),
            m["run_id"]
                .as_i64()
                .map_or("run (null)".to_string(), |r| format!("run:{r}")),
            m["receipt_id"]
                .as_i64()
                .map_or("receipt (null)".to_string(), |r| format!("receipt:{r}")),
        );
        let run = &m["run"];
        if run.is_object() {
            outln!("  run:{}  {}", s(&run["run_id"]), s(&run["label"]));
            outln!(
                "    date {}  build {}  total_settled_usd {}",
                s(&run["run_date"]),
                s(&run["build_hash"]),
                usd(&run["total_settled_usd"])
            );
            outln!("    verify_result: {}", s(&run["verify_result"]));
            for c in run["calls"].as_array().into_iter().flatten() {
                outln!(
                    "    call:{}  {}  model {}  settled_usd {}",
                    s(&c["call_id"]),
                    s(&c["tool"]),
                    s(&c["model"]),
                    usd(&c["settled_usd"])
                );
            }
        }
        let rc = &m["receipt"];
        if rc.is_object() {
            outln!(
                "  receipt:{}  {}  checked_px {}  changed_px {}",
                s(&rc["receipt_id"]),
                s(&rc["kind"]),
                s(&rc["checked_px"]),
                s(&rc["changed_px"])
            );
            outln!("    file_sha256:   {}", s(&rc["file_sha256"]));
            outln!("    source_file:   {}", s(&rc["source_file"]));
            outln!("    verify_cmd:    {}", s(&rc["verify_cmd"]));
            outln!("    expected_line: {}", s(&rc["expected_line"]));
        }
    }
    let chapters = claim["chapters"].as_array().cloned().unwrap_or_default();
    if chapters.is_empty() {
        outln!("chapters: (none)");
    }
    for ch in &chapters {
        outln!(
            "chapter:{}  {}-{}  {}",
            s(&ch["chapter_id"]),
            ch["t_start_s"].as_f64().map_or("?".to_string(), fmt_t),
            ch["t_end_s"].as_f64().map_or(String::new(), fmt_t),
            s(&ch["narration_line"])
        );
    }
    outln!("verify: agenthand evidence verify {id}");
    Ok(0)
}

fn cmd_rubric(ctx: &Ctx, o: &Opts) -> CliResult<u8> {
    let mut stmt = ctx.conn.prepare(
        "SELECT m.rubric_criterion, m.claim_id, c.status, c.text, m.run_id, m.receipt_id \
         FROM evidence_map m LEFT JOIN claims c ON c.claim_id = m.claim_id \
         ORDER BY m.rubric_criterion, m.claim_id, m.map_id",
    )?;
    let mut by: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let crit = py_str(r.get_ref(0)?);
        by.entry(crit).or_default().push(json!({
            "claim_id": opt_i64(r.get_ref(1)?),
            "status": opt_text(r.get_ref(2)?),
            "text": opt_text(r.get_ref(3)?),
            "run_id": opt_i64(r.get_ref(4)?),
            "receipt_id": opt_i64(r.get_ref(5)?),
        }));
    }
    if o.json {
        let list: Vec<Value> = by
            .iter()
            .map(|(k, v)| json!({"criterion": k, "node_id": format!("criterion:{k}"), "claims": v}))
            .collect();
        print_json(&json!({"criteria": list}));
        return Ok(0);
    }
    for (crit, claims) in &by {
        outln!("criterion:{crit}  ({} evidence_map row(s))", claims.len());
        for c in claims {
            outln!(
                "  claim:{}  [{}]  {}  {}",
                s(&c["claim_id"]),
                s(&c["status"]),
                c["run_id"]
                    .as_i64()
                    .map_or("run (null)".to_string(), |r| format!("run:{r}")),
                c["receipt_id"]
                    .as_i64()
                    .map_or("receipt (null)".to_string(), |r| format!("receipt:{r}")),
            );
            outln!("      {}", s(&c["text"]));
        }
    }
    outln!("{} criterion/criteria with evidence_map rows", by.len());
    Ok(0)
}

// ---------------------------------------------------------------------------
// calls
// ---------------------------------------------------------------------------

fn cmd_calls(ctx: &Ctx, o: &Opts) -> CliResult<u8> {
    let run_filter = match &o.run {
        Some(r) => Some(parse_prefixed_id(r, "run")?),
        None => None,
    };
    if o.by_model {
        let mut stmt = ctx.conn.prepare(
            "SELECT model, COUNT(*), COUNT(settled_usd), SUM(settled_usd) FROM calls \
             WHERE (?1 IS NULL OR run_id = ?1) GROUP BY model ORDER BY model IS NULL, model",
        )?;
        let groups = stmt
            .query_map(params![run_filter], |r| {
                Ok((
                    opt_text(r.get_ref(0)?),
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    opt_f64(r.get_ref(3)?),
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::new();
        for (model, n, n_costed, sum) in groups {
            let mut tstmt = ctx.conn.prepare(
                "SELECT DISTINCT tool FROM calls WHERE model IS ?1 \
                 AND (?2 IS NULL OR run_id = ?2) ORDER BY tool",
            )?;
            let tools = tstmt
                .query_map(params![model, run_filter], |r| Ok(py_str(r.get_ref(0)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            out.push(json!({
                "model": model,
                "calls": n,
                "costed_calls": n_costed,
                "settled_usd_sum_over_call_rows": sum,
                "tools": tools,
            }));
        }
        let note = "sums are over call rows; variant runs repeat their shared identify and \
                    cut_out calls per variant row, so per-run spend is runs.total_settled_usd";
        if o.json {
            print_json(&json!({"run_id": run_filter, "by_model": out, "note": note}));
            return Ok(0);
        }
        for g in &out {
            outln!(
                "model:{}  calls={}  costed={}  settled_usd_sum={}  tools={}",
                g["model"].as_str().unwrap_or("(null)"),
                s(&g["calls"]),
                s(&g["costed_calls"]),
                usd(&g["settled_usd_sum_over_call_rows"]),
                g["tools"]
                    .as_array()
                    .map(|a| a.iter().map(s).collect::<Vec<_>>().join(","))
                    .unwrap_or_default(),
            );
        }
        outln!("note: {note}");
        return Ok(0);
    }
    let mut stmt = ctx.conn.prepare(
        "SELECT call_id, tool, model, settled_usd, duration_s, mask_px, mode, run_id FROM calls \
         WHERE (?1 IS NULL OR run_id = ?1) ORDER BY call_id",
    )?;
    let calls = stmt
        .query_map(params![run_filter], |r| {
            let mut v = call_row(r)?;
            v["run_id"] = json!(opt_i64(r.get_ref(7)?));
            Ok(v)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if o.json {
        print_json(&json!({"run_id": run_filter, "calls": calls}));
        return Ok(0);
    }
    for c in &calls {
        let mut line = format!(
            "call:{}  run:{}  {}  model={}  settled_usd={}",
            s(&c["call_id"]),
            s(&c["run_id"]),
            s(&c["tool"]),
            s(&c["model"]),
            usd(&c["settled_usd"])
        );
        if !c["mask_px"].is_null() {
            line.push_str(&format!("  mask_px={}", s(&c["mask_px"])));
        }
        if !c["mode"].is_null() {
            line.push_str(&format!("  mode={}", s(&c["mode"])));
        }
        outln!("{line}");
    }
    outln!("{} call row(s)", calls.len());
    Ok(0)
}

// ---------------------------------------------------------------------------
// graph export
// ---------------------------------------------------------------------------

fn short(label: &str, n: usize) -> String {
    let mut out: String = label.chars().take(n).collect();
    if label.chars().count() > n {
        out.push_str("...");
    }
    out
}

fn node_caption(id: &str, info: &NodeInfo) -> String {
    let (kind, rest) = id.split_once(':').unwrap_or((id, ""));
    if info.label.is_empty()
        || info.label == rest
        || matches!(kind, "criterion" | "model" | "build")
    {
        id.to_string()
    } else {
        format!("{id}\n{}", short(&info.label, 48))
    }
}

fn cmd_graph(ctx: &Ctx, o: &Opts) -> CliResult<u8> {
    let g = load_graph(&ctx.conn)?;
    if o.json {
        let nodes: Vec<Value> = g
            .order
            .iter()
            .map(|id| {
                let n = &g.nodes[id];
                json!({"node_id": id, "node_type": n.node_type, "label": n.label})
            })
            .collect();
        let edges: Vec<Value> = g
            .edges
            .iter()
            .map(|(eid, f, t, rel, src)| {
                json!({"edge_id": eid, "from": f, "to": t, "relation": rel, "source": src})
            })
            .collect();
        print_json(&json!({"nodes": nodes, "edges": edges}));
        return Ok(0);
    }
    let format = o.format.as_deref().unwrap_or("mermaid");
    match format {
        "mermaid" => {
            let ids: HashMap<&str, String> = g
                .order
                .iter()
                .enumerate()
                .map(|(i, id)| (id.as_str(), format!("n{i}")))
                .collect();
            let esc = |s: &str| {
                s.replace('"', "#quot;")
                    .replace('<', "#lt;")
                    .replace('>', "#gt;")
                    .replace('\n', "<br/>")
            };
            outln!("flowchart LR");
            for id in &g.order {
                outln!(
                    "  {}[\"{}\"]",
                    ids[id.as_str()],
                    esc(&node_caption(id, &g.nodes[id]))
                );
            }
            for (_, f, t, rel, _) in &g.edges {
                outln!("  {} -->|{}| {}", ids[f.as_str()], rel, ids[t.as_str()]);
            }
        }
        "dot" => {
            let esc = |s: &str| {
                s.replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', "\\n")
            };
            outln!("digraph proof_graph {{");
            outln!("  rankdir=LR;");
            outln!("  node [shape=box, fontsize=10];");
            for id in &g.order {
                outln!(
                    "  \"{}\" [label=\"{}\"];",
                    esc(id),
                    esc(&node_caption(id, &g.nodes[id]))
                );
            }
            for (_, f, t, rel, _) in &g.edges {
                outln!(
                    "  \"{}\" -> \"{}\" [label=\"{}\"];",
                    esc(f),
                    esc(t),
                    esc(rel)
                );
            }
            outln!("}}");
        }
        other => {
            return Err(CliError::usage(format!(
                "--format must be mermaid or dot (got {other:?})"
            )))
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// tamper
// ---------------------------------------------------------------------------

fn cmd_tamper(ctx: &Ctx, o: &Opts) -> CliResult<u8> {
    let id = parse_prefixed_id(&o.positional[1], "receipt")?;
    let Some(rc) = receipt_value(&ctx.conn, id)? else {
        return Err(CliError::usage(format!("no receipt with id {id}")));
    };
    let db_sha = rc["file_sha256"].as_str().map(String::from);
    let sf = rc["source_file"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);
    let path = sf
        .as_deref()
        .map(|s| resolve_receipt_path(&ctx.base_dir, s));
    let mut out = json!({
        "receipt_id": id,
        "run_id": rc["run_id"],
        "source_file": sf,
        "resolved_path": path.as_ref().map(|p| p.display().to_string()),
        "db_file_sha256": db_sha,
    });
    let mut lines = vec![
        format!(
            "receipt:{id} (run:{})  source_file {}",
            s(&rc["run_id"]),
            sf.as_deref().unwrap_or("(null)")
        ),
        format!(
            "db file_sha256:   {}",
            db_sha.as_deref().unwrap_or("(null)")
        ),
    ];
    let finish = |out: &mut Value,
                  lines: &mut Vec<String>,
                  result: &str,
                  msg: String,
                  code: u8|
     -> CliResult<u8> {
        out["result"] = json!(result);
        out["detail"] = json!(msg);
        lines.push(msg);
        if o.json {
            print_json(out);
        } else {
            for l in lines.iter() {
                outln!("{l}");
            }
        }
        Ok(code)
    };
    let Some(db_sha) = db_sha else {
        return finish(
            &mut out,
            &mut lines,
            "not_checked",
            "not checked: receipt carries no file_sha256".into(),
            0,
        );
    };
    let Some(path) = path.filter(|p| p.is_file()) else {
        let shown = sf
            .as_deref()
            .map(|s| resolve_receipt_path(&ctx.base_dir, s).display().to_string())
            .unwrap_or_else(|| "(no source_file)".to_string());
        return finish(
            &mut out,
            &mut lines,
            "not_checked",
            format!("not checked: file absent ({shown})"),
            0,
        );
    };
    let mut bytes = std::fs::read(&path)
        .map_err(|e| CliError::usage(format!("cannot read {}: {e}", path.display())))?;
    let on_disk = hex(&Sha256::digest(&bytes));
    lines.push(format!("on-disk sha256:   {on_disk}"));
    out["on_disk_sha256"] = json!(on_disk);
    if on_disk != db_sha {
        return finish(
            &mut out,
            &mut lines,
            "fail_before_tamper",
            "FAIL: the file on disk already differs from the database sha256; no tamper applied"
                .into(),
            EXIT_FAIL,
        );
    }
    if bytes.is_empty() {
        return finish(
            &mut out,
            &mut lines,
            "not_checked",
            "not checked: file is empty".into(),
            0,
        );
    }
    let offset = bytes.len() / 2;
    bytes[offset] ^= 0x01;
    let tampered = hex(&Sha256::digest(&bytes));
    lines.push(format!(
        "tamper:           byte {offset} XOR 0x01, in memory only (the file on disk is not modified)"
    ));
    lines.push(format!("tampered sha256:  {tampered}"));
    out["tamper_offset"] = json!(offset);
    out["tampered_sha256"] = json!(tampered);
    if tampered != db_sha {
        finish(
            &mut out,
            &mut lines,
            "tamper_detected",
            "check: FAIL for the tampered copy (sha256 differs from the database); the unmodified file matches".into(),
            0,
        )
    } else {
        finish(
            &mut out,
            &mut lines,
            "tamper_not_detected",
            "check: the tampered copy still matches the database sha256".into(),
            EXIT_FAIL,
        )
    }
}
