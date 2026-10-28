//! The billing choke point — port of `billed_call` in the Python
//! `meta_client.py`: reserve-then-settle under a file lock, so two
//! processes cannot both pass the gate on the same stale total.
//! Reservations are written before the call runs; a call that raises
//! leaves its reservation counted (money that may have been spent is
//! treated as spent).

use crate::config;
use crate::ledger::{self, Receipt, PHASE_PENDING, PHASE_SETTLED};
use crate::pricing::{self, Usage};
use fs2::FileExt;
use serde_json::json;
use std::io::Write;

pub struct BillingSummary {
    pub call_id: String,
    pub projected_usd: f64,
    pub actual_usd: f64,
    pub usage: Usage,
}

impl BillingSummary {
    pub fn to_json(&self) -> serde_json::Value {
        json!({
            "call_id": self.call_id,
            "projected_usd": self.projected_usd,
            "actual_usd": self.actual_usd,
            "usage": {
                "input_tokens": self.usage.input_tokens,
                "output_tokens": self.usage.output_tokens,
                "cached_input_tokens": self.usage.cached_input_tokens,
                "images": self.usage.images,
            },
        })
    }
}

fn lock_file() -> Result<std::fs::File, String> {
    let data_dir = config::get_data_dir();
    std::fs::create_dir_all(&data_dir)
        .map_err(|e| format!("cannot create data dir: {e}"))?;
    let fh = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(data_dir.join("ledger.lock"))
        .map_err(|e| format!("cannot open ledger lock: {e}"))?;
    fh.lock_exclusive()
        .map_err(|e| format!("cannot lock ledger: {e}"))?;
    Ok(fh)
}

/// Run one billable call under the spend cap. `f` performs the call
/// and returns its usage; errors propagate with the reservation left
/// counted. The caller supplies the call_id (the Python passes it via
/// a context var so nested code can name artifacts after the ledger
/// call; here it is explicit).
pub fn billed_call<F>(
    tool: &str,
    model: Option<&str>,
    projected_usd: f64,
    status: &str,
    note: &str,
    call_id: &str,
    f: F,
) -> Result<BillingSummary, String>
where
    F: FnOnce() -> Result<Usage, String>,
{
    if let Some(model) = model {
        if !pricing::is_known_model(model) {
            return Err(format!(
                "Model {model:?} is not in the pricing table; refusing to run an unpriced billable call."
            ));
        }
    }
    if !projected_usd.is_finite() || projected_usd < 0.0 {
        return Err(format!(
            "projected cost: amount {projected_usd} is not a finite non-negative number."
        ));
    }
    let cap_usd = config::get_total_spend_cap_usd()?;
    let call_id = call_id.to_string();
    let store = config::get_ledger_store()?;

    // Reserve: check + append pending, atomically — under the file
    // lock for the JSONL store, inside one IMMEDIATE transaction for
    // the SQLite store. Either way the billable call below runs
    // WITHOUT the gate held.
    {
        let mut receipt = Receipt::new(tool, model, projected_usd, PHASE_PENDING);
        receipt.status = status.to_string();
        receipt.note = note.to_string();
        receipt.call_id = Some(call_id.clone());
        match &store {
            config::LedgerStore::Jsonl(ledger_path) => {
                let _lock = lock_file()?;
                ledger::check_spend_cap(ledger_path, projected_usd, cap_usd)?;
                ledger::append_receipt(ledger_path, &receipt)?;
            }
            config::LedgerStore::Sqlite(ledger_path) => {
                crate::sqlledger::reserve(ledger_path, &receipt, cap_usd)?;
            }
        }
    }

    // Call. If this errors, the reservation stays counted; no settle.
    let usage = f()?;

    let actual_usd = match model {
        Some(model) => pricing::estimate_cost_usd(model, &usage)?,
        None => 0.0,
    };

    // Settle with the actual cost and the usage counts.
    {
        let mut receipt = Receipt::new(tool, model, actual_usd, PHASE_SETTLED);
        receipt.status = status.to_string();
        receipt.note = note.to_string();
        receipt.call_id = Some(call_id.clone());
        receipt.input_tokens = usage.input_tokens;
        receipt.output_tokens = usage.output_tokens;
        receipt.cached_input_tokens = usage.cached_input_tokens;
        receipt.images = usage.images;
        match &store {
            config::LedgerStore::Jsonl(ledger_path) => {
                let _lock = lock_file()?;
                ledger::append_receipt(ledger_path, &receipt)?;
                // Keep the lock file from looking stale to other tools.
                let _ = std::io::stdout().flush();
            }
            config::LedgerStore::Sqlite(ledger_path) => {
                crate::sqlledger::append_receipt(ledger_path, &receipt)?;
            }
        }
    }

    Ok(BillingSummary {
        call_id,
        projected_usd,
        actual_usd,
        usage,
    })
}
