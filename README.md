![agenthand](docs/logo-r-v1.png)

# agenthand — Rust build

[![CI](https://github.com/AUsernameForYou/agenthand-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/AUsernameForYou/agenthand-rust/actions/workflows/ci.yml)

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

**Submission freeze:** the submitted build is the one tagged
`v1.0.0-hackathon` in this repository. If you are reading this on a
later `main`, the tag is the exact code the judges were given.

## For the judges

This README is laid out in the order of the judging rubric. Each
section leads with the strongest claim we can prove for that
criterion, then shows the proof: a receipt, a command, or a frozen
run you can re-check yourself.

| Criterion | Section | The claim in one line |
|---|---|---|
| Impact (30%) | [Impact](#impact) | A real room run staged six real items: 352,597 product pixels checked, 0 changed |
| Use of Meta AI tooling (25%) | [Use of Meta AI tooling](#use-of-meta-ai-tooling) | Every runtime model is a Meta model, and the open-weight one does real work |
| Demo (25%) | [Demo](#demo) | Every moment of the demo film traces to a claim in the Proof Graph |
| Technical Depth and Execution (20%) | [Technical Depth and Execution](#technical-depth-and-execution) | Two builds ran one frozen proof run and agreed exactly: 580,613 pixels checked, 0 changed. Architecture chosen on measured evidence: see "Paths we explored" under Technical Depth and Execution |

To run it, go to [Install](#install). To check the claims with no
API key at all, the frozen replay in `conformance/replay-package/`
re-proves the headline receipt offline, and
`agenthand evidence verify --all` re-checks every claim in the
Proof Graph.

## Install

Download an MCPB bundle or a bare binary from the
[latest release](https://github.com/AUsernameForYou/agenthand-rust/releases/latest):

- `agenthand-0.3.0-linux-x86_64.mcpb` / `agenthand-0.3.0-linux-x86_64` — Linux x86_64
- `agenthand-0.3.0-macos-arm64.mcpb` / `agenthand-0.3.0-macos-arm64` — macOS, Apple Silicon
- `agenthand-0.3.0-macos-x86_64.mcpb` / `agenthand-0.3.0-macos-x86_64` — macOS, Intel

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

**HEIC note:** the Rust build decodes HEIC photos itself (see "It
reads photos" below). Photos outside the decoder's subset refuse
loudly rather than failing silently; the Python build (`uvx
agenthand`) remains the fallback install.

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

**Build from source** (what the judges get): with a Rust toolchain,
`cargo build --release` in this repo produces `target/release/agenthand`;
`cargo test --release` runs the full suite. Keys: the live tools need
your own Meta Model API key (`META_API_KEY`; see `.env.example`).
Room-run narration calls Muse Spark Open on Baseten through the host's
credential helper; without one it records a clear error and returns no
transcript. The evidence verbs and the conformance replay below need
no keys at all.

## Impact

**Staged photos a buyer can check, not just trust.** On
October 28, 2026, a room run over a real room staged six distinct
items with 352,597 product pixels checked across them and 0
changed. Every staged image carries a fidelity receipt, so the
claim "the product itself was never touched" is a count anyone can
recount from the stored files.

### The problem

A staged listing photo sells the item, and it creates a trust
problem. Generative photo tools rebuild the product from scratch, so
details drift and flaws can quietly vanish. When a buyer says "you
hid the scratch," the seller has no answer. As of October 22, 2026,
most Etsy listings must include at least one unedited photo of the
actual item against a real background; AI-assisted images are
allowed only as supplements and must accurately represent the item.
Staging is fine. Proof is the missing piece.

### The three tools

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

**Pricing.** An ok identify result also carries a `pricing` block,
computed in code after identify returns (the identify prompt and
schema are unchanged): a range of possible asking prices, a
recommended price (the comps median), the 25th to 75th percentile
band and a confidence, from the comps category the item matches in a
local comps file (`AGENTHAND_COMPS_FILE`, default
`pricing-comps.json` in the data dir). There is no separate pricing
tool. Local comps price first (`scope: "local"`); a category with no
local comps uses its wider-area tier and says so (`scope: "wide"`,
"No local items found; priced from wider-area listings."). Prices are
asking prices from Facebook Marketplace comps and are advisory; an
item with no matching category, or no comps file, returns
`{"status": "no_comps"}`.

One operator key, `META_API_KEY`, covers all model calls. The server
starts and lists its tools without one; the key is required when a
tool makes a model call. Every billable call passes a single choke
point with a fail-closed spend cap.

### The room run

The three tools also compose into a room run (`agenthand room-run`):
the seller walks a room with photos, video, or a narrated
walkthrough, names what goes, and gets one staged listing per item,
each with its own Fidelity Lock receipt, its own listing copy, and a
backdrop chosen per run (a generated scene, a white wallpaper plate
with the agenthand mark, or studio gray). A mention the photos
cannot support fails honestly as NO_MATCH instead of becoming a
listing. When the calling agent supplies market comps, the run also
emits a priced table (suggested prices are recommendations, never
orders). On October 28, 2026, a room run over Daniel's real room
staged six distinct items with 352,597 product pixels checked across
them and 0 changed, for $0.154487 in settled Meta calls (including a same-day re-cut of one hat).

## Use of Meta AI tooling

**Every model that runs at runtime is a Meta model.** Three hosted
models do the staging work under one operator key, and a fourth,
the open-weight Muse Spark Open, reads narrated walkthroughs; its
intent split is what drives the room run.

| Tool | Model | Its job |
|---|---|---|
| identify | Muse Spark 1.3 | Reads the photo, plans the cut, writes the listing copy |
| cut_out | SAM 3.1 | Draws the product mask |
| draw | Muse Image 1.0 | Generates the background plate only |

### Open weight: Muse Spark Open on Baseten

Narration runs on an open-weight Meta model. When a room run is
driven by a narrated walkthrough, the audio goes to **Muse Spark
Open** (`meta-models/muse-spark-open-1026`) served through Baseten
partner inference, which returns the transcript and the sale intent
(which items are for sale, which are only mentioned) in one call.
That split drives the run: named items stage, mentioned items stage
only when the photos confirm them, and unsupported mentions fail as
NO_MATCH. The narration runs on Baseten partner credits, a pool
separate from the Meta Model API spend cap; the three tools
themselves run on the hosted Meta models in the table above.

**The product is never regenerated, and the receipt is the
product.** Generative staging tools rebuild the item and ask the
seller to trust the result. agenthand generates only the room
around the item, composites the original pixels over it in
deterministic code, and ships a receipt that lets anyone recount
those pixels from the stored files. The evidence is engineered as
a graph (the Proof Graph, under Technical Depth and Execution):
every claim a node, every receipt the proof at the end of a
traceable path.

![A staged image from the frozen proof run](conformance/replay-package/run/assets/45deaf1acea14ab5839c641eee1cd277.png)

The staged output of the frozen proof run (2026-10-26). Its
receipt appears in full under Technical Depth and Execution, and the offline
replay in `conformance/replay-package/` re-proves it with no
network and no key.

### Two zones, stated honestly

A cut is an estimate, so the proof holds a buffer. On the hero
bike run, the proof core (the cut eroded by a small margin) is
4,053,953 pixels, verbatim and receipted at 0 changed. The ring
the erosion removed, plus declared fragile structures (the spokes
first), is the repair margin: 381,802 pixels that may be rebuilt,
and are counted wherever the receipt shows. Of that margin,
278,417 pixels are real product pixels recovered from the photo
itself (138,892 verbatim spokes and 139,525 unmixed pixels);
1,055 pixels are drawn fill where the spokes were provably gone,
and 102,330 were released bare. The receipt covers the bike's
core, pixel for pixel. The margin and the spokes are rebuilt, and
counted, never claimed as verbatim.

### The counterpoint, measured

The headline count has a measured counterpoint. Method: photographs
of real physical products from the verified room run — a bright
orange hardshell suitcase (223,600 product pixels) and a gray
storage box (31,932) — handed to Muse Image 1.0's full-frame edit
mode with a staging prompt that explicitly instructed the model to
keep the product exactly as-is, three independent runs per item,
counted with the receipt's own counting rule (October 28, 2026).
Controls: the counter reproduces 0 on each item's Fidelity Lock
composite at every tolerance, and a pure resize round-trip floor
was measured separately.

| Subject | Regeneration changed, zero tolerance | Regeneration changed, beyond ±16 per channel | Fidelity Lock composite |
|---|---|---|---|
| Orange suitcase (223,600 px) | 223,599–223,600 | 212,823–215,878 | 0 |
| Gray storage box (31,932 px) | 31,858–31,932 | 16,369–29,577 | 0 |

Ranges are min–max across the three runs, never a single run. The
baseline is this model's edit mode, measured as a method, not a
tested third-party tool.

### Riding the model curve

![The same blue bike, staged two ways](docs/images/hero-bike-v109.jpg)

**Through agenthand, on the Meta models this MCP calls.** A real photo of
the seller's bike, staged with the two-zone rule: the core is byte-identical
to the source photo, the margin and the spokes are rebuilt and counted, and
the run carries a receipt in the Proof Graph.

![The same bike, rendered by a general image model](docs/images/bike-look-locked-b2.jpg)

**Rendered with a general-purpose image model** (an edit of our own
composite, made while art-directing this project). **Not an agenthand
output.** It is the prettier picture, and we kept it as our look target.
Nothing in it is receipted, and the same class of model melted this bike's
wheels on another take.

A general image model out-paints the Meta runtime models today. Given the
same bike, it grounds the tires, matches the light, and fills a courtyard
the way the second picture does. agenthand's runtime stays Meta-only, by
the hackathon rules and by our choice, so its plates are plainer for now.
We say that in the open because the gap is real, and because we spent a
large share of this build trying to close it with the allowed tools:

- **Model edit of the whole staged bike.** The prettiest path, and dead on
  arrival: the model repainted the wheels and the spokes melted.
- **Code-drawn spokes over the real cut.** Fully provable and visibly fake.
  It stayed in the project as the control everything else is measured
  against.
- **Frequency suppression.** Recovered genuine spoke pixels from the photo
  itself, 138,892 of them, and still read wrong to the eye. It proved the
  real pixels were there to recover.
- **Per-pixel unmixing, closed-form matting.** Recovered more of the real
  wheel and fed the shipped hybrid: real pixels wherever the photo allows
  them, drawn fill only where it does not, every drawn pixel counted in
  the margin.
- **Angle and surface redraws.** Round after round of scene steering, then
  measured fine-tuning (vanishing point, shadow direction, contact band)
  to seat a flat-lay product in a generated room without the floating read.
- **Background-only repaint of the source photo.** Killed by the API
  itself. The edit endpoint accepts no mask parameter, so the model can
  never be handed the background alone; the camera geometry is inherited
  and the new room is always a re-skin of the seller's own room.

The proof is ours. The product in an agenthand photo is the seller's actual
product, accounted for pixel by pixel, with a receipt anyone can re-check.
That layer has nothing to do with which model painted the room, and it
never has to change. The whole MCP is a bet placed on top of it: every
Meta tool behind one server, so as Meta's models improve, every install
of agenthand gets better pictures for free, proof intact. We built the
harness. The models bring the curve.

## Demo

**Every claim in the demo film traces to a receipt.** The film
(linked at the top of this README) shows live, unmocked Meta model
calls from real input to real result, and each of its chapters is
a node in the Proof Graph: pick any moment and walk it back to the
claim and the receipt behind it.

```sh
agenthand evidence chapter 1:42
```

The count itself also runs in a browser: the project site at
https://ausernameforyou.github.io/agenthand/#fidelity verifies a
staged image's product pixels without installing anything.

**Everything a judge needs is in this repo.**

- [docs/receipt-format.md](docs/receipt-format.md) — every receipt
  field, and exactly what the verifier recomputes.
- [docs/EVIDENCE.md](docs/EVIDENCE.md) — the Proof Graph verbs,
  exit codes, and the judge pack export (`agenthand evidence
  export <dir>`: the database, every receipt file, and a manifest
  in one folder).
- [JUDGING.md](JUDGING.md) — the judging index: every headline
  number in this README, pointed at its Proof Graph trace.
- [SUBMISSION.md](SUBMISSION.md) — the submission statement.
- [llms.txt](llms.txt) — the machine-readable summary of this
  README.
- Setup is [Install](#install) above; `.env.example` lists every
  key, and the replay and evidence verbs need none.

## Technical Depth and Execution

**Two independent builds agree exactly on the proof.** The Python
reference build and this Rust build ran the same frozen proof run
and produced the same output pixel hash, the same count (580,613
opaque pixels checked, 0 changed), and the same settled cost,
$0.020044 to the micro-dollar. The sections below show the
architecture that makes that agreement possible, and the receipt
that records it.

### Architecture

One binary, two halves: a library crate where all the policy lives,
and a thin `main.rs` with seven subcommands (`serve`, `parity`,
`verify`, `measure`, `heic-extract`, `room-run`, `evidence`). The MCP server is
a hand-rolled JSON-RPC stdio loop; each tool handler is a thin edge
that ingests, calls one policy function, and shapes the result JSON.

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
| `measure` | Prototype scale-card size estimator (not a fourth tool). `<card>` must be the id printed on the card in the photo (for example `AH-57`); a different id gives a different scale. `--bbox` is `x,y,w,h` in source pixels |
| `roomrun` | The room run: photos, video frames, and narration in; one staged, receipted listing per item out, plus the priced table |
| `backdrop`, `market_pricing` | The code-rendered stage plates (wallpaper, studio gray) and the deterministic comp-pricing rule behind the priced table |
| `heic` | From-scratch HEIC decoder, integrated into ingest: no external decoding library anywhere in the shipped path |
| `evidence` | The Proof Graph CLI: read-only queries and re-checks over the evidence database (docs/EVIDENCE.md) |

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

### Built twice, measured side by side

agenthand exists twice: the reference server in Python, and this
implementation in Rust. Both were run against the same frozen proof
run and measured side by side. The two builds agree exactly: same
output pixel hash (sha256 8b8defea…e802), same count (580,613 opaque
pixels checked, 0 changed), and the same total billed cost, $0.020044
to the micro-dollar. The interface both builds speak is written down
once, in `spec/interface.schema.json`, and a shared differential
fixture (`tests/fixtures/interface/`) runs in both test suites: one
written contract, two independent implementations, both tested
against it. What differs is how fast the local work runs:

| Operation | Python | Rust |
|---|---|---|
| Cold start to initialize response | ~1.3-3 s (interpreter and import bound) | 2.4 ms best case |
| Verify over the proof images | baseline | about 10.5x faster |

Measured on the same machine (a 2 vCPU AMD EPYC VM). Cold start:
the Rust binary answers in 2.4 ms at best; the Python build pays
interpreter startup plus imports (about 4.7 s of import time alone
for the full server on a warm cache), so its figure moves with
disk load. Verify ratio re-measured 2026-11-02: 23 interleaved
rounds of both shipped CLIs over the frozen replay images, Rust
won all 23, median per-round ratio about 10.5x. One honest
note: on a real tool call, Meta API latency dominates, so the
commercially real win here is cold start, because every MCP client
spawns the server fresh for each session.

Both builds also run the **Differential Conformance Suite**: a shared
interface schema plus shared test vectors, loaded by both test suites.
The vectors carry independently computed expected outputs for boundary
cases, so the builds are checked through differential testing against
the same conformance suite.

### Paths we explored

agenthand ships as two independent builds of the same tool: a Rust build and
a Python build, written separately, each checking the other's output.
Where they disagree, that is a defect, and the pair is the quality control.
Before settling on that shape we built and measured three alternatives.

**Differential Conformance Suite.** We declared the tool interface
once, as a schema, and loaded shared test vectors into both test
suites. The vectors carry independently computed expected outputs, so
each build is checked through differential testing against the same
conformance suite. In a seeded-drift test the suite caught all seven
planted divergences; the original suites had caught one. It also found
two real divergences nobody had planted: a Python rounding function in
receipt provenance that rounded ties the wrong way, and a Rust verifier
that accepted a stripped receipt the Python verifier rejects. Both are
fixed in this submission, and the suite ships with it, so the fixes
stay pinned.

**A generated seam.** We also built the stronger version: generate both
builds' surface code from a single declaration, so the surfaces cannot
drift by construction. That experiment was built in a silo, separate from
the shipping trees. Measured there, the seam build ran its full suites
green (Rust 412 passed / 0 failed, Python 584 passed / 0 failed) with
the frozen replay bit-identical, and it caught surface drift
exhaustively; it also structurally missed the one value-level
divergence above, which only the conformance suite's test vectors
caught. We did not adopt it for this submission. The
suite above bought the measured drift protection for a fraction of the
machinery, and a generated seam shares the fault space it generates: when
the generator is wrong, both builds are wrong together, in the same way,
silently. It stays on the list as the first upgrade candidate after this
submission.

**Three smaller lanes, one line each.** A transpiler that would emit both
builds from one source: rejected, apart from a few small decision tables
worth keeping. A WebAssembly component build of the Rust core: it works,
and nothing in the product needs it yet. A seam design study: the
inventory and design that fed the generated-seam experiment above.

Why we landed here: independence is the verification mechanism, so any
design that shares code between the builds spends the thing that makes the
pair worth having. Where a cheaper instrument measured the same
protection, we took the cheaper instrument. The results above, including
the ones that made our own suites look bad, are the reason the submission
is shaped the way it is.

### It reads photos

The Rust build decodes HEIC itself (no decoding library), and on
every real camera photo tested (iPhone, Canon, Samsung originals)
its output matched the reference decoders pixel for pixel. The
decoder is written from the format specifications and integrated
into the server's ingest: it sniffs the format from the file bytes,
decodes grid and single-picture primaries in-process, applies the
photo's rotation, converts to RGB, and carries the file's colour
profile through to the receipt. A malformed-input campaign
against the decoder (the M5 robustness pass: 1,520 mutations of
3 tiny fixtures, per-process, 10 s cap, run after three found
defects were fixed) produced 0 panics, 0 hangs, and 0 aborts; a
decoder fault is contained at the ingest boundary as an ordinary
error.

### A note on photo uploads

One gap sits outside agenthand's control. Photos attached in Muse
are re-encoded on upload, and the shot angle is stripped in that
re-encode, so a photo that arrives through a chat upload carries no
record of how it was shot. agenthand therefore measures the angle
from the image itself. The author has filed a feature request with
the Muse team asking Muse to pass the camera capture angle and
full photo metadata through with uploads, so tools like agenthand
can use how a photo was shot instead of re-measuring it.

### A real fidelity receipt

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

### Verify a staged image yourself

The binary carries the verifier:

```sh
agenthand verify <source.png> <staged.png> <mask.png>
```

It recounts the product pixels in the staged image against the source
and mask and reports how many changed. The count is the claim.

To re-check every claim in the evidence database:
`agenthand evidence verify --all` (verbs and exit codes in
[docs/EVIDENCE.md](docs/EVIDENCE.md)).

### The Proof Graph

Every claim this project makes traces to a receipt in the Proof Graph:
the verified runs, model calls, receipts, and demo-film chapters,
stored as one evidence graph in the Rust binary's rusqlite database
(`evidence/evidence.db`) and queried through the product itself. We
engineered the evidence as a graph: every claim is a node, every
receipt is proof at the end of a traceable path. The edges are derived
from the data's own foreign keys at import, never drawn by hand, so
the graph cannot disagree with the data.

Verify every claim in the Proof Graph:

```sh
agenthand evidence verify --all
```

- `agenthand evidence trace claim:1` — walk from a claim to its run,
  its calls, and its receipt.
- `agenthand evidence chapter 1:42` — from any moment in the demo
  film to the chapter at that timestamp and the claim behind it.
- `agenthand evidence export <dir>` — a judge pack: the database,
  every receipt file, and a manifest, in one folder.
- Receipts root: `1179530968c27879e3d84aa5475998a475cba64ba5b95f1823a8e7310174ecc3`
  (recompute it with `agenthand evidence root`).

The full graph, rendered by `agenthand evidence graph --format mermaid`:

<details>
<summary>The Proof Graph (all nodes and edges)</summary>

```mermaid
flowchart LR
  n0["claim:1<br/>Fidelity Lock receipt for the hero bike run (IMG..."]
  n1["claim:2<br/>ch7-parity: Frozen replay run: 580,613 pixels ch..."]
  n2["claim:3<br/>ch8-take: Recorded hero take: 5,659,218 pixels c..."]
  n3["claim:4<br/>ch5-fill: P13 open-house fill placed 1,128,708 o..."]
  n4["claim:5<br/>ch0-baseline: On the orange hard-shell suitcase ..."]
  n5["claim:6<br/>The estimator measured a fence-height extent of ..."]
  n6["claim:7<br/>estimator-refusal-1403: The hardened estimator r..."]
  n7["claim:8<br/>f5-overcap: An over-cap settle is recorded over_..."]
  n8["claim:9<br/>Two-zone hero (IMG_1443, decisions 143/145): pro..."]
  n9["run:1<br/>Frozen offline replay (live run frozen 2026-10-2..."]
  n10["run:35<br/>Hero re-cut v109 two-zone (W5 hybrid)"]
  n11["run:6<br/>Hero run IMG_1443 (final Rust master, hole-carve..."]
  n12["run:3<br/>Hero run IMG_1443 (merged build)"]
  n13["run:2<br/>Hero run IMG_1443 (rev-5 packaged build)"]
  n14["run:33<br/>Jacket full chain on IMG_1491 (reshoot)"]
  n15["run:4<br/>Look-test draw on hero cutout (logo sign scene t..."]
  n16["run:10<br/>P12 merged keep-path steps: barbecue (identify +..."]
  n17["run:8<br/>P12 merged re-run: blue bike (SELL)"]
  n18["run:9<br/>P12 merged re-run: patio furniture (SELL)"]
  n19["run:17<br/>P13 dream draw (patio set foreground, imagined b..."]
  n20["run:19<br/>P13 merged dream draw"]
  n21["run:18<br/>P13 merged-build rebuild (same canvas and placem..."]
  n22["run:16<br/>P13 open-house fill run (two placed items on IMG..."]
  n23["run:20<br/>P24 premium draw A: studio backdrop"]
  n24["run:21<br/>P24 premium draw B: golden-hour driveway"]
  n25["run:28<br/>P25 suitcase variants: mask differs from the Oct..."]
  n26["run:30<br/>P25 upgrade variant (bike-v102): A"]
  n27["run:31<br/>P25 upgrade variant (bike-v102): B"]
  n28["run:32<br/>P25 upgrade variant (bike-v102): C"]
  n29["run:29<br/>P25 upgrade variant (bike-v102): tidy"]
  n30["run:22<br/>P25 upgrade variant (first set): bike A"]
  n31["run:23<br/>P25 upgrade variant (first set): bike B"]
  n32["run:24<br/>P25 upgrade variant (first set): bike C"]
  n33["run:25<br/>P25 upgrade variant (first set): suitcase A"]
  n34["run:26<br/>P25 upgrade variant (first set): suitcase B"]
  n35["run:27<br/>P25 upgrade variant (first set): suitcase C"]
  n36["run:5<br/>Recorded hero take (screen recording, build 380e..."]
  n37["run:12<br/>Room run room-5bd7595c: RC cars lot (STAGED)"]
  n38["run:11<br/>Room run room-5bd7595c: barbecue KEEP record"]
  n39["run:13<br/>Room run room-5bd7595c: dip bar (STAGED)"]
  n40["run:15<br/>Room run room-5bd7595c: outdoor furniture (STAGE..."]
  n41["run:14<br/>Room run room-5bd7595c: white bike (STAGED)"]
  n42["run:34<br/>Suitcase still chain (Oct 28 room run)"]
  n43["run:7<br/>run-1443-fixed (Claude's package)"]
  n44["call:1<br/>identify"]
  n45["call:2<br/>cut_out"]
  n46["call:3<br/>draw"]
  n47["call:4<br/>identify (muse-spark-1.3)"]
  n48["call:5<br/>cut_out (sam-3.1)"]
  n49["call:6<br/>draw (muse-image-1.0)"]
  n50["call:7<br/>identify (muse-spark-1.3)"]
  n51["call:8<br/>cut_out (sam-3.1)"]
  n52["call:9<br/>draw (muse-image-1.0)"]
  n53["call:10<br/>draw (muse-image-1.0)"]
  n54["call:11<br/>identify (muse-spark-1.3)"]
  n55["call:12<br/>cut_out (sam-3.1)"]
  n56["call:13<br/>draw (muse-image-1.0)"]
  n57["call:14<br/>identify"]
  n58["call:15<br/>cut_out"]
  n59["call:16<br/>draw"]
  n60["call:17<br/>identify (muse-spark-1.3)"]
  n61["call:18<br/>cut_out (sam-3.1)"]
  n62["call:19<br/>draw (muse-image-1.0)"]
  n63["call:20<br/>identify (muse-spark-1.3)"]
  n64["call:21<br/>cut_out (sam-3.1)"]
  n65["call:22<br/>draw (muse-image-1.0)"]
  n66["call:23<br/>identify (muse-spark-1.3)"]
  n67["call:24<br/>cut_out (sam-3.1)"]
  n68["call:25<br/>identify (muse-spark-1.3)"]
  n69["call:26<br/>draw (muse-image-1.0)"]
  n70["call:27<br/>draw (muse-image-1.0)"]
  n71["call:28<br/>draw (muse-image-1.0)"]
  n72["call:29<br/>draw (muse-image-1.0)"]
  n73["call:30<br/>draw (muse-image-1.0)"]
  n74["call:31<br/>draw (muse-image-1.0)"]
  n75["call:32<br/>identify (muse-spark-1.3)"]
  n76["call:33<br/>cut_out (sam-3.1)"]
  n77["call:34<br/>draw (muse-image-1.0)"]
  n78["call:35<br/>identify (muse-spark-1.3)"]
  n79["call:36<br/>cut_out (sam-3.1)"]
  n80["call:37<br/>draw (muse-image-1.0)"]
  n81["call:38<br/>cut_out"]
  n82["call:39<br/>draw (muse-image-1.0)"]
  n83["call:40<br/>cut_out"]
  n84["call:41<br/>draw (muse-image-1.0)"]
  n85["call:42<br/>cut_out"]
  n86["call:43<br/>draw (muse-image-1.0)"]
  n87["call:44<br/>cut_out"]
  n88["call:45<br/>draw (muse-image-1.0)"]
  n89["call:46<br/>cut_out"]
  n90["call:47<br/>draw (muse-image-1.0)"]
  n91["call:48<br/>cut_out"]
  n92["call:49<br/>draw (muse-image-1.0)"]
  n93["call:50<br/>identify (muse-spark-1.3)"]
  n94["call:51<br/>cut_out (sam-3.1)"]
  n95["call:52<br/>draw (muse-image-1.0)"]
  n96["call:53<br/>identify (muse-spark-1.3)"]
  n97["call:54<br/>cut_out (sam-3.1)"]
  n98["call:55<br/>draw (muse-image-1.0)"]
  n99["call:56<br/>identify (muse-spark-1.3)"]
  n100["call:57<br/>cut_out (sam-3.1)"]
  n101["call:58<br/>draw (muse-image-1.0)"]
  n102["call:59<br/>identify (muse-spark-1.3)"]
  n103["call:60<br/>cut_out (sam-3.1)"]
  n104["call:61<br/>draw (muse-image-1.0)"]
  n105["call:62<br/>identify (muse-spark-1.3)"]
  n106["call:63<br/>cut_out (sam-3.1)"]
  n107["call:64<br/>draw (muse-image-1.0)"]
  n108["call:65<br/>cut_out"]
  n109["call:66<br/>draw (muse-image-1.0)"]
  n110["receipt:1<br/>fidelity_receipt 580613 checked / 0 changed"]
  n111["receipt:2<br/>fidelity_receipt 3422482 checked / 0 changed"]
  n112["receipt:3<br/>fidelity_receipt 5654781 checked / 0 changed"]
  n113["receipt:4<br/>fidelity_receipt 5654781 checked / 0 changed"]
  n114["receipt:5<br/>fidelity_receipt 5659218 checked / 0 changed"]
  n115["receipt:6<br/>fidelity_receipt 5072763 checked / 0 changed"]
  n116["receipt:7<br/>fidelity_receipt 5660280 checked / 0 changed"]
  n117["receipt:8<br/>fidelity_receipt 142071 checked / 0 changed"]
  n118["receipt:9<br/>fidelity_receipt 1116952 checked / 0 changed"]
  n119["receipt:10<br/>keep_record"]
  n120["receipt:11<br/>fidelity_receipt 232231 checked / 0 changed"]
  n121["receipt:12<br/>fidelity_receipt 38901 checked / 0 changed"]
  n122["receipt:13<br/>fidelity_receipt 106758 checked / 0 changed"]
  n123["receipt:14<br/>fidelity_receipt 288450 checked / 0 changed"]
  n124["receipt:15<br/>recount 1128708 checked / 0 changed"]
  n125["receipt:16<br/>fidelity_receipt 404265 checked / 0 changed"]
  n126["receipt:17<br/>recount 2216386 checked / 0 changed"]
  n127["receipt:18<br/>fidelity_receipt 1116952 checked / 0 changed"]
  n128["receipt:19<br/>fidelity_receipt 5654357 checked / 0 changed"]
  n129["receipt:20<br/>fidelity_receipt 5654357 checked / 0 changed"]
  n130["receipt:21<br/>fidelity_receipt 5656640 checked / 0 changed"]
  n131["receipt:22<br/>fidelity_receipt 5656640 checked / 0 changed"]
  n132["receipt:23<br/>fidelity_receipt 5656640 checked / 0 changed"]
  n133["receipt:24<br/>fidelity_receipt 223410 checked / 0 changed"]
  n134["receipt:25<br/>fidelity_receipt 223410 checked / 0 changed"]
  n135["receipt:26<br/>fidelity_receipt 223410 checked / 0 changed"]
  n136["receipt:27<br/>fidelity_receipt 5081270 checked / 0 changed"]
  n137["receipt:28<br/>fidelity_receipt 5081270 checked / 0 changed"]
  n138["receipt:29<br/>fidelity_receipt 5081270 checked / 0 changed"]
  n139["receipt:30<br/>fidelity_receipt 5081270 checked / 0 changed"]
  n140["receipt:31<br/>fidelity_receipt 5695383 checked / 0 changed"]
  n141["receipt:32<br/>fidelity_receipt 223600 checked / 0 changed"]
  n142["receipt:33<br/>fidelity_receipt 4053953 checked / 0 changed"]
  n143["chapter:1<br/>Ch0 baseline: suitcase still, Muse Image edit vs..."]
  n144["chapter:2<br/>Ch1 switchboard: yard, phone, board, run-1443-v1..."]
  n145["chapter:3<br/>Ch2 hero: run-1443-v102, 5,072,763 product pixel..."]
  n146["chapter:4<br/>Ch3a buyer/seller: verifier PASS, deliberate tam..."]
  n147["chapter:5<br/>Ch3b fence bid packet: card printed width 10 in,..."]
  n148["chapter:6<br/>Ch4 moving-day narration: what was said / what t..."]
  n149["chapter:7<br/>Ch5 open-house fill: 1,128,708 placed pixels, 0 ..."]
  n150["chapter:8<br/>Ch6 walkaround and dashboard counters"]
  n151["chapter:9<br/>Ch7 under the hood: Python/Rust parity, 580,613 ..."]
  n152["chapter:10<br/>Ch8 real screen recording 19:06 take sped up 20x..."]
  n153["chapter:11<br/>Ch9 cast and prints"]
  n154["chapter:12<br/>ChClose lockup, recorded-run call times (wall ti..."]
  n155["criterion:Demo"]
  n156["criterion:Technical Depth & Execution"]
  n157["criterion:Use of Meta AI Tooling"]
  n158["model:muse-image-1.0"]
  n159["model:muse-spark-1.3"]
  n160["model:sam-3.1"]
  n161["build:166ea6b"]
  n162["build:380eff0"]
  n163["build:380eff0-line (session transcript + receipt in package; exact hash per package transcript)"]
  n164["build:752a6af chain; two-zone rebuild local (Rust unmix-cf solver + step8_w5.py); consolidation HEAD a148586"]
  n165["build:cd5e250 (agenthand 0.3.0 rev-5 package)"]
  n166["build:d8d0efc"]
  n167["build:f70f194 (local review build)"]
  n0 -->|evidenced_by| n11
  n1 -->|evidenced_by| n9
  n2 -->|evidenced_by| n36
  n3 -->|evidenced_by| n22
  n4 -->|evidenced_by| n42
  n8 -->|evidenced_by| n10
  n0 -->|evidenced_by| n115
  n1 -->|evidenced_by| n110
  n2 -->|evidenced_by| n114
  n3 -->|evidenced_by| n124
  n4 -->|evidenced_by| n141
  n8 -->|evidenced_by| n142
  n0 -->|addresses| n156
  n1 -->|addresses| n156
  n2 -->|addresses| n155
  n3 -->|addresses| n155
  n4 -->|addresses| n157
  n5 -->|addresses| n156
  n6 -->|addresses| n156
  n7 -->|addresses| n156
  n8 -->|addresses| n156
  n9 -->|has_call| n44
  n9 -->|has_call| n45
  n9 -->|has_call| n46
  n13 -->|has_call| n47
  n13 -->|has_call| n48
  n13 -->|has_call| n49
  n12 -->|has_call| n50
  n12 -->|has_call| n51
  n12 -->|has_call| n52
  n15 -->|has_call| n53
  n11 -->|has_call| n54
  n11 -->|has_call| n55
  n11 -->|has_call| n56
  n43 -->|has_call| n57
  n43 -->|has_call| n58
  n43 -->|has_call| n59
  n17 -->|has_call| n60
  n17 -->|has_call| n61
  n17 -->|has_call| n62
  n18 -->|has_call| n63
  n18 -->|has_call| n64
  n18 -->|has_call| n65
  n16 -->|has_call| n66
  n16 -->|has_call| n67
  n38 -->|has_call| n68
  n37 -->|has_call| n69
  n39 -->|has_call| n70
  n41 -->|has_call| n71
  n40 -->|has_call| n72
  n19 -->|has_call| n73
  n20 -->|has_call| n74
  n23 -->|has_call| n75
  n23 -->|has_call| n76
  n23 -->|has_call| n77
  n24 -->|has_call| n78
  n24 -->|has_call| n79
  n24 -->|has_call| n80
  n30 -->|has_call| n81
  n30 -->|has_call| n82
  n31 -->|has_call| n83
  n31 -->|has_call| n84
  n32 -->|has_call| n85
  n32 -->|has_call| n86
  n33 -->|has_call| n87
  n33 -->|has_call| n88
  n34 -->|has_call| n89
  n34 -->|has_call| n90
  n35 -->|has_call| n91
  n35 -->|has_call| n92
  n29 -->|has_call| n93
  n29 -->|has_call| n94
  n29 -->|has_call| n95
  n26 -->|has_call| n96
  n26 -->|has_call| n97
  n26 -->|has_call| n98
  n27 -->|has_call| n99
  n27 -->|has_call| n100
  n27 -->|has_call| n101
  n28 -->|has_call| n102
  n28 -->|has_call| n103
  n28 -->|has_call| n104
  n14 -->|has_call| n105
  n14 -->|has_call| n106
  n14 -->|has_call| n107
  n42 -->|has_call| n108
  n42 -->|has_call| n109
  n9 -->|has_receipt| n110
  n13 -->|has_receipt| n111
  n12 -->|has_receipt| n112
  n15 -->|has_receipt| n113
  n36 -->|has_receipt| n114
  n11 -->|has_receipt| n115
  n43 -->|has_receipt| n116
  n17 -->|has_receipt| n117
  n18 -->|has_receipt| n118
  n38 -->|has_receipt| n119
  n37 -->|has_receipt| n120
  n39 -->|has_receipt| n121
  n41 -->|has_receipt| n122
  n40 -->|has_receipt| n123
  n22 -->|has_receipt| n124
  n19 -->|has_receipt| n125
  n21 -->|has_receipt| n126
  n20 -->|has_receipt| n127
  n23 -->|has_receipt| n128
  n24 -->|has_receipt| n129
  n30 -->|has_receipt| n130
  n31 -->|has_receipt| n131
  n32 -->|has_receipt| n132
  n33 -->|has_receipt| n133
  n34 -->|has_receipt| n134
  n35 -->|has_receipt| n135
  n29 -->|has_receipt| n136
  n26 -->|has_receipt| n137
  n27 -->|has_receipt| n138
  n28 -->|has_receipt| n139
  n14 -->|has_receipt| n140
  n42 -->|has_receipt| n141
  n10 -->|has_receipt| n142
  n49 -->|uses_model| n158
  n52 -->|uses_model| n158
  n53 -->|uses_model| n158
  n56 -->|uses_model| n158
  n62 -->|uses_model| n158
  n65 -->|uses_model| n158
  n69 -->|uses_model| n158
  n70 -->|uses_model| n158
  n71 -->|uses_model| n158
  n72 -->|uses_model| n158
  n73 -->|uses_model| n158
  n74 -->|uses_model| n158
  n77 -->|uses_model| n158
  n80 -->|uses_model| n158
  n82 -->|uses_model| n158
  n84 -->|uses_model| n158
  n86 -->|uses_model| n158
  n88 -->|uses_model| n158
  n90 -->|uses_model| n158
  n92 -->|uses_model| n158
  n95 -->|uses_model| n158
  n98 -->|uses_model| n158
  n101 -->|uses_model| n158
  n104 -->|uses_model| n158
  n107 -->|uses_model| n158
  n109 -->|uses_model| n158
  n47 -->|uses_model| n159
  n50 -->|uses_model| n159
  n54 -->|uses_model| n159
  n60 -->|uses_model| n159
  n63 -->|uses_model| n159
  n66 -->|uses_model| n159
  n68 -->|uses_model| n159
  n75 -->|uses_model| n159
  n78 -->|uses_model| n159
  n93 -->|uses_model| n159
  n96 -->|uses_model| n159
  n99 -->|uses_model| n159
  n102 -->|uses_model| n159
  n105 -->|uses_model| n159
  n48 -->|uses_model| n160
  n51 -->|uses_model| n160
  n55 -->|uses_model| n160
  n61 -->|uses_model| n160
  n64 -->|uses_model| n160
  n67 -->|uses_model| n160
  n76 -->|uses_model| n160
  n79 -->|uses_model| n160
  n94 -->|uses_model| n160
  n97 -->|uses_model| n160
  n100 -->|uses_model| n160
  n103 -->|uses_model| n160
  n106 -->|uses_model| n160
  n11 -->|built_with| n161
  n29 -->|built_with| n161
  n26 -->|built_with| n161
  n27 -->|built_with| n161
  n28 -->|built_with| n161
  n36 -->|built_with| n162
  n23 -->|built_with| n162
  n24 -->|built_with| n162
  n43 -->|built_with| n163
  n10 -->|built_with| n164
  n13 -->|built_with| n165
  n12 -->|built_with| n166
  n15 -->|built_with| n166
  n17 -->|built_with| n166
  n18 -->|built_with| n166
  n16 -->|built_with| n166
  n38 -->|built_with| n166
  n37 -->|built_with| n166
  n39 -->|built_with| n166
  n41 -->|built_with| n166
  n40 -->|built_with| n166
  n21 -->|built_with| n166
  n20 -->|built_with| n166
  n14 -->|built_with| n167
  n145 -->|supports| n0
  n151 -->|supports| n1
  n152 -->|supports| n2
  n149 -->|supports| n3
  n143 -->|supports| n4
```

</details>

### Refusals and limits

The refusals are designed, and they are part of the product:

- **HEIC photos outside the decoder's subset** are refused loudly:
  stills only (intra-coded), 4:2:0 chroma, up to 10-bit. A
  monochrome (4:0:0) primary, an inter-coded picture, or a
  range-extension stream refuses with the subset's own message. A
  refused auxiliary item (for example an HDR gain map) never blocks
  the photo itself: the primary still decodes.
- **Ingest caps.** Photos over 20 MB or 40 million pixels are
  refused before any model call (both caps are configurable).
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

This repository was created on October 28, 2026, inside the Meta
Global AI Developer Hackathon build window (Oct 26 – Nov 4, 2026),
and every commit in it was written in-window; the history is the
audit trail. The Rust implementation is a port of the Python
reference implementation, whose dated build log (BUILD_LOG.md in
that repo) records the project's provenance from an empty directory.

The whole project behind this submission — every run, test, and
experiment, not just the ones in the film — cost **$4.52 in Meta API
calls** (rounded to the nearest cent; the exact settled total is
$4.522337, struck from the project's call ledgers on November 3,
2026).

## License

Proprietary. © Daniel Wagner. All rights reserved.
