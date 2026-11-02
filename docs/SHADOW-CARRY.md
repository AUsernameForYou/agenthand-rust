# Shadow carry-over: the product's own cast shadow

Defect (Daniel, 2026-11-01, IMG_1443 run): in the staged bike photo only the elliptical contact shadow remains. The bike's long soft cast shadow is gone: it lies outside the product mask, the `source_edit` plate paints over it, and the composite never put it back, so the product reads pasted.

Fix: automatic, two layers. The prompt clause is best effort; the composite carry-over is the guarantee.

## 1. Prompt clause (best effort)
- `meta::SHADOW_CLAUSE`, appended by `plate_edit_prompt_locked` right after the angle lock, on every `source_edit` plate prompt (object and surface wording, with or without scene text, stated angle or not):
  - "Keep the object's cast shadow on the ground as in the original photo, same direction, length and softness."
- The legacy `generated` mode has no reference photo and gets no clause.
- Identify instructions and identify output schema are untouched.

## 2. Deterministic carry-over (composite)
`composite::carry_source_shadow(source, layer, plate)` reads a shadow matte from the ORIGINAL photo against the finished plate and multiplies it onto the plate before the product goes down.

1. Block means on a 4 px grid (`SHADOW_GRID`), photo and plate. The product footprint dilated by 8 px per 1024 px of the long edge (min 2 px; the reference hole's own dilation at the edit's working size) is not measured: inside it the plate is the model's fill and the photo carries the cut's fringe.
2. Shadow-like blocks:
   - plain luma ratio `r = L_orig / max(L_plate, 1)` at most 0.92;
   - chromaticity (each of r, g, b over r+g+b) moved by at most 0.08, so colour edits are not shadow;
   - plate block luma at least 16 (near-black ratios are noise).
   From the region's floor up they are candidates; below it, deep umbra. A 3x3 opening drops one-block seams (plate/photo misregistration lines).
3. Regions (v2, coordinator's ruling for Daniel, 2026-11-01):
   - attached: 8-connected components that touch the ring just outside the dilated mask on ANY side; floor 0.25 (a shadow thrown onto a sunlit wall behind the product is deeper than a ground shadow);
   - ground band only: components that touch only the band under the product's bottom edge (15% of the product height, min 2 blocks, product width), not the mask; floor 0.35;
   - components under 0.05% of the image are specks;
   - a shadow not connected to the mask or the band (e.g. thrown onto a fence, a gap of lit fence away from the silhouette) is never a candidate.
4. Per region, else it is listed in `rejected` with the reason:
   - coherence: at least 75% of the region's pixels within +-0.15 of its median ratio.
   Then, over the carried regions together (else `contact_only`):
   - at least 0.2% of the image;
   - at most 35% of the non-product area.
5. Rims of deeper shadows, at each region's own floor: region blocks within 2 blocks (8 px) of a deep zone are not carried. Deep-umbra blocks are shadow-like (same chroma and plate-luma tests) with ratio below the floor; a deep zone is a block whose 9x9 window holds at least 12 of them (15%): a solid core, or a shadow flickering across the floor (a one-block seam puts 9 in a window and does not count). A whole shadow body (connected shadow-like blocks, in-band or deep, opened 3x3) that is more than 25% deep-umbra blocks is not carried at all. Carrying the in-band rim alone would leave a half-dark blob beside the uncarried core. The gates in 4 still judge the whole region. If nothing of a region is left, it is rejected: "the region is the rim of a shadow deeper than the <floor> ratio floor (N% of its body below the floor)".
6. Matte at full resolution:
   - local ratio = median of the kept-block ratios in a 5x5-block window (smooths texture; the penumbra rim does not drag it lighter);
   - carried two blocks beyond the region and through the exclusion ring toward the product (breadth-first from the region, nearest value), and read bilinearly between block centres per pixel (no 4 px block steps);
   - a pixel is shadow when its 3x3-mean ratio is at most halfway between the local ratio and 1;
   - region weight: the region's blocks closed by two blocks (deep-umbra and noise holes up to 8 px filled), plus its exclusion ring and the footprint, at pixel level, grown by half a block and box-blurred by half a block: 1 across the region, 0 one block beyond it. Where the region stops inside a broader shadow of the photo, the carried shadow fades out with rounded corners instead of stepping along the 4 px grid.
7. Feather: the source shadow's own edge softness, measured like `estimate_source_edge_width` does for the product edge:
   - luma profiles across the matte boundary in the ORIGINAL, 6 px inside to 7 px outside, shadow side darker by at least 16 levels, range at most 1.5x the step (texture rejected), profiles touching the product footprint skipped;
   - width = step / steepest single step; median; rounded; capped at 12 px; under 64 profiles: 2 px;
   - blur: normalized box (product pixels never averaged in) of width `2*floor((feather-1)/2)+1`, never wider than the measured ramp.
8. Apply: plate RGB *= (1 - blurred density), density = 1 - local ratio inside the matte; only where the layer alpha is below 255.

Order in `draw_product_with_plate_kind` (source plates):
- fitted plate (the finished plate already carries the contact ellipse from `staging::finish_plate`) -> carried shadow -> edge fix (rim rebuild + contact band + feather, `apply_edge_fix`) -> product.
- The ratio is read against the finished plate, ellipse included, so the contact zone is not darkened twice.

## Record
`shadow` in the draw result (beside `angle_lock`) and, additively, in the receipt of source-plate draws (the `edge_fix` path; never graded):

```json
{"mode": "carried" | "contact_only", "reason": null | "...", "shadow_pixels": 0, "mean_ratio": 0.0 | null, "feather_px": 0, "coherence": 0.0 | null,
 "regions": [{"side": "behind" | "left" | "right" | "above" | "below", "pixels": 0, "mean_ratio": 0.0, "coherence": 0.0, "floor": 0.25}],
 "rejected": [{"side": "...", "pixels": 0, "mean_ratio": 0.0, "coherence": 0.0, "floor": 0.25, "reason": "..."}]}
```

- `shadow_pixels`: plate pixels the carried shadow changed.
- `mean_ratio`, `coherence`: pixel-weighted over the carried regions; with nothing carried, the largest rejected region's.
- `regions`: the surfaces carried. `side` is the region's centroid against the product's bbox: `behind` inside it, else the dominant direction. Ground-band-only regions are `below`.
- `rejected`: candidate regions refused, with the guard and its numbers.
- `contact_only` reasons:
  - "generated plate": `model_generated`, `local_placeholder`, `code_rendered` (no record in their receipts; the draw result gets it via `composite::shadow_record_of`);
  - "disabled by AGENTHAND_SHADOW";
  - "no coherent shadow region adjacent to the product";
  - "shadow region too small / too large: ...";
  - "incoherent shadow ratio: ..." and "the region is the rim of ...": the largest rejected region's reason, when no region is carried;
  - "source, plate and product layer frames differ", "the product layer has no opaque pixel", "the carried shadow changed no plate pixel".

## Guardrails
- No product pixel (alpha 255) is ever written. The carry-over writes plate pixels only, outside the opaque mask; `changed_opaque_pixels` and every receipt count are unchanged.
- Receipt semantics unchanged: one additive `shadow` key on source-plate receipts only. Generated, placeholder and code-rendered receipts are byte-identical to before.
- Kill switch: `AGENTHAND_SHADOW=off` (also `0`, `false`, `no`). Record: `contact_only`, "disabled by AGENTHAND_SHADOW"; composite identical to the pre-carry-over output.
- Deterministic: fixed grid, sorted medians, integer luma; tested by running twice.
- v1 gates (ratio band, chroma, coherence, size) were fixed before the bike was run. The rim guard (step 5) and the region weight (step 6) were added after the first bike run showed half-dark, grid-edged blobs on the wall; both only remove or soften carry. v2 (any-side attachment, 0.25 floor for attached regions, per-region coherence) is the coordinator's ruling; no number was tuned to make a bike region pass.

## Known limits (disclosed)
- Plain ratio: a model edit that darkens or brightens the plate uniformly (the "tidy, bright" default styling) moves `r` too. A uniform brightening of about 8% puts lit ground at r ~0.92, the band's top; a stronger one reads as shadow wherever it is connected to the seeds. The coherence and 35% gates catch the large cases, not all. Follow-up if it shows: divide `r` by the lit-ground baseline ratio.
- Detached shadows: a cast shadow that lands away from the silhouette (on a fence behind, with lit fence between) never touches the mask and is not carried.
- Umbras below 0.25 (attached) or 0.35 (ground band) are outside the band; their in-band rims are not carried either (step 5).

## Bike fixture (IMG_1443 run-1443-fixed)
v2 result, `carried`:
- `shadow_pixels` 919,162
- `mean_ratio` 0.588
- `coherence` 0.860
- `feather_px` 4
- product pixels: 5,660,280, bit-identical to the shipped `staged.png`; 0 changed with the edge fix on top.

Carried regions:
- wall course behind the frame and under the seat: 207,760 / 47,248 / 18,528 / 12,256 px, ratios 0.60 / 0.42 / 0.38 / 0.37;
- the rear-wheel arc on the wall: 81,200 px, 0.36;
- the ground between the wheels: 290,080 px, 0.68;
- the ground left of the rear wheel: 14,320 px, 0.81.

Not carried:
- the seat shadow on the fence, rejected by the coherence guard: 158,800 px, 65.4% within +-0.15 of median 0.366 (minimum 75%), mean 0.451;
- the handlebar and stem shadows on the fence: detached, so never candidates. Handlebars: 103,408 px, median 0.418, at least ~480 px from the bike. Stem: 184,464 px, median 0.355, at least ~88 px away. Neither touches the dilated mask.

v1 (lower half + ground band, 0.35 floor) carried 379,847 px, the ground between the wheels only.

Produced by the ignored test:

```text
AGENTHAND_SHADOW_ARTIFACTS=<run dir with source.png foreground.png plate.png staged.png> \
AGENTHAND_SHADOW_OUT=<out dir> \
cargo test --release --test composite_shadow -- --ignored --nocapture
```

The test writes:
- `bike_shadow_before_after<TAG>.jpg`: original | shipped staged | staged with the carried shadow, at 1/3 scale. The third panel is plate -> carried shadow -> product with no edge fix, so the shadow is the only difference. `AGENTHAND_SHADOW_TAG` sets the suffix (`_v2`).
- With `AGENTHAND_SHADOW_ZOOM=x,y,w,h`, a second row at 2x the overview. The v2 file uses `1800,300,2400,1300`, the wall behind the handlebars.
- `bike_shadow_record<TAG>.json`.
- With `AGENTHAND_SHADOW_CROP=x,y,w,h`, `bike_shadow_crop<TAG>.jpg`: the same three panels at full resolution.

It also asserts that the product pixels are bit-identical to the shipped `staged.png`.

## Tests (`tests/composite_shadow.rs`)
- `known_soft_cast_shadow_is_carried_and_the_product_is_untouched`: synthetic 240x160, product box, a cast shadow (ratio 0.5, 4 px ramps) right of it in its lower half. Carried; mean |luma error| over the shadow <= 4; feather 2-5 px; record keys exact; far pixels unchanged; product bit-identical (also through the edge fix); deterministic.
- `the_rim_of_a_deeper_shadow_is_not_carried`: the same shadow made deeper than the attached floor (ratio 0.15) at its far end. A small deep core (x >= 196): the 0.5 part next to the product is carried, the core is left exactly as the plate. A large one (x >= 170, a third of the body): `contact_only`, "the region is the rim of ...", plate unchanged.
- `wall_shadow_behind_the_product_is_carried_on_any_side`: a ratio-0.30 shadow on the wall, attached to the box's upper left. Carried as region `left`, floor 0.25, mean |luma error| <= 4. A detached patch far from the box is untouched.
- `ground_band_only_regions_keep_the_035_floor`: a ground strip under a tall box, 8 px clear of the mask. Ratio 0.30: not carried. Ratio 0.45: carried, `below`, floor 0.35.
- `incoherent_pairs_stay_contact_only`: tile-random plate; scattered tile ratios (0.36-0.90) -> "incoherent ...". Plate unchanged.
- `kill_switch_restores_the_previous_composite_and_the_record_says_so`: through the store; `AGENTHAND_SHADOW=off` gives the exact pre-carry-over composite (`composite_images_with_edge_fix` on the stored plate); on gives `carried`, 0 changed, receipt verifies.
- `generated_plate_kinds_are_contact_only`: no kind, `model_generated`, `local_placeholder`, `code_rendered` -> "generated plate"; their receipts carry no record and their composites are unchanged.
- `real_fixture_bike_shadow` (ignored, opt-in): the bike run above.
- Prompt: `tests/angle_lock.rs` and the `meta.rs` unit tests now expect `... {angle lock} {SHADOW_CLAUSE}` as the prompt suffix.
