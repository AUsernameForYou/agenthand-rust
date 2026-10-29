//! Single-shot decode through the SERVER path (`image::decode_image`)
//! for the M5 fix-lane drivers (pre-decode budget evidence and the
//! structural mutation campaign). Exit 0 = decoded, exit 2 = refused
//! (structured error), a panic or abort speaks for itself via its own
//! exit status. Prints one JSON line: outcome, dimensions when
//! decoded, the refusal message when refused, wall time, and peak
//! RSS (VmHWM).
//! Usage: heic_decode_server <file>

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
    let path = std::env::args()
        .nth(1)
        .expect("usage: heic_decode_server <file>");
    let data = std::fs::read(&path).expect("read");
    let t0 = std::time::Instant::now();
    let result = agenthand::image::decode_image(&data);
    let ms = t0.elapsed().as_millis();
    let rss_mb = peak_rss_kb() as f64 / 1024.0;
    match result {
        Ok(img) => {
            let img = img.into_rgba();
            println!(
                "{{\"outcome\": \"decoded\", \"dims\": [{}, {}], \"ms\": {ms}, \"peak_rss_mb\": {rss_mb:.0}}}",
                img.width, img.height
            );
            std::process::exit(0);
        }
        Err(e) => {
            println!(
                "{{\"outcome\": \"refused\", \"error\": {e:?}, \"ms\": {ms}, \"peak_rss_mb\": {rss_mb:.0}}}"
            );
            std::process::exit(2);
        }
    }
}
