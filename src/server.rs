//! The MCP server: the three model tools (identify / cut_out / draw)
//! with the same result shapes as the Python `server.py`, over a
//! hand-rolled JSON-RPC stdio loop. Comps pricing rides on identify's
//! ok result as a `pricing` block; there is no separate price tool.
//!
//! Why not the rmcp crate: the port's first job is byte-level parity
//! of the fidelity core, and the tool surface is three calls plus one
//! resource template. A hand-rolled loop keeps the wire shapes exactly
//! the Python server's (dict results as text + structuredContent,
//! draw's image preview as an image content block) with no SDK
//! version skew to audit. Revisit rmcp once parity is proven.

use crate::assets::{AssetStore, HANDLE_PREFIX, KIND_MASK, KIND_OVERLAY, KIND_PLATE};
use crate::backdrop;
use crate::billing::{self, BillingSummary};
use crate::composite;
use crate::comps;
use crate::config;
use crate::ingest;
use crate::meta::{self, IdentifyError, CUT_OUT_MODEL, DRAW_BACKGROUND_MODEL, IDENTIFY_MODEL};
use crate::segmentation;
use crate::staging::{self, SubjectGeometry};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::io::{BufRead, Write};

fn store() -> AssetStore {
    AssetStore::new(&config::get_data_dir())
}

fn new_call_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn usage_json(u: &crate::pricing::Usage) -> Value {
    json!({
        "input_tokens": u.input_tokens,
        "output_tokens": u.output_tokens,
        "cached_input_tokens": u.cached_input_tokens,
        "images": u.images,
    })
}

/// Run the identify planning step for a stored photo (port of
/// `_run_identify`). Ok(plan or {"error": ...}); Err propagates the
/// failures Python raises (spend cap, integrity).
fn run_identify(store: &AssetStore, handle: &str) -> Result<Value, String> {
    let key = match meta::require_meta_api_key() {
        Ok(k) => k,
        Err(e) => {
            return Ok(json!({"error": {"code": "NOT_CONFIGURED", "message": e}}));
        }
    };
    let record = store.get_record(handle)?;
    store.verify_integrity(handle)?;
    let png_bytes = store.read_bytes(handle)?;
    let projected = meta::project_identify_cost_usd(IDENTIFY_MODEL, record.width, record.height)?;
    let client = meta::build_client()?;

    let mut failed_attempts = 0usize;
    let mut last_error: Option<IdentifyError> = None;
    while failed_attempts < meta::IDENTIFY_MAX_ATTEMPTS {
        let call_id = new_call_id();
        let plan_cell: RefCell<Option<Value>> = RefCell::new(None);
        let err_cell: RefCell<Option<IdentifyError>> = RefCell::new(None);
        let result = billing::billed_call(
            "identify",
            Some(IDENTIFY_MODEL),
            projected,
            "ok",
            "",
            &call_id,
            || match meta::vision_identify(&client, &key, &png_bytes, IDENTIFY_MODEL, &call_id) {
                Ok((plan, usage)) => {
                    *plan_cell.borrow_mut() = Some(plan);
                    Ok(usage)
                }
                Err(e) => {
                    let message = e.message.clone();
                    *err_cell.borrow_mut() = Some(e);
                    Err(message)
                }
            },
        );
        match result {
            Ok(billing) => {
                let mut plan = plan_cell.into_inner().expect("plan set on success");
                plan["model"] = json!(IDENTIFY_MODEL);
                plan["cost_usd"] = json!(projected * failed_attempts as f64 + billing.actual_usd);
                plan["usage"] = usage_json(&billing.usage);
                plan["call_id"] = json!(billing.call_id);
                plan["attempts"] = json!(failed_attempts + 1);
                return Ok(plan);
            }
            Err(message) => {
                let stashed = err_cell.into_inner();
                match stashed {
                    None => return Err(message), // spend cap etc.: propagates
                    Some(e) => {
                        failed_attempts += 1;
                        let body = e.body_failure();
                        last_error = Some(e);
                        if !body {
                            break;
                        }
                    }
                }
            }
        }
    }
    let last = last_error.expect("the loop runs at least once");
    let mut error = json!({
        "code": "BAD_MODEL_RESPONSE",
        "message": last.message,
        "cost_counted_usd": projected * failed_attempts as f64,
    });
    if let Some(debug_path) = &last.debug_path {
        error["debug_path"] = json!(debug_path);
    }
    Ok(json!({"error": error}))
}

/// The identify planning step as shared internal logic (port of
/// `_plan_cut`).
fn plan_cut(store: &AssetStore, handle: &str) -> Result<Value, String> {
    let result = run_identify(store, handle)?;
    if let Some(error) = result.get("error") {
        return Ok(json!({
            "description": null,
            "concept": null,
            "alternates": [],
            "model": IDENTIFY_MODEL,
            "error": error,
        }));
    }
    Ok(result)
}

/// The `identify` tool handler: ingest, identify, shape the result JSON.
pub fn tool_identify(image: &str) -> Result<Value, String> {
    let store = store();
    let handle = ingest::ingest(image, &store)?;
    let result = run_identify(&store, &handle)?;
    if let Some(error) = result.get("error") {
        return Ok(json!({
            "status": "error",
            "tool": "identify",
            "image": handle,
            "description": null,
            "concept": null,
            "alternates": [],
            "suggested_title": null,
            "listing_bullets": [],
            "condition_notes": null,
            "model": IDENTIFY_MODEL,
            "error": error,
        }));
    }
    let mut out = json!({
        "status": "ok",
        "tool": "identify",
        "image": handle,
        "description": result["description"],
        "concept": result["concept"],
        "alternates": result["alternates"],
        "suggested_title": result["suggested_title"],
        "listing_bullets": result["listing_bullets"],
        "condition_notes": result["condition_notes"],
        "model": IDENTIFY_MODEL,
        "cost_usd": result["cost_usd"],
        "usage": result["usage"],
        "call_id": result["call_id"],
        "attempts": result["attempts"],
    });
    // Pricing is post-processing on the finished result: matched
    // code-side from the concept / alternates / title identify
    // already returned. The identify prompt and model schema are
    // untouched (longer identify wording caused empty 200 bodies).
    comps::attach_identify_pricing(&mut out, &comps::load_configured_book());
    Ok(out)
}

/// The `cut_out` tool handler: ingest, segment the concept, shape the
/// result JSON.
pub fn tool_cut_out(image: &str, concept: Option<String>) -> Result<Value, String> {
    let store = store();
    let handle = ingest::ingest(image, &store)?;
    let source_record = store.get_record(&handle)?;
    store.verify_integrity(&handle)?;
    let source_image = store.read_image(&handle)?;

    let mut concept = concept;
    let mut alternates: Vec<String> = Vec::new();
    let mut plan_error: Value = Value::Null;
    let mut concept_source = "provided";
    if concept.is_none() {
        let plan = plan_cut(&store, &handle)?;
        concept = plan
            .get("concept")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        alternates = plan
            .get("alternates")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        plan_error = plan.get("error").cloned().unwrap_or(Value::Null);
        concept_source = "identify_internal";
    }

    let error = |code: &str, message: &str, extra: Value| -> Value {
        let mut e = json!({
            "status": "error",
            "tool": "cut_out",
            "image": handle,
            "concept": concept,
            "concept_source": concept_source,
            "alternates": alternates,
            "plan_error": plan_error,
            "output_asset": null,
            "mask_asset": null,
            "model": CUT_OUT_MODEL,
            "error": {"code": code, "message": message},
        });
        if let (Some(base), Some(extra_obj)) = (e["error"].as_object_mut(), extra.as_object()) {
            for (k, v) in extra_obj {
                base.insert(k.clone(), v.clone());
            }
        }
        e
    };

    let Some(concept_text) = concept.clone() else {
        let code = plan_error
            .get("code")
            .and_then(|v| v.as_str())
            .unwrap_or("NO_CONCEPT")
            .to_string();
        return Ok(error(
            &code,
            "No segmentation concept available: the identify planning step did not produce one.",
            Value::Null,
        ));
    };

    let key = match meta::require_meta_api_key() {
        Ok(k) => k,
        Err(e) => return Ok(error("NOT_CONFIGURED", &e, Value::Null)),
    };

    let mut phrases = vec![concept_text];
    phrases.extend(alternates.iter().take(meta::SAM_MAX_ATTEMPTS - 1).cloned());
    let projected = meta::project_cut_out_cost_usd(CUT_OUT_MODEL, phrases.len())?;
    let client = meta::build_client()?;
    // SAM working resolution (decisions 68/69): past the proven cap,
    // only the SAM-bound copy is downscaled; the stored photo keeps
    // its full resolution and the returned mask is mapped back onto
    // it below.
    let sam_payload =
        segmentation::prepare_sam_png(&source_image, &store.read_bytes(&handle)?, None);
    let mask_provenance = json!({
        "source_width": source_image.width,
        "source_height": source_image.height,
        "sam_width": sam_payload.sam_width,
        "sam_height": sam_payload.sam_height,
        "scale_method": sam_payload.scale_method,
    });
    let png_bytes = sam_payload.png_bytes;
    let (sam_width, sam_height) = (sam_payload.sam_width, sam_payload.sam_height);
    let outcome_cell: RefCell<Option<segmentation::AttemptsOutcome>> = RefCell::new(None);
    let err_cell: RefCell<Option<segmentation::CutOutError>> = RefCell::new(None);
    let call_id = new_call_id();
    let billing_result = billing::billed_call(
        "cut_out",
        Some(CUT_OUT_MODEL),
        projected,
        "ok",
        "",
        &call_id,
        || match segmentation::run_attempts(
            &client,
            &key,
            &png_bytes,
            &phrases,
            CUT_OUT_MODEL,
            sam_width,
            sam_height,
        ) {
            Ok(outcome) => {
                let usage = outcome.usage;
                *outcome_cell.borrow_mut() = Some(outcome);
                Ok(usage)
            }
            Err(e) => {
                let message = e.message.clone();
                *err_cell.borrow_mut() = Some(e);
                Err(message)
            }
        },
    );
    let billing = match billing_result {
        Ok(b) => b,
        Err(message) => match err_cell.into_inner() {
            Some(e) => {
                return Ok(error(
                    &e.code,
                    &e.message,
                    json!({"attempts": e.attempts, "cost_counted_usd": projected}),
                ));
            }
            None => return Err(message),
        },
    };
    let outcome = outcome_cell.into_inner().expect("outcome set on success");

    // The winning mask is in the SAM frame; map it onto the full
    // source frame before anything is cut, reviewed, or stored, so
    // every downstream artifact (mask asset, foreground, overlay) is
    // at the photo's own resolution and the Fidelity Lock binds
    // those pixels. The mask PNG carries the provenance JSON as a
    // text chunk; draw republishes it in the receipt.
    let full_mask = segmentation::map_mask_to_frame(
        &outcome.mask_image,
        source_image.width,
        source_image.height,
    );
    // The mask's PNG chunk also carries the accepted concept phrase, so a
    // later draw knows the product class (docs/LIGHT-AND-THIN.md). The
    // result's mask_provenance and the receipt's mask_* fields are
    // unchanged (draw republishes only the five frame fields).
    let mut mask_text = mask_provenance.clone();
    mask_text["concept"] = json!(outcome.phrase);
    let provenance_json = serde_json::to_string(&mask_text).unwrap_or_default();
    let mask_bytes = crate::image::encode_png_gray_with_text(
        &full_mask,
        crate::assets::MASK_PROVENANCE_PNG_KEY,
        &provenance_json,
    );
    let mask_handle = store.put_bytes(
        &mask_bytes,
        KIND_MASK,
        ".png",
        Some(&source_record.asset_id),
    )?;
    let foreground = segmentation::extract_foreground(&source_image, &full_mask);
    // The lineage is a chain: mask parented to the source photo,
    // foreground parented to the mask (as in the Python server).
    // draw walks the foreground's ancestry for the mask, so this
    // shape is what lets the fidelity receipt name and pin it.
    let mask_record = store.get_record(&mask_handle)?;
    let foreground_handle = store.put_image(
        &foreground,
        crate::assets::KIND_FOREGROUND,
        Some(&mask_record.asset_id),
    )?;
    let foreground_record = store.get_record(&foreground_handle)?;
    let overlay = segmentation::mask_overlay_image(&source_image, &full_mask)?;
    let overlay_handle = store.put_image(&overlay, KIND_OVERLAY, Some(&source_record.asset_id))?;
    let mask_review = segmentation::review_mask(&full_mask);

    Ok(json!({
        "status": "ok",
        "tool": "cut_out",
        "image": handle,
        "concept": outcome.phrase,
        "requested_concept": concept,
        "concept_source": concept_source,
        "alternates": alternates,
        "plan_error": plan_error,
        "output_asset": foreground_handle,
        "mask_asset": mask_handle,
        "mask_overlay_asset": overlay_handle,
        "mask_review": mask_review,
        "mask_provenance": mask_provenance,
        "hashes": {
            "source": {
                "sha256": source_record.sha256,
                "pixel_sha256": source_record.pixel_sha256,
            },
            "foreground": {
                "sha256": foreground_record.sha256,
                "pixel_sha256": foreground_record.pixel_sha256,
            },
            "mask": {
                "sha256": mask_record.sha256,
                "pixel_sha256": mask_record.pixel_sha256,
            },
        },
        "attempts": outcome.attempts,
        "model": CUT_OUT_MODEL,
        "cost_usd": billing.actual_usd,
        "usage": usage_json(&billing.usage),
        "call_id": billing.call_id,
    }))
}

/// Enrich the persisted fidelity receipt with run attribution (port
/// of `_persist_full_receipt`); returns the full persisted receipt.
fn persist_full_receipt(
    store: &AssetStore,
    handle: &str,
    receipt: &Value,
    billing: &BillingSummary,
    scene_prompt: &str,
) -> Result<Value, String> {
    let mut full = receipt.clone();
    full["tool"] = json!("draw");
    full["model"] = json!(DRAW_BACKGROUND_MODEL);
    full["scene_prompt"] = json!(scene_prompt);
    full["cost_usd"] = json!(billing.actual_usd);
    full["usage"] = usage_json(&billing.usage);
    full["call_id"] = json!(billing.call_id);
    store.save_receipt(handle, &full)?;
    Ok(full)
}

/// The verify page every draw receipt points at (GitHub Pages for
/// now; Daniel, 2026-10-31). The receipt hash rides in the URL
/// fragment, so resolution happens entirely in the visitor's
/// browser and the static page never sees the receipt itself.
pub const VERIFY_URL_BASE: &str = "https://ausernameforyou.github.io/agenthand/verify";

/// Canonical receipt JSON for hashing. This crate builds
/// serde_json WITHOUT preserve_order, so Value objects are
/// BTreeMap-backed and serialization emits sorted keys with
/// compact separators and unescaped UTF-8 — the same canonical
/// form the Python mirror produces with json.dumps(sort_keys=
/// True, separators=(",", ":"), ensure_ascii=False) and the
/// docs verifier reproduces in JS. Anyone can strip the two
/// annotation fields below off a persisted receipt, re-run this
/// canonicalization, and land on the hash inside its verify URL.
pub fn canonical_receipt_json(receipt: &Value) -> String {
    serde_json::to_string(receipt).unwrap_or_default()
}

/// Render the AB 723 sidecar: a QR PNG encoding `payload`, ink
/// #111111 on white with a 4-module quiet zone, error-correction
/// level Q, scaled by whole modules to roughly 512 px. Pure local
/// rendering (the `qrcode` crate) — no call, no cost.
pub fn render_verify_qr_png(payload: &str) -> Result<Vec<u8>, String> {
    let code = qrcode::QrCode::with_error_correction_level(
        payload.as_bytes(),
        qrcode::EcLevel::Q,
    )
    .map_err(|e| format!("QR encode failed: {e:?}"))?;
    let width = code.width();
    let colors = code.to_colors();
    let quiet = 4usize;
    let total = width + quiet * 2;
    let scale = 512usize.div_ceil(total).max(1);
    let size = total * scale;
    let mut pixels = vec![255u8; size * size * 4];
    for (i, color) in colors.iter().enumerate() {
        if *color != qrcode::Color::Dark {
            continue;
        }
        let x0 = (i % width + quiet) * scale;
        let y0 = (i / width + quiet) * scale;
        for y in y0..y0 + scale {
            for x in x0..x0 + scale {
                let idx = (y * size + x) * 4;
                pixels[idx] = 17; // #111111
                pixels[idx + 1] = 17;
                pixels[idx + 2] = 17;
                pixels[idx + 3] = 255;
            }
        }
    }
    let img = crate::image::RgbaImage::new(size as u32, size as u32, pixels);
    Ok(crate::image::encode_png_rgba(&img))
}

/// Attach the verify sidecar to a settled draw: hash the full
/// persisted receipt (BEFORE annotation), build its verify URL,
/// render and store the QR PNG as its own asset parented to the
/// composite — never composited into the delivered photo (AB 723
/// wants the label/QR on or ADJACENT to the image). Returns
/// (qr handle, verify url, qr png sha256). Failure is non-fatal
/// by design: a local rendering problem must never fail a draw
/// that has already settled, so callers log and carry on with
/// the fields absent.
fn attach_verify_qr(
    store: &AssetStore,
    composite_handle: &str,
    full_receipt: &Value,
) -> Option<(String, String, String)> {
    let canonical = canonical_receipt_json(full_receipt);
    let receipt_sha256 = crate::image::sha256_hex(canonical.as_bytes());
    let verify_url = format!("{VERIFY_URL_BASE}#{receipt_sha256}");
    let png = match render_verify_qr_png(&verify_url) {
        Ok(png) => png,
        Err(e) => {
            eprintln!("agenthand: verify QR render failed (draw unaffected): {e}");
            return None;
        }
    };
    let qr_sha256 = crate::image::sha256_hex(&png);
    let parent = store.get_record(composite_handle).ok()?.asset_id;
    let qr_handle = store
        .put_bytes(&png, crate::assets::KIND_QR, ".png", Some(&parent))
        .ok()?;
    Some((qr_handle, verify_url, qr_sha256))
}

/// Annotate the persisted receipt with its verify fields and
/// mirror them (plus the ready-to-paste AB 723 label line) onto
/// the tool result. No-op when the sidecar could not be built.
fn annotate_draw_result(
    store: &AssetStore,
    composite_handle: &str,
    full_receipt: &mut Value,
    result: &mut Value,
) {
    // Provenance signals (bonus package): mirror the block the
    // receipt already carries onto the tool result, with the
    // location warning surfaced as its own field so a client can
    // show it before the user publishes anything. Independent of
    // the QR sidecar below: the warning must reach the caller
    // even when the sidecar cannot be built.
    if let Some(block) = full_receipt.get("provenance") {
        result["provenance"] = block.clone();
        if let Some(warning) = block.get("location_warning") {
            if !warning.is_null() {
                result["location_warning"] = warning.clone();
            }
        }
    }
    let Some((qr_handle, verify_url, qr_sha256)) =
        attach_verify_qr(store, composite_handle, full_receipt)
    else {
        return;
    };
    full_receipt["verify_url"] = json!(verify_url);
    full_receipt["qr_sha256"] = json!(qr_sha256);
    if let Err(e) = store.save_receipt(composite_handle, full_receipt) {
        eprintln!("agenthand: annotated receipt re-save failed: {e}");
    }
    result["qr_asset"] = json!(qr_handle);
    result["verify_url"] = json!(verify_url);
    result["label"] =
        json!(format!("Digitally altered. Original photo and proof: {verify_url}"));
}

/// Generate a background plate through the billing choke point (port
/// of `meta_client.generate_background_plate`).
fn generate_background_plate(
    store: &AssetStore,
    prompt: &str,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<(String, BillingSummary), PlateFailure> {
    let key = meta::require_meta_api_key().map_err(|e| meta::ImageError {
        code: "NOT_CONFIGURED".to_string(),
        message: e,
    })?;
    let client = meta::build_client().map_err(|e| meta::ImageError {
        code: "IMAGE_CALL_FAILED".to_string(),
        message: e,
    })?;
    let projected =
        meta::project_draw_cost_usd(DRAW_BACKGROUND_MODEL).map_err(|e| meta::ImageError {
            code: "IMAGE_CALL_FAILED".to_string(),
            message: e,
        })?;
    let handle_cell: RefCell<Option<String>> = RefCell::new(None);
    let err_cell: RefCell<Option<meta::ImageError>> = RefCell::new(None);
    let call_id = new_call_id();
    let result = billing::billed_call_detailed(
        "draw",
        Some(DRAW_BACKGROUND_MODEL),
        projected,
        "ok",
        "",
        &call_id,
        || match meta::request_plate_image(
            &client,
            &key,
            prompt,
            width,
            height,
            DRAW_BACKGROUND_MODEL,
        ) {
            Ok((png_bytes, usage)) => {
                match store.put_bytes(&png_bytes, crate::assets::KIND_PLATE, ".png", None) {
                    Ok(handle) => {
                        *handle_cell.borrow_mut() = Some(handle);
                        Ok(usage)
                    }
                    Err(e) => Err(e),
                }
            }
            Err(e) => {
                let message = e.message.clone();
                *err_cell.borrow_mut() = Some(e);
                Err(message)
            }
        },
    );
    match result {
        Ok(billing) => Ok((handle_cell.into_inner().expect("plate handle set"), billing)),
        Err(billing::BillingFailure::CallFailed {
            message,
            reserved_usd,
        }) => {
            let e = err_cell.into_inner().unwrap_or(meta::ImageError {
                code: "IMAGE_CALL_FAILED".to_string(),
                message,
            });
            Err(PlateFailure {
                code: e.code,
                message: e.message,
                details: json!({"cost_counted_usd": reserved_usd}),
            })
        }
        // Billing stopped it: before the call, or after a generation
        // that succeeded (its plate was stored inside the call).
        Err(stop) => Err(billing_stop_failure(
            stop,
            &call_id,
            "plate generation",
            handle_cell.into_inner().map(Ok),
        )),
    }
}

/// A plate-stage failure: the code and message draw reports, and the
/// details merged into its error object (always `cost_counted_usd`:
/// what the ledger counts for the plate call).
struct PlateFailure {
    code: String,
    message: String,
    details: Value,
}

impl From<meta::ImageError> for PlateFailure {
    /// A failure before any billed call ran: nothing was reserved.
    fn from(e: meta::ImageError) -> Self {
        PlateFailure {
            code: e.code,
            message: e.message,
            details: json!({"cost_counted_usd": 0.0}),
        }
    }
}

/// What draw reports when the billing choke point, not the image
/// call, stopped a plate call (`step`: "plate edit" or "plate
/// generation"). `paid_plate` is the plate a call that succeeded
/// returned, as kept in the store (or why it could not be kept).
///
/// - Before the call (spend cap, ledger, pricing table): nothing was
///   called. `SPEND_CAP_REFUSED` for the cap's own refusal, else
///   `BILLING_REFUSED`; counts what was reserved (normally nothing).
/// - After a call that SUCCEEDED (fix pass F5 over-cap settle, or a
///   settle write / pricing error): the call was made and paid for.
///   `BILLING_STOPPED_AFTER_CALL`, counting what the ledger counts
///   (the actual cost once settled), with the over-cap amount when
///   that is the cause, and the paid plate's handle. The draw still
///   fails loudly (F5: no further spend), but nothing paid for is lost.
/// - A failed call is not a billing stop; it stays `IMAGE_CALL_FAILED`
///   with its reservation counted (callers handle it before here).
fn billing_stop_failure(
    stop: billing::BillingFailure,
    call_id: &str,
    step: &str,
    paid_plate: Option<Result<String, String>>,
) -> PlateFailure {
    match stop {
        billing::BillingFailure::BeforeCall {
            message,
            reserved_usd,
        } => PlateFailure {
            code: if message.starts_with("Spend cap") {
                "SPEND_CAP_REFUSED"
            } else {
                "BILLING_REFUSED"
            }
            .to_string(),
            message: format!("Billing refused the {step} before any call was made: {message}"),
            details: json!({"cost_counted_usd": reserved_usd, "call_made": false, "call_id": call_id}),
        },
        billing::BillingFailure::AfterCall {
            message,
            counted_usd,
            actual_usd,
            overage,
            ..
        } => {
            let (plate, plate_error, kept) = match paid_plate {
                Some(Ok(handle)) => (
                    Some(handle.clone()),
                    None,
                    format!(" The paid plate is kept as {handle} (stored, not staged)."),
                ),
                Some(Err(e)) => (
                    None,
                    Some(e.clone()),
                    format!(" The paid plate could not be kept: {e}."),
                ),
                None => (None, None, String::new()),
            };
            PlateFailure {
                code: "BILLING_STOPPED_AFTER_CALL".to_string(),
                message: format!(
                    "Billing stopped the draw after the {step} call succeeded; the call was made and paid for. {message}{kept}"
                ),
                details: json!({
                    "cost_counted_usd": counted_usd,
                    "actual_usd": actual_usd,
                    "over_cap_usd": overage.map(|o| o.overage_usd),
                    "call_made": true,
                    "call_id": call_id,
                    "paid_plate_asset": plate,
                    "paid_plate_error": plate_error,
                }),
            }
        }
        billing::BillingFailure::CallFailed {
            message,
            reserved_usd,
        } => PlateFailure {
            code: "IMAGE_CALL_FAILED".to_string(),
            message,
            details: json!({"cost_counted_usd": reserved_usd}),
        },
    }
}

/// A plate ready to composite, with what draw reports about it.
struct StagedPlate {
    handle: String,
    billing: BillingSummary,
    kind_override: Option<(&'static str, Value)>,
    note: String,
    extra: Value,
}

/// Build the plate from the ORIGINAL photo (plate mode `source_edit`):
/// remove the product region (filled from the photo around it), send
/// that reference to the muse-image edit endpoint, resize the edit
/// back onto the photo's frame, then
/// draw the contact shadow and edge matte around the product. When
/// the edit call itself fails, the code-only plate stands in
/// (`source_blur`) and the failure is reported; a refusal before the
/// call (spend cap, ledger) is returned as an error, nothing staged.
fn stage_source_plate(
    store: &AssetStore,
    foreground_handle: &str,
    foreground: &crate::image::RgbaImage,
    geometry: &SubjectGeometry,
    angle: &staging::AngleEstimate,
    scene_prompt: &str,
    thin_trigger: &staging::ThinTrigger,
) -> Result<StagedPlate, PlateFailure> {
    let staging_err = |message: String| meta::ImageError {
        code: "STAGING_FAILED".to_string(),
        message,
    };
    let chain = store.lineage(foreground_handle).map_err(staging_err)?;
    let source_record = chain.last().expect("lineage is non-empty").clone();
    let source = store
        .read_image(&(HANDLE_PREFIX.to_string() + &source_record.asset_id))
        .map_err(staging_err)?;
    if (source.width, source.height) != (foreground.width, foreground.height) {
        return Err(staging_err(format!(
            "source {}x{} and foreground {}x{} frames differ; the plate must share the product's frame",
            source.width, source.height, foreground.width, foreground.height
        ))
        .into());
    }
    let size = meta::closest_image_size(Some(source.width), Some(source.height));
    let (ref_w, ref_h) = size
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
        .unwrap_or((1024, 1024));
    // Reference fill (docs/REFERENCE-FILL.md): inpainted from the
    // non-product pixels by default; AGENTHAND_REFERENCE_FILL=grey
    // restores the legacy grey hole and its prompt wording.
    let fill = staging::ReferenceFill::from_env();
    let edit_ref = staging::edit_reference(&source, foreground, ref_w, ref_h, fill);
    let reference_png = crate::image::encode_png_rgba(&edit_ref.image);
    // Angle lock: every source_edit prompt ends with the base clause,
    // plus the shot-angle phrase when the geometry states one.
    let angle_lock = meta::angle_lock_clause(Some(angle));
    let prompt =
        meta::plate_edit_prompt_for_fill(scene_prompt, geometry.fills_frame, Some(angle), fill);
    let cfg = meta::PlateEditConfig::from_env();

    let key = meta::require_meta_api_key().map_err(|e| meta::ImageError {
        code: "NOT_CONFIGURED".to_string(),
        message: e,
    })?;
    let client = meta::build_client().map_err(|e| meta::ImageError {
        code: "IMAGE_CALL_FAILED".to_string(),
        message: e,
    })?;
    let projected =
        meta::project_draw_cost_usd(DRAW_BACKGROUND_MODEL).map_err(|e| meta::ImageError {
            code: "IMAGE_CALL_FAILED".to_string(),
            message: e,
        })?;
    let bytes_cell: RefCell<Option<Vec<u8>>> = RefCell::new(None);
    let err_cell: RefCell<Option<meta::ImageError>> = RefCell::new(None);
    let call_id = new_call_id();
    let result = billing::billed_call_detailed(
        "draw",
        Some(DRAW_BACKGROUND_MODEL),
        projected,
        "ok",
        "",
        &call_id,
        || match meta::request_plate_edit(
            &client,
            &key,
            &reference_png,
            &prompt,
            &size,
            DRAW_BACKGROUND_MODEL,
            &cfg,
        ) {
            Ok((png_bytes, usage)) => {
                *bytes_cell.borrow_mut() = Some(png_bytes);
                Ok(usage)
            }
            Err(e) => {
                let message = e.message.clone();
                *err_cell.borrow_mut() = Some(e);
                Err(message)
            }
        },
    );
    let bytes = bytes_cell.into_inner();
    let call_error = err_cell.into_inner();
    // The code-only plate stands in when the edit call itself failed
    // or its image did not decode; the failure is reported.
    let fallback = |code: String, message: String, billing: BillingSummary| {
        (
            staging::fallback_plate(&source, foreground),
            backdrop::PLATE_KIND_SOURCE_BLUR,
            json!({"model": DRAW_BACKGROUND_MODEL, "path": cfg.path, "size": size, "failed": {"code": code, "message": message}}),
            billing,
            Some(json!({"code": code, "message": message})),
        )
    };
    let (plate, kind, edit, billing, fallback) = match result {
        Ok(billing) => match bytes.as_deref().and_then(|b| {
            crate::image::decode_image(b)
                .ok()
                .map(|d| (crate::image::sha256_hex(b), d.into_rgba()))
        }) {
            Some((output_sha256, image)) => (
                staging::plate_from_edit(&image, source.width, source.height),
                backdrop::PLATE_KIND_SOURCE_EDIT,
                json!({"model": DRAW_BACKGROUND_MODEL, "path": cfg.path, "size": size, "output_sha256": output_sha256}),
                billing,
                None,
            ),
            None => fallback(
                "BAD_IMAGE_RESPONSE".to_string(),
                "the edit image did not decode".to_string(),
                billing,
            ),
        },
        // The edit call itself failed: its reservation stays counted
        // under the fail-closed ledger rule; report that amount.
        Err(billing::BillingFailure::CallFailed {
            message,
            reserved_usd,
        }) => {
            let (code, message) = match call_error {
                Some(e) => (e.code, e.message),
                None => ("IMAGE_CALL_FAILED".to_string(), message),
            };
            fallback(
                code,
                message,
                BillingSummary {
                    call_id: call_id.clone(),
                    projected_usd: projected,
                    actual_usd: reserved_usd,
                    usage: crate::pricing::Usage::default(),
                },
            )
        }
        // Billing stopped it, before the call (spend cap, ledger,
        // pricing) or after an edit that succeeded and was paid for
        // (F5 over-cap settle, settle write or pricing error). Fail
        // loudly; a paid edit is kept in the store, not staged.
        Err(stop) => {
            let paid = match (&stop, bytes) {
                (billing::BillingFailure::AfterCall { .. }, Some(b)) => Some(store.put_bytes(
                    &b,
                    KIND_PLATE,
                    ".png",
                    Some(&source_record.asset_id),
                )),
                _ => None,
            };
            return Err(billing_stop_failure(stop, &call_id, "plate edit", paid));
        }
    };
    // --- insertion point: light and thin (docs/LIGHT-AND-THIN.md) ---
    // The contact shadow moves a little away from the photo's light when
    // a direction reads plainly (no prompt clause: the edit keeps the
    // photo's own light); then the thin-structure guard on the finished
    // plate, plate pixels outside the product only.
    let light = staging::estimate_light_direction(&source, foreground);
    let offset = light.side.map_or((0.0, 0.0), |side| {
        composite::contact_shadow_offset_px(foreground, side.shadow_step())
    });
    let finished = staging::finish_plate_lit(&plate, foreground, geometry, offset);
    let (finished, thin) = composite::soften_thin_holes(&finished, foreground, thin_trigger);
    // The plate call has been billed by now: a failure to store the
    // finished plate reports what the ledger counts for it.
    let handle = store
        .put_image(&finished, KIND_PLATE, Some(&source_record.asset_id))
        .map_err(|message| PlateFailure {
            code: "STAGING_FAILED".to_string(),
            message,
            details: json!({"cost_counted_usd": billing.actual_usd, "call_id": billing.call_id}),
        })?;
    let mut reference_record = json!({
        "sha256": crate::image::sha256_hex(&reference_png),
        "size": size,
        "dilate_px": staging::REFERENCE_DILATE_PX,
        "hole_pixels": edit_ref.hole_pixels,
        "product_region": edit_ref.product_region_note(),
    });
    // Additive: fill, fill_iterations, fill_texture; fill_rgb stays
    // (null for the inpaint).
    if let Some(fields) = edit_ref.fill_record().as_object() {
        for (k, v) in fields {
            reference_record[k.as_str()] = v.clone();
        }
    }
    let backdrop_value = backdrop::source_plate_receipt_value(
        kind,
        geometry.to_json(),
        reference_record,
        edit,
        geometry.shadow,
        geometry.matte_px,
    );
    let note = if fallback.is_none() {
        "Background plate built from the original photo: the product region was removed before upload (filled from the photo around it; its pixels never reached the image model), muse-image-1.0 filled and tidied the surroundings in the photo's own perspective and light, and the stored product layer was composited back untouched at its original position and scale."
    } else {
        "The muse-image edit call failed (see plate_fallback), so the plate is the original photo with the product region filled from its surroundings, blurred and brightened by deterministic code. The stored product layer was composited back untouched at its original position and scale."
    };
    let mut extra = json!({
        "plate_mode": backdrop::DRAW_PLATE_SOURCE_EDIT,
        "plate_kind": kind,
        "subject": geometry.to_json(),
        "angle_lock": angle_lock_json(angle, &angle_lock),
        "light_direction": light_direction_json(&light, None, geometry.shadow.then_some(offset)),
        "thin_structure": thin.to_json(),
    });
    if let Some(f) = fallback {
        extra["plate_fallback"] = f;
    }
    if geometry.fills_frame {
        extra["subject_note"] = json!(staging::SURFACE_NOTE);
    }
    Ok(StagedPlate {
        handle,
        billing,
        kind_override: Some((kind, backdrop_value)),
        note: note.to_string(),
        extra,
    })
}

/// The draw result's `light_direction` record (docs/LIGHT-AND-THIN.md):
/// `stated` is true only when a phrase was appended to the plate prompt
/// (generated plates); `side` and `method` name what the photo showed
/// either way, and `contact_shadow_offset_px` what the source plate's
/// contact shadow was moved by.
fn light_direction_json(
    light: &staging::LightEstimate,
    phrase: Option<&str>,
    contact_offset: Option<(f64, f64)>,
) -> Value {
    let mut record = json!({
        "stated": phrase.is_some(),
        "method": light.method,
        "side": light.side.map(|s| s.bucket()),
        "shadow_falls": light.side.map(|s| s.shadow_side()),
        "signals": light.signals,
    });
    match phrase {
        Some(p) => record["phrase"] = json!(p),
        None if light.side.is_some() => {
            record["reason"] = json!("source plate keeps the photo's own light; no clause added, the contact shadow follows the reading")
        }
        None => record["reason"] = json!(light.reason),
    }
    if let Some((dx, dy)) = contact_offset {
        record["contact_shadow_offset_px"] = json!([crate::round6(dx), crate::round6(dy)]);
    }
    record
}

/// The identify concept the foreground was cut with, from its mask's PNG
/// chunk (cut_out writes it there); None for older masks.
fn mask_concept_of(store: &AssetStore, foreground_handle: &str) -> Option<String> {
    let chain = store.lineage(foreground_handle).ok()?;
    let mask = chain.iter().find(|r| r.kind == KIND_MASK)?;
    let bytes = store
        .read_bytes(&(HANDLE_PREFIX.to_string() + &mask.asset_id))
        .ok()?;
    let raw = crate::assets::png_text_of_bytes(&bytes)
        .get(crate::assets::MASK_PROVENANCE_PNG_KEY)?
        .clone();
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    let concept = parsed.get("concept")?.as_str()?.trim().to_string();
    (!concept.is_empty()).then_some(concept)
}

/// The thin-structure guard on a stored plate (generated plates): when it
/// softened anything, the softened plate is stored as a new plate asset
/// (parented to the original) and its handle replaces the original's.
fn guard_stored_plate(
    store: &AssetStore,
    plate_handle: &str,
    foreground: &crate::image::RgbaImage,
    trigger: &staging::ThinTrigger,
) -> Result<(String, Value), String> {
    if !trigger.triggered || trigger.holes == 0 {
        // Nothing to soften: no need to read the plate back.
        let mut report = composite::ThinReport::from_trigger(trigger);
        if trigger.triggered {
            report.note = Some("no enclosed holes in the product mask".to_string());
        }
        return Ok((plate_handle.to_string(), report.to_json()));
    }
    let plate = store.read_image(plate_handle)?;
    let (softened, report) = composite::soften_thin_holes(&plate, foreground, trigger);
    if !report.holes_softened {
        return Ok((plate_handle.to_string(), report.to_json()));
    }
    let parent = AssetStore::parse_handle(plate_handle)?;
    let handle = store.put_image(&softened, KIND_PLATE, Some(&parent))?;
    Ok((handle, report.to_json()))
}

/// The draw result's `angle_lock` record: the estimated bucket, the
/// exact clause appended to the plate prompt, whether a specific angle
/// was stated, and the geometry it was read from.
fn angle_lock_json(angle: &staging::AngleEstimate, clause: &str) -> Value {
    json!({
        "bucket": angle.angle.bucket(),
        "clause": clause,
        "stated": angle.stated,
        "reason": angle.reason,
        "signals": angle.signals_json(),
    })
}

/// draw: returns (result, preview PNG bytes) — Python returns
/// [result, Image]; the JSON-RPC layer renders both. The plate mode
/// comes from `AGENTHAND_DRAW_PLATE` (default `source_edit`).
pub fn tool_draw(asset: &str, scene_prompt: &str) -> Result<(Value, Vec<u8>), String> {
    tool_draw_with_plate(asset, scene_prompt, None)
}

/// draw with an explicit plate mode (`source_edit` | `generated`).
pub fn tool_draw_with_plate(
    asset: &str,
    scene_prompt: &str,
    plate: Option<&str>,
) -> Result<(Value, Vec<u8>), String> {
    let store = store();
    composite::validate_foreground(&store, asset)?;
    let foreground_record = store.get_record(asset)?;

    let error = |code: &str, message: &str, extra: Value| -> Value {
        let mut e = json!({
            "status": "error",
            "tool": "draw",
            "asset": asset,
            "scene_prompt": scene_prompt,
            "output_asset": null,
            "fidelity_receipt": null,
            "model": DRAW_BACKGROUND_MODEL,
            "error": {"code": code, "message": message},
        });
        if let (Some(base), Some(extra_obj)) = (e["error"].as_object_mut(), extra.as_object()) {
            for (k, v) in extra_obj {
                base.insert(k.clone(), v.clone());
            }
        }
        e
    };

    let plate_mode = match backdrop::resolve_draw_plate(plate) {
        Ok(mode) => mode,
        Err(e) => return Ok((error("BAD_ARGUMENT", &e, Value::Null), Vec::new())),
    };
    // Never stage a plate with nothing on it: an empty (or fully
    // soft) foreground is refused before any spend, in every mode.
    let foreground = store.read_image(asset)?;
    let geometry = staging::analyze_subject(&foreground);
    let angle = staging::estimate_shot_angle(&geometry);
    if let Err(e) = staging::check_stageable(&geometry) {
        return Ok((
            error("EMPTY_FOREGROUND", &e, json!({"cost_counted_usd": 0.0})),
            Vec::new(),
        ));
    }

    if config::use_placeholder_plate() {
        return draw_with_placeholder_plate(&store, asset, scene_prompt, &error);
    }

    if let Err(e) = meta::require_meta_api_key() {
        return Ok((error("NOT_CONFIGURED", &e, Value::Null), Vec::new()));
    }

    // Priced before any work (an unpriced model raises here); the plate
    // paths report what the ledger counts on failure.
    meta::project_draw_cost_usd(DRAW_BACKGROUND_MODEL)?;
    // --- insertion point: light and thin (docs/LIGHT-AND-THIN.md) ---
    // The concept cut_out recorded in the mask, and the thin-structure
    // trigger (concept words or enclosed mask holes; AGENTHAND_THIN=off).
    let concept = mask_concept_of(&store, asset);
    let thin_trigger = composite::thin_trigger_for(&foreground, concept.as_deref());
    let staged = if plate_mode == backdrop::DRAW_PLATE_GENERATED {
        // No reference photo here, so the angle lock rides as steering
        // words from the estimated angle (eye level when unsure). The
        // receipt keeps the caller's scene text.
        let steer = meta::generated_angle_steer(&angle);
        // Light read off the source photo (stated only when conclusive),
        // the plainness clause for open-frame products and the scene
        // exclusions from the concept ride after the steer.
        // Never fatal: an unreadable source photo states nothing.
        let light = store
            .lineage(asset)
            .ok()
            .and_then(|chain| chain.last().cloned())
            .and_then(|r| {
                store
                    .read_image(&(HANDLE_PREFIX.to_string() + &r.asset_id))
                    .ok()
            })
            .map(|source| staging::estimate_light_direction(&source, &foreground))
            .unwrap_or_else(|| staging::LightEstimate {
                side: None,
                method: "none",
                reason: "source photo unreadable; light not estimated",
                signals: Value::Null,
            });
        let light_phrase = meta::light_clause(&light);
        let exclusions = meta::scene_exclusions(concept.as_deref());
        let plate_scene = meta::generated_plate_scene_dressed(
            scene_prompt,
            &angle,
            light_phrase,
            thin_trigger.triggered,
            &exclusions,
        );
        match generate_background_plate(
            &store,
            &plate_scene,
            foreground_record.width,
            foreground_record.height,
        ) {
            Ok((handle, billing)) => StagedPlate {
                handle,
                billing,
                kind_override: None,
                note: "Background plate generated by muse-image-1.0 and fitted to the product by deterministic code; the stored product layer was composited untouched and never sent to the image model.".to_string(),
                extra: json!({
                    "plate_mode": backdrop::DRAW_PLATE_GENERATED,
                    "angle_lock": angle_lock_json(&angle, &steer),
                    "light_direction": light_direction_json(&light, light_phrase, None),
                    "scene_exclusions": exclusions,
                }),
            },
            Err(e) => {
                return Ok((error(&e.code, &e.message, e.details), Vec::new()));
            }
        }
    } else {
        match stage_source_plate(
            &store,
            asset,
            &foreground,
            &geometry,
            &angle,
            scene_prompt,
            &thin_trigger,
        ) {
            Ok(staged) => staged,
            Err(e) => {
                return Ok((error(&e.code, &e.message, e.details), Vec::new()));
            }
        }
    };
    // --- insertion point: thin-structure guard on a generated plate ---
    // (source plates were guarded before they were stored).
    // Never fatal: the plate is already paid for, so a guard failure
    // keeps the original plate and says so.
    let mut staged = staged;
    if staged.extra.get("thin_structure").is_none() {
        match guard_stored_plate(&store, &staged.handle, &foreground, &thin_trigger) {
            Ok((handle, record)) => {
                staged.handle = handle;
                staged.extra["thin_structure"] = record;
            }
            Err(e) => {
                let mut record = composite::ThinReport::from_trigger(&thin_trigger).to_json();
                record["note"] = json!(format!("guard skipped, original plate kept: {e}"));
                staged.extra["thin_structure"] = record;
            }
        }
    }
    drop(foreground);
    let plate_handle = staged.handle.clone();
    let billing = staged.billing;

    let (handle, receipt) = composite::draw_product_with_plate_kind(
        &store,
        asset,
        scene_prompt,
        Some(&plate_handle),
        staged.kind_override,
    )?;
    let mut full = persist_full_receipt(&store, &handle, &receipt, &billing, scene_prompt)?;
    if receipt["changed_opaque_pixels"].as_i64().unwrap_or(-1) != 0
        || receipt["color_profile_preserved"].as_bool() != Some(true)
    {
        let mut failures = Vec::new();
        if receipt["changed_opaque_pixels"].as_i64().unwrap_or(0) != 0 {
            failures.push(format!(
                "the compositor measured {} changed opaque foreground pixels",
                receipt["changed_opaque_pixels"]
            ));
        }
        if receipt["color_profile_preserved"].as_bool() != Some(true) {
            failures.push(
                "the composite does not carry the source photo's colour profile unchanged"
                    .to_string(),
            );
        }
        let message =
            failures.join("; ") + "; refusing to present this composite under the Fidelity Lock.";
        // Python's _error puts these extras inside the error object.
        let e = error(
            "FIDELITY_VIOLATION",
            &message,
            json!({
                "output_asset": handle,
                "fidelity_receipt": receipt,
                "cost_usd": billing.actual_usd,
            }),
        );
        return Ok((e, Vec::new()));
    }

    let mut result = json!({
        "status": "ok",
        "tool": "draw",
        "asset": asset,
        "scene_prompt": scene_prompt,
        "output_asset": handle,
        "plate_asset": plate_handle,
        "fidelity_receipt": receipt,
        "model": DRAW_BACKGROUND_MODEL,
        "cost_usd": billing.actual_usd,
        "usage": usage_json(&billing.usage),
        "call_id": billing.call_id,
        "note": staged.note,
    });
    if let (Some(base), Some(extra)) = (result.as_object_mut(), staged.extra.as_object()) {
        for (k, v) in extra {
            base.insert(k.clone(), v.clone());
        }
    }
    // Shadow carry-over record (docs/SHADOW-CARRY.md), beside angle_lock:
    // carried | contact_only with the reason ("generated plate" off the
    // source plates).
    result["shadow"] = composite::shadow_record_of(&receipt);
    annotate_draw_result(&store, &handle, &mut full, &mut result);
    let preview = store.read_bytes(&handle)?;
    Ok((result, preview))
}

fn draw_with_placeholder_plate(
    store: &AssetStore,
    asset: &str,
    scene_prompt: &str,
    _error: &dyn Fn(&str, &str, Value) -> Value,
) -> Result<(Value, Vec<u8>), String> {
    let outcome: RefCell<Option<(String, Value)>> = RefCell::new(None);
    let call_id = new_call_id();
    let billing = billing::billed_call(
        "draw",
        Some(DRAW_BACKGROUND_MODEL),
        0.0,
        "stub",
        "Local placeholder plate (dev flag AGENTHAND_DRAW_PLACEHOLDER_PLATE); no API call.",
        &call_id,
        || {
            let (handle, receipt) = composite::draw_product(store, asset, scene_prompt, None)?;
            *outcome.borrow_mut() = Some((handle, receipt));
            Ok(crate::pricing::Usage::default())
        },
    )?;
    let (handle, receipt) = outcome.into_inner().expect("composite outcome set");
    let mut full = persist_full_receipt(store, &handle, &receipt, &billing, scene_prompt)?;
    let mut result = json!({
        "status": "ok",
        "tool": "draw",
        "asset": asset,
        "scene_prompt": scene_prompt,
        "output_asset": handle,
        "plate_asset": null,
        "fidelity_receipt": receipt,
        "model": DRAW_BACKGROUND_MODEL,
        "cost_usd": billing.actual_usd,
        "note": "OFFLINE DEV MODE (AGENTHAND_DRAW_PLACEHOLDER_PLATE): the plate is a deterministic local placeholder, not a generated scene; no API call was made. The compositing, lineage checks, and fidelity receipt are the real pipeline.",
    });
    result["shadow"] = composite::shadow_record_of(&receipt);
    annotate_draw_result(store, &handle, &mut full, &mut result);
    let preview = store.read_bytes(&handle)?;
    Ok((result, preview))
}

// ---------------------------------------------------------------------------
// JSON-RPC stdio transport (newline-delimited, per the MCP stdio spec).
// ---------------------------------------------------------------------------

fn tool_definitions() -> Value {
    json!([
        {
            "name": "identify",
            "description": "Plan the cut for a product photo (step inside the listing chain). Reads the photo with the vision model and returns the plan the chain consumes: a structured description, the noun-phrase concept suitable for text-prompted segmentation with at least two alternate phrasings, plus listing copy (suggested_title, listing_bullets, condition_notes). Without META_API_KEY configured the tool returns a structured NOT_CONFIGURED error and makes no call.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "image": {"type": "string", "description": "The product photo — an asset handle (asset://<id>), an http(s) URL, or a base64 data URI."}
                },
                "required": ["image"]
            }
        },
        {
            "name": "cut_out",
            "description": "Cut the product out of its photo as a transparent PNG. Calls SAM 3.1 with the concept phrase and decodes the returned mask; the mask is quality-checked and the loop retries ONCE with the first alternate phrase on a zero-match or poor mask. The foreground is the source pixels through the accepted mask, nothing regenerated. Returns the foreground and mask handles with hashes, the mask overlay and review signals, and the attempt trace.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "image": {"type": "string", "description": "The product photo — an asset handle (asset://<id>), an http(s) URL, or a base64 data URI."},
                    "concept": {"type": "string", "description": "Optional noun-phrase concept naming what to segment. When omitted, the identify planning step runs internally."}
                },
                "required": ["image"]
            }
        },
        {
            "name": "draw",
            "description": "Stage the real product in a cleaned-up version of its own photo. Fidelity Lock applies. By default (plate source_edit) the original photo is sent to muse-image-1.0's edit mode with the product region removed and filled from its surroundings, so the plate keeps the photo's camera angle, ground and light; the product stays at its original position and scale, its pixels are never sent to the image model, and a contact shadow and 1-2 px edge matte are drawn on the plate around it. If the edit call fails, a code-only plate (the masked photo, filled, blurred and brightened) stands in and the result says so. plate generated keeps the legacy text-only scene. Any subject works, surfaces included; a subject that fills the frame keeps its pixels and only its surroundings change. Returns the composite's asset handle, its fidelity receipt, and an image preview. Also returns a verify_qr sidecar asset (a QR encoding the receipt's public verify URL) and a ready-to-paste disclosure label naming that URL; the QR is a separate asset, never composited into the photo. A measured changed-opaque-pixel count other than zero (or a lost colour profile) returns a FIDELITY_VIOLATION error instead of a false receipt.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "asset": {"type": "string", "description": "Asset handle (asset://<id>) of the cut-out product layer produced by cut_out."},
                    "scene_prompt": {"type": "string", "description": "Description of the scene to generate around the product (styling only in source_edit mode; the photo's geometry is kept)."},
                    "plate": {"type": "string", "enum": ["source_edit", "generated"], "description": "How the plate is built: source_edit (default; the original photo with the product removed, edited in place) or generated (legacy text-only scene)."}
                },
                "required": ["asset", "scene_prompt"]
            }
        }
    ])
}

fn call_tool(name: &str, args: &Value) -> Result<Value, String> {
    // Panic containment at the tool boundary (2026-10-31
    // adversarial pass, deep-dive audit A1): a panic inside any
    // tool must surface as a tool error, never kill the server —
    // the SAM border-mask panic did exactly that before 7dd4bb9,
    // and the next unforeseen panic should cost one failed call,
    // not the process. The panic hook installed by run_stdio logs
    // the payload to stderr so contained faults stay visible.
    let name_owned = name.to_string();
    let args_owned = args.clone();
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        call_tool_inner(&name_owned, &args_owned)
    }))
    .unwrap_or_else(|_| {
        Err(format!(
            "{name} failed (internal fault contained; the server is still running)"
        ))
    })
}

fn call_tool_inner(name: &str, args: &Value) -> Result<Value, String> {
    // Render a tool result the way the Python server does: the dict
    // rides as a text content block plus structuredContent; draw adds
    // an image content block.
    let wrap = |result: Value, preview: Vec<u8>| -> Value {
        let mut content = vec![json!({
            "type": "text",
            "text": serde_json::to_string(&result).unwrap_or_default(),
        })];
        if !preview.is_empty() {
            use base64::Engine;
            content.push(json!({
                "type": "image",
                "data": base64::engine::general_purpose::STANDARD.encode(&preview),
                "mimeType": "image/png",
            }));
        }
        json!({
            "content": content,
            "structuredContent": result,
            "isError": false,
        })
    };
    match name {
        "identify" => {
            let image = args.get("image").and_then(|v| v.as_str()).unwrap_or("");
            Ok(wrap(tool_identify(image)?, Vec::new()))
        }
        "cut_out" => {
            let image = args.get("image").and_then(|v| v.as_str()).unwrap_or("");
            let concept = args
                .get("concept")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Ok(wrap(tool_cut_out(image, concept)?, Vec::new()))
        }
        "draw" => {
            let asset = args.get("asset").and_then(|v| v.as_str()).unwrap_or("");
            let scene = args
                .get("scene_prompt")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let plate = args.get("plate").and_then(|v| v.as_str());
            let (result, preview) = tool_draw_with_plate(asset, scene, plate)?;
            Ok(wrap(result, preview))
        }
        other => Err(format!("Unknown tool: {other}")),
    }
}

fn handle_request(request: &Value) -> Option<Value> {
    let method = request.get("method").and_then(|v| v.as_str())?;
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    if id.is_null() && request.get("id").is_none() {
        return None; // notification: no response
    }
    let result: Result<Value, String> = match method {
        "initialize" => {
            let protocol = request
                .pointer("/params/protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("2024-11-05");
            Ok(json!({
                "protocolVersion": protocol,
                "capabilities": {"tools": {}, "resources": {}},
                "serverInfo": {"name": "agenthand", "version": env!("CARGO_PKG_VERSION")},
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tool_definitions()})),
        "tools/call" => {
            let name = request
                .pointer("/params/name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let args = request
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or(json!({}));
            call_tool(name, &args)
        }
        "resources/list" => Ok(json!({"resources": []})),
        "resources/templates/list" => Ok(json!({
            "resourceTemplates": [{
                "uriTemplate": "asset://{asset_id}",
                "name": "agenthand asset",
                "mimeType": "image/png",
            }]
        })),
        "resources/read" => {
            let uri = request
                .pointer("/params/uri")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match store().read_bytes(uri) {
                Ok(bytes) => {
                    use base64::Engine;
                    Ok(json!({
                        "contents": [{
                            "uri": uri,
                            "mimeType": "image/png",
                            "blob": base64::engine::general_purpose::STANDARD.encode(&bytes),
                        }]
                    }))
                }
                Err(e) => Err(e),
            }
        }
        other => Err(format!("Method not found: {other}")),
    };
    Some(match result {
        Ok(value) => json!({"jsonrpc": "2.0", "id": id, "result": value}),
        Err(message) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32603, "message": message},
        }),
    })
}

/// Run the MCP stdio server loop until stdin closes.
pub fn run_stdio() {
    // Contained panics (tool boundary, decode boundary) must stay
    // visible: log every panic payload to stderr — stdout is the
    // protocol channel and stays clean.
    // N39 (Gauntlet fine-tooth, 2026-10-31): containment is not
    // absolution. A contained panic is a bug: whoever operates
    // this server files each distinct panic payload as a defect
    // with a regression case, not just an error response.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("agenthand: panic (contained; file as bug): {info}");
    }));
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut writer = std::io::BufWriter::new(stdout.lock());
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(response) = handle_request(&request) {
            let _ = writeln!(writer, "{response}");
            let _ = writer.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// After a plate call that SUCCEEDED, billing can still stop the
    /// draw (F5 over-cap settle, settle write or pricing error). That
    /// is not a refusal and not a failed image call: it says the call
    /// was made and paid for, counts the actual cost, carries the
    /// overage, and keeps the paid plate's handle so nothing paid for
    /// is lost. Before the call, a refusal counts what was reserved
    /// (nothing, normally).
    #[test]
    fn billing_stop_says_whether_the_plate_was_paid_for() {
        let over = billing::BillingFailure::AfterCall {
            message: "SPEND CAP EXCEEDED at settle: draw call c1 ...".to_string(),
            counted_usd: 0.95,
            actual_usd: Some(0.95),
            overage: Some(crate::ledger::SettleOverage {
                total_usd: 1.05,
                overage_usd: 0.05,
                cap_usd: 1.0,
            }),
            usage: crate::pricing::Usage::default(),
        };
        let f = billing_stop_failure(
            over,
            "c1",
            "plate edit",
            Some(Ok("asset://paidplate".to_string())),
        );
        assert_eq!(f.code, "BILLING_STOPPED_AFTER_CALL");
        assert!(!f.message.contains("Refused before any call"), "{}", f.message);
        assert!(
            f.message.starts_with(
                "Billing stopped the draw after the plate edit call succeeded; the call was made and paid for."
            ),
            "{}",
            f.message
        );
        assert!(f.message.contains("SPEND CAP EXCEEDED at settle"), "{}", f.message);
        assert!(f.message.contains("asset://paidplate"), "{}", f.message);
        assert_eq!(f.details["cost_counted_usd"], json!(0.95));
        assert_eq!(f.details["actual_usd"], json!(0.95));
        assert_eq!(f.details["over_cap_usd"], json!(0.05));
        assert_eq!(f.details["call_made"], json!(true));
        assert_eq!(f.details["call_id"], json!("c1"));
        assert_eq!(f.details["paid_plate_asset"], json!("asset://paidplate"));

        // A settle write error after a paid call: the reservation is
        // what the ledger counts; no overage; a plate that could not
        // be kept says why.
        let unsettled = billing::BillingFailure::AfterCall {
            message: "cannot lock ledger: disk full".to_string(),
            counted_usd: 0.01,
            actual_usd: Some(0.012),
            overage: None,
            usage: crate::pricing::Usage::default(),
        };
        let f = billing_stop_failure(
            unsettled,
            "c2",
            "plate generation",
            Some(Err("no space left".to_string())),
        );
        assert_eq!(f.code, "BILLING_STOPPED_AFTER_CALL");
        assert_eq!(f.details["cost_counted_usd"], json!(0.01));
        assert_eq!(f.details["actual_usd"], json!(0.012));
        assert!(f.details["over_cap_usd"].is_null());
        assert!(f.details["paid_plate_asset"].is_null());
        assert_eq!(f.details["paid_plate_error"], json!("no space left"));

        // Before the call: the spend cap's refusal counts nothing; any
        // other billing refusal is BILLING_REFUSED and counts what
        // was reserved.
        let refused = billing::BillingFailure::BeforeCall {
            message: "Spend cap: ledger total $0.00 + projected $0.0100 would exceed the $0.00 cap. Call refused before it ran.".to_string(),
            reserved_usd: 0.0,
        };
        let f = billing_stop_failure(refused, "c3", "plate edit", None);
        assert_eq!(f.code, "SPEND_CAP_REFUSED");
        assert!(f
            .message
            .starts_with("Billing refused the plate edit before any call was made: Spend cap"));
        assert_eq!(f.details["cost_counted_usd"], json!(0.0));
        assert_eq!(f.details["call_made"], json!(false));
        let mirror = billing::BillingFailure::BeforeCall {
            message: "cannot append mirror ledger".to_string(),
            reserved_usd: 0.01,
        };
        let f = billing_stop_failure(mirror, "c4", "plate edit", None);
        assert_eq!(f.code, "BILLING_REFUSED");
        assert_eq!(f.details["cost_counted_usd"], json!(0.01));
    }

    /// The sidecar's whole contract: an independent decoder reads
    /// the rendered PNG back to exactly the payload URL.
    #[test]
    fn verify_qr_png_roundtrips_through_a_decoder() {
        let url = format!(
            "{VERIFY_URL_BASE}#{}",
            "a".repeat(64)
        );
        let png = render_verify_qr_png(&url).expect("render");
        let img = image::load_from_memory(&png)
            .expect("png decodes")
            .to_luma8();
        let mut prepared = rqrr::PreparedImage::prepare(img);
        let grids = prepared.detect_grids();
        assert_eq!(grids.len(), 1, "exactly one QR in the sidecar");
        let (_meta, content) = grids[0].decode().expect("QR decodes");
        assert_eq!(content, url);
    }

    /// The draw result's angle-lock record: bucket, the exact clause,
    /// whether an angle was stated, the reason and the geometry.
    #[test]
    fn angle_lock_record_names_bucket_and_clause() {
        let angle = staging::estimate_shot_angle_from(
            1920,
            1080,
            Some((271, 0, 1419, 824)),
            0.31,
            false,
            &["top"],
        );
        let clause = meta::angle_lock_clause(Some(&angle));
        let record = angle_lock_json(&angle, &clause);
        assert_eq!(record["bucket"], "steep_down");
        assert_eq!(record["stated"], true);
        assert_eq!(
            record["clause"],
            "Keep the exact same camera angle and perspective as the original photo, same horizon position, same framing. The original photo was shot from standing height looking down at the surface at a steep angle."
        );
        assert!(record["signals"]["contact_y"].as_f64().unwrap() > 0.76);
        let none = staging::estimate_shot_angle_from(10, 10, None, 0.0, false, &[]);
        let record = angle_lock_json(&none, &meta::angle_lock_clause(Some(&none)));
        assert_eq!(record["bucket"], "eye_level");
        assert_eq!(record["stated"], false);
        assert_eq!(record["clause"], meta::ANGLE_LOCK_CLAUSE);
        assert!(record["signals"].is_null());
    }

    /// The verify hash is taken over the receipt BEFORE the two
    /// annotation fields exist; stripping them back off any
    /// persisted receipt must reproduce the hash in its URL, or
    /// the public verifier can never resolve a real receipt.
    #[test]
    fn stripping_annotations_reproduces_the_receipt_hash() {
        let receipt = json!({
            "tool": "draw",
            "pixels_checked": 580613,
            "changed_opaque_pixels": 0,
            "nested": {"b": 1, "a": [true, null]},
            "unicode": "café ☕"
        });
        let canonical = canonical_receipt_json(&receipt);
        // Sorted keys, compact separators, unescaped UTF-8: the
        // canonical form both mirrors (Python, JS) reproduce.
        assert_eq!(
            canonical,
            "{\"changed_opaque_pixels\":0,\"nested\":{\"a\":[true,null],\"b\":1},\"pixels_checked\":580613,\"tool\":\"draw\",\"unicode\":\"café ☕\"}"
        );
        let hash = crate::image::sha256_hex(canonical.as_bytes());
        let mut annotated = receipt.clone();
        annotated["verify_url"] = json!(format!("{VERIFY_URL_BASE}#{hash}"));
        annotated["qr_sha256"] = json!("b".repeat(64));
        let mut stripped = annotated.clone();
        stripped
            .as_object_mut()
            .expect("object")
            .remove("verify_url");
        stripped
            .as_object_mut()
            .expect("object")
            .remove("qr_sha256");
        assert_eq!(
            crate::image::sha256_hex(canonical_receipt_json(&stripped).as_bytes()),
            hash,
            "strip + re-canonicalize must land back on the URL hash"
        );
        // And the label line is exactly the approved wording.
        let label = format!("Digitally altered. Original photo and proof: {VERIFY_URL_BASE}#{hash}");
        assert!(label.starts_with("Digitally altered. Original photo and proof: https://"));
        assert_eq!(label.len(), "Digitally altered. Original photo and proof: ".len() + VERIFY_URL_BASE.len() + 1 + 64);
    }
}
