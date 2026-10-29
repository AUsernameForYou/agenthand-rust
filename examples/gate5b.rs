//! Gate 5b harness (rust-100 M5): RGB differential between the
//! server's own HEIC decode path (`image::decode_image`, in-process
//! decoder + VUI-driven conversion) and ffmpeg's RGB output for
//! the same file.
//!
//! Usage: gate5b <file.heic> <ffmpeg.png> [win_x win_y]
//! With win_x/win_y, the PNG is treated as ffmpeg's crop of the
//! frame at that offset (for files too large to hold two full
//! RGBA frames) and only that window is compared.
//! Prints one JSON object: per-channel max/mean absolute delta,
//! counts of pixels with any-channel delta > 1 and > 2, and the
//! dimension check. No image data is written anywhere.

use agenthand::image::decode_image;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 && args.len() != 5 {
        eprintln!("usage: gate5b <file.heic> <ffmpeg.png> [win_x win_y]");
        std::process::exit(2);
    }
    let (win_x, win_y): (u32, u32) = if args.len() == 5 {
        (args[3].parse().expect("win_x"), args[4].parse().expect("win_y"))
    } else {
        (0, 0)
    };
    let heic = std::fs::read(&args[1]).expect("read heic");
    let png = std::fs::read(&args[2]).expect("read ffmpeg png");
    let ours_full = decode_image(&heic)
        .expect("server decode path")
        .into_rgba();
    let theirs = decode_image(&png).expect("ffmpeg png decode").into_rgba();
    let ours = if win_x == 0 && win_y == 0 && (ours_full.width, ours_full.height) == (theirs.width, theirs.height) {
        ours_full
    } else {
        // Crop the window out of our frame.
        let (w, h) = (theirs.width, theirs.height);
        let mut px = vec![0u8; (w * h * 4) as usize];
        for row in 0..h {
            let src = (((win_y + row) * ours_full.width + win_x) * 4) as usize;
            let dst = (row * w * 4) as usize;
            px[dst..dst + (w * 4) as usize]
                .copy_from_slice(&ours_full.pixels[src..src + (w * 4) as usize]);
        }
        agenthand::image::RgbaImage::new(w, h, px)
    };
    if (ours.width, ours.height) != (theirs.width, theirs.height) {
        println!(
            "{{\"file\": {:?}, \"dims_ours\": [{}, {}], \"dims_ffmpeg\": [{}, {}], \"dims_match\": false}}",
            args[1], ours.width, ours.height, theirs.width, theirs.height
        );
        std::process::exit(1);
    }
    let n = (ours.width * ours.height) as usize;
    let mut max = [0u32; 3];
    let mut sum = [0u64; 3];
    let mut over1 = 0usize;
    let mut over2 = 0usize;
    let mut exact = 0usize;
    for i in 0..n {
        let mut px_max = 0u32;
        for c in 0..3 {
            let d = (ours.pixels[i * 4 + c] as i32 - theirs.pixels[i * 4 + c] as i32).unsigned_abs();
            if d > max[c] {
                max[c] = d;
            }
            sum[c] += d as u64;
            px_max = px_max.max(d);
        }
        if px_max == 0 {
            exact += 1;
        }
        if px_max > 1 {
            over1 += 1;
        }
        if px_max > 2 {
            over2 += 1;
        }
    }
    let mean: Vec<f64> = sum
        .iter()
        .map(|s| *s as f64 / n as f64)
        .collect();
    println!(
        "{{\"file\": {:?}, \"dims\": [{}, {}], \"pixels\": {}, \"max_delta\": [{}, {}, {}], \"mean_delta\": [{:.4}, {:.4}, {:.4}], \"exact_px\": {}, \"px_over_1\": {}, \"px_over_2\": {}}}",
        args[1], ours.width, ours.height, n, max[0], max[1], max[2], mean[0], mean[1], mean[2],
        exact, over1, over2
    );
}
