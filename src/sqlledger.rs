//! SQLite-backed billing ledger (rusqlite, bundled SQLite) — the
//! second store behind the same interface as `ledger.rs`, selected
//! via `AGENTHAND_LEDGER` (see `config::get_ledger_store`), with
//! byte-for-byte identical ACCOUNTING semantics:
//!
//! - reserve-then-settle: a "pending" receipt at the projected cost
//!   before the call, a "settled" receipt with the same call_id and
//!   the actual cost after;
//! - the total counts, per call_id, the settled actual when present,
//!   else the pending projection; call_id-less (or non-pending /
//!   non-settled phase) receipts count at face value;
//! - negative or non-finite amounts fail closed (raise), on write
//!   and on read;
//! - totals round to the micro-dollar exactly as the JSONL store
//!   does (`(total * 1e6).round() / 1e6`).
//!
//! The reduction runs in Rust over rows fetched in insertion order
//! (rowid), never as SQL aggregates, so float behaviour matches the
//! JSONL line-by-line reduction exactly.
//!
//! Reserve is check-and-insert inside one `BEGIN IMMEDIATE`
//! transaction: two threads or processes cannot both pass the cap
//! gate on the same stale total, and no file lock is held while the
//! billable call runs. WAL mode + a busy timeout let readers and the
//! single writer interleave without `SQLITE_BUSY` surfacing.

use crate::ledger::{Receipt, PHASE_FINAL, PHASE_PENDING, PHASE_SETTLED};
use rusqlite::{params, Connection, TransactionBehavior};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

const BUSY_TIMEOUT: Duration = Duration::from_secs(30);

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS receipts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    tool TEXT NOT NULL,
    model TEXT,
    usd_estimate REAL NOT NULL,
    pricing_table_version TEXT NOT NULL,
    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    cached_input_tokens INTEGER NOT NULL,
    images INTEGER NOT NULL,
    status TEXT NOT NULL,
    phase TEXT NOT NULL,
    call_id TEXT,
    note TEXT NOT NULL,
    timestamp TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_receipts_call_id ON receipts(call_id);
";

fn validate_amount(value: f64, where_: &str) -> Result<f64, String> {
    if !value.is_finite() {
        return Err(format!("{where_}: amount {value} is not finite."));
    }
    if value < 0.0 {
        return Err(format!("{where_}: amount {value} is negative."));
    }
    Ok(value)
}

fn open(path: &Path) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create ledger dir: {e}"))?;
    }
    let conn = Connection::open(path).map_err(|e| format!("cannot open sqlite ledger: {e}"))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|e| format!("cannot set sqlite busy timeout: {e}"))?;
    conn.execute_batch("PRAGMA journal_mode=WAL;")
        .map_err(|e| format!("cannot set sqlite journal mode: {e}"))?;
    conn.execute_batch(SCHEMA)
        .map_err(|e| format!("cannot create sqlite ledger schema: {e}"))?;
    Ok(conn)
}

fn insert_receipt(conn: &Connection, receipt: &Receipt) -> Result<(), String> {
    conn.execute(
        "INSERT INTO receipts (tool, model, usd_estimate, pricing_table_version,
                               input_tokens, output_tokens, cached_input_tokens,
                               images, status, phase, call_id, note, timestamp)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            receipt.tool,
            receipt.model,
            receipt.usd_estimate,
            receipt.pricing_table_version,
            receipt.input_tokens,
            receipt.output_tokens,
            receipt.cached_input_tokens,
            receipt.images,
            receipt.status,
            receipt.phase,
            receipt.call_id,
            receipt.note,
            receipt.timestamp,
        ],
    )
    .map_err(|e| format!("cannot insert sqlite ledger receipt: {e}"))?;
    Ok(())
}

/// The total reduction, identical to `ledger::ledger_total_usd` but
/// over SQLite rows in insertion order.
fn total_from_conn(conn: &Connection, path: &Path) -> Result<f64, String> {
    let mut stmt = conn
        .prepare("SELECT id, usd_estimate, call_id, phase FROM receipts ORDER BY id ASC")
        .map_err(|e| format!("cannot read sqlite ledger: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<f64>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|e| format!("cannot read sqlite ledger: {e}"))?;
    let mut standalone = 0.0f64;
    let mut calls: HashMap<String, f64> = HashMap::new();
    let mut settled: HashSet<String> = HashSet::new();
    for row in rows {
        let (id, amount, call_id, phase) = row.map_err(|e| {
            format!("sqlite ledger row is unreadable ({e}); refusing to spend with unknown spend state.")
        })?;
        let where_ = format!("Ledger {} row {}", path.display(), id);
        // SQLite stores NaN as NULL; a NULL amount is not a number and
        // fails closed here just like an unparseable JSONL line.
        let amount = amount.ok_or_else(|| format!("{where_}: amount NULL is not a number."))?;
        let amount = validate_amount(amount, &where_)?;
        let call_id = call_id.as_deref().filter(|s| !s.is_empty());
        let phase = phase.as_deref().unwrap_or(PHASE_FINAL);
        match call_id {
            Some(id) if phase == PHASE_SETTLED => {
                calls.insert(id.to_string(), amount);
                settled.insert(id.to_string());
            }
            Some(id) if phase == PHASE_PENDING => {
                if !settled.contains(id) && !calls.contains_key(id) {
                    calls.insert(id.to_string(), amount);
                }
            }
            _ => standalone += amount,
        }
    }
    let total: f64 = standalone + calls.values().sum::<f64>();
    Ok((total * 1e6).round() / 1e6)
}

/// The counted ledger total (same accounting as `ledger::ledger_total_usd`).
pub fn ledger_total_usd(path: &Path) -> Result<f64, String> {
    if !path.exists() {
        return Ok(0.0);
    }
    let conn = open(path)?;
    total_from_conn(&conn, path)
}

/// Number of receipt rows stored (test/diagnostic aid).
pub fn count_receipts(path: &Path) -> Result<usize, String> {
    if !path.exists() {
        return Ok(0);
    }
    let conn = open(path)?;
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM receipts", [], |row| row.get(0))
        .map_err(|e| format!("cannot count sqlite ledger receipts: {e}"))?;
    Ok(n as usize)
}

/// Insert one receipt row.
pub fn append_receipt(path: &Path, receipt: &Receipt) -> Result<(), String> {
    validate_amount(
        receipt.usd_estimate,
        &format!("receipt for {}", receipt.tool),
    )?;
    let conn = open(path)?;
    insert_receipt(&conn, receipt)
}

/// Fail-closed cap check; the refusal message matches the JSONL store's.
pub fn check_spend_cap(path: &Path, projected_usd: f64, cap_usd: f64) -> Result<f64, String> {
    let projected = validate_amount(projected_usd, "projected cost")?;
    let cap = validate_amount(cap_usd, "spend cap")?;
    let total = ledger_total_usd(path)?;
    if total + projected > cap {
        return Err(format!(
            "Spend cap: ledger total ${total:.2} + projected ${projected:.4} would exceed the ${cap:.2} cap. Call refused before it ran."
        ));
    }
    Ok(total)
}

/// Reserve: check the cap and append the pending receipt atomically
/// in one IMMEDIATE transaction. Returns the pre-call ledger total,
/// like `ledger::check_spend_cap`.
pub fn reserve(path: &Path, receipt: &Receipt, cap_usd: f64) -> Result<f64, String> {
    validate_amount(
        receipt.usd_estimate,
        &format!("receipt for {}", receipt.tool),
    )?;
    let cap = validate_amount(cap_usd, "spend cap")?;
    let mut conn = open(path)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("cannot begin sqlite ledger transaction: {e}"))?;
    let total = total_from_conn(&tx, path)?;
    if total + receipt.usd_estimate > cap {
        return Err(format!(
            "Spend cap: ledger total ${total:.2} + projected ${:.4} would exceed the ${cap:.2} cap. Call refused before it ran.",
            receipt.usd_estimate
        ));
    }
    insert_receipt(&tx, receipt)?;
    tx.commit()
        .map_err(|e| format!("cannot commit sqlite ledger reservation: {e}"))?;
    Ok(total)
}

/// Import a JSONL ledger into a fresh SQLite ledger, line by line,
/// with the JSONL store's fail-closed parsing (unreadable line,
/// missing/non-numeric/non-finite/negative amount all raise).
/// Returns (rows imported, total after import). The total is the
/// SQLite reduction; callers compare it with
/// `ledger::ledger_total_usd` on the source — they must be equal.
pub fn import_jsonl(jsonl_path: &Path, sqlite_path: &Path) -> Result<(usize, f64), String> {
    let text =
        std::fs::read_to_string(jsonl_path).map_err(|e| format!("cannot read ledger: {e}"))?;
    if sqlite_path.exists() {
        return Err(format!(
            "sqlite ledger {} already exists; refusing to import over it.",
            sqlite_path.display()
        ));
    }
    let mut conn = open(sqlite_path)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("cannot begin sqlite ledger transaction: {e}"))?;
    let mut count = 0usize;
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let where_ = format!("Ledger {} line {}", jsonl_path.display(), i + 1);
        let record: serde_json::Value = serde_json::from_str(line).map_err(|e| {
            format!("{where_} is unreadable ({e}); refusing to spend with unknown spend state.")
        })?;
        let amount_raw = record.get("usd_estimate").ok_or_else(|| {
            format!("{where_} has no usd_estimate; refusing to spend with unknown spend state.")
        })?;
        let amount = amount_raw
            .as_f64()
            .ok_or_else(|| format!("{where_}: amount {amount_raw} is not a number."))?;
        let amount = validate_amount(amount, &where_)?;
        let str_field = |key: &str| {
            record
                .get(key)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        };
        let int_field = |key: &str| record.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
        let receipt = Receipt {
            tool: str_field("tool").unwrap_or_default(),
            model: str_field("model"),
            usd_estimate: amount,
            pricing_table_version: str_field("pricing_table_version").unwrap_or_default(),
            input_tokens: int_field("input_tokens"),
            output_tokens: int_field("output_tokens"),
            cached_input_tokens: int_field("cached_input_tokens"),
            images: int_field("images"),
            status: str_field("status").unwrap_or_default(),
            phase: str_field("phase").unwrap_or_else(|| PHASE_FINAL.to_string()),
            call_id: str_field("call_id").filter(|s| !s.is_empty()),
            note: str_field("note").unwrap_or_default(),
            timestamp: str_field("timestamp").unwrap_or_default(),
        };
        insert_receipt(&tx, &receipt)?;
        count += 1;
    }
    let total = total_from_conn(&tx, sqlite_path)?;
    tx.commit()
        .map_err(|e| format!("cannot commit sqlite ledger import: {e}"))?;
    Ok((count, total))
}
