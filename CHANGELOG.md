# Changelog

## 0.3.0 — M6 packaging (release notes skeleton; final at the freeze)

The submission build. Everything below is on the consolidate master;
items marked PENDING land with the last two fix lanes and are verified
again in the final packaging run before this section is final.

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
- **PENDING — oversize originals.** A full-resolution phone photo
  (24 MP) identifies, but the segmentation provider rejects it for
  cut_out (payload limit, both builds). The fix shrinks only the copy
  sent for segmentation, maps the mask back onto the full-resolution
  foreground, and records the mask's provenance in the receipt.
- **PENDING — pre-decode budget.** The pixel budget is enforced from
  the photo's declared dimensions before a full decode begins, and
  the panic boundary covers the whole decode path.

## 0.2.0 — 2026-10-28

First Rust release: the three tools (identify, cut_out, draw with
Fidelity Lock receipts) as one static binary with MCPB bundles for
Linux and macOS, verified from a clean install against the frozen
proof run (580,613 opaque pixels checked, 0 changed, $0.020044).
HEIC inputs were refused; the Python build was the HEIC path.
