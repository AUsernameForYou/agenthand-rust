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
1. Every visible product pixel (alpha > 0) of a copy of the source is set to the constant `[128, 128, 128]`, then that copy is resized to (w, h) with Lanczos. No product value enters the resize at all.
2. The hole is the footprint: alpha > 0, mapped to the reference grid (along an upscaled axis a source pixel marks every reference pixel it overlaps) and dilated by `reference_dilate_px` = max(8, ceil(3 x upscale) + 2) px. A downscaled source (every phone photo) keeps 8 px and the same footprint: `hole_pixels` is unchanged (434,072 on IMG_1443). A 300x225 web photo sent at 1536x1024 (5.1x upscale) gets 18 px.
3. Hole values are zeroed, then filled ONLY from reference pixels outside the hole, using a coarse-to-fine harmonic (Laplace) fill:
   - A pyramid is built from known-weighted 2x2 means down to a long side of at most 8 px.
   - The coarsest level starts from the mean of its known pixels.
   - Each finer level starts its unknowns from the bilinear upsample of the level below.
   - Every level runs over-relaxed Gauss-Seidel (omega 1.5, 4-neighbour mean, known pixels fixed, image borders reflecting). Coarse levels (long side at most 256 px) run at most 200 sweeps and stop early once no hole value moves more than 0.02 (0-255 units) in a sweep. Fine levels (long side over 256) start from the converged level below and run at most 30 sweeps, stopping once no value moves more than 0.25 (`InpaintBudget::DEFAULT`).
4. Texture is added on top: the surround's fine grain (pixel minus its known-only 5x5 box mean), clamped to +-10, is mirrored into each hole row from both ends of the run (x -> 2a-1-x and 2b-1-x, used only where that pixel is known), cross-faded by distance and variance-normalised, at gain 1.0. Mirroring keeps the grain continuous at the hole edge and on the same image row, which is the same depth on a ground plane.
5. The pixels outside the hole are exactly the Lanczos-resized photo, byte for byte (the dilation keeps the constant's resize support inside the hole). Only the hole changes.

The fill is deterministic: the same input gives the same bytes.

Why "no product pixel was sent" stays literally true, at every source size:
- The reference is resized from a copy whose product pixels are already the constant, so no product value can reach it whatever the resize support.
- The fill never reads a hole pixel.
- The dilation covers the Lanczos support: 3 output px when downscaling, 3 x upscale output px when upscaling (edit sizes are at least 1024 px, so small sources ARE upscaled). The constant therefore never reaches the ring the inpaint reads.
- `tests/reference_leak.rs` proves it differentially (two sources that differ only in their alpha > 0 pixels give byte-identical references, both fills) at 300x225, 512x384, 600x450 and 1024x768 (upscaled) and 2856x2142 (downscaled), with centred, edge-touching and frame-filling masks; outside the hole the reference equals the plain resized photo. `tests/reference_fill.rs` does the same on synthetic bikes (same size, 2x down, a stretch) and on the real IMG_1443 run.
- Fixed in the aaa44dc review (defect 1): before, the source was resized with its product pixels in place and the dilation was a flat 8 px. At a 300x225 source 244k reference pixels (max 37/255) depended on product pixels with the inpaint, 8k with the grey fill; at 512x384, 32-45k (max 6); at 600x450, 858 (max 2); at 640 px and up, none.

Edge case: a footprint that covers the whole frame has nothing outside it to fill from. The hole is then neutral grey and `fill_iterations` is 0.

Cost: the fine-level cap (review aaa44dc defect 2) bounds the work at under 44 hole-sized sweeps; it was up to 200 x 1.33. Release build, 2-vCPU VM, `edit_reference` at 1536x1024 including the 24 MP resize:

| IMG_1443 | sweeps | work (x hole) | time | vs uncapped, inside the hole |
|---|---|---|---|---|
| bike, uncapped (before) | 397 | 106.2 | 2.79 s | |
| bike, capped | 228 | 24.2 | 2.27 s | mean abs diff 0.45/255, max 5 |
| review's 90% lawn, uncapped (before) | 841 | 137.5 | 5.72 s | |
| review's 90% lawn, capped | 534 | 25.9 | 2.71 s | mean abs diff 0.29/255, max 9 |

The grey reference costs about 1.6-2.1 s there (the resize), so the lawn's inpaint dropped from about +3-5 s to +0.8 s before the Meta call. `tests/inpaint_budget.rs` pins the bound, a 2.5 s wall clock for a 90% mask (release), and the capped fill against the uncapped one on a bike (`InpaintBudget::CONVERGED`, comparison only). The full 5712x4284 size is only built by the fixture example.

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
| `hole_pixels`, `size`, `sha256` | as before | as before |
| `dilate_px` | the dilation used: 8, wider for upscaled small sources (`reference_dilate_px`) | same |
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
