//! Workflow completion (decision 61): crop listing copy, narration
//! origins, code-rendered backdrop stages, and market pricing.
//! Fully offline: the chain is faked at the roomrun boundary, draw
//! runs the production compositor locally, and no test can reach
//! the network or spend.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use agenthand::assets::{AssetStore, KIND_CROP, KIND_FOREGROUND};
use agenthand::backdrop;
use agenthand::image::{encode_png_rgba, RgbaImage};
use agenthand::market_pricing::{
    percentile_type7, pricing_for_item, round_to_nearest_5, usable_prices, PRICED_TABLE_FOOTNOTE,
};
use agenthand::roomrun::{run_room, NarrationFn, RoomChain, RoomRunOptions};
use agenthand::{composite, ingest};
use serde_json::{json, Value};

static ENV_LOCK: Mutex<()> = Mutex::new(());
static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn temp_root(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "agenthand-workflow-{tag}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn photo(dir: &Path, name: &str, color: [u8; 4]) -> PathBuf {
    let path = dir.join(name);
    let img = RgbaImage::solid(48, 36, color);
    std::fs::write(&path, encode_png_rgba(&img)).unwrap();
    path
}

fn plan(concept: &str) -> Value {
    json!({
        "description": format!("a photo whose primary subject is {concept}"),
        "concept": concept,
        "concept_rewritten_from": Value::Null,
        "alternates": [],
        "suggested_title": format!("Listing: {concept}"),
        "listing_bullets": [format!("one {concept}")],
        "condition_notes": "good",
        "model": "fake",
    })
}

const IDENTIFY_COST: f64 = 0.008;
const CUT_COST: f64 = 0.0025;

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// A chain whose identify answers the photo plan for photos and a
/// separate crop plan for item-crop assets, whose cut_out returns a
/// lineage-correct foreground for known concepts, and whose draw
/// runs the production compositor.
struct Chain {
    store: AssetStore,
    photo_plan: Value,
    crop_plan: Option<Value>,
    known: HashSet<String>,
    spend: RefCell<f64>,
    draw_calls: RefCell<usize>,
    crop_identify_calls: RefCell<usize>,
    cut_error_code: RefCell<String>,
}

impl Chain {
    fn new(
        data_dir: &Path,
        photo_plan: Value,
        crop_plan: Option<Value>,
        known: &[&str],
    ) -> Rc<Self> {
        Rc::new(Chain {
            store: AssetStore::new(data_dir),
            photo_plan,
            crop_plan,
            known: known.iter().map(|s| s.to_string()).collect(),
            spend: RefCell::new(0.0),
            draw_calls: RefCell::new(0),
            crop_identify_calls: RefCell::new(0),
            cut_error_code: RefCell::new("NO_MATCH".to_string()),
        })
    }

    fn identify(&self, image_ref: &str) -> Result<Value, String> {
        *self.spend.borrow_mut() += IDENTIFY_COST;
        let is_crop = image_ref
            .strip_prefix("asset://")
            .and_then(|_| self.store.get_record(image_ref).ok())
            .map(|record| record.kind == KIND_CROP)
            .unwrap_or(false);
        if is_crop {
            *self.crop_identify_calls.borrow_mut() += 1;
            return match &self.crop_plan {
                Some(crop_plan) => {
                    let mut result = json!({
                        "status": "ok",
                        "image": image_ref,
                        "cost_usd": IDENTIFY_COST,
                    });
                    for (key, value) in crop_plan.as_object().unwrap() {
                        result[key] = value.clone();
                    }
                    Ok(result)
                }
                None => Ok(json!({
                    "status": "error",
                    "error": {"code": "BAD_MODEL_RESPONSE", "message": "crop unreadable", "cost_counted_usd": IDENTIFY_COST},
                })),
            };
        }
        let handle = ingest::ingest(image_ref, &self.store)?;
        let mut result = json!({
            "status": "ok",
            "image": handle,
            "cost_usd": IDENTIFY_COST,
        });
        for (key, value) in self.photo_plan.as_object().unwrap() {
            result[key] = value.clone();
        }
        Ok(result)
    }

    fn cut_out(&self, handle: &str, concept: &str) -> Result<Value, String> {
        *self.spend.borrow_mut() += CUT_COST;
        if !self.known.contains(concept) {
            return Ok(json!({
                "status": "error",
                "error": {"code": self.cut_error_code.borrow().clone(), "message": "not in this photo", "cost_counted_usd": CUT_COST},
            }));
        }
        let mut pixels = vec![0u8; 48 * 36 * 4];
        for y in 0..36usize {
            for x in 0..48usize {
                let i = (y * 48 + x) * 4;
                pixels[i..i + 4].copy_from_slice(if x < 20 {
                    &[200, 30, 30, 255]
                } else if x == 20 {
                    &[0, 200, 30, 128]
                } else {
                    &[0, 0, 0, 0]
                });
            }
        }
        let fg = self.store.put_image(
            &RgbaImage::new(48, 36, pixels),
            KIND_FOREGROUND,
            Some(&AssetStore::parse_handle(handle)?),
        )?;
        Ok(json!({
            "status": "ok",
            "output_asset": fg,
            "mask_asset": Value::Null,
            "mask_overlay_asset": Value::Null,
            "mask_review": {"bbox": [4, 4, 30, 30], "warnings": []},
            "cost_usd": CUT_COST,
        }))
    }

    fn draw(&self, fg_handle: &str, scene: &str) -> Result<Value, String> {
        *self.draw_calls.borrow_mut() += 1;
        let (handle, receipt) = composite::draw_product(&self.store, fg_handle, scene, None)?;
        Ok(json!({
            "status": "ok",
            "output_asset": handle,
            "fidelity_receipt": receipt,
            "cost_usd": 0.0,
        }))
    }

    fn chain(self: &Rc<Self>) -> RoomChain {
        let a = self.clone();
        let b = self.clone();
        let c = self.clone();
        RoomChain {
            identify: Box::new(move |image_ref: &str| a.identify(image_ref)),
            cut_out: Box::new(move |handle: &str, concept: &str| b.cut_out(handle, concept)),
            draw: Box::new(move |handle: &str, scene: &str| c.draw(handle, scene)),
        }
    }

    fn base_opts(self: &Rc<Self>, data_dir: &Path) -> RoomRunOptions {
        let fake = self.clone();
        RoomRunOptions {
            budget_usd: 5.00,
            store: Some(AssetStore::new(data_dir)),
            chain: Some(self.chain()),
            spend_probe: Some(Box::new(move || Ok(*fake.spend.borrow()))),
            // no verify_fn: the production verifier grades the draws
            ..RoomRunOptions::default()
        }
    }
}

fn items_by_name(manifest: &Value) -> std::collections::HashMap<String, Value> {
    manifest["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| (item["name"].as_str().unwrap().to_string(), item.clone()))
        .collect()
}

// ----------------------------------------------------------------------
// S1: listing copy via the item crop
// ----------------------------------------------------------------------

#[test]
fn crop_listing_supplies_copy_when_photo_plan_is_about_something_else() {
    let _guard = env_lock();
    let root = temp_root("crop");
    let photos = vec![photo(&root, "shelf.png", [120, 60, 20, 255])];
    let chain = Chain::new(
        &root,
        plan("white bookcase"),
        Some(plan("straw hat")),
        &["straw hat"],
    );
    let mut opts = chain.base_opts(&root);
    opts.item_names = vec!["straw hat".to_string()];
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    let item = &manifest["items"][0];
    assert_eq!(item["status"], "staged");
    assert_eq!(item["listing"]["source"], "item_crop");
    assert_eq!(item["listing"]["title"], "Listing: straw hat");
    assert_eq!(item["listing"]["bullets"], json!(["one straw hat"]));
    assert_eq!(*chain.crop_identify_calls.borrow(), 1);
    // The crop is a stored asset, sized from the sighting bbox
    // [4,4,30,30] on the 48x36 photo plus the 10% margin, clamped.
    let crop_handle = item["listing"]["crop_asset"].as_str().unwrap();
    let record = chain.store.get_record(crop_handle).unwrap();
    assert_eq!(record.kind, KIND_CROP);
    assert_eq!(record.width, Some(32));
    assert_eq!(record.height, Some(32));
    // The crop identify is billed into the item's cost.
    assert!(approx(
        item["cost_usd"].as_f64().unwrap(),
        CUT_COST + IDENTIFY_COST
    ));
    assert_eq!(item["origin"], "named");
}

#[test]
fn crop_listing_failure_falls_back_to_none() {
    let _guard = env_lock();
    let root = temp_root("cropfail");
    let photos = vec![photo(&root, "shelf.png", [120, 60, 20, 255])];
    let chain = Chain::new(&root, plan("white bookcase"), None, &["straw hat"]);
    let mut opts = chain.base_opts(&root);
    opts.item_names = vec!["straw hat".to_string()];
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    let item = &manifest["items"][0];
    assert_eq!(item["status"], "staged");
    assert_eq!(item["listing"]["source"], "none");
    assert!(item["listing"]["crop_asset"].is_null());
    assert!(item["listing"]["title"].is_null());
    assert!(item["listing"]["note"]
        .as_str()
        .unwrap()
        .ends_with("the item-crop identify attempt failed"));
    // The failed crop identify's counted cost still lands on the item.
    assert!(approx(
        item["cost_usd"].as_f64().unwrap(),
        CUT_COST + IDENTIFY_COST
    ));
}

#[test]
fn photo_plan_listing_carries_a_null_crop_asset() {
    let _guard = env_lock();
    let root = temp_root("photoplan");
    let photos = vec![photo(&root, "shelf.png", [120, 60, 20, 255])];
    let chain = Chain::new(&root, plan("straw hat"), None, &["straw hat"]);
    let mut opts = chain.base_opts(&root);
    opts.item_names = vec!["straw hat".to_string()];
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    let item = &manifest["items"][0];
    assert_eq!(item["listing"]["source"], "photo_plan");
    assert!(item["listing"]["crop_asset"].is_null());
    assert_eq!(*chain.crop_identify_calls.borrow(), 0);
}

// ----------------------------------------------------------------------
// S2: narration merge + origins
// ----------------------------------------------------------------------

fn wav_file(path: &Path, seconds: f64, rate: u32, peak: f64) -> PathBuf {
    let frames = (seconds * rate as f64) as usize;
    let mut data: Vec<u8> = Vec::with_capacity(44 + frames * 2);
    let data_len = (frames * 2) as u32;
    data.extend_from_slice(b"RIFF");
    data.extend_from_slice(&(36 + data_len).to_le_bytes());
    data.extend_from_slice(b"WAVE");
    data.extend_from_slice(b"fmt ");
    data.extend_from_slice(&16u32.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&rate.to_le_bytes());
    data.extend_from_slice(&(rate * 2).to_le_bytes());
    data.extend_from_slice(&2u16.to_le_bytes());
    data.extend_from_slice(&16u16.to_le_bytes());
    data.extend_from_slice(b"data");
    data.extend_from_slice(&data_len.to_le_bytes());
    for i in 0..frames {
        let sample =
            (peak * 32767.0 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / rate as f64).sin())
                as i16;
        data.extend_from_slice(&sample.to_le_bytes());
    }
    std::fs::write(path, data).unwrap();
    path.to_path_buf()
}

#[test]
fn narration_items_merge_with_origins_and_refutation() {
    let _guard = env_lock();
    let root = temp_root("origins");
    let photos = vec![photo(&root, "room.png", [120, 60, 20, 255])];
    let chain = Chain::new(&root, plan("air purifier"), None, &["filter", "suitcase"]);
    let mut opts = chain.base_opts(&root);
    opts.narrated_audios = vec![wav_file(&root.join("walk.wav"), 1.0, 8000, 0.5)];
    opts.narration_fn = Some(Box::new(|_path: &Path| {
        Ok(json!({
            "reply": "TRANSCRIPT\nsell this filter; a suitcase and a unicycle are here too\nSELLING\nfilter\nMENTIONED\nsuitcase\nunicycle\n",
            "usage": {"prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150},
            "latency_ms": 10,
            "model": "fake",
        }))
    }) as NarrationFn);
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    let items = items_by_name(&manifest);
    assert_eq!(items["filter"]["status"], "staged");
    assert_eq!(items["filter"]["origin"], "narration_selling");
    assert_eq!(items["suitcase"]["status"], "staged"); // mentioned, confirmed
    assert_eq!(items["suitcase"]["origin"], "narration_mentioned");
    let unicycle = &items["unicycle"];
    assert_eq!(unicycle["status"], "failed"); // mentioned, refuted
    assert_eq!(unicycle["origin"], "narration_mentioned");
    assert_eq!(unicycle["failure"]["stage"], "cut_out");
    assert_eq!(unicycle["failure"]["code"], "NO_MATCH");
}

// ----------------------------------------------------------------------
// S3: backdrop stage modes
// ----------------------------------------------------------------------

#[test]
fn backdrop_renderers_are_deterministic_with_no_shadow() {
    let wallpaper = backdrop::render_wallpaper(240, 180);
    let again = backdrop::render_wallpaper(240, 180);
    assert_eq!(wallpaper.pixel_sha256(), again.pixel_sha256());
    assert_eq!(
        wallpaper.pixel_sha256(),
        "6d6b2e2362c2ef867426d4365cea1455494180a53e28843f0bdbdcb820be7d0b"
    );
    // Every pixel is white or a cyan-over-white blend: red is the
    // most-attenuated channel, blue the least, and nothing is dark
    // (a contact shadow would introduce dark warm pixels).
    for px in wallpaper.pixels.as_chunks::<4>().0 {
        assert!(px[0] <= px[1] && px[1] <= px[2], "non-motif pixel {px:?}");
        assert!(px[0] >= 150, "dark (shadow-like) pixel {px:?}");
        assert_eq!(px[3], 255);
    }
    // The motif is actually present (full-strength motif pixels sit
    // near (153, 223, 243)).
    assert!(wallpaper
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .any(|px| px[0] < 200));

    let gray = backdrop::render_studio_gray(240, 180);
    let gray_again = backdrop::render_studio_gray(240, 180);
    assert_eq!(gray.pixel_sha256(), gray_again.pixel_sha256());
    assert_eq!(
        gray.pixel_sha256(),
        "a24acda695152a719338ad836cc728c4284a2423c4ee87d321a18ff14b4942a6"
    );
    // At a frame size where the gradient centre lands on a pixel,
    // that pixel is exactly the centre colour.
    let centred = backdrop::render_studio_gray(200, 200);
    assert_eq!(centred.pixel(100, 84), [244, 245, 247, 255]);
    let corner = gray.pixel(0, 0);
    assert!(corner[0] < 244 && corner[0] > 198, "corner {corner:?}");
}

#[test]
fn wallpaper_stage_mode_draws_with_a_code_rendered_plate() {
    let _guard = env_lock();
    let root = temp_root("wallpaper");
    let photos = vec![photo(&root, "shelf.png", [120, 60, 20, 255])];
    let chain = Chain::new(&root, plan("straw hat"), None, &["straw hat"]);
    let mut opts = chain.base_opts(&root);
    opts.item_names = vec!["straw hat".to_string()];
    opts.stage_mode = "wallpaper".to_string();
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    assert_eq!(manifest["stage_mode"], "wallpaper");
    let item = &manifest["items"][0];
    assert_eq!(item["status"], "staged"); // the production verifier reproved it
    assert_eq!(item["stage_mode"], "wallpaper");
    assert_eq!(*chain.draw_calls.borrow(), 0); // no model draw call
    assert!(approx(item["cost_usd"].as_f64().unwrap(), CUT_COST));
    let receipt = chain
        .store
        .load_receipt(item["output_asset"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(receipt["plate_kind"], "code_rendered");
    assert_eq!(
        receipt["backdrop"],
        json!({"kind": "wallpaper", "motif_diameter_px": 72, "opacity": 0.4, "shadow": false})
    );
    assert!(receipt["plate_sha256"].is_string());
    assert!(receipt["plate_asset"].is_string());
}

#[test]
fn studio_gray_stage_mode_receipt_fields() {
    let _guard = env_lock();
    let root = temp_root("studiogray");
    let photos = vec![photo(&root, "shelf.png", [120, 60, 20, 255])];
    let chain = Chain::new(&root, plan("straw hat"), None, &["straw hat"]);
    let mut opts = chain.base_opts(&root);
    opts.item_names = vec!["straw hat".to_string()];
    opts.stage_mode = "studio_gray".to_string();
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    let item = &manifest["items"][0];
    assert_eq!(item["status"], "staged");
    let receipt = chain
        .store
        .load_receipt(item["output_asset"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(receipt["plate_kind"], "code_rendered");
    assert_eq!(receipt["backdrop"], json!({"kind": "studio_gray"}));
}

#[test]
fn scene_mode_receipt_marks_the_placeholder_plate() {
    let _guard = env_lock();
    let root = temp_root("scenemode");
    let photos = vec![photo(&root, "shelf.png", [120, 60, 20, 255])];
    let chain = Chain::new(&root, plan("straw hat"), None, &["straw hat"]);
    let mut opts = chain.base_opts(&root);
    opts.item_names = vec!["straw hat".to_string()];
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    let item = &manifest["items"][0];
    assert_eq!(item["status"], "staged");
    let receipt = chain
        .store
        .load_receipt(item["output_asset"].as_str().unwrap())
        .unwrap()
        .unwrap();
    // The fake chain's draw passes no plate, which is the local
    // placeholder path in the production compositor.
    assert_eq!(receipt["plate_kind"], "local_placeholder");
    assert!(receipt["backdrop"].is_null());
}

// ----------------------------------------------------------------------
// S4: market pricing
// ----------------------------------------------------------------------

fn comp(title: &str, price: f64, is_new: bool) -> Value {
    json!({
        "title": title,
        "price_usd": price,
        "condition": "used",
        "distance_mi": 5.0,
        "age_days": 3,
        "is_new": is_new,
    })
}

fn winix_doc() -> Value {
    let prices = [40.0, 50.0, 50.0, 50.0, 60.0, 60.0, 65.0, 75.0, 85.0, 100.0];
    json!({
        "radius_mi": 40,
        "area": "Camas, WA",
        "pulled_at": "2026-10-28T12:00:00+00:00",
        "items": {
            "WINIX filter": {
                "comps": prices.iter().map(|p| comp("WINIX air purifier", *p, false)).collect::<Vec<_>>(),
            }
        }
    })
}

#[test]
fn pricing_winix_vector_matches_the_spec_formula() {
    let doc = winix_doc();
    // Name matching is exact after trim + case-folding.
    let pricing = pricing_for_item(Some(&doc), "  winix FILTER ");
    assert_eq!(pricing["status"], "priced");
    assert_eq!(pricing["comps_used"], 10);
    // Sorted [40,50,50,50,60,60,65,75,85,100]: p25 = 50, p50 = 60,
    // p75 = 72.5 -> rounds (half away from zero) to $75.
    assert_eq!(
        pricing["range"],
        json!({"quick": 50, "market": 60, "premium": 75})
    );
    assert_eq!(pricing["suggested"], 60);
    assert_eq!(pricing["confidence"], "moderate");
    assert_eq!(pricing["source"], "facebook_marketplace_comps");
    assert_eq!(pricing["pulled_at"], "2026-10-28T12:00:00+00:00");
    assert_eq!(pricing["radius_mi"], 40);
}

#[test]
fn pricing_percentiles_and_rounding() {
    assert!(approx(
        percentile_type7(&[10.0, 20.0, 30.0, 40.0], 25.0),
        17.5
    ));
    assert!(approx(
        percentile_type7(&[10.0, 20.0, 30.0, 40.0], 50.0),
        25.0
    ));
    assert!(approx(round_to_nearest_5(72.5), 75.0)); // half away from zero
    assert!(approx(round_to_nearest_5(77.5), 80.0));
    assert!(approx(round_to_nearest_5(72.4), 70.0));
    assert!(approx(round_to_nearest_5(1.0), 5.0)); // the $5 floor
    let comps = vec![
        comp("a", 10.0, false),
        comp("b", 999.0, true),
        comp("c", 20.0, false),
    ];
    assert_eq!(usable_prices(&comps), vec![10.0, 20.0]); // new stock excluded
}

#[test]
fn pricing_thin_market_and_no_comps() {
    let mut doc = winix_doc();
    doc["items"]["rare thing"] = json!({"comps": [comp("rare", 30.0, false)]});
    let thin = pricing_for_item(Some(&doc), "rare thing");
    assert_eq!(thin["status"], "unpriced_thin_market");
    assert_eq!(thin["comps_used"], 1);
    assert!(thin["range"].is_null() && thin["suggested"].is_null() && thin["confidence"].is_null());

    let missing = pricing_for_item(Some(&doc), "not in the file");
    assert_eq!(missing["status"], "no_comps_supplied");
    assert_eq!(missing["comps_used"], 0);

    // An entry present with an EMPTY comps list is also
    // no_comps_supplied, not a thin market.
    let mut empty_doc = winix_doc();
    empty_doc["items"]["empty thing"] = json!({"comps": []});
    let empty = pricing_for_item(Some(&empty_doc), "empty thing");
    assert_eq!(empty["status"], "no_comps_supplied");
    assert_eq!(empty["comps_used"], 0);

    let none = pricing_for_item(None, "anything");
    assert_eq!(none["status"], "no_comps_supplied");
    assert!(none["pulled_at"].is_null() && none["radius_mi"].is_null());

    // Confidence bands.
    let band = |n: usize| {
        let comps: Vec<Value> = (0..n).map(|i| comp("x", 10.0 + i as f64, false)).collect();
        let doc = json!({"radius_mi": 40, "area": "Camas, WA", "pulled_at": "t", "items": {"x": {"comps": comps}}});
        pricing_for_item(Some(&doc), "x")["confidence"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(band(8), "moderate");
    assert_eq!(band(4), "low-moderate");
    assert_eq!(band(2), "low");
}

#[test]
fn priced_run_writes_the_table_artifacts() {
    let _guard = env_lock();
    let root = temp_root("priced");
    let photos = vec![photo(&root, "purifier.png", [120, 60, 20, 255])];
    let chain = Chain::new(&root, plan("WINIX filter"), None, &["winix filter"]);
    let mut opts = chain.base_opts(&root);
    opts.item_names = vec!["WINIX filter".to_string(), "ghost item".to_string()];
    opts.comps = Some(winix_doc());
    let out = root.join("out");
    let manifest = run_room(&photos, &[], &out, opts).unwrap();
    let items = items_by_name(&manifest);
    let winix = &items["WINIX filter"];
    assert_eq!(winix["status"], "staged");
    assert_eq!(winix["pricing"]["status"], "priced");
    assert_eq!(
        winix["pricing"]["range"],
        json!({"quick": 50, "market": 60, "premium": 75})
    );
    assert_eq!(winix["pricing"]["suggested"], 60);
    assert_eq!(items["ghost item"]["status"], "failed");
    assert_eq!(
        items["ghost item"]["pricing"]["status"],
        "no_comps_supplied"
    );

    assert_eq!(
        manifest["pricing_source"],
        json!({
            "source": "facebook_marketplace_comps",
            "area": "Camas, WA",
            "radius_mi": 40,
            "pulled_at": "2026-10-28T12:00:00+00:00",
        })
    );
    // The staged item's receipt summary carries the plate fields.
    assert_eq!(winix["receipt"]["plate_kind"], "local_placeholder");
    assert!(winix["receipt"]["backdrop"].is_null());

    let png = std::fs::read(out.join("priced-table.png")).unwrap();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    let table = agenthand::image::decode_image(&png).unwrap().into_rgba();
    assert_eq!(table.width, 1240);
    assert!(table.height > 400);
    let html = std::fs::read_to_string(out.join("priced-table.html")).unwrap();
    assert!(html.contains(PRICED_TABLE_FOOTNOTE));
    assert!(html.contains("agenthand \u{2014} the room, priced"));
    assert!(html.contains("<strong>Listing: WINIX filter</strong>"));
    assert!(html.contains("$50 \u{2013} $75"));
    assert!(html.contains("ghost item"));
    assert!(html.contains("status: failed (cut_out / NO_MATCH)"));
    let notes = std::fs::read_to_string(out.join("pricing-notes.md")).unwrap();
    assert!(notes.contains("## Method"));
    assert!(notes.contains("ASKING prices, not sold prices"));
    assert!(notes.contains("### WINIX filter"));
    assert!(notes.contains("WINIX air purifier \u{2014} ask $40"));
    assert!(notes.contains(
        "- Usable prices (sorted): [40.0, 50.0, 50.0, 50.0, 60.0, 60.0, 65.0, 75.0, 85.0, 100.0]"
    ));
    assert!(
        notes.contains("- Computed range: $50 \u{2013} $75; suggested $60; confidence moderate")
    );

    let summary = std::fs::read_to_string(out.join("summary.txt")).unwrap();
    assert!(summary.contains("listing title: Listing: WINIX filter"));
    assert!(summary.contains("pricing: $50\u{2013}$75 range, suggested $60 (moderate, 10 comps)"));
}

#[test]
fn no_comps_run_manifest_shape() {
    let _guard = env_lock();
    let root = temp_root("nocomps");
    let photos = vec![photo(&root, "shelf.png", [120, 60, 20, 255])];
    let chain = Chain::new(&root, plan("straw hat"), None, &["straw hat"]);
    let mut opts = chain.base_opts(&root);
    opts.item_names = vec!["straw hat".to_string()];
    let out = root.join("out");
    let manifest = run_room(&photos, &[], &out, opts).unwrap();
    assert!(manifest["pricing_source"].is_null());
    assert_eq!(manifest["stage_mode"], "scene");
    assert_eq!(
        manifest["items"][0]["pricing"],
        json!({
            "status": "no_comps_supplied",
            "comps_used": 0,
            "range": null,
            "suggested": null,
            "confidence": null,
            "source": "facebook_marketplace_comps",
            "pulled_at": null,
            "radius_mi": null,
        })
    );
    assert!(!out.join("priced-table.png").exists());
}

#[test]
fn mentioned_refutation_forces_no_match_but_named_keeps_probe_code() {
    let _guard = env_lock();
    let root = temp_root("forced");
    let photos = vec![photo(&root, "room.png", [120, 60, 20, 255])];
    let chain = Chain::new(&root, plan("air purifier"), None, &[]);
    *chain.cut_error_code.borrow_mut() = "SEGMENTATION_EMPTY".to_string();
    let mut opts = chain.base_opts(&root);
    opts.item_names = vec!["ghost item".to_string()];
    opts.narrated_audios = vec![wav_file(&root.join("walk.wav"), 1.0, 8000, 0.5)];
    opts.narration_fn = Some(Box::new(|_path: &Path| {
        Ok(json!({
            "reply": "TRANSCRIPT\nthere is a unicycle somewhere\nSELLING\nNONE\nMENTIONED\nunicycle\n",
            "usage": {"prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150},
            "latency_ms": 10,
            "model": "fake",
        }))
    }) as NarrationFn);
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    let items = items_by_name(&manifest);
    // The mentioned item's failure code is normalized to NO_MATCH,
    // while its sightings keep the underlying probe code.
    assert_eq!(items["unicycle"]["failure"]["code"], "NO_MATCH");
    assert_eq!(items["unicycle"]["failure"]["stage"], "cut_out");
    assert_eq!(items["unicycle"]["origin"], "narration_mentioned");
    assert_eq!(
        items["unicycle"]["sightings"][0]["error"]["code"],
        "SEGMENTATION_EMPTY"
    );
    // The named item keeps the first probe error code as before.
    assert_eq!(items["ghost item"]["failure"]["code"], "SEGMENTATION_EMPTY");
    assert_eq!(items["ghost item"]["origin"], "named");
}

// ----------------------------------------------------------------------
// S5: category comps pricing on the staged record's listing
// ----------------------------------------------------------------------

fn category_comps_doc() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pricing-comps.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn staged_record_listing_carries_range_and_recommended() {
    let _guard = env_lock();
    let root = temp_root("compsrecord");
    let photos = vec![photo(&root, "bike.png", [40, 80, 200, 255])];
    // Lowercase: the run normalises concepts before cut_out, and
    // the fake chain matches its known set exactly.
    let chain = Chain::new(&root, plan("kids bmx bike"), None, &["kids bmx bike"]);
    let mut opts = chain.base_opts(&root);
    opts.item_names = vec!["kids bmx bike".to_string()];
    opts.comps = Some(category_comps_doc());
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    let item = &manifest["items"][0];
    assert_eq!(item["status"], "staged", "{item}");
    // The identify-fed listing copy is unchanged ...
    assert_eq!(item["listing"]["title"], "Listing: kids bmx bike");
    assert_eq!(item["listing"]["source"], "photo_plan");
    // ... and now carries the comps pricing block.
    let pricing = &item["listing"]["pricing"];
    assert_eq!(pricing["status"], "priced", "{pricing}");
    assert_eq!(pricing["scope"], "local");
    assert!(pricing.get("note").is_none());
    assert_eq!(pricing["category"], "kids_bmx_bike");
    assert_eq!(pricing["range"], json!([30, 125]));
    assert_eq!(pricing["recommended"], json!(50));
    assert_eq!(pricing["percentile_band"], json!([44, 80]));
    assert_eq!(pricing["confidence"], "medium");
    assert_eq!(pricing["advisory"], true);
    // The per-run market pricing field keeps its own shape: a
    // category document has no "items" table, so it reports
    // no_comps_supplied exactly as before.
    assert_eq!(item["pricing"]["status"], "no_comps_supplied");
}

#[test]
fn unmatched_listing_gets_no_comps() {
    let _guard = env_lock();
    let root = temp_root("compsnomatch");
    let photos = vec![photo(&root, "shelf.png", [120, 60, 20, 255])];
    let chain = Chain::new(&root, plan("straw hat"), None, &["straw hat"]);
    let mut opts = chain.base_opts(&root);
    opts.item_names = vec!["straw hat".to_string()];
    opts.comps = Some(category_comps_doc());
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    let item = &manifest["items"][0];
    assert_eq!(item["status"], "staged");
    assert_eq!(item["listing"]["pricing"], json!({"status": "no_comps"}));
}

#[test]
fn staged_record_without_local_comps_prices_from_the_wide_tier() {
    let _guard = env_lock();
    let root = temp_root("compswide");
    let photos = vec![photo(&root, "bike.png", [40, 80, 200, 255])];
    let chain = Chain::new(&root, plan("kids bmx bike"), None, &["kids bmx bike"]);
    let mut opts = chain.base_opts(&root);
    opts.item_names = vec!["kids bmx bike".to_string()];
    // The real file with the bike's local comps emptied and a nested
    // wider-area tier in the documented shape.
    let mut doc = category_comps_doc();
    let cats = doc["categories"].as_array_mut().unwrap();
    let bike = cats
        .iter_mut()
        .find(|c| c["key"] == "kids_bmx_bike")
        .unwrap();
    bike["comps"] = json!([]);
    bike["stats"] = json!({"count": 0});
    for k in [
        "recommended",
        "suggested",
        "range",
        "percentile_band",
        "confidence",
    ] {
        bike[k] = Value::Null;
    }
    bike["wide"] = json!({
        "comps": [
            {"title": "Kids BMX bike 20 inch", "price": 60},
            {"title": "Haro kids bike", "price": 90},
            {"title": "GT BMX bike", "price": 150}
        ],
        "stats": {"count": 3, "min": 60, "p25": 75, "median": 90, "p75": 120, "max": 150},
        "recommended": 90,
        "range": [60, 150],
        "percentile_band": [75, 120],
        "confidence": "low",
        "area": "Portland metro, 100 mi radius",
        "searched_at": "2026-11-01T18:00-08:00"
    });
    opts.comps = Some(doc);
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    let item = &manifest["items"][0];
    assert_eq!(item["status"], "staged", "{item}");
    let pricing = &item["listing"]["pricing"];
    assert_eq!(pricing["status"], "priced", "{pricing}");
    assert_eq!(pricing["scope"], "wide");
    assert_eq!(
        pricing["note"],
        "No local items found; priced from wider-area listings."
    );
    assert_eq!(pricing["category"], "kids_bmx_bike");
    assert_eq!(pricing["recommended"], json!(90));
    assert_eq!(pricing["range"], json!([60, 150]));
    assert_eq!(pricing["area"], "Portland metro, 100 mi radius");
}
