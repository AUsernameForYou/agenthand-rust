//! Angle lock: every source_edit plate prompt carries the base clause
//! (with or without caller scene text, object or surface wording), the
//! shot-angle estimator states a specific angle only when every
//! geometry cue agrees, and the legacy generated plate gets the same
//! angle as steering words. The staged composite keeps 0 changed.

use agenthand::composite::composite_images;
use agenthand::image::{GrayImage, RgbaImage};
use agenthand::meta::{
    angle_lock_clause, generated_plate_scene, plate_edit_prompt, plate_edit_prompt_locked,
    wrap_plate_prompt, ANGLE_LOCK_CLAUSE, SHADOW_CLAUSE, ANGLE_PHRASE_EYE_LEVEL, ANGLE_PHRASE_LOW_UP,
    ANGLE_PHRASE_STEEP_DOWN,
};
use agenthand::segmentation::extract_foreground;
use agenthand::staging::{
    self, estimate_shot_angle, estimate_shot_angle_from, AngleEstimate, ShotAngle,
};

const STEEP_SENTENCE: &str =
    "The original photo was shot from standing height looking down at the surface at a steep angle.";
const LOW_SENTENCE: &str = "The original photo was shot from a low angle looking slightly up.";

fn est(
    w: u32,
    h: u32,
    bbox: (u32, u32, u32, u32),
    coverage: f64,
    fills_frame: bool,
    touches: &[&str],
) -> AngleEstimate {
    estimate_shot_angle_from(w, h, Some(bbox), coverage, fills_frame, touches)
}

/// A 1000x1000 frame with the bbox given as fractions.
fn est_frac(top: f64, bottom: f64, left: f64, right: f64) -> AngleEstimate {
    let px = |f: f64| (f * 1000.0).round() as u32;
    est(1000, 1000, (px(left), px(top), px(right), px(bottom)), 0.2, false, &[])
}

fn stated(angle: ShotAngle) -> AngleEstimate {
    AngleEstimate {
        angle,
        stated: true,
        signals: None,
        reason: "test",
    }
}

fn unstated() -> AngleEstimate {
    AngleEstimate {
        angle: ShotAngle::EyeLevel,
        stated: false,
        signals: None,
        reason: "test",
    }
}

// --- the estimator -------------------------------------------------------

#[test]
fn real_photo_geometries_calibrate_the_buckets() {
    // Car (offline-out/car_summary.json): standing height, looking down
    // at a car on a driveway; cut by the top edge.
    let car = est(1920, 1080, (271, 0, 1419, 824), 0.309081, false, &["top"]);
    assert_eq!((car.angle, car.stated), (ShotAngle::SteepDown, true), "{car:?}");
    // Jacket flat lay on a floor, shot from above.
    let jacket = est(4284, 5712, (21, 1439, 4241, 4209), 0.240141, false, &[]);
    assert_eq!(
        (jacket.angle, jacket.stated),
        (ShotAngle::SteepDown, true),
        "{jacket:?}"
    );
    // Bike: mild downward tilt, ground line near the bottom. The cues do
    // not agree, so nothing beyond the base clause is claimed.
    let bike = est(5712, 4284, (330, 419, 5176, 3890), 0.24917, false, &[]);
    assert_eq!((bike.angle, bike.stated), (ShotAngle::EyeLevel, false), "{bike:?}");
    let s = bike.signals.expect("signals recorded");
    assert!((s.contact_y - 3890.0 / 4284.0).abs() < 1e-9);
    assert!((s.centre_y - (419.0 + 3890.0) / 2.0 / 4284.0).abs() < 1e-9);
}

#[test]
fn steep_down_needs_every_cue() {
    // Wide subject on a clear band of ground, reaching into the upper third.
    let base = est_frac(0.25, 0.75, 0.10, 0.90);
    assert_eq!((base.angle, base.stated), (ShotAngle::SteepDown, true));
    // Thresholds are inclusive: ground below exactly 0.15, top exactly 0.33.
    let edge = est_frac(0.33, 0.85, 0.05, 0.95);
    assert_eq!(edge.angle, ShotAngle::SteepDown, "{edge:?}");
    // Ground line too close to the bottom (0.14 of frame below it).
    assert!(!est_frac(0.25, 0.86, 0.05, 0.95).stated);
    // Top edge below the upper third.
    assert!(!est_frac(0.34, 0.75, 0.05, 0.95).stated);
    // Footprint not wide (bbox 0.5 x 0.5).
    assert!(!est_frac(0.25, 0.75, 0.25, 0.75).stated);
    // Too small a subject (0.2 of the frame tall).
    assert!(!est_frac(0.30, 0.50, 0.20, 0.80).stated);
    // The literal "subject low in frame" eye-level case: a low wide
    // object whose top sits at the horizon (midline) with ground in
    // front. That is what an eye-level shot looks like, so no claim.
    let low_wide = est_frac(0.50, 0.80, 0.10, 0.90);
    assert_eq!((low_wide.angle, low_wide.stated), (ShotAngle::EyeLevel, false));
}

#[test]
fn low_up_needs_a_towering_subject_on_a_thin_strip_of_ground() {
    // From the top edge down to 0.92, narrow: low camera, looking up.
    let low = est(1000, 1000, (350, 0, 650, 920), 0.2, false, &["top"]);
    assert_eq!((low.angle, low.stated), (ShotAngle::LowUp, true), "{low:?}");
    // Same, but wide: not towering, no claim.
    assert!(!est(1000, 1000, (50, 0, 950, 920), 0.4, false, &["top"]).stated);
    // Centre at the midline (top 0.08): not high in frame, no claim.
    assert!(!est(1000, 1000, (350, 80, 650, 920), 0.2, false, &[]).stated);
    // Ground line not near the bottom (0.80): no claim.
    assert!(!est(1000, 1000, (350, 0, 650, 800), 0.2, false, &["top"]).stated);
}

#[test]
fn unsure_geometry_defaults_to_unstated_eye_level() {
    // Cut by the bottom edge: the ground-contact line is not visible.
    let cut = est(1000, 1000, (100, 250, 900, 1000), 0.3, false, &["bottom"]);
    assert_eq!((cut.angle, cut.stated), (ShotAngle::EyeLevel, false));
    assert!(cut.reason.contains("bottom edge"));
    // A surface that fills the frame.
    let lawn = est(1000, 1000, (0, 300, 1000, 1000), 0.7, true, &["left", "right", "bottom"]);
    assert_eq!((lawn.angle, lawn.stated), (ShotAngle::EyeLevel, false));
    // No bbox, degenerate bbox.
    let none = estimate_shot_angle_from(1000, 1000, None, 0.0, false, &[]);
    assert_eq!((none.angle, none.stated, none.signals), (ShotAngle::EyeLevel, false, None));
    assert!(!est(1000, 1000, (10, 10, 10, 10), 0.0, false, &[]).stated);
    // A centred product, middle of the frame: inconclusive.
    let centred = est_frac(0.35, 0.65, 0.35, 0.65);
    assert_eq!((centred.angle, centred.stated), (ShotAngle::EyeLevel, false));
    assert_eq!(centred.angle.bucket(), "eye_level");
    assert_eq!(ShotAngle::SteepDown.bucket(), "steep_down");
    assert_eq!(ShotAngle::LowUp.bucket(), "low_up");
}

fn photo(w: u32, h: u32) -> RgbaImage {
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            px.extend_from_slice(&[
                (30 + (x * 5 + y * 7) % 180) as u8,
                (50 + (y * 3) % 150) as u8,
                (70 + (x * 13) % 160) as u8,
                255,
            ]);
        }
    }
    RgbaImage::new(w, h, px)
}

fn mask(w: u32, h: u32, on: impl Fn(u32, u32) -> bool) -> GrayImage {
    GrayImage {
        width: w,
        height: h,
        pixels: (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .map(|(x, y)| if on(x, y) { 255 } else { 0 })
            .collect(),
        icc_profile: None,
    }
}

#[test]
fn estimate_from_a_real_mask_and_the_staged_composite_keeps_the_lock() {
    // A flat lay seen from above: wide, rows 0.2..0.75 of a 160x120
    // frame, 41% coverage (under the 45% surface line).
    let src = photo(160, 120);
    let m = mask(160, 120, |x, y| (20..140).contains(&x) && (24..90).contains(&y));
    let fg = extract_foreground(&src, &m);
    let g = staging::analyze_subject(&fg);
    let angle = estimate_shot_angle(&g);
    assert_eq!((angle.angle, angle.stated), (ShotAngle::SteepDown, true), "{angle:?}");
    // The estimate is report-only: the subject record (a receipt field)
    // carries no angle data.
    assert!(g.to_json().get("angle_lock").is_none());
    // The prompt the server builds for this draw.
    let prompt = plate_edit_prompt_locked("on a marble floor", g.fills_frame, Some(&angle));
    assert!(prompt.ends_with(&format!("{ANGLE_LOCK_CLAUSE} {STEEP_SENTENCE} {SHADOW_CLAUSE}")), "{prompt}");
    // Product pixels: the staged composite changes none of them.
    let plate = staging::finish_plate(&staging::fallback_plate(&src, &fg), &fg, &g);
    let (out, counts, _) = composite_images(&fg, &plate);
    assert_eq!(counts.changed_opaque_pixels, 0);
    for (i, a) in m.pixels.iter().enumerate() {
        if *a == 255 {
            assert_eq!(out.pixels[i * 4..i * 4 + 4], src.pixels[i * 4..i * 4 + 4]);
        }
    }
}

// --- the prompts -----------------------------------------------------------

#[test]
fn the_base_clause_is_the_owners_wording() {
    assert_eq!(
        ANGLE_LOCK_CLAUSE,
        "Keep the exact same camera angle and perspective as the original photo, same horizon position, same framing."
    );
    assert_eq!(
        ANGLE_PHRASE_STEEP_DOWN,
        "shot from standing height looking down at the surface at a steep angle"
    );
    assert_eq!(ANGLE_PHRASE_EYE_LEVEL, "eye-level shot");
    assert_eq!(ANGLE_PHRASE_LOW_UP, "shot from a low angle looking slightly up");
    assert_eq!(angle_lock_clause(None), ANGLE_LOCK_CLAUSE);
    assert_eq!(angle_lock_clause(Some(&unstated())), ANGLE_LOCK_CLAUSE);
    assert_eq!(
        angle_lock_clause(Some(&stated(ShotAngle::SteepDown))),
        format!("{ANGLE_LOCK_CLAUSE} {STEEP_SENTENCE}")
    );
}

#[test]
fn every_object_prompt_carries_the_clause_with_and_without_scene_text() {
    // No scene text: the probe's object wording, then the clause.
    let bare = plate_edit_prompt("", false);
    assert!(bare.starts_with(
        "Edit @Image1. It is a real photo in which one region has been filled flat grey."
    ));
    assert!(bare.ends_with(&format!("{ANGLE_LOCK_CLAUSE} {SHADOW_CLAUSE}")), "{bare}");
    assert_eq!(bare.matches(ANGLE_LOCK_CLAUSE).count(), 1);
    // Whitespace-only scene text counts as none.
    assert_eq!(plate_edit_prompt("   ", false), bare);
    // Scene text: styling first, then the clause last so the scene
    // cannot pull the camera; the scene gets a sentence end.
    let scene = plate_edit_prompt("on a beach at sunset", false);
    assert!(scene.contains("Styling only (keep the geometry): on a beach at sunset. Keep the exact"));
    assert!(scene.ends_with(&format!("{ANGLE_LOCK_CLAUSE} {SHADOW_CLAUSE}")), "{scene}");
    assert!(
        scene.find("on a beach").unwrap() < scene.find(ANGLE_LOCK_CLAUSE).unwrap()
    );
    // Scene text that already ends a sentence is not doubled up.
    let punct = plate_edit_prompt("A bright loft!", false);
    assert!(punct.contains("A bright loft! Keep the exact"), "{punct}");
}

#[test]
fn the_surface_wording_path_also_carries_the_base_clause() {
    for scene in ["", "a tidy back garden"] {
        let p = plate_edit_prompt(scene, true);
        assert!(p.contains("Continue the surroundings"), "{p}");
        assert!(p.ends_with(&format!("{ANGLE_LOCK_CLAUSE} {SHADOW_CLAUSE}")), "{p}");
        // The estimator never states an angle for a surface, so the
        // server's locked prompt is the same text.
        let lawn = est(1000, 1000, (0, 300, 1000, 1000), 0.7, true, &["left", "right", "bottom"]);
        assert_eq!(plate_edit_prompt_locked(scene, true, Some(&lawn)), p);
    }
    // And a stated angle rides on the surface wording too.
    let p = plate_edit_prompt_locked("", true, Some(&stated(ShotAngle::SteepDown)));
    assert!(p.contains("Continue the surroundings"));
    assert!(p.ends_with(&format!("{ANGLE_LOCK_CLAUSE} {STEEP_SENTENCE} {SHADOW_CLAUSE}")));
}

#[test]
fn stated_angles_add_their_phrase_and_the_default_adds_nothing() {
    for scene in ["", "studio white sweep"] {
        let steep = plate_edit_prompt_locked(scene, false, Some(&stated(ShotAngle::SteepDown)));
        assert!(steep.ends_with(&format!("{ANGLE_LOCK_CLAUSE} {STEEP_SENTENCE} {SHADOW_CLAUSE}")), "{steep}");
        let low = plate_edit_prompt_locked(scene, false, Some(&stated(ShotAngle::LowUp)));
        assert!(low.ends_with(&format!("{ANGLE_LOCK_CLAUSE} {LOW_SENTENCE} {SHADOW_CLAUSE}")), "{low}");
        // Unstated eye level: the base clause only, never an
        // "eye-level" claim the geometry did not support.
        let eye = plate_edit_prompt_locked(scene, false, Some(&unstated()));
        assert_eq!(eye, plate_edit_prompt(scene, false));
        assert!(!eye.contains("eye-level"));
    }
}

#[test]
fn generated_mode_turns_the_angle_into_steering_words() {
    let steep = generated_plate_scene("cozy loft", &stated(ShotAngle::SteepDown));
    assert!(steep.starts_with("cozy loft. Camera angle and perspective: "), "{steep}");
    assert!(steep.contains(ANGLE_PHRASE_STEEP_DOWN));
    // The default eye level is named here: a text-only plate must be
    // told some angle.
    let eye = generated_plate_scene("", &unstated());
    assert!(eye.starts_with("Camera angle and perspective: eye-level shot"), "{eye}");
    let low = generated_plate_scene("garage", &stated(ShotAngle::LowUp));
    assert!(low.contains(ANGLE_PHRASE_LOW_UP));
    // The legacy instruction wrapper is unchanged; the steer rides in
    // the scene text after it.
    let wrapped = wrap_plate_prompt(&steep);
    assert!(wrapped.starts_with("An empty scene photographed straight-on:"));
    assert!(wrapped.ends_with(&steep));
    // No reference photo in this mode, so the photo clause is not used.
    assert!(!steep.contains(ANGLE_LOCK_CLAUSE));
}
