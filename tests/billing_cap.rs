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

/// Every receipt row of `call_id` in the store: (phase, status,
/// usd_estimate, note).
fn store_rows_for(
    data_dir: &Path,
    store: &str,
    call_id: &str,
) -> Vec<(String, String, f64, String)> {
    if store == "sqlite" {
        let conn = rusqlite::Connection::open(data_dir.join("ledger.sqlite3")).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT phase, status, usd_estimate, note FROM receipts \
                 WHERE call_id = ?1 ORDER BY id ASC",
            )
            .unwrap();
        let rows = stmt
            .query_map([call_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        rows
    } else {
        jsonl_rows(&data_dir.join("ledger.jsonl"))
            .into_iter()
            .filter(|r| r["call_id"] == call_id)
            .map(|r| {
                (
                    r["phase"].as_str().unwrap().to_string(),
                    r["status"].as_str().unwrap().to_string(),
                    r["usd_estimate"].as_f64().unwrap(),
                    r["note"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }
}

/// F5: settle is cap-checked. A call reserved at $0.01 that actually
/// costs $4.25125 under a $1.00 cap: the settle is recorded
/// truthfully (status over_cap, the overage in the note),
/// billed_call returns an error naming the breach, and the next
/// reservation is refused. Both stores.
#[test]
fn settle_over_the_cap_is_recorded_and_fails_loudly_in_both_stores() {
    for store in ["jsonl", "sqlite"] {
        with_clean_env(|| {
            let dir = tmpdir(&format!("settle-{store}"));
            let data_dir = dir.join("data");
            std::env::set_var("AGENTHAND_DATA_DIR", &data_dir);
            std::env::set_var("AGENTHAND_LEDGER", store);
            std::env::set_var("AGENTHAND_TOTAL_SPEND_CAP_USD", "1.00");

            // 1000 in + 1,000,000 out on muse-spark-1.3:
            // 1000 x $1.25/M + 1,000,000 x $4.25/M = $4.25125.
            let settled = billed_call(
                "identify",
                Some("muse-spark-1.3"),
                0.01,
                "ok",
                "",
                "underprojected",
                || {
                    Ok(Usage {
                        input_tokens: 1000,
                        output_tokens: 1_000_000,
                        ..Default::default()
                    })
                },
            );
            assert!(
                settled.is_err(),
                "store {store}: a $4.25125 settle under a $1.00 cap returned Ok"
            );
            let err = settled.err().unwrap();
            assert!(err.contains("SPEND CAP EXCEEDED"), "store {store}: {err}");
            assert!(err.contains("4.251250"), "store {store}: {err}");
            assert!(err.contains("over the $1.00 cap"), "store {store}: {err}");

            // The settle row is present, truthful, and marked over_cap.
            let rows = store_rows_for(&data_dir, store, "underprojected");
            assert_eq!(rows.len(), 2, "store {store}: pending + settled, got {rows:?}");
            let (phase, status, usd, note) = &rows[1];
            assert_eq!(phase, "settled");
            assert_eq!(status, "over_cap", "store {store}");
            assert!((usd - 4.25125).abs() < 1e-9, "store {store}: {usd}");
            assert!(note.contains("3.251250 over the $1.00 cap"), "store {store}: {note}");
            assert!((cumulative_total(&data_dir, store) - 4.25125).abs() < 1e-9);

            // Every later reservation is refused, before its call runs.
            let ran = Cell::new(false);
            let next = billed_call(
                "cut_out",
                Some("sam-3.1"),
                0.0025,
                "ok",
                "",
                "after-breach",
                || {
                    ran.set(true);
                    Ok(Usage {
                        images: 1,
                        ..Default::default()
                    })
                },
            );
            let err = next.err().expect("a reservation after the breach must be refused");
            assert!(err.contains("Spend cap"), "store {store}: {err}");
            assert!(!ran.get());
            let _ = std::fs::remove_dir_all(&dir);
        });
    }
}

/// F5: the settle check counts OTHER open reservations but not the
/// call's own: $0.50 reserved elsewhere + a $0.40 call settling at
/// $0.45 stays under $1.00 and settles cleanly.
#[test]
fn settle_within_the_cap_excludes_its_own_reservation() {
    with_clean_env(|| {
        let dir = tmpdir("settle-ok");
        let data_dir = dir.join("data");
        std::env::set_var("AGENTHAND_DATA_DIR", &data_dir);
        std::env::set_var("AGENTHAND_LEDGER", "jsonl");
        std::env::set_var("AGENTHAND_TOTAL_SPEND_CAP_USD", "1.00");
        // An open reservation from a call that errored ($0.50 stays counted).
        let _ = billed_call("draw", Some("muse-image-1.0"), 0.50, "ok", "", "open", || {
            Err("simulated failure".to_string())
        });
        let ok = billed_call("draw", Some("muse-image-1.0"), 0.40, "ok", "", "fits", || {
            Ok(images(45))
        })
        .unwrap();
        assert!((ok.actual_usd - 0.45).abs() < 1e-9);
        let rows = store_rows_for(&data_dir, "jsonl", "fits");
        assert_eq!(rows[1].1, "ok");
        assert!((cumulative_total(&data_dir, "jsonl") - 0.95).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    });
}
