from io import BytesIO
import hashlib, json
import numpy as np
from PIL import Image
from agenthand import segmentation as seg, meta_client
from agenthand.sam_masks import DecodedMask


def test_identify_surface_remains_frozen():
    digest = hashlib.sha256(
        (
            meta_client._IDENTIFY_INSTRUCTIONS
            + json.dumps(meta_client._IDENTIFY_SCHEMA, sort_keys=True)
        ).encode()
    ).hexdigest()
    assert digest == "c7b529289c168126cc9f033024ccdb3c699a036f6b7075ab20856d678fc8b2bd"


def test_ensemble_tiles_preserve_records_and_recover_observed_legs(monkeypatch):
    src = np.full((400, 600, 3), 180, np.uint8)
    body = np.zeros((400, 600), np.uint8)
    body[100:300, 100:500] = 255
    truth = body.copy()
    for x in (150, 440):
        src[300:360, x : x + 8] = 15
        truth[300:360, x : x + 8] = 255
    tiles = seg.band_tiles((600, 400), (100, 100, 500, 300), 5)
    calls = []

    def segment(data, phrase, **kw):
        im = Image.open(BytesIO(data))
        calls.append((phrase, im.size))
        return str(len(calls) - 1), {}

    def parse(text, w, h):
        i = int(text)
        if i < 3:
            a = body.copy()
            a[70:80, 240:270] = 255
        else:
            b = tiles[i - 3]["bbox"]
            a = truth[b[1] : b[3], b[0] : b[2]]
        im = Image.fromarray(a)
        return [DecodedMask(str(i), (0, 0, w, h), int((a > 0).sum()), im)]

    monkeypatch.setattr(meta_client, "segment_image", segment)
    monkeypatch.setattr(seg, "parse_sam_masks", parse)
    im = Image.fromarray(src)
    b = BytesIO()
    im.save(b, format="PNG")
    result = seg.run_cut_pipeline(
        im,
        b.getvalue(),
        ["cabinet", "wood cabinet", "two door cabinet"],
        model="sam-3.1",
    )
    assert [c[0] for c in calls[:3]] == ["cabinet", "wood cabinet", "two door cabinet"]
    assert 3 < len(calls) <= 8 and all(
        w * h <= seg.SAM_MAX_PIXELS for _, (w, h) in calls
    )
    assert len(result["records"]) == len(calls)
    assert result["usage"]["images"] == len(calls)
    out = np.asarray(result["mask_image"])
    assert np.array_equal(out, truth)
    assert result["dropped_component_pixels"] == 300
    assert any(
        c["role"] == "top_band_candidate"
        for r in result["records"]
        for c in r["components"]
    )
    assert any(
        c["role"] == "support" for r in result["records"] for c in r["components"]
    )


def test_tile_budget_covers_both_bands_with_cap():
    tiles = seg.band_tiles((5712, 4284), (1563, 863, 4419, 3009), 5)
    assert 2 <= len(tiles) <= 5
    assert {t["kind"] for t in tiles} == {"support", "top"}
    for t in tiles:
        x0, y0, x1, y1 = t["bbox"]
        assert (x1 - x0) * (y1 - y0) <= seg.SAM_MAX_PIXELS
    for kind in ("support", "top"):
        band = sorted(
            (t for t in tiles if t["kind"] == kind), key=lambda t: t["bbox"][0]
        )
        assert band[0]["bbox"][0] == 1563 and band[-1]["bbox"][2] == 4419
        assert all(a["bbox"][2] >= b["bbox"][0] for a, b in zip(band, band[1:]))


def test_mask_review_discloses_omitted_component_area():
    mask = Image.new("L", (100, 100))
    mask.paste(255, (20, 20, 80, 80))
    result = seg.review_mask(mask, dropped_component_pixels=300)
    assert result["dropped_component_pixels"] == 300
    assert any(
        "300 pixels" in warning and "completeness" in warning
        for warning in result["warnings"]
    )


def test_tiles_use_successful_body_phrase_when_alternates_no_match(monkeypatch):
    body = np.zeros((400, 600), np.uint8)
    body[100:300, 100:500] = 255
    tiles = seg.band_tiles((600, 400), (100, 100, 500, 300), 5)
    calls = []

    def segment(data, phrase, **kwargs):
        calls.append(phrase)
        return str(len(calls) - 1), {}

    def parse(text, w, h):
        i = int(text)
        if i < 3 and i != 1:
            return []
        if i < 3:
            a = body
        else:
            x0, y0, x1, y1 = tiles[i - 3]["bbox"]
            a = body[y0:y1, x0:x1]
        return [
            DecodedMask(str(i), (0, 0, w, h), int((a > 0).sum()), Image.fromarray(a))
        ]

    monkeypatch.setattr(meta_client, "segment_image", segment)
    monkeypatch.setattr(seg, "parse_sam_masks", parse)
    source = Image.new("RGB", (600, 400), (180, 180, 180))
    data = BytesIO()
    source.save(data, format="PNG")
    result = seg.run_cut_pipeline(
        source,
        data.getvalue(),
        ["failed concept", "successful body", "failed alternate"],
        model="sam-3.1",
    )
    assert result["phrase"] == "successful body"
    assert len(calls) > 3 and set(calls[3:]) == {"successful body"}
    assert result["usage"]["images"] == len(calls) <= 8
    assert [trace["outcome"] for trace in result["attempts"][:3]] == [
        "no_match",
        "accepted",
        "no_match",
    ]
