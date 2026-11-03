//! Ground match (docs/GROUND-MATCH.md): the source photo's own ground,
//! sampled and named, steers the generated plate's ground. Product
//! pixels (alpha 255) stay byte-exact.

use std::sync::Mutex;

use agenthand::assets::{AssetStore, KIND_FOREGROUND, KIND_PLATE, KIND_SOURCE};
use agenthand::composite::{self, alpha_composite};
use agenthand::ground::GroundLight;
use agenthand::ground_match::{
    ground_descriptor, plan_for_draw, GroundMatchPlan, GROUND_MATCH_ENV,
};
use agenthand::image::RgbaImage;
use agenthand::{meta, staging};
use serde_json::json;

static ENV_LOCK: Mutex<()> = Mutex::new(());

const W: u32 = 600;
const H: u32 = 450;
/// Wall above y = 200, ground below.
const HORIZON: u32 = 200;

fn noise(x: u32, y: u32) -> i32 {
    let mut h = x.wrapping_mul(374_761_393) ^ y.wrapping_mul(668_265_263);
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    (h >> 16) as i32 & 0xff
}

/// A photo: red-brick wall above the horizon, `ground` below with
/// +-`grain` per-pixel noise.
fn photo(ground: [i32; 3], grain: i32) -> RgbaImage {
    let mut img = RgbaImage::solid(W, H, [0, 0, 0, 255]);
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            let px = if y < HORIZON {
                [150, 62, 50]
            } else {
                let n = (noise(x, y) - 128) * grain / 128;
                [ground[0] + n, ground[1] + n, ground[2] + n]
            };
            for c in 0..3 {
                img.pixels[i + c] = px[c].clamp(0, 255) as u8;
            }
        }
    }
    img
}

/// A product layer cut from `source`: an open ring (a wheel) on the
/// ground plus a solid box above it, ground line at y = 380.
fn product(source: &RgbaImage) -> RgbaImage {
    let mut fg = source.clone();
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            let d = ((x as f64 - 300.0).powi(2) + (y as f64 - 320.0).powi(2)).sqrt();
            let ring = (52.0..=60.0).contains(&d);
            let frame = (240..360).contains(&x) && (150..200).contains(&y);
            let post = (296..304).contains(&x) && (200..262).contains(&y);
            if ring || frame || post {
                fg.pixels[i..i + 3].copy_from_slice(&[30, 70, 200]);
                fg.pixels[i + 3] = 255;
            } else {
                fg.pixels[i + 3] = 0;
            }
        }
    }
    fg
}

/// The photo with the product in it, and the product layer cut from it
/// (product pixels equal the photo's).
fn shot(ground: [i32; 3], grain: i32) -> (RgbaImage, RgbaImage) {
    let base = photo(ground, grain);
    let fg = product(&base);
    (alpha_composite(&base, &fg), fg)
}

#[test]
fn synthetic_grounds_are_named_by_surface_value_and_tone() {
    // Dark asphalt: cool, dark, grainy.
    let (src, fg) = shot([44, 47, 52], 18);
    let d = ground_descriptor(&src, &fg).unwrap();
    eprintln!("asphalt: {}", d.to_json());
    assert!(d.surface.contains("asphalt"), "{}", d.surface);
    assert!(d.surface.starts_with("weathered"), "{}", d.surface);
    assert_eq!(d.value_word, "dark");
    assert!(d.temperature.starts_with("cool"), "{}", d.temperature);
    // Ground seen inside the open ring counts as a remnant.
    assert!(d.remnant_cells > 0 && d.around_cells > d.remnant_cells);

    // Pale limestone: warm, pale, smooth.
    let (src, fg) = shot([224, 214, 194], 4);
    let d = ground_descriptor(&src, &fg).unwrap();
    eprintln!("limestone: {}", d.to_json());
    assert_eq!(d.surface, "smooth pale limestone paving");
    assert_eq!(d.value_word, "pale");
    assert!(d.temperature.starts_with("warm"), "{}", d.temperature);

    // Lawn: green.
    let (src, fg) = shot([72, 128, 52], 22);
    let d = ground_descriptor(&src, &fg).unwrap();
    eprintln!("lawn: {}", d.to_json());
    assert_eq!(d.surface, "green lawn grass");
    assert!(d.stats.green_fraction > 0.9);

    // Mid grey concrete, neutral.
    let (src, fg) = shot([128, 128, 128], 6);
    let d = ground_descriptor(&src, &fg).unwrap();
    assert!(d.surface.contains("grey concrete paving"), "{}", d.surface);
    assert_eq!(d.value_word, "mid");
    assert_eq!(d.temperature, "neutral");

    // The wall above the ground line is never sampled: a pale ground
    // under a red wall still reads pale and grey, not brick.
    assert!(!d.surface.contains("brick"));
    // Deterministic.
    assert_eq!(ground_descriptor(&src, &fg).unwrap(), d);
}

#[test]
fn clause_rides_the_generated_prompt_only_and_the_kill_switch_drops_it() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var(GROUND_MATCH_ENV).ok();
    let (src, fg) = shot([44, 47, 52], 18);
    let angle = staging::estimate_shot_angle(&staging::analyze_subject(&fg));

    std::env::remove_var(GROUND_MATCH_ENV);
    let plan = plan_for_draw(Some(&src), &fg);
    let clause = plan.clause().expect("a clause when on");
    assert!(clause.contains("dark asphalt") && clause.contains("dark value"), "{clause}");
    let scene = "a stone courtyard with planters";
    let generated = meta::generated_plate_scene_matched(scene, &angle, None, Some(&clause), true, &[]);
    assert!(generated.starts_with(scene), "the caller's setting leads: {generated}");
    assert!(generated.contains(&clause));
    assert_eq!(
        meta::generated_plate_scene_matched(scene, &angle, None, None, true, &[]),
        meta::generated_plate_scene_dressed(scene, &angle, None, true, &[]),
    );
    // source_edit prompts are built without it.
    let edit = meta::plate_edit_prompt_locked(scene, false, Some(&angle));
    assert!(!edit.contains("ground surface matches"), "{edit}");
    assert!(plan.to_json()["stated"] == json!(true));

    for off in ["off", "0", "false", "no"] {
        std::env::set_var(GROUND_MATCH_ENV, off);
        let plan = plan_for_draw(Some(&src), &fg);
        assert!(plan.clause().is_none());
        assert_eq!(plan.reason.as_deref(), Some("disabled by AGENTHAND_GROUND_MATCH"));
        assert_eq!(plan.to_json()["stated"], json!(false));
    }
    match previous {
        Some(v) => std::env::set_var(GROUND_MATCH_ENV, v),
        None => std::env::remove_var(GROUND_MATCH_ENV),
    }
    // No photo, a surface: no descriptor, the reason says why.
    let none = plan_for_draw(None, &fg);
    assert!(none.clause().is_none() && none.reason.is_some());
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("agenthand-match-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Through the store: the generated draw records `ground_match`
/// (sample, fitted plate, delta), keeps every alpha-255 pixel and
/// verifies; a disabled plan records why; the legacy path has no key.
#[test]
fn receipt_records_the_match_and_product_pixels_stay_exact() {
    let dir = tmpdir("draw");
    let store = AssetStore::new(&dir);
    let (src, fg) = shot([44, 47, 52], 18);
    let source_handle = store.put_image(&src, KIND_SOURCE, None).unwrap();
    let parent = AssetStore::parse_handle(&source_handle).unwrap();
    let fg_handle = store.put_image(&fg, KIND_FOREGROUND, Some(&parent)).unwrap();
    // A generated plate with a pale ground, at another size.
    let plate = {
        let p = photo([210, 205, 196], 4);
        let buf = image::RgbaImage::from_raw(W, H, p.pixels).unwrap();
        let big = image::imageops::resize(&buf, 900, 675, image::imageops::FilterType::Triangle);
        RgbaImage::new(900, 675, big.into_raw())
    };
    let plate_handle = store.put_image(&plate, KIND_PLATE, None).unwrap();
    let light = GroundLight {
        side: None,
        method: "none".to_string(),
    };
    let plan = GroundMatchPlan {
        descriptor: Some(ground_descriptor(&src, &fg).unwrap()),
        reason: None,
    };
    let (handle, receipt) = composite::draw_product_generated(
        &store,
        &fg_handle,
        "courtyard",
        &plate_handle,
        &light,
        Some(&plan),
    )
    .unwrap();
    let m = &receipt["ground_match"];
    eprintln!("{m}");
    assert_eq!(m["applied"], json!(true));
    assert!(m["sampled"]["surface"].as_str().unwrap().contains("asphalt"));
    assert_eq!(m["plate"]["value_word"], json!("pale"));
    assert!(m["delta"]["value"].as_f64().unwrap() > 0.5, "{m}");
    assert_eq!(m["delta"]["same_value_word"], json!(false));
    assert_eq!(receipt["changed_opaque_pixels"], json!(0));
    let delivered = store.read_image(&handle).unwrap();
    for i in (0..fg.pixels.len()).step_by(4) {
        if fg.pixels[i + 3] == 255 {
            assert_eq!(&delivered.pixels[i..i + 4], &fg.pixels[i..i + 4]);
        }
    }
    let report = composite::verify_receipt_from_disk(&store, &handle, Some(&receipt)).unwrap();
    assert_eq!(report["ok"], json!(true), "{}", report["problems"]);

    // The match never changes pixels: the same draw without a plan
    // gives the same composite.
    let (plain_handle, plain) =
        composite::draw_product_grounded(&store, &fg_handle, "courtyard", &plate_handle, &light).unwrap();
    assert!(plain.get("ground_match").is_none());
    assert_eq!(
        store.read_image(&plain_handle).unwrap().pixels,
        delivered.pixels
    );
    // A plan without a descriptor records why.
    let off = GroundMatchPlan {
        descriptor: None,
        reason: Some("disabled by AGENTHAND_GROUND_MATCH".to_string()),
    };
    let (_, r) = composite::draw_product_generated(
        &store, &fg_handle, "courtyard", &plate_handle, &light, Some(&off),
    )
    .unwrap();
    assert_eq!(r["ground_match"]["applied"], json!(false));
    assert_eq!(r["ground_match"]["reason"], json!("disabled by AGENTHAND_GROUND_MATCH"));
    // Legacy path: no record.
    let (_, legacy) =
        composite::draw_product_with_plate_kind(&store, &fg_handle, "x", Some(&plate_handle), None).unwrap();
    assert!(legacy.get("ground_match").is_none());
    let _ = std::fs::remove_dir_all(dir);
}

fn load(path: &std::path::Path) -> RgbaImage {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    agenthand::image::decode_image(&bytes).unwrap().into_rgba()
}

/// Opt-in: the descriptor on a real run (source.png plus mask.png, or
/// foreground.png when present), and the check against its plate.png.
#[test]
#[ignore]
fn real_fixture_ground_descriptor() {
    let Ok(dir) = std::env::var("AGENTHAND_MATCH_ARTIFACTS") else {
        eprintln!("AGENTHAND_MATCH_ARTIFACTS not set; skipping");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let source = load(&dir.join("source.png"));
    let fg = if std::env::var("AGENTHAND_MATCH_USE_MASK").is_ok() || !dir.join("foreground.png").exists() {
        let bytes = std::fs::read(dir.join("mask.png")).unwrap();
        let mask = agenthand::image::decode_image(&bytes).unwrap().into_rgba();
        let mut fg = source.clone();
        for i in (0..fg.pixels.len()).step_by(4) {
            fg.pixels[i + 3] = mask.pixels[i];
        }
        fg
    } else {
        load(&dir.join("foreground.png"))
    };
    let t = std::time::Instant::now();
    let d = ground_descriptor(&source, &fg).unwrap();
    eprintln!("descriptor in {:.2}s", t.elapsed().as_secs_f64());
    eprintln!("{}", serde_json::to_string_pretty(&d.to_json()).unwrap());
    eprintln!("clause: {}", d.clause());
    if dir.join("plate.png").exists() {
        let plate = load(&dir.join("plate.png"));
        let plan = GroundMatchPlan {
            descriptor: Some(d.clone()),
            reason: None,
        };
        eprintln!("check vs plate.png: {}", plan.record(&plate, &fg)["delta"]);
        eprintln!("plate: {}", plan.record(&plate, &fg)["plate"]);
    }
    if let Ok(out) = std::env::var("AGENTHAND_MATCH_OUT") {
        std::fs::write(out, serde_json::to_string_pretty(&d.to_json()).unwrap()).unwrap();
    }
}
