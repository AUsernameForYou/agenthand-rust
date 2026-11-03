"""Standalone verification of exported files — no store, no models.

The J2 Gauntlet's demo fallback and must-fix 2: a small, independent
checker that works on the DELIVERED files on disk, so the fidelity
claim can be re-proven anywhere (on camera, on a judge's machine)
without the asset store or the code that did the compositing.

    agenthand-verify SOURCE FINAL MASK
    python -m agenthand.verify SOURCE FINAL MASK

SOURCE is the product photo (as exported from the store — already
EXIF-normalized by ingest), FINAL is the delivered composite, MASK is
the cut_out mask PNG. The product region is where the mask is fully on
(mask value 255): at those pixels the final must equal the source
EXACTLY — same canvas, integer offset zero, no tolerance, no colour
conversion. Semi-transparent mask pixels are the edge band: counted,
not compared. The check also compares the embedded ICC colour
profiles of source and final: identical pixel values under a changed
or stripped profile display as different colours, so a profile
mismatch fails verification even when every compared pixel matches.

Exit code 0 when verification passes, 1 when it fails; the JSON
report prints either way.

With --reconstruction LAYER (and --plate PLATE), the paper-fill
layer is checked too: no paper pixel may sit under the product mask,
and with the plate supplied the delivered file must equal plate,
then paper, then product recomposited exactly — the only way the
non-product pixels become verified rather than merely unexamined.
The report's ``non_product_pixels_verified`` is true only when that
recomposite identity was actually checked.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

import numpy as np
from PIL import Image, ImageOps

from .assets import (
    color_profile_sha256_of_image,
    describe_color_profile,
    icc_profile_of_image,
)


def _load_rgba(path: str | Path) -> Image.Image:
    with Image.open(path) as img:
        img.load()
        converted = img.convert("RGBA")
        icc = img.info.get("icc_profile")
        if icc:
            converted.info["icc_profile"] = bytes(icc)
        return converted


def verify_files(
    source_path: str | Path,
    final_path: str | Path,
    mask_path: str | Path,
    reconstruction_path: str | Path | None = None,
    plate_path: str | Path | None = None,
) -> dict:
    """Verify an exported composite against its source photo and mask.

    Returns the report dict (see the module docstring for the rules);
    ``ok`` is True only when sizes match, at least one product pixel
    was actually checked (an empty or all-edge-band mask refuses),
    every fully-masked product pixel is byte-identical between source
    and final, the colour profile is preserved, and stageability is not
    RESHOOT. Paper is regraded with the same guard-seam bridge as preflight.
    Undecodable or
    missing files raise — the report is for files that exist but fail
    the fidelity rules.
    """
    source = _load_rgba(source_path)
    final = _load_rgba(final_path)
    with Image.open(mask_path) as mask_img:
        mask_img.load()
        mask = mask_img.convert("L")

    problems: list[str] = []
    if final.size != source.size:
        problems.append(
            f"final is {final.size}, source is {source.size}; "
            "the product canvas must never be rescaled"
        )
    if mask.size != source.size:
        problems.append(
            f"mask is {mask.size}, source is {source.size}; "
            "the mask must cover the source frame exactly"
        )

    checked = changed = edge_band = outside = 0
    if not problems:
        source_pixels = np.frombuffer(source.tobytes(), dtype=np.uint8).reshape(-1, 4)
        final_pixels = np.frombuffer(final.tobytes(), dtype=np.uint8).reshape(-1, 4)
        mask_values = np.frombuffer(mask.tobytes(), dtype=np.uint8)
        full = mask_values == 255
        checked = int(np.count_nonzero(full))
        changed = int(
            np.count_nonzero(np.any(source_pixels != final_pixels, axis=1) & full)
        )
        edge_band = int(np.count_nonzero((mask_values > 0) & ~full))
        outside = int(np.count_nonzero(mask_values == 0))
        if changed:
            problems.append(
                f"{changed} of {checked} fully-masked product pixels "
                "differ between the source photo and the final image"
            )
        if checked == 0:
            # A mask that selects nothing checks nothing: before this
            # refusal, two completely different images passed with an
            # empty mask because 0 of 0 pixels changed (architecture
            # Gauntlet, GPT reproduction).
            problems.append(
                "the mask selects no fully-on pixels, so no product "
                "pixels were checked; a verification that compares "
                "nothing does not pass"
            )

    source_profile = color_profile_sha256_of_image(source)
    output_profile = color_profile_sha256_of_image(final)
    color_profile_preserved = source_profile == output_profile
    if not color_profile_preserved:
        problems.append(
            "colour profile changed between the source photo and the "
            "final image (source "
            f"{describe_color_profile(icc_profile_of_image(source))}, "
            f"final {describe_color_profile(icc_profile_of_image(final))}); "
            "identical pixel values would display as different colours"
        )

    # Paper-fill layer checks (only when the layer file is supplied):
    # the overlap count binds paper to the empty region, and the
    # recomposite identity — plate, then paper, then the product cut
    # from the source by the mask — is what promotes the rest of the
    # frame from unexamined to verified.
    paper = None
    reconstruction_pixels = None
    reconstruction_overlap = None
    recomposite_identical = None
    non_product_pixels_verified = False
    if reconstruction_path is not None:
        recon = _load_rgba(reconstruction_path)
        if recon.size != source.size:
            problems.append(
                f"reconstruction layer is {recon.size}, source is "
                f"{source.size}; paper is rendered on the product's frame"
            )
        elif mask.size == source.size:
            recon_alpha = np.frombuffer(recon.getchannel("A").tobytes(), dtype=np.uint8)
            mask_values = np.frombuffer(mask.tobytes(), dtype=np.uint8)
            paper = recon_alpha > 0
            reconstruction_pixels = int(np.count_nonzero(paper))
            reconstruction_overlap = int(np.count_nonzero(paper & (mask_values > 0)))
            if reconstruction_overlap:
                problems.append(
                    f"reconstruction places paper on "
                    f"{reconstruction_overlap} pixels inside the "
                    "product mask; paper may only fill where the cut "
                    "is empty"
                )
            if plate_path is not None and not problems:
                plate = _load_rgba(plate_path)
                fitted = (
                    plate
                    if plate.size == source.size
                    else ImageOps.fit(plate, source.size)
                )
                foreground = source.copy()
                foreground.putalpha(mask)
                rebuilt = Image.alpha_composite(
                    Image.alpha_composite(fitted, recon), foreground
                )
                recomposite_identical = rebuilt.tobytes() == final.tobytes()
                if recomposite_identical:
                    non_product_pixels_verified = True
                else:
                    problems.append(
                        "the final image is not the exact composite "
                        "of plate, reconstruction layer, and product; "
                        "pixels outside the three layers are present"
                    )

    from .cut_quality import grade_cut
    from .mask_evidence import repaired_alpha
    from .paperfill import GUARD_RING_PX, MAX_RECONSTRUCTION_FRACTION
    import cv2

    quality = grade_cut(source, mask) if source.size == mask.size else None
    before = quality
    if paper is not None:
        alpha = np.asarray(mask)
        paper = paper.reshape(alpha.shape)
        near = (
            cv2.dilate(
                (alpha > 0).astype(np.uint8),
                np.ones((3, 3), np.uint8),
                iterations=GUARD_RING_PX,
            )
            > 0
        )
        if (paper & near).any():
            problems.append("paper violates foreground guard ring")
        fraction = int(paper.sum()) / max(
            1, int(paper.sum()) + int((alpha == 255).sum())
        )
        if fraction > MAX_RECONSTRUCTION_FRACTION:
            problems.append("reconstruction exceeds maximum fraction")
        # As in preflight_cut, a fatal raw cut cannot be rescued by paper.
        if before["grade"] != "RESHOOT":
            quality = grade_cut(source, repaired_alpha(alpha, paper))
    fidelity_ok = not problems
    if quality and quality["grade"] == "RESHOOT":
        problems.append("stageability: RESHOOT (" + ", ".join(quality["codes"]) + ")")

    return {
        "cut_quality": quality,
        "cut_quality_before": before,
        "fidelity_ok": fidelity_ok,
        "ok": not problems,
        "source": str(source_path),
        "final": str(final_path),
        "mask": str(mask_path),
        "checked_pixels": checked,
        "changed_pixels": changed,
        "edge_band_pixels": edge_band,
        "outside_mask_pixels": outside,
        "color_profile_source_sha256": source_profile,
        "color_profile_output_sha256": output_profile,
        "color_profile_preserved": color_profile_preserved,
        "reconstruction_pixels": reconstruction_pixels,
        "reconstruction_overlap_product_pixels": reconstruction_overlap,
        "recomposite_identical": recomposite_identical,
        "non_product_pixels_verified": non_product_pixels_verified,
        "problems": problems,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="agenthand-verify",
        description=(
            "Verify a delivered agenthand composite against its source "
            "photo and mask, from the files on disk. Exit 0 = the "
            "fidelity and stageability checks hold; 1 = a check fails."
        ),
    )
    parser.add_argument(
        "source", nargs="?", help="the source product photo (exported asset)"
    )
    parser.add_argument("final", nargs="?", help="the delivered composite image")
    parser.add_argument("mask", nargs="?", help="the cut_out mask PNG")
    parser.add_argument(
        "--reconstruction",
        help="the paper-fill reconstruction layer PNG (optional); "
        "when given, paper overlap with the product mask is checked",
    )
    parser.add_argument(
        "--plate",
        help="the background plate image (optional); with "
        "--reconstruction, the final must recomposite exactly from "
        "plate, paper, and product",
    )
    parser.add_argument(
        "--data-dir", help="asset store root for full stored-receipt verification"
    )
    parser.add_argument(
        "--asset",
        help="composite asset handle; verifies v1-v4 under their own policies",
    )
    args = parser.parse_args(argv)
    if args.data_dir or args.asset:
        if not (args.data_dir and args.asset) or any(
            (args.source, args.final, args.mask, args.plate, args.reconstruction)
        ):
            parser.error(
                "use --data-dir and --asset together, without exported-file arguments"
            )
        from .assets import AssetStore
        from .composite import verify_receipt_from_disk

        report = verify_receipt_from_disk(AssetStore(Path(args.data_dir)), args.asset)
        print(json.dumps(report, indent=2, sort_keys=True))
        return 0 if report["ok"] else 1
    if not all((args.source, args.final, args.mask)):
        parser.error("supply SOURCE FINAL MASK, or --data-dir DIR --asset HANDLE")

    report = verify_files(
        args.source,
        args.final,
        args.mask,
        reconstruction_path=args.reconstruction,
        plate_path=args.plate,
    )
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
