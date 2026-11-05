//! KEEP intent acceptance (Daniel's spec, 2026-10-31): one
//! narrated run containing a sell item, two keep items (one with a
//! RESHOOT-grade cut), a mentioned item the photos confirm, and a
//! mentioned item they refute (NO_MATCH). Keep items must get
//! identify + cut_out only — no draw, no listing — and produce
//! ownership records binding the source file's hash and its EXIF
//! capture time; every receipt in the run verifies 0 changed.

use agenthand::assets::AssetStore;
use agenthand::image::{encode_png_rgba, sha256_hex, RgbaImage};
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
const CROP_CONDITION: &str = "brass shows light tarnish on the stem";
const EXIF_DT: &str = "2026:10:31 09:15:00";

fn temp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("agenthand-keep-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn crc32_ieee(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB88320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *slot = c;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// A PNG photo; when `with_exif` is set, an eXIf chunk carrying
/// DateTimeOriginal EXIF_DT rides right after IHDR.
fn photo(dir: &Path, name: &str, color: [u8; 4], with_exif: bool) -> PathBuf {
    let path = dir.join(name);
    let img = RgbaImage::solid(48, 36, color);
    let mut png = encode_png_rgba(&img);
    if with_exif {
        // Little-endian TIFF: IFD0 {0x8769 -> Exif IFD}, Exif IFD
        // {0x9003 ASCII[20] -> the datetime string}.
        let mut tiff: Vec<u8> = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42u16.to_le_bytes());
        tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 at 8
        tiff.extend_from_slice(&1u16.to_le_bytes()); // 1 entry
        tiff.extend_from_slice(&0x8769u16.to_le_bytes());
        tiff.extend_from_slice(&4u16.to_le_bytes()); // LONG
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&26u32.to_le_bytes()); // Exif IFD at 26
        tiff.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
        tiff.extend_from_slice(&1u16.to_le_bytes()); // 1 entry
        tiff.extend_from_slice(&0x9003u16.to_le_bytes());
        tiff.extend_from_slice(&2u16.to_le_bytes()); // ASCII
        tiff.extend_from_slice(&20u32.to_le_bytes());
        tiff.extend_from_slice(&44u32.to_le_bytes()); // string at 44
        tiff.extend_from_slice(&0u32.to_le_bytes());
        let mut dt = EXIF_DT.as_bytes().to_vec();
        dt.push(0);
        tiff.extend_from_slice(&dt);
        assert_eq!(tiff.len(), 64);
        let mut chunk: Vec<u8> = Vec::new();
        chunk.extend_from_slice(&(tiff.len() as u32).to_be_bytes());
        chunk.extend_from_slice(b"eXIf");
        chunk.extend_from_slice(&tiff);
        let crc = crc32_ieee(&chunk[4..]);
        chunk.extend_from_slice(&crc.to_be_bytes());
        // IHDR chunk ends at byte 33 (8 sig + 4 len + 4 type + 13 + 4 crc).
        let mut out = png[..33].to_vec();
        out.extend_from_slice(&chunk);
        out.extend_from_slice(&png[33..]);
        png = out;
    }
    std::fs::write(&path, png).unwrap();
    path
}

/// A half-second of loud sine — passes the silence/duration gates.
fn narration_wav(dir: &Path) -> PathBuf {
    let path = dir.join("walkthrough.wav");
    let rate = 8000u32;
    let frames = 4000usize;
    let mut data: Vec<u8> = Vec::new();
    data.extend_from_slice(b"RIFF");
    data.extend_from_slice(&(36 + frames as u32 * 2).to_le_bytes());
    data.extend_from_slice(b"WAVEfmt ");
    data.extend_from_slice(&16u32.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes()); // PCM
    data.extend_from_slice(&1u16.to_le_bytes()); // mono
    data.extend_from_slice(&rate.to_le_bytes());
    data.extend_from_slice(&(rate * 2).to_le_bytes());
    data.extend_from_slice(&2u16.to_le_bytes());
    data.extend_from_slice(&16u16.to_le_bytes());
    data.extend_from_slice(b"data");
    data.extend_from_slice(&(frames as u32 * 2).to_le_bytes());
    for i in 0..frames {
        let s = (i as f64 * 0.2).sin() * 0.5;
        data.extend_from_slice(&((s * 32767.0) as i16).to_le_bytes());
    }
    std::fs::write(&path, data).unwrap();
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

/// Content-keyed fake chain (same discipline as the resume test):
/// plans by photo URI, objects by (photo, concept) with per-object
/// mask bboxes, draw counted so the test can prove keep items
/// never reach it.
struct Fake {
    store: AssetStore,
    plans_by_uri: HashMap<String, Value>,
    contents: HashMap<(usize, String), ([i64; 4], String)>,
    photo_handles: RefCell<HashMap<String, usize>>,
    spend: Cell<f64>,
    plan_identify_calls: Cell<usize>,
    crop_identify_calls: Cell<usize>,
    cut_out_calls: Cell<usize>,
    draw_calls: Cell<usize>,
}

impl Fake {
    fn identify(&self, image_ref: &str) -> Result<Value, String> {
        if !self.plans_by_uri.contains_key(image_ref) {
            // A crop identify (staging's second route, or the keep
            // path's condition call): one generic plan.
            self.crop_identify_calls
                .set(self.crop_identify_calls.get() + 1);
            self.spend.set(self.spend.get() + IDENTIFY_COST);
            return Ok(json!({
                "status": "ok",
                "image": image_ref,
                "suggested_title": "Listing: cropped item",
                "listing_bullets": ["one cropped item"],
                "condition_notes": CROP_CONDITION,
                "cost_usd": IDENTIFY_COST,
            }));
        }
        self.plan_identify_calls
            .set(self.plan_identify_calls.get() + 1);
        self.spend.set(self.spend.get() + IDENTIFY_COST);
        let plan = self.plans_by_uri.get(image_ref).cloned().unwrap();
        let handle = agenthand::ingest::ingest(image_ref, &self.store)?;
        let idx = plan["photo_index"].as_u64().unwrap() as usize;
        self.photo_handles.borrow_mut().insert(handle.clone(), idx);
        let mut result = json!({"status": "ok", "image": handle, "cost_usd": IDENTIFY_COST});
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
        self.cut_out_calls.set(self.cut_out_calls.get() + 1);
        self.spend.set(self.spend.get() + CUT_COST);
        let Some((bbox, _object)) = self.contents.get(&(idx, concept.to_string())).cloned() else {
            return Ok(json!({
                "status": "error",
                "error": {"code": "NO_MATCH", "message": format!("no {concept} in photo {idx}"), "cost_counted_usd": CUT_COST},
            }));
        };
        let fg = RgbaImage::solid(12, 12, [200, 30, 30, 255]);
        let fg_handle = self
            .store
            .put_image(&fg, agenthand::assets::KIND_FOREGROUND, None)?;
        Ok(json!({
            "status": "ok",
            "output_asset": fg_handle,
            "mask_asset": Value::Null,
            "mask_overlay_asset": Value::Null,
            "mask_review": {"bbox": bbox, "warnings": []},
            "cost_usd": CUT_COST,
        }))
    }

    fn draw(&self, fg_handle: &str, _scene: &str) -> Result<Value, String> {
        self.draw_calls.set(self.draw_calls.get() + 1);
        self.spend.set(self.spend.get() + DRAW_COST);
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
        Ok(
            json!({"status": "ok", "output_asset": composite, "fidelity_receipt": receipt, "cost_usd": DRAW_COST}),
        )
    }

    fn chain(self: &Rc<Self>) -> RoomChain {
        let a = self.clone();
        let b = self.clone();
        let c = self.clone();
        RoomChain {
            identify: Box::new(move |r: &str| a.identify(r)),
            cut_out: Box::new(move |h: &str, concept: &str| b.cut_out(h, concept)),
            draw: Box::new(move |h: &str, s: &str| c.draw(h, s)),
        }
    }
}

fn verify_ok() -> VerifyFn {
    Box::new(|_store: &AssetStore, _handle: &str| {
        Ok(json!({
            "ok": true,
            "checks": {"zero_changed_pixels": true},
            "problems": [],
            "recomputed": {"changed_opaque_pixels": 0, "opaque_pixels": 100, "color_profile_preserved": true},
        }))
    })
}

/// Fix pass F4: narration is billed through the CUMULATIVE ledger in
/// the data dir. Point the data dir at a per-process scratch folder
/// unless it already points into the temp dir, so no test row ever
/// reaches a real ledger (including one named by an exported
/// AGENTHAND_DATA_DIR).
fn scratch_data_dir() {
    let tmp = std::env::temp_dir();
    let already_scratch = std::env::var_os("AGENTHAND_DATA_DIR")
        .map(|v| std::path::PathBuf::from(v).starts_with(&tmp))
        .unwrap_or(false);
    if !already_scratch {
        std::env::set_var(
            "AGENTHAND_DATA_DIR",
            tmp.join(format!("agenthand-test-data-{}", std::process::id())),
        );
    }
}

#[test]
fn narrated_run_with_sell_keep_mentioned_and_no_match() {
    scratch_data_dir();
    let root = temp_root("room");
    let photos = vec![
        photo(&root, "bike.png", [200, 60, 60, 255], false),
        photo(&root, "lamp.png", [60, 200, 120, 255], true),
        photo(&root, "rug.png", [90, 90, 200, 255], false),
    ];
    let wav = narration_wav(&root);

    let mut plans_by_uri = HashMap::new();
    for (i, concept) in ["red bike", "brass lamp", "old rug"].iter().enumerate() {
        let mut p = plan(concept);
        p["photo_index"] = json!(i);
        plans_by_uri.insert(data_uri_for(&photos[i]), p);
    }
    // (photo, concept) -> (mask bbox px, object id). The desk's box
    // touches the photo's top edge: a RESHOOT-grade cut.
    let contents: HashMap<(usize, String), ([i64; 4], String)> = [
        ((0usize, "red bike"), ([4, 4, 30, 30], "bike1".to_string())),
        (
            (1usize, "brass lamp"),
            ([4, 4, 30, 30], "lamp1".to_string()),
        ),
        ((2usize, "old rug"), ([4, 4, 30, 30], "rug1".to_string())),
        ((2usize, "oak desk"), ([4, 0, 30, 30], "desk1".to_string())),
    ]
    .into_iter()
    .map(|((i, c), v)| ((i, c.to_string()), v))
    .collect();
    let fake = Rc::new(Fake {
        store: AssetStore::new(&root.join("store")),
        plans_by_uri,
        contents,
        photo_handles: RefCell::new(HashMap::new()),
        spend: Cell::new(0.0),
        plan_identify_calls: Cell::new(0),
        crop_identify_calls: Cell::new(0),
        cut_out_calls: Cell::new(0),
        draw_calls: Cell::new(0),
    });
    let reply = "TRANSCRIPT\nThe red bike is for sale. The brass lamp stays, keep it for insurance, and the oak desk stays too. There is an old rug and a grand piano somewhere.\nSELLING\nred bike\nKEEP\nbrass lamp\noak desk\nMENTIONED\nold rug\ngrand piano";
    let narration_fn = move |_path: &Path| -> Result<Value, String> {
        Ok(json!({"reply": reply, "usage": {"total_tokens": 10}, "latency_ms": 5}))
    };
    let out = root.join("out");
    let manifest = run_room(
        &photos,
        &[],
        &out,
        RoomRunOptions {
            room_id: Some("keep-room".to_string()),
            budget_usd: 5.00,
            probe_all_candidates: false,
            narrated_audios: vec![wav],
            narration_fn: Some(Box::new(narration_fn)),
            store: Some(AssetStore::new(&root.join("store"))),
            chain: Some(fake.chain()),
            verify_fn: Some(verify_ok()),
            ..RoomRunOptions::default()
        },
    )
    .unwrap();

    let items = manifest["items"].as_array().unwrap();
    let by_name = |name: &str| items.iter().find(|i| i["name"] == name).unwrap();
    // Intents across the whole vocabulary.
    assert_eq!(by_name("red bike")["intent"], "selling");
    assert_eq!(by_name("brass lamp")["intent"], "keep");
    assert_eq!(by_name("oak desk")["intent"], "keep");
    assert_eq!(by_name("old rug")["intent"], "mentioned");
    assert_eq!(by_name("grand piano")["intent"], "mentioned");
    // Outcomes.
    assert_eq!(by_name("red bike")["status"], "staged");
    assert_eq!(by_name("old rug")["status"], "staged");
    assert_eq!(by_name("brass lamp")["status"], "kept");
    assert_eq!(by_name("oak desk")["status"], "kept");
    let piano = by_name("grand piano");
    assert_eq!(piano["status"], "failed");
    assert_eq!(piano["failure"]["code"], "NO_MATCH");

    // Keep items never reach draw; exactly the two staged items do.
    assert_eq!(fake.draw_calls.get(), 2);
    // Keep items carry no listing.
    assert!(by_name("brass lamp")["listing"].is_null());

    // The lamp's ownership record binds the file, its stated
    // capture time, and the crop.
    let lamp = by_name("brass lamp");
    let own = &lamp["ownership"];
    assert_eq!(own["receipt_type"], "ownership");
    assert_eq!(
        own["source"]["file_sha256"],
        sha256_hex(&std::fs::read(&photos[1]).unwrap())
    );
    assert_eq!(own["source"]["exif_capture_time"], EXIF_DT);
    assert_eq!(own["source"]["file_name"], "lamp.png");
    assert_eq!(own["condition_notes"], CROP_CONDITION);
    assert_eq!(own["cut_quality"]["grade"], "PASS");
    let rect = own["crop"]["rect_px"].as_array().unwrap();
    assert_eq!(rect.len(), 4);
    assert!(rect[2].as_u64().unwrap() <= 48 && rect[3].as_u64().unwrap() <= 36);
    // Its verification recomputed the crop: 0 changed.
    assert_eq!(lamp["verification"]["ok"], json!(true));
    assert_eq!(lamp["verification"]["crop_pixels_changed"], 0);
    assert!(
        lamp["verification"]["crop_pixels_checked"]
            .as_u64()
            .unwrap()
            > 0
    );
    // And its cost is cut + one crop identify — no draw money.
    assert!((lamp["cost_usd"].as_f64().unwrap() - (CUT_COST + IDENTIFY_COST)).abs() < 1e-9);

    // The desk's RESHOOT grade rides in its record without
    // blocking it (decision 114).
    let desk = by_name("oak desk");
    assert_eq!(desk["ownership"]["cut_quality"]["grade"], "RESHOOT");
    let instruction = desk["ownership"]["cut_quality"]["reshoot_instruction"]
        .as_str()
        .unwrap();
    assert!(
        instruction.contains("top"),
        "instruction names the edge: {instruction}"
    );
    assert_eq!(desk["verification"]["crop_pixels_changed"], 0);

    // Every staged receipt verifies 0 changed (fake verifier's
    // recomputation), and the step log kept the two workflows
    // distinct: keep items own an "ownership" step, never a draw.
    for staged in ["red bike", "old rug"] {
        assert_eq!(by_name(staged)["verification"]["ok"], json!(true));
        assert_eq!(by_name(staged)["verification"]["changed_opaque_pixels"], 0);
    }
    let log = RunStepLog::open(&out.join("room-steps.sqlite3")).unwrap();
    let lamp_index = items
        .iter()
        .position(|i| i["name"] == "brass lamp")
        .unwrap() as i64;
    let row = log
        .get("keep-room", lamp_index, "ownership")
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "complete");
    assert!(log.get("keep-room", lamp_index, "draw").unwrap().is_none());

    // The inventory collects exactly the kept items, in both forms.
    assert_eq!(manifest["inventory"]["items"], 2);
    let inventory: Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("inventory.json")).unwrap())
            .unwrap();
    let names: Vec<&str> = inventory["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"brass lamp") && names.contains(&"oak desk"));
    let lamp_entry = inventory["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "brass lamp")
        .unwrap();
    assert_eq!(lamp_entry["exif_capture_time"], EXIF_DT);
    assert_eq!(lamp_entry["condition_notes"], CROP_CONDITION);
    assert_eq!(lamp_entry["receipt_sha256"].as_str().unwrap().len(), 64);
    let sheet = std::fs::read_to_string(out.join("inventory.html")).unwrap();
    assert!(sheet.contains("brass lamp") && sheet.contains("oak desk"));
    assert!(sheet.contains("data:image/png;base64,"));
    assert!(sheet.contains(lamp_entry["receipt_sha256"].as_str().unwrap()));
}

/// GPS TIFF (little-endian): IFD0 {Make, Model, Software (a bare
/// OS version), GPS ptr}, GPS IFD {45 30 0 N, 122 45 0 W} — the
/// iPhone shape, for the provenance assertions below.
fn gps_tiff() -> Vec<u8> {
    fn entry(out: &mut Vec<u8>, tag: u16, ty: u16, count: u32, value: u32) {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    }
    let make = b"Apple\0";
    let model = b"iPhone17,2\0";
    let sw = b"26.6.2\0";
    let data_at = 116u32;
    let make_off = data_at;
    let model_off = make_off + make.len() as u32;
    let sw_off = model_off + model.len() as u32;
    let lat_off = sw_off + sw.len() as u32;
    let lon_off = lat_off + 24;
    let mut t: Vec<u8> = Vec::new();
    t.extend_from_slice(b"II*\0");
    t.extend_from_slice(&8u32.to_le_bytes());
    t.extend_from_slice(&4u16.to_le_bytes());
    entry(&mut t, 0x010F, 2, make.len() as u32, make_off);
    entry(&mut t, 0x0110, 2, model.len() as u32, model_off);
    entry(&mut t, 0x0131, 2, sw.len() as u32, sw_off);
    entry(&mut t, 0x8825, 4, 1, 62);
    t.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(t.len(), 62);
    t.extend_from_slice(&4u16.to_le_bytes());
    let mut v = [0u8; 4];
    v[0] = b'N';
    entry(&mut t, 0x0001, 2, 2, u32::from_le_bytes(v));
    entry(&mut t, 0x0002, 5, 3, lat_off);
    let mut v = [0u8; 4];
    v[0] = b'W';
    entry(&mut t, 0x0003, 2, 2, u32::from_le_bytes(v));
    entry(&mut t, 0x0004, 5, 3, lon_off);
    t.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(t.len(), 116);
    t.extend_from_slice(make);
    t.extend_from_slice(model);
    t.extend_from_slice(sw);
    for (num, den) in [(45u32, 1u32), (30, 1), (0, 1), (122, 1), (45, 1), (0, 1)] {
        t.extend_from_slice(&num.to_le_bytes());
        t.extend_from_slice(&den.to_le_bytes());
    }
    t
}

fn gps_photo(dir: &Path, name: &str, color: [u8; 4]) -> PathBuf {
    let path = dir.join(name);
    let img = RgbaImage::solid(48, 36, color);
    let png = encode_png_rgba(&img);
    let chunk = agenthand::image::png_chunk(b"eXIf", &gps_tiff());
    let mut out = png[..33].to_vec();
    out.extend_from_slice(&chunk);
    out.extend_from_slice(&png[33..]);
    std::fs::write(&path, out).unwrap();
    path
}

/// Bonus package on the KEEP surface: a kept item whose photo
/// file carries GPS gets the provenance block on its ownership
/// record, the location warning in the inventory (JSON + HTML),
/// and a metadata-free public copy parented to the source —
/// while the record still verifies 0 changed.
#[test]
fn keep_run_with_gps_photo_carries_provenance_and_mints_public_copy() {
    scratch_data_dir();
    let root = temp_root("gps");
    let photos = vec![gps_photo(&root, "garage.png", [60, 200, 120, 255])];
    let wav = narration_wav(&root);

    let mut plans_by_uri = HashMap::new();
    let mut p = plan("red bike");
    p["photo_index"] = json!(0);
    plans_by_uri.insert(data_uri_for(&photos[0]), p);
    let contents: HashMap<(usize, String), ([i64; 4], String)> = [
        ((0usize, "red bike"), ([4, 4, 30, 30], "bike1".to_string())),
        (
            (0usize, "brass lamp"),
            ([4, 4, 30, 30], "lamp1".to_string()),
        ),
        ((0usize, "old rug"), ([4, 4, 30, 30], "rug1".to_string())),
    ]
    .into_iter()
    .map(|((i, c), v)| ((i, c.to_string()), v))
    .collect();
    let fake = Rc::new(Fake {
        store: AssetStore::new(&root.join("store")),
        plans_by_uri,
        contents,
        photo_handles: RefCell::new(HashMap::new()),
        spend: Cell::new(0.0),
        plan_identify_calls: Cell::new(0),
        crop_identify_calls: Cell::new(0),
        cut_out_calls: Cell::new(0),
        draw_calls: Cell::new(0),
    });
    let reply = "TRANSCRIPT\nThe red bike is for sale. The brass lamp stays, keep it for insurance. There is an old rug too.\nSELLING\nred bike\nKEEP\nbrass lamp\nMENTIONED\nold rug";
    let narration_fn = move |_path: &Path| -> Result<Value, String> {
        Ok(json!({"reply": reply, "usage": {"total_tokens": 10}, "latency_ms": 5}))
    };
    let out = root.join("out");
    let store = AssetStore::new(&root.join("store"));
    let manifest = run_room(
        &photos,
        &[],
        &out,
        RoomRunOptions {
            room_id: Some("keep-gps".to_string()),
            budget_usd: 5.00,
            probe_all_candidates: false,
            narrated_audios: vec![wav],
            narration_fn: Some(Box::new(narration_fn)),
            store: Some(AssetStore::new(&root.join("store"))),
            chain: Some(fake.chain()),
            verify_fn: Some(verify_ok()),
            ..RoomRunOptions::default()
        },
    )
    .unwrap();

    let items = manifest["items"].as_array().unwrap();
    let lamp = items.iter().find(|i| i["name"] == "brass lamp").unwrap();
    assert_eq!(lamp["status"], "kept");
    assert_eq!(lamp["verification"]["ok"], json!(true));

    let prov = &lamp["ownership"]["provenance"];
    assert_eq!(prov["format"], "png");
    assert_eq!(prov["location"]["present"], json!(true));
    assert_eq!(prov["location"]["latitude"], 45.5);
    assert_eq!(prov["location"]["longitude"], -122.75);
    let warning = prov["location_warning"].as_str().unwrap();
    assert!(warning.contains("GPS location coordinates"), "{warning}");
    let device = prov["signals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["signal"] == "capture_device")
        .unwrap();
    assert_eq!(device["finding"], "stated");
    assert_eq!(device["capture_software_version"], "26.6.2");
    let editor = prov["signals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["signal"] == "editor_software")
        .unwrap();
    assert_eq!(editor["finding"], "none_named");

    // The public copy: right kind, parented to the source asset,
    // location-free, pixel-identical to the photo.
    let copy_info = &prov["location"]["public_copy"];
    assert_eq!(
        copy_info["label"].as_str().unwrap(),
        agenthand::provenance::PUBLIC_COPY_LABEL
    );
    let copy_handle = copy_info["asset"].as_str().unwrap();
    let copy_record = store
        .load_record(&AssetStore::parse_handle(copy_handle).unwrap())
        .unwrap();
    assert_eq!(copy_record.kind, agenthand::assets::KIND_PUBLIC_COPY);
    let source_asset = lamp["ownership"]["source"]["asset"].as_str().unwrap();
    assert_eq!(
        copy_record.source_asset_id.as_deref(),
        Some(AssetStore::parse_handle(source_asset).unwrap().as_str())
    );
    let copy_bytes = store.read_bytes(copy_handle).unwrap();
    let copy_block = agenthand::provenance::provenance_block(&copy_bytes, Some((48, 36)));
    assert_eq!(copy_block["location"]["present"], json!(false));
    let photo_img = agenthand::image::decode_image(&std::fs::read(&photos[0]).unwrap()).unwrap();
    let photo_rgba = match photo_img {
        agenthand::image::DecodedImage::Rgba(i) => i,
        agenthand::image::DecodedImage::Gray(g) => g.to_rgba(),
    };
    assert_eq!(copy_info["pixel_sha256"], photo_rgba.pixel_sha256());

    // The inventory carries the warning in both forms.
    let inventory: Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("inventory.json")).unwrap())
            .unwrap();
    let entry = inventory["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "brass lamp")
        .unwrap();
    assert_eq!(entry["location_warning"].as_str().unwrap(), warning);
    assert_eq!(entry["public_copy"]["asset"], copy_info["asset"]);
    let sheet = std::fs::read_to_string(out.join("inventory.html")).unwrap();
    assert!(
        sheet.contains("Location warning:"),
        "sheet shows the warning"
    );
}
