"""Comps pricing (mirror of the Rust tests/comps_pricing.rs): the loader,
the category matcher and the pricing block attached to identify results,
against the real comps file (tests/fixtures/pricing-comps.json, Facebook
Marketplace comps around Camas WA, gathered 2026-11-01). Offline.
"""

import json
from pathlib import Path

import pytest

from agenthand import comps, config

FIXTURE = Path(__file__).parent / "fixtures" / "pricing-comps.json"


def book():
    loaded, note = comps.load_book(FIXTURE)
    assert loaded is not None, note
    return loaded


def loaded_book():
    return comps.load_book(FIXTURE)


def identify_result(concept, alternates, title):
    """An identify result shaped like server.identify's ok result."""
    return {
        "status": "ok",
        "tool": "identify",
        "image": "asset://0123456789abcdef0123456789abcdef",
        "description": f"A photo of a {concept}.",
        "concept": concept,
        "alternates": list(alternates),
        "suggested_title": title,
        "listing_bullets": ["one", "two", "three"],
        "condition_notes": "Light wear.",
        "model": "muse-spark-1.3",
        "cost_usd": 0.0075,
        "usage": {"input_tokens": 1, "output_tokens": 1,
                  "cached_input_tokens": 0, "images": 0},
        "call_id": "abc",
        "attempts": 1,
    }


def category_of(block):
    return block.get("category")


def name_category(name, title=None):
    return category_of(comps.pricing_for_item_name(loaded_book(), name, title))


def real_doc():
    return json.loads(FIXTURE.read_text(encoding="utf-8"))


# ---------------------------------------------------------------- loader


def test_loader_reads_the_real_comps_file():
    b = book()
    assert [c.key for c in b.categories] == [
        "kids_bmx_bike",
        "white_kids_bike",
        "patio_set",
        "gas_grill",
        "denim_jacket",
        "climbing_dome_jungle_gym",
        "nissan_versa",
    ]
    assert b.skipped == []
    assert [c.comps_count for c in b.categories] == [9, 7, 18, 12, 22, 8, 13]
    # The real file has local comps for every category.
    assert all(c.scope == "local" for c in b.categories)


def test_comps_file_path_resolution(tmp_path):
    data = tmp_path / "data"
    assert config.get_comps_file.__module__ == "agenthand.config"
    import os

    old = os.environ.pop("AGENTHAND_COMPS_FILE", None)
    try:
        os.environ["AGENTHAND_DATA_DIR"] = str(data)
        assert config.get_comps_file() == data / "pricing-comps.json"
        os.environ["AGENTHAND_COMPS_FILE"] = "/x/comps.json"
        assert config.get_comps_file() == Path("/x/comps.json")
    finally:
        os.environ.pop("AGENTHAND_COMPS_FILE", None)
        os.environ.pop("AGENTHAND_DATA_DIR", None)
        if old is not None:
            os.environ["AGENTHAND_COMPS_FILE"] = old


def test_missing_file_is_no_comps_with_a_one_line_note(tmp_path):
    missing = tmp_path / "agenthand-comps-definitely-missing" / "none.json"
    loaded = comps.load_book(missing)
    assert loaded[0] is None
    note = loaded[1]
    assert "not found" in note
    assert "\n" not in note
    block = comps.pricing_for_identify(
        loaded, identify_result("gas grill", [], "Gas grill")
    )
    assert block == {"status": "no_comps", "note": note}
    assert len(block) == 2
    block = comps.pricing_for_item_name(loaded, "gas grill", None)
    assert block["status"] == "no_comps"


@pytest.mark.parametrize(
    "tag,text",
    [
        ("notjson", "{ this is not json"),
        ("nocats", '{"generated": "2026-11-01"}'),
        ("catsnotlist", '{"categories": {"a": 1}}'),
        ("allbad", '{"categories": [{"item": "no key"}, {"key": "x"}]}'),
        ("array", "[1, 2, 3]"),
        ("empty", ""),
    ],
)
def test_malformed_files_are_no_comps_never_errors(tmp_path, tag, text):
    path = tmp_path / f"{tag}.json"
    path.write_text(text, encoding="utf-8")
    loaded = comps.load_book(path)
    assert loaded[0] is None, tag
    assert "\n" not in loaded[1]
    result = identify_result("gas grill", ["propane grill", "barbecue"], "Grill")
    comps.attach_identify_pricing(result, loaded)
    assert result["pricing"]["status"] == "no_comps", tag
    assert isinstance(result["pricing"]["note"], str), tag


def test_a_bad_category_is_skipped_and_the_rest_still_price():
    doc = {
        "categories": [
            {"key": "broken", "item": "Gas grill", "range": [10, 20],
             "recommended": 999},
            {"key": "grill", "item": "Gas barbecue grill", "comps": [],
             "stats": {"count": 3, "min": 20, "p25": 30, "median": 40,
                       "p75": 50, "max": 60}},
        ]
    }
    b = comps.parse_book(doc)
    assert len(b.skipped) == 1
    block = comps.pricing_for_item_name((b, None), "gas grill", None)
    assert block["category"] == "grill"
    # Fields missing from the category fall back to the stats, and
    # confidence to the documented rule (n=3 -> low).
    assert block["range"] == [20, 60]
    assert block["recommended"] == 40
    assert block["percentile_band"] == [30, 50]
    assert block["confidence"] == "low"
    assert block["comps_count"] == 3


# --------------------------------------------------------------- matcher


def test_bike_titles_match_kids_bmx_bike():
    for concept, alts, title in [
        ("kids BMX bike", ["blue BMX bicycle", "children's bike"],
         'Haro Shredder 20" Kids BMX Bike - Blue'),
        ("blue kids bike", ["BMX bike", "bicycle"], "Kids 20-inch BMX Bike, Blue"),
        ("BMX bike", ["kids bicycle", "bike"], "Haro Shredder Kids BMX Bike"),
        ("kids bike", ["bicycle", "child's bike"], "Kids Bike"),
    ]:
        result = identify_result(concept, alts, title)
        block = comps.pricing_for_identify(loaded_book(), result)
        assert category_of(block) == "kids_bmx_bike", f"{concept} / {title}: {block}"
    for name in [
        "Haro Shredder 20 inch kids BMX bike",
        "Haro Shredder Kids BMX Bike - Blue",
        "kids BMX bike",
        "children's bicycle",
    ]:
        assert name_category(name) == "kids_bmx_bike", name
    # The white P12 bike keeps its own comps.
    assert name_category("white kids bike") == "white_kids_bike"


def test_named_categories_match():
    for name, key in [
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
    ]:
        assert name_category(name) == key, name


def test_identify_result_for_the_film_car_matches_on_its_title():
    # A generic concept with a specific title: the title carries it.
    result = identify_result(
        "silver hatchback car",
        ["silver hatchback", "compact hatchback"],
        "2012 Nissan Versa Sedan - Gray",
    )
    block = comps.pricing_for_identify(loaded_book(), result)
    assert category_of(block) == "nissan_versa", block
    assert block["match"]["on"] == "suggested_title"


def test_unrelated_items_are_no_comps():
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
    ]:
        block = comps.pricing_for_item_name(loaded_book(), name, None)
        assert block == {"status": "no_comps"}, f"{name}: {block}"
    # The head noun gates: a bike-ish title cannot price a helmet.
    result = identify_result(
        "bike helmet", ["helmet", "kids helmet"], "White Kids Bike Helmet - Like New"
    )
    assert comps.pricing_for_identify(loaded_book(), result) == {"status": "no_comps"}
    duck = identify_result("rubber duck", ["bath toy", "yellow duck"], "Rubber Duck Bath Toy")
    assert comps.pricing_for_identify(loaded_book(), duck) == {"status": "no_comps"}


def test_pricing_notes_unrelated_probes_are_no_comps():
    # The additional no-comps probes recorded in PRICING-NOTES.md.
    for name in [
        "charcoal grill",
        "leather jacket",
        "silver car",
        "white car",
        "patio umbrella",
        "kids helmet",
        "lawn",
        "fence post",
        "stones",
    ]:
        block = comps.pricing_for_item_name(loaded_book(), name, None)
        assert block == {"status": "no_comps"}, f"{name}: {block}"


# ---------------------------------------------------------- result shape


def test_priced_block_shape_and_values():
    block = comps.pricing_for_item_name(loaded_book(), "kids BMX bike", None)
    assert sorted(block.keys()) == [
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
        "scope",
        "searched_at",
        "status",
    ]
    assert block["status"] == "priced"
    assert block["scope"] == "local"
    assert "note" not in block, "a local block carries no note"
    assert block["category"] == "kids_bmx_bike"
    assert block["range"] == [30, 125]
    assert block["recommended"] == 50
    assert block["percentile_band"] == [44, 80]
    assert block["confidence"] == "medium"
    assert block["comps_count"] == 9
    assert block["searched_at"] == "2026-11-01T15:41-08:00"
    assert block["area"] == "Camas, WA (lat 45.5871, lon -122.3995), 25 mi radius"
    assert block["basis"] == comps.PRICING_BASIS
    assert block["basis"] == "asking prices, Facebook Marketplace comps"
    assert block["advisory"] is True
    assert block["footnote"] == comps.PRICING_FOOTNOTE
    assert "\u2014" not in comps.PRICING_FOOTNOTE, "no em dash"
    assert block["match"]["on"] == "item_name"
    assert block["match"]["score"] >= comps.MIN_MATCH_SCORE

    # Fractional values pass through exactly as the file has them.
    patio = comps.pricing_for_item_name(loaded_book(), "outdoor patio furniture set", None)
    assert patio["recommended"] == 212.5
    assert patio["range"] == [20, 899]
    assert patio["percentile_band"] == [80, 300]
    grill = comps.pricing_for_item_name(loaded_book(), "gas grill", None)
    assert grill["recommended"] == 72.5
    assert grill["confidence"] == "high"
    versa = comps.pricing_for_item_name(loaded_book(), "Nissan Versa", None)
    assert versa["recommended"] == 2450
    assert versa["range"] == [1000, 4500]


def test_low_confidence_passes_through_with_the_advisory():
    block = comps.pricing_for_item_name(loaded_book(), "kids climbing dome", None)
    assert block["category"] == "climbing_dome_jungle_gym"
    assert block["confidence"] == "low"
    assert block["advisory"] is True
    assert block["recommended"] == 250
    assert block["range"] == [50, 1000]
    assert block["footnote"] == comps.PRICING_FOOTNOTE


def test_identify_result_gains_pricing_and_keeps_every_field():
    before = identify_result(
        "kids BMX bike",
        ["blue BMX bicycle", "children's bike"],
        'Haro Shredder 20" Kids BMX Bike - Blue',
    )
    after = json.loads(json.dumps(before))
    comps.attach_identify_pricing(after, loaded_book())
    pricing = after["pricing"]
    assert pricing["category"] == "kids_bmx_bike"
    assert pricing["range"] == [30, 125]
    assert pricing["recommended"] == 50
    assert pricing["match"]["on"] == "concept"
    stripped = {k: v for k, v in after.items() if k != "pricing"}
    assert stripped == before, "every existing field unchanged"
    assert len(after) == len(before) + 1


def test_identify_error_results_are_left_alone():
    before = {
        "status": "error",
        "tool": "identify",
        "image": "asset://x",
        "description": None,
        "concept": None,
        "alternates": [],
        "suggested_title": None,
        "listing_bullets": [],
        "condition_notes": None,
        "model": "muse-spark-1.3",
        "error": {"code": "BAD_MODEL_RESPONSE", "message": "empty body"},
    }
    after = json.loads(json.dumps(before))
    comps.attach_identify_pricing(after, loaded_book())
    assert after == before


def test_listing_title_helps_a_vague_item_name():
    block = comps.pricing_for_item_name(
        loaded_book(), "grill", "Weber Spirit E-325 Propane Grill"
    )
    assert block["category"] == "gas_grill"
    # A title cannot rescue an item whose own name gates it out.
    block = comps.pricing_for_item_name(loaded_book(), "helmet", "Kids BMX Bike Helmet")
    assert block == {"status": "no_comps"}


def test_hero_bike_block_matches_the_pricing_notes():
    # The block PRICING-NOTES.md recorded live for IMG_1443's identify.
    result = identify_result(
        "blue kids BMX bike",
        ["kids bicycle"],
        "Haro Shredder Kids Blue BMX Bike",
    )
    block = comps.pricing_for_identify(loaded_book(), result)
    assert block["status"] == "priced"
    assert block["category"] == "kids_bmx_bike"
    assert block["range"] == [30, 125]
    assert block["recommended"] == 50
    assert block["percentile_band"] == [44, 80]
    assert block["confidence"] == "medium"
    assert block["comps_count"] == 9
    assert block["match"] == {"score": 1.0, "on": "concept"}


# ------------------------------------------------- local / wide tiers


def wide_tier(prices, recommended):
    """A wider-area tier in the documented shape."""
    comps_list = [
        {"title": f"Gas grill {i}", "price": p, "location": "Salem, OR"}
        for i, p in enumerate(prices)
    ]
    lo, hi = min(prices), max(prices)
    return {
        "comps": comps_list,
        "stats": {"count": len(prices), "min": lo, "p25": lo,
                  "median": recommended, "p75": hi, "max": hi},
        "recommended": recommended,
        "range": [lo, hi],
        "percentile_band": [lo, hi],
        "confidence": "low",
        "area": "Portland-Salem metro, 100 mi radius",
        "searched_at": "2026-11-01T18:00-08:00",
    }


def doc_without_local_grill():
    """The real file with gas_grill's LOCAL comps emptied (the data
    lane's "searched locally, found nothing" shape)."""
    doc = real_doc()
    grill = next(c for c in doc["categories"] if c["key"] == "gas_grill")
    grill["comps"] = []
    grill["stats"] = {"count": 0}
    grill["recommended"] = None
    grill["suggested"] = None
    grill["range"] = None
    grill["percentile_band"] = None
    grill["confidence"] = None
    return doc


def test_local_comps_present_price_with_scope_local():
    # A wide tier is present too: local still wins.
    doc = real_doc()
    grill = next(c for c in doc["categories"] if c["key"] == "gas_grill")
    grill["wide"] = wide_tier([100.0, 150.0, 200.0], 150.0)
    doc["categories_wide"] = [
        {"key": "gas_grill", "item": "Gas grill",
         "comps": [{"title": "Grill", "price": 999}]}
    ]
    b = comps.parse_book(doc)
    block = comps.pricing_for_item_name((b, None), "gas grill", None)
    assert block["status"] == "priced"
    assert block["scope"] == "local"
    assert "note" not in block
    assert block["recommended"] == 72.5
    assert block["comps_count"] == 12
    assert block["footnote"] == comps.PRICING_FOOTNOTE
    assert block["area"] == "Camas, WA (lat 45.5871, lon -122.3995), 25 mi radius"


def test_local_missing_wide_nested_prices_with_scope_wide_and_the_note():
    doc = doc_without_local_grill()
    grill = next(c for c in doc["categories"] if c["key"] == "gas_grill")
    grill["wide"] = wide_tier([60.0, 90.0, 120.0, 175.0], 105.0)
    b = comps.parse_book(doc)
    assert b.skipped == [], b.skipped
    assert b.category("gas_grill").scope == "wide"
    block = comps.pricing_for_item_name((b, None), "propane gas grill", None)
    assert block["status"] == "priced", block
    assert block["scope"] == "wide"
    assert block["note"] == comps.WIDE_SCOPE_NOTE
    assert block["note"] == "No local items found; priced from wider-area listings."
    assert block["category"] == "gas_grill"
    assert block["recommended"] == 105.0
    assert block["range"] == [60.0, 175.0]
    assert block["comps_count"] == 4
    assert block["confidence"] == "low"
    assert block["area"] == "Portland-Salem metro, 100 mi radius"
    assert block["searched_at"] == "2026-11-01T18:00-08:00"
    assert block["footnote"] == comps.PRICING_FOOTNOTE_WIDE
    assert "local" not in comps.PRICING_FOOTNOTE_WIDE
    assert block["advisory"] is True
    # Other categories are untouched and still local.
    bike = comps.pricing_for_item_name((b, None), "kids BMX bike", None)
    assert bike["scope"] == "local"
    # identify results get the same wide block.
    result = identify_result("gas grill", ["propane grill", "barbecue grill"], "Weber Gas Grill")
    comps.attach_identify_pricing(result, (b, None))
    assert result["pricing"]["scope"] == "wide"
    assert result["pricing"]["note"] == comps.WIDE_SCOPE_NOTE


def test_local_missing_categories_wide_list_prices_with_scope_wide():
    doc = doc_without_local_grill()
    entry = wide_tier([80.0, 110.0, 140.0], 110.0)
    entry["key"] = "gas_grill"
    # A wide-only category with no local entry at all.
    tent = wide_tier([40.0, 70.0], 55.0)
    tent["key"] = "camping_tent"
    tent["item"] = "Camping tent"
    tent["queries"] = ["camping tent", "4 person tent"]
    tent["comps"] = [
        {"title": "Coleman 4 person tent", "price": 40},
        {"title": "REI camping tent", "price": 70},
    ]
    doc["categories_wide"] = [entry, tent]
    b = comps.parse_book(doc)
    block = comps.pricing_for_item_name((b, None), "gas grill", None)
    assert block["scope"] == "wide", block
    assert block["note"] == comps.WIDE_SCOPE_NOTE
    assert block["recommended"] == 110.0
    assert block["comps_count"] == 3
    tent_block = comps.pricing_for_item_name((b, None), "camping tent", None)
    assert tent_block["category"] == "camping_tent", tent_block
    assert tent_block["scope"] == "wide"
    assert tent_block["note"] == comps.WIDE_SCOPE_NOTE
    assert tent_block["item"] == "Camping tent"


def test_nested_wide_wins_over_a_categories_wide_entry():
    doc = doc_without_local_grill()
    grill = next(c for c in doc["categories"] if c["key"] == "gas_grill")
    grill["wide"] = wide_tier([60.0, 90.0], 75.0)
    other = wide_tier([500.0, 600.0], 550.0)
    other["key"] = "gas_grill"
    doc["categories_wide"] = [other]
    b = comps.parse_book(doc)
    block = comps.pricing_for_item_name((b, None), "gas grill", None)
    assert block["scope"] == "wide"
    assert block["recommended"] == 75.0


def test_neither_tier_is_no_comps():
    # Local emptied, no wide tier anywhere: no_comps for that item,
    # and the rest of the book still prices.
    b = comps.parse_book(doc_without_local_grill())
    assert b.category("gas_grill") is None
    assert len(b.skipped) == 1, b.skipped
    assert "no local comps" in b.skipped[0], b.skipped
    block = comps.pricing_for_item_name((b, None), "gas grill", None)
    assert block == {"status": "no_comps"}
    result = identify_result("gas grill", ["propane grill"], "Gas Grill")
    comps.attach_identify_pricing(result, (b, None))
    assert result["pricing"] == {"status": "no_comps"}
    assert comps.pricing_for_item_name((b, None), "kids BMX bike", None)["scope"] == "local"
    # A wide tier with no comps of its own is no tier either.
    doc = doc_without_local_grill()
    grill = next(c for c in doc["categories"] if c["key"] == "gas_grill")
    grill["wide"] = {"comps": [], "stats": {"count": 0}, "area": "wider"}
    b2 = comps.parse_book(doc)
    assert comps.pricing_for_item_name((b2, None), "gas grill", None) == {"status": "no_comps"}


@pytest.mark.parametrize(
    "tag,text",
    [
        ("widenotlist", '{"categories": [], "categories_wide": {"a": 1}}'),
        ("nothing", '{"generated": "2026-11-01", "categories_wide": null}'),
    ],
)
def test_wide_only_files_load_and_bad_wide_lists_are_notes(tmp_path, tag, text):
    entry = wide_tier([80.0, 110.0, 140.0], 110.0)
    entry["key"] = "gas_grill"
    entry["item"] = "Gas grill"
    b = comps.parse_book({"categories_wide": [entry]})
    block = comps.pricing_for_item_name((b, None), "gas grill", None)
    assert block["scope"] == "wide"

    path = tmp_path / f"{tag}.json"
    path.write_text(text, encoding="utf-8")
    loaded = comps.load_book(path)
    assert loaded[0] is None, tag
    assert "\n" not in loaded[1]
    block = comps.pricing_for_item_name(loaded, "gas grill", None)
    assert block["status"] == "no_comps", tag
    assert isinstance(block["note"], str), tag


# ------------------------------------------------------------ normaliser


def test_normaliser_matches_the_rust_cases():
    assert comps.tokens("Women's Cropped Denim Jackets (GAP light-wash, P18)") == [
        "women", "cropped", "denim", "jacket", "gap", "light", "wash",
    ]
    assert comps.tokens("Kids 20-inch BMX bike") == ["kid", "20", "inch", "bmx", "bike"]
    assert comps.tokens("children's bicycle") == ["kid", "bike"]
    assert comps.tokens("3pc Patio Set") == ["pc", "patio", "set"]
    assert comps.tokens("Jeep Wrangler (4-door)") == ["jeep", "wrangler", "door"]
    assert comps.tokens("glass top") == ["glass", "top"]
