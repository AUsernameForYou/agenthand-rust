//! Review aaa44dc defect 1: "no product pixel was sent" must hold at
//! every source size, not only when the edit size is a downscale.
//! Edit sizes are at least 1024 px, so a small web photo (300x225) is
//! UPSCALED to the reference; the Lanczos support then reaches 3 source
//! px = 3 x upscale reference px, past the old 8 px dilation, and the
//! inpaint spread whatever reached the ring through the hole.
//!
//! The proof here is differential: two sources that differ ONLY in
//! their product pixels (alpha > 0) must give byte-identical
//! references, for both fills, for upscaled, near-1:1 and downscaled
//! sources, with centred, edge-touching and frame-filling masks. And
//! outside the hole the reference is still exactly the Lanczos-resized
//! photo: the product's constant stand-in never bleeds into the ring
//! the inpaint reads (the dilation covers the resize support).

use agenthand::image::RgbaImage;
use agenthand::meta::closest_image_size;
use agenthand::resample::{resize_rgba, ResampleFilter};
use agenthand::staging::{edit_reference, ReferenceFill, REFERENCE_GREY};

/// A busy, photo-like texture. Red stays in 10..=90, so even with
/// Lanczos overshoot no resized pixel is the neutral grey fill.
fn photo(w: u32, h: u32, seed: u32) -> RgbaImage {
    let mut s = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            let base = (x * 7 + y * 3) % 60;
            px.extend_from_slice(&[
                (10 + base + (s & 15)) as u8,
                (60 + base + ((s >> 8) & 63)) as u8,
                (120 + base + ((s >> 16) & 63)) as u8,
                255,
            ]);
        }
    }
    RgbaImage::new(w, h, px)
}

fn with_alpha(src: &RgbaImage, alpha: impl Fn(u32, u32) -> u8) -> RgbaImage {
    let mut out = src.clone();
    for y in 0..src.height {
        for x in 0..src.width {
            out.pixels[((y * src.width + x) * 4 + 3) as usize] = alpha(x, y);
        }
    }
    out
}

/// `src` with every product pixel (alpha > 0 in `fg`) changed and
/// nothing else.
fn poison(src: &RgbaImage, fg: &RgbaImage) -> RgbaImage {
    let mut p = src.clone();
    for (q, f) in p.pixels.chunks_exact_mut(4).zip(fg.pixels.chunks_exact(4)) {
        if f[3] > 0 {
            q[0] = 255 - q[0];
            q[1] ^= 0x5A;
            q[2] = 255 - q[2];
        }
    }
    p
}

type Mask = Box<dyn Fn(u32, u32) -> u8>;

/// Centred disc with a soft (alpha 128) rim; a block touching the left
/// and bottom edges; a lawn touching left, right and bottom with the
/// top 10% open and a round bed cut out of it.
fn masks(w: u32, h: u32) -> Vec<(&'static str, Mask)> {
    let (wf, hf) = (w as f64, h as f64);
    vec![
        (
            "disc",
            Box::new(move |x, y| {
                let d = ((x as f64 - wf / 2.0).powi(2) + (y as f64 - hf / 2.0).powi(2)).sqrt();
                if d < hf / 4.0 {
                    255
                } else if d < hf / 4.0 + 1.5 {
                    128
                } else {
                    0
                }
            }),
        ),
        (
            "edge",
            Box::new(move |x, y| if x < w / 3 && y > h / 2 { 255 } else { 0 }),
        ),
        (
            "lawn",
            Box::new(move |x, y| {
                let d = ((x as f64 - wf * 0.5).powi(2) + (y as f64 - hf * 0.6).powi(2)).sqrt();
                if y < h / 10 || d < hf * 0.1 {
                    0
                } else {
                    255
                }
            }),
        ),
    ]
}

fn edit_size(w: u32, h: u32) -> (u32, u32) {
    let size = closest_image_size(Some(w), Some(h));
    let (a, b) = size.split_once('x').unwrap();
    (a.parse().unwrap(), b.parse().unwrap())
}

/// (pixels that differ, largest channel difference).
fn diff(a: &RgbaImage, b: &RgbaImage) -> (usize, u8) {
    let mut n = 0;
    let mut max = 0u8;
    for (p, q) in a.pixels.chunks_exact(4).zip(b.pixels.chunks_exact(4)) {
        if p != q {
            n += 1;
            max = max.max((0..4).map(|c| p[c].abs_diff(q[c])).max().unwrap());
        }
    }
    (n, max)
}

#[test]
fn product_pixels_never_reach_the_reference_at_any_source_size() {
    // Upscaled (5.1x, 3x, 2.6x, 1.5x) and downscaled (the 5712x4284
    // aspect at half size: 2856x2142 -> 1536x1024).
    let cases: [(u32, u32, &[&str]); 5] = [
        (300, 225, &["disc", "edge", "lawn"]),
        (512, 384, &["disc", "edge"]),
        (600, 450, &["disc"]),
        (1024, 768, &["disc", "edge"]),
        (2856, 2142, &["disc", "edge"]),
    ];
    let mut failures = Vec::new();
    for (w, h, wanted) in cases {
        let (ew, eh) = edit_size(w, h);
        let src = photo(w, h, w * 31 + h);
        for (name, mask) in masks(w, h) {
            if !wanted.contains(&name) {
                continue;
            }
            let fg = with_alpha(&src, &mask);
            let other = poison(&src, &fg);
            let plain = resize_rgba(&src, ew, eh, ResampleFilter::Lanczos);
            for fill in [ReferenceFill::InpaintDiffusion, ReferenceFill::Grey] {
                let a = edit_reference(&src, &fg, ew, eh, fill);
                let b = edit_reference(&other, &fg, ew, eh, fill);
                assert_eq!((a.image.width, a.image.height), (ew, eh));
                let (n, max) = diff(&a.image, &b.image);
                eprintln!(
                    "{w}x{h} {name} -> {ew}x{eh} {fill:?}: {n} reference px depend on product px (max |d| {max}), hole {}",
                    a.hole_pixels
                );
                if n != 0 {
                    failures.push(format!(
                        "{w}x{h} {name} -> {ew}x{eh} {fill:?}: {n} px differ, max {max}"
                    ));
                }
                if fill == ReferenceFill::Grey {
                    // Outside the hole (every non-grey pixel) the
                    // reference is exactly the resized photo: the
                    // dilation covers the resize support, so the
                    // product's stand-in never reaches the ring.
                    let mut ring = 0usize;
                    let mut hole = 0u64;
                    for (r, p) in a
                        .image
                        .pixels
                        .chunks_exact(4)
                        .zip(plain.pixels.chunks_exact(4))
                    {
                        if r[..3] == REFERENCE_GREY {
                            hole += 1;
                        } else if r[..3] != p[..3] {
                            ring += 1;
                        }
                    }
                    assert_eq!(
                        hole, a.hole_pixels,
                        "{w}x{h} {name}: grey count is the hole"
                    );
                    if ring != 0 {
                        failures.push(format!(
                            "{w}x{h} {name} -> {ew}x{eh}: {ring} px outside the hole differ from the resized photo"
                        ));
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "product pixels leaked:\n{}",
        failures.join("\n")
    );
}

/// The dilation widens with the upscale (max(8, ceil(3 x upscale) + 2))
/// and the receipt fields report the one used.
#[test]
fn dilation_covers_the_upscale_and_is_recorded() {
    use agenthand::staging::reference_dilate_px;
    assert_eq!(reference_dilate_px(5712, 4284, 1536, 1024), 8);
    assert_eq!(reference_dilate_px(1024, 768, 1536, 1024), 8);
    assert_eq!(reference_dilate_px(768, 512, 1536, 1024), 8);
    assert_eq!(reference_dilate_px(600, 450, 1536, 1024), 10);
    assert_eq!(reference_dilate_px(512, 384, 1536, 1024), 11);
    assert_eq!(reference_dilate_px(300, 225, 1536, 1024), 18);
    let src = photo(300, 225, 7);
    let fg = with_alpha(&src, |x, y| {
        u8::from((100..200).contains(&x) && (60..160).contains(&y)) * 255
    });
    for fill in [ReferenceFill::InpaintDiffusion, ReferenceFill::Grey] {
        let r = edit_reference(&src, &fg, 1536, 1024, fill);
        assert_eq!(r.dilate_px, 18);
        assert_eq!(r.fill_record()["dilate_px"], serde_json::json!(18));
    }
}
