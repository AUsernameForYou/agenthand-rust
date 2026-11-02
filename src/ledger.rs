//! Cost receipts and the fail-closed spend cap — port of `ledger.py`.
//!
//! JSONL ledger, reserve-then-settle: a "pending" receipt at the
//! projected cost before the call, a "settled" receipt with the same
//! call_id and the actual cost after. `ledger_total_usd` counts, per
//! call_id, the settled actual when present, else the pending
//! projection; call_id-less receipts count at face value. Unparseable
//! lines, negative or non-finite amounts fail closed (raise).

use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;

/// Receipt phase: a standalone, final charge.
pub const PHASE_FINAL: &str = "final";
/// Receipt phase: a reservation written before the call.
pub const PHASE_PENDING: &str = "pending";
/// Receipt phase: a reservation settled at the actual cost after the call.
pub const PHASE_SETTLED: &str = "settled";
/// Receipt status of a settle whose actual cost pushed the ledger over
/// the spend cap (fix pass F5). The amount is recorded truthfully and
/// counts like any other settle; the status (and the overage in the
/// note) makes the breach visible in the ledger itself.
pub const STATUS_OVER_CAP: &str = "over_cap";

/// A settle-time cap breach: the ledger total after the settle, how
/// far over the cap that is, and the cap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SettleOverage {
    pub total_usd: f64,
    pub overage_usd: f64,
    pub cap_usd: f64,
}

/// The settle-time cap check (fix pass F5), shared by both stores:
/// `others_usd` is the ledger total EXCLUDING this call's own rows
/// (every other settled actual plus every other open reservation).
/// When others + actual exceeds the cap, the receipt is marked
/// `over_cap` with the overage in its note, and the breach returned;
/// the caller still records the receipt (truthfully) and then fails.
pub fn apply_settle_cap(receipt: &mut Receipt, others_usd: f64, cap_usd: f64) -> Option<SettleOverage> {
    let total = ((others_usd + receipt.usd_estimate) * 1e6).round() / 1e6;
    if total <= cap_usd {
        return None;
    }
    let overage = ((total - cap_usd) * 1e6).round() / 1e6;
    let flag = format!(
        "over_cap: settled ${:.6} brought the ledger to ${total:.6}, ${overage:.6} over the ${cap_usd:.2} cap",
        receipt.usd_estimate
    );
    receipt.status = STATUS_OVER_CAP.to_string();
    receipt.note = if receipt.note.is_empty() {
        flag
    } else {
        format!("{}; {flag}", receipt.note)
    };
    Some(SettleOverage {
        total_usd: total,
        overage_usd: overage,
        cap_usd,
    })
}

#[derive(Serialize, Clone)]
pub struct Receipt {
    pub tool: String,
    pub model: Option<String>,
    pub usd_estimate: f64,
    pub pricing_table_version: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
    pub images: i64,
    pub status: String,
    pub phase: String,
    pub call_id: Option<String>,
    pub note: String,
    pub timestamp: String,
}

impl Receipt {
    pub fn new(tool: &str, model: Option<&str>, usd_estimate: f64, phase: &str) -> Receipt {
        Receipt {
            tool: tool.to_string(),
            model: model.map(|m| m.to_string()),
            usd_estimate,
            pricing_table_version: crate::pricing::PRICING_TABLE_VERSION.to_string(),
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
            images: 0,
            status: "ok".to_string(),
            phase: phase.to_string(),
            call_id: None,
            note: String::new(),
            timestamp: utc_now_iso(),
        }
    }
}

/// ISO-8601 UTC timestamp without pulling in a date crate: format the
/// system clock via civil-date math (Howard Hinnant's algorithm).
pub fn utc_now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00",
        y,
        m,
        d,
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn validate_amount(value: f64, where_: &str) -> Result<f64, String> {
    if !value.is_finite() {
        return Err(format!("{where_}: amount {value} is not finite."));
    }
    if value < 0.0 {
        return Err(format!("{where_}: amount {value} is negative."));
    }
    Ok(value)
}

/// Append one receipt to the JSONL ledger.
pub fn append_receipt(ledger_path: &Path, receipt: &Receipt) -> Result<(), String> {
    validate_amount(
        receipt.usd_estimate,
        &format!("receipt for {}", receipt.tool),
    )?;
    if let Some(parent) = ledger_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create ledger dir: {e}"))?;
    }
    let line = serde_json::to_string(receipt).map_err(|e| e.to_string())?;
    let mut fh = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ledger_path)
        .map_err(|e| format!("cannot open ledger: {e}"))?;
    fh.write_all(line.as_bytes())
        .and_then(|_| fh.write_all(b"\n"))
        .map_err(|e| format!("cannot append to ledger: {e}"))?;
    Ok(())
}

/// The counted ledger total: settled actuals where present, pending
/// projections otherwise, rounded to the micro-dollar.
pub fn ledger_total_usd(ledger_path: &Path) -> Result<f64, String> {
    ledger_total_reduced(ledger_path, None)
}

/// The counted ledger total with every row of `call_id` left out: the
/// settle-time cap check's "everything else" (fix pass F5).
pub fn ledger_total_excluding_call_usd(ledger_path: &Path, call_id: &str) -> Result<f64, String> {
    ledger_total_reduced(ledger_path, Some(call_id))
}

fn ledger_total_reduced(ledger_path: &Path, exclude_call: Option<&str>) -> Result<f64, String> {
    if !ledger_path.exists() {
        return Ok(0.0);
    }
    let text =
        std::fs::read_to_string(ledger_path).map_err(|e| format!("cannot read ledger: {e}"))?;
    let mut standalone = 0.0f64;
    let mut calls: HashMap<String, f64> = HashMap::new();
    let mut settled: HashSet<String> = HashSet::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let where_ = format!("Ledger {} line {}", ledger_path.display(), i + 1);
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
        let call_id = record
            .get("call_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        let phase = record
            .get("phase")
            .and_then(|v| v.as_str())
            .unwrap_or(PHASE_FINAL);
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

/// Fail-closed cap check: returns the current total, or an error when
/// total + projected would exceed the cap.
pub fn check_spend_cap(
    ledger_path: &Path,
    projected_usd: f64,
    cap_usd: f64,
) -> Result<f64, String> {
    let projected = validate_amount(projected_usd, "projected cost")?;
    let cap = validate_amount(cap_usd, "spend cap")?;
    let total = ledger_total_usd(ledger_path)?;
    if total + projected > cap {
        return Err(format!(
            "Spend cap: ledger total ${total:.2} + projected ${projected:.4} would exceed the ${cap:.2} cap. Call refused before it ran."
        ));
    }
    Ok(total)
}
