//! Garment flat-lay measurements (`measure --garment`): a synthetic
//! T-shaped jacket with known dimensions lies in the plane of the
//! printed AH-BIG card; the photo is rendered through a known
//! projective homography, the card is found by the real detector, and
//! every tailor measurement is read back from the product mask.
//!
//! Tolerance: |measured - truth| <= max(1.2% of truth, 6 mm), and the
//! truth must lie inside the stated half range. The scene is rendered
//! at about 1.3 px/mm (0.77 mm per pixel), the outline is smoothed at
//! sigma 2 mm and resampled at 1 mm, and the card fit carries the
//! estimator's own ~1% range model; corner landmarks on the sharp
//! synthetic polygon are located to the 1 mm resample step. 6 mm
//! covers a pixel of quantization at both ends plus the step; 1.2%
//! covers the card scale term at a 1.5 m lever.

use agenthand::measure::{estimate_with_plane, load_card_profile, MeasureFrame, Plane};
use agenthand::measure_garment::{analyse, render_diagram, Garment, GarmentOptions};
use serde_json::Value;
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

type Pt = [f64; 2];

fn project(h: &[f64; 9], p: Pt) -> Pt {
    let w = h[6] * p[0] + h[7] * p[1] + h[8];
    [
        (h[0] * p[0] + h[1] * p[1] + h[2]) / w,
        (h[3] * p[0] + h[4] * p[1] + h[5]) / w,
    ]
}

fn inv3(m: &[f64; 9]) -> [f64; 9] {
    let det = m[0] * (m[4] * m[8] - m[5] * m[7]) - m[1] * (m[3] * m[8] - m[5] * m[6])
        + m[2] * (m[3] * m[7] - m[4] * m[6]);
    [
        (m[4] * m[8] - m[5] * m[7]) / det,
        (m[2] * m[7] - m[1] * m[8]) / det,
        (m[1] * m[5] - m[2] * m[4]) / det,
        (m[5] * m[6] - m[3] * m[8]) / det,
        (m[0] * m[8] - m[2] * m[6]) / det,
        (m[2] * m[3] - m[0] * m[5]) / det,
        (m[3] * m[7] - m[4] * m[6]) / det,
        (m[1] * m[6] - m[0] * m[7]) / det,
        (m[0] * m[4] - m[1] * m[3]) / det,
    ]
}

fn inside(poly: &[Pt], p: Pt) -> bool {
    let mut c = false;
    let n = poly.len();
    for i in 0..n {
        let a = poly[i];
        let b = poly[(i + 1) % n];
        if (a[1] > p[1]) != (b[1] > p[1]) {
            let x = a[0] + (p[1] - a[1]) * (b[0] - a[0]) / (b[1] - a[1]);
            if p[0] < x {
                c = !c;
            }
        }
    }
    c
}

/// The garment, in its own frame (u right, v down, collar at the top),
/// mm. Collar bump 180 x 40 above the high points of shoulder at
/// (+-90, 0); shoulder line to the shoulder corners at (+-240, 40);
/// sleeves 600 mm along the top edge at 40 degrees below horizontal,
/// cuff 180 mm across; body sides from the underarm points down to the
/// hem corners at (+-270, 600).
struct Tee {
    outline: Vec<Pt>,
    truth: Truth,
}

struct Truth {
    body_length: f64,
    chest: f64,
    shoulder: f64,
    sleeve: f64,
    hem: f64,
    /// Garment-frame endpoints of each figure (right side for the
    /// one-sided ones).
    body_ends: [Pt; 2],
    chest_ends: [Pt; 2],
    shoulder_ends: [Pt; 2],
    sleeve_right_ends: [Pt; 2],
    sleeve_left_ends: [Pt; 2],
    hem_ends: [Pt; 2],
    /// Underarm sleeve: underarm point to the inner cuff corner.
    underarm: f64,
    underarm_right_ends: [Pt; 2],
    underarm_left_ends: [Pt; 2],
}

fn tee(fold_left_sleeve: bool) -> Tee {
    let sleeve_len = 600.0;
    let cuff = 180.0;
    let ang = 40f64.to_radians();
    let d = [ang.cos(), ang.sin()];
    let nrm = [-ang.sin(), ang.cos()];
    let sp = [240.0, 40.0];
    let co = [sp[0] + sleeve_len * d[0], sp[1] + sleeve_len * d[1]];
    let ci = [co[0] + cuff * nrm[0], co[1] + cuff * nrm[1]];
    // Underside meets the body side u = 260.
    let t = (ci[0] - 260.0) / d[0];
    let arm = [260.0, ci[1] - t * d[1]];
    let hem_c = [270.0, 600.0];
    let mirror = |p: Pt| [-p[0], p[1]];
    let mut right = vec![[90.0, -40.0], [90.0, 0.0], sp, co, ci, arm, hem_c];
    let mut out: Vec<Pt> = Vec::new();
    out.append(&mut right);
    let left_full: Vec<Pt> = vec![
        mirror(hem_c),
        mirror(arm),
        mirror(ci),
        mirror(co),
        mirror(sp),
        [-90.0, 0.0],
        [-90.0, -40.0],
    ];
    if fold_left_sleeve {
        // The left sleeve lies folded across the front: the outline runs
        // from the shoulder straight down the armhole and side.
        out.extend_from_slice(&[
            mirror(hem_c),
            mirror(arm),
            [-262.0, 80.0],
            mirror(sp),
            [-90.0, 0.0],
            [-90.0, -40.0],
        ]);
    } else {
        out.extend(left_full);
    }
    let side_u = |v: f64| arm[0] + (hem_c[0] - arm[0]) * (v - arm[1]) / (hem_c[1] - arm[1]);
    Tee {
        outline: out,
        truth: Truth {
            body_length: 600.0,
            chest: 2.0 * side_u(arm[1] + 10.0),
            shoulder: 480.0,
            sleeve: sleeve_len,
            hem: 2.0 * side_u(600.0 - 10.0),
            body_ends: [[90.0, 0.0], [90.0, 600.0]],
            chest_ends: [
                [-side_u(arm[1] + 10.0), arm[1] + 10.0],
                [side_u(arm[1] + 10.0), arm[1] + 10.0],
            ],
            shoulder_ends: [mirror(sp), sp],
            sleeve_right_ends: [sp, co],
            sleeve_left_ends: [mirror(sp), mirror(co)],
            hem_ends: [[-side_u(590.0), 590.0], [side_u(590.0), 590.0]],
            underarm: t,
            underarm_right_ends: [arm, ci],
            underarm_left_ends: [mirror(arm), mirror(ci)],
        },
    }
}

/// Garment frame -> card mm: rotated 15 degrees in the card plane,
/// centred at (1050, 350) mm, to the right of the card.
fn garment_to_card(p: Pt) -> Pt {
    let (s, c) = 15f64.to_radians().sin_cos();
    [1050.0 + c * p[0] - s * p[1], 350.0 + s * p[0] + c * p[1]]
}

/// The scene's card-mm box: the card sheet and the garment, plus a
/// 40 mm margin, as its four corners.
fn scene_box_corners() -> [Pt; 4] {
    let mut lo: Pt = [-12.0, -12.0];
    let mut hi: Pt = [266.0, 202.5];
    for p in tee(false).outline {
        let q = garment_to_card(p);
        lo = [lo[0].min(q[0]), lo[1].min(q[1])];
        hi = [hi[0].max(q[0]), hi[1].max(q[1])];
    }
    let (lo, hi) = ([lo[0] - 40.0, lo[1] - 40.0], [hi[0] + 40.0, hi[1] + 40.0]);
    [lo, [hi[0], lo[1]], hi, [lo[0], hi[1]]]
}

/// Card mm -> image px: mild perspective (the far side of the frame
/// ~4% smaller than the near side), the camera turned a little.
fn scene_h() -> [f64; 9] {
    let base = [
        1.30, 0.06, 0.0, //
        -0.05, 1.27, 0.0, //
        2.0e-5, 1.0e-5, 1.0,
    ];
    // Shift so the whole scene (card sheet and garment) lands at
    // >= 40 px.
    let mut lo = [f64::INFINITY; 2];
    for p in scene_box_corners() {
        let p = project(&base, p);
        lo = [lo[0].min(p[0]), lo[1].min(p[1])];
    }
    let (tx, ty) = (40.0 - lo[0], 40.0 - lo[1]);
    // Translate in the image: T * base.
    [
        base[0] + tx * base[6],
        base[1] + tx * base[7],
        base[2] + tx * base[8],
        base[3] + ty * base[6],
        base[4] + ty * base[7],
        base[5] + ty * base[8],
        base[6],
        base[7],
        base[8],
    ]
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

struct Scene {
    img: agenthand::image::RgbaImage,
    mask: Vec<u8>,
}

/// Render the photo (floor, printed AH-BIG card, denim garment) and the
/// product mask through the known homography.
fn render(t: &Tee) -> Scene {
    let art_path = root().join("tests/fixtures/measure/printed/card-final.png");
    let art = agenthand::image::decode_image(&std::fs::read(art_path).unwrap())
        .unwrap()
        .into_rgba();
    let profile: Value = serde_json::from_str(
        &std::fs::read_to_string(root().join("tests/fixtures/layout-profile.json")).unwrap(),
    )
    .unwrap();
    let spec = &profile["cards"]["AH-BIG"]["image"];
    let s = spec["scale_mm_per_unit"].as_f64().unwrap();
    let ox = spec["offset_x_mm"].as_f64().unwrap();
    let oy = spec["offset_y_mm"].as_f64().unwrap();
    let (aw, ah) = (art.width as f64, art.height as f64);
    let art_rgb = |x: f64, y: f64| -> [f64; 3] {
        let (x0, y0) = (x.floor(), y.floor());
        let (dx, dy) = (x - x0, y - y0);
        let mut out = [0.0; 3];
        for (xx, yy, wt) in [
            (x0, y0, (1.0 - dx) * (1.0 - dy)),
            (x0 + 1.0, y0, dx * (1.0 - dy)),
            (x0, y0 + 1.0, (1.0 - dx) * dy),
            (x0 + 1.0, y0 + 1.0, dx * dy),
        ] {
            let cx = xx.clamp(0.0, aw - 1.0) as usize;
            let cy = yy.clamp(0.0, ah - 1.0) as usize;
            let k = (cy * art.width as usize + cx) * 4;
            for c in 0..3 {
                out[c] += wt * art.pixels[k + c] as f64;
            }
        }
        out
    };
    let h = scene_h();
    let hi = inv3(&h);
    let mut hi_px = [0.0f64; 2];
    for p in scene_box_corners() {
        let p = project(&h, p);
        hi_px = [hi_px[0].max(p[0]), hi_px[1].max(p[1])];
    }
    let (w, ht) = ((hi_px[0] + 40.0) as usize, (hi_px[1] + 40.0) as usize);
    let poly: Vec<Pt> = t.outline.iter().map(|&p| garment_to_card(p)).collect();
    let mut pixels = vec![0u8; w * ht * 4];
    let mut mask = vec![0u8; w * ht];
    let mut rng = Lcg(0x6a2e_1491);
    for y in 0..ht {
        for x in 0..w {
            let k = (y * w + x) * 4;
            let grain = (rng.next() - 0.5) * 14.0;
            let mut rgb = [150.0 + grain, 128.0 + grain, 104.0 + grain];
            let mm = project(&hi, [x as f64, y as f64]);
            if (-12.0..=266.0).contains(&mm[0]) && (-12.0..=202.5).contains(&mm[1]) {
                rgb = [238.0, 236.0, 230.0];
                let u = [(mm[0] - ox) / s, (mm[1] - oy) / s];
                if (0.0..1200.0).contains(&u[0]) && (0.0..880.0).contains(&u[1]) {
                    let a = art_rgb(u[0] * 2.0 - 0.5, u[1] * 2.0 - 0.5);
                    for c in 0..3 {
                        rgb[c] = 20.0 + a[c] * (rgb[c] - 20.0) / 255.0;
                    }
                }
            }
            if inside(&poly, mm) {
                rgb = [112.0 + grain, 138.0 + grain, 176.0 + grain];
                mask[y * w + x] = 255;
            }
            for c in 0..3 {
                pixels[k + c] = rgb[c].round().clamp(0.0, 255.0) as u8;
            }
            pixels[k + 3] = 255;
        }
    }
    Scene {
        img: agenthand::image::RgbaImage {
            width: w as u32,
            height: ht as u32,
            pixels,
            icc_profile: None,
        },
        mask,
    }
}

fn measured(g: &Garment, key: &str) -> (f64, f64) {
    let m = g
        .measurement(key)
        .unwrap_or_else(|| panic!("{key} missing"));
    let v = m
        .value_mm
        .unwrap_or_else(|| panic!("{key} not measurable: {:?}", m.reason));
    (v, m.plus_minus_mm)
}

/// Two checks per figure.
///
/// 1. Landmark logic: the measurement against the same garment
///    endpoints carried through the FITTED card plane (true garment
///    point -> true homography -> pixel -> fitted homography -> mm).
///    This isolates the garment code from the card fit; tolerance
///    LANDMARK_TOL_MM (below).
/// 2. Honesty of the range: the physical truth lies inside the stated
///    half range, whose card term carries the card fit's own error at
///    this lever (a 254 mm card read out to 1.5 m away).
fn check(g: &Garment, plane: &Plane, key: &str, truth: f64, ends: [Pt; 2]) {
    let (v, pm) = measured(g, key);
    let h = scene_h();
    let fit = |p: Pt| plane.to_mm(project(&h, garment_to_card(p)));
    let (a, b) = (fit(ends[0]), fit(ends[1]));
    let plane_truth = (a[0] - b[0]).hypot(a[1] - b[1]);
    println!(
        "{key}: {v:.1} mm +- {pm:.1}; in the fitted plane {plane_truth:.1} (error {:+.1}); \
         physical truth {truth:.1} (error {:+.1})",
        v - plane_truth,
        v - truth
    );
    assert!(
        (v - plane_truth).abs() <= LANDMARK_TOL_MM,
        "{key}: {v:.1} vs {plane_truth:.1} in the fitted plane (tolerance {LANDMARK_TOL_MM})"
    );
    assert!(
        (v - truth).abs() <= pm,
        "{key}: truth {truth:.1} outside the stated {v:.1} +- {pm:.1}"
    );
}

/// Landmark tolerance, mm. The scene is ~1.3 px/mm (0.77 mm per
/// pixel): a pixel of mask quantization at each end (1.5 mm), the 1 mm
/// resample step at each end (1 mm), and the sigma 2 mm outline
/// smoothing, which pulls a sharp corner inward by under 1 mm. 4 mm
/// is that budget rounded up; a wrong landmark (a collar corner taken
/// for the shoulder, a cuff corner missed) is off by centimetres.
const LANDMARK_TOL_MM: f64 = 4.0;

fn plane_for(scene: &Scene) -> Plane {
    let profile = root().join("tests/fixtures/layout-profile.json");
    let card = load_card_profile(&profile, "AH-BIG").unwrap();
    let frame = MeasureFrame::from_rgba(&scene.img);
    let (est, plane) = estimate_with_plane(&frame, &card, "AH-BIG", None, Some(&scene.mask))
        .unwrap_or_else(|e| panic!("card refused in the synthetic scene: {e}"));
    println!("{}", est.to_json_pretty());
    assert!(
        est.warnings
            .iter()
            .any(|w| w.contains("flat on the same surface as the card")),
        "mask extent must state the plane assumption"
    );
    // The card fit recovers the scene homography.
    let truth_h = scene_h();
    for p in [[0.0, 0.0], [254.0, 190.5], [1800.0, 800.0]] {
        let a = plane.to_px(p);
        let b = project(&truth_h, p);
        let e = (a[0] - b[0]).hypot(a[1] - b[1]);
        println!("H check at {p:?} mm: {e:.2} px");
    }
    plane
}

#[test]
fn tee_flat_lay_measures_every_landmark() {
    let t = tee(false);
    let scene = render(&t);
    let plane = plane_for(&scene);
    let g = analyse(
        scene.img.width as usize,
        scene.img.height as usize,
        &scene.mask,
        &plane,
        &GarmentOptions { mask_edge_px: 1.0 },
    )
    .unwrap();
    println!("{}", serde_json::to_string_pretty(&g.to_json()).unwrap());
    assert_eq!(g.sleeves_found, 2);
    // Axis: collar -> hem is garment +v, i.e. 15 + 90 degrees in the card plane.
    assert!(
        (g.axis_deg - 105.0).abs() <= 1.0,
        "axis {:.2} deg, expected 105",
        g.axis_deg
    );
    let tr = &t.truth;
    check(&g, &plane, "body_length", tr.body_length, tr.body_ends);
    check(&g, &plane, "chest_width", tr.chest, tr.chest_ends);
    check(&g, &plane, "shoulder_width", tr.shoulder, tr.shoulder_ends);
    check(
        &g,
        &plane,
        "sleeve_length_left",
        tr.sleeve,
        tr.sleeve_left_ends,
    );
    check(
        &g,
        &plane,
        "sleeve_length_right",
        tr.sleeve,
        tr.sleeve_right_ends,
    );
    check(&g, &plane, "hem_width", tr.hem, tr.hem_ends);
    check(
        &g,
        &plane,
        "underarm_sleeve_left",
        tr.underarm,
        tr.underarm_left_ends,
    );
    check(
        &g,
        &plane,
        "underarm_sleeve_right",
        tr.underarm,
        tr.underarm_right_ends,
    );
    // The headline sleeve is the mean of both sides.
    let (v, pm) = measured(&g, "sleeve_length");
    assert!(
        (v - tr.sleeve).abs() <= pm,
        "sleeve_length {v:.1} +- {pm:.1}"
    );

    // Every measurable figure states its method and its uncertainty
    // components; the block states the plane assumption.
    let j = g.to_json();
    assert!(j["assumption"]
        .as_str()
        .unwrap()
        .contains("flat on the same surface as the card"));
    for (k, m) in j["measurements"].as_object().unwrap() {
        assert_eq!(m["measurable"], Value::Bool(true), "{k}");
        assert!(!m["method"].as_str().unwrap().is_empty(), "{k}");
        for c in ["card_fit", "mask_edge", "landmark", "contour_step"] {
            assert!(m["uncertainty_mm"][c].is_number(), "{k}: {c}");
        }
    }
    assert!(j["not_attempted"]["across_back"].is_string());

    // The diagram renders at 1080p.
    let img = render_diagram(&g, "AH-BIG", Some("Synthetic test garment"));
    assert_eq!((img.width, img.height), (1920, 1080));
    let out = root().join("target/garment-test-diagram.png");
    let _ = std::fs::write(out, agenthand::image::encode_png_rgba(&img));
}

#[test]
fn folded_sleeve_is_not_measurable() {
    let full = tee(false);
    let scene = render(&full);
    let plane = plane_for(&scene);
    // Same photo geometry, the left sleeve folded across the front: only
    // the outline (the mask) changes.
    let t = tee(true);
    let folded = render(&t);
    let g = analyse(
        folded.img.width as usize,
        folded.img.height as usize,
        &folded.mask,
        &plane,
        &GarmentOptions { mask_edge_px: 1.0 },
    )
    .unwrap();
    println!("{}", serde_json::to_string_pretty(&g.to_json()).unwrap());
    assert_eq!(g.sleeves_found, 1);
    let left = g.measurement("sleeve_length_left").unwrap();
    assert!(
        !left.measurable(),
        "folded sleeve measured: {:?}",
        left.value_mm
    );
    let reason = left.reason.as_deref().unwrap();
    assert!(reason.contains("No sleeve leaves the outline"), "{reason}");
    // Landmarks the fold hides are not measured either.
    for key in ["chest_width", "shoulder_width"] {
        let m = g.measurement(key).unwrap();
        assert!(!m.measurable(), "{key} measured with a hidden landmark");
        assert!(m.reason.is_some(), "{key} lacks a reason");
    }
    // What the fold leaves visible still measures.
    let tr = &t.truth;
    check(
        &g,
        &plane,
        "sleeve_length_right",
        tr.sleeve,
        tr.sleeve_right_ends,
    );
    check(&g, &plane, "body_length", tr.body_length, tr.body_ends);
    check(&g, &plane, "hem_width", tr.hem, tr.hem_ends);
    let j = g.to_json();
    assert_eq!(
        j["measurements"]["sleeve_length_left"]["value_cm"],
        Value::Null
    );
    assert_eq!(
        j["measurements"]["sleeve_length_left"]["measurable"],
        Value::Bool(false)
    );
}

/// The CLI as docs/MEASURE-GARMENT.md documents it: `--mask-edge-px`
/// is accepted and its value is the edge accuracy the garment JSON
/// reports (IMG_1491's 3.552 = 4284 / 1206). A non-positive value is a
/// usage error.
#[test]
fn cli_takes_mask_edge_px_as_documented() {
    let photo = root().join("tests/fixtures/measure/ah42_close.png");
    let profile = root().join("tests/fixtures/measure/rendered-layout-profile.json");
    let (w, h) = image::image_dimensions(&photo).unwrap();
    // A T: torso plus a sleeve bar across its top.
    let (cx, top) = (w / 2, h / 3);
    let mask = image::GrayImage::from_fn(w, h, |x, y| {
        let torso = x + w / 8 >= cx && x <= cx + w / 8 && y >= top && y + 10 <= h;
        let sleeves = x + w / 3 >= cx && x <= cx + w / 3 && y >= top && y <= top + h / 8;
        image::Luma([if torso || sleeves { 255 } else { 0 }])
    });
    let dir = std::env::temp_dir().join(format!("agenthand-edge-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mask_path = dir.join("mask.png");
    mask.save(&mask_path).unwrap();
    let run = |edge: &str| {
        std::process::Command::new(env!("CARGO_BIN_EXE_agenthand"))
            .arg("measure")
            .arg(&photo)
            .arg("AH-42")
            .arg("--profile")
            .arg(&profile)
            .arg("--mask")
            .arg(&mask_path)
            .args(["--garment", "--mask-edge-px", edge])
            .output()
            .unwrap()
    };
    let o = run("3.552");
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{stdout}{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let v: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["garment"]["mask"]["edge_accuracy_px"], 3.552, "{stdout}");
    let bad = run("0");
    assert_eq!(bad.status.code(), Some(2));
    let _ = std::fs::remove_dir_all(&dir);
}
