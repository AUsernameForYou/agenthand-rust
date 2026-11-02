# Live demo drill: "stage that one"

One page. Read it before the judged demo. Numbers marked *measured* come from real runs; the rest is what the code does (file:line).

## When a judge names a subject

1. **Before they speak:** the photo is already in: `identify(image)` → `image` handle (`asset://...`) plus listing copy. *Measured 28.6 s on the Mac, 2026-11-01.* Settled cost about $0.009 (live run 2026-10-27).
2. **They name it:** `cut_out(image=<handle>, concept="<their exact words>")`. One SAM 3.1 call, **one attempt**: an explicit concept has no alternates to retry with (server.rs:245-246). Cost $0.0025, billed even on a miss.
3. **Look before you stage:** open `mask_overlay_asset`; read `mask_review` (coverage, `touches_frame_edges`, `warnings`) and `attempts[0]` (`records`, `regions`, `object_ids`, `specks_dropped`).
4. **Stage it:** `draw(asset=<output_asset>, scene_prompt="...")`. Default plate `source_edit`: the original photo with the product removed and filled from its surroundings (product pixels never sent; docs/REFERENCE-FILL.md), edited in place by one Muse Image 1.0 call, then a contact shadow and a 1-2 px matte on plate pixels only. If that call fails, the code plate stands in (`plate_kind: source_blur`, `plate_fallback` says why) and the $0.01 reservation stays counted. `plate: "generated"` (or `AGENTHAND_DRAW_PLATE=generated`) restores the old text-only plate. Every plate prompt ends with the angle lock ("Keep the exact same camera angle and perspective as the original photo, same horizon position, same framing."), plus a shot-angle phrase when the mask geometry states one; `angle_lock` in the result shows the bucket and the exact clause. Returns the receipt, `verify_url`, QR sidecar, label line, `subject`, `angle_lock`. Cost $0.01.
5. **Say the receipt:** "`opaque_pixels` product pixels checked, `changed_opaque_pixels` changed" (should read 0).

Seconds for cut_out and draw are **not measured on the Mac yet**: take them from the first sweep (`cut_out_s`, `draw_s` in `sweep.csv`) and write them here.

**Timeout risk (open):** every Meta request runs under reqwest's default 30 s limit; `meta.rs` `build_client` sets none. identify at 28.6 s has 1.4 s of headroom. If a call fails with "timed out", run it once more, and get the limit raised before demo day.

## What it says for a surface (lawn, patio, gravel)

- One connected region under 98% of the frame: **status ok**. `touches_frame_edges` lists the edges (a lawn: `left, right, bottom`). cut_out adds no warning for size alone. draw reports it: over 45% of the frame, or spanning opposite edges, sets `subject.fills_frame`; the edit prompt then continues the surroundings, no contact shadow is drawn, and `subject_note` reads "subject is a surface or fills the frame; staging kept its pixels and replaced only what was around it". One edge, or two adjacent edges, is cropping (a bike cut at the bottom): object wording, shadow skipped only at the bottom edge.
- Touches all four edges: ok, plus warning "mask touches all four frame edges; this usually means the background was selected instead of the product".
- Fills under a quarter of its box (fence, pickets): ok, plus warning "...scattered or frame-like, review the overlay before listing".
- 98% or more of the frame: **POOR_MASK**, `rejected: full_frame`.
- Broken into pieces (lawn cut by a path; the N largest pieces, N = kept SAM records, together under 75% of the mask): **POOR_MASK**, `rejected: fragmented`, with the component count in `attempts`.
- Many separate things (stones, fence posts): SAM can return one record per item, and **cut_out unions every record of at least 0.1% of the frame** (`segmentation::union_masks`); smaller records are dropped as specks. `attempts[0].regions` and `specks_dropped` say what it kept. Say: "it kept all N pieces."
- On draw, a surface covers most of the frame, so most of the staged image is the original photo; the plate shows only around it.

## What it says when SAM finds nothing

- `status: error`, `error.code: NO_MATCH`: "No usable mask after 1 attempt(s); final outcome: NO_MATCH. Name the object differently, or retake the photo so the object is clearly visible."
- A speck under 0.1% of the frame reads as POOR_MASK `rejected: empty` (wire code unchanged); the error text says "too small (N px, under 0.1% of the frame)". A mask with no pixels says "empty (SAM selected no pixels)". Say "too small to stage", not "found nothing".
- draw refuses a cut-out with no fully opaque pixel (`EMPTY_FOREGROUND`) before any spend: there is never a plate with nothing on it.
- Move: rephrase once, more specific ("grey stepping stone", "wooden fence post"), then move on.

## If a call is refused (IMAGE_REFUSED, spend cap, FIDELITY_VIOLATION)

> "That call was refused, and agenthand shows you the refusal instead of a result it can't back with a receipt; let me rephrase and run it once more."

## The three commands

Run from the `agenthand-rust` folder, `META_API_KEY` exported (never printed). Set once: `SHOOT=<folder holding IMG_1358.HEIC>`.

```sh
# 1. Demo morning: handshake, tool schemas, key present, photos found. Spends nothing.
python3 scripts/qc_sweep.py --dry-run --photos "$SHOOT"/IMG_1358.HEIC "$SHOOT"/IMG_1361.HEIC "$SHOOT"/IMG_1365.HEIC

# 2. Day before: full sweep, 3 photos x 8 subjects (lawn ... kids bike). Capped at $2 (expect well under $1).
python3 scripts/qc_sweep.py --out ~/qc/full --photos "$SHOOT"/IMG_1358.HEIC "$SHOOT"/IMG_1361.HEIC "$SHOOT"/IMG_1365.HEIC

# 3. Review: one row per pair (coverage %, regions, bbox, px checked/changed, cost and seconds per call, status, notes).
open ~/qc/full/sweep.csv ~/qc/full
```

Per subject, `~/qc/full/<subject>/` holds `-mask.png`, `-overlay.png`, `-cutout.png`, `-staged.png` and the raw JSON. A row noting "RECEIPT DISAGREES WITH RECOUNT" is a stop-ship.
