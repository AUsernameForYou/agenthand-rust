//! Diagnostic (M5): print the colour facts a HEIC file carries —
//! container `colr` properties on the primary/tiles and the SPS
//! VUI the decoder parsed — beside what ffmpeg reports.

use agenthand::heic::container::{self, Property};
use agenthand::heic::decode;

fn main() {
    let path = std::env::args().nth(1).expect("usage: heic_vui <file>");
    let data = std::fs::read(&path).expect("read");
    let f = container::parse(&data).expect("parse");
    for (i, p) in f.properties.iter().enumerate() {
        if let Property::ColrNclx {
            primaries,
            transfer,
            matrix,
            full_range,
        } = p
        {
            println!("colr nclx [prop {}]: primaries={} transfer={} matrix={} full={}", i + 1, primaries, transfer, matrix, full_range);
        }
        if let Property::ColrOther { colour_type, data } = p {
            println!("colr other [prop {}]: type={} bytes={}", i + 1, colour_type, data.len());
        }
    }
    match decode::decode_heic(&data) {
        Ok(img) => println!(
            "ours: vui matrix={:?} full={} primaries={:?} transfer={:?} bd={}/{} grid_tiles={} icc={}",
            img.vui.matrix_coeffs,
            img.vui.video_full_range,
            img.vui.colour_primaries,
            img.vui.transfer_characteristics,
            img.bit_depth_luma,
            img.bit_depth_chroma,
            img.grid_tiles,
            img.icc_profile.as_ref().map_or(0, |v| v.len())
        ),
        Err(e) => println!("ours: decode refused: {e}"),
    }
}
