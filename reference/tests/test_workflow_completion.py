"""Workflow completion: item-crop listing copy, narration origins,
code-rendered backdrop stage modes, and Marketplace pricing.

Fully offline, like the rest of the room-run suite: every chain is a
fake at the roomrun adapter boundary, narration is a fake, and the
backdrop/pricing paths are pure code. No test here can reach the
network or spend from any pool.
"""

import os

os.environ.setdefault("AGENTHAND_DATA_DIR", "/tmp/agenthand-test-placeholder")

import json  # noqa: E402
import math  # noqa: E402
import struct  # noqa: E402
import wave as wave_mod  # noqa: E402
from pathlib import Path  # noqa: E402

import numpy as np  # noqa: E402
import pytest  # noqa: E402
from PIL import Image  # noqa: E402

from agenthand import (  # noqa: E402
    backdrop,
    composite,
    config,
    ingest,
    market_pricing,
    priced_table,
    roomrun,
)
from agenthand.assets import AssetStore  # noqa: E402


@pytest.fixture(autouse=True)
def scratch_data_dir(tmp_path, monkeypatch):
    monkeypatch.setenv("AGENTHAND_DATA_DIR", str(tmp_path))
    monkeypatch.delenv("META_API_KEY", raising=False)
    monkeypatch.setattr(config, "_DOTENV_LOADED", True)
    return tmp_path


def _photo(tmp_path: Path, name: str, color=(120, 60, 20)) -> Path:
    path = tmp_path / name
    Image.new("RGB", (48, 36), color).save(path, format="JPEG")
    return path


def _plan(concept: str, title: str | None = None) -> dict:
    return {
        "description": f"a photo whose primary subject is {concept}",
        "concept": concept,
        "concept_rewritten_from": None,
        "alternates": [],
        "suggested_title": title or f"Listing: {concept}",
        "listing_bullets": [f"one {concept}"],
        "condition_notes": "good",
        "model": "fake",
    }


def _verify_ok(store, handle):
    return {
        "ok": True,
        "checks": {"zero_changed_pixels": True},
        "problems": [],
        "recomputed": {
            "changed_opaque_pixels": 0,
            "opaque_pixels": 100,
            "color_profile_preserved": True,
        },
    }


class CropChain:
    """identify / cut_out / draw faked, with a distinct crop identify.

    Source photos arrive as data URIs and get the next entry of
    ``plans``; item crops arrive as asset handles (kind item_crop)
    and get ``crop_plan`` (or a failure when ``crop_fails``). The two
    counters are separate, mirroring production, where the crop
    identify happens after the whole plan pass.
    """

    IDENTIFY_COST = 0.008
    CROP_COST = 0.011
    CUT_COST = 0.0025
    DRAW_COST = 0.01

    def __init__(self, store, plans, contents, *, crop_plan=None, crop_fails=False):
        self.store = store
        self.plans = plans
        self.contents = contents
        self.crop_plan = crop_plan or _plan("straw hat", title="Crop Title: straw hat")
        self.crop_fails = crop_fails
        self.photo_handles: dict[str, int] = {}
        self.fg_object: dict[str, str] = {}
        self.photo_identifies = 0
        self.crop_identifies = 0
        self.spend = 0.0

    def chain(self) -> dict:
        return {"identify": self.identify, "cut_out": self.cut_out, "draw": self.draw}

    def identify(self, image_ref: str) -> dict:
        if isinstance(image_ref, str) and image_ref.startswith("asset://"):
            record = self.store.get_record(image_ref)
            assert record.kind == "item_crop"
            self.crop_identifies += 1
            self.spend += self.CROP_COST
            if self.crop_fails:
                return {
                    "status": "error",
                    "error": {
                        "code": "BAD_MODEL_RESPONSE",
                        "message": "crop identify failed",
                        "cost_counted_usd": self.CROP_COST,
                    },
                }
            return {
                "status": "ok",
                "image": image_ref,
                "cost_usd": self.CROP_COST,
                **self.crop_plan,
            }
        idx = self.photo_identifies
        self.photo_identifies += 1
        self.spend += self.IDENTIFY_COST
        plan = self.plans[idx]
        handle = ingest.ingest(image_ref, self.store)
        self.photo_handles[handle] = idx
        return {"status": "ok", "image": handle, "cost_usd": self.IDENTIFY_COST, **plan}

    def cut_out(self, handle: str, concept: str) -> dict:
        self.spend += self.CUT_COST
        idx = self.photo_handles[handle]
        object_id = self.contents.get((idx, concept))
        if object_id is None:
            return {
                "status": "error",
                "error": {
                    "code": "NO_MATCH",
                    "message": f"no {concept!r} in photo {idx}",
                    "cost_counted_usd": self.CUT_COST,
                },
            }
        fg_handle = self.store.put_image(
            Image.new("RGBA", (10, 10), (200, 30, 30, 255)), kind="foreground_png"
        )
        self.fg_object[fg_handle] = object_id
        return {
            "status": "ok",
            "output_asset": fg_handle,
            "mask_asset": None,
            "mask_overlay_asset": None,
            "mask_review": {"bbox": [4, 4, 30, 30], "warnings": []},
            "cost_usd": self.CUT_COST,
        }

    def draw(self, fg_handle: str, scene: str) -> dict:
        self.spend += self.DRAW_COST
        fg_record = self.store.get_record(fg_handle)
        composite_handle = self.store.put_image(
            Image.new("RGBA", (8, 8), (10, 10, 10, 255)),
            kind="composite",
            source_asset_id=fg_record.asset_id,
        )
        receipt = {
            "source_asset": "asset://source",
            "foreground_pixel_sha256": fg_record.pixel_sha256,
            "opaque_pixels": 100,
            "changed_opaque_pixels": 0,
            "color_profile_preserved": True,
        }
        self.store.save_receipt(composite_handle, receipt)
        return {
            "status": "ok",
            "output_asset": composite_handle,
            "fidelity_receipt": receipt,
            "cost_usd": self.DRAW_COST,
        }


def _run(tmp_path, photos, fake, **kwargs):
    kwargs.setdefault("budget_usd", 5.00)
    kwargs.setdefault("verify_fn", _verify_ok)
    return roomrun.run_room(
        photos,
        [],
        tmp_path / "out",
        store=fake.store,
        chain=fake.chain(),
        spend_probe=lambda: fake.spend,
        **kwargs,
    )


# ---------------------------------------------------------------------------
# S1: listing copy via the item crop
# ---------------------------------------------------------------------------


def test_crop_listing_copy_lands_with_item_crop_source(tmp_path):
    store = AssetStore(tmp_path)
    photos = [_photo(tmp_path, "shelf.jpg")]
    fake = CropChain(
        store,
        plans=[_plan("white bookcase")],  # whole-photo plan: another subject
        contents={(0, "straw hat"): "hat1"},
        crop_plan=_plan("straw hat", title="Woven Straw Sun Hat"),
    )
    manifest = _run(tmp_path, photos, fake, item_names=["straw hat"])
    item = manifest["items"][0]
    assert item["status"] == "staged"
    listing = item["listing"]
    assert listing["source"] == "item_crop"
    assert listing["title"] == "Woven Straw Sun Hat"
    assert listing["bullets"] == ["one straw hat"]
    assert listing["crop_asset"] is not None
    assert store.get_record(listing["crop_asset"]).kind == "item_crop"
    assert fake.crop_identifies == 1
    # The crop identify's cost counts into the item, like any chain call.
    assert item["cost_usd"] == pytest.approx(
        CropChain.CUT_COST + CropChain.CROP_COST + CropChain.DRAW_COST
    )
    assert item["origin"] == "named"
    assert item["stage_mode"] == "scene"


def test_crop_listing_failure_degrades_to_none_and_item_still_stages(tmp_path):
    store = AssetStore(tmp_path)
    photos = [_photo(tmp_path, "shelf.jpg")]
    fake = CropChain(
        store,
        plans=[_plan("white bookcase")],
        contents={(0, "straw hat"): "hat1"},
        crop_fails=True,
    )
    manifest = _run(tmp_path, photos, fake, item_names=["straw hat"])
    item = manifest["items"][0]
    assert item["status"] == "staged"
    listing = item["listing"]
    assert listing["source"] == "none"
    assert listing["title"] is None
    assert listing["crop_asset"] is None
    assert "item-crop" in listing["note"]


def test_photo_plan_listing_carries_null_crop_asset(tmp_path):
    store = AssetStore(tmp_path)
    photos = [_photo(tmp_path, "case.jpg", (200, 90, 10))]
    fake = CropChain(
        store,
        plans=[_plan("orange suitcase")],
        contents={(0, "orange suitcase"): "case1"},
    )
    manifest = _run(tmp_path, photos, fake, item_names=["orange suitcase"])
    listing = manifest["items"][0]["listing"]
    assert listing["source"] == "photo_plan"
    assert listing["crop_asset"] is None
    assert fake.crop_identifies == 0  # the plan path never crops


# ---------------------------------------------------------------------------
# S2: narration merge + origins
# ---------------------------------------------------------------------------


def _wav_file(path: Path, *, seconds: float = 1.0, rate: int = 8000) -> Path:
    with wave_mod.open(str(path), "wb") as wav:
        wav.setnchannels(1)
        wav.setsampwidth(2)
        wav.setframerate(rate)
        wav.writeframes(
            b"".join(
                struct.pack(
                    "<h", int(0.5 * 32767 * math.sin(2 * math.pi * 440 * i / rate))
                )
                for i in range(int(seconds * rate))
            )
        )
    return path


class FakeNarration:
    def __init__(self, reply: str):
        self.reply = reply
        self.calls: list[Path] = []

    def __call__(self, audio_path: Path) -> dict:
        self.calls.append(Path(audio_path))
        return {
            "reply": self.reply,
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150,
            },
            "latency_ms": 10,
            "model": roomrun.NARRATION_MODEL,
        }


def test_narration_merge_selling_mentioned_and_origins(tmp_path):
    store = AssetStore(tmp_path)
    photos = [
        _photo(tmp_path, "purifier.jpg"),
        _photo(tmp_path, "case.jpg", (200, 90, 10)),
    ]
    fake = CropChain(
        store,
        plans=[_plan("air purifier"), _plan("orange suitcase")],
        contents={
            (1, "orange suitcase"): "case1",
            (0, "filter"): "filter1",
            (1, "suitcase"): "case2",
        },
    )
    narration = FakeNarration(
        "TRANSCRIPT\nsell this filter. There is a suitcase and a unicycle.\n"
        "SELLING\nfilter\nMENTIONED\nsuitcase\nunicycle\n"
    )
    manifest = roomrun.run_room(
        photos,
        [],
        tmp_path / "out",
        item_names=["orange suitcase"],
        store=fake.store,
        chain=fake.chain(),
        spend_probe=lambda: fake.spend,
        verify_fn=_verify_ok,
        budget_usd=5.00,
        narrated_audios=[_wav_file(tmp_path / "walk.wav")],
        narration_fn=narration,
    )
    items = {i["name"]: i for i in manifest["items"]}
    # Typed item: untouched origin, no narration annotation.
    assert items["orange suitcase"]["status"] == "staged"
    assert items["orange suitcase"]["origin"] == "named"
    assert items["orange suitcase"]["narration"] is None
    # Narration SELLING item not in the named list: processed like one.
    assert items["filter"]["status"] == "staged"
    assert items["filter"]["origin"] == "narration_selling"
    assert items["filter"]["narration"]["kind"] == "named"
    # MENTIONED and confirmed against the photos: stages.
    assert items["suitcase"]["status"] == "staged"
    assert items["suitcase"]["origin"] == "narration_mentioned"
    assert items["suitcase"]["narration"]["kind"] == "mentioned"
    # MENTIONED and refuted: a failed NO_MATCH record, never dropped.
    unicycle = items["unicycle"]
    assert unicycle["status"] == "failed"
    assert unicycle["failure"]["stage"] == "cut_out"
    assert unicycle["failure"]["code"] == "NO_MATCH"
    assert unicycle["origin"] == "narration_mentioned"
    assert unicycle["sightings"]


# ---------------------------------------------------------------------------
# S3: backdrop renderers, receipts, and backdrop room runs
# ---------------------------------------------------------------------------


def test_wallpaper_renderer_deterministic_and_shadow_free():
    a = backdrop.render_wallpaper(120, 90)
    b = backdrop.render_wallpaper(120, 90)
    assert a.size == (120, 90)
    assert a.tobytes() == b.tobytes()
    pixels = [tuple(row) for row in np.asarray(a).reshape(-1, 4)]
    assert (255, 255, 255, 255) in pixels  # white ground shows between motifs
    assert any(p != (255, 255, 255, 255) for p in pixels)  # the motif is present
    # No contact shadow: every pixel is a white-to-cyan blend, so its
    # channels keep the cyan ordering R <= G <= B (a warm-gray shadow
    # would break it, R > G > B) and nothing is dark — crossing arcs
    # blend deeper than one 40% pass, but never toward black.
    assert all(p[0] <= p[1] <= p[2] for p in pixels)
    assert min(p[0] for p in pixels) >= 100
    assert min(p[3] for p in pixels) == 255


def test_studio_gray_renderer_deterministic_gradient():
    a = backdrop.render_studio_gray(101, 101)
    b = backdrop.render_studio_gray(101, 101)
    assert a.tobytes() == b.tobytes()
    center = a.getpixel((50, 42))
    for got, want in zip(center, (244, 245, 247)):
        assert abs(got - want) <= 2
    corner = a.getpixel((0, 0))
    assert corner[0] < center[0] - 20  # edges ease toward #C6CAD0


def test_backdrop_info_matches_the_contract():
    assert backdrop.backdrop_info("wallpaper") == {
        "kind": "wallpaper",
        "motif_diameter_px": 72,
        "opacity": 0.4,
        "shadow": False,
    }
    assert backdrop.backdrop_info("studio_gray") == {"kind": "studio_gray"}
    assert backdrop.backdrop_info("scene") is None
    with pytest.raises(ValueError):
        backdrop.render_plate("scene", 10, 10)


def _store_with_foreground(tmp_path):
    store = AssetStore(tmp_path)
    # The photo contains the object; the foreground is cut from it,
    # so its opaque pixels are the photo's own (the verifier checks).
    photo_img = Image.new("RGBA", (48, 36), (90, 90, 90, 255))
    px = photo_img.load()
    for y in range(36):
        for x in range(20):
            px[x, y] = (200, 30, 30, 255)
    photo = store.put_image(photo_img, kind="source_photo")
    fg_img = Image.new("RGBA", (48, 36), (0, 0, 0, 0))
    px = fg_img.load()
    for y in range(36):
        for x in range(20):
            px[x, y] = (200, 30, 30, 255)
    fg = store.put_image(
        fg_img, kind="foreground_png", source_asset_id=AssetStore.parse_handle(photo)
    )
    return store, photo, fg


def test_draw_receipt_plate_kinds_and_verifier_acceptance(tmp_path):
    store, _photo_handle, fg = _store_with_foreground(tmp_path)

    _h, placeholder = composite.draw_product(store, fg, "a regal room")
    assert placeholder["plate_kind"] == "local_placeholder"
    assert placeholder["backdrop"] is None

    plate = store.put_image(
        Image.new("RGBA", (48, 36), (0, 0, 255, 255)), kind="background_plate"
    )
    _h, model = composite.draw_product(store, fg, "ignored", plate_handle=plate)
    assert model["plate_kind"] == "model_generated"
    assert model["backdrop"] is None

    result = roomrun.draw_with_code_plate(store, fg, "wallpaper")
    handle, rendered = result["output_asset"], result["fidelity_receipt"]
    assert result["cost_usd"] == 0.0
    assert rendered["plate_kind"] == "code_rendered"
    assert rendered["backdrop"] == backdrop.backdrop_info("wallpaper")
    assert store.get_record(rendered["plate_asset"]).kind == "background_plate"

    report = composite.verify_receipt_from_disk(store, handle)
    assert report["ok"] is True
    # Stripping fields from a stamped v4 output cannot select legacy policy.
    stripped = dict(rendered)
    stripped["receipt_schema"] = 3
    stripped.pop("cut_quality")
    stripped.pop("plate_quality")
    stripped["reconstruction"] = {
        k: v
        for k, v in rendered["reconstruction"].items()
        if k not in ("policy_version", "max_fraction", "attempted")
    }
    stripped.pop("plate_kind")
    stripped.pop("backdrop")
    report = composite.verify_receipt_from_disk(store, handle, receipt=stripped)
    assert report["ok"] is False
    assert not report["checks"]["output_policy_stamp"]

    gray = roomrun.draw_with_code_plate(store, fg, "studio_gray")
    assert gray["fidelity_receipt"]["backdrop"] == {"kind": "studio_gray"}
    assert composite.verify_receipt_from_disk(store, gray["output_asset"])["ok"] is True


class RealBackdropChain:
    """identify/cut_out faked; draw and draw_backdrop are the real paths."""

    def __init__(self, store):
        self.store = store
        self.spend = 0.0

    def identify(self, image_ref: str) -> dict:
        handle = ingest.ingest(image_ref, self.store)
        return {"status": "ok", "image": handle, "cost_usd": 0.0, **_plan("straw hat")}

    def cut_out(self, handle: str, concept: str) -> dict:
        if concept != "straw hat":
            return {
                "status": "error",
                "error": {
                    "code": "NO_MATCH",
                    "message": "not here",
                    "cost_counted_usd": 0.0,
                },
            }
        # Foreground cut from the ingested photo's own pixels.
        source = self.store.read_image(handle).convert("RGBA")
        fg_img = Image.new("RGBA", source.size, (0, 0, 0, 0))
        spx = source.load()
        fpx = fg_img.load()
        for y in range(source.size[1]):
            for x in range(20):
                r, g, b, _a = spx[x, y]
                fpx[x, y] = (r, g, b, 255)
        fg = self.store.put_image(
            fg_img,
            kind="foreground_png",
            source_asset_id=AssetStore.parse_handle(handle),
        )
        return {
            "status": "ok",
            "output_asset": fg,
            "mask_asset": None,
            "mask_overlay_asset": None,
            "mask_review": {"bbox": [0, 0, 20, 36], "warnings": []},
            "cost_usd": 0.0,
        }

    def draw(self, fg_handle: str, scene: str) -> dict:
        handle, receipt = composite.draw_product(self.store, fg_handle, scene)
        return {
            "status": "ok",
            "output_asset": handle,
            "fidelity_receipt": receipt,
            "cost_usd": 0.0,
        }

    def draw_backdrop(self, fg_handle: str, stage_mode: str) -> dict:
        return roomrun.draw_with_code_plate(self.store, fg_handle, stage_mode)

    def chain(self) -> dict:
        return {
            "identify": self.identify,
            "cut_out": self.cut_out,
            "draw": self.draw,
            "draw_backdrop": self.draw_backdrop,
        }


def test_backdrop_room_run_stages_with_code_rendered_receipt(tmp_path):
    store = AssetStore(tmp_path)
    photos = [_photo(tmp_path, "shelf.jpg")]
    chain = RealBackdropChain(store)
    manifest = roomrun.run_room(
        photos,
        [],
        tmp_path / "out",
        item_names=["straw hat"],
        store=store,
        chain=chain.chain(),
        spend_probe=lambda: 0.0,
        budget_usd=5.00,
        stage_mode="wallpaper",
        # no verify_fn: the production verifier grades the receipt
    )
    assert manifest["stage_mode"] == "wallpaper"
    item = manifest["items"][0]
    assert item["status"] == "staged"
    assert item["stage_mode"] == "wallpaper"
    assert item["verification"]["ok"] is True
    assert item["receipt"]["plate_kind"] == "code_rendered"
    assert item["receipt"]["backdrop"] == {
        "kind": "wallpaper",
        "motif_diameter_px": 72,
        "opacity": 0.4,
        "shadow": False,
    }
    persisted = store.load_receipt(item["output_asset"])
    assert persisted["plate_kind"] == "code_rendered"
    assert store.get_record(persisted["plate_asset"]).kind == "background_plate"


# ---------------------------------------------------------------------------
# S4: market pricing
# ---------------------------------------------------------------------------

WINIX_PRICES = [40, 50, 50, 50, 60, 60, 65, 75, 85, 100]


def _comps_data(prices, *, name="winix filter", extra=()):
    comps = [
        {
            "title": f"comp {i}",
            "price_usd": p,
            "condition": "used",
            "distance_mi": 5.0,
            "age_days": 3,
            "is_new": False,
        }
        for i, p in enumerate(prices)
    ]
    comps.extend(extra)
    return {
        "radius_mi": 40,
        "area": "Camas, WA",
        "pulled_at": "2026-10-28T12:00:00+00:00",
        "items": {name: {"comps": comps}},
    }


def test_market_pricing_winix_vector_from_the_spec_formula():
    # Type-7 percentiles of [40,50,50,50,60,60,65,75,85,100]:
    # p25: h=2.25 -> 50 + 0.25*(50-50) = 50; p50: h=4.5 -> 60;
    # p75: h=6.75 -> 65 + 0.75*(75-65) = 72.5 -> rounds to 75.
    record = market_pricing.pricing_for_item("winix filter", _comps_data(WINIX_PRICES))
    assert record == {
        "status": "priced",
        "comps_used": 10,
        "range": {"quick": 50, "market": 60, "premium": 75},
        "suggested": 60,
        "confidence": "moderate",
        "source": "facebook_marketplace_comps",
        "pulled_at": "2026-10-28T12:00:00+00:00",
        "radius_mi": 40,
    }


def test_market_pricing_excludes_new_and_matches_case_insensitively():
    data = _comps_data(
        WINIX_PRICES,
        name="  WINIX Filter ",
        extra=[
            {
                "title": "new one",
                "price_usd": 999,
                "condition": None,
                "distance_mi": None,
                "age_days": None,
                "is_new": True,
            }
        ],
    )
    record = market_pricing.pricing_for_item("winix filter", data)
    assert record["comps_used"] == 10  # the new comp never counts
    assert record["suggested"] == 60


def test_market_pricing_thin_and_no_comps_cases():
    thin = market_pricing.pricing_for_item("winix filter", _comps_data([50]))
    assert thin["status"] == "unpriced_thin_market"
    assert thin["comps_used"] == 1
    assert thin["range"] is None and thin["suggested"] is None
    assert thin["confidence"] is None

    missing = market_pricing.pricing_for_item("other item", _comps_data(WINIX_PRICES))
    assert missing["status"] == "no_comps_supplied"
    assert missing["comps_used"] == 0
    assert missing["pulled_at"] == "2026-10-28T12:00:00+00:00"

    no_file = market_pricing.pricing_for_item("winix filter", None)
    assert no_file["status"] == "no_comps_supplied"
    assert no_file["pulled_at"] is None and no_file["radius_mi"] is None


def test_market_pricing_confidence_bands_and_rounding():
    assert (
        market_pricing.pricing_for_item("x", _comps_data([10, 20], name="x"))[
            "confidence"
        ]
        == "low"
    )
    five = market_pricing.pricing_for_item(
        "x", _comps_data([10, 20, 30, 40, 50], name="x")
    )
    assert five["confidence"] == "low-moderate"
    assert market_pricing.round_to_nearest_5(72.5) == 75  # half away from zero
    assert market_pricing.round_to_nearest_5(12.5) == 15
    assert market_pricing.round_to_nearest_5(1.0) == 5  # the $5 floor
    assert market_pricing.percentile_type7([10.0, 20.0], 0.25) == pytest.approx(12.5)


def test_priced_table_artifacts_from_a_run(tmp_path):
    store = AssetStore(tmp_path)
    photos = [_photo(tmp_path, "case.jpg", (200, 90, 10))]
    fake = CropChain(
        store,
        plans=[_plan("orange suitcase")],
        contents={(0, "orange suitcase"): "case1"},
    )
    comps = {
        "radius_mi": 40,
        "area": "Camas, WA",
        "pulled_at": "2026-10-28T12:00:00+00:00",
        "items": {
            "orange suitcase": {
                "comps": [
                    {
                        "title": f"used suitcase {i}",
                        "price_usd": p,
                        "condition": "used",
                        "distance_mi": 4.0,
                        "age_days": 2,
                        "is_new": False,
                    }
                    for i, p in enumerate(WINIX_PRICES)
                ]
            }
        },
    }
    manifest = _run(
        tmp_path,
        photos,
        fake,
        item_names=["orange suitcase", "ghost item"],
        comps_data=comps,
    )
    items = {i["name"]: i for i in manifest["items"]}
    assert items["orange suitcase"]["pricing"]["status"] == "priced"
    assert items["orange suitcase"]["pricing"]["suggested"] == 60
    assert items["ghost item"]["status"] == "failed"
    assert items["ghost item"]["pricing"]["status"] == "no_comps_supplied"
    assert manifest["pricing_source"]["area"] == "Camas, WA"

    out = tmp_path / "out"
    png = out / "priced-table.png"
    html_path = out / "priced-table.html"
    notes = out / "pricing-notes.md"
    assert png.exists() and png.stat().st_size > 0
    assert Image.open(png).size[0] == 1560
    html_text = html_path.read_text()
    assert priced_table.FOOTNOTE in html_text
    assert "Listing: orange suitcase" in html_text
    assert "$60" in html_text
    assert "ghost item" in html_text  # failed items are rows too
    notes_text = notes.read_text()
    assert "used suitcase 0" in notes_text  # per-item comp listings
    assert "ASKING prices, not sold prices" in notes_text
    assert "10–20% under ask" in notes_text
    assert "capped at moderate" in notes_text
    summary = (out / "summary.txt").read_text()
    assert "listing title: Listing: orange suitcase" in summary
    assert "pricing: $50–$75 range, suggested $60 (moderate, 10 comps)" in summary


# ---------------------------------------------------------------------------
# CLI wiring: --stage-mode and --comps reach run_room
# ---------------------------------------------------------------------------


def test_cli_passes_stage_mode_and_comps(tmp_path, monkeypatch):
    captured = {}

    def fake_run_room(*args, **kwargs):
        captured.update(kwargs)
        return {
            "room_id": "room-cli",
            "created_at": "2026-10-28T00:00:00+00:00",
            "mode": "named",
            "room_style": "scene",
            "total_cost_usd": 0.0,
            "sources": [],
            "items": [],
            "narration": None,
        }

    monkeypatch.setattr(roomrun, "run_room", fake_run_room)
    photo = _photo(tmp_path, "room.jpg")
    comps_path = tmp_path / "comps.json"
    comps_path.write_text(json.dumps(_comps_data(WINIX_PRICES)), encoding="utf-8")
    rc = roomrun.main(
        [
            "--photos",
            str(photo),
            "--items",
            "winix filter",
            "--out",
            str(tmp_path / "out"),
            "--stage-mode",
            "studio_gray",
            "--comps",
            str(comps_path),
        ]
    )
    assert rc == 0
    assert captured["stage_mode"] == "studio_gray"
    assert (
        captured["comps_data"]["items"]["winix filter"]["comps"][0]["price_usd"] == 40
    )
