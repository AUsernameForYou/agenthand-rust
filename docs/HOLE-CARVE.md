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

## The `hole_carve` record
`cut_out` returns `hole_carve` beside `mask_review`. It is additive: no existing field changes.

```json
{
  "version": 1,
  "applied": true,
  "reason": "open frame (concept names 'bike'): enclosed background 10.30% of the component (needs 2%)",
  "regions": 3,
  "carved_pixels": 587550,
  "protected_thin_pixels": 114473,
  "soft_edge_pixels": 22585,
  "speck_pixels": 9354,
  "released_line_pixels": 711,
  "appendage_pixels_kept": 0,
  "product_pixels_before": 5660280,
  "product_pixels_after": 5072730,
  "open_frame_keyword": "bike",
  "open_frame": {"keyword": "bike", "enclosed_background_fraction": 0.10302,
                 "enclosed_candidate_fraction": 0.110044,
                 "min_enclosed_background_fraction": 0.02, "min_open_frame_fraction": 0.1},
  "outer_band_px": 34,
  "product_pixels_written": 0
}
```

Fields:
- `carved_pixels` is the number of pixels that were 255 and now are not (soft edge included). `product_pixels_before - product_pixels_after == carved_pixels`.
- `protected_thin_pixels` is the count of thin-structure pixels inside the candidates (spokes, chain, cables).
- When nothing is carved, `applied` is false. `reason` then says why, the counts are 0, and before equals after. Possible reasons:
  - "disabled (AGENTHAND_HOLE_CARVE=off)"
  - "the mask has no product pixel"
  - "no mask pixel lies deeper than the outer band (nothing enclosed)"
  - "no enclosed background-like region: ..."
  - "not an open frame: ..."
  - "source and mask frames differ"
  - "too few background or product colour samples around the mask"

## Kill switch
`AGENTHAND_HOLE_CARVE=off` (also `0`, `false` or `no`, any case) returns SAM's mapped mask byte-for-byte. `hole_carve.applied` is then false with the reason "disabled (AGENTHAND_HOLE_CARVE=off)". Unset or any other value means the carve runs.

## Evidence
`tests/hole_carve.rs` (`cargo test --release --test hole_carve`) has 7 tests plus 1 opt-in.

| Test | Asserts |
|---|---|
| `wheel_background_is_carved_and_spokes_hub_and_silhouette_stay` | Ring, 12 spokes and a hub over bright textured stone with mortar lines: >= 95% of the enclosed background removed, >= 98% of spoke and hub pixels kept, tyre and outside unchanged, nothing raised, record counts match the mask, soft edge <= 128, deterministic |
| `wheel_is_carved_without_a_keyword_when_it_is_an_open_frame` | concept "product": carved through the 10% open-frame gate |
| `solid_box_is_a_no_op` | blue box with a dark label stripe, with and without a keyword: identical mask, `applied` false |
| `kill_switch_returns_the_identical_mask` | off / OFF / 0 / false / no: identical mask; unset or "on": carves |
| `ring_with_product_coloured_interior_is_a_no_op` | interior the tyre's colour: identical mask |
| `open_frame_keywords_match_whole_word_prefixes`, `empty_and_mismatched_masks_are_no_ops` | small contract pieces |
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

## Limits
- **Background in deep shadow is kept.** Wheel windows whose background is darker than the bike, or is in the colour range of the tyre and rims, stay product. On IMG_1443 these are the shadowed pavers in the lower front wheel and the wall shadow behind the rear wheel. This is deliberate: when the colour evidence does not clear the e margin, the pixel stays.
- **Window size.** Spaces between spokes narrower than about 13 px, or under the cell floor, are left. The very centre of a dense spoke cross, near the hub, usually stays.
- **One colour model per neighbourhood.** A background that looks nothing like the 6 to 30 px ring around the mask is not seen as background. For example, a sunlit patch visible only through the wheel.
- **Open frames without a keyword.** These need >= 10% enclosed candidate area, so a chair photographed tight may need the concept to name it. Solid products are never carved: they have no enclosed background, or less than 2%.
- **`mask_review`.** A heavily carved frame can push `bbox_fill` under 0.25 and raise its "scattered or frame-like" warning. Read the overlay as usual.
- **Cost.** Time is linear in the bbox area: about 5 s for a 24 MP frame on one core.
