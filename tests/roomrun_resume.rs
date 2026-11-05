//! P1 acceptance test (Hermes dive, tracker event 372): a room
//! run killed mid-run resumes from the step log without
//! re-billing completed work, and its final manifest matches an
//! uninterrupted run's manifest.
//!
//! The crash is simulated by a fake chain whose cut_out panics
//! (the process-death shape: everything already settled persists,
//! the in-flight call never returns) inside item 3's probe, after
//! one of its photos was already probed and checkpointed.
//! `catch_unwind` stands in for the process boundary; the step
//! rows, the asset store, and the fake's spend ledger are the
//! durable state a real restart would see.

use agenthand::assets::AssetStore;
use agenthand::image::{encode_png_rgba, RgbaImage};
use agenthand::roomrun::{run_room, RoomChain, RoomRunOptions, VerifyFn};
use agenthand::sqlledger::RunStepLog;
use base64::Engine;
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

const IDENTIFY_COST: f64 = 0.008;
const CUT_COST: f64 = 0.0025;
const DRAW_COST: f64 = 0.01;

fn temp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("agenthand-resume-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
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

fn data_uri_for(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap();
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    )
}

/// Deterministic fake chain, content-keyed (never call-order
/// keyed, so a resumed run's reduced call sequence still gets the
/// right answers). `crash_armed` makes the targeted cut_out call
/// panic before anything settles — a kill -9 as the call was
/// attempted.
struct Fake {
    store: AssetStore,
    plans_by_uri: HashMap<String, Value>,
    contents: HashMap<(usize, String), String>,
    photo_handles: RefCell<HashMap<String, usize>>,
    fg_object: RefCell<HashMap<String, String>>,
    spend: Cell<f64>,
    identify_calls: Cell<usize>,
    crop_identify_calls: Cell<usize>,
    cut_out_calls: Cell<usize>,
    draw_calls: Cell<usize>,
    crash_armed: Cell<bool>,
    crash_target: (usize, String),
}

impl Fake {
    fn identify(&self, image_ref: &str) -> Result<Value, String> {
        // Two call shapes reach identify: the plan pass (a photo
        // data URI) and the crop-identify inside staging (an
        // asset handle, when the primary photo's plan names a
        // different item). Crop answers are a fixed generic plan —
        // identical for every call, so control and resumed runs
        // produce identical listings regardless of call sequence.
        if !self.plans_by_uri.contains_key(image_ref) {
            self.crop_identify_calls
                .set(self.crop_identify_calls.get() + 1);
            self.spend.set(self.spend.get() + IDENTIFY_COST);
            return Ok(json!({
                "status": "ok",
                "image": image_ref,
                "suggested_title": "Listing: cropped item",
                "listing_bullets": ["one cropped item"],
                "condition_notes": "good",
                "cost_usd": IDENTIFY_COST,
            }));
        }
        self.identify_calls.set(self.identify_calls.get() + 1);
        self.spend.set(self.spend.get() + IDENTIFY_COST);
        let plan = self
            .plans_by_uri
            .get(image_ref)
            .cloned()
            .expect("identify called with an unknown image");
        let handle = agenthand::ingest::ingest(image_ref, &self.store)?;
        // Recover the photo index from the plan's concept position:
        // plans were inserted in photo order and the URI map keeps
        // its own index in the plan's helper field.
        let idx = plan["photo_index"].as_u64().unwrap() as usize;
        self.photo_handles.borrow_mut().insert(handle.clone(), idx);
        let mut result = json!({
            "status": "ok",
            "image": handle,
            "cost_usd": IDENTIFY_COST,
        });
        for (key, value) in plan.as_object().unwrap() {
            if key != "photo_index" {
                result[key] = value.clone();
            }
        }
        Ok(result)
    }

    fn cut_out(&self, handle: &str, concept: &str) -> Result<Value, String> {
        let idx = self
            .photo_handles
            .borrow()
            .get(handle)
            .copied()
            .expect("cut_out called with an unknown handle");
        if self.crash_armed.get() && (idx, concept.to_string()) == self.crash_target {
            self.crash_armed.set(false);
            panic!("simulated kill -9: process died as this call was attempted");
        }
        self.cut_out_calls.set(self.cut_out_calls.get() + 1);
        self.spend.set(self.spend.get() + CUT_COST);
        let Some(object_id) = self.contents.get(&(idx, concept.to_string())).cloned() else {
            return Ok(json!({
                "status": "error",
                "error": {
                    "code": "NO_MATCH",
                    "message": format!("no {concept} in photo {idx}"),
                    "cost_counted_usd": CUT_COST,
                },
            }));
        };
        let side = 10 + (idx as u32);
        let fg = RgbaImage::solid(side, side, [200, 30, 30, 255]);
        let fg_handle = self
            .store
            .put_image(&fg, agenthand::assets::KIND_FOREGROUND, None)?;
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
        self.draw_calls.set(self.draw_calls.get() + 1);
        self.spend.set(self.spend.get() + DRAW_COST);
        let object_id = self
            .fg_object
            .borrow()
            .get(fg_handle)
            .cloned()
            .expect("draw called with an unknown foreground");
        let fg_record = self.store.get_record(fg_handle)?;
        let composite = self.store.put_image(
            &RgbaImage::solid(8, 8, [10, 10, 10, 255]),
            agenthand::assets::KIND_COMPOSITE,
            Some(&fg_record.asset_id),
        )?;
        let receipt = json!({
            "source_asset": "asset://source",
            "foreground_pixel_sha256": fg_record.pixel_sha256,
            "opaque_pixels": 100,
            "changed_opaque_pixels": 0,
            "color_profile_preserved": true,
        });
        self.store.save_receipt(&composite, &receipt)?;
        let _ = object_id;
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
        Box::new(move || Ok(fake.spend.get()))
    }
}

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

const ITEMS: [&str; 6] = [
    "red mug",
    "green plate",
    "green lamp",
    "brass clock",
    "wooden chair",
    "silver kettle",
];

fn build_fake(root: &Path, photos: &[PathBuf]) -> Rc<Fake> {
    let mut plans_by_uri = HashMap::new();
    for (i, path) in photos.iter().enumerate() {
        let mut p = plan(ITEMS[i]);
        p["photo_index"] = json!(i);
        plans_by_uri.insert(data_uri_for(path), p);
    }
    // Photo 1 holds both the plate (its own item) and the lamp
    // (item 3) — item 3's own photo (photo 2) is a miss, so its
    // probe spans two photos and the crash can land mid-probe.
    let contents: HashMap<(usize, String), String> = [
        ((0, "red mug"), "mug1"),
        ((1, "green plate"), "plate1"),
        ((1, "green lamp"), "lamp1"),
        ((3, "brass clock"), "clock1"),
        ((4, "wooden chair"), "chair1"),
        ((5, "silver kettle"), "kettle1"),
    ]
    .iter()
    .map(|((i, c), o)| ((*i, c.to_string()), o.to_string()))
    .collect();
    Rc::new(Fake {
        store: AssetStore::new(&root.join("store")),
        plans_by_uri,
        contents,
        photo_handles: RefCell::new(HashMap::new()),
        fg_object: RefCell::new(HashMap::new()),
        spend: Cell::new(0.0),
        identify_calls: Cell::new(0),
        crop_identify_calls: Cell::new(0),
        cut_out_calls: Cell::new(0),
        draw_calls: Cell::new(0),
        crash_armed: Cell::new(false),
        crash_target: (1, "green lamp".to_string()),
    })
}

fn opts(fake: &Rc<Fake>, root: &Path, run_id: &str) -> RoomRunOptions {
    RoomRunOptions {
        item_names: ITEMS.iter().map(|s| s.to_string()).collect(),
        budget_usd: 5.00,
        room_id: Some(run_id.to_string()),
        probe_all_candidates: false,
        store: Some(AssetStore::new(&root.join("store"))),
        chain: Some(fake.chain()),
        spend_probe: Some(fake.spend_probe()),
        verify_fn: Some(verify_ok()),
        ..RoomRunOptions::default()
    }
}

/// Strip the fields that legitimately differ between a control
/// run and a crash+resume run, then compare the rest exactly.
/// Asset ids carry a per-store component (the same photo ingested
/// into two stores gets two ids), so ids are rewritten to
/// first-appearance placeholders — the manifests are structurally
/// identical, so the placeholder sequences line up one to one.
/// (Pixel hashes, which ARE content-derived, match as-is.)
fn normalize(manifest: &Value, root: &Path) -> Value {
    let mut text = serde_json::to_string(manifest).unwrap();
    text = text.replace(&root.to_string_lossy().to_string(), "<ROOT>");
    let mut out = String::with_capacity(text.len());
    let mut ids: HashMap<String, usize> = HashMap::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // A run of exactly 32 hex chars = an asset id.
        if chars[i].is_ascii_hexdigit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_hexdigit() {
                i += 1;
            }
            let run: String = chars[start..i].iter().collect();
            if run.len() == 32 {
                let next = ids.len() + 1;
                let n = *ids.entry(run).or_insert(next);
                out.push_str(&format!("<ID{n}>"));
            } else {
                out.push_str(&run);
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    let mut value: Value = serde_json::from_str(&out).unwrap();
    let obj = value.as_object_mut().unwrap();
    obj.remove("created_at");
    obj.remove("resumed");
    obj.remove("total_cost_usd");
    for item in value["items"].as_array_mut().unwrap() {
        item.as_object_mut().unwrap().remove("replayed");
    }
    value
}

#[test]
fn killed_room_run_resumes_without_rebilling() {
    // run_room sets process env (the room ledger pins); a panic
    // skips its restore, so save + restore by hand around the
    // crashed invocation.
    let saved: Vec<(&str, Option<std::ffi::OsString>)> =
        ["AGENTHAND_LEDGER_PATH", "AGENTHAND_LEDGER"]
            .iter()
            .map(|k| (*k, std::env::var_os(k)))
            .collect();
    let restore = |saved: &Vec<(&str, Option<std::ffi::OsString>)>| {
        for (k, v) in saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    };

    // ---- the room ------------------------------------------------
    let root = temp_root("room");
    let photos: Vec<PathBuf> = (0..6)
        .map(|i| {
            photo(
                &root,
                &format!("photo{i}.png"),
                [10 + i as u8 * 30, 40, 90, 255],
            )
        })
        .collect();

    // ---- crash run ------------------------------------------------
    let fake = build_fake(&root, &photos);
    fake.crash_armed.set(true);
    let out = root.join("out");
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_room(&photos, &[], &out, opts(&fake, &root, "resume-room"))
    }));
    assert!(crashed.is_err(), "the armed crash must kill the run");
    restore(&saved);
    let crash_spend = fake.spend.get();
    let crash_identifies = fake.identify_calls.get();
    let crash_cuts = fake.cut_out_calls.get();
    let crash_draws = fake.draw_calls.get();
    assert_eq!(crash_identifies, 6, "all photo plans settled pre-crash");
    assert_eq!(crash_draws, 2, "two items fully staged pre-crash");
    assert!(crash_spend > 0.0);

    // The step log holds the durable state the resume will use.
    let log = RunStepLog::open(&out.join("room-steps.sqlite3")).unwrap();
    let row = log.get("resume-room", 0, "draw").unwrap().unwrap();
    assert_eq!(row.status, "complete");
    let row = log.get("resume-room", 2, "cut_out").unwrap().unwrap();
    assert_eq!(row.status, "pending", "item 3 died mid-probe");
    let checkpoint: Value = serde_json::from_str(row.result_ref.as_deref().unwrap()).unwrap();
    assert_eq!(
        checkpoint["sightings"].as_array().unwrap().len(),
        1,
        "one photo was probed + checkpointed before the kill"
    );

    // ---- resume ----------------------------------------------------
    let resumed_manifest = run_room(&photos, &[], &out, opts(&fake, &root, "resume-room")).unwrap();
    restore(&saved);
    assert_eq!(resumed_manifest["resumed"], json!(true));
    assert_eq!(
        fake.identify_calls.get() - crash_identifies,
        0,
        "no photo plan is re-billed on resume"
    );
    let items = resumed_manifest["items"].as_array().unwrap();
    assert_eq!(items.len(), 6);
    for item in items {
        assert_eq!(item["status"], "staged", "item {} staged", item["name"]);
    }
    assert_eq!(items[0]["replayed"], json!(true));
    assert_eq!(items[1]["replayed"], json!(true));
    assert!(
        items[2].get("replayed").is_none(),
        "item 3 finished live (probe checkpoint + live stage), not replayed"
    );
    let resume_spend = fake.spend.get() - crash_spend;
    let resume_cuts = fake.cut_out_calls.get() - crash_cuts;
    let resume_draws = fake.draw_calls.get() - crash_draws;

    // Item 3's cut_out row shows the resume as a second attempt.
    let row = log.get("resume-room", 2, "cut_out").unwrap().unwrap();
    assert_eq!(row.status, "complete");
    assert_eq!(row.attempts, 2);

    // ---- control run ------------------------------------------------
    let croot = temp_root("control");
    let cphotos: Vec<PathBuf> = (0..6)
        .map(|i| {
            photo(
                &croot,
                &format!("photo{i}.png"),
                [10 + i as u8 * 30, 40, 90, 255],
            )
        })
        .collect();
    let cfake = build_fake(&croot, &cphotos);
    let cout = croot.join("out");
    let control_manifest =
        run_room(&cphotos, &[], &cout, opts(&cfake, &croot, "resume-room")).unwrap();
    restore(&saved);
    assert!(control_manifest.get("resumed").is_none());

    // (a) No double billing: crash + resume spend and call counts
    // equal the uninterrupted run's, exactly.
    assert!(
        (crash_spend + resume_spend - cfake.spend.get()).abs() < 1e-9,
        "spend: crash {crash_spend} + resume {resume_spend} != control {}",
        cfake.spend.get()
    );
    assert_eq!(
        crash_cuts + resume_cuts,
        cfake.cut_out_calls.get(),
        "cut_out calls: no probe is re-billed"
    );
    assert_eq!(
        crash_draws + resume_draws,
        cfake.draw_calls.get(),
        "draw calls: no staged item is re-drawn"
    );
    assert_eq!(cfake.identify_calls.get(), 6);
    // Item 3's listing comes from a crop identify in both runs
    // (its primary photo's plan names the plate, not the lamp):
    // one in the control run, one in the resume, none pre-crash.
    assert_eq!(cfake.crop_identify_calls.get(), 1);
    assert_eq!(fake.crop_identify_calls.get(), 1);

    // (b) The final manifests match.
    assert_eq!(
        normalize(&resumed_manifest, &root),
        normalize(&control_manifest, &croot),
        "resumed manifest must equal the uninterrupted manifest"
    );
    // Item-level cost sums also match (photo identifies are
    // run-level, the rest is per item).
    let item_cost = |m: &Value| -> f64 {
        m["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["cost_usd"].as_f64().unwrap_or(0.0))
            .sum()
    };
    assert!((item_cost(&resumed_manifest) - item_cost(&control_manifest)).abs() < 1e-9);

    // ---- third run: full replay, zero calls -------------------------
    let before = (
        fake.identify_calls.get(),
        fake.cut_out_calls.get(),
        fake.draw_calls.get(),
        fake.spend.get(),
    );
    let third = run_room(&photos, &[], &out, opts(&fake, &root, "resume-room")).unwrap();
    restore(&saved);
    assert_eq!(third["resumed"], json!(true));
    assert_eq!(
        (
            fake.identify_calls.get(),
            fake.cut_out_calls.get(),
            fake.draw_calls.get()
        ),
        (before.0, before.1, before.2),
        "a rerun of a finished run makes no billed calls at all"
    );
    assert!((fake.spend.get() - before.3).abs() < 1e-9);
    for item in third["items"].as_array().unwrap() {
        assert_eq!(item["replayed"], json!(true));
        assert_eq!(item["status"], "staged");
    }
}
