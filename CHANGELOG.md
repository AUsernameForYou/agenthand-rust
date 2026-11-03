# Changelog

## Unreleased: generated plates by default, with a ground shadow

- **Default plate is `generated`** (decision 135): `draw` without a
  `plate` argument stages the upgraded listing scene.
  `plate: "source_edit"` or `AGENTHAND_DRAW_PLATE=source_edit` keeps the
  photo-based plate. `backdrop::DRAW_PLATE_DEFAULT` names the default;
  the draw tool description and the `plate` schema text follow it.
  identify is untouched.
- **Ground shadow on generated plates** (decision 141,
  docs/GROUND-SHADOW.md). Generated draws floated: the plate's ground
  ran under the tyres with no shadow break. `ground::ground_shadow`
  finds the product's ground line and contact points from the layer's
  alpha (lowest pixel per column, lower convex hull) and multiplies a
  contact shadow and a short soft cast shadow onto plate pixels only.
  The cast direction follows the source light estimate; when it is
  inconclusive the shadow falls straight back (front-lit default).
  Penumbra widens and opacity drops with height; thin parts stay faint.
  Runs in the server's generated path only
  (`composite::draw_product_grounded`); `draw_product`,
  `draw_product_with_plate_kind`, `composite_images` and parity are
  byte-identical.
- **Record** `ground_shadow` (additive, never graded) in the draw result
  and the receipt, including the grounded plate's pixel hash;
  `plate_fitted_pixel_sha256` keeps hashing the fit before the shadow.
- **Kill switch** `AGENTHAND_GROUND_SHADOW=off` (0/false/no): plain
  composite, byte for byte.
- IMG_1443 (stored v102 foreground and plate, offline): two contacts,
  1,118,334 plate pixels shaded in 0.49 s, 5,072,763 product pixels,
  0 changed.

## Unreleased: hole carve completion pass and enclosed-remainder gate

- **Completion pass** (`segmentation::complete_windows`, step 4e in
  docs/HOLE-CARVE.md). The colour-proven carve left shaded background
  inside IMG_1443's wheels as product (rear disc 172,123 px transparent
  against 1,642,455 opaque): pavers in the bike's own shadow read like the
  tyre, so they did not clear the e margin. Once windows are accepted, the
  rest of the frame interior is judged by structure: a second colour model
  sampled inside the frame (ring plus accepted windows against band plus
  proven product) marks the kept structure (rim, tubes, fork, hub,
  chainguard); open cells deeper than the accepted windows reach
  (Euclidean depth floor), framed by look-through structure and linked to
  an accepted window across a spoke, are released whole. Completed windows
  join after the line release and the specks, so spokes, nipples and chain
  plates keep the colour-only status. IMG_1443 (with the guards below):
  528,077 px released; rear r = 760 disc 172,123 -> 494,655 px
  transparent, front 418,185 -> 601,331; product 5,660,280 -> 4,567,052
  px; spokes, tyre, rim, hub, fork, chain, cog and reflectors stay;
  nothing is added to the product and the outer band is untouched. Carve
  time 5.3 s -> 9.5 s on the offline machine (7.7 s without the guards).
- **Kill switch `AGENTHAND_HOLE_COMPLETE=off`** (0/false/no) turns off the
  completion alone; the mask is then the 0e338ba carve with the pale-part
  guard applied (reflector edges kept).
  `AGENTHAND_HOLE_CARVE=off` still turns all carving off.
- **Enclosed-remainder gate.** When the open-frame carve applied and more
  than 10% of the enclosed window area (accepted + completed + linked
  enclosed cells left) stays unreleased, `cut_out` returns
  `HOLE_CARVE_INCOMPLETE` in its usual error shape with the remainder,
  three retake changes (decision 114), the operator override and the
  settled SAM cost; nothing is stored. Measured remainder: 2.83% to 5.56%
  on four IMG_1443 masks and the P12 blue bike frame; colour-only carves
  14.2% to 51.4%. Solid items (barbecue, patio set, jackets, card, cars)
  do not apply the carve and are not gated.
- **`hole_carve` record version 2**: adds `completion {enabled,
  released_pixels, cells_released, depth_floor_px, core_depth_px,
  link_radius_px}` and `gate {applies, pass, accepted_window_pixels,
  completed_window_pixels, remainder_pixels, enclosed_candidate_pixels,
  enclosed_cells, remainder_fraction, max_remainder_fraction}`. Other
  fields keep their meaning.
- **Strip release and strip residue** (review round 3). Kept interior
  bands narrower than 0.37% of the bbox diagonal, cored by a 0.09%
  opening and bordered at least 60% by released windows are released,
  with spokes crossing them bridged back; bands bordered 30% to 60% count
  as gate residue. IMG_1443: 17,123 px of front-wheel paver joints
  released, 0 spoke-line px lost, reflectors and cog unchanged; front
  disc 601,331 -> 618,529 px transparent; gate 7.72% / 7.84%. The wider
  upper joint and the rear shade patches remain (docs/HOLE-CARVE.md
  Limits). Record: completion.strip_pixels, strip_cells,
  spoke_bridge_pixels; gate.strip_pixels, strip_residue_pixels.
- Tests: five new tests in `tests/hole_carve.rs` (shaded wheel
  completed, completion kill switch, gate refusal, reflector and bolt
  kept, dark joint band released).
- **Pocket guard and pale-part guard** (review round 2). The completion
  had released the rear cog between the chain and the dropout (17,640
  px) and the rear reflector's bolt; the colour-only carve already frayed
  both reflectors' pale edges. A completion cell lying at least half in a
  pocket of solid kept parts (structure, dense line work such as a chain,
  the outer band; closing of 0.42% of the bbox diagonal) is no longer
  completed. Compact pale parts the carve mostly kept (reflectors) get
  every carved pixel inside their convex hull back. Both only return
  pixels to the product. IMG_1443: cog 17,640 -> 0 px released;
  reflector pixels released against SAM's mask, rear 10,979 -> 211,
  front 2,469 -> 329 (all on the outline); rear disc 494,655 px
  transparent, front 601,331; gate 5.33% / 5.56%. Record field
  `pale_part_pixels_kept`. The dark paver joint bars in the front wheel
  and the shaded patches in the rear lower right still ship as product,
  and the gate does not count them yet (docs/HOLE-CARVE.md Limits).

## Unreleased: Proof Graph CLI (`agenthand evidence`)

- **New subcommand `agenthand evidence`** over the evidence database
  (SQLite, schema v2: runs, calls, receipts, claims, meta, evidence_map,
  video_chapters, the `nodes` view and the derived `edges` table).
  Read-only (`SQLITE_OPEN_READ_ONLY` + `query_only`), no network, no key.
  Verbs: `list`, `show`, `verify <claim-id> | --all` (alias
  `verify-all`), `trace`, `chapter`, `root`, `graph --format
  mermaid|dot`, `rubric`, `calls [--by-model]`, `tamper`; `--json` on
  each. `parity` and `export` are listed as planned and exit 3.
- `verify --all` re-runs the checks of the evidence layer's
  `verify_db.py` in the same order with the same line text and
  PASS / FAIL / REPORT / SKIP semantics; a receipt file that is not
  present is SKIP. Exit 0 only on `OVERALL: PASS`. Against the
  2026-11-02 `evidence.db` the check lines are byte-identical to
  `python3 verify_db.py` (OVERALL PASS, 29 receipt files not present on
  that machine).
- `root` recomputes the receipts root from the `receipts.file_sha256`
  rows with the importer's construction (sha256 over the sorted hex
  digests, concatenated) and compares it with `meta`; it never prints
  the stored value as the result. 2026-11-02 value:
  `54763f911420b4db2104178b5c2420fb46d8ca7ba1af3907e23e1ecc1917b77e`
  (29 receipts).
- Database location: `--db`, `AGENTHAND_EVIDENCE_DB`,
  `./evidence/evidence.db`, then `evidence/evidence.db` beside the
  binary or in a parent directory. Relative receipt paths resolve
  against `--base-dir`, `AGENTHAND_EVIDENCE_BASE`, or the parent of the
  database's directory (verify_db.py's rule). docs/EVIDENCE.md has the
  verbs, output formats and exit codes; tests in
  `tests/evidence_cli.rs` (one `#[ignore]` test for the real database).
- `export` counts file-backed receipts only (spec ruling M-51): a
  receipt whose row names no file is listed on its own
  `No file by design: receipts 5, 15, 17 (...)` line and no longer makes
  the pack PARTIAL. Included files are re-hashed against
  `receipts.file_sha256`. Verdict line:
  `EXPORT: COMPLETE|PARTIAL|FAIL (I of N file-backed receipt files
  included, M missing[, F not matching the database])`; exit 0 / 4 / 1.
  manifest.json adds `file_backed_receipts`, `receipt_files_included`,
  `no_file_receipt_ids`, `mismatched_receipt_ids` and a per-file
  `matches_db`; `missing_receipt_ids` now lists file-backed receipts
  only. On the real database 29 receipts are file-backed and 5, 15
  and 17 name no file.

## Unreleased: hole carving

- **Open-frame interiors go back to the plate** (Daniel's ruling: the
  product is pixel-exact, the background is variable). SAM returned the
  IMG_1443 bike's wheels as solid discs, so the pavers, wall and spoke
  shadows seen between the spokes shipped as "product" and showed the
  old scene through the wheels on every new plate. `cut_out` now runs
  `segmentation::carve_holes` on the full-frame mask before anything is
  cut, stored or reviewed: candidates deeper than the outer band
  (max(12 px, 0.6% of the bbox diagonal)) whose colour is at least e
  times as likely under a local background model (ring 6-30 px outside
  the mask) as under a local product model (the outer band) are carved
  when they form windows framed by narrow structure; spokes, chain and
  cables (13x13 black/top-hat, contrast >= 28) are protected; a
  component is carved only with >= 2% enclosed background AND either
  >= 10% enclosed candidate area or an open-frame concept word (bicycle,
  bike, wheel, chair, rack, stool, ladder, basket). Inner edges get a
  soft alpha <= 128 for the edge fix. Only alpha-255 pixels are ever
  lowered; nothing is raised or recoloured; the receipt counts the
  carved mask.
- **`hole_carve` record** (additive) in the cut_out result beside
  `mask_review`: `{version, applied, reason, regions, carved_pixels,
  protected_thin_pixels, soft_edge_pixels, speck_pixels,
  released_line_pixels, appendage_pixels_kept, product_pixels_before,
  product_pixels_after, open_frame_keyword, open_frame, outer_band_px,
  product_pixels_written: 0}`.
- Kill switch `AGENTHAND_HOLE_CARVE=off` (0/false/no) returns SAM's
  mapped mask byte-for-byte. IMG_1443: 587,550 px carved in 3 regions,
  114,473 spoke/chain px protected, product 5,660,280 -> 5,072,730 px.
  docs/HOLE-CARVE.md has the algorithm, thresholds and limits; tests in
  `tests/hole_carve.rs`.

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

## Unreleased: reference fill (no silhouette in the edit reference)

- **The edit reference no longer carries a grey product silhouette.**
  - On the IMG_1443 hero run, Muse read the flat-grey bike-shaped hole as a dark bike-shaped shadow and painted it. That left a dark halo in the dilation ring, put a footprint where the cast shadow should be, and let the silhouette dominate restyle prompts.
  - `staging::edit_reference` now fills the dilated footprint (still 8 px) ONLY from pixels outside it. The fill is a coarse-to-fine harmonic fill (over-relaxed Gauss-Seidel per pyramid level) plus the surround's fine grain, mirrored per row and clamped to +-10.
  - Pixels outside the hole are byte-identical to before. docs/REFERENCE-FILL.md has the details.
- **Known limit.** The fill is smooth, not synthesized. A soft, blurred ghost of the bike remains: dark in the wheel discs, where the real cast shadow wraps them. Whether Muse leaves it alone is for the Mac probe to show.
- **Prompt.** `meta::SHADOW_CLAUSE` is now "The object itself is not in this photo. Keep the existing cast shadow on the ground and wall exactly as it already appears, same direction, length and softness; do not add any new shadow or silhouette."
  - The object and surface base wordings no longer claim a grey region. The object wording now contains "Fill the area where the object stood with the continuous surrounding surface.", replacing the grey sentences rather than adding to them.
  - The legacy grey wording is kept verbatim for the grey fill (`meta::plate_edit_prompt_for_fill`). Identify is untouched.
- **Record (additive).** `backdrop.reference` gains `fill` (`inpaint_diffusion` | `grey`), `fill_iterations` and `fill_texture`. `fill_rgb` stays as a key and is `null` for the inpaint. `hole_pixels` is as before. The `product_region` sentence matches the fill.
- **Env.** `AGENTHAND_REFERENCE_FILL=grey` restores the grey reference and its wording byte for byte. The rebuilt IMG_1443 reference hashes to the live receipt's `b9c58efc...`.
- **Tests.**
  - New `tests/reference_fill.rs`:
    - no product-pixel leakage: magenta vs green product pixels give bit-identical references, and only the hole changes;
    - the fill is smooth and continuous at the boundary (mean |d| ~0.4-0.5 levels vs the 3 bound) and converges to the true background;
    - the whole-frame edge case;
    - grey env restore;
    - an `#[ignore]` real IMG_1443 check.
  - The prompt-prefix assertions in `tests/angle_lock.rs` and `tests/staging.rs` follow the new wording. A `meta.rs` unit test covers the fill-aware prompt.
- **Fixture.** `examples/reference_fill.rs` writes the full-size and edit-size references and a grey | inpaint compare.

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
