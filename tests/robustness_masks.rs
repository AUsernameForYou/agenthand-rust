//! Robustness QC for the mask -> cut -> plate -> composite path, driven
//! by synthetic masks shaped like the subjects a judge might name in a
//! live demo: a lawn (a surface filling most of the frame), a bike cut
//! by the bottom edge, something entering from the top-left corner, a
//! pile of separate stones, fence pickets 3 px wide, a speck, nothing.
//!
//! The pipeline under test is the offline part of the shipped path,
//! public API only, no network, no billing:
//! - the SAM-frame quality gate: `segmentation::assess_mask` (one
//!   record) and `assess_mask_regions` (a union of records);
//! - cut_out's post-SAM steps as `server::tool_cut_out` runs them:
//!   `map_mask_to_frame`, the mask stored with its provenance chunk,
//!   `extract_foreground`, `mask_overlay_image`, `review_mask`;
//! - draw's pre-spend guard `staging::check_stageable`, then the
//!   code-only plate draw falls back to when the edit call fails
//!   (`staging::fallback_plate` + `finish_plate`), composited by
//!   `composite::draw_product` and checked by `verify_receipt_from_disk`.
//!
//! Every test asserts the receipt invariant, recounted here rather
//! than trusted from `composite::measure`: every product pixel
//! (foreground alpha 255) in the delivered composite equals the source
//! pixel. Masks the gate rejects are still forced through cut and
//! composite, so the invariant holds for their shapes too.
//!
//! `#[ignore]` marks DESIRED behaviour the current code lacks; the
//! reason names the change that un-ignores it. (None at present: the
//! edge-cropping case was un-ignored once `analyze_subject` stopped
//! calling single- and adjacent-edge subjects surfaces.)

use agenthand::assets::{
    AssetStore, KIND_FOREGROUND, KIND_MASK, KIND_PLATE, KIND_SOURCE, MASK_PROVENANCE_PNG_KEY,
};
use agenthand::composite;
use agenthand::image::{encode_png_gray_with_text, GrayImage, RgbaImage};
use agenthand::segmentation::{
    self, MaskAssessment, MASK_SCALE_BILINEAR_THRESHOLD, MASK_SCALE_NONE,
};
use agenthand::staging::{self, SubjectGeometry};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, Ordering};

/// The full (source) frame every shape is defined in.
const W: u32 = 400;
const H: u32 = 300;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_store(tag: &str) -> AssetStore {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "agenthand-robustness-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    AssetStore::new(&dir)
}

/// A deterministic, high-variance opaque photo, so "equals the source
/// pixel" can never pass by accident against a smooth plate.
fn source_photo() -> RgbaImage {
    let mut pixels = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        for x in 0..W {
            pixels.extend_from_slice(&[
                ((x * 7 + y * 3) & 0xFF) as u8,
                (((x * 13) ^ (y * 5)) & 0xFF) as u8,
                ((x + 2 * y) & 0xFF) as u8,
                255,
            ]);
        }
    }
    RgbaImage::new(W, H, pixels)
}

/// Render a binary mask at (w, h). `shape` is defined in full-frame
/// pixel units, so one shape renders at the source frame and at a
/// smaller SAM working frame.
fn render(w: u32, h: u32, shape: impl Fn(f64, f64) -> bool) -> GrayImage {
    let sx = W as f64 / w as f64;
    let sy = H as f64 / h as f64;
    let mut pixels = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            let on = shape((x as f64 + 0.5) * sx, (y as f64 + 0.5) * sy);
            pixels.push(if on { 255 } else { 0 });
        }
    }
    GrayImage {
        width: w,
        height: h,
        pixels,
        icc_profile: None,
    }
}

/// The SAM working frame when the cap is a quarter of the source's
/// pixels (200x150): the downscale-and-map-back path cut_out takes for
/// every real phone photo.
fn sam_frame() -> (u32, u32) {
    segmentation::sam_frame_size(W, H, Some((W as u64 * H as u64) / 4))
}

fn on_count(mask: &GrayImage) -> u64 {
    mask.pixels.iter().filter(|&&v| v != 0).count() as u64
}

fn disc(x: f64, y: f64, cx: f64, cy: f64, r: f64) -> bool {
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

fn strings(review: &Value, field: &str) -> Vec<String> {
    review[field]
        .as_array()
        .unwrap_or_else(|| panic!("{field} is an array"))
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

fn touches(review: &Value) -> Vec<String> {
    strings(review, "touches_frame_edges")
}

fn warnings(review: &Value) -> Vec<String> {
    strings(review, "warnings")
}

/// One staged run: what cut_out stores and draw returns.
struct Staged {
    store: AssetStore,
    full_mask: GrayImage,
    review: Value,
    foreground_handle: String,
    geometry: SubjectGeometry,
    draw: Result<(String, Value), String>,
}

impl Staged {
    fn composite_handle(&self) -> &str {
        &self.draw.as_ref().expect("draw succeeded").0
    }

    fn receipt(&self) -> &Value {
        &self.draw.as_ref().expect("draw succeeded").1
    }

    fn verify(&self) -> Value {
        composite::verify_receipt_from_disk(&self.store, self.composite_handle(), None)
            .expect("the verifier runs")
    }
}

/// cut_out's post-SAM steps (server.rs tool_cut_out, after
/// run_attempts), then draw: the pre-spend guard, the code-only plate
/// (what draw uses when the edit call fails), and the composite. The
/// gate is NOT applied here; callers assert it separately.
fn stage(tag: &str, source: &RgbaImage, sam_mask: &GrayImage) -> Staged {
    let store = temp_store(tag);
    let source_handle = store
        .put_image(source, KIND_SOURCE, None)
        .expect("store source");
    let source_id = AssetStore::parse_handle(&source_handle).unwrap();

    let full_mask = segmentation::map_mask_to_frame(sam_mask, source.width, source.height);
    assert_eq!(
        (full_mask.width, full_mask.height),
        (source.width, source.height),
        "the mask is mapped onto the full source frame"
    );
    let scale_method = if (sam_mask.width, sam_mask.height) == (source.width, source.height) {
        MASK_SCALE_NONE
    } else {
        MASK_SCALE_BILINEAR_THRESHOLD
    };
    let provenance = json!({
        "source_width": source.width,
        "source_height": source.height,
        "sam_width": sam_mask.width,
        "sam_height": sam_mask.height,
        "scale_method": scale_method,
    });
    let mask_bytes =
        encode_png_gray_with_text(&full_mask, MASK_PROVENANCE_PNG_KEY, &provenance.to_string());
    let mask_handle = store
        .put_bytes(&mask_bytes, KIND_MASK, ".png", Some(&source_id))
        .expect("store mask");
    let mask_id = AssetStore::parse_handle(&mask_handle).unwrap();

    let foreground = segmentation::extract_foreground(source, &full_mask);
    let foreground_handle = store
        .put_image(&foreground, KIND_FOREGROUND, Some(&mask_id))
        .expect("store foreground");

    let overlay = segmentation::mask_overlay_image(source, &full_mask).expect("overlay renders");
    assert_eq!(
        (overlay.width, overlay.height),
        (source.width, source.height)
    );
    let review = segmentation::review_mask(&full_mask);

    let geometry = staging::analyze_subject(&foreground);
    let draw = staging::check_stageable(&geometry).and_then(|()| {
        let plate = staging::finish_plate(
            &staging::fallback_plate(source, &foreground),
            &foreground,
            &geometry,
        );
        let plate_handle = store.put_image(&plate, KIND_PLATE, Some(&source_id))?;
        composite::draw_product(
            &store,
            &foreground_handle,
            "a sunny backyard",
            Some(&plate_handle),
        )
    });
    Staged {
        store,
        full_mask,
        review,
        foreground_handle,
        geometry,
        draw,
    }
}

/// The receipt invariant, recounted from the delivered files: every
/// product pixel (foreground alpha 255) in the composite equals the
/// source pixel, and the receipt's counts agree. Returns the product
/// pixels checked.
fn assert_product_pixels_untouched(source: &RgbaImage, staged: &Staged) -> u64 {
    let foreground = staged.store.read_image(&staged.foreground_handle).unwrap();
    let out = staged.store.read_image(staged.composite_handle()).unwrap();
    assert_eq!(
        (out.width, out.height),
        (source.width, source.height),
        "the product canvas is never rescaled"
    );
    let mut checked = 0u64;
    for i in 0..(source.width * source.height) as usize {
        let j = i * 4;
        if foreground.pixels[j + 3] == 255 {
            checked += 1;
            assert_eq!(
                &out.pixels[j..j + 4],
                &source.pixels[j..j + 4],
                "product pixel at ({}, {}) differs from the source",
                i as u32 % source.width,
                i as u32 / source.width
            );
        }
    }
    let receipt = staged.receipt();
    assert_eq!(receipt["changed_opaque_pixels"], json!(0));
    assert_eq!(receipt["opaque_pixels"], json!(checked));
    // Binary masks: every on pixel is a product pixel, none dropped.
    assert_eq!(checked, on_count(&staged.full_mask));
    checked
}

/// A staged result a judge would see: product present, the plate
/// visible around it, and the receipt independently verified.
fn assert_honest_scene(source: &RgbaImage, staged: &Staged) {
    let checked = assert_product_pixels_untouched(source, staged);
    assert!(checked > 0, "a staged scene must contain the product");
    let foreground = staged.store.read_image(&staged.foreground_handle).unwrap();
    let out = staged.store.read_image(staged.composite_handle()).unwrap();
    let plate_visible = (0..(W * H) as usize).any(|i| {
        let j = i * 4;
        foreground.pixels[j + 3] == 0 && out.pixels[j..j + 4] != source.pixels[j..j + 4]
    });
    assert!(
        plate_visible,
        "the plate shows around the product (not the original photo handed back)"
    );
    let verify = staged.verify();
    assert_eq!(verify["ok"], json!(true), "verifier report: {verify}");
}

fn assert_gate(a: &MaskAssessment, accepted: bool, rejection: Option<&str>) {
    assert_eq!(a.accepted, accepted, "gate verdict");
    assert_eq!(a.rejection, rejection, "gate rejection code");
}

// ---------------------------------------------------------------------------
// 1. A mask filling more than 45% of the frame: the lawn.
// ---------------------------------------------------------------------------

/// Lawn from 42% of the way down to the bottom, full width, with a
/// patio set standing on it (a hole: holes never split a region).
fn lawn(x: f64, y: f64) -> bool {
    let hole = ((x - 200.0) / 48.0).powi(2) + ((y - 210.0) / 30.0).powi(2) <= 1.0;
    y >= 126.0 && !hole
}

#[test]
fn large_mask_over_45_percent_stages_as_a_surface_without_touching_product_pixels() {
    let source = source_photo();
    for (w, h) in [(W, H), sam_frame()] {
        let mask = render(w, h, lawn);
        let a = segmentation::assess_mask(&mask);
        assert!(a.coverage > 0.45, "lawn coverage {}", a.coverage);
        assert_gate(&a, true, None);
        assert_eq!(a.components, 1);

        let staged = stage("lawn", &source, &mask);
        assert!(segmentation::assess_mask(&staged.full_mask).coverage > 0.45);
        assert_eq!(touches(&staged.review), ["left", "right", "bottom"]);
        // Reported as a surface (draw's subject_note), and no contact
        // shadow is invented under it.
        assert!(staged.geometry.fills_frame);
        assert!(!staged.geometry.shadow);
        assert_honest_scene(&source, &staged);
    }

    // A "subject" that is the whole photo is refused, in plain words.
    let whole = render(W, H, |x, y| !(x < 4.0 && y < 4.0));
    let a = segmentation::assess_mask(&whole);
    assert!(a.coverage >= segmentation::MAX_COVERAGE);
    assert_gate(&a, false, Some("full_frame"));
    assert!(segmentation::rejection_reason(&a).contains("whole frame"));
    // Forced through anyway, the near-hole-only fallback plate still
    // builds and the lock still holds.
    let staged = stage("whole", &source, &whole);
    assert_product_pixels_untouched(&source, &staged);
}

// ---------------------------------------------------------------------------
// 2. A mask touching the bottom edge: a bike with its wheels cropped.
// ---------------------------------------------------------------------------

fn bike_on_bottom_edge(x: f64, y: f64) -> bool {
    disc(x, y, 130.0, 272.0, 40.0)
        || disc(x, y, 270.0, 272.0, 40.0)
        || ((130.0..=270.0).contains(&x) && (225.0..=240.0).contains(&y))
        || ((190.0..=200.0).contains(&x) && (180.0..=240.0).contains(&y))
}

#[test]
fn mask_touching_bottom_edge_keeps_the_edge_and_every_product_pixel() {
    let source = source_photo();
    for (w, h) in [(W, H), sam_frame()] {
        let mask = render(w, h, bike_on_bottom_edge);
        let a = segmentation::assess_mask(&mask);
        assert_gate(&a, true, None);
        assert_eq!(a.components, 1);

        let staged = stage("bottom", &source, &mask);
        // Mapping from the SAM frame must not peel the mask off the
        // edge it was cut against.
        let last_row = &staged.full_mask.pixels[((H - 1) * W) as usize..];
        assert!(
            last_row.iter().any(|&v| v != 0),
            "bottom row lost ({w}x{h})"
        );
        assert_eq!(touches(&staged.review), ["bottom"]);
        assert!(warnings(&staged.review).is_empty());
        // The frame cuts it at the ground: no invented contact shadow.
        assert!(!staged.geometry.shadow);
        assert_honest_scene(&source, &staged);
    }
}

// ---------------------------------------------------------------------------
// 3. A mask touching the left and top edges: a branch entering the frame.
// ---------------------------------------------------------------------------

fn corner_object(x: f64, y: f64) -> bool {
    (x < 150.0 && y < 110.0) || disc(x, y, 150.0, 110.0, 50.0)
}

#[test]
fn mask_touching_left_and_top_edges_keeps_both_edges_and_every_product_pixel() {
    let source = source_photo();
    for (w, h) in [(W, H), sam_frame()] {
        let mask = render(w, h, corner_object);
        let a = segmentation::assess_mask(&mask);
        assert_gate(&a, true, None);
        assert_eq!(a.components, 1);

        let staged = stage("corner", &source, &mask);
        let full = &staged.full_mask;
        assert!(full.pixels[0] != 0, "top-left pixel lost ({w}x{h})");
        assert!((0..W as usize).any(|x| full.pixels[x] != 0), "top row lost");
        assert!(
            (0..H as usize).any(|y| full.pixels[y * W as usize] != 0),
            "left column lost"
        );
        assert_eq!(touches(&staged.review), ["left", "top"]);
        assert!(warnings(&staged.review).is_empty());
        assert_honest_scene(&source, &staged);
    }
}

/// Touching one edge, or two adjacent edges, is cropping, not a
/// surface: `analyze_subject` sets `fills_frame` only for coverage over
/// `FILLS_FRAME_COVERAGE` or a subject spanning opposite frame edges.
/// Otherwise a bike cut by the bottom edge (about 8% of the frame)
/// would get draw's `subject_note` "subject is a surface or fills the
/// frame..." and the surface wording in the plate edit prompt: the
/// tool would misreport what it staged. The lawn must stay a surface.
#[test]
fn edge_cropped_objects_are_not_reported_as_surfaces() {
    let source = source_photo();
    for (tag, shape) in [
        ("bike", &bike_on_bottom_edge as &dyn Fn(f64, f64) -> bool),
        ("corner", &corner_object),
    ] {
        let staged = stage(tag, &source, &render(W, H, shape));
        assert_product_pixels_untouched(&source, &staged);
        assert!(
            staged.geometry.coverage < 0.2,
            "{tag} coverage {}",
            staged.geometry.coverage
        );
        assert!(
            !staged.geometry.fills_frame,
            "{tag} (touches {:?}) is reported as a surface",
            staged.geometry.touches
        );
    }
    let staged = stage("lawn-still", &source, &render(W, H, lawn));
    assert!(staged.geometry.fills_frame, "the lawn is still a surface");
}

// ---------------------------------------------------------------------------
// 4. A multi-region mask: five separate stones.
// ---------------------------------------------------------------------------

const STONES: [(f64, f64); 5] = [
    (60.0, 200.0),
    (140.0, 230.0),
    (210.0, 190.0),
    (290.0, 240.0),
    (350.0, 200.0),
];

fn five_stones(x: f64, y: f64) -> bool {
    STONES.iter().any(|&(cx, cy)| disc(x, y, cx, cy, 22.0))
}

/// Five stones as five SAM records (one per stone, merged by
/// union_masks) pass the gate; the same five blobs inside ONE record
/// are a shattered selection and fail it, with honest numbers. Either
/// way, forced through, every stone keeps every product pixel.
#[test]
fn five_separate_blobs_gate_on_record_count_and_lock_every_region() {
    let source = source_photo();
    let mask = render(W, H, five_stones);

    let one_record = segmentation::assess_mask(&mask);
    assert_gate(&one_record, false, Some("fragmented"));
    assert_eq!(one_record.components, 5);
    assert!(
        (one_record.dominant_fraction - 0.2).abs() < 0.01,
        "dominant_fraction {}",
        one_record.dominant_fraction
    );
    assert!(segmentation::rejection_reason(&one_record).contains("fragmented"));

    let five_records = segmentation::assess_mask_regions(&mask, 5);
    assert_gate(&five_records, true, None);
    assert_eq!(five_records.components, 5);

    let staged = stage("stones", &source, &mask);
    let fg = staged.store.read_image(&staged.foreground_handle).unwrap();
    for &(cx, cy) in &STONES {
        assert_eq!(
            fg.pixel(cx as u32, cy as u32)[3],
            255,
            "stone at ({cx}, {cy}) reached the cut-out"
        );
    }
    assert_honest_scene(&source, &staged);
}

// ---------------------------------------------------------------------------
// 5. A thin mask: 3 px vertical strokes like fence pickets.
// ---------------------------------------------------------------------------

/// A fence section: 17 pickets, each exactly 3 px wide at the source
/// frame, joined by two rails (one connected region).
fn fence_section(x: f64, y: f64) -> bool {
    let picket =
        (40.0..363.0).contains(&x) && (x - 40.0) % 20.0 < 3.0 && (80.0..260.0).contains(&y);
    let rail =
        (40.0..363.0).contains(&x) && ((120.0..126.0).contains(&y) || (210.0..216.0).contains(&y));
    picket || rail
}

/// Eight free-standing posts, 3 px wide, no rail.
fn loose_pickets(x: f64, y: f64) -> bool {
    (40.0..323.0).contains(&x) && (x - 40.0) % 40.0 < 3.0 && (80.0..260.0).contains(&y)
}

#[test]
fn thin_three_pixel_pickets_lose_no_stroke_and_lock_every_pixel() {
    let source = source_photo();

    // Joined pickets: one region, accepted, flagged for review.
    let fence = render(W, H, fence_section);
    let picket_columns = (0..W).filter(|&x| fence.pixels[(100 * W + x) as usize] != 0);
    assert_eq!(picket_columns.count(), 17 * 3, "17 pickets, 3 px each");
    let a = segmentation::assess_mask(&fence);
    assert_gate(&a, true, None);
    assert_eq!(a.components, 1);
    let staged = stage("fence", &source, &fence);
    assert!(staged.review["bbox_fill"].as_f64().unwrap() < 0.25);
    assert!(
        warnings(&staged.review)
            .iter()
            .any(|w| w.contains("frame-like")),
        "a fence section is flagged for review: {:?}",
        warnings(&staged.review)
    );
    assert!(staged.geometry.thin, "pickets read as thin structure");
    assert_eq!(staged.receipt()["edge_band_pixels"], json!(0));
    assert_honest_scene(&source, &staged);

    // Loose posts: eight regions. One record holding all eight is
    // shattered; eight records (one per post) pass. Forced through,
    // the lock holds on every 3 px stroke.
    let posts = render(W, H, loose_pickets);
    let a = segmentation::assess_mask(&posts);
    assert_gate(&a, false, Some("fragmented"));
    assert_eq!(a.components, 8);
    assert_gate(&segmentation::assess_mask_regions(&posts, 8), true, None);
    let staged = stage("posts", &source, &posts);
    assert_honest_scene(&source, &staged);

    // Through the SAM working frame the posts are 1 px wide; mapping
    // back to the source frame must not erase or merge any of them.
    let (sw, sh) = sam_frame();
    let small = render(sw, sh, loose_pickets);
    assert_eq!(segmentation::assess_mask(&small).components, 8);
    let staged = stage("posts-sam", &source, &small);
    assert_eq!(segmentation::assess_mask(&staged.full_mask).components, 8);
    for i in 0..8u32 {
        let x = 41 + 40 * i;
        assert!(
            (x - 2..=x + 2).any(|xx| staged.full_mask.pixels[(150 * W + xx) as usize] != 0),
            "post {i} vanished in the SAM-frame round trip"
        );
    }
    assert_honest_scene(&source, &staged);
}

// ---------------------------------------------------------------------------
// 6. A tiny mask: under 0.05% of the frame.
// ---------------------------------------------------------------------------

fn speck(x: f64, y: f64) -> bool {
    (300.0..307.0).contains(&x) && (60.0..67.0).contains(&y)
}

/// Refused by the gate, and the refusal says "too small" with the real
/// pixel count (the wire code stays "empty"; the text tells the two
/// apart), so nobody reports "SAM found nothing" when it found a speck.
#[test]
fn tiny_mask_under_0_05_percent_is_refused_as_too_small_and_still_locks_if_forced() {
    let source = source_photo();
    let mask = render(W, H, speck);
    let a = segmentation::assess_mask(&mask);
    assert_eq!(a.mask_pixels, 49);
    assert!(a.coverage < 0.0005, "coverage {}", a.coverage);
    assert_gate(&a, false, Some("empty"));
    assert_eq!(a.components, 1, "the gate still measured what SAM found");
    let reason = segmentation::rejection_reason(&a);
    assert!(
        reason.contains("too small") && reason.contains("49 px"),
        "reason: {reason}"
    );

    let staged = stage("speck", &source, &mask);
    assert_eq!(assert_product_pixels_untouched(&source, &staged), 49);
    assert_eq!(staged.verify()["ok"], json!(true));
}

// ---------------------------------------------------------------------------
// 7. An empty mask.
// ---------------------------------------------------------------------------

/// Never panics anywhere on the way, and draw refuses before any
/// plate exists: no empty plate, no receipt over zero pixels.
#[test]
fn empty_mask_never_panics_and_draw_refuses_before_any_plate() {
    let source = source_photo();
    let (sw, sh) = sam_frame();
    for mask in [render(W, H, |_, _| false), render(sw, sh, |_, _| false)] {
        let a = segmentation::assess_mask(&mask);
        assert_gate(&a, false, Some("empty"));
        assert_eq!((a.mask_pixels, a.components), (0, 0));
        assert_eq!(a.coverage, 0.0);
        assert!(segmentation::rejection_reason(&a).contains("no pixels"));

        let staged = stage("empty", &source, &mask);
        assert_eq!(on_count(&staged.full_mask), 0);
        assert_eq!(staged.review["bbox"], Value::Null);
        assert_eq!(staged.review["bbox_fill"], Value::Null);
        assert!(touches(&staged.review).is_empty());
        let overlay = segmentation::mask_overlay_image(&source, &staged.full_mask).unwrap();
        assert_eq!(overlay.pixels, source.pixels, "nothing outlined");

        assert_eq!(staged.geometry.opaque_pixels, 0);
        let refusal = staged.draw.as_ref().expect_err("draw refuses");
        assert!(refusal.contains("nothing to stage"), "refusal: {refusal}");
        // Receipt invariant: nothing was composited or stored, so no
        // product pixel could change and no receipt vouches for zero.
        let sidecars: Vec<String> = std::fs::read_dir(&staged.store.assets_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter_map(|p| std::fs::read_to_string(p).ok())
            .collect();
        assert_eq!(sidecars.len(), 3, "source, mask and cut-out only");
        assert!(sidecars.iter().all(|s| !s.contains("\"composite\"")));
        assert!(sidecars.iter().all(|s| !s.contains("background_plate")));
    }

    // Defence in depth: if a caller skipped the guard and composited
    // anyway, the verifier still refuses a receipt over zero pixels.
    let store = temp_store("empty-unguarded");
    let src = store.put_image(&source, KIND_SOURCE, None).unwrap();
    let blank = RgbaImage::solid(W, H, [0, 0, 0, 0]);
    let fg = store
        .put_image(
            &blank,
            KIND_FOREGROUND,
            Some(&AssetStore::parse_handle(&src).unwrap()),
        )
        .unwrap();
    if let Ok((handle, receipt)) = composite::draw_product(&store, &fg, "unguarded", None) {
        assert_eq!(receipt["opaque_pixels"], json!(0));
        assert_eq!(receipt["changed_opaque_pixels"], json!(0));
        let verify = composite::verify_receipt_from_disk(&store, &handle, None).unwrap();
        assert_eq!(verify["ok"], json!(false));
        assert_eq!(verify["checks"]["positive_coverage"], json!(false));
    }
}
