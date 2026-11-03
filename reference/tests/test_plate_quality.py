import base64
from pathlib import Path
import numpy as np
import pytest
from PIL import Image, ImageDraw
from agenthand import meta_client, composite, server, config, ledger
from agenthand.plate_quality import assess_plate
from agenthand.assets import AssetStore, KIND_SOURCE, KIND_FOREGROUND
from test_draw import _FakeClient, _image_response, _install_fake

FIXTURES = Path(__file__).parent / "fixtures" / "wonder"


@pytest.mark.parametrize(
    "name,grade", [("plate-v1-podium.png", "FAIL"), ("plate-v3.png", "PASS")]
)
def test_supplied_plate_fixtures(name, grade):
    quality = assess_plate(
        Image.open(FIXTURES / name), [1563, 863, 4419, 3009], (5712, 4284)
    )
    assert quality["grade"] == grade, quality


def test_stage_spec_is_constrained_and_versioned():
    spec = meta_client.StageSpec.from_scene("a warm room with concrete floor")
    assert spec.floor == "matte concrete"
    assert meta_client.wrap_plate_prompt(spec) == meta_client.wrap_plate_prompt(
        "warm concrete room"
    )
    assert "where an object will be placed" not in meta_client.wrap_plate_prompt(spec)
    with pytest.raises(ValueError):
        meta_client.StageSpec(floor="podium")


def test_central_object_interrupts_floor():
    image = Image.new("RGB", (640, 480), "white")
    d = ImageDraw.Draw(image)
    d.rectangle((0, 260, 640, 480), fill=(180, 165, 140))
    d.rectangle((260, 290, 380, 400), fill=(30, 30, 30))
    q = assess_plate(image, [180, 80, 470, 350], image.size)
    assert q["grade"] == "FAIL", q
    assert (
        "CENTRAL_OBJECT" in q["codes"]
        or "RAISED_SLAB_OR_INTERRUPTED_FLOOR" in q["codes"]
    )


def seed(store):
    source = Image.new("RGBA", (600, 450), (180, 180, 180, 255))
    sh = store.put_image(source, kind=KIND_SOURCE)
    fg = Image.new("RGBA", source.size)
    fg.paste(
        source,
        (0, 0),
        Image.fromarray(
            np.pad(np.full((230, 320), 255, np.uint8), ((85, 135), (150, 130)))
        ),
    )
    return store.put_image(
        fg, kind=KIND_FOREGROUND, source_asset_id=store.parse_handle(sh)
    )


@pytest.mark.parametrize(
    "second,fallback", [("plate-v3.png", False), ("plate-v1-podium.png", True)]
)
def test_two_billed_attempts_then_accepted_plate_or_disclosed_fallback(
    tmp_path, monkeypatch, second, fallback
):
    monkeypatch.setenv("AGENTHAND_DATA_DIR", str(tmp_path))
    monkeypatch.setenv("META_API_KEY", "test-not-real")
    monkeypatch.delenv("AGENTHAND_DRAW_PLACEHOLDER_PLATE", raising=False)
    monkeypatch.setattr(config, "_DOTENV_LOADED", True)
    responses = [
        _image_response(b64_json=base64.b64encode((FIXTURES / n).read_bytes()).decode())
        for n in ("plate-v1-podium.png", second)
    ]
    client = _install_fake(monkeypatch, _FakeClient(responses))
    store = AssetStore(tmp_path)
    fh = seed(store)
    result, _ = server.draw(fh, "soft daylight wood room")
    assert result["status"] == "ok"
    assert len(client.images.calls) == 2
    q = result["fidelity_receipt"]["plate_quality"]
    assert q["fallback"] is fallback and len(q["attempts"]) == 2
    assert q["assessment"]["grade"] == "PASS"
    assert result["usage"]["images"] == 2
    assert result["cost_usd"] == 2 * meta_client.project_draw_cost_usd("muse-image-1.0")
    assert ledger.ledger_total_usd(config.get_ledger_path()) == result["cost_usd"]
    if fallback:
        assert result["fidelity_receipt"]["plate_kind"] == "code_rendered"
        assert q["fallback_reason"] == "two_rejected_plates"
    report = composite.verify_receipt_from_disk(store, result["output_asset"])
    assert report["ok"], report["problems"]


@pytest.mark.parametrize("bottom", [0.50, 0.55, 0.60, 0.65, 0.70])
@pytest.mark.parametrize("name", ["plate-v3.png", "plate-v1-podium.png"])
def test_position_horizon_failure_skips_identical_second_purchase(
    tmp_path, monkeypatch, bottom, name
):
    from agenthand.quality_receipt import is_plate_block
    from agenthand import plate_quality

    monkeypatch.setenv("AGENTHAND_DATA_DIR", str(tmp_path))
    monkeypatch.setattr(config, "_DOTENV_LOADED", True)
    image = Image.open(FIXTURES / name)
    bbox = [1500, int(bottom * 3000) - 1200, 2500, int(bottom * 3000)]
    original = assess_plate(image, bbox, (4000, 3000))
    calls = []

    def request(*args, **kwargs):
        calls.append(1)
        return (FIXTURES / name).read_bytes(), {"images": 1}

    monkeypatch.setattr(meta_client, "_request_plate_image", request)
    store = AssetStore(tmp_path)
    h, billing = meta_client.generate_background_plate(
        "room", 4000, 3000, store=store, foreground_bbox=bbox
    )
    q = billing["plate_quality"]
    assert q["assessment"]["grade"] == "PASS"
    assert is_plate_block({"version": plate_quality.VERSION, **q})
    if "WALL_FLOOR_LINE_IN_PLACEMENT" in original["codes"]:
        assert len(calls) == 1 and q["fallback"]
        assert q["fallback_reason"] == "placement_horizon_mismatch"
    elif name == "plate-v3.png":
        assert original["grade"] == "PASS" and not q["fallback"]
        assert len(calls) == 1
    else:
        assert original["grade"] == "FAIL" and q["fallback"]
    assert billing["actual_usd"] == len(calls) * meta_client.project_draw_cost_usd(
        "muse-image-1.0"
    )
    assert q["attempts"][0]["quality"] == original


def test_one_horizon_failure_fallback_receipt_recomputes(tmp_path, monkeypatch):
    monkeypatch.setenv("AGENTHAND_DATA_DIR", str(tmp_path))
    monkeypatch.setenv("META_API_KEY", "test-not-real")
    monkeypatch.delenv("AGENTHAND_DRAW_PLACEHOLDER_PLATE", raising=False)
    monkeypatch.setattr(config, "_DOTENV_LOADED", True)
    client = _install_fake(
        monkeypatch,
        _FakeClient(
            [
                _image_response(
                    b64_json=base64.b64encode(
                        (FIXTURES / "plate-v3.png").read_bytes()
                    ).decode()
                )
            ]
        ),
    )
    store = AssetStore(tmp_path)
    source = Image.new("RGBA", (600, 450), (180, 180, 180, 255))
    sh = store.put_image(source, kind=KIND_SOURCE)
    fg = Image.new("RGBA", source.size)
    fg.paste((180, 180, 180, 255), (200, 60, 400, 225))
    fh = store.put_image(
        fg, kind=KIND_FOREGROUND, source_asset_id=store.parse_handle(sh)
    )
    result, _ = server.draw(fh, "room")
    assert len(client.images.calls) == 1
    q = result["fidelity_receipt"]["plate_quality"]
    assert q["fallback_reason"] == "placement_horizon_mismatch"
    checked = composite.verify_receipt_from_disk(store, result["output_asset"])
    assert checked["ok"], checked["problems"]
    from copy import deepcopy
    from agenthand.quality_receipt import is_plate_block

    q = deepcopy(q)
    q["attempts"][0]["quality"]["codes"] = ["CENTRAL_OBJECT"]
    assert not is_plate_block(q)


def test_second_plate_error_reports_settled_attempt_plus_open_reservation(
    tmp_path, monkeypatch
):
    from types import SimpleNamespace

    monkeypatch.setenv("AGENTHAND_DATA_DIR", str(tmp_path))
    monkeypatch.setenv("META_API_KEY", "test-not-real")
    monkeypatch.delenv("AGENTHAND_DRAW_PLACEHOLDER_PLATE", raising=False)
    monkeypatch.setattr(config, "_DOTENV_LOADED", True)
    _install_fake(
        monkeypatch,
        _FakeClient(
            [
                _image_response(
                    b64_json=base64.b64encode(
                        (FIXTURES / "plate-v1-podium.png").read_bytes()
                    ).decode()
                ),
                SimpleNamespace(data=[]),
            ]
        ),
    )
    store = AssetStore(tmp_path)
    result = server.draw(seed(store), "room")
    assert result["error"]["code"] == "BAD_IMAGE_RESPONSE"
    counted = 2 * meta_client.project_draw_cost_usd("muse-image-1.0")
    assert result["error"]["cost_counted_usd"] == counted
    assert ledger.ledger_total_usd(config.get_ledger_path()) == counted
