# agenthand-take: the filmed demo take

`agenthand-take` is the MCP client behind the recorded Terminal take. It is a Rust port of `hero1443-merged.py` and produces the same transcript lines and the same artifact files. It starts the shipped server binary (`<bin> serve`) and sends it raw JSON-RPC over stdio. The chain is identify -> cut_out -> draw (no scene text) -> look-test draw -> independent `<bin> verify`. Each line prints as it happens and is also appended to `<out>/session-transcript.txt`. The same binary makes both the delivered artifact run and the filmed take, so both always come from the same code.

It uses only `std` and `serde_json`. It needs no new crates and leaves `Cargo.lock` unchanged. Cargo finds the binary automatically from `src/bin/agenthand-take.rs`.

## Build

```sh
cargo build --release          # target/release/agenthand-take and target/release/agenthand
```

## Usage

```
agenthand-take [--src SRC] --out OUT [--bin BIN] [--dry-run] [--no-look-test]
```

| flag | meaning |
|---|---|
| `--src` | photo to stage. Default `~/Downloads/IMG_1443.heic`. It is sent as `data:image/heic;base64,...`, the same as the Python driver. |
| `--out` | output folder. Required. It is created if missing, and the transcript is appended, not truncated. |
| `--bin` | server binary. Default: the `agenthand` file next to `agenthand-take`. If there is none, `~/agenthand-release/agenthand-0.3.0-macos-arm64`. The banner and the verify line print this binary's real file name. |
| `--dry-run` | initialize + tools/list only, then stop before identify. No Meta call is made, nothing is billed, and no key is needed. |
| `--no-look-test` | skip the extra look-test draw. It is on by default, as in the Python. Its scene asks for the round cyan-blue logo on the back wall. |

**Key:** `META_API_KEY` is read only from the environment. The server inherits it. `agenthand-take` never reads its value, never prints it and never writes it to a file. The launcher exports it by sourcing `~/.config/agenthand/env`. The child server gets `AGENTHAND_DATA_DIR=<out>/run-data`, and `AGENTHAND_DRAW_PLACEHOLDER_PLATE` is removed from its environment.

**Exit codes:**
- `0`: the take finished, or the dry run finished.
- `1`: a step returned a non-ok status, or there was a protocol or I/O error. The message goes to stderr.
- `2`: bad arguments.

A missing key (on a real take) or a missing server binary fails before any line prints, so the camera never records half a transcript.

## What it prints

These are the sample responses from the 2026-11-01 IMG_1443 run (times are illustrative):

```
$ date
Sun Nov  1 12:15:11 PST 2026

$ ls -l IMG_1443.heic
-rw-r--r--@ 1 dans  staff  4096794 Oct 31 16:43 IMG_1443.heic

$ agenthand-0.3.0-macos-arm64 serve   # agenthand 0.3.0 (Rust), MCP stdio
initialize -> serverInfo name=agenthand version=0.3.0
tools/list -> ['identify', 'cut_out', 'draw']

$ tools/call identify  image=IMG_1443.heic (4,096,794 bytes)
  muse-spark-1.3 thinking ... 26.6s
  model        : muse-spark-1.3
  request id   : 434419d0c20f4e838299bed218294be6
  concept      : blue kids bike
  title        : Haro Shredder Kids Blue BMX Bike
  condition    : Rusty chain and axle nuts, scuffs on chain guard and fork.
  settled cost : $0.006692

$ tools/call cut_out  image=asset://279476a67847447287b147e0ac2d918f
  sam-3.1 segmenting ... 33.2s
  model        : sam-3.1
  request id   : 6912009efce1403fb9cc65ad66df660f
  mask pixels  : 5,660,280
  settled cost : $0.0025

$ tools/call draw  asset=asset://d67133ab3669483196cdc4040f0e9960
  scene: (none - merged-build defaults)
  muse-image-1.0 generating background plate only ... 41.0s
  model        : muse-image-1.0
  request id   : 9cd7b1ccba7f40368959c89616e99096
  settled cost : $0.01

  FIDELITY LOCK RECEIPT
  product pixels checked : 5,660,280
  product pixels changed : 0
  verify: https://ausernameforyou.github.io/agenthand/verify#9a5db66a...

$ tools/call draw  asset=asset://d67133ab3669483196cdc4040f0e9960   # LOOK-TEST
  scene: "the same place, tidy and bright, with a large round cyan-blue circular logo sign on the back wall"
  muse-image-1.0 ... 38.4s
  plate kind   : source_edit
  settled cost : $0.01
  look receipt : 5,660,280 checked / 0 changed

$ agenthand-0.3.0-macos-arm64 verify source.png staged.png mask.png   # independent checker, same shipped build
  verify PASS : checked 5,660,280 px, changed 0 px, ok=True

RESULT: 0 product pixels changed | TOTAL SETTLED: $0.019192

$ date
Sun Nov  1 12:16:52 PST 2026
```

How the lines are formatted:
- Every line is formatted the way Python formats it. A cost prints as `repr` (`0.0025`, `4e-06`). Counts use `format(n, ",")`. The tool list prints as a Python list repr.
- The elapsed time on each step prints after the call returns, as in the Python.
- The verdict is `PASS` only when `ok` is true and `changed_pixels == 0`. Otherwise it is `CHECK OUTPUT`. If the output can't be parsed, the line is `verify raw: ...`.
- TOTAL SETTLED is identify + cut_out + draw. The look-test cost is kept separately in `chain-summary.json`.
- If a step returns a non-ok status, the take prints `<tool> FAILED: <json, first 300 chars>` and exits 1.
- A dry run prints the first block, from date through the tools/list line, then `DRY RUN: ...`.

## Files written to `--out`

| file | from |
|---|---|
| `session-transcript.txt` | every printed line (appended) |
| `identify-response.json`, `cut_out-response.json`, `draw-response.json` | full JSON-RPC responses in Python `json.dumps(indent=2)` format. Image block data is replaced by `<stripped N chars>`. |
| `draw-look-test-response.json` | the look-test draw response. `draw-response.json` stays the record of the main chain. |
| `source.png`, `foreground.png`, `mask.png`, `mask-overlay.png`, `plate.png`, `staged.png`, `verify_qr.png` | copied from `run-data/assets/<id>.png` |
| `logo-look-test.png` | look-test composite (when its status is ok) |
| `receipt.json` | the staged asset's `<id>.receipt.json` sidecar |
| `IMG_1443.heic` (the `--src` file name) | copy of the source photo |
| `verify-output.json` | raw stdout (or stderr) of `<bin> verify` |
| `chain-summary.json` | `source_file`, `machine`, `binary`, `verify_verdict`, `steps{identify,cut_out,draw}`, `look_test`, `look_test_cost_usd`, `total_cost_usd`, in that key order |
| `run-data/` | the server's data dir: assets and ledger |

## On the Mac

One-time build:

```sh
cd ~/agenthand-rust && cargo build --release
```

Take, using the build's own server (banner reads `agenthand serve`):

```sh
set -a; . ~/.config/agenthand/env; set +a
~/agenthand-rust/target/release/agenthand-take --out ~/agenthand-runs/take-1443-$(date +%Y%m%d-%H%M%S)
```

To get the packaged binary's name in the banner (`agenthand-0.3.0-macos-arm64 serve`), either:
- pass `--bin ~/agenthand-release/agenthand-0.3.0-macos-arm64`, or
- copy `agenthand-take` into `~/agenthand-release/`. There is no sibling named `agenthand` there, so it falls back to that binary.

Check without spending, morning of the shoot:

```sh
~/agenthand-rust/target/release/agenthand-take --dry-run --out /tmp/take-dry
```

## How the take script should call it

In `hero-1443-take.sh`, replace the line `python3 "$HOME/agenthand-runs/hero1443.py" --out "$OUT"` with these two lines:

```bash
set -a; . "$HOME/.config/agenthand/env"; set +a     # exports META_API_KEY; prints nothing
"$HOME/agenthand-release/agenthand-take" --out "$OUT"
```

Keep the rest of the script: the Terminal cosmetics, the `clear`, the timestamped `OUT`, and the closing echo. Never run the script under `bash -x`, because xtrace would echo the sourced key. The script needs `set -a` because the env file holds plain `META_API_KEY=...` lines, which a bare `.` would not export to the child process.

## Differences from the Python driver (deliberate)

- **Key source:** the key comes from the environment, not from parsing `~/.config/agenthand/env` itself.
- **Fail fast:** a missing key or server binary stops the run before the first line prints. The Python printed date, ls and the banner first.
- **Banner name:** the banner prints the real binary's file name, not a hard-coded display name. The version in the banner comment is this build's crate version. The initialize line reports the server's own version.
- **New flags:** `--bin`, `--dry-run` and `--no-look-test`. With `--no-look-test`, `chain-summary.json` has no `look_test` keys.
- **Source copy:** if `--src` is already inside `--out`, the source is not copied onto itself. Python raised SameFileError there.

## Verification

- `tests/take_transcript.rs` feeds the sample responses from the IMG_1443 run through the transcript builders:
  - The output matches the recorded Mac transcript line for line, apart from the draw scene line (that recording still passed scene text).
  - The full body including the look-test is pinned.
  - Re-serialising the saved `identify`/`cut_out` response files reproduces them byte for byte.
  - It also covers verify verdicts, failure lines, totals, base64 and argument parsing.
- The dry-run tests start the locally built `agenthand` server and stop before identify.
- During development, a replay server answered both drivers with the same sample responses, in the success case and in identify, cut_out, draw and verify failure cases. The Python and the Rust driver sent identical tool arguments, including the data-URI hash. They printed identical transcripts, apart from the times and dates. Every artifact file was byte-identical, apart from the out-dir paths that the verify output echoes.
