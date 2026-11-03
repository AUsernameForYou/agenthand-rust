# Changelog

All notable changes to agenthand are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project uses
semantic versioning.

## [Unreleased]

Parity port of the Rust build's 380eff0 changes (angle lock, pricing
rulings, edge fix), plus the two mirrors left over from the Nov 1
merge. The identify prompt, instructions and schema are untouched;
pricing is pure post-processing of the result JSON.

### Added

- Comps pricing (`comps.py`): `identify` results gain a `pricing`
  block computed code-side from a local comps file
  (`AGENTHAND_COMPS_FILE`, default `<data dir>/pricing-comps.json`):
  range, recommended price, percentile band, confidence, comp count,
  and the category matched. Two tiers: local comps first; when a
  category has no local comps, a wider-area tier (nested `wide` or
  top-level `categories_wide`) prices it with `scope: "wide"` and the
  note "No local items found; priced from wider-area listings." A
  missing or malformed file, or no category match, degrades to
  `{"status": "no_comps"}` — never an error, never an invented price.
  Room-run listing records carry the same block. No `price` tool was
  added; the tool set stays identify / cut_out / draw.
- Angle lock (`staging.py`): subject geometry (opaque bbox, coverage,
  thin structure) and a shot-angle estimator (steep_down / eye_level /
  low_up buckets; stated only when the geometry supports it). Every
  source-edit plate prompt built by the module ends with the clause
  "Keep the exact same camera angle and perspective as the original
  photo, same horizon position, same framing." plus the angle
  sentence when one is stated. Live `draw` results record an
  `angle_lock` object (bucket, clause, stated, reason, signals);
  receipts never carry it.
- Edge fix (composite): on `source_edit` / `source_blur` plates the
  composite now un-blends the layer's semi-transparent edge band
  against the local original background, rebuilds a 3 px plate
  contact band (2 px on thin structures) from the product rim colour,
  and feathers the boundary by one ring capped at the source photo's
  own measured edge softness. Fully opaque product pixels are never
  modified; the receipt gains an additive `edge_fix` record.
  `AGENTHAND_EDGE_FIX=off` restores the previous composite exactly.
  The quality verifier reproduces edge-fixed composites.

### Changed

- `extract_foreground` now stores straight (un-premultiplied) colour
  for partial mask pixels, mirroring the Rust build: RGB copied from
  the source, alpha scaled by the mask.
- `_union_masks` added to `sam_masks.py`: the multi-region union with
  the speck floor, mirroring Rust `union_masks`. (Not wired into the
  cut pipeline's ensemble, which classifies components per record
  instead; see the module docstring.)

## [0.1.0] - 2026-10-27

First public release. agenthand is an MCP server (stdio transport) that
gives AI agents three tools for turning a seller's messy product photos
into listing-ready material. The seller never opens it; their agent
calls it.

### Added

- `identify` — reads a photo and plans the listing shot: a structured
  description of the item, the cut-out concept with alternates, a
  suggested title, listing bullets, and condition notes.
- `cut_out` — cuts the product out of its photo and returns a
  transparent PNG behind an `asset://` handle. The mask is
  quality-checked, with one bounded retry on a poor mask, and the
  result includes a mask review (bounding box, fill, frame-edge
  contact, warnings).
- `draw` (Fidelity Lock) — stages the real product into a generated
  scene without regenerating the product. The stored foreground is
  composited untouched onto a generated background plate, and the
  result ships with a fidelity receipt: hashes for the source photo,
  the foreground, and the mask, plus the changed-pixel count measured
  on the delivered file. The receipt also states what the count
  excluded.
- `agenthand-verify` — a standalone checker that re-derives a fidelity
  receipt from the files on disk, and can verify an exported
  source/final/mask triple with no server involved. It passes only on
  exactly zero changed pixels with real coverage.
- One billing choke point for every model call: reserve-then-settle
  accounting against a local ledger, a fail-closed spend cap, and a
  cost receipt per call. A response with no usable usage settles at
  its reserved projection rather than $0, and an unpriced model raises
  instead of estimating.
- Ingest by asset handle, http(s) URL, or base64 data URI, with size
  caps, EXIF orientation applied, and HEIC support. Colour profiles
  are preserved end to end and recorded on the receipt.
- Images chain between tools as `asset://` handles instead of bytes,
  and every asset is readable by the client as an MCP resource at its
  own URI.
