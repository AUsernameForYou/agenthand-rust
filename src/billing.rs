//! The billing choke point — port of `billed_call` in the Python
//! `meta_client.py`: reserve-then-settle under a file lock, so two
//! processes cannot both pass the gate on the same stale total.
//! Reservations are written before the call runs; a call that raises
//! leaves its reservation counted (money that may have been spent is
//! treated as spent).
//!
//! One cap, all spend (fix pass F4): every reservation and settle goes
//! to the CUMULATIVE store (`config::get_ledger_store`), and the cap is
//! checked there. When `AGENTHAND_LEDGER_PATH` names a mirror ledger
//! (the room runner's `<room>/ledger.jsonl`), each row is ALSO
//! appended to that JSONL file, so a room's own spend stays readable
//! from its directory without the room ever owning the cap.
//!
//! Settle is capped too (fix pass F5): a reservation is a projection,
//! and a call can cost more than projected. At settle the store
//! recomputes everything else in the ledger (settled actuals plus
//! other open reservations, excluding this call's own reservation)
//! plus this call's actual cost. Over the cap, the settle is STILL
//! recorded truthfully, marked `over_cap` with the overage, and
//! `billed_call` returns an error naming the breach; every later
//! reservation is then refused because the ledger total exceeds the
//! cap.

use crate::config;
use crate::ledger::{self, Receipt, PHASE_PENDING, PHASE_SETTLED};
use crate::pricing::{self, Usage};
use fs2::FileExt;
use serde_json::json;
use std::io::Write;

/// One billed call's outcome: its ids, projected vs actual cost, and
/// the provider usage the cost rests on.
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
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("cannot create data dir: {e}"))?;
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

/// The mirror ledger for this call, if any: `AGENTHAND_LEDGER_PATH`,
/// unless it names the cumulative store's own file (a row is never
/// written twice to one ledger).
fn mirror_ledger(store: &config::LedgerStore) -> Option<std::path::PathBuf> {
    let mirror = config::get_mirror_ledger_path()?;
    let store_path = match store {
        config::LedgerStore::Jsonl(p) | config::LedgerStore::Sqlite(p) => p,
    };
    let same = &mirror == store_path
        || matches!(
            (mirror.canonicalize(), store_path.canonicalize()),
            (Ok(a), Ok(b)) if a == b
        );
    if same {
        None
    } else {
        Some(mirror)
    }
}

/// Why [`billed_call_detailed`] returned no summary, and what the
/// cumulative ledger counts for the call. The split matters to a
/// caller that reports cost: a refusal before the call counted
/// nothing; a call that succeeded was made and paid for even when
/// billing stopped afterwards (fix pass F5).
#[derive(Debug, Clone, PartialEq)]
pub enum BillingFailure {
    /// Stopped before the call ran (pricing table, projection, cap
    /// configuration, ledger state, or the spend cap itself). Nothing
    /// was called. `reserved_usd` is 0 unless the reservation row was
    /// already written when a later step failed (a mirror write).
    BeforeCall { message: String, reserved_usd: f64 },
    /// The call itself returned an error; its reservation stays
    /// counted (fail-closed: money that may have been spent is spent).
    CallFailed { message: String, reserved_usd: f64 },
    /// The call SUCCEEDED, so it was made and paid for, but billing
    /// stopped after it: the settle went over the cap (F5, recorded
    /// truthfully as `over_cap`), or pricing the usage or writing the
    /// settle failed (the reservation then stays counted).
    AfterCall {
        message: String,
        /// What the cumulative ledger counts for this call: the actual
        /// cost once the settle row is written, else the reservation.
        counted_usd: f64,
        /// The priced actual cost, when pricing succeeded.
        actual_usd: Option<f64>,
        /// The settle-time cap breach, when that is why billing stopped.
        overage: Option<ledger::SettleOverage>,
        usage: Usage,
    },
}

impl BillingFailure {
    /// The error text `billed_call` has always returned.
    pub fn message(&self) -> &str {
        match self {
            BillingFailure::BeforeCall { message, .. }
            | BillingFailure::CallFailed { message, .. }
            | BillingFailure::AfterCall { message, .. } => message,
        }
    }

    pub fn into_message(self) -> String {
        match self {
            BillingFailure::BeforeCall { message, .. }
            | BillingFailure::CallFailed { message, .. }
            | BillingFailure::AfterCall { message, .. } => message,
        }
    }
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
    billed_call_detailed(tool, model, projected_usd, status, note, call_id, f)
        .map_err(BillingFailure::into_message)
}

/// [`billed_call`] with the failure classified: before the call,
/// the call itself, or after a call that succeeded. Same ledger
/// behaviour and the same error text.
pub fn billed_call_detailed<F>(
    tool: &str,
    model: Option<&str>,
    projected_usd: f64,
    status: &str,
    note: &str,
    call_id: &str,
    f: F,
) -> Result<BillingSummary, BillingFailure>
where
    F: FnOnce() -> Result<Usage, String>,
{
    let before = |message: String, reserved_usd: f64| BillingFailure::BeforeCall {
        message,
        reserved_usd,
    };
    if let Some(model) = model {
        if !pricing::is_known_model(model) {
            return Err(before(
                format!(
                    "Model {model:?} is not in the pricing table; refusing to run an unpriced billable call."
                ),
                0.0,
            ));
        }
    }
    if !projected_usd.is_finite() || projected_usd < 0.0 {
        return Err(before(
            format!("projected cost: amount {projected_usd} is not a finite non-negative number."),
            0.0,
        ));
    }
    let cap_usd = config::get_total_spend_cap_usd().map_err(|m| before(m, 0.0))?;
    let call_id = call_id.to_string();
    let store = config::get_ledger_store().map_err(|m| before(m, 0.0))?;
    let mirror = mirror_ledger(&store);

    // Reserve: check + append pending, atomically — under the file
    // lock for the JSONL store, inside one IMMEDIATE transaction for
    // the SQLite store. Either way the billable call below runs
    // WITHOUT the gate held. Once the cumulative row is written, a
    // later failure (the mirror) leaves the projection counted.
    {
        let mut receipt = Receipt::new(tool, model, projected_usd, PHASE_PENDING);
        receipt.status = status.to_string();
        receipt.note = note.to_string();
        receipt.call_id = Some(call_id.clone());
        let unreserved = |m: String| before(m, 0.0);
        let reserved = |m: String| before(m, projected_usd);
        match &store {
            config::LedgerStore::Jsonl(ledger_path) => {
                let _lock = lock_file().map_err(unreserved)?;
                ledger::check_spend_cap(ledger_path, projected_usd, cap_usd).map_err(unreserved)?;
                ledger::append_receipt(ledger_path, &receipt).map_err(unreserved)?;
                if let Some(mirror) = &mirror {
                    ledger::append_receipt(mirror, &receipt).map_err(reserved)?;
                }
            }
            config::LedgerStore::Sqlite(ledger_path) => {
                crate::sqlledger::reserve(ledger_path, &receipt, cap_usd).map_err(unreserved)?;
                if let Some(mirror) = &mirror {
                    let _lock = lock_file().map_err(reserved)?;
                    ledger::append_receipt(mirror, &receipt).map_err(reserved)?;
                }
            }
        }
    }

    // Call. If this errors, the reservation stays counted; no settle.
    let usage = f().map_err(|message| BillingFailure::CallFailed {
        message,
        reserved_usd: projected_usd,
    })?;

    // From here on the call has happened: every failure is AfterCall.
    let after = |message: String,
                 counted_usd: f64,
                 actual_usd: Option<f64>,
                 overage: Option<ledger::SettleOverage>,
                 usage: &Usage| BillingFailure::AfterCall {
        message,
        counted_usd,
        actual_usd,
        overage,
        usage: *usage,
    };
    let actual_usd = match model {
        Some(model) => pricing::estimate_cost_usd(model, &usage)
            .map_err(|m| after(m, projected_usd, None, None, &usage))?,
        None => 0.0,
    };

    // Settle with the actual cost and the usage counts, cap-checked.
    // Until the cumulative settle row is written the reservation is
    // what the ledger counts; after it, the actual cost.
    let overage = {
        let mut receipt = Receipt::new(tool, model, actual_usd, PHASE_SETTLED);
        receipt.status = status.to_string();
        receipt.note = note.to_string();
        receipt.call_id = Some(call_id.clone());
        receipt.input_tokens = usage.input_tokens;
        receipt.output_tokens = usage.output_tokens;
        receipt.cached_input_tokens = usage.cached_input_tokens;
        receipt.images = usage.images;
        let unsettled = |m: String| after(m, projected_usd, Some(actual_usd), None, &usage);
        match &store {
            config::LedgerStore::Jsonl(ledger_path) => {
                let _lock = lock_file().map_err(unsettled)?;
                let others = ledger::ledger_total_excluding_call_usd(ledger_path, &call_id)
                    .map_err(unsettled)?;
                let overage = ledger::apply_settle_cap(&mut receipt, others, cap_usd);
                ledger::append_receipt(ledger_path, &receipt).map_err(unsettled)?;
                if let Some(mirror) = &mirror {
                    ledger::append_receipt(mirror, &receipt)
                        .map_err(|m| after(m, actual_usd, Some(actual_usd), overage, &usage))?;
                }
                // Keep the lock file from looking stale to other tools.
                let _ = std::io::stdout().flush();
                overage
            }
            config::LedgerStore::Sqlite(ledger_path) => {
                let overage = crate::sqlledger::settle(ledger_path, &mut receipt, cap_usd)
                    .map_err(unsettled)?;
                if let Some(mirror) = &mirror {
                    let settled =
                        |m: String| after(m, actual_usd, Some(actual_usd), overage, &usage);
                    let _lock = lock_file().map_err(settled)?;
                    ledger::append_receipt(mirror, &receipt).map_err(settled)?;
                }
                overage
            }
        }
    };
    if let Some(over) = overage {
        return Err(after(
            format!(
                "SPEND CAP EXCEEDED at settle: {tool} call {call_id} reserved ${projected_usd:.6} \
                 but actually cost ${actual_usd:.6}, bringing the ledger to ${:.6}, ${:.6} over \
                 the ${:.2} cap. The settle is recorded truthfully with status \"{}\"; every \
                 further billable call will be refused until the cap is raised.",
                over.total_usd,
                over.overage_usd,
                over.cap_usd,
                ledger::STATUS_OVER_CAP
            ),
            actual_usd,
            Some(actual_usd),
            Some(over),
            &usage,
        ));
    }

    Ok(BillingSummary {
        call_id,
        projected_usd,
        actual_usd,
        usage,
    })
}
