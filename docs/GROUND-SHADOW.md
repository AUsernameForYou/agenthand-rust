# Ground shadow: generated plates put the product on the ground

Defect (Dan, 2026-11-02, decision 141): the mock images "put the bike on the ground"; agenthand's generated draws float. The generated plate is drawn without the product, so its ground texture runs straight under the tyres with no shadow break. `source_edit` plates keep the photo's own shadow (shadow carry, contact ellipse); the generated path had nothing.

Fix: automatic, draw only, generated plates only. A contact shadow and a short soft cast shadow are multiplied onto PLATE pixels at the product's ground line. No product pixel (alpha 255) is ever written. Identify is untouched.

## Method (`src/ground.rs`)
1. **Grid.** The product layer's alpha is averaged on a grid whose long side is at most 1024 cells (6 px cells on a 24 MP photo).
2. **Ground line.** For each pixel column, the lowest product pixel (alpha >= 128), folded per grid column. The lower convex hull of that profile is taken; its longest near-level edge (|slope| <= 0.35, at least a quarter of the product width) is the ground line. Otherwise a level line through the lowest point is used. On a bike the edge joins the two tyre bottoms.
3. **Contacts.** Grid columns whose lowest pixel lies within 1.2% of the product height (at least 1.5 cells) of the line; runs closer than three columns merge. On a bike: one run per tyre.
4. **Contact shadow.** Per column, weight `exp(-gap / (0.03 x product height))`, where `gap` is the product's clearance above the line. The weight is laid on the line as a thin band, Gaussian across it: sigma 1.2% of the product height on the side the shadow falls to, 0.5% on the other side (the thin crease toward the camera on a front-lit shot). The band is blurred along the line by 1.2% of the product height. Peak darkening 0.6. It hugs what touches the ground and fades to nothing under raised parts (pedals, chainring).
5. **Cast shadow.** Every grid cell's alpha coverage is projected onto the ground away from the light; `h` is the cell's height above the line:

   | Light | Shadow falls | Projection |
   |---|---|---|
   | behind the camera (front-lit), and the default | behind the product | `y' = L - 0.22 h` |
   | behind the product (back-lit) | toward the camera | `y' = L + 0.30 h` |
   | camera-left / camera-right | to the other side | `x' = x +- 0.50 h`, `y' = L - 0.06 h` |
   | overhead | beneath | `y' = L - 0.03 h` |

   - Coverage is splatted bilinearly and multiplied by the mapping's area scale, so a solid occluder gives density 1 and nothing exceeds 1.
   - Three height bands (below 20%, 20-50%, above 50% of the product height) are blurred with radii 0.8%, 1.6% and 2.8% of the product height (vertical radius half the horizontal: the ground is foreshortened) and weighted 1.0, 0.8 and 0.6. The shadow is crisper and darker near the contact and softer and lighter farther out.
   - Peak darkening 0.40.
   - Thin parts (spokes, cables) cover a fraction of a cell, so their projected line spreads over the blur and stays faint. On the synthetic test a 2 px pole peaks at 0.016 against 0.195 for a 40 px block.
6. **Apply.** `plate RGB *= (1 - 0.40 cast) (1 - 0.6 contact)`, read bilinearly from the grid, only where the product layer's alpha is below 255. Soft edge pixels (0 < alpha < 255) blend over the shaded plate as they always blend over the plate.

Skipped, plate unchanged, with the reason in the record:
- the product layer has no opaque pixel;
- the subject fills the frame (a surface);
- the product touches the bottom edge of the frame (its ground is not in the picture);
- plate and layer frames differ; the shadow changed no plate pixel.

## Light direction
The cast direction comes from `staging::estimate_light_direction` (docs/LIGHT-AND-THIN.md), the same estimate that steers the generated plate prompt. When it is inconclusive (method `none`), the shadow falls straight back: light from behind the camera, the commonest phone listing setup. That default puts most of the cast shadow behind the product, where the product hides it, so a wrong guess costs little. The record says which: `direction_source` is `light_estimate` or `default_front_lit`.

IMG_1443 reads `none` (no conclusive cast shadow, no gradient), so the hero gets the front-lit default, which matches Dan's locked look (front-lit, sun behind the photographer).

## Where it runs
- `server::tool_draw_with_plate`, generated branch: the plate is generated and thin-guarded as before, then `composite::draw_product_grounded` fits it, hashes the fit (`plate_fitted_pixel_sha256`, unchanged meaning), draws the ground shadow on the fitted plate and composites.
- Not on `source_edit` / `source_blur` plates (they keep shadow carry and the contact ellipse), the local placeholder plate, room-run code-rendered backdrops (wallpaper is shadowless by design), `draw_product` / `draw_product_with_plate_kind` and `composite_images`. The parity gate's paths are byte-identical.

## Record
`ground_shadow` in the draw result and, additively, in the receipt (never graded):

```json
{"version": 1, "applied": true, "reason": null,
 "light": {"side": null, "method": "none", "direction_source": "default_front_lit"},
 "shadow_falls": "behind",
 "ground_line": [[487, 3716], [5161, 3638]],
 "contacts": [{"x_start": 1086, "x_end": 1638, "y": 3701}, {"x_start": 4068, "x_end": 4578, "y": 3652}],
 "contact": {"opacity": 0.6, "blur_px": 40.068},
 "cast": {"opacity": 0.4, "blur_px": [26.712, 53.424, 93.492], "projection": [0.0, -0.22]},
 "grid_cell_px": 6, "product_height_px": 3339,
 "shadow_pixels": 1118334, "max_darkening": 0.724486,
 "product_pixels_written": 0, "plate_pixel_sha256": "8a95ff4a..."}
```

- `plate_pixel_sha256`: the plate actually composited (the fit with the shadow). Every alpha-0 output pixel equals it.
- `plate_fitted_pixel_sha256` (receipt top level): still the fit of the stored plate, before the shadow, so it stays re-derivable from `plate_pixel_sha256`.
- Kill switch `AGENTHAND_GROUND_SHADOW=off` (also `0`, `false`, `no`): `applied: false`, reason "disabled by AGENTHAND_GROUND_SHADOW", composite byte-identical to the plain generated composite.

## Guardrails
- Product pixels: the shadow writes plate pixels only, never under an alpha-255 pixel. `changed_opaque_pixels` and every receipt count are measured as before on the delivered file.
- Deterministic: fixed grid, fixed order; tested by running twice.
- No model call, no cost.

## IMG_1443 (stored fixtures, offline)
Foreground `run-1443-v102/foreground.png` (carved wheels) on the stored plate `run-1443-v102/plate.png`, which has no shadow under the tyres (the floating look):
- light estimate `none`, so `default_front_lit`, shadow behind;
- 2 contacts, one per tyre; ground line slopes 78 px over 4,674 px;
- 1,118,334 plate pixels darkened; 0.49 s for the shadow at 5712x4284 (release, 2-core container);
- 5,072,763 product pixels, 0 changed.

Also on `run-1443-fixed` (un-carved cut): 1,047,861 plate pixels, 5,660,280 product pixels, 0 changed.

Evidence: `ground-evidence/hero_ground_before_after.jpg` (full frame before | after, then a zoom on each tyre contact).

```text
AGENTHAND_GROUND_ARTIFACTS=<dir with source.png foreground.png plate.png> \
AGENTHAND_GROUND_OUT=<out dir> [AGENTHAND_GROUND_ZOOM_W=1100] \
cargo test --release --test ground_shadow real_fixture -- --ignored --nocapture
```

## Tests (`tests/ground_shadow.rs`)
- `front_lit_bike_is_grounded_at_both_tyres_and_no_product_pixel_changes`: synthetic bike (open wheels, 1 px spokes, 3 px tubes, soft tyre rim). Two contacts on the tyres; every alpha-255 pixel byte-exact in the composite and the plate under it untouched; contact darkening 0.23 at both tyres, 0 15 px in front; behind 0.083 against 0 in front; 0 around the frame; deterministic; record keys.
- `the_light_side_moves_the_cast_shadow`: camera-left, camera-right, back-lit, overhead each move the shadow to the expected side; product byte-exact each time.
- `thin_structures_cast_faint_shadows_not_blobs`: 2 px pole against a 40 px block, side light.
- `surfaces_and_bottom_cut_products_are_left_alone`.
- `grounded_draw_verifies_and_the_kill_switch_restores_the_plain_composite`: through the store; record present, 0 changed, `verify_receipt_from_disk` ok, `plate_fitted_pixel_sha256` equal to the plain draw's; every kill-switch spelling gives the plain composite byte for byte; the legacy draw path has no record.
- `real_fixture_hero_ground` (ignored, opt-in): the IMG_1443 run above; asserts 0 changed.

## Limits (disclosed)
- **Plate geometry is unknown.** The shadow is drawn where the product's own ground line is. If the generated scene's floor does not meet the product there (the model drew a step, a wall or a horizon at that height), the shadow lands on whatever is there. The angle steer and the short projection keep this rare, not impossible.
- **Plate light is not read.** The direction follows the source photo's estimate, else front-lit. A generated scene lit hard from one side (the model ignored the light clause, or no clause was stated) can disagree with a front-lit shadow.
- **One ground line.** A product resting on two levels, or seen from high above so its base is an area rather than a line, gets one line; the contact may sit a few pixels off.
- **Python reference:** not ported. Parity is unaffected (the gate's paths do not run it).
