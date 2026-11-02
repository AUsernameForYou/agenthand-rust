# Edge fix: the cut-out rim (compositor / matte side)

Defect (Daniel, 2026-11-01): on source-plate draws "around the edges you can really tell it was cut". A thin rim, about 1 px, traces the silhouette at 3x zoom.

Constraint: "do not blur too much". Feather is capped at about 1 px and is never softer than the source photo's own edge.

## What changed
- `src/segmentation.rs` (matte side)
  - `EdgeField`: exact chessboard distance maps of the layer's alpha over the footprint bbox plus 16 px:
    - distance to the footprint (alpha > 0)
    - distance to the product set (alpha 255)
    - depth inside the product set
  - Local colour estimates on the same field:
    - `product_colour`: product pixels 3-5 px deep, then 2+ deep, then any product pixel (thin parts).
    - `background_colour`: the original background, from SOURCE pixels 2-4 px outside the footprint. Radius 6 window, then radius 12.
  - `unblend_edge_pixel` and `decontaminate_edge_band`:
    - Every soft pixel (0 < alpha < 255) keeps its alpha.
    - Its colour becomes `F = (observed - (1-a) B) / a`, using the local original background `B`. Integer math, clamped.
    - Low-coverage pixels lean on the local product colour: weight `min(255, 2a)` on the un-blend.
    - Product (alpha 255) and transparent pixels are copied unchanged.
- `src/composite.rs` (compositor)
  - `apply_edge_fix(source, layer, plate, thin)`, in this order:
    1. Decontaminates the layer's soft edge band.
    2. Rebuilds the plate rim. Each plate pixel 1..band px from the product set takes the mean plate colour `band+1..band+3` px out (shares 256/256/160 of 256). This wipes:
       - any image-model rim
       - the old `apply_edge_matte` ring, which spread the boundary's contaminated colour 2 px onto the plate
    3. Adds the contact band: the contact shadow's colour (58, 50, 40) at alpha 30/18/8 per ring, alpha-composited with Pillow math. Per channel it never comes out brighter than the plate.
    4. Adds the feather: one outward ring (alpha-0 pixels touching the product set) in local PRODUCT colour.
       - Alpha is `feather * min(k,3)/3`, where k = product pixels in the 3x3. Straight edges get the full value; staircase corners touched only diagonally get a third, so the steps round off instead of getting an outline.
  - `estimate_source_edge_width`:
    - Takes luma profiles across every product boundary pixel, from 2 px inside to 3 px outside.
    - A profile counts only if its step is at least 32 levels and its range is at most 1.5x the step (texture is rejected).
    - Width = step / steepest single step; the median over all profiles is the result.
    - With fewer than 64 profiles the edge is treated as hard.
  - `feather_alpha_for_width`:
    - `a = min(128, 1 - 1/w)`. The composite ramp `1/(1-a)` then equals the source ramp and never exceeds it. 128 (one ring at 50%) is the ~1 px cap.
    - A ramp of 1.125 px or less counts as pixel-hard: no feather.
  - `composite_images_with_edge_fix`: fits the plate exactly as before, applies the fix, composites, and measures against the STORED layer.
  - `draw_product_with_plate_kind`:
    - Runs the fix only for plate kinds `source_edit` and `source_blur`.
    - The source photo comes from the layer's lineage. The thin flag comes from `backdrop.subject.thin_structure`, or `staging::analyze_subject` when that is absent.
    - The receipt gains an additive `edge_fix` record.
  - `composite_pixel`: one pixel of `alpha_composite`, with identical math (tested against it).
- Kill switch: `AGENTHAND_EDGE_FIX=off` (also `0`, `false`, `no`). The receipt then says `edge_fix.applied: false` with the reason, and the composite is the old one exactly.
- Unchanged:
  - `composite_images`, `alpha_composite`, `measure` and the verifier
  - the default `draw_product`, so parity is untouched
  - legacy `model_generated`, `local_placeholder` and `code_rendered` draws: byte-identical, no record
  - `apply_edge_matte` and `finish_plate`. On source plates the fix's rim rebuild supersedes the matte ring.

## Guardrails
- No product pixel (alpha 255) is ever written. The fix works on copies; the stored layer is untouched.
- Receipt counts are measured against the stored layer, so the pixel set, `opaque_pixels`, `edge_band_pixels` and `changed_opaque_pixels` are the same as without the fix.
- Feather pixels sit outside the footprint. They count as `transparent_pixels`, as plate pixels always have.
- No erosion. The feather is outward only, one ring.
- Thin structures (`staging::analyze_subject().thin`): the contact band is 2 px (alpha 18/8, rebuild 256/160), so pickets and spokes are not haloed. The feather stays one ring.
- Deterministic: integer math, integer median, and the rebuild reads the untouched plate, so visiting order cannot matter. Tested by running it twice on the bike.

## Parameters
| Name | Value | Meaning |
|---|---|---|
| `BACKGROUND_RING` | 2..4 px | original-background sample ring outside the footprint (ring 1 is itself soft edge in the photo) |
| `PRODUCT_DEPTH` | 3..5 px | product-colour sample depth (boundary rings carry the blend and SAM's misfit) |
| `EDGE_SAMPLE_RADIUS` / `_WIDE` | 6 / 12 px | local estimate windows |
| `FEATHER_MAX_ALPHA` | 128 | 1 ring at 50%, about 1 px |
| `EDGE_WIDTH_MIN_CONTRAST` | 32 luma | profile qualifies |
| `EDGE_WIDTH_MIN_SAMPLES` | 64 | below this: hard, no feather |
| `EDGE_WIDTH_HARD_FX` | 288 (1.125 px) | dead band for quantisation noise |
| `CONTACT_BAND_RGB` | 58, 50, 40 | contact shadow colour |
| `CONTACT_BAND_ALPHA` | 30, 18, 8 (thin: 18, 8) | per ring, ring 1 hugs the product |
| `RIM_REBUILD` | 256, 256, 160 (thin: 256, 160) | share of the clean plate colour per ring |

## Receipt record (additive, never graded)
`edge_fix`: `version`, `applied`, `stage: composite`, `product_pixels_written: 0`. Then:
- `edge_band {pixels, decontaminated, method, background_ring_px}`
- `feather {rings, alpha, max_alpha, pixels, colour}`
- `source_edge {ramp_width_px, samples, min_contrast, rule}`
- `contact_band {width_px, rgb, alpha, rim_rebuild_share_256, rebuilt_from_ring_px, pixels, rim_rebuilt_pixels}`
- `thin_structure`

The verifier ignores the record. Plates are pinned by hash and never recomputed; product pixels are recounted as before.

## Acceptance
- `tests/composite_edge.rs`, 11 tests:
  - `dark_product_on_dark_plate_rim_drops_in_the_edge_band` (the spec's acceptance case):
    - Dark product (40, 42, 46) photographed on a bright counter (232, 226, 214), with a soft 2 px band, composited on a dark plate.
    - Edge-band rim luma is ~79 before and must be over 70; after it must be under 50.
    - Interior pixels identical, opaque and edge-band counts unchanged, 0 changed, stored layer unmodified.
  - `binary_cut_plate_rim_is_rebuilt_and_boundary_feathered`: a bright 2 px plate rim (luma over 170) drops under 50. The feather is present in product colour, never counter light. The far plate is untouched; the product is identical.
  - `contaminated_product_pixels_stay_as_photographed`: a mask grown past the product keeps its counter-tinted boundary exactly, because those pixels are receipt-locked.
  - `feather_is_never_softer_than_the_source_edge`:
    - hard source: feather 0
    - 2 px source ramp: alpha 90-128, with composite ramp <= source ramp
    - 4 px ramp: capped at 128
    - no profiles: treated as hard
  - `thin_structures_keep_a_narrow_band_and_are_not_eroded`, `edge_fix_is_deterministic_and_leaves_the_far_plate_alone`, `old_matte_ring_is_rebuilt_from_the_clean_plate`, `composite_pixel_is_alpha_composite_exactly`, `unblend_recovers_product_colour`.
  - `receipt_records_the_edge_fix_and_still_verifies`:
    - both source kinds: record present, 0 changed, `verify_receipt_from_disk` ok
    - legacy and `code_rendered`: no record, byte-identical composite
  - `real_artifacts_before_after`: opt-in, see below.
- Real run on `film/v8/film-assets/P04-P09/run-1443-fixed` (bike, 5712x4284, binary SAM cut):
  - The plain recomposite reproduces the shipped `staged.png` byte for byte.
  - With the fix: 5,660,280 product px; every mask-on pixel equals the source; 0 changed; deterministic.
  - Source edge ramp 2.04 px from 9,208 profiles, so feather alpha 128 (at the cap). 34,064 feather px; 102,097 rim px rebuilt.
  - Mean luma 2 px out: 129.3 before (the old matte ring), 139.7 after; the clean plate 4-6 px out is 146.4.
  - Timing, 2-core container, release: 3.4 s. That span also includes the plain recomposite and decoding `staged.png`.
  - Output: `edge-out/bike_edge_before_after.jpg` (source | before | after, 3x crops), `bike_edge_summary.json`, `bike_edge_after.png`.

```
AGENTHAND_EDGE_ARTIFACTS=<dir: source.png mask.png foreground.png plate.png [staged.png]> \
AGENTHAND_EDGE_OUT=<out> [AGENTHAND_EDGE_WRITE_FULL=1] [AGENTHAND_EDGE_POINTS="x,y;x,y"] \
cargo test --release --test composite_edge real_artifacts -- --nocapture
```

## Failure modes and limits (stated up front)
- **Binary SAM cuts have no soft band.** Their boundary product pixels (alpha 255) carry the original background's blend as photographed, and the receipt locks them, so the fix cannot recolour them. On those cuts the fix does three things:
  - stops spreading that colour onto the plate (the old matte did)
  - rebuilds the plate rim
  - adds the product-coloured feather

  A dark-on-dark charger cut with a binary mask still shows its one boundary ring as photographed. Removing it would need the receipt to exclude that ring (the product set shrinks), which this fix deliberately does not do.
- **SAM's raster staircase (4-5 px steps on 24 MP) is inside the product set.** The feather rounds the corners slightly but cannot remove the steps. A finer mask is the fix for that.
- **The contact band is a darkening.** On a bright, textured plate it reads as a faint contact line, about 5% darker 2 px out on the bike. Its alpha is in `CONTACT_BAND_ALPHA` if it needs toning.
- **Python reference:** not ported. A port needs:
  - `EdgeField` (chamfer 8-neighbour, cap 32)
  - the integer un-blend: `(obs*255 - (255-a)*B + a/2) div a`, then `(f*w + p*(255-w) + 127)//255`
  - the width estimator (integer median)
  - the band and feather, using Pillow `alpha_composite` per pixel
  - the min-clamped contact band

  Plates are never recomputed by a verifier, so receipts are unaffected.
- **`finish_plate` still draws the old matte into the stored plate** (`staging.rs`, not this change). On source plates the fix's rebuild replaces those rings in the composite. Dropping the matte there is a follow-up for the staging owner.
