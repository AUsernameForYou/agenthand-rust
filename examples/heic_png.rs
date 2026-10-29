//! Diagnostic (M5): write the server decode path's RGBA output
//! for a HEIC file as a PNG. Usage: heic_png <file.heic> <out.png>

use agenthand::image::{decode_image, encode_png_rgba};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).expect("read heic");
    let img = decode_image(&data).expect("decode").into_rgba();
    let png = encode_png_rgba(&img);
    std::fs::write(&args[2], png).expect("write png");
    println!("wrote {} ({}x{})", args[2], img.width, img.height);
}
