from copy import deepcopy
import json
import pytest
from PIL import Image
from agenthand import composite
from agenthand.assets import AssetIntegrityError, AssetStore, KIND_FOREGROUND
from test_fidelity_properties import _small_run


def test_schema4_recomputes_all_quality_blocks_without_writing(tmp_path, monkeypatch):
    store, h, r = _small_run(tmp_path)
    assert r["receipt_schema"] == 4

    def forbidden(*a, **kw):
        pytest.fail("verification wrote an asset")

    monkeypatch.setattr(store, "put_image", forbidden)
    monkeypatch.setattr(store, "put_bytes", forbidden)
    report = composite.verify_receipt_from_disk(store, h)
    assert report["ok"], report["problems"]
    assert report["checks"]["cut_quality_recomputed"]
    assert report["checks"]["plate_quality_recomputed"]
    assert report["checks"]["quality_layer_reproduction"]


@pytest.mark.parametrize("field", ["cut_quality", "plate_quality", "reconstruction"])
@pytest.mark.parametrize("mutation", ["drop", "null", "retype"])
def test_required_blocks_fail_closed(tmp_path, field, mutation):
    store, h, r = _small_run(tmp_path)
    r = deepcopy(r)
    if mutation == "drop":
        r.pop(field)
    else:
        r[field] = None if mutation == "null" else []
    result = composite.verify_receipt_from_disk(store, h, r)
    assert not result["ok"] and not result["checks"]["receipt_shape"]


@pytest.mark.parametrize("schema", [None, 1, 2, 3])
def test_schema4_downgrade_guard(tmp_path, schema):
    store, h, r = _small_run(tmp_path)
    r["receipt_schema"] = schema
    assert not composite.verify_receipt_from_disk(store, h, r)["ok"]
    r.pop("cut_quality")
    r.pop("plate_quality")
    assert not composite.verify_receipt_from_disk(store, h, r)[
        "ok"
    ]  # reconstruction policy guard


@pytest.mark.parametrize("version", [2, 3])
def test_legacy_receipt_policies_still_verify(tmp_path, version):
    from pathlib import Path
    import hashlib
    import shutil

    fixture = Path(__file__).parent / "fixtures" / "legacy" / f"schema{version}"
    manifest = json.loads((fixture / "fixture.json").read_text())
    shutil.copytree(fixture, tmp_path, dirs_exist_ok=True)
    data = (tmp_path / manifest["receipt_file"]).read_bytes()
    assert hashlib.sha256(data).hexdigest() == manifest["receipt_sha256"]
    assert json.loads(data)["receipt_schema"] == version
    store = AssetStore(tmp_path)
    result = composite.verify_receipt_from_disk(store, manifest["output_asset"])
    assert result["ok"], result["problems"]
    assert "plate_quality_recomputed" not in result["checks"]


@pytest.mark.parametrize("schema", [None, 1, 2, 3])
@pytest.mark.parametrize("persisted", [False, True])
def test_fully_stripped_v4_cannot_verify_under_legacy_policy(
    tmp_path, schema, persisted
):
    from agenthand.assets import COMPOSITE_POLICY_PNG_KEY, png_text_of_bytes

    store, h, r = _small_run(tmp_path)
    stamp = json.loads(png_text_of_bytes(store.read_bytes(h))[COMPOSITE_POLICY_PNG_KEY])
    assert stamp["receipt_schema"] == 4
    if not persisted:
        store.receipt_path(h).unlink()
    r = deepcopy(r)
    r.pop("cut_quality")
    r.pop("plate_quality")
    if schema == 3:
        for field in ("policy_version", "max_fraction", "attempted"):
            r["reconstruction"].pop(field)
    else:
        r.pop("reconstruction")
    if schema is None:
        for field in composite._V2_ONLY_FIELDS:
            r.pop(field, None)
        r.pop("receipt_schema")
    else:
        r["receipt_schema"] = schema
    result = composite.verify_receipt_from_disk(store, h, r)
    assert not result["ok"]
    if schema != 1:  # Explicit schema 1 is already invalid by contract.
        assert result["checks"]["receipt_shape"]
        assert result["checks"].get("output_policy_stamp") is False


@pytest.mark.parametrize(
    "path,value",
    [
        (("cut_quality", "before", "metrics", "mask_pixels"), 999),
        (("cut_quality", "after", "metrics", "components"), 8),
        (("plate_quality", "assessment", "metrics", "uniform_studio"), False),
        (("reconstruction", "guard_ring_px"), 0),
        (("reconstruction", "max_fraction"), 1.0),
        (("plate_quality", "fallback"), True),
    ],
)
def test_quality_claim_mutations_are_recomputed(tmp_path, path, value):
    store, h, r = _small_run(tmp_path)
    r = deepcopy(r)
    at = r
    for part in path[:-1]:
        at = at[part]
    at[path[-1]] = value
    assert not composite.verify_receipt_from_disk(store, h, r)["ok"]


def test_foreground_fabrication_is_refused_before_plate(tmp_path, monkeypatch):
    store, h, r = _small_run(tmp_path)
    fg = store.read_image(r["foreground_asset"])
    fg.putpixel((2, 2), (255, 0, 255, 255))
    record = store.get_record(r["foreground_asset"])
    bad = store.put_image(
        fg, kind=KIND_FOREGROUND, source_asset_id=record.source_asset_id
    )
    monkeypatch.setattr(
        composite,
        "generate_background_plate",
        lambda *a: pytest.fail("plate generation reached"),
    )
    with pytest.raises(composite.FidelityLockError, match="source photo"):
        composite.draw_product(store, bad, "room")


def test_changed_mask_file_refuses_before_plate(tmp_path, monkeypatch):
    store, h, r = _small_run(tmp_path)
    store.resolve(r["mask_asset"]).write_bytes(b"changed-mask")
    monkeypatch.setattr(
        composite,
        "generate_background_plate",
        lambda *a: pytest.fail("plate generation reached"),
    )
    with pytest.raises(AssetIntegrityError):
        composite.draw_product(store, r["foreground_asset"], "room")


def test_cli_verifies_stored_schema4(tmp_path, capsys):
    from agenthand.verify import main

    store, h, r = _small_run(tmp_path)
    assert main(["--data-dir", str(tmp_path), "--asset", h]) == 0
    report = json.loads(capsys.readouterr().out)
    assert (
        report["ok"] and report["recomputed"]["cut_quality"]["after"]["grade"] == "PASS"
    )


def test_opaque_foreground_cannot_fabricate_source_alpha(tmp_path):
    from agenthand.assets import KIND_SOURCE

    store = AssetStore(tmp_path)
    source = store.put_image(
        Image.new("RGBA", (8, 6), (100, 100, 100, 0)), kind=KIND_SOURCE
    )
    fg = Image.new("RGBA", (8, 6))
    fg.paste((100, 100, 100, 255), (2, 1, 6, 5))
    handle = store.put_image(
        fg, kind=KIND_FOREGROUND, source_asset_id=store.parse_handle(source)
    )
    with pytest.raises(composite.FidelityLockError, match="source photo"):
        composite.preflight_cut(store, handle)


def test_actual_retained_v1_receipt_verifies_under_legacy_policy(tmp_path):
    from pathlib import Path
    from agenthand.assets import AssetRecord

    docs = Path(__file__).resolve().parents[1] / "docs"
    replay = json.loads((docs / "replay-run-v1.json").read_text())
    receipt = json.loads((docs / "proof-receipt-v1.json").read_text())
    store = AssetStore(tmp_path)
    store.assets_dir.mkdir(parents=True)
    # Restore original IDs and byte-exact assets; do not issue a new receipt.
    for asset in replay["assets"]:
        asset_id = store.parse_handle(asset["asset"])
        filename = asset_id + ".png"
        (store.assets_dir / filename).write_bytes(
            (docs / asset["site_file"]).read_bytes()
        )
        parent = (
            store.parse_handle(asset["source_asset"]) if asset["source_asset"] else None
        )
        store._save_record(
            AssetRecord(
                asset_id=asset_id,
                filename=filename,
                kind=asset["kind"],
                sha256=asset["sha256"],
                pixel_sha256=asset["pixel_sha256"],
                width=asset["width"],
                height=asset["height"],
                byte_size=asset["byte_size"],
                created_at=asset["created_at"],
                source_asset_id=parent,
            )
        )
    result = composite.verify_receipt_from_disk(store, receipt["output_asset"], receipt)
    assert result["ok"], result["problems"]
    assert result["recomputed"]["opaque_pixels"] == 580_613
    assert result["recomputed"]["changed_opaque_pixels"] == 0
    assert "plate_quality_recomputed" not in result["checks"]
