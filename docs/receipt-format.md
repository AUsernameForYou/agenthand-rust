# The fidelity receipt format

Every image the `draw` tool stages ships with a fidelity receipt: a
JSON object that pins the files involved by hash and records a pixel
recount measured on the delivered file. The receipt is produced in
`src/composite.rs` (`draw_product`), measured on the composite
decoded back from the asset store, and persisted beside the
composite. The server layer then enriches the persisted copy with
the billed call's attribution (see "Fields current receipts add").

The claim the receipt carries is narrow: the stored product layer
reached the composite unchanged. Hashes pin which files that layer
came from; the recount proves what happened to its pixels.

The worked example in the README ("A real fidelity receipt") is the
frozen proof run's receipt, issued by the reference run of
2026-10-26. It uses the original, unmarked receipt shape: it names
the mask asset without pinning the mask's hashes, and it predates
the `receipt_schema` marker (see "The schema marker"). Sections 1
and 2 define every field of that receipt. Section 3 defines the
fields receipts issued by the current code add.

Hash conventions used throughout:

- `*_sha256` is the SHA-256 of the stored file's bytes.
- `*_pixel_sha256` is the SHA-256 of the decoded image's RGBA pixel
  bytes.
- `*_profile_sha256` is the SHA-256 of the image's ICC profile
  bytes, or `null` when the file carries no profile.

## 1. Assets and hashes

| Field | Meaning |
|---|---|
| `source_asset` | Handle (`asset://` id) of the original seller photo, the root of the lineage chain. |
| `source_sha256` | File hash of the source photo. |
| `source_pixel_sha256` | Pixel hash of the source photo. |
| `foreground_asset` | Handle of the stored cut-out foreground layer: the product pixels the lock protects. |
| `foreground_sha256` | File hash of the foreground layer. |
| `foreground_pixel_sha256` | Pixel hash of the foreground layer. |
| `mask_asset` | Handle of the segmentation mask asset, or `null` when the mask was derived from the foreground's alpha plane. |
| `plate_asset` | Handle of the generated background plate the foreground was composited over, or `null` when no plate was stored. |
| `output_asset` | Handle of the delivered composite. |
| `output_sha256` | File hash of the delivered composite. |
| `output_pixel_sha256` | Pixel hash of the delivered composite. |

## 2. The recount

Counts come from comparing the stored foreground against the
delivered composite, pixel by pixel, classified by the foreground's
alpha channel: fully opaque (alpha 255), edge band (0 < alpha <
255), and transparent (alpha 0).

| Field | Meaning |
|---|---|
| `opaque_pixels` | Foreground pixels with alpha 255. These are the pixels the lock covers. |
| `edge_band_pixels` | Foreground pixels with 0 < alpha < 255. They blend with the plate by design and are excluded from the changed count. |
| `transparent_pixels` | Foreground pixels with alpha 0. |
| `mask_pixels_total` | `opaque_pixels + edge_band_pixels`: the mask's total coverage. |
| `opaque_fraction` | `opaque_pixels / mask_pixels_total`, rounded to 6 decimal places; 0 when the mask total is 0. |
| `edge_band_max_width_px` | Width of the widest part of the edge band, in pixels, measured with a chamfer distance transform over the alpha plane. |
| `changed_opaque_pixels` | Fully opaque positions where the composite's RGBA value differs at all from the stored foreground's. The Fidelity Lock verdict requires exactly 0. |

## 3. Colour profile

| Field | Meaning |
|---|---|
| `color_profile_source_sha256` | Profile hash of the source photo, or `null` (the frozen run's files carry no ICC profile, so both profile hashes are `null` there). |
| `color_profile_output_sha256` | Profile hash of the delivered composite, or `null`. |
| `color_profile_preserved` | `true` when the source and output profile hashes are equal. A profile change is a fidelity failure even when pixel values match, because identical values would display as different colours. |

## 4. Provenance

The `provenance` block is the run record attached when the frozen
run was packaged for replay. The `draw` tool does not emit it; it
documents the run the receipt came from.

| Field | Meaning |
|---|---|
| `provenance.run` | What the run was and when it ran. |
| `provenance.call_ids` | The model-call ids of the run's three tool calls: `identify`, `cut_out`, `draw`. |
| `provenance.settled_costs_usd` | The run's settled ledger costs per tool (`identify`, `cut_out`, `draw`) and their `total`. |
| `provenance.originally_recorded` | The headline numbers as recorded live by the original run: `opaque_pixels_checked` and `changed_opaque_pixels`. |
| `provenance.freeze_note` | Why the receipt's count and profile fields were re-derived from the frozen files at packaging time, and what they reproduce. |

## 5. Fields current receipts add

Receipts issued by the current code carry every field above except
`provenance`, plus the fields below. Two groups: fields `draw`
itself emits, and the attribution the server layer adds to the
persisted copy.

Emitted by `draw`:

| Field | Meaning |
|---|---|
| `receipt_schema` | The schema marker: 2, stamped by `draw` as the receipt's first field. The key the verifier's downgrade guard grades on. |
| `mask_sha256` | File hash of the mask asset, pinning it. `null` when the mask came from the foreground's alpha. |
| `mask_pixel_sha256` | Pixel hash of the mask asset, or `null` on the same condition. |
| `mask_source` | `asset` when the mask is a stored segmentation asset, `foreground_alpha` when derived from the foreground. |
| `opaque_alpha_threshold` | The alpha value that counts as fully opaque: 255. |
| `changed_predicate` | The exact definition of "changed" used for the recount, stated in the receipt itself. |
| `pixel_tolerance` | 0. Any RGBA difference at a covered pixel counts as a change. |
| `count_sample` | `decoded_delivered_file`: counts were measured on the delivered file decoded back from the store. |
| `output_format` | The delivered file's format: `PNG`. |
| `color_space` | A human-readable description of the output's colour profile. |
| `color_profile_foreground_sha256` | Profile hash of the foreground layer, or `null`. |
| `transformations` | Transformations applied to the plate to fit it to the product's dimensions (for example a resize and centre crop). The product layer is never transformed. |
| `product_layer_scaled` | `false`. The product layer is composited at its stored size, always. |
| `plate_source` | `asset` when a stored plate was reused, `local_placeholder` for the deterministic offline dev plate. |
| `plate_sha256` | File hash of the plate, or `null` when no plate was stored. |
| `plate_pixel_sha256` | Pixel hash of the plate, or `null` on the same condition. |
| `plate_kind` | How the plate was produced: `source_edit` for a plate built from the original photo (product region removed before upload, filled from the photo around it (grey on older receipts and under `AGENTHAND_REFERENCE_FILL=grey`), then edited in place by the image model; the product's pixels are never sent), `source_blur` for the code-only plate used when that edit call failed (the masked original, filled from its surroundings, blurred and brightened), `model_generated` for a stored text-only plate from the image model, `code_rendered` for a backdrop plate rendered by code (wallpaper / studio gray stage modes), `local_placeholder` for the deterministic offline dev plate. Absent on older receipts. |
| `backdrop` | For `code_rendered` plates, the backdrop that was rendered: `{"kind": "wallpaper", "motif_diameter_px": 72, "opacity": 0.4, "shadow": false}` or `{"kind": "studio_gray"}`. For `source_edit` / `source_blur` plates: `{"kind", "reference": {"sha256", "size", "fill" (`inpaint_diffusion` or `grey`; absent on older receipts, which were grey), "fill_rgb" (`null` for the inpaint), "fill_iterations", "fill_texture", "dilate_px", "hole_pixels", "product_region"}, "edit": {"model", "path", "size", "output_sha256"} (or `"failed"` with the error), "contact_shadow", "edge_matte_px", "subject": {"opaque_pixels", "coverage", "bbox", "touches_frame_edges", "fills_frame", "thin_fraction", "thin_structure"}}`. The shadow and matte are drawn on plate pixels only, outside the opaque product. `null` for model-generated and placeholder plates. Absent on older receipts. |
| `ground_shadow` | `model_generated` draws made by the server's generated plate path only (docs/GROUND-SHADOW.md): the contact and cast shadow drawn on the fitted plate at the product's ground line, `{"version", "applied", "reason", "light": {"side", "method", "direction_source"}, "shadow_falls", "ground_line", "contacts", "contact", "cast", "grid_cell_px", "product_height_px", "shadow_pixels", "max_darkening", "product_pixels_written": 0, "plate_pixel_sha256"}`. `plate_pixel_sha256` there pins the plate actually composited; `plate_fitted_pixel_sha256` stays the fit of the stored plate, before the shadow. Plate pixels only, never graded. Absent on every other draw and on older receipts. |
| `claim` | The fidelity claim sentence, carried in the receipt itself. |

Optional mask-provenance fields (decisions 68/69), emitted by `draw`
when the mask asset recorded provenance — every `cut_out` since the
SAM working-resolution change writes it as a text chunk inside the
mask PNG. Absent on older receipts and on `foreground_alpha`
receipts; verifiers accept both shapes. When any one is present the
set must be complete and self-consistent (the SAM frame never
exceeds the source frame, and `mask_scale_method` is `none` exactly
when the two frames are equal), or the receipt fails shape
validation:

| Field | Meaning |
|---|---|
| `mask_source_width` | Width of the full-resolution source frame the mask was cut from, in pixels. |
| `mask_source_height` | Height of that source frame, in pixels. |
| `mask_sam_width` | Width of the frame SAM actually saw: the source frame itself, or the working-resolution downscale of it (only the SAM-bound copy is ever downscaled). |
| `mask_sam_height` | Height of the frame SAM actually saw. |
| `mask_scale_method` | How the returned mask was mapped from the SAM frame onto the source frame: `none` when SAM saw the source frame, `bilinear_threshold_128` when its mask was bilinearly upscaled and thresholded at 128. |

Added by the server layer to the persisted receipt:

| Field | Meaning |
|---|---|
| `tool` | The tool that issued the receipt: `draw`. |
| `model` | The model that generated the background plate (muse-image-1.0). |
| `scene_prompt` | The scene prompt the plate was generated from. |
| `cost_usd` | The settled cost of the billed call. |
| `usage` | The provider's usage record for the call. |
| `call_id` | The billed call's id. |

## 6. The schema marker

The Python reference implementation stamps current receipts with
`receipt_schema: 2`. Its verifier grades schema-2 receipts against
the full field set, grades unmarked receipts under the legacy
policy, and refuses a receipt that carries current-schema fields
with a missing or invalid marker, so dropping the marker cannot buy
a weaker grading. The frozen run's receipt is unmarked: the run
predates the marker. This Rust build stamps `receipt_schema: 2` as
the receipt's first field, and its verifier mirrors the downgrade
guard.

The `plate_kind` and `backdrop` fields (section 5) are additive:
they appear on receipts issued since the backdrop stage modes
landed and are absent from older receipts, including the frozen
run's. They are not part of the current-schema field set the
downgrade guard grades on, and the verifier accepts receipts with
or without them; they record how the plate was produced, never a
claim the recount grades.

## 7. What the verifier recomputes

The verifier (`verify_receipt_from_disk` in `src/composite.rs`)
takes a composite and its receipt and re-derives every checkable
claim from the stored files. It returns a report with `ok` (every
check passed), `checks` (one boolean per check below), `problems`,
`recomputed` (the values it derived itself), and the three asset
handles. A standalone file mode (`agenthand verify`) recounts
exported source, staged, and mask files with no store involved.

Identity checks, which bind the receipt to these exact files:

- `source_file_integrity`, `foreground_file_integrity`,
  `composite_file_integrity`: each stored file matches its own
  asset record.
- `source_hash_matches_receipt`, `foreground_hash_matches_receipt`,
  `output_hash_matches_receipt`: the receipt's file and pixel
  hashes equal the asset records' hashes, and `output_asset` names
  the composite under verification. Without this step, one run's
  receipt could verify against another run's composite whenever the
  numbers happened to reproduce.
- `mask_hash_matches_receipt`: the same binding for the mask, run
  when the receipt pins the mask's hashes. When it does not (the
  frozen receipt's shape), the verifier falls back to
  `mask_asset_integrity`, the mask file's integrity against its own
  asset record.
- `foreground_pixel_hash`: the decoded foreground's pixels hash to
  the receipt's `foreground_pixel_sha256`.
- `composite_pixel_hash`: the decoded composite's pixels match its
  asset record.

Recount checks, which re-measure the claim:

- `recount`: the foreground and composite dimensions must match
  exactly, then the pixel recount runs again from disk.
- `recount_changed_opaque_pixels`, `recount_opaque_pixels`,
  `recount_edge_band_pixels`, `recount_transparent_pixels`,
  `recount_mask_pixels_total`, `recount_edge_band_max_width_px`:
  each recomputed count must equal the receipt's value exactly.
- `recount_opaque_fraction`: agreement within 1e-9.

Verdict checks, which grade the recomputed values:

- `zero_changed_pixels`: the receipt and the recount both say
  exactly 0 changed opaque pixels. A receipt that honestly reports
  a real change reproduces perfectly under the recount checks and
  still fails here.
- `positive_coverage`: the receipt and the recount both cover more
  than 0 fully opaque pixels. A receipt covering nothing vouches
  for nothing.

Colour checks:

- `color_profile_matches_receipt`: the profile hashes decoded from
  the source and composite files equal the receipt's recorded
  hashes.
- `color_profile_preserved`: the recomputed source and output
  profiles are identical, and the receipt claims `true`.
