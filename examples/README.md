# examples/ — internal harnesses, not product examples

These are **not** usage examples of the agenthand product, and none of
them is part of the demo. They are internal test and diagnostic
harnesses for the build's own verification campaigns:

- `heic_decode_one.rs`, `heic_decode_server.rs` — single-shot decode
  probes used by the mutation campaign and by
  `scripts/clean-install-verify.sh`.
- `heic_png.rs`, `heic_time.rs`, `heic_vui.rs`, `gate5b.rs` — HEIC
  decoder diagnostics (output dumps, timing/RSS, colour-fact and
  differential checks against ffmpeg).
- `reference_fill.rs` — fixture builder for the shipped reference-fill
  feature; see `docs/REFERENCE-FILL.md`.

The product is the MCP server in `src/` (tools: identify, cut_out,
draw), installed per the top-level README. Nothing in this folder adds
a tool or a user-facing capability.
