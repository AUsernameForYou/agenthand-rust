//! Source-derived staging: synthetic masks for every subject shape the
//! live demo can hand draw (full-frame, edge-touching, multi-region,
//! thin, tiny, empty), the plate steps around them, and the request
//! plumbing that does not need the network. Every staged composite
//! must keep the Fidelity Lock count at 0 changed.

use agenthand::backdrop;
use agenthand::composite::{apply_contact_shadow, apply_edge_matte, composite_images};
use agenthand::image::{GrayImage, RgbaImage};
use agenthand::meta::{multipart_body, plate_edit_prompt, PlateEditConfig};
use agenthand::sam::{encode_raster, parse_sam_masks};
use agenthand::segmentation::{
    assess_mask, assess_mask_regions, extract_foreground, rejection_reason, union_masks,
};
use agenthand::staging::{self, SURFACE_NOTE};

/// A textured "photo" so fills, blurs and mattes have something to do.
fn photo(w: u32, h: u32) -> RgbaImage {
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            px.extend_from_slice(&[
                (40 + (x * 7 + y * 3) % 160) as u8,
                (60 + (y * 5) % 120) as u8,
                (90 + (x * 11) % 140) as u8,
                255,
            ]);
        }
    }
    RgbaImage::new(w, h, px)
}

fn mask(w: u32, h: u32, on: impl Fn(u32, u32) -> bool) -> GrayImage {
    let pixels = (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .map(|(x, y)| if on(x, y) { 255 } else { 0 })
        .collect();
    GrayImage {
        width: w,
        height: h,
        pixels,
        icc_profile: None,
    }
}

/// Run the whole code-side plate path (fallback plate, shadow, matte)
/// and composite; assert the lock holds. Returns the geometry.
fn stage_and_check(source: &RgbaImage, m: &GrayImage) -> staging::SubjectGeometry {
    let fg = extract_foreground(source, m);
    let geometry = staging::analyze_subject(&fg);
    staging::check_stageable(&geometry).unwrap();
    let plate = staging::finish_plate(&staging::fallback_plate(source, &fg), &fg, &geometry);
    assert_eq!((plate.width, plate.height), (fg.width, fg.height));
    let (out, counts, transformations) = composite_images(&fg, &plate);
    assert!(
        transformations.is_empty(),
        "plate is already at the product's frame"
    );
    assert_eq!(counts.changed_opaque_pixels, 0);
    assert_eq!(counts.opaque_pixels, geometry.opaque_pixels);
    for (i, a) in m.pixels.iter().enumerate() {
        if *a == 255 {
            assert_eq!(
                out.pixels[i * 4..i * 4 + 4],
                source.pixels[i * 4..i * 4 + 4]
            );
        }
    }
    geometry
}

#[test]
fn full_frame_mask_is_refused_and_near_full_surfaces_stage_without_panic() {
    let src = photo(60, 40);
    // 100%: the quality gate refuses it, with a plain reason.
    let full = mask(60, 40, |_, _| true);
    let qa = assess_mask(&full);
    assert_eq!(qa.rejection, Some("full_frame"));
    assert!(rejection_reason(&qa).contains("whole frame"));
    // Even if such a layer reached the plate code: no panic, no known
    // pixels to fill from, plate still built at the frame size.
    let fg = extract_foreground(&src, &full);
    let plate = staging::fallback_plate(&src, &fg);
    assert_eq!((plate.width, plate.height), (60, 40));
    // A lawn: 70% of the frame, touching left, bottom and right.
    let lawn = mask(60, 40, |_, y| y >= 12);
    assert!(assess_mask(&lawn).accepted);
    let g = stage_and_check(&src, &lawn);
    assert!(g.fills_frame);
    assert!(!g.shadow, "no contact shadow under a surface");
    assert_eq!(g.touches, vec!["left", "right", "bottom"]);
    assert!(plate_edit_prompt("", g.fills_frame).contains("Continue the surroundings"));
    assert!(SURFACE_NOTE.contains("replaced only what was around it"));
}

#[test]
fn edge_touching_masks_skip_the_shadow_only_at_the_bottom() {
    let src = photo(80, 50);
    // Cut by the bottom edge: cropping, not a surface; shadow skipped.
    let bottom = mask(80, 50, |x, y| (30..50).contains(&x) && y >= 30);
    let g = stage_and_check(&src, &bottom);
    assert_eq!(g.touches, vec!["bottom"]);
    assert!(!g.fills_frame && !g.shadow);
    assert!(plate_edit_prompt("sunny", true).contains("Styling only"));
    // Cut by the right edge only (a clipped car): object, shadow kept.
    let right = mask(80, 50, |x, y| x >= 60 && (10..30).contains(&y));
    let g = stage_and_check(&src, &right);
    assert_eq!(g.touches, vec!["right"]);
    assert!(!g.fills_frame && g.shadow);
    // A fence run spanning left to right is a surface even when thin
    // on coverage: surroundings wording, no shadow.
    let run = mask(80, 50, |_, y| (20..32).contains(&y));
    let g = stage_and_check(&src, &run);
    assert!(g.coverage < 0.45 && g.fills_frame && !g.shadow);
    // A free-standing object: object wording, shadow, 2 px matte.
    let object = mask(80, 50, |x, y| {
        (25..55).contains(&x) && (10..35).contains(&y)
    });
    let g = stage_and_check(&src, &object);
    assert!(!g.fills_frame && g.shadow && g.touches.is_empty());
    assert_eq!(g.matte_px, staging::MATTE_PX);
    assert!(plate_edit_prompt("", false)
        .contains("Fill the area where the object stood with the continuous surrounding surface."));
}

#[test]
fn multi_region_records_union_above_the_speck_floor() {
    // Frame 100x60 (floor 6 px): two 20x20 blobs and a 2x2 speck.
    let block = |w: usize, h: usize| encode_raster(&vec![1u8; w * h], w, h).unwrap();
    let text = format!(
        "<0f>0<|box;x1=5;y1=5;x2=24;y2=24;w=100;h=60|><|mask;x=0;y=0;data=20,20,{a}|>,\
         1<|box;x1=60;y1=30;x2=79;y2=49;w=100;h=60|><|mask;x=0;y=0;data=20,20,{a}|>,\
         2<|box;x1=90;y1=2;x2=91;y2=3;w=100;h=60|><|mask;x=0;y=0;data=2,2,{b}|>",
        a = block(20, 20),
        b = block(2, 2)
    );
    let masks = parse_sam_masks(&text, 100, 60).unwrap();
    assert_eq!(masks.len(), 3);
    let u = union_masks(&masks, 100, 60);
    assert_eq!(u.object_ids, vec!["0", "1"]);
    assert_eq!(u.dropped, 1);
    assert_eq!(u.bounds, (5, 5, 80, 50));
    let on = u.image.iter().filter(|&&v| v == 255).count();
    assert_eq!(on, 800);
    let union = GrayImage {
        width: 100,
        height: 60,
        pixels: u.image.clone(),
        icc_profile: None,
    };
    // One region expected: two equal blobs read as fragmented.
    assert_eq!(assess_mask(&union).rejection, Some("fragmented"));
    // Two records, two regions: accepted.
    let qa = assess_mask_regions(&union, u.object_ids.len());
    assert!(qa.accepted, "{:?}", qa.rejection);
    assert_eq!(qa.components, 2);
    stage_and_check(&photo(100, 60), &union);
    // Only specks: the largest stands alone so the gate can say why.
    let specks = parse_sam_masks(
        &format!(
            "<0f>7<|box;x1=1;y1=1;x2=2;y2=2;w=100;h=60|><|mask;x=0;y=0;data=2,2,{}|>",
            block(2, 2)
        ),
        100,
        60,
    )
    .unwrap();
    let u = union_masks(&specks, 100, 60);
    assert_eq!(
        (u.object_ids.clone(), u.dropped),
        (vec!["7".to_string()], 0)
    );
}

#[test]
fn thin_structures_cap_the_matte_at_one_pixel_and_are_not_eroded() {
    let src = photo(90, 60);
    // Fence: 3 px pickets every 9 px, plus a 2 px rail.
    let fence = mask(90, 60, |x, y| {
        (10..50).contains(&y) && (x % 9 < 3 || (28..30).contains(&y))
    });
    let g = stage_and_check(&src, &fence);
    assert!(g.thin, "thin fraction {}", g.thin_fraction);
    assert_eq!(g.matte_px, staging::MATTE_PX_THIN);
    // The matte touches only ring 1: a pixel two columns right of a
    // picket is left as the plate had it; one column right blends.
    let fg = extract_foreground(&src, &fence);
    let plate = RgbaImage::solid(90, 60, [250, 250, 250, 255]);
    let matted = apply_edge_matte(&plate, &fg, g.matte_px);
    let at = |img: &RgbaImage, x: u32, y: u32| img.pixel(x, y);
    assert_ne!(at(&matted, 3, 20), [250, 250, 250, 255]);
    assert_eq!(at(&matted, 4, 20), [250, 250, 250, 255]);
    // No product pixel is written by the matte (no erosion).
    for (i, a) in fence.pixels.iter().enumerate() {
        if *a == 255 {
            assert_eq!(
                matted.pixels[i * 4..i * 4 + 4],
                plate.pixels[i * 4..i * 4 + 4]
            );
        }
    }
    // A solid blob is not thin and gets the 2 px matte.
    let blob = mask(90, 60, |x, y| {
        (20..60).contains(&x) && (10..40).contains(&y)
    });
    let fg = extract_foreground(&src, &blob);
    let g = staging::analyze_subject(&fg);
    assert!(!g.thin);
    let matted = apply_edge_matte(&plate, &fg, g.matte_px);
    assert_ne!(at(&matted, 61, 20), [250, 250, 250, 255]);
    assert_ne!(at(&matted, 60, 20), [250, 250, 250, 255]);
    assert_eq!(at(&matted, 62, 20), [250, 250, 250, 255]);
}

#[test]
fn tiny_masks_are_refused_with_a_clear_reason() {
    let tiny = mask(100, 100, |x, y| x < 3 && y == 0);
    let qa = assess_mask(&tiny);
    assert_eq!(qa.rejection, Some("empty"));
    assert_eq!(qa.mask_pixels, 3);
    let reason = rejection_reason(&qa);
    assert!(reason.contains("too small (3 px"), "{reason}");
}

#[test]
fn empty_masks_are_refused_before_any_plate() {
    let src = photo(40, 30);
    let empty = mask(40, 30, |_, _| false);
    let qa = assess_mask(&empty);
    assert_eq!(qa.rejection, Some("empty"));
    assert!(rejection_reason(&qa).contains("no pixels"));
    let fg = extract_foreground(&src, &empty);
    let g = staging::analyze_subject(&fg);
    assert_eq!(g.opaque_pixels, 0);
    let err = staging::check_stageable(&g).unwrap_err();
    assert!(err.contains("nothing to stage"), "{err}");
    // A layer of only soft (edge-band) pixels has no product either.
    let soft = GrayImage {
        width: 40,
        height: 30,
        pixels: vec![128; 1200],
        icc_profile: None,
    };
    let g = staging::analyze_subject(&extract_foreground(&src, &soft));
    assert!(staging::check_stageable(&g).is_err());
}

#[test]
fn masked_reference_greys_every_visible_product_footprint() {
    let src = photo(300, 200);
    let m = mask(300, 200, |x, y| {
        (100..180).contains(&x) && (50..150).contains(&y) && (x + y) % 5 != 0
    });
    let fg = extract_foreground(&src, &m);
    let (reference, hole) = staging::masked_reference(&src, &fg, 150, 100);
    assert_eq!((reference.width, reference.height), (150, 100));
    assert!(hole > 0);
    for y in 0..200u32 {
        for x in 0..300u32 {
            if m.pixels[(y * 300 + x) as usize] != 0 {
                let p = reference.pixel(x * 150 / 300, y * 100 / 200);
                assert_eq!(p, [128, 128, 128, 255], "source ({x},{y})");
            }
        }
    }
    // Far from the product the photo is still there.
    assert_ne!(reference.pixel(5, 5), [128, 128, 128, 255]);
}

#[test]
fn contact_shadow_darkens_under_the_product_and_never_inside_it() {
    let src = photo(120, 80);
    let m = mask(120, 80, |x, y| {
        (40..80).contains(&x) && (20..60).contains(&y)
    });
    let fg = extract_foreground(&src, &m);
    let plate = RgbaImage::solid(120, 80, [240, 240, 240, 255]);
    let shadowed = apply_contact_shadow(&plate, &fg);
    let below = shadowed.pixel(60, 61);
    assert!(
        below[0] < 240,
        "ground under the product darkens: {below:?}"
    );
    assert_eq!(
        shadowed.pixel(60, 40),
        [240, 240, 240, 255],
        "alpha 0 at product pixels"
    );
    assert_eq!(shadowed.pixel(5, 5), [240, 240, 240, 255]);
}

#[test]
fn soft_mask_values_keep_straight_colour() {
    // The premultiply-against-black fringe: a half-covered pixel keeps
    // its colour and carries the coverage in alpha.
    let src = RgbaImage::solid(2, 1, [200, 100, 50, 255]);
    let m = GrayImage {
        width: 2,
        height: 1,
        pixels: vec![255, 128],
        icc_profile: None,
    };
    let fg = extract_foreground(&src, &m);
    assert_eq!(fg.pixel(0, 0), [200, 100, 50, 255]);
    assert_eq!(fg.pixel(1, 0), [200, 100, 50, 128]);
}

#[test]
fn edit_request_plumbing() {
    let cfg = PlateEditConfig::default();
    assert_eq!(cfg.path, "/images/edits");
    assert!(cfg.multipart);
    assert_eq!(cfg.size_value("1536x1024"), "1536x1024");
    let ratio = PlateEditConfig {
        size_as_ratio: true,
        ..PlateEditConfig::default()
    };
    assert_eq!(ratio.size_value("1536x1024"), "3:2");
    let (ctype, body) = multipart_body(
        "B",
        &[
            ("model", "muse-image-1.0".to_string()),
            ("size", "1536x1024".to_string()),
        ],
        "image",
        b"PNGDATA",
    );
    assert_eq!(ctype, "multipart/form-data; boundary=B");
    let text = String::from_utf8(body).unwrap();
    assert!(text.starts_with(
        "--B\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nmuse-image-1.0\r\n"
    ));
    assert!(text.contains("name=\"image\"; filename=\"image1.png\"\r\nContent-Type: image/png\r\n\r\nPNGDATA\r\n--B--\r\n"));
    assert!(plate_edit_prompt("", false).contains("@Image1"));
}

#[test]
fn draw_plate_mode_resolution() {
    assert_eq!(
        backdrop::resolve_draw_plate(Some("generated")).unwrap(),
        "generated"
    );
    assert_eq!(
        backdrop::resolve_draw_plate(Some("source_edit")).unwrap(),
        "source_edit"
    );
    assert!(backdrop::resolve_draw_plate(Some("bogus")).is_err());
    if std::env::var_os("AGENTHAND_DRAW_PLATE").is_none() {
        assert_eq!(backdrop::resolve_draw_plate(None).unwrap(), "source_edit");
    }
}
