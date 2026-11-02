//! agenthand (Rust build) entry points:
//!
//! - `agenthand` / `agenthand serve` — the MCP stdio server.
//! - `agenthand parity <replay-package-dir>` — the parity gate: run
//!   the Rust fidelity core over the frozen verified run and compare
//!   every number and pixel hash against the frozen receipt.
//! - `agenthand verify <source> <final> <mask>` — the standalone
//!   file checker (port of `agenthand-verify`); exit 0/1.
//! - `agenthand measure <image> <card> [--profile <path>]
//!   [--bbox x,y,w,h] [--mask <png>] [--garment] [--mask-edge-px <px>]
//!   [--diagram <out.png>] [--diagram-note <text>]` — the scale-card
//!   size estimator (port of `python -m agenthand.measure`); `<card>`
//!   is the id printed on the card in the photo (e.g. `AH-57`),
//!   `--bbox` is x,y,w,h in source pixels, `--mask` the product mask
//!   (same size as the photo); `--garment` adds tailor flat-lay
//!   measurements (docs/MEASURE-GARMENT.md); JSON on stdout, errors as
//!   `{"error": ...}` on stderr with exit 1.
//! - `agenthand heic-extract <file> [--out <dir>] [--hash-only]` —
//!   the rust-100 M1 HEIF container inspector/extractor: parses the
//!   box structure, inventories every item, and extracts `hvc1` item
//!   bitstreams (iloc methods 0/1). `--hash-only` writes nothing and
//!   reports sha256 + byte counts only (the mode used for
//!   `corpus/real/`). Grid composition is NOT implemented (pending
//!   scope widening; see the rust-100 milestone docs).
//! - `agenthand room-run --photos P ... [--video V ...]
//!   [--narrated-video V | --narrated-audio A] [--items "a, b" |
//!   --auto] [--scene TEXT] [--out DIR] [--room-id ID]
//!   [--budget-usd N] [--frame-every S] [--max-frames-per-video N]
//!   [--first-success-only] [--data-dir DIR] [--ledger PATH]` —
//!   the room runner (port of scripts/room_run.py): one staged,
//!   verified listing per item from a whole room; manifest + summary
//!   land in the output dir. Exit 0 when every item staged, 1 when
//!   any item did not stage, 2 on refusal/usage errors.
//! - `agenthand evidence [--db <path>] <verb> ...` — the Proof Graph
//!   CLI over the evidence database (read-only, offline): list, show,
//!   verify, trace, chapter, root, graph, rubric, calls, tamper. See
//!   `agenthand evidence --help` and docs/EVIDENCE.md.

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
        Some("measure") => run_measure(&args[2..]),
        Some("heic-extract") => run_heic_extract(&args[2..]),
        Some("room-run") => run_room_run(&args[2..]),
        Some("evidence") => agenthand::evidence::run(&args[2..]),
        Some("verify") => {
            // Positionals plus the Python verifier's optional layer
            // files: --reconstruction LAYER [--plate PLATE].
            let usage = "usage: agenthand verify <source> <final> <mask> \
                         [--reconstruction <layer.png>] [--plate <plate>]";
            let mut positional: Vec<&str> = Vec::new();
            let (mut reconstruction, mut plate): (Option<&str>, Option<&str>) = (None, None);
            let mut rest = args[2..].iter();
            while let Some(arg) = rest.next() {
                let slot = match arg.as_str() {
                    "--reconstruction" => &mut reconstruction,
                    "--plate" => &mut plate,
                    a if a.starts_with("--") => {
                        eprintln!("unknown verify option {a:?}\n{usage}");
                        return ExitCode::from(2);
                    }
                    a => {
                        positional.push(a);
                        continue;
                    }
                };
                let Some(value) = rest.next() else {
                    eprintln!("{arg} needs a file\n{usage}");
                    return ExitCode::from(2);
                };
                *slot = Some(value.as_str());
            }
            let [source, final_, mask] = positional[..] else {
                eprintln!("{usage}");
                return ExitCode::from(2);
            };
            match composite::verify_files_with_layers(
                Path::new(source),
                Path::new(final_),
                Path::new(mask),
                reconstruction.map(Path::new),
                plate.map(Path::new),
            ) {
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
            eprintln!(
                "unknown subcommand {other:?} (expected: serve | parity | verify | measure | heic-extract | room-run | evidence)"
            );
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
        (
            "source",
            &source_handle,
            "source_sha256",
            "source_pixel_sha256",
        ),
        (
            "foreground",
            &foreground_handle,
            "foreground_sha256",
            "foreground_pixel_sha256",
        ),
        ("mask", &mask_handle, "mask_sha256", "mask_pixel_sha256"),
        ("plate", &plate_handle, "plate_sha256", "plate_pixel_sha256"),
        (
            "output",
            &output_handle,
            "output_sha256",
            "output_pixel_sha256",
        ),
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
        (
            "changed_opaque_pixels",
            counts.changed_opaque_pixels,
            "changed_opaque_pixels",
        ),
        ("opaque_pixels", counts.opaque_pixels, "opaque_pixels"),
        (
            "edge_band_pixels",
            counts.edge_band_pixels,
            "edge_band_pixels",
        ),
        (
            "transparent_pixels",
            counts.transparent_pixels,
            "transparent_pixels",
        ),
        (
            "mask_pixels_total",
            counts.mask_pixels_total,
            "mask_pixels_total",
        ),
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
        new_receipt["changed_opaque_pixels"].as_u64() == receipt["changed_opaque_pixels"].as_u64(),
        new_receipt["changed_opaque_pixels"].to_string(),
    ));
    checks.push((
        "draw_output_pixel_sha256".to_string(),
        new_receipt["output_pixel_sha256"].as_str() == receipt["output_pixel_sha256"].as_str(),
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

const MEASURE_USAGE: &str = "usage: agenthand measure <image> <card> [--profile <path>] \
     [--bbox x,y,w,h] [--mask <mask.png>] [--garment] [--mask-edge-px <px>] \
     [--diagram <out.png>] [--diagram-note <text>]";

/// Port of `python -m agenthand.measure`: estimate a flat object's
/// size from a photo with an agenthand scale card in the same plane.
/// `--mask` takes the product mask (8-bit, product = 255, same size as
/// the photo) so extents come from the product itself; `--garment`
/// adds the top-wear flat-lay measurements (needs `--mask`);
/// `--diagram` draws them.
fn run_measure(args: &[String]) -> ExitCode {
    let mut positional: Vec<&String> = Vec::new();
    let mut profile: Option<String> = None;
    let mut bbox: Option<(f64, f64, f64, f64)> = None;
    let mut mask_path: Option<String> = None;
    let mut garment = false;
    let mut diagram: Option<String> = None;
    let mut diagram_note: Option<String> = None;
    let mut mask_edge_px: Option<f64> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--profile" => {
                i += 1;
                profile = args.get(i).cloned();
            }
            "--mask" => {
                i += 1;
                mask_path = args.get(i).cloned();
            }
            "--garment" => garment = true,
            "--diagram" => {
                i += 1;
                diagram = args.get(i).cloned();
            }
            "--diagram-note" => {
                i += 1;
                diagram_note = args.get(i).cloned();
            }
            "--mask-edge-px" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<f64>().ok()) {
                    Some(v) if v > 0.0 && v.is_finite() => mask_edge_px = Some(v),
                    _ => {
                        eprintln!("--mask-edge-px must be a positive number of source pixels");
                        return ExitCode::from(2);
                    }
                }
            }
            "--bbox" => {
                i += 1;
                let Some(raw) = args.get(i) else {
                    eprintln!("{MEASURE_USAGE}");
                    return ExitCode::from(2);
                };
                let parts: Vec<f64> = raw
                    .split(',')
                    .filter_map(|v| v.trim().parse::<f64>().ok())
                    .collect();
                if parts.len() != 4 {
                    eprintln!("--bbox must be x,y,w,h");
                    return ExitCode::from(2);
                }
                bbox = Some((parts[0], parts[1], parts[2], parts[3]));
            }
            other if other.starts_with("--") => {
                eprintln!("unknown option: {other}");
                return ExitCode::from(2);
            }
            _ => positional.push(&args[i]),
        }
        i += 1;
    }
    if positional.len() != 2 {
        eprintln!("{MEASURE_USAGE}");
        return ExitCode::from(2);
    }
    let (source, card_id) = (positional[0].as_str(), positional[1].as_str());
    let profile_path = profile.map(std::path::PathBuf::from).unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_default();
        Path::new(&home).join("workspace/meta-hackathon/scale-card/print/layout-profile.json")
    });
    let fail = |msg: String| -> ExitCode {
        eprintln!("{}", json!({ "error": msg }));
        ExitCode::FAILURE
    };
    // Resolve the source like Python `_load_image` (asset:// via
    // the store), decode with ingest orientation handling (the
    // cv2.imread route applies EXIF/eXIf orientation too).
    let store = AssetStore::new(&agenthand::config::get_data_dir());
    let path: std::path::PathBuf = if let Some(handle) = source.strip_prefix("asset://") {
        let full = format!("asset://{handle}");
        match store.resolve(&full) {
            Ok(p) => p,
            Err(e) => return fail(e),
        }
    } else {
        Path::new(source).to_path_buf()
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return fail(format!("Could not decode image: {}", path.display())),
    };
    let normalized = match agenthand::ingest::normalize_image_bytes(&bytes) {
        Ok(b) => b,
        Err(_) => return fail(format!("Could not decode image: {}", path.display())),
    };
    let img = match decode_image(&normalized) {
        Ok(img) => img.into_rgba(),
        Err(_) => return fail(format!("Could not decode image: {}", path.display())),
    };
    let card = match agenthand::measure::load_card_profile(&profile_path, card_id) {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    if (garment || diagram.is_some()) && mask_path.is_none() {
        eprintln!("--garment and --diagram need --mask (the garment's own pixels)");
        return ExitCode::from(2);
    }
    let mask: Option<Vec<u8>> = match &mask_path {
        None => None,
        Some(mp) => {
            let decoded = match std::fs::read(mp)
                .map_err(|e| e.to_string())
                .and_then(|b| decode_image(&b))
            {
                Ok(d) => d.into_rgba(),
                Err(_) => return fail(format!("Could not decode mask: {mp}")),
            };
            if decoded.width != img.width || decoded.height != img.height {
                return fail(format!(
                    "Mask is {}x{} but the photo is {}x{}: the mask must be the same size \
                     as the photo, in the same orientation.",
                    decoded.width, decoded.height, img.width, img.height
                ));
            }
            // Product = 255 (8-bit L); anything >= 128 counts as product.
            Some(
                decoded
                    .pixels
                    .chunks_exact(4)
                    .map(|p| if p[0] >= 128 { 255 } else { 0 })
                    .collect(),
            )
        }
    };
    let frame = agenthand::measure::MeasureFrame::from_rgba(&img);
    let (est, plane) = match agenthand::measure::estimate_with_plane(
        &frame,
        &card,
        card_id,
        bbox,
        mask.as_deref(),
    ) {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let mut out = est.to_json_pretty();
    if garment || diagram.is_some() {
        let mut opts = agenthand::measure_garment::GarmentOptions::default();
        if let Some(e) = mask_edge_px {
            opts.mask_edge_px = e;
        }
        let g = match agenthand::measure_garment::analyse(
            frame.width,
            frame.height,
            mask.as_deref().unwrap_or(&[]),
            &plane,
            &opts,
        ) {
            Ok(g) => g,
            Err(e) => return fail(e),
        };
        let mut block = g.to_json();
        block["card_print_scale_assumed"] = json!(est.card_print_scale_assumed);
        let pretty = serde_json::to_string_pretty(&block).unwrap_or_default();
        let indented = pretty.replace('\n', "\n  ");
        // Additive: the estimate's fields stay as they were; the garment
        // block is appended as the last key.
        if let Some(stripped) = out.strip_suffix("\n}") {
            out = format!("{stripped},\n  \"garment\": {indented}\n}}");
        }
        if let Some(dp) = &diagram {
            let png = agenthand::image::encode_png_rgba(
                &agenthand::measure_garment::render_diagram(&g, card_id, diagram_note.as_deref()),
            );
            if let Err(e) = std::fs::write(dp, png) {
                return fail(format!("Could not write diagram {dp}: {e}"));
            }
        }
    }
    println!("{out}");
    ExitCode::SUCCESS
}

/// rust-100 M1: HEIF container inspection + `hvc1` item extraction.
///
/// Prints a JSON inventory (brands, primary item, per-item census,
/// hvcC summaries, grid header when the primary is a grid). With
/// `--out <dir>` (and without `--hash-only`), extracted `hvc1`
/// bitstreams are written as `<stem>_item<id>.h265` plus the
/// inventory as `<stem>.heic.json`. `--hash-only` never writes any
/// file: sha256 + byte counts are computed in memory — the required
/// mode for `corpus/real/` (no decoded renders, no extracted
/// bitstreams of real files persisted; see MILESTONE-M1.md §privacy).
fn run_heic_extract(args: &[String]) -> ExitCode {
    use agenthand::heic::container;
    let mut file_arg: Option<String> = None;
    let mut out_dir: Option<String> = None;
    let mut hash_only = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                i += 1;
                out_dir = args.get(i).cloned();
            }
            "--hash-only" => hash_only = true,
            other => {
                if file_arg.is_none() {
                    file_arg = Some(other.to_string());
                } else {
                    eprintln!("heic-extract: unexpected argument {other:?}");
                    return ExitCode::from(2);
                }
            }
        }
        i += 1;
    }
    let Some(path) = file_arg else {
        eprintln!("usage: agenthand heic-extract <file> [--out <dir>] [--hash-only]");
        return ExitCode::from(2);
    };
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("heic-extract: cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let f = match container::parse(&data) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("heic-extract: {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let stem = Path::new(&path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "heic".to_string());

    let mut extracted = Vec::new();
    let mut failures = Vec::new();
    let write_payloads = out_dir.is_some() && !hash_only;
    if write_payloads {
        if let Err(e) = std::fs::create_dir_all(out_dir.as_ref().unwrap()) {
            eprintln!("heic-extract: cannot create --out dir: {e}");
            return ExitCode::FAILURE;
        }
    }
    for item in &f.items {
        if item.item_type != "hvc1" {
            continue;
        }
        match container::extract_item_data_for(&data, item) {
            Ok(bytes) => {
                let hvcc = f.item_hvcc(item);
                let nal_summary = hvcc
                    .map(|h| container::walk_nals(&bytes, h.length_size))
                    .transpose()
                    .map(|nals| {
                        let mut m = std::collections::BTreeMap::new();
                        for (t, _) in nals.unwrap_or_default() {
                            *m.entry(t).or_insert(0usize) += 1;
                        }
                        m
                    })
                    .unwrap_or_default();
                let mut entry = json!({
                    "item_id": item.item_id,
                    "bytes": bytes.len(),
                    "sha256": sha256_hex(&bytes),
                    "nal_counts_by_type": nal_summary,
                });
                if write_payloads {
                    let out_path = format!(
                        "{}/{}_item{}.h265",
                        out_dir.as_ref().unwrap(),
                        stem,
                        item.item_id
                    );
                    match std::fs::write(&out_path, &bytes) {
                        Ok(()) => entry["written"] = json!(out_path),
                        Err(e) => {
                            eprintln!("heic-extract: cannot write {out_path}: {e}");
                            return ExitCode::FAILURE;
                        }
                    }
                    if let Some(h) = hvcc {
                        match container::to_annexb(h, &bytes) {
                            Ok(ab) => {
                                let ab_path = format!(
                                    "{}/{}_item{}.annexb.h265",
                                    out_dir.as_ref().unwrap(),
                                    stem,
                                    item.item_id
                                );
                                match std::fs::write(&ab_path, &ab) {
                                    Ok(()) => entry["written_annexb"] = json!(ab_path),
                                    Err(e) => {
                                        eprintln!("heic-extract: cannot write {ab_path}: {e}");
                                        return ExitCode::FAILURE;
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("heic-extract: annexb conversion: {e}");
                                return ExitCode::FAILURE;
                            }
                        }
                    }
                }
                extracted.push(entry);
            }
            Err(e) => failures.push(json!({
                "item_id": item.item_id,
                "error": e.to_string(),
            })),
        }
    }

    let primary = f.primary_item();
    let mut primary_json = json!({
        "item_id": f.primary_item_id,
        "item_type": primary.map(|p| p.item_type.clone()),
        "ispe": f.primary_item().and_then(|p| f.item_ispe(p)),
    });
    if let Some(p) = primary {
        if p.item_type == "grid" {
            match container::extract_item_data_for(&data, p)
                .and_then(|d| container::parse_grid_header(&d))
            {
                Ok(g) => {
                    primary_json["grid"] = json!({
                        "rows": g.rows,
                        "columns": g.columns,
                        "output_width": g.output_width,
                        "output_height": g.output_height,
                        "tile_item_ids": f.primary_dimg(),
                    });
                }
                Err(e) => {
                    primary_json["grid_error"] = json!(e.to_string());
                }
            }
            primary_json["grid_composition"] = json!(
                "IN SCOPE (Daniel decision 52, 2026-10-28): composition geometry \
                 implemented in heic::grid and unit-tested on synthetic planes; \
                 end-to-end grid decode lands at the M3.5 junction once M2/M3 \
                 tile decode exists"
            );
        }
    }
    // Non-hvc1 items recorded with their roles (aux / derived / meta).
    let recorded: Vec<Value> = f
        .items
        .iter()
        .filter(|it| it.item_type != "hvc1")
        .map(|it| {
            json!({
                "item_id": it.item_id,
                "item_type": it.item_type,
                "ispe": f.item_ispe(it),
                "aux_type": f.item_aux_type(it),
                "construction_method": it.construction_method,
                "extent_bytes": it.total_extent_bytes(),
                "references": it.references,
            })
        })
        .collect();
    let hvcc_summaries: Vec<Value> = f
        .items
        .iter()
        .filter_map(|it| {
            f.item_hvcc(it).map(|h| {
                json!({
                    "item_id": it.item_id,
                    "profile_idc": h.general_profile_idc,
                    "level_idc": h.general_level_idc,
                    "chroma_format_idc": h.chroma_format_idc,
                    "bit_depth_luma": h.bit_depth_luma_minus8 + 8,
                    "bit_depth_chroma": h.bit_depth_chroma_minus8 + 8,
                    "length_size": h.length_size,
                    "arrays": h.arrays.iter().map(|(t, n, b)| json!({"nal_type": t, "count": n, "bytes": b})).collect::<Vec<_>>(),
                })
            })
        })
        .collect();
    let report = json!({
        "file": path,
        "file_bytes": data.len(),
        "major_brand": f.major_brand,
        "compatible_brands": f.compatible_brands,
        "primary": primary_json,
        "item_census": f.census(),
        "item_count": f.items.len(),
        "extracted_hvc1_items": extracted,
        "extraction_failures": failures,
        "non_hvc1_items": recorded,
        "hvcc": hvcc_summaries,
        "mode": if hash_only { "hash-only (nothing written)" } else { "extract" },
    });
    let text = serde_json::to_string_pretty(&report).unwrap_or_default();
    if write_payloads {
        let out_path = format!("{}/{}.heic.json", out_dir.as_ref().unwrap(), stem);
        if let Err(e) = std::fs::write(&out_path, &text) {
            eprintln!("heic-extract: cannot write {out_path}: {e}");
            return ExitCode::FAILURE;
        }
        println!("heic-extract: inventory written to {out_path}");
    } else {
        println!("{text}");
    }
    if failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Port of scripts/room_run.py: run one room (stills + optional
/// videos + optional narration) and write the manifest + summary.
fn run_room_run(args: &[String]) -> ExitCode {
    use agenthand::roomrun::{run_room, summarize_manifest, RoomRunOptions};
    use std::path::PathBuf;

    let mut photos: Vec<PathBuf> = Vec::new();
    let mut videos: Vec<PathBuf> = Vec::new();
    let mut narrated_videos: Vec<PathBuf> = Vec::new();
    let mut narrated_audios: Vec<PathBuf> = Vec::new();
    let mut narration_max_usd: f64 = 0.25;
    let mut items_arg: Option<String> = None;
    let mut auto = false;
    let mut scene: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut room_id: Option<String> = None;
    let mut budget_usd: f64 = 1.00;
    let mut frame_every: f64 = 2.0;
    let mut max_frames: usize = 8;
    let mut first_success_only = false;
    let mut data_dir: Option<PathBuf> = None;
    let mut ledger: Option<PathBuf> = None;
    let mut stage_mode: String = agenthand::backdrop::STAGE_MODE_SCENE.to_string();
    let mut comps_path: Option<PathBuf> = None;

    let usage = "usage: agenthand room-run --photos PHOTO [PHOTO ...] [--video VIDEO ...] \
                 [--narrated-video VIDEO] [--narrated-audio AUDIO] [--items \"a, b\"] [--auto] \
                 [--out DIR] [--budget-usd N] [--stage-mode scene|wallpaper|studio_gray] \
                 [--comps PATH]";
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        // Value-taking flags: `--photos` and `--video` swallow every
        // following non-flag value (argparse nargs="+"), the rest
        // take exactly one value.
        let take_values = |i: &mut usize, min: usize| -> Option<Vec<String>> {
            let mut values = Vec::new();
            while *i + 1 < args.len() && !args[*i + 1].starts_with("--") {
                *i += 1;
                values.push(args[*i].clone());
            }
            if values.len() < min {
                None
            } else {
                Some(values)
            }
        };
        match arg {
            "--photos" => match take_values(&mut i, 1) {
                Some(values) => photos.extend(values.into_iter().map(PathBuf::from)),
                None => {
                    eprintln!("room-run: --photos needs at least one value\n{usage}");
                    return ExitCode::from(2);
                }
            },
            "--video" => match take_values(&mut i, 1) {
                Some(values) => videos.extend(values.into_iter().map(PathBuf::from)),
                None => {
                    eprintln!("room-run: --video needs at least one value\n{usage}");
                    return ExitCode::from(2);
                }
            },
            "--narrated-video" => match take_values(&mut i, 1) {
                Some(values) => narrated_videos.extend(values.into_iter().map(PathBuf::from)),
                None => {
                    eprintln!("room-run: --narrated-video needs a value\n{usage}");
                    return ExitCode::from(2);
                }
            },
            "--narrated-audio" => match take_values(&mut i, 1) {
                Some(values) => narrated_audios.extend(values.into_iter().map(PathBuf::from)),
                None => {
                    eprintln!("room-run: --narrated-audio needs a value\n{usage}");
                    return ExitCode::from(2);
                }
            },
            "--items" => match take_values(&mut i, 1) {
                Some(mut values) => items_arg = Some(values.remove(0)),
                None => {
                    eprintln!("room-run: --items needs a value\n{usage}");
                    return ExitCode::from(2);
                }
            },
            "--narration-max-usd" => match take_values(&mut i, 1) {
                Some(mut values) => match values.remove(0).parse::<f64>() {
                    Ok(v) => narration_max_usd = v,
                    Err(_) => {
                        eprintln!("room-run: --narration-max-usd must be a number");
                        return ExitCode::from(2);
                    }
                },
                None => {
                    eprintln!("room-run: --narration-max-usd needs a value\n{usage}");
                    return ExitCode::from(2);
                }
            },
            "--scene" => match take_values(&mut i, 1) {
                Some(mut values) => scene = Some(values.remove(0)),
                None => {
                    eprintln!("room-run: --scene needs a value\n{usage}");
                    return ExitCode::from(2);
                }
            },
            "--out" => match take_values(&mut i, 1) {
                Some(mut values) => out = Some(PathBuf::from(values.remove(0))),
                None => {
                    eprintln!("room-run: --out needs a value\n{usage}");
                    return ExitCode::from(2);
                }
            },
            "--room-id" => match take_values(&mut i, 1) {
                Some(mut values) => room_id = Some(values.remove(0)),
                None => {
                    eprintln!("room-run: --room-id needs a value\n{usage}");
                    return ExitCode::from(2);
                }
            },
            "--budget-usd" => match take_values(&mut i, 1) {
                Some(mut values) => match values.remove(0).parse::<f64>() {
                    Ok(v) => budget_usd = v,
                    Err(_) => {
                        eprintln!("room-run: --budget-usd must be a number");
                        return ExitCode::from(2);
                    }
                },
                None => {
                    eprintln!("room-run: --budget-usd needs a value\n{usage}");
                    return ExitCode::from(2);
                }
            },
            "--frame-every" => match take_values(&mut i, 1) {
                Some(mut values) => match values.remove(0).parse::<f64>() {
                    Ok(v) => frame_every = v,
                    Err(_) => {
                        eprintln!("room-run: --frame-every must be a number");
                        return ExitCode::from(2);
                    }
                },
                None => {
                    eprintln!("room-run: --frame-every needs a value\n{usage}");
                    return ExitCode::from(2);
                }
            },
            "--max-frames-per-video" => match take_values(&mut i, 1) {
                Some(mut values) => match values.remove(0).parse::<usize>() {
                    Ok(v) => max_frames = v,
                    Err(_) => {
                        eprintln!("room-run: --max-frames-per-video must be an integer");
                        return ExitCode::from(2);
                    }
                },
                None => {
                    eprintln!("room-run: --max-frames-per-video needs a value\n{usage}");
                    return ExitCode::from(2);
                }
            },
            "--data-dir" => match take_values(&mut i, 1) {
                Some(mut values) => data_dir = Some(PathBuf::from(values.remove(0))),
                None => {
                    eprintln!("room-run: --data-dir needs a value\n{usage}");
                    return ExitCode::from(2);
                }
            },
            "--ledger" => match take_values(&mut i, 1) {
                Some(mut values) => ledger = Some(PathBuf::from(values.remove(0))),
                None => {
                    eprintln!("room-run: --ledger needs a value\n{usage}");
                    return ExitCode::from(2);
                }
            },
            "--stage-mode" => match take_values(&mut i, 1) {
                Some(mut values) => {
                    let value = values.remove(0);
                    if agenthand::backdrop::STAGE_MODES.contains(&value.as_str()) {
                        stage_mode = value;
                    } else {
                        eprintln!(
                            "room-run: --stage-mode must be one of scene, wallpaper, studio_gray"
                        );
                        return ExitCode::from(2);
                    }
                }
                None => {
                    eprintln!("room-run: --stage-mode needs a value\n{usage}");
                    return ExitCode::from(2);
                }
            },
            "--comps" => match take_values(&mut i, 1) {
                Some(mut values) => comps_path = Some(PathBuf::from(values.remove(0))),
                None => {
                    eprintln!("room-run: --comps needs a value\n{usage}");
                    return ExitCode::from(2);
                }
            },
            "--auto" => auto = true,
            "--first-success-only" => first_success_only = true,
            other => {
                eprintln!("room-run: unknown argument {other:?}\n{usage}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    let item_names: Vec<String> = items_arg
        .as_deref()
        .map(|raw| {
            raw.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if !auto && item_names.is_empty() && narrated_videos.is_empty() && narrated_audios.is_empty() {
        eprintln!(
            "room-run: name the items (--items \"straw hat, suitcase\"), pass --auto to \
             take the photo plans' concepts, or narrate them (--narrated-video / --narrated-audio)"
        );
        return ExitCode::from(2);
    }
    if photos.is_empty() && videos.is_empty() {
        eprintln!("room-run: at least one --photos or --video source is required");
        return ExitCode::from(2);
    }
    if let Some(dir) = &data_dir {
        std::env::set_var("AGENTHAND_DATA_DIR", dir);
    }
    let room_id = room_id.unwrap_or_else(|| {
        let uuid = uuid::Uuid::new_v4().simple().to_string();
        format!("room-{}", &uuid[..8])
    });
    let out_dir = out.unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(format!("roomrun-{room_id}"))
    });
    let comps: Option<Value> = match &comps_path {
        Some(path) => match std::fs::read_to_string(path)
            .map_err(|e| e.to_string())
            .and_then(|text| serde_json::from_str::<Value>(&text).map_err(|e| e.to_string()))
        {
            Ok(doc) => Some(doc),
            Err(e) => {
                eprintln!("room-run: cannot read --comps {}: {e}", path.display());
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    let mut opts = RoomRunOptions {
        item_names,
        auto,
        room_id: Some(room_id),
        budget_usd,
        frame_every_seconds: frame_every,
        max_frames_per_video: max_frames,
        probe_all_candidates: !first_success_only,
        narrated_videos,
        narrated_audios,
        narration_max_projected_usd: narration_max_usd,
        ledger_path: ledger,
        stage_mode,
        comps,
        ..RoomRunOptions::default()
    };
    if let Some(scene) = scene {
        opts.scene = scene;
    }
    match run_room(&photos, &videos, &out_dir, opts) {
        Ok(manifest) => {
            print!("{}", summarize_manifest(&manifest));
            println!("manifest: {}", out_dir.join("manifest.json").display());
            let items = manifest
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let all_staged = items
                .iter()
                .all(|item| item.get("status").and_then(Value::as_str) == Some("staged"));
            if all_staged {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(exc) => {
            eprintln!("ROOM RUN REFUSED: {exc}");
            ExitCode::from(2)
        }
    }
}
