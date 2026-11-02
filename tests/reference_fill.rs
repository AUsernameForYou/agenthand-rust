//! Reference fill (docs/REFERENCE-FILL.md): the source_edit edit
//! reference fills the dilated product footprint from the pixels
//! OUTSIDE it (`inpaint_diffusion`), so the edit model sees no
//! object-shaped grey silhouette. Proven here: no product pixel can
//! reach the reference (two sources that differ only in product pixels
//! give bit-identical references), only the hole changes, the fill is
//! smooth and meets its surround, and `AGENTHAND_REFERENCE_FILL=grey`
//! restores the legacy flat grey byte for byte.

use agenthand::image::{GrayImage, RgbaImage};
use agenthand::segmentation::extract_foreground;
use agenthand::staging::{
    edit_reference, masked_reference, ReferenceFill, REFERENCE_FILL_ENV, REFERENCE_GREY,
};
use serde_json::json;

/// Product colour never present in any background here.
const MAGENTA: [u8; 3] = [255, 0, 255];
const GREEN: [u8; 3] = [0, 255, 0];

/// A smooth background: a gentle linear gradient (harmonic, so the
/// exact fill of any hole is the gradient itself). Blue stays <= 100,
/// so no pixel is ever neutral grey or magenta.
fn smooth(w: u32, h: u32) -> RgbaImage {
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let (xf, yf) = (x as f32, y as f32);
            px.extend_from_slice(&[
                (60.0 + 0.4 * xf + 0.2 * yf).round() as u8,
                (90.0 + 0.3 * yf).round() as u8,
                (40.0 + 0.25 * xf).round() as u8,
                255,
            ]);
        }
    }
    RgbaImage::new(w, h, px)
}

/// A textured "photo" (as in tests/staging.rs); red <= 199 and blue
/// <= 229, so never magenta.
fn textured(w: u32, h: u32) -> RgbaImage {
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

/// A bike-like product on a 240x160 frame: two wheel discs, a top bar
/// and a post, plus a few thin spokes (thin structure).
fn bike_mask(w: u32, h: u32) -> GrayImage {
    let disc =
        |x: i32, y: i32, cx: i32, cy: i32, r: i32| (x - cx).pow(2) + (y - cy).pow(2) <= r * r;
    let pixels = (0..h as i32)
        .flat_map(|y| (0..w as i32).map(move |x| (x, y)))
        .map(|(x, y)| {
            let on = disc(x, y, 60, 110, 30)
                || disc(x, y, 180, 110, 30)
                || ((60..180).contains(&x) && (70..80).contains(&y))
                || ((170..178).contains(&x) && (30..80).contains(&y))
                || ((118..121).contains(&x) && (80..110).contains(&y));
            if on {
                255
            } else {
                0
            }
        })
        .collect();
    GrayImage {
        width: w,
        height: h,
        pixels,
        icc_profile: None,
    }
}

/// `background` with every product pixel painted `colour`.
fn with_product(background: &RgbaImage, mask: &GrayImage, colour: [u8; 3]) -> RgbaImage {
    let mut out = background.clone();
    for (p, &m) in out.pixels.chunks_exact_mut(4).zip(&mask.pixels) {
        if m != 0 {
            p[..3].copy_from_slice(&colour);
        }
    }
    out
}

/// The reference's hole, read off the legacy grey reference (the
/// backgrounds here never contain neutral grey).
fn hole_of(source: &RgbaImage, fg: &RgbaImage, w: u32, h: u32) -> Vec<bool> {
    let (grey, _) = masked_reference(source, fg, w, h);
    grey.pixels
        .chunks_exact(4)
        .map(|p| p[..3] == REFERENCE_GREY)
        .collect()
}

#[test]
fn inpaint_reference_has_no_product_pixel_leakage() {
    let (w, h) = (240, 160);
    let mask = bike_mask(w, h);
    for background in [textured(w, h), smooth(w, h)] {
        let magenta = with_product(&background, &mask, MAGENTA);
        let green = with_product(&background, &mask, GREEN);
        let fg_m = extract_foreground(&magenta, &mask);
        let fg_g = extract_foreground(&green, &mask);
        // Same size, a 2x downscale and a stretch (as 5712x4284 ->
        // 1536x1024 is).
        for (ow, oh) in [(240, 160), (120, 80), (180, 100)] {
            let a = edit_reference(&magenta, &fg_m, ow, oh, ReferenceFill::InpaintDiffusion);
            let b = edit_reference(&green, &fg_g, ow, oh, ReferenceFill::InpaintDiffusion);
            assert_eq!((a.image.width, a.image.height), (ow, oh));
            // The product pixels have no influence at all: every pixel
            // is derived from the pixels outside the footprint.
            assert_eq!(
                a.image.pixels, b.image.pixels,
                "{ow}x{oh}: product pixels leaked"
            );
            assert!(a.fill_iterations > 0);
            // The unique product colour (or anything near it) is absent.
            let near_magenta = a
                .image
                .pixels
                .chunks_exact(4)
                .filter(|p| p[0] > 230 && p[1] < 25 && p[2] > 230)
                .count();
            assert_eq!(near_magenta, 0, "{ow}x{oh}");
            // Same footprint as the grey reference; only the hole moved.
            let (grey, grey_hole) = masked_reference(&magenta, &fg_m, ow, oh);
            assert_eq!(a.hole_pixels, grey_hole);
            let hole = hole_of(&magenta, &fg_m, ow, oh);
            assert_eq!(hole.iter().filter(|&&x| x).count() as u64, a.hole_pixels);
            for (i, (pa, pg)) in a
                .image
                .pixels
                .chunks_exact(4)
                .zip(grey.pixels.chunks_exact(4))
                .enumerate()
            {
                if !hole[i] {
                    assert_eq!(pa, pg, "{ow}x{oh}: pixel {i} outside the hole changed");
                }
                assert_eq!(pa[3], 255);
            }
        }
    }
}

#[test]
fn inpaint_fill_is_smooth_meets_its_surround_and_is_deterministic() {
    let (w, h) = (240, 160);
    let mask = bike_mask(w, h);
    let background = smooth(w, h);
    let source = with_product(&background, &mask, MAGENTA);
    let fg = extract_foreground(&source, &mask);
    for (ow, oh) in [(240, 160), (180, 100)] {
        let r = edit_reference(&source, &fg, ow, oh, ReferenceFill::InpaintDiffusion);
        let again = edit_reference(&source, &fg, ow, oh, ReferenceFill::InpaintDiffusion);
        assert_eq!(r.image.pixels, again.image.pixels, "deterministic");
        assert_eq!(r.fill_iterations, again.fill_iterations);
        let hole = hole_of(&source, &fg, ow, oh);
        let (wu, hu) = (ow as usize, oh as usize);
        let px = |i: usize| -> [f64; 3] {
            let p = &r.image.pixels[i * 4..i * 4 + 3];
            [p[0] as f64, p[1] as f64, p[2] as f64]
        };
        // No grey left in the hole.
        let grey_in_hole = (0..wu * hu)
            .filter(|&i| hole[i] && r.image.pixels[i * 4..i * 4 + 3] == REFERENCE_GREY)
            .count();
        assert_eq!(grey_in_hole, 0, "{ow}x{oh}");
        // Smooth inside, continuous across the boundary.
        let (mut max_inside, mut cross, mut n_cross) = (0f64, 0f64, 0f64);
        for y in 0..hu {
            for x in 0..wu {
                let i = y * wu + x;
                for j in [(x + 1 < wu).then(|| i + 1), (y + 1 < hu).then(|| i + wu)]
                    .into_iter()
                    .flatten()
                {
                    let (a, b) = (px(i), px(j));
                    let d = (0..3).map(|c| (a[c] - b[c]).abs());
                    if hole[i] && hole[j] {
                        max_inside = d.fold(max_inside, f64::max);
                    } else if hole[i] != hole[j] {
                        cross += d.sum::<f64>() / 3.0;
                        n_cross += 1.0;
                    }
                }
            }
        }
        let boundary = cross / n_cross;
        eprintln!(
            "{ow}x{oh}: hole {} px, sweeps {}, max inside step {max_inside}, boundary mean |d| {boundary:.3}",
            r.hole_pixels, r.fill_iterations
        );
        assert!(
            max_inside <= 3.0,
            "{ow}x{oh}: max step inside the hole {max_inside}"
        );
        assert!(
            boundary < 3.0,
            "{ow}x{oh}: boundary mean |d| {boundary} >= 3/255"
        );
        // Converged: the harmonic fill of a linear gradient is the
        // gradient (what the photo would show with no product), read
        // at the reference pixel's centre.
        let mut err = 0f64;
        let mut n = 0f64;
        for i in (0..wu * hu).filter(|&i| hole[i]) {
            let a = px(i);
            let (x, y) = ((i % wu) as f64, (i / wu) as f64);
            let (sx, sy) = (w as f64 / ow as f64, h as f64 / oh as f64);
            let (bx, by) = ((x + 0.5) * sx - 0.5, (y + 0.5) * sy - 0.5);
            let want = [
                60.0 + 0.4 * bx + 0.2 * by,
                90.0 + 0.3 * by,
                40.0 + 0.25 * bx,
            ];
            err += (0..3).map(|c| (a[c] - want[c]).abs()).sum::<f64>() / 3.0;
            n += 1.0;
        }
        let mean_err = err / n;
        assert!(
            mean_err < 2.0,
            "{ow}x{oh}: mean error vs the true background {mean_err}"
        );
    }
}

#[test]
fn whole_frame_footprint_has_nothing_to_fill_from() {
    let src = textured(40, 30);
    let all = GrayImage {
        width: 40,
        height: 30,
        pixels: vec![255; 1200],
        icc_profile: None,
    };
    let fg = extract_foreground(&src, &all);
    let r = edit_reference(&src, &fg, 40, 30, ReferenceFill::InpaintDiffusion);
    assert_eq!(r.hole_pixels, 1200);
    assert_eq!(r.fill_iterations, 0);
    assert!(r
        .image
        .pixels
        .chunks_exact(4)
        .all(|p| p == [128, 128, 128, 255]));
}

/// Review aaa44dc defect 4: when the footprint covers the whole frame
/// the hole is flat grey, so the receipt fields say grey (not
/// `inpaint_diffusion`, not "filled from the photo pixels outside it").
#[test]
fn whole_frame_footprint_is_recorded_as_grey() {
    let src = textured(40, 30);
    let all = GrayImage {
        width: 40,
        height: 30,
        pixels: vec![255; 1200],
        icc_profile: None,
    };
    let fg = extract_foreground(&src, &all);
    let r = edit_reference(&src, &fg, 40, 30, ReferenceFill::InpaintDiffusion);
    assert!(r.image.pixels.chunks_exact(4).all(|p| p == [128, 128, 128, 255]));
    let record = r.fill_record();
    assert_eq!(record["fill"], json!("grey"), "{record}");
    assert_eq!(record["fill_rgb"], json!([128, 128, 128]));
    assert_eq!(record["fill_iterations"], json!(0));
    assert_eq!(record["fill_texture"], json!(null));
    let note = r.product_region_note();
    assert!(!note.contains("photo pixels outside it"), "{note}");
    assert!(note.contains("grey"), "{note}");
    assert!(note.contains("no product pixel was sent"), "{note}");
    // A frame with something outside the footprint is still the inpaint.
    let mut corner = all.clone();
    for y in 0..30 {
        for x in 0..40 {
            if x < 20 && y < 12 {
                corner.pixels[y * 40 + x] = 0;
            }
        }
    }
    let fg = extract_foreground(&src, &corner);
    let r = edit_reference(&src, &fg, 40, 30, ReferenceFill::InpaintDiffusion);
    assert_eq!(r.fill_record()["fill"], json!("inpaint_diffusion"));
    assert!(r.product_region_note().contains("photo pixels outside it"));
}

/// The only test in this binary that touches the environment.
#[test]
fn grey_env_restores_the_legacy_fill_and_the_record_says_which() {
    assert_eq!(REFERENCE_FILL_ENV, "AGENTHAND_REFERENCE_FILL");
    assert_eq!(ReferenceFill::parse(Some("grey")), ReferenceFill::Grey);
    assert_eq!(ReferenceFill::parse(Some(" GRAY ")), ReferenceFill::Grey);
    assert_eq!(
        ReferenceFill::parse(Some("")),
        ReferenceFill::InpaintDiffusion
    );
    assert_eq!(
        ReferenceFill::parse(Some("inpaint")),
        ReferenceFill::InpaintDiffusion
    );
    assert_eq!(ReferenceFill::parse(None), ReferenceFill::InpaintDiffusion);

    let (w, h) = (240, 160);
    let mask = bike_mask(w, h);
    let source = with_product(&textured(w, h), &mask, MAGENTA);
    let fg = extract_foreground(&source, &mask);
    let (legacy, legacy_hole) = masked_reference(&source, &fg, 180, 100);

    std::env::set_var(REFERENCE_FILL_ENV, "grey");
    let fill = ReferenceFill::from_env();
    assert_eq!(fill, ReferenceFill::Grey);
    let grey = edit_reference(&source, &fg, 180, 100, fill);
    assert_eq!(
        grey.image.pixels, legacy.pixels,
        "grey restores the old reference byte for byte"
    );
    assert_eq!(grey.hole_pixels, legacy_hole);
    assert_eq!(
        grey.fill_record(),
        json!({"fill": "grey", "fill_rgb": [128, 128, 128], "fill_iterations": 0, "fill_texture": null, "dilate_px": 8})
    );
    assert!(grey.product_region_note().contains("filled grey"));

    std::env::remove_var(REFERENCE_FILL_ENV);
    let fill = ReferenceFill::from_env();
    assert_eq!(fill, ReferenceFill::InpaintDiffusion);
    let inpaint = edit_reference(&source, &fg, 180, 100, fill);
    assert_eq!(inpaint.hole_pixels, legacy_hole);
    assert_ne!(inpaint.image.pixels, legacy.pixels);
    let record = inpaint.fill_record();
    assert_eq!(record["fill"], json!("inpaint_diffusion"));
    assert_eq!(record["fill_rgb"], json!(null));
    assert!(record["fill_iterations"].as_u64().unwrap() > 0);
    assert_eq!(record["fill_texture"]["kind"], json!("row_mirror_residual"));
    assert!(inpaint
        .product_region_note()
        .contains("no product pixel was sent"));
    assert!(!inpaint.product_region_note().contains("grey"));
}

/// The real hero run (IMG_1443): the grey rebuild matches the live
/// receipt's reference sha256 (the server path is mirrored exactly),
/// and the inpaint is bit-identical whether the product pixels are the
/// bike or flat magenta. Needs the run directory; run with
/// `cargo test --release --test reference_fill -- --ignored`.
#[test]
#[ignore]
fn real_bike_fixture_reference() {
    use agenthand::image::{decode_image, encode_png_rgba, sha256_hex};
    let run = std::env::var("AGENTHAND_REFILL_RUN")
        .unwrap_or_else(|_| "/home/claude/film/v8/film-assets/P04-P09/run-1443-fixed".to_string());
    let run = std::path::Path::new(&run);
    let source = decode_image(&std::fs::read(run.join("source.png")).unwrap())
        .unwrap()
        .into_rgba();
    let m = decode_image(&std::fs::read(run.join("mask.png")).unwrap())
        .unwrap()
        .into_rgba();
    let mask = GrayImage {
        width: m.width,
        height: m.height,
        pixels: m.pixels.chunks_exact(4).map(|p| p[0]).collect(),
        icc_profile: None,
    };
    drop(m);
    let fg = extract_foreground(&source, &mask);
    let size = agenthand::meta::closest_image_size(Some(source.width), Some(source.height));
    assert_eq!(size, "1536x1024");
    let receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(run.join("receipt.json")).unwrap()).unwrap();
    let grey = edit_reference(&source, &fg, 1536, 1024, ReferenceFill::Grey);
    assert_eq!(
        json!(sha256_hex(&encode_png_rgba(&grey.image))),
        receipt["backdrop"]["reference"]["sha256"]
    );
    assert_eq!(
        json!(grey.hole_pixels),
        receipt["backdrop"]["reference"]["hole_pixels"]
    );
    let inpaint = edit_reference(&source, &fg, 1536, 1024, ReferenceFill::InpaintDiffusion);
    let painted = with_product(&source, &mask, MAGENTA);
    drop(source);
    let fg_m = extract_foreground(&painted, &mask);
    let inpaint_m = edit_reference(&painted, &fg_m, 1536, 1024, ReferenceFill::InpaintDiffusion);
    assert_eq!(
        inpaint.image.pixels, inpaint_m.image.pixels,
        "product pixels leaked"
    );
    assert_eq!(inpaint.hole_pixels, grey.hole_pixels);
    let grey_in_hole = inpaint
        .image
        .pixels
        .chunks_exact(4)
        .zip(grey.image.pixels.chunks_exact(4))
        .filter(|(p, g)| g[..3] == REFERENCE_GREY && p[..3] == REFERENCE_GREY)
        .count();
    eprintln!(
        "IMG_1443 1536x1024: hole {} px, sweeps {}, grey 128 px left in hole {grey_in_hole}, sha256 {}",
        inpaint.hole_pixels,
        inpaint.fill_iterations,
        sha256_hex(&encode_png_rgba(&inpaint.image))
    );
}
