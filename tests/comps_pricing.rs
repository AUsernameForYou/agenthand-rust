//! Comps pricing: the loader, the category matcher and the pricing
//! block attached to identify results, against the real comps file
//! (tests/fixtures/pricing-comps.json, Facebook Marketplace comps
//! around Camas WA, gathered 2026-11-01). Offline; no env mutation.

use std::path::{Path, PathBuf};

use agenthand::comps::{
    attach_identify_pricing, load_book, parse_book, pricing_for_identify, pricing_for_item_name,
    CompsBook, MIN_MATCH_SCORE, PRICING_BASIS, PRICING_FOOTNOTE,
};
use agenthand::config::comps_file_from;
use serde_json::{json, Value};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pricing-comps.json")
}

fn book() -> Result<CompsBook, String> {
    load_book(&fixture())
}

fn temp_file(tag: &str, contents: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agenthand-comps-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("pricing-comps.json");
    std::fs::write(&path, contents).unwrap();
    path
}

/// An identify result shaped like tool_identify's ok result.
fn identify_result(concept: &str, alternates: &[&str], title: &str) -> Value {
    json!({
        "status": "ok",
        "tool": "identify",
        "image": "asset://0123456789abcdef0123456789abcdef",
        "description": format!("A photo of a {concept}."),
        "concept": concept,
        "alternates": alternates,
        "suggested_title": title,
        "listing_bullets": ["one", "two", "three"],
        "condition_notes": "Light wear.",
        "model": "muse-spark-1.3",
        "cost_usd": 0.0075,
        "usage": {"input_tokens": 1, "output_tokens": 1, "cached_input_tokens": 0, "images": 0},
        "call_id": "abc",
        "attempts": 1,
    })
}

fn category_of(block: &Value) -> Option<&str> {
    block.get("category").and_then(Value::as_str)
}

fn name_category(name: &str) -> Option<String> {
    category_of(&pricing_for_item_name(&book(), name, None)).map(str::to_string)
}

// ---------------------------------------------------------------- loader

#[test]
fn loader_reads_the_real_comps_file() {
    let book = book().expect("real comps file loads");
    let keys: Vec<&str> = book.categories.iter().map(|c| c.key.as_str()).collect();
    assert_eq!(
        keys,
        vec![
            "kids_bmx_bike",
            "white_kids_bike",
            "patio_set",
            "gas_grill",
            "denim_jacket",
            "climbing_dome_jungle_gym",
            "nissan_versa"
        ]
    );
    assert!(book.skipped.is_empty(), "skipped: {:?}", book.skipped);
    let counts: Vec<u64> = book.categories.iter().map(|c| c.comps_count).collect();
    assert_eq!(counts, vec![9, 7, 18, 12, 22, 8, 13]);
}

#[test]
fn comps_file_path_resolution() {
    let data = Path::new("/data/dir");
    assert_eq!(
        comps_file_from("", data),
        PathBuf::from("/data/dir/pricing-comps.json")
    );
    assert_eq!(
        comps_file_from("   ", data),
        PathBuf::from("/data/dir/pricing-comps.json")
    );
    assert_eq!(
        comps_file_from("/x/comps.json", data),
        PathBuf::from("/x/comps.json")
    );
}

#[test]
fn missing_file_is_no_comps_with_a_one_line_note() {
    let missing = std::env::temp_dir().join("agenthand-comps-definitely-missing/none.json");
    let book = load_book(&missing);
    let note = book.as_ref().expect_err("missing file is an Err note");
    assert!(note.contains("not found"), "{note}");
    assert!(!note.contains('\n'));
    let block = pricing_for_identify(&book, &identify_result("gas grill", &[], "Gas grill"));
    assert_eq!(block["status"], "no_comps");
    assert_eq!(block["note"], json!(note));
    assert_eq!(block.as_object().unwrap().len(), 2);
    let block = pricing_for_item_name(&book, "gas grill", None);
    assert_eq!(block["status"], "no_comps");
}

#[test]
fn malformed_files_are_no_comps_never_errors() {
    for (tag, text) in [
        ("notjson", "{ this is not json"),
        ("nocats", "{\"generated\": \"2026-11-01\"}"),
        ("catsnotlist", "{\"categories\": {\"a\": 1}}"),
        ("allbad", "{\"categories\": [{\"item\": \"no key\"}, {\"key\": \"x\"}]}"),
        ("array", "[1, 2, 3]"),
        ("empty", ""),
    ] {
        let path = temp_file(tag, text);
        let book = load_book(&path);
        let note = book.as_ref().expect_err(tag);
        assert!(!note.contains('\n'), "{tag}: {note}");
        let mut result = identify_result("gas grill", &["propane grill", "barbecue"], "Grill");
        attach_identify_pricing(&mut result, &book);
        assert_eq!(result["pricing"]["status"], "no_comps", "{tag}");
        assert!(result["pricing"]["note"].is_string(), "{tag}");
    }
}

#[test]
fn a_bad_category_is_skipped_and_the_rest_still_price() {
    let doc = json!({
        "categories": [
            {"key": "broken", "item": "Gas grill", "range": [10, 20], "recommended": 999},
            {"key": "grill", "item": "Gas barbecue grill", "comps": [],
             "stats": {"count": 3, "min": 20, "p25": 30, "median": 40, "p75": 50, "max": 60}}
        ]
    });
    let book = parse_book(&doc).expect("one usable category");
    assert_eq!(book.skipped.len(), 1);
    let block = pricing_for_item_name(&Ok(book), "gas grill", None);
    assert_eq!(block["category"], "grill");
    // Fields missing from the category fall back to the stats, and
    // confidence to the documented rule (n=3 -> low).
    assert_eq!(block["range"], json!([20, 60]));
    assert_eq!(block["recommended"], json!(40));
    assert_eq!(block["percentile_band"], json!([30, 50]));
    assert_eq!(block["confidence"], "low");
    assert_eq!(block["comps_count"], 3);
}

// --------------------------------------------------------------- matcher

#[test]
fn bike_titles_match_kids_bmx_bike() {
    let book = book();
    for (concept, alts, title) in [
        (
            "kids BMX bike",
            vec!["blue BMX bicycle", "children's bike"],
            "Haro Shredder 20\" Kids BMX Bike - Blue",
        ),
        ("blue kids bike", vec!["BMX bike", "bicycle"], "Kids 20-inch BMX Bike, Blue"),
        ("BMX bike", vec!["kids bicycle", "bike"], "Haro Shredder Kids BMX Bike"),
        ("kids bike", vec!["bicycle", "child's bike"], "Kids Bike"),
    ] {
        let result = identify_result(concept, &alts, title);
        let block = pricing_for_identify(&book, &result);
        assert_eq!(
            category_of(&block),
            Some("kids_bmx_bike"),
            "{concept} / {title}: {block}"
        );
    }
    for name in [
        "Haro Shredder 20 inch kids BMX bike",
        "Haro Shredder Kids BMX Bike - Blue",
        "kids BMX bike",
        "children's bicycle",
    ] {
        assert_eq!(name_category(name).as_deref(), Some("kids_bmx_bike"), "{name}");
    }
    // The white P12 bike keeps its own comps.
    assert_eq!(
        name_category("white kids bike").as_deref(),
        Some("white_kids_bike")
    );
}

#[test]
fn named_categories_match() {
    let cases = [
        ("Outdoor patio furniture set", "patio_set"),
        ("patio dining set", "patio_set"),
        ("wicker patio chairs", "patio_set"),
        ("gas grill", "gas_grill"),
        ("propane grill", "gas_grill"),
        ("Weber grill", "gas_grill"),
        ("Nissan Versa", "nissan_versa"),
        ("gray Nissan Versa sedan", "nissan_versa"),
        ("2014 Nissan Versa Note SV hatchback", "nissan_versa"),
        ("women's cropped denim jacket", "denim_jacket"),
        ("jean jacket", "denim_jacket"),
        ("climbing dome", "climbing_dome_jungle_gym"),
        ("jungle gym", "climbing_dome_jungle_gym"),
    ];
    for (name, key) in cases {
        assert_eq!(name_category(name).as_deref(), Some(key), "{name}");
    }
}

#[test]
fn identify_result_for_the_film_car_matches_on_its_title() {
    // A generic concept with a specific title: the title carries it.
    let result = identify_result(
        "silver hatchback car",
        &["silver hatchback", "compact hatchback"],
        "2012 Nissan Versa Sedan - Gray",
    );
    let block = pricing_for_identify(&book(), &result);
    assert_eq!(category_of(&block), Some("nissan_versa"), "{block}");
    assert_eq!(block["match"]["on"], "suggested_title");
}

#[test]
fn unrelated_items_are_no_comps() {
    for name in [
        "rubber duck",
        "kids bike helmet",
        "white bike helmet",
        "wooden chair",
        "gray sofa",
        "patio umbrella",
        "gray car seat",
        "brown leather wallet",
        "",
    ] {
        let block = pricing_for_item_name(&book(), name, None);
        assert_eq!(block, json!({"status": "no_comps"}), "{name}: {block}");
    }
    // The head noun gates: a bike-ish title cannot price a helmet.
    let result = identify_result(
        "bike helmet",
        &["helmet", "kids helmet"],
        "White Kids Bike Helmet - Like New",
    );
    assert_eq!(
        pricing_for_identify(&book(), &result),
        json!({"status": "no_comps"})
    );
    let duck = identify_result("rubber duck", &["bath toy", "yellow duck"], "Rubber Duck Bath Toy");
    assert_eq!(
        pricing_for_identify(&book(), &duck),
        json!({"status": "no_comps"})
    );
}

// ---------------------------------------------------------- result shape

#[test]
fn priced_block_shape_and_values() {
    let block = pricing_for_item_name(&book(), "kids BMX bike", None);
    let keys: Vec<&str> = block.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec![
            "advisory",
            "area",
            "basis",
            "category",
            "comps_count",
            "confidence",
            "footnote",
            "item",
            "match",
            "percentile_band",
            "range",
            "recommended",
            "searched_at",
            "status"
        ]
    );
    assert_eq!(block["status"], "priced");
    assert_eq!(block["category"], "kids_bmx_bike");
    assert_eq!(block["range"], json!([30, 125]));
    assert_eq!(block["recommended"], json!(50));
    assert_eq!(block["percentile_band"], json!([44, 80]));
    assert_eq!(block["confidence"], "medium");
    assert_eq!(block["comps_count"], 9);
    assert_eq!(block["searched_at"], "2026-11-01T15:41-08:00");
    assert_eq!(
        block["area"],
        "Camas, WA (lat 45.5871, lon -122.3995), 25 mi radius"
    );
    assert_eq!(block["basis"], PRICING_BASIS);
    assert_eq!(block["basis"], "asking prices, Facebook Marketplace comps");
    assert_eq!(block["advisory"], true);
    assert_eq!(block["footnote"], PRICING_FOOTNOTE);
    assert!(!PRICING_FOOTNOTE.contains('\u{2014}'), "no em dash");
    assert_eq!(block["match"]["on"], "item_name");
    assert!(block["match"]["score"].as_f64().unwrap() >= MIN_MATCH_SCORE);

    // Fractional values pass through exactly as the file has them.
    let patio = pricing_for_item_name(&book(), "outdoor patio furniture set", None);
    assert_eq!(patio["recommended"], json!(212.5));
    assert_eq!(patio["range"], json!([20, 899]));
    assert_eq!(patio["percentile_band"], json!([80, 300]));
    let grill = pricing_for_item_name(&book(), "gas grill", None);
    assert_eq!(grill["recommended"], json!(72.5));
    assert_eq!(grill["confidence"], "high");
    let versa = pricing_for_item_name(&book(), "Nissan Versa", None);
    assert_eq!(versa["recommended"], json!(2450));
    assert_eq!(versa["range"], json!([1000, 4500]));
}

#[test]
fn low_confidence_passes_through_with_the_advisory() {
    let block = pricing_for_item_name(&book(), "kids climbing dome", None);
    assert_eq!(block["category"], "climbing_dome_jungle_gym");
    assert_eq!(block["confidence"], "low");
    assert_eq!(block["advisory"], true);
    assert_eq!(block["recommended"], json!(250));
    assert_eq!(block["range"], json!([50, 1000]));
    assert_eq!(block["footnote"], PRICING_FOOTNOTE);
}

#[test]
fn identify_result_gains_pricing_and_keeps_every_field() {
    let before = identify_result(
        "kids BMX bike",
        &["blue BMX bicycle", "children's bike"],
        "Haro Shredder 20\" Kids BMX Bike - Blue",
    );
    let mut after = before.clone();
    attach_identify_pricing(&mut after, &book());
    let pricing = after["pricing"].clone();
    assert_eq!(pricing["category"], "kids_bmx_bike");
    assert_eq!(pricing["range"], json!([30, 125]));
    assert_eq!(pricing["recommended"], json!(50));
    assert_eq!(pricing["match"]["on"], "concept");
    let mut stripped = after.clone();
    stripped.as_object_mut().unwrap().remove("pricing");
    assert_eq!(stripped, before, "every existing field unchanged");
    assert_eq!(
        after.as_object().unwrap().len(),
        before.as_object().unwrap().len() + 1
    );
}

#[test]
fn identify_error_results_are_left_alone() {
    let before = json!({
        "status": "error",
        "tool": "identify",
        "image": "asset://x",
        "description": null,
        "concept": null,
        "alternates": [],
        "suggested_title": null,
        "listing_bullets": [],
        "condition_notes": null,
        "model": "muse-spark-1.3",
        "error": {"code": "BAD_MODEL_RESPONSE", "message": "empty body"},
    });
    let mut after = before.clone();
    attach_identify_pricing(&mut after, &book());
    assert_eq!(after, before);
}

#[test]
fn listing_title_helps_a_vague_item_name() {
    let block = pricing_for_item_name(&book(), "grill", Some("Weber Spirit E-325 Propane Grill"));
    assert_eq!(block["category"], "gas_grill");
    // A title cannot rescue an item whose own name gates it out.
    let block = pricing_for_item_name(&book(), "helmet", Some("Kids BMX Bike Helmet"));
    assert_eq!(block, json!({"status": "no_comps"}));
}
