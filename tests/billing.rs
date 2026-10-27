//! Pricing + ledger tests. The cost vectors are the frozen run's
//! own settled ledger lines (replay package), so the Rust pricing
//! must reproduce the Python numbers to the micro-dollar.

use agenthand::ledger::{check_spend_cap, ledger_total_usd};
use agenthand::pricing::{estimate_cost_usd, Usage};

fn usage(input: i64, output: i64, cached: i64, images: i64) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: cached,
        images,
    }
}

#[test]
fn frozen_run_costs_reproduce_exactly() {
    // identify settled line: 3159 in / 1638 out / 3060 cached.
    let cost = estimate_cost_usd("muse-spark-1.3", &usage(3159, 1638, 3060, 0)).unwrap();
    assert_eq!(cost, 0.007544);
    // cut_out settled line: 1 image on sam-3.1.
    let cost = estimate_cost_usd("sam-3.1", &usage(0, 0, 0, 1)).unwrap();
    assert_eq!(cost, 0.0025);
    // draw settled line: 1 image on muse-image-1.0 (token counts on
    // the line are informational; the model prices per image only).
    let cost = estimate_cost_usd("muse-image-1.0", &usage(10242, 1019, 0, 1)).unwrap();
    assert_eq!(cost, 0.01);
}

#[test]
fn unknown_model_is_refused() {
    let err = estimate_cost_usd("gpt-5", &usage(1, 1, 0, 0)).unwrap_err();
    assert!(err.contains("No price for model"), "{err}");
}

#[test]
fn cached_tokens_cannot_exceed_input_tokens() {
    let err = estimate_cost_usd("muse-spark-1.3", &usage(10, 0, 11, 0)).unwrap_err();
    assert!(err.contains("cannot exceed"), "{err}");
}

#[test]
fn negative_counts_are_refused() {
    assert!(estimate_cost_usd("muse-spark-1.3", &usage(-1, 0, 0, 0)).is_err());
    assert!(estimate_cost_usd("muse-image-1.0", &usage(0, 0, 0, -1)).is_err());
}

const FROZEN_LEDGER: &str = "\
{\"tool\":\"identify\",\"model\":\"muse-spark-1.3\",\"usd_estimate\":0.024594,\"pricing_table_version\":\"2026-10-26.2\",\"input_tokens\":0,\"output_tokens\":0,\"cached_input_tokens\":0,\"images\":0,\"status\":\"ok\",\"phase\":\"pending\",\"call_id\":\"0b2f6e0953374858b4a554438944c1f1\",\"note\":\"\",\"timestamp\":\"2026-10-26T23:13:47.484718+00:00\"}
{\"tool\":\"identify\",\"model\":\"muse-spark-1.3\",\"usd_estimate\":0.007544,\"pricing_table_version\":\"2026-10-26.2\",\"input_tokens\":3159,\"output_tokens\":1638,\"cached_input_tokens\":3060,\"images\":0,\"status\":\"ok\",\"phase\":\"settled\",\"call_id\":\"0b2f6e0953374858b4a554438944c1f1\",\"note\":\"\",\"timestamp\":\"2026-10-26T23:13:56.563356+00:00\"}
{\"tool\":\"cut_out\",\"model\":\"sam-3.1\",\"usd_estimate\":0.005,\"pricing_table_version\":\"2026-10-26.2\",\"input_tokens\":0,\"output_tokens\":0,\"cached_input_tokens\":0,\"images\":0,\"status\":\"ok\",\"phase\":\"pending\",\"call_id\":\"bbdb8433ad30497b8e05d7baae490fcb\",\"note\":\"\",\"timestamp\":\"2026-10-26T23:13:56.566132+00:00\"}
{\"tool\":\"cut_out\",\"model\":\"sam-3.1\",\"usd_estimate\":0.0025,\"pricing_table_version\":\"2026-10-26.2\",\"input_tokens\":0,\"output_tokens\":0,\"cached_input_tokens\":0,\"images\":1,\"status\":\"ok\",\"phase\":\"settled\",\"call_id\":\"bbdb8433ad30497b8e05d7baae490fcb\",\"note\":\"\",\"timestamp\":\"2026-10-26T23:14:07.865121+00:00\"}
{\"tool\":\"draw\",\"model\":\"muse-image-1.0\",\"usd_estimate\":0.01,\"pricing_table_version\":\"2026-10-26.2\",\"input_tokens\":0,\"output_tokens\":0,\"cached_input_tokens\":0,\"images\":0,\"status\":\"ok\",\"phase\":\"pending\",\"call_id\":\"85cb0449d4eb47e2a272525ae04f5302\",\"note\":\"\",\"timestamp\":\"2026-10-26T23:14:08.384798+00:00\"}
{\"tool\":\"draw\",\"model\":\"muse-image-1.0\",\"usd_estimate\":0.01,\"pricing_table_version\":\"2026-10-26.2\",\"input_tokens\":10242,\"output_tokens\":1019,\"cached_input_tokens\":0,\"images\":1,\"status\":\"ok\",\"phase\":\"settled\",\"call_id\":\"85cb0449d4eb47e2a272525ae04f5302\",\"note\":\"\",\"timestamp\":\"2026-10-26T23:14:21.621972+00:00\"}
";

#[test]
fn ledger_total_settled_supersedes_pending() {
    let dir = std::env::temp_dir().join(format!("agenthand-ledger-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("ledger.jsonl");
    std::fs::write(&path, FROZEN_LEDGER).unwrap();
    let total = ledger_total_usd(&path).unwrap();
    assert!((total - 0.020044).abs() < 1e-9, "total {total}");
    // Cap checks against that total.
    assert!(check_spend_cap(&path, 100.0, 120.0).is_ok());
    assert!(check_spend_cap(&path, 119.99, 120.0).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ledger_total_counts_an_unsettled_reservation_at_face() {
    let dir = std::env::temp_dir().join(format!("agenthand-ledger-test2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("ledger.jsonl");
    std::fs::write(
        &path,
        "{\"tool\":\"cut_out\",\"model\":\"sam-3.1\",\"usd_estimate\":0.005,\"pricing_table_version\":\"2026-10-26.2\",\"input_tokens\":0,\"output_tokens\":0,\"cached_input_tokens\":0,\"images\":0,\"status\":\"ok\",\"phase\":\"pending\",\"call_id\":\"abc\",\"note\":\"\",\"timestamp\":\"2026-10-26T23:13:56.566132+00:00\"}\n",
    )
    .unwrap();
    let total = ledger_total_usd(&path).unwrap();
    assert!((total - 0.005).abs() < 1e-9, "total {total}");
    let _ = std::fs::remove_dir_all(&dir);
}
