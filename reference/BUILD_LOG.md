# Build Log

## Provenance

agenthand was created fresh, from an empty directory, inside the Meta Global
AI Developer Hackathon build window (Oct 26 - Nov 4, 2026). It is original
work written during the window for this event. No code was copied from any
earlier project; design ideas documented before the window (tool contracts,
the Fidelity Lock compositing design, pricing research) were planning notes,
and every line of implementation here was written in-window. This repository
starts with a single scaffolding commit so the in-window history is complete
and auditable.

## 2026-10-26 - Project scaffolding

- Created the project skeleton: pyproject (Python 3.12, official MCP Python
  SDK), src layout, README, .gitignore, .env.example (placeholder names only,
  no keys anywhere in the repo).
- MCP server over stdio with the three tools - identify, cut_out, draw -
  registered and stubbed. Stubs return structured "not yet wired" results
  and make no external calls.
- Asset handle system: images stored locally under a data dir, referenced
  as asset://<id>, sha256 recorded per asset at ingest, with an integrity
  re-check the Fidelity Lock receipt will rely on.
- Cost receipts: every tool call appends a JSONL receipt (tool, model,
  tokens, USD estimate, pricing-table version). Pricing table holds the
  verified Meta Model API prices: muse-spark-1.3 $1.25/$4.25 per 1M in/out
  (cached $0.15), muse-wonder-1.0 $4/$20 (cached $0.20), muse-image-1.0
  $0.01/image, sam-3.1 $2.50 per 1,000 images.
- Fail-closed spend cap: TOTAL_SPEND_CAP_USD defaults to $150.00 (the
  hackathon credit amount, the only authorized spend). The gate checks the
  ledger before any billable call and refuses calls that would exceed it;
  a corrupt ledger also refuses, since unknown spend state means no spend.
- Unit tests cover pricing math, receipt logging from stub tool calls, the
  asset store, and the cap refusing a seeded over-cap ledger.
- Installed mcp 2.3.0 (Python SDK v2; the high-level server class is
  MCPServer, renamed from FastMCP in v1) and pytest 9.1.1 into a project
  venv. 23 unit tests pass, and a live stdio smoke test (initialize,
  tools/list, one stub call) confirms the server answers as an MCP server
  and writes its receipt.

## 2026-10-26 - Junction-1 Gauntlet fix list

The scaffold went through the J1 Gauntlet (four model lanes: Grok, GPT,
Claude opus, Claude Fable; verdict GO WITH CHANGES, all four lanes
converging on the same four defects). This change set implements the
adopted fix list. Still zero external API calls: model wiring waits for
the hackathon credits to land.

- Billing choke point: new module `meta_client.py`. Every billable call
  goes through `billed_call`, which reserves the projected worst-case
  cost as a pending ledger receipt under an fcntl file lock, runs the
  call, then settles with the actual cost priced from the provider's
  usage. A call that raises leaves its reservation counted. Tools no
  longer touch the ledger directly. The module documents that the future
  SDK client must be built with max_retries=0 so retries cannot bypass
  the gate.
- Ledger hardening: receipts gained phase (final/pending/settled) and
  call_id; totals count settled actuals over their reservations and
  unsettled reservations at projection. Negative or non-finite amounts
  (including JSON NaN lines) fail closed, as do NaN/inf projections and
  caps.
- Pricing contract: unknown models raise instead of estimating $0.
  Cached input tokens are now priced as the cached subset of total input
  (fresh remainder at the fresh rate), fixing a double count under the
  old convention. Receipt usage fields (input/output/cached tokens,
  images) are actually stored now; the scaffold's receipt helper dropped
  them.
- Cap: working default is $120.00 under a $150.00 hard ceiling; the env
  var can only lower it, and unparseable or non-finite values fail
  closed.
- Asset store: the single rewritten assets.json index is replaced by
  per-asset sidecar JSON files written via temp file + os.replace; the
  legacy index is still read as a fallback. Records gained kind constants
  (source_photo / foreground_png / mask_png / background_plate /
  composite), the source_asset_id lineage chain, and a pixel_sha256 over
  decoded RGBA bytes alongside the file hash. verify_integrity checks
  both hashes and raises on mismatch.
- Ingest and egress: new `ingest.py`. A tool's image argument may be an
  asset handle, an http(s) URL, or a base64 data URI; ingest enforces
  byte and pixel caps, applies EXIF orientation, and normalizes to 8-bit
  RGBA PNG. Assets are registered as MCP resources at their own
  asset://<id> URIs, and draw returns an image preview with its result.
- Fidelity Lock as code: new `composite.py`. draw pre-validation refuses
  any asset that is not a foreground_png chained to a source_photo, and
  verifies stored hashes before compositing. The background-plate
  function takes only a prompt and dimensions (a deterministic local
  placeholder until muse-image is wired). The compositor fits the plate
  to the product, never scales the product, and the fidelity receipt
  reports file and pixel hashes for source/foreground/mask, the measured
  changed-opaque-pixel count (must be 0), and the edge-band count. The
  claim wording is now precise throughout: the stored extracted
  foreground reaches the composite unchanged (it is not a claim about
  the source photo's hash).
- Framing: README and docstrings reordered draw-first, identify last as
  the planning step inside the chain. cut_out's concept parameter is now
  optional; omitted, it runs the planning step internally.
- Tests: 66 pass (was 23), covering the unknown-model raise, NaN/inf/negative
  refusals, reservation-on-exception, a two-thread gate race, usage
  round-trip, sidecar and legacy-index reads, ingest caps and EXIF
  normalization, and the composite invariant test (solid-color fake
  plate, known foreground, zero changed opaque pixels). A live stdio
  smoke test exercises tools/list, resource templates, all three tools,
  and reading the composite back through its resource URI.

## 2026-10-26 - identify wired to the Meta Model API (live verification pending key)

- identify now makes the real vision call. `meta_client.py` gained the
  live client path: an OpenAI-compatible client (new `openai` dependency)
  built against https://api.meta.ai/v1 with max_retries=0, so no SDK
  retry can bypass the spend gate. The call runs only as the fn of
  billed_call: the reservation is priced as a worst case (image tokens
  estimated from pixel dimensions at one token per 32x32 patch, plus a
  bounded max_tokens of 1024 at the output price, times two to cover the
  one possible repair call), and settlement is priced from the usage the
  response actually returns — receipts now carry real token counts for
  identify.
- The request asks muse-spark-1.3 for strict structured output
  (json_schema, strict): description, concept (one short noun phrase
  for segmentation), alternates, plus the listing fields from the J1
  framing fix — suggested_title, listing_bullets (3-5), condition_notes.
  The concept/alternates contract is enforced after the call: if the
  only defect is fewer than 2 usable alternates, one text-only repair
  call asks for more phrases (its usage is added to the settlement);
  anything still unusable raises and the reservation stays counted.
- Key handling: the key is read from META_API_KEY (environment, or a
  local .env loaded by config — .env is gitignored, verified; no key
  material exists in the repo). The key check runs BEFORE billed_call:
  with no key configured, identify returns a structured NOT_CONFIGURED
  error and writes nothing to the ledger. An unusable model response
  returns a structured BAD_MODEL_RESPONSE error with the counted cost
  attached. cut_out's internal planning step runs this same identify
  path and degrades to an empty plan (with the error surfaced as
  plan_error) when planning fails.
- Live smoke script at scripts/smoke_identify.py: takes an image path,
  runs identify through the tool layer, prints the structured result
  and the ledger receipt lines. NOT run yet — no API key exists on this
  machine. Live verification of identify waits for the key to be
  provided; everything else about the wiring is covered offline.
- Tests: 76 pass (was 66). New tests/test_identify.py runs the whole
  path against a mocked transport: structured output parses into the
  result shape and settles with non-zero token counts on the receipt;
  a malformed response fails closed with the reservation counted;
  a missing key yields NOT_CONFIGURED with zero ledger writes; an
  over-cap ledger refuses before any API call; the alternates shortfall
  is repaired once (usage summed across both calls) and fails closed
  when the repair also falls short. The two scaffold-era tests that
  encoded identify's stub shape were updated to the wired contract.

## 2026-10-26 - First live identify call: truncation found and fixed (live re-verification pending)

- First live smoke (scripts/smoke_identify.py, real key, real call):
  HTTP 200 from api.meta.ai, but the tool returned BAD_MODEL_RESPONSE —
  "Identify response is not valid JSON: Unterminated string starting
  at line 1 column 461". The response was cut off mid-string at
  max_tokens=1024. The fail-closed path behaved as designed: the
  reservation was written before the call and stayed counted.
- Settlement check against the real ledger (data/ledger.jsonl): the
  smoke call (call_id ff934d79819b4af89fb6725f6f936151) has exactly one
  line, the pending reservation at $0.01323, and no settled line —
  correct for a raised call under reserve-then-settle: no settlement is
  written on failure and ledger_total_usd counts the reservation
  exactly once (total verified at $0.01323; not dropped, not doubled).
- Fixes:
  - max_tokens raised 1024 -> 2048 (IDENTIFY_MAX_TOKENS);
    project_identify_cost_usd prices from the same constant, so the
    worst-case reservation tracks the new bound (6x4 test image:
    $0.01323 -> $0.0188), keeping the x2 repair-call factor.
  - The identify prompt now demands brevity inside the schema fields:
    description at most 40 words, each listing bullet at most 12
    words, condition_notes at most 25 words. Schema unchanged.
  - Raw-response capture: when an identify response body cannot be
    parsed as JSON, the full raw text is saved to
    data/debug/identify-<call_id>.txt (named via a context var set by
    billed_call, so the file correlates with the ledger call) and the
    BAD_MODEL_RESPONSE error result carries debug_path. Raw model text
    is never written to the ledger.
- Test hermeticity: the repo .env now holds a real key for manual
  smokes, and config's dotenv loader was leaking it into
  tests/test_tools.py — its cut_out internal-plan test made real API
  calls during two suite runs in this session (spend reserved in tmp
  ledgers; the real ledger was not touched). The module's autouse
  fixture now deletes META_API_KEY and pins the dotenv loader off, so
  the suite is fully offline again.
- Tests: 77 pass (was 76), including a new offline test with a mocked
  truncated JSON body asserting BAD_MODEL_RESPONSE, the debug file's
  path and contents, no raw text in the ledger, and the reservation
  counted exactly once. Live re-verification pending: re-run
  scripts/smoke_identify.py with the key configured.

## 2026-10-26 - cut_out wired to SAM 3.1 (live verification pending)

- cut_out now calls the real segmentation model (sam-3.1) through the
  billing choke point. The call is priced per processed image
  ($2.50 / 1,000): the reservation for one cut_out invocation is the
  per-image rate times the maximum attempts the recovery loop may make
  (2), and settlement counts one image per attempt actually made.
- Mask parsing uses Meta's OFFICIAL parser package, meta-sam-parser
  0.1.0 (PyPI, Meta Platforms, Inc. — the SAM 3 segmentation protocol
  implementation), added as a dependency. All wire-format knowledge is
  isolated in a new module, sam_masks.py — the marked swap boundary:
  the rest of agenthand consumes decoded full-frame binary masks and
  never sees a payload string. If the parser is ever replaced, that
  module is the only file that changes.
- New module segmentation.py: mask QA and the bounded recovery loop.
  QA rejects a mask that is empty (coverage < 0.1%), ~the whole frame
  (>= 98%), or fragmented (largest connected component < 75% of mask
  pixels). The loop tries the concept phrase, then retries ONCE with
  the first alternate from identify; a SAM zero-match is a normal
  outcome that drives the retry, not an exception. Every attempt is
  traced (phrase, outcome, coverage, components, rejection). Total
  failure returns a structured NO_MATCH or POOR_MASK error with the
  trace and the attempts' spend counted; an unparseable SAM response
  is BAD_MODEL_RESPONSE with the reservation counted.
- On success the foreground is the source pixels through the accepted
  mask (nothing regenerated), stored as kind=foreground_png with
  source_asset_id = the source photo; the mask is stored as
  kind=mask_png on the same chain. The result carries source /
  foreground / mask file + pixel hashes, usage (images), and the
  settled cost. When several mask records come back, the largest is
  the candidate and the record count rides in the trace.
- SAM response contract assumptions, recorded for live verification:
  sam-3.1 is a Responses API model (the official parser is built
  around Responses streams), called via client.responses.create with
  an input_text phrase + input_image data URI; the segmentation
  arrives as special-token text in output_text (one `<0f>` line,
  box-local mask rasters placed at the records' half-open bounds);
  usage is metered as one image per call. If the live contract
  differs, meta_client.segment_image and sam_masks are where the
  correction lands.
- Tests: 95 pass (was 77). New offline suites with mocked transport:
  wire parsing incl. the parser package's published lossless example,
  QA rejections (empty, full-frame, fragmented), foreground pixel
  equality where mask alpha is 255, the recovery path (zero-match then
  accepted alternate), persistent failure with spend counted once per
  attempt, NOT_CONFIGURED with no ledger write, and over-cap refusal
  before any API call. The suite is hermetic against the real key in
  both directions (repo .env pinned off; ambient key overridden).
- scripts/smoke_cut_out.py added (image path + optional concept;
  prints the result and new ledger lines). NOT run: live verification
  of cut_out against the real API is the next step.

## 2026-10-26 - draw wired to Muse Image (live verification pending)

- meta_client.generate_background_plate is the draw plate path: it
  reserves the per-image price ($0.01) through billed_call, calls
  muse-image-1.0, stores the result as a background_plate asset, and
  settles at the actual usage (images=1). The raw call lives in
  _request_plate_image beside the other transports (injectable client,
  built with max_retries=0 like the rest); it accepts b64_json or a
  url payload and normalizes whatever returns to RGBA PNG bytes.
- The caller's scene words are wrapped, verbatim, in a fixed plate
  instruction: an EMPTY scene, a clear unobstructed central surface,
  no product, no people, no text. What comes back is a stage, never a
  picture of a product; the image model never receives product pixels.
- Plate size behavior: the request asks for the supported size closest
  to the product's dimensions from the standard OpenAI-style list
  (1024x1024, 1536x1024, 1024x1536, 1792x1024, 1024x1792; nearest
  aspect ratio, larger plate wins ties). The compositor then fits the
  returned plate to the product's exact dimensions — the plate adapts,
  the product layer is never scaled, per the Fidelity Lock rule
  already in composite.py.
- draw flow: validate (kind, lineage, integrity) before anything is
  billed — unchanged — then reserve + generate the plate, composite
  through the real draw_product, and return the composite handle, an
  image preview, and the fidelity receipt computed from the REAL
  composite. The receipt now also pins the plate (asset handle plus
  file and pixel hashes). If the compositor ever measured changed
  opaque pixels, draw returns a FIDELITY_VIOLATION error instead of
  shipping a false receipt.
- Failure behavior: no key -> NOT_CONFIGURED before billing, no ledger
  write. Plate-call failures return structured errors with the
  reservation counted: IMAGE_REFUSED (explicit safety refusal),
  BAD_IMAGE_RESPONSE (no usable image), IMAGE_CALL_FAILED (transport
  raised). A failed draw never regenerates the product and never
  substitutes the placeholder plate; the placeholder survives ONLY
  behind the explicit AGENTHAND_DRAW_PLACEHOLDER_PLATE dev flag, for
  offline tests and no-key demos.
- API contract assumptions, recorded for live verification:
  muse-image-1.0 is called via client.images.generate (OpenAI-style)
  with model/prompt/size/n and returns data[0].b64_json (a url payload
  is handled as a fallback); the endpoint accepts the standard
  OpenAI-style sizes listed above — if it takes arbitrary WxH sizes,
  closest_image_size's candidate list is the only thing to widen; a
  safety refusal surfaces either as an item-level refusal field, an
  empty data list with a refusal/error on the response, or a raised
  SDK error. If the live contract differs, _request_plate_image and
  _plate_bytes_from_response in meta_client are where the correction
  lands.
- Tests: 106 pass (was 95). New offline suite (test_draw.py, mocked
  images transport): the live plate priced at $0.01 and settled with
  images=1, the wrapped prompt carrying the scene words verbatim, the
  closest-size choice, a mocked plate composited under a known
  foreground with opaque pixels byte-identical, the plate pinned in
  the receipt, the empty/refusal/transport failure shapes with spend
  counted, NOT_CONFIGURED with no ledger write, a non-foreground
  refused before any reservation or API call, the placeholder working
  only under its flag, and an explicit assertion that a mocked draw
  leaves the repo's real ledger byte-identical. The two placeholder
  tests in test_tools.py now opt into the dev flag explicitly.
- scripts/smoke_draw.py added (photo path + scene prompt runs
  cut_out -> draw; --asset runs draw directly on a foreground handle;
  prints results, the fidelity receipt, and new ledger lines). NOT
  run: live verification of draw against the real API is the next
  step.

## J2 Gauntlet must-fixes (2026-10-27) — GO WITH CHANGES, 3 of 3 lanes

The draw-junction Gauntlet (verdict: ~/workspace/meta-hackathon/
gauntlet-j2-draw/VERDICT.md) accepted the plate-only architecture and
demanded the receipt stop grading itself. Landed, all server-side:

- Colour profile is a fidelity property (Claude/Fable's strongest
  attack): ingest normalization, foreground extraction, compositing,
  and asset storage now preserve the source's embedded ICC profile end
  to end; every asset record carries color_profile_sha256; the draw
  receipt pins source/foreground/output profile hashes and
  color_profile_preserved, and a profile break returns
  FIDELITY_VIOLATION from draw like a changed pixel. verify_integrity
  also checks the recorded profile.
- Independent verification (Grok/Claude/Fable): the receipt's counts
  are now measured on the delivered file decoded back from the store
  (count_sample: decoded_delivered_file); composite.
  verify_receipt_from_disk re-derives every number from the stored
  files; the new standalone agenthand-verify CLI (agenthand.verify)
  checks exported source/final/mask files with no store — exact
  equality at fully masked pixels plus profile identity.
- The receipt publishes the excluded set (item 3): edge-band count
  AND maximum width (longest opaque-to-transparent path through the
  band), mask_pixels_total, opaque_fraction, the changed predicate in
  words, pixel_tolerance 0, colour space, and the output file's hashes.
- Adversarial tests (item 5): hostile plate with an alpha ramp, an
  alpha-250 ring published as excluded, the one-pixel negative control
  (recount must return exactly 1), a 1-px checkerboard fixture,
  feathered mask, JPEG re-encode, 1-px shift, plate-over-product swap,
  profile strip with byte-identical pixels, EXIF rotation (existing).
- Mask correctness gate (item 6): cut_out now stores a mask_overlay
  asset (source photo with the mask outline in brand cyan) and returns
  mask_review — bbox, fill, frame-edge contact, warnings (all-four-
  edges and sparse-fill flags). segmentation.mask_iou added; golden-
  fixture IoU tests cover the wrong-object case. identify emits no
  spatial extent, so the runtime gate is mask-internal sanity plus the
  overlay; IoU-vs-golden lives in the suite.
- HEIC ingest (item 8, Fable): pillow-heif wired (declared in
  pyproject, registered in assets) — a real encoded HEIF fixture
  ingests and normalizes like any other photo.
- Tests: 136 pass (was 106). Suite still fully offline.
- Not server-side, left for their owners: item 1 (real product photo
  runs — Daniel's fixture), item 7 (offline replay freeze —
  production), item 9 (copy locks — site/film).

## Architecture review Gauntlet fixes (2026-10-27)

The four-lane architecture review (verdict in
~/workspace/meta-hackathon/gauntlet-archreview/VERDICT.md) came back
3 NOT CLEAN / 1 CLEAN WITH NOTES; the fixes land stage by stage.

- Fix 1 — fidelity admission + mask pin. draw now admits only the
  chain shapes foreground -> source_photo and foreground -> mask ->
  source_photo: GPT and Claude both reproduced draw ->
  cut_out(composite) -> draw returning ok with generated plate
  pixels inside the protected layer and the receipt naming the
  original photo. cut_out refuses any non-source input up front
  (NOT_A_SOURCE_PHOTO, before planning and billing), and storage is
  now a chain — mask parented to the source, foreground parented to
  the mask — so the draw receipt pins the real mask asset (production
  receipts previously said mask_asset: null because mask and
  foreground were siblings). New tests: the production chain through
  the real server tools (receipt mask hashes == stored mask record),
  the composite-handle refusal, and chain-shape refusals in
  composite. Suite: 188 pass.
- Fix 2 — verifier hardening. verify_receipt_from_disk now compares
  the receipt's own source/foreground/output/mask hashes against the
  records of the assets it names (run A's receipt previously verified
  against run B's composite), requires exactly zero changed opaque
  pixels (an honest violation receipt previously verified ok), and
  requires positive opaque coverage. The standalone agenthand-verify
  refuses an empty mask (two different images previously passed with
  0 of 0 pixels checked). Receipts that name a mask asset without
  pinning its hashes (the frozen replay package's) fall back to the
  mask file's record integrity; the replay package still verifies
  PASS under the hardened verifier.
- Fix 3 — fail-closed accounting. A priced call whose provider usage
  is missing/empty used to settle at $0, superseding and erasing its
  reservation (GPT reproduced $0.25 -> $0 counted); billed_call now
  settles such calls at the reserved projection with a usage_missing
  note, and _usage_of raises on a response with no usage object.
  The dry-run gate no longer resets cumulative accounting: config
  gained AGENTHAND_LEDGER_PATH, the gate isolates only its asset
  store and spends against the real ledger (the billing lock now
  lives beside the ledger it guards), and the gate reports per-tool
  costs as ledger deltas — which also fixes its silent under-report
  of cut_out (SAM only, internal identify invisible). The gate's
  mask_review summary read keys review_mask never emits
  (fill/frame_edge_contact); now bbox_fill/touches_frame_edges.
  README's cap claim corrected to what the code does (working cap
  configurable up to the $150 hard ceiling, never past it).
- Fix 4 — phrase-hygiene rewrite flag. Plans now carry
  concept_rewritten_from: when hygiene rewrites, promotes over, or
  replaces the model's concept (a real "computer monitor" product
  became "computer" silently), the plan and the identify/cut_out
  results name the phrase the model actually returned. None when the
  concept passed through intact. The rewrite behavior itself is
  unchanged (it is the plan contract); it is no longer silent.
- Fix 5 — estimator profile vendored; AH-35 envelope refusal. The
  layout profile now lives at tests/fixtures/layout-profile.json
  (verbatim copy of the print profile, with a sync-guard test that
  compares AH-46 against the print profile when both are present),
  so the 28 estimator tests that silently skipped on any machine
  without ~/workspace/meta-hackathon now run on a clean clone. And
  the AH-35 mini refuses below ~190 px across
  (CARD_ENVELOPE_MIN_PX): the architecture review reproduced
  centre errors beyond its quoted ranges at 134-138 px with
  healthy fit statistics, so outside its demonstrated close-up
  envelope (~200 px, clean light) it no longer answers at all.
  The degraded mid-distance scene that used to answer at ±111%
  is pinned as a refusal; the close-up still answers.
