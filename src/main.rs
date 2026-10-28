//! agenthand (Rust port) entry points:
//!
//! - `agenthand` / `agenthand serve` — the MCP stdio server.
//! - `agenthand parity <replay-package-dir>` — the parity gate: run
//!   the Rust fidelity core over the frozen verified run and compare
//!   every number and pixel hash against the frozen receipt.
//! - `agenthand verify <source> <final> <mask>` — the standalone
//!   file checker (port of `agenthand-verify`); exit 0/1.

use agenthand::assets::AssetStore;
use agenthand::composite;
use agenthand::image::{decode_image, sha256_hex};
use serde_json::{json, Value};
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        None | Some("serve") => {
            agenthand::server::run_stdio();
            ExitCode::SUCCESS
        }
        Some("parity") => {
            let Some(dir) = args.get(2) else {
                eprintln!("usage: agenthand parity <replay-package-dir>");
                return ExitCode::from(2);
            };
            match run_parity(Path::new(dir)) {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => ExitCode::FAILURE,
                Err(e) => {
                    eprintln!("parity run failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("verify") => {
            let (Some(source), Some(final_), Some(mask)) =
                (args.get(2), args.get(3), args.get(4))
            else {
                eprintln!("usage: agenthand verify <source> <final> <mask>");
                return ExitCode::from(2);
            };
            match composite::verify_files(Path::new(source), Path::new(final_), Path::new(mask)) {
                Ok(report) => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&report).unwrap_or_default()
                    );
                    if report["ok"].as_bool() == Some(true) {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    }
                }
                Err(e) => {
                    eprintln!("verify failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some(other) => {
            eprintln!("unknown subcommand {other:?} (expected: serve | parity | verify)");
            ExitCode::from(2)
        }
    }
}

/// The parity gate. Loads the frozen run's assets into a scratch
/// store, then:
/// 1. re-derives every file/pixel hash and compares with the receipt;
/// 2. re-runs the Rust composite (layer + plate) and compares the
///    counts and the output pixel hash with the frozen receipt;
/// 3. independently verifies the FROZEN composite against the frozen
///    receipt from disk (verify_receipt_from_disk).
fn run_parity(package_dir: &Path) -> Result<bool, String> {
    let receipt_text = std::fs::read_to_string(package_dir.join("receipt.json"))
        .map_err(|e| format!("cannot read receipt.json: {e}"))?;
    let receipt: Value = serde_json::from_str(&receipt_text)
        .map_err(|e| format!("receipt.json is not JSON: {e}"))?;
    let assets_src = package_dir.join("run").join("assets");

    // Scratch store populated with the frozen files + sidecars.
    let scratch = std::env::temp_dir().join(format!("agenthand-parity-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let data_dir = scratch.join("data");
    let assets_dir = data_dir.join("assets");
    std::fs::create_dir_all(&assets_dir).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(&assets_src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        std::fs::copy(entry.path(), assets_dir.join(entry.file_name()))
            .map_err(|e| e.to_string())?;
    }
    let store = AssetStore::new(&data_dir);

    let handle_of = |key: &str| -> String { receipt[key].as_str().unwrap_or_default().to_string() };
    let source_handle = handle_of("source_asset");
    let foreground_handle = handle_of("foreground_asset");
    let mask_handle = handle_of("mask_asset");
    let plate_handle = handle_of("plate_asset");
    let output_handle = handle_of("output_asset");

    let mut checks: Vec<(String, bool, String)> = Vec::new();

    // 1. Frozen files: file hashes and pixel hashes. The frozen
    // receipt pins source/foreground/output hashes; the mask and
    // plate hashes live in their sidecar records (the frozen receipt
    // predates those receipt fields), so each hash is checked against
    // the receipt where it carries the key, else the sidecar.
    for (label, handle, file_key, pixel_key) in [
        ("source", &source_handle, "source_sha256", "source_pixel_sha256"),
        (
            "foreground",
            &foreground_handle,
            "foreground_sha256",
            "foreground_pixel_sha256",
        ),
        ("mask", &mask_handle, "mask_sha256", "mask_pixel_sha256"),
        ("plate", &plate_handle, "plate_sha256", "plate_pixel_sha256"),
        ("output", &output_handle, "output_sha256", "output_pixel_sha256"),
    ] {
        let record = store.get_record(handle)?;
        let bytes = store.read_bytes(handle)?;
        let file_hash = sha256_hex(&bytes);
        let expected_file = receipt[file_key].as_str().unwrap_or(&record.sha256);
        checks.push((
            format!("{label}_file_sha256"),
            file_hash == expected_file,
            file_hash,
        ));
        let img = decode_image(&bytes)?.into_rgba();
        let pixel_hash = img.pixel_sha256();
        let expected_pixel = receipt[pixel_key]
            .as_str()
            .map(|s| s.to_string())
            .or_else(|| record.pixel_sha256.clone())
            .unwrap_or_default();
        checks.push((
            format!("{label}_pixel_sha256"),
            pixel_hash == expected_pixel,
            pixel_hash,
        ));
        store.verify_integrity(handle)?;
    }

    // 2. The Rust composite over the frozen layer + plate.
    let foreground = store.read_image(&foreground_handle)?;
    let plate = store.read_image(&plate_handle)?;
    let (recomposed, counts, transformations) = composite::composite_images(&foreground, &plate);
    let count_checks: [(&str, u64, &str); 6] = [
        ("changed_opaque_pixels", counts.changed_opaque_pixels, "changed_opaque_pixels"),
        ("opaque_pixels", counts.opaque_pixels, "opaque_pixels"),
        ("edge_band_pixels", counts.edge_band_pixels, "edge_band_pixels"),
        ("transparent_pixels", counts.transparent_pixels, "transparent_pixels"),
        ("mask_pixels_total", counts.mask_pixels_total, "mask_pixels_total"),
        (
            "edge_band_max_width_px",
            counts.edge_band_max_width_px as u64,
            "edge_band_max_width_px",
        ),
    ];
    for (name, mine, key) in count_checks {
        checks.push((
            format!("recomposed_{name}"),
            mine == receipt[key].as_u64().unwrap_or(u64::MAX),
            mine.to_string(),
        ));
    }
    let recomposed_pixel_hash = recomposed.pixel_sha256();
    checks.push((
        "recomposed_output_pixel_sha256".to_string(),
        recomposed_pixel_hash == receipt["output_pixel_sha256"].as_str().unwrap_or(""),
        recomposed_pixel_hash,
    ));

    // The full draw path (store-mediated) with the frozen plate.
    let (_new_handle, new_receipt) = composite::draw_product(
        &store,
        &foreground_handle,
        "parity gate (frozen plate, no scene prompt used)",
        Some(&plate_handle),
    )?;
    checks.push((
        "draw_changed_opaque_pixels".to_string(),
        new_receipt["changed_opaque_pixels"].as_u64()
            == receipt["changed_opaque_pixels"].as_u64(),
        new_receipt["changed_opaque_pixels"].to_string(),
    ));
    checks.push((
        "draw_output_pixel_sha256".to_string(),
        new_receipt["output_pixel_sha256"].as_str()
            == receipt["output_pixel_sha256"].as_str(),
        new_receipt["output_pixel_sha256"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    ));

    // 3. Independent verification of the frozen composite.
    let report = composite::verify_receipt_from_disk(&store, &output_handle, Some(&receipt))?;
    checks.push((
        "frozen_receipt_verify_from_disk_ok".to_string(),
        report["ok"].as_bool() == Some(true),
        serde_json::to_string(&report["problems"]).unwrap_or_default(),
    ));

    let pass = checks.iter().all(|(_, ok, _)| *ok);
    let out = json!({
        "parity": if pass { "PASS" } else { "FAIL" },
        "package": package_dir.display().to_string(),
        "transformations": transformations,
        "checks": checks.iter().map(|(name, ok, detail)| json!({
            "check": name, "ok": ok, "value": detail,
        })).collect::<Vec<_>>(),
        "frozen_receipt_verify_report": report,
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(pass)
}
