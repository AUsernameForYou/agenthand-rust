//! Ground shadow on generated plates (docs/GROUND-SHADOW.md): a contact
//! and cast shadow at the product's ground line, plate pixels only.
//! Every fully opaque product pixel (alpha 255) stays byte-exact.

use std::sync::Mutex;

use agenthand::assets::{AssetStore, KIND_FOREGROUND, KIND_PLATE, KIND_SOURCE};
use agenthand::composite::{self, alpha_composite, measure};
use agenthand::ground::{ground_shadow, GroundLight, GROUND_SHADOW_ENV};
use agenthand::image::RgbaImage;
use agenthand::staging::LightSide;
use serde_json::json;

static ENV_LOCK: Mutex<()> = Mutex::new(());

const W: u32 = 400;
const H: u32 = 300;

/// A textured ground plate (deterministic grain), mid-grey pavers.
fn ground_plate(w: u32, h: u32) -> RgbaImage {
    let mut img = RgbaImage::solid(w, h, [0, 0, 0, 255]);
    for y in 0..h {
        for x in 0..w {
            let grain = ((x * 7 + y * 13) % 11) as i32 - 5;
            let joint = if x % 40 == 0 || y % 30 == 0 { -25 } else { 0 };
            let v = (150 + grain + joint).clamp(0, 255) as u8;
            let i = ((y * w + x) * 4) as usize;
            img.pixels[i..i + 4].copy_from_slice(&[
                v,
                v.saturating_sub(6),
                v.saturating_sub(14),
                255,
            ]);
        }
    }
    img
}

fn put(fg: &mut RgbaImage, x: i64, y: i64, rgba: [u8; 4]) {
    if x < 0 || y < 0 || x >= fg.width as i64 || y >= fg.height as i64 {
        return;
    }
    let i = ((y as u32 * fg.width + x as u32) * 4) as usize;
    fg.pixels[i..i + 4].copy_from_slice(&rgba);
}

fn line(fg: &mut RgbaImage, a: (f64, f64), b: (f64, f64), half: f64, rgba: [u8; 4]) {
    let n = ((b.0 - a.0).hypot(b.1 - a.1).ceil() as usize).max(1) * 2;
    for k in 0..=n {
        let t = k as f64 / n as f64;
        let (cx, cy) = (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
        let r = half.ceil() as i64;
        for dy in -r..=r {
            for dx in -r..=r {
                if ((dx * dx + dy * dy) as f64).sqrt() <= half {
                    put(fg, cx.round() as i64 + dx, cy.round() as i64 + dy, rgba);
                }
            }
        }
    }
}

const TYRE: [u8; 4] = [30, 30, 32, 255];
const FRAME: [u8; 4] = [30, 70, 200, 255];
const SPOKE: [u8; 4] = [190, 190, 195, 255];
const WHEELS: [(f64, f64); 2] = [(130.0, 220.0), (290.0, 220.0)];
const R: f64 = 50.0;

/// A side-on bike: two open wheels (tyre ring, 1 px spokes), a frame
/// of 3 px tubes, a soft (alpha 128) rim on the tyres' outer edge.
fn bike() -> RgbaImage {
    let mut fg = RgbaImage::solid(W, H, [0, 0, 0, 0]);
    for &(cx, cy) in &WHEELS {
        for y in 0..H as i64 {
            for x in 0..W as i64 {
                let d = ((x as f64 - cx).powi(2) + (y as f64 - cy).powi(2)).sqrt();
                if (R - 7.0..=R).contains(&d) {
                    put(&mut fg, x, y, TYRE);
                } else if (R..R + 1.0).contains(&d) {
                    put(&mut fg, x, y, [30, 30, 32, 128]);
                }
            }
        }
        for k in 0..12 {
            let a = k as f64 * std::f64::consts::PI / 6.0;
            line(
                &mut fg,
                (cx, cy),
                (cx + (R - 6.0) * a.cos(), cy + (R - 6.0) * a.sin()),
                0.5,
                SPOKE,
            );
        }
    }
    let (rear, front) = (WHEELS[0], WHEELS[1]);
    let seat = (180.0, 140.0);
    let head = (270.0, 145.0);
    let crank = (200.0, 222.0);
    for (a, b) in [
        (rear, seat),
        (seat, crank),
        (crank, rear),
        (seat, head),
        (head, crank),
        (head, front),
    ] {
        line(&mut fg, a, b, 3.0, FRAME);
    }
    line(
        &mut fg,
        (170.0, 130.0),
        (195.0, 130.0),
        4.0,
        [20, 20, 20, 255],
    );
    fg
}

fn luma(px: &[u8]) -> f64 {
    0.299 * px[0] as f64 + 0.587 * px[1] as f64 + 0.114 * px[2] as f64
}

/// Mean darkening (1 - after/before luma) over plate pixels in a box
/// that the product does not cover at all.
fn darkening(
    before: &RgbaImage,
    after: &RgbaImage,
    fg: &RgbaImage,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
) -> f64 {
    let (mut sb, mut sa) = (0.0, 0.0);
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * before.width + x) * 4) as usize;
            if fg.pixels[i + 3] != 0 {
                continue;
            }
            sb += luma(&before.pixels[i..i + 4]);
            sa += luma(&after.pixels[i..i + 4]);
        }
    }
    if sb == 0.0 {
        0.0
    } else {
        1.0 - sa / sb
    }
}

/// Strongest darkening over plate pixels in a box the product does
/// not cover.
fn peak_darkening(
    before: &RgbaImage,
    after: &RgbaImage,
    fg: &RgbaImage,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
) -> f64 {
    let mut peak: f64 = 0.0;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * before.width + x) * 4) as usize;
            if fg.pixels[i + 3] != 0 {
                continue;
            }
            peak = peak.max(1.0 - luma(&after.pixels[i..i + 4]) / luma(&before.pixels[i..i + 4]));
        }
    }
    peak
}

fn front_lit() -> GroundLight {
    GroundLight {
        side: None,
        method: "none".to_string(),
    }
}

fn assert_product_byte_exact(fg: &RgbaImage, plate: &RgbaImage, grounded: &RgbaImage) {
    let composite = alpha_composite(grounded, fg);
    let mut opaque = 0;
    for i in (0..fg.pixels.len()).step_by(4) {
        if fg.pixels[i + 3] == 255 {
            opaque += 1;
            assert_eq!(
                &composite.pixels[i..i + 4],
                &fg.pixels[i..i + 4],
                "product pixel {}",
                i / 4
            );
            assert_eq!(
                &grounded.pixels[i..i + 4],
                &plate.pixels[i..i + 4],
                "plate under product {}",
                i / 4
            );
        }
    }
    assert!(opaque > 0);
    assert_eq!(measure(fg, &composite).changed_opaque_pixels, 0);
}

#[test]
fn front_lit_bike_is_grounded_at_both_tyres_and_no_product_pixel_changes() {
    let fg = bike();
    let plate = ground_plate(W, H);
    let out = ground_shadow(&plate, &fg, &front_lit());
    let r = &out.report;
    assert!(r.applied, "{:?}", r.reason);
    assert_eq!(r.direction_source.as_deref(), Some("default_front_lit"));
    assert_eq!(r.shadow_falls.as_deref(), Some("behind"));
    // Ground line through both tyre bottoms (y = 270).
    let line = r.ground_line.unwrap();
    assert!(
        (line[0][1] - 271.0).abs() <= 2.0 && (line[1][1] - 271.0).abs() <= 2.0,
        "{line:?}"
    );
    assert_eq!(r.contacts.len(), 2, "{:?}", r.contacts);
    for (c, &(cx, _)) in r.contacts.iter().zip(WHEELS.iter()) {
        assert!(
            c[0] <= cx && cx <= c[1],
            "contact {c:?} misses the tyre at {cx}"
        );
    }
    assert_product_byte_exact(&fg, &plate, &out.plate);

    // Dark right at each tyre's contact (just left and right of the
    // tyre bottom, where the plate shows).
    for &(cx, _) in &WHEELS {
        let cx = cx as u32;
        let d = darkening(&plate, &out.plate, &fg, cx - 20, 268, cx + 20, 274);
        let far = darkening(&plate, &out.plate, &fg, cx - 20, 284, cx + 20, 290);
        eprintln!("contact x={cx}: {d:.4}, 15 px in front: {far:.4}");
        assert!(
            d > 0.15 && far < 0.2 * d,
            "contact darkening {d} at x={cx}, in front {far}"
        );
    }
    // Falls away from the viewer: behind the contact line (up the
    // image, through the open wheels) is darker than in front of it.
    let behind = darkening(&plate, &out.plate, &fg, 100, 250, 320, 266);
    let front = darkening(&plate, &out.plate, &fg, 100, 278, 320, 294);
    eprintln!("front-lit: behind {behind:.4} in front {front:.4}");
    assert!(
        behind > 2.0 * front && behind > 0.04,
        "behind {behind} front {front}"
    );
    // No halo: nothing near the frame tubes high above the ground.
    let high = darkening(&plate, &out.plate, &fg, 140, 110, 300, 200);
    assert!(high.abs() < 1e-9, "darkening around the frame {high}");
    // Deterministic.
    let again = ground_shadow(&plate, &fg, &front_lit());
    assert_eq!(again.plate.pixels, out.plate.pixels);
    assert_eq!(again.report, out.report);
    // Record shape.
    let rec = r.to_json();
    assert_eq!(rec["applied"], json!(true));
    assert_eq!(rec["product_pixels_written"], json!(0));
    assert_eq!(rec["plate_pixel_sha256"], json!(out.plate.pixel_sha256()));
    assert_eq!(rec["light"]["direction_source"], json!("default_front_lit"));
}

#[test]
fn the_light_side_moves_the_cast_shadow() {
    let fg = bike();
    let plate = ground_plate(W, H);
    let run = |side: LightSide| {
        let light = GroundLight {
            side: Some(side),
            method: "cast_shadow".to_string(),
        };
        let out = ground_shadow(&plate, &fg, &light);
        assert!(out.report.applied);
        assert_eq!(
            out.report.direction_source.as_deref(),
            Some("light_estimate")
        );
        assert_product_byte_exact(&fg, &plate, &out.plate);
        out.plate
    };
    // Light from camera-left: the shadow falls to camera-right.
    // Side shadows lie along the ground line (rows 258..274), past the
    // wheels on the far side from the light.
    let p = run(LightSide::FromLeft);
    let right = peak_darkening(&plate, &p, &fg, 345, 258, 395, 274);
    let left = peak_darkening(&plate, &p, &fg, 20, 258, 75, 274);
    eprintln!("from camera-left: right {right:.4} left {left:.4}");
    assert!(
        right > 0.05 && right > 3.0 * left,
        "right {right} left {left}"
    );
    let p = run(LightSide::FromRight);
    let right = peak_darkening(&plate, &p, &fg, 345, 258, 395, 274);
    let left = peak_darkening(&plate, &p, &fg, 20, 258, 75, 274);
    eprintln!("from camera-right: right {right:.4} left {left:.4}");
    assert!(
        left > 0.05 && left > 3.0 * right,
        "left {left} right {right}"
    );
    // Back-lit: the shadow falls toward the camera (down the image).
    let p = run(LightSide::FromBehind);
    let behind = darkening(&plate, &p, &fg, 100, 250, 320, 266);
    let front = darkening(&plate, &p, &fg, 100, 278, 320, 294);
    eprintln!("back-lit: behind {behind:.4} in front {front:.4}");
    assert!(
        front > 2.0 * behind && front > 0.04,
        "front {front} behind {behind}"
    );
    // Overhead: only a band at the base.
    let p = run(LightSide::Overhead);
    let far = darkening(&plate, &p, &fg, 100, 284, 320, 298);
    let base = darkening(&plate, &p, &fg, 100, 264, 320, 278);
    eprintln!("overhead: base {base:.4} far {far:.4}");
    assert!(base > 0.05 && far < 0.2 * base, "base {base} far {far}");
}

#[test]
fn thin_structures_cast_faint_shadows_not_blobs() {
    // A 2 px pole and a 40 px block of the same height on the same
    // ground, lit from camera-left so the cast shadow lies in the open
    // to the right: the pole's cast shadow (clear of its contact band)
    // peaks at a small fraction of the block's.
    let plate = ground_plate(W, H);
    let side = GroundLight {
        side: Some(LightSide::FromLeft),
        method: "cast_shadow".to_string(),
    };
    let cast_peak = |x0: u32, x1: u32| {
        let mut fg = RgbaImage::solid(W, H, [0, 0, 0, 0]);
        for y in 120..270 {
            for x in x0..x1 {
                put(&mut fg, x as i64, y as i64, FRAME);
            }
        }
        let out = ground_shadow(&plate, &fg, &side);
        assert!(out.report.applied);
        assert_product_byte_exact(&fg, &plate, &out.plate);
        // Strongest darkening 40..80 px right of the object, where only
        // its upper half's shadow lands.
        let mut peak: f64 = 0.0;
        for y in 240..280u32 {
            for x in x1 + 40..x1 + 80 {
                let i = ((y * W + x) * 4) as usize;
                if fg.pixels[i + 3] != 0 {
                    continue;
                }
                let d = 1.0 - luma(&out.plate.pixels[i..i + 4]) / luma(&plate.pixels[i..i + 4]);
                peak = peak.max(d);
            }
        }
        (peak, out)
    };
    let (pole, _) = cast_peak(149, 151);
    let (block, _) = cast_peak(111, 151);
    eprintln!("cast peak: pole {pole:.4} block {block:.4}");
    assert!(block > 0.15, "block cast peak {block}");
    assert!(pole < 0.3 * block, "pole {pole} vs block {block}");
}

#[test]
fn surfaces_and_bottom_cut_products_are_left_alone() {
    let plate = ground_plate(W, H);
    // Cut by the bottom edge of the frame.
    let mut fg = RgbaImage::solid(W, H, [0, 0, 0, 0]);
    for y in 200..H {
        for x in 150..250 {
            put(&mut fg, x as i64, y as i64, FRAME);
        }
    }
    let out = ground_shadow(&plate, &fg, &front_lit());
    assert!(!out.report.applied);
    assert!(out
        .report
        .reason
        .as_deref()
        .unwrap()
        .contains("bottom edge"));
    assert_eq!(out.plate.pixels, plate.pixels);
    // Empty layer.
    let empty = RgbaImage::solid(W, H, [0, 0, 0, 0]);
    let out = ground_shadow(&plate, &empty, &front_lit());
    assert!(!out.report.applied);
    assert_eq!(out.plate.pixels, plate.pixels);
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("agenthand-ground-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Through the store: the grounded draw keeps every alpha-255 pixel,
/// verifies, records `ground_shadow`; the kill switch gives the plain
/// composite byte for byte; the legacy draw path has no record.
#[test]
fn grounded_draw_verifies_and_the_kill_switch_restores_the_plain_composite() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var(GROUND_SHADOW_ENV).ok();
    let dir = tmpdir("draw");
    let store = AssetStore::new(&dir);
    let fg = bike();
    let mut source = ground_plate(W, H);
    source = alpha_composite(&source, &fg);
    let source_handle = store.put_image(&source, KIND_SOURCE, None).unwrap();
    let parent = AssetStore::parse_handle(&source_handle).unwrap();
    let fg_handle = store
        .put_image(&fg, KIND_FOREGROUND, Some(&parent))
        .unwrap();
    // A generated plate at another size: the draw cover-fits it.
    let plate = ground_plate(600, 450);
    let plate_handle = store.put_image(&plate, KIND_PLATE, None).unwrap();

    std::env::remove_var(GROUND_SHADOW_ENV);
    let (plain_handle, plain_receipt) = composite::draw_product_with_plate_kind(
        &store,
        &fg_handle,
        "drive",
        Some(&plate_handle),
        None,
    )
    .unwrap();
    assert!(plain_receipt.get("ground_shadow").is_none());
    let plain = store.read_image(&plain_handle).unwrap();

    let (handle, receipt) =
        composite::draw_product_grounded(&store, &fg_handle, "drive", &plate_handle, &front_lit())
            .unwrap();
    let record = &receipt["ground_shadow"];
    assert_eq!(record["applied"], json!(true), "{record}");
    assert_eq!(record["product_pixels_written"], json!(0));
    assert_eq!(receipt["changed_opaque_pixels"], json!(0));
    assert_eq!(receipt["plate_kind"], json!("model_generated"));
    // The fitted-plate hash keeps its meaning: the fit of the stored plate.
    assert_eq!(
        receipt["plate_fitted_pixel_sha256"],
        plain_receipt["plate_fitted_pixel_sha256"]
    );
    assert_eq!(receipt["opaque_pixels"], plain_receipt["opaque_pixels"]);
    let delivered = store.read_image(&handle).unwrap();
    assert_ne!(delivered.pixels, plain.pixels);
    for i in (0..fg.pixels.len()).step_by(4) {
        if fg.pixels[i + 3] == 255 {
            assert_eq!(&delivered.pixels[i..i + 4], &fg.pixels[i..i + 4]);
        }
    }
    let report = composite::verify_receipt_from_disk(&store, &handle, Some(&receipt)).unwrap();
    assert_eq!(report["ok"], json!(true), "{}", report["problems"]);

    for off in ["off", "0", "false", "no", "OFF"] {
        std::env::set_var(GROUND_SHADOW_ENV, off);
        let (h, r) = composite::draw_product_grounded(
            &store,
            &fg_handle,
            "drive",
            &plate_handle,
            &front_lit(),
        )
        .unwrap();
        assert_eq!(r["ground_shadow"]["applied"], json!(false));
        assert_eq!(
            r["ground_shadow"]["reason"],
            json!(format!("disabled by {GROUND_SHADOW_ENV}"))
        );
        assert_eq!(store.read_image(&h).unwrap().pixels, plain.pixels, "{off}");
    }

    match previous {
        Some(v) => std::env::set_var(GROUND_SHADOW_ENV, v),
        None => std::env::remove_var(GROUND_SHADOW_ENV),
    }
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// Real fixture (opt-in): IMG_1443 foreground on a stored plate, with and
// without the ground shadow, full frame plus a zoom on each contact.
// ---------------------------------------------------------------------------

fn load(path: &std::path::Path) -> RgbaImage {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    agenthand::image::decode_image(&bytes).unwrap().into_rgba()
}

fn to_rgb(img: &RgbaImage, x0: u32, y0: u32, w: u32, h: u32, out_w: u32) -> image::RgbImage {
    let buf = image::RgbaImage::from_raw(img.width, img.height, img.pixels.clone()).unwrap();
    let crop = image::imageops::crop_imm(&buf, x0, y0, w, h).to_image();
    let out_h = (h as u64 * out_w as u64 / w as u64) as u32;
    let small = image::imageops::resize(&crop, out_w, out_h, image::imageops::FilterType::Triangle);
    image::DynamicImage::ImageRgba8(small).to_rgb8()
}

#[test]
#[ignore]
fn real_fixture_hero_ground() {
    let Ok(dir) = std::env::var("AGENTHAND_GROUND_ARTIFACTS") else {
        eprintln!("AGENTHAND_GROUND_ARTIFACTS not set; skipping");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let out_dir = std::path::PathBuf::from(
        std::env::var("AGENTHAND_GROUND_OUT").unwrap_or_else(|_| "ground-out".to_string()),
    );
    std::fs::create_dir_all(&out_dir).unwrap();
    let source = load(&dir.join("source.png"));
    let fg = load(&dir.join("foreground.png"));
    let plate = load(&dir.join("plate.png"));
    let estimate = agenthand::staging::estimate_light_direction(&source, &fg);
    eprintln!(
        "light estimate: {:?} {} ({})",
        estimate.side, estimate.method, estimate.reason
    );
    let light = GroundLight::from_estimate(&estimate);
    let t = std::time::Instant::now();
    let out = ground_shadow(&plate, &fg, &light);
    eprintln!("ground shadow in {:.2}s", t.elapsed().as_secs_f64());
    let rec = out.report.to_json();
    eprintln!("{}", serde_json::to_string_pretty(&rec).unwrap());
    assert!(out.report.applied);
    let before = alpha_composite(&plate, &fg);
    let after = alpha_composite(&out.plate, &fg);
    let mut opaque = 0u64;
    for i in (0..fg.pixels.len()).step_by(4) {
        if fg.pixels[i + 3] == 255 {
            opaque += 1;
            assert_eq!(&after.pixels[i..i + 4], &fg.pixels[i..i + 4]);
        }
    }
    let changed = measure(&fg, &after).changed_opaque_pixels;
    eprintln!("product pixels {opaque}, changed {changed}");
    assert_eq!(changed, 0);
    std::fs::write(
        out_dir.join("hero_ground_record.json"),
        serde_json::to_string_pretty(&json!({"record": rec, "product_pixels": opaque, "changed": changed,
            "light_estimate": {"side": estimate.side.map(|s| s.bucket()), "method": estimate.method, "reason": estimate.reason}}))
            .unwrap(),
    )
    .unwrap();

    // Canvas: full frame before | after; then for each contact, zoom
    // before | after.
    let pw = 1200u32;
    let full_b = to_rgb(&before, 0, 0, fg.width, fg.height, pw);
    let full_a = to_rgb(&after, 0, 0, fg.width, fg.height, pw);
    let ph = full_b.height();
    let zoom_w = std::env::var("AGENTHAND_GROUND_ZOOM_W")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1400u32);
    let zoom_h = zoom_w * 3 / 4;
    let mut zooms = Vec::new();
    for c in &out.report.contacts {
        let cx = ((c[0] + c[1]) / 2.0) as i64;
        let cy = c[2] as i64;
        let x0 = (cx - zoom_w as i64 / 2).clamp(0, (fg.width - zoom_w) as i64) as u32;
        let y0 = (cy - (zoom_h as i64 * 2) / 3).clamp(0, (fg.height - zoom_h) as i64) as u32;
        zooms.push((
            to_rgb(&before, x0, y0, zoom_w, zoom_h, pw),
            to_rgb(&after, x0, y0, zoom_w, zoom_h, pw),
        ));
    }
    let zh = zooms.first().map_or(0, |z| z.0.height());
    let gap = 16u32;
    let canvas_w = pw * 2 + gap;
    let canvas_h = ph + (zh + gap) * zooms.len() as u32;
    let mut canvas = image::RgbImage::from_pixel(canvas_w, canvas_h, image::Rgb([255, 255, 255]));
    image::imageops::replace(&mut canvas, &full_b, 0, 0);
    image::imageops::replace(&mut canvas, &full_a, (pw + gap) as i64, 0);
    for (k, (zb, za)) in zooms.iter().enumerate() {
        let y = (ph + gap + k as u32 * (zh + gap)) as i64;
        image::imageops::replace(&mut canvas, zb, 0, y);
        image::imageops::replace(&mut canvas, za, (pw + gap) as i64, y);
    }
    let name = std::env::var("AGENTHAND_GROUND_NAME")
        .unwrap_or_else(|_| "hero_ground_before_after.jpg".to_string());
    let mut file = std::io::BufWriter::new(std::fs::File::create(out_dir.join(name)).unwrap());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut file, 90)
        .encode_image(&canvas)
        .unwrap();
}
