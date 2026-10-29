//! M5 sizing evidence: decode + RGB-convert one file, report
//! wall time and peak RSS (VmHWM). Usage: heic_time <file>

use agenthand::heic::decode;

fn peak_rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("VmHWM"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: heic_time <file>");
    let data = std::fs::read(&path).expect("read");
    let t0 = std::time::Instant::now();
    let img = decode::decode_heic(&data).expect("decode");
    let t_decode = t0.elapsed();
    let rgb = decode::to_rgb8(&img);
    let t_all = t0.elapsed();
    println!(
        "{{\"file\": {:?}, \"dims\": [{}, {}], \"tiles\": {}, \"decode_ms\": {}, \"decode_plus_rgb_ms\": {}, \"rgb_bytes\": {}, \"peak_rss_mb\": {:.0}}}",
        path,
        img.planes.width,
        img.planes.height,
        img.grid_tiles,
        t_decode.as_millis(),
        t_all.as_millis(),
        rgb.len(),
        peak_rss_kb() as f64 / 1024.0
    );
}
