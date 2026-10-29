//! Single-shot decode for the M5 mutation driver: exit 0 =
//! decoded, exit 2 = refused (structured error), a panic or
//! abort speaks for itself via its own exit status.
//! Usage: heic_decode_one <file>

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: heic_decode_one <file>");
    let data = std::fs::read(&path).expect("read");
    match agenthand::heic::decode::decode_heic(&data) {
        Ok(_) => std::process::exit(0),
        Err(_) => std::process::exit(2),
    }
}
