//! rusqlite spike tests: accounting parity with the JSONL store,
//! the frozen-run import ($0.020044), fail-closed behaviour,
//! env-selected `billed_call`, and the concurrency proof
//! (cap integrity under parallel item pipelines + wall-clock table).
//!
//! The wall-clock harness uses 8 items x 2 s simulated latency and
//! takes ~36 s across both stores; it is `#[ignore]`d so the default
//! suite stays fast. Run it with:
//!   cargo test --release --test sqlite_ledger -- --ignored --nocapture

use agenthand::billing::billed_call;
use agenthand::ledger::{self, Receipt};
use agenthand::pricing::Usage;
use agenthand::sqlledger;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Env is process-global; every test that mutates it holds this lock
/// for its whole body so env-using tests never interleave.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agenthand-sqlite-test-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn receipt(tool: &str, usd: f64, phase: &str, call_id: Option<&str>) -> Receipt {
    let mut r = Receipt::new(tool, Some("sam-3.1"), usd, phase);
    r.call_id = call_id.map(|s| s.to_string());
    r
}

/// A JSONL ledger exercising every reduction rule: settled
/// supersedes pending, first pending wins, a later settled wins over
/// an earlier settled, pending-only counts at face, call_id-less and
/// non-pending/settled phases count standalone.
const TRICKY_LEDGER: &str = "\
{\"tool\":\"identify\",\"model\":\"muse-spark-1.3\",\"usd_estimate\":0.10,\"pricing_table_version\":\"v\",\"input_tokens\":0,\"output_tokens\":0,\"cached_input_tokens\":0,\"images\":0,\"status\":\"ok\",\"phase\":\"pending\",\"call_id\":\"a\",\"note\":\"\",\"timestamp\":\"t\"}
{\"tool\":\"identify\",\"model\":\"muse-spark-1.3\",\"usd_estimate\":0.04,\"pricing_table_version\":\"v\",\"input_tokens\":0,\"output_tokens\":0,\"cached_input_tokens\":0,\"images\":0,\"status\":\"ok\",\"phase\":\"settled\",\"call_id\":\"a\",\"note\":\"\",\"timestamp\":\"t\"}
{\"tool\":\"cut_out\",\"model\":\"sam-3.1\",\"usd_estimate\":0.005,\"pricing_table_version\":\"v\",\"input_tokens\":0,\"output_tokens\":0,\"cached_input_tokens\":0,\"images\":0,\"status\":\"ok\",\"phase\":\"pending\",\"call_id\":\"b\",\"note\":\"\",\"timestamp\":\"t\"}
{\"tool\":\"cut_out\",\"model\":\"sam-3.1\",\"usd_estimate\":0.007,\"pricing_table_version\":\"v\",\"input_tokens\":0,\"output_tokens\":0,\"cached_input_tokens\":0,\"images\":0,\"status\":\"ok\",\"phase\":\"pending\",\"call_id\":\"b\",\"note\":\"\",\"timestamp\":\"t\"}
{\"tool\":\"draw\",\"model\":\"muse-image-1.0\",\"usd_estimate\":0.01,\"pricing_table_version\":\"v\",\"input_tokens\":0,\"output_tokens\":0,\"cached_input_tokens\":0,\"images\":0,\"status\":\"ok\",\"phase\":\"settled\",\"call_id\":\"c\",\"note\":\"\",\"timestamp\":\"t\"}
{\"tool\":\"draw\",\"model\":\"muse-image-1.0\",\"usd_estimate\":0.02,\"pricing_table_version\":\"v\",\"input_tokens\":0,\"output_tokens\":0,\"cached_input_tokens\":0,\"images\":0,\"status\":\"ok\",\"phase\":\"settled\",\"call_id\":\"c\",\"note\":\"\",\"timestamp\":\"t\"}
{\"tool\":\"identify\",\"model\":null,\"usd_estimate\":0.003,\"pricing_table_version\":\"v\",\"input_tokens\":0,\"output_tokens\":0,\"cached_input_tokens\":0,\"images\":0,\"status\":\"ok\",\"phase\":\"final\",\"call_id\":null,\"note\":\"\",\"timestamp\":\"t\"}
{\"tool\":\"identify\",\"model\":null,\"usd_estimate\":0.006,\"pricing_table_version\":\"v\",\"input_tokens\":0,\"output_tokens\":0,\"cached_input_tokens\":0,\"images\":0,\"status\":\"ok\",\"phase\":\"final\",\"call_id\":\"d\",\"note\":\"\",\"timestamp\":\"t\"}
{\"tool\":\"identify\",\"model\":null,\"usd_estimate\":0.001,\"pricing_table_version\":\"v\",\"input_tokens\":0,\"output_tokens\":0,\"cached_input_tokens\":0,\"images\":0,\"status\":\"ok\",\"phase\":\"pending\",\"call_id\":\"\",\"note\":\"\",\"timestamp\":\"t\"}
";
// Expected: a=0.04 (settled), b=0.005 (first pending), c=0.02 (last
// settled), standalone 0.003 + 0.006 + 0.001 => total 0.075.

#[test]
fn sqlite_total_matches_jsonl_on_tricky_ledger() {
    let dir = tmpdir("tricky");
    let jsonl = dir.join("ledger.jsonl");
    std::fs::write(&jsonl, TRICKY_LEDGER).unwrap();
    let jsonl_total = ledger::ledger_total_usd(&jsonl).unwrap();
    assert!(
        (jsonl_total - 0.075).abs() < 1e-9,
        "jsonl total {jsonl_total}"
    );

    let sqlite = dir.join("ledger.sqlite3");
    let (rows, sqlite_total) = sqlledger::import_jsonl(&jsonl, &sqlite).unwrap();
    assert_eq!(rows, 9);
    assert!(
        (sqlite_total - jsonl_total).abs() < 1e-9,
        "sqlite {sqlite_total} vs jsonl {jsonl_total}"
    );
    assert_eq!((sqlite_total * 1e6).round(), 75000.0);
    // Re-reading through the standalone total agrees.
    let reread = sqlledger::ledger_total_usd(&sqlite).unwrap();
    assert_eq!(reread, sqlite_total);
    let _ = std::fs::remove_dir_all(&dir);
}

const FROZEN_LEDGER: &str = "\
{\"tool\":\"identify\",\"model\":\"muse-spark-1.3\",\"usd_estimate\":0.024594,\"pricing_table_version\":\"2026-10-26.2\",\"input_tokens\":0,\"output_tokens\":0,\"cached_input_tokens\":0,\"images\":0,\"status\":\"ok\",\"phase\":\"pending\",\"call_id\":\"0b2f6e0953374858b4a554438944c1f1\",\"note\":\"\",\"timestamp\":\"2026-10-26T23:13:47.484718+00:00\"}
{\"tool\":\"identify\",\"model\":\"muse-spark-1.3\",\"usd_estimate\":0.007544,\"pricing_table_version\":\"2026-10-26.2\",\"input_tokens\":3159,\"output_tokens\":1638,\"cached_input_tokens\":3060,\"images\":0,\"status\":\"ok\",\"phase\":\"settled\",\"call_id\":\"0b2f6e0953374858b4a554438944c1f1\",\"note\":\"\",\"timestamp\":\"2026-10-26T23:13:56.563356+00:00\"}
{\"tool\":\"cut_out\",\"model\":\"sam-3.1\",\"usd_estimate\":0.005,\"pricing_table_version\":\"2026-10-26.2\",\"input_tokens\":0,\"output_tokens\":0,\"cached_input_tokens\":0,\"images\":0,\"status\":\"ok\",\"phase\":\"pending\",\"call_id\":\"bbdb8433ad30497b8e05d7baae490fcb\",\"note\":\"\",\"timestamp\":\"2026-10-26T23:13:56.566132+00:00\"}
{\"tool\":\"cut_out\",\"model\":\"sam-3.1\",\"usd_estimate\":0.0025,\"pricing_table_version\":\"2026-10-26.2\",\"input_tokens\":0,\"output_tokens\":0,\"cached_input_tokens\":0,\"images\":1,\"status\":\"ok\",\"phase\":\"settled\",\"call_id\":\"bbdb8433ad30497b8e05d7baae490fcb\",\"note\":\"\",\"timestamp\":\"2026-10-26T23:13:56.566132+00:00\"}
{\"tool\":\"draw\",\"model\":\"muse-image-1.0\",\"usd_estimate\":0.01,\"pricing_table_version\":\"2026-10-26.2\",\"input_tokens\":0,\"output_tokens\":0,\"cached_input_tokens\":0,\"images\":0,\"status\":\"ok\",\"phase\":\"pending\",\"call_id\":\"85cb0449d4eb47e2a272525ae04f5302\",\"note\":\"\",\"timestamp\":\"2026-10-26T23:14:08.384798+00:00\"}
{\"tool\":\"draw\",\"model\":\"muse-image-1.0\",\"usd_estimate\":0.01,\"pricing_table_version\":\"2026-10-26.2\",\"input_tokens\":10242,\"output_tokens\":1019,\"cached_input_tokens\":0,\"images\":1,\"status\":\"ok\",\"phase\":\"settled\",\"call_id\":\"85cb0449d4eb47e2a272525ae04f5302\",\"note\":\"\",\"timestamp\":\"2026-10-26T23:14:21.621972+00:00\"}
";

#[test]
fn frozen_ledger_import_reproduces_the_frozen_total() {
    let dir = tmpdir("frozen");
    let jsonl = dir.join("ledger.jsonl");
    std::fs::write(&jsonl, FROZEN_LEDGER).unwrap();
    let sqlite = dir.join("ledger.sqlite3");
    let (rows, total) = sqlledger::import_jsonl(&jsonl, &sqlite).unwrap();
    assert_eq!(rows, 6);
    // The frozen proof accounting: $0.020044, to the micro-dollar.
    assert_eq!((total * 1e6).round(), 20044.0, "total {total}");
    assert!((total - 0.020044).abs() < 1e-9, "total {total}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sqlite_reserve_settle_and_cap_match_jsonl() {
    let dir = tmpdir("reserve");
    let sqlite = dir.join("ledger.sqlite3");
    let jsonl = dir.join("ledger.jsonl");

    // Reserve 0.005 against a 0.012 cap in both stores.
    let pending = receipt("cut_out", 0.005, ledger::PHASE_PENDING, Some("x"));
    sqlledger::reserve(&sqlite, &pending, 0.012).unwrap();
    {
        // JSONL equivalent flow: cap check + append under the same numbers.
        ledger::check_spend_cap(&jsonl, 0.005, 0.012).unwrap();
        ledger::append_receipt(&jsonl, &pending).unwrap();
    }
    // The unsettled reservation counts at face in both.
    assert!((sqlledger::ledger_total_usd(&sqlite).unwrap() - 0.005).abs() < 1e-9);
    assert!((ledger::ledger_total_usd(&jsonl).unwrap() - 0.005).abs() < 1e-9);

    // A second reserve of 0.008 would exceed the cap in both stores,
    // with the identical error message.
    let pending2 = receipt("cut_out", 0.008, ledger::PHASE_PENDING, Some("y"));
    let sqlite_err = sqlledger::reserve(&sqlite, &pending2, 0.012).unwrap_err();
    let jsonl_err = ledger::check_spend_cap(&jsonl, 0.008, 0.012).unwrap_err();
    assert_eq!(sqlite_err, jsonl_err, "cap error messages diverge");

    // Settle x at 0.0025: the settled actual supersedes the reserve.
    let settled = receipt("cut_out", 0.0025, ledger::PHASE_SETTLED, Some("x"));
    sqlledger::append_receipt(&sqlite, &settled).unwrap();
    assert!((sqlledger::ledger_total_usd(&sqlite).unwrap() - 0.0025).abs() < 1e-9);
    assert_eq!(sqlledger::count_receipts(&sqlite).unwrap(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sqlite_fails_closed_on_bad_amounts() {
    let dir = tmpdir("failclosed");
    let sqlite = dir.join("ledger.sqlite3");
    let neg = receipt("cut_out", -0.005, ledger::PHASE_PENDING, Some("x"));
    assert!(sqlledger::append_receipt(&sqlite, &neg)
        .unwrap_err()
        .contains("negative"));
    let nan = receipt("cut_out", f64::NAN, ledger::PHASE_PENDING, Some("x"));
    assert!(sqlledger::append_receipt(&sqlite, &nan)
        .unwrap_err()
        .contains("not finite"));
    assert!(sqlledger::reserve(&sqlite, &neg, 1.0).is_err());
    assert!(sqlledger::check_spend_cap(&sqlite, f64::INFINITY, 1.0).is_err());

    // Import refuses a corrupt JSONL line, exactly like reading it.
    let bad = dir.join("bad.jsonl");
    std::fs::write(&bad, "{\"tool\":\"x\",\"usd_estimate\":0.1}\nnot json\n").unwrap();
    assert!(sqlledger::import_jsonl(&bad, &dir.join("bad.sqlite3")).is_err());
    let missing = dir.join("missing.jsonl");
    std::fs::write(&missing, "{\"tool\":\"x\"}\n").unwrap();
    assert!(sqlledger::import_jsonl(&missing, &dir.join("missing.sqlite3")).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

fn sam_usage() -> Usage {
    Usage {
        input_tokens: 0,
        output_tokens: 0,
        cached_input_tokens: 0,
        images: 1,
    }
}

#[test]
fn billed_call_with_sqlite_selected_via_env() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tmpdir("envsqlite");
    std::env::set_var("AGENTHAND_DATA_DIR", &dir);
    std::env::set_var("AGENTHAND_LEDGER", "sqlite");
    std::env::remove_var("AGENTHAND_TOTAL_SPEND_CAP_USD");

    let summary = billed_call(
        "cut_out",
        Some("sam-3.1"),
        0.005,
        "ok",
        "",
        "env-call-1",
        || Ok(sam_usage()),
    )
    .unwrap();
    assert!((summary.actual_usd - 0.0025).abs() < 1e-12);

    let sqlite = dir.join("ledger.sqlite3");
    assert!(sqlite.exists(), "sqlite ledger was not created");
    assert!(
        !dir.join("ledger.jsonl").exists(),
        "JSONL ledger must not be written when sqlite is selected"
    );
    assert!((sqlledger::ledger_total_usd(&sqlite).unwrap() - 0.0025).abs() < 1e-9);

    // A call whose body errors leaves its reservation counted.
    let err = match billed_call(
        "cut_out",
        Some("sam-3.1"),
        0.005,
        "ok",
        "",
        "env-call-2",
        || Err("simulated API failure".to_string()),
    ) {
        Ok(_) => panic!("erroring call must not succeed"),
        Err(e) => e,
    };
    assert_eq!(err, "simulated API failure");
    // 0.0025 settled + 0.005 still-reserved = 0.0075.
    assert!((sqlledger::ledger_total_usd(&sqlite).unwrap() - 0.0075).abs() < 1e-9);

    std::env::remove_var("AGENTHAND_LEDGER");
    std::env::remove_var("AGENTHAND_DATA_DIR");
    let _ = std::fs::remove_dir_all(&dir);
}

/// One pipeline item: reserve, sleep out the simulated API latency
/// (the gate is NOT held during the call in either store), settle.
fn run_item(i: usize, latency: Duration) -> Result<(), String> {
    billed_call(
        "cut_out",
        Some("sam-3.1"),
        0.0025,
        "ok",
        "",
        &format!("item-{i}"),
        || {
            std::thread::sleep(latency);
            Ok(sam_usage())
        },
    )
    .map(|_| ())
}

fn store_total(dir: &PathBuf, sqlite: bool) -> f64 {
    if sqlite {
        sqlledger::ledger_total_usd(&dir.join("ledger.sqlite3")).unwrap()
    } else {
        ledger::ledger_total_usd(&dir.join("ledger.jsonl")).unwrap()
    }
}

fn store_rows(dir: &PathBuf, sqlite: bool) -> usize {
    if sqlite {
        sqlledger::count_receipts(&dir.join("ledger.sqlite3")).unwrap()
    } else {
        std::fs::read_to_string(dir.join("ledger.jsonl"))
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count()
    }
}

/// The upgrade claim, proven: 8 parallel items against ONE ledger
/// with a $0.0125 cap and $0.0025 per item — exactly 5 may reserve,
/// 3 must be refused, and the counted total is exactly $0.0125 with
/// exactly 10 receipts (5 pending + 5 settled): no cap breach, no
/// lost settle, no double settle. Runs against both stores.
#[test]
fn cap_holds_under_parallel_items_in_both_stores() {
    let _guard = ENV_LOCK.lock().unwrap();
    for sqlite in [false, true] {
        let dir = tmpdir(if sqlite { "capsqlite" } else { "capjsonl" });
        std::env::set_var("AGENTHAND_DATA_DIR", &dir);
        std::env::set_var("AGENTHAND_LEDGER", if sqlite { "sqlite" } else { "jsonl" });
        std::env::set_var("AGENTHAND_TOTAL_SPEND_CAP_USD", "0.0125");

        let handles: Vec<_> = (0..8)
            .map(|i| std::thread::spawn(move || run_item(i, Duration::from_millis(250))))
            .collect();
        let mut ok = 0;
        let mut refused = 0;
        for h in handles {
            match h.join().unwrap() {
                Ok(()) => ok += 1,
                Err(e) => {
                    assert!(e.contains("Spend cap"), "unexpected error: {e}");
                    refused += 1;
                }
            }
        }
        assert_eq!(ok, 5, "store sqlite={sqlite}");
        assert_eq!(refused, 3, "store sqlite={sqlite}");
        let total = store_total(&dir, sqlite);
        assert!(
            (total - 0.0125).abs() < 1e-9,
            "store sqlite={sqlite} total {total}"
        );
        assert_eq!(store_rows(&dir, sqlite), 10, "store sqlite={sqlite}");
        let _ = std::fs::remove_dir_all(&dir);
    }
    std::env::remove_var("AGENTHAND_LEDGER");
    std::env::remove_var("AGENTHAND_DATA_DIR");
    std::env::remove_var("AGENTHAND_TOTAL_SPEND_CAP_USD");
}

/// Wall-clock table: 8 items x 2 s simulated latency, sequential vs
/// parallel, per store. Correctness is asserted in every cell; the
/// timings are printed for the spike report. ~36 s total.
#[test]
#[ignore]
fn wall_clock_table() {
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::remove_var("AGENTHAND_TOTAL_SPEND_CAP_USD");
    let latency = Duration::from_secs(2);
    println!("\nstore      mode        wall_clock   total");
    for sqlite in [false, true] {
        for parallel in [false, true] {
            let dir = tmpdir("wallclock");
            std::env::set_var("AGENTHAND_DATA_DIR", &dir);
            std::env::set_var("AGENTHAND_LEDGER", if sqlite { "sqlite" } else { "jsonl" });
            let start = Instant::now();
            if parallel {
                let handles: Vec<_> = (0..8)
                    .map(|i| std::thread::spawn(move || run_item(i, latency)))
                    .collect();
                for h in handles {
                    h.join().unwrap().unwrap();
                }
            } else {
                for i in 0..8 {
                    run_item(i, latency).unwrap();
                }
            }
            let wall = start.elapsed();
            let total = store_total(&dir, sqlite);
            assert!((total - 0.02).abs() < 1e-9, "total {total}");
            assert_eq!(store_rows(&dir, sqlite), 16);
            println!(
                "{:<10} {:<10} {:>8.2}s   ${:.6}",
                if sqlite { "sqlite" } else { "jsonl" },
                if parallel { "parallel" } else { "sequential" },
                wall.as_secs_f64(),
                total
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
    std::env::remove_var("AGENTHAND_LEDGER");
    std::env::remove_var("AGENTHAND_DATA_DIR");
}
