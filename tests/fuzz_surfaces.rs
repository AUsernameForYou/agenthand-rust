//! Deterministic mutation harness for the three pure parse
//! surfaces (deep-dive audit A6; Gauntlet N17/N19/N20).
//!
//! cargo-fuzz needs a nightly toolchain, which this environment
//! does not carry, so the first fuzz invariant — a parser must
//! never panic; it accepts or it refuses (G18) — runs inside
//! `cargo test` instead. A seeded xorshift PRNG mutates real seed
//! inputs; any panic fails the test, and every crash found here
//! becomes a named regression fixture (G19/G20). Harness-side
//! caps keep huge inputs from turning the run into an OOM hunt
//! (N19): byte inputs are size-capped, and the HEIC surface
//! decodes under a small pixel budget so mutations refuse at
//! header checks instead of decoding full pictures.
//!
//! The three surfaces mirror the future cargo-fuzz targets
//! one-to-one: SAM wire text, HEIC bytes, identify payloads.

use agenthand::heic::{container, decode};
use agenthand::meta::parse_identify_payload;
use agenthand::sam::parse_sam_masks;

/// xorshift64* — deterministic across runs and platforms, so a
/// failure reproduces from the seed alone.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}

/// Mutate a byte string in place: flips, insertions, truncations,
/// and runs of one repeated byte (the shapes real corruption and
/// hostile files actually take).
fn mutate_bytes(rng: &mut Rng, data: &mut Vec<u8>) {
    let rounds = 1 + rng.below(6);
    for _ in 0..rounds {
        if data.is_empty() {
            data.push(rng.next() as u8);
            continue;
        }
        match rng.below(5) {
            0 => {
                let i = rng.below(data.len());
                data[i] = rng.next() as u8;
            }
            1 => {
                let len = data.len();
                data.truncate(rng.below(len + 1));
            }
            2 => {
                let i = rng.below(data.len());
                let b = rng.next() as u8;
                data.insert(i, b);
            }
            3 => {
                let i = rng.below(data.len());
                let n = 1 + rng.below(16);
                let b = *rng.pick(&[0x00, 0xFF, 0x03, b'f', b'<']);
                for k in 0..n {
                    if i + k < data.len() {
                        data[i + k] = b;
                    }
                }
            }
            _ => {
                let i = rng.below(data.len());
                data[i] ^= 1 << rng.below(8);
            }
        }
    }
}

const SAM_SEED_A: &str =
    "<0f>0<|box;x1=0;y1=0;x2=3;y2=5;w=8;h=6|><|mask;x=0;y=0;data=6,4,!!!!!'y8_cbF.|>";
const SAM_SEED_B: &str =
    "<0f>0<|box;x1=0;y1=0;x2=7;y2=5;w=8;h=6|><|mask;x=0;y=0;data=2,2,!!!!!&t.SKi|>";

/// Structured SAM fuzzing (N17): hold a real mask payload fixed
/// and randomize the geometry around it — box, raster dims, and
/// frame in deliberate disagreement, including the extremes that
/// killed the server (border-touching boxes, boxes outrunning
/// the raster, safe-integer-bound coordinates, overflowing frame
/// areas). Invariant: no panic, and every returned mask image is
/// exactly frame-sized (or empty when the frame area overflows).
#[test]
fn sam_geometry_tuples_never_panic() {
    let mut rng = Rng(0x5A5A_0001_5EED_0001);
    let coords: [i64; 12] = [
        0,
        1,
        2,
        3,
        5,
        7,
        8,
        100,
        -1,
        -8,
        (1 << 53) - 1,
        -((1 << 53) - 1),
    ];
    let dims: [usize; 9] = [0, 1, 2, 4, 6, 8, 64, 4096, usize::MAX];
    for _ in 0..1500 {
        let x1 = *rng.pick(&coords);
        let y1 = *rng.pick(&coords);
        let x2 = *rng.pick(&coords);
        let y2 = *rng.pick(&coords);
        let w = *rng.pick(&dims);
        let h = *rng.pick(&dims);
        let (fw, fh) = (*rng.pick(&dims[1..]), *rng.pick(&dims[1..]));
        let payload = *rng.pick(&["6,4,!!!!!'y8_cbF.", "2,2,!!!!!&t.SKi"]);
        let text = format!(
            "<0f>0<|box;x1={x1};y1={y1};x2={x2};y2={y2};w={w};h={h}|><|mask;x=0;y=0;data={payload}|>"
        );
        let masks = parse_sam_masks(&text, fw, fh).expect("parse itself never errors");
        match fw.checked_mul(fh) {
            Some(area) => {
                for m in &masks {
                    assert_eq!(m.image.len(), area, "mask image must be frame-sized");
                }
            }
            None => {
                for m in &masks {
                    assert!(
                        m.image.is_empty(),
                        "overflowing frame area yields empty images"
                    );
                }
            }
        }
    }
}

/// Raw SAM wire-text mutations: the parser must survive arbitrary
/// corruption of a valid response (truncations mid-record,
/// flipped digits, junk between records).
#[test]
fn sam_wire_text_mutations_never_panic() {
    let mut rng = Rng(0xA5A5_1234_5678_9ABC);
    for _ in 0..1500 {
        let seed = *rng.pick(&[SAM_SEED_A, SAM_SEED_B]);
        let mut bytes = seed.as_bytes().to_vec();
        mutate_bytes(&mut rng, &mut bytes);
        let text = String::from_utf8_lossy(&bytes);
        let _ = parse_sam_masks(&text, 8, 6);
        let _ = parse_sam_masks(&text, 1, 1);
        let _ = parse_sam_masks(&text, 5712, 4284);
    }
}

/// HEIC byte mutations against the container parser and the
/// budgeted decoder. Seeds: the tiny synthetic-encode fixtures
/// from the local corpus (skipped when the corpus is absent, the
/// same convention as tests/heic_integration.rs).
#[test]
fn heic_mutations_never_panic() {
    let dir = "/home/hatch/workspace/meta-hackathon/rust-100/corpus/fixtures/tiny";
    let mut seeds: Vec<Vec<u8>> = Vec::new();
    for name in ["solid-64.heic", "blocks-256.heic", "gradient-128.heic"] {
        if let Ok(data) = std::fs::read(format!("{dir}/{name}")) {
            assert!(data.len() <= 1 << 16, "seed must stay harness-sized (N19)");
            seeds.push(data);
        }
    }
    if seeds.is_empty() {
        eprintln!("corpus fixtures absent; HEIC mutation surface skipped");
        return;
    }
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..400 {
        let mut data = rng.pick(&seeds).clone();
        mutate_bytes(&mut rng, &mut data);
        if data.len() > 1 << 16 {
            continue; // harness cap: growth mutations stop here
        }
        let _ = container::parse(&data);
        // A small pixel budget makes hostile headers refuse early;
        // the invariant is survival, not success.
        let _ = decode::decode_heic_with_budget(&data, 1_000_000);
        let _ = agenthand::image::decode_image(&data);
    }
    // Every truncation of the smallest seed, at every length: the
    // classic fuzz corpus shape, exhaustive for a small file.
    let smallest = seeds.iter().min_by_key(|s| s.len()).unwrap();
    for n in 0..smallest.len() {
        let _ = container::parse(&smallest[..n]);
        let _ = decode::decode_heic_with_budget(&smallest[..n], 1_000_000);
    }
}

const IDENTIFY_SEED: &str = r#"{
    "description": "A light wood two-door storage cabinet with black handles.",
    "concept": "storage cabinet",
    "suggested_title": "Light Wood 2-Door Storage Cabinet",
    "condition_notes": "Light wear on the top edge.",
    "listing_bullets": ["Solid wood", "Two doors", "Black handles"],
    "alternates": ["wooden cupboard", "linen cabinet"]
}"#;

/// Identify payload mutations: JSON text corruption plus
/// structural damage (fields dropped, types swapped). The parser
/// must answer Ok or a typed IdentifyError — never panic, and a
/// success must still satisfy the contract (N20's dual: the
/// harness also checks accepted payloads are well-formed).
#[test]
fn identify_payload_mutations_never_panic() {
    // The seed itself is valid.
    parse_identify_payload(IDENTIFY_SEED).expect("seed payload is valid");
    let mut rng = Rng(0x1DEF_0001_AAAA_0001);
    for _ in 0..1500 {
        let mut bytes = IDENTIFY_SEED.as_bytes().to_vec();
        mutate_bytes(&mut rng, &mut bytes);
        let text = String::from_utf8_lossy(&bytes);
        if let Ok(plan) = parse_identify_payload(&text) {
            // Accepted means the contract held: the four string
            // fields are present and non-empty.
            for field in [
                "description",
                "concept",
                "suggested_title",
                "condition_notes",
            ] {
                let s = plan[field].as_str().expect("contract field is a string");
                assert!(!s.trim().is_empty(), "contract field {field} non-empty");
            }
        }
    }
    // Structural mutations on the parsed value.
    let base: serde_json::Value = serde_json::from_str(IDENTIFY_SEED).unwrap();
    let keys = [
        "description",
        "concept",
        "suggested_title",
        "condition_notes",
        "listing_bullets",
        "alternates",
    ];
    for _ in 0..500 {
        let mut v = base.clone();
        let key = *rng.pick(&keys);
        match rng.below(4) {
            0 => {
                v.as_object_mut().unwrap().remove(key);
            }
            1 => {
                v[key] = serde_json::json!(null);
            }
            2 => {
                v[key] = serde_json::json!(12345);
            }
            _ => {
                v[key] = serde_json::json!(["x", "y"]);
            }
        }
        let text = serde_json::to_string(&v).unwrap();
        let _ = parse_identify_payload(&text);
    }
}
