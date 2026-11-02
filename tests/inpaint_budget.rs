//! Review aaa44dc defect 2 (perf): the reference inpaint runs before
//! the Meta call, and on a frame-filling surface (a lawn, 90% of the
//! frame) its fine pyramid levels nearly saturated the 200-sweep cap:
//! +5.2 s over the grey reference at 24 MP. Fine levels (long side over
//! `INPAINT_FINE_LONG_SIDE`) now stop at `INPAINT_FINE_MAX_SWEEPS` or
//! once no value moves `INPAINT_FINE_TOLERANCE`; coarse levels still
//! converge, so the fill looks the same.
//!
//! Opt-in check on the real hero run (capped vs uncapped fill, timing):
//!
//! ```text
//! cargo test --release --test inpaint_budget -- --ignored --nocapture
//! ```

use agenthand::image::RgbaImage;
use agenthand::staging::{
    edit_reference, edit_reference_with_budget, EditReference, InpaintBudget, ReferenceFill,
    INPAINT_FINE_MAX_SWEEPS, INPAINT_MAX_SWEEPS,
};
use std::sync::Mutex;
use std::time::Instant;

/// The tests here run one at a time, so the wall-clock bound does not
/// measure the other test's sweeps.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// A busy, photo-like frame: high-contrast blocks of a few dozen px
/// (fence boards, bricks, foliage) over a gentle gradient, plus grain.
/// Strong mid-scale structure on the hole boundary is what kept the
/// fine levels sweeping on the real lawn.
fn photo(w: u32, h: u32, seed: u32) -> RgbaImage {
    let hash = |a: u32, b: u32| {
        let mut v = a.wrapping_mul(0x9E37_79B1) ^ b.wrapping_mul(0x85EB_CA77) ^ seed;
        v ^= v >> 15;
        v = v.wrapping_mul(0x2C1B_3C6D);
        v ^ (v >> 12)
    };
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let block = hash(x / 23, y / 31);
            let grain = hash(x, y);
            let (xf, yf) = (x as f32 / w as f32, y as f32 / h as f32);
            let ch = |k: u32, base: f32| {
                (base + ((block >> (8 * k)) & 0x7F) as f32 + ((grain >> (8 * k)) & 7) as f32)
                    .min(255.0) as u8
            };
            px.extend_from_slice(&[
                ch(0, 30.0 + 60.0 * xf),
                ch(1, 40.0 + 60.0 * yf),
                ch(2, 20.0 + 40.0 * (xf + yf)),
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

/// The review's synthetic lawn: opaque everywhere except the top 10%
/// and a round bed (touches left, right and bottom; ~90% of the frame).
fn lawn(w: u32, h: u32) -> impl Fn(u32, u32) -> u8 {
    let (cx, cy, r) = (w as f64 * 0.5, h as f64 * 0.6, h as f64 * 0.1);
    move |x, y| {
        let d = ((x as f64 - cx).powi(2) + (y as f64 - cy).powi(2)).sqrt();
        if y < h / 10 || d < r {
            0
        } else {
            255
        }
    }
}

#[test]
fn frame_filling_inpaint_finishes_under_a_bound() {
    let _serial = serial();
    // 3.1 MP 4:3 source sent at 1536x1024 (the edit size for 4:3).
    let (w, h) = (2048, 1536);
    let src = photo(w, h, 5);
    let fg = with_alpha(&src, lawn(w, h));
    let t = Instant::now();
    let grey = edit_reference(&src, &fg, 1536, 1024, ReferenceFill::Grey);
    let grey_s = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let r = edit_reference(&src, &fg, 1536, 1024, ReferenceFill::InpaintDiffusion);
    let inpaint_s = t.elapsed().as_secs_f64();
    let coverage = r.hole_pixels as f64 / (1536.0 * 1024.0);
    eprintln!(
        "lawn {w}x{h} -> 1536x1024: hole {:.1}%, sweeps {}, inpaint {inpaint_s:.2} s, grey {grey_s:.2} s",
        100.0 * coverage,
        r.fill_iterations
    );
    assert_eq!(r.hole_pixels, grey.hole_pixels);
    assert!(coverage > 0.85, "the mask fills the frame");
    // Deterministic cost: fine levels (each a quarter of the one above)
    // at most 30 sweeps over their hole, coarse levels (at most 1/64 of
    // the hole, then a quarter per level) at most 200:
    // 30 x 1.3125 + 200 x 0.0209 < 44 hole-sized sweeps.
    let bound = 44 * r.hole_pixels;
    let converged = edit_reference_with_budget(
        &src,
        &fg,
        1536,
        1024,
        ReferenceFill::InpaintDiffusion,
        InpaintBudget::CONVERGED,
    );
    eprintln!(
        "work: {} px updates ({:.1} x hole); uncapped schedule {} ({:.1} x hole, {} sweeps)",
        r.fill_work_px,
        r.fill_work_px as f64 / r.hole_pixels as f64,
        converged.fill_work_px,
        converged.fill_work_px as f64 / r.hole_pixels as f64,
        converged.fill_iterations
    );
    assert!(
        r.fill_work_px <= bound,
        "inpaint work {} > {bound}",
        r.fill_work_px
    );
    assert!(
        converged.fill_work_px > bound,
        "the uncapped schedule is what the cap removes"
    );
    let (mean, max) = hole_diff(&r, &converged);
    eprintln!("lawn vs uncapped, inside the hole: mean |d| {mean:.3}, max {max}");
    assert!(mean < 1.0, "lawn fill moved: mean |d| {mean}");
    // Wall clock only means something in an optimised build (the
    // review measured 5.2 s of inpaint here before the cap).
    if !cfg!(debug_assertions) {
        assert!(
            inpaint_s < 2.5,
            "frame-filling inpaint took {inpaint_s:.2} s (bound 2.5 s)"
        );
    }
}

/// (mean, max) absolute channel difference over the hole pixels (where
/// either reference differs from the shared surround, i.e. the hole:
/// both are built from the same footprint).
fn hole_diff(a: &EditReference, b: &EditReference) -> (f64, u8) {
    assert_eq!(a.hole_pixels, b.hole_pixels);
    let (mut sum, mut max) = (0f64, 0u8);
    for (p, q) in a
        .image
        .pixels
        .chunks_exact(4)
        .zip(b.image.pixels.chunks_exact(4))
    {
        for c in 0..3 {
            let d = p[c].abs_diff(q[c]);
            sum += d as f64;
            max = max.max(d);
        }
    }
    (sum / (3.0 * a.hole_pixels as f64), max)
}

/// A bike at the edit size: two spoked wheels (tyre ring, hub, 16
/// spokes), a diamond frame, fork, seat post and saddle, bars.
fn bike(w: u32) -> impl Fn(u32, u32) -> u8 {
    let s = w as f64 / 1536.0;
    let (rw, ry) = (190.0 * s, 700.0 * s);
    let wheels = [(400.0 * s, ry), (1130.0 * s, ry)];
    let seg = move |x: f64, y: f64, a: (f64, f64), b: (f64, f64), half: f64| {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let t = (((x - a.0) * dx + (y - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
        let (px, py) = (a.0 + t * dx - x, a.1 + t * dy - y);
        (px * px + py * py).sqrt() <= half
    };
    let (bb, seat, head) = (
        (720.0 * s, ry),
        (640.0 * s, 360.0 * s),
        (1060.0 * s, 380.0 * s),
    );
    move |x, y| {
        let (x, y) = (x as f64, y as f64);
        let mut on = false;
        for &(cx, cy) in &wheels {
            let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
            on |= (rw - 14.0 * s..=rw).contains(&d) || d <= 18.0 * s;
            for k in 0..16 {
                let a = k as f64 * std::f64::consts::PI / 8.0;
                on |= seg(
                    x,
                    y,
                    (cx, cy),
                    (cx + rw * a.cos(), cy + rw * a.sin()),
                    1.5 * s,
                );
            }
        }
        on |= seg(x, y, bb, seat, 12.0 * s)
            || seg(x, y, seat, head, 11.0 * s)
            || seg(x, y, bb, head, 13.0 * s)
            || seg(x, y, bb, wheels[0], 9.0 * s)
            || seg(x, y, seat, wheels[0], 8.0 * s)
            || seg(x, y, head, wheels[1], 10.0 * s)
            || seg(x, y, seat, (630.0 * s, 300.0 * s), 9.0 * s)
            || seg(
                x,
                y,
                (560.0 * s, 290.0 * s),
                (700.0 * s, 290.0 * s),
                14.0 * s,
            )
            || seg(x, y, head, (1080.0 * s, 300.0 * s), 9.0 * s)
            || seg(
                x,
                y,
                (1020.0 * s, 290.0 * s),
                (1150.0 * s, 300.0 * s),
                8.0 * s,
            );
        if on {
            255
        } else {
            0
        }
    }
}

/// The cap keeps the fill: on a bike at the edit size the capped
/// reference is within a small mean difference of the uncapped one
/// inside the hole (and identical outside it).
#[test]
fn bike_fill_matches_the_uncapped_fill() {
    let _serial = serial();
    let (w, h) = (1536, 1024);
    let src = photo(w, h, 11);
    let fg = with_alpha(&src, bike(w));
    let fill = ReferenceFill::InpaintDiffusion;
    let capped = edit_reference(&src, &fg, w, h, fill);
    let converged = edit_reference_with_budget(&src, &fg, w, h, fill, InpaintBudget::CONVERGED);
    let (mean, max) = hole_diff(&capped, &converged);
    eprintln!(
        "bike: hole {} px, sweeps {} vs {} uncapped, work {:.1} vs {:.1} x hole; inside the hole mean |d| {mean:.3}, max {max}",
        capped.hole_pixels,
        capped.fill_iterations,
        converged.fill_iterations,
        capped.fill_work_px as f64 / capped.hole_pixels as f64,
        converged.fill_work_px as f64 / converged.hole_pixels as f64,
    );
    assert!(capped.fill_iterations < converged.fill_iterations);
    assert!(mean < 0.5, "bike fill moved: mean |d| {mean}");
    assert!(max <= 16, "bike fill moved: max |d| {max}");
    assert_eq!(
        InpaintBudget::DEFAULT.fine_max_sweeps,
        INPAINT_FINE_MAX_SWEEPS
    );
    assert_eq!(InpaintBudget::DEFAULT.max_sweeps, INPAINT_MAX_SWEEPS);
}

/// The real IMG_1443 run (AGENTHAND_REFILL_RUN overrides the path):
/// the bike and the review's synthetic 90% lawn, capped vs uncapped.
#[test]
#[ignore]
fn real_bike_and_lawn_capped_vs_uncapped() {
    use agenthand::image::{decode_image, GrayImage};
    use agenthand::segmentation::extract_foreground;
    let _serial = serial();
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
    let (w, h) = (source.width, source.height);
    let bike_fg = extract_foreground(&source, &mask);
    let lawn_fg = with_alpha(&source, lawn(w, h));
    let fill = ReferenceFill::InpaintDiffusion;
    for (name, fg) in [("bike", &bike_fg), ("lawn", &lawn_fg)] {
        let t = Instant::now();
        let capped = edit_reference(&source, fg, 1536, 1024, fill);
        let capped_s = t.elapsed().as_secs_f64();
        let t = Instant::now();
        let converged =
            edit_reference_with_budget(&source, fg, 1536, 1024, fill, InpaintBudget::CONVERGED);
        let converged_s = t.elapsed().as_secs_f64();
        let (mean, max) = hole_diff(&capped, &converged);
        eprintln!(
            "IMG_1443 {name}: hole {} px; capped {capped_s:.2} s, {} sweeps, {:.1} x hole; uncapped {converged_s:.2} s, {} sweeps, {:.1} x hole; inside the hole mean |d| {mean:.3}, max {max}",
            capped.hole_pixels,
            capped.fill_iterations,
            capped.fill_work_px as f64 / capped.hole_pixels as f64,
            converged.fill_iterations,
            converged.fill_work_px as f64 / converged.hole_pixels as f64,
        );
        assert!(mean < 1.0, "{name}: mean |d| {mean}");
    }
}
