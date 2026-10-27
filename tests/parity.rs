//! The parity gate as a test: runs the binary's `parity` subcommand
//! against the frozen replay package. Skipped unless
//! AGENTHAND_REPLAY_DIR points at the package (keeps `cargo test`
//! self-contained on a fresh clone).

use std::process::Command;

#[test]
fn parity_gate_against_the_frozen_run() {
    let Ok(dir) = std::env::var("AGENTHAND_REPLAY_DIR") else {
        eprintln!("AGENTHAND_REPLAY_DIR not set; parity gate skipped");
        return;
    };
    let output = Command::new(env!("CARGO_BIN_EXE_agenthand"))
        .args(["parity", &dir])
        .output()
        .expect("run parity");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "parity failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["parity"], "PASS");
    let checks = report["checks"].as_array().unwrap();
    assert!(checks.iter().all(|c| c["ok"].as_bool() == Some(true)));
    // The headline numbers, spelled out.
    let value_of = |name: &str| {
        checks
            .iter()
            .find(|c| c["check"] == name)
            .map(|c| c["value"].as_str().unwrap_or("").to_string())
            .unwrap_or_default()
    };
    assert_eq!(value_of("recomposed_changed_opaque_pixels"), "0");
    assert_eq!(value_of("recomposed_opaque_pixels"), "580613");
    assert_eq!(
        value_of("recomposed_output_pixel_sha256"),
        "8b8defea41e94004ddc0affd2abef697f32e20c5cf2fdab4c268236611c9e802"
    );
}
