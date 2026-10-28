# agenthand — Rust build

agenthand is an MCP server for AI agents that help people sell things.
The seller never opens it; their agent calls it. It turns messy product
photos into listing-ready images, with a fidelity receipt on every
staged shot.

This repository is the **Rust build**, the public submission build of
agenthand: one static binary, no Python runtime. The Python build
([AUsernameForYou/agenthand](https://github.com/AUsernameForYou/agenthand),
also on PyPI as `agenthand`) remains available as the fallback install
and is the reference implementation this port is measured against.

**Demo film:** _link lands here at the reveal (submissions close
Nov 4, 2026)._
**Verify a staged image in your browser:**
https://ausernameforyou.github.io/agenthand/#fidelity
**Project site:** https://ausernameforyou.github.io/agenthand/

## The problem

A staged listing photo sells the item, and it creates a trust
problem. Generative photo tools rebuild the product from scratch, so
details drift and flaws can quietly vanish. When a buyer says "you
hid the scratch," the seller has no answer. As of October 22, 2026,
most Etsy listings must include at least one unedited photo of the
actual item against a real background; AI-assisted images are
allowed only as supplements and must accurately represent the item.
Staging is fine. Proof is the missing piece.

## The three tools

- **identify** — reads a product photo and returns a description, a
  cut-out-ready concept, and listing copy (title, bullets, condition).
- **cut_out** — segments the product with SAM 3.1 and stores a
  transparent foreground PNG plus mask, with file and pixel hashes
  recorded for lineage.
- **draw** — Fidelity Lock staging. The stored product layer is never
  regenerated: the model generates only the background plate and
  deterministic code composites the original pixels over it. Every
  staged image ships with a receipt, and an independent verifier can
  recount the product pixels from disk and prove none changed.

One operator key, `META_API_KEY`, covers all model calls. The server
starts and lists its tools without one; the key is required when a
tool makes a model call. Every billable call passes a single choke
point with a fail-closed spend cap.

## Built with Meta AI

| Tool | Model | Its job |
|---|---|---|
| identify | Muse Spark 1.3 | Reads the photo, plans the cut, writes the listing copy |
| cut_out | SAM 3.1 | Draws the product mask |
| draw | Muse Image 1.0 | Generates the background plate only |

## Architecture

One binary, two halves: a library crate where all the policy lives,
and a thin `main.rs` with four subcommands (`serve`, `parity`,
`verify`, `measure`). The MCP server is a hand-rolled JSON-RPC stdio
loop; each tool handler is a thin edge that ingests, calls one policy
function, and shapes the result JSON.

| Module | What it owns |
|---|---|
| `server` | The stdio loop and the three tool handlers |
| `meta` | Meta Model API client: identify and draw contracts, bounded retries |
| `sam`, `segmentation` | SAM 3.1 mask wire format; mask quality gates and the recovery loop |
| `composite` | Fidelity Lock compositing and receipt verification |
| `image`, `resample` | Image types; byte-exact Pillow-compatible resampling |
| `ingest`, `assets` | Ingest limits and the on-disk asset store behind `asset://` handles |
| `billing`, `ledger`, `sqlledger` | The single billed-call choke point; two ledger stores (JSONL, SQLite) with identical accounting |
| `pricing`, `config` | The model price table; environment configuration and the spend cap |
| `measure` | Prototype scale-card size estimator (not a fourth tool) |
| `heic` | From-scratch HEIC decoder, in progress |

Three invariants hold everywhere:

- **Fail closed on money.** Every model call goes through
  `billing::billed_call`, which reserves against the spend cap before
  the call and settles after. Unknown models and unidentifiable
  ledger stores are refused, never priced at zero.
- **The product layer is never regenerated.** `draw` composites the
  stored foreground pixels over a generated plate in deterministic
  code, and the receipt lets anyone recount them from disk.
- **Errors are values at the boundary.** Tool handlers return the
  error-shaped JSON the Python reference returns; inside the crate,
  fallible code returns `Result<_, String>`, or a typed error struct
  (`IdentifyError`, `CutOutError`) where the caller must distinguish
  failure kinds.

## Install

Download an MCPB bundle or a bare binary from the
[latest release](https://github.com/AUsernameForYou/agenthand-rust/releases/latest):

- `agenthand-0.2.0-linux-x86_64.mcpb` / `agenthand-0.2.0-linux-x86_64` — Linux x86_64
- `agenthand-0.2.0-macos-arm64.mcpb` / `agenthand-0.2.0-macos-arm64` — macOS, Apple Silicon
- `agenthand-0.2.0-macos-x86_64.mcpb` / `agenthand-0.2.0-macos-x86_64` — macOS, Intel

The `.mcpb` bundles install in one click in MCPB-supporting clients.
For any other MCP client, point it at the bare binary:

```json
{
  "mcpServers": {
    "agenthand": {
      "command": "/path/to/agenthand",
      "env": { "META_API_KEY": "your key" }
    }
  }
}
```

Or register it from the command line — Claude Code:

```sh
claude mcp add agenthand --env META_API_KEY=your-key -- /path/to/agenthand
```

Codex:

```sh
codex mcp add agenthand --env META_API_KEY=your-key -- /path/to/agenthand
```

**macOS note:** the macOS binaries are unsigned and not notarized, so
Gatekeeper will block the first launch of a browser-downloaded copy.
Right-click → Open, or clear the quarantine flag
(`xattr -d com.apple.quarantine <file>`), and it runs normally after
that. Notarization is a planned, paid step.

**HEIC note:** the Rust build does not decode HEIC photos yet — a
from-scratch decoder is in active development in this repo. It refuses
HEIC inputs loudly rather than failing silently. For HEIC today, use
the Python build (`uvx agenthand`), which handles it.

Fallback install (Python, full-featured):

```json
{
  "mcpServers": {
    "agenthand": {
      "command": "uvx",
      "args": ["agenthand"],
      "env": { "META_API_KEY": "your key" }
    }
  }
}
```

## Built twice, measured side by side

agenthand exists twice: the reference server in Python, and this
implementation in Rust. Both were run against the same frozen proof
run and measured side by side. The two builds agree exactly: same
output pixel hash (sha256 8b8defea…e802), same count (580,613 opaque
pixels checked, 0 changed), and the same total billed cost, $0.020044
to the micro-dollar. What differs is how fast the local work runs:

| Operation | Python | Rust |
|---|---|---|
| Cold start to initialize response | 1,291.5 ms | 2.4 ms (about 540x) |
| Verify over the proof images | 569.7 ms | 91.4 ms (about 6.2x) |
| Estimator run | 2,824.4 ms | 1,802.3 ms (about 1.6x) |

Measured on the same machine (a 2 vCPU AMD EPYC VM), medians of
interleaved runs (cold start: median of 8 after 1 warmup discard;
verify and estimator: median of 7, full CLI wall clock). One honest
note: on a real tool call, Meta API latency dominates, so the
commercially real win here is cold start, because every MCP client
spawns the server fresh for each session.

## A real fidelity receipt

Every staged image ships with one of these. This is the actual
receipt from the frozen proof run (the verified live run of
2026-10-26; file sha256 f0f68ec2…93198b), shown in full —
580,613 opaque product pixels checked, 0 changed, and every hash a
verifier needs to recount that from the stored files. What each
field means, and exactly what the verifier recomputes, is specified
in [docs/receipt-format.md](docs/receipt-format.md).

```json
{
  "changed_opaque_pixels": 0,
  "color_profile_output_sha256": null,
  "color_profile_preserved": true,
  "color_profile_source_sha256": null,
  "edge_band_max_width_px": 0,
  "edge_band_pixels": 0,
  "foreground_asset": "asset://b8663615419e40389f4e48dbd0e38698",
  "foreground_pixel_sha256": "99e8890ff5e3dcdf825275d327bf5a28c21c31673fc19ed29549a5aa0fb74100",
  "foreground_sha256": "fba19fe69dbb10f4ee087177b4ae07c6c42440af9b77a0e97316bafba9704adf",
  "mask_asset": "asset://7dc19362be4c47abadac2edd7e69129e",
  "mask_pixels_total": 580613,
  "opaque_fraction": 1.0,
  "opaque_pixels": 580613,
  "output_asset": "asset://45deaf1acea14ab5839c641eee1cd277",
  "output_pixel_sha256": "8b8defea41e94004ddc0affd2abef697f32e20c5cf2fdab4c268236611c9e802",
  "output_sha256": "13b58a327820aa97e9aa306e861b9dd85c199d6a9ef5d53ea7755c3365a35451",
  "plate_asset": "asset://4f1740f4ec3e493cbc380f97e49c7e8f",
  "provenance": {
    "call_ids": {
      "cut_out": "bbdb8433ad30497b8e05d7baae490fcb",
      "draw": "85cb0449d4eb47e2a272525ae04f5302",
      "identify": "0b2f6e0953374858b4a554438944c1f1"
    },
    "freeze_note": "The original run predates the expanded receipt schema (J2 fixes, commit 6571b8b). Count/profile fields below were re-derived from the frozen files with the current code at freeze time and reproduce the originally recorded headline numbers exactly (580,613 checked, 0 changed). The run's own ledger lines are frozen verbatim in run/ledger-lines.jsonl.",
    "originally_recorded": {
      "changed_opaque_pixels": 0,
      "opaque_pixels_checked": 580613
    },
    "run": "verified live end-to-end run, 2026-10-26 23:13:47-23:14:23 UTC",
    "settled_costs_usd": {
      "cut_out": 0.0025,
      "draw": 0.01,
      "identify": 0.007544,
      "total": 0.020044
    }
  },
  "source_asset": "asset://e3f0428fbc5e43c7826959a9eecbda50",
  "source_pixel_sha256": "2e2af4170256ffce7ef4653b3079c50d03c9fd1cbab004c3897d785ec307b7d4",
  "source_sha256": "f3dc3c11e57a837d4dc11f28123dd250ca6cde6fb1e2eda9a88548dc6e7aa78b",
  "transparent_pixels": 1740937
}
```

## Verify a staged image yourself

The binary carries the verifier:

```sh
agenthand verify <source.png> <staged.png> <mask.png>
```

It recounts the product pixels in the staged image against the source
and mask and reports how many changed. The count is the claim.

## Refusals and limits

The refusals are designed, and they are part of the product:

- **HEIC photos** are refused loudly; a from-scratch decoder is in
  active development in this repo. The Python build handles HEIC
  today.
- **A foreground with zero opaque pixels** is refused: there is no
  product layer to protect, so there is nothing to stage.
- **A product layer with generated ancestry** is refused by draw.
  Only a layer that came out of cut_out, untouched, can be staged.
- **The size estimator refuses** below its 130-pixel card floor
  rather than guessing at a dimension.
- **Spend refuses.** Unknown models are refused, never priced at
  zero, and every call must fit the cap before it runs: a $120
  working cap under a $150 hard ceiling.

`META_API_KEY` is a key for the Meta Model API (api.meta.ai); one
operator key covers all three models. The server starts and lists
its tools with no key at all, and the verifier plus the frozen
replay need no key.

## Provenance

This repository was created on October 27, 2026, inside the Meta
Global AI Developer Hackathon build window (Oct 26 – Nov 4, 2026),
and every commit in it was written in-window; the history is the
audit trail. The Rust implementation is a port of the Python
reference implementation, whose dated build log (BUILD_LOG.md in
that repo) records the project's provenance from an empty directory.

## License

Proprietary. © Daniel Wagner. All rights reserved.
