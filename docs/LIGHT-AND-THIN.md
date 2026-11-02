# Light direction and thin structure

Defects (Daniel, 2026-11-01 19:14 PT, premium driveway draw of the IMG_1443 bike in `generated` plate mode):
1. The generated scene was lit golden-hour from camera-right. The bike was shot in flat daylight with its shadow straight under it, so the composite reads pasted.
2. The spokes get lost. Through the wheel openings the plate shows dark blocks and hard edges that fight the thin dark spokes. The spokes are product pixels and verify untouched; they just stop reading.

Fix: automatic, draw only. Identify is untouched (instructions, output schema). Product pixels (alpha 255) never change, and the receipt's counts, claim and grading are unchanged.

## A. Light direction
`staging::estimate_light_direction(source, foreground)` is pure and cheap. It reads the source on a cell grid (long side at most 256 cells). It follows the angle lock's discipline: it claims a side only when the photo shows one plainly.

1. **Ground region.** From the product's vertical middle to the bottom of the frame, one product width either side of the bbox, outside the product mask (dilated one cell).
2. **Cast shadow (method `cast_shadow`).**
   - Candidate cells: luma between 0.20 and 0.72 of the ground's median luma, and hue-neutral (r and g shares of r+g+b within 0.05 of the ground's median).
   - Seeds: candidates within 2 cells of the product's lower half. Grown 4-connected inside the ground region.
   - Gates: at least 3% of the product bbox area, at most 50% of the ground cells, and filling at least 30% of its own bounding box (so a network of dark paving joints does not count).
   - Direction: the region centroid relative to the contact point (bbox bottom centre), in bbox widths (`nx`) and heights (`ny`):

     | Reading | Rule | Light |
     |---|---|---|
     | shadow to camera-right | `nx >= 0.15` and `|nx| >= |ny|` | from camera-left |
     | shadow to camera-left | `nx <= -0.15` and `|nx| >= |ny|` | from camera-right |
     | shadow behind | `ny <= -0.10` and deeper than sideways | from behind the camera |
     | shadow toward the camera | `ny >= 0.10` and deeper than sideways | from behind the product |
     | shadow beneath | `|nx|` and `|ny|` both at most 0.06 | overhead |
     | anything else | | inconclusive |
3. **Gradient (method `gradient`).** If the cast-shadow reading is inconclusive, the method compares the ground's left, middle and right thirds, excluding the shadow region. A side is claimed when the left and right thirds differ by at least 20% of the ground median, the middle third lies between them, and the ground covers at least 4% of the frame. The light comes from the brighter side.
4. **Otherwise nothing** (method `none`). No clause is added.

Phrases (`meta::light_clause`):
- Cast shadow, from camera-left: "Sun low from camera-left; long soft shadows fall to camera-right across the ground." The camera-right version is mirrored.
- From behind the camera: "Light from behind the camera; soft shadows fall straight back, away from the camera."
- From behind the product: "Light from behind the scene, facing the camera; soft shadows fall toward the camera."
- Overhead: "Soft high daylight from overhead; short soft shadows fall directly beneath objects."
- Gradient, which claims a side but not a low sun: "Light from camera-left; soft shadows fall to camera-right." The camera-right version is mirrored.

Where the phrase goes:
- **`generated` plates.** The phrase is appended to the plate scene after the angle steer (`meta::generated_plate_scene_dressed`).
- **`source_edit` and `source_blur` plates.** No clause is added; the edit keeps the photo's own light, and their prompts are unchanged. The estimate instead moves the contact shadow.
- **Contact shadow.** `composite::apply_contact_shadow_offset` (through `staging::finish_plate_lit`) moves the ellipse away from the light:
  - sideways by 4% of the opaque bbox width (`CONTACT_SHADOW_OFFSET_X`);
  - in depth by half the ellipse's vertical radius (`CONTACT_SHADOW_OFFSET_Y`);
  - not at all for overhead light.
  It is still zero under every alpha-255 pixel. A zero offset gives exactly `apply_contact_shadow`. Generated plates have no contact ellipse in this tree, so for them only the clause applies.

Record `light_direction` in the draw result, beside `angle_lock`:
```json
{"stated": true, "phrase": "...", "method": "cast_shadow" | "gradient" | "none",
 "side": "from_camera_left" | ... | null, "shadow_falls": "camera-right" | ... | null,
 "signals": {...}, "contact_shadow_offset_px": [dx, dy]}
```
- `stated` is true only when the phrase went into the plate prompt (generated plates). When it is false, `reason` replaces `phrase`.
- `contact_shadow_offset_px` appears on source plates that draw the contact shadow.

## B. Thin structure
**Trigger.** `composite::thin_trigger_for(foreground, concept)`, implemented in `staging::thin_structure_trigger`. The check runs in this order:
1. **`concept`.** The identify concept that cut_out used contains an open-frame word as a whole token, singular or plural: bicycle, bike, bmx, tricycle, trike, scooter, wheel, wheelchair, stroller, chair, stool, rack, ladder, frame, easel, tripod, trellis, cart.
   - cut_out now stores the accepted phrase in the mask PNG's provenance chunk (key `concept`), and draw reads it back.
   - The cut_out result's `mask_provenance` and the receipt's `mask_*` fields are unchanged.
   - Masks cut before this change have no concept, so only the hole trigger applies to them.
2. **`holes`.** The mask has an enclosed hole of at least 0.5% of the mask bbox area (`THIN_HOLE_TRIGGER_FRACTION`).
   - An enclosed hole is a 4-connected component of background cells inside the bbox that does not reach the bbox border.
   - The check runs on a grid with a long side of at most 1024 cells; a cell counts as product when any alpha>0 pixel lands in it.
   - Holes of at least 0.05% (`THIN_HOLE_MIN_FRACTION`) are counted and softened.
3. Otherwise `none`.

`analyze_subject`'s `thin` flag (thin runs) is unchanged and still drives the matte width. It never fires on a bike whose wheels SAM filled solid, which is why the trigger extends it.

**Prompt (generated plates only).** When triggered, `meta::THIN_PLAIN_CLAUSE` follows the light phrase:
"Keep the surface directly behind the product plain, smooth and evenly lit, with no dark blocks, doorways, vehicles, furniture or hard edges showing through the product's open frame."

**Plate guard (every plate kind drawn by `draw`).** `composite::soften_thin_holes(plate, foreground, trigger)` runs on the finished plate:
- `source_edit` and `source_blur` plates: in `stage_source_plate`, after the contact shadow and matte and before the plate is stored. The draw's shadow carry and edge fix follow as before.
- `generated` plates: on the stored plate, in the plate's own frame; the foreground maps on through `resample::fit`'s cover crop. A softened plate is stored as a new plate asset, parented to the original.

Steps:
1. Product cells are forward-rasterised onto the plate's analysis grid (long side at most 1024) and dilated by one cell. Enclosed holes are found again at this resolution.
2. Texture is the mean local luma standard deviation in a 5x5-cell window:
   - in the holes: cells more than 4 cells (margin 2 plus window radius 2) from the product;
   - in the ring: non-product, non-hole cells from that distance out to 4% of the bbox's long side.
3. The guard softens only when the holes' texture is at least 2.0 luma levels and at least 1.5x the ring's (`THIN_TEXTURE_RATIO`; the ring counts as at least 1.0).
4. Each step (at most 3) blurs the grid with three box passes of radius 0.6% of the bbox's long side. It then lifts the hole cells 25% (`THIN_LIFT`) toward the ring's mean tone. Hole cells within 2 cells of the product are untouched, and a 3-cell feather follows. The guard stops once the ratio is at most 1.15 (`THIN_TEXTURE_TARGET`).
5. The grid change is added to the full-resolution plate bilinearly. The guard writes only plate pixels whose footprint, with one pixel of slack, holds no product pixel (alpha > 0).
6. `texture_after` is measured again on the written plate.

Record `thin_structure` in the draw result:
```json
{"triggered": true, "trigger": "concept" | "holes" | "none", "holes": 4, "holes_softened": true,
 "texture_before": 9.1, "texture_after": 3.2, "texture_ring": 2.9,
 "largest_hole_fraction": 0.021, "concept_word": "bike", "steps": 2, "pixels_written": 51234,
 "note": null | "disabled by AGENTHAND_THIN" | "no enclosed holes in the product mask" | "hole texture within the ring's; plate unchanged" | ...}
```

**Kill switch.** `AGENTHAND_THIN=off` (also `0`, `false`, `no`) turns off both the clause and the plate guard. The record then reads triggered false, trigger `none`, note "disabled by AGENTHAND_THIN".

## C. Generated plates: scene exclusions and the fitted-plate hash
These come from the generated-mode investigation, ranked fixes 1 and 2.
- **Scene exclusions.** When the concept is known, `meta::scene_exclusions` gives the concept phrase, its head noun's plural, and then "bicycles or other vehicles", "people" and "animals". The generated scene ends with: "Empty scene, no blue kids bmx bike, no bikes, no bicycles or other vehicles, no people, no animals; the product will be placed afterwards."
  - The draw result records `scene_exclusions: [...]`, which is empty when the concept is unknown; the prompt is then unchanged.
  - The concept is reduced to lowercase letters, digits, hyphens and apostrophes, at most 8 words.
- **`plate_fitted_pixel_sha256`.** This is an additive receipt field for `model_generated` draws only. It holds the pixel SHA-256 of the cover-fitted plate that was actually composited, whereas `plate_pixel_sha256` hashes the stored plate before the fit. It is absent on source plates.
  - Verify does not grade it yet. It lets a later check re-derive the background: every alpha-0 output pixel equals this plate.

## Calibration: IMG_1443 (source_edit run `run-1443-fixed`)
- Thin guard: it triggers on the concept word "bmx" ("blue kids BMX bike"), with 4 enclosed holes, the largest 2.2% of the mask bbox (the frame triangle). Hole texture is 12.83 against a ring texture of 12.68 (ratio 1.01), because stone wall and pavers fill both. The plate is unchanged, 0 pixels are written, and all 5,660,280 product pixels are identical. The guard took 0.34 s at 5712x4284.
- Light: nothing is stated. A dark region touches the bike, centroid 0.10 bbox widths left and 0.20 heights behind the contact point, but it fills only 9% of its bounding box (paver joints and wall shadow), under the 30% fill gate. There is no gradient (0.05).

## Limits
- **Wheels SAM fills solid.** The IMG_1443 mask fills both wheel discs. The background seen through the spokes is then product pixels (alpha 255): baked into the layer, never touched, and verified as product. The guard can only soften enclosed holes, and on that cut those are the frame triangle and three small gaps. Seeing plate through the wheels needs a cut that opens the wheel interiors, which is a cut_out change, not a draw change.
- **Mask truth.** The light estimate reads whatever is dark and neutral beside the product's lower half. A dark, neutral object touching the product (a mat, a drain) can read as a shadow if it passes the size and fill gates. Wall shadows above the product's middle are not read.
- **Order.** On source plates, the guard runs before the shadow carry-over. The carry reads its ratio against the softened plate, so a softened hole can carry a little less of the photo's shadow inside it.
- **Placeholder and code-rendered draws.** The local placeholder plate and the room-run code-rendered backdrops get neither the guard nor the records.
- **Clause compliance.** The prompt clauses are best effort; the model may ignore them. The plate guard is deterministic.
