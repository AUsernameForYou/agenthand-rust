//! The MCP server: the three tools (identify / cut_out / draw) with
//! the same result shapes as the Python `server.py`, over a
//! hand-rolled JSON-RPC stdio loop.
//!
//! Why not the rmcp crate: the port's first job is byte-level parity
//! of the fidelity core, and the tool surface is three calls plus one
//! resource template. A hand-rolled loop keeps the wire shapes exactly
//! the Python server's (dict results as text + structuredContent,
//! draw's image preview as an image content block) with no SDK
//! version skew to audit. Revisit rmcp once parity is proven.

use crate::assets::{AssetStore, KIND_MASK, KIND_OVERLAY};
use crate::billing::{self, BillingSummary};
use crate::composite;
use crate::config;
use crate::ingest;
use crate::meta::{self, IdentifyError, CUT_OUT_MODEL, DRAW_BACKGROUND_MODEL, IDENTIFY_MODEL};
use crate::segmentation;
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
    Ok(json!({
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
    }))
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
    let provenance_json = serde_json::to_string(&mask_provenance).unwrap_or_default();
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

/// Generate a background plate through the billing choke point (port
/// of `meta_client.generate_background_plate`).
fn generate_background_plate(
    store: &AssetStore,
    prompt: &str,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<(String, BillingSummary), meta::ImageError> {
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
    let result = billing::billed_call(
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
        Err(message) => Err(err_cell.into_inner().unwrap_or(meta::ImageError {
            code: "IMAGE_CALL_FAILED".to_string(),
            message,
        })),
    }
}

/// draw: returns (result, preview PNG bytes) — Python returns
/// [result, Image]; the JSON-RPC layer renders both.
pub fn tool_draw(asset: &str, scene_prompt: &str) -> Result<(Value, Vec<u8>), String> {
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

    if config::use_placeholder_plate() {
        return draw_with_placeholder_plate(&store, asset, scene_prompt, &error);
    }

    if let Err(e) = meta::require_meta_api_key() {
        return Ok((error("NOT_CONFIGURED", &e, Value::Null), Vec::new()));
    }

    let projected = meta::project_draw_cost_usd(DRAW_BACKGROUND_MODEL)?;
    let (plate_handle, billing) = match generate_background_plate(
        &store,
        scene_prompt,
        foreground_record.width,
        foreground_record.height,
    ) {
        Ok(pair) => pair,
        Err(e) => {
            return Ok((
                error(&e.code, &e.message, json!({"cost_counted_usd": projected})),
                Vec::new(),
            ));
        }
    };

    let (handle, receipt) =
        composite::draw_product(&store, asset, scene_prompt, Some(&plate_handle))?;
    persist_full_receipt(&store, &handle, &receipt, &billing, scene_prompt)?;
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

    let result = json!({
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
        "note": "Background plate generated by muse-image-1.0 and fitted to the product by deterministic code; the stored product layer was composited untouched and never sent to the image model.",
    });
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
    persist_full_receipt(store, &handle, &receipt, &billing, scene_prompt)?;
    let result = json!({
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
            "description": "Stage the real product into a generated scene. Fidelity Lock applies. The background plate is generated by muse-image-1.0 and fitted to the product's dimensions; the stored product layer is composited onto it untouched and never sent to the image model. Returns the composite's asset handle, its fidelity receipt, and an image preview. A measured changed-opaque-pixel count other than zero (or a lost colour profile) returns a FIDELITY_VIOLATION error instead of a false receipt.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "asset": {"type": "string", "description": "Asset handle (asset://<id>) of the cut-out product layer produced by cut_out."},
                    "scene_prompt": {"type": "string", "description": "Description of the scene to generate around the product."}
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
            let (result, preview) = tool_draw(asset, scene)?;
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
