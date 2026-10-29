//! Byte-exact port of Pillow 13.0.0's resampling for 8-bit images
//! (`src/libImaging/Resample.c`, the `_8bpc` paths) plus `ImageOps.fit`.
//!
//! Why byte-exact matters: the Fidelity Lock fits the generated plate
//! to the product's dimensions with `ImageOps.fit` (BICUBIC). The
//! frozen verified run's output pixel hash covers plate pixels too, so
//! the Rust composite only reproduces that hash if every resampled
//! plate pixel matches Pillow's fixed-point arithmetic bit for bit.
//!
//! The port follows the C line by line:
//! - coefficients are computed in f64 from an f32 box (the C API takes
//!   `float box[4]`), normalized to sum 1, then converted to i32 fixed
//!   point with PRECISION_BITS = 22 via the ±0.5 branch in
//!   `normalize_coeffs_8bpc`;
//! - each pass accumulates `1 << (PRECISION_BITS-1) + Σ pixel·k` in i32
//!   (wrapping arithmetic, as in C) and stores `clip8` of the shifted
//!   sum — both passes are 8-bit, there is no wide intermediate;
//! - pass order follows `ImagingResampleInner`: horizontal-first
//!   unless the height shrinks by more than twice the width shrink, in
//!   which case vertical runs first; in horizontal-first mode the
//!   horizontal pass only processes the source rows the vertical pass
//!   will actually read (bounds shifted accordingly).

use crate::image::{GrayImage, RgbaImage};

/// Pillow's PRECISION_BITS for 8-bit resampling.
const PRECISION_BITS: i32 = 32 - 8 - 2;

fn clip8(v: i32) -> u8 {
    let q = v >> PRECISION_BITS;
    q.clamp(0, 255) as u8
}

fn bicubic_filter(x: f64) -> f64 {
    const A: f64 = -0.5;
    let x = if x < 0.0 { -x } else { x };
    if x < 1.0 {
        return ((A + 2.0) * x - (A + 3.0)) * x * x + 1.0;
    }
    if x < 2.0 {
        return (((x - 5.0) * x + 8.0) * x - 4.0) * A;
    }
    0.0
}

fn bilinear_filter(x: f64) -> f64 {
    let x = if x < 0.0 { -x } else { x };
    if x < 1.0 {
        1.0 - x
    } else {
        0.0
    }
}

fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        1.0
    } else {
        let x = x * std::f64::consts::PI;
        x.sin() / x
    }
}

fn lanczos_filter(x: f64) -> f64 {
    // Pillow's lanczos_filter: the a=3 windowed sinc; the resampler
    // only ever evaluates it inside its support.
    if x == 0.0 {
        1.0
    } else if (-3.0..3.0).contains(&x) {
        sinc(x) * sinc(x / 3.0)
    } else {
        0.0
    }
}

/// The resampling filters Pillow's `Resample.c` implements for 8-bit
/// images and this pipeline uses: BICUBIC for the plate fit, BILINEAR
/// for the SAM mask map-back, LANCZOS for the SAM-bound downscale.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResampleFilter {
    Bicubic,
    Bilinear,
    Lanczos,
}

impl ResampleFilter {
    fn support(self) -> f64 {
        match self {
            ResampleFilter::Bicubic => 2.0,
            ResampleFilter::Bilinear => 1.0,
            ResampleFilter::Lanczos => 3.0,
        }
    }

    fn eval(self, x: f64) -> f64 {
        match self {
            ResampleFilter::Bicubic => bicubic_filter(x),
            ResampleFilter::Bilinear => bilinear_filter(x),
            ResampleFilter::Lanczos => lanczos_filter(x),
        }
    }
}

struct Coeffs {
    ksize: usize,
    /// Per output position: (xmin, count).
    bounds: Vec<(i32, i32)>,
    /// Per output position: ksize normalized double coefficients.
    kk: Vec<f64>,
}

/// Port of `precompute_coeffs` for one resampling filter.
fn precompute_coeffs(
    filter: ResampleFilter,
    in_size: i32,
    in0: f32,
    in1: f32,
    out_size: i32,
) -> Coeffs {
    // C: filterscale = scale = (double)(in1 - in0) / outSize — the
    // subtraction happens in float, the division in double.
    let scale = (in1 - in0) as f64 / out_size as f64;
    let filterscale = if scale < 1.0 { 1.0 } else { scale };
    let support = filter.support() * filterscale;
    let ksize = (support.ceil() as i32 * 2 + 1) as usize;

    let mut bounds = vec![(0i32, 0i32); out_size as usize];
    let mut kk = vec![0.0f64; out_size as usize * ksize];
    let inv_filterscale = 1.0 / filterscale;
    for xx in 0..out_size {
        let center = in0 as f64 + (xx as f64 + 0.5) * scale;
        let mut ww = 0.0f64;
        // C: xmin = (int)(center - support + 0.5) — cast truncates
        // toward zero; then clamped below at 0.
        let mut xmin = (center - support + 0.5) as i32;
        if xmin < 0 {
            xmin = 0;
        }
        let mut xmax = (center + support + 0.5) as i32;
        if xmax > in_size {
            xmax = in_size;
        }
        xmax -= xmin;
        let base = xx as usize * ksize;
        for x in 0..xmax {
            let w = filter.eval(((x + xmin) as f64 - center + 0.5) * inv_filterscale);
            kk[base + x as usize] = w;
            ww += w;
        }
        if ww != 0.0 {
            for x in 0..xmax {
                kk[base + x as usize] /= ww;
            }
        }
        bounds[xx as usize] = (xmin, xmax);
    }
    Coeffs { ksize, bounds, kk }
}

/// Port of `normalize_coeffs_8bpc`: doubles -> i32 fixed point.
fn normalize_coeffs(coeffs: &Coeffs) -> Vec<i32> {
    coeffs
        .kk
        .iter()
        .map(|&v| {
            if v < 0.0 {
                (-0.5 + v * (1 << PRECISION_BITS) as f64) as i32
            } else {
                (0.5 + v * (1 << PRECISION_BITS) as f64) as i32
            }
        })
        .collect()
}

/// A byte image with a per-pixel stride (1 for L, 4 for RGBA).
struct ByteImage<'a> {
    width: usize,
    height: usize,
    stride: usize,
    data: &'a [u8],
}

fn resample_horizontal(
    img: &ByteImage,
    offset_y: usize,
    out_height: usize,
    out_width: usize,
    coeffs: &Coeffs,
    kk: &[i32],
) -> Vec<u8> {
    let stride = img.stride;
    let mut out = vec![0u8; out_width * out_height * stride];
    for yy in 0..out_height {
        let row_in = (yy + offset_y) * img.width * stride;
        let row_out = yy * out_width * stride;
        for xx in 0..out_width {
            let (xmin, xmax) = coeffs.bounds[xx];
            let kbase = xx * coeffs.ksize;
            for c in 0..stride {
                let mut ss: i32 = 1 << (PRECISION_BITS - 1);
                for x in 0..xmax {
                    let px = img.data[row_in + (x + xmin) as usize * stride + c] as i32;
                    ss = ss.wrapping_add(px.wrapping_mul(kk[kbase + x as usize]));
                }
                out[row_out + xx * stride + c] = clip8(ss);
            }
        }
    }
    out
}

fn resample_vertical(
    img: &ByteImage,
    out_width: usize,
    out_height: usize,
    coeffs: &Coeffs,
    kk: &[i32],
) -> Vec<u8> {
    let stride = img.stride;
    let mut out = vec![0u8; out_width * out_height * stride];
    for yy in 0..out_height {
        let (ymin, ymax) = coeffs.bounds[yy];
        let kbase = yy * coeffs.ksize;
        let row_out = yy * out_width * stride;
        for xx in 0..out_width {
            for c in 0..stride {
                let mut ss: i32 = 1 << (PRECISION_BITS - 1);
                for y in 0..ymax {
                    let px =
                        img.data[(y + ymin) as usize * img.width * stride + xx * stride + c] as i32;
                    ss = ss.wrapping_add(px.wrapping_mul(kk[kbase + y as usize]));
                }
                out[row_out + xx * stride + c] = clip8(ss);
            }
        }
    }
    out
}

/// Port of `ImagingResampleInner` for 8-bit data.
/// `bx` is the source box as f32, exactly as the C API receives it.
fn resample_inner(
    filter: ResampleFilter,
    data: &[u8],
    in_w: usize,
    in_h: usize,
    stride: usize,
    out_w: usize,
    out_h: usize,
    bx: [f32; 4],
) -> Vec<u8> {
    let need_horizontal = out_w != in_w || bx[0] != 0.0 || bx[2] != out_w as f32;
    let need_vertical = out_h != in_h || bx[1] != 0.0 || bx[3] != out_h as f32;

    if !need_horizontal && !need_vertical {
        return data.to_vec();
    }

    let horizontal_first = !((in_h as i64 - out_h as i64) > 0
        && (in_h as i64 - out_h as i64) > (in_w as i64 - out_w as i64) * 2);

    let mut coeffs_vert: Option<Coeffs> = None;
    if (need_horizontal && horizontal_first) || need_vertical {
        coeffs_vert = Some(precompute_coeffs(
            filter,
            in_h as i32,
            bx[1],
            bx[3],
            out_h as i32,
        ));
    }

    let mut cur: Vec<u8> = data.to_vec();
    let mut cur_w = in_w;
    let mut cur_h = in_h;

    if horizontal_first {
        if need_horizontal {
            let cv = coeffs_vert.as_ref().unwrap();
            // First/last source rows the vertical pass will read.
            let ybox_first = cv.bounds[0].0;
            let last = cv.bounds[out_h - 1];
            let ybox_last = last.0 + last.1;
            // Shift vertical bounds by -ybox_first (on a copy).
            let mut shifted = Coeffs {
                ksize: cv.ksize,
                bounds: cv
                    .bounds
                    .iter()
                    .map(|&(min, n)| (min - ybox_first, n))
                    .collect(),
                kk: cv.kk.clone(),
            };
            let coeffs_h = precompute_coeffs(filter, in_w as i32, bx[0], bx[2], out_w as i32);
            let kk_h = normalize_coeffs(&coeffs_h);
            let img = ByteImage {
                width: in_w,
                height: in_h,
                stride,
                data: &cur,
            };
            let rows = (ybox_last - ybox_first) as usize;
            cur = resample_horizontal(&img, ybox_first as usize, rows, out_w, &coeffs_h, &kk_h);
            cur_w = out_w;
            cur_h = rows;
            shifted.kk = cv.kk.clone();
            coeffs_vert = Some(shifted);
        }
        if need_vertical {
            let cv = coeffs_vert.take().unwrap();
            let kk_v = normalize_coeffs(&cv);
            let img = ByteImage {
                width: cur_w,
                height: cur_h,
                stride,
                data: &cur,
            };
            cur = resample_vertical(&img, cur_w, out_h, &cv, &kk_v);
            cur_h = out_h;
        }
    } else {
        if need_vertical {
            let cv = coeffs_vert.take().unwrap();
            let kk_v = normalize_coeffs(&cv);
            let img = ByteImage {
                width: cur_w,
                height: cur_h,
                stride,
                data: &cur,
            };
            cur = resample_vertical(&img, cur_w, out_h, &cv, &kk_v);
            cur_h = out_h;
        }
        if need_horizontal {
            let coeffs_h = precompute_coeffs(filter, in_w as i32, bx[0], bx[2], out_w as i32);
            let kk_h = normalize_coeffs(&coeffs_h);
            let img = ByteImage {
                width: cur_w,
                height: cur_h,
                stride,
                data: &cur,
            };
            cur = resample_horizontal(&img, 0, cur_h, out_w, &coeffs_h, &kk_h);
            cur_w = out_w;
        }
    }
    debug_assert_eq!(cur_w, out_w);
    debug_assert_eq!(cur_h, out_h);
    cur
}

fn shift_for_div255(a: u32) -> u32 {
    ((a >> 8) + a) >> 8
}

/// Pillow's RGBA -> RGBa premultiply (Convert.c rgbA2rgba):
/// c' = MULDIV255(c, a) per colour channel; alpha copied.
fn premultiply(pixels: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels.len());
    for px in pixels.as_chunks::<4>().0 {
        let a = px[3] as u32;
        for c in 0..3 {
            out.push(shift_for_div255(px[c] as u32 * a + 128) as u8);
        }
        out.push(px[3]);
    }
    out
}

/// Pillow's RGBa -> RGBA un-premultiply (Convert.c rgba2rgbA):
/// alpha 0 or 255 copies the channel; else CLIP8(255*c/a).
fn unpremultiply(pixels: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels.len());
    for px in pixels.as_chunks::<4>().0 {
        let a = px[3] as u32;
        for c in 0..3 {
            let v = px[c] as u32;
            let value = if a == 255 || a == 0 {
                v
            } else {
                (255 * v / a).min(255)
            };
            out.push(value as u8);
        }
        out.push(px[3]);
    }
    out
}

/// `Image.resize(size, BICUBIC, box)` for an RGBA image. Pillow
/// resamples RGBA in premultiplied (RGBa) space: convert, resample,
/// convert back (Image.py). A same-size full-box resize is a copy.
pub fn resize_bicubic_box(img: &RgbaImage, out_w: u32, out_h: u32, bx: [f32; 4]) -> RgbaImage {
    if img.width == out_w
        && img.height == out_h
        && bx == [0.0, 0.0, img.width as f32, img.height as f32]
    {
        return img.clone();
    }
    let premultiplied = premultiply(&img.pixels);
    let data = resample_inner(
        ResampleFilter::Bicubic,
        &premultiplied,
        img.width as usize,
        img.height as usize,
        4,
        out_w as usize,
        out_h as usize,
        bx,
    );
    RgbaImage {
        width: out_w,
        height: out_h,
        pixels: unpremultiply(&data),
        icc_profile: img.icc_profile.clone(),
    }
}

/// `ImageOps.fit(image, size)` with default bleed/centering, for RGBA.
/// The crop box is computed in f64 (Python floats) and passed to the
/// resampler as f32 (the C API's `float box[4]`).
pub fn fit(img: &RgbaImage, out_w: u32, out_h: u32) -> RgbaImage {
    if img.width == out_w && img.height == out_h {
        return img.clone();
    }
    let live_w = img.width as f64;
    let live_h = img.height as f64;
    let live_ratio = live_w / live_h;
    let output_ratio = out_w as f64 / out_h as f64;
    let (crop_w, crop_h) = if live_ratio == output_ratio {
        (live_w, live_h)
    } else if live_ratio >= output_ratio {
        (output_ratio * live_h, live_h)
    } else {
        (live_w, live_w / output_ratio)
    };
    let crop_left = (live_w - crop_w) * 0.5;
    let crop_top = (live_h - crop_h) * 0.5;
    let bx = [
        crop_left as f32,
        crop_top as f32,
        (crop_left + crop_w) as f32,
        (crop_top + crop_h) as f32,
    ];
    resize_bicubic_box(img, out_w, out_h, bx)
}

/// `Image.resize(size, filter)` over the full frame for an RGBA
/// image (premultiplied resampling, like `resize_bicubic_box`).
pub fn resize_rgba(img: &RgbaImage, out_w: u32, out_h: u32, filter: ResampleFilter) -> RgbaImage {
    if img.width == out_w && img.height == out_h {
        return img.clone();
    }
    let bx = [0.0, 0.0, img.width as f32, img.height as f32];
    let premultiplied = premultiply(&img.pixels);
    let data = resample_inner(
        filter,
        &premultiplied,
        img.width as usize,
        img.height as usize,
        4,
        out_w as usize,
        out_h as usize,
        bx,
    );
    RgbaImage {
        width: out_w,
        height: out_h,
        pixels: unpremultiply(&data),
        icc_profile: img.icc_profile.clone(),
    }
}

/// `Image.resize(size, filter)` over the full frame for a grayscale
/// (L) image: single-channel resampling, no premultiply.
pub fn resize_gray(img: &GrayImage, out_w: u32, out_h: u32, filter: ResampleFilter) -> GrayImage {
    if img.width == out_w && img.height == out_h {
        return img.clone();
    }
    let bx = [0.0, 0.0, img.width as f32, img.height as f32];
    let data = resample_inner(
        filter,
        &img.pixels,
        img.width as usize,
        img.height as usize,
        1,
        out_w as usize,
        out_h as usize,
        bx,
    );
    GrayImage {
        width: out_w,
        height: out_h,
        pixels: data,
        icc_profile: img.icc_profile.clone(),
    }
}
