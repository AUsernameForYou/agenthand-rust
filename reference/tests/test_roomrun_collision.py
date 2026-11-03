"""F1: candidate coverage and duplicate-collision re-selection.

Fully offline, like the rest of the room-run suite: the chain is a
fake at the roomrun adapter boundary, video frame sampling is faked
at the same boundary, and the spend probe is the fake's own counter.
The fake's foregrounds are real full-frame assets in the tmp store,
so mask-overlap maths runs against the same stored files production
computes from.
"""

import os

os.environ.setdefault("AGENTHAND_DATA_DIR", "/tmp/agenthand-test-placeholder")

from pathlib import Path  # noqa: E402

import pytest  # noqa: E402
from PIL import Image  # noqa: E402

from agenthand import config, ingest, roomrun  # noqa: E402
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


# Object masks on a shared 48x36 canvas: (x0, y0, x1, y1).
OBJECT_RECTS = {
    "straw_shared": (4, 4, 24, 20),   # 320 opaque px
    "straw_other": (28, 20, 40, 32),  # 144 opaque px, disjoint
    "fedora": (30, 4, 46, 20),        # 256 opaque px, disjoint from straw_shared
    "straw_big": (2, 2, 34, 26),      # 768 opaque px; overlaps the fedora by 64 px
}
OBJECT_COLORS = {
    "straw_shared": (220, 180, 60, 255),
    "straw_other": (210, 170, 50, 255),
    "fedora": (20, 20, 20, 255),
    "straw_big": (225, 185, 70, 255),
}


class CollisionChain:
    """identify / cut_out / draw faked with per-object mask geometry.

    plans: identify results for source photos, in source order.
    Item crops arrive as asset handles and get the next queued
    crop plan. contents maps (photo_index, concept) -> object_id.
    """

    IDENTIFY_COST = 0.008
    CROP_COST = 0.011
    CUT_COST = 0.0025
    DRAW_COST = 0.01

    def __init__(self, store, plans, contents, *, crop_plans=(), draw_fail_objects=()):
        self.store = store
        self.plans = plans
        self.contents = contents
        self.crop_plans = list(crop_plans)
        self.draw_fail_objects = set(draw_fail_objects)
        self.photo_handles: dict[str, int] = {}
        self.fg_object: dict[str, str] = {}
        self.photo_identifies = 0
        self.crop_identifies = 0
        self.draw_calls = 0
        self.cut_calls: list[tuple[int, str]] = []
        self.spend = 0.0

    def chain(self) -> dict:
        return {"identify": self.identify, "cut_out": self.cut_out, "draw": self.draw}

    def identify(self, image_ref: str) -> dict:
        if isinstance(image_ref, str) and image_ref.startswith("asset://"):
            self.crop_identifies += 1
            self.spend += self.CROP_COST
            plan = self.crop_plans.pop(0) if self.crop_plans else _plan("straw hat")
            return {
                "status": "ok",
                "image": image_ref,
                "cost_usd": self.CROP_COST,
                **plan,
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
        self.cut_calls.append((idx, concept))
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
        x0, y0, x1, y1 = OBJECT_RECTS[object_id]
        img = Image.new("RGBA", (48, 36), (0, 0, 0, 0))
        px = img.load()
        for y in range(y0, y1):
            for x in range(x0, x1):
                px[x, y] = OBJECT_COLORS[object_id]
        fg_handle = self.store.put_image(img, kind="foreground_png")
        self.fg_object[fg_handle] = object_id
        return {
            "status": "ok",
            "output_asset": fg_handle,
            "mask_asset": None,
            "mask_overlay_asset": None,
            "mask_review": {"bbox": [x0, y0, x1, y1], "warnings": []},
            "cost_usd": self.CUT_COST,
        }

    def draw(self, fg_handle: str, scene: str) -> dict:
        self.draw_calls += 1
        self.spend += self.DRAW_COST
        object_id = self.fg_object[fg_handle]
        if object_id in self.draw_fail_objects:
            return {
                "status": "error",
                "error": {
                    "code": "IMAGE_REFUSED",
                    "message": "the image model refused this plate",
                    "cost_counted_usd": self.DRAW_COST,
                },
            }
        fg_record = self.store.get_record(fg_handle)
        composite = self.store.put_image(
            Image.new("RGBA", (8, 8), (10, 10, 10, 255)),
            kind="composite",
            source_asset_id=fg_record.asset_id,
        )
        x0, y0, x1, y1 = OBJECT_RECTS[object_id]
        receipt = {
            "source_asset": "asset://source",
            "foreground_pixel_sha256": fg_record.pixel_sha256,
            "opaque_pixels": (x1 - x0) * (y1 - y0),
            "changed_opaque_pixels": 0,
            "color_profile_preserved": True,
        }
        self.store.save_receipt(composite, receipt)
        return {
            "status": "ok",
            "output_asset": composite,
            "fidelity_receipt": receipt,
            "cost_usd": self.DRAW_COST,
        }


def _with_frames(monkeypatch, frame_paths):
    """Fake video sampling: the given files become the video's frames."""

    def fake_sample(video_path, out_dir, **kwargs):
        return [(p, float(2 * i)) for i, p in enumerate(frame_paths)], None, []

    monkeypatch.setattr(roomrun, "sample_video_frames", fake_sample)


def _run(tmp_path, stills, fake, **kwargs):
    kwargs.setdefault("budget_usd", 5.00)
    kwargs.setdefault("verify_fn", _verify_ok)
    videos = kwargs.pop("videos", [])
    probe = kwargs.pop("spend_probe", None) or (lambda: fake.spend)
    return roomrun.run_room(
        stills,
        videos,
        tmp_path / "out",
        store=fake.store,
        chain=fake.chain(),
        spend_probe=probe,
        **kwargs,
    )


def _hat_room(
    tmp_path, monkeypatch, *, black_contents_extra=None, crop_plans=None,
    draw_fail_objects=(), spend_probe=None,
):
    """The acceptance scenario in miniature.

    Photo 0 is the bookcase still (the only fedora source), photos
    1-2 are frames whose plans name the straw hat, photo 3 is a
    frame about a storage box. Both hat items rank the frames
    first, so the pre-fix cap of 3 never probed the still.
    """
    store = AssetStore(tmp_path)
    still = _photo(tmp_path, "bookcase.jpg")
    frames = [
        _photo(tmp_path, "frame1.jpg", (200, 90, 10)),
        _photo(tmp_path, "frame2.jpg", (210, 95, 12)),
        _photo(tmp_path, "frame3.jpg", (100, 100, 100)),
    ]
    _with_frames(monkeypatch, frames)
    contents = {
        (1, "straw hat"): "straw_other",
        (2, "straw hat"): "straw_shared",
        (1, "hat"): "straw_other",
        (2, "hat"): "straw_shared",
        (0, "black hat"): "fedora",
    }
    if black_contents_extra is not None:
        contents = black_contents_extra
    fake = CollisionChain(
        store,
        plans=[
            _plan("white bookcase"),
            _plan("straw hat"),
            _plan("straw hat"),
            _plan("gray storage box"),
        ],
        contents=contents,
        crop_plans=crop_plans
        if crop_plans is not None
        else [
            _plan("straw hat", title="Straw Sun Hat"),
            _plan("black hat", title="Black Fedora Hat"),
        ],
        draw_fail_objects=draw_fail_objects,
    )
    run_kwargs = {}
    if spend_probe is not None:
        run_kwargs["spend_probe"] = spend_probe(fake)
    manifest = _run(
        tmp_path,
        [still],
        fake,
        videos=[tmp_path / "walk.mp4"],
        item_names=["straw hat", "black hat"],
        **run_kwargs,
    )
    return manifest, fake


# ---------------------------------------------------------------------------
# (a) Candidate coverage
# ---------------------------------------------------------------------------


def test_select_candidate_indices_never_drops_a_still():
    sources = [
        roomrun.PhotoSource(path=Path("s0.jpg"), origin="still"),
        roomrun.PhotoSource(path=Path("f1.jpg"), origin="video_frame"),
        roomrun.PhotoSource(path=Path("f2.jpg"), origin="video_frame"),
        roomrun.PhotoSource(path=Path("s3.jpg"), origin="still"),
        roomrun.PhotoSource(path=Path("f4.jpg"), origin="video_frame"),
    ]
    ranked = [1, 2, 4, 0, 3]  # frames fill the ranked top; stills sit low
    selected = roomrun._select_candidate_indices("black hat", ranked, sources, 3)
    assert set(selected) == {0, 3, 1}  # both stills plus the top-ranked frame
    assert selected == [1, 0, 3]  # probe order stays the rank order


def test_still_excluded_by_cap_is_now_probed(tmp_path, monkeypatch):
    manifest, fake = _hat_room(tmp_path, monkeypatch)
    black = {i["name"]: i for i in manifest["items"]}["black hat"]
    probed = [s["photo_index"] for s in black["sightings"]]
    assert 0 in probed  # the bookcase still was probed for the black hat
    still_sighting = next(s for s in black["sightings"] if s["photo_index"] == 0)
    assert still_sighting["status"] == "ok"  # and it holds the fedora


# ---------------------------------------------------------------------------
# (b) Collision re-selection
# ---------------------------------------------------------------------------


def test_collision_displaced_item_restages_from_the_still(tmp_path, monkeypatch):
    manifest, fake = _hat_room(tmp_path, monkeypatch)
    items = {i["name"]: i for i in manifest["items"]}
    straw, black = items["straw hat"], items["black hat"]

    # Keeper untouched: same primary, same listing, same staged output.
    assert straw["status"] == "staged"
    assert straw["primary_photo_index"] == 2
    assert straw["listing"]["source"] == "photo_plan"

    # Displaced: re-staged from the still's fedora sighting.
    assert black["status"] == "staged"
    assert black["primary_photo_index"] == 0
    assert black["also_seen_in"] == [1, 2]
    assert black["verification"]["ok"] is True
    assert black["receipt"]["opaque_pixels"] == 256  # the fedora, not the straw hat
    assert "primary_reselected_after_duplicate_collision" in black["flags"]
    assert any("straw hat" in f for f in black["flags"])
    # Duplicate flags describe the probing ambiguity and persist
    # after a successful re-selection (cycle 2 unified rule).
    assert "straw hat" in black["possible_duplicate_of"]
    assert "black hat" in straw["possible_duplicate_of"]
    assert any(
        "black hat" in n and "straw hat" in n for n in manifest["notes"]
    )
    # Listing recomputed from the new primary, not carried over.
    assert black["listing"]["source"] == "item_crop"
    assert black["listing"]["title"] == "Black Fedora Hat"
    # Re-stage spend counts into the item: 5 cuts + 2 crop identifies
    # (the original colliding crop, and the re-stage's fresh crop)
    # + 2 draws (the original stage, and the re-stage).
    assert black["cost_usd"] == pytest.approx(
        5 * CollisionChain.CUT_COST
        + 2 * CollisionChain.CROP_COST
        + 2 * CollisionChain.DRAW_COST
    )
    assert fake.draw_calls == 3  # straw once, black twice


def test_collision_without_qualifying_alternate_keeps_flags(tmp_path, monkeypatch):
    contents = {
        (1, "straw hat"): "straw_other",
        (2, "straw hat"): "straw_shared",
        (2, "hat"): "straw_shared",  # black hat succeeds ONLY on the colliding frame
    }
    manifest, fake = _hat_room(
        tmp_path, monkeypatch, black_contents_extra=contents
    )
    items = {i["name"]: i for i in manifest["items"]}
    black = items["black hat"]
    assert black["status"] == "staged"
    assert black["primary_photo_index"] == 2  # original primary kept
    assert "straw hat" in black["possible_duplicate_of"]  # duplicate flag stands
    assert "primary_reselected_after_duplicate_collision" not in black["flags"]
    assert fake.draw_calls == 2  # no re-stage was attempted


def test_collision_with_ambiguous_evidence_does_not_reselect(tmp_path, monkeypatch):
    # Both items' listing evidence matches their own name only:
    # the straw hat via its photo plan, the black hat via a crop
    # plan naming the black hat. Neither is the unique keeper.
    crop_plans = [_plan("black hat", title="Black Fedora Hat")]
    manifest, fake = _hat_room(tmp_path, monkeypatch, crop_plans=crop_plans)
    items = {i["name"]: i for i in manifest["items"]}
    black = items["black hat"]
    assert black["status"] == "staged"
    assert black["primary_photo_index"] == 2
    assert "straw hat" in black["possible_duplicate_of"]
    assert "primary_reselected_after_duplicate_collision" not in black["flags"]
    assert fake.draw_calls == 2


def test_collision_restage_failure_reverts_to_original(tmp_path, monkeypatch):
    manifest, fake = _hat_room(
        tmp_path, monkeypatch, draw_fail_objects={"fedora"}
    )
    items = {i["name"]: i for i in manifest["items"]}
    black = items["black hat"]
    assert black["status"] == "staged"  # the original staged result stands
    assert black["primary_photo_index"] == 2
    assert black["listing"]["title"] == "Straw Sun Hat"  # original listing kept
    assert "straw hat" in black["possible_duplicate_of"]
    assert "primary_reselected_after_duplicate_collision" not in black["flags"]
    assert any("re-selection" in f and "failed" in f for f in black["flags"])
    # The failed attempt's spend still counts into the item.
    assert black["cost_usd"] == pytest.approx(
        5 * CollisionChain.CUT_COST
        + 2 * CollisionChain.CROP_COST
        + 2 * CollisionChain.DRAW_COST
    )
    assert fake.draw_calls == 3


def test_collision_restage_skipped_when_budget_would_be_exceeded(tmp_path, monkeypatch):
    def exploding_probe(fake):
        calls = {"n": 0}

        def probe():
            calls["n"] += 1
            # Honest through the item loop (spend_before + one check
            # per item), then the ledger jumps past the budget, as
            # it can when the last item's own calls settle late.
            if calls["n"] <= 3:
                return fake.spend
            return fake.spend + 100.0

        return probe

    manifest, fake = _hat_room(tmp_path, monkeypatch, spend_probe=exploding_probe)
    items = {i["name"]: i for i in manifest["items"]}
    black = items["black hat"]
    assert black["status"] == "staged"
    assert black["primary_photo_index"] == 2  # no re-stage was attempted
    assert "straw hat" in black["possible_duplicate_of"]
    assert "primary_reselected_after_duplicate_collision" not in black["flags"]
    assert any("skipped" in f and "budget" in f for f in black["flags"])
    assert fake.draw_calls == 2


def test_mask_overlap_fraction_from_stored_foregrounds(tmp_path):
    store = AssetStore(tmp_path)

    def fg(rect):
        x0, y0, x1, y1 = rect
        img = Image.new("RGBA", (48, 36), (0, 0, 0, 0))
        px = img.load()
        for y in range(y0, y1):
            for x in range(x0, x1):
                px[x, y] = (200, 30, 30, 255)
        return store.put_image(img, kind="foreground_png")

    shared = fg(OBJECT_RECTS["straw_shared"])
    assert roomrun._mask_overlap_fraction(store, shared, shared) == pytest.approx(1.0)
    disjoint = fg(OBJECT_RECTS["fedora"])
    assert roomrun._mask_overlap_fraction(store, shared, disjoint) == pytest.approx(0.0)
    subset = fg((4, 4, 14, 20))  # inside straw_shared
    assert roomrun._mask_overlap_fraction(store, shared, subset) == pytest.approx(1.0)
def _hat_room_round2(tmp_path, monkeypatch, *, crop_plans=None, spend_probe=None):
    """Round-2 acceptance shape in miniature.

    Photo 0 is the still: it holds straw hat's primary (straw_big)
    and black hat's fedora. Photo 2 is a frame where BOTH items cut
    the same straw object (straw_shared): that sighting is black
    hat's primary (320 px beats the fedora's 256) but only a
    non-primary sighting of the straw hat, whose primary sits on
    photo 0. No same-photo primary pair ever collides.
    """
    store = AssetStore(tmp_path)
    still = _photo(tmp_path, "still.jpg")
    frames = [
        _photo(tmp_path, "frame1.jpg", (200, 90, 10)),
        _photo(tmp_path, "frame2.jpg", (210, 95, 12)),
        _photo(tmp_path, "frame3.jpg", (100, 100, 100)),
    ]
    _with_frames(monkeypatch, frames)
    contents = {
        (0, "straw hat"): "straw_big",
        (1, "straw hat"): "straw_other",
        (2, "straw hat"): "straw_shared",
        (0, "black hat"): "fedora",
        (1, "hat"): "straw_other",
        (2, "hat"): "straw_shared",
    }
    fake = CollisionChain(
        store,
        plans=[
            _plan("straw hat"),
            _plan("straw hat"),
            _plan("straw hat"),
            _plan("gray storage box"),
        ],
        contents=contents,
        crop_plans=crop_plans
        if crop_plans is not None
        else [
            _plan("straw hat", title="Straw Sun Hat"),
            _plan("black hat", title="Black Fedora Hat"),
        ],
    )
    run_kwargs = {}
    if spend_probe is not None:
        run_kwargs["spend_probe"] = spend_probe(fake)
    manifest = _run(
        tmp_path,
        [still],
        fake,
        videos=[tmp_path / "walk.mp4"],
        item_names=["straw hat", "black hat"],
        **run_kwargs,
    )
    return manifest, fake


def test_sighting_level_collision_restages_displaced_from_its_still(tmp_path, monkeypatch):
    manifest, fake = _hat_room_round2(tmp_path, monkeypatch)
    items = {i["name"]: i for i in manifest["items"]}
    straw, black = items["straw hat"], items["black hat"]

    # The collision is visible only through straw's non-primary
    # sighting on photo 2; the primaries sit on different photos.
    assert straw["primary_photo_index"] == 0
    assert black["status"] == "staged"
    assert black["primary_photo_index"] == 0  # re-staged from the still (fedora)
    assert black["also_seen_in"] == [1, 2]
    assert black["verification"]["ok"] is True
    assert black["receipt"]["opaque_pixels"] == 256  # the fedora
    assert "primary_reselected_after_duplicate_collision" in black["flags"]
    assert any("straw hat" in n and "black hat" in n for n in manifest["notes"])
    # Listing recomputed from the new primary.
    assert black["listing"]["source"] == "item_crop"
    assert black["listing"]["title"] == "Black Fedora Hat"
    # Sighting-level duplicate flags are mutual and persist after
    # the successful re-selection.
    assert "straw hat" in black["possible_duplicate_of"]
    assert "black hat" in straw["possible_duplicate_of"]


def test_keeper_whose_primary_is_elsewhere_is_not_moved(tmp_path, monkeypatch):
    manifest, fake = _hat_room_round2(tmp_path, monkeypatch)
    items = {i["name"]: i for i in manifest["items"]}
    straw = items["straw hat"]
    assert straw["status"] == "staged"
    assert straw["primary_photo_index"] == 0
    assert straw["listing"]["source"] == "photo_plan"
    assert straw["receipt"]["opaque_pixels"] == 768  # straw_big, untouched
    assert "primary_reselected_after_duplicate_collision" not in straw["flags"]
    assert fake.draw_calls == 3  # straw once, black twice (stage + re-stage)


def test_sighting_level_collision_with_ambiguous_evidence_flags_only(tmp_path, monkeypatch):
    # Black hat's crop evidence names the black hat itself, so both
    # items qualify as keeper and nobody is displaced.
    crop_plans = [_plan("black hat", title="Black Fedora Hat")]
    manifest, fake = _hat_room_round2(tmp_path, monkeypatch, crop_plans=crop_plans)
    items = {i["name"]: i for i in manifest["items"]}
    black = items["black hat"]
    assert black["status"] == "staged"
    assert black["primary_photo_index"] == 2  # original primary kept
    assert "straw hat" in black["possible_duplicate_of"]
    assert "black hat" in items["straw hat"]["possible_duplicate_of"]
    assert "primary_reselected_after_duplicate_collision" not in black["flags"]
    assert fake.draw_calls == 2
