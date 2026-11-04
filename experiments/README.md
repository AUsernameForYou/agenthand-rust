# Experiments — NOT the shipped product

**The product is the Rust crate at the repository root** (the `agenthand`
MCP server in `src/`). Nothing in this folder is built, shipped, or part
of the demo. Nothing here is installed by the setup instructions, and no
test in the shipped suites depends on it.

This folder quarantines working material from the build so it cannot be
mistaken for the product:

- `consol-fix/`, `pkg-fix/`, `python-fix/` — asset-store snapshots
  (ledger + run outputs) captured while the Python reference build was
  being consolidated into this repository. Kept for provenance only.
- `SCHEMA4-SAMPLE-PACKAGE/` — a sample package of schema-4 receipts from
  the Python build, with its own `PROVENANCE.md` and `regenerate.py`.
  The live receipt format is specified in `docs/receipt-format.md` and
  the shipped evidence set lives in `evidence/`.

Explored approaches that were **not** shipped (generated seam,
metric-plate gate, scene guard, band-release carving, tilt capture) are
described in the README's "Paths we explored" section. Their code lives
outside this repository, on experiment branches and in the project
archive — not here, and not in the product.
