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
//!
//! Connections are persistent: one connection per ledger path is
//! opened on first use and kept for the life of the process (see
//! `conn_for`). Opening a fresh connection per call re-paid the WAL
//! pragma + schema setup every time (~66–176 ms/call in the spike's
//! wall-clock harness); the cache removes that overhead while the
//! per-connection mutex preserves the one-writer-at-a-time semantics
//! the fresh-connection design got from SQLite itself.

use crate::ledger::{Receipt, SettleOverage, PHASE_FINAL, PHASE_PENDING, PHASE_SETTLED};
use rusqlite::{params, Connection, TransactionBehavior};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
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

/// One persistent connection per ledger path, opened lazily and
/// reused for the life of the process. A poisoned cache or
/// connection mutex fails closed: spend state reached through a
/// poisoned lock is state we cannot trust.
static CONN_CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<Connection>>>>> = OnceLock::new();

fn conn_for(path: &Path) -> Result<Arc<Mutex<Connection>>, String> {
    let cache = CONN_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().map_err(|_| {
        "sqlite ledger connection cache is poisoned; refusing to spend with unknown spend state."
            .to_string()
    })?;
    if let Some(conn) = cache.get(path) {
        return Ok(Arc::clone(conn));
    }
    let conn = Arc::new(Mutex::new(open(path)?));
    cache.insert(path.to_path_buf(), Arc::clone(&conn));
    Ok(conn)
}

fn lock_conn(
    conn: &Arc<Mutex<Connection>>,
) -> Result<std::sync::MutexGuard<'_, Connection>, String> {
    conn.lock().map_err(|_| {
        "sqlite ledger connection is poisoned; refusing to spend with unknown spend state."
            .to_string()
    })
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
    total_from_conn_excluding(conn, path, None)
}

/// `total_from_conn` with every row of `exclude_call` left out (the
/// settle-time cap check, fix pass F5).
fn total_from_conn_excluding(
    conn: &Connection,
    path: &Path,
    exclude_call: Option<&str>,
) -> Result<f64, String> {
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
        if exclude_call.is_some() && call_id == exclude_call {
            continue;
        }
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
    let conn = conn_for(path)?;
    let conn = lock_conn(&conn)?;
    total_from_conn(&conn, path)
}

/// Number of receipt rows stored (test/diagnostic aid).
pub fn count_receipts(path: &Path) -> Result<usize, String> {
    if !path.exists() {
        return Ok(0);
    }
    let conn = conn_for(path)?;
    let conn = lock_conn(&conn)?;
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
    let conn = conn_for(path)?;
    let conn = lock_conn(&conn)?;
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
    let conn = conn_for(path)?;
    let mut conn = lock_conn(&conn)?;
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

/// Settle (fix pass F5): inside one IMMEDIATE transaction, recompute
/// the ledger total without this call's rows (every other settled
/// actual plus every other open reservation), apply the settle-time
/// cap check (`ledger::apply_settle_cap`, which marks the receipt
/// `over_cap` when others + actual exceeds the cap), and insert the
/// receipt — truthfully, either way. Returns the breach, if any.
pub fn settle(
    path: &Path,
    receipt: &mut Receipt,
    cap_usd: f64,
) -> Result<Option<SettleOverage>, String> {
    validate_amount(
        receipt.usd_estimate,
        &format!("receipt for {}", receipt.tool),
    )?;
    let cap = validate_amount(cap_usd, "spend cap")?;
    let conn = conn_for(path)?;
    let mut conn = lock_conn(&conn)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("cannot begin sqlite ledger transaction: {e}"))?;
    let others = total_from_conn_excluding(&tx, path, receipt.call_id.as_deref())?;
    let overage = crate::ledger::apply_settle_cap(receipt, others, cap);
    insert_receipt(&tx, receipt)?;
    tx.commit()
        .map_err(|e| format!("cannot commit sqlite ledger settle: {e}"))?;
    Ok(overage)
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

// ---------------------------------------------------------------------
// Room-run step log (P1, Hermes dive 2026-10-31, tracker event 372)
//
// Durable execution state for room runs, Morling-style: a step row
// is written `pending` BEFORE the step's first billed call and
// flipped to `complete`/`failed` when it settles, so a crash or
// kill mid-run leaves a resumable record instead of an
// all-or-nothing manifest. The table lives in a SQLite file the
// CALLER chooses — for room runs that is `room-steps.sqlite3`
// inside the room's own output directory, beside the room's own
// JSONL ledger: a room stays self-contained and provable from its
// directory alone (the room-ledger isolation rule, tracker event
// 166), and the run's receipts + step state travel together.
// ---------------------------------------------------------------------

const RUN_STEPS_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS run_steps (
    run_id     TEXT NOT NULL,
    item_index INTEGER NOT NULL,
    step       TEXT NOT NULL,
    status     TEXT NOT NULL CHECK (status IN ('pending','complete','failed')),
    attempts   INTEGER NOT NULL DEFAULT 1,
    call_id    TEXT,
    result_ref TEXT,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (run_id, item_index, step)
);
";

/// One row of the room-run step log.
#[derive(Debug, Clone, PartialEq)]
pub struct StepRow {
    pub status: String,
    pub attempts: i64,
    pub call_id: Option<String>,
    pub result_ref: Option<String>,
    pub updated_at: i64,
}

fn now_epoch_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The room-run step log: one SQLite file, one connection, writes
/// serialized through a mutex (a room run is a sequential pipeline;
/// the mutex is for the type's own safety, not concurrency).
pub struct RunStepLog {
    conn: Mutex<Connection>,
}

impl RunStepLog {
    pub fn open(path: &Path) -> Result<RunStepLog, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create step-log dir: {e}"))?;
        }
        let conn =
            Connection::open(path).map_err(|e| format!("cannot open step log: {e}"))?;
        conn.busy_timeout(BUSY_TIMEOUT)
            .map_err(|e| format!("cannot set step-log busy timeout: {e}"))?;
        conn.execute_batch(RUN_STEPS_SCHEMA)
            .map_err(|e| format!("cannot create run_steps table: {e}"))?;
        Ok(RunStepLog {
            conn: Mutex::new(conn),
        })
    }

    /// Read one step row, if it exists.
    pub fn get(&self, run_id: &str, item_index: i64, step: &str) -> Result<Option<StepRow>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT status, attempts, call_id, result_ref, updated_at \
                 FROM run_steps WHERE run_id = ?1 AND item_index = ?2 AND step = ?3",
            )
            .map_err(|e| format!("step-log prepare: {e}"))?;
        let row = stmt
            .query_row(params![run_id, item_index, step], |r| {
                Ok(StepRow {
                    status: r.get(0)?,
                    attempts: r.get(1)?,
                    call_id: r.get(2)?,
                    result_ref: r.get(3)?,
                    updated_at: r.get(4)?,
                })
            })
            .ok();
        Ok(row)
    }

    /// True when the run already has any step rows — i.e. this
    /// invocation is a resume, not a first attempt.
    pub fn run_exists(&self, run_id: &str) -> Result<bool, String> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM run_steps WHERE run_id = ?1",
                params![run_id],
                |r| r.get(0),
            )
            .map_err(|e| format!("step-log count: {e}"))?;
        Ok(count > 0)
    }

    /// Mark a step pending before its first billed call. A repeat
    /// pending (a resume re-entering the step) bumps `attempts`
    /// and preserves any checkpoint already in `result_ref`.
    pub fn mark_pending(
        &self,
        run_id: &str,
        item_index: i64,
        step: &str,
        call_id: Option<&str>,
    ) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO run_steps (run_id, item_index, step, status, attempts, call_id, updated_at) \
             VALUES (?1, ?2, ?3, 'pending', 1, ?4, ?5) \
             ON CONFLICT (run_id, item_index, step) DO UPDATE SET \
               status = 'pending', attempts = attempts + 1, \
               call_id = COALESCE(?4, call_id), updated_at = ?5",
            params![run_id, item_index, step, call_id, now_epoch_s()],
        )
        .map_err(|e| format!("step-log pending: {e}"))?;
        Ok(())
    }

    /// Update the checkpoint payload of a pending step (mid-step
    /// progress: the sightings gathered so far, the latest billed
    /// call id) without changing its status.
    pub fn update_checkpoint(
        &self,
        run_id: &str,
        item_index: i64,
        step: &str,
        call_id: Option<&str>,
        result_ref: &str,
    ) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE run_steps SET call_id = COALESCE(?4, call_id), result_ref = ?5, \
             updated_at = ?6 WHERE run_id = ?1 AND item_index = ?2 AND step = ?3",
            params![run_id, item_index, step, call_id, result_ref, now_epoch_s()],
        )
        .map_err(|e| format!("step-log checkpoint: {e}"))?;
        Ok(())
    }

    fn finish(
        &self,
        run_id: &str,
        item_index: i64,
        step: &str,
        status: &str,
        call_id: Option<&str>,
        result_ref: &str,
    ) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO run_steps (run_id, item_index, step, status, attempts, call_id, result_ref, updated_at) \
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7) \
             ON CONFLICT (run_id, item_index, step) DO UPDATE SET \
               status = ?4, call_id = COALESCE(?5, call_id), result_ref = ?6, updated_at = ?7",
            params![run_id, item_index, step, status, call_id, result_ref, now_epoch_s()],
        )
        .map_err(|e| format!("step-log finish: {e}"))?;
        Ok(())
    }

    /// Mark a step complete, with its outputs in `result_ref`
    /// (JSON: asset handles, receipt ids, the item record).
    pub fn mark_complete(
        &self,
        run_id: &str,
        item_index: i64,
        step: &str,
        call_id: Option<&str>,
        result_ref: &str,
    ) -> Result<(), String> {
        self.finish(run_id, item_index, step, "complete", call_id, result_ref)
    }

    /// Mark a step failed with a typed failure (the failure JSON,
    /// including its correction text, rides in `result_ref`).
    pub fn mark_failed(
        &self,
        run_id: &str,
        item_index: i64,
        step: &str,
        result_ref: &str,
    ) -> Result<(), String> {
        self.finish(run_id, item_index, step, "failed", None, result_ref)
    }
}
