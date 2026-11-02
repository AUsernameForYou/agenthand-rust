//! Reference-fill fixture (docs/REFERENCE-FILL.md): build the
//! source_edit edit reference for a real run directory (source.png +
//! mask.png) with the inpaint fill, at the full source size and at the
//! edit size the server sends (`meta::closest_image_size`, the same
//! `staging::edit_reference` call), plus the legacy grey reference and
//! a side-by-side compare (grey | inpaint) at the edit size.
//!
//! Usage: reference_fill <run_dir> <out_dir>
//! Writes reference_inpaint.png (full size),
//! reference_inpaint_<WxH>.png, reference_grey_<WxH>.png and
//! reference_compare.jpg into <out_dir>; prints one JSON object.

use agenthand::image::{decode_image, encode_png_rgba, sha256_hex, GrayImage, RgbaImage};
use agenthand::meta::closest_image_size;
use agenthand::segmentation::extract_foreground;
use agenthand::staging::{edit_reference, ReferenceFill};
use serde_json::json;
use std::time::Instant;

fn load_mask(bytes: &[u8]) -> GrayImage {
    let rgba = decode_image(bytes).expect("mask decodes").into_rgba();
    GrayImage {
        width: rgba.width,
        height: rgba.height,
        pixels: rgba.pixels.chunks_exact(4).map(|p| p[0]).collect(),
        icc_profile: None,
    }
}

/// Mean |a - b| (0-255, per channel) over 4-neighbour pairs that cross
/// the hole boundary (one pixel in the hole, one outside), and the
/// same over neighbour pairs fully outside the hole as the baseline.
fn boundary_stats(img: &RgbaImage, hole: &[bool]) -> (f64, f64) {
    let (w, h) = (img.width as usize, img.height as usize);
    let (mut cross, mut nc, mut base, mut nb) = (0f64, 0f64, 0f64, 0f64);
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            for j in [(x + 1 < w).then(|| i + 1), (y + 1 < h).then(|| i + w)]
                .into_iter()
                .flatten()
            {
                let d: f64 = (0..3)
                    .map(|c| (img.pixels[i * 4 + c] as f64 - img.pixels[j * 4 + c] as f64).abs())
                    .sum::<f64>()
                    / 3.0;
                if hole[i] != hole[j] {
                    cross += d;
                    nc += 1.0;
                } else if !hole[i] {
                    base += d;
                    nb += 1.0;
                }
            }
        }
    }
    (cross / nc.max(1.0), base / nb.max(1.0))
}

/// The reference pixels that differ from the resized source: the hole.
fn hole_of(reference: &RgbaImage, grey: &RgbaImage) -> Vec<bool> {
    grey.pixels
        .chunks_exact(4)
        .zip(reference.pixels.chunks_exact(4))
        .map(|(g, _)| g[..3] == [128, 128, 128])
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let run = std::path::Path::new(
        args.get(1)
            .expect("usage: reference_fill <run_dir> <out_dir>"),
    );
    let out = std::path::Path::new(
        args.get(2)
            .expect("usage: reference_fill <run_dir> <out_dir>"),
    );
    std::fs::create_dir_all(out).unwrap();
    let source = decode_image(&std::fs::read(run.join("source.png")).unwrap())
        .unwrap()
        .into_rgba();
    let mask = load_mask(&std::fs::read(run.join("mask.png")).unwrap());
    let fg = extract_foreground(&source, &mask);
    drop(mask);
    let size = closest_image_size(Some(source.width), Some(source.height));
    let (ew, eh) = size
        .split_once('x')
        .map(|(a, b)| (a.parse::<u32>().unwrap(), b.parse::<u32>().unwrap()))
        .unwrap();

    // Edit size: exactly what the server builds and sends.
    let t = Instant::now();
    let inpaint = edit_reference(&source, &fg, ew, eh, ReferenceFill::InpaintDiffusion);
    let inpaint_ms = t.elapsed().as_millis();
    let grey = edit_reference(&source, &fg, ew, eh, ReferenceFill::Grey);
    let inpaint_png = encode_png_rgba(&inpaint.image);
    let grey_png = encode_png_rgba(&grey.image);
    let inpaint_edit_path = out.join(format!("reference_inpaint_{size}.png"));
    let grey_edit_path = out.join(format!("reference_grey_{size}.png"));
    std::fs::write(&inpaint_edit_path, &inpaint_png).unwrap();
    std::fs::write(&grey_edit_path, &grey_png).unwrap();
    let hole = hole_of(&inpaint.image, &grey.image);
    let (cross, baseline) = boundary_stats(&inpaint.image, &hole);
    let grey_left_in_hole = inpaint
        .image
        .pixels
        .chunks_exact(4)
        .zip(&hole)
        .filter(|(p, &h)| h && p[..3] == [128, 128, 128])
        .count();

    // Compare: grey | inpaint, side by side at the edit size.
    let mut side = image::RgbImage::new(ew * 2, eh);
    for (k, img) in [&grey.image, &inpaint.image].into_iter().enumerate() {
        for y in 0..eh {
            for x in 0..ew {
                let p = img.pixel(x, y);
                side.put_pixel(x + k as u32 * ew, y, image::Rgb([p[0], p[1], p[2]]));
            }
        }
    }
    let compare_path = out.join("reference_compare.jpg");
    let mut jpg = std::io::BufWriter::new(std::fs::File::create(&compare_path).unwrap());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpg, 90)
        .encode_image(&side)
        .unwrap();
    drop(jpg);

    // Full source size, same call.
    let t = Instant::now();
    let full = edit_reference(
        &source,
        &fg,
        source.width,
        source.height,
        ReferenceFill::InpaintDiffusion,
    );
    let full_ms = t.elapsed().as_millis();
    let full_png = encode_png_rgba(&full.image);
    let full_path = out.join("reference_inpaint.png");
    std::fs::write(&full_path, &full_png).unwrap();

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "run_dir": run,
            "source_size": [source.width, source.height],
            "edit_size": size,
            "edit": {
                "inpaint_png": inpaint_edit_path,
                "inpaint_sha256": sha256_hex(&inpaint_png),
                "grey_png": grey_edit_path,
                "grey_sha256": sha256_hex(&grey_png),
                "hole_pixels": inpaint.hole_pixels,
                "hole_pixels_grey": grey.hole_pixels,
                "fill_iterations": inpaint.fill_iterations,
                "fill_ms": inpaint_ms,
                "fill_record": inpaint.fill_record(),
                "boundary_mean_abs_diff": cross,
                "outside_neighbour_mean_abs_diff": baseline,
                "grey_128_pixels_in_hole": grey_left_in_hole,
            },
            "full": {
                "png": full_path,
                "sha256": sha256_hex(&full_png),
                "hole_pixels": full.hole_pixels,
                "fill_iterations": full.fill_iterations,
                "fill_ms": full_ms,
            },
            "compare_jpg": compare_path,
        }))
        .unwrap()
    );
}
