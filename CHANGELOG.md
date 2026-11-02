# Changelog

## Unreleased: light direction and thin structure

- **Light read off the source photo** (Daniel's spoke/shadow addendum,
  IMG_1443 driveway draw: golden-hour plate under a flat-daylight bike).
  `staging::estimate_light_direction` reads a cast shadow (a darker,
  hue-neutral region attached to the product's lower half; its
  centroid against the contact point names the side), else a
  left/right brightness gradient across the ground, else nothing.
  Generated plates append the phrase after the angle steer, e.g. "Sun
  low from camera-left; long soft shadows fall to camera-right across
  the ground."; source plates add no clause and instead move the
  contact ellipse away from the light (4% of the product width
  sideways, half its vertical radius in depth;
  `composite::apply_contact_shadow_offset`). Record `light_direction
  {stated, phrase | reason, method, side, shadow_falls, signals}` in the
  draw result beside `angle_lock`.
- **Thin-structure guard.** Triggered by the identify concept
  (bike, chair, rack, ladder, frame, ...; cut_out now keeps the
  accepted phrase in the mask PNG's provenance chunk) or by enclosed
  mask holes of at least 0.5% of the mask bbox. Generated plates get
  the plainness clause; every plate `draw` stages (generated,
  source_edit, source_blur) gets `composite::soften_thin_holes`: when
  the plate inside the holes is at least 1.5x as textured as the ring
  around the product, up to 3 steps of blur plus a 25% lift toward the
  ring tone, plate pixels outside the product only. Record
  `thin_structure {triggered, trigger, holes, holes_softened,
  texture_before, texture_after, ...}`. Kill switch `AGENTHAND_THIN=off`.
- **Generated plates name what must stay out.** With the concept known,
  the scene ends "Empty scene, no <concept>, no <head noun plural>, no
  bicycles or other vehicles, no people, no animals; the product will
  be placed afterwards."; the draw result records `scene_exclusions`.
- **`plate_fitted_pixel_sha256`** (additive receipt field,
  `model_generated` draws only): the cover-fitted plate actually
  composited; `plate_pixel_sha256` still hashes the stored plate.
- Identify instructions and output schema, source_edit prompts, product
  pixels and receipt counts/claim are unchanged. docs/LIGHT-AND-THIN.md
  has thresholds and limits; tests in `tests/thin_light.rs`.

## Unreleased: shadow carry-over

- **The product's own cast shadow comes back** (Daniel's shadow ruling,
  IMG_1443 run: only the contact ellipse survived the plate edit).
  Two layers, both automatic. docs/SHADOW-CARRY.md has the details.
- **Prompt clause (best effort).** Every `source_edit` plate prompt
  appends `meta::SHADOW_CLAUSE` right after the angle lock: "Keep the
  object's cast shadow on the ground as in the original photo, same
  direction, length and softness." The `generated` mode and identify
  are unchanged. The prompt-suffix assertions in `tests/angle_lock.rs`
  and the `meta.rs` unit tests now expect the clause after the lock.
- **Deterministic carry-over (the guarantee).**
  `composite::carry_source_shadow` reads a shadow matte from the
  ORIGINAL photo against the finished plate and multiplies it onto
  the plate:
  - plain luma ratio up to 0.92 from the region's floor (0.25
    attached, 0.35 ground band), similar chroma;
  - read on a 4 px grid, outside the dilated product mask;
  - connected to the dilated mask (v2: any side) or the band under
    the product;
  - coherent: 75% of the region within +-0.15 of its median;
  - sized: 0.2% of the image up to 35% of the non-product area.
  Rims of shadows deeper than the region's floor are not carried on
  their own, so no half-dark blob is left beside an uncarried core. The
  feather is the source shadow's own edge ramp (capped at 12 px,
  never wider). Order on `source_edit` / `source_blur` draws:
  plate -> carried shadow -> edge fix -> product.
- **v2 (coordinator's ruling for Daniel).**
  - Regions may attach to the dilated product mask on ANY side. Their ratio floor is 0.25; ground-band-only regions keep 0.35.
  - Coherence and the rim rule are judged per region.
  - The record lists `regions [{side, pixels, mean_ratio, coherence, floor}]` and `rejected [... reason]`.
- **Bike fixture (IMG_1443), v2.** `carried`: 919,162 px, mean ratio 0.588, coherence 0.860, feather 4 px, product bit-identical.
  - Restored: the wall course behind the frame and seat, the rear-wheel arc, and the ground between the wheels.
  - Not restored: the seat shadow on the fence, rejected by coherence (65.4% < 75%, median 0.366, 158,800 px), and the handlebar and stem shadows on the fence. Those two are detached: they never touch the mask, with a gap of at least ~88 px.
- **Record.** `shadow {mode: carried | contact_only, reason,
  shadow_pixels, mean_ratio, feather_px, coherence, regions, rejected}` in the draw
  result (beside `angle_lock`). It is additive in source-plate
  receipts. Generated, placeholder and code-rendered plates get
  `contact_only` / "generated plate" in the draw result; their
  receipts are unchanged.
- **Kill switch:** `AGENTHAND_SHADOW=off` gives the previous composite
  exactly, with the record `contact_only` / "disabled by
  AGENTHAND_SHADOW".
- Product pixels (alpha 255) are never written, and receipt counts and
  semantics are unchanged. Tests: `tests/composite_shadow.rs`, plus an
  opt-in real-fixture run (ignored).

## Unreleased: angle lock

- **The angle is never asked for.** Every `source_edit` draw ends its
  plate prompt with "Keep the exact same camera angle and perspective
  as the original photo, same horizon position, same framing.", with or
  without caller scene text, object or surface wording. It goes after
  the scene text, so a scene change cannot pull the camera.
- **Stated angle when the geometry supports one.**
  `staging::estimate_shot_angle` reads the product bbox (top edge,
  centre, ground-contact line, aspect, frame fill). It states
  `steep_down` ("shot from standing height looking down at the surface
  at a steep angle") or `low_up` ("shot from a low angle looking
  slightly up") only when every cue agrees. Otherwise it returns
  `eye_level`, unstated, and adds nothing beyond the base clause. The
  draw result and room-run item record carry `angle_lock {bucket,
  clause, stated, reason, signals}`.
- **Legacy `generated` mode** appends steering words from the estimate,
  with eye level named when unsure.
- **Tidy default styling** (matches the Mac tree): an object plate with
  no caller scene text gets `meta::PLATE_EDIT_DEFAULT_STYLE` ("a clean,
  tidy, bright version of the same place: ...") as its styling line;
  surface plates keep the plain wording. The angle-lock clause still
  comes last.
- Identify prompt and schema, product pixels, compositing and receipts
  are unchanged. Tests: `tests/angle_lock.rs`.

## Unreleased: pricing

- **Range plus recommended price.** New `src/comps.rs` loads a
  category comps file (`AGENTHAND_COMPS_FILE`, default
  `<data dir>/pricing-comps.json`), matches an item to a category by
  normalised token overlap with the category's item text, queries and
  comp titles (head-noun gate, minimum score 0.6), and returns
  `pricing`: range [min, max], recommended (median), percentile_band
  [p25, p75], confidence, comps_count, searched_at, area, basis,
  advisory and a footnote. No match, or a missing or malformed file,
  returns `{"status": "no_comps"}` (with a one-line note for file
  problems), never an error.
- **Where it shows.** Ok identify results gain `pricing` (computed
  after identify returns; the identify prompt, model schema and MCP
  definition are byte-identical). Room-run item records gain
  `listing.pricing`. No separate tool: the interim `price` tool was
  removed (owner's ruling), so tools/list is exactly identify,
  cut_out, draw.
- **Local first, then wider area.** Every priced block carries
  `scope`. Local comps (`categories`) always win; a category with no
  local comps uses its wider-area tier (a nested `wide` object, or the
  same key in a top-level `categories_wide` list) and the block says
  `scope: "wide"` with the note "No local items found; priced from
  wider-area listings." (and a footnote that does not say "local").
  Neither tier: `{"status": "no_comps"}` as before.
- Real comps file shipped as `tests/fixtures/pricing-comps.json`;
  tests in `tests/comps_pricing.rs`, `tests/server_e2e.rs`,
  `tests/workflow_completion.rs`.

## 0.3.0 — M6 packaging

The submission build. Everything below is on the consolidate master;
the last two fix lanes landed before packaging (`aa62452`,
`be73bd0`) and are re-verified in the final packaging run.

- **The product is a room.** The `room-run` command walks a whole
  room: a seller's photos, videos, and narration become one staged,
  receipted listing per named item, with per-item listing copy,
  backdrop stage modes (`scene`, `wallpaper`, `studio_gray`), and a
  priced table (`priced-table.png` / `.html` / `pricing-notes.md`)
  when the calling agent supplies market comps. The seller workflow
  passed a three-round acceptance on real room material in both
  builds: 8 of 9 items staged (the one failure is a narration mention
  the photos could not support, refused honestly as NO_MATCH), 0
  changed pixels on every receipt, pricing identical across builds.
- **It reads photos.** The Rust build decodes HEIC itself (no
  decoding library), and on every real camera photo tested (iPhone,
  Canon, Samsung originals) its output matched the reference decoders
  pixel for pixel. The from-scratch decoder is integrated into the
  shipped server: ingest sniffs the format from the file bytes, grid
  and single-picture primaries decode in-process, rotation is
  applied, and the file's colour profile is carried through to the
  receipt. Still photos outside the decoder's subset (for example a
  monochrome primary) refuse with the subset's own message; a refused
  auxiliary item never blocks the photo itself.
- **Hardened ingest.** A malformed-input pass over the decoder found
  and fixed three defects (an allocation wrap, a table index panic,
  a parser hang); after the fixes, 1,520 mutations produced 0 panics,
  0 hangs, 0 aborts, and a decoder fault is contained at the ingest
  boundary as an ordinary error. Ingest caps: 20 MB and 40 million
  pixels by default.
- **Colour conversion gate.** The decoder's RGB output was measured
  against the reference converter under a criterion pinned before the
  run: all six non-sensitive corpus files pass.
- **Receipts carry more.** Receipts are stamped `receipt_schema: 2`;
  draw receipts record the plate kind and backdrop parameters, and
  both builds' verifiers accept old and new receipts while refusing
  current-schema fields under a missing marker.
- **Oversize originals handled.** A full-resolution phone photo
  (24 MP) identifies, but the segmentation provider rejects it for
  cut_out (payload limit, both builds). The fix (landed `be73bd0`)
  shrinks only the copy sent for segmentation, maps the mask back
  onto the full-resolution foreground, and records the mask's
  provenance in the receipt; live proof at 24 MP passed.
- **Pre-decode budget enforced.** The pixel budget is enforced from
  the photo's declared dimensions before a full decode begins, and
  the panic boundary covers the whole decode path (landed
  `aa62452`).

## 0.2.0 — 2026-10-28

First Rust release: the three tools (identify, cut_out, draw with
Fidelity Lock receipts) as one static binary with MCPB bundles for
Linux and macOS, verified from a clean install against the frozen
proof run (580,613 opaque pixels checked, 0 changed, $0.020044).
HEIC inputs were refused; the Python build was the HEIC path.
