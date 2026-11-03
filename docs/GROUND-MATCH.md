# Ground match: the generated ground follows the photo's ground

Requirement (Dan via Muse, 2026-11-02): the dark asphalt patch seen in the rear wheel is fine. The background seen through the other spokes needs "a similar style to that real ground", and agenthand must do this automatically.

Why: a generated plate is drawn from text, so its ground can be any surface. The carved product layer keeps remnants of the real ground (paving seen between spokes the carve left as product). Next to them, a generated ground of another family or tone reads wrong.

Fix: draw only, generated plates only. The source photo's own ground is sampled, named and handed to the plate prompt as one short clause, the way the angle steer sets the camera. Identify is untouched. Pixels are not touched by this step.

## Sampling (`ground_match::ground_descriptor`)
Grid: long side at most 512 cells (12 px cells on a 24 MP photo). Per cell: mean RGB and the in-cell luma standard deviation.
- **Around and below.** Cells whose centre lies from 6% of the product height above the product's ground line (`ground::ground_line`, the same line the ground shadow uses) to 30% below it, within 60% of the product width either side of its bbox, at least 3 cells clear of any product pixel (alpha > 0).
- **Remnants inside open frames.** Background cells enclosed by the product (not 4-connected to the bbox border), in the lower 35% of the product height, 1 cell clear of product pixels: ground seen through the wheels.
- At least 50 cells, else no descriptor ("too little ground visible around the product").
- Not sampled: a subject that fills the frame, a layer with no opaque pixel, a source that does not share the layer's frame.

## Stats and words
- value: luma of the channel medians / 255;
- warmth: (R - B) / luma of the channel medians;
- saturation: median of the cells' HSV saturation;
- hue: saturation-weighted circular mean, only when at least 20% of cells carry colour (saturation >= 0.08);
- green fraction: cells with hue 70-170, saturation >= 0.12, green the largest channel;
- texture: median in-cell luma deviation.

Surface (a fixed, ordered rule list, `classify`):
1. green fraction >= 0.35: "green lawn grass" (value under 0.30: "dark green lawn");
2. saturation >= 0.22 with a hue:
   - under 15 or from 340: "red brick paving";
   - 15-50: "warm sandstone paving" (value >= 0.55), else "tan gravel" or "brown wooden decking" by texture (>= 9 levels: gravel) for value >= 0.30, else "dark brown earth";
   - 50-70: "sandy ground";
3. the grey family by value:
   - under 0.22: "dark asphalt";
   - under 0.40: "dark grey paving";
   - under 0.58: "grey concrete paving";
   - under 0.75: "light grey stone paving";
   - else "pale limestone paving" (warmth >= 0.05) or "pale concrete paving".
   Prefixed "weathered" when the texture is at least 6 levels, "smooth" under 3.

Value words: dark (< 0.20), mid-dark (< 0.35), mid (< 0.55), light (< 0.72), pale.

Temperature: warm (>= 0.12), warm-neutral (>= 0.05), neutral (> -0.02), cool-neutral (> -0.08), cool.

## Prompt clause
After the light phrase in the generated scene (`meta::generated_plate_scene_matched`):

"Premium, polished listing scene; the ground surface matches the original photo's ground: weathered dark grey paving, mid-dark value, neutral tone."

- The caller's scene text still leads. With Dan's B2 stone courtyard, the courtyard stays and the paving tone follows the source.
- With no clause, the text is `generated_plate_scene_dressed` exactly.
- `source_edit` and `source_blur` prompts are unchanged: those plates already keep the photo's ground.

## Record
- **Draw result `ground_descriptor`** (beside `angle_lock`):
  - with a descriptor: `{"stated": true, "clause", "surface", "value", "temperature", "phrase", "around_cells", "remnant_cells", "stats": {"cells", "value", "saturation", "hue_deg", "warmth", "green_fraction", "texture", "median_rgb"}}`;
  - otherwise: `{"stated": false, "reason"}`.
- **Receipt `ground_match`** (additive, not graded; generated draws through the server, `composite::draw_product_generated`):
  - `{"version": 1, "applied": true, "reason": null, "clause", "sampled": <descriptor>, "plate": <same stats on the fitted plate plus "surface", "value_word", "temperature">, "delta": {"value", "warmth", "saturation", "same_value_word", "same_temperature"}}`;
  - when no descriptor: `{"version": 1, "applied": false, "reason"}`.
  - The draw result carries a copy.
  - The plate side is read on the fitted plate before the ground shadow, from the product's ground line down 15% of its height, across its width.
  - The check reports; it does not refuse a draw.
- **Kill switch.** `AGENTHAND_GROUND_MATCH=off` (also `0`, `false`, `no`): no clause, and the records read "disabled by AGENTHAND_GROUND_MATCH".

## IMG_1443
`run-1443-fixed/source.png` with `mask.png` (offline, 0.42 s):
- **"weathered dark grey paving, mid-dark value, neutral tone"**;
- value 0.309, saturation 0.083, warmth 0.000 (median RGB 79/78/79), texture 18.9 levels;
- 29,074 cells, of which 142 are remnants. The carved v102 mask reads the same words with 1,296 remnant cells.
- Sunlit paving cells read warm and shaded cells cool. The channel medians balance at neutral, so the rule list says neutral, not cool-neutral. No threshold was moved for this photo.
- Sanity check of the plate side against the stored `source_edit` plate of the same photo: value delta +0.003, same value word, same temperature word.

## Tests (`tests/ground_match.rs`)
- `synthetic_grounds_are_named_by_surface_value_and_tone`:
  - dark cool asphalt reads "weathered dark asphalt", dark, cool, with remnant cells seen inside an open ring;
  - pale warm stone reads "smooth pale limestone paving", pale, warm;
  - lawn reads "green lawn grass";
  - mid grey reads "grey concrete paving", neutral;
  - the red wall above the line is not sampled;
  - deterministic.
- `clause_rides_the_generated_prompt_only_and_the_kill_switch_drops_it`: clause in the generated scene after the caller's setting; no clause gives the dressed scene exactly; absent from the `source_edit` prompt; each kill-switch spelling drops it.
- `receipt_records_the_match_and_product_pixels_stay_exact`: through the store; sample, plate and delta recorded (a pale plate under a dark source shows value delta > 0.5); alpha-255 pixels byte-exact; receipt verifies; the composite is identical with and without the plan; the legacy path has no record.
- `real_fixture_ground_descriptor` (ignored, opt-in): `AGENTHAND_MATCH_ARTIFACTS=<run dir> [AGENTHAND_MATCH_USE_MASK=1] cargo test --release --test ground_match real_fixture -- --ignored --nocapture`.

## Limits (disclosed)
- Words, not pixels: the clause steers the image model; it may still draw another tone. The `ground_match` delta shows when it did.
- The classifier is a short rule list on colour statistics. It does not see joints, pattern or material: wood decking against tan gravel is a texture threshold, and a grey stone against concrete is a value band.
- Shadows on the ground (the product's own, the photographer's) are part of the sample; medians damp them, not remove them.
- The plate check assumes the generated ground lies under the product's ground line; a scene that put a step or wall there is measured as it is.
