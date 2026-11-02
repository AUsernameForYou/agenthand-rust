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
     - it links to an accepted window across gaps of at most 14 px (a spoke or another thin line in between). Linking is transitive: the accepted windows and the window cells are each grown by 7 px and labelled together.
   - **Release.** Windows are released whole, colour-proven or not. They join the carve after the line release and the specks below, which judge only the colour-proven windows. So a background-coloured spoke, a spoke nipple or a chain plate next to a completed window keeps the status the colour-only carve gave it.
5. **Finish.**
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

## The `hole_carve` record
`cut_out` returns `hole_carve` beside `mask_review`. It is additive: no existing field changes.

```json
{
  "version": 2,
  "applied": true,
  "reason": "open frame (concept names 'bike'): enclosed background 10.30% of the component (needs 2%)",
  "regions": 3,
  "carved_pixels": 1138074,
  "protected_thin_pixels": 157940,
  "soft_edge_pixels": 48652,
  "speck_pixels": 9354,
  "released_line_pixels": 711,
  "appendage_pixels_kept": 0,
  "product_pixels_before": 5660280,
  "product_pixels_after": 4522206,
  "open_frame_keyword": "bike",
  "open_frame": {"keyword": "bike", "enclosed_background_fraction": 0.10302,
                 "enclosed_candidate_fraction": 0.110044,
                 "min_enclosed_background_fraction": 0.02, "min_open_frame_fraction": 0.1},
  "outer_band_px": 34,
  "product_pixels_written": 0,
  "completion": {"enabled": true, "released_pixels": 554993, "cells_released": 62,
                 "depth_floor_px": 238, "core_depth_px": 9, "link_radius_px": 7},
  "gate": {"applies": true, "pass": true, "accepted_window_pixels": 583123,
           "completed_window_pixels": 554993, "remainder_pixels": 35666,
           "enclosed_candidate_pixels": 1173782, "enclosed_cells": 96,
           "remainder_fraction": 0.030386, "max_remainder_fraction": 0.1}
}
```

Fields:
- `carved_pixels` is the number of pixels that were 255 and now are not (soft edge included). `product_pixels_before - product_pixels_after == carved_pixels`.
- `protected_thin_pixels` is the count of thin-structure pixels inside the candidates (spokes, chain, cables).
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
- `AGENTHAND_HOLE_COMPLETE=off` (same values) turns off only the completion pass. The mask is then byte-identical to the 0e338ba carve (checked on IMG_1443). The gate still runs on the result, so a photo whose wheels hold shaded background is refused with the completion off (IMG_1443: 50.3% remainder). To stage such a photo with SAM's uncarved disc instead, use `AGENTHAND_HOLE_CARVE=off`.

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
| IMG_1443, run-1443-fixed mask | 3.04% | 50.3% |
| IMG_1443, 19:06 recorded take mask | 3.08% | 51.4% |
| IMG_1443, run-1443-merged mask | 2.90% | 50.1% |
| IMG_1443 (older decode), box-fixed mask | 2.83% | 36.8% |
| P12 blue bike frame (1920x1080) | 2.83% | 14.2% |

The largest passing value is 3.08% and the smallest failing value is 14.2%. A 10% limit leaves 6.9 points (3.2x) of margin on the hero photo and still refuses every colour-only carve measured. P12 barbecue, P12 patio set, both P18 jacket masks, the P17 card and both P16 car frames do not apply the carve, so the gate does not apply to them.

## Evidence
`tests/hole_carve.rs` (`cargo test --release --test hole_carve`) has 10 tests plus 1 opt-in.

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

With the completion pass (same fixture, 6.5 s here against 5.2 s for the colour-only carve on the same machine):
- 1,138,074 px carved; product pixels went from 5,660,280 to 4,522,206;
- the completion released 554,993 px in 62 windows; gate remainder 3.04%;
- rear wheel, r = 760 px disc at the fitted centre: 172,123 -> 528,579 px transparent; front: 418,185 -> 612,253;
- outside the completed windows the mask differs from the colour-only carve only in soft-edge alpha (1,960 px) and in 1,178 px that the colour-only carve closed as 1 px gaps and that now border a completed window. No pixel is added to the product, the outer band is untouched, no completed pixel lies shallower than the depth floor.

## Limits
- **Background the inside model calls product stays.** The completion releases shaded background the colour test could not prove, but a pixel the second colour model proves product is kept structure. On IMG_1443 the darkest paver joints in the lower front wheel stay as dark lines across the window, and the synthetic test leaves about 22% of its shade (a band next to the shadow edge). The gate counts unreleased open cells, not structure, so it does not see this residue.
- **Background-coloured parts inside a completed window can go.** On IMG_1443 the rear reflector's brass bolt and part of its pale edge band (about 4,100 px) lie in a completed window and are released; the colour-only carve kept them. The dark band between the rear chain and the dropout (about 18,000 px) is released too; most of it is shaded background between the chain and the chainstay, its end next to the axle may include the edge of the cog.
- **Windows behind the frame.** Cells separated from every accepted window by a frame tube (over 14 px) are not linked and stay. On IMG_1443, 78,585 px of open cells in the rear wheel are unlinked or below the cell floor; most lie between the chainstay and the seat stay.
- **Window size.** Spaces between spokes narrower than about 13 px, or under the cell floor, are left. The very centre of a dense spoke cross, near the hub, usually stays.
- **One colour model per neighbourhood.** A background that looks nothing like the 6 to 30 px ring around the mask is not seen as background. For example, a sunlit patch visible only through the wheel.
- **Open frames without a keyword.** These need >= 10% enclosed candidate area, so a chair photographed tight may need the concept to name it. Solid products are never carved: they have no enclosed background, or less than 2%.
- **`mask_review`.** A heavily carved frame can push `bbox_fill` under 0.25 and raise its "scattered or frame-like" warning. Read the overlay as usual.
- **Cost.** Time is linear in the bbox area: about 5 s for a 24 MP frame on one core for the colour-only carve; the completion pass and the gate add about 1.3 s (measured 5.2 s -> 6.5 s on the offline machine).
