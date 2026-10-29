//! ROOM RUN: manifest, de-dup, and failure paths — fully offline.
//!
//! Port of tests/test_roomrun.py. The chain (identify / cut_out /
//! draw) is injected as a fake, so no test here can reach the network
//! or the real ledger: the spend probe is the fake's own counter. The
//! fake's cut_out/draw create REAL assets in the tmp store
//! (foregrounds, composites, persisted receipts), so the manifest's
//! opaque-pixel counts and receipt links are exercised against the
//! same store code production uses.
//!
//! run_room mutates the process-wide AGENTHAND_LEDGER_PATH (set for
//! the run, restored after — the very behavior under test), so every
//! test that runs a room takes the ENV_LOCK.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use agenthand::assets::{AssetStore, KIND_COMPOSITE, KIND_FOREGROUND};
use agenthand::image::{encode_png_rgba, RgbaImage};
use agenthand::roomrun::{
    apply_candidate_coverage, bbox_iou, concept_variants, group_candidates, narration_cost_usd,
    parse_narration_reply, probe_wav, project_narration_cost_usd, py_repr_str, rank_photos,
    run_room, sample_video_frames, token_similarity, Candidate, NarrationFn, PhotoSource,
    RoomChain, RoomRunOptions, VerifyFn, NARRATION_MODEL,
};
use agenthand::{composite, config, ingest, ledger};
use serde_json::{json, Value};

static ENV_LOCK: Mutex<()> = Mutex::new(());
static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn temp_root(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "agenthand-roomrun-{tag}-{}-{n}",
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
const DRAW_COST: f64 = 0.01;

/// The three tools, faked at the roomrun adapter boundary.
///
/// plans: identify results in source order (a dict with an "error"
/// key makes that photo's plan fail). contents maps
/// (photo_index, concept) -> object_id: a cut_out succeeds exactly
/// for those pairs, and the foreground it returns is derived from the
/// object_id alone — two names resolving to one object_id get
/// pixel-identical foregrounds, like two names for one real object.
struct FakeChain {
    store: AssetStore,
    plans: Vec<Value>,
    contents: HashMap<(usize, String), String>,
    draw_fail_objects: HashSet<String>,
    crop_plans: RefCell<VecDeque<Value>>,
    opaque_by_object: HashMap<String, i64>,
    photo_handles: RefCell<HashMap<String, usize>>,
    fg_object: RefCell<HashMap<String, String>>,
    identify_calls: RefCell<usize>,
    cut_out_calls: RefCell<usize>,
    draw_calls: RefCell<usize>,
    spend: RefCell<f64>,
}

impl FakeChain {
    fn new(
        data_dir: &Path,
        plans: Vec<Value>,
        contents: HashMap<(usize, String), String>,
    ) -> Rc<Self> {
        Rc::new(FakeChain {
            store: AssetStore::new(data_dir),
            plans,
            contents,
            draw_fail_objects: HashSet::new(),
            crop_plans: RefCell::new(VecDeque::new()),
            opaque_by_object: HashMap::new(),
            photo_handles: RefCell::new(HashMap::new()),
            fg_object: RefCell::new(HashMap::new()),
            identify_calls: RefCell::new(0),
            cut_out_calls: RefCell::new(0),
            draw_calls: RefCell::new(0),
            spend: RefCell::new(0.0),
        })
    }

    fn identify(&self, image_ref: &str) -> Result<Value, String> {
        let idx = {
            let mut calls = self.identify_calls.borrow_mut();
            let idx = *calls;
            *calls += 1;
            idx
        };
        *self.spend.borrow_mut() += IDENTIFY_COST;
        // Calls beyond the scripted plans are per-item crop
        // identifies: they answer from the crop-plan queue when one
        // is scripted, and fail as a plan error otherwise, so the
        // run falls back to its no-copy path.
        let Some(plan) = self.plans.get(idx).cloned() else {
            if let Some(crop_plan) = self.crop_plans.borrow_mut().pop_front() {
                let mut result = json!({
                    "status": "ok",
                    "image": image_ref,
                    "cost_usd": IDENTIFY_COST,
                });
                for (key, value) in crop_plan.as_object().unwrap() {
                    result[key] = value.clone();
                }
                return Ok(result);
            }
            return Ok(json!({
                "status": "error",
                "error": {"code": "NO_PLAN", "message": "no scripted plan for this call", "cost_counted_usd": IDENTIFY_COST},
            }));
        };
        if plan.get("error").map(|e| !e.is_null()).unwrap_or(false) {
            return Ok(json!({"status": "error", "error": plan["error"]}));
        }
        let handle = ingest::ingest(image_ref, &self.store)?;
        self.photo_handles.borrow_mut().insert(handle.clone(), idx);
        let mut result = json!({
            "status": "ok",
            "image": handle,
            "cost_usd": IDENTIFY_COST,
        });
        for (key, value) in plan.as_object().unwrap() {
            result[key] = value.clone();
        }
        Ok(result)
    }

    fn foreground_for(&self, object_id: &str) -> RgbaImage {
        let opaque = self.opaque_by_object.get(object_id).copied().unwrap_or(100);
        let side = ((opaque as f64).sqrt() as u32).max(1);
        // Deterministic per-object color (Python uses hash(); only
        // identity across names matters, and both go through here).
        let mut hash: u64 = 0xcbf29ce484222325;
        for byte in object_id.bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        RgbaImage::solid(
            side,
            side,
            [
                (hash & 0xFF) as u8,
                ((hash >> 8) & 0xFF) as u8,
                ((hash >> 16) & 0xFF) as u8,
                255,
            ],
        )
    }

    fn cut_out(&self, handle: &str, concept: &str) -> Result<Value, String> {
        *self.cut_out_calls.borrow_mut() += 1;
        *self.spend.borrow_mut() += CUT_COST;
        let idx = self
            .photo_handles
            .borrow()
            .get(handle)
            .copied()
            .expect("cut_out called with an unknown handle");
        let object_id = self.contents.get(&(idx, concept.to_string())).cloned();
        let Some(object_id) = object_id else {
            return Ok(json!({
                "status": "error",
                "error": {
                    "code": "NO_MATCH",
                    "message": format!("no {} in photo {idx}", py_repr_str(concept)),
                    "cost_counted_usd": CUT_COST,
                },
            }));
        };
        let fg_handle =
            self.store
                .put_image(&self.foreground_for(&object_id), KIND_FOREGROUND, None)?;
        self.fg_object
            .borrow_mut()
            .insert(fg_handle.clone(), object_id);
        Ok(json!({
            "status": "ok",
            "output_asset": fg_handle,
            "mask_asset": Value::Null,
            "mask_overlay_asset": Value::Null,
            "mask_review": {"bbox": [4, 4, 30, 30], "warnings": []},
            "cost_usd": CUT_COST,
        }))
    }

    fn draw(&self, fg_handle: &str, _scene: &str) -> Result<Value, String> {
        *self.draw_calls.borrow_mut() += 1;
        *self.spend.borrow_mut() += DRAW_COST;
        let object_id = self
            .fg_object
            .borrow()
            .get(fg_handle)
            .cloned()
            .expect("draw called with an unknown foreground");
        if self.draw_fail_objects.contains(&object_id) {
            return Ok(json!({
                "status": "error",
                "error": {
                    "code": "IMAGE_REFUSED",
                    "message": "the image model refused this plate",
                    "cost_counted_usd": DRAW_COST,
                },
            }));
        }
        let fg_record = self.store.get_record(fg_handle)?;
        let composite = self.store.put_image(
            &RgbaImage::solid(8, 8, [10, 10, 10, 255]),
            KIND_COMPOSITE,
            Some(&fg_record.asset_id),
        )?;
        let receipt = json!({
            "source_asset": "asset://source",
            "foreground_pixel_sha256": fg_record.pixel_sha256,
            "opaque_pixels": self.opaque_by_object.get(&object_id).copied().unwrap_or(100),
            "changed_opaque_pixels": 0,
            "color_profile_preserved": true,
        });
        self.store.save_receipt(&composite, &receipt)?;
        Ok(json!({
            "status": "ok",
            "output_asset": composite,
            "fidelity_receipt": receipt,
            "cost_usd": DRAW_COST,
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

    fn spend_probe(self: &Rc<Self>) -> agenthand::roomrun::SpendProbe {
        let fake = self.clone();
        Box::new(move || Ok(*fake.spend.borrow()))
    }
}

/// A passing verifier report, shaped like the real one's.
fn verify_ok() -> VerifyFn {
    Box::new(|_store: &AssetStore, _handle: &str| {
        Ok(json!({
            "ok": true,
            "checks": {"zero_changed_pixels": true},
            "problems": [],
            "recomputed": {
                "changed_opaque_pixels": 0,
                "opaque_pixels": 100,
                "color_profile_preserved": true,
            },
        }))
    })
}

fn base_opts(fake: &Rc<FakeChain>, data_dir: &Path) -> RoomRunOptions {
    RoomRunOptions {
        budget_usd: 5.00,
        store: Some(AssetStore::new(data_dir)),
        chain: Some(fake.chain()),
        spend_probe: Some(fake.spend_probe()),
        verify_fn: Some(verify_ok()),
        ..RoomRunOptions::default()
    }
}

fn contents(pairs: &[((usize, &str), &str)]) -> HashMap<(usize, String), String> {
    pairs
        .iter()
        .map(|((idx, concept), object)| ((*idx, concept.to_string()), object.to_string()))
        .collect()
}

fn items_by_name(manifest: &Value) -> HashMap<String, Value> {
    manifest["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| (item["name"].as_str().unwrap().to_string(), item.clone()))
        .collect()
}

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

#[test]
fn concept_variants_order_and_stripping() {
    assert_eq!(
        concept_variants("gray storage box", None),
        vec!["gray storage box", "storage box", "box"]
    );
    assert_eq!(
        concept_variants("straw hat", None),
        vec!["straw hat", "hat"]
    );
    assert_eq!(
        concept_variants("orange suitcase", None),
        vec!["orange suitcase", "suitcase"]
    );
    let variants = concept_variants("straw hat", Some(&plan("straw sun hat")));
    assert_eq!(variants[0], "straw hat");
    assert!(variants.contains(&"straw sun hat".to_string())); // the plan's own phrasing rides along
}

#[test]
fn token_similarity_and_bbox_iou() {
    assert_eq!(
        token_similarity(Some("orange suitcase"), Some("orange suitcase")),
        1.0
    );
    assert!(token_similarity(Some("orange suitcase"), Some("orange spinner suitcase")) >= 0.6);
    let grey = token_similarity(Some("red leather wallet"), Some("brown leather wallet"));
    assert!((0.4..0.6).contains(&grey));
    assert!(token_similarity(Some("book"), Some("bookcase")) > 0.0); // prefix rule
    assert_eq!(
        bbox_iou(Some(&json!([0, 0, 10, 10])), Some(&json!([0, 0, 10, 10]))),
        1.0
    );
    assert_eq!(
        bbox_iou(Some(&json!([0, 0, 10, 10])), Some(&json!([20, 20, 30, 30]))),
        0.0
    );
    assert_eq!(bbox_iou(None, Some(&json!([0, 0, 1, 1]))), 0.0);
}

#[test]
fn group_candidates_merge_and_grey_zone() {
    let cand = |name: &str, idx: usize| Candidate {
        name: name.to_string(),
        photo_index: idx,
        plan: json!({}),
    };
    let groups = group_candidates(&[
        cand("orange suitcase", 0),
        cand("orange spinner suitcase", 1),
    ]);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].photo_indices, vec![0, 1]);

    let groups = group_candidates(&[
        cand("red leather wallet", 0),
        cand("brown leather wallet", 1),
    ]);
    assert_eq!(groups.len(), 2); // grey zone: kept both...
    assert!(groups[0]
        .flags
        .iter()
        .any(|f| f.contains("possible duplicate"))); // ...and flagged
    assert!(groups[1]
        .flags
        .iter()
        .any(|f| f.contains("possible duplicate")));
}

fn source_with_plan(path: &str, origin: &'static str, plan: Value) -> PhotoSource {
    PhotoSource {
        path: PathBuf::from(path),
        origin,
        video_path: None,
        timestamp_s: None,
        handle: None,
        width: None,
        height: None,
        plan: Some(plan),
        plan_error: None,
    }
}

#[test]
fn rank_photos_prefers_mention_then_stills() {
    let mut sources = vec![
        source_with_plan("a.jpg", "still", plan("white bookcase")),
        source_with_plan("b.jpg", "still", plan("orange suitcase")),
    ];
    let ranked = rank_photos("orange suitcase", &sources);
    assert_eq!(ranked[0], 1);
    // No mention anywhere: full coverage order, stills before frames —
    // a silent plan is weak evidence of absence, so the probe cap,
    // not the ranker, decides how far down the order a run goes.
    sources.push(source_with_plan(
        "f.jpg",
        "video_frame",
        plan("white bookcase"),
    ));
    let ranked = rank_photos("unicycle", &sources);
    assert_eq!(ranked, vec![0, 1, 2]);
}

#[test]
fn rank_photos_head_noun_beats_modifier_match() {
    // The first real room run's failure shape: for "black hat", the
    // plans naming "black" (a suitcase's trim, a railing) outranked
    // the plans naming "hat" (the bookcase views that held it), and
    // the probe cap was spent before any hat-bearing photo was probed.
    let plan_with = |text: &str| -> Value {
        json!({
            "description": text,
            "concept": "",
            "suggested_title": "",
            "listing_bullets": [],
            "condition_notes": "",
        })
    };
    let sources = vec![
        source_with_plan(
            "case.jpg",
            "still",
            plan_with("orange suitcase with black trim and spinner wheels"),
        ),
        source_with_plan(
            "floor.jpg",
            "video_frame",
            plan_with("wood plank flooring with a black railing"),
        ),
        source_with_plan(
            "shelf.jpg",
            "video_frame",
            plan_with("white bookshelf holding boxes, a straw hat and hats"),
        ),
    ];
    let ranked = rank_photos("black hat", &sources);
    assert_eq!(ranked[0], 2); // the hat-bearing photo probes first
}

// ---------------------------------------------------------------------------
// Full runs against the fake chain
// ---------------------------------------------------------------------------

#[test]
fn named_run_stages_and_manifests() {
    let _guard = env_lock();
    let root = temp_root("named");
    let photos = vec![
        photo(&root, "shelf.png", [120, 60, 20, 255]),
        photo(&root, "case.png", [200, 90, 10, 255]),
    ];
    let fake = FakeChain::new(
        &root,
        vec![plan("straw hat"), plan("orange suitcase")],
        contents(&[
            ((0, "straw hat"), "hat1"),
            ((1, "orange suitcase"), "case1"),
        ]),
    );
    let mut opts = base_opts(&fake, &root);
    opts.item_names = vec!["straw hat".to_string(), "orange suitcase".to_string()];
    let out = root.join("out");
    let manifest = run_room(&photos, &[], &out, opts).unwrap();
    let items = items_by_name(&manifest);
    assert_eq!(items["straw hat"]["status"], "staged");
    assert_eq!(items["straw hat"]["primary_photo_index"], 0);
    assert_eq!(items["straw hat"]["receipt"]["changed_opaque_pixels"], 0);
    let receipt_file = items["straw hat"]["receipt_file"].as_str().unwrap();
    assert!(Path::new(receipt_file).exists());
    assert_eq!(items["straw hat"]["listing"]["source"], "photo_plan");
    assert_eq!(items["orange suitcase"]["status"], "staged");
    assert!(approx(
        manifest["total_cost_usd"].as_f64().unwrap(),
        *fake.spend.borrow()
    ));
    assert!(out.join("manifest.json").exists());
    let summary = std::fs::read_to_string(out.join("summary.txt")).unwrap();
    assert!(summary.contains("STAGED"));
}

#[test]
fn partial_failure_is_first_class() {
    let _guard = env_lock();
    let root = temp_root("partial");
    let photos = vec![photo(&root, "case.png", [200, 90, 10, 255])];
    let fake = FakeChain::new(
        &root,
        vec![plan("orange suitcase")],
        contents(&[((0, "orange suitcase"), "case1")]),
    );
    let mut opts = base_opts(&fake, &root);
    opts.item_names = vec!["orange suitcase".to_string(), "ghost item".to_string()];
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    let items = items_by_name(&manifest);
    assert_eq!(items["orange suitcase"]["status"], "staged");
    let ghost = &items["ghost item"];
    assert_eq!(ghost["status"], "failed");
    assert_eq!(ghost["failure"]["stage"], "cut_out");
    assert_eq!(ghost["failure"]["code"], "NO_MATCH");
    assert!(!ghost["sightings"].as_array().unwrap().is_empty()); // the failed probes are in the record
                                                                 // Failed probes still cost money, and the manifest says so:
                                                                 // 2 concept variants x the per-call counted cost.
    assert!(approx(ghost["cost_usd"].as_f64().unwrap(), 2.0 * CUT_COST));
    assert!(approx(
        ghost["sightings"][0]["cost_usd"].as_f64().unwrap(),
        2.0 * CUT_COST
    ));
    // A failed sighting records the phrasing it was probed under:
    // the first variant (the item's primary phrasing).
    assert_eq!(ghost["sightings"][0]["concept_used"], "ghost item");
}

#[test]
fn listing_copy_not_borrowed_from_another_subject() {
    let _guard = env_lock();
    let root = temp_root("listing");
    let photos = vec![photo(&root, "shelf.png", [120, 60, 20, 255])];
    let fake = FakeChain::new(
        &root,
        vec![plan("white bookcase")], // the photo's primary subject
        contents(&[((0, "straw hat"), "hat1")]), // ...but the hat is in it too
    );
    let mut opts = base_opts(&fake, &root);
    opts.item_names = vec!["straw hat".to_string()];
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    let item = &manifest["items"][0];
    assert_eq!(item["status"], "staged");
    assert_eq!(item["listing"]["source"], "none");
    assert!(item["listing"]["title"].is_null());
}

#[test]
fn auto_mode_dedups_across_photos() {
    let _guard = env_lock();
    let root = temp_root("auto");
    let photos = vec![
        photo(&root, "a.png", [200, 90, 10, 255]),
        photo(&root, "b.png", [210, 95, 12, 255]),
    ];
    let mut fake = FakeChain::new(
        &root,
        vec![plan("orange suitcase"), plan("orange suitcase")],
        contents(&[
            ((0, "orange suitcase"), "case1"),
            ((1, "orange suitcase"), "case1"),
        ]),
    );
    Rc::get_mut(&mut fake)
        .unwrap()
        .opaque_by_object
        .insert("case1".to_string(), 100);
    let mut opts = base_opts(&fake, &root);
    opts.auto = true;
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    assert_eq!(manifest["items"].as_array().unwrap().len(), 1);
    let item = &manifest["items"][0];
    assert_eq!(item["status"], "staged");
    let ok_sightings: Vec<&Value> = item["sightings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["status"] == "ok")
        .collect();
    assert_eq!(ok_sightings.len(), 2);
    assert!(!item["also_seen_in"].as_array().unwrap().is_empty()); // the non-primary sighting is recorded
}

#[test]
fn cross_item_same_object_flagged_not_merged() {
    let _guard = env_lock();
    let root = temp_root("dup");
    let photos = vec![photo(&root, "shelf.png", [120, 60, 20, 255])];
    let fake = FakeChain::new(
        &root,
        vec![plan("gray storage box")],
        contents(&[
            ((0, "gray storage box"), "box1"),
            ((0, "storage box"), "box1"), // same physical object, second name
        ]),
    );
    let mut opts = base_opts(&fake, &root);
    opts.item_names = vec!["gray storage box".to_string(), "storage box".to_string()];
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    let items = items_by_name(&manifest);
    assert_eq!(items.len(), 2); // kept both
    assert!(items["gray storage box"]["possible_duplicate_of"]
        .as_array()
        .unwrap()
        .contains(&json!("storage box")));
    assert!(items["storage box"]["possible_duplicate_of"]
        .as_array()
        .unwrap()
        .contains(&json!("gray storage box")));
}

#[test]
fn draw_failure_keeps_cut_evidence() {
    let _guard = env_lock();
    let root = temp_root("drawfail");
    let photos = vec![photo(&root, "case.png", [200, 90, 10, 255])];
    let mut fake = FakeChain::new(
        &root,
        vec![plan("orange suitcase")],
        contents(&[((0, "orange suitcase"), "case1")]),
    );
    Rc::get_mut(&mut fake)
        .unwrap()
        .draw_fail_objects
        .insert("case1".to_string());
    let mut opts = base_opts(&fake, &root);
    opts.item_names = vec!["orange suitcase".to_string()];
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    let item = &manifest["items"][0];
    assert_eq!(item["status"], "failed");
    assert_eq!(item["failure"]["stage"], "draw");
    assert_eq!(item["failure"]["code"], "IMAGE_REFUSED");
    assert!(item["sightings"][0]["foreground_asset"].is_string()); // the cut survives in the record
    assert!(approx(
        item["cost_usd"].as_f64().unwrap(),
        CUT_COST + DRAW_COST
    ));
}

#[test]
fn budget_stop_marks_remaining_items() {
    let _guard = env_lock();
    let root = temp_root("budget");
    let photos = vec![
        photo(&root, "a.png", [120, 60, 20, 255]),
        photo(&root, "b.png", [200, 90, 10, 255]),
    ];
    let fake = FakeChain::new(
        &root,
        vec![plan("orange suitcase"), plan("straw hat")],
        contents(&[
            ((0, "orange suitcase"), "case1"),
            ((1, "straw hat"), "hat1"),
        ]),
    );
    let greedy = fake.clone();
    let mut opts = base_opts(&fake, &root);
    opts.item_names = vec!["orange suitcase".to_string(), "straw hat".to_string()];
    opts.spend_probe = Some(Box::new(move || Ok(*greedy.spend.borrow() * 1000.0)));
    // spend_before was read through the same probe (0 at start), then
    // the plan pass makes the probe report >> the budget, so every
    // item is stopped before its first probe.
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    assert!(manifest["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|i| i["status"] == "failed"));
    assert!(manifest["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|i| i["failure"]["code"] == "BUDGET_STOP"));
    assert_eq!(*fake.cut_out_calls.borrow(), 0);
}

#[test]
fn projection_gate_refuses_before_any_call() {
    let _guard = env_lock();
    let root = temp_root("projection");
    let photos = vec![photo(&root, "a.png", [120, 60, 20, 255])];
    let fake = FakeChain::new(&root, vec![plan("orange suitcase")], contents(&[]));
    let mut opts = base_opts(&fake, &root);
    opts.item_names = vec!["orange suitcase".to_string()];
    opts.budget_usd = 0.01;
    let err = run_room(&photos, &[], &root.join("out"), opts).unwrap_err();
    assert!(err.contains("worst-case projection"), "got: {err}");
    assert_eq!(*fake.identify_calls.borrow(), 0);
    assert_eq!(*fake.cut_out_calls.borrow(), 0);
    assert_eq!(*fake.draw_calls.borrow(), 0);
}

// ---------------------------------------------------------------------------
// Verify loop: draw's receipt is never believed on its own — the
// independent verifier's verdict is a distinct manifest field, and a
// failed verdict un-stages the item.
// ---------------------------------------------------------------------------

fn verify_fail() -> VerifyFn {
    Box::new(|_store: &AssetStore, _handle: &str| {
        Ok(json!({
            "ok": false,
            "checks": {"zero_changed_pixels": false},
            "problems": ["2 of 100 fully-masked product pixels differ"],
            "recomputed": {
                "changed_opaque_pixels": 2,
                "opaque_pixels": 100,
                "color_profile_preserved": true,
            },
        }))
    })
}

#[test]
fn verify_failure_means_not_staged() {
    let _guard = env_lock();
    let root = temp_root("verifyfail");
    let photos = vec![photo(&root, "case.png", [200, 90, 10, 255])];
    let fake = FakeChain::new(
        &root,
        vec![plan("orange suitcase")],
        contents(&[((0, "orange suitcase"), "case1")]),
    );
    let mut opts = base_opts(&fake, &root);
    opts.item_names = vec!["orange suitcase".to_string()];
    opts.verify_fn = Some(verify_fail());
    let out = root.join("out");
    let manifest = run_room(&photos, &[], &out, opts).unwrap();
    let item = &manifest["items"][0];
    assert_eq!(item["status"], "verify_failed"); // NOT staged
    assert_eq!(item["failure"]["stage"], "verify");
    assert_eq!(item["failure"]["code"], "VERIFY_FAILED");
    // The verifier's own numbers are the record, distinct from the
    // receipt draw issued (which still claims 0 changed, untouched).
    assert_eq!(item["verification"]["ok"], false);
    assert_eq!(item["verification"]["changed_opaque_pixels"], 2);
    assert_eq!(item["receipt"]["changed_opaque_pixels"], 0);
    let summary = std::fs::read_to_string(out.join("summary.txt")).unwrap();
    assert!(summary.contains("VERIFY FAILED"));
    assert!(summary.contains("0 of 1 items staged"));
}

#[test]
fn verification_numbers_come_from_the_verifier() {
    let _guard = env_lock();
    let root = temp_root("verifynums");
    let photos = vec![photo(&root, "case.png", [200, 90, 10, 255])];
    let fake = FakeChain::new(
        &root,
        vec![plan("orange suitcase")],
        contents(&[((0, "orange suitcase"), "case1")]),
    );
    let mut opts = base_opts(&fake, &root);
    opts.item_names = vec!["orange suitcase".to_string()];
    opts.verify_fn = Some(Box::new(|_store: &AssetStore, _handle: &str| {
        Ok(json!({
            "ok": true,
            "checks": {"zero_changed_pixels": true},
            "problems": [],
            "recomputed": {
                "changed_opaque_pixels": 0,
                "opaque_pixels": 4242, // the recount's count, not draw's
                "color_profile_preserved": true,
            },
        }))
    }));
    let out = root.join("out");
    let manifest = run_room(&photos, &[], &out, opts).unwrap();
    let item = &manifest["items"][0];
    assert_eq!(item["status"], "staged");
    assert_eq!(item["verification"]["ok"], true);
    assert_eq!(item["verification"]["opaque_pixels"], 4242);
    assert_eq!(item["receipt"]["opaque_pixels"], 100); // the fake draw's claim
    let summary = std::fs::read_to_string(out.join("summary.txt")).unwrap();
    assert!(summary.contains("4242 opaque px recounted from disk, 0 changed"));
}

#[test]
fn verifier_that_cannot_run_fails_the_item_not_the_room() {
    let _guard = env_lock();
    let root = temp_root("verifybroken");
    let photos = vec![photo(&root, "case.png", [200, 90, 10, 255])];
    let fake = FakeChain::new(
        &root,
        vec![plan("orange suitcase")],
        contents(&[((0, "orange suitcase"), "case1")]),
    );
    let mut opts = base_opts(&fake, &root);
    opts.item_names = vec!["orange suitcase".to_string()];
    opts.verify_fn = Some(Box::new(|_store: &AssetStore, _handle: &str| {
        Err("no receipt persisted beside the composite".to_string())
    }));
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    let item = &manifest["items"][0];
    assert_eq!(item["status"], "verify_failed");
    assert_eq!(item["verification"]["ok"], false);
    assert!(item["verification"]["problems"][0]
        .as_str()
        .unwrap()
        .contains("could not run"));
}

// identify/cut_out faked, draw REAL (composite::draw_product). The
// foreground is lineage-correct (parented to the ingested source
// photo), so draw runs the production compositor and persists a
// production receipt — and run_room's DEFAULT verifier
// (composite::verify_receipt_from_disk) grades it from disk.
struct RealDrawChain {
    store: AssetStore,
}

impl RealDrawChain {
    fn identify(&self, image_ref: &str) -> Result<Value, String> {
        let handle = ingest::ingest(image_ref, &self.store)?;
        let mut result = json!({"status": "ok", "image": handle, "cost_usd": 0.0});
        for (key, value) in plan("straw hat").as_object().unwrap() {
            result[key] = value.clone();
        }
        Ok(result)
    }

    fn cut_out(&self, handle: &str, concept: &str) -> Result<Value, String> {
        if concept != "straw hat" {
            return Ok(json!({
                "status": "error",
                "error": {"code": "NO_MATCH", "message": "not here", "cost_counted_usd": 0.0},
            }));
        }
        let mut pixels = vec![0u8; 48 * 36 * 4];
        for y in 0..36usize {
            for x in 0..21usize {
                let i = (y * 48 + x) * 4;
                pixels[i..i + 4].copy_from_slice(if x < 20 {
                    &[200, 30, 30, 255]
                } else {
                    &[0, 200, 30, 128]
                });
            }
        }
        let fg_img = RgbaImage::new(48, 36, pixels);
        let fg = self.store.put_image(
            &fg_img,
            KIND_FOREGROUND,
            Some(&AssetStore::parse_handle(handle)?),
        )?;
        Ok(json!({
            "status": "ok",
            "output_asset": fg,
            "mask_asset": Value::Null,
            "mask_overlay_asset": Value::Null,
            "mask_review": {"bbox": [0, 0, 21, 36], "warnings": []},
            "cost_usd": 0.0,
        }))
    }

    fn draw(&self, fg_handle: &str, scene: &str) -> Result<Value, String> {
        let (handle, receipt) = composite::draw_product(&self.store, fg_handle, scene, None)?;
        Ok(json!({
            "status": "ok",
            "output_asset": handle,
            "fidelity_receipt": receipt,
            "cost_usd": 0.0,
        }))
    }
}

#[test]
fn default_verifier_reproves_a_real_draw() {
    let _guard = env_lock();
    let root = temp_root("realverify");
    let photos = vec![photo(&root, "shelf.png", [120, 60, 20, 255])];
    let real = Rc::new(RealDrawChain {
        store: AssetStore::new(&root),
    });
    let a = real.clone();
    let b = real.clone();
    let c = real.clone();
    let opts = RoomRunOptions {
        item_names: vec!["straw hat".to_string()],
        budget_usd: 5.00,
        store: Some(AssetStore::new(&root)),
        chain: Some(RoomChain {
            identify: Box::new(move |image_ref: &str| a.identify(image_ref)),
            cut_out: Box::new(move |handle: &str, concept: &str| b.cut_out(handle, concept)),
            draw: Box::new(move |handle: &str, scene: &str| c.draw(handle, scene)),
        }),
        spend_probe: Some(Box::new(|| Ok(0.0))),
        // no verify_fn: the production default must be what runs
        ..RoomRunOptions::default()
    };
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    let item = &manifest["items"][0];
    assert_eq!(item["status"], "staged");
    let verification = &item["verification"];
    assert_eq!(verification["ok"], true);
    assert_eq!(verification["changed_opaque_pixels"], 0);
    assert_eq!(verification["opaque_pixels"], 20 * 36); // the recount, from disk
    assert_eq!(
        verification["opaque_pixels"],
        item["receipt"]["opaque_pixels"]
    );
    assert_eq!(verification["checks_failed"], json!([]));
}

// ---------------------------------------------------------------------------
// Ledger isolation: a room run's receipts land in its own ledger
// (default <out>/ledger.jsonl); the configured (cumulative) ledger is
// never touched, and the manifest total is the room ledger's.
// ---------------------------------------------------------------------------

#[test]
fn room_run_spends_against_its_own_ledger() {
    let _guard = env_lock();
    let root = temp_root("ledger");
    let submission_ledger = root.join("submission-ledger.jsonl");
    std::env::set_var("AGENTHAND_LEDGER_PATH", &submission_ledger);
    let photos = vec![photo(&root, "case.png", [200, 90, 10, 255])];
    let fake = FakeChain::new(
        &root,
        vec![plan("orange suitcase")],
        contents(&[((0, "orange suitcase"), "case1")]),
    );
    // Wrap each chain fn so it also appends a receipt at the ledger
    // path resolved AT CALL TIME — the shape billed_call has.
    let wrap = |fake: &Rc<FakeChain>, tool: &'static str, cost: f64, which: u8| {
        let fake = fake.clone();
        move |a: &str, b: &str| -> Result<Value, String> {
            let result = match which {
                0 => fake.identify(a),
                1 => fake.cut_out(a, b),
                _ => fake.draw(a, b),
            }?;
            ledger::append_receipt(
                &config::get_ledger_path(),
                &ledger::Receipt::new(tool, Some("fake"), cost, ledger::PHASE_FINAL),
            )
            .map_err(|e| e.to_string())?;
            Ok(result)
        }
    };
    let i1 = wrap(&fake, "identify", IDENTIFY_COST, 0);
    let i2 = wrap(&fake, "cut_out", CUT_COST, 1);
    let i3 = wrap(&fake, "draw", DRAW_COST, 2);
    let out = root.join("out");
    let opts = RoomRunOptions {
        item_names: vec!["orange suitcase".to_string()],
        budget_usd: 5.00,
        store: Some(AssetStore::new(&root)),
        chain: Some(RoomChain {
            identify: Box::new(move |a: &str| i1(a, "")),
            cut_out: Box::new(move |a: &str, b: &str| i2(a, b)),
            draw: Box::new(move |a: &str, b: &str| i3(a, b)),
        }),
        verify_fn: Some(verify_ok()),
        // no spend_probe, no ledger_path: both defaults are the point
        ..RoomRunOptions::default()
    };
    let manifest = run_room(&photos, &[], &out, opts).unwrap();
    let room_ledger = out.join("ledger.jsonl");
    assert_eq!(
        manifest["ledger_path"].as_str().unwrap(),
        room_ledger.to_string_lossy()
    );
    let total = ledger::ledger_total_usd(&room_ledger).unwrap();
    assert!(approx(total, *fake.spend.borrow()));
    assert!(approx(
        manifest["total_cost_usd"].as_f64().unwrap(),
        *fake.spend.borrow()
    ));
    assert!(!submission_ledger.exists()); // the cumulative ledger untouched
                                          // The caller's environment is restored after the run.
    assert_eq!(
        std::env::var("AGENTHAND_LEDGER_PATH").unwrap(),
        submission_ledger.to_string_lossy()
    );
    std::env::remove_var("AGENTHAND_LEDGER_PATH");
}

// ---------------------------------------------------------------------------
// Video frame sampling
// ---------------------------------------------------------------------------

#[test]
fn sample_video_frames_without_ffmpeg() {
    let root = temp_root("frames");
    let (frames, note) = sample_video_frames(
        &root.join("nope.mp4"),
        &root.join("frames"),
        2.0,
        8,
        Some(""),
    );
    assert!(frames.is_empty());
    let note = note.unwrap();
    assert!(note.contains("ffmpeg"), "got: {note}");
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok()
}

#[test]
fn sample_video_frames_extracts() {
    if !ffmpeg_available() {
        return;
    }
    let root = temp_root("framesextract");
    let video = root.join("walk.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=3:size=64x64:rate=5",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&video)
        .arg("-y")
        .status()
        .unwrap();
    assert!(status.success());
    let (frames, note) = sample_video_frames(&video, &root.join("frames"), 1.0, 8, None);
    assert!(note.is_none(), "got: {note:?}");
    let stamps: Vec<f64> = frames.iter().map(|(_, ts)| *ts).collect();
    assert_eq!(stamps, vec![0.0, 1.0, 2.0]);
    assert!(frames.iter().all(|(path, _)| path.exists()));
}

// ---------------------------------------------------------------------------
// Narration: the seller's voice names the items (Baseten / Spark
// Open). The narration call is injected as a fake, so no test here
// can reach Baseten or spend from either pool. The audio files are
// real WAVs, so extraction, probing, and the silence gate all run
// their production code.
// ---------------------------------------------------------------------------

fn wav_file(path: &Path, seconds: f64, rate: u32, peak: f64) -> PathBuf {
    let frames = (seconds * rate as f64) as usize;
    let mut data: Vec<u8> = Vec::with_capacity(44 + frames * 2);
    let data_len = (frames * 2) as u32;
    data.extend_from_slice(b"RIFF");
    data.extend_from_slice(&(36 + data_len).to_le_bytes());
    data.extend_from_slice(b"WAVE");
    data.extend_from_slice(b"fmt ");
    data.extend_from_slice(&16u32.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes()); // PCM
    data.extend_from_slice(&1u16.to_le_bytes()); // mono
    data.extend_from_slice(&rate.to_le_bytes());
    data.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate
    data.extend_from_slice(&2u16.to_le_bytes()); // block align
    data.extend_from_slice(&16u16.to_le_bytes()); // bits
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

/// The Baseten narration call, faked at the roomrun boundary. Errors
/// carry the Python "{TypeName}: {message}" formatting the manifest
/// records, exactly as the production fn produces them.
struct FakeNarration {
    reply: String,
    error: Option<String>,
    calls: RefCell<Vec<PathBuf>>,
}

impl FakeNarration {
    fn new(reply: &str) -> Rc<Self> {
        Rc::new(FakeNarration {
            reply: reply.to_string(),
            error: None,
            calls: RefCell::new(Vec::new()),
        })
    }

    fn failing(message: &str) -> Rc<Self> {
        Rc::new(FakeNarration {
            reply: String::new(),
            error: Some(message.to_string()),
            calls: RefCell::new(Vec::new()),
        })
    }

    fn as_fn(self: &Rc<Self>) -> NarrationFn {
        let fake = self.clone();
        Box::new(move |audio_path: &Path| {
            fake.calls.borrow_mut().push(audio_path.to_path_buf());
            if let Some(error) = &fake.error {
                return Err(error.clone());
            }
            Ok(json!({
                "reply": fake.reply,
                "usage": {"prompt_tokens": 1000, "completion_tokens": 500, "total_tokens": 1500},
                "latency_ms": 123,
                "model": NARRATION_MODEL,
            }))
        })
    }
}

const NARRATION_REPLY: &str = "TRANSCRIPT\n\
    Alright, sell this filter. Also some suitcases and other junk here.\n\n\
    SELLING\n\
    filter\n\n\
    MENTIONED\n\
    suitcase\n";

fn narrated_opts(
    fake: &Rc<FakeChain>,
    data_dir: &Path,
    narration: &Rc<FakeNarration>,
) -> RoomRunOptions {
    let mut opts = base_opts(fake, data_dir);
    opts.narration_fn = Some(narration.as_fn());
    opts
}

#[test]
fn parse_narration_reply_splits_named_and_mentioned() {
    let parsed = parse_narration_reply(Some(NARRATION_REPLY));
    assert!(parsed.transcript.starts_with("Alright, sell this filter."));
    assert_eq!(parsed.selling, vec!["filter"]);
    assert_eq!(parsed.mentioned, vec!["suitcase"]);
}

#[test]
fn parse_narration_reply_cleans_bullets_and_none() {
    let parsed = parse_narration_reply(Some(
        "TRANSCRIPT:\nhello\nSELLING:\n- red bike\n2. blue chair\nMENTIONED:\nNONE\n",
    ));
    assert_eq!(parsed.selling, vec!["red bike", "blue chair"]);
    assert!(parsed.mentioned.is_empty());
}

#[test]
fn parse_narration_reply_without_headings_is_all_transcript() {
    let parsed = parse_narration_reply(Some("just some words the model said"));
    assert_eq!(parsed.transcript, "just some words the model said");
    assert!(parsed.selling.is_empty() && parsed.mentioned.is_empty());
}

#[test]
fn probe_wav_and_cost_projection() {
    let root = temp_root("wav");
    let loud = wav_file(&root.join("loud.wav"), 2.0, 8000, 0.5);
    let (duration, peak) = probe_wav(&loud).unwrap();
    assert!((duration - 2.0).abs() < 0.01, "duration {duration}");
    assert!((peak - 0.5).abs() < 0.02, "peak {peak}");
    let silent = wav_file(&root.join("silent.wav"), 1.0, 8000, 0.0);
    let (_, silent_peak) = probe_wav(&silent).unwrap();
    assert_eq!(silent_peak, 0.0);
    // Published rates: $1.25/M in, $4.25/M out; audio priced at
    // 150 tokens/s plus the fixed prompt allowance, output at the cap.
    let projected = project_narration_cost_usd(10.0, 4096);
    assert!(approx(
        projected,
        (10.0 * 150.0 + 512.0) * 1.25e-6 + 4096.0 * 4.25e-6
    ));
    assert!(approx(
        narration_cost_usd(Some(
            &json!({"prompt_tokens": 1_000_000, "completion_tokens": 1_000_000})
        ))
        .unwrap(),
        1.25 + 4.25
    ));
    assert!(narration_cost_usd(Some(&json!({}))).is_none());
}

#[test]
fn narrated_run_stages_named_and_confirms_mentioned() {
    let _guard = env_lock();
    let root = temp_root("narrated");
    let photos = vec![
        photo(&root, "purifier.png", [120, 60, 20, 255]),
        photo(&root, "case.png", [200, 90, 10, 255]),
    ];
    let fake = FakeChain::new(
        &root,
        vec![plan("air purifier"), plan("orange suitcase")],
        contents(&[((0, "filter"), "filter1"), ((1, "suitcase"), "case1")]),
    );
    let narration = FakeNarration::new(NARRATION_REPLY);
    let audio = wav_file(&root.join("walk.wav"), 1.0, 8000, 0.5);
    let mut opts = narrated_opts(&fake, &root, &narration);
    opts.narrated_audios = vec![audio.clone()];
    let out = root.join("out");
    let manifest = run_room(&photos, &[], &out, opts).unwrap();
    assert_eq!(manifest["mode"], "narrated");
    assert_eq!(narration.calls.borrow().len(), 1);

    let section = &manifest["narration"];
    let record = &section["files"][0];
    assert!(record["error"].is_null());
    assert!(record["transcript"]
        .as_str()
        .unwrap()
        .starts_with("Alright, sell this filter."));
    assert_eq!(record["named_items"], json!(["filter"]));
    assert_eq!(record["mentioned_items"], json!(["suitcase"]));
    assert_eq!(section["usage"]["total_tokens"], 1500);
    // Baseten spend is priced at the published rates and kept OUT of
    // the Meta room total.
    assert!(approx(
        section["cost_usd_estimate"].as_f64().unwrap(),
        1000.0 * 1.25e-6 + 500.0 * 4.25e-6
    ));
    assert_eq!(
        manifest["baseten_cost_usd_estimate"],
        section["cost_usd_estimate"]
    );
    assert!(approx(
        manifest["total_cost_usd"].as_f64().unwrap(),
        *fake.spend.borrow()
    ));
    let transcript_path = Path::new(section["transcript_file"].as_str().unwrap());
    assert!(transcript_path.exists());
    assert!(std::fs::read_to_string(transcript_path)
        .unwrap()
        .contains("sell this filter"));

    let items = items_by_name(&manifest);
    assert_eq!(items["filter"]["status"], "staged");
    assert_eq!(
        items["filter"]["narration"],
        json!({"kind": "named", "source": audio.to_string_lossy()})
    );
    assert_eq!(items["filter"]["verification"]["ok"], true);
    let suitcase = &items["suitcase"];
    assert_eq!(suitcase["status"], "staged"); // mentioned, then visually confirmed
    assert_eq!(suitcase["narration"]["kind"], "mentioned");
    assert!(suitcase["flags"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f.as_str().unwrap().contains("mentioned in passing")));
    let summary = std::fs::read_to_string(out.join("summary.txt")).unwrap();
    assert!(summary.contains("NARRATION") && summary.contains("sell this filter"));
}

#[test]
fn narrated_mentioned_item_refuted_when_not_visible() {
    let _guard = env_lock();
    let root = temp_root("refuted");
    let photos = vec![photo(&root, "purifier.png", [120, 60, 20, 255])];
    let fake = FakeChain::new(
        &root,
        vec![plan("air purifier")],
        contents(&[((0, "filter"), "filter1")]), // no unicycle anywhere
    );
    let narration = FakeNarration::new(
        "TRANSCRIPT\nsell this filter, and there is a unicycle somewhere\n\
         SELLING\nfilter\nMENTIONED\nunicycle\n",
    );
    let mut opts = narrated_opts(&fake, &root, &narration);
    opts.narrated_audios = vec![wav_file(&root.join("walk.wav"), 1.0, 8000, 0.5)];
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    let items = items_by_name(&manifest);
    assert_eq!(items["filter"]["status"], "staged");
    let unicycle = &items["unicycle"];
    assert_eq!(unicycle["status"], "failed"); // refuted by the visual pipeline
    assert_eq!(unicycle["failure"]["stage"], "cut_out");
    assert_eq!(unicycle["narration"]["kind"], "mentioned");
    assert!(!unicycle["sightings"].as_array().unwrap().is_empty()); // the refutation evidence is recorded
}

#[test]
fn narration_failure_is_a_row_and_the_stills_run_continues() {
    let _guard = env_lock();
    let root = temp_root("narfail");
    let photos = vec![photo(&root, "case.png", [200, 90, 10, 255])];
    let fake = FakeChain::new(
        &root,
        vec![plan("orange suitcase")],
        contents(&[((0, "orange suitcase"), "case1")]),
    );
    // A Python RuntimeError("baseten is down") reaches the manifest
    // as "RuntimeError: baseten is down".
    let narration = FakeNarration::failing("RuntimeError: baseten is down");
    let mut opts = narrated_opts(&fake, &root, &narration);
    opts.item_names = vec!["orange suitcase".to_string()];
    opts.narrated_audios = vec![wav_file(&root.join("walk.wav"), 1.0, 8000, 0.5)];
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    let record = &manifest["narration"]["files"][0];
    assert_eq!(record["error"]["code"], "NARRATION_FAILED");
    assert!(record["error"]["message"]
        .as_str()
        .unwrap()
        .contains("baseten is down"));
    assert_eq!(manifest["mode"], "named");
    assert_eq!(manifest["items"][0]["status"], "staged"); // stills-only mode intact
    assert!(manifest["notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n.as_str().unwrap().contains("NARRATION_FAILED")));
}

#[test]
fn silent_track_never_reaches_the_model() {
    let _guard = env_lock();
    let root = temp_root("silent");
    let photos = vec![photo(&root, "case.png", [200, 90, 10, 255])];
    let fake = FakeChain::new(
        &root,
        vec![plan("orange suitcase")],
        contents(&[((0, "orange suitcase"), "case1")]),
    );
    let narration = FakeNarration::new(NARRATION_REPLY);
    let mut opts = narrated_opts(&fake, &root, &narration);
    opts.item_names = vec!["orange suitcase".to_string()];
    opts.narrated_audios = vec![wav_file(&root.join("silent.wav"), 1.0, 8000, 0.0)];
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    let record = &manifest["narration"]["files"][0];
    assert_eq!(record["error"]["code"], "SILENT_TRACK");
    assert!(narration.calls.borrow().is_empty()); // no call was made, no pool was touched
    assert_eq!(manifest["items"][0]["status"], "staged");
}

#[test]
fn narration_cost_cap_refuses_before_the_call() {
    let _guard = env_lock();
    let root = temp_root("narcap");
    let photos = vec![photo(&root, "case.png", [200, 90, 10, 255])];
    let fake = FakeChain::new(
        &root,
        vec![plan("orange suitcase")],
        contents(&[((0, "orange suitcase"), "case1")]),
    );
    let narration = FakeNarration::new(NARRATION_REPLY);
    let mut opts = narrated_opts(&fake, &root, &narration);
    opts.item_names = vec!["orange suitcase".to_string()];
    opts.narrated_audios = vec![wav_file(&root.join("walk.wav"), 1.0, 8000, 0.5)];
    opts.narration_max_projected_usd = 0.0001;
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    let record = &manifest["narration"]["files"][0];
    assert_eq!(record["error"]["code"], "NARRATION_COST_REFUSED");
    assert!(record["projected_cost_usd"].as_f64().unwrap() > 0.0001);
    assert!(narration.calls.borrow().is_empty());
    assert_eq!(manifest["items"][0]["status"], "staged");
}

#[test]
fn narration_annotates_a_typed_item_instead_of_duplicating() {
    let _guard = env_lock();
    let root = temp_root("annotate");
    let photos = vec![photo(&root, "case.png", [200, 90, 10, 255])];
    let fake = FakeChain::new(
        &root,
        vec![plan("orange suitcase")],
        contents(&[((0, "orange suitcase"), "case1")]),
    );
    let narration = FakeNarration::new(
        "TRANSCRIPT\nthe orange suitcase is for sale\n\
         SELLING\norange suitcase\nMENTIONED\nNONE\n",
    );
    let mut opts = narrated_opts(&fake, &root, &narration);
    opts.item_names = vec!["orange suitcase".to_string()];
    opts.narrated_audios = vec![wav_file(&root.join("walk.wav"), 1.0, 8000, 0.5)];
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    assert_eq!(manifest["items"].as_array().unwrap().len(), 1);
    let item = &manifest["items"][0];
    assert_eq!(item["status"], "staged");
    assert_eq!(item["narration"]["kind"], "named");
}

#[test]
fn narrated_video_without_audio_track_is_a_row() {
    if !ffmpeg_available() {
        return;
    }
    let _guard = env_lock();
    let root = temp_root("noaudio");
    let video = root.join("silent-walk.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=64x64:rate=5",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&video)
        .arg("-y")
        .status()
        .unwrap();
    assert!(status.success());
    let photos = vec![photo(&root, "case.png", [200, 90, 10, 255])];
    let fake = FakeChain::new(
        &root,
        vec![plan("orange suitcase")],
        contents(&[((0, "orange suitcase"), "case1")]),
    );
    let narration = FakeNarration::new(NARRATION_REPLY);
    let mut opts = narrated_opts(&fake, &root, &narration);
    opts.item_names = vec!["orange suitcase".to_string()];
    opts.narrated_videos = vec![video];
    let manifest = run_room(&photos, &[], &root.join("out"), opts).unwrap();
    let record = &manifest["narration"]["files"][0];
    assert_eq!(record["error"]["code"], "NO_AUDIO_TRACK");
    assert!(narration.calls.borrow().is_empty());
    assert_eq!(manifest["items"][0]["status"], "staged");
}

// ---------------------------------------------------------------------------
// F1: candidate still coverage + collision re-selection post-pass
// ---------------------------------------------------------------------------

fn make_video(dir: &Path, name: &str, seconds: u32) -> PathBuf {
    let video = dir.join(name);
    let status = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=duration={seconds}:size=64x64:rate=5"),
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&video)
        .arg("-y")
        .status()
        .unwrap();
    assert!(status.success());
    video
}

fn flags_of(item: &Value) -> Vec<String> {
    item["flags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn candidate_coverage_keeps_every_still() {
    let mk = |origin: &'static str| PhotoSource {
        path: PathBuf::from("x.jpg"),
        origin,
        video_path: None,
        timestamp_s: None,
        handle: None,
        width: None,
        height: None,
        plan: None,
        plan_error: None,
    };
    let sources = vec![
        mk("still"),
        mk("still"),
        mk("video_frame"),
        mk("video_frame"),
        mk("video_frame"),
    ];
    // Ranking put every frame ahead of both stills; the cap of 3
    // still leaves room for exactly one frame, and probe order
    // stays the ranking's order.
    assert_eq!(
        apply_candidate_coverage(&[2, 3, 4, 0, 1], &sources, 3),
        vec![2, 0, 1]
    );
    // Stills alone can exceed the cap: coverage wins over the cap.
    let sources = vec![
        mk("still"),
        mk("still"),
        mk("still"),
        mk("still"),
        mk("video_frame"),
    ];
    assert_eq!(
        apply_candidate_coverage(&[4, 0, 1, 2, 3], &sources, 2),
        vec![0, 1, 2, 3]
    );
}

#[test]
fn still_coverage_probes_the_still_the_cap_would_have_dropped() {
    if !ffmpeg_available() {
        return;
    }
    let _guard = env_lock();
    let root = temp_root("coverage");
    let photos = vec![
        photo(&root, "shelf.png", [120, 60, 20, 255]),
        photo(&root, "case.png", [200, 90, 10, 255]),
    ];
    // A 5s walkthrough samples frames at 0/2/4s (indices 2..4);
    // every frame plan names the black hat, so the ranking spends
    // the whole 3-photo cap on frames — the still that actually
    // holds the fedora (photo 0) is probed only because still
    // coverage guarantees it a slot.
    let video = make_video(&root, "walk.mp4", 5);
    let fake = FakeChain::new(
        &root,
        vec![
            plan("white bookcase"),
            plan("orange suitcase"),
            plan("black hat"),
            plan("black hat"),
            plan("black hat"),
        ],
        contents(&[((0, "black hat"), "fedora")]),
    );
    let mut opts = base_opts(&fake, &root);
    opts.item_names = vec!["black hat".to_string()];
    let manifest = run_room(&photos, &[video], &root.join("out"), opts).unwrap();
    let item = &manifest["items"][0];
    assert_eq!(item["status"], "staged");
    assert_eq!(item["primary_photo_index"], 0);
    let probed: Vec<u64> = item["sightings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["photo_index"].as_u64().unwrap())
        .collect();
    assert_eq!(probed, vec![2, 0, 1]); // one frame fills the cap, both stills probed
}

/// The miniature acceptance scenario: two hat items, both primaries
/// the same straw-hat mask on a video frame, the fedora a distinct
/// mask on the still. The crop identifies name what each crop
/// actually shows, so the straw hat is the keeper and the black hat
/// must re-stage from the still.
fn two_hat_run(tag: &str, crop_plans: Vec<Value>) -> (PathBuf, Rc<FakeChain>, Value) {
    let root = temp_root(tag);
    let photos = vec![
        photo(&root, "shelf.png", [120, 60, 20, 255]),
        photo(&root, "case.png", [200, 90, 10, 255]),
    ];
    let video = make_video(&root, "walk.mp4", 1);
    let mut fake = FakeChain::new(
        &root,
        vec![
            plan("white bookcase"),
            plan("orange suitcase"),
            plan("room corner"),
        ],
        contents(&[
            ((0, "black hat"), "fedora"),
            ((2, "straw hat"), "straw"),
            ((2, "hat"), "straw"),
        ]),
    );
    {
        let fake_mut = Rc::get_mut(&mut fake).unwrap();
        fake_mut.opaque_by_object.insert("straw".to_string(), 400);
        fake_mut.opaque_by_object.insert("fedora".to_string(), 200);
    }
    for crop_plan in crop_plans {
        fake.crop_plans.borrow_mut().push_back(crop_plan);
    }
    let mut opts = base_opts(&fake, &root);
    opts.item_names = vec!["straw hat".to_string(), "black hat".to_string()];
    let manifest = run_room(&photos, &[video], &root.join("out"), opts).unwrap();
    (root, fake, manifest)
}

/// The round-2 acceptance shape: the two hat primaries sit on
/// DIFFERENT photos. The straw hat's primary is the still (its
/// largest sighting, the straw hat filling the frame); the black
/// hat's primary is the frame sighting of that same straw hat,
/// which the straw hat item also holds as a NON-primary sighting.
/// The collision is visible only through that non-primary sighting.
/// The fedora is the black hat's smaller sighting on the still.
fn round2_hat_run(tag: &str, crop_plans: Vec<Value>) -> (PathBuf, Rc<FakeChain>, Value) {
    let root = temp_root(tag);
    let photos = vec![
        photo(&root, "shelf.png", [120, 60, 20, 255]),
        photo(&root, "case.png", [200, 90, 10, 255]),
    ];
    let video = make_video(&root, "walk.mp4", 1);
    let mut fake = FakeChain::new(
        &root,
        vec![
            plan("white bookcase"),
            plan("orange suitcase"),
            plan("room corner"),
        ],
        contents(&[
            ((0, "black hat"), "fedora"),
            ((0, "straw hat"), "strawbig"),
            ((2, "straw hat"), "straw"),
            ((2, "hat"), "straw"),
        ]),
    );
    {
        let fake_mut = Rc::get_mut(&mut fake).unwrap();
        fake_mut
            .opaque_by_object
            .insert("strawbig".to_string(), 500);
        fake_mut.opaque_by_object.insert("straw".to_string(), 400);
        fake_mut.opaque_by_object.insert("fedora".to_string(), 200);
    }
    for crop_plan in crop_plans {
        fake.crop_plans.borrow_mut().push_back(crop_plan);
    }
    let mut opts = base_opts(&fake, &root);
    opts.item_names = vec!["straw hat".to_string(), "black hat".to_string()];
    let manifest = run_room(&photos, &[video], &root.join("out"), opts).unwrap();
    (root, fake, manifest)
}

#[test]
fn sighting_level_collision_reselects_across_different_primary_photos() {
    if !ffmpeg_available() {
        return;
    }
    let _guard = env_lock();
    let (_root, _fake, manifest) = round2_hat_run(
        "round2collision",
        vec![
            plan("straw hat"),
            plan("straw hat"),
            plan("black fedora hat"),
        ],
    );
    let items = items_by_name(&manifest);
    // The primaries really do start on different photos, and the
    // keeper (the straw hat, whose crop evidence names a straw hat
    // and not a black one) is NOT moved off its still primary.
    let straw = &items["straw hat"];
    assert_eq!(straw["status"], "staged");
    assert_eq!(straw["primary_photo_index"], 0);
    assert!(!flags_of(straw).contains(&"primary_reselected_after_duplicate_collision".to_string()));
    // The black hat's frame primary was the straw hat item's frame
    // sighting; the black hat re-stages from its still sighting
    // (the fedora), even though the keeper's own primary was never
    // the contested object.
    let black = &items["black hat"];
    assert_eq!(black["status"], "staged");
    assert_eq!(black["primary_photo_index"], 0);
    assert_eq!(black["also_seen_in"], json!([2]));
    assert!(flags_of(black).contains(&"primary_reselected_after_duplicate_collision".to_string()));
    assert_eq!(black["listing"]["source"], "item_crop");
    assert_eq!(black["listing"]["title"], "Listing: black fedora hat");
    assert_eq!(black["verification"]["ok"], true);
    assert!(manifest["notes"].as_array().unwrap().iter().any(|n| {
        let n = n.as_str().unwrap();
        n.contains("'black hat'") && n.contains("'straw hat'") && n.contains("re-selected")
    }));
}

#[test]
fn sighting_level_collision_with_ambiguous_evidence_flags_only() {
    if !ffmpeg_available() {
        return;
    }
    let _guard = env_lock();
    // Same round-2 shape, but both crops identify as the bare noun
    // "hat": the evidence matches neither item's full name, so
    // there is no keeper, no re-selection, and both primaries stand
    // with the pair flagged as possible duplicates.
    let (_root, _fake, manifest) =
        round2_hat_run("round2ambiguous", vec![plan("hat"), plan("hat")]);
    let items = items_by_name(&manifest);
    assert_eq!(items["straw hat"]["primary_photo_index"], 0);
    assert_eq!(items["black hat"]["primary_photo_index"], 2);
    for name in ["straw hat", "black hat"] {
        let item = &items[name];
        assert_eq!(item["status"], "staged");
        assert!(
            !flags_of(item).contains(&"primary_reselected_after_duplicate_collision".to_string())
        );
    }
    assert!(items["black hat"]["possible_duplicate_of"]
        .as_array()
        .unwrap()
        .contains(&json!("straw hat")));
    assert!(items["straw hat"]["possible_duplicate_of"]
        .as_array()
        .unwrap()
        .contains(&json!("black hat")));
}

#[test]
fn duplicate_flags_persist_after_sighting_level_reselection() {
    if !ffmpeg_available() {
        return;
    }
    let _guard = env_lock();
    let (_root, _fake, manifest) = round2_hat_run(
        "round2flagspersist",
        vec![
            plan("straw hat"),
            plan("straw hat"),
            plan("black fedora hat"),
        ],
    );
    let items = items_by_name(&manifest);
    let black = &items["black hat"];
    // The re-selection happened...
    assert!(flags_of(black).contains(&"primary_reselected_after_duplicate_collision".to_string()));
    assert_eq!(black["primary_photo_index"], 0);
    // ...and the de-dup record of the probing ambiguity stands on
    // BOTH items: the mutual possible_duplicate_of entries and the
    // sighting-overlap flags are not removed by the resolution.
    assert!(black["possible_duplicate_of"]
        .as_array()
        .unwrap()
        .contains(&json!("straw hat")));
    assert!(items["straw hat"]["possible_duplicate_of"]
        .as_array()
        .unwrap()
        .contains(&json!("black hat")));
    assert!(flags_of(black)
        .iter()
        .any(|f| f.contains("a sighting of 'straw hat' on photo 2 overlaps this item's primary")));
    assert!(flags_of(&items["straw hat"])
        .iter()
        .any(|f| f.contains("this item's sighting on photo 2 overlaps 'black hat''s primary")));
}

#[test]
fn collision_reselection_restages_the_displaced_item_from_its_still() {
    if !ffmpeg_available() {
        return;
    }
    let _guard = env_lock();
    let (_root, _fake, manifest) = two_hat_run(
        "collision",
        vec![
            plan("straw hat"),
            plan("straw hat"),
            plan("black fedora hat"),
        ],
    );
    let items = items_by_name(&manifest);
    let straw = &items["straw hat"];
    assert_eq!(straw["status"], "staged");
    assert_eq!(straw["primary_photo_index"], 2); // the keeper keeps the frame primary
    assert!(!flags_of(straw).contains(&"primary_reselected_after_duplicate_collision".to_string()));

    let black = &items["black hat"];
    assert_eq!(black["status"], "staged");
    assert_eq!(black["primary_photo_index"], 0); // re-staged from the still
    assert_eq!(black["also_seen_in"], json!([2]));
    assert!(flags_of(black).contains(&"primary_reselected_after_duplicate_collision".to_string()));
    // The duplicate flags from the de-dup pass stand.
    assert!(black["possible_duplicate_of"]
        .as_array()
        .unwrap()
        .contains(&json!("straw hat")));
    // The listing was recomputed from the new primary's own crop.
    assert_eq!(black["listing"]["source"], "item_crop");
    assert_eq!(black["listing"]["title"], "Listing: black fedora hat");
    assert_eq!(black["verification"]["ok"], true);
    // A run-level note names the collision partner.
    assert!(manifest["notes"].as_array().unwrap().iter().any(|n| {
        let n = n.as_str().unwrap();
        n.contains("'black hat'") && n.contains("'straw hat'") && n.contains("re-selected")
    }));
}

#[test]
fn collision_without_an_alternate_sighting_keeps_the_primary_and_flags() {
    if !ffmpeg_available() {
        return;
    }
    let _guard = env_lock();
    let root = temp_root("noalternate");
    let photos = vec![photo(&root, "shelf.png", [120, 60, 20, 255])];
    let video = make_video(&root, "walk.mp4", 1);
    let mut fake = FakeChain::new(
        &root,
        vec![plan("white bookcase"), plan("room corner")],
        contents(&[((1, "straw hat"), "straw"), ((1, "hat"), "straw")]),
    );
    Rc::get_mut(&mut fake)
        .unwrap()
        .opaque_by_object
        .insert("straw".to_string(), 400);
    for crop_plan in [plan("straw hat"), plan("straw hat")] {
        fake.crop_plans.borrow_mut().push_back(crop_plan);
    }
    let mut opts = base_opts(&fake, &root);
    opts.item_names = vec!["straw hat".to_string(), "black hat".to_string()];
    let manifest = run_room(&photos, &[video], &root.join("out"), opts).unwrap();
    let items = items_by_name(&manifest);
    let black = &items["black hat"];
    assert_eq!(black["status"], "staged");
    assert_eq!(black["primary_photo_index"], 1); // kept: its only sighting is the collision
    assert!(!flags_of(black).contains(&"primary_reselected_after_duplicate_collision".to_string()));
    assert!(flags_of(black)
        .iter()
        .any(|f| f.contains("no non-colliding alternate sighting exists")));
    assert!(black["possible_duplicate_of"]
        .as_array()
        .unwrap()
        .contains(&json!("straw hat")));
}

#[test]
fn collision_with_ambiguous_evidence_is_not_reselected() {
    if !ffmpeg_available() {
        return;
    }
    let _guard = env_lock();
    // Both crops identify as the bare noun "hat": the evidence
    // matches neither item's full name at the plan-match bar, so
    // there is no keeper and both primaries stand.
    let (_root, _fake, manifest) = two_hat_run("ambiguous", vec![plan("hat"), plan("hat")]);
    let items = items_by_name(&manifest);
    for name in ["straw hat", "black hat"] {
        let item = &items[name];
        assert_eq!(item["status"], "staged");
        assert_eq!(item["primary_photo_index"], 2);
        assert!(
            !flags_of(item).contains(&"primary_reselected_after_duplicate_collision".to_string())
        );
    }
    assert!(items["black hat"]["possible_duplicate_of"]
        .as_array()
        .unwrap()
        .contains(&json!("straw hat")));
}

#[test]
fn failed_reselection_reverts_to_the_original_staged_result() {
    if !ffmpeg_available() {
        return;
    }
    let _guard = env_lock();
    let root = temp_root("restagefail");
    let photos = vec![
        photo(&root, "shelf.png", [120, 60, 20, 255]),
        photo(&root, "case.png", [200, 90, 10, 255]),
    ];
    let video = make_video(&root, "walk.mp4", 1);
    let mut fake = FakeChain::new(
        &root,
        vec![
            plan("white bookcase"),
            plan("orange suitcase"),
            plan("room corner"),
        ],
        contents(&[
            ((0, "black hat"), "fedora"),
            ((2, "straw hat"), "straw"),
            ((2, "hat"), "straw"),
        ]),
    );
    {
        let fake_mut = Rc::get_mut(&mut fake).unwrap();
        fake_mut.opaque_by_object.insert("straw".to_string(), 400);
        fake_mut.opaque_by_object.insert("fedora".to_string(), 200);
        fake_mut.draw_fail_objects.insert("fedora".to_string());
    }
    for crop_plan in [
        plan("straw hat"),
        plan("straw hat"),
        plan("black fedora hat"),
    ] {
        fake.crop_plans.borrow_mut().push_back(crop_plan);
    }
    let mut opts = base_opts(&fake, &root);
    opts.item_names = vec!["straw hat".to_string(), "black hat".to_string()];
    let manifest = run_room(&photos, &[video], &root.join("out"), opts).unwrap();
    let items = items_by_name(&manifest);
    let black = &items["black hat"];
    assert_eq!(black["status"], "staged"); // the original staged result stands
    assert_eq!(black["primary_photo_index"], 2);
    assert_eq!(black["listing"]["title"], "Listing: straw hat");
    assert!(!flags_of(black).contains(&"primary_reselected_after_duplicate_collision".to_string()));
    assert!(flags_of(black)
        .iter()
        .any(|f| f.contains("re-selection after the collision with 'straw hat' failed")));
    // The failed attempt's spend (crop identify + refused draw)
    // still counts into the item: probes 5 cuts + crop + draw,
    // then the re-stage's crop + draw.
    assert!(approx(
        black["cost_usd"].as_f64().unwrap(),
        5.0 * CUT_COST + 2.0 * IDENTIFY_COST + 2.0 * DRAW_COST
    ));
}

#[test]
fn duplicate_flag_reaches_a_sighting_that_is_not_a_primary() {
    if !ffmpeg_available() {
        return;
    }
    let _guard = env_lock();
    // The acceptance run's flag shape in the Rust build: the straw
    // hat's primary is the still (its largest sighting), the black
    // hat's primary is the frame — but the straw hat's frame
    // sighting is the same mask the black hat staged, so the pair
    // is flagged even though the primaries sit on different photos.
    let root = temp_root("sightingdup");
    let photos = vec![photo(&root, "shelf.png", [120, 60, 20, 255])];
    let video = make_video(&root, "walk.mp4", 1);
    let mut fake = FakeChain::new(
        &root,
        vec![plan("white bookcase"), plan("room corner")],
        contents(&[
            ((0, "straw hat"), "strawbig"),
            ((1, "straw hat"), "straw"),
            ((1, "hat"), "straw"),
        ]),
    );
    {
        let fake_mut = Rc::get_mut(&mut fake).unwrap();
        fake_mut
            .opaque_by_object
            .insert("strawbig".to_string(), 500);
        fake_mut.opaque_by_object.insert("straw".to_string(), 400);
    }
    for crop_plan in [plan("straw hat"), plan("straw hat")] {
        fake.crop_plans.borrow_mut().push_back(crop_plan);
    }
    let mut opts = base_opts(&fake, &root);
    opts.item_names = vec!["straw hat".to_string(), "black hat".to_string()];
    let manifest = run_room(&photos, &[video], &root.join("out"), opts).unwrap();
    let items = items_by_name(&manifest);
    assert_eq!(items["straw hat"]["primary_photo_index"], 0);
    assert_eq!(items["black hat"]["primary_photo_index"], 1);
    assert!(items["black hat"]["possible_duplicate_of"]
        .as_array()
        .unwrap()
        .contains(&json!("straw hat")));
    assert!(items["straw hat"]["possible_duplicate_of"]
        .as_array()
        .unwrap()
        .contains(&json!("black hat")));
    assert!(flags_of(&items["black hat"])
        .iter()
        .any(|f| f.contains("a sighting of 'straw hat' on photo 1")));
    // The sighting-level contest fires (the black hat's primary is
    // contested and the straw hat is the keeper), but the black hat
    // has no alternate sighting to re-stage from, so its primary is
    // kept and the flag says why; no re-selection happened.
    assert!(flags_of(&items["black hat"])
        .iter()
        .any(|f| f.contains("no non-colliding alternate sighting exists")));
    assert!(!flags_of(&items["black hat"])
        .contains(&"primary_reselected_after_duplicate_collision".to_string()));
}

// ---------------------------------------------------------------------------
// Cross-implementation summary parity: the Rust summary renderer,
// fed the PYTHON runs' manifests, must reproduce the Python runs'
// summary.txt byte for byte (values and all — the manifests are the
// frozen record of the v2 and narrated Python runs).
// ---------------------------------------------------------------------------

#[test]
fn summarize_manifest_matches_python_output_on_frozen_manifests() {
    use agenthand::roomrun::summarize_manifest;
    let home = std::env::var("HOME").unwrap();
    for run in [
        "daniels-room-2026-10-28-v2",
        "daniels-room-narrated-2026-10-28",
    ] {
        let dir = PathBuf::from(format!("{home}/workspace/meta-hackathon/roomrun/{run}"));
        let (Ok(manifest_text), Ok(expected)) = (
            std::fs::read_to_string(dir.join("manifest.json")),
            std::fs::read_to_string(dir.join("summary.txt")),
        ) else {
            continue; // frozen run not present in this environment
        };
        let manifest: Value = serde_json::from_str(&manifest_text).unwrap();
        // The workflow-completion lines (listing title + pricing,
        // added for staged items after these runs were frozen) are
        // additive: every pre-existing line must still match the
        // frozen summary byte for byte.
        let rendered: String = summarize_manifest(&manifest)
            .lines()
            .filter(|line| {
                let trimmed = line.trim_start();
                !trimmed.starts_with("listing title:") && !trimmed.starts_with("pricing:")
            })
            .map(|line| format!("{line}\n"))
            .collect();
        assert_eq!(rendered, expected, "summary mismatch for {run}");
    }
}
