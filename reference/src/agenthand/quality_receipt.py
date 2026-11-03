"""Schema-4 quality disclosure validation and read-only reproduction.

Legacy receipt policies deliberately do not call this module's verifier.
"""

from __future__ import annotations
from dataclasses import asdict
import numpy as np
from . import cut_quality, plate_quality, paperfill, backdrop
from .assets import HANDLE_PREFIX, KIND_MASK, KIND_PLATE, KIND_REGION_MAP
from .meta_client import StageSpec


def is_grade(value, *, plate=False):
    if not isinstance(value, dict):
        return False
    required = {"version", "grade", "codes", "metrics"} | (
        set() if plate else {"instructions", "source_evidence"}
    )
    if not required.issubset(value):
        return False
    if value["version"] != (plate_quality.VERSION if plate else cut_quality.VERSION):
        return False
    if value["grade"] not in (
        ("PASS", "FAIL") if plate else ("PASS", "REPAIRABLE", "RESHOOT")
    ):
        return False
    if not isinstance(value["codes"], list) or not all(
        isinstance(c, str) for c in value["codes"]
    ):
        return False
    if not isinstance(value["metrics"], dict):
        return False
    if not plate:
        if type(value["source_evidence"]) is not bool:
            return False
        if not isinstance(value["instructions"], list) or not all(
            isinstance(s, str) for s in value["instructions"]
        ):
            return False
    return True


def is_cut_block(value):
    return (
        isinstance(value, dict)
        and value.get("version") == cut_quality.VERSION
        and is_grade(value.get("before"))
        and is_grade(value.get("after"))
        and value["before"]["grade"] in ("PASS", "REPAIRABLE")
        and value["after"]["grade"] == "PASS"
    )


def is_reconstruction_policy(value):
    if not isinstance(value, dict):
        return False
    if (
        value.get("policy_version") != paperfill.RENDERER_VERSION
        or value.get("max_fraction") != paperfill.MAX_RECONSTRUCTION_FRACTION
    ):
        return False
    if (
        type(value.get("attempted")) is not bool
        or type(value.get("guard_ring_px")) is not int
        or value["guard_ring_px"] != paperfill.GUARD_RING_PX
    ):
        return False
    evidence = value.get("evidence")
    if not isinstance(evidence, dict):
        return False
    for key in ("support_mask_handles", "occluder_mask_handles"):
        if not isinstance(evidence.get(key), list) or not all(
            isinstance(h, str) and h.startswith(HANDLE_PREFIX) for h in evidence[key]
        ):
            return False
    if value["reconstruction_fraction"] > paperfill.MAX_RECONSTRUCTION_FRACTION:
        return False
    if not value["attempted"]:
        return (
            value["status"] == "none"
            and not value["layer_asset"]
            and not value["regions"]
            and not value["refusals"]
        )
    if value.get("renderer") != paperfill.RENDERER_VERSION:
        return False
    if value["status"] == "applied" and not value["region_map_asset"]:
        return False
    return True


def is_plate_block(value):
    if not isinstance(value, dict) or value.get("version") != plate_quality.VERSION:
        return False
    if (
        not is_grade(value.get("assessment"), plate=True)
        or value["assessment"]["grade"] != "PASS"
    ):
        return False
    if type(value.get("fallback")) is not bool or "fallback_reason" not in value:
        return False
    attempts = value.get("attempts")
    if not isinstance(attempts, list) or len(attempts) > 2:
        return False
    for item in attempts:
        if not isinstance(item, dict) or not is_grade(item.get("quality"), plate=True):
            return False
        if not isinstance(item.get("plate_asset"), str) or not item[
            "plate_asset"
        ].startswith(HANDLE_PREFIX):
            return False
        for key in ("sha256", "pixel_sha256"):
            h = item.get(key)
            if (
                not isinstance(h, str)
                or len(h) != 64
                or any(c not in "0123456789abcdef" for c in h)
            ):
                return False
        if not isinstance(item.get("call_id"), str) or not item["call_id"]:
            return False
    spec = value.get("stage_spec")
    if "stage_spec" not in value:
        return False
    if spec is not None:
        if not isinstance(spec, dict):
            return False
        try:
            if asdict(StageSpec(**spec)) != spec:
                return False
        except (TypeError, ValueError):
            return False
    if attempts and spec is None:
        return False
    if value["fallback"]:
        if not attempts or not all(i["quality"]["grade"] == "FAIL" for i in attempts):
            return False
        reason = value.get("fallback_reason")
        horizon_failure = (
            "WALL_FLOOR_LINE_IN_PLACEMENT" in attempts[-1]["quality"]["codes"]
        )
        return (
            len(attempts) == 1
            and horizon_failure
            and reason == "placement_horizon_mismatch"
        ) or (
            len(attempts) == 2
            and not any(
                "WALL_FLOOR_LINE_IN_PLACEMENT" in i["quality"]["codes"]
                for i in attempts[:-1]
            )
            and reason
            == (
                "placement_horizon_mismatch"
                if horizon_failure
                else "two_rejected_plates"
            )
        )
    return value.get("fallback_reason") is None and (
        not attempts
        or (
            attempts[-1]["quality"]["grade"] == "PASS"
            and all(i["quality"]["grade"] == "FAIL" for i in attempts[:-1])
        )
    )


def verify_quality(store, foreground_handle, receipt):
    """Recompute quality and reconstruction; no model calls or asset writes."""
    from . import composite

    checks = {}
    data = {}

    def check(name, ok, detail=""):
        checks[name] = (bool(ok), detail)

    chain = composite.validate_foreground(store, foreground_handle)
    check(
        "quality_source_identity",
        HANDLE_PREFIX + chain[-1].asset_id == receipt["source_asset"],
    )
    actual_mask = (
        HANDLE_PREFIX + chain[1].asset_id if chain[1].kind == KIND_MASK else None
    )
    check("quality_mask_identity", actual_mask == receipt["mask_asset"])
    source = store.read_image(receipt["source_asset"])
    fg = store.read_image(foreground_handle)
    reconstruction = receipt["reconstruction"]
    evidence = reconstruction["evidence"]
    before = composite.preflight_cut(
        store,
        foreground_handle,
        occluder_mask_handles=tuple(evidence["occluder_mask_handles"]),
    )
    after = composite.preflight_cut(
        store,
        foreground_handle,
        reconstruction_handle=reconstruction["layer_asset"],
        require_pass=True,
        occluder_mask_handles=tuple(evidence["occluder_mask_handles"]),
    )
    block = {"version": cut_quality.VERSION, "before": before, "after": after}
    data["cut_quality"] = block
    check(
        "cut_quality_recomputed",
        block == receipt["cut_quality"],
        "source/mask/paper grade disagrees with disclosure",
    )
    if reconstruction["attempted"]:
        supports, occluders = paperfill.load_typed_evidence(
            store,
            foreground_handle,
            support_mask_handles=tuple(evidence["support_mask_handles"]),
            occluder_mask_handles=tuple(evidence["occluder_mask_handles"]),
        )
        layer, region_map, expected = paperfill.plan_reconstruction(
            np.asarray(source.convert("RGB")),
            np.asarray(fg.getchannel("A")),
            supports,
            occluders,
            support_mask_handles=tuple(evidence["support_mask_handles"]),
            occluder_mask_handles=tuple(evidence["occluder_mask_handles"]),
        )
        fields = (
            "status",
            "renderer",
            "evidence",
            "guard_ring_px",
            "motif",
            "regions",
            "refusals",
            "reconstruction_pixels",
            "unfilled_gap_pixels",
        )
        check(
            "reconstruction_recomputed",
            all(expected[k] == reconstruction[k] for k in fields),
            "rendered evidence/provenance differs from receipt",
        )
        if reconstruction["layer_asset"]:
            actual_layer = store.read_image(reconstruction["layer_asset"])
            check(
                "reconstruction_render_matches",
                actual_layer.tobytes() == layer.tobytes(),
                "paper is not the deterministic render of source evidence",
            )
            map_record = store.get_record(reconstruction["region_map_asset"])
            layer_record = store.get_record(reconstruction["layer_asset"])
            check(
                "reconstruction_map_lineage",
                map_record.kind == KIND_REGION_MAP
                and map_record.source_asset_id == layer_record.asset_id,
            )
            actual_map = store.read_image(reconstruction["region_map_asset"]).convert(
                "L"
            )
            check(
                "reconstruction_map_matches",
                actual_map.tobytes() == region_map.tobytes(),
            )
        data["reconstruction"] = {k: expected[k] for k in fields}
    plate = (
        store.read_image(receipt["plate_asset"])
        if receipt["plate_asset"]
        else composite.generate_background_plate(receipt["scene_prompt"], *fg.size)
    )
    if receipt["plate_asset"]:
        record = store.get_record(receipt["plate_asset"])
        store.verify_integrity(receipt["plate_asset"])
        check(
            "quality_plate_identity",
            record.kind == KIND_PLATE
            and record.sha256 == receipt["plate_sha256"]
            and record.pixel_sha256 == receipt["plate_pixel_sha256"],
        )
    bbox = fg.getbbox() or [0, 0, fg.width, fg.height]
    assessment = plate_quality.assess_plate(
        plate,
        bbox,
        fg.size,
        code_backdrop=receipt.get("backdrop")
        if receipt["plate_kind"] == "code_rendered"
        else None,
    )
    plate_block = receipt["plate_quality"]
    check(
        "plate_quality_recomputed",
        assessment == plate_block["assessment"] and assessment["grade"] == "PASS",
        "fitted plate assessment differs",
    )
    data["plate_quality"] = assessment
    for i, attempt in enumerate(plate_block["attempts"]):
        record = store.get_record(attempt["plate_asset"])
        store.verify_integrity(attempt["plate_asset"])
        actual = plate_quality.assess_plate(
            store.read_image(attempt["plate_asset"]), bbox, fg.size
        )
        check(
            f"plate_attempt_{i + 1}_recomputed",
            record.kind == KIND_PLATE
            and record.sha256 == attempt["sha256"]
            and record.pixel_sha256 == attempt["pixel_sha256"]
            and actual == attempt["quality"],
        )
    if plate_block["fallback"]:
        check(
            "plate_fallback_disclosed",
            receipt["plate_kind"] == "code_rendered"
            and receipt.get("backdrop") == backdrop.backdrop_info("studio_gray"),
        )
    elif plate_block["attempts"]:
        check(
            "plate_selected_attempt",
            receipt["plate_asset"] == plate_block["attempts"][-1]["plate_asset"]
            and receipt["plate_kind"] == "model_generated",
        )
    layer = (
        store.read_image(reconstruction["layer_asset"])
        if reconstruction["layer_asset"]
        else None
    )
    edge_fix = receipt.get("edge_fix")
    if isinstance(edge_fix, dict) and edge_fix.get("applied"):
        # A source-derived plate's delivered composite carries the
        # edge fix (composite.py); reproduce it from the same stored
        # layers and the record's thin-structure flag. The record is
        # never graded — the pixel recount above is the grade — but
        # the reproduction must rebuild what draw actually delivered.
        rebuilt, _, _, _ = composite.composite_images_with_edge_fix(
            fg,
            plate,
            source,
            bool(edge_fix.get("thin_structure")),
            layer,
        )
    else:
        rebuilt, _, _ = composite.composite_images(fg, plate, layer)
    delivered = store.read_image(receipt["output_asset"])
    check(
        "quality_layer_reproduction",
        rebuilt.tobytes() == delivered.tobytes(),
        "delivery differs from its three layers",
    )
    return data, checks
