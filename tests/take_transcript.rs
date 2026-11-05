//! agenthand-take: transcript formatter and dry-run checks.
//!
//! The formatter tests feed fixed sample responses (shapes from the
//! delivered IMG_1443 run, film-assets/P04-P09/run-1443-fixed) through
//! the same line builders the binary prints with, and compare against
//! the Python driver's exact output. The dry-run tests start the locally
//! built server and stop before identify, so no Meta key is needed.

#[path = "../src/bin/agenthand-take.rs"]
#[allow(dead_code)]
mod take;

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

/// identify-response.json exactly as the Python driver saved it
/// (`json.dumps(resp, indent=2)`, no trailing newline).
const IDENTIFY_RESPONSE_FILE: &str = r#"{
  "id": 3,
  "jsonrpc": "2.0",
  "result": {
    "content": [
      {
        "text": "{\"alternates\":[\"kids BMX bike\",\"small blue bicycle\"],\"attempts\":1,\"call_id\":\"434419d0c20f4e838299bed218294be6\",\"concept\":\"blue kids bike\",\"condition_notes\":\"Rusty chain and axle nuts, scuffs on chain guard and fork.\",\"cost_usd\":0.006692,\"description\":\"Blue Haro Shredder kids BMX-style bicycle with black handlebars, seat, and tires, leaning against low stone wall.\",\"image\":\"asset://279476a67847447287b147e0ac2d918f\",\"listing_bullets\":[\"Blue Haro Shredder frame with white logos\",\"Black padded seat and BMX handlebars\",\"16-inch black tires with spoke wheels\",\"Includes chain guard and wheel reflectors\"],\"model\":\"muse-spark-1.3\",\"status\":\"ok\",\"suggested_title\":\"Haro Shredder Kids Blue BMX Bike\",\"tool\":\"identify\",\"usage\":{\"cached_input_tokens\":4212,\"images\":0,\"input_tokens\":4225,\"output_tokens\":1422}}",
        "type": "text"
      }
    ],
    "isError": false,
    "structuredContent": {
      "alternates": [
        "kids BMX bike",
        "small blue bicycle"
      ],
      "attempts": 1,
      "call_id": "434419d0c20f4e838299bed218294be6",
      "concept": "blue kids bike",
      "condition_notes": "Rusty chain and axle nuts, scuffs on chain guard and fork.",
      "cost_usd": 0.006692,
      "description": "Blue Haro Shredder kids BMX-style bicycle with black handlebars, seat, and tires, leaning against low stone wall.",
      "image": "asset://279476a67847447287b147e0ac2d918f",
      "listing_bullets": [
        "Blue Haro Shredder frame with white logos",
        "Black padded seat and BMX handlebars",
        "16-inch black tires with spoke wheels",
        "Includes chain guard and wheel reflectors"
      ],
      "model": "muse-spark-1.3",
      "status": "ok",
      "suggested_title": "Haro Shredder Kids Blue BMX Bike",
      "tool": "identify",
      "usage": {
        "cached_input_tokens": 4212,
        "images": 0,
        "input_tokens": 4225,
        "output_tokens": 1422
      }
    }
  }
}"#;

/// cut_out-response.json exactly as the Python driver saved it.
const CUT_OUT_RESPONSE_FILE: &str = r#"{
  "id": 4,
  "jsonrpc": "2.0",
  "result": {
    "content": [
      {
        "text": "{\"alternates\":[\"kids bicycle\",\"blue bicycle\"],\"attempts\":[{\"components\":1,\"coverage\":0.23131,\"dominant_fraction\":1.0,\"mask_bounds\":[137,102,1453,1042],\"mask_pixels\":448567,\"object_id\":\"0\",\"object_ids\":[\"0\"],\"outcome\":\"accepted\",\"phrase\":\"blue kids BMX bike\",\"records\":1,\"regions\":1,\"specks_dropped\":0}],\"call_id\":\"6912009efce1403fb9cc65ad66df660f\",\"concept\":\"blue kids BMX bike\",\"concept_source\":\"identify_internal\",\"cost_usd\":0.0025,\"hashes\":{\"foreground\":{\"pixel_sha256\":\"fdbe4774c94ebd8d52439c7a22937fb5b78302066b096261234b842536ce8fa6\",\"sha256\":\"f0c2a48de780590964aa007bb3f9ee18517069d6155632ac130d6bd8e6c22c3c\"},\"mask\":{\"pixel_sha256\":\"46f4f824ff49ca8b9520a6d4b7201b4daa6ffdddec0799a2f71b5df4f3fb9480\",\"sha256\":\"78b0eddde000f5208ddc1bfc45e2d94e26cb3e5344877cc444b40e96ad4b440e\"},\"source\":{\"pixel_sha256\":\"1492a4daa4936e5595a9e25633cb6572ca7bba4c5dc755a8bb10664e6df20db7\",\"sha256\":\"2e767fa0805ccda18beb5de8206607993ee07691ed1d71a890f7276e6c006280\"}},\"image\":\"asset://279476a67847447287b147e0ac2d918f\",\"mask_asset\":\"asset://f8ebf6992ccc4c639a88ed7d965c8e7c\",\"mask_overlay_asset\":\"asset://2a29e3c73cfa435daf4eb91591fcc0e5\",\"mask_provenance\":{\"sam_height\":1206,\"sam_width\":1608,\"scale_method\":\"bilinear_threshold_128\",\"source_height\":4284,\"source_width\":5712},\"mask_review\":{\"bbox\":[487,362,5161,3701],\"bbox_fill\":0.362688,\"coverage\":0.231313,\"mask_pixels\":5660280,\"touches_frame_edges\":[],\"warnings\":[]},\"model\":\"sam-3.1\",\"output_asset\":\"asset://d67133ab3669483196cdc4040f0e9960\",\"plan_error\":null,\"requested_concept\":\"blue kids BMX bike\",\"status\":\"ok\",\"tool\":\"cut_out\",\"usage\":{\"cached_input_tokens\":0,\"images\":1,\"input_tokens\":0,\"output_tokens\":0}}",
        "type": "text"
      }
    ],
    "isError": false,
    "structuredContent": {
      "alternates": [
        "kids bicycle",
        "blue bicycle"
      ],
      "attempts": [
        {
          "components": 1,
          "coverage": 0.23131,
          "dominant_fraction": 1.0,
          "mask_bounds": [
            137,
            102,
            1453,
            1042
          ],
          "mask_pixels": 448567,
          "object_id": "0",
          "object_ids": [
            "0"
          ],
          "outcome": "accepted",
          "phrase": "blue kids BMX bike",
          "records": 1,
          "regions": 1,
          "specks_dropped": 0
        }
      ],
      "call_id": "6912009efce1403fb9cc65ad66df660f",
      "concept": "blue kids BMX bike",
      "concept_source": "identify_internal",
      "cost_usd": 0.0025,
      "hashes": {
        "foreground": {
          "pixel_sha256": "fdbe4774c94ebd8d52439c7a22937fb5b78302066b096261234b842536ce8fa6",
          "sha256": "f0c2a48de780590964aa007bb3f9ee18517069d6155632ac130d6bd8e6c22c3c"
        },
        "mask": {
          "pixel_sha256": "46f4f824ff49ca8b9520a6d4b7201b4daa6ffdddec0799a2f71b5df4f3fb9480",
          "sha256": "78b0eddde000f5208ddc1bfc45e2d94e26cb3e5344877cc444b40e96ad4b440e"
        },
        "source": {
          "pixel_sha256": "1492a4daa4936e5595a9e25633cb6572ca7bba4c5dc755a8bb10664e6df20db7",
          "sha256": "2e767fa0805ccda18beb5de8206607993ee07691ed1d71a890f7276e6c006280"
        }
      },
      "image": "asset://279476a67847447287b147e0ac2d918f",
      "mask_asset": "asset://f8ebf6992ccc4c639a88ed7d965c8e7c",
      "mask_overlay_asset": "asset://2a29e3c73cfa435daf4eb91591fcc0e5",
      "mask_provenance": {
        "sam_height": 1206,
        "sam_width": 1608,
        "scale_method": "bilinear_threshold_128",
        "source_height": 4284,
        "source_width": 5712
      },
      "mask_review": {
        "bbox": [
          487,
          362,
          5161,
          3701
        ],
        "bbox_fill": 0.362688,
        "coverage": 0.231313,
        "mask_pixels": 5660280,
        "touches_frame_edges": [],
        "warnings": []
      },
      "model": "sam-3.1",
      "output_asset": "asset://d67133ab3669483196cdc4040f0e9960",
      "plan_error": null,
      "requested_concept": "blue kids BMX bike",
      "status": "ok",
      "tool": "cut_out",
      "usage": {
        "cached_input_tokens": 0,
        "images": 1,
        "input_tokens": 0,
        "output_tokens": 0
      }
    }
  }
}"#;

/// verify-output.json from the same run (the server's `verify` stdout).
const VERIFY_OUTPUT: &str = r#"{
  "changed_pixels": 0,
  "checked_pixels": 5660280,
  "color_profile_output_sha256": "0ff6958f98684c61f6bbdce1368ddeaf3873baf84545baba482e920d92a914c0",
  "color_profile_preserved": true,
  "color_profile_source_sha256": "0ff6958f98684c61f6bbdce1368ddeaf3873baf84545baba482e920d92a914c0",
  "edge_band_pixels": 0,
  "final": "/Users/dans/agenthand-runs/take-1443-tidy-121511/staged.png",
  "mask": "/Users/dans/agenthand-runs/take-1443-tidy-121511/mask.png",
  "ok": true,
  "outside_mask_pixels": 18809928,
  "problems": [],
  "source": "/Users/dans/agenthand-runs/take-1443-tidy-121511/source.png"
}"#;

/// session-transcript.txt from the same run (Python driver, earlier
/// revision that still passed scene text to draw and had no look-test).
const SAMPLE_TRANSCRIPT: &str = r##"$ date
Sun Nov  1 12:15:11 PST 2026

$ ls -l IMG_1443.heic
-rw-r--r--@ 1 dans  staff  4096794 Oct 31 16:43 IMG_1443.heic

$ agenthand-0.3.0-macos-arm64 serve   # agenthand 0.3.0 (Rust), MCP stdio
initialize -> serverInfo name=agenthand version=0.3.0
tools/list -> ['identify', 'cut_out', 'draw']

$ tools/call identify  image=IMG_1443.heic (4,096,794 bytes)
  muse-spark-1.3 thinking ... 26.6s
  model        : muse-spark-1.3
  request id   : 434419d0c20f4e838299bed218294be6
  concept      : blue kids bike
  title        : Haro Shredder Kids Blue BMX Bike
  condition    : Rusty chain and axle nuts, scuffs on chain guard and fork.
  settled cost : $0.006692

$ tools/call cut_out  image=asset://279476a67847447287b147e0ac2d918f
  sam-3.1 segmenting ... 33.2s
  model        : sam-3.1
  request id   : 6912009efce1403fb9cc65ad66df660f
  mask pixels  : 5,660,280
  settled cost : $0.0025

$ tools/call draw  asset=asset://d67133ab3669483196cdc4040f0e9960
  scene: "clean bright marketplace listing photo backdrop, smooth light gray studi..."
  muse-image-1.0 generating background plate only ... 41.0s
  model        : muse-image-1.0
  request id   : 9cd7b1ccba7f40368959c89616e99096
  settled cost : $0.01

  FIDELITY LOCK RECEIPT
  product pixels checked : 5,660,280
  product pixels changed : 0
  verify: https://ausernameforyou.github.io/agenthand/verify#9a5db66a7e364913bc9ca26bddfbe9081f07fb956c47bd050a8414e1a0427a34

$ agenthand-0.3.0-macos-arm64 verify source.png staged.png mask.png   # independent checker, same shipped build
  verify PASS : checked 5,660,280 px, changed 0 px, ok=True

RESULT: 0 product pixels changed | TOTAL SETTLED: $0.019192

$ date
Sun Nov  1 12:16:52 PST 2026
"##;

const INIT_RESPONSE: &str = r#"{"id":1,"jsonrpc":"2.0","result":{"capabilities":{"resources":{},"tools":{}},"protocolVersion":"2024-11-05","serverInfo":{"name":"agenthand","version":"0.3.0"}}}"#;
const TOOLS_RESPONSE: &str = r#"{"id":2,"jsonrpc":"2.0","result":{"tools":[{"name":"identify"},{"name":"cut_out"},{"name":"draw"}]}}"#;

/// The draw tools/call response from the same run, trimmed to the
/// fields the driver reads (structuredContent shape unchanged), with
/// a short image block in place of the 45 MB PNG.
fn draw_response() -> Value {
    json!({
        "id": 5,
        "jsonrpc": "2.0",
        "result": {
            "content": [
                {"text": "{\"status\":\"ok\"}", "type": "text"},
                {"data": "QUJD".repeat(2500), "mimeType": "image/png", "type": "image"}
            ],
            "isError": false,
            "structuredContent": draw_structured()
        }
    })
}

fn draw_structured() -> Value {
    json!({
        "asset": "asset://d67133ab3669483196cdc4040f0e9960",
        "call_id": "9cd7b1ccba7f40368959c89616e99096",
        "cost_usd": 0.01,
        "fidelity_receipt": {
            "changed_opaque_pixels": 0,
            "edge_band_pixels": 0,
            "opaque_alpha_threshold": 255,
            "opaque_fraction": 1.0,
            "opaque_pixels": 5660280,
            "output_asset": "asset://06ea159da81a45e0844d022210945c13",
            "plate_asset": "asset://bd987141b0ed42d68eeb52f16a1dcf0a",
            "plate_kind": "source_edit",
            "receipt_schema": 2,
            "transparent_pixels": 18809928
        },
        "label": "Digitally altered. Original photo and proof: https://ausernameforyou.github.io/agenthand/verify#9a5db66a7e364913bc9ca26bddfbe9081f07fb956c47bd050a8414e1a0427a34",
        "model": "muse-image-1.0",
        "output_asset": "asset://06ea159da81a45e0844d022210945c13",
        "plate_asset": "asset://bd987141b0ed42d68eeb52f16a1dcf0a",
        "plate_kind": "source_edit",
        "plate_mode": "source_edit",
        "qr_asset": "asset://292f325b33534618a9963b8a6b19faa6",
        "scene_prompt": "",
        "status": "ok",
        "subject": {
            "bbox": [487, 362, 5161, 3701],
            "coverage": 0.231313,
            "fills_frame": false,
            "opaque_pixels": 5660280,
            "thin_fraction": 4e-6,
            "thin_structure": false,
            "touches_frame_edges": []
        },
        "tool": "draw",
        "usage": {"cached_input_tokens": 0, "images": 1, "input_tokens": 12423, "output_tokens": 901},
        "verify_url": "https://ausernameforyou.github.io/agenthand/verify#9a5db66a7e364913bc9ca26bddfbe9081f07fb956c47bd050a8414e1a0427a34"
    })
}

fn structured(file: &str) -> Value {
    let resp: Value = serde_json::from_str(file).expect("fixture JSON");
    resp["result"]["structuredContent"].clone()
}

fn lines(text: &str) -> Vec<String> {
    text.lines().map(str::to_string).collect()
}

/// The transcript body (initialize .. RESULT) the binary prints for the
/// sample responses, built from the same functions `run` calls.
fn build_body(look: Option<(&Value, f64)>) -> Vec<String> {
    let ident = structured(IDENTIFY_RESPONSE_FILE);
    let cut = structured(CUT_OUT_RESPONSE_FILE);
    let draw = draw_structured();
    let init: Value = serde_json::from_str(INIT_RESPONSE).unwrap();
    let tools: Value = serde_json::from_str(TOOLS_RESPONSE).unwrap();
    let verify: Value = serde_json::from_str(VERIFY_OUTPUT).unwrap();
    assert_eq!(verify["ok"], json!(true));

    let mut out = vec![
        take::banner_line("agenthand-0.3.0-macos-arm64", "0.3.0"),
        take::init_line(&init).unwrap(),
        take::tools_list_line(&tools).unwrap(),
    ];
    out.extend(take::identify_call_lines("IMG_1443.heic", 4_096_794));
    out.extend(take::identify_result_lines(&ident, 26.6));
    out.extend(take::cut_call_lines(ident["image"].as_str().unwrap()));
    out.extend(take::cut_result_lines(&cut, 33.2));
    let fg = cut["output_asset"].as_str().unwrap();
    out.extend(take::draw_call_lines(fg, take::SCENE));
    out.extend(take::draw_result_lines(&draw, 41.0));
    out.extend(take::receipt_lines(&draw));
    if let Some((look, secs)) = look {
        out.extend(take::look_call_lines(fg, take::LOOK_SCENE));
        out.extend(take::look_result_lines(look, secs));
    }
    out.extend(take::verify_call_lines("agenthand-0.3.0-macos-arm64"));
    let (verdict, line) = take::verify_outcome(VERIFY_OUTPUT, "");
    assert_eq!(verdict, "PASS");
    out.push(line);
    let (total, _) = take::total_settled(&[&ident, &cut, &draw]);
    out.push(String::new());
    out.push(take::result_line(
        draw["fidelity_receipt"].get("changed_opaque_pixels"),
        total,
    ));
    out
}

/// Line for line against the real Mac transcript of the same run: every
/// line from the banner to RESULT matches, except the draw scene line
/// (that driver revision still passed scene text; this one passes none,
/// as hero1443-merged.py does).
#[test]
fn transcript_matches_the_recorded_mac_run_line_for_line() {
    let sample = lines(SAMPLE_TRANSCRIPT);
    let start = sample.iter().position(|l| l.contains(" serve ")).unwrap();
    let end = sample
        .iter()
        .position(|l| l.starts_with("RESULT:"))
        .unwrap();
    let mut expected: Vec<String> = sample[start..=end].to_vec();
    let scene = expected
        .iter()
        .position(|l| l.starts_with("  scene: "))
        .unwrap();
    expected[scene] = "  scene: (none - merged-build defaults)".into();
    assert_eq!(build_body(None), expected);
}

/// The full body including the look-test block, pinned verbatim.
#[test]
fn transcript_with_look_test_is_pinned() {
    let mut look = draw_structured();
    look["plate_kind"] = json!("source_edit");
    let got = build_body(Some((&look, 38.4))).join("\n");
    let want = r##"$ agenthand-0.3.0-macos-arm64 serve   # agenthand 0.3.0 (Rust), MCP stdio
initialize -> serverInfo name=agenthand version=0.3.0
tools/list -> ['identify', 'cut_out', 'draw']

$ tools/call identify  image=IMG_1443.heic (4,096,794 bytes)
  muse-spark-1.3 thinking ... 26.6s
  model        : muse-spark-1.3
  request id   : 434419d0c20f4e838299bed218294be6
  concept      : blue kids bike
  title        : Haro Shredder Kids Blue BMX Bike
  condition    : Rusty chain and axle nuts, scuffs on chain guard and fork.
  settled cost : $0.006692

$ tools/call cut_out  image=asset://279476a67847447287b147e0ac2d918f
  sam-3.1 segmenting ... 33.2s
  model        : sam-3.1
  request id   : 6912009efce1403fb9cc65ad66df660f
  mask pixels  : 5,660,280
  settled cost : $0.0025

$ tools/call draw  asset=asset://d67133ab3669483196cdc4040f0e9960
  scene: (none - merged-build defaults)
  muse-image-1.0 generating background plate only ... 41.0s
  model        : muse-image-1.0
  request id   : 9cd7b1ccba7f40368959c89616e99096
  settled cost : $0.01

  FIDELITY LOCK RECEIPT
  product pixels checked : 5,660,280
  product pixels changed : 0
  verify: https://ausernameforyou.github.io/agenthand/verify#9a5db66a7e364913bc9ca26bddfbe9081f07fb956c47bd050a8414e1a0427a34

$ tools/call draw  asset=asset://d67133ab3669483196cdc4040f0e9960   # LOOK-TEST
  scene: "the same place, tidy and bright, with a large round cyan-blue circular logo sign on the back wall"
  muse-image-1.0 ... 38.4s
  plate kind   : source_edit
  settled cost : $0.01
  look receipt : 5,660,280 checked / 0 changed

$ agenthand-0.3.0-macos-arm64 verify source.png staged.png mask.png   # independent checker, same shipped build
  verify PASS : checked 5,660,280 px, changed 0 px, ok=True

RESULT: 0 product pixels changed | TOTAL SETTLED: $0.019192"##;
    assert_eq!(got, want);
}

#[test]
fn failure_lines_match_python_shapes() {
    let mut ident = structured(IDENTIFY_RESPONSE_FILE);
    ident["status"] = json!("error");
    ident["error"] = json!({"code": "IMAGE_REFUSED", "message": "caf\u{e9} ".repeat(80)});
    let line = take::failed_line("identify", &ident);
    assert!(line.starts_with("identify FAILED: {\"alternates\": [\"kids BMX bike\", \"small blue bicycle\"], \"attempts\": 1, "));
    assert_eq!(
        line.chars().count(),
        "identify FAILED: ".len() + 300,
        "json.dumps(...)[:300]"
    );
    assert_eq!(
        take::failed_line(
            "draw",
            &json!({"status": "error", "error": {"message": "caf\u{e9}"}})
        ),
        "draw FAILED: {\"error\": {\"message\": \"caf\\u00e9\"}, \"status\": \"error\"}",
        "sorted keys, Python separators, ensure_ascii"
    );

    let mut look = draw_structured();
    look["status"] = json!("error");
    let got = take::look_result_lines(&look, 2.0);
    assert_eq!(got.len(), 5);
    assert!(got[4].starts_with("  LOOK-TEST FAILED: {\"asset\": \"asset://d67133ab"));
}

#[test]
fn verify_outcome_pass_check_and_raw() {
    let (v, l) = take::verify_outcome(VERIFY_OUTPUT, "");
    assert_eq!(
        (v.as_str(), l.as_str()),
        (
            "PASS",
            "  verify PASS : checked 5,660,280 px, changed 0 px, ok=True"
        )
    );
    let (v, l) = take::verify_outcome(
        r#"{"ok": false, "changed_pixels": 3, "checked_pixels": 1200}"#,
        "",
    );
    assert_eq!(
        (v.as_str(), l.as_str()),
        (
            "CHECK OUTPUT",
            "  verify CHECK OUTPUT : checked 1,200 px, changed 3 px, ok=False"
        )
    );
    let (v, l) = take::verify_outcome("", "verify failed: cannot read mask.png");
    assert_eq!(
        (v.as_str(), l.as_str()),
        (
            "UNPARSED",
            "  verify raw: verify failed: cannot read mask.png"
        )
    );
    let (v, _) = take::verify_outcome("[1, 2]", "");
    assert_eq!(v, "UNPARSED");
}

/// `<tool>-response.json` is written with Python's json.dumps(indent=2):
/// re-serialising the saved sample files reproduces them byte for byte.
#[test]
fn saved_response_json_is_python_indent2_byte_for_byte() {
    for file in [IDENTIFY_RESPONSE_FILE, CUT_OUT_RESPONSE_FILE] {
        let v: Value = serde_json::from_str(file).unwrap();
        assert_eq!(take::py_json_dumps_indent(&v), file);
    }
}

#[test]
fn image_blocks_are_stripped_to_a_char_count() {
    let saved = take::strip_images(&draw_response());
    assert_eq!(
        saved["result"]["content"][1]["data"],
        json!("<stripped 10000 chars>")
    );
    assert_eq!(
        saved["result"]["content"][1]["mimeType"],
        json!("image/png")
    );
    assert_eq!(
        saved["result"]["content"][0]["text"],
        json!("{\"status\":\"ok\"}")
    );
    let text = take::py_json_dumps_indent(&saved);
    assert!(
        text.contains("\n        \"thin_fraction\": 4e-06,\n"),
        "Python float repr inside indent=2"
    );
    assert!(!text.ends_with('\n'));
}

#[test]
fn python_value_formatting() {
    for (f, want) in [
        (0.006692, "0.006692"),
        (0.0025, "0.0025"),
        (0.01, "0.01"),
        (4e-6, "4e-06"),
        (1e-5, "1e-05"),
        (0.0001, "0.0001"),
        (1.0, "1.0"),
        (1e15, "1000000000000000.0"),
        (1e16, "1e+16"),
        (1.5e16, "1.5e+16"),
        (0.0, "0.0"),
    ] {
        assert_eq!(take::py_float_repr(f), want, "repr({f})");
    }
    assert_eq!(take::py_commas(Some(&json!(5660280))), "5,660,280");
    assert_eq!(take::py_commas(Some(&json!(999))), "999");
    assert_eq!(take::py_commas(Some(&json!(1234.5))), "1,234.5");
    assert_eq!(take::py_commas(None), "0");
    assert_eq!(take::py_str(None), "None");
    assert_eq!(take::py_str(Some(&json!(true))), "True");
    assert_eq!(take::py_str(Some(&json!(0.0025))), "0.0025");
    assert_eq!(take::py_str(Some(&json!(0))), "0");
    assert_eq!(
        take::py_repr(&json!(["identify", "it's"])),
        "['identify', \"it's\"]"
    );
    assert_eq!(
        take::py_json_dumps(&json!({"b": [1, 2.5], "a": "caf\u{e9} \u{1F6B2}"})),
        "{\"a\": \"caf\\u00e9 \\ud83d\\udeb2\", \"b\": [1, 2.5]}"
    );
    assert_eq!(
        take::py_json_dumps_indent(&json!({"a": [], "b": {}})),
        "{\n  \"a\": [],\n  \"b\": {}\n}"
    );
    let ordered = take::obj(vec![("z", take::val(json!(1))), ("a", take::obj(vec![]))]);
    assert_eq!(
        take::py_json_dumps_ordered(&ordered, Some(2)),
        "{\n  \"z\": 1,\n  \"a\": {}\n}"
    );
}

#[test]
fn total_settled_matches_python_sum() {
    let a = json!({"cost_usd": 0.006692});
    let b = json!({"cost_usd": 0.0025});
    let c = json!({"cost_usd": 0.01});
    let (total, stored) = take::total_settled(&[&a, &b, &c]);
    assert_eq!(format!("{total:.6}"), "0.019192");
    assert_eq!(take::py_json_dumps(&stored), "0.019192");
    // `or 0`: missing, null and 0.0 all add int 0, so the stored total stays an int.
    let (total, stored) = take::total_settled(&[
        &json!({}),
        &json!({"cost_usd": null}),
        &json!({"cost_usd": 0.0}),
    ]);
    assert_eq!((total, stored), (0.0, json!(0)));
    assert_eq!(
        take::result_line(Some(&json!(0)), 0.019192),
        "RESULT: 0 product pixels changed | TOTAL SETTLED: $0.019192"
    );
    assert_eq!(
        take::result_line(None, 0.0),
        "RESULT: None product pixels changed | TOTAL SETTLED: $0.000000"
    );
}

#[test]
fn base64_matches_known_vectors() {
    for (raw, want) in [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ] {
        assert_eq!(take::base64_encode(raw.as_bytes()), want);
    }
    let all: Vec<u8> = (0..=255u8).collect();
    assert!(take::base64_encode(&all).starts_with("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7PD0+P0BBQkNERUZH"));
}

#[test]
fn args_parse_like_argparse() {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let o = take::parse_args(&s(&["--out", "/tmp/x", "--src=/p/IMG.heic", "--dry-run"]))
        .ok()
        .unwrap();
    assert_eq!(o.out, PathBuf::from("/tmp/x"));
    assert_eq!(o.src, PathBuf::from("/p/IMG.heic"));
    assert!(o.dry_run && !o.no_look_test && o.bin.is_none());
    let o = take::parse_args(&s(&["--out", "o"])).ok().unwrap();
    assert!(o.src.ends_with("Downloads/IMG_1443.heic"));
    assert!(matches!(
        take::parse_args(&s(&["--src", "x"])),
        Err(take::ArgsError::Bad(_))
    ));
    assert!(matches!(
        take::parse_args(&s(&["--out", "o", "--nope"])),
        Err(take::ArgsError::Bad(_))
    ));
    assert!(take::resolve_bin(Some(Path::new("/x/agenthand-9"))).ends_with("agenthand-9"));
}

// ---------------------------------------------------------------------
// Dry run against the locally built server (no key, no Meta call)
// ---------------------------------------------------------------------

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agenthand-take-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn dry_run_handshakes_with_the_local_server_and_stops_before_identify() {
    let out = scratch("dry");
    let server = env!("CARGO_BIN_EXE_agenthand");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let run = Command::new(env!("CARGO_BIN_EXE_agenthand-take"))
        .args(["--dry-run", "--bin", server, "--src"])
        .arg(&src)
        .arg("--out")
        .arg(&out)
        .env_remove("META_API_KEY")
        .output()
        .expect("run agenthand-take");
    let stdout = String::from_utf8(run.stdout).unwrap();
    assert!(
        run.status.success(),
        "stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let got = lines(&stdout);
    assert_eq!(got[0], "$ date");
    assert_eq!(got[3], "$ ls -l Cargo.toml");
    assert!(got[4].ends_with(" Cargo.toml"), "ls -l line: {}", got[4]);
    assert_eq!(
        got[6],
        format!(
            "$ agenthand serve   # agenthand {} (Rust), MCP stdio",
            env!("CARGO_PKG_VERSION")
        )
    );
    assert_eq!(
        got[7],
        format!(
            "initialize -> serverInfo name=agenthand version={}",
            env!("CARGO_PKG_VERSION")
        )
    );
    assert!(
        got[8].starts_with("tools/list -> ['identify', 'cut_out', 'draw'"),
        "{}",
        got[8]
    );
    assert!(got.last().unwrap().starts_with("DRY RUN: "));
    assert!(!stdout.contains("tools/call"));
    let transcript = std::fs::read_to_string(out.join("session-transcript.txt")).unwrap();
    assert_eq!(transcript, stdout, "transcript tees stdout exactly");
    assert!(out.join("run-data").is_dir());
    assert!(!out.join("identify-response.json").exists());
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn real_take_without_key_fails_before_printing_anything() {
    let out = scratch("nokey");
    let run = Command::new(env!("CARGO_BIN_EXE_agenthand-take"))
        .args(["--bin", env!("CARGO_BIN_EXE_agenthand"), "--out"])
        .arg(&out)
        .env_remove("META_API_KEY")
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(1));
    assert!(run.stdout.is_empty());
    assert!(String::from_utf8_lossy(&run.stderr).contains("META_API_KEY is not set"));
    assert!(!out.exists());
}

#[test]
fn missing_out_is_a_usage_error() {
    let run = Command::new(env!("CARGO_BIN_EXE_agenthand-take"))
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&run.stderr)
        .contains("the following arguments are required: --out"));
}
