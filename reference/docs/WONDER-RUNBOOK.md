# Wonder pipeline validation

Owner: Codex owns local builds, test completion, results and repairs on `wonder-fix`.
Coordinator owns pushing. No push, deployment or release is part of this build.

Before each changed/scoped run, collect the exact node IDs with the same Python
interpreter, paths and selection. Confirm expected parameter cases are present.
Then execute that scope and verify nonzero executed counts; collection alone is
not execution. Save command, source identity and result under
`~/wonder-audit/gpt-evidence/`. On failure preserve the log, diagnose the actual
stage, and rerun the demonstrated gap. On pass advance to the next phase and run
the full suite before its checkpoint. Never count skipped cases as passes.

Order: evidence/grading and app logic unit tests, mocked tool/store/billing
integration checks, fixture image checks, then full regression. This is a Python
MCP service with no native UI or deployed backend in this scope; native device,
accessibility and minimum mobile OS tests do not apply. Fake API tests prove
local orchestration, not live provider segmentation or photographic quality.

Baseline source: `ff72fcbfc1a926f84aa828c771d882a9ff078b04`.
Baseline: 337 collected; 336 passed, 1 skipped (52.57 s).
Retain original logs; do not carry a checked status to another source revision.

Quality policy constants are versioned heuristics. The two supplied hero masks
are failure fixtures. Existing synthetic accepted fixture controls are the
available pass fixtures; no labelled multi-category photo corpus or hero source
photos were supplied. A geometry pass is not human acceptance of a listing.

Fidelity rules remain alpha 255, tolerance 0, no foreground scaling; paper is
separate, only at alpha 0 with a one pixel guard. Identify prompt/schema must
remain identical to the baseline. New schema 4 verification is Python only.

## Current contracts and remaining validation

- Normal cut acquisition uses the identify concept and both alternates, with
  source-frame top/support bands (up to eight SAM requests). Small controls
  below 128 px on either side use only global passes. Huge crop requests may
  still require a disclosed downscale to fit the proven SAM cap. All decoded
  records, including rejected attempts, are stored with phrase/tile metadata.
  Legacy `mask_sam_*` fields describe the global pass; the mask's
  `agenthand_cut_pipeline` PNG text contains the full acquisition graph.
- A passing or provably repaired cut is required before any plate purchase.
  Unbounded dark background is not proof of a missing support. A top concavity
  must be bounded by product on both sides; ordinary silhouette taper does not
  establish top occlusion. No threshold is selected per photo.
- Supports use the measured cylindrical chart. Top footprints require foreign
  evidence and product flanks. The top paper chart remains an image-plane chart
  with clamped source shading; arbitrary perspective-correct surface wrapping
  and the owner's phone-size visual acceptance remain unproved.
- Plate requests use `StageSpec` version 1 and one template, with at most two
  separately billed attempts. Repeated measurable rejection selects studio
  gray and records both rejected plates, their hashes, quality and call IDs.
  The camera class is fixed; source-camera matching is not proved by this
  heuristic. A plate PASS is not a photographic-believability claim.
- New receipts use schema 4. Required cut-quality, reconstruction and plate
  disclosures are independently recomputed from stored files. Verification
  re-renders attempted paper without writing assets, checks the guard/cap and
  region map, and reproduces all three layers even for local placeholders.
  Downgrade guards reject schema-4 fields under an older marker. Historical
  unmarked v1 and v2/v3 receipts retain their policies. No Rust schema-4 parity.

Full stored-receipt check:

```sh
.venv/bin/python -m agenthand.verify --data-dir /path/to/data --asset asset://COMPOSITE_ID
```

Exported-file preservation check (also reports current source/mask grade):

```sh
.venv/bin/python -m agenthand.verify SOURCE.png FINAL.png MASK.png
```

The exported-file check is not full schema-4 receipt verification. Keep its
pixel-preservation result separate from the quality disclosure.

The retained `docs/proof-source-v1.png` + `proof-mask-v1.png` is an explicit pass
fixture alongside the synthetic controls. Hero masks remain explicit failure
fixtures. Do not claim multi-category calibration, live recovery of the hero
legs, source-camera matching or approved 3D wrapping from this offline suite.
The identify instructions/schema hash remains
`c7b529289c168126cc9f033024ccdb3c699a036f6b7075ab20856d678fc8b2bd`.

## FIX2 policy and validation

The FIX2 owner remains Codex. Work stays local on `wonder-fix`; no push or merge.
For every new or changed selection, collect the exact pytest node IDs first,
including parameter cases, with `.venv/bin/python`. Require the same nonzero
executed membership and preserve the command, source hashes and raw result.
Evidence for this round lives in `~/wonder-audit/fix2-evidence/`. Consume each
local completion: on pass advance the authorized dependency, on failure retain
its log and rerun the demonstrated gap after diagnosis, and on a scope blocker
record the real dependency. A green collection is never an executed pass.

Current grading policy is `agenthand.cut-quality/5`. Enclosed openings and top
concavities need product-like source pixels or overlapping, appearance-distinct
foreign evidence before they are defects or paper candidates. Otherwise they
remain open geometry. `TOP_PERFORATED` covers only the top 35% of body height.
Product-query top fragments have the non-authorizing `top_band_candidate` role.
Source appearance is a fixed heuristic, not semantic ownership or calibration
on a photographic corpus. The five opening controls must PASS with zero paper.

Supports use body-scaled height and floor contrast in both brightness directions
and chroma. Wide contact shadow within the body envelope is insufficient alone;
ambiguous support needs bounded extension or a narrow vertical stem. Broad
background reaching the search-band boundary is insufficient. Keep the original
wide-band shadow as a PASS control and the stem fused into shadow as RESHOOT.

Plate policy `agenthand.plate-quality/2` checks continuity below placement too.
A `WALL_FLOOR_LINE_IN_PLACEMENT` failure stops further purchases and immediately
selects the disclosed `placement_horizon_mismatch` studio fallback. Other plate
failures retain the two-attempt limit. The original v3 plate passes at compatible
positions; high product positions can require that fallback. Check bottom
positions 0.50, 0.55, 0.60, 0.65 and 0.70: podium rejected at each, selected stage
PASS at each, and no second purchase after a horizon-placement failure.

New composite PNGs carry schema and cut-policy stamps inside their hash-pinned
bytes. Legacy policies reject a v4-stamped output even when all receipt fields
are stripped and no persisted receipt remains. Genuine legacy files retain
legacy policy. Schema-2 fixtures are byte-exact from the retained Rust data
store. The schema-3 fixture is an unmodified offline replay from archived Python
producer `ff72fcb`; no previously retained schema-3 run was found. It is not a
stripped v4 receipt. Keep the original docs v1 receipt and assets byte-exact.

Exported verification now fails RESHOOT, reports fidelity separately, and
regrades supplied paper with the same seam bridge as preflight. Guard/cap checks
still apply; this mode remains narrower than full stored-receipt reproduction.
Decoded cut failures settle their completed image count; unknown call failures
retain the reservation. Plate error totals include settled attempts plus the
open reservation. Acquisition policy `agenthand.cut-pipeline/3` uses the selected
body phrase for tiles and discloses unique omitted pixels from that global
record in mask review and mask provenance. Restoring detached product parts
still requires ownership evidence; this round adds disclosure only.

Run affected grading/logic tests, then tool/store/billing integration and fixture
checks, then the full final suite. Backend checks here are local and mocked;
there is no deployment or native UI in scope. No live Meta call is needed.
The optional structured RESHOOT tool response remains deferred; the existing
exception contract and physical instructions are preserved.
