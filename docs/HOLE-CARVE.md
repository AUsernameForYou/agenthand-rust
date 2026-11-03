# Hole carving

## What it fixes
SAM returns open-frame products as solid regions. A bicycle wheel comes back as a disc, so whatever is seen between the spokes (pavers, the wall, a cast shadow) is counted as product. It ships untouched into every staged image. The new plate then shows the OLD scene through the wheels, and the composite reads as a pasted cut-out.

Ruling (Daniel): the product is pixel-exact and the background is variable. The background seen through an open frame therefore belongs to the plate, not to the product.

## Where it runs
`segmentation::carve_holes(source, full_mask, concept)` runs in `cut_out` (src/server.rs). It takes the full-frame mask right after `map_mask_to_frame`, before anything is cut, stored or reviewed. Everything downstream counts the carved mask:
- the mask asset and the foreground;
- the overlay and `mask_review`;
- `draw`, the fidelity receipt and verify.

The concept passed is the accepted phrase plus the requested concept.

Right after the carve, `cut_out` checks the enclosed-remainder gate (see below). When an open-frame carve left more than 10% of the frame interior as old background, `cut_out` returns a structured refusal (`HOLE_CARVE_INCOMPLETE`) before it stores anything.

Guarantees:
- It only ever lowers mask pixels that were at 255. It never raises a pixel, never touches a pixel below 255, and never recolours anything.
- The receipt counts what the final mask calls product (alpha 255), so the product pixel count shrinks and stays honest.
- The record's `product_pixels_written` is always 0.
- It is deterministic: the same input gives the same mask and the same record.

Interplay with the thin-structure guard (docs/LIGHT-AND-THIN.md): carved wheel windows are enclosed mask holes that `draw` can find. Where they are big enough at its work-grid resolution, `soften_thin_holes` can calm busy plate texture seen through the spokes. Before carving, the solid disc had no holes for it to act on.

## Algorithm
All distances are chessboard distances in source pixels. All work happens inside the mask's bbox, grown by the background ring. "diag" is the mask bbox diagonal.

1. **Candidates.** Mask pixels deeper than the outer band from every non-mask pixel, where band = max(12 px, 0.6% of diag). The outer silhouette is never touched.
2. **Colour evidence.**
   - Background samples come from a ring 6 to 30 px outside the mask.
   - Product samples come from the outer band (deeper than 6 px, at most band deep). This is thick product that is not the enclosed disc itself.
   - Both are binned (RGB, 32 levels per channel) and box-smoothed.
   - They are modelled per tile: tile side max(64 px, 8% of diag), at most 12 tiles a side. Each tile pools its 3x3 tile neighbourhood, with the global model mixed in as a 500-sample prior. Models are blended bilinearly between tile centres.
   - A candidate is **background-like** when two things hold:
     - P(colour | background) > e x P(colour | product);
     - P(colour | background) is at least 0.25 of a uniform bin, so it is a real background colour.
3. **Thin structures.** Spokes, chain, cables and the hub's edges are found with a black-hat / top-hat on luma using a 13x13 square, so lines up to 12 px wide count. The contrast floor is 28. Components must be at least max(24 px, 1% of diag) long. These pixels are **protected** and are never carved as part of a window.
4. **Windows.** Background-like cells (small pits filled) are judged by their border:
   - A **window** (a sector between spokes) is framed by narrow product structure.
   - Background-coloured **product** (a grey rim in shade, a brass screw on a reflector) is framed by the rest of its part.
   - A cell is carved when at least half its border is narrow, AND either of these holds:
     - it is deep: it holds a pixel at least max(8 px, 0.4% of diag) from anything that is not background-like;
     - it is shallow but strictly framed: the product-like set is closed with a 5x5 square first, so a part in shade stays one solid part.
   - Cells linked through narrow structure form a region. A region is carved when its background-like area is at least 0.2% of its mask component.
   - **Appendages.** Accepted cells are cut at necks narrower than 7 px, and each piece is judged again. A piece that fails returns to the product (`appendage_pixels_kept`).
   - **Open-frame gate.** A component is carved only when both of these hold:
     - its carved background is at least 2% of it;
     - it is an open frame: enclosed candidate area (carved regions plus thin structures) is at least 10% of it, OR the concept names one of `bicycle bike wheel chair rack stool ladder basket`. Matching is a whole-word prefix: "bikes" and "wheelchair" match, "track" does not match "rack".
4e. **Completion** (`complete_windows`). Step 4 accepts only colour-proven background. Background in shade (pavers under the bike's own shadow) reads like the tyre, so it stays product and shows through the wheel on every plate. Once step 4 has accepted windows, the completion pass judges the rest of the frame interior by its structure:
   - **Second colour model.** Background samples are the ring plus the accepted windows. Product samples are the band plus the candidates step 2 proves product (P(colour | product) >= e x P(colour | background)). Inside the frame, step 2 had only the global prior; this model is local there. A candidate it proves product, at the same e margin, is kept **structure**: the rim, frame tubes, the fork, the hub, the chainguard. Structure in pieces shorter than the thin step's extent floor, counted together with the thin lines they touch, is grain in the background and stays open. A spoke nipple, a chain plate or a nut joins its line work and stays structure.
   - **Depth floor.** Open candidates lie at least as deep inside the silhouette as the accepted windows reach: the 1st percentile of their Euclidean depth (238 px on IMG_1443). The tyre and the rim lie between the windows and the outline, so they stay. The depth is Euclidean because a chessboard depth is about 30% shallower along diagonals and would expose the tyre's inner edge at other angles.
   - **Cells.** Connected open candidates (not thin, not structure, not accepted) form cells. A cell is a **window** when all of these hold:
     - it does not touch the outer band, and it is at least the cell floor;
     - it holds a core of max(4 px, 0.15% of diag) px;
     - at least half of its border is look-through: accepted windows, or thin and structure pixels outside a part twice the thin square wide (spokes, not tread on a tyre);
     - it links to an accepted window across gaps of at most 14 px (a spoke or another thin line in between). Linking is transitive: the accepted windows and the window cells are each grown by 7 px and labelled together;
     - **pocket guard:** less than half of it lies in a pocket of solid kept parts. Solid kept parts are kept structure, dense line work (thin pixels with at least 60% thin pixels in the 25x25 box around them: a chain, reflector prisms) and the outer band, opened with the thin square so spokes drop out. Pockets are what a closing with a square of 0.42% of the bbox diagonal (24 px on IMG_1443) adds to them. The cog between the rear chain and the dropout is such a pocket and stays.
   - **Release.** Windows are released whole, colour-proven or not. They join the carve after the line release and the specks below, which judge only the colour-proven windows. So a background-coloured spoke, a spoke nipple or a chain plate next to a completed window keeps the status the colour-only carve gave it.
4d'. **Faint lines** (computed with the thin structures, used by 4e and 4f). A metal spoke in shadow over shaded paving sits under the thin contrast floor, and a light spoke over paving can fail the colour test; the completion then released it with its cell and the spoke read torn. Faint lines are raw line work at the thin floor whatever its colour, plus bright line work (top-hat at least 14) over a dark local background (luma opening at most 90), where at most 45% of the 25x25 box around a pixel is line work (both polarities counted, so rough shade and texture fail), in components at least as long as a thin line. The completion and the strip step do not open them; the colour-proven carve is unchanged. A dark band in shade is not bright, so paver joints still release.
4f. **Strips** (`release_strips`, after the completed windows join). Colour cannot tell a paver joint in shade from the tyre, so the completion keeps dark joint bands across the windows. Kept interior pixels (deeper than the depth floor, in a carved component, not thin line work, not pale) form the strip set when they are narrower than a square of 0.37% of the bbox diagonal (43 px on IMG_1443) and their core survives an opening of 0.09% of diag (11 px; a spoke in shade is narrower and stays).
   - A strip whose border is at least 60% released windows is released. It is part of the completion, so `AGENTHAND_HOLE_COMPLETE=off` turns it off.
   - **Spoke bridges:** a released strip pixel goes back to the product when a straight line through it (16 directions) leaves the strip within three times the width square on both sides and then runs through at least 12 kept pixels on both sides: a spoke crossing the joint.
   - A strip bordered at least 30% by released windows that stays is **strip residue**; the gate adds it to the remainder.
5. **Finish.**
   - **Pale-part guard** (`restore_pale_parts`, after the specks). Pale pixels (luma at least 120, channel spread at most 50) are opened with the thin square. They are labelled twice: with pieces split by a spoke joined (closing of 0.14% of diag, 8 px here) and with pieces split by a bolt joined (0.42% of diag, 24 px). A part is product when the carve kept at least 60% of its pale pixels and it fills at least 70% of its convex hull. Every carved candidate inside the hull goes back to the product, plus pale pixels within the thin radius of the hull (a sliver beyond a spoke at the end of the part). This keeps a reflector whole: its frayed pale edge (which the colour test reads as sunlit wall) and the brass bolt on it. Sunlit paving is warmer than the spread limit and is carved; a pale rim arc is not compact. `pale_part_pixels_kept` counts these pixels.
   - **Released.** A line-contrast pixel of BACKGROUND colour inside a window is carved, up to 6 rounds. Examples are wall grain or a spoke's cast shadow next to a spoke. The rule: at least 50% of the 25x25 box around it is carved window, and it is not part of a solid kept part. Line pixels of product colour are never released.
   - **Specks.** An uncarved patch that is not thin structure and is fully surrounded by carved or thin pixels is carved too. The size cap is max(64 px, 0.02% of the component).
   - **Gaps.** 1 px gaps along kept structures are closed back to product.
   - **Soft inner edge.** Carved pixels touching kept product get alpha from a local two-colour (product vs background) estimate. Alpha is capped at 128 and anything below 26 is dropped to 0. The edge fix (docs/EDGE-FIX.md) then treats the new inner edges like the outer one. These pixels are below 255, so they are not counted as product.

## Thresholds
All thresholds are `pub const` in src/segmentation.rs.

| Constant | Value | Meaning |
|---|---|---|
| `CARVE_BG_RING` | (6, 30) px | background sample ring outside the mask |
| `CARVE_PRODUCT_MIN_DEPTH` | 6 px | product samples start deeper than this |
| `CARVE_BAND_MIN_PX` / `CARVE_BAND_DIAG_FRAC` | 12 px / 0.006 | outer band, never carved |
| `CARVE_BIN_BITS` | 5 | colour bins per channel (32) |
| `CARVE_TILE_MIN_PX` / `_DIAG_FRAC` / `_MAX_GRID` | 64 / 0.08 / 12 | colour-model tiles |
| `CARVE_TILE_PRIOR` | 500 | global pseudo-samples per tile |
| `CARVE_BG_RATIO` | e | background vs product likelihood margin |
| `CARVE_BG_MIN_DENSITY` | 0.25 | minimum background density (share of uniform) |
| `CARVE_THIN_RADIUS` / `_CONTRAST` | 6 (13x13) / 28 | thin-structure detector |
| `CARVE_THIN_MIN_EXTENT_PX` / `_DIAG_FRAC` | 24 / 0.01 | minimum thin component length |
| `CARVE_CELL_FRAME_FRAC` | 0.5 | share of a cell's border that must be narrow |
| `CARVE_SOLID_CLOSE_RADIUS` | 2 (5x5) | closing for strict framing |
| `CARVE_CORE_DEPTH_MIN_PX` / `_DIAG_FRAC` | 8 / 0.004 | deep window |
| `CARVE_CELL_MIN_PX` / `_FRAC` | 64 / 0.0001 | cell floor |
| `CARVE_NECK_RADIUS` | 3 (7 px) | appendage neck cut |
| `CARVE_REGION_MIN_FRAC` | 0.002 | region floor (share of component) |
| `CARVE_MIN_ENCLOSED_FRAC` | 0.02 | component carved only above this background share |
| `CARVE_OPEN_FRAME_FRAC` | 0.10 | open frame without a keyword |
| `CARVE_RELEASE_RADIUS` / `_FRAC` / `_ROUNDS` | 12 / 0.5 / 6 | background line pixels released |
| `CARVE_SPECK_MIN_PX` / `_FRAC` | 64 / 0.0002 | enclosed specks |
| `CARVE_SOFT_MAX_ALPHA` / `_MIN_ALPHA` | 128 / 26 | soft inner edge |
| `CARVE_COMPLETE_FLOOR_FRAC` | 0.01 | completion depth floor: 1st percentile of the accepted windows' depth |
| `CARVE_COMPLETE_CORE_MIN_PX` / `_DIAG_FRAC` | 4 / 0.0015 | completed window core |
| `CARVE_COMPLETE_LINK_RADIUS` | 7 | link across gaps of at most 14 px |
| `CARVE_GATE_MAX_REMAINDER_FRAC` | 0.10 | gate: largest share of the enclosed window area left unreleased |
| `CARVE_POCKET_MIN_PX` / `_DIAG_FRAC` / `CARVE_POCKET_FRAC` | 2 / 0.0042 / 0.5 | pocket guard: closing radius, share of the cell in a pocket |
| `CARVE_DENSE_THIN_FRAC` | 0.6 | dense line work counts as a solid part |
| `CARVE_STRIP_WIDE_MIN_PX` / `_DIAG_FRAC` | 8 / 0.0037 | strips are narrower than this square |
| `CARVE_STRIP_CORE_MIN_PX` / `_DIAG_FRAC` | 2 / 0.0009 | strip core opening |
| `CARVE_STRIP_RELEASE_FRAC` / `_RESIDUE_FRAC` | 0.6 / 0.3 | released-window share of a strip's border: release / count as residue |
| `CARVE_STRIP_BRIDGE_CONTINUE_PX` | 12 | spoke bridge: kept run beyond the strip on both sides |
| `CARVE_FAINT_CONTRAST` / `_MAX_BG_LUMA` / `_MAX_DENSITY` | 14 / 90 / 0.45 | faint lines: bright line contrast over a dark background, line density limit |
| `CARVE_PALE_MIN_LUMA` / `_MAX_CHROMA` | 120 / 50 | pale pixels |
| `CARVE_PALE_KEPT_FRAC` / `_SOLIDITY` | 0.6 / 0.7 | pale part: kept share, share of its convex hull |
| `CARVE_PALE_SPOKE_JOIN_MIN_PX` / `_DIAG_FRAC` | 1 / 0.0014 | pale pieces joined across a spoke |
| `CARVE_PALE_JOIN_MIN_PX` / `_DIAG_FRAC` | 2 / 0.0042 | pale pieces joined across a bolt |

## The `hole_carve` record
`cut_out` returns `hole_carve` beside `mask_review`. It is additive: no existing field changes.

```json
{
  "version": 2,
  "applied": true,
  "reason": "open frame (concept names 'bike'): enclosed background 10.30% of the component (needs 2%)",
  "regions": 3,
  "carved_pixels": 1093228,
  "protected_thin_pixels": 144098,
  "soft_edge_pixels": 44593,
  "speck_pixels": 9354,
  "released_line_pixels": 711,
  "appendage_pixels_kept": 0,
  "pale_part_pixels_kept": 19456,
  "product_pixels_before": 5660280,
  "product_pixels_after": 4567052,
  "open_frame_keyword": "bike",
  "open_frame": {"keyword": "bike", "enclosed_background_fraction": 0.10302,
                 "enclosed_candidate_fraction": 0.110044,
                 "min_enclosed_background_fraction": 0.02, "min_open_frame_fraction": 0.1},
  "outer_band_px": 34,
  "product_pixels_written": 0,
  "completion": {"enabled": true, "released_pixels": 528077, "cells_released": 55,
                 "depth_floor_px": 238, "core_depth_px": 9, "link_radius_px": 7},
  "gate": {"applies": true, "pass": true, "accepted_window_pixels": 583123,
           "completed_window_pixels": 528077, "remainder_pixels": 62582,
           "enclosed_candidate_pixels": 1173782, "enclosed_cells": 96,
           "remainder_fraction": 0.053317, "max_remainder_fraction": 0.1}
}
```

Fields:
- `carved_pixels` is the number of pixels that were 255 and now are not (soft edge included). `product_pixels_before - product_pixels_after == carved_pixels`.
- `protected_thin_pixels` is the count of thin-structure pixels inside the candidates (spokes, chain, cables).
- `completion.strip_pixels`, `strip_cells`, `spoke_bridge_pixels`: what step 4f released and bridged back. `gate.strip_pixels` repeats the released count; `gate.strip_residue_pixels` is the residue, already included in `gate.remainder_pixels`.
- `pale_part_pixels_kept` is the count of carved pixels the pale-part guard returned to the product.
- `completion.released_pixels` is what the completion pass released (step 4e), counted before the 1 px gap closing and the soft edge. `completion.enabled` is false when `AGENTHAND_HOLE_COMPLETE=off` or the carve is off.
- `gate` is the enclosed-remainder gate. `accepted_window_pixels` are the colour-proven windows, `completed_window_pixels` the completed ones, `remainder_pixels` the enclosed window area left unreleased; `enclosed_candidate_pixels` is their sum. `pass` is false when `remainder_fraction` is above `max_remainder_fraction`.
- Version 2 adds `completion` and `gate`; the other fields keep their meaning.
- When nothing is carved, `applied` is false. `reason` then says why, the counts are 0, and before equals after. Possible reasons:
  - "disabled (AGENTHAND_HOLE_CARVE=off)"
  - "the mask has no product pixel"
  - "no mask pixel lies deeper than the outer band (nothing enclosed)"
  - "no enclosed background-like region: ..."
  - "not an open frame: ..."
  - "source and mask frames differ"
  - "too few background or product colour samples around the mask"
- A record with `applied` false carries `"gate": {"applies": false, "pass": true}`: nothing was carved, so no open-frame interior was judged.

## Kill switches
- `AGENTHAND_HOLE_CARVE=off` (also `0`, `false` or `no`, in upper or lower case) returns SAM's mapped mask byte-for-byte. `hole_carve.applied` is then false with the reason "disabled (AGENTHAND_HOLE_CARVE=off)", the completion is off too, and the gate does not apply. Unset or another value means the carve runs.
- `AGENTHAND_HOLE_COMPLETE=off` (same values) turns off only the completion pass. The mask is then the 0e338ba carve with the pale-part guard applied (on IMG_1443 the guard returns 13,170 reflector-edge pixels to the product; without the guard the output was byte-identical to 0e338ba). The gate still runs on the result, so a photo whose wheels hold shaded background is refused with the completion off (IMG_1443: 50.3% remainder). To stage such a photo with SAM's uncarved disc instead, use `AGENTHAND_HOLE_CARVE=off`.

## Enclosed-remainder gate
A partial carve inside an open frame (old background left in the wheels) must fail loudly in a live run. After carving, the completion pass's bookkeeping gives the frame interior: the accepted windows, the completed windows, and every enclosed cell (not touching the band, at least the cell floor, deeper than the depth floor) linked to an accepted window through enclosed cells. The linked enclosed cells left unreleased are the **remainder**.

`cut_out` refuses when remainder / (accepted + completed + remainder) is above 10%. The gate applies only when the open-frame carve applied; jackets, couches, cars and the other solid items have `gate.applies` false and are not affected.

The refusal follows cut_out's error shape (`status: "error"`, `error.code`, `error.message`) and adds:
- `error.code`: `HOLE_CARVE_INCOMPLETE`;
- `error.message`: the remainder in pixels and as a share, the limit, why the cut is refused, and the retake;
- `error.remainder`: `remainder_pixels`, `enclosed_candidate_pixels`, `remainder_fraction`, `max_remainder_fraction`, `accepted_window_pixels`, `completed_window_pixels`;
- `error.retake`: three concrete changes for the retake (decision 114: a failure carries its correction path): move the item out of shade or turn it so no shadow falls inside the frame; shoot against a plain, evenly lit background whose colour differs from the frame and tyres; keep the whole item in frame with a margin of background;
- `error.operator_override`: `AGENTHAND_HOLE_CARVE=off` stages SAM's uncarved mask instead;
- `error.cost_counted_usd`, and at the top level `cost_usd`, `usage`, `call_id`, `attempts`, `hole_carve`, `mask_provenance`: the SAM call already ran and is settled, so the refusal carries its cost and record. No asset is stored.

`segmentation::hole_carve_gate_refusal(record)` builds the refusal; `tool_cut_out` returns it.

Why 10%. Remainder fractions measured offline:

| Photo / mask | Completion on | Colour-only carve (completion off) |
|---|---|---|
| IMG_1443, run-1443-fixed mask | 7.33% | 51.1% |
| IMG_1443, 19:06 recorded take mask | 7.46% | 52.1% |
| IMG_1443, run-1443-merged mask | 7.37% | 50.9% |
| IMG_1443 (older decode), box-fixed mask | 3.87% | 37.5% |
| P12 blue bike frame (1920x1080) | 2.83% | 14.2% |

The pocket guard keeps cells such as the cog, and the strip residue (joints that step 4f does not release) counts too, so the hero values rose from about 3% to about 7.8%. The largest passing value is 7.84% and the smallest failing value is 14.2%. A 10% limit leaves 2.2 points (1.3x) of margin on the hero photo. The residue term does not yet separate a round-2 style output (no strip release) from round 3: emulated, it reads about 9%, under the limit. P12 barbecue, P12 patio set, both P18 jacket masks, the P17 card and both P16 car frames do not apply the carve, so the gate does not apply to them.

## Evidence
`tests/hole_carve.rs` (`cargo test --release --test hole_carve`) has 13 tests plus 1 opt-in.

| Test | Asserts |
|---|---|
| `wheel_background_is_carved_and_spokes_hub_and_silhouette_stay` | Ring, 12 spokes and a hub over bright textured stone with mortar lines: >= 95% of the enclosed background removed, >= 98% of spoke and hub pixels kept, tyre and outside unchanged, nothing raised, record counts match the mask, soft edge <= 128, deterministic |
| `wheel_is_carved_without_a_keyword_when_it_is_an_open_frame` | concept "product": carved through the 10% open-frame gate |
| `solid_box_is_a_no_op` | blue box with a dark label stripe, with and without a keyword: identical mask, `applied` false |
| `kill_switch_returns_the_identical_mask` | off / OFF / 0 / false / no: identical mask; unset or "on": carves |
| `ring_with_product_coloured_interior_is_a_no_op` | interior the tyre's colour: identical mask |
| `open_frame_keywords_match_whole_word_prefixes`, `empty_and_mismatched_masks_are_no_ops` | small contract pieces |
| `shaded_wheel_interior_is_completed_and_spokes_tyre_hub_stay` | The wheel with its lower part in a cast shadow (dark, nearly neutral paving a few levels above the tyre). Colour-only carve: 2.4% of the shaded interior removed. With the completion: 78.2% removed, 100% of spoke pixels kept (also in the shade), hub kept, tyre and outside unchanged, nothing raised, counts match, gate passes, deterministic |
| `completion_kill_switch_keeps_the_colour_proven_carve` | `AGENTHAND_HOLE_COMPLETE` off / OFF / 0 / false / no: the colour-only mask and record, completion disabled, gate still measured; unset or "on": the completed carve; `AGENTHAND_HOLE_CARVE=off` still returns the identical input mask |
| `reflector_and_bolt_stay_product_inside_a_completed_wheel` | A pale, prism-textured plate with a brass bolt between two spokes, half over sun and half over the shade of the shaded wheel: 0 plate and 0 bolt pixels released, the shade still completes, spokes kept, gate passes |
| `dark_joint_band_releases_while_spokes_reflector_and_bolt_stay` | A straight non-radial band 12 px wide (about 5% of the rim radius) darker than the shade, across the shaded wheel with the reflector plate: at least 60% released (measured 70%; the rest is fringe beside spokes and the ends below the depth floor), spokes in the band and overall kept, reflector and bolt 0 released, gate passes; with the completion off the band counts as strip residue |
| `dim_spokes_in_the_shade_stay_continuous` | Smooth shade with the spokes in it dimmed to about 20 levels above it (under the thin floor): every spoke's centre line kept through the shade (longest gap at most 2 px, at least 13 px from the shadow edge), at least 90% of shaded spoke px kept, the shade still completes |
| `gate_refuses_a_partial_open_frame_carve_with_a_retake` | The colour-only carve of the shaded wheel fails the gate (40% remainder); `hole_carve_gate_refusal` returns `HOLE_CARVE_INCOMPLETE` with the remainder numbers, three retake changes and the operator override. The completed carve, the sunlit wheel and a carve that did not apply return no refusal |
| `real_fixture` (`#[ignore]`) | real cut_out: writes `bike_carve_record.json`, `mask_carved.png` and `bike_carve_before_after.jpg` |

Red before the change:
- With the carve forced off (the pre-carve identity mask), the three carving tests fail: the wheel test, the no-keyword test, and the "on" half of the kill-switch test.
- The four no-op guards pass by design. They are what fails an over-eager carve.
- Against the original tree, the file does not compile, because `carve_holes` does not exist there.

Run `real_fixture` like this:
```text
AGENTHAND_CARVE_FIXTURE=<dir with source.png mask.png> AGENTHAND_CARVE_OUT=<out dir> \
[AGENTHAND_CARVE_CONCEPT="blue kids BMX bike"] \
cargo test --release --test hole_carve real_fixture -- --ignored --nocapture
```

IMG_1443 bike (run-1443-fixed, 5712x4284, concept "blue kids BMX bike") in 5.2 s:
- 587,550 px carved over 3 regions;
- 114,473 thin pixels protected;
- product pixels went from 5,660,280 to 5,072,730 (-10.4%).

The spokes stay continuous across every carved window. The reflector, hub, chain and rims stay. The wall, the pavers and the spokes' own cast shadows on the stone go to the plate.

With the completion pass and both guards (same fixture):
- 1,093,228 px carved; product pixels went from 5,660,280 to 4,567,052;
- the completion released 528,077 px; the pale-part guard returned 19,456 px; gate remainder 5.33%;
- rear wheel, r = 760 px disc at the fitted centre: 172,123 -> 494,655 px transparent; front: 418,185 -> 601,331;
- reflectors (polygons drawn from the source, harness only): pixels released against SAM's mask, rear 5,888 (colour-only carve) -> 211, front 2,088 -> 329. All of them lie on the reflector outline (58 and 79 inside the outline eroded by 3 px). The bolts and the cog are product;
- against the colour-only carve, the changes are the completed windows, soft-edge alpha next to them, and the pixels the two guards returned to the product. Against round 1 (completion without the guards), round 2 only returns pixels to the product: 45,804 px, none above SAM's mask, none in the outer band. No pixel is added beyond SAM's mask, the outer band is untouched, no completed pixel lies shallower than the depth floor.

## Limits
- **Background the inside model calls product stays.** The completion releases shaded background the colour test could not prove, but a pixel the second colour model proves product is kept structure. On IMG_1443 the darkest paver joints in the lower front wheel stay as dark lines across the window, and the synthetic test leaves about 22% of its shade (a band next to the shadow edge). The gate counts unreleased open cells, not structure, so it does not see this residue.
- **Dark parts are protected by shape only.** The pocket guard keeps the cog because the chain and the dropout enclose it. A dark product part that no solid part encloses (a dark sticker on a shaded window) has no colour evidence against the shade and can be completed. The pocket guard also keeps about 8,000 px of small background slivers on IMG_1443 (next to the dropout, the front hub nut and the rim).
- **Pale parts only.** The pale-part guard covers pale, compact parts (reflectors, light plates). A brass part on its own is not covered; on a reflector the hull covers it.
- **Spokes grazing a shadow edge.** A spoke whose shaded run lies within one line square (13 px) of the shadow edge sits in the edge's own line contrast, fails the density limit and is not a faint line; the completion can still release it. Faint lines need smooth shade: in rough shade the density limit rejects them.
- **Wide joints and shaded patches can stay.** Step 4f releases joint bands up to about 43 px wide on IMG_1443 (17,123 px in the front wheel's lower sector). The upper horizontal joint in the front wheel is wider and stays as a dark bar, as do parts of the lower and vertical joints next to spokes and below the depth floor. Raising the width limit would let the fork (about 70 px) qualify; telling them apart needs the wheel's hub (spoke convergence), which is not implemented. Shaded patches in the rear wheel's lower right sector and behind the frame tubes are not linked to an accepted window and stay. The gate counts strip residue (31,827 px on IMG_1443) but not these patches.
- **Windows behind the frame.** Cells separated from every accepted window by a frame tube (over 14 px) are not linked and stay. On IMG_1443, 78,585 px of open cells in the rear wheel are unlinked or below the cell floor; most lie between the chainstay and the seat stay.
- **Window size.** Spaces between spokes narrower than about 13 px, or under the cell floor, are left. The very centre of a dense spoke cross, near the hub, usually stays.
- **One colour model per neighbourhood.** A background that looks nothing like the 6 to 30 px ring around the mask is not seen as background. For example, a sunlit patch visible only through the wheel.
- **Open frames without a keyword.** These need >= 10% enclosed candidate area, so a chair photographed tight may need the concept to name it. Solid products are never carved: they have no enclosed background, or less than 2%.
- **`mask_review`.** A heavily carved frame can push `bbox_fill` under 0.25 and raise its "scattered or frame-like" warning. Read the overlay as usual.
- **Cost.** Time is linear in the bbox area. Measured back to back on the offline machine under the same load: 4.8 s for the colour-only carve (0e338ba), 8.2 s with the completion and both guards, 8.9 s with the strips (load average about 2.4 on 2 cores from other builds; not optimised).
