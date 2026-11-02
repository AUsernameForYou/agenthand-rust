# Reference fill: no product silhouette in the edit reference

## Why grey failed

The `source_edit` plate sends Muse Image ONE image: the original photo at the edit size (`meta::closest_image_size`, 1536x1024 for the 4:3 bike photo, a plain stretch, not a letterbox), with every visible product pixel's footprint plus an 8 px dilation removed. Until now the hole was filled flat `[128, 128, 128]` (`staging::masked_reference`).

On the IMG_1443 hero run (2026-11-01), Muse read that grey bike-shaped silhouette as a dark bike-shaped SHADOW and painted it onto the pavers and fence. The old clause ("Keep the object's cast shadow on the ground as in the original photo...") made it worse. Three effects followed:
- The composite covers most of the painted shape with the real bike, but the 8 px dilation ring leaves a dark halo.
- The "shadow" Muse painted is the bike's footprint, not a cast shadow.
- Any restyle prompt (renovated patio, shop corner) is dominated by the silhouette.

The reference must carry no silhouette.

## What the fill is now (`inpaint_diffusion`, default)

`staging::edit_reference(source, foreground, w, h, fill)` builds the reference in five steps:
1. The source is resized to (w, h) with Lanczos, exactly as before.
2. The hole is the same footprint as before: alpha > 0, mapped to the reference grid and dilated 8 px. `hole_pixels` is unchanged (434,072 on IMG_1443).
3. Hole values are zeroed, then filled ONLY from reference pixels outside the hole, using a coarse-to-fine harmonic (Laplace) fill:
   - A pyramid is built from known-weighted 2x2 means down to a long side of at most 8 px.
   - The coarsest level starts from the mean of its known pixels.
   - Each finer level starts its unknowns from the bilinear upsample of the level below.
   - Every level runs over-relaxed Gauss-Seidel (omega 1.5, 4-neighbour mean, known pixels fixed, image borders reflecting). It runs at most 200 sweeps per level and stops early once no hole value moves more than 0.02 (0-255 units) in a sweep.
4. Texture is added on top: the surround's fine grain (pixel minus its known-only 5x5 box mean), clamped to +-10, is mirrored into each hole row from both ends of the run (x -> 2a-1-x and 2b-1-x, used only where that pixel is known), cross-faded by distance and variance-normalised, at gain 1.0. Mirroring keeps the grain continuous at the hole edge and on the same image row, which is the same depth on a ground plane.
5. The pixels outside the hole are exactly the Lanczos-resized photo, byte for byte. Only the hole changes.

The fill is deterministic: the same input gives the same bytes.

Why "no product pixel was sent" stays literally true:
- The fill never reads a hole pixel.
- The Lanczos support is 3 output px when downscaling, and every pixel outside the hole is at least 8 px from any product pixel.
- `tests/reference_fill.rs` proves it on synthetic images (same size, 2x down, a stretch) and on the real IMG_1443 run. Two sources that differ only in their product pixels (the real bike vs flat magenta) give bit-identical references.
- Bound: an upscale of more than ~2.6x (a source under ~580 px on the long side sent at 1536) widens the Lanczos support past the 8 px dilation. That case existed with the grey fill too.

Edge case: a footprint that covers the whole frame has nothing outside it to fill from. The hole is then neutral grey and `fill_iterations` is 0.

Cost: 1536x1024 on IMG_1443 takes 397 sweeps over 8 levels, ~2.5 s in a release build. The full 5712x4284 size takes 698 sweeps and ~19 s; it is only built by the fixture example.

## What it does NOT do (failure modes)

- It is a smooth fill with faint grain, not an exemplar or texture synthesis. The hole reads as a soft, blurred patch whose tone follows its boundary. On IMG_1443 the wheel discs fill dark, because the bike's real cast shadow wraps them on the left and top. The frame tubes fill light tan from the wall course. A soft bike-shaped ghost is therefore still visible, but there are no hard edges and no grey.
- Whether Muse paints that ghost as an object or shadow is an empirical question for the Mac probe (`reference_inpaint_1536x1024.png`).
- A wider texture radius or clamp was tried and rejected (radius 6-12, clamp 30-40). Row mirroring then echoes fence edges and spokes into fan-shaped patterns, which is a silhouette of its own.

## Prompt changes that ride with it

- `meta::SHADOW_CLAUSE` (every `source_edit` prompt, after the angle lock) now cannot point the model at the hole: "The object itself is not in this photo. Keep the existing cast shadow on the ground and wall exactly as it already appears, same direction, length and softness; do not add any new shadow or silhouette."
- The base wording no longer claims a grey region, because none exists. `PLATE_EDIT_OBJECT` replaces the grey sentences with ones of the same length class, including "Fill the area where the object stood with the continuous surrounding surface." It is not appended on top of the old "Fill the grey region..." sentence. `PLATE_EDIT_SURFACE` changes the same way.
- The legacy grey wording is kept verbatim (`PLATE_EDIT_OBJECT_GREY`, `PLATE_EDIT_SURFACE_GREY`) and is used only with the grey fill (`meta::plate_edit_prompt_for_fill`).
- Identify is untouched.

## Record (receipt `backdrop.reference`, additive)

| key | inpaint | grey |
|---|---|---|
| `fill` | `"inpaint_diffusion"` | `"grey"` |
| `fill_rgb` | `null` (key kept) | `[128, 128, 128]` |
| `fill_iterations` | total Gauss-Seidel sweeps, all levels | `0` |
| `fill_texture` | `{"kind": "row_mirror_residual", "radius_px": 2, "clamp": 10.0, "gain": 1.0}` | `null` |
| `hole_pixels`, `dilate_px`, `size`, `sha256` | as before | as before |
| `product_region` | "...filled from the photo pixels outside it before upload; no product pixel was sent" | the old grey sentence |

No key was removed and none changed meaning; verification is unaffected.

## Env

`AGENTHAND_REFERENCE_FILL=grey` (or `gray`, any case) restores the flat-grey reference and the legacy prompt wording, byte for byte. The rebuilt IMG_1443 grey reference hashes to the live receipt's `b9c58efc...`. Use it for comparison only. Unset, or any other value, selects `inpaint_diffusion`.

## Fixture

`cargo run --release --example reference_fill -- <run_dir> <out_dir>` builds the outputs from `source.png` + `mask.png`:
- `reference_inpaint.png` at the full source size;
- `reference_inpaint_<WxH>.png` and `reference_grey_<WxH>.png` at the edit size, through the same `edit_reference` call the server makes;
- `reference_compare.jpg` (grey | inpaint).

The `#[ignore]` test `real_bike_fixture_reference` checks the live receipt hash and the leak-free property on the real run (`AGENTHAND_REFILL_RUN` overrides the path).
