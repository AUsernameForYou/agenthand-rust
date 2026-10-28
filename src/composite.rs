//! The Fidelity Lock engine — port of the Python `composite.py`.
//!
//! Invariants, enforced the same way:
//! - the product layer is the stored foreground asset, loaded after an
//!   integrity check; never scaled, re-encoded through a model, or
//!   regenerated;
//! - the plate is resized/cropped to the product's dimensions, never
//!   the reverse (`ImageOps.fit`, byte-exact via `resample.rs`);
//! - compositing is Pillow's `alpha_composite` integer math, ported
//!   from `AlphaComposite.c`;
//! - the receipt's counts are measured on the delivered file decoded
//!   back from the store, and the colour profile identity is pinned
//!   alongside the pixel hashes.

use crate::assets::{
    AssetRecord, AssetStore, HANDLE_PREFIX, KIND_COMPOSITE, KIND_FOREGROUND, KIND_MASK,
    KIND_PLATE, KIND_SOURCE,
};
use crate::image::{describe_color_profile, GrayImage, RgbaImage};
use serde_json::{json, Map, Value};

/// A foreground pixel counts as opaque product only at this alpha
/// (255: fully opaque), in receipts and fidelity checks alike.
pub const OPAQUE_ALPHA_THRESHOLD: u8 = 255;

/// Pillow's fast divide-by-255: `(((a >> 8) + a) >> 8)`.
fn shift_for_div_255(a: u32) -> u32 {
    ((a >> 8) + a) >> 8
}

/// `Image.alpha_composite(dst, src)` — src over dst, Pillow's integer
/// implementation (PRECISION_BITS = 7) ported exactly.
pub fn alpha_composite(dst: &RgbaImage, src: &RgbaImage) -> RgbaImage {
    assert_eq!(dst.width, src.width);
    assert_eq!(dst.height, src.height);
    let mut out = vec![0u8; dst.pixels.len()];
    for i in (0..dst.pixels.len()).step_by(4) {
        let s = [
            src.pixels[i],
            src.pixels[i + 1],
            src.pixels[i + 2],
            src.pixels[i + 3],
        ];
        let d = [
            dst.pixels[i],
            dst.pixels[i + 1],
            dst.pixels[i + 2],
            dst.pixels[i + 3],
        ];
        if s[3] == 0 {
            out[i..i + 4].copy_from_slice(&d);
            continue;
        }
        let sa = s[3] as u32;
        let blend = d[3] as u32 * (255 - sa);
        let outa255 = sa * 255 + blend;
        let coef1 = sa * 255 * 255 * (1 << 7) / outa255;
        let coef2 = 255 * (1 << 7) - coef1;
        for c in 0..3 {
            let tmp = s[c] as u32 * coef1 + d[c] as u32 * coef2;
            out[i + c] = (shift_for_div_255(tmp + (0x80 << 7)) >> 7) as u8;
        }
        out[i + 3] = shift_for_div_255(outa255 + 0x80) as u8;
    }
    RgbaImage {
        width: dst.width,
        height: dst.height,
        pixels: out,
        icc_profile: src.icc_profile.clone(),
    }
}

/// The deterministic local placeholder plate (dev/offline only):
/// a vertical gradient whose colours derive from the prompt hash.
pub fn generate_background_plate(prompt: &str, width: u32, height: u32) -> RgbaImage {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(prompt.as_bytes());
    let top = [digest[0], digest[1], digest[2]];
    let bottom = [digest[3], digest[4], digest[5]];
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        let t = y as f64 / (height.saturating_sub(1)).max(1) as f64;
        let mut color = [0u8; 4];
        for c in 0..3 {
            color[c] = (top[c] as f64 + (bottom[c] as f64 - top[c] as f64) * t) as u8;
        }
        color[3] = 255;
        for x in 0..width {
            let i = ((y * width + x) * 4) as usize;
            pixels[i..i + 4].copy_from_slice(&color);
        }
    }
    RgbaImage::new(width, height, pixels)
}

/// Resize/crop the PLATE to the product's dimensions. Never the reverse.
fn fit_plate(plate: &RgbaImage, width: u32, height: u32) -> (RgbaImage, Vec<String>) {
    if plate.width == width && plate.height == height {
        return (plate.clone(), Vec::new());
    }
    let fitted = crate::resample::fit(plate, width, height);
    (
        fitted,
        vec![format!(
            "plate_resized_and_center_cropped_to_{}x{}",
            width, height
        )],
    )
}

/// Manhattan distance from every pixel to the nearest target pixel
/// (two-pass chamfer transform). When no pixel matches the target,
/// distance is measured to the image border instead.
fn distance_to_class(alpha: &[u8], width: usize, height: usize, target: u8) -> Vec<i64> {
    let total = width * height;
    if !alpha.iter().any(|&a| a == target) {
        let mut out = vec![0i64; total];
        for y in 0..height {
            for x in 0..width {
                out[y * width + x] = ((x + 1).min(width - x).min(y + 1).min(height - y)) as i64;
            }
        }
        return out;
    }
    let inf = total as i64 + 1;
    let mut dist: Vec<i64> = alpha
        .iter()
        .map(|&a| if a == target { 0 } else { inf })
        .collect();
    for y in 0..height {
        let base = y * width;
        for x in 0..width {
            let i = base + x;
            if dist[i] != 0 {
                if x > 0 && dist[i - 1] + 1 < dist[i] {
                    dist[i] = dist[i - 1] + 1;
                }
                if y > 0 && dist[i - width] + 1 < dist[i] {
                    dist[i] = dist[i - width] + 1;
                }
            }
        }
    }
    for y in (0..height).rev() {
        let base = y * width;
        for x in (0..width).rev() {
            let i = base + x;
            if dist[i] != 0 {
                if x < width - 1 && dist[i + 1] + 1 < dist[i] {
                    dist[i] = dist[i + 1] + 1;
                }
                if y < height - 1 && dist[i + width] + 1 < dist[i] {
                    dist[i] = dist[i + width] + 1;
                }
            }
        }
    }
    dist
}

/// The edge band's maximum width: the longest shortest path through
/// band pixels from the opaque region to the transparent region.
fn edge_band_max_width(alpha: &[u8], width: usize, height: usize) -> i64 {
    if width == 0 || height == 0 {
        return 0;
    }
    if !alpha.iter().any(|&a| a > 0 && a < 255) {
        return 0;
    }
    let to_opaque = distance_to_class(alpha, width, height, 255);
    let to_transparent = distance_to_class(alpha, width, height, 0);
    alpha
        .iter()
        .enumerate()
        .filter(|(_, &a)| a > 0 && a < 255)
        .map(|(i, _)| to_opaque[i] + to_transparent[i] - 1)
        .max()
        .unwrap_or(0)
}

use crate::round6;

#[derive(Debug, Clone)]
pub struct Measure {
    pub opaque_pixels: u64,
    pub edge_band_pixels: u64,
    pub transparent_pixels: u64,
    pub changed_opaque_pixels: u64,
    pub mask_pixels_total: u64,
    pub opaque_fraction: f64,
    pub edge_band_max_width_px: i64,
}

/// Count opaque / edge-band / transparent pixels and verify the lock
/// (port of `_measure`).
pub fn measure(foreground: &RgbaImage, composite: &RgbaImage) -> Measure {
    assert_eq!(foreground.width, composite.width);
    assert_eq!(foreground.height, composite.height);
    let alpha = foreground.alpha_plane();
    let mut opaque = 0u64;
    let mut edge_band = 0u64;
    let mut transparent = 0u64;
    let mut changed_opaque = 0u64;
    for (i, &a) in alpha.iter().enumerate() {
        if a == OPAQUE_ALPHA_THRESHOLD {
            opaque += 1;
            let j = i * 4;
            if foreground.pixels[j..j + 4] != composite.pixels[j..j + 4] {
                changed_opaque += 1;
            }
        } else if a > 0 {
            edge_band += 1;
        } else {
            transparent += 1;
        }
    }
    let mask_total = opaque + edge_band;
    Measure {
        opaque_pixels: opaque,
        edge_band_pixels: edge_band,
        transparent_pixels: transparent,
        changed_opaque_pixels: changed_opaque,
        mask_pixels_total: mask_total,
        opaque_fraction: if mask_total > 0 {
            round6(opaque as f64 / mask_total as f64)
        } else {
            0.0
        },
        edge_band_max_width_px: edge_band_max_width(
            &alpha,
            foreground.width as usize,
            foreground.height as usize,
        ),
    }
}

/// Composite a foreground over a plate; (image, counts, transforms).
/// The composite carries the FOREGROUND's ICC profile (Pillow's
/// alpha_composite drops it; the Python pipeline re-attaches it).
pub fn composite_images(
    foreground: &RgbaImage,
    plate: &RgbaImage,
) -> (RgbaImage, Measure, Vec<String>) {
    let (fitted, transformations) = fit_plate(plate, foreground.width, foreground.height);
    let mut out = alpha_composite(&fitted, foreground);
    out.icc_profile = foreground.icc_profile.clone();
    let counts = measure(foreground, &out);
    (out, counts, transformations)
}

/// Check an asset may be composited; returns its lineage chain.
pub fn validate_foreground(
    store: &AssetStore,
    foreground_handle: &str,
) -> Result<Vec<AssetRecord>, String> {
    let record = store.get_record(foreground_handle)?;
    if record.kind != KIND_FOREGROUND {
        return Err(format!(
            "draw requires a {KIND_FOREGROUND} asset (the cut-out product layer); {foreground_handle:?} is kind {:?}. Compositing anything else would break the Fidelity Lock.",
            record.kind
        ));
    }
    let chain = store.lineage(foreground_handle)?;
    let source_record = chain.last().expect("lineage is non-empty");
    if source_record.kind != KIND_SOURCE {
        return Err(format!(
            "Foreground {foreground_handle:?} traces back to kind {:?}, not a {KIND_SOURCE}. Its provenance cannot be verified, so it cannot be composited.",
            source_record.kind
        ));
    }
    store.verify_integrity(foreground_handle)?;
    if source_record.asset_id != record.asset_id {
        store.verify_integrity(&(HANDLE_PREFIX.to_string() + &source_record.asset_id))?;
    }
    Ok(chain)
}

/// Composite a stored foreground into a scene; (handle, receipt).
pub fn draw_product(
    store: &AssetStore,
    foreground_handle: &str,
    scene_prompt: &str,
    plate_handle: Option<&str>,
) -> Result<(String, Value), String> {
    let chain = validate_foreground(store, foreground_handle)?;
    let record = chain[0].clone();
    let source_record = chain[chain.len() - 1].clone();

    let foreground = store.read_image(foreground_handle)?;

    let (mask_asset, mask_sha256, mask_pixel_sha256, mask_source) =
        match chain.iter().find(|r| r.kind == KIND_MASK) {
            Some(mask_record) => (
                Some(HANDLE_PREFIX.to_string() + &mask_record.asset_id),
                Some(mask_record.sha256.clone()),
                mask_record.pixel_sha256.clone(),
                "asset",
            ),
            None => {
                let mask_gray = GrayImage {
                    width: foreground.width,
                    height: foreground.height,
                    pixels: foreground.alpha_plane(),
                    icc_profile: None,
                };
                let bytes = crate::image::encode_png_gray(&mask_gray);
                (
                    None,
                    Some(crate::image::sha256_hex(&bytes)),
                    Some(mask_gray.to_rgba().pixel_sha256()),
                    "foreground_alpha",
                )
            }
        };

    let (plate, plate_source, plate_asset, plate_sha256, plate_pixel_sha256) =
        match plate_handle {
            Some(handle) => {
                let plate_record = store.get_record(handle)?;
                if plate_record.kind != KIND_PLATE {
                    return Err(format!(
                        "Background plate {handle:?} is kind {:?}, not {KIND_PLATE}.",
                        plate_record.kind
                    ));
                }
                let plate = store.read_image(handle)?;
                (
                    plate,
                    "asset",
                    Some(handle.to_string()),
                    Some(plate_record.sha256.clone()),
                    plate_record.pixel_sha256.clone(),
                )
            }
            None => {
                let plate =
                    generate_background_plate(scene_prompt, foreground.width, foreground.height);
                (plate, "local_placeholder", None, None, None)
            }
        };

    let (composite, _in_memory_counts, transformations) = composite_images(&foreground, &plate);
    let composite_handle = store.put_image(
        &composite,
        KIND_COMPOSITE,
        Some(&record.asset_id),
    )?;
    let composite_record = store.get_record(&composite_handle)?;

    // Counts measured on the delivered file, decoded back from the store.
    let delivered_fg = store.read_image(foreground_handle)?;
    let delivered_out = store.read_image(&composite_handle)?;
    let counts = measure(&delivered_fg, &delivered_out);

    let color_profile_preserved =
        source_record.color_profile_sha256 == composite_record.color_profile_sha256;

    let receipt = json!({
        "source_asset": HANDLE_PREFIX.to_string() + &source_record.asset_id,
        "source_sha256": source_record.sha256,
        "source_pixel_sha256": source_record.pixel_sha256,
        "foreground_asset": foreground_handle,
        "foreground_sha256": record.sha256,
        "foreground_pixel_sha256": record.pixel_sha256,
        "mask_asset": mask_asset,
        "mask_sha256": mask_sha256,
        "mask_pixel_sha256": mask_pixel_sha256,
        "mask_source": mask_source,
        "changed_opaque_pixels": counts.changed_opaque_pixels,
        "opaque_pixels": counts.opaque_pixels,
        "edge_band_pixels": counts.edge_band_pixels,
        "transparent_pixels": counts.transparent_pixels,
        "mask_pixels_total": counts.mask_pixels_total,
        "opaque_fraction": counts.opaque_fraction,
        "edge_band_max_width_px": counts.edge_band_max_width_px,
        "opaque_alpha_threshold": OPAQUE_ALPHA_THRESHOLD,
        "changed_predicate": "a pixel counts as changed when its RGBA value in the composite differs at all from the stored foreground at a position where the foreground alpha equals opaque_alpha_threshold; edge-band pixels (0 < alpha < threshold) blend by design, are excluded from the count, and are published above as edge_band_pixels and edge_band_max_width_px",
        "pixel_tolerance": 0,
        "count_sample": "decoded_delivered_file",
        "output_asset": composite_handle,
        "output_sha256": composite_record.sha256,
        "output_pixel_sha256": composite_record.pixel_sha256,
        "output_format": "PNG",
        "color_space": describe_color_profile(delivered_out.icc_profile.as_deref()),
        "color_profile_source_sha256": source_record.color_profile_sha256,
        "color_profile_foreground_sha256": record.color_profile_sha256,
        "color_profile_output_sha256": composite_record.color_profile_sha256,
        "color_profile_preserved": color_profile_preserved,
        "transformations": transformations,
        "product_layer_scaled": false,
        "plate_source": plate_source,
        "plate_asset": plate_asset,
        "plate_sha256": plate_sha256,
        "plate_pixel_sha256": plate_pixel_sha256,
        "claim": "The stored extracted foreground reached the composite unchanged: every fully opaque foreground pixel in the composite is identical to the stored foreground layer (foreground hashes above pin that layer). Edge-band pixels blend with the plate by design and are counted separately. The composite carries the source photo's colour profile unchanged (color_profile_preserved above); a profile change is a fidelity failure even when pixel values match."
    });
    // Persist the receipt beside the composite, as the Python
    // compositor does: a receipt that exists only in the tool's
    // return value is gone if the client drops it. The server layer
    // enriches this same file with the billed call's cost and
    // attribution once billing settles.
    store.save_receipt(&composite_handle, &receipt)?;
    Ok((composite_handle, receipt))
}

/// Independently re-verify a draw receipt from the stored FILES
/// (port of `verify_receipt_from_disk`). Returns a report object:
/// ok / checks / problems / recomputed.
///
/// The receipt may be omitted: draw persists it beside the
/// composite and it is loaded from there, so a composite found in
/// the store carries the claims to check with it. A supplied
/// receipt takes precedence. An error is returned when no receipt
/// is supplied and none is persisted for the composite.
pub fn verify_receipt_from_disk(
    store: &AssetStore,
    composite_handle: &str,
    receipt: Option<&Value>,
) -> Result<Value, String> {
    let loaded;
    let receipt = match receipt {
        Some(r) => r,
        None => {
            loaded = store.load_receipt(composite_handle)?.ok_or_else(|| {
                format!(
                    "No receipt supplied and none persisted beside \
                     '{composite_handle}'; there are no claims to verify."
                )
            })?;
            &loaded
        }
    };
    let mut checks: Map<String, Value> = Map::new();
    let mut problems: Vec<String> = Vec::new();
    let mut set_check = |checks: &mut Map<String, Value>,
                         problems: &mut Vec<String>,
                         name: &str,
                         ok: bool,
                         detail: &str| {
        checks.insert(name.to_string(), Value::Bool(ok));
        if !ok {
            problems.push(if detail.is_empty() {
                name.to_string()
            } else {
                format!("{name}: {detail}")
            });
        }
    };

    let composite_record = store.get_record(composite_handle)?;
    let foreground_handle = receipt["foreground_asset"]
        .as_str()
        .ok_or("receipt has no foreground_asset")?
        .to_string();
    let source_handle = receipt["source_asset"]
        .as_str()
        .ok_or("receipt has no source_asset")?
        .to_string();

    for (label, handle) in [
        ("source", source_handle.as_str()),
        ("foreground", foreground_handle.as_str()),
        ("composite", composite_handle),
    ] {
        match store.verify_integrity(handle) {
            Ok(()) => set_check(
                &mut checks,
                &mut problems,
                &format!("{label}_file_integrity"),
                true,
                "",
            ),
            Err(e) => set_check(
                &mut checks,
                &mut problems,
                &format!("{label}_file_integrity"),
                false,
                &e,
            ),
        }
    }

    // 1b. The receipt's own hashes must name THESE files. Step 1
    // only proves each file matches its own asset record; without
    // this step, run A's receipt verifies against run B's composite
    // whenever the numbers happen to reproduce.
    let source_record = store.get_record(&source_handle)?;
    let foreground_record = store.get_record(&foreground_handle)?;
    set_check(
        &mut checks,
        &mut problems,
        "source_hash_matches_receipt",
        receipt["source_sha256"].as_str() == Some(source_record.sha256.as_str())
            && receipt["source_pixel_sha256"].as_str()
                == source_record.pixel_sha256.as_deref(),
        "the receipt's source hashes do not match the source asset's recorded hashes; this receipt was not issued for this source",
    );
    set_check(
        &mut checks,
        &mut problems,
        "foreground_hash_matches_receipt",
        receipt["foreground_sha256"].as_str() == Some(foreground_record.sha256.as_str())
            && receipt["foreground_pixel_sha256"].as_str()
                == foreground_record.pixel_sha256.as_deref(),
        "the receipt's foreground hashes do not match the foreground asset's recorded hashes",
    );
    set_check(
        &mut checks,
        &mut problems,
        "output_hash_matches_receipt",
        receipt["output_asset"].as_str() == Some(composite_handle)
            && receipt["output_sha256"].as_str() == Some(composite_record.sha256.as_str())
            && receipt["output_pixel_sha256"].as_str()
                == composite_record.pixel_sha256.as_deref(),
        "the receipt does not name this composite (its output_asset / output hashes point elsewhere); this receipt was issued for a different composite, even if the files happen to be byte-identical",
    );
    if let Some(mask_handle) = receipt["mask_asset"].as_str() {
        let pins_hashes = receipt["mask_sha256"].as_str().is_some()
            || receipt["mask_pixel_sha256"].as_str().is_some();
        if pins_hashes {
            match store.get_record(mask_handle) {
                Ok(mask_record) => set_check(
                    &mut checks,
                    &mut problems,
                    "mask_hash_matches_receipt",
                    receipt["mask_sha256"].as_str() == Some(mask_record.sha256.as_str())
                        && receipt["mask_pixel_sha256"].as_str()
                            == mask_record.pixel_sha256.as_deref(),
                    "the receipt's mask hashes do not match the mask asset's recorded hashes",
                ),
                Err(e) => set_check(
                    &mut checks,
                    &mut problems,
                    "mask_hash_matches_receipt",
                    false,
                    &e,
                ),
            }
        } else {
            // A receipt that names a mask asset without pinning its
            // hashes (issued before mask pinning — the frozen replay
            // package's receipt is the live example): the strongest
            // check available is the mask file's integrity against
            // its own asset record.
            match store.verify_integrity(mask_handle) {
                Ok(()) => set_check(
                    &mut checks,
                    &mut problems,
                    "mask_asset_integrity",
                    true,
                    "",
                ),
                Err(e) => set_check(
                    &mut checks,
                    &mut problems,
                    "mask_asset_integrity",
                    false,
                    &e,
                ),
            }
        }
    }

    let foreground = store.read_image(&foreground_handle)?;
    let composite_image = store.read_image(composite_handle)?;
    set_check(
        &mut checks,
        &mut problems,
        "foreground_pixel_hash",
        foreground.pixel_sha256()
            == receipt["foreground_pixel_sha256"].as_str().unwrap_or(""),
        "decoded foreground pixels do not match the receipt's pinned hash",
    );
    set_check(
        &mut checks,
        &mut problems,
        "composite_pixel_hash",
        Some(composite_image.pixel_sha256()) == composite_record.pixel_sha256,
        "decoded composite pixels do not match the stored asset record",
    );

    let mut recomputed: Map<String, Value> = Map::new();
    if foreground.width != composite_image.width || foreground.height != composite_image.height {
        set_check(
            &mut checks,
            &mut problems,
            "recount",
            false,
            &format!(
                "foreground is {}x{}, composite is {}x{}; sizes must match exactly",
                foreground.width, foreground.height, composite_image.width, composite_image.height
            ),
        );
    } else {
        let counts = measure(&foreground, &composite_image);
        recomputed.insert("changed_opaque_pixels".into(), json!(counts.changed_opaque_pixels));
        recomputed.insert("opaque_pixels".into(), json!(counts.opaque_pixels));
        recomputed.insert("edge_band_pixels".into(), json!(counts.edge_band_pixels));
        recomputed.insert("transparent_pixels".into(), json!(counts.transparent_pixels));
        recomputed.insert("mask_pixels_total".into(), json!(counts.mask_pixels_total));
        recomputed.insert("opaque_fraction".into(), json!(counts.opaque_fraction));
        recomputed.insert("edge_band_max_width_px".into(), json!(counts.edge_band_max_width_px));
        for field in [
            "changed_opaque_pixels",
            "opaque_pixels",
            "edge_band_pixels",
            "transparent_pixels",
            "mask_pixels_total",
            "edge_band_max_width_px",
        ] {
            let mine = recomputed[field].as_u64().unwrap_or(0) as i64;
            let theirs = receipt[field].as_i64().unwrap_or(-1);
            set_check(
                &mut checks,
                &mut problems,
                &format!("recount_{field}"),
                mine == theirs,
                &format!("receipt says {theirs}, recount from disk finds {mine}"),
            );
        }
        let frac_diff = (counts.opaque_fraction
            - receipt["opaque_fraction"].as_f64().unwrap_or(f64::NAN))
        .abs();
        set_check(
            &mut checks,
            &mut problems,
            "recount_opaque_fraction",
            frac_diff < 1e-9,
            "receipt and recount disagree on opaque_fraction",
        );
    }

    // 2b. The verdict itself, stated as checks rather than implied
    // by agreement: a pass requires ZERO changed pixels — an honest
    // receipt reporting a real change reproduces perfectly under
    // the recount checks above and must still fail — and POSITIVE
    // coverage, because a receipt covering no opaque pixels vouches
    // for nothing.
    let show = |v: Option<i64>| match v {
        Some(n) => n.to_string(),
        None => "None".to_string(),
    };
    let recount_changed = recomputed["changed_opaque_pixels"].as_i64();
    let recount_opaque = recomputed["opaque_pixels"].as_i64();
    set_check(
        &mut checks,
        &mut problems,
        "zero_changed_pixels",
        recount_changed == Some(0)
            && receipt["changed_opaque_pixels"].as_i64() == Some(0),
        &format!(
            "changed opaque pixels: receipt says {}, recount from disk finds {}; the Fidelity Lock claim requires exactly 0",
            show(receipt["changed_opaque_pixels"].as_i64()),
            show(recount_changed)
        ),
    );
    set_check(
        &mut checks,
        &mut problems,
        "positive_coverage",
        recount_opaque.map(|v| v > 0).unwrap_or(false)
            && receipt["opaque_pixels"].as_i64().map(|v| v > 0).unwrap_or(false),
        "the receipt covers 0 fully opaque foreground pixels; there is no product evidence to verify",
    );

    let source_image = store.read_image(&source_handle)?;
    let source_profile = source_image.color_profile_sha256();
    let output_profile = composite_image.color_profile_sha256();
    recomputed.insert(
        "color_profile_source_sha256".into(),
        json!(source_profile),
    );
    recomputed.insert(
        "color_profile_output_sha256".into(),
        json!(output_profile),
    );
    set_check(
        &mut checks,
        &mut problems,
        "color_profile_matches_receipt",
        source_profile.as_deref() == receipt["color_profile_source_sha256"].as_str()
            && output_profile.as_deref() == receipt["color_profile_output_sha256"].as_str(),
        "profiles decoded from the files differ from the receipt's recorded profile hashes",
    );
    let preserved = source_profile == output_profile;
    recomputed.insert("color_profile_preserved".into(), json!(preserved));
    set_check(
        &mut checks,
        &mut problems,
        "color_profile_preserved",
        preserved && receipt["color_profile_preserved"].as_bool() == Some(true),
        "the delivered composite's colour profile differs from the source photo's; identical pixel values would display as different colours",
    );

    let ok = checks.values().all(|v| v.as_bool().unwrap_or(false));
    Ok(json!({
        "ok": ok,
        "checks": checks,
        "problems": problems,
        "recomputed": recomputed,
        "composite_asset": composite_handle,
        "foreground_asset": foreground_handle,
        "source_asset": source_handle,
    }))
}

/// Standalone verification of exported files (port of `verify.py`):
/// SOURCE photo, FINAL composite, MASK png; no store involved.
pub fn verify_files(
    source_path: &std::path::Path,
    final_path: &std::path::Path,
    mask_path: &std::path::Path,
) -> Result<Value, String> {
    let load_rgba = |p: &std::path::Path| -> Result<RgbaImage, String> {
        let data = std::fs::read(p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
        crate::image::decode_image(&data)
            .map(|d| d.into_rgba())
            .map_err(|e| format!("{}: {e}", p.display()))
    };
    let source = load_rgba(source_path)?;
    let final_img = load_rgba(final_path)?;
    let mask_data = std::fs::read(mask_path)
        .map_err(|e| format!("cannot read {}: {e}", mask_path.display()))?;
    let mask = crate::image::decode_image(&mask_data)
        .map_err(|e| format!("{}: {e}", mask_path.display()))?;
    let mask_gray: Vec<u8> = match &mask {
        crate::image::DecodedImage::Gray(g) => g.pixels.clone(),
        crate::image::DecodedImage::Rgba(r) => r
            .pixels
            .chunks_exact(4)
            .map(|px| {
                ((px[0] as u32 * 19595 + px[1] as u32 * 38470 + px[2] as u32 * 7471 + 0x8000)
                    >> 16) as u8
            })
            .collect(),
    };
    let (mask_w, mask_h) = match &mask {
        crate::image::DecodedImage::Gray(g) => (g.width, g.height),
        crate::image::DecodedImage::Rgba(r) => (r.width, r.height),
    };

    let mut problems: Vec<String> = Vec::new();
    if (final_img.width, final_img.height) != (source.width, source.height) {
        problems.push(format!(
            "final is {}x{}, source is {}x{}; the product canvas must never be rescaled",
            final_img.width, final_img.height, source.width, source.height
        ));
    }
    if (mask_w, mask_h) != (source.width, source.height) {
        problems.push(format!(
            "mask is {}x{}, source is {}x{}; the mask must cover the source frame exactly",
            mask_w, mask_h, source.width, source.height
        ));
    }

    let (mut checked, mut changed, mut edge_band, mut outside) = (0u64, 0u64, 0u64, 0u64);
    if problems.is_empty() {
        for (i, &m) in mask_gray.iter().enumerate() {
            if m == 255 {
                checked += 1;
                let j = i * 4;
                if source.pixels[j..j + 4] != final_img.pixels[j..j + 4] {
                    changed += 1;
                }
            } else if m > 0 {
                edge_band += 1;
            } else {
                outside += 1;
            }
        }
        if changed > 0 {
            problems.push(format!(
                "{changed} of {checked} fully-masked product pixels differ between the source photo and the final image"
            ));
        }
        if checked == 0 {
            // A mask that selects nothing checks nothing: without
            // this refusal, two completely different images pass
            // with an empty mask because 0 of 0 pixels changed.
            problems.push(
                "the mask selects no fully-on pixels, so no product pixels were checked; a verification that compares nothing does not pass"
                    .to_string(),
            );
        }
    }

    let source_profile = source.color_profile_sha256();
    let output_profile = final_img.color_profile_sha256();
    let preserved = source_profile == output_profile;
    if !preserved {
        problems.push(
            "colour profile changed between the source photo and the final image; identical pixel values would display as different colours"
                .to_string(),
        );
    }

    Ok(json!({
        "ok": problems.is_empty(),
        "source": source_path.display().to_string(),
        "final": final_path.display().to_string(),
        "mask": mask_path.display().to_string(),
        "checked_pixels": checked,
        "changed_pixels": changed,
        "edge_band_pixels": edge_band,
        "outside_mask_pixels": outside,
        "color_profile_source_sha256": source_profile,
        "color_profile_output_sha256": output_profile,
        "color_profile_preserved": preserved,
        "problems": problems,
    }))
}
