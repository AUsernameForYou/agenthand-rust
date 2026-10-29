//! M5 integration tests for the HEIC server path.
//!
//! The corpus-gated cases skip silently when the local corpus is
//! absent (CI machines do not carry it); on this rig they are the
//! refusal item-scope proof and the end-to-end decode proof.

use agenthand::image::decode_image;

const CORPUS: &str = "/home/hatch/workspace/meta-hackathon/rust-100/corpus";

fn corpus_file(rel: &str) -> Option<Vec<u8>> {
    std::fs::read(format!("{CORPUS}/{rel}")).ok()
}

/// Item-scope refusal proof (M4 junction, 4/5 lanes): IMG_1340
/// contains an auxiliary HDR gain-map grid whose tiles are
/// monochrome RExt — the decoder refuses that item class by rule.
/// The refusal must stay item-scoped: the primary picture decodes
/// through the integrated server path with no file-level error.
#[test]
fn refused_aux_item_never_blocks_the_primary() {
    let Some(data) = corpus_file("real/IMG_1340.HEIC") else {
        return;
    };
    let img = decode_image(&data)
        .expect("primary must decode despite the refused aux gain-map item")
        .into_rgba();
    assert_eq!((img.width, img.height), (4284, 5712));
    assert!(img.icc_profile.is_some(), "profile carried from the HEIC");
}

/// A HEIC the subset cannot decode refuses as an error value,
/// never a panic, through the same entry point.
#[test]
fn truncated_heic_is_an_error_not_a_panic() {
    let Some(data) = corpus_file("fixtures/smoke-encode.heic") else {
        return;
    };
    for cut in [16usize, 64, 256, 1024, 8192] {
        let r = std::panic::catch_unwind(|| decode_image(&data[..cut]));
        assert!(r.is_ok(), "decode panicked at truncation {cut}");
        assert!(r.unwrap().is_err(), "truncated file must refuse");
    }
}

/// Pre-decode budget proof (M5 fix lane): the Samsung 200 MP
/// primary declares a 12240x16320 grid canvas, over the 40 MP
/// product cap. Through the shipped path it must refuse from
/// those header dimensions, before decoding — at M5 this file
/// decoded for ~53 s and ~1.8 GB before the post-decode cap
/// fired. The bound below is two orders under that; the measured
/// refusal time is in rust-100/M5-FIXES.md.
#[test]
fn over_cap_grid_canvas_refuses_before_decode() {
    let Some(data) = corpus_file("real/samsung_200mp.heic") else {
        return;
    };
    let t0 = std::time::Instant::now();
    let r = decode_image(&data);
    let elapsed = t0.elapsed();
    let err = match r {
        Ok(_) => panic!("a 200 MP primary is over the product pixel budget"),
        Err(e) => e,
    };
    assert!(
        err.contains("pixel budget"),
        "refusal must name the budget, got: {err}"
    );
    assert!(
        elapsed.as_secs() < 5,
        "over-cap refusal must be header-time, took {elapsed:?}"
    );
}

/// The decoder-level budget plumbing (M5 fix lane): the same
/// grid file IMG_1340 decodes at the default ceiling, and refuses
/// from its declared grid canvas when the caller's budget is set
/// below its 4284x5712 primary.
#[test]
fn decoder_budget_refuses_from_declared_grid_canvas() {
    let Some(data) = corpus_file("real/IMG_1340.HEIC") else {
        return;
    };
    let t0 = std::time::Instant::now();
    let r = agenthand::heic::decode::decode_heic_with_budget(&data, 1_000_000);
    let elapsed = t0.elapsed();
    let err = r.expect_err("a 1 MP budget refuses a 24 MP grid canvas");
    let msg = format!("{err}");
    assert!(
        msg.contains("grid canvas"),
        "refusal must come from the declared grid canvas, got: {msg}"
    );
    assert!(
        elapsed.as_secs() < 5,
        "canvas refusal must be header-time, took {elapsed:?}"
    );
}

/// Garbage with a HEIC-looking brand box is an error, not a panic.
#[test]
fn brand_only_garbage_is_an_error() {
    let mut v = Vec::new();
    v.extend_from_slice(&24u32.to_be_bytes());
    v.extend_from_slice(b"ftyp");
    v.extend_from_slice(b"heic");
    v.extend_from_slice(&0u32.to_be_bytes());
    v.extend_from_slice(b"heic");
    v.extend_from_slice(b"mif1");
    v.extend_from_slice(&[0xAB; 512]);
    let r = std::panic::catch_unwind(|| decode_image(&v));
    assert!(r.is_ok() && r.unwrap().is_err());
}
