//! Spend-cap accounting across ledgers (fix pass F4, F5).
//!
//! F4: a room run points AGENTHAND_LEDGER_PATH at its own ledger.
//! That file must be a MIRROR of the run's rows, never the place the
//! cap is checked: the cap always runs against the cumulative store,
//! so two runs cannot each spend $0.90 under a $1.00 cap.
//!
//! Every test here sets AGENTHAND_DATA_DIR to a fresh temp dir, so no
//! row ever lands in a real ledger, and holds ENV_LOCK (the billing
//! layer reads its configuration from the process environment).

use agenthand::billing::billed_call;
use agenthand::ledger;
use agenthand::pricing::Usage;
use agenthand::sqlledger;
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());
static COUNTER: AtomicU32 = AtomicU32::new(0);

const ENV_VARS: [&str; 4] = [
    "AGENTHAND_DATA_DIR",
    "AGENTHAND_LEDGER",
    "AGENTHAND_LEDGER_PATH",
    "AGENTHAND_TOTAL_SPEND_CAP_USD",
];

fn tmpdir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "agenthand-billing-cap-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Clear every ledger knob, run `body`, clear them again (even on a
/// panic), so one test's settings never leak into the next.
fn with_clean_env(body: impl FnOnce()) {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for var in ENV_VARS {
        std::env::remove_var(var);
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
    for var in ENV_VARS {
        std::env::remove_var(var);
    }
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// `images` muse-image-1.0 images: $0.01 each.
fn images(n: i64) -> Usage {
    Usage {
        images: n,
        ..Default::default()
    }
}

fn cumulative_total(data_dir: &Path, store: &str) -> f64 {
    if store == "sqlite" {
        sqlledger::ledger_total_usd(&data_dir.join("ledger.sqlite3")).unwrap()
    } else {
        ledger::ledger_total_usd(&data_dir.join("ledger.jsonl")).unwrap()
    }
}

fn jsonl_rows(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        })
        .unwrap_or_default()
}

/// F4: two runs with different AGENTHAND_LEDGER_PATH under a $1.00
/// cap. The first settles $0.90; the second's $0.90 reservation is
/// refused before its call runs. Each room ledger still reads back
/// its own run's rows. Both stores: `jsonl` is what the room runner
/// selected before the fix, `sqlite` is the shipped default.
#[test]
fn room_ledgers_share_one_spend_cap_in_both_stores() {
    for store in ["jsonl", "sqlite"] {
        with_clean_env(|| {
            let dir = tmpdir(store);
            let data_dir = dir.join("data");
            std::env::set_var("AGENTHAND_DATA_DIR", &data_dir);
            std::env::set_var("AGENTHAND_LEDGER", store);
            std::env::set_var("AGENTHAND_TOTAL_SPEND_CAP_USD", "1.00");

            // Run 1: its own room ledger; settles $0.90.
            let room1 = dir.join("room1").join("ledger.jsonl");
            std::env::set_var("AGENTHAND_LEDGER_PATH", &room1);
            let summary = billed_call(
                "draw",
                Some("muse-image-1.0"),
                0.90,
                "ok",
                "",
                "run1-call",
                || Ok(images(90)),
            )
            .unwrap();
            assert!((summary.actual_usd - 0.90).abs() < 1e-9);

            // Run 2: a fresh room ledger; the $0.90 reservation must be
            // refused against the cumulative $0.90 already spent.
            let room2 = dir.join("room2").join("ledger.jsonl");
            std::env::set_var("AGENTHAND_LEDGER_PATH", &room2);
            let ran = Cell::new(false);
            let second = billed_call(
                "draw",
                Some("muse-image-1.0"),
                0.90,
                "ok",
                "",
                "run2-call",
                || {
                    ran.set(true);
                    Ok(images(90))
                },
            );
            assert!(
                second.is_err(),
                "store {store}: run 2's $0.90 reservation passed a $1.00 cap after run 1 spent $0.90"
            );
            let err = second.err().unwrap();
            assert!(err.contains("Spend cap"), "store {store}: {err}");
            assert!(!ran.get(), "store {store}: the refused call must not run");

            // The cap ledger holds run 1's spend only.
            assert!(
                (cumulative_total(&data_dir, store) - 0.90).abs() < 1e-9,
                "store {store}"
            );
            // Room totals stay readable from the room ledgers.
            let rows1 = jsonl_rows(&room1);
            assert_eq!(rows1.len(), 2, "store {store}: room 1 pending + settled");
            assert_eq!(rows1[0]["phase"], "pending");
            assert_eq!(rows1[1]["phase"], "settled");
            assert!((ledger::ledger_total_usd(&room1).unwrap() - 0.90).abs() < 1e-9);
            assert_eq!(ledger::ledger_total_usd(&room2).unwrap(), 0.0);
            let _ = std::fs::remove_dir_all(&dir);
        });
    }
}
